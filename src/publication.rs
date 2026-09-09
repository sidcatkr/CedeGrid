//! Native-only immutable worker publications, quota reservations and bounded file access.
use crate::{
    namespace::NamespaceGuard,
    state::{StateStore, StorageProfile},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

pub const DESCRIPTOR_LIMIT: usize = 1024 * 1024;
pub const LOCAL_CHUNK_LIMIT: usize = 256 * 1024;
pub const LOCAL_FRAME_LIMIT: usize = 3 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePublisherSettings {
    pub state_dir: PathBuf,
    pub storage_profile: StorageProfile,
    pub namespace_id: String,
    pub session_id: String,
    pub output_dir: PathBuf,
    pub task_id: String,
    pub assignment_id: String,
    pub generation: u64,
    pub max_spool_bytes: u64,
    pub max_artifact_bytes: u64,
}
struct Work {
    value: Value,
    reply: mpsc::Sender<Value>,
}
/// The worker owns a separate SQLite connection and lifecycle reference. Hashing
/// and synchronization never execute in the supervisor's lease/reaper loop.
pub struct PublisherService {
    sender: Option<mpsc::SyncSender<Work>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl PublisherService {
    pub fn new(settings: NativePublisherSettings, guard: NamespaceGuard) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Work>(16);
        let (started, ready) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("cedegrid-publication".into())
            .spawn(move || {
                let mut publisher = match NativePublisher::open(settings, guard) {
                    Ok(p) => {
                        let _ = started.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = started.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                while let Ok(work) = receiver.recv() {
                    let response = match publisher.request(&work.value) {
                        Ok(response) => response,
                        Err(error) => {
                            let mut response = error_response(&work.value, &format!("{error:#}"));
                            if let Some(id) = work.value["publication_id"].as_str()
                                && let Ok(status) = publisher.status(id)
                            {
                                response["sha256"] = status["sha256"].clone();
                                response["candidate_state"] = status["state"].clone();
                            }
                            response
                        }
                    };
                    let _ = work.reply.send(response);
                }
            })?;
        match ready.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                sender: Some(sender),
                worker: Some(worker),
            }),
            other => {
                drop(sender);
                let _ = worker.join();
                bail!("native publication initialization failed: {other:?}")
            }
        }
    }
    pub fn submit(&self, value: Value) -> Result<mpsc::Receiver<Value>> {
        let (reply, receiver) = mpsc::channel();
        self.sender
            .as_ref()
            .context("publication server closed")?
            .try_send(Work { value, reply })
            .context("ERR_CEDEGRID_BUSY: publication queue full")?;
        Ok(receiver)
    }
}
impl Drop for PublisherService {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn error_response(value: &Value, message: &str) -> Value {
    let code = message
        .split(|c: char| c == ':' || c.is_whitespace())
        .find(|part| part.starts_with("ERR_CEDEGRID_"))
        .unwrap_or("ERR_CEDEGRID_PUBLICATION");
    json!({"ok":false,"code":code,"message":message,"error":message,"request_id":value["request_id"],"publication_id":value["publication_id"],"assignment_id":value["assignment_id"],"generation":value["generation"],"namespace_id":value["namespace_id"],"session_id":value["session_id"]})
}
pub struct NativePublisher {
    settings: NativePublisherSettings,
    store: StateStore,
    _pin: crate::publication_gc::AttemptPin,
    #[cfg(test)]
    fail_after_insert: bool,
}
impl NativePublisher {
    pub fn open(settings: NativePublisherSettings, guard: NamespaceGuard) -> Result<Self> {
        ensure!(
            guard.identity().namespace_id == settings.namespace_id
                && guard
                    .identity()
                    .session_id
                    .as_deref()
                    .unwrap_or(&settings.namespace_id)
                    == settings.session_id,
            "ERR_CEDEGRID_STALE_ATTEMPT: namespace/session mismatch"
        );
        ensure!(
            settings.output_dir.starts_with(&settings.state_dir)
                && settings.generation <= i64::MAX as u64
                && settings.max_spool_bytes <= i64::MAX as u64,
            "invalid publisher settings"
        );
        let store = StateStore::open_existing_with_profile_guarded(
            &settings.state_dir,
            settings.storage_profile,
            guard,
        )?;
        initialize(&store)?;
        for dir in ["staging", "artifacts", "publications"] {
            private_dir(&settings.output_dir.join(dir))?;
        }
        let pin = crate::publication_gc::pin_attempt(&store, &settings.output_dir)?;
        let publisher = Self {
            settings,
            store,
            _pin: pin,
            #[cfg(test)]
            fail_after_insert: false,
        };
        account_existing(&publisher.store, &publisher.settings.state_dir)?;
        Ok(publisher)
    }
    fn request(&mut self, value: &Value) -> Result<Value> {
        ensure!(
            value["version"] == 2
                && value["namespace_id"] == self.settings.namespace_id
                && value["session_id"] == self.settings.session_id
                && value["assignment_id"] == self.settings.assignment_id
                && value["generation"] == self.settings.generation,
            "ERR_CEDEGRID_STALE_ATTEMPT: publication identity mismatch"
        );
        let request_id = id(value, "request_id")?;
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(value)?));
        let old: Option<(String, Option<String>)> = self.store.connection.query_row("SELECT digest,response FROM local_publication_requests WHERE assignment_id=?1 AND request_id=?2", params![self.settings.assignment_id,request_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((old_digest, response)) = old {
            ensure!(
                old_digest == digest,
                "ERR_CEDEGRID_CONFLICT: request ID reused with different content"
            );
            if let Some(response) = response {
                return Ok(serde_json::from_str(&response)?);
            }
        } else {
            self.store.connection.execute("INSERT INTO local_publication_requests(assignment_id,request_id,digest) VALUES (?1,?2,?3)", params![self.settings.assignment_id,request_id,digest])?;
        }
        let response = match value["op"].as_str() {
            Some("artifact_begin") => self.begin(value)?,
            Some("artifact_chunk") => self.chunk(value)?,
            Some("artifact_finish") => self.finish(value)?,
            Some("publication_commit") => self.commit(value)?,
            Some("publication_status") => self.reconcile(id(value, "publication_id")?)?,
            Some("publication_abort") => self.abort(value)?,
            _ => bail!("ERR_CEDEGRID_ARGUMENT: unknown publication operation"),
        };
        // Status is live. Its request-ID binding is retained but its result is not cached.
        if value["op"] != "publication_status" {
            self.store.connection.execute("UPDATE local_publication_requests SET response=?3 WHERE assignment_id=?1 AND request_id=?2", params![self.settings.assignment_id,request_id,serde_json::to_string(&response)?])?;
        }
        Ok(response)
    }
    fn begin(&mut self, value: &Value) -> Result<Value> {
        let request_id = id(value, "request_id")?;
        let name = value["name"]
            .as_str()
            .context("ERR_CEDEGRID_ARGUMENT: artifact name required")?;
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && !matches!(name, "." | "..")
                && !name.contains(['/', '\\', '\0']),
            "ERR_CEDEGRID_ARGUMENT: invalid artifact name"
        );
        let size = value["size"]
            .as_u64()
            .context("ERR_CEDEGRID_ARGUMENT: artifact size must be u64")?;
        ensure!(
            size <= self.settings.max_artifact_bytes && size <= i64::MAX as u64,
            "ERR_CEDEGRID_ARGUMENT: artifact too large"
        );
        if let Some((upload,offset,path)) = self.store.connection.query_row("SELECT upload_id,offset,path FROM local_uploads WHERE assignment_id=?1 AND begin_request=?2", params![self.settings.assignment_id,request_id], |r| Ok((r.get::<_,String>(0)?,row_u64(r,1)?,r.get::<_,String>(2)?))).optional()? {
            if offset == 0 && !Path::new(&path).try_exists()? { create_new(Path::new(&path))?.sync_all()?; sync_parent(Path::new(&path))?; }
            return Ok(json!({"ok":true,"upload_id":upload,"offset":offset}));
        }
        let upload = uuid::Uuid::new_v4().to_string();
        let path = self.settings.output_dir.join("staging").join(&upload);
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        reserve(
            &tx,
            &path,
            size,
            self.settings.max_spool_bytes,
            &self.settings.assignment_id,
        )?;
        tx.execute("INSERT INTO local_uploads(upload_id,assignment_id,begin_request,name,path,size,offset,sealed) VALUES (?1,?2,?3,?4,?5,?6,0,0)", params![upload,self.settings.assignment_id,request_id,name,path.to_str().context("non-UTF8 path")?,db(size)?])?;
        tx.commit()?;
        // A failed create keeps its reservation; retrying this ID will reconcile it.
        create_new(&path)?.sync_all()?;
        sync_parent(&path)?;
        Ok(json!({"ok":true,"upload_id":upload,"offset":0}))
    }
    fn upload(&self, upload: &str) -> Result<(PathBuf, u64, u64, bool, String)> {
        self.store.connection.query_row("SELECT path,size,offset,sealed,name FROM local_uploads WHERE upload_id=?1 AND assignment_id=?2", params![upload,self.settings.assignment_id], |r| Ok((PathBuf::from(r.get::<_,String>(0)?),row_u64(r,1)?,row_u64(r,2)?,r.get(3)?,r.get(4)?))).context("ERR_CEDEGRID_ARGUMENT: unknown artifact upload")
    }
    fn chunk(&mut self, value: &Value) -> Result<Value> {
        let upload = id(value, "upload_id")?;
        let offset = value["offset"]
            .as_u64()
            .context("ERR_CEDEGRID_ARGUMENT: offset must be u64")?;
        let encoded = value["data_hex"]
            .as_str()
            .context("ERR_CEDEGRID_ARGUMENT: chunk data missing")?;
        ensure!(
            !encoded.is_empty() && encoded.len() <= LOCAL_CHUNK_LIMIT * 2,
            "ERR_CEDEGRID_ARGUMENT: chunk exceeds 256 KiB"
        );
        let data = hex::decode(encoded)?;
        let (path, size, current, sealed, _) = self.upload(upload)?;
        let end = offset
            .checked_add(data.len() as u64)
            .context("chunk offset overflow")?;
        ensure!(
            offset <= current && end <= size,
            "ERR_CEDEGRID_ARGUMENT: nonmonotonic or oversized chunk"
        );
        let mut file = open_regular(&path, true)?;
        file.seek(SeekFrom::Start(offset))?;
        if offset < current || sealed {
            ensure!(
                end <= current,
                "ERR_CEDEGRID_CONFLICT: overlapping artifact chunk"
            );
            let mut existing = vec![0; data.len()];
            file.read_exact(&mut existing)?;
            ensure!(
                existing == data,
                "ERR_CEDEGRID_CONFLICT: artifact offset has different bytes"
            );
        } else {
            file.write_all(&data)?;
            file.sync_data()?;
            self.store.connection.execute(
                "UPDATE local_uploads SET offset=?2 WHERE upload_id=?1",
                params![upload, db(end)?],
            )?;
        }
        Ok(json!({"ok":true,"upload_id":upload,"offset":end}))
    }
    fn finish(&mut self, value: &Value) -> Result<Value> {
        let upload = id(value, "upload_id")?;
        let (path, size, offset, sealed, name) = self.upload(upload)?;
        ensure!(
            size == offset,
            "ERR_CEDEGRID_ARGUMENT: artifact is incomplete"
        );
        if sealed {
            let hash: String = self.store.connection.query_row(
                "SELECT sha256 FROM local_uploads WHERE upload_id=?1",
                [upload],
                |r| r.get(0),
            )?;
            return Ok(
                json!({"ok":true,"artifact_id":upload,"name":name,"sha256":hash,"size":size}),
            );
        }
        let destination = self.settings.output_dir.join("artifacts").join(upload);
        let mut file = match open_regular(&path, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                open_regular(&destination, false)?
            }
            Err(error) => return Err(error),
        };
        ensure!(file.metadata()?.len() == size, "artifact size changed");
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        file.sync_all()?;
        let hash = hex::encode(hash.finalize());
        // link is atomic and no-clobber; uncertainty retains both names/charge.
        if !destination.try_exists()? {
            fs::hard_link(&path, &destination)?;
        }
        let sealed_file = open_regular(&destination, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = file.metadata()?;
            let sealed = sealed_file.metadata()?;
            ensure!(
                original.dev() == sealed.dev() && original.ino() == sealed.ino(),
                "ERR_CEDEGRID_CONFLICT: sealed artifact entry was replaced"
            );
        }
        sealed_file.sync_all()?;
        sync_parent(&destination)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_parent(&path)?;
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE local_uploads SET path=?2,sha256=?3,sealed=1 WHERE upload_id=?1",
            params![upload, destination.to_str().unwrap(), hash],
        )?;
        tx.execute(
            "DELETE FROM local_spool_entries WHERE path=?1",
            [path.to_str().unwrap()],
        )?;
        tx.execute("INSERT INTO local_spool_entries(path,bytes,assignment_id) VALUES (?1,?2,?3) ON CONFLICT(path) DO UPDATE SET bytes=MAX(bytes,excluded.bytes)",params![destination.to_str().unwrap(),db(size)?,self.settings.assignment_id])?;
        tx.commit()?;
        Ok(json!({"ok":true,"artifact_id":upload,"name":name,"sha256":hash,"size":size}))
    }
    fn commit(&mut self, value: &Value) -> Result<Value> {
        let publication = id(value, "publication_id")?;
        let kind = value["kind"].as_str().context("publication kind missing")?;
        ensure!(
            matches!(kind, "checkpoint" | "result"),
            "ERR_CEDEGRID_ARGUMENT: invalid publication kind"
        );
        let payload_hash = hex::encode(Sha256::digest(serde_json::to_vec(
            &json!({"kind":kind,"metadata":value["metadata"],"artifact_ids":value["artifact_ids"]}),
        )?));
        let existing: Option<String> = self.store.connection.query_row("SELECT payload_hash FROM local_publications WHERE assignment_id=?1 AND publication_id=?2", params![self.settings.assignment_id,publication], |r| r.get(0)).optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing == payload_hash,
                "ERR_CEDEGRID_CONFLICT: publication ID reused"
            );
            return self.reconcile(publication);
        }
        let ids = value["artifact_ids"]
            .as_array()
            .context("artifact_ids must be an array")?;
        ensure!(
            ids.len() <= 64,
            "ERR_CEDEGRID_ARGUMENT: too many artifact references"
        );
        let mut artifacts = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for id_value in ids {
            let artifact_id = id_value.as_str().context("artifact ID must be a string")?;
            let (path, size, _, sealed, name) = self.upload(artifact_id)?;
            ensure!(
                sealed && names.insert(name.clone()),
                "ERR_CEDEGRID_ARGUMENT: unsealed or duplicate named artifact"
            );
            let sha: String = self.store.connection.query_row(
                "SELECT sha256 FROM local_uploads WHERE upload_id=?1",
                [artifact_id],
                |r| r.get(0),
            )?;
            artifacts.push(json!({"name":name,"artifact_id":artifact_id,"path":path.strip_prefix(&self.settings.output_dir)?.to_str().unwrap(),"size":size,"sha256":sha}));
        }
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_publications WHERE assignment_id=?1 AND state IN ('pending','uncertain'))", [&self.settings.assignment_id], |r| r.get(0))?;
        ensure!(
            !unresolved,
            "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: reconcile the original pending publication before creating another"
        );
        let sequence: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM local_publications WHERE assignment_id=?1",
            [&self.settings.assignment_id],
            |r| r.get(0),
        )?;
        ensure!(
            sequence > 0 && sequence < i64::MAX,
            "ERR_CEDEGRID_ARGUMENT: checkpoint sequence exhausted"
        );
        if kind == "result" {
            let committed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_publication_heads WHERE assignment_id=?1 AND kind='result')", [&self.settings.assignment_id], |r| r.get(0))?;
            ensure!(
                !committed,
                "ERR_CEDEGRID_CONFLICT: final result is immutable"
            );
        }
        let descriptor = json!({"schema_version":2,"publication_id":publication,"kind":kind,"task_id":self.settings.task_id,"assignment_id":self.settings.assignment_id,"generation":self.settings.generation,"namespace_id":self.settings.namespace_id,"session_id":self.settings.session_id,"checkpoint_sequence":sequence,"metadata":value["metadata"],"artifacts":artifacts});
        let bytes = serde_json::to_vec(&descriptor)?;
        ensure!(
            bytes.len() <= DESCRIPTOR_LIMIT,
            "ERR_CEDEGRID_DESCRIPTOR_LIMIT: complete descriptor exceeds 1 MiB"
        );
        let sha = hex::encode(Sha256::digest(&bytes));
        let path = self
            .settings
            .output_dir
            .join("publications")
            .join(format!("{publication}.json"));
        reserve(
            &tx,
            &path,
            bytes.len() as u64,
            self.settings.max_spool_bytes,
            &self.settings.assignment_id,
        )?;
        tx.execute("INSERT INTO local_publications(assignment_id,publication_id,kind,sequence,payload_hash,path,sha256,state) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending')", params![self.settings.assignment_id,publication,kind,sequence,payload_hash,path.to_str().unwrap(),sha])?;
        tx.commit()?;
        let durable = (|| -> Result<()> {
            let mut file = create_new(&path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(test)]
            if std::mem::take(&mut self.fail_after_insert) {
                bail!("injected publication directory sync failure");
            }
            sync_parent(&path)?;
            Ok(())
        })();
        if let Err(e) = durable {
            let _ = self.store.connection.execute("UPDATE local_publications SET state='uncertain' WHERE assignment_id=?1 AND publication_id=?2", params![self.settings.assignment_id,publication]);
            bail!("ERR_CEDEGRID_PUBLICATION_UNCERTAIN: {publication} candidate {sha}: {e:#}");
        }
        let commit = (|| -> Result<()> {
            let tx =
                Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
            let head: Option<i64> = tx.query_row("SELECT sequence FROM local_publication_heads WHERE assignment_id=?1 AND kind=?2", params![self.settings.assignment_id,kind], |r| r.get(0)).optional()?;
            ensure!(
                head.is_none_or(|s| s < sequence),
                "ERR_CEDEGRID_CONFLICT: stale publication cannot rewind head"
            );
            tx.execute("INSERT INTO local_publication_heads(assignment_id,kind,publication_id,sequence) VALUES (?1,?2,?3,?4) ON CONFLICT(assignment_id,kind) DO UPDATE SET publication_id=excluded.publication_id,sequence=excluded.sequence", params![self.settings.assignment_id,kind,publication,sequence])?;
            tx.execute("UPDATE local_publications SET state='committed' WHERE assignment_id=?1 AND publication_id=?2", params![self.settings.assignment_id,publication])?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(e) = commit {
            bail!("ERR_CEDEGRID_PUBLICATION_UNCERTAIN: {publication} candidate {sha}: {e:#}");
        }
        self.status(publication)
    }
    fn reconcile(&mut self, publication: &str) -> Result<Value> {
        let state = self.status(publication)?;
        if matches!(state["state"].as_str(), Some("committed" | "rejected")) {
            return Ok(state);
        }
        let path: String = self.store.connection.query_row(
            "SELECT path FROM local_publications WHERE assignment_id=?1 AND publication_id=?2",
            params![self.settings.assignment_id, publication],
            |r| r.get(0),
        )?;
        let path = Path::new(&path);
        let bytes = match read_bounded(path, DESCRIPTOR_LIMIT) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                sync_parent(path)?;
                let tx = Transaction::new_unchecked(
                    &self.store.connection,
                    TransactionBehavior::Immediate,
                )?;
                let selected:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_publication_heads WHERE assignment_id=?1 AND publication_id=?2)",params![self.settings.assignment_id,publication],|r|r.get(0))?;
                ensure!(
                    !selected,
                    "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: selected candidate is missing"
                );
                tx.execute("UPDATE local_publications SET state='rejected' WHERE assignment_id=?1 AND publication_id=?2",params![self.settings.assignment_id,publication])?;
                tx.execute(
                    "DELETE FROM local_spool_entries WHERE path=?1",
                    [path.to_str().unwrap()],
                )?;
                tx.commit()?;
                return self.status(publication);
            }
            Err(_) => return Ok(state),
        };
        if hex::encode(Sha256::digest(&bytes)) != state["sha256"].as_str().unwrap() {
            return Ok(state);
        }
        let descriptor: Value = crate::numeric::from_slice(&bytes)?;
        ensure!(
            descriptor["publication_id"] == publication
                && descriptor["assignment_id"] == self.settings.assignment_id
                && descriptor["generation"] == self.settings.generation,
            "ERR_CEDEGRID_STALE_ATTEMPT: recovery candidate identity mismatch"
        );
        // Re-establish the exact candidate's file and directory barriers before
        // deciding its original operation. No new ID or sequence is allocated.
        if open_regular(path, false)?.sync_all().is_err() || sync_parent(path).is_err() {
            return Ok(state);
        }
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let kind = state["kind"].as_str().unwrap();
        let sequence = state["sequence"].as_i64().unwrap();
        let head:Option<(i64,String)>=tx.query_row("SELECT sequence,publication_id FROM local_publication_heads WHERE assignment_id=?1 AND kind=?2",params![self.settings.assignment_id,kind],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if head
            .as_ref()
            .is_some_and(|(seq, id)| *seq >= sequence && id != publication)
        {
            tx.execute("UPDATE local_publications SET state='rejected' WHERE assignment_id=?1 AND publication_id=?2",params![self.settings.assignment_id,publication])?;
        } else {
            tx.execute("INSERT INTO local_publication_heads(assignment_id,kind,publication_id,sequence) VALUES (?1,?2,?3,?4) ON CONFLICT(assignment_id,kind) DO UPDATE SET publication_id=excluded.publication_id,sequence=excluded.sequence",params![self.settings.assignment_id,kind,publication,sequence])?;
            tx.execute("UPDATE local_publications SET state='committed' WHERE assignment_id=?1 AND publication_id=?2",params![self.settings.assignment_id,publication])?;
        }
        tx.commit().context(
            "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: reconciliation commit was not confirmed",
        )?;
        self.status(publication)
    }
    fn status(&self, publication: &str) -> Result<Value> {
        let record: Option<(String,i64,String,String)> = self.store.connection.query_row("SELECT kind,sequence,sha256,state FROM local_publications WHERE assignment_id=?1 AND publication_id=?2", params![self.settings.assignment_id,publication], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let (kind, sequence, sha, state) =
            record.context("ERR_CEDEGRID_ARGUMENT: unknown publication ID")?;
        Ok(
            json!({"ok":true,"publication_id":publication,"kind":kind,"sequence":sequence,"sha256":sha,"state":state,"assurance":self.settings.storage_profile.assurance(),"assignment_id":self.settings.assignment_id,"generation":self.settings.generation,"namespace_id":self.settings.namespace_id,"session_id":self.settings.session_id}),
        )
    }
    fn abort(&mut self, value: &Value) -> Result<Value> {
        let publication = id(value, "publication_id")?;
        let state = self.status(publication)?;
        ensure!(
            state["state"] == "rejected",
            "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: abort requires a proven uncommitted publication"
        );
        Ok(state)
    }
}
fn account_existing(store: &StateStore, state_dir: &Path) -> Result<()> {
    // Reconcile physical bytes upward only. Both the first input download and
    // later publishers retain every quarantine charge before reserving bytes.
    let mut files = Vec::new();
    collect_files(state_dir, &mut files, true)?;
    let namespace = crate::namespace::Namespace::new(state_dir)?;
    collect_files(namespace.control_dir(), &mut files, false)?;
    let leaf = state_dir
        .file_name()
        .context("state leaf missing")?
        .to_string_lossy();
    let prefix = format!(".{leaf}.quarantine-");
    for entry in fs::read_dir(state_dir.parent().context("state parent missing")?)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            ensure!(
                entry.file_type()?.is_dir(),
                "quarantine must be a real directory"
            );
            collect_files(&entry.path(), &mut files, true)?;
        }
    }
    let tx = Transaction::new_unchecked(&store.connection, TransactionBehavior::Immediate)?;
    for (path, size) in files {
        tx.execute("INSERT INTO local_spool_entries(path,bytes,assignment_id) VALUES (?1,?2,'') ON CONFLICT(path) DO UPDATE SET bytes=MAX(bytes,excluded.bytes)", params![path.to_str().context("non-UTF8 spool path")?,db(size)?])?;
    }
    tx.commit()?;
    Ok(())
}
fn initialize(store: &StateStore) -> Result<()> {
    store.connection.execute_batch("CREATE TABLE IF NOT EXISTS local_spool_entries(path TEXT PRIMARY KEY,bytes INTEGER NOT NULL CHECK(bytes>=0),assignment_id TEXT NOT NULL) STRICT;
      CREATE TABLE IF NOT EXISTS local_publication_requests(assignment_id TEXT NOT NULL,request_id TEXT NOT NULL,digest TEXT NOT NULL,response TEXT,PRIMARY KEY(assignment_id,request_id)) STRICT;
      CREATE TABLE IF NOT EXISTS local_uploads(upload_id TEXT PRIMARY KEY,assignment_id TEXT NOT NULL,begin_request TEXT NOT NULL,name TEXT NOT NULL,path TEXT NOT NULL,size INTEGER NOT NULL,offset INTEGER NOT NULL,sealed INTEGER NOT NULL,sha256 TEXT,UNIQUE(assignment_id,begin_request)) STRICT;
      CREATE TABLE IF NOT EXISTS local_publications(assignment_id TEXT NOT NULL,publication_id TEXT NOT NULL,kind TEXT NOT NULL,sequence INTEGER NOT NULL,payload_hash TEXT NOT NULL,path TEXT NOT NULL,sha256 TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(assignment_id,publication_id),UNIQUE(assignment_id,sequence)) STRICT;
      CREATE TABLE IF NOT EXISTS local_publication_heads(assignment_id TEXT NOT NULL,kind TEXT NOT NULL,publication_id TEXT NOT NULL,sequence INTEGER NOT NULL,PRIMARY KEY(assignment_id,kind)) STRICT;")?;
    Ok(())
}
fn reserve(
    tx: &Transaction<'_>,
    path: &Path,
    bytes: u64,
    quota: u64,
    assignment: &str,
) -> Result<()> {
    let total: i64 = tx.query_row(
        "SELECT COALESCE(SUM(bytes),0) FROM local_spool_entries",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        bytes <= i64::MAX as u64
            && (total as u64)
                .checked_add(bytes)
                .is_some_and(|n| n <= quota),
        "ERR_CEDEGRID_SPOOL_QUOTA: node-wide quota exhausted"
    );
    tx.execute(
        "INSERT INTO local_spool_entries(path,bytes,assignment_id) VALUES (?1,?2,?3)",
        params![
            path.to_str().context("non-UTF8 spool path")?,
            db(bytes)?,
            assignment
        ],
    )?;
    Ok(())
}
fn id<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    let id = value[field]
        .as_str()
        .with_context(|| format!("ERR_CEDEGRID_ARGUMENT: missing {field}"))?;
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && !matches!(id, "." | ".."),
        "ERR_CEDEGRID_ARGUMENT: invalid {field}"
    );
    Ok(id)
}
pub fn read_head(store: &StateStore, output: &Path, kind: &str) -> Result<Option<Vec<u8>>> {
    let exists: bool = store.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_publication_heads')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let assignment = output
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid attempt output")?;
    let row: Option<(String,String)> = store.connection.query_row("SELECT p.path,p.sha256 FROM local_publication_heads h JOIN local_publications p ON p.assignment_id=h.assignment_id AND p.publication_id=h.publication_id WHERE h.assignment_id=?1 AND h.kind=?2 AND p.state='committed'", params![assignment,kind], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if row.is_none() {
        let pending:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM local_publications WHERE assignment_id=?1 AND kind=?2 AND state IN ('pending','uncertain'))",params![assignment,kind],|r|r.get(0))?;
        ensure!(
            !pending,
            "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: retained candidate requires reconciliation; plain-command fallback is forbidden"
        );
    }
    row.map(|(path, hash)| {
        let bytes = read_bounded(Path::new(&path), DESCRIPTOR_LIMIT)?;
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == hash,
            "committed publication hash mismatch"
        );
        let _: Value = crate::numeric::from_slice(&bytes)?;
        Ok(bytes)
    })
    .transpose()
}
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = open_regular(path, false)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "ERR_CEDEGRID_DESCRIPTOR_LIMIT: file exceeds byte limit"
    );
    Ok(bytes)
}
#[cfg(unix)]
pub fn open_regular(path: &Path, write: bool) -> Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = File::open("/")?;
    let components: Vec<_> = absolute
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
        .collect();
    ensure!(!components.is_empty(), "file path has no leaf");
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            bail!("file path contains traversal")
        };
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(name.as_bytes())?;
        let last = index + 1 == components.len();
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if last {
                if write { libc::O_RDWR } else { libc::O_RDONLY }
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if last {
            ensure!(
                directory.metadata()?.is_file(),
                "ERR_CEDEGRID_FILE_TYPE: regular file required"
            );
        }
    }
    Ok(directory)
}
#[cfg(not(unix))]
pub fn open_regular(_path: &Path, _write: bool) -> Result<File> {
    bail!("ERR_CEDEGRID_UNSUPPORTED_PLATFORM: safe local file backend unavailable")
}
fn create_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    Ok(options.open(path)?)
}
fn private_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid publication directory"
        );
        return Ok(());
    }
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    sync_parent(path)
}
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}
fn collect_files(
    root: &Path,
    output: &mut Vec<(PathBuf, u64)>,
    exempt_root_database: bool,
) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(!metadata.file_type().is_symlink(), "spool symlink refused");
        if metadata.is_dir() {
            collect_files(&path, output, false)?;
        } else if metadata.is_file()
            && !(exempt_root_database
                && [
                    "state.sqlite3",
                    "state.sqlite3-wal",
                    "state.sqlite3-shm",
                    "state.sqlite3-journal",
                ]
                .iter()
                .any(|name| entry.file_name() == std::ffi::OsStr::new(name)))
        {
            output.push((path, metadata.len()));
        } else {
            ensure!(metadata.is_file(), "nonregular spool entry refused");
        }
        ensure!(output.len() <= 1_000_000, "spool reconciliation file limit");
    }
    Ok(())
}

fn db(value: u64) -> Result<i64> {
    i64::try_from(value).context("ERR_CEDEGRID_ARGUMENT: persisted integer overflow")
}
fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}

/// Capture threads reserve each bounded write before touching output bytes.
/// They never hold a transaction while waiting for a workload pipe.
pub struct CaptureCharge {
    store: StateStore,
    path: PathBuf,
    quota: u64,
    assignment: String,
}
impl CaptureCharge {
    pub fn for_download(
        state_dir: &Path,
        profile: StorageProfile,
        guard: NamespaceGuard,
        path: &Path,
        quota: u64,
    ) -> Result<Self> {
        let store = StateStore::open_existing_with_profile_guarded(state_dir, profile, guard)?;
        initialize(&store)?;
        account_existing(&store, state_dir)?;
        Ok(Self {
            store,
            path: path.to_owned(),
            quota,
            assignment: String::new(),
        })
    }
    pub fn reclaim_removed(&self) -> Result<()> {
        ensure!(
            !self.path.try_exists()?,
            "cannot release quota for an existing file"
        );
        sync_parent(&self.path)?;
        self.store.connection.execute(
            "DELETE FROM local_spool_entries WHERE path=?1",
            [self.path.to_str().context("non-UTF8 spool path")?],
        )?;
        Ok(())
    }
    pub fn new(
        settings: &NativePublisherSettings,
        guard: NamespaceGuard,
        path: &Path,
    ) -> Result<Self> {
        let store = StateStore::open_existing_with_profile_guarded(
            &settings.state_dir,
            settings.storage_profile,
            guard,
        )?;
        initialize(&store)?;
        Ok(Self {
            store,
            path: path.to_owned(),
            quota: settings.max_spool_bytes,
            assignment: settings.assignment_id.clone(),
        })
    }
    pub fn reserve(&self, bytes: u64) -> Result<()> {
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let total: i64 = tx.query_row(
            "SELECT COALESCE(SUM(bytes),0) FROM local_spool_entries",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            (total as u64)
                .checked_add(bytes)
                .is_some_and(|n| n <= self.quota),
            "ERR_CEDEGRID_SPOOL_QUOTA: output capture exceeds node-wide quota"
        );
        tx.execute("INSERT INTO local_spool_entries(path,bytes,assignment_id) VALUES (?1,?2,?3) ON CONFLICT(path) DO UPDATE SET bytes=bytes+excluded.bytes",params![self.path.to_str().context("non-UTF8 capture path")?,db(bytes)?,self.assignment])?;
        tx.commit()?;
        Ok(())
    }
}

// These fixtures require the Unix-protected local execution namespace.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn publisher() -> (tempfile::TempDir, NativePublisher) {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path().join("state");
        let store = StateStore::open(&root).unwrap();
        let guard = store.namespace_guard();
        let output = root.join("attempts").join("assignment");
        fs::create_dir_all(&output).unwrap();
        let settings = NativePublisherSettings {
            state_dir: root,
            storage_profile: StorageProfile::WalFull,
            namespace_id: guard.identity().namespace_id.clone(),
            session_id: guard.identity().namespace_id.clone(),
            output_dir: output,
            task_id: "task".into(),
            assignment_id: "assignment".into(),
            generation: 1,
            max_spool_bytes: 64 * 1024 * 1024,
            max_artifact_bytes: 256 * 1024 * 1024,
        };
        (temp, NativePublisher::open(settings, guard).unwrap())
    }
    fn request(p: &mut NativePublisher, mut value: Value) -> Result<Value> {
        value["version"] = json!(2);
        value["request_id"] = json!(uuid::Uuid::new_v4().to_string());
        value["namespace_id"] = json!(p.settings.namespace_id);
        value["session_id"] = json!(p.settings.session_id);
        value["assignment_id"] = json!(p.settings.assignment_id);
        value["generation"] = json!(1);
        p.request(&value)
    }
    #[test]
    fn first_input_reservation_counts_retained_quarantine_without_a_publisher() {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path().join("state");
        let store = StateStore::open(&root).unwrap();
        let guard = store.namespace_guard();
        let input = root.join("attempts/new/input");
        fs::create_dir_all(input.parent().unwrap()).unwrap();
        let mut baseline = Vec::new();
        collect_files(&root, &mut baseline, true).unwrap();
        collect_files(
            crate::namespace::Namespace::new(&root)
                .unwrap()
                .control_dir(),
            &mut baseline,
            false,
        )
        .unwrap();
        let baseline_bytes: u64 = baseline.iter().map(|(_, bytes)| *bytes).sum();
        let retained = temp
            .path()
            .join(".state.quarantine-old/attempts/old/artifacts/payload");
        fs::create_dir_all(retained.parent().unwrap()).unwrap();
        fs::write(&retained, [0u8; 4096]).unwrap();
        assert!(
            !store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='local_publications')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        let charge = CaptureCharge::for_download(
            &root,
            StorageProfile::WalFull,
            guard,
            &input,
            baseline_bytes + 4096,
        )
        .unwrap();
        assert!(format!("{:#}", charge.reserve(1).unwrap_err()).contains("SPOOL_QUOTA"));
        assert!(!input.exists());
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT bytes FROM local_spool_entries WHERE path=?1",
                    [retained.to_str().unwrap()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            4096
        );
        assert_eq!(fs::metadata(retained).unwrap().len(), 4096);
    }
    #[test]
    fn only_exact_database_names_at_state_roots_are_exempt_from_spool_accounting() {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path().join("state");
        let store = StateStore::open(&root).unwrap();
        let quarantine = temp.path().join(".state.quarantine-old");
        fs::create_dir(&quarantine).unwrap();
        let charged = [
            root.join("state.sqlite3.backup"),
            root.join("attempts/workspace/state.sqlite3"),
            root.join("attempts/workspace/state.sqlite3-wal"),
            quarantine.join("attempts/workspace/state.sqlite3-journal"),
        ];
        for path in &charged {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"retained workspace bytes").unwrap();
        }
        for name in [
            "state.sqlite3",
            "state.sqlite3-wal",
            "state.sqlite3-shm",
            "state.sqlite3-journal",
        ] {
            fs::write(quarantine.join(name), b"separate database budget").unwrap();
        }
        CaptureCharge::for_download(
            &root,
            StorageProfile::WalFull,
            store.namespace_guard(),
            &root.join("attempts/new/input"),
            1024 * 1024,
        )
        .unwrap();
        for path in charged {
            let bytes: i64 = store
                .connection
                .query_row(
                    "SELECT bytes FROM local_spool_entries WHERE path=?1",
                    [path.to_str().unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(bytes, 24, "{} was not charged", path.display());
        }
        for directory in [&root, &quarantine] {
            for name in [
                "state.sqlite3",
                "state.sqlite3-wal",
                "state.sqlite3-shm",
                "state.sqlite3-journal",
            ] {
                assert!(
                    !store
                        .connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM local_spool_entries WHERE path=?1)",
                            [directory.join(name).to_str().unwrap()],
                            |row| row.get::<_, bool>(0),
                        )
                        .unwrap()
                );
            }
        }
    }
    #[test]
    fn immutable_checkpoint_final_byte_limit_and_idempotent_retries() {
        let (_temp, mut p) = publisher();
        let first=request(&mut p,json!({"op":"publication_commit","publication_id":"first","kind":"checkpoint","metadata":{"n":9007199254740993u64},"artifact_ids":[]})).unwrap();
        assert_eq!(first["state"], "committed");
        let before = read_head(&p.store, &p.settings.output_dir, "checkpoint")
            .unwrap()
            .unwrap();
        let failure=request(&mut p,json!({"op":"publication_commit","publication_id":"oversized","kind":"checkpoint","metadata":{"text":"世".repeat(DESCRIPTOR_LIMIT/3)},"artifact_ids":[]})).unwrap_err();
        assert!(format!("{failure:#}").contains("DESCRIPTOR_LIMIT"));
        assert_eq!(
            read_head(&p.store, &p.settings.output_dir, "checkpoint")
                .unwrap()
                .unwrap(),
            before
        );
        let repeated=request(&mut p,json!({"op":"publication_commit","publication_id":"first","kind":"checkpoint","metadata":{"n":9007199254740993u64},"artifact_ids":[]})).unwrap();
        assert_eq!(first, repeated);
        assert!(request(&mut p,json!({"op":"publication_commit","publication_id":"first","kind":"checkpoint","metadata":{"n":2},"artifact_ids":[]})).is_err());
    }
    #[test]
    fn namespace_sync_failure_preserves_predecessor_and_resolves_original_id() {
        let (_temp, mut p) = publisher();
        request(&mut p,json!({"op":"publication_commit","publication_id":"old","kind":"checkpoint","metadata":{"step":1},"artifact_ids":[]})).unwrap();
        let old = read_head(&p.store, &p.settings.output_dir, "checkpoint")
            .unwrap()
            .unwrap();
        p.fail_after_insert = true;
        let error=request(&mut p,json!({"op":"publication_commit","publication_id":"uncertain","kind":"checkpoint","metadata":{"step":2},"artifact_ids":[]})).unwrap_err();
        assert!(format!("{error:#}").contains("PUBLICATION_UNCERTAIN"));
        assert_eq!(
            read_head(&p.store, &p.settings.output_dir, "checkpoint")
                .unwrap()
                .unwrap(),
            old
        );
        assert!(
            p.settings
                .output_dir
                .join("publications/old.json")
                .is_file()
        );
        assert!(
            p.settings
                .output_dir
                .join("publications/uncertain.json")
                .is_file()
        );
        let outcome = request(
            &mut p,
            json!({"op":"publication_status","publication_id":"uncertain"}),
        )
        .unwrap();
        assert_eq!(outcome["state"], "committed");
        assert_eq!(outcome["publication_id"], "uncertain");
        assert_eq!(outcome["sequence"], 2);
        let new: Value = serde_json::from_slice(
            &read_head(&p.store, &p.settings.output_dir, "checkpoint")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(new["metadata"]["step"], 2);
        assert!(
            p.settings
                .output_dir
                .join("publications/old.json")
                .is_file()
        );
    }
    #[test]
    fn uncertain_final_candidate_cannot_be_reported_as_plain_command_success() {
        let (_temp, mut p) = publisher();
        p.fail_after_insert = true;
        assert!(request(&mut p,json!({"op":"publication_commit","publication_id":"final","kind":"result","metadata":{},"artifact_ids":[]})).is_err());
        assert!(read_head(&p.store, &p.settings.output_dir, "result").is_err());
        assert_eq!(
            request(
                &mut p,
                json!({"op":"publication_status","publication_id":"final"})
            )
            .unwrap()["state"],
            "committed"
        );
    }
    #[test]
    fn stream_artifact_offset_replay_and_final_immutable_bytes() {
        let (_temp, mut p) = publisher();
        let begin = request(
            &mut p,
            json!({"op":"artifact_begin","name":"test.bin","size":3}),
        )
        .unwrap();
        let upload = &begin["upload_id"];
        for _ in 0..2 {
            request(
                &mut p,
                json!({"op":"artifact_chunk","upload_id":upload,"offset":0,"data_hex":"616263"}),
            )
            .unwrap();
        }
        assert!(
            request(
                &mut p,
                json!({"op":"artifact_chunk","upload_id":upload,"offset":0,"data_hex":"646566"})
            )
            .is_err()
        );
        let sealed = request(&mut p, json!({"op":"artifact_finish","upload_id":upload})).unwrap();
        assert_eq!(sealed["sha256"], hex::encode(Sha256::digest(b"abc")));
        request(&mut p,json!({"op":"publication_commit","publication_id":"result","kind":"result","metadata":{},"artifact_ids":[upload]})).unwrap();
        let descriptor: Value = serde_json::from_slice(
            &read_head(&p.store, &p.settings.output_dir, "result")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(descriptor["artifacts"][0]["name"], "test.bin");
        assert!(request(&mut p,json!({"op":"publication_commit","publication_id":"different","kind":"result","metadata":{},"artifact_ids":[]})).is_err());
    }
    #[test]
    fn node_quota_accounts_concurrent_publishers_and_retains_predecessor() {
        let (_temp, mut p) = publisher();
        p.settings.max_spool_bytes = 10 * 1024 * 1024;
        request(
            &mut p,
            json!({"op":"artifact_begin","name":"one","size":9*1024*1024}),
        )
        .unwrap();
        let mut other =
            NativePublisher::open(p.settings.clone(), p.store.namespace_guard()).unwrap();
        assert!(
            request(
                &mut other,
                json!({"op":"artifact_begin","name":"two","size":2*1024*1024})
            )
            .is_err()
        );
    }
    #[test]
    fn interrupted_artifact_seal_reuses_immutable_bytes_and_original_quota() {
        let (_temp, mut p) = publisher();
        let begun = request(
            &mut p,
            json!({"op":"artifact_begin","name":"crash.bin","size":3}),
        )
        .unwrap();
        let upload = begun["upload_id"].as_str().unwrap();
        request(
            &mut p,
            json!({"op":"artifact_chunk","upload_id":upload,"offset":0,"data_hex":"616263"}),
        )
        .unwrap();
        let (stage, _, _, _, _) = p.upload(upload).unwrap();
        let sealed = p.settings.output_dir.join("artifacts").join(upload);
        fs::hard_link(&stage, &sealed).unwrap();
        sync_parent(&sealed).unwrap();
        fs::remove_file(&stage).unwrap();
        sync_parent(&stage).unwrap();
        // Reopen at the exact crash gap: namespace changed, SQLite seal not committed.
        let mut recovered =
            NativePublisher::open(p.settings.clone(), p.store.namespace_guard()).unwrap();
        let result = request(
            &mut recovered,
            json!({"op":"artifact_finish","upload_id":upload}),
        )
        .unwrap();
        assert_eq!(result["sha256"], hex::encode(Sha256::digest(b"abc")));
        assert_eq!(read_bounded(&sealed, 3).unwrap(), b"abc");
        let charges: i64 = recovered
            .store
            .connection
            .query_row(
                "SELECT SUM(bytes) FROM local_spool_entries WHERE path=?1 OR path=?2",
                params![stage.to_str().unwrap(), sealed.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(charges, 3);
    }
    #[test]
    fn interrupted_artifact_begin_recovers_reserved_empty_stage() {
        let (_temp, mut p) = publisher();
        let value = json!({"request_id":"begin-original","name":"empty.bin","size":3});
        let first = p.begin(&value).unwrap();
        let upload = first["upload_id"].as_str().unwrap();
        let (stage, _, _, _, _) = p.upload(upload).unwrap();
        fs::remove_file(&stage).unwrap();
        let retry = p.begin(&value).unwrap();
        assert_eq!(retry, first);
        assert_eq!(fs::metadata(stage).unwrap().len(), 0);
    }
    #[test]
    #[cfg(unix)]
    fn bounded_open_rejects_fifo_and_intermediate_symlink_without_blocking() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/data"), b"abcdef").unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        assert!(read_bounded(&root.join("alias/data"), 10).is_err());
        assert!(read_bounded(&root.join("real/data"), 5).is_err());
        assert_eq!(read_bounded(&root.join("real/data"), 6).unwrap(), b"abcdef");
        let fifo = root.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(read_bounded(&fifo, 100).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
