//! Optional, read-only Linux diagnostics. No cgroup writes, process migration,
//! signals, pressure triggers, or privilege changes occur in this module.
//!
//! Runtime interface evidence takes precedence over kernel-version guesses.
//! PSI describes its recorded scope; system pressure never identifies a cause.
use crate::execution_model::ControlEvidence;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub enabled: bool,
    pub psi: bool,
    pub freshness_intervals: u64,
    pub monitor_interval_ms: u64,
    /// Observation only; this is not authorization to modify this directory.
    pub delegated_root: Option<PathBuf>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            psi: true,
            freshness_intervals: 3,
            monitor_interval_ms: 500,
            delegated_root: None,
        }
    }
}

impl KernelConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.monitor_interval_ms == 0 || self.freshness_intervals == 0 {
            return Err("kernel monitoring and freshness intervals must be positive".into());
        }
        if self
            .monitor_interval_ms
            .checked_mul(self.freshness_intervals)
            .is_none()
        {
            return Err("kernel freshness duration overflows".into());
        }
        if let Some(path) = &self.delegated_root {
            validate_absolute_path(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelSnapshot {
    pub schema_version: u32,
    pub runtime_os: String,
    pub kernel_release: Option<String>,
    pub observed_at_unix_ms: u64,
    /// Relative to this collector instance; never persist as a cross-boot clock.
    pub monotonic_elapsed_ms: u64,
    pub sample_interval_ms: Option<u64>,
    pub freshness_limit_ms: u64,
    pub controls: Vec<ControlEvidence>,
    pub cpu: CpuTopology,
    pub hierarchy: Option<CgroupHierarchy>,
    pub psi: Vec<PressureReading>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuTopology {
    pub allowed_cpu_ids: Option<Vec<u32>>,
    pub effective_cgroup_cpu_ids: Option<Vec<u32>>,
    pub effective_cpu_ids: Option<Vec<u32>>,
    pub cores: Vec<CpuCore>,
    /// Count of effective logical CPUs, independent of a cgroup bandwidth quota.
    pub effective_cpu_capacity_millicores: Option<u64>,
    /// Busy time on exactly effective_cpu_ids; never whole-host usage.
    pub busy_millicores: Option<u64>,
    pub busy_status: CpuUsageStatus,
    pub busy_interval_ms: Option<u64>,
    /// A visible ceiling, NOT free capacity or guaranteed CPU time. Hidden
    /// ancestors may restrict it further; scoped usage must be budgeted separately.
    pub visible_cpu_ceiling_millicores: Option<u64>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuCore {
    pub cpu_id: u32,
    pub package_id: Option<i32>,
    pub core_id: Option<i32>,
    pub thread_siblings: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CpuUsageStatus {
    Available,
    Baseline,
    NoInterval,
    Stale,
    CounterReset,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuTicks {
    pub total: u64,
    pub idle: u64,
    #[serde(default)]
    pub idle_only: u64,
}

/// /proc/stat guest counters are already included in user/nice, so only the
/// first eight counters contribute to total. iowait is not execution time.
pub fn parse_cpu_stat(input: &str) -> Result<BTreeMap<u32, CpuTicks>, String> {
    let mut result = BTreeMap::new();
    for line in input.lines() {
        let mut columns = line.split_whitespace();
        let Some(name) = columns.next() else {
            continue;
        };
        let Some(id) = name.strip_prefix("cpu").filter(|value| !value.is_empty()) else {
            continue;
        };
        let id = parse_u32(id)?;
        let counters: Vec<u64> = columns
            .take(8)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "invalid CPU tick counter".to_string())
            })
            .collect::<Result<_, _>>()?;
        if counters.len() < 4 {
            return Err("incomplete per-CPU stat counters".into());
        }
        let total = counters
            .iter()
            .try_fold(0u64, |sum, value| sum.checked_add(*value))
            .ok_or("CPU tick total overflows")?;
        let idle = counters[3]
            .checked_add(counters.get(4).copied().unwrap_or(0))
            .ok_or("CPU idle tick total overflows")?;
        if result
            .insert(
                id,
                CpuTicks {
                    total,
                    idle,
                    idle_only: counters[3],
                },
            )
            .is_some()
        {
            return Err("duplicate CPU stat row".into());
        }
    }
    if result.is_empty() {
        return Err("no per-CPU stat counters".into());
    }
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuUsage {
    pub status: CpuUsageStatus,
    pub busy_millicores: Option<u64>,
    pub interval_ms: Option<u64>,
    pub detail: String,
}

#[derive(Default)]
pub struct CpuUsageTracker {
    previous: Option<(u64, BTreeMap<u32, CpuTicks>)>,
    max_interval_ticks: Option<u64>,
    clock_tick_hz: Option<u64>,
}

/// Read boundaries and a conservative divisor for managed CPU attribution.
/// This is collector-local evidence, never a persisted cross-process clock.
#[derive(Clone, Copy)]
pub(crate) struct CpuAccountingWindow {
    pub started: Instant,
    pub ended: Instant,
    pub counter_interval: Duration,
}

impl CpuAccountingWindow {
    pub fn duration(self) -> Duration {
        self.ended
            .duration_since(self.started)
            .max(self.counter_interval)
    }
}

impl CpuUsageTracker {
    /// Supply the actual USER_HZ timebase, not kernel CONFIG_HZ. The monotonic
    /// idle complement conservatively covers busy ticks omitted by tick sampling.
    pub fn with_clock_tick_hz(hz: std::num::NonZeroU64) -> Self {
        Self {
            clock_tick_hz: Some(hz.get()),
            ..Self::default()
        }
    }
    /// Pure interval accounting permits deterministic tests without sleeping.
    pub fn observe(
        &mut self,
        input: Result<&str, io::Error>,
        effective_cpu_ids: Option<&[u32]>,
        monotonic_ms: u64,
        freshness_ms: u64,
    ) -> CpuUsage {
        self.max_interval_ticks = None;
        let mut result = CpuUsage {
            status: CpuUsageStatus::Unknown,
            busy_millicores: None,
            interval_ms: None,
            detail: "CPU usage unavailable; no free-capacity inference.".into(),
        };
        let selected = input
            .map_err(|error| error.to_string())
            .and_then(parse_cpu_stat)
            .and_then(|all| {
                let ids = effective_cpu_ids
                    .filter(|ids| !ids.is_empty())
                    .ok_or("effective CPU set is unavailable or empty")?;
                ids.iter()
                    .map(|id| {
                        all.get(id)
                            .cloned()
                            .map(|value| (*id, value))
                            .ok_or_else(|| format!("missing CPU {id} counter"))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()
            });
        let selected = match selected {
            Ok(selected) => selected,
            Err(error) => {
                self.previous = None;
                result.detail = error;
                return result;
            }
        };
        if let Some((old_time, previous)) = self
            .previous
            .as_ref()
            .filter(|(_, previous)| previous.keys().eq(selected.keys()))
        {
            result.interval_ms = monotonic_ms.checked_sub(*old_time);
            match result.interval_ms {
                None | Some(0) => {
                    result.status = CpuUsageStatus::NoInterval;
                    result.detail = "No positive monotonic CPU interval.".into();
                }
                Some(interval) if interval > freshness_ms => {
                    result.status = CpuUsageStatus::Stale;
                    result.detail = "CPU interval exceeded freshness limit; baseline reset.".into();
                }
                Some(interval) => {
                    let mut busy = 0u64;
                    let mut max_interval_ticks = 0u64;
                    let mut valid = true;
                    for (id, current) in &selected {
                        let old = &previous[id];
                        let Some((total, idle)) = current
                            .total
                            .checked_sub(old.total)
                            .zip(current.idle.checked_sub(old.idle))
                            .filter(|(total, idle)| total >= idle)
                        else {
                            result.status = CpuUsageStatus::CounterReset;
                            valid = false;
                            break;
                        };
                        if total == 0 {
                            result.status = CpuUsageStatus::NoInterval;
                            valid = false;
                            break;
                        }
                        let tick_busy =
                            ((u128::from(total - idle) * 1000).div_ceil(u128::from(total))) as u64;
                        let mut conservative_busy = tick_busy;
                        if let Some(hz) = self.clock_tick_hz {
                            let Some(idle_only) = current.idle_only.checked_sub(old.idle_only)
                            else {
                                result.status = CpuUsageStatus::CounterReset;
                                valid = false;
                                break;
                            };
                            // One counter tick bounds endpoint rounding. Treat
                            // iowait as occupied in this upper estimate; it is
                            // not reliable evidence of schedulable spare CPU.
                            let idle_lower = u128::from(idle_only.saturating_sub(1)) * 1000;
                            let elapsed_ticks = u128::from(interval) * u128::from(hz);
                            if idle_lower > elapsed_ticks {
                                result.status = CpuUsageStatus::Unknown;
                                valid = false;
                                break;
                            }
                            let non_idle = ((elapsed_ticks - idle_lower) * 1000)
                                .div_ceil(elapsed_ticks)
                                as u64;
                            conservative_busy = conservative_busy.max(non_idle);
                        }
                        busy += conservative_busy;
                        max_interval_ticks = max_interval_ticks.max(total);
                    }
                    if valid {
                        result.status = CpuUsageStatus::Available;
                        result.busy_millicores = Some(busy);
                        self.max_interval_ticks = Some(max_interval_ticks);
                        result.detail = if self.clock_tick_hz.is_some() {
                            "Per-effective-CPU conservative busy estimate: maximum of busy-tick ratio and monotonic non-idle fraction with one USER_HZ tick of idle rounding allowance. Missing busy ticks never manufacture spare capacity; iowait is occupied in the non-idle bound. Guest is not double counted. This is advisory observation, not a hard CPU limit."
                        } else {
                            "Per-effective-CPU tick ratio only; monotonic idle timebase unavailable. Guest is not double counted. Usage and hardware capacity share one CPU set; bandwidth quota is separate."
                        }.into();
                    } else {
                        result.detail =
                            "Counter reset, missing ticks or idle time inconsistent with elapsed time; busy usage is unknown.".into();
                    }
                }
            }
        } else {
            result.status = CpuUsageStatus::Baseline;
            result.detail = "Initial or changed CPU set requires a baseline.".into();
        }
        self.previous = Some((monotonic_ms, selected));
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CgroupMount {
    pub mount_point: PathBuf,
    pub root: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupHierarchy {
    pub mount: CgroupMount,
    pub self_membership: Option<PathBuf>,
    pub observed_path: PathBuf,
    pub explicitly_configured: bool,
    pub ancestors: Vec<CgroupLevel>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupLevel {
    pub path: PathBuf,
    pub cgroup_type: Option<String>,
    pub controllers: Option<Vec<String>>,
    pub subtree_control: Option<Vec<String>>,
    pub cpu_max: Option<CpuMax>,
    pub cpu_weight: Option<u64>,
    pub cpuset_effective: Option<Vec<u32>>,
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    /// Raw scope accounting supports comparisons without whole-host subtraction.
    pub cpu_stat: Option<String>,
    pub memory_current: Option<u64>,
    pub memory_events: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuMax {
    pub quota_us: Option<u64>,
    pub period_us: u64,
}

impl CpuMax {
    pub fn ceiling_millicores(&self) -> Option<u64> {
        self.quota_us.map(|quota| {
            ((u128::from(quota) * 1000) / u128::from(self.period_us)).min(u128::from(u64::MAX))
                as u64
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PressureResource {
    Cpu,
    Memory,
    Io,
}

impl PressureResource {
    fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Io => "io",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PressureScope {
    System,
    Cgroup { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PressureValue {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PressureCounters {
    pub some: PressureValue,
    pub full: Option<PressureValue>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PressureStatus {
    Available,
    Baseline,
    NoInterval,
    Stale,
    CounterReset,
    Missing,
    PermissionDenied,
    Invalid,
    Unsupported,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureReading {
    pub resource: PressureResource,
    pub scope: PressureScope,
    pub source: PathBuf,
    pub observed_at_unix_ms: u64,
    pub monotonic_elapsed_ms: u64,
    pub freshness_limit_ms: u64,
    pub status: PressureStatus,
    pub counters: Option<PressureCounters>,
    pub interval_ms: Option<u64>,
    pub some_stall_delta_us: Option<u64>,
    pub full_stall_delta_us: Option<u64>,
    pub detail: String,
}

/// Bounded parser for Linux CPU-list syntax, including sparse SMT identities.
pub fn parse_cpu_list(input: &str) -> Result<Vec<u32>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = BTreeSet::new();
    for item in input.split(',') {
        let (start, end) = match item.split_once('-') {
            Some((start, end)) => (parse_u32(start)?, parse_u32(end)?),
            None => {
                let value = parse_u32(item)?;
                (value, value)
            }
        };
        if start > end || end > 1_048_575 {
            return Err("invalid or excessively large CPU range".into());
        }
        values.extend(start..=end);
    }
    Ok(values.into_iter().collect())
}

fn parse_u32(input: &str) -> Result<u32, String> {
    if input.is_empty() || !input.bytes().all(|c| c.is_ascii_digit()) {
        return Err("invalid CPU ID".into());
    }
    input.parse().map_err(|_| "CPU ID overflows".into())
}

pub fn parse_cpu_max(input: &str) -> Result<CpuMax, String> {
    let values: Vec<_> = input.split_whitespace().collect();
    if values.len() != 2 {
        return Err("cpu.max must contain quota and period".into());
    }
    let period_us = values[1]
        .parse::<u64>()
        .map_err(|_| "invalid cpu.max period")?;
    if period_us == 0 {
        return Err("cpu.max period cannot be zero".into());
    }
    let quota_us = if values[0] == "max" {
        None
    } else {
        let quota = values[0]
            .parse::<u64>()
            .map_err(|_| "invalid cpu.max quota")?;
        if quota == 0 {
            return Err("cpu.max quota cannot be zero".into());
        }
        Some(quota)
    };
    Ok(CpuMax {
        quota_us,
        period_us,
    })
}

pub fn parse_psi(input: &str) -> Result<PressureCounters, String> {
    let mut some = None;
    let mut full = None;
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let mut columns = line.split_whitespace();
        let name = columns.next().ok_or("empty PSI row")?;
        let mut fields = BTreeMap::new();
        for column in columns {
            let (key, value) = column.split_once('=').ok_or("invalid PSI field")?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate PSI field".into());
            }
        }
        let average = |name| -> Result<f64, String> {
            let number = fields
                .get(name)
                .ok_or("missing PSI average")?
                .parse::<f64>()
                .map_err(|_| "invalid PSI average")?;
            if !number.is_finite() || !(0.0..=100.0).contains(&number) {
                return Err("PSI average outside 0..100".into());
            }
            Ok(number)
        };
        let value = PressureValue {
            avg10: average("avg10")?,
            avg60: average("avg60")?,
            avg300: average("avg300")?,
            total_us: fields
                .get("total")
                .ok_or("missing PSI total")?
                .parse()
                .map_err(|_| "invalid PSI total")?,
        };
        match name {
            "some" if some.is_none() => some = Some(value),
            "full" if full.is_none() => full = Some(value),
            _ => return Err("duplicate or unknown PSI row".into()),
        }
    }
    Ok(PressureCounters {
        some: some.ok_or("missing PSI some row")?,
        full,
    })
}

/// Scope is part of the cursor identity. Undefined system CPU `full` is dropped
/// before tracking and never becomes evidence of zero pressure.
#[derive(Default)]
pub struct PressureTracker {
    previous: BTreeMap<PathBuf, (PressureScope, PressureResource, u64, PressureCounters)>,
}

impl PressureTracker {
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        source: PathBuf,
        scope: PressureScope,
        resource: PressureResource,
        input: Result<&str, io::Error>,
        wall_ms: u64,
        monotonic_ms: u64,
        freshness_ms: u64,
    ) -> PressureReading {
        let mut reading = PressureReading {
            resource,
            scope: scope.clone(),
            source: source.clone(),
            observed_at_unix_ms: wall_ms,
            monotonic_elapsed_ms: monotonic_ms,
            freshness_limit_ms: freshness_ms,
            status: PressureStatus::Baseline,
            counters: None,
            interval_ms: None,
            some_stall_delta_us: None,
            full_stall_delta_us: None,
            detail: "First scope sample establishes a baseline; no external attribution.".into(),
        };
        let mut counters = match input {
            Ok(input) => match parse_psi(input) {
                Ok(counters) => counters,
                Err(error) => {
                    self.previous.remove(&source);
                    reading.status = PressureStatus::Invalid;
                    reading.detail = error;
                    return reading;
                }
            },
            Err(error) => {
                self.previous.remove(&source);
                reading.status = if error.kind() == io::ErrorKind::PermissionDenied {
                    PressureStatus::PermissionDenied
                } else {
                    PressureStatus::Missing
                };
                reading.detail =
                    format!("PSI read failed: {error}; pressure is unknown, not zero.");
                return reading;
            }
        };
        let undefined_full = resource == PressureResource::Cpu && scope == PressureScope::System;
        if undefined_full {
            counters.full = None;
        }
        if let Some((old_scope, old_resource, old_time, previous)) = self.previous.get(&source)
            && old_scope == &scope
            && old_resource == &resource
        {
            reading.interval_ms = monotonic_ms.checked_sub(*old_time);
            let resets = counters.some.total_us < previous.some.total_us
                || matches!((&counters.full, &previous.full), (Some(new), Some(old)) if new.total_us < old.total_us);
            match reading.interval_ms {
                None | Some(0) => {
                    reading.status = PressureStatus::NoInterval;
                    reading.detail =
                        "No positive monotonic interval; do not infer zero pressure.".into();
                }
                Some(interval) if interval > freshness_ms => {
                    reading.status = PressureStatus::Stale;
                    reading.detail = "Gap exceeds freshness limit; current counters are observed, but the interval is not usable.".into();
                }
                Some(_) if resets => {
                    reading.status = PressureStatus::CounterReset;
                    reading.detail = "Scope counters reset; interval invalidated.".into();
                }
                Some(_) => {
                    reading.status = PressureStatus::Available;
                    reading.some_stall_delta_us =
                        Some(counters.some.total_us - previous.some.total_us);
                    reading.full_stall_delta_us = counters
                        .full
                        .as_ref()
                        .zip(previous.full.as_ref())
                        .map(|(new, old)| new.total_us - old.total_us);
                    reading.detail = "Fresh cumulative stall deltas for this scope only; no external-workload attribution.".into();
                }
            }
        }
        if undefined_full {
            reading.detail.push_str(
                " System CPU full is undefined and omitted, even if the kernel reports zeros.",
            );
        }
        self.previous
            .insert(source, (scope, resource, monotonic_ms, counters.clone()));
        reading.counters = Some(counters);
        reading
    }
}

pub fn parse_cgroup_mounts(input: &str) -> Result<Vec<CgroupMount>, String> {
    let mut mounts = Vec::new();
    for line in input.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            return Err("invalid mountinfo separator".into());
        };
        let right: Vec<_> = right.split_whitespace().collect();
        if right.first() != Some(&"cgroup2") {
            continue;
        }
        let left: Vec<_> = left.split_whitespace().collect();
        if left.len() < 6 || right.len() < 3 {
            return Err("truncated cgroup mountinfo".into());
        }
        let root = PathBuf::from(decode_mount_field(left[3])?);
        let mount_point = PathBuf::from(decode_mount_field(left[4])?);
        validate_absolute_path(&root)?;
        validate_absolute_path(&mount_point)?;
        mounts.push(CgroupMount {
            root,
            mount_point,
            read_only: left[5]
                .split(',')
                .chain(right[2].split(','))
                .any(|flag| flag == "ro"),
        });
    }
    Ok(mounts)
}

fn decode_mount_field(input: &str) -> Result<String, String> {
    let mut output = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let digits = bytes
                .get(index + 1..index + 4)
                .ok_or("truncated mount escape")?;
            if !digits.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                return Err("invalid mount escape".into());
            }
            let value = (u16::from(digits[0] - b'0') * 64)
                + (u16::from(digits[1] - b'0') * 8)
                + u16::from(digits[2] - b'0');
            let value = u8::try_from(value).map_err(|_| "mount escape out of range")?;
            if value == 0 {
                return Err("NUL mount path".into());
            }
            output.push(value);
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|_| "non-UTF8 cgroup mount path; observation unavailable".into())
}

pub fn parse_self_cgroup(input: &str) -> Result<Option<PathBuf>, String> {
    let mut path = None;
    for line in input.lines() {
        if let Some(value) = line.strip_prefix("0::") {
            if path.is_some() || value.ends_with(" (deleted)") {
                return Err("ambiguous or deleted cgroup membership".into());
            }
            let value = PathBuf::from(value);
            validate_absolute_path(&value)?;
            path = Some(value);
        }
    }
    Ok(path)
}

fn validate_absolute_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err("path must be absolute and contain no traversal".into());
    }
    Ok(())
}

/// Fixture roots redirect reads only. Mountinfo should contain paths within the
/// fixture when used in tests; production always uses /proc and /sys.
#[derive(Debug, Clone)]
pub struct ProbeRoots {
    pub proc: PathBuf,
    pub sys: PathBuf,
}

impl Default for ProbeRoots {
    fn default() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
        }
    }
}

pub struct KernelCollector {
    config: KernelConfig,
    roots: ProbeRoots,
    started: Instant,
    last_sample_ms: Option<u64>,
    pressure: PressureTracker,
    cpu_usage: CpuUsageTracker,
    cpu_read_started: Option<Instant>,
    cpu_window: Option<CpuAccountingWindow>,
    fixture: bool,
}

impl KernelCollector {
    pub(crate) fn cpu_accounting_window(&self) -> Option<CpuAccountingWindow> {
        self.cpu_window
    }

    pub fn new(config: KernelConfig) -> Self {
        Self {
            config,
            roots: ProbeRoots::default(),
            started: Instant::now(),
            last_sample_ms: None,
            pressure: PressureTracker::default(),
            cpu_usage: runtime_cpu_tracker(),
            cpu_read_started: None,
            cpu_window: None,
            fixture: false,
        }
    }

    /// Portable fixture inspection does not assert native Linux validation.
    pub fn inspect_at(config: KernelConfig, roots: ProbeRoots) -> Self {
        Self {
            roots,
            fixture: true,
            cpu_usage: CpuUsageTracker::default(),
            ..Self::new(config)
        }
    }

    pub fn sample(&mut self) -> KernelSnapshot {
        self.cpu_window = None;
        let now = millis(self.started.elapsed().as_millis());
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| millis(value.as_millis()))
            .unwrap_or(0);
        let interval = self.last_sample_ms.and_then(|old| now.checked_sub(old));
        self.last_sample_ms = Some(now);
        let ttl = self
            .config
            .monitor_interval_ms
            .saturating_mul(self.config.freshness_intervals);
        let mut result = KernelSnapshot {
            schema_version: 1,
            runtime_os: std::env::consts::OS.into(),
            kernel_release: None,
            observed_at_unix_ms: wall,
            monotonic_elapsed_ms: now,
            sample_interval_ms: interval,
            freshness_limit_ms: ttl,
            controls: Vec::new(),
            cpu: CpuTopology::default(),
            hierarchy: None,
            psi: Vec::new(),
            limitations: Vec::new(),
        };
        if let Err(error) = self.config.validate() {
            result
                .limitations
                .push(format!("Invalid kernel diagnostics configuration: {error}"));
            return result;
        }
        if !self.config.enabled || (!cfg!(target_os = "linux") && !self.fixture) {
            let detail = if !self.config.enabled {
                "Kernel diagnostics disabled."
            } else {
                "Linux runtime behavior is unsupported here and remains unverified; portable baseline is available."
            };
            result.limitations.push(detail.into());
            for control in [
                "pidfd",
                "cgroup_v2",
                "cpu.weight",
                "cpu.max",
                "memory.high",
                "memory.max",
                "cgroup.kill",
                "psi",
            ] {
                result.controls.push(evidence(
                    control,
                    "runtime",
                    Some(false),
                    None,
                    false,
                    None,
                    detail,
                ));
            }
            result.cpu.detail = detail.into();
            return result;
        }
        result.kernel_release = read_text(&self.roots.proc.join("sys/kernel/osrelease")).ok();
        if self.fixture {
            result.limitations.push("Fixture inspection only; Linux runtime behavior has not been tested by this snapshot.".into());
        } else {
            result.controls.push(pidfd_evidence());
        }
        let hierarchy = discover_hierarchy(&self.roots, self.config.delegated_root.as_deref());
        match hierarchy {
            Ok(Some(hierarchy)) => {
                for control in [
                    "cgroup.procs",
                    "cgroup.subtree_control",
                    "cpu.weight",
                    "cpu.max",
                    "memory.high",
                    "memory.max",
                    "cgroup.kill",
                ] {
                    let mut entry = inspect_control(
                        &hierarchy.observed_path.join(control),
                        hierarchy.mount.read_only,
                    );
                    // Diagnostic root configuration is observation scope, not a requested control.
                    entry.configured = false;
                    result.controls.push(entry);
                }
                result.controls.push(evidence("cgroup_v2", &hierarchy.observed_path.display().to_string(), Some(true), None, false, None,
                    "Visible cgroup v2 hierarchy; root observation does not authorize changes or prove delegation."));
                result.hierarchy = Some(hierarchy);
            }
            Ok(None) => result.controls.push(evidence(
                "cgroup_v2",
                "runtime",
                Some(false),
                None,
                false,
                None,
                "No visible cgroup v2 hierarchy.",
            )),
            Err(error) => result.controls.push(evidence(
                "cgroup_v2",
                "runtime",
                None,
                None,
                false,
                None,
                &error,
            )),
        }
        result.cpu = inspect_cpu(&self.roots, result.hierarchy.as_ref());
        let cpu_read_started = Instant::now();
        let cpu_stat = read_text(&self.roots.proc.join("stat"));
        let cpu_read_ended = Instant::now();
        let usage = self.cpu_usage.observe(
            cpu_stat
                .as_deref()
                .map_err(|error| io::Error::new(error.kind(), error.to_string())),
            result.cpu.effective_cpu_ids.as_deref(),
            now,
            ttl,
        );
        result.cpu.busy_millicores = usage.busy_millicores;
        result.cpu.busy_status = usage.status;
        result.cpu.busy_interval_ms = usage.interval_ms;
        if let Some(started) = self.cpu_read_started
            && let Some(ticks) = self.cpu_usage.max_interval_ticks
            && let Some(counter_interval) = cpu_tick_duration(ticks)
        {
            self.cpu_window = Some(CpuAccountingWindow {
                started,
                ended: cpu_read_ended,
                counter_interval,
            });
        }
        self.cpu_read_started = Some(cpu_read_started);
        result.cpu.detail.push(' ');
        result.cpu.detail.push_str(&usage.detail);
        result.controls.push(evidence(
            "cpu_topology",
            "observer permitted CPU set",
            result.cpu.effective_cpu_ids.as_ref().map(|_| true),
            None,
            false,
            result
                .cpu
                .visible_cpu_ceiling_millicores
                .map(|value| format!("{value} logical millicores; visible ceiling only")),
            &result.cpu.detail,
        ));
        if self.config.psi {
            let mut scopes = vec![(PressureScope::System, self.roots.proc.join("pressure"))];
            // Per-cgroup PSI is collected only for the explicitly configured scope.
            if let Some(path) = self.config.delegated_root.as_ref() {
                if result
                    .hierarchy
                    .as_ref()
                    .is_some_and(|hierarchy| hierarchy.explicitly_configured)
                {
                    scopes.push((PressureScope::Cgroup { path: path.clone() }, path.clone()));
                } else {
                    result.limitations.push("Configured cgroup PSI scope could not be verified against mountinfo; no values inferred.".into());
                }
            }
            for (scope, path) in scopes {
                for resource in [
                    PressureResource::Cpu,
                    PressureResource::Memory,
                    PressureResource::Io,
                ] {
                    let source = path.join(if scope == PressureScope::System {
                        resource.name().to_string()
                    } else {
                        format!("{}.pressure", resource.name())
                    });
                    let input = read_text(&source);
                    let reading = self.pressure.observe(
                        source,
                        scope.clone(),
                        resource,
                        input
                            .as_deref()
                            .map_err(|error| io::Error::new(error.kind(), error.to_string())),
                        wall,
                        now,
                        ttl,
                    );
                    result.controls.push(evidence(
                        &format!("psi.{}", resource.name()),
                        &format!("{scope:?}"),
                        match reading.status {
                            PressureStatus::Missing => Some(false),
                            PressureStatus::PermissionDenied => None,
                            _ => reading.counters.as_ref().map(|_| true),
                        },
                        if reading.status == PressureStatus::PermissionDenied {
                            Some(false)
                        } else {
                            reading.counters.as_ref().map(|_| true)
                        },
                        true,
                        None,
                        &reading.detail,
                    ));
                    result.psi.push(reading);
                }
            }
        } else {
            result.controls.push(evidence(
                "psi",
                "runtime",
                None,
                None,
                false,
                None,
                "Optional pressure observation disabled; no zero-pressure inference.",
            ));
        }
        result.limitations.push("Read-only evidence never proves write permission or successful enforcement. PSI is observation-only; missing optional PSI does not establish spare capacity.".into());
        result
    }
}

fn runtime_cpu_tracker() -> CpuUsageTracker {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: read-only query of the process-visible procfs counter units.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz > 0 {
            return CpuUsageTracker::with_clock_tick_hz(
                std::num::NonZeroU64::new(hz as u64).expect("positive tick rate"),
            );
        }
    }
    CpuUsageTracker::default()
}

fn cpu_tick_duration(ticks: u64) -> Option<Duration> {
    #[cfg(unix)]
    {
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        let nanos = (u128::from(ticks) * 1_000_000_000).div_ceil(hz as u128);
        u64::try_from(nanos).ok().map(Duration::from_nanos)
    }
    #[cfg(not(unix))]
    {
        let _ = ticks;
        None
    }
}

fn millis(value: u128) -> u64 {
    value.min(u128::from(u64::MAX)) as u64
}
fn read_text(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map(|value| value.trim().to_owned())
}

fn evidence(
    control: &str,
    scope: &str,
    available: Option<bool>,
    permitted: Option<bool>,
    configured: bool,
    effective: Option<String>,
    detail: &str,
) -> ControlEvidence {
    ControlEvidence {
        control: control.into(),
        available,
        permitted,
        configured,
        applied: false,
        fallback: available != Some(true),
        scope: scope.into(),
        requested: None,
        effective,
        detail: detail.into(),
    }
}

fn pidfd_evidence() -> ControlEvidence {
    let existing = crate::telemetry::pidfd_capability();
    use crate::model::CapabilityStatus;
    let available = match existing.status {
        CapabilityStatus::Available => Some(true),
        CapabilityStatus::Unsupported => Some(false),
        _ => None,
    };
    evidence(
        "pidfd",
        "observer self-probe only",
        available,
        if available == Some(true) {
            Some(true)
        } else {
            None
        },
        false,
        None,
        &existing.detail,
    )
}

/// Metadata/readability is diagnostic evidence only. Do not open a write-only
/// cleanup control for reading, nor try a write as a permission test.
pub fn inspect_control(path: &Path, read_only_mount: bool) -> ControlEvidence {
    let control = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    match fs::metadata(path) {
        Ok(metadata) => {
            let permitted = read_only_mount.then_some(false);
            let permissions = permission_description(&metadata);
            let effective = if control == "cgroup.kill" {
                None
            } else {
                read_text(path).ok()
            };
            evidence(
                &control,
                &path.display().to_string(),
                Some(true),
                permitted,
                false,
                effective,
                &format!(
                    "Interface exists; {permissions}; mount read_only={read_only_mount}. Metadata does not establish write permission; no write attempted."
                ),
            )
        }
        Err(error) => evidence(
            &control,
            &path.display().to_string(),
            if error.kind() == io::ErrorKind::NotFound {
                Some(false)
            } else {
                None
            },
            if error.kind() == io::ErrorKind::PermissionDenied {
                Some(false)
            } else {
                None
            },
            false,
            None,
            &format!("Interface inspection failed: {error}; no control applied."),
        ),
    }
}

#[cfg(unix)]
fn permission_description(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "mode={:o}, uid={}, gid={}",
        metadata.mode() & 0o7777,
        metadata.uid(),
        metadata.gid()
    )
}
#[cfg(not(unix))]
fn permission_description(metadata: &fs::Metadata) -> String {
    format!("readonly metadata={}", metadata.permissions().readonly())
}

fn discover_hierarchy(
    roots: &ProbeRoots,
    configured: Option<&Path>,
) -> Result<Option<CgroupHierarchy>, String> {
    let mounts = parse_cgroup_mounts(
        &read_text(&roots.proc.join("self/mountinfo"))
            .map_err(|error| format!("mountinfo unavailable: {error}"))?,
    )?;
    let membership = parse_self_cgroup(
        &read_text(&roots.proc.join("self/cgroup"))
            .map_err(|error| format!("cgroup membership unavailable: {error}"))?,
    )?;
    let selected = if let Some(path) = configured {
        validate_absolute_path(path)?;
        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("configured observation root unavailable: {error}"))?;
        if canonical != path {
            return Err(
                "configured cgroup observation root must resolve without symlink substitution"
                    .into(),
            );
        }
        mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.mount_point))
            .max_by_key(|mount| mount.mount_point.components().count())
            .map(|mount| (mount.clone(), path.to_owned()))
    } else {
        membership.as_ref().and_then(|membership| {
            mounts
                .iter()
                .filter_map(|mount| {
                    membership
                        .strip_prefix(&mount.root)
                        .ok()
                        .map(|relative| (mount.clone(), mount.mount_point.join(relative)))
                })
                .max_by_key(|(mount, _)| mount.root.components().count())
        })
    };
    let Some((mount, observed_path)) = selected else {
        if mounts.is_empty() {
            return Ok(None);
        }
        return Err("Visible cgroup mount cannot be unambiguously mapped to membership/configured scope; namespace-hidden ancestors are unknown".into());
    };
    let mut ancestors = Vec::new();
    let mut current = observed_path.as_path();
    loop {
        if !current.starts_with(&mount.mount_point) {
            return Err("hierarchy mapping escaped visible mount".into());
        }
        ancestors.push(inspect_level(current));
        if current == mount.mount_point {
            break;
        }
        current = current.parent().ok_or("missing visible cgroup ancestor")?;
    }
    Ok(Some(CgroupHierarchy { mount, self_membership: membership, observed_path, explicitly_configured: configured.is_some(), ancestors,
        detail: "Visible ancestors only; namespace-hidden ancestors remain unknown. cpu.weight is relative and hierarchical, not a CPU percentage, hard limit, or guarantee. Competing workload placement is not inferred.".into() }))
}

fn inspect_level(path: &Path) -> CgroupLevel {
    let mut errors = Vec::new();
    let mut read = |name| match read_text(&path.join(name)) {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(format!("{name}: {error}"));
            None
        }
    };
    let cgroup_type = read("cgroup.type");
    let controllers = read("cgroup.controllers").map(words);
    let subtree_control = read("cgroup.subtree_control").map(words);
    let raw_cpu_max = read("cpu.max");
    let cpu_weight = read("cpu.weight").and_then(|value| value.parse().ok());
    let raw_cpuset = read("cpuset.cpus.effective");
    let memory_max = read("memory.max");
    let memory_high = read("memory.high");
    let cpu_stat = read("cpu.stat");
    let memory_current = read("memory.current").and_then(|value| value.parse().ok());
    let memory_events = read("memory.events");
    let cpu_max = raw_cpu_max.and_then(|value| match parse_cpu_max(&value) {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    });
    let cpuset_effective = raw_cpuset.and_then(|value| match parse_cpu_list(&value) {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    });
    CgroupLevel {
        path: path.into(),
        cgroup_type,
        controllers,
        subtree_control,
        cpu_max,
        cpu_weight,
        cpuset_effective,
        memory_max,
        memory_high,
        cpu_stat,
        memory_current,
        memory_events,
        errors,
    }
}
fn words(value: String) -> Vec<String> {
    value.split_whitespace().map(str::to_string).collect()
}

fn inspect_cpu(roots: &ProbeRoots, hierarchy: Option<&CgroupHierarchy>) -> CpuTopology {
    // /proc/self/status exposes the actual task affinity without a fixed-size
    // cpu_set_t that would truncate large systems. No set-affinity call occurs.
    let status = read_text(&roots.proc.join("self/status"));
    let allowed_cpu_ids = status
        .as_ref()
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        })
        .and_then(|value| parse_cpu_list(value).ok());
    let effective_cgroup_cpu_ids = hierarchy
        .and_then(|hierarchy| hierarchy.ancestors.first())
        .and_then(|level| level.cpuset_effective.clone());
    let effective_cpu_ids =
        allowed_cpu_ids
            .as_ref()
            .map(|allowed| match &effective_cgroup_cpu_ids {
                Some(effective) => allowed
                    .iter()
                    .filter(|cpu| effective.contains(cpu))
                    .copied()
                    .collect(),
                None => allowed.clone(),
            });
    let effective_cpu_capacity_millicores = effective_cpu_ids
        .as_ref()
        .map(|values: &Vec<u32>| (values.len() as u64).saturating_mul(1000));
    let mut ceiling = effective_cpu_capacity_millicores;
    if let Some(hierarchy) = hierarchy {
        for level in &hierarchy.ancestors {
            if let Some(limit) = level.cpu_max.as_ref().and_then(CpuMax::ceiling_millicores) {
                ceiling = ceiling.map(|previous| previous.min(limit));
            }
        }
    }
    let cores = effective_cpu_ids
        .as_ref()
        .map(|values| {
            values
                .iter()
                .map(|cpu| {
                    let path = roots
                        .sys
                        .join(format!("devices/system/cpu/cpu{cpu}/topology"));
                    CpuCore {
                        cpu_id: *cpu,
                        package_id: read_text(&path.join("physical_package_id"))
                            .ok()
                            .and_then(|value| value.parse().ok()),
                        core_id: read_text(&path.join("core_id"))
                            .ok()
                            .and_then(|value| value.parse().ok()),
                        thread_siblings: read_text(&path.join("thread_siblings_list"))
                            .ok()
                            .and_then(|value| parse_cpu_list(&value).ok()),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    CpuTopology { allowed_cpu_ids, effective_cgroup_cpu_ids, effective_cpu_ids, cores, effective_cpu_capacity_millicores, busy_millicores: None, busy_status: CpuUsageStatus::Unknown, busy_interval_ms: None, visible_cpu_ceiling_millicores: ceiling,
        detail: "Observer task affinity intersected with the observed cgroup effective set; minimum visible ancestor cpu.max. This is a ceiling only: no guaranteed share or headroom. Namespace-hidden ancestors and future workload affinity may differ. Never subtract whole-host CPU usage from this restricted budget.".into() }
}

#[cfg(test)]
mod cpu_alignment_tests {
    use super::*;

    #[test]
    fn aligned_cpu_divisor_matches_selected_counter_intervals_and_resets() {
        let mut tracker = CpuUsageTracker::default();
        let first =
            "cpu0 100 0 0 900 0 0 0 0\ncpu1 100 0 0 900 0 0 0 0\ncpu2 100 0 0 900 0 0 0 0\n";
        let second =
            "cpu0 120 0 0 930 0 0 0 0\ncpu1 110 0 0 950 0 0 0 0\ncpu2 500 0 0 9600 0 0 0 0\n";
        tracker.observe(Ok(first), Some(&[0, 1]), 0, 1500);
        assert!(tracker.max_interval_ticks.is_none());
        let usage = tracker.observe(Ok(second), Some(&[0, 1]), 510, 1500);
        assert_eq!(usage.busy_millicores, Some(567));
        assert_eq!(tracker.max_interval_ticks, Some(60));
        tracker.observe(Ok(second), Some(&[0, 1]), 1020, 1500);
        assert!(tracker.max_interval_ticks.is_none());
        tracker.observe(
            Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            Some(&[0, 1]),
            1530,
            1500,
        );
        assert!(tracker.max_interval_ticks.is_none());
    }
}
