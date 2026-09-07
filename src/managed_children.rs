//! Durable identities for explicitly mediated, bounded child processes.
//!
//! This registry never adopts a PID or sends a signal. The supervisor owns direct
//! child handles, verifies backend membership, and confirms release before writing
//! Released. A whole managed family consumes its existing parent reservation once.
use crate::{
    execution_model::{
        ControlEvidence, ExecutionPhase, ExecutionRecord, LaunchRequest, ProcessIdentity,
    },
    state::StateStore,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChildPhase {
    Reserved,
    Prepared,
    Authorized,
    Running,
    Draining,
    Released,
    NeedsReconciliation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedChildRecord {
    pub child_id: String,
    pub request_id: String,
    pub assignment_id: String,
    pub generation: u64,
    pub request: LaunchRequest,
    pub identity: Option<ProcessIdentity>,
    pub phase: ManagedChildPhase,
    pub controls: Vec<ControlEvidence>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub detail: String,
}
impl StateStore {
    pub(crate) fn ensure_managed_children_schema(&self) -> Result<()> {
        self.connection.execute_batch("CREATE TABLE IF NOT EXISTS managed_children (
   child_id TEXT PRIMARY KEY NOT NULL,assignment_id TEXT NOT NULL REFERENCES executions(assignment_id),request_id TEXT NOT NULL,record_json TEXT NOT NULL CHECK(json_valid(record_json)),UNIQUE(assignment_id,request_id)
  ) STRICT;
  CREATE TABLE IF NOT EXISTS managed_child_events(id INTEGER PRIMARY KEY,child_id TEXT NOT NULL REFERENCES managed_children(child_id),record_json TEXT NOT NULL CHECK(json_valid(record_json))) STRICT;
  CREATE UNIQUE INDEX IF NOT EXISTS managed_child_identity ON managed_children(json_extract(record_json,'$.identity.boot_id'),json_extract(record_json,'$.identity.pid'),json_extract(record_json,'$.identity.start_time')) WHERE json_extract(record_json,'$.identity') IS NOT NULL;")?;
        Ok(())
    }
    pub fn reserve_managed_child(
        &self,
        assignment_id: &str,
        generation: u64,
        child_id: &str,
        request_id: &str,
        request: &LaunchRequest,
    ) -> Result<ManagedChildRecord> {
        self.ensure_managed_children_schema()?;
        valid_id(child_id)?;
        valid_id(request_id)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some(old) = child_by_request(&tx, assignment_id, request_id)? {
            ensure!(
                old.generation == generation
                    && serde_json::to_value(&old.request)? == serde_json::to_value(request)?,
                "managed child request ID reused with changed generation or command"
            );
            return Ok(old);
        }
        let (parent, parent_request) = parent(&tx, assignment_id)?;
        ensure!(
            parent.generation == generation
                && matches!(
                    parent.phase,
                    ExecutionPhase::Authorized | ExecutionPhase::Running
                ),
            "parent is not authorizing new managed children"
        );
        ensure!(
            !parent_request.single_process
                && parent_request.no_escape
                && (1..=8).contains(&parent_request.managed_child_limit),
            "parent did not opt into bounded mediated children"
        );
        ensure!(
            request.task_id == parent.task_id
                && request.assignment_id == assignment_id
                && request.class == parent.class
                && request.replay_safe == parent_request.replay_safe,
            "child cannot change parent assignment, class or retry contract"
        );
        ensure!(
            request.single_process && request.no_escape && request.managed_child_limit == 0,
            "managed children must acknowledge single-process/no-daemon/no-nested-child contract"
        );
        ensure!(
            request.cwd.is_absolute() && !request.argv.is_empty() && !request.argv[0].is_empty(),
            "invalid managed child command"
        );
        ensure!(
            request.resources.cpu_millicores > 0
                && request.resources.ram_mib > 0
                && request.resources.cpu_millicores <= parent.resources.cpu_millicores
                && request.resources.ram_mib <= parent.resources.ram_mib
                && request.resources.gpu_memory_mib.iter().all(|(uuid, size)| {
                    *size > 0
                        && parent
                            .resources
                            .gpu_memory_mib
                            .get(uuid)
                            .is_some_and(|limit| size <= limit)
                }),
            "child request exceeds existing family reservation"
        );
        ensure!(
            request
                .input_artifacts
                .iter()
                .all(|input| parent_request.input_artifacts.contains(input)),
            "child cannot name inputs outside the parent assignment"
        );
        ensure!(
            parent_request
                .required_controls
                .iter()
                .all(|control| request.required_controls.contains(control)),
            "child cannot omit parent-required controls"
        );
        let held:u32=tx.query_row("SELECT count(*) FROM managed_children WHERE assignment_id=?1 AND json_extract(record_json,'$.phase')!='released'",[assignment_id],|r|r.get(0))?;
        ensure!(
            held < u32::from(parent_request.managed_child_limit),
            "managed child limit includes pending and uncertain children"
        );
        let record = ManagedChildRecord {
            child_id: child_id.into(),
            request_id: request_id.into(),
            assignment_id: assignment_id.into(),
            generation,
            request: request.clone(),
            identity: None,
            phase: ManagedChildPhase::Reserved,
            controls: vec![],
            exit_code: None,
            signal: None,
            detail: "child reserved inside existing family allocation; user code remains blocked"
                .into(),
        };
        let json = serde_json::to_string(&record)?;
        tx.execute("INSERT INTO managed_children(child_id,assignment_id,request_id,record_json) VALUES (?1,?2,?3,?4)",params![child_id,assignment_id,request_id,json])?;
        event(&tx, &record)?;
        tx.commit()?;
        Ok(record)
    }
    pub fn managed_child(
        &self,
        assignment_id: &str,
        request_id: &str,
    ) -> Result<Option<ManagedChildRecord>> {
        self.ensure_managed_children_schema()?;
        child_by_request(&self.connection, assignment_id, request_id)
    }
    pub fn managed_children(&self, assignment_id: &str) -> Result<Vec<ManagedChildRecord>> {
        self.ensure_managed_children_schema()?;
        let mut s = self.connection.prepare(
            "SELECT record_json FROM managed_children WHERE assignment_id=?1 ORDER BY child_id",
        )?;
        let rows = s.query_map([assignment_id], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn all_unreleased_managed_children(&self) -> Result<Vec<ManagedChildRecord>> {
        self.ensure_managed_children_schema()?;
        let mut s=self.connection.prepare("SELECT record_json FROM managed_children WHERE json_extract(record_json,'$.phase')!='released' ORDER BY child_id")?;
        let rows = s.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn prepare_managed_child(
        &self,
        child_id: &str,
        identity: &ProcessIdentity,
        evidence: &[ControlEvidence],
    ) -> Result<()> {
        self.ensure_managed_children_schema()?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut record = child(&tx, child_id)?;
        ensure!(
            identity.pid > 0
                && identity.start_time > 0
                && !identity.boot_id.is_empty()
                && identity.assignment_id == record.assignment_id
                && identity.generation == record.generation,
            "invalid managed child identity"
        );
        let (parent, _) = parent(&tx, &record.assignment_id)?;
        let parent_identity = parent
            .identity
            .as_ref()
            .context("parent identity missing")?;
        ensure!(
            identity.boot_id == parent_identity.boot_id && identity.pid != parent_identity.pid,
            "child identity is not distinct from its parent on the same boot"
        );
        if record.identity.is_some() {
            ensure!(
                record.identity.as_ref() == Some(identity) && record.controls == evidence,
                "managed child preparation identity or controls changed"
            );
            return Ok(());
        }
        ensure!(
            record.phase == ManagedChildPhase::Reserved
                && parent.generation == record.generation
                && matches!(
                    parent.phase,
                    ExecutionPhase::Authorized | ExecutionPhase::Running
                ),
            "child preparation no longer authorized"
        );
        let leader:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM executions WHERE json_extract(record_json,'$.identity.boot_id')=?1 AND json_extract(record_json,'$.identity.pid')=?2 AND json_extract(record_json,'$.identity.start_time')=?3)",params![identity.boot_id,identity.pid,i64::try_from(identity.start_time)?],|r|r.get(0))?;
        ensure!(
            !leader,
            "a managed allocation leader cannot also be registered as a child"
        );
        verify_controls(&record, evidence)?;
        record.identity = Some(identity.clone());
        record.controls = evidence.to_vec();
        record.phase = ManagedChildPhase::Prepared;
        record.detail =
            "direct child identity and required controls verified before authorization".into();
        save(&tx, &record)?;
        tx.commit()?;
        Ok(())
    }
    pub fn transition_managed_child(
        &self,
        child_id: &str,
        phase: ManagedChildPhase,
        exit_code: Option<i32>,
        signal: Option<i32>,
        detail: &str,
    ) -> Result<()> {
        self.ensure_managed_children_schema()?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut record = child(&tx, child_id)?;
        ensure!(
            exit_code.is_none() || signal.is_none(),
            "exit code and signal are mutually exclusive"
        );
        ensure!(signal.is_none_or(|s| s > 0), "invalid termination signal");
        ensure!(
            phase == ManagedChildPhase::Released || (exit_code.is_none() && signal.is_none()),
            "exit status requires confirmed child release"
        );
        ensure!(
            valid_transition(record.phase, phase),
            "invalid managed child lifecycle transition {:?} -> {:?}",
            record.phase,
            phase
        );
        if record.phase == ManagedChildPhase::Released {
            ensure!(
                record.exit_code == exit_code && record.signal == signal,
                "released child termination receipt changed"
            );
            return Ok(());
        }
        if matches!(
            phase,
            ManagedChildPhase::Authorized | ManagedChildPhase::Running
        ) {
            ensure!(
                record.identity.is_some(),
                "managed child launch requires verified identity"
            );
            verify_controls(&record, &record.controls)?;
            let (parent, _) = parent(&tx, &record.assignment_id)?;
            ensure!(
                parent.generation == record.generation
                    && matches!(
                        parent.phase,
                        ExecutionPhase::Authorized | ExecutionPhase::Running
                    ),
                "parent is draining or uncertain; child execution cannot start"
            );
        }
        record.phase = phase;
        record.exit_code = exit_code;
        record.signal = signal;
        record.detail = detail.into();
        save(&tx, &record)?;
        tx.commit()?;
        Ok(())
    }
}
fn parent(connection: &rusqlite::Connection, id: &str) -> Result<(ExecutionRecord, LaunchRequest)> {
    let (record, request): (String, String) = connection
        .query_row(
            "SELECT record_json,request_json FROM executions WHERE assignment_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("unknown parent execution")?;
    Ok((
        serde_json::from_str(&record)?,
        serde_json::from_str(&request)?,
    ))
}
fn child(connection: &rusqlite::Connection, id: &str) -> Result<ManagedChildRecord> {
    let json: String = connection
        .query_row(
            "SELECT record_json FROM managed_children WHERE child_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown managed child")?;
    Ok(serde_json::from_str(&json)?)
}
fn child_by_request(
    connection: &rusqlite::Connection,
    assignment: &str,
    request: &str,
) -> Result<Option<ManagedChildRecord>> {
    let json: Option<String> = connection
        .query_row(
            "SELECT record_json FROM managed_children WHERE assignment_id=?1 AND request_id=?2",
            params![assignment, request],
            |r| r.get(0),
        )
        .optional()?;
    json.map(|json| Ok(serde_json::from_str(&json)?))
        .transpose()
}
fn event(tx: &Transaction<'_>, record: &ManagedChildRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO managed_child_events(child_id,record_json) VALUES (?1,?2)",
        params![record.child_id, serde_json::to_string(record)?],
    )?;
    Ok(())
}
fn save(tx: &Transaction<'_>, record: &ManagedChildRecord) -> Result<()> {
    tx.execute(
        "UPDATE managed_children SET record_json=?2 WHERE child_id=?1",
        params![record.child_id, serde_json::to_string(record)?],
    )?;
    event(tx, record)
}
fn verify_controls(record: &ManagedChildRecord, evidence: &[ControlEvidence]) -> Result<()> {
    for required in &record.request.required_controls {
        ensure!(
            evidence.iter().any(|e| &e.control == required
                && e.applied
                && !e.fallback
                && e.available == Some(true)
                && e.permitted == Some(true)),
            "managed child required control not applied: {required}"
        );
    }
    Ok(())
}
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "invalid managed child/request ID"
    );
    Ok(())
}
fn valid_transition(from: ManagedChildPhase, to: ManagedChildPhase) -> bool {
    use ManagedChildPhase::*;
    from == to
        || matches!(
            (from, to),
            (Reserved, Prepared | NeedsReconciliation | Released)
                | (Prepared, Authorized | NeedsReconciliation | Released)
                | (
                    Authorized,
                    Running | Draining | NeedsReconciliation | Released
                )
                | (Running, Draining | NeedsReconciliation | Released)
                | (Draining, NeedsReconciliation | Released)
                | (NeedsReconciliation, Released)
        )
}
