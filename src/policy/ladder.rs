//! The yield ladder phases: who yields on each resource for what, how the other
//! steps hold or reverse, and the steps as actions.
use super::*;

impl<'a> Interval<'a> {
    /// The yield ladder. An operator drain stops all work; otherwise memory, CPU
    /// and GPU compute each step per allocation, every step that went down says
    /// why, and every step becomes an action.
    /// OOM order by fault (FR-023): while own RAM is above its fair share, the allocations
    /// that make up the excess, in the memory victim order, are preferred OOM victims; the
    /// opt-in marks every own allocation. While RAM is unknown the marks hold.
    pub(super) fn oom_order(&mut self) {
        let (config, input) = (self.config, self.input);
        let running = || {
            input
                .allocations
                .iter()
                .filter(|a| a.phase != AllocationPhase::Pending)
        };
        if config.ram.prefer_own_oom_victim {
            self.next.oom_preferred = running().map(|a| a.id.clone()).collect();
            return;
        }
        let Some(fair) = self.memory_fair_share.get(&ResourceKey::Ram).copied() else {
            self.next.oom_preferred = self.prev.oom_preferred.clone();
            return;
        };
        let key = ResourceKey::Ram;
        let total = running()
            .map(|a| judged(input, &self.charges, &key, a))
            .fold(0u64, u64::saturating_add);
        let excess = total.saturating_sub(fair);
        self.next.oom_preferred.clear();
        if excess == 0 {
            return;
        }
        let mut victims: Vec<VictimFacts> = running()
            .map(|a| {
                let amount = judged(input, &self.charges, &key, a);
                let facts = ladder_facts(input, &key, &a.id);
                VictimFacts {
                    id: a.id.clone(),
                    controllable: !facts.uncontrollable,
                    frees: amount > 0,
                    pending: false,
                    mode: self.allocation_modes[&a.id][&ResourceKind::Ram],
                    guaranteed: is_guaranteed(a),
                    progress_lost_ms: facts.progress_lost_ms,
                    amount,
                }
            })
            .collect();
        victim_order(&mut victims, excess);
        let mut covered = 0u64;
        for victim in victims {
            if covered >= excess {
                break;
            }
            covered = covered.saturating_add(victim.amount);
            self.next.oom_preferred.insert(victim.id);
        }
    }

    pub(super) fn plan_ladder(&mut self) {
        self.unknown_windows();
        if self.input.explicit_drain {
            self.drain_all();
            return;
        }
        // Memory first because only its holder can release it, then CPU.
        let mut keys = vec![ResourceKey::Ram];
        keys.extend(
            self.charged
                .gpu_memory_mib
                .keys()
                .map(|uuid| ResourceKey::GpuMemory(uuid.clone())),
        );
        keys.push(ResourceKey::Cpu);
        for key in keys {
            self.plan_resource(key);
        }
        let budgets = std::mem::take(&mut self.gpu_compute_budget);
        for (uuid, budget) in &budgets {
            if self.remeasuring(uuid, budget.own.is_some()) {
                let key = ResourceKey::GpuCompute(uuid.clone());
                if let Some(record) = self
                    .statuses
                    .get_mut(&key)
                    .and_then(|s| s.gpu_demand.as_mut())
                {
                    record.window = true;
                }
                self.hold_remeasured(uuid);
            } else {
                self.plan_gpu_compute(uuid, budget);
            }
            self.hold_slice_duty(uuid, budget);
        }
        self.hold_unbudgeted_gpu_compute(&budgets);
        self.gpu_compute_budget = budgets;
        self.note_reversals();
        self.collect_actions();
        self.record_sent_caps();
    }

    /// Whether this decision is part of a re-measure window on the device, opening one when the
    /// last uncapped interval is `GPU_DEMAND_WINDOW_EVERY_MS` old and own work there is held back
    /// by GPU compute caps alone, in a mode other than Yield First, beside a measured amount
    /// (FR-021, Q19). An amount of unknown activity ends a window: only a measured interval
    /// re-measures, and a cap for unknown activity is not the estimate's to lift.
    fn remeasuring(&mut self, uuid: &str, measured: bool) -> bool {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let compute = ResourceKey::GpuCompute(uuid.to_string());
        let unknown_cause = prev.ladder.iter().any(|((_, key), state)| {
            key == &compute && state.cause == YieldCause::UnknownMeasurement
        });
        let facts: Vec<RemeasureFacts> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running
                    && a.requested.gpu_memory_mib.contains_key(uuid)
                    && !(self.time_slice_holds.contains(uuid)
                        && self.time_slice_exempt(&a.id, uuid))
            })
            .map(|a| RemeasureFacts {
                yield_first: self
                    .allocation_modes
                    .get(&a.id)
                    .and_then(|modes| modes.get(&ResourceKind::GpuCompute))
                    .is_some_and(|mode| *mode == ContentionMode::YieldFirst),
                gpu_step: prev
                    .ladder
                    .get(&(a.id.clone(), compute.clone()))
                    .map_or(YieldStep::None, |s| s.step),
                cpu_step: prev
                    .ladder
                    .get(&(a.id.clone(), ResourceKey::Cpu))
                    .map_or(YieldStep::None, |s| s.step),
            })
            .collect();
        let Some(demand) = self.next.gpu_demand.get_mut(uuid) else {
            return false;
        };
        // Without a cap to lift, as under the time-slice rule, no window runs.
        if !measured || unknown_cause || facts.is_empty() {
            demand.window = None;
            return false;
        }
        if let Some(window) = demand.window.as_mut() {
            window.decisions_left = window.decisions_left.saturating_sub(1);
            self.remeasured.insert(uuid.to_string());
            return true;
        }
        // Until own work's share exists a window opens at once, and one ending short opens the
        // next, since a window feeds only 4–5 uncapped intervals (FR-021, Q24). The time-slice
        // duty needs no own share, so there a window opens at once only with none (Q28).
        let bootstrap = if self.time_slice_duty.contains_key(uuid) {
            demand.uncapped.is_empty()
        } else {
            demand.own_share().is_none()
        };
        let due = bootstrap
            || demand
                .last_uncapped_ms
                .is_none_or(|last| now.saturating_sub(last) >= GPU_DEMAND_WINDOW_EVERY_MS);
        if !due
            || governing(config, ResourceKind::GpuCompute) == ContentionMode::YieldFirst
            || !remeasure_window_allowed(&facts)
        {
            return false;
        }
        demand.window = Some(RemeasureWindow {
            decisions_left: GPU_DEMAND_WINDOW_DECISIONS - 1,
            before: demand.estimate().map(|(total, _)| total),
            fed: Vec::new(),
        });
        self.remeasured.insert(uuid.to_string());
        true
    }

    /// In a re-measure window each GPU compute step on the device holds as it was, so the cap
    /// returns unchanged after the window, and none is sent meanwhile. Its dwell restarts, since
    /// the window's uncapped intervals do not show whether the cap holds (FR-021, Q24).
    fn hold_remeasured(&mut self, uuid: &str) {
        let compute = ResourceKey::GpuCompute(uuid.to_string());
        for ((id, key), state) in &self.prev.ladder {
            if key == &compute
                && self.input.allocations.iter().any(|a| &a.id == id)
                && !self.time_slice_exempt(id, uuid)
            {
                let mut state = state.clone();
                state.entered_at_ms = self.now;
                self.next.ladder.insert((id.clone(), key.clone()), state);
            }
        }
    }

    /// Whether this decision sends own work on each device a cap or pause, on CPU or that
    /// device's GPU compute, for the next intervals' demand estimate (FR-021, Q19).
    fn record_sent_caps(&mut self) {
        let input = self.input;
        for (uuid, demand) in self.next.gpu_demand.iter_mut() {
            let compute = ResourceKey::GpuCompute(uuid.clone());
            // A GPU compute cap sent at full duty holds nothing back.
            let capped = self.actions.iter().any(|action| {
                let full = action.resource == compute
                    && action.step == YieldStep::Cap
                    && action.duty.is_some_and(|duty| duty >= GPU_COMPUTE_CAPACITY);
                matches!(action.step, YieldStep::Cap | YieldStep::Pause)
                    && !full
                    && (action.resource == ResourceKey::Cpu || action.resource == compute)
                    && input.allocations.iter().any(|a| {
                        a.id == action.allocation_id
                            && a.phase == AllocationPhase::Running
                            && a.requested.gpu_memory_mib.contains_key(uuid)
                    })
            });
            demand.sent_capped = [capped, demand.sent_capped[0]];
            let duty = self
                .actions
                .iter()
                .filter(|action| action.resource == compute)
                .filter_map(|action| action.duty)
                .min();
            if let Some(record) = self
                .statuses
                .get_mut(&compute)
                .and_then(|status| status.gpu_demand.as_mut())
            {
                record.duty = duty;
            }
        }
    }

    /// Since when each unknown or held resource, a device's memory included, and each
    /// allocation of unknown usage, has been unknown.
    fn unknown_windows(&mut self) {
        let (prev, now) = (self.prev, self.now);
        let gpu_memory: Vec<(ResourceKey, bool)> = self
            .gpu_memory_unknown
            .iter()
            .map(|uuid| (ResourceKey::GpuMemory(uuid.clone()), true))
            .collect();
        for (key, unknown) in [
            (ResourceKey::Cpu, self.cpu_unknown || self.cpu_held),
            (ResourceKey::Ram, self.ram_unknown || self.ram_held),
        ]
        .into_iter()
        .chain(gpu_memory)
        {
            if unknown {
                let start = prev.unknown_since.get(&key).copied().unwrap_or(now);
                self.next.unknown_since.insert(key, start);
            }
        }
        for id in &self.unknown_allocations {
            let start = prev
                .allocation_unknown_since
                .get(id)
                .copied()
                .unwrap_or(now);
            self.next.allocation_unknown_since.insert(id.clone(), start);
        }
    }

    /// A device without a compute budget this interval, its telemetry unknown or the device
    /// missing, keeps each GPU compute step as it was while the allocation is there (FR-008).
    fn hold_unbudgeted_gpu_compute(&mut self, budgets: &BTreeMap<String, GpuComputeBudget>) {
        let (input, prev) = (self.input, self.prev);
        for ((id, key), state) in &prev.ladder {
            let ResourceKey::GpuCompute(uuid) = key else {
                continue;
            };
            if !budgets.contains_key(uuid) && input.allocations.iter().any(|a| &a.id == id) {
                self.next
                    .ladder
                    .insert((id.clone(), key.clone()), state.clone());
            }
        }
    }

    /// Explicit node drain keeps its all-allocation stop semantics.
    fn drain_all(&mut self) {
        let input = self.input;
        let mut candidates: Vec<_> = input
            .allocations
            .iter()
            .filter(|a| a.phase != AllocationPhase::Draining)
            .collect();
        candidates.sort_by_key(|a| (a.phase != AllocationPhase::Pending, a.id.as_str()));
        for allocation in candidates {
            self.actions.push(YieldAction {
                allocation_id: allocation.id.clone(),
                resource: first_charged_resource(&self.charges[&allocation.id]),
                step: YieldStep::Drain,
                target: None,
                cause: YieldCause::OperatorDrain,
                code: "yield_step_drain".into(),
                deadline_ms: None,
                duty: None,
            });
        }
    }

    /// One memory resource or CPU: who yields for the excess, pending
    /// reservations first, and how the other steps on it hold or reverse.
    fn plan_resource(&mut self, key: ResourceKey) {
        let frame = self.resource_frame(key);
        let own_hold = frame.kind == ResourceKind::Cpu && self.cpu_own_hold;
        let mut planned = BTreeSet::new();
        for pending_pass in [true, false] {
            self.plan_pass(&frame, pending_pass, &mut planned);
        }
        if own_hold {
            self.hold_unchanged(&frame, &planned);
        } else {
            self.hold_or_reverse(&frame, &planned);
        }
    }

    /// Whether this interval's CPU budget fell beyond its per-interval bound since the last
    /// target, by external growth or contention. Without a last target, external usage beyond
    /// the bound counts as having risen.
    fn cpu_budget_fell(&self) -> bool {
        let external = self.external_of(&ResourceKey::Cpu);
        match self.prev.cpu_target {
            Some((target, before)) => {
                self.budget.cpu_millicores.saturating_add(self.cpu_bound) < target
                    || external > before.saturating_add(self.cpu_bound)
            }
            None => external > self.cpu_bound,
        }
    }

    /// During an enforcement hold, every held step on the resource stays exactly as it was.
    fn hold_unchanged(&mut self, frame: &Frame, planned: &BTreeSet<String>) {
        let (input, prev) = (self.input, self.prev);
        for ((id, step_key), state) in &prev.ladder {
            if step_key != &frame.key || planned.contains(id) {
                continue;
            }
            let Some(allocation) = input.allocations.iter().find(|a| &a.id == id) else {
                continue;
            };
            if replanned_pending_drain(
                Some(state),
                frame.kind,
                allocation.phase == AllocationPhase::Pending,
                is_guaranteed(allocation),
            ) {
                continue;
            }
            self.next
                .ladder
                .insert((id.clone(), step_key.clone()), state.clone());
        }
    }

    /// One resource's facts for the ladder: whether its measurement is known,
    /// the cause, timing, and the excess to reclaim.
    fn resource_frame(&mut self, key: ResourceKey) -> Frame {
        let (config, input, now) = (self.config, self.input, self.now);
        let kind = key.kind();
        let held = self.next.unknown_since.contains_key(&key);
        // Unknown CPU with no view to hold to never escalates (FR-022).
        let measurement_known = !held
            || !(kind == ResourceKind::Cpu && self.cpu_unviewed)
                && self.next.unknown_since.get(&key).is_some_and(|start| {
                    now.saturating_sub(*start) >= unknown_window(config, kind)
                });
        let gpu_unknown = matches!(&key, ResourceKey::GpuMemory(uuid) if self.blocked_gpus.contains(uuid))
            && self
                .statuses
                .get(&key)
                .is_some_and(|s| s.measurement == Some(Measurement::Unknown));
        // Existing drains are planned releases for choosing more victims only.
        let draining_charge = input
            .allocations
            .iter()
            .filter(|a| a.phase == AllocationPhase::Draining)
            .map(|a| amount_of(&key, &self.charges[&a.id]))
            .fold(0u64, u64::saturating_add);
        let held_charge = amount_of(&key, &self.charged).saturating_sub(draining_charge);
        let judged_total = input
            .allocations
            .iter()
            .filter(|a| a.phase != AllocationPhase::Pending && a.phase != AllocationPhase::Draining)
            .map(|a| judged(input, &self.charges, &key, a))
            .fold(0u64, u64::saturating_add);
        let (floor_breach, envelope_breach) = self.breaches(&key, judged_total);
        // A breach episode in which own memory is within its fair share when that share is
        // first known is others' growth: its in-share tasks are asked once to save their
        // progress (FR-075).
        let decided = self.prev.save_progress_decided.contains(&key);
        let fair = self.memory_fair_share.get(&key).copied();
        if kind.is_memory() && floor_breach && (decided || fair.is_some()) {
            self.next.save_progress_decided.insert(key.clone());
        }
        if kind.is_memory()
            && floor_breach
            && !decided
            && fair.is_some_and(|fair| judged_total <= fair)
        {
            self.save_progress.extend(
                input
                    .allocations
                    .iter()
                    .filter(|a| {
                        a.phase != AllocationPhase::Pending
                            && a.phase != AllocationPhase::Draining
                            && judged(input, &self.charges, &key, a) > 0
                    })
                    .map(|a| a.id.clone()),
            );
        }
        let mut cause = if held || gpu_unknown {
            YieldCause::UnknownMeasurement
        } else if floor_breach {
            YieldCause::FloorBreach
        } else if envelope_breach {
            YieldCause::EnvelopeBreach
        } else {
            YieldCause::ExternalGrowth
        };
        let critical = self.critical_keys.contains(&key);
        let timing = self.resource_timing(&key, critical);
        // Pending reservations are cancelled by their charge. CPU in use is
        // measured by usage against the mode's allowance, since a cap lowers
        // usage but never the request.
        let admission_excess = held_charge.saturating_sub(amount_of(&key, &self.budget));
        let cpu_usage = input
            .allocations
            .iter()
            .filter(|a| a.phase != AllocationPhase::Pending)
            .map(cpu_usage_of)
            .fold(self.unknown_charge.cpu_millicores, u64::saturating_add);
        // Running work yields for what it holds, not for its charge: a
        // charge-only shortfall blocks admission and steps nothing.
        let mut usage_excess = if kind == ResourceKind::Cpu {
            cpu_usage.saturating_sub(self.budget.cpu_millicores)
        } else {
            judged_total.saturating_sub(amount_of(&key, &self.budget))
        };
        // Floors by mode (FR-023): in Performance and Aggressive a floor breach takes only own
        // memory above its fair share, while a measurement is held too; in-share memory of work
        // in those modes stays. The envelope still applies in full.
        if floor_breach
            && governing(config, kind) >= ContentionMode::Performance
            && let Some(fair) = self.memory_fair_share.get(&key).copied()
        {
            let protected = input
                .allocations
                .iter()
                .filter(|a| {
                    a.phase != AllocationPhase::Pending
                        && a.phase != AllocationPhase::Draining
                        && self.allocation_modes[&a.id][&kind] >= ContentionMode::Performance
                })
                .map(|a| judged(input, &self.charges, &key, a))
                .fold(0u64, u64::saturating_add)
                .min(fair);
            let envelope_excess = input.envelope.as_ref().map_or(0, |envelope| {
                judged_total.saturating_sub(amount_of(&key, envelope))
            });
            usage_excess =
                usage_excess.min(judged_total.saturating_sub(protected).max(envelope_excess));
        }
        if usage_excess > 0 {
            note_excess(&mut self.over_target, &key, usage_excess);
        }
        // Own usage over its target while the budget did not fall is enforced first: held
        // caps keep their targets, which the actuator holds from the measured rate, and no
        // step is entered until usage has stayed over the target for the dwell with every
        // held cap applied; then it steps with its own cause (FR-011).
        if kind == ResourceKind::Cpu
            && cause == YieldCause::ExternalGrowth
            && usage_excess > 0
            && !self.cpu_budget_fell()
        {
            cause = YieldCause::OwnOverTarget;
            let caps_applied =
                self.prev
                    .ladder
                    .iter()
                    .filter(|((_, k), state)| k == &key && state.step == YieldStep::Cap)
                    .all(|((id, _), state)| {
                        now.saturating_sub(state.entered_at_ms) >= timing.dwell_ms
                            || input.facts.get(id).is_some_and(|facts| {
                                facts.applied.get(&key) == Some(&YieldStep::Cap)
                            })
                    });
            let since = if caps_applied {
                self.prev.cpu_over_since.unwrap_or(now)
            } else {
                now
            };
            self.next.cpu_over_since = Some(since);
            self.cpu_own_hold = now.saturating_sub(since) < timing.dwell_ms;
        }
        // Opportunistic work nobody can act on cannot be released first.
        let opportunistic_left = input.allocations.iter().any(|a| {
            !is_guaranteed(a)
                && amount_of(&key, &self.charges[&a.id]) > 0
                && !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable)
        });
        // Guaranteed memory is drained last (FR-024): after its hint deadline
        // when it can take a hint and, for RAM, the breach window. The critical
        // path uses the critical hint deadline and skips the window (FR-063).
        let breached_for = self
            .next
            .floor_breach_since
            .get(&key)
            .map_or(0, |start| now.saturating_sub(*start));
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
        let guaranteed_waits = guaranteed_may_drain(kind, cause, opportunistic_left, u64::MAX, 0);
        // What the mode allows beyond current usage, shared by the caps,
        // pauses and hints on this resource so they can rise again while calm.
        let held_back = self
            .prev
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
            self.budget.cpu_millicores.saturating_sub(cpu_usage) / held_back
        } else {
            amount_of(&key, &self.budget).saturating_sub(judged_total) / held_back
        };
        if kind.is_memory() && (floor_breach || usage_excess > 0) {
            self.next.reclaiming.insert(key.clone());
        }
        // Only while own work holds no excess: a step raised then frees nothing, and the
        // excess would fall on the next victim instead.
        let return_level =
            (kind == ResourceKind::Cpu && self.cpu_contention_returned && usage_excess == 0)
                .then(|| self.return_level(&key, cpu_usage))
                .flatten();
        Frame {
            external: self.external_of(&key),
            key,
            kind,
            measurement_known,
            cause,
            critical,
            timing,
            admission_excess,
            usage_excess,
            guaranteed_drain,
            guaranteed_waits,
            spare_share,
            own_usage: if kind == ResourceKind::Cpu {
                cpu_usage
            } else {
                judged_total
            },
            contention_ended: kind == ResourceKind::Cpu && self.cpu_contention_ended,
            return_level,
        }
    }

    /// The level every CPU step held for external growth rises to when contention has just
    /// ended: filled from the lowest up, each counted at the smaller of its target and its
    /// usage, since room a step does not use is spare, so those steps together hold the mode's
    /// target beside the usage of other own work, and none is lowered (FR-034, R44).
    fn return_level(&self, key: &ResourceKey, cpu_usage: u64) -> Option<u64> {
        let mut targets: Vec<u64> = Vec::new();
        let mut held_usage = 0u64;
        for ((id, k), state) in &self.prev.ladder {
            let Some(allocation) = self.input.allocations.iter().find(|a| &a.id == id) else {
                continue;
            };
            let Some(target) = returning_target(state).filter(|_| k == key) else {
                continue;
            };
            let usage = cpu_usage_of(allocation);
            targets.push(target.min(usage));
            held_usage = held_usage.saturating_add(usage);
        }
        let room = self
            .budget
            .cpu_millicores
            .saturating_sub(cpu_usage.saturating_sub(held_usage));
        water_level(&mut targets, room)
    }

    /// A floor breach or own work beyond the operator envelope on a resource,
    /// noted and timed from when it began. Returns both.
    fn breaches(&mut self, key: &ResourceKey, judged_total: u64) -> (bool, bool) {
        let (input, prev, now) = (self.input, self.prev, self.now);
        let floor_breach = self.floor_breaches.get(key).copied();
        if let Some((free, floor)) = floor_breach {
            self.breached.insert(key.clone());
            self.next.floor_breach_since.insert(
                key.clone(),
                prev.floor_breach_since.get(key).copied().unwrap_or(now),
            );
            note(
                &mut self.statuses,
                key.clone(),
                "floor_breach",
                format!("free memory {free} MiB is below the {floor} MiB floor"),
            );
        }
        let floor_breach = floor_breach.is_some();
        // Own work beyond the operator envelope, judged like a floor breach.
        let envelope_breach = input.envelope.as_ref().is_some_and(|envelope| {
            let held = if key.kind() == ResourceKind::Cpu {
                input
                    .allocations
                    .iter()
                    .filter(|a| a.phase != AllocationPhase::Pending)
                    .map(cpu_usage_of)
                    .fold(self.unknown_charge.cpu_millicores, u64::saturating_add)
            } else {
                judged_total
            };
            held > amount_of(key, envelope)
        });
        if envelope_breach {
            note(
                &mut self.statuses,
                key.clone(),
                "envelope_breach",
                format!(
                    "own work exceeds the operator envelope of {}",
                    amount_of(
                        key,
                        input.envelope.as_ref().unwrap_or(&Resources::default())
                    )
                ),
            );
            if !floor_breach {
                self.next.floor_breach_since.insert(
                    key.clone(),
                    prev.floor_breach_since.get(key).copied().unwrap_or(now),
                );
            }
        }
        (floor_breach, envelope_breach)
    }

    /// A resource's ladder timing: the mode's, with the critical hint deadline on
    /// the critical path (FR-063). Each recent reclaim episode doubles what a
    /// floor-breach step's reversal also waits (FR-013, FR-061).
    fn resource_timing(&self, key: &ResourceKey, critical: bool) -> LadderTiming {
        let (config, now) = (self.config, self.now);
        let kind = key.kind();
        let mut timing = LadderTiming::new(
            &config.contention.thresholds(kind),
            governing(config, kind),
            config.monitor.interval_ms,
        );
        if critical {
            timing.hint_deadline_ms = config
                .contention
                .critical_hint_deadline_s
                .saturating_mul(1000);
            timing.dwell_ms = timing.dwell_ms.min(timing.hint_deadline_ms);
        }
        if kind.is_memory() {
            let thresholds = config.contention.thresholds(kind);
            let window_ms = thresholds.memory_peak_window_s.saturating_mul(1000);
            let episodes = self.prev.reclaim_episodes.get(key).map_or(0, |starts| {
                starts
                    .iter()
                    .filter(|start| now.saturating_sub(**start) < window_ms)
                    .count()
            }) as u32;
            if episodes > 0 {
                timing.breach_backoff_ms = reclaim_backoff_ms(&thresholds, episodes);
            }
        }
        timing
    }

    /// One pass of victims on a resource: pending reservations for the
    /// admission excess, or running work for the usage excess.
    fn plan_pass(&mut self, frame: &Frame, pending_pass: bool, planned: &mut BTreeSet<String>) {
        let (config, input, now) = (self.config, self.input, self.now);
        let (key, kind) = (&frame.key, frame.kind);
        let needed = if pending_pass {
            frame.admission_excess
        } else {
            frame.usage_excess
        };
        let mut victims: Vec<VictimFacts> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase != AllocationPhase::Draining
                    && self
                        .next
                        .allocation_unknown_since
                        .get(&a.id)
                        .is_none_or(|start| {
                            now.saturating_sub(*start) >= unknown_window(config, kind)
                        })
            })
            .filter(|a| (a.phase == AllocationPhase::Pending) == pending_pass)
            .map(|a| {
                let amount = match (kind, pending_pass) {
                    (_, true) => amount_of(key, &self.charges[&a.id]),
                    (ResourceKind::Cpu, false) => cpu_usage_of(a).saturating_sub(kept_cpu(kind, a)),
                    (_, false) => judged(input, &self.charges, key, a),
                };
                let facts = ladder_facts(input, key, &a.id);
                VictimFacts {
                    id: a.id.clone(),
                    controllable: !facts.uncontrollable,
                    frees: amount > 0,
                    pending: pending_pass,
                    mode: self.allocation_modes[&a.id][&kind],
                    guaranteed: is_guaranteed(a),
                    progress_lost_ms: facts.progress_lost_ms,
                    amount,
                }
            })
            .collect();
        victim_order(&mut victims, needed);
        if !pending_pass && kind == ResourceKind::Cpu && self.cpu_own_hold {
            // While own usage over its target is enforced, only a job above its own mode's
            // target yields, that difference; the other steps stay as they are (FR-011).
            for victim in victims {
                let above_own_target = input.allocations.iter().any(|a| {
                    a.id == victim.id
                        && self
                            .ceiling(key, a)
                            .is_some_and(|limit| victim.amount + kept_cpu(kind, a) > limit)
                });
                if above_own_target {
                    planned.insert(victim.id.clone());
                    self.plan_victim(frame, victim, pending_pass, false, &mut 0);
                }
            }
            return;
        }
        // Running CPU yields tier by tier (FR-016). Within a tier, caps already held are lowered
        // at one level, as they rise equally (FR-013), and own usage over its target is shared
        // the same way (FR-011), so no single cap falls to the minimum duty and escalates; new
        // victims then share what remains by usage, at one level, so none is capped to 0 for it.
        if !pending_pass && kind == ResourceKind::Cpu {
            let own_cause = frame.cause == YieldCause::OwnOverTarget;
            let held = |v: &VictimFacts| {
                self.prev
                    .ladder
                    .get(&(v.id.clone(), key.clone()))
                    .is_some_and(|state| state.step == YieldStep::Cap)
            };
            let tier = |v: &VictimFacts| (v.frees, v.pending, v.mode, v.guaranteed);
            let mut remaining = needed;
            let mut rest = victims.as_slice();
            let mut tiers = Vec::new();
            while let Some(first) = rest.first() {
                let len = rest.iter().take_while(|v| tier(v) == tier(first)).count();
                let (held, new): (Vec<VictimFacts>, Vec<VictimFacts>) = rest[..len]
                    .iter()
                    .cloned()
                    .partition(|v| own_cause || held(v));
                tiers.push((held, new));
                rest = &rest[len..];
            }
            for group in tiers.into_iter().flat_map(|(held, new)| [held, new]) {
                let total: u64 = group.iter().map(|v| v.amount).sum();
                let level = fill_level(&group, remaining.min(total));
                for victim in group {
                    let mut share = victim.amount.saturating_sub(level);
                    remaining = remaining.saturating_sub(share);
                    planned.insert(victim.id.clone());
                    self.plan_victim(frame, victim, pending_pass, false, &mut share);
                }
            }
            return;
        }
        // Over-share tasks give memory back first (FR-016, FR-023): they lead the order, and
        // in-share tasks are asked only for what is left after them.
        let over = if pending_pass {
            Vec::new()
        } else {
            self.over_share(key, &victims, frame.own_usage)
        };
        victims.sort_by_key(|victim| {
            over.iter()
                .position(|id| id == &victim.id)
                .unwrap_or(usize::MAX)
        });
        let mut remaining = needed;
        for victim in victims {
            planned.insert(victim.id.clone());
            let over_share = over.contains(&victim.id);
            self.plan_victim(frame, victim, pending_pass, over_share, &mut remaining);
        }
    }

    /// The over-share tasks on a memory resource: the shortest prefix of the victims, in victim
    /// order, whose memory covers own memory above its fair share, as the OOM mark picks them
    /// (FR-023). None while the share is unknown or own memory is within it.
    fn over_share(&self, key: &ResourceKey, victims: &[VictimFacts], own: u64) -> Vec<String> {
        let Some(excess) = key
            .kind()
            .is_memory()
            .then(|| self.memory_fair_share.get(key))
            .flatten()
            .map(|fair| own.saturating_sub(*fair))
            .filter(|excess| *excess > 0)
        else {
            return Vec::new();
        };
        let mut ordered = victims.to_vec();
        victim_order(&mut ordered, excess);
        let mut covered = 0u64;
        let mut over = Vec::new();
        for victim in ordered {
            if covered >= excess {
                break;
            }
            covered = covered.saturating_add(victim.amount);
            over.push(victim.id);
        }
        over
    }

    /// Performance and Aggressive CPU: the Neutral target, which under contention is own
    /// work's fair share, below which a cap never pauses (FR-022).
    fn compute_floor(
        &self,
        key: &ResourceKey,
        mode: ContentionMode,
        own_usage: u64,
    ) -> Option<ComputeFloor> {
        (governing(self.config, key.kind()) >= ContentionMode::Performance
            && mode >= ContentionMode::Performance)
            .then(|| {
                self.mode_targets
                    .get(key)?
                    .get(&ContentionMode::Neutral)
                    .copied()
            })
            .flatten()
            .map(|fair_share| ComputeFloor {
                fair_share,
                own_usage,
            })
    }

    /// What a job's own mode allows it on a resource; Guaranteed keeps its CPU
    /// request, and memory in Performance and Aggressive its fair share (FR-023).
    fn ceiling(&self, key: &ResourceKey, allocation: &Allocation) -> Option<u64> {
        let kind = key.kind();
        let mode = self.allocation_modes[&allocation.id][&kind];
        let fair = self
            .memory_fair_share
            .get(key)
            .filter(|_| {
                mode >= ContentionMode::Performance
                    && governing(self.config, kind) >= ContentionMode::Performance
            })
            .copied()
            .unwrap_or(0);
        self.mode_targets
            .get(key)
            .map(|targets| targets[&mode].max(kept_cpu(kind, allocation)).max(fair))
    }

    /// One victim's next step on a resource, and what it frees of `remaining`.
    fn plan_victim(
        &mut self,
        frame: &Frame,
        victim: VictimFacts,
        pending_pass: bool,
        over_share: bool,
        remaining: &mut u64,
    ) {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let (key, kind) = (&frame.key, frame.kind);
        let allocation = input.allocations.iter().find(|a| a.id == victim.id);
        let kept = allocation.map_or(0, |a| kept_cpu(kind, a));
        // A running job above its own mode's target yields the difference even
        // while the aggregate has room.
        let limit = allocation
            .and_then(|a| self.ceiling(key, a))
            .filter(|_| !pending_pass);
        let own_excess = limit.map_or(0, |limit| (victim.amount + kept).saturating_sub(limit));
        if own_excess > 0 {
            note_excess(&mut self.over_target, key, own_excess);
        }
        let excess = (*remaining).max(own_excess);
        let mut state = prev.ladder.get(&(victim.id.clone(), key.clone()));
        // A CPU drain planned while the allocation was pending cancelled its reservation; if
        // it runs anyway, it is re-planned as running work, which CPU contention alone never
        // drains (FR-012).
        if replanned_pending_drain(state, kind, pending_pass, victim.guaranteed) {
            state = None;
        }
        // A Guaranteed hint waits the Guaranteed hint deadline (FR-024).
        let mut victim_timing = frame.timing_for(config, victim.mode);
        if victim.guaranteed && kind.is_memory() && !frame.critical {
            victim_timing.hint_deadline_ms = config
                .contention
                .guaranteed_hint_deadline_s
                .saturating_mul(1000);
        }
        let facts = ladder_facts(input, key, &victim.id);
        let mut unavailable = facts.unavailable.get(key).cloned().unwrap_or_default();
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
        // When contention has just ended, a binding step held for external growth rises to the
        // return level in one move, without the dwell; others take their own paths (R44).
        let returning = (!contended)
            .then_some(frame.return_level)
            .flatten()
            .zip(state.and_then(returning_target))
            .filter(|(_, held)| victim.amount.saturating_mul(100) >= held.saturating_mul(95))
            .map(|(level, held)| level.max(held));
        victim_timing.returning = returning.is_some();
        if kind == ResourceKind::Cpu
            && !pending_pass
            && !contended
            && let Some(held) = state.filter(|s| self.holds_own_cap(s, frame))
        {
            self.next
                .ladder
                .insert((victim.id.clone(), key.clone()), held.clone());
            return;
        }
        let mut decision = plan_step(
            state,
            &LadderInput {
                room: if kind.is_memory() {
                    self.floor_room.get(key).copied()
                } else {
                    None
                },
                kind,
                guaranteed: victim.guaranteed,
                // Running work is never drained for CPU alone (SC-002);
                // pause frees it completely.
                allow_drain: if victim.guaranteed {
                    frame.guaranteed_drain
                } else {
                    pending_pass || kind != ResourceKind::Cpu
                },
                unavailable,
                excess: if contended { excess } else { 0 },
                external: frame.external,
                measurement_known: frame.measurement_known,
                cause: frame.cause,
                usage: (kind == ResourceKind::Cpu && !pending_pass)
                    .then(|| victim.amount.saturating_add(kept)),
                compute_floor: if kind == ResourceKind::Cpu && !pending_pass {
                    self.compute_floor(key, victim.mode, frame.own_usage)
                } else {
                    None
                },
                at_minimum_duty: at_minimum_duty(input, key, &victim.id),
                // What the allocation may use: while contended, a cap
                // or hint target; while calm, its usage plus its share
                // of the spare, toward which a held cap rises, and when
                // contention has just ended, the return level; never
                // above its own mode's target.
                target: Some(
                    match (contended, returning) {
                        (true, _) => victim.amount.saturating_sub(excess).saturating_add(kept),
                        (false, Some(level)) => level,
                        _ => victim
                            .amount
                            .saturating_add(frame.spare_share)
                            .saturating_add(kept),
                    }
                    .min(limit.unwrap_or(u64::MAX)),
                ),
            },
            &victim_timing,
            now,
        );
        if !decision.skipped.is_empty() {
            note(
                &mut self.statuses,
                key.clone(),
                "step_skipped_unavailable",
                format!(
                    "allocation {}: unavailable steps {:?} were skipped",
                    victim.id, decision.skipped
                ),
            );
        }
        let before = state.map_or(YieldStep::None, |s| s.step);
        // On the critical path, a worker that does not poll the drain
        // file is sent TERM without the drain-file wait (FR-063).
        if let Some(next) = decision.state.as_mut()
            && frame.critical
            && next.step == YieldStep::Drain
            && next.cause == YieldCause::FloorBreach
            && !facts.observes_drain
        {
            next.step = YieldStep::Terminate;
        }
        // A Guaranteed RAM cap never goes below the request, as on
        // CPU (FR-024).
        if let Some(next) = decision.state.as_mut()
            && kind == ResourceKind::Ram
            && next.step == YieldStep::Cap
            && let Some(allocation) = allocation.filter(|a| is_guaranteed(a))
        {
            next.target = next
                .target
                .map(|target| target.max(allocation.requested.ram_mib));
        }
        if let Some(next) = &decision.state
            && next.step == YieldStep::Hint
        {
            self.hint_deadlines.insert(
                (victim.id.clone(), key.clone()),
                victim_timing.hint_deadline_ms,
            );
        }
        if pending_pass && let Some(next) = decision.state.as_mut() {
            next.pending = true;
        }
        // An over-share task keeps counting toward the excess while it is being handled, so
        // the excess is not passed to in-share tasks (FR-023, FR-037): a drain decided counts
        // in full, also while the task still runs, and a hint within its deadline, or a
        // Guaranteed task whose drain only waits for the breach window, counts what was asked.
        let step = decision.state.as_ref().map(|next| next.step);
        let waiting = over_share
            && kind.is_memory()
            && match &decision.state {
                Some(next) if next.step >= YieldStep::Drain => false,
                Some(next) if next.step == YieldStep::Hint => {
                    now.saturating_sub(next.entered_at_ms) < victim_timing.hint_deadline_ms
                        || (victim.guaranteed && frame.guaranteed_waits)
                }
                _ => victim.guaranteed && frame.guaranteed_waits,
            };
        if over_share && kind.is_memory() && step.is_some_and(|step| step >= YieldStep::Drain) {
            *remaining = remaining.saturating_sub(victim.amount);
        } else if waiting {
            *remaining = remaining.saturating_sub(victim.amount.min(*remaining));
        } else if let Some(next) = &decision.state {
            // A newly applied step, or a cap lowered below usage, is expected to release its
            // share; the next observation shows whether it did. Without counting a lowered
            // cap, every held cap would be lowered by the whole excess.
            let freed = match next.step {
                YieldStep::Cap => (victim.amount + kept)
                    .saturating_sub(next.target.unwrap_or(victim.amount + kept)),
                _ if next.step <= before => 0,
                YieldStep::Pause | YieldStep::Drain | YieldStep::Terminate => victim.amount,
                _ => victim.amount.min(*remaining),
            };
            *remaining = remaining.saturating_sub(freed);
        }
        if let Some(next) = &decision.state {
            self.next
                .ladder
                .insert((victim.id.clone(), key.clone()), next.clone());
        }
    }

    /// Hysteresis for `own_over_target` caps (FR-013): while own work is not below its
    /// target by the contention floor, a held cap within the budget is not relaxed because
    /// one task's usage fell, so own work stays at its target and the contender's share
    /// holds. A cap above the whole budget, as after external growth, no longer binds and
    /// reverses as before.
    fn holds_own_cap(&self, state: &StepState, frame: &Frame) -> bool {
        let floor = (self.cpu_capacity.saturating_mul(u64::from(
            self.config
                .contention
                .thresholds(ResourceKind::Cpu)
                .principal_min_percent,
        )) / 100)
            .max(1);
        state.step == YieldStep::Cap
            && state.cause == YieldCause::OwnOverTarget
            && frame.own_usage.saturating_add(floor) > self.budget.cpu_millicores
            && state
                .target
                .is_some_and(|target| target <= self.budget.cpu_millicores)
    }

    /// Steps of allocations that are no longer candidates hold or reverse.
    fn hold_or_reverse(&mut self, frame: &Frame, planned: &BTreeSet<String>) {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let (key, kind) = (&frame.key, frame.kind);
        for ((id, step_key), state) in &prev.ladder {
            if step_key != key || planned.contains(id) {
                continue;
            }
            let Some(allocation) = input.allocations.iter().find(|a| &a.id == id) else {
                continue;
            };
            if replanned_pending_drain(
                Some(state),
                kind,
                allocation.phase == AllocationPhase::Pending,
                is_guaranteed(allocation),
            ) {
                continue;
            }
            if kind == ResourceKind::Cpu && self.holds_own_cap(state, frame) {
                self.next
                    .ladder
                    .insert((id.clone(), step_key.clone()), state.clone());
                continue;
            }
            let decision = plan_step(
                Some(state),
                &LadderInput {
                    room: if kind.is_memory() {
                        self.floor_room.get(key).copied()
                    } else {
                        None
                    },
                    kind,
                    guaranteed: is_guaranteed(allocation),
                    allow_drain: false,
                    unavailable: ladder_facts(input, key, id)
                        .unavailable
                        .get(key)
                        .cloned()
                        .unwrap_or_default(),
                    excess: 0,
                    external: frame.external,
                    measurement_known: frame.measurement_known
                        && !self.unknown_allocations.contains(id),
                    cause: frame.cause,
                    target: None,
                    usage: None,
                    compute_floor: if kind == ResourceKind::Cpu {
                        self.compute_floor(
                            key,
                            self.allocation_modes[&allocation.id][&kind],
                            frame.own_usage,
                        )
                    } else {
                        None
                    },
                    at_minimum_duty: at_minimum_duty(input, key, id),
                },
                &frame.timing_for(config, self.allocation_modes[&allocation.id][&kind]),
                now,
            );
            if let Some(next) = decision.state {
                self.next.ladder.insert((id.clone(), key.clone()), next);
            }
        }
    }

    /// GPU compute on one device shares by pause duty cycle (FR-021, FR-022):
    /// own work on the device shares it equally, and each allocation's cap
    /// target is its share minus what the device has to free. Compute is
    /// reclaimed within an interval, so it is never drained for GPU compute
    /// alone.
    fn plan_gpu_compute(&mut self, uuid: &str, budget: &GpuComputeBudget) {
        let Some(frame) = self.compute_frame(uuid, budget) else {
            return;
        };
        let planned = self.plan_compute_victims(&frame, budget);
        self.hold_compute(&frame, &planned);
    }

    /// One device's GPU compute facts for the ladder, or None when no own work
    /// runs on it.
    fn compute_frame(&mut self, uuid: &str, budget: &GpuComputeBudget) -> Option<ComputeFrame<'a>> {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let key = ResourceKey::GpuCompute(uuid.to_string());
        let running: Vec<&'a Allocation> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running
                    && a.requested.gpu_memory_mib.contains_key(uuid)
                    && !self.time_slice_exempt(&a.id, uuid)
            })
            .collect();
        if running.is_empty() {
            return None;
        }
        // Activity of unknown amount first holds the steps, as any unknown
        // measurement does, and escalates after the unknown window (FR-008).
        let unknown = budget.own.is_none() && self.gpu_activity_unknown.contains(uuid);
        // Own work on a device shares what it holds equally: its measured
        // usage, else the whole device, but at most a duty its supervisor
        // reports applied, since the duty cycle is own actuation, under activity of
        // unknown amount too. Unreported duties keep the whole device (FR-008).
        let assumed = GPU_COMPUTE_CAPACITY / running.len() as u64;
        let duties: Vec<u64> = running
            .iter()
            .map(|a| applied_duty(prev, input, &key, assumed, a))
            .collect();
        let held = budget.own.unwrap_or_else(|| duties.iter().sum());
        if budget.own.is_none() {
            self.statuses.entry(key.clone()).or_default().own = Some((held, held));
            if duties.iter().any(|duty| *duty < assumed) {
                note(
                    &mut self.statuses,
                    key.clone(),
                    "applied_duty_credit",
                    format!("own work is credited at the applied duty, {held} in total"),
                );
            }
        }
        let share = held / running.len() as u64;
        let excess = held.saturating_sub(budget.share);
        if excess > 0 {
            note_excess(&mut self.over_target, &key, excess);
        }
        // While calm, a held cap rises toward its usage plus its part of
        // what the mode allows beyond own usage.
        let spare_share = budget.share.saturating_sub(held) / running.len() as u64;
        if unknown {
            let since = prev.unknown_since.get(&key).copied().unwrap_or(now);
            self.next.unknown_since.insert(key.clone(), since);
        }
        let measurement_known = !unknown
            || self.next.unknown_since.get(&key).is_some_and(|since| {
                now.saturating_sub(*since) >= unknown_window(config, ResourceKind::GpuCompute)
            });
        Some(ComputeFrame {
            key,
            running,
            share,
            excess,
            spare_share,
            external: budget.external,
            measurement_known,
            cause: if unknown {
                YieldCause::UnknownMeasurement
            } else {
                YieldCause::ExternalGrowth
            },
        })
    }

    /// Who yields GPU compute on a device: a job above its own mode's share
    /// yields the difference, and jobs in victim order yield what the device has
    /// to free. Returns the jobs planned.
    fn plan_compute_victims(
        &mut self,
        frame: &ComputeFrame<'a>,
        budget: &GpuComputeBudget,
    ) -> BTreeSet<String> {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let kind = ResourceKind::GpuCompute;
        let key = &frame.key;
        let mut victims: Vec<VictimFacts> = frame
            .running
            .iter()
            .filter(|a| {
                self.next
                    .allocation_unknown_since
                    .get(&a.id)
                    .is_none_or(|start| now.saturating_sub(*start) >= unknown_window(config, kind))
            })
            .map(|a| VictimFacts {
                id: a.id.clone(),
                controllable: !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable),
                frees: true,
                pending: false,
                mode: self.allocation_modes[&a.id][&kind],
                guaranteed: is_guaranteed(a),
                progress_lost_ms: input.facts.get(&a.id).map_or(0, |f| f.progress_lost_ms),
                amount: frame.share,
            })
            .collect();
        victim_order(&mut victims, frame.excess);
        let mut remaining = frame.excess;
        let mut planned = BTreeSet::new();
        for victim in victims {
            planned.insert(victim.id.clone());
            let state = prev.ladder.get(&(victim.id.clone(), key.clone()));
            // A job above its own mode's share yields the difference even
            // while the aggregate has room.
            let limit = budget.targets.get(&victim.mode).copied();
            let own_excess = limit.map_or(0, |limit| frame.share.saturating_sub(limit));
            if own_excess > 0 {
                note_excess(&mut self.over_target, key, own_excess);
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
                    unavailable: compute_unavailable(
                        input,
                        key,
                        &victim.id,
                        self.no_gpu_pause(&victim.id, key),
                    ),
                    excess: if contended_victim { excess } else { 0 },
                    external: frame.external,
                    measurement_known: frame.measurement_known,
                    cause: frame.cause,
                    // Measured, or under activity of unknown amount credited at the applied
                    // duties, as the frame holds it.
                    usage: (budget.own.is_some() || frame.cause == YieldCause::UnknownMeasurement)
                        .then_some(frame.share),
                    compute_floor: (governing(config, kind) >= ContentionMode::Performance
                        && victim.mode >= ContentionMode::Performance)
                        .then(|| budget.targets.get(&ContentionMode::Neutral))
                        .flatten()
                        .map(|fair_share| ComputeFloor {
                            fair_share: *fair_share,
                            own_usage: frame.share.saturating_mul(frame.running.len() as u64),
                        }),
                    at_minimum_duty: at_minimum_duty(input, key, &victim.id),
                    target: Some(
                        if contended_victim {
                            frame.share.saturating_sub(excess)
                        } else {
                            frame.share.saturating_add(frame.spare_share)
                        }
                        .min(limit.unwrap_or(u64::MAX)),
                    ),
                },
                // Each job yields and rises at its own mode's pace.
                &LadderTiming::new(
                    &config.contention.thresholds(kind),
                    victim.mode,
                    config.monitor.interval_ms,
                ),
                now,
            );
            if !decision.skipped.is_empty() {
                note(
                    &mut self.statuses,
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
                        YieldStep::Cap => frame
                            .share
                            .saturating_sub(next.target.unwrap_or(frame.share)),
                        YieldStep::Pause => frame.share,
                        _ => 0,
                    };
                    remaining = remaining.saturating_sub(freed);
                }
                self.next
                    .ladder
                    .insert((victim.id.clone(), key.clone()), next);
            }
        }
        planned
    }

    /// GPU compute steps of jobs that are no longer victims hold or reverse.
    fn hold_compute(&mut self, frame: &ComputeFrame<'a>, planned: &BTreeSet<String>) {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let kind = ResourceKind::GpuCompute;
        let ResourceKey::GpuCompute(uuid) = &frame.key else {
            return;
        };
        for ((id, step_key), state) in &prev.ladder {
            if step_key != &frame.key || planned.contains(id) || self.time_slice_exempt(id, uuid) {
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
                    guaranteed: is_guaranteed(allocation),
                    allow_drain: false,
                    unavailable: compute_unavailable(
                        input,
                        &frame.key,
                        id,
                        self.no_gpu_pause(id, &frame.key),
                    ),
                    excess: 0,
                    external: frame.external,
                    measurement_known: frame.measurement_known
                        && !self.unknown_allocations.contains(id),
                    cause: frame.cause,
                    target: None,
                    usage: None,
                    compute_floor: None,
                    at_minimum_duty: at_minimum_duty(input, &frame.key, id),
                },
                &LadderTiming::new(
                    &config.contention.thresholds(kind),
                    self.allocation_modes[&allocation.id][&kind],
                    config.monitor.interval_ms,
                ),
                now,
            );
            if let Some(next) = decision.state {
                self.next
                    .ladder
                    .insert((id.clone(), frame.key.clone()), next);
            }
        }
    }

    /// A step that went down, or a cap or hint whose target rose, says why: its
    /// cause no longer holds as it did.
    fn note_reversals(&mut self) {
        let (input, prev) = (self.input, self.prev);
        for ((id, key), before) in &prev.ladder {
            let next = self.next.ladder.get(&(id.clone(), key.clone()));
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
                YieldCause::OwnOverTarget => "step_reversed_own_usage_fell",
                YieldCause::FloorBreach | YieldCause::OomNotOwnFault => {
                    "step_reversed_floor_restored"
                }
                YieldCause::UnknownMeasurement => "step_reversed_measurement_known",
                YieldCause::EnvelopeBreach => "step_reversed_envelope_restored",
                YieldCause::OperatorDrain
                | YieldCause::CoordinatorDrain
                | YieldCause::LeaseExpired => "step_reversed_drain_lifted",
            };
            note(
                &mut self.statuses,
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
    }

    /// Every step above none becomes an action, a hint with its deadline; a device's GPU compute
    /// steps in a re-measure window are held but not sent.
    fn collect_actions(&mut self) {
        for ((id, key), state) in &self.next.ladder {
            if state.step == YieldStep::None {
                continue;
            }
            if let ResourceKey::GpuCompute(uuid) = key
                && self.remeasured.contains(uuid)
            {
                continue;
            }
            let duty = match key {
                ResourceKey::GpuCompute(uuid) if state.step == YieldStep::Cap => {
                    self.gpu_duty(id, uuid, state)
                }
                _ => None,
            };
            self.actions.push(YieldAction {
                allocation_id: id.clone(),
                resource: key.clone(),
                step: state.step,
                target: state.target,
                cause: state.cause,
                code: format!("yield_step_{}", step_name(state.step)),
                deadline_ms: self.hint_deadlines.get(&(id.clone(), key.clone())).copied(),
                duty,
            });
        }
    }

    /// On the time-slice duty each Neutral, Performance and Aggressive allocation on the device
    /// holds a cap at the duty, which `gpu_duty` sends, and never a pause; a re-measure window
    /// lifts it without a step change (FR-021, Q27, Q28).
    fn hold_slice_duty(&mut self, uuid: &str, budget: &GpuComputeBudget) {
        if !self.time_slice_duty.contains_key(uuid) {
            return;
        }
        let input = self.input;
        let key = ResourceKey::GpuCompute(uuid.to_string());
        let exempt: Vec<&Allocation> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running
                    && a.requested.gpu_memory_mib.contains_key(uuid)
                    && self.time_slice_exempt(&a.id, uuid)
            })
            .collect();
        let target = budget.share / exempt.len().max(1) as u64;
        for allocation in exempt {
            let id = (allocation.id.clone(), key.clone());
            let previous = self
                .prev
                .ladder
                .get(&id)
                .filter(|state| state.step == YieldStep::Cap);
            self.next.ladder.insert(
                id,
                StepState {
                    step: YieldStep::Cap,
                    entered_at_ms: previous.map_or(self.now, |state| state.entered_at_ms),
                    cause: YieldCause::ExternalGrowth,
                    target: Some(target),
                    trigger_external: previous.map_or(budget.external, |s| s.trigger_external),
                    calm_since_ms: None,
                    contended_since_ms: previous.and_then(|s| s.contended_since_ms),
                    after_pause: false,
                    to_free: None,
                    pending: false,
                    recomputed: false,
                },
            );
        }
    }

    /// Whether a Neutral, Performance or Aggressive allocation takes no GPU compute pause: its
    /// device was last read as time-sliced and its listing is now unknown (FR-021, Q29).
    fn no_gpu_pause(&self, id: &str, key: &ResourceKey) -> bool {
        let ResourceKey::GpuCompute(uuid) = key else {
            return false;
        };
        self.no_gpu_pause.contains(uuid)
            && self
                .allocation_modes
                .get(id)
                .and_then(|modes| modes.get(&ResourceKind::GpuCompute))
                .is_some_and(|mode| *mode != ContentionMode::YieldFirst)
    }

    /// Whether the ladder plans no GPU compute step for an allocation on a device: the time-slice
    /// rule holds there or the time-slice duty sets its cap, and the allocation runs in Neutral,
    /// Performance or Aggressive (FR-021, Q22, Q27).
    fn time_slice_exempt(&self, id: &str, uuid: &str) -> bool {
        (self.time_slice_holds.contains(uuid) || self.time_slice_duty.contains_key(uuid))
            && self
                .allocation_modes
                .get(id)
                .and_then(|modes| modes.get(&ResourceKind::GpuCompute))
                .is_some_and(|mode| *mode != ContentionMode::YieldFirst)
    }

    /// A GPU compute cap's duty in thousandths, which the supervisor applies: in Neutral,
    /// Performance and Aggressive, on a device measured this interval and for a cap not set for
    /// unknown activity, the share target over the allocation's part of own work's uncapped share,
    /// since own work at full duty beside another busy context holds only its slices (FR-021,
    /// Q20, R49). On the time-slice duty it is that duty (Q27), and on a device last read as
    /// time-sliced whose listing is unknown it is none (Q29). None leaves the duty at the share
    /// target, target ÷ 1000.
    fn gpu_duty(&self, id: &str, uuid: &str, state: &StepState) -> Option<u64> {
        let yield_first = self
            .allocation_modes
            .get(id)
            .and_then(|modes| modes.get(&ResourceKind::GpuCompute))
            .is_none_or(|mode| *mode == ContentionMode::YieldFirst);
        if let Some(duty) = self.time_slice_duty.get(uuid)
            && !yield_first
        {
            return Some(*duty);
        }
        let measured = self
            .gpu_compute_budget
            .get(uuid)
            .is_some_and(|budget| budget.own.is_some());
        if yield_first
            || !measured
            || state.cause == YieldCause::UnknownMeasurement
            || self.no_gpu_pause.contains(uuid)
        {
            return None;
        }
        let share = self
            .next
            .gpu_demand
            .get(uuid)?
            .own_share()
            .filter(|share| *share > 0)?;
        let running = self
            .input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running && a.requested.gpu_memory_mib.contains_key(uuid)
            })
            .count()
            .max(1) as u64;
        let target = state.target?;
        Some(
            (target
                .saturating_mul(GPU_COMPUTE_CAPACITY)
                .saturating_mul(running)
                / share)
                .min(GPU_COMPUTE_CAPACITY),
        )
    }

    /// The allocations a step would drain, each once, and charges beyond the
    /// budget.
    pub(super) fn drains(&mut self) {
        self.would_drain.extend(
            self.actions
                .iter()
                .filter(|action| action.step >= YieldStep::Drain)
                .map(|action| action.allocation_id.clone()),
        );
        let mut seen = BTreeSet::new();
        self.would_drain.retain(|id| seen.insert(id.clone()));
        if self
            .charged
            .cpu_millicores
            .saturating_sub(self.held_back_cpu)
            > self.budget.cpu_millicores
        {
            self.reasons.push(ReasonCode::new(
                "cpu_charges_exceed_budget",
                "managed CPU charges exceed scheduling budget",
            ));
        }
        if self.charged.ram_mib > self.budget.ram_mib {
            self.reasons.push(ReasonCode::new(
                "ram_charges_exceed_budget",
                "managed RAM charges exceed scheduling budget",
            ));
        }
    }
}

/// One memory resource's or the CPU's facts for the yield ladder.
struct Frame {
    key: ResourceKey,
    kind: ResourceKind,
    measurement_known: bool,
    cause: YieldCause,
    external: u64,
    critical: bool,
    timing: LadderTiming,
    admission_excess: u64,
    usage_excess: u64,
    guaranteed_drain: bool,
    /// A Guaranteed victim's drain is allowed for this cause and only waits for the breach
    /// window, so an over-share one keeps counting toward the excess meanwhile (FR-024).
    guaranteed_waits: bool,
    spare_share: u64,
    /// Own usage on the resource: CPU in use, or memory as judged.
    own_usage: u64,
    /// CPU contention has fully ended: held steps rise without the mode's interval.
    contention_ended: bool,
    /// The first interval it has: the level held CPU steps rise to in one move (R44).
    return_level: Option<u64>,
}

impl Frame {
    /// The resource's timing at a job's own mode's pace: each job yields and
    /// rises at its own mode's pace, and once CPU contention has fully ended
    /// it rises after the dwell alone (FR-034).
    fn timing_for(&self, config: &Config, mode: ContentionMode) -> LadderTiming {
        let paced = LadderTiming::new(
            &config.contention.thresholds(self.kind),
            mode,
            config.monitor.interval_ms,
        );
        LadderTiming {
            response_ms: paced.response_ms,
            scale_up_ms: if self.contention_ended {
                0
            } else {
                paced.scale_up_ms
            },
            ..self.timing.clone()
        }
    }
}

/// One device's GPU compute facts for the yield ladder.
struct ComputeFrame<'a> {
    key: ResourceKey,
    running: Vec<&'a Allocation>,
    /// Each running job's part of what own work holds on the device.
    share: u64,
    excess: u64,
    spare_share: u64,
    external: u64,
    measurement_known: bool,
    cause: YieldCause,
}

/// An unknown or held resource first holds; after this bounded unknown window
/// the ladder may escalate with cause unknown_measurement (FR-008, R10).
fn unknown_window(config: &Config, kind: ResourceKind) -> u64 {
    u64::from(config.contention.thresholds(kind).unknown_window_intervals)
        .saturating_mul(config.monitor.interval_ms)
}

/// An allocation's facts for the ladder on a resource. The supervisor reports
/// the steps, the hint among them, that it cannot apply. Only a worker that
/// acknowledged its target file takes a hint (FR-062); recordings without facts
/// have no target channel. A RAM cap needs a backend that reports it can apply
/// one.
fn ladder_facts(input: &PolicyInput, key: &ResourceKey, id: &str) -> AllocationFacts {
    let mut facts = input.facts.get(id).cloned().unwrap_or_default();
    if !facts.hint_ready {
        facts
            .unavailable
            .entry(key.clone())
            .or_default()
            .insert(YieldStep::Hint);
    }
    if *key == ResourceKey::Ram && !facts.ram_cap {
        facts
            .unavailable
            .entry(key.clone())
            .or_default()
            .insert(YieldStep::Cap);
    }
    facts
}

/// The supervisor reported the allocation's cap on the resource at the minimum duty.
fn at_minimum_duty(input: &PolicyInput, key: &ResourceKey, id: &str) -> bool {
    input.facts.get(id).is_some_and(|facts| {
        facts
            .step_codes
            .get(key)
            .is_some_and(|code| code == "cap_at_minimum_duty")
    })
}

/// What running memory work holds as yielding judges it: its usage. On GPU
/// memory, work that cannot take a hint keeps its charge, since its growth is
/// unbounded.
fn judged(
    input: &PolicyInput,
    charges: &BTreeMap<String, Resources>,
    key: &ResourceKey,
    allocation: &Allocation,
) -> u64 {
    match (key, &allocation.observed) {
        (ResourceKey::Ram, Some(observed)) => observed.ram_mib,
        (ResourceKey::GpuMemory(uuid), Some(observed))
            if input
                .facts
                .get(&allocation.id)
                .is_some_and(|f| f.hint_ready) =>
        {
            observed.gpu_memory_mib.get(uuid).copied().unwrap_or(0)
        }
        _ => amount_of(key, &charges[&allocation.id]),
    }
}

/// A Guaranteed allocation keeps its CPU request: only usage above it can be
/// capped (FR-024).
fn kept_cpu(kind: ResourceKind, allocation: &Allocation) -> u64 {
    if kind == ResourceKind::Cpu && is_guaranteed(allocation) {
        allocation.requested.cpu_millicores
    } else {
        0
    }
}

/// Record the largest amount own work holds above its target on a resource.
fn note_excess(over_target: &mut BTreeMap<ResourceKey, u64>, key: &ResourceKey, excess: u64) {
    let largest = over_target.entry(key.clone()).or_default();
    *largest = (*largest).max(excess);
}

/// The duty credited to own work on a device. Credit the larger of the target
/// the supervisor applied and the current one, so a lowered target counts only
/// once applied.
fn applied_duty(
    prev: &PolicyEngine,
    input: &PolicyInput,
    key: &ResourceKey,
    assumed: u64,
    allocation: &Allocation,
) -> u64 {
    let state = prev.ladder.get(&(allocation.id.clone(), key.clone()));
    let facts = input.facts.get(&allocation.id);
    let applied = facts.and_then(|facts| facts.applied.get(key));
    match (state, applied) {
        (Some(state), Some(step)) if *step == state.step => match state.step {
            YieldStep::Cap => {
                let current = state.target.unwrap_or(assumed);
                let ran = facts
                    .and_then(|facts| facts.applied_targets.get(key))
                    .copied()
                    .unwrap_or(current);
                assumed.min(ran.max(current))
            }
            YieldStep::Pause => 0,
            _ => assumed,
        },
        _ => assumed,
    }
}

/// Steps unavailable on GPU compute: no backend lowers a GPU compute share, and
/// a worker's resource targets carry no GPU compute amount; nor a pause where
/// `no_pause` says so.
fn compute_unavailable(
    input: &PolicyInput,
    key: &ResourceKey,
    id: &str,
    no_pause: bool,
) -> BTreeSet<YieldStep> {
    let mut steps = input
        .facts
        .get(id)
        .and_then(|f| f.unavailable.get(key))
        .cloned()
        .unwrap_or_default();
    steps.extend([YieldStep::LowerShare, YieldStep::Hint]);
    if no_pause {
        steps.insert(YieldStep::Pause);
    }
    steps
}

/// The level at which capping every victim's amount frees `needed`: the sum of each amount
/// above the level is at least `needed`, at the highest such level.
fn fill_level(victims: &[VictimFacts], needed: u64) -> u64 {
    let mut amounts: Vec<u64> = victims.iter().map(|v| v.amount).collect();
    amounts.sort_unstable();
    let total: u64 = amounts.iter().sum();
    let keep = total.saturating_sub(needed);
    // Water-fill from the smallest amounts up: those below the level keep all they use. Compared
    // before dividing, so with nothing needed the level is the largest amount and no one gives.
    let mut below = 0u64;
    for (count, amount) in amounts.iter().enumerate() {
        let open = (amounts.len() - count) as u64;
        let left = keep.saturating_sub(below);
        if amount.saturating_mul(open) >= left {
            return left / open;
        }
        below += amount;
    }
    0
}

/// Whether a held step is a CPU drain the pending pass planned for opportunistic work that now
/// runs: it no longer applies, and the allocation is planned as running work (FR-012).
fn replanned_pending_drain(
    state: Option<&StepState>,
    kind: ResourceKind,
    still_pending: bool,
    guaranteed: bool,
) -> bool {
    !still_pending
        && !guaranteed
        && kind == ResourceKind::Cpu
        && state.is_some_and(|state| state.pending && state.step >= YieldStep::Drain)
}

/// A step's target for the return level, when it was held for external growth: a cap's or
/// hint's, nothing for a pause. Steps held for other causes keep their own paths.
fn returning_target(state: &StepState) -> Option<u64> {
    if state.cause != YieldCause::ExternalGrowth {
        return None;
    }
    match state.step {
        YieldStep::Cap | YieldStep::Hint => Some(state.target.unwrap_or(0)),
        YieldStep::Pause => Some(0),
        _ => None,
    }
}

/// The level that lifts the lowest of `targets` so that together they hold `room`, none
/// lowered; none when they already hold it.
fn water_level(targets: &mut [u64], room: u64) -> Option<u64> {
    targets.sort_unstable();
    let mut above: u64 = targets.iter().sum();
    if targets.is_empty() || room <= above {
        return None;
    }
    for (i, target) in targets.iter().enumerate() {
        above -= target;
        let raised = i as u64 + 1;
        let level = room.saturating_sub(above) / raised;
        if targets.get(i + 1).is_none_or(|next| level <= *next) {
            return Some(level.max(*target));
        }
    }
    None
}

fn cpu_usage_of(allocation: &Allocation) -> u64 {
    allocation.observed.as_ref().map_or(0, |o| o.cpu_millicores)
}

#[cfg(test)]
mod tests {
    use super::water_level;

    /// The return level lifts the lowest steps until they hold the room, and never lowers one
    /// (FR-034, R44).
    #[test]
    fn the_return_level_fills_the_lowest_first_and_lowers_none() {
        let fill = |targets: &[u64], room| water_level(&mut targets.to_vec(), room);
        assert_eq!(fill(&[1, 5, 24, 135, 184], 1000), Some(200));
        // The highest stays where it is: 100 + 100 + 900 holds 1100.
        assert_eq!(fill(&[100, 900, 10], 1100), Some(100));
        assert_eq!(fill(&[300, 300], 600), None, "already holding the room");
        assert_eq!(fill(&[300, 300], 500), None, "never lowered");
        assert_eq!(fill(&[], 1000), None);
        for (targets, room) in [
            (vec![0, 7, 250, 3000], 4000u64),
            (vec![13; 9], 200),
            (vec![5], 6),
        ] {
            let level = fill(&targets, room).unwrap();
            let held: u64 = targets.iter().map(|t| (*t).max(level)).sum();
            assert!(
                held <= room && room - held < targets.len() as u64,
                "{targets:?} {room} {level}"
            );
        }
    }
}
