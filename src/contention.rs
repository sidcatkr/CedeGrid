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

/// Whether new own work may start on a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Admission {
    Open,
    /// Waiting for a measurement or a step to settle; retried every interval.
    Held {
        reason: String,
    },
    /// Refused until the cause changes, such as a floor or the node cap.
    Blocked {
        reason: String,
    },
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
    /// Memory: the extra cooldown after a repeated reclaim episode, doubling up to
    /// the maximum (FR-061).
    pub reclaim_backoff_ms: u64,
    pub reclaim_backoff_max_ms: u64,
}

impl ResourceThresholds {
    /// Provisional defaults from the spec, to validate on real hardware.
    pub fn defaults(kind: ResourceKind) -> Self {
        Self {
            yield_first_margin_percent: 25,
            neutral_margin_percent: 20,
            performance_margin_percent: 10,
            yield_first_response_intervals: 3,
            scale_up_step_interval_s: None,
            neutral_convergence_s: 60,
            neutral_tolerance_points: 15,
            performance_ramp_s: 30,
            performance_ramp_percent: 80,
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
            reclaim_backoff_ms: 1000,
            reclaim_backoff_max_ms: 30_000,
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

    /// A resource's scale-up cooldown for a mode (FR-034), independent of GPU
    /// settings: 30 s in Yield First, as 0.2's cooldown, and faster otherwise.
    pub fn cooldown_ms(&self, mode: ContentionMode) -> u64 {
        self.scale_up_cooldown_ms
            .unwrap_or_else(|| self.scale_up_interval_s(mode).saturating_mul(1000))
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
}

impl LadderTiming {
    pub fn new(thresholds: &ResourceThresholds, mode: ContentionMode, interval_ms: u64) -> Self {
        let interval_ms = interval_ms.max(1);
        let response_ms = match mode {
            ContentionMode::YieldFirst => {
                u64::from(thresholds.yield_first_response_intervals).saturating_mul(interval_ms)
            }
            ContentionMode::Neutral | ContentionMode::Performance => {
                thresholds.reclaim_s.saturating_mul(1000)
            }
        };
        Self {
            response_ms,
            dwell_ms: u64::from(thresholds.min_step_dwell_intervals).saturating_mul(interval_ms),
            scale_up_ms: thresholds.scale_up_interval_s(mode).saturating_mul(1000),
            pause_bound_ms: thresholds.pause_bound_s.saturating_mul(1000),
            hint_deadline_ms: thresholds.hint_deadline_s.saturating_mul(1000),
            hysteresis_percent: thresholds.hysteresis_percent,
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
    /// Whether a step can apply here. Memory starts at the hint; Guaranteed work is
    /// never paused and drained only when a floor policy allows it.
    fn applies(&self, step: YieldStep) -> bool {
        !(self.kind.is_memory()
            && matches!(
                step,
                YieldStep::LowerShare | YieldStep::Cap | YieldStep::Pause
            )
            || self.guaranteed && step == YieldStep::Pause
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
        // A hint waits for its deadline, never just the response time (FR-037).
        let due = since_change >= timing.dwell_ms
            && (current == YieldStep::None
                || current != YieldStep::Hint && persisted >= timing.response_ms
                || pause_bound
                || current == YieldStep::Hint && persisted >= timing.hint_deadline_ms)
            || input.unavailable.contains(&current)
            || cap_to_zero;
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
    // Paused work uses nothing, so a pause is seldom contended; its bound still holds.
    if current == YieldStep::Pause
        && since_change >= timing.pause_bound_ms
        && let Some(decision) = end_pause()
    {
        return decision;
    }
    // While nothing is contended, a held cap rises toward what the mode allows now,
    // one move per scale-up interval, and a pause becomes such a cap.
    if let Some(allowed) = input.target
        && since_change >= timing.dwell_ms.max(timing.scale_up_ms)
    {
        let raise = match current {
            YieldStep::Cap => held.target.is_some_and(|target| allowed > target),
            YieldStep::Pause => {
                allowed > 0
                    && input.applies(YieldStep::Cap)
                    && !input.unavailable.contains(&YieldStep::Cap)
            }
            _ => false,
        };
        if raise {
            held.step = YieldStep::Cap;
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
    if state.trigger_external > 0 && input.external >= threshold {
        held.calm_since_ms = None;
        return LadderDecision {
            state: Some(held),
            skipped: Vec::new(),
        };
    }
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
    if now_ms.saturating_sub(calm_since) >= timing.dwell_ms
        && since_change >= timing.dwell_ms.max(timing.scale_up_ms)
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
    pub guaranteed: bool,
    /// Time since the last accepted checkpoint, else since start.
    pub progress_lost_ms: u64,
    /// What this allocation would release on the contended resource.
    pub amount: u64,
}

/// Order candidates by least cost and drop uncontrollable ones: freeing the
/// resource, pending before running, opportunistic before Guaranteed, least
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
            a.guaranteed,
            a.progress_lost_ms,
            fit(a),
            &a.id,
        )
            .cmp(&(
                !b.frees,
                !b.pending,
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
    /// Unmanaged usage per principal, the owner included; `None` when unobserved.
    /// Users that are not principals yet are left out and count as unattributed.
    pub principals: Option<Vec<(Principal, u64)>>,
    /// Usage by system accounts (below `uid_min`): background demand that comes
    /// off the capacity and never contends (FR-059).
    pub background: u64,
    /// Usage at capacity hides how much more external work wants (CPU).
    pub saturated: bool,
    /// Activity of unknown amount, such as unsampled GPU compute.
    pub unknown_activity: bool,
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
/// zero and Neutral and Performance hold the fair share. Otherwise each keeps its
/// own margin free, 25%, 20% and 10% by default, validated as YF ≥ N ≥ P, so the
/// targets are ordered YF ≤ N ≤ P on every resource.
/// Memory floors are already outside `capacity`, and memory keeps no margin
/// beyond them except Yield First's.
pub fn own_allowance(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    let margin = |percent: u8| input.capacity.saturating_mul(u64::from(percent)) / 100;
    let available = input.capacity.saturating_sub(input.external);
    let contended = contended(input, thresholds);
    match input.mode {
        ContentionMode::YieldFirst if contended => 0,
        ContentionMode::YieldFirst => {
            available.saturating_sub(margin(thresholds.yield_first_margin_percent))
        }
        _ if input.kind.is_memory() => available,
        _ if contended => neutral_share(input, thresholds),
        ContentionMode::Neutral => {
            available.saturating_sub(margin(thresholds.neutral_margin_percent))
        }
        ContentionMode::Performance => {
            available.saturating_sub(margin(thresholds.performance_margin_percent))
        }
    }
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

/// Compute is contended when external activity of unknown amount exists, or when
/// the resource is saturated and an external principal, or the unattributed
/// usage that stands for one, uses at least the contention floor.
pub fn contended(input: &ModeInput, thresholds: &ResourceThresholds) -> bool {
    if input.kind.is_memory() {
        return false;
    }
    if input.unknown_activity {
        return true;
    }
    if !input.saturated {
        return false;
    }
    let floor = principal_floor(input, thresholds);
    let mut accounted = input.background;
    for (principal, usage) in input.principals.iter().flatten() {
        accounted = accounted.saturating_add(*usage);
        if !principal.is_owner && *usage >= floor {
            return true;
        }
    }
    input.external.saturating_sub(accounted) >= floor
}

/// Own work's max-min share: the owner principal's share minus the account's
/// unmanaged usage. Under saturation a principal above the contention floor
/// wants without bound; external usage no visible principal accounts for, such
/// as unreadable owners, is one more principal (research R12).
fn neutral_share(input: &ModeInput, thresholds: &ResourceThresholds) -> u64 {
    let floor = principal_floor(input, thresholds);
    let demand = |usage: u64| (!(input.saturated && usage >= floor)).then_some(usage);
    let mut demands = vec![None];
    let mut owner_unmanaged = 0u64;
    let mut attributed = 0u64;
    for (principal, usage) in input.principals.iter().flatten() {
        if principal.is_owner {
            owner_unmanaged = owner_unmanaged.saturating_add(*usage);
        } else {
            attributed = attributed.saturating_add(*usage);
            demands.push(demand(*usage));
        }
    }
    let unattributed = input.external.saturating_sub(
        attributed
            .saturating_add(owner_unmanaged)
            .saturating_add(input.background),
    );
    if unattributed >= floor {
        demands.push(demand(unattributed));
    }
    if input.unknown_activity {
        demands.push(None);
    }
    max_min_shares(input.capacity.saturating_sub(input.background), &demands)[0]
        .saturating_sub(owner_unmanaged)
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
