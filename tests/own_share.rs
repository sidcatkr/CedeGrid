//! Own work keeps its fair share for admission while another principal keeps its own, and holds
//! it steady (user, 2026-11-17, option A). 004's host gate F2c found own work admitted to half
//! its share: CedeGrid's own CPU was charged to the share and a 1000 m task no longer fit.
use cedegrid::{
    config::Config,
    contention::{ContentionMode, ResourceKey},
    execution_model::AllocationClass,
    kernel::{
        CgroupHierarchy, CgroupLevel, CgroupMount, CpuCore, CpuTopology, CpuUsageStatus,
        KernelSnapshot,
    },
    model::*,
    policy::PolicyEngine,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

const OWNER: u32 = 1000;
const OTHER: u32 = 2001;

fn config(mode: ContentionMode) -> Config {
    let mut config = Config::default();
    config.cpu.reserve_physical_cores = 0;
    config.contention.default_mode = mode;
    if mode == ContentionMode::Aggressive {
        config.contention.max_mode = Some(ContentionMode::Aggressive);
        config
            .contention
            .acknowledge_caution
            .insert("aggressive_mode".into());
    }
    config
}

fn level(path: &str, cpu: bool) -> CgroupLevel {
    CgroupLevel {
        path: PathBuf::from(path),
        cgroup_type: Some("domain".into()),
        controllers: Some(if cpu {
            vec!["cpu".into(), "memory".into(), "pids".into()]
        } else {
            vec!["memory".into(), "pids".into()]
        }),
        subtree_control: Some(vec![]),
        cpu_max: None,
        cpu_weight: None,
        cpuset_effective: None,
        memory_max: None,
        memory_high: None,
        cpu_stat: None,
        memory_current: None,
        memory_events: None,
        errors: vec![],
    }
}

/// F2c on a four-CPU scope: one own task of 1000 m running, CedeGrid at `cedegrid` m, and the
/// other user at `other` m on two of the CPUs. `per_user` places the agent in the owner's own
/// CPU cgroup, as on a host that enables the `cpu` controller down to each user slice.
fn reading(other: u64, cedegrid: u64, per_user: Option<bool>) -> (Snapshot, PolicyInput) {
    let busy = 1000 + cedegrid + other;
    let hierarchy = per_user.map(|cpu| {
        let user = format!("/sys/fs/cgroup/user.slice/user-{OWNER}.slice");
        CgroupHierarchy {
            mount: CgroupMount {
                mount_point: PathBuf::from("/sys/fs/cgroup"),
                root: PathBuf::from("/"),
                read_only: false,
            },
            self_membership: Some(PathBuf::from(format!(
                "/user.slice/user-{OWNER}.slice/session-1.scope"
            ))),
            observed_path: PathBuf::from(format!("{user}/session-1.scope")),
            explicitly_configured: false,
            ancestors: vec![
                level(&format!("{user}/session-1.scope"), cpu),
                level(&user, cpu),
                level("/sys/fs/cgroup/user.slice", true),
                level("/sys/fs/cgroup", true),
            ],
            detail: "fixture".into(),
        }
    });
    let snapshot = Snapshot {
        schema_version: SCHEMA_VERSION,
        node_id: Config::default().node_id,
        observed_at_unix_ms: 100,
        cpu_capacity_millicores: 4000,
        physical_cores: Some(4),
        cpu_busy_millicores: Some(busy),
        cpu_busy_lower_millicores: None,
        managed_cpu_lower_millicores: Some(1000),
        managed_cpu_window_lower_millicores: None,
        cedegrid_cpu_lower_millicores: Some(cedegrid),
        cedegrid_cpu_window_lower_millicores: None,
        total_ram_mib: 32_000,
        available_ram_mib: Some(26_000),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: BTreeMap::new(),
        kernel: Some(KernelSnapshot {
            schema_version: 1,
            runtime_os: "linux".into(),
            kernel_release: Some("fixture".into()),
            observed_at_unix_ms: 100,
            monotonic_elapsed_ms: 500,
            sample_interval_ms: Some(500),
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
                effective_cpu_capacity_millicores: Some(4000),
                busy_millicores: Some(busy),
                busy_lower_millicores: None,
                busy_status: CpuUsageStatus::Available,
                busy_interval_ms: Some(500),
                busy_window_millicores: None,
                busy_window_lower_millicores: None,
                busy_window_ms: None,
                visible_cpu_ceiling_millicores: Some(4000),
                detail: "fixture".into(),
            },
            hierarchy,
            psi: vec![],
            cpu_pressure: None,
            limitations: vec![],
        }),
        principals: Some(vec![
            // The owner's processes outside own tasks: CedeGrid's alone.
            PrincipalUsage {
                uid: Some(OWNER),
                is_owner: true,
                cpu_millicores: cedegrid,
                ..PrincipalUsage::default()
            },
            PrincipalUsage {
                uid: Some(OTHER),
                is_owner: false,
                cpu_millicores: other,
                ..PrincipalUsage::default()
            },
        ]),
        external_processes: None,
    };
    let input = PolicyInput {
        allocations: vec![Allocation {
            id: "job-1".into(),
            class: AllocationClass::Opportunistic,
            phase: AllocationPhase::Running,
            requested: Resources {
                cpu_millicores: 1000,
                ram_mib: 256,
                gpu_memory_mib: BTreeMap::new(),
            },
            observed: Some(Resources {
                cpu_millicores: 1000,
                ram_mib: 256,
                gpu_memory_mib: BTreeMap::new(),
            }),
            observed_upper: None,
            observed_window_cpu: None,
            usage_unknown: None,
            modes: Default::default(),
        }],
        ..PolicyInput::default()
    };
    (snapshot, input)
}

fn budget(mode: ContentionMode, other: u64, cedegrid: u64, per_user: Option<bool>) -> u64 {
    let (snapshot, input) = reading(other, cedegrid, per_user);
    let decision = PolicyEngine::new()
        .evaluate(&config(mode), &snapshot, &input, 0)
        .unwrap();
    assert!(
        decision.resources[&ResourceKey::Cpu]
            .admission
            .as_ref()
            .is_some(),
        "{decision:?}"
    );
    decision.managed_budget.cpu_millicores
}

/// The other user keeps its 2000 m, so own work keeps its 2000 m share for admission: room for
/// a second 1000 m task beside the one running. Where the kernel already charges CedeGrid's
/// CPU to the owner's own cgroup, admission does not charge it again.
#[test]
fn own_work_keeps_its_fair_share_beside_a_principal_at_its_own() {
    for mode in [
        ContentionMode::Neutral,
        ContentionMode::Performance,
        ContentionMode::Aggressive,
    ] {
        assert_eq!(budget(mode, 2000, 50, Some(true)), 2000, "{mode:?}");
    }
}

/// Where the owner has no CPU cgroup of its own, CedeGrid's CPU competes with the other user
/// directly, so it still comes out of own work's share, as before.
#[test]
fn without_a_cpu_cgroup_of_its_own_cedegrid_is_charged_to_the_share() {
    for per_user in [None, Some(false)] {
        assert_eq!(
            budget(ContentionMode::Performance, 2000, 50, per_user),
            1950,
            "{per_user:?}"
        );
    }
}

/// Readings that move by less than a principal's floor (1% of the scope) do not move
/// admission: the equal share holds while the other user's usage reads 1960 to 2040 m and
/// CedeGrid's 20 to 90 m.
#[test]
fn noisy_readings_do_not_move_the_kept_share() {
    for other in [1960, 1990, 2000, 2020, 2040] {
        for cedegrid in [20, 50, 90] {
            assert_eq!(
                budget(ContentionMode::Performance, other, cedegrid, Some(true)),
                2000,
                "other {other}, CedeGrid {cedegrid}"
            );
        }
    }
}

/// With the other user well under its share, nothing changes: the mode keeps its margin.
#[test]
fn a_principal_under_its_share_leaves_the_margin_as_before() {
    // 4000 m less the other's 1000 m, Performance's 2% (80 m) and CedeGrid's 50 m.
    assert_eq!(
        budget(ContentionMode::Performance, 1000, 50, Some(true)),
        2870
    );
}

/// The kept share is never more than the fair share: an other user taking more than its own
/// share leaves own work what is left, never more, and own work takes no more than its share
/// when the other leaves room within the margin.
#[test]
fn the_kept_share_never_takes_from_the_other_principals_share() {
    // The other user over its share by more than the floor: own work gets what is left.
    assert!(budget(ContentionMode::Performance, 2400, 50, Some(true)) < 2000);
}
