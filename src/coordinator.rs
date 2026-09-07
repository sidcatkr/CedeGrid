//! One active user-space coordinator, using the existing durable task ledger.
use crate::{
    artifacts::{ArtifactStore, CHUNK_LIMIT},
    execution_model::AllocationClass,
    model::Resources,
    protocol::*,
    state::{StateStore, TaskStatus},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Coordinator {
    store: StateStore,
    artifacts: ArtifactStore,
    config: CoordinatorConfig,
    epoch: u64,
    last_clock_ms: u64,
    _lock: crate::backup::StateLock,
}
impl Coordinator {
    pub fn open(config: CoordinatorConfig) -> Result<Self> {
        ensure!(
            !config.storage_profile.is_replayable(),
            "coordinator authority requires durable local storage; replayable local state is node-only"
        );
        ensure!(
            config.lease_ms > 0 && config.telemetry_ttl_ms > 0,
            "lease and freshness must be positive"
        );
        ensure!(
            config.retry_limit > 0
                && config.retry_backoff_ms > 0
                && config.retry_backoff_max_ms >= config.retry_backoff_ms
                && config.yield_retry_backoff_ms > 0
                && config.yield_retry_backoff_max_ms >= config.yield_retry_backoff_ms,
            "invalid retry policy"
        );
        crate::backup::ensure_runnable_state(&config.state_dir)?;
        let store = StateStore::open_with_profile(&config.state_dir, config.storage_profile)?;
        let lock_path = store
            .database_path()
            .parent()
            .unwrap()
            .join("coordinator.lock");
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = opts.open(lock_path)?;
        lock.try_lock_exclusive()
            .context("another coordinator owns this state directory")?;
        let lock = crate::backup::StateLock::from_locked(lock);
        let tx = Transaction::new_unchecked(&store.connection, TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS distributed_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS pools(pool_id TEXT PRIMARY KEY,spec_json TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS jobs(job_id TEXT PRIMARY KEY,pool_id TEXT NOT NULL REFERENCES pools(pool_id),priority INTEGER NOT NULL,cancelled INTEGER NOT NULL DEFAULT 0,submitted_ms INTEGER NOT NULL,spec_json TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS task_specs(sequence INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),job_id TEXT NOT NULL REFERENCES jobs(job_id),request_json TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS nodes(node_id TEXT PRIMARY KEY,report_json TEXT NOT NULL,received_ms INTEGER NOT NULL,drain INTEGER NOT NULL DEFAULT 0) STRICT;
 CREATE TABLE IF NOT EXISTS reservations(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),node_id TEXT NOT NULL,phase TEXT NOT NULL,resources_json TEXT NOT NULL,observed_json TEXT,lease_sequence INTEGER NOT NULL DEFAULT 0,lease_deadline_ms INTEGER NOT NULL DEFAULT 0,epoch INTEGER NOT NULL,detail TEXT NOT NULL DEFAULT '') STRICT;
 CREATE TABLE IF NOT EXISTS drain_requests(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),reason TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS preparations(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),record_json TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS unrecognized_allocations(node_id TEXT NOT NULL,assignment_id TEXT NOT NULL,generation INTEGER NOT NULL,report_json TEXT NOT NULL,PRIMARY KEY(node_id,assignment_id)) STRICT;
 CREATE TABLE IF NOT EXISTS allocation_fences(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),reason TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS replay_node_sessions(node_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,boot_id TEXT NOT NULL,ready INTEGER NOT NULL DEFAULT 0 CHECK(ready IN (0,1))) STRICT;
 CREATE TABLE IF NOT EXISTS replay_retired_sessions(node_id TEXT NOT NULL,session_id TEXT NOT NULL,PRIMARY KEY(node_id,session_id)) STRICT;
 CREATE TABLE IF NOT EXISTS retry_state(task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),failures INTEGER NOT NULL,not_before_ms INTEGER NOT NULL,yield_count INTEGER NOT NULL DEFAULT 0) STRICT;
 CREATE TABLE IF NOT EXISTS failure_receipts(assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),failure_kind TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS publications(assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),generation INTEGER NOT NULL,sha256 TEXT NOT NULL,size INTEGER NOT NULL,PRIMARY KEY(assignment_id,sha256)) STRICT;
 CREATE TABLE IF NOT EXISTS result_payloads(task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),submission_json TEXT NOT NULL) STRICT;
 CREATE TABLE IF NOT EXISTS checkpoints(task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),submission_json TEXT NOT NULL,receipt_hash TEXT NOT NULL) STRICT;
 INSERT OR IGNORE INTO distributed_meta(key,value) VALUES ('schema',1),('epoch',0),('last_clock_ms',0);")?;
        let mut columns = tx.prepare("PRAGMA table_info(retry_state)")?;
        let has_yield_count = columns
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "yield_count");
        drop(columns);
        if !has_yield_count {
            tx.execute_batch(
                "ALTER TABLE retry_state ADD COLUMN yield_count INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        let schema: i64 = tx.query_row(
            "SELECT value FROM distributed_meta WHERE key='schema'",
            [],
            |r| r.get(0),
        )?;
        ensure!(matches!(schema, 1 | 2), "unsupported distributed schema");
        tx.execute(
            "UPDATE distributed_meta SET value=value+1 WHERE key='epoch'",
            [],
        )?;
        let epoch: i64 = tx.query_row(
            "SELECT value FROM distributed_meta WHERE key='epoch'",
            [],
            |r| r.get(0),
        )?;
        tx.execute("INSERT OR IGNORE INTO drain_requests(assignment_id,reason) SELECT assignment_id,'drain persisted across coordinator restart' FROM reservations WHERE phase='draining'",[])?;
        tx.execute("UPDATE reservations SET phase='uncertain',detail='coordinator restarted; require node reconciliation' WHERE phase!='released'",[])?;
        let last_clock_ms = tx.query_row(
            "SELECT value FROM distributed_meta WHERE key='last_clock_ms'",
            [],
            |r| row_u64(r, 0),
        )?;
        tx.commit()?;
        let artifacts = ArtifactStore::open(
            &config.state_dir.join("artifacts"),
            config.max_artifact_bytes,
            config.artifact_quota_bytes,
        )?;
        Ok(Self {
            store,
            artifacts,
            config,
            epoch: epoch as u64,
            last_clock_ms,
            _lock: lock,
        })
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn handle(&mut self, principal: &Principal, request: Request) -> Result<Response> {
        self.handle_at(principal, request, now_ms())
    }
    pub fn handle_at(
        &mut self,
        principal: &Principal,
        request: Request,
        now: u64,
    ) -> Result<Response> {
        // Check the durable session before any clock update, expiration, upload,
        // or receipt operation. An old agent cannot regain authority by omitting
        // the wrapper, and a new session never conveys operator privileges.
        let request = self.validate_node_session(principal, request)?;
        ensure!(
            now >= self.last_clock_ms,
            "coordinator wall clock moved backwards; refusing new authorization until its durable clock watermark is reached"
        );
        if now > self.last_clock_ms {
            self.store.connection.execute(
                "UPDATE distributed_meta SET value=?1 WHERE key='last_clock_ms'",
                [db(now)?],
            )?;
            self.last_clock_ms = now;
        }
        self.expire(now)?;
        match request {
            Request::OpenReplaySession {
                node_id,
                boot_id,
                session_id,
            } => self.open_replay_session(&node_id, &boot_id, &session_id),
            Request::NodeSession { .. } => {
                unreachable!("session wrapper was validated and removed")
            }
            Request::PutPool { pool } => {
                operator(principal)?;
                valid_id(&pool.pool_id)?;
                ensure!(
                    pool.min_workers <= pool.max_workers && pool.max_workers <= 100_000,
                    "invalid pool worker bounds"
                );
                ensure!(
                    !pool.node_ids.is_empty(),
                    "pool must name eligible node IDs"
                );
                for n in &pool.node_ids {
                    valid_id(n)?;
                }
                if let Ok(existing) = self.pool(&pool.pool_id) {
                    let jobs: u32 = self.store.connection.query_row(
                        "SELECT count(*) FROM jobs WHERE pool_id=?1",
                        [&pool.pool_id],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        jobs == 0 || existing.class == pool.class,
                        "pool allocation class is immutable after job submission"
                    );
                }
                self.store.connection.execute("INSERT INTO pools(pool_id,spec_json) VALUES (?1,?2) ON CONFLICT(pool_id) DO UPDATE SET spec_json=excluded.spec_json",params![pool.pool_id,serde_json::to_string(&pool)?])?;
                Ok(Response::Ok)
            }
            Request::Submit { job } => {
                operator(principal)?;
                self.submit(job, now)?;
                Ok(Response::Ok)
            }
            Request::Status { job_id } => {
                operator(principal)?;
                self.status(job_id.as_deref())
            }
            Request::GetResult { task_id } => {
                operator(principal)?;
                let json: Option<String> = self
                    .store
                    .connection
                    .query_row(
                        "SELECT submission_json FROM result_payloads WHERE task_id=?1",
                        [task_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(Response::Result {
                    submission: json.map(|s| serde_json::from_str(&s)).transpose()?,
                })
            }
            Request::Retry {
                task_id,
                confirm_side_effects_reconciled,
            } => {
                operator(principal)?;
                let current = self.store.task(&task_id)?;
                ensure!(
                    matches!(
                        current.status,
                        TaskStatus::Queued | TaskStatus::NeedsReconciliation
                    ),
                    "only queued or reconciled tasks may retry"
                );
                ensure!(
                    current.replay_safe || confirm_side_effects_reconciled,
                    "unsafe retry requires explicit confirmation that external side effects were reconciled"
                );
                let request_json: String = self.store.connection.query_row(
                    "SELECT request_json FROM task_specs WHERE task_id=?1",
                    [&task_id],
                    |r| r.get(0),
                )?;
                let request: crate::execution_model::LaunchRequest =
                    serde_json::from_str(&request_json)?;
                ensure!(
                    request
                        .max_attempts
                        .is_none_or(|limit| current.generation < u64::from(limit)),
                    "task's immutable attempt budget is exhausted; Retry cannot increase it"
                );
                let held:i64=self.store.connection.query_row("SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) WHERE a.task_id=?1 AND r.phase!='released'",[&task_id],|r|r.get(0))?;
                ensure!(
                    held == 0,
                    "uncertain allocations remain reserved; retry cannot reclaim their capacity"
                );
                let tx = Transaction::new_unchecked(
                    &self.store.connection,
                    TransactionBehavior::Immediate,
                )?;
                tx.execute(
                    "UPDATE tasks SET status='queued' WHERE task_id=?1",
                    [&task_id],
                )?;
                tx.execute("DELETE FROM retry_state WHERE task_id=?1", [&task_id])?;
                tx.commit()?;
                Ok(Response::Ok)
            }
            Request::Cancel { job_id } => {
                operator(principal)?;
                let n = self
                    .store
                    .connection
                    .execute("UPDATE jobs SET cancelled=1 WHERE job_id=?1", [job_id])?;
                ensure!(n == 1, "unknown job");
                Ok(Response::Ok)
            }
            Request::DrainNode { node_id, drain } => {
                operator(principal)?;
                let n = self.store.connection.execute(
                    "UPDATE nodes SET drain=?2 WHERE node_id=?1",
                    params![node_id, drain],
                )?;
                ensure!(n == 1, "unknown node");
                Ok(Response::Ok)
            }
            Request::Heartbeat { report } => {
                node(principal, &report.node_id)?;
                self.heartbeat(report, now)
            }
            Request::Prepared {
                assignment_id,
                generation,
                coordinator_epoch,
                record,
            } => {
                self.authorize_assignment(principal, &assignment_id, generation)?;
                ensure!(coordinator_epoch == self.epoch, "stale coordinator epoch");
                ensure!(
                    record.assignment_id == assignment_id
                        && record.generation == generation
                        && record.phase == crate::execution_model::ExecutionPhase::Prepared,
                    "preparation record identity/phase mismatch"
                );
                let identity = record
                    .identity
                    .as_ref()
                    .context("prepared process identity absent")?;
                ensure!(
                    identity.assignment_id == assignment_id
                        && identity.generation == generation
                        && identity.pid > 0
                        && identity.start_time > 0
                        && !identity.boot_id.is_empty(),
                    "invalid prepared identity"
                );
                let owner_boot:String=self.store.connection.query_row("SELECT json_extract(n.report_json,'$.boot_id') FROM nodes n JOIN reservations r ON r.node_id=n.node_id WHERE r.assignment_id=?1",[&assignment_id],|r|r.get(0))?;
                ensure!(
                    identity.boot_id == owner_boot,
                    "prepared identity boot differs from node report"
                );
                let req = self.assignment_request(&assignment_id)?;
                ensure!(
                    req.task_id == record.task_id
                        && req.resources == record.resources
                        && req.class == record.class
                        && !record.backend.is_empty(),
                    "prepared resources/task changed"
                );
                for control in &req.required_controls {
                    ensure!(
                        record.evidence.iter().any(|e| &e.control == control
                            && e.applied
                            && !e.fallback
                            && e.available == Some(true)
                            && e.permitted == Some(true)),
                        "required control was not applied: {control}"
                    );
                }
                self.current_attempt(&assignment_id, generation)?;
                let prior: Option<String> = self
                    .store
                    .connection
                    .query_row(
                        "SELECT record_json FROM preparations WHERE assignment_id=?1",
                        [&assignment_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(prior) = prior {
                    let prior: crate::execution_model::ExecutionRecord =
                        serde_json::from_str(&prior)?;
                    ensure!(
                        prior.identity == record.identity && prior.backend == record.backend,
                        "prepared process identity/backend changed within an attempt"
                    );
                } else {
                    self.store.connection.execute(
                        "INSERT INTO preparations(assignment_id,record_json) VALUES (?1,?2)",
                        params![assignment_id, serde_json::to_string(&record)?],
                    )?;
                }
                self.grant(&assignment_id, generation, None, now)
            }
            Request::Renew {
                assignment_id,
                generation,
                coordinator_epoch,
                previous_sequence,
            } => {
                self.authorize_assignment(principal, &assignment_id, generation)?;
                ensure!(coordinator_epoch == self.epoch, "stale coordinator epoch");
                self.grant(&assignment_id, generation, Some(previous_sequence), now)
            }
            Request::Complete { submission } => self.accept(principal, submission, false),
            Request::PublishCheckpoint { submission } => self.accept(principal, submission, true),
            Request::Fail {
                assignment_id,
                generation,
                detail,
                failure_kind,
            } => {
                self.authorize_assignment(principal, &assignment_id, generation)?;
                self.record_failure(&assignment_id, generation, &detail, failure_kind, now)?;
                Ok(Response::Ok)
            }
            Request::BeginUpload {
                assignment_id,
                generation,
                artifact,
            } => {
                self.authorize_assignment(principal, &assignment_id, generation)?;
                self.current_attempt(&assignment_id, generation)?;
                let (upload_id, offset) =
                    self.artifacts
                        .begin(&assignment_id, generation, &artifact)?;
                Ok(Response::Upload { upload_id, offset })
            }
            Request::UploadChunk {
                upload_id,
                offset,
                data_hex,
            } => {
                ensure!(data_hex.len() <= CHUNK_LIMIT * 2, "upload chunk too large");
                let meta = self.artifacts.metadata(&upload_id)?;
                self.authorize_assignment(principal, &meta.assignment_id, meta.generation)?;
                self.current_attempt(&meta.assignment_id, meta.generation)?;
                let offset = self
                    .artifacts
                    .append(&upload_id, offset, &hex::decode(data_hex)?)?;
                Ok(Response::Upload { upload_id, offset })
            }
            Request::CommitUpload { upload_id } => {
                let m = self.artifacts.metadata(&upload_id)?;
                self.authorize_assignment(principal, &m.assignment_id, m.generation)?;
                self.current_attempt(&m.assignment_id, m.generation)?;
                let m = self.artifacts.publish(&upload_id)?;
                self.store.connection.execute("INSERT OR IGNORE INTO publications(assignment_id,generation,sha256,size) VALUES (?1,?2,?3,?4)",params![m.assignment_id,db(m.generation)?,m.artifact.sha256,db(m.artifact.size)?])?;
                Ok(Response::Artifact {
                    artifact: m.artifact,
                })
            }
            Request::AbortUpload { upload_id } => {
                let m = self.artifacts.metadata(&upload_id)?;
                self.authorize_assignment(principal, &m.assignment_id, m.generation)?;
                self.artifacts.abort(&upload_id)?;
                Ok(Response::Ok)
            }
            Request::ReadArtifact {
                sha256,
                offset,
                max_bytes,
            } => {
                self.authorize_read(principal, &sha256)?;
                let (data, eof) = self.artifacts.read(&sha256, offset, max_bytes)?;
                Ok(Response::Chunk {
                    data_hex: hex::encode(data),
                    eof,
                })
            }
        }
    }
    fn validate_node_session(&self, principal: &Principal, request: Request) -> Result<Request> {
        match request {
            Request::OpenReplaySession {
                node_id,
                boot_id,
                session_id,
            } => {
                node(principal, &node_id)?;
                valid_id(&node_id)?;
                ensure!(!boot_id.trim().is_empty(), "node boot identity absent");
                ensure!(
                    !uuid::Uuid::parse_str(&session_id)
                        .context("replay session must be a UUID")?
                        .is_nil(),
                    "replay session UUID must not be nil"
                );
                let retired: bool = self.store.connection.query_row("SELECT EXISTS(SELECT 1 FROM replay_retired_sessions WHERE node_id=?1 AND session_id=?2)", params![node_id, session_id], |r| r.get(0))?;
                ensure!(!retired, "retired replay node session cannot reopen");
                Ok(Request::OpenReplaySession {
                    node_id,
                    boot_id,
                    session_id,
                })
            }
            Request::NodeSession {
                session_id,
                request,
            } => {
                let Principal::Node { node_id } = principal else {
                    bail!("node session requires the owning node certificate");
                };
                ensure!(
                    !matches!(
                        &*request,
                        Request::NodeSession { .. } | Request::OpenReplaySession { .. }
                    ),
                    "nested node sessions and wrapped session opening are forbidden"
                );
                let current: Option<(String, String, bool)> = self.store.connection.query_row(
                    "SELECT session_id,boot_id,ready FROM replay_node_sessions WHERE node_id=?1",
                    [node_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).optional()?;
                let (current, boot_id, ready) =
                    current.context("node has no registered replay session")?;
                ensure!(session_id == current, "stale replay node session");
                ensure!(
                    ready || matches!(&*request, Request::Heartbeat { .. }),
                    "replay session requires a complete recovery heartbeat before other node operations"
                );
                if let Request::Heartbeat { report } = &*request {
                    node(principal, &report.node_id)?;
                    ensure!(
                        report.boot_id == boot_id,
                        "replay heartbeat boot identity differs from its session"
                    );
                }
                Ok(*request)
            }
            request => {
                if let Principal::Node { node_id } = principal {
                    let registered: bool = self.store.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM replay_node_sessions WHERE node_id=?1)",
                        [node_id],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        !registered,
                        "registered replay node requires a current session wrapper"
                    );
                }
                Ok(request)
            }
        }
    }

    fn open_replay_session(
        &self,
        node_id: &str,
        boot_id: &str,
        session_id: &str,
    ) -> Result<Response> {
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT session_id,boot_id FROM replay_node_sessions WHERE node_id=?1",
                [node_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if old
            .as_ref()
            .is_some_and(|(old_session, _)| old_session == session_id)
        {
            ensure!(
                old.as_ref()
                    .is_some_and(|(_, old_boot)| old_boot == boot_id),
                "same replay session cannot change boot identity"
            );
        } else {
            // Commit session replacement and allocation fencing together. The
            // distributed schema bump prevents older binaries bypassing this fence.
            tx.execute("UPDATE distributed_meta SET value=2 WHERE key='schema'", [])?;
            if let Some((old_session, _)) = old {
                tx.execute(
                    "INSERT INTO replay_retired_sessions(node_id,session_id) VALUES (?1,?2)",
                    params![node_id, old_session],
                )?;
            }
            tx.execute("INSERT INTO replay_node_sessions(node_id,session_id,boot_id,ready) VALUES (?1,?2,?3,0) ON CONFLICT(node_id) DO UPDATE SET session_id=excluded.session_id,boot_id=excluded.boot_id,ready=0", params![node_id, session_id, boot_id])?;
            tx.execute("UPDATE tasks SET status=CASE replay_safe WHEN 1 THEN 'queued' ELSE 'needs_reconciliation' END WHERE status!='completed' AND assignment_id IN (SELECT assignment_id FROM reservations WHERE node_id=?1 AND phase!='released')", [node_id])?;
            tx.execute("UPDATE reservations SET phase='uncertain',detail='replay node session replaced; require authoritative recovery and verified release' WHERE node_id=?1 AND phase!='released'", [node_id])?;
            tx.execute("INSERT OR IGNORE INTO allocation_fences(assignment_id,reason) SELECT assignment_id,'replay node session replaced' FROM reservations WHERE node_id=?1 AND phase!='released'", [node_id])?;
        }
        let mut rows = tx.prepare("SELECT r.assignment_id,a.generation,s.request_json,p.record_json,COALESCE(json_extract(p.record_json,'$.identity.boot_id'),json_extract(n.report_json,'$.boot_id')),r.lease_sequence,c.submission_json FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) LEFT JOIN preparations p USING(assignment_id) LEFT JOIN nodes n ON n.node_id=r.node_id LEFT JOIN checkpoints c ON c.task_id=a.task_id WHERE r.node_id=?1 AND r.phase!='released' ORDER BY r.assignment_id")?;
        let values = rows
            .query_map([node_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    row_u64(r, 1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    row_u64(r, 5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(rows);
        let allocations = values
            .into_iter()
            .map(
                |(
                    id,
                    generation,
                    request,
                    prepared,
                    previous_boot_id,
                    lease_sequence,
                    checkpoint,
                )| {
                    let mut request: crate::execution_model::LaunchRequest =
                        serde_json::from_str(&request)?;
                    request.assignment_id = id;
                    Ok(ReplayRecoveryAllocation {
                        assignment: Assignment {
                            node_id: node_id.into(),
                            generation,
                            coordinator_epoch: self.epoch,
                            request,
                            checkpoint: checkpoint
                                .map(|json| serde_json::from_str(&json))
                                .transpose()?,
                        },
                        prepared: prepared
                            .map(|json| serde_json::from_str(&json))
                            .transpose()?,
                        previous_boot_id,
                        lease_sequence,
                    })
                },
            )
            .collect::<Result<Vec<_>>>()?;
        let mut rows = tx.prepare("SELECT report_json FROM unrecognized_allocations WHERE node_id=?1 ORDER BY assignment_id")?;
        let unrecognized = rows
            .query_map([node_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|json| serde_json::from_str(&json))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(rows);
        tx.commit()?;
        Ok(Response::ReplayRecovery {
            snapshot: ReplayRecoverySnapshot {
                session_id: session_id.into(),
                coordinator_epoch: self.epoch,
                allocations,
                unrecognized,
            },
        })
    }

    fn submit(&self, job: JobSpec, now: u64) -> Result<()> {
        valid_id(&job.job_id)?;
        ensure!(
            !job.tasks.is_empty() && job.tasks.len() <= 100_000,
            "job must have bounded tasks"
        );
        let pool = self.pool(&job.pool_id)?;
        let serialized = serde_json::to_string(&job)?;
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT spec_json FROM jobs WHERE job_id=?1",
                [&job.job_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(
                serde_json::to_value(serde_json::from_str::<JobSpec>(&old)?)?
                    == serde_json::to_value(&job)?,
                "job ID already exists with different specification"
            );
            return Ok(());
        }
        tx.execute("INSERT INTO jobs(job_id,pool_id,priority,submitted_ms,spec_json) VALUES (?1,?2,?3,?4,?5)",params![job.job_id,job.pool_id,job.priority,db(now)?,serialized])?;
        for request in &job.tasks {
            valid_id(&request.task_id)?;
            ensure!(
                request.assignment_id.is_empty(),
                "submission assignment_id must be empty; coordinator creates it"
            );
            ensure!(
                !request.argv.is_empty() && !request.argv[0].is_empty(),
                "empty workload command"
            );
            ensure!(
                request.class == pool.class,
                "task and pool allocation classes differ"
            );
            ensure!(request.cwd.is_absolute(), "workload cwd must be absolute");
            ensure!(
                request.max_attempts.is_none_or(|limit| limit > 0),
                "max_attempts must be positive when configured"
            );
            ensure!(
                request.resources.cpu_millicores > 0 && request.resources.ram_mib > 0,
                "explicit positive CPU/RAM request required"
            );
            ensure!(
                request.input_artifacts.len() <= 64,
                "too many input artifacts"
            );
            let mut input_names = std::collections::BTreeSet::new();
            let mut input_bytes = 0u64;
            for input in &request.input_artifacts {
                ensure!(
                    !input.name.is_empty()
                        && input.name.len() <= 128
                        && !matches!(input.name.as_str(), "." | "..")
                        && input
                            .name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                        && input_names.insert(&input.name),
                    "input names must be unique safe basenames"
                );
                input_bytes = input_bytes
                    .checked_add(input.size)
                    .context("input size overflow")?;
                ensure!(
                    input_bytes <= self.config.artifact_quota_bytes,
                    "input artifacts exceed configured storage bound"
                );
                let published: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM publications WHERE sha256=?1 AND size=?2)",
                    params![input.sha256, db(input.size)?],
                    |r| r.get(0),
                )?;
                ensure!(published, "input artifact has no durable publication");
                self.artifacts.verify(&ArtifactRef {
                    sha256: input.sha256.clone(),
                    size: input.size,
                })?;
            }
            tx.execute(
                "INSERT INTO tasks(task_id,replay_safe,status) VALUES (?1,?2,'queued')",
                params![request.task_id, request.replay_safe],
            )?;
            tx.execute(
                "INSERT INTO task_specs(task_id,job_id,request_json) VALUES (?1,?2,?3)",
                params![request.task_id, job.job_id, serde_json::to_string(request)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    fn pool(&self, id: &str) -> Result<PoolSpec> {
        let json: String = self
            .store
            .connection
            .query_row("SELECT spec_json FROM pools WHERE pool_id=?1", [id], |r| {
                r.get(0)
            })
            .context("unknown pool")?;
        Ok(serde_json::from_str(&json)?)
    }
    fn heartbeat(&mut self, report: NodeReport, now: u64) -> Result<Response> {
        valid_id(&report.node_id)?;
        ensure!(!report.boot_id.is_empty(), "node boot identity absent");
        ensure!(
            report.observed_at_unix_ms <= now.saturating_add(self.config.telemetry_ttl_ms),
            "node observation is in the future"
        );
        let fresh = now.saturating_sub(report.observed_at_unix_ms) <= self.config.telemetry_ttl_ms;
        let observed_live = report.allocations.iter().all(|a| {
            !matches!(
                a.phase,
                RemotePhase::Running
                    | RemotePhase::Draining
                    | RemotePhase::Authorized
                    | RemotePhase::Uncertain
            ) || a.observed.is_some()
        });
        let previous_observed:Option<u64>=self.store.connection.query_row("SELECT json_extract(report_json,'$.observed_at_unix_ms') FROM nodes WHERE node_id=?1",[&report.node_id],|r|row_u64(r,0)).optional()?;
        ensure!(
            previous_observed.is_none_or(|old| report.observed_at_unix_ms >= old),
            "node observation regressed; refusing delayed report"
        );
        let replay_ready: Option<bool> = self
            .store
            .connection
            .query_row(
                "SELECT ready FROM replay_node_sessions WHERE node_id=?1",
                [&report.node_id],
                |r| r.get(0),
            )
            .optional()?;
        if replay_ready == Some(false) {
            let mut rows = self.store.connection.prepare("SELECT r.assignment_id,a.generation FROM reservations r JOIN assignments a USING(assignment_id) WHERE r.node_id=?1 AND r.phase!='released'")?;
            for row in rows.query_map([&report.node_id], |r| {
                Ok((r.get::<_, String>(0)?, row_u64(r, 1)?))
            })? {
                let (id, generation) = row?;
                ensure!(
                    report
                        .allocations
                        .iter()
                        .any(|allocation| allocation.assignment_id == id
                            && allocation.generation == generation
                            && matches!(
                                allocation.phase,
                                RemotePhase::Released | RemotePhase::Uncertain
                            )),
                    "initial replay recovery heartbeat must include every retained allocation at its exact generation as released or uncertain: {id}"
                );
            }
        }
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO nodes(node_id,report_json,received_ms) VALUES (?1,?2,?3) ON CONFLICT(node_id) DO UPDATE SET report_json=excluded.report_json,received_ms=excluded.received_ms",params![report.node_id,serde_json::to_string(&report)?,db(now)?])?;
        let mut unknown_allocations = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for allocation in &report.allocations {
            ensure!(
                seen.insert(&allocation.assignment_id),
                "duplicate allocation report"
            );
            let owned:Option<(String,u64,String)>=tx.query_row("SELECT r.node_id,a.generation,r.phase FROM reservations r JOIN assignments a USING(assignment_id) WHERE assignment_id=?1",[&allocation.assignment_id],|r|Ok((r.get(0)?,row_u64(r,1)?,r.get(2)?))).optional()?;
            let Some((owner, generation, oldphase)) = owned else {
                let prior:Option<u64>=tx.query_row("SELECT generation FROM unrecognized_allocations WHERE node_id=?1 AND assignment_id=?2",params![report.node_id,allocation.assignment_id],|r|row_u64(r,0)).optional()?;
                ensure!(
                    prior.is_none_or(|generation| generation == allocation.generation),
                    "unrecognized allocation generation changed before release"
                );
                if allocation.phase == RemotePhase::Released {
                    tx.execute("DELETE FROM unrecognized_allocations WHERE node_id=?1 AND assignment_id=?2",params![report.node_id,allocation.assignment_id])?;
                } else {
                    tx.execute("INSERT INTO unrecognized_allocations(node_id,assignment_id,generation,report_json) VALUES (?1,?2,?3,?4) ON CONFLICT(node_id,assignment_id) DO UPDATE SET report_json=excluded.report_json",params![report.node_id,allocation.assignment_id,db(allocation.generation)?,serde_json::to_string(allocation)?])?;
                }
                continue;
            };
            ensure!(
                owner == report.node_id && generation == allocation.generation,
                "reported allocation belongs to another node/attempt"
            );
            let requested_phase = phase_name(allocation.phase);
            let sequence: u64 = tx.query_row(
                "SELECT lease_sequence FROM reservations WHERE assignment_id=?1",
                [&allocation.assignment_id],
                |r| row_u64(r, 0),
            )?;
            let fenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM allocation_fences WHERE assignment_id=?1)",
                [&allocation.assignment_id],
                |r| r.get(0),
            )?;
            let contradiction = oldphase == "released"
                && allocation.phase != RemotePhase::Released
                && previous_observed.is_none_or(|old| report.observed_at_unix_ms > old);
            let invalid_running = sequence == 0
                && matches!(
                    allocation.phase,
                    RemotePhase::Running | RemotePhase::Authorized
                );
            let phase = if allocation.phase == RemotePhase::Released {
                "released"
            } else if contradiction || invalid_running || allocation.phase == RemotePhase::Uncertain
            {
                "uncertain"
            } else if oldphase == "released" {
                "released"
            } else if fenced {
                "uncertain"
            } else if oldphase == "draining" {
                "draining"
            } else if oldphase == "running"
                && matches!(
                    allocation.phase,
                    RemotePhase::Offered | RemotePhase::Prepared | RemotePhase::Authorized
                )
            {
                "running"
            } else if oldphase == "authorized"
                && matches!(
                    allocation.phase,
                    RemotePhase::Offered | RemotePhase::Prepared
                )
            {
                "authorized"
            } else {
                requested_phase
            };
            if phase == "draining" {
                tx.execute("INSERT OR IGNORE INTO drain_requests(assignment_id,reason) VALUES (?1,'node-local drain began')",[&allocation.assignment_id])?;
            }
            if phase == "uncertain" {
                tx.execute(
                    "INSERT OR IGNORE INTO allocation_fences(assignment_id,reason) VALUES (?1,?2)",
                    params![
                        allocation.assignment_id,
                        "node lifecycle uncertainty or contradictory report"
                    ],
                )?;
            }
            if contradiction || invalid_running {
                unknown_allocations.push(allocation.assignment_id.clone());
            }
            // A release report is a node assertion backed by its local journal, never a timeout inference.
            tx.execute("UPDATE reservations SET phase=?2,observed_json=?3,detail=?4 WHERE assignment_id=?1",params![allocation.assignment_id,phase,allocation.observed.as_ref().map(serde_json::to_string).transpose()?,allocation.detail])?;
        }
        let mut outstanding=tx.prepare("SELECT assignment_id FROM reservations WHERE node_id=?1 AND phase!='released' AND (?2 OR lease_sequence>0 OR phase='prepared')")?;
        let missing = outstanding
            .query_map(params![report.node_id, replay_ready.is_some()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(outstanding);
        for id in missing.into_iter().filter(|id| !seen.contains(id)) {
            tx.execute("UPDATE reservations SET phase='uncertain',observed_json=NULL,detail='owning node omitted a prepared or authorized allocation; require reconciliation' WHERE assignment_id=?1",[&id])?;
            tx.execute("INSERT OR IGNORE INTO allocation_fences(assignment_id,reason) VALUES (?1,'allocation missing from owning node report')",[&id])?;
            unknown_allocations.push(id);
        }
        let mut orphaned=tx.prepare("SELECT assignment_id FROM unrecognized_allocations WHERE node_id=?1 ORDER BY assignment_id")?;
        unknown_allocations.extend(
            orphaned
                .query_map([&report.node_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        drop(orphaned);
        if replay_ready == Some(false) {
            tx.execute(
                "UPDATE replay_node_sessions SET ready=1 WHERE node_id=?1",
                [&report.node_id],
            )?;
        }
        tx.commit()?;
        for allocation in &report.allocations {
            if allocation.phase == RemotePhase::Released {
                let state:Option<(String,u64)>=self.store.connection.query_row("SELECT t.status,a.generation FROM reservations r JOIN assignments a USING(assignment_id) JOIN tasks t USING(task_id) WHERE r.assignment_id=?1 AND t.assignment_id=r.assignment_id",[&allocation.assignment_id],|r|Ok((r.get(0)?,row_u64(r,1)?))).optional()?;
                if state.is_some_and(|(status, generation)| {
                    generation == allocation.generation
                        && matches!(status.as_str(), "queued" | "needs_reconciliation")
                }) {
                    self.record_failure(
                        &allocation.assignment_id,
                        allocation.generation,
                        "release after authorization expiry",
                        FailureKind::Yielded,
                        now,
                    )?;
                }
            }
        }
        if fresh && observed_live && unknown_allocations.is_empty() && report.expansion_allowed {
            self.schedule(&report, now)?;
        }
        let mut assignments = Vec::new();
        let mut drain = Vec::new();
        let mut uncertain = unknown_allocations;
        let rows = self.reservation_rows(&report.node_id)?;
        for (id, generation, phase, task_id) in rows {
            if phase == "released" {
                continue;
            }
            let should_drain = self.should_drain(&id)?
                || ((!fresh || phase == "uncertain")
                    && self.assignment_request(&id)?.class == AllocationClass::Opportunistic);
            if should_drain {
                drain.push(id.clone());
            }
            if phase == "uncertain" {
                uncertain.push(id.clone());
            }
            let never_authorized: bool = self.store.connection.query_row(
                "SELECT lease_sequence=0 FROM reservations WHERE assignment_id=?1",
                [&id],
                |r| r.get(0),
            )?;
            if phase == "uncertain" && never_authorized && !should_drain {
                drain.push(id.clone());
            }
            // Include revoked offers so a node can reconcile a lost offer delivery
            // without inventing its generation or launching its workload.
            if phase == "offered" || (phase == "uncertain" && never_authorized) {
                let checkpoint: Option<String> = self
                    .store
                    .connection
                    .query_row(
                        "SELECT submission_json FROM checkpoints WHERE task_id=?1",
                        [&task_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                assignments.push(Assignment {
                    node_id: report.node_id.clone(),
                    generation,
                    coordinator_epoch: self.epoch,
                    request: self.assignment_request(&id)?,
                    checkpoint: checkpoint.map(|s| serde_json::from_str(&s)).transpose()?,
                });
            }
        }
        uncertain.sort();
        uncertain.dedup();
        Ok(Response::Heartbeat {
            reply: HeartbeatReply {
                coordinator_epoch: self.epoch,
                assignments,
                drain,
                uncertain,
            },
        })
    }
    fn replay_node_can_admit(&self, node_id: &str) -> Result<bool> {
        let ready: Option<bool> = self
            .store
            .connection
            .query_row(
                "SELECT ready FROM replay_node_sessions WHERE node_id=?1",
                [node_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(ready) = ready else {
            return Ok(true);
        };
        if !ready {
            return Ok(false);
        }
        let blocked: bool = self.store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE node_id=?1 AND phase='uncertain') OR EXISTS(SELECT 1 FROM unrecognized_allocations WHERE node_id=?1)",
            [node_id], |r| r.get(0),
        )?;
        Ok(!blocked)
    }

    fn replay_request_allowed(
        &self,
        node_id: &str,
        request: &crate::execution_model::LaunchRequest,
    ) -> Result<bool> {
        let replayable: bool = self.store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM replay_node_sessions WHERE node_id=?1)",
            [node_id],
            |r| r.get(0),
        )?;
        Ok(!replayable
            || (request.replay_safe
                && request.class == AllocationClass::Opportunistic
                && !request
                    .required_controls
                    .iter()
                    .any(|control| control == "storage.durable_local")))
    }

    fn node_charge(&self, node_id: &str) -> Result<Resources> {
        let mut charge = Resources::default();
        let mut active=self.store.connection.prepare("SELECT resources_json,observed_json FROM reservations WHERE node_id=?1 AND phase!='released'")?;
        for row in active.query_map([node_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })? {
            let (r, o) = row?;
            let mut r: Resources = serde_json::from_str(&r)?;
            if let Some(o) = o {
                let o: Resources = serde_json::from_str(&o)?;
                r.cpu_millicores = r.cpu_millicores.max(o.cpu_millicores);
                r.ram_mib = r.ram_mib.max(o.ram_mib);
                for (k, v) in o.gpu_memory_mib {
                    let entry = r.gpu_memory_mib.entry(k).or_default();
                    *entry = (*entry).max(v);
                }
            }
            add(&mut charge, &r)?;
        }
        Ok(charge)
    }
    fn schedule(&self, report: &NodeReport, now: u64) -> Result<()> {
        let drain: bool = self.store.connection.query_row(
            "SELECT drain FROM nodes WHERE node_id=?1",
            [&report.node_id],
            |r| r.get(0),
        )?;
        if drain || !self.replay_node_can_admit(&report.node_id)? {
            return Ok(());
        }
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let unstarted: u32 = tx.query_row(
            "SELECT count(*) FROM reservations WHERE node_id=?1 AND phase='offered'",
            [&report.node_id],
            |r| r.get(0),
        )?;
        let mut remaining_slots = report.launch_slots.saturating_sub(unstarted);
        let mut charge = self.node_charge(&report.node_id)?;
        let mut stmt=tx.prepare("SELECT t.task_id,t.generation,s.request_json,j.pool_id FROM tasks t JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) WHERE t.status='queued' AND j.cancelled=0 ORDER BY j.priority DESC,s.sequence ASC")?;
        let queued = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    row_u64(r, 1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (task, generation, json, pool_id) in queued {
            let not_before: u64 = tx
                .query_row(
                    "SELECT not_before_ms FROM retry_state WHERE task_id=?1",
                    [&task],
                    |r| row_u64(r, 0),
                )
                .optional()?
                .unwrap_or(0);
            if now < not_before {
                continue;
            }
            let pool = self.pool(&pool_id)?;
            if !pool.node_ids.contains(&report.node_id) {
                continue;
            }
            let count:u32=tx.query_row("SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) WHERE j.pool_id=?1 AND r.phase!='released'",[&pool_id],|r|r.get(0))?;
            if pool.max_workers == 0 {
                continue;
            }
            let mut request: crate::execution_model::LaunchRequest = serde_json::from_str(&json)?;
            if !self.replay_request_allowed(&report.node_id, &request)? {
                continue;
            }
            if request
                .max_attempts
                .is_some_and(|limit| generation >= u64::from(limit))
            {
                tx.execute(
                    "UPDATE tasks SET status='needs_reconciliation' WHERE task_id=?1",
                    [&task],
                )?;
                continue;
            }
            if !gpu_request_allowed(&request, report) {
                continue;
            }
            if !request
                .required_controls
                .iter()
                .all(|c| report.available_controls.contains(c))
            {
                continue;
            }
            // A task retained on another node cannot preempt work while it is itself fenced.
            let held:i64=tx.query_row("SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) WHERE a.task_id=?1 AND r.phase!='released'",[&task],|r|r.get(0))?;
            if held > 0 {
                continue;
            }
            let mut next = charge.clone();
            add(&mut next, &request.resources)?;
            if remaining_slots == 0
                || count >= pool.max_workers
                || !fits_requested(&next, &report.managed_budget, &request.resources)
            {
                self.request_preemption(&tx, report, &request, &pool, count, &charge)?;
                continue;
            }
            request.assignment_id = uuid::Uuid::new_v4().to_string();
            let nextgen = generation.checked_add(1).context("generation overflow")?;
            tx.execute(
                "INSERT INTO assignments(assignment_id,task_id,generation) VALUES (?1,?2,?3)",
                params![request.assignment_id, task, db(nextgen)?],
            )?;
            tx.execute("UPDATE tasks SET status='assigned',generation=?2,assignment_id=?3 WHERE task_id=?1",params![task,db(nextgen)?,request.assignment_id])?;
            tx.execute("INSERT INTO reservations(assignment_id,node_id,phase,resources_json,lease_deadline_ms,epoch) VALUES (?1,?2,'offered',?3,?4,?5)",params![request.assignment_id,report.node_id,serde_json::to_string(&request.resources)?,db(now.saturating_add(self.config.lease_ms))?,db(self.epoch)?])?;
            charge = next;
            remaining_slots -= 1;
        }
        tx.commit()?;
        Ok(())
    }
    fn request_preemption(
        &self,
        tx: &Transaction<'_>,
        report: &NodeReport,
        request: &crate::execution_model::LaunchRequest,
        pool: &PoolSpec,
        count: u32,
        charge: &Resources,
    ) -> Result<()> {
        if !gpu_request_allowed(request, report)
            || !fits_requested(
                &request.resources,
                &report.managed_budget,
                &request.resources,
            )
        {
            return Ok(());
        }
        let priority: i32 = tx.query_row(
            "SELECT j.priority FROM jobs j JOIN task_specs s USING(job_id) WHERE s.task_id=?1",
            [&request.task_id],
            |r| r.get(0),
        )?;
        let mut statement=tx.prepare("SELECT r.assignment_id,r.resources_json,r.observed_json,j.pool_id,p.spec_json FROM reservations r JOIN assignments a USING(assignment_id) JOIN tasks t USING(task_id) JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) JOIN pools p USING(pool_id) WHERE r.node_id=?1 AND r.phase NOT IN ('released','uncertain') AND j.priority<?2 AND t.replay_safe=1 AND json_extract(s.request_json,'$.class')='opportunistic' ORDER BY j.priority ASC,s.sequence DESC")?;
        let candidates = statement
            .query_map(params![report.node_id, priority], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut projected = charge.clone();
        let mut selected = Vec::new();
        let mut selected_counts = std::collections::BTreeMap::<String, u32>::new();
        let mut frees_pool_slot = count < pool.max_workers;
        for (id, resources, observed, candidate_pool, pool_json) in candidates {
            let protected: PoolSpec = serde_json::from_str(&pool_json)?;
            let active:u32=tx.query_row("SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) WHERE j.pool_id=?1 AND r.phase!='released'",[&candidate_pool],|r|r.get(0))?;
            let already = selected_counts.get(&candidate_pool).copied().unwrap_or(0);
            if active.saturating_sub(already) <= protected.min_workers {
                continue;
            }
            let mut resource: Resources = serde_json::from_str(&resources)?;
            if let Some(observed) = observed {
                let observed: Resources = serde_json::from_str(&observed)?;
                resource.cpu_millicores = resource.cpu_millicores.max(observed.cpu_millicores);
                resource.ram_mib = resource.ram_mib.max(observed.ram_mib);
                for (k, v) in observed.gpu_memory_mib {
                    let entry = resource.gpu_memory_mib.entry(k).or_default();
                    *entry = (*entry).max(v);
                }
            }
            subtract(&mut projected, &resource);
            frees_pool_slot |= candidate_pool == pool.pool_id;
            *selected_counts.entry(candidate_pool).or_default() += 1;
            selected.push(id);
            let mut future = projected.clone();
            add(&mut future, &request.resources)?;
            if frees_pool_slot
                && fits_requested(&future, &report.managed_budget, &request.resources)
            {
                for id in selected {
                    tx.execute(
                        "INSERT OR IGNORE INTO drain_requests(assignment_id,reason) VALUES (?1,?2)",
                        params![
                            id,
                            format!(
                                "higher-priority task {} awaits confirmed release",
                                request.task_id
                            )
                        ],
                    )?;
                }
                break;
            }
        }
        Ok(())
    }
    fn record_failure(
        &self,
        id: &str,
        generation: u64,
        detail: &str,
        kind: FailureKind,
        now: u64,
    ) -> Result<()> {
        let tx =
            Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
        let seen: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM failure_receipts WHERE assignment_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if seen {
            return Ok(());
        }
        let (phase,task):(String,String)=tx.query_row("SELECT r.phase,a.task_id FROM reservations r JOIN assignments a USING(assignment_id) WHERE assignment_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            phase == "released",
            "failure cannot release uncertain capacity"
        );
        let current = self.store.task(&task)?;
        ensure!(
            current.generation == generation && current.assignment_id.as_deref() == Some(id),
            "stale failure report"
        );
        let yielded = kind == FailureKind::Yielded || self.should_drain(id)?;
        let previous: u32 = tx
            .query_row(
                "SELECT failures FROM retry_state WHERE task_id=?1",
                [&task],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let failures = if yielded {
            previous
        } else {
            previous.checked_add(1).context("failure count overflow")?
        };
        let prior_yields: u32 = tx
            .query_row(
                "SELECT yield_count FROM retry_state WHERE task_id=?1",
                [&task],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let yield_count = if yielded {
            prior_yields
                .checked_add(1)
                .context("yield count overflow")?
        } else {
            0
        };
        let (base, maximum, exponent) = if yielded {
            (
                self.config.yield_retry_backoff_ms,
                self.config.yield_retry_backoff_max_ms,
                yield_count,
            )
        } else {
            (
                self.config.retry_backoff_ms,
                self.config.retry_backoff_max_ms,
                failures,
            )
        };
        let delay = base
            .saturating_mul(
                1u64.checked_shl(exponent.saturating_sub(1))
                    .unwrap_or(u64::MAX),
            )
            .min(maximum);
        let request = self.assignment_request(id)?;
        let attempts_available = request
            .max_attempts
            .is_none_or(|limit| generation < u64::from(limit));
        if current.status != TaskStatus::Completed {
            let next = if current.replay_safe
                && failures < self.config.retry_limit
                && attempts_available
            {
                "queued"
            } else {
                "needs_reconciliation"
            };
            tx.execute(
                "UPDATE tasks SET status=?2 WHERE task_id=?1",
                params![task, next],
            )?;
            tx.execute("INSERT INTO retry_state(task_id,failures,not_before_ms,yield_count) VALUES (?1,?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET failures=excluded.failures,not_before_ms=excluded.not_before_ms,yield_count=excluded.yield_count",params![task,failures,db(now.saturating_add(delay))?,yield_count])?;
        }
        tx.execute(
            "INSERT INTO failure_receipts(assignment_id,failure_kind) VALUES (?1,?2)",
            params![
                id,
                if yielded {
                    "yielded"
                } else {
                    "execution_failure"
                }
            ],
        )?;
        tx.execute(
            "UPDATE reservations SET detail=?2 WHERE assignment_id=?1",
            params![
                id,
                format!(
                    "{detail}; execution failures={failures}/{}, consecutive yields={yield_count}, attempt={generation}/{:?}, retry delay={delay}ms",
                    self.config.retry_limit, request.max_attempts
                )
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn assignment_request(&self, id: &str) -> Result<crate::execution_model::LaunchRequest> {
        let json:String=self.store.connection.query_row("SELECT request_json FROM task_specs JOIN assignments USING(task_id) WHERE assignment_id=?1",[id],|r|r.get(0))?;
        let mut r: crate::execution_model::LaunchRequest = serde_json::from_str(&json)?;
        r.assignment_id = id.to_owned();
        Ok(r)
    }
    fn reservation_rows(&self, node_id: &str) -> Result<Vec<(String, u64, String, String)>> {
        let mut s=self.store.connection.prepare("SELECT assignment_id,generation,phase,task_id FROM reservations JOIN assignments USING(assignment_id) WHERE node_id=?1")?;
        Ok(s.query_map([node_id], |r| {
            Ok((r.get(0)?, row_u64(r, 1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    fn authorize_assignment(&self, p: &Principal, id: &str, generation: u64) -> Result<()> {
        let(owner,g):(String,u64)=self.store.connection.query_row("SELECT node_id,generation FROM reservations JOIN assignments USING(assignment_id) WHERE assignment_id=?1",[id],|r|Ok((r.get(0)?,row_u64(r,1)?)))?;
        ensure!(g == generation, "assignment generation mismatch");
        match p {
            Principal::Operator => Ok(()),
            Principal::Node { node_id } => {
                ensure!(node_id == &owner, "assignment belongs to a different node");
                Ok(())
            }
        }
    }
    fn current_attempt(&self, id: &str, generation: u64) -> Result<String> {
        let task: String = self.store.connection.query_row(
            "SELECT task_id FROM assignments WHERE assignment_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let current = self.store.task(&task)?;
        ensure!(
            current.generation == generation && current.assignment_id.as_deref() == Some(id),
            "stale attempt"
        );
        ensure!(
            matches!(current.status, TaskStatus::Assigned | TaskStatus::Completed),
            "task requires reconciliation or retry"
        );
        Ok(task)
    }
    fn should_drain(&self, id: &str) -> Result<bool> {
        let(cancelled,node_drain,pool_json):(bool,bool,String)=self.store.connection.query_row("SELECT j.cancelled,n.drain,p.spec_json FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) JOIN pools p USING(pool_id) JOIN nodes n ON n.node_id=r.node_id WHERE assignment_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let pool: PoolSpec = serde_json::from_str(&pool_json)?;
        let owner: String = self.store.connection.query_row(
            "SELECT node_id FROM reservations WHERE assignment_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let preempt: bool = self.store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM drain_requests WHERE assignment_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        let draining: bool = self.store.connection.query_row(
            "SELECT phase='draining' FROM reservations WHERE assignment_id=?1",
            [id],
            |r| r.get(0),
        )?;
        if cancelled || node_drain || preempt || draining || !pool.node_ids.contains(&owner) {
            return Ok(true);
        }
        let rank:u32=self.store.connection.query_row("SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) JOIN jobs j USING(job_id) WHERE j.pool_id=?1 AND r.phase!='released' AND (s.sequence<(SELECT sequence FROM task_specs JOIN assignments USING(task_id) WHERE assignment_id=?2) OR r.assignment_id=?2)",params![pool.pool_id,id],|r|r.get(0))?;
        Ok(rank > pool.max_workers)
    }
    fn grant(
        &self,
        id: &str,
        generation: u64,
        previous: Option<u64>,
        now: u64,
    ) -> Result<Response> {
        self.current_attempt(id, generation)?;
        let(phase,seq,deadline):(String,u64,u64)=self.store.connection.query_row("SELECT phase,lease_sequence,lease_deadline_ms FROM reservations WHERE assignment_id=?1",[id],|r|Ok((r.get(0)?,row_u64(r,1)?,row_u64(r,2)?)))?;
        ensure!(
            !matches!(phase.as_str(), "released" | "uncertain"),
            "allocation needs reconciliation before authorization"
        );
        let drain = self.should_drain(id)?;
        let request = self.assignment_request(id)?;
        let owner: String = self.store.connection.query_row(
            "SELECT node_id FROM reservations WHERE assignment_id=?1",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            self.replay_node_can_admit(&owner)? && self.replay_request_allowed(&owner, &request)?,
            "replay node authorization requires completed recovery and replay-safe opportunistic work without durable-local requirements"
        );
        if seq == 0 && !drain {
            let (received, report_json): (u64, String) = self.store.connection.query_row(
                "SELECT n.received_ms,n.report_json FROM reservations r JOIN nodes n ON n.node_id=r.node_id WHERE r.assignment_id=?1",
                [id], |r| Ok((row_u64(r, 0)?, r.get(1)?)),
            )?;
            let report: NodeReport = serde_json::from_str(&report_json)?;
            ensure!(
                report.expansion_allowed
                    && now.saturating_sub(received) <= self.config.telemetry_ttl_ms
                    && now.saturating_sub(report.observed_at_unix_ms)
                        <= self.config.telemetry_ttl_ms,
                "initial execution authorization requires fresh capacity evidence"
            );
            ensure!(
                gpu_request_allowed(&request, &report),
                "initial GPU authorization requires current per-device admission and compatible allocation class"
            );
            // This request is already reserved. Never add it twice and never drop
            // uncertain siblings. A deficit on an unrelated UUID cannot authorize
            // this UUID, nor block independently qualified CPU/other-GPU capacity.
            let charge = self.node_charge(&report.node_id)?;
            ensure!(
                fits_requested(&charge, &report.managed_budget, &request.resources),
                "initial execution authorization exceeds current CPU/RAM or requested GPU budget"
            );
        }
        if seq > 0 && deadline <= now && request.class == AllocationClass::Opportunistic {
            bail!("allocation lease expired")
        }
        let retry = match previous {
            None => seq > 0,
            Some(p) => {
                ensure!(seq > 0, "allocation was not prepared");
                ensure!(
                    p == seq || p.checked_add(1) == Some(seq),
                    "stale lease sequence"
                );
                p != seq
            }
        };
        let (next_seq, next_deadline) = if retry || drain {
            (seq, deadline)
        } else {
            (
                seq.checked_add(1).context("lease sequence overflow")?,
                now.saturating_add(self.config.lease_ms),
            )
        };
        if !retry && !drain {
            self.store.connection.execute("UPDATE reservations SET phase='authorized',lease_sequence=?2,lease_deadline_ms=?3,epoch=?4 WHERE assignment_id=?1",params![id,db(next_seq)?,db(next_deadline)?,db(self.epoch)?])?;
        }
        Ok(Response::Lease {
            lease: Lease {
                assignment_id: id.to_owned(),
                generation,
                coordinator_epoch: self.epoch,
                sequence: next_seq,
                valid_for_ms: next_deadline.saturating_sub(now).min(self.config.lease_ms),
                drain,
            },
        })
    }
    fn expire(&self, now: u64) -> Result<()> {
        let mut s=self.store.connection.prepare("SELECT r.assignment_id,a.task_id,a.generation,s.request_json FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id) WHERE r.phase NOT IN ('released','uncertain') AND r.lease_deadline_ms<=?1")?;
        let rows = s
            .query_map([db(now)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    row_u64(r, 2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(s);
        for (id, task, generation, json) in rows {
            let req: crate::execution_model::LaunchRequest = serde_json::from_str(&json)?;
            if req.class == AllocationClass::Opportunistic {
                self.store.connection.execute("UPDATE reservations SET phase='uncertain',detail='authorization expired; capacity retained until release confirmation' WHERE assignment_id=?1",[&id])?;
                self.store.connection.execute("INSERT OR IGNORE INTO allocation_fences(assignment_id,reason) VALUES (?1,'lease expired')",[&id])?;
                self.store.mark_uncertain(&task, generation)?;
            }
        }
        Ok(())
    }
    fn accept(
        &self,
        p: &Principal,
        submission: ResultSubmission,
        checkpoint: bool,
    ) -> Result<Response> {
        self.authorize_assignment(p, &submission.assignment_id, submission.generation)?;
        let task: String = self.store.connection.query_row(
            "SELECT task_id FROM assignments WHERE assignment_id=?1",
            [&submission.assignment_id],
            |r| r.get(0),
        )?;
        ensure!(task == submission.task_id, "result task mismatch");
        for artifact in &submission.artifacts {
            let published:bool=self.store.connection.query_row("SELECT EXISTS(SELECT 1 FROM publications WHERE assignment_id=?1 AND generation=?2 AND sha256=?3 AND size=?4)",params![submission.assignment_id,db(submission.generation)?,artifact.sha256,db(artifact.size)?],|r|r.get(0))?;
            ensure!(published, "result references unpublished artifact");
            self.artifacts.verify(artifact)?;
        }
        let serialized = serde_json::to_string(&submission)?;
        let hash = hex::encode(Sha256::digest(serialized.as_bytes()));
        if checkpoint {
            let committed: Option<(String, String)> = self
                .store
                .connection
                .query_row(
                    "SELECT submission_json,receipt_hash FROM checkpoints WHERE task_id=?1",
                    [&task],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((json, committed_hash)) = committed {
                let committed: ResultSubmission = serde_json::from_str(&json)?;
                if committed.assignment_id == submission.assignment_id
                    && committed.generation == submission.generation
                    && committed_hash == hash
                {
                    // Acknowledges an existing strong commit after local state loss;
                    // it does not publish new progress or revive this fenced attempt.
                    return Ok(Response::Receipt {
                        receipt: Receipt {
                            task_id: task,
                            assignment_id: submission.assignment_id,
                            generation: submission.generation,
                            receipt_hash: committed_hash,
                        },
                    });
                }
            }
        }
        self.current_attempt(&submission.assignment_id, submission.generation)?;
        if self.store.status(&task)? != TaskStatus::Completed {
            let seq: u64 = self.store.connection.query_row(
                "SELECT lease_sequence FROM reservations WHERE assignment_id=?1",
                [&submission.assignment_id],
                |r| row_u64(r, 0),
            )?;
            ensure!(seq > 0, "result requires a verified authorized launch");
        }
        if checkpoint {
            ensure!(
                self.store.status(&task)? == TaskStatus::Assigned,
                "completed task cannot change checkpoint"
            );
            let sequence = submission
                .result
                .get("checkpoint_sequence")
                .and_then(serde_json::Value::as_u64)
                .filter(|v| *v > 0)
                .context("checkpoint requires positive per-attempt checkpoint_sequence")?;
            let old: Option<(String, String)> = self
                .store
                .connection
                .query_row(
                    "SELECT submission_json,receipt_hash FROM checkpoints WHERE task_id=?1",
                    [&task],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((json, old_hash)) = old {
                let old: ResultSubmission = serde_json::from_str(&json)?;
                if old.generation == submission.generation {
                    let old_seq = old
                        .result
                        .get("checkpoint_sequence")
                        .and_then(serde_json::Value::as_u64)
                        .context("stored checkpoint sequence absent")?;
                    ensure!(
                        sequence > old_seq || (sequence == old_seq && old_hash == hash),
                        "checkpoint replay would rewind or replace accepted progress"
                    );
                }
            }
            self.store.connection.execute("INSERT INTO checkpoints(task_id,submission_json,receipt_hash) VALUES (?1,?2,?3) ON CONFLICT(task_id) DO UPDATE SET submission_json=excluded.submission_json,receipt_hash=excluded.receipt_hash",params![task,serialized,hash])?;
        } else {
            // Payload and existing receipt ledger commit together, including exact lost-ACK replay.
            let tx =
                Transaction::new_unchecked(&self.store.connection, TransactionBehavior::Immediate)?;
            let current = self.store.task(&task)?;
            if current.status == TaskStatus::Completed {
                ensure!(
                    current.receipt_hash.as_deref() == Some(&hash),
                    "different result already accepted"
                );
            } else {
                ensure!(
                    current.status == TaskStatus::Assigned,
                    "task cannot accept results"
                );
                tx.execute(
                    "INSERT INTO result_payloads(task_id,submission_json) VALUES (?1,?2)",
                    params![task, serialized],
                )?;
                tx.execute(
                    "UPDATE tasks SET status='completed',receipt_hash=?2 WHERE task_id=?1",
                    params![task, hash],
                )?;
            }
            tx.commit()?;
        }
        Ok(Response::Receipt {
            receipt: Receipt {
                task_id: task,
                assignment_id: submission.assignment_id,
                generation: submission.generation,
                receipt_hash: hash,
            },
        })
    }
    fn authorize_read(&self, p: &Principal, hash: &str) -> Result<()> {
        if matches!(p, Principal::Operator) {
            return Ok(());
        }
        let Principal::Node { node_id } = p else {
            unreachable!()
        };
        let allowed:bool=self.store.connection.query_row("SELECT EXISTS(SELECT 1 FROM publications p JOIN assignments a USING(assignment_id) JOIN reservations r USING(assignment_id) WHERE p.sha256=?1 AND (r.node_id=?2 OR a.task_id IN (SELECT a2.task_id FROM reservations r2 JOIN assignments a2 USING(assignment_id) WHERE r2.node_id=?2 AND r2.phase!='released')))",params![hash,node_id],|r|r.get(0))?;
        let input_allowed:bool=self.store.connection.query_row("SELECT EXISTS(SELECT 1 FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs s USING(task_id), json_each(s.request_json,'$.input_artifacts') i WHERE r.node_id=?1 AND r.phase!='released' AND json_extract(i.value,'$.sha256')=?2)",params![node_id,hash],|r|r.get(0))?;
        ensure!(
            allowed || input_allowed,
            "artifact is not assigned to this node"
        );
        Ok(())
    }
    fn status(&self, job_id: Option<&str>) -> Result<Response> {
        let mut statement = self.store.connection.prepare(
            "SELECT task_id FROM task_specs WHERE (?1 IS NULL OR job_id=?1) ORDER BY sequence",
        )?;
        let ids = statement
            .query_map([job_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let tasks = ids
            .iter()
            .map(|id| self.store.task(id))
            .collect::<Result<Vec<_>>>()?;
        let jobs=self.json_rows("SELECT json_object('job_id',job_id,'pool_id',pool_id,'priority',priority,'cancelled',json(CASE cancelled WHEN 1 THEN 'true' ELSE 'false' END),'submitted_ms',submitted_ms,'task_ids',json((SELECT json_group_array(task_id) FROM (SELECT s.task_id FROM task_specs s WHERE s.job_id=jobs.job_id ORDER BY s.sequence)))) FROM jobs")?;
        let pools = self
            .json_rows("SELECT spec_json FROM pools")?
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let nodes=self.json_rows("SELECT json_object('report',json(report_json),'received_ms',received_ms,'drain',json(CASE drain WHEN 1 THEN 'true' ELSE 'false' END),'unrecognized_allocations',json((SELECT json_group_array(json(u.report_json)) FROM unrecognized_allocations u WHERE u.node_id=nodes.node_id))) FROM nodes")?;
        let allocations=self.json_rows("SELECT json_object('assignment_id',assignment_id,'node_id',node_id,'phase',phase,'resources',json(resources_json),'lease_sequence',lease_sequence,'lease_deadline_ms',lease_deadline_ms,'epoch',epoch,'detail',detail) FROM reservations")?;
        Ok(Response::Status {
            tasks,
            jobs,
            pools,
            nodes,
            allocations,
        })
    }
    fn json_rows(&self, sql: &str) -> Result<Vec<serde_json::Value>> {
        let mut s = self.store.connection.prepare(sql)?;
        let rows = s.query_map([], |r| r.get::<_, String>(0))?;
        let mut output = Vec::new();
        for row in rows {
            output.push(serde_json::from_str(&row?)?);
        }
        Ok(output)
    }
}
fn operator(p: &Principal) -> Result<()> {
    ensure!(
        matches!(p, Principal::Operator),
        "operator certificate required"
    );
    Ok(())
}
fn node(p: &Principal, id: &str) -> Result<()> {
    ensure!(
        matches!(p,Principal::Node{node_id} if node_id==id),
        "node certificate does not authorize this node ID"
    );
    Ok(())
}
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "invalid runtime ID"
    );
    Ok(())
}
fn phase_name(phase: RemotePhase) -> &'static str {
    match phase {
        RemotePhase::Offered => "offered",
        RemotePhase::Prepared => "prepared",
        RemotePhase::Authorized => "authorized",
        RemotePhase::Running => "running",
        RemotePhase::Draining => "draining",
        RemotePhase::Uncertain => "uncertain",
        RemotePhase::Released => "released",
    }
}
fn add(a: &mut Resources, b: &Resources) -> Result<()> {
    a.cpu_millicores = a
        .cpu_millicores
        .checked_add(b.cpu_millicores)
        .context("CPU reservation overflow")?;
    a.ram_mib = a
        .ram_mib
        .checked_add(b.ram_mib)
        .context("RAM reservation overflow")?;
    for (k, v) in &b.gpu_memory_mib {
        let e = a.gpu_memory_mib.entry(k.clone()).or_default();
        *e = e.checked_add(*v).context("GPU reservation overflow")?;
    }
    Ok(())
}
fn gpu_request_allowed(
    request: &crate::execution_model::LaunchRequest,
    report: &NodeReport,
) -> bool {
    request.resources.gpu_memory_mib.is_empty()
        || (report.gpu_expansion_allowed
            && (request.class != AllocationClass::Guaranteed || report.gpu_guaranteed_allowed)
            && request.resources.gpu_memory_mib.keys().all(|uuid| {
                report
                    .managed_budget
                    .gpu_memory_mib
                    .get(uuid)
                    .copied()
                    .unwrap_or(0)
                    > 0
            }))
}

fn fits_requested(total: &Resources, budget: &Resources, request: &Resources) -> bool {
    // CPU/RAM scopes are shared by every allocation. GPU scopes are independent
    // UUIDs: retain all charges, compare only the devices this new work would use.
    total.cpu_millicores <= budget.cpu_millicores
        && total.ram_mib <= budget.ram_mib
        && request.gpu_memory_mib.keys().all(|uuid| {
            total.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                <= budget.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
        })
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Mandatory mTLS before HTTP. The leaf fingerprint binds every request to its role.
pub async fn serve(config: CoordinatorConfig) -> Result<()> {
    use axum::{Extension, Json, Router, extract::DefaultBodyLimit, routing::post};
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
        service::TowerToHyperService,
    };
    use rustls::{RootCertStore, server::WebPkiClientVerifier};
    use std::io::BufReader;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(File::open(&config.tls.ca_cert)?)) {
        roots.add(cert?)?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(&config.tls.certificate)?))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key =
        rustls_pemfile::private_key(&mut BufReader::new(File::open(&config.tls.private_key)?))?
            .context("TLS private key absent")?;
    let tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)?;
    ensure!(
        !config.clients.is_empty(),
        "configure at least one authorized client certificate fingerprint"
    );
    for hash in config.clients.keys() {
        crate::artifacts::validate_hash(hash)?;
    }
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let allow = Arc::new(config.clients.clone());
    let state = Arc::new(Mutex::new(Coordinator::open(config)?));
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let concurrency = Arc::new(tokio::sync::Semaphore::new(64));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = concurrency.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let acceptor = acceptor.clone();
        let allow = allow.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(tls)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), acceptor.accept(stream))
                    .await
            else {
                return;
            };
            let Some(cert) = tls.get_ref().1.peer_certificates().and_then(|c| c.first()) else {
                return;
            };
            let fingerprint = hex::encode(Sha256::digest(cert.as_ref()));
            let Some(principal) = allow.get(&fingerprint).cloned() else {
                return;
            };
            let app = Router::new()
                .route(
                    "/v1/rpc",
                    post(
                        |Extension(state): Extension<Arc<Mutex<Coordinator>>>,
                         Extension(principal): Extension<Principal>,
                         Json(request): Json<Request>| async move {
                            let response = tokio::task::spawn_blocking(move || {
                                let mut state = state.lock().map_err(|_| {
                                    anyhow::anyhow!("coordinator state lock poisoned")
                                })?;
                                state.handle(&principal, request)
                            })
                            .await;
                            Json(match response {
                                Ok(Ok(response)) => response,
                                Ok(Err(e)) => Response::Error {
                                    message: format!("{e:#}"),
                                },
                                Err(_) => Response::Error {
                                    message: "coordinator request failed".into(),
                                },
                            })
                        },
                    ),
                )
                .layer(Extension(state))
                .layer(Extension(principal))
                .layer(DefaultBodyLimit::max(3 * 1024 * 1024));
            let io = TokioIo::new(tls);
            let builder = Builder::new(TokioExecutor::new());
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                builder.serve_connection(io, TowerToHyperService::new(app)),
            )
            .await;
        });
    }
}

fn db(value: u64) -> Result<i64> {
    i64::try_from(value).context("integer exceeds durable SQLite range")
}
fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let n: i64 = row.get(index)?;
    u64::try_from(n).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}

fn subtract(a: &mut Resources, b: &Resources) {
    a.cpu_millicores = a.cpu_millicores.saturating_sub(b.cpu_millicores);
    a.ram_mib = a.ram_mib.saturating_sub(b.ram_mib);
    for (k, v) in &b.gpu_memory_mib {
        let entry = a.gpu_memory_mib.entry(k.clone()).or_default();
        *entry = entry.saturating_sub(*v);
    }
}
