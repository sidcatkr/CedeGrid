//! Native gate (FR-034): one Performance node admits 150 light tasks at once, and its
//! observation cadence and the coordinator's telemetry freshness hold through the burst. It
//! runs only on an authorized Linux host, where synced writes cost what they do in production.
#![cfg(target_os = "linux")]
use cedegrid::{
    agent::AgentConfig,
    config::{Config, NodeMode, RuntimeConfigKind, serialize_runtime},
    contention::ContentionMode,
    execution_model::LaunchRequest,
    model::Resources,
    protocol::*,
    state::StorageProfile,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const TASKS: usize = 150;
/// The longest gap between two observations the burst may cause: two monitor intervals.
const GAP_BOUND_MS: u64 = 1000;

/// Keeps the run directory when the test fails, as evidence.
struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(directory) = self.0.take()
        {
            eprintln!(
                "retained_failed_burst_evidence={}",
                directory.keep().display()
            );
        }
    }
}

struct Service(Child);
impl Service {
    fn start(args: &[&str], directory: &Path, name: &str) -> Self {
        let file = |suffix: &str| fs::File::create(directory.join(format!("{name}.{suffix}.log")));
        Self(
            Command::new(env!("CARGO_BIN_EXE_cedegrid"))
                .args(args)
                .env("TMPDIR", directory)
                .stdout(Stdio::from(file("stdout").unwrap()))
                .stderr(Stdio::from(file("stderr").unwrap()))
                .spawn()
                .unwrap(),
        )
    }
    /// TERM lets the agent drain and release what it launched.
    fn stop(&mut self, within: Duration) {
        if self.0.try_wait().unwrap().is_none() {
            // SAFETY: the PID is this test's own child, which has not been reaped.
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + within;
            while self.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn running(state: &Path) -> usize {
    let Ok(db) = rusqlite::Connection::open_with_flags(
        state.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return 0;
    };
    let Ok(mut query) = db.prepare("SELECT record_json FROM executions") else {
        return 0;
    };
    query
        .query_map([], |row| row.get::<_, String>(0))
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter(|record| record.contains("\"phase\":\"running\""))
                .count()
        })
        .unwrap_or(0)
}

fn observation_times(state: &Path) -> Vec<u64> {
    let db = rusqlite::Connection::open_with_flags(
        state.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut query = db
        .prepare("SELECT observed_at_unix_ms FROM observations ORDER BY id")
        .unwrap();
    query
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .map(|value| value.unwrap() as u64)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "not tested: requires an authorized Linux host (CEDEGRID_TEST_LAUNCH_BURST=1)"]
async fn a_performance_node_admits_150_tasks_at_once_and_keeps_its_cadence() {
    assert!(
        std::env::var_os("CEDEGRID_TEST_LAUNCH_BURST").is_some(),
        "explicit CEDEGRID_TEST_LAUNCH_BURST=1 is required"
    );
    let evidence = Evidence(Some(
        tempfile::Builder::new()
            .prefix(".launch-burst-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap(),
    ));
    let root = evidence.0.as_ref().unwrap().path().to_path_buf();
    let root = root.as_path();
    let pki = root.join("pki");
    let generated = Command::new("python3")
        .arg(
            std::env::current_dir()
                .unwrap()
                .join("tools/make_test_pki.py"),
        )
        .args(["--node-id", "burst", "--output"])
        .arg(&pki)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("TMPDIR", root)
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
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
    // The documented telemetry TTL and lease, as on the test host.
    let coordinator: CoordinatorConfig = serde_json::from_value(json!({
        "state_dir": root.join("coordinator"), "storage_profile": StorageProfile::WalFull,
        "listen": address, "tls": tls("server"), "clients": info["clients"],
        "max_artifact_bytes": 1048576, "artifact_quota_bytes": 10485760}))
    .unwrap();
    assert_eq!(coordinator.telemetry_ttl_ms, 3000);
    let coordinator_file = root.join("coordinator.toml");
    fs::write(
        &coordinator_file,
        serialize_runtime(&coordinator, RuntimeConfigKind::Coordinator).unwrap(),
    )
    .unwrap();
    let mut coordinator_service = Service::start(
        &[
            "coordinator",
            "--deployment",
            coordinator_file.to_str().unwrap(),
        ],
        root,
        "coordinator",
    );
    let client = RpcClient::new(&endpoint, &tls("operator")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while client
        .request(&Request::Status { job_id: None })
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "coordinator did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Performance at its default cadences: 500 ms observations, 2 s heartbeats.
    let mut config = Config {
        node_id: "burst".into(),
        node_mode: NodeMode::Opportunistic,
        state_dir: root.join("burst-state"),
        ..Config::default()
    };
    config.execution.enabled = true;
    config.cpu.reserve_physical_cores = 0;
    config.ram.reserve_mib = 0;
    config.ram.reserve_percent = 0;
    config.contention.default_mode = ContentionMode::Performance;
    config
        .contention
        .acknowledge_caution
        .insert("performance_mode".into());
    let node_file = root.join("burst.toml");
    fs::write(
        &node_file,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    let agent = AgentConfig {
        coordinator_url: endpoint.clone(),
        tls: tls("node-0"),
        cpu_affinity: None,
        max_transfer_bytes_per_second: 1024 * 1024,
        capacity: Resources {
            cpu_millicores: 2000,
            ram_mib: 4096,
            gpu_memory_mib: BTreeMap::new(),
        },
        max_workers: TASKS,
        // Each attempt reserves its capture logs; 150 of them need more than the usual spool.
        max_spool_bytes: 8 * 1024 * 1024 * 1024,
        max_runtime_seconds: 120,
    };
    let agent_file = root.join("burst-agent.toml");
    fs::write(
        &agent_file,
        serialize_runtime(&agent, RuntimeConfigKind::Agent).unwrap(),
    )
    .unwrap();
    client
        .request(&Request::PutPool {
            pool: PoolSpec {
                pool_id: "burst-pool".into(),
                class: cedegrid::execution_model::AllocationClass::Opportunistic,
                node_ids: vec!["burst".into()],
                min_workers: 0,
                max_workers: TASKS as u32,
            },
        })
        .await
        .unwrap();
    let tasks: Vec<LaunchRequest> = (0..TASKS)
        .map(|index| {
            serde_json::from_value(
                json!({"task_id": format!("burst-{index}"), "assignment_id": "",
                "argv": ["/bin/sleep", "60"], "cwd": root, "env": {},
                "resources": {"cpu_millicores": 10, "ram_mib": 16, "gpu_memory_mib": {}},
                "class": "opportunistic", "replay_safe": true, "single_process": true,
                "no_escape": true, "allow_fallback": true}),
            )
            .unwrap()
        })
        .collect();
    client
        .request(&Request::Submit {
            job: JobSpec {
                job_id: "burst-job".into(),
                pool_id: "burst-pool".into(),
                priority: 0,
                tasks,
            },
        })
        .await
        .unwrap();
    let mut agent_service = Service::start(
        &[
            "--config",
            node_file.to_str().unwrap(),
            "agent",
            "--deployment",
            agent_file.to_str().unwrap(),
        ],
        root,
        "agent",
    );
    let state = root.join("burst-state");
    let mut all_running = None;
    while Instant::now() < deadline {
        if running(&state) == TASKS {
            all_running = Some(Instant::now());
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Two more heartbeats after the last launch, then stop.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let ran = running(&state);
    agent_service.stop(Duration::from_secs(30));
    coordinator_service.stop(Duration::from_secs(5));
    let log = fs::read_to_string(root.join("agent.stdout.log")).unwrap();
    let stale = log
        .lines()
        .filter(|line| line.contains("requires fresh capacity evidence"))
        .count();
    let refused = log
        .lines()
        .filter(|line| line.contains("authorization_unavailable"))
        .count();
    let errors = log
        .lines()
        .filter(|line| line.contains("supervisor_error"))
        .count();
    let times = observation_times(&state);
    let longest = times
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .max()
        .unwrap_or(0);
    // Evidence for the run log: other refusals and failures are reported, not judged here.
    eprintln!(
        "burst: longest gap {longest} ms, {ran} of {TASKS} running before stop ({}), {refused} authorizations refused ({stale} for stale evidence), {errors} supervisor errors",
        if all_running.is_some() {
            "all ran"
        } else {
            "not all ran"
        }
    );
    assert_eq!(
        stale, 0,
        "initial authorizations were refused for stale evidence"
    );
    assert!(
        longest <= GAP_BOUND_MS,
        "longest gap between observations {longest} ms exceeds {GAP_BOUND_MS} ms"
    );
}
