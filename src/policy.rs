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
    Admission, ContentionMode, LadderInput, LadderTiming, Measurement, ModeInput, Principal,
    ResourceKey, ResourceKind, ResourceThresholds, StepState, VictimFacts, YieldCause, YieldStep,
    guaranteed_may_drain, own_allowance, plan_step, victim_order,
};
use crate::kernel::{CpuTopology, CpuUsageStatus, KernelSnapshot};
use crate::model::*;

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

        let mut charged = Resources::default();
        let mut managed_observed = Resources::default();
        let mut charges = BTreeMap::new();
        // Charges of running allocations whose usage is unknown this interval. Host
        // usage includes them, so they are own work, never external demand (R10).
        let mut unknown_charge = Resources::default();
        let mut unknown_allocations = BTreeSet::new();
        let mut first_observed_gpus = BTreeSet::new();
        let mut last_observed = BTreeMap::new();
        let mut cpu_usage = BTreeMap::new();
        let mut cpu_credit = 0i128;
        let cpu_window_ms = config
            .contention
            .thresholds(ResourceKind::Cpu)
            .admission_usage_window_s
            .saturating_mul(1000);
        let mut reasons = Vec::new();
        let mut statuses: BTreeMap<ResourceKey, ResourceStatus> = BTreeMap::new();
        for allocation in &input.allocations {
            let mut charge = allocation.requested.clone();
            if allocation.phase != AllocationPhase::Pending {
                if let Some(observed) = &allocation.observed {
                    managed_observed.add_checked(observed)?;
                    charge = Resources::charge(&charge, Some(observed));
                    // CPU is compressible: running opportunistic work is admitted
                    // against its recent usage, not its request, once a full window
                    // shows it past its startup phase.
                    if allocation.class == crate::execution_model::AllocationClass::Opportunistic
                        && allocation.phase == AllocationPhase::Running
                    {
                        let (since, mut samples) = self
                            .cpu_usage
                            .get(&allocation.id)
                            .cloned()
                            .unwrap_or((now_monotonic_ms, Default::default()));
                        samples
                            .retain(|(at, _)| now_monotonic_ms.saturating_sub(*at) < cpu_window_ms);
                        samples.push_back((now_monotonic_ms, observed.cpu_millicores));
                        if now_monotonic_ms.saturating_sub(since) >= cpu_window_ms {
                            let usage = p95(samples.iter().map(|(_, usage)| *usage));
                            cpu_credit += i128::from(charge.cpu_millicores) - i128::from(usage);
                            charge.cpu_millicores = usage;
                        }
                        cpu_usage.insert(allocation.id.clone(), (since, samples));
                    }
                    if !self.last_observed.contains_key(&allocation.id) {
                        first_observed_gpus.extend(charge.gpu_memory_mib.keys().cloned());
                    }
                    last_observed.insert(allocation.id.clone(), observed.clone());
                } else {
                    let last = self.last_observed.get(&allocation.id);
                    charge = Resources::charge(&charge, last);
                    if let Some(last) = last {
                        last_observed.insert(allocation.id.clone(), last.clone());
                    }
                    unknown_charge.add_checked(&charge)?;
                    unknown_allocations.insert(allocation.id.clone());
                    reasons.push(ReasonCode::new("allocation_unknown_hold", format!(
                        "allocation {} has unknown managed usage; charged at max(request, last known), its step holds and affected admission is blocked",
                        allocation.id
                    )));
                }
            }
            charged.add_checked(&charge)?;
            charges.insert(allocation.id.clone(), charge);
        }
        // Each allocation runs at min(requested, node cap).
        let allocation_modes: BTreeMap<
            String,
            BTreeMap<ResourceKind, crate::contention::ContentionMode>,
        > = input
            .allocations
            .iter()
            .map(|a| {
                let modes = ResourceKind::ALL
                    .into_iter()
                    .map(|kind| {
                        let mode = crate::contention::effective_mode(
                            config.contention.mode(kind),
                            a.modes.get(&kind).copied(),
                            config.contention.max_mode(),
                        );
                        (kind, mode)
                    })
                    .collect();
                (a.id.clone(), modes)
            })
            .collect();
        // A requested mode above the node cap runs at the cap; say so (FR-030).
        let cap = config.contention.max_mode();
        for allocation in &input.allocations {
            for (kind, requested) in &allocation.modes {
                if *requested <= cap {
                    continue;
                }
                let keys: Vec<ResourceKey> = match kind {
                    ResourceKind::Cpu => vec![ResourceKey::Cpu],
                    ResourceKind::Ram => vec![ResourceKey::Ram],
                    ResourceKind::GpuCompute => allocation
                        .requested
                        .gpu_memory_mib
                        .keys()
                        .map(|uuid| ResourceKey::GpuCompute(uuid.clone()))
                        .collect(),
                    ResourceKind::GpuMemory => allocation
                        .requested
                        .gpu_memory_mib
                        .keys()
                        .map(|uuid| ResourceKey::GpuMemory(uuid.clone()))
                        .collect(),
                };
                for key in keys {
                    note(
                        &mut statuses,
                        key,
                        "mode_capped",
                        format!(
                            "allocation {} requested {}; the node cap is {}, so it runs in {}",
                            allocation.id,
                            mode_name(*requested),
                            mode_name(cap),
                            mode_name(cap)
                        ),
                    );
                }
            }
        }
        // The owner's aggregate on a resource comes from the node mode and cap;
        // per-job modes only order who yields inside it and bound each job.
        let governing = |kind: ResourceKind| config.contention.mode(kind).min(cap);
        // Each mode's target per resource, which bounds the jobs in that mode.
        let mut mode_targets: BTreeMap<ResourceKey, BTreeMap<ContentionMode, u64>> =
            BTreeMap::new();
        let targets_of = |view: &ModeInput, thresholds: &ResourceThresholds, limit: u64| {
            ContentionMode::ALL
                .into_iter()
                .map(|mode| {
                    let view = ModeInput {
                        mode,
                        ..view.clone()
                    };
                    (mode, own_allowance(&view, thresholds).min(limit))
                })
                .collect::<BTreeMap<_, _>>()
        };
        let mut cpu_held = unknown_charge.cpu_millicores > 0;
        let mut ram_held = unknown_charge.ram_mib > 0;
        for (key, held) in [(ResourceKey::Cpu, cpu_held), (ResourceKey::Ram, ram_held)] {
            if held {
                note(
                    &mut statuses,
                    key,
                    "allocation_unknown_hold",
                    "an allocation's usage is unknown; it is charged at max(request, last known) and the resource holds".into(),
                );
            }
        }
        for gpu in unknown_charge.gpu_memory_mib.keys() {
            note(
                &mut statuses,
                ResourceKey::GpuMemory(gpu.clone()),
                "allocation_unknown_hold",
                "an allocation's usage is unknown; it is charged at max(request, last known) and the device holds".into(),
            );
        }

        let mut budget = Resources::default();
        let mut unsafe_now = input.explicit_drain;
        let mut cpu_unknown = false;
        let mut ram_unknown = false;
        if input.explicit_drain {
            reasons.push(ReasonCode::new(
                "operator_drain",
                "explicit node drain requested",
            ));
        }
        if snapshot.cpu_capacity_millicores == 0 {
            reasons.push(ReasonCode::new(
                "cpu_capacity_unknown",
                "CPU capacity unavailable; zero CPU scheduling budget",
            ));
        }
        if snapshot.total_ram_mib == 0 {
            reasons.push(ReasonCode::new(
                "ram_capacity_unknown",
                "RAM capacity unavailable; zero RAM scheduling budget",
            ));
        }

        let scoped_kernel = snapshot
            .kernel
            .as_ref()
            .filter(|kernel| config.kernel.enabled && kernel.runtime_os == "linux");
        let mut next_kernel_cpu_sample = self.last_kernel_cpu_sample;
        let (cpu_capacity, cpu_busy, cpu_ceiling, scoped_reserve) = if let Some(kernel) =
            scoped_kernel
        {
            let mut valid = scoped_cpu_valid(kernel, &mut reasons);
            match self.last_kernel_cpu_sample {
                Some((sample, _)) if kernel.monotonic_elapsed_ms < sample => {
                    valid = false;
                    reasons.push(ReasonCode::new(
                        "kernel_cpu_stale",
                        "kernel CPU collector clock reset; establish a new baseline",
                    ));
                    next_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now_monotonic_ms));
                }
                Some((sample, received)) if kernel.monotonic_elapsed_ms == sample => {
                    if now_monotonic_ms.saturating_sub(received) > kernel.freshness_limit_ms {
                        valid = false;
                        reasons.push(ReasonCode::new(
                            "kernel_cpu_stale",
                            "kernel CPU snapshot is stale; repeated readings do not refresh it",
                        ));
                    }
                }
                _ => next_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now_monotonic_ms)),
            }
            let capacity = kernel.cpu.effective_cpu_capacity_millicores.unwrap_or(0);
            let reserve = scoped_physical_reserve(&kernel.cpu, config.cpu.reserve_physical_cores);
            if reserve.is_none() {
                valid = false;
                reasons.push(ReasonCode::new("cpu_topology_unknown", "effective CPU topology unavailable; cannot map physical-core reserve within permitted CPUs"));
            }
            if !valid {
                cpu_unknown = true;
            }
            reasons.push(ReasonCode::new("cpu_scoped_budget", "CPU budget uses matching effective-CPU usage and hardware capacity; visible ancestor bandwidth is a separate ceiling, not a guaranteed share"));
            (
                capacity,
                kernel.cpu.busy_millicores,
                kernel.cpu.visible_cpu_ceiling_millicores.unwrap_or(0),
                reserve,
            )
        } else {
            // Schema-v1 recordings and portable non-Linux collectors retain the
            // original system-scope arithmetic. Missing Linux scope data is not
            // replaced by an optimistic host aggregate when diagnostics are enabled.
            next_kernel_cpu_sample = None;
            (
                snapshot.cpu_capacity_millicores,
                snapshot.cpu_busy_millicores,
                snapshot.cpu_capacity_millicores,
                None,
            )
        };

        let cpu_reserve = if scoped_kernel.is_some() {
            scoped_reserve.unwrap_or(cpu_capacity)
        } else if config.cpu.reserve_physical_cores == 0 {
            0
        } else if let Some(physical) = snapshot.physical_cores {
            // Convert physical-core reserve into the telemetry's logical-CPU units.
            ceil_ratio(
                snapshot.cpu_capacity_millicores,
                u64::from(config.cpu.reserve_physical_cores),
                u64::from(physical),
            )
            .min(snapshot.cpu_capacity_millicores)
        } else {
            cpu_unknown = true;
            reasons.push(ReasonCode::new(
                "cpu_topology_unknown",
                "CPU topology unavailable; cannot establish configured physical-core reserve",
            ));
            snapshot.cpu_capacity_millicores
        };
        if let Some(busy) = cpu_busy {
            // Own usage is a lower bound and host busy an upper bound (schema 3 adds
            // the host lower bound). Overlapping bounds are consistent (R9).
            let own_lower = managed_observed.cpu_millicores;
            let host_lower = match scoped_kernel {
                Some(kernel) => kernel.cpu.busy_lower_millicores,
                None => snapshot.cpu_busy_lower_millicores,
            }
            .unwrap_or(busy)
            .min(busy);
            let external = if own_lower > busy {
                cpu_held = true;
                statuses.entry(ResourceKey::Cpu).or_default().measurement =
                    Some(Measurement::Unknown);
                reasons.push(ReasonCode::new("measurement_unknown_hold", 
                    "managed CPU observation exceeds busy observation in the selected scope beyond the known error; CPU holds and admission is blocked",
                ));
                note(
                    &mut statuses,
                    ResourceKey::Cpu,
                    "measurement_unknown_hold",
                    format!(
                        "own CPU lower bound {own_lower}m exceeds host busy upper bound {busy}m"
                    ),
                );
                0
            } else {
                if own_lower > host_lower {
                    statuses.entry(ResourceKey::Cpu).or_default().measurement =
                        Some(Measurement::InconsistentWithinTolerance);
                    note(
                        &mut statuses,
                        ResourceKey::Cpu,
                        "inconsistent_within_tolerance",
                        format!(
                            "own CPU {own_lower}m exceeds host busy lower bound {host_lower}m within the known error; external clamped"
                        ),
                    );
                }
                busy.saturating_sub(own_lower.saturating_add(unknown_charge.cpu_millicores))
            };
            budget.cpu_millicores = cpu_capacity
                .saturating_sub(cpu_reserve)
                .saturating_sub(external)
                .min(cpu_ceiling);
            let status = statuses.entry(ResourceKey::Cpu).or_default();
            status.external = Some(external);
            status.own = Some((
                own_lower,
                input
                    .allocations
                    .iter()
                    .filter_map(|a| a.observed.as_ref().map(|o| (a, o)))
                    .map(|(a, o)| {
                        a.observed_upper
                            .as_ref()
                            .map_or(o.cpu_millicores, |u| u.cpu_millicores.max(o.cpu_millicores))
                    })
                    .fold(0u64, u64::saturating_add),
            ));
        } else {
            cpu_unknown = true;
            reasons.push(ReasonCode::new(
                "cpu_usage_unknown",
                "CPU busy telemetry unavailable; CPU admission blocked",
            ));
        }
        let ram_reserve = config.ram_floor_mib(snapshot.total_ram_mib);
        // An unconfirmed floor near its minimum applies as twice the minimum (FR-054).
        let raised_note = |floor: &crate::config::FloorStatus, what: &str| {
            format!(
                "the {what} floor of {} MiB is near its {} MiB minimum and applies as {} MiB until acknowledge_caution confirms small_floor",
                floor.configured_mib.max(floor.minimum_mib),
                floor.minimum_mib,
                floor.effective_mib
            )
        };
        let ram_floor = config.ram_floor(snapshot.total_ram_mib);
        if ram_floor.raised {
            note(
                &mut statuses,
                ResourceKey::Ram,
                "small_floor_raised",
                raised_note(&ram_floor, "RAM"),
            );
        }
        // A floor breach is critical once free memory falls below this level, which
        // never goes below the minimum floor (FR-063).
        let critical_level = |floor: u64, minimum: u64| {
            (floor.saturating_mul(u64::from(config.contention.critical_floor_percent)) / 100)
                .max(minimum)
        };
        let mut critical_keys: BTreeSet<ResourceKey> = BTreeSet::new();
        if snapshot.available_ram_mib.is_some_and(|free| {
            free < critical_level(ram_reserve, crate::config::MIN_RAM_FLOOR_MIB)
        }) {
            critical_keys.insert(ResourceKey::Ram);
        }
        // A floor breach is observed free memory below the floor, as free and
        // floor; charges beyond the budget only block admission (FR-023).
        let mut floor_breaches: BTreeMap<ResourceKey, (u64, u64)> = BTreeMap::new();
        // Free memory above each floor, which a floor-breach step reverses with.
        let mut floor_room: BTreeMap<ResourceKey, u64> = BTreeMap::new();
        if let Some(free) = snapshot.available_ram_mib {
            floor_room.insert(ResourceKey::Ram, free.saturating_sub(ram_reserve));
        }
        if let Some(free) = snapshot
            .available_ram_mib
            .filter(|free| *free < ram_reserve)
        {
            floor_breaches.insert(ResourceKey::Ram, (free, ram_reserve));
        }
        if let Some(available) = snapshot.available_ram_mib {
            let used = snapshot.total_ram_mib - available;
            let own_lower = managed_observed.ram_mib;
            let external = if own_lower > used {
                ram_held = true;
                statuses.entry(ResourceKey::Ram).or_default().measurement =
                    Some(Measurement::Unknown);
                reasons.push(ReasonCode::new("measurement_unknown_hold", 
                    "managed RAM observation exceeds system used observation; RAM holds and admission is blocked",
                ));
                note(
                    &mut statuses,
                    ResourceKey::Ram,
                    "measurement_unknown_hold",
                    format!("own RAM lower bound {own_lower} MiB exceeds system used {used} MiB"),
                );
                0
            } else {
                used.saturating_sub(own_lower.saturating_add(unknown_charge.ram_mib))
            };
            budget.ram_mib = snapshot
                .total_ram_mib
                .saturating_sub(ram_reserve)
                .saturating_sub(external);
            let status = statuses.entry(ResourceKey::Ram).or_default();
            status.external = Some(external);
            status.own = Some((own_lower, own_lower));
        } else {
            ram_unknown = true;
            reasons.push(ReasonCode::new(
                "ram_usage_unknown",
                "RAM availability telemetry unavailable; RAM admission blocked",
            ));
        }
        if cpu_unknown {
            budget.cpu_millicores = 0;
            unsafe_now = true;
        }
        if ram_unknown {
            budget.ram_mib = 0;
            unsafe_now = true;
        }
        if cpu_held || ram_held {
            unsafe_now = true;
        }
        // Unmanaged usage by principal for one resource (research R12).
        // The owner is always a principal; system accounts are background; other
        // users join and leave after the principal window (FR-059).
        let mut principal_counts = BTreeMap::new();
        let mut principals = |key: ResourceKey,
                              capacity: u64,
                              pick: &dyn Fn(&PrincipalUsage) -> u64|
         -> (Option<Vec<(Principal, u64)>>, u64) {
            let Some(list) = snapshot.principals.as_ref() else {
                return (None, 0);
            };
            let floor = (capacity.saturating_mul(u64::from(
                config
                    .contention
                    .thresholds(key.kind())
                    .principal_min_percent,
            )) / 100)
                .max(1);
            let window = config.contention.principal_window_intervals;
            let leave_ms = config.contention.principal_leave_s.saturating_mul(1000);
            let mut members = Vec::new();
            let mut background = 0u64;
            let mut seen = BTreeSet::new();
            for p in list {
                let usage = pick(p);
                let principal = Principal {
                    uid: p.uid,
                    is_owner: p.is_owner,
                };
                match p.uid {
                    _ if p.is_owner => members.push((principal, usage)),
                    None => {}
                    Some(uid) if uid < config.contention.uid_min => {
                        background = background.saturating_add(usage);
                    }
                    Some(uid) => {
                        seen.insert(uid);
                        let mut count = self
                            .principal_counts
                            .get(&(key.clone(), uid))
                            .copied()
                            .unwrap_or_default();
                        // A user whose demand is on record as satisfied is known:
                        // it rejoins at once, so the record applies on its return.
                        if usage >= floor
                            && self.satisfied.get(&key).is_some_and(|records| {
                                records.iter().any(|(principal, _)| *principal == Some(uid))
                            })
                        {
                            count.admitted = true;
                        }
                        count.observe(usage, floor, window, now_monotonic_ms, leave_ms);
                        if count.admitted {
                            members.push((principal, usage));
                        }
                        if count.admitted || count.above > 0 {
                            principal_counts.insert((key.clone(), uid), count);
                        }
                    }
                }
            }
            // A member no longer seen has no usage.
            for ((k, uid), count) in &self.principal_counts {
                if k == &key && count.admitted && !seen.contains(uid) {
                    let mut count = *count;
                    count.observe(0, floor, window, now_monotonic_ms, leave_ms);
                    if count.admitted {
                        principal_counts.insert((key.clone(), *uid), count);
                    }
                }
            }
            (Some(members), background)
        };
        // What own work may hold before the mode's target applies: for memory, what
        // keeps the floor free.
        let mut floor_available: BTreeMap<ResourceKey, u64> = BTreeMap::new();
        let cpu_total = cpu_capacity.min(cpu_ceiling);
        // A saturated host suppresses measured external usage.
        let saturation = u64::from(
            config
                .contention
                .thresholds(ResourceKind::Cpu)
                .saturation_percent,
        );
        let utilization_saturated = cpu_busy
            .is_some_and(|busy| busy.saturating_mul(100) >= cpu_total.saturating_mul(saturation));
        // External PSI is a second CPU saturation signal; own and system-wide
        // pressure never count, and unknown pressure adds nothing (FR-072).
        let psi_threshold = config
            .contention
            .thresholds(ResourceKind::Cpu)
            .psi_some_percent;
        let cpu_pressure = snapshot
            .kernel
            .as_ref()
            .and_then(|kernel| kernel.cpu_pressure.as_ref());
        let external_psi = cpu_pressure
            .filter(|pressure| pressure.status == crate::kernel::CpuPressureStatus::Available)
            .and_then(|pressure| pressure.external_some_avg10);
        let psi_saturated = external_psi.is_some_and(|avg10| avg10 > f64::from(psi_threshold));
        let psi_only = psi_saturated && !utilization_saturated;
        let cpu_saturated = utilization_saturated || psi_saturated;
        match cpu_pressure.map(|pressure| pressure.status) {
            _ if psi_only => note(
                &mut statuses,
                ResourceKey::Cpu,
                "saturated_by_psi",
                format!(
                    "external PSI some avg10 {:.1}% is above {psi_threshold}%; own and system-wide pressure are not counted",
                    external_psi.unwrap_or(0.0)
                ),
            ),
            Some(crate::kernel::CpuPressureStatus::SystemOnly) => note(
                &mut statuses,
                ResourceKey::Cpu,
                "psi_system_only",
                "only system-wide CPU pressure is readable; it cannot tell own from external waiting, so it adds no evidence".into(),
            ),
            Some(crate::kernel::CpuPressureStatus::Unknown) => note(
                &mut statuses,
                ResourceKey::Cpu,
                "psi_unknown",
                format!(
                    "CPU pressure is missing or unreadable, so it adds no evidence: {}",
                    cpu_pressure.map_or("", |pressure| pressure.detail.as_str())
                ),
            ),
            _ => {}
        }
        let mut satisfied = BTreeMap::new();
        let mut probes = BTreeMap::new();
        let mut principal_seen = BTreeMap::new();
        // Satisfied records keep at most the most recently seen principals.
        let mut bounded =
            |key: &ResourceKey, view: &ModeInput, known: &mut Vec<(Option<u32>, u64)>| {
                let mut seen = self.principal_seen.get(key).cloned().unwrap_or_default();
                let mut accounted = view.background;
                for (principal, usage) in view.principals.iter().flatten() {
                    accounted = accounted.saturating_add(*usage);
                    if !principal.is_owner && *usage > 0 {
                        seen.insert(principal.uid, now_monotonic_ms);
                    }
                }
                if view.external > accounted {
                    seen.insert(None, now_monotonic_ms);
                }
                crate::contention::bound_satisfied(known, &seen);
                seen.retain(|uid, _| known.iter().any(|(principal, _)| principal == uid));
                if !seen.is_empty() {
                    principal_seen.insert(key.clone(), seen);
                }
            };
        if !cpu_unknown && !cpu_held {
            floor_available.insert(ResourceKey::Cpu, budget.cpu_millicores);
            let (members, background) =
                principals(ResourceKey::Cpu, cpu_capacity, &|p| p.cpu_millicores);
            let thresholds = config.contention.thresholds(ResourceKind::Cpu);
            let mode = governing(ResourceKind::Cpu);
            let mut view = ModeInput {
                kind: ResourceKind::Cpu,
                mode,
                capacity: cpu_capacity,
                external: statuses
                    .get(&ResourceKey::Cpu)
                    .and_then(|s| s.external)
                    .unwrap_or(0),
                own: statuses
                    .get(&ResourceKey::Cpu)
                    .and_then(|s| s.own)
                    .map_or(0, |(lower, _)| lower),
                principals: members,
                background,
                saturated: cpu_saturated,
                psi_only,
                unknown_activity: false,
                satisfied: self
                    .satisfied
                    .get(&ResourceKey::Cpu)
                    .cloned()
                    .unwrap_or_default(),
                probe: None,
            };
            let response_ms =
                LadderTiming::new(&thresholds, mode, config.monitor.interval_ms).response_ms;
            let (mut known, probe) = crate::contention::probe_demand(
                &view,
                &thresholds,
                self.probes.get(&ResourceKey::Cpu),
                response_ms,
                now_monotonic_ms,
            );
            bounded(&ResourceKey::Cpu, &view, &mut known);
            view.satisfied = known.clone();
            // A watch keeps own work at the fair share; only a probe sets a target.
            view.probe = probe.as_ref().filter(|p| !p.watch).map(|p| p.target);
            note_demand(&mut statuses, ResourceKey::Cpu, &known, probe.as_ref());
            if !known.is_empty() {
                satisfied.insert(ResourceKey::Cpu, known);
            }
            if let Some(probe) = probe {
                probes.insert(ResourceKey::Cpu, probe);
            }
            mode_targets.insert(
                ResourceKey::Cpu,
                targets_of(&view, &thresholds, budget.cpu_millicores),
            );
            // The idle reserve is the larger of the explicit reserve and the mode's
            // margin of total capacity; the two never stack.
            budget.cpu_millicores = own_allowance(&view, &thresholds)
                // Capacity minus the reserve and external usage, within the ancestor quota.
                .min(budget.cpu_millicores);
        }
        if !ram_unknown && !ram_held {
            floor_available.insert(ResourceKey::Ram, budget.ram_mib);
            let capacity = snapshot.total_ram_mib.saturating_sub(ram_reserve);
            let (members, background) = principals(ResourceKey::Ram, capacity, &|p| p.ram_mib);
            let view = ModeInput {
                kind: ResourceKind::Ram,
                mode: governing(ResourceKind::Ram),
                capacity,
                external: statuses
                    .get(&ResourceKey::Ram)
                    .and_then(|s| s.external)
                    .unwrap_or(0),
                own: statuses
                    .get(&ResourceKey::Ram)
                    .and_then(|s| s.own)
                    .map_or(0, |(lower, _)| lower),
                principals: members,
                background,
                saturated: false,

                psi_only: false,
                unknown_activity: false,
                satisfied: vec![],
                probe: None,
            };
            let thresholds = config.contention.thresholds(ResourceKind::Ram);
            mode_targets.insert(ResourceKey::Ram, targets_of(&view, &thresholds, u64::MAX));
            budget.ram_mib = own_allowance(&view, &thresholds);
        }
        // The operator envelope bounds own work per resource (FR-057).
        if let Some(envelope) = &input.envelope {
            budget.cpu_millicores = budget.cpu_millicores.min(envelope.cpu_millicores);
            budget.ram_mib = budget.ram_mib.min(envelope.ram_mib);
            for (key, limit) in [
                (ResourceKey::Cpu, envelope.cpu_millicores),
                (ResourceKey::Ram, envelope.ram_mib),
            ] {
                for target in mode_targets
                    .get_mut(&key)
                    .into_iter()
                    .flat_map(|t| t.values_mut())
                {
                    *target = (*target).min(limit);
                }
            }
        }
        for (key, floor) in [
            (ResourceKey::Cpu, cpu_reserve),
            (ResourceKey::Ram, ram_reserve),
        ] {
            let status = statuses.entry(key.clone()).or_default();
            status.floor = Some(floor);
            if floor_available.contains_key(&key) {
                status.target_share = Some(match key {
                    ResourceKey::Cpu => budget.cpu_millicores,
                    _ => budget.ram_mib,
                });
            }
        }

        let mut next_gpus = self.previous_gpus.clone();
        for value in next_gpus.values_mut() {
            value.stable_since = None;
        }
        let mut blocked_gpus = BTreeSet::new();
        // Each device's GPU compute allowance and whether it is contended, and the
        // devices whose external activity is unknown.
        let mut gpu_compute_budget: BTreeMap<String, GpuComputeBudget> = BTreeMap::new();
        let mut compute_paths: BTreeMap<String, ComputePath> = BTreeMap::new();
        let mut gpu_activity_unknown: BTreeSet<String> = BTreeSet::new();
        // Why GPU admission is blocked on a device, for its admission state.
        let mut gpu_block_reasons: BTreeMap<String, String> = BTreeMap::new();
        if snapshot.gpu_inventory != CapabilityStatus::Available {
            reasons.push(ReasonCode::new(
                "gpu_inventory_unavailable",
                format!(
                    "GPU inventory {:?}; GPU admission blocked, CPU-only capacity remains eligible",
                    snapshot.gpu_inventory
                ),
            ));
            for gpu in charged.gpu_memory_mib.keys() {
                budget.gpu_memory_mib.insert(gpu.clone(), 0);
                blocked_gpus.insert(gpu.clone());
                unsafe_now = true;
            }
        } else {
            for gpu in &snapshot.gpus {
                let key = ResourceKey::GpuMemory(gpu.uuid.clone());
                // No growth or new-PID event without a prior observation of this device
                // by this engine; an allocation's first observation is a baseline too.
                let device_first = !self.previous_gpus.contains_key(&gpu.uuid);
                let baseline = device_first || first_observed_gpus.contains(&gpu.uuid);
                let previous = self
                    .previous_gpus
                    .get(&gpu.uuid)
                    .cloned()
                    .unwrap_or_default();
                let observed = managed_observed
                    .gpu_memory_mib
                    .get(&gpu.uuid)
                    .copied()
                    .unwrap_or(0);
                let unknown_own = unknown_charge
                    .gpu_memory_mib
                    .get(&gpu.uuid)
                    .copied()
                    .unwrap_or(0);
                let current_charge = charged.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0);
                let external = gpu
                    .used_memory_mib
                    .map(|used| used.saturating_sub(observed).saturating_sub(unknown_own));
                let memory_budget = external
                    .map(|used| {
                        gpu.total_memory_mib
                            .saturating_sub(config.vram_floor_mib(gpu.total_memory_mib))
                            .saturating_sub(used)
                    })
                    .unwrap_or(0);
                let pids: BTreeSet<u32> = gpu
                    .external_process_ids
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                let new_external =
                    !device_first && pids.difference(&previous.pids).next().is_some();
                // Growth must exceed the noise tolerance beyond the unattributed band (FR-005, FR-006).
                let tolerance = config
                    .contention
                    .thresholds(ResourceKind::GpuMemory)
                    .noise_tolerance_mib
                    .saturating_add(gpu.unattributed_memory_mib.unwrap_or(0));
                let (memory_growth, next_baseline) = match (external, previous.external_memory) {
                    (Some(current), Some(old)) if !baseline => {
                        if current > old.saturating_add(tolerance) {
                            (true, Some(current))
                        } else {
                            if current > old {
                                note(
                                    &mut statuses,
                                    key.clone(),
                                    "vram_noise_below_tolerance",
                                    format!(
                                        "external memory grew {} MiB, within the {tolerance} MiB tolerance",
                                        current - old
                                    ),
                                );
                            }
                            (false, Some(old.min(current)))
                        }
                    }
                    (Some(current), _) => {
                        note(
                            &mut statuses,
                            key.clone(),
                            "first_observation_baseline",
                            format!(
                                "{current} MiB not attributed to own work is the baseline, not growth"
                            ),
                        );
                        (false, Some(current))
                    }
                    (None, old) => (false, old),
                };
                // Distinct IDs can indicate fresh samples; repeated values never add evidence.
                let fresh_sample = gpu.compute_sample_id.is_some_and(|sample| {
                    previous.sample_id.is_none_or(|previous| sample > previous)
                });
                let activity_unknown = gpu.external_compute == ComputeActivity::Unknown
                    || (!pids.is_empty() && !fresh_sample);
                let telemetry_unknown = !gpu_observable(snapshot.observed_at_unix_ms, gpu)
                    || unknown_own > 0
                    || gpu.used_memory_mib.is_some_and(|used| observed > used);
                let breached = current_charge > memory_budget;
                let protection_event = new_external || memory_growth;
                let activity = if activity_unknown {
                    gpu_activity_unknown.insert(gpu.uuid.clone());
                    ComputeActivity::Unknown
                } else {
                    gpu.external_compute
                };
                // The same rule the launch recheck applies (FR-026, FR-027, FR-064).
                let compute_admission = crate::contention::gpu_admission(
                    governing(ResourceKind::GpuCompute),
                    activity,
                    gpu.external_process_ids.as_ref().map(Vec::len),
                    !telemetry_unknown,
                    config.contention.never_share_occupied_gpus,
                );
                let block_code = if telemetry_unknown {
                    Some("gpu_memory_unknown".to_string())
                } else if protection_event {
                    Some("gpu_external_workload_appeared".into())
                } else if breached {
                    Some("gpu_charges_exceed_reserve".into())
                } else {
                    compute_admission.code().map(str::to_string)
                };
                if let Some(code) = &block_code {
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    reasons.push(ReasonCode::new(
                        code,
                        format!(
                            "GPU {}: {} GPU admission is blocked.",
                            gpu.uuid,
                            crate::contention::reason_text(code).unwrap_or(code)
                        ),
                    ));
                    gpu_block_reasons.insert(gpu.uuid.clone(), code.clone());
                }
                // A measured external amount takes the CPU paths (FR-021, FR-022):
                // one principal of that amount, saturation from device utilization,
                // probing and satisfied records. Without one, external activity counts
                // as one principal of unknown amount. Margins on an idle device act
                // through admission only.
                let compute_key = ResourceKey::GpuCompute(gpu.uuid.clone());
                let compute_thresholds = config.contention.thresholds(ResourceKind::GpuCompute);
                // The unknown path applies at once; the measured path returns only
                // after the quiet window of consecutive measured intervals.
                let measured_streak = if gpu.external_compute_milli.is_some() {
                    previous.measured_streak.saturating_add(1)
                } else {
                    0
                };
                let measured_window = config.contention.quiet_window_intervals(
                    config.monitor.interval_ms,
                    config.gpu.process_sample_max_age_ms,
                );
                let settled = measured_streak >= measured_window;
                let measured = gpu
                    .external_compute_milli
                    .filter(|_| settled)
                    .map(|external| {
                        (
                            external.min(GPU_COMPUTE_CAPACITY),
                            gpu.own_compute_milli
                                .unwrap_or(GPU_COMPUTE_CAPACITY)
                                .min(GPU_COMPUTE_CAPACITY),
                        )
                    });
                let path = match measured {
                    Some(_) => ComputePath::Measured,
                    None if gpu.external_compute_milli.is_some() => ComputePath::Unknown {
                        code: "gpu_compute_recently_inconsistent".into(),
                        reason: format!(
                            "per-process samples were recently inconsistent or are new: measured {measured_streak} of {measured_window} consecutive intervals"
                        ),
                    },
                    None => ComputePath::Unknown {
                        code: if gpu.compute_amount_unknown.is_some() {
                            "gpu_compute_amount_unknown"
                        } else {
                            "gpu_compute_unknown_activity"
                        }
                        .into(),
                        reason: gpu.compute_amount_unknown.clone().unwrap_or_else(|| {
                            "the collector did not measure the external amount".into()
                        }),
                    },
                };
                note(
                    &mut statuses,
                    compute_key.clone(),
                    path.code(),
                    match &path {
                        ComputePath::Measured => {
                            "external GPU compute is measured from per-process samples".into()
                        }
                        ComputePath::Unknown { reason, .. } => reason.clone(),
                    },
                );
                compute_paths.insert(gpu.uuid.clone(), path);
                if !telemetry_unknown {
                    let budget = match measured {
                        Some((external, own)) if external > 0 => {
                            let mode = governing(ResourceKind::GpuCompute);
                            // External users are principals by UID, as on CPU.
                            let (members, background) =
                                principals(compute_key.clone(), GPU_COMPUTE_CAPACITY, &|p| {
                                    p.gpu_compute_milli.get(&gpu.uuid).copied().unwrap_or(0)
                                });
                            let mut view = ModeInput {
                                kind: ResourceKind::GpuCompute,
                                mode,
                                capacity: GPU_COMPUTE_CAPACITY,
                                external,
                                own,
                                principals: members,
                                background,
                                saturated: gpu.utilization_percent.is_some_and(|busy| {
                                    busy >= u32::from(compute_thresholds.saturation_percent)
                                }),
                                psi_only: false,
                                unknown_activity: false,
                                satisfied: self
                                    .satisfied
                                    .get(&compute_key)
                                    .cloned()
                                    .unwrap_or_default(),
                                probe: None,
                            };
                            let response_ms = LadderTiming::new(
                                &compute_thresholds,
                                mode,
                                config.monitor.interval_ms,
                            )
                            .response_ms;
                            let (mut known, probe) = crate::contention::probe_demand(
                                &view,
                                &compute_thresholds,
                                self.probes.get(&compute_key),
                                response_ms,
                                now_monotonic_ms,
                            );
                            bounded(&compute_key, &view, &mut known);
                            view.satisfied = known.clone();
                            view.probe = probe.as_ref().filter(|p| !p.watch).map(|p| p.target);
                            note_demand(&mut statuses, compute_key.clone(), &known, probe.as_ref());
                            if !known.is_empty() {
                                satisfied.insert(compute_key.clone(), known);
                            }
                            if let Some(probe) = probe {
                                probes.insert(compute_key.clone(), probe);
                            }
                            GpuComputeBudget {
                                share: own_allowance(&view, &compute_thresholds),
                                contended: crate::contention::contended(&view, &compute_thresholds),
                                external,
                                own: Some(own),
                                targets: targets_of(
                                    &view,
                                    &compute_thresholds,
                                    GPU_COMPUTE_CAPACITY,
                                ),
                            }
                        }
                        Some(_) => GpuComputeBudget {
                            share: GPU_COMPUTE_CAPACITY,
                            contended: false,
                            external: 0,
                            own: None,
                            targets: BTreeMap::new(),
                        },
                        None if matches!(
                            activity,
                            ComputeActivity::Active | ComputeActivity::Unknown
                        ) =>
                        {
                            let view = ModeInput {
                                kind: ResourceKind::GpuCompute,
                                mode: governing(ResourceKind::GpuCompute),
                                capacity: GPU_COMPUTE_CAPACITY,
                                external: 0,
                                own: GPU_COMPUTE_CAPACITY,
                                principals: None,
                                background: 0,
                                saturated: false,

                                psi_only: false,
                                unknown_activity: true,
                                satisfied: vec![],
                                probe: None,
                            };
                            GpuComputeBudget {
                                share: own_allowance(&view, &compute_thresholds),
                                contended: true,
                                external: GPU_COMPUTE_CAPACITY,
                                own: None,
                                targets: targets_of(
                                    &view,
                                    &compute_thresholds,
                                    GPU_COMPUTE_CAPACITY,
                                ),
                            }
                        }
                        None => GpuComputeBudget {
                            share: GPU_COMPUTE_CAPACITY,
                            contended: false,
                            external: 0,
                            own: None,
                            targets: BTreeMap::new(),
                        },
                    };
                    gpu_compute_budget.insert(gpu.uuid.clone(), budget);
                }
                let compute_status = statuses.entry(compute_key).or_default();
                compute_status.activity = Some(activity);
                if let Some((external, own)) = measured {
                    compute_status.external = Some(external);
                    compute_status.own = Some((own, own));
                }
                compute_status.target_share =
                    gpu_compute_budget.get(&gpu.uuid).map(|budget| budget.share);
                compute_status.admission = Some(if telemetry_unknown {
                    Admission::held("gpu_observation_unknown")
                } else {
                    compute_admission
                });
                let envelope_gpu = input
                    .envelope
                    .as_ref()
                    .map(|e| e.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0));
                let gpu_budget = if telemetry_unknown {
                    memory_budget
                } else {
                    floor_available.insert(key.clone(), memory_budget);
                    let vram_floor = config.vram_floor(gpu.total_memory_mib);
                    if vram_floor.raised {
                        note(
                            &mut statuses,
                            key.clone(),
                            "small_floor_raised",
                            raised_note(&vram_floor, "GPU memory"),
                        );
                    }
                    if gpu.used_memory_mib.is_some_and(|used| {
                        gpu.total_memory_mib.saturating_sub(used)
                            < critical_level(
                                config.vram_floor_mib(gpu.total_memory_mib),
                                crate::config::MIN_VRAM_FLOOR_MIB,
                            )
                    }) {
                        critical_keys.insert(key.clone());
                    }
                    let floor = config.vram_floor_mib(gpu.total_memory_mib);
                    let free = gpu
                        .used_memory_mib
                        .map(|used| gpu.total_memory_mib.saturating_sub(used));
                    if let Some(free) = free {
                        floor_room.insert(key.clone(), free.saturating_sub(floor));
                    }
                    if let Some(free) = free.filter(|free| *free < floor) {
                        floor_breaches.insert(key.clone(), (free, floor));
                    }
                    let capacity = gpu
                        .total_memory_mib
                        .saturating_sub(config.vram_floor_mib(gpu.total_memory_mib));
                    let (members, background) = principals(key.clone(), capacity, &|p| {
                        p.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0)
                    });
                    let view = ModeInput {
                        kind: ResourceKind::GpuMemory,
                        mode: governing(ResourceKind::GpuMemory),
                        capacity,
                        external: external.unwrap_or(0),
                        own: observed,
                        principals: members,
                        background,
                        saturated: false,

                        psi_only: false,
                        unknown_activity: false,
                        satisfied: vec![],
                        probe: None,
                    };
                    let thresholds = config.contention.thresholds(ResourceKind::GpuMemory);
                    mode_targets.insert(
                        key.clone(),
                        targets_of(&view, &thresholds, envelope_gpu.unwrap_or(u64::MAX)),
                    );
                    own_allowance(&view, &thresholds)
                };
                let gpu_budget = envelope_gpu.map_or(gpu_budget, |limit| gpu_budget.min(limit));
                budget.gpu_memory_mib.insert(gpu.uuid.clone(), gpu_budget);
                let status = statuses.entry(key).or_default();
                status.external = external;
                status.own = Some((observed, observed));
                status.activity = Some(gpu.external_compute);
                status.unattributed = gpu.unattributed_memory_mib;
                if telemetry_unknown {
                    status.measurement = Some(Measurement::Unknown);
                }
                next_gpus.insert(
                    gpu.uuid.clone(),
                    PreviousGpu {
                        pids,
                        external_memory: next_baseline,
                        sample_id: gpu.compute_sample_id.max(previous.sample_id),
                        // A device's memory stability depends on that device alone,
                        // never on CPU or RAM (FR-034). NVML reports no device whose
                        // memory is host RAM, so no device keeps a RAM coupling.
                        stable_since: if !blocked_gpus.contains(&gpu.uuid) && !input.explicit_drain
                        {
                            Some(previous.stable_since.unwrap_or(now_monotonic_ms))
                        } else {
                            None
                        },
                        measured_streak,
                    },
                );
            }
            for gpu in charged.gpu_memory_mib.keys() {
                if !snapshot.gpus.iter().any(|item| &item.uuid == gpu) {
                    budget.gpu_memory_mib.insert(gpu.clone(), 0);
                    blocked_gpus.insert(gpu.clone());
                    unsafe_now = true;
                    reasons.push(ReasonCode::new(
                        "gpu_absent",
                        format!("allocated GPU {gpu} is absent from the current inventory"),
                    ));
                }
            }
        }

        if charged.cpu_millicores > budget.cpu_millicores || charged.ram_mib > budget.ram_mib {
            unsafe_now = true;
        }
        let stable_since = if unsafe_now {
            None
        } else {
            Some(self.stable_since.unwrap_or(now_monotonic_ms))
        };
        let cooled_down = stable_since.is_some_and(|since| {
            now_monotonic_ms - since
                >= config.gpu_memory_cooldown_ms(governing(ResourceKind::GpuMemory))
        });
        if input.explicit_drain {
            budget = Resources::default();
        }

        let mut would_drain = Vec::new();
        let interval_ms = config.monitor.interval_ms;
        // An unknown or held resource first holds; after the bounded unknown window the
        // ladder may escalate with cause unknown_measurement (FR-008, R10).
        let window = |kind: ResourceKind| {
            u64::from(config.contention.thresholds(kind).unknown_window_intervals)
                .saturating_mul(interval_ms)
        };
        let mut unknown_since = BTreeMap::new();
        for (key, unknown) in [
            (ResourceKey::Cpu, cpu_unknown || cpu_held),
            (ResourceKey::Ram, ram_unknown || ram_held),
        ] {
            if unknown {
                let start = self
                    .unknown_since
                    .get(&key)
                    .copied()
                    .unwrap_or(now_monotonic_ms);
                unknown_since.insert(key, start);
            }
        }
        let mut allocation_unknown_since = BTreeMap::new();
        for id in &unknown_allocations {
            let start = self
                .allocation_unknown_since
                .get(id)
                .copied()
                .unwrap_or(now_monotonic_ms);
            allocation_unknown_since.insert(id.clone(), start);
        }
        let mut ladder = BTreeMap::new();
        // Resources where own work holds more than its target: reclaim is under way.
        // The largest amount own work holds above its target, per resource.
        let mut over_target: BTreeMap<ResourceKey, u64> = BTreeMap::new();
        let mut note_excess = |key: &ResourceKey, excess: u64| {
            let largest = over_target.entry(key.clone()).or_default();
            *largest = (*largest).max(excess);
        };
        // The deadline each hint carries to its worker, as the ladder enforces it.
        let mut hint_deadlines: BTreeMap<(String, ResourceKey), u64> = BTreeMap::new();
        let mut actions = Vec::new();
        let mut breached: BTreeSet<ResourceKey> = BTreeSet::new();
        // Memory resources with a floor breach or own usage beyond the budget.
        let mut memory_reclaim: BTreeSet<ResourceKey> = BTreeSet::new();
        let mut floor_breach_since = BTreeMap::new();
        if input.explicit_drain {
            // Explicit node drain keeps its all-allocation stop semantics.
            let mut candidates: Vec<_> = input
                .allocations
                .iter()
                .filter(|a| a.phase != AllocationPhase::Draining)
                .collect();
            candidates.sort_by_key(|a| (a.phase != AllocationPhase::Pending, a.id.as_str()));
            for allocation in candidates {
                actions.push(YieldAction {
                    allocation_id: allocation.id.clone(),
                    resource: first_charged_resource(&charges[&allocation.id]),
                    step: YieldStep::Drain,
                    target: None,
                    cause: YieldCause::OperatorDrain,
                    code: "yield_step_drain".into(),
                    deadline_ms: None,
                });
            }
        } else {
            // Memory first because only its holder can release it, then CPU.
            let mut keys = vec![ResourceKey::Ram];
            keys.extend(
                charged
                    .gpu_memory_mib
                    .keys()
                    .map(|uuid| ResourceKey::GpuMemory(uuid.clone())),
            );
            keys.push(ResourceKey::Cpu);
            for key in keys {
                let kind = key.kind();
                let held = unknown_since.contains_key(&key);
                let measurement_known = !held
                    || unknown_since.get(&key).is_some_and(|start| {
                        now_monotonic_ms.saturating_sub(*start) >= window(kind)
                    });
                let gpu_unknown = matches!(&key, ResourceKey::GpuMemory(uuid) if blocked_gpus.contains(uuid))
                    && statuses
                        .get(&key)
                        .is_some_and(|s| s.measurement == Some(Measurement::Unknown));
                let amount_of = |r: &Resources| match &key {
                    ResourceKey::Cpu => r.cpu_millicores,
                    ResourceKey::Ram => r.ram_mib,
                    ResourceKey::GpuCompute(uuid) | ResourceKey::GpuMemory(uuid) => {
                        r.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                    }
                };
                // Existing drains are planned releases for choosing more victims only.
                let draining_charge = input
                    .allocations
                    .iter()
                    .filter(|a| a.phase == AllocationPhase::Draining)
                    .map(|a| amount_of(&charges[&a.id]))
                    .fold(0u64, u64::saturating_add);
                let held_charge = amount_of(&charged).saturating_sub(draining_charge);
                // The supervisor reports the steps, the hint among them, that it cannot
                // apply. Only a worker that acknowledged its target file takes a hint
                // (FR-062); recordings without facts have no target channel.
                // A RAM cap needs a backend that reports it can apply one.
                let facts = |id: &str| {
                    let mut facts = input.facts.get(id).cloned().unwrap_or_default();
                    if !facts.hint_ready {
                        facts
                            .unavailable
                            .entry(key.clone())
                            .or_default()
                            .insert(YieldStep::Hint);
                    }
                    if key == ResourceKey::Ram && !facts.ram_cap {
                        facts
                            .unavailable
                            .entry(key.clone())
                            .or_default()
                            .insert(YieldStep::Cap);
                    }
                    facts
                };
                // Running memory work is judged on usage. On GPU memory, work that
                // cannot take a hint keeps its charge, since its growth is unbounded.
                let judged = |a: &Allocation| -> u64 {
                    match (&key, &a.observed) {
                        (ResourceKey::Ram, Some(observed)) => observed.ram_mib,
                        (ResourceKey::GpuMemory(uuid), Some(observed))
                            if input.facts.get(&a.id).is_some_and(|f| f.hint_ready) =>
                        {
                            observed.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                        }
                        _ => amount_of(&charges[&a.id]),
                    }
                };
                let judged_total = input
                    .allocations
                    .iter()
                    .filter(|a| {
                        a.phase != AllocationPhase::Pending && a.phase != AllocationPhase::Draining
                    })
                    .map(judged)
                    .fold(0u64, u64::saturating_add);
                let floor_breach = floor_breaches.get(&key).copied();
                if let Some((free, floor)) = floor_breach {
                    breached.insert(key.clone());
                    floor_breach_since.insert(
                        key.clone(),
                        self.floor_breach_since
                            .get(&key)
                            .copied()
                            .unwrap_or(now_monotonic_ms),
                    );
                    note(
                        &mut statuses,
                        key.clone(),
                        "floor_breach",
                        format!("free memory {free} MiB is below the {floor} MiB floor"),
                    );
                }
                let floor_breach = floor_breach.is_some();
                // Own work beyond the operator envelope, judged like a floor breach.
                let envelope_breach = input.envelope.as_ref().is_some_and(|envelope| {
                    let held = if kind == ResourceKind::Cpu {
                        input
                            .allocations
                            .iter()
                            .filter(|a| a.phase != AllocationPhase::Pending)
                            .map(|a| a.observed.as_ref().map_or(0, |o| o.cpu_millicores))
                            .fold(unknown_charge.cpu_millicores, u64::saturating_add)
                    } else {
                        judged_total
                    };
                    held > amount_of(envelope)
                });
                if envelope_breach {
                    note(
                        &mut statuses,
                        key.clone(),
                        "envelope_breach",
                        format!(
                            "own work exceeds the operator envelope of {}",
                            amount_of(input.envelope.as_ref().unwrap_or(&Resources::default()))
                        ),
                    );
                    if !floor_breach {
                        floor_breach_since.insert(
                            key.clone(),
                            self.floor_breach_since
                                .get(&key)
                                .copied()
                                .unwrap_or(now_monotonic_ms),
                        );
                    }
                }
                let cause = if held || gpu_unknown {
                    YieldCause::UnknownMeasurement
                } else if floor_breach {
                    YieldCause::FloorBreach
                } else if envelope_breach {
                    YieldCause::EnvelopeBreach
                } else {
                    YieldCause::ExternalGrowth
                };
                let external = statuses.get(&key).and_then(|s| s.external).unwrap_or(0);
                let critical = critical_keys.contains(&key);
                let mut timing = LadderTiming::new(
                    &config.contention.thresholds(kind),
                    governing(kind),
                    interval_ms,
                );
                if critical {
                    timing.hint_deadline_ms = config
                        .contention
                        .critical_hint_deadline_s
                        .saturating_mul(1000);
                    timing.dwell_ms = timing.dwell_ms.min(timing.hint_deadline_ms);
                }
                // Each recent reclaim episode doubles what a floor-breach step's
                // reversal also waits (FR-013, FR-061).
                if kind.is_memory() {
                    let thresholds = config.contention.thresholds(kind);
                    let window_ms = thresholds.memory_peak_window_s.saturating_mul(1000);
                    let episodes = self.reclaim_episodes.get(&key).map_or(0, |starts| {
                        starts
                            .iter()
                            .filter(|start| now_monotonic_ms.saturating_sub(**start) < window_ms)
                            .count()
                    }) as u32;
                    if episodes > 0 {
                        timing.breach_backoff_ms = thresholds
                            .reclaim_backoff_ms
                            .saturating_mul(1u64.checked_shl(episodes - 1).unwrap_or(u64::MAX))
                            .min(thresholds.reclaim_backoff_max_ms);
                    }
                }
                // Each job yields and rises at its own mode's pace.
                let timing_for = |mode: ContentionMode| {
                    let paced =
                        LadderTiming::new(&config.contention.thresholds(kind), mode, interval_ms);
                    LadderTiming {
                        response_ms: paced.response_ms,
                        scale_up_ms: paced.scale_up_ms,
                        ..timing.clone()
                    }
                };
                let mode_of = |a: &Allocation| allocation_modes[&a.id][&kind];
                let usage_of =
                    |a: &Allocation| -> u64 { a.observed.as_ref().map_or(0, |o| o.cpu_millicores) };
                // Pending reservations are cancelled by their charge. CPU in use is
                // measured by usage against the mode's allowance, since a cap lowers
                // usage but never the request.
                let admission_excess = held_charge.saturating_sub(amount_of(&budget));
                // Running work yields for what it holds, not for its charge: a
                // charge-only shortfall blocks admission and steps nothing.
                let usage_excess = if kind == ResourceKind::Cpu {
                    input
                        .allocations
                        .iter()
                        .filter(|a| a.phase != AllocationPhase::Pending)
                        .map(usage_of)
                        .fold(unknown_charge.cpu_millicores, u64::saturating_add)
                        .saturating_sub(budget.cpu_millicores)
                } else {
                    judged_total.saturating_sub(amount_of(&budget))
                };
                if usage_excess > 0 {
                    note_excess(&key, usage_excess);
                }
                let guaranteed =
                    |a: &Allocation| a.class == crate::execution_model::AllocationClass::Guaranteed;
                // Opportunistic work nobody can act on cannot be released first.
                let opportunistic_left = input.allocations.iter().any(|a| {
                    !guaranteed(a)
                        && amount_of(&charges[&a.id]) > 0
                        && !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable)
                });
                // Guaranteed memory is drained last (FR-024): after its hint deadline
                // when it can take a hint and, for RAM, the breach window. The critical
                // path uses the critical hint deadline and skips the window (FR-063).
                let breached_for = floor_breach_since
                    .get(&key)
                    .map_or(0, |start| now_monotonic_ms.saturating_sub(*start));
                // The ladder's hint step gives Guaranteed work its own hint deadline;
                // RAM also waits for the breach window, except on the critical path.
                let guaranteed_drain = guaranteed_may_drain(
                    kind,
                    cause,
                    opportunistic_left,
                    breached_for,
                    if critical {
                        0
                    } else {
                        config
                            .contention
                            .thresholds(kind)
                            .guaranteed_breach_window_s
                            .saturating_mul(1000)
                    },
                );
                let candidate = |a: &&Allocation| {
                    a.phase != AllocationPhase::Draining
                        && allocation_unknown_since.get(&a.id).is_none_or(|start| {
                            now_monotonic_ms.saturating_sub(*start) >= window(kind)
                        })
                };
                // A Guaranteed allocation keeps its CPU request: only usage above it
                // can be capped (FR-024).
                let kept_cpu = |a: &Allocation| {
                    if kind == ResourceKind::Cpu && guaranteed(a) {
                        a.requested.cpu_millicores
                    } else {
                        0
                    }
                };
                // What a job's own mode allows it here; Guaranteed keeps its CPU request.
                let ceiling = |a: &Allocation| {
                    mode_targets
                        .get(&key)
                        .map(|targets| targets[&mode_of(a)].max(kept_cpu(a)))
                };
                // What the mode allows beyond current usage, shared by the caps,
                // pauses and hints on this resource so they can rise again while calm.
                let held_back = self
                    .ladder
                    .iter()
                    .filter(|((_, k), state)| {
                        k == &key
                            && matches!(
                                state.step,
                                YieldStep::Cap | YieldStep::Pause | YieldStep::Hint
                            )
                    })
                    .count() as u64;
                let spare_share = if held_back == 0 {
                    0
                } else if kind == ResourceKind::Cpu {
                    let usage = input
                        .allocations
                        .iter()
                        .filter(|a| a.phase != AllocationPhase::Pending)
                        .map(usage_of)
                        .fold(unknown_charge.cpu_millicores, u64::saturating_add);
                    budget.cpu_millicores.saturating_sub(usage) / held_back
                } else {
                    amount_of(&budget).saturating_sub(judged_total) / held_back
                };
                if kind.is_memory() && (floor_breach || usage_excess > 0) {
                    memory_reclaim.insert(key.clone());
                }
                let mut planned = BTreeSet::new();
                for pending_pass in [true, false] {
                    let needed = if pending_pass {
                        admission_excess
                    } else {
                        usage_excess
                    };
                    let mut victims: Vec<VictimFacts> = input
                        .allocations
                        .iter()
                        .filter(candidate)
                        .filter(|a| (a.phase == AllocationPhase::Pending) == pending_pass)
                        .map(|a| {
                            let amount = match (kind, pending_pass) {
                                (_, true) => amount_of(&charges[&a.id]),
                                (ResourceKind::Cpu, false) => {
                                    usage_of(a).saturating_sub(kept_cpu(a))
                                }
                                (_, false) => judged(a),
                            };
                            VictimFacts {
                                id: a.id.clone(),
                                controllable: !facts(&a.id).uncontrollable,
                                frees: amount > 0,
                                pending: pending_pass,
                                mode: mode_of(a),
                                guaranteed: guaranteed(a),
                                progress_lost_ms: facts(&a.id).progress_lost_ms,
                                amount,
                            }
                        })
                        .collect();
                    victim_order(&mut victims, needed);
                    let mut remaining = needed;
                    for victim in victims {
                        planned.insert(victim.id.clone());
                        let allocation = input.allocations.iter().find(|a| a.id == victim.id);
                        let kept = allocation.map_or(0, kept_cpu);
                        // A running job above its own mode's target yields the
                        // difference even while the aggregate has room.
                        let limit = allocation.and_then(ceiling).filter(|_| !pending_pass);
                        let own_excess =
                            limit.map_or(0, |limit| (victim.amount + kept).saturating_sub(limit));
                        if own_excess > 0 {
                            note_excess(&key, own_excess);
                        }
                        let excess = remaining.max(own_excess);
                        let state = self.ladder.get(&(victim.id.clone(), key.clone()));
                        // A Guaranteed hint waits the Guaranteed hint deadline (FR-024).
                        let mut victim_timing = timing_for(victim.mode);
                        if victim.guaranteed && kind.is_memory() && !critical {
                            victim_timing.hint_deadline_ms = config
                                .contention
                                .guaranteed_hint_deadline_s
                                .saturating_mul(1000);
                        }
                        let mut unavailable = facts(&victim.id)
                            .unavailable
                            .get(&key)
                            .cloned()
                            .unwrap_or_default();
                        if pending_pass {
                            // Nothing runs yet: cancelling the reservation is the only step.
                            unavailable.extend([
                                YieldStep::LowerShare,
                                YieldStep::Cap,
                                YieldStep::Pause,
                                YieldStep::Hint,
                            ]);
                        }
                        let contended = excess > 0 && victim.frees;
                        let decision = plan_step(
                            state,
                            &LadderInput {
                                room: if kind.is_memory() {
                                    floor_room.get(&key).copied()
                                } else {
                                    None
                                },
                                kind,
                                guaranteed: victim.guaranteed,
                                // Running work is never drained for CPU alone (SC-002);
                                // pause frees it completely.
                                allow_drain: if victim.guaranteed {
                                    guaranteed_drain
                                } else {
                                    pending_pass || kind != ResourceKind::Cpu
                                },
                                unavailable,
                                excess: if contended { excess } else { 0 },
                                external,
                                measurement_known,
                                cause,
                                usage: (kind == ResourceKind::Cpu && !pending_pass)
                                    .then(|| victim.amount.saturating_add(kept)),
                                // What the allocation may use: while contended, a cap
                                // or hint target; while calm, its usage plus its share
                                // of the spare, toward which a held cap rises; never
                                // above its own mode's target.
                                target: Some(
                                    if contended {
                                        victim.amount.saturating_sub(excess)
                                    } else {
                                        victim.amount.saturating_add(spare_share)
                                    }
                                    .saturating_add(kept)
                                    .min(limit.unwrap_or(u64::MAX)),
                                ),
                            },
                            &victim_timing,
                            now_monotonic_ms,
                        );
                        if !decision.skipped.is_empty() {
                            note(
                                &mut statuses,
                                key.clone(),
                                "step_skipped_unavailable",
                                format!(
                                    "allocation {}: unavailable steps {:?} were skipped",
                                    victim.id, decision.skipped
                                ),
                            );
                        }
                        let before = state.map_or(YieldStep::None, |s| s.step);
                        let mut decision = decision;
                        // On the critical path, a worker that does not poll the drain
                        // file is sent TERM without the drain-file wait (FR-063).
                        if let Some(next) = decision.state.as_mut()
                            && critical
                            && next.step == YieldStep::Drain
                            && next.cause == YieldCause::FloorBreach
                            && !facts(&victim.id).observes_drain
                        {
                            next.step = YieldStep::Terminate;
                        }
                        // A Guaranteed RAM cap never goes below the request, as on
                        // CPU (FR-024).
                        if let Some(next) = decision.state.as_mut()
                            && kind == ResourceKind::Ram
                            && next.step == YieldStep::Cap
                            && let Some(allocation) = allocation.filter(|a| guaranteed(a))
                        {
                            next.target = next
                                .target
                                .map(|target| target.max(allocation.requested.ram_mib));
                        }
                        if let Some(next) = &decision.state
                            && next.step == YieldStep::Hint
                        {
                            hint_deadlines.insert(
                                (victim.id.clone(), key.clone()),
                                victim_timing.hint_deadline_ms,
                            );
                        }
                        if let Some(next) = &decision.state {
                            // A newly applied step is expected to release its share;
                            // the next observation shows whether it did.
                            if next.step > before {
                                let freed = match next.step {
                                    YieldStep::Cap => (victim.amount + kept).saturating_sub(
                                        next.target.unwrap_or(victim.amount + kept),
                                    ),
                                    YieldStep::Pause | YieldStep::Drain | YieldStep::Terminate => {
                                        victim.amount
                                    }
                                    _ => victim.amount.min(remaining),
                                };
                                remaining = remaining.saturating_sub(freed);
                            }
                            ladder.insert((victim.id.clone(), key.clone()), next.clone());
                        }
                    }
                }
                // Steps of allocations that are no longer candidates hold or reverse.
                for ((id, step_key), state) in &self.ladder {
                    if step_key != &key || planned.contains(id) {
                        continue;
                    }
                    let Some(allocation) = input.allocations.iter().find(|a| &a.id == id) else {
                        continue;
                    };
                    let decision = plan_step(
                        Some(state),
                        &LadderInput {
                            room: if kind.is_memory() {
                                floor_room.get(&key).copied()
                            } else {
                                None
                            },
                            kind,
                            guaranteed: allocation.class
                                == crate::execution_model::AllocationClass::Guaranteed,
                            allow_drain: false,
                            unavailable: facts(id)
                                .unavailable
                                .get(&key)
                                .cloned()
                                .unwrap_or_default(),
                            excess: 0,
                            external,
                            measurement_known: measurement_known
                                && !unknown_allocations.contains(id),
                            cause,
                            target: None,
                            usage: None,
                        },
                        &timing_for(mode_of(allocation)),
                        now_monotonic_ms,
                    );
                    if let Some(next) = decision.state {
                        ladder.insert((id.clone(), key.clone()), next);
                    }
                }
            }
            // GPU compute shares by pause duty cycle (FR-021, FR-022): own work on a
            // device shares it equally, and each allocation's cap target is its
            // share minus what the device has to free. Compute is reclaimed within
            // an interval, so it is never drained for GPU compute alone.
            for (uuid, budget) in &gpu_compute_budget {
                let key = ResourceKey::GpuCompute(uuid.clone());
                let kind = ResourceKind::GpuCompute;
                let running: Vec<&Allocation> = input
                    .allocations
                    .iter()
                    .filter(|a| {
                        a.phase == AllocationPhase::Running
                            && a.requested.gpu_memory_mib.contains_key(uuid)
                    })
                    .collect();
                if running.is_empty() {
                    continue;
                }
                // Activity of unknown amount first holds the steps, as any unknown
                // measurement does, and escalates after the unknown window (FR-008).
                let unknown = budget.own.is_none() && gpu_activity_unknown.contains(uuid);
                // Own work on a device shares what it holds equally: its measured
                // usage, else the whole device, but at most a duty its supervisor
                // reports applied, since the duty cycle is own actuation. Unknown
                // activity and unreported duties keep the whole device (FR-008).
                let assumed = GPU_COMPUTE_CAPACITY / running.len() as u64;
                // Credit the larger of the target the supervisor applied and the
                // current one, so a lowered target counts only once applied.
                let applied_duty = |allocation: &Allocation| -> u64 {
                    let state = self.ladder.get(&(allocation.id.clone(), key.clone()));
                    let facts = input.facts.get(&allocation.id);
                    let applied = facts.and_then(|facts| facts.applied.get(&key));
                    match (unknown, state, applied) {
                        (false, Some(state), Some(step)) if *step == state.step => {
                            match state.step {
                                YieldStep::Cap => {
                                    let current = state.target.unwrap_or(assumed);
                                    let ran = facts
                                        .and_then(|facts| facts.applied_targets.get(&key))
                                        .copied()
                                        .unwrap_or(current);
                                    assumed.min(ran.max(current))
                                }
                                YieldStep::Pause => 0,
                                _ => assumed,
                            }
                        }
                        _ => assumed,
                    }
                };
                let duties: Vec<u64> = running.iter().map(|a| applied_duty(a)).collect();
                let held = budget.own.unwrap_or_else(|| duties.iter().sum());
                if budget.own.is_none() {
                    statuses.entry(key.clone()).or_default().own = Some((held, held));
                    if duties.iter().any(|duty| *duty < assumed) {
                        note(
                            &mut statuses,
                            key.clone(),
                            "applied_duty_credit",
                            format!("own work is credited at the applied duty, {held} in total"),
                        );
                    }
                }
                let share = held / running.len() as u64;
                let excess = held.saturating_sub(budget.share);
                if excess > 0 {
                    note_excess(&key, excess);
                }
                // While calm, a held cap rises toward its usage plus its part of
                // what the mode allows beyond own usage.
                let spare_share = budget.share.saturating_sub(held) / running.len() as u64;
                let external = budget.external;
                if unknown {
                    let since = self
                        .unknown_since
                        .get(&key)
                        .copied()
                        .unwrap_or(now_monotonic_ms);
                    unknown_since.insert(key.clone(), since);
                }
                let measurement_known = !unknown
                    || unknown_since.get(&key).is_some_and(|since| {
                        now_monotonic_ms.saturating_sub(*since) >= window(kind)
                    });
                let cause = if unknown {
                    YieldCause::UnknownMeasurement
                } else {
                    YieldCause::ExternalGrowth
                };
                // Each job yields and rises at its own mode's pace.
                let timing_for = |mode: ContentionMode| {
                    LadderTiming::new(&config.contention.thresholds(kind), mode, interval_ms)
                };
                let mode_of = |a: &Allocation| allocation_modes[&a.id][&kind];
                // No backend lowers a GPU compute share, and a worker's resource
                // targets carry no GPU compute amount.
                let unavailable_of = |id: &str| {
                    let mut steps = input
                        .facts
                        .get(id)
                        .and_then(|f| f.unavailable.get(&key))
                        .cloned()
                        .unwrap_or_default();
                    steps.extend([YieldStep::LowerShare, YieldStep::Hint]);
                    steps
                };
                let guaranteed =
                    |a: &Allocation| a.class == crate::execution_model::AllocationClass::Guaranteed;
                let mut victims: Vec<VictimFacts> = running
                    .iter()
                    .filter(|a| {
                        allocation_unknown_since.get(&a.id).is_none_or(|start| {
                            now_monotonic_ms.saturating_sub(*start) >= window(kind)
                        })
                    })
                    .map(|a| VictimFacts {
                        id: a.id.clone(),
                        controllable: !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable),
                        frees: true,
                        pending: false,
                        mode: mode_of(a),
                        guaranteed: guaranteed(a),
                        progress_lost_ms: input.facts.get(&a.id).map_or(0, |f| f.progress_lost_ms),
                        amount: share,
                    })
                    .collect();
                victim_order(&mut victims, excess);
                let mut remaining = excess;
                let mut planned = BTreeSet::new();
                for victim in victims {
                    planned.insert(victim.id.clone());
                    let state = self.ladder.get(&(victim.id.clone(), key.clone()));
                    // A job above its own mode's share yields the difference even
                    // while the aggregate has room.
                    let limit = budget.targets.get(&victim.mode).copied();
                    let own_excess = limit.map_or(0, |limit| share.saturating_sub(limit));
                    if own_excess > 0 {
                        note_excess(&key, own_excess);
                    }
                    let excess = remaining.max(own_excess);
                    let contended_victim = excess > 0;
                    let decision = plan_step(
                        state,
                        &LadderInput {
                            room: None,
                            kind,
                            guaranteed: victim.guaranteed,
                            allow_drain: false,
                            unavailable: unavailable_of(&victim.id),
                            excess: if contended_victim { excess } else { 0 },
                            external,
                            measurement_known,
                            cause,
                            usage: budget.own.map(|_| share),
                            target: Some(
                                if contended_victim {
                                    share.saturating_sub(excess)
                                } else {
                                    share.saturating_add(spare_share)
                                }
                                .min(limit.unwrap_or(u64::MAX)),
                            ),
                        },
                        &timing_for(victim.mode),
                        now_monotonic_ms,
                    );
                    if !decision.skipped.is_empty() {
                        note(
                            &mut statuses,
                            key.clone(),
                            "step_skipped_unavailable",
                            format!(
                                "allocation {}: unavailable steps {:?} were skipped",
                                victim.id, decision.skipped
                            ),
                        );
                    }
                    let before = state.map_or(YieldStep::None, |s| s.step);
                    if let Some(next) = decision.state {
                        if next.step > before {
                            let freed = match next.step {
                                YieldStep::Cap => {
                                    share.saturating_sub(next.target.unwrap_or(share))
                                }
                                YieldStep::Pause => share,
                                _ => 0,
                            };
                            remaining = remaining.saturating_sub(freed);
                        }
                        ladder.insert((victim.id.clone(), key.clone()), next);
                    }
                }
                for ((id, step_key), state) in &self.ladder {
                    if step_key != &key || planned.contains(id) {
                        continue;
                    }
                    let Some(allocation) = input.allocations.iter().find(|a| &a.id == id) else {
                        continue;
                    };
                    let decision = plan_step(
                        Some(state),
                        &LadderInput {
                            room: None,
                            kind,
                            guaranteed: guaranteed(allocation),
                            allow_drain: false,
                            unavailable: unavailable_of(id),
                            excess: 0,
                            external,
                            measurement_known: measurement_known
                                && !unknown_allocations.contains(id),
                            cause,
                            target: None,
                            usage: None,
                        },
                        &timing_for(mode_of(allocation)),
                        now_monotonic_ms,
                    );
                    if let Some(next) = decision.state {
                        ladder.insert((id.clone(), key.clone()), next);
                    }
                }
            }
            // A step that went down, or a cap or hint whose target rose, says
            // why: its cause no longer holds as it did.
            for ((id, key), before) in &self.ladder {
                let next = ladder.get(&(id.clone(), key.clone()));
                let after = next.map(|s| s.step);
                let raised = next.is_some_and(|next| {
                    next.step == before.step
                        && matches!(next.step, YieldStep::Cap | YieldStep::Hint)
                        && next.target > before.target
                });
                if before.step == YieldStep::None
                    || (after.is_some_and(|step| step >= before.step) && !raised)
                    || !input.allocations.iter().any(|a| &a.id == id)
                {
                    continue;
                }
                let code = match before.cause {
                    YieldCause::ExternalGrowth => "step_reversed_contention_ended",
                    YieldCause::FloorBreach => "step_reversed_floor_restored",
                    YieldCause::UnknownMeasurement => "step_reversed_measurement_known",
                    YieldCause::EnvelopeBreach => "step_reversed_envelope_restored",
                    YieldCause::OperatorDrain
                    | YieldCause::CoordinatorDrain
                    | YieldCause::LeaseExpired => "step_reversed_drain_lifted",
                };
                note(
                    &mut statuses,
                    key.clone(),
                    code,
                    match (raised, next.and_then(|next| next.target)) {
                        (true, Some(target)) => format!(
                            "allocation {id}: {} target raised from {} to {target}",
                            step_name(before.step),
                            before.target.unwrap_or(0)
                        ),
                        _ => format!(
                            "allocation {id}: {} relaxed to {}",
                            step_name(before.step),
                            step_name(after.unwrap_or(YieldStep::None))
                        ),
                    },
                );
            }
            for ((id, key), state) in &ladder {
                if state.step == YieldStep::None {
                    continue;
                }
                actions.push(YieldAction {
                    allocation_id: id.clone(),
                    resource: key.clone(),
                    step: state.step,
                    target: state.target,
                    cause: state.cause,
                    code: format!("yield_step_{}", step_name(state.step)),
                    deadline_ms: hint_deadlines.get(&(id.clone(), key.clone())).copied(),
                });
            }
        }
        would_drain.extend(
            actions
                .iter()
                .filter(|action| action.step >= YieldStep::Drain)
                .map(|action| action.allocation_id.clone()),
        );
        let mut seen = BTreeSet::new();
        would_drain.retain(|id| seen.insert(id.clone()));
        if charged.cpu_millicores > budget.cpu_millicores {
            reasons.push(ReasonCode::new(
                "cpu_charges_exceed_budget",
                "managed CPU charges exceed scheduling budget",
            ));
        }
        if charged.ram_mib > budget.ram_mib {
            reasons.push(ReasonCode::new(
                "ram_charges_exceed_budget",
                "managed RAM charges exceed scheduling budget",
            ));
        }
        let legacy_expansion_allowed = !unsafe_now && cooled_down && would_drain.is_empty();
        // A reclaim episode on a memory resource starts when a floor breach or own
        // usage beyond the budget appears there; recent episodes lengthen the memory
        // cooldown by exponential backoff, and a quiet peak window resets it (FR-061).
        let reclaiming = memory_reclaim;
        let mut reclaim_episodes: BTreeMap<ResourceKey, Vec<u64>> = BTreeMap::new();
        for key in self.reclaim_episodes.keys().chain(reclaiming.iter()) {
            let window_ms = config
                .contention
                .thresholds(key.kind())
                .memory_peak_window_s
                .saturating_mul(1000);
            let mut starts: Vec<u64> = self
                .reclaim_episodes
                .get(key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|start| now_monotonic_ms.saturating_sub(*start) < window_ms)
                .collect();
            if reclaiming.contains(key) && !self.reclaiming.contains(key) {
                starts.push(now_monotonic_ms);
            }
            if !starts.is_empty() {
                reclaim_episodes.insert(key.clone(), starts);
            }
        }
        let backoff_ms = |key: &ResourceKey| {
            let episodes = reclaim_episodes.get(key).map_or(0, Vec::len) as u32;
            let thresholds = config.contention.thresholds(key.kind());
            if episodes == 0 {
                0
            } else {
                thresholds
                    .reclaim_backoff_ms
                    .saturating_mul(1u64.checked_shl(episodes - 1).unwrap_or(u64::MAX))
                    .min(thresholds.reclaim_backoff_max_ms)
            }
        };
        // Moves within the noise tolerance (memory), or within the reversal
        // hysteresis of the external growth a compute step answered, are jitter:
        // they reset no cooldown and block no admission.
        let tolerance = |key: &ResourceKey| -> u64 {
            let thresholds = config.contention.thresholds(key.kind());
            if key.kind().is_memory() {
                return thresholds.noise_tolerance_mib;
            }
            ladder
                .iter()
                .filter(|((_, k), state)| k == key && state.cause == YieldCause::ExternalGrowth)
                .map(|(_, state)| {
                    state
                        .trigger_external
                        .saturating_mul(u64::from(thresholds.hysteresis_percent))
                        / 100
                })
                .max()
                .unwrap_or(0)
        };
        let moving_down: BTreeSet<ResourceKey> = ladder
            .iter()
            .filter(|((id, key), next)| {
                let before = self.ladder.get(&(id.clone(), key.clone()));
                let step = before.map_or(YieldStep::None, |s| s.step);
                let fell = match (next.target, before.and_then(|s| s.target)) {
                    (Some(next), Some(before)) => before.saturating_sub(next) > tolerance(key),
                    (None, Some(_)) => true,
                    _ => false,
                };
                next.step > step
                    || (next.step == step
                        && matches!(step, YieldStep::Cap | YieldStep::Hint)
                        && fell)
            })
            .map(|((_, key), _)| key.clone())
            .collect();
        // Each resource waits for its own cooldown, never the GPU one (M1, FR-034).
        // A CPU cap that holds may sit within the tolerance above a jittering budget.
        let cpu_safe = !input.explicit_drain
            && !cpu_unknown
            && !cpu_held
            && cpu_capacity > 0
            && (charged.cpu_millicores <= budget.cpu_millicores
                || (!moving_down.contains(&ResourceKey::Cpu)
                    && charged.cpu_millicores
                        <= budget
                            .cpu_millicores
                            .saturating_add(tolerance(&ResourceKey::Cpu))));
        let ram_safe = !input.explicit_drain
            && !ram_unknown
            && !ram_held
            && snapshot.total_ram_mib > 0
            && charged.ram_mib <= budget.ram_mib;
        let cpu_stable_since =
            cpu_safe.then_some(self.cpu_stable_since.unwrap_or(now_monotonic_ms));
        let ram_stable_since =
            ram_safe.then_some(self.ram_stable_since.unwrap_or(now_monotonic_ms));
        let cooled = |since: Option<u64>, kind: ResourceKind, extra_ms: u64| {
            since.is_some_and(|since| {
                now_monotonic_ms - since
                    >= config
                        .contention
                        .thresholds(kind)
                        .cooldown_ms(governing(kind))
                        .saturating_add(extra_ms)
            })
        };
        let cpu_allowed = cooled(cpu_stable_since, ResourceKind::Cpu, 0);
        let ram_allowed = cooled(
            ram_stable_since,
            ResourceKind::Ram,
            backoff_ms(&ResourceKey::Ram),
        );
        // The 0.2 report has one flag for both; every launch reserves CPU and RAM.
        let cpu_ram_expansion_allowed = cpu_allowed && ram_allowed;
        // Memory admission uses the external peak over its window; yielding uses
        // current usage (FR-060).
        // Each listed external process keeps its own peak while it lives and loses
        // it when it exits; the rest of external usage keeps one peak (FR-060).
        let mut external_peaks = BTreeMap::new();
        let mut process_peaks = BTreeMap::new();
        let mut peak_excess = BTreeMap::new();
        let listed = snapshot.external_processes.as_deref().unwrap_or_default();
        for (key, status) in &statuses {
            let (true, Some(external)) = (key.kind().is_memory(), status.external) else {
                continue;
            };
            let window_ms = config
                .contention
                .thresholds(key.kind())
                .memory_peak_window_s
                .saturating_mul(1000);
            let usage_of = |process: &crate::model::ExternalProcessMemory| match key {
                ResourceKey::Ram => process.ram_mib,
                ResourceKey::GpuMemory(uuid) => {
                    process.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                }
                _ => 0,
            };
            let mut listed_total = 0u64;
            let mut peak = 0u64;
            for process in listed {
                let id = (key.clone(), process.pid, process.start_time);
                let usage = usage_of(process);
                if usage == 0 && !self.process_peaks.contains_key(&id) {
                    continue;
                }
                listed_total = listed_total.saturating_add(usage);
                let mut samples = self.process_peaks.get(&id).cloned().unwrap_or_default();
                peak = peak.saturating_add(window_peak(
                    &mut samples,
                    now_monotonic_ms,
                    usage,
                    window_ms,
                ));
                process_peaks.insert(id, samples);
            }
            let mut samples = self.external_peaks.get(key).cloned().unwrap_or_default();
            peak = peak.saturating_add(window_peak(
                &mut samples,
                now_monotonic_ms,
                external.saturating_sub(listed_total),
                window_ms,
            ));
            peak_excess.insert(key.clone(), peak.saturating_sub(external));
            external_peaks.insert(key.clone(), samples);
        }
        let mut headroom = budget.clone();
        // Recommendations are not release confirmation: subtract ALL allocations,
        // including existing draining work and everything just recommended for drain.
        headroom.subtract_saturating(&charged);
        headroom.gpu_memory_mib.retain(|uuid, _| {
            !input.explicit_drain
                && !blocked_gpus.contains(uuid)
                && next_gpus
                    .get(uuid)
                    .and_then(|p| p.stable_since)
                    .is_some_and(|since| {
                        now_monotonic_ms - since
                            >= config
                                .gpu_memory_cooldown_ms(governing(ResourceKind::GpuMemory))
                                .saturating_add(backoff_ms(&ResourceKey::GpuMemory(uuid.clone())))
                    })
        });
        headroom.ram_mib = headroom
            .ram_mib
            .saturating_sub(peak_excess.get(&ResourceKey::Ram).copied().unwrap_or(0));
        for (uuid, amount) in headroom.gpu_memory_mib.iter_mut() {
            let key = ResourceKey::GpuMemory(uuid.clone());
            *amount = amount.saturating_sub(peak_excess.get(&key).copied().unwrap_or(0));
        }
        // A breached floor blocks admission on that resource in every policy (FR-041).
        if breached.contains(&ResourceKey::Ram) {
            headroom.ram_mib = 0;
        }
        // An exhausted spool blocks admission only; nothing drains for it (FR-058).
        let cpu_ram_expansion_allowed = cpu_ram_expansion_allowed && !input.spool_full;
        if input.spool_full {
            headroom = Resources::default();
            reasons.push(ReasonCode::new(
                "admission_spool_exhausted",
                "node spool budget exhausted; admission is blocked and no allocation is drained",
            ));
        }
        let expansion_allowed = !input.spool_full
            && (legacy_expansion_allowed
                || (cpu_ram_expansion_allowed
                    && headroom.gpu_memory_mib.values().any(|amount| *amount > 0)));
        // CPU and RAM headroom open only after their own cooldowns.
        if !cpu_allowed {
            headroom.cpu_millicores = 0;
        }
        if !ram_allowed {
            headroom.ram_mib = 0;
        }
        if !expansion_allowed {
            if cpu_ram_expansion_allowed {
                reasons.push(ReasonCode::new("cpu_only_admission", "independently stable CPU/RAM headroom permits CPU-only admission; GPU admission remains blocked"));
            }
            if !unsafe_now && !cooled_down {
                // The catalog sentence says this covers only the 0.2 expansion flags.
                reasons.push(ReasonCode::new(
                    "cooldown_pending",
                    crate::contention::reason_text("cooldown_pending")
                        .unwrap_or("cooldown_pending"),
                ));
            }
        }
        // What each resource holds back and why (SC-016).
        let shortfall = |pick: &dyn Fn(&Resources) -> u64, ready_only: bool| -> u64 {
            input
                .allocations
                .iter()
                .filter(|a| a.phase == AllocationPhase::Running)
                .filter(|a| !ready_only || input.facts.get(&a.id).is_some_and(|f| f.hint_ready))
                .filter_map(|a| {
                    a.observed
                        .as_ref()
                        .map(|o| pick(&charges[&a.id]).saturating_sub(pick(o)))
                })
                .fold(0u64, u64::saturating_add)
        };
        let withheld = |pre: u64, cooled_down: bool, backed_off: bool| -> (u64, u64) {
            if !cooled_down {
                (pre, 0)
            } else if !backed_off {
                (0, pre)
            } else {
                (0, 0)
            }
        };
        let external_of =
            |key: &ResourceKey| statuses.get(key).and_then(|s| s.external).unwrap_or(0);
        let mut held_back: BTreeMap<ResourceKey, HeldBack> = BTreeMap::new();
        {
            let available = cpu_capacity.saturating_sub(external_of(&ResourceKey::Cpu));
            let mut held = HeldBack::default();
            if cpu_unknown || cpu_held {
                held.unknown = available;
            } else {
                let thresholds = config.contention.thresholds(ResourceKind::Cpu);
                let percent = match governing(ResourceKind::Cpu) {
                    crate::contention::ContentionMode::YieldFirst => {
                        thresholds.yield_first_margin_percent
                    }
                    crate::contention::ContentionMode::Neutral => thresholds.neutral_margin_percent,
                    crate::contention::ContentionMode::Performance => {
                        thresholds.performance_margin_percent
                    }
                };
                let margin = cpu_capacity.saturating_mul(u64::from(percent)) / 100;
                let target = available.saturating_sub(budget.cpu_millicores);
                held.reserve = if cpu_reserve > margin {
                    cpu_reserve.min(target)
                } else {
                    0
                };
                held.margin = target.saturating_sub(held.reserve).min(margin);
                held.contention = target.saturating_sub(held.reserve + held.margin);
                if psi_only {
                    held.contention_psi = std::mem::take(&mut held.contention);
                }
                let pre = budget.cpu_millicores.saturating_sub(charged.cpu_millicores);
                held.cooldown = if cpu_allowed { 0 } else { pre };
                held.charge_shortfall = shortfall(&|r| r.cpu_millicores, false);
            }
            held_back.insert(ResourceKey::Cpu, held);
        }
        {
            let available = snapshot
                .total_ram_mib
                .saturating_sub(external_of(&ResourceKey::Ram));
            let mut held = HeldBack {
                floor: ram_reserve,
                ..HeldBack::default()
            };
            if ram_unknown || ram_held {
                held.unknown = available.saturating_sub(ram_reserve);
            } else {
                held.margin = available
                    .saturating_sub(ram_reserve)
                    .saturating_sub(budget.ram_mib);
                held.peak = peak_excess.get(&ResourceKey::Ram).copied().unwrap_or(0);
                let pre = budget
                    .ram_mib
                    .saturating_sub(charged.ram_mib)
                    .saturating_sub(held.peak);
                (held.cooldown, held.backoff) = withheld(
                    pre,
                    cooled(ram_stable_since, ResourceKind::Ram, 0),
                    ram_allowed,
                );
                held.charge_shortfall = shortfall(&|r| r.ram_mib, false);
            }
            held_back.insert(ResourceKey::Ram, held);
        }
        for gpu in &snapshot.gpus {
            let key = ResourceKey::GpuMemory(gpu.uuid.clone());
            let floor = config.vram_floor_mib(gpu.total_memory_mib);
            let available = gpu
                .total_memory_mib
                .saturating_sub(floor)
                .saturating_sub(external_of(&key));
            let allowance = budget.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0);
            let mut held = HeldBack {
                floor,
                noise_tolerance: config
                    .contention
                    .thresholds(ResourceKind::GpuMemory)
                    .noise_tolerance_mib
                    .saturating_add(gpu.unattributed_memory_mib.unwrap_or(0)),
                ..HeldBack::default()
            };
            if statuses
                .get(&key)
                .is_some_and(|s| s.measurement == Some(Measurement::Unknown))
            {
                held.unknown = available;
            } else {
                held.margin = available.saturating_sub(allowance);
                held.peak = peak_excess.get(&key).copied().unwrap_or(0);
                let pre = allowance
                    .saturating_sub(charged.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0))
                    .saturating_sub(held.peak);
                let since = next_gpus.get(&gpu.uuid).and_then(|p| p.stable_since);
                let elapsed = since.map(|since| now_monotonic_ms - since);
                let cooldown = config.gpu_memory_cooldown_ms(governing(ResourceKind::GpuMemory));
                (held.cooldown, held.backoff) = withheld(
                    pre,
                    elapsed.is_some_and(|ms| ms >= cooldown),
                    elapsed.is_some_and(|ms| ms >= cooldown.saturating_add(backoff_ms(&key))),
                );
                let uuid = gpu.uuid.clone();
                held.charge_shortfall = shortfall(
                    &move |r: &Resources| r.gpu_memory_mib.get(&uuid).copied().unwrap_or(0),
                    true,
                );
            }
            held_back.insert(key, held);
        }
        // GPU compute: what own work leaves beside external compute, and whether
        // its amount was measured or why not (FR-021).
        for gpu in &snapshot.gpus {
            let mut held = HeldBack {
                compute_path: compute_paths.get(&gpu.uuid).cloned(),
                ..HeldBack::default()
            };
            match gpu_compute_budget.get(&gpu.uuid) {
                Some(budget) => {
                    let left = GPU_COMPUTE_CAPACITY.saturating_sub(budget.share);
                    match (budget.own, budget.contended) {
                        (Some(_), true) => held.contention = left.saturating_sub(budget.external),
                        (Some(_), false) => held.margin = left.saturating_sub(budget.external),
                        (None, true) => held.unknown = left,
                        (None, false) => {}
                    }
                }
                None => held.unknown = GPU_COMPUTE_CAPACITY,
            }
            held_back.insert(ResourceKey::GpuCompute(gpu.uuid.clone()), held);
        }
        for (key, held) in held_back {
            let capacity = match &key {
                ResourceKey::Cpu => cpu_capacity,
                ResourceKey::Ram => snapshot.total_ram_mib,
                ResourceKey::GpuMemory(uuid) => snapshot
                    .gpus
                    .iter()
                    .find(|gpu| &gpu.uuid == uuid)
                    .map_or(0, |gpu| gpu.total_memory_mib),
                ResourceKey::GpuCompute(_) => GPU_COMPUTE_CAPACITY,
            };
            statuses.entry(key).or_default().held_back = Some(held.with_capacity(capacity));
        }
        // What the supervisors report about how their steps hold, as catalog codes.
        for allocation in &input.allocations {
            let Some(facts) = input.facts.get(&allocation.id) else {
                continue;
            };
            for (key, code) in &facts.step_codes {
                if let Some(text) = crate::contention::reason_text(code) {
                    note(
                        &mut statuses,
                        key.clone(),
                        code,
                        format!("allocation {}: {text}", allocation.id),
                    );
                }
            }
        }
        // Admission per resource: blocked until its cause changes, held while it
        // waits for a measurement or a cooldown (FR-028, FR-038).
        // Own work is reclaimed while it holds more than its target, or a step is
        // still moving down, by more than the tolerance; a cap or hint that holds
        // at its target is not.
        let stepped = |key: &ResourceKey| {
            over_target
                .get(key)
                .is_some_and(|excess| *excess > tolerance(key))
                || moving_down.contains(key)
        };
        let blocked = Admission::blocked;
        let held = Admission::held;
        let admission = |key: &ResourceKey,
                         unknown: bool,
                         gpu_reason: Option<&String>,
                         cooled_down: bool,
                         backed_off: bool| {
            if input.explicit_drain {
                blocked("admission_draining")
            } else if input.spool_full {
                blocked("admission_spool_exhausted")
            } else if breached.contains(key) {
                blocked("admission_floor_breached")
            } else if unknown {
                held("admission_measurement_unknown")
            } else if let Some(code) = gpu_reason {
                held(code)
            } else if stepped(key) {
                blocked("admission_own_work_yielding")
            } else if !cooled_down {
                held("admission_cooling_down")
            } else if !backed_off {
                held("admission_reclaim_backoff")
            } else {
                Admission::Open
            }
        };
        let mut admissions = vec![
            (
                ResourceKey::Cpu,
                admission(
                    &ResourceKey::Cpu,
                    cpu_unknown || cpu_held,
                    None,
                    cpu_allowed,
                    true,
                ),
            ),
            (
                ResourceKey::Ram,
                admission(
                    &ResourceKey::Ram,
                    ram_unknown || ram_held,
                    None,
                    cooled(ram_stable_since, ResourceKind::Ram, 0),
                    ram_allowed,
                ),
            ),
        ];
        for gpu in &snapshot.gpus {
            let key = ResourceKey::GpuMemory(gpu.uuid.clone());
            let elapsed = next_gpus
                .get(&gpu.uuid)
                .and_then(|p| p.stable_since)
                .map(|since| now_monotonic_ms - since);
            let cooldown = config.gpu_memory_cooldown_ms(governing(ResourceKind::GpuMemory));
            let state = admission(
                &key,
                false,
                gpu_block_reasons.get(&gpu.uuid),
                elapsed.is_some_and(|ms| ms >= cooldown),
                elapsed.is_some_and(|ms| ms >= cooldown.saturating_add(backoff_ms(&key))),
            );
            admissions.push((key, state));
        }
        for gpu in &snapshot.gpus {
            let key = ResourceKey::GpuCompute(gpu.uuid.clone());
            if stepped(&key) {
                admissions.push((key, blocked("admission_own_work_yielding")));
            }
        }
        for (key, state) in admissions {
            statuses.entry(key).or_default().admission = Some(state);
        }
        // Admission is open but own charges fill own target: requests wait.
        if headroom.cpu_millicores == 0
            && budget.cpu_millicores > 0
            && statuses
                .get(&ResourceKey::Cpu)
                .and_then(|s| s.admission.as_ref())
                == Some(&Admission::Open)
        {
            note(
                &mut statuses,
                ResourceKey::Cpu,
                "admission_over_own_target",
                format!(
                    "own charges fill the CPU target of {}m, so a request waits",
                    budget.cpu_millicores
                ),
            );
        }
        // The allowance (FR-071): what each running allocation may grow to now
        // without causing a step. Its usage plus an equal part of the room under
        // the mode's allowance after pending reservations, less one publish
        // threshold in total, which covers the falls left unpublished.
        let mut allowances: BTreeMap<String, ResourceAmounts> = BTreeMap::new();
        let mut allowance_published = BTreeMap::new();
        {
            let mut keys = vec![
                (ResourceKey::Cpu, cpu_unknown || cpu_held, cpu_capacity),
                (
                    ResourceKey::Ram,
                    ram_unknown || ram_held,
                    snapshot.total_ram_mib,
                ),
            ];
            for gpu in &snapshot.gpus {
                keys.push((
                    ResourceKey::GpuMemory(gpu.uuid.clone()),
                    false,
                    gpu.total_memory_mib,
                ));
            }
            for (key, unknown, capacity) in keys {
                let kind = key.kind();
                let unknown = unknown
                    || statuses
                        .get(&key)
                        .is_some_and(|s| s.measurement == Some(Measurement::Unknown));
                let thresholds = config.contention.thresholds(kind);
                let pick = |r: &Resources| match &key {
                    ResourceKey::Cpu => r.cpu_millicores,
                    ResourceKey::Ram => r.ram_mib,
                    ResourceKey::GpuCompute(uuid) | ResourceKey::GpuMemory(uuid) => {
                        r.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                    }
                };
                let mode_allowance = pick(&budget);
                let on_resource = |a: &Allocation| match &key {
                    ResourceKey::GpuMemory(uuid) => a.requested.gpu_memory_mib.contains_key(uuid),
                    _ => true,
                };
                let guaranteed =
                    |a: &Allocation| a.class == crate::execution_model::AllocationClass::Guaranteed;
                // What an allocation holds as yielding judges it; Guaranteed work
                // holds at least its request.
                let held = |a: &Allocation| -> u64 {
                    let usage = match (&key, &a.observed) {
                        (ResourceKey::Cpu, Some(observed)) => observed.cpu_millicores,
                        (ResourceKey::Ram, Some(observed)) => observed.ram_mib,
                        (ResourceKey::GpuMemory(uuid), Some(observed))
                            if input.facts.get(&a.id).is_some_and(|f| f.hint_ready) =>
                        {
                            observed.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                        }
                        _ => pick(&charges[&a.id]),
                    };
                    if guaranteed(a) {
                        usage.max(pick(&a.requested))
                    } else {
                        usage
                    }
                };
                let running: Vec<&Allocation> = input
                    .allocations
                    .iter()
                    .filter(|a| a.phase == AllocationPhase::Running && on_resource(a))
                    .collect();
                if running.is_empty() {
                    continue;
                }
                let taken = input
                    .allocations
                    .iter()
                    .filter(|a| on_resource(a))
                    .map(|a| match a.phase {
                        AllocationPhase::Pending => pick(&a.requested),
                        _ => held(a),
                    })
                    .fold(0u64, u64::saturating_add);
                // A change publishes beyond the contention floor (CPU) or the noise
                // tolerance (memory); a rise moves at most one step, the mode's
                // margin of capacity, per scale-up interval.
                let threshold = if kind.is_memory() {
                    thresholds.noise_tolerance_mib
                } else {
                    (capacity.saturating_mul(u64::from(thresholds.principal_min_percent)) / 100)
                        .max(1)
                };
                // Each job rises at its own mode's pace.
                let step_of = |mode: ContentionMode| {
                    let percent = if kind.is_memory() {
                        mode.memory_allowance_step_percent()
                    } else {
                        match mode {
                            ContentionMode::YieldFirst => thresholds.yield_first_margin_percent,
                            ContentionMode::Neutral => thresholds.neutral_margin_percent,
                            ContentionMode::Performance => thresholds.performance_margin_percent,
                        }
                    };
                    (capacity.saturating_mul(u64::from(percent)) / 100).max(threshold + 1)
                };
                let mode_of = |a: &Allocation| allocation_modes[&a.id][&kind];
                // The room is shared equally, but no job is invited above its own
                // mode's target; what that bound leaves goes to the others.
                let room = mode_allowance
                    .saturating_sub(taken)
                    .saturating_sub(threshold);
                let headroom = |a: &Allocation| {
                    mode_targets.get(&key).map_or(u64::MAX, |targets| {
                        targets[&mode_of(a)].saturating_sub(held(a))
                    })
                };
                let mut shares: BTreeMap<&str, u64> = BTreeMap::new();
                loop {
                    let open: Vec<&&Allocation> = running
                        .iter()
                        .filter(|a| !shares.contains_key(a.id.as_str()))
                        .collect();
                    if open.is_empty() {
                        break;
                    }
                    let left = room.saturating_sub(shares.values().sum());
                    let equal = left / open.len() as u64;
                    let bound: Vec<&&&Allocation> =
                        open.iter().filter(|a| headroom(a) < equal).collect();
                    if bound.is_empty() {
                        for a in open {
                            shares.insert(&a.id, equal);
                        }
                        break;
                    }
                    for a in bound {
                        shares.insert(&a.id, headroom(a));
                    }
                }
                let mut current = Vec::with_capacity(running.len());
                for allocation in &running {
                    let allocation = *allocation;
                    // An unknown measurement invites no growth.
                    let mut value = if unknown {
                        held(allocation)
                    } else {
                        held(allocation).saturating_add(shares[allocation.id.as_str()])
                    };
                    match ladder.get(&(allocation.id.clone(), key.clone())) {
                        Some(state) if matches!(state.step, YieldStep::Cap | YieldStep::Hint) => {
                            value = value.min(state.target.unwrap_or(value));
                        }
                        Some(state) if state.step > YieldStep::None => {
                            value = value.min(held(allocation));
                        }
                        _ => {}
                    }
                    if let Some(envelope) = &input.envelope {
                        value = value.min(pick(envelope));
                    }
                    if guaranteed(allocation) {
                        value = value.max(pick(&allocation.requested));
                    }
                    current.push((allocation.id.clone(), value, mode_of(allocation)));
                }
                // Falls publish together once their summed drift passes the one
                // threshold; each rise is rate-limited on its own.
                let drift = current
                    .iter()
                    .filter_map(|(id, value, _)| {
                        self.allowance_published
                            .get(&(id.clone(), key.clone()))
                            .map(|(previous, _)| previous.saturating_sub(*value))
                    })
                    .fold(0u64, u64::saturating_add);
                for (allocation_id, value, mode) in current {
                    let (step, scale_up_ms) = (
                        step_of(mode),
                        thresholds.scale_up_interval_s(mode).saturating_mul(1000),
                    );
                    let id = (allocation_id.clone(), key.clone());
                    let published = match self.allowance_published.get(&id).copied() {
                        None => (value, now_monotonic_ms),
                        Some((previous, _)) if value < previous && drift > threshold => {
                            (value, now_monotonic_ms)
                        }
                        Some((previous, rose_at))
                            if value > previous.saturating_add(threshold)
                                && now_monotonic_ms.saturating_sub(rose_at) >= scale_up_ms =>
                        {
                            (previous + (value - previous).min(step), now_monotonic_ms)
                        }
                        Some(kept) => kept,
                    };
                    allowance_published.insert(id, published);
                    let amounts = allowances.entry(allocation_id).or_default();
                    match &key {
                        ResourceKey::Cpu => amounts.cpu_millicores = Some(published.0),
                        ResourceKey::Ram => amounts.ram_mib = Some(published.0),
                        ResourceKey::GpuMemory(uuid) => {
                            amounts.gpu_memory_mib.insert(uuid.clone(), published.0);
                        }
                        ResourceKey::GpuCompute(_) => {}
                    }
                }
            }
        }
        self.allowance_published = allowance_published;
        self.last_now = Some(now_monotonic_ms);
        self.node_id = Some(snapshot.node_id.clone());
        self.stable_since = stable_since;
        self.cpu_stable_since = cpu_stable_since;
        self.ram_stable_since = ram_stable_since;
        self.principal_counts = principal_counts;
        self.external_peaks = external_peaks;
        self.process_peaks = process_peaks;
        self.reclaim_episodes = reclaim_episodes;
        self.reclaiming = reclaiming;
        self.satisfied = satisfied;
        self.probes = probes;
        self.principal_seen = principal_seen;
        self.cpu_usage = cpu_usage;
        self.previous_gpus = next_gpus;
        self.last_kernel_cpu_sample = next_kernel_cpu_sample;
        self.last_observed = last_observed;
        self.ladder = ladder;
        self.unknown_since = unknown_since;
        self.allocation_unknown_since = allocation_unknown_since;
        self.floor_breach_since = floor_breach_since;
        for kind in [ResourceKind::Cpu, ResourceKind::Ram] {
            let key = if kind == ResourceKind::Cpu {
                ResourceKey::Cpu
            } else {
                ResourceKey::Ram
            };
            statuses.entry(key).or_default();
        }
        for (key, status) in &mut statuses {
            status.mode = Some(config.contention.mode(key.kind()));
            status.effective_mode = Some(governing(key.kind()));
            status.measurement.get_or_insert(Measurement::Known);
        }
        let decision = Decision {
            schema_version: SCHEMA_VERSION,
            node_id: snapshot.node_id.clone(),
            observe_only: true,
            managed_budget: budget,
            admission_headroom: headroom,
            expansion_allowed,
            cpu_ram_expansion_allowed,
            would_drain,
            reasons: reasons.iter().map(|reason| reason.text.clone()).collect(),
            reason_codes: reasons,
            resources: statuses,
            actions,
            exceptions: config.caution_options_on(snapshot),
            allocation_modes,
            max_mode: Some(cap),
            allowances,
            cpu_admission_credit_millicores: cpu_credit
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
        Ok(decision)
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

fn mode_name(mode: crate::contention::ContentionMode) -> &'static str {
    match mode {
        crate::contention::ContentionMode::YieldFirst => "yield_first",
        crate::contention::ContentionMode::Neutral => "neutral",
        crate::contention::ContentionMode::Performance => "performance",
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
