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
    /// Per-job modes (FR-030); sent only to agents that negotiated `contention/1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contention: Option<JobContention>,
}

/// A job's requested mode per resource; the node cap still applies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobContention {
    pub modes: BTreeMap<crate::contention::ResourceKind, crate::contention::ContentionMode>,
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
    /// The job's requested modes, kept with the record so the policy sees them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modes: BTreeMap<crate::contention::ResourceKind, crate::contention::ContentionMode>,
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
    /// The gate's own OOM preference, raised before user execution (FR-023).
    pub oom_score_adj: Option<i32>,
    /// The soft descriptor limit restored before user execution (FR-077).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile_soft: Option<u64>,
    /// The signal the gate asks the kernel to send it when its supervisor dies: SIGKILL for
    /// Opportunistic work, SIGCONT for Guaranteed work (FR-078). Linux only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_death_signal: Option<i32>,
}

pub trait ExecutionJournal {
    fn namespace_guard(&self) -> Option<crate::namespace::NamespaceGuard> {
        None
    }
    /// Verified journal assurance, supplied by the opened store at the launch barrier.
    fn storage_control_evidence(&self) -> Result<Vec<ControlEvidence>> {
        Ok(Vec::new())
    }
    /// Atomically reserve capacity and assign the next attempt; uncertain capacity stays charged.
    fn reserve(&self, request: &LaunchRequest, capacity: &Resources) -> Result<ExecutionRecord>;
    /// Must commit before authorizing user code. Transitions validate assignment/generation.
    fn transition(&self, record: &ExecutionRecord) -> Result<()>;
    /// Journal the steps applied to a running allocation without a phase change.
    fn record_contention(
        &self,
        _assignment_id: &str,
        _generation: u64,
        _contention: &crate::contention::AllocationContention,
    ) -> Result<()> {
        Ok(())
    }
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
/// The verified processes one supervisor owns. Signals go through owned handles
/// only, never a persisted or numeric PID (research R4).
pub trait Family {
    fn identities(&self) -> Vec<ProcessIdentity>;
    fn signal(&mut self, signal: i32) -> Result<()>;
    /// Cumulative CPU time of the family, when it can be read.
    fn cpu_time(&mut self) -> Option<std::time::Duration>;
}

/// Runtime yield-step mechanisms. Steps reach only the owned family, or a
/// delegated leaf whose membership equals the family's verified identities.
pub trait Actuator {
    /// Steps this mechanism cannot apply to a resource of this allocation.
    fn unavailable(
        &self,
        kind: crate::contention::ResourceKind,
    ) -> std::collections::BTreeSet<crate::contention::YieldStep>;
    /// Apply a step and target, and return the step that reads back as applied.
    fn apply(
        &mut self,
        family: &mut dyn Family,
        kind: crate::contention::ResourceKind,
        step: crate::contention::YieldStep,
        target: Option<u64>,
    ) -> Result<crate::contention::YieldStep>;
    /// Advance time-driven mechanisms such as a duty cycle.
    fn tick(&mut self, family: &mut dyn Family) -> Result<()>;
    /// When `tick` next has work to do, such as a duty cycle's phase switch; `None` when
    /// nothing is time-driven.
    fn next_deadline(&self) -> Option<std::time::Instant> {
        None
    }
    /// A note on how the last step was applied, such as a fallback.
    fn take_detail(&mut self) -> Option<String> {
        None
    }
    /// The target in effect where the mechanism set a different one than asked.
    fn applied_target(&self, _kind: crate::contention::ResourceKind) -> Option<u64> {
        None
    }
    /// A catalog code for why a step holds above its target or became unavailable.
    fn step_code(&self, _kind: crate::contention::ResourceKind) -> Option<&'static str> {
        None
    }
    /// The processes in the allocation's leaf, where the backend has one; read-only.
    fn members(&self) -> Option<std::collections::BTreeSet<u32>> {
        None
    }
    /// Match controls to the recorded steps when an agent reattaches: adopt a
    /// recorded step, and undo a limit no step records.
    fn reconcile(
        &mut self,
        _family: &mut dyn Family,
        _applied: &BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>,
    ) -> Result<()> {
        Ok(())
    }
    /// Undo every runtime step before drain, termination or exit.
    fn release(&mut self, family: &mut dyn Family) -> Result<()>;
}

/// Settings a backend needs to build its actuator for one allocation.
#[derive(Debug, Clone)]
pub struct ActuatorSettings {
    pub class: AllocationClass,
    pub requested: Resources,
    /// Duty-cycle period for a rootless CPU cap: the monitor interval.
    pub period: std::time::Duration,
    /// RAM cap: the noise tolerance above what a leaf cannot reclaim.
    pub ram_noise_tolerance_mib: u64,
    /// RAM cap: how long `memory.high` may throttle without freeing memory.
    pub ram_calm_window: std::time::Duration,
}

pub trait LaunchBackend {
    fn name(&self) -> &str;
    /// Runtime step mechanisms. The default applies rootless signals to the family.
    fn actuator(&self, settings: &ActuatorSettings) -> Box<dyn Actuator> {
        Box::new(crate::actuation::RootlessActuator::new(settings))
    }
    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup>;
    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        Vec::new()
    }
    fn verify(&mut self, identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>>;
    /// Verify a managed child the supervisor launched, by its own identity; a backend with a
    /// leaf also checks the leaf holds only the leader and verified children (SUP-3).
    fn verify_child(&mut self, identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>> {
        self.verify(identity)
    }
    /// Only clean a manager-owned empty leaf; error keeps capacity reserved.
    fn confirm_release(&mut self) -> Result<()>;
    /// The allocation's own out-of-memory kill count, where the backend can read one
    /// (FR-076). Read-only.
    fn oom_kills(&self) -> Option<u64> {
        None
    }
}
