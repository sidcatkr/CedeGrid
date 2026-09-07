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
    pub termination_started_ms: Option<u64>,
    pub process_exit_ms: u64,
    pub release_confirmed_ms: Option<u64>,
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
    pub fn agent_failed(&mut self, now_ms: u64) {
        if self.class == AllocationClass::Opportunistic {
            self.request_drain(now_ms);
        }
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
            if elapsed >= self.drain_ms.saturating_add(self.term_ms) {
                self.phase = LifecyclePhase::KillRequested;
            } else if elapsed >= self.drain_ms {
                self.phase = LifecyclePhase::Terminating;
            }
        }
        self.phase
    }
    pub fn confirm_release(&mut self) {
        self.phase = LifecyclePhase::Released;
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
mod unix {
    use super::*;
    use std::{
        fs::{self, File, OpenOptions},
        io::{self, BufRead, Read, Write},
        os::{
            fd::{AsRawFd, OwnedFd},
            unix::{
                fs::DirBuilderExt,
                process::{CommandExt, ExitStatusExt},
            },
        },
        path::PathBuf,
        process::{Child, Command, ExitStatus, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[cfg(target_os = "linux")]
    use std::os::fd::FromRawFd;

    /// Direct-child ownership and reaping stay serialized through &mut self.
    /// A process cannot be reaped between identity verification and pidfd_open.
    struct OwnedChild {
        child: Child,
        pidfd: Option<OwnedFd>,
        reaped: bool,
        handle_detail: String,
    }
    impl OwnedChild {
        fn new(child: Child) -> Self {
            Self {
                child,
                pidfd: None,
                reaped: false,
                handle_detail: "verified unreaped direct-child fallback".into(),
            }
        }
        fn acquire_handle(&mut self) -> Result<()> {
            #[cfg(target_os = "linux")]
            self.acquire_handle_with(|pid| {
                // SAFETY: owned unreaped direct child; no concurrent waiter exists.
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: successful syscall returns a newly owned descriptor.
                let handle = unsafe { OwnedFd::from_raw_fd(fd as i32) };
                // Signal zero is a non-destructive permission/interface probe
                // against this owned gate, before any workload is authorized.
                let permitted = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        handle.as_raw_fd(),
                        0,
                        std::ptr::null::<libc::siginfo_t>(),
                        0u32,
                    )
                };
                if permitted != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(handle)
            })?;
            Ok(())
        }
        #[cfg(target_os = "linux")]
        fn acquire_handle_with(
            &mut self,
            open: impl FnOnce(u32) -> io::Result<OwnedFd>,
        ) -> Result<()> {
            ensure!(
                !self.reaped && self.pidfd.is_none(),
                "handle acquisition must precede reaping"
            );
            match open(self.child.id()) {
                Ok(fd) => {
                    self.pidfd = Some(fd);
                    self.handle_detail = "linux_pidfd".into();
                }
                Err(error) => {
                    ensure!(
                        matches!(
                            error.raw_os_error(),
                            Some(libc::ENOSYS | libc::EINVAL | libc::EPERM | libc::EACCES)
                        ),
                        "pidfd acquisition failed for owned child: {error}"
                    );
                    self.handle_detail = format!(
                        "verified unreaped direct-child fallback: pidfd unavailable ({error})"
                    );
                }
            }
            Ok(())
        }
        fn signal(&mut self, signal: i32) -> Result<()> {
            ensure!(!self.reaped, "refusing to signal a reaped child");
            #[cfg(target_os = "linux")]
            if let Some(fd) = self.pidfd.as_ref() {
                // SAFETY: handle denotes the owned child; no PID lookup is used.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0u32,
                    )
                };
                if rc == 0 {
                    return Ok(());
                }
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                bail!(
                    "pidfd signal failed; no numeric PID fallback after handle acquisition: {err}"
                );
            }
            // SAFETY: only this owner can reap this direct child, SIGCHLD is normal,
            // and try_wait marks reaped before another signal can be attempted.
            let rc = unsafe { libc::kill(self.child.id() as libc::pid_t, signal) };
            if rc != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err.into());
                }
            }
            Ok(())
        }
        fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
            #[cfg(target_os = "linux")]
            if let Some(fd) = self.pidfd.as_ref() {
                let mut item = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: read-only lifecycle observation through this child's stable handle.
                let ready = unsafe { libc::poll(&mut item, 1, 0) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        return Ok(None);
                    }
                    return Err(error.into());
                }
                ensure!(
                    item.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
                    "pidfd lifecycle observation failed"
                );
                if ready == 0 {
                    return Ok(None);
                }
            }
            let status = self.child.try_wait()?;
            if status.is_some() {
                self.reaped = true;
            }
            Ok(status)
        }
        fn kill_and_confirm(&mut self, timeout: Duration) -> Result<()> {
            if self.reaped {
                return Ok(());
            }
            self.signal(libc::SIGKILL)?;
            let deadline = Instant::now()
                .checked_add(timeout)
                .context("release confirmation timeout is too large")?;
            loop {
                if self.try_wait()?.is_some() {
                    return Ok(());
                }
                ensure!(
                    Instant::now() < deadline,
                    "owned child exit remains unconfirmed after SIGKILL"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn check_reaping_policy() -> Result<()> {
        // SAFETY: read-only query to a correctly sized initialized sigaction.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) };
        ensure!(
            rc == 0,
            "cannot inspect SIGCHLD policy: {}",
            io::Error::last_os_error()
        );
        ensure!(
            action.sa_sigaction == libc::SIG_DFL && action.sa_flags & libc::SA_NOCLDWAIT == 0,
            "supervision requires default SIGCHLD and exclusive direct-child reaping"
        );
        Ok(())
    }

    fn clear_nonblocking(fd: i32) -> Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == 0,
            "cannot restore blocking log pipe"
        );
        Ok(())
    }
    fn set_nonblocking(fd: i32) -> Result<()> {
        // SAFETY: these descriptors belong to supervisor-owned protocol pipes.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(flags >= 0, "cannot read protocol descriptor flags");
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        ensure!(rc == 0, "cannot make protocol pipe nonblocking");
        Ok(())
    }
    fn wait_ready(fd: i32, events: i16, deadline: Instant) -> Result<()> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "launch preparation deadline exceeded");
            let mut item = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            // SAFETY: poll receives one valid initialized element.
            let rc = unsafe { libc::poll(&mut item, 1, remaining.as_millis().min(100) as i32 + 1) };
            if rc > 0 {
                return Ok(());
            }
            if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error().into());
            }
        }
    }
    fn write_deadline(
        pipe: &mut (impl Write + AsRawFd),
        bytes: &[u8],
        deadline: Instant,
    ) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(
                Instant::now() < deadline,
                "launch preparation deadline exceeded"
            );
            match pipe.write(&bytes[offset..]) {
                Ok(0) => bail!("worker gate closed its input"),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(pipe.as_raw_fd(), libc::POLLOUT, deadline)?
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn read_identity(
        pipe: &mut (impl Read + AsRawFd),
        deadline: Instant,
    ) -> Result<ProcessIdentity> {
        let mut bytes = Vec::new();
        let mut buf = [0; 1024];
        loop {
            ensure!(
                Instant::now() < deadline,
                "launch preparation deadline exceeded"
            );
            match pipe.read(&mut buf) {
                Ok(0) => bail!("worker gate exited before identity verification"),
                Ok(n) => {
                    bytes.extend_from_slice(&buf[..n]);
                    ensure!(bytes.len() <= 65_536, "oversized gate identity response");
                    if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                        ensure!(
                            end + 1 == bytes.len(),
                            "unexpected trailing gate protocol data"
                        );
                        return serde_json::from_slice(&bytes[..end])
                            .context("invalid gate identity");
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(pipe.as_raw_fd(), libc::POLLIN, deadline)?
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    struct DrainDirectory {
        path: PathBuf,
    }
    impl DrainDirectory {
        fn new() -> Result<Self> {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            Self::with_nonce(&std::env::temp_dir(), nonce)
        }
        fn with_nonce(parent: &Path, nonce: u128) -> Result<Self> {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
            Self::create_with_candidates(|| {
                // Wall-clock nanoseconds are not unique on every platform. This
                // sequence separates concurrent launches even at a frozen clock.
                let sequence = NEXT_DIRECTORY
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                        value.checked_add(1)
                    })
                    .map_err(|_| anyhow::anyhow!("drain directory sequence exhausted"))?;
                Ok(parent.join(format!(
                    "resmgr-drain-{}-{nonce}-{sequence}",
                    std::process::id()
                )))
            })
        }
        fn create_with_candidates(mut candidate: impl FnMut() -> Result<PathBuf>) -> Result<Self> {
            for _ in 0..64 {
                let path = candidate()?;
                let mut builder = fs::DirBuilder::new();
                match builder.mode(0o700).create(&path) {
                    // Ownership is established only by a successful atomic mkdir.
                    Ok(()) => return Ok(Self { path }),
                    // Existing directories/files/symlinks are never opened,
                    // removed, reused, or modified. Retry with a fresh sequence.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            bail!("could not allocate a private drain directory after name collisions")
        }
        fn file(&self) -> PathBuf {
            self.path.join("drain")
        }
        fn notify(&self) -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.file())?;
            file.write_all(b"drain\n")?;
            Ok(())
        }
    }
    impl Drop for DrainDirectory {
        fn drop(&mut self) {
            // Only known owned entries; no recursive deletion or traversal.
            let _ = fs::remove_file(self.file());
            let _ = fs::remove_dir(&self.path);
        }
    }

    struct OutputCapture {
        readers: Vec<std::thread::JoinHandle<io::Result<()>>>,
    }
    impl OutputCapture {
        fn start(
            output: impl Read + Send + 'static,
            error: impl Read + Send + 'static,
            directory: &Path,
        ) -> Result<Self> {
            use std::os::unix::fs::OpenOptionsExt;
            let capture = |mut reader: Box<dyn Read + Send>, name: &str| -> Result<_> {
                let path = directory.join(name);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?;
                Ok(std::thread::Builder::new()
                    .name("resmgr-log-drain".into())
                    .spawn(move || -> io::Result<()> {
                        const LIMIT: usize = 4 * 1024 * 1024;
                        let mut retained = 0;
                        let mut buffer = [0; 16384];
                        loop {
                            let n = reader.read(&mut buffer)?;
                            if n == 0 {
                                break;
                            }
                            let keep = n.min(LIMIT.saturating_sub(retained));
                            if keep > 0 {
                                file.write_all(&buffer[..keep])?;
                                retained += keep;
                            }
                            // Continue draining after the cap; workload pipes never block on log retention.
                        }
                        file.sync_all()
                    })?)
            };
            Ok(Self {
                readers: vec![
                    capture(Box::new(output), "stdout.log")?,
                    capture(Box::new(error), "stderr.log")?,
                ],
            })
        }
        fn finish(self) -> Result<()> {
            for reader in self.readers {
                reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("log drain thread panicked"))??;
            }
            Ok(())
        }
    }
    include!("child_supervision.rs");

    fn apply_control_commands(
        commands: Vec<SupervisorCommand>,
        lifecycle: &mut Lifecycle,
        start: Instant,
        polled_at: u64,
    ) {
        for command in commands {
            let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            match command {
                SupervisorCommand::Renew {
                    assignment_id,
                    generation,
                    sequence,
                    remaining_ms,
                } => {
                    // Invalid/stale grants never cancel an already-started drain.
                    if lifecycle
                        .coordinator_renewal(&assignment_id, generation, sequence, elapsed)
                        .is_ok()
                    {
                        // Remaining authority may have been read before slow
                        // telemetry inside poll. Anchoring it at poll entry
                        // never extends that grant by the observation delay.
                        lifecycle.cap_lease_remaining(polled_at, remaining_ms);
                    }
                }
                SupervisorCommand::Drain => lifecycle.request_drain(elapsed),
                SupervisorCommand::AgentLost => lifecycle.agent_failed(elapsed),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn supervise(
        request: &LaunchRequest,
        capacity: &Resources,
        options: &SupervisorOptions,
        journal: &dyn ExecutionJournal,
        backend: &mut dyn LaunchBackend,
        executable: &Path,
        control: &mut dyn SupervisorControl,
    ) -> Result<ExecutionOutcome> {
        ensure!(
            !request.argv.is_empty() && !request.argv[0].is_empty(),
            "argv must contain an executable"
        );
        ensure!(
            request.cwd.is_absolute() && request.cwd.is_dir(),
            "cwd must be an existing absolute directory"
        );
        ensure!(
            request.no_escape,
            "execution requires acknowledgement of the no-escape workload contract"
        );
        ensure!(
            backend.name() != "rootless"
                || (request.single_process && request.managed_child_limit == 0)
                || (!request.single_process && (1..=8).contains(&request.managed_child_limit)),
            "rootless execution requires single-process or explicitly bounded supervisor-mediated child contract"
        );
        ensure!(
            (0..=19).contains(&options.nice),
            "nice must be between 0 and 19"
        );
        ensure!(
            options.prepare_timeout_ms > 0
                && options.lease_ms > 0
                && options.release_confirm_timeout_ms > 0,
            "preparation timeout, release confirmation timeout and lease must be positive"
        );
        let gpu_release = crate::telemetry::GpuReleaseGuard::prepare(&request.resources)?;
        check_reaping_policy()?;
        let drain = DrainDirectory::new()?;
        let mut record = journal.reserve(request, capacity)?;
        record.backend = backend.name().into();
        let mut child: Option<OwnedChild> = None;
        let mut children: Option<ChildServer> = None;
        let mut capture: Option<OutputCapture> = None;
        let result = (|| -> Result<ExecutionOutcome> {
            let deadline = Instant::now()
                .checked_add(Duration::from_millis(options.prepare_timeout_ms))
                .context("preparation timeout is too large")?;
            let mut setup = backend.prepare(request)?;
            if request.class == AllocationClass::Opportunistic && setup.nice.is_none() {
                setup.nice = Some(options.nice);
            }
            children = ChildServer::new(request, record.generation, setup.clone())?;
            let mut gated_request = request.clone();
            if request.env.contains_key("RESMGR_OUTPUT_DIR") {
                gated_request
                    .env
                    .insert("RESMGR_CAPTURE_OUTPUT".into(), "1".into());
            }
            if let Some(server) = &children {
                server.expose(&mut gated_request);
            }
            let gate = GateRequest {
                request: gated_request,
                generation: record.generation,
                setup,
                drain_file: drain.file(),
            };
            let mut payload = serde_json::to_vec(&gate)?;
            ensure!(
                payload.len() <= 2 * 1024 * 1024,
                "launch request exceeds private protocol limit"
            );
            payload.push(b'\n');
            let mut gate_command = Command::new(executable);
            let raw_child = gate_command
                .arg("__worker-gate")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(if request.env.contains_key("RESMGR_OUTPUT_DIR") {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                })
                .spawn()
                .context("cannot spawn trusted worker gate")?;
            child = Some(OwnedChild::new(raw_child));
            let owned = child.as_mut().expect("just installed child");
            // Never call try_wait or permit another reaper before this operation.
            owned.acquire_handle()?;
            let mut input = owned.child.stdin.take().context("missing gate input")?;
            let mut output = owned.child.stdout.take().context("missing gate output")?;
            set_nonblocking(input.as_raw_fd())?;
            set_nonblocking(output.as_raw_fd())?;
            write_deadline(&mut input, &payload, deadline)?;
            let claimed = read_identity(&mut output, deadline)?;
            let actual = identity(owned.child.id(), &record.assignment_id, record.generation)?;
            ensure!(
                claimed == actual,
                "gate identity does not match owned child and assignment"
            );
            record.identity = Some(actual.clone());
            record.backend = backend.name().into();
            record.evidence = backend.verify(&actual)?;
            record.evidence.extend(journal.storage_control_evidence()?);
            if let Some(cpus) = request.env.get("RESMGR_CPU_AFFINITY") {
                let mut requested: Vec<u32> = serde_json::from_str(cpus)?;
                requested.sort_unstable();
                #[cfg(target_os = "linux")]
                {
                    ensure!(
                        read_cpu_affinity(actual.pid)? == requested,
                        "owned gate CPU affinity verification failed"
                    );
                    record.evidence.push(ControlEvidence{control:"cpu.affinity".into(),available:Some(true),permitted:Some(true),configured:true,applied:true,fallback:false,scope:format!("direct_child:{}",actual.pid),requested:Some(cpus.clone()),effective:Some(serde_json::to_string(&requested)?),detail:"Exact inherited/gate CPU-ID placement within permitted CPUs; not a CPU bandwidth guarantee".into()});
                }
                #[cfg(not(target_os = "linux"))]
                bail!("required CPU affinity unavailable");
            }
            if let Some(requested_nice) = gate.setup.nice {
                // SAFETY: a read-only query for the verified, unreaped owned gate.
                let actual_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, actual.pid) };
                ensure!(
                    actual_nice == requested_nice,
                    "gate nice setting was not verified"
                );
                record.evidence.push(ControlEvidence { control: "cpu.nice".into(), available: Some(true),
                    permitted: Some(true), configured: true, applied: true, fallback: false,
                    scope: format!("direct_child:{}", actual.pid), requested: Some(requested_nice.to_string()),
                    effective: Some(actual_nice.to_string()),
                    detail: "Verified per-process scheduler priority; not a CPU percentage or bandwidth guarantee".into() });
            }

            record.evidence.push(ControlEvidence { control: "process_handle".into(), available: Some(true),
                permitted: Some(true), configured: true, applied: true,
                fallback: owned.pidfd.is_none(), scope: format!("direct_child:{}", actual.pid),
                requested: Some("stable owned process handle".into()), effective: Some(owned.handle_detail.clone()),
                detail: "Leader-only lifecycle observation; not descendant containment. Exclusive direct-child reaping required.".into() });
            for required in &request.required_controls {
                ensure!(
                    record.evidence.iter().any(|e| &e.control == required
                        && e.applied
                        && !e.fallback
                        && e.available == Some(true)
                        && e.permitted == Some(true)),
                    "required control {required} has not been successfully applied"
                );
            }
            if let Some(directory) = request.env.get("RESMGR_OUTPUT_DIR") {
                // Identity is fully consumed before this reader takes the gate stdout pipe.
                // Reset O_NONBLOCK for the dedicated drain thread; no process reaping there.
                clear_nonblocking(output.as_raw_fd())?;
                capture = Some(OutputCapture::start(
                    output,
                    owned
                        .child
                        .stderr
                        .take()
                        .context("missing workload stderr pipe")?,
                    Path::new(directory),
                )?);
            } else {
                drop(output);
            }
            record.phase = ExecutionPhase::Prepared;
            record.detail =
                "Gate blocked: identity, required controls and backend membership verified".into();
            journal.transition(&record)?;
            control.prepared(&record)?;
            ensure!(
                Instant::now() < deadline,
                "launch preparation deadline exceeded before authorization"
            );
            record.phase = ExecutionPhase::Authorized;
            record.detail =
                "Durable authorization committed before sending EXEC to the blocked gate".into();
            journal.transition(&record)?;
            write_deadline(&mut input, b"EXEC\n", deadline)?;
            drop(input);
            let start = Instant::now();
            let mut lifecycle = Lifecycle::new(
                request.class,
                record.assignment_id.clone(),
                record.generation,
                0,
                options,
            );
            if let Some(remaining) = control.initial_remaining_ms() {
                lifecycle.cap_lease_remaining(0, remaining);
            }
            record.phase = ExecutionPhase::Running;
            record.detail =
                "EXEC sent after durable authorization; no claim that exec itself succeeded yet"
                    .into();
            journal.transition(&record)?;
            let mut drain_started_ms = None;
            let mut termination_started_ms = None;
            let mut killed = false;
            let mut kill_at = None;
            let mut leader_status = None;
            let mut leader_exit_ms = None;
            loop {
                let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                if leader_status.is_none()
                    && let Some(status) = owned.try_wait()?
                {
                    leader_status = Some(status);
                    leader_exit_ms = Some(elapsed);
                    if let Some(server) = &mut children {
                        server.request_stop();
                    }
                }
                if let Some(status) = leader_status
                    && children.as_ref().is_none_or(ChildServer::all_released)
                {
                    if let Some(logs) = capture.take() {
                        logs.finish()?;
                    }
                    backend.confirm_release()?;
                    gpu_release.confirm(
                        actual.pid,
                        Duration::from_millis(options.release_confirm_timeout_ms),
                    )?;
                    lifecycle.confirm_release();
                    record.phase = ExecutionPhase::Released;
                    record.detail = "Leader and every registered mediated child reaped; backend/GPU release confirmed".into();
                    journal.transition(&record)?;
                    return Ok(ExecutionOutcome {
                        record: record.clone(),
                        exit_code: status.code(),
                        termination_signal: status.signal(),
                        yielded: drain_started_ms.is_some(),
                        process_handle: owned.handle_detail.clone(),
                        drain_started_ms,
                        termination_started_ms,
                        process_exit_ms: leader_exit_ms.unwrap_or(elapsed),
                        release_confirmed_ms: Some(
                            start.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        ),
                    });
                }
                let polled_at = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                apply_control_commands(control.poll()?, &mut lifecycle, start, polled_at);
                let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let phase = lifecycle.tick(elapsed);
                if let Some(server) = &mut children {
                    let accepting = phase == LifecyclePhase::Running && leader_status.is_none();
                    let mut authorize_child = |request: &LaunchRequest| -> Result<()> {
                        let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                        ensure!(
                            lifecycle.tick(elapsed) == LifecyclePhase::Running
                                && leader_status.is_none(),
                            "parent no longer authorizes managed child launches"
                        );
                        let commands = control.authorize_managed_child(request)?;
                        // The hook returns queued cancellation/grants after its
                        // final telemetry check. Do not poll telemetry again:
                        // Guaranteed continuity could ignore newer unsafe GPU
                        // observations for the parent but not for a new child.
                        apply_control_commands(commands, &mut lifecycle, start, elapsed);
                        let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                        if let Some(status) = owned.try_wait()? {
                            leader_status = Some(status);
                            leader_exit_ms = Some(elapsed);
                        }
                        ensure!(
                            lifecycle.tick(elapsed) == LifecyclePhase::Running
                                && leader_status.is_none(),
                            "parent authorization expired or ended during child launch verification"
                        );
                        Ok(())
                    };
                    server.poll(
                        &actual,
                        accepting,
                        journal,
                        backend,
                        executable,
                        options,
                        &gpu_release,
                        &mut authorize_child,
                    )?;
                }
                let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let phase = lifecycle.tick(elapsed);
                if phase != LifecyclePhase::Running && drain_started_ms.is_none() {
                    drain.notify()?;
                    drain_started_ms = Some(elapsed);
                    record.phase = ExecutionPhase::Draining;
                    record.detail =
                        "Local policy, cancellation, or authorization deadline requested drain"
                            .into();
                    journal.transition(&record)?;
                }
                if matches!(
                    phase,
                    LifecyclePhase::Terminating | LifecyclePhase::KillRequested
                ) && termination_started_ms.is_none()
                {
                    if leader_status.is_none() {
                        owned.signal(libc::SIGTERM)?;
                    }
                    termination_started_ms = Some(elapsed);
                }
                if phase == LifecyclePhase::KillRequested && !killed {
                    if leader_status.is_none() {
                        owned.signal(libc::SIGKILL)?;
                    }
                    killed = true;
                    kill_at = Some(Instant::now());
                }
                if let Some(kill_at) = kill_at {
                    ensure!(
                        kill_at.elapsed()
                            < Duration::from_millis(options.release_confirm_timeout_ms),
                        "resource release remains unconfirmed after forced termination"
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        match result {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                for evidence in backend.preparation_evidence() {
                    if let Some(prior) = record
                        .evidence
                        .iter_mut()
                        .find(|e| e.control == evidence.control)
                    {
                        *prior = evidence;
                    } else {
                        record.evidence.push(evidence);
                    }
                }
                // A failed preparation never sends EXEC. If authorization was sent,
                // only a verified owned child is signaled. Uncertain release remains charged.
                let child_release = match child.as_mut() {
                    Some(owned) => owned.kill_and_confirm(Duration::from_millis(
                        options.release_confirm_timeout_ms,
                    )),
                    None => Ok(()),
                };
                let family_release = children.as_mut().map_or(Ok(()), |server| {
                    server.cleanup(journal, options, &gpu_release)
                });
                let release = child_release
                    .and(family_release)
                    .and_then(|()| backend.confirm_release())
                    .and_then(|()| {
                        if let Some(identity) = &record.identity {
                            gpu_release.confirm(
                                identity.pid,
                                Duration::from_millis(options.release_confirm_timeout_ms),
                            )
                        } else {
                            Ok(())
                        }
                    });
                record.phase = if release.is_ok() {
                    ExecutionPhase::Released
                } else {
                    ExecutionPhase::NeedsReconciliation
                };
                record.detail = format!(
                    "Execution failed: {error:#}; cleanup: {}",
                    match release {
                        Ok(()) => "verified release".into(),
                        Err(ref e) => format!("unconfirmed ({e:#}); capacity retained"),
                    }
                );
                if let Err(persist_error) = journal.transition(&record) {
                    return Err(error.context(format!("cleanup state not committed: {persist_error:#}; persisted reservation must remain charged")));
                }
                Err(error.context(record.detail))
            }
        }
    }

    pub(super) fn worker_gate() -> Result<()> {
        let input = io::stdin();
        let mut input = input.lock();
        let mut line = String::new();
        let bytes = input
            .by_ref()
            .take(2 * 1024 * 1024 + 1)
            .read_line(&mut line)?;
        ensure!(
            bytes > 0 && bytes <= 2 * 1024 * 1024 && line.ends_with('\n'),
            "missing or oversized gate setup"
        );
        let gate: GateRequest = serde_json::from_str(&line)?;
        ensure!(!gate.request.argv.is_empty(), "missing workload argv");
        if let Some(path) = &gate.setup.cgroup_path {
            #[cfg(target_os = "linux")]
            {
                // Backend created and verified the leaf before this trusted gate.
                // Write only this process's membership, never another PID.
                join_cgroup(path)?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = path;
                bail!("cgroup gate unavailable on this platform");
            }
        }
        if let Some(nice) = gate.setup.nice {
            ensure!((0..=19).contains(&nice), "invalid requested nice value");
            // SAFETY: changes only the current gate, before user execution.
            let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            ensure!(
                rc == 0,
                "gate nice application failed: {}",
                io::Error::last_os_error()
            );
        }
        if let Some(cpus) = gate.request.env.get("RESMGR_CPU_AFFINITY") {
            apply_current_cpu_affinity(&serde_json::from_str::<Vec<u32>>(cpus)?)?;
        }
        let id = identity(
            std::process::id(),
            &gate.request.assignment_id,
            gate.generation,
        )?;
        serde_json::to_writer(io::stdout().lock(), &id)?;
        io::stdout().write_all(b"\n")?;
        io::stdout().flush()?;
        line.clear();
        let n = input.by_ref().take(16).read_line(&mut line)?;
        ensure!(
            n > 0 && line == "EXEC\n",
            "supervisor disappeared or did not authorize execution"
        );
        drop(input);
        let mut command = Command::new(&gate.request.argv[0]);
        command
            .args(&gate.request.argv[1..])
            .current_dir(&gate.request.cwd)
            .envs(&gate.request.env)
            .env("RESMGR_ASSIGNMENT_ID", &gate.request.assignment_id)
            .env("RESMGR_TASK_ID", &gate.request.task_id)
            .env("RESMGR_ATTEMPT_GENERATION", gate.generation.to_string())
            .env("RESMGR_DRAIN_FILE", &gate.drain_file)
            .stdin(Stdio::from(File::open("/dev/null")?))
            .stdout(
                if gate
                    .request
                    .env
                    .get("RESMGR_CAPTURE_OUTPUT")
                    .is_some_and(|v| v == "1")
                {
                    Stdio::inherit()
                } else {
                    Stdio::from(OpenOptions::new().write(true).open("/dev/null")?)
                },
            )
            .stderr(Stdio::inherit());
        Err(command.exec()).context("authorized workload exec failed")
    }

    #[cfg(target_os = "linux")]
    fn join_cgroup(path: &Path) -> Result<()> {
        use std::{ffi::CString, path::Component};
        ensure!(
            path.is_absolute(),
            "cgroup gate requires an absolute leaf path"
        );
        // Resolve every component through directory handles; never follow a
        // substituted symlink or create/truncate a file through the gate.
        let root_fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        ensure!(root_fd >= 0, "cannot open filesystem root for cgroup join");
        // SAFETY: successful open returns a new owned descriptor.
        let mut dir = unsafe { OwnedFd::from_raw_fd(root_fd) };
        for component in path.components() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) => {
                    use std::os::unix::ffi::OsStrExt;
                    let name = CString::new(name.as_bytes())?;
                    // SAFETY: valid retained directory, bounded NUL-terminated component.
                    let fd = unsafe {
                        libc::openat(
                            dir.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    ensure!(
                        fd >= 0,
                        "cgroup gate path traversal failed: {}",
                        io::Error::last_os_error()
                    );
                    // SAFETY: successful openat returns a new owned descriptor.
                    dir = unsafe { OwnedFd::from_raw_fd(fd) };
                }
                _ => bail!("unsafe cgroup gate path component"),
            }
        }
        // SAFETY: initialized native statfs storage and valid retained descriptor.
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatfs(dir.as_raw_fd(), &mut info) };
        ensure!(
            rc == 0 && info.f_type as u64 == 0x6367_7270,
            "cgroup gate refuses a leaf outside cgroup v2"
        );
        // SAFETY: relative open under the verified cgroup filesystem handle.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c"cgroup.procs".as_ptr(),
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(
            fd >= 0,
            "cannot open cgroup membership for self-join: {}",
            io::Error::last_os_error()
        );
        // SAFETY: successful openat returns a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn identity(pid: u32, assignment: &str, generation: u64) -> Result<ProcessIdentity> {
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        ensure!(!boot_id.is_empty(), "missing kernel boot identity");
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let after_name = stat
            .rsplit_once(')')
            .context("invalid /proc process stat")?
            .1;
        // First field after ')' is field 3; starttime is field 22.
        let start_time = after_name
            .split_whitespace()
            .nth(19)
            .context("missing process start ticks")?
            .parse()?;
        Ok(ProcessIdentity {
            pid,
            boot_id,
            start_time,
            assignment_id: assignment.into(),
            generation,
        })
    }
    #[cfg(target_os = "macos")]
    pub(super) fn identity(pid: u32, assignment: &str, generation: u64) -> Result<ProcessIdentity> {
        // SAFETY: initialized structure, exact libproc ABI size and valid pointer.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        ensure!(
            read == size as i32 && info.pbi_pid == pid,
            "cannot verify native process identity: {}",
            io::Error::last_os_error()
        );
        let mut uuid = [0u8; 128];
        let mut len = uuid.len();
        // SAFETY: read-only sysctl into bounded writable storage.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                uuid.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        ensure!(
            rc == 0 && len <= uuid.len() && len > 1,
            "cannot read native boot identity"
        );
        let boot_id = std::str::from_utf8(&uuid[..len])?
            .trim_end_matches('\0')
            .to_owned();
        let start_time = info
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|v| v.checked_add(info.pbi_start_tvusec))
            .context("native process start overflow")?;
        Ok(ProcessIdentity {
            pid,
            boot_id,
            start_time,
            assignment_id: assignment.into(),
            generation,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn identity(_pid: u32, _assignment: &str, _generation: u64) -> Result<ProcessIdentity> {
        bail!("native process identity is unavailable on this platform")
    }
    #[cfg(test)]
    mod handle_tests {
        use super::*;
        fn child() -> OwnedChild {
            check_reaping_policy().unwrap();
            OwnedChild::new(Command::new("/bin/sleep").arg("5").spawn().unwrap())
        }
        #[test]
        fn concurrent_drain_directories_are_unique_with_a_frozen_clock() {
            use std::os::unix::fs::PermissionsExt;
            use std::{
                collections::BTreeSet,
                sync::{Arc, Barrier},
            };
            let parent = tempfile::tempdir().unwrap();
            let barrier = Arc::new(Barrier::new(32));
            let threads: Vec<_> = (0..32)
                .map(|_| {
                    let base = parent.path().to_owned();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        DrainDirectory::with_nonce(&base, 1_000_000).unwrap()
                    })
                })
                .collect();
            let directories: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            let paths: BTreeSet<_> = directories.iter().map(|d| d.path.clone()).collect();
            assert_eq!(paths.len(), 32);
            for directory in &directories {
                assert_eq!(
                    fs::metadata(&directory.path).unwrap().permissions().mode() & 0o077,
                    0
                );
            }
            drop(directories);
            assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        }
        #[test]
        fn drain_directory_collision_never_adopts_or_modifies_existing_paths() {
            use std::os::unix::fs::symlink;
            let parent = tempfile::tempdir().unwrap();
            let existing = parent.path().join("existing");
            fs::create_dir(&existing).unwrap();
            fs::write(existing.join("drain"), "unrelated").unwrap();
            let link = parent.path().join("link");
            symlink(&existing, &link).unwrap();
            let fresh = parent.path().join("fresh");
            let mut candidates = [existing.clone(), link.clone(), fresh.clone()].into_iter();
            let owned =
                DrainDirectory::create_with_candidates(|| Ok(candidates.next().unwrap())).unwrap();
            assert_eq!(owned.path, fresh);
            owned.notify().unwrap();
            drop(owned);
            assert!(!fresh.exists());
            assert_eq!(
                fs::read_to_string(existing.join("drain")).unwrap(),
                "unrelated"
            );
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
        #[test]
        fn reaping_and_signaling_are_serialized_by_one_owner() {
            let mut owned = child();
            owned.acquire_handle().unwrap();
            owned.kill_and_confirm(Duration::from_secs(2)).unwrap();
            assert!(owned.reaped);
            assert!(owned.signal(libc::SIGTERM).is_err());
        }
        #[cfg(target_os = "linux")]
        #[test]
        fn pidfd_permission_failure_reports_direct_child_fallback() {
            let mut owned = child();
            owned
                .acquire_handle_with(|_| Err(io::Error::from_raw_os_error(libc::EPERM)))
                .unwrap();
            assert!(owned.pidfd.is_none());
            assert!(owned.handle_detail.contains("fallback"));
            owned.kill_and_confirm(Duration::from_secs(2)).unwrap();
        }
        #[cfg(target_os = "linux")]
        #[test]
        fn unexpected_pidfd_failure_does_not_lose_owned_child() {
            let mut owned = child();
            assert!(
                owned
                    .acquire_handle_with(|_| Err(io::Error::from_raw_os_error(libc::ESRCH)))
                    .is_err()
            );
            assert!(!owned.reaped);
            owned.kill_and_confirm(Duration::from_secs(2)).unwrap();
        }
        #[cfg(target_os = "linux")]
        #[test]
        fn pidfd_signal_failure_never_silently_uses_numeric_pid() {
            let mut owned = child();
            owned.pidfd = Some(File::open("/dev/null").unwrap().into());
            assert!(owned.signal(libc::SIGKILL).is_err());
            // Discard the injected fake handle, then inspect/reap only our child.
            owned.pidfd = None;
            assert!(owned.try_wait().unwrap().is_none());
            owned.kill_and_confirm(Duration::from_secs(2)).unwrap();
        }
        #[cfg(target_os = "linux")]
        #[test]
        fn gate_refuses_ordinary_and_symlinked_filesystems_before_writes() {
            use std::os::unix::fs::symlink;
            let temp = tempfile::tempdir().unwrap();
            let ordinary = temp.path().join("leaf");
            fs::create_dir(&ordinary).unwrap();
            fs::write(ordinary.join("cgroup.procs"), "untouched").unwrap();
            let link = temp.path().join("link");
            symlink(&ordinary, &link).unwrap();
            assert!(join_cgroup(&ordinary).is_err());
            assert!(join_cgroup(&link).is_err());
            assert_eq!(
                fs::read_to_string(ordinary.join("cgroup.procs")).unwrap(),
                "untouched"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        opportunistic.agent_failed(5);
        assert_eq!(opportunistic.tick(5), LifecyclePhase::Draining);
        opportunistic.supervisor_failed();
        assert_eq!(
            opportunistic.tick(100_000),
            LifecyclePhase::NeedsReconciliation
        );
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
