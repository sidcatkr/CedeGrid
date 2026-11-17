//! Supervisor-side actuation of yield steps on verified own processes only.
use crate::contention::{ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass, Family};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::time::Duration;

/// Rootless mechanisms (research R4): SIGSTOP and SIGCONT through owned handles for a pause. No
/// rate cap runs here: CPU in Tracked and GPU compute at both levels are held back by whole tasks
/// (FR-089). Lower share and memory caps have no rootless runtime mechanism; hints need resource
/// targets (US4).
pub struct RootlessActuator {
    class: AllocationClass,
    /// Resources that asked for a pause.
    paused: BTreeSet<ResourceKind>,
    stopped: bool,
}

impl RootlessActuator {
    pub fn new(settings: &ActuatorSettings) -> Self {
        Self {
            class: settings.class,
            paused: BTreeSet::new(),
            stopped: false,
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
        Ok(())
    }
}

impl Actuator for RootlessActuator {
    fn unavailable(&self, kind: ResourceKind) -> BTreeSet<YieldStep> {
        let mut steps = BTreeSet::from([YieldStep::LowerShare, YieldStep::Cap, YieldStep::Hint]);
        // Memory is never paused, and Guaranteed work never is (FR-024).
        if kind.is_memory() || self.class == AllocationClass::Guaranteed || !cfg!(unix) {
            steps.insert(YieldStep::Pause);
        }
        steps
    }

    fn apply(
        &mut self,
        family: &mut dyn Family,
        kind: ResourceKind,
        step: YieldStep,
        _target: Option<u64>,
    ) -> Result<YieldStep> {
        if self.unavailable(kind).contains(&step) || step >= YieldStep::Drain {
            bail!("step {step:?} is not a runtime step for {kind:?} here");
        }
        if step == YieldStep::Pause {
            self.paused.insert(kind);
        } else {
            self.paused.remove(&kind);
        }
        if self.paused.is_empty() {
            self.resume(family)?;
        } else {
            self.stop(family)?;
        }
        Ok(step)
    }

    fn tick(&mut self, _family: &mut dyn Family) -> Result<()> {
        Ok(())
    }

    fn release(&mut self, family: &mut dyn Family) -> Result<()> {
        self.paused.clear();
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

    /// Guaranteed work takes neither a GPU compute cap nor a pause (FR-024).
    #[test]
    fn guaranteed_gpu_compute_steps_are_unavailable() {
        let unavailable =
            actuator(AllocationClass::Guaranteed).unavailable(ResourceKind::GpuCompute);
        assert!(unavailable.contains(&YieldStep::Cap) && unavailable.contains(&YieldStep::Pause));
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
    /// every member, so none stays stopped (R2).
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

    /// No rate cap runs rootless: CPU and GPU compute caps are unavailable, a cap is refused, a
    /// pause stops the family and its end continues it, and nothing is time-driven (FR-089).
    #[test]
    fn no_rate_cap_runs_rootless() {
        let mut actuator = actuator(AllocationClass::Opportunistic);
        for kind in [ResourceKind::Cpu, ResourceKind::GpuCompute] {
            assert!(
                actuator.unavailable(kind).contains(&YieldStep::Cap),
                "{kind:?}"
            );
            assert!(
                !actuator.unavailable(kind).contains(&YieldStep::Pause),
                "{kind:?}"
            );
        }
        let mut family = Recorder::default();
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
                .is_err()
        );
        assert!(family.signals.is_empty());
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Pause,
                None,
            )
            .unwrap();
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(family.signals, [signal::STOP, signal::CONT]);
        assert_eq!(actuator.next_deadline(), None);
    }
}
