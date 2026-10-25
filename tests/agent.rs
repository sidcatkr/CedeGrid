#![cfg(unix)]
use cedegrid::{
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
    path::{Path, PathBuf},
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
        contention: None,
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
        control_token: None,
    }
}
fn start(spec: &SupervisorSpec) -> (Child, ChildStdin, mpsc::Receiver<serde_json::Value>) {
    let bootstrap_store =
        StateStore::open_with_profile(&spec.config.state_dir, spec.config.storage_profile).unwrap();
    let namespace = serde_json::to_string(bootstrap_store.namespace_guard().identity()).unwrap();
    let path = spec.output_dir.join("spec.json");
    fs::write(&path, serde_json::to_vec(spec).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .arg("__assignment-supervisor")
        .arg(path)
        .env("TMPDIR", &spec.output_dir)
        .env("CEDEGRID_SUPERVISOR_STATE_ROOT", &spec.config.state_dir)
        .env("CEDEGRID_SUPERVISOR_NAMESPACE", namespace)
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
fn independent_supervisor_agent_loss_holds_until_the_lease_expires() {
    for class in [AllocationClass::Opportunistic, AllocationClass::Guaranteed] {
        let dir = directory();
        let spec = spec(
            dir.path(),
            class,
            vec![
                std::env::current_exe().unwrap().display().to_string(),
                "--ignored".into(),
                "--exact".into(),
                "full_single_cpu_fixture".into(),
            ],
        );
        let (mut child, mut input, rx) = start(&spec);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
            "prepared"
        );
        authorize(&mut input, 1000);
        drop(input);
        assert!(wait(&mut child).success());
        let outcome: ExecutionOutcome = serde_json::from_slice(
            &fs::read(spec.output_dir.join("execution-outcome.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(outcome.record.phase, ExecutionPhase::Released);
        // Opportunistic work yields at its lease expiry, not at the moment of loss.
        assert_eq!(outcome.yielded, class == AllocationClass::Opportunistic);
        if class == AllocationClass::Opportunistic {
            assert!(outcome.drain_started_ms.unwrap() >= 900, "{outcome:?}");
        } else {
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
    let threads: usize = std::env::var("CEDEGRID_TEST_BUSY_THREADS")
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

fn busy_envelope_spec(dir: &Path) -> SupervisorSpec {
    let mut spec = spec(
        dir,
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
        .insert("CEDEGRID_TEST_BUSY_THREADS".into(), "3".into());
    spec.assignment.request.resources.cpu_millicores = 1000;
    spec.capacity.cpu_millicores = 1000;
    spec.config.monitor.interval_ms = 500;
    spec.envelope = Some(spec.capacity.clone());
    spec
}

#[test]
fn supervisors_run_no_collector_or_policy_and_never_drain_locally() {
    // Three busy threads exceed the 1000m envelope, but only the agent decides.
    let dir = directory();
    let spec = busy_envelope_spec(dir.path());
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    authorize(&mut input, 10_000);
    assert!(wait(&mut child).success());
    let mut diagnostics = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut diagnostics).unwrap();
    assert!(!diagnostics.contains("local_policy_drain"), "{diagnostics}");
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    assert!(
        !outcome.yielded,
        "a supervisor never drains on its own measurement"
    );
    assert_eq!(outcome.exit_code, Some(0));
}

#[test]
fn real_extra_cpu_threads_still_trigger_the_agent_envelope() {
    // The test plays the agent: it measures the owned worker with the agent's
    // collector, applies the agent's envelope check and delivers the drain.
    let dir = directory();
    let spec = busy_envelope_spec(dir.path());
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    authorize(&mut input, 10_000);
    let mut collector = cedegrid::telemetry::ManagedCollector::new(&spec.config).unwrap();
    let store = StateStore::open_read_only(&spec.config.state_dir).unwrap();
    let mut drained = false;
    let end = Instant::now() + Duration::from_secs(3);
    while !drained && Instant::now() < end && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(500));
        let (_, allocations) = collector.sample(&store.executions().unwrap()).unwrap();
        if cedegrid::agent::envelope_exceeded(&allocations, &spec.capacity, 0) {
            writeln!(input, "{}", serde_json::json!({"kind":"drain"})).unwrap();
            input.flush().unwrap();
            drained = true;
        }
    }
    assert!(
        drained,
        "real measured parallel CPU excess must exceed the envelope"
    );
    assert!(wait(&mut child).success());
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    assert!(outcome.yielded, "the agent's envelope drain must yield");
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
}

#[test]
fn full_single_cpu_does_not_yield_from_counter_rounding() {
    let dir = directory();
    let mut spec = spec(
        dir.path(),
        AllocationClass::Guaranteed,
        vec![
            std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into()),
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
    use cedegrid::{agent::AgentConfig, coordinator::serve, protocol::*};
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
            "printf managed > \"$CEDEGRID_OUTPUT_DIR/marker\"".into(),
        ],
    );
    local.config.gpu.scale_up_cooldown_ms = Some(0);
    local.config.contention.cpu.scale_up_cooldown_ms = Some(0);
    local.config.contention.ram.scale_up_cooldown_ms = Some(0);
    local.config.lifecycle.heartbeat_interval_ms = 200;
    local.config.monitor.interval_ms = 200;
    let config_file = dir.path().join("node.toml");
    fs::write(
        &config_file,
        cedegrid::config::serialize_runtime(
            &local.config,
            cedegrid::config::RuntimeConfigKind::Node,
        )
        .unwrap(),
    )
    .unwrap();
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
    let deployment_file = dir.path().join("agent.toml");
    fs::write(
        &deployment_file,
        cedegrid::config::serialize_runtime(
            &deployment,
            cedegrid::config::RuntimeConfigKind::Agent,
        )
        .unwrap(),
    )
    .unwrap();
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
        std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_|"python3".into()),
        "-c".into(),
        "from cedegrid import WorkerContext; c=WorkerContext.from_env(); p=c.output/'payload'; p.write_bytes(b'bounded real SDK artifact'); a=c.artifact('payload',p); q=c.output/'checkpoint-payload'; q.write_bytes(b'x'*262144); c.checkpoint({'completed_steps':2},[a,c.artifact('checkpoint-payload',q)]); c.complete({'completed_steps':4},[a])".into(),
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
    resume_request.argv[2]="from cedegrid import WorkerContext\nfrom pathlib import Path\nimport time\nc=WorkerContext.from_env()\nif c.resume:\n p=Path(c.resume['artifacts'][0]['path']); assert p.read_bytes()==b'resume-payload'; c.complete({'continued_from':c.resume['metadata']['cursor']},[c.artifact('payload',p)])\nelse:\n p=c.output/'payload'; p.write_bytes(b'resume-payload'); c.checkpoint({'cursor':3},[c.artifact('payload',p)])\n while not c.draining(): time.sleep(0.02)\n raise SystemExit(75)\n".into();
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
    let mut process = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
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
        if tasks
            .iter()
            .any(|t| t.task_id == "resume-task" && t.status == cedegrid::state::TaskStatus::Queued)
        {
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
        let attempts: Vec<_> = fs::read_dir(local.config.state_dir.join("attempts"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                let directory = entry.path();
                (
                    directory.display().to_string(),
                    fs::read_to_string(directory.join("supervisor.stderr")).unwrap_or_default(),
                    fs::read_to_string(directory.join("stderr.log")).unwrap_or_default(),
                    fs::read_to_string(directory.join("execution-outcome.json"))
                        .unwrap_or_default(),
                )
            })
            .collect();
        let records = StateStore::open_read_only(&local.config.state_dir)
            .and_then(|store| store.executions());
        let _ = process.kill();
        let _ = process.wait();
        panic!(
            "input source result missing; agent stdout={} stderr={} attempts={attempts:?} records={records:?}",
            fs::read_to_string(&log).unwrap_or_default(),
            fs::read_to_string(&err).unwrap_or_default()
        )
    };
    input_request.input_artifacts = vec![cedegrid::execution_model::NamedArtifact {
        name: "published-model".into(),
        sha256: source.artifacts[0].sha256.clone(),
        size: source.artifacts[0].size,
    }];
    input_request.argv[2]="from cedegrid import WorkerContext\nimport os,json\nfrom pathlib import Path\nc=WorkerContext.from_env()\ni=json.loads(Path(os.environ['CEDEGRID_CONTEXT']).read_text())['inputs'][0]\nassert Path(i['path']).read_bytes()==b'bounded real SDK artifact'\nc.complete({'verified_input':i['sha256']})".into();
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
            .all(|t| t.status == cedegrid::state::TaskStatus::Completed)
        {
            all_short = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !all_short || std::env::var_os("CEDEGRID_TEST_CAPACITY_DIAGNOSTICS").is_some() {
        let captured_ms = cedegrid::coordinator::now_ms();
        let node_report = StateStore::open_read_only(&dir.path().join("coordinator"))
            .and_then(|store| {
                let connection = rusqlite::Connection::open_with_flags(
                    store.database_path(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let (received, report): (i64, String) = connection.query_row(
                    "SELECT received_ms,report_json FROM nodes WHERE node_id='node'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let report: serde_json::Value = serde_json::from_str(&report)?;
                Ok(serde_json::json!({
                    "received_age_ms": captured_ms.saturating_sub(u64::try_from(received)?),
                    "observed_age_ms": captured_ms.saturating_sub(report["observed_at_unix_ms"].as_u64().unwrap_or(0)),
                    "expansion_allowed": report["expansion_allowed"],
                    "launch_slots": report["launch_slots"],
                    "managed_budget": report["managed_budget"],
                }))
            });
        let observations = StateStore::open_read_only(&local.config.state_dir)
            .and_then(|store| store.observations(20))
            .map(|rows| {
                rows.into_iter().map(|row| serde_json::json!({
                "id": row["id"],
                "observed_at_unix_ms": row["snapshot"]["observed_at_unix_ms"],
                "cpu_busy_millicores": row["snapshot"]["cpu_busy_millicores"],
                "cpu_telemetry": row["snapshot"]["capabilities"]["cpu_telemetry"],
                "expansion_allowed": row["decision"]["expansion_allowed"],
                "cpu_ram_expansion_allowed": row["decision"]["cpu_ram_expansion_allowed"],
                "reasons": row["decision"]["reasons"],
            })).collect::<Vec<_>>()
            });
        eprintln!("short-task diagnostic: node={node_report:?} observations={observations:?}");
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
    let saved_spec: SupervisorSpec =
        serde_json::from_slice(&fs::read(output.join("supervisor-spec.json")).unwrap()).unwrap();
    let workspace_output = PathBuf::from(&saved_spec.assignment.request.env["CEDEGRID_OUTPUT_DIR"]);
    assert_eq!(
        fs::read_to_string(workspace_output.join("marker")).unwrap(),
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

#[test]
fn unknown_control_kinds_are_ignored_and_adjust_reports_applied_steps() {
    let dir = directory();
    let mut spec = spec(
        dir.path(),
        AllocationClass::Opportunistic,
        vec![
            std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into()),
            "-c".into(),
            "import time; time.sleep(1.5)".into(),
        ],
    );
    spec.config.lifecycle.allocation_lease_ms = 10_000;
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    // A newer agent's control kind is reported and ignored, never agent loss.
    writeln!(
        input,
        "{}",
        serde_json::json!({"kind":"future_control","x":1})
    )
    .unwrap();
    authorize(&mut input, 10_000);
    writeln!(input, "{}", serde_json::json!({"kind":"future_control"})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"kind":"adjust","sequence":1,"resources":{"cpu":{"step":"pause","cause":"external_growth"}}})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"kind":"adjust","sequence":2,"resources":{"cpu":{"step":"none","cause":"external_growth"}}})
    )
    .unwrap();
    writeln!(input, "{}", serde_json::json!({"kind":"query"})).unwrap();
    input.flush().unwrap();
    let mut events = vec![];
    let end = Instant::now() + Duration::from_secs(5);
    while events.len() < 3 && Instant::now() < end {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(200))
            && matches!(event["event"].as_str(), Some("adjusted" | "state"))
        {
            events.push(event);
        }
    }
    assert_eq!(events[0]["event"], "adjusted");
    assert_eq!(events[0]["sequence"], 1);
    assert_eq!(events[0]["applied"]["cpu"], "pause");
    assert_eq!(events[1]["applied"]["cpu"], "none");
    assert_eq!(events[2]["event"], "state");
    assert_eq!(events[2]["authority"], "connected");
    assert!(wait(&mut child).success());
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    assert!(
        !outcome.yielded,
        "an unknown control line is not agent loss"
    );
    assert_eq!(outcome.exit_code, Some(0));
}

/// A coordinator and a node configuration for an end-to-end agent test.
struct Service {
    dir: tempfile::TempDir,
    operator: cedegrid::protocol::RpcClient,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    local: SupervisorSpec,
    config_file: PathBuf,
    deployment_file: PathBuf,
}

impl Service {
    /// Start a coordinator, write node and agent configs, and submit one task.
    async fn start(
        class: AllocationClass,
        argv: Vec<String>,
        configure: impl FnOnce(&mut SupervisorSpec),
    ) -> Self {
        use cedegrid::{agent::AgentConfig, coordinator::serve, protocol::*};
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
        let server = tokio::spawn(serve(CoordinatorConfig {
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
            lease_ms: 30_000,
            telemetry_ttl_ms: 3000,
            max_artifact_bytes: 1024 * 1024,
            artifact_quota_bytes: 10 * 1024 * 1024,
            retry_limit: 3,
            retry_backoff_ms: 1000,
            retry_backoff_max_ms: 30000,
            yield_retry_backoff_ms: 1000,
            yield_retry_backoff_max_ms: 30000,
        }));
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
        let mut local = spec(dir.path(), class, argv);
        local.config.gpu.scale_up_cooldown_ms = Some(0);
        local.config.contention.cpu.scale_up_cooldown_ms = Some(0);
        local.config.contention.ram.scale_up_cooldown_ms = Some(0);
        local.config.lifecycle.heartbeat_interval_ms = 400;
        local.config.monitor.interval_ms = 400;
        configure(&mut local);
        let config_file = dir.path().join("node.toml");
        fs::write(
            &config_file,
            cedegrid::config::serialize_runtime(
                &local.config,
                cedegrid::config::RuntimeConfigKind::Node,
            )
            .unwrap(),
        )
        .unwrap();
        let deployment_file = dir.path().join("agent.toml");
        fs::write(
            &deployment_file,
            cedegrid::config::serialize_runtime(
                &AgentConfig {
                    coordinator_url: endpoint,
                    tls: tls("node"),
                    cpu_affinity: None,
                    max_transfer_bytes_per_second: 10 * 1024 * 1024,
                    capacity: local.capacity.clone(),
                    max_workers: 1,
                    max_spool_bytes: 10 * 1024 * 1024,
                    max_runtime_seconds: 90,
                },
                cedegrid::config::RuntimeConfigKind::Agent,
            )
            .unwrap(),
        )
        .unwrap();
        operator
            .request(&Request::PutPool {
                pool: PoolSpec {
                    pool_id: "pool".into(),
                    class,
                    node_ids: vec!["node".into()],
                    min_workers: 1,
                    max_workers: 1,
                },
            })
            .await
            .unwrap();
        local.assignment.request.assignment_id = String::new();
        operator
            .request(&Request::Submit {
                job: JobSpec {
                    job_id: "job".into(),
                    pool_id: "pool".into(),
                    priority: 0,
                    tasks: vec![local.assignment.request.clone()],
                },
            })
            .await
            .unwrap();
        Self {
            dir,
            operator,
            server,
            local,
            config_file,
            deployment_file,
        }
    }

    fn agent(&self, log: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .arg("--config")
            .arg(&self.config_file)
            .arg("agent")
            .arg("--deployment")
            .arg(&self.deployment_file)
            .env("TMPDIR", self.dir.path())
            .stdout(Stdio::from(
                fs::File::create(self.dir.path().join(format!("{log}.log"))).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(self.dir.path().join(format!("{log}.err"))).unwrap(),
            ))
            .spawn()
            .unwrap()
    }

    async fn result(&self, attempts: usize) -> Option<cedegrid::protocol::ResultSubmission> {
        use cedegrid::protocol::*;
        for _ in 0..attempts {
            if let Response::Result { submission } = self
                .operator
                .request(&Request::GetResult {
                    task_id: "task".into(),
                })
                .await
                .unwrap()
                && submission.is_some()
            {
                return submission;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    fn record(&self) -> Option<ExecutionRecord> {
        StateStore::open_read_only(&self.local.config.state_dir)
            .ok()?
            .executions()
            .ok()?
            .into_iter()
            .find(|r| r.task_id == "task")
    }

    fn outcome(&self, assignment: &str) -> ExecutionOutcome {
        serde_json::from_slice(
            &fs::read(
                self.local
                    .config
                    .state_dir
                    .join("attempts")
                    .join(assignment)
                    .join("execution-outcome.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }
}

/// Quickstart §3: rootless CPU contention caps the job and reverses, never drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rootless_cpu_contention_caps_the_job_and_reverses_without_drain() {
    let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let service = Service::start(
        AllocationClass::Opportunistic,
        vec![
            python.clone(),
            "-c".into(),
            "import time\nend=time.monotonic()+30\nwhile time.monotonic()<end: pass\n".into(),
        ],
        |local| {
            local.config.lifecycle.allocation_lease_ms = 30_000;
            local.config.contention.cpu.mode =
                Some(cedegrid::contention::ContentionMode::YieldFirst);
            local.config.contention.cpu.scale_up_step_interval_s = Some(1);
            local.assignment.request.resources.cpu_millicores = 1000;
        },
    )
    .await;
    let mut agent = service.agent("agent");
    // The latest decision's own CPU and the job's CPU step, from the agent's history.
    let state = service.local.config.state_dir.clone();
    let latest = move || -> Option<(u64, Option<String>, String)> {
        let store = StateStore::open_read_only(&state).ok()?;
        let row = store.observations(1).ok()?.into_iter().next()?;
        let decision = &row["decision"];
        let own = decision["resources"]["cpu"]["own"][0].as_u64()?;
        let step = decision["actions"].as_array().and_then(|actions| {
            actions
                .iter()
                .find(|a| a["resource"] == "cpu")
                .and_then(|a| a["step"].as_str().map(str::to_owned))
        });
        let evidence = format!(
            "busy={} budget={} cpu={} actions={}",
            row["snapshot"]["cpu_busy_millicores"],
            decision["managed_budget"],
            decision["resources"]["cpu"],
            decision["actions"]
        );
        Some((own, step, evidence))
    };
    let wait_for = |what: &str, deadline: Duration, ok: &dyn Fn(u64, Option<&str>) -> bool| {
        let end = Instant::now() + deadline;
        loop {
            let current = latest();
            if let Some((own, step, _)) = &current
                && ok(*own, step.as_deref())
            {
                return (*own, step.clone());
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}: {}",
                current.map(|c| c.2).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let (alone, _) = wait_for("the job to run", Duration::from_secs(20), &|own, step| {
        own >= 600 && step.is_none()
    });
    // Test-owned external load on every logical CPU.
    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut load: Vec<Child> = (0..cpus)
        .map(|_| {
            Command::new(&python)
                .args([
                    "-c",
                    "import time\nend=time.monotonic()+40\nwhile time.monotonic()<end: pass\n",
                ])
                .spawn()
                .unwrap()
        })
        .collect();
    let (_, step) = wait_for("a cap or pause", Duration::from_secs(8), &|_, step| {
        matches!(step, Some("cap" | "pause"))
    });
    let (capped, _) = wait_for("own CPU to fall", Duration::from_secs(8), &|own, step| {
        own < alone / 2 && matches!(step, Some("cap" | "pause"))
    });
    let assignment = service.record().unwrap().assignment_id;
    let journaled = StateStore::open_read_only(&service.local.config.state_dir)
        .unwrap()
        .contention(&assignment)
        .unwrap();
    for process in &mut load {
        let _ = process.kill();
        let _ = process.wait();
    }
    wait_for(
        "the step to reverse",
        Duration::from_secs(15),
        &|_, step| step.is_none(),
    );
    let completed = service.result(400).await;
    let _ = agent.kill();
    let _ = agent.wait();
    service.server.abort();
    assert!(step.is_some(), "own work was capped or paused");
    assert!(capped < alone / 2, "own CPU fell from {alone} to {capped}");
    assert!(
        journaled.is_some_and(|c| c.applied.values().any(|s| matches!(
            s.step,
            cedegrid::contention::YieldStep::Cap | cedegrid::contention::YieldStep::Pause
        ))),
        "the supervisor journaled the applied step"
    );
    assert!(completed.is_some(), "the job completed instead of draining");
    assert!(
        !service.outcome(&assignment).yielded,
        "CPU contention never drains running work"
    );
}

/// A restarted agent reattaches through control.sock, renews the lease and never
/// marks the live supervisor NeedsReconciliation (FR-015, audit H5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_agent_reattaches_to_live_supervisors() {
    let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let service = Service::start(
        AllocationClass::Opportunistic,
        vec![python, "-c".into(), "import time; time.sleep(8)".into()],
        |local| {
            // Without renewals after the restart this lease would expire and drain.
            local.config.lifecycle.allocation_lease_ms = 3000;
        },
    )
    .await;
    let mut agent = service.agent("first");
    let end = Instant::now() + Duration::from_secs(20);
    while !service
        .record()
        .is_some_and(|r| r.phase == ExecutionPhase::Running)
    {
        assert!(Instant::now() < end, "job never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    agent.kill().unwrap();
    agent.wait().unwrap();
    let mut restarted = service.agent("second");
    let mut phases = std::collections::BTreeSet::new();
    let mut completed = None;
    for _ in 0..200 {
        if let Some(record) = service.record() {
            phases.insert(format!("{:?}", record.phase));
        }
        completed = service.result(1).await;
        if completed.is_some() {
            break;
        }
    }
    let _ = restarted.kill();
    let _ = restarted.wait();
    service.server.abort();
    let log = fs::read_to_string(service.dir.path().join("second.log")).unwrap();
    assert!(log.contains("supervisor_reattached"), "{log}");
    assert!(!phases.contains("NeedsReconciliation"), "{phases:?}");
    assert!(completed.is_some(), "the job completed after the restart");
    let assignment = service.record().unwrap().assignment_id;
    let outcome = service.outcome(&assignment);
    assert!(
        !outcome.yielded,
        "the reattached agent renewed the lease: {outcome:?}"
    );
    assert_eq!(outcome.exit_code, Some(0));
}

#[test]
fn advertised_controls_come_from_backend_capability_never_from_observations() {
    let mut config = Config::default();
    config.cgroup.enabled = true;
    config.cgroup.delegated_root = Some("/nonexistent/cedegrid-control-test".into());
    config.cgroup.authorized_controls = vec![
        "cgroup.procs".into(),
        "cpu.weight".into(),
        "cpu.max".into(),
        "cgroup.freeze".into(),
    ];
    config.cgroup.validate().unwrap();
    // The controls a prepared backend would offer follow its authorized settings.
    let offered = config.cgroup.advertised_controls();
    assert!(offered.contains(&"cpu.weight".to_string()));
    assert!(offered.contains(&"cpu.max".to_string()));
    assert!(offered.contains(&"cgroup.freeze".to_string()));
    assert!(!offered.iter().any(|c| c.ends_with(".pressure")));
    // This backend cannot prepare, so none of them is advertised.
    let advertised = cedegrid::agent::advertised_controls(&config, false);
    assert!(advertised.contains(&"cpu.nice".to_string()));
    for name in [
        "cpu.weight",
        "cpu.max",
        "cgroup.freeze",
        "memory.high",
        "cpu.pressure",
        "memory.pressure",
        "io.pressure",
    ] {
        assert!(
            !advertised.contains(&name.to_string()),
            "{name}: {advertised:?}"
        );
    }
}

/// The `ps` run state of a process, such as `S`, `R` or `T` (stopped).
fn run_state(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[test]
fn a_reconnected_controller_still_reaches_only_the_verified_family() {
    let dir = directory();
    let mut foreign = Command::new("sleep").arg("30").spawn().unwrap();
    let code = r#"
import os,sys,time,subprocess
from pathlib import Path
d=Path.cwd()
loose=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
(d/'loose.pid').write_text(str(loose.pid))
(d/'leader.pid').write_text(str(os.getpid()))
while not (d/'stop').exists():
    time.sleep(0.02)
loose.kill();loose.wait()
"#;
    let mut spec = spec(
        dir.path(),
        AllocationClass::Opportunistic,
        vec![
            std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into()),
            "-c".into(),
            code.into(),
        ],
    );
    spec.config.lifecycle.allocation_lease_ms = 20_000;
    let token = "5a".repeat(32);
    spec.control_token = Some(token.clone());
    let (mut child, mut input, rx) = start(&spec);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap()["event"],
        "prepared"
    );
    authorize(&mut input, 20_000);
    let end = Instant::now() + Duration::from_secs(10);
    while read_pid(&dir.path().join("leader.pid")).is_none() {
        assert!(Instant::now() < end, "job never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let leader = read_pid(&dir.path().join("leader.pid")).unwrap();
    let loose = read_pid(&dir.path().join("loose.pid")).unwrap();
    // The first agent is gone; a restarted one reconnects through the socket.
    drop(input);
    let socket: serde_json::Value =
        serde_json::from_slice(&fs::read(spec.output_dir.join("control-socket.json")).unwrap())
            .unwrap();
    let mut stream = cedegrid::supervision::control_socket::connect(
        Path::new(socket["path"].as_str().unwrap()),
        &token,
    )
    .unwrap();
    let (tx, events) = mpsc::channel();
    let reader = stream.try_clone().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = tx.send(value);
            }
        }
    });
    let mut adjust = |sequence: u64, step: &str| {
        writeln!(
            stream,
            "{}",
            serde_json::json!({"kind":"adjust","sequence":sequence,"resources":{"cpu":{"step":step,"cause":"external_growth"}}})
        )
        .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < end, "no adjusted event for {sequence}");
            if let Ok(event) = events.recv_timeout(Duration::from_millis(100))
                && event["event"] == "adjusted"
            {
                assert_eq!(event["sequence"], sequence);
                assert_eq!(event["applied"]["cpu"], step, "{event}");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        [leader, loose, foreign.id()].map(run_state)
    };
    let paused = adjust(1, "pause");
    let resumed = adjust(2, "none");
    fs::write(dir.path().join("stop"), "").unwrap();
    let status = wait(&mut child);
    let foreign_alive = foreign.try_wait().unwrap().is_none();
    let foreign_after = run_state(foreign.id());
    foreign.kill().unwrap();
    foreign.wait().unwrap();
    assert!(status.success());
    assert!(paused[0].contains('T'), "{paused:?}");
    assert!(
        !paused[1].contains('T') && !paused[2].contains('T'),
        "{paused:?}"
    );
    assert!(
        resumed.iter().all(|state| !state.contains('T')),
        "{resumed:?}"
    );
    assert!(
        foreign_alive && !foreign_after.contains('T'),
        "{foreign_after}"
    );
    let outcome: ExecutionOutcome =
        serde_json::from_slice(&fs::read(spec.output_dir.join("execution-outcome.json")).unwrap())
            .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.yielded, "{outcome:?}");
}

/// A restarted agent never reattaches to, or signals, a process that reuses the
/// recorded supervisor PID (FR-002).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_agent_refuses_a_reused_supervisor_pid() {
    let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let service = Service::start(
        AllocationClass::Opportunistic,
        vec![python, "-c".into(), "import time; time.sleep(30)".into()],
        |local| {
            // The orphaned supervisor stays live past the restart, then drains.
            local.config.lifecycle.allocation_lease_ms = 6000;
        },
    )
    .await;
    let mut agent = service.agent("first");
    let end = Instant::now() + Duration::from_secs(20);
    while !service
        .record()
        .is_some_and(|r| r.phase == ExecutionPhase::Running)
    {
        assert!(Instant::now() < end, "job never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    agent.kill().unwrap();
    agent.wait().unwrap();
    // The recorded supervisor PID now names another live process.
    let mut decoy = Command::new("sleep").arg("30").spawn().unwrap();
    let record = service.record().unwrap();
    let path = service
        .local
        .config
        .state_dir
        .join("attempts")
        .join(&record.assignment_id)
        .join("supervisor-identity.json");
    let mut identity: ProcessIdentity = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let supervisor = identity.pid;
    identity.pid = decoy.id();
    fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
    let mut restarted = service.agent("second");
    let mut states = vec![];
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        states.push(run_state(decoy.id()));
    }
    let _ = restarted.kill();
    let _ = restarted.wait();
    let decoy_alive = decoy.try_wait().unwrap().is_none();
    decoy.kill().unwrap();
    decoy.wait().unwrap();
    // The orphaned supervisor drains at its own lease expiry and exits.
    let end = Instant::now() + Duration::from_secs(15);
    while unsafe { libc::kill(supervisor as libc::pid_t, 0) } == 0 {
        assert!(Instant::now() < end, "orphaned supervisor did not exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    service.server.abort();
    let log = fs::read_to_string(service.dir.path().join("second.log")).unwrap();
    assert!(!log.contains("supervisor_reattached"), "{log}");
    assert!(
        log.contains("supervisor_not_reattached") && log.contains("supervisor is not live"),
        "{log}"
    );
    assert!(decoy_alive, "the decoy was signaled");
    assert!(
        states.iter().all(|state| !state.contains('T')),
        "{states:?}"
    );
}

async fn task_state(service: &Service) -> Option<(String, u64)> {
    use cedegrid::protocol::*;
    let Response::Status { tasks, .. } = service
        .operator
        .request(&Request::Status { job_id: None })
        .await
        .ok()?
    else {
        return None;
    };
    let task = serde_json::to_value(tasks.first()?).ok()?;
    Some((
        task["status"].as_str()?.to_owned(),
        task["generation"].as_u64()?,
    ))
}

/// A job cancelled while its agent is down ends as cancelled once the restarted
/// agent drains it, and the task is never assigned again (A18).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_task_stays_cancelled_across_an_agent_restart() {
    use cedegrid::protocol::*;
    let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let service = Service::start(
        AllocationClass::Opportunistic,
        vec![python, "-c".into(), "import time; time.sleep(30)".into()],
        |local| {
            local.config.lifecycle.allocation_lease_ms = 6000;
        },
    )
    .await;
    let mut agent = service.agent("first");
    let end = Instant::now() + Duration::from_secs(20);
    while !service
        .record()
        .is_some_and(|r| r.phase == ExecutionPhase::Running)
    {
        assert!(Instant::now() < end, "job never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    agent.kill().unwrap();
    agent.wait().unwrap();
    service
        .operator
        .request(&Request::Cancel {
            job_id: "job".into(),
        })
        .await
        .unwrap();
    let mut restarted = service.agent("second");
    let end = Instant::now() + Duration::from_secs(30);
    let mut state = None;
    while Instant::now() < end {
        state = task_state(&service).await;
        if state
            .as_ref()
            .is_some_and(|(status, _)| status != "assigned")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Several more heartbeats with free capacity never bring the task back.
    let mut later = vec![];
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        later.push(task_state(&service).await);
    }
    let _ = restarted.kill();
    let _ = restarted.wait();
    service.server.abort();
    let log = fs::read_to_string(service.dir.path().join("second.log")).unwrap();
    assert!(log.contains("supervisor_reattached"), "{log}");
    assert_eq!(state, Some(("cancelled".into(), 1)), "{log}");
    assert!(
        later.iter().all(|s| s == &Some(("cancelled".into(), 1))),
        "{later:?}"
    );
    let assignment = service.record().unwrap().assignment_id;
    assert!(service.outcome(&assignment).yielded);
}

/// Operator limits through the policy authority (FR-057, FR-058).
mod operator_limits {
    use cedegrid::{
        config::Config,
        contention::{ResourceKey, YieldCause, YieldStep},
        execution_model::AllocationClass,
        model::*,
        policy::PolicyEngine,
    };
    use std::collections::BTreeMap;

    fn config() -> Config {
        let mut config = Config::default();
        config.cpu.reserve_physical_cores = 0;
        config.ram.reserve_mib = 1000;
        config.ram.reserve_percent = 0;
        config
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            node_id: "local".into(),
            observed_at_unix_ms: 100,
            cpu_capacity_millicores: 8000,
            physical_cores: Some(4),
            cpu_busy_millicores: Some(3500),
            cpu_busy_lower_millicores: None,
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

    fn allocation(
        id: &str,
        class: AllocationClass,
        requested: (u64, u64),
        observed: (u64, u64),
    ) -> Allocation {
        let resources = |(cpu, ram): (u64, u64)| Resources {
            cpu_millicores: cpu,
            ram_mib: ram,
            gpu_memory_mib: BTreeMap::new(),
        };
        Allocation {
            id: id.into(),
            class,
            phase: AllocationPhase::Running,
            requested: resources(requested),
            observed: Some(resources(observed)),
            observed_upper: None,
            modes: Default::default(),
        }
    }

    fn step(decision: &Decision, id: &str, key: ResourceKey) -> Option<(YieldStep, YieldCause)> {
        decision
            .actions
            .iter()
            .find(|a| a.allocation_id == id && a.resource == key)
            .map(|a| (a.step, a.cause))
    }

    #[test]
    fn an_envelope_breach_is_restored_per_resource_opportunistic_first_without_an_all_stop() {
        let input = PolicyInput {
            allocations: vec![
                // Over its RAM request: together the work exceeds the 4000 MiB envelope.
                allocation(
                    "o",
                    AllocationClass::Opportunistic,
                    (500, 1000),
                    (500, 3000),
                ),
                allocation("g", AllocationClass::Guaranteed, (500, 2000), (500, 2000)),
                // Over its CPU request: the 2000m envelope is exceeded by usage.
                allocation("c", AllocationClass::Opportunistic, (500, 0), (2500, 0)),
            ],
            envelope: Some(Resources {
                cpu_millicores: 2000,
                ram_mib: 4000,
                gpu_memory_mib: BTreeMap::new(),
            }),
            // Plain commands without the SDK cannot take a hint.
            facts: ["o", "g", "c"]
                .map(|id| {
                    let mut facts = AllocationFacts::default();
                    facts
                        .unavailable
                        .insert(ResourceKey::Ram, [YieldStep::Hint].into());
                    (id.to_string(), facts)
                })
                .into(),
            ..PolicyInput::default()
        };
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snapshot(), &input, 0)
            .unwrap();
        assert_eq!(
            step(&decision, "o", ResourceKey::Ram),
            Some((YieldStep::Drain, YieldCause::EnvelopeBreach)),
            "{:?}",
            decision.actions
        );
        assert_eq!(step(&decision, "g", ResourceKey::Ram), None);
        let cpu = step(&decision, "c", ResourceKey::Cpu).unwrap();
        assert!(
            cpu.0 < YieldStep::Drain && cpu.0 > YieldStep::None,
            "{cpu:?}"
        );
        assert_eq!(cpu.1, YieldCause::EnvelopeBreach);
        assert_eq!(decision.would_drain, ["o"]);
    }

    #[test]
    fn spool_exhaustion_blocks_admission_and_drains_nothing() {
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "o",
                    AllocationClass::Opportunistic,
                    (500, 1000),
                    (500, 1000),
                ),
                allocation("g", AllocationClass::Guaranteed, (500, 1000), (500, 1000)),
            ],
            spool_full: true,
            ..PolicyInput::default()
        };
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snapshot(), &input, 0)
            .unwrap();
        assert!(decision.actions.is_empty(), "{:?}", decision.actions);
        assert!(decision.would_drain.is_empty());
        assert!(!decision.expansion_allowed && !decision.cpu_ram_expansion_allowed);
        assert_eq!(decision.admission_headroom.cpu_millicores, 0);
        assert_eq!(decision.admission_headroom.ram_mib, 0);
        assert!(decision.reasons.iter().any(|r| r.contains("spool")));
    }

    #[test]
    fn an_explicit_operator_drain_still_drains_every_allocation() {
        let input = PolicyInput {
            allocations: vec![
                allocation(
                    "o",
                    AllocationClass::Opportunistic,
                    (500, 1000),
                    (500, 1000),
                ),
                allocation("g", AllocationClass::Guaranteed, (500, 1000), (500, 1000)),
            ],
            explicit_drain: true,
            ..PolicyInput::default()
        };
        let decision = PolicyEngine::new()
            .evaluate(&config(), &snapshot(), &input, 0)
            .unwrap();
        let mut drained = decision.would_drain.clone();
        drained.sort();
        assert_eq!(drained, ["g", "o"]);
        assert!(
            decision
                .actions
                .iter()
                .all(|a| a.cause == YieldCause::OperatorDrain)
        );
    }
}
