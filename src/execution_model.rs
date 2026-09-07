//! Local execution contracts. Transport and coordinator implementations stay separate.
use crate::model::Resources;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationClass {
    Guaranteed,
    Opportunistic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedArtifact {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub task_id: String,
    pub assignment_id: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub resources: Resources,
    #[serde(default)]
    pub replay_safe: bool,
    pub class: AllocationClass,
    /// Explicit acknowledgement of the rootless descendant lifecycle contract.
    #[serde(default)]
    pub no_escape: bool,
    /// Rootless v1 execution can prove cleanup only for a single process.
    #[serde(default)]
    pub single_process: bool,
    /// Explicit opt-in to bounded supervisor-mediated children; zero preserves the baseline contract.
    #[serde(default)]
    pub managed_child_limit: u16,
    /// Optional immutable bound on all top-level attempt reservations, including yields.
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Immutable, published input content downloaded and verified before preparation.
    #[serde(default)]
    pub input_artifacts: Vec<NamedArtifact>,
    #[serde(default)]
    pub required_controls: Vec<String>,
    #[serde(default)]
    pub allow_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub boot_id: String,
    /// OS-native process start marker, not a rounded wall-clock estimate.
    pub start_time: u64,
    pub assignment_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Reserved,
    Prepared,
    Authorized,
    Running,
    Draining,
    NeedsReconciliation,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub task_id: String,
    pub assignment_id: String,
    pub generation: u64,
    pub class: AllocationClass,
    pub phase: ExecutionPhase,
    pub resources: Resources,
    pub identity: Option<ProcessIdentity>,
    pub backend: String,
    pub evidence: Vec<ControlEvidence>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlEvidence {
    pub control: String,
    pub available: Option<bool>,
    pub permitted: Option<bool>,
    pub configured: bool,
    pub applied: bool,
    pub fallback: bool,
    pub scope: String,
    pub requested: Option<String>,
    pub effective: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateSetup {
    /// Manager-created workload leaf; gate joins itself before any user exec.
    pub cgroup_path: Option<PathBuf>,
    pub nice: Option<i32>,
}

pub trait ExecutionJournal {
    /// Verified journal assurance, supplied by the opened store at the launch barrier.
    fn storage_control_evidence(&self) -> Result<Vec<ControlEvidence>> {
        Ok(Vec::new())
    }
    /// Atomically reserve capacity and assign the next attempt; uncertain capacity stays charged.
    fn reserve(&self, request: &LaunchRequest, capacity: &Resources) -> Result<ExecutionRecord>;
    /// Must commit before authorizing user code. Transitions validate assignment/generation.
    fn transition(&self, record: &ExecutionRecord) -> Result<()>;
    fn reserve_managed_child(
        &self,
        _assignment_id: &str,
        _generation: u64,
        _child_id: &str,
        _request_id: &str,
        _request: &LaunchRequest,
    ) -> Result<crate::managed_children::ManagedChildRecord> {
        anyhow::bail!("execution journal does not support mediated children")
    }
    fn managed_child(
        &self,
        _assignment_id: &str,
        _request_id: &str,
    ) -> Result<Option<crate::managed_children::ManagedChildRecord>> {
        anyhow::bail!("execution journal does not support mediated children")
    }
    fn managed_children(
        &self,
        _assignment_id: &str,
    ) -> Result<Vec<crate::managed_children::ManagedChildRecord>> {
        anyhow::bail!("execution journal does not support mediated children")
    }
    fn prepare_managed_child(
        &self,
        _child_id: &str,
        _identity: &ProcessIdentity,
        _evidence: &[ControlEvidence],
    ) -> Result<()> {
        anyhow::bail!("execution journal does not support mediated children")
    }
    fn transition_managed_child(
        &self,
        _child_id: &str,
        _phase: crate::managed_children::ManagedChildPhase,
        _exit_code: Option<i32>,
        _signal: Option<i32>,
        _detail: &str,
    ) -> Result<()> {
        anyhow::bail!("execution journal does not support mediated children")
    }
}

/// A small, statically implemented backend boundary, not a dynamic plugin framework.
pub trait LaunchBackend {
    fn name(&self) -> &str;
    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup>;
    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        Vec::new()
    }
    fn verify(&mut self, identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>>;
    /// Only clean a manager-owned empty leaf; error keeps capacity reserved.
    fn confirm_release(&mut self) -> Result<()>;
}
