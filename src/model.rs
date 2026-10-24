//! Transport-independent, versioned inputs to the policy engine.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 3;
pub fn supported_schema(version: u32) -> bool {
    matches!(version, 1..=3)
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

impl Resources {
    /// One allocation's charge: the larger of request and observed usage per
    /// resource. Unknown usage charges the request, never zero.
    pub fn charge(requested: &Resources, observed: Option<&Resources>) -> Resources {
        let mut result = requested.clone();
        if let Some(observed) = observed {
            result.cpu_millicores = result.cpu_millicores.max(observed.cpu_millicores);
            result.ram_mib = result.ram_mib.max(observed.ram_mib);
            for (gpu, amount) in &observed.gpu_memory_mib {
                let current = result.gpu_memory_mib.entry(gpu.clone()).or_default();
                *current = (*current).max(*amount);
            }
        }
        result
    }

    /// Adds a charge; overflow is an error so no reservation is silently lost.
    pub fn add_checked(&mut self, other: &Resources) -> anyhow::Result<()> {
        use anyhow::Context;
        self.cpu_millicores = self
            .cpu_millicores
            .checked_add(other.cpu_millicores)
            .context("CPU resource sum overflow")?;
        self.ram_mib = self
            .ram_mib
            .checked_add(other.ram_mib)
            .context("RAM resource sum overflow")?;
        for (gpu, amount) in &other.gpu_memory_mib {
            let current = self.gpu_memory_mib.entry(gpu.clone()).or_default();
            *current = current
                .checked_add(*amount)
                .context("GPU resource sum overflow")?;
        }
        Ok(())
    }

    pub fn add_saturating(&mut self, other: &Resources) {
        self.cpu_millicores = self.cpu_millicores.saturating_add(other.cpu_millicores);
        self.ram_mib = self.ram_mib.saturating_add(other.ram_mib);
        for (gpu, amount) in &other.gpu_memory_mib {
            let current = self.gpu_memory_mib.entry(gpu.clone()).or_default();
            *current = current.saturating_add(*amount);
        }
    }

    /// Subtracts without going below zero. Devices absent here stay absent.
    pub fn subtract_saturating(&mut self, other: &Resources) {
        self.cpu_millicores = self.cpu_millicores.saturating_sub(other.cpu_millicores);
        self.ram_mib = self.ram_mib.saturating_sub(other.ram_mib);
        for (gpu, amount) in &other.gpu_memory_mib {
            if let Some(current) = self.gpu_memory_mib.get_mut(gpu) {
                *current = current.saturating_sub(*amount);
            }
        }
    }

    /// Whether `self`, the total charge including a new request, fits `cap`.
    /// CPU and RAM are shared by all work; each GPU is its own scope, so only the
    /// devices the request uses are compared and every old charge stays counted.
    pub fn fits(&self, cap: &Resources, request: &Resources) -> bool {
        self.cpu_millicores <= cap.cpu_millicores
            && self.ram_mib <= cap.ram_mib
            && request.gpu_memory_mib.keys().all(|gpu| {
                self.gpu_memory_mib.get(gpu).copied().unwrap_or(0)
                    <= cap.gpu_memory_mib.get(gpu).copied().unwrap_or(0)
            })
    }
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
    /// Collectors report a lower bound here.
    pub observed: Option<Resources>,
    /// Upper bound of attributed usage (schema 3). Missing means no bound is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_upper: Option<Resources>,
}

fn legacy_policy_allocation_class() -> crate::execution_model::AllocationClass {
    crate::execution_model::AllocationClass::Opportunistic
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComputeActivity {
    Idle,
    /// External contexts exist but showed no compute activity over a sustained
    /// window of healthy observation (schema 3).
    Quiet,
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
    /// Device memory reserved by the driver or firmware (schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_memory_mib: Option<u64>,
    /// Device memory free for allocation (schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_memory_mib: Option<u64>,
    /// Memory that is neither attributed to own work nor proven external (schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed_memory_mib: Option<u64>,
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
    /// Lower bound of host busy time (schema 3); `cpu_busy_millicores` is the upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_busy_lower_millicores: Option<u64>,
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
    /// Derived from `actions` with a step at or above drain.
    pub would_drain: Vec<String>,
    pub reasons: Vec<String>,
    /// Per-resource state (schema 3).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub resources: BTreeMap<crate::contention::ResourceKey, ResourceStatus>,
    /// The yield steps the policy authority decided (schema 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<YieldAction>,
    /// Active opt-in exceptions with their risk marker (schema 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<ExceptionNotice>,
}

/// A stable machine-readable code plus one sentence for people. Tests and tools
/// rely on the code only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonCode {
    pub code: String,
    pub text: String,
}

impl ReasonCode {
    pub fn new(code: &str, text: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            text: text.into(),
        }
    }
}

/// One step for one allocation and resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YieldAction {
    pub allocation_id: String,
    pub resource: crate::contention::ResourceKey,
    pub step: crate::contention::YieldStep,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
    pub cause: crate::contention::YieldCause,
    pub code: String,
}

/// Per-resource state reported in decisions, `observe`, `doctor` and node reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::contention::ContentionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_mode: Option<crate::contention::ContentionMode>,
    /// Lower and upper bound of own usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_share: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<crate::contention::Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ComputeActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<crate::contention::Admission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<ReasonCode>,
}

/// An opt-in exception that is active, with its risk marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionNotice {
    pub name: String,
    pub marker: RiskMarker,
}

/// Risk markers for opt-in exceptions: caution can make other workloads'
/// allocations fail or slow them; danger can let an external process be
/// killed or starved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskMarker {
    Caution,
    Danger,
}
