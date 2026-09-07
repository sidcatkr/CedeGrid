use resource_manager::{
    backup::{self, Limits},
    coordinator::Coordinator,
    protocol::*,
    state::{StateStore, StorageProfile},
};
use serde_json::json;
use std::{fs, path::Path, time::Duration};
fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".backup-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}
fn config(path: &Path) -> CoordinatorConfig {
    serde_json::from_value(json!({"state_dir":path,"listen":"127.0.0.1:0","tls":{"ca_cert":"","certificate":"","private_key":""},"clients":{}})).unwrap()
}
fn limits() -> Limits {
    Limits {
        max_bytes: 10 * 1024 * 1024,
        max_files: 100,
        timeout: Duration::from_secs(10),
    }
}
fn seed(path: &Path) -> (String, String) {
    seed_with_profile(path, StorageProfile::WalFull)
}
fn seed_with_profile(path: &Path, profile: StorageProfile) -> (String, String) {
    use sha2::{Digest, Sha256};
    let mut configuration = config(path);
    configuration.storage_profile = profile;
    let mut c = Coordinator::open(configuration).unwrap();
    let p = Principal::Operator;
    let pool:PoolSpec=serde_json::from_value(json!({"pool_id":"p","class":"opportunistic","node_ids":["node"],"min_workers":0,"max_workers":1})).unwrap();
    c.handle_at(&p, Request::PutPool { pool }, 100).unwrap();
    let job:JobSpec=serde_json::from_value(json!({"job_id":"job","pool_id":"p","tasks":[{"task_id":"task","assignment_id":"","argv":["/bin/true"],"cwd":path,"resources":{"cpu_millicores":1000,"ram_mib":100,"gpu_memory_mib":{}},"replay_safe":true,"class":"opportunistic","single_process":true,"no_escape":true}]})).unwrap();
    c.handle_at(&p, Request::Submit { job }, 100).unwrap();
    let report:NodeReport=serde_json::from_value(json!({"node_id":"node","boot_id":"boot","observed_at_unix_ms":100,"managed_budget":{"cpu_millicores":1000,"ram_mib":100,"gpu_memory_mib":{}},"expansion_allowed":true,"launch_slots":1,"available_controls":[],"allocations":[]})).unwrap();
    let Response::Heartbeat { reply } = c
        .handle_at(
            &Principal::Node {
                node_id: "node".into(),
            },
            Request::Heartbeat { report },
            100,
        )
        .unwrap()
    else {
        panic!()
    };
    let assignment = reply.assignments[0].request.assignment_id.clone();
    let data = b"durable checkpoint";
    let hash = hex::encode(Sha256::digest(data));
    let Response::Upload { upload_id, .. } = c
        .handle_at(
            &p,
            Request::BeginUpload {
                assignment_id: assignment.clone(),
                generation: 1,
                artifact: ArtifactRef {
                    sha256: hash.clone(),
                    size: data.len() as u64,
                },
            },
            100,
        )
        .unwrap()
    else {
        panic!()
    };
    c.handle_at(
        &p,
        Request::UploadChunk {
            upload_id: upload_id.clone(),
            offset: 0,
            data_hex: hex::encode(data),
        },
        100,
    )
    .unwrap();
    c.handle_at(&p, Request::CommitUpload { upload_id }, 100)
        .unwrap();
    // Also retain an interrupted but valid upload for resumed delivery.
    let partial = b"partial upload";
    let Response::Upload {
        upload_id: partial_id,
        ..
    } = c
        .handle_at(
            &p,
            Request::BeginUpload {
                assignment_id: assignment.clone(),
                generation: 1,
                artifact: ArtifactRef {
                    sha256: hex::encode(Sha256::digest(partial)),
                    size: partial.len() as u64,
                },
            },
            100,
        )
        .unwrap()
    else {
        panic!()
    };
    c.handle_at(
        &p,
        Request::UploadChunk {
            upload_id: partial_id,
            offset: 0,
            data_hex: hex::encode(&partial[..3]),
        },
        100,
    )
    .unwrap();
    (assignment, hash)
}
#[test]
fn snapshot_restore_preserves_ledger_artifacts_and_fences_uncertain_capacity() {
    let d = dir();
    let source = d.path().join("source");
    let (assignment, hash) = seed(&source);
    fs::write(
        source.join("operator-private-key.pem"),
        "must not be copied",
    )
    .unwrap();
    let snapshot = d.path().join("snapshot");
    let manifest = backup::create(&source, &snapshot, limits()).unwrap();
    assert_eq!(manifest.retained_allocations, 1);
    assert!(!snapshot.join("operator-private-key.pem").exists());
    assert!(
        manifest
            .files
            .iter()
            .any(|f| f.path.ends_with(".part") && f.size == 3)
    );
    assert!(Coordinator::open(config(&snapshot)).is_err());
    assert!(backup::create(&source, &snapshot, limits()).is_err());
    let restored = d.path().join("restored");
    assert!(backup::restore(&snapshot, &restored, limits(), false).is_err());
    assert!(!restored.exists());
    let result = backup::restore(&snapshot, &restored, limits(), true).unwrap();
    assert_eq!(result.fenced_allocations, 1);
    assert!(result.coordinator_epoch > manifest.coordinator_epoch);
    let state = StateStore::open_read_only(&restored).unwrap();
    let task = state.task("task").unwrap();
    assert_eq!(task.assignment_id.as_deref(), Some(assignment.as_str()));
    assert_eq!(task.generation, 1);
    assert_eq!(
        fs::read(restored.join(format!("artifacts/blobs/{hash}"))).unwrap(),
        b"durable checkpoint"
    );
    drop(state);
    let mut coordinator = Coordinator::open(config(&restored)).unwrap();
    let Response::Status { allocations, .. } = coordinator
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 101)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(allocations[0]["phase"], "uncertain");
    assert_eq!(allocations[0]["resources"]["cpu_millicores"], 1000);
    assert!(backup::restore(&snapshot, &restored, limits(), true).is_err());
    assert!(
        StateStore::open_read_only(&source)
            .unwrap()
            .task("task")
            .is_ok()
    );
}
#[test]
fn active_services_and_agent_outboxes_refuse_offline_snapshot() {
    use fs2::FileExt;
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let c = Coordinator::open(config(&source)).unwrap();
    assert!(backup::create(&source, &d.path().join("busy-coordinator"), limits()).is_err());
    drop(c);
    let agent = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(source.join("agent.lock"))
        .unwrap();
    agent.try_lock_exclusive().unwrap();
    assert!(backup::create(&source, &d.path().join("busy-agent"), limits()).is_err());
    drop(agent);
    fs::create_dir(source.join("attempts")).unwrap();
    fs::write(
        source.join("attempts/outbox.json"),
        "unpublished durable result",
    )
    .unwrap();
    assert!(backup::create(&source, &d.path().join("agent-outbox"), limits()).is_err());
}
#[test]
fn corrupt_missing_and_traversing_snapshots_fail_before_restore_destination() {
    let d = dir();
    let source = d.path().join("source");
    let (_, hash) = seed(&source);
    let snapshot = d.path().join("snapshot");
    backup::create(&source, &snapshot, limits()).unwrap();
    let blob = snapshot.join(format!("artifacts/blobs/{hash}"));
    fs::write(&blob, b"corrupted").unwrap();
    let target = d.path().join("corrupt-target");
    assert!(backup::restore(&snapshot, &target, limits(), true).is_err());
    assert!(!target.exists());
    fs::remove_file(blob).unwrap();
    assert!(backup::restore(&snapshot, &target, limits(), true).is_err());
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(snapshot.join(backup::MANIFEST_FILENAME)).unwrap())
            .unwrap();
    manifest["files"][0]["path"] = json!("../escape");
    fs::write(
        snapshot.join(backup::MANIFEST_FILENAME),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(backup::restore(&snapshot, &target, limits(), true).is_err());
    assert!(!target.exists());
}
#[test]
fn bounded_or_interrupted_copies_cannot_be_started_as_coordinators() {
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let target = d.path().join("too-small");
    let mut bound = limits();
    bound.max_bytes = 1;
    let failure = backup::create(&source, &target, bound).unwrap_err();
    assert!(
        target.join(backup::INCOMPLETE_FILENAME).exists(),
        "{failure:#}"
    );
    assert!(backup::ensure_runnable_state(&target).is_err());
    assert!(Coordinator::open(config(&target)).is_err());
    assert!(backup::restore(&target, &d.path().join("never-created"), limits(), true).is_err());
}
#[cfg(unix)]
#[test]
fn symlinked_files_and_nonhome_destinations_are_refused() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let d = dir();
    let source = d.path().join("source");
    let (_, hash) = seed(&source);
    let blob = source.join(format!("artifacts/blobs/{hash}"));
    fs::remove_file(&blob).unwrap();
    let unrelated = d.path().join("unrelated");
    fs::write(&unrelated, "not managed data").unwrap();
    symlink(&unrelated, &blob).unwrap();
    assert!(backup::create(&source, &d.path().join("symlink"), limits()).is_err());
    assert!(
        backup::create(
            &source,
            Path::new("/tmp/resmgr-forbidden-snapshot"),
            limits()
        )
        .is_err()
    );
    let mode = fs::metadata(d.path().join("symlink"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "not managed data");
}
#[test]
fn cli_requires_no_deployment_config_and_round_trips_offline() {
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let snapshot = d.path().join("cli-snapshot");
    let target = d.path().join("cli-restored");
    let command = env!("CARGO_BIN_EXE_resmgr");
    let output = std::process::Command::new(command)
        .args(["backup", "--state-dir"])
        .arg(&source)
        .arg("--destination")
        .arg(&snapshot)
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = std::process::Command::new(command)
        .arg("restore")
        .arg("--snapshot")
        .arg(&snapshot)
        .arg("--destination")
        .arg(&target)
        .arg("--confirm-source-stopped")
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(target.join("restore-receipt.json").is_file());
}

#[test]
fn rollback_snapshot_restore_preserves_profile_publications_and_uncertain_reservations() {
    let d = dir();
    let source = d.path().join("rollback-source");
    let profile = StorageProfile::DeleteExtra;
    let (assignment, hash) = seed_with_profile(&source, profile);
    let snapshot = d.path().join("rollback-snapshot");
    let manifest = backup::create(&source, &snapshot, limits()).unwrap();
    assert_eq!(manifest.storage_profile, profile);
    assert_eq!(manifest.retained_allocations, 1);
    assert!(
        manifest
            .files
            .iter()
            .any(|f| f.path.ends_with(".part") && f.size == 3)
    );
    let archived = StateStore::open_read_only_with_profile(&snapshot, profile).unwrap();
    assert_eq!(
        archived.durability_settings().unwrap().journal_mode,
        "delete"
    );
    assert_eq!(archived.durability_settings().unwrap().synchronous, 3);
    drop(archived);
    let destination = d.path().join("rollback-restored");
    let result = backup::restore(&snapshot, &destination, limits(), true).unwrap();
    assert_eq!(result.fenced_allocations, 1);
    assert!(result.coordinator_epoch > manifest.coordinator_epoch);
    let restored = StateStore::open_read_only_with_profile(&destination, profile).unwrap();
    assert_eq!(restored.storage_profile(), profile);
    assert_eq!(
        restored.task("task").unwrap().assignment_id.as_deref(),
        Some(assignment.as_str())
    );
    assert_eq!(
        fs::read(destination.join(format!("artifacts/blobs/{hash}"))).unwrap(),
        b"durable checkpoint"
    );
    drop(restored);
    assert!(
        Coordinator::open(config(&destination)).is_err(),
        "default profile cannot change restored rollback state"
    );
    let mut configuration = config(&destination);
    configuration.storage_profile = profile;
    let mut coordinator = Coordinator::open(configuration).unwrap();
    let Response::Status { allocations, .. } = coordinator
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 101)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0]["phase"], "uncertain");
    assert_eq!(allocations[0]["resources"]["cpu_millicores"], 1000);
    drop(coordinator);
    for path in [&source, &snapshot, &destination] {
        assert!(!path.join("state.sqlite3-wal").exists());
        assert!(!path.join("state.sqlite3-shm").exists());
    }
    // A changed manifest profile must fail before a new restoration directory
    // exists, even when all archived content hashes still match.
    let manifest_path = snapshot.join(backup::MANIFEST_FILENAME);
    let mut wrong: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    wrong["storage_profile"] = json!("wal_full");
    fs::write(&manifest_path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    let forbidden = d.path().join("mismatched-profile");
    assert!(backup::restore(&snapshot, &forbidden, limits(), true).is_err());
    assert!(!forbidden.exists());
}

#[test]
fn legacy_snapshot_manifest_without_profile_remains_wal_compatible() {
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let snapshot = d.path().join("snapshot");
    backup::create(&source, &snapshot, limits()).unwrap();
    let manifest_path = snapshot.join(backup::MANIFEST_FILENAME);
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("storage_profile");
    fs::write(&manifest_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let restored = d.path().join("restored");
    backup::restore(&snapshot, &restored, limits(), true).unwrap();
    assert_eq!(
        StateStore::open_read_only(&restored)
            .unwrap()
            .storage_profile(),
        StorageProfile::WalFull
    );
}

#[test]
fn replayable_local_state_cannot_become_an_offline_recovery_authority() {
    let d = dir();
    let source = d.path().join("replayable");
    drop(StateStore::open_with_profile(&source, StorageProfile::BurstReplayDeleteExtra).unwrap());
    let destination = d.path().join("forbidden-snapshot");
    assert!(backup::create(&source, &destination, Limits::default()).is_err());
    assert!(!destination.exists());
}

#[test]
fn restore_rejects_replayable_manifest_before_creating_destination() {
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let snapshot = d.path().join("snapshot");
    backup::create(&source, &snapshot, limits()).unwrap();
    let manifest_path = snapshot.join(backup::MANIFEST_FILENAME);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["storage_profile"] = json!("burst_replay_delete_extra");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let destination = d.path().join("forbidden-restore");
    let error = backup::restore(&snapshot, &destination, limits(), true).unwrap_err();
    assert!(format!("{error:#}").contains("authoritative reconciliation"));
    assert!(!destination.exists());
}

#[test]
fn offline_snapshot_restore_supports_replay_fenced_distributed_schema_two() {
    let d = dir();
    let source = d.path().join("source");
    seed(&source);
    let db = rusqlite::Connection::open(source.join(resource_manager::state::DATABASE_FILENAME))
        .unwrap();
    db.execute("UPDATE distributed_meta SET value=2 WHERE key='schema'", [])
        .unwrap();
    drop(db);
    let snapshot = d.path().join("snapshot-v2");
    backup::create(&source, &snapshot, limits()).unwrap();
    let restored = d.path().join("restored-v2");
    let report = backup::restore(&snapshot, &restored, limits(), true).unwrap();
    assert!(report.fenced_allocations > 0);
    let db = rusqlite::Connection::open(restored.join(resource_manager::state::DATABASE_FILENAME))
        .unwrap();
    let schema: i64 = db
        .query_row(
            "SELECT value FROM distributed_meta WHERE key='schema'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(schema, 2);
    let unfenced: i64 = db
        .query_row(
            "SELECT count(*) FROM reservations WHERE phase!='released' AND phase!='uncertain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unfenced, 0);
    drop(db);
    // A future distributed schema still fails closed before any snapshot output.
    let db = rusqlite::Connection::open(source.join(resource_manager::state::DATABASE_FILENAME))
        .unwrap();
    db.execute("UPDATE distributed_meta SET value=3 WHERE key='schema'", [])
        .unwrap();
    drop(db);
    let forbidden = d.path().join("future-schema-snapshot");
    assert!(backup::create(&source, &forbidden, limits()).is_err());
    assert!(!forbidden.exists());
}
