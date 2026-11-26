use cedegrid::config::{Config, RuntimeConfigKind, serialize_runtime};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn setup(directory: &Path) -> std::path::PathBuf {
    let config = Config {
        node_id: "test-node".into(),
        state_dir: "state".into(),
        ..Config::default()
    };
    let path = directory.join("node.toml");
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    path
}

fn success_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn validation_resolves_paths_against_config_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let result = success_json(run(&path, &["validate"]));
    assert_eq!(result["valid"], true);
    assert_eq!(
        Path::new(result["config"]["state_dir"].as_str().unwrap()),
        dir.path().canonicalize().unwrap().join("state")
    );
    assert!(!dir.path().join("state").exists());
}

/// `validate` and `doctor` show a GPU sample age derived from the interval, and say so; an
/// explicit value is shown as configured and listed nowhere as derived (FR-070).
#[cfg(unix)]
#[test]
fn validate_and_doctor_show_a_derived_sample_age() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.toml");
    let mut config = Config {
        node_id: "test-node".into(),
        state_dir: "state".into(),
        ..Config::default()
    };
    config.monitor.interval_ms = 3000;
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let result = success_json(run(&path, &["validate"]));
    let derived = &result["derived"]["gpu.process_sample_max_age_ms"];
    assert_eq!(derived["value"], 6000, "{result}");
    assert!(
        derived["rule"]
            .as_str()
            .unwrap()
            .contains("2 × monitor.interval_ms")
    );
    let doctor = success_json(run(&path, &["doctor"]));
    assert_eq!(
        doctor["derived"]["gpu.process_sample_max_age_ms"]["value"],
        6000
    );
    let text = String::from_utf8(run(&path, &["doctor", "--text"]).stdout).unwrap();
    assert!(
        text.contains("Derived settings") && text.contains("gpu.process_sample_max_age_ms = 6000"),
        "{text}"
    );
    config.gpu.process_sample_max_age_ms = Some(7000);
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let result = success_json(run(&path, &["validate"]));
    assert_eq!(result["config"]["gpu"]["process_sample_max_age_ms"], 7000);
    assert!(
        result["derived"]
            .as_object()
            .is_none_or(|map| map.is_empty()),
        "{result}"
    );
}

#[test]
fn invalid_config_and_missing_history_never_initialize_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let output = run(&path, &["history"]);
    assert!(!output.status.success());
    fs::write(
        &path,
        "config_version = 1\nnode_id = 'test-node'\n[monitor]\ninterval_ms = 0\n",
    )
    .unwrap();
    assert!(!run(&path, &["observe"]).status.success());
    assert!(!dir.path().join("state").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn doctor_separates_selected_profile_from_requested_role_without_creating_state() {
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let path = setup(dir.path());
    let mut config = Config::load(&path).unwrap();
    config.storage_profile = cedegrid::state::StorageProfile::BurstReplayDeleteExtra;
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let agent = success_json(run(&path, &["doctor", "--role", "agent"]));
    assert_eq!(agent["requested_role"], "agent");
    // Doctor shows how the legacy GPU key was read (FR-031).
    assert_eq!(agent["legacy_gpu"]["execution_mode"], "auto");
    assert!(agent["legacy_gpu"]["interpretation"].is_string());
    assert!(agent["config_warnings"].is_array());
    // Doctor shows each floor as it applies on this host.
    assert!(agent["floors"]["ram"]["effective_mib"].as_u64().unwrap() >= 512);
    assert!(agent["floors"]["ram"]["raised"].is_boolean());
    assert_eq!(agent["selected_profile_admitted"], true);
    assert_eq!(agent["role_admitted"], true);
    let coordinator = run(&path, &["doctor", "--role", "coordinator"]);
    assert!(!coordinator.status.success());
    let coordinator: Value = serde_json::from_slice(&coordinator.stdout).unwrap();
    assert_eq!(coordinator["selected_profile_admitted"], true);
    assert_eq!(coordinator["role_admitted"], false);
    assert!(!dir.path().join("state").exists());
}

/// `observe` and `doctor` show each resource's state, `held_back` and caution
/// options (T109, FR-038, FR-068, FR-069).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn observe_and_doctor_show_each_resource() {
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let path = setup(dir.path());
    let mut config = Config::load(&path).unwrap();
    // Neutral below a Performance cap, with a reduced margin as the caution option.
    config.contention.default_mode = cedegrid::contention::ContentionMode::Neutral;
    config.contention.max_mode = Some(cedegrid::contention::ContentionMode::Performance);
    config.contention.cpu.neutral_margin_percent = Some(3);
    config
        .contention
        .acknowledge_caution
        .insert("reduced_margin".into());
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let output = run(&path, &["observe", "--samples", "2", "--no-state"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let samples: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(samples.len(), 2);
    for sample in &samples {
        let decision = &sample["decision"];
        assert_eq!(decision["resources"]["cpu"]["mode"], "neutral");
        assert!(decision["resources"]["cpu"]["held_back"].is_object());
        assert!(decision["actions"].is_array());
        assert_eq!(
            decision["exceptions"],
            serde_json::json!([{"name": "reduced_margin", "marker": "caution"}])
        );
    }
    let doctor = success_json(run(&path, &["doctor"]));
    let cpu = &doctor["resources"]["cpu"];
    assert_eq!(cpu["mode"], "neutral");
    assert_eq!(cpu["effective_mode"], "neutral");
    assert!(cpu["admission"]["state"].is_string());
    assert!(cpu["held_back"]["capacity"].as_u64().unwrap() > 0);
    assert!(cpu["held_back"]["percent"].is_object());
    assert_eq!(doctor["max_mode"], "performance");
    assert_eq!(doctor["caution_options"][0]["name"], "reduced_margin");
    assert_eq!(doctor["caution_options"][0]["marker"], "caution");
    assert_eq!(
        doctor["caution_options"][0]["settings"][0]["key"],
        "contention.cpu.neutral_margin_percent"
    );
    // Tracked takes no CPU rate cap; it pauses whole tasks (FR-089).
    let cpu_steps = doctor["available_steps"]["cpu"].as_array().unwrap();
    assert!(cpu_steps.contains(&json!("pause")), "{cpu_steps:?}");
    assert!(!cpu_steps.contains(&json!("cap")), "{cpu_steps:?}");
    assert!(doctor["floors"]["ram"]["effective_mib"].is_u64());
    assert!(doctor["legacy_gpu"]["interpretation"].is_string());
    let text = run(&path, &["doctor", "--text"]);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8_lossy(&text.stdout);
    for expected in [
        "cpu: Neutral",
        "held back:",
        "Floors",
        "RAM floor",
        "Caution options",
        "[caution] reduced_margin: contention.cpu.neutral_margin_percent = 3",
        "Available steps",
        "Legacy GPU keys",
    ] {
        assert!(text.contains(expected), "{expected} in:\n{text}");
    }
    assert!(!dir.path().join("state").exists());
}

/// A file without `default_mode` now runs in Performance, and `doctor` says so once
/// (T253, FR-030).
#[cfg(unix)]
#[test]
fn doctor_reports_the_new_default_mode_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let text = fs::read_to_string(&path).unwrap();
    let unset: String = text
        .lines()
        .filter(|line| !line.starts_with("default_mode"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&path, unset).unwrap();
    let doctor = success_json(run(&path, &["doctor"]));
    assert_eq!(doctor["resources"]["cpu"]["mode"], "performance");
    let notices: Vec<&Value> = doctor["config_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|warning| {
            warning
                .as_str()
                .is_some_and(|w| w.starts_with("default_mode_changed"))
        })
        .collect();
    assert_eq!(notices.len(), 1, "{doctor}");
    let mut config = Config::load(&path).unwrap();
    config.contention.default_mode = cedegrid::contention::ContentionMode::Neutral;
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let explicit = success_json(run(&path, &["doctor"]));
    assert_eq!(explicit["resources"]["cpu"]["mode"], "neutral");
    assert!(
        !explicit["config_warnings"]
            .to_string()
            .contains("default_mode_changed"),
        "{explicit}"
    );
}

#[test]
fn observation_without_state_is_read_only_and_honest_about_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let value = success_json(run(&path, &["observe", "--samples", "1", "--no-state"]));
    assert!(value["observation_id"].is_null());
    assert_eq!(value["decision"]["observe_only"], true);
    assert_eq!(value["decision"]["would_drain"], serde_json::json!([]));
    // 004 FR-016: the list of unprotected runs is always present; without state it is empty.
    assert_eq!(value["unprotected"], serde_json::json!([]));
    for capability in value["snapshot"]["capabilities"]
        .as_object()
        .unwrap()
        .values()
    {
        assert_eq!(capability["enforced"], false);
    }
    assert!(!dir.path().join("state").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn observation_is_persisted_and_history_round_trips() {
    // Durable integration requires a local volume; /tmp may be volatile even
    // when the checkout is on a supported filesystem.
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let path = setup(dir.path());
    let observed = success_json(run(&path, &["observe"]));
    assert!(observed["observation_id"].as_i64().unwrap() > 0);
    assert_eq!(observed["unprotected"], serde_json::json!([]));
    let history = success_json(run(&path, &["history"]));
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert!(dir.path().join("state/state.sqlite3").is_file());
}

#[test]
fn synthetic_replay_explains_pressure_without_affecting_live_system() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("node.toml");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/node.toml"),
        &config,
    )
    .unwrap();
    let trace = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/gpu-pressure.json");
    let output = run(&config, &["replay", trace.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 9);
    assert_eq!(rows[1]["decision"]["expansion_allowed"], true);
    assert_eq!(rows[2]["decision"]["expansion_allowed"], false);
    assert!(
        !rows[2]["decision"]["would_drain"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The external job of 31.5 s has exited, so its memory peak no longer holds
    // GPU admission back (FR-060).
    assert_eq!(rows[4]["decision"]["expansion_allowed"], true);
    let gpu_headroom = |row: usize| {
        rows[row]["decision"]["admission_headroom"]["gpu_memory_mib"]["example-gpu"].as_u64()
    };
    // A live external job that shrank keeps its peak: GPU headroom stays at zero.
    assert_eq!(gpu_headroom(7), Some(0));
    // Once it exits, admission reopens.
    assert!(gpu_headroom(8).is_some_and(|mib| mib > 0));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn legacy_run_subcommand_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .arg("run")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn disabled_supervision_refuses_before_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let config = setup(dir.path());
    let job = dir.path().join("job.json");
    fs::write(&job, "{}").unwrap();
    let output = run(&config, &["supervise", job.to_str().unwrap()]);
    assert!(!output.status.success());
    let expected = if cfg!(windows) {
        "ERR_CEDEGRID_UNSUPPORTED_PLATFORM"
    } else {
        "execution is disabled"
    };
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    assert!(!dir.path().join("state").exists());
}

#[cfg(windows)]
#[test]
fn windows_execution_and_recovery_refuse_before_reading_inputs_or_mutating_state() {
    let dir = tempfile::tempdir().unwrap();
    let missing_config = dir.path().join("missing.toml");
    for args in [
        vec!["agent", "--deployment", "missing.toml"],
        vec!["coordinator", "--deployment", "missing.toml"],
        vec!["supervise", "missing.json"],
        vec!["reconcile"],
        vec!["observe"],
        vec!["history"],
        vec!["executions"],
        vec!["backup", "--state-dir", "state", "--destination", "backup"],
        vec![
            "restore",
            "--snapshot",
            "backup",
            "--destination",
            "state",
            "--confirm-source-stopped",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .current_dir(dir.path())
            .arg("--config")
            .arg(&missing_config)
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ERR_CEDEGRID_UNSUPPORTED_PLATFORM"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    for command in ["__worker-gate", "__assignment-supervisor"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .current_dir(dir.path())
            .arg(command)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ERR_CEDEGRID_UNSUPPORTED_PLATFORM")
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn execution_setup(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use cedegrid::{
        config::{CpuConfig, ExecutionConfig, GpuConfig, MonitorConfig, NodeMode, RamConfig},
        kernel::KernelConfig,
    };
    let config = Config {
        node_id: "test-node".into(),
        node_mode: NodeMode::Guaranteed,
        state_dir: "state".into(),
        execution: ExecutionConfig {
            enabled: true,
            admission_timeout_ms: 5_000,
            ..Default::default()
        },
        monitor: MonitorConfig { interval_ms: 20 },
        cpu: CpuConfig {
            reserve_physical_cores: 0,
            ..Default::default()
        },
        ram: RamConfig {
            reserve_mib: 0,
            reserve_percent: 0,
            ..Default::default()
        },
        gpu: GpuConfig {
            scale_up_cooldown_ms: Some(0),
            ..Default::default()
        },
        contention: cedegrid::config::ContentionConfig {
            cpu: cedegrid::config::ResourceContentionConfig {
                scale_up_cooldown_ms: Some(0),
                ..Default::default()
            },
            ram: cedegrid::config::ResourceContentionConfig {
                scale_up_cooldown_ms: Some(0),
                ..Default::default()
            },
            ..Default::default()
        },
        kernel: KernelConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let config_path = directory.join("node.toml");
    fs::write(
        &config_path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let job_path = directory.join("job.json");
    let job = serde_json::json!({
        "task_id": "cli-task", "assignment_id": "assignment-one",
        "argv": ["/bin/sh", "-c", "printf completed > marker"],
        "cwd": directory.canonicalize().unwrap(),
        "resources": { "cpu_millicores": 1, "ram_mib": 1 },
        "class": "guaranteed", "replay_safe": true, "no_escape": true,
        "single_process": true, "allow_fallback": true
    });
    fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
    (config_path, job_path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn enabled_supervision_commits_exit_receipt_and_releases_capacity() {
    use cedegrid::state::{StateStore, TaskStatus};
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let (config, job) = execution_setup(dir.path());
    let outcome = success_json(run(&config, &["supervise", job.to_str().unwrap()]));
    assert_eq!(outcome["record"]["phase"], "released");
    assert_eq!(outcome["exit_code"], 0);
    assert_eq!(
        fs::read_to_string(dir.path().join("marker")).unwrap(),
        "completed"
    );
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let task = store.task("cli-task").unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(
        task.receipt_hash.as_deref(),
        Some("local-exit:0:assignment-one:1")
    );
    assert!(store.execution_allocations().unwrap().is_empty());
    let records = success_json(run(&config, &["executions"]));
    assert_eq!(records[0]["phase"], "released");
}

/// Standalone `supervise` drains its run on SIGINT, as an agent's supervisor does: the
/// workload, in a session of its own, no longer gets the terminal's Ctrl-C, so the supervisor
/// passes it on as a drain instead of leaving the run behind (FR-078, R41).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn standalone_supervise_drains_its_run_on_sigint() {
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let (config_path, job_path) = execution_setup(dir.path());
    let mut config: Config = cedegrid::config::parse_runtime(
        &fs::read_to_string(&config_path).unwrap(),
        &config_path,
        RuntimeConfigKind::Node,
    )
    .unwrap();
    config.lifecycle.drain_timeout_ms = 200;
    config.lifecycle.term_grace_ms = 2_000;
    fs::write(
        &config_path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let mut job: Value = serde_json::from_slice(&fs::read(&job_path).unwrap()).unwrap();
    job["argv"] = serde_json::json!([
        "/bin/sh",
        "-c",
        "trap 'echo drained > got-term; exit 0' TERM; echo $$ > workload; while :; do sleep 0.05; done"
    ]);
    fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .arg("--config")
        .arg(&config_path)
        .args(["supervise", job_path.to_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let workload = dir.path().join("workload");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !workload.exists() {
        assert!(
            std::time::Instant::now() < end,
            "the workload never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // SAFETY: this test's own child, the CLI it just spawned.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > end {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let pid: i32 = fs::read_to_string(&workload)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: a probe of the workload this test's run started.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        // SAFETY: test cleanup of the workload this test's run started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    if status.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    assert!(status.is_some_and(|s| s.success()), "{status:?}");
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["yielded"], true, "{outcome}");
    assert!(dir.path().join("got-term").exists());
    assert!(!alive, "the workload outlived its supervisor");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn missing_required_control_never_executes_and_safe_resubmission_uses_new_attempt() {
    use cedegrid::state::{StateStore, TaskStatus};
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let (config, job) = execution_setup(dir.path());
    let mut request: Value = serde_json::from_slice(&fs::read(&job).unwrap()).unwrap();
    request["required_controls"] = serde_json::json!(["cpu.max"]);
    fs::write(&job, serde_json::to_vec(&request).unwrap()).unwrap();
    let failure = run(&config, &["supervise", job.to_str().unwrap()]);
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("cpu.max"));
    assert!(!dir.path().join("marker").exists());
    {
        let store = StateStore::open(&dir.path().join("state")).unwrap();
        assert_eq!(store.status("cli-task").unwrap(), TaskStatus::Queued);
        assert!(store.execution_allocations().unwrap().is_empty());
        assert_eq!(store.executions().unwrap().len(), 1);
    }
    request["required_controls"] = serde_json::json!([]);
    request["assignment_id"] = "assignment-two".into();
    fs::write(&job, serde_json::to_vec(&request).unwrap()).unwrap();
    let outcome = success_json(run(&config, &["supervise", job.to_str().unwrap()]));
    assert_eq!(outcome["record"]["generation"], 2);
    assert_eq!(outcome["record"]["assignment_id"], "assignment-two");
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    assert_eq!(store.status("cli-task").unwrap(), TaskStatus::Completed);
    assert_eq!(
        store.task("cli-task").unwrap().receipt_hash.as_deref(),
        Some("local-exit:0:assignment-two:2")
    );
    assert_eq!(store.executions().unwrap().len(), 2);
    assert!(store.execution_allocations().unwrap().is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn local_gpu_execution_refuses_before_state_or_workload_creation() {
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let (config, job) = execution_setup(dir.path());
    let mut request: Value = serde_json::from_slice(&fs::read(&job).unwrap()).unwrap();
    request["resources"]["gpu_memory_mib"] = serde_json::json!({"GPU-test": 64});
    fs::write(&job, serde_json::to_vec(&request).unwrap()).unwrap();
    let failure = run(&config, &["supervise", job.to_str().unwrap()]);
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("continuously observing agent"));
    assert!(!dir.path().join("state").exists());
    assert!(!dir.path().join("marker").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn configured_rollback_profile_is_used_through_execution_history_and_restart() {
    use cedegrid::state::{StateStore, StorageProfile};
    let dir = StateDir(tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap());
    let (config_path, job) = execution_setup(dir.path());
    let mut config = Config::load(&config_path).unwrap();
    config.storage_profile = StorageProfile::DeleteExtra;
    fs::write(
        &config_path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let outcome = success_json(run(&config_path, &["supervise", job.to_str().unwrap()]));
    assert_eq!(outcome["record"]["phase"], "released");
    let rows = success_json(run(&config_path, &["executions"]));
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let store =
        StateStore::open_with_profile(&dir.path().join("state"), StorageProfile::DeleteExtra)
            .unwrap();
    assert_eq!(store.storage_profile(), StorageProfile::DeleteExtra);
    assert!(!dir.path().join("state/state.sqlite3-wal").exists());
    drop(store);
    config.storage_profile = StorageProfile::WalFull;
    fs::write(
        &config_path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    assert!(!run(&config_path, &["executions"]).status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("marker")).unwrap(),
        "completed"
    );
}

/// Replay reads recorded snapshots of schemas 1-3 and refuses a newer one (FR-033).
#[test]
fn replay_reads_snapshot_schemas_one_to_three() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("node.toml");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/node.toml"),
        &config,
    )
    .unwrap();
    let trace: Vec<Value> = serde_json::from_slice(
        &fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/gpu-pressure.json"))
            .unwrap(),
    )
    .unwrap();
    let frames = |schema: u64| -> Vec<Value> {
        let mut frame = trace[0].clone();
        frame["snapshot"]["schema_version"] = json!(schema);
        vec![frame]
    };
    for schema in [1, 2, 3, 4] {
        let path = dir.path().join(format!("schema-{schema}.json"));
        fs::write(&path, serde_json::to_vec(&frames(schema)).unwrap()).unwrap();
        let output = run(&config, &["replay", path.to_str().unwrap()]);
        assert_eq!(
            output.status.success(),
            schema <= 3,
            "schema {schema}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// `cedegrid setup` without a terminal (T099, contracts/setup-cli.md).
mod setup_non_interactive {
    use super::*;
    use std::process::Stdio;

    /// Setup preselects Performance, the default, and writes it without an
    /// acknowledgement (FR-030, FR-051).
    #[cfg(unix)]
    #[test]
    fn setup_preselects_performance_without_an_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "node",
                "--default-mode",
                "performance",
            ],
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let fresh = dir.path().join("fresh.toml");
        let output = setup_cmd(&fresh, &["--non-interactive", "--json", "--role", "node"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let config = cedegrid::config::Config::load(&fresh).unwrap();
        assert_eq!(
            config
                .contention
                .mode(cedegrid::contention::ResourceKind::Cpu),
            cedegrid::contention::ContentionMode::Performance
        );
    }

    fn setup_cmd(config: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .arg("--config")
            .arg(config)
            .arg("setup")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stdout)))
    }

    fn error_code(output: &Output) -> String {
        json(output)["error"]["code"].as_str().unwrap().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn exit_codes_follow_the_contract_and_nothing_is_written_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        // 2: a new file needs a role.
        let output = setup_cmd(&path, &["--non-interactive", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(error_code(&output), "ERR_CEDEGRID_SETUP_MISSING_CHOICE");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--role")
        );
        assert!(!path.exists());
        // 3: a caution option without its acknowledgement.
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "node",
                "--ram-floor-mib",
                "600",
            ],
        );
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(error_code(&output), "ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--acknowledge-caution small_floor")
        );
        assert!(!path.exists());
        // 4: a result that fails validation, here Neutral's margin below Performance's.
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "node",
                "--margin",
                "cpu.neutral=1",
                "--acknowledge-caution",
                "reduced_margin",
            ],
        );
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(error_code(&output), "ERR_CEDEGRID_CONFIG");
        assert!(!path.exists());
        // 0: acknowledged, written and valid.
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "node",
                "--default-mode",
                "neutral",
                "--ram-floor-mib",
                "600",
                "--acknowledge-caution",
                "small_floor",
            ],
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.contention.default_mode,
            cedegrid::contention::ContentionMode::Neutral
        );
        // The same choices again change nothing.
        let output = setup_cmd(
            &path,
            &["--non-interactive", "--json", "--default-mode", "neutral"],
        );
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(json(&output)["written"], false);
    }

    #[cfg(unix)]
    #[test]
    fn margins_are_set_for_every_mode_and_validated_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--role",
                "node",
                "--margin",
                "cpu.yield_first=20",
                "--margin",
                "cpu.neutral=8",
                "--margin",
                "cpu.performance=3",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let thresholds = Config::load(&path)
            .unwrap()
            .contention
            .thresholds(cedegrid::contention::ResourceKind::Cpu);
        assert_eq!(
            (
                thresholds.yield_first_margin_percent,
                thresholds.neutral_margin_percent,
                thresholds.performance_margin_percent
            ),
            (20, 8, 3)
        );
        // Yield First below Neutral fails validation and writes nothing.
        let before = fs::read(&path).unwrap();
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--margin",
                "cpu.yield_first=6",
                "--margin",
                "cpu.neutral=7",
            ],
        );
        assert_eq!(
            output.status.code(),
            Some(4),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn json_output_names_the_path_choices_and_caution_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "node",
                "--max-mode",
                "performance",
                "--ram-floor-mib",
                "600",
                "--acknowledge-caution",
                "small_floor",
            ],
        );
        let result = json(&output);
        assert_eq!(result["written"], true);
        assert_eq!(
            Path::new(result["path"].as_str().unwrap()),
            path.canonicalize().unwrap()
        );
        assert_eq!(result["choices"]["contention.max_mode"], "performance");
        // Performance is no caution option; the small floor is.
        assert_eq!(
            result["exceptions"],
            json!([{"name": "small_floor", "marker": "caution"}])
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_never_prompts_or_blocks_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut child = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .arg("--config")
            .arg(&path)
            .args(["setup", "--role", "node"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        // stdin stays open and empty: a prompt would block.
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "setup blocked"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(status.success());
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn a_missing_config_on_a_non_tty_never_offers_setup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let output = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .arg("--config")
            .arg(&path)
            .arg("validate")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!text.to_lowercase().contains("start setup"), "{text}");
        assert!(!path.exists());
    }

    /// Memory margins exist for Yield First only.
    #[cfg(unix)]
    #[test]
    fn memory_neutral_and_performance_margins_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        for margin in [
            "ram.neutral=5",
            "ram.performance=2",
            "gpu_memory.neutral=5",
            "gpu_memory.performance=2",
        ] {
            let output = setup_cmd(
                &path,
                &[
                    "--non-interactive",
                    "--json",
                    "--role",
                    "node",
                    "--margin",
                    margin,
                ],
            );
            assert_eq!(output.status.code(), Some(2), "{margin}");
            assert_eq!(error_code(&output), "ERR_CEDEGRID_ARGUMENT");
            assert!(
                json(&output)["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("keeps only the floor under Neutral and Performance"),
                "{margin}"
            );
            assert!(!path.exists());
        }
        assert!(
            setup_cmd(
                &path,
                &[
                    "--non-interactive",
                    "--role",
                    "node",
                    "--margin",
                    "ram.yield_first=30"
                ],
            )
            .status
            .success()
        );
    }

    /// Client and coordinator files take their connection settings; node choices
    /// are refused for them (T107).
    #[cfg(unix)]
    #[test]
    fn client_and_coordinator_roles_take_connection_settings_only() {
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("client.toml");
        let output = setup_cmd(
            &client,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "client",
                "--endpoint",
                "https://192.0.2.1:7443",
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--ca-cert")
        );
        let output = setup_cmd(
            &client,
            &[
                "--non-interactive",
                "--json",
                "--role",
                "client",
                "--default-mode",
                "neutral",
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(error_code(&output), "ERR_CEDEGRID_ARGUMENT");
        let tls = [
            "--ca-cert",
            "tls/ca.pem",
            "--certificate",
            "tls/operator.pem",
            "--private-key",
            "tls/operator.key",
        ];
        let mut args = vec![
            "--non-interactive",
            "--role",
            "client",
            "--endpoint",
            "https://192.0.2.1:7443",
        ];
        args.extend(tls);
        assert!(setup_cmd(&client, &args).status.success());
        let loaded: cedegrid::config::RuntimeClientConfig =
            cedegrid::config::load_runtime(&client, RuntimeConfigKind::Client).unwrap();
        assert_eq!(loaded.endpoint, "https://192.0.2.1:7443");
        let coordinator = dir.path().join("coordinator.toml");
        let mut args = vec![
            "--non-interactive",
            "--role",
            "coordinator",
            "--listen",
            "127.0.0.1:7443",
            "--state-dir",
            "state",
            "--operator-fingerprint",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ];
        args.extend(tls);
        let output = setup_cmd(&coordinator, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _: cedegrid::protocol::CoordinatorConfig =
            cedegrid::config::load_runtime(&coordinator, RuntimeConfigKind::Coordinator).unwrap();
        // A file keeps its role.
        let output = setup_cmd(
            &coordinator,
            &["--non-interactive", "--json", "--role", "node"],
        );
        assert_eq!(output.status.code(), Some(2));
    }

    #[cfg(unix)]
    #[test]
    fn unmanaged_settings_and_comments_are_preserved_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let original = "# Operator notes stay.\nconfig_version = 1\nnode_id = \"node-a\" # inline\n\n[monitor]\ninterval_ms = 500 # half a second\n\n[ram]\n# the fixed part of the floor\nreserve_mib = 8192 # keep\nreserve_percent = 5\n\n[contention]\ndefault_mode = \"neutral\" # chosen in 2026\n\n[contention.ram]\nhint_deadline_s = 5 # unmanaged here\n";
        fs::write(&path, original).unwrap();
        let output = setup_cmd(
            &path,
            &[
                "--non-interactive",
                "--default-mode",
                "yield_first",
                "--ram-floor-mib",
                "16384",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let written = fs::read_to_string(&path).unwrap();
        let expected = original
            .replace(
                "default_mode = \"neutral\" # chosen in 2026",
                "default_mode = \"yield_first\" # chosen in 2026",
            )
            .replace("reserve_mib = 8192 # keep", "reserve_mib = 16384 # keep");
        assert_eq!(written, expected);
    }
}

/// The terminal interface, driven through a pseudo-terminal (T100).
#[cfg(unix)]
mod setup_terminal {
    use super::*;
    use std::io::{Read, Write as _};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // Interface rows (src/setup.rs `node_fields`): Up from the top wraps to
    // Cancel, then Review and write.
    const DEFAULT_MODE: usize = 1;
    const MAX_MODE: usize = 6;
    const PLAIN_PRIORITY: usize = 7;
    const RAM_FLOOR: usize = 8;
    const CPU_NEUTRAL_MARGIN: usize = 11;
    const UP: &[u8] = b"\x1b[A";
    const DOWN: &[u8] = b"\x1b[B";

    struct Pty {
        master: std::fs::File,
        /// Kept open so the terminal outlives the session for the checks.
        _slave: std::fs::File,
        /// The terminal's settings before setup started.
        initial: libc::termios,
        child: std::process::Child,
        output: Arc<Mutex<Vec<u8>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    fn termios(file: &std::fs::File) -> libc::termios {
        let mut value: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(file.as_raw_fd(), &mut value) }, 0);
        value
    }

    impl Pty {
        fn spawn(config: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
            let (mut master, mut slave) = (0, 0);
            let size = libc::winsize {
                ws_row: 60,
                ws_col: 140,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let opened = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut::<libc::termios>() as _,
                    &size as *const libc::winsize as _,
                )
            };
            assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
            let (master, slave) = unsafe {
                (
                    std::fs::File::from_raw_fd(master),
                    std::fs::File::from_raw_fd(slave),
                )
            };
            let initial = termios(&master);
            let mut command = Command::new(env!("CARGO_BIN_EXE_cedegrid"));
            command
                .arg("--config")
                .arg(config)
                .args(args)
                .env("TERM", "xterm")
                .env_remove("NO_COLOR")
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(slave.try_clone().unwrap());
            for (key, value) in env {
                command.env(key, value);
            }
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                    Ok(())
                });
            }
            let child = command.spawn().unwrap();
            drop(command);
            let output = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut reading = master.try_clone().unwrap();
            let (sink, halt) = (output.clone(), stop.clone());
            let reader = std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while !halt.load(std::sync::atomic::Ordering::SeqCst) {
                    let mut poll = libc::pollfd {
                        fd: reading.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut poll, 1, 50) } <= 0 {
                        continue;
                    }
                    match reading.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => sink.lock().unwrap().extend_from_slice(&buffer[..n]),
                    }
                }
            });
            Pty {
                master,
                _slave: slave,
                initial,
                child,
                output,
                stop,
                reader: Some(reader),
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
        }

        fn wait_for(&self, needle: &str) {
            let started = Instant::now();
            while !self.text().contains(needle) {
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "waited for {needle:?} in:\n{}",
                    self.text()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Send one key and give the interface time to redraw.
        fn key(&mut self, bytes: &[u8]) {
            let before = self.output.lock().unwrap().len();
            self.master.write_all(bytes).unwrap();
            self.master.flush().unwrap();
            let started = Instant::now();
            while self.output.lock().unwrap().len() == before
                && started.elapsed() < Duration::from_secs(5)
                && self.child.try_wait().unwrap().is_none()
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(30));
        }

        fn keys(&mut self, bytes: &[u8], times: usize) {
            for _ in 0..times {
                self.key(bytes);
            }
        }

        fn finish(mut self) -> (std::process::ExitStatus, String, libc::termios) {
            let started = Instant::now();
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "setup did not exit:\n{}",
                    self.text()
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            std::thread::sleep(Duration::from_millis(100));
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(reader) = self.reader.take() {
                reader.join().unwrap();
            }
            // The master shares the terminal's settings and outlives the session.
            let settings = termios(&self.master);
            (status, self.text(), settings)
        }
    }

    fn interface(config: &Path, env: &[(&str, &str)]) -> Pty {
        let pty = Pty::spawn(config, &["setup", "--role", "node"], env);
        pty.wait_for("Review and write");
        pty
    }

    /// Open the row `index` levels below the top, from the top.
    fn open(pty: &mut Pty, index: usize) {
        pty.keys(DOWN, index);
        pty.key(b"\r");
    }

    /// From the main screen, with the selection on `from`, review and write.
    fn write_from(pty: &mut Pty, from: usize) {
        pty.keys(UP, from + 2);
        pty.key(b"\r");
        pty.wait_for("Review before writing");
        pty.key(b"\r");
    }

    #[test]
    fn arrows_and_enter_choose_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut pty = interface(&path, &[]);
        open(&mut pty, DEFAULT_MODE);
        pty.wait_for("Holds the fair share");
        // Options: Yield First, Neutral, Performance, Aggressive; the highlight starts on
        // Performance, the current value. Space does nothing; Enter chooses.
        pty.keys(UP, 2);
        pty.key(b" ");
        pty.key(b"\r");
        write_from(&mut pty, DEFAULT_MODE);
        let (status, text, _) = pty.finish();
        assert!(status.success(), "{text}");
        assert_eq!(
            Config::load(&path).unwrap().contention.default_mode,
            cedegrid::contention::ContentionMode::YieldFirst
        );
    }

    #[test]
    fn a_caution_choice_needs_its_own_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut pty = interface(&path, &[]);
        // Ascending options; 768 MiB, near the minimum, is four above the current 8192.
        open(&mut pty, RAM_FLOOR);
        pty.keys(UP, 4);
        pty.key(b"\r");
        pty.wait_for("Confirm a caution option");
        pty.wait_for("Less headroom is kept");
        // Declining keeps the previous value.
        pty.key(b"\r");
        pty.wait_for("Review and write");
        let text = pty.text();
        let row = text[text.rfind("RAM floor").unwrap()..]
            .lines()
            .next()
            .unwrap();
        assert!(row.contains("8192"), "{row}");
        assert!(!row.contains("[caution]"), "{row}");
        pty.key(b"\r");
        pty.keys(UP, 4);
        pty.key(b"\r");
        pty.wait_for("Confirm a caution option");
        pty.key(DOWN);
        pty.key(b"\r");
        write_from(&mut pty, RAM_FLOOR);
        let (status, text, _) = pty.finish();
        assert!(status.success(), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.ram.reserve_mib, 768);
        assert!(
            config
                .contention
                .acknowledge_caution
                .contains("small_floor")
        );
    }

    #[test]
    fn esc_q_and_ctrl_c_write_nothing_and_restore_the_terminal() {
        for key in [&b"\x1b"[..], b"q", b"\x03"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("node.toml");
            fs::write(&path, "config_version = 1\n").unwrap();
            let mut pty = interface(&path, &[]);
            let before = pty.initial;
            // A change made before cancelling is not written either.
            open(&mut pty, DEFAULT_MODE);
            pty.key(UP);
            pty.key(b"\r");
            pty.key(key);
            let (status, text, after) = pty.finish();
            assert_eq!(status.code(), Some(130), "{key:?}: {text}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "config_version = 1\n");
            assert_eq!(after.c_lflag, before.c_lflag, "{key:?}");
            assert_eq!(after.c_iflag, before.c_iflag, "{key:?}");
            assert!(after.c_lflag & libc::ICANON != 0 && after.c_lflag & libc::ECHO != 0);
        }
    }

    #[test]
    fn sigterm_restores_the_terminal_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut pty = interface(&path, &[]);
        let before = pty.initial;
        open(&mut pty, DEFAULT_MODE);
        assert_eq!(
            unsafe { libc::kill(pty.child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        let (status, text, after) = pty.finish();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{text}");
        assert!(!path.exists());
        assert_eq!(after.c_lflag, before.c_lflag);
        assert_eq!(after.c_iflag, before.c_iflag);
    }

    #[test]
    fn no_color_writes_no_escape_sequence_and_keeps_caution_labels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut pty = interface(&path, &[("NO_COLOR", "1")]);
        // A floor near its minimum is marked; Performance, the default, is not.
        open(&mut pty, RAM_FLOOR);
        pty.wait_for("[caution]");
        pty.key(b"q");
        let (_, text, _) = pty.finish();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("[caution]"));
        // Memory keeps only the floor under Neutral and Performance.
        assert!(text.contains("RAM margin, Yield First"));
        for row in [
            "RAM margin, Neutral",
            "RAM margin, Performance",
            "GPU memory margin, Neutral",
            "GPU memory margin, Performance",
        ] {
            assert!(!text.contains(row), "{row}");
        }
        // With color the marker is yellow.
        let mut pty = interface(&path, &[]);
        open(&mut pty, RAM_FLOOR);
        pty.wait_for("\x1b[33m[caution]\x1b[0m");
        pty.key(b"q");
        pty.finish();
    }

    /// 004 FR-007, FR-008: raising the cap to Aggressive in the interface takes its own caution
    /// confirmation and records `aggressive_mode`.
    #[test]
    fn the_interface_raises_the_cap_to_aggressive_only_after_its_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let mut pty = interface(&path, &[]);
        open(&mut pty, MAX_MODE);
        pty.keys(DOWN, 2);
        pty.key(b"\r");
        pty.wait_for("Confirm a caution option");
        pty.wait_for("record aggressive_mode");
        // No is preselected; choose Yes.
        pty.key(DOWN);
        pty.key(b"\r");
        write_from(&mut pty, MAX_MODE);
        let (status, text, _) = pty.finish();
        assert!(status.success(), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.contention.max_mode,
            Some(cedegrid::contention::ContentionMode::Aggressive)
        );
        assert!(
            config
                .contention
                .acknowledge_caution
                .contains("aggressive_mode")
        );
    }

    /// SC-011: the interface and the flags write byte-identical files. On a terminal, priority
    /// over plain users starts on Yes where the file does not set it (006 FR-025).
    #[test]
    fn the_interface_and_flags_write_identical_files() {
        let start = "# Operator notes stay.\nconfig_version = 1\nnode_id = \"node-a\"\n\n[ram]\nreserve_mib = 8192 # fixed part\n";
        type Steps = fn(&mut Pty);
        let cases: [(&str, Option<&str>, Steps, &[&str]); 5] = [
            (
                "defaults",
                None,
                |pty| write_from(pty, 0),
                &["--priority-over-plain"],
            ),
            (
                "priority declined",
                None,
                |pty| {
                    // Options: Yes (preselected), No.
                    open(pty, PLAIN_PRIORITY);
                    pty.key(DOWN);
                    pty.key(b"\r");
                    write_from(pty, PLAIN_PRIORITY);
                },
                &["--no-priority-over-plain"],
            ),
            (
                "performance cap",
                Some(start),
                |pty| {
                    // Options: no explicit cap (current), Performance, Aggressive; Performance
                    // needs no confirmation.
                    open(pty, MAX_MODE);
                    pty.key(DOWN);
                    pty.key(b"\r");
                    write_from(pty, MAX_MODE);
                },
                &["--max-mode", "performance", "--priority-over-plain"],
            ),
            (
                "floor change",
                Some(start),
                |pty| {
                    // Ascending options; 16 384 follows the current 8192.
                    open(pty, RAM_FLOOR);
                    pty.key(DOWN);
                    pty.key(b"\r");
                    write_from(pty, RAM_FLOOR);
                },
                &["--ram-floor-mib", "16384", "--priority-over-plain"],
            ),
            (
                "margin change",
                Some(start),
                |pty| {
                    // Descending options; 7 precedes the default 5.
                    open(pty, CPU_NEUTRAL_MARGIN);
                    pty.key(UP);
                    pty.key(b"\r");
                    write_from(pty, CPU_NEUTRAL_MARGIN);
                },
                &["--margin", "cpu.neutral=7", "--priority-over-plain"],
            ),
        ];
        for (name, initial, steps, flags) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (by_keys, by_flags) = (dir.path().join("keys.toml"), dir.path().join("flags.toml"));
            if let Some(initial) = initial {
                fs::write(&by_keys, initial).unwrap();
                fs::write(&by_flags, initial).unwrap();
            }
            let mut pty = interface(&by_keys, &[]);
            steps(&mut pty);
            let (status, text, _) = pty.finish();
            assert!(status.success(), "{name}: {text}");
            let mut args = vec!["setup", "--non-interactive", "--role", "node"];
            args.extend_from_slice(flags);
            let output = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
                .arg("--config")
                .arg(&by_flags)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{name}: {output:?}");
            let (keys, flags) = (fs::read(&by_keys).unwrap(), fs::read(&by_flags).unwrap());
            assert_eq!(
                String::from_utf8_lossy(&keys),
                String::from_utf8_lossy(&flags),
                "{name}"
            );
            if let Some(initial) = initial {
                assert_ne!(keys, initial.as_bytes(), "{name} changed nothing");
            }
        }
    }

    /// 004 FR-013, 006 FR-015: the allowance turns on only on a terminal, after the numbers, the
    /// raise this host offers, a yes and the exact phrase, under the highest warning label.
    #[test]
    fn the_allowance_needs_a_yes_and_the_exact_phrase_on_a_terminal() {
        use cedegrid::state::{Allowance, StateStore, read_allowance};
        for (answer, phrase, on) in [
            ("y", "slow other users' compute on this host", true),
            ("y", "slow other users", false),
            ("n", "", false),
            ("", "", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = setup(dir.path());
            let state = dir.path().join("state");
            drop(StateStore::open(&state).unwrap());
            let mut pty = Pty::spawn(&path, &["setup", "--allow-unprotected"], &[]);
            pty.wait_for("Allow unprotected on this host? [y/n]");
            pty.key(format!("{answer}\r").as_bytes());
            if answer == "y" {
                pty.wait_for("to confirm:");
                pty.key(format!("{phrase}\r").as_bytes());
            }
            let (status, text, _) = pty.finish();
            assert!(status.success(), "{text}");
            assert!(text.contains("Other users active now"), "{text}");
            assert!(text.contains("[highest warning]"), "{text}");
            assert!(text.contains("Raise on this host"), "{text}");
            // No slowdown figure: setup cannot know how the kernel will split CPU time.
            assert!(!text.contains("slower"), "{text}");
            let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                words.contains("Memory floors and OOM handling do not change."),
                "{text}"
            );
            assert_eq!(
                matches!(read_allowance(&state), Allowance::On { .. }),
                on,
                "{answer:?} {phrase:?}: {text}"
            );
            // Each outcome is one line: the result, or the cancel and why.
            let last = text
                .trim_end()
                .lines()
                .last()
                .unwrap_or_default()
                .to_owned();
            if on {
                assert!(text.contains("is now allowed on this host"), "{text}");
            } else {
                assert!(last.starts_with("Cancelled"), "{last}");
                assert!(last.ends_with("The allowance stays off."), "{last}");
                assert!(!text.contains("allowance: off"), "{text}");
            }
        }
    }

    /// A terminal that cannot move the cursor gets no second, typed interface: setup stops,
    /// names the flags and writes nothing (FR-055).
    #[test]
    fn a_terminal_without_cursor_control_is_pointed_to_the_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let pty = Pty::spawn(&path, &["setup", "--role", "node"], &[("TERM", "dumb")]);
        let (status, text, _) = pty.finish();
        assert_eq!(status.code(), Some(2), "{text}");
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("ERR_CEDEGRID_SETUP_TERMINAL"), "{text}");
        assert!(text.contains("--non-interactive"), "{text}");
        assert!(!path.exists());
    }

    #[test]
    fn a_command_that_needs_a_missing_config_offers_setup_on_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let mut pty = Pty::spawn(&path, &["validate"], &[]);
        pty.wait_for("Start setup now? [y/N]");
        pty.key(b"n\r");
        let (status, text, _) = pty.finish();
        assert!(!status.success(), "{text}");
        assert!(!path.exists());
    }
}

#[cfg(unix)]
#[test]
fn the_allowance_never_turns_on_without_a_terminal_and_turns_off_anywhere() {
    use cedegrid::state::{Allowance, StateStore, read_allowance, set_allowance};
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let state = dir.path().join("state");
    drop(StateStore::open(&state).unwrap());
    for extra in [&["--non-interactive"][..], &["--json"][..], &[][..]] {
        let mut args = vec!["setup", "--allow-unprotected"];
        args.extend_from_slice(extra);
        let output = run(&path, &args);
        assert!(!output.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY"),
            "{text}"
        );
        assert!(matches!(read_allowance(&state), Allowance::Off { .. }));
    }
    set_allowance(&state, 1).unwrap();
    let output = run(
        &path,
        &["setup", "--non-interactive", "--disallow-unprotected"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(read_allowance(&state), Allowance::Off { .. }));
}

include!("support/state_dir.rs");

#[test]
fn doctor_marks_aggressive_and_its_caution() {
    // 004 FR-007: the [Aggressive] label and the caution it needs show in doctor's text.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.toml");
    let mut config = Config {
        node_id: "test-node".into(),
        state_dir: "state".into(),
        ..Config::default()
    };
    config.contention.default_mode = cedegrid::contention::ContentionMode::Aggressive;
    config.contention.max_mode = Some(cedegrid::contention::ContentionMode::Aggressive);
    config
        .contention
        .acknowledge_caution
        .insert("aggressive_mode".into());
    fs::write(
        &path,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let text = String::from_utf8(run(&path, &["doctor", "--text"]).stdout).unwrap();
    let row = |key: &str| {
        text.lines()
            .find(|line| line.trim_start().starts_with(key))
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(row("Default mode"), "Default mode [Aggressive]", "{text}");
    assert_eq!(row("Node cap"), "Node cap [Aggressive]", "{text}");
    // Memory follows Performance under Aggressive (004 FR-006).
    assert!(
        text.contains("\nram: Performance (Aggressive covers compute only)"),
        "{text}"
    );
    assert!(row("ram ").starts_with("ram Performance"), "{text}");
    // Unprotected lists what is missing here, each with its fix.
    assert!(
        text.contains("Unprotected\n  not available on this host\n"),
        "{text}"
    );
    assert!(
        text.contains("    fix: run cedegrid setup --allow-unprotected on a terminal"),
        "{text}"
    );
    assert!(
        text.contains("[caution] aggressive_mode: contention.default_mode = aggressive"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn setup_flags_set_aggressive_only_with_its_acknowledgement() {
    // 004 T014: the flags path; the interface keeps the three margined modes for now.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.toml");
    let base = ["setup", "--non-interactive", "--role", "node"];
    let mut refused = base.to_vec();
    refused.extend(["--default-mode", "aggressive", "--max-mode", "aggressive"]);
    let output = run(&path, &refused);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("aggressive_mode"),
        "{output:?}"
    );
    let mut accepted = refused.clone();
    accepted.extend(["--acknowledge-caution", "aggressive_mode"]);
    let output = run(&path, &accepted);
    assert!(output.status.success(), "{output:?}");
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config
            .contention
            .mode(cedegrid::contention::ResourceKind::Cpu),
        cedegrid::contention::ContentionMode::Aggressive
    );
}
