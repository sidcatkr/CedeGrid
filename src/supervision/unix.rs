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
    fn acquire_handle_with(&mut self, open: impl FnOnce(u32) -> io::Result<OwnedFd>) -> Result<()> {
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
                self.handle_detail =
                    format!("verified unreaped direct-child fallback: pidfd unavailable ({error})");
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
            bail!("pidfd signal failed; no numeric PID fallback after handle acquisition: {err}");
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
fn read_identity(pipe: &mut (impl Read + AsRawFd), deadline: Instant) -> Result<ProcessIdentity> {
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
                    return serde_json::from_slice(&bytes[..end]).context("invalid gate identity");
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
                "cedegrid-drain-{}-{nonce}-{sequence}",
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
        crate::actuation::remove_target_files(&self.path);
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
        guard: Option<crate::namespace::NamespaceGuard>,
        publication: Option<crate::publication::NativePublisherSettings>,
    ) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let capture = |mut reader: Box<dyn Read + Send>, name: &str| -> Result<_> {
            let path = directory.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            let guard = guard.clone();
            let charge = publication
                .as_ref()
                .map(|settings| {
                    crate::publication::CaptureCharge::new(
                        settings,
                        guard.clone().context("capture namespace missing")?,
                        &directory.join(name),
                    )
                })
                .transpose()?;
            Ok(std::thread::Builder::new()
                .name("cedegrid-log-drain".into())
                .spawn(move || -> io::Result<()> {
                    let _guard = guard;
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
                            if let Some(charge) = &charge {
                                charge.reserve(keep as u64).map_err(io::Error::other)?;
                            }
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
        let mut failure = None;
        for reader in self.readers {
            let result = reader
                .join()
                .map_err(|_| anyhow::anyhow!("log drain thread panicked"))
                .and_then(|result| result.map_err(anyhow::Error::from));
            if failure.is_none() {
                failure = result.err();
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
include!("../child_supervision.rs");

/// Apply lifecycle commands and return the runtime-step commands for the caller.
fn apply_control_commands(
    commands: Vec<SupervisorCommand>,
    lifecycle: &mut Lifecycle,
    start: Instant,
    polled_at: u64,
) -> Vec<SupervisorCommand> {
    let mut runtime = Vec::new();
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
            command @ (SupervisorCommand::Adjust { .. }
            | SupervisorCommand::Query
            | SupervisorCommand::Allowance { .. }) => runtime.push(command),
        }
    }
    runtime
}

/// The owned family seen by an actuator: the leader through its owned
/// handle and running mediated children through theirs.
struct OwnedFamily<'a> {
    leader: &'a mut OwnedChild,
    identity: &'a ProcessIdentity,
    children: Option<&'a mut ChildServer>,
    meter: &'a mut crate::actuation::CpuMeter,
}
impl Family for OwnedFamily<'_> {
    fn identities(&self) -> Vec<ProcessIdentity> {
        let mut identities = Vec::new();
        if !self.leader.reaped {
            identities.push(self.identity.clone());
        }
        if let Some(children) = &self.children {
            identities.extend(children.running_identities());
        }
        identities
    }
    fn signal(&mut self, signal: i32) -> Result<()> {
        if !self.leader.reaped {
            self.leader.signal(signal)?;
        }
        if let Some(children) = self.children.as_mut() {
            children.signal_running(signal)?;
        }
        Ok(())
    }
    fn cpu_time(&mut self) -> Option<Duration> {
        let pids: Vec<u32> = self.identities().iter().map(|id| id.pid).collect();
        self.meter.read(&pids)
    }
}

fn record_contention_with_retry(
    journal: &dyn ExecutionJournal,
    record: &ExecutionRecord,
    contention: &crate::contention::AllocationContention,
) -> Result<()> {
    let mut delay = Duration::from_millis(10);
    let mut attempt = 1;
    loop {
        match journal.record_contention(&record.assignment_id, record.generation, contention) {
            Ok(()) => return Ok(()),
            Err(error) if attempt >= JOURNAL_ATTEMPTS => return Err(error),
            Err(_) => {
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                attempt += 1;
            }
        }
    }
}

/// The cap or hint targets applied per resource.
fn applied_targets(
    contention: &crate::contention::AllocationContention,
) -> std::collections::BTreeMap<crate::contention::ResourceKey, u64> {
    contention
        .applied
        .iter()
        .filter(|(_, step)| step.step != crate::contention::YieldStep::None)
        .filter_map(|(key, step)| step.target.map(|target| (key.clone(), target)))
        .collect()
}

/// The actuator's catalog code for each applied resource and for RAM.
fn step_codes(
    actuator: &dyn Actuator,
    contention: &crate::contention::AllocationContention,
) -> std::collections::BTreeMap<crate::contention::ResourceKey, String> {
    contention
        .applied
        .keys()
        .cloned()
        .chain([crate::contention::ResourceKey::Ram])
        .filter_map(|key| {
            actuator
                .step_code(key.kind())
                .map(|code| (key, code.to_owned()))
        })
        .collect()
}

/// Apply `adjust` and answer `query` commands. Unavailable or refused steps are
/// reported, never forced (FR-011); actuation errors mark the step unavailable.
#[allow(clippy::too_many_arguments)]
fn apply_runtime_commands(
    commands: Vec<SupervisorCommand>,
    request: &LaunchRequest,
    record: &ExecutionRecord,
    lifecycle: &mut Lifecycle,
    elapsed: u64,
    actuator: &mut dyn Actuator,
    family: &mut dyn Family,
    contention: &mut crate::contention::AllocationContention,
    targets: &mut crate::actuation::TargetChannel,
    directories: &[PathBuf],
    journal: &dyn ExecutionJournal,
    control: &mut dyn SupervisorControl,
    (hint_ready, observes_drain): (bool, bool),
) {
    use crate::contention::{ResourceKind, StepRequest, YieldCause, YieldStep};
    for command in commands {
        match command {
            SupervisorCommand::Adjust {
                sequence,
                resources,
            } => {
                if sequence <= contention.sequence {
                    continue;
                }
                let mut applied = std::collections::BTreeMap::new();
                let mut unavailable = std::collections::BTreeMap::new();
                let mut details = Vec::new();
                let mut hints_changed = false;
                for (key, mut wanted) in resources {
                    let kind = key.kind();
                    // A Guaranteed CPU cap never goes below the request, nor does
                    // a Guaranteed RAM cap (FR-024).
                    let floor = match kind {
                        ResourceKind::Cpu
                            if matches!(wanted.step, YieldStep::Cap | YieldStep::Hint) =>
                        {
                            Some(request.resources.cpu_millicores)
                        }
                        ResourceKind::Ram if wanted.step == YieldStep::Cap => {
                            Some(request.resources.ram_mib)
                        }
                        _ => None,
                    };
                    if request.class == AllocationClass::Guaranteed
                        && let Some(floor) = floor
                    {
                        wanted.target = wanted.target.map(|target| target.max(floor));
                    }
                    let current = contention
                        .applied
                        .get(&key)
                        .map_or(YieldStep::None, |s| s.step);
                    // Any other step ends this resource's hint.
                    if wanted.step != YieldStep::Hint {
                        hints_changed |= targets.set(key.clone(), None);
                    }
                    if wanted.step >= YieldStep::Drain {
                        // Guaranteed work drains only for a floor or envelope
                        // breach or an operator drain (supervisor-control contract).
                        // A Guaranteed envelope drain is for memory only (FR-024).
                        let permitted = request.class == AllocationClass::Opportunistic
                            || match wanted.cause {
                                YieldCause::FloorBreach | YieldCause::OperatorDrain => true,
                                YieldCause::EnvelopeBreach => kind.is_memory(),
                                _ => false,
                            };
                        if permitted && lifecycle.phase == LifecyclePhase::Running {
                            // The critical floor path skips the drain-file wait
                            // for a worker that does not poll it (FR-063).
                            let step = if wanted.step == YieldStep::Terminate
                                && wanted.cause == YieldCause::FloorBreach
                            {
                                lifecycle.request_terminate(elapsed);
                                YieldStep::Terminate
                            } else {
                                lifecycle.request_drain(elapsed);
                                YieldStep::Drain
                            };
                            applied.insert(key.clone(), step);
                            contention.applied.insert(key, wanted);
                        } else {
                            unavailable
                                .entry(key.clone())
                                .or_insert_with(Vec::new)
                                .push(wanted.step);
                            applied.insert(key, current);
                        }
                        continue;
                    }
                    if lifecycle.phase != LifecyclePhase::Running {
                        details.push(format!(
                            "{key}: drain in progress; runtime steps are released"
                        ));
                        applied.insert(key, current);
                        continue;
                    }
                    if wanted.step == YieldStep::Hint {
                        // Only a worker that acknowledged the channel can take a
                        // hint; the file has no GPU compute target.
                        if kind == ResourceKind::GpuCompute
                            || !crate::actuation::TargetChannel::acknowledged(directories)
                        {
                            unavailable
                                .entry(key.clone())
                                .or_insert_with(Vec::new)
                                .push(YieldStep::Hint);
                            applied.insert(key, current);
                            continue;
                        }
                        // A CPU hint keeps CPU capped at the target while the worker
                        // adapts; a RAM cap already held only stops growth and stays.
                        let runtime = if kind == ResourceKind::Cpu
                            && !actuator.unavailable(kind).contains(&YieldStep::Cap)
                        {
                            YieldStep::Cap
                        } else {
                            YieldStep::None
                        };
                        let held =
                            kind == ResourceKind::Ram && actuator.applied_target(kind).is_some();
                        let result = if held {
                            Ok(YieldStep::Cap)
                        } else {
                            actuator.apply(family, kind, runtime, wanted.target)
                        };
                        if let Err(error) = result {
                            details.push(format!("{key} Hint: {error:#}"));
                            unavailable
                                .entry(key.clone())
                                .or_insert_with(Vec::new)
                                .push(YieldStep::Hint);
                            applied.insert(key, current);
                            continue;
                        }
                        hints_changed |= targets.set_hint(
                            key.clone(),
                            wanted.target.unwrap_or(0),
                            wanted.deadline_ms,
                        );
                        applied.insert(key.clone(), YieldStep::Hint);
                        contention.applied.insert(key, wanted);
                        continue;
                    }
                    if wanted.step != YieldStep::None
                        && actuator.unavailable(kind).contains(&wanted.step)
                    {
                        unavailable
                            .entry(key.clone())
                            .or_insert_with(Vec::new)
                            .push(wanted.step);
                        applied.insert(key, current);
                        continue;
                    }
                    match actuator.apply(family, kind, wanted.step, wanted.target) {
                        Ok(step) => {
                            if let Some(note) = actuator.take_detail() {
                                details.push(format!("{key}: {note}"));
                            }
                            // A cap held above its target only stops growth: the
                            // next step frees the rest.
                            if step == YieldStep::Cap && actuator.step_code(kind).is_some() {
                                unavailable
                                    .entry(key.clone())
                                    .or_insert_with(Vec::new)
                                    .push(YieldStep::Cap);
                            }
                            applied.insert(key.clone(), step);
                            if step == YieldStep::None {
                                contention.applied.remove(&key);
                            } else {
                                contention.applied.insert(
                                    key,
                                    StepRequest {
                                        step,
                                        target: actuator.applied_target(kind).or(wanted.target),
                                        cause: wanted.cause,
                                        deadline_ms: None,
                                    },
                                );
                            }
                        }
                        Err(error) => {
                            details.push(format!("{key} {:?}: {error:#}", wanted.step));
                            unavailable
                                .entry(key.clone())
                                .or_insert_with(Vec::new)
                                .push(wanted.step);
                            applied.insert(key, current);
                        }
                    }
                }
                if hints_changed && let Err(error) = targets.publish(directories) {
                    details.push(format!("resource targets unavailable: {error:#}"));
                }
                contention.sequence = sequence;
                if let Err(error) = record_contention_with_retry(journal, record, contention) {
                    details.push(format!("journal unavailable: {error:#}"));
                }
                let _ = control.report(SupervisorReport::Adjusted {
                    sequence,
                    applied,
                    unavailable,
                    detail: details.join("; "),
                    hint_ready,
                    observes_drain,
                    applied_targets: applied_targets(contention),
                    step_codes: step_codes(actuator, contention),
                });
            }
            SupervisorCommand::Allowance { allowance } => {
                if targets.set_allowance(allowance)
                    && let Err(error) = targets.publish_allowance(directories)
                {
                    eprintln!("resource allowance unavailable: {error:#}");
                }
            }
            SupervisorCommand::Query => {
                // A new agent: no limit stays on own work without a recorded step.
                if let Err(error) = actuator.reconcile(family, &contention.applied) {
                    eprintln!("step reconcile failed: {error:#}");
                }
                let _ = control.report(SupervisorReport::State {
                    applied: contention
                        .applied
                        .iter()
                        .map(|(k, v)| (k.clone(), v.step))
                        .collect(),
                    targets: contention
                        .applied
                        .iter()
                        .filter_map(|(k, v)| v.target.map(|t| (k.clone(), t)))
                        .collect(),
                    lease_sequence: lifecycle.lease_sequence(),
                    sequence: contention.sequence,
                    hint_ready,
                    observes_drain,
                    causes: contention
                        .applied
                        .iter()
                        .map(|(k, v)| (k.clone(), v.cause))
                        .collect(),
                });
            }
            _ => {}
        }
    }
}

/// Bounded attempts for a journal transition after EXEC (research R3).
const JOURNAL_ATTEMPTS: u32 = 4;
/// The running loop's pass while a source it watches has no event to wait on.
const PASS: Duration = Duration::from_millis(5);
/// The longest wait for an event before the running loop passes anyway; every source it
/// watches has an event or a deadline, so this only bounds an unforeseen one.
const IDLE_PASS: Duration = Duration::from_secs(1);

/// Waits until a descriptor turns readable or `until` passes, whichever comes first. An
/// interrupted or failed wait only ends this pass early.
fn wait_for_events(fds: &[std::os::fd::RawFd], until: Instant) {
    let timeout = until.saturating_duration_since(Instant::now());
    if fds.is_empty() {
        std::thread::sleep(timeout);
        return;
    }
    let mut items: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // Rounded up, so the pass never comes before its deadline.
    let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
    // SAFETY: `items` holds `items.len()` valid pollfd entries for the call.
    unsafe { libc::poll(items.as_mut_ptr(), items.len() as libc::nfds_t, ms) };
}

fn persist_with_retry(journal: &dyn ExecutionJournal, record: &ExecutionRecord) -> Result<()> {
    let mut delay = Duration::from_millis(10);
    let mut attempt = 1;
    loop {
        match journal.transition(record) {
            Ok(()) => return Ok(()),
            Err(error) if attempt >= JOURNAL_ATTEMPTS => return Err(error),
            Err(_) => {
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                attempt += 1;
            }
        }
    }
}

/// Best-effort operator diagnostic; a closed stderr never unwinds supervision.
fn report(event: &str, assignment_id: &str, detail: String) {
    let _ = writeln!(
        io::stderr(),
        "{}",
        serde_json::json!({"event": event, "assignment_id": assignment_id, "detail": detail})
    );
}

/// Wait up to `ms` for the owned child to exit. An unobservable child counts
/// as still running so the chain proceeds to its next step.
fn exited_within(owned: &mut OwnedChild, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if owned.try_wait().ok().flatten().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The opportunistic chain after an unrecoverable error once EXEC was sent:
/// drain, TERM and KILL with their deadlines, on the owned child only.
fn drain_chain(
    owned: &mut OwnedChild,
    drain: &DrainDirectory,
    options: &SupervisorOptions,
) -> Result<()> {
    if owned.reaped {
        return Ok(());
    }
    // A failed drain-file write only forgoes cooperative exit; TERM and KILL still follow.
    let _ = drain.notify();
    if exited_within(owned, options.drain_timeout_ms) {
        return Ok(());
    }
    if owned.signal(libc::SIGTERM).is_ok() && exited_within(owned, options.term_grace_ms) {
        return Ok(());
    }
    owned.kill_and_confirm(Duration::from_millis(options.release_confirm_timeout_ms))
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
    let mut exec_sent = false;
    let mut actuator: Option<Box<dyn Actuator>> = None;
    let mut meter = crate::actuation::CpuMeter::default();
    // (hint_ready, observes_drain), reported when either changes (FR-062).
    let mut readiness = (false, false);
    let mut readiness_checked_at: Option<Instant> = None;
    let duty_period = Duration::from_millis(options.duty_period_ms.max(1));
    let mut contention = crate::contention::AllocationContention::default();
    // The RAM cap code last reported to the agent.
    let mut ram_code: Option<&'static str> = None;
    let mut targets = crate::actuation::TargetChannel::new(options.hint_deadline_ms);
    let result = (|| -> Result<ExecutionOutcome> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(options.prepare_timeout_ms))
            .context("preparation timeout is too large")?;
        let mut setup = backend.prepare(request)?;
        if request.class == AllocationClass::Opportunistic && setup.nice.is_none() {
            setup.nice = Some(options.nice);
        }
        // Every own process is a preferred OOM victim where an unprivileged
        // process can mark itself (FR-023).
        if cfg!(target_os = "linux") {
            setup.oom_score_adj = Some(OOM_PREFERRED);
        }
        children = ChildServer::new(request, record.generation, setup.clone(), journal)?;
        if let Some(server) = children.as_mut() {
            server.leader_directory = Some(drain.path.clone());
        }
        let mut gated_request = request.clone();
        if request.env.contains_key("CEDEGRID_OUTPUT_DIR") {
            gated_request
                .env
                .insert("CEDEGRID_CAPTURE_OUTPUT".into(), "1".into());
        }
        if let Some(server) = &children {
            server.expose(&mut gated_request);
        }
        let gate = GateRequest {
            request: gated_request,
            generation: record.generation,
            setup,
            drain_file: drain.file(),
            resource_target_file: Some(drain.path.join(crate::actuation::TARGET_FILE)),
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
            .stderr(if request.env.contains_key("CEDEGRID_OUTPUT_DIR") {
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
        if let Some(cpus) = request.env.get("CEDEGRID_CPU_AFFINITY") {
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
        if let Some(requested) = gate.setup.oom_score_adj {
            record.evidence.push(verify_oom_score_adj(
                actual.pid,
                requested,
                &format!("direct_child:{}", actual.pid),
            )?);
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
        if let Some(directory) = request.env.get("CEDEGRID_OUTPUT_DIR") {
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
                &request
                    .env
                    .get("CEDEGRID_PUBLICATION_SPEC")
                    .map(|raw| {
                        serde_json::from_str::<crate::publication::NativePublisherSettings>(raw)
                            .map(|s| s.output_dir)
                    })
                    .transpose()?
                    .unwrap_or_else(|| PathBuf::from(directory)),
                journal.namespace_guard(),
                request
                    .env
                    .get("CEDEGRID_PUBLICATION_SPEC")
                    .map(|raw| serde_json::from_str(raw))
                    .transpose()?,
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
        exec_sent = true;
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
            "EXEC sent after durable authorization; no claim that exec itself succeeded yet".into();
        // After EXEC no error leads straight to SIGKILL (research R3). A journal
        // that stays unavailable drains opportunistic work through its
        // deadlines; Guaranteed work stays supervised and is reported.
        let journal_unavailable =
            |lifecycle: &mut Lifecycle, elapsed: u64, error: anyhow::Error| {
                report(
                    "journal_unavailable",
                    &request.assignment_id,
                    format!("{error:#}; reservation needs reconciliation"),
                );
                if request.class == AllocationClass::Opportunistic {
                    lifecycle.request_drain(elapsed);
                }
            };
        if let Err(error) = persist_with_retry(journal, &record) {
            let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            journal_unavailable(&mut lifecycle, elapsed, error);
        }
        let actuator = actuator.insert(backend.actuator(&ActuatorSettings {
            class: request.class,
            requested: request.resources.clone(),
            period: Duration::from_millis(options.duty_period_ms),
            ram_noise_tolerance_mib: options.ram_noise_tolerance_mib,
            ram_calm_window: Duration::from_millis(options.ram_calm_window_ms),
        }));
        let leader_identity = record.identity.clone().context("verified gate identity")?;
        let mut drain_started_ms = None;
        let mut drain_notified = false;
        let mut termination_started_ms = None;
        let mut termination_deadline_ms = None;
        let mut term_sent_at: Option<Instant> = None;
        let mut term_unavailable = false;
        let mut control_failing = false;
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
                // Continue any stopped mediated child so it can stop cleanly.
                let mut family = OwnedFamily {
                    leader: &mut *owned,
                    identity: &leader_identity,
                    children: children.as_mut(),
                    meter: &mut meter,
                };
                if let Err(error) = actuator.release(&mut family) {
                    report(
                        "step_release_failed",
                        &request.assignment_id,
                        format!("{error:#}"),
                    );
                }
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
                    termination_deadline_ms,
                    process_exit_ms: leader_exit_ms.unwrap_or(elapsed),
                    release_confirmed_ms: Some(
                        start.elapsed().as_millis().min(u64::MAX as u128) as u64
                    ),
                });
            }
            let polled_at = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            // A control-channel error holds the current step; lease rules still apply.
            let commands = match control.poll() {
                Ok(commands) => {
                    control_failing = false;
                    commands
                }
                Err(error) => {
                    if !control_failing {
                        report(
                            "control_unavailable",
                            &request.assignment_id,
                            format!("{error:#}; holding the current step"),
                        );
                    }
                    control_failing = true;
                    Vec::new()
                }
            };
            let mut runtime = apply_control_commands(commands, &mut lifecycle, start, polled_at);
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
                    runtime.extend(apply_control_commands(
                        commands,
                        &mut lifecycle,
                        start,
                        elapsed,
                    ));
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
            {
                let mut directories = vec![drain.path.clone()];
                if let Some(server) = children.as_ref() {
                    directories.extend(server.target_directories());
                }
                // Readiness only ever turns true, so a ready family is not
                // checked again; others are checked once per duty period.
                let served = children.as_ref().is_some_and(|server| server.served());
                if !readiness.0
                    && readiness_checked_at.is_none_or(|at: Instant| at.elapsed() >= duty_period)
                {
                    readiness_checked_at = Some(Instant::now());
                    let hint_ready = crate::actuation::TargetChannel::acknowledged(&directories);
                    let next = (hint_ready, hint_ready || served);
                    if next != readiness {
                        readiness = next;
                        let _ = control.report(SupervisorReport::Readiness {
                            hint_ready: readiness.0,
                            observes_drain: readiness.1,
                        });
                    }
                } else if served && !readiness.1 {
                    readiness.1 = true;
                    let _ = control.report(SupervisorReport::Readiness {
                        hint_ready: readiness.0,
                        observes_drain: true,
                    });
                }
                let mut family = OwnedFamily {
                    leader: &mut *owned,
                    identity: &leader_identity,
                    children: children.as_mut(),
                    meter: &mut meter,
                };
                if !runtime.is_empty() {
                    apply_runtime_commands(
                        std::mem::take(&mut runtime),
                        request,
                        &record,
                        &mut lifecycle,
                        elapsed,
                        actuator.as_mut(),
                        &mut family,
                        &mut contention,
                        &mut targets,
                        &directories,
                        journal,
                        control,
                        readiness,
                    );
                    ram_code = actuator.step_code(crate::contention::ResourceKind::Ram);
                }
                if lifecycle.phase == LifecyclePhase::Running
                    && leader_status.is_none()
                    && let Err(error) = actuator.tick(&mut family)
                {
                    // A failed actuation marks the step unavailable; it never
                    // escalates by itself (research R3).
                    report(
                        "step_unavailable",
                        &request.assignment_id,
                        format!("{error:#}"),
                    );
                    let _ = actuator.release(&mut family);
                    contention.applied.clear();
                }
                // A RAM cap the actuator raised to current usage is reported at
                // once, so the policy goes on to the hint.
                let code = actuator.step_code(crate::contention::ResourceKind::Ram);
                if code != ram_code {
                    ram_code = code;
                    if let Some(step) = contention
                        .applied
                        .get_mut(&crate::contention::ResourceKey::Ram)
                        && step.step == crate::contention::YieldStep::Cap
                    {
                        step.target = actuator
                            .applied_target(crate::contention::ResourceKind::Ram)
                            .or(step.target);
                    }
                    if let Err(error) = record_contention_with_retry(journal, &record, &contention)
                    {
                        journal_unavailable(&mut lifecycle, elapsed, error);
                    }
                    let unavailable = actuator
                        .unavailable(crate::contention::ResourceKind::Ram)
                        .contains(&crate::contention::YieldStep::Cap)
                        .then(|| {
                            (
                                crate::contention::ResourceKey::Ram,
                                vec![crate::contention::YieldStep::Cap],
                            )
                        })
                        .into_iter()
                        .collect();
                    let _ = control.report(SupervisorReport::Adjusted {
                        sequence: contention.sequence,
                        applied: contention
                            .applied
                            .iter()
                            .map(|(key, step)| (key.clone(), step.step))
                            .collect(),
                        unavailable,
                        detail: actuator.take_detail().unwrap_or_default(),
                        hint_ready: readiness.0,
                        observes_drain: readiness.1,
                        applied_targets: applied_targets(&contention),
                        step_codes: step_codes(actuator.as_ref(), &contention),
                    });
                }
            }
            let phase = lifecycle.tick(elapsed);
            if phase != LifecyclePhase::Running && drain_started_ms.is_none() {
                // Runtime steps are released first so the worker can drain.
                let mut family = OwnedFamily {
                    leader: &mut *owned,
                    identity: &leader_identity,
                    children: children.as_mut(),
                    meter: &mut meter,
                };
                if let Err(error) = actuator.release(&mut family) {
                    report(
                        "step_release_failed",
                        &request.assignment_id,
                        format!("{error:#}"),
                    );
                }
                drain_started_ms = Some(lifecycle.drain_requested_ms().unwrap_or(elapsed));
                record.phase = ExecutionPhase::Draining;
                record.detail =
                    "Agent, cancellation, or authorization deadline requested drain".into();
                if let Err(error) = persist_with_retry(journal, &record) {
                    journal_unavailable(&mut lifecycle, elapsed, error);
                }
            }
            // A failed drain-file write is retried every pass; the drain
            // deadline still escalates to TERM.
            if drain_started_ms.is_some() && !drain_notified {
                match drain.notify() {
                    Ok(()) => drain_notified = true,
                    Err(error) => report(
                        "drain_file_unavailable",
                        &request.assignment_id,
                        format!("{error:#}; retrying"),
                    ),
                }
            }
            if matches!(
                phase,
                LifecyclePhase::Terminating | LifecyclePhase::KillRequested
            ) && termination_started_ms.is_none()
                && !term_unavailable
            {
                termination_deadline_ms = lifecycle.termination_deadline_ms();
                if leader_status.is_none() {
                    match owned.signal(libc::SIGTERM) {
                        Ok(()) => {
                            termination_started_ms = Some(elapsed);
                            term_sent_at = Some(Instant::now());
                        }
                        // An unavailable step is skipped; KILL follows at its deadline.
                        Err(error) => {
                            term_unavailable = true;
                            report(
                                "step_unavailable",
                                &request.assignment_id,
                                format!("SIGTERM: {error:#}"),
                            );
                        }
                    }
                } else {
                    termination_started_ms = Some(elapsed);
                }
            }
            // KILL never follows a SIGTERM actually sent sooner than the TERM grace,
            // even when a stalled pass reached both deadlines together (audit M6).
            let grace_elapsed = term_sent_at
                .is_none_or(|sent| sent.elapsed() >= Duration::from_millis(options.term_grace_ms));
            if phase == LifecyclePhase::KillRequested && !killed && grace_elapsed {
                if leader_status.is_none() {
                    owned.signal(libc::SIGKILL)?;
                }
                killed = true;
                kill_at = Some(Instant::now());
            }
            if let Some(kill_at) = kill_at {
                ensure!(
                    kill_at.elapsed() < Duration::from_millis(options.release_confirm_timeout_ms),
                    "resource release remains unconfirmed after forced termination"
                );
            }
            // Wait for a control line, the leader's exit or a new SDK connection until the
            // next deadline; a source without an event keeps the short pass.
            let mut fds = Vec::new();
            let mut busy = control_failing
                || (drain_started_ms.is_some() && !drain_notified)
                || children.as_ref().is_some_and(|server| !server.idle());
            match control.wake() {
                Some(wake) => fds.push(wake.fd()),
                None => busy = true,
            }
            if leader_status.is_none() {
                match owned.pidfd.as_ref() {
                    Some(pidfd) => fds.push(pidfd.as_raw_fd()),
                    None => busy = true,
                }
            }
            if let Some(server) = children.as_ref().filter(|server| server.idle()) {
                fds.push(server.listener_fd());
            }
            let now = Instant::now();
            let deadlines = [
                lifecycle
                    .next_deadline_ms()
                    .map(|ms| start + Duration::from_millis(ms)),
                actuator.next_deadline(),
                (!readiness.0).then(|| readiness_checked_at.map_or(now, |at| at + duty_period)),
                (phase == LifecyclePhase::KillRequested && !killed).then(|| {
                    term_sent_at.map_or(now, |sent| {
                        sent + Duration::from_millis(options.term_grace_ms)
                    })
                }),
                kill_at.map(|at| at + Duration::from_millis(options.release_confirm_timeout_ms)),
            ];
            let until = deadlines
                .into_iter()
                .flatten()
                .fold(now + if busy { PASS } else { IDLE_PASS }, Instant::min);
            wait_for_events(&fds, until);
            if let Some(wake) = control.wake() {
                wake.drain();
            }
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
            // A failed preparation never sends EXEC: the gate is killed before it
            // runs user code. After EXEC no error kills directly (research R3):
            // opportunistic work goes through drain, TERM and KILL with their
            // deadlines, and running Guaranteed work is never signaled for an
            // error. Only a verified owned child is signaled; uncertain release
            // remains charged.
            // Never leave a family stopped by a runtime step.
            if let (Some(actuator), Some(owned), Some(identity)) =
                (actuator.as_mut(), child.as_mut(), record.identity.as_ref())
            {
                let mut family = OwnedFamily {
                    leader: owned,
                    identity,
                    children: children.as_mut(),
                    meter: &mut meter,
                };
                if let Err(release_error) = actuator.release(&mut family) {
                    report(
                        "step_release_failed",
                        &request.assignment_id,
                        format!("{release_error:#}"),
                    );
                }
            }
            let guaranteed_running = exec_sent
                && request.class == AllocationClass::Guaranteed
                && child.as_ref().is_some_and(|owned| !owned.reaped);
            let child_release = match child.as_mut() {
                Some(_) if guaranteed_running => Err(anyhow::anyhow!(
                    "Guaranteed workload left running after a supervisor error"
                )),
                Some(owned) if exec_sent => drain_chain(owned, &drain, options),
                Some(owned) => owned
                    .kill_and_confirm(Duration::from_millis(options.release_confirm_timeout_ms)),
                None => Ok(()),
            };
            let family_release = if guaranteed_running {
                Ok(())
            } else {
                children.as_mut().map_or(Ok(()), |server| {
                    server.cleanup(journal, options, &gpu_release)
                })
            };
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
    if let Some(adj) = gate.setup.oom_score_adj {
        ensure!(
            adj == OOM_PREFERRED,
            "invalid requested oom_score_adj value"
        );
        #[cfg(target_os = "linux")]
        {
            // Raising only this gate's own value needs no privilege.
            std::fs::write("/proc/self/oom_score_adj", adj.to_string())
                .context("gate oom_score_adj application failed")?;
        }
        #[cfg(not(target_os = "linux"))]
        bail!("oom_score_adj is unavailable on this platform");
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
    if let Some(cpus) = gate.request.env.get("CEDEGRID_CPU_AFFINITY") {
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
        .env_remove("CEDEGRID_PUBLICATION_SPEC")
        .env_remove("CEDEGRID_SUPERVISOR_STATE_ROOT")
        .env_remove("CEDEGRID_SUPERVISOR_NAMESPACE")
        .env_remove("CEDEGRID_LAUNCHER_PID")
        .env("CEDEGRID_ASSIGNMENT_ID", &gate.request.assignment_id)
        .env("CEDEGRID_TASK_ID", &gate.request.task_id)
        .env("CEDEGRID_ATTEMPT_GENERATION", gate.generation.to_string())
        .env("CEDEGRID_DRAIN_FILE", &gate.drain_file)
        .stdin(Stdio::from(File::open("/dev/null")?))
        .stdout(
            if gate
                .request
                .env
                .get("CEDEGRID_CAPTURE_OUTPUT")
                .is_some_and(|v| v == "1")
            {
                Stdio::inherit()
            } else {
                Stdio::from(OpenOptions::new().write(true).open("/dev/null")?)
            },
        )
        .stderr(Stdio::inherit());
    if let Some(path) = &gate.resource_target_file {
        command.env("CEDEGRID_RESOURCE_TARGET_FILE", path);
    }
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
