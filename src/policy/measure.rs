//! The interval's measurement phases: charges and modes, the CPU, RAM and GPU
//! readings, the mode targets and budgets, and whether the node is safe.
use super::arrival::{OwnArrival, Standing};
use super::*;

impl<'a> Interval<'a> {
    /// Charges: running and draining work at max(request, observed), pending
    /// reservations in full, and work of unknown usage at max(request, last
    /// known).
    /// Whether no principal but the account running CedeGrid is active on CPU: this interval's
    /// list shows none at `principal_min_percent` of capacity, and the last interval's estimate
    /// left none unattributed. An unreadable list counts as one (FR-034, Principle III).
    fn cpu_alone(&self) -> bool {
        let snapshot = self.snapshot;
        let Some(list) = snapshot.principals.as_ref() else {
            return false;
        };
        let capacity = self
            .scoped_kernel()
            .and_then(|kernel| kernel.cpu.effective_cpu_capacity_millicores)
            .unwrap_or(snapshot.cpu_capacity_millicores);
        let percent = self
            .config
            .contention
            .thresholds(ResourceKind::Cpu)
            .principal_min_percent;
        let floor = (capacity.saturating_mul(u64::from(percent)) / 100).max(1);
        let uid_min = self.config.contention.uid_min;
        self.prev.cpu_quiet_intervals > 0
            && !list.iter().any(|principal| {
                !principal.is_owner
                    && principal.uid.is_none_or(|uid| uid >= uid_min)
                    && principal.cpu_millicores >= floor
            })
    }

    pub(super) fn charge(&mut self) -> Result<()> {
        let (input, prev, now) = (self.input, self.prev, self.now);
        let cpu_window_ms = self
            .config
            .contention
            .thresholds(ResourceKind::Cpu)
            .admission_usage_window_s
            .saturating_mul(1000);
        let alone = self.cpu_alone();
        for allocation in &input.allocations {
            let mut charge = allocation.requested.clone();
            if allocation.phase != AllocationPhase::Pending {
                if let Some(observed) = &allocation.observed {
                    self.managed_observed.add_checked(observed)?;
                    charge = Resources::charge(&charge, Some(observed));
                    // CPU is compressible: running opportunistic work is admitted
                    // against its recent usage, not its request, once a full window
                    // shows it past its startup phase.
                    if allocation.class == crate::execution_model::AllocationClass::Opportunistic
                        && allocation.phase == AllocationPhase::Running
                    {
                        let (since, mut samples) = prev
                            .cpu_usage
                            .get(&allocation.id)
                            .cloned()
                            .unwrap_or((now, Default::default()));
                        // Usage a CPU step holds back is not demand: while a pause, or a cap
                        // or hint its usage reaches, holds the allocation, the window keeps
                        // its samples from before the step and earns no credit (FR-034). A
                        // cap above its usage holds nothing back, so demand that fell on its
                        // own still frees room (FR-028).
                        let held_back = prev
                            .ladder
                            .get(&(allocation.id.clone(), ResourceKey::Cpu))
                            .is_some_and(|state| match state.step {
                                YieldStep::Pause => true,
                                YieldStep::Cap | YieldStep::Hint => {
                                    state.target.is_some_and(|target| {
                                        observed.cpu_millicores.saturating_mul(100)
                                            >= target.saturating_mul(95)
                                    })
                                }
                                _ => false,
                            });
                        if held_back {
                            let held = allocation
                                .requested
                                .cpu_millicores
                                .max(p95(samples.iter().map(|(_, usage)| *usage)));
                            self.held_back_cpu = self
                                .held_back_cpu
                                .saturating_add(held.saturating_sub(charge.cpu_millicores));
                            charge.cpu_millicores = held;
                        } else {
                            samples.retain(|(at, _)| now.saturating_sub(*at) < cpu_window_ms);
                            // The window's value, with one bound for it, when the collector has
                            // one (FR-073).
                            samples.push_back((
                                now,
                                allocation
                                    .observed_window_cpu
                                    .unwrap_or(observed.cpu_millicores),
                            ));
                            if now.saturating_sub(since) >= cpu_window_ms {
                                // With no other principal an over-admission can only cost own
                                // work; its p95 protects others from the interval one appears.
                                let values = samples.iter().map(|(_, usage)| *usage);
                                let usage = if alone { mean(values) } else { p95(values) };
                                self.cpu_credit +=
                                    i128::from(charge.cpu_millicores) - i128::from(usage);
                                self.cpu_credit_requests += i128::from(
                                    allocation.requested.cpu_millicores.saturating_sub(usage),
                                );
                                charge.cpu_millicores = usage;
                            }
                        }
                        self.next
                            .cpu_usage
                            .insert(allocation.id.clone(), (since, samples));
                    }
                    if !prev.last_observed.contains_key(&allocation.id) {
                        self.first_observed_gpus
                            .extend(charge.gpu_memory_mib.keys().cloned());
                    }
                    // A requested device missing from the observation has unknown attribution:
                    // it is charged at max(request, last known) as unknown own usage, while CPU
                    // and RAM keep their readings.
                    let mut kept = observed.clone();
                    for (uuid, requested) in &allocation.requested.gpu_memory_mib {
                        if observed.gpu_memory_mib.contains_key(uuid) {
                            continue;
                        }
                        let last = prev
                            .last_observed
                            .get(&allocation.id)
                            .and_then(|last| last.gpu_memory_mib.get(uuid))
                            .copied();
                        let held = last.unwrap_or(0).max(*requested);
                        charge.gpu_memory_mib.insert(uuid.clone(), held);
                        let unknown = self
                            .unknown_charge
                            .gpu_memory_mib
                            .entry(uuid.clone())
                            .or_default();
                        *unknown = unknown.saturating_add(held);
                        if let Some(last) = last {
                            kept.gpu_memory_mib.insert(uuid.clone(), last);
                        }
                    }
                    self.next.last_observed.insert(allocation.id.clone(), kept);
                } else {
                    let last = prev.last_observed.get(&allocation.id);
                    charge = Resources::charge(&charge, last);
                    if let Some(last) = last {
                        self.next
                            .last_observed
                            .insert(allocation.id.clone(), last.clone());
                    }
                    // A missing observation keeps the admission window.
                    if let Some(window) = prev.cpu_usage.get(&allocation.id) {
                        self.next
                            .cpu_usage
                            .insert(allocation.id.clone(), window.clone());
                    }
                    self.unknown_charge.add_checked(&charge)?;
                    if last.is_none() {
                        self.unknown_new.add_checked(&charge)?;
                    } else if matches!(
                        allocation.usage_unknown.as_deref(),
                        Some("not_alive" | "not_verified")
                    ) {
                        // Its process is gone: the task has ended (FR-018's checks).
                        self.unknown_ended.add_checked(&charge)?;
                    }
                    self.unknown_allocations.insert(allocation.id.clone());
                    let check = allocation
                        .usage_unknown
                        .as_deref()
                        .map_or(String::new(), |check| format!(" ({check})"));
                    self.reasons.push(ReasonCode::new("allocation_unknown_hold", format!(
                        "allocation {} has unknown managed usage{check}; charged at max(request, last known), its step holds and affected admission is blocked",
                        allocation.id
                    )));
                }
            }
            self.charged_cpu_requests = self.charged_cpu_requests.saturating_add(
                charge
                    .cpu_millicores
                    .min(allocation.requested.cpu_millicores),
            );
            self.charged.add_checked(&charge)?;
            self.charges.insert(allocation.id.clone(), charge);
        }
        Ok(())
    }

    /// Each allocation's mode per resource kind: its request within the node cap,
    /// noted where the request is above the cap (FR-030).
    pub(super) fn resolve_modes(&mut self) {
        let (config, input) = (self.config, self.input);
        let cap = config.contention.max_mode();
        // Each allocation runs at min(requested, node cap).
        self.allocation_modes = input
            .allocations
            .iter()
            .map(|a| {
                let modes = ResourceKind::ALL
                    .into_iter()
                    .map(|kind| {
                        let mode = crate::contention::effective_mode(
                            config.contention.mode(kind),
                            a.modes.get(&kind).copied(),
                            cap,
                        );
                        (kind, mode)
                    })
                    .collect();
                (a.id.clone(), modes)
            })
            .collect();
        // A launch nice cannot be lowered again, so a task that left Yield First keeps it.
        for allocation in &input.allocations {
            let nice = input
                .facts
                .get(&allocation.id)
                .and_then(|facts| facts.launch_nice)
                .filter(|nice| *nice > 0);
            if let Some(nice) = nice
                && self.allocation_modes[&allocation.id][&ResourceKind::Cpu]
                    != ContentionMode::YieldFirst
            {
                note(
                    &mut self.statuses,
                    ResourceKey::Cpu,
                    "launch_nice_kept",
                    format!(
                        "allocation {} keeps its launch nice {nice} from Yield First; its caps do the yielding",
                        allocation.id
                    ),
                );
            }
        }
        // A requested mode above the node cap runs at the cap; say so (FR-030).
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
                        &mut self.statuses,
                        key,
                        "mode_capped",
                        format!(
                            "allocation {} requested {}; the node cap is {}, so it runs in {}",
                            allocation.id,
                            requested.name(),
                            cap.name(),
                            cap.name()
                        ),
                    );
                }
            }
        }
    }

    /// Holds for allocations of unknown usage, and the node-wide reasons: an
    /// operator drain and unknown capacity.
    pub(super) fn node_reasons(&mut self) {
        let (snapshot, input) = (self.snapshot, self.input);
        // In Performance and Aggressive, a measured task whose process is gone, as when it ends,
        // holds nothing: it is charged at max(request, last known), and admission starts the
        // next tasks as plain Linux would. Any other unknown usage, and CPU beside an account
        // whose CedeGrid work came first, holds as before (006 FR-007).
        let config = self.config;
        let plain = |kind: ResourceKind| governing(config, kind) >= ContentionMode::Performance;
        let cpu_unknown = if plain(ResourceKind::Cpu) && !self.cpu_first_comer_present() {
            self.unknown_charge
                .cpu_millicores
                .saturating_sub(self.unknown_ended.cpu_millicores)
        } else {
            self.unknown_charge.cpu_millicores
        };
        let ram_unknown = if plain(ResourceKind::Ram) {
            self.unknown_charge
                .ram_mib
                .saturating_sub(self.unknown_ended.ram_mib)
        } else {
            self.unknown_charge.ram_mib
        };
        self.cpu_held = cpu_unknown > 0;
        self.ram_held = ram_unknown > 0;
        for (key, held) in [
            (ResourceKey::Cpu, self.cpu_held),
            (ResourceKey::Ram, self.ram_held),
        ] {
            if held {
                note(
                    &mut self.statuses,
                    key,
                    "allocation_unknown_hold",
                    "an allocation's usage is unknown; it is charged at max(request, last known) and the resource holds".into(),
                );
            }
        }
        for gpu in self.unknown_charge.gpu_memory_mib.keys() {
            note(
                &mut self.statuses,
                ResourceKey::GpuMemory(gpu.clone()),
                "allocation_unknown_hold",
                "an allocation's usage is unknown; it is charged at max(request, last known) and the device holds".into(),
            );
        }
        self.unsafe_now = input.explicit_drain;
        if input.explicit_drain {
            self.reasons.push(ReasonCode::new(
                "operator_drain",
                "explicit node drain requested",
            ));
        }
        if snapshot.cpu_capacity_millicores == 0 {
            self.reasons.push(ReasonCode::new(
                "cpu_capacity_unknown",
                "CPU capacity unavailable; zero CPU scheduling budget",
            ));
        }
        if snapshot.total_ram_mib == 0 {
            self.reasons.push(ReasonCode::new(
                "ram_capacity_unknown",
                "RAM capacity unavailable; zero RAM scheduling budget",
            ));
        }
    }

    /// The Linux kernel snapshot when kernel diagnostics are on, which scopes
    /// CPU to the effective CPU set.
    fn scoped_kernel(&self) -> Option<&'a KernelSnapshot> {
        let (config, snapshot) = (self.config, self.snapshot);
        snapshot
            .kernel
            .as_ref()
            .filter(|kernel| config.kernel.enabled && kernel.runtime_os == "linux")
    }

    /// The CPU scope: with a scoped kernel snapshot, the effective CPU set's
    /// capacity, busy time, ancestor ceiling and physical-core reserve, and
    /// otherwise the system-wide figures. Invalid or stale scoped telemetry is
    /// unknown.
    pub(super) fn cpu_scope(&mut self) {
        let (config, snapshot, prev, now) = (self.config, self.snapshot, self.prev, self.now);
        let scoped_kernel = self.scoped_kernel();
        let scoped_reserve = if let Some(kernel) = scoped_kernel {
            let mut valid = scoped_cpu_valid(kernel, &mut self.reasons);
            match prev.last_kernel_cpu_sample {
                Some((sample, _)) if kernel.monotonic_elapsed_ms < sample => {
                    valid = false;
                    self.reasons.push(ReasonCode::new(
                        "kernel_cpu_stale",
                        "kernel CPU collector clock reset; establish a new baseline",
                    ));
                    self.next.last_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now));
                }
                Some((sample, received)) if kernel.monotonic_elapsed_ms == sample => {
                    if now.saturating_sub(received) > kernel.freshness_limit_ms {
                        valid = false;
                        self.reasons.push(ReasonCode::new(
                            "kernel_cpu_stale",
                            "kernel CPU snapshot is stale; repeated readings do not refresh it",
                        ));
                    }
                }
                _ => self.next.last_kernel_cpu_sample = Some((kernel.monotonic_elapsed_ms, now)),
            }
            let reserve = scoped_physical_reserve(&kernel.cpu, config.cpu.reserve_physical_cores);
            if reserve.is_none() {
                valid = false;
                self.reasons.push(ReasonCode::new("cpu_topology_unknown", "effective CPU topology unavailable; cannot map physical-core reserve within permitted CPUs"));
            }
            if !valid {
                self.cpu_unknown = true;
            }
            self.reasons.push(ReasonCode::new("cpu_scoped_budget", "CPU budget uses matching effective-CPU usage and hardware capacity; visible ancestor bandwidth is a separate ceiling, not a guaranteed share"));
            self.cpu_capacity = kernel.cpu.effective_cpu_capacity_millicores.unwrap_or(0);
            self.cpu_busy = kernel.cpu.busy_millicores;
            self.cpu_ceiling = kernel.cpu.visible_cpu_ceiling_millicores.unwrap_or(0);
            reserve
        } else {
            // Schema-v1 recordings and portable non-Linux collectors retain the
            // original system-scope arithmetic. Missing Linux scope data is not
            // replaced by an optimistic host aggregate when diagnostics are enabled.
            self.next.last_kernel_cpu_sample = None;
            self.cpu_capacity = snapshot.cpu_capacity_millicores;
            self.cpu_busy = snapshot.cpu_busy_millicores;
            self.cpu_ceiling = snapshot.cpu_capacity_millicores;
            None
        };
        self.cpu_reserve = if scoped_kernel.is_some() {
            scoped_reserve.unwrap_or(self.cpu_capacity)
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
            self.cpu_unknown = true;
            self.reasons.push(ReasonCode::new(
                "cpu_topology_unknown",
                "CPU topology unavailable; cannot establish configured physical-core reserve",
            ));
            snapshot.cpu_capacity_millicores
        };
    }

    /// External CPU, host busy time less own usage, and the CPU budget it leaves.
    /// Own usage beyond host busy time holds the CPU.
    pub(super) fn cpu_external(&mut self) {
        let (snapshot, input) = (self.snapshot, self.input);
        // The reading that resets the window leaves the estimate kept for its refill as it was.
        self.next.cpu_window_external = self.prev.cpu_window_external;
        let Some(busy) = self.cpu_busy else {
            self.cpu_unknown = true;
            self.reasons.push(ReasonCode::new(
                "cpu_usage_unknown",
                "CPU busy telemetry unavailable; CPU admission blocked",
            ));
            return;
        };
        // Own usage is a lower bound and host busy an upper bound (schema 3 adds
        // the host lower bound). Overlapping bounds are consistent (R9).
        let own_upper = input
            .allocations
            .iter()
            .filter_map(|a| a.observed.as_ref().map(|o| (a, o)))
            .map(|(a, o)| {
                a.observed_upper
                    .as_ref()
                    .map_or(o.cpu_millicores, |u| u.cpu_millicores.max(o.cpu_millicores))
            })
            .fold(0u64, u64::saturating_add);
        // The summed bound counts one tick of pending time per scope CPU rather
        // than per process; it stays within the per-allocation sums.
        let own_lower = snapshot.managed_cpu_lower_millicores.map_or(
            self.managed_observed.cpu_millicores,
            |summed| {
                summed
                    .min(own_upper)
                    .max(self.managed_observed.cpu_millicores)
            },
        );
        let host_lower = match self.scoped_kernel() {
            Some(kernel) => kernel.cpu.busy_lower_millicores,
            None => snapshot.cpu_busy_lower_millicores,
        }
        .unwrap_or(busy)
        .min(busy);
        let external = if own_lower > busy {
            self.cpu_held = true;
            self.cpu_held_inconsistent = true;
            self.statuses
                .entry(ResourceKey::Cpu)
                .or_default()
                .measurement = Some(Measurement::Unknown);
            self.reasons.push(ReasonCode::new("measurement_unknown_hold",
                "managed CPU observation exceeds busy observation in the selected scope beyond the known error; CPU holds and admission is blocked",
            ));
            note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "measurement_unknown_hold",
                format!("own CPU lower bound {own_lower}m exceeds host busy upper bound {busy}m"),
            );
            0
        } else {
            if own_lower > host_lower {
                self.statuses
                    .entry(ResourceKey::Cpu)
                    .or_default()
                    .measurement = Some(Measurement::InconsistentWithinTolerance);
                note(
                    &mut self.statuses,
                    ResourceKey::Cpu,
                    "inconsistent_within_tolerance",
                    format!(
                        "own CPU {own_lower}m exceeds host busy lower bound {host_lower}m within the known error; external clamped"
                    ),
                );
            }
            // CedeGrid's own CPU is on own work's side; beyond what host busy leaves, it is
            // clamped rather than making the reading inconsistent (FR-069).
            let own_charged = own_lower.saturating_add(self.unknown_charge.cpu_millicores);
            let cedegrid = snapshot
                .cedegrid_cpu_lower_millicores
                .unwrap_or(0)
                .min(busy.saturating_sub(own_charged));
            self.cedegrid_cpu = cedegrid;
            if snapshot.cedegrid_cpu_lower_millicores.is_some() {
                self.statuses.entry(ResourceKey::Cpu).or_default().cedegrid = Some(cedegrid);
            }
            let interval = busy.saturating_sub(own_charged.saturating_add(cedegrid));
            // Over the window, host busy and own CPU carry one bound for the whole window. Its
            // estimate sets the steady state while this interval's stays within its own bound
            // of it; beyond that bound, growth or fall, this interval's applies at once (FR-073).
            let window = self
                .scoped_kernel()
                .and_then(|kernel| kernel.cpu.busy_window_millicores)
                .zip(snapshot.managed_cpu_window_lower_millicores)
                .filter(|(busy, own)| own <= busy)
                .map(|(busy, own)| {
                    let cedegrid = snapshot
                        .cedegrid_cpu_window_lower_millicores
                        .unwrap_or(cedegrid);
                    busy.saturating_sub(
                        own.saturating_add(self.unknown_charge.cpu_millicores)
                            .saturating_add(cedegrid),
                    )
                });
            let bound = (busy - host_lower).saturating_add(own_upper.saturating_sub(own_lower));
            self.cpu_bound = bound;
            self.cpu_interval_external = Some(interval);
            // A reset window is refilling, and this interval's estimate alone carries its wide
            // bound, which is not external growth (FR-018): the last estimate stands, by the
            // window's rule, for at most one window.
            let window_ms = (crate::kernel::CPU_WINDOW_INTERVALS as u64)
                .saturating_mul(self.config.monitor.interval_ms);
            // Another user's measured usage is not looseness: the kept estimate gives way once
            // the other principals' measured CPU has risen by `principal_min_percent` of
            // capacity since it was taken. It is kept only while the list shows another
            // account's process with a readable user ID, then and now (FR-018).
            let others = snapshot
                .principals
                .as_ref()
                .filter(|list| list.iter().any(|p| !p.is_owner && p.uid.is_some()))
                .map(|list| {
                    list.iter()
                        .filter(|p| !p.is_owner)
                        .map(|p| p.cpu_millicores)
                        .fold(0u64, u64::saturating_add)
                });
            let rise = (self.cpu_capacity.saturating_mul(u64::from(
                self.config
                    .contention
                    .thresholds(crate::contention::ResourceKind::Cpu)
                    .principal_min_percent,
            )) / 100)
                .max(1);
            let recent = self
                .prev
                .cpu_window_external
                .filter(|(_, at, _)| self.now.saturating_sub(*at) <= window_ms);
            let kept = recent.filter(|(_, _, then)| match (then, others) {
                (Some(then), Some(now)) => now < then.saturating_add(rise),
                _ => false,
            });
            if window.is_none()
                && recent.is_some_and(|(_, _, then)| then.is_none() || others.is_none())
            {
                note(
                    &mut self.statuses,
                    ResourceKey::Cpu,
                    "cpu_window_estimate_not_kept",
                    "the principal list does not show another account's process, so no estimate is kept for the window's refill".into(),
                );
            }
            self.next.cpu_window_external =
                window.map(|window| (window, self.now, others)).or(kept);
            match window.or(kept.map(|(estimate, _, _)| estimate)) {
                Some(window) if window.abs_diff(interval) <= bound => window,
                _ => {
                    // Real growth beyond the bound ends what was kept.
                    if window.is_none() {
                        self.next.cpu_window_external = None;
                    }
                    interval
                }
            }
        };
        self.budget.cpu_millicores = self
            .cpu_capacity
            .saturating_sub(self.cpu_reserve)
            .saturating_sub(external)
            .min(self.cpu_ceiling);
        let status = self.statuses.entry(ResourceKey::Cpu).or_default();
        status.external = Some(external);
        status.own = Some((own_lower, own_upper));
    }

    /// RAM: the floor and its critical level, floor breaches and room, and the
    /// external usage and budget beside own usage.
    pub(super) fn measure_ram(&mut self) {
        let (config, snapshot) = (self.config, self.snapshot);
        let ram_reserve = config.ram_floor_mib(snapshot.total_ram_mib);
        self.ram_reserve = ram_reserve;
        let ram_floor = config.ram_floor(snapshot.total_ram_mib);
        if ram_floor.raised {
            note(
                &mut self.statuses,
                ResourceKey::Ram,
                "small_floor_raised",
                raised_note(&ram_floor, "RAM"),
            );
        }
        if snapshot.available_ram_mib.is_some_and(|free| {
            free < critical_level(config, ram_reserve, crate::config::MIN_RAM_FLOOR_MIB)
        }) {
            self.critical_keys.insert(ResourceKey::Ram);
        }
        if let Some(free) = snapshot.available_ram_mib {
            self.floor_room
                .insert(ResourceKey::Ram, free.saturating_sub(ram_reserve));
        }
        if let Some(free) = snapshot
            .available_ram_mib
            .filter(|free| *free < ram_reserve)
        {
            self.floor_breaches
                .insert(ResourceKey::Ram, (free, ram_reserve));
        }
        let Some(available) = snapshot.available_ram_mib else {
            self.ram_unknown = true;
            self.reasons.push(ReasonCode::new(
                "ram_usage_unknown",
                "RAM availability telemetry unavailable; RAM admission blocked",
            ));
            return;
        };
        let used = snapshot.total_ram_mib - available;
        let own_lower = self.managed_observed.ram_mib;
        let external = if own_lower > used {
            self.ram_held = true;
            self.statuses
                .entry(ResourceKey::Ram)
                .or_default()
                .measurement = Some(Measurement::Unknown);
            self.reasons.push(ReasonCode::new("measurement_unknown_hold",
                "managed RAM observation exceeds system used observation; RAM holds and admission is blocked",
            ));
            note(
                &mut self.statuses,
                ResourceKey::Ram,
                "measurement_unknown_hold",
                format!("own RAM lower bound {own_lower} MiB exceeds system used {used} MiB"),
            );
            0
        } else {
            used.saturating_sub(own_lower.saturating_add(self.unknown_charge.ram_mib))
        };
        self.budget.ram_mib = snapshot
            .total_ram_mib
            .saturating_sub(ram_reserve)
            .saturating_sub(external);
        let status = self.statuses.entry(ResourceKey::Ram).or_default();
        status.external = Some(external);
        status.own = Some((own_lower, own_lower));
    }

    /// Unknown CPU or RAM telemetry leaves no budget there, and any unknown or
    /// held measurement makes the node unsafe.
    pub(super) fn unknown_budgets(&mut self) {
        if self.cpu_unknown {
            self.budget.cpu_millicores = 0;
            self.cpu_kept_running = 0;
            self.admit_immediately_cpu = None;
            self.admit_immediately_plain = false;
            self.unsafe_now = true;
        }
        if self.ram_unknown {
            self.budget.ram_mib = 0;
            self.unsafe_now = true;
        }
        if self.cpu_held || self.ram_held {
            self.unsafe_now = true;
        }
    }

    /// Unmanaged usage by principal on one resource (research R12). The owner is
    /// always a principal; system accounts are background; other users join and
    /// leave after the principal window (FR-059).
    fn principals(
        &mut self,
        key: ResourceKey,
        capacity: u64,
        pick: &dyn Fn(&PrincipalUsage) -> u64,
    ) -> (Option<Vec<(Principal, u64)>>, u64) {
        let (config, snapshot, prev, now) = (self.config, self.snapshot, self.prev, self.now);
        let Some(list) = snapshot.principals.as_ref() else {
            // An unreadable list is not a departure: the counts stand as they were, so an
            // admitted user still counts when the list reads again (FR-077).
            for ((k, uid), count) in &prev.principal_counts {
                if k == &key {
                    self.next
                        .principal_counts
                        .insert((key.clone(), *uid), *count);
                }
            }
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
                    let mut count = prev
                        .principal_counts
                        .get(&(key.clone(), uid))
                        .copied()
                        .unwrap_or_default();
                    // A user whose demand is on record as satisfied is known:
                    // it rejoins at once, so the record applies on its return.
                    if usage >= floor
                        && prev.satisfied.get(&key).is_some_and(|records| {
                            records.iter().any(|(principal, _)| *principal == Some(uid))
                        })
                    {
                        count.admitted = true;
                    }
                    count.observe(usage, floor, window, now, leave_ms);
                    if count.admitted {
                        members.push((principal, usage));
                    }
                    if count.admitted || count.above > 0 {
                        self.next.principal_counts.insert((key.clone(), uid), count);
                    }
                }
            }
        }
        // A member no longer seen has no usage.
        for ((k, uid), count) in &prev.principal_counts {
            if k == &key && count.admitted && !seen.contains(uid) {
                let mut count = *count;
                count.observe(0, floor, window, now, leave_ms);
                if count.admitted {
                    self.next
                        .principal_counts
                        .insert((key.clone(), *uid), count);
                }
            }
        }
        (Some(members), background)
    }

    /// Satisfied records keep at most the most recently seen principals.
    fn bound_records(
        &mut self,
        key: &ResourceKey,
        view: &ModeInput,
        known: &mut Vec<(Option<u32>, u64)>,
    ) {
        let now = self.now;
        let mut seen = self
            .prev
            .principal_seen
            .get(key)
            .cloned()
            .unwrap_or_default();
        let mut accounted = view.background;
        for (principal, usage) in view.principals.iter().flatten() {
            accounted = accounted.saturating_add(*usage);
            if !principal.is_owner && *usage > 0 {
                seen.insert(principal.uid, now);
            }
        }
        if view.external > accounted {
            seen.insert(None, now);
        }
        crate::contention::bound_satisfied(known, &seen);
        seen.retain(|uid, _| known.iter().any(|(principal, _)| principal == uid));
        if !seen.is_empty() {
            self.next.principal_seen.insert(key.clone(), seen);
        }
    }

    /// Probe external demand on a compute resource (FR-021): record what is found
    /// satisfied and step own work down to a probe's target. A watch keeps own
    /// work at the fair share; only a probe sets a target.
    fn probe(&mut self, key: &ResourceKey, view: &mut ModeInput, thresholds: &ResourceThresholds) {
        let prev = self.prev;
        let response_ms =
            LadderTiming::new(thresholds, view.mode, self.config.monitor.interval_ms).response_ms;
        let (mut known, probe) = crate::contention::probe_demand(
            view,
            thresholds,
            prev.probes.get(key),
            response_ms,
            self.now,
        );
        self.bound_records(key, view, &mut known);
        view.satisfied = known.clone();
        view.probe = probe.as_ref().filter(|p| !p.watch).map(|p| p.target);
        note_demand(&mut self.statuses, key.clone(), &known, probe.as_ref());
        if !known.is_empty() {
            self.next.satisfied.insert(key.clone(), known);
        }
        if let Some(probe) = probe {
            self.next.probes.insert(key.clone(), probe);
        }
    }

    /// Whether the CPU is saturated: utilization at or above `saturation_percent`
    /// suppresses measured external usage, and external PSI is a second signal.
    /// Own and system-wide pressure never count, and unknown pressure adds
    /// nothing (FR-072).
    fn cpu_saturation(&mut self) -> bool {
        let (config, snapshot) = (self.config, self.snapshot);
        let cpu_total = self.cpu_capacity.min(self.cpu_ceiling);
        let saturation = u64::from(
            config
                .contention
                .thresholds(ResourceKind::Cpu)
                .saturation_percent,
        );
        let utilization_saturated = self
            .cpu_busy
            .is_some_and(|busy| busy.saturating_mul(100) >= cpu_total.saturating_mul(saturation));
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
        self.psi_only = psi_saturated && !utilization_saturated;
        match cpu_pressure.map(|pressure| pressure.status) {
            _ if self.psi_only => note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "saturated_by_psi",
                format!(
                    "external PSI some avg10 {:.1}% is above {psi_threshold}%; own and system-wide pressure are not counted",
                    external_psi.unwrap_or(0.0)
                ),
            ),
            Some(crate::kernel::CpuPressureStatus::SystemOnly) => note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "psi_system_only",
                "only system-wide CPU pressure is readable; it cannot tell own from external waiting, so it adds no evidence".into(),
            ),
            Some(crate::kernel::CpuPressureStatus::Unknown) => note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "psi_unknown",
                format!(
                    "CPU pressure is missing or unreadable, so it adds no evidence: {}",
                    cpu_pressure.map_or("", |pressure| pressure.detail.as_str())
                ),
            ),
            _ => {}
        }
        self.psi_saturated = psi_saturated;
        utilization_saturated || psi_saturated
    }

    /// CPU: the mode's target beside the contending principals, with probing,
    /// which bounds the budget.
    pub(super) fn cpu_target(&mut self) {
        let config = self.config;
        // Own tasks that stopped running since the last interval (006 FR-007a).
        let running: BTreeSet<String> = self
            .input
            .allocations
            .iter()
            .filter(|a| a.phase == AllocationPhase::Running)
            .map(|a| a.id.clone())
            .collect();
        if self.prev.own_running.iter().any(|id| !running.contains(id)) {
            self.next.own_exit_ms = Some(self.now);
        }
        self.next.own_running = running;
        let saturated = self.cpu_saturation();
        // CedeGrid's own CPU comes out of own tasks' budget and targets (FR-069).
        let cedegrid = self.cedegrid_cpu;
        if self.cpu_unknown || self.cpu_held {
            self.budget.cpu_millicores = self.budget.cpu_millicores.saturating_sub(cedegrid);
            self.next.cpu_quiet_intervals = self.prev.cpu_quiet_intervals;
            self.next.cpu_unattributed_intervals = self.prev.cpu_unattributed_intervals;
            self.next.cpu_list_over = self.prev.cpu_list_over.clone();
            self.carry_principals(&ResourceKey::Cpu);
            return;
        }
        self.floor_available.insert(
            ResourceKey::Cpu,
            self.budget.cpu_millicores.saturating_sub(cedegrid),
        );
        let capacity = self.cpu_capacity;
        let (mut members, background) =
            self.principals(ResourceKey::Cpu, capacity, &|p| p.cpu_millicores);
        // The agent and supervisors run as the owner: their CPU is not the owner's unmanaged use.
        for (principal, usage) in members.iter_mut().flatten() {
            if principal.is_owner {
                *usage = usage.saturating_sub(cedegrid);
            }
        }
        let thresholds = config.contention.thresholds(ResourceKind::Cpu);
        let status = self.statuses.get(&ResourceKey::Cpu);
        let mut view = ModeInput {
            kind: ResourceKind::Cpu,
            mode: governing(config, ResourceKind::Cpu),
            capacity,
            external: status.and_then(|s| s.external).unwrap_or(0),
            own: status
                .and_then(|s| s.own)
                .map_or(0, |(lower, _)| lower)
                .saturating_add(cedegrid),
            principals: members,
            background,
            saturated,
            psi_only: self.psi_only,
            unknown_activity: false,
            satisfied: self
                .prev
                .satisfied
                .get(&ResourceKey::Cpu)
                .cloned()
                .unwrap_or_default(),
            probe: None,
            fair_share_limit: self.fair_share_limit(&ResourceKey::Cpu),
            unprotected: false,
            unattributed_hold: 0,
        };
        // In Performance and Aggressive own work above its Neutral share can saturate the CPU
        // itself, which is no evidence of hidden external demand: utilization is judged with
        // own work at that share, so a satisfied record does not make the target cycle.
        if view.saturated && !self.psi_saturated && view.mode >= ContentionMode::Performance {
            let share = crate::contention::neutral_share(
                &ModeInput {
                    saturated: true,
                    fair_share_limit: None,
                    ..view.clone()
                },
                &thresholds,
            );
            let judged = self
                .cpu_busy
                .unwrap_or(0)
                .saturating_sub(view.own.saturating_sub(share));
            let total = self.cpu_capacity.min(self.cpu_ceiling);
            view.saturated = judged.saturating_mul(100)
                >= total.saturating_mul(u64::from(thresholds.saturation_percent));
        }
        self.note_fair_share(&ResourceKey::Cpu, &view, &thresholds);
        self.note_cpu_quiet(&view, &thresholds);
        view.unattributed_hold = self.unattributed_noise(&view);
        self.probe(&ResourceKey::Cpu, &mut view, &thresholds);
        // Own work keeps an equal share while the others keep theirs (user, 2026-11-17). Where
        // the agent runs in a CPU cgroup of the owner's own, the kernel splits CPU between users,
        // so CedeGrid and the owner's other processes share the owner's slice and the share is
        // counted on own tasks' requests alone; elsewhere both still come out of it.
        let owner_cgroup = owner_cpu_cgroup(self.snapshot);
        let owner_other: u64 = view
            .principals
            .iter()
            .flatten()
            .filter(|(principal, _)| principal.is_owner)
            .map(|(_, usage)| *usage)
            .sum();
        let requests_only = owner_cgroup.then(|| ModeInput {
            principals: view.principals.as_ref().map(|list| {
                list.iter()
                    .map(|(principal, usage)| {
                        (*principal, if principal.is_owner { 0 } else { *usage })
                    })
                    .collect()
            }),
            ..view.clone()
        });
        let schedulable = self
            .cpu_capacity
            .saturating_sub(self.cpu_reserve)
            .min(self.cpu_ceiling);
        let kept_with = |room: u64| {
            let kept = crate::contention::kept_share(
                requests_only.as_ref().unwrap_or(&view),
                &thresholds,
                schedulable,
                room,
            );
            if owner_cgroup {
                kept
            } else {
                kept.saturating_sub(cedegrid)
            }
        };
        // The owner's other processes leave the room they use in the owner's slice.
        let kept = kept_with(if owner_cgroup {
            self.budget.cpu_millicores.saturating_add(owner_other)
        } else {
            self.budget.cpu_millicores
        });
        // Running own work keeps the equal share whatever room the others leave: room it left
        // by running below the share is not a principal's to keep, so a principal that takes
        // all it gets cannot hold it below its share (SC-003). Admission still gets only what
        // is left. With the principal list unreadable no principal is known to have taken it,
        // so running work keeps what the room leaves, as admission does.
        let kept_running = if self.snapshot.principals.is_some() {
            kept_with(schedulable)
        } else {
            kept
        };
        self.cpu_kept_running = kept_running;
        let mut targets = targets_of(&view, &thresholds, self.budget.cpu_millicores);
        for (mode, target) in targets.iter_mut() {
            *target = target.saturating_sub(cedegrid);
            if *mode != ContentionMode::YieldFirst {
                *target = (*target).max(kept_running);
            }
        }
        self.mode_targets.insert(ResourceKey::Cpu, targets);
        // The idle reserve is the larger of the explicit reserve and the mode's
        // margin of total capacity; the two never stack.
        self.budget.cpu_millicores = own_allowance(&view, &thresholds)
            // Capacity minus the reserve and external usage, within the ancestor quota.
            .min(self.budget.cpu_millicores)
            .saturating_sub(cedegrid)
            .max(kept);
        self.first_comer_cpu(&view, &thresholds, saturated);
    }

    /// Performance and Aggressive hold CPU admission only for the accounts that arrived first,
    /// and admit immediately only for the protected ones among them (006 FR-007, FR-008,
    /// FR-013); each decision says which it did (FR-020).
    fn first_comer_cpu(
        &mut self,
        view: &ModeInput,
        thresholds: &ResourceThresholds,
        saturated_now: bool,
    ) {
        let own = self.own_arrival(&|_| true);
        let standings = self.standings(own, &|p| p.cedegrid_cpu);
        let known = standings.as_ref();
        let first = |protected: bool| {
            move |uid: u32| {
                known.is_none_or(|standings| {
                    standings
                        .get(&uid)
                        .is_some_and(|s| s.first_comer && (s.protected || !protected))
                })
            }
        };
        let others = crate::contention::contending_principals(view, thresholds);
        let mut held = None;
        if view.mode >= ContentionMode::Performance {
            let (budget, uids) = self.cpu_budget_for(view, thresholds, &first(false));
            self.budget.cpu_millicores = budget;
            held = uids;
        }
        self.admit_immediately_cpu = None;
        self.admit_immediately_plain = false;
        if self.config.contention.max_mode() >= ContentionMode::Performance {
            let (budget, protected) = self.cpu_budget_for(
                &ModeInput {
                    mode: view.mode.max(ContentionMode::Performance),
                    ..view.clone()
                },
                thresholds,
                &first(true),
            );
            self.admit_immediately_plain = protected.is_none();
            if let Some(protected) = protected.filter(|uids| !uids.is_empty()) {
                note(
                    &mut self.statuses,
                    ResourceKey::Cpu,
                    "admit_immediately_ignored_for_protected_first_comer",
                    format!(
                        "admit immediately does not lift the CPU hold for {}, which arrived first with Performance or Aggressive work",
                        uid_list(&protected)
                    ),
                );
            }
            self.admit_immediately_cpu = Some(budget.max(self.budget.cpu_millicores));
        }
        // The rows show every account at the contention floor this interval, admitted as a
        // principal or not yet.
        let floor = (view
            .capacity
            .saturating_mul(u64::from(thresholds.principal_min_percent))
            / 100)
            .max(1);
        let in_scope = self.cpu_in_scope(view);
        let contending: BTreeSet<u32> = self
            .snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| !p.is_owner && in_scope(p.cpu_millicores) >= floor)
            .filter_map(|p| p.uid)
            .collect();
        let held_set: BTreeSet<u32> = held.iter().flatten().copied().collect();
        // A plain principal contends: an account with no CedeGrid work on the CPU, or usage
        // that no readable account accounts for. Accounts below uid_min are background.
        let uid_min = self.config.contention.uid_min;
        // CPU no account accounts for right after own tasks stop is their last interval's
        // usage, read in the host's busy time after the own reading dropped: no principal.
        let exit_recent = self.next.own_exit_ms.is_some_and(|at| {
            self.now.saturating_sub(at) <= self.config.monitor.interval_ms.saturating_mul(2)
        });
        // A plain principal contends only on a saturated CPU. A named account counts on a CPU
        // full this interval: own work above its half share does not hide it, as the judged
        // saturation of Performance would until pressure builds. Usage no account accounts for
        // needs the longer evidence: the judged saturation and the principal window, as 001
        // counts it (FR-034).
        let unattributed = view.saturated
            && !exit_recent
            && self.next.cpu_unattributed_intervals
                >= self.config.contention.principal_window_intervals
            && self.cpu_unattributed >= floor.max(self.cpu_bound);
        // The most a named plain account uses, when one reaches the contention floor.
        let named_plain = self
            .snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| {
                !p.is_owner
                    && match p.uid {
                        None => true,
                        Some(uid) => {
                            uid >= uid_min
                                && standings
                                    .as_ref()
                                    .is_some_and(|s| s.get(&uid).is_none_or(|s| s.work.is_none()))
                        }
                    }
            })
            .map(|p| in_scope(p.cpu_millicores))
            .filter(|usage| *usage >= floor)
            .max();
        let plain_contends = unattributed || saturated_now && named_plain.is_some();
        // The flag needs a plain principal's contention for a few intervals in a row and holds
        // a while after it ends, so one reading does not swing admission either way.
        let open = view.mode >= ContentionMode::Performance && held.is_none();
        let streak = if open && plain_contends {
            self.prev.beside_plain_streak.saturating_add(1)
        } else {
            0
        };
        self.next.beside_plain_streak = streak;
        // A named account taking at least a whole CPU competes as soon as own work fills the
        // rest: the flag rises on that first reading, before own work has started, so a batch
        // starts at once beside it, as plain Linux starts it, rather than in two steps.
        let whole_cpu = named_plain.is_some_and(|usage| usage >= crate::policy::ONE_CPU_MILLICORES);
        if streak >= crate::policy::BESIDE_PLAIN_INTERVALS || open && whole_cpu {
            self.next.beside_plain_ms = Some(self.now);
        }
        self.cpu_plain_admission = open;
        self.statuses
            .entry(ResourceKey::Cpu)
            .or_default()
            .plain_admission = open;
        self.cpu_beside_plain = open
            && self.next.beside_plain_ms.is_some_and(|at| {
                self.now.saturating_sub(at) <= crate::policy::BESIDE_PLAIN_HOLD_MS
            });
        let status = self.statuses.entry(ResourceKey::Cpu).or_default();
        if let OwnArrival::At(at) = own {
            status.own_arrival_unix_ms = Some(at);
        }
        status.principals = standing_rows(standings.as_ref(), &contending, &held_set);
        if view.mode < ContentionMode::Performance || others.is_empty() {
            return;
        }
        match held {
            None => note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "admitted_as_plain",
                "CPU admission starts new work as plain Linux would: no account that arrived first with CedeGrid work contends".into(),
            ),
            Some(held) => note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "held_for_first_comer",
                format!(
                    "CPU admission holds at the fair share for {}, which arrived first with CedeGrid work",
                    if held.is_empty() {
                        "every account, since the account list cannot be read".to_string()
                    } else {
                        uid_list(&held)
                    }
                ),
            ),
        }
    }

    /// While a resource is unknown or held, its principals, satisfied records and probes
    /// stand as they were.
    fn carry_principals(&mut self, key: &ResourceKey) {
        let prev = self.prev;
        for ((k, uid), count) in &prev.principal_counts {
            if k == key {
                self.next.principal_counts.insert((k.clone(), *uid), *count);
            }
        }
        if let Some(records) = prev.satisfied.get(key) {
            self.next.satisfied.insert(key.clone(), records.clone());
        }
        if let Some(probe) = prev.probes.get(key) {
            self.next.probes.insert(key.clone(), probe.clone());
        }
        if let Some(seen) = prev.principal_seen.get(key) {
            self.next.principal_seen.insert(key.clone(), seen.clone());
        }
    }

    /// Counts the intervals in which every external CPU principal stayed below
    /// `principal_min_percent`: readable users each, background left out, and what no
    /// readable principal accounts for as one more (FR-034).
    fn note_cpu_quiet(&mut self, view: &ModeInput, thresholds: &ResourceThresholds) {
        let floor = (view
            .capacity
            .saturating_mul(u64::from(thresholds.principal_min_percent))
            / 100)
            .max(1);
        // This interval's estimate, not the window's, beside the larger of the floor and the
        // scope's per-interval measurement band (FR-034).
        let external = self.cpu_interval_external.unwrap_or(view.external);
        let mut unattributed = external;
        let quiet = match self.snapshot.principals.as_ref() {
            Some(list) => {
                let mut accounted = 0u64;
                let mut contending = Vec::new();
                for principal in list {
                    let usage = if principal.is_owner {
                        principal.cpu_millicores.saturating_sub(self.cedegrid_cpu)
                    } else {
                        principal.cpu_millicores
                    };
                    match principal.uid {
                        Some(uid)
                            if !principal.is_owner && uid >= self.config.contention.uid_min =>
                        {
                            contending.push(usage);
                        }
                        None if !principal.is_owner => continue,
                        _ => {}
                    }
                    accounted = accounted.saturating_add(usage);
                }
                self.cpu_listed = Some(accounted);
                unattributed = external.saturating_sub(accounted);
                crate::contention::cpu_quiet(contending, unattributed, floor, self.cpu_bound)
            }
            None => crate::contention::cpu_quiet([], external, floor, self.cpu_bound),
        };
        self.cpu_unattributed = unattributed;
        // How far the list read above the scope: its own error, kept for four principal
        // windows so a gap it leaves the other way within that error is not taken for a
        // principal (FR-021, SC-003).
        let keep_ms = u64::from(self.config.contention.principal_window_intervals)
            .saturating_mul(4)
            .saturating_mul(self.config.monitor.interval_ms);
        let now = self.now;
        self.next.cpu_list_over = self
            .prev
            .cpu_list_over
            .iter()
            .filter(|(at, _)| now.saturating_sub(*at) < keep_ms)
            .copied()
            .collect();
        if let Some(accounted) = self.cpu_listed {
            self.next
                .cpu_list_over
                .push_back((now, accounted.saturating_sub(external)));
        }
        self.next.cpu_unattributed_intervals = if unattributed >= floor.max(self.cpu_bound) {
            self.prev.cpu_unattributed_intervals.saturating_add(1)
        } else {
            0
        };
        self.next.cpu_quiet_intervals = if quiet {
            self.prev.cpu_quiet_intervals.saturating_add(1)
        } else {
            0
        };
        let window = self.config.contention.principal_window_intervals;
        self.cpu_contention_ended = self.next.cpu_quiet_intervals >= window;
    }

    /// Usage no readable account accounts for that stands for no principal yet: no more than
    /// the account list's own error, how far it read above the scope in recent intervals or
    /// the scope's measurement band, and not lasting the principal window. A larger gap, or
    /// one that lasts, is one more principal at once, as before; an account listed but not
    /// yet admitted is not part of the gap (FR-021, SC-003).
    fn unattributed_noise(&self, view: &ModeInput) -> u64 {
        let Some(listed) = self.cpu_listed else {
            return 0;
        };
        let window = self.config.contention.principal_window_intervals;
        if self.next.cpu_unattributed_intervals >= window {
            return 0;
        }
        let error = self
            .next
            .cpu_list_over
            .iter()
            .map(|(_, over)| *over)
            .max()
            .unwrap_or(0)
            .max(self.cpu_bound);
        let gap = view.external.saturating_sub(listed);
        if gap <= error { gap } else { 0 }
    }

    /// RAM: the mode's target beside the contending principals.
    pub(super) fn ram_target(&mut self) {
        if self.ram_unknown || self.ram_held {
            self.next.ram_fair_share = self.prev.ram_fair_share;
            self.next.ram_unattributed = self.prev.ram_unattributed;
            self.carry_principals(&ResourceKey::Ram);
            // While RAM is held its availability is still known, so a floor breach is real
            // and in-share own memory keeps its protection at the last fair share (FR-023).
            if !self.ram_unknown
                && let Some(fair) = self.prev.ram_fair_share
            {
                self.memory_fair_share.insert(ResourceKey::Ram, fair);
            }
            return;
        }
        let (config, snapshot) = (self.config, self.snapshot);
        self.floor_available
            .insert(ResourceKey::Ram, self.budget.ram_mib);
        let capacity = snapshot.total_ram_mib.saturating_sub(self.ram_reserve);
        let (members, background) = self.principals(ResourceKey::Ram, capacity, &|p| p.ram_mib);
        let status = self.statuses.get(&ResourceKey::Ram);
        let view = ModeInput {
            kind: ResourceKind::Ram,
            mode: governing(config, ResourceKind::Ram),
            capacity,
            external: status.and_then(|s| s.external).unwrap_or(0),
            own: status.and_then(|s| s.own).map_or(0, |(lower, _)| lower),
            principals: members,
            background,
            saturated: false,
            psi_only: false,
            unknown_activity: false,
            satisfied: vec![],
            probe: None,
            fair_share_limit: self.fair_share_limit(&ResourceKey::Ram),
            unprotected: false,
            unattributed_hold: 0,
        };
        let thresholds = config.contention.thresholds(ResourceKind::Ram);
        self.note_fair_share(&ResourceKey::Ram, &view, &thresholds);
        self.note_ram_unattributed(&view, &thresholds);
        self.mode_targets
            .insert(ResourceKey::Ram, targets_of(&view, &thresholds, u64::MAX));
        self.budget.ram_mib = own_allowance(&view, &thresholds);
        let fair = crate::contention::neutral_share(&view, &thresholds);
        self.memory_fair_share.insert(ResourceKey::Ram, fair);
        self.next.ram_fair_share = Some(fair);
    }

    /// The cap on own work's fair share while the principal list cannot be read: the last one a
    /// readable list gave, if any (FR-059).
    fn fair_share_limit(&self, key: &ResourceKey) -> Option<u64> {
        self.snapshot
            .principals
            .is_none()
            .then(|| self.prev.fair_share_readable.get(key).copied())
            .flatten()
    }

    /// A readable list's share is kept for a later unreadable one, on compute the share under
    /// contention, since only that one is read there; an unreadable list notes when it holds the
    /// share down (FR-059).
    fn note_fair_share(
        &mut self,
        key: &ResourceKey,
        view: &ModeInput,
        thresholds: &ResourceThresholds,
    ) {
        let uncapped = |saturated: bool| {
            crate::contention::neutral_share(
                &ModeInput {
                    saturated,
                    fair_share_limit: None,
                    ..view.clone()
                },
                thresholds,
            )
        };
        if self.snapshot.principals.is_some() {
            let compute = matches!(key.kind(), ResourceKind::Cpu | ResourceKind::GpuCompute);
            self.next
                .fair_share_readable
                .insert(key.clone(), uncapped(view.saturated || compute));
        } else if let Some(limit) = view.fair_share_limit
            && uncapped(view.saturated) > limit
        {
            note(
                &mut self.statuses,
                key.clone(),
                "fair_share_held",
                format!(
                    "the principal list cannot be read; own work's fair share stays at {limit}"
                ),
            );
        }
    }

    /// Counts the intervals in which RAM no process lists has stayed `principal_min_percent`
    /// above its level. Kernel memory sits in it for good, so the level is the baseline, and a
    /// rise that lasts past the window becomes the next level (FR-034).
    fn note_ram_unattributed(&mut self, view: &ModeInput, thresholds: &ResourceThresholds) {
        let floor = (view
            .capacity
            .saturating_mul(u64::from(thresholds.principal_min_percent))
            / 100)
            .max(1);
        let listed: u64 = self
            .snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| p.is_owner || p.uid.is_some())
            .map(|p| p.ram_mib)
            .sum();
        let unattributed = view.external.saturating_sub(listed);
        let (level, intervals) = self.prev.ram_unattributed.unwrap_or((unattributed, 0));
        let window = self.config.contention.principal_window_intervals;
        self.next.ram_unattributed = Some(if unattributed < level.saturating_add(floor) {
            (level.min(unattributed), 0)
        } else if intervals >= window {
            (unattributed, 0)
        } else {
            (level, intervals + 1)
        });
    }

    /// The operator envelope bounds own work per resource (FR-057). Each floor,
    /// and the target share where own work has room, goes into the status.
    pub(super) fn envelope_and_floors(&mut self) {
        if let Some(envelope) = &self.input.envelope {
            self.budget.cpu_millicores = self.budget.cpu_millicores.min(envelope.cpu_millicores);
            self.admit_immediately_cpu = self
                .admit_immediately_cpu
                .map(|cpu| cpu.min(envelope.cpu_millicores));
            self.budget.ram_mib = self.budget.ram_mib.min(envelope.ram_mib);
            for (key, limit) in [
                (ResourceKey::Cpu, envelope.cpu_millicores),
                (ResourceKey::Ram, envelope.ram_mib),
            ] {
                for target in self
                    .mode_targets
                    .get_mut(&key)
                    .into_iter()
                    .flat_map(|t| t.values_mut())
                {
                    *target = (*target).min(limit);
                }
            }
        }
        for (key, floor) in [
            (ResourceKey::Cpu, self.cpu_reserve),
            (ResourceKey::Ram, self.ram_reserve),
        ] {
            let status = self.statuses.entry(key.clone()).or_default();
            status.floor = Some(floor);
            if self.floor_available.contains_key(&key) {
                status.target_share = Some(match key {
                    ResourceKey::Cpu => self.budget.cpu_millicores,
                    _ => self.budget.ram_mib,
                });
            }
        }
    }

    /// GPU devices: admission, the compute path and budget, and the memory budget
    /// of each device. Without an inventory, or for an allocated device missing
    /// from it, GPU admission is blocked.
    pub(super) fn observe_gpus(&mut self) {
        let snapshot = self.snapshot;
        if snapshot.gpu_inventory != CapabilityStatus::Available {
            self.reasons.push(ReasonCode::new(
                "gpu_inventory_unavailable",
                format!(
                    "GPU inventory {:?}; GPU admission blocked, CPU-only capacity remains eligible",
                    snapshot.gpu_inventory
                ),
            ));
            for gpu in self.charged.gpu_memory_mib.keys() {
                self.budget.gpu_memory_mib.insert(gpu.clone(), 0);
                self.blocked_gpus.insert(gpu.clone());
                self.gpu_memory_unknown.insert(gpu.clone());
                self.unsafe_now = true;
            }
            return;
        }
        for gpu in &snapshot.gpus {
            self.observe_gpu(gpu);
        }
        for gpu in self.charged.gpu_memory_mib.keys() {
            if !snapshot.gpus.iter().any(|item| &item.uuid == gpu) {
                self.budget.gpu_memory_mib.insert(gpu.clone(), 0);
                self.blocked_gpus.insert(gpu.clone());
                self.gpu_memory_unknown.insert(gpu.clone());
                self.unsafe_now = true;
                self.reasons.push(ReasonCode::new(
                    "gpu_absent",
                    format!("allocated GPU {gpu} is absent from the current inventory"),
                ));
            }
        }
    }

    /// One device: its memory events, admission, compute path and budget, and
    /// memory budget, and what the next interval compares against.
    fn observe_gpu(&mut self, gpu: &GpuSnapshot) {
        let previous = self
            .prev
            .previous_gpus
            .get(&gpu.uuid)
            .cloned()
            .unwrap_or_default();
        let memory = self.gpu_memory(gpu, &previous);
        let (activity, compute_admission, telemetry_unknown) =
            self.gpu_admission(gpu, &previous, &memory);
        let (measured, measured_streak) = self.gpu_compute_path(gpu, &previous);
        let (estimate, mut demand_record) = self.gpu_demand(gpu, measured);
        demand_record.source = match (&estimate, governing(self.config, ResourceKind::GpuCompute)) {
            (None, _) => GpuDemandSource::Interval,
            (Some(_), ContentionMode::YieldFirst) => GpuDemandSource::Larger,
            (Some(_), _) => GpuDemandSource::Estimate,
        };
        if telemetry_unknown {
            self.gpu_memory_unknown.insert(gpu.uuid.clone());
        }
        demand_record.first_come = self.note_arrivals(gpu).map(str::to_string);
        let mut time_slice = None;
        let mut slice_admission_held = false;
        let mut slice_held_by_own_new = false;
        if !telemetry_unknown {
            let mut budget = self.compute_budget_for(gpu, measured, activity, estimate.as_ref());
            // Performance and Aggressive hold GPU admission only for the accounts that arrived
            // first, and admit immediately only for the protected ones (006 FR-008, FR-013).
            let own = self.own_arrival(&|a| a.requested.gpu_memory_mib.contains_key(&gpu.uuid));
            let standings = self.standings(own, &|p| p.cedegrid_gpu.get(&gpu.uuid).copied());
            let known = standings.as_ref();
            let first = |protected: bool| {
                move |uid: Option<u32>| {
                    known.is_none_or(|standings| {
                        uid.and_then(|uid| standings.get(&uid))
                            .is_some_and(|s| s.first_comer && (s.protected || !protected))
                    })
                }
            };
            let mode = governing(self.config, ResourceKind::GpuCompute);
            // The record and the hold of running tasks count every principal: a Yield First or
            // Neutral job keeps its give-back on any node (FR-010).
            let full = self.time_slice(gpu, measured.is_some(), budget.unattributed, None);
            let slice = full.as_ref().map(|(slice, _, _)| slice.clone());
            let mut next_held = full.as_ref().map(|(_, next, _)| *next);
            let mut own_new = full.as_ref().is_some_and(|(_, _, own_new)| *own_new);
            let mut bound = slice.as_ref().is_some_and(|slice| !slice.bound.is_empty());
            if mode >= ContentionMode::Performance {
                let first_comers = first(false);
                let held = self.time_slice(
                    gpu,
                    measured.is_some(),
                    budget.unattributed,
                    Some(&first_comers),
                );
                bound = held
                    .as_ref()
                    .is_some_and(|(slice, _, _)| !slice.bound.is_empty());
                own_new = held.as_ref().is_some_and(|(_, _, own_new)| *own_new);
                next_held = held.map(|(_, next, _)| next);
            }
            if self.config.contention.max_mode() >= ContentionMode::Performance {
                let protected = first(true);
                // A hold that comes only from own work placed and not yet listed keeps the device,
                // as admission does, so that work's own authorization fits the budget (FR-028).
                if let Some((slice, next, own_new)) = self.time_slice(
                    gpu,
                    measured.is_some(),
                    budget.unattributed,
                    Some(&protected),
                ) && (!slice.bound.is_empty() || next && !own_new)
                {
                    self.admit_immediately_closed.insert(gpu.uuid.clone());
                    let uids: Vec<u32> = slice.bound.iter().filter_map(|b| b.uid).collect();
                    note(
                        &mut self.statuses,
                        ResourceKey::GpuCompute(gpu.uuid.clone()),
                        "admit_immediately_ignored_for_protected_first_comer",
                        format!(
                            "admit immediately does not lift the GPU hold for {}, which arrived first with Performance or Aggressive work",
                            if uids.is_empty() {
                                "the accounts listed there".to_string()
                            } else {
                                uid_list(&uids)
                            }
                        ),
                    );
                }
            }
            self.gpu_standings(
                gpu,
                own,
                known,
                mode,
                slice.as_ref(),
                bound || next_held == Some(true),
            );
            // No account whose CedeGrid work came first is on the device: the time-slice
            // admission rule does not apply in Performance and Aggressive (006 FR-007).
            self.statuses
                .entry(ResourceKey::GpuCompute(gpu.uuid.clone()))
                .or_default()
                .plain_admission = mode >= ContentionMode::Performance
                && known.is_some_and(|standings| !standings.values().any(|s| s.first_comer));
            time_slice = slice;
            if let Some(demand) = self.next.gpu_demand.get_mut(&gpu.uuid) {
                match gpu.time_sliced {
                    Some(sliced) => demand.last_time_sliced = sliced,
                    // An unknown listing on a device last read as time-sliced: no pause (Q29).
                    None if demand.last_time_sliced => {
                        self.no_gpu_pause.insert(gpu.uuid.clone());
                    }
                    None => {}
                }
            }
            // No mode sets a duty: a slice-bound principal holds GPU admission in every mode, and
            // in Yield First and Neutral own tasks there are held by whole tasks (FR-021, FR-089).
            slice_admission_held = bound || next_held == Some(true);
            slice_held_by_own_new = !bound && next_held == Some(true) && own_new;
            if let Some(hold) = time_slice.as_ref().and_then(|slice| slice.hold) {
                self.slice_hold.insert(gpu.uuid.clone(), hold);
                note_reason(&mut self.statuses, &gpu.uuid, "gpu_time_slice_hold");
            }
            // Under the time-slice rule, or with no slice-bound principal, Neutral, Performance
            // and Aggressive hold the device; Yield First keeps its target (FR-021, Q22, Q27).
            if time_slice
                .as_ref()
                .is_some_and(|slice| slice.holds || slice.hold.is_none())
            {
                self.time_slice_holds.insert(gpu.uuid.clone());
                if governing(self.config, ResourceKind::GpuCompute) != ContentionMode::YieldFirst {
                    budget.share = GPU_COMPUTE_CAPACITY;
                }
                for (mode, target) in budget.targets.iter_mut() {
                    if *mode != ContentionMode::YieldFirst {
                        *target = GPU_COMPUTE_CAPACITY;
                    }
                }
                let code = if time_slice.as_ref().is_some_and(|slice| slice.holds) {
                    "gpu_time_slice_fair"
                } else {
                    "gpu_time_slice_unbound"
                };
                note_reason(&mut self.statuses, &gpu.uuid, code);
            }
            // Own work that came first keeps at least the floor in Yield First and Neutral
            // (FR-090).
            if let Some(floor) = self.first_come_share(gpu) {
                for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
                    if let Some(target) = budget.targets.get_mut(&mode) {
                        *target = (*target).max(floor);
                    }
                }
                if governing(self.config, ResourceKind::GpuCompute) < ContentionMode::Performance {
                    budget.share = budget.share.max(floor);
                }
                note_reason(&mut self.statuses, &gpu.uuid, "gpu_first_come_floor");
            }
            self.gpu_compute_budget.insert(gpu.uuid.clone(), budget);
        }
        let compute_status = self
            .statuses
            .entry(ResourceKey::GpuCompute(gpu.uuid.clone()))
            .or_default();
        compute_status.activity = Some(activity);
        compute_status.gpu_demand = Some(demand_record);
        compute_status.time_slice = time_slice;
        if let Some((external, own)) = measured {
            compute_status.external = Some(external);
            compute_status.own = Some((own, own));
        }
        compute_status.target_share = self
            .gpu_compute_budget
            .get(&gpu.uuid)
            .map(|budget| budget.share);
        compute_status.admission = Some(if telemetry_unknown {
            Admission::held("gpu_observation_unknown")
        } else if slice_admission_held {
            Admission::held("gpu_time_slice_admission_held")
        } else {
            compute_admission
        });
        // Held only for one more own process beyond tasks placed and not yet started: their
        // authorization may proceed, and placement still waits (FR-028).
        compute_status.admission_own_new =
            !telemetry_unknown && slice_admission_held && slice_held_by_own_new;
        let gpu_budget = self.gpu_memory_budget(gpu, &memory, telemetry_unknown);
        self.budget
            .gpu_memory_mib
            .insert(gpu.uuid.clone(), gpu_budget);
        let status = self
            .statuses
            .entry(ResourceKey::GpuMemory(gpu.uuid.clone()))
            .or_default();
        status.external = memory.external;
        status.own = Some((memory.observed, memory.observed));
        status.activity = Some(gpu.external_compute);
        status.unattributed = gpu.unattributed_memory_mib;
        if telemetry_unknown {
            status.measurement = Some(Measurement::Unknown);
        }
        let stable = !self.blocked_gpus.contains(&gpu.uuid) && !self.input.explicit_drain;
        self.next.previous_gpus.insert(
            gpu.uuid.clone(),
            PreviousGpu {
                pids: memory.pids,
                external_memory: memory.next_baseline,
                sample_id: gpu.compute_sample_id.max(previous.sample_id),
                // A device's memory stability depends on that device alone,
                // never on CPU or RAM (FR-034). NVML reports no device whose
                // memory is host RAM, so no device keeps a RAM coupling.
                stable_since: if stable {
                    Some(previous.stable_since.unwrap_or(self.now))
                } else {
                    None
                },
                measured_streak,
            },
        );
    }

    /// A device's memory this interval: own and external usage, the budget
    /// beside the floor, and whether a new external process or growth beyond
    /// the noise tolerance appeared.
    fn gpu_memory(&mut self, gpu: &GpuSnapshot, previous: &PreviousGpu) -> DeviceMemory {
        let config = self.config;
        let key = ResourceKey::GpuMemory(gpu.uuid.clone());
        // No growth or new-PID event without a prior observation of this device
        // by this engine; an allocation's first observation is a baseline too.
        let device_first = !self.prev.previous_gpus.contains_key(&gpu.uuid);
        let baseline = device_first || self.first_observed_gpus.contains(&gpu.uuid);
        let observed = self
            .managed_observed
            .gpu_memory_mib
            .get(&gpu.uuid)
            .copied()
            .unwrap_or(0);
        let unknown_own = self
            .unknown_charge
            .gpu_memory_mib
            .get(&gpu.uuid)
            .copied()
            .unwrap_or(0);
        let current_charge = self
            .charged
            .gpu_memory_mib
            .get(&gpu.uuid)
            .copied()
            .unwrap_or(0);
        let external = gpu
            .used_memory_mib
            .map(|used| used.saturating_sub(observed).saturating_sub(unknown_own));
        let budget = external
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
        let new_external = !device_first && pids.difference(&previous.pids).next().is_some();
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
                            &mut self.statuses,
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
                    &mut self.statuses,
                    key.clone(),
                    "first_observation_baseline",
                    format!("{current} MiB not attributed to own work is the baseline, not growth"),
                );
                (false, Some(current))
            }
            (None, old) => (false, old),
        };
        DeviceMemory {
            observed,
            unknown_own,
            external,
            budget,
            pids,
            protection_event: new_external || memory_growth,
            breached: current_charge > budget,
            next_baseline,
        }
    }

    /// A device's compute activity and admission, and why its GPU admission is
    /// blocked, if it is. Returns the activity, the compute admission and whether
    /// the device's telemetry is unknown.
    fn gpu_admission(
        &mut self,
        gpu: &GpuSnapshot,
        previous: &PreviousGpu,
        memory: &DeviceMemory,
    ) -> (ComputeActivity, Admission, bool) {
        let config = self.config;
        // Distinct IDs can indicate fresh samples; repeated values never add evidence.
        let fresh_sample = gpu
            .compute_sample_id
            .is_some_and(|sample| previous.sample_id.is_none_or(|previous| sample > previous));
        // A listed context without a fresh sample is unknown, unless the collector found it
        // background (FR-021) or quiet: an idle context returns no sample, and Quiet is the
        // collector's own evidence over its window, so asking for a sample turned it into
        // Unknown until some process produced one (FR-010).
        let activity_unknown = gpu.external_compute == ComputeActivity::Unknown
            || (gpu.external_compute != ComputeActivity::Quiet
                && memory
                    .pids
                    .iter()
                    .any(|pid| !gpu.background_process_ids.contains(pid))
                && !fresh_sample);
        let telemetry_unknown = !gpu_observable(self.snapshot.observed_at_unix_ms, gpu)
            || memory.unknown_own > 0
            || gpu
                .used_memory_mib
                .is_some_and(|used| memory.observed > used);
        let activity = if activity_unknown {
            self.gpu_activity_unknown.insert(gpu.uuid.clone());
            ComputeActivity::Unknown
        } else {
            gpu.external_compute
        };
        // The same rule the launch recheck applies (FR-026, FR-027, FR-064).
        let compute_admission = crate::contention::gpu_admission(
            governing(config, ResourceKind::GpuCompute),
            activity,
            gpu.compute_contexts.as_ref(),
            gpu.external_process_ids.as_ref().map(Vec::len),
            !telemetry_unknown,
            config.contention.never_share_occupied_gpus,
        );
        let block_code = if telemetry_unknown {
            Some("gpu_memory_unknown".to_string())
        } else if memory.protection_event {
            Some("gpu_external_workload_appeared".into())
        } else if memory.breached {
            Some("gpu_charges_exceed_reserve".into())
        } else {
            compute_admission.code().map(str::to_string)
        };
        if let Some(code) = &block_code {
            self.blocked_gpus.insert(gpu.uuid.clone());
            self.unsafe_now = true;
            // Unknown activity names the check that failed: the policy's own, or
            // the collector's freshness decision.
            let check = (code == "gpu_yield_first_activity_unknown").then(|| {
                if gpu.external_compute != ComputeActivity::Unknown {
                    "no_fresh_sample"
                } else {
                    gpu.observation
                        .as_ref()
                        .map(|observation| observation.freshness_decision.as_str())
                        .filter(|decision| !decision.is_empty())
                        .unwrap_or("not_recorded")
                }
            });
            let text = crate::contention::reason_text(code).unwrap_or(code);
            self.reasons.push(ReasonCode::new(
                code,
                match check {
                    Some(check) => format!(
                        "GPU {}: {} (failed check: {check}). GPU admission is blocked.",
                        gpu.uuid,
                        text.trim_end_matches('.')
                    ),
                    None => format!("GPU {}: {text} GPU admission is blocked.", gpu.uuid),
                },
            ));
            self.gpu_block_reasons
                .insert(gpu.uuid.clone(), code.clone());
        }
        (activity, compute_admission, telemetry_unknown)
    }

    /// Whether a device's external compute amount is measured, noted on its
    /// compute status. The unknown path applies at once; the measured path
    /// returns only after the quiet window of consecutive measured intervals.
    /// Returns the measured external and own amounts and the measured streak.
    fn gpu_compute_path(
        &mut self,
        gpu: &GpuSnapshot,
        previous: &PreviousGpu,
    ) -> (Option<(u64, u64)>, u32) {
        let config = self.config;
        let measured_streak = if gpu.external_compute_milli.is_some() {
            previous.measured_streak.saturating_add(1)
        } else {
            0
        };
        let measured_window = config
            .contention
            .quiet_window_intervals(config.monitor.interval_ms, config.gpu_sample_max_age_ms());
        // An own-only interval knows external compute is zero without waiting.
        let settled = measured_streak >= measured_window || gpu.own_only;
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
                reason: gpu
                    .compute_amount_unknown
                    .clone()
                    .unwrap_or_else(|| "the collector did not measure the external amount".into()),
            },
        };
        note(
            &mut self.statuses,
            ResourceKey::GpuCompute(gpu.uuid.clone()),
            path.code(),
            match &path {
                ComputePath::Measured => {
                    "external GPU compute is measured from per-process samples".into()
                }
                ComputePath::Unknown { reason, .. } => reason.clone(),
            },
        );
        self.compute_paths.insert(gpu.uuid.clone(), path);
        (measured, measured_streak)
    }

    /// Feeds the device's demand estimate with an uncapped interval's amounts and returns the
    /// estimate (FR-021, Q19). An interval is uncapped when neither the decision that set it nor
    /// the one before sent own work on the device a cap or pause, since a lifted cap takes effect
    /// within one duty period, or when no own allocation runs there.
    fn gpu_demand(
        &mut self,
        gpu: &GpuSnapshot,
        measured: Option<(u64, u64)>,
    ) -> (Option<DemandEstimate>, GpuDemandRecord) {
        let mut demand = self
            .prev
            .gpu_demand
            .get(&gpu.uuid)
            .cloned()
            .unwrap_or_default();
        // Another set of external processes is another demand. The owner's unmanaged processes
        // are listed too, but their usage is already out of the estimate (Q23, Q36).
        if let Some(listed) = &gpu.external_process_ids {
            let owner: BTreeSet<u32> = gpu
                .compute_contexts
                .iter()
                .flat_map(|contexts| contexts.owner.iter().copied())
                .collect();
            let mut pids: Vec<u32> = listed
                .iter()
                .copied()
                .filter(|pid| !gpu.background_process_ids.contains(pid) && !owner.contains(pid))
                .collect();
            pids.sort_unstable();
            if demand.pids.as_ref().is_some_and(|before| before != &pids) {
                demand.uncapped.clear();
            }
            demand.pids = Some(pids);
        }
        note_contexts(&mut demand.seen, gpu);
        if gpu
            .compute_contexts
            .as_ref()
            .is_some_and(|contexts| contexts.samples_readable)
        {
            demand.last_readable_ms = Some(self.now);
        }
        let mut running: Vec<String> = self
            .input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running
                    && a.requested.gpu_memory_mib.contains_key(&gpu.uuid)
            })
            .map(|a| a.id.clone())
            .collect();
        running.sort_unstable();
        // Other own work is another own share.
        if demand
            .own_allocations
            .as_ref()
            .is_some_and(|before| before != &running)
        {
            for entry in demand.uncapped.iter_mut() {
                entry.own = None;
            }
        }
        let uncapped = running.is_empty() || !demand.sent_capped.contains(&true);
        let own_ran = !running.is_empty();
        demand.own_allocations = Some(running);
        if let Some((external, own)) = measured
            && uncapped
        {
            // The owner's unmanaged usage is the owner principal's, not external demand (Q23).
            let external = external.saturating_sub(owner_unmanaged(self.snapshot, &gpu.uuid));
            let amounts = self
                .snapshot
                .principals
                .iter()
                .flatten()
                .filter(|p| !p.is_owner)
                .filter_map(|p| {
                    Some((
                        p.uid?,
                        p.gpu_compute_milli.get(&gpu.uuid).copied().unwrap_or(0),
                    ))
                })
                .collect();
            demand.uncapped.push_back(UncappedInterval {
                at_ms: self.now,
                external,
                by_uid: amounts,
                own: own_ran.then_some(own),
            });
            while demand.uncapped.len() > GPU_DEMAND_INTERVALS {
                demand.uncapped.pop_front();
            }
            demand.last_uncapped_ms = Some(self.now);
            demand.unfed = false;
        } else if uncapped {
            demand.unfed = true;
        }
        let estimate = demand.estimate();
        let record = GpuDemandRecord {
            uncapped,
            fed: uncapped && measured.is_some(),
            estimate: estimate.as_ref().map(|(total, _)| *total),
            intervals: demand.uncapped.len() as u32,
            since_uncapped_ms: demand
                .last_uncapped_ms
                .map(|last| self.now.saturating_sub(last)),
            window: false,
            source: GpuDemandSource::Interval,
            own_uncapped: demand.own_share(),
            duty: None,
            first_come: None,
        };
        self.next.gpu_demand.insert(gpu.uuid.clone(), demand);
        (estimate, record)
    }

    /// The time-slice rule on a device (FR-021, Q22, Q27-Q29). On a time-sliced device with its
    /// contexts known, it holds while own work's busy contexts m, the external ones E and their
    /// principals k give m × k ≤ E, an unattributed principal counting as one principal with one
    /// context. While the amounts are unknown, or the estimate's window is empty, the listing
    /// stands for the window and the rule holds only for m ≤ 1, since unseen busy contexts can
    /// only add to E and k. When it does not hold, `slice_duty` sets the duty from the counts.
    fn time_slice(
        &mut self,
        gpu: &GpuSnapshot,
        measured: bool,
        unattributed: bool,
        holds_for: Option<&dyn Fn(Option<u32>) -> bool>,
    ) -> Option<(TimeSliceRecord, bool, bool)> {
        if gpu.time_sliced != Some(true) {
            return None;
        }
        let contexts = gpu.compute_contexts.as_ref()?;
        let uid_min = self.config.contention.uid_min;
        let own_listed: BTreeSet<u32> = contexts
            .own
            .iter()
            .chain(&contexts.owner)
            .copied()
            .collect();
        let external_listed: BTreeSet<u32> = gpu
            .external_process_ids
            .iter()
            .flatten()
            .filter(|pid| !gpu.background_process_ids.contains(pid) && !own_listed.contains(pid))
            .copied()
            .collect();
        let demand = self.next.gpu_demand.get(&gpu.uuid)?;
        // While the samples are readable the counts are the contexts within one counting window
        // of their last sample, and own work's also of their first listing, capped intervals
        // included; the listing only while the samples are not readable (Q33). An interval without
        // samples within the sample age of the last readable one keeps those counts: NVML can
        // have none every other interval, and the listing would count every listed context as
        // busy by turns.
        let readable = contexts.samples_readable
            || demand.last_readable_ms.is_some_and(|at| {
                self.now.saturating_sub(at) <= self.config.gpu_sample_max_age_ms()
            });
        let mut own_counted = Vec::new();
        let (own, mut principals, source) = if readable {
            let seen = |pid: &u32| demand.seen.get(pid).filter(|context| context.counts());
            for pid in &contexts.own {
                if let Some(context) = seen(pid) {
                    own_counted.push(OwnCounted {
                        pid: *pid,
                        by: if context.sampled {
                            CountedBy::Sample
                        } else {
                            CountedBy::Listing
                        },
                    });
                }
            }
            let owner = contexts
                .owner
                .iter()
                .filter(|pid| !contexts.own.contains(pid))
                .filter(|pid| seen(pid).is_some_and(|context| context.sampled))
                .count();
            let mut principals: BTreeMap<Option<u32>, u32> = BTreeMap::new();
            for pid in &external_listed {
                if let Some(context) = seen(pid).filter(|context| context.sampled)
                    && context.uid.is_none_or(|uid| uid >= uid_min)
                {
                    *principals.entry(context.uid).or_default() += 1;
                }
            }
            // The estimate's readings decide which principals are slice-bound; without an
            // uncapped interval every counted one is (Q29).
            let source = if demand.uncapped.is_empty() {
                SliceSource::Interval
            } else {
                SliceSource::Window
            };
            ((own_counted.len() + owner) as u32, principals, source)
        } else {
            own_counted.extend(contexts.own.iter().map(|pid| OwnCounted {
                pid: *pid,
                by: CountedBy::Listing,
            }));
            {
                let mut principals: BTreeMap<Option<u32>, u32> = BTreeMap::new();
                for (_, uid) in contexts
                    .listed
                    .iter()
                    .filter(|(_, uid)| uid.is_none_or(|uid| uid >= uid_min))
                {
                    *principals.entry(*uid).or_default() += 1;
                }
                (own_listed.len() as u32, principals, SliceSource::Listing)
            }
        };
        own_counted.sort_by_key(|counted| counted.pid);
        // The window's readings decide which principals are slice-bound, even in an interval
        // whose amounts are inconsistent; without a window every counted principal is (Q29), and
        // so is every one once uncapped intervals have gone unmeasured for the sample age, since
        // the window then holds only the past.
        let read = source == SliceSource::Window
            && demand.current(self.now, self.config.gpu_sample_max_age_ms());
        if read && measured && unattributed {
            principals.entry(None).or_insert(1);
        }
        // Performance and Aggressive count only the accounts they hold for (006 FR-008).
        if let Some(holds_for) = holds_for {
            principals.retain(|uid, _| holds_for(*uid));
        }
        let external: u32 = principals.values().sum();
        let count = principals.len() as u32;
        let counted = source != SliceSource::Listing;
        let holds = if counted {
            own.saturating_mul(count) <= external
        } else {
            own <= 1
        };
        let mut record = TimeSliceRecord {
            own,
            external: counted.then_some(external),
            principals: counted.then_some(count),
            source: Some(source),
            holds,
            own_counted,
            ..TimeSliceRecord::default()
        };
        if !holds {
            self.slice_hold(&principals, read, false, &gpu.uuid, &mut record);
        }
        // One more own process is admitted only while (m + 1) × k ≤ E or no counted principal
        // would be slice-bound beside it, so a task is not admitted only to be held (FR-022,
        // FR-089); a principal's latest readings count as well as the window's. An own task
        // placed on the device but not yet listed there counts as one context, so tasks placed a
        // heartbeat apart do not both start.
        let placed = self
            .input
            .allocations
            .iter()
            .filter(|a| {
                matches!(a.phase, AllocationPhase::Pending | AllocationPhase::Running)
                    && a.requested.gpu_memory_mib.contains_key(&gpu.uuid)
            })
            .count() as u32;
        let own_admitting = own.max(placed);
        let next_holds = if counted {
            own_admitting.saturating_add(1).saturating_mul(count) <= external
        } else {
            own_admitting == 0
        };
        let next_held = holds && !next_holds && {
            let mut beside = TimeSliceRecord {
                own: own_admitting.saturating_add(1),
                ..TimeSliceRecord::default()
            };
            self.slice_hold(&principals, read, true, &gpu.uuid, &mut beside);
            !beside.bound.is_empty()
        };
        // The hold comes only from own tasks placed but not yet listed when the rule holds with
        // them running: their own authorization adds nothing beyond what was counted, so it is
        // own new work's, as on CPU (FR-028).
        let own_new = next_held
            && placed > own
            && if counted {
                own_admitting.saturating_mul(count) <= external
            } else {
                own_admitting <= 1
            };
        Some((record, next_held, own_new))
    }

    /// Own work's and each listed UID's arrival on the device, and the first-come floor's state
    /// there (FR-090): `None` while no other principal is listed. An unknown listing restarts
    /// nothing and withholds the floor; so does a listed context whose UID cannot be read.
    fn note_arrivals(&mut self, gpu: &GpuSnapshot) -> Option<&'static str> {
        let now = self.now;
        let mut arrivals = self
            .prev
            .gpu_arrivals
            .get(&gpu.uuid)
            .cloned()
            .unwrap_or_default();
        let Some(contexts) = &gpu.compute_contexts else {
            let listed = gpu
                .external_process_ids
                .as_ref()
                .is_none_or(|pids| !pids.is_empty());
            self.next.gpu_arrivals.insert(gpu.uuid.clone(), arrivals);
            return listed.then_some("order_unknown");
        };
        if !contexts.own.is_empty() {
            arrivals.own = Some(DeviceArrivals::note(arrivals.own, now));
        }
        let uid_min = self.config.contention.uid_min;
        let mut unreadable = false;
        let mut listed = BTreeSet::new();
        for (_, uid) in &contexts.listed {
            match uid {
                Some(uid) if *uid >= uid_min => {
                    listed.insert(*uid);
                }
                Some(_) => {}
                None => unreadable = true,
            }
        }
        for uid in &listed {
            let seen = arrivals.uids.get(uid).copied();
            arrivals.uids.insert(*uid, DeviceArrivals::note(seen, now));
        }
        arrivals
            .uids
            .retain(|_, (_, last)| now.saturating_sub(*last) <= ARRIVAL_GAP_MS);
        let state = if unreadable {
            Some("uid_unreadable")
        } else if listed.is_empty() {
            None
        } else {
            match arrivals.own.filter(|_| !contexts.own.is_empty()) {
                None => Some("order_unknown"),
                Some((own_first, _)) => {
                    let later = listed.iter().all(|uid| {
                        arrivals
                            .uids
                            .get(uid)
                            .is_some_and(|(first, _)| *first > own_first)
                    });
                    Some(if later { "applies" } else { "own_later" })
                }
            }
        };
        if state == Some("applies") {
            self.first_come.insert(gpu.uuid.clone());
        }
        self.next.gpu_arrivals.insert(gpu.uuid.clone(), arrivals);
        state
    }

    /// The share the first-come floor keeps for own work on the device, in thousandths, where it
    /// applies (FR-090): on a time-sliced device, the share 2 × E_max − E running own contexts
    /// get beside E external ones, E_max the most of one principal; elsewhere half the device
    /// beside exactly one listed principal, and none beside more.
    fn first_come_share(&self, gpu: &GpuSnapshot) -> Option<u64> {
        if !self.first_come.contains(&gpu.uuid) {
            return None;
        }
        let contexts = gpu.compute_contexts.as_ref()?;
        let mut by_uid: BTreeMap<u32, u64> = BTreeMap::new();
        for (_, uid) in &contexts.listed {
            *by_uid.entry((*uid)?).or_default() += 1;
        }
        let most = by_uid.values().copied().max()?;
        let all: u64 = by_uid.values().sum();
        if gpu.time_sliced == Some(true) {
            let own = (2 * most).saturating_sub(all);
            Some(GPU_COMPUTE_CAPACITY * own / (own + all).max(1))
        } else {
            (by_uid.len() == 1).then_some(GPU_COMPUTE_CAPACITY / 2)
        }
    }

    /// The time-slice hold at m × k > E (FR-021, FR-089): the slice-bound principals, from the
    /// external estimate's readings when `read`, else every counted principal, and the own tasks
    /// that keep running, ⌊E_b ÷ k_b⌋, at least one. For `admitting` a principal reads the larger
    /// of the window's average and its most within the sample age.
    fn slice_hold(
        &mut self,
        principals: &BTreeMap<Option<u32>, u32>,
        read: bool,
        admitting: bool,
        uuid: &str,
        record: &mut TimeSliceRecord,
    ) {
        let thresholds = self.config.contention.thresholds(ResourceKind::GpuCompute);
        let own = u64::from(record.own);
        let external: u64 = principals.values().map(|n| u64::from(*n)).sum();
        let Some(demand) = self.next.gpu_demand.get_mut(uuid) else {
            return;
        };
        let estimate = demand.estimate();
        let recent =
            admitting.then(|| demand.recent(self.now, self.config.gpu_sample_max_age_ms()));
        // A lone principal reads the total: usage below the tolerance is its sampling error.
        let read_of = |estimate: Option<&DemandEstimate>, uid: &Option<u32>| match (
            estimate,
            principals.len(),
        ) {
            (Some((total, _)), 1) => *total,
            (Some((total, by_uid)), _) => match uid {
                Some(uid) => by_uid.get(uid).copied().unwrap_or(0),
                None => total.saturating_sub(by_uid.values().sum()),
            },
            (None, _) => 0,
        };
        let reading =
            |uid: &Option<u32>| read_of(estimate.as_ref(), uid).max(read_of(recent.as_ref(), uid));
        let threshold = |contexts: u32| {
            GPU_COMPUTE_CAPACITY
                .saturating_mul(u64::from(contexts))
                .saturating_mul(u64::from(thresholds.slice_bound_percent))
                / (100 * (own + external).max(1))
        };
        record.bound = principals
            .iter()
            .map(|(uid, contexts)| SliceBound {
                uid: *uid,
                contexts: *contexts,
                reading: if read { reading(uid) } else { 0 },
                threshold: if read { threshold(*contexts) } else { 0 },
            })
            .filter(|bound| !read || bound.reading >= bound.threshold)
            .collect();
        if record.bound.is_empty() {
            return;
        }
        let contexts: u32 = record.bound.iter().map(|b| b.contexts).sum();
        let mut hold = (contexts / record.bound.len() as u32).max(1);
        // Own work that came first keeps enough contexts running that no later principal gets
        // more than half of the device: 2 × E_max − E (FR-090).
        if self.first_come.contains(uuid) {
            let most = principals.values().copied().max().unwrap_or(0);
            let all: u32 = principals.values().sum();
            hold = hold.max((2 * most).saturating_sub(all));
        }
        record.hold = Some(hold);
    }

    /// A device's GPU compute budget. A measured external amount takes the CPU
    /// paths (FR-021, FR-022): one principal of that amount, saturation from
    /// device utilization, probing and satisfied records. Without one, external
    /// activity counts as one principal of unknown amount. Margins on an idle
    /// device act through admission only.
    fn compute_budget_for(
        &mut self,
        gpu: &GpuSnapshot,
        measured: Option<(u64, u64)>,
        activity: ComputeActivity,
        estimate: Option<&DemandEstimate>,
    ) -> GpuComputeBudget {
        let config = self.config;
        let compute_key = ResourceKey::GpuCompute(gpu.uuid.clone());
        let compute_thresholds = config.contention.thresholds(ResourceKind::GpuCompute);
        let mode = governing(config, ResourceKind::GpuCompute);
        // Neutral, Performance and Aggressive read external demand where own work ran uncapped;
        // Yield First takes the larger of that and this interval's amount, so its target stays
        // at or below Neutral's (FR-021, FR-065).
        // The estimate leaves the owner's unmanaged usage out; this interval's counts, as in the
        // interval's view (FR-021, Q23).
        let averaged_total =
            estimate.map(|(total, _)| total + owner_unmanaged(self.snapshot, &gpu.uuid));
        match measured {
            Some((external, own)) if external.max(averaged_total.unwrap_or(0)) > 0 => {
                let averaged_total = averaged_total.unwrap_or(external);
                let now_of =
                    |p: &PrincipalUsage| p.gpu_compute_milli.get(&gpu.uuid).copied().unwrap_or(0);
                let averaged_of = |p: &PrincipalUsage| match (estimate, p.uid) {
                    (Some((_, by_uid)), Some(uid)) if !p.is_owner => {
                        by_uid.get(&uid).copied().unwrap_or_else(|| now_of(p))
                    }
                    _ => now_of(p),
                };
                // External users are principals by UID, as on CPU, joining on the larger amount.
                let (larger_members, larger_background) =
                    self.principals(compute_key.clone(), GPU_COMPUTE_CAPACITY, &|p| {
                        now_of(p).max(averaged_of(p))
                    });
                let listed: Vec<&PrincipalUsage> =
                    self.snapshot.principals.iter().flatten().collect();
                let averaged_members = larger_members.as_ref().map(|members| {
                    members
                        .iter()
                        .map(|(principal, usage)| {
                            let averaged = listed
                                .iter()
                                .find(|p| {
                                    p.uid == principal.uid && p.is_owner == principal.is_owner
                                })
                                .map_or(*usage, |p| averaged_of(p));
                            (*principal, averaged)
                        })
                        .collect()
                });
                let averaged_background = listed
                    .iter()
                    .filter(|p| {
                        !p.is_owner && p.uid.is_some_and(|uid| uid < config.contention.uid_min)
                    })
                    .map(|p| averaged_of(p))
                    .sum();
                let satisfied = self
                    .prev
                    .satisfied
                    .get(&compute_key)
                    .cloned()
                    .unwrap_or_default();
                let fair_share_limit = self.fair_share_limit(&compute_key);
                let view = |external, principals, background| ModeInput {
                    kind: ResourceKind::GpuCompute,
                    mode,
                    capacity: GPU_COMPUTE_CAPACITY,
                    external,
                    own,
                    principals,
                    background,
                    saturated: gpu.utilization_percent.is_some_and(|busy| {
                        busy >= u32::from(compute_thresholds.saturation_percent)
                    }),
                    psi_only: false,
                    unknown_activity: false,
                    satisfied: satisfied.clone(),
                    probe: None,
                    fair_share_limit,
                    unprotected: false,
                    unattributed_hold: 0,
                };
                let mut averaged_view = view(averaged_total, averaged_members, averaged_background);
                self.note_fair_share(&compute_key, &averaged_view, &compute_thresholds);
                let mut larger_view = view(
                    external.max(averaged_total),
                    larger_members,
                    larger_background,
                );
                // The governing mode's view probes and records; the other takes its records.
                if mode == ContentionMode::YieldFirst {
                    self.probe(&compute_key, &mut larger_view, &compute_thresholds);
                    averaged_view.satisfied = larger_view.satisfied.clone();
                } else {
                    self.probe(&compute_key, &mut averaged_view, &compute_thresholds);
                    larger_view.satisfied = averaged_view.satisfied.clone();
                }
                let governing_view = if mode == ContentionMode::YieldFirst {
                    &larger_view
                } else {
                    &averaged_view
                };
                let mut targets =
                    targets_of(&averaged_view, &compute_thresholds, GPU_COMPUTE_CAPACITY);
                if let Some(yield_first) =
                    targets_of(&larger_view, &compute_thresholds, GPU_COMPUTE_CAPACITY)
                        .get(&ContentionMode::YieldFirst)
                {
                    targets.insert(ContentionMode::YieldFirst, *yield_first);
                }
                GpuComputeBudget {
                    share: own_allowance(governing_view, &compute_thresholds),
                    contended: crate::contention::contended(governing_view, &compute_thresholds),
                    external: governing_view.external,
                    own: Some(own),
                    targets,
                    unattributed: crate::contention::contending_principals(
                        &averaged_view,
                        &compute_thresholds,
                    )
                    .iter()
                    .any(|(uid, _)| uid.is_none()),
                }
            }
            None if matches!(activity, ComputeActivity::Active | ComputeActivity::Unknown) => {
                let view = ModeInput {
                    kind: ResourceKind::GpuCompute,
                    mode,
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
                    fair_share_limit: None,
                    unprotected: false,
                    unattributed_hold: 0,
                };
                GpuComputeBudget {
                    share: own_allowance(&view, &compute_thresholds),
                    contended: true,
                    external: GPU_COMPUTE_CAPACITY,
                    own: None,
                    targets: targets_of(&view, &compute_thresholds, GPU_COMPUTE_CAPACITY),
                    unattributed: false,
                }
            }
            // A measured idle device, or no external activity.
            _ => GpuComputeBudget {
                share: GPU_COMPUTE_CAPACITY,
                contended: false,
                external: 0,
                own: None,
                targets: BTreeMap::new(),
                unattributed: false,
            },
        }
    }

    /// A device's memory budget: the mode's allowance beside external usage or,
    /// with unknown telemetry, what the floor leaves, within the envelope. With
    /// known telemetry it also notes a raised floor, critical free memory, and
    /// the floor's room and breach.
    fn gpu_memory_budget(
        &mut self,
        gpu: &GpuSnapshot,
        memory: &DeviceMemory,
        telemetry_unknown: bool,
    ) -> u64 {
        let (config, input) = (self.config, self.input);
        let key = ResourceKey::GpuMemory(gpu.uuid.clone());
        let envelope_gpu = input
            .envelope
            .as_ref()
            .map(|e| e.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0));
        let gpu_budget = if telemetry_unknown {
            memory.budget
        } else {
            self.floor_available.insert(key.clone(), memory.budget);
            let vram_floor = config.vram_floor(gpu.total_memory_mib);
            if vram_floor.raised {
                note(
                    &mut self.statuses,
                    key.clone(),
                    "small_floor_raised",
                    raised_note(&vram_floor, "GPU memory"),
                );
            }
            let floor = config.vram_floor_mib(gpu.total_memory_mib);
            if gpu.used_memory_mib.is_some_and(|used| {
                gpu.total_memory_mib.saturating_sub(used)
                    < critical_level(config, floor, crate::config::MIN_VRAM_FLOOR_MIB)
            }) {
                self.critical_keys.insert(key.clone());
            }
            let free = gpu
                .used_memory_mib
                .map(|used| gpu.total_memory_mib.saturating_sub(used));
            if let Some(free) = free {
                self.floor_room
                    .insert(key.clone(), free.saturating_sub(floor));
            }
            if let Some(free) = free.filter(|free| *free < floor) {
                self.floor_breaches.insert(key.clone(), (free, floor));
            }
            let capacity = gpu.total_memory_mib.saturating_sub(floor);
            let (members, background) = self.principals(key.clone(), capacity, &|p| {
                p.gpu_memory_mib.get(&gpu.uuid).copied().unwrap_or(0)
            });
            let view = ModeInput {
                kind: ResourceKind::GpuMemory,
                mode: governing(config, ResourceKind::GpuMemory),
                capacity,
                external: memory.external.unwrap_or(0),
                own: memory.observed,
                principals: members,
                background,
                saturated: false,
                psi_only: false,
                unknown_activity: false,
                satisfied: vec![],
                probe: None,
                fair_share_limit: self.fair_share_limit(&key),
                unprotected: false,
                unattributed_hold: 0,
            };
            let thresholds = config.contention.thresholds(ResourceKind::GpuMemory);
            self.note_fair_share(&key, &view, &thresholds);
            self.mode_targets.insert(
                key.clone(),
                targets_of(&view, &thresholds, envelope_gpu.unwrap_or(u64::MAX)),
            );
            self.memory_fair_share.insert(
                key.clone(),
                crate::contention::neutral_share(&view, &thresholds),
            );
            own_allowance(&view, &thresholds)
        };
        envelope_gpu.map_or(gpu_budget, |limit| gpu_budget.min(limit))
    }

    /// Whether the node is safe, and since when, and whether the GPU memory
    /// cooldown has passed for the 0.2 flags; an operator drain empties the
    /// budget.
    pub(super) fn safety(&mut self) {
        let (config, now) = (self.config, self.now);
        if self
            .charged
            .cpu_millicores
            .saturating_sub(self.held_back_cpu)
            > self.budget.cpu_millicores
            || self.charged.ram_mib > self.budget.ram_mib
        {
            self.unsafe_now = true;
        }
        self.next.stable_since = if self.unsafe_now {
            None
        } else {
            Some(self.prev.stable_since.unwrap_or(now))
        };
        self.cooled_down = self.next.stable_since.is_some_and(|since| {
            now - since >= config.gpu_memory_cooldown_ms(governing(config, ResourceKind::GpuMemory))
        });
        if self.input.explicit_drain {
            self.budget = Resources::default();
        }
    }
}

/// What one interval shows of a device's memory.
struct DeviceMemory {
    observed: u64,
    unknown_own: u64,
    external: Option<u64>,
    /// What the floor and external usage leave.
    budget: u64,
    pids: BTreeSet<u32>,
    /// A new external process or growth beyond the noise tolerance appeared.
    protection_event: bool,
    /// Own charges exceed the budget.
    breached: bool,
    next_baseline: Option<u64>,
}

/// Each mode's target on a resource, within `limit`.
fn targets_of(
    view: &ModeInput,
    thresholds: &ResourceThresholds,
    limit: u64,
) -> BTreeMap<ContentionMode, u64> {
    ContentionMode::ALL
        .into_iter()
        .map(|mode| {
            let view = ModeInput {
                mode,
                ..view.clone()
            };
            (mode, own_allowance(&view, thresholds).min(limit))
        })
        .collect()
}

/// An unconfirmed floor near its minimum applies as twice the minimum (FR-054).
fn raised_note(floor: &crate::config::FloorStatus, what: &str) -> String {
    format!(
        "the {what} floor of {} MiB is near its {} MiB minimum and applies as {} MiB until acknowledge_caution confirms small_floor",
        floor.configured_mib.max(floor.minimum_mib),
        floor.minimum_mib,
        floor.effective_mib
    )
}

/// A floor breach is critical once free memory falls below this level, which
/// never goes below the minimum floor (FR-063).
fn critical_level(config: &Config, floor: u64, minimum: u64) -> u64 {
    (floor.saturating_mul(u64::from(config.contention.critical_floor_percent)) / 100).max(minimum)
}

/// Advance a device's contexts by one interval (FR-021): a context no longer listed
/// goes; with the samples readable each one's quiet count rises, own work's newly listed ones enter
/// at their listing and every sampled one restarts at its sample. An interval with no listing
/// changes nothing.
fn note_contexts(seen: &mut BTreeMap<u32, SeenContext>, gpu: &GpuSnapshot) {
    let Some(contexts) = gpu.compute_contexts.as_ref() else {
        return;
    };
    if let Some(external) = &gpu.external_process_ids {
        seen.retain(|pid, _| {
            contexts.own.contains(pid) || contexts.owner.contains(pid) || external.contains(pid)
        });
    }
    if contexts.samples_readable {
        for context in seen.values_mut() {
            context.quiet = context.quiet.saturating_add(1);
        }
    }
    for pid in &contexts.own {
        seen.entry(*pid).or_default();
    }
    if contexts.samples_readable {
        for (pid, uid) in &contexts.sampled {
            seen.insert(
                *pid,
                SeenContext {
                    quiet: 0,
                    sampled: true,
                    uid: *uid,
                },
            );
        }
    }
}

/// The owner's unmanaged GPU compute on a device this interval: its own-account processes that
/// CedeGrid did not launch.
fn owner_unmanaged(snapshot: &Snapshot, uuid: &str) -> u64 {
    snapshot
        .principals
        .iter()
        .flatten()
        .filter(|p| p.is_owner)
        .filter_map(|p| p.gpu_compute_milli.get(uuid))
        .sum()
}

/// A catalog reason on a device's GPU compute status.
impl Interval<'_> {
    /// A device's standing rows and the decision's sentence on whom GPU admission holds for in
    /// Performance and Aggressive (006 FR-020, FR-021).
    fn gpu_standings(
        &mut self,
        gpu: &GpuSnapshot,
        own: OwnArrival,
        standings: Option<&BTreeMap<u32, Standing>>,
        mode: ContentionMode,
        slice: Option<&TimeSliceRecord>,
        held: bool,
    ) {
        let listed: BTreeSet<u32> = gpu
            .compute_contexts
            .iter()
            .flat_map(|contexts| contexts.listed.iter().filter_map(|(_, uid)| *uid))
            .collect();
        let held_for: BTreeSet<u32> = if !held {
            BTreeSet::new()
        } else if mode >= ContentionMode::Performance {
            standings
                .into_iter()
                .flatten()
                .filter(|(uid, s)| s.first_comer && listed.contains(uid))
                .map(|(uid, _)| *uid)
                .collect()
        } else {
            slice
                .into_iter()
                .flat_map(|slice| slice.bound.iter().filter_map(|b| b.uid))
                .collect()
        };
        let key = ResourceKey::GpuCompute(gpu.uuid.clone());
        let status = self.statuses.entry(key.clone()).or_default();
        if let OwnArrival::At(at) = own {
            status.own_arrival_unix_ms = Some(at);
        }
        status.principals = standing_rows(standings, &listed, &held_for);
        if mode < ContentionMode::Performance || listed.is_empty() {
            return;
        }
        if held {
            let first: Vec<u32> = held_for.iter().copied().collect();
            note(
                &mut self.statuses,
                key,
                "held_for_first_comer",
                format!(
                    "GPU admission holds at the time-slice rule for {}, which arrived first with CedeGrid work",
                    if first.is_empty() {
                        "the accounts listed there".to_string()
                    } else {
                        uid_list(&first)
                    }
                ),
            );
        } else {
            note(
                &mut self.statuses,
                key,
                "admitted_as_plain",
                "GPU admission starts new work as plain Linux would: no account that arrived first with CedeGrid work holds a slice there".into(),
            );
        }
    }
}

/// The rows for the accounts on a compute resource that contend or run CedeGrid work there.
fn standing_rows(
    standings: Option<&BTreeMap<u32, Standing>>,
    contending: &BTreeSet<u32>,
    held: &BTreeSet<u32>,
) -> Vec<PrincipalStanding> {
    standings
        .into_iter()
        .flatten()
        .filter(|(uid, standing)| standing.work.is_some() || contending.contains(uid))
        .map(|(uid, standing)| PrincipalStanding {
            uid: *uid,
            cedegrid: standing.work.is_some(),
            arrival_unix_ms: standing.work.and_then(|work| work.arrival_unix_ms),
            first_comer: standing.first_comer,
            protected: standing.protected,
            held_for: held.contains(uid),
        })
        .collect()
}

/// UIDs for a reason sentence: "UID 1001" or "UIDs 1001, 1002".
fn uid_list(uids: &[u32]) -> String {
    let list: Vec<String> = uids.iter().map(u32::to_string).collect();
    match list.len() {
        1 => format!("UID {}", list[0]),
        _ => format!("UIDs {}", list.join(", ")),
    }
}

fn note_reason(
    statuses: &mut BTreeMap<ResourceKey, ResourceStatus>,
    uuid: &str,
    code: &'static str,
) {
    note(
        statuses,
        ResourceKey::GpuCompute(uuid.to_string()),
        code,
        crate::contention::reason_text(code)
            .unwrap_or_default()
            .to_string(),
    );
}

/// Whether the agent runs in a CPU cgroup of the owner's own, so the kernel charges CedeGrid's
/// CPU to the owner rather than to other users: an ancestor that is the owner's user slice or
/// System mode unit and has the `cpu` controller.
pub(super) fn owner_cpu_cgroup(snapshot: &Snapshot) -> bool {
    let own = |name: &str| {
        let user = name
            .strip_prefix("user-")
            .and_then(|rest| rest.strip_suffix(".slice"));
        let unit = name
            .strip_prefix("cedegrid-u")
            .and_then(|rest| rest.strip_suffix(".service"));
        user.or(unit)
            .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
    };
    snapshot
        .kernel
        .as_ref()
        .and_then(|kernel| kernel.hierarchy.as_ref())
        .is_some_and(|hierarchy| {
            hierarchy.ancestors.iter().any(|level| {
                level
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(own)
                    && level
                        .controllers
                        .as_ref()
                        .is_some_and(|controllers| controllers.iter().any(|c| c == "cpu"))
            })
        })
}
