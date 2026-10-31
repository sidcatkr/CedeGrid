#[allow(dead_code)]
mod snapshot {
    include!("support/snapshot.rs");
}
use cedegrid::{
    config::*,
    execution_model::AllocationClass,
    kernel::{CpuCore, CpuTopology, CpuUsageStatus, KernelSnapshot},
    model::*,
    policy::PolicyEngine,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn config() -> Config {
    let mut result = Config::default();
    result.cpu.reserve_physical_cores = 1;
    result.ram.reserve_mib = 1000;
    result.ram.reserve_percent = 0;
    // These accounting tests run Neutral with a 10% margin; the default mode and
    // margins are tested in tests/contention.rs.
    result.contention.default_mode = cedegrid::contention::ContentionMode::Neutral;
    result.contention.cpu.neutral_margin_percent = Some(10);
    // A 1000 MiB floor is near the 512 MiB minimum; the tests confirm it.
    result
        .contention
        .acknowledge_caution
        .insert("small_floor".into());
    result.gpu.reserve_vram_mib = 1000;
    result.gpu.reserve_vram_percent = 0;
    result.gpu.scale_up_cooldown_ms = Some(0);
    scale_up_cooldowns(&mut result, 0);
    result
}

fn snapshot() -> Snapshot {
    Snapshot {
        schema_version: 1,
        node_id: "local".into(),
        observed_at_unix_ms: 100,
        cpu_capacity_millicores: 8000,
        physical_cores: Some(4),
        cpu_busy_millicores: Some(2000),
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

fn scoped_snapshot() -> Snapshot {
    let mut result = snapshot();
    result.schema_version = SCHEMA_VERSION;
    result.kernel = Some(KernelSnapshot {
        schema_version: 1,
        runtime_os: "linux".into(),
        kernel_release: Some("fixture".into()),
        observed_at_unix_ms: 100,
        monotonic_elapsed_ms: 500,
        sample_interval_ms: Some(500),
        freshness_limit_ms: 1500,
        controls: vec![],
        cpu: CpuTopology {
            allowed_cpu_ids: Some(vec![2, 3, 4, 5]),
            effective_cgroup_cpu_ids: Some(vec![2, 3, 4, 5]),
            effective_cpu_ids: Some(vec![2, 3, 4, 5]),
            cores: (2..=5)
                .map(|id| CpuCore {
                    cpu_id: id,
                    package_id: Some(0),
                    core_id: Some(id as i32),
                    thread_siblings: Some(vec![id]),
                })
                .collect(),
            effective_cpu_capacity_millicores: Some(4000),
            busy_millicores: Some(1000),
            busy_lower_millicores: None,
            busy_status: CpuUsageStatus::Available,
            busy_interval_ms: Some(500),
            busy_window_millicores: None,
            busy_window_lower_millicores: None,
            busy_window_ms: None,
            visible_cpu_ceiling_millicores: Some(4000),
            detail: "synthetic scoped telemetry".into(),
        },
        hierarchy: None,
        psi: vec![],
        cpu_pressure: None,
        limitations: vec![],
    });
    result
}

#[test]
fn scoped_cpu_busy_is_never_replaced_by_whole_host_busy() {
    let mut snap = scoped_snapshot();
    snap.cpu_busy_millicores = Some(7000);
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2000);
    assert!(decision.expansion_allowed);
    // A missing host aggregate does not invalidate independently complete scope telemetry.
    snap.cpu_busy_millicores = None;
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2000);
}

#[test]
fn ancestor_quota_clamps_after_matching_scope_usage_and_reserve() {
    let mut snap = scoped_snapshot();
    snap.cpu_busy_millicores = Some(7000);
    snap.kernel
        .as_mut()
        .unwrap()
        .cpu
        .visible_cpu_ceiling_millicores = Some(1500);
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    // 4000 hardware - 1000 core reserve - 1000 scoped busy = 2000, clamped to 1500.
    assert_eq!(decision.managed_budget.cpu_millicores, 1500);
    let input = PolicyInput {
        allocations: vec![allocation(
            "pending",
            AllocationPhase::Pending,
            resources(500, 0, 0),
            None,
        )],
        ..Default::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 1000);
}

/// An ancestor ceiling below the scoped capacity is held back as its own cause,
/// never as contention, and doctor shows it on its own line (SC-016).
#[test]
fn held_back_shows_an_ancestor_ceiling_apart_from_contention() {
    use cedegrid::contention::ResourceKey;
    let mut snap = scoped_snapshot();
    let cpu = &mut snap.kernel.as_mut().unwrap().cpu;
    cpu.busy_millicores = Some(0);
    cpu.visible_cpu_ceiling_millicores = Some(2000);
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2000);
    let held = decision.resources[&ResourceKey::Cpu]
        .held_back
        .clone()
        .unwrap();
    assert_eq!(held.contention, 0);
    // 4000 m scoped - 1000 m core reserve = 3000 m, of which the ceiling takes 1000 m.
    assert_eq!((held.reserve, held.ceiling), (1000, 1000));
    assert_eq!(held.capacity, 4000);
    assert_eq!(held.percent["ceiling"], Percent(25.0));
    let text = cedegrid::explain::doctor_text(&config(), &snap, &decision, &[]);
    let cpu_section = text
        .split("\ncpu")
        .nth(1)
        .expect("doctor has a CPU section");
    assert!(
        cpu_section.contains("  ceiling: 1000 m (25.0%)"),
        "{cpu_section}"
    );
    let held_line = cpu_section
        .lines()
        .find(|line| line.starts_with("  held back:"))
        .unwrap();
    assert!(
        !held_line.contains("ceiling") && !held_line.contains("contention"),
        "{held_line}"
    );
}

#[test]
fn scoped_observed_usage_and_reservations_are_not_double_charged() {
    let snap = scoped_snapshot();
    let input = PolicyInput {
        allocations: vec![allocation(
            "managed",
            AllocationPhase::Running,
            resources(1000, 0, 0),
            Some(resources(750, 0, 0)),
        )],
        ..Default::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2750);
    // One observation is no full usage window: the request is charged once.
    assert_eq!(decision.admission_headroom.cpu_millicores, 1750);
}

#[test]
fn physical_core_reserve_uses_effective_smt_groups_not_host_average() {
    let mut snap = scoped_snapshot();
    let cpu = &mut snap.kernel.as_mut().unwrap().cpu;
    cpu.effective_cpu_ids = Some(vec![2, 3, 4]);
    cpu.effective_cpu_capacity_millicores = Some(3000);
    cpu.visible_cpu_ceiling_millicores = Some(3000);
    cpu.busy_millicores = Some(0);
    cpu.cores.retain(|core| core.cpu_id != 5);
    cpu.cores[1].core_id = cpu.cores[0].core_id;
    // Two effective threads from one physical core plus one thread from another.
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 1000);
    cpu_missing_topology_blocks_unless_no_physical_reserve(snap);
}

fn cpu_missing_topology_blocks_unless_no_physical_reserve(mut snap: Snapshot) {
    snap.kernel.as_mut().unwrap().cpu.cores.clear();
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 0);
    assert!(!decision.expansion_allowed);
    let mut cfg = config();
    cfg.cpu.reserve_physical_cores = 0;
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2700);
}

#[test]
fn unavailable_inconsistent_or_stale_scoped_cpu_never_falls_back_to_idle_host() {
    for failure in [
        "missing",
        "stale",
        "warmup",
        "no_interval",
        "scope",
        "capacity",
        "cpuset",
        "duplicate",
        "old_interval",
    ] {
        let mut snap = scoped_snapshot();
        snap.cpu_busy_millicores = Some(0);
        let cpu = &mut snap.kernel.as_mut().unwrap().cpu;
        match failure {
            "missing" => cpu.busy_millicores = None,
            "stale" => cpu.busy_status = CpuUsageStatus::Stale,
            "warmup" => cpu.busy_status = CpuUsageStatus::Baseline,
            "no_interval" => cpu.busy_interval_ms = Some(0),
            "scope" => cpu.effective_cpu_ids = None,
            "capacity" => cpu.effective_cpu_capacity_millicores = Some(8000),
            "cpuset" => cpu.allowed_cpu_ids = Some(vec![0, 1]),
            "duplicate" => cpu.effective_cpu_ids = Some(vec![2, 2, 4, 5]),
            "old_interval" => cpu.busy_interval_ms = Some(2000),
            _ => unreachable!(),
        }
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .unwrap();
        assert_eq!(decision.managed_budget.cpu_millicores, 0, "{failure}");
        assert!(!decision.expansion_allowed, "{failure}");
    }
}

#[test]
fn replayed_scoped_snapshots_expire_without_comparing_unrelated_clock_origins() {
    let mut snap = scoped_snapshot();
    let mut engine = PolicyEngine::new();
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 10_000)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 11_500)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        !engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 11_501)
            .unwrap()
            .expansion_allowed
    );
    snap.kernel.as_mut().unwrap().monotonic_elapsed_ms = 1000;
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 11_502)
            .unwrap()
            .expansion_allowed
    );
    snap.kernel.as_mut().unwrap().monotonic_elapsed_ms = 100;
    assert!(
        !engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 11_503)
            .unwrap()
            .expansion_allowed
    );
    snap.kernel.as_mut().unwrap().monotonic_elapsed_ms = 600;
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 11_504)
            .unwrap()
            .expansion_allowed
    );
}

#[test]
fn non_linux_and_explicitly_disabled_diagnostics_preserve_legacy_scope() {
    let mut snap = scoped_snapshot();
    snap.kernel.as_mut().unwrap().runtime_os = "macos".into();
    snap.kernel.as_mut().unwrap().cpu.busy_status = CpuUsageStatus::Unknown;
    assert_eq!(
        PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .unwrap()
            .managed_budget
            .cpu_millicores,
        4000
    );
    snap.kernel.as_mut().unwrap().runtime_os = "linux".into();
    let mut cfg = config();
    cfg.kernel.enabled = false;
    assert_eq!(
        PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .managed_budget
            .cpu_millicores,
        4000
    );
}

fn gpu_snapshot() -> Snapshot {
    let mut result = snapshot();
    result.gpu_inventory = CapabilityStatus::Available;
    result.gpus.push(GpuSnapshot {
        uuid: "GPU-test".into(),
        total_memory_mib: 16_000,
        used_memory_mib: Some(6000),
        utilization_percent: Some(95),
        external_process_ids: Some(vec![]),
        external_compute: ComputeActivity::Idle,
        compute_sample_id: Some(1),
        observation: None,
        reserved_memory_mib: None,
        free_memory_mib: None,
        unattributed_memory_mib: None,
        external_compute_milli: None,
        own_compute_milli: None,
        compute_amount_unknown: None,
        own_only: false,
        background_process_ids: vec![],
        compute_sampling: None,
    });
    result
}

/// A config whose GPU compute runs in Yield First, where external activity holds
/// GPU admission (FR-026).
fn yield_first_gpu() -> Config {
    let mut result = config();
    result.contention.gpu_compute.mode = Some(cedegrid::contention::ContentionMode::YieldFirst);
    result
}

/// The CPU/RAM scale-up cooldown, independent of the GPU one since audit M1.
fn scale_up_cooldowns(config: &mut Config, ms: u64) {
    config.contention.cpu.scale_up_cooldown_ms = Some(ms);
    config.contention.ram.scale_up_cooldown_ms = Some(ms);
}

fn resources(cpu: u64, ram: u64, gpu: u64) -> Resources {
    Resources {
        cpu_millicores: cpu,
        ram_mib: ram,
        gpu_memory_mib: if gpu == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([("GPU-test".into(), gpu)])
        },
    }
}

/// Facts for plain commands on rootless backends: CPU lower share is unavailable
/// at runtime, and without the SDK there is no hint.
fn rootless(ids: &[&str]) -> BTreeMap<String, AllocationFacts> {
    use cedegrid::contention::{ResourceKey, YieldStep};
    ids.iter()
        .map(|id| {
            let facts = AllocationFacts {
                unavailable: BTreeMap::from([(
                    ResourceKey::Cpu,
                    std::collections::BTreeSet::from([YieldStep::LowerShare, YieldStep::Hint]),
                )]),
                ..Default::default()
            };
            (id.to_string(), facts)
        })
        .collect()
}

fn allocation(
    id: &str,
    phase: AllocationPhase,
    requested: Resources,
    observed: Option<Resources>,
) -> Allocation {
    Allocation {
        id: id.into(),
        class: AllocationClass::Opportunistic,
        phase,
        requested,
        observed,
        observed_upper: None,
        observed_window_cpu: None,
        modes: Default::default(),
    }
}

#[test]
fn gpu_yield_keeps_guaranteed_charges_and_selects_the_opportunistic_victim() {
    for (phase, observed) in [
        (AllocationPhase::Pending, None),
        (AllocationPhase::Running, Some(resources(0, 0, 1000))),
        // Retained uncertain executions are Running with unknown observations.
        (AllocationPhase::Running, None),
    ] {
        let unobserved = observed.is_none();
        let mut guaranteed = allocation("a-guaranteed", phase, resources(0, 0, 1000), observed);
        guaranteed.class = AllocationClass::Guaranteed;
        let input = PolicyInput {
            allocations: vec![
                guaranteed,
                allocation(
                    "b-opportunistic",
                    AllocationPhase::Running,
                    resources(0, 0, 1000),
                    Some(resources(0, 0, 1000)),
                ),
            ],
            ..Default::default()
        };
        // External memory leaves 1500 MiB for 2000 MiB of charges. Own usage, observed
        // or charged while unknown, is never counted as external.
        let own = 1000
            + if phase == AllocationPhase::Running {
                1000
            } else {
                0
            };
        let mut snap = gpu_snapshot();
        snap.gpus[0].used_memory_mib = Some(13_500 + own);
        snap.gpus[0].external_compute = ComputeActivity::Unknown;
        // An own observation unknown leaves the device's memory unknown: its steps hold
        // through the unknown window first (FR-008).
        let mut engine = PolicyEngine::new();
        let mut decision = engine.evaluate(&config(), &snap, &input, 0).unwrap();
        if unobserved && phase == AllocationPhase::Running {
            assert!(decision.would_drain.is_empty(), "held at first");
            decision = engine.evaluate(&config(), &snap, &input, 1500).unwrap();
        }
        assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 1500);
        // With the Guaranteed reservation pending, 1500 MiB stay free above the
        // 1000 MiB floor: a charge-only shortfall that steps nothing.
        let expected: &[&str] = if phase == AllocationPhase::Pending {
            &[]
        } else {
            &["b-opportunistic"]
        };
        assert_eq!(decision.would_drain, expected);
        assert!(decision.admission_headroom.gpu_memory_mib.is_empty());
        assert!(
            !decision.expansion_allowed,
            "class protection never bypasses Unknown admission"
        );
    }
}

#[test]
fn cpu_yield_does_not_plan_an_ignored_guaranteed_release() {
    let mut guaranteed = allocation(
        "a-guaranteed",
        AllocationPhase::Running,
        resources(1000, 0, 0),
        Some(resources(1000, 0, 0)),
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let input = PolicyInput {
        allocations: vec![
            guaranteed,
            allocation(
                "b-opportunistic",
                AllocationPhase::Running,
                resources(1000, 0, 0),
                Some(resources(1000, 0, 0)),
            ),
        ],
        facts: rootless(&["a-guaranteed", "b-opportunistic"]),
        ..Default::default()
    };
    // External work uses 5000m, leaving 1000m for 2000m of own usage.
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(7000);
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 1000);
    // CPU excess pauses running work, since its cap target is zero, instead of
    // draining it; Guaranteed work within its request is not planned.
    assert!(decision.would_drain.is_empty());
    assert_eq!(decision.actions.len(), 1);
    assert_eq!(decision.actions[0].allocation_id, "b-opportunistic");
    assert_eq!(
        decision.actions[0].step,
        cedegrid::contention::YieldStep::Pause
    );
    assert_eq!(decision.admission_headroom.cpu_millicores, 0);
}

#[test]
fn guaranteed_pending_and_uncertain_capacity_is_never_free_headroom() {
    let mut guaranteed = allocation(
        "guaranteed",
        AllocationPhase::Pending,
        resources(1000, 2000, 0),
        None,
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let mut input = PolicyInput {
        allocations: vec![guaranteed],
        ..Default::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 3000);
    assert_eq!(decision.admission_headroom.ram_mib, 23000);
    assert!(decision.would_drain.is_empty());
    input.allocations[0].phase = AllocationPhase::Running;
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 0);
    assert_eq!(decision.admission_headroom.ram_mib, 0);
    assert!(decision.would_drain.is_empty());
    assert!(!decision.expansion_allowed);
}

#[test]
fn explicit_node_drain_includes_guaranteed_and_opportunistic_work() {
    let mut guaranteed = allocation(
        "guaranteed",
        AllocationPhase::Pending,
        resources(1000, 2000, 0),
        None,
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let input = PolicyInput {
        allocations: vec![
            guaranteed,
            allocation(
                "opportunistic",
                AllocationPhase::Running,
                resources(1000, 1000, 0),
                Some(Resources::default()),
            ),
        ],
        explicit_drain: true,
        facts: Default::default(),
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.would_drain, ["guaranteed", "opportunistic"]);
    assert_eq!(decision.admission_headroom, Resources::default());
}

#[test]
fn old_policy_recordings_keep_their_opportunistic_default() {
    let old = serde_json::json!({"id":"old", "phase":"running",
        "requested":{"cpu_millicores":1000}, "observed":null});
    let mut decoded: Allocation = serde_json::from_value(old).unwrap();
    assert_eq!(decoded.class, AllocationClass::Opportunistic);
    decoded.class = AllocationClass::Guaranteed;
    let encoded = serde_json::to_value(&decoded).unwrap();
    assert_eq!(encoded["class"], "guaranteed");
    assert_eq!(
        serde_json::from_value::<Allocation>(encoded).unwrap().class,
        AllocationClass::Guaranteed
    );
}

#[test]
fn configuration_defaults_and_partial_overrides_are_portable() {
    let defaults = Config::default();
    assert_eq!(defaults.node_id, "local");
    assert!(!defaults.state_dir.is_absolute());
    assert_eq!(defaults.lifecycle.drain_timeout_ms, 3000);
    let configured: Config = serde_yaml::from_str("node_id: portable-node\nlifecycle:\n  drain_timeout_ms: 127\n  term_grace_ms: 731\nmonitor:\n  interval_ms: 37\n").unwrap();
    configured.validate().unwrap();
    assert_eq!(configured.lifecycle.drain_timeout_ms, 127);
    assert_eq!(configured.lifecycle.term_grace_ms, 731);
    assert_eq!(configured.lifecycle.allocation_lease_ms, 10_000);
    assert_eq!(configured.monitor.interval_ms, 37);
}

#[test]
fn configuration_rejects_unknown_fields_at_each_level() {
    for yaml in [
        "ssh_host: example",
        "gpu:\n  utilization_threshold: 80",
        "lifecycle:\n  drain_timeuot_ms: 3",
    ] {
        assert!(serde_yaml::from_str::<Config>(yaml).is_err(), "{yaml}");
    }
}

#[test]
fn configuration_rejects_invalid_ranges_and_deadline_overflow() {
    let mut value = config();
    value.monitor.interval_ms = 0;
    assert!(value.validate().is_err());
    value.monitor.interval_ms = 1;
    value.gpu.active_shrink_percent = 101;
    assert!(value.validate().is_err());
    value.gpu.active_shrink_percent = 50;
    value.lifecycle.drain_timeout_ms = u64::MAX;
    assert!(value.validate().is_err());
    value.lifecycle.drain_timeout_ms = 0;
    value.lifecycle.allocation_lease_ms = value.lifecycle.heartbeat_interval_ms;
    assert!(value.validate().is_err());
}

#[test]
fn configuration_load_validates_and_resolves_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "config_version = 1\nstate_dir = 'state'\nnode_mode = 'guaranteed'\n",
    )
    .unwrap();
    let result = Config::load(&path).unwrap();
    assert_eq!(
        result.state_dir,
        dir.path().canonicalize().unwrap().join("state")
    );
    assert_eq!(result.node_mode, NodeMode::Guaranteed);
    std::fs::write(&path, "config_version = 2000").unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn running_reservation_and_observation_are_charged_once() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(3000, 5000, 0),
            Some(resources(1000, 4000, 0)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    // CPU external = 2000 - 1000, SMT reserve = 2000, larger than the 800 Neutral margin, budget = 5000.
    assert_eq!(decision.managed_budget.cpu_millicores, 5000);
    // One observation is no full usage window: max(request, observed) is charged once.
    assert_eq!(decision.admission_headroom.cpu_millicores, 2000);
    // Used RAM 6000 - own 4000 = external 2000; budget 29000 - charge 5000.
    assert_eq!(decision.managed_budget.ram_mib, 29_000);
    assert_eq!(decision.admission_headroom.ram_mib, 24_000);
    assert!(decision.would_drain.is_empty());
}

#[test]
fn observed_usage_above_reservation_is_fully_charged() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(500, 1000, 0),
            Some(resources(1000, 4000, 0)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 4000);
    assert_eq!(decision.admission_headroom.ram_mib, 25_000);
}

#[test]
fn pending_reservations_reduce_headroom_before_launch() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "pending",
            AllocationPhase::Pending,
            resources(1500, 2000, 0),
            None,
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 2500);
    assert_eq!(decision.admission_headroom.ram_mib, 23_000);
}

#[test]
fn already_draining_resources_are_not_released_by_recommendation() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "draining",
            AllocationPhase::Draining,
            resources(3000, 5000, 0),
            Some(resources(1000, 4000, 0)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.admission_headroom.cpu_millicores, 2000);
    assert!(decision.would_drain.is_empty());
}

#[test]
fn existing_drain_does_not_trigger_unnecessary_additional_drain() {
    let input = PolicyInput {
        allocations: vec![
            allocation(
                "leaving",
                AllocationPhase::Draining,
                resources(3000, 0, 0),
                Some(Resources::default()),
            ),
            allocation(
                "healthy",
                AllocationPhase::Running,
                resources(3000, 0, 0),
                Some(Resources::default()),
            ),
        ],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert!(decision.would_drain.is_empty());
    // Both stay charged at their reservations: the running one has no full usage
    // window yet.
    assert_eq!(decision.admission_headroom.cpu_millicores, 0);
}

#[test]
fn gpu_accounting_does_not_double_subtract_owned_usage() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "gpu-worker",
            AllocationPhase::Running,
            resources(1000, 1000, 8000),
            Some(resources(500, 1000, 6000)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &gpu_snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 15_000);
    assert_eq!(decision.admission_headroom.gpu_memory_mib["GPU-test"], 7000);
    assert!(decision.would_drain.is_empty());
}

#[test]
fn own_high_gpu_utilization_is_not_external_contention() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![321]);
    snap.gpus[0].used_memory_mib = Some(7000);
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 6000)),
        )],
        ..PolicyInput::default()
    };
    let mut engine = PolicyEngine::new();
    // The first observation of a device is a baseline (FR-004).
    let first = engine.evaluate(&config(), &snap, &input, 0).unwrap();
    assert!(first.would_drain.is_empty());
    // A new external process later is protective irrespective of utilization: GPU
    // admission stops, and own work keeps running while its charge still fits.
    snap.gpus[0].external_process_ids = Some(vec![321, 654]);
    snap.gpus[0].compute_sample_id = Some(2);
    let discovered = engine.evaluate(&config(), &snap, &input, 1).unwrap();
    assert!(discovered.admission_headroom.gpu_memory_mib.is_empty());
    assert!(!discovered.expansion_allowed);
    assert!(discovered.would_drain.is_empty());
    snap.gpus[0].compute_sample_id = Some(3);
    let second = engine.evaluate(&config(), &snap, &input, 2).unwrap();
    assert!(second.would_drain.is_empty());
    assert!(second.expansion_allowed);
    assert_eq!(second.managed_budget.gpu_memory_mib["GPU-test"], 14_000);
}

#[test]
fn low_total_utilization_does_not_override_external_compute_activity() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].utilization_percent = Some(10);
    snap.gpus[0].external_process_ids = Some(vec![321]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 5000)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    // Activity, not total utilization, decides: the device is contended, so own GPU
    // work gets half of it. Without a memory breach nothing is drained.
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 14_000);
    assert!(decision.would_drain.is_empty());
    let share = decision
        .actions
        .iter()
        .find(|a| a.resource == "gpu_compute:GPU-test".parse().unwrap())
        .map(|a| a.target);
    assert_eq!(share, Some(Some(500)));
    // Yield First admits only on a quiet device.
    let held = PolicyEngine::new()
        .evaluate(&yield_first_gpu(), &snap, &input, 0)
        .unwrap();
    assert!(held.admission_headroom.gpu_memory_mib.is_empty());
    assert!(!held.expansion_allowed);
}

#[test]
fn repeated_driver_sample_does_not_compound_shrink_or_prove_idle() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![321]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 6000)),
        )],
        ..PolicyInput::default()
    };
    let cfg = yield_first_gpu();
    let mut engine = PolicyEngine::new();
    let first = engine.evaluate(&cfg, &snap, &input, 0).unwrap();
    let second = engine.evaluate(&cfg, &snap, &input, 500).unwrap();
    assert_eq!(
        first.managed_budget.gpu_memory_mib,
        second.managed_budget.gpu_memory_mib
    );
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    let stale_idle = engine.evaluate(&cfg, &snap, &input, 90_000).unwrap();
    assert!(!stale_idle.expansion_allowed);
}

#[test]
fn out_of_order_compute_sample_cannot_prove_recovery() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].compute_sample_id = Some(30);
    snap.gpus[0].external_process_ids = Some(vec![321]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let cfg = yield_first_gpu();
    let mut engine = PolicyEngine::new();
    engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    snap.gpus[0].compute_sample_id = Some(29);
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    assert!(
        !engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), 1)
            .unwrap()
            .expansion_allowed
    );
    snap.gpus[0].compute_sample_id = Some(30);
    assert!(
        !engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), 2)
            .unwrap()
            .expansion_allowed
    );
    snap.gpus[0].compute_sample_id = Some(31);
    assert!(
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), 3)
            .unwrap()
            .expansion_allowed
    );
}

#[test]
fn unknown_external_activity_blocks_expansion_and_holds() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 6000)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&yield_first_gpu(), &snap, &input, 0)
        .unwrap();
    // An unknown measurement first holds the current step (FR-008).
    assert!(decision.would_drain.is_empty() && decision.actions.is_empty());
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 15_000);
    assert!(decision.admission_headroom.gpu_memory_mib.is_empty());
    assert!(!decision.expansion_allowed);
}

#[test]
fn missing_managed_observation_is_not_zero_usage() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 2000, 0),
            None,
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert!(!decision.expansion_allowed);
    assert!(decision.would_drain.is_empty());
    // The request is charged as own usage: 8000 - 2000 reserve - (2000 busy - 1000).
    assert_eq!(decision.managed_budget.cpu_millicores, 5000);
    assert_eq!(decision.admission_headroom, Resources::default());
    for key in ["cpu", "ram"] {
        assert!(codes(&decision, key).contains(&"allocation_unknown_hold".into()));
    }
}

#[test]
fn missing_nvml_does_not_disable_cpu_only_systems() {
    for status in [
        CapabilityStatus::Unsupported,
        CapabilityStatus::Unavailable,
        CapabilityStatus::Unknown,
    ] {
        let mut snap = snapshot();
        snap.gpu_inventory = status;
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .unwrap();
        assert!(decision.expansion_allowed);
        assert!(decision.admission_headroom.cpu_millicores > 0);
        assert!(decision.admission_headroom.gpu_memory_mib.is_empty());
    }
}

#[test]
fn unknown_sensor_totals_emit_zero_budget_diagnostics_even_without_reserves() {
    let mut snap = snapshot();
    snap.cpu_capacity_millicores = 0;
    snap.physical_cores = None;
    snap.cpu_busy_millicores = None;
    snap.total_ram_mib = 0;
    snap.available_ram_mib = None;
    snap.gpu_inventory = CapabilityStatus::Unknown;
    for zero_reserves in [false, true] {
        let mut cfg = config();
        if zero_reserves {
            cfg.cpu.reserve_physical_cores = 0;
            cfg.ram.reserve_mib = 0;
            cfg.ram.reserve_percent = 0;
        }
        let decision = PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap();
        assert_eq!(decision.managed_budget, Resources::default());
        assert_eq!(decision.admission_headroom, Resources::default());
        assert!(!decision.expansion_allowed);
        assert!(decision.observe_only);
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.contains("CPU capacity unavailable"))
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.contains("RAM capacity unavailable"))
        );
    }
}

#[test]
fn unknown_totals_reject_contradictory_known_sensor_values() {
    let mut snap = snapshot();
    snap.cpu_capacity_millicores = 0;
    snap.physical_cores = None;
    snap.cpu_busy_millicores = Some(0);
    assert!(
        PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .is_err()
    );
    snap.cpu_busy_millicores = None;
    snap.physical_cores = Some(1);
    assert!(
        PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .is_err()
    );
    snap = snapshot();
    snap.total_ram_mib = 0;
    snap.available_ram_mib = Some(0);
    assert!(
        PolicyEngine::new()
            .evaluate(&config(), &snap, &PolicyInput::default(), 0)
            .is_err()
    );
}

#[test]
fn lost_gpu_inventory_yields_existing_gpu_allocations() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "gpu-worker",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 6000)),
        )],
        ..PolicyInput::default()
    };
    // Lost GPU telemetry first holds for the unknown window, then yields (FR-008).
    let mut engine = PolicyEngine::new();
    let held = engine.evaluate(&config(), &snapshot(), &input, 0).unwrap();
    assert!(!held.expansion_allowed);
    assert!(held.would_drain.is_empty());
    let decision = engine
        .evaluate(&config(), &snapshot(), &input, 1500)
        .unwrap();
    assert!(!decision.expansion_allowed);
    assert_eq!(decision.would_drain, ["gpu-worker"]);
}

/// The CPU idle reserve is the larger of the explicit reserve and the mode's
/// margin of total capacity, never both.
#[test]
fn cpu_idle_reserve_is_the_larger_of_the_reserve_and_the_margin() {
    use cedegrid::contention::ContentionMode::{self, *};
    let mut snap = snapshot();
    snap.cpu_capacity_millicores = 32_000;
    snap.physical_cores = Some(16);
    snap.cpu_busy_millicores = Some(0);
    let allowance = |mode: ContentionMode, reserve: u32| {
        let mut config = Config::default();
        config.cpu.reserve_physical_cores = reserve;
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        PolicyEngine::new()
            .evaluate(&config, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .managed_budget
            .cpu_millicores
    };
    // Margins of 15%, 5% and 2% of 32 000 m are 4800, 1600 and 640 m; one and four
    // reserved physical cores of 16 are 2000 and 8000 m.
    for (reserve, expected) in [
        (0, [27_200, 30_400, 31_360]),
        (1, [27_200, 30_000, 30_000]),
        (4, [24_000, 24_000, 24_000]),
    ] {
        let got = [YieldFirst, Neutral, Performance].map(|mode| allowance(mode, reserve));
        assert_eq!(got, expected, "reserve {reserve}");
    }
}

/// GPU memory: work that takes a hint is judged on usage, so a charge beyond the
/// budget only blocks admission; work without a hint channel is judged at its
/// charge, since its growth cannot be bounded.
#[test]
fn gpu_memory_judges_hint_ready_work_on_usage_and_other_work_on_its_charge() {
    // 16000 MiB with a 1000 MiB floor; 6000 MiB used, 4000 MiB of it the job's,
    // which requested 14000 MiB: 10000 MiB free, but the charge exceeds the budget.
    let input = |hint_ready: bool| PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(0, 0, 14_000),
            Some(resources(0, 0, 4000)),
        )],
        facts: BTreeMap::from([(
            "job".into(),
            AllocationFacts {
                hint_ready,
                observes_drain: true,
                ..AllocationFacts::default()
            },
        )]),
        ..PolicyInput::default()
    };
    let evaluate = |hint_ready| {
        PolicyEngine::new()
            .evaluate(&config(), &gpu_snapshot(), &input(hint_ready), 0)
            .unwrap()
    };
    let ready = evaluate(true);
    assert!(ready.actions.is_empty(), "{:?}", ready.actions);
    assert_eq!(
        ready
            .admission_headroom
            .gpu_memory_mib
            .get("GPU-test")
            .copied()
            .unwrap_or(0),
        0
    );
    let plain = evaluate(false);
    assert_eq!(plain.would_drain, ["job"]);
    for decision in [&ready, &plain] {
        assert!(!codes(decision, "gpu_memory:GPU-test").contains(&"floor_breach".to_string()));
    }
}

#[test]
fn vram_reserve_breach_yields_even_when_gpu_is_idle() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(15_500);
    snap.gpus[0].utilization_percent = Some(0);
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(1000, 1000, 15_500),
            Some(resources(1000, 1000, 15_500)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(decision.would_drain, ["worker"]);
    assert!(
        decision
            .reasons
            .iter()
            .any(|reason| reason.contains("reserve"))
    );
}

#[test]
fn custom_cooldown_requires_continuously_known_spare_capacity() {
    let mut config = config();
    config.gpu.scale_up_cooldown_ms = Some(137);
    let mut engine = PolicyEngine::new();
    let mut snap = snapshot();
    assert!(
        !engine
            .evaluate(&config, &snap, &PolicyInput::default(), 10)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        !engine
            .evaluate(&config, &snap, &PolicyInput::default(), 146)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        engine
            .evaluate(&config, &snap, &PolicyInput::default(), 147)
            .unwrap()
            .expansion_allowed
    );
    snap.cpu_busy_millicores = None;
    assert!(
        !engine
            .evaluate(&config, &snap, &PolicyInput::default(), 150)
            .unwrap()
            .expansion_allowed
    );
    snap.cpu_busy_millicores = Some(2000);
    assert!(
        !engine
            .evaluate(&config, &snap, &PolicyInput::default(), 151)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        engine
            .evaluate(&config, &snap, &PolicyInput::default(), 288)
            .unwrap()
            .expansion_allowed
    );
}

#[test]
fn cpu_pressure_resets_recovery_cooldown() {
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = Some(100);
    let mut engine = PolicyEngine::new();
    engine
        .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 0)
        .unwrap();
    assert!(
        engine
            .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 100)
            .unwrap()
            .expansion_allowed
    );
    let over_budget = PolicyInput {
        allocations: vec![allocation(
            "pending",
            AllocationPhase::Pending,
            resources(5000, 0, 0),
            None,
        )],
        ..PolicyInput::default()
    };
    assert!(
        !engine
            .evaluate(&cfg, &snapshot(), &over_budget, 101)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        !engine
            .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 1000)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        !engine
            .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 1099)
            .unwrap()
            .expansion_allowed
    );
    assert!(
        engine
            .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 1100)
            .unwrap()
            .expansion_allowed
    );
}

#[test]
fn drain_choice_is_pending_first_then_id_independent_of_input_order() {
    let running_a = allocation(
        "a",
        AllocationPhase::Running,
        resources(3000, 0, 0),
        Some(resources(3000, 0, 0)),
    );
    let running_b = allocation(
        "b",
        AllocationPhase::Running,
        resources(3000, 0, 0),
        Some(resources(3000, 0, 0)),
    );
    let pending = allocation("z", AllocationPhase::Pending, resources(1000, 0, 0), None);
    let first = PolicyInput {
        allocations: vec![running_b.clone(), pending.clone(), running_a.clone()],
        facts: rootless(&["a", "b", "z"]),
        ..PolicyInput::default()
    };
    let second = PolicyInput {
        allocations: vec![running_a, running_b, pending],
        facts: rootless(&["a", "b", "z"]),
        ..PolicyInput::default()
    };
    // External work uses 1000m: the budget is 5000m for 6000m of own usage and a
    // 1000m reservation.
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(7000);
    let a = PolicyEngine::new()
        .evaluate(&config(), &snap, &first, 0)
        .unwrap();
    let b = PolicyEngine::new()
        .evaluate(&config(), &snap, &second, 0)
        .unwrap();
    // The pending reservation is cancelled by its charge; running work is capped by
    // usage, the equal candidates in ID order.
    assert_eq!(a.would_drain, ["z"]);
    assert_eq!(a.actions, b.actions);
    let planned: std::collections::BTreeSet<_> = a
        .actions
        .iter()
        .map(|action| (action.allocation_id.as_str(), action.step, action.target))
        .collect();
    use cedegrid::contention::YieldStep;
    assert_eq!(
        planned,
        [
            ("z", YieldStep::Drain, None),
            ("a", YieldStep::Cap, Some(2000))
        ]
        .into()
    );
    // CPU admits nothing more; RAM stays admissible on its own (T086a).
    assert_eq!(a.admission_headroom.cpu_millicores, 0);
    assert!(a.admission_headroom.ram_mib > 0);
}

#[test]
fn explicit_drain_covers_all_pending_and_running_allocations() {
    let input = PolicyInput {
        allocations: vec![
            allocation(
                "a",
                AllocationPhase::Running,
                Resources::default(),
                Some(Resources::default()),
            ),
            allocation("b", AllocationPhase::Pending, Resources::default(), None),
            allocation(
                "c",
                AllocationPhase::Draining,
                Resources::default(),
                Some(Resources::default()),
            ),
        ],
        explicit_drain: true,
        facts: Default::default(),
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert_eq!(decision.would_drain, ["b", "a"]);
    assert!(decision.observe_only);
    assert!(!decision.expansion_allowed);
}

#[test]
fn invalid_inputs_and_clock_regression_are_errors() {
    let mut engine = PolicyEngine::new();
    let mut snap = snapshot();
    engine
        .evaluate(&config(), &snap, &PolicyInput::default(), 100)
        .unwrap();
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 99)
            .is_err()
    );
    snap.node_id = "different".into();
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 101)
            .is_err()
    );
    snap = snapshot();
    snap.available_ram_mib = Some(33_000);
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 101)
            .is_err()
    );
    let invalid_pending = PolicyInput {
        allocations: vec![allocation(
            "a",
            AllocationPhase::Pending,
            Resources::default(),
            Some(Resources::default()),
        )],
        ..PolicyInput::default()
    };
    assert!(
        engine
            .evaluate(&config(), &snapshot(), &invalid_pending, 101)
            .is_err()
    );
}

#[test]
fn duplicate_ids_and_resource_overflow_are_errors() {
    let duplicate = allocation("a", AllocationPhase::Pending, Resources::default(), None);
    let input = PolicyInput {
        allocations: vec![duplicate.clone(), duplicate],
        ..PolicyInput::default()
    };
    assert!(
        PolicyEngine::new()
            .evaluate(&config(), &snapshot(), &input, 0)
            .is_err()
    );
    let input = PolicyInput {
        allocations: vec![
            allocation(
                "a",
                AllocationPhase::Pending,
                resources(u64::MAX, 0, 0),
                None,
            ),
            allocation("b", AllocationPhase::Pending, resources(1, 0, 0), None),
        ],
        ..PolicyInput::default()
    };
    assert!(
        PolicyEngine::new()
            .evaluate(&config(), &snapshot(), &input, 0)
            .is_err()
    );
}

proptest! {
    #[test]
    fn headroom_never_double_charges_running_usage(reserved in 0u64..4000, observed in 0u64..4000, external in 0u64..1000) {
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(observed + external);
        let input = PolicyInput { allocations: vec![allocation("worker", AllocationPhase::Running, resources(reserved, 0, 0), Some(resources(observed, 0, 0)))], ..PolicyInput::default() };
        let decision = PolicyEngine::new().evaluate(&config(), &snap, &input, 0).unwrap();
        let budget = 6000 - external;
        prop_assert_eq!(decision.managed_budget.cpu_millicores, budget);
        // One observation is no full usage window: max(request, observed), once.
        prop_assert_eq!(decision.admission_headroom.cpu_millicores, budget.saturating_sub(reserved.max(observed)));
    }

    #[test]
    fn pending_admission_conserves_capacity(a in 0u64..1700, b in 0u64..1700) {
        let input = PolicyInput { allocations: vec![allocation("a", AllocationPhase::Pending, resources(a, 0, 0), None), allocation("b", AllocationPhase::Pending, resources(b, 0, 0), None)], ..PolicyInput::default() };
        let decision = PolicyEngine::new().evaluate(&config(), &snapshot(), &input, 0).unwrap();
        prop_assert_eq!(decision.admission_headroom.cpu_millicores + a + b, decision.managed_budget.cpu_millicores);
    }
}

#[test]
fn cpu_ram_stability_survives_gpu_uncertainty_without_reusing_retained_gpu_cap() {
    let mut cfg = yield_first_gpu();
    cfg.gpu.scale_up_cooldown_ms = Some(137);
    scale_up_cooldowns(&mut cfg, 137);
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    snap.gpus[0].used_memory_mib = Some(6000);
    let input = PolicyInput {
        allocations: vec![allocation(
            "gpu",
            AllocationPhase::Running,
            resources(1000, 1000, 6000),
            Some(resources(1000, 1000, 6000)),
        )],
        ..Default::default()
    };
    let mut policy = PolicyEngine::new();
    let first = policy.evaluate(&cfg, &snap, &input, 0).unwrap();
    assert!(!first.expansion_allowed && !first.cpu_ram_expansion_allowed);
    assert!(first.managed_budget.gpu_memory_mib["GPU-test"] > 0);
    assert!(first.admission_headroom.gpu_memory_mib.is_empty());
    let waiting = policy.evaluate(&cfg, &snap, &input, 136).unwrap();
    assert!(!waiting.cpu_ram_expansion_allowed);
    // Confirmed release permits CPU capacity; the retained positive GPU cap must
    // not become new admission while external activity remains unknown.
    snap.gpus[0].used_memory_mib = Some(0);
    let next = policy
        .evaluate(&cfg, &snap, &PolicyInput::default(), 137)
        .unwrap();
    assert!(next.cpu_ram_expansion_allowed);
    assert!(!next.expansion_allowed);
    assert!(next.managed_budget.gpu_memory_mib["GPU-test"] > 0);
    assert!(next.admission_headroom.cpu_millicores > 0);
    assert!(next.admission_headroom.ram_mib > 0);
    assert!(next.admission_headroom.gpu_memory_mib.is_empty());
    assert!(next.observe_only);
    let mut old = serde_json::to_value(next).unwrap();
    old.as_object_mut()
        .unwrap()
        .remove("cpu_ram_expansion_allowed");
    assert!(
        !serde_json::from_value::<Decision>(old)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
}

#[test]
fn cpu_ram_fallback_fails_closed_for_missing_stale_excess_and_drain_evidence() {
    for fault in 0..5 {
        let mut snap = gpu_snapshot();
        snap.gpus[0].external_compute = ComputeActivity::Unknown;
        let mut input = PolicyInput::default();
        match fault {
            0 => snap.cpu_busy_millicores = None,
            1 => snap.available_ram_mib = None,
            2 => input.explicit_drain = true,
            3 => input.allocations.push(allocation(
                "excess",
                AllocationPhase::Pending,
                resources(8000, 1000, 0),
                None,
            )),
            _ => input.allocations.push(allocation(
                "uncertain",
                AllocationPhase::Running,
                resources(1000, 1000, 0),
                None,
            )),
        }
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snap, &input, 0)
            .unwrap();
        assert!(!decision.cpu_ram_expansion_allowed, "fault {fault}");
        // Each fault closes the resources it concerns, and only those (T086a).
        let (cpu, ram) = match fault {
            0 | 3 => (true, false),
            1 => (false, true),
            _ => (true, true),
        };
        let open = |key: cedegrid::contention::ResourceKey| {
            decision.resources[&key].admission == Some(cedegrid::contention::Admission::Open)
        };
        let headroom = &decision.admission_headroom;
        assert_eq!(
            headroom.cpu_millicores == 0,
            cpu,
            "fault {fault}: {headroom:?}"
        );
        assert_eq!(headroom.ram_mib == 0, ram, "fault {fault}: {headroom:?}");
        assert_eq!(
            open(cedegrid::contention::ResourceKey::Cpu),
            !cpu,
            "fault {fault}"
        );
        assert_eq!(
            open(cedegrid::contention::ResourceKey::Ram),
            !ram,
            "fault {fault}"
        );
    }
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = Some(137);
    scale_up_cooldowns(&mut cfg, 137);
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    let mut policy = PolicyEngine::new();
    policy
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    snap.cpu_busy_millicores = None;
    assert!(
        !policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 136)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
    snap.cpu_busy_millicores = Some(2000);
    assert!(
        !policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 137)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
    assert!(
        policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 274)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
    let mut scoped = scoped_snapshot();
    scoped.gpu_inventory = CapabilityStatus::Available;
    scoped.gpus = snap.gpus;
    let mut policy = PolicyEngine::new();
    assert!(
        policy
            .evaluate(&config(), &scoped, &PolicyInput::default(), 0)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
    assert!(
        !policy
            .evaluate(&config(), &scoped, &PolicyInput::default(), 1501)
            .unwrap()
            .cpu_ram_expansion_allowed
    );
}

fn gpu_observation(snap: &Snapshot, capability: GpuExecutionCapability) -> GpuObservation {
    GpuObservation {
        capability,
        best_effort_external_processes: None,
        activity_api: "nvmlDeviceGetProcessUtilization".into(),
        activity_api_status: "NotFound".into(),
        query_cursor_us: 100,
        newest_sample_timestamp_us: None,
        samples_returned: 0,
        observed_at_unix_ms: snap.observed_at_unix_ms,
        baseline_age_ms: Some(100),
        sample_max_age_ms: 2000,
        fresh_after_baseline: false,
        freshness_decision: "no_new_driver_timestamp_not_evidence_of_idleness".into(),
    }
}

#[test]
fn one_unobservable_device_does_not_block_another_qualified_device() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].uuid = "GPU-bad".into();
    snap.gpus[0].total_memory_mib = 0;
    snap.gpus[0].used_memory_mib = None;
    snap.gpus[0].external_process_ids = None;
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    snap.gpus[0].observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::InsufficientObservability,
    ));
    let mut good = gpu_snapshot().gpus.remove(0);
    good.used_memory_mib = Some(0);
    good.compute_sample_id = None;
    good.observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::ConservativeNonSharing,
    ));
    snap.gpus.push(good);
    let d = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert!(d.expansion_allowed && d.cpu_ram_expansion_allowed);
    assert!(d.admission_headroom.gpu_memory_mib["GPU-test"] > 0);
    assert!(!d.admission_headroom.gpu_memory_mib.contains_key("GPU-bad"));
    assert_eq!(
        cedegrid::policy::schedulable_budget(&d).gpu_memory_mib["GPU-bad"],
        0
    );
}

#[test]
fn coordinator_budget_never_reuses_an_ineligible_device_protection_cap() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].uuid = "GPU-bad".into();
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    let mut good = gpu_snapshot().gpus.remove(0);
    good.used_memory_mib = Some(0);
    snap.gpus.push(good);
    let mut r = resources(100, 100, 0);
    r.gpu_memory_mib.insert("GPU-bad".into(), 6000);
    let input = PolicyInput {
        allocations: vec![allocation(
            "bad",
            AllocationPhase::Running,
            r.clone(),
            Some(r),
        )],
        ..Default::default()
    };
    let d = PolicyEngine::new()
        .evaluate(&yield_first_gpu(), &snap, &input, 0)
        .unwrap();
    assert!(d.managed_budget.gpu_memory_mib["GPU-bad"] > 0);
    let offered = cedegrid::policy::schedulable_budget(&d);
    assert_eq!(offered.gpu_memory_mib["GPU-bad"], 0);
    assert!(offered.gpu_memory_mib["GPU-test"] > 0);
}

/// Utilization never decides GPU admission; an external context with activity of
/// unknown amount holds Yield First, whatever the utilization (research R8).
#[test]
fn utilization_never_decides_gpu_admission() {
    let cfg = yield_first_gpu();
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    let headroom = |snap: &Snapshot| {
        PolicyEngine::new()
            .evaluate(&cfg, snap, &PolicyInput::default(), 0)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .get("GPU-test")
            .copied()
            .unwrap_or(0)
    };
    for utilization in [Some(0), Some(100), None] {
        snap.gpus[0].utilization_percent = utilization;
        assert_eq!(headroom(&snap), 15_000, "{utilization:?}");
    }
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    for utilization in [Some(0), Some(100), None] {
        snap.gpus[0].utilization_percent = utilization;
        assert_eq!(headroom(&snap), 0, "{utilization:?}");
    }
}

#[test]
fn stale_gpu_observation_blocks_only_that_device() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ConservativeNonSharing);
    observation.observed_at_unix_ms += 1;
    snap.gpus[0].observation = Some(observation);
    let d = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert!(d.cpu_ram_expansion_allowed);
    assert!(d.admission_headroom.gpu_memory_mib.is_empty());
}

#[test]
fn legacy_gpu_record_and_config_readers_remain_compatible() {
    let mut json = serde_json::to_value(gpu_snapshot()).unwrap();
    json["gpus"][0]
        .as_object_mut()
        .unwrap()
        .remove("observation");
    let old: Snapshot = serde_json::from_value(json).unwrap();
    assert!(old.gpus[0].observation.is_none());
    let cfg: cedegrid::config::GpuConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.execution_mode, cedegrid::config::GpuExecutionMode::Auto);
    assert!(cfg.process_sample_max_age_ms.is_none());
}

#[test]
fn launch_recheck_refuses_an_external_context_appearing_after_empty_device_offer() {
    // With never_share_occupied_gpus, an offer made on an empty device is refused at
    // the launch recheck once an external context appears (FR-064).
    let mut cfg = config();
    cfg.contention.never_share_occupied_gpus = true;
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    snap.gpus[0].external_process_ids = Some(vec![]);
    let check = |snap: &Snapshot| {
        cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            snap,
            &resources(1, 1, 500),
            AllocationClass::Opportunistic,
        )
    };
    assert!(check(&snap).is_ok());
    // Even a contradictory Idle flag cannot overrule the inventory.
    snap.gpus[0].external_process_ids = Some(vec![987]);
    let error = check(&snap).unwrap_err();
    assert!(
        error.to_string().contains("never_share_occupied_gpus"),
        "{error}"
    );
}

/// Pinned until US5 floors protect external usage; the US5 task that adds those
/// floors inverts this test for Neutral and Performance.
#[test]
fn launch_recheck_admits_active_external_compute_except_in_yield_first() {
    use cedegrid::contention::ContentionMode;
    let mut cfg = config();
    cfg.contention
        .acknowledge_caution
        .insert("performance_mode".into());
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ContentionAware);
    observation.fresh_after_baseline = true;
    observation.newest_sample_timestamp_us = Some(101);
    snap.gpus[0].observation = Some(observation);
    let check = |cfg: &Config, snap: &Snapshot| {
        cedegrid::policy::validate_gpu_launch_contract(
            cfg,
            snap,
            &resources(1, 1, 500),
            AllocationClass::Opportunistic,
        )
    };
    // The floors and the peak headroom protect the external usage (T087a).
    for mode in [ContentionMode::Neutral, ContentionMode::Performance] {
        cfg.contention.default_mode = mode;
        check(&cfg, &snap).unwrap();
    }
    cfg.contention.default_mode = ContentionMode::YieldFirst;
    let error = check(&cfg, &snap).unwrap_err();
    assert!(error.to_string().contains("quiet device"), "{error}");
    // Quiet stays admitted in every mode.
    snap.gpus[0].external_compute = ComputeActivity::Quiet;
    for mode in [
        ContentionMode::YieldFirst,
        ContentionMode::Neutral,
        ContentionMode::Performance,
    ] {
        cfg.contention.default_mode = mode;
        check(&cfg, &snap).unwrap();
    }
}

#[test]
fn best_effort_mode_config_rejects_empty_and_partial_authorization() {
    let empty: GpuConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.execution_mode, GpuExecutionMode::Auto);
    assert!(empty.best_effort_external_processes.is_empty());
    // The list is still validated, as in 0.2, although it is no longer used (FR-031).
    let mut cfg = config();
    cfg.gpu.execution_mode = GpuExecutionMode::BestEffortOccupied;
    cfg.gpu.best_effort_external_processes.insert(
        "GPU-test".into(),
        vec![ExternalGpuProcessIdentity {
            pid: 4242,
            boot_id: "fixture-boot".into(),
            start_ticks: 87654,
            uid: 1000,
        }],
    );
    cfg.validate().unwrap();
    cfg.gpu
        .best_effort_external_processes
        .get_mut("GPU-test")
        .unwrap()[0]
        .start_ticks = 0;
    assert!(cfg.validate().is_err());
    cfg.gpu.best_effort_external_processes.clear();
    assert!(cfg.validate().is_err());
    assert!(
        serde_json::from_str::<ExternalGpuProcessIdentity>(r#"{"pid":42,"uid":1000}"#).is_err()
    );
}

// Yielding defects D1–D6 from the code-quality audit, inverted by specs/001-contention-modes.

fn large_gpu_snapshot(used_mib: u64, external_pids: Vec<u32>) -> Snapshot {
    let mut snap = gpu_snapshot();
    snap.gpus[0].total_memory_mib = 24_576;
    snap.gpus[0].used_memory_mib = Some(used_mib);
    snap.gpus[0].utilization_percent = Some(0);
    snap.gpus[0].external_process_ids = Some(external_pids);
    snap
}

fn codes(decision: &Decision, key: &str) -> Vec<String> {
    let key: cedegrid::contention::ResourceKey = key.parse().unwrap();
    decision
        .resources
        .get(&key)
        .map(|status| status.reasons.iter().map(|r| r.code.clone()).collect())
        .unwrap_or_default()
}

/// A run whose supervisor found processes outside its verified family is noted as
/// `single_process_violated` with the count, on CPU and RAM, where `explain` shows it; its
/// steps are unchanged (FR-082).
#[test]
fn processes_outside_the_family_are_noted() {
    let mut facts = rootless(&["job"]);
    facts.get_mut("job").unwrap().outside_family = 2;
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(1000, 1000, 0),
            Some(resources(1000, 1000, 0)),
        )],
        facts,
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    for key in ["cpu", "ram"] {
        let notes = &decision.resources[&key.parse().unwrap()].reasons;
        let note = notes
            .iter()
            .find(|r| r.code == "single_process_violated")
            .unwrap_or_else(|| panic!("{key}: {notes:?}"));
        assert!(
            note.text.contains("job") && note.text.contains('2'),
            "{note:?}"
        );
    }
    assert!(decision.actions.is_empty() && decision.would_drain.is_empty());
}

/// A run with more than its leader is noted as `family_members` with its size, the leader,
/// SDK children and descendants, on CPU and RAM; processes outside it carry their reason
/// (FR-082).
#[test]
fn a_family_is_noted_with_its_size_and_outsiders_with_their_reason() {
    let mut facts = rootless(&["job"]);
    let job = facts.get_mut("job").unwrap();
    job.family = Some(cedegrid::model::FamilySize {
        leader: 1,
        sdk_children: 1,
        descendants: 4,
    });
    job.outside_family = 3;
    job.outside_reason = Some("over_bound".into());
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(1000, 1000, 0),
            Some(resources(1000, 1000, 0)),
        )],
        facts,
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    for key in ["cpu", "ram"] {
        let notes = &decision.resources[&key.parse().unwrap()].reasons;
        let family = notes
            .iter()
            .find(|r| r.code == "family_members")
            .unwrap_or_else(|| panic!("{key}: {notes:?}"));
        assert!(
            family.text.contains("job") && family.text.contains("6 processes"),
            "{family:?}"
        );
        assert!(family.text.contains("4 descendants"), "{family:?}");
        let outside = notes
            .iter()
            .find(|r| r.code == "single_process_violated")
            .unwrap();
        assert!(outside.text.contains("over_bound"), "{outside:?}");
    }
    // A run that is its leader alone gets no note.
    let mut facts = rootless(&["job"]);
    facts.get_mut("job").unwrap().family = Some(cedegrid::model::FamilySize {
        leader: 1,
        ..Default::default()
    });
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(1000, 1000, 0),
            Some(resources(1000, 1000, 0)),
        )],
        facts,
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert!(!codes(&decision, "cpu").contains(&"family_members".to_string()));
}

/// GPU attribution unknown for one allocation, its device's entry missing from the
/// observation, keeps its CPU and RAM readings: neither holds for it, while the device's
/// memory charges it at max(request, last known) as unknown own usage, so its external usage
/// is not inflated and GPU admission holds (FR-008, TEL-5).
#[test]
fn unknown_gpu_attribution_keeps_the_cpu_and_ram_readings() {
    use cedegrid::contention::ResourceKey;
    let mut job = allocation(
        "job",
        AllocationPhase::Running,
        resources(1000, 1000, 2000),
        Some(resources(1000, 1000, 3000)),
    );
    let input = |job: &Allocation| PolicyInput {
        allocations: vec![job.clone()],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let snap = gpu_snapshot();
    let mut engine = PolicyEngine::new();
    engine.evaluate(&config(), &snap, &input(&job), 0).unwrap();
    job.observed.as_mut().unwrap().gpu_memory_mib.clear();
    let decision = engine
        .evaluate(&config(), &snap, &input(&job), 500)
        .unwrap();
    for key in ["cpu", "ram"] {
        assert!(
            !codes(&decision, key).contains(&"allocation_unknown_hold".to_string()),
            "{key}: {:?}",
            codes(&decision, key)
        );
    }
    let gpu = &decision.resources[&ResourceKey::GpuMemory("GPU-test".into())];
    // 6000 MiB used on the device, the job's last known 3000 MiB of it own.
    assert_eq!(gpu.external, Some(3000), "{gpu:?}");
    assert!(
        decision.resources[&ResourceKey::GpuCompute("GPU-test".into())]
            .admission
            .as_ref()
            .is_some_and(|admission| admission.code() == Some("gpu_observation_unknown")),
        "{:?}",
        decision.resources[&ResourceKey::GpuCompute("GPU-test".into())].admission
    );
}

#[test]
fn d1_first_observation_is_a_baseline_not_external_growth() {
    // A fresh engine sees 300 MiB of driver/context memory on an otherwise empty
    // device before its own task has allocated anything.
    // A known device with nothing allocated reads 0 MiB; a missing entry would be unknown.
    let none_yet = || Resources {
        gpu_memory_mib: BTreeMap::from([("GPU-test".into(), 0)]),
        ..resources(0, 0, 0)
    };
    let mut input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(0, 0, 4000),
            Some(none_yet()),
        )],
        ..Default::default()
    };
    let mut engine = PolicyEngine::new();
    let first = engine
        .evaluate(&config(), &large_gpu_snapshot(300, vec![]), &input, 0)
        .unwrap();
    assert!(first.would_drain.is_empty() && first.actions.is_empty());
    // 24576 total - 1000 reserve - 300 not attributed to own work; no protection cap.
    assert_eq!(first.managed_budget.gpu_memory_mib["GPU-test"], 23_276);
    assert!(codes(&first, "gpu_memory:GPU-test").contains(&"first_observation_baseline".into()));
    // A second task's first observation, with 300 MiB of its own context memory
    // not yet attributed, is a baseline as well.
    input.allocations.push(allocation(
        "second",
        AllocationPhase::Running,
        resources(0, 0, 4000),
        Some(none_yet()),
    ));
    let second = engine
        .evaluate(&config(), &large_gpu_snapshot(600, vec![]), &input, 1000)
        .unwrap();
    assert!(second.would_drain.is_empty() && second.actions.is_empty());
    assert!(codes(&second, "gpu_memory:GPU-test").contains(&"first_observation_baseline".into()));
}

#[test]
fn d2_vram_changes_below_the_noise_tolerance_are_not_growth() {
    let input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(0, 0, 4000),
            Some(resources(0, 0, 4000)),
        )],
        ..Default::default()
    };
    let mut engine = PolicyEngine::new();
    let baseline = engine
        .evaluate(&config(), &large_gpu_snapshot(4000, vec![]), &input, 0)
        .unwrap();
    assert!(baseline.would_drain.is_empty());
    let noise = engine
        .evaluate(&config(), &large_gpu_snapshot(4001, vec![]), &input, 1000)
        .unwrap();
    assert!(noise.would_drain.is_empty() && noise.actions.is_empty());
    assert_eq!(noise.managed_budget.gpu_memory_mib["GPU-test"], 23_575);
    assert!(
        noise
            .admission_headroom
            .gpu_memory_mib
            .contains_key("GPU-test")
    );
    assert!(codes(&noise, "gpu_memory:GPU-test").contains(&"vram_noise_below_tolerance".into()));
    // Small steps accumulate against the baseline instead of resetting it.
    let creeping = engine
        .evaluate(&config(), &large_gpu_snapshot(4200, vec![]), &input, 2000)
        .unwrap();
    assert!(
        creeping
            .admission_headroom
            .gpu_memory_mib
            .contains_key("GPU-test")
    );
    let growth = engine
        .evaluate(&config(), &large_gpu_snapshot(4300, vec![]), &input, 3000)
        .unwrap();
    // Growth beyond the tolerance blocks GPU admission, but 23276 MiB still fits
    // the 4000 MiB task, so nothing yields.
    assert!(
        !growth
            .admission_headroom
            .gpu_memory_mib
            .contains_key("GPU-test")
    );
    assert!(growth.would_drain.is_empty() && growth.actions.is_empty());
    assert_eq!(growth.managed_budget.gpu_memory_mib["GPU-test"], 23_276);
}

#[test]
fn d3_a_quiet_external_context_admits_gpu_work() {
    let mut cfg = config();
    cfg.gpu.execution_mode = GpuExecutionMode::Auto;
    cfg.gpu.scale_up_cooldown_ms = Some(60_000);
    // One external process holds 2 GiB of 24 GiB and has been quiet over the window.
    let mut snap = large_gpu_snapshot(2048, vec![4242]);
    snap.gpus[0].external_compute = ComputeActivity::Quiet;
    // NVML returns no samples for a quiet context (variant A) or a fresh zero sample
    // (variant B). Both are healthy observation.
    let stale = gpu_observation(&snap, GpuExecutionCapability::ContentionAware);
    let mut fresh = stale.clone();
    fresh.activity_api_status = "Success".into();
    fresh.newest_sample_timestamp_us = Some(200);
    fresh.samples_returned = 1;
    fresh.fresh_after_baseline = true;
    for observation in [stale, fresh] {
        snap.gpus[0].observation = Some(observation);
        let mut engine = PolicyEngine::new();
        for (now, offered) in [(0, false), (30_000, false), (60_000, true), (120_000, true)] {
            snap.gpus[0].compute_sample_id = Some(now + 1);
            let decision = engine
                .evaluate(&cfg, &snap, &PolicyInput::default(), now)
                .unwrap();
            let headroom = decision.admission_headroom.gpu_memory_mib.get("GPU-test");
            assert_eq!(headroom.copied(), offered.then_some(21_528), "at {now}");
        }
        cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(0, 0, 4000),
            AllocationClass::Opportunistic,
        )
        .unwrap();
    }
    // Activity of unknown amount holds Yield First.
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    cfg.contention.default_mode = cedegrid::contention::ContentionMode::YieldFirst;
    assert!(
        cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(0, 0, 4000),
            AllocationClass::Opportunistic,
        )
        .is_err()
    );
}

#[test]
fn d4_cpu_contention_caps_instead_of_draining() {
    // 8 logical CPUs on 4 physical cores; reserving 1 physical core holds 2000m.
    // A rootless backend cannot lower the share at runtime, so cap comes first.
    let input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(4000, 0, 0),
            Some(resources(4000, 0, 0)),
        )],
        facts: rootless(&["mine"]),
        ..Default::default()
    };
    let mut engine = PolicyEngine::new();
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(4000);
    let alone = engine.evaluate(&config(), &snap, &input, 0).unwrap();
    assert!(alone.would_drain.is_empty() && alone.actions.is_empty());
    assert_eq!(alone.managed_budget.cpu_millicores, 6000);
    // Another workload starts using three cores.
    snap.cpu_busy_millicores = Some(7000);
    let contended = engine.evaluate(&config(), &snap, &input, 1000).unwrap();
    assert_eq!(contended.managed_budget.cpu_millicores, 3000);
    assert!(contended.would_drain.is_empty());
    assert_eq!(
        contended.actions,
        [YieldAction {
            allocation_id: "mine".into(),
            resource: cedegrid::contention::ResourceKey::Cpu,
            step: cedegrid::contention::YieldStep::Cap,
            target: Some(3000),
            cause: cedegrid::contention::YieldCause::ExternalGrowth,
            code: "yield_step_cap".into(),
            deadline_ms: None,
        }]
    );
    assert_eq!(contended.admission_headroom.cpu_millicores, 0);
}

#[test]
fn d5_sampling_skew_between_own_and_host_cpu_keeps_the_budget() {
    let two = |observed: u64, upper: Option<u64>| PolicyInput {
        allocations: ["a", "b"]
            .into_iter()
            .map(|id| {
                let mut a = allocation(
                    id,
                    AllocationPhase::Running,
                    resources(2000, 0, 0),
                    Some(resources(observed, 0, 0)),
                );
                a.observed_upper = upper.map(|cpu| resources(cpu, 0, 0));
                a
            })
            .collect(),
        ..Default::default()
    };
    // The recorded defect: own usage sums to 4000m while the host reads 3990m and no
    // error bounds are known. The resource holds instead of draining everything.
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(3990);
    let held = PolicyEngine::new()
        .evaluate(&config(), &snap, &two(2000, None), 0)
        .unwrap();
    assert!(held.would_drain.is_empty() && held.actions.is_empty());
    assert_eq!(held.managed_budget.cpu_millicores, 6000);
    assert_eq!(held.admission_headroom.cpu_millicores, 0);
    assert!(codes(&held, "cpu").contains(&"measurement_unknown_hold".into()));
    // With schema 3 bounds the same skew is inside the known sampling error: external
    // is clamped and admission continues.
    let mut snap = snapshot();
    snap.schema_version = SCHEMA_VERSION;
    snap.cpu_busy_millicores = Some(4100);
    snap.cpu_busy_lower_millicores = Some(3990);
    let consistent = PolicyEngine::new()
        .evaluate(&config(), &snap, &two(2000, Some(2050)), 0)
        .unwrap();
    assert!(consistent.would_drain.is_empty() && consistent.actions.is_empty());
    // 8000 - 2000 reserve - (4100 host upper - 4000 own lower); the reserve exceeds the margin.
    assert_eq!(consistent.managed_budget.cpu_millicores, 5900);
    assert_eq!(consistent.admission_headroom.cpu_millicores, 1900);
    assert!(codes(&consistent, "cpu").contains(&"inconsistent_within_tolerance".into()));
}

/// The collector's summed own CPU bound (one tick of pending time per scope CPU,
/// not per process) replaces the sum of per-allocation lower bounds, within that
/// sum and the sum of upper bounds (item 024 Part 0).
#[test]
fn the_summed_own_cpu_bound_applies_within_the_per_allocation_sums() {
    use cedegrid::contention::ResourceKey;
    let input = PolicyInput {
        allocations: (0..2)
            .map(|i| {
                let mut a = allocation(
                    &format!("a{i}"),
                    AllocationPhase::Running,
                    resources(2000, 100, 0),
                    Some(resources(1843, 100, 0)),
                );
                a.observed_upper = Some(resources(1957, 100, 0));
                a
            })
            .collect(),
        ..Default::default()
    };
    let own = |summed| {
        let mut snap = snapshot();
        snap.schema_version = SCHEMA_VERSION;
        snap.cpu_busy_millicores = Some(4000);
        snap.cpu_busy_lower_millicores = Some(3990);
        snap.managed_cpu_lower_millicores = summed;
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snap, &input, 0)
            .unwrap();
        let status = &decision.resources[&ResourceKey::Cpu];
        (status.own, status.external)
    };
    assert_eq!(own(None), (Some((3686, 3914)), Some(314)));
    assert_eq!(own(Some(3791)), (Some((3791, 3914)), Some(209)));
    // Never above the upper sum, never below the lower sum.
    assert_eq!(own(Some(9999)).0, Some((3914, 3914)));
    assert_eq!(own(Some(10)).0, Some((3686, 3914)));
}

#[test]
fn d6_one_unknown_allocation_does_not_drain_others() {
    let input = |a_observed: Option<u64>| PolicyInput {
        allocations: vec![
            allocation(
                "a-unobserved",
                AllocationPhase::Running,
                resources(500, 0, 0),
                a_observed.map(|cpu| resources(cpu, 0, 0)),
            ),
            allocation(
                "b-observed",
                AllocationPhase::Running,
                resources(500, 0, 0),
                Some(resources(500, 0, 0)),
            ),
        ],
        ..Default::default()
    };
    let mut engine = PolicyEngine::new();
    engine
        .evaluate(&config(), &snapshot(), &input(Some(800)), 0)
        .unwrap();
    let decision = engine
        .evaluate(&config(), &snapshot(), &input(None), 1000)
        .unwrap();
    assert!(decision.would_drain.is_empty() && decision.actions.is_empty());
    // The unobserved task is charged at its last known 800m and is not counted as
    // external: 8000 - 2000 reserve - (2000 busy - 500 - 800).
    assert_eq!(decision.managed_budget.cpu_millicores, 5300);
    assert_eq!(decision.admission_headroom.cpu_millicores, 0);
    assert!(codes(&decision, "cpu").contains(&"allocation_unknown_hold".into()));
    // Without any earlier observation the request is charged.
    let fresh = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input(None), 0)
        .unwrap();
    assert!(fresh.would_drain.is_empty());
    assert_eq!(fresh.managed_budget.cpu_millicores, 5000);
}

#[test]
fn schema_v1_v2_and_v3_snapshots_decide_identically() {
    assert_eq!(SCHEMA_VERSION, 3);
    let input = PolicyInput {
        allocations: vec![allocation(
            "a",
            AllocationPhase::Running,
            resources(1000, 1000, 0),
            Some(resources(800, 900, 0)),
        )],
        ..Default::default()
    };
    let decisions: Vec<_> = [1, 2, SCHEMA_VERSION]
        .into_iter()
        .map(|version| {
            let mut snap = snapshot();
            snap.schema_version = version;
            let decision = PolicyEngine::new()
                .evaluate(&config(), &snap, &input, 0)
                .unwrap();
            (
                decision.managed_budget,
                decision.admission_headroom,
                decision.would_drain,
                decision.expansion_allowed,
            )
        })
        .collect();
    assert!(decisions.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn schema_v3_fields_round_trip_and_older_recordings_default_them() {
    let mut snap = gpu_snapshot();
    snap.schema_version = SCHEMA_VERSION;
    snap.cpu_busy_lower_millicores = Some(1900);
    snap.gpus[0].reserved_memory_mib = Some(400);
    snap.gpus[0].free_memory_mib = Some(9600);
    snap.gpus[0].unattributed_memory_mib = Some(120);
    snap.gpus[0].external_compute = ComputeActivity::Quiet;
    let back: Snapshot = serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
    assert_eq!(back.cpu_busy_lower_millicores, Some(1900));
    assert_eq!(back.gpus[0].reserved_memory_mib, Some(400));
    assert_eq!(back.gpus[0].free_memory_mib, Some(9600));
    assert_eq!(back.gpus[0].unattributed_memory_mib, Some(120));
    assert_eq!(back.gpus[0].external_compute, ComputeActivity::Quiet);
    let mut managed = allocation(
        "a",
        AllocationPhase::Running,
        resources(1000, 1000, 0),
        Some(resources(980, 900, 0)),
    );
    managed.observed_upper = Some(resources(1044, 1200, 0));
    let back: Allocation = serde_json::from_str(&serde_json::to_string(&managed).unwrap()).unwrap();
    assert_eq!(back.observed_upper, Some(resources(1044, 1200, 0)));
    // A schema 2 recording omits every new field; readers default them to unknown.
    let mut old = serde_json::to_value(gpu_snapshot()).unwrap();
    old["schema_version"] = 2.into();
    let old: Snapshot = serde_json::from_value(old).unwrap();
    assert_eq!(old.cpu_busy_lower_millicores, None);
    assert_eq!(old.gpus[0].reserved_memory_mib, None);
}

#[test]
fn resource_arithmetic_charges_adds_subtracts_and_fits_per_scope() {
    let gpu = |pairs: &[(&str, u64)]| -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    };
    let requested = Resources {
        cpu_millicores: 500,
        ram_mib: 100,
        gpu_memory_mib: gpu(&[("GPU-a", 10)]),
    };
    let observed = Resources {
        cpu_millicores: 300,
        ram_mib: 200,
        gpu_memory_mib: gpu(&[("GPU-a", 4), ("GPU-b", 7)]),
    };
    let charge = Resources::charge(&requested, Some(&observed));
    assert_eq!(
        charge,
        Resources {
            cpu_millicores: 500,
            ram_mib: 200,
            gpu_memory_mib: gpu(&[("GPU-a", 10), ("GPU-b", 7)]),
        }
    );
    assert_eq!(Resources::charge(&requested, None), requested);

    let mut total = Resources::default();
    total.add_checked(&charge).unwrap();
    total.add_checked(&charge).unwrap();
    assert_eq!(total.gpu_memory_mib, gpu(&[("GPU-a", 20), ("GPU-b", 14)]));
    let mut full = Resources {
        cpu_millicores: u64::MAX,
        ..Resources::default()
    };
    assert!(full.add_checked(&charge).is_err());
    full.add_saturating(&charge);
    assert_eq!(full.cpu_millicores, u64::MAX);

    let mut remaining = Resources {
        cpu_millicores: 400,
        ram_mib: 1000,
        gpu_memory_mib: gpu(&[("GPU-a", 5)]),
    };
    remaining.subtract_saturating(&charge);
    assert_eq!(
        remaining,
        Resources {
            cpu_millicores: 0,
            ram_mib: 800,
            gpu_memory_mib: gpu(&[("GPU-a", 0)]),
        },
        "subtraction saturates and never adds devices"
    );

    let cap = Resources {
        cpu_millicores: 1000,
        ram_mib: 1000,
        gpu_memory_mib: gpu(&[("GPU-a", 20), ("GPU-b", 0)]),
    };
    let cpu_only = Resources {
        cpu_millicores: 1,
        ..Resources::default()
    };
    assert!(
        total.fits(&cap, &cpu_only),
        "unrelated GPU charges do not block CPU work"
    );
    assert!(total.fits(&cap, &requested));
    assert!(!total.fits(&cap, &observed), "GPU-b is over its budget");
}

#[test]
fn cpu_contention_climbs_to_pause_and_never_drains_running_work() {
    use cedegrid::contention::{ContentionMode, YieldStep};
    let mut cfg = config();
    cfg.contention.default_mode = ContentionMode::YieldFirst;
    let input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(4000, 0, 0),
            Some(resources(4000, 0, 0)),
        )],
        facts: rootless(&["mine"]),
        ..Default::default()
    };
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(7000);
    let mut engine = PolicyEngine::new();
    let mut steps = vec![];
    // Usage never falls in this recording, so every step looks ineffective.
    for now in (0..40_000).step_by(500) {
        let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
        assert!(decision.would_drain.is_empty(), "at {now}");
        let step = decision.actions.first().map(|a| a.step);
        if steps.last() != Some(&step) {
            steps.push(step);
        }
    }
    // The pause ends at its bound as a cap at the minimum duty, which then holds
    // (FR-014); running CPU work is never drained.
    assert_eq!(
        steps,
        [
            Some(YieldStep::Cap),
            Some(YieldStep::Pause),
            Some(YieldStep::Cap)
        ]
    );
}

/// Performance never pauses own CPU within its fair share: a cap that misses its target is
/// applied again, and a pause, if any, lasts one interval (FR-022).
#[test]
fn performance_cpu_contention_holds_the_cap_instead_of_pausing() {
    use cedegrid::contention::{ContentionMode, YieldStep};
    let mut cfg = config();
    cfg.contention.default_mode = ContentionMode::Performance;
    let input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(4000, 0, 0),
            Some(resources(4000, 0, 0)),
        )],
        facts: rootless(&["mine"]),
        ..Default::default()
    };
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(7000);
    let mut engine = PolicyEngine::new();
    let mut paused = 0;
    // Usage never falls in this recording, so every cap looks ineffective.
    for now in (0..40_000).step_by(500) {
        let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
        assert!(decision.would_drain.is_empty(), "at {now}");
        if decision.actions.iter().any(|a| a.step == YieldStep::Pause) {
            paused += 1;
        }
    }
    assert!(paused <= 1, "paused for {paused} intervals");
}

/// CPU/RAM admission waits its own cooldown, whose default follows the mode's
/// scale-up interval; the GPU cooldown never gates it (audit M1).
#[test]
fn cpu_ram_expansion_follows_its_own_cooldown_not_the_gpu_one() {
    use cedegrid::contention::ContentionMode;
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = Some(60_000);
    for set in [&mut cfg.contention.cpu, &mut cfg.contention.ram] {
        set.scale_up_cooldown_ms = Some(5000);
    }
    let over_budget = PolicyInput {
        allocations: vec![allocation(
            "pending",
            AllocationPhase::Pending,
            resources(50_000, 0, 0),
            None,
        )],
        ..PolicyInput::default()
    };
    let mut engine = PolicyEngine::new();
    let mut allowed = |input: &PolicyInput, now: u64| {
        let decision = engine.evaluate(&cfg, &snapshot(), input, now).unwrap();
        (
            decision.cpu_ram_expansion_allowed,
            decision.expansion_allowed,
        )
    };
    let idle = PolicyInput::default();
    assert_eq!(allowed(&idle, 0), (false, false));
    assert_eq!(allowed(&idle, 4999), (false, false));
    assert_eq!(allowed(&idle, 5000), (true, false));
    // Instability restarts the CPU/RAM cooldown.
    assert!(!allowed(&over_budget, 6000).0);
    assert!(!allowed(&idle, 7000).0);
    assert!(!allowed(&idle, 11_999).0);
    assert_eq!(allowed(&idle, 12_000), (true, false));
    // The GPU cooldown still gates GPU expansion on its own.
    assert!(!allowed(&idle, 66_999).1);
    assert!(allowed(&idle, 67_000).1);
    // Unset, the cooldown is the mode's scale-up interval: 2 s in Performance.
    let mut cfg = config();
    cfg.contention.default_mode = ContentionMode::Performance;
    cfg.contention
        .acknowledge_caution
        .insert("performance_mode".into());
    cfg.contention.cpu.scale_up_cooldown_ms = None;
    cfg.contention.ram.scale_up_cooldown_ms = None;
    let mut engine = PolicyEngine::new();
    let mut allowed = |now: u64| {
        engine
            .evaluate(&cfg, &snapshot(), &PolicyInput::default(), now)
            .unwrap()
            .cpu_ram_expansion_allowed
    };
    assert!(!allowed(0));
    assert!(!allowed(1999));
    assert!(allowed(2000));
}

/// A job runs at the less aggressive of its requested mode and the node cap,
/// and the effective mode is reported per allocation (FR-030).
#[test]
fn effective_mode_is_the_lesser_of_request_and_node_cap_per_allocation() {
    use cedegrid::contention::{ContentionMode, ResourceKey, ResourceKind};
    let mut cfg = config();
    cfg.contention.max_mode = Some(ContentionMode::Neutral);
    let with_modes = |id: &str, modes: &[(ResourceKind, ContentionMode)]| {
        let mut allocation = allocation(
            id,
            AllocationPhase::Running,
            resources(500, 100, 0),
            Some(resources(500, 100, 0)),
        );
        allocation.modes = modes.iter().copied().collect();
        allocation
    };
    let input = PolicyInput {
        allocations: vec![
            with_modes("fast", &[(ResourceKind::Cpu, ContentionMode::Performance)]),
            with_modes("gentle", &[(ResourceKind::Cpu, ContentionMode::YieldFirst)]),
            with_modes("plain", &[]),
        ],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snapshot(), &input, 0)
        .unwrap();
    let mode = |id: &str, kind| decision.allocation_modes[id][&kind];
    assert_eq!(mode("fast", ResourceKind::Cpu), ContentionMode::Neutral);
    // A mode the cap refuses is reported with a reason code and the node's cap.
    assert_eq!(decision.max_mode, Some(ContentionMode::Neutral));
    let capped: Vec<_> = decision.resources[&ResourceKey::Cpu]
        .reasons
        .iter()
        .filter(|r| r.code == "mode_capped")
        .collect();
    assert_eq!(capped.len(), 1, "{capped:?}");
    assert!(capped[0].text.contains("fast") && capped[0].text.contains("neutral"));
    assert_eq!(
        mode("gentle", ResourceKind::Cpu),
        ContentionMode::YieldFirst
    );
    assert_eq!(mode("plain", ResourceKind::Cpu), ContentionMode::Neutral);
    assert_eq!(mode("fast", ResourceKind::Ram), ContentionMode::Neutral);
    // The node mode governs a resource's aggregate; per-job modes order yielding
    // inside it (T093a).
    let cpu = &decision.resources[&ResourceKey::Cpu];
    assert_eq!(cpu.mode, Some(ContentionMode::Neutral));
    assert_eq!(cpu.effective_mode, Some(ContentionMode::Neutral));
    // Without a configured cap, no job reaches Performance unless the operator chose it.
    let modes = |cfg: &Config| {
        PolicyEngine::new()
            .evaluate(cfg, &snapshot(), &input, 0)
            .unwrap()
            .allocation_modes["fast"][&ResourceKind::Cpu]
    };
    assert_eq!(modes(&config()), ContentionMode::Neutral);
    let mut chosen = config();
    chosen.contention.max_mode = Some(ContentionMode::Performance);
    chosen
        .contention
        .acknowledge_caution
        .insert("performance_mode".into());
    assert_eq!(modes(&chosen), ContentionMode::Performance);
}

#[test]
fn lost_cpu_telemetry_holds_for_the_unknown_window_then_escalates() {
    use cedegrid::contention::{YieldCause, YieldStep};
    let input = PolicyInput {
        allocations: vec![allocation(
            "a",
            AllocationPhase::Running,
            resources(2000, 0, 0),
            Some(resources(2000, 0, 0)),
        )],
        facts: rootless(&["a"]),
        ..Default::default()
    };
    let config = config();
    let interval = config.monitor.interval_ms;
    let window = u64::from(
        config
            .contention
            .thresholds(cedegrid::contention::ResourceKind::Cpu)
            .unknown_window_intervals,
    ) * interval;
    let mut lost = snapshot();
    lost.cpu_busy_millicores = None;
    let mut engine = PolicyEngine::new();
    let mut now = 0;
    // Inside the window the budget is zero for admission but no step is taken.
    while now < window {
        let held = engine.evaluate(&config, &lost, &input, now).unwrap();
        assert_eq!(held.managed_budget.cpu_millicores, 0);
        assert!(
            held.actions.is_empty() && held.would_drain.is_empty(),
            "t={now}"
        );
        now += interval;
    }
    let after = engine.evaluate(&config, &lost, &input, now).unwrap();
    let action = after.actions.first().expect("escalates after the window");
    assert_eq!(
        (action.step, action.cause),
        (YieldStep::Cap, YieldCause::UnknownMeasurement)
    );
    assert!(after.would_drain.is_empty());
}

/// While CPU telemetry is unknown, Performance holds own CPU at Neutral's fair share of the
/// last known view with the unknown as one more principal, and never steps below it; with no
/// view yet it takes no new step. Yield First and Neutral keep a CPU budget of 0 (FR-008,
/// FR-022).
#[test]
fn unknown_cpu_in_performance_holds_the_fair_share_of_the_last_view() {
    use cedegrid::contention::{ContentionMode, YieldCause, YieldStep};
    let input = PolicyInput {
        allocations: vec![allocation(
            "a",
            AllocationPhase::Running,
            resources(5000, 0, 0),
            Some(resources(5000, 0, 0)),
        )],
        facts: rootless(&["a"]),
        ..Default::default()
    };
    let mode = |mode: ContentionMode| {
        let mut cfg = config();
        cfg.cpu.reserve_physical_cores = 0;
        cfg.contention.default_mode = mode;
        cfg
    };
    let cfg = mode(ContentionMode::Performance);
    let interval = cfg.monitor.interval_ms;
    let window = u64::from(
        cfg.contention
            .thresholds(cedegrid::contention::ResourceKind::Cpu)
            .unknown_window_intervals,
    ) * interval;
    // Own 5000m beside 2000m external, unsaturated: the view the hold comes from.
    let mut known = snapshot();
    known.cpu_busy_millicores = Some(7000);
    let mut lost = known.clone();
    lost.cpu_busy_millicores = None;
    let held = |decision: &cedegrid::model::Decision| {
        decision
            .reason_codes
            .iter()
            .any(|r| r.code == "cpu_unknown_fair_share_hold")
    };

    let mut engine = PolicyEngine::new();
    let first = engine.evaluate(&cfg, &known, &input, 0).unwrap();
    assert!(first.actions.is_empty() && !held(&first));
    // 8000m between own work, the 2000m external demand and the unknown: 3000m each beside it.
    let mut now = interval;
    while now <= window {
        let decision = engine.evaluate(&cfg, &lost, &input, now).unwrap();
        assert_eq!(decision.managed_budget.cpu_millicores, 3000, "t={now}");
        assert!(held(&decision), "t={now}");
        assert!(decision.actions.is_empty(), "t={now}: steps hold");
        now += interval;
    }
    let mut capped = false;
    for _ in 0..20 {
        let decision = engine.evaluate(&cfg, &lost, &input, now).unwrap();
        assert_eq!(decision.managed_budget.cpu_millicores, 3000, "t={now}");
        for action in &decision.actions {
            assert_eq!(
                (action.step, action.cause),
                (YieldStep::Cap, YieldCause::UnknownMeasurement),
                "t={now}"
            );
            assert!(action.target >= Some(3000), "t={now}: {action:?}");
            capped = true;
        }
        assert!(decision.would_drain.is_empty());
        now += interval;
    }
    assert!(capped, "own work above the hold is capped to it");

    // No view yet: no new step, however long CPU stays unknown.
    let mut engine = PolicyEngine::new();
    for step in 0..3 * window / interval {
        let decision = engine
            .evaluate(&cfg, &lost, &input, step * interval)
            .unwrap();
        assert_eq!(decision.managed_budget.cpu_millicores, 0);
        assert!(decision.actions.is_empty(), "t={}", step * interval);
    }

    // Yield First and Neutral keep a budget of 0 while CPU is unknown.
    for chosen in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
        let cfg = mode(chosen);
        let mut engine = PolicyEngine::new();
        engine.evaluate(&cfg, &known, &input, 0).unwrap();
        let decision = engine.evaluate(&cfg, &lost, &input, interval).unwrap();
        assert_eq!(decision.managed_budget.cpu_millicores, 0, "{chosen:?}");
        assert!(!held(&decision), "{chosen:?}");
    }
}

/// One CPU principal for the snapshot.
fn cpu_principal(uid: u32, is_owner: bool, cpu_millicores: u64) -> PrincipalUsage {
    PrincipalUsage {
        uid: Some(uid),
        is_owner,
        cpu_millicores,
        ..PrincipalUsage::default()
    }
}

/// Principals (FR-059): system accounts are background demand; a new user joins
/// only after the principal window and leaves only after `principal_leave_s`
/// below the threshold. Until it joins, its usage counts toward the unattributed
/// principal, so contention is still seen at once.
#[test]
fn system_accounts_are_background_and_users_join_and_leave_after_a_window() {
    let own = PolicyInput {
        allocations: vec![allocation(
            "own",
            AllocationPhase::Running,
            resources(4000, 0, 0),
            Some(resources(4000, 0, 0)),
        )],
        facts: rootless(&["own"]),
        ..Default::default()
    };
    // Without a CPU reserve the shares come from the whole 8000m.
    let mut cfg = config();
    cfg.cpu.reserve_physical_cores = 0;
    let mut engine = PolicyEngine::new();
    // Saturated: own 4000m, root 1000m, two users.
    let mut share = |owner: PrincipalUsage, users: &[(u32, u64)], now: u64| {
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(8000);
        let mut principals = vec![owner, cpu_principal(0, false, 1000)];
        principals.extend(
            users
                .iter()
                .map(|(uid, usage)| cpu_principal(*uid, false, *usage)),
        );
        snap.principals = Some(principals);
        engine.evaluate(&cfg, &snap, &own, now).unwrap().resources
            [&cedegrid::contention::ResourceKey::Cpu]
            .target_share
            .unwrap()
    };
    let owner = || cpu_principal(1000, true, 0);
    let both = [(2001, 1500), (2002, 1500)];
    // Root's 1000m comes off the capacity; the new users form one principal at first.
    for now in [0, 500, 1000] {
        assert_eq!(share(owner(), &both, now), 3500, "at {now}");
    }
    // After four observations each user is its own principal.
    // 7000m among three principals; the remainder goes to the owner.
    assert_eq!(share(owner(), &both, 1500), 2334);
    // A drop shorter than the 60 s leave window does not end membership.
    let dropped = [(2001, 0), (2002, 0)];
    let mut now = 2000;
    while now < 61_500 {
        share(owner(), &dropped, now);
        now += 500;
    }
    assert_eq!(share(owner(), &both, now), 2334);
    // 60 s below the threshold does: back to one pooled principal.
    now += 500;
    let left_at = now + 60_000;
    while now <= left_at {
        share(owner(), &dropped, now);
        now += 500;
    }
    assert_eq!(share(owner(), &both, now), 3500);
    // The account CedeGrid runs as stays the owner principal below uid_min.
    let mut engine = PolicyEngine::new();
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(8000);
    snap.principals = Some(vec![
        cpu_principal(999, true, 500),
        cpu_principal(2001, false, 3500),
    ]);
    let mut decision = None;
    for now in [0, 500, 1000, 1500] {
        decision = Some(engine.evaluate(&cfg, &snap, &own, now).unwrap());
    }
    // Two principals share 8000m; the account's unmanaged 500m comes off its share.
    assert_eq!(
        decision.unwrap().resources[&cedegrid::contention::ResourceKey::Cpu].target_share,
        Some(3500)
    );
}

/// Decisions show, per resource, how much is held back from own work and why,
/// for `doctor` and `status` (SC-016).
#[test]
fn decisions_show_what_each_resource_holds_back_and_why() {
    use cedegrid::contention::ResourceKey;
    let mut cfg = config();
    cfg.contention.ram.scale_up_cooldown_ms = Some(10_000);
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(0, 5000, 0),
            Some(resources(0, 4000, 0)),
        )],
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snapshot(), &input, 0)
        .unwrap();
    let held = |key: ResourceKey| decision.resources[&key].held_back.clone().unwrap();
    // CPU: one reserved physical core of four, 2000m, exceeds the 10% margin.
    let cpu = held(ResourceKey::Cpu);
    assert_eq!((cpu.reserve, cpu.margin), (2000, 0), "{cpu:?}");
    // RAM: the 1000 MiB floor; the job's 1000 MiB above its usage; admission
    // waits for the 10 s cooldown with 24 000 MiB free beyond the charges.
    let ram = held(ResourceKey::Ram);
    assert_eq!(ram.floor, 1000, "{ram:?}");
    assert_eq!(ram.charge_shortfall, 1000, "{ram:?}");
    assert_eq!(ram.cooldown, 24_000, "{ram:?}");
    assert_eq!(ram.unknown, 0, "{ram:?}");
}

/// Each resource reports its admission: open, held while it waits for a
/// measurement or a cooldown, or blocked while own work on it yields or its floor
/// is breached (FR-028, FR-038).
#[test]
fn each_resource_reports_its_admission() {
    use cedegrid::contention::{Admission, ResourceKey};
    let admission =
        |decision: &Decision, key: ResourceKey| decision.resources[&key].admission.clone().unwrap();
    let calm = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(admission(&calm, ResourceKey::Cpu), Admission::Open);
    assert_eq!(admission(&calm, ResourceKey::Ram), Admission::Open);
    // Own work above its CPU allowance is capped: CPU admission is blocked.
    let mut busy = snapshot();
    busy.cpu_busy_millicores = Some(8000);
    let stepped = PolicyEngine::new()
        .evaluate(
            &config(),
            &busy,
            &PolicyInput {
                allocations: vec![allocation(
                    "job",
                    AllocationPhase::Running,
                    resources(1000, 0, 0),
                    Some(resources(7000, 0, 0)),
                )],
                facts: rootless(&["job"]),
                ..PolicyInput::default()
            },
            0,
        )
        .unwrap();
    assert!(
        matches!(
            admission(&stepped, ResourceKey::Cpu),
            Admission::Blocked { .. }
        ),
        "{:?}",
        stepped.resources[&ResourceKey::Cpu]
    );
    // A breached RAM floor blocks RAM admission; unknown CPU telemetry holds CPU.
    let mut breached = snapshot();
    breached.available_ram_mib = Some(700);
    breached.cpu_busy_millicores = None;
    let decision = PolicyEngine::new()
        .evaluate(&config(), &breached, &PolicyInput::default(), 0)
        .unwrap();
    assert!(matches!(
        admission(&decision, ResourceKey::Ram),
        Admission::Blocked { .. }
    ));
    assert!(matches!(
        admission(&decision, ResourceKey::Cpu),
        Admission::Held { .. }
    ));
    // RAM waits for its cooldown.
    let mut cfg = config();
    cfg.contention.ram.scale_up_cooldown_ms = Some(10_000);
    let cooling = PolicyEngine::new()
        .evaluate(&cfg, &snapshot(), &PolicyInput::default(), 0)
        .unwrap();
    assert!(matches!(
        admission(&cooling, ResourceKey::Ram),
        Admission::Held { .. }
    ));
}

/// Repeated memory reclaim lengthens the RAM cooldown by backoff, and five quiet
/// minutes reset it (FR-061).
#[test]
fn reclaim_backoff_doubles_and_resets_after_a_quiet_period() {
    let mut engine = PolicyEngine::new();
    // The job's own growth breaches the floor, so no external peak holds
    // admission; 2000 MiB stay external throughout.
    let mut evaluate = |job: u64, now: u64| {
        let input = PolicyInput {
            allocations: vec![allocation(
                "job",
                AllocationPhase::Running,
                resources(0, 4000, 0),
                Some(resources(0, job, 0)),
            )],
            facts: BTreeMap::from([(
                "job".into(),
                AllocationFacts {
                    hint_ready: true,
                    observes_drain: true,
                    ..AllocationFacts::default()
                },
            )]),
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        snap.available_ram_mib = Some(32_000 - 2000 - job);
        engine.evaluate(&config(), &snap, &input, now).unwrap()
    };
    // How long RAM admission stays closed after a breach that starts at `start`
    // and clears half a second later.
    let mut cycle = |start: u64| -> u64 {
        assert!(
            !evaluate(29_300, start).actions.is_empty(),
            "no reclaim at {start}"
        );
        let cleared = start + 500;
        let mut now = cleared;
        while evaluate(4000, now).admission_headroom.ram_mib == 0 {
            now += 500;
            assert!(now < start + 60_000, "closed since {start}");
        }
        now - cleared
    };
    assert_eq!(cycle(0), 1000);
    assert_eq!(cycle(60_000), 2000);
    assert_eq!(cycle(120_000), 4000);
    // Five quiet minutes after the last episode reset it.
    assert_eq!(cycle(120_000 + 300_000 + 5000), 1000);
}

/// One interval of a hint-ready RAM job using `job` MiB, with 2000 MiB external on
/// the 32 000 MiB host; returns whether the job holds a RAM step.
fn ram_job_step(engine: &mut PolicyEngine, cfg: &Config, job: u64, now: u64) -> bool {
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(0, 30_000, 0),
            Some(resources(0, job, 0)),
        )],
        facts: BTreeMap::from([(
            "job".into(),
            AllocationFacts {
                hint_ready: true,
                observes_drain: true,
                ..AllocationFacts::default()
            },
        )]),
        ..PolicyInput::default()
    };
    let mut snap = snapshot();
    snap.available_ram_mib = Some(32_000 - 2000 - job);
    engine
        .evaluate(cfg, &snap, &input, now)
        .unwrap()
        .actions
        .iter()
        .any(|a| a.allocation_id == "job" && a.resource == cedegrid::contention::ResourceKey::Ram)
}

/// Own work that regrows into the floor after each reversal backs off: each
/// episode's reversal waits the doubling reclaim backoff, so the gaps grow instead
/// of cycling at a fixed period (FR-013, FR-061).
#[test]
fn breach_steps_back_off_when_own_work_regrows_into_the_floor() {
    let cfg = config();
    let mut engine = PolicyEngine::new();
    let (mut job, mut stepped, mut reversals) = (29_300u64, false, vec![]);
    let mut now = 0;
    while now < 250_000 && reversals.len() < 5 {
        let step = ram_job_step(&mut engine, &cfg, job, now);
        if stepped && !step {
            reversals.push(now);
        }
        stepped = step;
        // A compliant worker shrinks under its hint and regrows without one.
        job = if step { 4000 } else { 29_300 };
        now += 500;
    }
    assert_eq!(reversals.len(), 5, "{reversals:?}");
    let gaps: Vec<u64> = reversals.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.windows(2).all(|g| g[1] > g[0]), "{gaps:?}");
}

/// A breach step reverses only after free memory has stayed above the floor by
/// what the step had to free, or the noise tolerance if larger (FR-013).
#[test]
fn a_breach_step_holds_until_the_floor_has_room_for_what_it_freed() {
    let cfg = config();
    let mut engine = PolicyEngine::new();
    // 700 MiB free against the 1000 MiB floor: the step has to free 300 MiB.
    assert!(ram_job_step(&mut engine, &cfg, 29_300, 0));
    // Just under its 29 000 MiB target the floor has 100 MiB of room: it holds.
    let mut now = 500;
    while now < 120_000 {
        assert!(
            ram_job_step(&mut engine, &cfg, 28_900, now),
            "reversed at {now}"
        );
        now += 500;
    }
    // 400 MiB of room is more than the 300 MiB it had to free and the 256 MiB
    // noise tolerance: it reverses.
    let start = now;
    while ram_job_step(&mut engine, &cfg, 28_600, now) {
        now += 500;
        assert!(now < start + 60_000, "still stepped at {now}");
    }
}

/// A RAM reclaim backoff holds RAM admission only: CPU stays open with its
/// headroom, each by its own cooldown (T086a).
#[test]
fn a_ram_reclaim_backoff_holds_only_ram_admission() {
    use cedegrid::contention::{Admission, ResourceKey};
    let mut cfg = config();
    // A node-wide cooldown that has not elapsed must not couple CPU to RAM.
    cfg.gpu.scale_up_cooldown_ms = Some(60_000);
    // The step's reversal waits the 120 s backoff too; a 30 s cooldown on top
    // leaves RAM held by the backoff alone once the step has reversed.
    cfg.contention.ram.reclaim_backoff_ms = Some(120_000);
    cfg.contention.ram.reclaim_backoff_max_ms = Some(120_000);
    cfg.contention.ram.scale_up_cooldown_ms = Some(30_000);
    let mut engine = PolicyEngine::new();
    let mut evaluate = |job: u64, now: u64| {
        let input = PolicyInput {
            allocations: vec![allocation(
                "job",
                AllocationPhase::Running,
                resources(1000, 4000, 0),
                Some(resources(1000, job, 0)),
            )],
            facts: BTreeMap::from([(
                "job".into(),
                AllocationFacts {
                    hint_ready: true,
                    observes_drain: true,
                    ..AllocationFacts::default()
                },
            )]),
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(1000);
        snap.available_ram_mib = Some(32_000 - 2000 - job);
        engine.evaluate(&cfg, &snap, &input, now).unwrap()
    };
    assert!(!evaluate(29_300, 0).actions.is_empty());
    // Once the RAM step has reversed, the backoff alone holds RAM.
    let mut now = 500;
    let decision = loop {
        let decision = evaluate(4000, now);
        if decision.actions.is_empty() {
            break decision;
        }
        now += 500;
        assert!(
            now < 200_000,
            "the RAM step did not reverse: {:?}",
            decision.actions
        );
    };
    let admission = |key: ResourceKey| decision.resources[&key].admission.clone().unwrap();
    assert!(
        matches!(&admission(ResourceKey::Ram), Admission::Held { code, .. } if code == "admission_reclaim_backoff"),
        "{:?}",
        admission(ResourceKey::Ram)
    );
    assert_eq!(decision.admission_headroom.ram_mib, 0);
    assert_eq!(admission(ResourceKey::Cpu), Admission::Open);
    assert!(
        decision.admission_headroom.cpu_millicores > 0,
        "{:?}",
        decision.admission_headroom
    );
}

/// CPU admission charges running opportunistic work at its p95 usage over the
/// admission window, not its request, once it has a full window of samples;
/// pending and Guaranteed work keep their requests.
#[test]
fn cpu_admission_charges_running_opportunistic_work_at_recent_usage() {
    let mut guaranteed = allocation(
        "guaranteed",
        AllocationPhase::Running,
        resources(1000, 0, 0),
        Some(resources(500, 0, 0)),
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let mut engine = PolicyEngine::new();
    let mut headroom = |usage: u64, now: u64| {
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "opportunistic",
                    AllocationPhase::Running,
                    resources(4000, 0, 0),
                    Some(resources(usage, 0, 0)),
                ),
                guaranteed.clone(),
                allocation(
                    "pending",
                    AllocationPhase::Pending,
                    resources(500, 0, 0),
                    None,
                ),
            ],
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        // 500m external on 8000m with a 2000m reserve: a 5500m budget.
        snap.cpu_busy_millicores = Some(usage + 500 + 500);
        engine
            .evaluate(&config(), &snap, &input, now)
            .unwrap()
            .admission_headroom
            .cpu_millicores
    };
    // Until a full window of samples, a job in its startup phase is charged its
    // request.
    let mut now = 0;
    while now < 60_000 {
        assert_eq!(headroom(1000, now), 5500 - 4000 - 1000 - 500, "at {now}");
        now += 500;
    }
    // Then: the opportunistic usage, the Guaranteed request and the pending one.
    assert_eq!(headroom(1000, now), 5500 - 1000 - 1000 - 500);
    now += 500;
    // More than 5% of the window at 3000m sets the p95.
    for _ in 0..10 {
        headroom(3000, now);
        now += 500;
    }
    assert_eq!(headroom(1000, now), 5500 - 3000 - 1000 - 500);
    // Once those samples leave the 60 s window, the charge falls with usage.
    now += 60_500;
    assert_eq!(headroom(500, now), 5500 - 500 - 1000 - 500);
}

/// While a running allocation has a held CPU step, CPU admission charges it at
/// the larger of its request and its p95 usage from before the step, with no
/// credit, so usage a cap holds back never becomes room for more work: report
/// 024 saw a 14th task admitted with all 13 capped (FR-034).
#[test]
fn usage_held_back_by_a_cpu_step_is_not_admission_room() {
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};
    let mut config = config();
    config.contention.default_mode = ContentionMode::YieldFirst;
    let ids: Vec<String> = (0..13).map(|i| format!("t{i}")).collect();
    let mut engine = PolicyEngine::new();
    // Each task runs at its 250m request, or at what a cap allows, or not at all
    // while paused, as the tasks in report 024 did.
    let mut limits: BTreeMap<String, u64> = BTreeMap::new();
    let mut evaluate = |external: u64, now: u64| {
        let usage = |id: &String| limits.get(id).copied().unwrap_or(250).min(250);
        let input = PolicyInput {
            allocations: ids
                .iter()
                .map(|id| {
                    allocation(
                        id,
                        AllocationPhase::Running,
                        resources(250, 0, 0),
                        Some(resources(usage(id), 0, 0)),
                    )
                })
                .collect(),
            facts: rootless(&ids.iter().map(String::as_str).collect::<Vec<_>>()),
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(ids.iter().map(usage).sum::<u64>() + external);
        let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
        limits = decision
            .actions
            .iter()
            .filter(|a| a.resource == ResourceKey::Cpu)
            .filter_map(|a| match a.step {
                YieldStep::Cap => Some((a.allocation_id.clone(), a.target.unwrap_or(250))),
                YieldStep::Pause => Some((a.allocation_id.clone(), 0)),
                _ => None,
            })
            .collect();
        decision
    };
    // A full window at the request: its p95 is the request.
    let mut now = 0;
    while now <= 60_000 {
        evaluate(0, now);
        now += 500;
    }
    // External demand grows, own work steps down, and the held tasks run at
    // their caps for longer than the window.
    let mut busy = evaluate(3500, now);
    for _ in 0..240 {
        now += 500;
        busy = evaluate(3500, now);
    }
    let capped = busy
        .actions
        .iter()
        .filter(|a| a.resource == ResourceKey::Cpu)
        .count();
    assert!(capped > 0, "own work holds CPU steps");
    // Tasks held back earn no credit, and the others run at their request.
    assert_eq!(busy.cpu_admission_credit_millicores, 0);
    assert!(
        busy.admission_headroom.cpu_millicores < 250,
        "no room for a 14th task while 13 are held back: {}",
        busy.admission_headroom.cpu_millicores
    );
    // Once demand recedes and the steps are released, the charge follows usage.
    let mut calm = evaluate(0, now);
    for _ in 0..400 {
        now += 500;
        calm = evaluate(0, now);
    }
    assert!(!calm.actions.iter().any(|a| a.resource == ResourceKey::Cpu));
    assert!(calm.admission_headroom.cpu_millicores > 0);
}

/// Item 026 (FR-028): CPU and RAM report `admission_own_new` while their admission
/// is closed only by the node's own allocations that have no first measurement
/// yet, or by the cooldown only they restarted; never for another cause.
mod admission_own_new {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey};

    /// Running allocations by ID with their usage, `None` when unknown this interval.
    fn decide(
        engine: &mut PolicyEngine,
        config: &Config,
        jobs: &[(&str, Option<(u64, u64)>)],
        external: u64,
        now: u64,
    ) -> Decision {
        let input = PolicyInput {
            allocations: jobs
                .iter()
                .map(|(id, used)| {
                    allocation(
                        id,
                        AllocationPhase::Running,
                        resources(500, 200, 0),
                        used.map(|(cpu, ram)| resources(cpu, ram, 0)),
                    )
                })
                .collect(),
            ..PolicyInput::default()
        };
        let own: u64 = jobs.iter().filter_map(|(_, u)| u.map(|(cpu, _)| cpu)).sum();
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(own + external);
        engine.evaluate(config, &snap, &input, now).unwrap()
    }

    fn flags(decision: &Decision) -> (bool, bool) {
        let flag = |key| {
            decision
                .resources
                .get(&key)
                .is_some_and(|status| status.admission_own_new)
        };
        (flag(ResourceKey::Cpu), flag(ResourceKey::Ram))
    }

    /// An idle node with one measured job, cooled down.
    fn settled(config: &Config) -> (PolicyEngine, u64) {
        let mut engine = PolicyEngine::new();
        let mut now = 0;
        while now <= 30_000 {
            decide(&mut engine, config, &[("old", Some((500, 200)))], 0, now);
            now += 500;
        }
        (engine, now)
    }

    #[test]
    fn new_unmeasured_allocations_and_the_cooldown_they_restart_are_marked() {
        let mut config = config();
        scale_up_cooldowns(&mut config, 5000);
        let (mut engine, mut now) = settled(&config);
        let burst = [("old", Some((500, 200))), ("n1", None), ("n2", None)];
        let first = decide(&mut engine, &config, &burst, 0, now);
        assert!(
            first.resources[&ResourceKey::Cpu].admission
                != Some(cedegrid::contention::Admission::Open)
        );
        assert_eq!(flags(&first), (true, true));
        now += 500;
        let measured = [
            ("old", Some((500, 200))),
            ("n1", Some((500, 200))),
            ("n2", Some((500, 200))),
        ];
        let cooling = decide(&mut engine, &config, &measured, 0, now);
        assert_eq!(
            cooling.resources[&ResourceKey::Cpu]
                .admission
                .as_ref()
                .and_then(|a| a.code()),
            Some("admission_cooling_down")
        );
        assert_eq!(flags(&cooling), (true, true));
        // Once the cooldown passes, admission is open and nothing is marked.
        let mut open = cooling;
        for _ in 0..60 {
            now += 500;
            open = decide(&mut engine, &config, &measured, 0, now);
        }
        assert_eq!(
            open.resources[&ResourceKey::Cpu].admission,
            Some(cedegrid::contention::Admission::Open)
        );
        assert_eq!(flags(&open), (false, false));
        assert!(open.resources.keys().all(|key| !matches!(
            key,
            ResourceKey::GpuMemory(_) | ResourceKey::GpuCompute(_)
        ) || !open.resources[key].admission_own_new));
    }

    #[test]
    fn an_allocation_measured_before_is_not_new() {
        let config = config();
        let (mut engine, now) = settled(&config);
        let decision = decide(&mut engine, &config, &[("old", None)], 0, now);
        assert_eq!(flags(&decision), (false, false));
    }

    #[test]
    fn own_usage_above_host_busy_is_not_new() {
        let config = config();
        let (mut engine, now) = settled(&config);
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "old",
                    AllocationPhase::Running,
                    resources(500, 200, 0),
                    Some(resources(3000, 200, 0)),
                ),
                allocation("n1", AllocationPhase::Running, resources(500, 200, 0), None),
            ],
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(1000);
        let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
        assert!(!flags(&decision).0);
    }

    /// External growth that makes own work step down, and the steps it holds, are
    /// not the burst's own cause, even with new allocations among them.
    #[test]
    fn contention_and_held_steps_are_not_new() {
        let mut config = config();
        config.contention.default_mode = ContentionMode::YieldFirst;
        let (mut engine, mut now) = settled(&config);
        let wants = |id| (id, Some((2000, 200)));
        for _ in 0..4 {
            decide(&mut engine, &config, &[wants("old")], 0, now);
            now += 500;
        }
        // External demand arrives with a new allocation: contention, not the burst.
        for _ in 0..4 {
            let contended = decide(
                &mut engine,
                &config,
                &[wants("old"), ("n1", None)],
                4000,
                now,
            );
            assert!(
                !flags(&contended).0,
                "{:?}",
                contended.resources[&ResourceKey::Cpu].admission
            );
            now += 500;
        }
        // Once measured, own work steps down; a newer allocation under held steps is
        // not the burst either.
        let measured = [wants("old"), ("n1", Some((500, 200)))];
        let mut stepped = decide(&mut engine, &config, &measured, 4000, now);
        for _ in 0..6 {
            now += 500;
            stepped = decide(&mut engine, &config, &measured, 4000, now);
        }
        assert!(
            stepped
                .actions
                .iter()
                .any(|a| a.resource == ResourceKey::Cpu)
        );
        now += 500;
        let later = decide(
            &mut engine,
            &config,
            &[wants("old"), ("n1", Some((500, 200))), ("n2", None)],
            4000,
            now,
        );
        assert!(!flags(&later).0);
    }

    /// A RAM floor breach is not the burst's cause either.
    #[test]
    fn a_floor_breach_is_not_new() {
        let config = config();
        let (mut engine, now) = settled(&config);
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "old",
                    AllocationPhase::Running,
                    resources(500, 200, 0),
                    Some(resources(500, 200, 0)),
                ),
                allocation("n1", AllocationPhase::Running, resources(500, 200, 0), None),
            ],
            ..PolicyInput::default()
        };
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(500);
        snap.available_ram_mib = Some(100);
        let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
        assert!(!flags(&decision).1);
    }
}

/// The schedulable budget adds the CPU that running opportunistic work holds
/// above its p95 usage, so placement and the launch check, which charge requests,
/// admit by usage as the policy does (FR-034).
#[test]
fn the_schedulable_budget_admits_cpu_by_recent_usage() {
    let mut guaranteed = allocation(
        "guaranteed",
        AllocationPhase::Running,
        resources(1000, 0, 0),
        Some(resources(500, 0, 0)),
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let input = PolicyInput {
        allocations: vec![
            allocation(
                "opportunistic",
                AllocationPhase::Running,
                resources(4000, 0, 0),
                Some(resources(1000, 0, 0)),
            ),
            guaranteed,
            allocation(
                "pending",
                AllocationPhase::Pending,
                resources(500, 0, 0),
                None,
            ),
        ],
        ..PolicyInput::default()
    };
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(1000 + 500 + 500);
    let mut engine = PolicyEngine::new();
    let mut evaluate = |now| engine.evaluate(&config(), &snap, &input, now).unwrap();
    // A job 10 s old earns no credit: it is charged its request.
    let young = evaluate(10_000);
    assert_eq!(young.cpu_admission_credit_millicores, 0);
    assert_eq!(
        cedegrid::policy::schedulable_budget(&young).cpu_millicores,
        young.managed_budget.cpu_millicores
    );
    let mut now = 10_000;
    while now < 70_000 {
        now += 500;
        evaluate(now);
    }
    let decision = evaluate(now);
    // A 5500m budget; admission charges 1000 + 1000 + 500, requests 4000 + 1000 + 500.
    assert_eq!(decision.admission_headroom.cpu_millicores, 3000);
    let schedulable = cedegrid::policy::schedulable_budget(&decision);
    assert_eq!(schedulable.cpu_millicores, 5500 + 3000);
    assert_eq!(
        schedulable.cpu_millicores - (4000 + 1000 + 500),
        decision.admission_headroom.cpu_millicores
    );
    // RAM and GPU memory keep the budget.
    assert_eq!(schedulable.ram_mib, decision.managed_budget.ram_mib);
}

/// Admission headroom for memory uses the external peak over the peak window,
/// while yielding follows current usage (FR-060).
#[test]
fn memory_headroom_uses_the_external_peak_over_its_window() {
    let mut cfg = config();
    cfg.contention.ram.memory_peak_window_s = Some(10);
    // A shorter window is a caution option.
    cfg.contention
        .acknowledge_caution
        .insert("relaxed_detection".into());
    let mut engine = PolicyEngine::new();
    let mut headroom = |available: u64, now: u64| {
        let mut snap = snapshot();
        snap.available_ram_mib = Some(available);
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .ram_mib
    };
    // 32000 MiB total, a 1000 MiB floor and no own work.
    assert_eq!(headroom(26_000, 0), 25_000);
    assert_eq!(headroom(16_000, 500), 15_000);
    assert_eq!(headroom(26_000, 1000), 15_000);
    assert_eq!(headroom(26_000, 10_000), 15_000);
    assert_eq!(headroom(26_000, 10_500), 25_000);
}

/// One unknown RAM interval keeps the external peaks: once RAM is known again, admission
/// still subtracts the window's peak until it ages out (FR-060, POL-4).
#[test]
fn an_unknown_ram_interval_keeps_the_external_peaks() {
    let mut cfg = config();
    cfg.contention.ram.memory_peak_window_s = Some(10);
    cfg.contention
        .acknowledge_caution
        .insert("relaxed_detection".into());
    let mut engine = PolicyEngine::new();
    let mut headroom = |available: Option<u64>, now: u64| {
        let mut snap = snapshot();
        snap.available_ram_mib = available;
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .ram_mib
    };
    assert_eq!(headroom(Some(26_000), 0), 25_000);
    assert_eq!(headroom(Some(16_000), 500), 15_000);
    assert_eq!(headroom(None, 1000), 0);
    assert_eq!(headroom(Some(26_000), 1500), 15_000);
    assert_eq!(headroom(Some(26_000), 10_000), 15_000);
    assert_eq!(headroom(Some(26_000), 10_500), 25_000);
}

/// A quiet GPU context's memory counts toward the device's external peak.
#[test]
fn a_quiet_gpu_context_counts_toward_the_device_peak() {
    let mut cfg = config();
    cfg.contention.gpu_memory.memory_peak_window_s = Some(10);
    // A shorter window is a caution option.
    cfg.contention
        .acknowledge_caution
        .insert("relaxed_detection".into());
    let mut engine = PolicyEngine::new();
    let mut headroom = |used: u64, now: u64| {
        // A healthy observation of one quiet external context, as in D3.
        let mut snap = large_gpu_snapshot(used, vec![4242]);
        snap.gpus[0].external_compute = ComputeActivity::Quiet;
        snap.gpus[0].compute_sample_id = Some(now + 1);
        snap.gpus[0].observation = Some(gpu_observation(
            &snap,
            GpuExecutionCapability::ContentionAware,
        ));
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .get("GPU-test")
            .copied()
    };
    // 24576 MiB with a 1000 MiB floor; the quiet context holds 6000 MiB, then frees
    // 4000 MiB, which stay reserved for it until the window passes.
    headroom(6000, 0);
    assert_eq!(headroom(6000, 500), Some(17_576));
    assert_eq!(headroom(2000, 1000), Some(17_576));
    assert_eq!(headroom(2000, 10_500), Some(21_576));
}

/// CPU and RAM each wait for their own scale-up cooldown, from their own stable
/// time (FR-034): RAM instability does not restart CPU's cooldown. Admission
/// opens once both have cooled.
#[test]
fn cpu_and_ram_cool_down_independently() {
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = Some(60_000);
    cfg.contention.cpu.scale_up_cooldown_ms = Some(6000);
    cfg.contention.ram.scale_up_cooldown_ms = Some(2000);
    let mut engine = PolicyEngine::new();
    let mut allowed = |available: Option<u64>, now: u64| {
        let mut snap = snapshot();
        snap.available_ram_mib = available;
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), now)
            .unwrap()
            .cpu_ram_expansion_allowed
    };
    assert!(!allowed(Some(26_000), 0));
    // RAM telemetry missing at 3 s restarts only RAM's 2 s cooldown.
    assert!(!allowed(None, 3000));
    assert!(!allowed(Some(26_000), 3500));
    assert!(!allowed(Some(26_000), 5499));
    // CPU's 6 s ran from 0, and RAM's 2 s from 3.5 s, so both are cool at 6 s.
    assert!(allowed(Some(26_000), 6000));
}

/// Each further RAM reclaim episode within the peak window doubles an extra
/// delay before own work may grow on RAM again: 1 s, 2 s, 4 s (FR-061).
#[test]
fn repeated_ram_reclaim_episodes_back_off_exponentially() {
    let cfg = config();
    let mut engine = PolicyEngine::new();
    let mut breached = snapshot();
    breached.available_ram_mib = Some(700);
    let mut now = 0;
    let mut delays = vec![];
    for episode in 1..=3 {
        // A plain command holding RAM is drained for the floor: the episode starts.
        let id = format!("a{episode}");
        let input = PolicyInput {
            allocations: vec![allocation(
                &id,
                AllocationPhase::Running,
                resources(0, 8000, 0),
                Some(resources(0, 8000, 0)),
            )],
            facts: rootless(&[id.as_str()]),
            ..Default::default()
        };
        let drained = engine.evaluate(&cfg, &breached, &input, now).unwrap();
        assert!(drained.would_drain.contains(&id), "episode {episode}");
        // The allocation is released; RAM is safe again from the next interval.
        now += 500;
        let calm = now;
        loop {
            let decision = engine
                .evaluate(&cfg, &snapshot(), &PolicyInput::default(), now)
                .unwrap();
            if decision.admission_headroom.ram_mib > 0 {
                delays.push(now - calm);
                break;
            }
            now += 500;
            assert!(
                now < calm + 60_000,
                "episode {episode} never allowed RAM again"
            );
        }
        now += 500;
    }
    assert_eq!(delays, [1000, 2000, 4000]);
}

/// External memory peaks follow each external process: a live process keeps its
/// peak for the window, and its peak goes the moment it exits (FR-060).
#[test]
fn external_memory_peaks_follow_each_external_process() {
    let mut engine = PolicyEngine::new();
    let mut headroom = |job_mib: Option<u64>, now: u64| {
        let mut snap = snapshot();
        snap.available_ram_mib = Some(32_000 - job_mib.unwrap_or(0));
        snap.external_processes = Some(
            job_mib
                .map(|ram_mib| ExternalProcessMemory {
                    pid: 100,
                    start_time: 1,
                    ram_mib,
                    gpu_memory_mib: BTreeMap::new(),
                })
                .into_iter()
                .collect(),
        );
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .ram_mib
    };
    // 32000 MiB with a 1000 MiB floor; an external job holds 10000 MiB.
    assert_eq!(headroom(Some(10_000), 0), 21_000);
    // It shrinks but lives: its peak keeps admission closed.
    assert_eq!(headroom(Some(1000), 500), 21_000);
    assert_eq!(headroom(Some(1000), 60_000), 21_000);
    // It exits: its peak goes at once.
    assert_eq!(headroom(None, 60_500), 31_000);
}

/// External usage beyond the listed processes, such as processes past the
/// collector's 64, shares one peak over the window and is never dropped (FR-060).
#[test]
fn external_usage_beyond_the_listed_processes_keeps_its_peak() {
    let mut engine = PolicyEngine::new();
    let mut headroom = |unlisted: u64, now: u64| {
        let mut snap = snapshot();
        // 64 listed processes hold 100 MiB each.
        snap.available_ram_mib = Some(32_000 - 6400 - unlisted);
        snap.external_processes = Some(
            (0..64)
                .map(|i| ExternalProcessMemory {
                    pid: 100 + i,
                    start_time: 1,
                    ram_mib: 100,
                    gpu_memory_mib: BTreeMap::new(),
                })
                .collect(),
        );
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .ram_mib
    };
    // 32000 MiB with a 1000 MiB floor; unlisted processes hold 3600 MiB more.
    assert_eq!(headroom(3600, 0), 21_000);
    // They exit: their usage stays in the shared peak for the 300 s window.
    assert_eq!(headroom(0, 500), 21_000);
    assert_eq!(headroom(0, 299_000), 21_000);
    assert_eq!(headroom(0, 300_500), 24_600);
}

/// US5: one device's admission under a mode, activity, external contexts and
/// memory observation, as the policy reports it on GPU compute and the headroom
/// it offers.
fn us5_device(
    mode: cedegrid::contention::ContentionMode,
    activity: ComputeActivity,
    pids: Option<Vec<u32>>,
    used: Option<u64>,
    never_share: bool,
) -> (Config, Snapshot) {
    let mut cfg = config();
    cfg.contention.default_mode = mode;
    cfg.contention
        .acknowledge_caution
        .insert("performance_mode".into());
    cfg.contention.never_share_occupied_gpus = never_share;
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_compute = activity;
    snap.gpus[0].external_process_ids = pids;
    snap.gpus[0].used_memory_mib = used;
    (cfg, snap)
}

fn us5_admission(cfg: &Config, snap: &Snapshot) -> (cedegrid::contention::Admission, u64) {
    use cedegrid::contention::ResourceKey;
    let decision = PolicyEngine::new()
        .evaluate(cfg, snap, &PolicyInput::default(), 0)
        .unwrap();
    let compute = decision.resources[&ResourceKey::GpuCompute("GPU-test".into())]
        .admission
        .clone()
        .unwrap();
    let headroom = decision
        .admission_headroom
        .gpu_memory_mib
        .get("GPU-test")
        .copied()
        .unwrap_or(0);
    (compute, headroom)
}

/// GPU admission follows the device's mode (FR-026, FR-027, FR-064): Neutral and
/// Performance admit on quiet or active devices and under activity of unknown
/// amount, which counts as one principal; Yield First requires a quiet device;
/// unknown memory blocks; with `never_share_occupied_gpus` an occupied device or
/// an unknown process list blocks.
#[test]
fn gpu_admission_follows_the_mode() {
    use ComputeActivity::*;
    use cedegrid::contention::{Admission, ContentionMode::*};
    for mode in [Neutral, Performance] {
        for activity in [Idle, Quiet, Active, Unknown] {
            let (cfg, snap) = us5_device(mode, activity, Some(vec![987]), Some(6000), false);
            let (admission, headroom) = us5_admission(&cfg, &snap);
            assert_eq!(admission, Admission::Open, "{mode:?} {activity:?}");
            // 16000 - 1000 floor - 6000 external.
            assert_eq!(headroom, 9000, "{mode:?} {activity:?}");
        }
    }
    for (activity, open) in [
        (Idle, true),
        (Quiet, true),
        (Active, false),
        (Unknown, false),
    ] {
        let (cfg, snap) = us5_device(YieldFirst, activity, Some(vec![987]), Some(6000), false);
        let (admission, headroom) = us5_admission(&cfg, &snap);
        assert_eq!(
            admission == Admission::Open,
            open,
            "{activity:?}: {admission:?}"
        );
        assert_eq!(headroom > 0, open, "{activity:?}");
    }
    let (cfg, snap) = us5_device(Neutral, Quiet, Some(vec![]), None, false);
    let (admission, headroom) = us5_admission(&cfg, &snap);
    assert_ne!(admission, Admission::Open);
    assert_eq!(headroom, 0);
    for (pids, open) in [
        (Some(vec![]), true),
        (Some(vec![987]), false),
        (None, false),
    ] {
        let (cfg, snap) = us5_device(Neutral, Quiet, pids.clone(), Some(6000), true);
        let (admission, _) = us5_admission(&cfg, &snap);
        assert_eq!(
            admission == Admission::Open,
            open,
            "{pids:?}: {admission:?}"
        );
    }
}

/// A system account's context the collector found background makes no activity unknown,
/// even without a fresh sample, so a Yield First device stays open; its memory stays
/// external (FR-021).
#[test]
fn a_background_gpu_context_is_not_unknown_activity() {
    use cedegrid::contention::{Admission, ContentionMode::YieldFirst, ResourceKey};
    let (cfg, mut snap) = us5_device(
        YieldFirst,
        ComputeActivity::Idle,
        Some(vec![42]),
        Some(6000),
        false,
    );
    let compute = ResourceKey::GpuCompute("GPU-test".into());
    let evaluate = |snap: &Snapshot| {
        let mut engine = PolicyEngine::new();
        engine
            .evaluate(&cfg, snap, &PolicyInput::default(), 0)
            .unwrap();
        // The same sample again: no fresh evidence.
        engine
            .evaluate(&cfg, snap, &PolicyInput::default(), 500)
            .unwrap()
    };
    let unknown = evaluate(&snap);
    assert_eq!(
        unknown.resources[&compute].activity,
        Some(ComputeActivity::Unknown)
    );
    snap.gpus[0].background_process_ids = vec![42];
    let background = evaluate(&snap);
    assert_eq!(
        background.resources[&compute].activity,
        Some(ComputeActivity::Idle)
    );
    assert_eq!(
        background.resources[&compute].admission,
        Some(Admission::Open)
    );
    let memory = &background.resources[&ResourceKey::GpuMemory("GPU-test".into())];
    assert_eq!(memory.external, Some(6000));
}

/// The launch recheck applies the same admission to the job's effective mode on a
/// fresh observation, and refuses a request that would leave less than the floor
/// free (FR-026, FR-027).
#[test]
fn launch_recheck_admits_by_mode_within_the_floor() {
    use ComputeActivity::*;
    use cedegrid::contention::ContentionMode::*;
    let check = |cfg: &Config, snap: &Snapshot, mib: u64| {
        cedegrid::policy::validate_gpu_launch_contract(
            cfg,
            snap,
            &resources(1, 1, mib),
            AllocationClass::Opportunistic,
        )
    };
    for mode in [YieldFirst, Neutral, Performance] {
        for activity in [Idle, Quiet, Active, Unknown] {
            let (cfg, snap) = us5_device(mode, activity, Some(vec![987]), Some(6000), false);
            let admitted = mode != YieldFirst || matches!(activity, Idle | Quiet);
            assert_eq!(
                check(&cfg, &snap, 500).is_ok(),
                admitted,
                "{mode:?} {activity:?}"
            );
        }
    }
    let (cfg, snap) = us5_device(Neutral, Active, Some(vec![987]), Some(6000), false);
    // 16000 - 6000 used - 1000 floor leaves 9000.
    assert!(check(&cfg, &snap, 9000).is_ok());
    assert!(check(&cfg, &snap, 9001).is_err());
    let (cfg, snap) = us5_device(Neutral, Quiet, Some(vec![987]), Some(6000), true);
    assert!(check(&cfg, &snap, 500).is_err());
    let (cfg, snap) = us5_device(Neutral, Quiet, Some(vec![]), None, false);
    assert!(check(&cfg, &snap, 500).is_err());
    // Guaranteed GPU work no longer needs a legacy contention-aware contract.
    let (cfg, snap) = us5_device(Neutral, Quiet, Some(vec![987]), Some(6000), false);
    cedegrid::policy::validate_gpu_launch_contract(
        &cfg,
        &snap,
        &resources(1, 1, 500),
        AllocationClass::Guaranteed,
    )
    .unwrap();
}

/// The policy's GPU admission and the launch recheck call the same pure function,
/// so they agree on every combination (T087b).
#[test]
fn policy_and_launch_recheck_agree_on_gpu_admission() {
    use ComputeActivity::*;
    use cedegrid::contention::{Admission, ContentionMode::*};
    for mode in [YieldFirst, Neutral, Performance] {
        for activity in [Idle, Quiet, Active, Unknown] {
            for pids in [Some(vec![]), Some(vec![987]), None] {
                for never_share in [false, true] {
                    let (cfg, snap) =
                        us5_device(mode, activity, pids.clone(), Some(6000), never_share);
                    let (admission, _) = us5_admission(&cfg, &snap);
                    let recheck = cedegrid::policy::validate_gpu_launch_contract(
                        &cfg,
                        &snap,
                        &resources(1, 1, 500),
                        AllocationClass::Opportunistic,
                    );
                    assert_eq!(
                        admission == Admission::Open,
                        recheck.is_ok(),
                        "{mode:?} {activity:?} {pids:?} {never_share}: {admission:?} {recheck:?}"
                    );
                }
            }
        }
    }
}

/// Own GPU work gets a compute share under contention by pause duty-cycling
/// (FR-021, FR-022): with external compute active, Neutral and Performance cap a
/// lone allocation at half the device and Yield First pauses it; a quiet device
/// takes no step; Guaranteed work takes none (FR-024).
#[test]
fn gpu_compute_shares_follow_the_mode_under_contention() {
    use ComputeActivity::*;
    use cedegrid::contention::{ContentionMode::*, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let step = |mode, activity, class| {
        let (cfg, mut snap) = us5_device(mode, activity, Some(vec![987]), Some(8000), false);
        snap.gpus[0].compute_sample_id = Some(1);
        let mut job = allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        );
        job.class = class;
        let decision = PolicyEngine::new()
            .evaluate(
                &cfg,
                &snap,
                &PolicyInput {
                    allocations: vec![job],
                    facts: rootless(&["job"]),
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap();
        decision
            .actions
            .iter()
            .find(|a| a.resource == key)
            .map(|a| (a.step, a.target))
    };
    let opportunistic = AllocationClass::Opportunistic;
    assert_eq!(
        step(Neutral, Active, opportunistic),
        Some((YieldStep::Cap, Some(500)))
    );
    assert_eq!(
        step(Performance, Active, opportunistic),
        Some((YieldStep::Cap, Some(500)))
    );
    assert_eq!(
        step(YieldFirst, Active, opportunistic).map(|s| s.0),
        Some(YieldStep::Pause)
    );
    assert_eq!(step(Neutral, Quiet, opportunistic), None);
    assert_eq!(step(Neutral, Active, AllocationClass::Guaranteed), None);
}

/// While device telemetry is unknown (here a missing process list), GPU compute caps and
/// pauses hold rather than being lifted (FR-008, GPU-3).
#[test]
fn gpu_compute_steps_hold_while_device_telemetry_is_unknown() {
    use ComputeActivity::*;
    use cedegrid::contention::{ContentionMode::*, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    for (mode, expected) in [(Neutral, YieldStep::Cap), (YieldFirst, YieldStep::Pause)] {
        let (cfg, mut known) = us5_device(mode, Active, Some(vec![987]), Some(8000), false);
        known.gpus[0].compute_sample_id = Some(1);
        let mut unknown = known.clone();
        unknown.gpus[0].external_process_ids = None;
        let input = PolicyInput {
            allocations: vec![allocation(
                "job",
                AllocationPhase::Running,
                resources(100, 100, 2000),
                Some(resources(100, 100, 2000)),
            )],
            facts: rootless(&["job"]),
            ..PolicyInput::default()
        };
        let step = |decision: &Decision| {
            decision
                .actions
                .iter()
                .find(|a| a.resource == key)
                .map(|a| (a.step, a.target))
        };
        let mut engine = PolicyEngine::new();
        let first = step(&engine.evaluate(&cfg, &known, &input, 0).unwrap());
        assert_eq!(first.map(|s| s.0), Some(expected), "{mode:?}");
        for i in 1..6u64 {
            let held = engine.evaluate(&cfg, &unknown, &input, i * 500).unwrap();
            assert_eq!(step(&held), first, "{mode:?} at {}", i * 500);
        }
    }
}

/// Under GPU activity of unknown amount, a cap at the fair share that its supervisor reports
/// applied is credited at its duty, so it holds and never escalates to a pause (FR-022,
/// GPU-4).
#[test]
fn an_applied_gpu_cap_under_unknown_activity_is_credited_at_its_duty() {
    use ComputeActivity::*;
    use cedegrid::contention::{ContentionMode::*, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    for mode in [Neutral, Performance] {
        let (cfg, mut snap) = us5_device(mode, Unknown, Some(vec![987]), Some(8000), false);
        let mut engine = PolicyEngine::new();
        let mut facts = rootless(&["job"]);
        let mut steps = Vec::new();
        for i in 0..60u64 {
            snap.gpus[0].compute_sample_id = Some(i + 1);
            let decision = engine
                .evaluate(
                    &cfg,
                    &snap,
                    &PolicyInput {
                        allocations: vec![allocation(
                            "job",
                            AllocationPhase::Running,
                            resources(100, 100, 2000),
                            Some(resources(100, 100, 2000)),
                        )],
                        facts: facts.clone(),
                        ..PolicyInput::default()
                    },
                    i * 500,
                )
                .unwrap();
            let action = decision.actions.iter().find(|a| a.resource == key);
            steps.push(action.map(|a| (a.step, a.target)));
            // The supervisor applies each step at once and reports it.
            let job = facts.get_mut("job").unwrap();
            match action {
                Some(action) => {
                    job.applied.insert(key.clone(), action.step);
                    if let Some(target) = action.target {
                        job.applied_targets.insert(key.clone(), target);
                    }
                }
                None => {
                    job.applied.remove(&key);
                    job.applied_targets.remove(&key);
                }
            }
        }
        // Steps hold through the unknown window, then the cap applies and stays.
        let first = steps.iter().position(Option::is_some).expect("a step");
        let capped = &steps[first..];
        assert_eq!(capped[0], Some((YieldStep::Cap, Some(500))), "{mode:?}");
        assert!(
            capped.iter().all(|step| *step == capped[0]),
            "{mode:?}: {:?}",
            capped
                .iter()
                .filter(|step| **step != capped[0])
                .take(3)
                .collect::<Vec<_>>()
        );
    }
}

/// A gap in the device's memory accounting, here its used memory missing, holds GPU memory
/// steps through the unknown window and drains nothing, whether the gap lasts one interval
/// or the whole window; the inventory missing for a while is held the same way (FR-008,
/// GPU-5).
#[test]
fn a_gpu_memory_gap_holds_through_the_unknown_window() {
    use cedegrid::contention::ResourceKey;
    let cfg = config();
    let window = u64::from(
        cfg.contention
            .thresholds(cedegrid::contention::ResourceKind::GpuMemory)
            .unknown_window_intervals,
    );
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let known = gpu_snapshot();
    let mut no_memory = known.clone();
    no_memory.gpus[0].used_memory_mib = None;
    let mut no_inventory = known.clone();
    no_inventory.gpu_inventory = CapabilityStatus::Unknown;
    no_inventory.gpus.clear();
    for gap in [no_memory, no_inventory] {
        for length in [1, window - 1] {
            let mut engine = PolicyEngine::new();
            let mut now = 0;
            let mut steps = Vec::new();
            for snap in std::iter::once(&known)
                .chain(std::iter::repeat_n(&gap, length as usize))
                .chain(std::iter::repeat_n(&known, 4))
            {
                let decision = engine.evaluate(&cfg, snap, &input, now).unwrap();
                steps.extend(
                    decision
                        .actions
                        .into_iter()
                        .filter(|a| matches!(a.resource, ResourceKey::GpuMemory(_)))
                        .map(|a| (now, a.step)),
                );
                assert!(decision.would_drain.is_empty(), "at {now}");
                now += cfg.monitor.interval_ms;
            }
            assert!(steps.is_empty(), "{length} intervals: {steps:?}");
        }
    }
}

/// Takes the measured GPU compute path after two measured intervals, for tests of that
/// path; the quiet window's hysteresis has its own test.
fn measured_at_once(cfg: &mut Config) {
    // Two intervals span longer than the GPU sample age, which stays above one interval (TEL-3).
    cfg.gpu.process_sample_max_age_ms = Some(600);
    cfg.contention.gpu_compute.quiet_window_intervals = Some(2);
    cfg.contention
        .acknowledge_caution
        .insert("relaxed_detection".into());
}

/// One evaluation of a lone running GPU job beside external GPU compute: a
/// measured `external` amount (own work using the rest of the device at full
/// utilization), or an unknown amount. Returns the job's GPU compute cap target and
/// the path held_back reports.
fn gpu_compute_target(
    mode: cedegrid::contention::ContentionMode,
    external: Option<u64>,
) -> (
    Option<cedegrid::contention::YieldStep>,
    Option<u64>,
    Option<ComputePath>,
) {
    let key = cedegrid::contention::ResourceKey::GpuCompute("GPU-test".into());
    let (mut cfg, mut snap) = us5_device(
        mode,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    measured_at_once(&mut cfg);
    snap.gpus[0].utilization_percent = Some(100);
    snap.gpus[0].external_compute_milli = external;
    snap.gpus[0].own_compute_milli = external.map(|external| 1000 - external);
    if external.is_none() {
        snap.gpus[0].compute_amount_unknown =
            Some("summed per-process utilization differs from the device".into());
    }
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    // The measured path applies from the quiet window's second interval: a first measured one
    // with no external context and own work at half the device, within every mode's target.
    let mut warm = snap.clone();
    warm.gpus[0].external_compute = ComputeActivity::Idle;
    warm.gpus[0].external_process_ids = Some(vec![]);
    warm.gpus[0].external_compute_milli = Some(0);
    warm.gpus[0].own_compute_milli = Some(500);
    warm.gpus[0].compute_amount_unknown = None;
    let mut engine = PolicyEngine::new();
    engine.evaluate(&cfg, &warm, &input, 0).unwrap();
    snap.gpus[0].compute_sample_id = Some(2);
    let decision = engine.evaluate(&cfg, &snap, &input, 500).unwrap();
    let action = decision.actions.iter().find(|a| a.resource == key);
    (
        action.map(|a| a.step),
        action.and_then(|a| a.target),
        decision.resources[&key]
            .held_back
            .as_ref()
            .and_then(|held| held.compute_path.clone()),
    )
}

/// A measured external GPU compute amount takes the CPU paths on a saturated
/// device (FR-021, FR-022): Performance keeps capacity minus the external amount
/// minus its 2% margin, never below the fair share; Neutral gives the fair share
/// at once. Yield First probes a demand below its 15% margin step, stepping to
/// capacity minus the user minus the margin, and pauses for a larger one while
/// own work below capacity minus the margin does not cause the saturation. An
/// unknown amount keeps the one-principal share, and held_back names the path.
#[test]
fn measured_external_gpu_compute_takes_the_cpu_paths() {
    use cedegrid::contention::{
        ContentionMode::*,
        YieldStep::{Cap, Pause},
    };
    for (external, performance, yield_first) in [
        (50, 930, (Some(Cap), Some(800))),
        (400, 580, (Some(Pause), None)),
    ] {
        assert_eq!(
            gpu_compute_target(Performance, Some(external)),
            (Some(Cap), Some(performance), Some(ComputePath::Measured)),
            "{external}"
        );
        assert_eq!(
            gpu_compute_target(YieldFirst, Some(external)),
            (yield_first.0, yield_first.1, Some(ComputePath::Measured)),
            "{external}"
        );
        assert_eq!(
            gpu_compute_target(Neutral, Some(external)),
            (Some(Cap), Some(500), Some(ComputePath::Measured)),
            "{external}"
        );
    }
    let (step, target, path) = gpu_compute_target(Neutral, None);
    assert_eq!((step, target), (Some(Cap), Some(500)));
    assert!(
        matches!(&path, Some(ComputePath::Unknown { code, reason }) if code == "gpu_compute_amount_unknown" && reason.contains("differs")),
        "{path:?}"
    );
    assert_eq!(gpu_compute_target(YieldFirst, None).0, Some(Pause));
}

/// Under GPU activity of unknown amount, Neutral and Performance hold the fair
/// share: once the supervisor reports the duty cycle applied, own work holds at
/// most that duty, so the cap at the share is met and never escalates; for 5
/// minutes after the first step own work stays at 500 and is never paused.
#[test]
fn an_applied_duty_holds_the_fair_share_under_unknown_amounts() {
    use cedegrid::contention::{ContentionMode::*, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    for mode in [Neutral, Performance] {
        let (cfg, mut snap) = us5_device(
            mode,
            ComputeActivity::Active,
            Some(vec![987]),
            Some(8000),
            false,
        );
        snap.gpus[0].compute_amount_unknown = Some("not measured".into());
        let mut engine = PolicyEngine::new();
        let mut applied: Option<YieldStep> = None;
        let mut targets = vec![];
        let mut now = 0;
        while now < 300_000 {
            snap.gpus[0].compute_sample_id = Some(now + 1);
            let mut facts = rootless(&["job"]);
            if let Some(step) = applied {
                facts
                    .get_mut("job")
                    .unwrap()
                    .applied
                    .insert(key.clone(), step);
            }
            let decision = engine
                .evaluate(
                    &cfg,
                    &snap,
                    &PolicyInput {
                        allocations: vec![allocation(
                            "job",
                            AllocationPhase::Running,
                            resources(100, 100, 2000),
                            Some(resources(100, 100, 2000)),
                        )],
                        facts,
                        ..PolicyInput::default()
                    },
                    now,
                )
                .unwrap();
            let action = decision.actions.iter().find(|a| a.resource == key);
            // The supervisor applies each step it is sent.
            applied = action.map(|a| a.step);
            targets.push(action.map(|a| (a.step, a.target)));
            now += 500;
        }
        assert_eq!(targets[0], Some((YieldStep::Cap, Some(500))), "{mode:?}");
        assert!(
            targets
                .iter()
                .all(|t| *t == Some((YieldStep::Cap, Some(500)))),
            "{mode:?}: {:?}",
            targets
                .iter()
                .find(|t| **t != Some((YieldStep::Cap, Some(500))))
        );
    }
}

/// The policy credits a GPU compute cap its supervisor reports applied at the cap's duty
/// from the reported facts alone. Full and Tracked report the same facts for that cap
/// (`tests/cgroup.rs`), so own work is credited alike in both (plan "Item 028").
#[test]
fn a_reported_gpu_compute_cap_is_credited_at_its_duty() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    snap.gpus[0].compute_amount_unknown = Some("not measured".into());
    let input = |applied: bool| {
        let mut facts = rootless(&["job"]);
        let job = facts.get_mut("job").unwrap();
        // The GPU compute steps either actuator reports unavailable.
        job.unavailable.insert(
            key.clone(),
            std::collections::BTreeSet::from([YieldStep::LowerShare, YieldStep::Hint]),
        );
        if applied {
            job.applied.insert(key.clone(), YieldStep::Cap);
        }
        PolicyInput {
            allocations: vec![allocation(
                "job",
                AllocationPhase::Running,
                resources(100, 100, 2000),
                Some(resources(100, 100, 2000)),
            )],
            facts,
            ..PolicyInput::default()
        }
    };
    let mut engine = PolicyEngine::new();
    snap.gpus[0].compute_sample_id = Some(1);
    let first = engine.evaluate(&cfg, &snap, &input(false), 0).unwrap();
    let action = first.actions.iter().find(|a| a.resource == key).unwrap();
    assert_eq!((action.step, action.target), (YieldStep::Cap, Some(500)));
    snap.gpus[0].compute_sample_id = Some(2);
    let second = engine.evaluate(&cfg, &snap, &input(true), 500).unwrap();
    let status = &second.resources[&key];
    assert_eq!(status.own, Some((500, 500)));
    assert!(
        status
            .reasons
            .iter()
            .any(|reason| reason.code == "applied_duty_credit"),
        "{:?}",
        status.reasons
    );
}

/// A task that launched at the Yield First nice and now runs in another CPU mode keeps
/// that nice, which an unprivileged process cannot lower; the decision says so (FR-011).
#[test]
fn a_kept_launch_nice_is_reported_outside_yield_first() {
    use cedegrid::contention::{ContentionMode::*, ResourceKey};
    for (mode, reported) in [(Performance, true), (Neutral, true), (YieldFirst, false)] {
        let mut cfg = config();
        cfg.contention.default_mode = mode;
        cfg.contention
            .acknowledge_caution
            .insert("performance_mode".into());
        let mut facts = rootless(&["job"]);
        facts.get_mut("job").unwrap().launch_nice = Some(10);
        let decision = PolicyEngine::new()
            .evaluate(
                &cfg,
                &snapshot(),
                &PolicyInput {
                    allocations: vec![allocation(
                        "job",
                        AllocationPhase::Running,
                        resources(1000, 100, 0),
                        Some(resources(500, 100, 0)),
                    )],
                    facts,
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap();
        let kept = decision.resources[&ResourceKey::Cpu]
            .reasons
            .iter()
            .any(|reason| reason.code == "launch_nice_kept");
        assert_eq!(kept, reported, "{mode:?}");
    }
}

/// External GPU compute is grouped by UID, as CPU principals are: two external
/// users each wanting half the device leave Neutral own work a third (FR-021).
#[test]
fn external_gpu_users_are_principals_by_uid() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (mut cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987, 988]),
        Some(8000),
        false,
    );
    measured_at_once(&mut cfg);
    snap.gpus[0].utilization_percent = Some(100);
    snap.gpus[0].external_compute_milli = Some(666);
    snap.gpus[0].own_compute_milli = Some(334);
    let user = |uid: u32, amount: u64| PrincipalUsage {
        uid: Some(uid),
        gpu_compute_milli: BTreeMap::from([("GPU-test".into(), amount)]),
        ..PrincipalUsage::default()
    };
    snap.principals = Some(vec![
        PrincipalUsage {
            uid: Some(1000),
            is_owner: true,
            ..PrincipalUsage::default()
        },
        user(2000, 333),
        user(3000, 333),
    ]);
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let mut engine = PolicyEngine::new();
    let mut share = None;
    let mut now = 0;
    while now <= 30_000 {
        snap.gpus[0].compute_sample_id = Some(now + 1);
        share = engine.evaluate(&cfg, &snap, &input, now).unwrap().resources[&key].target_share;
        now += 500;
    }
    // A third of 1000; max-min hands the remainder to own work.
    assert_eq!(share, Some(334));
}

/// Device time nobody accounts for is an unattributed external principal while an
/// external PID is listed: with own samples at 85 on a saturated device, the 150
/// contends; Neutral steps to the fair share, learns the user is satisfied and
/// rises to capacity minus 150 minus its 5% margin (FR-021).
#[test]
fn unattributed_gpu_time_contends_and_is_learned_satisfied() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (mut cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    measured_at_once(&mut cfg);
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let mut engine = PolicyEngine::new();
    let (mut cap, mut lowest) = (None::<u64>, u64::MAX);
    let mut now = 0;
    while now <= 90_000 {
        // Own work wants 850; the listed external PID shows no samples, so the
        // device time beside own work is the 150 nobody accounts for.
        let own = cap.unwrap_or(850).min(850);
        lowest = lowest.min(cap.unwrap_or(1000));
        snap.gpus[0].compute_sample_id = Some(now + 1);
        snap.gpus[0].own_compute_milli = Some(own);
        snap.gpus[0].external_compute_milli = Some(150);
        snap.gpus[0].utilization_percent = Some(((own + 150) / 10) as u32);
        let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
        let action = decision.actions.iter().find(|a| a.resource == key);
        assert_ne!(action.map(|a| a.step), Some(YieldStep::Pause), "at {now}");
        cap = action.and_then(|a| a.target);
        now += 500;
    }
    assert_eq!(lowest, 500, "the fair share applied first");
    assert_eq!(cap.unwrap_or(1000), 800);
}

/// A device whose samples alternate between consistent and inconsistent every
/// interval changes own work's cap at most once: the unknown path applies at once
/// and the measured path returns only after the quiet window of consecutive
/// measured intervals, which held_back explains (FR-021).
#[test]
fn alternating_gpu_measurements_cause_at_most_one_step_change() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey, YieldStep};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    let mut engine = PolicyEngine::new();
    let mut applied = None::<YieldStep>;
    let mut cap = None::<(YieldStep, Option<u64>)>;
    let mut path = None;
    let mut changes = 0;
    let mut now = 0;
    while now < 120_000 {
        // Consistent samples for 60 s, then alternating every interval.
        let measured = now < 60_000 || (now / 500) % 2 == 0;
        let own = cap.and_then(|c| c.1).unwrap_or(950).min(950);
        snap.gpus[0].compute_sample_id = Some(now + 1);
        snap.gpus[0].utilization_percent = Some(((own + 50) / 10) as u32);
        snap.gpus[0].external_compute_milli = measured.then_some(50);
        snap.gpus[0].own_compute_milli = measured.then_some(own);
        snap.gpus[0].compute_amount_unknown =
            (!measured).then(|| "summed per-process utilization differs".into());
        let mut facts = rootless(&["job"]);
        if let Some(applied) = applied {
            facts
                .get_mut("job")
                .unwrap()
                .applied
                .insert(key.clone(), applied);
        }
        let decision = engine
            .evaluate(
                &cfg,
                &snap,
                &PolicyInput {
                    allocations: vec![allocation(
                        "job",
                        AllocationPhase::Running,
                        resources(100, 100, 2000),
                        Some(resources(100, 100, 2000)),
                    )],
                    facts,
                    ..PolicyInput::default()
                },
                now,
            )
            .unwrap();
        let action = decision.actions.iter().find(|a| a.resource == key);
        let next = action.map(|a| (a.step, a.target));
        if now == 59_500 {
            assert_eq!(
                next,
                Some((YieldStep::Cap, Some(900))),
                "learned before alternating"
            );
        }
        if now >= 60_000 && next != cap {
            changes += 1;
        }
        cap = next;
        applied = action.map(|a| a.step);
        // A measured interval waiting out the window says why it is not trusted.
        if measured {
            path = decision.resources[&key]
                .held_back
                .as_ref()
                .and_then(|held| held.compute_path.clone());
        }
        now += 500;
    }
    assert!(changes <= 1, "{changes} cap changes");
    assert!(
        matches!(&path, Some(ComputePath::Unknown { code, reason }) if code == "gpu_compute_recently_inconsistent" && reason.contains("recently inconsistent")),
        "{path:?}"
    );
}

/// An interval whose inventory lists only own verified PIDs (research R18) is
/// measured at once with no external compute: a new own process neither waits
/// out the quiet window nor is capped for activity of unknown amount, and the
/// measured streak keeps counting (FR-021).
#[test]
fn an_own_only_gpu_interval_is_measured_at_once() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Idle,
        Some(vec![]),
        Some(8000),
        false,
    );
    snap.gpus[0].utilization_percent = Some(100);
    snap.gpus[0].external_compute_milli = Some(0);
    snap.gpus[0].own_compute_milli = Some(1000);
    snap.gpus[0].compute_amount_unknown = None;
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let path = |snap: &Snapshot, engine: &mut PolicyEngine, now| {
        let decision = engine.evaluate(&cfg, snap, &input, now).unwrap();
        let held = decision.resources[&key].held_back.clone();
        (
            held.and_then(|held| held.compute_path),
            decision.actions.iter().any(|a| a.resource == key),
        )
    };
    // Today's path: a fresh device waits out the quiet window.
    let mut engine = PolicyEngine::new();
    snap.gpus[0].compute_sample_id = Some(1);
    assert!(matches!(
        path(&snap, &mut engine, 0).0,
        Some(ComputePath::Unknown { code, .. }) if code == "gpu_compute_recently_inconsistent"
    ));
    // Own-only: measured from the first interval, and no step on own work.
    snap.gpus[0].own_only = true;
    let mut engine = PolicyEngine::new();
    for now in (0..10_000).step_by(500) {
        snap.gpus[0].compute_sample_id = Some(now + 1);
        assert_eq!(
            path(&snap, &mut engine, now),
            (Some(ComputePath::Measured), false),
            "at {now}"
        );
    }
    // A measured interval with an external PID after own-only intervals stays
    // on the measured path: the streak kept counting.
    snap.gpus[0].own_only = false;
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    snap.gpus[0].external_compute_milli = Some(100);
    snap.gpus[0].own_compute_milli = Some(900);
    snap.gpus[0].compute_sample_id = Some(10_001);
    assert_eq!(
        path(&snap, &mut engine, 10_000).0,
        Some(ComputePath::Measured)
    );
}

/// Own work is credited at the larger of the duty its supervisor reports applied
/// and the policy's current target: when the policy lowers a cap within the
/// step (as when a new allocation halves each share) the allocation counts at
/// its old duty until its supervisor reports the new one.
#[test]
fn own_gpu_work_counts_at_the_duty_the_supervisor_applied() {
    use cedegrid::contention::{
        ContentionMode::Neutral, ResourceKey, StepRequest, YieldCause, YieldStep,
    };
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    snap.gpus[0].compute_amount_unknown = Some("not measured".into());
    let own_counted = |applied_target: u64| {
        let mut engine = PolicyEngine::new();
        // The policy's current target for the cap is 250.
        engine.seed(
            "a",
            key.clone(),
            StepRequest {
                step: YieldStep::Cap,
                target: Some(250),
                cause: YieldCause::ExternalGrowth,
                deadline_ms: None,
            },
            0,
        );
        let mut facts = rootless(&["a"]);
        let a = facts.get_mut("a").unwrap();
        a.applied.insert(key.clone(), YieldStep::Cap);
        a.applied_targets.insert(key.clone(), applied_target);
        let decision = engine
            .evaluate(
                &cfg,
                &snap,
                &PolicyInput {
                    allocations: vec![allocation(
                        "a",
                        AllocationPhase::Running,
                        resources(100, 100, 1000),
                        Some(resources(100, 100, 1000)),
                    )],
                    facts,
                    ..PolicyInput::default()
                },
                500,
            )
            .unwrap();
        decision.resources[&key].own.map(|own| own.0)
    };
    // Its supervisor still runs the old 500 duty: own work counts at 500.
    assert_eq!(own_counted(500), Some(500));
    // Once it reports 250 applied, own work counts at 250.
    assert_eq!(own_counted(250), Some(250));
}

/// Neutral learns that a measured 5% external user is satisfied once own work
/// holds the fair share and the device has capacity to spare, and own work then
/// runs at capacity minus the user minus the 5% margin (FR-021).
#[test]
fn neutral_learns_a_measured_gpu_user_is_satisfied() {
    use cedegrid::contention::{ContentionMode::Neutral, ResourceKey};
    let key = ResourceKey::GpuCompute("GPU-test".into());
    let (mut cfg, mut snap) = us5_device(
        Neutral,
        ComputeActivity::Active,
        Some(vec![987]),
        Some(8000),
        false,
    );
    measured_at_once(&mut cfg);
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let mut engine = PolicyEngine::new();
    let mut cap: Option<u64> = None;
    let mut now = 0;
    while now <= 60_000 {
        // Own work wants 950 and runs at its cap; the user takes 50.
        let own = cap.unwrap_or(950).min(950);
        snap.gpus[0].own_compute_milli = Some(own);
        snap.gpus[0].external_compute_milli = Some(50);
        snap.gpus[0].utilization_percent = Some(((own + 50) / 10) as u32);
        let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
        cap = decision
            .actions
            .iter()
            .find(|a| a.resource == key)
            .and_then(|a| a.target);
        now += 500;
    }
    assert_eq!(cap.unwrap_or(1000), 900);
}

/// Running own work is not drained because a device becomes occupied, even with
/// `never_share_occupied_gpus` (FR-064); missing GPU observation holds it as an
/// unknown measurement instead of a full yield (constitution III).
#[test]
fn occupation_and_lost_observation_do_not_drain_running_gpu_work() {
    use cedegrid::contention::ContentionMode::Neutral;
    let input = PolicyInput {
        allocations: vec![allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 100, 2000),
            Some(resources(100, 100, 2000)),
        )],
        facts: rootless(&["job"]),
        ..PolicyInput::default()
    };
    let (cfg, snap) = us5_device(
        Neutral,
        ComputeActivity::Quiet,
        Some(vec![987]),
        Some(8000),
        true,
    );
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snap, &input, 0)
        .unwrap();
    assert!(decision.would_drain.is_empty(), "{:?}", decision.actions);
    let (cfg, mut snap) = us5_device(Neutral, ComputeActivity::Unknown, None, Some(8000), false);
    snap.gpus[0].observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::InsufficientObservability,
    ));
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snap, &input, 0)
        .unwrap();
    assert!(decision.would_drain.is_empty(), "{:?}", decision.actions);
}

/// Without a legacy `gpu.scale_up_cooldown_ms`, GPU memory admission opens after
/// the mode's cooldown, 10 s in Neutral, instead of 30 s (FR-034).
#[test]
fn gpu_memory_admission_waits_for_the_mode_cooldown() {
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = None;
    let snap = gpu_snapshot();
    let mut engine = PolicyEngine::new();
    for (now, open) in [(0, false), (9500, false), (10_000, true)] {
        let headroom = engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), now)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .get("GPU-test")
            .copied()
            .unwrap_or(0);
        assert_eq!(headroom > 0, open, "at {now}");
    }
}

/// The allowance signal (FR-071): what each running allocation may grow to now.
mod allowance {
    use super::*;
    use cedegrid::contention::ContentionMode;

    fn cfg(mode: ContentionMode) -> Config {
        let mut cfg = config();
        cfg.cpu.reserve_physical_cores = 0;
        // The default margins, so a Neutral scale-up step is 5% of capacity.
        cfg.contention.cpu.neutral_margin_percent = None;
        cfg.contention.default_mode = mode;
        cfg.contention
            .acknowledge_caution
            .insert("performance_mode".into());
        cfg
    }

    fn job(id: &str, cpu: u64, ram: u64, class: AllocationClass) -> Allocation {
        let mut job = allocation(
            id,
            AllocationPhase::Running,
            resources(cpu, ram, 0),
            Some(resources(cpu, ram, 0)),
        );
        job.class = class;
        job
    }

    /// The host: 8 CPUs, 32 000 MiB, external CPU and RAM usage beside own work.
    fn host(jobs: &[Allocation], external_cpu: u64, external_ram: u64) -> Snapshot {
        let mut snap = snapshot();
        let own_cpu: u64 = jobs
            .iter()
            .filter_map(|j| j.observed.as_ref())
            .map(|o| o.cpu_millicores)
            .sum();
        let own_ram: u64 = jobs
            .iter()
            .filter_map(|j| j.observed.as_ref())
            .map(|o| o.ram_mib)
            .sum();
        snap.cpu_busy_millicores = Some((own_cpu + external_cpu).min(8000));
        snap.available_ram_mib = Some(32_000 - own_ram - external_ram);
        snap
    }

    fn facts(jobs: &[Allocation]) -> BTreeMap<String, AllocationFacts> {
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        rootless(&ids)
    }

    fn evaluate(mode: ContentionMode, jobs: Vec<Allocation>, external: (u64, u64)) -> Decision {
        let snap = host(&jobs, external.0, external.1);
        PolicyEngine::new()
            .evaluate(
                &cfg(mode),
                &snap,
                &PolicyInput {
                    facts: facts(&jobs),
                    allocations: jobs,
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap()
    }

    proptest! {
        /// If every running allocation grows to its allowance at once, the totals
        /// stay within the mode's allowance (and so above every floor), and for the
        /// same state Yield First <= Neutral <= Performance per allocation.
        #[test]
        fn growing_to_every_allowance_at_once_stays_within_the_mode(
            usages in proptest::collection::vec((0u64..250, 0u64..800, any::<bool>()), 1..=32),
            pending in 0u64..1500,
            external_cpu in 0u64..2500,
            external_ram in 0u64..6000,
        ) {
            let mut jobs: Vec<Allocation> = usages
                .iter()
                .enumerate()
                .map(|(i, (cpu, ram, guaranteed))| {
                    let class = if *guaranteed { AllocationClass::Guaranteed } else { AllocationClass::Opportunistic };
                    job(&format!("job-{i}"), *cpu, *ram, class)
                })
                .collect();
            jobs.push(allocation("pending", AllocationPhase::Pending, resources(pending, pending, 0), None));
            let mut previous: Option<BTreeMap<String, ResourceAmounts>> = None;
            for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral, ContentionMode::Performance] {
                let decision = evaluate(mode, jobs.clone(), (external_cpu, external_ram));
                let allowances = decision.allowances.clone();
                // Steps the policy took already cap their allowances; check only
                // states it leaves alone.
                if !decision.actions.is_empty() {
                    previous = None;
                    continue;
                }
                let sum = |pick: &dyn Fn(&ResourceAmounts) -> u64| -> u64 {
                    allowances.values().map(pick).sum::<u64>()
                };
                // Growth is invited only within the mode's allowance; what running
                // work already holds (Guaranteed at its request) may exceed it.
                let held = |pick: &dyn Fn(&Resources) -> u64| -> u64 {
                    jobs.iter()
                        .filter(|j| j.phase == AllocationPhase::Running)
                        .map(|j| pick(j.observed.as_ref().unwrap()).max(
                            if j.class == AllocationClass::Guaranteed { pick(&j.requested) } else { 0 },
                        ))
                        .sum::<u64>()
                };
                prop_assert!(
                    sum(&|a| a.cpu_millicores.unwrap_or(0)) + pending
                        <= decision.managed_budget.cpu_millicores.max(held(&|r| r.cpu_millicores) + pending),
                    "{mode:?} cpu {allowances:?} budget {:?}", decision.managed_budget
                );
                prop_assert!(
                    sum(&|a| a.ram_mib.unwrap_or(0)) + pending
                        <= decision.managed_budget.ram_mib.max(held(&|r| r.ram_mib) + pending),
                    "{mode:?} ram {allowances:?} budget {:?}", decision.managed_budget
                );
                if let Some(previous) = &previous {
                    for (id, amounts) in &allowances {
                        let before = &previous[id];
                        prop_assert!(before.cpu_millicores <= amounts.cpu_millicores, "{mode:?} {id}");
                        prop_assert!(before.ram_mib <= amounts.ram_mib, "{mode:?} {id}");
                    }
                }
                previous = Some(allowances);
            }
        }
    }

    /// Each running allocation gets its usage plus an equal part of the room under
    /// the mode's allowance after pending reservations; a Guaranteed allocation
    /// never less than its request, and no allocation more than the envelope.
    #[test]
    fn the_allowance_is_usage_plus_an_equal_part_of_the_room() {
        let decision = evaluate(
            ContentionMode::Neutral,
            vec![
                job("a", 1000, 2000, AllocationClass::Opportunistic),
                job("b", 500, 1000, AllocationClass::Opportunistic),
            ],
            (0, 0),
        );
        let budget = &decision.managed_budget;
        let a = &decision.allowances["a"];
        let b = &decision.allowances["b"];
        // The room, less one publish threshold in total, is shared equally.
        let cpu_room = budget.cpu_millicores - 1500 - 80;
        assert_eq!(a.cpu_millicores, Some(1000 + cpu_room / 2));
        assert_eq!(b.cpu_millicores, Some(500 + cpu_room / 2));
        let ram_room = budget.ram_mib - 3000 - 256;
        assert_eq!(a.ram_mib, Some(2000 + ram_room / 2));
        // A Guaranteed allocation using less than its request keeps the request.
        let mut guaranteed = job("c", 100, 100, AllocationClass::Guaranteed);
        guaranteed.requested = resources(4000, 20_000, 0);
        let decision = evaluate(ContentionMode::YieldFirst, vec![guaranteed], (6000, 0));
        let c = &decision.allowances["c"];
        assert!(c.cpu_millicores.unwrap() >= 4000, "{c:?}");
        assert!(c.ram_mib.unwrap() >= 20_000, "{c:?}");
        // The operator envelope bounds every allowance.
        let snap = host(
            &[job("a", 1000, 1000, AllocationClass::Opportunistic)],
            0,
            0,
        );
        let decision = PolicyEngine::new()
            .evaluate(
                &cfg(ContentionMode::Performance),
                &snap,
                &PolicyInput {
                    allocations: vec![job("a", 1000, 1000, AllocationClass::Opportunistic)],
                    facts: rootless(&["a"]),
                    envelope: Some(resources(3000, 9000, 0)),
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap();
        assert!(decision.allowances["a"].cpu_millicores.unwrap() <= 3000);
        assert!(decision.allowances["a"].ram_mib.unwrap() <= 9000);
    }

    /// Publishing has no churn: a fall publishes at once, a rise at most once per
    /// scale-up interval by at most one scale-up step (the mode's margin of
    /// capacity) while contention only falls, and only changes beyond the contention
    /// floor (CPU) or the noise tolerance (memory) publish. Once contention has fully
    /// ended, CPU rises in one move (FR-034).
    #[test]
    fn allowances_fall_at_once_and_rise_one_step_per_interval() {
        let cfg = cfg(ContentionMode::Neutral);
        let mut engine = PolicyEngine::new();
        let mut run = |external: u64, usage: u64, now: u64| -> u64 {
            let jobs = vec![job("a", usage, 1000, AllocationClass::Opportunistic)];
            let snap = host(&jobs, external, 0);
            engine
                .evaluate(
                    &cfg,
                    &snap,
                    &PolicyInput {
                        allocations: jobs,
                        facts: rootless(&["a"]),
                        ..PolicyInput::default()
                    },
                    now,
                )
                .unwrap()
                .allowances["a"]
                .cpu_millicores
                .unwrap()
        };
        let first = run(0, 1000, 0);
        // External CPU appears: the allowance falls at once.
        let fallen = run(3000, 1000, 500);
        assert!(fallen + 2000 < first, "{first} -> {fallen}");
        // A small external change within the 80m floor does not publish.
        assert_eq!(run(3050, 1000, 1000), fallen);
        // External CPU falls but stays: the allowance rises one 400m step (5% of 8000m)
        // per 10 s scale-up interval.
        let mut now = 1500;
        let mut published = vec![];
        while now <= 31_500 {
            published.push((now, run(1000, 1000, now)));
            now += 500;
        }
        let rises: Vec<(u64, u64)> = published
            .windows(2)
            .filter(|w| w[1].1 != w[0].1)
            .map(|w| (w[1].0, w[1].1 - w[0].1))
            .collect();
        assert!(!rises.is_empty(), "{published:?}");
        assert!(rises.iter().all(|(_, step)| *step <= 400), "{rises:?}");
        assert!(
            rises.windows(2).all(|w| w[1].0 - w[0].0 >= 10_000),
            "{rises:?}"
        );
        // External CPU leaves: after the 2 s principal window, one move to the target.
        let before = published.last().unwrap().1;
        let returned: Vec<u64> = (1..=5).map(|n| run(0, 1000, now + n * 500)).collect();
        assert_eq!(returned[..3], [before; 3], "{returned:?}");
        assert!(returned[4] >= before + 1000, "{before} -> {returned:?}");
    }

    /// The reserve is one publish threshold in total: small falls on eight
    /// allocations that drift less than one threshold in sum publish nothing, and
    /// the fall that passes it publishes every stale allocation at once.
    #[test]
    fn small_falls_publish_once_their_sum_passes_one_threshold() {
        let cfg = cfg(ContentionMode::Neutral);
        let mut engine = PolicyEngine::new();
        let jobs: Vec<Allocation> = (0..8)
            .map(|i| {
                job(
                    &format!("job-{i}"),
                    200,
                    1000,
                    AllocationClass::Opportunistic,
                )
            })
            .collect();
        let mut run = |external: u64, now: u64| -> Vec<u64> {
            let snap = host(&jobs, external, 0);
            let decision = engine
                .evaluate(
                    &cfg,
                    &snap,
                    &PolicyInput {
                        facts: facts(&jobs),
                        allocations: jobs.clone(),
                        ..PolicyInput::default()
                    },
                    now,
                )
                .unwrap();
            jobs.iter()
                .map(|j| decision.allowances[&j.id].cpu_millicores.unwrap())
                .collect()
        };
        let first = run(0, 0);
        // External CPU grows 32m at a time: each allowance falls 4m.
        assert_eq!(run(32, 500), first, "a 32m drift publishes nothing");
        assert_eq!(run(64, 1000), first, "a 64m drift publishes nothing");
        let fallen = run(96, 1500);
        assert!(
            fallen
                .iter()
                .zip(&first)
                .all(|(now, before)| now + 12 == *before),
            "a 96m drift publishes every stale allocation: {first:?} -> {fallen:?}"
        );
    }

    /// While a resource's measurement is unknown, the allowance is current usage.
    #[test]
    fn an_unknown_measurement_invites_no_growth() {
        let jobs = vec![job("a", 1000, 3000, AllocationClass::Opportunistic)];
        let mut snap = host(&jobs, 0, 0);
        snap.cpu_busy_millicores = None;
        let decision = PolicyEngine::new()
            .evaluate(
                &cfg(ContentionMode::Neutral),
                &snap,
                &PolicyInput {
                    allocations: jobs,
                    facts: rootless(&["a"]),
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap();
        assert_eq!(decision.allowances["a"].cpu_millicores, Some(1000));
        assert!(decision.allowances["a"].ram_mib.unwrap() > 3000);
    }

    /// A worker that grows to its published allowance never triggers a step, on an
    /// idle host and beside a steady 12.5% external CPU user, in every mode.
    #[test]
    fn growing_to_the_published_allowance_triggers_no_step() {
        for mode in [
            ContentionMode::YieldFirst,
            ContentionMode::Neutral,
            ContentionMode::Performance,
        ] {
            for external in [0, 1000] {
                let cfg = cfg(mode);
                let mut engine = PolicyEngine::new();
                let (mut cpu, mut ram) = (500u64, 1000u64);
                let mut now = 0;
                while now < 300_000 {
                    let jobs = vec![job("a", cpu, ram, AllocationClass::Opportunistic)];
                    let snap = host(&jobs, external, 0);
                    let decision = engine
                        .evaluate(
                            &cfg,
                            &snap,
                            &PolicyInput {
                                allocations: jobs,
                                facts: rootless(&["a"]),
                                ..PolicyInput::default()
                            },
                            now,
                        )
                        .unwrap();
                    assert!(
                        decision.actions.is_empty(),
                        "{mode:?} external {external} at {now}: cpu {cpu} ram {ram} {:?}",
                        decision.actions
                    );
                    let allowance = &decision.allowances["a"];
                    cpu = allowance.cpu_millicores.unwrap();
                    ram = allowance.ram_mib.unwrap();
                    now += 500;
                }
                assert!(
                    cpu > 5000,
                    "{mode:?} external {external}: grew only to {cpu}"
                );
            }
        }
    }

    /// Node Neutral, capped at Performance, with per-job modes on CPU and RAM.
    fn mixed(jobs: Vec<Allocation>, external: (u64, u64), node: ContentionMode) -> Decision {
        let mut cfg = cfg(node);
        cfg.contention.max_mode = Some(ContentionMode::Performance);
        let snap = host(&jobs, external.0, external.1);
        PolicyEngine::new()
            .evaluate(
                &cfg,
                &snap,
                &PolicyInput {
                    facts: facts(&jobs),
                    allocations: jobs,
                    ..PolicyInput::default()
                },
                0,
            )
            .unwrap()
    }

    fn moded(mut job: Allocation, mode: ContentionMode) -> Allocation {
        job.modes = BTreeMap::from([
            (cedegrid::contention::ResourceKind::Cpu, mode),
            (cedegrid::contention::ResourceKind::Ram, mode),
        ]);
        job
    }

    proptest! {
        /// Under per-job modes, growing to every allowance at once still stays
        /// within the node mode's allowance, and no job is invited above its own
        /// mode's target (T093a).
        #[test]
        fn growing_to_every_allowance_at_once_stays_within_the_mode_with_mixed_modes(
            usages in proptest::collection::vec((0u64..250, 0u64..800, any::<bool>(), 0usize..3), 1..=32),
            external_cpu in 0u64..2500,
            external_ram in 0u64..6000,
        ) {
            let jobs: Vec<Allocation> = usages
                .iter()
                .enumerate()
                .map(|(i, (cpu, ram, guaranteed, mode))| {
                    let class = if *guaranteed { AllocationClass::Guaranteed } else { AllocationClass::Opportunistic };
                    moded(job(&format!("job-{i}"), *cpu, *ram, class), ContentionMode::ALL[*mode])
                })
                .collect();
            let plain: Vec<Allocation> = jobs.iter().cloned().map(|mut j| { j.modes.clear(); j }).collect();
            let mut previous: Option<BTreeMap<String, ResourceAmounts>> = None;
            for node in ContentionMode::ALL {
                let decision = mixed(jobs.clone(), (external_cpu, external_ram), node);
                if !decision.actions.is_empty() {
                    previous = None;
                    continue;
                }
                // The aggregate is the node mode's.
                let alone = mixed(plain.clone(), (external_cpu, external_ram), node);
                prop_assert_eq!(&decision.managed_budget, &alone.managed_budget);
                let allowances = decision.allowances.clone();
                let held = |j: &Allocation, pick: &dyn Fn(&Resources) -> u64| -> u64 {
                    pick(j.observed.as_ref().unwrap()).max(
                        if j.class == AllocationClass::Guaranteed { pick(&j.requested) } else { 0 },
                    )
                };
                let total = |pick: &dyn Fn(&Resources) -> u64| -> u64 {
                    jobs.iter().map(|j| held(j, pick)).sum()
                };
                let sum = |pick: &dyn Fn(&ResourceAmounts) -> u64| -> u64 {
                    allowances.values().map(pick).sum::<u64>()
                };
                prop_assert!(
                    sum(&|a| a.cpu_millicores.unwrap_or(0))
                        <= decision.managed_budget.cpu_millicores.max(total(&|r| r.cpu_millicores)),
                    "{node:?} cpu {allowances:?} budget {:?}", decision.managed_budget
                );
                prop_assert!(
                    sum(&|a| a.ram_mib.unwrap_or(0))
                        <= decision.managed_budget.ram_mib.max(total(&|r| r.ram_mib)),
                    "{node:?} ram {allowances:?} budget {:?}", decision.managed_budget
                );
                // Each job's own mode's target, as a node in that mode computes it.
                for j in &jobs {
                    let mode = j.modes[&cedegrid::contention::ResourceKind::Cpu].min(ContentionMode::Performance);
                    let own = mixed(plain.clone(), (external_cpu, external_ram), mode).managed_budget;
                    let amounts = &allowances[&j.id];
                    prop_assert!(
                        amounts.cpu_millicores.unwrap() <= own.cpu_millicores.max(held(j, &|r| r.cpu_millicores)),
                        "{node:?} {} cpu {:?} own {}", j.id, amounts, own.cpu_millicores
                    );
                    prop_assert!(
                        amounts.ram_mib.unwrap() <= own.ram_mib.max(held(j, &|r| r.ram_mib)),
                        "{node:?} {} ram {:?} own {}", j.id, amounts, own.ram_mib
                    );
                }
                if let Some(previous) = &previous {
                    for (id, amounts) in &allowances {
                        let before = &previous[id];
                        prop_assert!(before.cpu_millicores <= amounts.cpu_millicores, "{node:?} {id}");
                        prop_assert!(before.ram_mib <= amounts.ram_mib, "{node:?} {id}");
                    }
                }
                previous = Some(allowances);
            }
        }
    }

    /// When the aggregate shrinks, the Yield First job's allowance falls first: it
    /// is bounded by its own mode's target, and the room that bound leaves goes to
    /// the Performance job (T093a).
    #[test]
    fn yield_first_allowances_fall_first_when_the_aggregate_shrinks() {
        let jobs = || {
            vec![
                moded(
                    job("gentle", 5500, 1000, AllocationClass::Opportunistic),
                    ContentionMode::YieldFirst,
                ),
                moded(
                    job("eager", 300, 1000, AllocationClass::Opportunistic),
                    ContentionMode::Performance,
                ),
            ]
        };
        let cpu = |external: u64| {
            let decision = mixed(jobs(), (external, 0), ContentionMode::Neutral);
            assert!(decision.actions.is_empty(), "{:?}", decision.actions);
            let a = &decision.allowances;
            (
                a["gentle"].cpu_millicores.unwrap(),
                a["eager"].cpu_millicores.unwrap(),
            )
        };
        // 8000m, Neutral's 5% margin, one 80m threshold: 720m of room. Yield First's
        // target, 8000 less 15% less external, leaves the gentle job 300m, then 100m.
        assert_eq!(cpu(1000), (5800, 720));
        assert_eq!(cpu(1200), (5600, 720));
    }
}

/// The target model with per-job modes (T093a, plan.md): the owner's aggregate on a
/// resource comes from the node mode and cap; per-job modes order who yields inside
/// it, Yield First first, and each job is also held to its own mode's target.
mod per_job_modes {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKind, YieldStep};

    const CPU: u64 = 8000;

    /// Node default Neutral with the default margins, capped at Performance.
    pub(super) fn cfg() -> Config {
        let mut cfg = config();
        cfg.cpu.reserve_physical_cores = 0;
        cfg.contention.cpu.neutral_margin_percent = None;
        cfg.contention.default_mode = ContentionMode::Neutral;
        cfg.contention.max_mode = Some(ContentionMode::Performance);
        cfg.contention
            .acknowledge_caution
            .insert("performance_mode".into());
        cfg
    }

    /// Max-min fair shares of the CPU among demands.
    pub(super) fn schedule(demands: &[u64]) -> Vec<u64> {
        let mut given = vec![0u64; demands.len()];
        let mut left = CPU;
        let mut open: Vec<usize> = (0..demands.len()).filter(|i| demands[*i] > 0).collect();
        while left > 0 && !open.is_empty() {
            let each = (left / open.len() as u64).max(1);
            let mut next = vec![];
            for i in open {
                let take = (demands[i] - given[i]).min(each).min(left);
                given[i] += take;
                left -= take;
                if given[i] < demands[i] {
                    next.push(i);
                }
            }
            open = next;
        }
        given
    }

    pub(super) struct Job {
        pub id: &'static str,
        pub mode: Option<ContentionMode>,
        pub class: AllocationClass,
        pub request: u64,
        /// What the job would use without a cap.
        pub wants: u64,
    }

    pub(super) struct Interval {
        pub usage: Vec<u64>,
        pub aggregate: u64,
        pub shadow_aggregate: u64,
        pub actions: Vec<YieldAction>,
    }

    /// Runs the policy with the supervisor applying every cap and pause at once,
    /// beside a shadow engine that sees the same hosts without per-job modes.
    pub(super) fn simulate(jobs: &[Job], external: u64, intervals: u64) -> Vec<Interval> {
        let cfg = cfg();
        let (mut engine, mut shadow) = (PolicyEngine::new(), PolicyEngine::new());
        let mut actions: Vec<YieldAction> = vec![];
        let mut out = vec![];
        for i in 0..intervals {
            let limit = |id: &str| {
                actions
                    .iter()
                    .find(|a| a.allocation_id == id)
                    .map_or(CPU, |a| match a.step {
                        YieldStep::Cap => a.target.unwrap_or(CPU),
                        YieldStep::Pause => 0,
                        _ => CPU,
                    })
            };
            let mut demands = vec![external];
            demands.extend(jobs.iter().map(|j| j.wants.min(limit(j.id))));
            let given = schedule(&demands);
            let allocations: Vec<Allocation> = jobs
                .iter()
                .zip(&given[1..])
                .map(|(job, used)| {
                    let mut a = allocation(
                        job.id,
                        AllocationPhase::Running,
                        resources(job.request, 100, 0),
                        Some(resources(*used, 100, 0)),
                    );
                    a.class = job.class;
                    if let Some(mode) = job.mode {
                        a.modes = BTreeMap::from([(ResourceKind::Cpu, mode)]);
                    }
                    a
                })
                .collect();
            let mut snap = snapshot();
            snap.cpu_busy_millicores = Some(given.iter().sum::<u64>().min(CPU));
            let mut principals = vec![super::cpu_principal(1000, true, 0)];
            if external > 0 {
                principals.push(super::cpu_principal(2001, false, given[0]));
            }
            snap.principals = Some(principals);
            let ids: Vec<&str> = jobs.iter().map(|j| j.id).collect();
            let input = PolicyInput {
                facts: rootless(&ids),
                allocations: allocations.clone(),
                ..PolicyInput::default()
            };
            let now = i * 500;
            let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
            let plain = PolicyInput {
                allocations: allocations
                    .into_iter()
                    .map(|mut a| {
                        a.modes.clear();
                        a
                    })
                    .collect(),
                ..input
            };
            let shadow_decision = shadow.evaluate(&cfg, &snap, &plain, now).unwrap();
            actions = decision.actions.clone();
            out.push(Interval {
                usage: given[1..].to_vec(),
                aggregate: decision.managed_budget.cpu_millicores,
                shadow_aggregate: shadow_decision.managed_budget.cpu_millicores,
                actions: decision.actions,
            });
        }
        out
    }

    pub(super) fn average(run: &[Interval], job: usize) -> u64 {
        run.iter().map(|i| i.usage[job]).sum::<u64>() / run.len() as u64
    }

    fn pair(gentle_class: AllocationClass, eager_wants: u64) -> [Job; 2] {
        [
            Job {
                id: "gentle",
                mode: Some(ContentionMode::YieldFirst),
                class: gentle_class,
                request: 1000,
                wants: CPU,
            },
            Job {
                id: "eager",
                mode: Some(ContentionMode::Performance),
                class: AllocationClass::Opportunistic,
                request: 1000,
                wants: eager_wants,
            },
        ]
    }

    /// Two jobs in different modes on one resource: the Yield First job yields
    /// first and is held to its own mode's target, the Performance job keeps the
    /// rest, and the aggregate is the node mode's.
    #[test]
    fn the_yield_first_job_yields_first_and_the_aggregate_is_the_node_modes() {
        for external in [0, 1000] {
            let run = simulate(&pair(AllocationClass::Opportunistic, CPU), external, 240);
            for (i, interval) in run.iter().enumerate() {
                assert_eq!(
                    interval.aggregate, interval.shadow_aggregate,
                    "external {external}, interval {i}"
                );
            }
            let first = run.iter().find(|i| !i.actions.is_empty()).unwrap();
            assert!(
                first.actions.iter().all(|a| a.allocation_id == "gentle"),
                "external {external}: {:?}",
                first.actions
            );
            // Settled over the last minute: the Performance job holds the
            // aggregate less what the Yield First job keeps.
            let settled = &run[120..];
            let (gentle, eager) = (average(settled, 0), average(settled, 1));
            let aggregate = settled.last().unwrap().aggregate;
            assert!(
                gentle + eager <= aggregate + CPU / 100,
                "external {external}: {gentle} + {eager} against {aggregate}"
            );
            assert!(
                eager + gentle + CPU / 10 >= aggregate,
                "external {external}: {gentle} + {eager} against {aggregate}"
            );
            assert!(eager > gentle, "external {external}: {gentle} {eager}");
            if external > 0 {
                // Contended, Yield First's target is zero: a pause, then at most
                // the minimum duty once the pause bound passes (FR-014).
                assert!(
                    settled.iter().all(|i| i.usage[0] <= CPU * 5 / 100),
                    "external {external}: gentle {:?}",
                    settled.iter().map(|i| i.usage[0]).collect::<Vec<_>>()
                );
            }
        }
    }

    /// With room in the aggregate, a Yield First job is still held to its own
    /// mode's target: capacity less Yield First's 15% idle margin.
    #[test]
    fn a_yield_first_job_is_held_to_its_own_target_below_the_aggregate() {
        let run = simulate(&pair(AllocationClass::Opportunistic, 200), 0, 240);
        let settled = &run[120..];
        let gentle = settled.iter().map(|i| i.usage[0]).max().unwrap();
        assert!(gentle <= CPU * 85 / 100, "{gentle}");
        assert!(
            average(settled, 0) + CPU / 10 >= CPU * 85 / 100,
            "{}",
            average(settled, 0)
        );
        assert_eq!(
            settled.last().unwrap().aggregate,
            settled.last().unwrap().shadow_aggregate
        );
    }

    /// A Guaranteed job keeps its CPU request in every tier (FR-024).
    #[test]
    fn a_guaranteed_yield_first_job_keeps_its_cpu_request() {
        let run = simulate(&pair(AllocationClass::Guaranteed, CPU), 1000, 240);
        let settled = &run[120..];
        assert!(
            settled.iter().all(|i| i.usage[0] >= 1000),
            "{:?}",
            settled.iter().map(|i| i.usage[0]).collect::<Vec<_>>()
        );
    }
}

/// Each GPU's memory admission opens on its own cooldown, whatever CPU and RAM do
/// (FR-034).
mod gpu_cooldown_alone {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{Admission, ContentionMode, ResourceKey};

    fn cfg(mode: ContentionMode) -> Config {
        let mut cfg = config();
        cfg.cpu.reserve_physical_cores = 0;
        // The mode's own interval: 10 s in Neutral, 2 s in Performance.
        cfg.gpu.scale_up_cooldown_ms = None;
        cfg.contention.default_mode = mode;
        cfg.contention
            .acknowledge_caution
            .insert("performance_mode".into());
        cfg
    }

    fn device(uuid: &str) -> GpuSnapshot {
        let mut gpu = gpu_snapshot().gpus.remove(0);
        gpu.uuid = uuid.into();
        gpu.utilization_percent = Some(0);
        gpu.used_memory_mib = Some(2000);
        gpu
    }

    /// A saturated CPU: own work at 6000m beside an external user at 2000m, so the
    /// job is over its CPU target and capped.
    fn contended(cpu_known: bool) -> (Snapshot, PolicyInput) {
        let mut snap = snapshot();
        snap.gpu_inventory = CapabilityStatus::Available;
        snap.gpus = vec![device("GPU-a"), device("GPU-b")];
        snap.cpu_busy_millicores = cpu_known.then_some(8000);
        snap.principals = Some(vec![
            cpu_principal(1000, true, 0),
            cpu_principal(2001, false, 2000),
        ]);
        let job = allocation(
            "job",
            AllocationPhase::Running,
            resources(1000, 100, 2000),
            Some(resources(6000, 100, 2000)),
        );
        (
            snap,
            PolicyInput {
                allocations: vec![job],
                facts: rootless(&["job"]),
                ..PolicyInput::default()
            },
        )
    }

    fn admission(decision: &Decision, key: ResourceKey) -> Admission {
        decision.resources[&key].admission.clone().unwrap()
    }

    #[test]
    fn gpu_memory_opens_on_its_own_cooldown_beside_a_capped_cpu() {
        for (mode, cooldown_ms) in [
            (ContentionMode::Neutral, 10_000),
            (ContentionMode::Performance, 2000),
        ] {
            let cfg = cfg(mode);
            let (snap, input) = contended(true);
            let mut engine = PolicyEngine::new();
            let mut opened = None;
            for i in 0..30u64 {
                let now = i * 500;
                let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
                assert!(
                    decision
                        .actions
                        .iter()
                        .any(|a| a.resource == ResourceKey::Cpu),
                    "{mode:?}"
                );
                if opened.is_none()
                    && admission(&decision, ResourceKey::GpuMemory("GPU-a".into()))
                        == Admission::Open
                {
                    assert!(
                        decision
                            .admission_headroom
                            .gpu_memory_mib
                            .get("GPU-a")
                            .copied()
                            .unwrap_or(0)
                            > 0
                    );
                    opened = Some(now);
                }
            }
            assert_eq!(opened, Some(cooldown_ms), "{mode:?}");
        }
    }

    #[test]
    fn an_external_gpu_workload_resets_only_its_device() {
        let cfg = cfg(ContentionMode::Neutral);
        let (mut snap, input) = contended(true);
        let mut engine = PolicyEngine::new();
        for i in 0..=24u64 {
            engine.evaluate(&cfg, &snap, &input, i * 500).unwrap();
        }
        snap.gpus[0].external_process_ids = Some(vec![987]);
        snap.gpus[0].used_memory_mib = Some(6000);
        let decision = engine.evaluate(&cfg, &snap, &input, 12_500).unwrap();
        assert_ne!(
            admission(&decision, ResourceKey::GpuMemory("GPU-a".into())),
            Admission::Open
        );
        assert_eq!(
            admission(&decision, ResourceKey::GpuMemory("GPU-b".into())),
            Admission::Open
        );
    }

    #[test]
    fn an_unknown_cpu_reading_leaves_gpu_memory_counting() {
        let cfg = cfg(ContentionMode::Neutral);
        let (snap, input) = contended(false);
        let mut engine = PolicyEngine::new();
        let mut last = None;
        for i in 0..=20u64 {
            last = Some(engine.evaluate(&cfg, &snap, &input, i * 500).unwrap());
        }
        let decision = last.unwrap();
        assert_eq!(
            admission(&decision, ResourceKey::GpuMemory("GPU-a".into())),
            Admission::Open
        );
        assert_eq!(
            admission(&decision, ResourceKey::Cpu).code(),
            Some("admission_measurement_unknown")
        );
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "gpu_memory_opens_on_its_own_cooldown_beside_a_capped_cpu",
            gpu_memory_opens_on_its_own_cooldown_beside_a_capped_cpu,
        ),
        (
            "an_external_gpu_workload_resets_only_its_device",
            an_external_gpu_workload_resets_only_its_device,
        ),
        (
            "an_unknown_cpu_reading_leaves_gpu_memory_counting",
            an_unknown_cpu_reading_leaves_gpu_memory_counting,
        ),
    ];
}

/// Admission is blocked for own work only while it is being reclaimed; a cap that
/// holds at the target leaves admission to headroom and cooldown.
mod reclaim_admission {
    use super::*;
    use crate::snapshot::PolicyEngine;
    use cedegrid::contention::{Admission, ContentionMode, ResourceKey, YieldStep};

    const CPU: u64 = 32_000;

    fn cfg(mode: ContentionMode) -> Config {
        let mut cfg = Config::default();
        cfg.contention.default_mode = mode;
        cfg.contention
            .acknowledge_caution
            .insert("performance_mode".into());
        cfg
    }

    fn host(busy: u64, external: u64) -> Snapshot {
        let mut snap = snapshot();
        snap.cpu_capacity_millicores = CPU;
        snap.physical_cores = Some(16);
        snap.total_ram_mib = 262_144;
        snap.available_ram_mib = Some(250_000);
        snap.cpu_busy_millicores = Some(busy.min(CPU));
        snap.principals = Some(vec![
            cpu_principal(1000, true, 0),
            cpu_principal(2001, false, external),
        ]);
        snap
    }

    fn share(demands: &[u64]) -> Vec<u64> {
        let mut given = vec![0u64; demands.len()];
        let mut left = CPU;
        let mut open: Vec<usize> = (0..demands.len()).filter(|i| demands[*i] > 0).collect();
        while left > 0 && !open.is_empty() {
            let each = (left / open.len() as u64).max(1);
            let mut next = vec![];
            for i in open {
                let take = (demands[i] - given[i]).min(each).min(left);
                given[i] += take;
                left -= take;
                if given[i] < demands[i] {
                    next.push(i);
                }
            }
            open = next;
        }
        given
    }

    /// Jobs by ID with what each wants, beside one external user; every cap and
    /// pause applies at once.
    struct Sim {
        engine: PolicyEngine,
        cfg: Config,
        actions: Vec<YieldAction>,
        now: u64,
    }

    impl Sim {
        fn new(mode: ContentionMode) -> Self {
            Self {
                engine: PolicyEngine::new(),
                cfg: cfg(mode),
                actions: vec![],
                now: 0,
            }
        }

        fn step(&mut self, external: u64, jobs: &[(&str, u64, u64)]) -> Decision {
            let limit = |id: &str| {
                self.actions
                    .iter()
                    .find(|a| a.allocation_id == id && a.resource == ResourceKey::Cpu)
                    .map_or(CPU, |a| match a.step {
                        YieldStep::Cap => a.target.unwrap_or(CPU),
                        YieldStep::Pause => 0,
                        _ => CPU,
                    })
            };
            let mut demands = vec![external];
            demands.extend(jobs.iter().map(|(id, _, wants)| (*wants).min(limit(id))));
            let given = share(&demands);
            let allocations: Vec<Allocation> = jobs
                .iter()
                .zip(&given[1..])
                .map(|((id, request, _), used)| {
                    allocation(
                        id,
                        AllocationPhase::Running,
                        resources(*request, 100, 0),
                        Some(resources(*used, 100, 0)),
                    )
                })
                .collect();
            let ids: Vec<&str> = jobs.iter().map(|(id, _, _)| *id).collect();
            let decision = self
                .engine
                .evaluate(
                    &self.cfg,
                    &host(given.iter().sum(), given[0]),
                    &PolicyInput {
                        facts: rootless(&ids),
                        allocations,
                        ..PolicyInput::default()
                    },
                    self.now,
                )
                .unwrap();
            self.actions = decision.actions.clone();
            self.now += 500;
            decision
        }

        fn run(
            &mut self,
            intervals: u64,
            external: u64,
            jobs: &[(&str, u64, u64)],
        ) -> Vec<Decision> {
            (0..intervals).map(|_| self.step(external, jobs)).collect()
        }
    }

    fn cpu_admission(decision: &Decision) -> Admission {
        decision.resources[&ResourceKey::Cpu]
            .admission
            .clone()
            .unwrap()
    }

    fn yielding(decision: &Decision) -> bool {
        cpu_admission(decision).code() == Some("admission_own_work_yielding")
    }

    fn request(cpu: u64) -> Resources {
        resources(cpu, 100, 0)
    }

    /// A steady 12.5% user recorded as satisfied and a job that wants the whole
    /// CPU, capped and holding at the target share.
    fn capped(mode: ContentionMode) -> Sim {
        let mut sim = Sim::new(mode);
        let capped = sim.run(120, 4000, &[("old", 1000, CPU)]);
        let last = capped.last().unwrap();
        let target = last.resources[&ResourceKey::Cpu].target_share;
        assert!(
            last.actions
                .iter()
                .any(|a| a.resource == ResourceKey::Cpu && a.target == target),
            "{:?}",
            last.actions
        );
        // The item's setup is Neutral, where the steady user is recorded as satisfied.
        if mode == ContentionMode::Neutral {
            assert!(
                last.resources[&ResourceKey::Cpu]
                    .reasons
                    .iter()
                    .any(|r| r.code == "satisfied_demand"),
                "{:?}",
                last.resources[&ResourceKey::Cpu].reasons
            );
        }
        sim
    }

    /// The setup: `capped`, then the job's usage falls to 10 000 m for one full
    /// p95 window.
    fn settled(mode: ContentionMode) -> (Sim, Decision) {
        let mut sim = capped(mode);
        let fallen = sim.run(130, 4000, &[("old", 1000, 10_000)]);
        (sim, fallen.last().unwrap().clone())
    }

    /// What a negotiated coordinator places for one `cpu` request, from the report
    /// the agent builds for `decision`.
    fn coordinator_places(sim: &Sim, decision: &Decision, cpu: u64) -> Vec<String> {
        use cedegrid::protocol::*;
        let dir = StateDir(
            tempfile::Builder::new()
                .prefix(".coordinator-test-")
                .tempdir_in(std::env::current_dir().unwrap())
                .unwrap(),
        );
        let mut coordinator = cedegrid::coordinator::Coordinator::open(CoordinatorConfig {
            storage_profile: Default::default(),
            state_dir: dir.path().join("state"),
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: TlsIdentity {
                ca_cert: Default::default(),
                certificate: Default::default(),
                private_key: Default::default(),
            },
            clients: BTreeMap::new(),
            lease_ms: 10_000,
            telemetry_ttl_ms: 3000,
            max_artifact_bytes: 1024 * 1024,
            artifact_quota_bytes: 10 * 1024 * 1024,
            retry_limit: 3,
            retry_backoff_ms: 1000,
            retry_backoff_max_ms: 30_000,
            yield_retry_backoff_ms: 1000,
            yield_retry_backoff_max_ms: 30_000,
        })
        .unwrap();
        let pool = PoolSpec {
            pool_id: "pool".into(),
            class: AllocationClass::Opportunistic,
            node_ids: vec!["node".into()],
            min_workers: 0,
            max_workers: 8,
        };
        coordinator
            .handle_at(&Principal::Operator, Request::PutPool { pool }, 100)
            .unwrap();
        let task = cedegrid::execution_model::LaunchRequest {
            task_id: "new".into(),
            assignment_id: String::new(),
            argv: vec!["/bin/true".into()],
            cwd: std::env::current_dir().unwrap(),
            env: BTreeMap::new(),
            resources: request(cpu),
            replay_safe: true,
            class: AllocationClass::Opportunistic,
            no_escape: true,
            single_process: true,
            managed_child_limit: 0,
            max_attempts: None,
            input_artifacts: vec![],
            required_controls: vec![],
            allow_fallback: true,
            contention: None,
        };
        let job = JobSpec {
            job_id: "job".into(),
            pool_id: "pool".into(),
            priority: 0,
            tasks: vec![task],
        };
        coordinator
            .handle_at(&Principal::Operator, Request::Submit { job }, 100)
            .unwrap();
        let report = NodeReport {
            node_id: "node".into(),
            boot_id: "boot".into(),
            observed_at_unix_ms: 100,
            managed_budget: cedegrid::policy::schedulable_budget(decision),
            expansion_allowed: true,
            gpu_expansion_allowed: true,
            gpu_guaranteed_allowed: true,
            launch_slots: 2,
            available_controls: vec![],
            allocations: vec![],
            features: COORDINATOR_FEATURES.map(String::from).to_vec(),
            contention: Some(cedegrid::agent::node_contention_report(
                &sim.cfg, decision, 8,
            )),
            sample_age_ms: None,
        };
        let Response::Heartbeat { reply } = coordinator
            .handle_at(
                &Principal::Node {
                    node_id: "node".into(),
                },
                Request::Heartbeat { report },
                100,
            )
            .unwrap()
        else {
            panic!()
        };
        reply
            .assignments
            .iter()
            .map(|a| a.request.task_id.clone())
            .collect()
    }

    /// FR-028, end to end: while own work is reclaimed on CPU, nothing that asks
    /// for CPU is placed.
    #[test]
    fn a_reclaiming_resource_places_nothing_end_to_end() {
        let (mut sim, _) = settled(ContentionMode::Neutral);
        let jobs = [("old", 1000, 10_000), ("new", 8000, 8000)];
        sim.run(10, 4000, &jobs);
        let reclaiming = (0..240)
            .map(|_| sim.step(20_000, &jobs))
            .find(yielding)
            .expect("own work is reclaimed");
        assert!(coordinator_places(&sim, &reclaiming, 1000).is_empty());
    }

    /// FR-028, end to end: a cap that holds at the target with room below it, so an
    /// 8000 m job passes `request_admission` and the coordinator places it.
    #[test]
    fn a_holding_cap_places_an_8000_m_job_end_to_end() {
        let (sim, decision) = settled(ContentionMode::Neutral);
        assert!(
            decision
                .actions
                .iter()
                .any(|a| a.resource == ResourceKey::Cpu && a.step == YieldStep::Cap)
        );
        assert_eq!(
            cedegrid::policy::request_admission(&decision, &request(8000)),
            Ok(())
        );
        assert_eq!(coordinator_places(&sim, &decision, 8000), ["new"]);
    }

    /// External usage that jitters by 2% of capacity beside a holding cap toggles
    /// neither admission nor placement: with room below the cap, and with the job
    /// using all of it, so the cap follows the target share down and up.
    #[test]
    fn small_external_jitter_keeps_admission_and_placement_steady() {
        let cases = [
            (settled(ContentionMode::Neutral).0, 10_000),
            (
                {
                    let mut sim = capped(ContentionMode::Neutral);
                    sim.run(140, 4000, &[("old", 1000, CPU)]);
                    sim
                },
                CPU,
            ),
        ];
        for (mut sim, wants) in cases {
            let jitter = CPU / 50;
            let mut answers = vec![];
            for i in 0..20u64 {
                let external = match i % 4 {
                    0 => 4000 + jitter,
                    2 => 4000 - jitter,
                    _ => 4000,
                };
                let decision = sim.step(external, &[("old", 1000, wants)]);
                assert!(
                    decision
                        .actions
                        .iter()
                        .any(|a| a.resource == ResourceKey::Cpu && a.step == YieldStep::Cap),
                    "wants {wants}, interval {i}: the cap holds"
                );
                assert_eq!(
                    cpu_admission(&decision),
                    Admission::Open,
                    "wants {wants}, interval {i}, external {external}"
                );
                answers.push((
                    cedegrid::policy::request_admission(&decision, &request(8000)),
                    coordinator_places(&sim, &decision, 8000),
                ));
            }
            assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
            if wants == 10_000 {
                assert_eq!(answers[0], (Ok(()), vec!["new".to_string()]));
            }
        }
    }

    #[test]
    fn a_holding_cap_leaves_room_below_the_target_open() {
        let (_, decision) = settled(ContentionMode::Neutral);
        assert_eq!(
            decision.resources[&ResourceKey::Cpu].target_share,
            Some(26_400)
        );
        assert!(!yielding(&decision), "{:?}", cpu_admission(&decision));
        assert_eq!(cpu_admission(&decision), Admission::Open);
        assert_eq!(
            cedegrid::policy::request_admission(&decision, &request(8000)),
            Ok(())
        );
        let own: u64 = decision.resources[&ResourceKey::Cpu].own.unwrap().0;
        assert!(own + 8000 <= 26_400, "{own}");
    }

    #[test]
    fn a_request_over_own_target_is_held() {
        let (mut sim, _) = settled(ContentionMode::Neutral);
        // The admitted 8000 m job runs beside the old one.
        let decision = sim.step(4000, &[("old", 1000, 10_000), ("new", 8000, 8000)]);
        assert_eq!(
            cedegrid::policy::request_admission(&decision, &request(2000)),
            Ok(())
        );
        assert_eq!(
            cedegrid::policy::request_admission(&decision, &request(9000)),
            Err("admission_over_own_target")
        );
    }

    #[test]
    fn growth_past_the_fair_share_blocks_admission_until_reclaim_completes() {
        let (mut sim, _) = settled(ContentionMode::Neutral);
        let jobs = [("old", 1000, 10_000), ("new", 8000, 8000)];
        sim.run(10, 4000, &jobs);
        let grown = sim.run(240, 20_000, &jobs);
        let first = grown
            .iter()
            .position(|d| !d.actions.is_empty() && yielding(d))
            .expect("a step and a block");
        // Blocked while own work is reclaimed; once it holds, admission reopens
        // only after the CPU cooldown, never in the same interval.
        let released = grown[first..]
            .iter()
            .position(|d| !yielding(d))
            .expect("reclaim completes")
            + first;
        let open = grown[released..]
            .iter()
            .position(|d| cpu_admission(d) == Admission::Open)
            .map(|i| i + released);
        if let Some(open) = open {
            assert!(
                (open - released) as u64 * 500 >= 10_000,
                "{released} {open}"
            );
        }
    }

    /// Admit, grow, cap, admit: the cooldown after each reclaim stops a cycle.
    #[test]
    fn the_cooldown_stops_an_admit_grow_cap_cycle() {
        let (mut sim, _) = settled(ContentionMode::Neutral);
        let mut jobs: Vec<(String, u64, u64)> = vec![("old".into(), 1000, 10_000)];
        let mut opens = vec![];
        let mut previous_open = false;
        for _ in 0..600 {
            let view: Vec<(&str, u64, u64)> = jobs
                .iter()
                .map(|(id, r, w)| (id.as_str(), *r, *w))
                .collect();
            let decision = sim.step(4000, &view);
            let open = cedegrid::policy::request_admission(&decision, &request(4000)).is_ok();
            if open && !previous_open {
                opens.push(sim.now);
                // Each admitted job wants more than it asked for.
                jobs.push((format!("job-{}", jobs.len()), 4000, 12_000));
            }
            previous_open = open;
        }
        for pair in opens.windows(2) {
            assert!(pair[1] - pair[0] >= 10_000, "{opens:?}");
        }
    }

    /// Modes stay monotonic: Yield First admits no more than Neutral, Neutral no
    /// more than Performance, in the same state.
    #[test]
    fn modes_admit_monotonically() {
        let headroom = |mode| settled(mode).1.admission_headroom.cpu_millicores;
        let (yield_first, neutral, performance) = (
            headroom(ContentionMode::YieldFirst),
            headroom(ContentionMode::Neutral),
            headroom(ContentionMode::Performance),
        );
        assert!(
            yield_first <= neutral && neutral <= performance,
            "{yield_first} {neutral} {performance}"
        );
    }

    /// The same for GPU compute: a cap that holds at the share does not block.
    #[test]
    fn a_holding_gpu_compute_cap_does_not_block_admission() {
        let key = ResourceKey::GpuCompute("GPU-test".into());
        let mut cfg = cfg(ContentionMode::Neutral);
        cfg.gpu.process_sample_max_age_ms = Some(600);
        cfg.contention.gpu_compute.quiet_window_intervals = Some(2);
        cfg.contention
            .acknowledge_caution
            .insert("relaxed_detection".into());
        let mut engine = PolicyEngine::new();
        let mut actions: Vec<YieldAction> = vec![];
        let mut last = None;
        for i in 0..400u64 {
            let wants = if i < 200 { 1000 } else { 300 };
            let limit = actions
                .iter()
                .find(|a| a.resource == key)
                .map_or(1000, |a| match a.step {
                    YieldStep::Cap => a.target.unwrap_or(1000),
                    YieldStep::Pause => 0,
                    _ => 1000,
                });
            let gpu = share(&[125, wants.min(limit)]);
            let mut snap = gpu_snapshot();
            snap.gpus[0].external_compute = ComputeActivity::Active;
            snap.gpus[0].external_process_ids = Some(vec![987]);
            snap.gpus[0].external_compute_milli = Some(gpu[0].min(125));
            snap.gpus[0].own_compute_milli = Some(gpu[1].min(1000 - gpu[0].min(125)));
            snap.gpus[0].utilization_percent =
                Some(((gpu[0].min(125) + gpu[1]) / 10).min(100) as u32);
            snap.gpus[0].compute_sample_id = Some(i + 1);
            let mut user = cpu_principal(2001, false, 0);
            user.gpu_compute_milli = BTreeMap::from([("GPU-test".to_string(), gpu[0].min(125))]);
            snap.principals = Some(vec![cpu_principal(1000, true, 0), user]);
            let decision = engine
                .evaluate(
                    &cfg,
                    &snap,
                    &PolicyInput {
                        allocations: vec![allocation(
                            "job",
                            AllocationPhase::Running,
                            resources(100, 100, 2000),
                            Some(resources(100, 100, 2000)),
                        )],
                        facts: rootless(&["job"]),
                        ..PolicyInput::default()
                    },
                    i * 500,
                )
                .unwrap();
            actions = decision.actions.clone();
            last = Some(decision);
        }
        let decision = last.unwrap();
        assert_ne!(
            decision.resources[&key]
                .admission
                .as_ref()
                .and_then(|a| a.code()),
            Some("admission_own_work_yielding"),
            "{:?}",
            decision.resources[&key]
        );
    }

    /// This module's runs in the decision snapshot.
    pub(super) const SNAPSHOT: &[(&str, fn())] = &[
        (
            "a_reclaiming_resource_places_nothing_end_to_end",
            a_reclaiming_resource_places_nothing_end_to_end,
        ),
        (
            "a_holding_cap_places_an_8000_m_job_end_to_end",
            a_holding_cap_places_an_8000_m_job_end_to_end,
        ),
        (
            "small_external_jitter_keeps_admission_and_placement_steady",
            small_external_jitter_keeps_admission_and_placement_steady,
        ),
        (
            "a_holding_cap_leaves_room_below_the_target_open",
            a_holding_cap_leaves_room_below_the_target_open,
        ),
        (
            "a_request_over_own_target_is_held",
            a_request_over_own_target_is_held,
        ),
        (
            "growth_past_the_fair_share_blocks_admission_until_reclaim_completes",
            growth_past_the_fair_share_blocks_admission_until_reclaim_completes,
        ),
        (
            "the_cooldown_stops_an_admit_grow_cap_cycle",
            the_cooldown_stops_an_admit_grow_cap_cycle,
        ),
        ("modes_admit_monotonically", modes_admit_monotonically),
        (
            "a_holding_gpu_compute_cap_does_not_block_admission",
            a_holding_gpu_compute_cap_does_not_block_admission,
        ),
    ];
}

/// The scripted runs above as a decision snapshot (tests/support/snapshot.rs).
#[test]
#[ignore = "decision snapshot: needs CEDEGRID_DECISION_SNAPSHOT"]
fn decision_snapshot() {
    let mut scenarios = Vec::new();
    scenarios.extend(snapshot::scripted(
        "gpu_cooldown_alone",
        gpu_cooldown_alone::SNAPSHOT,
    ));
    scenarios.extend(snapshot::scripted(
        "reclaim_admission",
        reclaim_admission::SNAPSHOT,
    ));
    snapshot::check("policy", scenarios);
}

/// Own CPU and host busy over the 8-interval window set the steady-state budget, while the
/// per-interval estimate decides growth (FR-073).
mod measurement_window {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey};

    /// A scoped 4-CPU reading: host busy upper and lower, own lower, and optionally the
    /// window's host busy upper and lower and own lower.
    fn reading(busy: (u64, u64), own: u64, window: Option<((u64, u64), u64)>) -> Snapshot {
        let mut snap = scoped_snapshot();
        let cpu = &mut snap.kernel.as_mut().unwrap().cpu;
        cpu.busy_millicores = Some(busy.0);
        cpu.busy_lower_millicores = Some(busy.1);
        if let Some(((upper, lower), own_window)) = window {
            cpu.busy_window_millicores = Some(upper);
            cpu.busy_window_lower_millicores = Some(lower);
            cpu.busy_window_ms = Some(4000);
            snap.managed_cpu_window_lower_millicores = Some(own_window);
        }
        snap.managed_cpu_lower_millicores = Some(own);
        snap
    }

    fn input(own: u64, upper: u64) -> PolicyInput {
        let mut task = allocation(
            "a",
            AllocationPhase::Running,
            resources(250, 0, 0),
            Some(resources(own, 0, 0)),
        );
        task.observed_upper = Some(resources(upper, 0, 0));
        PolicyInput {
            allocations: vec![task],
            facts: rootless(&["a"]),
            ..Default::default()
        }
    }

    fn external(decision: &Decision) -> Option<u64> {
        decision.resources[&ResourceKey::Cpu].external
    }

    #[test]
    fn the_window_sets_the_steady_state_external_estimate_within_the_per_interval_bound() {
        let config = config();
        // Per interval: 3080 m busy (2990 m lower), own 2950 m (3000 m upper), so 130 m
        // external within a 140 m bound; the window reads 10 m.
        let windowed = reading((3080, 2990), 2950, Some(((2965, 2955), 2955)));
        let decision = PolicyEngine::new()
            .evaluate(&config, &windowed, &input(2950, 3000), 0)
            .unwrap();
        assert_eq!(external(&decision), Some(10));
        // Without a window the per-interval estimate stays as before.
        let plain = reading((3080, 2990), 2950, None);
        let decision = PolicyEngine::new()
            .evaluate(&config, &plain, &input(2950, 3000), 0)
            .unwrap();
        assert_eq!(external(&decision), Some(130));
    }

    /// A one-core external start reads beyond the per-interval bound at once, so it is used at
    /// once and own work is contended within 2 intervals in Yield First and Neutral (SC-002).
    #[test]
    fn a_real_external_start_is_contended_within_two_intervals_with_the_window_in_place() {
        for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
            let mut config = config();
            config.contention.default_mode = mode;
            let mut engine = PolicyEngine::new();
            let steady = reading((2980, 2960), 2950, Some(((2965, 2955), 2955)));
            let decision = engine
                .evaluate(&config, &steady, &input(2950, 2960), 0)
                .unwrap();
            assert!(
                decision.actions.is_empty(),
                "{mode:?}: {:?}",
                decision.actions
            );
            // A one-core contender: per interval 1050 m external, the window only an eighth of it.
            let started = reading((4000, 3990), 2950, Some(((3090, 3080), 2955)));
            let first = engine
                .evaluate(&config, &started, &input(2950, 2960), 500)
                .unwrap();
            assert_eq!(external(&first), Some(1050), "{mode:?}");
            let second = engine
                .evaluate(&config, &started, &input(2950, 2960), 1000)
                .unwrap();
            assert!(
                first
                    .actions
                    .iter()
                    .chain(&second.actions)
                    .any(|a| a.resource == ResourceKey::Cpu && a.allocation_id == "a"),
                "{mode:?}: no CPU step within 2 intervals"
            );
        }
    }

    /// The admission p95 takes each allocation's window value when it has one.
    #[test]
    fn the_admission_p95_takes_the_window_value() {
        let config = config();
        let credit = |window: Option<u64>| {
            let mut engine = PolicyEngine::new();
            let mut last = None;
            let mut now = 0;
            while now <= 61_000 {
                let mut input = input(250, 260);
                input.allocations[0].observed_window_cpu = window;
                let mut snap = reading((1300, 1280), 250, Some(((1290, 1280), 250)));
                // Fresh scoped telemetry each interval.
                snap.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + now;
                last = Some(engine.evaluate(&config, &snap, &input, now).unwrap());
                now += 500;
            }
            last.unwrap().cpu_admission_credit_millicores
        };
        assert_eq!(credit(None), 0);
        assert_eq!(credit(Some(200)), 50);
    }
}

/// One missing observation keeps an allocation's admission p95 window: once observed again
/// it is charged at its p95 at once, not at its request for another window (POL-4).
#[test]
fn a_missing_observation_keeps_the_admission_p95_window() {
    let cfg = config();
    let job = |observed: Option<Resources>| PolicyInput {
        allocations: vec![allocation(
            "a",
            AllocationPhase::Running,
            resources(4000, 0, 0),
            observed,
        )],
        facts: rootless(&["a"]),
        ..Default::default()
    };
    let seen = job(Some(resources(1000, 0, 0)));
    let snap = snapshot();
    let mut engine = PolicyEngine::new();
    let mut now = 0;
    let mut credit = 0;
    while now <= 61_000 {
        credit = engine
            .evaluate(&cfg, &snap, &seen, now)
            .unwrap()
            .cpu_admission_credit_millicores;
        now += 500;
    }
    assert_eq!(
        credit, 3000,
        "charged at its 1000m p95, not its 4000m request"
    );
    engine.evaluate(&cfg, &snap, &job(None), now).unwrap();
    now += 500;
    let back = engine.evaluate(&cfg, &snap, &seen, now).unwrap();
    assert_eq!(back.cpu_admission_credit_millicores, 3000);
}

/// A CPU drain planned while an allocation was pending cancels the reservation; once the
/// allocation runs anyway, it is re-planned as running work, which CPU contention alone never
/// drains (FR-012, SC-007).
#[test]
fn a_pending_pass_cpu_drain_is_replanned_once_the_allocation_runs() {
    use cedegrid::contention::{ResourceKey, YieldStep};
    let config = config();
    let mut engine = PolicyEngine::new();
    let running = allocation(
        "r",
        AllocationPhase::Running,
        resources(3000, 0, 0),
        Some(resources(3000, 0, 0)),
    );
    // External work uses 2000m: the 4000m budget fits the running task but not a 2000m
    // reservation beside it, so the pending pass cancels the reservation.
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(5000);
    let pending = allocation("p", AllocationPhase::Pending, resources(2000, 0, 0), None);
    let first = engine
        .evaluate(
            &config,
            &snap,
            &PolicyInput {
                allocations: vec![running.clone(), pending],
                facts: rootless(&["r", "p"]),
                ..PolicyInput::default()
            },
            0,
        )
        .unwrap();
    assert!(
        first
            .actions
            .iter()
            .any(|a| a.allocation_id == "p" && a.step == YieldStep::Drain),
        "{:?}",
        first.actions
    );
    // It was authorized before the cancellation landed and now runs at 1000m, within the
    // budget: nothing drains it.
    let started = allocation(
        "p",
        AllocationPhase::Running,
        resources(2000, 0, 0),
        Some(resources(1000, 0, 0)),
    );
    snap.cpu_busy_millicores = Some(6000);
    for now in [500, 1000, 1500] {
        let decision = engine
            .evaluate(
                &config,
                &snap,
                &PolicyInput {
                    allocations: vec![running.clone(), started.clone()],
                    facts: rootless(&["r", "p"]),
                    ..PolicyInput::default()
                },
                now,
            )
            .unwrap();
        assert!(
            !decision
                .actions
                .iter()
                .any(|a| a.resource == ResourceKey::Cpu && a.step >= YieldStep::Drain),
            "at {now} ms: {:?}",
            decision.actions
        );
        assert!(decision.would_drain.is_empty(), "at {now} ms");
    }
}

/// Own CPU usage over its target while the budget did not fall is enforced first and stepped
/// down only after the dwell, with its own cause (FR-011, FR-018, research R25).
mod own_over_target {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldCause, YieldStep};

    const CAPACITY: u64 = 4000;

    fn config(mode: ContentionMode) -> Config {
        let mut config = super::config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = mode;
        config
            .contention
            .acknowledge_caution
            .insert("performance_mode".into());
        config
    }

    /// A scoped 4-CPU reading at `now`: own usage per task, external usage, per-interval
    /// noise within the bound, and a steady window.
    fn reading(usage: &[u64], external: u64, noise: u64, now: u64) -> (Snapshot, PolicyInput) {
        let own: u64 = usage.iter().sum();
        let mut snap = scoped_snapshot();
        let kernel = snap.kernel.as_mut().unwrap();
        kernel.monotonic_elapsed_ms = 500 + now;
        let cpu = &mut kernel.cpu;
        let busy = (own + external + noise).min(CAPACITY);
        cpu.busy_millicores = Some(busy);
        cpu.busy_lower_millicores = Some(busy.saturating_sub(80));
        cpu.busy_window_millicores = Some((own + external).min(CAPACITY));
        cpu.busy_window_lower_millicores = Some((own + external).min(CAPACITY).saturating_sub(10));
        cpu.busy_window_ms = Some(4000);
        snap.managed_cpu_lower_millicores = Some(own);
        snap.managed_cpu_window_lower_millicores = Some(own);
        let ids: Vec<String> = (0..usage.len()).map(|i| format!("t{i}")).collect();
        let allocations = ids
            .iter()
            .zip(usage)
            .map(|(id, used)| {
                let mut task = allocation(
                    id,
                    AllocationPhase::Running,
                    resources(250, 0, 0),
                    Some(resources(*used, 0, 0)),
                );
                task.observed_upper = Some(resources(used + 2, 0, 0));
                task
            })
            .collect();
        let input = PolicyInput {
            allocations,
            facts: rootless(&ids.iter().map(String::as_str).collect::<Vec<_>>()),
            ..PolicyInput::default()
        };
        (snap, input)
    }

    fn cpu_actions(decision: &Decision) -> Vec<(String, YieldStep, Option<u64>, YieldCause)> {
        decision
            .actions
            .iter()
            .filter(|a| a.resource == ResourceKey::Cpu)
            .map(|a| (a.allocation_id.clone(), a.step, a.target, a.cause))
            .collect()
    }

    #[test]
    fn own_usage_over_its_target_holds_for_the_dwell_then_steps_with_its_own_cause() {
        let config = config(ContentionMode::Performance);
        let mut engine = PolicyEngine::new();
        // Two tasks at 1960m beside 40m external: 3920m against a 3880m target.
        let mut first_step = None;
        for now in (0..=3000).step_by(500) {
            let (snap, input) = reading(&[1960, 1960], 40, 0, now);
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            let actions = cpu_actions(&decision);
            if !actions.is_empty() && first_step.is_none() {
                // The node report counts steps by cause, so `status` shows it apart.
                let report = cedegrid::agent::node_contention_report(&config, &decision, 2);
                assert_eq!(
                    report.resources[&ResourceKey::Cpu].cause_summary,
                    std::collections::BTreeMap::from([(YieldCause::OwnOverTarget, 2)])
                );
                first_step = Some((now, actions));
            }
        }
        let (at, actions) = first_step.expect("own work never stepped down");
        assert!(at >= 1500, "stepped at {at} ms, before the dwell");
        assert!(
            actions
                .iter()
                .all(|(_, step, _, cause)| *step == YieldStep::Cap
                    && *cause == YieldCause::OwnOverTarget),
            "{actions:?}"
        );
    }

    #[test]
    fn a_budget_that_falls_keeps_external_growth_and_its_bounds() {
        let config = config(ContentionMode::Performance);
        let mut engine = PolicyEngine::new();
        let (snap, input) = reading(&[1450, 1450], 40, 0, 0);
        assert!(cpu_actions(&engine.evaluate(&config, &snap, &input, 0).unwrap()).is_empty());
        // A 1000m external user arrives: the budget falls to 2880m, and own work steps at once.
        let (mut snap, input) = reading(&[1450, 1450], 1040, 0, 500);
        let cpu = &mut snap.kernel.as_mut().unwrap().cpu;
        cpu.busy_window_millicores = Some(2940 + 125);
        let actions = cpu_actions(&engine.evaluate(&config, &snap, &input, 500).unwrap());
        assert!(!actions.is_empty(), "no step for external growth");
        assert!(
            actions
                .iter()
                .all(|(_, _, _, cause)| *cause == YieldCause::ExternalGrowth),
            "{actions:?}"
        );
    }

    /// Caps already held yield an external increase once between them, at one level, and not
    /// each the whole increase (FR-011, FR-013).
    #[test]
    fn held_caps_yield_external_growth_once_between_them() {
        let config = config(ContentionMode::Performance);
        let mut engine = PolicyEngine::new();
        // Four tasks at 980m beside 40m external, 40m over the target: after the dwell, all
        // four hold caps at one level.
        let mut held = Vec::new();
        for now in (0..=3000).step_by(500) {
            let (snap, input) = reading(&[980; 4], 40, 0, now);
            held = cpu_actions(&engine.evaluate(&config, &snap, &input, now).unwrap());
        }
        assert_eq!(held.len(), 4, "{held:?}");
        assert!(
            held.iter().all(|(_, _, target, _)| *target == Some(970)),
            "{held:?}"
        );
        // A 400m external user arrives and the host gives own work 3600m: 80m over the new
        // 3520m target, which the caps give between them, 20m each.
        let (snap, input) = reading(&[900; 4], 400, 0, 3500);
        let lowered = cpu_actions(&engine.evaluate(&config, &snap, &input, 3500).unwrap());
        let after: Vec<u64> = lowered
            .iter()
            .map(|(_, _, target, _)| target.unwrap())
            .collect();
        let freed: u64 = after
            .iter()
            .map(|target| 900u64.saturating_sub(*target))
            .sum();
        assert!((76..=84).contains(&freed), "{after:?}");
        assert!(after.iter().all(|t| t.abs_diff(after[0]) <= 1), "{after:?}");
    }

    /// One thread per claim, weighted max-min over the capacity.
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

    /// Report 026's run 6 shape: 16 tasks of 250m that make up lost CPU, on 4 CPUs beside a
    /// steady 60m user, in Yield First for 5 minutes, read one interval late with
    /// per-interval noise inside the bound. No pause or drain, caps that change only when
    /// external usage changes, and own work averaging at least 90% of capacity minus the user
    /// and the margin (SC-015, SC-007).
    #[test]
    fn the_run_6_shape_converges_without_pauses_or_cap_cycling() {
        const TASKS: usize = 16;
        const INTERVAL: u64 = 500;
        let config = config(ContentionMode::YieldFirst);
        let mut engine = PolicyEngine::new();
        let mut steps: Vec<(YieldStep, Option<u64>)> = vec![(YieldStep::None, None); TASKS];
        let mut debt = [0u64; TASKS];
        let mut last = vec![250u64; TASKS];
        let mut seed: u64 = 9;
        let (mut own_sum, mut intervals) = (0u64, 0u64);
        let mut changes_after_convergence = 0;
        let mut previous_caps: Option<Vec<(YieldStep, Option<u64>)>> = None;
        for tick in 0..600u64 {
            let now = tick * INTERVAL;
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let noise = (seed >> 33) % 40;
            let mut demands: Vec<u64> = (0..TASKS)
                .map(|i| {
                    let want = (250 + debt[i] * 1000 / INTERVAL).min(1000);
                    match steps[i] {
                        (YieldStep::Pause, _) => 0,
                        (YieldStep::Cap, target) => target.unwrap_or(want).min(want),
                        _ => want,
                    }
                })
                .collect();
            demands.push(60);
            let fresh = schedule(&demands);
            for i in 0..TASKS {
                debt[i] =
                    (debt[i] * 1000 + 250 * INTERVAL).saturating_sub(fresh[i] * INTERVAL) / 1000;
            }
            // The reading lags one interval; the user gets what the CPUs left it.
            let own_seen: u64 = last.iter().sum();
            let external_seen = fresh[TASKS].min(CAPACITY - own_seen);
            let (snap, input) = reading(&last, external_seen, noise, now);
            last = fresh[..TASKS].to_vec();
            own_sum += last.iter().sum::<u64>();
            intervals += 1;
            let decision = engine.evaluate(&config, &snap, &input, now).unwrap();
            for action in cpu_actions(&decision) {
                assert!(
                    action.1 != YieldStep::Pause && action.1 < YieldStep::Drain,
                    "at {now} ms: {action:?}"
                );
            }
            for (i, step) in steps.iter_mut().enumerate() {
                *step = decision
                    .actions
                    .iter()
                    .find(|a| a.allocation_id == format!("t{i}") && a.resource == ResourceKey::Cpu)
                    .map_or((YieldStep::None, None), |a| (a.step, a.target));
            }
            if now >= 20_000 {
                if previous_caps.as_ref().is_some_and(|caps| caps != &steps) {
                    changes_after_convergence += 1;
                }
                previous_caps = Some(steps.clone());
            }
        }
        assert_eq!(
            changes_after_convergence, 0,
            "caps changed with external usage steady"
        );
        // Capacity less the 60m user and Yield First's 15% margin.
        let target = CAPACITY - 60 - 600;
        let average = own_sum / intervals;
        assert!(
            average * 10 >= target * 9,
            "own work averaged {average}m of {target}m"
        );
    }
}

/// CedeGrid's own CPU, the agent and its supervisors, is in use by CedeGrid on the owner's
/// side: it is taken out of external usage and the owner's unmanaged usage and lowers own
/// tasks' budget, so it never forms an unattributed principal (FR-059, FR-069, SC-014).
mod cedegrid_usage {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey};

    fn config() -> Config {
        let mut config = super::config();
        config.cpu.reserve_physical_cores = 0;
        config.contention.default_mode = ContentionMode::Neutral;
        config
    }

    /// Own tasks use 6000m of 8000m; CedeGrid 400m.
    fn reading(busy: u64, principals: Option<Vec<PrincipalUsage>>) -> (Snapshot, PolicyInput) {
        let mut snap = snapshot();
        snap.cpu_busy_millicores = Some(busy);
        snap.cedegrid_cpu_lower_millicores = Some(400);
        snap.principals = principals;
        let input = PolicyInput {
            allocations: vec![allocation(
                "job",
                AllocationPhase::Running,
                resources(6000, 0, 0),
                Some(resources(6000, 0, 0)),
            )],
            facts: rootless(&["job"]),
            ..PolicyInput::default()
        };
        (snap, input)
    }

    #[test]
    fn cedegrid_cpu_is_not_an_unattributed_principal() {
        let config = config();
        let (snap, input) = reading(6400, None);
        let decision = PolicyEngine::new()
            .evaluate(&config, &snap, &input, 0)
            .unwrap();
        let cpu = &decision.resources[&ResourceKey::Cpu];
        assert_eq!(cpu.external, Some(0));
        assert_eq!(cpu.cedegrid, Some(400));
        // Neutral keeps its 800m margin; CedeGrid's 400m comes out of own work's 7200m.
        assert_eq!(cpu.target_share, Some(6800));
        let report = cedegrid::agent::node_contention_report(&config, &decision, 1);
        assert_eq!(report.resources[&ResourceKey::Cpu].cedegrid, Some(400));
        let text = cedegrid::explain::doctor_text(&config, &snap, &decision, &[]);
        assert!(text.contains("in use (CedeGrid) 400 m"), "{text}");
        // In use, not held back: Neutral's margin is all the CPU keeps free.
        let held = cpu.held_back.as_ref().unwrap();
        assert_eq!((held.margin, held.contention), (800, 0));
    }

    #[test]
    fn cedegrid_cpu_comes_out_of_the_owners_unmanaged_usage() {
        let config = config();
        // The owner's processes use 500m beside own tasks: CedeGrid's 400m and a 100m program.
        let owner = PrincipalUsage {
            uid: Some(1000),
            is_owner: true,
            cpu_millicores: 500,
            ..PrincipalUsage::default()
        };
        let (snap, input) = reading(6500, Some(vec![owner]));
        let decision = PolicyEngine::new()
            .evaluate(&config, &snap, &input, 0)
            .unwrap();
        let cpu = &decision.resources[&ResourceKey::Cpu];
        assert_eq!(cpu.external, Some(100));
        assert_eq!(cpu.cedegrid, Some(400));
        assert_eq!(cpu.target_share, Some(6700));
    }

    /// CedeGrid's reading never makes the host reading inconsistent: beyond what the host
    /// reports busy, it is clamped.
    #[test]
    fn cedegrid_cpu_beyond_host_busy_is_clamped() {
        let config = config();
        let (snap, input) = reading(6200, None);
        let decision = PolicyEngine::new()
            .evaluate(&config, &snap, &input, 0)
            .unwrap();
        let cpu = &decision.resources[&ResourceKey::Cpu];
        assert_eq!(cpu.external, Some(0));
        assert_eq!(cpu.cedegrid, Some(200));
        assert_ne!(
            cpu.measurement,
            Some(cedegrid::contention::Measurement::Unknown)
        );
    }
}

mod oom_order {
    use super::*;

    /// Two own tasks of 10 000 and 6 000 MiB beside external usage that leaves
    /// `available` MiB free, on 32 000 MiB with a 1 000 MiB floor.
    fn evaluate(engine: &mut PolicyEngine, cfg: &Config, available: u64, now: u64) -> Decision {
        let mut snap = snapshot();
        snap.available_ram_mib = Some(available);
        engine
            .evaluate(
                cfg,
                &snap,
                &PolicyInput {
                    allocations: vec![
                        allocation(
                            "a",
                            AllocationPhase::Running,
                            resources(100, 10_000, 0),
                            Some(resources(100, 10_000, 0)),
                        ),
                        allocation(
                            "b",
                            AllocationPhase::Running,
                            resources(100, 6_000, 0),
                            Some(resources(100, 6_000, 0)),
                        ),
                    ],
                    facts: rootless(&["a", "b"]),
                    ..PolicyInput::default()
                },
                now,
            )
            .unwrap()
    }

    /// OOM order by fault (FR-023): in-share own RAM marks nothing; own RAM above its fair
    /// share marks exactly the tasks that make up the excess, in the memory victim order;
    /// back within its share nothing stays marked.
    #[test]
    fn own_ram_above_its_fair_share_marks_the_excess_tasks_in_victim_order() {
        let cfg = config();
        let mut engine = PolicyEngine::new();
        // 6 000 MiB external: own work's fair share is all but that, 25 000 MiB.
        let calm = evaluate(&mut engine, &cfg, 10_000, 0);
        assert!(calm.oom_preferred.is_empty(), "{:?}", calm.oom_preferred);
        // 16 000 MiB external: the max-min share is 15 500 MiB, so 500 MiB is own excess and
        // the best fit for it, the 6 000 MiB task, is marked.
        let over = evaluate(&mut engine, &cfg, 0, 500);
        assert_eq!(
            over.oom_preferred,
            std::collections::BTreeSet::from(["b".to_string()])
        );
        let back = evaluate(&mut engine, &cfg, 10_000, 1_000);
        assert!(back.oom_preferred.is_empty(), "{:?}", back.oom_preferred);
    }

    /// An estimate between PSS passes never marks or steps own work by itself (FR-085, Q16): a
    /// new mark on an estimated allocation makes the agent read it fresh and evaluate again
    /// from the policy state before the interval. A fresh reading back within the share marks
    /// nothing, one that confirms it marks, and a mark held unchanged reads nothing new.
    #[test]
    fn a_new_memory_mark_on_an_estimate_is_read_fresh_first() {
        use std::cell::Cell;
        // In Performance own memory within its share is not given up for others' growth.
        let mut cfg = config();
        cfg.contention.default_mode = cedegrid::contention::ContentionMode::Performance;
        let estimated = std::collections::BTreeSet::from(["b".to_string()]);
        let reads = Cell::new(0);
        let interval = |engine: &mut PolicyEngine, available, now, fresh: Option<u64>| {
            let mut snap = snapshot();
            snap.available_ram_mib = Some(available);
            let mut input = PolicyInput {
                allocations: vec![
                    allocation(
                        "a",
                        AllocationPhase::Running,
                        resources(100, 10_000, 0),
                        Some(resources(100, 10_000, 0)),
                    ),
                    // Requested 4 000 MiB, so its judged memory follows the reading.
                    allocation(
                        "b",
                        AllocationPhase::Running,
                        resources(100, 4_000, 0),
                        Some(resources(100, 6_000, 0)),
                    ),
                ],
                facts: rootless(&["a", "b"]),
                ..PolicyInput::default()
            };
            let decision = cedegrid::agent::confirm_memory_steps(
                engine,
                &cfg,
                &snap,
                &mut input,
                now,
                &estimated,
                |runs| {
                    reads.set(reads.get() + 1);
                    runs.iter().map(|run| (run.clone(), fresh)).collect()
                },
            )
            .unwrap();
            (decision, input.allocations[1].observed.clone())
        };
        // The estimate says 6 000 MiB, over the share once external use grows; fresh says
        // 4 000 MiB, within it.
        let mut engine = PolicyEngine::new();
        let (calm, _) = interval(&mut engine, 10_000, 0, Some(4_000));
        assert!(calm.oom_preferred.is_empty());
        assert_eq!(reads.get(), 0, "nothing new to confirm");
        let (refuted, observed) = interval(&mut engine, 0, 500, Some(4_000));
        assert_eq!(reads.get(), 1);
        assert!(
            refuted.oom_preferred.is_empty(),
            "{:?}",
            refuted.oom_preferred
        );
        let ram_steps = |decision: &Decision| -> Vec<String> {
            decision
                .actions
                .iter()
                .filter(|a| a.resource == cedegrid::contention::ResourceKey::Ram)
                .map(|a| a.allocation_id.clone())
                .collect()
        };
        assert!(ram_steps(&refuted).is_empty(), "{:?}", refuted.actions);
        assert_eq!(observed.map(|o| o.ram_mib), Some(4_000));
        // Fresh confirms the estimate: the mark stands, and held unchanged it reads nothing.
        let mut engine = PolicyEngine::new();
        interval(&mut engine, 10_000, 0, Some(6_000));
        let (confirmed, _) = interval(&mut engine, 0, 500, Some(6_000));
        assert_eq!(reads.get(), 2);
        assert_eq!(
            confirmed.oom_preferred,
            std::collections::BTreeSet::from(["b".to_string()])
        );
        assert_eq!(ram_steps(&confirmed), vec!["b".to_string()]);
        let (held, _) = interval(&mut engine, 0, 1_000, Some(6_000));
        assert_eq!(held.oom_preferred, confirmed.oom_preferred);
        assert_eq!(reads.get(), 2, "a held mark needs no new read");
    }

    /// `ram.prefer_own_oom_victim` marks every own task, within its share too (FR-023).
    #[test]
    fn the_opt_in_marks_every_own_task() {
        let mut cfg = config();
        cfg.ram.prefer_own_oom_victim = true;
        let decision = evaluate(&mut PolicyEngine::new(), &cfg, 10_000, 0);
        assert_eq!(
            decision.oom_preferred,
            std::collections::BTreeSet::from(["a".to_string(), "b".to_string()])
        );
    }
}

/// Floors by mode (FR-023, FR-024, FR-063, FR-065): in Performance a floor breach steps own
/// memory only down to its fair share; Yield First and Neutral keep the floors first.
mod floors_by_mode {
    use super::*;
    use cedegrid::contention::{ContentionMode, ResourceKey, YieldStep};

    fn config_in(mode: ContentionMode) -> Config {
        let mut cfg = config();
        cfg.contention.default_mode = mode;
        cfg
    }

    /// Facts for workers that take a hint and observe the drain file, without a cap.
    fn hinted(ids: &[&str]) -> BTreeMap<String, AllocationFacts> {
        ids.iter()
            .map(|id| {
                let mut facts = AllocationFacts {
                    hint_ready: true,
                    observes_drain: true,
                    ..AllocationFacts::default()
                };
                facts
                    .unavailable
                    .insert(ResourceKey::Ram, [YieldStep::Cap].into());
                (id.to_string(), facts)
            })
            .collect()
    }

    fn ram_job(id: &str, class: AllocationClass, mib: u64) -> Allocation {
        let mut job = allocation(
            id,
            AllocationPhase::Running,
            resources(100, mib, 0),
            Some(resources(100, mib, 0)),
        );
        job.class = class;
        job
    }

    /// Every RAM action over `until_ms`, with external usage leaving `available` MiB free
    /// on 32 000 MiB with a 1 000 MiB floor.
    fn ram_actions(
        cfg: &Config,
        jobs: &[Allocation],
        facts: BTreeMap<String, AllocationFacts>,
        available: u64,
        until_ms: u64,
    ) -> Vec<(u64, YieldAction)> {
        let mut engine = PolicyEngine::new();
        let mut snap = snapshot();
        snap.available_ram_mib = Some(available);
        let input = PolicyInput {
            allocations: jobs.to_vec(),
            facts,
            ..PolicyInput::default()
        };
        let mut actions = Vec::new();
        for now in (0..=until_ms).step_by(500) {
            let decision = engine.evaluate(cfg, &snap, &input, now).unwrap();
            assert!(
                codes(&decision, "ram").contains(&"floor_breach".to_string()),
                "at {now}: {:?}",
                decision.resources
            );
            actions.extend(
                decision
                    .actions
                    .into_iter()
                    .filter(|a| a.resource == cedegrid::contention::ResourceKey::Ram)
                    .map(|a| (now, a)),
            );
        }
        actions
    }

    /// Own work holds 14 000 MiB, within its 15 500 MiB fair share, and external growth to
    /// 17 500 MiB or more breaches the floor. In Performance nothing of own work is hinted,
    /// drained or sent TERM, Guaranteed or not, on the critical path too; Yield First and
    /// Neutral step it.
    #[test]
    fn in_share_own_memory_keeps_its_place_for_a_breach_others_caused() {
        for class in [AllocationClass::Opportunistic, AllocationClass::Guaranteed] {
            let jobs = [ram_job("a", class, 8_000), ram_job("b", class, 6_000)];
            // 500 MiB free is a breach; 0 MiB free is critical (FR-063).
            for available in [500, 0] {
                for facts in [hinted(&["a", "b"]), rootless(&["a", "b"])] {
                    let kept = ram_actions(
                        &config_in(ContentionMode::Performance),
                        &jobs,
                        facts.clone(),
                        available,
                        30_000,
                    );
                    assert!(kept.is_empty(), "{class:?} at {available}: {kept:?}");
                    for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
                        let stepped =
                            ram_actions(&config_in(mode), &jobs, facts.clone(), available, 30_000);
                        assert!(
                            !stepped.is_empty(),
                            "{mode:?} {class:?} at {available} steps own memory"
                        );
                    }
                }
            }
        }
    }

    /// RAM is held while one allocation has no observation, and external growth then breaches
    /// the floor: the last known fair share stays in force, so in Performance in-share own
    /// memory is neither hinted nor drained, and the in-share tasks are asked once to save
    /// their progress in every mode (FR-023, FR-075).
    #[test]
    fn a_held_interval_keeps_the_last_fair_share_for_a_breach_others_caused() {
        use std::collections::BTreeSet;
        let observed = [
            ram_job("a", AllocationClass::Opportunistic, 8_000),
            ram_job("b", AllocationClass::Opportunistic, 6_000),
            ram_job("c", AllocationClass::Opportunistic, 100),
        ];
        let mut unobserved = observed.clone();
        unobserved[2].observed = None;
        let run = |mode: ContentionMode, available: u64| {
            let cfg = config_in(mode);
            let mut engine = PolicyEngine::new();
            let input = |jobs: &[Allocation]| PolicyInput {
                allocations: jobs.to_vec(),
                facts: hinted(&["a", "b", "c"]),
                ..PolicyInput::default()
            };
            let mut snap = snapshot();
            snap.available_ram_mib = Some(10_000);
            let first = engine.evaluate(&cfg, &snap, &input(&observed), 0).unwrap();
            assert!(first.save_progress.is_empty());
            snap.available_ram_mib = Some(available);
            let held = input(&unobserved);
            let mut actions = Vec::new();
            let mut asked = Vec::new();
            for now in (500..=30_000).step_by(500) {
                let decision = engine.evaluate(&cfg, &snap, &held, now).unwrap();
                assert!(
                    codes(&decision, "ram").contains(&"allocation_unknown_hold".to_string()),
                    "at {now}"
                );
                actions.extend(
                    decision
                        .actions
                        .into_iter()
                        .filter(|a| a.resource == cedegrid::contention::ResourceKey::Ram)
                        .map(|a| (now, a)),
                );
                if !decision.save_progress.is_empty() {
                    asked.push((now, decision.save_progress));
                }
            }
            (actions, asked)
        };
        let all = BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()]);
        for available in [500, 0] {
            let (kept, asked) = run(ContentionMode::Performance, available);
            assert!(
                kept.is_empty(),
                "at {available}: {:?}",
                &kept[..kept.len().min(3)]
            );
            assert_eq!(asked, [(500, all.clone())], "at {available}");
            let (stepped, asked) = run(ContentionMode::Neutral, available);
            assert!(
                !stepped.is_empty(),
                "Neutral at {available} steps own memory"
            );
            assert_eq!(asked, [(500, all.clone())], "Neutral at {available}");
        }
    }

    /// Own work holds 15 800 MiB beside 15 600 MiB external, 300 MiB above its 15 500 MiB fair
    /// share, and the floor needs 400 MiB. Performance hints the 300 MiB and no more; Neutral
    /// hints all 400 MiB.
    #[test]
    fn own_memory_above_its_fair_share_steps_down_to_it_and_no_further() {
        let jobs = [
            ram_job("a", AllocationClass::Opportunistic, 9_800),
            ram_job("b", AllocationClass::Opportunistic, 6_000),
        ];
        let first = |mode| {
            ram_actions(&config_in(mode), &jobs, hinted(&["a", "b"]), 600, 0)
                .into_iter()
                .map(|(_, a)| (a.allocation_id, a.step, a.target))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            first(ContentionMode::Performance),
            [("b".to_string(), YieldStep::Hint, Some(5_700))]
        );
        assert_eq!(
            first(ContentionMode::Neutral),
            [("b".to_string(), YieldStep::Hint, Some(5_600))]
        );
    }

    /// GPU memory: own work's 6 000 MiB is within its fair share of the device beside
    /// 9 500 MiB external, which breaches the 1 000 MiB floor. Performance leaves own work
    /// alone; Neutral steps it.
    #[test]
    fn in_share_gpu_memory_keeps_its_place_for_a_breach_others_caused() {
        let job = allocation(
            "job",
            AllocationPhase::Running,
            resources(100, 1_000, 6_000),
            Some(resources(100, 1_000, 6_000)),
        );
        let facts = BTreeMap::from([(
            "job".to_string(),
            AllocationFacts {
                hint_ready: true,
                observes_drain: true,
                ..AllocationFacts::default()
            },
        )]);
        let gpu_actions = |mode| {
            let cfg = config_in(mode);
            let mut engine = PolicyEngine::new();
            let input = PolicyInput {
                allocations: vec![job.clone()],
                facts: facts.clone(),
                ..PolicyInput::default()
            };
            let mut actions = Vec::new();
            for now in (0..=20_000).step_by(500) {
                let mut snap = gpu_snapshot();
                snap.gpus[0].utilization_percent = Some(0);
                snap.gpus[0].external_process_ids = Some(vec![4242]);
                // External usage grows from 3 000 to 9 500 MiB after the first second.
                snap.gpus[0].used_memory_mib = Some(if now < 1_000 { 9_000 } else { 15_500 });
                let decision = engine.evaluate(&cfg, &snap, &input, now).unwrap();
                actions.extend(
                    decision
                        .actions
                        .into_iter()
                        .filter(|a| a.resource == ResourceKey::GpuMemory("GPU-test".into())),
                );
            }
            actions
        };
        let kept = gpu_actions(ContentionMode::Performance);
        assert!(kept.is_empty(), "{kept:?}");
        assert!(!gpu_actions(ContentionMode::Neutral).is_empty());
    }

    /// While the floor is breached, RAM admission stays closed in every mode, so a task
    /// requeued after an out-of-memory kill starts again only once free memory is back
    /// above the floor (FR-076).
    #[test]
    fn ram_admission_stays_closed_through_the_breach() {
        let jobs = [ram_job("a", AllocationClass::Opportunistic, 8_000)];
        for mode in ContentionMode::ALL {
            let mut snap = snapshot();
            let input = PolicyInput {
                allocations: jobs.to_vec(),
                facts: hinted(&["a"]),
                ..PolicyInput::default()
            };
            let mut engine = PolicyEngine::new();
            for (now, available) in [(0, 500), (500, 0), (1_000, 500)] {
                snap.available_ram_mib = Some(available);
                let decision = engine
                    .evaluate(&config_in(mode), &snap, &input, now)
                    .unwrap();
                assert_ne!(
                    decision.resources[&ResourceKey::Ram].admission,
                    Some(cedegrid::contention::Admission::Open),
                    "{mode:?} at {now}"
                );
                assert_eq!(decision.admission_headroom.ram_mib, 0, "{mode:?} at {now}");
            }
        }
    }

    /// What own work keeps of RAM after this interval's steps.
    fn kept(cfg: &Config, jobs: &[Allocation], available: u64) -> u64 {
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        let total: u64 = jobs.iter().map(|j| j.requested.ram_mib).sum();
        let freed: u64 = ram_actions(cfg, jobs, hinted(&ids), available, 0)
            .into_iter()
            .map(|(_, action)| {
                let amount = jobs
                    .iter()
                    .find(|j| j.id == action.allocation_id)
                    .map_or(0, |j| j.requested.ram_mib);
                match action.step {
                    YieldStep::Hint | YieldStep::Cap => {
                        amount.saturating_sub(action.target.unwrap_or(amount))
                    }
                    _ => amount,
                }
            })
            .sum();
        total.saturating_sub(freed)
    }

    /// Own memory in Yield First ≤ Neutral ≤ Performance for every breach (FR-065).
    #[test]
    fn own_memory_keeps_the_mode_order_through_a_breach() {
        for a in (2_000..=14_000).step_by(3_000) {
            for b in (1_000..=7_000).step_by(2_000) {
                let jobs = [
                    ram_job("a", AllocationClass::Opportunistic, a),
                    ram_job("b", AllocationClass::Opportunistic, b),
                ];
                for available in [0, 300, 700] {
                    if a + b > 32_000 - available {
                        continue;
                    }
                    let [yield_first, neutral, performance] = [
                        ContentionMode::YieldFirst,
                        ContentionMode::Neutral,
                        ContentionMode::Performance,
                    ]
                    .map(|mode| kept(&config_in(mode), &jobs, available));
                    assert!(
                        yield_first <= neutral && neutral <= performance,
                        "a {a} b {b} free {available}: {yield_first} {neutral} {performance}"
                    );
                }
            }
        }
    }
}

/// The save-progress signal (FR-075): in-share own tasks are listed once per floor-breach
/// episode others' growth caused; an own-fault breach lists nobody.
mod save_progress {
    use super::*;
    use cedegrid::contention::ContentionMode;
    use std::collections::BTreeSet;

    fn listed(mode: ContentionMode, own: [u64; 2], free: &[u64]) -> Vec<BTreeSet<String>> {
        let mut cfg = config();
        cfg.contention.default_mode = mode;
        let mut engine = PolicyEngine::new();
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "a",
                    AllocationPhase::Running,
                    resources(100, own[0], 0),
                    Some(resources(100, own[0], 0)),
                ),
                allocation(
                    "b",
                    AllocationPhase::Running,
                    resources(100, own[1], 0),
                    Some(resources(100, own[1], 0)),
                ),
            ],
            facts: rootless(&["a", "b"]),
            ..PolicyInput::default()
        };
        free.iter()
            .enumerate()
            .map(|(i, available)| {
                let mut snap = snapshot();
                snap.available_ram_mib = Some(*available);
                engine
                    .evaluate(&cfg, &snap, &input, i as u64 * 500)
                    .unwrap()
                    .save_progress
            })
            .collect()
    }

    #[test]
    fn in_share_tasks_are_listed_once_per_episode_others_caused() {
        let both = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let none = BTreeSet::new();
        for mode in [ContentionMode::Performance, ContentionMode::Neutral] {
            // 14 000 MiB own is within its 15 500 MiB share; 17 500 MiB external breaches the
            // floor twice, with a calm interval between.
            assert_eq!(
                listed(
                    mode,
                    [8_000, 6_000],
                    &[10_000, 500, 500, 500, 10_000, 500, 500]
                ),
                [&none, &both, &none, &none, &none, &both, &none].map(Clone::clone),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn an_own_fault_breach_lists_nobody() {
        // 16 000 MiB own is above its 15 500 MiB share beside 16 000 MiB external.
        for listed in listed(ContentionMode::Performance, [10_000, 6_000], &[0, 0, 0]) {
            assert!(listed.is_empty(), "{listed:?}");
        }
    }
}

include!("support/state_dir.rs");
