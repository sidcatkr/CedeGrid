use resource_manager::{
    agent, config::Config, execution_model::*, model::Resources, state::StateStore, supervision,
    telemetry::ManagedCollector,
};
use std::{collections::BTreeMap, fs::OpenOptions};
fn setup() -> (tempfile::TempDir, Config, StateStore, ExecutionRecord) {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
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
