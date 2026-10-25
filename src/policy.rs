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
    Admission, LadderInput, LadderTiming, Measurement, ModeInput, Principal, ResourceKey,
    ResourceKind, StepState, VictimFacts, YieldCause, YieldStep, guaranteed_may_drain,
    own_allowance, plan_step, victim_order,
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
        if let Admission::Held { reason } | Admission::Blocked { reason } = admission {
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
                    reasons.push(format!(
                        "allocation {} has unknown managed usage; charged at max(request, last known), its step holds and affected admission is blocked",
                        allocation.id
                    ));
                }
            }
            charged.add_checked(&charge)?;
            charges.insert(allocation.id.clone(), charge);
        }
        // Each allocation runs at min(requested, node cap); the least aggressive
        // effective mode among running work governs a resource's target.
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
        let governing = |kind: ResourceKind| {
            allocation_modes
                .values()
                .map(|modes| modes[&kind])
                .min()
                .unwrap_or_else(|| config.contention.mode(kind))
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
            reasons.push("explicit node drain requested".into());
        }
        if snapshot.cpu_capacity_millicores == 0 {
            reasons.push("CPU capacity unavailable; zero CPU scheduling budget".into());
        }
        if snapshot.total_ram_mib == 0 {
            reasons.push("RAM capacity unavailable; zero RAM scheduling budget".into());
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
                    reasons
                        .push("kernel CPU collector clock reset; establish a new baseline".into());
                    next_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now_monotonic_ms));
                }
                Some((sample, received)) if kernel.monotonic_elapsed_ms == sample => {
                    if now_monotonic_ms.saturating_sub(received) > kernel.freshness_limit_ms {
                        valid = false;
                        reasons.push(
                            "kernel CPU snapshot is stale; repeated readings do not refresh it"
                                .into(),
                        );
                    }
                }
                _ => next_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now_monotonic_ms)),
            }
            let capacity = kernel.cpu.effective_cpu_capacity_millicores.unwrap_or(0);
            let reserve = scoped_physical_reserve(&kernel.cpu, config.cpu.reserve_physical_cores);
            if reserve.is_none() {
                valid = false;
                reasons.push("effective CPU topology unavailable; cannot map physical-core reserve within permitted CPUs".into());
            }
            if !valid {
                cpu_unknown = true;
            }
            reasons.push("CPU budget uses matching effective-CPU usage and hardware capacity; visible ancestor bandwidth is a separate ceiling, not a guaranteed share".into());
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
            reasons.push(
                "CPU topology unavailable; cannot establish configured physical-core reserve"
                    .into(),
            );
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
                reasons.push(
                    "managed CPU observation exceeds busy observation in the selected scope beyond the known error; CPU holds and admission is blocked"
                        .into(),
                );
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
            reasons.push("CPU busy telemetry unavailable; CPU admission blocked".into());
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
                reasons.push(
                    "managed RAM observation exceeds system used observation; RAM holds and admission is blocked"
                        .into(),
                );
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
            reasons.push("RAM availability telemetry unavailable; RAM admission blocked".into());
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
        let cpu_saturated = cpu_busy
            .is_some_and(|busy| busy.saturating_mul(100) >= cpu_total.saturating_mul(saturation));
        let mut satisfied = BTreeMap::new();
        let mut probes = BTreeMap::new();
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
            let (known, probe) = crate::contention::probe_demand(
                &view,
                &thresholds,
                self.probes.get(&ResourceKey::Cpu),
                response_ms,
                now_monotonic_ms,
            );
            view.satisfied = known.clone();
            view.probe = probe.as_ref().map(|p| p.target);
            if !known.is_empty() {
                satisfied.insert(ResourceKey::Cpu, known);
            }
            if let Some(probe) = probe {
                probes.insert(ResourceKey::Cpu, probe);
            }
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
            budget.ram_mib = own_allowance(
                &ModeInput {
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
                    unknown_activity: false,
                    satisfied: vec![],
                    probe: None,
                },
                &config.contention.thresholds(ResourceKind::Ram),
            );
        }
        // The operator envelope bounds own work per resource (FR-057).
        if let Some(envelope) = &input.envelope {
            budget.cpu_millicores = budget.cpu_millicores.min(envelope.cpu_millicores);
            budget.ram_mib = budget.ram_mib.min(envelope.ram_mib);
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

        let host_capacity_safe = !input.explicit_drain
            && !cpu_unknown
            && !ram_unknown
            && !cpu_held
            && !ram_held
            && cpu_capacity > 0
            && snapshot.total_ram_mib > 0
            && charged.cpu_millicores <= budget.cpu_millicores
            && charged.ram_mib <= budget.ram_mib;
        let mut next_gpus = self.previous_gpus.clone();
        for value in next_gpus.values_mut() {
            value.stable_since = None;
        }
        let mut blocked_gpus = BTreeSet::new();
        // Each device's GPU compute allowance and whether it is contended, and the
        // devices whose external activity is unknown.
        let mut gpu_compute_budget: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        let mut gpu_activity_unknown: BTreeSet<String> = BTreeSet::new();
        // Why GPU admission is blocked on a device, for its admission state.
        let mut gpu_block_reasons: BTreeMap<String, String> = BTreeMap::new();
        if snapshot.gpu_inventory != CapabilityStatus::Available {
            reasons.push(format!(
                "GPU inventory {:?}; GPU admission blocked, CPU-only capacity remains eligible",
                snapshot.gpu_inventory
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
                let block_reason = if telemetry_unknown {
                    Some("GPU memory or its attribution is unknown".to_string())
                } else if protection_event {
                    Some("a new external workload or external memory growth appeared".into())
                } else if breached {
                    Some("own charges exceed the memory reserve or allocation budget".into())
                } else if let Admission::Held { reason } | Admission::Blocked { reason } =
                    &compute_admission
                {
                    Some(reason.clone())
                } else {
                    None
                };
                if let Some(reason) = &block_reason {
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    reasons.push(format!("GPU {}: {reason}; GPU admission blocked", gpu.uuid));
                    gpu_block_reasons.insert(gpu.uuid.clone(), reason.clone());
                }
                // GPU compute shares apply under external activity, which counts
                // as one principal of unknown amount; own GPU usage is not measured
                // per process, so idle-device margins act through admission only.
                let compute_key = ResourceKey::GpuCompute(gpu.uuid.clone());
                let contended_compute =
                    matches!(activity, ComputeActivity::Active | ComputeActivity::Unknown);
                if !telemetry_unknown {
                    let share = if contended_compute {
                        own_allowance(
                            &ModeInput {
                                kind: ResourceKind::GpuCompute,
                                mode: governing(ResourceKind::GpuCompute),
                                capacity: GPU_COMPUTE_CAPACITY,
                                external: 0,
                                own: GPU_COMPUTE_CAPACITY,
                                principals: None,
                                background: 0,
                                saturated: false,
                                unknown_activity: true,
                                satisfied: vec![],
                                probe: None,
                            },
                            &config.contention.thresholds(ResourceKind::GpuCompute),
                        )
                    } else {
                        GPU_COMPUTE_CAPACITY
                    };
                    gpu_compute_budget.insert(gpu.uuid.clone(), (share, contended_compute));
                }
                let compute_status = statuses.entry(compute_key).or_default();
                compute_status.activity = Some(activity);
                compute_status.target_share =
                    gpu_compute_budget.get(&gpu.uuid).map(|(share, _)| *share);
                compute_status.admission = Some(if telemetry_unknown {
                    Admission::Held {
                        reason: "GPU observation is unknown".into(),
                    }
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
                    if let Some(free) = gpu
                        .used_memory_mib
                        .map(|used| gpu.total_memory_mib.saturating_sub(used))
                        .filter(|free| *free < floor)
                    {
                        floor_breaches.insert(key.clone(), (free, floor));
                    }
                    let capacity = gpu
                        .total_memory_mib
                        .saturating_sub(config.vram_floor_mib(gpu.total_memory_mib));
                    let (members, background) = principals(key.clone(), capacity, &|p| {
                        p.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0)
                    });
                    own_allowance(
                        &ModeInput {
                            kind: ResourceKind::GpuMemory,
                            mode: governing(ResourceKind::GpuMemory),
                            capacity,
                            external: external.unwrap_or(0),
                            own: observed,
                            principals: members,
                            background,
                            saturated: false,
                            unknown_activity: false,
                            satisfied: vec![],
                            probe: None,
                        },
                        &config.contention.thresholds(ResourceKind::GpuMemory),
                    )
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
                        stable_since: if !blocked_gpus.contains(&gpu.uuid) && host_capacity_safe {
                            Some(previous.stable_since.unwrap_or(now_monotonic_ms))
                        } else {
                            None
                        },
                    },
                );
            }
            for gpu in charged.gpu_memory_mib.keys() {
                if !snapshot.gpus.iter().any(|item| &item.uuid == gpu) {
                    budget.gpu_memory_mib.insert(gpu.clone(), 0);
                    blocked_gpus.insert(gpu.clone());
                    unsafe_now = true;
                    reasons.push(format!(
                        "allocated GPU {gpu} is absent from the current inventory"
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
                let facts = |id: &str| {
                    let mut facts = input.facts.get(id).cloned().unwrap_or_default();
                    if !facts.hint_ready {
                        facts
                            .unavailable
                            .entry(key.clone())
                            .or_default()
                            .insert(YieldStep::Hint);
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
                        let kept = input
                            .allocations
                            .iter()
                            .find(|a| a.id == victim.id)
                            .map_or(0, kept_cpu);
                        let state = self.ladder.get(&(victim.id.clone(), key.clone()));
                        // A Guaranteed hint waits the Guaranteed hint deadline (FR-024).
                        let mut victim_timing = timing.clone();
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
                        let contended = remaining > 0 && victim.frees;
                        let decision = plan_step(
                            state,
                            &LadderInput {
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
                                excess: if contended { remaining } else { 0 },
                                external,
                                measurement_known,
                                cause,
                                usage: (kind == ResourceKind::Cpu && !pending_pass)
                                    .then(|| victim.amount.saturating_add(kept)),
                                // What the allocation may use: while contended, a cap
                                // or hint target; while calm, its usage plus its share
                                // of the spare, toward which a held cap rises.
                                target: Some(
                                    if contended {
                                        victim.amount.saturating_sub(remaining)
                                    } else {
                                        victim.amount.saturating_add(spare_share)
                                    }
                                    .saturating_add(kept),
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
                        &timing,
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
            for (uuid, (allowance, contended)) in &gpu_compute_budget {
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
                let share = GPU_COMPUTE_CAPACITY / running.len() as u64;
                let held = share * running.len() as u64;
                let excess = held.saturating_sub(*allowance);
                let external = if *contended { GPU_COMPUTE_CAPACITY } else { 0 };
                // Activity of unknown amount first holds the steps, as any unknown
                // measurement does, and escalates after the unknown window (FR-008).
                let unknown = gpu_activity_unknown.contains(uuid);
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
                let timing = LadderTiming::new(
                    &config.contention.thresholds(kind),
                    governing(kind),
                    interval_ms,
                );
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
                    let contended_victim = remaining > 0;
                    let decision = plan_step(
                        state,
                        &LadderInput {
                            kind,
                            guaranteed: victim.guaranteed,
                            allow_drain: false,
                            unavailable: unavailable_of(&victim.id),
                            excess: if contended_victim { remaining } else { 0 },
                            external,
                            measurement_known,
                            cause,
                            usage: None,
                            target: Some(if contended_victim {
                                share.saturating_sub(remaining)
                            } else {
                                share
                            }),
                        },
                        &timing,
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
                        &timing,
                        now_monotonic_ms,
                    );
                    if let Some(next) = decision.state {
                        ladder.insert((id.clone(), key.clone()), next);
                    }
                }
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
            reasons.push("managed CPU charges exceed scheduling budget".into());
        }
        if charged.ram_mib > budget.ram_mib {
            reasons.push("managed RAM charges exceed scheduling budget".into());
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
        // Each resource waits for its own cooldown, never the GPU one (M1, FR-034).
        let cpu_safe = !input.explicit_drain
            && !cpu_unknown
            && !cpu_held
            && cpu_capacity > 0
            && charged.cpu_millicores <= budget.cpu_millicores;
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
            host_capacity_safe
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
            reasons.push(
                "node spool budget exhausted; admission is blocked and no allocation is drained"
                    .into(),
            );
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
                reasons.push("independently stable CPU/RAM headroom permits CPU-only admission; GPU admission remains blocked".into());
            }
            if !unsafe_now && !cooled_down {
                reasons.push("stable-capacity cooldown has not elapsed".into());
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
        for (key, held) in held_back {
            statuses.entry(key).or_default().held_back = Some(held);
        }
        // Admission per resource: blocked until its cause changes, held while it
        // waits for a measurement or a cooldown (FR-028, FR-038).
        let stepped = |key: &ResourceKey| {
            ladder
                .iter()
                .any(|((_, k), state)| k == key && state.step > YieldStep::None)
        };
        let blocked = |reason: &str| Admission::Blocked {
            reason: reason.into(),
        };
        let held = |reason: &str| Admission::Held {
            reason: reason.into(),
        };
        let admission = |key: &ResourceKey,
                         unknown: bool,
                         gpu_reason: Option<&String>,
                         cooled_down: bool,
                         backed_off: bool| {
            if input.explicit_drain {
                blocked("the node is draining")
            } else if input.spool_full {
                blocked("the node spool budget is exhausted")
            } else if breached.contains(key) {
                blocked("the floor is breached")
            } else if unknown {
                held("the measurement is unknown")
            } else if let Some(reason) = gpu_reason {
                held(reason)
            } else if stepped(key) {
                blocked("own work on this resource is yielding")
            } else if !cooled_down {
                held("the resource is cooling down")
            } else if !backed_off {
                held("the reclaim backoff applies")
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
                admissions.push((key, blocked("own work on this resource is yielding")));
            }
        }
        for (key, state) in admissions {
            statuses.entry(key).or_default().admission = Some(state);
        }
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
        Ok(Decision {
            schema_version: SCHEMA_VERSION,
            node_id: snapshot.node_id.clone(),
            observe_only: true,
            managed_budget: budget,
            admission_headroom: headroom,
            expansion_allowed,
            cpu_ram_expansion_allowed,
            would_drain,
            reasons,
            resources: statuses,
            actions,
            exceptions: config.caution_options_on(snapshot),
            allocation_modes,
            max_mode: Some(cap),
            cpu_admission_credit_millicores: cpu_credit
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                as i64,
        })
    }
}

/// GPU compute is accounted in thousandths of one device's time.
const GPU_COMPUTE_CAPACITY: u64 = 1000;

fn scoped_cpu_valid(kernel: &KernelSnapshot, reasons: &mut Vec<String>) -> bool {
    let cpu = &kernel.cpu;
    let Some(ids) = &cpu.effective_cpu_ids else {
        reasons.push("effective CPU set is unavailable; scoped CPU admission blocked".into());
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
        reasons.push(format!("scoped CPU telemetry missing, stale or inconsistent ({:?}); host-wide busy time cannot substitute for the effective CPU set", cpu.busy_status));
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
