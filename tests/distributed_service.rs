//! Two actual local node services over mTLS; this is not two-machine Linux evidence.
#![cfg(unix)]
use cedegrid::{
    agent::AgentConfig,
    config::{Config, NodeMode, RuntimeConfigKind, serialize_runtime},
    coordinator::Coordinator,
    execution_model::{
        AllocationClass, ExecutionPhase, ExecutionRecord, LaunchRequest, NamedArtifact,
    },
    model::Resources,
    protocol::*,
    state::{StateStore, StorageProfile},
};
use rusqlite::OptionalExtension;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct FailureEvidence(Option<tempfile::TempDir>);
impl FailureEvidence {
    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}
impl Drop for FailureEvidence {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(directory) = self.0.take()
        {
            eprintln!(
                "retained_failed_service_evidence={}",
                directory.keep().display()
            );
        }
    }
}
struct OwnedService {
    child: Child,
    log: PathBuf,
}
impl OwnedService {
    fn start(args: &[&str], directory: &Path, name: &str) -> Self {
        let log = directory.join(format!("{name}.stderr.log"));
        let child = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
            .args(args)
            .env("TMPDIR", directory)
            .stdout(Stdio::from(
                fs::File::create(directory.join(format!("{name}.stdout.log"))).unwrap(),
            ))
            .stderr(Stdio::from(fs::File::create(&log).unwrap()))
            .spawn()
            .unwrap();
        Self { child, log }
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}
impl Drop for OwnedService {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if std::thread::panicking() {
            eprintln!(
                "{}: {}",
                self.log.display(),
                fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}
async fn ready(client: &RpcClient, deadline: Instant) {
    loop {
        if client
            .request(&Request::Status { job_id: None })
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "coordinator did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
async fn result(client: &RpcClient, task: &str, deadline: Instant) -> ResultSubmission {
    loop {
        if let Response::Result {
            submission: Some(value),
        } = client
            .request(&Request::GetResult {
                task_id: task.into(),
            })
            .await
            .unwrap()
        {
            let Response::Status { tasks, .. } = client
                .request(&Request::Status {
                    job_id: Some(format!("job-{task}")),
                })
                .await
                .unwrap()
            else {
                panic!("result receipt status missing");
            };
            let receipt = tasks.iter().find(|record| record.task_id == task).unwrap();
            assert_eq!(
                receipt.receipt_hash.as_deref(),
                Some(hex::encode(Sha256::digest(serde_json::to_vec(&value).unwrap())).as_str())
            );
            return value;
        }
        assert!(Instant::now() < deadline, "result timeout: {task}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn local_phase(state: &Path, task: &str, phase: ExecutionPhase, deadline: Instant) {
    loop {
        if let Ok(store) = StateStore::open_read_only(state)
            && store
                .executions()
                .unwrap()
                .iter()
                .any(|r| r.task_id == task && r.phase == phase)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "local phase timeout: {task} {phase:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
async fn checkpoint(
    client: &RpcClient,
    state: &Path,
    profile: StorageProfile,
    task: &str,
    deadline: Instant,
) -> ExecutionRecord {
    loop {
        let Response::Status {
            tasks, allocations, ..
        } = client
            .request(&Request::Status {
                job_id: Some(format!("job-{task}")),
            })
            .await
            .unwrap()
        else {
            panic!("checkpoint status missing")
        };
        let current = tasks.iter().find(|r| r.task_id == task);
        if let Ok(store) = StateStore::open_read_only_with_profile(state, profile) {
            for record in store
                .executions()
                .unwrap()
                .iter()
                .filter(|r| r.task_id == task && r.phase == ExecutionPhase::Running)
            {
                if !current.is_some_and(|task| {
                    task.assignment_id.as_deref() == Some(&record.assignment_id)
                        && task.generation == record.generation
                }) || !allocations.iter().any(|a| {
                    a["assignment_id"] == record.assignment_id
                        && a["generation"] == record.generation
                        && a["phase"] != "released"
                        && a["phase"] != "uncertain"
                        && a["lease_sequence"].as_u64().is_some_and(|s| s > 0)
                }) {
                    continue;
                }
                let output = state.join("attempts").join(&record.assignment_id);
                if fs::read_dir(output).is_ok_and(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if !name.starts_with("checkpoint-") || !name.ends_with(".receipt.json") {
                            return false;
                        }
                        let receipt: serde_json::Value =
                            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
                        let submission: ResultSubmission =
                            serde_json::from_value(receipt["submission"].clone()).unwrap();
                        assert_eq!(submission.assignment_id, record.assignment_id);
                        assert_eq!(submission.generation, record.generation);
                        assert_eq!(
                            receipt["response"]["receipt"]["receipt_hash"],
                            hex::encode(Sha256::digest(serde_json::to_vec(&submission).unwrap()))
                        );
                        true
                    })
                }) {
                    return record.clone();
                }
            }
        }
        assert!(Instant::now() < deadline, "checkpoint timeout: {task}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
async fn released(client: &RpcClient, node: &str, deadline: Instant) {
    loop {
        let Response::Status { allocations, .. } = client
            .request(&Request::Status { job_id: None })
            .await
            .unwrap()
        else {
            panic!()
        };
        if allocations
            .iter()
            .filter(|a| a["node_id"] == node)
            .all(|a| a["phase"] == "released")
        {
            return;
        }
        assert!(Instant::now() < deadline, "release timeout: {node}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn lose_running_journal_row(
    client: &RpcClient,
    state: &Path,
    profile: StorageProfile,
    task: &str,
    deadline: Instant,
) -> ExecutionRecord {
    use std::os::unix::fs::MetadataExt;
    loop {
        let record = checkpoint(client, state, profile, task, deadline).await;
        let database = state.join(cedegrid::state::DATABASE_FILENAME);
        let before = fs::metadata(&database).unwrap();
        let deleted = {
            let mut db = rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            db.busy_timeout(Duration::from_secs(2)).unwrap();
            db.pragma_update(None, "foreign_keys", "ON").unwrap();
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let live: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM executions WHERE assignment_id=?1",
                    [&record.assignment_id],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            let live = live.map(|value| serde_json::from_str::<ExecutionRecord>(&value).unwrap());
            if live.is_some_and(|value| {
                value.phase == ExecutionPhase::Running
                    && value.generation == record.generation
                    && value.identity == record.identity
            }) {
                // Recheck under the actual SQLite write transaction. A naturally
                // released historical attempt is never the rollback target.
                tx.execute(
                    "DELETE FROM execution_events WHERE assignment_id=?1",
                    [&record.assignment_id],
                )
                .unwrap();
                assert_eq!(
                    tx.execute(
                        "DELETE FROM executions WHERE assignment_id=?1",
                        [&record.assignment_id]
                    )
                    .unwrap(),
                    1
                );
                tx.commit().unwrap();
                true
            } else {
                false
            }
        };
        if deleted {
            let after = fs::metadata(&database).unwrap();
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "no current running checkpointed attempt to roll back"
        );
    }
}

async fn new_recovery_snapshot(
    state: &Path,
    old_session: &str,
    deadline: Instant,
) -> ReplayRecoverySnapshot {
    loop {
        // This startup evidence file is written before the ready event. Its
        // existence alone does not establish that serialization has completed.
        if let Ok(bytes) = fs::read(state.join("replay-recovery.json"))
            && let Ok(snapshot) = serde_json::from_slice::<ReplayRecoverySnapshot>(&bytes)
            && snapshot.session_id != old_session
        {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "fresh authenticated replay recovery snapshot missing"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn node_config(
    root: &Path,
    id: &str,
    mode: NodeMode,
    storage_profile: StorageProfile,
) -> (Config, PathBuf) {
    let mut config = Config {
        node_id: id.into(),
        node_mode: mode,
        state_dir: root.join(format!("{id}-state")),
        storage_profile,
        ..Config::default()
    };
    config.execution.enabled = true;
    config.execution.prepare_timeout_ms = 2000;
    config.execution.release_confirm_timeout_ms = 1000;
    config.cpu.reserve_physical_cores = 0;
    config.ram.reserve_mib = 0;
    config.ram.reserve_percent = 0;
    config.gpu.scale_up_cooldown_ms = 0;
    // Constrained Linux validation can use the normal 500ms observation cadence.
    // The 50ms fixture default leaves only 150ms before a sample is stale; an
    // ARM VM's synchronous launch preparation can exceed that interval. Keep
    // unknown/stale yielding and the whole-test 50-second deadline unchanged.
    let monitor_ms = std::env::var("CEDEGRID_SERVICE_TEST_MONITOR_MS")
        .map(|value| {
            value
                .parse::<u64>()
                .expect("monitor override must be milliseconds")
        })
        .unwrap_or(50);
    assert!(
        (50..=500).contains(&monitor_ms),
        "monitor override must be in 50..=500ms"
    );
    config.monitor.interval_ms = monitor_ms;
    config.lifecycle.heartbeat_interval_ms = 50;
    config.lifecycle.allocation_lease_ms = 3000;
    config.lifecycle.drain_timeout_ms = 150;
    config.lifecycle.term_grace_ms = 100;
    let file = root.join(format!("{id}.toml"));
    fs::write(
        &file,
        serialize_runtime(&config, RuntimeConfigKind::Node).unwrap(),
    )
    .unwrap();
    (config, file)
}
const WORKER: &str = r#"from cedegrid import WorkerContext
from pathlib import Path
import json, os, time
c=WorkerContext.from_env()
seed=int(os.environ['TEST_SEED'])
mode=os.environ.get('TEST_MODE','normal')
observations=[18.400785964971874,31.859559996519238]
if c.inputs:
    model=json.loads(Path(c.inputs[0]['path']).read_text())
    assert model['experiment_id']=='local-sdk-experiment'
if mode=='hold' and not Path(os.environ['TEST_RESUME_ALLOWED']).is_file():
    score=sum(i*i for i in range(seed,seed+128))
    p=c.output/'partial.json'; p.write_text(json.dumps({'score':score}))
    c.checkpoint({'experiment_id':'local-sdk-experiment','next':128,'seed':seed,'observations':observations},[c.artifact('partial.json',p)])
    end=time.monotonic()+12
    while not c.draining() and time.monotonic()<end: time.sleep(.02)
    if not c.draining(): raise RuntimeError('bounded drain did not arrive')
    raise SystemExit(75)
if c.resume:
    assert c.resume['metadata']['seed']==seed
    assert c.resume['metadata']['observations']==observations
    score=json.loads(Path(c.resume['artifacts'][0]['path']).read_text())['score']
    score+=sum(i*i for i in range(seed+128,seed+256))
else:
    score=sum(i*i for i in range(seed,seed+256))
time.sleep(float(os.environ.get('TEST_DELAY','0.05')))
metadata={'experiment_id':'local-sdk-experiment','node':os.environ['TEST_NODE'],'seed':seed,'score':score,'resumed':bool(c.resume),'model_version':os.environ.get('TEST_MODEL_VERSION','1'),'observations':observations}
p=c.output/'result.json.data'; p.write_text(json.dumps(metadata))
c.complete(metadata,[c.artifact('result.json.data',p)])
"#;
fn request(root: &Path, node: &str, id: &str, seed: u64, mode: &str, delay: f64) -> LaunchRequest {
    let repo = std::env::current_dir().unwrap();
    serde_json::from_value(json!({"task_id":id,"assignment_id":"","argv":[std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_|"python3".into()),"-c",WORKER],"cwd":root,"env":{"PYTHONPATH":repo.join("python"),"PYTHONDONTWRITEBYTECODE":"1","TEST_NODE":node,"TEST_SEED":seed.to_string(),"TEST_MODE":mode,"TEST_DELAY":delay.to_string(),"TEST_RESUME_ALLOWED":root.join("allow-explicit-resume")},"resources":{"cpu_millicores":100,"ram_mib":128,"gpu_memory_mib":{}},"class":if node=="anchor"{"guaranteed"}else{"opportunistic"},"replay_safe":true,"single_process":true,"no_escape":true,"allow_fallback":true})).unwrap()
}
async fn submit(client: &RpcClient, node: &str, request: LaunchRequest) {
    client
        .request(&Request::Submit {
            job: JobSpec {
                job_id: format!("job-{}", request.task_id),
                pool_id: format!("{node}-pool"),
                priority: 0,
                tasks: vec![request],
            },
        })
        .await
        .unwrap();
}
fn check_result(value: &ResultSubmission, node: &str, seed: u64) {
    let metadata = &value.result["metadata"];
    for (actual, expected) in metadata["observations"]
        .as_array()
        .unwrap()
        .iter()
        .zip([18.400785964971874_f64, 31.859559996519238])
    {
        assert_eq!(actual.as_f64().unwrap().to_bits(), expected.to_bits());
    }
    // Version 2 commits an immutable native descriptor; result() checks the
    // coordinator's durable receipt hash over the exact returned submission.
    assert_eq!(value.result["schema_version"], 2);
    assert_eq!(value.result["kind"], "result");
    assert_eq!(value.result["task_id"], value.task_id);
    assert_eq!(value.result["assignment_id"], value.assignment_id);
    assert_eq!(value.result["generation"], value.generation);
    assert_eq!(metadata["experiment_id"], "local-sdk-experiment");
    assert_eq!(metadata["node"], node);
    assert_eq!(metadata["seed"], seed);
    assert_eq!(
        metadata["score"],
        (seed..seed + 256).map(|i| i * i).sum::<u64>()
    );
}
async fn assert_persisted_profile(state: &Path, profile: StorageProfile, deadline: Instant) {
    let store = loop {
        match StateStore::open_read_only_with_profile(state, profile) {
            Ok(store) => break store,
            Err(error)
                if matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(code, _))
                        if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "profile inspection timeout: {error:#}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("profile inspection failed: {error:#}"),
        }
    };
    assert_eq!(store.storage_profile(), profile);
    let settings = store.durability_settings().unwrap();
    assert_eq!(settings.journal_mode, profile.journal_mode());
    assert_eq!(settings.schema_version, profile.schema_version());
    // Synchronous is connection-local. Each production writable open verifies
    // its own setting; this read-only check verifies persisted profile/journal.
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_real_local_agents_continue_drain_resume_and_recover_one_experiment() {
    connected_lifecycle(StorageProfile::WalFull).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_extra_local_agents_continue_drain_resume_and_recover_one_experiment() {
    connected_lifecycle(StorageProfile::DeleteExtra).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayable_agent_quarantines_corrupt_cache_recovers_reservations_and_committed_checkpoint()
{
    connected_lifecycle(StorageProfile::BurstReplayDeleteExtra).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayable_agent_fences_in_place_logical_rollback_before_authoritative_recovery() {
    connected_lifecycle_with_fault(StorageProfile::BurstReplayDeleteExtra, true).await;
}
async fn connected_lifecycle(storage_profile: StorageProfile) {
    connected_lifecycle_with_fault(storage_profile, false).await;
}
async fn connected_lifecycle_with_fault(storage_profile: StorageProfile, in_place_rollback: bool) {
    let authority_profile = if storage_profile.is_replayable() {
        StorageProfile::WalFull
    } else {
        storage_profile
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(50);
    let dir = FailureEvidence(Some(
        tempfile::Builder::new()
            .prefix(".distributed-service-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap(),
    ));
    let root = dir.path();
    let pki = root.join("pki");
    let generated = Command::new("python3")
        .arg(
            std::env::current_dir()
                .unwrap()
                .join("tools/make_test_pki.py"),
        )
        .args(["--node-id", "anchor", "--node-id", "burst", "--output"])
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
    let cc:CoordinatorConfig=serde_json::from_value(json!({"state_dir":root.join("coordinator"),"storage_profile":authority_profile,"listen":address,"tls":tls("server"),"clients":info["clients"],"lease_ms":3000,"max_artifact_bytes":1048576,"artifact_quota_bytes":10485760})).unwrap();
    let coordinator_config = root.join("coordinator.toml");
    fs::write(
        &coordinator_config,
        serialize_runtime(&cc, RuntimeConfigKind::Coordinator).unwrap(),
    )
    .unwrap();
    let mut coordinator = OwnedService::start(
        &[
            "coordinator",
            "--deployment",
            coordinator_config.to_str().unwrap(),
        ],
        root,
        "coordinator-first",
    );
    let client = RpcClient::new(&endpoint, &tls("operator")).unwrap();
    ready(&client, deadline).await;
    let (anchor_config, anchor_file) =
        node_config(root, "anchor", NodeMode::Guaranteed, authority_profile);
    let (burst_config, burst_file) =
        node_config(root, "burst", NodeMode::Opportunistic, storage_profile);
    let capacity = Resources {
        cpu_millicores: 1000,
        ram_mib: 1024,
        gpu_memory_mib: BTreeMap::new(),
    };
    let deployment = |name: &str, cert: &str| {
        let file = root.join(format!("{name}-agent.toml"));
        let cfg = AgentConfig {
            coordinator_url: endpoint.clone(),
            tls: tls(cert),
            cpu_affinity: None,
            max_transfer_bytes_per_second: 1024 * 1024,
            capacity: capacity.clone(),
            max_workers: 1,
            max_spool_bytes: 10 * 1024 * 1024,
            max_runtime_seconds: 45,
        };
        fs::write(
            &file,
            serialize_runtime(&cfg, RuntimeConfigKind::Agent).unwrap(),
        )
        .unwrap();
        file
    };
    let anchor_deployment = deployment("anchor", "node-0");
    let burst_deployment = deployment("burst", "node-1");
    for (name, class) in [
        ("anchor", AllocationClass::Guaranteed),
        ("burst", AllocationClass::Opportunistic),
    ] {
        client
            .request(&Request::PutPool {
                pool: PoolSpec {
                    pool_id: format!("{name}-pool"),
                    class,
                    node_ids: vec![name.into()],
                    min_workers: 1,
                    max_workers: 1,
                },
            })
            .await
            .unwrap();
    }
    let mut anchor = OwnedService::start(
        &[
            "--config",
            anchor_file.to_str().unwrap(),
            "agent",
            "--deployment",
            anchor_deployment.to_str().unwrap(),
        ],
        root,
        "anchor",
    );
    let mut burst = OwnedService::start(
        &[
            "--config",
            burst_file.to_str().unwrap(),
            "agent",
            "--deployment",
            burst_deployment.to_str().unwrap(),
        ],
        root,
        "burst-first",
    );
    submit(
        &client,
        "anchor",
        request(root, "anchor", "anchor-initial", 1000, "normal", 0.05),
    )
    .await;
    submit(
        &client,
        "burst",
        request(root, "burst", "burst-initial", 2000, "normal", 0.05),
    )
    .await;
    let anchor_initial = result(&client, "anchor-initial", deadline).await;
    let burst_initial = result(&client, "burst-initial", deadline).await;
    check_result(&anchor_initial, "anchor", 1000);
    check_result(&burst_initial, "burst", 2000);
    for (state, profile) in [
        (&cc.state_dir, authority_profile),
        (&anchor_config.state_dir, authority_profile),
        (&burst_config.state_dir, storage_profile),
    ] {
        assert_persisted_profile(state, profile, deadline).await;
    }
    submit(
        &client,
        "burst",
        request(root, "burst", "burst-resume", 3000, "hold", 0.05),
    )
    .await;
    checkpoint(
        &client,
        &burst_config.state_dir,
        storage_profile,
        "burst-resume",
        deadline,
    )
    .await;
    submit(
        &client,
        "anchor",
        request(root, "anchor", "anchor-continuity", 4000, "normal", 0.5),
    )
    .await;
    let lost_attempt = if storage_profile.is_replayable() {
        // Kill only our direct owned agent and preserve its complete cache.
        // Its independent supervisor still owns cleanup; this test never adopts
        // a numeric worker PID for signaling. Authoritative preparation and the
        // accepted checkpoint were committed before the injected cache loss.
        let old_snapshot: ReplayRecoverySnapshot = serde_json::from_slice(
            &fs::read(burst_config.state_dir.join("replay-recovery.json")).unwrap(),
        )
        .unwrap();
        let record;
        if in_place_rollback {
            record = lose_running_journal_row(
                &client,
                &burst_config.state_dir,
                storage_profile,
                "burst-resume",
                deadline,
            )
            .await;
            let failure_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let Response::Status { allocations, .. } = client
                    .request(&Request::Status { job_id: None })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                let retained = allocations
                    .iter()
                    .find(|a| a["assignment_id"] == record.assignment_id)
                    .unwrap();
                assert_ne!(
                    retained["phase"], "released",
                    "lost weak journal row must not prove release"
                );
                if burst.child.try_wait().unwrap().is_some() {
                    break;
                }
                assert!(
                    Instant::now() < failure_deadline,
                    "weak agent did not fail closed after logical journal rollback"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        } else {
            record = checkpoint(
                &client,
                &burst_config.state_dir,
                storage_profile,
                "burst-resume",
                deadline,
            )
            .await;
            burst.stop();
            fs::rename(&burst_config.state_dir, root.join("owned-lost-cache")).unwrap();
            fs::create_dir(&burst_config.state_dir).unwrap();
            fs::write(
                burst_config
                    .state_dir
                    .join(cedegrid::state::DATABASE_FILENAME),
                b"injected owned corrupt cache",
            )
            .unwrap();
        }
        Some((
            record.assignment_id,
            record.generation,
            old_snapshot.session_id,
        ))
    } else {
        None
    };
    client
        .request(&Request::DrainNode {
            node_id: "burst".into(),
            drain: true,
        })
        .await
        .unwrap();
    if lost_attempt.is_some() {
        burst = OwnedService::start(
            &[
                "--config",
                burst_file.to_str().unwrap(),
                "agent",
                "--deployment",
                burst_deployment.to_str().unwrap(),
            ],
            root,
            "burst-corrupt-recovery",
        );
    }
    if let Some((id, generation, old_session)) = &lost_attempt {
        let snapshot = new_recovery_snapshot(&burst_config.state_dir, old_session, deadline).await;
        assert!(
            snapshot
                .allocations
                .iter()
                .any(|a| a.assignment.request.assignment_id == *id
                    && a.assignment.generation == *generation
                    && a.prepared.is_some()
                    && a.lease_sequence > 0)
        );
        let quarantine = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".burst-state.quarantine-")
                    && (in_place_rollback
                        || fs::read(p.join(cedegrid::state::DATABASE_FILENAME))
                            .is_ok_and(|bytes| bytes == b"injected owned corrupt cache"))
            });
        assert!(
            quarantine.is_some(),
            "corrupt input must be retained unchanged in quarantine"
        );
        let preserved = result(&client, "burst-initial", deadline).await;
        assert_eq!(
            serde_json::to_value(preserved).unwrap(),
            serde_json::to_value(&burst_initial).unwrap()
        );
    }
    released(&client, "burst", deadline).await;
    let continuity = result(&client, "anchor-continuity", deadline).await;
    check_result(&continuity, "anchor", 4000);
    assert!(
        StateStore::open_read_only_with_profile(&burst_config.state_dir, storage_profile)
            .unwrap()
            .executions()
            .unwrap()
            .iter()
            .all(|r| r.phase == ExecutionPhase::Released)
    );
    burst.stop();
    let mut latest = request(root, "anchor", "anchor-restart", 4500, "normal", 1.2);
    latest.env.insert("TEST_MODEL_VERSION".into(), "2".into());
    submit(&client, "anchor", latest).await;
    local_phase(
        &anchor_config.state_dir,
        "anchor-restart",
        ExecutionPhase::Running,
        deadline,
    )
    .await;
    coordinator.stop();
    coordinator = OwnedService::start(
        &[
            "coordinator",
            "--deployment",
            coordinator_config.to_str().unwrap(),
        ],
        root,
        "coordinator-restarted",
    );
    ready(&client, deadline).await;
    let preserved = result(&client, "anchor-initial", deadline).await;
    assert_eq!(
        serde_json::to_value(&preserved).unwrap(),
        serde_json::to_value(&anchor_initial).unwrap()
    );
    let latest = result(&client, "anchor-restart", deadline).await;
    check_result(&latest, "anchor", 4500);
    let Response::Status { nodes, .. } = client
        .request(&Request::Status { job_id: None })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(
        nodes
            .iter()
            .any(|node| node["report"]["node_id"] == "burst" && node["drain"] == true)
    );
    burst = OwnedService::start(
        &[
            "--config",
            burst_file.to_str().unwrap(),
            "agent",
            "--deployment",
            burst_deployment.to_str().unwrap(),
        ],
        root,
        "burst-rejoined",
    );
    // Retried hold workers remain checkpointed until the explicit rejoin stage.
    fs::write(
        root.join("allow-explicit-resume"),
        b"resume after verified recovery and drain",
    )
    .unwrap();
    client
        .request(&Request::DrainNode {
            node_id: "burst".into(),
            drain: false,
        })
        .await
        .unwrap();
    let resumed = result(&client, "burst-resume", deadline).await;
    check_result(&resumed, "burst", 3000);
    assert!(resumed.generation > 1);
    assert_eq!(resumed.result["metadata"]["resumed"], true);
    let mut current = request(root, "burst", "burst-current-model", 5000, "normal", 0.05);
    current.input_artifacts = vec![NamedArtifact {
        name: "current-model.json".into(),
        sha256: latest.artifacts[0].sha256.clone(),
        size: latest.artifacts[0].size,
    }];
    current.env.insert("TEST_MODEL_VERSION".into(), "2".into());
    submit(&client, "burst", current).await;
    let current = result(&client, "burst-current-model", deadline).await;
    check_result(&current, "burst", 5000);
    assert_eq!(current.result["metadata"]["model_version"], "2");
    for name in ["anchor", "burst"] {
        client
            .request(&Request::DrainNode {
                node_id: name.into(),
                drain: true,
            })
            .await
            .unwrap();
        released(&client, name, deadline).await;
    }
    let mut expected_local_records = Vec::new();
    for cfg in [&anchor_config, &burst_config] {
        let records = StateStore::open_read_only_with_profile(&cfg.state_dir, cfg.storage_profile)
            .unwrap()
            .executions()
            .unwrap();
        assert!(records.iter().all(|r| r.phase == ExecutionPhase::Released));
        assert!(!records.is_empty());
        expected_local_records.push((
            &cfg.state_dir,
            cfg.storage_profile,
            serde_json::to_value(records).unwrap(),
        ));
    }
    let Response::Status {
        tasks: expected_tasks,
        allocations: expected_allocations,
        ..
    } = client
        .request(&Request::Status { job_id: None })
        .await
        .unwrap()
    else {
        panic!()
    };
    anchor.stop();
    burst.stop();
    coordinator.stop();
    // stop() kills and reaps each owned service. A rollback journal can be hot
    // even after every workload released, because the service still writes
    // heartbeats/clock state. Perform normal configured writable recovery only
    // now that all owned services are stopped; read-only inspection must not
    // recover a hot journal or reinterpret SQLITE_READONLY_ROLLBACK as success.
    for (state, profile) in [
        (&cc.state_dir, authority_profile),
        (&anchor_config.state_dir, authority_profile),
        (&burst_config.state_dir, storage_profile),
    ] {
        let recovered = StateStore::open_with_profile(state, profile).unwrap();
        let settings = recovered.durability_settings().unwrap();
        assert_eq!(settings.journal_mode, profile.journal_mode());
        assert_eq!(settings.synchronous, profile.synchronous());
        assert!(settings.foreign_keys);
        assert_eq!(settings.schema_version, profile.schema_version());
        drop(recovered);
        assert_persisted_profile(state, profile, deadline).await;
    }
    for (state, profile, expected) in expected_local_records {
        let recovered = StateStore::open_read_only_with_profile(state, profile).unwrap();
        assert_eq!(
            serde_json::to_value(recovered.executions().unwrap()).unwrap(),
            expected
        );
    }
    let mut recovered = Coordinator::open(cc.clone()).unwrap();
    for expected in [
        &anchor_initial,
        &burst_initial,
        &continuity,
        &latest,
        &resumed,
        &current,
    ] {
        let Response::Result {
            submission: Some(actual),
        } = recovered
            .handle(
                &Principal::Operator,
                Request::GetResult {
                    task_id: expected.task_id.clone(),
                },
            )
            .unwrap()
        else {
            panic!("acknowledged result missing after stopped-service recovery")
        };
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let Response::Status {
        tasks, allocations, ..
    } = recovered
        .handle(&Principal::Operator, Request::Status { job_id: None })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        serde_json::to_value(tasks).unwrap(),
        serde_json::to_value(expected_tasks).unwrap()
    );
    assert_eq!(allocations, expected_allocations);
    assert!(!allocations.is_empty());
    assert!(allocations.iter().all(|a| a["phase"] == "released"));
    drop(recovered);
    let payload = json!({"scope":"two_node_services_one_local_kernel_not_physical_hosts","in_place_logical_rollback_injected":in_place_rollback,"storage_profile":storage_profile,"persisted_profile_and_journal_verified_before_and_after_recovery":true,"post_stop_writable_recovery_preserved_exact_results_and_allocations":true,"elapsed_seconds":started.elapsed().as_secs_f64(),"experiment_id":"local-sdk-experiment","anchor_tasks":3,"burst_tasks":3,"checkpoint_resume_generation":resumed.generation,"coordinator_restart_preserved_receipt":true,"all_local_allocations_released":true});
    eprintln!("distributed_service_evidence={payload}");
    assert!(started.elapsed() < Duration::from_secs(60));
}
