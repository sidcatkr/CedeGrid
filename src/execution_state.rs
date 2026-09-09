//! The existing SQLite store is also the local supervisor's durable journal.
use crate::{
    execution_model::*,
    model::{Allocation, AllocationPhase, Resources},
    state::StateStore,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

impl StateStore {
    /// Read one durable allocation after observing the owned supervisor lifecycle.
    pub fn execution_record(&self, assignment_id: &str) -> Result<Option<ExecutionRecord>> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT record_json FROM executions WHERE assignment_id=?1",
                [assignment_id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    /// Retained uncertainty remains active; only verified Released rows are excluded.
    pub fn active_executions(&self) -> Result<Vec<ExecutionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT record_json FROM executions WHERE json_extract(record_json,'$.phase')!='released' ORDER BY assignment_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn executions(&self) -> Result<Vec<ExecutionRecord>> {
        let mut stmt = self
            .connection
            .prepare("SELECT record_json FROM executions ORDER BY assignment_id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }

    /// Unknown surviving usage cannot be treated as zero. Original reservations persist.
    pub fn execution_allocations(&self) -> Result<Vec<Allocation>> {
        Ok(self
            .executions()?
            .into_iter()
            .filter(|e| e.phase != ExecutionPhase::Released)
            .map(|e| Allocation {
                id: e.assignment_id,
                class: e.class,
                phase: match e.phase {
                    ExecutionPhase::Reserved | ExecutionPhase::Prepared => AllocationPhase::Pending,
                    ExecutionPhase::Draining => AllocationPhase::Draining,
                    _ => AllocationPhase::Running,
                },
                requested: e.resources,
                observed: None,
            })
            .collect())
    }
}

impl StateStore {
    /// Distributed generation is assigned by the coordinator; the node never
    /// substitutes its local per-task counter for remote fencing identity.
    pub fn reserve_assigned(
        &self,
        request: &LaunchRequest,
        capacity: &Resources,
        generation: u64,
    ) -> Result<ExecutionRecord> {
        self.reserve_generation(request, capacity, Some(generation))
    }
    pub fn execution_request(&self, assignment: &str) -> Result<LaunchRequest> {
        let json: String = self.connection.query_row(
            "SELECT request_json FROM executions WHERE assignment_id=?1",
            [assignment],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&json)?)
    }
    /// Rebuild only the local capacity journal from a strong coordinator's
    /// fenced recovery snapshot. This imports history, never admits new work:
    /// old reservations remain charged even over current capacity or policy.
    pub fn import_replay_reservation(
        &self,
        recovery: &crate::protocol::ReplayRecoveryAllocation,
    ) -> Result<ExecutionRecord> {
        ensure!(
            self.storage_profile().is_replayable(),
            "replay recovery import requires replayable local storage"
        );
        let assignment = &recovery.assignment;
        let request = &assignment.request;
        ensure!(
            !request.task_id.is_empty() && !request.assignment_id.is_empty(),
            "recovery task and assignment identity required"
        );
        let generation = i64::try_from(assignment.generation)?;
        ensure!(
            generation > 0,
            "recovery attempt generation must be positive"
        );
        ensure!(
            request.resources.cpu_millicores > 0 && request.resources.ram_mib > 0,
            "recovery CPU and RAM reservation must be positive"
        );
        let mut record = if let Some(prepared) = &recovery.prepared {
            ensure!(
                prepared.task_id == request.task_id
                    && prepared.assignment_id == request.assignment_id
                    && prepared.generation == assignment.generation
                    && prepared.class == request.class
                    && prepared.resources == request.resources
                    && prepared.phase == ExecutionPhase::Prepared,
                "recovery preparation does not match authoritative assignment"
            );
            let identity = prepared
                .identity
                .as_ref()
                .context("recovery preparation identity absent")?;
            ensure!(
                identity.assignment_id == request.assignment_id
                    && identity.generation == assignment.generation
                    && identity.pid > 0
                    && identity.start_time > 0
                    && !identity.boot_id.trim().is_empty(),
                "invalid recovery process identity"
            );
            ensure!(
                !prepared.backend.trim().is_empty() && prepared.backend != "unprepared",
                "recovery preparation backend unverified"
            );
            ensure!(
                prepared
                    .evidence
                    .iter()
                    .all(|e| !e.control.trim().is_empty() && !e.control.starts_with("recovery.")),
                "invalid recovery preparation evidence"
            );
            for required in &request.required_controls {
                ensure!(
                    prepared.evidence.iter().any(|e| &e.control == required
                        && e.applied
                        && !e.fallback
                        && e.available == Some(true)
                        && e.permitted == Some(true)),
                    "required recovery preparation control {required} was not verified"
                );
            }
            prepared.clone()
        } else {
            ExecutionRecord {
                task_id: request.task_id.clone(),
                assignment_id: request.assignment_id.clone(),
                generation: assignment.generation,
                class: request.class,
                phase: ExecutionPhase::NeedsReconciliation,
                resources: request.resources.clone(),
                identity: None,
                backend: "unprepared".into(),
                evidence: Vec::new(),
                detail: String::new(),
            }
        };
        record.phase = ExecutionPhase::NeedsReconciliation;
        record.detail = "authoritative recovery import; original capacity retained until verified reconciliation".into();
        record.evidence.push(recovery_marker(
            "recovery.imported",
            "Imported coordinator reservation; inclusion is not absence proof",
            None,
        ));
        if recovery.prepared.is_none() {
            record.evidence.push(recovery_marker(
                "recovery.missing_prepared",
                "No authoritative preparation identity; workload absence remains unknown",
                None,
            ));
        }
        if recovery.lease_sequence == 0 {
            record.evidence.push(recovery_marker("recovery.unauthorized", "No recorded authorization lease; unacknowledged preparation or descendants remain unknown", None));
        }
        if request.managed_child_limit > 0 {
            record.evidence.push(recovery_marker("recovery.unknown_children", "Local managed-child journal was lost; parent absence cannot prove same-boot family release", None));
        }
        if let Some(boot) = &recovery.previous_boot_id {
            record.evidence.push(recovery_marker(
                "recovery.previous_boot",
                "Previous coordinator session boot identity; not a process absence proof",
                Some(boot.clone()),
            ));
        }
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM assignments WHERE assignment_id=?1)",
            [&request.assignment_id],
            |r| r.get(0),
        )?;
        ensure!(
            !existing,
            "recovery assignment already exists; refusing replacement"
        );
        let task: Option<(bool, i64, String)> = tx
            .query_row(
                "SELECT replay_safe,generation,status FROM tasks WHERE task_id=?1",
                [&request.task_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((safe, _, _)) = &task {
            ensure!(
                *safe == request.replay_safe,
                "recovery task replay-safety contract changed"
            );
        }
        // A replay-safe task may receive a newer remote generation only after
        // every imported execution is released. The execution rows carry the
        // uncertainty fence; an unsafe task also keeps the task-level fence.
        let task_status = if request.replay_safe {
            "assigned"
        } else {
            "needs_reconciliation"
        };
        if task.is_none() {
            tx.execute("INSERT INTO tasks(task_id,replay_safe,status,generation,assignment_id) VALUES (?1,?2,?3,?4,?5)", params![request.task_id,request.replay_safe,task_status,generation,request.assignment_id])?;
        } else if let Some((_, latest, status)) = task {
            ensure!(
                status != "completed",
                "recovery cannot replace a completed local task"
            );
            if generation > latest {
                tx.execute(
                    "UPDATE tasks SET status=?2,generation=?3,assignment_id=?4 WHERE task_id=?1",
                    params![
                        request.task_id,
                        task_status,
                        generation,
                        request.assignment_id
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE tasks SET status=?2 WHERE task_id=?1",
                    params![request.task_id, task_status],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO assignments(assignment_id,task_id,generation) VALUES (?1,?2,?3)",
            params![request.assignment_id, request.task_id, generation],
        )?;
        let json = serde_json::to_string(&record)?;
        tx.execute(
            "INSERT INTO executions(assignment_id,request_json,record_json) VALUES (?1,?2,?3)",
            params![request.assignment_id, serde_json::to_string(request)?, json],
        )?;
        tx.execute(
            "INSERT INTO execution_events(assignment_id,record_json) VALUES (?1,?2)",
            params![request.assignment_id, json],
        )?;
        tx.commit()?;
        Ok(record)
    }

    fn reserve_generation(
        &self,
        request: &LaunchRequest,
        capacity: &Resources,
        remote_generation: Option<u64>,
    ) -> Result<ExecutionRecord> {
        if self.storage_profile().is_replayable() {
            ensure!(
                remote_generation.is_some(),
                "replayable local storage requires a coordinator-assigned generation"
            );
            ensure!(
                request.replay_safe && request.class == AllocationClass::Opportunistic,
                "replayable local storage admits only replay-safe opportunistic work"
            );
            ensure!(
                !request
                    .required_controls
                    .iter()
                    .any(|control| control == "storage.durable_local"),
                "replayable local storage cannot provide storage.durable_local"
            );
        }
        ensure!(
            !request.task_id.is_empty() && !request.assignment_id.is_empty(),
            "task and assignment identity required"
        );
        ensure!(
            request.resources.cpu_millicores > 0 && request.resources.ram_mib > 0,
            "CPU and RAM reservations must be positive"
        );

        ensure!(
            request.max_attempts.is_none_or(|limit| limit > 0),
            "max_attempts must be positive"
        );
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let previous_request:Option<String>=tx.query_row("SELECT e.request_json FROM executions e JOIN assignments a USING(assignment_id) WHERE a.task_id=?1 ORDER BY a.generation LIMIT 1",[&request.task_id],|r|r.get(0)).optional()?;
        if let Some(old) = previous_request {
            ensure!(
                serde_json::from_str::<LaunchRequest>(&old)?.max_attempts == request.max_attempts,
                "task attempt budget is immutable across retries"
            );
        }
        let mut charged = request.resources.clone();
        {
            let mut stmt = tx.prepare("SELECT record_json FROM executions")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let old: ExecutionRecord = serde_json::from_str(&row?)?;
                if old.phase != ExecutionPhase::Released {
                    ensure!(
                        old.task_id != request.task_id,
                        "prior allocation remains reserved; reconciliation required before replacing this task"
                    );
                    add(&mut charged, &old.resources)?;
                }
            }
        }
        ensure!(
            fits_requested(&charged, capacity, &request.resources),
            "insufficient capacity including pending and uncertain reservations"
        );
        tx.execute("INSERT OR IGNORE INTO tasks(task_id,replay_safe,status,generation) VALUES (?1,?2,'queued',0)", params![request.task_id, request.replay_safe])?;
        let (safe, status, generation): (bool, String, i64) = tx.query_row(
            "SELECT replay_safe,status,generation FROM tasks WHERE task_id=?1",
            [&request.task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            safe == request.replay_safe,
            "task replay-safety contract changed"
        );
        ensure!(
            status == "queued"
                || (remote_generation.is_some_and(|g| g > generation as u64)
                    && safe
                    && status == "assigned"),
            "task retry contract or state prevents assignment"
        );
        let next = generation
            .checked_add(1)
            .context("attempt generation overflow")?;
        let generation = match remote_generation {
            Some(remote) => {
                let remote = i64::try_from(remote)?;
                ensure!(remote >= next, "stale remote generation");
                remote
            }
            None => next,
        };
        ensure!(
            request
                .max_attempts
                .is_none_or(|limit| generation <= i64::from(limit)),
            "task attempt budget exhausted before local reservation"
        );
        tx.execute(
            "INSERT INTO assignments(assignment_id,task_id,generation) VALUES (?1,?2,?3)",
            params![request.assignment_id, request.task_id, generation],
        )?;
        tx.execute(
            "UPDATE tasks SET status='assigned',generation=?2,assignment_id=?3 WHERE task_id=?1",
            params![request.task_id, generation, request.assignment_id],
        )?;
        let record = ExecutionRecord {
            task_id: request.task_id.clone(),
            assignment_id: request.assignment_id.clone(),
            generation: generation.try_into()?,
            class: request.class,
            phase: ExecutionPhase::Reserved,
            resources: request.resources.clone(),
            identity: None,
            backend: "unprepared".into(),
            evidence: Vec::new(),
            detail: "capacity reserved before launch preparation".into(),
        };
        let json = serde_json::to_string(&record)?;
        tx.execute(
            "INSERT INTO executions(assignment_id,request_json,record_json) VALUES (?1,?2,?3)",
            params![record.assignment_id, serde_json::to_string(request)?, json],
        )?;
        tx.execute(
            "INSERT INTO execution_events(assignment_id,record_json) VALUES (?1,?2)",
            params![record.assignment_id, json],
        )?;
        tx.commit()?;
        Ok(record)
    }
}

impl ExecutionJournal for StateStore {
    fn namespace_guard(&self) -> Option<crate::namespace::NamespaceGuard> {
        Some(StateStore::namespace_guard(self))
    }
    fn storage_control_evidence(&self) -> Result<Vec<ControlEvidence>> {
        let settings = self.durability_settings()?;
        let replayable = settings.profile.is_replayable();
        ensure!(
            settings
                .journal_mode
                .eq_ignore_ascii_case(settings.profile.journal_mode())
                && settings.synchronous == settings.profile.synchronous()
                && settings.schema_version == settings.profile.schema_version()
                && settings.foreign_keys,
            "effective storage settings changed before control evidence"
        );
        Ok(vec![ControlEvidence {
            control: if replayable {
                "storage.replayable_local"
            } else {
                "storage.durable_local"
            }
            .into(),
            available: Some(true),
            permitted: Some(true),
            configured: true,
            applied: true,
            fallback: false,
            scope: "local_state".into(),
            requested: Some(settings.profile.name().into()),
            effective: Some(settings.assurance.name().into()),
            detail: settings.profile.assurance().detail().into(),
        }])
    }
    fn reserve_managed_child(
        &self,
        assignment_id: &str,
        generation: u64,
        child_id: &str,
        request_id: &str,
        request: &LaunchRequest,
    ) -> Result<crate::managed_children::ManagedChildRecord> {
        StateStore::reserve_managed_child(
            self,
            assignment_id,
            generation,
            child_id,
            request_id,
            request,
        )
    }
    fn managed_child(
        &self,
        assignment_id: &str,
        request_id: &str,
    ) -> Result<Option<crate::managed_children::ManagedChildRecord>> {
        StateStore::managed_child(self, assignment_id, request_id)
    }
    fn managed_children(
        &self,
        assignment_id: &str,
    ) -> Result<Vec<crate::managed_children::ManagedChildRecord>> {
        StateStore::managed_children(self, assignment_id)
    }
    fn prepare_managed_child(
        &self,
        child_id: &str,
        identity: &ProcessIdentity,
        evidence: &[ControlEvidence],
    ) -> Result<()> {
        StateStore::prepare_managed_child(self, child_id, identity, evidence)
    }
    fn transition_managed_child(
        &self,
        child_id: &str,
        phase: crate::managed_children::ManagedChildPhase,
        exit_code: Option<i32>,
        signal: Option<i32>,
        detail: &str,
    ) -> Result<()> {
        StateStore::transition_managed_child(self, child_id, phase, exit_code, signal, detail)
    }

    fn reserve(&self, request: &LaunchRequest, capacity: &Resources) -> Result<ExecutionRecord> {
        self.reserve_generation(request, capacity, None)
    }

    fn transition(&self, record: &ExecutionRecord) -> Result<()> {
        self.ensure_managed_children_schema()?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let (old_json, request_json): (String, String) = tx
            .query_row(
                "SELECT record_json,request_json FROM executions WHERE assignment_id=?1",
                [&record.assignment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("unreserved assignment")?;
        let old: ExecutionRecord = serde_json::from_str(&old_json)?;
        let request: LaunchRequest = serde_json::from_str(&request_json)?;
        ensure!(
            old.task_id == record.task_id
                && old.generation == record.generation
                && old.class == record.class
                && old.resources == record.resources,
            "assignment identity or reservation changed"
        );
        ensure!(
            valid_transition(&old.phase, &record.phase),
            "invalid execution transition {:?} -> {:?}",
            old.phase,
            record.phase
        );
        if old.phase != ExecutionPhase::Reserved {
            ensure!(
                old.backend == record.backend,
                "verified execution backend changed"
            );
        }
        if let Some(identity) = &old.identity {
            ensure!(
                record.identity.as_ref() == Some(identity),
                "verified process identity changed"
            );
        }
        if let Some(identity) = &record.identity {
            ensure!(
                identity.assignment_id == record.assignment_id
                    && identity.generation == record.generation
                    && identity.pid > 0
                    && !identity.boot_id.is_empty()
                    && identity.start_time > 0,
                "invalid persisted process identity"
            );
        }
        if matches!(
            record.phase,
            ExecutionPhase::Prepared | ExecutionPhase::Authorized | ExecutionPhase::Running
        ) {
            ensure!(
                record.identity.is_some() && record.backend != "unprepared",
                "launch barrier requires identity and backend verification"
            );
        }
        if matches!(
            record.phase,
            ExecutionPhase::Authorized | ExecutionPhase::Running
        ) {
            for required in &request.required_controls {
                ensure!(
                    record.evidence.iter().any(|e| &e.control == required
                        && e.applied
                        && !e.fallback
                        && e.available == Some(true)
                        && e.permitted == Some(true)),
                    "required control {required} not verified at launch barrier"
                );
            }
        }
        for marker in old
            .evidence
            .iter()
            .filter(|e| e.control.starts_with("recovery."))
        {
            ensure!(
                record.evidence.contains(marker),
                "imported recovery evidence cannot be removed or changed"
            );
        }
        if record.phase == ExecutionPhase::Released {
            ensure!(
                !record
                    .evidence
                    .iter()
                    .any(|e| e.control == "recovery.missing_prepared"),
                "recovery without preparation identity cannot prove release"
            );
            if (self.storage_profile().is_replayable()
                && request.managed_child_limit > 0
                && old.phase == ExecutionPhase::NeedsReconciliation)
                || record.evidence.iter().any(|e| {
                    matches!(
                        e.control.as_str(),
                        "recovery.unauthorized" | "recovery.unknown_children"
                    )
                })
            {
                let identity = old
                    .identity
                    .as_ref()
                    .context("recovery release identity absent")?;
                let current =
                    crate::supervision::process_identity(std::process::id(), "recovery", 0)?;
                ensure!(
                    identity.boot_id != current.boot_id,
                    "same-boot recovery cannot prove release of an unacknowledged or unknown process family"
                );
            }
            let held:u32=tx.query_row("SELECT count(*) FROM managed_children WHERE assignment_id=?1 AND json_extract(record_json,'$.phase')!='released'",[&record.assignment_id],|r|r.get(0))?;
            ensure!(
                held == 0,
                "managed family retains unreleased or uncertain children; parent reservation cannot be released"
            );
        }
        let json = serde_json::to_string(record)?;
        tx.execute(
            "UPDATE executions SET record_json=?2 WHERE assignment_id=?1",
            params![record.assignment_id, json],
        )?;
        tx.execute(
            "INSERT INTO execution_events(assignment_id,record_json) VALUES (?1,?2)",
            params![record.assignment_id, json],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn valid_transition(from: &ExecutionPhase, to: &ExecutionPhase) -> bool {
    use ExecutionPhase::*;
    matches!(
        (from, to),
        (Reserved, Prepared | NeedsReconciliation | Released)
            | (Prepared, Authorized | NeedsReconciliation | Released)
            | (
                Authorized,
                Running | Draining | NeedsReconciliation | Released
            )
            | (Running, Draining | NeedsReconciliation | Released)
            | (Draining, NeedsReconciliation | Released)
            | (NeedsReconciliation, NeedsReconciliation | Released)
            | (Released, Released)
    )
}
fn add(total: &mut Resources, next: &Resources) -> Result<()> {
    total.cpu_millicores = total
        .cpu_millicores
        .checked_add(next.cpu_millicores)
        .context("CPU reservation overflow")?;
    total.ram_mib = total
        .ram_mib
        .checked_add(next.ram_mib)
        .context("RAM reservation overflow")?;
    for (id, amount) in &next.gpu_memory_mib {
        let value = total.gpu_memory_mib.entry(id.clone()).or_default();
        *value = value
            .checked_add(*amount)
            .context("GPU reservation overflow")?;
    }
    Ok(())
}
fn fits_requested(used: &Resources, cap: &Resources, request: &Resources) -> bool {
    // Keep every pending/uncertain reservation charged. CPU/RAM are shared by
    // all work; GPU budgets are independent admission scopes. An unavailable
    // unrelated UUID must not block a CPU-only or independently eligible GPU
    // request, while a request for that UUID still includes its full old charge.
    used.cpu_millicores <= cap.cpu_millicores
        && used.ram_mib <= cap.ram_mib
        && request.gpu_memory_mib.keys().all(|id| {
            used.gpu_memory_mib.get(id).copied().unwrap_or(0)
                <= cap.gpu_memory_mib.get(id).copied().unwrap_or(0)
        })
}

fn recovery_marker(control: &str, detail: &str, effective: Option<String>) -> ControlEvidence {
    ControlEvidence {
        control: control.into(),
        available: None,
        permitted: None,
        configured: true,
        applied: false,
        fallback: false,
        scope: "recovery".into(),
        requested: None,
        effective,
        detail: detail.into(),
    }
}
