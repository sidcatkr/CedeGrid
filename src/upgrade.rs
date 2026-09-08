//! Explicit offline 0.1 -> 0.2 upgrades. Full recovery bundles preserve the old
//! database, sidecars and outboxes; no historical payload or identity is renamed.
use crate::{
    execution_model::{ExecutionPhase, ExecutionRecord, ProcessIdentity},
    namespace::{Namespace, NamespaceGuard},
    state::{DATABASE_FILENAME, SCHEMA_VERSION, StorageProfile},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct UpgradeReport {
    pub format_version: u32,
    pub state_dir: PathBuf,
    pub backup: PathBuf,
    pub from_schema: i64,
    pub to_schema: i64,
    pub storage_profile: StorageProfile,
    pub requires_authenticated_recovery: bool,
    pub held_tasks: u64,
    pub files: Vec<UpgradeFile>,
    #[serde(default)]
    pub legacy_outboxes: Vec<LegacyOutbox>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct LegacyOutbox {
    pub path: PathBuf,
    pub sha256: String,
    pub status: String,
    pub detail: String,
    pub task_id: Option<String>,
    pub assignment_id: Option<String>,
    pub generation: Option<u64>,
    pub receipt_hash: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct UpgradeFile {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

/// The confirmation supplements independently verified process identities. It
/// never makes a live/unknown writer safe and never authorizes killing by PID.
pub fn upgrade(
    state_dir: &Path,
    backup: &Path,
    profile: StorageProfile,
    confirm_legacy_stopped: bool,
) -> Result<UpgradeReport> {
    ensure!(
        confirm_legacy_stopped,
        "offline upgrade requires --confirm-legacy-stopped"
    );
    ensure!(
        state_dir.is_absolute() && backup.is_absolute(),
        "upgrade paths must be absolute"
    );
    ensure!(
        !backup.starts_with(state_dir) && !state_dir.starts_with(backup),
        "upgrade bundle and state must be separate trees"
    );
    let namespace = Namespace::new(state_dir)?;
    let maintenance = namespace.begin_maintenance(Duration::ZERO)?;
    let guard = maintenance.exclusive(Duration::ZERO)?;
    let state_dir = namespace.root();
    let _legacy_locks = legacy_locks(state_dir)?;
    let operation_path = namespace.control_dir().join("upgrade-operation.json");
    let resumed: Option<UpgradeReport> = if operation_path.exists() {
        Some(serde_json::from_slice(&read_regular(
            &operation_path,
            32 * 1024 * 1024,
        )?)?)
    } else {
        None
    };
    if let Some(report) = &resumed {
        ensure!(
            report.backup == backup
                && report.state_dir == state_dir
                && report.storage_profile == profile,
            "interrupted upgrade must resume the same recorded bundle/profile"
        );
        verify_bundle(backup, &report.files)?;
    } else {
        ensure!(
            !backup.exists(),
            "upgrade backup already exists; refusing overwrite"
        );
    }
    // Inspect effective schema in a separate complete copy. SQLite may recover
    // its journals there without changing the original or the untouched bundle.
    let inspection = namespace
        .control_dir()
        .join(format!("inspection-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&inspection)?;
    private_permissions(&inspection, true)?;
    let inspected_files = copy_tree(state_dir, &inspection)?;
    let inspected = Connection::open_with_flags(
        inspection.join(DATABASE_FILENAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let version: i64 = inspected.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let persisted = read_profile(&inspected)?;
    ensure!(
        persisted == profile,
        "upgrade must preserve persisted storage profile"
    );
    let supported = match profile {
        StorageProfile::WalFull => version == 2,
        StorageProfile::DeleteExtra => version == 3,
        StorageProfile::BurstReplayDeleteExtra => version == 4,
    };
    ensure!(
        supported || (version == SCHEMA_VERSION && resumed.is_some()),
        "unsupported database schema {version}; original state is unchanged"
    );
    let distributed: Option<i64> = if has_table(&inspected, "distributed_meta")? {
        inspected
            .query_row(
                "SELECT value FROM distributed_meta WHERE key='schema'",
                [],
                |r| r.get(0),
            )
            .optional()?
    } else {
        None
    };
    ensure!(
        distributed.is_none_or(|v| matches!(v, 1..=3)),
        "unsupported distributed schema; original state is unchanged"
    );
    let integrity: String = inspected.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(
        integrity == "ok",
        "upgrade inspection failed database integrity: {integrity}"
    );
    if version == SCHEMA_VERSION
        && namespace
            .control_dir()
            .join("upgrade-complete.json")
            .is_file()
    {
        drop(inspected);
        fs::remove_dir_all(&inspection)?;
        return Ok(serde_json::from_slice(&read_regular(
            &namespace.control_dir().join("upgrade-complete.json"),
            32 * 1024 * 1024,
        )?)?);
    }
    verify_writer_quiescence(&inspected, &inspection, &inspected_files)?;
    let legacy_outboxes = index_legacy_outboxes(&inspected, &inspection, &inspected_files);
    drop(inspected);
    let mut report = if let Some(report) = resumed {
        report
    } else {
        fs::create_dir(backup)?;
        private_permissions(backup, true)?;
        let files = copy_tree(state_dir, backup)?;
        let report = UpgradeReport {
            format_version: 2,
            state_dir: state_dir.into(),
            backup: backup.into(),
            from_schema: version,
            to_schema: SCHEMA_VERSION,
            storage_profile: profile,
            requires_authenticated_recovery: profile.is_replayable(),
            held_tasks: 0,
            files,
            legacy_outboxes,
        };
        write_new_json(&backup.join("upgrade-bundle-manifest.json"), &report)?;
        File::open(backup)?.sync_all()?;
        File::open(backup.parent().unwrap())?.sync_all()?;
        write_new_json(&operation_path, &report)?;
        report
    };
    verify_bundle(backup, &report.files)?;
    maintenance.record_intent("offline-upgrade")?;
    if profile.is_replayable() {
        // Local replay state cannot become recovery authority. The next agent
        // start gets fresh coordinator fencing before quarantine/reconstruction.
        let marker = namespace.control_dir().join("legacy-replay-upgrade.json");
        if !marker.exists() {
            write_new_json(&marker, &report)?;
        }
    } else {
        report.held_tasks = migrate_database(state_dir, profile, &guard)?;
    }
    let completed = namespace.control_dir().join("upgrade-complete.json");
    if !completed.exists() {
        write_new_json(&completed, &report)?;
    }
    maintenance.set_phase("ready")?;
    // Inspection copies contain private state and are never public artifacts.
    fs::remove_dir_all(&inspection)?;
    File::open(namespace.control_dir())?.sync_all()?;
    Ok(report)
}

pub(crate) fn require_replay_upgrade(namespace: &Namespace, guard: &NamespaceGuard) -> Result<()> {
    guard.validate_root(namespace.root())?;
    ensure!(
        guard.is_exclusive(),
        "replay schema inspection requires exclusive lifecycle"
    );
    let path = namespace.root().join(DATABASE_FILENAME);
    if !path.exists() {
        return Ok(());
    }
    let header = read_regular(&path, 100).or_else(|_| {
        let mut f = open_regular(&path)?;
        let mut bytes = vec![0; 100];
        f.read_exact(&mut bytes)?;
        Ok::<_, anyhow::Error>(bytes)
    })?;
    let schema = (header.len() >= 100 && &header[..16] == b"SQLite format 3\0")
        .then(|| u32::from_be_bytes(header[60..64].try_into().unwrap()));
    ensure!(
        schema.is_none_or(|v| v <= SCHEMA_VERSION as u32),
        "replay state has unsupported future schema"
    );
    let legacy = matches!(schema, Some(1..=4));
    let unknown_previous_version =
        namespace.state()?.is_none() && schema != Some(SCHEMA_VERSION as u32);
    ensure!(
        !(legacy || unknown_previous_version)
            || namespace
                .control_dir()
                .join("legacy-replay-upgrade.json")
                .is_file(),
        "legacy replay state requires explicit offline state upgrade before authenticated recovery"
    );
    Ok(())
}

pub(crate) fn migrate_database(
    root: &Path,
    profile: StorageProfile,
    guard: &NamespaceGuard,
) -> Result<u64> {
    guard.validate_root(root)?;
    ensure!(guard.is_exclusive(), "upgrade requires exclusive lifecycle");
    let mut connection = Connection::open_with_flags(
        root.join(DATABASE_FILENAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    ensure!(
        read_profile(&connection)? == profile,
        "state profile changed since inspection"
    );
    connection.pragma_update(None, "synchronous", profile.synchronous())?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    #[cfg(target_os = "macos")]
    {
        connection.pragma_update(None, "fullfsync", "ON")?;
        connection.pragma_update(None, "checkpoint_fullfsync", "ON")?;
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS state_storage_profile(singleton INTEGER PRIMARY KEY CHECK(singleton=1),profile TEXT NOT NULL CHECK(profile IN ('wal_full','delete_extra','burst_replay_delete_extra'))) STRICT;
        CREATE TABLE IF NOT EXISTS upgrade_task_holds(task_id TEXT PRIMARY KEY REFERENCES tasks(task_id)) STRICT;
        CREATE TABLE IF NOT EXISTS upgrade_attempt_holds(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id)) STRICT;")?;
    tx.execute(
        "INSERT OR IGNORE INTO state_storage_profile VALUES(1,?1)",
        [profile.name()],
    )?;
    tx.execute("INSERT OR IGNORE INTO upgrade_task_holds SELECT task_id FROM tasks WHERE status!='completed'", [])?;
    tx.execute(
        "INSERT OR IGNORE INTO upgrade_attempt_holds SELECT assignment_id FROM assignments",
        [],
    )?;
    tx.execute("UPDATE tasks SET status='needs_reconciliation' WHERE task_id IN (SELECT task_id FROM upgrade_task_holds)", [])?;
    if has_table(&tx, "distributed_meta")? {
        crate::pagination::initialize(&tx)?;
        tx.execute("UPDATE distributed_meta SET value=3 WHERE key='schema'", [])?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    let held: i64 = tx.query_row("SELECT count(*) FROM upgrade_task_holds", [], |r| r.get(0))?;
    tx.commit()?;
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    ensure!(busy == 0, "upgrade checkpoint remained busy");
    drop(connection);
    open_regular(&root.join(DATABASE_FILENAME))?.sync_all()?;
    File::open(root)?.sync_all()?;
    Ok(held.try_into()?)
}
fn has_table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
fn index_legacy_outboxes(db: &Connection, root: &Path, files: &[UpgradeFile]) -> Vec<LegacyOutbox> {
    files.iter().filter(|file| file.path.starts_with("attempts") && file.path.file_name().and_then(|v|v.to_str()).is_some_and(|name| name == "result.json" || name == "checkpoint.json" || name.ends_with(".receipt.json"))).map(|file| {
        let mut entry = LegacyOutbox { path:file.path.clone(), sha256:file.sha256.clone(), status:"unresolved".into(), detail:String::new(), task_id:None, assignment_id:None, generation:None, receipt_hash:None };
        let result: Result<()> = (|| {
            ensure!(file.size <= 3*1024*1024, "outbox exceeds 3 MiB inspection bound; original bytes retained");
            let bytes = read_regular(&root.join(&file.path), 3*1024*1024)?;
            let value: serde_json::Value = crate::numeric::from_slice(&bytes)?;
            let is_receipt = file.path.file_name().and_then(|n|n.to_str()).is_some_and(|n|n.ends_with(".receipt.json"));
            let (descriptor, receipt) = if is_receipt {
                let submission: crate::protocol::ResultSubmission = serde_json::from_value(value.get("submission").context("receipt submission missing")?.clone())?;
                let response: crate::protocol::Response = serde_json::from_value(value.get("response").context("receipt response missing")?.clone())?;
                crate::agent::verify_publication_receipt(&response, &submission)?;
                let crate::protocol::Response::Receipt { receipt } = response else { unreachable!() };
                ensure!(submission.result.get("task_id").and_then(|v|v.as_str()) == Some(submission.task_id.as_str()) && submission.result.get("assignment_id").and_then(|v|v.as_str()) == Some(submission.assignment_id.as_str()) && submission.result.get("generation").and_then(|v|v.as_u64()) == Some(submission.generation), "receipt descriptor identity mismatch");
                (submission.result, Some(receipt.receipt_hash))
            } else { (value, None) };
            let task = descriptor.get("task_id").and_then(|v|v.as_str()).context("descriptor task missing")?;
            let assignment = descriptor.get("assignment_id").and_then(|v|v.as_str()).context("descriptor assignment missing")?;
            let generation = descriptor.get("generation").and_then(|v|v.as_u64()).context("descriptor generation missing")?;
            entry.task_id=Some(task.into()); entry.assignment_id=Some(assignment.into()); entry.generation=Some(generation);
            ensure!(descriptor.get("schema_version").and_then(|v|v.as_u64()) == Some(1), "legacy descriptor format is not version 1");
            ensure!(matches!(descriptor.get("kind").and_then(|v|v.as_str()), Some("result" | "checkpoint")), "legacy descriptor kind invalid");
            let found: Option<(String, i64)> = db.query_row("SELECT task_id,generation FROM assignments WHERE assignment_id=?1", [assignment], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            ensure!(found.as_ref().is_some_and(|(t,g)|t==task && u64::try_from(*g).ok()==Some(generation)), "legacy outbox is not tied to its unchanged stored assignment");
            let artifacts = descriptor.get("artifacts").and_then(|v|v.as_array()).context("artifact list missing")?;
            if !is_receipt {
                for artifact in artifacts {
                    let relative = Path::new(artifact.get("path").and_then(|v|v.as_str()).context("artifact path missing")?);
                    ensure!(!relative.as_os_str().is_empty() && relative.components().all(|c|matches!(c,std::path::Component::Normal(_))), "artifact path escapes outbox");
                    let expected = artifact.get("size").and_then(|v|v.as_u64()).context("artifact size missing")?;
                    ensure!(expected <= 256*1024*1024, "legacy artifact exceeds supported bound");
                    let hash = artifact.get("sha256").and_then(|v|v.as_str()).context("artifact digest missing")?;
                    let mut input = open_regular(&root.join(file.path.parent().context("outbox parent missing")?).join(relative))?;
                    let mut digest = Sha256::new(); let mut size=0u64; let mut buffer=[0u8;64*1024];
                    loop { let n=input.read(&mut buffer)?; if n==0 {break;} size+=n as u64; ensure!(size<=expected,"legacy artifact grew or size mismatched");digest.update(&buffer[..n]); }
                    ensure!(size==expected && format!("{:x}",digest.finalize())==hash,"legacy artifact integrity mismatch");
                }
            }
            if let Some(hash) = receipt {
                let known: Option<String> = db.query_row("SELECT receipt_hash FROM tasks WHERE task_id=?1", [task], |r|r.get(0))?;
                ensure!(known.as_ref().is_none_or(|h|h==&hash) || descriptor["kind"]=="checkpoint", "receipt conflicts with existing authoritative identity/hash");
                entry.receipt_hash=Some(hash);entry.status="accepted_receipt".into();
            } else { entry.status="valid_finalized_descriptor".into(); }
            entry.detail="Original bytes and identity preserved; activation remains held for explicit reconciliation".into();
            Ok(())
        })();
        if let Err(error) = result { entry.detail=format!("{error:#}"); }
        entry
    }).collect()
}
fn read_profile(db: &Connection) -> Result<StorageProfile> {
    if !has_table(db, "state_storage_profile")? {
        return Ok(StorageProfile::WalFull);
    }
    let value: String = db.query_row(
        "SELECT profile FROM state_storage_profile WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    Ok(match value.as_str() {
        "wal_full" => StorageProfile::WalFull,
        "delete_extra" => StorageProfile::DeleteExtra,
        "burst_replay_delete_extra" => StorageProfile::BurstReplayDeleteExtra,
        _ => bail!("unknown persisted storage profile"),
    })
}
fn verify_writer_quiescence(db: &Connection, root: &Path, files: &[UpgradeFile]) -> Result<()> {
    if has_table(db, "executions")? {
        let mut statement = db.prepare("SELECT record_json FROM executions")?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            let record: ExecutionRecord = serde_json::from_str(&row?)?;
            if let Some(identity) = &record.identity {
                ensure!(
                    crate::agent::identity_absent(identity),
                    "live or unresolved workload identity blocks upgrade: {}",
                    record.assignment_id
                );
            }
            if record.phase == ExecutionPhase::Released {
                continue;
            }
            ensure!(
                crate::agent::recovery_allows_release(&record),
                "unresolved process family blocks upgrade: {}",
                record.assignment_id
            );
            ensure!(
                record
                    .identity
                    .as_ref()
                    .is_some_and(crate::agent::identity_absent),
                "live or unresolved workload writer blocks upgrade: {}",
                record.assignment_id
            );
        }
    }
    if has_table(db, "managed_children")? {
        let mut statement = db.prepare("SELECT record_json FROM managed_children")?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            let record: crate::managed_children::ManagedChildRecord = serde_json::from_str(&row?)?;
            if let Some(identity) = &record.identity {
                ensure!(
                    crate::agent::identity_absent(identity),
                    "live or unresolved managed identity blocks upgrade: {}",
                    record.child_id
                );
            }
            if record.phase == crate::managed_children::ManagedChildPhase::Released {
                continue;
            }
            ensure!(
                record
                    .identity
                    .as_ref()
                    .is_some_and(crate::agent::identity_absent),
                "live or unresolved managed writer blocks upgrade: {}",
                record.child_id
            );
        }
    }
    for file in files.iter().filter(|f| {
        f.path
            .file_name()
            .is_some_and(|n| n == "supervisor-identity.json")
    }) {
        let identity: ProcessIdentity =
            serde_json::from_slice(&read_regular(&root.join(&file.path), 16 * 1024)?)?;
        ensure!(
            crate::agent::identity_absent(&identity),
            "live or unresolved native supervisor blocks upgrade"
        );
    }
    Ok(())
}
fn legacy_locks(root: &Path) -> Result<Vec<File>> {
    let mut locks = vec![];
    for name in ["agent.lock", "coordinator.lock"] {
        let path = root.join(name);
        if !path.try_exists()? {
            continue;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(&path)?;
        ensure!(file.metadata()?.is_file(), "invalid legacy service lock");
        fs2::FileExt::try_lock_exclusive(&file).context("legacy service is still running")?;
        locks.push(file);
    }
    Ok(locks)
}
fn copy_tree(source: &Path, target: &Path) -> Result<Vec<UpgradeFile>> {
    fn visit(
        source: &Path,
        target: &Path,
        relative: &Path,
        files: &mut Vec<UpgradeFile>,
        total: &mut u64,
    ) -> Result<()> {
        for entry in fs::read_dir(source.join(relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "upgrade refuses symbolic links: {}",
                path.display()
            );
            if kind.is_dir() {
                fs::create_dir(target.join(&path))?;
                private_permissions(&target.join(&path), true)?;
                visit(source, target, &path, files, total)?;
                File::open(target.join(&path))?.sync_all()?;
            } else {
                ensure!(
                    kind.is_file(),
                    "upgrade refuses nonregular entries: {}",
                    path.display()
                );
                ensure!(files.len() < 100_000, "upgrade exceeds file-count bound");
                let mut input = open_regular(&source.join(&path))?;
                let expected = input.metadata()?.len();
                *total = total
                    .checked_add(expected)
                    .context("upgrade byte count overflow")?;
                ensure!(
                    *total <= 16 * 1024 * 1024 * 1024,
                    "upgrade exceeds 16 GiB bundle bound"
                );
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options
                        .mode(0o600)
                        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
                }
                let mut output = options.open(target.join(&path))?;
                let mut digest = Sha256::new();
                let mut bytes = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let n = input.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    bytes += n as u64;
                    ensure!(bytes <= expected, "upgrade source grew during copy");
                    digest.update(&buffer[..n]);
                    output.write_all(&buffer[..n])?;
                }
                ensure!(bytes == expected, "upgrade source size changed");
                output.sync_all()?;
                files.push(UpgradeFile {
                    path,
                    size: bytes,
                    sha256: format!("{:x}", digest.finalize()),
                });
            }
        }
        Ok(())
    }
    let mut files = vec![];
    visit(source, target, Path::new(""), &mut files, &mut 0)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    File::open(target)?.sync_all()?;
    Ok(files)
}
fn verify_bundle(root: &Path, files: &[UpgradeFile]) -> Result<()> {
    for entry in files {
        ensure!(
            !entry.path.is_absolute()
                && entry
                    .path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "invalid bundle member path"
        );
        let mut file = open_regular(&root.join(&entry.path))?;
        let mut digest = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            ensure!(size <= entry.size, "bundle size mismatch");
            digest.update(&buffer[..n]);
        }
        ensure!(
            size == entry.size && format!("{:x}", digest.finalize()) == entry.sha256,
            "bundle integrity mismatch: {}",
            entry.path.display()
        );
    }
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    let file = crate::publication::open_regular(path, false)?;
    ensure!(file.metadata()?.is_file(), "expected regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        ensure!(
            m.nlink() == 1 && m.uid() == unsafe { libc::geteuid() },
            "upgrade file must be uniquely linked and owned"
        );
    }
    Ok(file)
}
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let f = open_regular(path)?;
    let mut b = vec![];
    f.take(limit + 1).read_to_end(&mut b)?;
    ensure!(b.len() as u64 <= limit, "metadata exceeds bound");
    Ok(b)
}
fn private_permissions(path: &Path, _directory: bool) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut f = o.open(path)?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
