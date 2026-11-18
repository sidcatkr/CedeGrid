use cedegrid::{
    agent, config::Config, execution_model::*, model::Resources, state::StateStore, supervision,
    telemetry::ManagedCollector,
};
use std::{collections::BTreeMap, fs::OpenOptions};
fn setup() -> (StateDir, Config, StateStore, ExecutionRecord) {
    let dir = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
    let config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    let store = StateStore::open(&config.state_dir).unwrap();
    let resources = Resources {
        cpu_millicores: 100,
        ram_mib: 32,
        gpu_memory_mib: BTreeMap::new(),
    };
    let request = LaunchRequest {
        task_id: "task".into(),
        assignment_id: "attempt".into(),
        argv: vec!["unused".into()],
        cwd: dir.path().to_path_buf(),
        env: BTreeMap::new(),
        resources: resources.clone(),
        replay_safe: true,
        class: AllocationClass::Guaranteed,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
        contention: None,
    };
    let mut record = store.reserve(&request, &resources).unwrap();
    record.backend = "rootless".into();
    record.identity = Some(
        supervision::process_identity(std::process::id(), "attempt", record.generation).unwrap(),
    );
    record.phase = ExecutionPhase::Prepared;
    store.transition(&record).unwrap();
    (dir, config, store, record)
}
#[test]
fn reconciliation_preserves_a_live_identity_and_never_signals_it() {
    let (_dir, config, store, record) = setup();
    let result = agent::reconcile(&config).unwrap();
    assert_eq!(result[0].phase, ExecutionPhase::NeedsReconciliation);
    assert_eq!(result[0].identity, record.identity);
    assert_eq!(store.execution_allocations().unwrap().len(), 1);
    assert_eq!(
        supervision::process_identity(std::process::id(), "attempt", record.generation).unwrap(),
        record.identity.unwrap()
    );
}
/// A dead rootless run is released only once its recorded descendants are gone too: one that
/// still runs keeps the reservation, and nothing is recovered without proof that the
/// supervisor is gone (FR-088).
#[test]
fn a_run_with_a_living_recorded_descendant_is_not_released() {
    let dir = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
    let config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    let store = StateStore::open(&config.state_dir).unwrap();
    let (_, _, _, template) = setup();
    let request = LaunchRequest {
        assignment_id: "gone-leader".into(),
        task_id: "gone-task".into(),
        cwd: dir.path().to_path_buf(),
        ..store_request(&template)
    };
    let mut record = store.reserve(&request, &request.resources).unwrap();
    record.backend = "rootless".into();
    // A leader that has exited: a short-lived child's identity, read while it ran.
    let mut child = std::process::Command::new("sleep")
        .arg("0.2")
        .spawn()
        .unwrap();
    record.identity =
        Some(supervision::process_identity(child.id(), "gone-leader", record.generation).unwrap());
    child.wait().unwrap();
    record.phase = ExecutionPhase::Prepared;
    store.transition(&record).unwrap();
    let alive = supervision::process_identity(std::process::id(), "gone-leader", 1).unwrap();
    let attempt = config.state_dir.join("attempts").join("gone-leader");
    std::fs::create_dir_all(&attempt).unwrap();
    let family = serde_json::json!({
        "class": "guaranteed",
        "boot_id": alive.boot_id,
        // SAFETY: getuid and getgid cannot fail.
        "uid": unsafe { libc::getuid() },
        "gid": unsafe { libc::getgid() },
        "descendants": [{"pid": alive.pid, "start_time": alive.start_time}],
    });
    std::fs::write(attempt.join("family.json"), family.to_string()).unwrap();
    let phase = |records: Vec<ExecutionRecord>| {
        records
            .into_iter()
            .find(|r| r.assignment_id == "gone-leader")
            .map(|r| r.phase)
    };
    assert_eq!(
        phase(agent::reconcile(&config).unwrap()),
        Some(ExecutionPhase::NeedsReconciliation)
    );
    // No proof that the supervisor is gone, so nothing was recovered or signalled.
    assert!(!attempt.join("family-recovery.json").exists());
    std::fs::remove_file(attempt.join("family.json")).unwrap();
    assert_eq!(
        phase(agent::reconcile(&config).unwrap()),
        Some(ExecutionPhase::Released)
    );
}

/// The request the shared setup reserved, for a test that reserves its own record.
fn store_request(record: &ExecutionRecord) -> LaunchRequest {
    LaunchRequest {
        task_id: record.task_id.clone(),
        assignment_id: record.assignment_id.clone(),
        argv: vec!["unused".into()],
        cwd: std::env::current_dir().unwrap(),
        env: BTreeMap::new(),
        resources: record.resources.clone(),
        replay_safe: true,
        class: record.class,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
        contention: None,
    }
}

#[test]
fn reconciliation_refuses_live_agent_lock() {
    let (_dir, config, _store, _record) = setup();
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.state_dir.join("agent.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(agent::reconcile(&config).is_err());
}
#[test]
fn pending_usage_stays_unobserved_and_reused_pid_identity_never_becomes_zero() {
    let (_dir, config, _store, mut record) = setup();
    let mut collector = ManagedCollector::new(&config).unwrap();
    collector.prime(std::slice::from_ref(&record));
    let (_, allocations) = collector.sample(std::slice::from_ref(&record)).unwrap();
    assert!(allocations[0].observed.is_none());
    record.phase = ExecutionPhase::Running;
    record.identity.as_mut().unwrap().start_time += 1;
    let (_, allocations) = collector.sample(&[record]).unwrap();
    assert!(allocations[0].observed.is_none());
    assert_eq!(allocations[0].requested.ram_mib, 32);
}
/// A job's requested modes reach the policy's allocations through the record (FR-030).
#[test]
fn requested_modes_travel_from_the_request_to_the_policy_allocation() {
    use cedegrid::contention::{ContentionMode, ResourceKind};
    let dir = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
    let config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    let store = StateStore::open(&config.state_dir).unwrap();
    let resources = Resources {
        cpu_millicores: 100,
        ram_mib: 32,
        gpu_memory_mib: BTreeMap::new(),
    };
    let modes = BTreeMap::from([(ResourceKind::Cpu, ContentionMode::YieldFirst)]);
    let request = LaunchRequest {
        task_id: "task".into(),
        assignment_id: "attempt".into(),
        argv: vec!["unused".into()],
        cwd: dir.path().to_path_buf(),
        env: BTreeMap::new(),
        resources: resources.clone(),
        replay_safe: true,
        class: AllocationClass::Opportunistic,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
        contention: Some(JobContention {
            modes: modes.clone(),
            admit_immediately: None,
            unprotected: None,
        }),
    };
    let record = store.reserve(&request, &resources).unwrap();
    assert_eq!(record.modes, modes);
    assert_eq!(store.execution_allocations().unwrap()[0].modes, modes);
    let mut collector = ManagedCollector::new(&config).unwrap();
    let (_, allocations) = collector.sample(std::slice::from_ref(&record)).unwrap();
    assert_eq!(allocations[0].modes, modes);
}

include!("support/state_dir.rs");
