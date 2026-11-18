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
/// The first retry of a missed re-adoption, doubled after each miss up to the maximum.
const READOPT_FIRST: Duration = Duration::from_secs(1);
const READOPT_MAX: Duration = Duration::from_secs(30);
/// Iterations between prunes of `observations` and settled attempt directories (FR-081).
const OBSERVATIONS_PER_PRUNE: u32 = 600;

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
pub(super) fn renewal_reply(
    slot: &mut ChildSlot,
    requested: Instant,
    result: Result<Response>,
) -> Result<()> {
    match result {
        Ok(Response::Lease { lease }) => {
            let remaining_ms = lease
                .valid_for_ms
                .saturating_sub(requested.elapsed().as_millis() as u64);
            if lease.drain {
                // Taking authority away needs no live lease: the drain goes as a drain line,
                // with or without time left (FR-079, R85).
                slot.sequence = lease.sequence;
                let delivered =
                    super::drain(slot, Some(crate::contention::YieldCause::CoordinatorDrain));
                let closed = !delivered && !slot.drain_unsent;
                if closed && !slot.drain_undelivered {
                    // Reported once for the slot; a failed report must not stop the loop.
                    let _ = emit(&serde_json::json!({
                        "event": "drain_undelivered",
                        "assignment_id": slot.assignment.request.assignment_id,
                        "source": "renewal_reply",
                        "lease_sequence": lease.sequence,
                        "remaining_ms": remaining_ms,
                        "detail": "the supervisor's control pipe is closed, so the coordinator's drain did not reach it",
                    }));
                }
                slot.drain_undelivered = closed;
            } else if remaining_ms > 0 {
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
/// Record a published checkpoint's time and hash for a later agent that re-adopts the run.
fn record_checkpoint(output: &Path, hash: &str) -> Result<()> {
    let path = output.join(super::reconcile::CHECKPOINT_PUBLISHED);
    let temporary = output.join(".checkpoint-published.json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(
            &serde_json::json!({"unix_ms": super::reconcile::now_unix_ms(), "hash": hash}),
        )?,
    )?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

/// A coordinator request bounded by the loop's budget: a hung or slow coordinator never
/// holds sampling and local yielding, and a request cut short is sent again next iteration
/// (lease items, heartbeats and failures are all idempotent).
async fn bounded<T>(
    budget: Duration,
    request: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(budget, request)
        .await
        .context("coordinator request exceeded the loop's budget; retried next iteration")?
}

pub(super) fn lease_turn(sent: usize, elapsed: Duration, budget: Duration) -> bool {
    sent == 0 || elapsed < budget
}

/// Whether a heartbeat is due: once per heartbeat interval, stopping or not.
pub(super) fn heartbeat_due(since_last: Duration, interval: Duration) -> bool {
    since_last >= interval
}

/// Whether lease renewals are asked for: only while the newest sample the loop acted on is no
/// older than one lease, so work it no longer watches is not kept leased (FR-079).
pub(super) fn renewals_asked(sample_age: Duration, lease: Duration) -> bool {
    sample_age <= lease
}

/// The failure reports an iteration sends while the control lane is behind: 16 a second.
pub(super) fn failure_floor(monitor: Duration) -> usize {
    usize::try_from((u128::from(monitor.as_millis() as u64) * 16).div_ceil(1000))
        .unwrap_or(usize::MAX)
        .max(1)
}

/// Whether one more failure report goes this iteration: the loop's share allows it, and the
/// control lane has caught up or fewer than the floor went (FR-034).
pub(super) fn failure_turn(
    sent: usize,
    elapsed: Duration,
    budget: Duration,
    failures: usize,
    floor: usize,
    behind: Duration,
) -> bool {
    lease_turn(sent, elapsed, budget) && (failures < floor || behind.is_zero())
}

/// What a stop left when its deadline passed, in one sentence.
pub(super) fn stop_uncertain_detail(
    unreleased: usize,
    live: usize,
    unanswered: usize,
    in_flight: usize,
) -> String {
    let count = |n: usize, one: &str, many: &str, rest: &str| {
        (n > 0).then(|| format!("{n} {}{rest}", if n == 1 { one } else { many }))
    };
    let parts: Vec<String> = [
        count(
            unreleased,
            "allocation",
            "allocations",
            " not released, which stay charged until reconciliation",
        ),
        count(live, "supervisor", "supervisors", " still running"),
        count(
            unanswered,
            "failure report or result",
            "failure reports or results",
            " unanswered, which the next agent sends again",
        ),
        count(
            in_flight,
            "publication or preparation",
            "publications or preparations",
            " in flight",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "release deadline elapsed".into()
    } else {
        format!("release deadline elapsed with {}", parts.join("; "))
    }
}

/// Whether a released slot's result pass takes its attempt pin. `failure` is whether the
/// outcome ends in failure, `None` when it could not be read; `descriptor` whether a result
/// descriptor is committed, `None` when the head could not be read.
pub(super) fn pins_result(failure: Option<bool>, descriptor: Option<bool>, turn: bool) -> bool {
    turn || failure != Some(true) || descriptor != Some(false)
}

/// Takes a released run's out-of-memory verdict at the first read of its outcome: the host's
/// count is judged over its last samples, so a later read could miss the kill (FR-076).
fn take_oom_verdict(
    oom_kills: &std::collections::VecDeque<Option<u64>>,
    slot: &mut ChildSlot,
    outcome: &ExecutionOutcome,
) -> Result<()> {
    if slot.oom_verdict.is_some() {
        return Ok(());
    }
    let host: Vec<Option<u64>> = oom_kills.iter().copied().collect();
    let verdict = oom::classify(&oom::Ending {
        termination_signal: outcome.termination_signal,
        supervisor_terminated: outcome.termination_started_ms.is_some(),
        leaf_oom_kills: outcome.leaf_oom_kills,
        host_oom_kills: &host,
        view: slot.ram_view.as_ref(),
    });
    if let oom::OomVerdict::NotOwnFault(detail) | oom::OomVerdict::OwnFault(detail) = &verdict {
        emit(&serde_json::json!({
            "event": "oom_kill",
            "task_id": slot.assignment.request.task_id,
            "assignment_id": slot.assignment.request.assignment_id,
            "own_fault": matches!(verdict, oom::OomVerdict::OwnFault(_)),
            "detail": detail,
        }))?;
    }
    if matches!(verdict, oom::OomVerdict::NotOwnFault(_)) {
        slot.drain_cause = Some(crate::contention::YieldCause::OomNotOwnFault);
    }
    slot.oom_verdict = Some(verdict);
    Ok(())
}

/// Whether a slot's publication check takes an attempt pin, which costs a state connection
/// and two synced writes: for a released result, or for a checkpoint head that is unreadable
/// (the pinned path reports it) or not yet published. `head` is the head's hash, `None` when
/// it could not be read.
/// An iteration's attempt pins: after one finds the journal busy, no other is tried, so the
/// iteration waits at most one pin's bound and its reports go in the next one (FR-080).
#[derive(Default)]
pub(super) struct PinGate {
    busy: bool,
}

impl PinGate {
    /// The pin, or `None` when the journal is busy now or was earlier in this iteration.
    pub(super) fn take(
        &mut self,
        pin: impl FnOnce() -> Result<crate::publication_gc::AttemptPin>,
    ) -> Result<Option<crate::publication_gc::AttemptPin>> {
        if self.busy {
            return Ok(None);
        }
        match pin() {
            Ok(pin) => Ok(Some(pin)),
            Err(error) if super::journal_busy(&error) => {
                self.busy = true;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Whether a pin found the journal busy in this iteration.
    pub(super) fn found_busy(&self) -> bool {
        self.busy
    }
}

/// A condition that holds over consecutive intervals, named once when it starts.
#[derive(Default)]
pub(super) struct Stretch(bool);

impl Stretch {
    /// Records this interval; true when it starts a stretch.
    pub(super) fn starts(&mut self, now: bool) -> bool {
        !std::mem::replace(&mut self.0, now) && now
    }
}

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

/// A publication task with its assignment and kind, so one that panicked is retried.
type Publication = (
    String,
    bool,
    tokio::task::JoinHandle<(String, bool, String, Result<()>)>,
);

type Preparation = (Assignment, tokio::task::JoinHandle<Result<ChildSlot>>, bool);

/// State of one agent service loop. Each stage of an interval is a method.
struct AgentLoop<'a> {
    /// The running configuration; a SIGHUP reload replaces its `[contention]` (FR-080).
    config: std::sync::Arc<Config>,
    config_path: Option<std::path::PathBuf>,
    agent: &'a AgentConfig,
    executable: &'a Path,
    store: StateStore,
    /// This iteration's attempt pins.
    pins: PinGate,
    client: std::sync::Arc<NodeClient>,
    namespace_guard: NamespaceGuard,
    collector: ManagedCollector,
    /// The agent's and its supervisors' own CPU (FR-069).
    cedegrid_cpu: crate::telemetry::CedegridCpu,
    /// The loop's own CPU by stage (SC-014).
    cost: LoopCost,
    policy: PolicyEngine,
    slots: BTreeMap<String, ChildSlot>,
    /// Own RAM when the current floor breach began, for FR-076's rule in the modes that keep
    /// own memory at a breach.
    ram_breach_start: Option<u64>,
    refusals: BTreeMap<String, (Assignment, String)>,
    publications: Vec<Publication>,
    publishing: BTreeSet<(String, bool)>,
    preparations: BTreeMap<String, Preparation>,
    start: Instant,
    last_heartbeat: Instant,
    negotiated: NegotiatedFeatures,
    /// The host's `oom_kill` count at the last three samples, oldest first (FR-076).
    oom_kills: std::collections::VecDeque<Option<u64>>,
    stopping: Option<Instant>,
    last_spool_check: Instant,
    spool_full: bool,
    admitted_parallelism: usize,
    last_growth: Instant,
    /// Slots whose drain already reduced the admitted parallelism.
    counted_drains: BTreeSet<String>,
    /// Assignments already reported as held back by the descriptor limit (FR-077).
    descriptor_refused: BTreeSet<String>,
    /// Offers refused once under System mode's administrator policy (005 FR-010).
    policy_refused: BTreeSet<String>,
    /// Whether the descriptor limit holds launch slots back, as last reported.
    descriptor_limited: bool,
    /// Host floor warnings already printed.
    floor_warnings: BTreeSet<String>,
    /// Whether the first sample passed the host-dependent configuration checks.
    host_checked: bool,
    /// Bounds the launch preparations that run at once.
    launch_gate: std::sync::Arc<tokio::sync::Semaphore>,
    /// Which journal records each iteration reads (FR-081).
    history: super::history::HistoryScope,
    /// Observations written since `observations` was last pruned.
    observations_since_prune: u32,
    /// When this iteration's sample was taken.
    sampled_at: Instant,
    /// When the newest sample whose steps reached the supervisors was taken; the loop's start
    /// until the first one.
    acted_sample_at: Instant,
    /// Names a stretch of withheld renewals once.
    withheld: Stretch,
    /// How long one coordinator request may hold the loop.
    rpc_budget: Duration,
    /// A signal stopped the agent: supervisors keep running for re-adoption (FR-080).
    leave_supervisors: bool,
    /// The wall time of the last observation write, the cost of one synced journal write,
    /// which sizes a draining stop's share of drains per iteration (FR-080).
    store_write: Duration,
    /// What is left of this iteration's share of new drains.
    drain_quota: usize,
    /// Live supervisors that did not answer re-adoption, with the next try and its backoff.
    readopt: Vec<(ExecutionRecord, Instant, Duration)>,
}

impl AgentLoop<'_> {
    /// Finished preparations become slots; refused launches get a durable refusal.
    async fn collect_preparations(&mut self) -> Result<()> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
        let ready: Vec<_> = self
            .preparations
            .iter()
            .filter(|(_, (_, h, _))| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let (assignment, handle, revoked) =
                self.preparations.remove(&id).expect("listed preparation");
            // A preparation task that panicked refuses its launch, as a failed one does.
            let prepared = handle
                .await
                .unwrap_or_else(|join| Err(anyhow::anyhow!("preparation task failed: {join}")));
            match prepared {
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
            if self.publications[handle_index].2.is_finished() {
                let (id, checkpoint, handle) = self.publications.swap_remove(handle_index);
                let (id, checkpoint, hash, result) = handle.await.unwrap_or_else(|join| {
                    (
                        id,
                        checkpoint,
                        String::new(),
                        Err(anyhow::anyhow!("publication task failed: {join}")),
                    )
                });
                self.publishing.remove(&(id.clone(), checkpoint));
                match result {
                    Ok(()) => {
                        if let Some(slot) = self.slots.get_mut(&id) {
                            if checkpoint {
                                record_checkpoint(&slot.output_dir, &hash)?;
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
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
        let interval = Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
        let lease = Duration::from_millis(config.lifecycle.allocation_lease_ms);
        let sample_age = self.acted_sample_at.elapsed();
        // Past one lease without a sample acted on, no Opportunistic renewal is asked and those
        // leases run out; a Guaranteed lease bounds nothing and its reply carries drains (FR-079).
        let renew = renewals_asked(sample_age, lease);
        let mut withheld = 0usize;
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
                        outside_family,
                        outside_reason,
                        family,
                        ..
                    }) => {
                        slot.applied = applied;
                        slot.applied_targets = targets;
                        slot.note_readiness(hint_ready, observes_drain);
                        slot.note_outside(outside_family, outside_reason);
                        if family.is_some() {
                            slot.family = family;
                            slot.keep_family(&self.config.state_dir);
                        }
                    }
                    Ok(Event::OutsideFamily { count, reason }) => slot.note_outside(count, reason),
                    Ok(Event::Family { family }) => {
                        let known = slot.descendants();
                        slot.family = Some(family);
                        slot.keep_family(&self.config.state_dir);
                        // New members get a baseline now, so their run's next interval is
                        // measured rather than unknown.
                        let joined: Vec<_> = slot
                            .descendants()
                            .into_iter()
                            .filter(|member| !known.contains(member))
                            .collect();
                        if let Some(record) = &slot.prepared {
                            self.collector
                                .prime_descendants(std::slice::from_ref(record), &joined);
                        }
                    }
                    Ok(Event::Readiness {
                        hint_ready,
                        observes_drain,
                    }) => slot.note_readiness(hint_ready, observes_drain),
                    // Each OOM preference change is evidence: task, from, to, cause, own
                    // RAM and its fair share (FR-023).
                    Ok(Event::Oom {
                        preferred,
                        changes,
                        detail,
                    }) => {
                        emit(&serde_json::json!({
                            "event": "oom_score_adj",
                            "task_id": slot.assignment.request.task_id,
                            "assignment_id": slot.assignment.request.assignment_id,
                            "preferred": preferred,
                            "changes": changes,
                            "context": slot.oom_context.take(),
                            "detail": detail,
                        }))?;
                    }
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
            } else if running
                && !slot.exited
                && slot.sequence > 0
                && slot.last_renew.elapsed() >= interval
            {
                // A supervisor that has exited is renewed no lease: nobody runs that work (R83).
                if renew
                    || slot.assignment.request.class
                        == crate::execution_model::AllocationClass::Guaranteed
                {
                    waiting.push((slot.last_renew + interval, id.clone()));
                } else {
                    withheld += 1;
                }
            }
            records.insert(id.clone(), record);
        }
        if self.withheld.starts(withheld > 0) {
            emit(&serde_json::json!({
                "event": "renewals_withheld",
                "code": "renewals_withheld",
                "allocations": withheld,
                "sample_age_ms": sample_age.as_millis() as u64,
                "limit_ms": lease.as_millis() as u64,
                "detail": "no sample has been acted on for one lease; Opportunistic renewals wait for the next one",
            }))?;
        }
        // Requests run one at a time and each costs a synced coordinator write, so they share
        // half the monitor interval; the rest wait for the next iteration (research R19).
        lease_order(&mut waiting);
        let started = Instant::now();
        let budget = Duration::from_millis(config.monitor.interval_ms / 2);
        let mut sent = 0;
        // Failure reports keep a floor against the control lane (FR-034).
        let mut failures = 0;
        let floor = failure_floor(Duration::from_millis(config.monitor.interval_ms));
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
                match bounded(
                    self.rpc_budget,
                    self.client.request_leases(&batch, &dues, interval),
                )
                .await
                {
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
                    let result = bounded(
                        self.rpc_budget,
                        self.client.request(&Request::Prepared {
                            assignment_id: record.assignment_id.clone(),
                            generation: record.generation,
                            coordinator_epoch: slot.assignment.coordinator_epoch,
                            record: record.clone(),
                        }),
                    )
                    .await;
                    authorization_reply(slot, requested, result)?;
                } else {
                    let requested = Instant::now();
                    let due = next_control_due(&mut slot.last_renew, interval);
                    let result = bounded(
                        self.rpc_budget,
                        self.client.request_control(
                            &Request::Renew {
                                assignment_id: slot.assignment.request.assignment_id.clone(),
                                generation: slot.assignment.generation,
                                coordinator_epoch: slot.assignment.coordinator_epoch,
                                previous_sequence: slot.sequence,
                            },
                            due,
                            interval,
                        ),
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
                // A failure with nothing to publish waits for its turn unpinned; its verdict is
                // taken at the first read all the same. Every read that can lead to a
                // publication is made again under the pin (FR-080).
                if !checkpoint
                    && !failure_turn(
                        sent,
                        started.elapsed(),
                        budget,
                        failures,
                        floor,
                        self.client
                            .lanes
                            .control
                            .behind_at(tokio::time::Instant::now()),
                    )
                {
                    let outcome =
                        ExecutionOutcome::read(&slot.output_dir.join("execution-outcome.json"))
                            .ok();
                    if let Some(Some(outcome)) = outcome.as_ref() {
                        take_oom_verdict(&self.oom_kills, slot, outcome)?;
                    }
                    let failure = match &outcome {
                        Some(Some(o)) => Some(o.exit_code != Some(0) || o.yielded),
                        Some(None) => Some(supervisor_exited),
                        None => None,
                    };
                    let descriptor =
                        crate::publication::read_head(&self.store, &slot.output_dir, "result")
                            .ok()
                            .map(|bytes| bytes.is_some());
                    if !pins_result(failure, descriptor, false) {
                        continue;
                    }
                }
                let Some(publication_pin) = self.pins.take(|| {
                    crate::publication_gc::pin_attempt_for_report(&self.store, &slot.output_dir)
                })?
                else {
                    continue;
                };
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
                    // A read that failed decides no outcome; it is read again.
                    let Ok(outcome) =
                        ExecutionOutcome::read(&slot.output_dir.join("execution-outcome.json"))
                    else {
                        continue;
                    };
                    if outcome.is_none() && !supervisor_exited {
                        continue;
                    }
                    if let Some(outcome) = outcome.as_ref() {
                        take_oom_verdict(&self.oom_kills, slot, outcome)?;
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
                            if !failure_turn(
                                sent,
                                started.elapsed(),
                                budget,
                                failures,
                                floor,
                                self.client
                                    .lanes
                                    .control
                                    .behind_at(tokio::time::Instant::now()),
                            ) {
                                continue;
                            }
                            sent += 1;
                            failures += 1;
                            let released = &record
                                .as_ref()
                                .expect("record was checked released above")
                                .detail;
                            let (not_own_fault, detail) = match &slot.oom_verdict {
                                Some(oom::OomVerdict::NotOwnFault(why)) => {
                                    (true, format!("{why}; {released}"))
                                }
                                Some(oom::OomVerdict::OwnFault(why)) => {
                                    (false, format!("{why}; {released}"))
                                }
                                _ => (false, released.clone()),
                            };
                            if matches!(
                                bounded(
                                    self.rpc_budget,
                                    self.client.request(&Request::Fail {
                                        assignment_id: id.clone(),
                                        generation: slot.assignment.generation,
                                        detail,
                                        failure_kind: if not_own_fault
                                            || outcome.as_ref().is_some_and(|o| o.yielded)
                                        {
                                            FailureKind::Yielded
                                        } else {
                                            FailureKind::ExecutionFailure
                                        },
                                        yield_cause: (not_own_fault && self.negotiated.contention)
                                            .then_some(
                                                crate::contention::YieldCause::OomNotOwnFault
                                            ),
                                    })
                                )
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
                let key = id.clone();
                self.publications.push((
                    key,
                    checkpoint,
                    tokio::spawn(async move {
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
                    }),
                ));
            }
            if let Some(status) = slot.child.exited()?
                && let Some(mut record) = self
                    .store
                    .execution_record(&slot.assignment.request.assignment_id)?
                && record.phase != ExecutionPhase::Released
            {
                // Reaping synchronizes with the supervisor's last durable write.
                // The earlier loop snapshot may predate its Released commit. A Tracked run's
                // members are recovered by identity: Guaranteed continued, Opportunistic
                // ended (FR-088).
                let recovered =
                    super::recovery::recover(&self.config.state_dir, &self.store, &record)
                        .unwrap_or_else(|error| {
                            let _ = emit(&serde_json::json!({
                                "event": "family_recovery_failed",
                                "assignment_id": record.assignment_id,
                                "detail": format!("{error:#}"),
                            }));
                            None
                        });
                record.phase = ExecutionPhase::NeedsReconciliation;
                record.detail = format!(
                    "supervisor exited {status}; existing allocation remains charged{}",
                    recovered.map_or(String::new(), |r| format!("; members: {}", r.summary()))
                );
                self.store.transition(&record)?;
            }
        }
        Ok(())
    }

    fn retire_slots(&mut self, records: &[ExecutionRecord]) -> Result<()> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
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
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
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
            if self
                .history
                .upgrade_held(&self.store, &record.assignment_id)?
            {
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
                let Ok(outcome) = ExecutionOutcome::read(&output.join("execution-outcome.json"))
                else {
                    continue;
                };
                let normal_exit = outcome.is_some_and(|o| o.exit_code == Some(0) && !o.yielded);
                let Some(publication_pin) = self
                    .pins
                    .take(|| crate::publication_gc::pin_attempt_for_report(&self.store, &output))?
                else {
                    continue;
                };
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
                    let key = id.clone();
                    self.publications.push((
                        key,
                        false,
                        tokio::spawn(async move {
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
                        }),
                    ));
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

    /// CedeGrid's own CPU this interval, from the agent's process clock and its live
    /// supervisors' (FR-069). Without the agent's clock it stays unknown.
    fn note_cedegrid_cpu(&mut self, snapshot: &mut crate::model::Snapshot) {
        #[cfg(target_os = "linux")]
        {
            let Some(agent_ns) = crate::telemetry::own_process_cpu_ns() else {
                return;
            };
            let mut supervisors = Vec::new();
            for (id, slot) in &mut self.slots {
                let ns = match &mut slot.child {
                    // Unreaped, so the PID is still this child's.
                    SupervisorProcess::Owned(child) => match child.try_wait() {
                        Ok(None) => crate::telemetry::supervisor_cpu_ns(child.id(), None),
                        _ => None,
                    },
                    SupervisorProcess::Attached(identity) => {
                        crate::telemetry::supervisor_cpu_ns(identity.pid, Some(identity))
                    }
                };
                if let Some(ns) = ns {
                    supervisors.push((id.clone(), ns));
                }
            }
            if let Some((interval, window)) =
                self.cedegrid_cpu
                    .sample(Instant::now(), agent_ns, &supervisors)
            {
                snapshot.cedegrid_cpu_lower_millicores = Some(interval);
                snapshot.cedegrid_cpu_window_lower_millicores = Some(window);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (snapshot, &self.cedegrid_cpu);
    }

    async fn observe(
        &mut self,
        records: &[ExecutionRecord],
    ) -> Result<(
        crate::model::Snapshot,
        Vec<crate::model::Allocation>,
        crate::model::Decision,
    )> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
        let agent = self.agent;
        // Absolute monitor cadence can arrive just before the OS CPU sampling
        // interval. Wait for fresh evidence after pending control/reaping work;
        // do not substitute cached utilization or extend control deadlines.
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            self.collector.next_cpu_sample_at(),
        ))
        .await;
        let mut mark = LoopCost::thread_ns();
        let descendants: Vec<_> = self
            .slots
            .values()
            .flat_map(ChildSlot::descendants)
            .collect();
        let (mut snapshot, allocations) = self.collector.sample_with_descendants(
            records,
            &self.store.all_unreleased_managed_children()?,
            &descendants,
        )?;
        self.sampled_at = Instant::now();
        mark = self.cost.charge("sample", mark);
        self.oom_kills.push_back(crate::telemetry::host_oom_kills());
        if self.oom_kills.len() > 3 {
            self.oom_kills.pop_front();
        }
        self.note_cedegrid_cpu(&mut snapshot);
        mark = self.cost.charge("cedegrid_clocks", mark);
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
        let mut input = PolicyInput {
            allocations,
            // A signal stop leaves running work to the next agent (FR-080).
            explicit_drain: self.stopping.is_some() && !self.leave_supervisors,
            facts: BTreeMap::new(),
            envelope: Some(agent.capacity.clone()),
            spool_full: self.spool_full,
            // Unreadable arrivals leave own work's arrival unknown: later than any first-comer.
            arrivals: self.store.arrivals().unwrap_or_default(),
        };
        input.facts = self.allocation_facts(records, &input.allocations);
        let estimated = self.collector.ram_estimated().clone();
        let collector = &mut self.collector;
        let mut decision = crate::agent::confirm_memory_steps(
            &mut self.policy,
            config,
            &snapshot,
            &mut input,
            self.start.elapsed().as_millis() as u64,
            &estimated,
            |runs| collector.fresh_family_ram(runs),
        )?;
        let allocations = input.allocations;
        let mark = self.cost.charge("policy", mark);
        decision.managed_budget = minimum(&decision.managed_budget, &agent.capacity);
        decision.observe_only = false;
        decision.admission_headroom = minimum(
            &decision.admission_headroom,
            &remaining_capacity(
                &schedulable_budget(&decision),
                &allocation_charge(&allocations),
            ),
        );
        self.cost.charge("decision", mark);
        self.note_ram_views(&snapshot, &allocations, &decision);
        Ok((snapshot, allocations, decision))
    }

    /// Each running task's view of RAM, kept for judging an OOM kill after it (FR-076).
    fn note_ram_views(
        &mut self,
        snapshot: &crate::model::Snapshot,
        allocations: &[crate::model::Allocation],
        decision: &crate::model::Decision,
    ) {
        let own = decision
            .resources
            .get(&crate::contention::ResourceKey::Ram)
            .and_then(|status| status.own)
            .map(|(lower, _)| lower);
        let status = decision.resources.get(&crate::contention::ResourceKey::Ram);
        let breached = status.is_some_and(|status| {
            status
                .reasons
                .iter()
                .any(|reason| reason.code == "floor_breach")
        });
        // Own RAM when the current floor breach began, for FR-076's rule in the modes that
        // keep own memory at a breach.
        self.ram_breach_start = match (breached, own) {
            (true, Some(own)) => Some(self.ram_breach_start.unwrap_or(own)),
            (true, None) => self.ram_breach_start,
            (false, _) => None,
        };
        let breach_start = self.ram_breach_start;
        let tolerance_mib = self
            .config
            .contention
            .thresholds(crate::contention::ResourceKind::Ram)
            .noise_tolerance_mib;
        let view = own
            .zip(decision.ram_fair_share_mib)
            .map(|(own_mib, fair_share_mib)| oom::RamView {
                keeps_memory: Some(oom::Breach {
                    growth_mib: breach_start.map(|start| own_mib.saturating_sub(start)),
                    tolerance_mib,
                }),
                own_mib,
                fair_share_mib,
                above: snapshot
                    .principals
                    .iter()
                    .flatten()
                    .filter(|p| !p.is_owner && p.ram_mib > fair_share_mib)
                    .map(|p| (p.uid, p.ram_mib))
                    .collect(),
            });
        // Each task is judged by its own RAM mode (FR-030).
        for (id, slot) in self.slots.iter_mut() {
            if slot.exited {
                continue;
            }
            let Some(allocation) = allocations
                .iter()
                .find(|a| &a.id == id && a.observed.is_some())
            else {
                continue;
            };
            let mut slot_view = view.clone();
            if let Some(slot_view) = slot_view.as_mut()
                && !oom::keeps_memory(&self.config, &allocation.modes)
            {
                slot_view.keeps_memory = None;
            }
            slot.ram_view = slot_view;
        }
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
        let draining_stop = self.stopping.is_some() && !self.leave_supervisors;
        self.drain_quota = stop_drain_quota(self.store_write);
        dispatch_drains(
            &mut self.slots,
            &decision.would_drain,
            &decision.actions,
            draining_stop,
            &mut self.drain_quota,
        );
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
                        // A GPU compute cap goes to its supervisor as the duty to apply.
                        target: action.duty.or(action.target),
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
        // OOM order by fault: a task's mark goes to its supervisor when it changes (FR-023).
        // In a System mode unit the broker writes every score instead (005 T049).
        let marks_here = !crate::system_mode::in_unit();
        let own_ram = decision
            .resources
            .get(&crate::contention::ResourceKey::Ram)
            .and_then(|status| status.own)
            .map(|(lower, _)| lower);
        for (id, slot) in self.slots.iter_mut() {
            if !marks_here || slot.exited || slot.sequence == 0 {
                continue;
            }
            let preferred = decision.oom_preferred.contains(id.as_str());
            if slot.sent_oom == Some(preferred) {
                continue;
            }
            let cause = match (preferred, self.config.ram.prefer_own_oom_victim) {
                (true, true) => "prefer_own_oom_victim",
                (true, false) => "own_memory_above_fair_share",
                (false, _) => "own_memory_within_fair_share",
            };
            if send(slot, &Control::Oom { preferred }).is_ok() {
                slot.sent_oom = Some(preferred);
                slot.oom_context = Some(serde_json::json!({
                    "cause": cause,
                    "own_ram_mib": own_ram,
                    "ram_fair_share_mib": decision.ram_fair_share_mib,
                }));
            }
        }
        // A save-progress request goes once, as the policy lists it once per episode (FR-075).
        for id in &decision.save_progress {
            let Some(slot) = self.slots.get_mut(id.as_str()) else {
                continue;
            };
            if slot.exited || slot.sequence == 0 || slot.drain_sent_at.is_some() {
                continue;
            }
            if send(slot, &Control::SaveProgress).is_ok() {
                emit(&serde_json::json!({
                    "event": "save_progress",
                    "task_id": slot.assignment.request.task_id,
                    "assignment_id": slot.assignment.request.assignment_id,
                }))?;
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
                facts.ram_cap = ram_cap_available(&self.config, backend);
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
        snapshot: &crate::model::Snapshot,
        allocations: &[crate::model::Allocation],
        decision: &crate::model::Decision,
    ) -> Result<()> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
        let agent = self.agent;
        let executable = self.executable;
        // A stopping agent keeps the heartbeat interval too (FR-080).
        if !heartbeat_due(
            self.last_heartbeat.elapsed(),
            Duration::from_millis(self.config.lifecycle.heartbeat_interval_ms),
        ) {
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
        let reconciled_records = self.store.heartbeat_executions()?;
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
            >= Some(crate::contention::ContentionMode::Performance);
        let launch_slots = if admission_safe && self.stopping.is_none() && !self.spool_full {
            let paced = launch_slots(LaunchPacing {
                max_workers: agent.max_workers,
                active: active_workers,
                admitted: self.admitted_parallelism,
                may_grow,
                fill,
            });
            // Launches whose descriptors would not fit with headroom are refused (FR-077).
            let room = descriptor_room(self.preparations.len() as u64) as usize;
            if (paced > room) != self.descriptor_limited {
                self.descriptor_limited = paced > room;
                if self.descriptor_limited {
                    emit(&serde_json::json!({
                        "event": "launch_refused",
                        "code": "descriptor_limit",
                        "launch_slots": room,
                        "detail": "launches whose descriptors do not fit under the agent's limit with 64 kept free are refused",
                    }))?;
                }
            }
            paced.min(room) as u32
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
            // The age of this iteration's sample on this host's monotonic clock (FR-079).
            sample_age_ms: self
                .negotiated
                .report_age
                .then(|| self.sampled_at.elapsed().as_millis().min(u64::MAX as u128) as u64),
        };
        self.negotiated.apply(&mut report, || {
            node_contention_report(config, decision, agent.max_workers)
        });
        // Report preparation can cross a scheduled slot. Advance at actual
        // enqueue so the next due time agrees with recorded lateness.
        let due = next_control_due(&mut self.last_heartbeat, interval);
        match bounded(
            self.rpc_budget,
            self.client
                .request_control(&Request::Heartbeat { report }, due, interval),
        )
        .await
        {
            Ok(Response::Heartbeat { reply }) => {
                self.negotiated.learn(&reply);
                for slot in self.slots.values_mut() {
                    slot.assignment.coordinator_epoch = reply.coordinator_epoch;
                }
                let remote_drains: BTreeSet<_> = reply.drain.iter().cloned().collect();
                for id in &reply.drain {
                    if let Some((_, _, revoked)) = self.preparations.get_mut(id) {
                        *revoked = true;
                    }
                }
                apply_coordinator_drains(&mut self.slots, &reply.drain, &mut self.drain_quota);
                let mut acknowledged = vec![];
                // Refusal reports share half the monitor interval, like lease requests; the
                // rest are reported at the next heartbeat (research R19).
                let reports_started = Instant::now();
                let budget = Duration::from_millis(config.monitor.interval_ms / 2);
                let floor = failure_floor(Duration::from_millis(config.monitor.interval_ms));
                for (reported, (id, (assignment, detail))) in self.refusals.iter().enumerate() {
                    if !failure_turn(
                        reported,
                        reports_started.elapsed(),
                        budget,
                        reported,
                        floor,
                        self.client
                            .lanes
                            .control
                            .behind_at(tokio::time::Instant::now()),
                    ) {
                        break;
                    }
                    let Ok(outcome) = ExecutionOutcome::read(
                        &config
                            .state_dir
                            .join("attempts")
                            .join(id)
                            .join("execution-outcome.json"),
                    ) else {
                        continue;
                    };
                    let yielded = outcome.is_some_and(|o| o.yielded);
                    if matches!(
                        bounded(
                            self.rpc_budget,
                            self.client.request(&Request::Fail {
                                assignment_id: id.clone(),
                                generation: assignment.generation,
                                detail: detail.clone(),
                                // The coordinator classifies by its recorded cause;
                                // the node adds only its journal-backed outcome.
                                failure_kind: if yielded {
                                    FailureKind::Yielded
                                } else {
                                    FailureKind::ExecutionFailure
                                },
                                yield_cause: None,
                            })
                        )
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
                for assignment in reply.assignments {
                    let id = assignment.request.assignment_id.clone();
                    let journaled = self.store.execution_record(&id)?.is_some();
                    if remote_drains.contains(&id)
                        && !self.slots.contains_key(&id)
                        && !self.preparations.contains_key(&id)
                        && !journaled
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
                        || journaled
                    {
                        continue;
                    }
                    let active = self.slots.values().filter(|s| !s.reported).count()
                        + self.preparations.len();
                    if active >= agent.max_workers || self.stopping.is_some() {
                        continue;
                    }
                    // In a System mode unit the administrator policy limits modes and tasks; an
                    // offer beyond it is refused once and left to the coordinator (005 FR-010).
                    if crate::system_mode::in_unit()
                        && let Some((code, detail)) =
                            crate::system_mode::launch_refusal(config, &assignment.request, active)
                    {
                        if self.policy_refused.insert(id.clone()) {
                            emit(&serde_json::json!({
                                "event": "launch_refused",
                                "code": code,
                                "assignment_id": id,
                                "detail": detail,
                            }))?;
                        }
                        continue;
                    }
                    // An unprotected grant starts only under the host's allowance and, in a
                    // unit, the administrator key (006 FR-015). A later attempt of a run that
                    // started unprotected here keeps the allowance it began under.
                    let grant = assignment
                        .request
                        .contention
                        .as_ref()
                        .and_then(|contention| contention.unprotected);
                    let allowance = grant.map(|grant| {
                        crate::state::allowance_for_run(
                            &config.state_dir,
                            &assignment.request.task_id,
                            grant.requested_unix_ms,
                        )
                    });
                    if let Some((code, detail)) = crate::system_mode::grant_refusal(
                        assignment.request.contention.as_ref(),
                        || {
                            allowance
                                .clone()
                                .unwrap_or(crate::state::Allowance::Off { reason: None })
                        },
                        || {
                            crate::system_mode::in_unit()
                                .then(|| {
                                    crate::system_mode::unprotected_refusal_with(
                                        crate::broker::admin_policy::load(),
                                    )
                                })
                                .flatten()
                        },
                    ) {
                        if self.policy_refused.insert(id.clone()) {
                            emit(&serde_json::json!({
                                "event": "launch_refused",
                                "code": code,
                                "assignment_id": id,
                                "detail": detail,
                            }))?;
                        }
                        continue;
                    }
                    // A launch whose descriptors do not fit with headroom is refused and the
                    // offer left to the coordinator (FR-077).
                    if descriptor_room(self.preparations.len() as u64) == 0 {
                        if self.descriptor_refused.insert(id.clone()) {
                            emit(&serde_json::json!({
                                "event": "launch_refused",
                                "code": "descriptor_limit",
                                "assignment_id": id,
                                "detail": "the launch's descriptors do not fit under the agent's limit with 64 kept free",
                            }))?;
                        }
                        continue;
                    }
                    if let (Some(grant), Some(crate::state::Allowance::On { set_at_unix_ms })) =
                        (grant, &allowance)
                    {
                        // Recorded so later attempts of this run keep the grant; without the
                        // record they read the host's allowance again, which is the safe side.
                        if let Err(error) = crate::state::record_grant(
                            &config.state_dir,
                            &assignment.request.task_id,
                            grant.requested_unix_ms,
                            *set_at_unix_ms,
                            super::reconcile::now_unix_ms(),
                        ) {
                            emit(&serde_json::json!({
                                "event": "unprotected_grant_unrecorded",
                                "assignment_id": id,
                                "detail": format!("later attempts will read the allowance again: {error:#}"),
                            }))?;
                        }
                        emit(&serde_json::json!({
                            "event": "unprotected",
                            "assignment_id": id,
                            "detail": "warning: this run is UNPROTECTED: other users' compute may slow down until it ends",
                        }))?;
                    } else if assignment
                        .request
                        .contention
                        .as_ref()
                        .is_some_and(|contention| contention.admit_immediately.is_some())
                    {
                        emit(&serde_json::json!({
                            "event": "admit_immediately",
                            "assignment_id": id,
                            "detail": "caution: this run starts beside CedeGrid users that arrived first and yield",
                        }))?;
                    }
                    // The run's arrival goes on the supervisor's marker and into the journal, so
                    // other accounts' agents and this one read the same order (006 FR-005). An
                    // unrecorded arrival leaves own work later than any first-comer: the safe side.
                    let now = super::reconcile::now_unix_ms();
                    let devices = assignment
                        .request
                        .resources
                        .gpu_memory_mib
                        .keys()
                        .cloned()
                        .collect();
                    let arrival = match self.store.launch_arrival(&devices, now).and_then(
                        |arrival| self.store.record_arrival(&id, arrival).map(|()| arrival),
                    ) {
                        Ok(arrival) => arrival,
                        Err(error) => {
                            emit(&serde_json::json!({
                                "event": "arrival_unrecorded",
                                "assignment_id": id,
                                "detail": format!("own work counts as arriving later: {error:#}"),
                            }))?;
                            now
                        }
                    };
                    let task_config = config.clone();
                    let task_agent = agent.clone();
                    let task_assignment = assignment.clone();
                    // Admit immediately needs no allowance; unprotected, which implies it, has
                    // passed the allowance check above (006 FR-012, FR-015).
                    let admits = assignment.request.contention.as_ref().is_some_and(|c| {
                        c.admit_immediately.is_some()
                            || (c.unprotected.is_some()
                                && matches!(allowance, Some(crate::state::Allowance::On { .. })))
                    });
                    let task_capacity = launch_capacity(
                        schedulable_budget(decision),
                        decision.unprotected_budget.as_ref(),
                        admits,
                    );
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
                                arrival,
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

    /// Try missed re-adoptions that are due again (FR-080).
    fn readopt_supervisors(&mut self) -> Result<()> {
        #[cfg(unix)]
        for (record, next, backoff) in std::mem::take(&mut self.readopt) {
            let id = record.assignment_id.clone();
            if self.slots.contains_key(&id) {
                continue;
            }
            if Instant::now() < next {
                self.readopt.push((record, next, backoff));
                continue;
            }
            match super::reconcile::reattach_again(self.config.as_ref(), &record) {
                Ok(Some(slot)) => {
                    self.slots.insert(id, slot);
                }
                Ok(None) => {
                    let backoff = (backoff * 2).min(READOPT_MAX);
                    self.readopt
                        .push((record, Instant::now() + backoff, backoff));
                }
                Err(error) => emit(
                    &serde_json::json!({"event":"supervisor_not_reattached","assignment_id":id,"detail":format!("{error:#}")}),
                )?,
            }
        }
        Ok(())
    }

    /// Reload the configuration file on SIGHUP: a change to `[contention]` applies from the
    /// next iteration; any other change is refused and the running settings stay (FR-080).
    fn reload_config(&mut self) -> Result<()> {
        let Some(path) = self.config_path.clone() else {
            return emit(
                &serde_json::json!({"event":"config_reload_refused","detail":"the agent runs without a configuration file"}),
            );
        };
        let next = crate::config::load_for_run(&path)
            .and_then(|(next, _)| crate::config::reloadable(&self.config, &next));
        match next {
            Ok(Ok(applied)) => {
                self.config = std::sync::Arc::new(applied);
                emit(
                    &serde_json::json!({"event":"config_reloaded","detail":"contention settings apply from the next iteration"}),
                )
            }
            Ok(Err(keys)) => emit(
                &serde_json::json!({"event":"config_reload_refused","keys":keys,"detail":"these settings need a restart; the running ones stay"}),
            ),
            Err(error) => emit(
                &serde_json::json!({"event":"config_reload_refused","detail":format!("{error:#}")}),
            ),
        }
    }

    /// Whether the stop sequence finished or its release deadline elapsed.
    fn stop_due(&mut self, records: &[ExecutionRecord]) -> Result<bool> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
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
            // A signal stop ends once finished runs are reported and only live supervisors,
            // left to the next agent, remain (FR-080).
            if self.leave_supervisors
                && self.publications.is_empty()
                && self.preparations.is_empty()
                && self.slots.values().all(|slot| !slot.exited)
            {
                return Ok(true);
            }
            let limit = config.lifecycle.drain_timeout_ms
                + config.lifecycle.term_grace_ms
                + config.execution.release_confirm_timeout_ms
                + config.execution.prepare_timeout_ms;
            // A spread stop counts its deadline from the last drain it sent.
            let last_drain = self
                .slots
                .values()
                .filter_map(|slot| slot.drain_sent_at)
                .fold(stop, Instant::max);
            if last_drain.elapsed() >= Duration::from_millis(limit) {
                let unreleased = records
                    .iter()
                    .filter(|r| r.phase != ExecutionPhase::Released)
                    .count();
                let live = self.slots.values().filter(|slot| !slot.exited).count();
                let unanswered = self
                    .slots
                    .values()
                    .filter(|slot| slot.exited && !slot.reported)
                    .count();
                let detail = stop_uncertain_detail(
                    unreleased,
                    live,
                    unanswered,
                    self.publications.len() + self.preparations.len(),
                );
                emit(&serde_json::json!({"event":"stop_uncertain","detail":detail}))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn shutdown(mut self) -> Result<()> {
        let config = std::sync::Arc::clone(&self.config);
        let config = &*config;
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
        // We deliberately never signal an uncertain supervisor/workload by PID. A signal
        // stop leaves supervisors to the next agent (FR-080); other stops drain.
        for slot in self.slots.values_mut() {
            if !self.leave_supervisors {
                drain(slot, None);
            }
            let _ = slot.child.exited()?;
        }
        let retained: Vec<_>=self.slots.iter().map(|(id,slot)|serde_json::json!({"assignment_id":id,"exited":slot.exited,"reported":slot.reported,"publishing":self.publishing.iter().filter(|(publishing_id,_)|publishing_id==id).collect::<Vec<_>>(),"result_receipt":slot.output_dir.join("result.receipt.json").is_file(),"failure_receipt":slot.output_dir.join("failure.receipt.json").is_file()})).collect();
        let status = serde_json::json!({"event":"agent_stopped","node_id":config.node_id,"elapsed_ms":self.start.elapsed().as_millis(),"retained_slots":self.slots.len(),"retained_slot_details":retained,"supervisors_left_running":self.leave_supervisors,"executions":self.store.heartbeat_executions()?});
        emit(&status)?;
        Ok(())
    }
}

/// Run a node service until shutdown or its configured finite envelope. Every
/// managed workload has a separate supervisor; disconnects do not stop its clock.
/// How often the agent reports its loop's own CPU by stage.
const LOOP_COST_PERIOD: Duration = Duration::from_secs(60);

pub async fn run(
    config: &Config,
    config_path: Option<&Path>,
    agent: &AgentConfig,
    executable: &Path,
) -> Result<()> {
    ensure!(
        config.execution.enabled,
        "agent execution requires execution.enabled=true"
    );
    // Room for many tasks' descriptors; each task gets the starting limit back (FR-077).
    #[cfg(unix)]
    raise_descriptor_limit()?;
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
    // Replay mode re-adopts too: re-adoption checks each supervisor's identity and token.
    #[cfg(unix)]
    let (attached, missed) = reattach_supervisors(config, &store)?;
    #[cfg(not(unix))]
    let (attached, missed): (Vec<ChildSlot>, Vec<ExecutionRecord>) = (Vec::new(), Vec::new());
    let attached_ids: BTreeSet<String> = attached
        .iter()
        .map(|slot| slot.assignment.request.assignment_id.clone())
        .collect();
    let _ = reconcile_guarded(config, &attached_ids, namespace_guard.clone(), false)?;
    let mut client = NodeClient::new(
        agent,
        &config.state_dir,
        config.storage_profile,
        namespace_guard.clone(),
    )?;
    // The control lane keeps at most one heartbeat interval owed.
    client.lanes.cap = Duration::from_millis(config.lifecycle.heartbeat_interval_ms)
        .max(Duration::from_millis(config.monitor.interval_ms));
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
    // Installed before the loop: SIGHUP's default action would end the agent.
    let mut hangup = reload_signal_stream();
    let history = super::history::HistoryScope::new(&store, &config.state_dir)?;
    let mut state = AgentLoop {
        config: std::sync::Arc::new(config.clone()),
        config_path: config_path.map(Path::to_path_buf),
        agent,
        executable,
        store,
        pins: PinGate::default(),
        client,
        namespace_guard,
        collector,
        cedegrid_cpu: crate::telemetry::CedegridCpu::default(),
        cost: LoopCost::new(start, LOOP_COST_PERIOD),
        policy,
        slots: BTreeMap::new(),
        ram_breach_start: None,
        refusals,
        publications: Vec::new(),
        publishing: BTreeSet::new(),
        preparations: BTreeMap::new(),
        start,
        last_heartbeat: start - Duration::from_millis(config.lifecycle.heartbeat_interval_ms),
        negotiated: NegotiatedFeatures::default(),
        oom_kills: std::collections::VecDeque::new(),
        stopping: None,
        last_spool_check: Instant::now(),
        spool_full: false,
        admitted_parallelism: 0,
        last_growth: start,
        counted_drains: BTreeSet::new(),
        descriptor_refused: BTreeSet::new(),
        policy_refused: BTreeSet::new(),
        descriptor_limited: false,
        floor_warnings: BTreeSet::new(),
        host_checked: false,
        launch_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(LAUNCH_CONCURRENCY)),
        history,
        // Prune on the first iteration, then every `OBSERVATIONS_PER_PRUNE`.
        observations_since_prune: OBSERVATIONS_PER_PRUNE - 1,
        sampled_at: start,
        acted_sample_at: start,
        withheld: Stretch::default(),
        rpc_budget: observation_interval.max(Duration::from_millis(250)),
        leave_supervisors: false,
        store_write: Duration::ZERO,
        drain_quota: 0,
        readopt: missed
            .into_iter()
            .map(|record| (record, Instant::now() + READOPT_FIRST, READOPT_FIRST))
            .collect(),
    };
    for slot in attached {
        state
            .slots
            .insert(slot.assignment.request.assignment_id.clone(), slot);
    }
    let (mut held_stretch, mut pin_stretch) = (Stretch::default(), Stretch::default());
    loop {
        // Anchor observations to their scheduled cadence. Processing and RPC
        // time consume this interval instead of extending it. After an overrun,
        // run once immediately and return to the original cadence.
        next_observation =
            next_periodic_wake(next_observation, observation_interval, Instant::now());
        let iteration: Result<bool> = async {
            let mut mark = LoopCost::thread_ns();
            state.pins = PinGate::default();
            if let Some(guard) = &replay_journal {
                guard.verify(config)?;
            }
            state.collect_preparations().await?;
            state.collect_publications().await?;
            mark = state.cost.charge("collect", mark);
            state.readopt_supervisors()?;
            state.service_slots().await?;
            mark = state.cost.charge("slots", mark);
            let records = state.history.records(
                &state.store,
                state.slots.keys().chain(state.preparations.keys()),
            )?;
            collect_released_spool(
                config,
                &state.store,
                &state.history,
                &records,
                &state.publishing,
            )?;
            state.retire_slots(&records)?;
            state.recover_outbox(&records)?;
            state.history.settle(&config.state_dir);
            state.cost.charge("records", mark);
            // Observation charges its own stages.
            let (snapshot, allocations, mut decision) = state.observe(&records).await?;
            mark = LoopCost::thread_ns();
            state.share_observation(&snapshot);
            state.actuate(&mut decision, &allocations)?;
            // This interval acted on its sample: renewals may go on (FR-079).
            state.acted_sample_at = state.sampled_at;
            mark = state.cost.charge("actuate", mark);
            let before = state.store.change_key()?;
            let written = Instant::now();
            state.store.append_observation(&snapshot, &decision)?;
            state.store_write = written.elapsed();
            state.observations_since_prune += 1;
            if state.observations_since_prune >= OBSERVATIONS_PER_PRUNE {
                state.observations_since_prune = 0;
                state.store.prune_observations(
                    snapshot.observed_at_unix_ms,
                    crate::state::OBSERVATION_RETENTION_MS,
                    crate::state::OBSERVATION_ROWS,
                )?;
                super::history::prune_attempts(&state.store, &config.state_dir)?;
            }
            rekey_journal_cache(&mut state.slots, before, state.store.change_key()?);
            mark = state.cost.charge("store", mark);
            state.heartbeat(&snapshot, &allocations, &decision).await?;
            state.cost.charge("heartbeat", mark);
            state.cost.iteration();
            if let Some(mut report) = state.cost.take_report(Instant::now()) {
                // The journal's lock waits since the agent started, for the burst stop's record.
                let (waits, longest) = state.store.lock_waits();
                report["journal_waits"] = waits.into();
                report["journal_longest_wait_ms"] = (longest.as_millis() as u64).into();
                emit(&report)?;
            }
            state.stop_due(&records)
        }
        .await;
        let journal_held = matches!(&iteration, Err(error) if journal_busy(error));
        let name_held = held_stretch.starts(journal_held);
        if pin_stretch.starts(state.pins.found_busy()) {
            emit(&serde_json::json!({
                "event": "journal_busy",
                "code": "journal_busy",
                "stage": "pin",
                "detail": "an attempt pin found the journal busy; its report waits for the next interval",
            }))?;
        }
        match iteration {
            Ok(true) => break,
            Ok(false) => {}
            // A journal still locked after its wait is transient: the interval ends there,
            // what it sent stands, every step holds, and the next interval decides again
            // (FR-077).
            Err(error) if journal_held => {
                if name_held {
                    emit(&serde_json::json!({
                        "event": "journal_busy",
                        "code": "journal_busy",
                        "stage": "interval",
                        "detail": format!("{error:#}"),
                    }))?;
                }
            }
            // Running out of descriptors never ends the loop: the interval's readings are
            // unknown, so nothing new is decided and steps hold; the next one tries again
            // (FR-077).
            Err(error) if descriptor_exhausted(&error) => {
                emit(&serde_json::json!({
                    "event": "descriptors_exhausted",
                    "code": "descriptor_limit",
                    "detail": format!("{error:#}"),
                }))?;
            }
            // A full disk or quota is transient too: the interval is skipped.
            Err(error) if storage_exhausted(&error) => {
                emit(&serde_json::json!({
                    "event": "storage_exhausted",
                    "detail": format!("{error:#}"),
                }))?;
            }
            Err(error) => return Err(error),
        }
        tokio::select! {_ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_observation))=>{}, _=&mut stop_signal,if state.stopping.is_none()=>{state.stopping=Some(Instant::now()); state.leave_supervisors=true;}, _=reload_signal(&mut hangup)=>{state.reload_config()?;}}
    }
    state.shutdown().await?;
    drop(lock);
    Ok(())
}

/// The agent loop's own CPU by stage, in this thread's CPU time, reported once a period so
/// CedeGrid's cost can be told apart by stage (SC-014).
pub(super) struct LoopCost {
    stages: BTreeMap<&'static str, u64>,
    iterations: u64,
    since: Instant,
    period: Duration,
}

impl LoopCost {
    pub(super) fn new(now: Instant, period: Duration) -> Self {
        Self {
            stages: BTreeMap::new(),
            iterations: 0,
            since: now,
            period,
        }
    }

    /// This thread's CPU time; zero where the clock is unavailable.
    fn thread_ns() -> u64 {
        #[cfg(unix)]
        {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: valid output pointer.
            if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0 {
                return u64::try_from(time.tv_sec)
                    .unwrap_or(0)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::try_from(time.tv_nsec).unwrap_or(0));
            }
        }
        0
    }

    /// Charges the thread CPU used since `mark` to `stage` and returns the new mark.
    fn charge(&mut self, stage: &'static str, mark: u64) -> u64 {
        let now = Self::thread_ns();
        self.record(stage, now.saturating_sub(mark));
        now
    }

    pub(super) fn record(&mut self, stage: &'static str, ns: u64) {
        let total = self.stages.entry(stage).or_default();
        *total = total.saturating_add(ns);
    }

    pub(super) fn iteration(&mut self) {
        self.iterations += 1;
    }

    /// The report once the period has passed, after which counting starts again.
    pub(super) fn take_report(&mut self, now: Instant) -> Option<serde_json::Value> {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < self.period {
            return None;
        }
        let stages: BTreeMap<&str, u64> = self
            .stages
            .iter()
            .map(|(stage, ns)| (*stage, ns / 1000))
            .collect();
        let report = serde_json::json!({
            "event": "loop_cost",
            "period_ms": elapsed.as_millis() as u64,
            "iterations": self.iterations,
            "thread_cpu_us": stages,
        });
        *self = Self::new(now, self.period);
        Some(report)
    }
}

#[cfg(unix)]
type ReloadSignal = Option<tokio::signal::unix::Signal>;
#[cfg(not(unix))]
type ReloadSignal = Option<()>;

fn reload_signal_stream() -> ReloadSignal {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Resolves at each SIGHUP; never without one.
async fn reload_signal(signal: &mut ReloadSignal) {
    #[cfg(unix)]
    if let Some(signal) = signal {
        signal.recv().await;
        return;
    }
    let _ = signal;
    std::future::pending::<()>().await
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

fn collect_released_spool(
    config: &Config,
    store: &StateStore,
    history: &super::history::HistoryScope,
    records: &[ExecutionRecord],
    publishing: &BTreeSet<(String, bool)>,
) -> Result<()> {
    for record in records
        .iter()
        .filter(|r| r.phase == ExecutionPhase::Released)
    {
        if history.upgrade_held(store, &record.assignment_id)?
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

/// The capacity a launch reserves against: the schedulable budget, with CPU raised to the
/// unprotected budget for a grant that passed its check (004 R12). Without this the coordinator
/// places a granted task the agent then cannot reserve.
pub(crate) fn launch_capacity(
    mut budget: Resources,
    unprotected: Option<&Resources>,
    granted: bool,
) -> Resources {
    if granted && let Some(unprotected) = unprotected {
        budget.cpu_millicores = budget.cpu_millicores.max(unprotected.cpu_millicores);
    }
    budget
}

#[cfg(test)]
mod launch_capacity_tests {
    use super::launch_capacity;
    use crate::model::Resources;

    #[test]
    fn a_granted_launch_reserves_against_the_unprotected_budget_only() {
        let cpu = |millicores| Resources {
            cpu_millicores: millicores,
            ram_mib: 4096,
            ..Resources::default()
        };
        assert_eq!(launch_capacity(cpu(1000), None, true), cpu(1000));
        assert_eq!(
            launch_capacity(cpu(1000), Some(&cpu(3900)), false),
            cpu(1000)
        );
        assert_eq!(
            launch_capacity(cpu(1000), Some(&cpu(3900)), true),
            cpu(3900)
        );
        // The unprotected budget never lowers what a launch may reserve.
        assert_eq!(launch_capacity(cpu(1000), Some(&cpu(500)), true), cpu(1000));
    }
}
