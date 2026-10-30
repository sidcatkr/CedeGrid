//! Deterministic, effect-free admission and yielding recommendations.
//!
//! For admission, running/draining allocations are charged max(reservation,
//! observed), dimension by dimension, except running opportunistic CPU at its p95
//! usage over the admission window, and pending reservations in full. Yielding
//! judges running work on what it holds: CPU and RAM usage, and GPU memory usage
//! for work that takes a hint. GPU utilization is auxiliary telemetry and is
//! deliberately never a predicate for contention.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::contention::{
    Admission, ComputeFloor, ContentionMode, LadderInput, LadderTiming, Measurement, ModeInput,
    Principal, ResourceKey, ResourceKind, ResourceThresholds, StepState, VictimFacts, YieldCause,
    YieldStep, guaranteed_may_drain, own_allowance, plan_step, victim_order,
};
use crate::kernel::{CpuTopology, CpuUsageStatus, KernelSnapshot};
use crate::model::*;

mod admission;
mod ladder;
mod measure;

/// Whether a device's basic observation can be trusted this interval: inventory,
/// memory use and process list, and no stale or insufficient observation.
fn gpu_observable(observed_at_unix_ms: u64, gpu: &GpuSnapshot) -> bool {
    gpu.total_memory_mib > 0
        && gpu
            .used_memory_mib
            .is_some_and(|used| used <= gpu.total_memory_mib)
        && gpu.external_process_ids.is_some()
        && !gpu.observation.as_ref().is_some_and(|o| {
            o.observed_at_unix_ms != observed_at_unix_ms
                || o.capability == GpuExecutionCapability::InsufficientObservability
        })
}

/// The launch recheck on a fresh observation: each requested device must admit
/// own GPU work by the rule the policy applies (FR-026, FR-027, FR-064), and the
/// request must leave the device's floor free.
pub fn validate_gpu_launch_contract(
    config: &Config,
    snapshot: &Snapshot,
    resources: &Resources,
    _class: crate::execution_model::AllocationClass,
) -> Result<()> {
    if resources.gpu_memory_mib.is_empty() {
        return Ok(());
    }
    ensure!(
        snapshot.gpu_inventory == CapabilityStatus::Available,
        "GPU launch requires available device inventory"
    );
    let mode = config.contention.mode(ResourceKind::GpuCompute);
    for (uuid, requested) in &resources.gpu_memory_mib {
        let gpu = snapshot
            .gpus
            .iter()
            .find(|g| &g.uuid == uuid)
            .with_context(|| format!("GPU {uuid} is absent from the current inventory"))?;
        let admission = crate::contention::gpu_admission(
            mode,
            gpu.external_compute,
            gpu.external_process_ids.as_ref().map(Vec::len),
            gpu_observable(snapshot.observed_at_unix_ms, gpu),
            config.contention.never_share_occupied_gpus,
        );
        if let Admission::Held { reason, .. } | Admission::Blocked { reason, .. } = admission {
            anyhow::bail!(
                "GPU {uuid} is not admitted by mode {mode:?} at the launch recheck: {reason}"
            );
        }
        let floor = config.vram_floor_mib(gpu.total_memory_mib);
        let free = gpu
            .total_memory_mib
            .saturating_sub(gpu.used_memory_mib.unwrap_or(gpu.total_memory_mib));
        ensure!(
            *requested <= free.saturating_sub(floor),
            "GPU {uuid} request of {requested} MiB would leave less than its {floor} MiB floor free"
        );
    }
    Ok(())
}

/// Whether a new request is admitted now: every resource it asks for must be
/// admitted, and own charged usage plus the request must stay within own target,
/// the aggregate from the node mode and cap. Returns the catalog code of the
/// first refusal. Coordinator placement charges the same headroom.
pub fn request_admission(decision: &Decision, request: &Resources) -> Result<(), &'static str> {
    let catalogued = |code: &str| {
        crate::contention::REASON_CODES
            .iter()
            .find(|(known, _)| *known == code)
            .map_or("admission_over_own_target", |(known, _)| *known)
    };
    let admitted = |key: &ResourceKey| -> Result<(), &'static str> {
        match decision
            .resources
            .get(key)
            .and_then(|status| status.admission.as_ref())
        {
            Some(admission) => admission
                .code()
                .map_or(Ok(()), |code| Err(catalogued(code))),
            None => Err("admission_measurement_unknown"),
        }
    };
    let headroom = &decision.admission_headroom;
    if request.cpu_millicores > 0 {
        admitted(&ResourceKey::Cpu)?;
        if request.cpu_millicores > headroom.cpu_millicores {
            return Err("admission_over_own_target");
        }
    }
    if request.ram_mib > 0 {
        admitted(&ResourceKey::Ram)?;
        if request.ram_mib > headroom.ram_mib {
            return Err("admission_over_own_target");
        }
    }
    for (uuid, mib) in &request.gpu_memory_mib {
        admitted(&ResourceKey::GpuMemory(uuid.clone()))?;
        admitted(&ResourceKey::GpuCompute(uuid.clone()))?;
        if *mib > headroom.gpu_memory_mib.get(uuid).copied().unwrap_or(0) {
            return Err("admission_over_own_target");
        }
    }
    Ok(())
}

/// Coordinator placement uses this admission budget, while policy accounting
/// retains the original managed budget and charges every live/reserved allocation.
pub fn schedulable_budget(decision: &Decision) -> Resources {
    let mut budget = decision.managed_budget.clone();
    budget.cpu_millicores = (i128::from(budget.cpu_millicores)
        + i128::from(decision.cpu_admission_credit_millicores))
    .clamp(0, i128::from(u64::MAX)) as u64;
    for (uuid, value) in &mut budget.gpu_memory_mib {
        if !decision
            .admission_headroom
            .gpu_memory_mib
            .contains_key(uuid)
        {
            *value = 0;
        }
    }
    budget
}

#[derive(Debug, Clone, Default)]
struct PreviousGpu {
    pids: BTreeSet<u32>,
    /// External memory that growth is measured against. Changes below the noise
    /// tolerance accumulate against it instead of moving it up.
    external_memory: Option<u64>,
    sample_id: Option<u64>,
    stable_since: Option<u64>,
    /// Consecutive intervals with a measured external GPU compute amount.
    measured_streak: u32,
}

#[derive(Debug, Default)]
pub struct PolicyEngine {
    previous_gpus: BTreeMap<String, PreviousGpu>,
    stable_since: Option<u64>,
    /// Since when CPU and RAM have each been safe, for their own cooldowns (FR-034).
    cpu_stable_since: Option<u64>,
    ram_stable_since: Option<u64>,
    /// Whether the last reset of CPU's and RAM's stability had no cause but own
    /// allocations without a first measurement (FR-028).
    cpu_reset_by_new: bool,
    ram_reset_by_new: bool,
    /// The last CPU and RAM target shares computed, carried through held intervals,
    /// with the external usage they were computed beside, to tell external growth.
    cpu_target: Option<(u64, u64)>,
    ram_target: Option<(u64, u64)>,
    /// Since when own CPU usage has been over its target while the budget did not fall,
    /// counted once every held cap is applied (FR-011).
    cpu_over_since: Option<u64>,
    /// Consecutive intervals in which every external CPU principal, the unattributed
    /// remainder included and background left out, stayed below `principal_min_percent`.
    cpu_quiet_intervals: u32,
    /// The last known CPU view, carried through unknown and held intervals, for the hold
    /// while CPU is unknown (FR-022).
    cpu_view: Option<CpuView>,
    /// The last known RAM fair share, carried through unknown and held intervals and in
    /// force while RAM is held (FR-023).
    ram_fair_share: Option<u64>,
    /// Memory resources whose breach episode has had its save-progress decision (FR-075).
    save_progress_decided: BTreeSet<ResourceKey>,
    /// The allocations marked as preferred OOM victims, held while RAM is unknown.
    oom_preferred: BTreeSet<String>,
    last_now: Option<u64>,
    node_id: Option<String>,
    /// Collector-relative sample marker and the policy time it first arrived.
    /// The two monotonic clocks are never directly subtracted from one another.
    last_kernel_cpu_sample: Option<(u64, u64)>,
    /// Last observation per allocation. Its presence also marks that the
    /// allocation's first observation, a baseline, has already happened.
    last_observed: BTreeMap<String, Resources>,
    /// The yield ladder: the step applied per allocation and resource.
    ladder: BTreeMap<(String, ResourceKey), StepState>,
    /// When a resource's measurement became unknown or held (FR-008 window).
    unknown_since: BTreeMap<ResourceKey, u64>,
    /// When an allocation's usage became unknown (R10 window).
    allocation_unknown_since: BTreeMap<String, u64>,
    /// When a floor or envelope breach began, for the Guaranteed hint deadline.
    floor_breach_since: BTreeMap<ResourceKey, u64>,
    /// Users on their way into or out of the principals, per resource (FR-059).
    principal_counts: BTreeMap<(ResourceKey, u32), PrincipalCount>,
    /// External memory not listed per process, over the peak window, as a
    /// decreasing deque (FR-060).
    external_peaks: BTreeMap<ResourceKey, std::collections::VecDeque<(u64, u64)>>,
    /// Each listed external process's memory over the window, by resource, PID and
    /// start time; an entry goes when the process is no longer listed.
    process_peaks: BTreeMap<(ResourceKey, u32, u64), std::collections::VecDeque<(u64, u64)>>,
    /// Starts of recent reclaim episodes per memory resource, and the resources
    /// with one in progress (FR-061).
    reclaim_episodes: BTreeMap<ResourceKey, Vec<u64>>,
    reclaiming: BTreeSet<ResourceKey>,
    /// Demands found satisfied by probing, per compute resource (FR-021).
    satisfied: BTreeMap<ResourceKey, Vec<(Option<u32>, u64)>>,
    /// Probes of small external demand in progress, per compute resource.
    probes: BTreeMap<ResourceKey, crate::contention::DemandProbe>,
    /// When each principal with a satisfied record was last seen, per resource.
    principal_seen: BTreeMap<ResourceKey, BTreeMap<Option<u32>, u64>>,
    /// Each allocation's published allowance per resource, and when it last rose.
    allowance_published: BTreeMap<(String, ResourceKey), (u64, u64)>,
    /// Running opportunistic CPU usage over the admission window, per allocation,
    /// with the time of its first sample.
    cpu_usage: BTreeMap<String, (u64, std::collections::VecDeque<(u64, u64)>)>,
}

/// A known interval's CPU view: the mode input, the room beside it (capacity less the
/// reserve, within the ceiling) and CedeGrid's own CPU, which comes out of own targets.
#[derive(Debug, Clone)]
struct CpuView {
    view: ModeInput,
    room: u64,
    cedegrid: u64,
}

/// One user's way into or out of the principals on one resource (FR-059):
/// consecutive observations at or above the threshold, and since when it has been
/// below it.
#[derive(Debug, Clone, Copy, Default)]
struct PrincipalCount {
    admitted: bool,
    above: u32,
    below_since_ms: Option<u64>,
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-establish a step a supervisor already applied, after an agent restart
    /// (research R2). The step then holds, escalates or reverses as usual.
    pub fn seed(
        &mut self,
        allocation_id: &str,
        key: ResourceKey,
        request: crate::contention::StepRequest,
        now_monotonic_ms: u64,
    ) {
        self.ladder.insert(
            (allocation_id.to_owned(), key),
            StepState {
                step: request.step,
                entered_at_ms: now_monotonic_ms,
                cause: request.cause,
                target: request.target,
                trigger_external: 0,
                calm_since_ms: None,
                contended_since_ms: None,
                after_pause: false,
                to_free: None,
                pending: false,
                recomputed: false,
            },
        );
    }

    pub fn evaluate(
        &mut self,
        config: &Config,
        snapshot: &Snapshot,
        input: &PolicyInput,
        now_monotonic_ms: u64,
    ) -> Result<Decision> {
        config.validate()?;
        validate(snapshot, input, &config.node_id)?;
        ensure!(
            self.last_now.is_none_or(|last| now_monotonic_ms >= last),
            "policy monotonic clock moved backwards"
        );
        ensure!(
            self.node_id
                .as_ref()
                .is_none_or(|id| id == &snapshot.node_id),
            "a policy engine instance cannot switch node identity"
        );
        // The phases run in this order; each reads what the earlier ones built.
        let mut interval = Interval::new(self, config, snapshot, input, now_monotonic_ms);
        interval.charge()?;
        interval.resolve_modes();
        interval.node_reasons();
        interval.cpu_scope();
        interval.cpu_external();
        interval.measure_ram();
        interval.unknown_budgets();
        interval.cpu_target();
        interval.ram_target();
        interval.envelope_and_floors();
        interval.observe_gpus();
        interval.safety();
        interval.plan_ladder();
        interval.oom_order();
        interval.drains();
        interval.reclaim_episodes();
        interval.cooldowns();
        interval.memory_peaks();
        interval.admission_headroom();
        interval.held_back();
        interval.step_codes();
        interval.admissions();
        interval.publish_allowances();
        let (decision, next) = interval.finish();
        *self = next;
        Ok(decision)
    }
}

/// One interval's evaluation: its inputs, the engine as the last interval left
/// it, the state each phase builds, and the engine state it hands over.
struct Interval<'a> {
    config: &'a Config,
    snapshot: &'a Snapshot,
    input: &'a PolicyInput,
    now: u64,
    prev: &'a PolicyEngine,
    next: PolicyEngine,
    reasons: Vec<ReasonCode>,
    statuses: BTreeMap<ResourceKey, ResourceStatus>,
    // Charges and observed usage of own work.
    charged: Resources,
    managed_observed: Resources,
    charges: BTreeMap<String, Resources>,
    /// Charges of running allocations whose usage is unknown this interval. Host
    /// usage includes them, so they are own work, never external demand (R10).
    unknown_charge: Resources,
    /// The part of `unknown_charge` from allocations never measured yet.
    unknown_new: Resources,
    unknown_allocations: BTreeSet<String>,
    first_observed_gpus: BTreeSet<String>,
    cpu_credit: i128,
    /// What held-back allocations are charged above what they hold now: it closes
    /// admission but is not own usage beyond the budget.
    held_back_cpu: u64,
    allocation_modes: BTreeMap<String, BTreeMap<ResourceKind, ContentionMode>>,
    // Measurement and budgets.
    budget: Resources,
    unsafe_now: bool,
    cpu_unknown: bool,
    ram_unknown: bool,
    cpu_held: bool,
    ram_held: bool,
    /// CPU is held because own usage read above host busy.
    cpu_held_inconsistent: bool,
    /// The per-interval external estimate's own bound: host busy upper less lower, plus own
    /// upper less lower (FR-073).
    cpu_bound: u64,
    /// This interval's external CPU estimate, beside the window's that sets the budget.
    cpu_interval_external: Option<u64>,
    /// Own CPU usage is over its target without the budget falling, within the dwell: held
    /// steps keep their targets and none is entered (FR-011).
    cpu_own_hold: bool,
    /// CedeGrid's own CPU this interval: own work's side, out of external usage (FR-069).
    cedegrid_cpu: u64,
    /// CPU contention has fully ended: Yield First and Neutral return CPU in one move,
    /// without the mode's scale-up interval (FR-034).
    cpu_contention_ended: bool,
    /// CPU is unknown in Performance or Aggressive with no view known yet: steps hold and
    /// none is taken (FR-022).
    cpu_unviewed: bool,
    /// CPU and RAM admission, when closed, is closed only by own new allocations.
    cpu_new_only: bool,
    ram_new_only: bool,
    cpu_capacity: u64,
    cpu_busy: Option<u64>,
    cpu_ceiling: u64,
    cpu_reserve: u64,
    ram_reserve: u64,
    psi_only: bool,
    /// External CPU pressure is above its threshold this interval.
    psi_saturated: bool,
    /// Each mode's target per resource, which bounds the jobs in that mode.
    mode_targets: BTreeMap<ResourceKey, BTreeMap<ContentionMode, u64>>,
    /// What own work may hold before the mode's target applies: for memory, what
    /// keeps the floor free.
    floor_available: BTreeMap<ResourceKey, u64>,
    critical_keys: BTreeSet<ResourceKey>,
    /// A floor breach is observed free memory below the floor, as free and
    /// floor; charges beyond the budget only block admission (FR-023).
    floor_breaches: BTreeMap<ResourceKey, (u64, u64)>,
    /// Free memory above each floor, which a floor-breach step reverses with.
    floor_room: BTreeMap<ResourceKey, u64>,
    // GPU devices.
    blocked_gpus: BTreeSet<String>,
    /// Each device's GPU compute allowance and whether it is contended.
    gpu_compute_budget: BTreeMap<String, GpuComputeBudget>,
    compute_paths: BTreeMap<String, ComputePath>,
    /// The devices whose external activity is unknown.
    gpu_activity_unknown: BTreeSet<String>,
    /// The allocated devices whose memory telemetry is unknown: unobservable, missing
    /// from the inventory, or with no inventory at all.
    gpu_memory_unknown: BTreeSet<String>,
    /// Why GPU admission is blocked on a device, for its admission state.
    gpu_block_reasons: BTreeMap<String, String>,
    cooled_down: bool,
    // The yield ladder.
    actions: Vec<YieldAction>,
    /// The largest amount own work holds above its target, per resource.
    over_target: BTreeMap<ResourceKey, u64>,
    /// The deadline each hint carries to its worker, as the ladder enforces it.
    hint_deadlines: BTreeMap<(String, ResourceKey), u64>,
    breached: BTreeSet<ResourceKey>,
    would_drain: Vec<String>,
    // Cooldowns and admission.
    moving_down: BTreeSet<ResourceKey>,
    cpu_allowed: bool,
    ram_allowed: bool,
    cpu_ram_expansion_allowed: bool,
    expansion_allowed: bool,
    peak_excess: BTreeMap<ResourceKey, u64>,
    headroom: Resources,
    allowances: BTreeMap<String, ResourceAmounts>,
    /// Own work's memory fair share beside each memory resource's principals, where its
    /// usage is known (FR-023).
    memory_fair_share: BTreeMap<ResourceKey, u64>,
    /// In-share allocations to save their progress this interval (FR-075).
    save_progress: BTreeSet<String>,
}

impl<'a> Interval<'a> {
    fn new(
        prev: &'a PolicyEngine,
        config: &'a Config,
        snapshot: &'a Snapshot,
        input: &'a PolicyInput,
        now: u64,
    ) -> Self {
        let mut previous_gpus = prev.previous_gpus.clone();
        for value in previous_gpus.values_mut() {
            value.stable_since = None;
        }
        Self {
            config,
            snapshot,
            input,
            now,
            prev,
            next: PolicyEngine {
                previous_gpus,
                last_kernel_cpu_sample: prev.last_kernel_cpu_sample,
                ..PolicyEngine::default()
            },
            reasons: Vec::new(),
            statuses: BTreeMap::new(),
            charged: Resources::default(),
            managed_observed: Resources::default(),
            charges: BTreeMap::new(),
            unknown_charge: Resources::default(),
            unknown_new: Resources::default(),
            unknown_allocations: BTreeSet::new(),
            first_observed_gpus: BTreeSet::new(),
            cpu_credit: 0,
            held_back_cpu: 0,
            allocation_modes: BTreeMap::new(),
            budget: Resources::default(),
            unsafe_now: false,
            cpu_unknown: false,
            ram_unknown: false,
            cpu_held: false,
            cpu_held_inconsistent: false,
            cpu_bound: 0,
            cpu_interval_external: None,
            cpu_own_hold: false,
            cedegrid_cpu: 0,
            cpu_contention_ended: false,
            cpu_unviewed: false,
            cpu_new_only: false,
            ram_new_only: false,
            ram_held: false,
            cpu_capacity: 0,
            cpu_busy: None,
            cpu_ceiling: 0,
            cpu_reserve: 0,
            ram_reserve: 0,
            psi_only: false,
            psi_saturated: false,
            mode_targets: BTreeMap::new(),
            floor_available: BTreeMap::new(),
            critical_keys: BTreeSet::new(),
            floor_breaches: BTreeMap::new(),
            floor_room: BTreeMap::new(),
            blocked_gpus: BTreeSet::new(),
            gpu_compute_budget: BTreeMap::new(),
            compute_paths: BTreeMap::new(),
            gpu_activity_unknown: BTreeSet::new(),
            gpu_memory_unknown: BTreeSet::new(),
            gpu_block_reasons: BTreeMap::new(),
            cooled_down: false,
            actions: Vec::new(),
            over_target: BTreeMap::new(),
            hint_deadlines: BTreeMap::new(),
            breached: BTreeSet::new(),
            would_drain: Vec::new(),
            moving_down: BTreeSet::new(),
            cpu_allowed: false,
            ram_allowed: false,
            cpu_ram_expansion_allowed: false,
            expansion_allowed: false,
            peak_excess: BTreeMap::new(),
            headroom: Resources::default(),
            allowances: BTreeMap::new(),
            memory_fair_share: BTreeMap::new(),
            save_progress: BTreeSet::new(),
        }
    }

    /// Hand over the engine state and assemble the decision.
    fn finish(mut self) -> (Decision, PolicyEngine) {
        let (config, snapshot) = (self.config, self.snapshot);
        self.next.last_now = Some(self.now);
        self.next.node_id = Some(snapshot.node_id.clone());
        for key in [ResourceKey::Cpu, ResourceKey::Ram] {
            self.statuses.entry(key).or_default();
        }
        for (key, status) in &mut self.statuses {
            status.mode = Some(config.contention.mode(key.kind()));
            status.effective_mode = Some(governing(config, key.kind()));
            status.measurement.get_or_insert(Measurement::Known);
        }
        let decision = Decision {
            schema_version: SCHEMA_VERSION,
            node_id: snapshot.node_id.clone(),
            observe_only: true,
            managed_budget: self.budget,
            admission_headroom: self.headroom,
            expansion_allowed: self.expansion_allowed,
            cpu_ram_expansion_allowed: self.cpu_ram_expansion_allowed,
            would_drain: self.would_drain,
            reasons: self
                .reasons
                .iter()
                .map(|reason| reason.text.clone())
                .collect(),
            reason_codes: self.reasons,
            resources: self.statuses,
            actions: self.actions,
            exceptions: config.caution_options_on(snapshot),
            allocation_modes: self.allocation_modes,
            max_mode: Some(config.contention.max_mode()),
            allowances: self.allowances,
            oom_preferred: self.next.oom_preferred.clone(),
            save_progress: self.save_progress,
            ram_fair_share_mib: self.memory_fair_share.get(&ResourceKey::Ram).copied(),
            cpu_admission_credit_millicores: self
                .cpu_credit
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                as i64,
        };
        // Every reported code is in the catalog, so tests fail on a new one (T113).
        debug_assert!(
            decision
                .codes()
                .iter()
                .all(|code| crate::contention::reason_text(code).is_some()),
            "uncatalogued reason codes: {:?}",
            decision
                .codes()
                .into_iter()
                .filter(|code| crate::contention::reason_text(code).is_none())
                .collect::<Vec<_>>()
        );
        (decision, self.next)
    }
}

/// The mode that sets own work's aggregate on a resource: the node mode within
/// the node cap. Per-job modes only order who yields inside it and bound each job.
fn governing(config: &Config, kind: ResourceKind) -> ContentionMode {
    config
        .contention
        .mode(kind)
        .min(config.contention.max_mode())
}

/// The amount of one resource in a set of resources.
fn amount_of(key: &ResourceKey, resources: &Resources) -> u64 {
    match key {
        ResourceKey::Cpu => resources.cpu_millicores,
        ResourceKey::Ram => resources.ram_mib,
        ResourceKey::GpuCompute(uuid) | ResourceKey::GpuMemory(uuid) => {
            resources.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
        }
    }
}

fn is_guaranteed(allocation: &Allocation) -> bool {
    allocation.class == crate::execution_model::AllocationClass::Guaranteed
}

/// What each recent reclaim episode doubles, up to the maximum (FR-061).
fn reclaim_backoff_ms(thresholds: &ResourceThresholds, episodes: u32) -> u64 {
    if episodes == 0 {
        0
    } else {
        thresholds
            .reclaim_backoff_ms
            .saturating_mul(1u64.checked_shl(episodes - 1).unwrap_or(u64::MAX))
            .min(thresholds.reclaim_backoff_max_ms)
    }
}

/// GPU compute is accounted in thousandths of one device's time.
const GPU_COMPUTE_CAPACITY: u64 = 1000;

/// One device's GPU compute for own work, in thousandths of device time.
struct GpuComputeBudget {
    /// What own work may hold on the device.
    share: u64,
    contended: bool,
    /// External compute: measured, the whole device for activity of unknown
    /// amount, or zero.
    external: u64,
    /// Own compute, when measured.
    own: Option<u64>,
    /// Each mode's share, which bounds the jobs in that mode; empty when no
    /// external work contends.
    targets: BTreeMap<ContentionMode, u64>,
}

fn scoped_cpu_valid(kernel: &KernelSnapshot, reasons: &mut Vec<ReasonCode>) -> bool {
    let cpu = &kernel.cpu;
    let Some(ids) = &cpu.effective_cpu_ids else {
        reasons.push(ReasonCode::new(
            "cpu_set_unknown",
            "effective CPU set is unavailable; scoped CPU admission blocked",
        ));
        return false;
    };
    let unique: BTreeSet<_> = ids.iter().copied().collect();
    let hardware = (ids.len() as u64).saturating_mul(1000);
    let membership_matches = cpu
        .allowed_cpu_ids
        .as_ref()
        .is_some_and(|allowed| ids.iter().all(|id| allowed.contains(id)))
        && cpu
            .effective_cgroup_cpu_ids
            .as_ref()
            .is_none_or(|effective| ids.iter().all(|id| effective.contains(id)));
    let valid = kernel.schema_version == 1
        && !ids.is_empty()
        && unique.len() == ids.len()
        && membership_matches
        && cpu.effective_cpu_capacity_millicores == Some(hardware)
        && cpu
            .visible_cpu_ceiling_millicores
            .is_some_and(|ceiling| ceiling <= hardware)
        && cpu.busy_millicores.is_some_and(|busy| busy <= hardware)
        && cpu.busy_status == CpuUsageStatus::Available
        && kernel.freshness_limit_ms > 0
        && cpu
            .busy_interval_ms
            .is_some_and(|interval| interval > 0 && interval <= kernel.freshness_limit_ms);
    if !valid {
        reasons.push(ReasonCode::new("cpu_scoped_telemetry_unknown", format!("scoped CPU telemetry missing, stale or inconsistent ({:?}); host-wide busy time cannot substitute for the effective CPU set", cpu.busy_status)));
    }
    valid
}

/// Charge actual effective SMT siblings, not the whole-host average. With an
/// asymmetric permitted set, reserving the largest core groups is conservative.
/// This is budget accounting; rootless operation does not enforce core placement.
fn scoped_physical_reserve(cpu: &CpuTopology, reserve: u32) -> Option<u64> {
    if reserve == 0 {
        return Some(0);
    }
    let ids: BTreeSet<_> = cpu.effective_cpu_ids.as_ref()?.iter().copied().collect();
    let mut seen = BTreeSet::new();
    let mut groups = BTreeMap::<(i32, i32), u64>::new();
    for core in &cpu.cores {
        if !ids.contains(&core.cpu_id) || !seen.insert(core.cpu_id) {
            return None;
        }
        let package = core.package_id.filter(|value| *value >= 0)?;
        let physical = core.core_id.filter(|value| *value >= 0)?;
        *groups.entry((package, physical)).or_default() += 1000;
    }
    if seen != ids || ids.is_empty() {
        return None;
    }
    let mut amounts: Vec<_> = groups.into_values().collect();
    amounts.sort_unstable_by(|left, right| right.cmp(left));
    Some(amounts.into_iter().take(reserve as usize).sum())
}

fn validate(snapshot: &Snapshot, input: &PolicyInput, node_id: &str) -> Result<()> {
    ensure!(
        supported_schema(snapshot.schema_version),
        "unsupported snapshot schema_version"
    );
    ensure!(
        snapshot.node_id == node_id,
        "snapshot node_id does not match configuration"
    );
    ensure!(
        snapshot.cpu_capacity_millicores > 0
            || (snapshot.physical_cores.is_none() && snapshot.cpu_busy_millicores.is_none()),
        "unknown CPU capacity requires unknown topology and busy telemetry"
    );
    ensure!(
        snapshot.total_ram_mib > 0 || snapshot.available_ram_mib.is_none(),
        "unknown RAM capacity requires unknown availability telemetry"
    );
    ensure!(
        snapshot
            .physical_cores
            .is_none_or(|n| n > 0 && u64::from(n) <= snapshot.cpu_capacity_millicores / 1000),
        "invalid physical CPU count"
    );
    ensure!(
        snapshot
            .cpu_busy_millicores
            .is_none_or(|busy| busy <= snapshot.cpu_capacity_millicores),
        "CPU busy exceeds capacity"
    );
    ensure!(
        snapshot
            .available_ram_mib
            .is_none_or(|available| available <= snapshot.total_ram_mib),
        "available RAM exceeds total"
    );
    ensure!(
        snapshot.gpu_inventory == CapabilityStatus::Available || snapshot.gpus.is_empty(),
        "GPU entries require an available inventory"
    );
    let mut gpu_ids = BTreeSet::new();
    for gpu in &snapshot.gpus {
        ensure!(
            !gpu.uuid.trim().is_empty() && gpu_ids.insert(&gpu.uuid),
            "empty or duplicate GPU UUID"
        );
        ensure!(
            gpu.total_memory_mib > 0
                || (gpu.used_memory_mib.is_none()
                    && gpu.observation.as_ref().is_some_and(
                        |o| o.capability == GpuExecutionCapability::InsufficientObservability
                    )),
            "GPU total memory must be positive unless unavailable with an explicit blocked capability"
        );
        ensure!(
            gpu.used_memory_mib
                .is_none_or(|used| used <= gpu.total_memory_mib),
            "GPU used memory exceeds total"
        );
        ensure!(
            gpu.utilization_percent.is_none_or(|percent| percent <= 100),
            "GPU utilization must be between 0 and 100"
        );
        if let Some(pids) = &gpu.external_process_ids {
            let unique: BTreeSet<_> = pids.iter().collect();
            ensure!(
                pids.iter().all(|pid| *pid > 0) && unique.len() == pids.len(),
                "external GPU PIDs must be positive and unique"
            );
        }
    }
    let mut allocation_ids = BTreeSet::new();
    for allocation in &input.allocations {
        ensure!(
            !allocation.id.trim().is_empty() && allocation_ids.insert(&allocation.id),
            "empty or duplicate allocation ID"
        );
        ensure!(
            allocation.phase != AllocationPhase::Pending || allocation.observed.is_none(),
            "pending allocations cannot have observed usage"
        );
        ensure!(
            allocation
                .requested
                .gpu_memory_mib
                .keys()
                .all(|id| !id.trim().is_empty()),
            "GPU resource IDs cannot be empty"
        );
        if let Some(observed) = &allocation.observed {
            ensure!(
                observed
                    .gpu_memory_mib
                    .keys()
                    .all(|id| !id.trim().is_empty()),
                "observed GPU IDs cannot be empty"
            );
        }
    }
    Ok(())
}

fn ceil_ratio(value: u64, numerator: u64, denominator: u64) -> u64 {
    (u128::from(value) * u128::from(numerator))
        .div_ceil(u128::from(denominator))
        .min(u128::from(u64::MAX)) as u64
}

fn step_name(step: YieldStep) -> &'static str {
    match step {
        YieldStep::None => "none",
        YieldStep::LowerShare => "lower_share",
        YieldStep::Cap => "cap",
        YieldStep::Pause => "pause",
        YieldStep::Hint => "hint",
        YieldStep::Drain => "drain",
        YieldStep::Terminate => "terminate",
    }
}

/// Say whether a small external demand is probed, a larger one watched, and
/// which demands are recorded as satisfied (FR-021).
fn note_demand(
    statuses: &mut BTreeMap<ResourceKey, ResourceStatus>,
    key: ResourceKey,
    satisfied: &[(Option<u32>, u64)],
    probe: Option<&crate::contention::DemandProbe>,
) {
    match probe {
        Some(probe) if probe.watch => note(
            statuses,
            key.clone(),
            "watching_larger_demand",
            "own work holds the fair share while a larger external demand is watched".into(),
        ),
        Some(probe) => note(
            statuses,
            key.clone(),
            "probing_demand",
            format!(
                "own work steps down to {} to learn whether a small external demand is satisfied",
                probe.target
            ),
        ),
        None => {}
    }
    if !satisfied.is_empty() {
        let users: Vec<String> = satisfied
            .iter()
            .map(|(uid, demand)| match uid {
                Some(uid) => format!("uid {uid} at {demand}"),
                None => format!("unattributed usage at {demand}"),
            })
            .collect();
        note(
            statuses,
            key,
            "satisfied_demand",
            format!("satisfied demand recorded: {}", users.join(", ")),
        );
    }
}

fn note(
    statuses: &mut BTreeMap<ResourceKey, ResourceStatus>,
    key: ResourceKey,
    code: &str,
    text: String,
) {
    statuses
        .entry(key)
        .or_default()
        .reasons
        .push(ReasonCode::new(code, text));
}

fn first_charged_resource(charge: &Resources) -> ResourceKey {
    if charge.cpu_millicores > 0 {
        ResourceKey::Cpu
    } else if charge.ram_mib > 0 {
        ResourceKey::Ram
    } else if let Some(gpu) = charge.gpu_memory_mib.keys().next() {
        ResourceKey::GpuMemory(gpu.clone())
    } else {
        ResourceKey::Cpu
    }
}

impl PrincipalCount {
    /// Count one observation: join after `window` observations at or above the
    /// threshold, leave after `leave_ms` below it.
    fn observe(&mut self, usage: u64, floor: u64, window: u32, now_ms: u64, leave_ms: u64) {
        if usage >= floor {
            self.above = self.above.saturating_add(1);
            self.below_since_ms = None;
        } else {
            self.above = 0;
            self.below_since_ms.get_or_insert(now_ms);
        }
        if !self.admitted && self.above >= window {
            self.admitted = true;
        } else if self.admitted
            && self
                .below_since_ms
                .is_some_and(|since| now_ms.saturating_sub(since) >= leave_ms)
        {
            *self = Self::default();
        }
    }
}

/// The 95th percentile by nearest rank; 0 for no values.
fn p95(values: impl Iterator<Item = u64>) -> u64 {
    let mut values: Vec<u64> = values.collect();
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = (values.len() * 95).div_ceil(100);
    values[rank.saturating_sub(1)]
}

/// Add a sample to a decreasing deque of the last `window_ms` and return the peak.
fn window_peak(
    samples: &mut std::collections::VecDeque<(u64, u64)>,
    now_ms: u64,
    value: u64,
    window_ms: u64,
) -> u64 {
    while samples
        .front()
        .is_some_and(|(at, _)| now_ms.saturating_sub(*at) >= window_ms)
    {
        samples.pop_front();
    }
    while samples.back().is_some_and(|(_, usage)| *usage <= value) {
        samples.pop_back();
    }
    samples.push_back((now_ms, value));
    samples.front().map_or(value, |(_, usage)| *usage)
}
