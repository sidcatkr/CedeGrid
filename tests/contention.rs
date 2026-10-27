//! Contention-mode and yield-ladder properties. Builders mirror tests/policy.rs.
#[allow(dead_code)]
mod snapshot {
    include!("support/snapshot.rs");
}
use cedegrid::{config::Config, execution_model::AllocationClass, model::*, policy::PolicyEngine};
use proptest::prelude::*;
use std::collections::BTreeMap;

pub fn config() -> Config {
    let mut result = Config::default();
    result.cpu.reserve_physical_cores = 1;
    result.ram.reserve_mib = 1000;
    result.ram.reserve_percent = 0;
    // A 1000 MiB floor is near the 512 MiB minimum; the tests confirm it.
    result
        .contention
        .acknowledge_caution
        .insert("small_floor".into());
    result.gpu.reserve_vram_mib = 1000;
    result.gpu.reserve_vram_percent = 0;
    result.gpu.scale_up_cooldown_ms = Some(0);
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
        managed_cpu_lower_millicores: None,
        total_ram_mib: 32_000,
        available_ram_mib: Some(26_000),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: None,
        principals: None,
        external_processes: None,
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
            room: None,
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

    /// Pause and cap are never used for GPU memory, whatever the backend offers
    /// (FR-011).
    #[test]
    fn gpu_memory_never_pauses_or_caps() {
        let timing = timing(ResourceKind::GpuMemory, ContentionMode::Neutral);
        for unavailable in [BTreeSet::new(), BTreeSet::from([YieldStep::Hint])] {
            let (_, changes) = run(None, &timing, 0..60, |_| LadderInput {
                room: None,
                unavailable: unavailable.clone(),
                ..input(ResourceKind::GpuMemory, 1000, 4000)
            });
            assert!(!changes.is_empty());
            assert!(
                changes.iter().all(|(_, step)| !matches!(
                    step,
                    YieldStep::LowerShare | YieldStep::Cap | YieldStep::Pause
                )),
                "{changes:?}"
            );
        }
    }

    /// GPU compute shares are approximated by pausing, so Guaranteed work takes
    /// neither a GPU compute cap nor a pause (FR-024).
    #[test]
    fn guaranteed_gpu_compute_takes_no_cap_or_pause() {
        let timing = timing(ResourceKind::GpuCompute, ContentionMode::Neutral);
        let (_, changes) = run(None, &timing, 0..40, |_| LadderInput {
            room: None,
            guaranteed: true,
            allow_drain: false,
            unavailable: BTreeSet::from([YieldStep::LowerShare, YieldStep::Hint]),
            target: Some(500),
            ..input(ResourceKind::GpuCompute, 500, 1000)
        });
        assert!(changes.is_empty(), "{changes:?}");
    }

    /// A cap whose usage follows its target holds while the target moves down, as
    /// in Performance and probing; usage that stays above the cap escalates after
    /// the response time.
    #[test]
    fn an_effective_cap_holds_while_its_target_moves_down() {
        let timing = timing(ResourceKind::Cpu, ContentionMode::Performance);
        let at = |follows: bool| {
            move |now: u64| {
                let target = 7000u64.saturating_sub(now / INTERVAL * 100).max(4000);
                let usage = if follows { target + 100 } else { 8000 };
                LadderInput {
                    room: None,
                    unavailable: BTreeSet::from([YieldStep::LowerShare]),
                    target: Some(target),
                    usage: Some(usage),
                    ..input(ResourceKind::Cpu, usage.saturating_sub(target), 1000)
                }
            }
        };
        let (state, changes) = run(None, &timing, 0..40, at(true));
        assert_eq!(changes, [(0, YieldStep::Cap)]);
        assert_eq!(state.unwrap().target, Some(4000));
        let (_, changes) = run(None, &timing, 0..40, at(false));
        assert_eq!(
            changes[..2],
            [(0, YieldStep::Cap), (5000, YieldStep::Pause)]
        );
    }

    #[test]
    fn steps_escalate_in_order_and_memory_starts_at_a_ram_cap_or_the_hint() {
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
        // RAM starts at a cap where a delegated leaf allows it, GPU memory at the
        // hint; neither is ever paused (FR-011).
        for (kind, step) in [
            (ResourceKind::Ram, YieldStep::Cap),
            (ResourceKind::GpuMemory, YieldStep::Hint),
        ] {
            let first = plan_step(
                None,
                &input(kind, 1, 1),
                &timing(kind, ContentionMode::Neutral),
                0,
            );
            assert_eq!(first.state.unwrap().step, step, "{kind:?}");
            assert!(first.skipped.is_empty());
        }
        let mut rootless = input(ResourceKind::Ram, 1, 1);
        rootless.unavailable = [YieldStep::Cap].into();
        let first = plan_step(
            None,
            &rootless,
            &timing(ResourceKind::Ram, ContentionMode::Neutral),
            0,
        );
        assert_eq!(first.state.unwrap().step, YieldStep::Hint);
        assert_eq!(first.skipped, [YieldStep::Cap]);
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
        memory.unavailable = [YieldStep::Cap, YieldStep::Hint].into();
        let first = plan_step(
            None,
            &memory,
            &timing(ResourceKind::Ram, ContentionMode::Neutral),
            0,
        );
        assert_eq!(first.state.unwrap().step, YieldStep::Drain);
        assert_eq!(first.skipped, [YieldStep::Cap, YieldStep::Hint]);
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
            room: None,
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
            to_free: None,
            step: YieldStep::Pause,
            entered_at_ms: 0,
            cause: YieldCause::ExternalGrowth,
            target: None,
            trigger_external: 4000,
            calm_since_ms: None,
            contended_since_ms: Some(0),
            after_pause: false,
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

    /// FR-014: without a hint and with no drain for CPU alone, a pause still ends at
    /// its bound, as a cap at the minimum duty that does not become a pause again.
    #[test]
    fn a_pause_without_a_hint_ends_at_its_bound_as_a_minimum_duty_cap() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::YieldFirst);
        let paused = StepState {
            to_free: None,
            step: YieldStep::Pause,
            entered_at_ms: 0,
            cause: YieldCause::ExternalGrowth,
            target: Some(0),
            trigger_external: 4000,
            calm_since_ms: None,
            contended_since_ms: Some(0),
            after_pause: false,
        };
        let plain = |excess| LadderInput {
            room: None,
            allow_drain: false,
            unavailable: [YieldStep::LowerShare, YieldStep::Hint].into(),
            target: Some(0),
            usage: Some(0),
            ..input(ResourceKind::Cpu, excess, 4000)
        };
        // Paused work uses nothing, so nothing is contended, and the bound still holds.
        for excess in [0, 500] {
            let before = plan_step(Some(&paused), &plain(excess), &cpu, cpu.pause_bound_ms - 1);
            assert_eq!(before.state.unwrap().step, YieldStep::Pause, "{excess}");
            let after = plan_step(Some(&paused), &plain(excess), &cpu, cpu.pause_bound_ms);
            assert_eq!(after.skipped, [YieldStep::Hint], "{excess}");
            let capped = after.state.unwrap();
            assert_eq!(
                (capped.step, capped.target),
                (YieldStep::Cap, Some(0)),
                "{excess}"
            );
            assert!(capped.after_pause);
            // The minimum duty does not turn back into a pause while contended.
            let next = plan_step(Some(&capped), &plain(500), &cpu, cpu.pause_bound_ms + 500);
            assert_eq!(next.state.unwrap().step, YieldStep::Cap, "{excess}");
        }
    }

    #[test]
    fn steps_reverse_one_level_at_a_time_below_the_hysteresis_threshold() {
        let cpu = timing(ResourceKind::Cpu, ContentionMode::Performance);
        let capped = StepState {
            to_free: None,
            step: YieldStep::Cap,
            entered_at_ms: 0,
            cause: YieldCause::ExternalGrowth,
            target: Some(2000),
            trigger_external: 4000,
            calm_since_ms: None,
            contended_since_ms: None,
            after_pause: false,
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
                to_free: None,
                step,
                entered_at_ms: 0,
                cause: YieldCause::ExternalGrowth,
                target: None,
                trigger_external: 4000,
                calm_since_ms: None,
                contended_since_ms: None,
                after_pause: false,
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
    use cedegrid::contention::{ContentionMode, VictimFacts, victim_order};

    fn facts(id: &str) -> VictimFacts {
        VictimFacts {
            id: id.into(),
            controllable: true,
            frees: true,
            pending: false,
            mode: ContentionMode::Neutral,
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
        // 3. By tier: Yield First, then Neutral, then Performance (T093a).
        let mut eager = facts("a");
        eager.mode = ContentionMode::Performance;
        let mut gentle = facts("c");
        gentle.mode = ContentionMode::YieldFirst;
        gentle.guaranteed = true;
        gentle.progress_lost_ms = 60_000;
        assert_eq!(order(vec![eager, facts("b"), gentle], 500), ["c", "b", "a"]);
        // 4. Within a tier, opportunistic before Guaranteed.
        let mut guaranteed = facts("a");
        guaranteed.guaranteed = true;
        assert_eq!(order(vec![guaranteed, facts("b")], 500), ["b", "a"]);
        // 5. Least progress lost since the last checkpoint.
        let mut long = facts("a");
        long.progress_lost_ms = 60_000;
        let mut recent = facts("b");
        recent.progress_lost_ms = 1_000;
        assert_eq!(order(vec![long, recent], 500), ["b", "a"]);
        // 6. Best fit: the smallest amount that covers the need, else the largest.
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
        // 7. Assignment ID breaks the remaining ties.
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
            own: 0,
            principals: None,
            background: 0,
            saturated: false,
            psi_only: false,
            unknown_activity: false,
            satisfied: vec![],
            probe: None,
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
    fn own_work_keeps_its_margin_free_on_every_resource() {
        // 15% of compute and 20% of memory beyond the floor.
        for (kind, expected) in [
            (ResourceKind::Cpu, 4800),
            (ResourceKind::Ram, 4400),
            (ResourceKind::GpuMemory, 4400),
        ] {
            let mut view = input(kind, ContentionMode::YieldFirst, 8000, 2000);
            assert_eq!(allowance(&view), expected, "{kind:?}");
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
        assert_eq!(allowance(&view), 6800);
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
        // Idle capacity is own work's to use, minus the Neutral margin (FR-021).
        assert_eq!(
            allowance(&input(
                ResourceKind::Cpu,
                ContentionMode::Neutral,
                8000,
                1000
            )),
            6600
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
        assert_eq!(allowance(&view), 7590);
        // The account's unmanaged usage comes out of own work's share.
        view.external = 6500;
        view.principals = Some(vec![
            principal(Some(1000), 500),
            principal(Some(2000), 6000),
        ]);
        assert_eq!(allowance(&view), 3500);
    }

    /// A principal in its leave window uses little: max-min hands what it does not
    /// use to the others (FR-059, FR-021).
    #[test]
    fn a_leaving_principals_unused_share_is_redistributed() {
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 9000, 4500);
        view.saturated = true;
        view.principals = Some(vec![
            principal(Some(1000), 0),
            principal(Some(2000), 4500),
            principal(Some(2001), 0),
        ]);
        // 2001 is still a member but uses nothing: own work and 2000 split 9000.
        assert_eq!(allowance(&view), 4500);
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
        assert_eq!(allowance(&view), 3800);
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
            own in 0u64..120_000,
            satisfied in prop::collection::vec((prop_oneof![Just(None), (0u32..3000).prop_map(Some)], 0u64..60_000), 0..3),
            probe in prop::option::of(0u64..120_000),
        ) {
            let external = capacity * external_share / 100;
            let mut principals = vec![principal(Some(1000), owner_unmanaged)];
            principals.extend(usages.into_iter().filter(|(uid, _)| *uid != Some(1000)).map(|(uid, usage)| principal(uid, usage)));
            let view = ModeInput {
                principals: observed.then_some(principals),
                saturated,
                unknown_activity,
                own,
                satisfied,
                probe,
                ..input(kind, ContentionMode::Neutral, capacity, external)
            };
            let [yield_first, neutral, performance] = targets(&view);
            prop_assert!(yield_first <= neutral, "{view:?}: {yield_first} > {neutral}");
            prop_assert!(neutral <= performance, "{view:?}: {neutral} > {performance}");
        }
    }

    /// A principal whose demand probing found satisfied no longer contends and is
    /// bounded at that demand; usage above it contends again (FR-021).
    #[test]
    fn a_satisfied_principal_is_bounded_and_no_longer_contends() {
        let thresholds = ResourceThresholds::defaults(ResourceKind::Cpu);
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 120);
        view.saturated = true;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 120)]);
        view.satisfied = vec![(Some(2000), 200)];
        assert!(!contended(&view, &thresholds));
        // Idle targets: 8000 - 120 minus 15%, 5% or 2% of 8000.
        assert_eq!(targets(&view), [6680, 7480, 7720]);
        // Usage above the recorded demand contends again.
        view.external = 300;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 300)]);
        assert!(contended(&view, &thresholds));
        assert_eq!(targets(&view), [0, 4000, 7540]);
        // The unattributed principal is probed the same way.
        let mut pooled = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 120);
        pooled.saturated = true;
        pooled.principals = Some(vec![principal(Some(1000), 0)]);
        pooled.satisfied = vec![(None, 200)];
        assert!(!contended(&pooled, &thresholds));
    }

    /// While a small demand is probed, own work steps down from its level instead
    /// of falling to the fair share (FR-021, FR-022).
    #[test]
    fn a_probe_steps_down_instead_of_falling_to_the_fair_share() {
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 120);
        view.saturated = true;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 120)]);
        view.own = 7880;
        view.probe = Some(7480);
        // Yield First keeps its margin, at most the probe; Neutral holds the probe;
        // Performance keeps what external demand does not take.
        assert_eq!(targets(&view), [6680, 7480, 7720]);
    }

    /// Saturation while own work holds more than capacity minus Yield First's
    /// margin may be own work's doing, so it caps to the margin within the fair
    /// share; held within its margin, saturation means external demand outgrew it.
    #[test]
    fn own_saturation_beside_a_small_external_user_caps_before_it_pauses() {
        for kind in [ResourceKind::Cpu, ResourceKind::GpuCompute] {
            let mut view = input(kind, ContentionMode::Neutral, 8000, 160);
            view.saturated = true;
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 160)]);
            view.own = 7840;
            // 8000 - 160 - 15% is 6640; the fair share beside one other principal is
            // 4000; Performance keeps 8000 - 160 - 2%.
            assert_eq!(targets(&view), [4000, 4000, 7680], "{kind:?}");
            view.own = 6640;
            view.external = 1360;
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 1360)]);
            assert_eq!(targets(&view), [0, 4000, 6480], "{kind:?}");
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
            // Idle margins: 15% in Yield First, 5% in Neutral, 2% in Performance.
            assert_eq!(targets(&alone), [6800, 7600, 7840], "{kind:?}");
        }
    }

    #[test]
    fn memory_floors_bound_every_mode_and_yield_first_keeps_its_margin() {
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            let view = input(kind, ContentionMode::Neutral, 8000, 2000);
            assert_eq!(targets(&view), [4400, 6000, 6000], "{kind:?}");
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
            5840
        );
        assert_eq!(
            allowance(&input(
                ResourceKind::GpuCompute,
                ContentionMode::Performance,
                8000,
                0
            )),
            7840
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
        // Compute scales up every interval in Performance; memory keeps 2 s (FR-034).
        let thresholds = ResourceThresholds::defaults(ResourceKind::Cpu);
        assert_eq!(
            LadderTiming::new(&thresholds, ContentionMode::Performance, 500).scale_up_ms,
            500
        );
        let thresholds = ResourceThresholds::defaults(ResourceKind::Ram);
        assert_eq!(
            LadderTiming::new(&thresholds, ContentionMode::Performance, 500).scale_up_ms,
            2000
        );
    }

    /// Contended Performance keeps what external demand does not take, never less
    /// than the fair share; unknown activity holds it to the fair share (FR-022).
    #[test]
    fn contended_performance_gives_back_only_what_external_demand_takes() {
        for kind in [ResourceKind::Cpu, ResourceKind::GpuCompute] {
            let mut view = input(kind, ContentionMode::Performance, 8000, 600);
            view.saturated = true;
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 600)]);
            // 8000 - 600 - 2% is 7240, above the fair share of 4000.
            assert_eq!(allowance(&view), 7240, "{kind:?}");
            view.external = 6000;
            view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 6000)]);
            assert_eq!(allowance(&view), 4000, "{kind:?}");
            let mut unknown = input(kind, ContentionMode::Performance, 8000, 0);
            unknown.unknown_activity = true;
            assert_eq!(allowance(&unknown), 4000, "{kind:?}");
        }
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

    /// A memory hint lasts its hint deadline in every mode, Yield First included,
    /// and the hint carries the deadline the policy enforces (FR-037).
    #[test]
    fn memory_hints_last_their_deadline_in_every_mode_and_carry_it() {
        let mut config = config();
        config.contention.ram.mode = Some(ContentionMode::YieldFirst);
        let sdk = || {
            vec![ram(
                "a",
                AllocationClass::Opportunistic,
                AllocationPhase::Running,
                8000,
            )]
        };
        let first = first_steps(
            &config,
            &|_| breached(),
            sdk(),
            BTreeMap::from([("a".into(), ready(true, true))]),
            6000,
        );
        assert_eq!(
            first.get(&("a".into(), YieldStep::Drain)),
            Some(&5000),
            "{first:?}"
        );
        let deadline = |snapshot: Snapshot| {
            PolicyEngine::new()
                .evaluate(
                    &config,
                    &snapshot,
                    &PolicyInput {
                        allocations: sdk(),
                        facts: BTreeMap::from([("a".into(), ready(true, true))]),
                        ..PolicyInput::default()
                    },
                    0,
                )
                .unwrap()
                .actions
                .iter()
                .find(|a| a.step == YieldStep::Hint)
                .and_then(|a| a.deadline_ms)
        };
        assert_eq!(deadline(breached()), Some(5000));
        assert_eq!(deadline(critical()), Some(1000));
    }

    /// A Guaranteed hint gets the Guaranteed hint deadline from the hint, whatever
    /// the resource's hint deadline (FR-024).
    #[test]
    fn a_guaranteed_hint_gets_the_guaranteed_deadline() {
        for seconds in [2, 8] {
            let mut config = config();
            config.contention.guaranteed_hint_deadline_s = seconds;
            config.contention.ram.guaranteed_breach_window_s = Some(0);
            let first = first_steps(
                &config,
                &|_| breached(),
                vec![ram(
                    "g",
                    AllocationClass::Guaranteed,
                    AllocationPhase::Running,
                    8000,
                )],
                BTreeMap::from([("g".into(), ready(true, true))]),
                10_000,
            );
            assert_eq!(
                first.get(&("g".into(), YieldStep::Drain)),
                Some(&(seconds * 1000)),
                "{first:?}"
            );
        }
    }

    /// Opportunistic work nobody can act on does not block Guaranteed release.
    #[test]
    fn uncontrollable_opportunistic_work_does_not_block_guaranteed_release() {
        let mut lost = ready(false, false);
        lost.uncontrollable = true;
        let first = first_steps(
            &config(),
            &|_| critical(),
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
            BTreeMap::from([("g".into(), ready(false, true)), ("o".into(), lost)]),
            3000,
        );
        assert_eq!(
            first.get(&("g".into(), YieldStep::Drain)),
            Some(&0),
            "{first:?}"
        );
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

    /// A floor breach needs free memory below the floor. A Guaranteed job that
    /// requested 64 GiB and uses 8 GiB, with free memory well above the floor, is
    /// never stepped; only RAM admission is blocked. A real breach still follows
    /// FR-024.
    #[test]
    fn a_charge_beyond_the_budget_is_no_floor_breach() {
        let config = config();
        let mut job = ram(
            "g",
            AllocationClass::Guaranteed,
            AllocationPhase::Running,
            8192,
        );
        job.requested.ram_mib = 65_536;
        // 128 GiB with a 1000 MiB floor and the job's 8192 MiB in use.
        let host = |external: u64| {
            let mut snapshot = snapshot(1000);
            snapshot.total_ram_mib = 131_072;
            snapshot.available_ram_mib = Some(131_072 - external - 8192);
            snapshot
        };
        let facts = BTreeMap::from([("g".into(), ready(true, true))]);
        let mut engine = PolicyEngine::new();
        let input = PolicyInput {
            allocations: vec![job.clone()],
            facts: facts.clone(),
            ..PolicyInput::default()
        };
        for now in (0..=20_000).step_by(500) {
            // 42 880 MiB free: the charge exceeds the budget, usage does not.
            let decision = engine
                .evaluate(&config, &host(80_000), &input, now)
                .unwrap();
            assert!(
                decision.actions.is_empty(),
                "at {now}: {:?}",
                decision.actions
            );
            assert_eq!(decision.admission_headroom.ram_mib, 0, "at {now}");
            assert!(
                !decision.resources[&ResourceKey::Ram]
                    .reasons
                    .iter()
                    .any(|r| r.code == "floor_breach"),
                "at {now}"
            );
        }
        // 780 MiB free: below the floor, above the critical level.
        let first = first_steps(&config, &|_| host(122_100), vec![job], facts, 12_000);
        assert_eq!(first.get(&("g".into(), YieldStep::Hint)), Some(&0));
        assert_eq!(first.get(&("g".into(), YieldStep::Drain)), Some(&10_000));
    }

    /// Even the longest RAM breach window never delays the critical path (FR-063).
    #[test]
    fn the_longest_ram_breach_window_is_skipped_in_the_critical_zone() {
        let mut config = config();
        config.contention.ram.guaranteed_breach_window_s = Some(60);
        for (hint_ready, expected) in [(true, 1000), (false, 0)] {
            let first = first_steps(
                &config,
                &|_| critical(),
                vec![ram(
                    "g",
                    AllocationClass::Guaranteed,
                    AllocationPhase::Running,
                    8000,
                )],
                BTreeMap::from([("g".into(), ready(hint_ready, true))]),
                3000,
            );
            assert_eq!(first.get(&("g".into(), YieldStep::Drain)), Some(&expected));
        }
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
    use crate::snapshot::PolicyEngine;
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
                .acknowledge_caution
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
            let (_, mut previous) = world.tick(now);
            // Until the pause bound, the CPU stays returned. Performance gives back
            // what external work takes, at least its margin per interval, and never
            // goes below the fair share.
            for _ in 1..18 {
                now += INTERVAL;
                let (own, external) = world.tick(now);
                let returned = match mode {
                    ContentionMode::YieldFirst => own == 0 && external == 5000,
                    ContentionMode::Neutral => {
                        own <= 4000 + CAPACITY / 100 && external + CAPACITY / 100 >= 4000
                    }
                    ContentionMode::Performance => {
                        own + CAPACITY / 100 >= 4000
                            && (external >= previous + CAPACITY * 2 / 100
                                || external + CAPACITY / 100 >= 4000)
                    }
                };
                assert!(returned, "{mode:?} at {now}: own {own} external {external}");
                previous = external;
            }
            assert!(!world.drained, "{mode:?}");
        }
    }

    /// With own work held to its margin, Yield First pauses only once external
    /// demand outgrows the margin: a small blip fits in it and never pauses own
    /// work, while demand beyond the margin does (FR-020).
    #[test]
    fn yield_first_pauses_only_when_external_demand_exceeds_its_margin() {
        let mut world = World::new(ContentionMode::YieldFirst, (64, 8000));
        let mut now = 0;
        while now < 5000 {
            world.tick(now);
            now += INTERVAL;
        }
        // 5% of capacity, well inside the 15% margin.
        world.external = (1, 400);
        let margin = CAPACITY * 15 / 100;
        for interval in 0..40 {
            let (own, external) = world.tick(now);
            assert_ne!(world.step, YieldStep::Pause, "at {now}: own {own}");
            assert_eq!(external, 400);
            // After the step applies, own work keeps the margin free beside the blip.
            if interval >= 2 {
                assert!(own + 400 + margin <= CAPACITY + CAPACITY / 100, "{own}");
                assert!(own + CAPACITY / 100 >= CAPACITY - 400 - margin, "{own}");
            }
            now += INTERVAL;
        }
        // Demand beyond the margin is contention: own work pauses within 2 intervals.
        world.external = (5, 4000);
        world.tick(now);
        now += INTERVAL;
        world.tick(now);
        assert_eq!(world.step, YieldStep::Pause);
        assert!(!world.drained);
    }

    /// Own work that saturates the host itself is capped to the margin, not paused,
    /// when a small external user appears beside it (FR-020).
    #[test]
    fn yield_first_does_not_pause_own_saturation_for_a_small_external_user() {
        let mut world = World::new(ContentionMode::YieldFirst, (64, 8000));
        world.external = (1, 400);
        let mut now = 0;
        let mut last = (0, 0);
        for _ in 0..40 {
            last = world.tick(now);
            assert_ne!(world.step, YieldStep::Pause, "at {now}: {last:?}");
            now += INTERVAL;
        }
        let (own, external) = last;
        assert_eq!(external, 400);
        assert!(
            own > 0 && own + 400 + CAPACITY * 15 / 100 <= CAPACITY + CAPACITY / 100,
            "{own}"
        );
        assert!(!world.drained);
    }

    /// Runs the world for `intervals` and returns own work's average, the number of
    /// step changes and whether own work was ever paused.
    fn steady(world: &mut World, from: u64, intervals: u64) -> (u64, u32, bool) {
        let (mut total, mut changes, mut paused) = (0, 0, false);
        let mut last = world.step;
        for i in 0..intervals {
            let (own, _) = world.tick(from + i * INTERVAL);
            total += own;
            if world.step != last {
                changes += 1;
                last = world.step;
            }
            paused |= world.step == YieldStep::Pause;
        }
        (total / intervals, changes, paused)
    }

    /// SC-015: beside a steady external user of 1.5% or 2%, own work keeps at least
    /// 90% of capacity minus that user and the margin over 5 minutes, without cap
    /// or pause cycles.
    #[test]
    fn sc_015_a_steady_small_external_user_leaves_own_work_its_share() {
        for (mode, margin) in [
            (ContentionMode::YieldFirst, 15),
            (ContentionMode::Neutral, 5),
            (ContentionMode::Performance, 2),
        ] {
            for user in [CAPACITY * 15 / 1000, CAPACITY * 2 / 100] {
                let mut world = World::new(mode, (64, 8000));
                world.external = (1, user);
                let (average, changes, paused) = steady(&mut world, 0, 600);
                let goal = (CAPACITY - user - CAPACITY * margin / 100) * 9 / 10;
                assert!(average >= goal, "{mode:?} {user}: {average} < {goal}");
                assert!(!paused, "{mode:?} {user}");
                assert!(changes <= 2, "{mode:?} {user}: {changes} step changes");
            }
        }
    }

    /// SC-014: on an idle host own work uses at least 97% of the CPU in Performance,
    /// 94% in Neutral and 84% in Yield First, and memory within 1% of capacity
    /// minus the floor, minus Yield First's margin too.
    #[test]
    fn sc_014_own_work_uses_an_idle_host() {
        use cedegrid::contention::{ModeInput, ResourceKind, own_allowance};
        for (mode, percent) in [
            (ContentionMode::YieldFirst, 84),
            (ContentionMode::Neutral, 94),
            (ContentionMode::Performance, 97),
        ] {
            let mut world = World::new(mode, (64, 8000));
            let (average, _, paused) = steady(&mut world, 0, 600);
            assert!(average * 100 >= CAPACITY * percent, "{mode:?}: {average}");
            assert!(!paused, "{mode:?}");
        }
        // Memory with the default floors: 256 GiB of RAM and 8, 24 and 80 GiB GPUs.
        for mode in [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ] {
            let mut config = Config::default();
            config.contention.default_mode = mode;
            config
                .contention
                .acknowledge_caution
                .insert("performance_mode".into());
            let keep = |capacity: u64| {
                if mode == ContentionMode::YieldFirst {
                    capacity - capacity * 20 / 100
                } else {
                    capacity
                }
            };
            let mut host = snapshot(0);
            host.total_ram_mib = 262_144;
            host.available_ram_mib = Some(262_144);
            let ram = PolicyEngine::new()
                .evaluate(&config, &host, &PolicyInput::default(), 0)
                .unwrap()
                .managed_budget
                .ram_mib;
            let goal = keep(262_144 - config.ram_floor_mib(262_144));
            assert!(
                ram.abs_diff(goal) * 100 <= goal,
                "{mode:?} RAM {ram} {goal}"
            );
            for total in [8192, 24_576, 81_920] {
                let capacity = total - config.vram_floor_mib(total);
                let allowance = own_allowance(
                    &ModeInput {
                        kind: ResourceKind::GpuMemory,
                        mode,
                        capacity,
                        external: 0,
                        own: 0,
                        principals: None,
                        background: 0,
                        saturated: false,
                        psi_only: false,
                        unknown_activity: false,
                        satisfied: vec![],
                        probe: None,
                    },
                    &config.contention.thresholds(ResourceKind::GpuMemory),
                );
                let goal = keep(capacity);
                assert!(
                    allowance.abs_diff(goal) * 100 <= goal,
                    "{mode:?} GPU {total}: {allowance} {goal}"
                );
            }
        }
    }

    /// A small external user on a resource own work keeps saturated is probed: its
    /// demand is recorded as satisfied and own work stays near its target instead
    /// of cycling down to the fair share. When its usage rises, it contends again
    /// (FR-021).
    #[test]
    fn a_satisfied_small_demand_stops_the_probe_and_rising_usage_restarts_it() {
        let mut world = World::new(ContentionMode::Neutral, (64, 8000));
        // Saturated at Neutral's own target: 7480 + 120 is 95%.
        world.config.contention.cpu.saturation_percent = Some(95);
        world.external = (1, 120);
        let (average, changes, paused) = steady(&mut world, 0, 600);
        let goal = (CAPACITY - 120 - CAPACITY * 5 / 100) * 9 / 10;
        assert!(average >= goal, "{average} < {goal}");
        assert!(!paused);
        assert!(changes <= 3, "{changes} step changes");
        // A demanding user gets its fair share within 2 intervals.
        world.external = (8, 8000);
        let mut now = 600 * INTERVAL;
        world.tick(now);
        for _ in 0..4 {
            now += INTERVAL;
            let (own, _) = world.tick(now);
            assert!(own <= 4000 + CAPACITY / 100, "at {now}: {own}");
        }
    }

    /// While a resource is not contended, a held cap rises again toward the mode's
    /// target at the mode's scale-up rate (FR-013, FR-020).
    #[test]
    fn a_held_cap_rises_again_when_external_demand_shrinks() {
        for (mode, target) in [
            (ContentionMode::YieldFirst, 5800),
            (ContentionMode::Neutral, 6600),
            (ContentionMode::Performance, 6840),
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
            // Idle capacity minus the margin: 8000 - 1000 - 15%, 5% or 2% of 8000.
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
            .acknowledge_caution
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
                .cooldown_ms(ContentionMode::Performance, config.monitor.interval_ms);
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

    /// Ticks `world` until `until`, returning own work's usage per interval.
    fn run_until(world: &mut World, now: &mut u64, until: u64) -> Vec<u64> {
        let mut own = vec![];
        while *now < until {
            own.push(world.tick(*now).0);
            *now += INTERVAL;
        }
        own
    }

    /// One steady external user at 12.5% gets the fair share at once and is then
    /// learned satisfied: at most one cap episode in 5 minutes, and own work
    /// averages at least 90% of capacity minus the user minus the margin (FR-021).
    #[test]
    fn a_steady_large_external_user_is_capped_once_then_learned_satisfied() {
        let mut world = World::new(ContentionMode::Neutral, (32, 8000));
        let mut now = 0;
        run_until(&mut world, &mut now, 5000);
        world.external = (1, 1000);
        let (start, mut previous, mut episodes, mut total) = (now, world.step, 0, 0u64);
        while now < start + 300_000 {
            total += world.tick(now).0;
            if previous == YieldStep::None && world.step != YieldStep::None {
                episodes += 1;
            }
            previous = world.step;
            now += INTERVAL;
        }
        let average = total / 600;
        let goal = CAPACITY - 1000 - CAPACITY * 5 / 100;
        assert!(episodes <= 1, "{episodes} cap episodes");
        assert!(average * 10 >= goal * 9, "average {average} of {goal}");
        assert!(!world.drained);
    }

    /// A user learned satisfied keeps its record while it is gone: when it returns
    /// at the same level, own work is not cut to the fair share again (FR-021).
    #[test]
    fn a_returning_satisfied_user_does_not_cut_own_work_again() {
        let mut world = World::new(ContentionMode::Neutral, (32, 8000));
        let mut now = 0;
        run_until(&mut world, &mut now, 5000);
        world.external = (1, 1000);
        run_until(&mut world, &mut now, 65_000);
        world.external = (0, 0);
        // Alone, own work saturating the host is held to its margin.
        let alone = run_until(&mut world, &mut now, 185_000);
        assert!(alone.last().unwrap() + CAPACITY / 100 >= CAPACITY * 95 / 100);
        world.external = (1, 1000);
        let own = run_until(&mut world, &mut now, 215_000);
        let lowest = own.iter().copied().min().unwrap();
        assert!(lowest > 5000, "own work fell to {lowest}: {own:?}");
    }

    /// A satisfied user that grows is real demand: it gets the fair share within
    /// SC-002's two intervals and keeps it while the host stays saturated (FR-021).
    #[test]
    fn a_satisfied_user_that_grows_gets_its_fair_share_within_two_intervals() {
        let mut world = World::new(ContentionMode::Neutral, (32, 8000));
        let mut now = 0;
        run_until(&mut world, &mut now, 5000);
        world.external = (1, 1000);
        run_until(&mut world, &mut now, 65_000);
        world.external = (5, 4800);
        let own = run_until(&mut world, &mut now, 125_000);
        for (interval, own) in own.iter().enumerate().skip(1) {
            assert!(
                *own <= 4000 + CAPACITY / 100,
                "own {own} at interval {interval} after the growth"
            );
        }
        assert!(!world.drained);
    }

    /// An external workload that starts while Performance is still growing gets its
    /// demand, up to its fair share, within the reclaim bound SC-004 uses (SC-012).
    #[test]
    fn an_external_workload_started_mid_ramp_gets_its_demand() {
        let mut world = World::new(ContentionMode::Performance, (8, 0));
        let mut now = 0;
        let mut served_at = None;
        while now < 20_000 {
            // Own work grows by 1000m per interval, as a ramp does.
            world.own.1 = (now / INTERVAL + 1).saturating_mul(1000).min(CAPACITY);
            if now == 1500 {
                world.external = (4, 3000);
            }
            let (_, external) = world.tick(now);
            if now >= 1500 && served_at.is_none() && external >= 3000 {
                served_at = Some(now);
            }
            now += INTERVAL;
        }
        let served_at = served_at.expect("external work never got its demand");
        assert!(served_at - 1500 <= 5000, "after {} ms", served_at - 1500);
        assert!(!world.drained);
    }

    /// An external process that appears and disappears every interval causes no
    /// cap and uncap cycling: steps change no faster than the minimum dwell.
    #[test]
    fn a_process_flapping_every_interval_does_not_cycle_caps() {
        let mut world = World::new(ContentionMode::Performance, (8, 8000));
        let dwell = u64::from(
            world
                .config
                .contention
                .thresholds(cedegrid::contention::ResourceKind::Cpu)
                .min_step_dwell_intervals,
        ) * INTERVAL;
        let mut now = 0;
        let mut changes = vec![];
        let mut previous = world.step;
        while now < 60_000 {
            world.external = if (now / INTERVAL).is_multiple_of(2) {
                (4, 3000)
            } else {
                (0, 0)
            };
            world.tick(now);
            if world.step != previous {
                changes.push(now);
                previous = world.step;
            }
            now += INTERVAL;
        }
        for pair in changes.windows(2) {
            assert!(pair[1] - pair[0] >= dwell, "steps changed at {changes:?}");
        }
        assert!(!world.drained);
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "an_external_workload_started_mid_ramp_gets_its_demand",
            an_external_workload_started_mid_ramp_gets_its_demand,
        ),
        (
            "a_process_flapping_every_interval_does_not_cycle_caps",
            a_process_flapping_every_interval_does_not_cycle_caps,
        ),
        (
            "sc_002_yield_first_restores_its_margin_within_three_intervals_without_draining",
            sc_002_yield_first_restores_its_margin_within_three_intervals_without_draining,
        ),
        (
            "sc_002_contended_cpu_returns_within_two_intervals_in_every_mode",
            sc_002_contended_cpu_returns_within_two_intervals_in_every_mode,
        ),
        (
            "yield_first_pauses_only_when_external_demand_exceeds_its_margin",
            yield_first_pauses_only_when_external_demand_exceeds_its_margin,
        ),
        (
            "yield_first_does_not_pause_own_saturation_for_a_small_external_user",
            yield_first_does_not_pause_own_saturation_for_a_small_external_user,
        ),
        (
            "sc_015_a_steady_small_external_user_leaves_own_work_its_share",
            sc_015_a_steady_small_external_user_leaves_own_work_its_share,
        ),
        (
            "sc_014_own_work_uses_an_idle_host",
            sc_014_own_work_uses_an_idle_host,
        ),
        (
            "a_satisfied_small_demand_stops_the_probe_and_rising_usage_restarts_it",
            a_satisfied_small_demand_stops_the_probe_and_rising_usage_restarts_it,
        ),
        (
            "a_held_cap_rises_again_when_external_demand_shrinks",
            a_held_cap_rises_again_when_external_demand_shrinks,
        ),
        (
            "sc_003_neutral_converges_to_half_with_one_contending_principal",
            sc_003_neutral_converges_to_half_with_one_contending_principal,
        ),
        (
            "sc_004_performance_ramps_on_an_idle_host_and_yields_within_the_reclaim_time",
            sc_004_performance_ramps_on_an_idle_host_and_yields_within_the_reclaim_time,
        ),
        (
            "a_steady_large_external_user_is_capped_once_then_learned_satisfied",
            a_steady_large_external_user_is_capped_once_then_learned_satisfied,
        ),
        (
            "a_returning_satisfied_user_does_not_cut_own_work_again",
            a_returning_satisfied_user_does_not_cut_own_work_again,
        ),
        (
            "a_satisfied_user_that_grows_gets_its_fair_share_within_two_intervals",
            a_satisfied_user_that_grows_gets_its_fair_share_within_two_intervals,
        ),
    ];
}

/// Hints through the policy engine: compliance is judged by observed usage
/// and the hint deadline bounds the wait (FR-037).
/// The Performance ramp (SC-004, FR-034, Session 2026-10-26).
mod ramp {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::agent::{LaunchPacing, launch_slots};
    use cedegrid::contention::{
        ContentionMode, LadderTiming, ResourceKey, ResourceKind, YieldStep,
    };

    const CAPACITY: u64 = 8000;
    const INTERVAL: u64 = 500;
    const HEARTBEAT: u64 = 2000;
    const JOB: u64 = 500;
    const GPU: &str = "GPU-ramp";

    fn mode_config(mode: ContentionMode) -> Config {
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        config
    }

    /// 90% of available capacity minus the mode's margin.
    fn target(config: &Config, mode: ContentionMode, capacity: u64) -> u64 {
        let thresholds = config.contention.thresholds(ResourceKind::Cpu);
        let margin = capacity * u64::from(thresholds.margin_percent(mode)) / 100;
        (capacity - margin) * u64::from(thresholds.performance_ramp_percent) / 100
    }

    /// Own CPU per interval on an idle host that admits 500m jobs on each 2 s
    /// heartbeat, as the agent's launch slots and the coordinator's placement do.
    fn admission_ramp(mode: ContentionMode, until_ms: u64) -> Vec<u64> {
        let config = mode_config(mode);
        let mut engine = PolicyEngine::new();
        let mut jobs: Vec<Allocation> = vec![];
        let (mut admitted, mut last_growth) = (0usize, 0u64);
        let mut own_per_interval = vec![];
        for now in (0..=until_ms).step_by(INTERVAL as usize) {
            for job in &mut jobs {
                job.phase = AllocationPhase::Running;
                job.observed = Some(job.requested.clone());
            }
            let own: u64 = jobs
                .iter()
                .filter_map(|j| j.observed.as_ref())
                .map(|o| o.cpu_millicores)
                .sum();
            own_per_interval.push(own);
            let input = PolicyInput {
                allocations: jobs.clone(),
                ..PolicyInput::default()
            };
            let decision = engine
                .evaluate(&config, &snapshot(own), &input, now)
                .unwrap();
            if now % HEARTBEAT != 0 {
                continue;
            }
            let grow_after = [ResourceKind::Cpu, ResourceKind::Ram]
                .into_iter()
                .map(|kind| {
                    config
                        .contention
                        .thresholds(kind)
                        .cooldown_ms(mode, config.monitor.interval_ms)
                })
                .max()
                .unwrap();
            let fill = decision.resources[&ResourceKey::Cpu].effective_mode
                == Some(ContentionMode::Performance);
            let slots = if decision.expansion_allowed || decision.cpu_ram_expansion_allowed {
                launch_slots(LaunchPacing {
                    max_workers: 64,
                    active: jobs.len(),
                    admitted,
                    may_grow: now - last_growth >= grow_after,
                    fill,
                })
            } else {
                0
            };
            let mut headroom = decision.admission_headroom.cpu_millicores;
            for k in 0..slots {
                if headroom < JOB {
                    break;
                }
                headroom -= JOB;
                let mut job = allocation(
                    &format!("job-{now}-{k}"),
                    AllocationClass::Opportunistic,
                    JOB,
                    None,
                );
                job.phase = AllocationPhase::Pending;
                jobs.push(job);
                if jobs.len() > admitted {
                    admitted = jobs.len();
                    last_growth = now;
                }
            }
        }
        own_per_interval
    }

    fn reached_at(own: &[u64], target: u64) -> Option<u64> {
        own.iter()
            .position(|own| *own >= target)
            .map(|i| i as u64 * INTERVAL)
    }

    /// One heartbeat takes at most 32 new assignments in every mode, and a
    /// 16-task scope still fills in one Performance heartbeat (FR-034).
    #[test]
    fn a_heartbeat_takes_at_most_32_new_assignments() {
        assert_eq!(cedegrid::agent::LAUNCHES_PER_HEARTBEAT, 32);
        for fill in [false, true] {
            for max_workers in [1, 16, 32, 33, 150] {
                for active in 0..=max_workers {
                    for admitted in [0, active, max_workers] {
                        for may_grow in [false, true] {
                            let slots = launch_slots(LaunchPacing {
                                max_workers,
                                active,
                                admitted,
                                may_grow,
                                fill,
                            });
                            assert!(slots <= 32 && slots <= max_workers - active);
                        }
                    }
                }
            }
        }
        let idle = |max_workers| LaunchPacing {
            max_workers,
            active: 0,
            admitted: 0,
            may_grow: true,
            fill: true,
        };
        assert_eq!(launch_slots(idle(16)), 16);
        assert_eq!(launch_slots(idle(150)), 32);
    }

    /// On an idle host, Performance reaches 90% of available CPU minus the margin
    /// within 10 s, the new defaults (SC-004).
    #[test]
    fn performance_reaches_ninety_percent_of_cpu_within_ten_seconds() {
        let config = mode_config(ContentionMode::Performance);
        let thresholds = config.contention.thresholds(ResourceKind::Cpu);
        assert_eq!(
            (
                thresholds.performance_ramp_percent,
                thresholds.performance_ramp_s
            ),
            (90, 10)
        );
        let own = admission_ramp(ContentionMode::Performance, 20_000);
        let reached = reached_at(&own, target(&config, ContentionMode::Performance, CAPACITY))
            .expect("Performance never reached its ramp target");
        assert!(reached <= 10_000, "ramp target reached at {reached} ms");
    }

    /// Yield First and Neutral still add one worker per 30 s and 10 s, and the modes
    /// stay ordered at every interval.
    #[test]
    fn yield_first_and_neutral_ramps_are_unchanged_and_modes_stay_ordered() {
        let until = 60_000;
        let yield_first = admission_ramp(ContentionMode::YieldFirst, until);
        let neutral = admission_ramp(ContentionMode::Neutral, until);
        let performance = admission_ramp(ContentionMode::Performance, until);
        for i in 0..yield_first.len() {
            assert!(
                yield_first[i] <= neutral[i] && neutral[i] <= performance[i],
                "interval {i}: {} {} {}",
                yield_first[i],
                neutral[i],
                performance[i]
            );
        }
        let rises = |own: &[u64]| -> Vec<usize> {
            own.windows(2)
                .enumerate()
                .filter(|(_, pair)| pair[1] > pair[0])
                .map(|(i, _)| i + 1)
                .collect()
        };
        for (own, gap_ms) in [(&yield_first, 30_000), (&neutral, 10_000)] {
            let rises = rises(own);
            for pair in rises.windows(2) {
                assert!(
                    (pair[1] - pair[0]) as u64 * INTERVAL >= gap_ms,
                    "rises at intervals {rises:?}"
                );
            }
            for i in &rises {
                assert_eq!(own[*i] - own[*i - 1], JOB, "one worker per rise");
            }
        }
    }

    /// Performance scales CPU and GPU compute up every monitor interval; RAM and GPU
    /// memory keep 2 s; Yield First and Neutral keep 30 s and 10 s (FR-034).
    #[test]
    fn performance_scales_compute_up_every_interval_and_memory_every_two_seconds() {
        let mut config = mode_config(ContentionMode::Performance);
        // The shared fixture zeroes the legacy GPU memory cooldown.
        config.gpu.scale_up_cooldown_ms = None;
        let interval = config.monitor.interval_ms;
        for kind in ResourceKind::ALL {
            let thresholds = config.contention.thresholds(kind);
            let performance = if kind.is_memory() { 2000 } else { interval };
            for (mode, expected) in [
                (ContentionMode::YieldFirst, 30_000),
                (ContentionMode::Neutral, 10_000),
                (ContentionMode::Performance, performance),
            ] {
                assert_eq!(
                    thresholds.scale_up_interval_ms(mode, interval),
                    expected,
                    "{kind:?} {mode:?}"
                );
                assert_eq!(
                    thresholds.cooldown_ms(mode, interval),
                    expected,
                    "{kind:?} {mode:?}"
                );
                assert_eq!(
                    LadderTiming::new(&thresholds, mode, interval).scale_up_ms,
                    expected,
                    "{kind:?} {mode:?}"
                );
            }
        }
        assert_eq!(
            config.gpu_memory_cooldown_ms(ContentionMode::Performance),
            2000
        );
    }

    fn gpu_snapshot(own: u64, external: u64, sample: u64) -> Snapshot {
        let mut snap = snapshot(0);
        snap.gpu_inventory = CapabilityStatus::Available;
        snap.gpus = vec![GpuSnapshot {
            uuid: GPU.into(),
            total_memory_mib: 16_000,
            used_memory_mib: Some(2000),
            utilization_percent: Some(((own + external) / 10).min(100) as u32),
            external_process_ids: Some(if external > 0 { vec![987] } else { vec![] }),
            external_compute: if external > 0 {
                ComputeActivity::Active
            } else {
                ComputeActivity::Idle
            },
            compute_sample_id: Some(sample),
            observation: None,
            reserved_memory_mib: None,
            free_memory_mib: None,
            unattributed_memory_mib: None,
            external_compute_milli: Some(external),
            own_compute_milli: Some(own),
            compute_amount_unknown: None,
            own_only: false,
        }];
        snap.principals = Some(vec![
            PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                ..PrincipalUsage::default()
            },
            PrincipalUsage {
                uid: Some(2001),
                gpu_compute_milli: BTreeMap::from([(GPU.to_string(), external)]),
                ..PrincipalUsage::default()
            },
        ]);
        snap
    }

    /// Own GPU compute reaches 90% of the device minus the margin within 10 s on an
    /// idle device, and again within 10 s after an external user leaves (SC-004).
    #[test]
    fn performance_reaches_ninety_percent_of_gpu_compute_within_ten_seconds() {
        let mut config = mode_config(ContentionMode::Performance);
        config.gpu.process_sample_max_age_ms = 400;
        config.contention.gpu_compute.quiet_window_intervals = Some(1);
        config
            .contention
            .acknowledge_caution
            .insert("relaxed_detection".into());
        let key = ResourceKey::GpuCompute(GPU.into());
        let target = target(&config, ContentionMode::Performance, 1000);
        let mut engine = PolicyEngine::new();
        let mut cap = 1000u64;
        let mut own_per_interval = vec![];
        for i in 0..80u64 {
            // External compute below the fair share on a saturated device for
            // intervals 20 to 39, and a short burst at 60 and 61.
            let external_demand = if (20..40).contains(&i) || (60..62).contains(&i) {
                300
            } else {
                0
            };
            let external = external_demand.min(1000);
            let own = cap.min(1000 - external);
            own_per_interval.push(own);
            let job = Allocation {
                id: "job".into(),
                class: AllocationClass::Opportunistic,
                phase: AllocationPhase::Running,
                requested: Resources {
                    cpu_millicores: 100,
                    ram_mib: 100,
                    gpu_memory_mib: BTreeMap::from([(GPU.to_string(), 1000)]),
                },
                observed: Some(Resources {
                    cpu_millicores: 100,
                    ram_mib: 100,
                    gpu_memory_mib: BTreeMap::from([(GPU.to_string(), 1000)]),
                }),
                observed_upper: None,
                modes: BTreeMap::new(),
            };
            let input = PolicyInput {
                allocations: vec![job],
                ..PolicyInput::default()
            };
            let decision = engine
                .evaluate(
                    &config,
                    &gpu_snapshot(own, external, i + 1),
                    &input,
                    i * INTERVAL,
                )
                .unwrap();
            cap = decision
                .actions
                .iter()
                .find(|a| a.resource == key)
                .map_or(1000, |a| match a.step {
                    YieldStep::Cap => a.target.unwrap_or(1000),
                    YieldStep::Pause => 0,
                    _ => 1000,
                });
        }
        let idle = reached_at(&own_per_interval[..20], target).expect("idle device never reached");
        assert!(idle <= 10_000, "idle ramp at {idle} ms");
        assert!(
            own_per_interval[21..40].iter().all(|own| *own < 700),
            "the external user never held own work back"
        );
        let after = reached_at(&own_per_interval[40..60], target).expect("never regained");
        assert!(
            after <= 10_000,
            "regained at {after} ms after the external user left"
        );
        // After a fresh cap, the raise waits the minimum step dwell, now longer than
        // the compute scale-up interval, and no more.
        let timing = LadderTiming::new(
            &config.contention.thresholds(ResourceKind::GpuCompute),
            ContentionMode::Performance,
            INTERVAL,
        );
        assert!(timing.scale_up_ms < timing.dwell_ms);
        let burst = reached_at(&own_per_interval[61..], target).expect("burst never regained");
        assert!(
            burst <= timing.dwell_ms,
            "regained {burst} ms after the burst's cap"
        );
    }

    /// RAM keeps its Performance scale-up cooldown: a new floor breach still opens
    /// admission only after 2 s plus the reclaim backoff (FR-034, FR-061).
    #[test]
    fn memory_scale_up_timing_is_unchanged() {
        let config = mode_config(ContentionMode::Performance);
        let thresholds = config.contention.thresholds(ResourceKind::Ram);
        assert_eq!(
            thresholds.cooldown_ms(ContentionMode::Performance, 500),
            2000
        );
        assert_eq!(thresholds.reclaim_backoff_ms, 1000);
        assert_eq!(thresholds.reclaim_backoff_max_ms, 30_000);
        let gpu = config.contention.thresholds(ResourceKind::GpuMemory);
        assert_eq!(gpu.cooldown_ms(ContentionMode::Performance, 500), 2000);
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "performance_reaches_ninety_percent_of_cpu_within_ten_seconds",
            performance_reaches_ninety_percent_of_cpu_within_ten_seconds,
        ),
        (
            "yield_first_and_neutral_ramps_are_unchanged_and_modes_stay_ordered",
            yield_first_and_neutral_ramps_are_unchanged_and_modes_stay_ordered,
        ),
        (
            "performance_reaches_ninety_percent_of_gpu_compute_within_ten_seconds",
            performance_reaches_ninety_percent_of_gpu_compute_within_ten_seconds,
        ),
    ];
}

mod hints {
    use super::*;
    use crate::snapshot::PolicyEngine;
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

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "memory_contention_starts_with_a_hint_and_its_target",
            memory_contention_starts_with_a_hint_and_its_target,
        ),
        (
            "a_compliant_worker_is_not_drained",
            a_compliant_worker_is_not_drained,
        ),
        (
            "a_non_compliant_worker_is_drained_at_the_hint_deadline",
            a_non_compliant_worker_is_drained_at_the_hint_deadline,
        ),
        (
            "a_hint_the_supervisor_cannot_apply_escalates_at_once",
            a_hint_the_supervisor_cannot_apply_escalates_at_once,
        ),
    ];
}

/// T138: a RAM cap step through `memory.high` where a delegated leaf allows it.
mod ram_cap {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldCause, YieldStep};

    type Action = Option<(YieldStep, Option<u64>, YieldCause)>;

    /// The `hints` host: 32000 MiB, a 1000 MiB floor and `external` MiB in
    /// external use beside an allocation holding `held` MiB.
    fn evaluate(
        engine: &mut PolicyEngine,
        config: &Config,
        now: u64,
        (held, external): (u64, u64),
        class: AllocationClass,
        facts: AllocationFacts,
    ) -> Decision {
        let mut snapshot = snapshot(1000);
        snapshot.available_ram_mib = Some(32_000 - external - held);
        let input = PolicyInput {
            allocations: vec![Allocation {
                id: "worker".into(),
                class,
                phase: AllocationPhase::Running,
                requested: Resources {
                    ram_mib: 7900,
                    ..Resources::default()
                },
                observed: Some(Resources {
                    ram_mib: held,
                    ..Resources::default()
                }),
                observed_upper: None,
                modes: Default::default(),
            }],
            facts: BTreeMap::from([("worker".into(), facts)]),
            ..PolicyInput::default()
        };
        engine.evaluate(config, &snapshot, &input, now).unwrap()
    }

    fn capable() -> AllocationFacts {
        AllocationFacts {
            hint_ready: true,
            observes_drain: true,
            ram_cap: true,
            ..AllocationFacts::default()
        }
    }

    fn action(decision: &Decision) -> Action {
        decision
            .actions
            .iter()
            .find(|a| a.allocation_id == "worker" && a.resource == ResourceKey::Ram)
            .map(|a| (a.step, a.target, a.cause))
    }

    fn codes(decision: &Decision) -> Vec<String> {
        decision.resources[&ResourceKey::Ram]
            .reasons
            .iter()
            .map(|r| r.code.clone())
            .collect()
    }

    /// RAM starts at a cap where the backend reports it can apply one (FR-011);
    /// elsewhere the cap is skipped and reported, and the hint comes first.
    #[test]
    fn a_floor_breach_caps_ram_where_the_leaf_allows_it() {
        let mut engine = PolicyEngine::new();
        let decision = evaluate(
            &mut engine,
            &config(),
            0,
            (8000, 23_300),
            AllocationClass::Opportunistic,
            capable(),
        );
        assert_eq!(
            action(&decision),
            Some((YieldStep::Cap, Some(7700), YieldCause::FloorBreach))
        );
        let cap = decision
            .actions
            .iter()
            .find(|a| a.resource == ResourceKey::Ram)
            .unwrap();
        assert_eq!(cap.code, "yield_step_cap");
        let mut engine = PolicyEngine::new();
        let rootless = AllocationFacts {
            ram_cap: false,
            ..capable()
        };
        let decision = evaluate(
            &mut engine,
            &config(),
            0,
            (8000, 23_300),
            AllocationClass::Opportunistic,
            rootless,
        );
        assert_eq!(
            action(&decision),
            Some((YieldStep::Hint, Some(7700), YieldCause::FloorBreach))
        );
        assert!(codes(&decision).contains(&"step_skipped_unavailable".to_string()));
    }

    /// A cap the supervisor holds above its target, or found ineffective, only
    /// stops growth: the ladder goes on to the hint and shows why.
    #[test]
    fn a_cap_that_only_stops_growth_goes_on_to_the_hint() {
        for code in ["ram_cap_growth_stop", "ram_cap_ineffective"] {
            let mut engine = PolicyEngine::new();
            let first = evaluate(
                &mut engine,
                &config(),
                0,
                (8000, 23_300),
                AllocationClass::Opportunistic,
                capable(),
            );
            assert_eq!(action(&first).map(|a| a.0), Some(YieldStep::Cap));
            let reported = AllocationFacts {
                unavailable: BTreeMap::from([(ResourceKey::Ram, [YieldStep::Cap].into())]),
                applied: BTreeMap::from([(ResourceKey::Ram, YieldStep::Cap)]),
                applied_targets: BTreeMap::from([(ResourceKey::Ram, 7990)]),
                step_codes: BTreeMap::from([(ResourceKey::Ram, code.to_string())]),
                ..capable()
            };
            let next = evaluate(
                &mut engine,
                &config(),
                500,
                (8000, 23_300),
                AllocationClass::Opportunistic,
                reported,
            );
            assert_eq!(
                action(&next),
                Some((YieldStep::Hint, Some(7700), YieldCause::FloorBreach)),
                "{code}"
            );
            assert!(codes(&next).contains(&code.to_string()), "{code}");
        }
    }

    /// Guaranteed work is never capped below its request, as on CPU (FR-024).
    #[test]
    fn a_guaranteed_ram_cap_never_goes_below_its_request() {
        let mut engine = PolicyEngine::new();
        let mut caps = vec![];
        for now in (0..=20_000).step_by(500) {
            let decision = evaluate(
                &mut engine,
                &config(),
                now,
                (8000, 23_300),
                AllocationClass::Guaranteed,
                capable(),
            );
            if let Some((YieldStep::Cap, target, _)) = action(&decision) {
                caps.push(target);
            }
        }
        assert!(!caps.is_empty());
        assert!(
            caps.iter().all(|target| target.is_some_and(|t| t >= 7900)),
            "{caps:?}"
        );
    }

    /// Reversal follows the memory rule: room above the floor of at least
    /// max(to_free, noise tolerance) for the calm window, plus the reclaim
    /// backoff, and the step returns to none.
    #[test]
    fn ram_cap_reversal_waits_for_room_the_calm_window_and_backoff() {
        let config = config();
        let mut engine = PolicyEngine::new();
        let mut held = 8000;
        let mut reversed = None;
        let mut capped = false;
        for now in (0..=30_000).step_by(500) {
            // External use falls by 1000 MiB at 3 s, leaving 1000 MiB of room.
            let external = if now < 3000 { 23_300 } else { 22_300 };
            let decision = evaluate(
                &mut engine,
                &config,
                now,
                (held, external),
                AllocationClass::Opportunistic,
                capable(),
            );
            match action(&decision) {
                Some((YieldStep::Cap, Some(target), _)) => {
                    capped = true;
                    // The supervisor applies the cap; the job wants 8000 MiB.
                    held = target.min(8000);
                }
                None if capped && reversed.is_none() => reversed = Some(now),
                _ => {}
            }
        }
        let reversed = reversed.expect("the cap reverses");
        let thresholds = config
            .contention
            .thresholds(cedegrid::contention::ResourceKind::Ram);
        let dwell = u64::from(thresholds.min_step_dwell_intervals) * config.monitor.interval_ms;
        let scale_up =
            thresholds.scale_up_interval_ms(ContentionMode::Neutral, config.monitor.interval_ms);
        let backoff = thresholds.reclaim_backoff_ms;
        assert!(
            reversed >= (3000 + dwell).max(scale_up) + backoff,
            "{reversed}"
        );
    }

    /// Modes stay monotonic: Yield First caps RAM no later and no higher than
    /// Neutral, and Neutral no later and no higher than Performance.
    #[test]
    fn modes_cap_ram_monotonically() {
        let first_cap = |mode: ContentionMode| {
            let mut config = config();
            config.contention.ram.mode = Some(mode);
            config
                .contention
                .acknowledge_caution
                .insert("performance_mode".into());
            let mut engine = PolicyEngine::new();
            for now in (0..=10_000).step_by(500) {
                let decision = evaluate(
                    &mut engine,
                    &config,
                    now,
                    (8000, 23_000),
                    AllocationClass::Opportunistic,
                    capable(),
                );
                if let Some((YieldStep::Cap, Some(target), _)) = action(&decision) {
                    return (now, target);
                }
            }
            (u64::MAX, u64::MAX)
        };
        let yield_first = first_cap(ContentionMode::YieldFirst);
        let neutral = first_cap(ContentionMode::Neutral);
        let performance = first_cap(ContentionMode::Performance);
        assert!(
            yield_first.0 <= neutral.0 && neutral.0 <= performance.0,
            "{yield_first:?} {neutral:?} {performance:?}"
        );
        assert!(
            yield_first.1 <= neutral.1 && neutral.1 <= performance.1,
            "{yield_first:?} {neutral:?} {performance:?}"
        );
        assert!(yield_first.0 < u64::MAX, "Yield First caps here");
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "a_floor_breach_caps_ram_where_the_leaf_allows_it",
            a_floor_breach_caps_ram_where_the_leaf_allows_it,
        ),
        (
            "a_cap_that_only_stops_growth_goes_on_to_the_hint",
            a_cap_that_only_stops_growth_goes_on_to_the_hint,
        ),
        (
            "a_guaranteed_ram_cap_never_goes_below_its_request",
            a_guaranteed_ram_cap_never_goes_below_its_request,
        ),
        (
            "ram_cap_reversal_waits_for_room_the_calm_window_and_backoff",
            ram_cap_reversal_waits_for_room_the_calm_window_and_backoff,
        ),
        ("modes_cap_ram_monotonically", modes_cap_ram_monotonically),
    ];
}

/// T141: external PSI as a second CPU saturation signal (FR-072).
mod psi_saturation {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};
    use cedegrid::kernel::{CpuPressure, CpuPressureStatus, KernelCollector, KernelConfig};

    fn pressure(status: CpuPressureStatus, external: Option<f64>, own: f64) -> CpuPressure {
        CpuPressure {
            status,
            external_some_avg10: external,
            external_cgroups: if external.is_some() { 3 } else { 0 },
            own_some_avg10: Some(own),
            system_some_avg10: Some(own.max(external.unwrap_or(0.0))),
            detail: String::new(),
        }
    }

    fn available(external: f64) -> Option<CpuPressure> {
        Some(pressure(CpuPressureStatus::Available, Some(external), 20.0))
    }

    /// An 8000 m host where a job wants 5500 m beside an external user, so busy
    /// stays below `saturation_percent`.
    struct Host {
        config: Config,
        engine: PolicyEngine,
        cap: Option<u64>,
        wants: u64,
        class: AllocationClass,
    }

    impl Host {
        fn new(mode: ContentionMode) -> Self {
            let mut config = config();
            config.cpu.reserve_physical_cores = 0;
            config.contention.default_mode = mode;
            config
                .contention
                .acknowledge_caution
                .insert("performance_mode".into());
            Self {
                config,
                engine: PolicyEngine::new(),
                cap: None,
                wants: 5500,
                class: AllocationClass::Opportunistic,
            }
        }

        fn wanting(mut self, wants: u64) -> Self {
            self.wants = wants;
            self
        }

        fn guaranteed(mut self) -> Self {
            self.class = AllocationClass::Guaranteed;
            self
        }

        /// What own work runs at under the current step.
        fn own(&self) -> u64 {
            self.cap.unwrap_or(self.wants).min(self.wants)
        }

        fn tick(&mut self, now: u64, external: u64, pressure: Option<CpuPressure>) -> Decision {
            let own = self.own();
            let mut snap = snapshot(own + external);
            snap.principals = Some(vec![
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
            let mut kernel = KernelCollector::new(KernelConfig {
                enabled: false,
                ..KernelConfig::default()
            })
            .sample();
            kernel.runtime_os = "fixture".into();
            kernel.cpu_pressure = pressure;
            snap.kernel = Some(kernel);
            let input = PolicyInput {
                allocations: vec![allocation("own", self.class, 1000, Some(own))],
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
                .evaluate(&self.config, &snap, &input, now)
                .unwrap();
            self.cap = decision
                .actions
                .iter()
                .find(|a| a.allocation_id == "own" && a.resource == ResourceKey::Cpu)
                .map(|a| match a.step {
                    YieldStep::Cap => a.target.unwrap_or(self.wants),
                    YieldStep::Pause => 0,
                    _ => self.wants,
                });
            decision
        }

        fn run(
            &mut self,
            intervals: u64,
            external: u64,
            pressure: Option<CpuPressure>,
        ) -> Vec<Decision> {
            (0..intervals)
                .map(|i| self.tick(i * 500, external, pressure.clone()))
                .collect()
        }
    }

    fn codes(decision: &Decision) -> Vec<String> {
        decision.resources[&ResourceKey::Cpu]
            .reasons
            .iter()
            .map(|r| r.code.clone())
            .collect()
    }

    fn capped(decisions: &[Decision]) -> bool {
        decisions
            .iter()
            .any(|d| d.actions.iter().any(|a| a.resource == ResourceKey::Cpu))
    }

    /// External PSI above the threshold saturates the CPU below `saturation_percent`;
    /// with external demand it contends, and the hold shows as `contention_psi`.
    #[test]
    fn external_psi_saturates_the_cpu_below_saturation_percent() {
        let decisions = Host::new(ContentionMode::Neutral).run(12, 500, available(40.0));
        let last = decisions.last().unwrap();
        assert!(
            codes(last).contains(&"saturated_by_psi".to_string()),
            "{:?}",
            codes(last)
        );
        assert!(capped(&decisions));
        // While the probe runs, the hold shows as `contention_psi`.
        assert!(
            decisions.iter().any(|d| {
                let held = d.resources[&ResourceKey::Cpu].held_back.clone().unwrap();
                held.contention_psi > 0 && held.contention == 0
            }),
            "no PSI-only hold"
        );
        // Without PSI the same host is not saturated.
        let quiet = Host::new(ContentionMode::Neutral).run(12, 500, None);
        assert!(!capped(&quiet));
        assert!(!codes(quiet.last().unwrap()).contains(&"saturated_by_psi".to_string()));
        // Saturated but without external demand: not contended.
        let alone = Host::new(ContentionMode::Neutral).run(12, 0, available(40.0));
        assert!(codes(alone.last().unwrap()).contains(&"saturated_by_psi".to_string()));
        assert!(!capped(&alone));
    }

    /// Own pressure is never saturation evidence.
    #[test]
    fn own_psi_alone_does_not_saturate() {
        let decisions = Host::new(ContentionMode::Neutral).run(
            12,
            500,
            Some(pressure(CpuPressureStatus::Available, Some(2.0), 95.0)),
        );
        assert!(!capped(&decisions));
        assert!(!codes(decisions.last().unwrap()).contains(&"saturated_by_psi".to_string()));
    }

    /// System-wide-only and unknown pressure add no evidence and say why.
    #[test]
    fn system_only_and_unknown_pressure_add_no_evidence() {
        for (status, code) in [
            (CpuPressureStatus::SystemOnly, "psi_system_only"),
            (CpuPressureStatus::Unknown, "psi_unknown"),
        ] {
            let decisions =
                Host::new(ContentionMode::Neutral).run(12, 500, Some(pressure(status, None, 90.0)));
            assert!(!capped(&decisions), "{code}");
            let last = codes(decisions.last().unwrap());
            assert!(last.contains(&code.to_string()), "{last:?}");
            assert!(!last.contains(&"saturated_by_psi".to_string()));
            let held = decisions.last().unwrap().resources[&ResourceKey::Cpu]
                .held_back
                .clone()
                .unwrap();
            assert_eq!(
                (held.contention, held.contention_psi, held.unknown),
                (0, 0, 0),
                "{code}"
            );
        }
    }

    /// PSI that crosses the threshold back and forth never uncaps and recaps: the
    /// ladder's calm window and scale-up interval hold the cap.
    #[test]
    fn pressure_crossing_the_threshold_does_not_cycle() {
        let mut host = Host::new(ContentionMode::Neutral);
        let mut caps = vec![];
        for i in 0..80u64 {
            let external = if i % 2 == 0 { 40.0 } else { 5.0 };
            host.tick(i * 500, 500, available(external));
            caps.push(host.cap);
        }
        let first = caps.iter().position(Option::is_some).expect("a cap");
        assert!(caps[first..].iter().all(Option::is_some), "{caps:?}");
        let changes = caps[first..].windows(2).filter(|w| w[0] != w[1]).count();
        // At most one move per 10 s scale-up interval over the 40 s run.
        assert!(changes <= 4, "{changes} changes: {caps:?}");
    }

    fn modes() -> [Host; 3] {
        [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ]
        .map(Host::new)
    }

    /// PSI that stays high while external usage stays flat ends in a satisfied
    /// record in every mode: each returns to its uncontended target, with Yield
    /// First ≤ Neutral ≤ Performance at every interval.
    #[test]
    fn psi_with_flat_external_usage_returns_every_mode_to_its_target() {
        let mut hosts = modes().map(|host| host.wanting(7200));
        let mut last = vec![];
        for i in 0..400u64 {
            last = hosts
                .iter_mut()
                .map(|host| host.tick(i * 500, 500, available(40.0)))
                .collect();
            let own: Vec<u64> = hosts.iter().map(Host::own).collect();
            assert!(
                own[0] <= own[1] && own[1] <= own[2],
                "interval {i}: {own:?}"
            );
        }
        let targets: Vec<Option<u64>> = last
            .iter()
            .map(|d| d.resources[&ResourceKey::Cpu].target_share)
            .collect();
        // 8000 − 500 external − each mode's margin (15%, 5%, 2%).
        assert_eq!(targets, [Some(6300), Some(7100), Some(7340)]);
        assert_eq!(hosts.map(|host| host.own()), [6300, 7100, 7200]);
    }

    /// Once recorded, a satisfied principal stays satisfied while its usage stays
    /// within the record, so nothing probes or caps again.
    #[test]
    fn a_satisfied_record_holds_while_usage_stays_within_it() {
        let mut host = Host::new(ContentionMode::Neutral).wanting(7200);
        host.run(100, 500, available(40.0));
        let settled = host.own();
        assert_eq!(settled, 7100);
        for i in 100..300u64 {
            let external = if i % 2 == 0 { 500 } else { 560 };
            let decision = host.tick(i * 500, external, available(40.0));
            // Not contended: the target is the uncontended one, capacity less
            // external usage and the 5% margin, and nothing probes again.
            assert_eq!(
                decision.resources[&ResourceKey::Cpu].target_share,
                Some(8000 - external - 400),
                "interval {i}"
            );
            assert!(
                codes(&decision).iter().all(|c| c != "probing_demand"),
                "interval {i}: {:?}",
                codes(&decision)
            );
        }
    }

    /// An external user that grows into the capacity own work frees, as one
    /// sharing own work's cores would, keeps growing while own work steps down to
    /// the fair share, including after it moves from the unattributed principal
    /// to its own.
    #[test]
    fn an_external_user_that_grows_into_freed_capacity_keeps_its_share() {
        let mut host = Host::new(ContentionMode::Neutral);
        let mut external = 2000;
        let (mut lowest_own, mut highest_external) = (u64::MAX, 0);
        for i in 0..120u64 {
            host.tick(i * 500, external, available(40.0));
            external = (2000 + (5500 - host.own())).min(4500);
            lowest_own = lowest_own.min(host.own());
            highest_external = highest_external.max(external);
        }
        assert!(lowest_own <= 4100, "own {lowest_own}");
        assert!(highest_external >= 3400, "external {highest_external}");
    }

    /// An external user whose demand grows while PSI stays high is probed again
    /// and recorded at its new usage; own work keeps the rest.
    #[test]
    fn an_external_user_that_grows_while_psi_is_high_gets_its_share() {
        let mut host = Host::new(ContentionMode::Neutral).wanting(5000);
        host.run(100, 500, available(40.0));
        assert_eq!(host.own(), 5000);
        let grown: Vec<Decision> = (100..300u64)
            .map(|i| host.tick(i * 500, 2500, available(40.0)))
            .collect();
        assert!(
            grown
                .iter()
                .any(|d| codes(d).contains(&"probing_demand".to_string())),
            "the grown demand is probed"
        );
        let last = grown.last().unwrap();
        // Recorded as satisfied, it no longer contends: own work's target is
        // capacity less its usage and the 5% margin.
        assert!(codes(last).contains(&"satisfied_demand".to_string()));
        assert_eq!(
            last.resources[&ResourceKey::Cpu].target_share,
            Some(8000 - 2500 - 400)
        );
        assert_eq!(host.own(), 5000);
    }

    /// PSI never pauses Guaranteed work, never caps its CPU below the request, and
    /// never changes what memory holds back for the floors.
    #[test]
    fn floors_hold_and_guaranteed_is_never_paused_under_psi() {
        let ram = |pressure: Option<CpuPressure>| {
            let mut host = Host::new(ContentionMode::Neutral)
                .wanting(7200)
                .guaranteed();
            let decisions = host.run(60, 500, pressure);
            for d in &decisions {
                for a in &d.actions {
                    assert_ne!(a.step, YieldStep::Pause, "{a:?}");
                    if a.resource == ResourceKey::Cpu && a.step == YieldStep::Cap {
                        assert!(a.target.unwrap_or(0) >= 1000, "{a:?}");
                    }
                }
            }
            let last = decisions.last().unwrap();
            (
                last.resources[&ResourceKey::Ram].held_back.clone(),
                last.actions
                    .iter()
                    .filter(|a| a.resource == ResourceKey::Ram)
                    .count(),
            )
        };
        let without = ram(None);
        for pressure in [
            available(40.0),
            Some(pressure(CpuPressureStatus::SystemOnly, None, 90.0)),
            Some(pressure(CpuPressureStatus::Unknown, None, 90.0)),
        ] {
            assert_eq!(ram(pressure), without);
        }
    }

    /// The same evidence in every mode, and the existing responses keep modes
    /// monotonic.
    #[test]
    fn modes_respond_to_psi_monotonically() {
        let own = |mode| {
            let mut host = Host::new(mode);
            host.run(200, 500, available(40.0));
            host.own()
        };
        let (yield_first, neutral, performance) = (
            own(ContentionMode::YieldFirst),
            own(ContentionMode::Neutral),
            own(ContentionMode::Performance),
        );
        assert!(
            yield_first <= neutral && neutral <= performance,
            "{yield_first} {neutral} {performance}"
        );
    }

    /// Doctor shows external, own and system pressure under cpu, and says that a
    /// cgroup shared with own work never counts (T141d).
    #[test]
    fn doctor_shows_the_pressure_line() {
        let config = Host::new(ContentionMode::Neutral).config;
        let mut snap = snapshot(1000);
        let decision = PolicyEngine::new()
            .evaluate(&config, &snap, &PolicyInput::default(), 0)
            .unwrap();
        let cpu_block = |snap: &Snapshot| {
            let text = cedegrid::explain::doctor_text(&config, snap, &decision, &[]);
            text.split("\n\n")
                .find(|block| block.starts_with("cpu:"))
                .unwrap_or_else(|| panic!("no cpu block in:\n{text}"))
                .to_string()
        };
        assert!(!cpu_block(&snap).contains("psi:"));
        let mut kernel = KernelCollector::new(KernelConfig {
            enabled: false,
            ..KernelConfig::default()
        })
        .sample();
        for (reading, line) in [
            (
                available(35.0),
                "psi: external 35.0% over 3 cgroups (saturates above 10%), own 20.0%, \
                 system 35.0%; a cgroup shared with own work never counts",
            ),
            (
                Some(pressure(CpuPressureStatus::SystemOnly, None, 15.0)),
                "psi: system-wide only, no external evidence; own 15.0%, system 15.0%",
            ),
            (
                Some(pressure(CpuPressureStatus::Unknown, None, 0.0)),
                "psi: unknown, no evidence; own 0.0%, system 0.0%",
            ),
        ] {
            kernel.cpu_pressure = reading;
            snap.kernel = Some(kernel.clone());
            let block = cpu_block(&snap);
            assert!(block.contains(line), "{block}");
        }
        // observe carries the same reading in the snapshot.
        let observed = cedegrid::explain::observation(&snap, &decision, None);
        assert_eq!(
            observed["snapshot"]["kernel"]["cpu_pressure"]["status"],
            "unknown"
        );
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "external_psi_saturates_the_cpu_below_saturation_percent",
            external_psi_saturates_the_cpu_below_saturation_percent,
        ),
        (
            "own_psi_alone_does_not_saturate",
            own_psi_alone_does_not_saturate,
        ),
        (
            "system_only_and_unknown_pressure_add_no_evidence",
            system_only_and_unknown_pressure_add_no_evidence,
        ),
        (
            "pressure_crossing_the_threshold_does_not_cycle",
            pressure_crossing_the_threshold_does_not_cycle,
        ),
        (
            "psi_with_flat_external_usage_returns_every_mode_to_its_target",
            psi_with_flat_external_usage_returns_every_mode_to_its_target,
        ),
        (
            "a_satisfied_record_holds_while_usage_stays_within_it",
            a_satisfied_record_holds_while_usage_stays_within_it,
        ),
        (
            "an_external_user_that_grows_into_freed_capacity_keeps_its_share",
            an_external_user_that_grows_into_freed_capacity_keeps_its_share,
        ),
        (
            "an_external_user_that_grows_while_psi_is_high_gets_its_share",
            an_external_user_that_grows_while_psi_is_high_gets_its_share,
        ),
        (
            "floors_hold_and_guaranteed_is_never_paused_under_psi",
            floors_hold_and_guaranteed_is_never_paused_under_psi,
        ),
        (
            "modes_respond_to_psi_monotonically",
            modes_respond_to_psi_monotonically,
        ),
        (
            "doctor_shows_the_pressure_line",
            doctor_shows_the_pressure_line,
        ),
    ];
}

/// Satisfied records are kept without expiry, at most 64 per resource key: past
/// the bound, the records whose principals were seen longest ago go first, and a
/// principal never seen counts as the oldest (FR-021).
#[test]
fn satisfied_records_keep_the_64_most_recently_seen_principals() {
    use cedegrid::contention::{SATISFIED_RECORD_LIMIT, bound_satisfied};
    assert_eq!(SATISFIED_RECORD_LIMIT, 64);
    let mut records: Vec<(Option<u32>, u64)> = (0..70).map(|uid| (Some(uid), 100)).collect();
    records.push((None, 50));
    // UIDs 0-69 were last seen at time uid; the unattributed principal never.
    let last_seen: BTreeMap<Option<u32>, u64> =
        (0..70).map(|uid| (Some(uid), u64::from(uid))).collect();
    bound_satisfied(&mut records, &last_seen);
    assert_eq!(records.len(), 64);
    let kept: Vec<Option<u32>> = records.iter().map(|(uid, _)| *uid).collect();
    assert_eq!(kept, (6..70).map(Some).collect::<Vec<_>>());
    // Within the bound nothing is dropped.
    let mut few = vec![(Some(1), 10), (None, 20)];
    bound_satisfied(&mut few, &BTreeMap::new());
    assert_eq!(few.len(), 2);
}

/// The scripted runs above as a decision snapshot (tests/support/snapshot.rs).
#[test]
#[ignore = "decision snapshot: needs CEDEGRID_DECISION_SNAPSHOT"]
fn decision_snapshot() {
    let mut scenarios = Vec::new();
    scenarios.extend(snapshot::scripted("scenarios", scenarios::SNAPSHOT));
    scenarios.extend(snapshot::scripted("ramp", ramp::SNAPSHOT));
    scenarios.extend(snapshot::scripted("hints", hints::SNAPSHOT));
    scenarios.extend(snapshot::scripted("ram_cap", ram_cap::SNAPSHOT));
    scenarios.extend(snapshot::scripted(
        "psi_saturation",
        psi_saturation::SNAPSHOT,
    ));
    snapshot::check("contention", scenarios);
}

/// Report 021's T178 contender, item 023 Part 3: a test, not a policy change. Own
/// work is 16 tasks of 250 m on 4000 m, measured exactly; a contender that wants
/// one core starts once own work has ramped. The simulated host splits CPU by
/// threads, weighted max-min, and honours the caps the policy applies.
mod t178_contender {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const CAPACITY: u64 = 4000;
    const INTERVAL: u64 = 500;
    const TASKS: usize = 16;
    const TASK: u64 = 250;
    const CONTENDER: u64 = 1000;

    /// One thread per claim, weighted max-min over CAPACITY.
    fn schedule(demands: &[u64]) -> Vec<u64> {
        let mut shares = vec![0u64; demands.len()];
        let mut open: Vec<usize> = (0..demands.len()).filter(|i| demands[*i] > 0).collect();
        let mut remaining = CAPACITY;
        while !open.is_empty() && remaining > 0 {
            let level = remaining / open.len() as u64;
            let (done, rest): (Vec<usize>, Vec<usize>) = open
                .iter()
                .partition(|i| demands[**i] - shares[**i] <= level);
            if done.is_empty() {
                for i in &open {
                    shares[*i] += level;
                }
                break;
            }
            for i in done {
                remaining -= demands[i] - shares[i];
                shares[i] = demands[i];
            }
            open = rest;
        }
        shares
    }

    /// The contender's CPU in each interval after it starts, and own work's.
    pub fn run(mode: ContentionMode, same_account: bool) -> Vec<(u64, u64)> {
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        let mut engine = PolicyEngine::new();
        let mut caps: Vec<(YieldStep, Option<u64>)> = vec![(YieldStep::None, None); TASKS];
        let mut served = Vec::new();
        for tick in 0..120u64 {
            let now = tick * INTERVAL;
            let started = now >= 10_000;
            let mut demands: Vec<u64> = caps
                .iter()
                .map(|(step, target)| match step {
                    YieldStep::Pause => 0,
                    YieldStep::Cap => target.unwrap_or(TASK).min(TASK),
                    _ => TASK,
                })
                .collect();
            demands.push(if started { CONTENDER } else { 0 });
            let usage = schedule(&demands);
            let contender = usage[TASKS];
            let own: u64 = usage[..TASKS].iter().sum();
            let mut snap = snapshot(own + contender);
            snap.cpu_capacity_millicores = CAPACITY;
            snap.physical_cores = Some(4);
            let mut principals = vec![PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                cpu_millicores: if same_account { contender } else { 0 },
                ..PrincipalUsage::default()
            }];
            if !same_account {
                principals.push(PrincipalUsage {
                    uid: Some(2000),
                    cpu_millicores: contender,
                    ..PrincipalUsage::default()
                });
            }
            snap.principals = Some(principals);
            let ids: Vec<String> = (0..TASKS).map(|i| format!("t{i}")).collect();
            let input = PolicyInput {
                allocations: ids
                    .iter()
                    .zip(&usage)
                    .map(|(id, used)| {
                        allocation(id, AllocationClass::Opportunistic, TASK, Some(*used))
                    })
                    .collect(),
                facts: ids
                    .iter()
                    .map(|id| {
                        (
                            id.clone(),
                            AllocationFacts {
                                unavailable: BTreeMap::from([(
                                    ResourceKey::Cpu,
                                    [YieldStep::LowerShare].into(),
                                )]),
                                ..AllocationFacts::default()
                            },
                        )
                    })
                    .collect(),
                ..PolicyInput::default()
            };
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            for (i, id) in ids.iter().enumerate() {
                caps[i] = decision
                    .actions
                    .iter()
                    .find(|a| &a.allocation_id == id && a.resource == ResourceKey::Cpu)
                    .map_or((YieldStep::None, None), |a| (a.step, a.target));
            }
            if started {
                served.push((contender, own));
            }
        }
        served
    }

    /// The first interval, counted from the contender's start, in which it gets
    /// its full core, and whether it keeps it from then on.
    fn served_from(mode: ContentionMode, same_account: bool) -> usize {
        let served = run(mode, same_account);
        let first = served
            .iter()
            .position(|(contender, _)| *contender >= CONTENDER)
            .expect("the contender never got its core");
        assert!(
            served[first..]
                .iter()
                .all(|(contender, _)| *contender >= CONTENDER),
            "{mode:?} same_account={same_account}: {served:?}"
        );
        first
    }

    /// With exact own measurement, the contender reaches its demand in every mode,
    /// as another user and under the account that runs CedeGrid, so report 021's
    /// 0.72 core is not explained by the account the contender ran under.
    #[test]
    fn the_contender_reaches_its_demand_with_exact_own_measurement() {
        use ContentionMode::*;
        // Performance (SC-002, SC-012): ⌈(own − fair share) ÷ margin⌉ intervals,
        // ⌈(3755 − 2000) ÷ 80⌉ = 22 here. The account makes no difference.
        assert_eq!(
            served_from(Performance, true),
            served_from(Performance, false)
        );
        assert!(served_from(Performance, false) <= 22);
        // Another user in Neutral and Yield First: within 2 intervals (SC-002).
        assert!(served_from(Neutral, false) <= 2);
        assert!(served_from(YieldFirst, false) <= 2);
        assert!(served_from(YieldFirst, true) <= 2);
        // The account's own unmanaged program is part of the owner's principal, so
        // in Neutral it never contends for a fair share: it is served only as own
        // work's margin frees capacity, after 5 intervals here (report 023,
        // Decisions needed).
        let owner = served_from(Neutral, true);
        assert!((3..=10).contains(&owner), "{owner}");
    }
}
