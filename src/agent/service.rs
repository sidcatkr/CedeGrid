//! The node service loop: each interval samples, evaluates policy, drains,
//! reports and launches.
use super::*;

/// What the agent's launch slots depend on in one heartbeat.
#[derive(Debug, Clone, Copy)]
pub struct LaunchPacing {
    pub max_workers: usize,
    pub active: usize,
    /// Workers admitted so far; parallelism grows from here.
    pub admitted: usize,
    /// The CPU and RAM cooldown since the last growth has passed.
    pub may_grow: bool,
    /// CPU runs in Performance.
    pub fill: bool,
}

/// New assignments one heartbeat may take in any mode. Each needs a launch and an initial
/// authorization; 32 still fills a 16-task scope in one Performance heartbeat (research R19).
pub const LAUNCHES_PER_HEARTBEAT: usize = 32;

/// Launch preparations that run at once on the blocking pool; each makes synced writes.
const LAUNCH_CONCURRENCY: usize = 4;

/// How many new assignments one heartbeat may take. Parallelism grows by one
/// worker per CPU and RAM cooldown, except in Performance, where one heartbeat may
/// fill free worker slots so the coordinator admits what fits within the
/// allowance (FR-034), up to [`LAUNCHES_PER_HEARTBEAT`].
pub fn launch_slots(pacing: LaunchPacing) -> usize {
    let free = pacing
        .max_workers
        .saturating_sub(pacing.active)
        .min(LAUNCHES_PER_HEARTBEAT);
    if pacing.fill && pacing.may_grow {
        free
    } else {
        free.min(pacing.admitted.saturating_sub(pacing.active) + usize::from(pacing.may_grow))
    }
}

/// Waiting lease requests, longest waiting first.
pub(super) fn lease_order(waiting: &mut [(Instant, String)]) {
    waiting.sort();
}

/// One iteration's lease batches: renewals first, earliest deadline first, then initial
/// authorizations, longest waiting first, each batch at most `MAX_LEASE_BATCH_ITEMS` items and
/// `MAX_LEASE_BATCH_BYTES` (FR-074). A renewal's instant is its last renewal, so its deadline.
pub(super) fn lease_batches(
    mut renewals: Vec<(Instant, LeaseItem)>,
    mut initial: Vec<(Instant, LeaseItem)>,
) -> Vec<Vec<LeaseItem>> {
    renewals.sort_by_key(|(at, _)| *at);
    initial.sort_by_key(|(at, _)| *at);
    let mut batches: Vec<Vec<LeaseItem>> = Vec::new();
    let mut bytes = 0usize;
    for (_, item) in renewals.into_iter().chain(initial) {
        let size = serde_json::to_vec(&item).map_or(MAX_LEASE_BATCH_BYTES, |json| json.len());
        let full = batches.last().is_none_or(|batch| {
            batch.len() >= MAX_LEASE_BATCH_ITEMS || bytes + size > MAX_LEASE_BATCH_BYTES
        });
        if full {
            batches.push(Vec::new());
            bytes = 0;
        }
        bytes += size;
        if let Some(batch) = batches.last_mut() {
            batch.push(item);
        }
    }
    batches
}

/// A batch item's result as the single request's, so its events keep their bytes.
pub(super) fn item_result(result: LeaseResult) -> Result<Response> {
    match result {
        LeaseResult::Lease { lease } => Ok(Response::Lease { lease }),
        LeaseResult::Error { message } => {
            Err(anyhow::anyhow!("coordinator rejected request: {message}"))
        }
    }
}

/// An initial authorization's reply: the lease goes to the supervisor, anything else is
/// reported.
fn authorization_reply(
    slot: &mut ChildSlot,
    requested: Instant,
    result: Result<Response>,
) -> Result<()> {
    match result {
        Ok(Response::Lease { lease }) => {
            let remaining_ms = lease
                .valid_for_ms
                .saturating_sub(requested.elapsed().as_millis() as u64);
            if remaining_ms > 0 {
                let seq = lease.sequence;
                let _ = send(
                    slot,
                    &Control::Authorize {
                        lease,
                        expires_monotonic_ms: local_clock_ms()?.saturating_add(remaining_ms),
                    },
                );
                slot.sequence = seq;
                slot.last_renew = Instant::now();
            }
        }
        result => {
            emit(
                &serde_json::json!({"event":"authorization_unavailable","assignment_id":slot.assignment.request.assignment_id,"detail":format!("{result:?}")}),
            )?;
        }
    }
    Ok(())
}

/// A renewal's reply: the lease goes to the supervisor, anything else is reported.
fn renewal_reply(slot: &mut ChildSlot, requested: Instant, result: Result<Response>) -> Result<()> {
    match result {
        Ok(Response::Lease { lease }) => {
            let remaining_ms = lease
                .valid_for_ms
                .saturating_sub(requested.elapsed().as_millis() as u64);
            if remaining_ms > 0 {
                let seq = lease.sequence;
                let _ = send(
                    slot,
                    &Control::Renewal {
                        lease,
                        expires_monotonic_ms: local_clock_ms()?.saturating_add(remaining_ms),
                    },
                );
                slot.sequence = seq;
            }
        }
        result => {
            emit(
                &serde_json::json!({"event":"lease_unavailable","assignment_id":slot.assignment.request.assignment_id,"detail":format!("{result:?}")}),
            )?;
        }
    }
    Ok(())
}

/// Whether a slot's initial authorization is requested: prepared, not yet authorized, its
/// supervisor alive, and not cancelled by a drain (FR-012).
pub(super) fn wants_authorization(slot: &ChildSlot) -> bool {
    slot.prepared.is_some() && slot.sequence == 0 && !slot.exited && slot.drain_sent_at.is_none()
}

/// An allocation whose journal still reads Prepared after its authorization went out is no
/// longer the pending pass's to cancel.
pub(super) fn withhold_authorized_pending(
    facts: &mut BTreeMap<String, crate::model::AllocationFacts>,
    allocations: &[crate::model::Allocation],
    authorized: impl Fn(&str) -> bool,
) {
    for allocation in allocations {
        if allocation.phase == crate::model::AllocationPhase::Pending
            && authorized(&allocation.id)
            && let Some(facts) = facts.get_mut(&allocation.id)
        {
            facts.uncontrollable = true;
        }
    }
}

/// Whether one more lease request goes out this iteration: the longest-waiting one always
/// does, the rest while the budget lasts.
pub(super) fn lease_turn(sent: usize, elapsed: Duration, budget: Duration) -> bool {
    sent == 0 || elapsed < budget
}

/// Whether a slot's publication check takes an attempt pin, which costs a state connection
/// and two synced writes: for a released result, or for a checkpoint head that is unreadable
/// (the pinned path reports it) or not yet published. `head` is the head's hash, `None` when
/// it could not be read.
pub(super) fn needs_pin(
    checkpoint: bool,
    released: bool,
    head: Option<Option<&str>>,
    published: Option<&str>,
) -> bool {
    if !checkpoint {
        return released;
    }
    match head {
        None => true,
        Some(None) => false,
        Some(Some(hash)) => published != Some(hash),
    }
}

type Publication = tokio::task::JoinHandle<(String, bool, String, Result<()>)>;

type Preparation = (Assignment, tokio::task::JoinHandle<Result<ChildSlot>>, bool);

/// State of one agent service loop. Each stage of an interval is a method.
struct AgentLoop<'a> {
    config: &'a Config,
    agent: &'a AgentConfig,
    executable: &'a Path,
    store: StateStore,
    client: std::sync::Arc<NodeClient>,
    namespace_guard: NamespaceGuard,
    collector: ManagedCollector,
    policy: PolicyEngine,
    slots: BTreeMap<String, ChildSlot>,
    refusals: BTreeMap<String, (Assignment, String)>,
    publications: Vec<Publication>,
    publishing: BTreeSet<(String, bool)>,
    preparations: BTreeMap<String, Preparation>,
    start: Instant,
    last_heartbeat: Instant,
    negotiated: NegotiatedFeatures,
    stopping: Option<Instant>,
    last_spool_check: Instant,
    spool_full: bool,
    admitted_parallelism: usize,
    last_growth: Instant,
    /// Slots whose drain already reduced the admitted parallelism.
    counted_drains: BTreeSet<String>,
    /// Host floor warnings already printed.
    floor_warnings: BTreeSet<String>,
    /// Whether the first sample passed the host-dependent configuration checks.
    host_checked: bool,
    /// Bounds the launch preparations that run at once.
    launch_gate: std::sync::Arc<tokio::sync::Semaphore>,
}

impl AgentLoop<'_> {
    /// Finished preparations become slots; refused launches get a durable refusal.
    async fn collect_preparations(&mut self) -> Result<()> {
        let config = self.config;
        let ready: Vec<_> = self
            .preparations
            .iter()
            .filter(|(_, (_, h, _))| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let (assignment, handle, revoked) =
                self.preparations.remove(&id).expect("listed preparation");
            match handle.await.context("assignment preparation task failed")? {
                Ok(mut slot) => {
                    if revoked || self.stopping.is_some() {
                        drain(&mut slot, None);
                    }
                    self.slots.insert(id, slot);
                }
                Err(error) => {
                    let pair = (
                        assignment,
                        format!("launch refused before execution: {error:#}"),
                    );
                    let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                    if !proof.exists() {
                        write_json_new(&proof, &pair)?;
                    }
                    self.refusals.insert(id.clone(), pair);
                    emit(
                        &serde_json::json!({"event":"launch_refused","assignment_id":id,"detail":format!("{error:#}")}),
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn collect_publications(&mut self) -> Result<()> {
        for handle_index in (0..self.publications.len()).rev() {
            if self.publications[handle_index].is_finished() {
                let (id, checkpoint, hash, result) =
                    self.publications.swap_remove(handle_index).await?;
                self.publishing.remove(&(id.clone(), checkpoint));
                match result {
                    Ok(()) => {
                        if let Some(slot) = self.slots.get_mut(&id) {
                            if checkpoint {
                                slot.checkpoint_hash = Some(hash);
                                slot.last_checkpoint_at = Some(Instant::now());
                            } else {
                                slot.reported = true;
                            }
                        }
                    }
                    Err(error) => {
                        emit(
                            &serde_json::json!({"event":"publication_retry","assignment_id":id,"checkpoint":checkpoint,"detail":format!("{error:#}")}),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Slot lifecycle: supervisor events, authorization, lease renewal, result
    /// publication and reaping, one pass over every owned slot.
    async fn service_slots(&mut self) -> Result<()> {
        let config = self.config;
        let interval = Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
        let mut records = BTreeMap::new();
        // Lease requests wait in one queue, longest waiting first: an authorization since
        // its launch, a renewal since it fell due.
        let mut waiting = Vec::new();
        let key = self.store.change_key()?;
        for (id, slot) in self.slots.iter_mut() {
            let supervisor_exited = slot.child.exited()?.is_some();
            // A slot with no new event and an unchanged journal needs no new read.
            let mut fresh = slot.exited != supervisor_exited;
            slot.exited = supervisor_exited;
            while let Ok(event) = slot.events.try_recv() {
                fresh = true;
                match event {
                    Ok(Event::Prepared { record }) => {
                        self.collector.prime(std::slice::from_ref(&record));
                        slot.prepared = Some(record);
                    }
                    Ok(Event::Finished { .. }) => {}
                    Ok(Event::Adjusted {
                        sequence,
                        applied,
                        unavailable,
                        hint_ready,
                        observes_drain,
                        applied_targets,
                        step_codes,
                        ..
                    }) => {
                        for (key, steps) in unavailable {
                            slot.unavailable.entry(key).or_default().extend(steps);
                        }
                        slot.applied = applied;
                        slot.applied_targets = applied_targets;
                        slot.step_codes = step_codes;
                        slot.applied_sequence = slot.applied_sequence.max(sequence);
                        slot.note_readiness(hint_ready, observes_drain);
                    }
                    Ok(Event::State {
                        applied,
                        targets,
                        hint_ready,
                        observes_drain,
                        ..
                    }) => {
                        slot.applied = applied;
                        slot.applied_targets = targets;
                        slot.note_readiness(hint_ready, observes_drain);
                    }
                    Ok(Event::Readiness {
                        hint_ready,
                        observes_drain,
                    }) => slot.note_readiness(hint_ready, observes_drain),
                    Ok(Event::Failed { detail }) | Err(detail) => {
                        emit(
                            &serde_json::json!({"event":"supervisor_error","assignment_id":slot.assignment.request.assignment_id,"detail":detail}),
                        )?;
                    }
                }
            }
            let record = match slot
                .journal_seen
                .as_ref()
                .filter(|seen| !fresh && seen.key == key)
            {
                Some(seen) => seen.record.clone(),
                None => {
                    let record = self
                        .store
                        .execution_record(&slot.assignment.request.assignment_id)?;
                    slot.journal_seen = Some(JournalSeen {
                        key,
                        record: record.clone(),
                        head: None,
                    });
                    record
                }
            };
            ensure!(
                !config.storage_profile.is_replayable()
                    || record.is_some()
                    || (!supervisor_exited && slot.prepared.is_none() && slot.sequence == 0),
                "replayable execution journal lost an owned attempt; reservation remains uncertain until authenticated recovery"
            );
            if supervisor_exited && record.is_none() {
                // The only route to EXEC first persists an execution row. An
                // exited owned supervisor with no row could not authorize code.
                let id = slot.assignment.request.assignment_id.clone();
                let pair = (
                    slot.assignment.clone(),
                    "owned supervisor exited before reservation/authorization".to_string(),
                );
                let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                if !proof.exists() {
                    write_json_new(&proof, &pair)?;
                }
                self.refusals.insert(id, pair);
                slot.reported = true;
                continue;
            }
            let running = record.as_ref().is_some_and(|r| {
                matches!(
                    r.phase,
                    ExecutionPhase::Authorized | ExecutionPhase::Running
                )
            });
            if wants_authorization(slot) {
                waiting.push((slot.started_at, id.clone()));
            } else if running && slot.sequence > 0 && slot.last_renew.elapsed() >= interval {
                waiting.push((slot.last_renew + interval, id.clone()));
            }
            records.insert(id.clone(), record);
        }
        // Requests run one at a time and each costs a synced coordinator write, so they share
        // half the monitor interval; the rest wait for the next iteration (research R19).
        lease_order(&mut waiting);
        let started = Instant::now();
        let budget = Duration::from_millis(config.monitor.interval_ms / 2);
        let mut sent = 0;
        if self.negotiated.lease_batch {
            // One request per batch, served in one coordinator transaction (FR-074).
            let mut renewals = Vec::new();
            let mut initial = Vec::new();
            for (_, id) in &waiting {
                let Some(slot) = self.slots.get(id) else {
                    continue;
                };
                if slot.sequence == 0 {
                    let Some(record) = slot.prepared.clone() else {
                        continue;
                    };
                    initial.push((
                        slot.started_at,
                        LeaseItem::Prepared {
                            assignment_id: record.assignment_id.clone(),
                            generation: record.generation,
                            coordinator_epoch: slot.assignment.coordinator_epoch,
                            record: Box::new(record),
                        },
                    ));
                } else {
                    renewals.push((
                        slot.last_renew,
                        LeaseItem::Renew {
                            assignment_id: slot.assignment.request.assignment_id.clone(),
                            generation: slot.assignment.generation,
                            coordinator_epoch: slot.assignment.coordinator_epoch,
                            previous_sequence: slot.sequence,
                        },
                    ));
                }
            }
            for batch in lease_batches(renewals, initial) {
                if !lease_turn(sent, started.elapsed(), budget) {
                    break;
                }
                sent += 1;
                let mut dues = BTreeMap::new();
                for item in &batch {
                    if let LeaseItem::Renew { assignment_id, .. } = item
                        && let Some(slot) = self.slots.get_mut(assignment_id)
                    {
                        dues.insert(
                            assignment_id.clone(),
                            next_control_due(&mut slot.last_renew, interval),
                        );
                    }
                }
                let requested = Instant::now();
                match self.client.request_leases(&batch, &dues, interval).await {
                    Ok(Response::Leases { results }) if results.len() == batch.len() => {
                        for (item, result) in batch.iter().zip(results) {
                            let Some(slot) = self.slots.get_mut(item.assignment_id()) else {
                                continue;
                            };
                            match item {
                                LeaseItem::Prepared { .. } => {
                                    authorization_reply(slot, requested, item_result(result))?
                                }
                                LeaseItem::Renew { .. } => {
                                    renewal_reply(slot, requested, item_result(result))?
                                }
                            }
                        }
                    }
                    other => {
                        // A refused batch may come from an older coordinator: single requests
                        // from the next iteration. Each item reads as a request that met it.
                        let refused = match &other {
                            Ok(_) => true,
                            Err(error) => {
                                format!("{error:#}").contains("coordinator rejected request")
                            }
                        };
                        if refused {
                            self.negotiated.forget_lease_batch();
                        }
                        let detail = format!("{other:?}");
                        for item in &batch {
                            let event = match item {
                                LeaseItem::Prepared { .. } => "authorization_unavailable",
                                LeaseItem::Renew { .. } => "lease_unavailable",
                            };
                            emit(
                                &serde_json::json!({"event":event,"assignment_id":item.assignment_id(),"detail":detail}),
                            )?;
                        }
                    }
                }
            }
        } else {
            for (_, id) in waiting {
                if !lease_turn(sent, started.elapsed(), budget) {
                    break;
                }
                sent += 1;
                let Some(slot) = self.slots.get_mut(&id) else {
                    continue;
                };
                if slot.sequence == 0 {
                    let Some(record) = slot.prepared.clone() else {
                        continue;
                    };
                    let requested = Instant::now();
                    let result = self
                        .client
                        .request(&Request::Prepared {
                            assignment_id: record.assignment_id.clone(),
                            generation: record.generation,
                            coordinator_epoch: slot.assignment.coordinator_epoch,
                            record: record.clone(),
                        })
                        .await;
                    authorization_reply(slot, requested, result)?;
                } else {
                    let requested = Instant::now();
                    let due = next_control_due(&mut slot.last_renew, interval);
                    let result = self
                        .client
                        .request_control(
                            &Request::Renew {
                                assignment_id: slot.assignment.request.assignment_id.clone(),
                                generation: slot.assignment.generation,
                                coordinator_epoch: slot.assignment.coordinator_epoch,
                                previous_sequence: slot.sequence,
                            },
                            due,
                            interval,
                        )
                        .await;
                    renewal_reply(slot, requested, result)?;
                }
            }
        }
        for (id, slot) in self.slots.iter_mut() {
            // A slot refused above has no record entry and needs nothing more this interval.
            let Some(record) = records.remove(id) else {
                continue;
            };
            let supervisor_exited = slot.exited;
            for checkpoint in [true, false] {
                let id = slot.assignment.request.assignment_id.clone();
                if self.publishing.contains(&(id.clone(), checkpoint))
                    || slot.reported
                    || (checkpoint && self.publishing.contains(&(id.clone(), false)))
                {
                    continue;
                }
                let released = record
                    .as_ref()
                    .is_some_and(|r| r.phase == ExecutionPhase::Released);
                // The checkpoint head's hash, `None` when it cannot be read.
                let head = if checkpoint {
                    let seen = slot
                        .journal_seen
                        .as_ref()
                        .filter(|seen| seen.key == key)
                        .and_then(|seen| seen.head.clone());
                    match seen {
                        Some(head) => head,
                        None => {
                            let head = crate::publication::read_head(
                                &self.store,
                                &slot.output_dir,
                                "checkpoint",
                            )
                            .ok()
                            .map(|bytes| {
                                bytes.map(|bytes| {
                                    use sha2::{Digest, Sha256};
                                    hex::encode(Sha256::digest(bytes))
                                })
                            });
                            if let Some(seen) = slot.journal_seen.as_mut() {
                                seen.head = Some(head.clone());
                            }
                            head
                        }
                    }
                } else {
                    Some(None)
                };
                if !needs_pin(
                    checkpoint,
                    released,
                    head.as_ref().map(|hash| hash.as_deref()),
                    slot.checkpoint_hash.as_deref(),
                ) {
                    continue;
                }
                let publication_pin =
                    crate::publication_gc::pin_attempt(&self.store, &slot.output_dir)?;
                let descriptor_bytes = match crate::publication::read_head(
                    &self.store,
                    &slot.output_dir,
                    if checkpoint { "checkpoint" } else { "result" },
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        // A publisher reserves its candidate before durable insertion.
                        // Pending or damaged candidates must retain this attempt and
                        // must never turn into a plain-command success or stop leases
                        // for other workloads. A later poll can observe reconciliation.
                        if record
                            .as_ref()
                            .is_some_and(|r| r.phase == ExecutionPhase::Released)
                        {
                            emit(
                                &serde_json::json!({"event":"publication_retained","assignment_id":id,"detail":error.to_string()}),
                            )?;
                        }
                        continue;
                    }
                };
                let has_descriptor = descriptor_bytes.is_some();
                let hash = descriptor_bytes
                    .as_ref()
                    .map(|bytes| {
                        use sha2::{Digest, Sha256};
                        hex::encode(Sha256::digest(bytes))
                    })
                    .unwrap_or_default();
                if checkpoint && (hash.is_empty() || slot.checkpoint_hash.as_ref() == Some(&hash)) {
                    continue;
                }
                if !checkpoint {
                    // Checkpoint and final acceptance update the same attempt.
                    // Final acceptance fences later checkpoints, so do not race
                    // it against an outstanding checkpoint publication.
                    if self.publishing.contains(&(id.clone(), true)) {
                        continue;
                    }
                    if !record
                        .as_ref()
                        .is_some_and(|r| r.phase == ExecutionPhase::Released)
                    {
                        continue;
                    }
                    let outcome = fs::read(slot.output_dir.join("execution-outcome.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<ExecutionOutcome>(&b).ok());
                    if outcome.is_none() && !supervisor_exited {
                        continue;
                    }
                    if outcome
                        .as_ref()
                        .is_none_or(|o| o.exit_code != Some(0) || o.yielded)
                    {
                        if self.publishing.contains(&(id.clone(), true)) {
                            continue;
                        }
                        if has_descriptor { /* Native immutable result committed before a racing drain. */
                        } else {
                            // A failure report is a synced coordinator write too; it waits
                            // for the next iteration once the budget is spent.
                            if !lease_turn(sent, started.elapsed(), budget) {
                                continue;
                            }
                            sent += 1;
                            if matches!(
                                self.client
                                    .request(&Request::Fail {
                                        assignment_id: id.clone(),
                                        generation: slot.assignment.generation,
                                        detail: record
                                            .as_ref()
                                            .expect("record was checked released above")
                                            .detail
                                            .clone(),
                                        failure_kind: if outcome.as_ref().is_some_and(|o| o.yielded)
                                        {
                                            FailureKind::Yielded
                                        } else {
                                            FailureKind::ExecutionFailure
                                        }
                                    })
                                    .await,
                                Ok(Response::Ok)
                            ) {
                                let receipt = slot.output_dir.join("failure.receipt.json");
                                if !receipt.exists() {
                                    write_json_new(
                                        &receipt,
                                        &serde_json::json!({"assignment_id":id,"generation":slot.assignment.generation,"accepted_failure":true}),
                                    )?;
                                }
                                slot.reported = true;
                            }
                            continue;
                        }
                    }
                }
                let task_client = self.client.clone();
                let assignment = slot.assignment.clone();
                let output = slot.output_dir.clone();
                self.publishing.insert((id.clone(), checkpoint));
                self.publications.push(tokio::spawn(async move {
                    let _pin = publication_pin;
                    let result = publish(
                        &task_client,
                        &assignment,
                        &output,
                        checkpoint,
                        descriptor_bytes,
                    )
                    .await;
                    (id, checkpoint, hash, result)
                }));
            }
            if let Some(status) = slot.child.exited()?
                && let Some(mut record) = self
                    .store
                    .execution_record(&slot.assignment.request.assignment_id)?
                && record.phase != ExecutionPhase::Released
            {
                // Reaping synchronizes with the supervisor's last durable write.
                // The earlier loop snapshot may predate its Released commit.
                record.phase = ExecutionPhase::NeedsReconciliation;
                record.detail =
                    format!("supervisor exited {status}; existing allocation remains charged");
                self.store.transition(&record)?;
            }
        }
        Ok(())
    }

    fn retire_slots(&mut self, records: &[ExecutionRecord]) -> Result<()> {
        let config = self.config;
        let mut released_families = BTreeSet::new();
        for record in records.iter().filter(|r| {
            r.phase == ExecutionPhase::Released && self.slots.contains_key(&r.assignment_id)
        }) {
            if self
                .store
                .managed_children(&record.assignment_id)?
                .iter()
                .all(|c| c.phase == crate::managed_children::ManagedChildPhase::Released)
            {
                released_families.insert(record.assignment_id.clone());
            }
        }
        self.slots.retain(|id, slot| {
            !(slot.exited
                && slot.reported
                && !self.publishing.iter().any(|(published, _)| published == id)
                && (released_families.contains(id)
                    || self.refusals.contains_key(id)
                    || config
                        .state_dir
                        .join("failure-receipts")
                        .join(format!("{id}.json"))
                        .exists()))
        });
        Ok(())
    }

    /// Durable outbox recovery after agent loss or a lost final ACK.
    fn recover_outbox(&mut self, records: &[ExecutionRecord]) -> Result<()> {
        let config = self.config;
        // Durable outbox recovery after agent loss or a lost final ACK. Only
        // previously journaled attempts are considered, never directory names as IDs.
        for record in records.iter().filter(|r| {
            r.phase == ExecutionPhase::Released && !self.slots.contains_key(&r.assignment_id)
        }) {
            let output = config
                .state_dir
                .join("attempts")
                .join(&record.assignment_id);
            if output.join("result.receipt.json").exists()
                || config
                    .state_dir
                    .join("failure-receipts")
                    .join(format!("{}.json", record.assignment_id))
                    .exists()
                || output.join("failure.receipt.json").exists()
                || self
                    .publishing
                    .contains(&(record.assignment_id.clone(), false))
            {
                continue;
            }
            if upgrade_attempt_held(&self.store, &record.assignment_id)? {
                continue;
            }
            if let Ok(bytes) = crate::publication::read_bounded(
                &output.join("supervisor-spec.json"),
                3 * 1024 * 1024,
            ) {
                let spec: SupervisorSpec = serde_json::from_slice(&bytes)?;
                ensure!(
                    spec.assignment.request.assignment_id == record.assignment_id
                        && spec.assignment.generation == record.generation,
                    "outbox identity mismatch"
                );
                let normal_exit = fs::read(output.join("execution-outcome.json"))
                    .ok()
                    .and_then(|v| serde_json::from_slice::<ExecutionOutcome>(&v).ok())
                    .is_some_and(|o| o.exit_code == Some(0) && !o.yielded);
                let publication_pin = crate::publication_gc::pin_attempt(&self.store, &output)?;
                let descriptor_bytes = match crate::publication::read_head(
                    &self.store,
                    &output,
                    "result",
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        emit(
                            &serde_json::json!({"event":"publication_retained","assignment_id":record.assignment_id,"detail":error.to_string()}),
                        )?;
                        continue;
                    }
                };
                if descriptor_bytes.is_some()
                    || (normal_exit && !output.join("result.json").try_exists()?)
                {
                    let task_client = self.client.clone();
                    let id = record.assignment_id.clone();
                    self.publishing.insert((id.clone(), false));
                    self.publications.push(tokio::spawn(async move {
                        let _pin = publication_pin;
                        let result = publish(
                            &task_client,
                            &spec.assignment,
                            &output,
                            false,
                            descriptor_bytes,
                        )
                        .await;
                        (id, false, String::new(), result)
                    }));
                } else {
                    // Confirmed released failure after an agent crash still needs
                    // its idempotent Fail acknowledgement, never result publication.
                    let pair = (spec.assignment, record.detail.clone());
                    let proof = config
                        .state_dir
                        .join("refusals")
                        .join(format!("{}.json", record.assignment_id));
                    if !proof.exists() {
                        write_json_new(&proof, &pair)?;
                    }
                    self.refusals.insert(record.assignment_id.clone(), pair);
                }
            }
        }
        Ok(())
    }

    async fn observe(
        &mut self,
        records: &[ExecutionRecord],
    ) -> Result<(
        crate::model::Snapshot,
        Vec<crate::model::Allocation>,
        crate::model::Decision,
    )> {
        let config = self.config;
        let agent = self.agent;
        // Absolute monitor cadence can arrive just before the OS CPU sampling
        // interval. Wait for fresh evidence after pending control/reaping work;
        // do not substitute cached utilization or extend control deadlines.
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            self.collector.next_cpu_sample_at(),
        ))
        .await;
        let (mut snapshot, allocations) = self
            .collector
            .sample_with_children(records, &self.store.all_unreleased_managed_children()?)?;
        restrict_gpu_scope(&mut snapshot, &agent.capacity);
        // Limits that depend on the host are checked once; the agent stops if they fail.
        if !self.host_checked {
            config
                .validate_host(&snapshot)
                .context("configuration does not fit this host")?;
            self.host_checked = true;
        }
        // Percentage floors are judged on the host, so their warning comes here.
        for warning in config.host_floor_warnings(&snapshot) {
            if self.floor_warnings.insert(warning.clone()) {
                eprintln!("cedegrid: warning: {warning}");
            }
        }
        if self.last_spool_check.elapsed() >= Duration::from_secs(5) {
            self.spool_full =
                spool_size(&config.state_dir.join("attempts"))? > agent.max_spool_bytes;
            self.last_spool_check = Instant::now();
        }
        for (id, slot) in self.slots.iter_mut() {
            for (key, step) in slot.seed.take().unwrap_or_default() {
                self.policy
                    .seed(id, key, step, self.start.elapsed().as_millis() as u64);
            }
        }
        let mut decision = self.policy.evaluate(
            config,
            &snapshot,
            &PolicyInput {
                allocations: allocations.clone(),
                explicit_drain: self.stopping.is_some(),
                facts: self.allocation_facts(records, &allocations),
                envelope: Some(agent.capacity.clone()),
                spool_full: self.spool_full,
            },
            self.start.elapsed().as_millis() as u64,
        )?;
        decision.managed_budget = minimum(&decision.managed_budget, &agent.capacity);
        decision.observe_only = false;
        decision.admission_headroom = minimum(
            &decision.admission_headroom,
            &remaining_capacity(
                &schedulable_budget(&decision),
                &allocation_charge(&allocations),
            ),
        );
        Ok((snapshot, allocations, decision))
    }

    /// Supervisors run no collector; send the GPU part of this observation to
    /// those that can launch GPU work so their launch rechecks stay current.
    fn share_observation(&mut self, snapshot: &crate::model::Snapshot) {
        let mut shared = snapshot.clone();
        shared.capabilities.clear();
        shared.kernel = None;
        let message = Control::Observation {
            snapshot: Box::new(shared),
        };
        for slot in self.slots.values_mut().filter(|slot| {
            !slot.exited
                && (!slot.assignment.request.resources.gpu_memory_mib.is_empty()
                    || slot.assignment.request.managed_child_limit > 0)
        }) {
            // A closed pipe leaves that supervisor's observation stale, which fails its recheck closed.
            let _ = send(slot, &message);
        }
    }

    /// Actuation stage: the operator envelope check and drain delivery.
    fn actuate(
        &mut self,
        decision: &mut crate::model::Decision,
        allocations: &[crate::model::Allocation],
    ) -> Result<()> {
        let agent = self.agent;
        block_admission_beyond_envelope(decision, allocations, &agent.capacity)?;
        let causes: BTreeMap<&str, crate::contention::YieldCause> = decision
            .actions
            .iter()
            .filter(|a| a.step >= crate::contention::YieldStep::Drain)
            .map(|a| (a.allocation_id.as_str(), a.cause))
            .collect();
        // The critical floor path terminates workers that do not poll the drain file.
        let terminations: BTreeMap<&str, crate::contention::ResourceKey> = decision
            .actions
            .iter()
            .filter(|a| a.step == crate::contention::YieldStep::Terminate)
            .map(|a| (a.allocation_id.as_str(), a.resource.clone()))
            .collect();
        for id in &decision.would_drain {
            // Guaranteed work drains only for a breach the policy permits.
            if let Some(slot) = self.slots.get_mut(id)
                && slot.drain_sent_at.is_none()
                && (self.stopping.is_some()
                    || slot.assignment.request.class == AllocationClass::Opportunistic
                    || matches!(
                        causes.get(id.as_str()),
                        Some(
                            crate::contention::YieldCause::FloorBreach
                                | crate::contention::YieldCause::EnvelopeBreach
                        )
                    ))
            {
                let cause = causes.get(id.as_str()).copied();
                match terminations.get(id.as_str()) {
                    Some(key) => terminate(slot, key.clone(), cause),
                    None => drain(slot, cause),
                }
            }
        }
        // Runtime steps go to the supervisors, which apply them to their own
        // family only; a resource whose step ended returns to none.
        let mut wanted: BTreeMap<
            &str,
            BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>,
        > = BTreeMap::new();
        for action in decision
            .actions
            .iter()
            .filter(|a| a.step < crate::contention::YieldStep::Drain)
        {
            wanted
                .entry(action.allocation_id.as_str())
                .or_default()
                .insert(
                    action.resource.clone(),
                    crate::contention::StepRequest {
                        step: action.step,
                        target: action.target,
                        cause: action.cause,
                        deadline_ms: action.deadline_ms,
                    },
                );
        }
        for (id, slot) in self.slots.iter_mut() {
            if slot.exited || slot.sequence == 0 || slot.drain_sent_at.is_some() {
                continue;
            }
            let steps = wanted.remove(id.as_str()).unwrap_or_default();
            if steps == slot.sent_steps {
                continue;
            }
            let mut resources = steps.clone();
            for (key, sent) in &slot.sent_steps {
                resources
                    .entry(key.clone())
                    .or_insert(crate::contention::StepRequest {
                        step: crate::contention::YieldStep::None,
                        target: None,
                        cause: sent.cause,
                        deadline_ms: None,
                    });
            }
            slot.adjust_sequence += 1;
            let message = Control::Adjust {
                sequence: slot.adjust_sequence,
                resources,
            };
            if send(slot, &message).is_ok() {
                slot.sent_steps = steps;
            }
        }
        // The allowance goes to each supervisor when the published value changes.
        for (id, slot) in self.slots.iter_mut() {
            if slot.exited || slot.sequence == 0 || slot.drain_sent_at.is_some() {
                continue;
            }
            let Some(allowance) = decision.allowances.get(id.as_str()) else {
                continue;
            };
            if slot.sent_allowance.as_ref() == Some(allowance) {
                continue;
            }
            let message = Control::Allowance {
                allowance: allowance.clone(),
            };
            if send(slot, &message).is_ok() {
                slot.sent_allowance = Some(allowance.clone());
            }
        }
        Ok(())
    }

    /// What observation cannot show the policy: who can act, progress at risk
    /// and the steps each backend or supervisor cannot apply.
    fn allocation_facts(
        &self,
        records: &[ExecutionRecord],
        allocations: &[crate::model::Allocation],
    ) -> BTreeMap<String, crate::model::AllocationFacts> {
        use crate::contention::{ResourceKey, YieldStep};
        let mut facts = allocations
            .iter()
            .map(|allocation| {
                let mut facts = crate::model::AllocationFacts::default();
                match self.slots.get(&allocation.id) {
                    Some(slot) => facts = slot.facts(),
                    None => facts.uncontrollable = true,
                }
                let backend = records
                    .iter()
                    .find(|r| r.assignment_id == allocation.id)
                    .map(|r| r.backend.as_str());
                // A rootless backend has no runtime share control (research R4).
                if backend == Some("rootless") {
                    facts
                        .unavailable
                        .entry(ResourceKey::Cpu)
                        .or_default()
                        .insert(YieldStep::LowerShare);
                }
                facts.ram_cap = ram_cap_available(self.config, backend);
                (allocation.id.clone(), facts)
            })
            .collect();
        withhold_authorized_pending(&mut facts, allocations, |id| {
            self.slots.get(id).is_some_and(|slot| slot.sequence > 0)
        });
        facts
    }

    /// Heartbeat and admission: report, apply coordinator drains, acknowledge
    /// refusals and start newly offered assignments.
    async fn heartbeat(
        &mut self,
        records: &[ExecutionRecord],
        snapshot: &crate::model::Snapshot,
        allocations: &[crate::model::Allocation],
        decision: &crate::model::Decision,
    ) -> Result<()> {
        let config = self.config;
        let agent = self.agent;
        let executable = self.executable;
        if self.stopping.is_none()
            && self.last_heartbeat.elapsed()
                < Duration::from_millis(self.config.lifecycle.heartbeat_interval_ms)
        {
            return Ok(());
        }
        let interval = Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
        let _ = reconcile_agent_owned(
            config,
            self.slots
                .iter()
                .filter(|(_, slot)| !slot.exited)
                .map(|(id, _)| id),
            self.preparations.keys(),
            self.namespace_guard.clone(),
        )?;
        let boot_id = supervision::process_identity(std::process::id(), "agent", 0)?.boot_id;
        let reconciled_records = self.store.executions()?;
        let mut reports: Vec<AllocationReport> = reconciled_records
            .iter()
            .map(|r| AllocationReport {
                assignment_id: r.assignment_id.clone(),
                generation: r.generation,
                phase: phase(&r.phase),
                observed: allocations
                    .iter()
                    .find(|a| a.id == r.assignment_id)
                    .and_then(|a| a.observed.clone()),
                detail: r.detail.clone(),
                // Stripped again unless the coordinator negotiated contention/1.
                yield_cause: self
                    .slots
                    .get(&r.assignment_id)
                    .and_then(|slot| slot.drain_cause),
                hint_ready: self.slots.get(&r.assignment_id).map(|slot| slot.hint_ready),
                observes_drain: self
                    .slots
                    .get(&r.assignment_id)
                    .map(|slot| slot.observes_drain),
            })
            .collect();
        reports.extend(
            self.refusals
                .values()
                .filter(|(assignment, _)| {
                    !reconciled_records
                        .iter()
                        .any(|record| record.assignment_id == assignment.request.assignment_id)
                })
                .map(|(assignment, detail)| AllocationReport {
                    assignment_id: assignment.request.assignment_id.clone(),
                    generation: assignment.generation,
                    phase: RemotePhase::Released,
                    observed: None,
                    detail: detail.clone(),
                    yield_cause: None,
                    hint_ready: None,
                    observes_drain: None,
                }),
        );
        let available_controls = advertised_controls(config, agent.cpu_affinity.is_some());
        let active_workers =
            self.slots.values().filter(|s| !s.reported).count() + self.preparations.len();
        // Every drain sent since the last heartbeat backs off once, whether local
        // policy or the coordinator asked for it (audit M8).
        let draining_workers = self
            .slots
            .values()
            .filter(|s| s.drain_sent_at.is_some() && !s.reported)
            .count();
        let fresh_drains = new_drains(
            self.slots.iter().map(|(id, s)| (id, s.drain_sent_at)),
            &mut self.counted_drains,
        );
        self.counted_drains.retain(|id| self.slots.contains_key(id));
        if fresh_drains > 0 {
            self.admitted_parallelism = self
                .admitted_parallelism
                .min(active_workers.saturating_sub(draining_workers));
            self.last_growth = Instant::now();
        }
        // Parallelism grows at the CPU/RAM scale-up cooldown, never the GPU one
        // (audit M1); GPU admission keeps its own cooldown in the policy.
        let grow_after_ms = [
            (
                crate::contention::ResourceKind::Cpu,
                crate::contention::ResourceKey::Cpu,
            ),
            (
                crate::contention::ResourceKind::Ram,
                crate::contention::ResourceKey::Ram,
            ),
        ]
        .into_iter()
        .map(|(kind, key)| {
            let mode = decision
                .resources
                .get(&key)
                .and_then(|s| s.effective_mode)
                .unwrap_or_else(|| config.contention.mode(kind));
            config
                .contention
                .thresholds(kind)
                .cooldown_ms(mode, config.monitor.interval_ms)
        })
        .max()
        .unwrap_or(0);
        let may_grow = self.last_growth.elapsed() >= Duration::from_millis(grow_after_ms);
        let admission_safe = decision.expansion_allowed || decision.cpu_ram_expansion_allowed;
        let fill = decision
            .resources
            .get(&crate::contention::ResourceKey::Cpu)
            .and_then(|s| s.effective_mode)
            == Some(crate::contention::ContentionMode::Performance);
        let launch_slots = if admission_safe && self.stopping.is_none() && !self.spool_full {
            launch_slots(LaunchPacing {
                max_workers: agent.max_workers,
                active: active_workers,
                admitted: self.admitted_parallelism,
                may_grow,
                fill,
            }) as u32
        } else {
            0
        };
        let mut report = NodeReport {
            node_id: config.node_id.clone(),
            launch_slots,
            boot_id,
            observed_at_unix_ms: snapshot.observed_at_unix_ms,
            managed_budget: schedulable_budget(decision),
            // No mode fully yields GPU work any more, so Guaranteed GPU work fits.
            gpu_guaranteed_allowed: true,
            expansion_allowed: admission_safe && self.stopping.is_none() && !self.spool_full,
            gpu_expansion_allowed: decision.expansion_allowed
                && self.stopping.is_none()
                && !self.spool_full,
            available_controls,
            allocations: reports,
            features: Vec::new(),
            contention: None,
        };
        self.negotiated.apply(&mut report, || {
            node_contention_report(config, decision, agent.max_workers)
        });
        // Report preparation can cross a scheduled slot. Advance at actual
        // enqueue so the next due time agrees with recorded lateness.
        let due = next_control_due(&mut self.last_heartbeat, interval);
        match self
            .client
            .request_control(&Request::Heartbeat { report }, due, interval)
            .await
        {
            Ok(Response::Heartbeat { reply }) => {
                self.negotiated.learn(&reply);
                for slot in self.slots.values_mut() {
                    slot.assignment.coordinator_epoch = reply.coordinator_epoch;
                }
                let remote_drains: BTreeSet<_> = reply.drain.iter().cloned().collect();
                for id in reply.drain {
                    if let Some((_, _, revoked)) = self.preparations.get_mut(&id) {
                        *revoked = true;
                    }
                    if let Some(slot) = self.slots.get_mut(&id) {
                        drain(slot, Some(crate::contention::YieldCause::CoordinatorDrain));
                    }
                }
                let mut acknowledged = vec![];
                // Refusal reports share half the monitor interval, like lease requests; the
                // rest are reported at the next heartbeat (research R19).
                let reports_started = Instant::now();
                let budget = Duration::from_millis(config.monitor.interval_ms / 2);
                for (reported, (id, (assignment, detail))) in self.refusals.iter().enumerate() {
                    if !lease_turn(reported, reports_started.elapsed(), budget) {
                        break;
                    }
                    let yielded = fs::read(
                        config
                            .state_dir
                            .join("attempts")
                            .join(id)
                            .join("execution-outcome.json"),
                    )
                    .ok()
                    .and_then(|b| serde_json::from_slice::<ExecutionOutcome>(&b).ok())
                    .is_some_and(|o| o.yielded);
                    if matches!(
                        self.client
                            .request(&Request::Fail {
                                assignment_id: id.clone(),
                                generation: assignment.generation,
                                detail: detail.clone(),
                                // The coordinator classifies by its recorded cause;
                                // the node adds only its journal-backed outcome.
                                failure_kind: if yielded {
                                    FailureKind::Yielded
                                } else {
                                    FailureKind::ExecutionFailure
                                }
                            })
                            .await,
                        Ok(Response::Ok)
                    ) {
                        let receipt = config
                            .state_dir
                            .join("failure-receipts")
                            .join(format!("{id}.json"));
                        if !receipt.exists() {
                            write_json_new(
                                &receipt,
                                &serde_json::json!({"assignment_id":id,"generation":assignment.generation,"accepted_failure":true}),
                            )?;
                        }
                        acknowledged.push(id.clone());
                    }
                }
                for id in acknowledged {
                    self.refusals.remove(&id);
                }
                let journaled = self.store.executions()?;
                for assignment in reply.assignments {
                    let id = assignment.request.assignment_id.clone();
                    if remote_drains.contains(&id)
                        && !self.slots.contains_key(&id)
                        && !self.preparations.contains_key(&id)
                        && !journaled.iter().any(|r| r.assignment_id == id)
                    {
                        ensure!(
                            !config.storage_profile.is_replayable(),
                            "revoked replayable offer without local identity requires authoritative recovery; absence is not release proof"
                        );
                        let pair=(assignment,"coordinator revoked never-launched offer; no local execution journal row".to_string());
                        let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                        if !proof.exists() {
                            write_json_new(&proof, &pair)?;
                        }
                        self.refusals.insert(id, pair);
                        continue;
                    }
                    if self.slots.contains_key(&id)
                        || self.preparations.contains_key(&id)
                        || self.refusals.contains_key(&id)
                        || records.iter().any(|r| r.assignment_id == id)
                    {
                        continue;
                    }
                    let active = self.slots.values().filter(|s| !s.reported).count()
                        + self.preparations.len();
                    if active >= agent.max_workers || self.stopping.is_some() {
                        continue;
                    }
                    let task_config = config.clone();
                    let task_agent = agent.clone();
                    let task_assignment = assignment.clone();
                    let task_capacity = schedulable_budget(decision);
                    let task_snapshot = snapshot.clone();
                    let task_executable = executable.to_path_buf();
                    let task_client = self.client.clone();
                    // A launch blocks on synced writes and the spawn; on the blocking pool it
                    // cannot hold up this current-thread loop, and the gate bounds how many
                    // write at once (research R19).
                    let runtime = tokio::runtime::Handle::current();
                    let gate = self.launch_gate.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        runtime.block_on(async move {
                            let _permit = gate.acquire().await?;
                            validate_gpu_launch_contract(
                                &task_config,
                                &task_snapshot,
                                &task_assignment.request.resources,
                                task_assignment.request.class,
                            )?;
                            start_assignment(
                                &task_config,
                                &task_agent,
                                task_assignment,
                                &task_capacity,
                                &task_executable,
                                &task_client,
                            )
                            .await
                        })
                    });
                    self.preparations.insert(id, (assignment, handle, false));
                    if active >= self.admitted_parallelism {
                        self.admitted_parallelism = active + 1;
                        self.last_growth = Instant::now();
                    }
                }
            }
            result => {
                self.negotiated.forget();
                emit(
                    &serde_json::json!({"event":"coordinator_disconnected","detail":format!("{result:?}")}),
                )?;
            }
        }
        Ok(())
    }

    /// Whether the stop sequence finished or its release deadline elapsed.
    fn stop_due(&mut self, records: &[ExecutionRecord]) -> Result<bool> {
        let config = self.config;
        let agent = self.agent;
        if agent.max_runtime_seconds > 0
            && self.start.elapsed() >= Duration::from_secs(agent.max_runtime_seconds)
            && self.stopping.is_none()
        {
            self.stopping = Some(Instant::now());
        }
        if let Some(stop) = self.stopping {
            if records.iter().all(|r| r.phase == ExecutionPhase::Released)
                && self.publications.is_empty()
                && self.preparations.is_empty()
                && self.slots.is_empty()
            {
                return Ok(true);
            }
            let limit = config.lifecycle.drain_timeout_ms
                + config.lifecycle.term_grace_ms
                + config.execution.release_confirm_timeout_ms
                + config.execution.prepare_timeout_ms;
            if stop.elapsed() >= Duration::from_millis(limit) {
                emit(
                    &serde_json::json!({"event":"stop_uncertain","detail":"release deadline elapsed; retained allocations require reconciliation"}),
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn shutdown(mut self) -> Result<()> {
        let config = self.config;
        for (id, (assignment, handle, _)) in std::mem::take(&mut self.preparations) {
            handle.abort();
            match handle.await {
                Ok(Ok(mut slot)) => {
                    drain(&mut slot, None);
                    self.slots.insert(id, slot);
                }
                Ok(Err(_)) | Err(_) => {
                    // No await exists after supervisor spawn, so cancellation either
                    // returns the owned slot or proves the future stopped before spawn.
                    let pair = (
                        assignment,
                        "agent stopped during pre-execution checkpoint preparation".to_string(),
                    );
                    let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                    if !proof.exists() {
                        write_json_new(&proof, &pair)?;
                    }
                }
            }
        }
        // Closing pipes is agent failure for any remaining opportunistic supervisor.
        // We deliberately never signal an uncertain supervisor/workload by PID.
        for slot in self.slots.values_mut() {
            drain(slot, None);
            let _ = slot.child.exited()?;
        }
        let retained: Vec<_>=self.slots.iter().map(|(id,slot)|serde_json::json!({"assignment_id":id,"exited":slot.exited,"reported":slot.reported,"publishing":self.publishing.iter().filter(|(publishing_id,_)|publishing_id==id).collect::<Vec<_>>(),"result_receipt":slot.output_dir.join("result.receipt.json").is_file(),"failure_receipt":slot.output_dir.join("failure.receipt.json").is_file()})).collect();
        let status = serde_json::json!({"event":"agent_stopped","node_id":config.node_id,"elapsed_ms":self.start.elapsed().as_millis(),"retained_slots":self.slots.len(),"retained_slot_details":retained,"executions":self.store.executions()?});
        emit(&status)?;
        Ok(())
    }
}

/// Run a node service until shutdown or its configured finite envelope. Every
/// managed workload has a separate supervisor; disconnects do not stop its clock.
pub async fn run(config: &Config, agent: &AgentConfig, executable: &Path) -> Result<()> {
    ensure!(
        config.execution.enabled,
        "agent execution requires execution.enabled=true"
    );
    ensure!(
        agent.max_workers > 0 && agent.max_workers <= 256,
        "invalid maximum worker count"
    );
    ensure!(
        agent.capacity.cpu_millicores > 0 && agent.capacity.ram_mib > 0,
        "explicit node CPU/RAM budget required"
    );
    ensure!(
        config.state_dir.is_absolute(),
        "state directory must be resolved before service startup"
    );
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?).canonicalize()?;
    ensure!(
        config.state_dir.starts_with(&home),
        "agent-created state must be within runtime home"
    );
    let mut existing = config.state_dir.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .context("state path has no existing ancestor")?;
    }
    ensure!(
        existing.canonicalize()?.starts_with(&home),
        "state ancestor resolves outside runtime home"
    );
    ensure!(
        !config
            .state_dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "parent traversal in state directory is refused"
    );
    if let Some(cpus) = &agent.cpu_affinity {
        #[cfg(target_os = "linux")]
        ensure!(
            unsafe { libc::syscall(libc::SYS_gettid) } == i64::from(std::process::id()),
            "configured affinity requires agent main-thread service entrypoint"
        );
        supervision::apply_current_cpu_affinity(cpus)?;
    }
    ensure!(
        agent.max_transfer_bytes_per_second > 0,
        "transfer rate must be positive"
    );
    let replay = if config.storage_profile.is_replayable() {
        Some(start_replay_session(config, agent, &home).await?)
    } else {
        None
    };
    let namespace = Namespace::new(&config.state_dir)?;
    let _owner = if replay.is_none() {
        Some(namespace.owner(Duration::ZERO)?)
    } else {
        None
    };
    let namespace_guard = if let Some((_, guard, _)) = &replay {
        guard.clone()
    } else {
        namespace.acquire(None, Duration::from_secs(30))?
    };
    crate::backup::ensure_runnable_state(&config.state_dir)?;
    let store = StateStore::open_with_profile_guarded(
        &config.state_dir,
        config.storage_profile,
        namespace_guard.clone(),
    )?;
    if let Some((_, _, snapshot)) = &replay {
        emit(
            &serde_json::json!({"event":"replay_recovery_started", "session_id":snapshot.session_id,
            "coordinator_epoch":snapshot.coordinator_epoch, "retained_allocations":snapshot.allocations.len(),
            "storage_assurance":config.storage_profile.assurance(),
            "detail":"Authenticated recovery ready; uncertain allocations remain charged."}),
        )?;
    }
    let replay_journal = replay
        .as_ref()
        .map(|_| ReplayJournalGuard::capture(config))
        .transpose()?;
    ensure!(
        config.state_dir.canonicalize()?.starts_with(&home),
        "state path resolves outside runtime home"
    );
    for name in ["attempts", "tmp", "cache", "refusals", "failure-receipts"] {
        private_dir(&config.state_dir.join(name))?;
    }
    let lock = super::AgentLock::acquire(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.state_dir.join("agent.lock"))?,
        "another agent already owns this node state",
    )?;
    #[cfg(unix)]
    let attached = if replay.is_none() {
        reattach_supervisors(config, &store)?
    } else {
        Vec::new()
    };
    #[cfg(not(unix))]
    let attached: Vec<ChildSlot> = Vec::new();
    let attached_ids: BTreeSet<String> = attached
        .iter()
        .map(|slot| slot.assignment.request.assignment_id.clone())
        .collect();
    let _ = reconcile_guarded(config, &attached_ids, namespace_guard.clone())?;
    let mut client = NodeClient::new(
        agent,
        &config.state_dir,
        config.storage_profile,
        namespace_guard.clone(),
    )?;
    client.replay_session = replay
        .as_ref()
        .map(|(_, _, snapshot)| snapshot.session_id.clone());
    let client = std::sync::Arc::new(client);
    let collector = ManagedCollector::new(config)?;
    let policy = PolicyEngine::new();
    let mut refusals: BTreeMap<String, (Assignment, String)> = BTreeMap::new();
    for item in fs::read_dir(config.state_dir.join("refusals"))? {
        let item = item?;
        ensure!(
            item.file_type()?.is_file() && item.metadata()?.len() <= 2 * 1024 * 1024,
            "invalid refusal record"
        );
        let pair: (Assignment, String) = serde_json::from_slice(
            &crate::publication::read_bounded(&item.path(), 3 * 1024 * 1024)?,
        )?;
        if !config
            .state_dir
            .join("failure-receipts")
            .join(format!("{}.json", pair.0.request.assignment_id))
            .is_file()
        {
            refusals.insert(pair.0.request.assignment_id.clone(), pair);
        }
    }
    let start = Instant::now();
    let observation_interval = Duration::from_millis(config.monitor.interval_ms);
    let mut next_observation = start;
    let mut stop_signal = Box::pin(shutdown_signal());
    let mut state = AgentLoop {
        config,
        agent,
        executable,
        store,
        client,
        namespace_guard,
        collector,
        policy,
        slots: BTreeMap::new(),
        refusals,
        publications: Vec::new(),
        publishing: BTreeSet::new(),
        preparations: BTreeMap::new(),
        start,
        last_heartbeat: start - Duration::from_millis(config.lifecycle.heartbeat_interval_ms),
        negotiated: NegotiatedFeatures::default(),
        stopping: None,
        last_spool_check: Instant::now(),
        spool_full: false,
        admitted_parallelism: 0,
        last_growth: start,
        counted_drains: BTreeSet::new(),
        floor_warnings: BTreeSet::new(),
        host_checked: false,
        launch_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(LAUNCH_CONCURRENCY)),
    };
    for slot in attached {
        state
            .slots
            .insert(slot.assignment.request.assignment_id.clone(), slot);
    }
    loop {
        // Anchor observations to their scheduled cadence. Processing and RPC
        // time consume this interval instead of extending it. After an overrun,
        // run once immediately and return to the original cadence.
        next_observation =
            next_periodic_wake(next_observation, observation_interval, Instant::now());
        if let Some(guard) = &replay_journal {
            guard.verify(config)?;
        }
        state.collect_preparations().await?;
        state.collect_publications().await?;
        state.service_slots().await?;
        let records = state.store.executions()?;
        collect_released_spool(config, &state.store, &records, &state.publishing)?;
        state.retire_slots(&records)?;
        state.recover_outbox(&records)?;
        let (snapshot, allocations, mut decision) = state.observe(&records).await?;
        state.share_observation(&snapshot);
        state.actuate(&mut decision, &allocations)?;
        state.store.append_observation(&snapshot, &decision)?;
        state
            .heartbeat(&records, &snapshot, &allocations, &decision)
            .await?;
        if state.stop_due(&records)? {
            break;
        }
        tokio::select! {_ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_observation))=>{}, _=&mut stop_signal,if state.stopping.is_none()=>{state.stopping=Some(Instant::now());}}
    }
    state.shutdown().await?;
    drop(lock);
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn upgrade_attempt_held(store: &StateStore, assignment: &str) -> Result<bool> {
    let exists: bool = store.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='upgrade_attempt_holds')", [], |r| r.get(0))?;
    if !exists {
        return Ok(false);
    }
    Ok(store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM upgrade_attempt_holds WHERE assignment_id=?1)",
        [assignment],
        |r| r.get(0),
    )?)
}

fn collect_released_spool(
    config: &Config,
    store: &StateStore,
    records: &[ExecutionRecord],
    publishing: &BTreeSet<(String, bool)>,
) -> Result<()> {
    for record in records
        .iter()
        .filter(|r| r.phase == ExecutionPhase::Released)
    {
        if upgrade_attempt_held(store, &record.assignment_id)?
            || publishing.iter().any(|(id, _)| id == &record.assignment_id)
        {
            continue;
        }
        let output = config
            .state_dir
            .join("attempts")
            .join(&record.assignment_id);
        if output.join("spool-reclaimed.json").exists() {
            continue;
        }
        let Some(reclaimed) = crate::publication_gc::reclaim_released(store, &output)? else {
            continue;
        };
        write_json_new(
            &output.join("spool-reclaimed.json"),
            &serde_json::json!({"assignment_id":record.assignment_id,"generation":record.generation,"logical_unlinked_bytes":reclaimed,"basis":"confirmed Released, exact accepted final receipt/head, absent supervisor and publication pins; coordinator retains published content"}),
        )?;
    }
    Ok(())
}

/// Restrict admission telemetry to the explicit GPU UUID resource scope. An
/// empty scope permits CPU-only admission without asserting host GPU idleness.
pub fn restrict_gpu_scope(snapshot: &mut crate::model::Snapshot, capacity: &Resources) {
    snapshot
        .gpus
        .retain(|g| capacity.gpu_memory_mib.contains_key(&g.uuid));
    if capacity.gpu_memory_mib.is_empty() {
        // Known-empty operator-authorized GPU set, not a claim about host inventory.
        snapshot.gpu_inventory = crate::model::CapabilityStatus::Available;
    }
    snapshot.capabilities.insert("gpu_admission_scope".into(),crate::model::Capability{status:crate::model::CapabilityStatus::Available,enforced:false,detail:format!("Only explicitly authorized GPU UUIDs are eligible: {:?}; other device activity is not a placement signal for this node envelope",capacity.gpu_memory_mib.keys().collect::<Vec<_>>())});
}

/// Record the steps a supervisor could not apply; they are skipped from now on.
/// The agent's operator-envelope check: cumulative charges, max(request,
/// observed), against the explicit node envelope.
/// The policy restores the operator envelope per resource (FR-057); an exceeded
/// envelope only blocks admission here, every resource's included, never stops
/// everything.
pub(super) fn block_admission_beyond_envelope(
    decision: &mut crate::model::Decision,
    allocations: &[crate::model::Allocation],
    capacity: &Resources,
) -> Result<()> {
    if envelope_exceeded(
        allocations,
        capacity,
        decision.cpu_admission_credit_millicores,
    ) {
        decision.expansion_allowed = false;
        decision.cpu_ram_expansion_allowed = false;
        for status in decision.resources.values_mut() {
            status.admission = Some(crate::contention::Admission::blocked(
                "admission_envelope_exceeded",
            ));
        }
        let text = format!(
            "operator-approved node envelope exceeded: capacity={}; restoring it per resource",
            serde_json::to_string(capacity)?
        );
        decision.reasons.push(text.clone());
        decision
            .reason_codes
            .push(crate::model::ReasonCode::new("envelope_breach", text));
    }
    Ok(())
}

/// CPU counts as admission charges it: `cpu_credit` is what running opportunistic
/// work is charged above its recent usage.
pub fn envelope_exceeded(
    allocations: &[crate::model::Allocation],
    capacity: &Resources,
    cpu_credit: i64,
) -> bool {
    let mut capacity = capacity.clone();
    capacity.cpu_millicores = (i128::from(capacity.cpu_millicores) + i128::from(cpu_credit))
        .clamp(0, i128::from(u64::MAX)) as u64;
    !allocations_fit(allocations, &capacity)
}

pub(super) fn allocations_fit(
    allocations: &[crate::model::Allocation],
    capacity: &Resources,
) -> bool {
    let used = allocation_charge(allocations);
    used.cpu_millicores <= capacity.cpu_millicores
        && used.ram_mib <= capacity.ram_mib
        && used
            .gpu_memory_mib
            .iter()
            .all(|(id, n)| *n <= capacity.gpu_memory_mib.get(id).copied().unwrap_or(0))
}

pub(super) fn remaining_capacity(capacity: &Resources, used: &Resources) -> Resources {
    let mut remaining = capacity.clone();
    remaining.subtract_saturating(used);
    remaining
}

pub(super) fn allocation_charge(allocations: &[crate::model::Allocation]) -> Resources {
    let mut used = Resources::default();
    for allocation in allocations {
        used.add_saturating(&Resources::charge(
            &allocation.requested,
            allocation.observed.as_ref(),
        ));
    }
    used
}
