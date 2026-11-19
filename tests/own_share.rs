//! Own work keeps its fair share for admission while another principal keeps its own, and holds
//! it steady (user, 2026-11-17, option A). 004's host gate F2c found own work admitted to half
//! its share: CedeGrid's own CPU was charged to the share and a 1000 m task no longer fit.
//!
//! Since 006, Performance and Aggressive keep that share only beside another account's CedeGrid
//! work that arrived first; beside a plain principal, or CedeGrid work that arrived later, they
//! start new work as plain Linux would (006 FR-006 to FR-008, FR-013). The other user here is a
//! first-comer unless a test says otherwise.
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
/// The owner's other processes, outside own tasks and CedeGrid, run at `owner` m.
fn reading_with(
    other: u64,
    cedegrid: u64,
    owner: u64,
    per_user: Option<bool>,
) -> (Snapshot, PolicyInput) {
    reading_beside(other, cedegrid, owner, per_user, FIRST)
}

/// Own work's run arrived on the node at this time.
const OWN_ARRIVAL: u64 = 1_000;
/// The other user's CedeGrid work arrived before own work's, with Yield First or Neutral work.
const FIRST: Option<CedeGridWork> = Some(CedeGridWork {
    arrival_unix_ms: Some(500),
    protected_arrival_unix_ms: None,
});

fn reading_beside(
    other: u64,
    cedegrid: u64,
    owner: u64,
    per_user: Option<bool>,
    work: Option<CedeGridWork>,
) -> (Snapshot, PolicyInput) {
    let busy = 1000 + cedegrid + owner + other;
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
            // The owner's processes outside own tasks: CedeGrid's and the others'.
            PrincipalUsage {
                uid: Some(OWNER),
                is_owner: true,
                cpu_millicores: cedegrid + owner,
                ..PrincipalUsage::default()
            },
            PrincipalUsage {
                uid: Some(OTHER),
                is_owner: false,
                cpu_millicores: other,
                cedegrid_cpu: work,
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
        arrivals: BTreeMap::from([("job-1".into(), OWN_ARRIVAL)]),
        ..PolicyInput::default()
    };
    (snapshot, input)
}

fn budget(mode: ContentionMode, other: u64, cedegrid: u64, per_user: Option<bool>) -> u64 {
    budget_with(mode, other, cedegrid, 0, per_user)
}

fn budget_with(
    mode: ContentionMode,
    other: u64,
    cedegrid: u64,
    owner: u64,
    per_user: Option<bool>,
) -> u64 {
    let (snapshot, input) = reading_with(other, cedegrid, owner, per_user);
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

/// The owner's other processes, such as a shell or the coordinator, share the owner's slice
/// with own work where the owner has a CPU cgroup of its own, so own tasks still get the equal
/// share on their requests; where it has none, they come out of the share as before.
#[test]
fn the_owners_other_processes_share_its_slice_where_it_has_one() {
    assert_eq!(
        budget_with(ContentionMode::Performance, 2000, 30, 200, Some(true)),
        2000
    );
    // The equal 2000 m less the owner's other 200 m and CedeGrid's 30 m.
    assert_eq!(
        budget_with(ContentionMode::Performance, 2000, 30, 200, None),
        1770
    );
}

fn decide(mode: ContentionMode, other: u64, work: Option<CedeGridWork>) -> Decision {
    let (snapshot, input) = reading_beside(other, 50, 0, Some(true), work);
    PolicyEngine::new()
        .evaluate(&config(mode), &snapshot, &input, 0)
        .unwrap()
}

fn codes(decision: &Decision) -> Vec<String> {
    decision.resources[&ResourceKey::Cpu]
        .reasons
        .iter()
        .map(|reason| reason.code.clone())
        .collect()
}

/// Beside an account that does not run CedeGrid, Performance and Aggressive start new work as
/// plain Linux would: up to the scope's capacity less the reserve, with no margin, no deduction
/// of the other's usage and CedeGrid's own CPU left in (006 FR-007, SC-002).
#[test]
fn beside_a_plain_principal_performance_admits_up_to_capacity() {
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        for other in [500, 2000, 2900] {
            let decision = decide(mode, other, None);
            assert_eq!(
                decision.managed_budget.cpu_millicores, 4000,
                "{mode:?} beside {other} m"
            );
            assert!(
                codes(&decision).contains(&"admitted_as_plain".to_string()),
                "{mode:?}: {:?}",
                codes(&decision)
            );
            let rows = &decision.resources[&ResourceKey::Cpu].principals;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert!(!rows[0].cedegrid && !rows[0].held_for, "{rows:?}");
        }
    }
}

/// CedeGrid work that arrived after own work's is not held for either (006 FR-006, FR-007).
#[test]
fn beside_cedegrid_work_that_arrived_later_performance_admits_as_plain() {
    let later = Some(CedeGridWork {
        arrival_unix_ms: Some(OWN_ARRIVAL + 1),
        protected_arrival_unix_ms: Some(OWN_ARRIVAL + 1),
    });
    let decision = decide(ContentionMode::Performance, 2000, later);
    assert_eq!(decision.managed_budget.cpu_millicores, 4000);
    let row = &decision.resources[&ResourceKey::Cpu].principals[0];
    assert!(row.cedegrid && !row.first_comer && !row.held_for, "{row:?}");
    assert_eq!(
        decision.resources[&ResourceKey::Cpu].own_arrival_unix_ms,
        Some(OWN_ARRIVAL)
    );
}

/// Beside CedeGrid work that arrived first, at the same time, or at a time either side cannot
/// read, Performance holds at the kept equal share, and the decision names the first-comer
/// (006 FR-006, FR-008, FR-020).
#[test]
fn beside_a_first_comer_performance_holds_at_the_fair_share() {
    for arrival in [Some(500), Some(OWN_ARRIVAL), None] {
        let work = Some(CedeGridWork {
            arrival_unix_ms: arrival,
            protected_arrival_unix_ms: None,
        });
        let decision = decide(ContentionMode::Performance, 2000, work);
        assert_eq!(
            decision.managed_budget.cpu_millicores, 2000,
            "arrival {arrival:?}"
        );
        assert!(codes(&decision).contains(&"held_for_first_comer".to_string()));
        let row = &decision.resources[&ResourceKey::Cpu].principals[0];
        assert!(row.first_comer && row.held_for, "{row:?}");
    }
}

/// Own work with no arrival on record, or with nothing running, comes after every CedeGrid
/// principal (006 FR-006).
#[test]
fn own_work_without_a_known_arrival_holds_for_cedegrid_work() {
    let later = Some(CedeGridWork {
        arrival_unix_ms: Some(OWN_ARRIVAL + 1),
        protected_arrival_unix_ms: None,
    });
    let (snapshot, mut input) = reading_beside(2000, 50, 0, Some(true), later);
    input.arrivals.clear();
    let decision = PolicyEngine::new()
        .evaluate(&config(ContentionMode::Performance), &snapshot, &input, 0)
        .unwrap();
    assert_eq!(decision.managed_budget.cpu_millicores, 2000);
}

/// Yield First and Neutral keep 001's admission beside every principal, plain or not
/// (006 FR-010).
#[test]
fn neutral_holds_beside_a_plain_principal_as_before() {
    assert_eq!(
        decide(ContentionMode::Neutral, 2000, None)
            .managed_budget
            .cpu_millicores,
        2000
    );
    assert!(
        !codes(&decide(ContentionMode::Neutral, 2000, None))
            .iter()
            .any(|code| code == "admitted_as_plain")
    );
}

/// The admit immediately budget lifts the hold beside a first-comer that runs only Yield First
/// or Neutral work, and keeps it beside one that runs Performance or Aggressive work there,
/// saying so (006 FR-013).
#[test]
fn admit_immediately_lifts_the_hold_only_for_an_unprotected_first_comer() {
    let decision = decide(ContentionMode::Performance, 2000, FIRST);
    assert_eq!(decision.managed_budget.cpu_millicores, 2000);
    assert_eq!(
        decision
            .admit_immediately_budget
            .as_ref()
            .map(|budget| budget.cpu_millicores),
        Some(4000)
    );
    let protected = Some(CedeGridWork {
        arrival_unix_ms: Some(500),
        protected_arrival_unix_ms: Some(500),
    });
    let decision = decide(ContentionMode::Performance, 2000, protected);
    assert_eq!(
        decision
            .admit_immediately_budget
            .as_ref()
            .map(|budget| budget.cpu_millicores),
        Some(2000)
    );
    assert!(
        codes(&decision)
            .contains(&"admit_immediately_ignored_for_protected_first_comer".to_string())
    );
}

/// While the principal list cannot be read, no account is known to be plain: Performance holds
/// as 001 does (Principle III).
#[test]
fn an_unreadable_principal_list_holds_as_before() {
    let (mut snapshot, input) = reading_beside(2000, 50, 0, Some(true), None);
    snapshot.principals = None;
    let decision = PolicyEngine::new()
        .evaluate(&config(ContentionMode::Performance), &snapshot, &input, 0)
        .unwrap();
    assert!(decision.managed_budget.cpu_millicores < 4000);
}

/// `doctor` lists each other account on the CPU with its kind, arrival and whether admission
/// holds for it, and own work's arrival (006 FR-021).
#[test]
fn doctor_lists_the_other_accounts_and_whom_admission_holds_for() {
    let (snapshot, input) = reading_beside(2000, 50, 0, Some(true), FIRST);
    let mut config = config(ContentionMode::Performance);
    config.node_id = snapshot.node_id.clone();
    let decision = PolicyEngine::new()
        .evaluate(&config, &snapshot, &input, 0)
        .unwrap();
    let text = cedegrid::explain::doctor_text(&config, &snapshot, &decision, &[]);
    assert!(
        text.contains("own arrival: 1000 ms since the epoch"),
        "{text}"
    );
    let row = text
        .lines()
        .find(|line| line.trim_start().starts_with(&OTHER.to_string()))
        .unwrap_or_else(|| panic!("no row for the other account: {text}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells,
        [&OTHER.to_string(), "cedegrid", "500", "yes", "no", "yes"]
    );
}

/// 006 FR-007a: the decision says when Performance or Aggressive starts work beside a plain
/// principal with no first-comer holding it, so the agent may start tasks as plain Linux would.
#[test]
fn the_decision_flags_cpu_beside_a_plain_principal() {
    // The flag sets after three intervals of contention (the debounce test below).
    let flag = |mode: ContentionMode, other: u64, work: Option<CedeGridWork>| {
        let (mut snapshot, input) = reading_beside(other, 50, 0, Some(true), work);
        let mut engine = PolicyEngine::new();
        let mut on = false;
        for i in 0..4u64 {
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            on = engine
                .evaluate(&config(mode), &snapshot, &input, i * 500)
                .unwrap()
                .cpu_beside_plain;
        }
        on
    };
    let later = Some(CedeGridWork {
        arrival_unix_ms: Some(OWN_ARRIVAL + 1),
        protected_arrival_unix_ms: None,
    });
    // At 2900 m the other user and own work keep the CPU saturated: they contend.
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        assert!(
            flag(mode, 2900, None),
            "{mode:?}: a plain principal contends"
        );
        assert!(
            !flag(mode, 2900, FIRST),
            "{mode:?}: a first-comer holds own work"
        );
        assert!(
            !flag(mode, 2900, later),
            "{mode:?}: later CedeGrid work is not plain"
        );
        assert!(!flag(mode, 0, None), "{mode:?}: nobody contends");
        assert!(
            !flag(mode, 1000, None),
            "{mode:?}: a plain principal on a CPU with room to spare does not contend"
        );
    }
    for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
        assert!(!flag(mode, 2900, None), "{mode:?}");
    }
}

/// Usage no principal accounts for is unidentifiable, so it counts as plain; an unreadable
/// principal list holds for everyone, so the flag stays off (006 FR-004, FR-007a).
#[test]
fn unattributed_usage_counts_as_plain_and_an_unreadable_list_does_not() {
    let (mut snapshot, input) = reading_beside(2900, 50, 0, Some(true), None);
    for principal in snapshot.principals.iter_mut().flatten() {
        if principal.uid == Some(OTHER) {
            principal.uid = None;
        }
    }
    let after = |snapshot: &mut Snapshot| {
        let mut engine = PolicyEngine::new();
        let mut on = false;
        for i in 0..4u64 {
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            on = engine
                .evaluate(
                    &config(ContentionMode::Performance),
                    snapshot,
                    &input,
                    i * 500,
                )
                .unwrap()
                .cpu_beside_plain;
        }
        on
    };
    assert!(after(&mut snapshot));
    snapshot.principals = None;
    assert!(!after(&mut snapshot));
}

/// Performance and Aggressive leave own CPU above the operator envelope alone: no yield, cap or
/// drain, as plain Linux would run it. Neutral still steps it, and memory over the envelope
/// still steps in every mode (006 FR-007a, FR-057).
#[test]
fn performance_takes_no_cpu_step_for_the_envelope() {
    let run = |mode: ContentionMode, envelope: Resources| {
        let (snapshot, mut input) = reading_beside(500, 50, 0, Some(true), None);
        input.envelope = Some(envelope);
        let mut engine = PolicyEngine::new();
        let mut actions = Vec::new();
        for i in 0..40u64 {
            let decision = engine
                .evaluate(&config(mode), &snapshot, &input, i * 500)
                .unwrap();
            actions.extend(decision.actions);
        }
        actions
    };
    let cpu_over = Resources {
        cpu_millicores: 500,
        ram_mib: 32_000,
        gpu_memory_mib: BTreeMap::new(),
    };
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        let actions = run(mode, cpu_over.clone());
        assert!(
            actions.iter().all(|a| a.resource != ResourceKey::Cpu),
            "{mode:?}: {actions:?}"
        );
    }
    let neutral = run(ContentionMode::Neutral, cpu_over);
    assert!(
        neutral.iter().any(|a| a.resource == ResourceKey::Cpu),
        "Neutral steps own CPU over the envelope"
    );
    let ram_over = Resources {
        cpu_millicores: 4000,
        ram_mib: 100,
        gpu_memory_mib: BTreeMap::new(),
    };
    let actions = run(ContentionMode::Performance, ram_over);
    assert!(
        actions.iter().any(|a| a.resource == ResourceKey::Ram),
        "memory over the envelope still steps: {actions:?}"
    );
}

/// An agent scoped to one CPU beside a first-comer whose work runs on the host's other CPUs:
/// the principal list reads that work host-wide, but the scope sees none of it, so the hold
/// counts only what the scope measures and the scope's CPU stays open (006 FR-008).
#[test]
fn a_first_comer_busy_outside_the_scope_does_not_hold_it() {
    let scoped = |other_in_scope: u64| {
        // Own work at 500 m and CedeGrid at 50 m run on CPU 0; the first-comer runs 2000 m
        // host-wide, `other_in_scope` of it on CPU 0.
        let (mut snapshot, mut input) = reading_beside(2000, 50, 0, Some(true), FIRST);
        input.allocations[0].requested.cpu_millicores = 500;
        input.allocations[0]
            .observed
            .as_mut()
            .unwrap()
            .cpu_millicores = 500;
        snapshot.managed_cpu_lower_millicores = Some(500);
        snapshot.cpu_busy_millicores = Some(500 + 50 + 2000);
        let kernel = snapshot.kernel.as_mut().unwrap();
        kernel.cpu.allowed_cpu_ids = Some(vec![0]);
        kernel.cpu.effective_cgroup_cpu_ids = Some(vec![0]);
        kernel.cpu.effective_cpu_ids = Some(vec![0]);
        kernel.cpu.cores.truncate(1);
        kernel.cpu.effective_cpu_capacity_millicores = Some(1000);
        kernel.cpu.visible_cpu_ceiling_millicores = Some(1000);
        kernel.cpu.busy_millicores = Some(500 + 50 + other_in_scope);
        PolicyEngine::new()
            .evaluate(&config(ContentionMode::Performance), &snapshot, &input, 0)
            .unwrap()
    };
    let outside = scoped(0);
    assert_eq!(
        outside.managed_budget.cpu_millicores,
        1000,
        "{:?}",
        outside.resources[&ResourceKey::Cpu]
    );
    // Where the first-comer's work does run in the scope, the hold applies to that part.
    let inside = scoped(450);
    assert!(
        inside.managed_budget.cpu_millicores < 1000,
        "{:?}",
        inside.resources[&ResourceKey::Cpu]
    );
}

/// A run of `intervals` decisions 500 ms apart, with the other user at `other(i)` m and the
/// own tasks listed by `running(i)`, each at 1000 m.
fn flags(
    mode: ContentionMode,
    intervals: u64,
    other: &dyn Fn(u64) -> u64,
    unattributed: &dyn Fn(u64) -> u64,
    running: &dyn Fn(u64) -> usize,
) -> Vec<bool> {
    let mut engine = PolicyEngine::new();
    (0..intervals)
        .map(|i| {
            let (mut snapshot, mut input) = reading_beside(other(i), 50, 0, Some(true), None);
            let n = running(i);
            let job = input.allocations[0].clone();
            input.allocations = (0..n)
                .map(|k| Allocation {
                    id: format!("job-{k}"),
                    ..job.clone()
                })
                .collect();
            input.arrivals = (0..n).map(|k| (format!("job-{k}"), OWN_ARRIVAL)).collect();
            let own = 1000 * n as u64;
            snapshot.managed_cpu_lower_millicores = Some(own);
            let busy = (own + 50 + other(i) + unattributed(i)).min(4000);
            snapshot.cpu_busy_millicores = Some(busy);
            snapshot.kernel.as_mut().unwrap().cpu.busy_millicores = Some(busy);
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            engine
                .evaluate(&config(mode), &snapshot, &input, i * 500)
                .unwrap()
                .cpu_beside_plain
        })
        .collect()
}

/// The flag needs a plain principal's contention for three intervals in a row and holds for
/// 10 s after it ends, so one reading does not swing admission (006 FR-007a).
#[test]
fn the_beside_plain_flag_is_debounced_and_held() {
    // The other user contends from interval 10 to 29, the CPU saturated beside two own tasks.
    let run = flags(
        ContentionMode::Performance,
        80,
        &|i| if (10..30).contains(&i) { 1950 } else { 0 },
        &|_| 0,
        &|_| 2,
    );
    assert!(!run[..12].iter().any(|on| *on), "{run:?}");
    assert!(run[12..30].iter().all(|on| *on), "{run:?}");
    // Held for 10 s, 20 intervals, after the last contended interval.
    assert!(run[30..50].iter().all(|on| *on), "{run:?}");
    assert!(!run[51..].iter().any(|on| *on), "{run:?}");
    // A one-interval reading never sets it.
    let blip = flags(
        ContentionMode::Performance,
        40,
        &|i| if i == 10 || i == 20 { 1950 } else { 0 },
        &|_| 0,
        &|_| 2,
    );
    assert!(!blip.iter().any(|on| *on), "{blip:?}");
}

/// Own tasks that exit leave their last interval's CPU in the host's busy reading after the own
/// reading has dropped; that unattributed CPU is not a plain principal (006 FR-004, FR-007a).
#[test]
fn own_task_exits_never_make_a_plain_principal() {
    // Three tasks exit at interval 10, two at 11 and two more at 12, each leaving its CPU
    // unattributed for that interval and the next.
    let running = |i: u64| match i {
        0..10 => 3 + 2 + 2,
        10 => 4,
        11 => 2,
        _ => 0,
    };
    let left = |i: u64| match i {
        10 => 3000,
        11 => 2000 + 1500,
        12 => 2000 + 1000,
        13 => 1000,
        _ => 0,
    };
    let run = flags(ContentionMode::Performance, 40, &|_| 0, &left, &|i| {
        running(i).min(4)
    });
    assert!(!run.iter().any(|on| *on), "{run:?}");
}

/// Performance and Aggressive drain no CPU reservation that is admitted but not started when
/// own charges come to exceed the budget: new work waits instead (006 FR-007a, FR-009).
#[test]
fn performance_drains_no_pending_cpu_reservation() {
    let run = |mode: ContentionMode| {
        // Four tasks run and twelve more were admitted; the budget is back at 4000 m.
        let (mut snapshot, mut input) = reading_beside(0, 50, 0, Some(true), None);
        snapshot.managed_cpu_lower_millicores = Some(4000);
        snapshot.cpu_busy_millicores = Some(4000);
        snapshot.kernel.as_mut().unwrap().cpu.busy_millicores = Some(4000);
        let job = input.allocations[0].clone();
        input.allocations = (0..16)
            .map(|k| Allocation {
                id: format!("job-{k}"),
                phase: if k < 4 {
                    AllocationPhase::Running
                } else {
                    AllocationPhase::Pending
                },
                observed: (k < 4).then(|| job.observed.clone()).flatten(),
                ..job.clone()
            })
            .collect();
        let mut engine = PolicyEngine::new();
        let mut actions = Vec::new();
        let mut last = None;
        for i in 0..10u64 {
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            let decision = engine
                .evaluate(&config(mode), &snapshot, &input, i * 500)
                .unwrap();
            actions.extend(decision.actions.clone());
            last = Some(decision);
        }
        (actions, last.unwrap())
    };
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        let (actions, last) = run(mode);
        assert!(
            actions.iter().all(|a| a.resource != ResourceKey::Cpu),
            "{mode:?}: {actions:?}"
        );
        // New work waits through the headroom; the admission state stays open, so the tasks
        // already placed are still authorized.
        assert_eq!(last.admission_headroom.cpu_millicores, 0, "{mode:?}");
        assert_eq!(
            last.resources[&ResourceKey::Cpu].admission,
            Some(cedegrid::contention::Admission::Open),
            "{mode:?}"
        );
    }
    let (actions, _) = run(ContentionMode::Neutral);
    assert!(
        actions
            .iter()
            .any(|a| a.resource == ResourceKey::Cpu
                && a.step >= cedegrid::contention::YieldStep::Drain),
        "Neutral still releases reservations that no longer fit"
    );
}

/// On a CPU with room to spare, the system's own small, unattributed use, such as the agent
/// starting up or its supervisors, is no plain principal contending (006 FR-007a).
#[test]
fn unattributed_noise_on_an_idle_cpu_sets_no_flag() {
    let run = flags(
        ContentionMode::Performance,
        60,
        &|_| 0,
        &|i| [200, 42, 129, 92, 53, 81, 93][(i % 7) as usize],
        &|i| usize::from(i >= 30),
    );
    assert!(!run.iter().any(|on| *on), "{run:?}");
}

/// Own work running past the CPU budget, as the agent starts it beside a plain user, is no reason
/// for a cooldown: CPU admission stays open for the tasks already placed (006 FR-007a, FR-009).
#[test]
fn own_work_over_the_cpu_budget_holds_no_cooldown_in_performance() {
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        let mut engine = PolicyEngine::new();
        let mut codes = Vec::new();
        for i in 0..20u64 {
            // Eight 1000 m tasks admitted beside a plain user; four have started, all by 3 s.
            let (mut snapshot, mut input) = reading_beside(2900, 50, 0, Some(true), None);
            let job = input.allocations[0].clone();
            // The running tasks share the 1000 m the other user leaves.
            let started = if i >= 6 { 8 } else { 4 };
            input.allocations = (0..8)
                .map(|k| {
                    let running = k < started;
                    Allocation {
                        id: format!("job-{k}"),
                        phase: if running {
                            AllocationPhase::Running
                        } else {
                            AllocationPhase::Pending
                        },
                        observed: running.then(|| Resources {
                            cpu_millicores: 1000 / started as u64,
                            ram_mib: 256,
                            gpu_memory_mib: BTreeMap::new(),
                        }),
                        ..job.clone()
                    }
                })
                .collect();
            input.arrivals = (0..8).map(|k| (format!("job-{k}"), OWN_ARRIVAL)).collect();
            snapshot.managed_cpu_lower_millicores = Some(1000);
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            let decision = engine
                .evaluate(&config(mode), &snapshot, &input, i * 500)
                .unwrap();
            codes.push(
                decision.resources[&ResourceKey::Cpu]
                    .admission
                    .as_ref()
                    .and_then(|a| a.code())
                    .map(str::to_string),
            );
        }
        assert!(
            !codes
                .iter()
                .skip(1)
                .any(|code| code.as_deref() == Some("admission_cooling_down")),
            "{mode:?}: {codes:?}"
        );
    }
}

/// F2c: a plain user at 2000 m from before submission, own work filling the rest of a saturated
/// CPU. The flag sets within the three-interval streak, so own work fills to its task limit as
/// plain Linux would start it (006 FR-007a).
#[test]
fn a_named_plain_user_on_a_saturated_cpu_sets_the_flag_promptly() {
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        let mut engine = PolicyEngine::new();
        let mut run = Vec::new();
        for i in 0..20u64 {
            // Before interval 4 own work is not running; then four 1000 m tasks take 2400 m of
            // the full CPU, more than half, and the plain user gets the rest, as on the host.
            let started = if i >= 4 { 4 } else { 0 };
            let other = if started > 0 { 1550 } else { 2000 };
            let (mut snapshot, mut input) = reading_beside(other, 50, 0, Some(true), None);
            let job = input.allocations[0].clone();
            input.allocations = (0..started)
                .map(|k| Allocation {
                    id: format!("job-{k}"),
                    observed: Some(Resources {
                        cpu_millicores: 600,
                        ram_mib: 256,
                        gpu_memory_mib: BTreeMap::new(),
                    }),
                    ..job.clone()
                })
                .collect();
            input.arrivals = (0..started)
                .map(|k| (format!("job-{k}"), OWN_ARRIVAL))
                .collect();
            let own = 600 * started as u64;
            let busy = (own + 50 + other).min(4000);
            snapshot.managed_cpu_lower_millicores = Some(own);
            snapshot.cpu_busy_millicores = Some(busy);
            snapshot.kernel.as_mut().unwrap().cpu.busy_millicores = Some(busy);
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            run.push(
                engine
                    .evaluate(&config(mode), &snapshot, &input, i * 500)
                    .unwrap()
                    .cpu_beside_plain,
            );
        }
        // Saturated from interval 4: set by interval 6, and on from there.
        assert!(run[6..].iter().all(|on| *on), "{mode:?}: {run:?}");
    }
}

/// SC-002: eight 500 m tasks on an idle four-CPU scope, each using about 1000 m. Plain Linux
/// starts all eight; Performance admits against their requests, not the usage above them, so
/// the three still waiting fit beside the five running (006 FR-007, SC-002).
#[test]
fn performance_admits_by_requests_where_tasks_use_more() {
    for mode in [ContentionMode::Performance, ContentionMode::Aggressive] {
        let mut engine = PolicyEngine::new();
        let mut last = None;
        for i in 0..10u64 {
            let (mut snapshot, mut input) = reading_beside(0, 50, 0, Some(true), None);
            let job = input.allocations[0].clone();
            input.allocations = (0..5)
                .map(|k| Allocation {
                    id: format!("job-{k}"),
                    requested: Resources {
                        cpu_millicores: 500,
                        ..job.requested.clone()
                    },
                    observed: Some(Resources {
                        cpu_millicores: 790,
                        ram_mib: 256,
                        gpu_memory_mib: BTreeMap::new(),
                    }),
                    ..job.clone()
                })
                .collect();
            input.arrivals = (0..5).map(|k| (format!("job-{k}"), OWN_ARRIVAL)).collect();
            snapshot.managed_cpu_lower_millicores = Some(3950);
            snapshot.cpu_busy_millicores = Some(4000);
            snapshot.kernel.as_mut().unwrap().cpu.busy_millicores = Some(4000);
            snapshot.observed_at_unix_ms = 100 + i * 500;
            snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
            last = Some(
                engine
                    .evaluate(&config(mode), &snapshot, &input, i * 500)
                    .unwrap(),
            );
        }
        let last = last.unwrap();
        // 4000 m less the five requests of 500 m: room for the other three.
        assert!(
            last.admission_headroom.cpu_millicores >= 1500,
            "{mode:?}: {:?}",
            last.admission_headroom
        );
        assert!(
            cedegrid::policy::schedulable_budget(&last).cpu_millicores >= 4000,
            "{mode:?}"
        );
    }
    // Beside a first-comer the hold still charges own usage as before: the 1000 m job,
    // requested at 500 m, leaves 1000 m of the 2000 m share.
    let (mut snapshot, mut input) = reading_beside(2000, 50, 0, Some(true), FIRST);
    input.allocations[0].requested.cpu_millicores = 500;
    let mut engine = PolicyEngine::new();
    let mut held = None;
    for i in 0..10u64 {
        snapshot.observed_at_unix_ms = 100 + i * 500;
        snapshot.kernel.as_mut().unwrap().monotonic_elapsed_ms = 500 + i * 500;
        held = Some(
            engine
                .evaluate(
                    &config(ContentionMode::Performance),
                    &snapshot,
                    &input,
                    i * 500,
                )
                .unwrap(),
        );
    }
    let held = held.unwrap();
    assert_eq!(held.managed_budget.cpu_millicores, 2000);
    assert_eq!(held.admission_headroom.cpu_millicores, 1000);
}
