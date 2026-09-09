#![cfg(unix)]
use cedegrid::{
    namespace::Namespace,
    state::{StateStore, StorageProfile},
};
use std::{fs, io::Write, sync::mpsc, time::Duration};

#[test]
fn recovery_waits_for_db_and_output_lifetimes_and_fences_delayed_supervisor() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = temp.path().join("state");
    let store = StateStore::open(&root).unwrap();
    store.submit("retained-task", true).unwrap();
    let identity = store.namespace_guard().identity().clone();
    let capture_guard = store.namespace_guard();
    let (close_database, database_closed) = mpsc::channel();
    let (database_done, database_ack) = mpsc::channel();
    let (close_output, output_closed) = mpsc::channel();
    let (output_ready, ready) = mpsc::channel();
    let output = root.join("capture.log");
    let thread = std::thread::spawn(move || {
        let mut file = fs::File::create(output).unwrap();
        file.write_all(b"original capture").unwrap();
        output_ready.send(()).unwrap();
        database_closed.recv().unwrap();
        drop(store);
        database_done.send(()).unwrap();
        output_closed.recv().unwrap();
        file.sync_all().unwrap();
        drop(file);
        drop(capture_guard);
    });
    ready.recv().unwrap();
    let namespace = Namespace::new(&root).unwrap();
    let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
    maintenance.record_intent("new-session").unwrap();
    assert!(maintenance.exclusive(Duration::ZERO).is_err());
    assert!(
        namespace.begin_maintenance(Duration::ZERO).is_err(),
        "second recovery must not enter"
    );
    assert!(
        namespace.acquire(None, Duration::ZERO).is_err(),
        "new reader must wait outside admission"
    );
    assert!(root.join("state.sqlite3").is_file());
    close_database.send(()).unwrap();
    database_ack.recv().unwrap();
    assert!(
        maintenance.exclusive(Duration::ZERO).is_err(),
        "capture still owns namespace after DB closes"
    );
    assert_eq!(
        fs::read(root.join("capture.log")).unwrap(),
        b"original capture"
    );
    close_output.send(()).unwrap();
    thread.join().unwrap();
    let guard = maintenance.exclusive(Duration::ZERO).unwrap();
    fs::rename(&root, temp.path().join("quarantine")).unwrap();
    let guard = maintenance
        .initialize_session(guard, "new-session")
        .unwrap();
    let replacement =
        StateStore::open_with_profile_guarded(&root, StorageProfile::WalFull, guard.clone())
            .unwrap();
    assert!(
        replacement.task("retained-task").is_err(),
        "quarantined data must remain a separate namespace"
    );
    drop(replacement);
    let (_owner, active) = maintenance.activate(guard, Duration::ZERO).unwrap();
    assert_ne!(&identity, active.identity());
    assert!(
        namespace.acquire(Some(&identity), Duration::ZERO).is_err(),
        "late supervisor must fail before spec/DB/output open"
    );
    let new_reader = namespace
        .acquire(Some(active.identity()), Duration::ZERO)
        .unwrap();
    assert_eq!(new_reader.identity(), active.identity());
    let old = StateStore::open_read_only(&temp.path().join("quarantine")).unwrap();
    assert_eq!(old.task("retained-task").unwrap().task_id, "retained-task");
}

#[test]
fn nested_existing_store_reuses_guard_while_recovery_blocks_admission() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = temp.path().join("state");
    let store = StateStore::open(&root).unwrap();
    let namespace = Namespace::new(&root).unwrap();
    let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
    assert!(namespace.acquire(None, Duration::ZERO).is_err());
    let nested = StateStore::open_existing_with_profile_guarded(
        &root,
        StorageProfile::WalFull,
        store.namespace_guard(),
    )
    .unwrap();
    nested.submit("nested-operation", true).unwrap();
    assert!(maintenance.exclusive(Duration::ZERO).is_err());
    drop(store);
    assert!(maintenance.exclusive(Duration::ZERO).is_err());
    drop(nested);
    assert!(maintenance.exclusive(Duration::ZERO).is_ok());
}

#[test]
fn persisted_fencing_intent_prevents_readiness_after_recovery_process_dies() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = temp.path().join("state");
    let store = StateStore::open(&root).unwrap();
    store.submit("must-remain-retained", true).unwrap();
    drop(store);
    let namespace = Namespace::new(&root).unwrap();
    let old = namespace.state().unwrap().unwrap().identity;
    let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
    maintenance.record_intent("first-fencing-request").unwrap();
    drop(maintenance); // crash boundary before any coordinator acknowledgement
    assert!(namespace.acquire(None, Duration::ZERO).is_err());
    assert!(root.join("state.sqlite3").is_file());
    let restarted = namespace.begin_maintenance(Duration::ZERO).unwrap();
    restarted.record_intent("fresh-process-session").unwrap();
    let histories: Vec<_> = fs::read_dir(namespace.control_dir())
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("recovery-history-")
        })
        .collect();
    assert_eq!(histories.len(), 1);
    let history: cedegrid::namespace::RecoveryState =
        serde_json::from_slice(&fs::read(histories[0].path()).unwrap()).unwrap();
    assert_eq!(
        history.operation_id.as_deref(),
        Some("first-fencing-request")
    );
    assert_eq!(history.identity, old);
    let exclusive = restarted.exclusive(Duration::ZERO).unwrap();
    let preserved = StateStore::open_existing_with_profile_guarded(
        &root,
        StorageProfile::WalFull,
        exclusive.clone(),
    )
    .unwrap();
    assert_eq!(
        preserved.task("must-remain-retained").unwrap().task_id,
        "must-remain-retained"
    );
    drop(preserved);
    let exclusive = restarted
        .initialize_session(exclusive, "fresh-process-session")
        .unwrap();
    let (_owner, guard) = restarted.activate(exclusive, Duration::ZERO).unwrap();
    assert_ne!(guard.identity(), &old);
}

#[test]
fn namespace_rejects_lock_substitution_symlinks_and_hardlinks() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = temp.path().join("state");
    let namespace = Namespace::new(&root).unwrap();
    let path = namespace.control_dir().join("lifecycle.lock");
    fs::hard_link(&path, temp.path().join("extra-link")).unwrap();
    assert!(namespace.acquire(None, Duration::ZERO).is_err());
    fs::remove_file(temp.path().join("extra-link")).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(temp.path().join("other"), &path).unwrap();
    assert!(namespace.acquire(None, Duration::ZERO).is_err());
}

#[test]
fn replaced_regular_lock_cannot_split_an_existing_lifecycle() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = temp.path().join("state");
    let namespace = Namespace::new(&root).unwrap();
    let guard = namespace.acquire(None, Duration::ZERO).unwrap();
    let lock = namespace.control_dir().join("lifecycle.lock");
    fs::rename(&lock, namespace.control_dir().join("old-lifecycle.lock")).unwrap();
    fs::write(&lock, b"").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Namespace::new(&root).is_err());
    let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
    assert!(maintenance.exclusive(Duration::ZERO).is_err());
    drop(guard);
    assert!(
        maintenance.exclusive(Duration::ZERO).is_err(),
        "even no holder cannot authorize an unanchored replacement inode"
    );
}
