//! Deterministic, effect-free admission and yielding recommendations.
//!
//! Running/draining allocations are charged max(reservation, observed), dimension by
//! dimension. Pending reservations are charged in full. GPU utilization is auxiliary
//! telemetry and is deliberately never a predicate for contention.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
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
    match config.gpu.execution_mode {
        GpuExecutionMode::Auto => observation.map(|_| {
            if fresh {
                ContentionAware
            } else {
                ConservativeNonSharing
            }
        }),
        GpuExecutionMode::ContentionAware => Some(if fresh {
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
        ensure!(
            capability == Some(GpuExecutionCapability::BestEffortOccupied)
                || gpu.external_compute == ComputeActivity::Idle,
            "GPU {uuid} has active or unknown external activity at the launch recheck"
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
    external_memory: Option<u64>,
    sample_id: Option<u64>,
    /// Retain the protection cap until a full stable idle interval has elapsed.
    protection_cap: Option<u64>,
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
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
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
        let mut missing_observations = Resources::default();
        let mut reasons = Vec::new();
        for allocation in &input.allocations {
            let mut charge = allocation.requested.clone();
            if allocation.phase != AllocationPhase::Pending {
                if let Some(observed) = &allocation.observed {
                    add_resources(&mut managed_observed, observed)?;
                    charge = maximum(&charge, observed);
                } else {
                    add_resources(&mut missing_observations, &allocation.requested)?;
                    reasons.push(format!(
                        "allocation {} has unknown managed usage; affected admission is blocked",
                        allocation.id
                    ));
                }
            }
            add_resources(&mut charged, &charge)?;
            charges.insert(allocation.id.clone(), charge);
        }

        let mut budget = Resources::default();
        let mut unsafe_now = input.explicit_drain;
        let mut cpu_unknown = missing_observations.cpu_millicores > 0;
        let mut ram_unknown = missing_observations.ram_mib > 0;
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
            let external = busy.saturating_sub(managed_observed.cpu_millicores);
            budget.cpu_millicores = cpu_capacity
                .saturating_sub(cpu_reserve)
                .saturating_sub(external)
                .min(cpu_ceiling);
            if managed_observed.cpu_millicores > busy {
                cpu_unknown = true;
                reasons.push(
                    "managed CPU observation exceeds busy observation in the selected scope; admission blocked"
                        .into(),
                );
            }
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
            let external = used.saturating_sub(managed_observed.ram_mib);
            budget.ram_mib = snapshot
                .total_ram_mib
                .saturating_sub(ram_reserve)
                .saturating_sub(external);
            if managed_observed.ram_mib > used {
                ram_unknown = true;
                reasons.push(
                    "managed RAM observation exceeds system used observation; admission blocked"
                        .into(),
                );
            }
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

        let host_capacity_safe = !input.explicit_drain
            && !cpu_unknown
            && !ram_unknown
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
                let current_charge = charged.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0);
                let external = gpu
                    .used_memory_mib
                    .map(|used| used.saturating_sub(observed));
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
                let new_external = pids.difference(&previous.pids).next().is_some();
                let memory_growth = match (external, previous.external_memory) {
                    (Some(current), Some(old)) => current > old,
                    (Some(current), None) => current > 0,
                    _ => false,
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
                    || missing_observations.gpu_memory_mib.contains_key(&gpu.uuid)
                    || gpu.used_memory_mib.is_some_and(|used| observed > used);
                let active = !best_effort && gpu.external_compute == ComputeActivity::Active;
                let breached = current_charge > memory_budget;
                let protection_event = new_external || memory_growth;
                let unknown = activity_unknown || telemetry_unknown;
                let non_sharing_conflict = mode
                    == Some(GpuExecutionCapability::ConservativeNonSharing)
                    && (!pids.is_empty() || gpu.external_process_ids.is_none());
                let mut cap = previous.protection_cap;
                if non_sharing_conflict
                    || mode == Some(GpuExecutionCapability::InsufficientObservability)
                {
                    cap = Some(0);
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    reasons.push(format!("GPU {} conservative non-sharing/insufficient observation: full opportunistic yield; no new GPU work until complete empty inventory and stable cooldown", gpu.uuid));
                }
                if protection_event || unknown || active || breached {
                    blocked_gpus.insert(gpu.uuid.clone());
                    unsafe_now = true;
                    if protection_event {
                        reasons.push(format!("GPU {} new external workload or external memory growth; protective yielding", gpu.uuid));
                    }
                    if active {
                        reasons.push(format!("GPU {} external compute activity; utilization percentage is not an interference measurement", gpu.uuid));
                    }
                    if unknown {
                        reasons.push(format!("GPU {} external activity or memory attribution unknown; expansion blocked and conservative yielding", gpu.uuid));
                    }
                    if breached {
                        reasons.push(format!(
                            "GPU {} memory reserve or allocation budget breached",
                            gpu.uuid
                        ));
                    }
                    let shrink = if active && fresh_sample {
                        config.gpu.active_shrink_percent
                    } else {
                        config.gpu.protective_shrink_percent
                    };
                    // Hold the same cap on repeated uncertain/active samples. New external
                    // evidence, fresh active evidence, or first uncertainty can tighten it.
                    if protection_event
                        || (active && fresh_sample)
                        || ((unknown || active) && cap.is_none())
                    {
                        let target = current_charge.saturating_sub(ceil_ratio(
                            current_charge,
                            u64::from(shrink),
                            100,
                        ));
                        cap = Some(cap.map_or(target, |old| old.min(target)));
                    }
                }
                let gpu_budget = cap.map_or(memory_budget, |cap| memory_budget.min(cap));
                budget.gpu_memory_mib.insert(gpu.uuid.clone(), gpu_budget);
                next_gpus.insert(
                    gpu.uuid.clone(),
                    PreviousGpu {
                        pids,
                        external_memory: external,
                        sample_id: gpu.compute_sample_id.max(previous.sample_id),
                        protection_cap: cap,
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
        {
            // Each UUID has independent stability. One unavailable device must
            // not erase another device's qualified capacity or recovery clock.
            for gpu in &snapshot.gpus {
                let device_cooled_down = next_gpus
                    .get(&gpu.uuid)
                    .and_then(|p| p.stable_since)
                    .is_some_and(|since| {
                        now_monotonic_ms - since >= config.gpu.scale_up_cooldown_ms
                    });
                if !blocked_gpus.contains(&gpu.uuid) && device_cooled_down {
                    if let Some(prior) = next_gpus.get_mut(&gpu.uuid) {
                        prior.protection_cap = None;
                    }
                    if let Some(used) = gpu.used_memory_mib {
                        let own = managed_observed
                            .gpu_memory_mib
                            .get(&gpu.uuid)
                            .copied()
                            .unwrap_or(0);
                        budget.gpu_memory_mib.insert(
                            gpu.uuid.clone(),
                            gpu.total_memory_mib
                                .saturating_sub(config.gpu.reserve_vram_mib)
                                .saturating_sub(used.saturating_sub(own)),
                        );
                    }
                }
            }
        }
        if input.explicit_drain {
            budget = Resources::default();
        }

        let mut remaining = charged.clone();
        let mut would_drain = Vec::new();
        // Existing drains are planned releases only for choosing additional victims.
        // They remain charged against admission below until release is confirmed.
        for allocation in &input.allocations {
            if allocation.phase == AllocationPhase::Draining {
                subtract_resources(&mut remaining, &charges[&allocation.id]);
            }
        }
        // Automatic yielding may only plan releases the agent/supervisor can enact.
        // Guaranteed work stays fully charged but cannot consume a victim slot and
        // hide the need to drain opportunistic work. Explicit drain includes both.
        // Cancel eligible pending reservations before running work, then lexical ID.
        let mut candidates: Vec<_> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase != AllocationPhase::Draining
                    && (input.explicit_drain
                        || a.class == crate::execution_model::AllocationClass::Opportunistic)
            })
            .collect();
        candidates.sort_by_key(|a| (a.phase != AllocationPhase::Pending, a.id.as_str()));
        for allocation in candidates {
            let charge = &charges[&allocation.id];
            if input.explicit_drain || contributes_to_excess(&remaining, &budget, charge) {
                would_drain.push(allocation.id.clone());
                subtract_resources(&mut remaining, charge);
            }
        }
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
            && cpu_capacity > 0
            && snapshot.total_ram_mib > 0
            && charged.cpu_millicores <= budget.cpu_millicores
            && charged.ram_mib <= budget.ram_mib;
        let cpu_ram_stable_since =
            cpu_ram_safe.then_some(self.cpu_ram_stable_since.unwrap_or(now_monotonic_ms));
        let cpu_ram_expansion_allowed = cpu_ram_stable_since
            .is_some_and(|since| now_monotonic_ms - since >= config.gpu.scale_up_cooldown_ms);
        let mut headroom = budget.clone();
        // Recommendations are not release confirmation: subtract ALL allocations,
        // including existing draining work and everything just recommended for drain.
        subtract_resources(&mut headroom, &charged);
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

fn add_resources(total: &mut Resources, value: &Resources) -> Result<()> {
    total.cpu_millicores = total
        .cpu_millicores
        .checked_add(value.cpu_millicores)
        .context("CPU resource sum overflow")?;
    total.ram_mib = total
        .ram_mib
        .checked_add(value.ram_mib)
        .context("RAM resource sum overflow")?;
    for (gpu, amount) in &value.gpu_memory_mib {
        let current = total.gpu_memory_mib.entry(gpu.clone()).or_default();
        *current = current
            .checked_add(*amount)
            .context("GPU resource sum overflow")?;
    }
    Ok(())
}

fn maximum(left: &Resources, right: &Resources) -> Resources {
    let mut result = left.clone();
    result.cpu_millicores = result.cpu_millicores.max(right.cpu_millicores);
    result.ram_mib = result.ram_mib.max(right.ram_mib);
    for (gpu, amount) in &right.gpu_memory_mib {
        let current = result.gpu_memory_mib.entry(gpu.clone()).or_default();
        *current = (*current).max(*amount);
    }
    result
}

fn subtract_resources(total: &mut Resources, value: &Resources) {
    total.cpu_millicores = total.cpu_millicores.saturating_sub(value.cpu_millicores);
    total.ram_mib = total.ram_mib.saturating_sub(value.ram_mib);
    for (gpu, amount) in &value.gpu_memory_mib {
        if let Some(current) = total.gpu_memory_mib.get_mut(gpu) {
            *current = current.saturating_sub(*amount);
        }
    }
}

fn contributes_to_excess(total: &Resources, budget: &Resources, charge: &Resources) -> bool {
    (total.cpu_millicores > budget.cpu_millicores && charge.cpu_millicores > 0)
        || (total.ram_mib > budget.ram_mib && charge.ram_mib > 0)
        || charge.gpu_memory_mib.iter().any(|(gpu, amount)| {
            *amount > 0
                && total.gpu_memory_mib.get(gpu).copied().unwrap_or(0)
                    > budget.gpu_memory_mib.get(gpu).copied().unwrap_or(0)
        })
}
