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
    // These accounting tests keep a 10% Neutral margin; the default margins are
    // tested in tests/contention.rs.
    result.contention.cpu.neutral_margin_percent = Some(10);
    // A 1000 MiB floor is near the 512 MiB minimum; the tests confirm it.
    result
        .contention
        .acknowledge_caution
        .insert("small_floor".into());
    result.gpu.reserve_vram_mib = 1000;
    result.gpu.reserve_vram_percent = 0;
    result.gpu.scale_up_cooldown_ms = 0;
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
        total_ram_mib: 32_000,
        available_ram_mib: Some(26_000),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: None,
        principals: None,
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
            visible_cpu_ceiling_millicores: Some(4000),
            detail: "synthetic scoped telemetry".into(),
        },
        hierarchy: None,
        psi: vec![],
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
    assert_eq!(decision.managed_budget.cpu_millicores, 1700);
    assert!(decision.expansion_allowed);
    // A missing host aggregate does not invalidate independently complete scope telemetry.
    snap.cpu_busy_millicores = None;
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 1700);
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
    assert_eq!(decision.managed_budget.cpu_millicores, 2450);
    assert_eq!(decision.admission_headroom.cpu_millicores, 1450);
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
    assert_eq!(decision.managed_budget.cpu_millicores, 900);
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
        3400
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
        3400
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
    });
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
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snap, &input, 0)
            .unwrap();
        assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 1500);
        assert_eq!(decision.would_drain, ["b-opportunistic"]);
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
    assert_eq!(decision.managed_budget.cpu_millicores, 400);
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
    assert_eq!(decision.admission_headroom.cpu_millicores, 2400);
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
    // CPU external = 2000 - 1000, SMT reserve = 2000, Neutral margin = 10% of 6000, budget = 4400.
    assert_eq!(decision.managed_budget.cpu_millicores, 4400);
    assert_eq!(decision.admission_headroom.cpu_millicores, 1400);
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
    assert_eq!(decision.admission_headroom.cpu_millicores, 3400);
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
    assert_eq!(decision.admission_headroom.cpu_millicores, 1900);
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
    assert_eq!(decision.admission_headroom.cpu_millicores, 1400);
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
    assert!(!decision.expansion_allowed);
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
    // Activity, not total utilization, decides: GPU admission is blocked. Without a
    // memory breach the running work is not drained; GPU compute steps come later.
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 14_000);
    assert!(decision.admission_headroom.gpu_memory_mib.is_empty());
    assert!(decision.would_drain.is_empty());
    assert!(!decision.expansion_allowed);
    assert!(
        decision
            .reasons
            .iter()
            .any(|r| r.contains("external compute activity"))
    );
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
    let mut engine = PolicyEngine::new();
    let first = engine.evaluate(&config(), &snap, &input, 0).unwrap();
    let second = engine.evaluate(&config(), &snap, &input, 500).unwrap();
    assert_eq!(
        first.managed_budget.gpu_memory_mib,
        second.managed_budget.gpu_memory_mib
    );
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    let stale_idle = engine.evaluate(&config(), &snap, &input, 90_000).unwrap();
    assert!(!stale_idle.expansion_allowed);
}

#[test]
fn out_of_order_compute_sample_cannot_prove_recovery() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].compute_sample_id = Some(30);
    snap.gpus[0].external_process_ids = Some(vec![321]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let mut engine = PolicyEngine::new();
    engine
        .evaluate(&config(), &snap, &PolicyInput::default(), 0)
        .unwrap();
    snap.gpus[0].compute_sample_id = Some(29);
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    assert!(
        !engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 1)
            .unwrap()
            .expansion_allowed
    );
    snap.gpus[0].compute_sample_id = Some(30);
    assert!(
        !engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 2)
            .unwrap()
            .expansion_allowed
    );
    snap.gpus[0].compute_sample_id = Some(31);
    assert!(
        engine
            .evaluate(&config(), &snap, &PolicyInput::default(), 3)
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
        .evaluate(&config(), &snap, &input, 0)
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
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &input, 0)
        .unwrap();
    assert!(!decision.expansion_allowed);
    assert_eq!(decision.would_drain, ["gpu-worker"]);
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
    config.gpu.scale_up_cooldown_ms = 137;
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
    cfg.gpu.scale_up_cooldown_ms = 100;
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
            ("a", YieldStep::Cap, Some(1400))
        ]
        .into()
    );
    assert_eq!(a.admission_headroom, Resources::default());
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
        let budget = 6000 - 600 - external;
        prop_assert_eq!(decision.managed_budget.cpu_millicores, budget);
        prop_assert_eq!(decision.admission_headroom.cpu_millicores, budget - reserved.max(observed));
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
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = 137;
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
        assert_eq!(decision.admission_headroom.cpu_millicores, 0);
        assert_eq!(decision.admission_headroom.ram_mib, 0);
    }
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = 137;
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
fn non_sharing_admits_empty_then_fully_yields_and_recovers_after_confirmed_release() {
    let mut cfg = config();
    cfg.gpu.execution_mode = cedegrid::config::GpuExecutionMode::ConservativeNonSharing;
    cfg.gpu.scale_up_cooldown_ms = 137;
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    snap.gpus[0].compute_sample_id = None;
    snap.gpus[0].observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::ConservativeNonSharing,
    ));
    let mut policy = PolicyEngine::new();
    let first = policy
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert!(!first.expansion_allowed);
    let ready = policy
        .evaluate(&cfg, &snap, &PolicyInput::default(), 137)
        .unwrap();
    assert!(ready.expansion_allowed);
    assert!(ready.admission_headroom.gpu_memory_mib["GPU-test"] > 0);
    let mut input = PolicyInput {
        allocations: vec![
            allocation(
                "a",
                AllocationPhase::Running,
                resources(100, 100, 2000),
                Some(resources(100, 100, 2000)),
            ),
            allocation(
                "b",
                AllocationPhase::Running,
                resources(100, 100, 2000),
                Some(resources(100, 100, 2000)),
            ),
        ],
        ..Default::default()
    };
    snap.gpus[0].used_memory_mib = Some(5000);
    snap.gpus[0].external_process_ids = Some(vec![999]);
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    let yielding = policy.evaluate(&cfg, &snap, &input, 138).unwrap();
    assert_eq!(yielding.would_drain, ["a", "b"]);
    assert_eq!(yielding.managed_budget.gpu_memory_mib["GPU-test"], 0);
    assert!(yielding.admission_headroom.gpu_memory_mib.is_empty());
    for allocation in &mut input.allocations {
        allocation.phase = AllocationPhase::Draining;
    }
    let draining = policy.evaluate(&cfg, &snap, &input, 139).unwrap();
    assert!(draining.admission_headroom.gpu_memory_mib.is_empty());
    snap.gpus[0].used_memory_mib = Some(0);
    snap.gpus[0].external_process_ids = Some(vec![]);
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    let released = policy
        .evaluate(&cfg, &snap, &PolicyInput::default(), 140)
        .unwrap();
    assert!(released.admission_headroom.gpu_memory_mib.is_empty());
    assert!(
        policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 1276)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .is_empty(),
        "the cooldown plus 1 s of reclaim backoff (FR-061)"
    );
    assert!(
        policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 1277)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib["GPU-test"]
            > 0
    );
}

#[test]
fn auto_unknown_external_activity_uses_non_sharing_full_yield() {
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![77]);
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    snap.gpus[0].compute_sample_id = None;
    snap.gpus[0].observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::ConservativeNonSharing,
    ));
    let input = PolicyInput {
        allocations: vec![allocation(
            "worker",
            AllocationPhase::Running,
            resources(100, 100, 6000),
            Some(resources(100, 100, 6000)),
        )],
        ..Default::default()
    };
    let d = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(d.would_drain, ["worker"]);
    assert_eq!(d.managed_budget.gpu_memory_mib["GPU-test"], 0);
    assert!(d.admission_headroom.gpu_memory_mib.is_empty());
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
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert!(d.managed_budget.gpu_memory_mib["GPU-bad"] > 0);
    let offered = cedegrid::policy::schedulable_budget(&d);
    assert_eq!(offered.gpu_memory_mib["GPU-bad"], 0);
    assert!(offered.gpu_memory_mib["GPU-test"] > 0);
}

#[test]
fn explicit_sharing_needs_healthy_process_observation_and_never_infers_from_utilization() {
    use cedegrid::config::GpuExecutionMode;
    let mut cfg = config();
    cfg.gpu.execution_mode = GpuExecutionMode::ContentionAware;
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    for utilization in [Some(0), Some(100), None] {
        snap.gpus[0].utilization_percent = utilization;
        let d = PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap();
        assert!(d.admission_headroom.gpu_memory_mib.is_empty());
    }
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ContentionAware);
    observation.activity_api_status = "NVML_SUCCESS".into();
    observation.fresh_after_baseline = true;
    observation.newest_sample_timestamp_us = Some(101);
    snap.gpus[0].observation = Some(observation.clone());
    assert!(
        PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib["GPU-test"]
            > 0
    );
    // Health does not depend on fresh samples: an empty, idle device stays
    // admissible when its cursor ages out (research R8)...
    observation.baseline_age_ms = Some(cfg.gpu.process_sample_max_age_ms + 1);
    observation.fresh_after_baseline = false;
    snap.gpus[0].observation = Some(observation);
    assert!(
        PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib["GPU-test"]
            > 0
    );
    // ...while an external context with unknown activity blocks admission.
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    assert!(
        PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .is_empty()
    );
    // Without healthy observation the explicit mode yields fully.
    snap.gpus[0].observation.as_mut().unwrap().capability =
        GpuExecutionCapability::ConservativeNonSharing;
    snap.gpus[0].external_process_ids = Some(vec![]);
    snap.gpus[0].external_compute = ComputeActivity::Idle;
    let d = PolicyEngine::new()
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert!(d.admission_headroom.gpu_memory_mib.is_empty());
    assert_eq!(d.managed_budget.gpu_memory_mib["GPU-test"], 0);
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
fn guaranteed_gpu_rejects_conflicting_non_sharing_contract_before_launch() {
    use cedegrid::config::GpuExecutionMode;
    use cedegrid::execution_model::AllocationClass;
    let mut cfg = config();
    let snap = gpu_snapshot();
    for mode in [
        GpuExecutionMode::Auto,
        GpuExecutionMode::ConservativeNonSharing,
    ] {
        cfg.gpu.execution_mode = mode;
        assert!(
            cedegrid::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 1),
                AllocationClass::Guaranteed
            )
            .is_err()
        );
        assert!(
            cedegrid::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 0),
                AllocationClass::Guaranteed
            )
            .is_ok()
        );
        assert!(
            cedegrid::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 1),
                AllocationClass::Opportunistic
            )
            .is_ok()
        );
    }
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
    assert!(cfg.process_sample_max_age_ms > 0);
}

#[test]
fn launch_recheck_refuses_an_external_context_appearing_after_empty_device_offer() {
    use cedegrid::config::GpuExecutionMode;
    let mut cfg = config();
    let mut snap = gpu_snapshot();
    snap.gpus[0].used_memory_mib = Some(0);
    snap.gpus[0].observation = Some(gpu_observation(
        &snap,
        GpuExecutionCapability::ConservativeNonSharing,
    ));
    for mode in [
        GpuExecutionMode::Auto,
        GpuExecutionMode::ConservativeNonSharing,
    ] {
        cfg.gpu.execution_mode = mode;
        snap.gpus[0].external_process_ids = Some(vec![]);
        assert!(
            cedegrid::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 500),
                AllocationClass::Opportunistic
            )
            .is_ok()
        );
        // Even a contradictory Idle flag cannot overrule the non-sharing inventory contract.
        snap.gpus[0].external_process_ids = Some(vec![987]);
        let error = cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(1, 1, 500),
            AllocationClass::Opportunistic,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("complete empty external inventory")
        );
    }
}

/// Pinned until US5 floors protect external usage; the US5 task that adds those
/// floors inverts this test for Neutral and Performance.
#[test]
fn launch_recheck_blocks_active_external_compute_in_every_mode_until_floors_exist() {
    use cedegrid::contention::ContentionMode;
    let mut cfg = config();
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ContentionAware);
    observation.fresh_after_baseline = true;
    observation.newest_sample_timestamp_us = Some(101);
    snap.gpus[0].observation = Some(observation);
    for mode in [
        ContentionMode::YieldFirst,
        ContentionMode::Neutral,
        ContentionMode::Performance,
    ] {
        cfg.contention.default_mode = mode;
        let error = cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(1, 1, 500),
            AllocationClass::Opportunistic,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("external activity Active"),
            "{mode:?}: {error}"
        );
    }
    // Quiet stays admitted in every mode.
    snap.gpus[0].external_compute = ComputeActivity::Quiet;
    for mode in [
        ContentionMode::YieldFirst,
        ContentionMode::Neutral,
        ContentionMode::Performance,
    ] {
        cfg.contention.default_mode = mode;
        cedegrid::policy::validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(1, 1, 500),
            AllocationClass::Opportunistic,
        )
        .unwrap();
    }
}

fn best_effort_fixture() -> (Config, Snapshot) {
    let mut cfg = config();
    cfg.gpu.execution_mode = GpuExecutionMode::BestEffortOccupied;
    cfg.gpu.scale_up_cooldown_ms = 100;
    let identity = ExternalGpuProcessIdentity {
        pid: 4242,
        boot_id: "fixture-boot".into(),
        start_ticks: 87654,
        uid: 1000,
    };
    cfg.gpu
        .best_effort_external_processes
        .insert("GPU-test".into(), vec![identity.clone()]);
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![identity.pid]);
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
    snap.gpus[0].compute_sample_id = None;
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ConservativeNonSharing);
    observation.activity_api_status = "NotSupported".into();
    observation.best_effort_external_processes = Some(vec![identity]);
    snap.gpus[0].observation = Some(observation);
    (cfg, snap)
}

#[test]
fn best_effort_occupied_is_explicit_and_unknown_activity_remains_unknown() {
    let (mut cfg, snap) = best_effort_fixture();
    assert_eq!(
        cedegrid::policy::effective_gpu_capability(&cfg, snap.observed_at_unix_ms, &snap.gpus[0]),
        Some(GpuExecutionCapability::BestEffortOccupied)
    );
    let mut engine = PolicyEngine::new();
    let first = engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    assert!(first.admission_headroom.gpu_memory_mib.is_empty());
    engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 1)
        .unwrap();
    let ready = engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 101)
        .unwrap();
    assert_eq!(ready.admission_headroom.gpu_memory_mib["GPU-test"], 9000);
    assert_eq!(snap.gpus[0].external_compute, ComputeActivity::Unknown);
    assert!(
        ready
            .reasons
            .iter()
            .any(|r| r.contains("interference and slowdown are unmeasured"))
    );
    assert!(ready.reasons.iter().any(|r| r.contains("remains Unknown")));
    for mode in [
        GpuExecutionMode::Auto,
        GpuExecutionMode::ConservativeNonSharing,
        GpuExecutionMode::ContentionAware,
    ] {
        cfg.gpu.execution_mode = mode;
        let mut engine = PolicyEngine::new();
        engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap();
        let strict = engine
            .evaluate(&cfg, &snap, &PolicyInput::default(), 1000)
            .unwrap();
        assert!(strict.admission_headroom.gpu_memory_mib.is_empty());
        assert_ne!(
            cedegrid::policy::effective_gpu_capability(
                &cfg,
                snap.observed_at_unix_ms,
                &snap.gpus[0]
            ),
            Some(GpuExecutionCapability::BestEffortOccupied)
        );
    }
}

#[test]
fn best_effort_launch_recheck_accepts_only_scoped_verified_opportunistic_contract() {
    use cedegrid::policy::validate_gpu_launch_contract;
    let (cfg, mut snap) = best_effort_fixture();
    for activity in [
        ComputeActivity::Unknown,
        ComputeActivity::Active,
        ComputeActivity::Idle,
    ] {
        snap.gpus[0].external_compute = activity;
        validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(0, 0, 500),
            AllocationClass::Opportunistic,
        )
        .unwrap();
        assert!(
            validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(0, 0, 500),
                AllocationClass::Guaranteed
            )
            .is_err()
        );
    }
    snap.gpus[0]
        .external_process_ids
        .as_mut()
        .unwrap()
        .push(4243);
    assert!(
        validate_gpu_launch_contract(
            &cfg,
            &snap,
            &resources(0, 0, 500),
            AllocationClass::Opportunistic
        )
        .is_err()
    );
}

#[test]
fn best_effort_identity_or_basic_telemetry_loss_fully_yields() {
    let (cfg, original) = best_effort_fixture();
    let input = PolicyInput {
        allocations: vec![allocation(
            "managed",
            AllocationPhase::Running,
            resources(0, 0, 2000),
            Some(resources(0, 0, 2000)),
        )],
        ..Default::default()
    };
    for case in 0..13 {
        let mut snap = original.clone();
        let gpu = &mut snap.gpus[0];
        match case {
            0 => {
                gpu.observation
                    .as_mut()
                    .unwrap()
                    .best_effort_external_processes = None
            }
            1 => {
                gpu.observation
                    .as_mut()
                    .unwrap()
                    .best_effort_external_processes
                    .as_mut()
                    .unwrap()[0]
                    .pid += 1
            }
            2 => {
                gpu.observation
                    .as_mut()
                    .unwrap()
                    .best_effort_external_processes
                    .as_mut()
                    .unwrap()[0]
                    .start_ticks += 1
            }
            3 => gpu
                .observation
                .as_mut()
                .unwrap()
                .best_effort_external_processes
                .as_mut()
                .unwrap()[0]
                .boot_id
                .push('x'),
            4 => {
                gpu.observation
                    .as_mut()
                    .unwrap()
                    .best_effort_external_processes
                    .as_mut()
                    .unwrap()[0]
                    .uid += 1
            }
            5 => gpu.external_process_ids.as_mut().unwrap().push(9999),
            6 => gpu.external_process_ids = None,
            7 => gpu.used_memory_mib = None,
            8 => {
                gpu.total_memory_mib = 0;
                gpu.used_memory_mib = None;
                gpu.observation.as_mut().unwrap().capability =
                    GpuExecutionCapability::InsufficientObservability;
            }
            9 => {
                gpu.observation.as_mut().unwrap().capability =
                    GpuExecutionCapability::InsufficientObservability
            }
            10 => gpu.observation.as_mut().unwrap().observed_at_unix_ms += 1,
            11 => gpu.uuid = "GPU-not-authorized".into(),
            12 => {
                gpu.observation
                    .as_mut()
                    .unwrap()
                    .best_effort_external_processes = Some(vec![])
            }
            _ => unreachable!(),
        }
        let decision = PolicyEngine::new()
            .evaluate(&cfg, &snap, &input, 0)
            .unwrap();
        assert_eq!(
            decision.managed_budget.gpu_memory_mib["GPU-test"], 0,
            "case {case}"
        );
        assert!(
            decision.admission_headroom.gpu_memory_mib.is_empty(),
            "case {case}"
        );
        assert_eq!(decision.would_drain, ["managed"], "case {case}");
    }
}

#[test]
fn best_effort_memory_growth_preserves_yield_reserves_and_retained_charges() {
    let (cfg, mut snap) = best_effort_fixture();
    let mut engine = PolicyEngine::new();
    engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
        .unwrap();
    engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 1)
        .unwrap();
    engine
        .evaluate(&cfg, &snap, &PolicyInput::default(), 101)
        .unwrap();
    // Own 2000 MiB is independently attributed; the competitor's 6000 MiB stays external.
    snap.gpus[0].used_memory_mib = Some(8000);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let mut input = PolicyInput {
        allocations: vec![
            allocation(
                "managed",
                AllocationPhase::Running,
                resources(0, 0, 3000),
                Some(resources(0, 0, 2000)),
            ),
            allocation(
                "pending",
                AllocationPhase::Pending,
                resources(0, 0, 500),
                None,
            ),
        ],
        ..Default::default()
    };
    let running = engine.evaluate(&cfg, &snap, &input, 102).unwrap();
    assert_eq!(running.managed_budget.gpu_memory_mib["GPU-test"], 9000);
    assert_eq!(running.admission_headroom.gpu_memory_mib["GPU-test"], 5500);
    assert!(running.would_drain.is_empty());
    // The competitor grows to 12000 MiB: 3000 MiB remain for 3500 MiB of charges.
    snap.gpus[0].used_memory_mib = Some(14_000);
    let growth = engine.evaluate(&cfg, &snap, &input, 103).unwrap();
    assert!(growth.admission_headroom.gpu_memory_mib.is_empty());
    assert_eq!(growth.would_drain, ["pending"]);
    assert!(
        growth
            .reasons
            .iter()
            .any(|r| r.contains("external memory growth"))
    );
    input.allocations[0].phase = AllocationPhase::Draining;
    let draining = engine.evaluate(&cfg, &snap, &input, 104).unwrap();
    assert!(draining.admission_headroom.gpu_memory_mib.is_empty());
    assert!(draining.managed_budget.gpu_memory_mib["GPU-test"] < 3500);
    input.allocations[0].observed = None;
    let unknown_own = engine.evaluate(&cfg, &snap, &input, 105).unwrap();
    assert!(unknown_own.admission_headroom.gpu_memory_mib.is_empty());
    // Charged at max(3000 request, 2000 last known) as own work, never external.
    assert_eq!(unknown_own.managed_budget.gpu_memory_mib["GPU-test"], 4000);
    assert!(codes(&unknown_own, "gpu_memory:GPU-test").contains(&"allocation_unknown_hold".into()));
}

#[test]
fn best_effort_mode_config_rejects_empty_and_partial_authorization() {
    let empty: GpuConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.execution_mode, GpuExecutionMode::Auto);
    assert!(empty.best_effort_external_processes.is_empty());
    let (mut cfg, _) = best_effort_fixture();
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

#[test]
fn d1_first_observation_is_a_baseline_not_external_growth() {
    // A fresh engine sees 300 MiB of driver/context memory on an otherwise empty
    // device before its own task has allocated anything.
    let mut input = PolicyInput {
        allocations: vec![allocation(
            "mine",
            AllocationPhase::Running,
            resources(0, 0, 4000),
            Some(resources(0, 0, 0)),
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
        Some(resources(0, 0, 0)),
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
    cfg.gpu.scale_up_cooldown_ms = 60_000;
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
        assert_eq!(
            cedegrid::policy::effective_gpu_capability(
                &cfg,
                snap.observed_at_unix_ms,
                &snap.gpus[0]
            ),
            Some(GpuExecutionCapability::ContentionAware)
        );
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
    snap.gpus[0].external_compute = ComputeActivity::Unknown;
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
    assert_eq!(alone.managed_budget.cpu_millicores, 5400);
    // Another workload starts using three cores.
    snap.cpu_busy_millicores = Some(7000);
    let contended = engine.evaluate(&config(), &snap, &input, 1000).unwrap();
    assert_eq!(contended.managed_budget.cpu_millicores, 2400);
    assert!(contended.would_drain.is_empty());
    assert_eq!(
        contended.actions,
        [YieldAction {
            allocation_id: "mine".into(),
            resource: cedegrid::contention::ResourceKey::Cpu,
            step: cedegrid::contention::YieldStep::Cap,
            target: Some(2400),
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
    // 8000 - 2000 reserve - (4100 host upper - 4000 own lower) - the 600 margin.
    assert_eq!(consistent.managed_budget.cpu_millicores, 5300);
    assert_eq!(consistent.admission_headroom.cpu_millicores, 1300);
    assert!(codes(&consistent, "cpu").contains(&"inconsistent_within_tolerance".into()));
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

/// CPU/RAM admission waits its own cooldown, whose default follows the mode's
/// scale-up interval; the GPU cooldown never gates it (audit M1).
#[test]
fn cpu_ram_expansion_follows_its_own_cooldown_not_the_gpu_one() {
    use cedegrid::contention::ContentionMode;
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = 60_000;
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
    // The least aggressive effective mode on a resource governs its target.
    let cpu = &decision.resources[&ResourceKey::Cpu];
    assert_eq!(cpu.mode, Some(ContentionMode::Neutral));
    assert_eq!(cpu.effective_mode, Some(ContentionMode::YieldFirst));
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
/// only after the principal window and leaves only after the same window below
/// half the threshold. Until it joins, its usage counts toward the unattributed
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
    let mut engine = PolicyEngine::new();
    // Saturated: own 4000m, root 1000m, two users. Schedulable capacity is 6000m.
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
        engine
            .evaluate(&config(), &snap, &own, now)
            .unwrap()
            .resources[&cedegrid::contention::ResourceKey::Cpu]
            .target_share
            .unwrap()
    };
    let owner = || cpu_principal(1000, true, 0);
    let both = [(2001, 1500), (2002, 1500)];
    // Root's 1000m comes off the capacity; the new users form one principal at first.
    for now in [0, 500, 1000] {
        assert_eq!(share(owner(), &both, now), 2500, "at {now}");
    }
    // After four observations each user is its own principal.
    // 5000m among three principals; the remainder goes to the owner.
    assert_eq!(share(owner(), &both, 1500), 1667);
    // A drop shorter than the 60 s leave window does not end membership.
    let dropped = [(2001, 0), (2002, 0)];
    let mut now = 2000;
    while now < 61_500 {
        share(owner(), &dropped, now);
        now += 500;
    }
    assert_eq!(share(owner(), &both, now), 1667);
    // 60 s below the threshold does: back to one pooled principal.
    now += 500;
    let left_at = now + 60_000;
    while now <= left_at {
        share(owner(), &dropped, now);
        now += 500;
    }
    assert_eq!(share(owner(), &both, now), 2500);
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
        decision = Some(engine.evaluate(&config(), &snap, &own, now).unwrap());
    }
    // Two principals share 6000m; the account's unmanaged 500m comes off its share.
    assert_eq!(
        decision.unwrap().resources[&cedegrid::contention::ResourceKey::Cpu].target_share,
        Some(2500)
    );
}

/// Admission headroom for memory uses the external peak over the peak window,
/// while yielding follows current usage (FR-060).
#[test]
fn memory_headroom_uses_the_external_peak_over_its_window() {
    let mut cfg = config();
    cfg.contention.ram.memory_peak_window_s = Some(10);
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

/// A quiet GPU context's memory counts toward the device's external peak.
#[test]
fn a_quiet_gpu_context_counts_toward_the_device_peak() {
    let mut cfg = config();
    cfg.contention.gpu_memory.memory_peak_window_s = Some(10);
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
    cfg.gpu.scale_up_cooldown_ms = 60_000;
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

/// Until GPU admission follows the modes, `never_share_occupied_gpus` uses the
/// existing non-sharing path, so the legacy key's replacement takes effect (FR-064).
#[test]
fn never_share_occupied_gpus_uses_the_non_sharing_path() {
    let mut cfg = config();
    let snap = large_gpu_snapshot(2048, vec![4242]);
    let auto =
        cedegrid::policy::effective_gpu_capability(&cfg, snap.observed_at_unix_ms, &snap.gpus[0]);
    assert_ne!(auto, Some(GpuExecutionCapability::ConservativeNonSharing));
    cfg.contention.never_share_occupied_gpus = true;
    assert_eq!(
        cedegrid::policy::effective_gpu_capability(&cfg, snap.observed_at_unix_ms, &snap.gpus[0]),
        Some(GpuExecutionCapability::ConservativeNonSharing)
    );
}
