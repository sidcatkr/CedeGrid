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

/// Keeps the run directory when the test fails, or always with `CEDEGRID_TEST_BURST_KEEP=1`,
/// so the agent's and coordinator's logs can be read after a passing run too.
struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) {
        let failed = std::thread::panicking();
        let keep = failed || std::env::var_os("CEDEGRID_TEST_BURST_KEEP").is_some();
        if keep && let Some(directory) = self.0.take() {
            let label = if failed { "failed_" } else { "" };
            eprintln!(
                "retained_{label}burst_evidence={}",
                directory.keep().display()
            );
        } else if let Some(directory) = &self.0
            && let Ok(control) = cedegrid::namespace::control_dir_for(directory.path())
            && control.exists()
        {
            // The namespace control directory beside the state root goes with it.
            let _ = std::fs::remove_dir_all(control);
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
    /// TERM stops the agent; it leaves running supervisors to the next agent (FR-080).
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

/// Runs whose journal reads Released, and runs that ever recorded Draining.
fn released_and_drained(state: &Path) -> (usize, usize) {
    let Ok(db) = rusqlite::Connection::open_with_flags(
        state.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return (0, 0);
    };
    let count = |sql: &str| {
        db.query_row(sql, [], |row| row.get::<_, i64>(0))
            .map_or(0, |n| n as usize)
    };
    (
        count(
            "SELECT count(*) FROM executions WHERE json_extract(record_json,'$.phase')='released'",
        ),
        count(
            "SELECT count(DISTINCT assignment_id) FROM execution_events WHERE json_extract(record_json,'$.phase')='draining'",
        ),
    )
}

/// Allocations the coordinator fenced because their lease expired.
fn lease_expiries(coordinator: &Path) -> usize {
    let Ok(db) = rusqlite::Connection::open_with_flags(
        coordinator.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return 0;
    };
    db.query_row(
        "SELECT count(*) FROM allocation_fences WHERE reason='lease expired'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map_or(0, |n| n as usize)
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
        // The transfer budget, 1 MiB/s unless the run sets another (two budgets are run).
        max_transfer_bytes_per_second: std::env::var("CEDEGRID_TEST_BURST_TRANSFER_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1024 * 1024),
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
    // Two more heartbeats after the last launch, then an operator drain of the node, which
    // reaches each run with its next lease renewal or heartbeat reply, then TERM, which leaves
    // no supervisor once every run is released.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let ran = running(&state);
    let expired = lease_expiries(&root.join("coordinator"));
    client
        .request(&Request::DrainNode {
            node_id: "burst".into(),
            drain: true,
        })
        .await
        .unwrap();
    let drain_started = Instant::now();
    let drain_started_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let drained = drain_started + Duration::from_secs(60);
    // The supervisors' journal waits and their release times, read while the attempt
    // directories exist: they are reclaimed after release.
    let mut usage: BTreeMap<std::path::PathBuf, (Option<u64>, Option<u64>)> = BTreeMap::new();
    let mut collect = |state: &Path| {
        for attempt in fs::read_dir(state.join("attempts"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        {
            let wait = fs::read(attempt.join("supervisor-usage.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|usage| usage["journal_longest_wait_ms"].as_u64());
            let released = fs::metadata(attempt.join("execution-outcome.json"))
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|time| time.as_millis() as u64);
            let entry = usage.entry(attempt).or_default();
            entry.0 = entry.0.or(wait);
            entry.1 = entry.1.or(released);
        }
    };
    while running(&state) > 0 && Instant::now() < drained {
        collect(&state);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    collect(&state);
    let drain_ms = drain_started.elapsed().as_millis();
    let left = running(&state);
    // "None running" counts a run that reads Draining as stopped; the releases come later, once
    // a worker that ignores the drain file has had its TERM.
    while released_and_drained(&state).0 < TASKS && Instant::now() < drained {
        collect(&state);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    collect(&state);
    let released_ms = drain_started.elapsed().as_millis();
    let (released, recorded_draining) = released_and_drained(&state);
    // The coordinator fences an expired lease only when a request advances its clock.
    client.request(&Request::Status { job_id: None }).await.ok();
    let expired_in_drain = lease_expiries(&root.join("coordinator")).saturating_sub(expired);
    let term_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    agent_service.stop(Duration::from_secs(30));
    client.request(&Request::Status { job_id: None }).await.ok();
    let expired_after_drain = lease_expiries(&root.join("coordinator")).saturating_sub(expired);
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
    // The stop's own iterations: the gaps from the drain's start until the agent's last
    // observation.
    let longest_in_stop = times
        .windows(2)
        .filter(|pair| pair[1] >= drain_started_unix_ms)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .max()
        .unwrap_or(0);
    // Each phase's longest gap: before the drain, from the drain to TERM, and after TERM.
    let phase = |from: u64, to: u64| {
        times
            .windows(2)
            .filter(|pair| pair[1] > from && pair[1] <= to)
            .map(|pair| pair[1].saturating_sub(pair[0]))
            .max()
            .unwrap_or(0)
    };
    let first = times.first().copied().unwrap_or(drain_started_unix_ms);
    let count = |event: &str| {
        log.lines()
            .filter(|line| line.contains(&format!("\"event\":\"{event}\"")))
            .count()
    };
    eprintln!(
        "burst phases: drain at {} ms and TERM at {} ms after the first observation; longest gap {} ms before the drain, {} ms from the drain to TERM, {} ms after TERM; leases expired {expired} before the drain, {expired_in_drain} during it and {expired_after_drain} after it in all; events: lease_unavailable {}, coordinator_disconnected {}, drain_unsent {}, stop_uncertain {}",
        drain_started_unix_ms.saturating_sub(first),
        term_unix_ms.saturating_sub(first),
        phase(0, drain_started_unix_ms),
        phase(drain_started_unix_ms, term_unix_ms),
        phase(term_unix_ms, u64::MAX),
        count("lease_unavailable"),
        count("coordinator_disconnected"),
        count("drain_unsent"),
        count("stop_uncertain"),
    );
    // The last release after the drain, and the longest journal lock wait any process saw,
    // from what was read while the attempt directories existed.
    let last_release = usage
        .values()
        .filter_map(|(_, released)| *released)
        .max()
        .map(|at| at.saturating_sub(drain_started_unix_ms));
    let wait_of = |value: &serde_json::Value| value["journal_longest_wait_ms"].as_u64();
    let supervisor_wait = usage.values().filter_map(|(wait, _)| *wait).max();
    let agent_wait = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "loop_cost")
        .filter_map(|event| wait_of(&event))
        .max();
    eprintln!(
        "burst journal: last release {} ms after the drain began; longest journal wait {} ms in a supervisor ({} attempts read), {} ms in the agent (none when no loop_cost period ended); journal_busy events {}",
        last_release.map_or("none".into(), |ms| ms.to_string()),
        supervisor_wait.map_or("none".into(), |ms| ms.to_string()),
        usage.len(),
        agent_wait.map_or("none".into(), |ms| ms.to_string()),
        count("journal_busy"),
    );
    eprintln!(
        "burst release: none running {drain_ms} ms after the drain began, {released} of {TASKS} released by {released_ms} ms; {recorded_draining} of {TASKS} recorded Draining"
    );
    // Evidence for the run log: other refusals and failures are reported, not judged here.
    eprintln!(
        "burst: longest gap {longest} ms, {ran} of {TASKS} running before stop ({}), {left} left running after the drain, the drain took {drain_ms} ms with a longest gap of {longest_in_stop} ms, {expired} leases expired, {refused} authorizations refused ({stale} for stale evidence), {errors} supervisor errors",
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
    assert_eq!(left, 0, "the operator drain released every run");
    // B1's gates: no lease fenced after the drain and no supervisor error.
    assert_eq!(
        expired_after_drain, 0,
        "leases expired after the drain began"
    );
    assert_eq!(errors, 0, "supervisor errors during the burst stop");
    assert!(
        longest <= GAP_BOUND_MS,
        "longest gap between observations {longest} ms exceeds {GAP_BOUND_MS} ms"
    );
}
