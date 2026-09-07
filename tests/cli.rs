use resource_manager::config::Config;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_resmgr"))
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
    let path = directory.join("node.yaml");
    fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
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

#[test]
fn invalid_config_and_missing_history_never_initialize_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let output = run(&path, &["history"]);
    assert!(!output.status.success());
    fs::write(&path, "node_id: test-node\nmonitor:\n  interval_ms: 0\n").unwrap();
    assert!(!run(&path, &["observe"]).status.success());
    assert!(!dir.path().join("state").exists());
}

#[test]
fn observation_without_state_is_read_only_and_honest_about_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let path = setup(dir.path());
    let value = success_json(run(&path, &["observe", "--samples", "1", "--no-state"]));
    assert!(value["observation_id"].is_null());
    assert_eq!(value["decision"]["observe_only"], true);
    assert_eq!(value["decision"]["would_drain"], serde_json::json!([]));
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
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = setup(dir.path());
    let observed = success_json(run(&path, &["observe"]));
    assert!(observed["observation_id"].as_i64().unwrap() > 0);
    let history = success_json(run(&path, &["history"]));
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert!(dir.path().join("state/state.sqlite3").is_file());
}

#[test]
fn synthetic_replay_explains_pressure_without_affecting_live_system() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("node.yaml");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/node.yaml"),
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
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[1]["decision"]["expansion_allowed"], true);
    assert_eq!(rows[2]["decision"]["expansion_allowed"], false);
    assert!(
        !rows[2]["decision"]["would_drain"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(rows[4]["decision"]["expansion_allowed"], true);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn legacy_run_subcommand_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_resmgr"))
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("execution is disabled"));
    assert!(!dir.path().join("state").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn execution_setup(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use resource_manager::{
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
        },
        gpu: GpuConfig {
            scale_up_cooldown_ms: 0,
            ..Default::default()
        },
        kernel: KernelConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let config_path = directory.join("node.yaml");
    fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
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
    use resource_manager::state::{StateStore, TaskStatus};
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn missing_required_control_never_executes_and_safe_resubmission_uses_new_attempt() {
    use resource_manager::state::{StateStore, TaskStatus};
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
    use resource_manager::state::{StateStore, StorageProfile};
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let (config_path, job) = execution_setup(dir.path());
    let mut config: Config = serde_yaml::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config.storage_profile = StorageProfile::DeleteExtra;
    fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
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
    fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    assert!(!run(&config_path, &["executions"]).status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("marker")).unwrap(),
        "completed"
    );
}
