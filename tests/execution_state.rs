use cedegrid::{
    execution_model::*,
    model::Resources,
    state::{DATABASE_FILENAME, StateStore, TaskStatus},
};
use std::collections::BTreeMap;

fn store() -> (tempfile::TempDir, StateStore) {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(dir.path()).unwrap();
    (dir, store)
}
fn resources(cpu: u64) -> Resources {
    Resources {
        cpu_millicores: cpu,
        ram_mib: 64,
        gpu_memory_mib: BTreeMap::new(),
    }
}
fn request(task: &str, assignment: &str) -> LaunchRequest {
    LaunchRequest {
        task_id: task.into(),
        assignment_id: assignment.into(),
        argv: vec!["unused".into()],
        cwd: std::env::current_dir().unwrap(),
        env: BTreeMap::new(),
        resources: resources(1000),
        replay_safe: true,
        class: AllocationClass::Opportunistic,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: false,
    }
}
fn prepared(mut row: ExecutionRecord) -> ExecutionRecord {
    row.phase = ExecutionPhase::Prepared;
    row.backend = "test-owned-backend".into();
    row.identity = Some(ProcessIdentity {
        pid: 123,
        boot_id: "fixture-boot".into(),
        start_time: 456,
        assignment_id: row.assignment_id.clone(),
        generation: row.generation,
    });
    row
}

#[test]
fn pending_and_uncertain_reservations_survive_reopen_and_block_replacement() {
    let (dir, store) = store();
    let mut row = store
        .reserve(&request("a", "a1"), &resources(1000))
        .unwrap();
    assert!(
        store
            .reserve(&request("b", "b1"), &resources(1000))
            .is_err()
    );
    assert_eq!(
        store.execution_allocations().unwrap()[0].phase,
        cedegrid::model::AllocationPhase::Pending
    );
    row.phase = ExecutionPhase::NeedsReconciliation;
    store.transition(&row).unwrap();
    store.mark_uncertain("a", 1).unwrap(); // replay-safe does not override capacity fencing
    drop(store);
    let reopened = StateStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened.execution_allocations().unwrap()[0].requested,
        resources(1000)
    );
    assert!(
        reopened
            .reserve(&request("a", "a2"), &resources(10000))
            .is_err()
    );
    assert!(
        reopened
            .reserve(&request("b", "b1"), &resources(1000))
            .is_err()
    );
    assert!(reopened.assign("a", "legacy-replacement").is_err());
    assert_eq!(
        reopened.execution_record("a1").unwrap().unwrap().phase,
        ExecutionPhase::NeedsReconciliation
    );
    assert_eq!(reopened.active_executions().unwrap().len(), 1);
    assert!(reopened.execution_record("missing").unwrap().is_none());
    row.phase = ExecutionPhase::Released;
    reopened.transition(&row).unwrap();
    assert!(reopened.execution_allocations().unwrap().is_empty());
    assert!(reopened.active_executions().unwrap().is_empty());
    assert_eq!(
        reopened.execution_record("a1").unwrap().unwrap().phase,
        ExecutionPhase::Released
    );
    assert_eq!(
        reopened
            .reserve(&request("a", "a2"), &resources(1000))
            .unwrap()
            .generation,
        2
    );
}

#[test]
fn observe_allocations_retain_durable_class_across_pending_and_uncertain_states() {
    let (dir, store) = store();
    let mut request = request("guaranteed", "guaranteed-1");
    request.class = AllocationClass::Guaranteed;
    let mut row = store.reserve(&request, &resources(1000)).unwrap();
    let pending = store.execution_allocations().unwrap();
    assert_eq!(pending[0].class, AllocationClass::Guaranteed);
    assert_eq!(pending[0].requested, resources(1000));
    row.phase = ExecutionPhase::NeedsReconciliation;
    store.transition(&row).unwrap();
    drop(store);
    let reopened = StateStore::open(dir.path()).unwrap();
    let uncertain = reopened.execution_allocations().unwrap();
    assert_eq!(uncertain[0].class, AllocationClass::Guaranteed);
    assert_eq!(
        uncertain[0].phase,
        cedegrid::model::AllocationPhase::Running
    );
    assert_eq!(uncertain[0].requested, resources(1000));
    assert!(uncertain[0].observed.is_none());
}

#[test]
fn launch_barrier_requires_order_identity_and_nonfallback_control_evidence() {
    let (_dir, store) = store();
    let mut req = request("a", "a1");
    req.required_controls = vec!["cpu.max".into()];
    let row = store.reserve(&req, &resources(1000)).unwrap();
    let mut candidate = prepared(row.clone());
    candidate.phase = ExecutionPhase::Authorized;
    assert!(store.transition(&candidate).is_err()); // no skipped preparation
    candidate.phase = ExecutionPhase::Prepared;
    candidate.identity = None;
    assert!(store.transition(&candidate).is_err());
    let mut candidate = prepared(row);
    store.transition(&candidate).unwrap();
    candidate.phase = ExecutionPhase::Authorized;
    assert!(store.transition(&candidate).is_err());
    assert_eq!(
        store.executions().unwrap()[0].phase,
        ExecutionPhase::Prepared
    );
    let good = ControlEvidence {
        control: "cpu.max".into(),
        available: Some(true),
        permitted: Some(true),
        configured: true,
        applied: true,
        fallback: false,
        scope: "fixture".into(),
        requested: Some("1000 1000".into()),
        effective: Some("1000 1000".into()),
        detail: "verified fixture".into(),
    };
    for variant in 0..4 {
        let mut bad = good.clone();
        match variant {
            0 => bad.available = None,
            1 => bad.permitted = Some(false),
            2 => bad.applied = false,
            _ => bad.fallback = true,
        }
        candidate.evidence = vec![bad];
        assert!(store.transition(&candidate).is_err());
    }
    candidate.evidence = vec![good];
    store.transition(&candidate).unwrap();
    candidate.phase = ExecutionPhase::Running;
    store.transition(&candidate).unwrap();
    assert_eq!(
        store.executions().unwrap()[0].phase,
        ExecutionPhase::Running
    );
}

#[test]
fn persistent_attempt_identity_and_reservation_are_immutable() {
    let (_dir, store) = store();
    let row = store
        .reserve(&request("a", "a1"), &resources(1000))
        .unwrap();
    let row = prepared(row);
    store.transition(&row).unwrap();
    for variant in 0..7 {
        let mut changed = row.clone();
        changed.phase = ExecutionPhase::Authorized;
        match variant {
            0 => changed.generation += 1,
            1 => changed.resources.cpu_millicores += 1,
            2 => changed.identity.as_mut().unwrap().pid += 1,
            3 => changed.identity.as_mut().unwrap().start_time += 1,
            4 => changed.identity.as_mut().unwrap().boot_id = "other-boot".into(),
            5 => changed.class = AllocationClass::Guaranteed,
            _ => changed.backend = "other-backend".into(),
        }
        assert!(store.transition(&changed).is_err());
    }
}

#[test]
fn capacity_reservation_is_atomic_across_connections() {
    let (dir, one) = store();
    let two = StateStore::open(dir.path()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [one, two]
        .into_iter()
        .enumerate()
        .map(|(i, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .reserve(
                        &request(&format!("t{i}"), &format!("a{i}")),
                        &resources(1000),
                    )
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().then_some(()))
            .count(),
        1
    );
}

#[test]
fn offline_schema_two_upgrade_preserves_tasks_and_observations() {
    let (dir, store) = store();
    store.submit("existing-task", false).unwrap();
    let original = rusqlite::Connection::open(store.database_path()).unwrap();
    original.execute("INSERT INTO observations(observed_at_unix_ms,node_id,snapshot_json,decision_json) VALUES(7,'existing-node','{}','{}')",[]).unwrap();
    drop(original);
    drop(store);
    // Audited 0.1 WAL state is schema 2. Startup cannot migrate it implicitly.
    let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILENAME)).unwrap();
    db.execute_batch("PRAGMA user_version=2; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(db);
    assert!(StateStore::open(dir.path()).is_err());
    let backup_root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    cedegrid::upgrade::upgrade(
        dir.path(),
        &backup_root.path().join("bundle"),
        cedegrid::state::StorageProfile::WalFull,
        true,
    )
    .unwrap();
    let upgraded = StateStore::open(dir.path()).unwrap();
    assert_eq!(
        upgraded.status("existing-task").unwrap(),
        TaskStatus::NeedsReconciliation
    );
    assert!(upgraded.executions().unwrap().is_empty());
    assert_eq!(upgraded.observations(10).unwrap().len(), 1);
    assert_eq!(upgraded.durability_settings().unwrap().schema_version, 5);
    assert!(upgraded.assign("existing-task", "forbidden-retry").is_err());
    upgraded
        .reserve(&request("new-task", "new-attempt"), &resources(1000))
        .unwrap();
}

#[test]
fn unaudited_schema_one_requires_external_legacy_recovery_without_mutation() {
    let (dir, store) = store();
    store.submit("retained", false).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILENAME)).unwrap();
    db.execute_batch("DROP TABLE execution_events; DROP TABLE executions; PRAGMA user_version=1; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    drop(db);
    let before = std::fs::read(dir.path().join(DATABASE_FILENAME)).unwrap();
    assert!(StateStore::open(dir.path()).is_err());
    let backup_root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let backup = backup_root.path().join("bundle");
    assert!(
        cedegrid::upgrade::upgrade(
            dir.path(),
            &backup,
            cedegrid::state::StorageProfile::WalFull,
            true
        )
        .is_err()
    );
    assert!(!backup.exists());
    assert_eq!(
        before,
        std::fs::read(dir.path().join(DATABASE_FILENAME)).unwrap()
    );
}

#[test]
fn remote_attempt_generation_is_preserved_and_cannot_reuse_uncertain_capacity() {
    let (_dir, store) = store();
    let mut row = store
        .reserve_assigned(&request("remote", "r7"), &resources(1000), 7)
        .unwrap();
    assert_eq!(row.generation, 7);
    assert!(
        store
            .reserve_assigned(&request("remote", "r8"), &resources(10000), 8)
            .is_err()
    );
    row.phase = ExecutionPhase::Released;
    store.transition(&row).unwrap();
    assert!(
        store
            .reserve_assigned(&request("remote", "r6"), &resources(1000), 6)
            .is_err()
    );
    assert_eq!(
        store
            .reserve_assigned(&request("remote", "r8"), &resources(1000), 8)
            .unwrap()
            .generation,
        8
    );
}

#[test]
fn gpu_pending_reservations_are_charged_once_and_require_uuid_capacity() {
    let (_dir, store) = store();
    let mut req = request("gpu", "g1");
    req.resources.gpu_memory_mib.insert("GPU-test".into(), 1024);
    assert!(store.reserve_assigned(&req, &resources(10000), 1).is_err());
    let mut capacity = resources(10000);
    capacity.ram_mib = 1000;
    capacity.gpu_memory_mib.insert("GPU-test".into(), 1024);
    store.reserve_assigned(&req, &capacity, 1).unwrap();
    let mut second = request("gpu2", "g2");
    second.resources.gpu_memory_mib.insert("GPU-test".into(), 1);
    assert!(store.reserve_assigned(&second, &capacity, 1).is_err());
    assert_eq!(
        store.execution_allocations().unwrap()[0]
            .requested
            .gpu_memory_mib["GPU-test"],
        1024
    );
}

#[test]
fn assigned_admission_keeps_uncertain_gpu_charges_scoped_to_requested_devices() {
    let (directory, store) = store();
    let mut original = request("blocked-gpu", "blocked-1");
    original
        .resources
        .gpu_memory_mib
        .insert("GPU-blocked".into(), 1024);
    let mut initial = resources(4000);
    initial.ram_mib = 256;
    initial.gpu_memory_mib.insert("GPU-blocked".into(), 1024);
    initial.gpu_memory_mib.insert("GPU-good".into(), 512);
    let mut uncertain = store.reserve_assigned(&original, &initial, 1).unwrap();
    uncertain.phase = ExecutionPhase::NeedsReconciliation;
    store.transition(&uncertain).unwrap();
    drop(store);

    // The agent zeroes this UUID's schedulable budget while retaining its full
    // reservation. Shared CPU/RAM and the other GPU still have room.
    let store = StateStore::open(directory.path()).unwrap();
    let mut eligible = initial.clone();
    eligible.gpu_memory_mib.insert("GPU-blocked".into(), 0);
    store
        .reserve_assigned(&request("cpu", "cpu-1"), &eligible, 1)
        .unwrap();
    let mut good = request("other-gpu", "good-1");
    good.resources.gpu_memory_mib.insert("GPU-good".into(), 512);
    store.reserve_assigned(&good, &eligible, 1).unwrap();

    let retained = store.execution_record("blocked-1").unwrap().unwrap();
    assert_eq!(retained.phase, ExecutionPhase::NeedsReconciliation);
    assert_eq!(retained.resources, original.resources);
    assert_eq!(store.active_executions().unwrap().len(), 3);

    // Unrelated admission does not forget the unavailable UUID or spend pending
    // capacity twice; neither same-device request can fit alongside its charge.
    for device in ["GPU-blocked", "GPU-good"] {
        let mut denied = request("same-device", "same-device-1");
        denied.resources.gpu_memory_mib.insert(device.into(), 1);
        assert!(store.reserve_assigned(&denied, &eligible, 1).is_err());
    }
    // Even restoring a positive blocked-device budget cannot erase its 1024 MiB
    // uncertain charge when checking a new request for that same UUID.
    let mut same = request("same-blocked", "same-blocked-1");
    same.resources
        .gpu_memory_mib
        .insert("GPU-blocked".into(), 1);
    assert!(store.reserve_assigned(&same, &initial, 1).is_err());
    assert!(store.reserve_assigned(&original, &initial, 2).is_err());

    // All three reservations continue contributing to every shared dimension.
    let mut insufficient_cpu = eligible.clone();
    insufficient_cpu.cpu_millicores = 3999;
    assert!(
        store
            .reserve_assigned(&request("cpu-excess", "cpu-excess-1"), &insufficient_cpu, 1)
            .is_err()
    );
    let mut insufficient_ram = eligible;
    insufficient_ram.ram_mib = 255;
    assert!(
        store
            .reserve_assigned(&request("ram-excess", "ram-excess-1"), &insufficient_ram, 1)
            .is_err()
    );
    assert_eq!(store.active_executions().unwrap().len(), 3);
}

#[test]
fn assigned_multi_gpu_request_checks_every_requested_uuid_with_pending_charges() {
    let (_directory, store) = store();
    let mut capacity = resources(4000);
    capacity.ram_mib = 256;
    capacity.gpu_memory_mib = BTreeMap::from([("GPU-a".into(), 1024), ("GPU-b".into(), 1024)]);
    let mut pending = request("pending", "pending-1");
    pending
        .resources
        .gpu_memory_mib
        .insert("GPU-a".into(), 1024);
    store.reserve_assigned(&pending, &capacity, 1).unwrap();
    let mut both = request("both", "both-1");
    both.resources.gpu_memory_mib = BTreeMap::from([("GPU-a".into(), 1), ("GPU-b".into(), 1)]);
    assert!(store.reserve_assigned(&both, &capacity, 1).is_err());
    both.resources.gpu_memory_mib.remove("GPU-a");
    store.reserve_assigned(&both, &capacity, 1).unwrap();
    assert_eq!(store.active_executions().unwrap().len(), 2);
}

#[test]
fn local_and_remote_reservations_cannot_exceed_or_change_task_attempt_cap() {
    let d = tempfile::Builder::new()
        .prefix(".attempt-limit-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let s = StateStore::open(d.path()).unwrap();
    let mut r = request("bounded", "first");
    r.max_attempts = Some(1);
    let mut first = s.reserve(&r, &resources(2000)).unwrap();
    first.phase = ExecutionPhase::Released;
    s.transition(&first).unwrap();
    s.mark_uncertain("bounded", 1).unwrap();
    r.assignment_id = "second".into();
    assert!(s.reserve(&r, &resources(2000)).is_err());
    assert!(s.reserve_assigned(&r, &resources(2000), 2).is_err());
    r.max_attempts = None;
    assert!(s.reserve(&r, &resources(2000)).is_err());
    assert_eq!(s.task("bounded").unwrap().generation, 1);
    r.task_id = "zero".into();
    r.max_attempts = Some(0);
    assert!(s.reserve(&r, &resources(2000)).is_err());
}

#[test]
fn rollback_profile_preserves_launch_barrier_and_uncertain_capacity_across_reopen() {
    use cedegrid::state::StorageProfile;
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store =
        StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).unwrap();
    let pending = store
        .reserve(&request("rollback", "rollback-1"), &resources(1000))
        .unwrap();
    let mut unauthorized = pending.clone();
    unauthorized.phase = ExecutionPhase::Authorized;
    assert!(store.transition(&unauthorized).is_err());
    let mut row = prepared(pending);
    store.transition(&row).unwrap();
    row.phase = ExecutionPhase::Authorized;
    store.transition(&row).unwrap();
    drop(store);
    let store =
        StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).unwrap();
    assert_eq!(
        store.execution_record("rollback-1").unwrap().unwrap().phase,
        ExecutionPhase::Authorized
    );
    row.phase = ExecutionPhase::NeedsReconciliation;
    store.transition(&row).unwrap();
    drop(store);
    let store =
        StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).unwrap();
    assert!(
        store
            .reserve(&request("replacement", "replacement-1"), &resources(1000))
            .is_err()
    );
    assert_eq!(
        store.execution_allocations().unwrap()[0].requested,
        resources(1000)
    );
    assert_eq!(store.active_executions().unwrap().len(), 1);
    row.phase = ExecutionPhase::Released;
    store.transition(&row).unwrap();
    assert!(store.execution_allocations().unwrap().is_empty());
    assert!(
        store
            .reserve(&request("replacement", "replacement-1"), &resources(1000))
            .is_ok()
    );
}
