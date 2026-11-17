//! The admission phases: cooldowns, external memory peaks, headroom, what each
//! resource holds back, admission per resource and the allowances.
use super::*;

impl Interval<'_> {
    /// A reclaim episode on a memory resource starts when a floor breach or own
    /// usage beyond the budget appears there; recent episodes lengthen the memory
    /// cooldown by exponential backoff, and a quiet peak window resets it (FR-061).
    pub(super) fn reclaim_episodes(&mut self) {
        let (config, prev, now) = (self.config, self.prev, self.now);
        let reclaiming = &self.next.reclaiming;
        let mut episodes: BTreeMap<ResourceKey, Vec<u64>> = BTreeMap::new();
        for key in prev.reclaim_episodes.keys().chain(reclaiming.iter()) {
            let window_ms = config
                .contention
                .thresholds(key.kind())
                .memory_peak_window_s
                .saturating_mul(1000);
            let mut starts: Vec<u64> = prev
                .reclaim_episodes
                .get(key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|start| now.saturating_sub(*start) < window_ms)
                .collect();
            if reclaiming.contains(key) && !prev.reclaiming.contains(key) {
                starts.push(now);
            }
            if !starts.is_empty() {
                episodes.insert(key.clone(), starts);
            }
        }
        self.next.reclaim_episodes = episodes;
    }

    /// Which resources have a step still moving down, and whether CPU and RAM
    /// have each been safe for their own cooldown, never the GPU one (M1,
    /// FR-034).
    pub(super) fn cooldowns(&mut self) {
        let (config, snapshot, input, prev, now) =
            (self.config, self.snapshot, self.input, self.prev, self.now);
        let ladder = &self.next.ladder;
        // A pause that moves to another task at its bound is the hold staying, not a step down:
        // on each resource, new pauses count only beyond the pauses ended in the same interval
        // (FR-028, FR-089).
        let mut ended: BTreeMap<&ResourceKey, usize> = BTreeMap::new();
        for ((id, key), before) in &prev.ladder {
            if before.step == YieldStep::Pause
                && ladder
                    .get(&(id.clone(), key.clone()))
                    .is_none_or(|next| next.step < YieldStep::Pause)
            {
                *ended.entry(key).or_default() += 1;
            }
        }
        let mut paused: BTreeMap<&ResourceKey, usize> = BTreeMap::new();
        let mut moving_down = BTreeSet::new();
        for ((id, key), next) in ladder {
            let before = prev.ladder.get(&(id.clone(), key.clone()));
            let step = before.map_or(YieldStep::None, |s| s.step);
            if next.step == YieldStep::Pause && step < YieldStep::Pause {
                *paused.entry(key).or_default() += 1;
                continue;
            }
            let fell = match (next.target, before.and_then(|s| s.target)) {
                (Some(next), Some(before)) => {
                    before.saturating_sub(next) > jitter_tolerance(config, ladder, key)
                }
                (None, Some(_)) => true,
                _ => false,
            };
            if next.step > step
                || (next.step == step && matches!(step, YieldStep::Cap | YieldStep::Hint) && fell)
            {
                moving_down.insert(key.clone());
            }
        }
        for (key, count) in paused {
            if count > ended.get(key).copied().unwrap_or(0) {
                moving_down.insert(key.clone());
            }
        }
        // A CPU cap that holds may sit within the tolerance above a jittering budget.
        let cpu_fits = self.charged.cpu_millicores <= self.budget.cpu_millicores
            || (!moving_down.contains(&ResourceKey::Cpu)
                && self.charged.cpu_millicores
                    <= self.budget.cpu_millicores.saturating_add(jitter_tolerance(
                        config,
                        ladder,
                        &ResourceKey::Cpu,
                    )));
        let cpu_safe = !input.explicit_drain
            && !self.cpu_unknown
            && !self.cpu_held
            && self.cpu_capacity > 0
            && cpu_fits;
        let ram_safe = !input.explicit_drain
            && !self.ram_unknown
            && !self.ram_held
            && snapshot.total_ram_mib > 0
            && self.charged.ram_mib <= self.budget.ram_mib;
        // Holding back only for own allocations that have no first measurement yet:
        // nothing else about the resource is unsafe, reclaimed or stepped (FR-028).
        let steps_held = |key: &ResourceKey| {
            ladder
                .iter()
                .any(|((_, k), state)| k == key && state.step != YieldStep::None)
        };
        // Own work, counting the new allocations at their charge, within the mode's
        // target share, and external usage no higher than beside that target: steps
        // are held while a measurement is unknown, so contention and external growth
        // show here rather than in the ladder. A held interval computes no target, so
        // the last one carries through with the external usage it was computed beside.
        let target_of = |key: &ResourceKey, before: Option<(u64, u64)>| {
            self.statuses
                .get(key)
                .and_then(|status| status.target_share)
                .map(|target| (target, self.external_of(key)))
                .or(before)
        };
        let (cpu_target, ram_target) = (
            target_of(&ResourceKey::Cpu, prev.cpu_target),
            target_of(&ResourceKey::Ram, prev.ram_target),
        );
        self.next.cpu_target = cpu_target;
        self.next.ram_target = ram_target;
        let within = |key: &ResourceKey, target: Option<(u64, u64)>, charged: u64| {
            let tolerance = jitter_tolerance(config, ladder, key);
            target.is_some_and(|(target, external)| {
                charged <= target.saturating_add(tolerance)
                    && self.external_of(key) <= external.saturating_add(tolerance)
            })
        };
        let cpu_within = within(&ResourceKey::Cpu, cpu_target, self.charged.cpu_millicores);
        let ram_within = within(&ResourceKey::Ram, ram_target, self.charged.ram_mib);
        let cpu_new_now = !input.explicit_drain
            && !self.cpu_unknown
            && !self.cpu_held_inconsistent
            && self.cpu_capacity > 0
            && cpu_fits
            && self.unknown_charge.cpu_millicores > 0
            && self.unknown_charge.cpu_millicores == self.unknown_new.cpu_millicores
            && cpu_within
            && !steps_held(&ResourceKey::Cpu);
        let ram_new_now = !input.explicit_drain
            && !self.ram_unknown
            && snapshot.total_ram_mib > 0
            && self.charged.ram_mib <= self.budget.ram_mib
            && self.unknown_charge.ram_mib > 0
            && self.unknown_charge.ram_mib == self.unknown_new.ram_mib
            && ram_within
            && !self.breached.contains(&ResourceKey::Ram)
            && !steps_held(&ResourceKey::Ram);
        self.next.cpu_reset_by_new = if cpu_safe {
            prev.cpu_reset_by_new
        } else {
            cpu_new_now
        };
        self.next.ram_reset_by_new = if ram_safe {
            prev.ram_reset_by_new
        } else {
            ram_new_now
        };
        self.cpu_new_only = if cpu_safe {
            self.next.cpu_reset_by_new && !steps_held(&ResourceKey::Cpu)
        } else {
            cpu_new_now
        };
        self.ram_new_only = if ram_safe {
            self.next.ram_reset_by_new
                && !steps_held(&ResourceKey::Ram)
                && !self.breached.contains(&ResourceKey::Ram)
        } else {
            ram_new_now
        };
        // In Yield First an interval in which own work does not fit restarts the count only for a
        // principal's cause; anything else holds admission through the headroom while it lasts,
        // and the count keeps running. An unreadable list tells no such change from a
        // principal (FR-034).
        let window = config.contention.principal_window_intervals;
        let floor = (self.cpu_capacity.saturating_mul(u64::from(
            config
                .contention
                .thresholds(ResourceKind::Cpu)
                .principal_min_percent,
        )) / 100)
            .max(1);
        let cpu_cause = self.principal_cause(
            &ResourceKey::Cpu,
            prev.cpu_fit_usage.as_ref(),
            floor.max(self.cpu_bound),
            |p| p.cpu_millicores,
        ) || (self.next.cpu_unattributed_intervals >= window
            && (prev.cpu_unattributed_intervals < window
                || prev.cpu_fit_usage.as_ref().is_none_or(|fit| {
                    self.cpu_unattributed
                        > fit
                            .get(&None)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(floor.max(self.cpu_bound))
                })));
        let ram_cause = self.principal_cause(
            &ResourceKey::Ram,
            prev.ram_fit_usage.as_ref(),
            jitter_tolerance(config, ladder, &ResourceKey::Ram),
            |p| p.ram_mib,
        ) || self
            .next
            .ram_unattributed
            .is_some_and(|(_, intervals)| intervals >= window);
        let short = |kind: ResourceKind, cause: bool| {
            governing(config, kind) == ContentionMode::YieldFirst
                && !input.explicit_drain
                && snapshot.principals.is_some()
                && !cause
        };
        let cpu_short = short(ResourceKind::Cpu, cpu_cause)
            && !self.cpu_unknown
            && !self.cpu_held
            && self.cpu_capacity > 0;
        let ram_short = short(ResourceKind::Ram, ram_cause)
            && !self.ram_unknown
            && !self.ram_held
            && snapshot.total_ram_mib > 0
            && !self.breached.contains(&ResourceKey::Ram);
        // What each principal used at the last interval own work fit, for the next cause.
        self.next.cpu_fit_usage = if cpu_safe {
            self.fit_usage(&ResourceKey::Cpu, Some(self.cpu_unattributed), |p| {
                p.cpu_millicores
            })
        } else {
            prev.cpu_fit_usage.clone()
        };
        self.next.ram_fit_usage = if ram_safe {
            self.fit_usage(&ResourceKey::Ram, None, |p| p.ram_mib)
        } else {
            prev.ram_fit_usage.clone()
        };
        // An enforcement hold of own usage over its target does not restart stability (FR-011).
        self.next.cpu_stable_since = (cpu_safe || self.cpu_own_hold || cpu_short)
            .then_some(prev.cpu_stable_since.unwrap_or(now));
        self.next.ram_stable_since =
            (ram_safe || ram_short).then_some(prev.ram_stable_since.unwrap_or(now));
        self.cpu_allowed = cooled(
            config,
            now,
            self.next.cpu_stable_since,
            ResourceKind::Cpu,
            0,
        );
        self.ram_allowed = cooled(
            config,
            now,
            self.next.ram_stable_since,
            ResourceKind::Ram,
            backoff_for(config, &self.next.reclaim_episodes, &ResourceKey::Ram),
        );
        // The 0.2 report has one flag for both; every launch reserves CPU and RAM.
        self.cpu_ram_expansion_allowed = self.cpu_allowed && self.ram_allowed;
        self.moving_down = moving_down;
    }

    /// A listed user's usage on a resource this interval; 0 when it is not listed.
    fn listed_usage(&self, uid: u32, pick: &dyn Fn(&PrincipalUsage) -> u64) -> u64 {
        self.snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| !p.is_owner && p.uid == Some(uid))
            .map(pick)
            .sum()
    }

    /// Whether a principal caused this interval's change on a resource (FR-034): a user
    /// became a principal in it, or the principals admitted before and now rose together by
    /// more than `tolerance` since the last interval in which own work fit. With no usage on
    /// record for that interval, an admitted principal counts as risen (Principle III).
    fn principal_cause(
        &self,
        key: &ResourceKey,
        fit: Option<&BTreeMap<Option<u32>, u64>>,
        tolerance: u64,
        pick: impl Fn(&PrincipalUsage) -> u64,
    ) -> bool {
        let admitted = |counts: &BTreeMap<(ResourceKey, u32), PrincipalCount>, uid: u32| {
            counts
                .get(&(key.clone(), uid))
                .is_some_and(|count| count.admitted)
        };
        let mut rise = 0u64;
        for ((k, uid), count) in &self.next.principal_counts {
            if k != key || !count.admitted {
                continue;
            }
            if !admitted(&self.prev.principal_counts, *uid) {
                return true;
            }
            let Some(fit) = fit else {
                return true;
            };
            let before = fit.get(&Some(*uid)).copied().unwrap_or(0);
            rise = rise.saturating_add(self.listed_usage(*uid, &pick).saturating_sub(before));
        }
        rise > tolerance
    }

    /// Each counted user's usage on a resource this interval, and `unattributed` under `None`;
    /// `None` while the list is unreadable.
    fn fit_usage(
        &self,
        key: &ResourceKey,
        unattributed: Option<u64>,
        pick: impl Fn(&PrincipalUsage) -> u64,
    ) -> Option<BTreeMap<Option<u32>, u64>> {
        self.snapshot.principals.as_ref()?;
        let mut usage: BTreeMap<Option<u32>, u64> = self
            .next
            .principal_counts
            .keys()
            .filter(|(k, _)| k == key)
            .map(|(_, uid)| (Some(*uid), self.listed_usage(*uid, &pick)))
            .collect();
        if let Some(unattributed) = unattributed {
            usage.insert(None, unattributed);
        }
        Some(usage)
    }

    /// Memory admission uses the external peak over its window; yielding uses
    /// current usage (FR-060). Each listed external process keeps its own peak
    /// while it lives and loses it when it exits; the rest of external usage
    /// keeps one peak.
    pub(super) fn memory_peaks(&mut self) {
        let (config, snapshot, prev, now) = (self.config, self.snapshot, self.prev, self.now);
        let listed = snapshot.external_processes.as_deref().unwrap_or_default();
        for (key, status) in &self.statuses {
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
                if usage == 0 && !prev.process_peaks.contains_key(&id) {
                    continue;
                }
                listed_total = listed_total.saturating_add(usage);
                let mut samples = prev.process_peaks.get(&id).cloned().unwrap_or_default();
                peak = peak.saturating_add(window_peak(&mut samples, now, usage, window_ms));
                self.next.process_peaks.insert(id, samples);
            }
            // An unreadable list is not an exit: each tracked process keeps its peak and counts
            // at its last sample, so remembered demand is not admitted against (FR-077).
            // On a device, its own process list is known evidence: a process it no longer lists
            // has exited.
            if snapshot.external_processes.is_none() {
                let on_device = |pid: u32| match key {
                    ResourceKey::GpuMemory(uuid) => snapshot
                        .gpus
                        .iter()
                        .find(|gpu| &gpu.uuid == uuid)
                        .and_then(|gpu| gpu.external_process_ids.as_ref())
                        .is_none_or(|listed| listed.contains(&pid)),
                    _ => true,
                };
                for (id, samples) in prev
                    .process_peaks
                    .iter()
                    .filter(|(id, _)| &id.0 == key && on_device(id.1))
                {
                    let usage = samples.back().map_or(0, |(_, usage)| *usage);
                    listed_total = listed_total.saturating_add(usage);
                    let mut samples = samples.clone();
                    peak = peak.saturating_add(window_peak(&mut samples, now, usage, window_ms));
                    self.next.process_peaks.insert(id.clone(), samples);
                }
            }
            let mut samples = prev.external_peaks.get(key).cloned().unwrap_or_default();
            peak = peak.saturating_add(window_peak(
                &mut samples,
                now,
                external.saturating_sub(listed_total),
                window_ms,
            ));
            self.peak_excess
                .insert(key.clone(), peak.saturating_sub(external));
            self.next.external_peaks.insert(key.clone(), samples);
        }
        // A memory resource still present but unknown this interval keeps its peaks, which
        // age out by time once it is known again (FR-060).
        let present = |key: &ResourceKey| match key {
            ResourceKey::GpuMemory(uuid) => snapshot.gpus.iter().any(|gpu| &gpu.uuid == uuid),
            _ => true,
        };
        for (key, samples) in &prev.external_peaks {
            if !self.next.external_peaks.contains_key(key) && present(key) {
                self.next
                    .external_peaks
                    .insert(key.clone(), samples.clone());
                for (id, samples) in &prev.process_peaks {
                    if &id.0 == key {
                        self.next.process_peaks.insert(id.clone(), samples.clone());
                    }
                }
            }
        }
    }

    /// Admission headroom: the budget less every charge, less the external memory
    /// peak, open per resource only after its own cooldown, and closed by a
    /// breached floor or an exhausted spool. Sets the expansion flags.
    pub(super) fn admission_headroom(&mut self) {
        let (config, input, now) = (self.config, self.input, self.now);
        let mut headroom = self.budget.clone();
        // Recommendations are not release confirmation: subtract ALL allocations,
        // including existing draining work and everything just recommended for drain.
        headroom.subtract_saturating(&self.charged);
        headroom.gpu_memory_mib.retain(|uuid, _| {
            !input.explicit_drain
                && !self.blocked_gpus.contains(uuid)
                && self
                    .next
                    .previous_gpus
                    .get(uuid)
                    .and_then(|p| p.stable_since)
                    .is_some_and(|since| {
                        now - since
                            >= config
                                .gpu_memory_cooldown_ms(governing(config, ResourceKind::GpuMemory))
                                .saturating_add(backoff_for(
                                    config,
                                    &self.next.reclaim_episodes,
                                    &ResourceKey::GpuMemory(uuid.clone()),
                                ))
                    })
        });
        headroom.ram_mib = headroom.ram_mib.saturating_sub(
            self.peak_excess
                .get(&ResourceKey::Ram)
                .copied()
                .unwrap_or(0),
        );
        for (uuid, amount) in headroom.gpu_memory_mib.iter_mut() {
            let key = ResourceKey::GpuMemory(uuid.clone());
            *amount = amount.saturating_sub(self.peak_excess.get(&key).copied().unwrap_or(0));
        }
        // A breached floor blocks admission on that resource in every policy (FR-041).
        if self.breached.contains(&ResourceKey::Ram) {
            headroom.ram_mib = 0;
        }
        let legacy_expansion_allowed =
            !self.unsafe_now && self.cooled_down && self.would_drain.is_empty();
        // An exhausted spool blocks admission only; nothing drains for it (FR-058).
        self.cpu_ram_expansion_allowed = self.cpu_ram_expansion_allowed && !input.spool_full;
        if input.spool_full {
            headroom = Resources::default();
            self.reasons.push(ReasonCode::new(
                "admission_spool_exhausted",
                "node spool budget exhausted; admission is blocked and no allocation is drained",
            ));
        }
        self.expansion_allowed = !input.spool_full
            && (legacy_expansion_allowed
                || (self.cpu_ram_expansion_allowed
                    && headroom.gpu_memory_mib.values().any(|amount| *amount > 0)));
        // CPU and RAM headroom open only after their own cooldowns.
        if !self.cpu_allowed {
            headroom.cpu_millicores = 0;
        }
        if !self.ram_allowed {
            headroom.ram_mib = 0;
        }
        if !self.expansion_allowed {
            if self.cpu_ram_expansion_allowed {
                self.reasons.push(ReasonCode::new("cpu_only_admission", "independently stable CPU/RAM headroom permits CPU-only admission; GPU admission remains blocked"));
            }
            if !self.unsafe_now && !self.cooled_down {
                // The catalog sentence says this covers only the 0.2 expansion flags.
                self.reasons.push(ReasonCode::new(
                    "cooldown_pending",
                    crate::contention::reason_text("cooldown_pending")
                        .unwrap_or("cooldown_pending"),
                ));
            }
        }
        self.headroom = headroom;
    }

    pub(super) fn external_of(&self, key: &ResourceKey) -> u64 {
        self.statuses.get(key).and_then(|s| s.external).unwrap_or(0)
    }

    /// What each resource holds back and why (SC-016).
    pub(super) fn held_back(&mut self) {
        let snapshot = self.snapshot;
        let mut held_back: BTreeMap<ResourceKey, HeldBack> = BTreeMap::new();
        held_back.insert(ResourceKey::Cpu, self.cpu_held_back());
        held_back.insert(ResourceKey::Ram, self.ram_held_back());
        for gpu in &snapshot.gpus {
            held_back.insert(
                ResourceKey::GpuMemory(gpu.uuid.clone()),
                self.gpu_memory_held_back(gpu),
            );
        }
        for gpu in &snapshot.gpus {
            held_back.insert(
                ResourceKey::GpuCompute(gpu.uuid.clone()),
                self.gpu_compute_held_back(gpu),
            );
        }
        for (key, held) in held_back {
            let capacity = match &key {
                ResourceKey::Cpu => self.cpu_capacity,
                ResourceKey::Ram => snapshot.total_ram_mib,
                ResourceKey::GpuMemory(uuid) => snapshot
                    .gpus
                    .iter()
                    .find(|gpu| &gpu.uuid == uuid)
                    .map_or(0, |gpu| gpu.total_memory_mib),
                ResourceKey::GpuCompute(_) => GPU_COMPUTE_CAPACITY,
            };
            self.statuses.entry(key).or_default().held_back = Some(held.with_capacity(capacity));
        }
    }

    /// CPU held back: the reserve where it exceeds the margin, the mode's margin, what
    /// an ancestor ceiling takes beyond them, contention (by PSI alone when only PSI
    /// saturates), and the cooldown.
    fn cpu_held_back(&self) -> HeldBack {
        let config = self.config;
        // CedeGrid's own CPU is in use, not held back (FR-069).
        let available = self
            .cpu_capacity
            .saturating_sub(self.external_of(&ResourceKey::Cpu))
            .saturating_sub(self.cedegrid_cpu);
        let mut held = HeldBack::default();
        if self.cpu_unknown || self.cpu_held {
            held.unknown = available;
            return held;
        }
        let thresholds = config.contention.thresholds(ResourceKind::Cpu);
        let percent = thresholds.margin_percent(governing(config, ResourceKind::Cpu));
        let margin = self.cpu_capacity.saturating_mul(u64::from(percent)) / 100;
        let target = available.saturating_sub(self.budget.cpu_millicores);
        // The budget is capped at the ancestor ceiling after the reserve; only what
        // the ceiling takes beyond that is the ceiling's.
        held.ceiling = available
            .saturating_sub(self.cpu_reserve)
            .saturating_sub(self.cpu_ceiling)
            .min(target);
        let rest = target - held.ceiling;
        held.reserve = if self.cpu_reserve > margin {
            self.cpu_reserve.min(rest)
        } else {
            0
        };
        held.margin = rest.saturating_sub(held.reserve).min(margin);
        held.contention = rest.saturating_sub(held.reserve + held.margin);
        if self.psi_only {
            held.contention_psi = std::mem::take(&mut held.contention);
        }
        let pre = self
            .budget
            .cpu_millicores
            .saturating_sub(self.charged.cpu_millicores);
        held.cooldown = if self.cpu_allowed { 0 } else { pre };
        held.charge_shortfall =
            charge_shortfall(self.input, &self.charges, &|r| r.cpu_millicores, false);
        held
    }

    /// RAM held back: the floor, the mode's margin, the external peak, and the
    /// cooldown or reclaim backoff.
    fn ram_held_back(&self) -> HeldBack {
        let (config, now) = (self.config, self.now);
        let available = self
            .snapshot
            .total_ram_mib
            .saturating_sub(self.external_of(&ResourceKey::Ram));
        let mut held = HeldBack {
            floor: self.ram_reserve,
            ..HeldBack::default()
        };
        if self.ram_unknown || self.ram_held {
            held.unknown = available.saturating_sub(self.ram_reserve);
            return held;
        }
        held.margin = available
            .saturating_sub(self.ram_reserve)
            .saturating_sub(self.budget.ram_mib);
        held.peak = self
            .peak_excess
            .get(&ResourceKey::Ram)
            .copied()
            .unwrap_or(0);
        let pre = self
            .budget
            .ram_mib
            .saturating_sub(self.charged.ram_mib)
            .saturating_sub(held.peak);
        (held.cooldown, held.backoff) = withheld(
            pre,
            cooled(
                config,
                now,
                self.next.ram_stable_since,
                ResourceKind::Ram,
                0,
            ),
            self.ram_allowed,
        );
        held.charge_shortfall = charge_shortfall(self.input, &self.charges, &|r| r.ram_mib, false);
        held
    }

    /// A device's memory held back: the floor, the mode's margin, the external peak, and the
    /// cooldown or reclaim backoff. The noise tolerance is a growth threshold that withholds
    /// nothing, so it is not a part (SC-016).
    fn gpu_memory_held_back(&self, gpu: &GpuSnapshot) -> HeldBack {
        let (config, now) = (self.config, self.now);
        let key = ResourceKey::GpuMemory(gpu.uuid.clone());
        let floor = config.vram_floor_mib(gpu.total_memory_mib);
        let available = gpu
            .total_memory_mib
            .saturating_sub(floor)
            .saturating_sub(self.external_of(&key));
        let allowance = self
            .budget
            .gpu_memory_mib
            .get(&gpu.uuid)
            .copied()
            .unwrap_or(0);
        let mut held = HeldBack {
            floor,
            ..HeldBack::default()
        };
        if self
            .statuses
            .get(&key)
            .is_some_and(|s| s.measurement == Some(Measurement::Unknown))
        {
            held.unknown = available;
            return held;
        }
        held.margin = available.saturating_sub(allowance);
        held.peak = self.peak_excess.get(&key).copied().unwrap_or(0);
        let pre = allowance
            .saturating_sub(
                self.charged
                    .gpu_memory_mib
                    .get(&gpu.uuid)
                    .copied()
                    .unwrap_or(0),
            )
            .saturating_sub(held.peak);
        let since = self
            .next
            .previous_gpus
            .get(&gpu.uuid)
            .and_then(|p| p.stable_since);
        let elapsed = since.map(|since| now - since);
        let cooldown = config.gpu_memory_cooldown_ms(governing(config, ResourceKind::GpuMemory));
        let backoff = backoff_for(config, &self.next.reclaim_episodes, &key);
        (held.cooldown, held.backoff) = withheld(
            pre,
            elapsed.is_some_and(|ms| ms >= cooldown),
            elapsed.is_some_and(|ms| ms >= cooldown.saturating_add(backoff)),
        );
        let uuid = gpu.uuid.clone();
        held.charge_shortfall = charge_shortfall(
            self.input,
            &self.charges,
            &move |r: &Resources| r.gpu_memory_mib.get(&uuid).copied().unwrap_or(0),
            true,
        );
        held
    }

    /// GPU compute held back: what own work leaves beside external compute, and
    /// whether its amount was measured or why not (FR-021).
    fn gpu_compute_held_back(&self, gpu: &GpuSnapshot) -> HeldBack {
        let mut held = HeldBack {
            compute_path: self.compute_paths.get(&gpu.uuid).cloned(),
            ..HeldBack::default()
        };
        match self.gpu_compute_budget.get(&gpu.uuid) {
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
        held
    }

    /// What the supervisors report about how their steps hold, as catalog codes.
    pub(super) fn step_codes(&mut self) {
        let input = self.input;
        for allocation in &input.allocations {
            let Some(facts) = input.facts.get(&allocation.id) else {
                continue;
            };
            // Processes outside the verified family are neither own work nor stepped (FR-082).
            if facts.outside_family > 0 {
                let reason = facts
                    .outside_reason
                    .as_deref()
                    .map_or(String::new(), |reason| format!(" ({reason})"));
                for key in [ResourceKey::Cpu, ResourceKey::Ram] {
                    note(
                        &mut self.statuses,
                        key,
                        "single_process_violated",
                        format!(
                            "allocation {}: {} processes outside its verified family{reason} are not measured as own work or stepped",
                            allocation.id, facts.outside_family
                        ),
                    );
                }
            }
            // A family of more than its leader is measured and stepped as one (FR-082).
            if let Some(family) = facts.family
                && family.sdk_children + family.descendants > 0
            {
                for key in [ResourceKey::Cpu, ResourceKey::Ram] {
                    note(
                        &mut self.statuses,
                        key,
                        "family_members",
                        format!(
                            "allocation {}: {} processes, the leader, {} SDK children and {} descendants, are measured and stepped as own work",
                            allocation.id,
                            family.leader + family.sdk_children + family.descendants,
                            family.sdk_children,
                            family.descendants
                        ),
                    );
                }
            }
            for (key, code) in &facts.step_codes {
                if let Some(text) = crate::contention::reason_text(code) {
                    note(
                        &mut self.statuses,
                        key.clone(),
                        code,
                        format!("allocation {}: {text}", allocation.id),
                    );
                }
            }
        }
    }

    /// Own work is reclaimed while it holds more than its target, or a step is
    /// still moving down, by more than the tolerance; a cap or hint that holds
    /// at its target is not.
    fn stepped(&self, key: &ResourceKey) -> bool {
        self.over_target
            .get(key)
            .is_some_and(|excess| *excess > jitter_tolerance(self.config, &self.next.ladder, key))
            || self.moving_down.contains(key)
    }

    /// One resource's admission: blocked until its cause changes, held while it
    /// waits for a measurement or a cooldown (FR-028, FR-038).
    fn admission(
        &self,
        key: &ResourceKey,
        unknown: bool,
        gpu_reason: Option<&String>,
        cooled_down: bool,
        backed_off: bool,
    ) -> Admission {
        let input = self.input;
        if input.explicit_drain {
            Admission::blocked("admission_draining")
        } else if input.spool_full {
            Admission::blocked("admission_spool_exhausted")
        } else if self.breached.contains(key) {
            Admission::blocked("admission_floor_breached")
        } else if unknown {
            Admission::held("admission_measurement_unknown")
        } else if let Some(code) = gpu_reason {
            Admission::held(code)
        } else if self.stepped(key) {
            Admission::blocked("admission_own_work_yielding")
        } else if !cooled_down {
            Admission::held("admission_cooling_down")
        } else if !backed_off {
            Admission::held("admission_reclaim_backoff")
        } else {
            Admission::Open
        }
    }

    /// Admission per resource, and a note when own charges fill the CPU target.
    pub(super) fn admissions(&mut self) {
        let (config, snapshot, now) = (self.config, self.snapshot, self.now);
        let mut admissions = vec![
            (
                ResourceKey::Cpu,
                self.admission(
                    &ResourceKey::Cpu,
                    self.cpu_unknown || self.cpu_held,
                    None,
                    self.cpu_allowed,
                    true,
                ),
            ),
            (
                ResourceKey::Ram,
                self.admission(
                    &ResourceKey::Ram,
                    self.ram_unknown || self.ram_held,
                    None,
                    cooled(
                        config,
                        now,
                        self.next.ram_stable_since,
                        ResourceKind::Ram,
                        0,
                    ),
                    self.ram_allowed,
                ),
            ),
        ];
        for gpu in &snapshot.gpus {
            let key = ResourceKey::GpuMemory(gpu.uuid.clone());
            let elapsed = self
                .next
                .previous_gpus
                .get(&gpu.uuid)
                .and_then(|p| p.stable_since)
                .map(|since| now - since);
            let cooldown =
                config.gpu_memory_cooldown_ms(governing(config, ResourceKind::GpuMemory));
            let backoff = backoff_for(config, &self.next.reclaim_episodes, &key);
            let state = self.admission(
                &key,
                false,
                self.gpu_block_reasons.get(&gpu.uuid),
                elapsed.is_some_and(|ms| ms >= cooldown),
                elapsed.is_some_and(|ms| ms >= cooldown.saturating_add(backoff)),
            );
            admissions.push((key, state));
        }
        for gpu in &snapshot.gpus {
            let key = ResourceKey::GpuCompute(gpu.uuid.clone());
            if self.stepped(&key) {
                admissions.push((key, Admission::blocked("admission_own_work_yielding")));
            }
        }
        for (key, state) in admissions {
            // Only a hold for a first measurement or the cooldown can be the burst's
            // own; reclaim, floors, drains and backoff never are (FR-028).
            let own_new = match key {
                ResourceKey::Cpu => self.cpu_new_only && !self.stepped(&key),
                ResourceKey::Ram => self.ram_new_only && !self.stepped(&key),
                _ => false,
            } && matches!(
                state.code(),
                Some("admission_measurement_unknown" | "admission_cooling_down")
            );
            let status = self.statuses.entry(key).or_default();
            status.admission = Some(state);
            status.admission_own_new = own_new;
        }
        // Admission is open but own charges fill own target: requests wait.
        if self.headroom.cpu_millicores == 0
            && self.budget.cpu_millicores > 0
            && self
                .statuses
                .get(&ResourceKey::Cpu)
                .and_then(|s| s.admission.as_ref())
                == Some(&Admission::Open)
        {
            note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "admission_over_own_target",
                format!(
                    "own charges fill the CPU target of {}m, so a request waits",
                    self.budget.cpu_millicores
                ),
            );
        }
    }

    /// The allowance (FR-071): what each running allocation may grow to now
    /// without causing a step. Its usage plus an equal part of the room under
    /// the mode's allowance after pending reservations, less one publish
    /// threshold in total, which covers the falls left unpublished.
    pub(super) fn publish_allowances(&mut self) {
        let snapshot = self.snapshot;
        let mut keys = vec![
            (
                ResourceKey::Cpu,
                self.cpu_unknown || self.cpu_held,
                self.cpu_capacity,
            ),
            (
                ResourceKey::Ram,
                self.ram_unknown || self.ram_held,
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
            self.allowance_on(key, unknown, capacity);
        }
    }

    /// One resource's allowances. A change publishes beyond the contention floor
    /// (CPU) or the noise tolerance (memory); a rise moves at most one step, the
    /// mode's margin of capacity, per scale-up interval.
    fn allowance_on(&mut self, key: ResourceKey, unknown: bool, capacity: u64) {
        let (config, prev, now) = (self.config, self.prev, self.now);
        let kind = key.kind();
        let unknown = unknown
            || self
                .statuses
                .get(&key)
                .is_some_and(|s| s.measurement == Some(Measurement::Unknown));
        let thresholds = config.contention.thresholds(kind);
        let threshold = if kind.is_memory() {
            thresholds.noise_tolerance_mib
        } else {
            (capacity.saturating_mul(u64::from(thresholds.principal_min_percent)) / 100).max(1)
        };
        let Some(current) = self.allowance_values(&key, unknown, threshold) else {
            return;
        };
        // Falls publish together once their summed drift passes the one
        // threshold; each rise is rate-limited on its own.
        let drift = current
            .iter()
            .filter_map(|(id, value, _)| {
                prev.allowance_published
                    .get(&(id.clone(), key.clone()))
                    .map(|(previous, _)| previous.saturating_sub(*value))
            })
            .fold(0u64, u64::saturating_add);
        // Once CPU contention has fully ended, the allowance rises in one move (FR-034).
        let ended = key == ResourceKey::Cpu && self.cpu_contention_ended;
        for (allocation_id, value, mode) in current {
            let step = if ended {
                u64::MAX
            } else {
                allowance_step(&thresholds, kind, capacity, threshold, mode)
            };
            let scale_up_ms = if ended {
                0
            } else {
                thresholds.scale_up_interval_ms(mode, config.monitor.interval_ms)
            };
            let id = (allocation_id.clone(), key.clone());
            let published = match prev.allowance_published.get(&id).copied() {
                None => (value, now),
                Some((previous, _)) if value < previous && drift > threshold => (value, now),
                Some((previous, rose_at))
                    if value > previous.saturating_add(threshold)
                        && now.saturating_sub(rose_at) >= scale_up_ms =>
                {
                    (previous + (value - previous).min(step), now)
                }
                Some(kept) => kept,
            };
            self.next.allowance_published.insert(id, published);
            let amounts = self.allowances.entry(allocation_id).or_default();
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

    /// Each running allocation's allowance on a resource before publishing, with
    /// its mode: what it holds plus its part of the room, within its step, the
    /// envelope and, for Guaranteed work, at least its request. None when no
    /// work runs there.
    fn allowance_values(
        &self,
        key: &ResourceKey,
        unknown: bool,
        threshold: u64,
    ) -> Option<Vec<(String, u64, ContentionMode)>> {
        let input = self.input;
        let kind = key.kind();
        let on_resource = |a: &Allocation| match key {
            ResourceKey::GpuMemory(uuid) => a.requested.gpu_memory_mib.contains_key(uuid),
            _ => true,
        };
        // What an allocation holds as yielding judges it; Guaranteed work
        // holds at least its request.
        let held = |a: &Allocation| -> u64 {
            let usage = match (key, &a.observed) {
                (ResourceKey::Cpu, Some(observed)) => observed.cpu_millicores,
                (ResourceKey::Ram, Some(observed)) => observed.ram_mib,
                (ResourceKey::GpuMemory(uuid), Some(observed))
                    if input.facts.get(&a.id).is_some_and(|f| f.hint_ready) =>
                {
                    observed.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
                }
                _ => amount_of(key, &self.charges[&a.id]),
            };
            if is_guaranteed(a) {
                usage.max(amount_of(key, &a.requested))
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
            return None;
        }
        let taken = input
            .allocations
            .iter()
            .filter(|a| on_resource(a))
            .map(|a| match a.phase {
                AllocationPhase::Pending => amount_of(key, &a.requested),
                _ => held(a),
            })
            .fold(0u64, u64::saturating_add);
        let mode_of = |a: &Allocation| self.allocation_modes[&a.id][&kind];
        // The room is shared equally, but no job is invited above its own
        // mode's target; what that bound leaves goes to the others.
        let room = amount_of(key, &self.budget)
            .saturating_sub(taken)
            .saturating_sub(threshold);
        let headroom = |a: &Allocation| {
            self.mode_targets.get(key).map_or(u64::MAX, |targets| {
                targets[&mode_of(a)].saturating_sub(held(a))
            })
        };
        let shares = equal_shares(&running, room, &headroom);
        let mut current = Vec::with_capacity(running.len());
        for allocation in running {
            // An unknown measurement invites no growth.
            let mut value = if unknown {
                held(allocation)
            } else {
                held(allocation).saturating_add(shares[allocation.id.as_str()])
            };
            match self.next.ladder.get(&(allocation.id.clone(), key.clone())) {
                Some(state) if matches!(state.step, YieldStep::Cap | YieldStep::Hint) => {
                    value = value.min(state.target.unwrap_or(value));
                }
                Some(state) if state.step > YieldStep::None => {
                    value = value.min(held(allocation));
                }
                _ => {}
            }
            if let Some(envelope) = &input.envelope {
                value = value.min(amount_of(key, envelope));
            }
            if is_guaranteed(allocation) {
                value = value.max(amount_of(key, &allocation.requested));
            }
            current.push((allocation.id.clone(), value, mode_of(allocation)));
        }
        Some(current)
    }
}

/// The cooldown backoff of a memory resource's recent reclaim episodes.
fn backoff_for(
    config: &Config,
    episodes: &BTreeMap<ResourceKey, Vec<u64>>,
    key: &ResourceKey,
) -> u64 {
    let count = episodes.get(key).map_or(0, Vec::len) as u32;
    reclaim_backoff_ms(&config.contention.thresholds(key.kind()), count)
}

/// Moves within the noise tolerance (memory), or within the reversal
/// hysteresis of the external growth a compute step answered, are jitter:
/// they reset no cooldown and block no admission.
fn jitter_tolerance(
    config: &Config,
    ladder: &BTreeMap<(String, ResourceKey), StepState>,
    key: &ResourceKey,
) -> u64 {
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
}

/// Whether a resource has been safe since `since` for its mode's cooldown plus
/// `extra_ms`.
fn cooled(
    config: &Config,
    now: u64,
    since: Option<u64>,
    kind: ResourceKind,
    extra_ms: u64,
) -> bool {
    since.is_some_and(|since| {
        now - since
            >= config
                .contention
                .thresholds(kind)
                .cooldown_ms(governing(config, kind), config.monitor.interval_ms)
                .saturating_add(extra_ms)
    })
}

/// The most an allowance rises per scale-up interval: the mode's step of
/// capacity, above the publish threshold.
fn allowance_step(
    thresholds: &ResourceThresholds,
    kind: ResourceKind,
    capacity: u64,
    threshold: u64,
    mode: ContentionMode,
) -> u64 {
    let percent = if kind.is_memory() {
        mode.memory_allowance_step_percent()
    } else {
        thresholds.margin_percent(mode)
    };
    (capacity.saturating_mul(u64::from(percent)) / 100).max(threshold + 1)
}

/// Share `room` equally among `running`, giving none more than its headroom;
/// what that bound leaves goes to the others.
fn equal_shares<'b>(
    running: &[&'b Allocation],
    room: u64,
    headroom: &dyn Fn(&Allocation) -> u64,
) -> BTreeMap<&'b str, u64> {
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
        let bound: Vec<&&&Allocation> = open.iter().filter(|a| headroom(a) < equal).collect();
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
    shares
}

/// What running work's charges hold above its observed usage, counting only
/// work that takes a hint when `ready_only`.
fn charge_shortfall(
    input: &PolicyInput,
    charges: &BTreeMap<String, Resources>,
    pick: &dyn Fn(&Resources) -> u64,
    ready_only: bool,
) -> u64 {
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
}

/// Room withheld while a resource cools down, or after that while its reclaim
/// backoff runs, as (cooldown, backoff).
fn withheld(pre: u64, cooled_down: bool, backed_off: bool) -> (u64, u64) {
    if !cooled_down {
        (pre, 0)
    } else if !backed_off {
        (0, pre)
    } else {
        (0, 0)
    }
}
