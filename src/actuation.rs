//! Supervisor-side actuation of yield steps on verified own processes only.
use crate::contention::{ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass, Family};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// The smallest share of each period a capped family may run.
pub(crate) const MIN_DUTY: f64 = 0.05;

/// The share of each period a GPU compute cap lets own work run: its target in
/// thousandths of the device's time, at least the minimum duty.
pub(crate) fn gpu_compute_duty(target: Option<u64>) -> f64 {
    (target.unwrap_or(500) as f64 / 1000.0).clamp(MIN_DUTY, 1.0)
}

/// The node's duty-cycle clock: CLOCK_MONOTONIC, which every supervisor on the node reads alike,
/// so aligned phases need no coordination (FR-021, Q31). Elsewhere, where duty cycles are
/// unavailable, a process-local monotonic anchor keeps it defined.
pub fn node_clock() -> Duration {
    #[cfg(unix)]
    {
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0
            && time.tv_sec >= 0
            && (0..1_000_000_000).contains(&time.tv_nsec)
        {
            return Duration::new(time.tv_sec as u64, time.tv_nsec as u32);
        }
    }
    static ANCHOR: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ANCHOR.get_or_init(Instant::now).elapsed()
}

/// A GPU duty cycle's phase on the node clock (FR-021, Q31): it runs while the clock's position in
/// the period is below duty × period, and a stop holds until that period ends, so a duty change
/// mid-period never resumes inside an off-phase and never skips one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlignedPhase {
    /// The period, counted on the node clock, in which the cycle last stopped.
    stopped_in: Option<u128>,
}

impl AlignedPhase {
    /// Whether the cycle runs at `now` on the node clock; a stop holds for the rest of its period.
    pub fn running(&mut self, now: Duration, period: Duration, duty: f64) -> bool {
        if duty >= 1.0 {
            self.stopped_in = None;
            return true;
        }
        let (index, position, length) = Self::position(now, period);
        if self.stopped_in == Some(index) {
            return false;
        }
        let on = (position as f64) < duty * length as f64;
        if !on {
            self.stopped_in = Some(index);
        }
        on
    }

    /// When, on the node clock, the cycle next switches: the on-phase's end while running, the
    /// period's end while stopped.
    pub fn next_switch(
        &self,
        now: Duration,
        period: Duration,
        duty: f64,
        running: bool,
    ) -> Duration {
        let (index, position, length) = Self::position(now, period);
        let on = (duty.clamp(0.0, 1.0) * length as f64) as u128;
        let start = index * length;
        let at = if running && position < on {
            start + on
        } else {
            start + length
        };
        Duration::from_nanos(u64::try_from(at).unwrap_or(u64::MAX))
    }

    /// The period's index on the node clock, the position within it and its length, in ns.
    fn position(now: Duration, period: Duration) -> (u128, u128, u128) {
        let length = period.as_nanos().max(1);
        let now = now.as_nanos();
        (now / length, now % length, length)
    }
}

/// Rootless mechanisms (research R4): a SIGSTOP/SIGCONT duty cycle for a CPU cap
/// and for a GPU compute share, and SIGSTOP for a pause, all through owned
/// handles. One cycle serves both caps at the smaller duty. Lower share and memory
/// caps have no rootless runtime mechanism; hints need resource targets (US4).
pub struct RootlessActuator {
    class: AllocationClass,
    requested_cpu: u64,
    period: Duration,
    /// The CPU cap in millicores, when one applies.
    cap: Option<u64>,
    /// Resources that asked for a pause.
    paused: BTreeSet<ResourceKind>,
    stopped: bool,
    /// The share of each period that meets the CPU cap.
    duty: f64,
    /// The share of each period a GPU compute cap allows: its target in
    /// thousandths of the device's time.
    gpu_duty: f64,
    /// Where a CPU-only cycle's current phase began; a cycle with a GPU compute cap follows
    /// `aligned` on the node clock instead (FR-021, Q31).
    phase_started: Instant,
    aligned: AlignedPhase,
    clock: fn() -> Duration,
    /// CPU time and instant at the start of the current running phase.
    run_mark: Option<(Duration, Instant)>,
    /// The family's natural CPU rate in millicores, from its last measured
    /// running phase.
    rate: Option<f64>,
}

impl RootlessActuator {
    pub fn new(settings: &ActuatorSettings) -> Self {
        Self {
            class: settings.class,
            requested_cpu: settings.requested.cpu_millicores.max(1),
            period: settings.period.max(Duration::from_millis(20)),
            cap: None,
            paused: BTreeSet::new(),
            stopped: false,
            duty: 1.0,
            gpu_duty: 1.0,
            phase_started: Instant::now(),
            aligned: AlignedPhase::default(),
            clock: node_clock,
            run_mark: None,
            rate: None,
        }
    }

    fn stop(&mut self, family: &mut dyn Family) -> Result<()> {
        if !self.stopped {
            // Marked first: a stop that reached only part of the family is still continued.
            self.stopped = true;
            family.signal(signal::STOP)?;
        }
        Ok(())
    }

    fn resume(&mut self, family: &mut dyn Family) -> Result<()> {
        if self.stopped {
            family.signal(signal::CONT)?;
            self.stopped = false;
        }
        self.run_mark = family.cpu_time().map(|cpu| (cpu, Instant::now()));
        Ok(())
    }

    /// The duty-cycle period.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// A CPU cap runs as this duty cycle.
    pub fn cpu_capped(&self) -> bool {
        self.cap.is_some()
    }

    /// The share of each period the family runs under the applied caps.
    pub fn duty(&self) -> f64 {
        let cpu = if self.cap.is_some() { self.duty } else { 1.0 };
        cpu.min(self.gpu_duty)
    }

    /// Whether a GPU compute cap is part of the cycle, so it follows the node clock.
    fn aligned(&self) -> bool {
        self.gpu_duty < 1.0
    }

    /// Bring the family to the state the applied steps require. A cycle with a GPU compute cap
    /// takes its aligned phase at once, so a changed duty never resumes inside an off-phase.
    fn enforce(&mut self, family: &mut dyn Family) -> Result<()> {
        if !self.paused.is_empty() {
            self.stop(family)
        } else if self.aligned() {
            let running = self
                .aligned
                .running((self.clock)(), self.period, self.duty());
            if running {
                self.resume(family)
            } else {
                self.stop(family)
            }
        } else {
            self.phase_started = Instant::now();
            self.resume(family)
        }
    }

    /// Measure the family's unthrottled rate over the running phase that is ending and set the
    /// share of the next period it may run under the CPU cap.
    fn measure_rate(&mut self, family: &mut dyn Family) {
        if let (Some(target), Some((start_cpu, start)), Some(cpu)) =
            (self.cap, self.run_mark, family.cpu_time())
        {
            let running = start.elapsed().as_secs_f64();
            if running > 0.0 {
                let rate = cpu.saturating_sub(start_cpu).as_secs_f64() / running * 1000.0;
                // The family runs unthrottled in this phase, so the rate is its
                // natural usage and target / rate is the share that meets the cap.
                if rate > 1.0 {
                    self.rate = Some(rate);
                    self.duty = (target as f64 / rate).clamp(MIN_DUTY, 1.0);
                }
            }
        }
    }
}

impl Actuator for RootlessActuator {
    fn unavailable(&self, kind: ResourceKind) -> BTreeSet<YieldStep> {
        let mut steps = BTreeSet::from([YieldStep::LowerShare, YieldStep::Hint]);
        match kind {
            ResourceKind::Cpu => {}
            // A GPU compute share is a pause duty cycle; Guaranteed work is never
            // paused (FR-024).
            ResourceKind::GpuCompute if self.class == AllocationClass::Guaranteed => {
                steps.insert(YieldStep::Cap);
            }
            ResourceKind::GpuCompute => {}
            ResourceKind::Ram | ResourceKind::GpuMemory => {
                steps.extend([YieldStep::Cap, YieldStep::Pause]);
            }
        }
        // Guaranteed work is never paused (FR-024).
        if self.class == AllocationClass::Guaranteed {
            steps.insert(YieldStep::Pause);
        }
        if !cfg!(unix) {
            steps.extend([YieldStep::Cap, YieldStep::Pause]);
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
        if self.unavailable(kind).contains(&step) || step >= YieldStep::Drain {
            bail!("step {step:?} is not a runtime step for {kind:?} here");
        }
        match step {
            YieldStep::Pause => {
                self.paused.insert(kind);
            }
            YieldStep::Cap if kind == ResourceKind::GpuCompute => {
                self.paused.remove(&kind);
                self.gpu_duty = gpu_compute_duty(target);
            }
            YieldStep::Cap => {
                let target = target.unwrap_or(self.requested_cpu / 2);
                // The same cap on a cycle already running keeps its phase: an adjust must not
                // start the on phase again or continue the family inside an off phase (SUP-8).
                if self.cap == Some(target) && self.paused.is_empty() && !self.aligned() {
                    return Ok(step);
                }
                self.paused.remove(&kind);
                self.cap = Some(target);
                // Against the measured rate, a changed cap holds from its first period;
                // against the request, one above it would run uncapped for a period
                // (research R20).
                let rate = self.rate.unwrap_or(self.requested_cpu as f64);
                self.duty = (target as f64 / rate).clamp(MIN_DUTY, 1.0);
            }
            _ => {
                self.paused.remove(&kind);
                if kind == ResourceKind::Cpu {
                    self.cap = None;
                    self.duty = 1.0;
                }
                if kind == ResourceKind::GpuCompute {
                    self.gpu_duty = 1.0;
                }
            }
        }
        self.enforce(family)?;
        Ok(step)
    }

    fn tick(&mut self, family: &mut dyn Family) -> Result<()> {
        if !self.paused.is_empty() || (self.cap.is_none() && self.gpu_duty >= 1.0) {
            return Ok(());
        }
        if self.aligned() {
            let running = self
                .aligned
                .running((self.clock)(), self.period, self.duty());
            if running && self.stopped {
                self.resume(family)?;
            } else if !running && !self.stopped {
                self.measure_rate(family);
                self.stop(family)?;
            }
            return Ok(());
        }
        let elapsed = self.phase_started.elapsed();
        if !self.stopped {
            if elapsed < self.period.mul_f64(self.duty()) {
                return Ok(());
            }
            self.measure_rate(family);
            if self.duty() < 1.0 {
                self.stop(family)?;
            }
            self.phase_started = Instant::now();
        } else if elapsed >= self.period.mul_f64(1.0 - self.duty()) {
            self.resume(family)?;
            self.phase_started = Instant::now();
        }
        Ok(())
    }

    fn step_code(&self, kind: ResourceKind) -> Option<&'static str> {
        let at_minimum = match kind {
            ResourceKind::Cpu => self.cap.is_some() && self.duty <= MIN_DUTY,
            ResourceKind::GpuCompute => self.gpu_duty <= MIN_DUTY,
            ResourceKind::Ram | ResourceKind::GpuMemory => false,
        };
        at_minimum.then_some("cap_at_minimum_duty")
    }

    fn next_deadline(&self) -> Option<Instant> {
        if !self.paused.is_empty() || (self.cap.is_none() && self.gpu_duty >= 1.0) {
            return None;
        }
        if self.aligned() {
            let now = (self.clock)();
            let at = self
                .aligned
                .next_switch(now, self.period, self.duty(), !self.stopped);
            return Some(Instant::now() + at.saturating_sub(now));
        }
        let share = if self.stopped {
            1.0 - self.duty()
        } else {
            self.duty()
        };
        Some(self.phase_started + self.period.mul_f64(share))
    }

    fn release(&mut self, family: &mut dyn Family) -> Result<()> {
        self.paused.clear();
        self.cap = None;
        self.duty = 1.0;
        self.gpu_duty = 1.0;
        self.aligned = AlignedPhase::default();
        // The flag is set before any stop and covers a member stopped as it joined, so the
        // continue reaches every member the family holds (R2, FR-086).
        if self.stopped {
            family.signal(signal::CONT)?;
            self.stopped = false;
        }
        Ok(())
    }
}

/// Reads the cumulative CPU time of verified family members. Read-only: it
/// never signals or changes any process.
#[derive(Default)]
pub struct CpuMeter {
    system: Option<sysinfo::System>,
}

impl CpuMeter {
    pub fn read(&mut self, pids: &[u32]) -> Option<Duration> {
        if pids.is_empty() {
            return None;
        }
        let system = self.system.get_or_insert_with(sysinfo::System::new);
        let ids: Vec<_> = pids
            .iter()
            .map(|pid| sysinfo::Pid::from_u32(*pid))
            .collect();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&ids),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cpu(),
        );
        let mut total = 0u64;
        for id in &ids {
            total = total.saturating_add(system.process(*id)?.accumulated_cpu_time());
        }
        Some(Duration::from_millis(total))
    }
}

#[cfg(unix)]
mod signal {
    pub const STOP: i32 = libc::SIGSTOP;
    pub const CONT: i32 = libc::SIGCONT;
}
#[cfg(not(unix))]
mod signal {
    pub const STOP: i32 = 19;
    pub const CONT: i32 = 18;
}

/// The target file and ack names inside a process's private drain directory.
pub const TARGET_FILE: &str = "resource-target.json";
pub const ACK_FILE: &str = "resource-target.ack";
const TARGET_TEMPORARY: &str = ".resource-target.json.tmp";

/// Per-resource amounts in a target file; unset amounts carry no target.
pub type TargetAmounts = crate::model::ResourceAmounts;

/// A target file (contracts/resource-target-file.md). `sequence` counts target
/// and step changes; `allowance_sequence` counts allowance changes apart, so an
/// allowance-only change never reads as a new target.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceTarget {
    pub version: u32,
    pub sequence: u64,
    pub deadline_unix_ms: u64,
    pub step: YieldStep,
    pub targets: TargetAmounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowance: Option<TargetAmounts>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub allowance_sequence: u64,
    /// Rises once per save-progress request (FR-075).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub save_progress_sequence: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Write a target file atomically: a temporary file in the same directory,
/// fsync, then rename over the target.
pub fn write_target(directory: &std::path::Path, target: &ResourceTarget) -> Result<()> {
    use std::io::Write;
    let temporary = directory.join(TARGET_TEMPORARY);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(target)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, directory.join(TARGET_FILE))?;
    Ok(())
}

/// Copy the current target file, if any, into a new family member's directory, so
/// a managed child sees the current target when it starts (FR-035).
pub fn copy_current_target(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    let raw = match std::fs::read(from.join(TARGET_FILE)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    write_target(to, &serde_json::from_slice(&raw)?)
}

/// The highest sequence a worker acknowledged, if any. It only shows the worker
/// reads targets; compliance is judged from observed usage.
pub fn read_ack(directory: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(directory.join(ACK_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Remove the target files a channel may have left in a directory; only known
/// names, never other entries.
pub fn remove_target_files(directory: &std::path::Path) {
    for name in [TARGET_FILE, ACK_FILE, TARGET_TEMPORARY] {
        let _ = std::fs::remove_file(directory.join(name));
    }
}

/// The resource-target channel of one family: current hints per resource and
/// the file sequence, written to every member's private directory.
#[derive(Debug, Default)]
pub struct TargetChannel {
    sequence: u64,
    /// Each hint's target and the deadline the policy enforces for it.
    hints: std::collections::BTreeMap<crate::contention::ResourceKey, (u64, u64)>,
    deadline_ms: u64,
    /// The deadline last published, kept by allowance-only rewrites.
    deadline_unix_ms: u64,
    allowance: Option<TargetAmounts>,
    allowance_sequence: u64,
    save_progress_sequence: u64,
}

impl TargetChannel {
    pub fn new(deadline_ms: u64) -> Self {
        Self {
            deadline_ms,
            ..Self::default()
        }
    }

    /// Whether any member acknowledged that it reads targets.
    pub fn acknowledged(directories: &[std::path::PathBuf]) -> bool {
        directories.iter().any(|d| read_ack(d).is_some())
    }

    /// Set or clear one resource's hint with the default deadline; true when it changed.
    pub fn set(&mut self, key: crate::contention::ResourceKey, target: Option<u64>) -> bool {
        match target {
            Some(target) => self.set_hint(key, target, None),
            None => self.hints.remove(&key).is_some(),
        }
    }

    /// Set one resource's hint and the deadline the policy enforces (FR-037).
    pub fn set_hint(
        &mut self,
        key: crate::contention::ResourceKey,
        target: u64,
        deadline_ms: Option<u64>,
    ) -> bool {
        let hint = (target, deadline_ms.unwrap_or(self.deadline_ms));
        self.hints.insert(key, hint) != Some(hint)
    }

    /// Set the allowance (FR-071); true when it changed.
    pub fn set_allowance(&mut self, allowance: TargetAmounts) -> bool {
        let changed = self.allowance.as_ref() != Some(&allowance);
        self.allowance = Some(allowance);
        changed
    }

    /// Write the current allowance with the next allowance sequence; the target
    /// sequence, step and deadline stay as last published.
    pub fn publish_allowance(&mut self, directories: &[std::path::PathBuf]) -> Result<()> {
        self.allowance_sequence += 1;
        self.write(directories)
    }

    /// Ask the worker to save its progress: the next save-progress sequence, with the
    /// targets and allowance as last published (FR-075).
    pub fn publish_save_progress(&mut self, directories: &[std::path::PathBuf]) -> Result<()> {
        self.save_progress_sequence += 1;
        self.write(directories)
    }

    /// Write the current hints with the next sequence; no hints relax them.
    pub fn publish(&mut self, directories: &[std::path::PathBuf]) -> Result<()> {
        self.sequence += 1;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        // The earliest hint deadline is the one the next step follows.
        self.deadline_unix_ms = (now.as_millis() as u64).saturating_add(
            self.hints
                .values()
                .map(|(_, deadline)| *deadline)
                .min()
                .unwrap_or(self.deadline_ms),
        );
        self.write(directories)
    }

    fn write(&self, directories: &[std::path::PathBuf]) -> Result<()> {
        use crate::contention::ResourceKey;
        let mut targets = TargetAmounts::default();
        for (key, (amount, _)) in &self.hints {
            match key {
                ResourceKey::Cpu => targets.cpu_millicores = Some(*amount),
                ResourceKey::Ram => targets.ram_mib = Some(*amount),
                ResourceKey::GpuMemory(uuid) => {
                    targets.gpu_memory_mib.insert(uuid.clone(), *amount);
                }
                // The file has no compute target; the supervisor refuses those hints.
                ResourceKey::GpuCompute(_) => {}
            }
        }
        let target = ResourceTarget {
            version: 1,
            sequence: self.sequence,
            deadline_unix_ms: self.deadline_unix_ms,
            step: if self.hints.is_empty() {
                YieldStep::None
            } else {
                YieldStep::Hint
            },
            targets,
            allowance: self.allowance.clone(),
            allowance_sequence: self.allowance_sequence,
            save_progress_sequence: self.save_progress_sequence,
        };
        let mut result = Ok(());
        for directory in directories {
            if let Err(error) = write_target(directory, &target) {
                result = Err(error);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_model::ProcessIdentity;
    use crate::model::Resources;

    #[derive(Default)]
    struct Recorder {
        signals: Vec<i32>,
    }

    impl Family for Recorder {
        fn identities(&self) -> Vec<ProcessIdentity> {
            Vec::new()
        }
        fn signal(&mut self, signal: i32) -> Result<()> {
            self.signals.push(signal);
            Ok(())
        }
        fn cpu_time(&mut self) -> Option<Duration> {
            None
        }
    }

    fn actuator(class: AllocationClass) -> RootlessActuator {
        RootlessActuator::new(&ActuatorSettings {
            class,
            requested: Resources {
                cpu_millicores: 1000,
                ..Resources::default()
            },
            period: Duration::from_millis(100),
            ram_noise_tolerance_mib: 256,
            ram_calm_window: Duration::from_millis(1500),
        })
    }

    /// The next phase switch of a duty cycle is the supervisor's next deadline; without a
    /// duty cycle there is none (plan "Item 027").
    #[test]
    fn the_next_deadline_is_the_duty_cycles_phase_switch() {
        let mut family = Recorder::default();
        let mut cap = actuator(AllocationClass::Opportunistic);
        assert_eq!(cap.next_deadline(), None);
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        cap.duty = 0.25;
        let running = cap.next_deadline().unwrap();
        assert_eq!(running, cap.phase_started + Duration::from_millis(25));
        cap.stopped = true;
        assert_eq!(
            cap.next_deadline(),
            Some(cap.phase_started + Duration::from_millis(75))
        );
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(cap.next_deadline(), None);
    }

    /// An adjust that leaves the CPU cap where it is keeps the running cycle's phase: it neither
    /// starts the on phase again nor continues the family inside an off phase, so the cap holds
    /// through adjusts (SUP-8). A changed cap still takes effect at once.
    #[test]
    fn an_unchanged_cpu_cap_keeps_the_cycles_phase() {
        let mut family = Recorder::default();
        let mut cap = actuator(AllocationClass::Opportunistic);
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        cap.phase_started = Instant::now() - Duration::from_millis(10);
        let on = cap.phase_started;
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        assert_eq!(cap.phase_started, on, "the on phase is not started again");
        cap.phase_started = Instant::now() - Duration::from_millis(30);
        cap.tick(&mut family).unwrap();
        assert!(cap.stopped);
        let (off, signals) = (cap.phase_started, family.signals.len());
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        assert!(cap.stopped, "the off phase is not cut short");
        assert_eq!(family.signals.len(), signals);
        assert_eq!(cap.phase_started, off);
        cap.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(500))
            .unwrap();
        assert!(!cap.stopped);
        assert_eq!(family.signals.last(), Some(&signal::CONT));
    }

    /// A GPU compute cap is a pause duty cycle at the target's share of the device;
    /// with a CPU cap too, one cycle runs at the smaller duty (FR-021).
    #[test]
    fn gpu_compute_cap_shares_the_cpu_duty_cycle() {
        let mut family = Recorder::default();
        let mut actuator = actuator(AllocationClass::Opportunistic);
        assert!(
            !actuator
                .unavailable(ResourceKind::GpuCompute)
                .contains(&YieldStep::Cap)
                || !cfg!(unix)
        );
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(250),
            )
            .unwrap();
        assert_eq!(actuator.duty(), 0.25);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(750))
            .unwrap();
        assert_eq!(actuator.duty(), 0.25);
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(actuator.duty(), 0.75);
        // The minimum duty still applies.
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(0),
            )
            .unwrap();
        assert_eq!(actuator.duty(), MIN_DUTY);
    }

    /// A CPU or GPU compute cap at the minimum duty is reported as such, so Performance
    /// may pause it (FR-022); a cap above the minimum, or none, is not.
    #[test]
    fn a_cap_at_the_minimum_duty_reports_it() {
        let mut family = Recorder::default();
        let mut actuator = actuator(AllocationClass::Opportunistic);
        for kind in [ResourceKind::Cpu, ResourceKind::GpuCompute] {
            assert_eq!(actuator.step_code(kind), None, "{kind:?}");
        }
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(1))
            .unwrap();
        assert_eq!(actuator.duty(), MIN_DUTY);
        assert_eq!(
            actuator.step_code(ResourceKind::Cpu),
            Some("cap_at_minimum_duty")
        );
        assert_eq!(actuator.step_code(ResourceKind::GpuCompute), None);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(750))
            .unwrap();
        assert_eq!(actuator.step_code(ResourceKind::Cpu), None);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(0),
            )
            .unwrap();
        assert_eq!(
            actuator.step_code(ResourceKind::GpuCompute),
            Some("cap_at_minimum_duty")
        );
        assert_eq!(actuator.step_code(ResourceKind::Cpu), None);
    }

    /// Guaranteed work takes neither a GPU compute cap nor a pause (FR-024).
    #[test]
    fn guaranteed_gpu_compute_steps_are_unavailable() {
        let unavailable =
            actuator(AllocationClass::Guaranteed).unavailable(ResourceKind::GpuCompute);
        assert!(unavailable.contains(&YieldStep::Cap) && unavailable.contains(&YieldStep::Pause));
    }

    /// A family that runs a full core whenever it runs.
    struct Spinner(Instant);

    impl Family for Spinner {
        fn identities(&self) -> Vec<ProcessIdentity> {
            Vec::new()
        }
        fn signal(&mut self, _: i32) -> Result<()> {
            Ok(())
        }
        fn cpu_time(&mut self) -> Option<Duration> {
            Some(self.0.elapsed())
        }
    }

    /// A changed cap starts at its target over the family's measured natural rate,
    /// so a cap raised above the request does not run uncapped for a period and a
    /// task that makes up lost CPU does not burst to a full core (research R20).
    #[test]
    fn a_changed_cap_starts_at_its_target_over_the_measured_rate() {
        let settings = |cpu_millicores| ActuatorSettings {
            class: AllocationClass::Opportunistic,
            requested: Resources {
                cpu_millicores,
                ..Resources::default()
            },
            period: Duration::from_millis(100),
            ram_noise_tolerance_mib: 256,
            ram_calm_window: Duration::from_millis(1500),
        };
        let mut family = Spinner(Instant::now());
        let mut actuator = RootlessActuator::new(&settings(250));
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(100))
            .unwrap();
        // One running phase measures the family at about a full core.
        std::thread::sleep(Duration::from_millis(60));
        actuator.tick(&mut family).unwrap();
        assert!((actuator.duty() - 0.1).abs() < 0.03, "{}", actuator.duty());
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(300))
            .unwrap();
        assert!((actuator.duty() - 0.3).abs() < 0.05, "{}", actuator.duty());
        // Without a measured rate, a cap starts at its target over the request.
        let mut fresh = RootlessActuator::new(&settings(250));
        fresh
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(100))
            .unwrap();
        assert_eq!(fresh.duty(), 0.4);
    }

    /// Three members, the second of which refuses STOP; a family signal goes on past a
    /// failing member and returns its error, as the supervisor's family does.
    struct PartlyStoppable {
        stopped: [bool; 3],
    }
    impl Family for PartlyStoppable {
        fn identities(&self) -> Vec<ProcessIdentity> {
            Vec::new()
        }
        fn signal(&mut self, signal: i32) -> Result<()> {
            let mut failed = false;
            for (member, stopped) in self.stopped.iter_mut().enumerate() {
                match signal {
                    signal::STOP if member == 1 => failed = true,
                    signal::STOP => *stopped = true,
                    signal::CONT => *stopped = false,
                    _ => {}
                }
            }
            if failed {
                bail!("member 1 refused STOP");
            }
            Ok(())
        }
        fn cpu_time(&mut self) -> Option<Duration> {
            None
        }
    }

    /// A stop that reached only part of the family still counts as a stop: release continues
    /// every member, so none stays stopped (030, SUP-5 and R2).
    #[test]
    fn a_partial_stop_is_continued_on_release() {
        let mut family = PartlyStoppable {
            stopped: [false; 3],
        };
        let mut actuator = actuator(AllocationClass::Opportunistic);
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
                .is_err()
        );
        assert_eq!(
            family.stopped,
            [true, false, true],
            "the failing member is passed over"
        );
        actuator.release(&mut family).unwrap();
        assert_eq!(family.stopped, [false; 3], "every member is continued");
    }

    thread_local! {
        /// The node clock as these tests set it.
        static NODE_CLOCK: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    }

    fn test_clock() -> Duration {
        NODE_CLOCK.with(|clock| clock.get())
    }

    fn set_clock(millis: u64) {
        NODE_CLOCK.with(|clock| clock.set(Duration::from_millis(millis)));
    }

    /// An actuator on the tests' node clock, with a 100 ms period.
    fn clocked() -> RootlessActuator {
        let mut actuator = actuator(AllocationClass::Opportunistic);
        actuator.clock = test_clock;
        actuator
    }

    /// The aligned phase runs while the node clock's position in the period is below the duty,
    /// and a stop holds until that period ends (FR-021, Q31).
    #[test]
    fn the_aligned_phase_runs_below_the_duty_and_holds_a_stop_to_the_period_end() {
        let period = Duration::from_millis(100);
        let at = Duration::from_millis;
        let mut phase = AlignedPhase::default();
        assert!(phase.running(at(10_010), period, 0.75));
        assert_eq!(
            phase.next_switch(at(10_010), period, 0.75, true),
            at(10_075)
        );
        assert!(!phase.running(at(10_080), period, 0.75));
        assert_eq!(
            phase.next_switch(at(10_080), period, 0.75, false),
            at(10_100)
        );
        // A higher duty in the off-phase waits for the next period.
        assert!(!phase.running(at(10_085), period, 0.9));
        assert!(phase.running(at(10_101), period, 0.9));
        // A lower duty ends the on-phase sooner.
        assert!(phase.running(at(10_140), period, 0.5));
        assert!(!phase.running(at(10_160), period, 0.5));
        assert!(!phase.running(at(10_170), period, 0.75));
        // At full duty the cycle always runs.
        assert!(phase.running(at(10_299), period, 1.0));
    }

    /// Two allocations whose GPU compute caps began 30 ms apart stop and resume together on the
    /// node clock (FR-021, Q31).
    #[test]
    fn gpu_duty_cycles_capped_apart_stop_and_resume_together() {
        let (mut a, mut b) = (Recorder::default(), Recorder::default());
        let (mut first, mut second) = (clocked(), clocked());
        set_clock(50_007);
        first
            .apply(&mut a, ResourceKind::GpuCompute, YieldStep::Cap, Some(750))
            .unwrap();
        set_clock(50_037);
        second
            .apply(&mut b, ResourceKind::GpuCompute, YieldStep::Cap, Some(750))
            .unwrap();
        let mut off = 0;
        for millis in 50_037..50_537 {
            set_clock(millis);
            first.tick(&mut a).unwrap();
            second.tick(&mut b).unwrap();
            assert_eq!(first.stopped, second.stopped, "at {millis} ms");
            off += usize::from(first.stopped);
        }
        assert!(off.abs_diff(125) <= 5, "stopped {off} of 500 ms");
    }

    /// A duty change mid-period shortens or lengthens the on-phase, never resumes inside an
    /// off-phase and never skips one (FR-021, Q31).
    #[test]
    fn a_gpu_duty_change_mid_period_never_resumes_inside_an_off_phase() {
        let mut family = Recorder::default();
        let mut actuator = clocked();
        set_clock(1_000);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        set_clock(1_060);
        actuator.tick(&mut family).unwrap();
        assert!(actuator.stopped);
        set_clock(1_070);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(900),
            )
            .unwrap();
        actuator.tick(&mut family).unwrap();
        assert!(actuator.stopped, "no resume inside the off-phase");
        set_clock(1_105);
        actuator.tick(&mut family).unwrap();
        assert!(!actuator.stopped, "the next period runs");
        set_clock(1_140);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(300),
            )
            .unwrap();
        actuator.tick(&mut family).unwrap();
        assert!(actuator.stopped, "a lower duty ends the on-phase");
    }

    /// On the aligned phase the next deadline is the next switch on the node clock: the on-phase's
    /// end while running, the period's end while stopped (FR-021, Q31).
    #[test]
    fn the_aligned_deadline_is_the_next_switch_on_the_node_clock() {
        let mut family = Recorder::default();
        let mut actuator = clocked();
        set_clock(2_010);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(750),
            )
            .unwrap();
        let ahead = |actuator: &RootlessActuator| {
            actuator
                .next_deadline()
                .unwrap()
                .saturating_duration_since(Instant::now())
                .as_millis()
        };
        assert!(
            (60..=65).contains(&ahead(&actuator)),
            "{}",
            ahead(&actuator)
        );
        set_clock(2_080);
        actuator.tick(&mut family).unwrap();
        assert!(actuator.stopped);
        assert!(
            (15..=20).contains(&ahead(&actuator)),
            "{}",
            ahead(&actuator)
        );
    }

    /// A CPU-only duty cycle keeps its per-allocation phase: it starts running when the cap is
    /// applied, wherever the node clock stands (FR-021, Q31).
    #[test]
    fn a_cpu_only_duty_cycle_keeps_its_own_phase() {
        let mut family = Recorder::default();
        let mut actuator = clocked();
        set_clock(3_090);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        actuator.tick(&mut family).unwrap();
        assert!(!actuator.stopped, "the CPU cycle starts in its on-phase");
        assert_eq!(
            actuator.next_deadline(),
            Some(actuator.phase_started + Duration::from_millis(25))
        );
    }
}
