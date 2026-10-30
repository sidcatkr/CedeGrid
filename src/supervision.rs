//! Local process supervision with a fail-closed, persisted launch barrier.
//!
//! The supervisor owns and reaps its direct child on one thread. Callers must not
//! install another child reaper or change SIGCHLD while it runs. A leader handle
//! is not descendant containment. Rootless execution currently requires the
//! explicit single-process contract; a delegated backend may contain descendants.
//! Loss of this supervisor is NOT self-healing: persisted capacity stays charged
//! until a separate reconciler proves release. No numeric PID is reattached here.
use crate::{execution_model::*, model::Resources};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub nice: i32,
    pub drain_timeout_ms: u64,
    pub term_grace_ms: u64,
    pub lease_ms: u64,
    pub prepare_timeout_ms: u64,
    pub release_confirm_timeout_ms: u64,
    /// Duty-cycle period of a rootless CPU cap: the monitor interval.
    pub duty_period_ms: u64,
    /// How long a worker has to comply with a resource target (FR-037).
    pub hint_deadline_ms: u64,
    /// Every own process a preferred OOM victim from launch (FR-023).
    pub prefer_own_oom_victim: bool,
    /// RAM cap: the noise tolerance and the ineffective-cap calm window.
    pub ram_noise_tolerance_mib: u64,
    pub ram_calm_window_ms: u64,
    /// The soft descriptor limit the user's command gets back (FR-077).
    pub nofile_soft: Option<u64>,
}
impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            nice: 10,
            drain_timeout_ms: 3_000,
            term_grace_ms: 2_000,
            lease_ms: 10_000,
            prepare_timeout_ms: 10_000,
            release_confirm_timeout_ms: 10_000,
            prefer_own_oom_victim: false,
            duty_period_ms: 500,
            hint_deadline_ms: 5_000,
            ram_noise_tolerance_mib: 256,
            ram_calm_window_ms: 1_500,
            nofile_soft: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub record: ExecutionRecord,
    pub exit_code: Option<i32>,
    pub termination_signal: Option<i32>,
    pub yielded: bool,
    pub process_handle: String,
    /// From launch authorization; these are observations, not release guarantees.
    pub drain_started_ms: Option<u64>,
    /// When the first SIGTERM was actually sent.
    pub termination_started_ms: Option<u64>,
    /// The deadline that SIGTERM was due at: drain request plus drain timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_deadline_ms: Option<u64>,
    pub process_exit_ms: u64,
    pub release_confirmed_ms: Option<u64>,
    /// The leaf's `memory.events` `oom_kill` count at release, in Full (FR-076).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_oom_kills: Option<u64>,
    /// Output bytes not kept, past the retention cap or after a capture error (030, SUP-2).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub log_bytes_dropped: u64,
    /// Why output capture stopped keeping bytes, if it did; the run's result is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_capture_error: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    Running,
    Draining,
    Terminating,
    KillRequested,
    NeedsReconciliation,
    Released,
}

/// Transport-independent monotonic deadline reducer. Renewals must already have
/// been authenticated as coordinator grants by the caller; agent heartbeats do
/// not renew authority. This local milestone provides no remote renewal channel.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    class: AllocationClass,
    assignment_id: String,
    generation: u64,
    last_sequence: u64,
    last_grant_ms: u64,
    lease_deadline_ms: u64,
    lease_ms: u64,
    drain_ms: u64,
    term_ms: u64,
    drain_at_ms: Option<u64>,
    /// TERM without the drain-file wait, on the critical floor path (FR-063).
    terminate: bool,
    pub phase: LifecyclePhase,
}
impl Lifecycle {
    pub fn new(
        class: AllocationClass,
        assignment_id: String,
        generation: u64,
        now_ms: u64,
        options: &SupervisorOptions,
    ) -> Self {
        Self {
            class,
            assignment_id,
            generation,
            last_sequence: 0,
            last_grant_ms: now_ms,
            lease_deadline_ms: now_ms.saturating_add(options.lease_ms),
            lease_ms: options.lease_ms,
            drain_ms: options.drain_timeout_ms,
            term_ms: options.term_grace_ms,
            drain_at_ms: None,
            terminate: false,
            phase: LifecyclePhase::Running,
        }
    }
    pub fn coordinator_renewal(
        &mut self,
        assignment: &str,
        generation: u64,
        sequence: u64,
        now_ms: u64,
    ) -> Result<()> {
        self.tick(now_ms);
        ensure!(
            self.phase == LifecyclePhase::Running,
            "allocation no longer accepts grants"
        );
        ensure!(
            assignment == self.assignment_id && generation == self.generation,
            "stale assignment or generation"
        );
        ensure!(
            sequence > self.last_sequence && now_ms >= self.last_grant_ms,
            "replayed grant or non-monotonic time"
        );
        self.last_sequence = sequence;
        self.last_grant_ms = now_ms;
        self.lease_deadline_ms = now_ms.saturating_add(self.lease_ms);
        Ok(())
    }
    pub fn cap_lease_remaining(&mut self, now_ms: u64, remaining_ms: u64) {
        self.lease_deadline_ms = self
            .lease_deadline_ms
            .min(now_ms.saturating_add(remaining_ms));
    }
    pub fn coordinator_disconnected(&mut self) { /* existing grant keeps its original expiry */
    }
    pub fn agent_heartbeat(&mut self) { /* deliberately does not extend a coordinator lease */
    }
    /// Agent loss holds the applied steps; the existing lease rules then apply
    /// (FR-015, supervisor-control contract).
    pub fn agent_failed(&mut self, _now_ms: u64) {}
    pub fn lease_sequence(&self) -> u64 {
        self.last_sequence
    }
    /// A separate observer may record this. A dead supervisor cannot run tick().
    pub fn supervisor_failed(&mut self) {
        self.phase = LifecyclePhase::NeedsReconciliation;
    }
    pub fn request_drain(&mut self, now_ms: u64) {
        if self.phase == LifecyclePhase::Running {
            self.drain_at_ms = Some(now_ms);
            self.phase = LifecyclePhase::Draining;
        }
    }
    /// Skip the drain-file wait and send TERM now, then KILL after the grace.
    pub fn request_terminate(&mut self, now_ms: u64) {
        if self.phase == LifecyclePhase::Running {
            self.terminate = true;
            self.request_drain(now_ms);
            self.tick(now_ms);
        }
    }
    fn drain_wait_ms(&self) -> u64 {
        if self.terminate { 0 } else { self.drain_ms }
    }
    pub fn tick(&mut self, now_ms: u64) -> LifecyclePhase {
        if matches!(
            self.phase,
            LifecyclePhase::NeedsReconciliation | LifecyclePhase::Released
        ) {
            return self.phase;
        }
        if self.phase == LifecyclePhase::Running
            && self.class == AllocationClass::Opportunistic
            && now_ms >= self.lease_deadline_ms
        {
            // Expiry is a deadline, not the (potentially late) observation time.
            self.request_drain(self.lease_deadline_ms);
        }
        if let Some(start) = self.drain_at_ms {
            let elapsed = now_ms.saturating_sub(start);
            if elapsed >= self.drain_wait_ms().saturating_add(self.term_ms) {
                self.phase = LifecyclePhase::KillRequested;
            } else if elapsed >= self.drain_wait_ms() {
                self.phase = LifecyclePhase::Terminating;
            }
        }
        self.phase
    }
    pub fn confirm_release(&mut self) {
        self.phase = LifecyclePhase::Released;
    }
    /// When drain was requested, as the deadline anchor.
    pub fn drain_requested_ms(&self) -> Option<u64> {
        self.drain_at_ms
    }
    /// The next deadline at which `tick` changes the phase: the lease expiry while
    /// opportunistic work runs, then TERM and KILL after a drain.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        match (self.phase, self.drain_at_ms) {
            (LifecyclePhase::Running, None) => {
                (self.class == AllocationClass::Opportunistic).then_some(self.lease_deadline_ms)
            }
            (LifecyclePhase::Running | LifecyclePhase::Draining, Some(start)) => {
                Some(start.saturating_add(self.drain_wait_ms()))
            }
            (LifecyclePhase::Terminating, Some(start)) => Some(
                start
                    .saturating_add(self.drain_wait_ms())
                    .saturating_add(self.term_ms),
            ),
            _ => None,
        }
    }
    /// When SIGTERM is due: the drain request plus the drain timeout.
    pub fn termination_deadline_ms(&self) -> Option<u64> {
        self.drain_at_ms
            .map(|start| start.saturating_add(self.drain_wait_ms()))
    }
}

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateRequest {
    request: LaunchRequest,
    generation: u64,
    setup: GateSetup,
    drain_file: std::path::PathBuf,
    /// Exposed as CEDEGRID_RESOURCE_TARGET_FILE, next to the drain file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_target_file: Option<std::path::PathBuf>,
}

/// Turn SIGTERM, SIGINT and SIGHUP to this supervisor process into a release of its run's
/// steps and a drain (FR-078). Only the supervisor's own process installs it.
pub fn install_stop_handlers() -> Result<()> {
    #[cfg(unix)]
    {
        unix::install_stop_handlers()
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// Entry point for the exact hidden `__worker-gate` command, before Tokio or
/// other threads initialize. The protocol is private; user stdout is /dev/null,
/// stderr is inherited, and user stdin is /dev/null. EOF before EXEC exits.
pub fn worker_gate() -> Result<()> {
    #[cfg(unix)]
    {
        unix::worker_gate()
    }
    #[cfg(not(unix))]
    {
        bail!("verified launch gate is unsupported on this platform")
    }
}

/// A supervisor-owned control source. Implementations must authenticate grants and
/// conservatively account for transport elapsed time before returning Renew.
pub trait SupervisorControl {
    fn prepared(&mut self, _record: &ExecutionRecord) -> Result<()> {
        Ok(())
    }
    fn initial_remaining_ms(&self) -> Option<u64> {
        None
    }
    /// Recheck each new mediated child before reservation and immediately before
    /// EXEC, and return queued authenticated commands without sampling telemetry
    /// after the final launch check. Local controls have no live GPU admission
    /// authority; GPU children require independent current-telemetry validation.
    fn authorize_managed_child(
        &mut self,
        request: &LaunchRequest,
    ) -> Result<Vec<SupervisorCommand>> {
        ensure!(
            request.resources.gpu_memory_mib.is_empty(),
            "managed GPU child requires a live GPU launch authorization check"
        );
        self.poll()
    }
    fn poll(&mut self) -> Result<Vec<SupervisorCommand>> {
        Ok(Vec::new())
    }
    /// Deliver an `adjusted` or `state` report to the policy authority.
    fn report(&mut self, _report: SupervisorReport) -> Result<()> {
        Ok(())
    }
    /// A descriptor that turns readable when a control line arrives, so the supervisor
    /// can wait for its next deadline; without one it polls every pass.
    #[cfg(unix)]
    fn wake(&self) -> Option<&Wake> {
        None
    }
}

/// Wakes a supervisor that waits for its next deadline when a control line arrives.
#[cfg(unix)]
#[derive(Clone)]
pub struct Wake(
    std::sync::Arc<(
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    )>,
);

#[cfg(unix)]
impl Wake {
    pub fn new() -> Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(Self(std::sync::Arc::new((reader, writer))))
    }
    /// A full socket buffer already wakes the reader, so a refused write loses nothing.
    pub fn notify(&self) {
        use std::io::Write;
        let _ = (&self.0.1).write(&[1]);
    }
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.0.0.as_raw_fd()
    }
    pub fn drain(&self) {
        use std::io::Read;
        let mut buffer = [0u8; 64];
        while matches!((&self.0.0).read(&mut buffer), Ok(read) if read > 0) {}
    }
}
/// Reports from a supervisor about the steps it applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "report", rename_all = "snake_case")]
pub enum SupervisorReport {
    Adjusted {
        sequence: u64,
        applied: std::collections::BTreeMap<
            crate::contention::ResourceKey,
            crate::contention::YieldStep,
        >,
        unavailable: std::collections::BTreeMap<
            crate::contention::ResourceKey,
            Vec<crate::contention::YieldStep>,
        >,
        detail: String,
        /// Readiness as of this report (FR-062).
        #[serde(default)]
        hint_ready: bool,
        #[serde(default)]
        observes_drain: bool,
        /// The cap or hint target applied per resource, so the policy credits
        /// own work at what runs, not at what it asked for.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        applied_targets: std::collections::BTreeMap<crate::contention::ResourceKey, u64>,
        /// Catalog codes for steps that hold above their target or became
        /// unavailable, such as a RAM cap that only stops growth.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        step_codes: std::collections::BTreeMap<crate::contention::ResourceKey, String>,
    },
    State {
        applied: std::collections::BTreeMap<
            crate::contention::ResourceKey,
            crate::contention::YieldStep,
        >,
        targets: std::collections::BTreeMap<crate::contention::ResourceKey, u64>,
        lease_sequence: u64,
        /// The last applied `adjust` sequence.
        sequence: u64,
        #[serde(default)]
        hint_ready: bool,
        #[serde(default)]
        observes_drain: bool,
        /// Each applied step's cause, so a reattached agent reseeds it unchanged.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        causes: std::collections::BTreeMap<
            crate::contention::ResourceKey,
            crate::contention::YieldCause,
        >,
    },
    /// Readiness changed: the worker acked its target file or used the
    /// supervisor socket (FR-062).
    Readiness {
        hint_ready: bool,
        observes_drain: bool,
    },
    /// The family's OOM preference changed (FR-023): each member's value before and
    /// after, and why a member was skipped.
    Oom {
        preferred: bool,
        changes: Vec<OomChange>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
    },
}

/// One family member's `oom_score_adj` change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OomChange {
    pub pid: u32,
    pub from: i32,
    pub to: i32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SupervisorCommand {
    Renew {
        assignment_id: String,
        generation: u64,
        sequence: u64,
        remaining_ms: u64,
    },
    Drain,
    AgentLost,
    /// Apply one step per resource; older sequences are ignored.
    Adjust {
        sequence: u64,
        resources: std::collections::BTreeMap<
            crate::contention::ResourceKey,
            crate::contention::StepRequest,
        >,
    },
    /// Report the currently applied steps.
    Query,
    /// Publish what own work may grow to now (FR-071).
    Allowance {
        allowance: crate::model::ResourceAmounts,
    },
    /// Make the family's processes preferred OOM victims, or return them to their launch
    /// baseline (FR-023).
    Oom {
        preferred: bool,
    },
    /// Ask the worker to save its progress now (FR-075).
    SaveProgress,
}
#[cfg(unix)]
struct LocalControl;
#[cfg(unix)]
impl SupervisorControl for LocalControl {}

pub fn supervise(
    request: &LaunchRequest,
    capacity: &Resources,
    options: &SupervisorOptions,
    journal: &dyn ExecutionJournal,
    backend: &mut dyn LaunchBackend,
    executable: &Path,
) -> Result<ExecutionOutcome> {
    #[cfg(unix)]
    {
        unix::supervise(
            request,
            capacity,
            options,
            journal,
            backend,
            executable,
            &mut LocalControl,
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (request, capacity, options, journal, backend, executable);
        bail!("local supervision is unavailable on this platform; observation remains supported")
    }
}

/// Independent services use this entry point; existing local callers retain their API.
#[allow(clippy::too_many_arguments)]
pub fn supervise_controlled(
    request: &LaunchRequest,
    capacity: &Resources,
    options: &SupervisorOptions,
    journal: &dyn ExecutionJournal,
    backend: &mut dyn LaunchBackend,
    executable: &Path,
    control: &mut dyn SupervisorControl,
) -> Result<ExecutionOutcome> {
    #[cfg(unix)]
    {
        unix::supervise(
            request, capacity, options, journal, backend, executable, control,
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (
            request, capacity, options, journal, backend, executable, control,
        );
        bail!("supervision unsupported")
    }
}
/// Read-only identity reconciliation. Never authorizes numeric-PID signaling.
/// The OOM preference own processes take: the most likely victim.
pub(crate) const OOM_PREFERRED: i32 = 1000;

/// Read back an owned gate's OOM preference and record it as evidence.
pub(crate) fn verify_oom_score_adj(
    pid: u32,
    requested: i32,
    scope: &str,
) -> Result<ControlEvidence> {
    let actual: i32 = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
        .context("cannot read the gate oom_score_adj")?
        .trim()
        .parse()?;
    ensure!(
        actual == requested,
        "gate oom_score_adj setting was not verified"
    );
    Ok(ControlEvidence {
        control: "memory.oom_score_adj".into(),
        available: Some(true),
        permitted: Some(true),
        configured: true,
        applied: true,
        fallback: false,
        scope: scope.into(),
        requested: Some(requested.to_string()),
        effective: Some(actual.to_string()),
        detail:
            "Verified own OOM preference; own processes are the preferred out-of-memory victims"
                .into(),
    })
}

pub fn process_identity(pid: u32, assignment: &str, generation: u64) -> Result<ProcessIdentity> {
    #[cfg(unix)]
    {
        unix::identity(pid, assignment, generation)
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, assignment, generation);
        bail!("native process identity unavailable")
    }
}

/// Apply only the calling process/thread's placement. No host cpuset hierarchy
/// or other process is modified. A requested unsupported control is an error.
pub fn apply_current_cpu_affinity(cpus: &[u32]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let allowed = read_cpu_affinity(0)?;
        ensure!(
            !cpus.is_empty() && cpus.iter().all(|id| allowed.contains(id)),
            "CPU affinity is empty or outside the caller's permitted set"
        );
        ensure!(
            cpus.iter().collect::<std::collections::BTreeSet<_>>().len() == cpus.len(),
            "CPU affinity contains duplicate IDs"
        );
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for id in cpus {
            ensure!(
                (*id as usize) < libc::CPU_SETSIZE as usize,
                "CPU affinity exceeds supported native mask"
            );
            unsafe { libc::CPU_SET(*id as usize, &mut set) };
        }
        ensure!(
            unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) } == 0,
            "CPU affinity application failed: {}",
            std::io::Error::last_os_error()
        );
        let mut expected = cpus.to_vec();
        expected.sort_unstable();
        ensure!(
            read_cpu_affinity(0)? == expected,
            "CPU affinity exact readback failed"
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cpus;
        bail!("exact CPU affinity control is unsupported on this platform")
    }
}
#[cfg(target_os = "linux")]
fn read_cpu_affinity(pid: u32) -> Result<Vec<u32>> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe {
            libc::sched_getaffinity(pid as libc::pid_t, std::mem::size_of_val(&set), &mut set)
        } == 0,
        "CPU affinity read failed: {}",
        std::io::Error::last_os_error()
    );
    Ok((0..libc::CPU_SETSIZE)
        .filter(|id| unsafe { libc::CPU_ISSET(*id as usize, &set) })
        .map(|id| id as u32)
        .collect())
}

#[cfg(unix)]
mod unix;

#[cfg(test)]
mod tests {
    use super::*;

    /// Setting affinity acts on the calling thread only: another thread's mask is
    /// unchanged (constitution I, calls that take only a thread ID).
    #[cfg(target_os = "linux")]
    #[test]
    fn current_cpu_affinity_leaves_other_threads_unchanged() {
        let allowed = read_cpu_affinity(0).unwrap();
        if allowed.len() < 2 {
            return;
        }
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            tid_tx
                .send(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
                .unwrap();
            done_rx.recv().unwrap();
        });
        let tid = tid_rx.recv().unwrap();
        let before = read_cpu_affinity(tid).unwrap();
        let caller = std::thread::spawn(move || {
            apply_current_cpu_affinity(&allowed[..1]).unwrap();
            read_cpu_affinity(0).unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(caller.len(), 1);
        assert_eq!(read_cpu_affinity(tid).unwrap(), before);
        done_tx.send(()).unwrap();
        other.join().unwrap();
    }

    fn lifecycle(class: AllocationClass) -> Lifecycle {
        Lifecycle::new(
            class,
            "a".into(),
            3,
            0,
            &SupervisorOptions {
                lease_ms: 100,
                drain_timeout_ms: 30,
                term_grace_ms: 20,
                ..Default::default()
            },
        )
    }
    #[test]
    fn deadlines_are_distinct_and_agent_heartbeat_is_not_authority() {
        let mut state = lifecycle(AllocationClass::Opportunistic);
        state.coordinator_disconnected();
        for _ in 0..10 {
            state.agent_heartbeat();
        }
        assert_eq!(state.tick(99), LifecyclePhase::Running);
        assert_eq!(state.tick(100), LifecyclePhase::Draining);
        assert_eq!(state.tick(129), LifecyclePhase::Draining);
        assert_eq!(state.tick(130), LifecyclePhase::Terminating);
        assert_eq!(state.tick(150), LifecyclePhase::KillRequested);
        assert_ne!(state.tick(100_000), LifecyclePhase::Released);
    }
    /// The critical floor path skips the drain-file wait (FR-063).
    #[test]
    fn terminate_sends_term_at_once_and_kill_after_the_grace() {
        let mut state = lifecycle(AllocationClass::Guaranteed);
        state.request_terminate(10);
        assert_eq!(state.phase, LifecyclePhase::Terminating);
        assert_eq!(state.termination_deadline_ms(), Some(10));
        assert_eq!(state.tick(29), LifecyclePhase::Terminating);
        assert_eq!(state.tick(30), LifecyclePhase::KillRequested);
    }
    #[test]
    fn loss_modes_have_different_guarantees() {
        let mut guaranteed = lifecycle(AllocationClass::Guaranteed);
        guaranteed.coordinator_disconnected();
        guaranteed.agent_failed(0);
        assert_eq!(guaranteed.tick(100_000), LifecyclePhase::Running);
        guaranteed.supervisor_failed();
        assert_eq!(
            guaranteed.tick(200_000),
            LifecyclePhase::NeedsReconciliation
        );
        let mut opportunistic = lifecycle(AllocationClass::Opportunistic);
        // Agent loss holds; the lease expiry (100 ms here) then drains.
        opportunistic.agent_failed(5);
        assert_eq!(opportunistic.tick(5), LifecyclePhase::Running);
        assert_eq!(opportunistic.tick(100), LifecyclePhase::Draining);
        opportunistic.supervisor_failed();
        assert_eq!(
            opportunistic.tick(100_000),
            LifecyclePhase::NeedsReconciliation
        );
    }
    /// The supervisor waits until the next lifecycle deadline: the lease expiry while it
    /// runs, then TERM and KILL after a drain (plan "Item 027").
    #[test]
    fn the_next_deadline_is_the_lease_then_term_then_kill() {
        let mut state = lifecycle(AllocationClass::Opportunistic);
        assert_eq!(state.next_deadline_ms(), Some(100));
        state.request_drain(40);
        assert_eq!(state.next_deadline_ms(), Some(70));
        assert_eq!(state.tick(70), LifecyclePhase::Terminating);
        assert_eq!(state.next_deadline_ms(), Some(90));
        assert_eq!(state.tick(90), LifecyclePhase::KillRequested);
        assert_eq!(state.next_deadline_ms(), None);
        // Guaranteed work has no lease expiry.
        assert_eq!(
            lifecycle(AllocationClass::Guaranteed).next_deadline_ms(),
            None
        );
    }
    /// A control line wakes a supervisor that waits on the wake socket.
    #[cfg(unix)]
    #[test]
    fn a_wake_makes_its_descriptor_readable_once_drained() {
        let wake = Wake::new().unwrap();
        let readable = |fd| {
            let mut item = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut item, 1, 0) == 1 }
        };
        assert!(!readable(wake.fd()));
        wake.clone().notify();
        wake.notify();
        assert!(readable(wake.fd()));
        wake.drain();
        assert!(!readable(wake.fd()));
    }
    #[test]
    fn grants_are_fenced_and_cannot_resurrect_expired_allocation() {
        let mut state = lifecycle(AllocationClass::Opportunistic);
        assert!(state.coordinator_renewal("other", 3, 1, 10).is_err());
        assert!(state.coordinator_renewal("a", 2, 1, 10).is_err());
        state.coordinator_renewal("a", 3, 1, 10).unwrap();
        assert!(state.coordinator_renewal("a", 3, 1, 50).is_err());
        assert_eq!(state.tick(109), LifecyclePhase::Running);
        assert!(state.coordinator_renewal("a", 3, 2, 110).is_err());
        assert_eq!(state.phase, LifecyclePhase::Draining);
    }
}

/// The supervisor's re-attachable control socket (research R2): an owner-only
/// directory and socket, a 32-byte token, a same-UID peer check, and one active
/// controller at a time. Socket paths are short-lived files under /tmp because a
/// Unix socket path must stay within about 100 bytes.
#[cfg(unix)]
pub mod control_socket {
    use anyhow::{Context, Result, bail, ensure};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const VERSION: u64 = 1;
    const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

    /// What the active controller sent, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Line {
        Attached,
        Text(String),
        Detached,
    }

    /// 32 random bytes as hex, for the supervisor spec (mode 0600).
    pub fn token() -> Result<String> {
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .context("random source unavailable")?
            .read_exact(&mut bytes)?;
        Ok(hex::encode(bytes))
    }

    fn same(a: &str, b: &str) -> bool {
        a.len() == b.len()
            && a.bytes()
                .zip(b.bytes())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0
    }

    fn peer_is_owner(stream: &UnixStream) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: a correctly sized credential query on a connected socket.
            let rc = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut size,
                )
            };
            rc == 0 && credentials.uid == unsafe { libc::geteuid() }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: a read-only credential query on a connected socket.
            let queried = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
            queried && uid == unsafe { libc::geteuid() }
        }
    }

    pub struct Listener {
        listener: UnixListener,
        files: Arc<Files>,
        token: String,
    }

    struct Files {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.directory);
        }
    }

    type Active = Arc<Mutex<Option<(u64, UnixStream)>>>;

    /// Writes to the active controller. Removes the socket files once dropped.
    #[derive(Clone)]
    pub struct Sink {
        active: Active,
        _files: Arc<Files>,
    }
    impl Sink {
        /// Returns false when no controller is attached or the write failed.
        pub fn write_line(&self, line: &str) -> bool {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, stream)) = active.as_mut() else {
                return false;
            };
            if writeln!(stream, "{line}")
                .and_then(|()| stream.flush())
                .is_ok()
            {
                return true;
            }
            *active = None;
            false
        }
    }

    impl Listener {
        pub fn bind(token: String) -> Result<Self> {
            ensure!(token.len() == 64, "control token must be 32 bytes of hex");
            let nonce = &self::token()?[..16];
            let directory = Path::new("/tmp")
                .canonicalize()?
                .join(format!("cedegrid-ctl-{nonce}"));
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let path = directory.join("c");
            let files = Arc::new(Files {
                directory,
                path: path.clone(),
            });
            let listener = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                listener,
                files,
                token,
            })
        }

        pub fn path(&self) -> &Path {
            &self.files.path
        }

        /// Accept controllers on background threads and deliver their lines.
        pub fn serve(self, deliver: impl Fn(Line) + Send + Sync + 'static) -> Sink {
            let active: Active = Arc::new(Mutex::new(None));
            let deliver = Arc::new(deliver);
            let accept_active = active.clone();
            let token = self.token;
            let listener = self.listener;
            std::thread::spawn(move || {
                let mut next_id = 0u64;
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    next_id += 1;
                    let (id, active, deliver, token) = (
                        next_id,
                        accept_active.clone(),
                        deliver.clone(),
                        token.clone(),
                    );
                    std::thread::spawn(move || controller(id, stream, &token, &active, &*deliver));
                }
            });
            Sink {
                active,
                _files: self.files,
            }
        }
    }

    fn controller(
        id: u64,
        stream: UnixStream,
        token: &str,
        active: &Active,
        deliver: &dyn Fn(Line),
    ) {
        let authenticated = (|| -> Result<BufReader<UnixStream>> {
            ensure!(
                peer_is_owner(&stream),
                "control peer is not the runtime user"
            );
            stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut line = String::new();
            reader.by_ref().take(4096).read_line(&mut line)?;
            let hello: serde_json::Value = serde_json::from_str(&line)?;
            ensure!(
                hello["kind"] == "hello"
                    && hello["version"] == VERSION
                    && hello["token"].as_str().is_some_and(|t| same(t, token)),
                "control hello refused"
            );
            stream.set_read_timeout(None)?;
            Ok(reader)
        })();
        let Ok(reader) = authenticated else {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        };
        {
            let mut current = active.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, old)) = current.take() {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
            let Ok(mut writer) = stream.try_clone() else {
                return;
            };
            if writeln!(
                writer,
                "{}",
                serde_json::json!({"ok":true,"version":VERSION})
            )
            .is_err()
            {
                return;
            }
            *current = Some((id, writer));
        }
        deliver(Line::Attached);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let still_active = active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|(current, _)| *current == id);
            if !still_active {
                return;
            }
            deliver(Line::Text(line));
        }
        let mut current = active.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|(current, _)| *current == id) {
            *current = None;
            drop(current);
            deliver(Line::Detached);
        }
    }

    /// Connect as the controller and authenticate with the token.
    pub fn connect(path: &Path, token: &str) -> Result<UnixStream> {
        let mut stream = UnixStream::connect(path)?;
        writeln!(
            stream,
            "{}",
            serde_json::json!({"kind":"hello","token":token,"version":VERSION})
        )?;
        stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
        let mut reply = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut reply)?;
        let reply: serde_json::Value = match serde_json::from_str(&reply) {
            Ok(reply) => reply,
            Err(_) => bail!("supervisor refused the control connection"),
        };
        ensure!(
            reply["ok"] == true,
            "supervisor refused the control connection"
        );
        stream.set_read_timeout(None)?;
        Ok(stream)
    }
}
