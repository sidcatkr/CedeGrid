//! The interval's measurement phases: charges and modes, the CPU, RAM and GPU
//! readings, the mode targets and budgets, and whether the node is safe.
use super::*;

impl<'a> Interval<'a> {
    /// Charges: running and draining work at max(request, observed), pending
    /// reservations in full, and work of unknown usage at max(request, last
    /// known).
    pub(super) fn charge(&mut self) -> Result<()> {
        let (input, prev, now) = (self.input, self.prev, self.now);
        let cpu_window_ms = self
            .config
            .contention
            .thresholds(ResourceKind::Cpu)
            .admission_usage_window_s
            .saturating_mul(1000);
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
                                let usage = p95(samples.iter().map(|(_, usage)| *usage));
                                self.cpu_credit +=
                                    i128::from(charge.cpu_millicores) - i128::from(usage);
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
                    // and RAM keep their readings (TEL-5).
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
                    // A missing observation keeps the admission window (POL-4).
                    if let Some(window) = prev.cpu_usage.get(&allocation.id) {
                        self.next
                            .cpu_usage
                            .insert(allocation.id.clone(), window.clone());
                    }
                    self.unknown_charge.add_checked(&charge)?;
                    if last.is_none() {
                        self.unknown_new.add_checked(&charge)?;
                    }
                    self.unknown_allocations.insert(allocation.id.clone());
                    self.reasons.push(ReasonCode::new("allocation_unknown_hold", format!(
                        "allocation {} has unknown managed usage; charged at max(request, last known), its step holds and affected admission is blocked",
                        allocation.id
                    )));
                }
            }
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
        self.cpu_held = self.unknown_charge.cpu_millicores > 0;
        self.ram_held = self.unknown_charge.ram_mib > 0;
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
            match window {
                Some(window) if window.abs_diff(interval) <= bound => window,
                _ => interval,
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
        let saturated = self.cpu_saturation();
        // CedeGrid's own CPU comes out of own tasks' budget and targets (FR-069).
        let cedegrid = self.cedegrid_cpu;
        if self.cpu_unknown || self.cpu_held {
            self.budget.cpu_millicores = self.budget.cpu_millicores.saturating_sub(cedegrid);
            self.next.cpu_view = self.prev.cpu_view.clone();
            self.next.cpu_quiet_intervals = self.prev.cpu_quiet_intervals;
            self.carry_principals(&ResourceKey::Cpu);
            if self.cpu_unknown
                && governing(config, ResourceKind::Cpu) >= ContentionMode::Performance
            {
                self.cpu_unknown_hold();
            }
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
        };
        // In Performance and Aggressive own work above its Neutral share can saturate the CPU
        // itself, which is no evidence of hidden external demand: utilization is judged with
        // own work at that share, so a satisfied record does not make the target cycle.
        if view.saturated && !self.psi_saturated && view.mode >= ContentionMode::Performance {
            let share = crate::contention::neutral_share(
                &ModeInput {
                    saturated: true,
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
        self.note_cpu_quiet(&view, &thresholds);
        self.probe(&ResourceKey::Cpu, &mut view, &thresholds);
        self.next.cpu_view = Some(CpuView {
            view: view.clone(),
            room: self
                .cpu_capacity
                .saturating_sub(self.cpu_reserve)
                .min(self.cpu_ceiling),
            cedegrid,
        });
        let mut targets = targets_of(&view, &thresholds, self.budget.cpu_millicores);
        for target in targets.values_mut() {
            *target = target.saturating_sub(cedegrid);
        }
        self.mode_targets.insert(ResourceKey::Cpu, targets);
        // The idle reserve is the larger of the explicit reserve and the mode's
        // margin of total capacity; the two never stack.
        self.budget.cpu_millicores = own_allowance(&view, &thresholds)
            // Capacity minus the reserve and external usage, within the ancestor quota.
            .min(self.budget.cpu_millicores)
            .saturating_sub(cedegrid);
    }

    /// While a resource is unknown or held, its principals, satisfied records and probes
    /// stand as they were (POL-4).
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

    /// While CPU telemetry is unknown, Performance and Aggressive hold own CPU at Neutral's
    /// fair share of the last known view, with the unknown as one more principal of unbounded
    /// demand. With no view yet, the ladder takes no new step (FR-022).
    fn cpu_unknown_hold(&mut self) {
        let Some(last) = self.prev.cpu_view.as_ref() else {
            self.cpu_unviewed = true;
            return;
        };
        let thresholds = self.config.contention.thresholds(ResourceKind::Cpu);
        let mode = governing(self.config, ResourceKind::Cpu);
        let view = ModeInput {
            mode,
            unknown_activity: true,
            probe: None,
            ..last.view.clone()
        };
        let mut targets = targets_of(&view, &thresholds, last.room);
        for target in targets.values_mut() {
            *target = target.saturating_sub(last.cedegrid);
        }
        let hold = targets[&mode];
        self.budget.cpu_millicores = hold;
        self.mode_targets.insert(ResourceKey::Cpu, targets);
        self.reasons.push(ReasonCode::new(
            "cpu_unknown_fair_share_hold",
            "CPU telemetry unknown; own CPU holds at Neutral's fair share of the last known view",
        ));
        note(
            &mut self.statuses,
            ResourceKey::Cpu,
            "cpu_unknown_fair_share_hold",
            format!("own CPU holds at {hold}m, the unknown counted as one more principal"),
        );
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
                crate::contention::cpu_quiet(
                    contending,
                    external.saturating_sub(accounted),
                    floor,
                    self.cpu_bound,
                )
            }
            None => crate::contention::cpu_quiet([], external, floor, self.cpu_bound),
        };
        self.next.cpu_quiet_intervals = if quiet {
            self.prev.cpu_quiet_intervals.saturating_add(1)
        } else {
            0
        };
        let window = self.config.contention.principal_window_intervals;
        self.cpu_contention_ended = self.next.cpu_quiet_intervals >= window;
        self.cpu_contention_returned =
            self.cpu_contention_ended && self.prev.cpu_quiet_intervals < window;
    }

    /// RAM: the mode's target beside the contending principals.
    pub(super) fn ram_target(&mut self) {
        if self.ram_unknown || self.ram_held {
            self.next.ram_fair_share = self.prev.ram_fair_share;
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
        };
        let thresholds = config.contention.thresholds(ResourceKind::Ram);
        self.mode_targets
            .insert(ResourceKey::Ram, targets_of(&view, &thresholds, u64::MAX));
        self.budget.ram_mib = own_allowance(&view, &thresholds);
        let fair = crate::contention::neutral_share(&view, &thresholds);
        self.memory_fair_share.insert(ResourceKey::Ram, fair);
        self.next.ram_fair_share = Some(fair);
    }

    /// The operator envelope bounds own work per resource (FR-057). Each floor,
    /// and the target share where own work has room, goes into the status.
    pub(super) fn envelope_and_floors(&mut self) {
        if let Some(envelope) = &self.input.envelope {
            self.budget.cpu_millicores = self.budget.cpu_millicores.min(envelope.cpu_millicores);
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
        if telemetry_unknown {
            self.gpu_memory_unknown.insert(gpu.uuid.clone());
        }
        if !telemetry_unknown {
            let budget = self.compute_budget_for(gpu, measured, activity);
            self.gpu_compute_budget.insert(gpu.uuid.clone(), budget);
        }
        let compute_status = self
            .statuses
            .entry(ResourceKey::GpuCompute(gpu.uuid.clone()))
            .or_default();
        compute_status.activity = Some(activity);
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
        } else {
            compute_admission
        });
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
        // A background context listed without a fresh sample is no activity (FR-021).
        let activity_unknown = gpu.external_compute == ComputeActivity::Unknown
            || (memory
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
            self.reasons.push(ReasonCode::new(
                code,
                format!(
                    "GPU {}: {} GPU admission is blocked.",
                    gpu.uuid,
                    crate::contention::reason_text(code).unwrap_or(code)
                ),
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
    ) -> GpuComputeBudget {
        let config = self.config;
        let compute_key = ResourceKey::GpuCompute(gpu.uuid.clone());
        let compute_thresholds = config.contention.thresholds(ResourceKind::GpuCompute);
        let mode = governing(config, ResourceKind::GpuCompute);
        match measured {
            Some((external, own)) if external > 0 => {
                // External users are principals by UID, as on CPU.
                let (members, background) =
                    self.principals(compute_key.clone(), GPU_COMPUTE_CAPACITY, &|p| {
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
                        .prev
                        .satisfied
                        .get(&compute_key)
                        .cloned()
                        .unwrap_or_default(),
                    probe: None,
                };
                self.probe(&compute_key, &mut view, &compute_thresholds);
                GpuComputeBudget {
                    share: own_allowance(&view, &compute_thresholds),
                    contended: crate::contention::contended(&view, &compute_thresholds),
                    external,
                    own: Some(own),
                    targets: targets_of(&view, &compute_thresholds, GPU_COMPUTE_CAPACITY),
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
                };
                GpuComputeBudget {
                    share: own_allowance(&view, &compute_thresholds),
                    contended: true,
                    external: GPU_COMPUTE_CAPACITY,
                    own: None,
                    targets: targets_of(&view, &compute_thresholds, GPU_COMPUTE_CAPACITY),
                }
            }
            // A measured idle device, or no external activity.
            _ => GpuComputeBudget {
                share: GPU_COMPUTE_CAPACITY,
                contended: false,
                external: 0,
                own: None,
                targets: BTreeMap::new(),
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
            };
            let thresholds = config.contention.thresholds(ResourceKind::GpuMemory);
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
