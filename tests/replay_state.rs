use resource_manager::{
    execution_model::*,
    model::Resources,
    state::{StateStore, StorageProfile},
};
use std::collections::BTreeMap;

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
fn store() -> (tempfile::TempDir, StateStore) {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store =
        StateStore::open_with_profile(dir.path(), StorageProfile::BurstReplayDeleteExtra).unwrap();
    (dir, store)
}

#[test]
fn weak_reservations_require_remote_replay_safe_opportunistic_contract() {
    let (_dir, store) = store();
    let capacity = resources(10000);
    let ordinary = request("work", "attempt");
    assert!(
        store.reserve(&ordinary, &capacity).is_err(),
        "standalone weak reserve must fail before any row"
    );
    assert!(store.reserve_assigned(&ordinary, &capacity, 0).is_err());
    let mut unsafe_request = ordinary.clone();
    unsafe_request.replay_safe = false;
    assert!(
        store
            .reserve_assigned(&unsafe_request, &capacity, 1)
            .is_err()
    );
    let mut guaranteed = ordinary.clone();
    guaranteed.class = AllocationClass::Guaranteed;
    assert!(store.reserve_assigned(&guaranteed, &capacity, 1).is_err());
    let mut durable = ordinary.clone();
    durable
        .required_controls
        .push("storage.durable_local".into());
    assert!(store.reserve_assigned(&durable, &capacity, 1).is_err());
    assert!(store.executions().unwrap().is_empty());
    let accepted = store.reserve_assigned(&ordinary, &capacity, 4).unwrap();
    assert_eq!(accepted.generation, 4);
    assert_eq!(store.executions().unwrap().len(), 1);
}

fn recovery(
    task: &str,
    assignment: &str,
    generation: u64,
) -> resource_manager::protocol::ReplayRecoveryAllocation {
    resource_manager::protocol::ReplayRecoveryAllocation {
        assignment: resource_manager::protocol::Assignment {
            node_id: "owner".into(),
            generation,
            coordinator_epoch: 42,
            request: request(task, assignment),
            checkpoint: None,
        },
        prepared: None,
        previous_boot_id: Some("previous-session-boot".into()),
        lease_sequence: 1,
    }
}
fn preparation(
    recovery: &resource_manager::protocol::ReplayRecoveryAllocation,
    boot: &str,
) -> ExecutionRecord {
    let request = &recovery.assignment.request;
    ExecutionRecord {
        task_id: request.task_id.clone(),
        assignment_id: request.assignment_id.clone(),
        generation: recovery.assignment.generation,
        class: request.class,
        phase: ExecutionPhase::Prepared,
        resources: request.resources.clone(),
        backend: "fixture-verified-backend".into(),
        identity: Some(ProcessIdentity {
            pid: 123,
            boot_id: boot.into(),
            start_time: 456,
            assignment_id: request.assignment_id.clone(),
            generation: recovery.assignment.generation,
        }),
        evidence: vec![],
        detail: "prepared".into(),
    }
}
fn marker(record: &ExecutionRecord, control: &str) -> bool {
    record.evidence.iter().any(|e| e.control == control)
}
fn current_boot() -> Option<String> {
    resource_manager::supervision::process_identity(std::process::id(), "self-test", 0)
        .ok()
        .map(|identity| identity.boot_id)
}

#[test]
fn import_retains_full_over_capacity_history_and_max_generation_in_any_order() {
    let (dir, store) = store();
    let mut newer = recovery("task", "attempt-7", 7);
    newer.assignment.request.max_attempts = Some(1); // Retention must not apply fresh admission policy.
    newer.assignment.request.resources.cpu_millicores = 900_000;
    newer
        .assignment
        .request
        .resources
        .gpu_memory_mib
        .insert("old-gpu".into(), 99_999);
    newer.assignment.request.argv = vec!["original-command".into(), "original-argument".into()];
    newer
        .assignment
        .request
        .env
        .insert("ORIGINAL".into(), "preserved".into());
    let original = serde_json::to_value(&newer.assignment.request).unwrap();
    store.import_replay_reservation(&newer).unwrap();
    let mut older = newer.clone();
    older.assignment.request.assignment_id = "attempt-3".into();
    older.assignment.generation = 3;
    store.import_replay_reservation(&older).unwrap();
    let mut historical = recovery("historical-unsafe", "historical-4", 4);
    historical.assignment.request.replay_safe = false;
    historical.assignment.request.class = AllocationClass::Guaranteed;
    store.import_replay_reservation(&historical).unwrap();
    assert_eq!(store.task("task").unwrap().generation, 7);
    assert_eq!(
        store.task("task").unwrap().assignment_id.as_deref(),
        Some("attempt-7")
    );
    assert_eq!(store.execution_allocations().unwrap().len(), 3);
    assert_eq!(
        serde_json::to_value(store.execution_request("attempt-7").unwrap()).unwrap(),
        original
    );
    assert!(
        store
            .reserve_assigned(&request("new", "new-1"), &resources(1000), 1)
            .is_err()
    );
    assert!(
        store
            .reserve_assigned(&request("task", "replacement"), &resources(u64::MAX), 8)
            .is_err()
    );
    assert!(
        store.import_replay_reservation(&newer).is_err(),
        "duplicates must not replace history"
    );
    let mut conflicting = older.clone();
    conflicting.assignment.request.assignment_id = "same-generation-different-id".into();
    assert!(store.import_replay_reservation(&conflicting).is_err());
    assert_eq!(store.execution_allocations().unwrap().len(), 3);
    drop(store);
    let reopened =
        StateStore::open_with_profile(dir.path(), StorageProfile::BurstReplayDeleteExtra).unwrap();
    assert_eq!(reopened.active_executions().unwrap().len(), 3);
    assert!(
        reopened
            .active_executions()
            .unwrap()
            .iter()
            .all(|r| r.phase == ExecutionPhase::NeedsReconciliation)
    );
    assert_eq!(
        reopened
            .execution_record("attempt-7")
            .unwrap()
            .unwrap()
            .resources,
        newer.assignment.request.resources
    );
}

#[test]
fn import_validates_preparation_and_preserves_identity_backend_and_required_evidence() {
    let (_dir, store) = store();
    let mut recovery = recovery("task", "attempt", 8);
    recovery
        .assignment
        .request
        .required_controls
        .push("fixture.control".into());
    let mut prepared = preparation(&recovery, "known-old-boot");
    let good = ControlEvidence {
        control: "fixture.control".into(),
        available: Some(true),
        permitted: Some(true),
        configured: true,
        applied: true,
        fallback: false,
        scope: "owned-leaf".into(),
        requested: Some("original".into()),
        effective: Some("verified".into()),
        detail: "original proof".into(),
    };
    prepared.evidence.push(good.clone());
    recovery.prepared = Some(prepared.clone());
    let imported = store.import_replay_reservation(&recovery).unwrap();
    assert_eq!(imported.identity, prepared.identity);
    assert_eq!(imported.backend, prepared.backend);
    assert_eq!(imported.resources, prepared.resources);
    assert_eq!(imported.phase, ExecutionPhase::NeedsReconciliation);
    assert!(imported.evidence.contains(&good));
    assert!(marker(&imported, "recovery.imported"));
    assert!(!marker(&imported, "recovery.missing_prepared"));
    assert_eq!(
        imported
            .evidence
            .iter()
            .find(|e| e.control == "recovery.previous_boot")
            .unwrap()
            .effective
            .as_deref(),
        Some("previous-session-boot")
    );
}

#[test]
fn malformed_preparation_never_imports_partial_state() {
    for corruption in 0..9 {
        let (_dir, store) = store();
        let mut recovery = recovery("task", "attempt", 2);
        let mut prepared = preparation(&recovery, "fixture");
        match corruption {
            0 => prepared.task_id = "different".into(),
            1 => prepared.generation = 3,
            2 => prepared.resources.cpu_millicores += 1,
            3 => prepared.identity.as_mut().unwrap().start_time = 0,
            4 => prepared.identity.as_mut().unwrap().assignment_id = "different".into(),
            5 => prepared.backend = "unprepared".into(),
            6 => prepared.phase = ExecutionPhase::Released,
            7 => recovery
                .assignment
                .request
                .required_controls
                .push("missing-proof".into()),
            8 => prepared.class = AllocationClass::Guaranteed,
            _ => unreachable!(),
        }
        recovery.prepared = Some(prepared);
        assert!(
            store.import_replay_reservation(&recovery).is_err(),
            "case {corruption}"
        );
        assert!(store.executions().unwrap().is_empty());
        assert!(store.task("task").is_err());
    }
}

#[test]
fn missing_preparation_stays_unknown_and_recovery_markers_cannot_be_removed() {
    let (_dir, store) = store();
    let recovery = recovery("task", "attempt", 1);
    let mut imported = store.import_replay_reservation(&recovery).unwrap();
    assert!(imported.identity.is_none());
    assert!(marker(&imported, "recovery.missing_prepared"));
    let mut stripped = imported.clone();
    stripped.evidence.clear();
    assert!(store.transition(&stripped).is_err());
    imported.phase = ExecutionPhase::Released;
    assert!(store.transition(&imported).is_err());
    assert_eq!(store.execution_allocations().unwrap().len(), 1);
}

#[test]
fn unacknowledged_or_unknown_child_families_require_verified_boot_change_for_release() {
    for children in [false, true] {
        for old_boot in [false, true] {
            let (_dir, store) = store();
            let mut recovery = recovery("task", "attempt", 1);
            if children {
                recovery.assignment.request.managed_child_limit = 2;
            } else {
                recovery.lease_sequence = 0;
            }
            let current = current_boot();
            let boot = if old_boot {
                format!(
                    "old-{}",
                    current.as_deref().unwrap_or("unknown-native-boot")
                )
            } else {
                current
                    .clone()
                    .unwrap_or_else(|| "unknown-native-boot".into())
            };
            recovery.prepared = Some(preparation(&recovery, &boot));
            let mut imported = store.import_replay_reservation(&recovery).unwrap();
            assert!(marker(
                &imported,
                if children {
                    "recovery.unknown_children"
                } else {
                    "recovery.unauthorized"
                }
            ));
            imported.phase = ExecutionPhase::Released;
            // Native boot inspection may be unavailable in a restricted test
            // runner; that case must refuse every release, never skip the guard.
            let release_proved = old_boot && current.is_some();
            assert_eq!(store.transition(&imported).is_ok(), release_proved);
            assert_eq!(
                store.execution_allocations().unwrap().is_empty(),
                release_proved
            );
        }
    }
}

#[test]
fn ordinary_weak_family_reconciliation_cannot_treat_lost_child_rows_as_empty() {
    let (_dir, store) = store();
    let mut request = request("ordinary-family", "family-attempt");
    request.single_process = false;
    request.managed_child_limit = 2;
    let mut record = store
        .reserve_assigned(&request, &resources(2000), 1)
        .unwrap();
    record.identity = Some(ProcessIdentity {
        pid: std::process::id(),
        boot_id: current_boot().unwrap_or_else(|| "native-unavailable".into()),
        start_time: 1,
        assignment_id: request.assignment_id.clone(),
        generation: 1,
    });
    record.backend = "rootless".into();
    record.phase = ExecutionPhase::Prepared;
    store.transition(&record).unwrap();
    record.phase = ExecutionPhase::NeedsReconciliation;
    store.transition(&record).unwrap();
    // This state is indistinguishable from a journal rollback that lost all
    // child rows. There are no import markers or registered children to help.
    assert!(record.evidence.is_empty());
    assert!(
        store
            .managed_children(&record.assignment_id)
            .unwrap()
            .is_empty()
    );
    record.phase = ExecutionPhase::Released;
    assert!(store.transition(&record).is_err());
    assert_eq!(store.execution_allocations().unwrap().len(), 1);
}

#[test]
fn strong_storage_refuses_replay_import_and_storage_control_matches_verified_profile() {
    for profile in [
        StorageProfile::WalFull,
        StorageProfile::DeleteExtra,
        StorageProfile::BurstReplayDeleteExtra,
    ] {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = StateStore::open_with_profile(dir.path(), profile).unwrap();
        if !profile.is_replayable() {
            assert!(
                store
                    .import_replay_reservation(&recovery("task", "attempt", 1))
                    .is_err()
            );
        }
        let evidence = store.storage_control_evidence().unwrap();
        assert_eq!(evidence.len(), 1);
        let expected = if profile.is_replayable() {
            "storage.replayable_local"
        } else {
            "storage.durable_local"
        };
        assert_eq!(evidence[0].control, expected);
        assert!(evidence[0].applied && !evidence[0].fallback);
        assert_eq!(evidence[0].available, Some(true));
        assert_eq!(evidence[0].permitted, Some(true));
        assert_eq!(evidence[0].detail, profile.assurance().detail());
        let db = rusqlite::Connection::open(store.database_path()).unwrap();
        db.pragma_update(None, "user_version", 99).unwrap();
        assert!(store.storage_control_evidence().is_err());
    }
}

#[test]
fn verified_release_preserves_old_history_and_allows_only_newer_remote_attempt() {
    let (_dir, store) = store();
    let mut recovery = recovery("task", "old-attempt", 7);
    recovery.prepared = Some(preparation(&recovery, "known-old-boot"));
    let mut imported = store.import_replay_reservation(&recovery).unwrap();
    assert!(
        store
            .reserve_assigned(&request("task", "next-attempt"), &resources(10000), 8)
            .is_err()
    );
    imported.phase = ExecutionPhase::Released; // Caller supplied verified direct-process release.
    store.transition(&imported).unwrap();
    assert!(
        store
            .reserve_assigned(&request("task", "stale-attempt"), &resources(10000), 7)
            .is_err()
    );
    let next = store
        .reserve_assigned(&request("task", "next-attempt"), &resources(10000), 8)
        .unwrap();
    assert_eq!(next.generation, 8);
    assert_eq!(
        store
            .execution_record("old-attempt")
            .unwrap()
            .unwrap()
            .phase,
        ExecutionPhase::Released
    );
    assert_eq!(store.executions().unwrap().len(), 2);
}
