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
    /// Modes the job requested per resource; the node cap still applies (FR-030).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modes: BTreeMap<crate::contention::ResourceKind, crate::contention::ContentionMode>,
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
    /// External GPU compute this interval, in thousandths of device time, when fresh
    /// per-process samples account for device utilization; None is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_compute_milli: Option<u64>,
    /// Own GPU compute this interval, in the same unit, when the amount is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_compute_milli: Option<u64>,
    /// Why the external compute amount is unknown, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_amount_unknown: Option<String>,
    /// Every listed process is an own verified process (research R18): external
    /// compute is measured as zero, and the measured path applies at once.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own_only: bool,
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
    /// Own work's CPU lower bound summed with one tick of pending time per scope
    /// CPU rather than per process; the policy uses it within the per-allocation
    /// sums.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_cpu_lower_millicores: Option<u64>,
    pub total_ram_mib: u64,
    pub available_ram_mib: Option<u64>,
    pub gpu_inventory: CapabilityStatus,
    pub gpus: Vec<GpuSnapshot>,
    pub capabilities: BTreeMap<String, Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<crate::kernel::KernelSnapshot>,
    /// Unmanaged usage by process owner (research R12); `None` when not observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principals: Option<Vec<PrincipalUsage>>,
    /// External processes whose memory the policy tracks for admission peaks
    /// (FR-060); `None` when not observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_processes: Option<Vec<ExternalProcessMemory>>,
}

/// One external process's memory, identified by PID and start time so that a
/// reused PID is another process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalProcessMemory {
    pub pid: u32,
    /// Seconds since the epoch; 0 when the process table does not show it.
    pub start_time: u64,
    pub ram_mib: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_memory_mib: BTreeMap<String, u64>,
}

/// Per-resource amounts in the resource-target file, as targets or as the
/// allowance; unset amounts carry none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAmounts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millicores: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_memory_mib: BTreeMap<String, u64>,
}

/// Usage of the processes one owner runs, outside managed work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrincipalUsage {
    /// Owner UID; `None` groups every process whose owner cannot be read.
    pub uid: Option<u32>,
    /// The account CedeGrid runs as; its usage here is the unmanaged part.
    pub is_owner: bool,
    pub cpu_millicores: u64,
    pub ram_mib: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_memory_mib: BTreeMap<String, u64>,
    /// Measured GPU compute per device, in thousandths of device time.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_compute_milli: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyInput {
    pub allocations: Vec<Allocation>,
    pub explicit_drain: bool,
    /// Per-allocation facts for the ladder and victim selection (schema 3).
    /// Missing facts mean a controllable allocation with every step available
    /// except the hint, which needs a live target channel.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, AllocationFacts>,
    /// The operator's node envelope; own work beyond it yields per resource (FR-057).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Resources>,
    /// The node spool budget is exhausted: admission is blocked and nothing drains (FR-058).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub spool_full: bool,
}

/// What the agent knows about an allocation that observation does not show.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllocationFacts {
    /// No live supervisor can carry out a step, for example while preparing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub uncontrollable: bool,
    /// Time since the last accepted checkpoint, else since start.
    pub progress_lost_ms: u64,
    /// Steps the backend cannot apply, or that the supervisor reported unavailable.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable: BTreeMap<
        crate::contention::ResourceKey,
        std::collections::BTreeSet<crate::contention::YieldStep>,
    >,
    /// The worker acknowledged its resource-target file (FR-062).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hint_ready: bool,
    /// The worker polls the drain file: it acked targets or used the supervisor socket.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub observes_drain: bool,
    /// The steps the supervisor reported applied, per resource.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub applied: BTreeMap<crate::contention::ResourceKey, crate::contention::YieldStep>,
    /// The cap or hint targets the supervisor reported applied, per resource.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub applied_targets: BTreeMap<crate::contention::ResourceKey, u64>,
    /// The backend can cap RAM through `memory.high` on a delegated leaf; without
    /// it the RAM cap step is unavailable.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ram_cap: bool,
    /// Catalog codes the supervisor reported for steps held above their target or
    /// found unavailable, per resource.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub step_codes: BTreeMap<crate::contention::ResourceKey, String>,
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
    /// The same reasons with their catalog codes (FR-039).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<ReasonCode>,
    /// Per-resource state (schema 3).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub resources: BTreeMap<crate::contention::ResourceKey, ResourceStatus>,
    /// The yield steps the policy authority decided (schema 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<YieldAction>,
    /// Active caution options with their marker (schema 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<ExceptionNotice>,
    /// The effective mode of each allocation per resource (FR-030).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allocation_modes: BTreeMap<
        String,
        BTreeMap<crate::contention::ResourceKind, crate::contention::ContentionMode>,
    >,
    /// The node's cap on the most aggressive mode a job may request (FR-030).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mode: Option<crate::contention::ContentionMode>,
    /// What each running allocation may grow to now without causing a step, as
    /// published (FR-071).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowances: BTreeMap<String, ResourceAmounts>,
    /// CPU that running opportunistic work is charged above its p95 usage, which
    /// the schedulable budget adds so request-based charges admit by usage
    /// (FR-034). Negative when recent usage exceeds max(request, observed).
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cpu_admission_credit_millicores: i64,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
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
    /// For a hint, the deadline the policy enforces before the next step (FR-037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_back: Option<HeldBack>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<ReasonCode>,
}

/// What a resource keeps from own work or from admission, and why, in resource
/// units (SC-016). Zero amounts are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeldBack {
    /// The no-harm floor (memory).
    #[serde(skip_serializing_if = "is_zero")]
    pub floor: u64,
    /// The explicit CPU reserve where it exceeds the mode's margin.
    #[serde(skip_serializing_if = "is_zero")]
    pub reserve: u64,
    /// The mode's idle margin.
    #[serde(skip_serializing_if = "is_zero")]
    pub margin: u64,
    /// CPU a visible ancestor CPU ceiling keeps from the budget beyond the reserve and margin;
    /// not contention.
    #[serde(skip_serializing_if = "is_zero")]
    pub ceiling: u64,
    /// Capacity left to contending external demand under contention.
    #[serde(skip_serializing_if = "is_zero")]
    pub contention: u64,
    /// The contention hold when only external PSI saturates the CPU (FR-072).
    #[serde(skip_serializing_if = "is_zero")]
    pub contention_psi: u64,
    /// External memory peak above current external usage (admission).
    #[serde(skip_serializing_if = "is_zero")]
    pub peak: u64,
    /// External GPU memory growth tolerated as noise.
    #[serde(skip_serializing_if = "is_zero")]
    pub noise_tolerance: u64,
    /// Capacity withheld while the measurement is unknown or held.
    #[serde(skip_serializing_if = "is_zero")]
    pub unknown: u64,
    /// Admission headroom withheld until the resource has cooled down.
    #[serde(skip_serializing_if = "is_zero")]
    pub cooldown: u64,
    /// Admission headroom withheld by the reclaim backoff.
    #[serde(skip_serializing_if = "is_zero")]
    pub backoff: u64,
    /// Admission capacity held by charges above usage.
    #[serde(skip_serializing_if = "is_zero")]
    pub charge_shortfall: u64,
    /// GPU compute: whether the external amount was measured, or why it is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_path: Option<ComputePath>,
    /// The resource's capacity, in its units.
    #[serde(skip_serializing_if = "is_zero")]
    pub capacity: u64,
    /// Each cause above as a percent of `capacity`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub percent: BTreeMap<String, Percent>,
}

impl HeldBack {
    /// The causes that hold something back, with their amounts, in a fixed order.
    pub fn causes(&self) -> Vec<(&'static str, u64)> {
        [
            ("floor", self.floor),
            ("reserve", self.reserve),
            ("margin", self.margin),
            ("ceiling", self.ceiling),
            ("contention", self.contention),
            ("contention_psi", self.contention_psi),
            ("unknown", self.unknown),
            ("peak", self.peak),
            ("noise_tolerance", self.noise_tolerance),
            ("cooldown", self.cooldown),
            ("backoff", self.backoff),
            ("charge_shortfall", self.charge_shortfall),
        ]
        .into_iter()
        .filter(|(_, amount)| *amount > 0)
        .collect()
    }

    /// Record `capacity` and each cause's share of it.
    pub fn with_capacity(mut self, capacity: u64) -> Self {
        self.capacity = capacity;
        self.percent = if capacity == 0 {
            BTreeMap::new()
        } else {
            self.causes()
                .into_iter()
                .map(|(cause, amount)| {
                    let tenths = (amount as f64 * 1000.0 / capacity as f64).round() / 10.0;
                    (cause.to_string(), Percent(tenths))
                })
                .collect()
        };
        self
    }
}

/// A share of capacity in percent, to one decimal.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Percent(pub f64);

// Percentages are finite by construction.
impl Eq for Percent {}

/// The path a device's GPU compute share took (FR-021).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "path", rename_all = "snake_case")]
pub enum ComputePath {
    /// Fresh per-process samples accounted for device utilization.
    Measured,
    /// External activity of unknown amount: one principal (FR-021).
    Unknown {
        /// A catalog code.
        #[serde(default)]
        code: String,
        reason: String,
    },
}

impl ComputePath {
    pub fn code(&self) -> &str {
        match self {
            ComputePath::Measured => "gpu_compute_measured",
            ComputePath::Unknown { code, .. } => code,
        }
    }
}

impl Decision {
    /// Every code this decision reports: resource reasons, admission, `held_back`
    /// causes and compute paths, actions and their causes, caution options and
    /// node-wide reasons (T113).
    pub fn codes(&self) -> std::collections::BTreeSet<String> {
        let mut codes = std::collections::BTreeSet::new();
        for status in self.resources.values() {
            codes.extend(status.reasons.iter().map(|reason| reason.code.clone()));
            if let Some(code) = status.admission.as_ref().and_then(|a| a.code()) {
                codes.insert(code.to_string());
            }
            if let Some(held) = &status.held_back {
                codes.extend(
                    held.causes()
                        .into_iter()
                        .map(|(cause, _)| format!("held_back_{cause}")),
                );
                if let Some(path) = &held.compute_path {
                    codes.insert(path.code().to_string());
                }
            }
        }
        for action in &self.actions {
            codes.insert(action.code.clone());
            if let Ok(serde_json::Value::String(cause)) = serde_json::to_value(action.cause) {
                codes.insert(cause);
            }
        }
        codes.extend(self.exceptions.iter().map(|notice| notice.name.clone()));
        codes.extend(self.reason_codes.iter().map(|reason| reason.code.clone()));
        codes
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A caution option that is active, with its marker (FR-043).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionNotice {
    pub name: String,
    pub marker: RiskMarker,
}

/// Risk markers. Caution options let own work hold more capacity within the
/// floors and reclaim bounds; no option may put another workload at risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskMarker {
    Caution,
}
