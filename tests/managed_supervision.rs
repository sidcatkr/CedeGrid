#![cfg(unix)]
use cedegrid::{
    execution_model::*,
    managed_children::ManagedChildPhase,
    model::Resources,
    rootless::RootlessBackend,
    state::StateStore,
    supervision::{self, SupervisorOptions},
};
use std::{collections::BTreeMap, fs, path::Path};
fn request(dir: &Path, code: &str) -> LaunchRequest {
    let python =
        std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    LaunchRequest {
        task_id: "family-task".into(),
        assignment_id: "family-attempt".into(),
        argv: vec![python, "-c".into(), code.into()],
        cwd: dir.to_path_buf(),
        env: BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ]),
        resources: Resources {
            cpu_millicores: 1000,
            ram_mib: 128,
            gpu_memory_mib: BTreeMap::new(),
        },
        replay_safe: true,
        class: AllocationClass::Guaranteed,
        no_escape: true,
        single_process: false,
        managed_child_limit: 2,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    }
}
fn options() -> SupervisorOptions {
    SupervisorOptions {
        drain_timeout_ms: 50,
        term_grace_ms: 50,
        lease_ms: 5000,
        prepare_timeout_ms: 3000,
        release_confirm_timeout_ms: 1000,
        nice: 10,
    }
}

#[derive(Default)]
struct ChangingChildPolicy {
    checks: usize,
    reject_at: Option<usize>,
    incomplete_inventory: bool,
    delay_at: Option<usize>,
    delay_ms: u64,
    drain_after_second_check: bool,
}
impl supervision::SupervisorControl for ChangingChildPolicy {
    fn authorize_managed_child(
        &mut self,
        _request: &LaunchRequest,
    ) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        self.checks += 1;
        if self.delay_at == Some(self.checks) {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        // Real owned CPU gates exercise the native child protocol. Inject only
        // the GPU observation into the same production launch-contract check;
        // these tests do not claim an actual NVIDIA runtime or CUDA execution.
        let rejected = self.reject_at.is_some_and(|at| self.checks >= at);
        let inventory = if rejected && self.incomplete_inventory {
            serde_json::Value::Null
        } else if rejected {
            serde_json::json!([991])
        } else {
            serde_json::json!([])
        };
        let snapshot = serde_json::from_value(serde_json::json!({
            "schema_version":2,"node_id":"fixture","observed_at_unix_ms":1000,
            "cpu_capacity_millicores":2000,"physical_cores":2,"cpu_busy_millicores":0,
            "total_ram_mib":8192,"available_ram_mib":8192,"gpu_inventory":"available",
            "gpus":[{"uuid":"GPU-fixture","total_memory_mib":8192,"used_memory_mib":0,
                "utilization_percent":0,"external_process_ids":inventory,
                "external_compute":if rejected {"unknown"} else {"idle"},"compute_sample_id":null,
                "observation":{"capability":if rejected && self.incomplete_inventory {"insufficient_observability"} else {"conservative_non_sharing"},
                    "activity_api":"nvmlDeviceGetProcessUtilization","activity_api_status":"NotFound",
                    "query_cursor_us":0,"newest_sample_timestamp_us":null,"samples_returned":0,
                    "observed_at_unix_ms":1000,"baseline_age_ms":null,"sample_max_age_ms":2000,
                    "fresh_after_baseline":false,"freshness_decision":"fixture_without_new_samples"}}],
            "capabilities":{}
        }))?;
        let mut config = cedegrid::config::Config::default();
        config.gpu.execution_mode = cedegrid::config::GpuExecutionMode::ConservativeNonSharing;
        cedegrid::policy::validate_gpu_launch_contract(
            &config,
            &snapshot,
            &Resources {
                gpu_memory_mib: BTreeMap::from([("GPU-fixture".into(), 512)]),
                ..Resources::default()
            },
            AllocationClass::Opportunistic,
        )?;
        self.poll()
    }
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        Ok(if self.drain_after_second_check && self.checks >= 2 {
            vec![supervision::SupervisorCommand::Drain]
        } else {
            vec![]
        })
    }
}

fn run_with_child_policy(
    control: &mut dyn supervision::SupervisorControl,
    options: &SupervisorOptions,
    class: AllocationClass,
) -> (tempfile::TempDir, StateStore, supervision::ExecutionOutcome) {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let mut request = request(
        dir.path(),
        r#"
import sys,json
from pathlib import Path
from cedegrid import spawn_managed
try:
    child=spawn_managed([sys.executable,'-c',"from pathlib import Path;Path('child-user-executed').write_text('executed')"],single_process=True,no_escape=True)
except RuntimeError as error:
    Path('spawn-denied').write_text(str(error))
else:
    result=child.wait(timeout=6,poll_interval=.01)
    Path('child-status').write_text(json.dumps(result))
"#,
    );
    request.class = class;
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        options,
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        control,
    )
    .unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert!(store.execution_allocations().unwrap().is_empty());
    for child in store.managed_children("family-attempt").unwrap() {
        assert_eq!(child.phase, ManagedChildPhase::Released);
        if let Some(identity) = child.identity {
            assert_ne!(
                supervision::process_identity(
                    identity.pid,
                    &identity.assignment_id,
                    identity.generation
                )
                .ok(),
                Some(identity)
            );
        }
    }
    (dir, store, outcome)
}

#[test]
fn default_child_control_rejects_gpu_authority_and_preserves_cpu_launches() {
    struct DefaultControl;
    impl supervision::SupervisorControl for DefaultControl {}
    let mut control = DefaultControl;
    let mut request = request(&std::env::current_dir().unwrap(), "unused");
    supervision::SupervisorControl::authorize_managed_child(&mut control, &request).unwrap();
    request
        .resources
        .gpu_memory_mib
        .insert("GPU-unobserved".into(), 1);
    assert!(
        supervision::SupervisorControl::authorize_managed_child(&mut control, &request).is_err()
    );
    let (dir, _, outcome) =
        run_with_child_policy(&mut control, &options(), AllocationClass::Guaranteed);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(dir.path().join("child-user-executed").is_file());
}

#[test]
fn child_launch_rechecks_gpu_inventory_before_reservation_and_before_exec() {
    for (reject_at, incomplete) in [(1, false), (1, true), (2, false), (2, true)] {
        let mut control = ChangingChildPolicy {
            reject_at: Some(reject_at),
            incomplete_inventory: incomplete,
            ..Default::default()
        };
        let (dir, store, outcome) =
            run_with_child_policy(&mut control, &options(), AllocationClass::Opportunistic);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(control.checks, reject_at);
        assert!(!dir.path().join("child-user-executed").exists());
        assert_eq!(
            store.managed_children("family-attempt").unwrap().len(),
            usize::from(reject_at == 2)
        );
        if reject_at == 1 {
            assert!(dir.path().join("spawn-denied").is_file());
        }
    }
    let mut qualified = ChangingChildPolicy::default();
    let (dir, _, outcome) =
        run_with_child_policy(&mut qualified, &options(), AllocationClass::Opportunistic);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(qualified.checks >= 2);
    assert!(dir.path().join("child-user-executed").is_file());
}

#[test]
fn slow_child_check_cannot_outlive_parent_lease_or_child_preparation_deadline() {
    for parent_lease_expires in [true, false] {
        let mut control = ChangingChildPolicy {
            delay_at: Some(2),
            delay_ms: if parent_lease_expires { 3500 } else { 1800 },
            ..Default::default()
        };
        let mut limits = options();
        limits.lease_ms = 3000;
        limits.prepare_timeout_ms = if parent_lease_expires { 5000 } else { 1500 };
        let class = if parent_lease_expires {
            AllocationClass::Opportunistic
        } else {
            AllocationClass::Guaranteed
        };
        let (dir, store, outcome) = run_with_child_policy(&mut control, &limits, class);
        assert_eq!(control.checks, 2);
        assert!(!dir.path().join("child-user-executed").exists());
        assert_eq!(outcome.yielded, parent_lease_expires);
        let child = store.managed_children("family-attempt").unwrap().remove(0);
        assert!(child.detail.contains(if parent_lease_expires {
            "parent authorization expired"
        } else {
            "preparation deadline expired"
        }));
    }
}

#[test]
fn queued_parent_drain_during_child_check_prevents_exec() {
    let mut control = ChangingChildPolicy {
        drain_after_second_check: true,
        ..Default::default()
    };
    let (dir, store, outcome) =
        run_with_child_policy(&mut control, &options(), AllocationClass::Guaranteed);
    assert_eq!(control.checks, 2);
    assert!(outcome.yielded);
    assert!(!dir.path().join("child-user-executed").exists());
    assert_eq!(store.managed_children("family-attempt").unwrap().len(), 1);
}

#[test]
fn slow_control_poll_cannot_extend_a_child_launch_renewal() {
    #[derive(Default)]
    struct DelayedRenewal {
        checks: usize,
        sent: bool,
    }
    impl supervision::SupervisorControl for DelayedRenewal {
        fn authorize_managed_child(
            &mut self,
            _: &LaunchRequest,
        ) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
            self.checks += 1;
            self.poll()
        }
        fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
            if self.checks == 2 && !self.sent {
                self.sent = true;
                // PipeControl may read a grant before doing telemetry. Its
                // remaining duration must not restart when slow poll returns.
                std::thread::sleep(std::time::Duration::from_millis(500));
                return Ok(vec![supervision::SupervisorCommand::Renew {
                    assignment_id: "family-attempt".into(),
                    generation: 1,
                    sequence: 1,
                    remaining_ms: 200,
                }]);
            }
            Ok(vec![])
        }
    }
    let mut control = DelayedRenewal::default();
    let (dir, store, outcome) =
        run_with_child_policy(&mut control, &options(), AllocationClass::Opportunistic);
    assert!(control.sent && outcome.yielded);
    assert!(!dir.path().join("child-user-executed").exists());
    assert_eq!(store.managed_children("family-attempt").unwrap().len(), 1);
}

#[test]
fn guaranteed_child_final_gpu_check_is_not_superseded_by_parent_policy_poll() {
    struct ModeAControl {
        checks: usize,
        reject_final_check: bool,
        state_path: Option<std::path::PathBuf>,
        superseded: bool,
    }
    impl supervision::SupervisorControl for ModeAControl {
        fn authorize_managed_child(
            &mut self,
            request: &LaunchRequest,
        ) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
            self.checks += 1;
            self.state_path = Some(request.cwd.join("state"));
            // Inject qualified Mode A observations while using real owned CPU
            // gates. New GPU work must still pass the production contract even
            // when Guaranteed continuity permits its parent to keep running.
            let activity = if self.reject_final_check && self.checks == 2 {
                "unknown"
            } else {
                "idle"
            };
            let snapshot = serde_json::from_value(serde_json::json!({
                "schema_version":2,"node_id":"fixture","observed_at_unix_ms":1000,
                "cpu_capacity_millicores":2000,"physical_cores":2,"cpu_busy_millicores":0,
                "total_ram_mib":8192,"available_ram_mib":8192,"gpu_inventory":"available",
                "gpus":[{"uuid":"GPU-fixture","total_memory_mib":8192,"used_memory_mib":0,
                    "utilization_percent":0,"external_process_ids":[991],
                    "external_compute":activity,"compute_sample_id":101,
                    "observation":{"capability":"contention_aware",
                        "activity_api":"nvmlDeviceGetProcessUtilization","activity_api_status":"success",
                        "query_cursor_us":100,"newest_sample_timestamp_us":101,"samples_returned":1,
                        "observed_at_unix_ms":1000,"baseline_age_ms":500,"sample_max_age_ms":2000,
                        "fresh_after_baseline":true,"freshness_decision":"fixture_fresh"}}],
                "capabilities":{}
            }))?;
            let mut config = cedegrid::config::Config::default();
            config.gpu.execution_mode = cedegrid::config::GpuExecutionMode::ContentionAware;
            cedegrid::policy::validate_gpu_launch_contract(
                &config,
                &snapshot,
                &Resources {
                    gpu_memory_mib: BTreeMap::from([("GPU-fixture".into(), 512)]),
                    ..Resources::default()
                },
                AllocationClass::Guaranteed,
            )?;
            // Mirrors PipeControl's command-only receive after validation.
            Ok(vec![])
        }
        fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
            if self.checks == 2 {
                let store = StateStore::open(self.state_path.as_ref().unwrap())?;
                if store
                    .managed_children("family-attempt")?
                    .iter()
                    .any(|child| child.phase == ManagedChildPhase::Authorized)
                {
                    // A full policy poll could see a newer unknown/active GPU
                    // observation here but correctly keep a Guaranteed parent
                    // alive. It must not supersede the child's final check.
                    self.superseded = true;
                }
            }
            Ok(vec![])
        }
    }
    for reject_final_check in [false, true] {
        let mut control = ModeAControl {
            checks: 0,
            reject_final_check,
            state_path: None,
            superseded: false,
        };
        let (dir, _, outcome) =
            run_with_child_policy(&mut control, &options(), AllocationClass::Guaranteed);
        assert_eq!(control.checks, 2);
        assert!(
            !control.superseded,
            "parent policy polled after final child GPU check but before EXEC"
        );
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.yielded, "Guaranteed parent continuation changed");
        assert_eq!(
            dir.path().join("child-user-executed").is_file(),
            !reject_final_check
        );
    }
}

#[test]
fn sdk_mediated_child_executes_once_and_parent_waits_for_verified_release() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let code = r#"
import sys,json
from pathlib import Path
from cedegrid import spawn_managed
argv=[sys.executable,'-c',"from pathlib import Path; Path('child-result').write_text('useful output')"]
a=spawn_managed(argv,single_process=True,no_escape=True,request_id='same-request')
b=spawn_managed(argv,single_process=True,no_escape=True,request_id='same-request')
assert a.child_id==b.child_id
result=a.wait(timeout=4,poll_interval=.01)
assert result['exit_code']==0 and result['state']=='released',result
Path('family-result').write_text(json.dumps(result))
"#;
    let request = request(dir.path(), code);
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert_eq!(
        fs::read_to_string(dir.path().join("child-result")).unwrap(),
        "useful output"
    );
    let children = store.managed_children("family-attempt").unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].phase, ManagedChildPhase::Released);
    assert!(children[0].identity.is_some());
    assert!(store.execution_allocations().unwrap().is_empty());
}
#[test]
fn leader_exit_drains_and_reaps_each_registered_child_before_parent_release() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import sys,time
from pathlib import Path
from cedegrid import spawn_managed
child=spawn_managed([sys.executable,'-c',"import time;from pathlib import Path;Path('child-ready').write_text('ready');time.sleep(30)"],single_process=True,no_escape=True)
while not Path('child-ready').exists(): time.sleep(.01)
"#,
    );
    let start = std::time::Instant::now();
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(outcome.exit_code, Some(0));
    let children = store.managed_children("family-attempt").unwrap();
    assert_eq!(children[0].phase, ManagedChildPhase::Released);
    assert!(children[0].signal.is_some());
    let identity = children[0].identity.as_ref().unwrap();
    assert_ne!(
        supervision::process_identity(identity.pid, &identity.assignment_id, identity.generation)
            .ok()
            .as_ref(),
        Some(identity)
    );
    assert!(store.execution_allocations().unwrap().is_empty());
}
#[test]
fn managed_child_preparation_failure_never_executes_child_code() {
    struct RejectChild {
        count: usize,
    }
    impl LaunchBackend for RejectChild {
        fn name(&self) -> &str {
            "rootless"
        }
        fn prepare(&mut self, _: &LaunchRequest) -> anyhow::Result<GateSetup> {
            Ok(GateSetup::default())
        }
        fn verify(&mut self, _: &ProcessIdentity) -> anyhow::Result<Vec<ControlEvidence>> {
            self.count += 1;
            anyhow::ensure!(self.count == 1, "injected child backend membership refusal");
            Ok(vec![])
        }
        fn confirm_release(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import sys
from cedegrid import spawn_managed
child=spawn_managed([sys.executable,'-c',"from pathlib import Path;Path('forbidden').write_text('executed')"],single_process=True,no_escape=True)
result=child.wait(timeout=4,poll_interval=.01)
assert result['exit_code'] is None
"#,
    );
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RejectChild { count: 0 },
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!dir.path().join("forbidden").exists());
    assert!(
        store
            .managed_children("family-attempt")
            .unwrap()
            .iter()
            .all(|c| c.phase == ManagedChildPhase::Released)
    );
}

#[test]
fn workload_stdout_and_stderr_are_retained_with_bounded_size() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let mut request = request(
        dir.path(),
        "import os;os.write(1,b'x'*(5*1024*1024));os.write(2,b'y'*(5*1024*1024))",
    );
    request.single_process = true;
    request.managed_child_limit = 0;
    request.env.insert(
        "CEDEGRID_OUTPUT_DIR".into(),
        dir.path().display().to_string(),
    );
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(
        fs::metadata(dir.path().join("stdout.log")).unwrap().len(),
        4 * 1024 * 1024
    );
    assert_eq!(
        fs::metadata(dir.path().join("stderr.log")).unwrap().len(),
        4 * 1024 * 1024
    );
}

struct InjectJournal {
    store: StateStore,
    stage: u8,
    pause: Option<std::path::PathBuf>,
}
impl ExecutionJournal for InjectJournal {
    fn reserve(&self, r: &LaunchRequest, c: &Resources) -> anyhow::Result<ExecutionRecord> {
        self.store.reserve(r, c)
    }
    fn transition(&self, r: &ExecutionRecord) -> anyhow::Result<()> {
        self.store.transition(r)
    }
    fn reserve_managed_child(
        &self,
        a: &str,
        g: u64,
        c: &str,
        r: &str,
        q: &LaunchRequest,
    ) -> anyhow::Result<cedegrid::managed_children::ManagedChildRecord> {
        anyhow::ensure!(self.stage != 0, "injected child reservation failure");
        self.store.reserve_managed_child(a, g, c, r, q)
    }
    fn managed_child(
        &self,
        a: &str,
        r: &str,
    ) -> anyhow::Result<Option<cedegrid::managed_children::ManagedChildRecord>> {
        self.store.managed_child(a, r)
    }
    fn managed_children(
        &self,
        a: &str,
    ) -> anyhow::Result<Vec<cedegrid::managed_children::ManagedChildRecord>> {
        self.store.managed_children(a)
    }
    fn prepare_managed_child(
        &self,
        c: &str,
        i: &ProcessIdentity,
        e: &[ControlEvidence],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.stage != 1,
            "injected child identity/control persistence failure"
        );
        self.store.prepare_managed_child(c, i, e)?;
        if let Some(path) = &self.pause {
            fs::write(
                path,
                serde_json::to_vec(
                    &serde_json::json!({"leader":self.store.executions()?[0].identity,"child":i}),
                )?,
            )?;
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        Ok(())
    }
    fn transition_managed_child(
        &self,
        c: &str,
        p: ManagedChildPhase,
        e: Option<i32>,
        s: Option<i32>,
        d: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !(self.stage == 2 && p == ManagedChildPhase::Authorized),
            "injected child authorization persistence failure"
        );
        self.store.transition_managed_child(c, p, e, s, d)
    }
}
#[test]
fn every_mediated_child_persistence_barrier_failure_prevents_execution() {
    for stage in 0..3 {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let journal = InjectJournal {
            store: StateStore::open(&dir.path().join("state")).unwrap(),
            stage,
            pause: None,
        };
        let request = request(
            dir.path(),
            r#"
import sys
from cedegrid import spawn_managed
try:
 child=spawn_managed([sys.executable,'-c',"from pathlib import Path;Path('forbidden').write_text('executed')"],single_process=True,no_escape=True)
 child.wait(timeout=4,poll_interval=.01)
except RuntimeError: pass
"#,
        );
        let outcome = supervision::supervise(
            &request,
            &request.resources,
            &options(),
            &journal,
            &mut RootlessBackend::new(10),
            Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        )
        .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!dir.path().join("forbidden").exists());
        assert!(journal.store.execution_allocations().unwrap().is_empty());
    }
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "launched only by the bounded supervisor-loss harness"]
fn mediated_child_supervisor_loss_fixture() {
    let dir = std::path::PathBuf::from(std::env::var("CEDEGRID_FAMILY_FIXTURE_DIR").unwrap());
    let journal = InjectJournal {
        store: StateStore::open(&dir.join("state")).unwrap(),
        stage: 3,
        pause: Some(dir.join("ready.json")),
    };
    let request = request(
        &dir,
        r#"
import sys
from cedegrid import spawn_managed
try:
 child=spawn_managed([sys.executable,'-c',"from pathlib import Path;Path('forbidden').write_text('executed')"],single_process=True,no_escape=True)
 child.wait(timeout=3,poll_interval=.01)
except (RuntimeError,OSError): pass
"#,
    );
    let _ = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &journal,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    );
}
#[cfg(target_os = "linux")]
#[test]
fn supervisor_loss_before_child_authorization_retains_then_reconciles_family() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    struct Tracked(OwnedFd);
    impl Drop for Tracked {
        fn drop(&mut self) {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                );
            }
        }
    }
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let mut supervisor = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "mediated_child_supervisor_loss_fixture",
        ])
        .env("CEDEGRID_FAMILY_FIXTURE_DIR", dir.path())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !dir.path().join("ready.json").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !dir.path().join("ready.json").exists() {
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        panic!("child never reached persisted blocked preparation");
    }
    let identities: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("ready.json")).unwrap()).unwrap();
    let mut tracked = vec![];
    for key in ["leader", "child"] {
        let id: ProcessIdentity = serde_json::from_value(identities[key].clone()).unwrap();
        assert_eq!(
            supervision::process_identity(id.pid, &id.assignment_id, id.generation).unwrap(),
            id
        );
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, id.pid, 0u32) };
        assert!(raw >= 0, "native family crash validation requires pidfds");
        let handle = Tracked(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        assert_eq!(
            supervision::process_identity(id.pid, &id.assignment_id, id.generation).unwrap(),
            id
        );
        tracked.push(handle);
    }
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    assert_eq!(store.execution_allocations().unwrap().len(), 1);
    assert_eq!(
        store.managed_children("family-attempt").unwrap()[0].phase,
        ManagedChildPhase::Prepared
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        exited = tracked.iter().all(|h| {
            let mut p = libc::pollfd {
                fd: h.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            (unsafe { libc::poll(&mut p, 1, 0) }) > 0
        });
        if exited {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        exited,
        "owned fixture cleanup handles retained; no claim of supervisor self-enforcement"
    );
    assert!(!dir.path().join("forbidden").exists());
    let config = cedegrid::config::Config {
        state_dir: dir.path().join("state"),
        ..Default::default()
    };
    let records = cedegrid::agent::reconcile(&config).unwrap();
    assert_eq!(records[0].phase, ExecutionPhase::Released);
}
