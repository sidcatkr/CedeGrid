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
        contention: None,
    }
}
fn options() -> SupervisorOptions {
    SupervisorOptions {
        drain_timeout_ms: 50,
        term_grace_ms: 50,
        lease_ms: 5000,
        prepare_timeout_ms: 3000,
        release_confirm_timeout_ms: 1000,
        duty_period_ms: 100,
        hint_deadline_ms: 5_000,
        prefer_own_oom_victim: false,
        ram_noise_tolerance_mib: 256,
        ram_calm_window_ms: 1_500,
        nofile_soft: None,
        nice: 10,
    }
}

/// Records every report a supervisor sends.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Reports(Vec<supervision::SupervisorReport>);
#[cfg(target_os = "linux")]
impl supervision::SupervisorControl for Reports {
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        self.0.push(report);
        Ok(())
    }
}

/// The largest family a run reported, its descendants' PIDs, and the outside reports.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Seen {
    largest: u64,
    pids: std::collections::BTreeSet<u32>,
    outside: Vec<(u64, Option<supervision::OutsideReason>)>,
}
#[cfg(target_os = "linux")]
fn family_seen(reports: &Reports) -> Seen {
    let mut seen = Seen::default();
    for report in &reports.0 {
        match report {
            supervision::SupervisorReport::Family { family } => {
                assert_eq!(family.leader, 1, "{family:?}");
                seen.largest = seen.largest.max(family.descendants.len() as u64);
                seen.pids
                    .extend(family.descendants.iter().map(|member| member.pid));
            }
            supervision::SupervisorReport::OutsideFamily { count, reason } => {
                seen.outside.push((*count, *reason));
            }
            _ => {}
        }
    }
    seen
}

#[cfg(target_os = "linux")]
fn supervise_family(
    request: &LaunchRequest,
    dir: &Path,
) -> (supervision::ExecutionOutcome, Reports) {
    let store = StateStore::open(&dir.join("state")).unwrap();
    let mut reports = Reports::default();
    let outcome = supervision::supervise_controlled(
        request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut reports,
    )
    .unwrap();
    (outcome, reports)
}

/// A request for a leader that forks its own workers; until the rootless launch accepts
/// multi-process work on Linux it says single-process, as the fork test of 030 did.
#[cfg(target_os = "linux")]
fn forking(dir: &Path, code: &str) -> LaunchRequest {
    let mut request = request(dir, code);
    request.single_process = true;
    request.managed_child_limit = 0;
    request
}

/// A leader that forks 4 busy workers, plainly or through `multiprocessing`, reports a family
/// of the leader and 4 descendants, all verified, and nothing outside it (FR-083).
#[cfg(target_os = "linux")]
#[test]
fn forked_workers_join_the_verified_family() {
    for code in [
        r#"
import os, sys, time
pids = []
for _ in range(4):
    pid = os.fork()
    if pid == 0:
        end = time.time() + 1.2
        while time.time() < end:
            pass
        os._exit(0)
    pids.append(pid)
sys.exit(sum(os.WEXITSTATUS(os.waitpid(pid, 0)[1]) for pid in pids))
"#,
        r#"
import multiprocessing, sys, time
def work():
    end = time.time() + 1.2
    while time.time() < end:
        pass
if __name__ == '__main__':
    ctx = multiprocessing.get_context('fork')
    workers = [ctx.Process(target=work) for _ in range(4)]
    for worker in workers:
        worker.start()
    for worker in workers:
        worker.join()
    sys.exit(sum(worker.exitcode for worker in workers))
"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (outcome, reports) = supervise_family(&forking(dir.path(), code), dir.path());
        assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
        let seen = family_seen(&reports);
        assert_eq!(seen.largest, 4, "{:?}", reports.0);
        assert!(seen.outside.is_empty(), "{:?}", seen.outside);
        assert_eq!(outcome.single_process_violated, 0);
    }
}

/// A helper whose parent exits first is reparented to the supervisor, verified with the
/// supervisor as its chain's root, kept in the family, and reaped by its own PID: no zombie
/// stays, and the leader's exit status is the leader's (FR-083, R34, R36).
#[cfg(target_os = "linux")]
#[test]
fn an_orphan_joins_and_is_reaped_by_its_pid() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
import os, sys, time
from pathlib import Path
child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        time.sleep(1.0)
        os._exit(0)
    Path('grandchild').write_text(str(grandchild))
    os._exit(0)
os.waitpid(child, 0)
time.sleep(2.0)
sys.exit(3)
"#;
    let (outcome, reports) = supervise_family(&forking(dir.path(), code), dir.path());
    assert_eq!(outcome.exit_code, Some(3), "the leader's own status");
    let grandchild: u32 = fs::read_to_string(dir.path().join("grandchild"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let seen = family_seen(&reports);
    assert!(seen.pids.contains(&grandchild), "{:?}", reports.0);
    assert!(seen.outside.is_empty(), "{:?}", seen.outside);
    let mut status = 0;
    // SAFETY: a wait for the one PID this test's run left, which must already be reaped.
    let rc = unsafe { libc::waitpid(grandchild as i32, &mut status, libc::WNOHANG) };
    assert_eq!(rc, -1, "a zombie was left under the supervisor");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

/// A run tracks at most 256 members, the leader included; the rest are reported over the
/// bound and left alone, and no descriptor runs out (FR-083, R39).
#[cfg(target_os = "linux")]
#[test]
fn descendants_past_the_bound_are_reported_not_tracked() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
import os, sys, time
pids = []
for _ in range(300):
    pid = os.fork()
    if pid == 0:
        time.sleep(2.5)
        os._exit(0)
    pids.append(pid)
sys.exit(max(os.WEXITSTATUS(os.waitpid(pid, 0)[1]) for pid in pids))
"#;
    let (outcome, reports) = supervise_family(&forking(dir.path(), code), dir.path());
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    let seen = family_seen(&reports);
    assert_eq!(seen.largest, 255, "{:?}", seen.outside);
    assert_eq!(
        seen.outside.last(),
        Some(&(45, Some(supervision::OutsideReason::OverBound)))
    );
    assert_eq!(outcome.single_process_violated, 45);
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
        // The FR-031 reading of the legacy conservative non-sharing mode.
        let mut config = cedegrid::config::Config::default();
        config.contention.never_share_occupied_gpus = true;
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
    let dir = tempfile::tempdir().unwrap();
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
            // Yield First admits only on a quiet device, so unknown activity refuses
            // the final check; Neutral would admit it as one principal.
            let mut config = cedegrid::config::Config::default();
            config.contention.gpu_compute.mode =
                Some(cedegrid::contention::ContentionMode::YieldFirst);
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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

thread_local! {
    /// Stage 4: how many child release transitions still fail.
    static RELEASE_FAILURES: std::cell::Cell<u8> = const { std::cell::Cell::new(2) };
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
        if self.stage == 4 && p == ManagedChildPhase::Released {
            let left = RELEASE_FAILURES.with(|left| left.get());
            if left > 0 {
                RELEASE_FAILURES.with(|failures| failures.set(left - 1));
                anyhow::bail!("injected transient child release persistence failure");
            }
        }
        self.store.transition_managed_child(c, p, e, s, d)
    }
}
#[test]
fn every_mediated_child_persistence_barrier_failure_prevents_execution() {
    for stage in 0..3 {
        let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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

/// The `ps` run state of a process, such as `S`, `R` or `T` (stopped).
fn run_state(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Pauses CPU, then reverses it, and samples each process's run state after
/// each adjustment is reported.
struct ActuationAudit {
    dir: std::path::PathBuf,
    foreign: u32,
    stage: u8,
    reports: Vec<supervision::SupervisorReport>,
    /// Counts of processes outside the verified family, as reported (FR-082).
    outside: Vec<u64>,
    paused: BTreeMap<&'static str, String>,
    resumed: BTreeMap<&'static str, String>,
}
impl ActuationAudit {
    fn pids(&self) -> Option<BTreeMap<&'static str, u32>> {
        let read = |name: &str| -> Option<u32> {
            fs::read_to_string(self.dir.join(name))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        Some(BTreeMap::from([
            ("leader", read("leader.pid")?),
            ("child", read("child.pid")?),
            ("loose", read("loose.pid")?),
            ("foreign", self.foreign),
        ]))
    }
    fn sample(&self) -> BTreeMap<&'static str, String> {
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.pids()
            .unwrap()
            .into_iter()
            .map(|(name, pid)| (name, run_state(pid)))
            .collect()
    }
}
fn cpu_step(
    sequence: u64,
    step: cedegrid::contention::YieldStep,
) -> supervision::SupervisorCommand {
    use cedegrid::contention::{ResourceKey, StepRequest, YieldCause};
    supervision::SupervisorCommand::Adjust {
        sequence,
        resources: BTreeMap::from([(
            ResourceKey::Cpu,
            StepRequest {
                step,
                target: None,
                cause: YieldCause::ExternalGrowth,
                deadline_ms: None,
            },
        )]),
    }
}
impl supervision::SupervisorControl for ActuationAudit {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        use cedegrid::contention::YieldStep;
        Ok(match self.stage {
            0 if self.pids().is_some() => {
                self.stage = 1;
                vec![cpu_step(1, YieldStep::Pause)]
            }
            1 if self.reports.len() == 1 => {
                self.paused = self.sample();
                self.stage = 2;
                vec![cpu_step(2, YieldStep::None)]
            }
            2 if self.reports.len() == 2 => {
                self.resumed = self.sample();
                self.stage = 3;
                fs::write(self.dir.join("stop"), "").unwrap();
                vec![]
            }
            _ => vec![],
        })
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        match report {
            supervision::SupervisorReport::Readiness { .. } => {}
            supervision::SupervisorReport::OutsideFamily { count, .. } => self.outside.push(count),
            supervision::SupervisorReport::Family { .. } => {}
            report => self.reports.push(report),
        }
        Ok(())
    }
}

/// A step reaches the leader, its SDK child and, on Linux, the fork the leader made outside
/// the SDK, which joins the verified family; a process the test started before the run is
/// outside the family and no step reaches it (FR-083, FR-086, Constitution I).
#[test]
fn runtime_steps_reach_only_the_verified_family() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    // A test-owned process outside the family is never verified, so no step may reach it.
    let mut foreign = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut request = request(
        dir.path(),
        r#"
import os,sys,time,subprocess
from pathlib import Path
from cedegrid import spawn_managed
d=Path.cwd()
child=spawn_managed([sys.executable,'-c',"import os,time,pathlib;pathlib.Path(%r,'child.pid').write_text(str(os.getpid()));time.sleep(30)" % str(d)],single_process=True,no_escape=True)
loose=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
(d/'loose.pid').write_text(str(loose.pid))
(d/'leader.pid').write_text(str(os.getpid()))
while not (d/'stop').exists():
    time.sleep(0.02)
loose.kill();loose.wait()
child.stop();child.wait(timeout=6,poll_interval=.01)
"#,
    );
    request.class = AllocationClass::Opportunistic;
    let mut audit = ActuationAudit {
        dir: dir.path().to_owned(),
        foreign: foreign.id(),
        stage: 0,
        reports: vec![],
        outside: vec![],
        paused: BTreeMap::new(),
        resumed: BTreeMap::new(),
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut audit,
    )
    .unwrap();
    let foreign_after = run_state(foreign.id());
    let foreign_alive = foreign.try_wait().unwrap().is_none();
    foreign.kill().unwrap();
    foreign.wait().unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert_eq!(audit.stage, 3, "{:?}", audit.reports);
    let applied: Vec<_> = audit
        .reports
        .iter()
        .map(|report| match report {
            supervision::SupervisorReport::Adjusted { applied, .. } => {
                applied[&cedegrid::contention::ResourceKey::Cpu]
            }
            other => panic!("unexpected report {other:?}"),
        })
        .collect();
    use cedegrid::contention::YieldStep;
    assert_eq!(applied, [YieldStep::Pause, YieldStep::None]);
    // Only Linux tracks the leader's own forks.
    let (members, outside): (&[&str], &[&str]) = if cfg!(target_os = "linux") {
        (&["leader", "child", "loose"], &["foreign"])
    } else {
        (&["leader", "child"], &["loose", "foreign"])
    };
    for name in members {
        assert!(audit.paused[*name].contains('T'), "{:?}", audit.paused);
        assert!(!audit.resumed[*name].contains('T'), "{:?}", audit.resumed);
    }
    for name in outside {
        assert!(!audit.paused[*name].contains('T'), "{:?}", audit.paused);
        assert!(!audit.resumed[*name].contains('T'), "{:?}", audit.resumed);
    }
    assert!(
        foreign_alive && !foreign_after.contains('T'),
        "{foreign_after}"
    );
    // The foreign process started before EXEC, so it is not counted as the run's either.
    assert!(audit.outside.is_empty(), "{:?}", audit.outside);
}

/// The workload and each SDK child lead a session of their own, so no exit of a supervisor
/// can orphan their process group into SIGHUP (FR-078, R41).
#[test]
fn the_workload_and_its_sdk_children_lead_their_own_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import os, sys
from pathlib import Path
from cedegrid import spawn_managed
d = Path.cwd()
Path('leader-session').write_text(str(os.getsid(0) == os.getpid() == os.getpgrp()))
child = spawn_managed([sys.executable, '-c', "import os, pathlib; pathlib.Path(%r, 'child-session').write_text(str(os.getsid(0) == os.getpid() == os.getpgrp()))" % str(d)], single_process=True, no_escape=True)
child.wait(timeout=6, poll_interval=.01)
"#,
    );
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut ChangingChildPolicy::default(),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    for file in ["leader-session", "child-session"] {
        assert_eq!(
            fs::read_to_string(dir.path().join(file)).unwrap(),
            "True",
            "{file}"
        );
    }
}

/// A managed child carries the same OOM preference as its parent (FR-023).
#[cfg(target_os = "linux")]
#[test]
fn managed_children_carry_the_oom_preference() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import sys
from pathlib import Path
from cedegrid import spawn_managed
d=Path.cwd()
Path('parent-adj').write_text(Path('/proc/self/oom_score_adj').read_text())
child=spawn_managed([sys.executable,'-c',"import pathlib;pathlib.Path(%r,'child-adj').write_text(pathlib.Path('/proc/self/oom_score_adj').read_text())" % str(d)],single_process=True,no_escape=True)
child.wait(timeout=6,poll_interval=.01)
"#,
    );
    // Under the opt-in every own process, managed children too, is preferred (FR-023).
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &SupervisorOptions {
            prefer_own_oom_victim: true,
            ..options()
        },
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut ChangingChildPolicy::default(),
    )
    .unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    for file in ["parent-adj", "child-adj"] {
        assert_eq!(
            fs::read_to_string(dir.path().join(file)).unwrap().trim(),
            "1000",
            "{file}"
        );
    }
}

/// Sends scripted adjustments, each once the previous report arrived and its
/// gate file exists, and stops the job after the last report.
struct TargetScript {
    dir: std::path::PathBuf,
    key: cedegrid::contention::ResourceKey,
    /// The hint deadline each step carries, as the policy enforces it.
    deadline_ms: Option<u64>,
    steps: Vec<(&'static str, cedegrid::contention::YieldStep, Option<u64>)>,
    sent: usize,
    /// One `adjusted` report per command; readiness changes are kept apart.
    reports: Vec<supervision::SupervisorReport>,
    readiness: Vec<(bool, bool)>,
    /// A marker the worker writes once it has seen the last step; `stop` waits for it, at most
    /// 5 s, so the worker's poll cannot miss that step between two supervisor passes.
    stop_gate: Option<&'static str>,
    gate_since: Option<std::time::Instant>,
}
impl supervision::SupervisorControl for TargetScript {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        use cedegrid::contention::{StepRequest, YieldCause};
        if self.sent == self.reports.len() && self.sent == self.steps.len() {
            if let Some(gate) = self.stop_gate {
                let since = *self.gate_since.get_or_insert_with(std::time::Instant::now);
                if !self.dir.join(gate).exists()
                    && since.elapsed() < std::time::Duration::from_secs(5)
                {
                    return Ok(vec![]);
                }
            }
            fs::write(self.dir.join("stop"), "").unwrap();
            return Ok(vec![]);
        }
        let Some((gate, step, target)) = self.steps.get(self.sent).copied() else {
            return Ok(vec![]);
        };
        if self.sent != self.reports.len() || !self.dir.join(gate).exists() {
            return Ok(vec![]);
        }
        self.sent += 1;
        Ok(vec![supervision::SupervisorCommand::Adjust {
            sequence: self.sent as u64,
            resources: BTreeMap::from([(
                self.key.clone(),
                StepRequest {
                    step,
                    target,
                    cause: YieldCause::FloorBreach,
                    deadline_ms: self.deadline_ms,
                },
            )]),
        }])
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        match report {
            supervision::SupervisorReport::Readiness {
                hint_ready,
                observes_drain,
            } => self.readiness.push((hint_ready, observes_drain)),
            report => self.reports.push(report),
        }
        Ok(())
    }
}

fn run_targets(
    code: &str,
    steps: Vec<(&'static str, cedegrid::contention::YieldStep, Option<u64>)>,
) -> (tempfile::TempDir, TargetScript) {
    run_targets_on(cedegrid::contention::ResourceKey::Ram, code, steps)
}

fn run_targets_on(
    key: cedegrid::contention::ResourceKey,
    code: &str,
    steps: Vec<(&'static str, cedegrid::contention::YieldStep, Option<u64>)>,
) -> (tempfile::TempDir, TargetScript) {
    run_targets_with(key, None, None, code, steps)
}

fn run_targets_with(
    key: cedegrid::contention::ResourceKey,
    deadline_ms: Option<u64>,
    stop_gate: Option<&'static str>,
    code: &str,
    steps: Vec<(&'static str, cedegrid::contention::YieldStep, Option<u64>)>,
) -> (tempfile::TempDir, TargetScript) {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(dir.path(), code);
    let mut script = TargetScript {
        dir: dir.path().to_owned(),
        key,
        deadline_ms,
        steps,
        sent: 0,
        reports: vec![],
        readiness: vec![],
        stop_gate,
        gate_since: None,
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut script,
    )
    .unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert_eq!(outcome.exit_code, Some(0), "{:?}", script.reports);
    (dir, script)
}

fn applied(
    report: &supervision::SupervisorReport,
) -> (Option<cedegrid::contention::YieldStep>, bool) {
    use cedegrid::contention::{ResourceKey, YieldStep};
    match report {
        supervision::SupervisorReport::Adjusted {
            applied,
            unavailable,
            ..
        } => (
            applied.get(&ResourceKey::Ram).copied(),
            unavailable
                .get(&ResourceKey::Ram)
                .is_some_and(|steps| steps.contains(&YieldStep::Hint)),
        ),
        other => panic!("unexpected report {other:?}"),
    }
}

#[test]
fn hint_targets_are_written_atomically_with_increasing_sequences() {
    use cedegrid::contention::{ResourceKey, YieldStep};
    // Each step waits until the reader has seen the one before, and the stop until it has
    // seen the last, so a slow reader cannot miss a target between two polls.
    let (dir, script) = run_targets_with(
        ResourceKey::Ram,
        None,
        Some("seen3"),
        r#"
import json,time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env();seen=[]
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    t=signals.resource_targets()
    seen.append(None if t is None else [t.sequence,t.step,t.ram_mib])
    if t is not None: (d/f'seen{t.sequence}').write_text('1')
    time.sleep(0.005)
(d/'seen').write_text(json.dumps(seen))
"#,
        vec![
            ("ready", YieldStep::Hint, Some(100)),
            ("seen1", YieldStep::Hint, Some(50)),
            ("seen2", YieldStep::None, None),
        ],
    );
    let applied: Vec<_> = script.reports.iter().map(applied).collect();
    assert_eq!(
        applied,
        [
            (Some(YieldStep::Hint), false),
            (Some(YieldStep::Hint), false),
            (Some(YieldStep::None), false)
        ]
    );
    // Each report carries the target the supervisor applied.
    let targets: Vec<Option<u64>> = script
        .reports
        .iter()
        .map(|report| match report {
            supervision::SupervisorReport::Adjusted {
                applied_targets, ..
            } => applied_targets
                .get(&cedegrid::contention::ResourceKey::Ram)
                .copied(),
            other => panic!("unexpected report {other:?}"),
        })
        .collect();
    assert_eq!(targets, [Some(100), Some(50), None]);
    let seen: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(dir.path().join("seen")).unwrap()).unwrap();
    let first = seen
        .iter()
        .position(|s| !s.is_null())
        .expect("no target seen");
    // A reader never sees a missing or partial file once a target exists.
    assert!(seen[first..].iter().all(|s| !s.is_null()), "{seen:?}");
    let mut distinct = seen[first..].to_vec();
    distinct.dedup();
    assert_eq!(
        distinct,
        [
            serde_json::json!([1, "hint", 100]),
            serde_json::json!([2, "hint", 50]),
            serde_json::json!([3, "none", null])
        ]
    );
}

/// A managed child started after a hint finds the current target at once (FR-035).
#[test]
fn a_managed_child_started_after_a_hint_finds_the_current_target() {
    use cedegrid::contention::YieldStep;
    let (dir, _script) = run_targets(
        r#"
import sys,time
from pathlib import Path
from cedegrid import ResourceSignals, spawn_managed
d=Path.cwd();signals=ResourceSignals.from_env()
signals.draining()
(d/'ready').write_text('1')
while signals.resource_targets() is None:
    time.sleep(0.005)
code="import pathlib;from cedegrid import ResourceSignals;t=ResourceSignals.from_env().resource_targets();pathlib.Path(%r,'child-seen').write_text(str(None if t is None else t.ram_mib))" % str(d)
child=spawn_managed([sys.executable,'-c',code],single_process=True,no_escape=True)
child.wait(timeout=10,poll_interval=.01)
(d/'spawned').write_text('1')
while not (d/'stop').exists():
    time.sleep(0.01)
"#,
        vec![
            ("ready", YieldStep::Hint, Some(100)),
            ("spawned", YieldStep::Hint, Some(100)),
        ],
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("child-seen")).unwrap(),
        "100"
    );
}

/// The target file carries the deadline the policy enforces, here the 1 s of the
/// critical floor path, not the supervisor's default (FR-037, FR-063).
#[test]
fn a_hint_carries_the_deadline_the_policy_enforces() {
    use cedegrid::contention::{ResourceKey, YieldStep};
    let (dir, _script) = run_targets_with(
        ResourceKey::Ram,
        Some(1000),
        Some("seen"),
        r#"
import time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env();left=None
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    t=signals.resource_targets()
    if t is not None and left is None:
        left=t.deadline_unix_ms-int(time.time()*1000);(d/'seen').write_text('1')
    time.sleep(0.005)
(d/'left').write_text(str(left))
"#,
        vec![("ready", YieldStep::Hint, Some(100))],
    );
    let left: i64 = fs::read_to_string(dir.path().join("left"))
        .unwrap()
        .parse()
        .unwrap();
    assert!((0..=1000).contains(&left), "{left} ms left");
}

/// The request helper is Guaranteed with 1000m of CPU: a CPU target never goes
/// below the request, whatever the agent asks (FR-024).
#[test]
fn a_guaranteed_cpu_target_never_goes_below_the_request() {
    use cedegrid::contention::{ResourceKey, YieldStep};
    // The stop waits until the reader has seen the target.
    let (dir, _script) = run_targets_with(
        ResourceKey::Cpu,
        None,
        Some("seen1"),
        r#"
import time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env();seen=None
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    t=signals.resource_targets()
    if t is not None: seen=t.cpu_millicores;(d/f'seen{t.sequence}').write_text('1')
    time.sleep(0.005)
(d/'seen').write_text(str(seen))
"#,
        vec![("ready", YieldStep::Hint, Some(0))],
    );
    assert_eq!(fs::read_to_string(dir.path().join("seen")).unwrap(), "1000");
}

/// The supervisor reports readiness when it changes and in every `adjusted`
/// report, so the agent learns a late ack by the next interval (FR-062).
#[test]
fn readiness_follows_the_ack_and_a_plain_command_is_never_ready() {
    use cedegrid::contention::YieldStep;
    let ready = |report: &supervision::SupervisorReport| match report {
        supervision::SupervisorReport::Adjusted {
            hint_ready,
            observes_drain,
            ..
        } => (*hint_ready, *observes_drain),
        other => panic!("unexpected report {other:?}"),
    };
    let (_dir, late) = run_targets(
        r#"
import time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env()
(d/'ready').write_text('1')
time.sleep(0.5)
signals.draining()
time.sleep(0.3)
(d/'acked').write_text('1')
while not (d/'stop').exists():
    time.sleep(0.01)
"#,
        vec![
            ("ready", YieldStep::None, None),
            ("acked", YieldStep::None, None),
        ],
    );
    assert_eq!(
        late.reports.iter().map(ready).collect::<Vec<_>>(),
        [(false, false), (true, true)]
    );
    assert_eq!(late.readiness, [(true, true)]);
    let (_dir, plain) = run_targets(
        r#"
import time
from pathlib import Path
d=Path.cwd()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    time.sleep(0.01)
"#,
        vec![("ready", YieldStep::None, None)],
    );
    assert_eq!(
        plain.reports.iter().map(ready).collect::<Vec<_>>(),
        [(false, false)]
    );
    assert!(plain.readiness.is_empty());
}

#[test]
fn without_an_ack_the_hint_step_is_unavailable() {
    use cedegrid::contention::YieldStep;
    let (_dir, script) = run_targets(
        r#"
import time
from pathlib import Path
d=Path.cwd()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    time.sleep(0.01)
"#,
        vec![("ready", YieldStep::Hint, Some(100))],
    );
    assert_eq!(
        script.reports.iter().map(applied).collect::<Vec<_>>(),
        [(Some(YieldStep::None), true)]
    );
}

#[test]
fn managed_children_receive_the_target_file_and_its_targets() {
    use cedegrid::contention::YieldStep;
    let (dir, script) = run_targets(
        r#"
import os,sys
from pathlib import Path
from cedegrid import spawn_managed
d=Path.cwd()
child=spawn_managed([sys.executable,'-c',"""
import os,time,pathlib
from cedegrid import ResourceSignals
d=pathlib.Path(%r);signals=ResourceSignals.from_env()
signals.draining();(d/'child-ready').write_text(os.environ.get('CEDEGRID_RESOURCE_TARGET_FILE',''))
while True:
    t=signals.resource_targets()
    if t is not None:
        (d/'child-target').write_text(str(t.ram_mib));break
    time.sleep(0.005)
""" % str(d)],single_process=True,no_escape=True,env={'PYTHONPATH':os.environ['PYTHONPATH']})
child.wait(timeout=20,poll_interval=.01)
while not (d/'stop').exists():
    import time;time.sleep(0.01)
"#,
        vec![("child-ready", YieldStep::Hint, Some(70))],
    );
    assert_eq!(
        script.reports.iter().map(applied).collect::<Vec<_>>(),
        [(Some(YieldStep::Hint), false)]
    );
    let path = fs::read_to_string(dir.path().join("child-ready")).unwrap();
    assert!(path.ends_with("resource-target.json"), "{path}");
    assert_eq!(
        fs::read_to_string(dir.path().join("child-target")).unwrap(),
        "70"
    );
}

/// Sends allowance updates and one hint once the worker is ready, paced so the
/// worker polls between them.
struct AllowanceScript {
    dir: std::path::PathBuf,
    commands: Vec<supervision::SupervisorCommand>,
    sent: usize,
    last: Option<std::time::Instant>,
}
impl supervision::SupervisorControl for AllowanceScript {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        if !self.dir.join("ready").exists()
            || self
                .last
                .is_some_and(|last| last.elapsed() < std::time::Duration::from_millis(300))
        {
            return Ok(vec![]);
        }
        self.last = Some(std::time::Instant::now());
        if self.sent == self.commands.len() {
            fs::write(self.dir.join("stop"), "").unwrap();
            return Ok(vec![]);
        }
        self.sent += 1;
        Ok(vec![self.commands[self.sent - 1].clone()])
    }
}

fn allowance(ram_mib: u64, cpu_millicores: u64) -> supervision::SupervisorCommand {
    supervision::SupervisorCommand::Allowance {
        allowance: cedegrid::model::ResourceAmounts {
            cpu_millicores: Some(cpu_millicores),
            ram_mib: Some(ram_mib),
            gpu_memory_mib: BTreeMap::new(),
        },
    }
}

/// The supervisor publishes the allowance in the target file with its own
/// sequence, also without a target. An allowance-only change calls only the
/// allowance callback, a hint only the target callback, and a managed child
/// started later finds the current allowance in its first file (FR-071).
#[test]
fn allowances_publish_apart_from_targets_and_reach_managed_children() {
    use cedegrid::contention::{ResourceKey, StepRequest, YieldCause, YieldStep};
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import json,sys,time
from pathlib import Path
from cedegrid import ResourceSignals, spawn_managed
d=Path.cwd();signals=ResourceSignals.from_env();targets=[];allowances=[]
signals.on_resource_target(lambda t: targets.append([t.sequence,t.step,t.ram_mib]))
signals.on_resource_allowance(lambda a: allowances.append([a.sequence,a.ram_mib,a.cpu_millicores]))
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    signals.draining()
    time.sleep(0.005)
code="import pathlib;from cedegrid import ResourceSignals;a=ResourceSignals.from_env().resource_allowance();pathlib.Path(%r,'child-seen').write_text(str(None if a is None else a.ram_mib))" % str(d)
child=spawn_managed([sys.executable,'-c',code],single_process=True,no_escape=True)
child.wait(timeout=6,poll_interval=.01)
(d/'seen').write_text(json.dumps({'targets':targets,'allowances':allowances}))
"#,
    );
    let mut script = AllowanceScript {
        dir: dir.path().to_owned(),
        commands: vec![
            allowance(1000, 2000),
            allowance(900, 2000),
            supervision::SupervisorCommand::Adjust {
                sequence: 1,
                resources: BTreeMap::from([(
                    ResourceKey::Ram,
                    StepRequest {
                        step: YieldStep::Hint,
                        target: Some(500),
                        cause: YieldCause::FloorBreach,
                        deadline_ms: None,
                    },
                )]),
            },
            allowance(800, 1500),
        ],
        sent: 0,
        last: None,
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut script,
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    let seen: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("seen")).unwrap()).unwrap();
    assert_eq!(seen["targets"], serde_json::json!([[1, "hint", 500]]));
    assert_eq!(
        seen["allowances"],
        serde_json::json!([[1, 1000, 2000], [2, 900, 2000], [3, 800, 1500]])
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("child-seen")).unwrap(),
        "800"
    );
}

/// A save-progress line raises the target file's `save_progress_sequence` once, with no
/// target and apart from the allowance: the worker's hook fires once per new value and
/// no target or allowance hook fires for it (FR-075).
#[test]
fn save_progress_lines_reach_the_worker_once_each() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import json,time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env();targets=[];allowances=[];saves=[]
signals.on_resource_target(lambda t: targets.append(t.sequence))
signals.on_resource_allowance(lambda a: allowances.append(a.sequence))
signals.on_save_progress(lambda sequence: saves.append(sequence))
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    signals.draining()
    time.sleep(0.005)
(d/'seen').write_text(json.dumps({'targets':targets,'allowances':allowances,'saves':saves}))
"#,
    );
    let mut script = AllowanceScript {
        dir: dir.path().to_owned(),
        commands: vec![
            supervision::SupervisorCommand::SaveProgress,
            allowance(1000, 2000),
            supervision::SupervisorCommand::SaveProgress,
        ],
        sent: 0,
        last: None,
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut script,
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    let seen: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("seen")).unwrap()).unwrap();
    assert_eq!(
        seen,
        serde_json::json!({"targets": [], "allowances": [1], "saves": [1, 2]})
    );
}

/// What the fake RAM cap holds, shared between its actuator and the test.
#[derive(Default)]
struct FakeRam {
    /// Every RAM cap target the supervisor asked the backend to apply.
    caps: Vec<Option<u64>>,
    cap: Option<u64>,
    /// The next tick raises a held cap here, as an ineffective cap does.
    raise_to: Option<u64>,
    refused: bool,
    /// The applied steps each reconcile was given.
    reconciled: Vec<BTreeMap<cedegrid::contention::ResourceKey, cedegrid::contention::StepRequest>>,
}

/// The rootless actuator, plus a RAM cap like the cgroup backend's `memory.high`.
struct FakeRamActuator {
    inner: Box<dyn Actuator>,
    state: std::sync::Arc<std::sync::Mutex<FakeRam>>,
}
impl Actuator for FakeRamActuator {
    fn unavailable(
        &self,
        kind: cedegrid::contention::ResourceKind,
    ) -> std::collections::BTreeSet<cedegrid::contention::YieldStep> {
        use cedegrid::contention::{ResourceKind, YieldStep};
        let mut steps = self.inner.unavailable(kind);
        if kind == ResourceKind::Ram && !self.state.lock().unwrap().refused {
            steps.remove(&YieldStep::Cap);
        }
        steps
    }
    fn apply(
        &mut self,
        family: &mut dyn Family,
        kind: cedegrid::contention::ResourceKind,
        step: cedegrid::contention::YieldStep,
        target: Option<u64>,
    ) -> anyhow::Result<cedegrid::contention::YieldStep> {
        use cedegrid::contention::{ResourceKind, YieldStep};
        if kind != ResourceKind::Ram {
            return self.inner.apply(family, kind, step, target);
        }
        let mut state = self.state.lock().unwrap();
        match step {
            YieldStep::Cap => {
                state.caps.push(target);
                state.cap = target;
            }
            YieldStep::None => state.cap = None,
            other => anyhow::bail!("the fake RAM backend has no {other:?}"),
        }
        Ok(step)
    }
    fn tick(&mut self, family: &mut dyn Family) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.cap.is_some()
            && let Some(raised) = state.raise_to.take()
        {
            state.cap = Some(raised);
            state.refused = true;
        }
        drop(state);
        self.inner.tick(family)
    }
    fn applied_target(&self, kind: cedegrid::contention::ResourceKind) -> Option<u64> {
        (kind == cedegrid::contention::ResourceKind::Ram)
            .then(|| self.state.lock().unwrap().cap)
            .flatten()
    }
    fn step_code(&self, kind: cedegrid::contention::ResourceKind) -> Option<&'static str> {
        (kind == cedegrid::contention::ResourceKind::Ram && self.state.lock().unwrap().refused)
            .then_some("ram_cap_ineffective")
    }
    fn reconcile(
        &mut self,
        family: &mut dyn Family,
        applied: &BTreeMap<cedegrid::contention::ResourceKey, cedegrid::contention::StepRequest>,
    ) -> anyhow::Result<()> {
        use cedegrid::contention::{ResourceKey, YieldStep};
        let mut state = self.state.lock().unwrap();
        state.reconciled.push(applied.clone());
        // A RAM cap no recorded step accounts for is undone.
        if !applied
            .get(&ResourceKey::Ram)
            .is_some_and(|step| matches!(step.step, YieldStep::Cap | YieldStep::Hint))
        {
            state.cap = None;
        }
        drop(state);
        self.inner.reconcile(family, applied)
    }
    fn release(&mut self, family: &mut dyn Family) -> anyhow::Result<()> {
        self.state.lock().unwrap().cap = None;
        self.inner.release(family)
    }
}

/// The rootless backend with the fake RAM cap.
struct FakeRamBackend {
    inner: RootlessBackend,
    state: std::sync::Arc<std::sync::Mutex<FakeRam>>,
}
impl LaunchBackend for FakeRamBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn actuator(&self, settings: &ActuatorSettings) -> Box<dyn Actuator> {
        Box::new(FakeRamActuator {
            inner: self.inner.actuator(settings),
            state: self.state.clone(),
        })
    }
    fn prepare(&mut self, request: &LaunchRequest) -> anyhow::Result<GateSetup> {
        self.inner.prepare(request)
    }
    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        self.inner.preparation_evidence()
    }
    fn verify(&mut self, identity: &ProcessIdentity) -> anyhow::Result<Vec<ControlEvidence>> {
        self.inner.verify(identity)
    }
    fn confirm_release(&mut self) -> anyhow::Result<()> {
        self.inner.confirm_release()
    }
}

/// Caps RAM below the Guaranteed request, lets the cap turn ineffective, hints,
/// then queries, each once the previous report arrived; it keeps the journal's
/// contention at each report.
struct RamCapScript {
    dir: std::path::PathBuf,
    state: std::sync::Arc<std::sync::Mutex<FakeRam>>,
    stage: usize,
    reports: Vec<supervision::SupervisorReport>,
    journaled: Vec<Option<cedegrid::contention::AllocationContention>>,
    /// The backend's RAM cap when each report arrived.
    held: Vec<Option<u64>>,
}
impl supervision::SupervisorControl for RamCapScript {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        use cedegrid::contention::{ResourceKey, StepRequest, YieldCause, YieldStep};
        let adjust = |sequence, step, target| supervision::SupervisorCommand::Adjust {
            sequence,
            resources: BTreeMap::from([(
                ResourceKey::Ram,
                StepRequest {
                    step,
                    target,
                    cause: YieldCause::FloorBreach,
                    deadline_ms: None,
                },
            )]),
        };
        let commands = match (self.stage, self.reports.len()) {
            (0, 0) if self.dir.join("ready").exists() => vec![adjust(1, YieldStep::Cap, Some(10))],
            (1, 1) => {
                self.state.lock().unwrap().raise_to = Some(512);
                vec![]
            }
            (2, 2) => vec![adjust(2, YieldStep::Hint, Some(64))],
            (3, 3) => vec![supervision::SupervisorCommand::Query],
            (4, 4) => {
                fs::write(self.dir.join("stop"), "").unwrap();
                vec![]
            }
            _ => return Ok(vec![]),
        };
        self.stage += 1;
        Ok(commands)
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        if !matches!(report, supervision::SupervisorReport::Readiness { .. }) {
            self.reports.push(report);
            self.held.push(self.state.lock().unwrap().cap);
            self.journaled.push(
                StateStore::open_read_only(&self.dir.join("state"))?
                    .contention("family-attempt")?,
            );
        }
        Ok(())
    }
}

/// The supervisor end to end with a backend that can cap RAM (FR-011, FR-024):
/// a Guaranteed cap is clamped to the request and journaled, a cap the backend
/// raised is reported unprompted, a hint keeps the held cap, and a query
/// reconciles the actuator with the recorded steps.
#[test]
fn a_ram_cap_is_clamped_recorded_reported_kept_under_a_hint_and_reconciled() {
    use cedegrid::contention::{ResourceKey, YieldStep};
    use supervision::SupervisorReport::{Adjusted, State};
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let request = request(
        dir.path(),
        r#"
import time
from pathlib import Path
from cedegrid import ResourceSignals
d=Path.cwd();signals=ResourceSignals.from_env();hint=None
signals.draining()
(d/'ready').write_text('1')
while not (d/'stop').exists():
    t=signals.resource_targets()
    if t is not None and t.step=='hint': hint=t.ram_mib
    time.sleep(0.005)
(d/'hint').write_text(str(hint))
"#,
    );
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeRam::default()));
    let mut script = RamCapScript {
        dir: dir.path().to_owned(),
        state: state.clone(),
        stage: 0,
        reports: vec![],
        journaled: vec![],
        held: vec![],
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut FakeRamBackend {
            inner: RootlessBackend::new(10),
            state: state.clone(),
        },
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut script,
    )
    .unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert_eq!(outcome.exit_code, Some(0), "{:?}", script.reports);
    let ram = |map: &BTreeMap<ResourceKey, u64>| map.get(&ResourceKey::Ram).copied();
    let journaled = |index: usize| {
        script.journaled[index]
            .as_ref()
            .and_then(|c| c.applied.get(&ResourceKey::Ram))
            .map(|step| (step.step, step.target))
    };
    assert_eq!(script.reports.len(), 4, "{:?}", script.reports);
    // The Guaranteed clamp: a 10 MiB cap is applied at the 128 MiB request, and
    // the journal records the applied target.
    match &script.reports[0] {
        Adjusted {
            sequence,
            applied,
            applied_targets,
            ..
        } => {
            assert_eq!(*sequence, 1);
            assert_eq!(applied.get(&ResourceKey::Ram), Some(&YieldStep::Cap));
            assert_eq!(ram(applied_targets), Some(128));
        }
        other => panic!("unexpected report {other:?}"),
    }
    assert_eq!(journaled(0), Some((YieldStep::Cap, Some(128))));
    // The unprompted report: the backend raised the cap, so the supervisor reports
    // the new target, the code and the unavailable cap with no command, and
    // journals the raised target.
    match &script.reports[1] {
        Adjusted {
            sequence,
            applied,
            unavailable,
            applied_targets,
            step_codes,
            ..
        } => {
            assert_eq!(*sequence, 1);
            assert_eq!(applied.get(&ResourceKey::Ram), Some(&YieldStep::Cap));
            assert_eq!(ram(applied_targets), Some(512));
            assert_eq!(
                step_codes.get(&ResourceKey::Ram).map(String::as_str),
                Some("ram_cap_ineffective")
            );
            assert_eq!(
                unavailable.get(&ResourceKey::Ram),
                Some(&vec![YieldStep::Cap])
            );
        }
        other => panic!("unexpected report {other:?}"),
    }
    assert_eq!(journaled(1), Some((YieldStep::Cap, Some(512))));
    // Keeping the cap under a hint: the hint is published, and the backend's cap
    // is neither reapplied nor released.
    match &script.reports[2] {
        Adjusted {
            sequence, applied, ..
        } => {
            assert_eq!(*sequence, 2);
            assert_eq!(applied.get(&ResourceKey::Ram), Some(&YieldStep::Hint));
        }
        other => panic!("unexpected report {other:?}"),
    }
    assert_eq!(journaled(2), Some((YieldStep::Hint, Some(64))));
    // Reconcile on query: the actuator sees the recorded hint and keeps its cap.
    match &script.reports[3] {
        State {
            applied, targets, ..
        } => {
            assert_eq!(applied.get(&ResourceKey::Ram), Some(&YieldStep::Hint));
            assert_eq!(ram(targets), Some(64));
        }
        other => panic!("unexpected report {other:?}"),
    }
    assert_eq!(script.held, [Some(128), Some(512), Some(512), Some(512)]);
    let state = state.lock().unwrap();
    assert_eq!(state.caps, [Some(128)]);
    assert_eq!(state.reconciled.len(), 1);
    assert_eq!(
        state.reconciled[0]
            .get(&ResourceKey::Ram)
            .map(|step| step.step),
        Some(YieldStep::Hint)
    );
    assert_eq!(fs::read_to_string(dir.path().join("hint")).unwrap(), "64");
    // Release undid the cap before the run ended.
    assert_eq!(state.cap, None);
}

/// Stops the leader once it is up, writes `paused`, then waits to be killed or signalled:
/// Opportunistic work by a pause, Guaranteed work, which is never paused, by a CPU cap at its
/// request that its spinning leader runs above.
#[cfg(target_os = "linux")]
struct PauseThenWait {
    dir: std::path::PathBuf,
    paused: bool,
    guaranteed: bool,
}
#[cfg(target_os = "linux")]
impl supervision::SupervisorControl for PauseThenWait {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        use cedegrid::contention::{ResourceKey, StepRequest, YieldCause, YieldStep};
        if !self.paused && self.dir.join("leader.pid").exists() {
            self.paused = true;
            if !self.guaranteed {
                return Ok(vec![cpu_step(1, YieldStep::Pause)]);
            }
            return Ok(vec![supervision::SupervisorCommand::Adjust {
                sequence: 1,
                resources: BTreeMap::from([(
                    ResourceKey::Cpu,
                    StepRequest {
                        step: YieldStep::Cap,
                        target: Some(1),
                        cause: YieldCause::ExternalGrowth,
                        deadline_ms: None,
                    },
                )]),
            }]);
        }
        Ok(vec![])
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        if matches!(report, supervision::SupervisorReport::Adjusted { .. }) {
            fs::write(self.dir.join("paused"), "").unwrap();
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "launched only by the supervisor-death tests"]
fn paused_supervisor_fixture() {
    let dir = std::path::PathBuf::from(std::env::var("CEDEGRID_DEATH_FIXTURE_DIR").unwrap());
    if std::env::var_os("CEDEGRID_DEATH_FIXTURE_HANDLERS").is_some() {
        supervision::install_stop_handlers().unwrap();
    }
    let store = StateStore::open(&dir.join("state")).unwrap();
    let guaranteed =
        std::env::var("CEDEGRID_DEATH_FIXTURE_CLASS").as_deref() != Ok("opportunistic");
    let mut request = request(
        &dir,
        r#"
import os
from pathlib import Path
Path('leader.pid').write_text(str(os.getpid()))
while True:
    pass
"#,
    );
    if guaranteed {
        request.resources.cpu_millicores = 200;
    } else {
        request.class = AllocationClass::Opportunistic;
    }
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut PauseThenWait {
            dir: dir.clone(),
            paused: false,
            guaranteed,
        },
    );
    fs::write(
        dir.join("outcome"),
        format!("{:?}", outcome.map(|o| o.record.phase)),
    )
    .unwrap();
}

/// A pidfd for a fixture's leader, for observing it without its numeric PID and for cleanup.
#[cfg(target_os = "linux")]
struct Watched(std::os::fd::OwnedFd);
#[cfg(target_os = "linux")]
impl Watched {
    fn open(pid: u32) -> Self {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        assert!(raw >= 0, "pidfd_open");
        Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) })
    }
    fn exited_within(&self, wait: std::time::Duration) -> bool {
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (unsafe { libc::poll(&mut poll, 1, wait.as_millis() as i32) }) > 0
    }
}
#[cfg(target_os = "linux")]
impl Drop for Watched {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
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

#[cfg(target_os = "linux")]
fn start_paused_fixture(
    dir: &Path,
    class: &str,
    handlers: bool,
) -> (std::process::Child, u32, Watched) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "paused_supervisor_fixture"])
        .env("CEDEGRID_DEATH_FIXTURE_DIR", dir)
        .env("CEDEGRID_DEATH_FIXTURE_CLASS", class)
        .stdout(std::process::Stdio::null());
    if handlers {
        command.env("CEDEGRID_DEATH_FIXTURE_HANDLERS", "1");
    }
    let mut fixture = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dir.join("paused").exists() {
        if std::time::Instant::now() > deadline {
            let _ = fixture.kill();
            panic!("the fixture never paused its leader");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid: u32 = fs::read_to_string(dir.join("leader.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let watched = Watched::open(pid);
    // A pause stops it at once; a cap stops it in each off phase.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !run_state(pid).contains('T') {
        assert!(
            std::time::Instant::now() < deadline,
            "the leader is never stopped"
        );
    }
    (fixture, pid, watched)
}

/// A supervisor that dies while its run is paused takes Opportunistic work with it and lets
/// Guaranteed work continue, through the gate's parent-death signal (FR-078).
#[cfg(target_os = "linux")]
#[test]
fn a_supervisor_that_dies_kills_opportunistic_work_and_resumes_guaranteed_work() {
    for class in ["opportunistic", "guaranteed"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut fixture, pid, watched) = start_paused_fixture(dir.path(), class, false);
        fixture.kill().unwrap();
        fixture.wait().unwrap();
        if class == "opportunistic" {
            assert!(
                watched.exited_within(std::time::Duration::from_secs(5)),
                "an Opportunistic leader without its supervisor is killed"
            );
        } else {
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!watched.exited_within(std::time::Duration::ZERO));
            assert!(
                !run_state(pid).contains('T'),
                "a Guaranteed leader is continued"
            );
        }
    }
}

/// SIGTERM, SIGINT or SIGHUP to a supervisor releases its steps and drains its run (FR-078).
#[cfg(target_os = "linux")]
#[test]
fn a_signalled_supervisor_releases_its_steps_and_drains() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let dir = tempfile::tempdir().unwrap();
        let (mut fixture, _pid, watched) = start_paused_fixture(dir.path(), "opportunistic", true);
        unsafe { libc::kill(fixture.id() as i32, signal) };
        assert!(
            watched.exited_within(std::time::Duration::from_secs(10)),
            "signal {signal}: the drained leader exits"
        );
        let status = fixture.wait().unwrap();
        assert!(
            status.success(),
            "signal {signal}: the supervisor ends normally"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("outcome")).unwrap(),
            "Ok(Released)",
            "signal {signal}"
        );
    }
}

/// Sends a CPU pause once the leader has exited and been reaped, then records the next adjust
/// report.
struct PauseAfterLeaderExit {
    dir: std::path::PathBuf,
    sent: bool,
    applied: Option<cedegrid::contention::YieldStep>,
    child_state: Option<String>,
}
impl supervision::SupervisorControl for PauseAfterLeaderExit {
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        let reaped = self.dir.join("leader-exited").exists()
            && fs::read_to_string(self.dir.join("leader.pid"))
                .is_ok_and(|pid| run_state(pid.trim().parse().unwrap()).is_empty());
        if !self.sent && reaped {
            self.sent = true;
            return Ok(vec![cpu_step(1, cedegrid::contention::YieldStep::Pause)]);
        }
        Ok(vec![])
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        if let supervision::SupervisorReport::Adjusted { applied, .. } = report {
            self.applied = Some(applied[&cedegrid::contention::ResourceKey::Cpu]);
            std::thread::sleep(std::time::Duration::from_millis(100));
            let pid = fs::read_to_string(self.dir.join("child.pid")).unwrap();
            self.child_state = Some(run_state(pid.trim().parse().unwrap()));
        }
        Ok(())
    }
}

/// Once the leader has exited, its draining children take no new runtime step, so a pause
/// cannot stop a child that must see its drain file (030, SUP-6).
#[test]
fn no_runtime_step_reaches_children_draining_after_the_leader_exit() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let mut request = request(
        dir.path(),
        r#"
import os,sys,time
from pathlib import Path
from cedegrid import spawn_managed
d=Path.cwd()
child=spawn_managed([sys.executable,'-c',"""
import os,time,pathlib
d=pathlib.Path(%r)
(d/'child.pid').write_text(str(os.getpid()))
drain=pathlib.Path(os.environ['CEDEGRID_DRAIN_FILE'])
while not drain.exists():
    time.sleep(0.02)
time.sleep(0.5)
(d/'drained').write_text('1')
""" % str(d)],single_process=True,no_escape=True)
while not (d/'child.pid').exists():
    time.sleep(0.02)
(d/'leader.pid').write_text(str(os.getpid()))
(d/'leader-exited').write_text('1')
"#,
    );
    request.class = AllocationClass::Opportunistic;
    let mut control = PauseAfterLeaderExit {
        dir: dir.path().to_owned(),
        sent: false,
        applied: None,
        child_state: None,
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &SupervisorOptions {
            drain_timeout_ms: 3_000,
            term_grace_ms: 1_000,
            ..options()
        },
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut control,
    )
    .unwrap();
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert_eq!(control.applied, Some(cedegrid::contention::YieldStep::None));
    assert!(
        !control.child_state.clone().unwrap().contains('T'),
        "{:?}",
        control.child_state
    );
    assert!(
        dir.path().join("drained").exists(),
        "the child drained on its own"
    );
}

/// A failure in the supervisor's own bookkeeping, here a child's journal transition, holds the
/// run and is retried; it never drains the leader (030, R3).
#[test]
fn a_bookkeeping_failure_holds_and_retries_instead_of_draining() {
    let dir = tempfile::tempdir().unwrap();
    let journal = InjectJournal {
        store: StateStore::open(&dir.path().join("state")).unwrap(),
        stage: 4,
        pause: None,
    };
    let mut request = request(
        dir.path(),
        r#"
import sys,time
from pathlib import Path
from cedegrid import spawn_managed
child=spawn_managed([sys.executable,'-c','pass'],single_process=True,no_escape=True)
child.wait(timeout=10,poll_interval=.01)
time.sleep(0.3)
Path('leader-done').write_text('1')
"#,
    );
    request.class = AllocationClass::Opportunistic;
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options(),
        &journal,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert_eq!((outcome.exit_code, outcome.yielded), (Some(0), false));
    assert!(
        dir.path().join("leader-done").exists(),
        "the leader ran to its end"
    );
    assert_eq!(
        journal.store.managed_children("family-attempt").unwrap()[0].phase,
        ManagedChildPhase::Released,
        "the release transition went through on a retry"
    );
}

/// A control driven by a closure over the reports so far, for the family tests (item 031).
#[cfg(target_os = "linux")]
struct Driven<F: FnMut(&[supervision::SupervisorReport]) -> Vec<supervision::SupervisorCommand>> {
    reports: Vec<supervision::SupervisorReport>,
    step: F,
}
#[cfg(target_os = "linux")]
impl<F: FnMut(&[supervision::SupervisorReport]) -> Vec<supervision::SupervisorCommand>>
    supervision::SupervisorControl for Driven<F>
{
    fn poll(&mut self) -> anyhow::Result<Vec<supervision::SupervisorCommand>> {
        Ok((self.step)(&self.reports))
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> anyhow::Result<()> {
        self.reports.push(report);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn pids_in(dir: &Path, prefix: &str) -> Vec<u32> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().ok()?;
            name.strip_prefix(prefix)?.parse().ok()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn oom_of(pid: u32) -> i32 {
    fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A Pause reaches the whole family, members the leader keeps forking during the stop
/// included, and the continue reaches them all again; a process the test started before
/// the run is never stopped (FR-086, R38, Constitution I).
#[cfg(target_os = "linux")]
#[test]
fn a_pause_reaches_members_forked_during_the_stop() {
    use cedegrid::contention::YieldStep;
    let dir = tempfile::tempdir().unwrap();
    let mut foreign = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut request = forking(
        dir.path(),
        r#"
import os, signal, time
from pathlib import Path
kids = []
Path('leader-' + str(os.getpid())).touch()
while not Path('stop').exists():
    pid = os.fork()
    if pid == 0:
        Path('kid-' + str(os.getpid())).touch()
        time.sleep(30)
        os._exit(0)
    kids.append(pid)
    time.sleep(0.03)
for pid in kids:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
"#,
    );
    request.class = AllocationClass::Opportunistic;
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let path = dir.path().to_owned();
    let (mut stage, mut at) = (0, std::time::Instant::now());
    let (mut paused, mut resumed) = (Vec::new(), Vec::new());
    let mut control = Driven {
        reports: vec![],
        step: |reports: &[supervision::SupervisorReport]| {
            let adjusted = reports
                .iter()
                .filter(|r| matches!(r, supervision::SupervisorReport::Adjusted { .. }))
                .count();
            let states = || -> Vec<(u32, String)> {
                pids_in(&path, "leader-")
                    .into_iter()
                    .chain(pids_in(&path, "kid-"))
                    .map(|pid| (pid, run_state(pid)))
                    .filter(|(_, state)| !state.is_empty() && !state.starts_with('Z'))
                    .collect()
            };
            match stage {
                0 if pids_in(&path, "kid-").len() >= 3 => {
                    stage = 1;
                    return vec![cpu_step(1, YieldStep::Pause)];
                }
                1 if adjusted == 1 => {
                    (stage, at) = (2, std::time::Instant::now());
                }
                // A member forked as the stop began is stopped by the pass loop or by the
                // next interval's discovery at the latest.
                2 if at.elapsed() >= std::time::Duration::from_millis(400) => {
                    paused = states();
                    stage = 3;
                    return vec![cpu_step(2, YieldStep::None)];
                }
                3 if adjusted == 2 => {
                    (stage, at) = (4, std::time::Instant::now());
                }
                4 if at.elapsed() >= std::time::Duration::from_millis(200) => {
                    resumed = states();
                    stage = 5;
                    fs::write(path.join("stop"), "").unwrap();
                }
                _ => {}
            }
            vec![]
        },
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut control,
    )
    .unwrap();
    drop(control);
    let foreign_state = run_state(foreign.id());
    foreign.kill().unwrap();
    foreign.wait().unwrap();
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    assert!(paused.len() >= 4, "{paused:?}");
    assert!(
        paused.iter().all(|(_, state)| state.contains('T')),
        "{paused:?}"
    );
    assert!(
        resumed.iter().all(|(_, state)| !state.contains('T')),
        "{resumed:?}"
    );
    assert!(!foreign_state.contains('T'), "{foreign_state}");
}

/// Each member returns to its own `oom_score_adj` baseline: a worker forked while its run is
/// marked inherits the mark, is marked as a member when it joins, and returns to its parent's
/// baseline, not to the inherited mark; a process outside the family is never changed
/// (FR-023, FR-086, spec Q9, Constitution I).
#[cfg(target_os = "linux")]
#[test]
fn members_return_to_their_own_oom_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let mut foreign = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let request = forking(
        dir.path(),
        r#"
import os, signal, time
from pathlib import Path
def worker(name):
    pid = os.fork()
    if pid == 0:
        if name == 'early':
            Path('/proc/self/oom_score_adj').write_text('500')
        Path(name + '-' + str(os.getpid())).touch()
        time.sleep(30)
        os._exit(0)
    return pid
Path('leader-' + str(os.getpid())).touch()
kids = [worker('early')]
while not Path('marked').exists():
    time.sleep(0.01)
kids.append(worker('late'))
while not Path('stop').exists():
    time.sleep(0.01)
for pid in kids:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
"#,
    );
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let path = dir.path().to_owned();
    let foreign_before = oom_of(foreign.id());
    let (mut stage, mut at) = (0, std::time::Instant::now());
    let mut baseline = None;
    let mut seen: Vec<(&'static str, i32)> = Vec::new();
    let mut control = Driven {
        reports: vec![],
        step: |_: &[supervision::SupervisorReport]| {
            let one = |prefix: &str| pids_in(&path, prefix).first().copied();
            match stage {
                0 if one("early-").is_some() => (stage, at) = (1, std::time::Instant::now()),
                1 if at.elapsed() >= std::time::Duration::from_millis(300) => {
                    baseline = Some(oom_of(one("leader-").unwrap()));
                    stage = 2;
                    return vec![supervision::SupervisorCommand::Oom { preferred: true }];
                }
                2 if oom_of(one("early-").unwrap()) == 1000 => {
                    seen.push(("leader marked", oom_of(one("leader-").unwrap())));
                    fs::write(path.join("marked"), "").unwrap();
                    stage = 3;
                }
                3 if one("late-").is_some() => (stage, at) = (4, std::time::Instant::now()),
                4 if at.elapsed() >= std::time::Duration::from_millis(400) => {
                    seen.push(("late marked", oom_of(one("late-").unwrap())));
                    stage = 5;
                    return vec![supervision::SupervisorCommand::Oom { preferred: false }];
                }
                5 if oom_of(one("leader-").unwrap()) == baseline.unwrap() => {
                    (stage, at) = (6, std::time::Instant::now());
                }
                6 if at.elapsed() >= std::time::Duration::from_millis(200) => {
                    for (name, prefix) in [
                        ("leader", "leader-"),
                        ("early", "early-"),
                        ("late", "late-"),
                    ] {
                        seen.push((name, oom_of(one(prefix).unwrap())));
                    }
                    stage = 7;
                    fs::write(path.join("stop"), "").unwrap();
                }
                _ => {}
            }
            vec![]
        },
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options(),
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut control,
    )
    .unwrap();
    drop(control);
    let foreign_after = oom_of(foreign.id());
    foreign.kill().unwrap();
    foreign.wait().unwrap();
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    let baseline = baseline.unwrap();
    assert_eq!(
        seen,
        [
            ("leader marked", 1000),
            ("late marked", 1000),
            ("leader", baseline),
            ("early", 500),
            ("late", baseline)
        ]
    );
    assert_eq!(foreign_after, foreign_before);
}

#[cfg(target_os = "linux")]
fn gone_and_reaped(pid: u32) -> bool {
    let mut status = 0;
    // SAFETY: a probe and a non-blocking wait for one PID this test's run started.
    let reaped = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    // SAFETY: signal zero only probes whether the process still exists.
    reaped && unsafe { libc::kill(pid as i32, 0) } != 0
}

/// When the leader exits, its members still alive get SIGTERM, then SIGKILL after the grace,
/// in both classes; the outcome counts them, none is left as a zombie, and a process outside
/// the family is not ended (FR-087, Constitution I).
#[cfg(target_os = "linux")]
#[test]
fn leftover_members_are_ended_when_the_leader_exits() {
    for class in [AllocationClass::Guaranteed, AllocationClass::Opportunistic] {
        let dir = tempfile::tempdir().unwrap();
        let mut foreign = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut request = forking(
            dir.path(),
            r#"
import os, signal, time
from pathlib import Path
for name in ['polite', 'stubborn']:
    if os.fork() == 0:
        if name == 'stubborn':
            signal.signal(signal.SIGTERM, lambda *_: Path('stubborn-term').touch())
        Path(name + '-' + str(os.getpid())).touch()
        while True:
            time.sleep(1)
while len(list(Path('.').glob('polite-*')) + list(Path('.').glob('stubborn-*'))) < 2:
    time.sleep(0.01)
"#,
        );
        request.class = class;
        let (outcome, _) = supervise_family(&request, dir.path());
        let foreign_alive = foreign.try_wait().unwrap().is_none();
        foreign.kill().unwrap();
        foreign.wait().unwrap();
        assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
        assert_eq!(outcome.members_ended, 2, "{class:?}: {outcome:?}");
        assert!(dir.path().join("stubborn-term").exists(), "TERM came first");
        for pid in pids_in(dir.path(), "polite-")
            .into_iter()
            .chain(pids_in(dir.path(), "stubborn-"))
        {
            assert!(gone_and_reaped(pid), "{class:?}: member {pid} left");
        }
        assert!(foreign_alive);
    }
}

/// A drain signals the leader first, so the workload can stop its own workers; members still
/// alive after the grace get SIGKILL. A leader that exits during the drain leaves its members
/// TERMed at once and KILLed by the drain's deadline (FR-087, item 031 D.2).
#[cfg(target_os = "linux")]
#[test]
fn a_drain_signals_the_leader_first_then_kills_members() {
    for leader_exits in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut request = forking(
            dir.path(),
            &format!(
                r#"
import os, signal, sys, time
from pathlib import Path
if os.fork() == 0:
    signal.signal(signal.SIGTERM, lambda *_: Path('worker-term').touch())
    Path('worker-' + str(os.getpid())).touch()
    while True:
        time.sleep(1)
def term(*_):
    Path('leader-term').touch()
    if {leader_exits}:
        sys.exit(0)
signal.signal(signal.SIGTERM, term)
Path('ready').touch()
while True:
    time.sleep(1)
"#,
                leader_exits = if leader_exits { "True" } else { "False" }
            ),
        );
        request.class = AllocationClass::Opportunistic;
        let store = StateStore::open(&dir.path().join("state")).unwrap();
        let path = dir.path().to_owned();
        let mut sent = None;
        let mut control = Driven {
            reports: vec![],
            step: |_: &[supervision::SupervisorReport]| {
                if sent.is_none()
                    && path.join("ready").exists()
                    && !pids_in(&path, "worker-").is_empty()
                {
                    sent = Some(std::time::Instant::now());
                    return vec![supervision::SupervisorCommand::Drain];
                }
                vec![]
            },
        };
        let outcome = supervision::supervise_controlled(
            &request,
            &request.resources,
            &options(),
            &store,
            &mut RootlessBackend::new(10),
            Path::new(env!("CARGO_BIN_EXE_cedegrid")),
            &mut control,
        )
        .unwrap();
        drop(control);
        let took = sent.unwrap().elapsed();
        assert!(outcome.yielded, "{outcome:?}");
        assert!(dir.path().join("leader-term").exists());
        assert_eq!(outcome.members_ended, 1, "{outcome:?}");
        let worker = pids_in(dir.path(), "worker-")[0];
        assert!(gone_and_reaped(worker));
        // The drain's TERM went to the leader alone; the worker was TERMed only once the
        // leader had exited, and is ended within the drain's deadlines either way.
        assert_eq!(dir.path().join("worker-term").exists(), leader_exits);
        let deadline = options().drain_timeout_ms + 2 * options().term_grace_ms + 1_000;
        assert!(took.as_millis() < u128::from(deadline), "{took:?}");
    }
}

/// A member found in a stop pass is reported at once, not at the next interval, and the
/// family report carries the run's user and group IDs, so an agent can recover the member if
/// the supervisor dies then (FR-083, FR-088).
#[cfg(target_os = "linux")]
#[test]
fn a_member_found_in_a_stop_is_reported_at_once() {
    use cedegrid::contention::YieldStep;
    let dir = tempfile::tempdir().unwrap();
    let mut request = forking(
        dir.path(),
        r#"
import os, time
from pathlib import Path
time.sleep(0.3)
if os.fork() == 0:
    Path('child-%d' % os.getpid()).touch()
    time.sleep(3)
    os._exit(0)
time.sleep(3)
"#,
    );
    request.class = AllocationClass::Opportunistic;
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let path = dir.path().to_owned();
    let (mut paused_at, mut reported) = (None, None);
    let mut control = Driven {
        reports: vec![],
        step: |reports: &[supervision::SupervisorReport]| {
            let child: Option<u32> = pids_in(&path, "child-").first().copied();
            if paused_at.is_none() && child.is_some() {
                paused_at = Some(std::time::Instant::now());
                return vec![cpu_step(1, YieldStep::Pause)];
            }
            if let (Some(at), Some(child), None) = (paused_at, child, reported.as_ref()) {
                let family = reports.iter().rev().find_map(|r| match r {
                    supervision::SupervisorReport::Family { family } => Some(family.clone()),
                    _ => None,
                });
                if let Some(family) =
                    family.filter(|f| f.descendants.iter().any(|m| m.pid == child))
                {
                    reported = Some((at.elapsed(), family));
                    return vec![cpu_step(2, YieldStep::None)];
                }
            }
            vec![]
        },
    };
    let options = SupervisorOptions {
        duty_period_ms: 2_000,
        ..options()
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &request.resources,
        &options,
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        &mut control,
    )
    .unwrap();
    drop(control);
    assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
    let (after, family) = reported.expect("the stopped member was reported");
    assert!(after < std::time::Duration::from_millis(1_000), "{after:?}");
    // SAFETY: getuid and getgid cannot fail.
    assert_eq!((family.uid, family.gid), unsafe {
        (libc::getuid(), libc::getgid())
    });
}
