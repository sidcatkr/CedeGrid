use proptest::prelude::*;
use resource_manager::{
    config::*,
    execution_model::AllocationClass,
    kernel::{CpuCore, CpuTopology, CpuUsageStatus, KernelSnapshot},
    model::*,
    policy::PolicyEngine,
};
use std::collections::BTreeMap;

fn config() -> Config {
    let mut result = Config::default();
    result.cpu.reserve_physical_cores = 1;
    result.ram.reserve_mib = 1000;
    result.ram.reserve_percent = 0;
    result.gpu.reserve_vram_mib = 1000;
    result.gpu.scale_up_cooldown_ms = 0;
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
        total_ram_mib: 32_000,
        available_ram_mib: Some(26_000),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: None,
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
    assert_eq!(decision.managed_budget.cpu_millicores, 3000);
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
    });
    result
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
        let mut snap = gpu_snapshot();
        snap.gpus[0].used_memory_mib = Some(2000);
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
        Some(Resources::default()),
    );
    guaranteed.class = AllocationClass::Guaranteed;
    let input = PolicyInput {
        allocations: vec![
            guaranteed,
            allocation(
                "b-opportunistic",
                AllocationPhase::Running,
                resources(1000, 0, 0),
                Some(Resources::default()),
            ),
        ],
        ..Default::default()
    };
    let mut snap = snapshot();
    snap.cpu_busy_millicores = Some(5000);
    let decision = PolicyEngine::new()
        .evaluate(&config(), &snap, &input, 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 1000);
    assert_eq!(decision.would_drain, ["b-opportunistic"]);
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
fn configuration_load_validates_and_preserves_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "state_dir: state\nnode_mode: guaranteed\n").unwrap();
    let result = Config::load(&path).unwrap();
    assert_eq!(result.state_dir.to_str(), Some("state"));
    assert_eq!(result.node_mode, NodeMode::Guaranteed);
    std::fs::write(&path, "schema_version: 2000").unwrap();
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
    // CPU external = 2000 - 1000, SMT reserve = 2000, budget = 5000.
    assert_eq!(decision.managed_budget.cpu_millicores, 5000);
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
    // First external discovery is protective irrespective of utilization.
    let first = engine.evaluate(&config(), &snap, &input, 0).unwrap();
    assert_eq!(first.would_drain, ["worker"]);
    snap.gpus[0].compute_sample_id = Some(2);
    let second = engine.evaluate(&config(), &snap, &input, 1).unwrap();
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
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 3000);
    assert_eq!(decision.would_drain, ["worker"]);
    assert!(!decision.expansion_allowed);
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
fn unknown_external_activity_blocks_expansion_and_yields() {
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
    assert_eq!(decision.would_drain, ["worker"]);
    assert_eq!(decision.managed_budget.gpu_memory_mib["GPU-test"], 4500);
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
    assert_eq!(decision.would_drain, ["worker"]);
    assert!(
        decision
            .reasons
            .iter()
            .any(|reason| reason.contains("unknown managed usage"))
    );
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
        Some(resources(0, 0, 0)),
    );
    let running_b = allocation(
        "b",
        AllocationPhase::Running,
        resources(3000, 0, 0),
        Some(resources(0, 0, 0)),
    );
    let pending = allocation("z", AllocationPhase::Pending, resources(1000, 0, 0), None);
    let first = PolicyInput {
        allocations: vec![running_b.clone(), pending.clone(), running_a.clone()],
        ..PolicyInput::default()
    };
    let second = PolicyInput {
        allocations: vec![running_a, running_b, pending],
        ..PolicyInput::default()
    };
    let a = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &first, 0)
        .unwrap();
    let b = PolicyEngine::new()
        .evaluate(&config(), &snapshot(), &second, 0)
        .unwrap();
    assert_eq!(a.would_drain, ["z", "a"]);
    assert_eq!(a.would_drain, b.would_drain);
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
        prop_assert_eq!(decision.admission_headroom.cpu_millicores, budget - reserved.max(observed));
    }

    #[test]
    fn pending_admission_conserves_capacity(a in 0u64..2000, b in 0u64..2000) {
        let input = PolicyInput { allocations: vec![allocation("a", AllocationPhase::Pending, resources(a, 0, 0), None), allocation("b", AllocationPhase::Pending, resources(b, 0, 0), None)], ..PolicyInput::default() };
        let decision = PolicyEngine::new().evaluate(&config(), &snapshot(), &input, 0).unwrap();
        prop_assert_eq!(decision.admission_headroom.cpu_millicores + a + b, decision.managed_budget.cpu_millicores);
    }
}

#[test]
fn cpu_ram_stability_survives_gpu_uncertainty_without_reusing_retained_gpu_cap() {
    let mut cfg = config();
    cfg.gpu.scale_up_cooldown_ms = 137;
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
    cfg.gpu.execution_mode = resource_manager::config::GpuExecutionMode::ConservativeNonSharing;
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
            .evaluate(&cfg, &snap, &PolicyInput::default(), 276)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .is_empty()
    );
    assert!(
        policy
            .evaluate(&cfg, &snap, &PolicyInput::default(), 277)
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
        resource_manager::policy::schedulable_budget(&d).gpu_memory_mib["GPU-bad"],
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
    let offered = resource_manager::policy::schedulable_budget(&d);
    assert_eq!(offered.gpu_memory_mib["GPU-bad"], 0);
    assert!(offered.gpu_memory_mib["GPU-test"] > 0);
}

#[test]
fn explicit_sharing_needs_recent_process_capability_and_never_infers_from_utilization() {
    use resource_manager::config::GpuExecutionMode;
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
    observation.baseline_age_ms = Some(cfg.gpu.process_sample_max_age_ms + 1);
    snap.gpus[0].observation = Some(observation);
    assert!(
        PolicyEngine::new()
            .evaluate(&cfg, &snap, &PolicyInput::default(), 0)
            .unwrap()
            .admission_headroom
            .gpu_memory_mib
            .is_empty()
    );
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
    use resource_manager::config::GpuExecutionMode;
    use resource_manager::execution_model::AllocationClass;
    let mut cfg = config();
    let snap = gpu_snapshot();
    for mode in [
        GpuExecutionMode::Auto,
        GpuExecutionMode::ConservativeNonSharing,
    ] {
        cfg.gpu.execution_mode = mode;
        assert!(
            resource_manager::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 1),
                AllocationClass::Guaranteed
            )
            .is_err()
        );
        assert!(
            resource_manager::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 0),
                AllocationClass::Guaranteed
            )
            .is_ok()
        );
        assert!(
            resource_manager::policy::validate_gpu_launch_contract(
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
    let cfg: resource_manager::config::GpuConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(
        cfg.execution_mode,
        resource_manager::config::GpuExecutionMode::Auto
    );
    assert!(cfg.process_sample_max_age_ms > 0);
}

#[test]
fn launch_recheck_refuses_an_external_context_appearing_after_empty_device_offer() {
    use resource_manager::config::GpuExecutionMode;
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
            resource_manager::policy::validate_gpu_launch_contract(
                &cfg,
                &snap,
                &resources(1, 1, 500),
                AllocationClass::Opportunistic
            )
            .is_ok()
        );
        // Even a contradictory Idle flag cannot overrule the non-sharing inventory contract.
        snap.gpus[0].external_process_ids = Some(vec![987]);
        let error = resource_manager::policy::validate_gpu_launch_contract(
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

#[test]
fn launch_recheck_does_not_admit_new_work_during_fresh_external_compute_activity() {
    let cfg = config();
    let mut snap = gpu_snapshot();
    snap.gpus[0].external_process_ids = Some(vec![987]);
    snap.gpus[0].external_compute = ComputeActivity::Active;
    let mut observation = gpu_observation(&snap, GpuExecutionCapability::ContentionAware);
    observation.fresh_after_baseline = true;
    observation.newest_sample_timestamp_us = Some(101);
    snap.gpus[0].observation = Some(observation);
    let error = resource_manager::policy::validate_gpu_launch_contract(
        &cfg,
        &snap,
        &resources(1, 1, 500),
        AllocationClass::Opportunistic,
    )
    .unwrap_err();
    assert!(error.to_string().contains("active or unknown"));
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
        resource_manager::policy::effective_gpu_capability(
            &cfg,
            snap.observed_at_unix_ms,
            &snap.gpus[0]
        ),
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
            resource_manager::policy::effective_gpu_capability(
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
    use resource_manager::policy::validate_gpu_launch_contract;
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
    snap.gpus[0].used_memory_mib = Some(9000);
    let growth = engine.evaluate(&cfg, &snap, &input, 103).unwrap();
    assert!(growth.admission_headroom.gpu_memory_mib.is_empty());
    assert!(!growth.would_drain.is_empty());
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
    assert!(unknown_own.managed_budget.gpu_memory_mib["GPU-test"] < 3500);
    assert!(
        unknown_own
            .reasons
            .iter()
            .any(|r| r.contains("unknown managed usage"))
    );
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
