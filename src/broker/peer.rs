//! Who is calling (FR-003, FR-004, research R7).
//!
//! The caller's user ID comes only from the socket's peer credentials. Group membership is read
//! from the host's group database at each request, so a removal takes effect at once.

use super::GROUP;
use super::request::{Refusal, RefusalCode};
use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::os::unix::net::UnixStream;

/// The peer of a connection, as the kernel recorded it at `connect()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    /// The peer's PID where the platform gives it; for the journal only.
    pub pid: Option<u32>,
}

/// Reads the peer's credentials from the socket (`SO_PEERCRED` on Linux).
#[cfg(target_os = "linux")]
pub fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid socket descriptor, and a buffer of exactly the size passed.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(std::io::Error::other("short SO_PEERCRED reply"));
    }
    Ok(PeerCredentials {
        uid: cred.uid,
        gid: cred.gid,
        pid: u32::try_from(cred.pid).ok().filter(|&pid| pid != 0),
    })
}

/// Reads the peer's credentials where `SO_PEERCRED` does not exist; used by tests off Linux.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: a valid socket descriptor and two valid out-pointers.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PeerCredentials {
        uid,
        gid,
        pid: None,
    })
}

/// The account facts the broker needs, read fresh at each request.
pub trait Accounts {
    /// The account's primary group ID.
    fn primary_gid(&self, uid: u32) -> Result<u32>;
    /// Whether the account is in the `cedegrid` group now, as primary or supplementary group.
    fn is_member(&self, uid: u32) -> Result<bool>;
}

/// The host's passwd and group databases, through NSS.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostAccounts;

struct Passwd {
    name: CString,
    gid: u32,
}

fn passwd(uid: u32) -> Result<Passwd> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwuid_r.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid out-pointers and a buffer of the length passed.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 {
        bail!("the passwd database could not be read for user ID {uid} (error {rc})");
    }
    if result.is_null() {
        bail!("user ID {uid} has no passwd entry");
    }
    // SAFETY: on success pw_name points to a NUL-terminated string inside `buffer`.
    let name = unsafe { CStr::from_ptr(entry.pw_name) }.to_owned();
    Ok(Passwd {
        name,
        gid: entry.pw_gid,
    })
}

/// The `cedegrid` group's ID and member names.
fn group() -> Result<(u32, Vec<CString>)> {
    let name = CString::new(GROUP).context("invalid group name")?;
    let mut buffer = vec![0 as libc::c_char; 64 * 1024];
    // SAFETY: zeroed group is a valid out-parameter for getgrnam_r.
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: valid out-pointers and a buffer of the length passed.
    let rc = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 {
        bail!("the group database could not be read (error {rc})");
    }
    if result.is_null() {
        bail!("the {GROUP} group does not exist");
    }
    let mut members = Vec::new();
    let mut cursor = entry.gr_mem;
    // SAFETY: gr_mem is a NULL-terminated array of NUL-terminated strings inside `buffer`.
    unsafe {
        while !cursor.is_null() && !(*cursor).is_null() {
            members.push(CStr::from_ptr(*cursor).to_owned());
            cursor = cursor.add(1);
        }
    }
    Ok((entry.gr_gid, members))
}

impl Accounts for HostAccounts {
    fn primary_gid(&self, uid: u32) -> Result<u32> {
        Ok(passwd(uid)?.gid)
    }

    fn is_member(&self, uid: u32) -> Result<bool> {
        let account = passwd(uid)?;
        let (gid, members) = group()?;
        Ok(account.gid == gid || members.contains(&account.name))
    }
}

/// The caller, once checked.
pub type Caller = super::units::Caller;

/// Checks the caller: never user ID 0 or below the host's first ordinary user ID, and a member
/// of the group now (FR-004). An unreadable database refuses.
pub fn authorize(
    peer: PeerCredentials,
    accounts: &dyn Accounts,
    host_uid_min: u32,
) -> Result<Caller, Refusal> {
    if peer.uid == 0 || peer.uid < host_uid_min {
        return Err(Refusal::new(
            RefusalCode::RefusedAccount,
            "root and system accounts (below UID_MIN) cannot use System mode; connect as a regular user",
        ));
    }
    match accounts.is_member(peer.uid) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            return Err(Refusal::new(
                RefusalCode::NotAMember,
                "not a member of group cedegrid; ask an administrator to add your account",
            ));
        }
    }
    let primary_gid = accounts.primary_gid(peer.uid).map_err(|_| {
        Refusal::new(
            RefusalCode::RefusedAccount,
            "cannot read your account from the user database; ask an administrator to check NSS",
        )
    })?;
    Ok(Caller {
        uid: peer.uid,
        primary_gid,
    })
}
