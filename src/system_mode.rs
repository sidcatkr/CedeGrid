//! System mode's client side (feature 005).
//!
//! The root refusal every command starts with (FR-015), and the one request a member's
//! `cedegrid system` sends the broker, with the fallback when the broker is missing or silent
//! (FR-013). Nothing here needs privilege.

use crate::broker::BROKER_SUBCOMMAND;
use crate::broker::request::{Refusal, RefusalCode, Reply, Request, UnitStatus};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

/// How long the client waits for the broker before it falls back (*provisional*, FR-013).
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

/// The most the client reads of a reply.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// The line a command run as root refuses with, or `None` when it may run (FR-015, 003 FR-082).
///
/// The broker's own subcommand, started by its unit as root, is the one exception.
pub fn root_refusal(euid: u32, first_arg: Option<&OsStr>) -> Option<String> {
    if euid != 0 || first_arg == Some(OsStr::new(BROKER_SUBCOMMAND)) {
        return None;
    }
    Some("refusing to run as root; run cedegrid as your own user, without sudo".to_owned())
}

/// What came back from asking the broker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No broker socket: the system package is not installed (FR-001).
    NotInstalled,
    /// The socket exists but no usable answer came within the timeout; the reason is for the
    /// one line the client prints.
    NoAnswer(String),
    /// The broker served the request.
    Units(Vec<UnitStatus>),
    /// The broker refused the request.
    Refused(Refusal),
}

/// What the client does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run in System mode, in these units.
    System(Vec<UnitStatus>),
    /// Run as without the package, after printing the line if there is one.
    Fallback(Option<String>),
    /// Stop with this line.
    Refuse(String),
}

/// Decides between System mode, the fallback and a refusal (FR-010, FR-013, FR-035).
pub fn decide(outcome: Outcome, require_system: bool) -> Decision {
    let fallback = |line: String| {
        if require_system {
            Decision::Refuse(format!(
                "System mode required (--require-system) but unavailable: {line}"
            ))
        } else {
            Decision::Fallback(Some(format!(
                "System mode unavailable, continuing without it: {line}"
            )))
        }
    };
    match outcome {
        Outcome::Units(units) => Decision::System(units),
        Outcome::NotInstalled if require_system => Decision::Refuse(
            "System mode required (--require-system) but cedegrid-system is not installed on this host"
                .to_owned(),
        ),
        // Without the package, nothing changes and nothing is printed (FR-013).
        Outcome::NotInstalled => Decision::Fallback(None),
        Outcome::NoAnswer(reason) => fallback(format!("broker {reason}")),
        Outcome::Refused(refusal) => match refusal.code {
            // The administrator's policy is refused, never bypassed by the fallback (FR-010).
            RefusalCode::ModeNotAllowed | RefusalCode::TaskLimit | RefusalCode::UnitNotOurs => {
                Decision::Refuse(refusal.message)
            }
            _ => fallback(refusal.message),
        },
    }
}

/// Sends `request` to the broker at `socket` and reads its one reply within `timeout`.
#[cfg(unix)]
pub fn ask(socket: &Path, request: &Request, timeout: Duration) -> Outcome {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream;

    match std::fs::symlink_metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Outcome::NotInstalled,
        Err(_) => return Outcome::NoAnswer("socket cannot be read".to_owned()),
        Ok(meta) if !meta.file_type().is_socket() => {
            return Outcome::NoAnswer("socket path is not a socket".to_owned());
        }
        Ok(_) => {}
    }
    // A unix-socket connect does not wait for the server; the timeouts below bound the rest.
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Outcome::Refused(Refusal::new(
                RefusalCode::NotAMember,
                "not a member of group cedegrid; ask an administrator to add your account",
            ));
        }
        Err(_) => return Outcome::NoAnswer("is not running".to_owned()),
    };
    let silent = || Outcome::NoAnswer(format!("did not answer within {} s", timeout.as_secs_f64()));
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return silent();
    }
    if stream.write_all(request.to_line().as_bytes()).is_err() {
        return silent();
    }
    let mut line = Vec::new();
    let mut reader = BufReader::new((&stream).take(MAX_REPLY_BYTES));
    match reader.read_until(b'\n', &mut line) {
        Ok(_) if line.ends_with(b"\n") => {}
        Ok(_) => return Outcome::NoAnswer("closed the connection without a reply".to_owned()),
        Err(_) => return silent(),
    }
    reply_outcome(&line)
}

/// Without unix sockets there is no System mode.
#[cfg(not(unix))]
pub fn ask(_socket: &Path, _request: &Request, _timeout: Duration) -> Outcome {
    Outcome::NotInstalled
}

/// Reads one reply line.
pub fn reply_outcome(line: &[u8]) -> Outcome {
    let Ok(reply) = serde_json::from_slice::<Reply>(line) else {
        return Outcome::NoAnswer("sent an unreadable reply".to_owned());
    };
    if reply.protocol != crate::broker::PROTOCOL {
        let side = if reply.protocol > crate::broker::PROTOCOL {
            "upgrade the cedegrid client"
        } else {
            "ask an administrator to upgrade cedegrid-system"
        };
        return Outcome::Refused(Refusal::new(
            RefusalCode::ProtocolMismatch,
            format!(
                "protocol mismatch: broker {}, client {}; {side}",
                reply.protocol,
                crate::broker::PROTOCOL
            ),
        ));
    }
    match (reply.ok, reply.code) {
        (true, None) => Outcome::Units(reply.units.unwrap_or_default()),
        (false, Some(code)) => Outcome::Refused(Refusal::new(
            code,
            reply
                .message
                .unwrap_or_else(|| "request refused by the broker".to_owned()),
        )),
        _ => Outcome::NoAnswer("sent an unreadable reply".to_owned()),
    }
}

/// What `cedegrid system` prints.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SystemReport {
    /// Whether the request was served in System mode.
    pub system: bool,
    pub units: Vec<UnitStatus>,
}

/// Runs one `cedegrid system` request against the installed broker.
pub fn run(request: &Request, require_system: bool) -> anyhow::Result<SystemReport> {
    let outcome = ask(
        Path::new(crate::broker::SOCKET_PATH),
        request,
        CLIENT_TIMEOUT,
    );
    match decide(outcome, require_system) {
        Decision::System(units) => Ok(SystemReport {
            system: true,
            units,
        }),
        Decision::Fallback(line) => {
            if let Some(line) = line {
                use std::io::Write;
                // A lost notice does not change what runs.
                let _ = writeln!(std::io::stderr().lock(), "cedegrid: {line}");
            }
            Ok(SystemReport {
                system: false,
                units: Vec::new(),
            })
        }
        Decision::Refuse(line) => anyhow::bail!(line),
    }
}

/// What the two units run until the agent and the idle executor are built into them (T022):
/// the process lays out the unit's cgroups, then holds the unit, and with it the user's
/// delegated cgroup, until systemd stops it.
#[cfg(target_os = "linux")]
pub fn hold_unit(kind: crate::broker::units::UnitKind) -> anyhow::Result<()> {
    let jobs = prepare_unit(kind)?;
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "cedegrid: System mode unit ready; jobs go to {}",
        jobs.display()
    );
    loop {
        std::thread::park();
    }
}

/// Lays out a System mode unit's cgroups as the user (T022, T023): this process moves into
/// `<unit>/agent`, the cpu, memory and pids controllers are passed down, and `<unit>/jobs` is
/// made for the jobs' leaves. Returns the jobs' directory.
#[cfg(target_os = "linux")]
pub fn prepare_unit(kind: crate::broker::units::UnitKind) -> anyhow::Result<std::path::PathBuf> {
    use crate::broker::units::{IDLE_SLICE, UnitKind, marker_value, unit_name};
    use anyhow::{Context, ensure};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let mount = Path::new("/sys/fs/cgroup");
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let own = std::fs::read_to_string("/proc/self/cgroup").context("cannot read own cgroup")?;
    let own = crate::broker::process_handle::parse_cgroup_v2(&own)
        .context("not in a cgroup v2 hierarchy")?;
    let marker = std::env::var(crate::broker::units::MARKER_VARIABLE).ok();
    let owner = |path: &Path| std::fs::metadata(path).ok().map(|meta| meta.uid());
    let jobs = match kind {
        UnitKind::Ordinary => {
            crate::cgroup::system_unit_jobs_root(&own, uid, marker.as_deref(), mount, owner)?
        }
        UnitKind::Idle => {
            ensure!(
                marker.as_deref() == Some(marker_value(UnitKind::Idle, uid).as_str()),
                "not started by the System mode broker for this user"
            );
            let unit = format!("/{IDLE_SLICE}/{}", unit_name(UnitKind::Idle, uid));
            let base = own.strip_suffix("/agent").unwrap_or(&own);
            ensure!(base == unit, "cgroup {own} is not this user's idle unit");
            let dir = mount.join(unit.trim_start_matches('/'));
            ensure!(
                owner(&dir) == Some(uid),
                "{} is not delegated to user ID {uid}",
                dir.display()
            );
            dir.join(crate::broker::oom::JOBS_DIR)
        }
    };
    let unit_dir = jobs
        .parent()
        .context("jobs directory has no parent")?
        .to_owned();
    let agent = unit_dir.join("agent");
    for dir in [&agent, &jobs] {
        match std::fs::DirBuilder::new().mode(0o755).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", dir.display()));
            }
        }
    }
    if !own.ends_with("/agent") {
        // "0" moves the writing process; no other process is named.
        std::fs::write(agent.join("cgroup.procs"), "0")
            .context("cannot move into the unit's agent cgroup")?;
    }
    for dir in [&unit_dir, &jobs] {
        let available = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
        let wanted: Vec<String> = ["cpu", "memory", "pids"]
            .into_iter()
            .filter(|name| available.split_whitespace().any(|have| have == *name))
            .map(|name| format!("+{name}"))
            .collect();
        if !wanted.is_empty() {
            std::fs::write(dir.join("cgroup.subtree_control"), wanted.join(" "))
                .with_context(|| format!("cannot pass controllers down in {}", dir.display()))?;
        }
    }
    Ok(jobs)
}
