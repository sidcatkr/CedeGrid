#![cfg(unix)]
use cedegrid::{
    execution_model::*,
    managed_children::ManagedChildPhase,
    model::Resources,
    rootless::RootlessBackend,
    state::StateStore,
    supervision::{self, SupervisorOptions},
};
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
#[test]
fn family_termination_deadline_is_shared_and_unrelated_same_uid_child_survives() {
    let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut unrelated = Owned(
        Command::new(&python)
            .args(["-c", "import time;time.sleep(30)"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let code = r#"
import sys,time
from pathlib import Path
from cedegrid import spawn_managed
for index in range(8):
 code="import signal,time;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);Path('ready-%d').write_text('ready');time.sleep(30)"%index
 spawn_managed([sys.executable,'-c',code],single_process=True,no_escape=True,request_id='child-%d'%index)
while len(list(Path('.').glob('ready-*')))!=8: time.sleep(.01)
Path('leader-exit-time').write_text(str(time.monotonic()))
"#;
    let request = LaunchRequest {
        task_id: "family-deadline".into(),
        assignment_id: "family-deadline-attempt".into(),
        argv: vec![python, "-c".into(), code.into()],
        cwd: dir.path().into(),
        env: BTreeMap::from([
            (
                "PYTHONPATH".into(),
                format!("{}/python", env!("CARGO_MANIFEST_DIR")),
            ),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ]),
        resources: Resources {
            cpu_millicores: 1000,
            ram_mib: 256,
            gpu_memory_mib: BTreeMap::new(),
        },
        replay_safe: true,
        class: AllocationClass::Guaranteed,
        no_escape: true,
        single_process: false,
        managed_child_limit: 8,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    };
    let options = SupervisorOptions {
        nice: 10,
        drain_timeout_ms: 50,
        term_grace_ms: 500,
        lease_ms: 10000,
        prepare_timeout_ms: 3000,
        release_confirm_timeout_ms: 1000,
    };
    let start = Instant::now();
    let outcome = supervision::supervise(
        &request,
        &request.resources,
        &options,
        &store,
        &mut RootlessBackend::new(10),
        Path::new(env!("CARGO_BIN_EXE_cedegrid")),
    )
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "termination grace multiplied by child count: {:?}",
        start.elapsed()
    );
    let children = store.managed_children(&request.assignment_id).unwrap();
    assert_eq!(children.len(), 8);
    assert!(
        children
            .iter()
            .all(|c| c.phase == ManagedChildPhase::Released && c.signal == Some(libc::SIGKILL))
    );
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert!(store.execution_allocations().unwrap().is_empty());
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "manager affected unrelated same-user child"
    );
}
