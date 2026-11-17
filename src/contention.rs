//! Pure, deterministic contention modes, resource views and yield-step planning.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

/// How much spare capacity own work may use and how fast it gives it back.
/// Ordered from least to most aggressive, which is the order the node cap uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentionMode {
    YieldFirst,
    Neutral,
    Performance,
    /// Performance with no admission margin, filling idle compute at once (004). A caution
    /// option the node cap admits only when raised to it explicitly.
    Aggressive,
}

impl ContentionMode {
    /// The name in configuration and on the wire.
    pub fn name(self) -> &'static str {
        match self {
            ContentionMode::YieldFirst => "yield_first",
            ContentionMode::Neutral => "neutral",
            ContentionMode::Performance => "performance",
            ContentionMode::Aggressive => "aggressive",
        }
    }

    /// The name shown to people.
    pub fn label(self) -> &'static str {
        match self {
            ContentionMode::YieldFirst => "Yield First",
            ContentionMode::Neutral => "Neutral",
            ContentionMode::Performance => "Performance",
            ContentionMode::Aggressive => "Aggressive",
        }
    }

    /// How far a memory allowance may rise per scale-up interval, in percent of
    /// capacity. A mechanism constant: memory keeps only the floor under Neutral
    /// and Performance, so no margin sizes it there.
    pub fn memory_allowance_step_percent(self) -> u8 {
        match self {
            ContentionMode::YieldFirst => 20,
            ContentionMode::Neutral => 5,
            ContentionMode::Performance | ContentionMode::Aggressive => 2,
        }
    }

    pub const ALL: [ContentionMode; 4] = [
        ContentionMode::YieldFirst,
        ContentionMode::Neutral,
        ContentionMode::Performance,
        ContentionMode::Aggressive,
    ];

    /// The modes with a configurable margin; Aggressive's is fixed at 0%.
    pub const MARGINED: [ContentionMode; 3] = [
        ContentionMode::YieldFirst,
        ContentionMode::Neutral,
        ContentionMode::Performance,
    ];
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YieldCause {
    ExternalGrowth,
    /// Own usage over its target while the budget did not fall (FR-011).
    OwnOverTarget,
    FloorBreach,
    UnknownMeasurement,
    EnvelopeBreach,
    OperatorDrain,
    LeaseExpired,
    CoordinatorDrain,
    /// The kernel's OOM killer ended a task while own memory was within its fair share
    /// (FR-076); the task is requeued without an attempt.
    OomNotOwnFault,
}

/// Every code the policy reports, each with one sentence (FR-039, T113). Codes
/// are stable; the emitted text may add detail and is not.
pub const REASON_CODES: &[(&str, &str)] = &[
    // Yield steps, as action codes.
    (
        "yield_step_lower_share",
        "Own work on the resource runs at a lower share.",
    ),
    (
        "yield_step_cap",
        "Own work on the resource is capped at a target.",
    ),
    ("yield_step_pause", "Own work on the resource is paused."),
    (
        "yield_step_hint",
        "Own work is asked through its resource-target file to shrink to a target.",
    ),
    ("yield_step_drain", "Own work is asked to drain and stop."),
    (
        "yield_step_terminate",
        "Own work that did not drain in time is terminated.",
    ),
    // Why a step was taken.
    (
        "external_growth",
        "External demand grew into what own work holds.",
    ),
    (
        "own_over_target",
        "Own usage stayed over its target while the budget did not fall.",
    ),
    ("floor_breach", "Free memory fell below the no-harm floor."),
    (
        "unknown_measurement",
        "The resource's measurement stayed unknown past the unknown window.",
    ),
    ("envelope_breach", "Own work exceeds the operator envelope."),
    ("operator_drain", "The operator asked the node to drain."),
    (
        "coordinator_drain",
        "The coordinator asked for the allocation to drain.",
    ),
    (
        "lease_expired",
        "The allocation's lease expired before it was renewed.",
    ),
    (
        "oom_not_own_fault",
        "The out-of-memory killer ended own work within its fair share, so it is requeued without an attempt.",
    ),
    // Why a step was relaxed.
    (
        "step_reversed_contention_ended",
        "A step taken for external growth was relaxed because external demand fell back.",
    ),
    (
        "step_reversed_own_usage_fell",
        "A step taken for own usage over its target was relaxed because own usage fell back.",
    ),
    (
        "step_reversed_floor_restored",
        "A step taken for a floor breach was relaxed because free memory is back above the floor.",
    ),
    (
        "step_reversed_measurement_known",
        "A step taken for an unknown measurement was relaxed because the measurement is known again.",
    ),
    (
        "step_reversed_envelope_restored",
        "A step taken for the operator envelope was relaxed because own work is back within it.",
    ),
    (
        "step_reversed_drain_lifted",
        "A step taken for an operator drain was relaxed because the drain was lifted.",
    ),
    (
        "step_skipped_unavailable",
        "A step this allocation cannot take was skipped for the next one.",
    ),
    (
        "saturated_by_psi",
        "External tasks wait for CPU above psi_some_percent (PSI), so the CPU counts as saturated; a cgroup shared with own work never counts.",
    ),
    (
        "psi_system_only",
        "Only system-wide CPU pressure is readable; it cannot tell own from external waiting, so it adds no evidence.",
    ),
    (
        "psi_unknown",
        "CPU pressure is missing, unreadable or malformed, so it adds no evidence.",
    ),
    (
        "ram_cap_growth_stop",
        "The RAM cap holds above its target, at what the leaf cannot reclaim, so it only stops growth and the hint frees the rest.",
    ),
    (
        "ram_cap_ineffective",
        "The RAM cap throttled without freeing memory, so it was raised to current usage and the hint frees the rest.",
    ),
    (
        "ram_cap_refused",
        "A RAM cap write or its readback failed, so the cap is unavailable for this allocation and the hint follows.",
    ),
    // Measurements.
    (
        "allocation_unknown_hold",
        "An allocation's usage is unknown, so it is charged at the larger of its request and last known usage and its step holds.",
    ),
    (
        "measurement_unknown_hold",
        "Own usage exceeds what the host reports beyond the known error, so the resource holds.",
    ),
    (
        "cpu_window_estimate_not_kept",
        "The principal list does not show another account's process, so no CPU estimate is kept for a window's refill.",
    ),
    (
        "inconsistent_within_tolerance",
        "Own usage exceeds what the host reports within the known error, so external usage is clamped.",
    ),
    (
        "first_observation_baseline",
        "External GPU memory at the first observation is the baseline, not growth.",
    ),
    (
        "vram_noise_below_tolerance",
        "External GPU memory grew within the noise tolerance.",
    ),
    // Contention.
    (
        "mode_capped",
        "A job's requested mode is above the node cap, so it runs at the cap.",
    ),
    (
        "probing_demand",
        "Own work steps down by one margin step to learn whether a small external demand is satisfied.",
    ),
    (
        "fair_share_held",
        "The principal list cannot be read, so own work's fair share stays at the last one a readable list gave.",
    ),
    (
        "launch_nice_kept",
        "A task launched at the Yield First nice keeps it in another CPU mode, since it cannot be lowered again; its caps do the yielding.",
    ),
    (
        "watching_larger_demand",
        "Own work holds the fair share while a larger external demand is watched for one response window.",
    ),
    (
        "satisfied_demand",
        "An external user's demand is recorded as satisfied, so it no longer contends below that demand.",
    ),
    (
        "gpu_time_slice_fair",
        "Own work holds no more than its fair share of the time-sliced device's busy contexts, so the driver's slices keep it fair without a GPU compute cap.",
    ),
    (
        "gpu_time_slice_unbound",
        "Own work holds more than its share of the time-sliced device's busy contexts, but no other principal reads slice-bound, so the driver's slices already give each what it asks for without a GPU compute cap.",
    ),
    (
        "gpu_time_slice_hold",
        "Own tasks on the time-sliced device are paused down to the running ones the time-slice rule allows beside the slice-bound principals.",
    ),
    (
        "gpu_first_come_floor",
        "Own GPU work was on the device before every other principal listed there, so it keeps at least the share that leaves none of them more than half of the device.",
    ),
    (
        "gpu_time_slice_admission_held",
        "One more own process on the time-sliced device would take a slice-bound principal below its fair share of the busy contexts, so GPU admission holds.",
    ),
    (
        "applied_duty_credit",
        "Own GPU work is credited at what its supervisor applied: a paused task at nothing.",
    ),
    (
        "gpu_compute_measured",
        "External GPU compute is measured from per-process samples.",
    ),
    (
        "gpu_compute_unknown_activity",
        "External GPU activity without per-process amounts counts as one principal of unknown amount.",
    ),
    (
        "gpu_compute_amount_unknown",
        "Per-process GPU samples do not account for the device, so the external amount is unknown.",
    ),
    (
        "gpu_compute_recently_inconsistent",
        "Per-process GPU samples were recently inconsistent or are new, so the measured path waits for the quiet window.",
    ),
    (
        "small_floor_raised",
        "A floor near its minimum applies as twice the minimum until small_floor is acknowledged.",
    ),
    // Admission.
    (
        "admission_draining",
        "The node is draining, so nothing is admitted.",
    ),
    (
        "admission_spool_exhausted",
        "The node spool budget is exhausted, so nothing is admitted and nothing is drained.",
    ),
    (
        "admission_floor_breached",
        "The floor is breached, so nothing is admitted on this resource.",
    ),
    (
        "admission_measurement_unknown",
        "The measurement is unknown, so admission waits for it.",
    ),
    (
        "admission_own_work_yielding",
        "Own work on this resource is yielding, so nothing more is admitted.",
    ),
    (
        "admission_cooling_down",
        "The resource is cooling down after its last change.",
    ),
    (
        "admission_reclaim_backoff",
        "The reclaim backoff after recent floor breaches applies.",
    ),
    (
        "admission_over_own_target",
        "Own charged usage plus the request would exceed own target, so the request waits.",
    ),
    (
        "admission_envelope_exceeded",
        "Own work exceeds the operator envelope, so nothing is admitted.",
    ),
    (
        "gpu_memory_unknown",
        "GPU memory or its attribution is unknown, so the device admits nothing.",
    ),
    (
        "gpu_external_workload_appeared",
        "A new external GPU workload or external memory growth appeared.",
    ),
    (
        "gpu_charges_exceed_reserve",
        "Own GPU charges exceed the memory reserve or the allocation budget.",
    ),
    (
        "gpu_never_share_occupied",
        "The never_share_occupied_gpus setting refuses a device with external contexts.",
    ),
    (
        "gpu_yield_first_needs_quiet_device",
        "Yield First admits GPU work only on a quiet device.",
    ),
    (
        "gpu_yield_first_activity_unknown",
        "Yield First admits GPU work only on a quiet device, and the device's activity is unknown.",
    ),
    ("gpu_observation_unknown", "The GPU observation is unknown."),
    (
        "gpu_inventory_unavailable",
        "The GPU inventory is unavailable, so GPU admission is blocked.",
    ),
    (
        "gpu_absent",
        "An allocated GPU is absent from the current inventory.",
    ),
    // Node-wide readings.
    (
        "cpu_capacity_unknown",
        "The CPU capacity is unknown, so the CPU budget is zero.",
    ),
    (
        "ram_capacity_unknown",
        "The RAM capacity is unknown, so the RAM budget is zero.",
    ),
    (
        "cpu_usage_unknown",
        "CPU busy time is unavailable, so CPU admission is blocked.",
    ),
    (
        "config_reloaded",
        "The agent applied a reloaded configuration's contention settings from the next interval.",
    ),
    (
        "config_reload_refused",
        "The agent kept its configuration because the reloaded one needs a restart or could not be read.",
    ),
    (
        "report_stale",
        "The coordinator renews no lease and offers no work while the node's last report is stale.",
    ),
    (
        "single_process_violated",
        "Processes outside the run's verified family were found; they are not measured as own work or stepped.",
    ),
    (
        "family_members",
        "The run has more processes than its leader; its verified family is measured and stepped as own work.",
    ),
    (
        "cpu_unknown_fair_share_hold",
        "No longer emitted; kept for recorded decisions from before Performance stopped stepping for unknown CPU.",
    ),
    (
        "ram_usage_unknown",
        "Available RAM is unknown, so RAM admission is blocked.",
    ),
    (
        "kernel_cpu_stale",
        "The kernel CPU snapshot is stale, and repeated readings do not refresh it.",
    ),
    (
        "cpu_scoped_budget",
        "The CPU budget uses the effective CPU set's usage and capacity.",
    ),
    (
        "cpu_topology_unknown",
        "The CPU topology is unknown, so the physical-core reserve cannot be placed.",
    ),
    (
        "cpu_set_unknown",
        "The effective CPU set is unavailable, so scoped CPU admission is blocked.",
    ),
    (
        "cpu_scoped_telemetry_unknown",
        "Scoped CPU telemetry is missing, stale or inconsistent.",
    ),
    (
        "cpu_charges_exceed_budget",
        "Managed CPU charges exceed the scheduling budget.",
    ),
    (
        "ram_charges_exceed_budget",
        "Managed RAM charges exceed the scheduling budget.",
    ),
    (
        "cpu_only_admission",
        "Stable CPU and RAM headroom allows CPU-only admission while GPU admission stays blocked.",
    ),
    (
        "cooldown_pending",
        "The node-wide cooldown behind the 0.2 expansion flags has not elapsed; each resource's admission keeps its own cooldown.",
    ),
    // What `held_back` holds, by cause.
    ("held_back_floor", "Memory kept free as the no-harm floor."),
    (
        "held_back_reserve",
        "CPU kept free by the explicit reserve where it exceeds the margin.",
    ),
    (
        "held_back_margin",
        "Capacity kept free by the mode's idle margin.",
    ),
    (
        "held_back_ceiling",
        "CPU a visible ancestor CPU ceiling keeps from the budget beyond the reserve and margin; it is not contention.",
    ),
    (
        "held_back_contention",
        "Capacity left to contending external demand.",
    ),
    (
        "held_back_contention_psi",
        "CPU left to external demand while only external pressure (PSI) shows the CPU saturated.",
    ),
    (
        "held_back_peak",
        "Admission headroom kept for the external memory peak.",
    ),
    (
        "held_back_noise_tolerance",
        "Never shown: the GPU memory noise tolerance is a growth threshold that withholds nothing.",
    ),
    (
        "held_back_unknown",
        "Capacity withheld while the measurement is unknown or held.",
    ),
    (
        "held_back_cooldown",
        "Admission headroom withheld until the resource has cooled down.",
    ),
    (
        "held_back_backoff",
        "Admission headroom withheld by the reclaim backoff.",
    ),
    (
        "held_back_charge_shortfall",
        "Admission capacity held by charges above usage.",
    ),
    // Caution options.
    (
        "small_floor",
        "A floor below twice its minimum keeps less headroom for other workloads' bursts.",
    ),
    (
        "reduced_margin",
        "A margin below its default leaves external demand less idle capacity to grow into.",
    ),
    (
        "relaxed_detection",
        "A detection setting beyond its default recognizes contention later or forgets it sooner.",
    ),
];

/// The catalog sentence for `code`.
pub fn reason_text(code: &str) -> Option<&'static str> {
    REASON_CODES
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, text)| *text)
}

/// Whether new own work may start on a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Admission {
    Open,
    /// Waiting for a measurement or a step to settle; retried every interval.
    Held {
        /// A catalog code.
        #[serde(default)]
        code: String,
        reason: String,
    },
    /// Refused until the cause changes, such as a floor or the node cap.
    Blocked {
        #[serde(default)]
        code: String,
        reason: String,
    },
}

impl Admission {
    /// Held for a catalog code, with the catalog sentence.
    pub fn held(code: &str) -> Self {
        Admission::Held {
            code: code.into(),
            reason: reason_text(code).unwrap_or(code).into(),
        }
    }

    /// Blocked for a catalog code, with the catalog sentence.
    pub fn blocked(code: &str) -> Self {
        Admission::Blocked {
            code: code.into(),
            reason: reason_text(code).unwrap_or(code).into(),
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Admission::Open => None,
            Admission::Held { code, .. } | Admission::Blocked { code, .. } => Some(code),
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Admission::Open => None,
            Admission::Held { reason, .. } | Admission::Blocked { reason, .. } => Some(reason),
        }
    }
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
/// Every value is a configuration setting; the defaults are provisional. Built only
/// from `defaults` and configuration, never deserialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceThresholds {
    pub yield_first_margin_percent: u8,
    /// Compute: what Neutral leaves free while nothing is contended.
    pub neutral_margin_percent: u8,
    pub performance_margin_percent: u8,
    pub yield_first_response_intervals: u32,
    /// Configured scale-up interval; `None` uses the mode's default.
    pub scale_up_step_interval_s: Option<u64>,
    pub neutral_convergence_s: u64,
    pub neutral_tolerance_points: u8,
    pub performance_ramp_s: u64,
    pub performance_ramp_percent: u8,
    /// How long a Neutral or Performance compute step has to free the contended
    /// amount before the next step; memory moves on at the hint deadline.
    pub reclaim_s: u64,
    pub hint_deadline_s: u64,
    pub unknown_window_intervals: u32,
    pub hysteresis_percent: u8,
    pub min_step_dwell_intervals: u32,
    pub pause_bound_s: u64,
    pub noise_tolerance_mib: u64,
    pub quiet_window_intervals: u32,
    /// How long CPU and RAM must stay stable before own work grows on them;
    /// `None` uses the mode's scale-up interval.
    pub scale_up_cooldown_ms: Option<u64>,
    /// Usage, in percent of capacity, from which a principal contends for a
    /// fair share; smaller usage is counted as it is.
    pub principal_min_percent: u8,
    /// RAM only: how long a floor breach must last before Guaranteed work is
    /// drained for it (FR-024).
    pub guaranteed_breach_window_s: u64,
    /// Memory: admission headroom uses the external peak over this window (FR-060).
    pub memory_peak_window_s: u64,
    /// CPU, GPU compute: busy at or above this share of capacity is saturated.
    pub saturation_percent: u8,
    /// CPU: external PSI `some` avg10 above this also saturates the CPU (FR-072).
    pub psi_some_percent: u8,
    /// Memory: the extra cooldown after a repeated reclaim episode, doubling up to
    /// the maximum (FR-061).
    pub reclaim_backoff_ms: u64,
    pub reclaim_backoff_max_ms: u64,
    /// CPU: running opportunistic work is charged for admission at its p95 usage
    /// over this window (FR-034).
    pub admission_usage_window_s: u64,
    /// GPU compute: summed per-process utilization within this many points of
    /// device utilization makes the external amount measured (FR-021).
    pub attribution_tolerance_percent: u8,
    /// GPU compute, the time-slice duty: an external principal reading at least this share of
    /// its slice is slice-bound (FR-021, Q27).
    pub slice_bound_percent: u8,
    /// GPU compute, the time-slice duty: the points of duty one step moves (FR-021, Q28).
    pub slice_step_percent: u8,
    /// CPU and GPU compute, which Performance scales up every monitor interval;
    /// memory keeps its slower scale-up (FR-034). Set from the resource kind.
    pub compute: bool,
}

impl ResourceThresholds {
    /// Provisional defaults from the spec, to validate on real hardware.
    pub fn defaults(kind: ResourceKind) -> Self {
        // Compute is reclaimed within an interval, so its idle margins are small;
        // memory keeps the floors, and Yield First keeps 20% more.
        Self {
            yield_first_margin_percent: if kind.is_memory() { 20 } else { 15 },
            neutral_margin_percent: 5,
            performance_margin_percent: 2,
            yield_first_response_intervals: 3,
            scale_up_step_interval_s: None,
            neutral_convergence_s: 60,
            neutral_tolerance_points: 15,
            performance_ramp_s: 10,
            performance_ramp_percent: 90,
            reclaim_s: 5,
            hint_deadline_s: 5,
            unknown_window_intervals: 3,
            hysteresis_percent: 50,
            min_step_dwell_intervals: 3,
            pause_bound_s: 10,
            noise_tolerance_mib: 256,
            quiet_window_intervals: 20,
            scale_up_cooldown_ms: None,
            principal_min_percent: 1,
            guaranteed_breach_window_s: if kind == ResourceKind::Ram { 10 } else { 0 },
            memory_peak_window_s: 300,
            saturation_percent: 98,
            psi_some_percent: 10,
            reclaim_backoff_ms: 1000,
            reclaim_backoff_max_ms: 30_000,
            admission_usage_window_s: 60,
            attribution_tolerance_percent: 15,
            slice_bound_percent: 75,
            slice_step_percent: 5,
            compute: !kind.is_memory(),
        }
    }

    /// The mode's idle margin of capacity, in percent.
    pub fn margin_percent(&self, mode: ContentionMode) -> u8 {
        match mode {
            ContentionMode::YieldFirst => self.yield_first_margin_percent,
            ContentionMode::Neutral => self.neutral_margin_percent,
            ContentionMode::Performance => self.performance_margin_percent,
            // A constant, not a setting: the constitution fixes Aggressive's margin at 0%.
            ContentionMode::Aggressive => 0,
        }
    }

    /// The least time between two scale-up moves: 30 s in Yield First, 10 s in
    /// Neutral, and in Performance one monitor interval on compute but 2 s on
    /// memory, which is slow to reclaim. A set `scale_up_step_interval_s` applies
    /// to every mode.
    pub fn scale_up_interval_ms(&self, mode: ContentionMode, interval_ms: u64) -> u64 {
        if let Some(seconds) = self.scale_up_step_interval_s {
            return seconds.saturating_mul(1000);
        }
        match mode {
            ContentionMode::YieldFirst => 30_000,
            ContentionMode::Neutral => 10_000,
            ContentionMode::Performance if self.compute => interval_ms,
            // Aggressive fills idle compute at once; memory paces as Performance.
            ContentionMode::Aggressive if self.compute => 0,
            ContentionMode::Performance | ContentionMode::Aggressive => 2000,
        }
    }

    /// A resource's scale-up cooldown for a mode (FR-034), independent of GPU
    /// settings: 30 s in Yield First, as 0.2's cooldown, and faster otherwise.
    pub fn cooldown_ms(&self, mode: ContentionMode, interval_ms: u64) -> u64 {
        self.scale_up_cooldown_ms
            .unwrap_or_else(|| self.scale_up_interval_ms(mode, interval_ms))
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

/// Applied ladder state for one allocation and resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepState {
    pub step: YieldStep,
    /// When the current step was entered; every step change waits the dwell time.
    pub entered_at_ms: u64,
    pub cause: YieldCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
    /// External demand when the step was entered; reversal compares against it.
    pub trigger_external: u64,
    /// Since when demand has stayed below the reversal threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calm_since_ms: Option<u64>,
    /// Since when the resource has been contended without a break.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contended_since_ms: Option<u64>,
    /// A cap that ended a pause at its bound (FR-014); it does not become a pause
    /// again while the contention lasts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_pause: bool,
    /// What the step had to free when it began; a floor-breach step reverses only
    /// while the floor has that much room (FR-013).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_free: Option<u64>,
    /// Planned on the pending pass, for a reservation that had not started; a CPU drain from it
    /// is re-planned once the allocation runs (FR-012).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// The target a memory hint was entered with; compliance at its deadline is judged against
    /// the larger of it and the current target (FR-037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_target: Option<u64>,
}

/// What the planner needs to know about one allocation and resource this interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderInput {
    pub kind: ResourceKind,
    pub guaranteed: bool,
    /// False for Guaranteed work unless a floor policy permits its drain.
    pub allow_drain: bool,
    /// Steps the backend or host cannot apply to this allocation.
    pub unavailable: std::collections::BTreeSet<YieldStep>,
    /// What own work still has to release on this resource; zero when nothing is contended.
    pub excess: u64,
    pub external: u64,
    pub measurement_known: bool,
    pub cause: YieldCause,
    /// What the allocation may use this interval, in resource units: the cap or
    /// hint target while contended, and the level a held cap may rise to while calm.
    pub target: Option<u64>,
    /// The allocation's measured usage of the resource, when known.
    pub usage: Option<u64>,
    /// The allocation's reading of a memory resource, for a hint's compliance (FR-037); `None`
    /// for CPU and GPU compute.
    pub memory_usage: Option<u64>,
    /// Free memory above the floor, for a memory resource, when known.
    pub room: Option<u64>,
}

/// Mode-dependent ladder timing in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderTiming {
    /// How long a step has to free the contended amount before the next one.
    pub response_ms: u64,
    pub dwell_ms: u64,
    /// The mode's scale-up rate: the least time between two reversals.
    pub scale_up_ms: u64,
    pub pause_bound_ms: u64,
    /// How long a hint waits for compliance before the next step (FR-037).
    pub hint_deadline_ms: u64,
    pub hysteresis_percent: u8,
    /// The least room above the floor a floor-breach step reverses with.
    pub reversal_room_min: u64,
    /// The reclaim backoff a floor-breach step's reversal also waits (FR-061).
    pub breach_backoff_ms: u64,
    /// The first interval CPU contention has ended: a held step's rise to the return level is
    /// one move and does not wait for the dwell (FR-034, R44).
    pub returning: bool,
}

impl LadderTiming {
    pub fn new(thresholds: &ResourceThresholds, mode: ContentionMode, interval_ms: u64) -> Self {
        let interval_ms = interval_ms.max(1);
        let response_ms = match mode {
            ContentionMode::YieldFirst => {
                u64::from(thresholds.yield_first_response_intervals).saturating_mul(interval_ms)
            }
            ContentionMode::Neutral | ContentionMode::Performance | ContentionMode::Aggressive => {
                thresholds.reclaim_s.saturating_mul(1000)
            }
        };
        Self {
            response_ms,
            dwell_ms: u64::from(thresholds.min_step_dwell_intervals).saturating_mul(interval_ms),
            scale_up_ms: thresholds.scale_up_interval_ms(mode, interval_ms),
            pause_bound_ms: thresholds.pause_bound_s.saturating_mul(1000),
            hint_deadline_ms: thresholds.hint_deadline_s.saturating_mul(1000),
            hysteresis_percent: thresholds.hysteresis_percent,
            reversal_room_min: thresholds.noise_tolerance_mib,
            breach_backoff_ms: 0,
            returning: false,
        }
    }
}

/// The planner's result: the new state (None once nothing is applied and the dwell
/// time has passed) and the steps it skipped, which must be reported (FR-011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderDecision {
    pub state: Option<StepState>,
    pub skipped: Vec<YieldStep>,
}

const LADDER: [YieldStep; 5] = [
    YieldStep::LowerShare,
    YieldStep::Cap,
    YieldStep::Pause,
    YieldStep::Hint,
    YieldStep::Drain,
];

impl LadderInput {
    /// Whether a step can apply here. Memory is never paused or given a lower
    /// share; RAM may take a `memory.high` cap where a delegated leaf allows it,
    /// GPU memory starts at the hint. Guaranteed work is never paused, so it takes
    /// no GPU compute cap either, which is a pause duty cycle, and is drained only
    /// when a floor policy allows it.
    fn applies(&self, step: YieldStep) -> bool {
        !(self.kind.is_memory() && matches!(step, YieldStep::LowerShare | YieldStep::Pause)
            || self.kind == ResourceKind::GpuMemory && step == YieldStep::Cap
            || self.guaranteed && step == YieldStep::Pause
            || self.guaranteed && self.kind == ResourceKind::GpuCompute && step == YieldStep::Cap
            || step == YieldStep::Drain && !self.allow_drain)
    }

    fn next_step(&self, current: YieldStep) -> (Option<YieldStep>, Vec<YieldStep>) {
        let mut skipped = Vec::new();
        for step in LADDER.into_iter().filter(|step| *step > current) {
            if !self.applies(step) {
                continue;
            }
            if self.unavailable.contains(&step) {
                skipped.push(step);
                continue;
            }
            // A cap to zero for external demand still runs at the minimum duty; a
            // pause frees it all. Unknown measurement keeps the gentler cap first.
            if step == YieldStep::Cap
                && self.cause == YieldCause::ExternalGrowth
                && self.target == Some(0)
                && self.applies(YieldStep::Pause)
                && !self.unavailable.contains(&YieldStep::Pause)
            {
                continue;
            }
            return (Some(step), skipped);
        }
        (None, skipped)
    }

    fn previous_step(&self, current: YieldStep) -> YieldStep {
        LADDER
            .into_iter()
            .rev()
            .filter(|step| *step < current)
            .find(|step| self.applies(*step) && !self.unavailable.contains(step))
            .unwrap_or(YieldStep::None)
    }
}

/// Whether a held hint's wait is over, on the clock its step escalates by: the dwell since its
/// last change, and its deadline since the later of that change and the need's return (FR-037).
/// The ladder counts an over-share hint by the same clock, so a hint whose wait starts again
/// with the need counts what it was asked through that wait (FR-023).
pub fn hint_wait_over(state: &StepState, timing: &LadderTiming, now_ms: u64) -> bool {
    let contended_since = state.contended_since_ms.unwrap_or(now_ms);
    let anchor = state.entered_at_ms.max(contended_since);
    now_ms.saturating_sub(state.entered_at_ms) >= timing.dwell_ms
        && now_ms.saturating_sub(anchor) >= timing.hint_deadline_ms
}

/// Pure ladder transition for one allocation and resource (data-model.md YieldStep).
pub fn plan_step(
    state: Option<&StepState>,
    input: &LadderInput,
    timing: &LadderTiming,
    now_ms: u64,
) -> LadderDecision {
    let hold = |skipped| LadderDecision {
        state: state.cloned(),
        skipped,
    };
    // Unknown measurement holds the current step (FR-008, FR-017).
    if !input.measurement_known {
        return hold(Vec::new());
    }
    let current = state.map_or(YieldStep::None, |s| s.step);
    // Drain and terminate never reverse; the drain chain owns them now.
    if current >= YieldStep::Drain {
        return hold(Vec::new());
    }
    let since_change = state.map_or(u64::MAX, |s| now_ms.saturating_sub(s.entered_at_ms));
    let enter = |step: YieldStep, skipped: Vec<YieldStep>| LadderDecision {
        state: Some(StepState {
            step,
            entered_at_ms: now_ms,
            cause: input.cause,
            target: matches!(step, YieldStep::Cap | YieldStep::Hint)
                .then_some(input.target)
                .flatten(),
            trigger_external: input.external,
            calm_since_ms: None,
            contended_since_ms: Some(now_ms),
            after_pause: false,
            to_free: (input.excess > 0).then_some(input.excess),
            pending: false,
            asked_target: (step == YieldStep::Hint && input.kind.is_memory())
                .then_some(input.target)
                .flatten(),
        }),
        skipped,
    };
    // A pause ends at its bound (FR-014): at the next step when one applies, else as
    // a cap at the minimum duty that does not become a pause again.
    let end_pause = || -> Option<LadderDecision> {
        let (next, skipped) = input.next_step(YieldStep::Pause);
        if let Some(next) = next {
            return Some(enter(next, skipped));
        }
        if !input.applies(YieldStep::Cap) || input.unavailable.contains(&YieldStep::Cap) {
            return None;
        }
        let mut decision = enter(YieldStep::Cap, skipped);
        if let Some(capped) = decision.state.as_mut() {
            capped.target = Some(input.target.unwrap_or(0));
            capped.after_pause = true;
        }
        Some(decision)
    };
    if input.excess > 0 {
        let contended_since = state.and_then(|s| s.contended_since_ms).unwrap_or(now_ms);
        let anchor = state.map_or(now_ms, |s| s.entered_at_ms.max(contended_since));
        let persisted = now_ms.saturating_sub(anchor);
        // The first step applies at once; later ones only when the current step has
        // not freed the contended amount within the response time, a pause its
        // bound or a hint its deadline. A step the supervisor reported it cannot
        // apply moves on at once.
        // A held cap whose target drops to zero becomes a pause at once.
        let cap_to_zero = current == YieldStep::Cap
            && input.cause == YieldCause::ExternalGrowth
            && input.target == Some(0)
            && input.applies(YieldStep::Pause)
            && !input.unavailable.contains(&YieldStep::Pause);
        let pause_bound = current == YieldStep::Pause && persisted >= timing.pause_bound_ms;
        // The cap that ended a pause is the last CPU step for a worker without a
        // hint, as the hint is for one with it: it never pauses again (FR-014).
        let after_pause = state.is_some_and(|s| s.after_pause);
        // A cap whose usage follows its target is working; excess left while the
        // target moves down, as in Performance and probing, is no reason to go on.
        let cap_effective = current == YieldStep::Cap
            && state
                .and_then(|s| s.target)
                .zip(input.usage)
                .is_some_and(|(target, usage)| {
                    usage.saturating_mul(100) <= target.saturating_mul(105)
                });
        // A hint waits for its deadline, never just the response time (FR-037).
        let due = since_change >= timing.dwell_ms
            && (current == YieldStep::None
                || current != YieldStep::Hint && !cap_effective && persisted >= timing.response_ms
                || pause_bound
                || current == YieldStep::Hint
                    && state.is_some_and(|s| hint_wait_over(s, timing, now_ms)))
            || input.unavailable.contains(&current)
            || cap_to_zero;
        // A memory hint met at its deadline is entered again with the current target and a new
        // deadline; only a task above what it was asked goes on (FR-037).
        if due
            && current == YieldStep::Hint
            && input.kind.is_memory()
            && !input.unavailable.contains(&YieldStep::Hint)
            && let Some(state) = state
            && hint_wait_over(state, timing, now_ms)
            && let (Some(usage), Some(target)) = (input.memory_usage, input.target)
            && usage <= state.asked_target.unwrap_or(target).max(target)
        {
            return enter(YieldStep::Hint, Vec::new());
        }
        if due && !after_pause {
            let (next, skipped) = input.next_step(current);
            if let Some(next) = next {
                return enter(next, skipped);
            }
            if pause_bound && let Some(decision) = end_pause() {
                return decision;
            }
            if current == YieldStep::None {
                return LadderDecision {
                    state: None,
                    skipped,
                };
            }
        }
        let Some(state) = state else {
            return hold(Vec::new());
        };
        let mut held = state.clone();
        held.calm_since_ms = None;
        held.contended_since_ms = Some(contended_since);
        // A step held from earlier, such as a margin cap, now answers this demand.
        held.trigger_external = held.trigger_external.max(input.external);
        held.to_free = held.to_free.max(Some(input.excess));
        if matches!(held.step, YieldStep::Cap | YieldStep::Hint) && input.target.is_some() {
            held.target = input.target;
        }
        return LadderDecision {
            state: Some(held),
            skipped: Vec::new(),
        };
    }
    let Some(state) = state else {
        return hold(Vec::new());
    };
    let threshold = state
        .trigger_external
        .saturating_mul(u64::from(timing.hysteresis_percent))
        / 100;
    let mut held = state.clone();
    held.contended_since_ms = None;
    // Where no cap applies, a pause held for external growth ends in the interval contention has
    // ended, as a cap would rise there (FR-034, FR-089).
    if current == YieldStep::Pause
        && timing.returning
        && state.cause == YieldCause::ExternalGrowth
        && (!input.applies(YieldStep::Cap) || input.unavailable.contains(&YieldStep::Cap))
    {
        held.step = input.previous_step(current);
        held.entered_at_ms = now_ms;
        held.target = None;
        held.calm_since_ms = None;
        return LadderDecision {
            state: Some(held),
            skipped: Vec::new(),
        };
    }
    // Paused work uses nothing, so a pause is seldom contended; its bound still holds.
    if current == YieldStep::Pause
        && since_change >= timing.pause_bound_ms
        && let Some(decision) = end_pause()
    {
        return decision;
    }
    // While nothing is contended, a held cap or hint rises toward what the mode
    // allows now, one move per scale-up interval, and a pause becomes such a cap.
    if let Some(allowed) = input.target
        && (timing.returning || since_change >= timing.dwell_ms.max(timing.scale_up_ms))
    {
        let raise = match current {
            YieldStep::Cap | YieldStep::Hint => held.target.is_some_and(|target| allowed > target),
            YieldStep::Pause => {
                allowed > 0
                    && input.applies(YieldStep::Cap)
                    && !input.unavailable.contains(&YieldStep::Cap)
            }
            _ => false,
        };
        if raise {
            if current == YieldStep::Pause {
                held.step = YieldStep::Cap;
            }
            held.target = Some(allowed);
            held.entered_at_ms = now_ms;
            held.calm_since_ms = None;
            return LadderDecision {
                state: Some(held),
                skipped: Vec::new(),
            };
        }
    }
    // A cap that still binds is what keeps own work at the mode's target.
    let binding = current == YieldStep::Cap
        && held
            .target
            .zip(input.usage)
            .is_some_and(|(target, usage)| usage.saturating_mul(100) >= target.saturating_mul(95));
    if binding {
        held.calm_since_ms = None;
        return LadderDecision {
            state: Some(held),
            skipped: Vec::new(),
        };
    }
    // Only a step that answered external growth waits for that demand to fall;
    // one that answered a breach reverses once calm, or admission stays closed.
    if state.cause == YieldCause::ExternalGrowth
        && state.trigger_external > 0
        && input.external >= threshold
    {
        held.calm_since_ms = None;
        return LadderDecision {
            state: Some(held),
            skipped: Vec::new(),
        };
    }
    // A floor-breach step waits for room above the floor for what it had to free,
    // and its reversal also waits the episode's reclaim backoff, so work that
    // regrows into the floor backs off instead of cycling.
    let breach = state.cause == YieldCause::FloorBreach && input.kind.is_memory();
    if breach && current != YieldStep::None {
        let needed = state.to_free.unwrap_or(0).max(timing.reversal_room_min);
        if input.room.is_none_or(|room| room < needed) {
            held.calm_since_ms = None;
            return LadderDecision {
                state: Some(held),
                skipped: Vec::new(),
            };
        }
    }
    let backoff_ms = if breach { timing.breach_backoff_ms } else { 0 };
    let calm_since = state.calm_since_ms.unwrap_or(now_ms);
    held.calm_since_ms = Some(calm_since);
    if current == YieldStep::None {
        // Forget the entry once the dwell time after the last change has passed.
        return LadderDecision {
            state: (since_change < timing.dwell_ms).then_some(held),
            skipped: Vec::new(),
        };
    }
    // Reverse one level per scale-up interval after calm has lasted the dwell time.
    if now_ms.saturating_sub(calm_since) >= timing.dwell_ms.saturating_add(backoff_ms)
        && since_change
            >= timing
                .dwell_ms
                .max(timing.scale_up_ms)
                .saturating_add(backoff_ms)
    {
        let step = input.previous_step(current);
        held.step = step;
        held.entered_at_ms = now_ms;
        if !matches!(step, YieldStep::Cap | YieldStep::Hint) {
            held.target = None;
        }
    }
    LadderDecision {
        state: Some(held),
        skipped: Vec::new(),
    }
}

/// Uncapped intervals of own work from which its uncapped share is recorded (FR-021, Q24).
pub const GPU_OWN_SHARE_INTERVALS: usize = 6;

/// Facts about one candidate for victim selection (FR-016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VictimFacts {
    pub id: String,
    /// False for allocations nobody can act on: needing reconciliation, still
    /// preparing, or with a lost supervisor.
    pub controllable: bool,
    /// Whether reducing this allocation frees the contended resource.
    pub frees: bool,
    pub pending: bool,
    /// The job's effective mode on the resource: its yield tier.
    pub mode: ContentionMode,
    pub guaranteed: bool,
    /// Time since the last accepted checkpoint, else since start.
    pub progress_lost_ms: u64,
    /// What this allocation would release on the contended resource.
    pub amount: u64,
}

/// Order candidates by least cost and drop uncontrollable ones: freeing the
/// resource, pending before running, then by tier (Yield First, Neutral,
/// Performance), and within a tier opportunistic before Guaranteed, least
/// progress lost, best fit for `needed`, then assignment ID.
pub fn victim_order(candidates: &mut Vec<VictimFacts>, needed: u64) {
    candidates.retain(|c| c.controllable);
    candidates.sort_by(|a, b| {
        let fit = |c: &VictimFacts| {
            if c.amount >= needed {
                (0, c.amount)
            } else {
                (1, u64::MAX - c.amount)
            }
        };
        (
            !a.frees,
            !a.pending,
            a.mode,
            a.guaranteed,
            a.progress_lost_ms,
            fit(a),
            &a.id,
        )
            .cmp(&(
                !b.frees,
                !b.pending,
                b.mode,
                b.guaranteed,
                b.progress_lost_ms,
                fit(b),
                &b.id,
            ))
    });
}

/// One requested step for one resource in an `adjust` control message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepRequest {
    pub step: YieldStep,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
    pub cause: YieldCause,
    /// For a hint, how long the worker has before the next step, as the policy
    /// enforces it (FR-037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// The steps a supervisor applied, journaled so a restarted agent can learn them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllocationContention {
    /// The highest `adjust` sequence applied; older messages are ignored.
    pub sequence: u64,
    pub applied: std::collections::BTreeMap<ResourceKey, StepRequest>,
}

/// One interval's view of a resource for the mode targets (FR-020 to FR-022).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeInput {
    pub kind: ResourceKind,
    pub mode: ContentionMode,
    /// Schedulable capacity after operator reserves and, for memory, the floor.
    pub capacity: u64,
    /// Usage within that capacity that is not own work.
    pub external: u64,
    /// Own work's usage, its lower bound where only bounds are known.
    pub own: u64,
    /// Unmanaged usage per principal, the owner included; `None` when unobserved.
    /// Users that are not principals yet are left out and count as unattributed.
    pub principals: Option<Vec<(Principal, u64)>>,
    /// Usage by system accounts (below `uid_min`): background demand that comes
    /// off the capacity and never contends (FR-059).
    pub background: u64,
    /// Usage at capacity hides how much more external work wants (CPU).
    pub saturated: bool,
    /// Saturated by external PSI alone, with utilization below saturation: every
    /// contending principal is probed, not only small demands (FR-072).
    pub psi_only: bool,
    /// Activity of unknown amount, such as unsampled GPU compute.
    pub unknown_activity: bool,
    /// Principals whose demand probing found satisfied, by UID (`None`: the
    /// unattributed principal), with that demand: observed usage plus hysteresis.
    pub satisfied: Vec<(Option<u32>, u64)>,
    /// While a small external demand is probed, the allowance stepping down by the
    /// mode's margin per response window (FR-021).
    pub probe: Option<u64>,
    /// While the principal list cannot be read, the last fair share a readable list gave,
    /// which the share never exceeds (FR-059).
    pub fair_share_limit: Option<u64>,
    /// The allocation runs under an unprotected Aggressive grant (004 R5): on compute, with no
    /// unknown activity, admission may take the whole capacity.
    pub unprotected: bool,
}

/// Work-conserving max-min shares over current demand (research R12). `None`
/// demands without bound. The remainder of an uneven split goes to the first
/// principals in order.
pub fn max_min_shares(capacity: u64, demands: &[Option<u64>]) -> Vec<u64> {
    let mut shares = vec![0; demands.len()];
    let mut open: Vec<usize> = (0..demands.len()).collect();
    let mut remaining = capacity;
    while !open.is_empty() {
        let level = remaining / open.len() as u64;
        let (satisfied, rest): (Vec<usize>, Vec<usize>) = open
            .iter()
            .partition(|i| demands[**i].is_some_and(|demand| demand <= level));
        if satisfied.is_empty() {
            let extra = remaining % open.len() as u64;
            for (n, i) in open.iter().enumerate() {
                shares[*i] = level + u64::from((n as u64) < extra);
            }
            break;
        }
        for i in satisfied {
            let demand = demands[i].unwrap_or(0);
            shares[i] = demand;
            remaining -= demand;
        }
        open = rest;
    }
    shares
}

/// What a mode lets own work hold on one resource this interval (FR-020 to
/// FR-022, FR-065). On contended compute, Yield First takes opportunistic work to
/// zero, Neutral holds the fair share and Performance keeps what external demand
/// does not take, never less than the fair share. Otherwise each keeps its own
/// margin free, 15%, 5% and 2% of compute by default, validated as YF ≥ N ≥ P, so
/// the targets are ordered YF ≤ N ≤ P on every resource.
/// Memory floors are already outside `capacity`, and memory keeps no margin
/// beyond them except Yield First's.
pub fn own_allowance(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    let margin = |percent: u8| input.capacity.saturating_mul(u64::from(percent)) / 100;
    let available = input
        .capacity
        .saturating_sub(expected_external(input, thresholds));
    let contended = contended(input, thresholds);
    let yield_first_margin = margin(thresholds.yield_first_margin_percent);
    let probe = input.probe.filter(|_| !input.unknown_activity);
    // Unprotected Aggressive starts where others contend, never into activity of unknown amount
    // or past the floors, which are already outside `capacity` (004 FR-010).
    if input.unprotected
        && input.mode == ContentionMode::Aggressive
        && !input.kind.is_memory()
        && !input.unknown_activity
    {
        return input.capacity;
    }
    match input.mode {
        // While a small demand is probed, Yield First steps down with the probe.
        ContentionMode::YieldFirst if contended && probe.is_some() => available
            .saturating_sub(yield_first_margin)
            .min(probe.unwrap_or(0)),
        // Own work holding more than capacity minus the margin may cause the
        // saturation itself: it caps to the margin, within the fair share, first.
        ContentionMode::YieldFirst
            if contended
                && !input.unknown_activity
                && input.own > input.capacity.saturating_sub(yield_first_margin) =>
        {
            available
                .saturating_sub(yield_first_margin)
                .min(neutral_share(input, thresholds))
        }
        ContentionMode::YieldFirst if contended => 0,
        ContentionMode::YieldFirst => available.saturating_sub(yield_first_margin),
        _ if input.kind.is_memory() => available,
        // Performance gives back what external demand takes, never below the fair
        // share; activity of unknown amount holds it to the share.
        ContentionMode::Performance | ContentionMode::Aggressive
            if contended && !input.unknown_activity =>
        {
            neutral_share(input, thresholds)
                .max(available.saturating_sub(margin(thresholds.margin_percent(input.mode))))
                .max(probe.unwrap_or(0))
        }
        _ if contended => neutral_share(input, thresholds).max(probe.unwrap_or(0)),
        ContentionMode::Neutral => {
            available.saturating_sub(margin(thresholds.neutral_margin_percent))
        }
        ContentionMode::Performance | ContentionMode::Aggressive => {
            available.saturating_sub(margin(thresholds.margin_percent(input.mode)))
        }
    }
}

/// External usage with room for a principal whose demand is on record as satisfied but who
/// uses clearly less of a saturated resource: it may be starved, as when it comes back, so its
/// recorded demand counts and it is served within SC-002's bound.
fn expected_external(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    if !input.saturated || input.kind.is_memory() {
        return input.external;
    }
    let floor = principal_floor(input, thresholds);
    contending_principals(input, thresholds).into_iter().fold(
        input.external,
        |external, (uid, usage)| match satisfied_demand(input, uid, usage) {
            Some(demand) if usage.saturating_add(floor) < demand => {
                external.saturating_add(demand - usage)
            }
            _ => external,
        },
    )
}

/// The contention floor: usage, in resource units, from which a principal
/// contends for a fair share.
fn principal_floor(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    (input
        .capacity
        .saturating_mul(u64::from(thresholds.principal_min_percent))
        / 100)
        .max(1)
}

/// The usage from which unattributed usage stands for a principal: the contention floor, on
/// GPU compute the attribution tolerance, below which a gap between the device and its
/// per-process samples is sampling error the tolerance already accepts (FR-021, Q23).
fn unattributed_floor(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    let floor = principal_floor(input, thresholds);
    if input.kind == ResourceKind::GpuCompute {
        floor.max(
            input
                .capacity
                .saturating_mul(u64::from(thresholds.attribution_tolerance_percent))
                / 100,
        )
    } else {
        floor
    }
}

/// External principals using at least the contention floor, by UID (`None`: the
/// unattributed usage that stands for one, from `unattributed_floor`), with their usage.
pub fn contending_principals(
    input: &ModeInput,
    thresholds: &ResourceThresholds,
) -> Vec<(Option<u32>, u64)> {
    let floor = principal_floor(input, thresholds);
    let mut result = Vec::new();
    let mut accounted = input.background;
    for (principal, usage) in input.principals.iter().flatten() {
        accounted = accounted.saturating_add(*usage);
        if !principal.is_owner && *usage >= floor {
            result.push((principal.uid, *usage));
        }
    }
    let unattributed = input.external.saturating_sub(accounted);
    if unattributed >= unattributed_floor(input, thresholds) {
        result.push((None, unattributed));
    }
    result
}

/// The demand probing found satisfied for a principal, while its usage stays
/// within it.
fn satisfied_demand(input: &ModeInput, uid: Option<u32>, usage: u64) -> Option<u64> {
    input
        .satisfied
        .iter()
        .find(|(principal, demand)| *principal == uid && usage <= *demand)
        .map(|(_, demand)| *demand)
}

/// Compute is contended when external activity of unknown amount exists, or when
/// the resource is saturated and an external principal, or the unattributed
/// usage that stands for one, uses at least the contention floor without its
/// demand being known to be satisfied.
pub fn contended(input: &ModeInput, thresholds: &ResourceThresholds) -> bool {
    if input.kind.is_memory() {
        return false;
    }
    if input.unknown_activity {
        return true;
    }
    input.saturated
        && contending_principals(input, thresholds)
            .into_iter()
            .any(|(uid, usage)| satisfied_demand(input, uid, usage).is_none())
}

/// Whether own GPU work may be admitted to a device now (FR-026, FR-027, FR-064).
/// The policy and the launch recheck both call it, so they cannot disagree.
/// `external_contexts` is `None` when the process list is unknown, and `contexts` when the
/// device's compute contexts are.
pub fn gpu_admission(
    mode: ContentionMode,
    activity: crate::model::ComputeActivity,
    contexts: Option<&crate::model::GpuComputeContexts>,
    external_contexts: Option<usize>,
    memory_known: bool,
    never_share: bool,
) -> Admission {
    use crate::model::ComputeActivity::*;
    if !memory_known {
        return Admission::blocked("gpu_memory_unknown");
    }
    if never_share && external_contexts != Some(0) {
        return Admission::blocked("gpu_never_share_occupied");
    }
    match (mode, activity) {
        // The owner's own compute is the owner's share, not activity to wait out (FR-027).
        (ContentionMode::YieldFirst, Active) if owners_compute_only(contexts) => Admission::Open,
        (ContentionMode::YieldFirst, Active) => {
            Admission::held("gpu_yield_first_needs_quiet_device")
        }
        (ContentionMode::YieldFirst, Unknown) => {
            Admission::held("gpu_yield_first_activity_unknown")
        }
        // Neutral and Performance count activity of unknown amount as one principal.
        _ => Admission::Open,
    }
}

/// Whether the device's compute activity this interval is only the owner's unmanaged processes:
/// the samples were read, each busy context that is not own work is the owner's, and every other
/// account's listed context has been quiet for the window, which the owner's activity neither
/// breaks nor restarts. A context whose owner cannot be read, listed with no user ID, holds
/// admission (FR-027).
fn owners_compute_only(contexts: Option<&crate::model::GpuComputeContexts>) -> bool {
    contexts.is_some_and(|contexts| {
        let mut busy = contexts
            .sampled
            .iter()
            .filter(|(pid, _)| !contexts.own.contains(pid))
            .peekable();
        let others_quiet = contexts.listed.is_empty()
            || (contexts.others_quiet && contexts.listed.iter().all(|(_, uid)| uid.is_some()));
        contexts.samples_readable
            && others_quiet
            && busy.peek().is_some()
            && busy.all(|(pid, _)| contexts.owner.contains(pid))
    })
}

/// A probe of small external demand on one compute resource (FR-021).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DemandProbe {
    /// The allowance while probing.
    pub target: u64,
    /// When the target last stepped down.
    pub stepped_at_ms: u64,
    /// The probed principals' usage when it did.
    pub usage_at_step: BTreeMap<Option<u32>, u64>,
    /// A larger demand that got the fair share at once: own work keeps that share
    /// while the probe watches whether the demand grows into what it was given.
    pub watch: bool,
    /// Whether a watch has taken its baseline, the usage once the share applied.
    pub baselined: bool,
}

/// Satisfied records kept per resource key (FR-021).
pub const SATISFIED_RECORD_LIMIT: usize = 64;

/// Records never expire, but at most `SATISFIED_RECORD_LIMIT` are kept: past the
/// bound, those whose principals were seen longest ago go first, and a principal
/// never seen counts as the oldest.
pub fn bound_satisfied(
    records: &mut Vec<(Option<u32>, u64)>,
    last_seen: &BTreeMap<Option<u32>, u64>,
) {
    if records.len() <= SATISFIED_RECORD_LIMIT {
        return;
    }
    records.sort_by_key(|(uid, _)| std::cmp::Reverse(last_seen.get(uid).copied()));
    records.truncate(SATISFIED_RECORD_LIMIT);
    records.sort();
}

/// One interval of demand probing on a compute resource. Under saturation, a
/// contending principal whose demand is unknown and smaller than one margin step
/// is probed: own work steps down by the mode's margin per response window, and a
/// principal whose usage does not grow into the freed capacity is recorded as
/// satisfied at its usage plus hysteresis (the contention floor). A larger demand
/// gets the fair share at once and is watched for one response window from the
/// interval after: if the host then has capacity to spare and its usage stayed
/// within its baseline plus hysteresis, it is recorded the same way. A record holds
/// while usage stays within it. Returns the satisfied records and the probe that
/// continues, if any.
pub fn probe_demand(
    input: &ModeInput,
    thresholds: &ResourceThresholds,
    probe: Option<&DemandProbe>,
    response_ms: u64,
    now_ms: u64,
) -> (Vec<(Option<u32>, u64)>, Option<DemandProbe>) {
    let floor = principal_floor(input, thresholds);
    let usage_of = |uid: Option<u32>| -> Option<u64> {
        let mut accounted = input.background;
        let mut found = None;
        for (principal, usage) in input.principals.iter().flatten() {
            accounted = accounted.saturating_add(*usage);
            if !principal.is_owner && uid.is_some() && principal.uid == uid {
                found = Some(*usage);
            }
        }
        match uid {
            Some(_) => found,
            None => Some(input.external.saturating_sub(accounted)),
        }
    };
    // A record holds while usage stays within it; a principal no longer listed
    // uses nothing.
    let mut satisfied: Vec<(Option<u32>, u64)> = input
        .satisfied
        .iter()
        .filter(|(uid, demand)| usage_of(*uid).unwrap_or(0) <= *demand)
        .copied()
        .collect();
    if input.kind.is_memory() || input.unknown_activity {
        return (satisfied, None);
    }
    let percent = thresholds.margin_percent(input.mode);
    let step = (input.capacity.saturating_mul(u64::from(percent)) / 100).max(1);
    let known = |uid: Option<u32>, satisfied: &[(Option<u32>, u64)]| {
        satisfied.iter().any(|(principal, _)| *principal == uid)
    };
    let unknown: BTreeMap<Option<u32>, u64> = if input.saturated {
        contending_principals(input, thresholds)
            .into_iter()
            .filter(|(uid, _)| !known(*uid, &satisfied))
            .collect()
    } else {
        BTreeMap::new()
    };
    // Under PSI-only saturation the host has room to spare, so every demand is
    // probed: a watch would wait for a saturation that never clears (FR-072).
    let probe = probe.filter(|probe| !(input.psi_only && probe.watch));
    let larger: BTreeMap<Option<u32>, u64> = unknown
        .iter()
        .filter(|(_, usage)| !input.psi_only && **usage >= step)
        .map(|(uid, usage)| (*uid, *usage))
        .collect();
    if let Some(watch) = probe.filter(|probe| probe.watch) {
        let mut watch = watch.clone();
        // External principals at or above the floor, the unattributed one included.
        let mut present: BTreeMap<Option<u32>, u64> = input
            .principals
            .iter()
            .flatten()
            .filter(|(principal, usage)| !principal.is_owner && *usage >= floor)
            .map(|(principal, usage)| (principal.uid, *usage))
            .collect();
        let unattributed = unattributed_floor(input, thresholds);
        if let Some(usage) = usage_of(None).filter(|usage| *usage >= unattributed) {
            present.insert(None, usage);
        }
        // The baseline is the usage once the fair share applies, an interval later;
        // it is taken again when a principal joins or leaves, such as a new user
        // moving from the unattributed principal to its own.
        let moved = watch.baselined && !present.keys().eq(watch.usage_at_step.keys());
        if !watch.baselined && now_ms > watch.stepped_at_ms || moved {
            watch.usage_at_step = present;
            watch.stepped_at_ms = now_ms;
            watch.baselined = true;
            return (satisfied, Some(watch));
        }
        if !watch.baselined || now_ms.saturating_sub(watch.stepped_at_ms) < response_ms {
            return (satisfied, Some(watch));
        }
        // Only spare capacity shows a demand is met; on a saturated host its usage
        // is what the host left it.
        if !input.saturated {
            for (uid, before) in &watch.usage_at_step {
                let usage = usage_of(*uid).unwrap_or(0);
                if !known(*uid, &satisfied) && usage <= before.saturating_add(floor) {
                    satisfied.push((*uid, usage.saturating_add(floor)));
                }
            }
        }
        return (satisfied, None);
    }
    // A demand of a margin step or more gets the fair share at once (SC-002); a
    // watch then learns whether it is satisfied.
    if !larger.is_empty() {
        let watch = DemandProbe {
            target: 0,
            stepped_at_ms: now_ms,
            usage_at_step: larger,
            watch: true,
            baselined: false,
        };
        return (satisfied, Some(watch));
    }
    let Some(probe) = probe else {
        if unknown.is_empty() {
            return (satisfied, None);
        }
        let started = DemandProbe {
            target: input.own.saturating_sub(step),
            stepped_at_ms: now_ms,
            usage_at_step: unknown,
            ..DemandProbe::default()
        };
        return (satisfied, Some(started));
    };
    // Usage that moves from the unattributed principal to a principal joining
    // during the window, as when a user is admitted, keeps its baseline, so growth
    // into the freed capacity stays visible.
    let mut probe = probe.clone();
    let joiners: Vec<(Option<u32>, u64)> = unknown
        .iter()
        .filter(|(uid, _)| !probe.usage_at_step.contains_key(uid))
        .map(|(uid, usage)| (*uid, *usage))
        .collect();
    if !joiners.is_empty() {
        let unattributed = usage_of(None).unwrap_or(0);
        let moved = probe
            .usage_at_step
            .get(&None)
            .map_or(0, |before| before.saturating_sub(unattributed));
        let joined: u64 = joiners.iter().map(|(_, usage)| *usage).sum();
        for (uid, usage) in joiners {
            let carried = if moved == 0 || joined == 0 {
                usage
            } else {
                let share = u128::from(moved) * u128::from(usage) / u128::from(joined);
                u64::try_from(share).unwrap_or(u64::MAX).min(usage)
            };
            probe.usage_at_step.insert(uid, carried);
        }
        if moved > 0 {
            probe.usage_at_step.insert(None, unattributed);
        }
    }
    if now_ms.saturating_sub(probe.stepped_at_ms) < response_ms {
        return (satisfied, Some(probe));
    }
    // The window is over: who grew into the freed capacity, even after the step
    // ended the saturation?
    let mut grown = BTreeMap::new();
    for (uid, before) in probe.usage_at_step.iter().chain(unknown.iter()) {
        if known(*uid, &satisfied) || grown.contains_key(uid) {
            continue;
        }
        let usage = usage_of(*uid).unwrap_or(0);
        if usage <= before.saturating_add(floor) {
            if usage >= floor || probe.usage_at_step.contains_key(uid) {
                satisfied.push((*uid, usage.saturating_add(floor)));
            }
        } else {
            grown.insert(*uid, usage);
        }
    }
    if grown.is_empty() {
        return (satisfied, None);
    }
    let stepped = DemandProbe {
        target: probe.target.saturating_sub(step),
        stepped_at_ms: now_ms,
        usage_at_step: grown,
        ..DemandProbe::default()
    };
    (satisfied, Some(stepped))
}

/// Own work's max-min share: the owner principal's share minus the account's
/// unmanaged usage. Under saturation a principal above the contention floor
/// wants without bound; external usage no visible principal accounts for, such
/// as unreadable owners, is one more principal (research R12).
/// Own work's max-min fair share beside the contending principals' demands; on memory it
/// is the memory fair share (FR-023).
pub fn neutral_share(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    let floor = principal_floor(input, thresholds);
    // A principal whose demand is known to be satisfied wants that much.
    let demand = |uid: Option<u32>, usage: u64| {
        if input.saturated && usage >= floor {
            satisfied_demand(input, uid, usage)
        } else {
            Some(usage)
        }
    };
    let mut demands = vec![None];
    let mut owner_unmanaged = 0u64;
    let mut attributed = 0u64;
    for (principal, usage) in input.principals.iter().flatten() {
        if principal.is_owner {
            owner_unmanaged = owner_unmanaged.saturating_add(*usage);
        } else {
            attributed = attributed.saturating_add(*usage);
            demands.push(demand(principal.uid, *usage));
        }
    }
    let unattributed = input.external.saturating_sub(
        attributed
            .saturating_add(owner_unmanaged)
            .saturating_add(input.background),
    );
    if unattributed >= unattributed_floor(input, thresholds) {
        demands.push(demand(None, unattributed));
    }
    if input.unknown_activity {
        demands.push(None);
    }
    max_min_shares(input.capacity.saturating_sub(input.background), &demands)[0]
        .saturating_sub(owner_unmanaged)
        .min(input.fair_share_limit.unwrap_or(u64::MAX))
}

/// Whether CPU was quiet this interval for the return after contention (FR-034): every
/// external principal's usage and this interval's unattributed external estimate below the
/// quiet threshold, the larger of the contention floor and the scope's per-interval
/// measurement band, so an idle scope's measurement error does not hold the return back.
pub fn cpu_quiet(
    principals: impl IntoIterator<Item = u64>,
    unattributed: u64,
    floor: u64,
    band: u64,
) -> bool {
    let threshold = floor.max(band);
    principals.into_iter().all(|usage| usage < threshold) && unattributed < threshold
}

/// Whether Guaranteed work may be drained (FR-024, FR-057): only for a memory
/// floor or envelope breach, as the last step of floor policy A, after all
/// opportunistic work on the resource is released and the breach has lasted
/// `wait_ms`, the Guaranteed hint deadline or the RAM breach window.
pub fn guaranteed_may_drain(
    kind: ResourceKind,
    cause: YieldCause,
    opportunistic_left: bool,
    breached_for_ms: u64,
    wait_ms: u64,
) -> bool {
    kind.is_memory()
        && matches!(cause, YieldCause::FloorBreach | YieldCause::EnvelopeBreach)
        && !opportunistic_left
        && breached_for_ms >= wait_ms
}

/// A job runs at the less aggressive of its requested mode and the node cap
/// (FR-030); without a request it runs at the node's mode.
pub fn effective_mode(
    node: ContentionMode,
    requested: Option<ContentionMode>,
    max_mode: ContentionMode,
) -> ContentionMode {
    requested.unwrap_or(node).min(max_mode)
}
