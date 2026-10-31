#![cfg(unix)]
use cedegrid::{
    state::{SCHEMA_VERSION, StateStore, StorageProfile},
    upgrade,
};
use rusqlite::Connection;
use std::{fs, path::Path};

/// Rebuild `tasks` with the schema 5 shape, which has no `cancelled` status,
/// and mark the database with `version`.
fn downgrade_tasks(root: &Path, version: i64) {
    let db = Connection::open(root.join("state.sqlite3")).unwrap();
    db.execute_batch(&format!(
        "PRAGMA foreign_keys=OFF;
         BEGIN;
         CREATE TABLE tasks_v5 (task_id TEXT PRIMARY KEY NOT NULL, replay_safe INTEGER NOT NULL CHECK(replay_safe IN (0, 1)), status TEXT NOT NULL CHECK(status IN ('queued','assigned','completed','needs_reconciliation')), generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0), assignment_id TEXT, receipt_hash TEXT, CHECK((status = 'completed') = (receipt_hash IS NOT NULL))) STRICT;
         INSERT INTO tasks_v5 SELECT task_id,replay_safe,status,generation,assignment_id,receipt_hash FROM tasks;
         DROP TABLE tasks;
         ALTER TABLE tasks_v5 RENAME TO tasks;
         PRAGMA user_version={version};
         COMMIT;
         PRAGMA wal_checkpoint(TRUNCATE);"
    ))
    .unwrap();
    assert!(!accepts_cancelled(&db));
}

fn accepts_cancelled(db: &Connection) -> bool {
    let sql: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='tasks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    sql.contains("'cancelled'")
}

fn foreign_key_violations(db: &Connection) -> i64 {
    db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })
    .unwrap()
}

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
        let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
        downgrade_tasks(&state, previous);
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
        drop(reopened);
        // The offline upgrade runs the same task-status migration.
        let db = Connection::open(state.join("state.sqlite3")).unwrap();
        assert!(accepts_cancelled(&db));
        assert_eq!(foreign_key_violations(&db), 0);
    }
}

#[test]
fn future_schema_and_live_handles_fail_before_original_changes() {
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
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

fn coordinator_config(path: &Path) -> cedegrid::protocol::CoordinatorConfig {
    serde_json::from_value(serde_json::json!({"state_dir":path,"listen":"127.0.0.1:0","tls":{"ca_cert":"","certificate":"","private_key":""},"clients":{}})).unwrap()
}

fn distributed_schema(db: &Connection) -> i64 {
    db.query_row(
        "SELECT value FROM distributed_meta WHERE key='schema'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn assert_schema_four(db: &Connection) {
    assert_eq!(distributed_schema(db), 4);
    let columns = |table: &str| -> Vec<String> {
        let mut statement = db
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(columns("drain_requests").contains(&"cause".to_owned()));
    assert!(columns("reservations").contains(&"yield_cause".to_owned()));
    let default: String = db
        .query_row(
            "SELECT dflt_value FROM pragma_table_info('drain_requests') WHERE name='cause'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(default, "'coordinator_drain'");
    let reservations: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='reservations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(reservations.contains("CHECK(phase IN ("), "{reservations}");
    for trigger in ["status_reservations_insert", "status_reservations_delete"] {
        let present: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?1)",
                [trigger],
                |r| r.get(0),
            )
            .unwrap();
        assert!(present, "{trigger} must exist after the migration");
    }
}

#[test]
fn new_coordinator_state_starts_at_distributed_schema_four() {
    let root = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let state = root.path().join("state");
    drop(cedegrid::coordinator::Coordinator::open(coordinator_config(&state)).unwrap());
    assert_schema_four(&Connection::open(state.join("state.sqlite3")).unwrap());
}

#[test]
fn distributed_schema_three_migrates_to_four_with_check_and_recreated_triggers() {
    let root = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let state = root.path().join("state");
    drop(cedegrid::coordinator::Coordinator::open(coordinator_config(&state)).unwrap());
    {
        // Reproduce the schema 3 shape: no cause/yield_cause columns, no phase CHECK.
        let db = Connection::open(state.join("state.sqlite3")).unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=OFF;
             BEGIN;
             DROP TRIGGER IF EXISTS status_reservations_insert;
             ALTER TABLE drain_requests DROP COLUMN cause;
             CREATE TABLE reservations_v3(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),node_id TEXT NOT NULL,phase TEXT NOT NULL,resources_json TEXT NOT NULL,observed_json TEXT,lease_sequence INTEGER NOT NULL DEFAULT 0,lease_deadline_ms INTEGER NOT NULL DEFAULT 0,epoch INTEGER NOT NULL,detail TEXT NOT NULL DEFAULT '') STRICT;
             INSERT INTO reservations_v3 SELECT assignment_id,node_id,phase,resources_json,observed_json,lease_sequence,lease_deadline_ms,epoch,detail FROM reservations;
             DROP TABLE reservations;
             ALTER TABLE reservations_v3 RENAME TO reservations;
             UPDATE distributed_meta SET value=3 WHERE key='schema';
             COMMIT;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
        assert_eq!(distributed_schema(&db), 3);
    }
    drop(cedegrid::coordinator::Coordinator::open(coordinator_config(&state)).unwrap());
    let db = Connection::open(state.join("state.sqlite3")).unwrap();
    assert_schema_four(&db);
    assert!(
        db.execute(
            "INSERT INTO reservations(assignment_id,node_id,phase,resources_json,epoch) VALUES ('x','n','bogus','{}',1)",
            [],
        )
        .is_err(),
        "an unknown reservation phase must be rejected"
    );
}

/// Schema 5 state migrates in place when opened: `tasks` gains the terminal
/// `cancelled` status, and rows and references survive the rebuild.
#[test]
fn schema_five_state_migrates_in_place_to_the_cancelled_status() {
    use cedegrid::state::TaskStatus;
    for profile in [
        StorageProfile::WalFull,
        StorageProfile::DeleteExtra,
        StorageProfile::BurstReplayDeleteExtra,
    ] {
        let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
        let state = temp.path().join("state");
        let store = StateStore::open_with_profile(&state, profile).unwrap();
        store.submit("waiting", true).unwrap();
        store.submit("running", true).unwrap();
        assert_eq!(store.assign("running", "attempt-1").unwrap(), 1);
        drop(store);
        downgrade_tasks(&state, 5);
        let store = StateStore::open_with_profile(&state, profile).unwrap();
        assert_eq!(
            store.durability_settings().unwrap().schema_version,
            SCHEMA_VERSION,
            "{profile:?}"
        );
        assert_eq!(store.task("waiting").unwrap().status, TaskStatus::Queued);
        let running = store.task("running").unwrap();
        assert_eq!(running.status, TaskStatus::Assigned);
        assert_eq!(running.assignment_id.as_deref(), Some("attempt-1"));
        drop(store);
        let db = Connection::open(state.join("state.sqlite3")).unwrap();
        assert!(accepts_cancelled(&db), "{profile:?}");
        assert_eq!(foreign_key_violations(&db), 0);
        db.execute(
            "UPDATE tasks SET status='cancelled' WHERE task_id='waiting'",
            [],
        )
        .unwrap();
        drop(db);
        // Reopening neither repeats nor reverses the migration.
        let store = StateStore::open_with_profile(&state, profile).unwrap();
        assert_eq!(store.task("waiting").unwrap().status, TaskStatus::Cancelled);
    }
}

/// A coordinator's schema 5 state migrates on open with every reference to
/// `tasks` intact.
#[test]
fn schema_five_coordinator_state_migrates_with_its_references_intact() {
    let root = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let state = root.path().join("state");
    drop(cedegrid::coordinator::Coordinator::open(coordinator_config(&state)).unwrap());
    {
        let db = Connection::open(state.join("state.sqlite3")).unwrap();
        db.execute_batch(
            "INSERT INTO pools(pool_id,spec_json) VALUES ('pool','{}');
             INSERT INTO jobs(job_id,pool_id,priority,submitted_ms,spec_json) VALUES ('job','pool',0,1,'{}');
             INSERT INTO tasks(task_id,replay_safe,status) VALUES ('task',1,'queued');
             INSERT INTO task_specs(task_id,job_id,request_json) VALUES ('task','job','{}');
             INSERT INTO retry_state(task_id,failures,not_before_ms) VALUES ('task',1,2);",
        )
        .unwrap();
    }
    downgrade_tasks(&state, 5);
    drop(cedegrid::coordinator::Coordinator::open(coordinator_config(&state)).unwrap());
    let db = Connection::open(state.join("state.sqlite3")).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert!(accepts_cancelled(&db));
    assert_eq!(foreign_key_violations(&db), 0);
    let failures: i64 = db
        .query_row(
            "SELECT r.failures FROM retry_state r JOIN task_specs s USING(task_id) JOIN tasks t USING(task_id) WHERE t.status='queued'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 1);
}

/// Schema 5 needs no offline upgrade; the command refuses and changes nothing.
#[test]
fn offline_upgrade_refuses_schema_five_because_it_migrates_in_place() {
    let temp = StateDir(tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap());
    let state = temp.path().join("state");
    drop(StateStore::open(&state).unwrap());
    downgrade_tasks(&state, 5);
    let before = fs::read(state.join("state.sqlite3")).unwrap();
    let bundle = temp.path().join("bundle");
    let error = upgrade::upgrade(&state, &bundle, StorageProfile::WalFull, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("in place"), "{error}");
    assert_eq!(fs::read(state.join("state.sqlite3")).unwrap(), before);
    assert!(!bundle.exists());
}

include!("support/state_dir.rs");
