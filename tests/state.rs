use cedegrid::model::{CapabilityStatus, Decision, Resources, Snapshot};
use cedegrid::state::{
    DATABASE_FILENAME, SCHEMA_VERSION, StateStore, StorageProfile, TaskStatus,
    filesystem_supported, preflight, runtime_sqlite_supported, sqlite_version_supported,
};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn storage_dir() -> TempDir {
    // Prefer the actual project filesystem: /tmp may be volatile or a container
    // overlay, both deliberately unsupported for this durable store.
    let current = std::env::current_dir().unwrap();
    assert!(
        preflight(&current).unwrap().supported,
        "state integration tests require a verified local disk filesystem"
    );
    tempfile::Builder::new()
        .prefix(".state-test-")
        .tempdir_in(current)
        .unwrap()
}

fn observation() -> (Snapshot, Decision) {
    let snapshot = Snapshot {
        schema_version: 1,
        node_id: "portable-node".into(),
        observed_at_unix_ms: 1234,
        cpu_capacity_millicores: 4000,
        physical_cores: Some(2),
        cpu_busy_millicores: Some(500),
        total_ram_mib: 8192,
        available_ram_mib: Some(4096),
        gpu_inventory: CapabilityStatus::Available,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: None,
    };
    let decision = Decision {
        schema_version: 1,
        node_id: snapshot.node_id.clone(),
        observe_only: true,
        managed_budget: Resources::default(),
        admission_headroom: Resources::default(),
        expansion_allowed: false,
        cpu_ram_expansion_allowed: false,
        would_drain: vec![],
        reasons: vec!["recorded decision".into()],
    };
    (snapshot, decision)
}

#[test]
fn sqlite_version_gate_requires_known_upstream_fix() {
    assert!(!sqlite_version_supported(3_051_002));
    assert!(
        !sqlite_version_supported(3_050_007),
        "this build deliberately does not maintain a backport allowlist"
    );
    assert!(sqlite_version_supported(3_051_003));
    assert!(sqlite_version_supported(3_053_002));
    assert!(runtime_sqlite_supported());
}

#[test]
fn filesystem_classification_fails_closed() {
    for local in [
        "apfs", "hfs", "ufs", "ext", "xfs", "btrfs", "zfs", "f2fs", "jfs", "nilfs",
    ] {
        assert!(filesystem_supported(local, true), "{local}");
        assert!(
            !filesystem_supported(local, false),
            "non-local mount flag: {local}"
        );
    }
    for rejected in [
        "nfs",
        "nfs4",
        "smb",
        "smbfs",
        "cifs",
        "fuse",
        "fuse.sshfs",
        "ceph",
        "lustre",
        "overlay",
        "tmpfs",
        "ramfs",
        "unknown",
        "future-filesystem",
    ] {
        assert!(!filesystem_supported(rejected, true), "{rejected}");
    }
}

#[test]
fn preflight_does_not_create_missing_directories() {
    let directory = storage_dir();
    let target = directory.path().join("not-yet-created/nested");
    let report = preflight(&target).unwrap();
    assert!(report.supported);
    assert_eq!(
        report.inspected_path,
        directory.path().canonicalize().unwrap()
    );
    assert!(!target.exists());
    assert!(preflight(Path::new("")).is_err());
    let traversal = directory.path().join("missing/../elsewhere");
    assert!(preflight(&traversal).is_err());
    assert!(!directory.path().join("missing").exists());
}

#[cfg(unix)]
#[test]
fn preflight_resolves_directory_symlinks_and_rejects_database_symlinks() {
    use std::os::unix::fs::symlink;
    let directory = storage_dir();
    let real = directory.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = directory.path().join("link");
    symlink(&real, &link).unwrap();
    assert_eq!(
        preflight(&link.join("child")).unwrap().inspected_path,
        real.canonicalize().unwrap()
    );
    let outside = directory.path().join("outside.sqlite3");
    std::fs::write(&outside, b"").unwrap();
    symlink(&outside, real.join(DATABASE_FILENAME)).unwrap();
    assert!(StateStore::open(&real).is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"");
}

#[test]
fn verified_pragmas_and_atomic_observations_survive_reopen() {
    let directory = storage_dir();
    let (snapshot, decision) = observation();
    {
        let store = StateStore::open(directory.path()).unwrap();
        let pragmas = store.durability_settings().unwrap();
        assert_eq!(pragmas.journal_mode, "wal");
        assert_eq!(pragmas.synchronous, 2);
        assert!(pragmas.foreign_keys);
        assert_eq!(pragmas.schema_version, SCHEMA_VERSION);
        assert_eq!(store.append_observation(&snapshot, &decision).unwrap(), 1);
        let mut wrong_decision = decision.clone();
        wrong_decision.node_id = "other-node".into();
        assert!(
            store
                .append_observation(&snapshot, &wrong_decision)
                .is_err()
        );
    }
    let reopened = StateStore::open(directory.path()).unwrap();
    let records = reopened.observations(10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["snapshot"],
        serde_json::to_value(snapshot).unwrap()
    );
    assert_eq!(
        records[0]["decision"],
        serde_json::to_value(decision).unwrap()
    );
    assert!(reopened.observations(0).unwrap().is_empty());
}

#[test]
fn observation_schema_and_timestamp_rejections_do_not_write_partial_rows() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    let (mut snapshot, decision) = observation();
    snapshot.schema_version = 999;
    assert!(store.append_observation(&snapshot, &decision).is_err());
    snapshot.schema_version = 1;
    snapshot.observed_at_unix_ms = u64::MAX;
    assert!(store.append_observation(&snapshot, &decision).is_err());
    assert!(store.observations(10).unwrap().is_empty());
}

#[test]
fn future_schema_is_rejected_without_changing_it() {
    let directory = storage_dir();
    let path = directory.path().join(DATABASE_FILENAME);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    let before: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    let error = StateStore::open(directory.path())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("newer"));
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION + 1
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .unwrap(),
        before
    );
}

#[test]
fn history_open_is_read_only_and_does_not_initialize_missing_state() {
    let directory = storage_dir();
    let missing = directory.path().join("missing");
    assert!(StateStore::open_read_only(&missing).is_err());
    assert!(!missing.exists());
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("t", true).unwrap();
    let read_only = StateStore::open_read_only(directory.path()).unwrap();
    assert_eq!(read_only.status("t").unwrap(), TaskStatus::Queued);
    assert!(read_only.submit("forbidden", false).is_err());
    assert!(store.status("forbidden").is_err());
}

#[test]
fn retries_are_fenced_and_completed_receipts_are_idempotent_after_reopen() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("t", true).unwrap();
    store.submit("t", true).unwrap();
    assert!(store.submit("t", false).is_err());
    let first = store.assign("t", "attempt-1").unwrap();
    assert_eq!(store.assign("t", "attempt-1").unwrap(), first);
    assert!(store.assign("t", "concurrent-attempt").is_err());
    assert_eq!(
        store.mark_uncertain("t", first).unwrap(),
        TaskStatus::Queued
    );
    assert!(store.accept_result("t", first, "old-result").is_err());
    assert!(store.assign("t", "attempt-1").is_err());
    let second = store.assign("t", "attempt-2").unwrap();
    assert_eq!(second, first + 1);
    assert!(store.mark_uncertain("t", first).is_err());
    assert!(store.accept_result("t", first, "old-result").is_err());
    let receipt = store.accept_result("t", second, "final-digest").unwrap();
    drop(store);
    let reopened = StateStore::open(directory.path()).unwrap();
    assert_eq!(
        reopened.accept_result("t", second, "final-digest").unwrap(),
        receipt
    );
    assert!(
        reopened
            .accept_result("t", second, "different-digest")
            .is_err()
    );
    assert_eq!(
        reopened.mark_uncertain("t", second).unwrap(),
        TaskStatus::Completed
    );
    assert!(reopened.assign("t", "attempt-3").is_err());
}

#[test]
fn uncertain_non_replay_safe_tasks_require_reconciliation() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("external-side-effect", false).unwrap();
    let generation = store.assign("external-side-effect", "attempt").unwrap();
    assert_eq!(
        store
            .mark_uncertain("external-side-effect", generation)
            .unwrap(),
        TaskStatus::NeedsReconciliation
    );
    assert_eq!(
        store
            .mark_uncertain("external-side-effect", generation)
            .unwrap(),
        TaskStatus::NeedsReconciliation
    );
    assert!(store.assign("external-side-effect", "retry").is_err());
    assert!(
        store
            .accept_result("external-side-effect", generation, "late-result")
            .is_err()
    );
    drop(store);
    assert_eq!(
        StateStore::open(directory.path())
            .unwrap()
            .status("external-side-effect")
            .unwrap(),
        TaskStatus::NeedsReconciliation
    );
}

#[test]
fn assignment_identifiers_cannot_move_between_tasks() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("a", true).unwrap();
    store.submit("b", true).unwrap();
    store.assign("a", "shared-id").unwrap();
    assert!(store.assign("b", "shared-id").is_err());
    assert_eq!(store.task("b").unwrap().generation, 0);
    assert_eq!(store.status("b").unwrap(), TaskStatus::Queued);
}

#[test]
fn two_connections_cannot_assign_the_same_task_twice() {
    let directory = storage_dir();
    let first = StateStore::open(directory.path()).unwrap();
    first.submit("t", true).unwrap();
    let second = StateStore::open(directory.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.assign("t", &format!("attempt-{index}")).is_ok()
            })
        })
        .collect();
    let successes = handles
        .into_iter()
        .filter_map(|handle| handle.join().unwrap().then_some(()))
        .count();
    assert_eq!(successes, 1);
    assert_eq!(
        StateStore::open(directory.path())
            .unwrap()
            .task("t")
            .unwrap()
            .generation,
        1
    );
}

#[test]
fn competing_receipts_are_accepted_once_across_connections() {
    let directory = storage_dir();
    let first = StateStore::open(directory.path()).unwrap();
    first.submit("t", true).unwrap();
    let generation = first.assign("t", "attempt").unwrap();
    let second = StateStore::open(directory.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .accept_result("t", generation, &format!("digest-{index}"))
                    .ok()
            })
        })
        .collect();
    let accepted: Vec<_> = handles
        .into_iter()
        .filter_map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(accepted.len(), 1);
    let store = StateStore::open(directory.path()).unwrap();
    assert_eq!(
        store
            .accept_result("t", generation, &accepted[0].receipt_hash)
            .unwrap(),
        accepted[0]
    );
}

#[test]
fn duplicate_ack_retries_succeed_across_connections() {
    let directory = storage_dir();
    let first = StateStore::open(directory.path()).unwrap();
    first.submit("t", true).unwrap();
    let generation = first.assign("t", "attempt").unwrap();
    let second = StateStore::open(directory.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .map(|store| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.accept_result("t", generation, "same-digest").unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(receipts[0], receipts[1]);
}

#[test]
fn concurrent_observations_preserve_snapshot_decision_pairs() {
    let directory = storage_dir();
    let first = StateStore::open(directory.path()).unwrap();
    let second = StateStore::open(directory.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for ordinal in 0..20 {
                    let (mut snapshot, mut decision) = observation();
                    snapshot.node_id = format!("node-{index}-{ordinal}");
                    decision.node_id = snapshot.node_id.clone();
                    store.append_observation(&snapshot, &decision).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let rows = StateStore::open(directory.path())
        .unwrap()
        .observations(100)
        .unwrap();
    assert_eq!(rows.len(), 40);
    for row in rows {
        assert_eq!(row["snapshot"]["node_id"], row["decision"]["node_id"]);
    }
}

#[test]
fn unknown_stored_status_fails_closed() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("t", false).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    connection
        .execute(
            "UPDATE tasks SET status='unknown_future_state' WHERE task_id='t'",
            [],
        )
        .unwrap();
    assert!(
        store
            .status("t")
            .unwrap_err()
            .to_string()
            .contains("unknown task status")
    );
    assert!(store.assign("t", "attempt").is_err());
}

#[test]
fn committed_state_survives_exit_without_connection_cleanup() {
    let directory = storage_dir();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "state_exit_fixture", "--nocapture"])
        .env("CEDEGRID_TEST_EXIT_STATE_DIR", directory.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(17));
    let store = StateStore::open(directory.path()).unwrap();
    assert_eq!(store.observations(10).unwrap().len(), 1);
    assert_eq!(store.status("committed-task").unwrap(), TaskStatus::Queued);
    assert!(store.status("uncommitted-task").is_err());
}

#[test]
fn state_exit_fixture() {
    let Some(directory) = std::env::var_os("CEDEGRID_TEST_EXIT_STATE_DIR") else {
        return;
    };
    let store = StateStore::open(Path::new(&directory)).unwrap();
    let (snapshot, decision) = observation();
    store.append_observation(&snapshot, &decision).unwrap();
    store.submit("committed-task", true).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE; INSERT INTO tasks(task_id,replay_safe,status) VALUES ('uncommitted-task',1,'queued');").unwrap();
    // Process exit skips Connection::drop, leaving WAL recovery to a new process.
    std::process::exit(17);
}

#[test]
fn live_agent_and_observe_only_decisions_preserve_their_actual_mode_without_launching() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    let (snapshot, mut decision) = observation();
    store.append_observation(&snapshot, &decision).unwrap();
    decision.observe_only = false;
    store.append_observation(&snapshot, &decision).unwrap();
    drop(store);
    let store = StateStore::open_read_only(directory.path()).unwrap();
    let history = store.observations(10).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["decision"]["observe_only"], false);
    assert_eq!(history[1]["decision"]["observe_only"], true);
    assert!(store.executions().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
#[ignore = "owned subprocess fixture with an isolated permissive umask"]
fn private_state_permission_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::var_os("CEDEGRID_PERMISSION_TEST_DIRECTORY").unwrap();
    if std::env::var_os("CEDEGRID_PERMISSION_LOCK_PROBE").is_some() {
        let connection = Connection::open(Path::new(&root).join(DATABASE_FILENAME)).unwrap();
        connection
            .busy_timeout(std::time::Duration::from_millis(20))
            .unwrap();
        let error = connection.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        ));
        return;
    }
    let target = Path::new(&root).join("new/private-state");
    // SAFETY: this fixture runs only in its owned subprocess, with no other test
    // thread; changing its umask cannot affect the parent or other test processes.
    unsafe { libc::umask(0) };
    let store = StateStore::open(&target).unwrap();
    store.submit("private-environment-task", true).unwrap();
    for (path, expected) in [
        (target.parent().unwrap().to_path_buf(), 0o700),
        (target.clone(), 0o700),
        (target.join(DATABASE_FILENAME), 0o600),
        (target.join(format!("{DATABASE_FILENAME}-wal")), 0o600),
        (target.join(format!("{DATABASE_FILENAME}-shm")), 0o600),
    ] {
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            expected,
            "incorrect private permissions: {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn private_state_creation_ignores_permissive_umask_without_changing_ancestors() {
    use std::os::unix::fs::PermissionsExt;
    let directory = storage_dir();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "private_state_permission_fixture",
            "--ignored",
            "--test-threads=1",
        ])
        .env("CEDEGRID_PERMISSION_TEST_DIRECTORY", directory.path())
        .env("TMPDIR", directory.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        StateStore::open_read_only(&directory.path().join("new/private-state"))
            .unwrap()
            .status("private-environment-task")
            .unwrap(),
        TaskStatus::Queued
    );
}

#[cfg(unix)]
#[test]
fn existing_state_is_tightened_without_changing_data_and_reader_does_not_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("retained", false).unwrap();
    drop(store);
    let db = directory.path().join(DATABASE_FILENAME);
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    let reader = StateStore::open_read_only(directory.path()).unwrap();
    assert_eq!(reader.status("retained").unwrap(), TaskStatus::Queued);
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
        0o644
    );
    drop(reader);
    let writer = StateStore::open(directory.path()).unwrap();
    assert!(!writer.task("retained").unwrap().replay_safe);
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for suffix in ["", "-wal", "-shm"] {
        let path = directory
            .path()
            .join(format!("{DATABASE_FILENAME}{suffix}"));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn ambiguous_or_unrelated_state_paths_are_refused_before_permission_changes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let directory = storage_dir();
    let unrelated = directory.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("keep.txt"), b"preserve unrelated data").unwrap();
    std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(StateStore::open(&unrelated).is_err());
    assert_eq!(
        std::fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!unrelated.join(DATABASE_FILENAME).exists());
    assert_eq!(
        std::fs::read_dir(&unrelated).unwrap().count(),
        1,
        "even initialization metadata must not be added to unrelated directories"
    );

    let linked = directory.path().join("linked-state");
    symlink(&unrelated, &linked).unwrap();
    assert!(StateStore::open(&linked).is_err());
    assert!(StateStore::open(&linked.join("")).is_err());
    assert_eq!(
        std::fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777,
        0o755
    );

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let candidate = directory.path().join(format!("hardlink{suffix}"));
        std::fs::create_dir(&candidate).unwrap();
        let store = StateStore::open(&candidate).unwrap();
        store.submit("retained", true).unwrap();
        drop(store);
        let external = directory.path().join(format!("external{suffix}.data"));
        if suffix.is_empty() {
            std::fs::rename(candidate.join(DATABASE_FILENAME), &external).unwrap();
        } else {
            std::fs::write(&external, b"do not modify hardlink target").unwrap();
        }
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(
            &external,
            candidate.join(format!("{DATABASE_FILENAME}{suffix}")),
        )
        .unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = std::fs::read(&external).unwrap();
        assert!(StateStore::open(&candidate).is_err());
        assert_eq!(std::fs::read(&external).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&external).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(&candidate).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let before = std::fs::metadata(&home).unwrap().mode();
    assert!(StateStore::open(&home).is_err());
    assert_eq!(std::fs::metadata(&home).unwrap().mode(), before);
    // Runtime-owned system directory: metadata inspection only; it must be
    // rejected before any database creation or chmod, including for same-host use.
    let system = Path::new("/usr");
    let before = std::fs::metadata(system).unwrap();
    if before.uid() != unsafe { libc::geteuid() } {
        assert!(StateStore::open(system).is_err());
        let after = std::fs::metadata(system).unwrap();
        assert_eq!(after.mode(), before.mode());
        assert_eq!(after.mtime(), before.mtime());
    }
}

#[cfg(unix)]
#[test]
fn privacy_preparation_does_not_cancel_another_live_sqlite_connections_os_locks() {
    use std::os::unix::fs::PermissionsExt;
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("lock-holder", true).unwrap();
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute_batch(
            "BEGIN IMMEDIATE; UPDATE tasks SET replay_safe=0 WHERE task_id='lock-holder'",
        )
        .unwrap();
    for suffix in ["", "-wal", "-shm"] {
        std::fs::set_permissions(
            directory
                .path()
                .join(format!("{DATABASE_FILENAME}{suffix}")),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
    let reopened =
        StateStore::open_with_busy_timeout(directory.path(), std::time::Duration::from_millis(20))
            .unwrap();
    assert!(reopened.task("lock-holder").unwrap().replay_safe);
    assert!(reopened.submit("blocked-by-live-writer", true).is_err());
    for suffix in ["", "-wal", "-shm"] {
        assert_eq!(
            std::fs::metadata(
                directory
                    .path()
                    .join(format!("{DATABASE_FILENAME}{suffix}"))
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }
    // A separate process observes actual kernel locking, not SQLite's same-process
    // bookkeeping. Permission repair must not release the existing WAL write lock.
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "private_state_permission_fixture",
            "--ignored",
            "--test-threads=1",
        ])
        .env("CEDEGRID_PERMISSION_TEST_DIRECTORY", directory.path())
        .env("CEDEGRID_PERMISSION_LOCK_PROBE", "1")
        .env("TMPDIR", directory.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    connection.execute_batch("ROLLBACK").unwrap();
    assert!(
        StateStore::open(directory.path())
            .unwrap()
            .task("lock-holder")
            .unwrap()
            .replay_safe
    );
}

#[test]
fn rollback_profile_persists_effective_settings_and_receipts_without_wal() {
    let directory = storage_dir();
    let profile = StorageProfile::DeleteExtra;
    let store = StateStore::open_with_profile_and_busy_timeout(
        directory.path(),
        profile,
        std::time::Duration::from_millis(321),
    )
    .unwrap();
    let settings = store.durability_settings().unwrap();
    assert_eq!(settings.profile, profile);
    assert_eq!(settings.journal_mode, "delete");
    assert_eq!(settings.synchronous, 3);
    assert_eq!(
        settings.schema_version,
        cedegrid::state::DELETE_EXTRA_SCHEMA_VERSION
    );
    assert!(
        settings.schema_version > 4,
        "previous binaries must refuse before changing journal mode"
    );
    assert_eq!(settings.busy_timeout_ms, 321);
    store.submit("durable-result", true).unwrap();
    let generation = store.assign("durable-result", "attempt-one").unwrap();
    let first = store
        .accept_result("durable-result", generation, "sha256:result")
        .unwrap();
    assert_eq!(
        first,
        store
            .accept_result("durable-result", generation, "sha256:result")
            .unwrap()
    );
    assert!(
        store
            .accept_result("durable-result", generation, "other-result")
            .is_err()
    );
    drop(store);
    assert!(
        !directory
            .path()
            .join(format!("{DATABASE_FILENAME}-wal"))
            .exists()
    );
    assert!(
        !directory
            .path()
            .join(format!("{DATABASE_FILENAME}-shm"))
            .exists()
    );
    assert!(
        !directory
            .path()
            .join(format!("{DATABASE_FILENAME}-journal"))
            .exists()
    );
    let reopened = StateStore::open_with_profile(directory.path(), profile).unwrap();
    assert_eq!(
        reopened.status("durable-result").unwrap(),
        TaskStatus::Completed
    );
    assert_eq!(
        reopened
            .task("durable-result")
            .unwrap()
            .receipt_hash
            .as_deref(),
        Some("sha256:result")
    );
    let reader = StateStore::open_read_only(directory.path()).unwrap();
    assert_eq!(reader.storage_profile(), profile);
    assert_eq!(reader.durability_settings().unwrap().synchronous, 3);
    assert!(reader.submit("forbidden-read-only", true).is_err());
}

#[test]
fn profile_mismatch_is_rejected_live_and_offline_without_changing_database() {
    for profile in [StorageProfile::WalFull, StorageProfile::DeleteExtra] {
        let directory = storage_dir();
        let other = if profile == StorageProfile::WalFull {
            StorageProfile::DeleteExtra
        } else {
            StorageProfile::WalFull
        };
        let store = StateStore::open_with_profile(directory.path(), profile).unwrap();
        store.submit("original", false).unwrap();
        let refusal = StateStore::open_with_profile(directory.path(), other)
            .err()
            .unwrap()
            .to_string();
        assert!(refusal.contains("profile mismatch") || refusal.contains("newer"));
        assert!(StateStore::open_read_only_with_profile(directory.path(), other).is_err());
        assert_eq!(
            store.durability_settings().unwrap().journal_mode,
            profile.journal_mode()
        );
        store.submit("still-writable", false).unwrap();
        drop(store);
        let before = std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap();
        assert!(StateStore::open_with_profile(directory.path(), other).is_err());
        assert_eq!(
            std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap(),
            before
        );
        let reopened = StateStore::open_with_profile(directory.path(), profile).unwrap();
        assert_eq!(
            reopened.status("still-writable").unwrap(),
            TaskStatus::Queued
        );
    }
}

#[test]
fn legacy_wal_startup_requires_explicit_upgrade_without_profile_adoption() {
    let directory = storage_dir();
    let store = StateStore::open(directory.path()).unwrap();
    store.submit("legacy", true).unwrap();
    drop(store);
    let db = Connection::open(directory.path().join(DATABASE_FILENAME)).unwrap();
    db.execute_batch("DROP TABLE state_storage_profile; DROP TABLE execution_events; DROP TABLE executions; PRAGMA user_version=1; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    drop(db);
    let before = std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap();
    assert!(StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).is_err());
    assert!(
        StateStore::open(directory.path())
            .err()
            .unwrap()
            .to_string()
            .contains("upgrade required")
    );
    assert_eq!(
        std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap(),
        before
    );
    let db = Connection::open(directory.path().join(DATABASE_FILENAME)).unwrap();
    assert_eq!(
        db.query_row("SELECT status FROM tasks WHERE task_id='legacy'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "queued"
    );
}

#[test]
fn rollback_profile_retains_busy_bound_and_does_not_release_other_connection_lock() {
    let directory = storage_dir();
    let store =
        StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).unwrap();
    store.submit("committed", true).unwrap();
    let writer = Connection::open(store.database_path()).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE tasks SET replay_safe=0 WHERE task_id='committed';")
        .unwrap();
    let started = std::time::Instant::now();
    let attempt = StateStore::open_with_profile_and_busy_timeout(
        directory.path(),
        StorageProfile::DeleteExtra,
        std::time::Duration::from_millis(20),
    );
    let reopened = attempt.unwrap();
    let settings = reopened.durability_settings().unwrap();
    assert_eq!(settings.busy_timeout_ms, 20);
    assert_eq!(settings.journal_mode, "delete");
    assert_eq!(settings.synchronous, 3);
    assert!(reopened.task("committed").unwrap().replay_safe);
    // Opening current state needs no writer reservation. Real writes still
    // honor the same busy bound, without discarding the first writer's lock.
    assert!(reopened.submit("blocked-by-writer", true).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let rival = Connection::open(store.database_path()).unwrap();
    rival.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert!(rival.execute_batch("BEGIN IMMEDIATE").is_err());
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(store.task("committed").unwrap().replay_safe);
    reopened.submit("after-release", true).unwrap();
}

#[test]
fn wrong_profile_metadata_and_unmarked_rollback_database_fail_closed() {
    let directory = storage_dir();
    let store =
        StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).unwrap();
    store.submit("existing", true).unwrap();
    drop(store);
    let db = Connection::open(directory.path().join(DATABASE_FILENAME)).unwrap();
    db.execute("UPDATE state_storage_profile SET profile='wal_full'", [])
        .unwrap();
    drop(db);
    assert!(StateStore::open(directory.path()).is_err());
    assert!(StateStore::open_read_only(directory.path()).is_err());
    let db = Connection::open(directory.path().join(DATABASE_FILENAME)).unwrap();
    db.execute_batch("DROP TABLE state_storage_profile")
        .unwrap();
    drop(db);
    assert!(StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra).is_err());
    assert!(StateStore::open(directory.path()).is_err());
}

#[test]
fn first_open_profile_selection_is_serialized_across_threads() {
    let directory = storage_dir();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [StorageProfile::WalFull, StorageProfile::DeleteExtra]
        .into_iter()
        .map(|profile| {
            let path = directory.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                StateStore::open_with_profile(&path, profile).map(|s| s.storage_profile())
            })
        })
        .collect();
    let successes: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap().ok())
        .collect();
    assert_eq!(successes.len(), 1);
    assert_eq!(
        StateStore::open_read_only(directory.path())
            .unwrap()
            .storage_profile(),
        successes[0]
    );
}

#[test]
#[ignore = "child process fixture, invoked by rollback profile crash/concurrency tests"]
fn storage_profile_process_fixture() {
    let directory =
        std::path::PathBuf::from(std::env::var_os("CEDEGRID_PROFILE_DIRECTORY").unwrap());
    let mode = std::env::var("CEDEGRID_PROFILE_FIXTURE").unwrap();
    let profile: StorageProfile =
        serde_json::from_str(&std::env::var("CEDEGRID_PROFILE").unwrap()).unwrap();
    let store = StateStore::open_with_profile(&directory, profile).unwrap();
    if let Some(versions) = mode.strip_prefix("hot-rollback-") {
        store.submit("hot-journal-acknowledged", true).unwrap();
        let (original, pending) = versions.split_once('-').unwrap();
        let original: i64 = original.parse().unwrap();
        let pending: i64 = pending.parse().unwrap();
        let db = Connection::open(store.database_path()).unwrap();
        db.execute_batch("CREATE TABLE hot_journal_spill(value BLOB); WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<256) INSERT INTO hot_journal_spill SELECT zeroblob(4000) FROM n;").unwrap();
        db.pragma_update(None, "user_version", original).unwrap();
        db.pragma_update(None, "synchronous", "EXTRA").unwrap();
        db.execute_batch("PRAGMA cache_size=5; PRAGMA cache_spill=ON; BEGIN IMMEDIATE;")
            .unwrap();
        db.pragma_update(None, "user_version", pending).unwrap();
        // Spill dirty pages before the crash; a small unspilled transaction
        // leaves a cold journal and would not exercise READONLY_ROLLBACK.
        db.execute_batch("UPDATE hot_journal_spill SET value=randomblob(4000);")
            .unwrap();
        // Force the exact hot-journal crash point independently of SQLite's
        // cache-spill heuristics and allocator/cache size on this platform.
        assert_eq!(
            unsafe { rusqlite::ffi::sqlite3_db_cacheflush(db.handle()) },
            rusqlite::ffi::SQLITE_OK
        );
        // cacheflush intentionally retains page one. Reproduce the later
        // commit crash point where that page is written but the synced journal
        // still contains its previous schema, using the same live VFS handle.
        let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_file_control(
                    db.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
                    std::ptr::from_mut(&mut file).cast()
                ),
                rusqlite::ffi::SQLITE_OK
            );
            assert!(!file.is_null());
            let methods = &*(*file).pMethods;
            let version = (pending as i32).to_be_bytes();
            assert_eq!(
                methods.xWrite.unwrap()(file, version.as_ptr().cast(), 4, 60),
                rusqlite::ffi::SQLITE_OK
            );
            assert_eq!(
                methods.xSync.unwrap()(file, rusqlite::ffi::SQLITE_SYNC_FULL),
                rusqlite::ffi::SQLITE_OK
            );
        }
        std::fs::write(directory.join("crash-ready"), b"hot rollback journal").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
        panic!("parent did not interrupt bounded hot-journal fixture");
    }
    if mode == "crash-before-commit" {
        store.submit("acknowledged", true).unwrap();
        let generation = store.assign("acknowledged", "durable-attempt").unwrap();
        store
            .accept_result("acknowledged", generation, "durable-digest")
            .unwrap();
        store.submit("unsafe-side-effect", false).unwrap();
        let generation = store
            .assign("unsafe-side-effect", "unsafe-attempt")
            .unwrap();
        store
            .mark_uncertain("unsafe-side-effect", generation)
            .unwrap();
        let db = Connection::open(store.database_path()).unwrap();
        db.pragma_update(None, "synchronous", profile.synchronous())
            .unwrap();
        db.execute_batch("BEGIN IMMEDIATE; INSERT INTO tasks(task_id,replay_safe,status) VALUES ('uncommitted',1,'queued');").unwrap();
        std::fs::write(
            directory.join("crash-ready"),
            b"committed receipts before uncommitted transaction",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
        panic!("parent did not interrupt bounded fixture");
    }
    let worker = mode.strip_prefix("writer-").unwrap();
    for index in 0..25 {
        let task = format!("{worker}-{index}");
        store.submit(&task, true).unwrap();
        let generation = store.assign(&task, &format!("attempt-{task}")).unwrap();
        store
            .accept_result(&task, generation, &format!("digest-{task}"))
            .unwrap();
    }
}

struct ProfileChild(std::process::Child, tempfile::NamedTempFile);
impl Drop for ProfileChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn profile_child(directory: &Path, profile: StorageProfile, mode: &str) -> ProfileChild {
    // Fresh state must remain empty until the child initializes it. Keep this
    // owned diagnostic file beside the state directory, not inside it.
    let log = tempfile::Builder::new()
        .prefix(&format!(".profile-fixture-{mode}-"))
        .tempfile_in(directory.parent().unwrap())
        .unwrap();
    ProfileChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage_profile_process_fixture",
                "--ignored",
                "--test-threads=1",
            ])
            .env("CEDEGRID_PROFILE_DIRECTORY", directory)
            .env("CEDEGRID_PROFILE", serde_json::to_string(&profile).unwrap())
            .env("CEDEGRID_PROFILE_FIXTURE", mode)
            .env("TMPDIR", directory)
            .stdout(std::process::Stdio::from(
                log.as_file().try_clone().unwrap(),
            ))
            .stderr(std::process::Stdio::from(
                log.as_file().try_clone().unwrap(),
            ))
            .spawn()
            .unwrap(),
        log,
    )
}

#[test]
fn both_profiles_preserve_acknowledged_receipts_and_uncertainty_after_process_loss() {
    for profile in [StorageProfile::WalFull, StorageProfile::DeleteExtra] {
        let directory = storage_dir();
        let mut child = profile_child(directory.path(), profile, "crash-before-commit");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !directory.path().join("crash-ready").exists() {
            assert!(
                std::time::Instant::now() < until,
                "fixture readiness deadline"
            );
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "fixture exited before crash point; diagnostics:\n{}",
                std::fs::read_to_string(child.1.path()).unwrap_or_else(|error| error.to_string())
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let store = StateStore::open_with_profile(directory.path(), profile).unwrap();
        assert_eq!(store.status("acknowledged").unwrap(), TaskStatus::Completed);
        assert_eq!(
            store.task("acknowledged").unwrap().receipt_hash.as_deref(),
            Some("durable-digest")
        );
        assert!(
            store
                .accept_result("acknowledged", 1, "durable-digest")
                .is_ok()
        );
        assert!(store.accept_result("acknowledged", 1, "different").is_err());
        assert!(store.task("uncommitted").is_err());
        assert_eq!(
            store.status("unsafe-side-effect").unwrap(),
            TaskStatus::NeedsReconciliation
        );
        assert!(store.assign("unsafe-side-effect", "unsafe-retry").is_err());
        let db = Connection::open(store.database_path()).unwrap();
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}

#[test]
fn hot_rollback_recovery_checks_current_and_rolled_back_schema_before_mutation() {
    for (original, pending, allowed) in [(5, 5, true), (3, 5, false), (6, 5, false), (5, 6, false)]
    {
        let directory = storage_dir();
        let mode = format!("hot-rollback-{original}-{pending}");
        let mut child = profile_child(directory.path(), StorageProfile::DeleteExtra, &mode);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !directory.path().join("crash-ready").exists() {
            assert!(
                std::time::Instant::now() < until,
                "hot-journal fixture readiness deadline"
            );
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "hot-journal fixture exited early; diagnostics:\n{}",
                std::fs::read_to_string(child.1.path()).unwrap_or_else(|error| error.to_string())
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let database = directory.path().join(DATABASE_FILENAME);
        let journal = directory
            .path()
            .join(format!("{DATABASE_FILENAME}-journal"));
        let before_database = std::fs::read(&database).unwrap();
        let before_journal = std::fs::read(&journal).unwrap();
        assert_eq!(
            &before_journal[..8],
            &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7],
            "expected a synced hot journal for {mode}; length {}",
            before_journal.len()
        );
        assert_eq!(
            i32::from_be_bytes(before_database[60..64].try_into().unwrap()),
            pending
        );
        let probe =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let error = probe
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap_err();
        assert_eq!(
            error.sqlite_error().unwrap().extended_code,
            rusqlite::ffi::SQLITE_READONLY_ROLLBACK
        );
        drop(probe);
        let result = StateStore::open_with_profile(directory.path(), StorageProfile::DeleteExtra);
        if allowed {
            let recovered = result.unwrap();
            assert_eq!(
                recovered.task("hot-journal-acknowledged").unwrap().status,
                TaskStatus::Queued
            );
            assert_eq!(
                recovered.durability_settings().unwrap().schema_version,
                SCHEMA_VERSION
            );
            let db = Connection::open(recovered.database_path()).unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM hot_journal_spill WHERE value=zeroblob(4000)",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                256
            );
            assert_eq!(
                db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            assert!(!journal.exists());
        } else {
            let error = result
                .err()
                .expect("unsupported hot-journal schema must be refused")
                .to_string();
            assert!(
                error.contains("state upgrade required") || error.contains("newer than supported"),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&database).unwrap(),
                before_database,
                "refusal changed the original database"
            );
            assert_eq!(
                std::fs::read(&journal).unwrap(),
                before_journal,
                "refusal changed the original journal"
            );
        }
    }
}

#[test]
fn rollback_profile_serializes_real_process_writers_without_losing_receipts() {
    let directory = storage_dir();
    let profile = StorageProfile::DeleteExtra;
    let store = StateStore::open_with_profile(directory.path(), profile).unwrap();
    let mut children = [
        profile_child(directory.path(), profile, "writer-a"),
        profile_child(directory.path(), profile, "writer-b"),
    ];
    let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
    for child in &mut children {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "writer failed: {status}; diagnostics:\n{}",
                    std::fs::read_to_string(child.1.path())
                        .unwrap_or_else(|error| error.to_string())
                );
                break;
            }
            assert!(
                std::time::Instant::now() < until,
                "concurrent writer deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    for worker in ["a", "b"] {
        for index in 0..25 {
            let task = store.task(&format!("{worker}-{index}")).unwrap();
            assert_eq!(task.status, TaskStatus::Completed);
            assert_eq!(task.generation, 1);
            assert_eq!(task.receipt_hash, Some(format!("digest-{worker}-{index}")));
        }
    }
}

#[test]
fn rollback_schema_fence_corruption_is_refused_without_repair() {
    let directory = storage_dir();
    let profile = StorageProfile::DeleteExtra;
    let store = StateStore::open_with_profile(directory.path(), profile).unwrap();
    store.submit("must-survive", false).unwrap();
    drop(store);
    // Simulate only this isolated test database's malformed persisted fence.
    let path = directory.path().join(DATABASE_FILENAME);
    let database = Connection::open(&path).unwrap();
    database.pragma_update(None, "user_version", 4).unwrap();
    drop(database);
    let before = std::fs::read(&path).unwrap();
    let error = StateStore::open_with_profile(directory.path(), profile)
        .err()
        .unwrap();
    assert!(error.to_string().contains("upgrade required"));
    assert!(StateStore::open_read_only_with_profile(directory.path(), profile).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let database = Connection::open(&path).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT status FROM tasks WHERE task_id='must-survive'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "queued"
    );
}

#[cfg(unix)]
#[test]
fn interrupted_lock_only_initialization_resumes_without_accepting_other_files() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    let directory = storage_dir();
    let target = directory.path().join("lock-only");
    std::fs::create_dir(&target).unwrap();
    let lock = target.join(".storage-profile.lock");
    drop(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lock)
            .unwrap(),
    );
    let store = StateStore::open_with_profile(&target, StorageProfile::DeleteExtra).unwrap();
    store.submit("resumed", true).unwrap();
    assert_eq!(store.status("resumed").unwrap(), TaskStatus::Queued);
    drop(store);
    let unrelated = directory.path().join("lock-and-unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    drop(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(unrelated.join(".storage-profile.lock"))
            .unwrap(),
    );
    std::fs::write(unrelated.join("keep.txt"), b"preserve").unwrap();
    std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(StateStore::open(&unrelated).is_err());
    assert_eq!(
        std::fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read(unrelated.join("keep.txt")).unwrap(),
        b"preserve"
    );
    assert!(!unrelated.join(DATABASE_FILENAME).exists());
    let linked = directory.path().join("linked-lock");
    std::fs::create_dir(&linked).unwrap();
    symlink(&lock, linked.join(".storage-profile.lock")).unwrap();
    assert!(StateStore::open(&linked).is_err());
    assert!(!linked.join(DATABASE_FILENAME).exists());
}

#[test]
fn replayable_profile_is_explicit_persisted_and_fenced_from_strict_openers() {
    let profile: StorageProfile = serde_json::from_str("\"burst_replay_delete_extra\"").unwrap();
    let directory = storage_dir();
    let state = directory.path().join("replayable/nested");
    let store = StateStore::open_with_profile(&state, profile).unwrap();
    let settings = serde_json::to_value(store.durability_settings().unwrap()).unwrap();
    assert_eq!(settings["assurance"], "replayable_local");
    assert_eq!(settings["journal_mode"], "delete");
    assert_eq!(settings["synchronous"], 3);
    assert_eq!(settings["schema_version"], 5);
    store.submit("replayable", true).unwrap();
    let generation = store.assign("replayable", "attempt-one").unwrap();
    assert!(
        store
            .accept_result("replayable", generation, "sha256:result")
            .is_err()
    );
    assert!(store.submit("unsafe", false).is_err());
    assert_eq!(store.status("replayable").unwrap(), TaskStatus::Assigned);
    assert!(StateStore::open(&state).is_err());
    assert!(StateStore::open_with_profile(&state, StorageProfile::DeleteExtra).is_err());
    assert!(StateStore::open_read_only(&state).is_err());
    assert!(StateStore::open_read_only_with_profile(&state, StorageProfile::DeleteExtra).is_err());
    drop(store);
    let reopened = StateStore::open_with_profile(&state, profile).unwrap();
    assert_eq!(reopened.task("replayable").unwrap().generation, generation);
    drop(reopened);
    assert_eq!(
        StateStore::open_read_only_with_profile(&state, profile)
            .unwrap()
            .storage_profile(),
        profile
    );
    let db = Connection::open(state.join(DATABASE_FILENAME)).unwrap();
    let persisted: (String, String) = db.query_row("SELECT profile,assurance FROM state_storage_profile JOIN state_storage_assurance USING(singleton)", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(
        persisted,
        (
            "burst_replay_delete_extra".into(),
            "replayable_local".into()
        )
    );
}

#[test]
fn replayable_profile_cannot_adopt_existing_strong_state_or_repair_invalid_assurance() {
    let replay = StorageProfile::BurstReplayDeleteExtra;
    for strong in [StorageProfile::WalFull, StorageProfile::DeleteExtra] {
        let directory = storage_dir();
        drop(StateStore::open_with_profile(directory.path(), strong).unwrap());
        let before = std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap();
        assert!(StateStore::open_with_profile(directory.path(), replay).is_err());
        assert_eq!(
            std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap(),
            before
        );
    }
    for damage in [
        "DROP TABLE state_storage_assurance",
        "DELETE FROM state_storage_assurance",
        "PRAGMA user_version=3",
    ] {
        let directory = storage_dir();
        drop(StateStore::open_with_profile(directory.path(), replay).unwrap());
        let db = Connection::open(directory.path().join(DATABASE_FILENAME)).unwrap();
        db.execute_batch(damage).unwrap();
        drop(db);
        let before = std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap();
        assert!(
            StateStore::open_with_profile(directory.path(), replay).is_err(),
            "{damage}"
        );
        assert!(
            StateStore::open_read_only_with_profile(directory.path(), replay).is_err(),
            "{damage}"
        );
        assert_eq!(
            std::fs::read(directory.path().join(DATABASE_FILENAME)).unwrap(),
            before
        );
    }
}

#[test]
fn replayable_filesystem_admission_is_explicit_and_does_not_change_strict_preflight() {
    use cedegrid::state::StoragePreflight;
    let mut capability = StoragePreflight {
        requested_path: "/example/state".into(),
        inspected_path: "/example".into(),
        filesystem: "fuse".into(),
        supported: false,
        detail: "namespace barrier unavailable".into(),
    };
    assert!(!capability.admitted_by(StorageProfile::WalFull));
    assert!(!capability.admitted_by(StorageProfile::DeleteExtra));
    assert!(capability.admitted_by(StorageProfile::BurstReplayDeleteExtra));
    assert!(!capability.supported);
    for rejected in [
        "unknown",
        "nfs",
        "smb",
        "overlay",
        "tmpfs",
        "ramfs",
        "fuse.sshfs",
        "future-filesystem",
    ] {
        capability.filesystem = rejected.into();
        assert!(
            !capability.admitted_by(StorageProfile::BurstReplayDeleteExtra),
            "{rejected}"
        );
    }
}

fn existing_entries(directory: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn existing_only_open_never_creates_missing_directory_or_database() {
    let directory = storage_dir();
    let profile = StorageProfile::BurstReplayDeleteExtra;
    let missing = directory.path().join("absent/nested");
    let before = existing_entries(directory.path());
    assert!(StateStore::open_existing_with_profile(&missing, profile).is_err());
    assert_eq!(existing_entries(directory.path()), before);
    assert!(!directory.path().join("absent").exists());
    assert!(StateStore::open_existing_with_profile(directory.path(), profile).is_err());
    assert_eq!(existing_entries(directory.path()), before);
    assert!(!directory.path().join(DATABASE_FILENAME).exists());
    assert!(!directory.path().join(".storage-profile.lock").exists());
}

#[cfg(unix)]
#[test]
fn existing_only_open_rejects_empty_and_corrupt_files_without_initialization() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for bytes in [b"".as_slice(), b"corrupt-private-test-database".as_slice()] {
        let directory = storage_dir();
        let path = directory.path().join(DATABASE_FILENAME);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let entries = existing_entries(directory.path());
        assert!(
            StateStore::open_existing_with_profile(
                directory.path(),
                StorageProfile::BurstReplayDeleteExtra
            )
            .is_err()
        );
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.len(), before.mode()),
            (after.dev(), after.ino(), after.len(), after.mode())
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(existing_entries(directory.path()), entries);
    }
}

#[test]
fn existing_only_open_preserves_profile_and_schema_without_adoption_or_migration() {
    for case in 0..4 {
        let directory = storage_dir();
        let profile = StorageProfile::BurstReplayDeleteExtra;
        drop(StateStore::open_with_profile(directory.path(), profile).unwrap());
        let path = directory.path().join(DATABASE_FILENAME);
        {
            let db = Connection::open(&path).unwrap();
            match case {
                0 => db
                    .execute_batch("DROP TABLE state_storage_profile")
                    .unwrap(),
                1 => db
                    .pragma_update(None, "user_version", profile.schema_version() + 1)
                    .unwrap(),
                2 => db
                    .pragma_update(None, "user_version", profile.schema_version() - 1)
                    .unwrap(),
                3 => {}
                _ => unreachable!(),
            }
        }
        let expected = if case == 3 {
            StorageProfile::DeleteExtra
        } else {
            profile
        };
        let before = std::fs::read(&path).unwrap();
        let entries = existing_entries(directory.path());
        assert!(
            StateStore::open_existing_with_profile(directory.path(), expected).is_err(),
            "case {case}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "case {case}");
        assert_eq!(existing_entries(directory.path()), entries, "case {case}");
    }
}

#[cfg(unix)]
#[test]
fn existing_only_open_rejects_links_and_permissions_without_repairing_them() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let directory = storage_dir();
    let state = directory.path().join("state");
    let profile = StorageProfile::BurstReplayDeleteExtra;
    drop(StateStore::open_with_profile(&state, profile).unwrap());
    let path = state.join(DATABASE_FILENAME);
    let before = std::fs::metadata(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(StateStore::open_existing_with_profile(&state, profile).is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0o644);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(StateStore::open_existing_with_profile(&state, profile).is_err());
    assert_eq!(std::fs::metadata(&state).unwrap().mode() & 0o7777, 0o755);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = directory.path().join("db-hardlink");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(StateStore::open_existing_with_profile(&state, profile).is_err());
    std::fs::remove_file(alias).unwrap();
    let state_alias = directory.path().join("state-link");
    symlink(&state, &state_alias).unwrap();
    assert!(StateStore::open_existing_with_profile(&state_alias, profile).is_err());
    let journal = state.join(format!("{DATABASE_FILENAME}-journal"));
    symlink(&path, &journal).unwrap();
    assert!(StateStore::open_existing_with_profile(&state, profile).is_err());
    std::fs::remove_file(journal).unwrap();
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    StateStore::open_existing_with_profile(&state, profile).unwrap();
}

#[test]
fn existing_only_open_preserves_normal_weak_state_and_effective_connection_settings() {
    let directory = storage_dir();
    let profile = StorageProfile::BurstReplayDeleteExtra;
    let store = StateStore::open_with_profile(directory.path(), profile).unwrap();
    store.submit("retained", true).unwrap();
    drop(store);
    let entries = existing_entries(directory.path());
    let store = StateStore::open_existing_with_profile(directory.path(), profile).unwrap();
    assert_eq!(store.status("retained").unwrap(), TaskStatus::Queued);
    let settings = store.durability_settings().unwrap();
    assert_eq!(settings.profile, profile);
    assert_eq!(settings.schema_version, profile.schema_version());
    assert_eq!(settings.journal_mode, "delete");
    assert_eq!(settings.synchronous, 3);
    assert!(settings.foreign_keys);
    assert_eq!(settings.busy_timeout_ms, 5000);
    store.submit("next", true).unwrap();
    assert!(store.submit("unsafe", false).is_err());
    drop(store);
    assert_eq!(existing_entries(directory.path()), entries);
    assert_eq!(
        StateStore::open_existing_with_profile(directory.path(), profile)
            .unwrap()
            .status("next")
            .unwrap(),
        TaskStatus::Queued
    );
}
