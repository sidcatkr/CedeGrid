#![cfg(unix)]
use cedegrid::{
    state::{SCHEMA_VERSION, StateStore, StorageProfile},
    upgrade,
};
use rusqlite::Connection;
use std::{fs, path::Path};

fn mark_legacy(root: &Path, version: i64) {
    let db = Connection::open(root.join("state.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
}

#[test]
fn offline_upgrade_preserves_payload_bytes_receipts_outboxes_and_profile() {
    for (profile, previous) in [
        (StorageProfile::WalFull, 2),
        (StorageProfile::DeleteExtra, 3),
    ] {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let state = temp.path().join("state");
        let store = StateStore::open_with_profile(&state, profile).unwrap();
        store.submit("historical-RESMGR-identity", true).unwrap();
        drop(store);
        fs::create_dir(state.join("attempts")).unwrap();
        fs::write(
            state.join("attempts/legacy-result.json"),
            b"{ \"historical\": \"RESMGR\", \"n\": 9007199254740993 }\n",
        )
        .unwrap();
        mark_legacy(&state, previous);
        assert!(
            StateStore::open_with_profile(&state, profile).is_err(),
            "startup must refuse implicit migration"
        );
        let original = fs::read(state.join("state.sqlite3")).unwrap();
        let bundle = temp.path().join("bundle");
        let report = upgrade::upgrade(&state, &bundle, profile, true).unwrap();
        assert_eq!(report.from_schema, previous);
        assert_eq!(report.to_schema, SCHEMA_VERSION);
        assert_eq!(report.held_tasks, 1);
        assert!(!report.requires_authenticated_recovery);
        assert_eq!(fs::read(bundle.join("state.sqlite3")).unwrap(), original);
        assert_eq!(
            fs::read(state.join("attempts/legacy-result.json")).unwrap(),
            fs::read(bundle.join("attempts/legacy-result.json")).unwrap()
        );
        let reopened = StateStore::open_with_profile(&state, profile).unwrap();
        assert_eq!(reopened.storage_profile(), profile);
        assert_eq!(
            reopened
                .task("historical-RESMGR-identity")
                .unwrap()
                .generation,
            0
        );
        assert_eq!(
            reopened.task("historical-RESMGR-identity").unwrap().status,
            cedegrid::state::TaskStatus::NeedsReconciliation
        );
    }
}

#[test]
fn future_schema_and_live_handles_fail_before_original_changes() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    let store = StateStore::open(&state).unwrap();
    let bundle = temp.path().join("bundle");
    assert!(upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).is_err());
    assert!(!bundle.exists());
    drop(store);
    mark_legacy(&state, SCHEMA_VERSION + 1);
    let before = fs::read(state.join("state.sqlite3")).unwrap();
    assert!(upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).is_err());
    assert_eq!(fs::read(state.join("state.sqlite3")).unwrap(), before);
    assert!(!bundle.exists());
}

#[test]
fn replay_upgrade_preserves_local_bytes_and_requires_authenticated_rebuild() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    let profile = StorageProfile::BurstReplayDeleteExtra;
    let store = StateStore::open_with_profile(&state, profile).unwrap();
    store.submit("old-cache-task", true).unwrap();
    drop(store);
    mark_legacy(&state, 4);
    let before = fs::read(state.join("state.sqlite3")).unwrap();
    let report = upgrade::upgrade(&state, &temp.path().join("bundle"), profile, true).unwrap();
    assert!(report.requires_authenticated_recovery);
    assert_eq!(fs::read(state.join("state.sqlite3")).unwrap(), before);
    assert!(
        StateStore::open_with_profile(&state, profile).is_err(),
        "offline upgrade must not promote replay ledger to authority"
    );
}

#[test]
fn upgrade_requires_confirmation_and_refuses_symlink_members() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    drop(StateStore::open(&state).unwrap());
    mark_legacy(&state, 2);
    assert!(
        upgrade::upgrade(
            &state,
            &temp.path().join("bundle"),
            StorageProfile::WalFull,
            false
        )
        .is_err()
    );
    symlink(temp.path().join("outside"), state.join("outbox-link")).unwrap();
    assert!(
        upgrade::upgrade(
            &state,
            &temp.path().join("bundle"),
            StorageProfile::WalFull,
            true
        )
        .is_err()
    );
    assert!(!temp.path().join("bundle").exists());
}

#[test]
fn upgrade_indexes_valid_receipts_and_preserves_unresolved_outboxes() {
    use cedegrid::protocol::{Receipt, Response, ResultSubmission};
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    let store = StateStore::open(&state).unwrap();
    store.submit("legacy-task", true).unwrap();
    let generation = store.assign("legacy-task", "legacy-attempt").unwrap();
    let output = state.join("attempts/legacy-attempt");
    fs::create_dir_all(&output).unwrap();
    let descriptor = serde_json::json!({"schema_version":1,"kind":"result","task_id":"legacy-task","assignment_id":"legacy-attempt","generation":generation,"artifacts":[],"metadata":{"historical":"RESMGR"}});
    let original = serde_json::to_vec_pretty(&descriptor).unwrap();
    fs::write(output.join("result.json"), &original).unwrap();
    let submission = ResultSubmission {
        task_id: "legacy-task".into(),
        assignment_id: "legacy-attempt".into(),
        generation,
        result: descriptor,
        artifacts: vec![],
    };
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&submission).unwrap())
    );
    let response = Response::Receipt {
        receipt: Receipt {
            task_id: submission.task_id.clone(),
            assignment_id: submission.assignment_id.clone(),
            generation,
            receipt_hash: hash,
        },
    };
    fs::write(
        output.join("result.receipt.json"),
        serde_json::to_vec(&serde_json::json!({"response":response,"submission":submission}))
            .unwrap(),
    )
    .unwrap();
    fs::write(output.join("checkpoint.json"), b"malformed retained bytes").unwrap();
    fs::write(
        output.join("checkpoint-oversized.receipt.json"),
        vec![b'x'; 3 * 1024 * 1024 + 1],
    )
    .unwrap();
    drop(store);
    mark_legacy(&state, 2);
    let bundle = temp.path().join("bundle");
    let report = upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).unwrap();
    assert_eq!(report.legacy_outboxes.len(), 4);
    assert!(
        report
            .legacy_outboxes
            .iter()
            .any(|r| r.status == "accepted_receipt" && r.receipt_hash.is_some())
    );
    assert!(
        report
            .legacy_outboxes
            .iter()
            .any(|r| r.status == "valid_finalized_descriptor")
    );
    assert_eq!(
        report
            .legacy_outboxes
            .iter()
            .filter(|r| r.status == "unresolved")
            .count(),
        2
    );
    assert_eq!(fs::read(output.join("result.json")).unwrap(), original);
    let db = Connection::open(state.join("state.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM upgrade_attempt_holds WHERE assignment_id='legacy-attempt'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn completed_upgrade_retry_does_not_hold_new_work_and_rejects_changed_bundle() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    let store = StateStore::open(&state).unwrap();
    store.submit("old", true).unwrap();
    drop(store);
    mark_legacy(&state, 2);
    let bundle = temp.path().join("bundle");
    upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).unwrap();
    let store = StateStore::open(&state).unwrap();
    store.submit("new", true).unwrap();
    drop(store);
    upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).unwrap();
    assert_eq!(
        StateStore::open(&state).unwrap().status("new").unwrap(),
        cedegrid::state::TaskStatus::Queued
    );
    fs::write(bundle.join("state.sqlite3"), b"modified bundle").unwrap();
    assert!(upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true).is_err());
}

#[test]
fn native_writer_identity_blocks_upgrade_without_touching_legacy_state() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    drop(StateStore::open(&state).unwrap());
    mark_legacy(&state, 2);
    let attempt = state.join("attempts/old");
    fs::create_dir_all(&attempt).unwrap();
    let identity = cedegrid::supervision::process_identity(std::process::id(), "old", 1).unwrap();
    fs::write(
        attempt.join("supervisor-identity.json"),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();
    let before = fs::read(state.join("state.sqlite3")).unwrap();
    let backup = temp.path().join("bundle");
    let error = upgrade::upgrade(&state, &backup, StorageProfile::WalFull, true).unwrap_err();
    assert!(format!("{error:#}").contains("supervisor"));
    assert!(!backup.exists());
    assert_eq!(before, fs::read(state.join("state.sqlite3")).unwrap());
}

#[test]
fn interrupted_after_schema_commit_resumes_recorded_bundle_without_empty_replacement() {
    use cedegrid::namespace::Namespace;
    use std::time::Duration;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let state = temp.path().join("state");
    let store = StateStore::open(&state).unwrap();
    store.submit("retained", true).unwrap();
    drop(store);
    mark_legacy(&state, 2);
    let backup = temp.path().join("bundle");
    upgrade::upgrade(&state, &backup, StorageProfile::WalFull, true).unwrap();
    let namespace = Namespace::new(&state).unwrap();
    fs::remove_file(namespace.control_dir().join("upgrade-complete.json")).unwrap();
    let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
    maintenance.record_intent("offline-upgrade").unwrap();
    drop(maintenance);
    assert!(StateStore::open(&state).is_err());
    assert!(
        upgrade::upgrade(
            &state,
            &temp.path().join("different-bundle"),
            StorageProfile::WalFull,
            true
        )
        .is_err()
    );
    let report = upgrade::upgrade(&state, &backup, StorageProfile::WalFull, true).unwrap();
    assert_eq!(report.held_tasks, 1);
    assert_eq!(
        StateStore::open(&state)
            .unwrap()
            .status("retained")
            .unwrap(),
        cedegrid::state::TaskStatus::NeedsReconciliation
    );
}
