//! GPU devices through NVML: memory, activity classified over the quiet window,
//! measured compute, external process identity, and the release check for own work.
use super::*;
use nvml_wrapper::{enums::device::SampleValue, struct_wrappers::device::Sample};

pub(super) fn gpu_memory_mib(total: u64, used: u64) -> Option<(u64, u64)> {
    let total_mib = total / MIB;
    (total_mib > 0 && used <= total).then(|| (total_mib, used.div_ceil(MIB).min(total_mib)))
}

pub(super) fn merge_process_lists(
    compute: Option<&[u32]>,
    graphics: Option<&[u32]>,
) -> Option<Vec<u32>> {
    let processes: Vec<u32> = compute?
        .iter()
        .chain(graphics?)
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    (!processes.contains(&0)).then_some(processes)
}

/// Native identity for an independently tracked external Linux process. Requiring
/// all four UID slots to agree prevents a credential transition from retaining a
/// stale authorization. Other platforms cannot establish this contract yet.
pub fn external_gpu_process_identity(pid: u32) -> Option<ExternalGpuProcessIdentity> {
    #[cfg(target_os = "linux")]
    {
        if pid == 0 {
            return None;
        }
        let native = crate::supervision::process_identity(pid, "external-gpu-identity", 0).ok()?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let uids: Vec<u32> = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_whitespace()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if uids.len() != 4
            || uids.iter().any(|uid| *uid != uids[0])
            || native.start_time == 0
            || native.boot_id.trim().is_empty()
        {
            return None;
        }
        Some(ExternalGpuProcessIdentity {
            pid,
            boot_id: native.boot_id,
            start_ticks: native.start_time,
            uid: uids[0],
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub(super) fn verify_external_gpu_inventory(
    external: Option<&[u32]>,
    authorized: &[ExternalGpuProcessIdentity],
    before: &[ExternalGpuProcessIdentity],
    mut read_after: impl FnMut(u32) -> Option<ExternalGpuProcessIdentity>,
) -> Option<Vec<ExternalGpuProcessIdentity>> {
    let external = external?;
    if authorized.is_empty() {
        return None;
    }
    let unique: BTreeSet<_> = external.iter().copied().collect();
    if unique.len() != external.len() || unique.contains(&0) {
        return None;
    }
    unique
        .into_iter()
        .map(|pid| {
            let identity = read_after(pid)?;
            (identity.pid == pid
                && identity.start_ticks > 0
                && !identity.boot_id.trim().is_empty()
                && authorized.contains(&identity)
                && before.contains(&identity))
            .then_some(identity)
        })
        .collect()
}

/// One NVML memory read in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceMemoryRead {
    pub(super) total: u64,
    pub(super) free: u64,
    pub(super) reserved: u64,
}

/// Device memory from reads before and after process enumeration (research R8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceMemory {
    pub(super) total_mib: u64,
    /// Total minus free. Unlike NVML v2 `used`, this includes reserved memory.
    pub(super) unavailable_mib: u64,
    pub(super) free_mib: u64,
    pub(super) reserved_mib: u64,
    /// False when the two reads disagree beyond the tolerance or the second read
    /// failed: process memory read between them cannot be attributed this tick.
    pub(super) attribution_known: bool,
}

pub(super) fn device_memory(
    before: Option<DeviceMemoryRead>,
    after: Option<DeviceMemoryRead>,
    tolerance_mib: u64,
) -> Option<DeviceMemory> {
    let valid = |read: DeviceMemoryRead| read.free <= read.total && read.reserved <= read.total;
    let before = before.filter(|read| valid(*read))?;
    let after = after.filter(|read| valid(*read) && read.total == before.total);
    let free = after.map_or(before.free, |after| after.free.min(before.free));
    let (total_mib, unavailable_mib) = gpu_memory_mib(before.total, before.total - free)?;
    Some(DeviceMemory {
        total_mib,
        unavailable_mib,
        free_mib: (free / MIB).min(total_mib - unavailable_mib),
        reserved_mib: after
            .map_or(before.reserved, |after| after.reserved.max(before.reserved))
            .div_ceil(MIB),
        attribution_known: after
            .is_some_and(|after| after.free.abs_diff(before.free) <= tolerance_mib * MIB),
    })
}

/// One device interval's evidence for the own-only rule (FR-021, research R18).
pub(super) struct OwnOnlyEvidence<'a> {
    /// The compute and the graphics process queries both succeeded.
    pub(super) queries_ok: bool,
    /// Every PID either query listed, own work included.
    pub(super) listed: &'a [u32],
    pub(super) mps: bool,
    /// Device memory attributed to no listed PID, when attribution is known; with
    /// only own PIDs listed, the memory not attributed to own PIDs.
    pub(super) unattributed_memory_mib: Option<u64>,
    /// The same at the last measured, own-only or empty interval. Memory of an
    /// external PID listed then is not in it, so a context that later drops out of
    /// the list while keeping its memory still counts as growth.
    pub(super) baseline_mib: Option<u64>,
    pub(super) tolerance_mib: u64,
}

/// Whether every own-only condition holds. With only own verified PIDs listed and
/// no MPS, no unlisted context computes unless it holds memory nobody accounts
/// for, so that memory must not have grown. Any failure keeps the existing path.
pub(super) fn own_only_interval(
    evidence: &OwnOnlyEvidence<'_>,
    own_verified: impl Fn(u32) -> bool,
) -> bool {
    evidence.queries_ok
        && !evidence.mps
        && !evidence.listed.is_empty()
        && evidence
            .listed
            .iter()
            .all(|pid| *pid != 0 && own_verified(*pid))
        && evidence
            .unattributed_memory_mib
            .zip(evidence.baseline_mib)
            .is_some_and(|(now, baseline)| now <= baseline.saturating_add(evidence.tolerance_mib))
}

/// Whether an interval's unattributed memory becomes the own-only baseline: a
/// measured or own-only interval, or one whose complete inventory lists nothing.
/// Without the empty case, a device that starts empty could never be own-only.
pub(super) fn sets_memory_baseline(
    own_only: bool,
    queries_ok: bool,
    listed: Option<&[u32]>,
    measured: bool,
) -> bool {
    own_only || measured || (queries_ok && listed.is_some_and(<[u32]>::is_empty))
}

/// Consecutive healthy intervals with no external compute, per device.
#[derive(Debug, Clone, Default)]
pub(super) struct QuietState {
    pub(super) pids: BTreeSet<u32>,
    pub(super) ticks: u32,
}

/// Whether one query is healthy evidence that no external context computed: a
/// complete non-empty inventory, a successful or NOT_FOUND query, and no positive
/// sample for an external PID newer than the submitted cursor.
pub(super) fn quiet_tick(
    processes: Option<&[u32]>,
    previous: Option<u64>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
) -> bool {
    let Some(pids) = processes.filter(|pids| !pids.is_empty()) else {
        return false;
    };
    let samples: &[ProcessUtilizationSample] = match samples {
        Ok(samples) => samples,
        Err(NvmlError::NotFound) => &[],
        Err(_) => return false,
    };
    !samples.iter().any(|sample| {
        pids.contains(&sample.pid) && sample.timestamp > previous.unwrap_or(0) && sample.sm_util > 0
    })
}

/// A process leaving keeps the window; a new external process restarts it.
pub(super) fn advance_quiet(
    state: &mut QuietState,
    processes: Option<&[u32]>,
    healthy: bool,
) -> u32 {
    let pids: BTreeSet<u32> = processes.unwrap_or_default().iter().copied().collect();
    let new_process = !pids.is_subset(&state.pids);
    state.pids = pids;
    state.ticks = match (healthy, new_process) {
        (false, _) => 0,
        (true, true) => 1,
        (true, false) => state.ticks.saturating_add(1),
    };
    state.ticks
}

/// Quiet only replaces Unknown; Idle and Active evidence always wins.
pub(super) fn quiet_activity(
    activity: ComputeActivity,
    ticks: u32,
    window: u32,
) -> ComputeActivity {
    if activity == ComputeActivity::Unknown && window > 0 && ticks >= window {
        ComputeActivity::Quiet
    } else {
        activity
    }
}

/// Whether a per-process sample cursor's age restarts: on a newer timestamp, and on a
/// successful query with nothing newer, which shows the cursor still stands for now; a failed
/// query never refreshes it (TEL-3).
pub(super) fn refreshes_cursor(stored: Option<u64>, cursor: u64, queried: bool) -> bool {
    stored.is_none_or(|old| cursor > old || queried && cursor == old)
}

pub(super) fn fresh_cursor(
    previous: Option<u64>,
    age: Option<Duration>,
    max_age: Duration,
) -> Option<u64> {
    previous.filter(|_| age.is_some_and(|age| age <= max_age))
}

pub(super) fn describe_gpu_observation(
    basic_known: bool,
    previous: Option<u64>,
    baseline_age: Option<Duration>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    fresh_id: Option<u64>,
    observed_at_unix_ms: u64,
    max_age: Duration,
) -> GpuObservation {
    let newest = samples
        .as_ref()
        .ok()
        .and_then(|s| s.iter().map(|s| s.timestamp).max());
    let device_error = matches!(
        samples,
        Err(NvmlError::GpuLost
            | NvmlError::ResetRequired
            | NvmlError::DriverNotLoaded
            | NvmlError::Uninitialized
            | NvmlError::LibRmVersionMismatch
            | NvmlError::OperatingSystem)
    );
    // Health and fresh activity evidence are separate: a successful or NOT_FOUND
    // query is healthy observation even when it carries no new sample (R8).
    let healthy = matches!(samples, Ok(_) | Err(NvmlError::NotFound));
    let capability = if !basic_known || device_error {
        GpuExecutionCapability::InsufficientObservability
    } else if healthy || fresh_id.is_some() {
        GpuExecutionCapability::ContentionAware
    } else {
        GpuExecutionCapability::ConservativeNonSharing
    };
    let freshness_decision = if fresh_id.is_some() {
        "new_driver_timestamp_after_recent_baseline"
    } else if baseline_age.is_some_and(|age| age > max_age) {
        "baseline_expired_before_query_completion_buffered_history_not_activity"
    } else if previous.is_none() {
        "no_recent_baseline_buffered_history_not_activity"
    } else {
        "no_new_driver_timestamp_not_evidence_of_idleness"
    };
    GpuObservation {
        capability,
        best_effort_external_processes: None,
        activity_api: "nvmlDeviceGetProcessUtilization".into(),
        activity_api_status: match samples {
            Ok(_) => "NVML_SUCCESS".into(),
            Err(error) => format!("{error:?}"),
        },
        query_cursor_us: previous.unwrap_or(0),
        newest_sample_timestamp_us: newest,
        samples_returned: samples.as_ref().map_or(0, Vec::len),
        observed_at_unix_ms,
        baseline_age_ms: baseline_age.map(|v| v.as_millis().min(u128::from(u64::MAX)) as u64),
        sample_max_age_ms: max_age.as_millis().min(u128::from(u64::MAX)) as u64,
        fresh_after_baseline: fresh_id.is_some(),
        freshness_decision: freshness_decision.into(),
    }
}

pub(super) fn classify_activity(
    processes: Option<&[u32]>,
    previous: Option<u64>,
    samples: Option<&[ProcessUtilizationSample]>,
) -> (ComputeActivity, Option<u64>, Option<u64>) {
    let newest = samples.and_then(|samples| samples.iter().map(|s| s.timestamp).max());
    let next = previous.into_iter().chain(newest).max();
    // Discard all buffered samples on the first successful read. A repeated
    // timestamp (including overlapping polling windows) is not new evidence.
    let fresh_id = previous.and_then(|old| newest.filter(|new| *new > old));
    let activity = match processes {
        Some([]) => ComputeActivity::Idle,
        Some(pids)
            if fresh_id.is_some()
                && samples.is_some_and(|samples| {
                    samples.iter().any(|sample| {
                        sample.timestamp > previous.unwrap_or(u64::MAX)
                            && sample.sm_util > 0
                            && sample.sm_util <= 100
                            && pids.contains(&sample.pid)
                    })
                }) =>
        {
            ComputeActivity::Active
        }
        _ => ComputeActivity::Unknown,
    };
    (activity, fresh_id, next)
}

/// Listed external contexts that are background this interval (FR-021): a user ID below
/// `uid_min`, per-process utilization readable from a fresh baseline (the query succeeds or
/// reports no samples), and the context's average utilization below `min_percent`.
pub(super) fn background_contexts(
    external: Option<&[u32]>,
    previous: Option<u64>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    uid_of: impl Fn(u32) -> Option<u32>,
    uid_min: u32,
    min_percent: u8,
) -> BTreeSet<u32> {
    let (Some(external), Some(previous)) = (external, previous) else {
        return BTreeSet::new();
    };
    let samples: &[ProcessUtilizationSample] = match samples {
        Ok(samples) => samples,
        Err(NvmlError::NotFound) => &[],
        Err(_) => return BTreeSet::new(),
    };
    external
        .iter()
        .copied()
        .filter(|pid| uid_of(*pid).is_some_and(|uid| uid < uid_min))
        .filter(|pid| {
            let fresh: Vec<u64> = samples
                .iter()
                .filter(|s| s.pid == *pid && s.timestamp > previous)
                .map(|s| u64::from(s.sm_util.min(100)))
                .collect();
            let average = fresh.iter().sum::<u64>() / (fresh.len() as u64).max(1);
            average < u64::from(min_percent)
        })
        .collect()
}

/// A listed context's user ID when all four UID slots agree; otherwise it is unattributed.
pub(super) fn context_uid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let uids: Vec<u32> = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_whitespace()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        (uids.len() == 4 && uids.iter().all(|uid| *uid == uids[0])).then(|| uids[0])
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Device utilization over the per-process samples' interval: the mean of the device's own
/// samples since the cursor, with their count, or the driver's latest rate without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceUtilization {
    pub percent: u32,
    /// The device's samples over the interval; each PID's share is its summed samples over
    /// this count, so time a process was stopped counts as idle (FR-021, R33).
    pub samples: Option<u64>,
}

impl From<u32> for DeviceUtilization {
    fn from(percent: u32) -> Self {
        Self {
            percent,
            samples: None,
        }
    }
}

/// The device's utilization over the interval from its samples newer than `previous`, rounded
/// to the nearest point, else `rate`, the driver's latest sample period.
pub(super) fn device_utilization(
    samples: &std::result::Result<Vec<Sample>, NvmlError>,
    previous: Option<u64>,
    rate: Option<u32>,
) -> Option<DeviceUtilization> {
    let values: Vec<u64> = match (samples, previous) {
        (Ok(samples), Some(previous)) => samples
            .iter()
            .filter(|sample| sample.timestamp > previous)
            .filter_map(|sample| match sample.value {
                SampleValue::U32(value) => Some(u64::from(value.min(100))),
                SampleValue::U64(value) => Some(value.min(100)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if values.is_empty() {
        return rate.map(DeviceUtilization::from);
    }
    let count = values.len() as u64;
    let percent = (values.iter().sum::<u64>() + count / 2) / count;
    Some(DeviceUtilization {
        percent: u32::try_from(percent).unwrap_or(100),
        samples: Some(count),
    })
}

/// External and own GPU compute of one interval, in thousandths of device time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ComputeAmount {
    Measured { external: u64, own: u64 },
    Unknown(String),
}

/// Whether the device time-slices its busy contexts (FR-021, Q22): no MPS control daemon or
/// server and MIG mode disabled or not supported. MPS or MIG says no at once; otherwise an
/// unreadable process inventory or MIG mode is unknown, which is not taken as time-sliced.
pub(super) fn time_sliced(
    queries_ok: bool,
    mps: bool,
    mig_enabled: &std::result::Result<bool, NvmlError>,
) -> Option<bool> {
    match mig_enabled {
        _ if mps => Some(false),
        Ok(true) => Some(false),
        _ if !queries_ok => None,
        Ok(false) | Err(NvmlError::NotSupported) => Some(true),
        Err(_) => None,
    }
}

/// The interval's compute contexts for the time-slice rule (FR-021, Q22), with a complete
/// process inventory: own work's verified PIDs listed, the owner's unmanaged PIDs listed, and
/// each listed non-background PID with a positive sample newer than the cursor, with its UID.
/// Without a fresh cursor or readable samples nothing counts as sampled.
pub(super) fn compute_contexts(
    listed: Option<&[u32]>,
    own: &BTreeSet<u32>,
    background: &BTreeSet<u32>,
    previous: Option<u64>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    uid_of: impl Fn(u32) -> Option<u32>,
    owner: Option<u32>,
) -> Option<crate::model::GpuComputeContexts> {
    let listed = listed?;
    let mut contexts = crate::model::GpuComputeContexts::default();
    for pid in listed {
        if own.contains(pid) {
            contexts.own.push(*pid);
        } else if !background.contains(pid) && owner.is_some() && uid_of(*pid) == owner {
            contexts.owner.push(*pid);
        }
    }
    if let (Some(previous), Ok(samples)) = (previous, samples) {
        let busy: BTreeSet<u32> = samples
            .iter()
            .filter(|s| s.timestamp > previous && s.sm_util > 0)
            .map(|s| s.pid)
            .filter(|pid| listed.contains(pid) && !background.contains(pid))
            .collect();
        contexts.sampled = busy.into_iter().map(|pid| (pid, uid_of(pid))).collect();
    }
    contexts.own.sort_unstable();
    contexts.own.dedup();
    contexts.owner.sort_unstable();
    contexts.owner.dedup();
    Some(contexts)
}

/// MPS runs its clients in one shared context, so per-process samples do not
/// attribute compute to them.
pub(super) fn is_mps_process(name: &str) -> bool {
    name.starts_with("nvidia-cuda-mps")
}

/// The command name of a process, read-only, where the platform exposes it.
pub(super) fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|name| name.trim().to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// What one device's per-process measurement keeps between intervals (FR-021, R46, R47): each
/// PID's newest sample timestamp, where its next sample's span starts, and the last intervals
/// compared, for the 4-interval check.
#[derive(Debug, Clone, Default)]
pub(super) struct GpuSampleState {
    last: BTreeMap<u32, u64>,
    recent: std::collections::VecDeque<Compared>,
    /// Whether the last measured amount came from the last 4 intervals, not the interval alone.
    pub(super) over_last_4: bool,
}

/// How many intervals a missed sample is compared over (R47).
const COMPARED_INTERVALS: usize = 4;

/// One interval's summed shares beside the device's utilization, both less background, with each
/// side's part, each external PID's share and how many processes were sampled, in percent.
#[derive(Debug, Clone, Default)]
struct Compared {
    shares: u64,
    device: u64,
    external: u64,
    own: u64,
    by_pid: BTreeMap<u32, u64>,
    sampled: u64,
}

impl Compared {
    /// The intervals' average, each value rounded, and every PID sampled in any of them.
    fn average(intervals: &std::collections::VecDeque<Compared>) -> Self {
        let n = intervals.len().max(1) as u64;
        let mean = |sum: u64| (sum + n / 2) / n;
        let mut by_pid: BTreeMap<u32, u64> = BTreeMap::new();
        for interval in intervals {
            for (pid, share) in &interval.by_pid {
                *by_pid.entry(*pid).or_default() += share;
            }
        }
        Self {
            shares: mean(intervals.iter().map(|i| i.shares).sum()),
            device: mean(intervals.iter().map(|i| i.device).sum()),
            external: mean(intervals.iter().map(|i| i.external).sum()),
            own: mean(intervals.iter().map(|i| i.own).sum()),
            sampled: intervals.iter().map(|i| i.sampled).max().unwrap_or(0),
            by_pid: by_pid
                .into_iter()
                .map(|(pid, sum)| (pid, mean(sum)))
                .collect(),
        }
    }
}

impl GpuSampleState {
    /// An interval measured another way, own only, also ends the run of compared intervals.
    pub(super) fn interrupt(&mut self) {
        self.recent.clear();
        self.over_last_4 = false;
    }

    /// Keeps each listed PID's newest fresh sample timestamp for its next sample's span, and drops
    /// a PID that left the device's process list.
    pub(super) fn remember(
        &mut self,
        previous: u64,
        samples: &[ProcessUtilizationSample],
        listed: Option<&[u32]>,
    ) {
        for sample in samples.iter().filter(|sample| sample.timestamp > previous) {
            let newest = self.last.entry(sample.pid).or_insert(sample.timestamp);
            *newest = (*newest).max(sample.timestamp);
        }
        if let Some(listed) = listed {
            self.last.retain(|pid, _| listed.contains(pid));
        }
    }
}

/// One PID's fresh samples in an interval: the previous sample its span started from (none for
/// the window's start), each sample's timestamp and sm_util, the time counted and its share.
#[derive(Debug, Clone, Default)]
pub(super) struct Span {
    pub(super) previous: Option<u64>,
    pub(super) samples: Vec<(u64, u32)>,
    pub(super) counted_us: u64,
    pub(super) share: u64,
}

/// Each PID's span over the window from `previous` to the newest fresh per-process sample, with
/// that window's end: each sample counts sm_util × min(t − p, window) ÷ window, p the process's
/// previous sample (from `last`, else its earlier sample in this window, else the window's
/// start), the share rounded and capped at 100%. A sample covers the time since its process's
/// previous one, so it counts its whole span, at most one window, in the window where it arrives
/// (FR-021, R46).
pub(super) fn spans(
    previous: u64,
    samples: &[ProcessUtilizationSample],
    last: &BTreeMap<u32, u64>,
) -> (Option<u64>, BTreeMap<u32, Span>) {
    let mut per_pid: BTreeMap<u32, Vec<(u64, u32)>> = BTreeMap::new();
    for sample in samples.iter().filter(|sample| sample.timestamp > previous) {
        per_pid
            .entry(sample.pid)
            .or_default()
            .push((sample.timestamp, sample.sm_util.min(100)));
    }
    let Some(end) = per_pid.values().flatten().map(|(at, _)| *at).max() else {
        return (None, BTreeMap::new());
    };
    let window = end - previous;
    let spans = per_pid
        .into_iter()
        .map(|(pid, mut samples)| {
            samples.sort_unstable();
            let known = last.get(&pid).copied();
            let mut start = known.unwrap_or(previous);
            let (mut weighted, mut counted) = (0u128, 0u64);
            for (at, sm_util) in &samples {
                let covered = at.saturating_sub(start).min(window);
                weighted += u128::from(*sm_util) * u128::from(covered);
                counted += covered;
                start = *at;
            }
            let share = ((weighted + u128::from(window) / 2) / u128::from(window)).min(100);
            let span = Span {
                previous: known,
                samples,
                counted_us: counted,
                share: u64::try_from(share).unwrap_or(100),
            };
            (pid, span)
        })
        .collect();
    (Some(end), spans)
}

/// What a replay of this interval needs, for the persisted observation (FR-021, Q17): the
/// device's samples since `previous`, the window, whether the amount came from the interval alone
/// or the last 4, and per PID its previous sample, its samples (at most 32) and the time counted;
/// none without a baseline. Read before `remember` takes this interval's timestamps.
pub(super) fn compute_sampling(
    previous: Option<u64>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    device: &std::result::Result<Vec<Sample>, NvmlError>,
    device_percent: Option<u32>,
    state: &GpuSampleState,
) -> Option<crate::model::ComputeSampling> {
    use crate::model::{ComparedOver, ComputeSampling, ProcessSampling};
    const KEPT_SAMPLES: usize = 32;
    let previous = previous?;
    let device: Vec<u64> = device
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|sample| sample.timestamp)
        .filter(|at| *at > previous)
        .collect();
    let (end, spans) = spans(
        previous,
        samples.as_deref().unwrap_or_default(),
        &state.last,
    );
    Some(ComputeSampling {
        device_samples: device.len() as u64,
        device_first_us: device.iter().min().copied(),
        device_last_us: device.iter().max().copied(),
        device_percent,
        window_start_us: previous,
        window_end_us: end,
        compared_over: if state.over_last_4 {
            ComparedOver::Last4
        } else {
            ComparedOver::Interval
        },
        processes: spans
            .into_iter()
            .map(|(pid, span)| ProcessSampling {
                pid,
                previous_us: span.previous,
                samples: span.samples.into_iter().take(KEPT_SAMPLES).collect(),
                counted_us: span.counted_us,
            })
            .collect(),
    })
}

/// The interval's GPU compute per side from fresh per-process samples: each PID's
/// share of the whole interval from `spans`, own PIDs being the verified managed ones, all
/// others external. The amount is trusted only when the sum is within `tolerance` points
/// of device utilization over the same interval (FR-021). While an external PID is
/// listed, device time nobody accounts for beyond one point per sampled process is
/// external too, the unattributed part; within that it is rounding, and with own PIDs
/// only it is sampling error on own work (R33).
#[allow(clippy::too_many_arguments)]
pub(super) fn measure_compute(
    external: Option<&[u32]>,
    own: &BTreeSet<u32>,
    previous: Option<u64>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    device: Option<DeviceUtilization>,
    mps: bool,
    tolerance: u8,
    background: &BTreeSet<u32>,
    state: &mut GpuSampleState,
) -> (ComputeAmount, BTreeMap<u32, u64>) {
    // An interval that cannot be compared ends the run of intervals the 4-interval check sums.
    let unknown = |state: &mut GpuSampleState, reason: &str| {
        state.recent.clear();
        state.over_last_4 = false;
        (ComputeAmount::Unknown(reason.into()), BTreeMap::new())
    };
    if mps {
        return unknown(
            state,
            "an MPS daemon or server shares the device, so per-process samples do not attribute compute",
        );
    }
    let Some(device) = device else {
        return unknown(state, "device utilization is unavailable");
    };
    if external.is_none() {
        return unknown(state, "the device's process inventory is incomplete");
    }
    let Some(previous) = previous else {
        return unknown(state, "no fresh per-process sample baseline");
    };
    let samples: &[ProcessUtilizationSample] = match samples {
        Ok(samples) => samples,
        Err(NvmlError::NotFound) => &[],
        Err(error) => {
            return unknown(
                state,
                &format!("per-process utilization is unavailable: {error:?}"),
            );
        }
    };
    let (_, spans) = spans(previous, samples, &state.last);
    let sampled = spans.len() as u64;
    let (mut external_percent, mut own_percent, mut background_percent) = (0u64, 0u64, 0u64);
    let mut by_pid = BTreeMap::new();
    for (pid, share) in spans.into_iter().map(|(pid, span)| (pid, span.share)) {
        // Background contexts are neither side: their share leaves the device's time.
        if background.contains(&pid) {
            background_percent += share;
        } else if own.contains(&pid) {
            own_percent += share;
        } else {
            external_percent += share;
            by_pid.insert(pid, share);
        }
    }
    let device = u64::from(
        device
            .percent
            .saturating_sub(u32::try_from(background_percent).unwrap_or(u32::MAX)),
    );
    let total = external_percent + own_percent;
    let interval = Compared {
        shares: total,
        device,
        external: external_percent,
        own: own_percent,
        by_pid,
        sampled,
    };
    state.recent.push_back(interval.clone());
    while state.recent.len() > COMPARED_INTERVALS {
        state.recent.pop_front();
    }
    // The interval alone; else, when a sample was missed, the last 4 summed on both sides, with
    // their average amounts so a missed sample does not read its process as idle (R47).
    let tolerance = u64::from(tolerance);
    let chosen = if total.abs_diff(device) <= tolerance {
        state.over_last_4 = false;
        interval
    } else {
        let average = Compared::average(&state.recent);
        if state.recent.len() == COMPARED_INTERVALS
            && average.shares.abs_diff(average.device) <= tolerance
        {
            state.over_last_4 = true;
            average
        } else {
            state.over_last_4 = false;
            return (
                ComputeAmount::Unknown(format!(
                    "summed per-process utilization {total}% differs from device utilization {device}% by more than {tolerance} points"
                )),
                BTreeMap::new(),
            );
        }
    };
    let (external_percent, own_percent) = if external.is_some_and(|pids| !pids.is_empty()) {
        let remainder = chosen.device.saturating_sub(chosen.shares);
        (
            if remainder <= chosen.sampled {
                chosen.external
            } else {
                chosen.external + remainder
            },
            chosen.own,
        )
    } else {
        (0, chosen.device)
    };
    (
        ComputeAmount::Measured {
            external: external_percent.min(100) * 10,
            own: own_percent.min(100) * 10,
        },
        chosen
            .by_pid
            .into_iter()
            .map(|(pid, share)| (pid, share * 10))
            .collect(),
    )
}

pub(super) fn error_status(error: &NvmlError) -> CapabilityStatus {
    match error {
        NvmlError::NotSupported | NvmlError::FunctionNotFound => CapabilityStatus::Unsupported,
        NvmlError::NoData | NvmlError::NotFound => CapabilityStatus::Unknown,
        _ => CapabilityStatus::Unavailable,
    }
}

/// GPU admission requires complete device/process APIs before launch. Release is
/// a fresh successful absence check for the verified managed PID after OS exit;
/// it is not a promise about unrelated allocations or a hard VRAM partition.
pub struct GpuReleaseGuard {
    nvml: Option<Nvml>,
    uuids: Vec<String>,
}

impl GpuReleaseGuard {
    pub fn prepare(resources: &crate::model::Resources) -> Result<Self> {
        if resources.gpu_memory_mib.is_empty() {
            return Ok(Self {
                nvml: None,
                uuids: Vec::new(),
            });
        }
        ensure!(
            cfg!(target_os = "linux"),
            "GPU execution release qualification is Linux-only"
        );
        let nvml = Nvml::init()?;
        for (uuid, amount) in &resources.gpu_memory_mib {
            ensure!(*amount > 0, "GPU reservation must be positive");
            let device = nvml.device_by_uuid(uuid.as_str())?;
            ensure!(
                device.memory_info()?.total / MIB >= *amount,
                "GPU reservation exceeds device memory"
            );
            let compute = device.running_compute_processes()?;
            let graphics = device.running_graphics_processes()?;
            ensure!(
                compute.iter().chain(&graphics).all(|p| p.pid > 0),
                "GPU process inventory contains invalid identity"
            );
        }
        Ok(Self {
            nvml: Some(nvml),
            uuids: resources.gpu_memory_mib.keys().cloned().collect(),
        })
    }
    /// One fresh observation, never a sleep; family supervisors poll every member
    /// so a delayed GPU context cannot stall another member's termination clock.
    pub fn is_released(&self, pid: u32) -> Result<bool> {
        let Some(nvml) = &self.nvml else {
            return Ok(true);
        };
        for uuid in &self.uuids {
            let device = nvml.device_by_uuid(uuid.as_str())?;
            let compute = device.running_compute_processes()?;
            let graphics = device.running_graphics_processes()?;
            ensure!(
                compute.iter().chain(&graphics).all(|p| p.pid > 0),
                "GPU process inventory contains invalid identity"
            );
            if compute.iter().chain(&graphics).any(|p| p.pid == pid) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn confirm(&self, pid: u32, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            if self.is_released(pid)? {
                return Ok(());
            }
            ensure!(
                start.elapsed() < timeout,
                "managed GPU context remains visible after verified process exit"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
