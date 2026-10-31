//! Opt-in measurement (SC-014): CedeGrid's own cost with 117 tasks on one node. It reports the
//! agent's and its supervisors' CPU and resident memory, and the agent loop's costs by stage in
//! thread CPU time. Light tasks leave CedeGrid's cost apart from own work's. It runs only on
//! Linux, where the numbers are runtime evidence.
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

const TASKS: usize = 117;
/// The steady-state window measured once every task runs.
const WINDOW: Duration = Duration::from_secs(60);

/// Keeps the run directory when the test fails, as evidence.
struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(directory) = self.0.take()
        {
            eprintln!(
                "retained_failed_cost_evidence={}",
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

/// The agent's direct children: its supervisors.
fn supervisors(agent: u32) -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                // The parent PID is the second field after the command's closing parenthesis.
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
                    == Some(agent)
            })
        })
        .collect()
}

/// A process's CPU time, all threads, from its process clock.
fn cpu_ns(pid: u32) -> Option<u64> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: valid output pointer; the call only names the clock.
    if unsafe { libc::clock_getcpuclockid(pid as libc::pid_t, &mut clock) } != 0 {
        return None;
    }
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid output pointer.
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return None;
    }
    Some(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

/// Resident memory in KiB, from `file` and its line starting with `field`.
fn kib(pid: u32, file: &str, field: &str) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/{file}"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Resident memory, shared pages counted in full.
fn rss_kib(pid: u32) -> Option<u64> {
    kib(pid, "status", "VmRSS:")
}

/// Proportional resident memory: shared pages divided among the processes sharing them.
fn pss_kib(pid: u32) -> Option<u64> {
    kib(pid, "smaps_rollup", "Pss:")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "opt-in measurement: CEDEGRID_TEST_OWN_COST=1 on Linux"]
async fn cedegrid_costs_at_117_tasks() {
    assert!(
        std::env::var_os("CEDEGRID_TEST_OWN_COST").is_some(),
        "explicit CEDEGRID_TEST_OWN_COST=1 is required"
    );
    let evidence = Evidence(Some(StateDir(
        tempfile::Builder::new()
            .prefix(".own-cost-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap(),
    )));
    let root = evidence.0.as_ref().unwrap().path().to_path_buf();
    let root = root.as_path();
    let pki = root.join("pki");
    let generated = Command::new("python3")
        .arg(
            std::env::current_dir()
                .unwrap()
                .join("tools/make_test_pki.py"),
        )
        .args(["--node-id", "cost", "--output"])
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
    let coordinator: CoordinatorConfig = serde_json::from_value(json!({
        "state_dir": root.join("coordinator"), "storage_profile": StorageProfile::WalFull,
        "listen": address, "tls": tls("server"), "clients": info["clients"],
        "max_artifact_bytes": 1048576, "artifact_quota_bytes": 10485760}))
    .unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(120);
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
    let mut config = Config {
        node_id: "cost".into(),
        node_mode: NodeMode::Opportunistic,
        state_dir: root.join("cost-state"),
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
    let node_file = root.join("cost.toml");
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
        max_spool_bytes: 8 * 1024 * 1024 * 1024,
        max_runtime_seconds: 600,
    };
    let agent_file = root.join("cost-agent.toml");
    fs::write(
        &agent_file,
        serialize_runtime(&agent, RuntimeConfigKind::Agent).unwrap(),
    )
    .unwrap();
    client
        .request(&Request::PutPool {
            pool: PoolSpec {
                pool_id: "cost-pool".into(),
                class: cedegrid::execution_model::AllocationClass::Opportunistic,
                node_ids: vec!["cost".into()],
                min_workers: 0,
                max_workers: TASKS as u32,
            },
        })
        .await
        .unwrap();
    let tasks: Vec<LaunchRequest> = (0..TASKS)
        .map(|index| {
            serde_json::from_value(
                json!({"task_id": format!("cost-{index}"), "assignment_id": "",
                "argv": ["/bin/sleep", "600"], "cwd": root, "env": {},
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
                job_id: "cost-job".into(),
                pool_id: "cost-pool".into(),
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
    let agent_pid = agent_service.0.id();
    let state = root.join("cost-state");
    while running(&state) < TASKS {
        assert!(Instant::now() < deadline, "not every task ran");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Settle past the launches, then measure the steady state.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let pids = supervisors(agent_pid);
    let read = |pids: &[u32]| -> (u64, u64) {
        let agent = cpu_ns(agent_pid).unwrap();
        let supervisors = pids.iter().filter_map(|pid| cpu_ns(*pid)).sum();
        (agent, supervisors)
    };
    let (agent_before, supervisors_before) = read(&pids);
    let started = Instant::now();
    tokio::time::sleep(WINDOW).await;
    let (agent_after, supervisors_after) = read(&pids);
    let wall = started.elapsed().as_nanos() as u64;
    let millicores = |ns: u64| ns * 1000 / wall;
    let agent_cpu = millicores(agent_after - agent_before);
    let supervisor_cpu = millicores(supervisors_after - supervisors_before);
    let agent_rss = rss_kib(agent_pid).unwrap();
    let agent_pss = pss_kib(agent_pid);
    let mut supervisor_rss: Vec<u64> = pids.iter().filter_map(|pid| rss_kib(*pid)).collect();
    supervisor_rss.sort_unstable();
    let mut supervisor_pss: Vec<u64> = pids.iter().filter_map(|pid| pss_kib(*pid)).collect();
    supervisor_pss.sort_unstable();
    let ran = running(&state);
    agent_service.stop(Duration::from_secs(60));
    coordinator_service.stop(Duration::from_secs(5));
    let log = fs::read_to_string(root.join("agent.stdout.log")).unwrap();
    let stages = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|event| event["event"] == "loop_cost")
        .expect("the agent reported its loop cost");
    let capacity = std::thread::available_parallelism().unwrap().get() as u64 * 1000;
    let report = json!({
        "tasks": TASKS, "running": ran, "supervisors": pids.len(), "capacity_millicores": capacity,
        "window_ms": wall / 1_000_000,
        "agent_cpu_millicores": agent_cpu, "supervisors_cpu_millicores": supervisor_cpu,
        "cedegrid_percent_of_capacity": (agent_cpu + supervisor_cpu) as f64 * 100.0 / capacity as f64,
        "agent_rss_kib": agent_rss,
        "supervisor_rss_kib_median": supervisor_rss.get(supervisor_rss.len() / 2),
        "supervisors_rss_kib_total": supervisor_rss.iter().sum::<u64>(),
        "agent_pss_kib": agent_pss,
        "supervisor_pss_kib_median": supervisor_pss.get(supervisor_pss.len() / 2),
        "supervisors_pss_kib_total": supervisor_pss.iter().sum::<u64>(),
        "agent_loop_cost": stages,
    });
    eprintln!("own_cost={report}");
    assert_eq!(pids.len(), TASKS, "one supervisor per task");
    assert_eq!(ran, TASKS, "every task ran through the window");
}

include!("support/state_dir.rs");
