//! A process held by a pidfd, with its `/proc/<pid>` directory open (005 FR-023, T014). It lives
//! in the broker, not in `kernel.rs`, because it writes and kernel code stays read-only.
//!
//! The directory is opened after the pidfd and checked against it, so it is the same process.
//! Files read or written through the directory reach only that process: once it exits, they fail
//! even if its PID is reused. This module only reads and writes the files it is asked to; the
//! broker decides what to write.

use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A live process, its pidfd and its `/proc/<pid>` directory.
#[derive(Debug)]
pub struct ProcessHandle {
    pid: u32,
    start_time: u64,
    pidfd: OwnedFd,
    proc_dir: OwnedFd,
}

/// The user and group IDs of a process, as `/proc/<pid>/status` gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIds {
    /// Real, effective, saved and filesystem user IDs.
    pub uids: [u32; 4],
    /// Real, effective, saved and filesystem group IDs.
    pub gids: [u32; 4],
}

impl ProcessIds {
    /// Whether every user ID is `uid` and every group ID is `gid`.
    pub fn all_are(&self, uid: u32, gid: u32) -> bool {
        self.uids.iter().all(|&value| value == uid) && self.gids.iter().all(|&value| value == gid)
    }
}

impl ProcessHandle {
    /// Opens `pid`. When `expected_start_time` is given, a process that started at another time
    /// is refused: its PID was reused.
    pub fn open(pid: u32, expected_start_time: Option<u64>) -> Result<Self> {
        ensure!(pid != 0, "PID 0 names no process");
        let raw_pid = libc::pid_t::try_from(pid).context("PID out of range")?;
        // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0u32) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("process {pid} could not be held"));
        }
        // SAFETY: a successful pidfd_open returns a new descriptor this handle now owns.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let path = CString::new(format!("/proc/{pid}")).context("invalid /proc path")?;
        // SAFETY: a NUL-terminated path and plain flags; the result is checked.
        let dir = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if dir < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("process {pid}'s /proc directory could not be opened"));
        }
        // SAFETY: a successful open returns a new descriptor this handle now owns.
        let proc_dir = unsafe { OwnedFd::from_raw_fd(dir) };
        let mut handle = ProcessHandle {
            pid,
            start_time: 0,
            pidfd,
            proc_dir,
        };
        // While the held process lives its PID cannot be reused, so the directory opened above is
        // its own.
        ensure!(
            handle.is_alive()?,
            "process {pid} exited while it was opened"
        );
        handle.start_time = parse_start_time(&handle.read_file("stat")?)
            .with_context(|| format!("process {pid}'s stat has no start time"))?;
        if let Some(expected) = expected_start_time {
            ensure!(
                handle.start_time == expected,
                "process {pid} started at {} and not {expected}; its PID was reused",
                handle.start_time
            );
        }
        Ok(handle)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Start time after boot in clock ticks, from `/proc/<pid>/stat`.
    pub fn start_time(&self) -> u64 {
        self.start_time
    }

    pub fn pidfd(&self) -> &OwnedFd {
        &self.pidfd
    }

    /// Whether the process is still running. An error is not "running".
    pub fn is_alive(&self) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and a zero timeout.
        match unsafe { libc::poll(&mut poll, 1, 0) } {
            0 => Ok(true),
            1 if poll.revents & libc::POLLIN != 0 => Ok(false),
            _ => bail!("process {}'s pidfd could not be polled", self.pid),
        }
    }

    /// User and group IDs from `/proc/<pid>/status`.
    pub fn ids(&self) -> Result<ProcessIds> {
        parse_ids(&self.read_file("status")?)
            .with_context(|| format!("process {}'s status has no user or group IDs", self.pid))
    }

    /// The process's cgroup v2 path from `/proc/<pid>/cgroup`.
    pub fn cgroup(&self) -> Result<String> {
        parse_cgroup_v2(&self.read_file("cgroup")?)
            .with_context(|| format!("process {} is in no cgroup v2 hierarchy", self.pid))
    }

    /// `oom_score_adj` as the kernel holds it.
    pub fn oom_score_adj(&self) -> Result<i32> {
        self.read_file("oom_score_adj")?
            .trim()
            .parse()
            .with_context(|| format!("process {}'s oom_score_adj is not a number", self.pid))
    }

    /// Writes `oom_score_adj` and reads it back (Principle V).
    pub fn set_oom_score_adj(&self, value: i32) -> Result<()> {
        ensure!(
            (-1000..=1000).contains(&value),
            "oom_score_adj {value} is outside -1000 to 1000"
        );
        let mut file = self.open_file("oom_score_adj", libc::O_WRONLY)?;
        file.write_all(value.to_string().as_bytes())
            .with_context(|| {
                format!("process {}'s oom_score_adj could not be written", self.pid)
            })?;
        drop(file);
        let read_back = self.oom_score_adj()?;
        ensure!(
            read_back == value,
            "process {}'s oom_score_adj reads {read_back} after writing {value}",
            self.pid
        );
        ensure!(
            self.is_alive()?,
            "process {} exited during the write",
            self.pid
        );
        Ok(())
    }

    fn read_file(&self, name: &str) -> Result<String> {
        let mut file = self.open_file(name, libc::O_RDONLY)?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("process {}'s {name} could not be read", self.pid))?;
        Ok(text)
    }

    fn open_file(&self, name: &str, access: libc::c_int) -> Result<std::fs::File> {
        ensure!(
            !name.contains('/') && !name.is_empty(),
            "only a file directly in /proc/<pid> may be opened"
        );
        let name_c = CString::new(name).context("invalid /proc file name")?;
        // SAFETY: a valid directory descriptor, a NUL-terminated relative name and plain flags.
        let fd = unsafe {
            libc::openat(
                self.proc_dir.as_raw_fd(),
                name_c.as_ptr(),
                access | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("process {}'s {name} could not be opened", self.pid));
        }
        // SAFETY: a successful openat returns a new descriptor the File now owns.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

/// Start time (field 22) of a `/proc/<pid>/stat` line, split after the last `)`.
pub fn parse_start_time(stat: &str) -> Option<u64> {
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    // The first field after ')' is field 3.
    fields.get(22 - 3)?.parse().ok()
}

/// The four user and group IDs of a `/proc/<pid>/status` text.
pub fn parse_ids(status: &str) -> Option<ProcessIds> {
    let four = |name: &str| -> Option<[u32; 4]> {
        let line = status.lines().find_map(|line| line.strip_prefix(name))?;
        let mut values = line.split_whitespace().map(str::parse::<u32>);
        let mut next = || values.next()?.ok();
        Some([next()?, next()?, next()?, next()?])
    };
    Some(ProcessIds {
        uids: four("Uid:")?,
        gids: four("Gid:")?,
    })
}

/// The cgroup v2 path in a `/proc/<pid>/cgroup` text: the `0::` line.
pub fn parse_cgroup_v2(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim_end().to_owned())
        .filter(|path| path.starts_with('/'))
}
