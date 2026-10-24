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
        modes: Default::default(),
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
            usage: None,
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

/// Shared builders for the mode tests.
mod modes {
    pub use cedegrid::contention::*;

    pub fn input(
        kind: ResourceKind,
        mode: ContentionMode,
        capacity: u64,
        external: u64,
    ) -> ModeInput {
        ModeInput {
            kind,
            mode,
            capacity,
            external,
            principals: None,
            background: 0,
            saturated: false,
            unknown_activity: false,
        }
    }

    pub fn allowance(input: &ModeInput) -> u64 {
        own_allowance(input, &ResourceThresholds::defaults(input.kind))
    }

    pub fn principal(uid: Option<u32>, usage: u64) -> (Principal, u64) {
        (
            Principal {
                uid,
                is_owner: uid == Some(1000),
            },
            usage,
        )
    }
}

mod yield_first {
    use super::modes::*;

    #[test]
    fn own_work_keeps_a_quarter_of_capacity_free_on_every_resource() {
        for kind in [
            ResourceKind::Cpu,
            ResourceKind::Ram,
            ResourceKind::GpuMemory,
        ] {
            let mut view = input(kind, ContentionMode::YieldFirst, 8000, 2000);
            assert_eq!(allowance(&view), 4000, "{kind:?}");
            // To zero if needed.
            view.external = 7000;
            assert_eq!(allowance(&view), 0, "{kind:?}");
        }
    }

    #[test]
    fn unknown_activity_counts_as_external_demand() {
        let mut view = input(
            ResourceKind::GpuCompute,
            ContentionMode::YieldFirst,
            8000,
            0,
        );
        assert_eq!(allowance(&view), 6000);
        view.unknown_activity = true;
        assert_eq!(allowance(&view), 0);
    }

    #[test]
    fn it_responds_in_three_intervals_and_scales_up_slowly() {
        let thresholds = ResourceThresholds::defaults(ResourceKind::Cpu);
        let timing = |mode| LadderTiming::new(&thresholds, mode, 500);
        assert_eq!(timing(ContentionMode::YieldFirst).response_ms, 1500);
        assert_eq!(timing(ContentionMode::YieldFirst).scale_up_ms, 30_000);
        assert!(
            timing(ContentionMode::YieldFirst).scale_up_ms
                > timing(ContentionMode::Neutral).scale_up_ms
        );
    }
}

mod neutral {
    use super::modes::*;

    #[test]
    fn max_min_shares_split_contended_capacity_equally_and_conserve_work() {
        assert_eq!(max_min_shares(9000, &[None, None]), [4500, 4500]);
        // A principal that wants less than an equal share gets what it wants.
        assert_eq!(
            max_min_shares(9000, &[None, Some(1000), None]),
            [4000, 1000, 4000]
        );
        assert_eq!(
            max_min_shares(9000, &[None, Some(1000), Some(500)]),
            [7500, 1000, 500]
        );
        // The remainder of an uneven split goes to the first principal.
        assert_eq!(max_min_shares(10, &[None, None, None]), [4, 3, 3]);
    }

    #[test]
    fn saturated_cpu_converges_to_the_fair_share_of_one_to_three_principals_and_unknown_owners() {
        for (external, expected) in [(1, 4000), (2, 2666), (3, 2000)] {
            let mut principals = vec![principal(Some(1000), 0)];
            for uid in 0..external {
                principals.push(principal(Some(2000 + uid), 3000));
            }
            let mut view = input(
                ResourceKind::Cpu,
                ContentionMode::Neutral,
                8000,
                3000 * u64::from(external),
            );
            view.saturated = true;
            view.principals = Some(principals.clone());
            assert!(allowance(&view).abs_diff(expected) <= 1, "{external}");
            // Processes whose owner is unreadable are one more principal.
            principals.push(principal(None, 900));
            view.principals = Some(principals);
            let with_unknown = allowance(&view);
            assert!(with_unknown < expected, "{external}: {with_unknown}");
        }
    }

    #[test]
    fn own_work_keeps_its_share_and_idle_capacity_but_never_more_under_contention() {
        // Idle capacity is own work's to use, minus the Performance margin (FR-021).
        assert_eq!(
            allowance(&input(
                ResourceKind::Cpu,
                ContentionMode::Neutral,
                8000,
                1000
            )),
            6200
        );
        // Saturated by a demanding principal: exactly the fair share, whatever own uses now.
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 6000);
        view.saturated = true;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 6000)]);
        assert_eq!(allowance(&view), 4000);
        // Own work alone saturating the CPU, with only a principal below the
        // contention floor, is not contended: idle capacity minus the margin.
        view.external = 10;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 10)]);
        assert_eq!(allowance(&view), 7190);
        // The account's unmanaged usage comes out of own work's share.
        view.external = 6500;
        view.principals = Some(vec![
            principal(Some(1000), 500),
            principal(Some(2000), 6000),
        ]);
        assert_eq!(allowance(&view), 3500);
    }

    #[test]
    fn system_accounts_are_background_that_never_contends() {
        // External usage of 6000: 3000 by a principal, 3000 by system accounts,
        // which come off the capacity and do not share it.
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 6000);
        view.saturated = true;
        view.background = 3000;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 3000)]);
        assert_eq!(allowance(&view), 2500);
        // System usage alone on a saturated host is not contention.
        view.external = 3000;
        view.principals = Some(vec![principal(Some(1000), 0)]);
        assert!(!contended(
            &view,
            &ResourceThresholds::defaults(ResourceKind::Cpu)
        ));
        view.mode = ContentionMode::YieldFirst;
        assert_eq!(allowance(&view), 3000);
    }

    #[test]
    fn unobserved_owners_still_count_as_one_principal_under_saturation() {
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 5000);
        view.saturated = true;
        assert_eq!(allowance(&view), 4000);
        // Usage that no visible principal accounts for is one unknown principal.
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(0), 0)]);
        assert_eq!(allowance(&view), 4000);
    }

    #[test]
    fn memory_floors_always_come_before_the_fair_share() {
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            // Capacity is above the floor; external use leaves own work 1000.
            let mut view = input(kind, ContentionMode::Neutral, 10_000, 9000);
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 9000)]);
            assert_eq!(allowance(&view), 1000, "{kind:?}");
        }
    }
}

/// Ordered targets (FR-065, SC-013) and the contended state (FR-020 to FR-022).
mod ordered_targets {
    use super::modes::*;
    use proptest::prelude::*;

    fn targets(view: &ModeInput) -> [u64; 3] {
        [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ]
        .map(|mode| {
            allowance(&ModeInput {
                mode,
                ..view.clone()
            })
        })
    }

    proptest! {
        #[test]
        fn yield_first_never_exceeds_neutral_and_neutral_never_exceeds_performance(
            kind in prop_oneof![
                Just(ResourceKind::Cpu),
                Just(ResourceKind::Ram),
                Just(ResourceKind::GpuCompute),
                Just(ResourceKind::GpuMemory)
            ],
            capacity in 1u64..100_000,
            external_share in 0u64..=150,
            observed in any::<bool>(),
            usages in prop::collection::vec((prop_oneof![Just(None), (0u32..3000).prop_map(Some)], 0u64..60_000), 0..5),
            owner_unmanaged in 0u64..5000,
            saturated in any::<bool>(),
            unknown_activity in any::<bool>(),
        ) {
            let external = capacity * external_share / 100;
            let mut principals = vec![principal(Some(1000), owner_unmanaged)];
            principals.extend(usages.into_iter().filter(|(uid, _)| *uid != Some(1000)).map(|(uid, usage)| principal(uid, usage)));
            let view = ModeInput {
                principals: observed.then_some(principals),
                saturated,
                unknown_activity,
                ..input(kind, ContentionMode::Neutral, capacity, external)
            };
            let [yield_first, neutral, performance] = targets(&view);
            prop_assert!(yield_first <= neutral, "{view:?}: {yield_first} > {neutral}");
            prop_assert!(neutral <= performance, "{view:?}: {neutral} > {performance}");
        }
    }

    #[test]
    fn contended_compute_takes_yield_first_to_zero_and_the_others_to_the_fair_share() {
        for kind in [ResourceKind::Cpu, ResourceKind::GpuCompute] {
            // Saturated, with an external principal above the contention floor.
            let mut view = input(kind, ContentionMode::Neutral, 8000, 6000);
            view.saturated = true;
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 6000)]);
            assert_eq!(targets(&view), [0, 4000, 4000], "{kind:?}");
            // Unknown activity counts as one contending principal.
            let mut unknown = input(kind, ContentionMode::Neutral, 8000, 0);
            unknown.unknown_activity = true;
            assert_eq!(targets(&unknown), [0, 4000, 4000], "{kind:?}");
            // Saturation by own work alone is not contention.
            let mut alone = input(kind, ContentionMode::Neutral, 8000, 0);
            alone.saturated = true;
            assert_eq!(targets(&alone), [6000, 7200, 7200], "{kind:?}");
        }
    }

    #[test]
    fn memory_floors_bound_every_mode_and_yield_first_keeps_its_margin() {
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            let view = input(kind, ContentionMode::Neutral, 8000, 2000);
            assert_eq!(targets(&view), [4000, 6000, 6000], "{kind:?}");
        }
    }
}

mod performance_and_guaranteed {
    use super::modes::*;

    #[test]
    fn performance_keeps_a_small_margin_on_compute_and_the_floor_on_memory() {
        assert_eq!(
            allowance(&input(
                ResourceKind::Cpu,
                ContentionMode::Performance,
                8000,
                2000
            )),
            5200
        );
        assert_eq!(
            allowance(&input(
                ResourceKind::GpuCompute,
                ContentionMode::Performance,
                8000,
                0
            )),
            7200
        );
        // Memory capacity is already above the floor; no margin is added.
        assert_eq!(
            allowance(&input(
                ResourceKind::Ram,
                ContentionMode::Performance,
                8000,
                2000
            )),
            6000
        );
        let thresholds = ResourceThresholds::defaults(ResourceKind::Cpu);
        assert_eq!(
            LadderTiming::new(&thresholds, ContentionMode::Performance, 500).scale_up_ms,
            2000
        );
    }

    #[test]
    fn guaranteed_memory_is_drained_last_and_only_after_its_wait() {
        let rule = |opportunistic_left, breached_ms| {
            guaranteed_may_drain(
                ResourceKind::Ram,
                YieldCause::FloorBreach,
                opportunistic_left,
                breached_ms,
                10_000,
            )
        };
        assert!(rule(false, 10_000));
        assert!(!rule(false, 9999), "the wait has not passed");
        assert!(!rule(true, 60_000), "opportunistic work goes first");
        assert!(guaranteed_may_drain(
            ResourceKind::Ram,
            YieldCause::EnvelopeBreach,
            false,
            5000,
            5000
        ));
        assert!(!guaranteed_may_drain(
            ResourceKind::Ram,
            YieldCause::ExternalGrowth,
            false,
            60_000,
            5000
        ));
        assert!(!guaranteed_may_drain(
            ResourceKind::Cpu,
            YieldCause::FloorBreach,
            false,
            60_000,
            5000
        ));
    }
}

/// Floors, floor policy A and the Guaranteed rules through the policy engine.
mod guaranteed_policy {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldCause, YieldStep};

    fn ram(id: &str, class: AllocationClass, phase: AllocationPhase, mib: u64) -> Allocation {
        let resources = Resources {
            ram_mib: mib,
            ..Resources::default()
        };
        Allocation {
            id: id.into(),
            class,
            phase,
            requested: resources.clone(),
            observed: (phase != AllocationPhase::Pending).then_some(resources),
            observed_upper: None,
            modes: Default::default(),
        }
    }

    /// 32000 MiB total with 700 MiB free and a 1000 MiB floor: the floor is
    /// breached, but free memory is above the critical level (512 MiB, the
    /// minimum floor, since half the floor is less).
    fn breached() -> Snapshot {
        let mut snapshot = snapshot(1000);
        snapshot.available_ram_mib = Some(700);
        snapshot
    }

    /// No free memory: a critical floor breach (FR-063).
    fn critical() -> Snapshot {
        let mut snapshot = snapshot(1000);
        snapshot.available_ram_mib = Some(0);
        snapshot
    }

    /// Readiness facts for one allocation, as its supervisor reports them.
    fn ready(hint_ready: bool, observes_drain: bool) -> AllocationFacts {
        AllocationFacts {
            hint_ready,
            observes_drain,
            ..AllocationFacts::default()
        }
    }

    /// When each allocation first reaches each step, over `until_ms`.
    fn first_steps(
        config: &Config,
        snapshot: &dyn Fn(u64) -> Snapshot,
        allocations: Vec<Allocation>,
        facts: BTreeMap<String, AllocationFacts>,
        until_ms: u64,
    ) -> BTreeMap<(String, YieldStep), u64> {
        let mut engine = PolicyEngine::new();
        let input = PolicyInput {
            allocations,
            facts,
            ..PolicyInput::default()
        };
        let mut first = BTreeMap::new();
        for now in (0..=until_ms).step_by(500) {
            let decision = engine
                .evaluate(config, &snapshot(now), &input, now)
                .unwrap();
            for action in decision
                .actions
                .iter()
                .filter(|a| a.resource == ResourceKey::Ram)
            {
                first
                    .entry((action.allocation_id.clone(), action.step))
                    .or_insert(now);
            }
        }
        first
    }

    /// When one opportunistic allocation with these facts first reaches each step.
    fn alone(
        snapshot: &dyn Fn(u64) -> Snapshot,
        facts: AllocationFacts,
    ) -> BTreeMap<YieldStep, u64> {
        first_steps(
            &config(),
            snapshot,
            vec![ram(
                "a",
                AllocationClass::Opportunistic,
                AllocationPhase::Running,
                8000,
            )],
            BTreeMap::from([("a".into(), facts)]),
            6000,
        )
        .into_iter()
        .map(|((_, step), at)| (step, at))
        .collect()
    }

    #[test]
    fn only_hint_ready_allocations_are_hinted_and_others_drain_without_a_probe() {
        // A plain command drains at once, without a refused hint first.
        let plain = alone(&|_| breached(), ready(false, false));
        assert_eq!(plain.get(&YieldStep::Drain), Some(&0), "{plain:?}");
        assert!(!plain.contains_key(&YieldStep::Hint), "{plain:?}");
        // An SDK worker is hinted first and drained only at the hint deadline.
        let sdk = alone(&|_| breached(), ready(true, true));
        assert_eq!(sdk.get(&YieldStep::Hint), Some(&0), "{sdk:?}");
        assert_eq!(sdk.get(&YieldStep::Drain), Some(&5000), "{sdk:?}");
    }

    #[test]
    fn a_late_acker_is_hinted_once_its_supervisor_reports_it_ready() {
        let mut engine = PolicyEngine::new();
        let allocations = vec![ram(
            "late",
            AllocationClass::Guaranteed,
            AllocationPhase::Running,
            8000,
        )];
        let input = |hint_ready| PolicyInput {
            allocations: allocations.clone(),
            facts: BTreeMap::from([("late".into(), ready(hint_ready, hint_ready))]),
            ..PolicyInput::default()
        };
        let calm = snapshot(1000);
        engine.evaluate(&config(), &calm, &input(false), 0).unwrap();
        // The worker acked during the previous interval; the next breach hints it.
        let decision = engine
            .evaluate(&config(), &breached(), &input(true), 500)
            .unwrap();
        assert_eq!(
            step(&decision, "late"),
            Some((YieldStep::Hint, YieldCause::FloorBreach))
        );
    }

    #[test]
    fn the_critical_path_shortens_the_hint_drains_at_once_and_terminates_the_unobserving() {
        let sdk = alone(&|_| critical(), ready(true, true));
        assert_eq!(sdk.get(&YieldStep::Hint), Some(&0), "{sdk:?}");
        assert_eq!(sdk.get(&YieldStep::Drain), Some(&1000), "{sdk:?}");
        // A 0.2 SDK worker polls the drain file but does not ack targets.
        let old = alone(&|_| critical(), ready(false, true));
        assert_eq!(old.get(&YieldStep::Drain), Some(&0), "{old:?}");
        let plain = alone(&|_| critical(), ready(false, false));
        assert_eq!(plain.get(&YieldStep::Terminate), Some(&0), "{plain:?}");
        assert!(!plain.contains_key(&YieldStep::Drain), "{plain:?}");
    }

    /// SC-012: the policy's decision time plus the supervisor's deadlines (drain
    /// timeout, termination grace) and one observation interval.
    #[test]
    fn sc_012_memory_reclaim_stays_within_its_bounds() {
        let config = config();
        let after_drain = config.lifecycle.drain_timeout_ms + config.lifecycle.term_grace_ms + 500;
        let after_terminate = config.lifecycle.term_grace_ms + 500;
        let decided = |snapshot: &dyn Fn(u64) -> Snapshot, class, facts, step| {
            first_steps(
                &config,
                snapshot,
                vec![ram("a", class, AllocationPhase::Running, 8000)],
                BTreeMap::from([("a".into(), facts)]),
                12_000,
            )
            .get(&("a".into(), step))
            .copied()
            .expect("the step is decided")
        };
        let opportunistic = AllocationClass::Opportunistic;
        let guaranteed = AllocationClass::Guaranteed;
        let hinted = decided(
            &|_| breached(),
            opportunistic,
            ready(true, true),
            YieldStep::Drain,
        );
        assert!(hinted + after_drain <= 10_500, "{hinted}");
        let plain = decided(
            &|_| breached(),
            opportunistic,
            ready(false, false),
            YieldStep::Drain,
        );
        assert!(plain + after_drain <= 10_500, "{plain}");
        let kept = decided(
            &|_| breached(),
            guaranteed,
            ready(true, true),
            YieldStep::Drain,
        );
        assert!(kept + after_drain <= 15_500, "{kept}");
        let critical_hint = decided(
            &|_| critical(),
            opportunistic,
            ready(true, true),
            YieldStep::Drain,
        );
        assert!(critical_hint + after_drain <= 6500, "{critical_hint}");
        let unobserved = decided(
            &|_| critical(),
            opportunistic,
            ready(false, false),
            YieldStep::Terminate,
        );
        assert!(unobserved + after_terminate <= 2500, "{unobserved}");
    }

    #[test]
    fn guaranteed_memory_waits_for_its_hint_and_ram_window_except_on_the_critical_path() {
        let guaranteed = || {
            vec![ram(
                "g",
                AllocationClass::Guaranteed,
                AllocationPhase::Running,
                8000,
            )]
        };
        let facts = |hint_ready| BTreeMap::from([("g".into(), ready(hint_ready, true))]);
        let drained = |snapshot: &dyn Fn(u64) -> Snapshot, hint_ready, until| {
            first_steps(&config(), snapshot, guaranteed(), facts(hint_ready), until)
                .get(&("g".into(), YieldStep::Drain))
                .copied()
        };
        // RAM: the longer of the Guaranteed hint deadline (5 s) and the window (10 s).
        assert_eq!(drained(&|_| breached(), true, 12_000), Some(10_000));
        assert_eq!(drained(&|_| breached(), false, 12_000), Some(10_000));
        // A breach that clears within the window drains nothing.
        let clears = |now: u64| {
            if now < 6000 {
                breached()
            } else {
                snapshot(1000)
            }
        };
        assert_eq!(drained(&clears, true, 12_000), None);
        // The critical path skips the window: the critical hint deadline, or at once.
        assert_eq!(drained(&|_| critical(), true, 3000), Some(1000));
        assert_eq!(drained(&|_| critical(), false, 3000), Some(0));
    }

    fn step(decision: &Decision, id: &str) -> Option<(YieldStep, YieldCause)> {
        decision
            .actions
            .iter()
            .find(|a| a.allocation_id == id && a.resource == ResourceKey::Ram)
            .map(|a| (a.step, a.cause))
    }

    fn run(config: &Config, allocations: Vec<Allocation>, until_ms: u64) -> Vec<(u64, Decision)> {
        let mut engine = PolicyEngine::new();
        // Plain commands without the SDK cannot take a hint.
        let facts = allocations
            .iter()
            .map(|a| {
                let mut facts = AllocationFacts::default();
                facts
                    .unavailable
                    .insert(ResourceKey::Ram, [YieldStep::Hint].into());
                (a.id.clone(), facts)
            })
            .collect();
        let input = PolicyInput {
            allocations,
            facts,
            ..PolicyInput::default()
        };
        (0..=until_ms)
            .step_by(500)
            .map(|now| {
                (
                    now,
                    engine.evaluate(config, &breached(), &input, now).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn policy_a_drains_guaranteed_ram_only_after_the_breach_window_and_hint_deadline() {
        // The RAM breach window (10 s) applies to a plain command too; without a
        // window, an allocation that cannot take a hint has nothing to wait for.
        let mut no_window = config();
        no_window.contention.ram.guaranteed_breach_window_s = Some(0);
        for (config, wait_ms) in [(config(), 10_000), (no_window, 0)] {
            let decisions = run(
                &config,
                vec![ram(
                    "g",
                    AllocationClass::Guaranteed,
                    AllocationPhase::Running,
                    8000,
                )],
                11_000,
            );
            for (now, decision) in &decisions {
                let drained =
                    step(decision, "g") == Some((YieldStep::Drain, YieldCause::FloorBreach));
                assert_eq!(
                    drained,
                    *now >= wait_ms,
                    "at {now} ms: {:?}",
                    decision.actions
                );
                assert!(
                    decision.resources[&ResourceKey::Ram]
                        .reasons
                        .iter()
                        .any(|r| r.code == "floor_breach"),
                    "at {now} ms"
                );
            }
        }
    }

    #[test]
    fn opportunistic_work_is_released_before_guaranteed_work() {
        let decisions = run(
            &config(),
            vec![
                ram(
                    "g",
                    AllocationClass::Guaranteed,
                    AllocationPhase::Running,
                    6000,
                ),
                ram(
                    "o",
                    AllocationClass::Opportunistic,
                    AllocationPhase::Running,
                    2000,
                ),
            ],
            10_000,
        );
        for (now, decision) in &decisions {
            assert_eq!(
                step(decision, "o").map(|s| s.0),
                Some(YieldStep::Drain),
                "at {now} ms"
            );
            assert_ne!(
                step(decision, "g").map(|s| s.0),
                Some(YieldStep::Drain),
                "at {now} ms"
            );
        }
    }

    #[test]
    fn guaranteed_work_is_capped_but_never_paused_or_drained_for_cpu() {
        let mut config = config();
        config.contention.cpu.mode = Some(ContentionMode::YieldFirst);
        let mut engine = PolicyEngine::new();
        // Usage above the request may be capped; the request itself never (FR-024).
        let input = PolicyInput {
            allocations: vec![allocation(
                "g",
                AllocationClass::Guaranteed,
                2000,
                Some(6000),
            )],
            ..PolicyInput::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        for now in (0..=20_000).step_by(500) {
            let decision = engine
                .evaluate(&config, &snapshot(8000), &input, now)
                .unwrap();
            for action in decision.actions.iter().filter(|a| a.allocation_id == "g") {
                seen.insert(action.step);
                if action.step == YieldStep::Cap {
                    assert!(action.target.is_some_and(|t| t >= 2000), "{action:?}");
                }
            }
        }
        assert!(seen.contains(&YieldStep::Cap), "{seen:?}");
        assert!(
            !seen.contains(&YieldStep::Pause) && !seen.contains(&YieldStep::Drain),
            "{seen:?}"
        );
        // Usage within the request is never capped, even when Yield First targets zero.
        let within = PolicyInput {
            allocations: vec![allocation(
                "g",
                AllocationClass::Guaranteed,
                6000,
                Some(6000),
            )],
            ..PolicyInput::default()
        };
        let mut engine = PolicyEngine::new();
        for now in (0..=5000).step_by(500) {
            let decision = engine
                .evaluate(&config, &snapshot(8000), &within, now)
                .unwrap();
            assert!(decision.actions.is_empty(), "{:?}", decision.actions);
        }
    }
}

/// Time-stepped scenarios for SC-002 to SC-004 with the provisional defaults. A
/// simulated host splits CPU by weighted max-min over threads and honours the
/// steps the policy applies; the real policy engine runs every 500 ms.
mod scenarios {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const CAPACITY: u64 = 8000;
    const INTERVAL: u64 = 500;

    /// Weighted max-min: claimants get CPU in proportion to their threads, never
    /// above what they can use.
    fn schedule(claims: &[(u64, u64)]) -> Vec<u64> {
        let mut shares = vec![0u64; claims.len()];
        let mut open: Vec<usize> = (0..claims.len()).filter(|i| claims[*i].1 > 0).collect();
        let mut remaining = CAPACITY;
        while !open.is_empty() && remaining > 0 {
            let weight: u64 = open.iter().map(|i| claims[*i].0).sum();
            let (done, rest): (Vec<usize>, Vec<usize>) = open
                .iter()
                .partition(|i| claims[**i].1 - shares[**i] <= remaining * claims[**i].0 / weight);
            if done.is_empty() {
                let level = remaining;
                for i in &open {
                    shares[*i] += level * claims[*i].0 / weight;
                }
                break;
            }
            for i in done {
                remaining -= claims[i].1 - shares[i];
                shares[i] = claims[i].1;
            }
            open = rest;
        }
        shares
    }

    struct World {
        config: Config,
        engine: PolicyEngine,
        /// Own work: threads and what it would use uncapped.
        own: (u64, u64),
        /// External work: threads and demand.
        external: (u64, u64),
        step: YieldStep,
        target: Option<u64>,
        drained: bool,
    }

    impl World {
        fn new(mode: ContentionMode, own: (u64, u64)) -> Self {
            let mut config = config();
            config.cpu.reserve_physical_cores = 0;
            config.contention.default_mode = mode;
            config
                .contention
                .acknowledge_danger
                .insert("performance_mode".into());
            Self {
                config,
                engine: PolicyEngine::new(),
                own,
                external: (0, 0),
                step: YieldStep::None,
                target: None,
                drained: false,
            }
        }

        /// One interval: run the host under the applied step, then the policy.
        fn tick(&mut self, now: u64) -> (u64, u64) {
            let own_demand = match self.step {
                YieldStep::Pause => 0,
                YieldStep::Cap => self.target.unwrap_or(self.own.1).min(self.own.1),
                _ => self.own.1,
            };
            let usage = schedule(&[(self.own.0, own_demand), self.external]);
            let (own, external) = (usage[0], usage[1]);
            let mut snapshot = snapshot(own + external);
            snapshot.principals = Some(vec![
                PrincipalUsage {
                    uid: Some(1000),
                    is_owner: true,
                    ..PrincipalUsage::default()
                },
                PrincipalUsage {
                    uid: Some(2000),
                    cpu_millicores: external,
                    ..PrincipalUsage::default()
                },
            ]);
            let input = PolicyInput {
                allocations: vec![allocation(
                    "own",
                    AllocationClass::Opportunistic,
                    1000,
                    Some(own),
                )],
                facts: BTreeMap::from([(
                    "own".into(),
                    AllocationFacts {
                        unavailable: BTreeMap::from([(
                            ResourceKey::Cpu,
                            [YieldStep::LowerShare].into(),
                        )]),
                        ..AllocationFacts::default()
                    },
                )]),
                ..PolicyInput::default()
            };
            let decision = self
                .engine
                .evaluate(&self.config, &snapshot, &input, now)
                .unwrap();
            let action = decision
                .actions
                .iter()
                .find(|a| a.allocation_id == "own" && a.resource == ResourceKey::Cpu);
            self.step = action.map_or(YieldStep::None, |a| a.step);
            self.target = action.and_then(|a| a.target);
            self.drained |= self.step >= YieldStep::Drain || !decision.would_drain.is_empty();
            (own, external)
        }
    }

    #[test]
    fn sc_002_yield_first_restores_its_margin_within_three_intervals_without_draining() {
        let mut world = World::new(ContentionMode::YieldFirst, (8, 8000));
        let mut now = 0;
        while now < 5000 {
            world.tick(now);
            now += INTERVAL;
        }
        world.external = (4, 4000);
        let appeared = now;
        let mut restored_at = None;
        while now < appeared + 30_000 {
            let (own, external) = world.tick(now);
            // Within the margin: own and external together leave 25% free.
            if restored_at.is_none() && own + external <= CAPACITY * 3 / 4 && external >= 4000 {
                restored_at = Some(now);
            }
            now += INTERVAL;
        }
        let restored_at = restored_at.expect("Yield First never restored its margin");
        assert!(
            restored_at - appeared <= 3 * INTERVAL,
            "after {} ms",
            restored_at - appeared
        );
        assert!(!world.drained, "CPU contention alone never drains (SC-007)");
    }

    /// Saturation hides how much external work wants: own work with many threads
    /// leaves a demanding external process only a sliver. In every mode the
    /// contended target applies at once, so the CPU is back within 2 monitor
    /// intervals: interval 1 observes the demand, interval 2 runs with the step.
    #[test]
    fn sc_002_contended_cpu_returns_within_two_intervals_in_every_mode() {
        for mode in [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ] {
            let mut world = World::new(mode, (64, 8000));
            let mut now = 0;
            while now < 5000 {
                world.tick(now);
                now += INTERVAL;
            }
            world.external = (5, 5000);
            world.tick(now);
            // Until the pause bound, the CPU stays returned.
            for _ in 1..18 {
                now += INTERVAL;
                let (own, external) = world.tick(now);
                let returned = match mode {
                    ContentionMode::YieldFirst => own == 0 && external == 5000,
                    _ => own <= 4000 + CAPACITY / 100 && external + CAPACITY / 100 >= 4000,
                };
                assert!(returned, "{mode:?} at {now}: own {own} external {external}");
            }
            assert!(!world.drained, "{mode:?}");
        }
    }

    /// While a resource is not contended, a held cap rises again toward the mode's
    /// target at the mode's scale-up rate (FR-013, FR-020).
    #[test]
    fn a_held_cap_rises_again_when_external_demand_shrinks() {
        for (mode, target) in [
            (ContentionMode::YieldFirst, 5000),
            (ContentionMode::Neutral, 6200),
            (ContentionMode::Performance, 6200),
        ] {
            let mut world = World::new(mode, (64, 8000));
            let mut now = 0;
            while now < 5000 {
                world.tick(now);
                now += INTERVAL;
            }
            world.external = (5, 5000);
            while now < 15_000 {
                world.tick(now);
                now += INTERVAL;
            }
            // Idle capacity minus the margin: 8000 - 1000 - 25% or 10% of 8000.
            world.external = (1, 1000);
            let shrunk = now;
            let mut recovered = None;
            while now < shrunk + 40_000 {
                let (own, external) = world.tick(now);
                assert!(own + external <= CAPACITY, "{mode:?}");
                if recovered.is_none() && own + CAPACITY / 100 >= target {
                    recovered = Some(now);
                }
                now += INTERVAL;
            }
            assert!(recovered.is_some(), "{mode:?} never recovered to {target}");
            assert!(!world.drained, "{mode:?}");
        }
    }

    #[test]
    fn sc_003_neutral_converges_to_half_with_one_contending_principal() {
        // Own work has twice the threads, so the host alone would give it two thirds.
        let mut world = World::new(ContentionMode::Neutral, (16, 16_000));
        let mut now = 0;
        while now < 5000 {
            world.tick(now);
            now += INTERVAL;
        }
        world.external = (8, 8000);
        let arrived = now;
        let mut shares = vec![];
        while now < arrived + 90_000 {
            let (own, _) = world.tick(now);
            if now >= arrived + 60_000 {
                shares.push(own * 100 / CAPACITY);
            }
            now += INTERVAL;
        }
        assert!(
            shares.iter().all(|share| (35..=65).contains(share)),
            "own share after 60 s: {shares:?}"
        );
        assert!(!world.drained);
    }

    #[test]
    fn sc_004_performance_ramps_on_an_idle_host_and_yields_within_the_reclaim_time() {
        // Ramp: one more 1000m job each time CPU/RAM expansion is allowed.
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = ContentionMode::Performance;
        config
            .contention
            .acknowledge_danger
            .insert("performance_mode".into());
        let mut engine = PolicyEngine::new();
        let mut jobs: Vec<Allocation> = vec![];
        let target = (CAPACITY - CAPACITY / 10) * 80 / 100;
        let mut reached = None;
        let mut last_growth = 0;
        for now in (0..=30_000).step_by(INTERVAL as usize) {
            for job in &mut jobs {
                job.phase = AllocationPhase::Running;
                job.observed = Some(job.requested.clone());
            }
            let own: u64 = jobs
                .iter()
                .filter_map(|j| j.observed.as_ref())
                .map(|o| o.cpu_millicores)
                .sum();
            if reached.is_none() && own >= target {
                reached = Some(now);
            }
            let input = PolicyInput {
                allocations: jobs.clone(),
                ..PolicyInput::default()
            };
            let decision = engine
                .evaluate(&config, &snapshot(own), &input, now)
                .unwrap();
            let cooldown = config
                .contention
                .thresholds(cedegrid::contention::ResourceKind::Cpu)
                .cooldown_ms(ContentionMode::Performance);
            if decision.cpu_ram_expansion_allowed
                && decision.admission_headroom.cpu_millicores >= 1000
                && now - last_growth >= cooldown
            {
                let mut job = allocation(
                    &format!("job-{now}"),
                    AllocationClass::Opportunistic,
                    1000,
                    None,
                );
                job.phase = AllocationPhase::Pending;
                jobs.push(job);
                last_growth = now;
            }
        }
        let reached = reached.expect("Performance never reached its ramp target");
        assert!(reached <= 30_000, "ramp target reached at {reached} ms");

        // Reclaim: external work gets its demand within 5 s of starting.
        let mut world = World::new(ContentionMode::Performance, (8, 8000));
        let mut now = 0;
        while now < 5000 {
            world.tick(now);
            now += INTERVAL;
        }
        world.external = (4, 3000);
        let started = now;
        let mut served_at = None;
        while now < started + 20_000 {
            let (_, external) = world.tick(now);
            if served_at.is_none() && external >= 3000 {
                served_at = Some(now);
            }
            now += INTERVAL;
        }
        let served_at = served_at.expect("external work never got its demand");
        assert!(
            served_at - started <= 5000,
            "after {} ms",
            served_at - started
        );
        assert!(!world.drained);
    }
}

/// Hints through the policy engine: compliance is judged by observed usage
/// and the hint deadline bounds the wait (FR-037).
mod hints {
    use super::*;
    use cedegrid::contention::{ResourceKey, YieldCause, YieldStep};

    /// The worker's RAM action at each interval: step, target and cause.
    type Steps = Vec<(u64, Option<(YieldStep, Option<u64>, YieldCause)>)>;

    /// 32000 MiB total, a 1000 MiB floor and 23300 MiB of external use: own work
    /// may hold 7700 MiB, so an 8000 MiB allocation must release 300 MiB. The
    /// 700 MiB left free stay above the critical level (512 MiB).
    fn run(compliant: bool, hint_unavailable_after: Option<u64>) -> Steps {
        let mut engine = PolicyEngine::new();
        let mut held = 8000;
        let mut steps = vec![];
        for now in (0..=8000).step_by(500) {
            let resources = Resources {
                ram_mib: held,
                ..Resources::default()
            };
            let mut snapshot = snapshot(1000);
            snapshot.available_ram_mib = Some(8700 - held);
            let mut facts = AllocationFacts {
                hint_ready: true,
                observes_drain: true,
                ..AllocationFacts::default()
            };
            if hint_unavailable_after.is_some_and(|after| now > after) {
                facts
                    .unavailable
                    .insert(ResourceKey::Ram, [YieldStep::Hint].into());
            }
            let input = PolicyInput {
                allocations: vec![Allocation {
                    id: "worker".into(),
                    class: AllocationClass::Opportunistic,
                    phase: AllocationPhase::Running,
                    requested: Resources {
                        ram_mib: 1000,
                        ..Resources::default()
                    },
                    observed: Some(resources),
                    observed_upper: None,
                    modes: Default::default(),
                }],
                facts: BTreeMap::from([("worker".into(), facts)]),
                ..PolicyInput::default()
            };
            let decision = engine.evaluate(&config(), &snapshot, &input, now).unwrap();
            let action = decision
                .actions
                .iter()
                .find(|a| a.allocation_id == "worker" && a.resource == ResourceKey::Ram)
                .map(|a| (a.step, a.target, a.cause));
            // A compliant worker shrinks to its target by the next observation.
            if compliant && let Some((YieldStep::Hint, Some(target), _)) = action {
                held = target;
            }
            steps.push((now, action));
        }
        steps
    }

    #[test]
    fn memory_contention_starts_with_a_hint_and_its_target() {
        let steps = run(false, None);
        assert_eq!(
            steps[0].1,
            Some((YieldStep::Hint, Some(7700), YieldCause::FloorBreach))
        );
    }

    #[test]
    fn a_compliant_worker_is_not_drained() {
        let steps = run(true, None);
        assert!(
            steps
                .iter()
                .all(|(_, action)| action.is_none_or(|a| a.0 < YieldStep::Drain)),
            "{steps:?}"
        );
    }

    #[test]
    fn a_non_compliant_worker_is_drained_at_the_hint_deadline() {
        let steps = run(false, None);
        let drained = steps
            .iter()
            .find(|(_, action)| action.is_some_and(|a| a.0 == YieldStep::Drain))
            .map(|(now, _)| *now);
        assert_eq!(drained, Some(5000), "{steps:?}");
    }

    #[test]
    fn a_hint_the_supervisor_cannot_apply_escalates_at_once() {
        let steps = run(false, Some(0));
        assert_eq!(steps[1].1.map(|a| a.0), Some(YieldStep::Drain), "{steps:?}");
    }
}
