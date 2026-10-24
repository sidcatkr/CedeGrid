//! Supervisor-side actuation of yield steps on verified own processes only.
use crate::contention::{ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass, Family};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// The smallest share of each period a capped family may run.
const MIN_DUTY: f64 = 0.05;

/// Rootless mechanisms (research R4): a SIGSTOP/SIGCONT duty cycle for a CPU cap
/// and SIGSTOP for a pause, both through owned handles. Lower share and memory
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
    duty: f64,
    phase_started: Instant,
    /// CPU time and instant at the start of the current running phase.
    run_mark: Option<(Duration, Instant)>,
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
            phase_started: Instant::now(),
            run_mark: None,
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
            ResourceKind::GpuCompute => {
                steps.insert(YieldStep::Cap);
            }
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
            YieldStep::Cap => {
                self.paused.remove(&kind);
                let target = target.unwrap_or(self.requested_cpu / 2);
                self.cap = Some(target);
                self.duty = (target as f64 / self.requested_cpu as f64).clamp(MIN_DUTY, 1.0);
            }
            _ => {
                self.paused.remove(&kind);
                if kind == ResourceKind::Cpu {
                    self.cap = None;
                    self.duty = 1.0;
                }
            }
        }
        self.enforce(family)?;
        Ok(step)
    }

    fn tick(&mut self, family: &mut dyn Family) -> Result<()> {
        let Some(target) = self.cap.filter(|_| self.paused.is_empty()) else {
            return Ok(());
        };
        let elapsed = self.phase_started.elapsed();
        if !self.stopped {
            if elapsed < self.period.mul_f64(self.duty) {
                return Ok(());
            }
            // Measure the family's unthrottled rate over this running phase and
            // set the share of the next period it may run.
            if let (Some((start_cpu, start)), Some(cpu)) = (self.run_mark, family.cpu_time()) {
                let running = start.elapsed().as_secs_f64();
                if running > 0.0 {
                    let rate = cpu.saturating_sub(start_cpu).as_secs_f64() / running * 1000.0;
                    // The family runs unthrottled in this phase, so the rate is its
                    // natural usage and target / rate is the share that meets the cap.
                    if rate > 1.0 {
                        self.duty = (target as f64 / rate).clamp(MIN_DUTY, 1.0);
                    }
                }
            }
            if self.duty < 1.0 {
                self.stop(family)?;
            }
            self.phase_started = Instant::now();
        } else if elapsed >= self.period.mul_f64(1.0 - self.duty) {
            self.resume(family)?;
            self.phase_started = Instant::now();
        }
        Ok(())
    }

    fn release(&mut self, family: &mut dyn Family) -> Result<()> {
        self.paused.clear();
        self.cap = None;
        self.duty = 1.0;
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
