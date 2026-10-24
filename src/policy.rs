//! Deterministic, effect-free admission and yielding recommendations.
//!
//! Running/draining allocations are charged max(reservation, observed), dimension by
//! dimension. Pending reservations are charged in full. GPU utilization is auxiliary
//! telemetry and is deliberately never a predicate for contention.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::contention::{
    LadderInput, LadderTiming, Measurement, ModeInput, Principal, ResourceKey, ResourceKind,
    StepState, VictimFacts, YieldCause, YieldStep, guaranteed_may_drain, own_allowance, plan_step,
    victim_order,
};
use crate::kernel::{CpuTopology, CpuUsageStatus, KernelSnapshot};
use crate::model::*;

/// Select only behavior supported by the current observation. Legacy recordings
/// retain their original interpretation instead of inferring missing capability.
pub fn effective_gpu_capability(
    config: &Config,
    observed_at_unix_ms: u64,
    gpu: &GpuSnapshot,
) -> Option<GpuExecutionCapability> {
    use crate::config::GpuExecutionMode;
    use GpuExecutionCapability::*;
    let basic = gpu.total_memory_mib > 0
        && gpu
            .used_memory_mib
            .is_some_and(|used| used <= gpu.total_memory_mib)
        && gpu.external_process_ids.is_some();
    let observation = gpu.observation.as_ref();
    if !basic
        || observation.is_some_and(|o| {
            o.observed_at_unix_ms != observed_at_unix_ms
                || o.capability == InsufficientObservability
        })
    {
        return Some(InsufficientObservability);
    }
    let fresh = observation.is_some_and(|o| {
        o.capability == ContentionAware
            && o.fresh_after_baseline
            && o.query_cursor_us > 0
            && o.newest_sample_timestamp_us
                .is_some_and(|v| v > o.query_cursor_us)
            && o.baseline_age_ms
                .is_some_and(|age| age <= config.gpu.process_sample_max_age_ms)
    });
    // A Quiet class is positive evidence from healthy observation over a window,
    // not the absence of samples, so it qualifies like a fresh sample (research R8).
    let healthy = observation.is_some_and(|o| o.capability == ContentionAware);
    let evidence = fresh || (gpu.external_compute == ComputeActivity::Quiet && healthy);
    match config.gpu.execution_mode {
        GpuExecutionMode::Auto => observation.map(|_| {
            if evidence {
                ContentionAware
            } else {
                ConservativeNonSharing
            }
        }),
        // Explicit contention-aware mode needs healthy observation; unknown activity
        // then blocks admission and holds instead of forcing a full yield.
        GpuExecutionMode::ContentionAware => Some(if healthy {
            ContentionAware
        } else {
            InsufficientObservability
        }),
        GpuExecutionMode::ConservativeNonSharing => Some(ConservativeNonSharing),
        GpuExecutionMode::BestEffortOccupied => Some(
            if best_effort_identity_contract(config, observed_at_unix_ms, gpu) {
                BestEffortOccupied
            } else {
                InsufficientObservability
            },
        ),
    }
}

/// Pure verification of collector evidence against this operator's exact device
/// contract. Unavailable evidence, extra contexts, PID reuse, and stale snapshots
/// never turn into permission. Activity remains independently Active/Unknown.
fn best_effort_identity_contract(
    config: &Config,
    observed_at_unix_ms: u64,
    gpu: &GpuSnapshot,
) -> bool {
    let Some(allowed) = config.gpu.best_effort_external_processes.get(&gpu.uuid) else {
        return false;
    };
    let Some(observation) = gpu.observation.as_ref() else {
        return false;
    };
    let Some(verified) = observation.best_effort_external_processes.as_ref() else {
        return false;
    };
    let Some(pids) = gpu.external_process_ids.as_ref() else {
        return false;
    };
    let unique: BTreeSet<_> = pids.iter().copied().collect();
    !allowed.is_empty()
        && observation.observed_at_unix_ms == observed_at_unix_ms
        && unique.len() == pids.len()
        && verified.len() == unique.len()
        && unique.iter().all(|pid| {
            verified
                .iter()
                .filter(|identity| identity.pid == *pid)
                .count()
                == 1
        })
        && verified.iter().all(|identity| {
            identity.pid > 0
                && identity.start_ticks > 0
                && !identity.boot_id.trim().is_empty()
                && allowed.contains(identity)
        })
}

/// Automatic full yield conflicts with Guaranteed continuity. Reject the contract
/// before preparation, including Auto which may enter non-sharing after startup.
pub fn validate_gpu_launch_contract(
    config: &Config,
    snapshot: &Snapshot,
    resources: &Resources,
    class: crate::execution_model::AllocationClass,
) -> Result<()> {
    if resources.gpu_memory_mib.is_empty() {
        return Ok(());
    }
    ensure!(
        class != crate::execution_model::AllocationClass::Guaranteed
            || config.gpu.execution_mode == crate::config::GpuExecutionMode::ContentionAware,
        "Guaranteed GPU continuity conflicts with Auto/conservative_non_sharing/best_effort_occupied yielding; use an opportunistic allocation or explicitly qualified contention_aware policy"
    );
    ensure!(
        snapshot.gpu_inventory == CapabilityStatus::Available,
        "GPU launch requires available device inventory"
    );
    for uuid in resources.gpu_memory_mib.keys() {
        let gpu = snapshot
            .gpus
            .iter()
            .find(|g| &g.uuid == uuid)
            .with_context(|| format!("GPU {uuid} is absent from the current inventory"))?;
        let capability = effective_gpu_capability(config, snapshot.observed_at_unix_ms, gpu);
        ensure!(
            capability != Some(GpuExecutionCapability::InsufficientObservability),
            "GPU {uuid} lacks the observation required by its configured execution mode"
        );
        if capability == Some(GpuExecutionCapability::ConservativeNonSharing) {
            ensure!(
                gpu.external_process_ids.as_ref().is_some_and(Vec::is_empty),
                "GPU {uuid} conservative non-sharing launch requires a complete empty external inventory"
            );
        }
        let mode = config.contention.mode(ResourceKind::GpuCompute);
        // Active devices stay blocked in every mode, as in policy admission, until
        // the US5 floors protect external usage.
        let admitted = matches!(
            gpu.external_compute,
            ComputeActivity::Idle | ComputeActivity::Quiet
        );
        ensure!(
            capability == Some(GpuExecutionCapability::BestEffortOccupied) || admitted,
            "GPU {uuid} external activity {:?} is not admitted by mode {mode:?} at the launch recheck",
            gpu.external_compute
        );
    }
    Ok(())
}

/// Coordinator placement uses this admission budget, while policy accounting
/// retains the original managed budget and charges every live/reserved allocation.
pub fn schedulable_budget(decision: &Decision) -> Resources {
    let mut budget = decision.managed_budget.clone();
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
    cpu_ram_stable_since: Option<u64>,
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
    /// When a memory floor breach began, for the Guaranteed hint deadline.
    floor_breach_since: BTreeMap<ResourceKey, u64>,
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
        let mut reasons = Vec::new();
        let mut statuses: BTreeMap<ResourceKey, ResourceStatus> = BTreeMap::new();
        for allocation in &input.allocations {
            let mut charge = allocation.requested.clone();
            if allocation.phase != AllocationPhase::Pending {
                if let Some(observed) = &allocation.observed {
                    managed_observed.add_checked(observed)?;
                    charge = Resources::charge(&charge, Some(observed));
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
                            config.contention.max_mode,
                        );
                        (kind, mode)
                    })
                    .collect();
                (a.id.clone(), modes)
            })
            .collect();
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
        let ram_reserve = config.ram.reserve_mib.max(ceil_ratio(
            snapshot.total_ram_mib,
            u64::from(config.ram.reserve_percent),
            100,
        ));
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
        let principals = |pick: &dyn Fn(&PrincipalUsage) -> u64| {
            snapshot.principals.as_ref().map(|list| {
                list.iter()
                    .map(|p| {
                        let principal = Principal {
                            uid: p.uid,
                            is_owner: p.is_owner,
                        };
                        (principal, pick(p))
                    })
                    .collect::<Vec<_>>()
            })
        };
        let fair_share_over_floor =
            config.contention.neutral_memory_policy == crate::config::NeutralMemoryPolicy::B;
        // What own work may hold before the mode's target applies: for memory, what
        // keeps the floor free.
        let mut floor_available: BTreeMap<ResourceKey, u64> = BTreeMap::new();
        let cpu_schedulable = cpu_capacity.saturating_sub(cpu_reserve);
        let cpu_total = cpu_capacity.min(cpu_ceiling);
        // A saturated host suppresses measured external usage.
        let cpu_saturated =
            cpu_busy.is_some_and(|busy| busy.saturating_add(cpu_total / 50) >= cpu_total);
        if !cpu_unknown && !cpu_held {
            floor_available.insert(ResourceKey::Cpu, budget.cpu_millicores);
            budget.cpu_millicores = own_allowance(
                &ModeInput {
                    kind: ResourceKind::Cpu,
                    mode: governing(ResourceKind::Cpu),
                    capacity: cpu_schedulable,
                    external: statuses
                        .get(&ResourceKey::Cpu)
                        .and_then(|s| s.external)
                        .unwrap_or(0),
                    principals: principals(&|p| p.cpu_millicores),
                    saturated: cpu_saturated,
                    unknown_activity: false,
                    fair_share_over_floor: false,
                },
                &config.contention.thresholds(ResourceKind::Cpu),
            )
            // An ancestor quota bounds own work after external usage is subtracted.
            .min(cpu_ceiling);
        }
        if !ram_unknown && !ram_held {
            floor_available.insert(ResourceKey::Ram, budget.ram_mib);
            budget.ram_mib = own_allowance(
                &ModeInput {
                    kind: ResourceKind::Ram,
                    mode: governing(ResourceKind::Ram),
                    capacity: snapshot.total_ram_mib.saturating_sub(ram_reserve),
                    external: statuses
                        .get(&ResourceKey::Ram)
                        .and_then(|s| s.external)
                        .unwrap_or(0),
                    principals: principals(&|p| p.ram_mib),
                    saturated: false,
                    unknown_activity: false,
                    fair_share_over_floor,
                },
                &config.contention.thresholds(ResourceKind::Ram),
            );
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
                let mode = effective_gpu_capability(config, snapshot.observed_at_unix_ms, gpu);
                if let Some(mode) = mode {
                    reasons.push(format!(
                        "GPU {} execution capability {:?}; configured {:?}",
                        gpu.uuid, mode, config.gpu.execution_mode
                    ));
                }
                let best_effort = mode == Some(GpuExecutionCapability::BestEffortOccupied);
                if best_effort {
                    reasons.push(format!("GPU {} best_effort_occupied explicitly permits only verified external identities; external activity remains {:?}, interference and slowdown are unmeasured, no hard GPU isolation or continuity guarantee", gpu.uuid, gpu.external_compute));
                }
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
                            .saturating_sub(config.gpu.reserve_vram_mib)
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
                let activity_unknown = !best_effort
                    && (gpu.external_compute == ComputeActivity::Unknown
                        || (!pids.is_empty() && !fresh_sample));
                let telemetry_unknown = mode
                    == Some(GpuExecutionCapability::InsufficientObservability)
                    || gpu.used_memory_mib.is_none()
                    || gpu.external_process_ids.is_none()
                    || unknown_own > 0
                    || gpu.used_memory_mib.is_some_and(|used| observed > used);
                let active = !best_effort && gpu.external_compute == ComputeActivity::Active;
                let breached = current_charge > memory_budget;
                let protection_event = new_external || memory_growth;
                let unknown = activity_unknown || telemetry_unknown;
                let non_sharing_conflict = mode
                    == Some(GpuExecutionCapability::ConservativeNonSharing)
                    && (!pids.is_empty() || gpu.external_process_ids.is_none());
                // The configured non-sharing contract and missing observation yield
                // fully; everything else blocks admission and yields only on a real
                // budget breach.
                let full_yield = non_sharing_conflict
                    || mode == Some(GpuExecutionCapability::InsufficientObservability);
                if full_yield {
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    reasons.push(format!("GPU {} conservative non-sharing/insufficient observation: full opportunistic yield; no new GPU work until complete empty inventory and stable cooldown", gpu.uuid));
                }
                if protection_event || unknown || active || breached {
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    if protection_event {
                        reasons.push(format!("GPU {} new external workload or external memory growth beyond the noise tolerance; GPU admission blocked", gpu.uuid));
                    }
                    if active {
                        reasons.push(format!("GPU {} external compute activity; utilization percentage is not an interference measurement", gpu.uuid));
                    }
                    if unknown {
                        reasons.push(format!("GPU {} external activity or memory attribution unknown; expansion blocked", gpu.uuid));
                    }
                    if breached {
                        reasons.push(format!(
                            "GPU {} memory reserve or allocation budget breached",
                            gpu.uuid
                        ));
                    }
                }
                let gpu_budget = if full_yield || telemetry_unknown {
                    if full_yield { 0 } else { memory_budget }
                } else {
                    floor_available.insert(key.clone(), memory_budget);
                    own_allowance(
                        &ModeInput {
                            kind: ResourceKind::GpuMemory,
                            mode: governing(ResourceKind::GpuMemory),
                            capacity: gpu
                                .total_memory_mib
                                .saturating_sub(config.gpu.reserve_vram_mib),
                            external: external.unwrap_or(0),
                            principals: principals(&|p| {
                                p.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0)
                            }),
                            saturated: false,
                            unknown_activity: false,
                            fair_share_over_floor,
                        },
                        &config.contention.thresholds(ResourceKind::GpuMemory),
                    )
                };
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
        let cooled_down = stable_since
            .is_some_and(|since| now_monotonic_ms - since >= config.gpu.scale_up_cooldown_ms);
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
        let mut actions = Vec::new();
        let mut breached: BTreeSet<ResourceKey> = BTreeSet::new();
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
                // Memory beyond what keeps the floor free breaches it in every mode.
                let floor_breach = kind.is_memory()
                    && floor_available
                        .get(&key)
                        .is_some_and(|available| held_charge > *available);
                if floor_breach {
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
                        format!(
                            "own work holds {held_charge}, above the {} that keeps the floor free",
                            floor_available[&key]
                        ),
                    );
                }
                let cause = if held || gpu_unknown {
                    YieldCause::UnknownMeasurement
                } else if floor_breach {
                    YieldCause::FloorBreach
                } else {
                    YieldCause::ExternalGrowth
                };
                let external = statuses.get(&key).and_then(|s| s.external).unwrap_or(0);
                let timing = LadderTiming::new(
                    &config.contention.thresholds(kind),
                    governing(kind),
                    interval_ms,
                );
                let usage_of =
                    |a: &Allocation| -> u64 { a.observed.as_ref().map_or(0, |o| o.cpu_millicores) };
                // Pending reservations are cancelled by their charge. CPU in use is
                // measured by usage against the mode's allowance, since a cap lowers
                // usage but never the request.
                let admission_excess = held_charge.saturating_sub(amount_of(&budget));
                let usage_excess = if kind == ResourceKind::Cpu {
                    input
                        .allocations
                        .iter()
                        .filter(|a| a.phase != AllocationPhase::Pending)
                        .map(usage_of)
                        .fold(unknown_charge.cpu_millicores, u64::saturating_add)
                        .saturating_sub(budget.cpu_millicores)
                } else {
                    admission_excess
                };
                let guaranteed =
                    |a: &Allocation| a.class == crate::execution_model::AllocationClass::Guaranteed;
                let opportunistic_left = input
                    .allocations
                    .iter()
                    .any(|a| !guaranteed(a) && amount_of(&charges[&a.id]) > 0);
                // Guaranteed work is drained only for a memory floor under floor
                // policy A, after opportunistic work and the hint deadline (FR-024).
                let guaranteed_drain = guaranteed_may_drain(
                    kind,
                    cause,
                    config.contention.floor_policy == crate::config::FloorPolicy::A,
                    opportunistic_left,
                    floor_breach_since
                        .get(&key)
                        .map_or(0, |start| now_monotonic_ms.saturating_sub(*start)),
                    config
                        .contention
                        .thresholds(kind)
                        .hint_deadline_s
                        .saturating_mul(1000),
                );
                let candidate = |a: &&Allocation| {
                    a.phase != AllocationPhase::Draining
                        && allocation_unknown_since.get(&a.id).is_none_or(|start| {
                            now_monotonic_ms.saturating_sub(*start) >= window(kind)
                        })
                };
                // No resource-target channel exists yet (US4), so the hint step is
                // unavailable everywhere and reported as skipped.
                let facts = |id: &str| {
                    let mut facts = input.facts.get(id).cloned().unwrap_or_default();
                    facts
                        .unavailable
                        .entry(key.clone())
                        .or_default()
                        .insert(YieldStep::Hint);
                    facts
                };
                let mut planned = BTreeSet::new();
                // Memory's running pass covers what cancelled reservations left over.
                let mut carried = admission_excess;
                for pending_pass in [true, false] {
                    let needed = if pending_pass {
                        admission_excess
                    } else if kind == ResourceKind::Cpu {
                        usage_excess
                    } else {
                        carried
                    };
                    let mut victims: Vec<VictimFacts> = input
                        .allocations
                        .iter()
                        .filter(candidate)
                        .filter(|a| (a.phase == AllocationPhase::Pending) == pending_pass)
                        .map(|a| {
                            let amount = if kind == ResourceKind::Cpu && !pending_pass {
                                usage_of(a)
                            } else {
                                amount_of(&charges[&a.id])
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
                        let state = self.ladder.get(&(victim.id.clone(), key.clone()));
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
                                target: (kind == ResourceKind::Cpu)
                                    .then(|| victim.amount.saturating_sub(remaining)),
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
                        if let Some(next) = &decision.state {
                            // A newly applied step is expected to release its share;
                            // the next observation shows whether it did.
                            if next.step > before {
                                let freed = match next.step {
                                    YieldStep::Cap => victim
                                        .amount
                                        .saturating_sub(next.target.unwrap_or(victim.amount)),
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
                    carried = remaining;
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
        let cpu_ram_safe = !input.explicit_drain
            && !cpu_unknown
            && !ram_unknown
            && !cpu_held
            && !ram_held
            && cpu_capacity > 0
            && snapshot.total_ram_mib > 0
            && charged.cpu_millicores <= budget.cpu_millicores
            && charged.ram_mib <= budget.ram_mib;
        // CPU/RAM admission waits its own cooldown, never the GPU one (M1).
        let cpu_ram_stable_since =
            cpu_ram_safe.then_some(self.cpu_ram_stable_since.unwrap_or(now_monotonic_ms));
        let cpu_ram_cooldown_ms = [ResourceKind::Cpu, ResourceKind::Ram]
            .into_iter()
            .map(|kind| {
                config
                    .contention
                    .thresholds(kind)
                    .cpu_ram_cooldown_ms(governing(kind))
            })
            .max()
            .unwrap_or(0);
        let cpu_ram_expansion_allowed = cpu_ram_stable_since
            .is_some_and(|since| now_monotonic_ms - since >= cpu_ram_cooldown_ms);
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
                        now_monotonic_ms - since >= config.gpu.scale_up_cooldown_ms
                    })
        });
        // A breached floor blocks admission on that resource in every policy (FR-041).
        if breached.contains(&ResourceKey::Ram) {
            headroom.ram_mib = 0;
        }
        let expansion_allowed = legacy_expansion_allowed
            || (cpu_ram_expansion_allowed
                && headroom.gpu_memory_mib.values().any(|amount| *amount > 0));
        if !expansion_allowed {
            if !cpu_ram_expansion_allowed {
                headroom.cpu_millicores = 0;
                headroom.ram_mib = 0;
            } else {
                reasons.push("independently stable CPU/RAM headroom permits CPU-only admission; GPU admission remains blocked".into());
            }
            if !unsafe_now && !cooled_down {
                reasons.push("stable-capacity cooldown has not elapsed".into());
            }
        }
        self.last_now = Some(now_monotonic_ms);
        self.node_id = Some(snapshot.node_id.clone());
        self.stable_since = stable_since;
        self.cpu_ram_stable_since = cpu_ram_stable_since;
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
            exceptions: config.contention.exceptions(),
            allocation_modes,
        })
    }
}

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
