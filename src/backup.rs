//! Offline coordinator snapshots. Local agent outboxes are deliberately excluded
//! and cause refusal, rather than producing an incomplete recovery archive.
use crate::state::{self, DATABASE_FILENAME, StateStore};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rusqlite::{
    Connection, TransactionBehavior,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Explicitly ends lock ownership when the service/operation guard drops, even
/// if a concurrent fork temporarily retains a duplicate file description.
pub struct StateLock {
    file: File,
}
impl StateLock {
    pub(crate) fn from_locked(file: File) -> Self {
        Self { file }
    }
}
impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub const MANIFEST_FILENAME: &str = "snapshot-manifest.json";
pub const INCOMPLETE_FILENAME: &str = "offline-in-progress.json";
/// Every service entry must reject archived or incomplete offline state.
pub fn ensure_runnable_state(path: &Path) -> Result<()> {
    ensure!(
        !path.join(MANIFEST_FILENAME).exists() && !path.join(INCOMPLETE_FILENAME).exists(),
        "snapshot archives and incomplete offline outputs cannot run as service state"
    );
    Ok(())
}
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_files: usize,
    pub timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 20 * 1024 * 1024 * 1024,
            max_files: 100_000,
            timeout: Duration::from_secs(300),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub kind: String,
    pub created_at_unix_ms: u64,
    pub sqlite_version: String,
    #[serde(default)]
    pub storage_profile: state::StorageProfile,
    pub source_state_dir: PathBuf,
    pub coordinator_epoch: u64,
    pub retained_allocations: u64,
    pub files: Vec<FileEntry>,
    pub total_bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct RestoreReport {
    pub state_dir: PathBuf,
    pub snapshot_created_at_unix_ms: u64,
    pub fenced_allocations: u64,
    pub coordinator_epoch: u64,
    pub recovery_point: &'static str,
}
struct Budget {
    limits: Limits,
    start: Instant,
    bytes: u64,
    files: usize,
}
impl Budget {
    fn new(limits: Limits) -> Result<Self> {
        ensure!(
            limits.max_bytes > 0
                && (1..=100_000).contains(&limits.max_files)
                && !limits.timeout.is_zero(),
            "snapshot limits must be positive"
        );
        Ok(Self {
            limits,
            start: Instant::now(),
            bytes: 0,
            files: 0,
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(
            self.start.elapsed() < self.limits.timeout,
            "snapshot time budget exceeded"
        );
        Ok(())
    }
    fn add(&mut self, size: u64) -> Result<()> {
        self.check()?;
        self.bytes = self
            .bytes
            .checked_add(size)
            .context("snapshot byte count overflow")?;
        self.files = self
            .files
            .checked_add(1)
            .context("snapshot file count overflow")?;
        ensure!(
            self.bytes <= self.limits.max_bytes && self.files <= self.limits.max_files,
            "snapshot storage/file budget exceeded"
        );
        Ok(())
    }
}

/// A complete snapshot is committed by writing its manifest last. Existing output
/// directories are never reused, including a directory left by interrupted work.
pub fn create(source: &Path, destination: &Path, limits: Limits) -> Result<Manifest> {
    let mut budget = Budget::new(limits)?;
    let source = home_local_path(source, true)?;
    ensure_runnable_state(&source)?;
    ensure!(
        source.join(DATABASE_FILENAME).is_file(),
        "source coordinator database does not exist"
    );
    let _locks = offline_locks(&source, true)?;
    let profile = StateStore::open_read_only(&source)?.storage_profile();
    ensure!(
        !profile.is_replayable(),
        "offline snapshots require strong storage assurance; replayable local state is not a recovery authority"
    );
    let store = StateStore::open_with_profile(&source, profile)?;
    require_coordinator_only(&store, &source)?;
    checkpoint(&store.connection)?;
    integrity(&store.connection)?;
    let epoch: i64 = store.connection.query_row(
        "SELECT value FROM distributed_meta WHERE key='epoch'",
        [],
        |r| r.get(0),
    )?;
    let retained: i64 = store.connection.query_row(
        "SELECT count(*) FROM reservations WHERE phase!='released'",
        [],
        |r| r.get(0),
    )?;
    let destination = new_destination(destination)?;
    write_json(
        &destination.join(INCOMPLETE_FILENAME),
        &serde_json::json!({"operation":"offline_copy","completed":false}),
    )?;
    let _destination_locks = offline_locks(&destination, true)?;
    let page_size: i64 = store
        .connection
        .pragma_query_value(None, "page_size", |r| r.get(0))?;
    let page_count: i64 = store
        .connection
        .pragma_query_value(None, "page_count", |r| r.get(0))?;
    ensure!(
        u64::try_from(page_size)?
            .checked_mul(u64::try_from(page_count)?)
            .context("database size overflow")?
            <= limits.max_bytes,
        "database exceeds snapshot byte budget"
    );
    let database = destination.join(DATABASE_FILENAME);
    create_file(&database)?.sync_all()?;
    let mut target = Connection::open(&database)?;
    target.pragma_update(None, "synchronous", profile.synchronous())?;
    {
        let backup = Backup::new(&store.connection, &mut target)?;
        loop {
            budget.check()?;
            match backup.step(256)? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    bail!("database became busy during offline snapshot")
                }
                _ => bail!("unknown SQLite backup status"),
            }
            ensure!(
                fs::metadata(&database)?.len() <= limits.max_bytes,
                "database exceeds snapshot byte budget"
            );
        }
    }
    // The offline destination is new and exclusively owned. Backup copies the
    // profile metadata; match the connection pragmas before publishing it.
    target.pragma_update(None, "journal_mode", profile.journal_mode())?;
    target.pragma_update(None, "synchronous", profile.synchronous())?;
    let actual_mode: String = target.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let actual_sync: i64 = target.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    ensure!(
        actual_mode.eq_ignore_ascii_case(profile.journal_mode())
            && actual_sync == profile.synchronous(),
        "snapshot target did not apply the requested storage profile"
    );
    checkpoint(&target)?;
    integrity(&target)?;
    drop(target);
    let mut files = vec![copy_or_hash(
        &database,
        None,
        DATABASE_FILENAME,
        &mut budget,
    )?];
    for category in ["blobs", "uploads"] {
        let directory = source.join("artifacts").join(category);
        if !directory.exists() {
            continue;
        }
        ensure!(
            fs::symlink_metadata(&directory)?.file_type().is_dir(),
            "artifact directory must not be a symlink"
        );
        home_local_path(&directory, true)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory)? {
            ensure!(
                entries.len() < limits.max_files,
                "too many artifact directory entries"
            );
            entries.push(entry?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("artifact filename is not UTF-8"))?;
            // Uncommitted metadata temporary files have no publication semantics.
            if category == "uploads" && name.starts_with(".metadata-") && name.ends_with(".tmp") {
                continue;
            }
            let relative = format!("artifacts/{category}/{name}");
            validate_relative(&relative)?;
            let target = destination.join(&relative);
            make_artifact_parent(&destination, category)?;
            files.push(copy_or_hash(
                &entry.path(),
                Some(&target),
                &relative,
                &mut budget,
            )?);
        }
    }
    verify_publications(&store.connection, &files)?;
    let manifest = Manifest {
        schema_version: 1,
        kind: "coordinator_offline_snapshot".into(),
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
        sqlite_version: state::sqlite_version(),
        storage_profile: profile,
        source_state_dir: source,
        coordinator_epoch: epoch.try_into()?,
        retained_allocations: retained.try_into()?,
        files,
        total_bytes: budget.bytes,
    };
    write_json(&destination.join(MANIFEST_FILENAME), &manifest)?;
    fs::remove_file(destination.join(INCOMPLETE_FILENAME))?;
    sync_dir(&destination)?;
    sync_dir(destination.parent().unwrap())?;
    Ok(manifest)
}

/// Restoring an older snapshot is a point-in-time rollback, not ordinary crash
/// recovery. The operator must retire the original coordinator and reconcile any
/// post-snapshot side effects separately. No PKI or deployment files are copied.
pub fn restore(
    snapshot: &Path,
    destination: &Path,
    limits: Limits,
    confirm_source_stopped: bool,
) -> Result<RestoreReport> {
    ensure!(
        confirm_source_stopped,
        "restore requires explicit confirmation that the original coordinator is stopped and will not restart against its old state"
    );
    let mut budget = Budget::new(limits)?;
    let snapshot = home_local_path(snapshot, true)?;
    let _locks = offline_locks(&snapshot, false)?;
    ensure!(
        !snapshot.join(INCOMPLETE_FILENAME).exists(),
        "snapshot copy was interrupted"
    );
    let mut f = open_regular(&snapshot.join(MANIFEST_FILENAME))?;
    ensure!(
        f.metadata()?.len() <= 16 * 1024 * 1024,
        "snapshot manifest exceeds metadata bound"
    );
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    let manifest: Manifest = serde_json::from_slice(&data)?;
    ensure!(
        !manifest.storage_profile.is_replayable(),
        "offline restore requires strong storage assurance; replayable local state requires authoritative reconciliation"
    );
    ensure!(
        manifest.schema_version == 1 && manifest.kind == "coordinator_offline_snapshot",
        "unsupported snapshot format"
    );
    ensure!(
        manifest.files.len() <= limits.max_files && manifest.total_bytes <= limits.max_bytes,
        "snapshot exceeds restore budget"
    );
    let mut seen = BTreeSet::new();
    for entry in &manifest.files {
        validate_relative(&entry.path)?;
        crate::artifacts::validate_hash(&entry.sha256)?;
        ensure!(seen.insert(entry.path.clone()), "duplicate snapshot path");
        home_local_path(
            snapshot
                .join(&entry.path)
                .parent()
                .context("snapshot entry parent missing")?,
            true,
        )?;
        let actual = copy_or_hash(&snapshot.join(&entry.path), None, &entry.path, &mut budget)?;
        ensure!(
            actual.size == entry.size && actual.sha256 == entry.sha256,
            "snapshot integrity failure: {}",
            entry.path
        );
    }
    ensure!(
        seen.contains(DATABASE_FILENAME) && budget.bytes == manifest.total_bytes,
        "snapshot database/byte count missing or inconsistent"
    );
    // Verify schema/version/storage without changing the archived database.
    let original = StateStore::open_read_only_with_profile(&snapshot, manifest.storage_profile)?;
    require_coordinator_only(&original, &snapshot)?;
    integrity(&original.connection)?;
    verify_publications(&original.connection, &manifest.files)?;
    drop(original);
    let destination = new_destination(destination)?;
    write_json(
        &destination.join(INCOMPLETE_FILENAME),
        &serde_json::json!({"operation":"offline_copy","completed":false}),
    )?;
    let _destination_locks = offline_locks(&destination, true)?;
    for entry in &manifest.files {
        budget.check()?;
        if let Some(category) = entry
            .path
            .strip_prefix("artifacts/")
            .and_then(|s| s.split('/').next())
        {
            make_artifact_parent(&destination, category)?;
        }
        // Copy and hash again: modification after validation cannot silently pass.
        let actual = copy_verified(
            &snapshot.join(&entry.path),
            &destination.join(&entry.path),
            entry,
            &budget,
        )?;
        ensure!(actual == entry.sha256, "snapshot changed during restore");
    }
    let mut restored = StateStore::open_with_profile(&destination, manifest.storage_profile)?;
    require_coordinator_only(&restored, &destination)?;
    let tx = restored
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS allocation_fences(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),reason TEXT NOT NULL) STRICT;")?;
    let fenced=tx.execute("INSERT INTO allocation_fences(assignment_id,reason) SELECT assignment_id,'point-in-time restore requires verified release/reconciliation' FROM reservations WHERE phase!='released' ON CONFLICT(assignment_id) DO UPDATE SET reason=excluded.reason",[])?;
    tx.execute("UPDATE reservations SET phase='uncertain',lease_deadline_ms=0,detail='restored snapshot; capacity retained until verified reconciliation' WHERE phase!='released'",[])?;
    let previous: i64 = tx.query_row(
        "SELECT value FROM distributed_meta WHERE key='epoch'",
        [],
        |r| r.get(0),
    )?;
    let epoch = previous
        .checked_add(1)
        .context("coordinator epoch exhausted")?;
    tx.execute(
        "UPDATE distributed_meta SET value=?1 WHERE key='epoch'",
        [epoch],
    )?;
    tx.commit()?;
    checkpoint(&restored.connection)?;
    integrity(&restored.connection)?;
    drop(restored);
    open_regular(&destination.join(DATABASE_FILENAME))?.sync_all()?;
    let report = RestoreReport {
        state_dir: destination.clone(),
        snapshot_created_at_unix_ms: manifest.created_at_unix_ms,
        fenced_allocations: fenced as u64,
        coordinator_epoch: epoch.try_into()?,
        recovery_point: "Only commits captured by the snapshot are restored. Later results and external side effects require separate reconciliation.",
    };
    write_json(&destination.join("restore-receipt.json"), &report)?;
    fs::remove_file(destination.join(INCOMPLETE_FILENAME))?;
    sync_dir(&destination)?;
    sync_dir(destination.parent().unwrap())?;
    Ok(report)
}
fn require_coordinator_only(store: &StateStore, root: &Path) -> Result<()> {
    ensure!(
        !root.join("attempts").exists() && !root.join("refusals").exists(),
        "coordinator snapshots refuse agent state/outboxes; agent spool backup is unsupported"
    );
    let distributed: bool = store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='distributed_meta')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        distributed,
        "snapshot requires an initialized coordinator database"
    );
    let schema: i64 = store.connection.query_row(
        "SELECT value FROM distributed_meta WHERE key='schema'",
        [],
        |r| r.get(0),
    )?;
    ensure!(matches!(schema, 1 | 2), "unsupported distributed schema");
    let local: i64 = store
        .connection
        .query_row("SELECT count(*) FROM executions", [], |r| r.get(0))?;
    ensure!(
        local == 0,
        "snapshot refuses local execution journals; surviving supervisors/outboxes need separate reconciliation"
    );
    Ok(())
}
fn checkpoint(connection: &Connection) -> Result<()> {
    let mode: String = connection.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("delete") {
        return Ok(());
    }
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "unsupported snapshot journal mode"
    );
    let (busy, logged, done): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    ensure!(
        busy == 0 && (logged == done || logged == -1),
        "offline WAL checkpoint was blocked"
    );
    Ok(())
}
fn integrity(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(result == "ok", "SQLite integrity check failed");
    let mut s = connection.prepare("PRAGMA foreign_key_check")?;
    ensure!(
        s.query([])?.next()?.is_none(),
        "SQLite foreign key check failed"
    );
    Ok(())
}
fn verify_publications(connection: &Connection, files: &[FileEntry]) -> Result<()> {
    let index: BTreeMap<_, _> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut s = connection.prepare("SELECT DISTINCT sha256,size FROM publications")?;
    let mut rows = s.query([])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let size: i64 = row.get(1)?;
        let path = format!("artifacts/blobs/{hash}");
        let file = index
            .get(path.as_str())
            .context("published artifact missing from snapshot")?;
        ensure!(
            file.sha256 == hash && i64::try_from(file.size)? == size,
            "published artifact checksum/size mismatch"
        );
    }
    Ok(())
}
fn home_local_path(path: &Path, existing: bool) -> Result<PathBuf> {
    ensure!(
        path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "snapshot paths must be absolute without parent traversal"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("runtime HOME unavailable")?)
        .canonicalize()?;
    ensure!(
        path.starts_with(&home),
        "snapshot paths must stay inside runtime home"
    );
    let mut at = home.clone();
    for c in path.strip_prefix(&home)?.components() {
        at.push(c);
        match fs::symlink_metadata(&at) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "snapshot paths cannot contain symlinks"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        state::preflight(path)?.supported,
        "snapshot requires verified local filesystem"
    );
    if existing {
        ensure!(path.is_dir(), "snapshot directory does not exist");
        Ok(path.canonicalize()?)
    } else {
        Ok(path.to_path_buf())
    }
}
fn new_destination(path: &Path) -> Result<PathBuf> {
    let path = home_local_path(path, false)?;
    ensure!(
        path.parent()
            .context("destination missing parent")?
            .is_dir(),
        "destination parent must already exist"
    );
    make_dir_new(&path).context(
        "snapshot/restore destination must not already exist; incomplete outputs are never reused",
    )?;
    Ok(path)
}
fn make_dir_new(path: &Path) -> Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path)?;
    sync_dir(path)?;
    sync_dir(path.parent().context("directory has no parent")?)?;
    Ok(())
}
fn make_artifact_parent(root: &Path, category: &str) -> Result<()> {
    for p in [
        root.join("artifacts"),
        root.join("artifacts").join(category),
    ] {
        if !p.exists() {
            make_dir_new(&p)?;
        }
        ensure!(
            fs::symlink_metadata(&p)?.file_type().is_dir(),
            "artifact parent is not a direct directory"
        );
    }
    Ok(())
}
/// Standalone execution shares the node-agent lock so offline snapshots cannot
/// race a newly launched supervisor. The returned guard must outlive execution.
pub fn execution_lock(root: &Path) -> Result<StateLock> {
    ensure_runnable_state(root)?;
    service_lock(root, "agent.lock", true)
}
fn offline_locks(root: &Path, create: bool) -> Result<Vec<StateLock>> {
    Ok(vec![
        service_lock(root, "coordinator.lock", create)?,
        service_lock(root, "agent.lock", create)?,
    ])
}
fn service_lock(root: &Path, name: &str, create: bool) -> Result<StateLock> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(create).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(root.join(name))?;
    ensure!(f.metadata()?.is_file(), "invalid service lock file");
    f.try_lock_exclusive()
        .with_context(|| format!("active service prevents offline operation: {name}"))?;
    Ok(StateLock::from_locked(f))
}
fn validate_relative(value: &str) -> Result<()> {
    if value == DATABASE_FILENAME {
        return Ok(());
    }
    let parts: Vec<_> = value.split('/').collect();
    ensure!(
        parts.len() == 3 && parts[0] == "artifacts",
        "unexpected snapshot path"
    );
    let hash = match parts[1] {
        "blobs" => parts[2],
        "uploads" => parts[2]
            .strip_suffix(".json")
            .or_else(|| parts[2].strip_suffix(".part"))
            .context("invalid upload filename")?,
        _ => bail!("unexpected artifact directory"),
    };
    crate::artifacts::validate_hash(hash)
}
fn create_file(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(o.open(path)?)
}
fn open_regular(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(path)?;
    ensure!(
        f.metadata()?.is_file(),
        "snapshot entry is not a regular file"
    );
    Ok(f)
}
fn copy_or_hash(
    source: &Path,
    destination: Option<&Path>,
    relative: &str,
    budget: &mut Budget,
) -> Result<FileEntry> {
    let mut f = open_regular(source)?;
    let size = f.metadata()?.len();
    budget.add(size)?;
    let mut out = destination.map(create_file).transpose()?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 65536];
    loop {
        budget.check()?;
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).context("file byte overflow")?;
        ensure!(bytes <= size, "snapshot source grew during copy");
        digest.update(&buf[..n]);
        if let Some(out) = &mut out {
            out.write_all(&buf[..n])?;
        }
    }
    ensure!(bytes == size, "snapshot source shrank during copy");
    if let Some(out) = out {
        out.sync_all()?;
        sync_dir(destination.unwrap().parent().unwrap())?;
    } else {
        f.sync_all()?;
    }
    Ok(FileEntry {
        path: relative.into(),
        size,
        sha256: hex::encode(digest.finalize()),
    })
}
fn copy_verified(
    source: &Path,
    destination: &Path,
    entry: &FileEntry,
    budget: &Budget,
) -> Result<String> {
    let mut f = open_regular(source)?;
    ensure!(
        f.metadata()?.len() == entry.size,
        "snapshot changed before copy"
    );
    let mut out = create_file(destination)?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 65536];
    loop {
        budget.check()?;
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).context("copy byte overflow")?;
        ensure!(bytes <= entry.size, "snapshot grew during restore");
        digest.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    ensure!(bytes == entry.size, "snapshot shrank during restore");
    out.sync_all()?;
    sync_dir(destination.parent().unwrap())?;
    Ok(hex::encode(digest.finalize()))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    ensure!(
        data.len() <= 16 * 1024 * 1024,
        "offline metadata exceeds 16 MiB bound"
    );
    let mut f = create_file(path)?;
    f.write_all(&data)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    sync_dir(path.parent().unwrap())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    #[test]
    fn duplicate_description_does_not_prolong_finished_service_ownership() {
        let directory = tempfile::Builder::new()
            .prefix(".lock-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let path = directory.path().join("coordinator.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.try_lock_exclusive().unwrap();
        let inherited_description = file.try_clone().unwrap();
        let owner = StateLock::from_locked(file);
        assert!(service_lock(directory.path(), "coordinator.lock", false).is_err());
        drop(owner);
        let successor = service_lock(directory.path(), "coordinator.lock", false).unwrap();
        assert!(inherited_description.metadata().unwrap().is_file());
        drop(successor);
    }
}
