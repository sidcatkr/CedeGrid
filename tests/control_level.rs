//! 003 FR-040, FR-047, research R2: the control level from the host's state, read only.

use cedegrid::control_level::{ControlLevel, Sharing, detect_in, sharing_in};
use std::fs;
use std::path::Path;

fn manager(root: &Path, uid: u32, controllers: &str) {
    let dir = root
        .join("user.slice")
        .join(format!("user-{uid}.slice"))
        .join(format!("user@{uid}.service"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("cgroup.controllers"), controllers).unwrap();
}

fn me() -> u32 {
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions and cannot fail.
    return unsafe { libc::getuid() };
    #[cfg(not(unix))]
    0
}

/// Full only when the manager answers and holds cpu and memory; every other case is Tracked
/// with one sentence saying why.
#[test]
fn full_needs_an_answering_manager_with_cpu_and_memory() {
    let root = tempfile::tempdir().unwrap();
    let uid = me();
    manager(root.path(), uid, "cpu memory pids\n");
    let full = detect_in(root.path(), uid, true);
    assert_eq!(full.level, ControlLevel::Full, "{}", full.reason);
    let silent = detect_in(root.path(), uid, false);
    assert_eq!(silent.level, ControlLevel::Tracked);
    assert!(
        silent.reason.contains("does not answer"),
        "{}",
        silent.reason
    );
    manager(root.path(), uid, "pids memory\n");
    let no_cpu = detect_in(root.path(), uid, true);
    assert_eq!(no_cpu.level, ControlLevel::Tracked);
    assert!(no_cpu.reason.contains("cpu"), "{}", no_cpu.reason);
    let nobody = detect_in(root.path(), uid + 1, true);
    assert_eq!(nobody.level, ControlLevel::Tracked, "{}", nobody.reason);
    for reason in [&full.reason, &silent.reason, &no_cpu.reason, &nobody.reason] {
        assert!(
            !reason.contains('\u{b7}') && !reason.ends_with('.'),
            "{reason}"
        );
    }
}

/// Contenders share the CPU cgroup unless user.slice passes cpu down to each account.
#[test]
fn sharing_follows_user_slice_subtree_control() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(sharing_in(root.path()), Sharing::Unknown);
    fs::create_dir_all(root.path().join("user.slice")).unwrap();
    fs::write(
        root.path().join("user.slice/cgroup.subtree_control"),
        "cpu memory\n",
    )
    .unwrap();
    assert_eq!(sharing_in(root.path()), Sharing::No);
    fs::write(
        root.path().join("user.slice/cgroup.subtree_control"),
        "memory pids\n",
    )
    .unwrap();
    assert_eq!(sharing_in(root.path()), Sharing::Yes);
}
