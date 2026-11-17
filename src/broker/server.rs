//! The broker's listener (FR-002, FR-008, FR-011, T020, T032, T034).
//!
//! One request per connection, bounded in size, time and rate, answered by a fixed set of
//! workers. A synchronous server with no async runtime, so it can be reviewed line by line.

use super::admin_policy::AdminPolicy;
use super::peer::{Accounts, PeerCredentials, authorize, peer_credentials};
use super::request::{MAX_REQUEST_BYTES, Refusal, RefusalCode, Reply, Request, parse_frame};
use super::systemd::{Systemd, start_units, status_units, stop_units};
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
    /// Where units go; its `pam` field is replaced by the policy's `units.pam_session`.
    pub placement: Placement,
    pub rate: Mutex<RateLimiter>,
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
                pam: policy.pam_session.value,
                ..state.placement
            };
            start_units(systemd, caller, &policy, placement)
        }
        Request::Stop => stop_units(systemd, caller),
        Request::Status => status_units(systemd, caller),
        Request::Move { .. } => Err(Refusal::new(
            RefusalCode::MoveRefused,
            "job moves are not supported by this broker version",
        )),
    };
    match result {
        Ok(units) => Handled {
            reply: Reply::units(units),
            kind,
        },
        Err(refusal) => refused(refusal, kind),
    }
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
