//! What the node shows (US8): the reason-code catalog, `held_back`, and the
//! rendered `observe` JSON and `doctor` text through a scripted contention run
//! (SC-010).
use cedegrid::{
    config::Config,
    contention::{ContentionMode, REASON_CODES, ResourceKey, YieldStep},
    execution_model::AllocationClass,
    model::*,
};
use snapshot::PolicyEngine;
use std::collections::BTreeMap;

#[allow(dead_code)]
mod snapshot {
    include!("support/snapshot.rs");
}

const CPU: u64 = 32_000;
const GPU: &str = "GPU-a";

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.contention.default_mode = ContentionMode::Neutral;
    cfg
}

/// The 32-CPU, 256 GiB, 24 GiB-GPU host.
fn host() -> Snapshot {
    Snapshot {
        schema_version: SCHEMA_VERSION,
        node_id: "local".into(),
        observed_at_unix_ms: 100,
        cpu_capacity_millicores: CPU,
        physical_cores: Some(16),
        cpu_busy_millicores: Some(0),
        cpu_busy_lower_millicores: None,
        managed_cpu_lower_millicores: None,
        managed_cpu_window_lower_millicores: None,
        cedegrid_cpu_lower_millicores: None,
        cedegrid_cpu_window_lower_millicores: None,
        total_ram_mib: 262_144,
        available_ram_mib: Some(250_000),
        gpu_inventory: CapabilityStatus::Available,
        gpus: vec![GpuSnapshot {
            uuid: GPU.into(),
            total_memory_mib: 24_576,
            used_memory_mib: Some(2048),
            utilization_percent: Some(0),
            external_process_ids: Some(vec![]),
            external_compute: ComputeActivity::Idle,
            compute_sample_id: Some(1),
            observation: None,
            reserved_memory_mib: None,
            free_memory_mib: None,
            unattributed_memory_mib: None,
            external_compute_milli: Some(0),
            own_compute_milli: Some(0),
            compute_amount_unknown: None,
            own_only: false,
            background_process_ids: vec![],
        }],
        capabilities: BTreeMap::new(),
        kernel: None,
        principals: Some(vec![]),
        external_processes: None,
    }
}

fn user(uid: u32, cpu: u64, gpu: u64) -> PrincipalUsage {
    PrincipalUsage {
        uid: Some(uid),
        is_owner: false,
        cpu_millicores: cpu,
        gpu_compute_milli: if gpu > 0 {
            BTreeMap::from([(GPU.to_string(), gpu)])
        } else {
            BTreeMap::new()
        },
        ..PrincipalUsage::default()
    }
}

fn owner() -> PrincipalUsage {
    PrincipalUsage {
        uid: Some(1000),
        is_owner: true,
        ..PrincipalUsage::default()
    }
}

/// Max-min fair shares of `capacity` among demands.
fn share(capacity: u64, demands: &[u64]) -> Vec<u64> {
    let mut given = vec![0u64; demands.len()];
    let mut left = capacity;
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

/// The limit the supervisor applies for the job on `key`.
fn limit(actions: &[YieldAction], key: &ResourceKey, capacity: u64) -> u64 {
    actions
        .iter()
        .find(|a| &a.resource == key)
        .map_or(capacity, |a| match a.step {
            YieldStep::Cap => a.target.unwrap_or(capacity),
            YieldStep::Pause => 0,
            _ => capacity,
        })
}

fn facts() -> BTreeMap<String, AllocationFacts> {
    BTreeMap::from([(
        "job".to_string(),
        AllocationFacts {
            unavailable: BTreeMap::from([(
                ResourceKey::Cpu,
                std::collections::BTreeSet::from([YieldStep::LowerShare, YieldStep::Hint]),
            )]),
            ..AllocationFacts::default()
        },
    )])
}

fn assert_catalogued(decision: &Decision) {
    for code in decision.codes() {
        assert!(
            cedegrid::contention::reason_text(&code).is_some(),
            "{code} is not in the catalog"
        );
    }
}

struct Step {
    decision: Decision,
    snapshot: Snapshot,
}

/// A job that wants the whole CPU and GPU beside one external user whose CPU and
/// GPU compute demands follow `phases` (intervals, CPU, GPU), with the supervisor
/// applying every cap and pause at once.
fn run(cfg: &Config, phases: &[(u64, u64, u64)]) -> Vec<Step> {
    let mut engine = PolicyEngine::new();
    let mut actions: Vec<YieldAction> = vec![];
    let mut out = vec![];
    let mut i = 0u64;
    let gpu_key = ResourceKey::GpuCompute(GPU.into());
    for &(intervals, external_cpu, external_gpu) in phases {
        for _ in 0..intervals {
            let cpu = share(
                CPU,
                &[external_cpu, limit(&actions, &ResourceKey::Cpu, CPU)],
            );
            let gpu = share(1000, &[external_gpu, limit(&actions, &gpu_key, 1000)]);
            let mut snapshot = host();
            snapshot.cpu_busy_millicores = Some((cpu[0] + cpu[1]).min(CPU));
            snapshot.principals = Some(vec![owner(), user(2001, cpu[0], gpu[0])]);
            let device = &mut snapshot.gpus[0];
            device.utilization_percent = Some(((gpu[0] + gpu[1]) / 10).min(100) as u32);
            device.external_compute_milli = Some(gpu[0]);
            device.own_compute_milli = Some(gpu[1]);
            device.compute_sample_id = Some(i + 1);
            if gpu[0] > 0 {
                device.external_process_ids = Some(vec![987]);
                device.external_compute = ComputeActivity::Active;
            }
            let job = Allocation {
                id: "job".into(),
                class: AllocationClass::Opportunistic,
                phase: AllocationPhase::Running,
                requested: Resources {
                    cpu_millicores: 1000,
                    ram_mib: 1024,
                    gpu_memory_mib: BTreeMap::from([(GPU.to_string(), 2048)]),
                },
                observed: Some(Resources {
                    cpu_millicores: cpu[1],
                    ram_mib: 1024,
                    gpu_memory_mib: BTreeMap::from([(GPU.to_string(), 2048)]),
                }),
                observed_upper: None,
                observed_window_cpu: None,
                modes: BTreeMap::new(),
            };
            let input = PolicyInput {
                allocations: vec![job],
                facts: facts(),
                ..PolicyInput::default()
            };
            let decision = engine.evaluate(cfg, &snapshot, &input, i * 500).unwrap();
            assert_catalogued(&decision);
            actions = decision.actions.clone();
            out.push(Step { decision, snapshot });
            i += 1;
        }
    }
    out
}

/// GPU tests take the measured path after two measured intervals.
fn measured(cfg: &mut Config) {
    cfg.gpu.process_sample_max_age_ms = 600;
    cfg.contention.gpu_compute.quiet_window_intervals = Some(2);
    cfg.contention
        .acknowledge_caution
        .insert("relaxed_detection".into());
}

fn codes_of(decision: &Decision, key: &ResourceKey) -> Vec<String> {
    decision.resources[key]
        .reasons
        .iter()
        .map(|r| r.code.clone())
        .collect()
}

/// The rendered `observe` JSON and `doctor` text show this step's resource state.
fn assert_shown(
    step: &Step,
    cfg: &Config,
    key: &ResourceKey,
    kind: cedegrid::contention::ResourceKind,
) {
    let observed = cedegrid::explain::observation(&step.snapshot, &step.decision, None);
    let status = &step.decision.resources[key];
    let name = key.to_string();
    assert_eq!(
        observed["decision"]["resources"][&name]["target_share"],
        serde_json::json!(status.target_share),
        "{name}"
    );
    assert_eq!(
        observed["decision"]["resources"][&name]["mode"],
        serde_json::json!(status.mode)
    );
    assert!(observed["decision"]["actions"].is_array());
    assert!(observed["decision"]["exceptions"].is_array());
    let text = cedegrid::explain::doctor_text(cfg, &step.snapshot, &step.decision, &[]);
    let block = text
        .split("\n\n")
        .find(|block| block.starts_with(&format!("{name}:")))
        .unwrap_or_else(|| panic!("no {name} block in:\n{text}"));
    let amount = |value: u64| cedegrid::explain::amount(kind, value);
    assert!(block.contains("Neutral"), "{block}");
    if let Some((own, _)) = status.own {
        assert!(block.contains(&format!("own {}", amount(own))), "{block}");
    }
    if let Some(external) = status.external {
        assert!(
            block.contains(&format!("external {}", amount(external))),
            "{block}"
        );
    }
    if let Some(target) = status.target_share {
        assert!(
            block.contains(&format!("target share {}", amount(target))),
            "{block}"
        );
    }
    for action in step.decision.actions.iter().filter(|a| &a.resource == key) {
        assert!(block.contains(&action.code), "{block}");
    }
    for reason in &status.reasons {
        assert!(block.contains(&reason.code), "{block}");
    }
    if let Some(held) = &status.held_back {
        for (cause, percent) in &held.percent {
            assert!(block.contains(cause), "{cause} in {block}");
            assert!(
                block.contains(&format!("{:.1}%", percent.0)),
                "{percent:?} in {block}"
            );
        }
    }
}

/// Idle, a steady external user, growth past the fair share, then the user leaves:
/// each step's mode, usage, demand, target share, step, reason codes and
/// `held_back` appear in `observe` and `doctor` (SC-010). CPU here.
#[test]
fn a_scripted_cpu_contention_run_is_shown_at_every_step() {
    let cfg = config();
    let phases = [(20, 0, 0), (120, 4000, 0), (120, 20_000, 0), (400, 0, 0)];
    let steps = run(&cfg, &phases);
    let key = ResourceKey::Cpu;
    let kind = cedegrid::contention::ResourceKind::Cpu;
    let ends: Vec<usize> = phases
        .iter()
        .scan(0usize, |at, (n, _, _)| {
            *at += *n as usize;
            Some(*at - 1)
        })
        .collect();
    for end in &ends {
        assert_shown(&steps[*end], &cfg, &key, kind);
    }
    // Idle: Neutral keeps its 5% margin; the job, which wants the whole CPU, is
    // capped at that target for its own usage over it (FR-011).
    let idle = &steps[ends[0]].decision;
    assert!(
        idle.actions.iter().all(|a| a.resource == key
            && a.step == YieldStep::Cap
            && a.target == Some(30_400)
            && a.cause == cedegrid::contention::YieldCause::OwnOverTarget),
        "{:?}",
        idle.actions
    );
    assert_eq!(idle.resources[&key].target_share, Some(30_400));
    assert_eq!(
        idle.resources[&key].held_back.as_ref().unwrap().margin,
        1600
    );
    // A steady small user is probed or watched, then recorded as satisfied.
    let steady: Vec<String> = steps[ends[0] + 1..=ends[1]]
        .iter()
        .flat_map(|s| codes_of(&s.decision, &key))
        .collect();
    assert!(
        steady
            .iter()
            .any(|c| c == "probing_demand" || c == "watching_larger_demand"),
        "{steady:?}"
    );
    assert!(steady.iter().any(|c| c == "satisfied_demand"), "{steady:?}");
    // Growth past the fair share: the target falls to the fair share, and the user takes
    // at least the other half, which counts as external. Own usage falls to the target, so
    // the idle cap no longer binds and reverses with its cause.
    let grown = &steps[ends[2]].decision;
    assert!(grown.resources[&key].target_share.unwrap() <= CPU / 2 + CPU / 100);
    assert!(grown.resources[&key].external.unwrap() >= CPU / 2 - CPU / 100);
    let growth: Vec<String> = steps[ends[1] + 1..=ends[2]]
        .iter()
        .flat_map(|s| codes_of(&s.decision, &key))
        .collect();
    assert!(
        growth.iter().any(|c| c == "step_reversed_own_usage_fell"),
        "{growth:?}"
    );
    // The user leaves: own usage is over the margin's target again and is capped there
    // with its own cause.
    let after = &steps[ends[2] + 1..];
    assert!(
        after.iter().any(|s| s.decision.actions.iter().any(
            |a| a.resource == key && a.cause == cedegrid::contention::YieldCause::OwnOverTarget
        )),
        "no step for own usage after the user left"
    );
    let last = &steps[ends[3]].decision;
    assert!(
        last.actions
            .iter()
            .all(|a| a.resource == key && a.target == Some(30_400)),
        "{:?}",
        last.actions
    );
    assert_eq!(last.resources[&key].target_share, Some(30_400));
}

/// The same run on one GPU device on the measured compute path.
#[test]
fn a_scripted_gpu_compute_run_is_shown_at_every_step() {
    let mut cfg = config();
    measured(&mut cfg);
    let phases = [(20, 0, 0), (120, 0, 125), (120, 0, 700), (400, 0, 0)];
    let steps = run(&cfg, &phases);
    let key = ResourceKey::GpuCompute(GPU.into());
    let kind = cedegrid::contention::ResourceKind::GpuCompute;
    let ends: Vec<usize> = phases
        .iter()
        .scan(0usize, |at, (n, _, _)| {
            *at += *n as usize;
            Some(*at - 1)
        })
        .collect();
    for end in &ends {
        assert_shown(&steps[*end], &cfg, &key, kind);
    }
    let path = |step: &Step| {
        step.decision.resources[&key]
            .held_back
            .as_ref()
            .and_then(|held| held.compute_path.clone())
    };
    assert_eq!(path(&steps[ends[1]]), Some(ComputePath::Measured));
    assert!(codes_of(&steps[ends[1]].decision, &key).contains(&"gpu_compute_measured".to_string()));
    // Growth past the fair share: the target share drops to it, and own work,
    // capped, runs within it.
    let grown = &steps[ends[2]];
    let cap = grown
        .decision
        .actions
        .iter()
        .find(|a| a.resource == key)
        .expect("a GPU compute step for external growth");
    assert_eq!(cap.code, "yield_step_cap");
    let target = grown.decision.resources[&key].target_share.unwrap();
    assert!(target <= 500, "{target}");
    assert!(grown.snapshot.gpus[0].own_compute_milli.unwrap() <= target);
    let after: Vec<String> = steps[ends[2] + 1..]
        .iter()
        .flat_map(|s| codes_of(&s.decision, &key))
        .collect();
    assert!(
        after.iter().any(|c| c == "step_reversed_contention_ended"),
        "{after:?}"
    );
    assert!(
        steps[ends[3]]
            .decision
            .actions
            .iter()
            .all(|a| a.resource != key)
    );
}

/// The node-wide cooldown says it covers only the 0.2 expansion flags, while GPU
/// memory admission is already open on its own cooldown.
#[test]
fn the_node_cooldown_names_the_legacy_flags_it_covers() {
    // The external user's arrival briefly takes own work over the budget, so the
    // node cooldown restarts; ten intervals later it is still pending.
    let steps = run(&config(), &[(20, 0, 0), (10, 4000, 0)]);
    let last = &steps.last().unwrap().decision;
    let reason = last
        .reason_codes
        .iter()
        .find(|r| r.code == "cooldown_pending")
        .expect("the node cooldown is pending");
    assert_eq!(
        Some(reason.text.as_str()),
        cedegrid::contention::reason_text("cooldown_pending")
    );
    assert!(
        last.resources[&ResourceKey::GpuMemory(GPU.into())]
            .admission
            .as_ref()
            .is_some_and(|a| a.code().is_none())
    );
}

/// Doctor offers the RAM cap only where a delegated leaf has `memory.high`
/// authorized, and shows a RAM cap step in MiB with its catalog codes.
#[test]
fn doctor_shows_the_ram_cap_where_the_leaf_allows_it() {
    use cedegrid::explain::{available_steps, doctor_text};
    let mut cfg = config();
    assert_eq!(available_steps(&cfg)["ram"], ["hint", "drain"]);
    cfg.cgroup.enabled = true;
    cfg.cgroup.delegated_root = Some("/sys/fs/cgroup/example.slice".into());
    cfg.cgroup.cpu_weight = None;
    cfg.cgroup.authorized_controls = vec!["cgroup.procs".into(), "memory.high".into()];
    assert_eq!(available_steps(&cfg)["ram"], ["cap", "hint", "drain"]);
    assert_eq!(available_steps(&cfg)["gpu_memory"], ["hint", "drain"]);
    // A floor breach of 3108 MiB on the 256 GiB host, with the RAM cap available.
    let mut snapshot = host();
    snapshot.available_ram_mib = Some(10_000);
    let input = PolicyInput {
        allocations: vec![Allocation {
            id: "job".into(),
            class: AllocationClass::Opportunistic,
            phase: AllocationPhase::Running,
            requested: Resources {
                ram_mib: 1024,
                ..Resources::default()
            },
            observed: Some(Resources {
                ram_mib: 20_000,
                ..Resources::default()
            }),
            observed_upper: None,
            observed_window_cpu: None,
            modes: BTreeMap::new(),
        }],
        facts: BTreeMap::from([(
            "job".to_string(),
            AllocationFacts {
                hint_ready: true,
                observes_drain: true,
                ram_cap: true,
                step_codes: BTreeMap::from([(ResourceKey::Ram, "ram_cap_growth_stop".into())]),
                ..AllocationFacts::default()
            },
        )]),
        ..PolicyInput::default()
    };
    let decision = PolicyEngine::new()
        .evaluate(&cfg, &snapshot, &input, 0)
        .unwrap();
    assert_catalogued(&decision);
    let text = doctor_text(&cfg, &snapshot, &decision, &[]);
    assert!(
        text.contains("step yield_step_cap to 16 892 MiB on job: floor_breach"),
        "{text}"
    );
    assert!(
        text.contains("reason ram_cap_growth_stop: allocation job:"),
        "{text}"
    );
    assert!(text.contains("  ram: cap, hint, drain"), "{text}");
}

/// Every code the policy reports, wherever it reports it, is in the catalog
/// (T113), across floors, envelopes, unknown readings, the node cap, drains and
/// both GPU compute paths.
#[test]
fn every_code_the_policy_emits_is_in_the_catalog() {
    let evaluate = |cfg: &Config, snapshot: &Snapshot, input: &PolicyInput| {
        let mut engine = PolicyEngine::new();
        for i in 0..4 {
            assert_catalogued(&engine.evaluate(cfg, snapshot, input, i * 500).unwrap());
        }
    };
    let job = |cpu: u64, ram: u64| Allocation {
        id: "job".into(),
        class: AllocationClass::Opportunistic,
        phase: AllocationPhase::Running,
        requested: Resources {
            cpu_millicores: cpu,
            ram_mib: ram,
            gpu_memory_mib: BTreeMap::new(),
        },
        observed: Some(Resources {
            cpu_millicores: cpu,
            ram_mib: ram,
            gpu_memory_mib: BTreeMap::new(),
        }),
        observed_upper: None,
        observed_window_cpu: None,
        modes: BTreeMap::from([(
            cedegrid::contention::ResourceKind::Cpu,
            ContentionMode::Performance,
        )]),
    };
    let base = PolicyInput {
        allocations: vec![job(8000, 60_000)],
        facts: facts(),
        ..PolicyInput::default()
    };
    let cfg = config();
    // The node cap refuses the job's Performance request.
    evaluate(&cfg, &host(), &base);
    // A RAM floor breach.
    let mut floor = host();
    floor.available_ram_mib = Some(1000);
    evaluate(&cfg, &floor, &base);
    // The operator envelope, an explicit drain and a full spool.
    for input in [
        PolicyInput {
            envelope: Some(Resources {
                cpu_millicores: 1000,
                ram_mib: 1000,
                gpu_memory_mib: BTreeMap::new(),
            }),
            ..base.clone()
        },
        PolicyInput {
            explicit_drain: true,
            ..base.clone()
        },
        PolicyInput {
            spool_full: true,
            ..base.clone()
        },
    ] {
        evaluate(&cfg, &host(), &input);
    }
    // Unknown CPU and RAM readings.
    let mut unknown = host();
    unknown.cpu_busy_millicores = None;
    unknown.available_ram_mib = None;
    evaluate(&cfg, &unknown, &base);
    // GPU compute of unknown amount, and samples that are not yet consistent.
    let mut gpu = host();
    gpu.gpus[0].external_compute = ComputeActivity::Active;
    gpu.gpus[0].external_process_ids = Some(vec![987]);
    gpu.gpus[0].external_compute_milli = None;
    gpu.gpus[0].compute_amount_unknown = Some("summed per-process use differs".into());
    gpu.gpus[0].utilization_percent = Some(100);
    evaluate(&cfg, &gpu, &base);
    gpu.gpus[0].external_compute_milli = Some(300);
    gpu.gpus[0].compute_amount_unknown = None;
    evaluate(&cfg, &gpu, &base);
    // A small floor that is not acknowledged.
    let mut small = config();
    small.ram.reserve_mib = 600;
    small.ram.reserve_percent = 0;
    evaluate(&small, &host(), &base);
}

/// Each code has one sentence, codes are unique `snake_case`, the catalog covers
/// the pinned codes, and `docs/guarantees.md` lists every code.
#[test]
fn the_catalog_has_one_sentence_per_stable_code() {
    let mut seen = std::collections::BTreeSet::new();
    for (code, text) in REASON_CODES {
        assert!(seen.insert(*code), "{code} twice");
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{code}"
        );
        assert!(
            text.ends_with('.') && !text.trim_end_matches('.').contains(". "),
            "{code}: {text}"
        );
        assert!(
            text.chars().next().unwrap().is_uppercase(),
            "{code}: {text}"
        );
    }
    for code in [
        "yield_step_lower_share",
        "yield_step_cap",
        "yield_step_pause",
        "yield_step_hint",
        "yield_step_drain",
        "yield_step_terminate",
        "external_growth",
        "own_over_target",
        "floor_breach",
        "envelope_breach",
        "unknown_measurement",
        "operator_drain",
        "step_reversed_contention_ended",
        "step_reversed_own_usage_fell",
        "step_reversed_floor_restored",
        "step_reversed_measurement_known",
        "step_reversed_envelope_restored",
        "gpu_compute_unknown_activity",
        "gpu_compute_amount_unknown",
        "gpu_compute_recently_inconsistent",
        "gpu_compute_measured",
        "mode_capped",
        "probing_demand",
        "watching_larger_demand",
        "satisfied_demand",
        "applied_duty_credit",
        "admission_draining",
        "admission_spool_exhausted",
        "admission_floor_breached",
        "admission_measurement_unknown",
        "admission_own_work_yielding",
        "admission_cooling_down",
        "admission_reclaim_backoff",
        "admission_envelope_exceeded",
        "small_floor",
        "reduced_margin",
        "relaxed_detection",
    ] {
        assert!(seen.contains(code), "{code} missing from the catalog");
    }
    // docs/guarantees.md carries the catalog row for row, so the two cannot drift.
    let guarantees = include_str!("../docs/guarantees.md");
    let table: Vec<&str> = guarantees
        .lines()
        .skip_while(|line| *line != "| Code | Meaning |")
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .collect();
    let expected: Vec<String> = REASON_CODES
        .iter()
        .map(|(code, text)| format!("| `{code}` | {text} |"))
        .collect();
    assert_eq!(
        table, expected,
        "docs/guarantees.md and REASON_CODES differ"
    );
}

/// The scripted runs above as a decision snapshot (tests/support/snapshot.rs).
#[test]
#[ignore = "decision snapshot: needs CEDEGRID_DECISION_SNAPSHOT"]
fn decision_snapshot() {
    snapshot::check(
        "observability",
        snapshot::scripted(
            "observability",
            &[
                (
                    "a_scripted_cpu_contention_run_is_shown_at_every_step",
                    a_scripted_cpu_contention_run_is_shown_at_every_step,
                ),
                (
                    "a_scripted_gpu_compute_run_is_shown_at_every_step",
                    a_scripted_gpu_compute_run_is_shown_at_every_step,
                ),
                (
                    "the_node_cooldown_names_the_legacy_flags_it_covers",
                    the_node_cooldown_names_the_legacy_flags_it_covers,
                ),
                (
                    "doctor_shows_the_ram_cap_where_the_leaf_allows_it",
                    doctor_shows_the_ram_cap_where_the_leaf_allows_it,
                ),
                (
                    "every_code_the_policy_emits_is_in_the_catalog",
                    every_code_the_policy_emits_is_in_the_catalog,
                ),
            ],
        ),
    );
}

/// `doctor` says how a run's descendants are found, and names the scan of `/proc` that stands
/// in on a kernel without children lists (FR-083, FR-084).
#[test]
fn doctor_names_how_descendants_are_found() {
    use cedegrid::explain::descendants_text;
    assert!(descendants_text(Some(true)).contains("children lists"));
    assert!(descendants_text(Some(false)).contains("scan of /proc"));
    assert!(descendants_text(None).contains("single-process contract"));
    assert_eq!(
        cedegrid::supervision::descendant_lists().is_some(),
        cfg!(target_os = "linux")
    );
}
