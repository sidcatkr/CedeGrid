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
