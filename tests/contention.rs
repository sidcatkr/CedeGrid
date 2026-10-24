//! Contention-mode and yield-ladder properties. Builders mirror tests/policy.rs.
use cedegrid::{config::Config, execution_model::AllocationClass, model::*, policy::PolicyEngine};
use proptest::prelude::*;
use std::collections::BTreeMap;

pub fn config() -> Config {
    let mut result = Config::default();
    result.cpu.reserve_physical_cores = 1;
    result.ram.reserve_mib = 1000;
    result.ram.reserve_percent = 0;
    result.gpu.reserve_vram_mib = 1000;
    result.gpu.scale_up_cooldown_ms = 0;
    result
}

pub fn snapshot(cpu_busy_millicores: u64) -> Snapshot {
    Snapshot {
        schema_version: SCHEMA_VERSION,
        node_id: "local".into(),
        observed_at_unix_ms: 100,
        cpu_capacity_millicores: 8000,
        physical_cores: Some(4),
        cpu_busy_millicores: Some(cpu_busy_millicores),
        cpu_busy_lower_millicores: None,
        total_ram_mib: 32_000,
        available_ram_mib: Some(26_000),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: None,
        principals: None,
    }
}

pub fn allocation(
    id: &str,
    class: AllocationClass,
    cpu: u64,
    observed_cpu: Option<u64>,
) -> Allocation {
    let resources = |cpu| Resources {
        cpu_millicores: cpu,
        ..Resources::default()
    };
    Allocation {
        id: id.into(),
        class,
        phase: AllocationPhase::Running,
        requested: resources(cpu),
        observed: observed_cpu.map(resources),
        observed_upper: None,
    }
}

proptest! {
    #[test]
    fn builders_produce_snapshots_the_policy_accepts(busy in 0u64..=8000) {
        let input = PolicyInput {
            allocations: vec![allocation("a", AllocationClass::Opportunistic, 500, Some(500))],
            ..PolicyInput::default()
        };
        prop_assert!(PolicyEngine::new().evaluate(&config(), &snapshot(busy.max(500)), &input, 0).is_ok());
    }
}

mod ladder {
    use cedegrid::contention::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    const INTERVAL: u64 = 500;

    fn timing(kind: ResourceKind, mode: ContentionMode) -> LadderTiming {
        LadderTiming::new(&ResourceThresholds::defaults(kind), mode, INTERVAL)
    }

    fn input(kind: ResourceKind, excess: u64, external: u64) -> LadderInput {
        LadderInput {
            kind,
            guaranteed: false,
            allow_drain: true,
            unavailable: BTreeSet::new(),
            excess,
            external,
            measurement_known: true,
            cause: YieldCause::ExternalGrowth,
            target: None,
        }
    }

    /// Run the planner once per interval from `start` while the closure supplies input.
    fn run(
        mut state: Option<StepState>,
        timing: &LadderTiming,
        intervals: std::ops::Range<u64>,
        mut at: impl FnMut(u64) -> LadderInput,
    ) -> (Option<StepState>, Vec<(u64, YieldStep)>) {
        let mut changes = vec![];
        for i in intervals {
            let now = i * INTERVAL;
            let next = plan_step(state.as_ref(), &at(now), timing, now).state;
            let before = state.as_ref().map_or(YieldStep::None, |s| s.step);
            let after = next.as_ref().map_or(YieldStep::None, |s| s.step);
            if before != after {
                changes.push((now, after));
            }
            state = next;
        }
        (state, changes)
    }

    #[test]
    fn steps_escalate_in_order_and_memory_starts_at_hint() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::YieldFirst);
        let (_, changes) = run(None, &cpu, 0..60, |_| input(ResourceKind::Cpu, 1000, 4000));
        let steps: Vec<_> = changes.iter().map(|(_, step)| *step).collect();
        assert_eq!(
            steps,
            [
                YieldStep::LowerShare,
                YieldStep::Cap,
                YieldStep::Pause,
                YieldStep::Hint,
                YieldStep::Drain
            ]
        );
        // Each change waits for the response time and the dwell time (3 intervals).
        assert!(changes.windows(2).all(|w| w[1].0 - w[0].0 >= 3 * INTERVAL));
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            let first = plan_step(
                None,
                &input(kind, 1, 1),
                &timing(kind, ContentionMode::Neutral),
                0,
            );
            assert_eq!(first.state.unwrap().step, YieldStep::Hint, "{kind:?}");
        }
    }

    #[test]
    fn unavailable_steps_are_skipped_and_reported() {
        let mut rootless = input(ResourceKind::Cpu, 1000, 4000);
        rootless.unavailable = [YieldStep::LowerShare, YieldStep::Hint].into();
        let first = plan_step(
            None,
            &rootless,
            &timing(ResourceKind::Cpu, ContentionMode::Neutral),
            0,
        );
        assert_eq!(first.state.as_ref().unwrap().step, YieldStep::Cap);
        assert_eq!(first.skipped, [YieldStep::LowerShare]);
        let mut memory = input(ResourceKind::Ram, 1, 1);
        memory.unavailable = [YieldStep::Hint].into();
        let first = plan_step(
            None,
            &memory,
            &timing(ResourceKind::Ram, ContentionMode::Neutral),
            0,
        );
        assert_eq!(first.state.unwrap().step, YieldStep::Drain);
        assert_eq!(first.skipped, [YieldStep::Hint]);
    }

    #[test]
    fn guaranteed_work_is_never_paused_and_never_drained_automatically() {
        let mut guaranteed = input(ResourceKind::Cpu, 1000, 4000);
        guaranteed.guaranteed = true;
        guaranteed.allow_drain = false;
        let (state, changes) = run(
            None,
            &timing(ResourceKind::Cpu, ContentionMode::YieldFirst),
            0..100,
            |_| guaranteed.clone(),
        );
        let steps: Vec<_> = changes.iter().map(|(_, step)| *step).collect();
        assert_eq!(
            steps,
            [YieldStep::LowerShare, YieldStep::Cap, YieldStep::Hint]
        );
        assert_eq!(state.unwrap().step, YieldStep::Hint);
    }

    #[test]
    fn an_unknown_measurement_holds_the_current_step() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::YieldFirst);
        let (state, _) = run(None, &cpu, 0..4, |_| input(ResourceKind::Cpu, 1000, 4000));
        let held = state.clone().unwrap();
        let (after, changes) = run(state, &cpu, 4..40, |_| LadderInput {
            measurement_known: false,
            ..input(ResourceKind::Cpu, 1000, 0)
        });
        assert!(changes.is_empty());
        assert_eq!(after.unwrap(), held);
    }

    #[test]
    fn a_pause_escalates_after_its_bound_while_contention_persists() {
        let mut cpu = timing(ResourceKind::Cpu, ContentionMode::Neutral);
        cpu.response_ms = u64::MAX; // only the pause bound can move it on
        let paused = StepState {
            step: YieldStep::Pause,
            entered_at_ms: 0,
            cause: YieldCause::ExternalGrowth,
            target: None,
            trigger_external: 4000,
            calm_since_ms: None,
            contended_since_ms: Some(0),
        };
        let before = plan_step(
            Some(&paused),
            &input(ResourceKind::Cpu, 500, 4000),
            &cpu,
            cpu.pause_bound_ms - 1,
        );
        assert_eq!(before.state.unwrap().step, YieldStep::Pause);
        let after = plan_step(
            Some(&paused),
            &input(ResourceKind::Cpu, 500, 4000),
            &cpu,
            cpu.pause_bound_ms,
        );
        assert_eq!(after.state.unwrap().step, YieldStep::Hint);
    }

    #[test]
    fn steps_reverse_one_level_at_a_time_below_the_hysteresis_threshold() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::Performance);
        let capped = StepState {
            step: YieldStep::Cap,
            entered_at_ms: 0,
            cause: YieldCause::ExternalGrowth,
            target: Some(2000),
            trigger_external: 4000,
            calm_since_ms: None,
            contended_since_ms: None,
        };
        // 60% of the trigger is inside the hysteresis band: hold.
        let (held, changes) = run(Some(capped.clone()), &cpu, 1..40, |_| {
            input(ResourceKind::Cpu, 0, 2400)
        });
        assert!(changes.is_empty());
        assert_eq!(held.unwrap().step, YieldStep::Cap);
        // Below 50%: one level per scale-up interval (2 s in Performance), after dwell.
        let (gone, changes) = run(Some(capped), &cpu, 1..40, |_| {
            input(ResourceKind::Cpu, 0, 1000)
        });
        let steps: Vec<_> = changes.iter().map(|(_, step)| *step).collect();
        assert_eq!(steps, [YieldStep::LowerShare, YieldStep::None]);
        assert!(changes[1].0 - changes[0].0 >= cpu.scale_up_ms);
        assert!(gone.is_none());
    }

    #[test]
    fn drain_and_terminate_never_reverse() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::Performance);
        for step in [YieldStep::Drain, YieldStep::Terminate] {
            let state = StepState {
                step,
                entered_at_ms: 0,
                cause: YieldCause::ExternalGrowth,
                target: None,
                trigger_external: 4000,
                calm_since_ms: None,
                contended_since_ms: None,
            };
            let (after, changes) = run(Some(state), &cpu, 1..100, |_| {
                input(ResourceKind::Cpu, 0, 0)
            });
            assert!(changes.is_empty());
            assert_eq!(after.unwrap().step, step);
        }
    }

    proptest! {
        #[test]
        fn min_dwell_separates_every_step_change(
            pattern in proptest::collection::vec((0u64..3000, 0u64..8000), 1..80),
        ) {
            let cpu = timing(ResourceKind::Cpu, ContentionMode::Performance);
            let (_, changes) = run(None, &cpu, 0..pattern.len() as u64, |now| {
                let (excess, external) = pattern[(now / INTERVAL) as usize];
                input(ResourceKind::Cpu, excess, external)
            });
            for pair in changes.windows(2) {
                prop_assert!(pair[1].0 - pair[0].0 >= cpu.dwell_ms, "{changes:?}");
            }
        }

        #[test]
        fn an_on_off_load_changes_the_step_at_most_once_per_switch(
            mode in prop_oneof![
                Just(ContentionMode::YieldFirst),
                Just(ContentionMode::Neutral),
                Just(ContentionMode::Performance)
            ],
            switches in 2u64..12,
        ) {
            // External load toggles every 4 monitor intervals (SC-009). A step that
            // is applied frees the excess, as a working cap does.
            let cpu = timing(ResourceKind::Cpu, mode);
            let mut state: Option<StepState> = None;
            let mut changes = vec![];
            for i in 0..switches * 4 {
                let now = i * INTERVAL;
                let stepped = state.as_ref().is_some_and(|s| s.step != YieldStep::None);
                let at = if (i / 4) % 2 == 0 {
                    input(ResourceKind::Cpu, if stepped { 0 } else { 1000 }, 4000)
                } else {
                    input(ResourceKind::Cpu, 0, 0)
                };
                let next = plan_step(state.as_ref(), &at, &cpu, now).state;
                let before = state.as_ref().map_or(YieldStep::None, |s| s.step);
                let after = next.as_ref().map_or(YieldStep::None, |s| s.step);
                if before != after {
                    changes.push(now);
                }
                state = next;
            }
            for switch in 0..switches {
                let window = switch * 4 * INTERVAL..(switch + 1) * 4 * INTERVAL;
                let count = changes.iter().filter(|at| window.contains(at)).count();
                prop_assert!(count <= 1, "{mode:?} {changes:?}");
            }
        }
    }
}

mod victims {
    use cedegrid::contention::{VictimFacts, victim_order};

    fn facts(id: &str) -> VictimFacts {
        VictimFacts {
            id: id.into(),
            controllable: true,
            frees: true,
            pending: false,
            guaranteed: false,
            progress_lost_ms: 0,
            amount: 1000,
        }
    }

    fn order(mut candidates: Vec<VictimFacts>, needed: u64) -> Vec<String> {
        victim_order(&mut candidates, needed);
        candidates.into_iter().map(|c| c.id).collect()
    }

    #[test]
    fn uncontrollable_allocations_are_excluded() {
        let mut lost = facts("a");
        lost.controllable = false;
        assert_eq!(order(vec![lost, facts("b")], 500), ["b"]);
    }

    #[test]
    fn each_rule_orders_in_turn() {
        // 1. Only allocations that free the contended resource come first.
        let mut idle = facts("a");
        idle.frees = false;
        assert_eq!(order(vec![idle, facts("b")], 500), ["b", "a"]);
        // 2. Pending before running.
        let mut pending = facts("b");
        pending.pending = true;
        assert_eq!(order(vec![facts("a"), pending], 500), ["b", "a"]);
        // 3. Opportunistic before Guaranteed.
        let mut guaranteed = facts("a");
        guaranteed.guaranteed = true;
        assert_eq!(order(vec![guaranteed, facts("b")], 500), ["b", "a"]);
        // 4. Least progress lost since the last checkpoint.
        let mut long = facts("a");
        long.progress_lost_ms = 60_000;
        let mut recent = facts("b");
        recent.progress_lost_ms = 1_000;
        assert_eq!(order(vec![long, recent], 500), ["b", "a"]);
        // 5. Best fit: the smallest amount that covers the need, else the largest.
        let mut small = facts("a");
        small.amount = 400;
        let mut fits = facts("b");
        fits.amount = 600;
        let mut large = facts("c");
        large.amount = 5000;
        assert_eq!(
            order(vec![small.clone(), large.clone(), fits], 500),
            ["b", "c", "a"]
        );
        assert_eq!(order(vec![small, large], 9000), ["c", "a"]);
        // 6. Assignment ID breaks the remaining ties.
        assert_eq!(order(vec![facts("b"), facts("a")], 500), ["a", "b"]);
    }

    #[test]
    fn pending_outranks_class_as_the_spec_orders_it() {
        // FR-016 ranks pending before running above opportunistic before Guaranteed.
        let mut pending_guaranteed = facts("a");
        pending_guaranteed.pending = true;
        pending_guaranteed.guaranteed = true;
        assert_eq!(order(vec![facts("b"), pending_guaranteed], 500), ["a", "b"]);
    }
}

mod cpu_allowance {
    use cedegrid::contention::*;

    fn allowance(mode: ContentionMode, budget: u64, saturated: bool) -> u64 {
        cpu_allowance(
            mode,
            &ResourceThresholds::defaults(ResourceKind::Cpu),
            8000,
            budget,
            saturated,
        )
    }

    #[test]
    fn each_mode_bounds_own_cpu_as_the_spec_defines() {
        // Unsaturated: idle capacity minus the mode's margin of schedulable capacity.
        assert_eq!(allowance(ContentionMode::YieldFirst, 5000, false), 3000);
        assert_eq!(allowance(ContentionMode::Performance, 5000, false), 4200);
        assert_eq!(allowance(ContentionMode::Neutral, 5000, false), 5000);
        // Saturated: measured external demand is suppressed. Yield First and
        // Performance still keep their margin free; Neutral converges to the fair
        // share and never below it.
        assert_eq!(allowance(ContentionMode::YieldFirst, 700, true), 0);
        assert_eq!(allowance(ContentionMode::Performance, 7000, true), 6200);
        assert_eq!(allowance(ContentionMode::Neutral, 700, true), 4000);
        assert_eq!(allowance(ContentionMode::Neutral, 7000, true), 4000);
    }
}
