//! Explicit operator-owned GPU runtime test; never runs in the default suite.
#![cfg(target_os = "linux")]
use anyhow::Result;
use cedegrid::{
    execution_model::*,
    model::Resources,
    rootless::RootlessBackend,
    state::StateStore,
    supervision::{self, SupervisorCommand, SupervisorControl, SupervisorOptions},
};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

struct GpuReadyDrain {
    marker: PathBuf,
    uuid: String,
    pid: Option<u32>,
    observed: bool,
    start: Instant,
}
impl SupervisorControl for GpuReadyDrain {
    fn prepared(&mut self, record: &ExecutionRecord) -> Result<()> {
        self.pid = record.identity.as_ref().map(|p| p.pid);
        Ok(())
    }
    fn poll(&mut self) -> Result<Vec<SupervisorCommand>> {
        if self.marker.exists() && !self.observed {
            let nvml = nvml_wrapper::Nvml::init()?;
            let device = nvml.device_by_uuid(self.uuid.as_str())?;
            self.observed = device.running_compute_processes()?.iter().any(|p| {
                Some(p.pid) == self.pid
                    && matches!(p.used_gpu_memory,
                    nvml_wrapper::enums::device::UsedGpuMemory::Used(bytes) if bytes > 0)
            });
        }
        if self.observed || self.start.elapsed().as_secs() >= 20 {
            Ok(vec![SupervisorCommand::Drain])
        } else {
            Ok(vec![])
        }
    }
}

#[test]
#[ignore = "requires approved Linux GPU UUID, Python CUDA environment and bounded runtime authorization"]
fn owned_cuda_context_is_observed_then_released_after_drain() {
    let uuid = std::env::var("CEDEGRID_TEST_GPU_UUID").expect("explicit approved GPU UUID");
    let python = std::env::var("CEDEGRID_TEST_PYTHON").expect("qualified isolated Python");
    let evidence = std::env::var_os("CEDEGRID_GPU_EVIDENCE")
        .map(PathBuf::from)
        .expect("retained evidence path required");
    let home = PathBuf::from(std::env::var("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(
        evidence
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .starts_with(home)
    );
    assert!(
        !evidence.exists(),
        "refuse overwriting an existing evidence file"
    );
    let dir = tempfile::Builder::new()
        .prefix("gpu-native-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let marker = dir.path().join("cuda-ready");
    let script = "import os,pathlib,time,torch; assert torch.cuda.is_available(); torch.set_num_threads(1); x=torch.ones(8*1024*1024,device='cuda'); torch.cuda.synchronize(); pathlib.Path(os.environ['READY']).write_text(str(x.sum().item())); time.sleep(30)";
    let request = LaunchRequest {
        task_id: "gpu-native-task".into(),
        assignment_id: "gpu-native-assignment".into(),
        argv: vec![python, "-c".into(), script.into()],
        cwd: dir.path().to_path_buf(),
        env: BTreeMap::from([
            ("CUDA_VISIBLE_DEVICES".into(), uuid.clone()),
            ("READY".into(), marker.display().to_string()),
            ("TMPDIR".into(), dir.path().display().to_string()),
        ]),
        resources: Resources {
            cpu_millicores: 1000,
            ram_mib: 4096,
            gpu_memory_mib: BTreeMap::from([(uuid.clone(), 1024)]),
        },
        replay_safe: true,
        class: AllocationClass::Opportunistic,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec!["process_handle".into()],
        allow_fallback: false,
    };
    let capacity = request.resources.clone();
    let store = StateStore::open(&dir.path().join("state")).unwrap();
    let mut backend = RootlessBackend::new(10);
    let options = SupervisorOptions {
        nice: 10,
        drain_timeout_ms: 3000,
        term_grace_ms: 2000,
        lease_ms: 30000,
        prepare_timeout_ms: 10000,
        release_confirm_timeout_ms: 10000,
    };
    let mut control = GpuReadyDrain {
        marker,
        uuid,
        pid: None,
        observed: false,
        start: Instant::now(),
    };
    let outcome = supervision::supervise_controlled(
        &request,
        &capacity,
        &options,
        &store,
        &mut backend,
        &PathBuf::from(env!("CARGO_BIN_EXE_cedegrid")),
        &mut control,
    )
    .unwrap();
    assert!(
        control.observed,
        "actual managed CUDA memory was never observed"
    );
    assert_eq!(outcome.record.phase, ExecutionPhase::Released);
    assert!(
        outcome
            .record
            .evidence
            .iter()
            .any(|e| e.control == "process_handle" && e.applied && !e.fallback)
    );
    fs::write(
        evidence,
        serde_json::to_vec_pretty(
            &serde_json::json!({"actual_cuda_context_observed":true,"outcome":outcome}),
        )
        .unwrap(),
    )
    .unwrap();
}
