//! The broker's listener (FR-002, FR-008, FR-011, T020, T032, T034).
//!
//! One request per connection, bounded in size, time and rate, answered by a fixed set of
//! workers. A synchronous server with no async runtime, so it can be reviewed line by line.

use super::admin_policy::AdminPolicy;
use super::moves::{CgroupFs, move_job};
use super::oom::ProcessAccess;
use super::peer::{Accounts, PeerCredentials, authorize, peer_credentials};
use super::request::{MAX_REQUEST_BYTES, Refusal, RefusalCode, Reply, Request, parse_frame};
use super::systemd::{Systemd, caller_units, start_units, status_units, stop_units};
use super::units::Placement;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// How long a caller has, from accept, to send its request (*provisional*).
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the broker waits to hand a reply over.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Workers answering requests.
pub const WORKERS: usize = 4;

/// Accepted connections waiting for a worker; past this the broker answers `rate_limited`.
pub const QUEUE: usize = 16;

/// Requests one user may send in a burst, and per second after it (*provisional*).
pub const RATE_BURST: u32 = 8;
pub const RATE_PER_SECOND: u32 = 2;

/// A token bucket per user ID (FR-008).
#[derive(Debug)]
pub struct RateLimiter {
    burst: f64,
    per_second: f64,
    buckets: HashMap<u32, (f64, Instant)>,
}

impl RateLimiter {
    pub fn new(burst: u32, per_second: u32) -> Self {
        Self {
            burst: f64::from(burst.max(1)),
            per_second: f64::from(per_second),
            buckets: HashMap::new(),
        }
    }

    /// Takes one token for `uid` at `now`, if one is left.
    pub fn allow(&mut self, uid: u32, now: Instant) -> bool {
        if self.buckets.len() > 4096 {
            let (burst, per_second) = (self.burst, self.per_second);
            self.buckets.retain(|_, (tokens, at)| {
                *tokens + per_second * now.saturating_duration_since(*at).as_secs_f64() < burst
            });
        }
        let (tokens, at) = self.buckets.entry(uid).or_insert((self.burst, now));
        let refill = self.per_second * now.saturating_duration_since(*at).as_secs_f64();
        *tokens = (*tokens + refill).min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Everything the workers share.
pub struct BrokerState {
    pub systemd: Box<dyn Systemd + Send + Sync>,
    pub accounts: Box<dyn Accounts + Send + Sync>,
    /// Loads the administrator policy; called at each request (FR-010).
    pub load_policy: Box<dyn Fn() -> anyhow::Result<AdminPolicy> + Send + Sync>,
    pub host_uid_min: u32,
    /// Whether `user.slice` splits CPU between accounts, for `units.ordinary_slice = "auto"`.
    pub user_slice_splits_cpu: Box<dyn Fn() -> bool + Send + Sync>,
    /// The cgroup files moves read and write.
    pub cgroups: Box<dyn CgroupFs + Send + Sync>,
    /// How moves hold processes.
    pub processes: Box<dyn ProcessAccess + Send + Sync>,
    pub rate: Mutex<RateLimiter>,
    /// One lock per caller, so a caller's requests run one at a time.
    pub callers: Mutex<HashMap<u32, Arc<Mutex<()>>>>,
}

/// One journal line (FR-011). Losing a log line never stops the broker.
pub fn journal(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "cedegrid-broker: {line}");
}

/// Reads one request's bytes: up to the newline, at most `MAX_REQUEST_BYTES + 1`, before
/// `deadline`.
pub fn read_frame(stream: &UnixStream, deadline: Instant) -> Result<Vec<u8>, Refusal> {
    let timeout = || {
        Refusal::new(
            RefusalCode::RequestTimeout,
            "request not received within 2 s of connecting",
        )
    };
    let mut frame = Vec::new();
    let mut chunk = [0u8; 4096];
    while frame.len() <= MAX_REQUEST_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| timeout())?;
        let read = match (&*stream).read(&mut chunk) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(timeout());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(timeout()),
        };
        if read == 0 {
            break;
        }
        frame.extend_from_slice(&chunk[..read]);
        if chunk[..read].contains(&b'\n') {
            break;
        }
    }
    frame.truncate(MAX_REQUEST_BYTES + 1);
    Ok(frame)
}

/// The outcome of one request, for the reply and the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handled {
    pub reply: Reply,
    pub kind: &'static str,
}

/// Answers one request from `peer`. The caller is checked before the request is read.
pub fn handle(state: &BrokerState, peer: PeerCredentials, frame: &[u8]) -> Handled {
    let refused = |refusal: Refusal, kind| Handled {
        reply: Reply::refused(&refusal),
        kind,
    };
    let caller = match authorize(peer, state.accounts.as_ref(), state.host_uid_min) {
        Ok(caller) => caller,
        Err(refusal) => return refused(refusal, "-"),
    };
    let request = match parse_frame(frame) {
        Ok(request) => request,
        Err(refusal) => return refused(refusal, "-"),
    };
    // Two requests from one caller at once would race on the same unit names.
    let caller_lock = Arc::clone(
        state
            .callers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(caller.uid)
            .or_default(),
    );
    let _serialized = caller_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let kind = request.kind();
    let policy = match (state.load_policy)() {
        Ok(policy) => policy,
        Err(error) => {
            journal(&format!("the administrator policy was refused: {error:#}"));
            return refused(
                Refusal::new(
                    RefusalCode::PolicyRefused,
                    "/etc/cedegrid/policy.toml rejected; ask an administrator to check the broker journal",
                ),
                kind,
            );
        }
    };
    if let Some(mode) = request.mode()
        && !policy.allows(mode)
    {
        let allowed: Vec<&str> = policy
            .allowed_modes
            .value
            .iter()
            .map(|mode| mode.name())
            .collect();
        return refused(
            Refusal::new(
                RefusalCode::ModeNotAllowed,
                format!(
                    "mode {} not allowed by policy modes.allowed (allowed: {})",
                    mode.name(),
                    allowed.join(", ")
                ),
            ),
            kind,
        );
    }
    let systemd = state.systemd.as_ref();
    let result = match request {
        Request::Start { .. } => {
            let placement = Placement {
                ordinary_slice: policy
                    .ordinary_slice
                    .value
                    .resolve((state.user_slice_splits_cpu)()),
                pam: policy.pam_session.value,
            };
            start_units(systemd, caller, &policy, placement).and_then(|units| {
                if let Err(refusal) = ensure_idle_slice(state.cgroups.as_ref()) {
                    // Without cpu.idle, Yield First work would compete at a normal weight.
                    let _stopped = systemd.stop(&super::units::unit_name(
                        super::units::UnitKind::Idle,
                        caller.uid,
                    ));
                    return Err(refusal);
                }
                Ok(units)
            })
        }
        Request::Stop => stop_units(systemd, caller),
        Request::Status => status_units(systemd, caller),
        Request::Move { job, mode } => caller_units(systemd, caller).and_then(|units| {
            let moved = move_job(
                state.cgroups.as_ref(),
                state.processes.as_ref(),
                caller,
                &units,
                &job,
                mode,
            )?;
            journal(&format!(
                "move uid={} job={} from={} to={} pids={}",
                caller.uid,
                job.as_str(),
                moved.from,
                moved.to,
                moved.pids.len()
            ));
            status_units(systemd, caller)
        }),
    };
    match result {
        Ok(units) => Handled {
            reply: Reply::units(units),
            kind,
        },
        Err(refusal) => refused(refusal, kind),
    }
}

/// Sets `cpu.idle` on the Yield First slice where systemd could not (research R1) and reads it
/// back (Principle V).
pub fn ensure_idle_slice(cgroups: &dyn CgroupFs) -> Result<(), Refusal> {
    let slice = format!("/{}", super::units::IDLE_SLICE);
    let is_idle = |cgroups: &dyn CgroupFs| {
        cgroups
            .read(&slice, "cpu.idle")
            .is_ok_and(|value| value.trim() == "1")
    };
    if is_idle(cgroups) {
        return Ok(());
    }
    if cgroups.write(&slice, "cpu.idle", "1").is_ok() && is_idle(cgroups) {
        journal(&format!("cpu.idle set on {}", super::units::IDLE_SLICE));
        return Ok(());
    }
    Err(Refusal::new(
        RefusalCode::SystemdFailed,
        "cannot set cpu.idle on cedegrid_idle.slice; ask an administrator to check the broker journal",
    ))
}

fn result_name(reply: &Reply) -> String {
    match reply.code {
        Some(code) => serde_json::to_value(code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "refused".to_owned()),
        None => "ok".to_owned(),
    }
}

fn send(stream: &UnixStream, reply: &Reply) {
    // A caller that stopped reading loses its own reply; the broker carries on.
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = (&*stream).write_all(reply.to_line().as_bytes());
}

fn log_request(peer: PeerCredentials, kind: &str, reply: &Reply, started: Instant) {
    let pid = peer
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_owned());
    journal(&format!(
        "uid={} pid={pid} kind={kind} result={} duration_ms={}",
        peer.uid,
        result_name(reply),
        started.elapsed().as_millis()
    ));
}

/// Answers one accepted connection.
pub fn serve_connection(
    state: &BrokerState,
    stream: &UnixStream,
    peer: PeerCredentials,
    accepted: Instant,
) {
    let handled = match read_frame(stream, accepted + READ_TIMEOUT) {
        Ok(frame) => handle(state, peer, &frame),
        Err(refusal) => Handled {
            reply: Reply::refused(&refusal),
            kind: "-",
        },
    };
    send(stream, &handled.reply);
    log_request(peer, handled.kind, &handled.reply, accepted);
}

/// Serves `listener` until it fails for good. Single accept errors never stop the broker.
pub fn serve(listener: UnixListener, state: Arc<BrokerState>) -> anyhow::Result<()> {
    let (sender, receiver) = mpsc::sync_channel::<(UnixStream, PeerCredentials, Instant)>(QUEUE);
    let receiver = Arc::new(Mutex::new(receiver));
    for index in 0..WORKERS {
        let receiver = Arc::clone(&receiver);
        let state = Arc::clone(&state);
        std::thread::Builder::new()
            .name(format!("broker-worker-{index}"))
            .spawn(move || {
                loop {
                    let next = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv();
                    match next {
                        Ok((stream, peer, accepted)) => {
                            serve_connection(&state, &stream, peer, accepted);
                        }
                        Err(_) => break,
                    }
                }
            })?;
    }
    let busy = || {
        Reply::refused(&Refusal::new(
            RefusalCode::RateLimited,
            "too many requests or broker busy; retry later",
        ))
    };
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                journal(&format!("accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let accepted = Instant::now();
        let peer = match peer_credentials(&stream) {
            Ok(peer) => peer,
            Err(error) => {
                journal(&format!("peer credentials could not be read: {error}"));
                continue;
            }
        };
        let allowed = state
            .rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .allow(peer.uid, accepted);
        if !allowed {
            let reply = busy();
            send(&stream, &reply);
            log_request(peer, "-", &reply, accepted);
            continue;
        }
        match sender.try_send((stream, peer, accepted)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full((stream, peer, accepted))) => {
                let reply = busy();
                send(&stream, &reply);
                log_request(peer, "-", &reply, accepted);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                anyhow::bail!("every broker worker stopped");
            }
        }
    }
    anyhow::bail!("the broker's socket stopped accepting connections")
}

/// The broker's entry point, started by `cedegrid-broker.service` through its socket.
#[cfg(target_os = "linux")]
pub fn run() -> anyhow::Result<()> {
    use super::admin_policy;
    use super::peer::HostAccounts;
    use super::systemd::SystemctlRunner;
    use anyhow::{Context, ensure};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    // SAFETY: geteuid has no preconditions and cannot fail.
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "the broker runs as root from cedegrid-broker.service only"
    );
    // Socket activation passes the listener as descriptor 3 (sd_listen_fds).
    let pid_matches = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        == Some(std::process::id());
    let one_socket = std::env::var("LISTEN_FDS").ok().as_deref() == Some("1");
    ensure!(
        pid_matches && one_socket,
        "no socket from cedegrid-broker.socket; start the broker through its socket unit"
    );
    let gid = cedegrid_gid()?;
    let meta = std::fs::symlink_metadata(super::SOCKET_PATH)
        .with_context(|| format!("cannot read {}", super::SOCKET_PATH))?;
    ensure!(
        meta.file_type().is_socket()
            && meta.uid() == 0
            && meta.gid() == gid
            && meta.mode() & 0o7777 == 0o660,
        "{} must be a socket owned by root, group {}, mode 0660",
        super::SOCKET_PATH,
        super::GROUP
    );
    // SAFETY: descriptor 3 is the listening socket systemd passed, owned from here on.
    let listener = unsafe { UnixListener::from_raw_fd(3) };
    // SAFETY: fcntl on the descriptor just taken; FD_CLOEXEC keeps it from systemd-run children.
    ensure!(
        unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "cannot set close-on-exec on the broker socket"
    );
    let state = Arc::new(BrokerState {
        systemd: Box::new(SystemctlRunner::default()),
        accounts: Box::new(HostAccounts),
        load_policy: Box::new(admin_policy::load),
        host_uid_min: admin_policy::host_uid_min(),
        user_slice_splits_cpu: Box::new(super::units::user_slice_splits_cpu),
        cgroups: Box::new(super::moves::HostCgroupFs {
            root: std::path::PathBuf::from("/sys/fs/cgroup"),
        }),
        processes: Box::new(super::oom::PidfdAccess),
        rate: Mutex::new(RateLimiter::new(RATE_BURST, RATE_PER_SECOND)),
        callers: Mutex::new(HashMap::new()),
    });
    std::thread::Builder::new()
        .name("broker-oom".to_owned())
        .spawn(oom_loop)?;
    journal("started");
    serve(listener, state)
}

/// How often the OOM pass runs while memory is short or a score is held (research R10).
#[cfg(target_os = "linux")]
const OOM_INTERVAL: Duration = Duration::from_secs(1);

/// How often it runs while memory is plentiful and every score is 0 (SC-012).
#[cfg(target_os = "linux")]
const OOM_INTERVAL_AT_REST: Duration = Duration::from_secs(30);

/// The OOM loop: one pass per second over the broker's units (FR-022 to FR-026). The user's
/// decisions of 2026-11-17 fix lowering to N1 B and marking to D2 case A.
#[cfg(target_os = "linux")]
fn oom_loop() {
    use super::admin_policy;
    use super::oom::{OomLoop, PidfdAccess, ScoredUnit, find_units, read_host, unit_pids};
    use super::peer::{Accounts, HostAccounts};
    use super::score::{Lowering, OverShareMarking};
    use super::systemd::SystemctlRunner;
    use super::units::unit_name;

    // SAFETY: sysconf has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_kb = u64::try_from(page_size).map_or(4, |bytes| (bytes / 1024).max(1));
    let cgroup_root = std::path::Path::new("/sys/fs/cgroup");
    let proc_root = std::path::Path::new("/proc");
    let systemd = SystemctlRunner::default();
    let accounts = HostAccounts;
    let mut oom = OomLoop::default();
    // A unit's marker and its user's group, by cgroup path, checked once per cgroup ID.
    let mut markers: HashMap<String, (u64, Option<u32>)> = HashMap::new();
    let mut last_error: Option<Instant> = None;
    let mut report = |message: String| {
        if last_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            journal(&message);
            last_error = Some(Instant::now());
        }
    };
    let mut last_pass: Option<Instant> = None;
    loop {
        let started = Instant::now();
        // At rest, one small read a second; the full scan waits for pressure or the rest interval.
        let plentiful = std::fs::read_to_string(proc_root.join("meminfo"))
            .is_ok_and(|meminfo| super::oom::memory_is_plentiful(&meminfo));
        if plentiful
            && oom.quiet()
            && last_pass.is_some_and(|at| at.elapsed() < OOM_INTERVAL_AT_REST)
        {
            std::thread::sleep(OOM_INTERVAL.saturating_sub(started.elapsed()));
            continue;
        }
        last_pass = Some(started);
        match (admin_policy::load(), read_host(proc_root, page_kb)) {
            (Ok(policy), Ok(host)) => {
                let found = find_units(cgroup_root);
                markers.retain(|cgroup, _| found.iter().any(|unit| &unit.cgroup == cgroup));
                let mut units = Vec::new();
                for unit in found {
                    let known = markers
                        .get(&unit.cgroup)
                        .is_some_and(|(id, _)| *id == unit.id && unit.id != 0);
                    if !known {
                        let ours =
                            systemd
                                .show(&unit_name(unit.kind, unit.uid))
                                .is_ok_and(|show| {
                                    show.is_ours(unit.kind, unit.uid)
                                        && show.control_group == unit.cgroup
                                });
                        let gid = if ours {
                            accounts.primary_gid(unit.uid).ok()
                        } else {
                            None
                        };
                        markers.insert(unit.cgroup.clone(), (unit.id, gid));
                    }
                    if let Some(&(_, Some(gid))) = markers.get(&unit.cgroup) {
                        units.push(ScoredUnit { unit, gid });
                    }
                }
                let writes = oom.pass(
                    Instant::now(),
                    &host,
                    &units,
                    &|scored: &ScoredUnit| unit_pids(cgroup_root, &scored.unit.cgroup),
                    &PidfdAccess,
                    policy.uid_min.value,
                    policy.principal_min_percent.value,
                    Lowering::WhileAnotherOverShare,
                    OverShareMarking::YieldFirstAndNeutral,
                );
                for write in writes {
                    let result = match &write.result {
                        Ok(()) => "ok".to_owned(),
                        Err(error) => format!("failed: {error:#}"),
                    };
                    journal(&format!(
                        "oom_score_adj uid={} pid={} start={} old={} new={} result={result}",
                        write.uid, write.key.pid, write.key.start_time, write.old, write.new
                    ));
                }
            }
            (Err(error), _) => report(format!("OOM pass skipped: policy refused: {error:#}")),
            (_, Err(error)) => report(format!("OOM pass skipped: {error:#}")),
        }
        std::thread::sleep(OOM_INTERVAL.saturating_sub(started.elapsed()));
    }
}

#[cfg(target_os = "linux")]
fn cedegrid_gid() -> anyhow::Result<u32> {
    let name = std::ffi::CString::new(super::GROUP)?;
    let mut buffer = vec![0 as libc::c_char; 64 * 1024];
    // SAFETY: zeroed group is a valid out-parameter for getgrnam_r.
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: valid out-pointers and a buffer of the length passed.
    let rc = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    anyhow::ensure!(
        rc == 0 && !result.is_null(),
        "group {} does not exist",
        super::GROUP
    );
    Ok(entry.gr_gid)
}
