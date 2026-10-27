//! Supervisor-side actuation of yield steps on verified own processes only.
use crate::contention::{ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass, Family};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// The smallest share of each period a capped family may run.
const MIN_DUTY: f64 = 0.05;

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
    phase_started: Instant,
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
            run_mark: None,
            rate: None,
        }
    }

    fn stop(&mut self, family: &mut dyn Family) -> Result<()> {
        if !self.stopped {
            family.signal(signal::STOP)?;
            self.stopped = true;
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

    /// The share of each period the family runs under the applied caps.
    pub fn duty(&self) -> f64 {
        let cpu = if self.cap.is_some() { self.duty } else { 1.0 };
        cpu.min(self.gpu_duty)
    }

    /// Bring the family to the state the applied steps require.
    fn enforce(&mut self, family: &mut dyn Family) -> Result<()> {
        if !self.paused.is_empty() {
            self.stop(family)
        } else {
            self.phase_started = Instant::now();
            self.resume(family)
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
                let target = target.unwrap_or(500);
                self.gpu_duty = (target as f64 / 1000.0).clamp(MIN_DUTY, 1.0);
            }
            YieldStep::Cap => {
                self.paused.remove(&kind);
                let target = target.unwrap_or(self.requested_cpu / 2);
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
        let elapsed = self.phase_started.elapsed();
        if !self.stopped {
            if elapsed < self.period.mul_f64(self.duty()) {
                return Ok(());
            }
            // Measure the family's unthrottled rate over this running phase and
            // set the share of the next period it may run.
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

    fn next_deadline(&self) -> Option<Instant> {
        if !self.paused.is_empty() || (self.cap.is_none() && self.gpu_duty >= 1.0) {
            return None;
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
}
