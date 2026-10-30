//! Baseline backend; process ownership is verified by the dedicated supervisor.
use crate::execution_model::*;
use anyhow::{Result, ensure};

#[derive(Default)]
pub struct RootlessBackend {
    pub nice: i32,
    pub fallback_evidence: Vec<ControlEvidence>,
}
impl RootlessBackend {
    pub fn new(nice: i32) -> Self {
        Self {
            nice,
            ..Self::default()
        }
    }
}
/// The rootless launch contract. On Linux a run's descendants join its verified family, so
/// multi-process work needs no contract; elsewhere a run is one process or has 1 to 8 SDK
/// children (FR-084). SDK children always need a multi-process request.
pub fn contract_holds(request: &LaunchRequest) -> bool {
    let sdk = !request.single_process && (1..=8).contains(&request.managed_child_limit);
    let alone =
        request.managed_child_limit == 0 && (request.single_process || cfg!(target_os = "linux"));
    request.no_escape && (alone || sdk)
}

impl LaunchBackend for RootlessBackend {
    fn name(&self) -> &str {
        "rootless"
    }
    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup> {
        ensure!(
            contract_holds(request),
            "rootless execution requires single-process or bounded supervisor-mediated children with no-escape contract"
        );
        // The supervisor applies the launch nice, which depends on the task's mode.
        Ok(GateSetup {
            cgroup_path: None,
            nice: None,
            oom_score_adj: None,
            nofile_soft: None,
            parent_death_signal: None,
        })
    }
    fn verify(&mut self, _identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>> {
        // The supervisor owns the unreaped child and verifies GateSetup.nice;
        // this backend must not duplicate numeric-PID inspection or evidence.
        Ok(self.fallback_evidence.clone())
    }
    fn confirm_release(&mut self) -> Result<()> {
        Ok(())
    }
}
