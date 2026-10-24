//! Pure, deterministic contention modes, resource views and yield-step planning.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// How much spare capacity own work may use and how fast it gives it back.
/// Ordered from least to most aggressive, which is the order the node cap uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentionMode {
    YieldFirst,
    Neutral,
    Performance,
}

/// The four resource families a mode applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Cpu,
    Ram,
    GpuCompute,
    GpuMemory,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 4] = [
        ResourceKind::Cpu,
        ResourceKind::Ram,
        ResourceKind::GpuCompute,
        ResourceKind::GpuMemory,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ResourceKind::Cpu => "cpu",
            ResourceKind::Ram => "ram",
            ResourceKind::GpuCompute => "gpu_compute",
            ResourceKind::GpuMemory => "gpu_memory",
        }
    }

    /// RAM and GPU memory can only be released by the process that holds them.
    pub fn is_memory(self) -> bool {
        matches!(self, ResourceKind::Ram | ResourceKind::GpuMemory)
    }
}

/// One contended resource. GPU keys carry the device UUID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    Cpu,
    Ram,
    GpuCompute(String),
    GpuMemory(String),
}

impl ResourceKey {
    pub fn kind(&self) -> ResourceKind {
        match self {
            ResourceKey::Cpu => ResourceKind::Cpu,
            ResourceKey::Ram => ResourceKind::Ram,
            ResourceKey::GpuCompute(_) => ResourceKind::GpuCompute,
            ResourceKey::GpuMemory(_) => ResourceKind::GpuMemory,
        }
    }

    fn sort_key(&self) -> (u8, &str, u8) {
        match self {
            ResourceKey::Cpu => (0, "", 0),
            ResourceKey::Ram => (1, "", 0),
            ResourceKey::GpuCompute(uuid) => (2, uuid, 0),
            ResourceKey::GpuMemory(uuid) => (2, uuid, 1),
        }
    }
}

impl Ord for ResourceKey {
    /// CPU, RAM, then GPU devices by UUID with compute before memory.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for ResourceKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ResourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceKey::Cpu => f.write_str("cpu"),
            ResourceKey::Ram => f.write_str("ram"),
            ResourceKey::GpuCompute(uuid) => write!(f, "gpu_compute:{uuid}"),
            ResourceKey::GpuMemory(uuid) => write!(f, "gpu_memory:{uuid}"),
        }
    }
}

impl std::str::FromStr for ResourceKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let device = |uuid: &str| -> Result<String, String> {
            if uuid.trim().is_empty() {
                Err(format!("resource key {value:?} has an empty device UUID"))
            } else {
                Ok(uuid.to_owned())
            }
        };
        match value.split_once(':') {
            None if value == "cpu" => Ok(ResourceKey::Cpu),
            None if value == "ram" => Ok(ResourceKey::Ram),
            Some(("gpu_compute", uuid)) => Ok(ResourceKey::GpuCompute(device(uuid)?)),
            Some(("gpu_memory", uuid)) => Ok(ResourceKey::GpuMemory(device(uuid)?)),
            _ => Err(format!("unknown resource key {value:?}")),
        }
    }
}

impl Serialize for ResourceKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ResourceKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// One level of the graduated response, applied per allocation and resource.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum YieldStep {
    #[default]
    None,
    LowerShare,
    Cap,
    Pause,
    Hint,
    Drain,
    Terminate,
}

/// Why a step was taken. It travels with the decision to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YieldCause {
    ExternalGrowth,
    FloorBreach,
    UnknownMeasurement,
    EnvelopeBreach,
    OperatorDrain,
    LeaseExpired,
    CoordinatorDrain,
}

/// A user ID with current demand. `uid: None` stands for every process whose
/// owner cannot be read; they count together as one principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Principal {
    pub uid: Option<u32>,
    pub is_owner: bool,
}

/// Whether a resource measurement can be trusted this interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Measurement {
    Known,
    /// Own and host readings disagree, but within the known sampling error.
    InconsistentWithinTolerance,
    Unknown,
}

/// Effective thresholds for one resource after defaults are applied.
/// Every value is a configuration setting; the defaults are provisional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceThresholds {
    pub yield_first_margin_percent: u8,
    pub performance_margin_percent: u8,
    pub yield_first_response_intervals: u32,
    /// Configured scale-up interval; `None` uses the mode's default.
    pub scale_up_step_interval_s: Option<u64>,
    pub neutral_convergence_s: u64,
    pub neutral_tolerance_points: u8,
    pub performance_ramp_s: u64,
    pub performance_ramp_percent: u8,
    pub reclaim_s: u64,
    pub hint_deadline_s: u64,
    pub unknown_window_intervals: u32,
    pub hysteresis_percent: u8,
    pub min_step_dwell_intervals: u32,
    pub pause_bound_s: u64,
    pub noise_tolerance_mib: u64,
    pub quiet_window_intervals: u32,
    pub cpu_ram_scale_up_cooldown_ms: u64,
}

impl ResourceThresholds {
    /// Provisional defaults from the spec, to validate on real hardware.
    pub fn defaults(kind: ResourceKind) -> Self {
        Self {
            yield_first_margin_percent: 25,
            performance_margin_percent: 10,
            yield_first_response_intervals: 3,
            scale_up_step_interval_s: None,
            neutral_convergence_s: 60,
            neutral_tolerance_points: 15,
            performance_ramp_s: 30,
            performance_ramp_percent: 80,
            reclaim_s: if kind.is_memory() { 15 } else { 5 },
            hint_deadline_s: 5,
            unknown_window_intervals: 3,
            hysteresis_percent: 50,
            min_step_dwell_intervals: 3,
            pause_bound_s: 10,
            noise_tolerance_mib: 256,
            quiet_window_intervals: 20,
            cpu_ram_scale_up_cooldown_ms: 30_000,
        }
    }

    /// The scale-up interval for a mode when none is configured.
    pub fn scale_up_interval_s(&self, mode: ContentionMode) -> u64 {
        self.scale_up_step_interval_s.unwrap_or(match mode {
            ContentionMode::YieldFirst => 30,
            ContentionMode::Neutral => 10,
            ContentionMode::Performance => 2,
        })
    }
}

/// One resource as the policy sees it in one interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceView {
    pub key: ResourceKey,
    /// Schedulable capacity after operator reserves.
    pub capacity: u64,
    /// Lower and upper bound of attributed own usage.
    pub own: (u64, u64),
    /// Usage not attributed to own work.
    pub external: u64,
    /// Measurement-error band that is neither own nor external.
    pub unattributed: u64,
    pub floor: u64,
    pub measurement: Measurement,
}
