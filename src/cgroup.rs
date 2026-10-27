//! Optional, explicitly authorized cgroup v2 controls.
//!
//! The coordinator and policy remain in user space. This backend only creates
//! private workload leaves under an already delegated, configured subtree. It
//! never enables controllers, changes ancestors, moves existing processes, or
//! treats RAM controls as GPU memory enforcement. Linux runtime behavior must be
//! verified on an authorized Linux host; fixture tests are not runtime evidence.
use crate::contention::{ResourceKey, ResourceKind, StepRequest, YieldStep};
use crate::execution_model::{
    Actuator, ActuatorSettings, AllocationClass, ControlEvidence, Family, GateSetup, LaunchBackend,
    LaunchRequest, ProcessIdentity,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
/// How often a held RAM cap is checked for throttling without freeing memory.
const RAM_CAP_CHECK: Duration = Duration::from_millis(100);
const WRITABLE_CONTROLS: &[&str] = &[
    "cgroup.procs",
    "cpu.weight",
    "cpu.max",
    "memory.high",
    "memory.max",
    "cgroup.kill",
    "cgroup.freeze",
];
/// Controls that runtime yield steps write, when the operator authorizes them. A
/// RAM cap is `memory.high` only; `memory.max`, `memory.low` and `memory.min` are
/// never runtime steps.
const RUNTIME_CONTROLS: &[&str] = &["cpu.weight", "cpu.max", "cgroup.freeze", "memory.high"];

/// CPU bandwidth is a quota per period, unlike the relative `cpu.weight`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuMax {
    pub quota_us: u64,
    pub period_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CgroupConfig {
    pub enabled: bool,
    pub delegated_root: Option<PathBuf>,
    /// Exact control-file names the operator permits this backend to write.
    /// `cgroup.procs` authorizes the gate to join its own new workload leaf.
    pub authorized_controls: Vec<String>,
    /// Relative hierarchical weight; neither a CPU percentage nor a guarantee.
    pub cpu_weight: Option<u16>,
    /// No bandwidth limit is introduced unless this is explicitly configured.
    pub cpu_max: Option<CpuMax>,
    pub memory_high_mib: Option<u64>,
    pub memory_max_mib: Option<u64>,
    /// Permission is necessary but not sufficient: every member must also be
    /// proven managed before destructive subtree cleanup can be implemented.
    pub allow_kill: bool,
}

impl Default for CgroupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            delegated_root: None,
            authorized_controls: vec![],
            cpu_weight: Some(10),
            cpu_max: None,
            memory_high_mib: None,
            memory_max_mib: None,
            allow_kill: false,
        }
    }
}

impl CgroupConfig {
    /// Pure validation: this never creates a directory or writes a kernel file.
    pub fn validate(&self) -> Result<()> {
        if let Some(root) = &self.delegated_root {
            validate_absolute_path(root)?;
        }
        let mut seen = BTreeSet::new();
        for control in &self.authorized_controls {
            ensure!(
                WRITABLE_CONTROLS.contains(&control.as_str()),
                "unsupported authorized cgroup control: {control}"
            );
            ensure!(
                seen.insert(control),
                "duplicate authorized cgroup control: {control}"
            );
        }
        if let Some(weight) = self.cpu_weight {
            ensure!(
                (1..=10_000).contains(&weight),
                "cpu.weight must be in 1..=10000"
            );
        }
        if let Some(max) = &self.cpu_max {
            ensure!(
                max.quota_us >= 1_000,
                "cpu.max quota_us must be at least 1000"
            );
            ensure!(
                (1_000..=1_000_000).contains(&max.period_us),
                "cpu.max period_us must be in 1000..=1000000"
            );
        }
        for (name, value) in [
            ("memory.high", self.memory_high_mib),
            ("memory.max", self.memory_max_mib),
        ] {
            if let Some(value) = value {
                ensure!(
                    value > 0 && value.checked_mul(MIB).is_some(),
                    "{name} must be a positive, representable MiB value"
                );
            }
        }
        if let (Some(high), Some(max)) = (self.memory_high_mib, self.memory_max_mib) {
            ensure!(high <= max, "memory.high must not exceed memory.max");
        }
        if self.enabled {
            ensure!(
                self.delegated_root.is_some(),
                "enabled cgroup backend requires an explicitly authorized delegated_root"
            );
            self.require_authorized("cgroup.procs")?;
            for setting in self.settings() {
                self.require_authorized(setting.0)?;
            }
            if self.allow_kill {
                self.require_authorized("cgroup.kill")?;
            }
        }
        Ok(())
    }

    /// Controls a successfully prepared backend applies: launch settings and the
    /// authorized runtime-step controls. Observations such as PSI are never controls.
    pub fn advertised_controls(&self) -> Vec<String> {
        let mut controls: BTreeSet<String> = self
            .settings()
            .into_iter()
            .map(|(control, _)| control.to_owned())
            .collect();
        controls.extend(
            RUNTIME_CONTROLS
                .iter()
                .filter(|control| self.authorized_controls.iter().any(|c| c == *control))
                .map(|control| (*control).to_owned()),
        );
        controls.into_iter().collect()
    }

    fn require_authorized(&self, control: &str) -> Result<()> {
        ensure!(
            self.authorized_controls.iter().any(|c| c == control),
            "configured cgroup control is not explicitly authorized: {control}"
        );
        Ok(())
    }

    fn settings(&self) -> Vec<(&'static str, String)> {
        let mut settings = Vec::new();
        if let Some(weight) = self.cpu_weight {
            settings.push(("cpu.weight", weight.to_string()));
        }
        if let Some(max) = &self.cpu_max {
            settings.push(("cpu.max", format!("{} {}", max.quota_us, max.period_us)));
        }
        if let Some(value) = self.memory_high_mib {
            settings.push(("memory.high", (value.saturating_mul(MIB)).to_string()));
        }
        if let Some(value) = self.memory_max_mib {
            settings.push(("memory.max", (value.saturating_mul(MIB)).to_string()));
        }
        settings
    }
}

fn validate_absolute_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path != Path::new("/"),
        "delegated_root must be an explicit absolute subtree path"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "delegated_root must not contain traversal components"
    );
    // Path::components normalizes interior '.', so reject it in the original text.
    ensure!(
        !path
            .to_string_lossy()
            .split('/')
            .any(|part| part == "." || part == ".."),
        "delegated_root must not contain traversal components"
    );
    Ok(())
}

/// Readback uses the canonical kernel representation, not string substrings.
fn canonical_control(name: &str, value: &str) -> Result<String> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    match name {
        "cgroup.freeze" => {
            ensure!(
                fields.len() == 1 && matches!(fields[0], "0" | "1"),
                "invalid cgroup.freeze readback"
            );
            Ok(fields[0].into())
        }
        "cpu.weight" => {
            ensure!(fields.len() == 1, "invalid cpu.weight readback");
            Ok(fields[0].parse::<u16>()?.to_string())
        }
        "memory.high" | "memory.max" => {
            ensure!(fields.len() == 1, "invalid {name} readback");
            if fields[0] == "max" {
                Ok("max".into())
            } else {
                Ok(fields[0].parse::<u64>()?.to_string())
            }
        }
        "cpu.max" => {
            ensure!(fields.len() == 2, "invalid cpu.max readback");
            let quota = if fields[0] == "max" {
                "max".into()
            } else {
                fields[0].parse::<u64>()?.to_string()
            };
            Ok(format!("{quota} {}", fields[1].parse::<u64>()?))
        }
        _ => bail!("unsupported cgroup control readback: {name}"),
    }
}

/// A cgroup limit: `max` or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Max,
    Value(u64),
}

impl std::str::FromStr for Limit {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "max" => Ok(Limit::Max),
            number => Ok(Limit::Value(number.parse()?)),
        }
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Limit::Max => f.write_str("max"),
            Limit::Value(value) => write!(f, "{value}"),
        }
    }
}

/// The files of one workload leaf, for runtime step writes.
pub trait LeafIo {
    /// Whether this is still the leaf created for the allocation.
    fn same_leaf(&self) -> Result<bool>;
    fn read(&self, control: &str) -> Result<String>;
    fn write(&self, control: &str, value: &str) -> Result<()>;
    /// The host's swap, in bytes.
    fn host_swap_bytes(&self) -> Result<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo")?;
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix("SwapTotal:"))
            .and_then(|rest| rest.split_whitespace().next())
            .context("no SwapTotal in /proc/meminfo")?
            .parse::<u64>()
            .map(|kib| kib.saturating_mul(1024))
            .map_err(Into::into)
    }
}

/// The lowest `memory.high` a RAM cap sets: what the leaf cannot reclaim, its
/// unevictable memory and, without swap, its anonymous memory, plus the noise
/// tolerance. A lower limit would only stall the job and free nothing.
pub fn ram_cap_floor_bytes(stat: &str, swap: bool, noise_bytes: u64) -> Result<u64> {
    let field = |name: &str| -> Result<u64> {
        stat.lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some(name)).then(|| parts.next()).flatten()
            })
            .with_context(|| format!("memory.stat has no {name}"))?
            .parse::<u64>()
            .map_err(Into::into)
    };
    let anon = if swap { 0 } else { field("anon")? };
    Ok(field("unevictable")?
        .saturating_add(anon)
        .saturating_add(noise_bytes))
}

/// The `high` count of `memory.events`.
fn memory_high_events(io: &dyn LeafIo) -> Result<u64> {
    io.read("memory.events")?
        .lines()
        .find_map(|line| line.strip_prefix("high "))
        .context("memory.events has no high count")?
        .trim()
        .parse::<u64>()
        .map_err(Into::into)
}

fn memory_current(io: &dyn LeafIo) -> Result<u64> {
    Ok(io.read("memory.current")?.trim().parse::<u64>()?)
}

/// Write a runtime-step control only while the leaf is unchanged and holds
/// exactly the verified processes, and only with a matching readback. Unlike
/// launch settings, the leaf is populated here, so membership replaces emptiness.
pub fn runtime_write(
    io: &dyn LeafIo,
    verified: &BTreeSet<u32>,
    control: &str,
    value: &str,
) -> Result<String> {
    verify_runtime_target(io, verified, control)?;
    write_checked(io, control, value)
}

fn verify_runtime_target(io: &dyn LeafIo, verified: &BTreeSet<u32>, control: &str) -> Result<()> {
    ensure!(
        RUNTIME_CONTROLS.contains(&control),
        "{control} is not a runtime step control"
    );
    ensure!(
        io.same_leaf()?,
        "workload leaf was replaced; refuse runtime write"
    );
    let members = io
        .read("cgroup.procs")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    ensure!(
        !verified.is_empty() && members == *verified,
        "leaf membership differs from the verified processes; refuse runtime write"
    );
    Ok(())
}

fn write_checked(io: &dyn LeafIo, control: &str, value: &str) -> Result<String> {
    io.write(control, value)?;
    let actual = canonical_control(control, &io.read(control)?)?;
    ensure!(
        actual == canonical_control(control, value)?,
        "{control} readback differs from the requested value"
    );
    Ok(actual)
}

/// A plain directory standing in for a leaf in non-native tests. It performs
/// no cgroup v2 checks and is never used for a real allocation.
#[cfg(unix)]
pub struct DirectoryLeaf {
    path: PathBuf,
    identity: (u64, u64),
    host_swap: u64,
}

#[cfg(unix)]
impl DirectoryLeaf {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(metadata.is_dir(), "leaf fixture must be a directory");
        Ok(Self {
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
            host_swap: 0,
        })
    }

    /// The host swap this fixture reports; none unless set.
    pub fn with_host_swap(mut self, bytes: u64) -> Self {
        self.host_swap = bytes;
        self
    }

    fn file(&self, control: &str) -> Result<PathBuf> {
        ensure!(
            !control.contains('/') && control != "." && control != "..",
            "invalid control file name"
        );
        Ok(self.path.join(control))
    }
}

#[cfg(unix)]
impl LeafIo for DirectoryLeaf {
    fn same_leaf(&self) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == self.identity))
    }
    fn read(&self, control: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.file(control)?)?)
    }
    fn write(&self, control: &str, value: &str) -> Result<()> {
        Ok(std::fs::write(self.file(control)?, value)?)
    }
    fn host_swap_bytes(&self) -> Result<u64> {
        Ok(self.host_swap)
    }
}

/// A RAM cap in effect, and what the ineffective-cap check compares against.
struct RamCap {
    mib: u64,
    /// The cap sits above its target, so it only stops growth.
    growth_stop: bool,
    high_events: u64,
    current: u64,
    throttled_since: Option<Instant>,
}

/// Runtime steps through authorized delegated controls (research R4), with the
/// rootless mechanisms as fallback for steps whose control is not authorized
/// or whose write is refused.
pub struct CgroupActuator<L: LeafIo> {
    leaf: L,
    authorized: BTreeSet<String>,
    class: AllocationClass,
    requested_cpu: u64,
    /// Launch values restored when a step reverses.
    weight: String,
    cpu_max: String,
    launch_quota_us: Option<u64>,
    period_us: u64,
    fallback: crate::actuation::RootlessActuator,
    written: std::collections::BTreeMap<ResourceKind, BTreeSet<&'static str>>,
    detail: Option<String>,
    /// The launch `memory.high`, which a RAM cap never exceeds and reversal restores.
    memory_high: String,
    requested_ram_mib: u64,
    ram_noise_bytes: u64,
    ram_calm_window: Duration,
    ram_cap: Option<RamCap>,
    /// Why the RAM cap step is unavailable for this allocation, as a catalog code.
    ram_refused: Option<&'static str>,
}

impl<L: LeafIo> CgroupActuator<L> {
    pub fn new(leaf: L, config: &CgroupConfig, settings: &ActuatorSettings) -> Self {
        let period_us = config.cpu_max.as_ref().map_or(100_000, |max| max.period_us);
        Self {
            leaf,
            authorized: config.authorized_controls.iter().cloned().collect(),
            class: settings.class,
            requested_cpu: settings.requested.cpu_millicores.max(1),
            // Without a launch setting the leaf keeps the kernel default weight.
            weight: config
                .cpu_weight
                .map_or_else(|| "100".into(), |weight| weight.to_string()),
            cpu_max: config.cpu_max.as_ref().map_or_else(
                || format!("max {period_us}"),
                |max| format!("{} {}", max.quota_us, max.period_us),
            ),
            launch_quota_us: config.cpu_max.as_ref().map(|max| max.quota_us),
            period_us,
            fallback: crate::actuation::RootlessActuator::new(settings),
            written: Default::default(),
            detail: None,
            memory_high: config
                .memory_high_mib
                .map_or_else(|| "max".into(), |mib| mib.saturating_mul(MIB).to_string()),
            requested_ram_mib: settings.requested.ram_mib,
            ram_noise_bytes: settings.ram_noise_tolerance_mib.saturating_mul(MIB),
            ram_calm_window: settings.ram_calm_window,
            ram_cap: None,
            ram_refused: None,
        }
    }

    /// The cap quota per period, never above the launch-time quota.
    fn quota_us(&self, target: Option<u64>) -> u64 {
        let quota = target
            .unwrap_or(self.requested_cpu / 2)
            .saturating_mul(self.period_us)
            .div_ceil(1000)
            .max(1000);
        self.launch_quota_us
            .map_or(quota, |launch| quota.min(launch))
    }

    fn authorized(&self, control: &str) -> bool {
        self.authorized.contains(control)
    }

    fn write(
        &mut self,
        family: &dyn Family,
        kind: ResourceKind,
        control: &'static str,
        value: &str,
    ) -> Result<()> {
        let verified = family.identities().iter().map(|id| id.pid).collect();
        verify_runtime_target(&self.leaf, &verified, control)?;
        // Recorded before the write: a failed readback may still have changed it.
        self.written.entry(kind).or_default().insert(control);
        write_checked(&self.leaf, control, value)?;
        Ok(())
    }

    /// Return this resource's controls to their launch values. A freeze another
    /// resource still holds stays; a refused restore stays recorded for a retry.
    fn restore(&mut self, family: &dyn Family, kind: ResourceKind) -> Result<()> {
        let verified = family.identities().iter().map(|id| id.pid).collect();
        let mut kept = BTreeSet::new();
        let mut result = Ok(());
        for control in self.written.remove(&kind).unwrap_or_default() {
            if self.written.values().any(|held| held.contains(control)) {
                continue;
            }
            let value = match control {
                "cpu.weight" => self.weight.clone(),
                "cpu.max" => self.cpu_max.clone(),
                "memory.high" => self.memory_high.clone(),
                _ => "0".into(),
            };
            if let Err(error) = runtime_write(&self.leaf, &verified, control, &value) {
                kept.insert(control);
                result = Err(error);
            }
        }
        if !kept.is_empty() {
            self.written.insert(kind, kept);
        } else if kind == ResourceKind::Ram {
            self.ram_cap = None;
        }
        result
    }

    /// Cap RAM through `memory.high`, never below what the leaf can reclaim, below
    /// a Guaranteed request, or above the launch limit. A refused write or readback
    /// makes the cap unavailable for this allocation.
    fn cap_ram(&mut self, family: &dyn Family, target: Option<u64>) -> Result<()> {
        let target_bytes = target.unwrap_or(0).saturating_mul(MIB);
        let planned = (|| -> Result<(u64, u64)> {
            let swap = self
                .leaf
                .read("memory.swap.max")
                .map_or(true, |value| value.trim() != "0")
                && self.leaf.host_swap_bytes().unwrap_or(0) > 0;
            let floor =
                ram_cap_floor_bytes(&self.leaf.read("memory.stat")?, swap, self.ram_noise_bytes)?;
            let guaranteed = if self.class == AllocationClass::Guaranteed {
                self.requested_ram_mib.saturating_mul(MIB)
            } else {
                0
            };
            let mut bytes = target_bytes.max(floor).max(guaranteed);
            if let Ok(Limit::Value(launch)) = self.memory_high.parse::<Limit>() {
                bytes = bytes.min(launch);
            }
            Ok((bytes.div_ceil(MIB).saturating_mul(MIB), floor))
        })();
        let result = planned.and_then(|(bytes, floor)| {
            self.write(family, ResourceKind::Ram, "memory.high", &bytes.to_string())?;
            self.ram_cap = Some(RamCap {
                mib: bytes / MIB,
                growth_stop: floor > target_bytes && bytes > target_bytes,
                high_events: memory_high_events(&self.leaf)?,
                current: memory_current(&self.leaf)?,
                throttled_since: None,
            });
            Ok(())
        });
        if let Err(error) = result {
            self.ram_refused = Some("ram_cap_refused");
            let _ = self.restore(family, ResourceKind::Ram);
            return Err(error);
        }
        Ok(())
    }

    /// A cap that keeps throttling for a calm window while usage does not fall by
    /// the noise tolerance frees nothing: raise it to current usage, so it only
    /// stops growth, and leave the rest to the hint.
    fn check_ram_cap(&mut self, family: &dyn Family) -> Result<()> {
        if self.ram_refused.is_some() {
            return Ok(());
        }
        let Some(cap) = self.ram_cap.as_mut() else {
            return Ok(());
        };
        let high = memory_high_events(&self.leaf)?;
        let current = memory_current(&self.leaf)?;
        let throttled = high > cap.high_events;
        cap.high_events = high;
        if current.saturating_add(self.ram_noise_bytes) <= cap.current || !throttled {
            if current.saturating_add(self.ram_noise_bytes) <= cap.current {
                cap.current = current;
            }
            cap.throttled_since = None;
            return Ok(());
        }
        let since = *cap.throttled_since.get_or_insert_with(Instant::now);
        if since.elapsed() < self.ram_calm_window {
            return Ok(());
        }
        let raised = current.div_ceil(MIB).max(cap.mib);
        self.write(
            family,
            ResourceKind::Ram,
            "memory.high",
            &raised.saturating_mul(MIB).to_string(),
        )?;
        if let Some(cap) = self.ram_cap.as_mut() {
            cap.mib = raised;
            cap.growth_stop = true;
        }
        self.ram_refused = Some("ram_cap_ineffective");
        self.detail = Some(format!(
            "ram_cap_ineffective: memory.high throttled for {} ms without freeing {} MiB; raised to current usage, {raised} MiB",
            self.ram_calm_window.as_millis(),
            self.ram_noise_bytes / MIB
        ));
        Ok(())
    }

    /// Freeze and confirm through `cgroup.events` within a bound.
    fn freeze(&mut self, family: &dyn Family, kind: ResourceKind) -> Result<()> {
        self.write(family, kind, "cgroup.freeze", "1")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let events = self.leaf.read("cgroup.events")?;
            if events
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["frozen", "1"])
            {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                self.restore(family, kind)?;
                bail!("cgroup did not report frozen within its bound");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Apply a refused step through the rootless mechanism, which reaches only
    /// the verified family, and keep the refusal for the adjust report.
    fn fall_back(
        &mut self,
        family: &mut dyn Family,
        kind: ResourceKind,
        step: YieldStep,
        target: Option<u64>,
        refusal: anyhow::Error,
    ) -> Result<()> {
        if self.fallback.unavailable(kind).contains(&step) {
            return Err(refusal);
        }
        self.fallback
            .apply(family, kind, step, target)
            .with_context(|| format!("{refusal:#}"))?;
        self.detail = Some(format!(
            "cgroup write refused, rootless {step:?} applied: {refusal:#}"
        ));
        Ok(())
    }
}

impl<L: LeafIo> Actuator for CgroupActuator<L> {
    fn unavailable(&self, kind: ResourceKind) -> BTreeSet<YieldStep> {
        let fallback = self.fallback.unavailable(kind);
        let mut steps = BTreeSet::from([YieldStep::Hint]);
        match kind {
            ResourceKind::Cpu => {
                if !self.authorized("cpu.weight") {
                    steps.insert(YieldStep::LowerShare);
                }
                if !self.authorized("cpu.max") && fallback.contains(&YieldStep::Cap) {
                    steps.insert(YieldStep::Cap);
                }
                if !self.authorized("cgroup.freeze") && fallback.contains(&YieldStep::Pause) {
                    steps.insert(YieldStep::Pause);
                }
            }
            ResourceKind::GpuCompute => {
                steps.extend([YieldStep::LowerShare, YieldStep::Cap]);
                if !self.authorized("cgroup.freeze") && fallback.contains(&YieldStep::Pause) {
                    steps.insert(YieldStep::Pause);
                }
            }
            ResourceKind::Ram => {
                steps.extend([YieldStep::LowerShare, YieldStep::Pause]);
                if !self.authorized("memory.high") || self.ram_refused.is_some() {
                    steps.insert(YieldStep::Cap);
                }
            }
            ResourceKind::GpuMemory => {
                steps.extend([YieldStep::LowerShare, YieldStep::Cap, YieldStep::Pause]);
            }
        }
        // Guaranteed work is never paused (FR-024).
        if self.class == AllocationClass::Guaranteed {
            steps.insert(YieldStep::Pause);
        }
        steps
    }

    fn apply(
        &mut self,
        family: &mut dyn Family,
        kind: ResourceKind,
        step: YieldStep,
        target: Option<u64>,
    ) -> Result<YieldStep> {
        ensure!(
            step < YieldStep::Drain
                && (step == YieldStep::None || !self.unavailable(kind).contains(&step)),
            "step {step:?} is not a runtime step for {kind:?} here"
        );
        // One step per resource: undo the previous one before applying the next.
        self.restore(family, kind)?;
        self.fallback.apply(family, kind, YieldStep::None, None)?;
        match (kind, step) {
            (_, YieldStep::None) => {}
            (ResourceKind::Cpu, YieldStep::LowerShare) => {
                self.write(family, kind, "cpu.weight", "1")?;
            }
            (ResourceKind::Cpu, YieldStep::Cap) if self.authorized("cpu.max") => {
                let value = format!("{} {}", self.quota_us(target), self.period_us);
                if let Err(refusal) = self.write(family, kind, "cpu.max", &value) {
                    self.fall_back(family, kind, step, target, refusal)?;
                }
            }
            (_, YieldStep::Pause) if self.authorized("cgroup.freeze") => {
                if let Err(refusal) = self.freeze(family, kind) {
                    self.fall_back(family, kind, step, target, refusal)?;
                }
            }
            (ResourceKind::Ram, YieldStep::Cap) => {
                self.cap_ram(family, target)?;
            }
            (_, step) => {
                self.fallback.apply(family, kind, step, target)?;
            }
        }
        Ok(step)
    }

    fn tick(&mut self, family: &mut dyn Family) -> Result<()> {
        self.fallback.tick(family)?;
        self.check_ram_cap(family)
    }

    /// The duty cycle's next phase switch and, while a RAM cap holds, its next check.
    fn next_deadline(&self) -> Option<Instant> {
        let ram = (self.ram_cap.is_some() && self.ram_refused.is_none())
            .then(|| Instant::now() + RAM_CAP_CHECK);
        match (self.fallback.next_deadline(), ram) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn take_detail(&mut self) -> Option<String> {
        self.detail.take()
    }

    fn applied_target(&self, kind: ResourceKind) -> Option<u64> {
        match kind {
            ResourceKind::Ram => self.ram_cap.as_ref().map(|cap| cap.mib),
            _ => None,
        }
    }

    fn step_code(&self, kind: ResourceKind) -> Option<&'static str> {
        match kind {
            ResourceKind::Ram => self.ram_refused.or_else(|| {
                self.ram_cap
                    .as_ref()
                    .filter(|cap| cap.growth_stop)
                    .map(|_| "ram_cap_growth_stop")
            }),
            _ => None,
        }
    }

    fn reconcile(
        &mut self,
        family: &mut dyn Family,
        applied: &BTreeMap<ResourceKey, StepRequest>,
    ) -> Result<()> {
        if !self.authorized("memory.high") || self.ram_cap.is_some() {
            return Ok(());
        }
        let current = canonical_control("memory.high", &self.leaf.read("memory.high")?)?;
        if current == canonical_control("memory.high", &self.memory_high)? {
            return Ok(());
        }
        let verified = family.identities().iter().map(|id| id.pid).collect();
        verify_runtime_target(&self.leaf, &verified, "memory.high")?;
        let recorded = applied
            .get(&ResourceKey::Ram)
            .is_some_and(|step| step.step == YieldStep::Cap);
        match current.parse::<Limit>()? {
            Limit::Value(bytes) if recorded => {
                self.written
                    .entry(ResourceKind::Ram)
                    .or_default()
                    .insert("memory.high");
                self.ram_cap = Some(RamCap {
                    mib: bytes.div_ceil(MIB),
                    growth_stop: false,
                    high_events: memory_high_events(&self.leaf)?,
                    current: memory_current(&self.leaf)?,
                    throttled_since: None,
                });
            }
            _ => {
                write_checked(&self.leaf, "memory.high", &self.memory_high)?;
            }
        }
        Ok(())
    }

    fn release(&mut self, family: &mut dyn Family) -> Result<()> {
        let kinds: Vec<_> = self.written.keys().copied().collect();
        let mut result = Ok(());
        for kind in kinds {
            if let Err(error) = self.restore(family, kind) {
                result = Err(error);
            }
        }
        self.fallback.release(family)?;
        result
    }
}

/// A value with explicit scope and availability. Missing files never mean zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupReading {
    pub observed_at_unix_ms: Option<u64>,
    pub scope: PathBuf,
    pub control: String,
    pub value: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct CgroupBackend {
    config: CgroupConfig,
    evidence: Vec<ControlEvidence>,
    assignment: Option<String>,
    identity: Option<ProcessIdentity>,
    #[cfg(target_os = "linux")]
    runtime: linux::Runtime,
}

impl CgroupBackend {
    /// Opens and verifies existing directories only. All writes are deferred to
    /// prepare(), after explicit authorization and job-control validation.
    pub fn new(config: CgroupConfig) -> Result<Self> {
        config.validate()?;
        ensure!(
            config.enabled,
            "cgroup backend is disabled; use the rootless baseline"
        );
        #[cfg(target_os = "linux")]
        {
            let runtime = linux::Runtime::open(&config)?;
            Ok(Self {
                config,
                runtime,
                evidence: vec![],
                assignment: None,
                identity: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!(
                "cgroup v2 execution is unavailable on this platform; Linux runtime behavior is unverified here"
            )
        }
    }

    pub fn control_evidence(&self) -> &[ControlEvidence] {
        &self.evidence
    }

    /// Visible ancestor restrictions and leaf accounting. These observations do
    /// not prove protection from competitors in other hierarchy placements.
    pub fn accounting(&self) -> Vec<CgroupReading> {
        #[cfg(target_os = "linux")]
        {
            self.runtime.accounting()
        }
        #[cfg(not(target_os = "linux"))]
        {
            vec![]
        }
    }

    fn check_required(&self, request: &LaunchRequest) -> Result<()> {
        check_required_controls(&self.config, &request.required_controls)
    }
}

fn check_required_controls(config: &CgroupConfig, required: &[String]) -> Result<()> {
    let settings = config.settings();
    for control in required {
        ensure!(
            control == "cgroup.procs"
                || control == "process_handle"
                || control == "cpu.nice"
                || settings.iter().any(|s| s.0 == control),
            "required control is not configured by cgroup backend: {control}"
        );
    }
    // process_handle and requested cpu.nice are verified by the owning supervisor;
    // accepting the requirement here never supplies backend enforcement evidence.
    Ok(())
}

impl LaunchBackend for CgroupBackend {
    fn name(&self) -> &str {
        "cgroup_v2"
    }

    /// Runtime steps write the pinned leaf; without one, the rootless mechanisms apply.
    fn actuator(&self, settings: &ActuatorSettings) -> Box<dyn Actuator> {
        #[cfg(target_os = "linux")]
        {
            if let Ok(leaf) = self.runtime.leaf_handle() {
                return Box::new(CgroupActuator::new(leaf, &self.config, settings));
            }
        }
        Box::new(crate::actuation::RootlessActuator::new(settings))
    }

    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        self.evidence.clone()
    }

    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup> {
        self.check_required(request)?;
        ensure!(
            self.assignment.is_none(),
            "cgroup backend already owns an allocation; reconcile it before reuse"
        );
        #[cfg(target_os = "linux")]
        {
            self.runtime.create_leaf()?;
            self.assignment = Some(request.assignment_id.clone());
            let path = self.runtime.leaf_path()?.to_owned();
            self.evidence.clear();
            self.evidence.push(ControlEvidence {
                control: "cgroup.procs".into(), available: Some(true), permitted: None,
                configured: true, applied: false, fallback: false,
                scope: path.display().to_string(), requested: Some("gate self-join before user exec".into()),
                effective: None, detail: "Owned leaf prepared; membership has not yet been verified. User execution remains forbidden.".into(),
            });
            apply_settings(&self.runtime, &self.config, &path, &mut self.evidence)?;
            Ok(GateSetup {
                cgroup_path: Some(path),
                nice: None,
                oom_score_adj: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }

    fn verify(&mut self, identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>> {
        ensure!(
            self.assignment.as_deref() == Some(&identity.assignment_id),
            "cgroup assignment identity mismatch"
        );
        if let Some(previous) = &self.identity {
            ensure!(
                previous == identity,
                "cgroup process or attempt identity changed"
            );
        }
        #[cfg(target_os = "linux")]
        {
            self.runtime.verify_membership(identity)?;
            verify_settings(&self.runtime, &self.config)?;
            self.identity = Some(identity.clone());
            let scope = self.runtime.leaf_path()?.display().to_string();
            self.evidence.retain(|e| e.control != "cgroup.procs");
            self.evidence.push(ControlEvidence { control: "cgroup.procs".into(), available: Some(true), permitted: Some(true), configured: true, applied: true, fallback: false, scope: scope.clone(), requested: Some("gate self-join before user exec".into()), effective: Some(format!("pid {} in exact workload leaf", identity.pid)), detail: "Boot/start identity and sole pre-exec membership verified. This is not proof of ownership of future descendants.".into() });
            if self.config.allow_kill {
                self.evidence.retain(|e| e.control != "cgroup.kill");
                self.evidence.push(ControlEvidence { control: "cgroup.kill".into(), available: self.runtime.has_leaf_control("cgroup.kill"), permitted: None, configured: true, applied: false, fallback: true, scope: scope.clone(), requested: Some("authorized subtree cleanup".into()), effective: None, detail: "Subtree killing is not implemented without a complete verified member registry. Nonempty cleanup requires reconciliation; authorization alone is insufficient.".into() });
            }
            self.evidence.retain(|e| e.control != "cgroup.hierarchy");
            self.evidence.push(ControlEvidence { control: "cgroup.hierarchy".into(), available: Some(true), permitted: None, configured: false, applied: false, fallback: false, scope, requested: None, effective: Some(serde_json::to_string(&self.accounting())?), detail: "Read-only leaf and visible ancestor observations. Hidden ancestors and competitor placement require deployment-level comparison records; weight is relative within this hierarchy.".into() });
            Ok(self.evidence.clone())
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }

    fn confirm_release(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.runtime.remove_empty_leaf()?;
            self.assignment = None;
            self.identity = None;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }
}

// A private test seam exercises the exact production preparation algorithm.
#[cfg(any(target_os = "linux", test))]
trait ControlIo {
    fn assert_empty(&self) -> Result<()>;
    fn read(&self, control: &str) -> Result<String>;
    fn write(&self, control: &str, value: &str) -> Result<()>;
}

#[cfg(any(target_os = "linux", test))]
fn apply_settings(
    io: &impl ControlIo,
    config: &CgroupConfig,
    path: &Path,
    evidence: &mut Vec<ControlEvidence>,
) -> Result<()> {
    io.assert_empty()?;
    for (control, requested) in config.settings() {
        let mut entry = ControlEvidence {
            control: control.into(),
            available: None,
            permitted: None,
            configured: true,
            applied: false,
            fallback: false,
            scope: path.display().to_string(),
            requested: Some(requested.clone()),
            effective: None,
            detail: String::new(),
        };
        let result: Result<()> = (|| {
            io.assert_empty()?;
            if let Err(error) = io.read(control) {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    entry.available = Some(false);
                }
                return Err(error).context("control must exist before write");
            }
            entry.available = Some(true);
            if let Err(error) = io.write(control, &requested) {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    entry.permitted = Some(false);
                }
                return Err(error);
            }
            entry.permitted = Some(true);
            let actual = canonical_control(control, &io.read(control)?)?;
            entry.effective = Some(actual.clone());
            ensure!(
                actual == canonical_control(control, &requested)?,
                "{control} readback differs from requested setting"
            );
            entry.applied = true;
            entry.detail = match control {
                "cpu.weight" => "Relative hierarchical weight, not a percentage or CPU guarantee; effect depends on ancestors and competitor placement.",
                "cpu.max" => "Explicit CPU bandwidth ceiling; ancestor restrictions still apply.",
                "memory.high" => "Explicit RAM reclaim/throttling threshold; not a hard ceiling or GPU VRAM enforcement.",
                "memory.max" => "Explicit RAM hard limit with possible OOM consequences; not GPU VRAM enforcement.",
                _ => "Verified before workload execution.",
            }.into();
            Ok(())
        })();
        if let Err(error) = result {
            entry.detail =
                format!("Preparation failed; user execution remains forbidden: {error:#}");
            evidence.push(entry);
            return Err(error).with_context(|| format!("preparing {control}"));
        }
        evidence.push(entry);
    }
    io.assert_empty()?;
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn verify_settings(io: &impl ControlIo, config: &CgroupConfig) -> Result<()> {
    for (control, requested) in config.settings() {
        ensure!(
            canonical_control(control, &io.read(control)?)?
                == canonical_control(control, &requested)?,
            "{control} changed before launch authorization"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        ffi::CString,
        fs::{self, File},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    #[derive(Debug)]
    struct Directory {
        file: File,
    }

    impl Directory {
        fn from_fd(fd: libc::c_int) -> Result<Self> {
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: a successful open/openat returns a new owned descriptor.
            Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
            })
        }

        fn open_absolute(path: &Path) -> Result<Self> {
            let root = CString::new("/")?;
            // SAFETY: valid C string; flags request only a directory descriptor.
            let mut current = Self::from_fd(unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            for part in path.components() {
                match part {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        current = current.open_child(name.as_bytes())?;
                    }
                    _ => bail!("unsafe directory path component"),
                }
            }
            Ok(current)
        }

        fn open_child(&self, name: &[u8]) -> Result<Self> {
            ensure!(
                !name.contains(&b'/'),
                "child name must be one path component"
            );
            let name = CString::new(name)?;
            // SAFETY: retained directory fd and valid single-component name.
            Self::from_fd(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })
        }

        fn stat(&self) -> Result<libc::stat> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: valid descriptor and output pointer; initialized on success.
            if unsafe { libc::fstat(self.file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fstat succeeded.
            Ok(unsafe { stat.assume_init() })
        }

        fn try_clone(&self) -> Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
            })
        }

        fn same(&self, other: &Self) -> Result<bool> {
            let a = self.stat()?;
            let b = other.stat()?;
            Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
        }

        fn verify_cgroup2(&self) -> Result<()> {
            let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: valid descriptor and output pointer; initialized on success.
            if unsafe { libc::fstatfs(self.file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fstatfs succeeded.
            ensure!(
                unsafe { stat.assume_init() }.f_type == 0x6367_7270,
                "delegated_root is not on a real cgroup v2 filesystem"
            );
            Ok(())
        }

        fn control(&self, name: &str, write: bool) -> Result<File> {
            ensure!(
                !name.contains('/') && !name.contains('\0') && name != "." && name != "..",
                "invalid control file name"
            );
            let name = CString::new(name)?;
            let flags = (if write {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            }) | libc::O_NOFOLLOW
                | libc::O_CLOEXEC;
            // SAFETY: valid retained fd; no O_CREAT, path traversal or symlinks.
            let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: openat returned a newly owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        fn read(&self, name: &str) -> Result<String> {
            let mut output = String::new();
            self.control(name, false)?
                .take(1_048_577)
                .read_to_string(&mut output)?;
            ensure!(
                output.len() <= 1_048_576,
                "cgroup file exceeds bounded read size"
            );
            Ok(output)
        }

        fn empty(&self) -> Result<()> {
            ensure!(
                self.read("cgroup.procs")?.trim().is_empty(),
                "workload cgroup contains live processes; capacity remains reserved"
            );
            let events = self.read("cgroup.events")?;
            ensure!(
                events
                    .lines()
                    .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["populated", "0"]),
                "cgroup populated state is unknown or nonempty; capacity remains reserved"
            );
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Leaf {
        directory: Directory,
        name: CString,
        path: PathBuf,
        membership: PathBuf,
    }

    #[derive(Debug)]
    pub(super) struct Runtime {
        root: Directory,
        root_path: PathBuf,
        mountpoint: PathBuf,
        membership_root: PathBuf,
        leaf: Option<Leaf>,
        unreconciled_leaf: Option<PathBuf>,
    }

    impl Runtime {
        pub(super) fn open(config: &CgroupConfig) -> Result<Self> {
            let root_path = config
                .delegated_root
                .as_ref()
                .context("missing delegated root")?
                .clone();
            let root = Directory::open_absolute(&root_path)
                .context("open explicitly authorized subtree without symlinks")?;
            root.verify_cgroup2()?;
            ensure!(
                root.read("cgroup.type")?.trim() == "domain",
                "delegated subtree must be a valid domain cgroup; no hierarchy rearrangement is performed"
            );
            ensure!(
                root.read("cgroup.procs")?.trim().is_empty(),
                "delegated root contains processes; backend will not relocate them"
            );
            let enabled = root.read("cgroup.subtree_control")?;
            let available = root.read("cgroup.controllers")?;
            for (control, _) in config.settings() {
                let controller = control.split('.').next().context("invalid controller")?;
                ensure!(
                    enabled.split_whitespace().any(|c| c == controller)
                        && available.split_whitespace().any(|c| c == controller),
                    "{controller} must already be delegated and enabled for child cgroups; backend will not write subtree_control"
                );
            }
            let (mountpoint, mountroot) =
                find_mount(&fs::read_to_string("/proc/self/mountinfo")?, &root_path)?;
            let membership_root = mountroot.join(root_path.strip_prefix(&mountpoint)?);
            Ok(Self {
                root,
                root_path,
                mountpoint,
                membership_root,
                leaf: None,
                unreconciled_leaf: None,
            })
        }

        fn verify_paths(&self) -> Result<()> {
            ensure!(
                self.root
                    .same(&Directory::open_absolute(&self.root_path)?)?,
                "delegated root was replaced; refuse mutations"
            );
            if let Some(leaf) = &self.leaf {
                ensure!(
                    leaf.directory
                        .same(&self.root.open_child(leaf.name.as_bytes())?)?,
                    "owned workload leaf was replaced; refuse mutations"
                );
            }
            Ok(())
        }

        pub(super) fn create_leaf(&mut self) -> Result<()> {
            ensure!(
                self.leaf.is_none() && self.unreconciled_leaf.is_none(),
                "existing owned cgroup requires reconciliation"
            );
            self.verify_paths()?;
            ensure!(
                self.root.read("cgroup.procs")?.trim().is_empty(),
                "delegated root acquired processes; backend will not relocate them"
            );
            let mut random = [0_u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let name = format!(
                "cedegrid-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let c_name = CString::new(name.clone())?;
            // SAFETY: retained verified root fd, fresh random component; exclusive
            // mkdir never adopts an existing leaf, even for the same assignment.
            if unsafe { libc::mkdirat(self.root.file.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.unreconciled_leaf = Some(self.root_path.join(&name));
            let opened = self.root.open_child(name.as_bytes());
            let directory = match opened {
                Ok(directory) => directory,
                // We cannot verify ownership through a descriptor after open
                // failed, so do not attempt a guessed path-based deletion.
                Err(error) => return Err(error).context(format!("created leaf {} but could not pin its identity; manual reconciliation required", self.root_path.join(&name).display())),
            };
            self.leaf = Some(Leaf {
                directory,
                name: c_name,
                path: self.root_path.join(&name),
                membership: self.membership_root.join(name),
            });
            self.unreconciled_leaf = None;
            let leaf = self.leaf.as_ref().context("leaf not created")?;
            leaf.directory.verify_cgroup2()?;
            ensure!(
                leaf.directory.read("cgroup.type")?.trim() == "domain",
                "new workload cgroup has invalid hierarchy type"
            );
            leaf.directory.empty()?;
            self.verify_paths()?;
            Ok(())
        }

        /// Duplicated descriptors of the pinned root and leaf for runtime steps.
        pub(super) fn leaf_handle(&self) -> Result<LeafHandle> {
            let leaf = self.leaf.as_ref().context("no prepared cgroup")?;
            Ok(LeafHandle {
                root: self.root.try_clone()?,
                root_path: self.root_path.clone(),
                leaf: leaf.directory.try_clone()?,
                name: leaf.name.clone(),
            })
        }

        pub(super) fn leaf_path(&self) -> Result<&Path> {
            Ok(&self.leaf.as_ref().context("no prepared cgroup")?.path)
        }
        pub(super) fn read_leaf(&self, control: &str) -> Result<String> {
            self.leaf
                .as_ref()
                .context("no prepared cgroup")?
                .directory
                .read(control)
        }
        pub(super) fn has_leaf_control(&self, control: &str) -> Option<bool> {
            match self.leaf.as_ref()?.directory.control(control, false) {
                Ok(_) => Some(true),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Some(false)
                }
                Err(_) => None,
            }
        }

        pub(super) fn verify_membership(&self, identity: &ProcessIdentity) -> Result<()> {
            self.verify_paths()?;
            let leaf = self.leaf.as_ref().context("no prepared cgroup")?;
            ensure!(
                fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() == identity.boot_id,
                "process boot identity changed"
            );
            let stat = fs::read_to_string(format!("/proc/{}/stat", identity.pid))?;
            let fields = stat
                .rsplit_once(')')
                .context("invalid proc stat")?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            ensure!(
                fields
                    .get(19)
                    .context("proc stat lacks starttime")?
                    .parse::<u64>()?
                    == identity.start_time,
                "process start identity changed"
            );
            let membership = fs::read_to_string(format!("/proc/{}/cgroup", identity.pid))?;
            let paths = membership
                .lines()
                .filter_map(|line| line.strip_prefix("0::"))
                .collect::<Vec<_>>();
            ensure!(
                paths.len() == 1 && Path::new(paths[0]) == leaf.membership,
                "gate is not in the exact prepared cgroup"
            );
            let pids = leaf
                .directory
                .read("cgroup.procs")?
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<std::result::Result<BTreeSet<_>, _>>()?;
            ensure!(
                pids == BTreeSet::from([identity.pid]),
                "unfamiliar members in pre-execution cgroup; refuse launch"
            );
            Ok(())
        }

        pub(super) fn remove_empty_leaf(&mut self) -> Result<()> {
            ensure!(
                self.unreconciled_leaf.is_none(),
                "created leaf identity could not be pinned; manual reconciliation required before release"
            );
            let Some(leaf) = &self.leaf else {
                return Ok(());
            };
            self.verify_paths()?;
            leaf.directory.empty()?;
            // SAFETY: retained verified parent fd, exact freshly owned leaf.
            // Kernel rmdir atomically refuses a populated group or child groups.
            if unsafe {
                libc::unlinkat(
                    self.root.file.as_raw_fd(),
                    leaf.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.leaf = None;
            Ok(())
        }

        pub(super) fn accounting(&self) -> Vec<CgroupReading> {
            let mut readings = Vec::new();
            let observed_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok());
            let mut scope = self
                .leaf
                .as_ref()
                .map(|leaf| leaf.path.as_path())
                .unwrap_or(&self.root_path);
            while scope.starts_with(&self.mountpoint) {
                let directory = Directory::open_absolute(scope);
                for control in [
                    "cpu.weight",
                    "cpu.max",
                    "cpuset.cpus.effective",
                    "cpuset.mems.effective",
                    "cpu.stat",
                    "memory.current",
                    "memory.high",
                    "memory.max",
                    "memory.events",
                    "cgroup.events",
                ] {
                    let value = directory
                        .as_ref()
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|dir| dir.read(control).map_err(|e| format!("{e:#}")));
                    readings.push(CgroupReading {
                        observed_at_unix_ms,
                        scope: scope.to_owned(),
                        control: control.into(),
                        value: value.as_ref().ok().cloned(),
                        error: value.err(),
                    });
                }
                if scope == self.mountpoint {
                    break;
                }
                let Some(parent) = scope.parent() else {
                    break;
                };
                scope = parent;
            }
            readings
        }
    }

    /// The allocation's leaf as pinned at creation; writes go through its descriptor.
    pub(super) struct LeafHandle {
        root: Directory,
        root_path: PathBuf,
        leaf: Directory,
        name: CString,
    }

    impl LeafIo for LeafHandle {
        fn same_leaf(&self) -> Result<bool> {
            Ok(self
                .root
                .same(&Directory::open_absolute(&self.root_path)?)?
                && self
                    .leaf
                    .same(&self.root.open_child(self.name.as_bytes())?)?)
        }
        fn read(&self, control: &str) -> Result<String> {
            self.leaf.read(control)
        }
        fn write(&self, control: &str, value: &str) -> Result<()> {
            self.leaf
                .control(control, true)?
                .write_all(value.as_bytes())?;
            Ok(())
        }
    }

    impl ControlIo for Runtime {
        fn assert_empty(&self) -> Result<()> {
            self.verify_paths()?;
            self.leaf
                .as_ref()
                .context("no prepared leaf")?
                .directory
                .empty()
        }
        fn read(&self, control: &str) -> Result<String> {
            self.read_leaf(control)
        }
        fn write(&self, control: &str, value: &str) -> Result<()> {
            self.assert_empty()?;
            let directory = &self.leaf.as_ref().context("no prepared leaf")?.directory;
            directory
                .control(control, true)?
                .write_all(value.as_bytes())?;
            Ok(())
        }
    }

    fn unescape_mount(value: &str) -> Result<PathBuf> {
        let mut bytes = Vec::new();
        let input = value.as_bytes();
        let mut offset = 0;
        while offset < input.len() {
            if input[offset] == b'\\' {
                ensure!(offset + 3 < input.len(), "invalid mountinfo escape");
                let octal = &input[offset + 1..offset + 4];
                ensure!(
                    octal.iter().all(|b| (b'0'..=b'7').contains(b)),
                    "invalid mountinfo octal escape"
                );
                let value = (u16::from(octal[0] - b'0') * 64)
                    + (u16::from(octal[1] - b'0') * 8)
                    + u16::from(octal[2] - b'0');
                ensure!(value > 0 && value <= 255, "invalid mountinfo byte");
                bytes.push(value as u8);
                offset += 4;
            } else {
                bytes.push(input[offset]);
                offset += 1;
            }
        }
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }

    fn find_mount(mountinfo: &str, delegated: &Path) -> Result<(PathBuf, PathBuf)> {
        let mut candidates = Vec::new();
        for line in mountinfo.lines() {
            let Some((before, after)) = line.split_once(" - ") else {
                continue;
            };
            if after.split_whitespace().next() != Some("cgroup2") {
                continue;
            }
            let fields: Vec<_> = before.split_whitespace().collect();
            if fields.len() < 6 {
                continue;
            }
            let mountpoint = unescape_mount(fields[4])?;
            let root = unescape_mount(fields[3])?;
            if delegated.starts_with(&mountpoint) {
                candidates.push((mountpoint, root));
            }
        }
        candidates
            .into_iter()
            .max_by_key(|(path, _)| path.as_os_str().len())
            .context("delegated cgroup2 mount not visible in current mount namespace")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn mount_resolution_preserves_bind_root_and_escapes() {
            let (mount, root) = find_mount("1 2 0:3 /tenant /sys/fs/cgroup rw - cgroup2 none rw\n2 3 0:3 /nested /tmp/cg\\040space rw - cgroup2 none rw", Path::new("/tmp/cg space/authorized")).unwrap();
            assert_eq!(mount, Path::new("/tmp/cg space"));
            assert_eq!(root, Path::new("/nested"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };
    #[derive(Default)]
    struct FakeIo {
        values: RefCell<BTreeMap<String, String>>,
        writes: RefCell<Vec<String>>,
        deny: Option<String>,
        corrupt: Option<String>,
        nonempty_after: Option<usize>,
        checks: Cell<usize>,
    }
    impl ControlIo for FakeIo {
        fn assert_empty(&self) -> Result<()> {
            let checks = self.checks.get();
            self.checks.set(checks + 1);
            ensure!(
                self.nonempty_after.is_none_or(|limit| checks < limit),
                "unfamiliar process appeared"
            );
            Ok(())
        }
        fn read(&self, control: &str) -> Result<String> {
            self.values
                .borrow()
                .get(control)
                .cloned()
                .context("control unavailable")
        }
        fn write(&self, control: &str, value: &str) -> Result<()> {
            if self.deny.as_deref() == Some(control) {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
            }
            self.writes.borrow_mut().push(control.into());
            self.values.borrow_mut().insert(
                control.into(),
                if self.corrupt.as_deref() == Some(control) {
                    "999".into()
                } else {
                    value.into()
                },
            );
            Ok(())
        }
    }
    fn configured() -> CgroupConfig {
        CgroupConfig {
            memory_max_mib: Some(64),
            ..CgroupConfig::default()
        }
    }
    fn fake() -> FakeIo {
        FakeIo {
            values: RefCell::new(BTreeMap::from([
                ("cpu.weight".into(), "100".into()),
                ("memory.max".into(), "max".into()),
            ])),
            ..FakeIo::default()
        }
    }
    #[test]
    fn partial_permission_failure_preserves_applied_evidence() {
        let io = FakeIo {
            deny: Some("memory.max".into()),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(
            apply_settings(
                &io,
                &configured(),
                Path::new("/authorized/owned"),
                &mut evidence
            )
            .is_err()
        );
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
        assert!(evidence[0].applied);
        assert!(!evidence[1].applied);
        assert_eq!(evidence[1].permitted, Some(false));
    }
    #[test]
    fn unavailable_control_is_not_reported_applied() {
        let io = FakeIo::default();
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert!(io.writes.borrow().is_empty());
        assert!(!evidence[0].applied);
    }
    #[test]
    fn readback_mismatch_blocks_execution() {
        let io = FakeIo {
            corrupt: Some("cpu.weight".into()),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert_eq!(evidence[0].effective.as_deref(), Some("999"));
        assert!(!evidence[0].applied);
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
    }
    #[test]
    fn appeared_process_prevents_further_writes() {
        let io = FakeIo {
            nonempty_after: Some(2),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
    }
    #[test]
    fn verification_detects_later_control_changes() {
        let io = fake();
        let mut evidence = Vec::new();
        apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).unwrap();
        io.values
            .borrow_mut()
            .insert("cpu.weight".into(), "100".into());
        assert!(verify_settings(&io, &configured()).is_err());
    }
    #[test]
    fn no_default_cpu_bandwidth_or_memory_limit() {
        assert_eq!(
            CgroupConfig::default().settings(),
            vec![("cpu.weight", "10".into())]
        );
    }
    #[test]
    fn supervisor_process_handle_requirement_is_deferred_without_fake_evidence() {
        let config = CgroupConfig::default();
        check_required_controls(
            &config,
            &[
                "process_handle".into(),
                "cpu.nice".into(),
                "cpu.weight".into(),
            ],
        )
        .unwrap();
        assert!(check_required_controls(&config, &["cgroup.kill".into()]).is_err());
        assert!(check_required_controls(&config, &["cpu.max".into()]).is_err());
    }
    #[test]
    fn whitespace_is_canonical_but_extra_fields_are_invalid() {
        assert_eq!(
            canonical_control("cpu.max", " 20000  100000\n").unwrap(),
            "20000 100000"
        );
        assert!(canonical_control("memory.max", "1000 extra").is_err());
    }
}
