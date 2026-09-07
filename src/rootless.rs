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
impl LaunchBackend for RootlessBackend {
    fn name(&self) -> &str {
        "rootless"
    }
    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup> {
        ensure!(
            request.no_escape
                && ((request.single_process && request.managed_child_limit == 0)
                    || (!request.single_process && (1..=8).contains(&request.managed_child_limit))),
            "rootless execution requires single-process or bounded supervisor-mediated children with no-escape contract"
        );
        let apply_nice = request.class == AllocationClass::Opportunistic;
        Ok(GateSetup {
            cgroup_path: None,
            nice: apply_nice.then_some(self.nice),
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
