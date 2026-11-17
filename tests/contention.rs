//! Contention-mode and yield-ladder properties. Builders mirror tests/policy.rs.
#[allow(dead_code)]
mod snapshot {
    include!("support/snapshot.rs");
}
use cedegrid::{config::Config, execution_model::AllocationClass, model::*, policy::PolicyEngine};
use proptest::prelude::*;
use std::collections::BTreeMap;

/// The node in Full, with `cpu.max` authorized, so the CPU caps these runs model apply; whole-task
/// holds without a cap are tested in tests/policy.rs (FR-089).
pub fn config() -> Config {
    let mut result = Config::default();
    result.cgroup.enabled = true;
    result.cgroup.delegated_root = Some("/sys/fs/cgroup/cedegrid-test".into());
    result.cgroup.authorized_controls = ["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"]
        .map(String::from)
        .to_vec();
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
        managed_cpu_window_lower_millicores: None,
        cedegrid_cpu_lower_millicores: None,
        cedegrid_cpu_window_lower_millicores: None,
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
        observed_window_cpu: None,
        usage_unknown: None,
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
            memory_usage: None,
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

    /// Where no rate cap applies, a pause held for external growth ends in the interval that
    /// contention has ended, not after the mode's scale-up interval (FR-034, FR-089).
    #[test]
    fn without_a_cap_a_pause_ends_in_one_move_when_contention_ends() {
        let mut whole = input(ResourceKind::Cpu, 1000, 4000);
        whole.unavailable = [YieldStep::LowerShare, YieldStep::Cap, YieldStep::Hint].into();
        let paused = plan_step(
            None,
            &whole,
            &timing(ResourceKind::Cpu, ContentionMode::YieldFirst),
            0,
        )
        .state
        .unwrap();
        assert_eq!(paused.step, YieldStep::Pause);
        let mut calm = whole.clone();
        calm.excess = 0;
        calm.external = 0;
        calm.target = Some(4000);
        let mut returning = timing(ResourceKind::Cpu, ContentionMode::YieldFirst);
        returning.returning = true;
        let next = plan_step(Some(&paused), &calm, &returning, 1000).state;
        assert!(
            next.as_ref().is_none_or(|s| s.step == YieldStep::None),
            "{next:?}"
        );
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
            pending: false,
            asked_target: None,
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
            pending: false,
            asked_target: None,
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
            pending: false,
            asked_target: None,
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
                pending: false,
                asked_target: None,
            };
            let (after, changes) = run(Some(state), &cpu, 1..100, |_| {
                input(ResourceKind::Cpu, 0, 0)
            });
            assert!(changes.is_empty());
            assert_eq!(after.unwrap().step, step);
        }
    }

    /// At its deadline a memory hint whose reading is at or below what it was asked is entered
    /// again with the current target and a new deadline; one above it, or one asked for a
    /// target it cannot reach, goes on to the drain (FR-037).
    #[test]
    fn a_complied_memory_hint_is_renewed_at_its_deadline() {
        let ram = timing(ResourceKind::Ram, ContentionMode::Neutral);
        let deadline = ram.hint_deadline_ms;
        let ask = |target: u64, usage: u64| LadderInput {
            unavailable: [YieldStep::Cap].into(),
            cause: YieldCause::FloorBreach,
            target: Some(target),
            memory_usage: Some(usage),
            ..input(ResourceKind::Ram, 300, 0)
        };
        let step = |state: &StepState, input: &LadderInput, now: u64| {
            plan_step(Some(state), input, &ram, now).state.unwrap()
        };
        let hinted = plan_step(None, &ask(6_700, 7_000), &ram, 0).state.unwrap();
        assert_eq!(
            (hinted.step, hinted.target, hinted.asked_target),
            (YieldStep::Hint, Some(6_700), Some(6_700))
        );
        // The task freed what it was asked, and another task's growth left 300 MiB more.
        let renewed = step(&hinted, &ask(6_400, 6_700), deadline);
        assert_eq!(
            (renewed.step, renewed.target, renewed.asked_target),
            (YieldStep::Hint, Some(6_400), Some(6_400))
        );
        assert_eq!(renewed.entered_at_ms, deadline);
        assert_eq!(
            step(&renewed, &ask(6_400, 6_700), deadline + 500).step,
            YieldStep::Hint
        );
        assert_eq!(
            step(&renewed, &ask(6_400, 6_700), 2 * deadline).step,
            YieldStep::Drain
        );
        // One MiB above both targets has not complied.
        assert_eq!(
            step(&hinted, &ask(6_400, 6_701), deadline).step,
            YieldStep::Drain
        );
        // A target of 0 for a task that holds memory it cannot free.
        let empty = plan_step(None, &ask(0, 100), &ram, 0).state.unwrap();
        assert_eq!(step(&empty, &ask(0, 100), deadline).step, YieldStep::Drain);
        // State written before `asked_target` is judged against the current target alone.
        let older = StepState {
            asked_target: None,
            ..hinted.clone()
        };
        assert_eq!(
            step(&older, &ask(6_400, 6_700), deadline).step,
            YieldStep::Drain
        );
        assert_eq!(
            step(&older, &ask(6_400, 6_400), deadline).step,
            YieldStep::Hint
        );
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
            fair_share_limit: None,
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

    /// Under saturation a satisfied principal using less than its recorded demand by more than
    /// the contention floor counts at that demand in own work's allowance; without saturation,
    /// or within the floor of its demand, at its usage (FR-021, SC-002).
    #[test]
    fn a_satisfied_principal_below_its_demand_under_saturation_counts_at_its_demand() {
        let margin = 400;
        let mut view = input(ResourceKind::Cpu, ContentionMode::Neutral, 8000, 235);
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 235)]);
        view.satisfied = vec![(Some(2000), 1080)];
        assert_eq!(allowance(&view), 8000 - 235 - margin);
        view.saturated = true;
        assert_eq!(allowance(&view), 8000 - 1080 - margin);
        view.external = 1040;
        view.principals = Some(vec![principal(Some(1000), 0), principal(Some(2000), 1040)]);
        assert_eq!(allowance(&view), 8000 - 1040 - margin);
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

    /// These tests test the floors first, which hold in Neutral; Performance keeps in-share
    /// memory (FR-023, tests/policy.rs `floors_by_mode`).
    fn config() -> Config {
        let mut result = super::config();
        result.contention.default_mode = ContentionMode::Neutral;
        result
    }

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
            observed_window_cpu: None,
            usage_unknown: None,
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
        // Performance takes no CPU step for others (SC-002, FR-022).
        for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
            let mut world = World::new(mode, (64, 8000));
            let mut now = 0;
            while now < 5000 {
                world.tick(now);
                now += INTERVAL;
            }
            world.external = (5, 5000);
            world.tick(now);
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
                    ContentionMode::Performance => unreachable!("not in this loop"),
                };
                assert!(returned, "{mode:?} at {now}: own {own} external {external}");
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
                        fair_share_limit: None,
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

        // Running own work is not stepped for external work that starts later, and nothing
        // is drained: what it gets is the kernel's split beside the same work (SC-004, FR-022).
        let mut world = World::new(ContentionMode::Performance, (8, 8000));
        let mut now = 0;
        let before = run_until(&mut world, &mut now, 5000);
        world.external = (4, 3000);
        let beside = run_until(&mut world, &mut now, 25_000);
        assert!(
            beside
                .iter()
                .all(|own| *own >= *before.last().unwrap() * 8 / 12),
            "own work was stepped: {beside:?}"
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
            background_process_ids: vec![],
            compute_sampling: None,
            time_sliced: None,
            compute_contexts: None,
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
        config.gpu.process_sample_max_age_ms = Some(600);
        config.contention.gpu_compute.quiet_window_intervals = Some(2);
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
                observed_window_cpu: None,
                usage_unknown: None,
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
        // Performance takes no GPU compute step for the external user, after it leaves, or
        // after the burst: the driver splits the device as beside the same work started by
        // hand (FR-022).
        for (i, own) in own_per_interval.iter().enumerate().skip(20) {
            let external = if (20..40).contains(&i) || (60..62).contains(&i) {
                300
            } else {
                0
            };
            assert_eq!(*own, 1000 - external, "Performance stepped own work at {i}");
        }
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
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldCause, YieldStep};

    /// These tests test the floors first, which hold in Neutral; Performance keeps in-share
    /// memory (FR-023, tests/policy.rs `floors_by_mode`).
    fn config() -> Config {
        let mut result = super::config();
        result.contention.default_mode = ContentionMode::Neutral;
        result
    }

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
                    observed_window_cpu: None,
                    usage_unknown: None,
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

    /// These tests test the floors first, which hold in Neutral; Performance keeps in-share
    /// memory (FR-023, tests/policy.rs `floors_by_mode`).
    fn config() -> Config {
        let mut result = super::config();
        result.contention.default_mode = ContentionMode::Neutral;
        result
    }

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
                observed_window_cpu: None,
                usage_unknown: None,
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
        // Neutral's scale-up interval bounds the reversal below.
        let mut config = config();
        config.contention.ram.mode = Some(ContentionMode::Neutral);
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
        /// Whether own usage is observed; without it CPU holds.
        observed: bool,
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
                observed: true,
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
                allocations: vec![allocation(
                    "own",
                    self.class,
                    1000,
                    self.observed.then_some(own),
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

    /// A principal recorded as satisfied that now uses clearly less, beside own work at its
    /// Performance target: the saturation own work causes does not make the target cycle.
    #[test]
    fn own_caused_saturation_beside_a_lower_satisfied_principal_keeps_the_target_steady() {
        let mut host = Host::new(ContentionMode::Performance).wanting(5000);
        let learned = host.run(200, 2000, available(40.0));
        assert!(
            codes(learned.last().unwrap()).contains(&"satisfied_demand".to_string()),
            "{:?}",
            codes(learned.last().unwrap())
        );
        // Own work now wants all the external user leaves.
        host.wants = 7500;
        let calm = Some(pressure(CpuPressureStatus::Available, Some(0.0), 0.0));
        let ticks: Vec<(Option<u64>, u64, Vec<String>)> = (200..400u64)
            .map(|i| {
                let decision = host.tick(i * 500, 500, calm.clone());
                (
                    decision.resources[&ResourceKey::Cpu].target_share,
                    host.own(),
                    codes(&decision),
                )
            })
            .collect();
        let late = &ticks[100..];
        let changes = late
            .windows(2)
            .filter(|pair| pair[0].0 != pair[1].0)
            .count();
        assert!(changes <= 1, "{:?}", &late[..late.len().min(12)]);
    }

    /// One interval in which own usage is unknown holds CPU but keeps the satisfied record:
    /// on return nothing probes again and the target stays the uncontended one
    /// (FR-021).
    #[test]
    fn a_held_interval_keeps_the_satisfied_record() {
        let mut host = Host::new(ContentionMode::Neutral).wanting(7200);
        host.run(100, 500, available(40.0));
        assert_eq!(host.own(), 7100);
        host.observed = false;
        let held = host.tick(100 * 500, 500, available(40.0));
        assert!(codes(&held).contains(&"allocation_unknown_hold".to_string()));
        host.observed = true;
        for i in 101..200u64 {
            let decision = host.tick(i * 500, 500, available(40.0));
            assert!(
                codes(&decision).iter().all(|c| c != "probing_demand"),
                "interval {i}: {:?}",
                codes(&decision)
            );
            assert_eq!(
                decision.resources[&ResourceKey::Cpu].target_share,
                Some(8000 - 500 - 400),
                "interval {i}"
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

/// A measured contender: a test, not a policy change. Own
/// work is 16 tasks of 250 m on 4000 m, measured exactly; a contender that wants
/// one core starts once own work has ramped. The simulated host splits CPU by
/// threads, weighted max-min, and honours the caps the policy applies.
mod t178_contender {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, ResourceKind, YieldCause, YieldStep};

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

    /// A measured return (FR-013): in Performance, with one task waiting on I/O now and then
    /// while own work's total stays within the contention floor of its target, no held
    /// `own_over_target` cap is raised or removed; own work stays at its target within the
    /// contention floor and the contender keeps its core.
    #[test]
    fn own_over_target_caps_hold_while_one_task_dips() {
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = ContentionMode::Neutral;
        // The contention floor: principal_min_percent of capacity.
        let floor = CAPACITY
            * u64::from(
                config
                    .contention
                    .thresholds(ResourceKind::Cpu)
                    .principal_min_percent,
            )
            / 100;
        let mut engine = PolicyEngine::new();
        let mut caps: Vec<(YieldStep, Option<u64>)> = vec![(YieldStep::None, None); TASKS];
        let ids: Vec<String> = (0..TASKS).map(|i| format!("t{i}")).collect();
        let mut last: BTreeMap<String, Option<u64>> = BTreeMap::new();
        for tick in 0..120u64 {
            let now = tick * INTERVAL;
            let mut demands: Vec<u64> = caps
                .iter()
                .map(|(step, target)| match step {
                    YieldStep::Pause => 0,
                    YieldStep::Cap => target.unwrap_or(TASK).min(TASK),
                    _ => TASK,
                })
                .collect();
            // t0 waits on I/O for two intervals in every ten, using 30 less.
            if tick % 10 == 4 || tick % 10 == 5 {
                demands[0] = demands[0].saturating_sub(30);
            }
            // The contender starts once own work holds the host, as in run 5.
            demands.push(if now >= 10_000 { CONTENDER } else { 0 });
            let usage = schedule(&demands);
            let contender = usage[TASKS];
            let own: u64 = usage[..TASKS].iter().sum();
            let mut snap = snapshot(own + contender);
            snap.cpu_capacity_millicores = CAPACITY;
            snap.physical_cores = Some(4);
            snap.principals = Some(vec![
                PrincipalUsage {
                    uid: Some(1000),
                    is_owner: true,
                    ..PrincipalUsage::default()
                },
                PrincipalUsage {
                    uid: Some(2000),
                    cpu_millicores: contender,
                    ..PrincipalUsage::default()
                },
            ]);
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
            let target = decision.managed_budget.cpu_millicores;
            for (i, id) in ids.iter().enumerate() {
                let action = decision
                    .actions
                    .iter()
                    .find(|a| &a.allocation_id == id && a.resource == ResourceKey::Cpu);
                caps[i] = action.map_or((YieldStep::None, None), |a| (a.step, a.target));
                // Settled after 20 s: no held own-over-target cap is relaxed while own
                // work is not below its target by the contention floor.
                let held = action
                    .filter(|a| a.step == YieldStep::Cap && a.cause == YieldCause::OwnOverTarget);
                if now >= 20_000
                    && own + floor > target
                    && let Some(Some(before)) = last.get(id)
                {
                    let after = held.and_then(|a| a.target);
                    assert!(
                        after.is_some_and(|after| after <= *before),
                        "at {now} {id}: {before} relaxed to {after:?} with own {own} beside {target}"
                    );
                }
                last.insert(id.clone(), held.and_then(|a| a.target));
            }
            if now >= 20_000 {
                assert!(own <= target + floor, "at {now}: own {own} beside {target}");
                assert!(
                    own + 2 * floor >= target,
                    "at {now}: own {own} lost beside {target}"
                );
                assert!(contender >= CONTENDER, "at {now}: contender {contender}");
            }
        }
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
    /// as another user and under the account that runs CedeGrid, so a measured
    /// 0.72 core is not explained by the account the contender ran under.
    #[test]
    fn the_contender_reaches_its_demand_with_exact_own_measurement() {
        use ContentionMode::*;
        // Another user in Neutral and Yield First: within 2 intervals (SC-002).
        assert!(served_from(Neutral, false) <= 2);
        assert!(served_from(YieldFirst, false) <= 2);
        assert!(served_from(YieldFirst, true) <= 2);
        // The account's own unmanaged program is part of the owner's principal, so
        // in Neutral it never contends for a fair share: it is served only as own
        // work's margin frees capacity, after 5 intervals here.
        let owner = served_from(Neutral, true);
        assert!((3..=10).contains(&owner), "{owner}");
    }
}

/// Held caps rise within the spare, and tasks that make up lost CPU do not cycle
/// once caps hold from their first period (FR-013, SC-015).
mod cap_rises {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const CAPACITY: u64 = 4000;
    const INTERVAL: u64 = 500;
    const TASKS: usize = 13;
    const TASK: u64 = 250;

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

    struct Interval {
        own: u64,
        decision: Decision,
        usage: Vec<u64>,
    }

    /// 13 tasks of 250m in Yield First on 4 CPUs beside 190-350m of external noise
    /// and, from `spike.0` to `spike.1`, 800m more. Each task makes up CPU lost to a
    /// cap or pause, up to a full core; a cap holds from its first period; the
    /// policy reads each interval's usage one interval late, as a reading lags caps.
    fn run(ticks: u64, spike: (u64, u64)) -> Vec<Interval> {
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = ContentionMode::YieldFirst;
        let mut engine = PolicyEngine::new();
        let ids: Vec<String> = (0..TASKS).map(|i| format!("t{i}")).collect();
        let mut caps: Vec<(YieldStep, Option<u64>)> = vec![(YieldStep::None, None); TASKS];
        let mut debt = [0u64; TASKS];
        let mut last = vec![TASK; TASKS];
        let mut seed: u64 = 7;
        let mut out = Vec::new();
        for tick in 0..ticks {
            let now = tick * INTERVAL;
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let mut external = 190 + (seed >> 33) % 161;
            if (spike.0..spike.1).contains(&now) {
                external += 800;
            }
            let mut demands: Vec<u64> = (0..TASKS)
                .map(|i| {
                    let want = (TASK + debt[i] * 1000 / INTERVAL).min(1000);
                    match caps[i] {
                        (YieldStep::Pause, _) => 0,
                        (YieldStep::Cap, target) => target.unwrap_or(want).min(want),
                        _ => want,
                    }
                })
                .collect();
            demands.push(external);
            let fresh = schedule(&demands);
            let seen = last.clone();
            let own_seen: u64 = seen.iter().sum();
            let external_seen = fresh[TASKS].min(CAPACITY - own_seen);
            for i in 0..TASKS {
                debt[i] =
                    (debt[i] * 1000 + TASK * INTERVAL).saturating_sub(fresh[i] * INTERVAL) / 1000;
            }
            last = fresh[..TASKS].to_vec();
            let mut snap = snapshot(own_seen + external_seen);
            snap.cpu_capacity_millicores = CAPACITY;
            snap.physical_cores = Some(4);
            let input = PolicyInput {
                allocations: ids
                    .iter()
                    .zip(&seen)
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
            out.push(Interval {
                own: own_seen,
                decision,
                usage: seen,
            });
        }
        out
    }

    fn cpu_targets(decision: &Decision) -> BTreeMap<String, Option<u64>> {
        decision
            .actions
            .iter()
            .filter(|a| a.resource == ResourceKey::Cpu && a.step == YieldStep::Cap)
            .map(|a| (a.allocation_id.clone(), a.target))
            .collect()
    }

    /// Every round in which held caps rise totals at most the spare over usage,
    /// split equally among the held steps (FR-013).
    #[test]
    fn a_round_of_rises_stays_within_the_spare_and_splits_it_equally() {
        let run = run(1200, (200_000, 220_000));
        let mut rounds = 0;
        for pair in run.windows(2) {
            let (before, after) = (&pair[0], &pair[1]);
            let (old, new) = (cpu_targets(&before.decision), cpu_targets(&after.decision));
            let risen: Vec<&String> = new
                .iter()
                .filter(|(id, target)| old.get(*id).is_some_and(|old| *target > old))
                .map(|(id, _)| id)
                .collect();
            if risen.is_empty() {
                continue;
            }
            rounds += 1;
            let budget = after.decision.managed_budget.cpu_millicores;
            let spare = budget.saturating_sub(after.own);
            let held = after
                .decision
                .actions
                .iter()
                .filter(|a| a.resource == ResourceKey::Cpu)
                .count() as u64;
            let index = |id: &String| id[1..].parse::<usize>().unwrap();
            let over_usage: Vec<u64> = risen
                .iter()
                .map(|id| new[*id].unwrap().saturating_sub(after.usage[index(id)]))
                .collect();
            assert!(
                over_usage.iter().sum::<u64>() <= spare + held,
                "{over_usage:?} > {spare}"
            );
            let share = spare / held.max(1);
            assert!(
                over_usage.iter().all(|r| *r <= share + 1),
                "{over_usage:?} vs {share}"
            );
        }
        assert!(rounds > 0, "caps rose at least once");
    }

    /// With caps that hold from their first period, tasks that make up lost CPU
    /// settle near the share after an external spike: no pause and at most two
    /// step changes in the last 300 s, and own work at 90% of the share or more
    /// (SC-015).
    #[test]
    fn tasks_that_make_up_lost_cpu_do_not_cycle_under_caps_that_hold() {
        let run = run(2400, (200_000, 220_000));
        let tail = &run[run.len() - 600..];
        // A change of step (none, cap, pause) per task, as the host harness counted.
        let steps = |decision: &Decision| -> BTreeMap<String, YieldStep> {
            decision
                .actions
                .iter()
                .filter(|a| a.resource == ResourceKey::Cpu)
                .map(|a| (a.allocation_id.clone(), a.step))
                .collect()
        };
        let changes: usize = tail
            .windows(2)
            .map(|pair| {
                let (before, after) = (steps(&pair[0].decision), steps(&pair[1].decision));
                (0..TASKS)
                    .map(|i| format!("t{i}"))
                    .filter(|id| before.get(id) != after.get(id))
                    .count()
            })
            .sum();
        let paused = tail.iter().any(|interval| {
            interval
                .decision
                .actions
                .iter()
                .any(|a| a.resource == ResourceKey::Cpu && a.step == YieldStep::Pause)
        });
        let own = tail.iter().map(|interval| interval.own).sum::<u64>() / tail.len() as u64;
        let share = tail
            .iter()
            .map(|interval| {
                interval.decision.resources[&ResourceKey::Cpu]
                    .target_share
                    .unwrap()
            })
            .sum::<u64>()
            / tail.len() as u64;
        assert!(!paused);
        assert!(changes <= 2, "{changes} step changes in the last 300 s");
        assert!(
            own * 10 >= share * 9,
            "own {own} against a share of {share}"
        );
    }
}

/// CPU returns at once once contention has fully ended (FR-034): own work
/// of 16 tasks of 250 m on 4000 m beside a contender of another user. The simulated host
/// splits CPU by threads, weighted max-min, and honours the caps the policy applies.
/// New victims of a contended CPU excess share it by usage within their tier, at one level, as
/// caps already held do (FR-016).
mod new_victims {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const CAPACITY: u64 = 4000;
    const INTERVAL: u64 = 500;

    /// Tasks with `usage` alone on 4 CPUs, then beside `external` from 5 s on, the caps not yet
    /// taking effect: each task's step and target 2 s after that.
    fn steps(
        mode: ContentionMode,
        usage: &[u64],
        external: u64,
    ) -> BTreeMap<String, (YieldStep, Option<u64>)> {
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        let mut engine = PolicyEngine::new();
        let ids: Vec<String> = (0..usage.len()).map(|i| format!("t{i}")).collect();
        let own: u64 = usage.iter().sum();
        let mut last = BTreeMap::new();
        for tick in 0..=14 {
            let now = tick * INTERVAL;
            let beside = if now >= 5000 { external } else { 0 };
            let mut snap = snapshot(own + beside);
            snap.cpu_capacity_millicores = CAPACITY;
            snap.physical_cores = Some(4);
            let input = PolicyInput {
                allocations: ids
                    .iter()
                    .zip(usage)
                    .map(|(id, used)| {
                        allocation(id, AllocationClass::Opportunistic, 250, Some(*used))
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
            last = decision
                .actions
                .iter()
                .filter(|a| a.resource == ResourceKey::Cpu)
                .map(|a| (a.allocation_id.clone(), (a.step, a.target)))
                .collect();
        }
        last
    }

    /// With no excess no task gives anything, however uneven their usage. Nearly equal tasks are
    /// the case that rounding could cap: 403 m over four is 100 m after rounding down.
    #[test]
    fn uneven_tasks_with_no_excess_hold_no_step() {
        for mode in [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ] {
            for usage in [
                &[100, 100, 100, 103][..],
                &[100, 101, 103, 107, 110, 98, 99],
            ] {
                let steps = steps(mode, usage, 0);
                assert!(steps.is_empty(), "{mode:?}: {steps:?}");
            }
        }
    }

    #[test]
    fn equal_tasks_share_the_excess_at_one_level() {
        // 3200m beside 800m on a saturated host, over the 2000m fair share by 1200m: 300m each.
        // Capped by the whole excess left, the first would be paused and the second capped to 400m.
        let steps = steps(ContentionMode::Neutral, &[800; 4], 800);
        assert_eq!(steps.len(), 4, "{steps:?}");
        for (id, (step, target)) in &steps {
            assert_eq!(*step, YieldStep::Cap, "{id}: {steps:?}");
            assert!(
                target.unwrap().abs_diff(500) <= 1,
                "{id} keeps its share: {steps:?}"
            );
        }
    }

    #[test]
    fn no_new_victim_is_paused_for_an_excess_the_others_can_give() {
        // 800m beside 2800m, over Yield First's 600m target by 200m. Capped by the whole excess,
        // the first task would be capped to 0, which becomes a pause.
        let steps = steps(ContentionMode::YieldFirst, &[200; 4], 2800);
        assert_eq!(steps.len(), 4, "{steps:?}");
        for (id, (step, target)) in &steps {
            assert_eq!(*step, YieldStep::Cap, "{id}: {steps:?}");
            assert!(
                target.unwrap().abs_diff(150) <= 1,
                "{id} keeps its share: {steps:?}"
            );
        }
    }

    #[test]
    fn a_task_below_the_level_keeps_what_it_uses() {
        // 2400m beside 1600m on a saturated host, over the 2000m fair share by 400m: the two
        // large tasks give it between them, down to 900m each, and the two small ones hold no
        // step.
        let steps = steps(ContentionMode::Neutral, &[1100, 1100, 100, 100], 1600);
        assert_eq!(steps.len(), 2, "{steps:?}");
        for (id, (step, target)) in &steps {
            assert!(["t0", "t1"].contains(&id.as_str()), "{steps:?}");
            assert_eq!(*step, YieldStep::Cap, "{steps:?}");
            assert!(target.unwrap().abs_diff(900) <= 1, "{steps:?}");
        }
    }
}

mod contention_ended {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const CAPACITY: u64 = 4000;
    const INTERVAL: u64 = 500;
    const TASKS: usize = 16;
    const TASK: u64 = 250;

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

    struct Tick {
        now: u64,
        own: u64,
        contender: u64,
        steps: Vec<(YieldStep, Option<u64>)>,
        share: u64,
    }

    /// Each interval's usage and steps while the contender wants `demand(now)`.
    fn run(mode: ContentionMode, ticks: u64, demand: impl Fn(u64) -> u64) -> Vec<Tick> {
        run_with(mode, ticks, demand, |_, _| {})
    }

    /// `run`, with each interval's snapshot adjusted by `tweak`.
    fn run_with(
        mode: ContentionMode,
        ticks: u64,
        demand: impl Fn(u64) -> u64,
        tweak: impl Fn(u64, &mut Snapshot),
    ) -> Vec<Tick> {
        run_shaped(mode, ticks, TASKS, 100, demand, tweak)
    }

    /// `run_with` for `tasks` tasks, each capped below its demand running at `undershoot`
    /// percent of its cap, as a duty cycle that loses part of each period does.
    fn run_shaped(
        mode: ContentionMode,
        ticks: u64,
        tasks: usize,
        undershoot: u64,
        demand: impl Fn(u64) -> u64,
        tweak: impl Fn(u64, &mut Snapshot),
    ) -> Vec<Tick> {
        run_seeded(mode, ticks, &vec![undershoot; tasks], &[], demand, tweak)
    }

    /// `run_shaped` with each task's own undershoot, and with CPU caps already held at
    /// `seeded` targets from the start, as an agent that re-adopts them begins.
    fn run_seeded(
        mode: ContentionMode,
        ticks: u64,
        undershoots: &[u64],
        seeded: &[u64],
        demand: impl Fn(u64) -> u64,
        tweak: impl Fn(u64, &mut Snapshot),
    ) -> Vec<Tick> {
        let tasks = undershoots.len();
        let mut config = config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = mode;
        let mut engine = PolicyEngine::new();
        let mut caps: Vec<(YieldStep, Option<u64>)> = vec![(YieldStep::None, None); tasks];
        let ids: Vec<String> = (0..tasks).map(|i| format!("t{i}")).collect();
        for (i, target) in seeded.iter().enumerate() {
            let request = cedegrid::contention::StepRequest {
                step: YieldStep::Cap,
                target: Some(*target),
                cause: cedegrid::contention::YieldCause::ExternalGrowth,
                deadline_ms: None,
            };
            engine.seed(&ids[i], ResourceKey::Cpu, request, 0);
            caps[i] = (YieldStep::Cap, Some(*target));
        }
        let mut out = Vec::new();
        for tick in 0..ticks {
            let now = tick * INTERVAL;
            let mut demands: Vec<u64> = caps
                .iter()
                .zip(undershoots)
                .map(|((step, target), undershoot)| match step {
                    YieldStep::Pause => 0,
                    YieldStep::Cap if target.is_some_and(|target| target < TASK) => {
                        target.unwrap_or(TASK) * undershoot / 100
                    }
                    YieldStep::Cap => TASK,
                    _ => TASK,
                })
                .collect();
            demands.push(demand(now));
            let usage = schedule(&demands);
            let contender = usage[tasks];
            let own: u64 = usage[..tasks].iter().sum();
            let mut snap = snapshot(own + contender);
            snap.cpu_capacity_millicores = CAPACITY;
            snap.physical_cores = Some(4);
            snap.principals = Some(vec![
                PrincipalUsage {
                    uid: Some(1000),
                    is_owner: true,
                    ..PrincipalUsage::default()
                },
                PrincipalUsage {
                    uid: Some(2000),
                    cpu_millicores: contender,
                    ..PrincipalUsage::default()
                },
            ]);
            tweak(now, &mut snap);
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
            out.push(Tick {
                now,
                own,
                contender,
                steps: caps.clone(),
                share: decision.resources[&ResourceKey::Cpu]
                    .target_share
                    .unwrap_or(0),
            });
        }
        out
    }

    /// The first interval at or after `from` in which own work is within 2% of `target`.
    fn reached(run: &[Tick], from: u64, target: u64) -> Option<u64> {
        run.iter()
            .find(|tick| tick.now >= from && tick.own * 50 >= target * 49)
            .map(|tick| tick.now - from)
    }

    /// A one-core contender drops to half a core, then leaves 5 s later: own work is at the
    /// uncontended target within 2 s plus one interval, though a rise came less than the
    /// mode's interval before.
    #[test]
    fn a_contender_that_leaves_returns_the_cpu_within_two_seconds_and_an_interval() {
        for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
            let run = run(mode, 240, |now| match now {
                10_000..40_000 => 1000,
                40_000..45_000 => 500,
                _ => 0,
            });
            let target = run.last().unwrap().share;
            let after = reached(&run, 45_000, target);
            assert!(
                after.is_some_and(|ms| ms <= 2000 + INTERVAL),
                "{mode:?}: {after:?} to reach {target}m"
            );
        }
    }

    /// An unknown CPU interval after the contender leaves keeps the quiet count, so the return
    /// is late by that one interval only (FR-034).
    #[test]
    fn an_unknown_interval_keeps_the_quiet_count() {
        for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
            let run = run_with(
                mode,
                240,
                |now| match now {
                    10_000..45_000 => 1000,
                    _ => 0,
                },
                |now, snap| {
                    if now == 46_000 {
                        snap.cpu_busy_millicores = None;
                    }
                },
            );
            let target = run.last().unwrap().share;
            let after = reached(&run, 45_000, target);
            assert!(
                after.is_some_and(|ms| ms <= 2000 + 2 * INTERVAL),
                "{mode:?}: {after:?} to reach {target}m"
            );
        }
    }

    /// The return bound after the contender stops, in ms from its stop (FR-034, Q18): Yield First
    /// within 2 intervals of its quiet count, reached `principal_window_intervals` intervals into
    /// the quiet, since that count is the mode's hysteresis; Neutral and Performance within 3 s.
    fn return_bound(mode: ContentionMode) -> u64 {
        match mode {
            ContentionMode::YieldFirst => {
                (u64::from(config().contention.principal_window_intervals) - 1 + 2) * INTERVAL
            }
            _ => 3000,
        }
    }

    /// The 033 batch's run 7 shape (FR-034, Q15, R44): at the contender's end the held caps ran
    /// from the minimum duty up (1 m to 253 m), each binding. At the quiet count the held caps
    /// together reach the mode's target in one move, so in Yield First own usage is back within
    /// 2 intervals of its quiet count, not in rises one dwell apart (6.5 s before).
    /// Neutral's (3.5 s before) and Performance's (1.0 s) returns on the same shape
    /// stay within their 3 s and get no slower.
    #[test]
    fn uneven_held_caps_return_to_the_target_in_one_move() {
        const SEEDED: [u64; 13] = [1, 5, 24, 135, 184, 205, 220, 230, 240, 245, 250, 253, 253];
        let level = 13 * TASK;
        for (mode, bound) in [
            (
                ContentionMode::YieldFirst,
                return_bound(ContentionMode::YieldFirst),
            ),
            (ContentionMode::Neutral, 3000),
            (ContentionMode::Performance, 1000),
        ] {
            let run = run_seeded(
                mode,
                60,
                &[100; 13],
                &SEEDED,
                |now| if now < 10_000 { 1000 } else { 0 },
                |_, _| {},
            );
            let after: Vec<u64> = run
                .iter()
                .filter(|tick| tick.now >= 10_000)
                .map(|tick| tick.own)
                .collect();
            let back = after.iter().position(|own| own * 100 >= level * 98);
            assert!(
                back.is_some_and(|i| i as u64 * INTERVAL <= bound),
                "{mode:?}: back after {:?} intervals, {:?}",
                back,
                &after[..14]
            );
            if mode == ContentionMode::YieldFirst {
                let rose = after.iter().position(|own| *own > after[0]).unwrap();
                assert!(
                    after[rose..].iter().all(|own| *own == after[rose]),
                    "{mode:?}: {:?}",
                    &after[..14]
                );
            }
        }
    }

    /// A measured return (FR-034): 13 steady 250 m tasks in Yield First
    /// and 15 in Performance beside a one-core contender for 30 s, each capped task running at
    /// 85% of a binding cap. Once the quiet count says contention has ended, own usage comes
    /// back to its level before in one move: in Yield First within 2 intervals of the quiet
    /// count, in Performance within 3 s of the contender stopping (FR-034, Q18).
    #[test]
    fn run_7s_return_brings_own_usage_back_in_one_move_within_three_seconds() {
        // Performance takes no CPU step to return from (FR-034, FR-022).
        {
            let (mode, tasks) = (ContentionMode::YieldFirst, 13);
            let run = run_shaped(
                mode,
                140,
                tasks,
                85,
                |now| match now {
                    10_000..40_000 => 1000,
                    _ => 0,
                },
                |_, _| {},
            );
            let before = run.iter().find(|tick| tick.now == 9_500).unwrap().own;
            let after: Vec<u64> = run
                .iter()
                .filter(|tick| tick.now >= 40_000)
                .map(|tick| tick.own)
                .collect();
            let back = after.iter().position(|own| own * 100 >= before * 98);
            assert!(
                back.is_some_and(|i| i as u64 * INTERVAL <= return_bound(mode)),
                "{mode:?}: {before}m before, {:?}",
                &after[..12]
            );
            // One move: once own usage rises, it stays where it rose to.
            let rose = after.iter().position(|own| *own > after[0]).unwrap();
            assert!(
                after[rose..].iter().all(|own| *own == after[rose]),
                "{mode:?}: {:?}",
                &after[..12]
            );
        }
    }

    /// Contention that only falls keeps the mode's interval between rises of a held cap.
    #[test]
    fn contention_that_only_falls_keeps_the_modes_interval() {
        for (mode, interval) in [
            (ContentionMode::YieldFirst, 30_000),
            (ContentionMode::Neutral, 10_000),
        ] {
            let run = run(mode, 240, |now| {
                if now < 10_000 {
                    0
                } else if now < 40_000 {
                    1000
                } else {
                    500
                }
            });
            for task in 0..TASKS {
                let rises: Vec<u64> = run
                    .windows(2)
                    .filter(|pair| {
                        let (before, after) = (pair[0].steps[task], pair[1].steps[task]);
                        before.0 == YieldStep::Cap
                            && after.0 == YieldStep::Cap
                            && after.1 > before.1
                    })
                    .map(|pair| pair[1].now)
                    .collect();
                assert!(
                    rises.windows(2).all(|pair| pair[1] - pair[0] >= interval),
                    "{mode:?} t{task}: {rises:?}"
                );
            }
        }
    }

    /// An on/off contender, every 4 intervals as SC-009 states and every 30 s, changes each
    /// task's step at most once per switch, and when it comes back it has its core within 2
    /// intervals (SC-002).
    #[test]
    fn an_on_off_contender_changes_each_step_once_per_switch_and_is_served_on_return() {
        for (mode, half) in [
            (ContentionMode::YieldFirst, 2000),
            (ContentionMode::Neutral, 2000),
            (ContentionMode::YieldFirst, 30_000),
            (ContentionMode::Neutral, 30_000),
        ] {
            let on = |now: u64| now >= half && (now / half) % 2 == 1;
            let run = run(mode, 1200, |now| if on(now) { 1000 } else { 0 });
            for switch in 1..(run.last().unwrap().now / half) {
                let window: Vec<&Tick> = run
                    .iter()
                    .filter(|tick| tick.now / half == switch)
                    .collect();
                for task in 0..TASKS {
                    let changes = window
                        .windows(2)
                        .filter(|pair| pair[0].steps[task].0 != pair[1].steps[task].0)
                        .count();
                    assert!(
                        changes <= 1,
                        "{mode:?} {half} ms switch {switch} t{task}: {changes}"
                    );
                }
                if on(switch * half) {
                    let served = window
                        .iter()
                        .position(|tick| tick.contender >= 1000)
                        .unwrap_or(usize::MAX);
                    assert!(
                        served <= 2,
                        "{mode:?} {half} ms switch {switch}: served after {served}"
                    );
                }
            }
        }
    }
    /// A one-core contender drops to half a core, then leaves 5 s later, as in the test
    /// above: the caps moved less than the mode's interval before it leaves.
    fn leaving(now: u64) -> u64 {
        match now {
            10_000..40_000 => 1000,
            40_000..45_000 => 500,
            _ => 0,
        }
    }

    /// The quiet count (FR-034): a 4-CPU scope whose per-interval measurement band is 60m,
    /// above the 40m contention floor, with a 45m unattributed trickle once the contender
    /// leaves, still counts quiet intervals, so the CPU returns within 2 s and an interval.
    #[test]
    fn an_idle_scope_within_its_measurement_band_does_not_hold_the_return_back() {
        let run = run_with(ContentionMode::Neutral, 200, leaving, |now, snap| {
            let busy = snap.cpu_busy_millicores.unwrap_or(0);
            let trickle = if now >= 45_000 { 45 } else { 0 };
            snap.cpu_busy_millicores = Some(busy + trickle);
            snap.cpu_busy_lower_millicores = Some((busy + trickle).saturating_sub(60));
        });
        let target = run.last().unwrap().share;
        let after = reached(&run, 45_000, target);
        assert!(
            after.is_some_and(|ms| ms <= 2000 + INTERVAL),
            "{after:?} to reach {target}m"
        );
    }

    /// The quiet count uses this interval's unattributed estimate, not the window's: a
    /// window still showing 70m beside 20m now, within the band of each other, counts as
    /// quiet, so the CPU returns within 2 s and an interval (FR-034).
    #[test]
    fn the_quiet_count_uses_this_intervals_estimate_not_the_windows() {
        let run = run_with(ContentionMode::Neutral, 200, leaving, |now, snap| {
            let busy = snap.cpu_busy_millicores.unwrap_or(0);
            if now >= 45_000 {
                // A 60m band holds the window's 70m and this interval's 20m together.
                snap.cpu_busy_millicores = Some(busy + 20);
                snap.cpu_busy_lower_millicores = Some((busy + 20).saturating_sub(60));
                snap.kernel = Some(scoped_kernel(
                    now,
                    busy + 20,
                    (busy + 20).saturating_sub(60),
                    busy + 70,
                ));
                snap.managed_cpu_window_lower_millicores =
                    Some(snap.managed_cpu_lower_millicores.unwrap_or(busy).min(busy));
            }
        });
        let target = run.last().unwrap().share;
        let after = reached(&run, 45_000, target);
        assert!(
            after.is_some_and(|ms| ms <= 2000 + INTERVAL),
            "{after:?} to reach {target}m"
        );
    }

    /// A scoped kernel snapshot of all four CPUs with this interval's busy time, its lower
    /// bound and the window's.
    fn scoped_kernel(
        now: u64,
        busy: u64,
        lower: u64,
        window: u64,
    ) -> cedegrid::kernel::KernelSnapshot {
        use cedegrid::kernel::*;
        KernelSnapshot {
            schema_version: 1,
            runtime_os: "linux".into(),
            kernel_release: Some("fixture".into()),
            observed_at_unix_ms: 100,
            monotonic_elapsed_ms: now + 500,
            sample_interval_ms: Some(INTERVAL),
            freshness_limit_ms: 1500,
            controls: vec![],
            cpu: CpuTopology {
                allowed_cpu_ids: Some(vec![0, 1, 2, 3]),
                effective_cgroup_cpu_ids: Some(vec![0, 1, 2, 3]),
                effective_cpu_ids: Some(vec![0, 1, 2, 3]),
                cores: (0..4)
                    .map(|id| CpuCore {
                        cpu_id: id,
                        package_id: Some(0),
                        core_id: Some(id as i32),
                        thread_siblings: Some(vec![id]),
                    })
                    .collect(),
                effective_cpu_capacity_millicores: Some(CAPACITY),
                busy_millicores: Some(busy),
                busy_lower_millicores: Some(lower),
                busy_status: CpuUsageStatus::Available,
                busy_interval_ms: Some(INTERVAL),
                busy_window_millicores: Some(window),
                busy_window_lower_millicores: Some(window.saturating_sub(busy - lower)),
                busy_window_ms: Some(4 * INTERVAL),
                visible_cpu_ceiling_millicores: Some(CAPACITY),
                detail: "synthetic scoped telemetry".into(),
            },
            hierarchy: None,
            psi: vec![],
            cpu_pressure: None,
            limitations: vec![],
        }
    }

    /// A contender that stops and comes back within the quiet window restarts the count:
    /// the return waits the full window after it stops again, then comes within 2 s and an
    /// interval (FR-034).
    #[test]
    fn a_contender_that_comes_back_restarts_the_quiet_count() {
        let window = u64::from(config().contention.principal_window_intervals);
        let run = run(ContentionMode::Neutral, 200, |now| match now {
            45_000..45_500 => 0,
            45_500..46_000 => 500,
            now if now >= 46_000 => 0,
            now => leaving(now),
        });
        let target = run.last().unwrap().share;
        let after = reached(&run, 46_000, target).expect("own work never returned");
        assert!(
            after >= (window - 1) * INTERVAL,
            "{after} ms after it stopped again"
        );
        assert!(
            after <= 2000 + INTERVAL,
            "{after} ms after it stopped again"
        );
    }

    /// The quiet rule itself: every principal and this interval's unattributed estimate
    /// below the larger of the contention floor and the measurement band (FR-034).
    #[test]
    fn quiet_is_judged_against_the_larger_of_the_floor_and_the_band() {
        use cedegrid::contention::cpu_quiet;
        assert!(cpu_quiet([10, 30], 35, 40, 0));
        assert!(!cpu_quiet([10, 45], 35, 40, 0));
        assert!(!cpu_quiet([10], 45, 40, 20));
        assert!(cpu_quiet([10, 45], 45, 40, 60));
        assert!(!cpu_quiet([], 60, 40, 60));
    }
}

mod launch_nice {
    use cedegrid::config::Config;
    use cedegrid::contention::{ContentionMode::*, ResourceKind};
    use cedegrid::execution_model::{AllocationClass, JobContention, LaunchRequest};
    use std::collections::BTreeMap;

    /// Only Yield First Opportunistic work launches at `cpu.nice`; every other mode, and
    /// Guaranteed work, keeps the default priority and yields through its caps. A task
    /// that requires the `cpu.nice` control gets it in any mode (FR-011).
    #[test]
    fn only_yield_first_opportunistic_work_launches_at_the_configured_nice() {
        for (node, mode, class, required, nice) in [
            (YieldFirst, None, AllocationClass::Opportunistic, false, 10),
            (Neutral, None, AllocationClass::Opportunistic, false, 0),
            (Performance, None, AllocationClass::Opportunistic, false, 0),
            (
                Performance,
                Some(YieldFirst),
                AllocationClass::Opportunistic,
                false,
                10,
            ),
            // The node's mode caps the request.
            (
                YieldFirst,
                Some(Performance),
                AllocationClass::Opportunistic,
                false,
                10,
            ),
            (YieldFirst, None, AllocationClass::Guaranteed, false, 0),
            (Performance, None, AllocationClass::Opportunistic, true, 10),
        ] {
            let mut config = Config::default();
            config.contention.default_mode = node;
            config
                .contention
                .acknowledge_caution
                .insert("performance_mode".into());
            let request: LaunchRequest = serde_json::from_value(serde_json::json!({
                "task_id": "t", "assignment_id": "a", "argv": ["/bin/true"], "cwd": "/",
                "env": {}, "resources": {"cpu_millicores": 100, "ram_mib": 16, "gpu_memory_mib": {}},
                "class": if class == AllocationClass::Guaranteed { "guaranteed" } else { "opportunistic" },
                "replay_safe": true, "single_process": true, "no_escape": true,
                "required_controls": if required { vec!["cpu.nice"] } else { vec![] },
                "contention": mode.map(|mode| JobContention {
                    modes: BTreeMap::from([(ResourceKind::Cpu, mode)]),
                }),
            }))
            .unwrap();
            assert_eq!(
                config.launch_nice(&request),
                nice,
                "{node:?} {mode:?} {class:?} required {required}"
            );
        }
    }
}

/// External GPU compute demand read where own work ran uncapped (FR-021, FR-022, Q19).
mod gpu_demand {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    const INTERVAL: u64 = 500;
    const GPU: &str = "GPU-demand";
    const PEER: u32 = 2001;

    /// T368's Performance light run at `5f729bc` (036's evidence): the light user's reading per
    /// interval alone, beside the probe at full duty without CedeGrid, and beside it under
    /// CedeGrid's caps. Its demand was the same 200 requests a second throughout.
    const ALONE: [u64; 60] = [
        60, 150, 150, 150, 150, 160, 150, 150, 160, 150, 150, 160, 150, 150, 150, 160, 160, 150,
        150, 160, 150, 150, 160, 150, 150, 160, 160, 160, 150, 150, 160, 160, 150, 160, 150, 160,
        150, 150, 150, 150, 150, 150, 150, 150, 150, 150, 150, 150, 160, 160, 160, 160, 160, 150,
        150, 160, 150, 160, 160, 160,
    ];
    const BESIDE_FULL_DUTY: [u64; 60] = [
        150, 70, 0, 330, 220, 660, 0, 0, 250, 0, 0, 0, 330, 0, 0, 330, 220, 0, 0, 330, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 330, 0, 660, 0, 330, 330, 250, 0, 250, 0, 0, 330, 330, 0, 0, 0, 330,
        0, 0, 330, 0, 0, 0, 0, 80, 220, 0, 0, 330,
    ];
    const BESIDE_CAPS: [u64; 60] = [
        150, 160, 160, 160, 330, 0, 0, 0, 330, 150, 270, 360, 260, 320, 310, 640, 310, 310, 640,
        310, 310, 640, 420, 310, 310, 310, 630, 300, 300, 630, 300, 300, 630, 420, 300, 300, 420,
        290, 290, 420, 290, 290, 280, 280, 280, 280, 280, 270, 270, 270, 270, 270, 260, 260, 260,
        250, 180, 170, 160, 150,
    ];

    pub(super) fn mode_config(mode: ContentionMode) -> Config {
        let mut config = config();
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        config
    }

    pub(super) fn job(mode: Option<ContentionMode>) -> Allocation {
        let resources = Resources {
            cpu_millicores: 100,
            ram_mib: 100,
            gpu_memory_mib: BTreeMap::from([(GPU.to_string(), 1000)]),
        };
        Allocation {
            id: "job".into(),
            class: AllocationClass::Opportunistic,
            phase: AllocationPhase::Running,
            requested: resources.clone(),
            observed: Some(resources),
            observed_upper: None,
            observed_window_cpu: None,
            usage_unknown: None,
            modes: mode
                .map(|mode| {
                    BTreeMap::from([(cedegrid::contention::ResourceKind::GpuCompute, mode)])
                })
                .unwrap_or_default(),
        }
    }

    /// One measured interval: the other user's reading, own work's (none while it does not run),
    /// the device busy as both; the other user's process stays listed whether sampled or not.
    pub(super) fn interval(own: Option<u64>, external: u64, sample: u64) -> Snapshot {
        let mut snap = snapshot(0);
        snap.gpu_inventory = CapabilityStatus::Available;
        snap.gpus = vec![GpuSnapshot {
            uuid: GPU.into(),
            total_memory_mib: 16_000,
            used_memory_mib: Some(2000),
            utilization_percent: Some(((own.unwrap_or(0) + external) / 10).min(100) as u32),
            external_process_ids: Some(vec![987]),
            external_compute: ComputeActivity::Active,
            compute_sample_id: Some(sample),
            observation: None,
            reserved_memory_mib: None,
            free_memory_mib: None,
            unattributed_memory_mib: None,
            external_compute_milli: Some(external),
            own_compute_milli: Some(own.unwrap_or(0)),
            compute_amount_unknown: None,
            own_only: false,
            background_process_ids: vec![],
            compute_sampling: None,
            time_sliced: None,
            compute_contexts: None,
        }];
        snap.principals = Some(vec![
            PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                ..PrincipalUsage::default()
            },
            PrincipalUsage {
                uid: Some(PEER),
                gpu_compute_milli: BTreeMap::from([(GPU.to_string(), external)]),
                ..PrincipalUsage::default()
            },
        ]);
        snap
    }

    /// The GPU compute cap a decision sends the job: 1000 when none.
    pub(super) fn cap_of(decision: &Decision) -> u64 {
        let key = ResourceKey::GpuCompute(GPU.into());
        decision
            .actions
            .iter()
            .find(|a| a.resource == key && a.allocation_id == "job")
            .map_or(1000, |a| match a.step {
                YieldStep::Cap => a.duty.or(a.target).unwrap_or(1000),
                YieldStep::Pause => 0,
                _ => 1000,
            })
    }

    pub(super) fn target_of(decision: &Decision) -> u64 {
        decision.resources[&ResourceKey::GpuCompute(GPU.into())]
            .target_share
            .unwrap_or(0)
    }

    /// `closed_loop`'s decisions: the light user's alone, then own work's beside the other user.
    pub(super) fn loop_decisions(
        mode: ContentionMode,
        job_mode: Option<ContentionMode>,
        intervals: u64,
        reading: &dyn Fn(u64, bool) -> u64,
    ) -> (Vec<Decision>, Vec<Decision>) {
        let config = mode_config(mode);
        let mut engine = PolicyEngine::new();
        let (mut now, mut alone) = (0, vec![]);
        for i in 0..60u64 {
            let snap = interval(None, ALONE[i as usize], i + 1);
            alone.push(
                engine
                    .evaluate(&config, &snap, &PolicyInput::default(), now)
                    .unwrap(),
            );
            now += INTERVAL;
        }
        let input = PolicyInput {
            allocations: vec![job(job_mode)],
            ..PolicyInput::default()
        };
        let (mut caps, mut out) = ([1000u64, 1000u64], vec![]);
        for i in 0..intervals {
            let capped = caps.iter().any(|cap| *cap < 1000);
            let external = reading(i, capped);
            let own = caps[0].min(990 - external);
            let snap = interval(Some(own), external, 61 + i);
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            caps = [cap_of(&decision), caps[0]];
            out.push(decision);
            now += INTERVAL;
        }
        (alone, out)
    }

    fn median(mut values: Vec<u64>) -> u64 {
        values.sort_unstable();
        values[values.len() / 2]
    }

    /// Each decision records how the device's GPU demand was read (FR-021, Q19): whether the
    /// interval was uncapped and fed the estimate, the estimate and its count, the time since the
    /// last uncapped interval, an open window, and which amount set the target. The contention
    /// report does not carry it.
    #[test]
    fn each_decision_records_how_gpu_demand_was_read() {
        use cedegrid::model::GpuDemandSource;
        let key = ResourceKey::GpuCompute(GPU.into());
        let record = |decision: &Decision| {
            decision.resources[&key]
                .gpu_demand
                .clone()
                .expect("no gpu_demand record")
        };
        let light = |i: u64, capped: bool| {
            if capped {
                BESIDE_CAPS[(i % 60) as usize]
            } else {
                BESIDE_FULL_DUTY[(i % 60) as usize]
            }
        };
        let (alone, own) = loop_decisions(ContentionMode::Neutral, None, 200, &light);
        // A new device reads unknown until its amount is measured; those intervals feed nothing.
        let start = alone
            .iter()
            .position(|d| record(d).fed)
            .expect("the light user alone never fed the estimate");
        for (i, decision) in alone[..start].iter().enumerate() {
            let read = record(decision);
            assert!(
                read.uncapped && !read.fed && read.estimate.is_none(),
                "alone {i}: {read:?}"
            );
            assert_eq!(read.source, GpuDemandSource::Interval);
        }
        for (i, decision) in alone.iter().enumerate().skip(start) {
            let read = record(decision);
            assert!(read.uncapped && read.fed, "alone {i}: {read:?}");
            assert_eq!(read.intervals, (i - start + 1) as u32);
            assert_eq!(read.since_uncapped_ms, Some(0));
            assert_eq!(read.source, GpuDemandSource::Estimate);
            assert!(!read.window);
        }
        let fed = &ALONE[start..];
        let mean = (fed.iter().sum::<u64>() + fed.len() as u64 / 2) / fed.len() as u64;
        assert_eq!(record(&alone[59]).estimate, Some(mean));
        // A lone Neutral job beside the light user is never held (FR-089): every interval it runs
        // in stays uncapped and feeds the estimate, and no window opens.
        for (i, decision) in own.iter().enumerate() {
            let read = record(decision);
            assert!(
                read.uncapped && read.fed && !read.window,
                "own {i}: {read:?}"
            );
        }
    }

    fn record_of(decision: &Decision) -> cedegrid::model::GpuDemandRecord {
        decision.resources[&ResourceKey::GpuCompute(GPU.into())]
            .gpu_demand
            .clone()
            .expect("no gpu_demand record")
    }

    /// Replays a recorded run's GPU readings through the policy, open loop:
    /// `CEDEGRID_GPU_DEMAND_REPLAY` names a run folder with `gpu-trace.json` and
    /// `gpu-replay.json`, `CEDEGRID_GPU_DEMAND_MODE` its mode (`performance`, `neutral`). Own work
    /// runs in the managed windows only; the readings are those recorded under the run's own caps.
    /// With `CEDEGRID_GPU_DEMAND_OWN=unmanaged` the owner's processes in the unmanaged windows are
    /// taken as own work's instead, so the policy reads them uncapped.
    #[test]
    #[ignore = "replay: needs CEDEGRID_GPU_DEMAND_REPLAY"]
    fn replay_gpu_demand() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("CEDEGRID_GPU_DEMAND_REPLAY")
                .expect("CEDEGRID_GPU_DEMAND_REPLAY names a run folder"),
        );
        // `aggressive` is Performance with no GPU compute margin, as on the host.
        let config = match std::env::var("CEDEGRID_GPU_DEMAND_MODE").as_deref() {
            Ok("neutral") => mode_config(ContentionMode::Neutral),
            Ok("aggressive") => time_slice::aggressive_config(),
            _ => mode_config(ContentionMode::Performance),
        };
        let read = |name: &str| -> Vec<serde_json::Value> {
            serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
        };
        let (replay, trace) = (read("gpu-replay.json"), read("gpu-trace.json"));
        let own_window = std::env::var("CEDEGRID_GPU_DEMAND_OWN").unwrap_or("managed".into());
        let mut engine = PolicyEngine::new();
        let mut now = 0;
        // The recordings predate the device's contexts. The other user's processes are
        // those listed while it ran alone, the owner's unmanaged ones the other listed processes,
        // and own work's the sampled ones not listed as external; the device is taken as
        // time-sliced, as the test host's has neither MPS nor MIG.
        let snapshots = |window: &serde_json::Value| -> Vec<GpuSnapshot> {
            serde_json::from_value(window["snapshots"].clone()).unwrap()
        };
        let peer: std::collections::BTreeSet<u32> = replay
            .iter()
            .filter(|window| window["name"] == "alone")
            .flat_map(snapshots)
            .flat_map(|gpu| gpu.external_process_ids.unwrap_or_default())
            .collect();
        let peer_uid = trace
            .iter()
            .flat_map(|window| {
                window["observations"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .flat_map(|o| o["principals"].as_array().cloned().unwrap_or_default())
            .filter_map(|entry| entry[0].as_u64().map(|uid| uid as u32))
            .find(|uid| *uid != 1000);
        for (window, recorded) in replay.iter().zip(&trace) {
            let name = window["name"].as_str().unwrap_or("?");
            let as_own = name == own_window && name != "managed";
            let mut gpus = snapshots(window);
            let mut own_seen = std::collections::BTreeSet::new();
            for gpu in gpus.iter_mut() {
                // The owner's unmanaged processes stand for own work's: no longer external.
                if as_own && let Some(pids) = gpu.external_process_ids.as_mut() {
                    pids.retain(|pid| peer.contains(pid));
                }
                let listed = gpu.external_process_ids.clone().unwrap_or_default();
                let sampled: Vec<u32> = gpu
                    .compute_sampling
                    .iter()
                    .flat_map(|sampling| &sampling.processes)
                    .filter(|p| p.samples.iter().any(|(_, util)| *util > 0))
                    .map(|p| p.pid)
                    .collect();
                own_seen.extend(sampled.iter().copied().filter(|pid| !listed.contains(pid)));
                let owner: Vec<u32> = listed
                    .iter()
                    .copied()
                    .filter(|pid| !peer.contains(pid))
                    .collect();
                gpu.time_sliced = Some(true);
                gpu.compute_contexts = Some(cedegrid::model::GpuComputeContexts {
                    own: own_seen.iter().copied().collect(),
                    owner: owner.clone(),
                    sampled: sampled
                        .iter()
                        .map(|pid| {
                            let uid = if peer.contains(pid) {
                                peer_uid
                            } else {
                                Some(1000)
                            };
                            (*pid, uid)
                        })
                        .collect(),
                    listed: listed
                        .iter()
                        .copied()
                        .filter(|pid| peer.contains(pid))
                        .map(|pid| (pid, peer_uid))
                        .collect(),
                    // The recordings predate the flag; samples were read where the
                    // interval recorded any process's.
                    samples_readable: gpu
                        .compute_sampling
                        .as_ref()
                        .is_some_and(|sampling| !sampling.processes.is_empty()),
                    others_quiet: false,
                });
            }
            let observations = recorded["observations"].as_array().unwrap();
            let (mut targets, mut caps, mut was, mut fed, mut windows) =
                (vec![], vec![], vec![], 0, 0);
            let (mut last, mut own_share) = (None, None);
            let (mut held, mut counts) = (0, vec![]);
            let mut steps: BTreeMap<String, usize> = BTreeMap::new();
            // d0, w and d0' per decision.
            type D0Key = (
                Option<u64>,
                Option<u64>,
                Option<u64>,
                Option<cedegrid::model::SliceSource>,
            );
            let (mut d0s, mut duties, mut readings, mut unbound, mut pauses) =
                (BTreeMap::<D0Key, usize>::new(), vec![], vec![], 0, 0);
            for (gpu, observation) in gpus.iter_mut().zip(observations) {
                let key = ResourceKey::GpuCompute(gpu.uuid.clone());
                // Busy CPU above own work's, so own CPU reads consistent beside it.
                let mut snap = snapshot(1000);
                snap.gpu_inventory = CapabilityStatus::Available;
                // The recorded observation belongs to the snapshot it was taken with.
                if let Some(observation) = &gpu.observation {
                    snap.observed_at_unix_ms = observation.observed_at_unix_ms;
                }
                let mut principals = vec![PrincipalUsage {
                    uid: Some(1000),
                    is_owner: true,
                    ..PrincipalUsage::default()
                }];
                for entry in observation["principals"].as_array().into_iter().flatten() {
                    let uid = entry[0].as_u64().map(|uid| uid as u32);
                    let milli = entry[1]
                        .as_object()
                        .and_then(|m| m.values().next())
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    // The test account is the agent's owner, as on the host.
                    if uid == Some(1000) && as_own {
                        gpu.own_compute_milli = Some(milli);
                        gpu.external_compute_milli =
                            gpu.external_compute_milli.map(|e| e.saturating_sub(milli));
                        continue;
                    }
                    if uid == Some(1000) {
                        principals[0].gpu_compute_milli =
                            BTreeMap::from([(gpu.uuid.clone(), milli)]);
                        continue;
                    }
                    principals.push(PrincipalUsage {
                        uid,
                        gpu_compute_milli: BTreeMap::from([(gpu.uuid.clone(), milli)]),
                        ..PrincipalUsage::default()
                    });
                }
                snap.principals = Some(principals);
                snap.gpus = vec![gpu.clone()];
                let mut allocation = job(None);
                allocation.requested.gpu_memory_mib = BTreeMap::from([(gpu.uuid.clone(), 1000)]);
                // Own memory within what the device reports in use, as the probe's was.
                let mut observed = allocation.requested.clone();
                observed.gpu_memory_mib = BTreeMap::from([(gpu.uuid.clone(), 100)]);
                allocation.observed = Some(observed);
                let input = PolicyInput {
                    allocations: if name == own_window {
                        vec![allocation]
                    } else {
                        vec![]
                    },
                    ..PolicyInput::default()
                };
                let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
                now += INTERVAL;
                let status = &decision.resources[&key];
                targets.push(status.target_share.unwrap_or(0));
                caps.push(
                    decision
                        .actions
                        .iter()
                        .find(|a| a.resource == key)
                        .map_or(1000, |a| {
                            if a.step == YieldStep::Cap {
                                a.duty.or(a.target).unwrap_or(1000)
                            } else {
                                0
                            }
                        }),
                );
                was.push(observation["status"]["target_share"].as_u64().unwrap_or(0));
                let record = status.gpu_demand.clone().unwrap_or_default();
                fed += usize::from(record.fed);
                windows += usize::from(record.window);
                last = record.estimate;
                own_share = record.own_uncapped;
                if let Some(slice) = &status.time_slice {
                    held += usize::from(slice.holds);
                    counts.push((slice.own, slice.external, slice.principals));
                    if !slice.holds {
                        *d0s.entry((
                            slice.d0,
                            slice.window_share,
                            slice.d0_windows,
                            slice.source,
                        ))
                        .or_default() += 1;
                    }
                    if let Some(step) = slice
                        .step
                        .filter(|step| *step != cedegrid::model::SliceStep::Holding)
                    {
                        *steps.entry(format!("{step:?}")).or_default() += 1;
                    }
                    duties.extend(slice.duty);
                    readings.extend(slice.bound.iter().map(|b| (b.reading, b.threshold)));
                    unbound += usize::from(!slice.holds && slice.d0.is_none());
                }
                pauses += usize::from(
                    decision
                        .actions
                        .iter()
                        .any(|a| a.resource == key && a.step == YieldStep::Pause),
                );
            }
            let (low, high) = (
                readings.iter().map(|r| r.0).min(),
                readings.iter().map(|r| r.0).max(),
            );
            let thresholds: std::collections::BTreeSet<u64> =
                readings.iter().map(|r| r.1).collect();
            let at = |i: usize| counts.get(i).copied();
            println!(
                "replay {name}: {} intervals, target median {} (recorded {}), duty median {}, fed {fed}, window decisions {windows}, estimate at the end {last:?}, own share at the end {own_share:?}; time-slice rule held in {held} of {} (m, E, k) at the start {:?}, middle {:?}, end {:?}",
                targets.len(),
                median(targets.clone()),
                median(was),
                median(caps),
                counts.len(),
                at(0),
                at(counts.len() / 2),
                counts.last()
            );
            println!(
                "replay {name}: steps {steps:?}; duty in {} decisions, (d0, w, d0', source) {d0s:?}, duty median {:?} min {:?}; bound readings {low:?}-{high:?} against {thresholds:?}; no principal bound in {unbound}; pauses {pauses}",
                duties.len(),
                (!duties.is_empty()).then(|| median(duties.clone())),
                duties.iter().min(),
            );
        }
    }

    /// Replays one recorded window with its recorded contexts through the policy, open loop
    ///: `CEDEGRID_TIME_SLICE_REPLAY` names a run folder with `gpu-replay.json` and
    /// `gpu-trace.json`, `CEDEGRID_TIME_SLICE_WINDOW` the window's index (5 for the Full run, 3
    /// for the Tracked one). The run's `alone` window feeds the estimate first; then two own
    /// allocations run in Performance. From the first decision whose device lists both own
    /// processes, m reads 2 and own work gets the time-slice duty, whether the processes were
    /// sampled yet or the estimate's feed is shut (FR-021).
    #[test]
    #[ignore = "replay: needs CEDEGRID_TIME_SLICE_REPLAY"]
    fn replay_time_slice_counts() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("CEDEGRID_TIME_SLICE_REPLAY")
                .expect("CEDEGRID_TIME_SLICE_REPLAY names a run folder"),
        );
        let index: usize = std::env::var("CEDEGRID_TIME_SLICE_WINDOW")
            .expect("CEDEGRID_TIME_SLICE_WINDOW names a window")
            .parse()
            .unwrap();
        let read = |name: &str| -> Vec<serde_json::Value> {
            serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
        };
        let (replay, trace) = (read("gpu-replay.json"), read("gpu-trace.json"));
        let snapshots = |window: &serde_json::Value| -> Vec<GpuSnapshot> {
            serde_json::from_value(window["snapshots"].clone()).unwrap()
        };
        let alone = replay
            .iter()
            .position(|window| window["name"] == "alone")
            .expect("an alone window");
        let config = mode_config(ContentionMode::Performance);
        let mut engine = PolicyEngine::new();
        let mut now = 0;
        let feed = |gpu: &GpuSnapshot, observation: &serde_json::Value| {
            let mut snap = snapshot(1000);
            snap.gpu_inventory = CapabilityStatus::Available;
            if let Some(observation) = &gpu.observation {
                snap.observed_at_unix_ms = observation.observed_at_unix_ms;
            }
            let mut principals = vec![PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                ..PrincipalUsage::default()
            }];
            for entry in observation["principals"].as_array().into_iter().flatten() {
                let uid = entry[0].as_u64().map(|uid| uid as u32);
                let milli = entry[1]
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if uid == Some(1000) {
                    continue;
                }
                principals.push(PrincipalUsage {
                    uid,
                    gpu_compute_milli: BTreeMap::from([(gpu.uuid.clone(), milli)]),
                    ..PrincipalUsage::default()
                });
            }
            snap.principals = Some(principals);
            snap.gpus = vec![gpu.clone()];
            snap
        };
        let observations = |i: usize| trace[i]["observations"].as_array().unwrap().clone();
        for (gpu, observation) in snapshots(&replay[alone]).iter().zip(observations(alone)) {
            let snap = feed(gpu, &observation);
            engine
                .evaluate(&config, &snap, &PolicyInput::default(), now)
                .unwrap();
            now += INTERVAL;
        }
        let gpus = snapshots(&replay[index]);
        let uuid = gpus[0].uuid.clone();
        let allocations: Vec<Allocation> = ["probe-1", "probe-2"]
            .iter()
            .map(|id| {
                let mut allocation = job(None);
                allocation.id = (*id).into();
                allocation.requested.gpu_memory_mib = BTreeMap::from([(uuid.clone(), 1000)]);
                let mut observed = allocation.requested.clone();
                observed.gpu_memory_mib = BTreeMap::from([(uuid.clone(), 100)]);
                allocation.observed = Some(observed);
                allocation
            })
            .collect();
        let input = PolicyInput {
            allocations,
            ..PolicyInput::default()
        };
        let key = ResourceKey::GpuCompute(uuid.clone());
        let (mut both, mut two, mut capped_shut, mut first_duty) = (None, 0, 0, None);
        let total = gpus.len();
        for (i, (gpu, observation)) in gpus.iter().zip(observations(index)).enumerate() {
            let listed = gpu.compute_contexts.as_ref().map_or(0, |c| c.own.len());
            let snap = feed(gpu, &observation);
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            now += INTERVAL;
            let status = &decision.resources[&key];
            let slice = status.time_slice.clone().expect("a time-sliced device");
            let shut = !status.gpu_demand.clone().unwrap_or_default().uncapped;
            if listed >= 2 {
                both.get_or_insert(i);
                assert_eq!(slice.own, 2, "decision {i}: {slice:?}");
                two += 1;
                capped_shut += usize::from(shut);
            }
            if first_duty.is_none()
                && decision
                    .actions
                    .iter()
                    .any(|a| a.resource == key && a.step == YieldStep::Cap)
            {
                first_duty = Some(i);
            }
        }
        println!(
            "replay window {index}: both own processes listed from decision {both:?}; m = 2 in {two} of {total} decisions, {capped_shut} of them with the feed shut; first duty at {first_duty:?}"
        );
        assert_eq!(
            first_duty, both,
            "the duty goes out from the first decision listing both"
        );
    }

    /// T368's Performance saturating run (036's evidence): the probe's reading per interval beside
    /// the saturating user without CedeGrid, 419 on average. Its slices come in whole samples, so
    /// one interval reads 0 and the next 990 at the same share.
    const SATURATING_FULL_DUTY: [u64; 60] = [
        0, 0, 0, 220, 330, 330, 390, 330, 330, 360, 660, 660, 330, 580, 660, 330, 440, 0, 0, 190,
        990, 0, 500, 990, 360, 0, 990, 300, 440, 990, 0, 580, 990, 0, 580, 520, 0, 990, 390, 440,
        0, 0, 250, 990, 330, 990, 0, 0, 990, 990, 0, 0, 990, 990, 0, 580, 990, 440, 0, 440,
    ];

    /// A time-sliced device: each busy context gets an equal slice of the time the
    /// device is busy. The other user, saturating, runs alone for 60 intervals, then own work
    /// runs `contexts(i)` contexts at the duty its last decision sent and holds duty × its slices,
    /// the other user the rest; own work's reading swings as the recorded pattern does around that
    /// share. Returns, per own interval, the duty sent, own work's share and whether a window ran.
    fn saturating_loop(
        mode: ContentionMode,
        contexts: &dyn Fn(u64) -> u64,
        intervals: u64,
    ) -> Vec<(u64, u64, bool)> {
        let config = mode_config(mode);
        let mut engine = PolicyEngine::new();
        let mut now = 0;
        for i in 0..60u64 {
            engine
                .evaluate(
                    &config,
                    &interval(None, 990, i + 1),
                    &PolicyInput::default(),
                    now,
                )
                .unwrap();
            now += INTERVAL;
        }
        let input = PolicyInput {
            allocations: vec![job(None)],
            ..PolicyInput::default()
        };
        // The recorded pattern's swing around its mean, at half its size, keeps every reading
        // below the device's and the readings' mean at the share.
        let mean = SATURATING_FULL_DUTY.iter().sum::<u64>() as i64 / 60;
        let (mut duty, mut out) = (1000u64, vec![]);
        for i in 0..intervals {
            let k = contexts(i);
            let share = duty * k / (k + 1);
            let swing = (SATURATING_FULL_DUTY[(i % 60) as usize] as i64 - mean) / 2;
            let own = (share as i64 + swing).clamp(0, 990) as u64;
            let snap = interval(Some(own), 990 - own, 61 + i);
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            duty = cap_of(&decision);
            out.push((duty, share, record_of(&decision).window));
            now += INTERVAL;
        }
        out
    }

    /// One own context beside a saturating user already holds its slice, the fair share, at full
    /// duty, so a cap at the fair share keeps a duty near 1 and own work at the unmanaged share,
    /// where a duty of target ÷ 1000 halved it (FR-021, Q20, R49).
    #[test]
    fn one_own_context_beside_a_saturating_user_keeps_its_slice() {
        for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
            let run = saturating_loop(mode, &|_| 1, 200);
            for (i, (duty, own, _)) in run.iter().enumerate().skip(20) {
                assert!(*duty >= 950, "{mode:?} interval {i}: duty {duty}");
                assert!(
                    own.abs_diff(500) <= 30,
                    "{mode:?} interval {i}: own work holds {own}"
                );
            }
        }
    }

    /// 039's light loop (FR-021, Q20): beside a light user own work holds the device less that
    /// user's slices at full duty, so a cap at capacity less the estimate and the margin leaves a
    /// duty near 1, where a duty of target ÷ 1000 left about 0.81.
    #[test]
    fn a_light_user_leaves_the_duty_near_full() {
        let light = |i: u64, capped: bool| {
            if capped {
                BESIDE_CAPS[(i % 60) as usize]
            } else {
                BESIDE_FULL_DUTY[(i % 60) as usize]
            }
        };
        let (_, decisions) = loop_decisions(ContentionMode::Performance, None, 600, &light);
        let duties: Vec<u64> = decisions[20..].iter().map(cap_of).collect();
        assert!(
            median(duties.clone()) >= 970,
            "duty median {}",
            median(duties.clone())
        );
        let low = *duties.iter().min().unwrap();
        assert!(low >= 950, "duty fell to {low}");
    }

    /// Across the two-decision rule and the windows the duty settles (FR-021, Q20): with two own
    /// contexts beside a saturating user in Neutral, every duty outside a window stays within
    /// 30 of their median.
    #[test]
    fn the_duty_settles_without_oscillating() {
        let run = saturating_loop(ContentionMode::Neutral, &|_| 2, 400);
        let settled: Vec<u64> = run[100..]
            .iter()
            .filter(|(_, _, window)| !window)
            .map(|(duty, _, _)| *duty)
            .collect();
        let middle = median(settled.clone());
        let far = settled
            .iter()
            .filter(|duty| duty.abs_diff(middle) > 30)
            .count();
        assert_eq!(
            far, 0,
            "{far} duties more than 30 from {middle}: {settled:?}"
        );
    }

    /// A time-sliced device, with its contexts listed and sampled: each busy context
    /// gets an equal slice of the time the device is busy (FR-021, Q22).
    mod time_slice {
        use super::*;
        use cedegrid::model::{GpuComputeContexts, SliceSource, TimeSliceRecord, YieldAction};
        use std::collections::BTreeSet;

        const OWN_PIDS: [u32; 3] = [100, 101, 102];
        const PEER_PIDS: [u32; 3] = [900, 901, 902];

        /// One measured interval on a time-sliced device: own work's first `own_n` processes and
        /// the other user's first `peer_n` listed, each side's sampled while it reads above zero.
        pub(super) fn sliced(
            own: Option<u64>,
            external: u64,
            sample: u64,
            own_n: usize,
            peer_n: usize,
        ) -> Snapshot {
            let mut snap = interval(own, external, sample);
            let gpu = &mut snap.gpus[0];
            gpu.time_sliced = Some(true);
            let (own_pids, peer) = (&OWN_PIDS[..own_n], &PEER_PIDS[..peer_n]);
            gpu.external_process_ids = Some(peer.to_vec());
            let mut sampled = vec![];
            if own.unwrap_or(0) > 0 {
                sampled.extend(own_pids.iter().map(|pid| (*pid, Some(1000))));
            }
            if external > 0 {
                sampled.extend(peer.iter().map(|pid| (*pid, Some(PEER))));
            }
            gpu.compute_contexts = Some(GpuComputeContexts {
                own: own_pids.to_vec(),
                owner: vec![],
                sampled,
                listed: peer.iter().map(|pid| (*pid, Some(PEER))).collect(),
                samples_readable: true,
                others_quiet: false,
            });
            snap
        }

        /// The saturating loop on contexts: the other user's `peer` processes alone for 60
        /// intervals, then own work's `own(i)` processes at the duty its last decision sent,
        /// holding duty × own ÷ (own + peer) of the device, its reading swinging as 040's model's.
        /// Returns each own interval's decision and own work's share.
        pub(super) fn saturating(
            mode: ContentionMode,
            own: &dyn Fn(u64) -> usize,
            peer: usize,
            intervals: u64,
        ) -> Vec<(Decision, u64)> {
            saturating_with(mode, None, own, peer, intervals, &|snap| snap)
        }

        /// `saturating` with the job in `job_mode` and each snapshot passed through `adjust`.
        pub(super) fn saturating_with(
            mode: ContentionMode,
            job_mode: Option<ContentionMode>,
            own: &dyn Fn(u64) -> usize,
            peer: usize,
            intervals: u64,
            adjust: &dyn Fn(Snapshot) -> Snapshot,
        ) -> Vec<(Decision, u64)> {
            let config = mode_config(mode);
            let mut engine = PolicyEngine::new();
            let mut now = 0;
            for i in 0..60u64 {
                let snap = adjust(sliced(None, 990, i + 1, 0, peer));
                engine
                    .evaluate(&config, &snap, &PolicyInput::default(), now)
                    .unwrap();
                now += INTERVAL;
            }
            let input = PolicyInput {
                allocations: vec![job(job_mode)],
                ..PolicyInput::default()
            };
            let mean = SATURATING_FULL_DUTY.iter().sum::<u64>() as i64 / 60;
            let (mut duty, mut out) = (1000u64, vec![]);
            for i in 0..intervals {
                let m = own(i);
                let share = duty * m as u64 / (m + peer) as u64;
                let swing = (SATURATING_FULL_DUTY[(i % 60) as usize] as i64 - mean) / 2;
                let reading = (share as i64 + swing).clamp(0, 990) as u64;
                let snap = adjust(sliced(Some(reading), 990 - reading, 61 + i, m, peer));
                let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
                duty = cap_of(&decision);
                out.push((decision, share));
                now += INTERVAL;
            }
            out
        }

        /// The light loop on contexts: the light user alone, then one own process beside it,
        /// reading its full-duty amounts while own work runs uncapped and its amounts under caps
        /// otherwise; own work takes the rest of the device within its cap.
        pub(super) fn light(mode: ContentionMode, intervals: u64) -> Vec<(Decision, u64)> {
            let config = mode_config(mode);
            let mut engine = PolicyEngine::new();
            let mut now = 0;
            for i in 0..60u64 {
                let snap = sliced(None, ALONE[i as usize], i + 1, 0, 1);
                engine
                    .evaluate(&config, &snap, &PolicyInput::default(), now)
                    .unwrap();
                now += INTERVAL;
            }
            let input = PolicyInput {
                allocations: vec![job(None)],
                ..PolicyInput::default()
            };
            let (mut caps, mut out) = ([1000u64, 1000u64], vec![]);
            for i in 0..intervals {
                let external = if caps.iter().any(|cap| *cap < 1000) {
                    BESIDE_CAPS[(i % 60) as usize]
                } else {
                    BESIDE_FULL_DUTY[(i % 60) as usize]
                };
                let own = caps[0].min(990 - external);
                let snap = sliced(Some(own), external, 61 + i, 1, 1);
                let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
                caps = [cap_of(&decision), caps[0]];
                out.push((decision, own));
                now += INTERVAL;
            }
            out
        }

        /// One interval of a two-task run: each live task's GPU compute step, the drains decided
        /// and own work's share of the device.
        pub(super) struct TaskTick {
            pub decision: Decision,
            pub steps: BTreeMap<String, YieldStep>,
            pub drains: Vec<String>,
            pub own: u64,
        }

        /// Run E9's shape: the other user's `peer` saturating contexts alone for 60 intervals, then
        /// own tasks "a" and "b", one context each, every busy context an equal slice of 990; a
        /// paused task's context stays listed, unsampled, and a drained task leaves.
        pub(super) fn two_tasks(
            config: &Config,
            job_mode: Option<ContentionMode>,
            peer: usize,
            intervals: u64,
        ) -> Vec<TaskTick> {
            let run = tasks_run(
                config,
                job_mode,
                &|i| i >= 60,
                &|_| peer,
                Some(PEER),
                60 + intervals,
            );
            run.into_iter().skip(60).collect()
        }

        /// A two-task run: `own_at(i)` says whether own tasks "a" and "b" are on the device in
        /// interval i, and `peer_at(i)` how many saturating contexts the other user, of
        /// `peer_uid`, has there. Every busy context gets an equal slice of 990; a paused task's
        /// context stays listed, unsampled, and a drained task leaves for good.
        pub(super) fn tasks_run(
            config: &Config,
            job_mode: Option<ContentionMode>,
            own_at: &dyn Fn(u64) -> bool,
            peer_at: &dyn Fn(u64) -> usize,
            peer_uid: Option<u32>,
            intervals: u64,
        ) -> Vec<TaskTick> {
            let mut engine = PolicyEngine::new();
            let tasks = [("a", OWN_PIDS[0]), ("b", OWN_PIDS[1])];
            let key = ResourceKey::GpuCompute(GPU.into());
            let (mut steps, mut gone): (BTreeMap<String, YieldStep>, BTreeSet<String>) =
                Default::default();
            let mut out = vec![];
            for i in 0..intervals {
                let peer = peer_at(i);
                let live: Vec<(&str, u32)> = if own_at(i) {
                    tasks
                        .iter()
                        .copied()
                        .filter(|(id, _)| !gone.contains(*id))
                        .collect()
                } else {
                    vec![]
                };
                let running: Vec<u32> = live
                    .iter()
                    .filter(|(id, _)| steps.get(*id).is_none_or(|s| *s < YieldStep::Pause))
                    .map(|(_, pid)| *pid)
                    .collect();
                let r = running.len() as u64;
                let own = 990 * r / (r + peer as u64).max(1);
                let reading = (!live.is_empty()).then_some(own);
                let external = if peer == 0 { 0 } else { 990 - own };
                let mut snap = sliced(reading, external, i + 1, live.len(), peer);
                if peer == 0 {
                    snap.gpus[0].external_compute = ComputeActivity::Idle;
                    if let Some(principals) = snap.principals.as_mut() {
                        principals.retain(|p| p.is_owner);
                    }
                }
                let contexts = snap.gpus[0].compute_contexts.as_mut().unwrap();
                contexts.own = live.iter().map(|(_, pid)| *pid).collect();
                contexts.sampled.retain(|(pid, _)| !OWN_PIDS.contains(pid));
                contexts
                    .sampled
                    .extend(running.iter().map(|pid| (*pid, Some(1000))));
                for (_, uid) in contexts
                    .listed
                    .iter_mut()
                    .chain(contexts.sampled.iter_mut())
                {
                    if *uid == Some(PEER) {
                        *uid = peer_uid;
                    }
                }
                let input = PolicyInput {
                    allocations: live
                        .iter()
                        .map(|(id, _)| {
                            let mut allocation = job(job_mode);
                            allocation.id = id.to_string();
                            allocation
                        })
                        .collect(),
                    ..PolicyInput::default()
                };
                let decision = engine
                    .evaluate(config, &snap, &input, i * INTERVAL)
                    .unwrap();
                steps = decision
                    .actions
                    .iter()
                    .filter(|a| a.resource == key)
                    .map(|a| (a.allocation_id.clone(), a.step))
                    .collect();
                gone.extend(decision.would_drain.iter().cloned());
                out.push(TaskTick {
                    drains: decision.would_drain.clone(),
                    steps: steps.clone(),
                    decision,
                    own,
                });
            }
            out
        }

        /// Run E9's shape in Neutral: two own tasks beside one saturating context. One task is
        /// paused, so own work keeps half of the device and the other user its fair share; no cap
        /// or duty is sent, GPU admission holds, the paused task drains at its bound and the other
        /// then runs unheld (FR-021, FR-089, SC-020).
        #[test]
        fn two_own_tasks_beside_a_saturating_one_pause_one_whole_task() {
            let run = two_tasks(&mode_config(ContentionMode::Neutral), None, 1, 60);
            for (i, t) in run.iter().enumerate() {
                assert!(
                    t.decision
                        .actions
                        .iter()
                        .all(|a| a.step != YieldStep::Cap && a.duty.is_none()),
                    "interval {i}: {:?}",
                    t.decision.actions
                );
            }
            for (i, t) in run.iter().enumerate().take(19).skip(2) {
                let paused = t.steps.values().filter(|s| **s == YieldStep::Pause).count();
                assert_eq!(paused, 1, "interval {i}: {:?}", t.steps);
                assert!(t.own >= 416, "interval {i}: own {}", t.own);
                let status = &t.decision.resources[&ResourceKey::GpuCompute(GPU.into())];
                assert_eq!(
                    status.admission.as_ref().and_then(|a| a.code()),
                    Some("gpu_time_slice_admission_held"),
                    "interval {i}"
                );
                assert_eq!(slice_of(&t.decision).hold, Some(1), "interval {i}");
            }
            let drains: Vec<(usize, String)> = run
                .iter()
                .enumerate()
                .flat_map(|(i, t)| t.drains.iter().map(move |id| (i, id.clone())))
                .collect();
            assert_eq!(drains.len(), 1, "{drains:?}");
            assert!(drains[0].0 >= 20, "{drains:?}");
            for (i, t) in run.iter().enumerate().skip(drains[0].0 + 1) {
                assert!(t.steps.is_empty(), "interval {i}: {:?}", t.steps);
            }
        }

        /// Yield First on a contended time-sliced device pauses every own task, with no cap
        /// (FR-020, FR-089).
        #[test]
        fn yield_first_pauses_every_own_task_on_a_contended_device() {
            let run = two_tasks(&mode_config(ContentionMode::YieldFirst), None, 1, 16);
            assert!(
                run.iter()
                    .all(|t| t.steps.values().all(|s| *s == YieldStep::Pause)),
                "{:?}",
                run.iter().map(|t| &t.steps).collect::<Vec<_>>()
            );
            assert!(run.iter().any(|t| t.steps.len() == 2));
        }

        /// In Performance no own task is held; GPU admission holds instead (FR-022).
        #[test]
        fn performance_holds_no_own_task_on_the_time_sliced_device() {
            let run = two_tasks(&mode_config(ContentionMode::Performance), None, 1, 30);
            assert!(run.iter().all(|t| t.steps.is_empty()));
        }

        /// The first-come record of the device (FR-090).
        pub(super) fn first_come(decision: &Decision) -> Option<String> {
            decision.resources[&ResourceKey::GpuCompute(GPU.into())]
                .gpu_demand
                .as_ref()
                .and_then(|record| record.first_come.clone())
        }

        /// Own tasks alone on the device for 20 intervals, then the other user arrives, in Yield
        /// First: own work was there first, so it keeps half the device, one of its two tasks
        /// running; the record says the floor applies (FR-090).
        #[test]
        fn yield_first_keeps_half_beside_a_user_that_arrived_later() {
            let config = mode_config(ContentionMode::YieldFirst);
            let run = tasks_run(
                &config,
                None,
                &|_| true,
                &|i| usize::from(i >= 20),
                Some(PEER),
                40,
            );
            for (i, t) in run.iter().enumerate().skip(24) {
                let paused = t.steps.values().filter(|s| **s == YieldStep::Pause).count();
                assert_eq!(paused, 1, "interval {i}: {:?}", t.steps);
                assert!(t.own >= 495, "interval {i}: own {}", t.own);
                assert_eq!(
                    first_come(&t.decision).as_deref(),
                    Some("applies"),
                    "interval {i}"
                );
            }
        }

        /// The floor is withheld where the order cannot be known or own work came later: an
        /// unreadable UID, or a user that arrived while own work was away for more than 30 s.
        /// Yield First then pauses both tasks (FR-090).
        #[test]
        fn no_floor_where_the_order_is_unknown_or_own_work_came_later() {
            let config = mode_config(ContentionMode::YieldFirst);
            let unreadable = tasks_run(
                &config,
                None,
                &|_| true,
                &|i| usize::from(i >= 20),
                None,
                40,
            );
            let back = tasks_run(
                &config,
                None,
                &|i| !(10..80).contains(&i),
                &|i| usize::from(i >= 30),
                Some(PEER),
                100,
            );
            for (run, from, why) in [
                (&unreadable, 24, "uid_unreadable"),
                (&back, 84, "own_later"),
            ] {
                for (i, t) in run.iter().enumerate().skip(from).take(14) {
                    let paused = t.steps.values().filter(|s| **s == YieldStep::Pause).count();
                    assert_eq!(paused, 2, "{why} interval {i}: {:?}", t.steps);
                    assert_eq!(
                        first_come(&t.decision).as_deref(),
                        Some(why),
                        "interval {i}"
                    );
                }
            }
        }

        pub(super) fn gpu_steps(decision: &Decision) -> Vec<&YieldAction> {
            let key = ResourceKey::GpuCompute(GPU.into());
            decision
                .actions
                .iter()
                .filter(|a| a.resource == key)
                .collect()
        }

        pub(super) fn slice_of(decision: &Decision) -> TimeSliceRecord {
            decision.resources[&ResourceKey::GpuCompute(GPU.into())]
                .time_slice
                .clone()
                .expect("a time-sliced device records the rule")
        }

        /// Own process 101 is listed from the first own interval but never sampled. It counts by
        /// its listing from that interval, so m = 2 at once and the other user gets its fair share
        /// (Principles II and III), for one counting window of readable intervals; then it stops
        /// counting, the rule holds and the cap ends, as for a principal that leaves (FR-021, item
        /// 053).
        #[test]
        fn a_listed_own_process_counts_from_its_listing_and_stops_after_one_window() {
            use cedegrid::model::{CountedBy, OwnCounted};
            let unsampled = |mut snap: Snapshot| {
                if let Some(contexts) = snap.gpus[0].compute_contexts.as_mut() {
                    contexts.sampled.retain(|(pid, _)| *pid != OWN_PIDS[1]);
                }
                snap
            };
            let run = saturating_with(ContentionMode::Neutral, None, &|_| 2, 1, 80, &unsampled);
            for (i, (decision, _)) in run.iter().enumerate() {
                let slice = slice_of(decision);
                if i < 60 {
                    assert_eq!(slice.own, 2, "interval {i}");
                    assert!(!slice.holds, "interval {i}");
                    assert_eq!(
                        slice.own_counted,
                        [
                            OwnCounted {
                                pid: OWN_PIDS[0],
                                by: CountedBy::Sample
                            },
                            OwnCounted {
                                pid: OWN_PIDS[1],
                                by: CountedBy::Listing
                            },
                        ],
                        "interval {i}"
                    );
                    // A lone job is not held, whatever its contexts (FR-089).
                    assert_eq!(slice.hold, Some(1), "interval {i}");
                    assert!(gpu_steps(decision).is_empty(), "interval {i}");
                } else {
                    assert_eq!(slice.own, 1, "interval {i}");
                    assert!(slice.holds, "interval {i}");
                    assert!(
                        gpu_steps(decision).is_empty(),
                        "interval {i}: {:?}",
                        gpu_steps(decision)
                    );
                }
                assert!(
                    gpu_steps(decision)
                        .iter()
                        .all(|a| a.step != YieldStep::Pause),
                    "interval {i}: no pause"
                );
            }
        }

        /// The counts come from every interval whose samples are readable, also while own work is
        /// held and no uncapped interval feeds the estimate: a second own process that starts
        /// computing while the job is paused counts at once (FR-021). Checked before the pause's
        /// bound.
        #[test]
        fn the_counts_update_while_own_work_is_held() {
            let run = saturating_with(
                ContentionMode::Neutral,
                Some(ContentionMode::YieldFirst),
                &|i| if i < 10 { 1 } else { 2 },
                1,
                19,
                &|snap| snap,
            );
            for (i, (decision, _)) in run.iter().enumerate().skip(5) {
                assert!(
                    !record_of(decision).uncapped,
                    "interval {i}: the feed is shut"
                );
                let want = if i < 10 { 1 } else { 2 };
                assert_eq!(slice_of(decision).own, want, "interval {i}");
            }
        }

        /// A listed context that is not own work, the owner's unmanaged one or another user's,
        /// counts only while sampled: unchanged (FR-021).
        #[test]
        fn a_listed_context_that_is_not_own_work_counts_only_when_sampled() {
            use cedegrid::model::{CountedBy, OwnCounted};
            const OWNER_PID: u32 = 300;
            const OTHER: u32 = 2002;
            let listed = |mut snap: Snapshot| {
                let gpu = &mut snap.gpus[0];
                gpu.external_process_ids
                    .as_mut()
                    .unwrap()
                    .extend([OWNER_PID, 950]);
                let contexts = gpu.compute_contexts.as_mut().unwrap();
                contexts.owner.push(OWNER_PID);
                contexts.listed.push((950, Some(OTHER)));
                snap
            };
            let run = saturating_with(ContentionMode::Performance, None, &|_| 1, 1, 200, &listed);
            for (i, (decision, _)) in run.iter().enumerate() {
                let slice = slice_of(decision);
                assert_eq!(
                    (slice.own, slice.external, slice.principals),
                    (1, Some(1), Some(1)),
                    "interval {i}"
                );
                assert!(slice.holds, "interval {i}");
                assert!(gpu_steps(decision).is_empty(), "interval {i}");
                assert_eq!(
                    slice.own_counted,
                    [OwnCounted {
                        pid: OWN_PIDS[0],
                        by: CountedBy::Sample
                    }],
                    "interval {i}"
                );
            }
        }

        /// One own context beside a saturating user already holds its slice, the fair share, so
        /// Performance and Neutral send it no GPU compute step and it keeps the unmanaged share;
        /// the record shows m = 1, E = 1 and k = 1 (FR-021, Q22).
        #[test]
        fn one_own_context_beside_a_saturating_user_is_not_capped() {
            for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
                let run = saturating(mode, &|_| 1, 1, 200);
                for (i, (decision, share)) in run.iter().enumerate() {
                    let steps = gpu_steps(decision);
                    assert!(steps.is_empty(), "{mode:?} interval {i}: {steps:?}");
                    assert_eq!(*share, 500, "{mode:?} interval {i}");
                }
                let slice = slice_of(&run[199].0);
                assert_eq!(
                    (slice.own, slice.external, slice.principals, slice.holds),
                    (1, Some(1), Some(1), true),
                    "{mode:?}"
                );
            }
        }

        /// Beside a light user one own context takes the rest of the device at full duty, and
        /// no step is sent either (FR-021, Q22).
        #[test]
        fn one_own_context_beside_a_light_user_is_not_capped() {
            for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
                let run = light(mode, 200);
                for (i, (decision, own)) in run.iter().enumerate() {
                    let steps = gpu_steps(decision);
                    assert!(steps.is_empty(), "{mode:?} interval {i}: {steps:?}");
                    let external = BESIDE_FULL_DUTY[i % 60];
                    assert_eq!(*own, 990 - external, "{mode:?} interval {i}");
                }
                assert!(slice_of(&run[199].0).holds, "{mode:?}");
            }
        }

        /// One job with two contexts beside one saturating context holds two slices of three, more
        /// than the fair share: the rule fails and the hold count is 1, but pausing the job would
        /// leave none running, so it is left alone, as plain Linux leaves it. Once one context
        /// leaves the device, the rule holds (FR-021, FR-089).
        #[test]
        fn one_job_with_two_contexts_is_not_held() {
            const LEAVE: usize = 300;
            let run = saturating(
                ContentionMode::Neutral,
                &|i| if (i as usize) < LEAVE { 2 } else { 1 },
                1,
                400,
            );
            for (i, (decision, share)) in run.iter().enumerate() {
                let steps = gpu_steps(decision);
                assert!(steps.is_empty(), "interval {i}: {steps:?}");
                let slice = slice_of(decision);
                if (100..LEAVE).contains(&i) {
                    assert_eq!((slice.own, slice.holds, slice.hold), (2, false, Some(1)));
                    assert_eq!(*share, 666, "interval {i}");
                }
                if i >= LEAVE {
                    assert_eq!((slice.own, slice.holds), (1, true), "interval {i}");
                }
            }
        }

        /// Two own contexts beside an external principal with two contexts hold half the device,
        /// its fair share: m × k = 2 ≤ E = 2, so there is no cap (FR-021, Q22).
        #[test]
        fn two_own_contexts_beside_two_external_ones_are_not_capped() {
            for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
                let run = saturating(mode, &|_| 2, 2, 200);
                for (i, (decision, share)) in run.iter().enumerate() {
                    let steps = gpu_steps(decision);
                    assert!(steps.is_empty(), "{mode:?} interval {i}: {steps:?}");
                    assert_eq!(*share, 500, "{mode:?} interval {i}");
                }
                let slice = slice_of(&run[199].0);
                assert_eq!(
                    (slice.own, slice.external, slice.principals, slice.holds),
                    (2, Some(2), Some(1), true),
                    "{mode:?}"
                );
            }
        }

        /// One interval beside a saturating user whose per-process samples leave `gap` of the
        /// device unattributed: the device is busy 990, the other user reads 990 − gap.
        fn gapped(gap: u64, sample: u64) -> Snapshot {
            let mut snap = interval(None, 990, sample);
            let principals = snap.principals.as_mut().unwrap();
            principals[1].gpu_compute_milli = BTreeMap::from([(GPU.to_string(), 990 - gap)]);
            snap
        }

        /// On GPU compute, a gap between the device and the per-process sum forms the
        /// unattributed principal only at the attribution tolerance, 15 points (FR-021, Q23):
        /// below it Neutral's fair share is two-way, at or above it three-way. Either way the gap
        /// counts in external demand.
        #[test]
        fn a_sampling_gap_below_the_tolerance_forms_no_principal() {
            let config = mode_config(ContentionMode::Neutral);
            for (gap, target) in [(100, 500), (140, 500), (150, 334), (200, 334)] {
                let mut engine = PolicyEngine::new();
                let mut last = None;
                for i in 0..60u64 {
                    let decision = engine
                        .evaluate(
                            &config,
                            &gapped(gap, i + 1),
                            &PolicyInput::default(),
                            i * INTERVAL,
                        )
                        .unwrap();
                    last = Some(decision);
                }
                let last = last.unwrap();
                assert_eq!(target_of(&last), target, "gap {gap}");
                assert_eq!(record_of(&last).estimate, Some(990), "gap {gap}");
            }
        }

        /// The owner's unmanaged usage is the owner principal's, not external demand: the
        /// interval in which an unmanaged own-account process exits leaves the estimate at the
        /// other user's amounts, as if the process had never run (FR-021, FR-059, Q23).
        #[test]
        fn the_owners_unmanaged_usage_stays_out_of_the_estimate() {
            let config = mode_config(ContentionMode::Neutral);
            let estimate = |owner: u64| {
                let mut engine = PolicyEngine::new();
                let mut last = None;
                for i in 0..40u64 {
                    // The exiting process in interval 20: the owner's amount beside the other
                    // user's 80.
                    let snap = if i == 20 {
                        let mut exit = interval(None, owner + 80, i + 1);
                        let principals = exit.principals.as_mut().unwrap();
                        principals[0].gpu_compute_milli =
                            BTreeMap::from([(GPU.to_string(), owner)]);
                        principals[1].gpu_compute_milli = BTreeMap::from([(GPU.to_string(), 80)]);
                        exit
                    } else {
                        interval(None, 160, i + 1)
                    };
                    let decision = engine
                        .evaluate(&config, &snap, &PolicyInput::default(), i * INTERVAL)
                        .unwrap();
                    assert!(
                        i != 20 || record_of(&decision).fed,
                        "the exit interval feeds"
                    );
                    last = Some(decision);
                }
                record_of(&last.unwrap()).estimate.unwrap()
            };
            assert_eq!(estimate(740), estimate(0));
        }

        /// The rule needs a device known to be time-sliced (FR-021, Q22): under MPS, or with the
        /// MPS or MIG state unknown, one own context beside a saturating user keeps today's caps
        /// and no time-slice record.
        #[test]
        fn mps_or_an_unknown_mode_keeps_the_measured_path() {
            for slicing in [Some(false), None] {
                let run = saturating_with(
                    ContentionMode::Neutral,
                    None,
                    &|_| 2,
                    1,
                    120,
                    &|mut snap| {
                        snap.gpus[0].time_sliced = slicing;
                        snap
                    },
                );
                let key = ResourceKey::GpuCompute(GPU.into());
                assert!(
                    run.iter()
                        .any(|(d, _)| d.resources[&key].target_share.is_some_and(|s| s < 1000)),
                    "{slicing:?}: the measured path sets a target"
                );
                assert!(
                    run.iter()
                        .all(|(d, _)| d.resources[&key].time_slice.is_none())
                );
            }
        }

        /// A background context takes slices but is no principal and is not counted (FR-021,
        /// Q22): beside one external context it leaves E = 1, so two own contexts are capped;
        /// a busy context of a UID below `uid_min` that is not background is left out too.
        #[test]
        fn a_background_context_is_not_counted() {
            for listed_as_background in [true, false] {
                let run =
                    saturating_with(ContentionMode::Neutral, None, &|_| 2, 1, 80, &|mut snap| {
                        let gpu = &mut snap.gpus[0];
                        gpu.external_process_ids.as_mut().unwrap().push(50);
                        if listed_as_background {
                            gpu.background_process_ids = vec![50];
                        }
                        gpu.compute_contexts
                            .as_mut()
                            .unwrap()
                            .sampled
                            .push((50, Some(0)));
                        snap
                    });
                let slice = slice_of(&run[79].0);
                assert_eq!(
                    (slice.own, slice.external, slice.principals, slice.holds),
                    (2, Some(1), Some(1), false),
                    "background listed {listed_as_background}"
                );
            }
        }

        /// An external process that leaves stops counting (FR-021, Q22): two own contexts beside
        /// two of the other user's hold the fair share without a cap; once one of the other
        /// user's processes leaves, E falls to 1 within one window and own work is capped.
        #[test]
        fn an_external_process_that_leaves_stops_counting() {
            const LEAVE: u64 = 100;
            let config = mode_config(ContentionMode::Neutral);
            let mut engine = PolicyEngine::new();
            let input = PolicyInput {
                allocations: vec![job(None)],
                ..PolicyInput::default()
            };
            let mut now = 0;
            for i in 0..60u64 {
                let snap = sliced(None, 990, i + 1, 0, 2);
                engine
                    .evaluate(&config, &snap, &PolicyInput::default(), now)
                    .unwrap();
                now += INTERVAL;
            }
            let (mut duty, mut out) = (1000u64, vec![]);
            for i in 0..200u64 {
                let peer = if i < LEAVE { 2 } else { 1 };
                let share = duty * 2 / (2 + peer as u64);
                let snap = sliced(Some(share), 990 - share, 61 + i, 2, peer);
                let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
                duty = cap_of(&decision);
                out.push(decision);
                now += INTERVAL;
            }
            for (i, decision) in out[..LEAVE as usize].iter().enumerate() {
                assert!(gpu_steps(decision).is_empty(), "interval {i}");
                assert!(slice_of(decision).holds, "interval {i}");
            }
            let after = &out[LEAVE as usize..LEAVE as usize + 60];
            assert!(
                after
                    .iter()
                    .any(|d| slice_of(d).external == Some(1) && !slice_of(d).holds),
                "E never fell"
            );
            assert!(
                after.iter().any(|d| slice_of(d).hold.is_some()),
                "never capped"
            );
        }

        /// With the amounts unknown from the start the estimate has no interval. While the
        /// samples are readable the interval's busy contexts count: one own process holds the
        /// rule, and with two the busy external principal is slice-bound at d0 = (2 + 1) ÷ 2 ×
        /// (½ − Neutral's 5%). With the samples unreadable the listing counts, and the rule holds
        /// only with at most one own process listed, since unseen external contexts can only add
        /// to E and k (FR-021, Q22, Q29, Q32, Q33).
        #[test]
        fn the_unknown_path_holds_the_rule_only_for_one_own_process() {
            let key = ResourceKey::GpuCompute(GPU.into());
            let config = mode_config(ContentionMode::Neutral);
            for (own_n, readable) in [(1, true), (2, true), (1, false), (2, false)] {
                let mut engine = PolicyEngine::new();
                let input = PolicyInput {
                    allocations: vec![job(None)],
                    ..PolicyInput::default()
                };
                let (mut last, mut held) = (None, None);
                for i in 0..200u64 {
                    let mut snap = sliced(Some(500), 490, i + 1, own_n, 1);
                    let gpu = &mut snap.gpus[0];
                    gpu.external_compute_milli = None;
                    gpu.own_compute_milli = None;
                    gpu.compute_amount_unknown =
                        Some("summed per-process utilization differs from the device".into());
                    gpu.compute_contexts.as_mut().unwrap().samples_readable = readable;
                    let decision = engine
                        .evaluate(&config, &snap, &input, i * INTERVAL)
                        .unwrap();
                    if own_n == 1 {
                        assert!(gpu_steps(&decision).is_empty(), "interval {i}");
                    }
                    assert!(
                        decision.actions.iter().all(|a| a.resource != key),
                        "interval {i}: a lone job is not held"
                    );
                    if slice_of(&decision).hold.is_some() {
                        held.get_or_insert(slice_of(&decision).hold);
                    }
                    last = Some(decision);
                }
                let slice = slice_of(&last.unwrap());
                let (counts, source) = if readable {
                    (Some(1), SliceSource::Interval)
                } else {
                    (None, SliceSource::Listing)
                };
                assert_eq!(
                    (slice.own, slice.external, slice.principals, slice.holds),
                    (own_n as u32, counts, counts, own_n == 1),
                    "readable {readable}"
                );
                assert_eq!(slice.source, Some(source), "readable {readable}");
                if own_n == 2 {
                    assert_eq!(
                        held,
                        Some(Some(1)),
                        "two own processes on the unknown path take the hold from the listed counts"
                    );
                    assert_eq!((slice.hold, slice.bound.len()), (Some(1), 1));
                }
            }
        }

        /// Yield First is unchanged by the rule (FR-020, FR-021, Q22): a Yield First job's GPU
        /// compute steps, targets and duties on a time-sliced device are those it gets on a
        /// device not known to be time-sliced, with the node in Yield First and in Performance.
        #[test]
        fn yield_first_keeps_its_targets_and_duties() {
            let steps = |decision: &Decision| {
                gpu_steps(decision)
                    .iter()
                    .map(|a| (a.step, a.target, a.duty))
                    .collect::<Vec<_>>()
            };
            for node in [ContentionMode::YieldFirst, ContentionMode::Performance] {
                let run = |slicing: Option<bool>| {
                    saturating_with(
                        node,
                        Some(ContentionMode::YieldFirst),
                        &|_| 1,
                        1,
                        200,
                        &move |mut snap| {
                            snap.gpus[0].time_sliced = slicing;
                            snap
                        },
                    )
                };
                let (sliced_run, plain) = (run(Some(true)), run(None));
                assert!(plain.iter().any(|(d, _)| !gpu_steps(d).is_empty()));
                for (i, ((a, _), (b, _))) in sliced_run.iter().zip(&plain).enumerate() {
                    assert_eq!(steps(a), steps(b), "{node:?} interval {i}");
                }
            }
        }

        /// A tick-level time-sliced device: each interval is 100 ticks. Own work's m
        /// contexts run in the first duty × 100 ticks; each external principal (UID, contexts,
        /// demand in thousandths of the device it would take alone) runs while it still wants
        /// time; each busy context takes an equal part of a tick. Returns own work's share, each
        /// principal's, and the device's busy percent.
        pub(super) fn tick(
            m: usize,
            duty: u64,
            externals: &[(u32, usize, u64)],
        ) -> (u64, Vec<u64>, u32) {
            let (mut own, mut got, mut busy) = (0f64, vec![0f64; externals.len()], 0u32);
            for t in 0..100u64 {
                let on = m > 0 && t * 10 < duty;
                let wanting: Vec<usize> = (0..externals.len())
                    .filter(|&j| got[j] * 10.0 + 1e-9 < externals[j].2 as f64)
                    .collect();
                let active =
                    if on { m } else { 0 } + wanting.iter().map(|&j| externals[j].1).sum::<usize>();
                if active == 0 {
                    continue;
                }
                busy += 1;
                if on {
                    own += m as f64 / active as f64;
                }
                for &j in &wanting {
                    got[j] += externals[j].1 as f64 / active as f64;
                }
            }
            let milli = |ticks: f64| (ticks * 10.0).round() as u64;
            (milli(own), got.iter().map(|g| milli(*g)).collect(), busy)
        }

        /// A per-process reading in whole samples (0, 330, 660 or 990), carrying its rounding so
        /// the readings average to the share, as NVML's do under time-slicing. A fixed-seed
        /// dither on the rounding keeps the pattern from repeating in step with the 30 s windows,
        /// which real readings do not.
        pub(super) struct Quantized(i64, u32);

        impl Default for Quantized {
            fn default() -> Self {
                Self(0, 0x9e37_79b9)
            }
        }

        impl Quantized {
            pub(super) fn read(&mut self, share: u64) -> u64 {
                self.1 ^= self.1 << 13;
                self.1 ^= self.1 >> 17;
                self.1 ^= self.1 << 5;
                let dither = i64::from(self.1 % 330) - 165;
                let want = share as i64 + self.0;
                let reading = ((want + 165 + dither).div_euclid(330) * 330).clamp(0, 990);
                self.0 = want - reading;
                reading as u64
            }
        }

        /// One interval on the tick device: own work's quantized reading and listed contexts, and
        /// each external principal's, with the device busy as the ticks say.
        pub(super) fn ticked(
            own: Option<u64>,
            m: usize,
            externals: &[(u32, usize, u64)],
            util: u32,
            sample: u64,
        ) -> Snapshot {
            let external: u64 = externals.iter().map(|e| e.2).sum();
            let mut snap = interval(own, external.min(1000), sample);
            let gpu = &mut snap.gpus[0];
            gpu.utilization_percent = Some(util);
            gpu.time_sliced = Some(true);
            let own_pids = &OWN_PIDS[..m];
            let pids = |uid: u32, n: usize| (0..n as u32).map(move |i| uid * 10 + i);
            gpu.external_process_ids =
                Some(externals.iter().flat_map(|e| pids(e.0, e.1)).collect());
            let mut sampled: Vec<(u32, Option<u32>)> = vec![];
            if own.unwrap_or(0) > 0 {
                sampled.extend(own_pids.iter().map(|pid| (*pid, Some(1000))));
            }
            for e in externals.iter().filter(|e| e.2 > 0) {
                sampled.extend(pids(e.0, e.1).map(|pid| (pid, Some(e.0))));
            }
            gpu.compute_contexts = Some(GpuComputeContexts {
                own: own_pids.to_vec(),
                owner: vec![],
                sampled,
                listed: externals
                    .iter()
                    .flat_map(|e| pids(e.0, e.1).map(move |pid| (pid, Some(e.0))))
                    .collect(),
                samples_readable: true,
                others_quiet: false,
            });
            let mut principals = vec![PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                ..PrincipalUsage::default()
            }];
            principals.extend(externals.iter().map(|e| PrincipalUsage {
                uid: Some(e.0),
                gpu_compute_milli: BTreeMap::from([(GPU.to_string(), e.2)]),
                ..PrincipalUsage::default()
            }));
            snap.principals = Some(principals);
            snap
        }

        /// One interval of the tick loop: the decision and own work's share.
        pub(super) struct Tick {
            pub(super) decision: Decision,
            pub(super) own: u64,
        }

        /// The tick loop: the external principals alone for 60 intervals, then own work's m(i)
        /// contexts at the duty each decision sends (0 while paused), every reading quantized.
        /// `externals(i)` gives each principal's UID, contexts and demand in interval i.
        pub(super) fn tick_loop(
            mode: ContentionMode,
            m_of: &dyn Fn(u64) -> usize,
            externals: &dyn Fn(u64) -> Vec<(u32, usize, u64)>,
            intervals: u64,
        ) -> Vec<Tick> {
            tick_loop_with(&mode_config(mode), m_of, externals, intervals, &|_, _| {})
        }

        /// Aggressive's GPU compute behavior: Performance with no margin, which is how a node
        /// configures it, `reduced_margin` acknowledged.
        pub(super) fn aggressive_config() -> Config {
            let mut config = mode_config(ContentionMode::Performance);
            config.contention.gpu_compute.performance_margin_percent = Some(0);
            config
                .contention
                .acknowledge_caution
                .insert("reduced_margin".into());
            config
        }

        /// `tick_loop` under a given configuration, each snapshot, the other users' alone ones
        /// included, passed through `adjust` with its index in its part of the run.
        pub(super) fn tick_loop_with(
            config: &Config,
            m_of: &dyn Fn(u64) -> usize,
            externals: &dyn Fn(u64) -> Vec<(u32, usize, u64)>,
            intervals: u64,
            adjust: &dyn Fn(u64, &mut Snapshot),
        ) -> Vec<Tick> {
            let config = config.clone();
            let mut engine = PolicyEngine::new();
            let mut now = 0;
            let mut quantized: BTreeMap<u32, Quantized> = BTreeMap::new();
            for i in 0..60u64 {
                let ext = externals(0);
                let (_, got, util) = tick(0, 0, &ext);
                let readings: Vec<(u32, usize, u64)> = ext
                    .iter()
                    .zip(&got)
                    .map(|(e, g)| (e.0, e.1, quantized.entry(e.0).or_default().read(*g)))
                    .collect();
                let mut snap = ticked(None, 0, &readings, util, i + 1);
                adjust(i, &mut snap);
                engine
                    .evaluate(&config, &snap, &PolicyInput::default(), now)
                    .unwrap();
                now += INTERVAL;
            }
            let input = PolicyInput {
                allocations: vec![job(None)],
                ..PolicyInput::default()
            };
            let (mut duty, mut own_reading, mut out) = (1000u64, Quantized::default(), vec![]);
            for i in 0..intervals {
                let (m, ext) = (m_of(i), externals(i));
                let (own, got, util) = tick(m, duty, &ext);
                let readings: Vec<(u32, usize, u64)> = ext
                    .iter()
                    .zip(&got)
                    .map(|(e, g)| (e.0, e.1, quantized.entry(e.0).or_default().read(*g)))
                    .collect();
                let mut snap = ticked(Some(own_reading.read(own)), m, &readings, util, 61 + i);
                adjust(i, &mut snap);
                let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
                duty = cap_of(&decision);
                out.push(Tick { decision, own });
                now += INTERVAL;
            }
            out
        }

        pub(super) fn paused(decision: &Decision) -> bool {
            gpu_steps(decision)
                .iter()
                .any(|a| a.step == YieldStep::Pause)
        }

        /// In Performance two own contexts beside a saturating one set no duty and open no
        /// re-measure window, so own work keeps the driver's slices, as beside the same work
        /// started by hand; GPU admission holds instead (FR-021, FR-022, R87).
        #[test]
        fn performance_sets_no_time_slice_duty_and_holds_gpu_admission() {
            let config = mode_config(ContentionMode::Performance);
            let run = tick_loop_with(&config, &|_| 2, &|_| vec![(PEER, 1, 1000)], 120, &|_, _| {});
            let (unmanaged, _, _) = tick(2, 1000, &[(PEER, 1, 1000)]);
            for (i, t) in run.iter().enumerate().skip(10) {
                let steps = gpu_steps(&t.decision);
                assert!(steps.is_empty(), "interval {i}: {steps:?}");
                assert_eq!(t.own, unmanaged, "interval {i}");
                assert!(
                    !record_of(&t.decision).window,
                    "interval {i}: a window opened"
                );
                let status = &t.decision.resources[&ResourceKey::GpuCompute(GPU.into())];
                assert_eq!(
                    status.admission.as_ref().and_then(|a| a.code()),
                    Some("gpu_time_slice_admission_held"),
                    "interval {i}"
                );
            }
        }

        /// Beside a light context, which reads well under its slice, no principal is slice-bound,
        /// so there is no cap and own work keeps what it takes without CedeGrid (FR-021, Q27).
        #[test]
        fn two_own_contexts_beside_a_light_one_are_not_capped() {
            for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
                let run = tick_loop(mode, &|_| 2, &|_| vec![(PEER, 1, 158)], 200);
                for (i, t) in run.iter().enumerate() {
                    let steps = gpu_steps(&t.decision);
                    assert!(steps.is_empty(), "{mode:?} interval {i}: {steps:?}");
                }
                let (unmanaged, _, _) = tick(2, 1000, &[(PEER, 1, 158)]);
                assert!(run.iter().all(|t| t.own == unmanaged), "{mode:?}");
                let reasons = &run.last().unwrap().decision.resources
                    [&ResourceKey::GpuCompute(GPU.into())]
                    .reasons;
                assert!(
                    reasons.iter().any(|r| r.code == "gpu_time_slice_unbound"),
                    "{mode:?}: {reasons:?}"
                );
            }
        }

        /// After the estimate restarts on a new external PID, its window is empty: the hold comes
        /// from the interval's sampled busy contexts, every counted principal bound (FR-021, Q29,
        /// Q33).
        #[test]
        fn an_empty_window_after_a_restart_takes_the_listed_counts() {
            const CHANGE: u64 = 61 + 120;
            let renamed = |mut snap: Snapshot| {
                let gpu = &mut snap.gpus[0];
                if gpu.compute_sample_id.is_some_and(|sample| sample >= CHANGE) {
                    let rename = |pid: &mut u32| {
                        if *pid == PEER_PIDS[0] {
                            *pid = 905;
                        }
                    };
                    gpu.external_process_ids
                        .iter_mut()
                        .flatten()
                        .for_each(rename);
                    let contexts = gpu.compute_contexts.as_mut().unwrap();
                    contexts.listed.iter_mut().for_each(|(pid, _)| rename(pid));
                    contexts.sampled.iter_mut().for_each(|(pid, _)| rename(pid));
                }
                snap
            };
            let run = saturating_with(ContentionMode::Neutral, None, &|_| 2, 1, 200, &renamed);
            let before = slice_of(&run[118].0);
            assert_eq!((before.external, before.hold), (Some(1), Some(1)));
            let after = slice_of(&run[120].0);
            assert_eq!(
                (
                    after.external,
                    after.principals,
                    after.hold,
                    after.bound.len()
                ),
                (Some(1), Some(1), Some(1), 1),
                "{after:?}"
            );
            // A lone job is not held, so the interval itself feeds the new estimate at once.
            assert!(after.source.is_some());
            assert!(run[120..].iter().all(|(d, _)| !paused(d)));
        }

        /// The owner's unmanaged processes are listed among the device's external PIDs, but their
        /// usage is already out of the estimate, so their coming and going keeps the average; a
        /// new external process still restarts it (FR-022, Q36).
        #[test]
        fn the_owners_processes_coming_and_going_keep_the_estimate() {
            const OWNER_PID: u32 = 300;
            let config = mode_config(ContentionMode::Performance);
            let mut engine = PolicyEngine::new();
            let mut intervals = vec![];
            for i in 0..120u64 {
                let mut snap = sliced(None, 490, i + 1, 0, 1);
                let gpu = &mut snap.gpus[0];
                if (40..70).contains(&i) {
                    gpu.external_process_ids.as_mut().unwrap().push(OWNER_PID);
                    gpu.compute_contexts.as_mut().unwrap().owner.push(OWNER_PID);
                }
                if i >= 100 {
                    gpu.external_process_ids = Some(vec![PEER_PIDS[1]]);
                    let contexts = gpu.compute_contexts.as_mut().unwrap();
                    contexts.listed = vec![(PEER_PIDS[1], Some(PEER))];
                    for (pid, _) in contexts.sampled.iter_mut() {
                        *pid = PEER_PIDS[1];
                    }
                }
                let decision = engine
                    .evaluate(&config, &snap, &PolicyInput::default(), i * INTERVAL)
                    .unwrap();
                intervals.push(record_of(&decision).intervals);
            }
            for i in [40, 70] {
                assert_eq!(
                    intervals[i],
                    intervals[i - 1] + 1,
                    "the owner's process at interval {i}: {intervals:?}"
                );
            }
            assert_eq!(
                intervals[100], 1,
                "a new external process restarts the average"
            );
        }

        /// An idle context of another principal, listed throughout but never sampled busy, does
        /// not count while the samples are readable, even in intervals whose amounts are
        /// inconsistent: the counts come from the busy contexts, the window's or, while every
        /// third interval cuts each re-measure window short, the interval's, and d0 stays the
        /// one-principal value, 720 in Performance. With the samples unreadable the listing
        /// counts it, and d0 is the two-principal value, 626 (FR-021, Q33).
        #[test]
        fn an_idle_listed_context_counts_only_when_the_samples_are_unreadable() {
            const IDLE: u32 = 950;
            let config = mode_config(ContentionMode::Performance);
            // Readable, the idle context is not counted and only the busy user is slice-bound;
            // from the listing both principals are.
            for (readable, bound, sources_wanted) in [
                (
                    true,
                    1,
                    BTreeSet::from([SliceSource::Window, SliceSource::Interval]),
                ),
                (false, 2, BTreeSet::from([SliceSource::Listing])),
            ] {
                let mut engine = PolicyEngine::new();
                let input = PolicyInput {
                    allocations: vec![job(None)],
                    ..PolicyInput::default()
                };
                let (mut sources, mut bounds) = (BTreeSet::new(), BTreeSet::new());
                for i in 0..240u64 {
                    let mut snap = sliced(Some(500), 490, i + 1, 2, 1);
                    let gpu = &mut snap.gpus[0];
                    gpu.external_process_ids.as_mut().unwrap().push(IDLE);
                    let contexts = gpu.compute_contexts.as_mut().unwrap();
                    contexts.listed.push((IDLE, Some(2002)));
                    contexts.samples_readable = readable;
                    if !readable || i % 3 == 0 {
                        gpu.external_compute_milli = None;
                        gpu.own_compute_milli = None;
                        gpu.compute_amount_unknown =
                            Some("summed per-process utilization differs from the device".into());
                    }
                    let decision = engine
                        .evaluate(&config, &snap, &input, i * INTERVAL)
                        .unwrap();
                    assert!(!paused(&decision), "readable {readable}, interval {i}");
                    if i >= 120 {
                        let slice = slice_of(&decision);
                        sources.extend(slice.source);
                        bounds.insert(slice.bound.len());
                    }
                }
                assert!(
                    sources.is_subset(&sources_wanted),
                    "readable {readable}: {sources:?}"
                );
                assert_eq!(bounds, BTreeSet::from([bound]), "readable {readable}");
            }
        }

        /// Beside a light user, with an idle context of another principal listed throughout,
        /// nothing is capped, also through the stretches after an interval with inconsistent
        /// amounts, while the device waits to read as measured again: the window's busy contexts
        /// and its readings still decide, and no principal is slice-bound (FR-021, Q27, Q33).
        #[test]
        fn beside_a_light_user_an_idle_listed_context_caps_nothing() {
            const IDLE: u32 = 950;
            let idle = |i: u64, snap: &mut Snapshot| {
                let gpu = &mut snap.gpus[0];
                gpu.external_process_ids.as_mut().unwrap().push(IDLE);
                let contexts = gpu.compute_contexts.as_mut().unwrap();
                contexts.listed.push((IDLE, Some(2002)));
                // The alone part has no own context, so only the own part has inconsistent
                // intervals.
                if contexts.own.len() == 2 && (i == 40 || i == 120) {
                    gpu.external_compute_milli = None;
                    gpu.own_compute_milli = None;
                    gpu.compute_amount_unknown =
                        Some("summed per-process utilization differs from the device".into());
                }
            };
            for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
                let run = tick_loop_with(
                    &mode_config(mode),
                    &|_| 2,
                    &|_| vec![(PEER, 1, 158)],
                    200,
                    &idle,
                );
                for (i, t) in run.iter().enumerate() {
                    let steps = gpu_steps(&t.decision);
                    assert!(steps.is_empty(), "{mode:?} interval {i}: {steps:?}");
                }
            }
        }

        /// Each decision on the duty records the slice-bound principals with their readings and
        /// thresholds, d0, the duty, the step and the utilization the step read (FR-021, Q30).
        #[test]
        fn the_record_states_the_slice_bound_principals_and_the_hold() {
            let run = tick_loop(
                ContentionMode::Neutral,
                &|_| 2,
                &|_| vec![(PEER, 1, 1000)],
                120,
            );
            let t = run[60..]
                .iter()
                .find(|t| slice_of(&t.decision).hold.is_some())
                .unwrap();
            let slice = slice_of(&t.decision);
            let [bound] = slice.bound.as_slice() else {
                panic!("{slice:?}");
            };
            assert_eq!(
                (bound.uid, bound.contexts, bound.threshold),
                (Some(PEER), 1, 250)
            );
            assert!(bound.reading >= bound.threshold, "{bound:?}");
            assert_eq!(
                (slice.own, slice.external, slice.principals, slice.holds),
                (2, Some(1), Some(1), false)
            );
            assert_eq!(
                (slice.hold, slice.d0, slice.duty, slice.step),
                (Some(1), None, None, None)
            );
            assert_eq!(slice.source, Some(SliceSource::Window));
            let reasons = &t.decision.resources[&ResourceKey::GpuCompute(GPU.into())].reasons;
            assert!(
                reasons.iter().any(|r| r.code == "gpu_time_slice_hold"),
                "{reasons:?}"
            );
        }
    }
}
