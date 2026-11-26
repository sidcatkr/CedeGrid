//! The control level a launch gets (003 FR-040, research R2), detected without privilege and
//! without changing anything on the host: System mode where this account's agent runs in its
//! broker unit, Full where the user's service manager delegates the CPU and memory controllers,
//! and Tracked otherwise, with one sentence saying why.

use serde::Serialize;
use std::path::Path;

/// How much a run is contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlLevel {
    Full,
    Tracked,
    /// This account's agent runs in its System mode unit (005; 003 Session 2026-11-26).
    System,
}

impl ControlLevel {
    pub fn name(self) -> &'static str {
        match self {
            ControlLevel::Full => "Full",
            ControlLevel::Tracked => "Tracked",
            ControlLevel::System => "System mode",
        }
    }
}

/// Whether other users' work shares the run's CPU cgroup (FR-047).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    Yes,
    No,
    Unknown,
}

/// The level, why, and whether contenders share the CPU cgroup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Detected {
    pub level: ControlLevel,
    /// One sentence.
    pub reason: String,
    pub contenders_share_cgroup: Sharing,
}

/// Forces Tracked on a host with delegation, so both levels can be measured side by side
/// (FR-041); for tests and native gates only.
pub const FORCE_TRACKED: &str = "CEDEGRID_TEST_FORCE_TRACKED";

/// The level on this host now.
pub fn detect_now() -> Detected {
    if std::env::var_os(FORCE_TRACKED).as_deref() == Some(std::ffi::OsStr::new("1")) {
        return Detected {
            level: ControlLevel::Tracked,
            reason: format!("{FORCE_TRACKED}=1 forces Tracked"),
            contenders_share_cgroup: sharing_now(),
        };
    }
    if crate::system_mode::in_unit() {
        return Detected {
            level: ControlLevel::System,
            reason: "this agent runs in its System mode unit".to_owned(),
            contenders_share_cgroup: sharing_now(),
        };
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        detect_in(Path::new("/sys/fs/cgroup"), uid, manager_answers())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Detected {
            level: ControlLevel::Tracked,
            reason: "this platform has no cgroup delegation".to_owned(),
            contenders_share_cgroup: Sharing::Unknown,
        }
    }
}

/// The level from a cgroup v2 root, the user ID and whether `systemctl --user` answers: Full
/// only when the user's manager answers, its cgroup is the user's, and it holds the cpu and
/// memory controllers; any other case or error gives Tracked with the reason.
pub fn detect_in(cgroup_root: &Path, uid: u32, manager_answers: bool) -> Detected {
    let contenders_share_cgroup = sharing_in(cgroup_root);
    let tracked = |reason: String| Detected {
        level: ControlLevel::Tracked,
        reason,
        contenders_share_cgroup,
    };
    if !manager_answers {
        return tracked("the user's service manager does not answer (systemctl --user)".to_owned());
    }
    let manager = cgroup_root
        .join("user.slice")
        .join(format!("user-{uid}.slice"))
        .join(format!("user@{uid}.service"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(&manager) {
            Ok(meta) if meta.uid() == uid => {}
            Ok(_) => {
                return tracked(
                    "the user's manager cgroup is not delegated to this user".to_owned(),
                );
            }
            Err(_) => return tracked("the user's manager cgroup cannot be read".to_owned()),
        }
    }
    let controllers = match std::fs::read_to_string(manager.join("cgroup.controllers")) {
        Ok(text) => text,
        Err(_) => return tracked("the user's manager cgroup lists no controllers".to_owned()),
    };
    let held: Vec<&str> = controllers.split_whitespace().collect();
    let missing: Vec<&str> = ["cpu", "memory"]
        .into_iter()
        .filter(|controller| !held.contains(controller))
        .collect();
    if !missing.is_empty() {
        return tracked(format!(
            "the user's manager is not delegated the {} controller",
            missing.join(" and ")
        ));
    }
    Detected {
        level: ControlLevel::Full,
        reason: "the user's service manager delegates the cpu and memory controllers".to_owned(),
        contenders_share_cgroup,
    }
}

/// Other users share the CPU cgroup when `user.slice` does not pass the cpu controller down to
/// each account's slice (FR-047).
pub fn sharing_in(cgroup_root: &Path) -> Sharing {
    match std::fs::read_to_string(
        cgroup_root
            .join("user.slice")
            .join("cgroup.subtree_control"),
    ) {
        Ok(text)
            if text
                .split_whitespace()
                .any(|controller| controller == "cpu") =>
        {
            Sharing::No
        }
        Ok(_) => Sharing::Yes,
        Err(_) => Sharing::Unknown,
    }
}

fn sharing_now() -> Sharing {
    #[cfg(target_os = "linux")]
    {
        sharing_in(Path::new("/sys/fs/cgroup"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Sharing::Unknown
    }
}

/// Whether the user's service manager answers, asked read-only with a short bound.
#[cfg(target_os = "linux")]
fn manager_answers() -> bool {
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("systemctl")
        .args(["--user", "--no-pager", "show", "--property=Version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < end => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // A manager that does not answer in time counts as absent; the child is ours.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
