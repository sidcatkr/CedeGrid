#![cfg(unix)]
use anyhow::{Result, bail};
use cedegrid::{
    execution_model::*,
    model::Resources,
    supervision::{self, SupervisorOptions},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Journal {
    records: RefCell<Vec<ExecutionRecord>>,
    fail: Option<ExecutionPhase>,
    fail_reserve: bool,
    ready: Option<PathBuf>,
    pause_phase: ExecutionPhase,
}
impl Journal {
    fn new() -> Self {
        Self {
            records: RefCell::new(vec![]),
            fail: None,
            fail_reserve: false,
            ready: None,
            pause_phase: ExecutionPhase::Prepared,
        }
    }
    fn last(&self) -> ExecutionRecord {
        self.records.borrow().last().unwrap().clone()
    }
}
impl ExecutionJournal for Journal {
    fn reserve(&self, request: &LaunchRequest, _: &Resources) -> Result<ExecutionRecord> {
        if self.fail_reserve {
            bail!("injected reservation persistence failure");
        }
        let record = ExecutionRecord {
            task_id: request.task_id.clone(),
            assignment_id: request.assignment_id.clone(),
            generation: 7,
            class: request.class,
            phase: ExecutionPhase::Reserved,
            resources: request.resources.clone(),
            identity: None,
            backend: "pending".into(),
            evidence: vec![],
            detail: String::new(),
        };
        self.records.borrow_mut().push(record.clone());
        Ok(record)
    }
    fn transition(&self, record: &ExecutionRecord) -> Result<()> {
        if self.fail.as_ref() == Some(&record.phase) {
            bail!("injected {:?} persistence failure", record.phase);
        }
        self.records.borrow_mut().push(record.clone());
        if record.phase == self.pause_phase
            && let Some(path) = &self.ready
        {
            fs::write(path, serde_json::to_vec(&record.identity)?)?;
            // Outer test kills this supervisor while the gate still awaits EXEC.
            std::thread::sleep(Duration::from_secs(20));
        }
        Ok(())
    }
}

struct Backend {
    fail_prepare: bool,
    bad_setup: bool,
    fail_verify: bool,
    fail_release: bool,
    partial: bool,
    prepared: bool,
    verified: bool,
    released: bool,
}
impl Backend {
    fn new() -> Self {
        Self {
            fail_prepare: false,
            bad_setup: false,
            fail_verify: false,
            fail_release: false,
            partial: false,
            prepared: false,
            verified: false,
            released: false,
        }
    }
}
impl LaunchBackend for Backend {
    fn name(&self) -> &str {
        "rootless"
    }
    fn prepare(&mut self, _: &LaunchRequest) -> Result<GateSetup> {
        self.prepared = true;
        if self.fail_prepare {
            bail!("injected control preparation failure");
        }
        Ok(GateSetup {
            cgroup_path: None,
            nice: if self.bad_setup { Some(-1) } else { None },
            oom_score_adj: None,
        })
    }
    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        if !self.partial {
            return vec![];
        }
        vec![ControlEvidence {
            control: "cpu.weight".into(),
            available: Some(true),
            permitted: Some(true),
            configured: true,
            applied: true,
            fallback: false,
            scope: "fixture".into(),
            requested: Some("10".into()),
            effective: Some("10".into()),
            detail: "partial application fixture".into(),
        }]
    }
    fn verify(&mut self, _: &ProcessIdentity) -> Result<Vec<ControlEvidence>> {
        self.verified = true;
        if self.fail_verify {
            bail!("injected backend membership verification failure");
        }
        Ok(vec![])
    }
    fn confirm_release(&mut self) -> Result<()> {
        self.released = true;
        if self.fail_release {
            bail!("unconfirmed descendants / release");
        }
        Ok(())
    }
}
fn request(dir: &Path) -> LaunchRequest {
    LaunchRequest {
        task_id: "task".into(),
        assignment_id: "assignment".into(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf authorized > marker".into(),
        ],
        cwd: dir.to_path_buf(),
        env: BTreeMap::new(),
        resources: Resources {
            cpu_millicores: 100,
            ram_mib: 1,
            ..Default::default()
        },
        replay_safe: true,
        class: AllocationClass::Guaranteed,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    }
}
fn options() -> SupervisorOptions {
    SupervisorOptions {
        nice: 10,
        lease_ms: 40,
        drain_timeout_ms: 20,
        term_grace_ms: 20,
        prepare_timeout_ms: 2_000,
        release_confirm_timeout_ms: 2_000,
        duty_period_ms: 100,
        hint_deadline_ms: 5_000,
    }
}
fn run(
    request: &LaunchRequest,
    options: &SupervisorOptions,
    journal: &Journal,
    backend: &mut Backend,
) -> Result<supervision::ExecutionOutcome> {
    supervision::supervise(
        request,
        &Resources {
            cpu_millicores: 1_000,
            ram_mib: 1_000,
            ..Default::default()
        },
        options,
        journal,
        backend,
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
}

#[test]
fn gate_runs_only_after_prepared_and_authorized_commits() {
    let temp = TempDir::new().unwrap();
    let req = request(temp.path());
    let journal = Journal::new();
    let outcome = run(&req, &options(), &journal, &mut Backend::new()).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("marker")).unwrap(),
        "authorized"
    );
    let phases: Vec<_> = journal
        .records
        .borrow()
        .iter()
        .map(|r| r.phase.clone())
        .collect();
    assert_eq!(
        phases,
        vec![
            ExecutionPhase::Reserved,
            ExecutionPhase::Prepared,
            ExecutionPhase::Authorized,
            ExecutionPhase::Running,
            ExecutionPhase::Released
        ]
    );
    let records = journal.records.borrow();
    let prepared = &records[1];
    assert_eq!(prepared.identity.as_ref().unwrap().generation, 7);
    assert_eq!(
        prepared.identity.as_ref().unwrap().assignment_id,
        "assignment"
    );
    assert!(!prepared.identity.as_ref().unwrap().boot_id.is_empty());
    assert!(prepared.identity.as_ref().unwrap().start_time > 0);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.yielded);
    assert!(outcome.release_confirmed_ms.is_some());
    #[cfg(target_os = "macos")]
    assert!(outcome.process_handle.contains("fallback"));
}

#[test]
fn every_preparation_failure_keeps_user_code_behind_barrier() {
    for stage in [
        "reserve",
        "prepare",
        "setup",
        "verify",
        "required",
        "prepared_commit",
        "authorized_commit",
    ] {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        let mut journal = Journal::new();
        let mut backend = Backend::new();
        match stage {
            "reserve" => journal.fail_reserve = true,
            "prepare" => {
                backend.fail_prepare = true;
                backend.partial = true;
            }
            "setup" => backend.bad_setup = true,
            "verify" => backend.fail_verify = true,
            "required" => req.required_controls = vec!["unavailable_hard_control".into()],
            "prepared_commit" => journal.fail = Some(ExecutionPhase::Prepared),
            "authorized_commit" => journal.fail = Some(ExecutionPhase::Authorized),
            _ => unreachable!(),
        }
        assert!(
            run(&req, &options(), &journal, &mut backend).is_err(),
            "{stage}"
        );
        assert!(
            !temp.path().join("marker").exists(),
            "user exec escaped barrier at {stage}"
        );
        if stage == "reserve" {
            assert!(!backend.prepared && !backend.released);
        } else {
            assert_eq!(journal.last().phase, ExecutionPhase::Released, "{stage}");
        }
        if matches!(
            stage,
            "verify" | "required" | "prepared_commit" | "authorized_commit"
        ) {
            assert!(
                backend.verified,
                "gate did not reach the intended failure stage {stage}"
            );
        }
        if stage == "prepare" {
            assert!(
                journal
                    .last()
                    .evidence
                    .iter()
                    .any(|e| e.control == "cpu.weight" && e.applied)
            );
        }
    }
}

#[test]
fn spawn_failure_and_identity_protocol_failure_do_not_authorize() {
    for executable in ["/cedegrid-test-missing-gate", "/bin/false"] {
        let temp = TempDir::new().unwrap();
        let req = request(temp.path());
        let journal = Journal::new();
        let mut backend = Backend::new();
        assert!(
            supervision::supervise(
                &req,
                &req.resources,
                &options(),
                &journal,
                &mut backend,
                Path::new(executable)
            )
            .is_err()
        );
        assert!(!temp.path().join("marker").exists());
        assert_eq!(journal.last().phase, ExecutionPhase::Released);
    }
}

#[test]
fn failed_release_is_reconciliation_and_retains_evidence() {
    let temp = TempDir::new().unwrap();
    let req = request(temp.path());
    let journal = Journal::new();
    let mut backend = Backend::new();
    backend.fail_release = true;
    assert!(run(&req, &options(), &journal, &mut backend).is_err());
    assert_eq!(journal.last().phase, ExecutionPhase::NeedsReconciliation);
    assert!(journal.last().identity.is_some());
    assert!(journal.last().detail.contains("capacity retained"));
}

#[test]
fn rootless_descendants_and_gpu_work_require_future_verified_support() {
    for unsupported in ["descendants", "escape", "gpu"] {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        match unsupported {
            "descendants" => req.single_process = false,
            "escape" => req.no_escape = false,
            "gpu" => {
                req.resources.gpu_memory_mib.insert("GPU-fixture".into(), 1);
            }
            _ => unreachable!(),
        }
        let journal = Journal::new();
        assert!(run(&req, &options(), &journal, &mut Backend::new()).is_err());
        assert!(journal.records.borrow().is_empty());
        assert!(!temp.path().join("marker").exists());
    }
}

#[test]
fn opportunistic_fixed_lease_terminates_owned_process_only() {
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.class = AllocationClass::Opportunistic;
    req.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "trap '' TERM; while :; do :; done".into(),
    ];
    // The unrelated same-user child is deliberately outside manager ownership.
    let mut unrelated = Command::new("/bin/sleep").arg("5").spawn().unwrap();
    let result = run(&req, &options(), &Journal::new(), &mut Backend::new());
    let unrelated_survived = unrelated.try_wait().unwrap().is_none();
    if unrelated_survived {
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }
    let outcome = result.unwrap();
    assert!(outcome.yielded);
    assert_eq!(outcome.termination_signal, Some(libc::SIGKILL));
    assert!(
        outcome.termination_started_ms.unwrap() >= options().lease_ms + options().drain_timeout_ms
    );
    assert!(
        outcome.process_exit_ms
            >= options().lease_ms + options().drain_timeout_ms + options().term_grace_ms
    );
    assert!(unrelated_survived);
}

#[test]
fn cooperative_drain_file_allows_exit_before_term() {
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.class = AllocationClass::Opportunistic;
    req.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "while [ ! -f \"$CEDEGRID_DRAIN_FILE\" ]; do :; done".into(),
    ];
    let opts = SupervisorOptions {
        drain_timeout_ms: 200,
        ..options()
    };
    let outcome = run(&req, &opts, &Journal::new(), &mut Backend::new()).unwrap();
    assert!(outcome.yielded);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(outcome.termination_started_ms.is_none());
}

#[test]
fn gate_eof_before_authorization_never_executes_workload() {
    let temp = TempDir::new().unwrap();
    let req = request(temp.path());
    let mut gate = Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .arg("__worker-gate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = gate.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({ "request": req, "generation": 7,
        "setup": {}, "drain_file": temp.path().join("drain") })
    )
    .unwrap();
    let mut identity = String::new();
    BufReader::new(gate.stdout.take().unwrap())
        .read_line(&mut identity)
        .unwrap();
    let id: ProcessIdentity = serde_json::from_str(&identity).unwrap();
    assert_eq!(id.pid, gate.id());
    drop(input); // exact pipe behavior when the supervisor exits before EXEC
    assert!(!gate.wait().unwrap().success());
    assert!(!temp.path().join("marker").exists());
}

/// Runs only in a helper process selected by supervisor_loss_before_authorization.
#[test]
fn supervisor_loss_fixture() {
    let Some(dir) = std::env::var_os("CEDEGRID_TEST_SUPERVISOR_LOSS_DIR") else {
        return;
    };
    let path = PathBuf::from(dir);
    let mut journal = Journal::new();
    journal.ready = Some(path.join("ready"));
    if std::env::var("CEDEGRID_TEST_PAUSE_PHASE").as_deref() == Ok("authorized") {
        journal.pause_phase = ExecutionPhase::Authorized;
    }
    let _ = run(&request(&path), &options(), &journal, &mut Backend::new());
}

#[test]
fn supervisor_loss_before_authorization_closes_gate_without_running_user_code() {
    for pause_phase in ["prepared", "authorized"] {
        let temp = TempDir::new().unwrap();
        let mut supervisor = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "supervisor_loss_fixture", "--nocapture"])
            .env("CEDEGRID_TEST_SUPERVISOR_LOSS_DIR", temp.path())
            .env("CEDEGRID_TEST_PAUSE_PHASE", pause_phase)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !temp.path().join("ready").exists() {
            if supervisor.try_wait().unwrap().is_some() {
                panic!("fixture exited before gate preparation");
            }
            if Instant::now() > deadline {
                let _ = supervisor.kill();
                let _ = supervisor.wait();
                panic!("fixture did not prepare its child");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!temp.path().join("marker").exists());
        supervisor.kill().unwrap();
        supervisor.wait().unwrap();
        // EOF is consumed by the orphaned trusted gate. Never signal its numeric PID.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!temp.path().join("marker").exists());
    }
}

#[test]
fn malformed_identity_and_unresponsive_gate_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    for body in [
        "#!/bin/sh\nread -r request\nprintf '%s\\n' '{\"pid\":1,\"boot_id\":\"fake\",\"start_time\":1,\"assignment_id\":\"assignment\",\"generation\":7}'\nread -r ignored\n",
        "#!/bin/sh\nread -r request\nwhile :; do :; done\n",
    ] {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("gate-fixture");
        fs::write(&executable, body).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let req = request(temp.path());
        let journal = Journal::new();
        let mut backend = Backend::new();
        let opts = SupervisorOptions {
            prepare_timeout_ms: 100,
            ..options()
        };
        assert!(
            supervision::supervise(
                &req,
                &req.resources,
                &opts,
                &journal,
                &mut backend,
                &executable
            )
            .is_err()
        );
        assert!(!backend.verified);
        assert_eq!(journal.last().phase, ExecutionPhase::Released);
        assert!(!temp.path().join("marker").exists());
    }
}

#[test]
fn incompatible_reaping_policy_fixture() {
    if std::env::var_os("CEDEGRID_TEST_IGNORE_SIGCHLD").is_none() {
        return;
    }
    // This exact test runs alone in a disposable helper process. Never alter
    // the parent test runner's process-wide reaping policy.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
    let temp = TempDir::new().unwrap();
    let journal = Journal::new();
    let error = run(
        &request(temp.path()),
        &options(),
        &journal,
        &mut Backend::new(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("SIGCHLD"));
    assert!(journal.records.borrow().is_empty());
}

#[test]
fn inherited_auto_reaping_is_rejected_before_reservation() {
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "incompatible_reaping_policy_fixture"])
        .env("CEDEGRID_TEST_IGNORE_SIGCHLD", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn required_nice_has_one_verified_application_record_before_launch() {
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.class = AllocationClass::Opportunistic;
    req.required_controls = vec!["cpu.nice".into()];
    let journal = Journal::new();
    let outcome = run(
        &req,
        &SupervisorOptions {
            lease_ms: 1_000,
            ..options()
        },
        &journal,
        &mut Backend::new(),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    let records = journal.records.borrow();
    let prepared = records
        .iter()
        .find(|r| r.phase == ExecutionPhase::Prepared)
        .unwrap();
    let nice: Vec<_> = prepared
        .evidence
        .iter()
        .filter(|e| e.control == "cpu.nice")
        .collect();
    assert_eq!(nice.len(), 1);
    assert!(nice[0].applied && !nice[0].fallback);
    assert_eq!(nice[0].effective.as_deref(), Some("10"));
}

#[test]
fn external_authorization_refusal_never_runs_user_code() {
    struct Refuse;
    impl supervision::SupervisorControl for Refuse {
        fn prepared(&mut self, _: &ExecutionRecord) -> Result<()> {
            bail!("authenticated grant unavailable")
        }
    }
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("must-not-execute");
    let mut req = request(dir.path());
    req.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("touch '{}'", marker.display()),
    ];
    let journal = Journal::new();
    let mut backend = Backend::new();
    assert!(
        supervision::supervise_controlled(
            &req,
            &req.resources,
            &SupervisorOptions::default(),
            &journal,
            &mut backend,
            Path::new(env!("CARGO_BIN_EXE_cedegrid")),
            &mut Refuse
        )
        .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(journal.last().phase, ExecutionPhase::Released);
}

#[test]
fn explicit_cpu_affinity_is_verified_or_refused_before_execution() {
    let dir = TempDir::new().unwrap();
    let mut request = request(dir.path());
    let journal = Journal::new();
    let mut backend = Backend::new();
    #[cfg(target_os = "linux")]
    let cpus = {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let ids = cedegrid::kernel::parse_cpu_list(
            status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        vec![ids[0]]
    };
    #[cfg(not(target_os = "linux"))]
    let cpus = vec![0u32];
    request.env.insert(
        "CEDEGRID_CPU_AFFINITY".into(),
        serde_json::to_string(&cpus).unwrap(),
    );
    request.required_controls.push("cpu.affinity".into());
    let result = run(&request, &options(), &journal, &mut backend);
    #[cfg(target_os = "linux")]
    {
        let outcome = result.unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            outcome
                .record
                .evidence
                .iter()
                .any(|e| e.control == "cpu.affinity" && e.applied && !e.fallback)
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert!(result.is_err());
        assert!(!dir.path().join("marker").exists());
        assert_eq!(journal.last().phase, ExecutionPhase::Released);
    }
}

fn run_controlled(
    request: &LaunchRequest,
    options: &SupervisorOptions,
    journal: &Journal,
    backend: &mut Backend,
    control: &mut dyn supervision::SupervisorControl,
) -> Result<supervision::ExecutionOutcome> {
    supervision::supervise_controlled(
        request,
        &Resources {
            cpu_millicores: 1_000,
            ram_mib: 1_000,
            ..Default::default()
        },
        options,
        journal,
        backend,
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        control,
    )
}

fn short_marker_job(dir: &Path) -> Vec<String> {
    let _ = dir;
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "sleep 0.3; printf done > marker".into(),
    ]
}

#[test]
fn a_control_channel_error_after_exec_holds_the_step() {
    struct Broken;
    impl supervision::SupervisorControl for Broken {
        fn poll(&mut self) -> Result<Vec<supervision::SupervisorCommand>> {
            bail!("injected control channel failure")
        }
    }
    for class in [AllocationClass::Guaranteed, AllocationClass::Opportunistic] {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        req.class = class;
        req.argv = short_marker_job(temp.path());
        // The lease outlives the job, so only the control error could stop it.
        let opts = SupervisorOptions {
            lease_ms: 10_000,
            ..options()
        };
        let outcome = run_controlled(
            &req,
            &opts,
            &Journal::new(),
            &mut Backend::new(),
            &mut Broken,
        )
        .unwrap();
        assert_eq!(outcome.exit_code, Some(0), "{class:?}");
        assert!(!outcome.yielded && outcome.termination_signal.is_none());
        assert!(temp.path().join("marker").exists());
    }
}

#[test]
fn a_rejected_journal_transition_is_retried_and_never_kills_guaranteed_work() {
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.argv = short_marker_job(temp.path());
    let journal = Journal {
        fail: Some(ExecutionPhase::Running),
        ..Journal::new()
    };
    let outcome = run(&req, &options(), &journal, &mut Backend::new()).unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.yielded && outcome.termination_signal.is_none());
    assert!(temp.path().join("marker").exists());
    assert_eq!(journal.last().phase, ExecutionPhase::Released);
    assert!(
        !journal
            .records
            .borrow()
            .iter()
            .any(|r| r.phase == ExecutionPhase::Running)
    );
}

#[test]
fn opportunistic_work_with_a_failing_journal_goes_through_drain_term_and_kill() {
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.class = AllocationClass::Opportunistic;
    req.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "trap '' TERM; while :; do :; done".into(),
    ];
    let journal = Journal {
        fail: Some(ExecutionPhase::Running),
        ..Journal::new()
    };
    let opts = SupervisorOptions {
        lease_ms: 10_000,
        drain_timeout_ms: 50,
        term_grace_ms: 50,
        ..options()
    };
    let outcome = run(&req, &opts, &journal, &mut Backend::new()).unwrap();
    assert!(outcome.yielded);
    assert_eq!(outcome.termination_signal, Some(libc::SIGKILL));
    let drain = outcome.drain_started_ms.unwrap();
    let term = outcome.termination_started_ms.unwrap();
    assert!(term >= drain + opts.drain_timeout_ms, "{outcome:?}");
    assert!(
        outcome.process_exit_ms >= term + opts.term_grace_ms,
        "{outcome:?}"
    );
    assert_eq!(journal.last().phase, ExecutionPhase::Released);
}

#[test]
fn kill_waits_a_minimum_term_grace_after_the_first_sigterm_actually_sent() {
    struct Stalled(u32);
    impl supervision::SupervisorControl for Stalled {
        fn poll(&mut self) -> Result<Vec<supervision::SupervisorCommand>> {
            self.0 += 1;
            match self.0 {
                1 => Ok(vec![supervision::SupervisorCommand::Drain]),
                // The loop stalls past both the drain and TERM deadlines.
                2 => {
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(vec![])
                }
                _ => Ok(vec![]),
            }
        }
    }
    let temp = TempDir::new().unwrap();
    let mut req = request(temp.path());
    req.class = AllocationClass::Opportunistic;
    req.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "trap '' TERM; while :; do :; done".into(),
    ];
    let opts = SupervisorOptions {
        lease_ms: 10_000,
        drain_timeout_ms: 20,
        term_grace_ms: 100,
        ..options()
    };
    let outcome = run_controlled(
        &req,
        &opts,
        &Journal::new(),
        &mut Backend::new(),
        &mut Stalled(0),
    )
    .unwrap();
    assert_eq!(outcome.termination_signal, Some(libc::SIGKILL));
    let anchor = outcome.termination_deadline_ms.unwrap();
    let sent = outcome.termination_started_ms.unwrap();
    // Both are recorded: the deadline anchor and the late actual send.
    assert!(sent >= anchor + 300, "{outcome:?}");
    assert!(
        outcome.process_exit_ms >= sent + opts.term_grace_ms,
        "{outcome:?}"
    );
}

mod actuation {
    use super::*;
    use cedegrid::contention::{ResourceKey, StepRequest, YieldCause, YieldStep};
    use std::collections::VecDeque;
    use supervision::{SupervisorCommand, SupervisorReport};

    fn python() -> String {
        std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into())
    }

    /// A single-process worker that bumps a counter file until a `stop` file
    /// appears, so time spent paused does not end it. Writes are atomic renames.
    fn counter_worker(busy: bool) -> Vec<String> {
        let body = if busy {
            "import os, time\nend=time.monotonic()+60\ni=0\nwhile time.monotonic()<end:\n    i+=1\n    if i%20000==0:\n        open('counter.tmp','w').write(str(i))\n        os.replace('counter.tmp','counter')\n        if os.path.exists('stop'): break\n"
        } else {
            "import os, time\nend=time.monotonic()+60\ni=0\nwhile time.monotonic()<end and not os.path.exists('stop'):\n    i+=1\n    open('counter.tmp','w').write(str(i))\n    os.replace('counter.tmp','counter')\n    time.sleep(0.02)\n"
        };
        vec![python(), "-c".into(), body.into()]
    }

    fn read_counter(path: &Path) -> u64 {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn adjust(sequence: u64, step: YieldStep, target: Option<u64>) -> SupervisorCommand {
        SupervisorCommand::Adjust {
            sequence,
            resources: BTreeMap::from([(
                ResourceKey::Cpu,
                StepRequest {
                    step,
                    target,
                    cause: YieldCause::ExternalGrowth,
                    deadline_ms: None,
                },
            )]),
        }
    }

    /// What phases observe: elapsed time, every report and named counter samples.
    #[derive(Default)]
    struct Seen {
        now_ms: u64,
        reports: Vec<SupervisorReport>,
        adjusted_ms: BTreeMap<u64, u64>,
        marks: BTreeMap<&'static str, (u64, u64)>,
    }
    impl Seen {
        /// Reads a counter and keeps it, with the time, under `name`.
        fn record(&mut self, name: &'static str, path: &Path) -> u64 {
            let value = read_counter(path);
            self.marks.insert(name, (self.now_ms, value));
            value
        }
        fn mark(&self, name: &str) -> u64 {
            self.marks[name].1
        }
        fn since(&self, name: &str) -> u64 {
            self.now_ms - self.marks[name].0
        }
        fn adjusted(&self, sequence: u64) -> bool {
            self.adjusted_ms.contains_key(&sequence)
        }
        fn since_adjusted(&self, sequence: u64) -> u64 {
            self.now_ms - self.adjusted_ms[&sequence]
        }
        fn rate(&self, from: &str, to: &str) -> f64 {
            let ((t0, v0), (t1, v1)) = (self.marks[from], self.marks[to]);
            v1.saturating_sub(v0) as f64 / t1.saturating_sub(t0).max(1) as f64
        }
    }

    /// Polled until it returns the commands to send; then the next phase starts.
    type Phase = Box<dyn FnMut(&mut Seen) -> Option<Vec<SupervisorCommand>>>;

    fn wait(mut until: impl FnMut(&mut Seen) -> bool + 'static) -> Phase {
        Box::new(move |seen| until(seen).then(Vec::new))
    }

    fn send(command: SupervisorCommand) -> Phase {
        let mut command = Some(command);
        Box::new(move |_| command.take().map(|command| vec![command]))
    }

    fn running(path: &Path) -> Phase {
        let path = path.to_path_buf();
        wait(move |_| read_counter(&path) > 0)
    }

    fn record(counters: &[(&'static str, &Path)]) -> Phase {
        let counters: Vec<_> = counters
            .iter()
            .map(|(name, path)| (*name, path.to_path_buf()))
            .collect();
        wait(move |seen| {
            for (name, path) in &counters {
                seen.record(name, path);
            }
            true
        })
    }

    /// Waits until the counter is `by` past the `from` mark and keeps it as `name`.
    fn advances(name: &'static str, path: &Path, from: &'static str, by: u64) -> Phase {
        let path = path.to_path_buf();
        wait(move |seen| seen.record(name, &path) >= seen.mark(from) + by)
    }

    /// Waits `ms` past the `from` mark and keeps the counter as `name`.
    fn after(name: &'static str, path: &Path, from: &'static str, ms: u64) -> Phase {
        let path = path.to_path_buf();
        wait(move |seen| seen.since(from) >= ms && seen.record(name, &path) > 0)
    }

    /// Waits until the counter has not changed for `quiet_ms`.
    fn settled(path: &Path, quiet_ms: u64) -> Phase {
        let path = path.to_path_buf();
        let mut last: Option<(u64, u64)> = None;
        wait(move |seen| {
            let value = read_counter(&path);
            match last {
                Some((previous, since)) if previous == value => seen.now_ms - since >= quiet_ms,
                _ => {
                    last = Some((value, seen.now_ms));
                    false
                }
            }
        })
    }

    const PHASE_DEADLINE_MS: u64 = 20_000;

    /// Drives the supervisor through phases that wait for observed conditions,
    /// each within `PHASE_DEADLINE_MS`. When the phases end or one times out, it
    /// resumes the worker and creates the `stop` file that ends it.
    struct Script {
        started: Instant,
        phase_started_ms: u64,
        phases: VecDeque<Phase>,
        completed: usize,
        stop: PathBuf,
        stopped: bool,
        failure: Option<String>,
        seen: Seen,
    }
    impl Script {
        fn new(dir: &Path, phases: Vec<Phase>) -> Self {
            Self {
                started: Instant::now(),
                phase_started_ms: 0,
                phases: phases.into(),
                completed: 0,
                stop: dir.join("stop"),
                stopped: false,
                failure: None,
                seen: Seen::default(),
            }
        }

        fn finish(&mut self) -> Vec<SupervisorCommand> {
            self.stopped = true;
            if let Err(error) = fs::write(&self.stop, b"") {
                self.failure.get_or_insert(format!("stop file: {error}"));
            }
            vec![adjust(1_000_000, YieldStep::None, None)]
        }

        fn check(&self) {
            if let Some(failure) = &self.failure {
                panic!("{failure}");
            }
            assert!(
                self.phases.is_empty(),
                "the worker ended in phase {}: {:?}",
                self.completed,
                self.seen.reports
            );
        }
    }
    impl supervision::SupervisorControl for Script {
        fn poll(&mut self) -> Result<Vec<SupervisorCommand>> {
            self.seen.now_ms = self.started.elapsed().as_millis() as u64;
            if self.stopped {
                return Ok(vec![]);
            }
            let Some(phase) = self.phases.front_mut() else {
                return Ok(self.finish());
            };
            if let Some(commands) = phase(&mut self.seen) {
                self.phases.pop_front();
                self.completed += 1;
                self.phase_started_ms = self.seen.now_ms;
                return Ok(commands);
            }
            if self.seen.now_ms - self.phase_started_ms > PHASE_DEADLINE_MS {
                self.failure = Some(format!(
                    "phase {} timed out: {:?}",
                    self.completed, self.seen.reports
                ));
                return Ok(self.finish());
            }
            Ok(vec![])
        }
        fn report(&mut self, report: SupervisorReport) -> Result<()> {
            if let SupervisorReport::Adjusted { sequence, .. } = &report {
                let now = self.started.elapsed().as_millis() as u64;
                self.seen.adjusted_ms.insert(*sequence, now);
            }
            self.seen.reports.push(report);
            Ok(())
        }
    }

    fn adjusted(
        reports: &[SupervisorReport],
        sequence: u64,
    ) -> (
        BTreeMap<ResourceKey, YieldStep>,
        BTreeMap<ResourceKey, Vec<YieldStep>>,
    ) {
        reports
            .iter()
            .find_map(|r| match r {
                SupervisorReport::Adjusted {
                    sequence: s,
                    applied,
                    unavailable,
                    ..
                } if *s == sequence => Some((applied.clone(), unavailable.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no adjusted report for {sequence}: {reports:?}"))
    }

    fn long_lease() -> SupervisorOptions {
        SupervisorOptions {
            lease_ms: 30_000,
            duty_period_ms: 100,
            hint_deadline_ms: 5_000,
            ..options()
        }
    }

    #[test]
    fn pause_stops_only_the_owned_worker_and_resume_continues_it() {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        req.class = AllocationClass::Opportunistic;
        req.argv = counter_worker(false);
        // A same-user process outside manager ownership must never be touched.
        let unrelated_dir = TempDir::new().unwrap();
        let mut unrelated = Command::new(python())
            .args(["-c", "import os, time\ni=0\nwhile True:\n    i+=1\n    open('counter.tmp','w').write(str(i))\n    os.replace('counter.tmp','counter')\n    time.sleep(0.02)\n"])
            .current_dir(unrelated_dir.path())
            .spawn()
            .unwrap();
        let mine = temp.path().join("counter");
        let theirs = unrelated_dir.path().join("counter");
        let mut script = Script::new(
            temp.path(),
            vec![
                running(&mine),
                running(&theirs),
                send(adjust(1, YieldStep::Pause, None)),
                wait(|seen| seen.adjusted(1)),
                // A write in flight when the pause lands may still complete.
                settled(&mine, 200),
                record(&[("mine_paused", &mine), ("theirs_paused", &theirs)]),
                advances("theirs_later", &theirs, "theirs_paused", 10),
                record(&[("mine_later", &mine)]),
                send(adjust(2, YieldStep::None, None)),
                wait(|seen| seen.adjusted(2)),
                advances("mine_resumed", &mine, "mine_later", 1),
            ],
        );
        let result = run_controlled(
            &req,
            &long_lease(),
            &Journal::new(),
            &mut Backend::new(),
            &mut script,
        );
        let unrelated_alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        let outcome = result.unwrap();
        script.check();
        assert!(unrelated_alive);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.yielded);
        let (applied, _) = adjusted(&script.seen.reports, 1);
        assert_eq!(applied[&ResourceKey::Cpu], YieldStep::Pause);
        let (applied, _) = adjusted(&script.seen.reports, 2);
        assert_eq!(applied[&ResourceKey::Cpu], YieldStep::None);
        let mark = |name| script.seen.mark(name);
        assert_eq!(
            mark("mine_paused"),
            mark("mine_later"),
            "the paused worker must not progress"
        );
        assert!(
            mark("theirs_later") > mark("theirs_paused"),
            "an unrelated process is never paused"
        );
        assert!(
            mark("mine_resumed") > mark("mine_later"),
            "resume continues the worker"
        );
    }

    #[test]
    fn pause_is_refused_for_guaranteed_work_and_unavailable_steps_are_reported() {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        req.argv = counter_worker(false);
        let counter = temp.path().join("counter");
        let mut script = Script::new(
            temp.path(),
            vec![
                running(&counter),
                send(adjust(1, YieldStep::Pause, None)),
                wait(|seen| seen.adjusted(1)),
                send(SupervisorCommand::Adjust {
                    sequence: 2,
                    resources: BTreeMap::from([
                        (
                            ResourceKey::Cpu,
                            StepRequest {
                                step: YieldStep::LowerShare,
                                target: None,
                                cause: YieldCause::ExternalGrowth,
                                deadline_ms: None,
                            },
                        ),
                        (
                            ResourceKey::Ram,
                            StepRequest {
                                step: YieldStep::Hint,
                                target: Some(1),
                                cause: YieldCause::ExternalGrowth,
                                deadline_ms: None,
                            },
                        ),
                    ]),
                }),
                wait(|seen| seen.adjusted(2)),
                record(&[("refused", &counter)]),
                advances("later", &counter, "refused", 10),
            ],
        );
        let outcome = run_controlled(
            &req,
            &long_lease(),
            &Journal::new(),
            &mut Backend::new(),
            &mut script,
        )
        .unwrap();
        script.check();
        assert_eq!(outcome.exit_code, Some(0));
        let (applied, unavailable) = adjusted(&script.seen.reports, 1);
        assert_eq!(applied.get(&ResourceKey::Cpu), Some(&YieldStep::None));
        assert_eq!(unavailable[&ResourceKey::Cpu], [YieldStep::Pause]);
        let (_, unavailable) = adjusted(&script.seen.reports, 2);
        assert_eq!(unavailable[&ResourceKey::Cpu], [YieldStep::LowerShare]);
        assert_eq!(unavailable[&ResourceKey::Ram], [YieldStep::Hint]);
        assert!(
            script.seen.mark("later") > script.seen.mark("refused"),
            "Guaranteed work kept running"
        );
    }

    #[test]
    fn a_cpu_cap_throttles_a_busy_worker_by_duty_cycle_and_reverses() {
        let temp = TempDir::new().unwrap();
        let mut req = request(temp.path());
        req.class = AllocationClass::Opportunistic;
        req.resources.cpu_millicores = 1000;
        req.argv = counter_worker(true);
        let counter = temp.path().join("counter");
        let mut script = Script::new(
            temp.path(),
            vec![
                running(&counter),
                record(&[("free0", &counter)]),
                after("free1", &counter, "free0", 1000),
                send(adjust(1, YieldStep::Cap, Some(250))),
                wait(|seen| seen.adjusted(1)),
                // Two duty periods let the cap take hold.
                wait(|seen| seen.since_adjusted(1) >= 200),
                record(&[("capped0", &counter)]),
                after("capped1", &counter, "capped0", 1000),
                send(adjust(2, YieldStep::None, None)),
                wait(|seen| seen.adjusted(2)),
                record(&[("resumed0", &counter)]),
                after("resumed1", &counter, "resumed0", 1000),
            ],
        );
        let outcome = run_controlled(
            &req,
            &long_lease(),
            &Journal::new(),
            &mut Backend::new(),
            &mut script,
        )
        .unwrap();
        script.check();
        assert_eq!(outcome.exit_code, Some(0));
        let (applied, _) = adjusted(&script.seen.reports, 1);
        assert_eq!(applied[&ResourceKey::Cpu], YieldStep::Cap);
        let free = script.seen.rate("free0", "free1");
        let capped = script.seen.rate("capped0", "capped1");
        let resumed = script.seen.rate("resumed0", "resumed1");
        assert!(capped < 0.6 * free, "free {free} capped {capped}");
        assert!(resumed > 0.7 * free, "free {free} resumed {resumed}");
    }
}

mod control_socket {
    use cedegrid::supervision::control_socket::{Line, Listener, connect, token};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    fn next(rx: &mpsc::Receiver<Line>) -> Line {
        rx.recv_timeout(Duration::from_secs(3)).unwrap()
    }

    fn read_line(stream: &UnixStream) -> Option<String> {
        // macOS refuses socket options once the peer has closed the connection.
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        let mut line = String::new();
        let read = BufReader::new(stream).read_line(&mut line).ok()?;
        (read > 0).then(|| line.trim_end().to_owned())
    }

    #[test]
    fn only_the_token_holder_controls_and_a_newer_controller_replaces_the_older() {
        let token = token().unwrap();
        assert_eq!(token.len(), 64, "32 random bytes, hex");
        let listener = Listener::bind(token.clone()).unwrap();
        let path = listener.path().to_owned();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let (tx, rx) = mpsc::channel();
        let sink = listener.serve(move |line| {
            let _ = tx.send(line);
        });

        // A wrong token and a connection without hello are both refused.
        assert!(connect(&path, &"0".repeat(64)).is_err());
        let mut stranger = UnixStream::connect(&path).unwrap();
        writeln!(stranger, "{{\"kind\":\"adjust\"}}").unwrap();
        assert_eq!(read_line(&stranger), None, "closed without authority");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

        let mut first = connect(&path, &token).unwrap();
        assert_eq!(next(&rx), Line::Attached);
        writeln!(first, "from-first").unwrap();
        assert_eq!(next(&rx), Line::Text("from-first".into()));
        assert!(sink.write_line("to-first"));
        assert_eq!(read_line(&first).as_deref(), Some("to-first"));

        // A newer authenticated controller replaces the older one.
        let mut second = connect(&path, &token).unwrap();
        assert_eq!(next(&rx), Line::Attached);
        assert_eq!(read_line(&first), None, "the replaced controller is closed");
        writeln!(second, "from-second").unwrap();
        assert_eq!(next(&rx), Line::Text("from-second".into()));
        assert!(sink.write_line("to-second"));
        assert_eq!(read_line(&second).as_deref(), Some("to-second"));
        drop(second);
        assert_eq!(next(&rx), Line::Detached);
        assert!(!sink.write_line("nobody"));
    }
}

/// Own work is marked as the preferred OOM victim before EXEC (FR-023), except
/// Guaranteed work under floor policy C (FR-041).
#[cfg(target_os = "linux")]
#[test]
fn the_gate_marks_own_work_as_the_preferred_oom_victim_before_exec() {
    // Every own process is a preferred victim; no option exempts Guaranteed work (FR-023).
    for (class, expected) in [
        (AllocationClass::Opportunistic, "1000"),
        (AllocationClass::Guaranteed, "1000"),
    ] {
        let dir = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let mut request = request(dir.path());
        request.class = class;
        request.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat /proc/self/oom_score_adj > adj".into(),
        ];
        let outcome = run(&request, &options(), &Journal::new(), &mut Backend::new()).unwrap();
        assert_eq!(outcome.exit_code, Some(0), "{class:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("adj")).unwrap().trim(),
            expected,
            "{class:?}"
        );
        let claimed = outcome
            .record
            .evidence
            .iter()
            .any(|e| e.control == "memory.oom_score_adj" && e.applied);
        assert_eq!(claimed, expected == "1000", "{class:?}");
    }
}

/// Platforms where an unprivileged process cannot mark itself claim nothing.
#[cfg(not(target_os = "linux"))]
#[test]
fn no_oom_preference_is_claimed_where_the_platform_cannot_apply_it() {
    let dir = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
    let outcome = run(
        &request(dir.path()),
        &options(),
        &Journal::new(),
        &mut Backend::new(),
    )
    .unwrap();
    assert!(
        !outcome
            .record
            .evidence
            .iter()
            .any(|e| e.control == "memory.oom_score_adj")
    );
}
