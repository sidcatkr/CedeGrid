#![cfg(unix)]
use resource_manager::{
    agent::SupervisorSpec,
    config::{Config, NodeMode},
    execution_model::*,
    model::Resources,
    protocol::{Assignment, Lease},
    state::StateStore,
    supervision::ExecutionOutcome,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".agent-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}
fn clock_ms() -> u64 {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
        0
    );
    time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000
}
fn spec(dir: &Path, class: AllocationClass, argv: Vec<String>) -> SupervisorSpec {
    let mut config = Config {
        node_id: "node".into(),
        node_mode: NodeMode::Guaranteed,
        state_dir: dir.join("state"),
        ..Config::default()
    };
    config.execution.enabled = true;
    config.execution.prepare_timeout_ms = 2000;
    config.execution.release_confirm_timeout_ms = 1000;
    config.cpu.reserve_physical_cores = 0;
    config.ram.reserve_mib = 0;
    config.ram.reserve_percent = 0;
    config.lifecycle.drain_timeout_ms = 50;
    config.lifecycle.term_grace_ms = 50;
    config.lifecycle.heartbeat_interval_ms = 100;
    config.lifecycle.allocation_lease_ms = 2000;
    let output_dir = dir.join("output");
    fs::create_dir(&output_dir).unwrap();
    let request = LaunchRequest {
        task_id: "task".into(),
        assignment_id: "assignment".into(),
        argv,
        cwd: dir.to_path_buf(),
        env: BTreeMap::new(),
        resources: Resources {
            cpu_millicores: 100,
            ram_mib: 32,
            gpu_memory_mib: BTreeMap::new(),
        },
        replay_safe: true,
        class,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    };
    SupervisorSpec {
        config,
        assignment: Assignment {
            node_id: "node".into(),
            generation: 7,
            coordinator_epoch: 3,
            request,
            checkpoint: None,
        },
        capacity: Resources {
            cpu_millicores: 8000,
            ram_mib: 2048,
            gpu_memory_mib: BTreeMap::new(),
        },
        envelope: None,
        output_dir,
    }
}
fn start(spec: &SupervisorSpec) -> (Child, ChildStdin, mpsc::Receiver<serde_json::Value>) {
    let path = spec.output_dir.join("spec.json");
    fs::write(&path, serde_json::to_vec(spec).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_resmgr"))
        .arg("__assignment-supervisor")
        .arg(path)
        .env("TMPDIR", &spec.output_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str(&line) {
                let _ = tx.send(value);
            }
        }
    });
    (child, input, rx)
}
fn wait(child: &mut Child) -> std::process::ExitStatus {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("owned test supervisor failed deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn authorize(input: &mut ChildStdin, lease_ms: u64) {
    let lease = Lease {
        assignment_id: "assignment".into(),
        generation: 7,
        coordinator_epoch: 3,
        sequence: 1,
        valid_for_ms: lease_ms,
        drain: false,
    };
    writeln!(input,"{}",serde_json::json!({"kind":"authorize","lease":lease,"expires_monotonic_ms":clock_ms()+lease_ms})).unwrap();
    input.flush().unwrap();
}
#[test]
fn independent_supervisor_agent_loss_before_authorization_never_executes() {
    let dir = directory();
    let marker = dir.path().join("must-not-run");
    let spec = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf forbidden > '{}'", marker.display()),
        ],
    );
    let (mut child, input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    drop(input);
    assert!(!wait(&mut child).success());
    assert!(!marker.exists());
    let rows = StateStore::open(&spec.config.state_dir)
        .unwrap()
        .executions()
        .unwrap();
    assert_eq!(rows[0].phase, ExecutionPhase::Released);
    assert_eq!(rows[0].generation, 7);
}
#[test]
fn independent_supervisor_agent_loss_yields_opportunistic_but_preserves_guaranteed() {
    for class in [AllocationClass::Opportunistic, AllocationClass::Guaranteed] {
        let dir = directory();
        let spec = spec(
            dir.path(),
            class,
            vec![
                std::env::current_exe().unwrap().display().to_string(),
                "--ignored".into(),
                "--exact".into(),
                "short_single_process_fixture".into(),
            ],
        );
        let (mut child, mut input, rx) = start(&spec);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
            "prepared"
        );
        authorize(&mut input, 2000);
        drop(input);
        assert!(wait(&mut child).success());
        let outcome: ExecutionOutcome = serde_json::from_slice(
            &fs::read(spec.output_dir.join("execution-outcome.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(outcome.record.phase, ExecutionPhase::Released);
        assert_eq!(outcome.yielded, class == AllocationClass::Opportunistic);
        if class == AllocationClass::Guaranteed {
            assert_eq!(outcome.exit_code, Some(0));
        }
    }
}
#[test]
fn delayed_authorization_cannot_extend_its_remote_deadline() {
    let dir = directory();
    let marker = dir.path().join("must-not-run");
    let spec = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf forbidden > '{}'", marker.display()),
        ],
    );
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    let lease = Lease {
        assignment_id: "assignment".into(),
        generation: 7,
        coordinator_epoch: 3,
        sequence: 1,
        valid_for_ms: 1000,
        drain: false,
    };
    writeln!(
        input,
        "{}",
        serde_json::json!({"kind":"authorize","lease":lease,"expires_monotonic_ms":clock_ms()-1})
    )
    .unwrap();
    assert!(!wait(&mut child).success());
    assert!(!marker.exists());
}

#[test]
#[ignore = "owned subprocess fixture"]
fn full_single_cpu_fixture() {
    let until = Instant::now() + Duration::from_secs(3);
    let threads: usize = std::env::var("RESMGR_TEST_BUSY_THREADS")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(1);
    let mut workers = vec![];
    for _ in 0..threads {
        workers.push(std::thread::spawn(move || {
            let mut value = 1u64;
            while Instant::now() < until {
                value =
                    std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
            std::hint::black_box(value);
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn real_extra_cpu_threads_still_trigger_the_unchanged_envelope() {
    let dir = directory();
    let mut spec = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            std::env::current_exe().unwrap().display().to_string(),
            "--ignored".into(),
            "--exact".into(),
            "full_single_cpu_fixture".into(),
        ],
    );
    spec.assignment
        .request
        .env
        .insert("RESMGR_TEST_BUSY_THREADS".into(), "3".into());
    spec.assignment.request.resources.cpu_millicores = 1000;
    spec.capacity.cpu_millicores = 1000;
    spec.config.monitor.interval_ms = 500;
    spec.envelope = Some(spec.capacity.clone());
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    authorize(&mut input, 2000);
    assert!(wait(&mut child).success());
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    assert!(
        outcome.yielded,
        "real measured parallel CPU excess must still yield"
    );
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
}

#[test]
fn full_single_cpu_does_not_yield_from_counter_rounding() {
    let dir = directory();
    let mut spec = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            std::env::var("RESMGR_TEST_PYTHON").unwrap_or_else(|_| "python3".into()),
            "-c".into(),
            "import time\nend=time.monotonic()+3\nwhile time.monotonic()<end: pass".into(),
        ],
    );
    spec.assignment.request.resources.cpu_millicores = 1000;
    spec.capacity.cpu_millicores = 1000;
    spec.config.monitor.interval_ms = 500;
    spec.envelope = Some(spec.capacity.clone());
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    authorize(&mut input, 2000);
    assert!(wait(&mut child).success());
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    let mut diagnostics = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut diagnostics).unwrap();
    assert!(
        !outcome.yielded,
        "single CPU counter rounding must not trigger eviction: {diagnostics}"
    );
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    let usage: serde_json::Value =
        serde_json::from_slice(&fs::read(spec.output_dir.join("supervisor-usage.json")).unwrap())
            .unwrap();
    let identity: serde_json::Value = serde_json::from_slice(
        &fs::read(spec.output_dir.join("supervisor-identity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(usage["identity"], identity);
    assert_eq!(usage["scope"], "supervisor_self_excludes_workers");
    assert!(usage["peak_rss_bytes"].as_u64().unwrap() > 0);
    assert!(
        usage["user_cpu_us"].as_u64().unwrap() + usage["system_cpu_us"].as_u64().unwrap()
            < 1_500_000,
        "supervisor accounting must exclude the three-second busy user worker"
    );
}
#[test]
#[ignore = "only executed as a supervised single-process fixture"]
fn short_single_process_fixture() {
    std::thread::sleep(Duration::from_millis(700));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_agent_schedules_command_over_mtls_and_publishes_one_receipt() {
    use resource_manager::{agent::AgentConfig, coordinator::serve, protocol::*};
    let dir = directory();
    let root = std::env::current_dir().unwrap();
    let pki = dir.path().join("pki");
    let generated = Command::new("python3")
        .arg(root.join("tools/make_test_pki.py"))
        .arg("--output")
        .arg(&pki)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "PKI: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&generated.stdout).unwrap();
    let tls = |name: &str| TlsIdentity {
        ca_cert: pki.join("ca.pem"),
        certificate: pki.join(format!("{name}.pem")),
        private_key: pki.join(format!("{name}.key")),
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = format!("https://{address}");
    let cc = CoordinatorConfig {
        storage_profile: Default::default(),
        state_dir: dir.path().join("coordinator"),
        listen: address,
        tls: tls("server"),
        clients: BTreeMap::from([
            (
                info["client_fingerprints"]["operator"]
                    .as_str()
                    .unwrap()
                    .into(),
                Principal::Operator,
            ),
            (
                info["client_fingerprints"]["node"].as_str().unwrap().into(),
                Principal::Node {
                    node_id: "node".into(),
                },
            ),
        ]),
        lease_ms: 3000,
        telemetry_ttl_ms: 3000,
        max_artifact_bytes: 1024 * 1024,
        artifact_quota_bytes: 10 * 1024 * 1024,
        retry_limit: 3,
        retry_backoff_ms: 1000,
        retry_backoff_max_ms: 30000,
        yield_retry_backoff_ms: 1000,
        yield_retry_backoff_max_ms: 30000,
    };
    let server = tokio::spawn(serve(cc));
    let operator = RpcClient::new(&endpoint, &tls("operator")).unwrap();
    for _ in 0..50 {
        if operator
            .request(&Request::Status { job_id: None })
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut local = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf managed > \"$RESMGR_OUTPUT_DIR/marker\"".into(),
        ],
    );
    local.config.gpu.scale_up_cooldown_ms = 0;
    local.config.lifecycle.heartbeat_interval_ms = 200;
    local.config.monitor.interval_ms = 200;
    let config_file = dir.path().join("node.yaml");
    fs::write(&config_file, serde_yaml::to_string(&local.config).unwrap()).unwrap();
    let deployment = AgentConfig {
        coordinator_url: endpoint,
        tls: tls("node"),
        cpu_affinity: None,
        max_transfer_bytes_per_second: 10 * 1024 * 1024,
        capacity: local.capacity,
        max_workers: 3,
        max_spool_bytes: 10 * 1024 * 1024,
        max_runtime_seconds: 60,
    };
    let deployment_file = dir.path().join("agent.json");
    fs::write(&deployment_file, serde_json::to_vec(&deployment).unwrap()).unwrap();
    operator
        .request(&Request::PutPool {
            pool: PoolSpec {
                pool_id: "bounded".into(),
                class: AllocationClass::Guaranteed,
                node_ids: vec!["node".into()],
                min_workers: 1,
                max_workers: 1,
            },
        })
        .await
        .unwrap();
    local.assignment.request.assignment_id = String::new();
    let mut sdk_request = local.assignment.request.clone();
    sdk_request.task_id = "sdk-task".into();
    sdk_request.cwd = root.clone();
    sdk_request.argv = vec![
        std::env::var("RESMGR_TEST_PYTHON").unwrap_or_else(|_|"python3".into()),
        "-c".into(),
        "from resmgr import WorkerContext; c=WorkerContext.from_env(); p=c.output/'payload'; p.write_bytes(b'bounded real SDK artifact'); a=c.artifact('payload',p); q=c.output/'checkpoint-payload'; q.write_bytes(b'x'*262144); c.checkpoint({'completed_steps':2},[a,c.artifact('checkpoint-payload',q)]); c.complete({'completed_steps':4},[a])".into(),
    ];
    sdk_request.env.insert(
        "PYTHONPATH".into(),
        root.join("python").display().to_string(),
    );
    sdk_request
        .env
        .insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    let mut input_request = sdk_request.clone();
    let mut resume_request = sdk_request.clone();
    resume_request.task_id = "resume-task".into();
    resume_request.argv[2]="from resmgr import WorkerContext\nfrom pathlib import Path\nimport time\nc=WorkerContext.from_env()\nif c.resume:\n p=Path(c.resume['artifacts'][0]['path']); assert p.read_bytes()==b'resume-payload'; c.complete({'continued_from':c.resume['metadata']['cursor']},[c.artifact('payload',p)])\nelse:\n p=c.output/'payload'; p.write_bytes(b'resume-payload'); c.checkpoint({'cursor':3},[c.artifact('payload',p)])\n while not c.draining(): time.sleep(0.02)\n raise SystemExit(75)\n".into();
    operator
        .request(&Request::Submit {
            job: JobSpec {
                job_id: "job".into(),
                pool_id: "bounded".into(),
                priority: 0,
                tasks: vec![local.assignment.request, sdk_request, resume_request],
            },
        })
        .await
        .unwrap();
    let log = dir.path().join("agent.log");
    let err = dir.path().join("agent.err");
    let mut process = Command::new(env!("CARGO_BIN_EXE_resmgr"))
        .arg("--config")
        .arg(config_file)
        .arg("agent")
        .arg("--deployment")
        .arg(deployment_file)
        .env("TMPDIR", dir.path())
        .stdout(Stdio::from(fs::File::create(&log).unwrap()))
        .stderr(Stdio::from(fs::File::create(&err).unwrap()))
        .spawn()
        .unwrap();
    let mut result = None;
    for _ in 0..80 {
        if let Response::Result { submission } = operator
            .request(&Request::GetResult {
                task_id: "task".into(),
            })
            .await
            .unwrap()
            && submission.is_some()
        {
            result = submission;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut checkpoint_ready = false;
    for _ in 0..80 {
        if let Ok(store) = StateStore::open_read_only(&local.config.state_dir) {
            for record in store
                .executions()
                .unwrap()
                .iter()
                .filter(|r| r.task_id == "resume-task")
            {
                let output = local
                    .config
                    .state_dir
                    .join("attempts")
                    .join(&record.assignment_id);
                checkpoint_ready = fs::read_dir(&output).is_ok_and(|mut entries| {
                    entries.any(|e| {
                        e.is_ok_and(|e| {
                            let n = e.file_name();
                            let n = n.to_string_lossy();
                            n.starts_with("checkpoint-") && n.ends_with(".receipt.json")
                        })
                    })
                });
            }
        }
        if checkpoint_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    operator
        .request(&Request::DrainNode {
            node_id: "node".into(),
            drain: true,
        })
        .await
        .unwrap();
    for _ in 0..50 {
        let Response::Status { tasks, .. } = operator
            .request(&Request::Status { job_id: None })
            .await
            .unwrap()
        else {
            panic!("status missing");
        };
        if tasks.iter().any(|t| {
            t.task_id == "resume-task" && t.status == resource_manager::state::TaskStatus::Queued
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    operator
        .request(&Request::DrainNode {
            node_id: "node".into(),
            drain: false,
        })
        .await
        .unwrap();
    let mut resumed = None;
    for _ in 0..50 {
        if let Response::Result { submission } = operator
            .request(&Request::GetResult {
                task_id: "resume-task".into(),
            })
            .await
            .unwrap()
            && submission.is_some()
        {
            resumed = submission;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Response::Result {
        submission: Some(source),
    } = operator
        .request(&Request::GetResult {
            task_id: "sdk-task".into(),
        })
        .await
        .unwrap()
    else {
        panic!("input source result missing")
    };
    input_request.input_artifacts = vec![resource_manager::execution_model::NamedArtifact {
        name: "published-model".into(),
        sha256: source.artifacts[0].sha256.clone(),
        size: source.artifacts[0].size,
    }];
    input_request.argv[2]="from resmgr import WorkerContext\nimport os,json\nfrom pathlib import Path\nc=WorkerContext.from_env()\ni=json.loads(Path(os.environ['RESMGR_CONTEXT']).read_text())['inputs'][0]\nassert Path(i['path']).read_bytes()==b'bounded real SDK artifact'\nc.complete({'verified_input':i['sha256']})".into();
    input_request.task_id = "input-task-a".into();
    let mut second = input_request.clone();
    second.task_id = "input-task-b".into();
    operator
        .request(&Request::Submit {
            job: JobSpec {
                job_id: "input-job".into(),
                pool_id: "bounded".into(),
                priority: 0,
                tasks: vec![input_request, second],
            },
        })
        .await
        .unwrap();
    for task in ["input-task-a", "input-task-b"] {
        let mut done = false;
        for _ in 0..60 {
            if let Response::Result {
                submission: Some(value),
            } = operator
                .request(&Request::GetResult {
                    task_id: task.into(),
                })
                .await
                .unwrap()
            {
                assert_eq!(
                    value.result["metadata"]["verified_input"],
                    source.artifacts[0].sha256
                );
                done = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            done,
            "named input was not consumed: {}",
            fs::read_to_string(&log).unwrap()
        );
    }
    operator
        .request(&Request::PutPool {
            pool: PoolSpec {
                pool_id: "bounded".into(),
                class: AllocationClass::Guaranteed,
                node_ids: vec!["node".into()],
                min_workers: 1,
                max_workers: 3,
            },
        })
        .await
        .unwrap();
    let original: SupervisorSpec = serde_json::from_slice(
        &fs::read(
            local
                .config
                .state_dir
                .join("attempts")
                .join(&result.as_ref().unwrap().assignment_id)
                .join("supervisor-spec.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let mut tasks = vec![];
    for index in 0..55 {
        let mut task = original.assignment.request.clone();
        task.task_id = format!("short-{index}");
        task.assignment_id = String::new();
        task.argv = vec!["/bin/sh".into(), "-c".into(), ":".into()];
        task.env = BTreeMap::new();
        tasks.push(task);
    }
    operator
        .request(&Request::Submit {
            job: JobSpec {
                job_id: "many-short".into(),
                pool_id: "bounded".into(),
                priority: 0,
                tasks,
            },
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut all_short = false;
    while Instant::now() < deadline {
        let Response::Status { tasks, .. } = operator
            .request(&Request::Status {
                job_id: Some("many-short".into()),
            })
            .await
            .unwrap()
        else {
            panic!("status missing")
        };
        if tasks
            .iter()
            .all(|t| t.status == resource_manager::state::TaskStatus::Completed)
        {
            all_short = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        all_short,
        "55 bounded short tasks did not complete: {}",
        fs::read_to_string(&log).unwrap()
    );
    // This harness exclusively owns the unreaped direct agent child; request
    // graceful service shutdown so its final report can confirm zero held slots.
    assert_eq!(
        unsafe { libc::kill(process.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let status = wait(&mut process);
    assert!(
        status.success(),
        "agent failed: {} {}",
        fs::read_to_string(&log).unwrap(),
        fs::read_to_string(&err).unwrap()
    );
    assert!(
        result.is_some(),
        "missing useful result: {} {}",
        fs::read_to_string(&log).unwrap(),
        fs::read_to_string(&err).unwrap()
    );
    let result = result.unwrap();
    assert_eq!(result.result["metadata"]["exit_code"], 0);
    let output = local
        .config
        .state_dir
        .join("attempts")
        .join(&result.assignment_id);
    assert_eq!(
        fs::read_to_string(output.join("marker")).unwrap(),
        "managed"
    );
    assert!(output.join("result.receipt.json").is_file());
    let records = StateStore::open(&local.config.state_dir)
        .unwrap()
        .executions()
        .unwrap();
    assert_eq!(
        records.len(),
        61,
        "expected six lifecycle/input attempts and 55 short commands; log:{}",
        fs::read_to_string(&log).unwrap()
    );
    assert!(checkpoint_ready, "checkpoint upload was not confirmed");
    assert_eq!(
        resumed.expect("resume result missing").result["metadata"]["continued_from"],
        3
    );
    assert!(records.iter().all(|r| r.phase == ExecutionPhase::Released));
    let Response::Result {
        submission: Some(sdk_result),
    } = operator
        .request(&Request::GetResult {
            task_id: "sdk-task".into(),
        })
        .await
        .unwrap()
    else {
        panic!("SDK result missing: {}", fs::read_to_string(&log).unwrap());
    };
    assert_eq!(sdk_result.result["metadata"]["completed_steps"], 4);
    assert_eq!(sdk_result.artifacts.len(), 1);
    let Response::Chunk { data_hex, eof } = operator
        .request(&Request::ReadArtifact {
            sha256: sdk_result.artifacts[0].sha256.clone(),
            offset: 0,
            max_bytes: 1024,
        })
        .await
        .unwrap()
    else {
        panic!("artifact missing");
    };
    assert!(eof);
    assert_eq!(hex::decode(data_hex).unwrap(), b"bounded real SDK artifact");
    let cache = local
        .config
        .state_dir
        .join("attempts/.input-cache")
        .join(&source.artifacts[0].sha256);
    assert_eq!(fs::read(&cache).unwrap(), b"bounded real SDK artifact");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&cache).unwrap().nlink(),
            1,
            "released input hardlinks should be reclaimed"
        );
    }
    assert!(
        records
            .iter()
            .filter(|r| r.task_id.starts_with("input-task-"))
            .all(|r| local
                .config
                .state_dir
                .join("attempts")
                .join(&r.assignment_id)
                .join("spool-reclaimed.json")
                .is_file())
    );
    let stopped: serde_json::Value = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["event"] == "agent_stopped")
        .unwrap();
    assert_eq!(
        stopped["retained_slots"],
        0,
        "completed supervisor handles must not accumulate: log={} stderr={}",
        fs::read_to_string(&log).unwrap(),
        fs::read_to_string(&err).unwrap()
    );
    assert!(
        !fs::read_to_string(&log)
            .unwrap()
            .contains("completed task cannot change checkpoint"),
        "checkpoint publication must finish before final result acceptance fences it"
    );
    server.abort();
    let _ = server.await;
}
