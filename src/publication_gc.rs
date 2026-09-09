//! Persistent attempt pins protect native publication and upload lifetimes.
//! Garbage collection proves process release and receipt identity before unlink.
use crate::{
    agent,
    execution_model::{ExecutionPhase, ProcessIdentity},
    managed_children::ManagedChildPhase,
    namespace::Namespace,
    protocol::{Response, ResultSubmission},
    state::StateStore,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::Path,
};

pub struct AttemptPin {
    store: StateStore,
    id: String,
}
impl Drop for AttemptPin {
    fn drop(&mut self) {
        // Failure conservatively leaves a crash pin. Its verified owner identity
        // must later be proven absent before GC can remove it.
        let _ = self.store.connection.execute(
            "DELETE FROM local_publication_pins WHERE pin_id=?1",
            [&self.id],
        );
    }
}
fn initialize(db: &Connection) -> Result<()> {
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='local_publication_pins')", [], |r| r.get(0))?;
    if !exists {
        db.execute_batch("CREATE TABLE IF NOT EXISTS local_publication_pins(pin_id TEXT PRIMARY KEY,assignment_id TEXT NOT NULL,output_path TEXT NOT NULL,owner_json TEXT NOT NULL) STRICT;
        CREATE INDEX IF NOT EXISTS local_publication_pins_attempt ON local_publication_pins(assignment_id);")?;
    }
    Ok(())
}
fn assignment(output: &Path) -> Result<&str> {
    output
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid attempt output")
}
pub fn pin_attempt(store: &StateStore, output: &Path) -> Result<AttemptPin> {
    let root = store
        .database_path()
        .parent()
        .context("state root missing")?;
    ensure!(
        output.parent() == Some(root.join("attempts").as_path()),
        "pin output must be a direct native attempt directory"
    );
    let assignment = assignment(output)?;
    ensure!(
        !is_upgrade_held(&store.connection, assignment)?,
        "legacy attempt is held for explicit upgrade reconciliation"
    );
    let owned = StateStore::open_existing_with_profile_guarded(
        root,
        store.storage_profile(),
        store.namespace_guard(),
    )?;
    initialize(&owned.connection)?;
    let generation = owned
        .execution_record(assignment)?
        .map_or(0, |r| r.generation);
    let identity =
        crate::supervision::process_identity(std::process::id(), assignment, generation)?;
    let id = uuid::Uuid::new_v4().to_string();
    owned.connection.execute(
        "INSERT INTO local_publication_pins VALUES(?1,?2,?3,?4)",
        params![
            id,
            assignment,
            output.to_str().context("non-UTF8 output path")?,
            serde_json::to_string(&identity)?
        ],
    )?;
    Ok(AttemptPin { store: owned, id })
}
#[derive(Serialize, Deserialize)]
struct Accepted {
    response: Response,
    submission: ResultSubmission,
}

/// Returns logical bytes unlinked in this pass. Pins and unresolved publications
/// cause a safe no-op. Descriptors/receipts remain immutable recovery evidence.
pub fn reclaim_released(store: &StateStore, output: &Path) -> Result<Option<u64>> {
    let root = store
        .database_path()
        .parent()
        .context("state root missing")?;
    ensure!(
        output.parent() == Some(root.join("attempts").as_path()),
        "GC output must remain within native attempts"
    );
    let assignment = assignment(output)?;
    if is_upgrade_held(&store.connection, assignment)? {
        return Ok(None);
    }
    let Some(record) = store.execution_record(assignment)? else {
        return Ok(None);
    };
    if record.phase != ExecutionPhase::Released || !agent::recovery_allows_release(&record) {
        return Ok(None);
    }
    if store
        .managed_children(assignment)?
        .iter()
        .any(|r| r.phase != ManagedChildPhase::Released)
    {
        return Ok(None);
    }
    let supervisor_path = output.join("supervisor-identity.json");
    let supervisor: ProcessIdentity = match read_optional(&supervisor_path, 16 * 1024)? {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => return Ok(None),
    };
    if !agent::identity_absent(&supervisor) {
        return Ok(None);
    }
    let Some(bytes) = read_optional(&output.join("result.receipt.json"), 3 * 1024 * 1024)? else {
        return Ok(None);
    };
    let accepted: Accepted = crate::numeric::from_slice(&bytes)?;
    agent::verify_publication_receipt(&accepted.response, &accepted.submission)?;
    ensure!(
        accepted.submission.assignment_id == assignment
            && accepted.submission.generation == record.generation
            && accepted.submission.task_id == record.task_id,
        "GC receipt does not match the released attempt"
    );
    initialize(&store.connection)?;
    let tx = Transaction::new_unchecked(&store.connection, TransactionBehavior::Immediate)?;
    let pins: Vec<(String, String, String)> = {
        let mut q = tx.prepare("SELECT pin_id,owner_json,output_path FROM local_publication_pins WHERE assignment_id=?1")?;
        q.query_map([assignment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, owner, path) in pins {
        ensure!(Path::new(&path) == output, "pin output identity mismatch");
        let owner: ProcessIdentity = serde_json::from_str(&owner)?;
        if !agent::identity_absent(&owner) {
            return Ok(None);
        }
        tx.execute("DELETE FROM local_publication_pins WHERE pin_id=?1", [id])?;
    }
    // A current committed head must equal the receipt we validated. A historical
    // descriptor read cannot authorize reclamation after a newer head appeared.
    if let Some(head) = crate::publication::read_head(store, output, "result")? {
        let current: serde_json::Value = crate::numeric::from_slice(&head)?;
        ensure!(
            current == accepted.submission.result,
            "GC result head changed since authoritative acceptance"
        );
    } else if accepted.submission.result.get("publication_id").is_some() {
        return Ok(None);
    }
    let publication_table: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='local_publications')", [], |r| r.get(0))?;
    if publication_table {
        let uncertain: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_publications WHERE assignment_id=?1 AND state NOT IN ('committed','rejected'))", [assignment], |r| r.get(0))?;
        if uncertain {
            return Ok(None);
        }
    }
    let ledger: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='local_spool_entries')", [], |r| r.get(0))?;
    let mut directories = vec![output.join("artifacts"), output.join("staging")];
    // Worker-owned external temporary/output files may still have arbitrary open
    // handles in unsupported daemonized children. Only native copied input/resume
    // files are reclaimed here, after the supported family is verified released.
    if let Some(spec) = read_optional(&output.join("supervisor-spec.json"), 3 * 1024 * 1024)? {
        let spec: crate::agent::SupervisorSpec = serde_json::from_slice(&spec)?;
        ensure!(
            spec.assignment.request.assignment_id == assignment
                && spec.assignment.generation == record.generation,
            "GC supervisor spec identity mismatch"
        );
        if let Some(context) = spec.assignment.request.env.get("CEDEGRID_CONTEXT") {
            let workspace = Path::new(context)
                .parent()
                .context("workspace parent missing")?;
            let control = Namespace::new(root)?;
            ensure!(
                workspace.starts_with(control.control_dir().join("workspaces"))
                    && workspace.file_name().and_then(|n| n.to_str()) == Some(assignment),
                "GC workspace escaped namespace"
            );
            directories.push(workspace.join("inputs"));
            directories.push(workspace.join("resume"));
        }
    }
    let mut unlinked = 0u64;
    for directory in directories {
        let meta = match fs::symlink_metadata(&directory) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "GC directory substituted"
        );
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file(),
                "GC refuses nonregular artifact entry"
            );
            let path = entry.path();
            let file = crate::publication::open_regular(&path, false)?;
            let metadata = file.metadata()?;
            verify_named_file(&path, &metadata)?;
            fs::remove_file(&path)?;
            File::open(&directory)?.sync_all()?;
            // The unlink and its directory barrier precede releasing the charge.
            if ledger {
                tx.execute(
                    "DELETE FROM local_spool_entries WHERE path=?1",
                    [path.to_str().context("non-UTF8 spool path")?],
                )?;
            }
            unlinked = unlinked
                .checked_add(metadata.len())
                .context("reclaimed byte count overflow")?;
        }
        if ledger {
            // A prior GC crash may have unlinked/synced an object but retained its
            // quota row. Prove absence under the same admission/GC transaction.
            let prefix = format!("{}/", directory.display());
            let upper = format!("{}\u{10ffff}", prefix);
            let rows: Vec<String> = {
                let mut q = tx.prepare("SELECT path FROM local_spool_entries WHERE path>=?1 AND path<?2 ORDER BY path LIMIT 1024")?;
                q.query_map(params![prefix, upper], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for path in rows {
                if Path::new(&path).parent() == Some(directory.as_path())
                    && fs::symlink_metadata(&path)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    File::open(&directory)?.sync_all()?;
                    tx.execute("DELETE FROM local_spool_entries WHERE path=?1", [path])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(Some(unlinked))
}
fn is_upgrade_held(db: &Connection, assignment: &str) -> Result<bool> {
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='upgrade_attempt_holds')", [], |r|r.get(0))?;
    Ok(exists
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM upgrade_attempt_holds WHERE assignment_id=?1)",
            [assignment],
            |r| r.get(0),
        )?)
}
fn read_optional(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    match crate::publication::read_bounded(path, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
fn verify_named_file(path: &Path, before: &fs::Metadata) -> Result<()> {
    #[cfg(not(unix))]
    let _ = before;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        named.is_file() && !named.file_type().is_symlink(),
        "GC artifact type changed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == named.dev()
                && before.ino() == named.ino()
                && before.len() == named.len(),
            "GC artifact identity changed"
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        execution_model::{AllocationClass, ExecutionJournal, LaunchRequest},
        model::Resources,
        protocol::Receipt,
    };
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, path::PathBuf, time::Duration};

    fn fixture() -> (tempfile::TempDir, StateStore, PathBuf) {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path().join("state");
        let store = StateStore::open(&root).unwrap();
        let resources = Resources {
            cpu_millicores: 100,
            ram_mib: 32,
            gpu_memory_mib: BTreeMap::new(),
        };
        let request = LaunchRequest {
            task_id: "task".into(),
            assignment_id: "attempt".into(),
            argv: vec!["unused".into()],
            cwd: temp.path().into(),
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
        record.phase = ExecutionPhase::Released;
        store.transition(&record).unwrap();
        let output = root.join("attempts/attempt");
        fs::create_dir_all(output.join("artifacts")).unwrap();
        fs::write(output.join("artifacts/value"), b"retained").unwrap();
        let mut absent =
            crate::supervision::process_identity(std::process::id(), "attempt", record.generation)
                .unwrap();
        absent.start_time += 1;
        fs::write(
            output.join("supervisor-identity.json"),
            serde_json::to_vec(&absent).unwrap(),
        )
        .unwrap();
        let submission = ResultSubmission {
            task_id: "task".into(),
            assignment_id: "attempt".into(),
            generation: record.generation,
            result: serde_json::json!({"metadata":{"value":"original"}}),
            artifacts: vec![],
        };
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&submission).unwrap())
        );
        let accepted = Accepted {
            response: Response::Receipt {
                receipt: Receipt {
                    task_id: "task".into(),
                    assignment_id: "attempt".into(),
                    generation: record.generation,
                    receipt_hash: hash,
                },
            },
            submission,
        };
        fs::write(
            output.join("result.receipt.json"),
            serde_json::to_vec(&accepted).unwrap(),
        )
        .unwrap();
        store.connection.execute_batch("CREATE TABLE local_spool_entries(path TEXT PRIMARY KEY,bytes INTEGER NOT NULL,assignment_id TEXT NOT NULL) STRICT;").unwrap();
        store
            .connection
            .execute(
                "INSERT INTO local_spool_entries VALUES(?1,8,'attempt')",
                [output.join("artifacts/value").to_str().unwrap()],
            )
            .unwrap();
        (temp, store, output)
    }
    #[test]
    fn live_upload_pin_blocks_gc_and_unlink_precedes_quota_release() {
        let (_temp, store, output) = fixture();
        let pin = pin_attempt(&store, &output).unwrap();
        assert_eq!(reclaim_released(&store, &output).unwrap(), None);
        assert!(output.join("artifacts/value").exists());
        drop(pin);
        assert_eq!(reclaim_released(&store, &output).unwrap(), Some(8));
        assert!(!output.join("artifacts/value").exists());
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM local_spool_entries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(output.join("result.receipt.json").exists());
    }
    #[test]
    fn a_pin_owns_its_lifecycle_until_database_close() {
        let (_temp, store, output) = fixture();
        let root = store.database_path().parent().unwrap().to_owned();
        let pin = pin_attempt(&store, &output).unwrap();
        drop(store);
        let namespace = Namespace::new(&root).unwrap();
        let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
        assert!(maintenance.exclusive(Duration::ZERO).is_err());
        drop(pin);
        assert!(maintenance.exclusive(Duration::ZERO).is_ok());
    }
    #[test]
    fn stale_receipt_cannot_delete_a_replaced_current_head() {
        let (_temp, store, output) = fixture();
        let file = output.join("new-head.json");
        let bytes = br#"{"metadata":{"value":"newer"}}"#;
        fs::write(&file, bytes).unwrap();
        store.connection.execute_batch("CREATE TABLE local_publications(assignment_id TEXT,publication_id TEXT,path TEXT,sha256 TEXT,state TEXT);CREATE TABLE local_publication_heads(assignment_id TEXT,publication_id TEXT,kind TEXT);").unwrap();
        store
            .connection
            .execute(
                "INSERT INTO local_publications VALUES('attempt','new',?1,?2,'committed')",
                params![
                    file.to_str().unwrap(),
                    format!("{:x}", Sha256::digest(bytes))
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO local_publication_heads VALUES('attempt','new','result')",
                [],
            )
            .unwrap();
        assert!(reclaim_released(&store, &output).is_err());
        assert!(output.join("artifacts/value").exists());
        assert_eq!(
            store
                .connection
                .query_row("SELECT sum(bytes) FROM local_spool_entries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            8
        );
    }
    #[test]
    fn crash_after_unlink_conservatively_retains_then_reconciles_charge() {
        let (_temp, store, output) = fixture();
        fs::remove_file(output.join("artifacts/value")).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT sum(bytes) FROM local_spool_entries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            8
        );
        assert_eq!(reclaim_released(&store, &output).unwrap(), Some(0));
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM local_spool_entries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
