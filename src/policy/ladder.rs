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
        // Jobs in Performance and Aggressive are never marked, whatever the governing mode, so
        // the kernel chooses among them as it would without CedeGrid (FR-023, FR-030).
        victims.retain(|v| v.mode < ContentionMode::Performance);
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
        self.note_below_fair_share();
        let budgets = std::mem::take(&mut self.gpu_compute_budget);
        for (uuid, budget) in &budgets {
            self.plan_gpu_compute(uuid, budget);
            self.plan_slice_holds(uuid, budget);
        }
        self.hold_unbudgeted_gpu_compute(&budgets);
        self.gpu_compute_budget = budgets;
        self.note_reversals();
        self.collect_actions();
        self.record_sent_caps();
    }

    /// Whether this decision holds own work back on each device, by a GPU compute pause or a CPU
    /// step of cap or more, for the next intervals' demand estimate (FR-021, Q19).
    fn record_sent_caps(&mut self) {
        let input = self.input;
        for (uuid, demand) in self.next.gpu_demand.iter_mut() {
            let compute = ResourceKey::GpuCompute(uuid.clone());
            let capped = self.actions.iter().any(|action| {
                matches!(action.step, YieldStep::Cap | YieldStep::Pause)
                    && (action.resource == ResourceKey::Cpu || action.resource == compute)
                    && input.allocations.iter().any(|a| {
                        a.id == action.allocation_id
                            && a.phase == AllocationPhase::Running
                            && a.requested.gpu_memory_mib.contains_key(uuid)
                    })
            });
            demand.sent_capped = [capped, demand.sent_capped[0]];
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
        let mut frame = self.resource_frame(key);
        if frame.kind == ResourceKind::Cpu {
            // Runs with a core lever give back by cores; what that frees comes off the excess the
            // other tasks' pauses cover (003 FR-043).
            let freed = self.plan_core_grants(&frame);
            frame.usage_excess = frame.usage_excess.saturating_sub(freed);
        }
        // A pause at its bound moves even while own usage over its target waits its dwell.
        let own_hold =
            frame.kind == ResourceKind::Cpu && self.cpu_own_hold && !self.pause_at_bound(&frame);
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

    /// Own work that gets less than its fair share of a saturated, contended CPU while no step
    /// holds it back says so and why (003 FR-049): the other users' threads get more of the CPU,
    /// and CedeGrid never raises its thread count or priority to win it back.
    fn note_below_fair_share(&mut self) {
        let fair = self.cpu_kept_running;
        let Some((own, _)) = self
            .statuses
            .get(&ResourceKey::Cpu)
            .and_then(|status| status.own)
        else {
            return;
        };
        let thresholds = self.config.contention.thresholds(ResourceKind::Cpu);
        let total = self.cpu_capacity.min(self.cpu_ceiling);
        let saturated = self.cpu_busy.is_some_and(|busy| {
            busy.saturating_mul(100)
                >= total.saturating_mul(u64::from(thresholds.saturation_percent))
        });
        let floor =
            (total.saturating_mul(u64::from(thresholds.principal_min_percent)) / 100).max(1);
        let held = self
            .next
            .ladder
            .iter()
            .any(|((_, key), state)| *key == ResourceKey::Cpu && state.step > YieldStep::None);
        if fair > 0 && saturated && !held && own.saturating_add(floor) < fair {
            note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "below_fair_share",
                format!(
                    "own work uses {own} m of its fair share of {fair} m: the other users' threads get more of the CPU, and CedeGrid does not raise its own thread count or priority to win it back"
                ),
            );
        }
    }

    /// Runs with a core lever give CPU back by the cores they may use, never by a pause
    /// (003 FR-043): each gets an equal part of the running CPU target left after the other
    /// running own work, in whole physical cores, at least one (FR-048); a job in Performance or
    /// Aggressive keeps every core, as plain Linux would run it. Fewer cores apply at once, more
    /// after the mode's scale-up interval, and an unknown measurement keeps the last grant. The
    /// cores chosen first avoid the CPUs a waiting user may use, where it may use fewer than the
    /// scope's (FR-056). Returns the CPU the grants take from the runs' current use.
    fn plan_core_grants(&mut self, frame: &Frame) -> u64 {
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let levers: Vec<(&Allocation, &crate::model::CoreLever)> = input
            .allocations
            .iter()
            .filter(|a| a.phase == AllocationPhase::Running)
            .filter_map(|a| Some((a, core_lever(input, &a.id)?)))
            .collect();
        if levers.is_empty() {
            return 0;
        }
        let others: u64 = input
            .allocations
            .iter()
            .filter(|a| a.phase == AllocationPhase::Running && core_lever(input, &a.id).is_none())
            .map(cpu_usage_of)
            .sum();
        let part = self.cpu_running_target().saturating_sub(others) / levers.len() as u64;
        let capacity = self.snapshot.cpu_capacity_millicores;
        let floor = (capacity.saturating_mul(u64::from(
            config
                .contention
                .thresholds(ResourceKind::Cpu)
                .principal_min_percent,
        )) / 100)
            .max(1);
        let avoid: BTreeSet<u32> = self
            .snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| !p.is_owner && p.cpu_wait_millicores.is_some_and(|wait| wait >= floor))
            .filter_map(|p| p.allowed_cpus.as_ref())
            .filter(|cpus| (cpus.len() as u64).saturating_mul(1000) < capacity)
            .flatten()
            .copied()
            .collect();
        let thresholds = config.contention.thresholds(ResourceKind::Cpu);
        let mut freed = 0u64;
        for (allocation, lever) in levers {
            let id = &allocation.id;
            let mode = self.allocation_modes[id][&ResourceKind::Cpu];
            let total = lever.cores.len();
            let wanted = if mode >= ContentionMode::Performance {
                total
            } else {
                cores_for(part, lever).clamp(1, total)
            };
            let before = prev.granted_cores.get(id).map(|cpus| {
                lever
                    .cores
                    .iter()
                    .filter(|core| core.iter().all(|c| cpus.contains(c)))
                    .count()
            });
            let changed_at = prev.core_changed_at.get(id).copied().unwrap_or(0);
            let count = match before {
                Some(before) if !frame.measurement_known => before,
                Some(before)
                    if wanted > before
                        && now.saturating_sub(changed_at)
                            < thresholds.scale_up_interval_ms(mode, config.monitor.interval_ms) =>
                {
                    before
                }
                _ => wanted,
            };
            self.next.core_changed_at.insert(
                id.clone(),
                if Some(count) == before {
                    changed_at
                } else {
                    now
                },
            );
            let touches = |core: &&Vec<u32>| core.iter().any(|cpu| avoid.contains(cpu));
            let mut cpus: Vec<u32> = lever
                .cores
                .iter()
                .filter(|core| !touches(core))
                .chain(lever.cores.iter().filter(|core| touches(core)))
                .take(count)
                .flatten()
                .copied()
                .collect();
            cpus.sort_unstable();
            freed = freed.saturating_add(
                cpu_usage_of(allocation).saturating_sub((cpus.len() as u64).saturating_mul(1000)),
            );
            note(
                &mut self.statuses,
                ResourceKey::Cpu,
                "cpu_cores_granted",
                format!("allocation {id} may use {count} of {total} cores"),
            );
            self.granted_cores.insert(id.clone(), cpus);
        }
        freed
    }

    /// The CPU target running own work is held to: the budget, never below the equal share a
    /// contending principal leaves own work, whatever room it took, within the envelope
    /// (SC-003, FR-057). Admission keeps the budget.
    fn cpu_running_target(&self) -> u64 {
        let kept = self
            .input
            .envelope
            .as_ref()
            .map_or(self.cpu_kept_running, |envelope| {
                self.cpu_kept_running.min(envelope.cpu_millicores)
            });
        self.budget.cpu_millicores.max(kept)
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
        // Performance and Aggressive hold every step while a measurement is unknown and never
        // escalate for it; admission stays closed (FR-008, FR-022).
        let escalates = governing(config, kind) < ContentionMode::Performance;
        let measurement_known = !held
            || escalates
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
        // A breach episode asks each in-share task once to save its progress, whoever caused
        // it; the episode's record of who was asked lasts while the floor stays breached
        // (FR-075). The memory pass lists them once it knows the over-share tasks.
        let save_progress =
            kind.is_memory() && floor_breach && self.memory_fair_share.contains_key(&key);
        if kind.is_memory()
            && floor_breach
            && let Some(asked) = self.prev.save_progress_decided.get(&key)
        {
            self.next
                .save_progress_decided
                .insert(key.clone(), asked.clone());
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
        // Performance and Aggressive release no CPU reservation that was admitted: charges over
        // the budget hold admission instead, as plain Linux would start what it was given
        // (006 FR-007a, FR-009).
        let admission_excess = if kind == ResourceKind::Cpu
            && governing(self.config, kind) >= ContentionMode::Performance
        {
            0
        } else {
            held_charge.saturating_sub(amount_of(&key, &self.budget))
        };
        let cpu_usage = input
            .allocations
            .iter()
            .filter(|a| a.phase != AllocationPhase::Pending)
            .map(cpu_usage_of)
            .fold(self.unknown_charge.cpu_millicores, u64::saturating_add);
        // Running work yields for what it holds, not for its charge: a
        // charge-only shortfall blocks admission and steps nothing.
        let mut usage_excess = if kind == ResourceKind::Cpu {
            cpu_usage.saturating_sub(self.cpu_running_target())
        } else {
            judged_total.saturating_sub(amount_of(&key, &self.budget))
        };
        // Floors by mode (FR-023, FR-030): a floor breach takes no memory of jobs in Performance
        // and Aggressive, whatever the governing mode, as without CedeGrid; the kernel decides at
        // exhaustion. The envelope still applies in full.
        let mut keeps_mode_memory = false;
        if floor_breach {
            let protected = input
                .allocations
                .iter()
                .filter(|a| {
                    a.phase != AllocationPhase::Pending
                        && a.phase != AllocationPhase::Draining
                        && self.allocation_modes[&a.id][&kind] >= ContentionMode::Performance
                })
                .map(|a| judged(input, &self.charges, &key, a))
                .fold(0u64, u64::saturating_add);
            let envelope_excess = input.envelope.as_ref().map_or(0, |envelope| {
                judged_total.saturating_sub(amount_of(&key, envelope))
            });
            keeps_mode_memory = envelope_excess == 0;
            usage_excess =
                usage_excess.min(judged_total.saturating_sub(protected).max(envelope_excess));
        }
        // What own work holds beyond the operator envelope, which steps jobs of every mode except
        // on CPU in Performance and Aggressive (006 FR-007a).
        let envelope_excess = input.envelope.as_ref().map_or(0, |envelope| {
            if kind == ResourceKind::Cpu {
                if governing(config, kind) >= ContentionMode::Performance {
                    0
                } else {
                    cpu_usage.saturating_sub(envelope.cpu_millicores)
                }
            } else {
                judged_total.saturating_sub(amount_of(&key, envelope))
            }
        });
        // Performance and Aggressive take no CPU step for others, for own usage over the
        // admission target or for unknown activity: the budget sets admission only, and only the
        // operator's envelope still steps running work (FR-022).
        if kind == ResourceKind::Cpu && governing(config, kind) >= ContentionMode::Performance {
            usage_excess = envelope_excess;
        }
        if usage_excess > 0 {
            note_excess(&mut self.over_target, &key, usage_excess);
        }
        // Own usage over its target while the budget did not fall is enforced first: no step is
        // entered until usage has stayed over the target for the dwell; then it steps with its
        // own cause (FR-011).
        if kind == ResourceKind::Cpu
            && cause == YieldCause::ExternalGrowth
            && usage_excess > 0
            && !self.cpu_budget_fell()
        {
            cause = YieldCause::OwnOverTarget;
            let since = self.prev.cpu_over_since.unwrap_or(now);
            self.next.cpu_over_since = Some(since);
            self.cpu_own_hold = now.saturating_sub(since) < timing.dwell_ms;
        }
        // Opportunistic work nobody can act on cannot be released first. Where all own
        // memory gives way, work that holds none of the memory is not work left (FR-024).
        let by_holding =
            kind != ResourceKind::Cpu && governing(config, kind) <= ContentionMode::Neutral;
        let opportunistic_left = input.allocations.iter().any(|a| {
            !is_guaranteed(a)
                && amount_of(&key, &self.charges[&a.id]) > 0
                && !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable)
                && (!by_holding || holds(input, &self.charges, &key, a) > 0)
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
            self.cpu_running_target().saturating_sub(cpu_usage) / held_back
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
            guaranteed_waits,
            spare_share,
            own_usage: if kind == ResourceKind::Cpu {
                cpu_usage
            } else {
                judged_total
            },
            contention_ended: kind == ResourceKind::Cpu && self.cpu_contention_ended,
            save_progress,
            keeps_mode_memory,
            envelope_excess,
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
        // Own work beyond the operator envelope, judged like a floor breach. Performance and
        // Aggressive leave own CPU over it alone, as plain Linux would run it (006 FR-007a).
        let cpu_exempt = key.kind() == ResourceKind::Cpu
            && governing(self.config, ResourceKind::Cpu) >= ContentionMode::Performance;
        let envelope_breach = !cpu_exempt
            && input.envelope.as_ref().is_some_and(|envelope| {
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
            // A run with a core lever is never paused for CPU (003 FR-043).
            .filter(|a| kind != ResourceKind::Cpu || core_lever(input, &a.id).is_none())
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
        // A job in Performance or Aggressive keeps its memory at a floor breach and yields no
        // CPU, running or reserved, not even for the operator envelope, whatever the governing
        // mode (FR-023, FR-030, 006 FR-007a).
        if frame.keeps_mode_memory || kind == ResourceKind::Cpu {
            victims.retain(|v| v.mode < ContentionMode::Performance);
        }
        victim_order(&mut victims, needed);
        if !pending_pass
            && kind == ResourceKind::Cpu
            && self.cpu_own_hold
            && !self.pause_at_bound(frame)
        {
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
        // Where no rate cap applies, running CPU work yields whole tasks: in victim order, each
        // only while the excess left is at least its usage, so the own work left running stays at
        // or above its target; a paused task frees nothing more and keeps its step (FR-089).
        if !pending_pass && kind == ResourceKind::Cpu {
            // The operator envelope is own work's hard limit, not a yield: what own work holds
            // beyond it pauses a task even where that goes below the target, whatever else the
            // resource waits on (FR-057).
            let mut hard = if frame.cause == YieldCause::EnvelopeBreach {
                needed
            } else {
                frame.envelope_excess.min(needed)
            };
            // A draining task is leaving: its usage makes no other task hold (FR-089).
            let draining: u64 = input
                .allocations
                .iter()
                .filter(|a| a.phase == AllocationPhase::Draining)
                .map(cpu_usage_of)
                .fold(0, u64::saturating_add);
            // A pause still answering its demand at its bound moves to another task: it resumes,
            // and other tasks cover what it used, as far as own work with it running would be over
            // its target, so the hold stays without growing and no task is drained for CPU
            // (FR-014, FR-089). The envelope's pauses drain.
            let bound = frame.timing.pause_bound_ms;
            let mut rotated = BTreeSet::new();
            let mut resumed_usage = 0u64;
            for victim in &victims {
                if let Some(state) = self.prev.ladder.get(&(victim.id.clone(), key.clone()))
                    && state.step == YieldStep::Pause
                    && state.cause != YieldCause::EnvelopeBreach
                    && now.saturating_sub(state.entered_at_ms) >= bound
                {
                    rotated.insert(victim.id.clone());
                    resumed_usage = resumed_usage.saturating_add(state.paused_usage.unwrap_or(0));
                }
            }
            let target = self.cpu_running_target();
            let over = |own: u64| own.saturating_sub(draining).saturating_sub(target);
            // While own usage over its target waits its dwell, only the moved pauses are covered.
            let fresh = if self.cpu_own_hold {
                0
            } else {
                needed.min(over(frame.own_usage))
            };
            let cover = resumed_usage.min(over(frame.own_usage.saturating_add(resumed_usage)));
            // A pause already held covers what its task still shows; the others that can be paused
            // now are chosen for the rest, a task whose step changed within the dwell waiting.
            let dwell = frame.timing.dwell_ms;
            let state_of = |id: &String| self.prev.ladder.get(&(id.clone(), key.clone()));
            let held: BTreeSet<String> = victims
                .iter()
                .filter(|v| {
                    !rotated.contains(&v.id)
                        && state_of(&v.id).is_some_and(|s| s.step == YieldStep::Pause)
                })
                .map(|v| v.id.clone())
                .collect();
            let held_usage: u64 = victims
                .iter()
                .filter(|v| held.contains(&v.id))
                .map(|v| v.amount)
                .sum();
            let candidates: Vec<(String, u64)> = victims
                .iter()
                .filter(|v| {
                    v.amount > 0
                        && !v.guaranteed
                        && !rotated.contains(&v.id)
                        && !held.contains(&v.id)
                        && state_of(&v.id)
                            .is_none_or(|s| now.saturating_sub(s.entered_at_ms) >= dwell)
                })
                .map(|v| (v.id.clone(), v.amount))
                .collect();
            let victims_left: Vec<u64> = victims
                .iter()
                .filter(|v| {
                    v.amount > 0
                        && !v.guaranteed
                        && !rotated.contains(&v.id)
                        && !held.contains(&v.id)
                })
                .map(|v| v.amount)
                .collect();
            let chosen = nearest(
                &candidates,
                fresh.saturating_sub(held_usage),
                &BTreeSet::new(),
            );
            let covering = nearest(&candidates, cover, &chosen);
            let covered: u64 = candidates
                .iter()
                .filter(|(id, _)| covering.contains(id))
                .map(|(_, amount)| *amount)
                .sum();
            let moved_cause = rotated
                .iter()
                .next()
                .and_then(|id| state_of(id).map(|state| (state.cause, state.trigger_external)));
            let mut moved = Vec::new();
            for victim in victims {
                if rotated.contains(&victim.id) {
                    moved.push((victim.id.clone(), victim.amount));
                    planned.insert(victim.id.clone());
                    let mut resumed = self.prev.ladder[&(victim.id.clone(), key.clone())].clone();
                    resumed.step = YieldStep::None;
                    resumed.entered_at_ms = now;
                    resumed.paused_usage = None;
                    self.next
                        .ladder
                        .insert((victim.id.clone(), key.clone()), resumed);
                    continue;
                }
                let fits = held.contains(&victim.id)
                    || chosen.contains(&victim.id)
                    || covering.contains(&victim.id)
                    || hard > 0;
                let asked = if victim.amount > 0 && fits {
                    victim.amount
                } else {
                    0
                };
                let mut share = asked;
                let id = (victim.id.clone(), key.clone());
                planned.insert(victim.id.clone());
                self.plan_victim(frame, victim, pending_pass, false, &mut share);
                let paused = self
                    .next
                    .ladder
                    .get(&id)
                    .is_some_and(|state| state.step >= YieldStep::Pause);
                hard = hard.saturating_sub(if paused { asked } else { asked - share });
                // A task that takes a moved pause's place holds for what that pause answered, its
                // cause and the demand that started it, so it ends as that pause would have.
                if covering.contains(&id.0)
                    && let Some((cause, trigger)) = moved_cause
                    && let Some(state) = self.next.ladder.get_mut(&id)
                    && state.step == YieldStep::Pause
                    && state.entered_at_ms == now
                {
                    state.cause = cause;
                    state.trigger_external = trigger;
                }
            }
            // Where too few running tasks are left to take their place, as when Yield First pauses
            // every task, the moved pauses take turns: the first resumes and the others hold
            // again from now as far as the cover needs them, so own work keeps one task running
            // instead of all at once (FR-014, FR-089).
            // Only where the running tasks could not cover it, whatever their dwell: a cover that
            // waits a dwell is taken up in a later interval.
            let running: u64 = victims_left
                .iter()
                .fold(0u64, |sum, amount| sum.saturating_add(*amount));
            let left = cover.saturating_sub(covered.max(running));
            let turns: Vec<(String, u64)> = moved
                .iter()
                .skip(1)
                .map(|(id, _)| {
                    let usage = self.prev.ladder[&(id.clone(), key.clone())]
                        .paused_usage
                        .unwrap_or(0);
                    (id.clone(), usage)
                })
                .collect();
            for id in nearest(&turns, left, &BTreeSet::new()) {
                let mut held = self.prev.ladder[&(id.clone(), key.clone())].clone();
                held.entered_at_ms = now;
                self.next.ladder.insert((id, key.clone()), held);
            }
            return;
        }
        // Over-share tasks give memory back first (FR-016, FR-023): they are asked together for
        // own memory above its fair share and no more, and what the floor or the mode's target
        // needs beyond it is asked in the same interval by the cost order.
        let over = if pending_pass {
            Vec::new()
        } else {
            self.over_share(key, &victims, frame.own_usage)
        };
        if !pending_pass && frame.save_progress {
            self.list_save_progress(key, &over);
        }
        if over.is_empty() {
            let mut remaining = needed;
            for victim in victims {
                planned.insert(victim.id.clone());
                self.plan_victim(frame, victim, pending_pass, false, &mut remaining);
            }
            return;
        }
        let excess = self
            .memory_fair_share
            .get(key)
            .map_or(0, |fair| frame.own_usage.saturating_sub(*fair));
        let mut first = needed.min(excess);
        let mut rest = needed - first;
        // The over-share part, in their order, as built: each task's ask and what it counts.
        let mut parts: BTreeMap<String, OverSharePart> = BTreeMap::new();
        for id in &over {
            let Some(victim) = victims.iter().find(|v| &v.id == id).cloned() else {
                continue;
            };
            let at_turn = first;
            let (counted, step) = self.dry_plan(frame, victim.clone(), first);
            let asked = victim.amount.min(at_turn);
            let settled = step.is_some_and(|step| step >= YieldStep::Drain);
            if settled {
                // A decided drain frees all the task holds, the rest of it for the remainder.
                rest = rest.saturating_sub(victim.amount.saturating_sub(asked));
            }
            first = first.saturating_sub(counted);
            parts.insert(
                id.clone(),
                OverSharePart {
                    at_turn,
                    asked,
                    counted,
                    settled,
                    passed: 0,
                },
            );
        }
        // What no over-share task counted passes on once its wait is over (FR-024's class
        // rule): to the tasks after it in the cost order, never back to the task asked for it.
        let passed = first;
        for part in parts.values_mut() {
            part.passed = part.asked.saturating_sub(part.counted).min(passed);
        }
        rest = rest.saturating_add(passed);
        let open = rest;
        // The remainder by the cost order as before over-share tasks led it: each over-share
        // task counted at what it holds less what it was asked, and a drained one at nothing.
        let mut left: Vec<VictimFacts> = victims
            .iter()
            .map(|victim| {
                let mut view = victim.clone();
                if let Some(part) = parts.get(&victim.id) {
                    view.amount = if part.settled {
                        0
                    } else {
                        view.amount.saturating_sub(part.asked)
                    };
                    view.frees = view.amount > 0;
                }
                view
            })
            .collect();
        victim_order(&mut left, rest);
        for view in left {
            let Some(victim) = victims.iter().find(|v| v.id == view.id).cloned() else {
                continue;
            };
            planned.insert(victim.id.clone());
            let Some(part) = parts.get(&victim.id).copied() else {
                self.plan_victim(frame, victim, false, false, &mut rest);
                continue;
            };
            // Beyond its part it is asked what the cost order asks of it, less its own part.
            let beyond = rest.min(open.saturating_sub(part.passed));
            if part.settled || beyond == 0 || view.amount == 0 {
                self.plan_victim(frame, victim, false, true, &mut part.at_turn.clone());
                continue;
            }
            // One step for both parts: its share of the excess and what it is asked beyond it.
            let before = self
                .prev
                .ladder
                .get(&(victim.id.clone(), key.clone()))
                .map_or(YieldStep::None, |s| s.step);
            let mut both = part.at_turn.saturating_add(beyond);
            self.plan_victim(frame, victim.clone(), false, true, &mut both);
            let freed = match self.next.ladder.get(&(victim.id.clone(), key.clone())) {
                Some(next) if next.step == YieldStep::Cap => victim
                    .amount
                    .saturating_sub(next.target.unwrap_or(victim.amount))
                    .saturating_sub(part.counted),
                Some(next) if next.step <= before => 0,
                Some(next)
                    if matches!(
                        next.step,
                        YieldStep::Pause | YieldStep::Drain | YieldStep::Terminate
                    ) =>
                {
                    view.amount
                }
                Some(_) => view.amount.min(beyond),
                None => 0,
            };
            rest = rest.saturating_sub(freed);
        }
    }

    /// The in-share own tasks on a breached memory resource not yet asked in the episode: they
    /// get the save-progress signal, each once (FR-075). Over-share tasks get their hint instead.
    fn list_save_progress(&mut self, key: &ResourceKey, over: &[String]) {
        let input = self.input;
        let population: Vec<String> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase != AllocationPhase::Pending
                    && a.phase != AllocationPhase::Draining
                    && !over.contains(&a.id)
                    && judged(input, &self.charges, key, a) > 0
            })
            .map(|a| a.id.clone())
            .collect();
        let asked = self
            .next
            .save_progress_decided
            .entry(key.clone())
            .or_default();
        for id in population {
            if asked.insert(id.clone()) {
                self.save_progress.insert(id);
            }
        }
    }

    /// One over-share task's step and what it counts against `remaining`, without recording
    /// anything: its effects are taken back.
    fn dry_plan(
        &mut self,
        frame: &Frame,
        victim: VictimFacts,
        remaining: u64,
    ) -> (u64, Option<YieldStep>) {
        let id = (victim.id.clone(), frame.key.clone());
        let saved = (
            self.statuses.clone(),
            self.over_target.clone(),
            self.hint_deadlines.clone(),
            self.next.ladder.get(&id).cloned(),
        );
        let mut left = remaining;
        self.plan_victim(frame, victim, false, true, &mut left);
        let step = self.next.ladder.get(&id).map(|next| next.step);
        (self.statuses, self.over_target, self.hint_deadlines) = (saved.0, saved.1, saved.2);
        match saved.3 {
            Some(state) => self.next.ladder.insert(id, state),
            None => self.next.ladder.remove(&id),
        };
        (remaining.saturating_sub(left), step)
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

    /// What a job's own mode allows it on a resource; Guaranteed keeps its CPU
    /// request, and memory in Performance and Aggressive its fair share (FR-023).
    fn ceiling(&self, key: &ResourceKey, allocation: &Allocation) -> Option<u64> {
        let kind = key.kind();
        let mode = self.allocation_modes[&allocation.id][&kind];
        // A job in Performance or Aggressive is not capped back to its target on compute, whatever
        // the governing mode (FR-022, FR-030).
        if !kind.is_memory() && mode >= ContentionMode::Performance {
            return None;
        }
        let fair = self
            .memory_fair_share
            .get(key)
            .filter(|_| mode >= ContentionMode::Performance)
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
        // CPU takes no rate cap, so running CPU work is held back by whole tasks (FR-089): a job
        // above its own mode's target is paused for it only when that target leaves it nothing
        // to run.
        let whole = kind == ResourceKind::Cpu && !pending_pass;
        let own_excess = if whole && own_excess < victim.amount {
            0
        } else {
            own_excess
        };
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
        // When contention has ended, a whole-task pause returns in one move, without the dwell
        // (R44, FR-089).
        victim_timing.returning = whole && frame.contention_ended && frame.usage_excess == 0;
        // A pause held for own usage over its target is not ended because one task's usage fell
        // while own work is not below its target by the contention floor (FR-013, FR-089).
        if whole
            && !contended
            && let Some(held) = state.filter(|s| self.holds_own_pause(s, frame))
        {
            self.next
                .ladder
                .insert((victim.id.clone(), key.clone()), held.clone());
            return;
        }
        // Whole tasks take no CPU hint and drain only for the envelope, at the pause bound
        // (FR-014, FR-057, FR-089).
        let escalates = whole
            && !victim.guaranteed
            && state.is_some_and(|s| {
                s.step == YieldStep::Pause && s.cause == YieldCause::EnvelopeBreach
            });
        if whole {
            unavailable.insert(YieldStep::Hint);
        }
        // A whole-task pause for the envelope holds to its bound: that the breach cleared with the
        // task paused does not show it would stay clear with the task running again.
        if whole
            && let Some(held) = state.filter(|s| {
                s.step == YieldStep::Pause
                    && s.cause == YieldCause::EnvelopeBreach
                    && now.saturating_sub(s.entered_at_ms) < victim_timing.pause_bound_ms
            })
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
                    pending_pass || kind != ResourceKind::Cpu || escalates
                },
                unavailable,
                excess: if contended { excess } else { 0 },
                external: frame.external,
                measurement_known: frame.measurement_known,
                cause: frame.cause,
                usage: (kind == ResourceKind::Cpu && !pending_pass)
                    .then(|| victim.amount.saturating_add(kept)),
                memory_usage: kind.is_memory().then_some(victim.amount),
                // What the allocation may use: while contended, a cap
                // or hint target; while calm, its usage plus its share
                // of the spare, toward which a held cap rises; never
                // above its own mode's target.
                target: Some(
                    if contended {
                        victim.amount.saturating_sub(excess).saturating_add(kept)
                    } else {
                        victim
                            .amount
                            .saturating_add(frame.spare_share)
                            .saturating_add(kept)
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
        if whole
            && let Some(next) = decision.state.as_mut()
            && next.step == YieldStep::Pause
            && state.is_none_or(|s| s.step != YieldStep::Pause)
        {
            next.paused_usage = Some(victim.amount.saturating_add(kept));
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
                    !hint_wait_over(next, &victim_timing, now)
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

    /// Whether a whole-task CPU pause on the frame's resource has reached its bound, so it moves
    /// to another task whatever else holds (FR-014, FR-089).
    fn pause_at_bound(&self, frame: &Frame) -> bool {
        frame.kind == ResourceKind::Cpu
            && self.prev.ladder.iter().any(|((_, key), state)| {
                *key == frame.key
                    && state.step == YieldStep::Pause
                    && state.cause != YieldCause::EnvelopeBreach
                    && self.now.saturating_sub(state.entered_at_ms) >= frame.timing.pause_bound_ms
            })
    }

    /// Hysteresis for `own_over_target` pauses (FR-013, FR-089): while own work is not below its
    /// target by the contention floor or half the paused task, a held pause holds, so own work
    /// stays at its target and does not pause and resume a task by turns. At its bound the pause
    /// moves as any does.
    fn holds_own_pause(&self, state: &StepState, frame: &Frame) -> bool {
        let floor = (self.cpu_capacity.saturating_mul(u64::from(
            self.config
                .contention
                .thresholds(ResourceKind::Cpu)
                .principal_min_percent,
        )) / 100)
            .max(1);
        // Resuming it would leave own work further over its target than the pause leaves it
        // below, as FR-089's nearest choice would pause it again.
        let half = state.paused_usage.unwrap_or(0) / 2;
        state.step == YieldStep::Pause
            && state.cause == YieldCause::OwnOverTarget
            && frame.own_usage.saturating_add(floor.max(half)) > self.cpu_running_target()
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
                    memory_usage: None,
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
        // Performance and Aggressive take no GPU compute step for others; jobs in Yield First
        // and Neutral are still held to their own mode's share (FR-016, FR-022).
        let excess = if governing(config, ResourceKind::GpuCompute) >= ContentionMode::Performance {
            0
        } else {
            held.saturating_sub(budget.share)
        };
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
        // Jobs in Performance and Aggressive take no GPU compute step for others, whatever the
        // governing mode (FR-022, FR-030).
        victims.retain(|v| v.mode < ContentionMode::Performance);
        victim_order(&mut victims, frame.excess);
        // Whole tasks (FR-089): pausing one more running task frees own work's part on the device
        // divided among the tasks still running.
        let paused = |id: &str| {
            prev.ladder
                .get(&(id.to_string(), key.clone()))
                .is_some_and(|s| s.step >= YieldStep::Pause)
        };
        let unpaused = frame
            .running
            .iter()
            .filter(|a| !paused(&a.id))
            .count()
            .max(1) as u64;
        let part = frame.share.saturating_mul(frame.running.len() as u64) / unpaused;
        // On a time-sliced device whose counts are known, what own work keeps after one more
        // pause is its running tasks' part of the busy contexts, each task one context; elsewhere
        // its measured amount less one task's part (FR-021, FR-089).
        let sliced_external = self
            .statuses
            .get(key)
            .and_then(|status| status.time_slice.as_ref())
            .and_then(|slice| slice.external)
            .map(u64::from);
        let mut running = unpaused;
        let mut own_now = frame.share.saturating_mul(frame.running.len() as u64);
        let target = own_now.saturating_sub(frame.excess);
        let mut remaining = frame.excess;
        // A pause at its bound moves to another task: it resumes, and its part is to be held back
        // by the next task in the order, so no task is drained for GPU compute (FR-014, FR-089).
        let bound = config
            .contention
            .thresholds(kind)
            .pause_bound_s
            .saturating_mul(1000);
        let mut rotated = BTreeSet::new();
        for victim in &victims {
            if let Some(state) = prev.ladder.get(&(victim.id.clone(), key.clone()))
                && state.step == YieldStep::Pause
                && now.saturating_sub(state.entered_at_ms) >= bound
            {
                rotated.insert(victim.id.clone());
                let usage = state.paused_usage.unwrap_or(part);
                running = running.saturating_add(1);
                own_now = own_now.saturating_add(usage);
                remaining = remaining.saturating_add(usage);
            }
        }
        let mut planned = BTreeSet::new();
        for victim in victims {
            planned.insert(victim.id.clone());
            let state = prev.ladder.get(&(victim.id.clone(), key.clone()));
            if rotated.contains(&victim.id)
                && let Some(state) = state
            {
                let mut resumed = state.clone();
                resumed.step = YieldStep::None;
                resumed.entered_at_ms = now;
                resumed.paused_usage = None;
                self.next
                    .ladder
                    .insert((victim.id.clone(), key.clone()), resumed);
                continue;
            }
            let held = paused(&victim.id);
            // A job above its own mode's share yields the difference even
            // while the aggregate has room.
            let limit = (victim.mode < ContentionMode::Performance
                || governing(self.config, ResourceKind::GpuCompute) < ContentionMode::Performance)
                .then(|| budget.targets.get(&victim.mode).copied())
                .flatten();
            let own_excess = limit.map_or(0, |limit| frame.share.saturating_sub(limit));
            if own_excess > 0 {
                note_excess(&mut self.over_target, key, own_excess);
            }
            // A running task is paused only while the others keep at least the target, and for
            // its own mode's target only when that leaves it nothing; a paused one frees nothing
            // more and keeps its step to its bound.
            let own_excess = if own_excess >= frame.share {
                own_excess
            } else {
                0
            };
            let left = match sliced_external {
                Some(external) if running > 0 => {
                    GPU_COMPUTE_CAPACITY * (running - 1) / (running - 1 + external).max(1)
                }
                _ => own_now.saturating_sub(part),
            };
            let excess = if held {
                0
            } else if remaining > 0 && left >= target {
                remaining.max(own_excess)
            } else {
                own_excess
            };
            let contended_victim = excess > 0;
            let decision = plan_step(
                state,
                &LadderInput {
                    memory_usage: None,
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
            if let Some(mut next) = decision.state {
                if next.step > before {
                    let freed = match next.step {
                        YieldStep::Cap => frame
                            .share
                            .saturating_sub(next.target.unwrap_or(frame.share)),
                        YieldStep::Pause => part,
                        _ => 0,
                    };
                    remaining = remaining.saturating_sub(freed);
                    if next.step == YieldStep::Pause {
                        running = running.saturating_sub(1);
                        own_now = left;
                        next.paused_usage = Some(part);
                    }
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
                    memory_usage: None,
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

    /// Every step above none becomes an action, a hint with its deadline.
    fn collect_actions(&mut self) {
        for ((id, key), state) in &self.next.ladder {
            if state.step == YieldStep::None {
                continue;
            }
            // No duty cycle runs: GPU compute takes whole-task pauses only (FR-089).
            let duty = None;
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

    /// The time-slice hold (FR-021, FR-089): on a device where the rule fails beside a slice-bound
    /// principal, own tasks in Yield First and Neutral are paused in victim order while the own
    /// tasks left running, each counted as one context, stay at or above the hold count. A held
    /// pause stays while the hold lasts and drains at its bound; Guaranteed work is never paused.
    fn plan_slice_holds(&mut self, uuid: &str, budget: &GpuComputeBudget) {
        let Some(hold) = self.slice_hold.get(uuid).copied() else {
            return;
        };
        let (config, input, prev, now) = (self.config, self.input, self.prev, self.now);
        let kind = ResourceKind::GpuCompute;
        let key = ResourceKey::GpuCompute(uuid.to_string());
        let on_device: Vec<&Allocation> = input
            .allocations
            .iter()
            .filter(|a| {
                a.phase == AllocationPhase::Running && a.requested.gpu_memory_mib.contains_key(uuid)
            })
            .collect();
        let paused = |id: &str| {
            prev.ladder
                .get(&(id.to_string(), key.clone()))
                .is_some_and(|state| state.step >= YieldStep::Pause)
        };
        let mut running = on_device.iter().filter(|a| !paused(&a.id)).count() as u32;
        let mut victims: Vec<VictimFacts> = on_device
            .iter()
            .filter(|a| {
                self.time_slice_exempt(&a.id, uuid)
                    && self.allocation_modes[&a.id][&kind] < ContentionMode::Performance
            })
            .map(|a| VictimFacts {
                id: a.id.clone(),
                controllable: !input.facts.get(&a.id).is_some_and(|f| f.uncontrollable),
                frees: true,
                pending: false,
                mode: self.allocation_modes[&a.id][&kind],
                guaranteed: is_guaranteed(a),
                progress_lost_ms: input.facts.get(&a.id).map_or(0, |f| f.progress_lost_ms),
                amount: 1,
            })
            .collect();
        victim_order(&mut victims, u64::from(running.saturating_sub(hold)));
        let pause_bound_ms = config
            .contention
            .thresholds(kind)
            .pause_bound_s
            .saturating_mul(1000);
        // A held pause stays while the hold lasts; at its bound it moves to the next task in the
        // order, and the paused task resumes (FR-014, FR-089).
        let at_bound = |id: &str| {
            prev.ladder
                .get(&(id.to_string(), key.clone()))
                .is_some_and(|s| {
                    s.step == YieldStep::Pause
                        && now.saturating_sub(s.entered_at_ms) >= pause_bound_ms
                })
        };
        running = running.saturating_add(victims.iter().filter(|v| at_bound(&v.id)).count() as u32);
        for victim in victims {
            let id = (victim.id.clone(), key.clone());
            if let Some(state) = prev.ladder.get(&id).filter(|s| s.step >= YieldStep::Pause) {
                let mut next = state.clone();
                if at_bound(&victim.id) {
                    next.step = YieldStep::None;
                    next.entered_at_ms = now;
                }
                self.next.ladder.insert(id, next);
                continue;
            }
            let pausable = !victim.guaranteed
                && victim.controllable
                && !self.no_gpu_pause(&victim.id, &key)
                && !compute_unavailable(input, &key, &victim.id, false).contains(&YieldStep::Pause);
            if running > hold && pausable {
                running -= 1;
                self.next.ladder.insert(
                    id,
                    StepState {
                        step: YieldStep::Pause,
                        entered_at_ms: now,
                        cause: YieldCause::ExternalGrowth,
                        target: None,
                        trigger_external: budget.external,
                        calm_since_ms: None,
                        contended_since_ms: Some(now),
                        after_pause: false,
                        to_free: None,
                        pending: false,
                        asked_target: None,
                        paused_usage: None,
                    },
                );
            }
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
        (self.time_slice_holds.contains(uuid) || self.slice_hold.contains_key(uuid))
            && self
                .allocation_modes
                .get(id)
                .and_then(|modes| modes.get(&ResourceKind::GpuCompute))
                .is_some_and(|mode| *mode != ContentionMode::YieldFirst)
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
    /// A memory floor is breached with a known fair share: in-share tasks not yet asked in the
    /// episode get the save-progress signal (FR-075).
    save_progress: bool,
    /// A floor breach where work in Performance or Aggressive keeps its memory (FR-023): it is
    /// no victim, gets the save-progress signal and no OOM mark.
    keeps_mode_memory: bool,
    /// Own work beyond the operator envelope, the only excess jobs in Performance and
    /// Aggressive pay (FR-030, FR-057).
    envelope_excess: u64,
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
    // CPU work has no rate cap at either level: it is held back by whole tasks (FR-089).
    if *key == ResourceKey::Cpu {
        facts
            .unavailable
            .entry(key.clone())
            .or_default()
            .insert(YieldStep::Cap);
    }
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

/// What an allocation holds of a memory resource, as `judged` reads it, except that a device
/// entry missing from a hint-ready allocation's reading counts by its charge: a missing
/// reading is not "holds none" (FR-024).
fn holds(
    input: &PolicyInput,
    charges: &BTreeMap<String, Resources>,
    key: &ResourceKey,
    allocation: &Allocation,
) -> u64 {
    match (key, &allocation.observed) {
        (ResourceKey::GpuMemory(uuid), Some(observed))
            if !observed.gpu_memory_mib.contains_key(uuid) =>
        {
            amount_of(key, &charges[&allocation.id])
        }
        _ => judged(input, charges, key, allocation),
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

/// What an allocation is credited with on a device: nothing while its pause is applied, else its
/// assumed part (FR-021, FR-089).
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
        (Some(state), Some(step)) if *step == state.step && state.step == YieldStep::Pause => 0,
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
    // No rate cap: GPU compute is held back by whole tasks (FR-089).
    steps.extend([YieldStep::LowerShare, YieldStep::Cap, YieldStep::Hint]);
    if no_pause {
        steps.insert(YieldStep::Pause);
    }
    steps
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

/// The tasks to pause for `need`, from `candidates` in victim order: each that fits what is left,
/// so own work stays at or above its target; then, if a part is left, the smallest other task
/// whose pause leaves own work less below its target than running leaves it above. Neither own
/// work nor the others then lose more than half a task, and with equal tasks the choice does not
/// change while `need` moves by less than half of one (FR-089).
fn nearest(candidates: &[(String, u64)], need: u64, taken: &BTreeSet<String>) -> BTreeSet<String> {
    let mut left = need;
    let mut chosen = BTreeSet::new();
    for (id, amount) in candidates {
        if left > 0 && *amount > 0 && *amount <= left && !taken.contains(id) {
            chosen.insert(id.clone());
            left -= amount;
        }
    }
    if left > 0
        && let Some((id, _)) = candidates
            .iter()
            .filter(|(id, amount)| {
                *amount > 0
                    && *amount < left.saturating_mul(2)
                    && !taken.contains(id)
                    && !chosen.contains(id)
            })
            .min_by_key(|(_, amount)| *amount)
    {
        chosen.insert(id.clone());
    }
    chosen
}

/// A run's core lever, where the agent reports one with cores (003 FR-043).
fn core_lever<'a>(input: &'a PolicyInput, id: &str) -> Option<&'a crate::model::CoreLever> {
    input
        .facts
        .get(id)?
        .cpu_cores
        .as_ref()
        .filter(|lever| !lever.cores.is_empty())
}

/// The fewest of a lever's cores, in its order, whose logical CPUs reach `part` millicores.
fn cores_for(part: u64, lever: &crate::model::CoreLever) -> usize {
    let mut reach = 0u64;
    for (count, core) in lever.cores.iter().enumerate() {
        if reach >= part {
            return count;
        }
        reach = reach.saturating_add((core.len() as u64).saturating_mul(1000));
    }
    lever.cores.len()
}

fn cpu_usage_of(allocation: &Allocation) -> u64 {
    allocation.observed.as_ref().map_or(0, |o| o.cpu_millicores)
}

/// An over-share task's part of the excess in one memory pass (FR-016, FR-023): the excess left
/// at its turn, what it was asked, what it counts as built, and whether its drain is decided.
#[derive(Debug, Clone, Copy)]
struct OverSharePart {
    at_turn: u64,
    asked: u64,
    counted: u64,
    settled: bool,
    /// What it was asked and did not count, which passes on once its wait is over (FR-024's
    /// class rule) and is never asked of it again.
    passed: u64,
}

#[cfg(test)]
mod tests {
    use super::nearest;
    use std::collections::BTreeSet;

    fn pick(amounts: &[u64], need: u64) -> Vec<String> {
        let candidates: Vec<(String, u64)> = amounts
            .iter()
            .enumerate()
            .map(|(i, amount)| (format!("t{i}"), *amount))
            .collect();
        nearest(&candidates, need, &BTreeSet::new())
            .into_iter()
            .collect()
    }

    /// The paused total lands nearest the excess, each side within half the smallest task, and
    /// equal tasks keep their choice while the excess moves by less than half of one (FR-089).
    #[test]
    fn whole_tasks_are_chosen_nearest_the_excess() {
        // 230 m over with two 1000 m tasks: pausing one leaves 770 m under, so none is paused.
        assert!(pick(&[1000, 1000], 230).is_empty());
        // 600 m over: pausing one leaves 400 m under, nearer than 600 m over.
        assert_eq!(pick(&[1000, 1000], 600), ["t0"]);
        // Fits in victim order first, then the smallest that overshoots less than it leaves.
        assert_eq!(pick(&[1000, 300, 250], 1225), ["t0", "t2"]);
        // Equal tasks: 200 m over pauses one 250 m task; 100 m over pauses none.
        assert_eq!(pick(&[250; 4], 200), ["t0"]);
        assert!(pick(&[250; 4], 100).is_empty());
        assert_eq!(pick(&[250; 4], 230), pick(&[250; 4], 200));
        assert!(pick(&[250; 4], 0).is_empty());
    }
}
