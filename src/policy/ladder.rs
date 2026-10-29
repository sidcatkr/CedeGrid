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
            self.plan_gpu_compute(uuid, budget);
        }
        self.gpu_compute_budget = budgets;
        self.note_reversals();
        self.collect_actions();
    }

    /// Since when each unknown or held resource, and each allocation of unknown
    /// usage, has been unknown.
    fn unknown_windows(&mut self) {
        let (prev, now) = (self.prev, self.now);
        for (key, unknown) in [
            (ResourceKey::Cpu, self.cpu_unknown || self.cpu_held),
            (ResourceKey::Ram, self.ram_unknown || self.ram_held),
        ] {
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
        let measurement_known =
            !held
                || self.next.unknown_since.get(&key).is_some_and(|start| {
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
        // memory above its fair share; in-share memory of work in those modes stays. The
        // envelope still applies in full.
        if cause == YieldCause::FloorBreach
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
            spare_share,
            contention_ended: kind == ResourceKind::Cpu && self.cpu_contention_ended,
        }
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
                    self.plan_victim(frame, victim, pending_pass, &mut 0);
                }
            }
            return;
        }
        // Running CPU yields tier by tier (FR-016). Within a tier, caps already held are lowered
        // at one level, as they rise equally (FR-013), and own usage over its target is shared
        // the same way (FR-011), so no single cap falls to the minimum duty and escalates; new
        // victims follow the cost order for what remains.
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
                let (level_group, ordered): (Vec<VictimFacts>, Vec<VictimFacts>) = rest[..len]
                    .iter()
                    .cloned()
                    .partition(|v| own_cause || held(v));
                tiers.push((level_group, ordered));
                rest = &rest[len..];
            }
            for (level_group, ordered) in tiers {
                let total: u64 = level_group.iter().map(|v| v.amount).sum();
                let level = fill_level(&level_group, remaining.min(total));
                for victim in level_group {
                    let mut share = victim.amount.saturating_sub(level);
                    remaining = remaining.saturating_sub(share);
                    planned.insert(victim.id.clone());
                    self.plan_victim(frame, victim, pending_pass, &mut share);
                }
                for victim in ordered {
                    planned.insert(victim.id.clone());
                    self.plan_victim(frame, victim, pending_pass, &mut remaining);
                }
            }
            return;
        }
        let mut remaining = needed;
        for victim in victims {
            planned.insert(victim.id.clone());
            self.plan_victim(frame, victim, pending_pass, &mut remaining);
        }
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
                // What the allocation may use: while contended, a cap
                // or hint target; while calm, its usage plus its share
                // of the spare, toward which a held cap rises; never
                // above its own mode's target.
                target: Some(
                    if contended {
                        victim.amount.saturating_sub(excess)
                    } else {
                        victim.amount.saturating_add(frame.spare_share)
                    }
                    .saturating_add(kept)
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
        if let Some(next) = &decision.state {
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
            self.next
                .ladder
                .insert((victim.id.clone(), key.clone()), next.clone());
        }
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
                a.phase == AllocationPhase::Running && a.requested.gpu_memory_mib.contains_key(uuid)
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
        // reports applied, since the duty cycle is own actuation. Unknown
        // activity and unreported duties keep the whole device (FR-008).
        let assumed = GPU_COMPUTE_CAPACITY / running.len() as u64;
        let duties: Vec<u64> = running
            .iter()
            .map(|a| applied_duty(prev, input, &key, unknown, assumed, a))
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
                    unavailable: compute_unavailable(input, key, &victim.id),
                    excess: if contended_victim { excess } else { 0 },
                    external: frame.external,
                    measurement_known: frame.measurement_known,
                    cause: frame.cause,
                    usage: budget.own.map(|_| frame.share),
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
        for ((id, step_key), state) in &prev.ladder {
            if step_key != &frame.key || planned.contains(id) {
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
                    unavailable: compute_unavailable(input, &frame.key, id),
                    excess: 0,
                    external: frame.external,
                    measurement_known: frame.measurement_known
                        && !self.unknown_allocations.contains(id),
                    cause: frame.cause,
                    target: None,
                    usage: None,
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
                YieldCause::FloorBreach => "step_reversed_floor_restored",
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

    /// Every step above none becomes an action, a hint with its deadline.
    fn collect_actions(&mut self) {
        for ((id, key), state) in &self.next.ladder {
            if state.step == YieldStep::None {
                continue;
            }
            self.actions.push(YieldAction {
                allocation_id: id.clone(),
                resource: key.clone(),
                step: state.step,
                target: state.target,
                cause: state.cause,
                code: format!("yield_step_{}", step_name(state.step)),
                deadline_ms: self.hint_deadlines.get(&(id.clone(), key.clone())).copied(),
            });
        }
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
    spare_share: u64,
    /// CPU contention has fully ended: held steps rise without the mode's interval.
    contention_ended: bool,
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
    unknown: bool,
    assumed: u64,
    allocation: &Allocation,
) -> u64 {
    let state = prev.ladder.get(&(allocation.id.clone(), key.clone()));
    let facts = input.facts.get(&allocation.id);
    let applied = facts.and_then(|facts| facts.applied.get(key));
    match (unknown, state, applied) {
        (false, Some(state), Some(step)) if *step == state.step => match state.step {
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
/// a worker's resource targets carry no GPU compute amount.
fn compute_unavailable(input: &PolicyInput, key: &ResourceKey, id: &str) -> BTreeSet<YieldStep> {
    let mut steps = input
        .facts
        .get(id)
        .and_then(|f| f.unavailable.get(key))
        .cloned()
        .unwrap_or_default();
    steps.extend([YieldStep::LowerShare, YieldStep::Hint]);
    steps
}

/// The level at which capping every victim's amount frees `needed`: the sum of each amount
/// above the level is at least `needed`, at the highest such level.
fn fill_level(victims: &[VictimFacts], needed: u64) -> u64 {
    let mut amounts: Vec<u64> = victims.iter().map(|v| v.amount).collect();
    amounts.sort_unstable_by(|a, b| b.cmp(a));
    let total: u64 = amounts.iter().sum();
    let keep = total.saturating_sub(needed);
    // Water-fill from the smallest amounts up: those below the level keep all they use.
    let mut below = 0u64;
    for (count, amount) in amounts.iter().rev().enumerate() {
        let open = (amounts.len() - count) as u64;
        let level = keep.saturating_sub(below) / open;
        if *amount >= level {
            return level;
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

fn cpu_usage_of(allocation: &Allocation) -> u64 {
    allocation.observed.as_ref().map_or(0, |o| o.cpu_millicores)
}
