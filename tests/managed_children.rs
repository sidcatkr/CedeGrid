use resource_manager::{
    execution_model::*, managed_children::*, model::Resources, state::StateStore,
};
use std::{collections::BTreeMap, path::PathBuf};
fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".managed-child-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}
fn setup(path: &std::path::Path) -> (StateStore, ExecutionRecord, LaunchRequest) {
    let store = StateStore::open(path).unwrap();
    let request = LaunchRequest {
        task_id: "task".into(),
        assignment_id: "assignment".into(),
        argv: vec!["/bin/true".into()],
        cwd: std::env::current_dir().unwrap(),
        env: BTreeMap::new(),
        resources: Resources {
            cpu_millicores: 2000,
            ram_mib: 256,
            gpu_memory_mib: BTreeMap::new(),
        },
        replay_safe: true,
        class: AllocationClass::Guaranteed,
        no_escape: true,
        single_process: false,
        managed_child_limit: 2,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    };
    let mut parent = store.reserve(&request, &request.resources).unwrap();
    parent.phase = ExecutionPhase::Prepared;
    parent.backend = "rootless".into();
    parent.identity = Some(ProcessIdentity {
        pid: 123,
        boot_id: "boot".into(),
        start_time: 1,
        assignment_id: "assignment".into(),
        generation: 1,
    });
    store.transition(&parent).unwrap();
    parent.phase = ExecutionPhase::Authorized;
    store.transition(&parent).unwrap();
    parent.phase = ExecutionPhase::Running;
    store.transition(&parent).unwrap();
    let mut child = request;
    child.single_process = true;
    child.managed_child_limit = 0;
    (store, parent, child)
}
fn identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        boot_id: "boot".into(),
        start_time: pid as u64,
        assignment_id: "assignment".into(),
        generation: 1,
    }
}
#[test]
fn deduplication_and_limits_preserve_one_family_reservation() {
    let d = dir();
    let (s, _, r) = setup(d.path());
    let first = s
        .reserve_managed_child("assignment", 1, "child-1", "request-1", &r)
        .unwrap();
    let replay = s
        .reserve_managed_child("assignment", 1, "new-child-id", "request-1", &r)
        .unwrap();
    assert_eq!(first.child_id, replay.child_id);
    let mut changed = r.clone();
    changed.argv.push("different".into());
    assert!(
        s.reserve_managed_child("assignment", 1, "new-child-id", "request-1", &changed)
            .is_err()
    );
    s.reserve_managed_child("assignment", 1, "child-2", "request-2", &r)
        .unwrap();
    assert!(
        s.reserve_managed_child("assignment", 1, "child-3", "request-3", &r)
            .is_err()
    );
    s.transition_managed_child(
        "child-1",
        ManagedChildPhase::NeedsReconciliation,
        None,
        None,
        "supervisor lost before complete identity",
    )
    .unwrap();
    assert!(
        s.reserve_managed_child("assignment", 1, "child-3", "request-3", &r)
            .is_err()
    );
    let a = s.execution_allocations().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].requested.cpu_millicores, 2000);
    assert_eq!(s.all_unreleased_managed_children().unwrap().len(), 2);
}
#[test]
fn child_identity_and_preparation_are_fenced_before_exec() {
    let d = dir();
    let (s, _, r) = setup(d.path());
    s.reserve_managed_child("assignment", 1, "child-1", "request-1", &r)
        .unwrap();
    assert!(
        s.transition_managed_child(
            "child-1",
            ManagedChildPhase::Authorized,
            None,
            None,
            "premature EXEC"
        )
        .is_err()
    );
    let mut wrong = identity(124);
    wrong.boot_id = "other-boot".into();
    assert!(s.prepare_managed_child("child-1", &wrong, &[]).is_err());
    assert!(
        s.prepare_managed_child("child-1", &identity(123), &[])
            .is_err()
    );
    s.prepare_managed_child("child-1", &identity(124), &[])
        .unwrap();
    s.prepare_managed_child("child-1", &identity(124), &[])
        .unwrap();
    assert!(
        s.prepare_managed_child("child-1", &identity(125), &[])
            .is_err()
    );
    s.reserve_managed_child("assignment", 1, "child-2", "request-2", &r)
        .unwrap();
    assert!(
        s.prepare_managed_child("child-2", &identity(124), &[])
            .is_err()
    );
    s.transition_managed_child(
        "child-1",
        ManagedChildPhase::Authorized,
        None,
        None,
        "authorization persisted",
    )
    .unwrap();
    s.transition_managed_child(
        "child-1",
        ManagedChildPhase::Running,
        None,
        None,
        "executed",
    )
    .unwrap();
}
#[test]
fn required_controls_and_nested_children_are_not_silently_weakened() {
    let d = dir();
    let (s, _, mut r) = setup(d.path());
    r.managed_child_limit = 1;
    assert!(
        s.reserve_managed_child("assignment", 1, "child", "request", &r)
            .is_err()
    );
    r.managed_child_limit = 0;
    r.required_controls.push("cpu.max".into());
    s.reserve_managed_child("assignment", 1, "child", "request", &r)
        .unwrap();
    assert!(
        s.prepare_managed_child("child", &identity(124), &[])
            .is_err()
    );
    let e = ControlEvidence {
        control: "cpu.max".into(),
        available: Some(true),
        permitted: Some(true),
        configured: true,
        applied: false,
        fallback: true,
        scope: "test fixture".into(),
        requested: None,
        effective: None,
        detail: "not applied".into(),
    };
    assert!(
        s.prepare_managed_child("child", &identity(124), &[e])
            .is_err()
    );
}
#[test]
fn parent_release_waits_for_child_release_and_reconciles_after_restart() {
    let d = dir();
    let (s, mut parent, r) = setup(d.path());
    s.reserve_managed_child("assignment", 1, "child", "request", &r)
        .unwrap();
    s.prepare_managed_child("child", &identity(124), &[])
        .unwrap();
    s.transition_managed_child(
        "child",
        ManagedChildPhase::Authorized,
        None,
        None,
        "persisted authorization",
    )
    .unwrap();
    s.transition_managed_child(
        "child",
        ManagedChildPhase::NeedsReconciliation,
        None,
        None,
        "supervisor died",
    )
    .unwrap();
    parent.phase = ExecutionPhase::Released;
    assert!(s.transition(&parent).is_err());
    drop(s);
    let s = StateStore::open(d.path()).unwrap();
    assert_eq!(
        s.all_unreleased_managed_children().unwrap()[0].phase,
        ManagedChildPhase::NeedsReconciliation
    );
    assert!(s.transition(&parent).is_err());
    s.transition_managed_child(
        "child",
        ManagedChildPhase::Released,
        None,
        Some(9),
        "verified owned handle exited",
    )
    .unwrap();
    s.transition(&parent).unwrap();
    assert!(s.all_unreleased_managed_children().unwrap().is_empty());
    assert!(
        s.transition_managed_child("child", ManagedChildPhase::Running, None, None, "resurrect")
            .is_err()
    );
    assert!(
        s.transition_managed_child(
            "child",
            ManagedChildPhase::Released,
            Some(0),
            None,
            "changed termination receipt"
        )
        .is_err()
    );
}
#[test]
fn draining_parent_cannot_authorize_pending_child() {
    let d = dir();
    let (s, mut parent, r) = setup(d.path());
    s.reserve_managed_child("assignment", 1, "child", "request", &r)
        .unwrap();
    s.prepare_managed_child("child", &identity(124), &[])
        .unwrap();
    parent.phase = ExecutionPhase::Draining;
    s.transition(&parent).unwrap();
    assert!(
        s.transition_managed_child(
            "child",
            ManagedChildPhase::Authorized,
            None,
            None,
            "too late"
        )
        .is_err()
    );
    assert!(
        s.reserve_managed_child("assignment", 1, "second", "request-2", &r)
            .is_err()
    );
}
#[test]
fn release_and_child_reservation_race_is_atomic() {
    let d = dir();
    let (s, parent, r) = setup(d.path());
    drop(s);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let root = PathBuf::from(d.path());
    let b = barrier.clone();
    let p = root.clone();
    let release = std::thread::spawn(move || {
        let s = StateStore::open(&p).unwrap();
        let mut parent = parent;
        parent.phase = ExecutionPhase::Released;
        b.wait();
        s.transition(&parent).is_ok()
    });
    let reserve = std::thread::spawn(move || {
        let s = StateStore::open(&root).unwrap();
        barrier.wait();
        s.reserve_managed_child("assignment", 1, "child", "request", &r)
            .is_ok()
    });
    let released = release.join().unwrap();
    let reserved = reserve.join().unwrap();
    assert_ne!(released, reserved, "both operations must not commit");
    let s = StateStore::open(d.path()).unwrap();
    assert!(!released || s.all_unreleased_managed_children().unwrap().is_empty());
}

#[test]
fn child_gpu_visibility_requires_a_positive_parent_budget() {
    let d = dir();
    let (s, _, mut r) = setup(d.path());
    r.resources
        .gpu_memory_mib
        .insert("unassigned-gpu".into(), 0);
    assert!(
        s.reserve_managed_child("assignment", 1, "child", "request", &r)
            .is_err()
    );
    r.resources
        .gpu_memory_mib
        .insert("unassigned-gpu".into(), 1);
    assert!(
        s.reserve_managed_child("assignment", 1, "child", "request", &r)
            .is_err()
    );
}
