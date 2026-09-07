//! Transport-independent, versioned inputs to the policy engine.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 2;
pub fn supported_schema(version: u32) -> bool {
    matches!(version, 1 | 2)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    Unsupported,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub status: CapabilityStatus,
    pub detail: String,
    /// Capability availability never implies that a kernel control was applied.
    pub enforced: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    /// 1000 millicores = one logical CPU. CPU time and reservations use the same unit.
    pub cpu_millicores: u64,
    pub ram_mib: u64,
    pub gpu_memory_mib: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationPhase {
    Pending,
    Running,
    Draining,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub id: String,
    /// Runtime producers copy the durable execution class. Legacy policy recordings
    /// omitted it and retain their original opportunistic victim-selection behavior.
    #[serde(default = "legacy_policy_allocation_class")]
    pub class: crate::execution_model::AllocationClass,
    pub phase: AllocationPhase,
    pub requested: Resources,
    /// Independently attributed usage; None is unknown, never zero usage.
    pub observed: Option<Resources>,
}

fn legacy_policy_allocation_class() -> crate::execution_model::AllocationClass {
    crate::execution_model::AllocationClass::Opportunistic
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComputeActivity {
    Idle,
    Active,
    Unknown,
}

/// Observation capabilities do not grant admission or apply a hard GPU partition.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GpuExecutionCapability {
    ContentionAware,
    ConservativeNonSharing,
    /// Explicitly permitted occupied sharing; activity may remain unknown.
    BestEffortOccupied,
    InsufficientObservability,
}

/// An independently tracked external process. Authorization is device-scoped in
/// configuration and never follows a username, executable name, or recycled PID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalGpuProcessIdentity {
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: u64,
    pub uid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuObservation {
    pub capability: GpuExecutionCapability,
    /// Exact external identities verified before and after complete device process
    /// enumeration. None is absent/failed verification; Some([]) is verified empty.
    /// These processes remain external in inventory, activity, and memory accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_effort_external_processes: Option<Vec<ExternalGpuProcessIdentity>>,
    /// Exact entry point and interpreted NVML return variant; no-data is distinct
    /// from unsupported, permission failure, and successful empty responses.
    pub activity_api: String,
    pub activity_api_status: String,
    pub query_cursor_us: u64,
    pub newest_sample_timestamp_us: Option<u64>,
    pub samples_returned: usize,
    pub observed_at_unix_ms: u64,
    /// Observer monotonic time since its last new driver-timestamp baseline,
    /// including query latency. NVML documents CPU microseconds without a Unix
    /// epoch contract; this is deliberately not an absolute driver-sample age.
    pub baseline_age_ms: Option<u64>,
    pub sample_max_age_ms: u64,
    pub fresh_after_baseline: bool,
    pub freshness_decision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuSnapshot {
    pub uuid: String,
    pub total_memory_mib: u64,
    pub used_memory_mib: Option<u64>,
    /// Auxiliary kernel-busy-time percentage, not compute capacity or interference.
    pub utilization_percent: Option<u32>,
    /// None means process enumeration failed or is unsupported.
    pub external_process_ids: Option<Vec<u32>>,
    pub external_compute: ComputeActivity,
    /// Driver sample identity, when provided. Repeated reads are not independent samples.
    pub compute_sample_id: Option<u64>,
    /// Older recordings lacked capability diagnostics; readers retain their
    /// original fail-closed activity interpretation, never inventing a mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<GpuObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    pub node_id: String,
    pub observed_at_unix_ms: u64,
    pub cpu_capacity_millicores: u64,
    pub physical_cores: Option<u32>,
    pub cpu_busy_millicores: Option<u64>,
    pub total_ram_mib: u64,
    pub available_ram_mib: Option<u64>,
    pub gpu_inventory: CapabilityStatus,
    pub gpus: Vec<GpuSnapshot>,
    pub capabilities: BTreeMap<String, Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<crate::kernel::KernelSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyInput {
    pub allocations: Vec<Allocation>,
    pub explicit_drain: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub schema_version: u32,
    pub node_id: String,
    pub observe_only: bool,
    /// Budget for all managed allocations; includes currently occupied capacity.
    pub managed_budget: Resources,
    /// Remaining admission capacity after one charge per existing/pending allocation.
    pub admission_headroom: Resources,
    pub expansion_allowed: bool,
    /// Independent CPU/RAM scope; never authorizes a GPU allocation.
    #[serde(default)]
    pub cpu_ram_expansion_allowed: bool,
    pub would_drain: Vec<String>,
    pub reasons: Vec<String>,
}
