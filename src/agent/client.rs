//! The coordinator client: authenticated requests, paced control traffic and the
//! shared transfer budget.
use super::*;

/// Where the node's requests go: a coordinator over mutual TLS, or a local agent's own
/// in-process coordinator, which needs no network and no certificates (003 R1).
pub(super) enum Transport {
    Remote(RpcClient),
    Local(std::sync::Arc<super::local::LocalCoordinator>),
}

impl Transport {
    pub(super) async fn request(&self, request: &Request) -> Result<Response> {
        match self {
            Transport::Remote(rpc) => rpc.request(request).await,
            Transport::Local(local) => local.node_request(request).await,
        }
    }
}

/// Two bounded lanes reserve control traffic so artifact queues cannot starve
/// authoritative renewals. Their rates sum to the configured per-agent budget.
pub(super) struct NodeClient {
    pub(super) transport: Transport,
    pub(super) replay_session: Option<String>,
    pub(super) lanes: Lanes,
    pub(super) cache_dir: PathBuf,
    pub(super) cache_lock: tokio::sync::Mutex<()>,
    pub(super) spool_root: PathBuf,
    pub(super) max_spool_bytes: u64,
    pub(super) namespace_guard: NamespaceGuard,
    pub(super) storage_profile: crate::state::StorageProfile,
}

impl NodeClient {
    /// `local` is the in-process coordinator of a local agent (003 R1); without it the client
    /// talks to the configured coordinator over mutual TLS.
    pub(super) fn new(
        config: &AgentConfig,
        state_dir: &Path,
        storage_profile: crate::state::StorageProfile,
        namespace_guard: NamespaceGuard,
        local: Option<std::sync::Arc<super::local::LocalCoordinator>>,
    ) -> Result<Self> {
        ensure!(
            config.max_transfer_bytes_per_second >= 10,
            "transfer budget must be at least ten bytes/second"
        );
        let control = config.max_transfer_bytes_per_second / 10;
        let cache_dir = state_dir.join("attempts").join(".input-cache");
        private_dir(&cache_dir)?;
        Ok(Self {
            transport: match local {
                Some(local) => Transport::Local(local),
                None => Transport::Remote(RpcClient::with_settings(
                    &config.coordinator_url,
                    &config.tls,
                    15.0,
                    0,
                )?),
            },
            replay_session: None,
            lanes: Lanes {
                control: RateLimiter::new(control),
                artifacts: RateLimiter::new(config.max_transfer_bytes_per_second - control),
                cap: Duration::from_secs(2),
            },
            cache_dir,
            cache_lock: tokio::sync::Mutex::new(()),
            spool_root: state_dir.join("attempts"),
            max_spool_bytes: config.max_spool_bytes,
            namespace_guard,
            storage_profile,
        })
    }
    pub(super) fn download_charge(&self, path: &Path) -> Result<crate::publication::CaptureCharge> {
        crate::publication::CaptureCharge::for_download(
            self.spool_root.parent().context("spool parent missing")?,
            self.storage_profile,
            self.namespace_guard.clone(),
            path,
            self.max_spool_bytes,
        )
    }
    pub(super) fn write_json_new(&self, path: &Path, value: &impl Serialize) -> Result<()> {
        let charge = self.download_charge(path)?;
        // Receipt/control bytes share the node ledger with artifacts and logs.
        // After reservation, any write/sync uncertainty retains the full charge.
        write_json_new_reserved(path, value, Some(&charge))
    }
    /// Writes new files of encoded JSON lines after reserving all their bytes at once: one
    /// ledger open and one commit for a launch's files instead of one of each per file (006
    /// SC-001). As with one file, the reservation comes first and is kept on any failure.
    pub(super) fn write_lines_new(&self, files: &[(&Path, Vec<u8>)]) -> Result<()> {
        let first = files.first().context("no files to write")?.0;
        let charge = self.download_charge(first)?;
        let entries = files
            .iter()
            .map(|(path, bytes)| Ok((*path, u64::try_from(bytes.len())?)))
            .collect::<Result<Vec<_>>>()?;
        charge.reserve_paths(&entries)?;
        for (path, bytes) in files {
            write_new_synced(path, bytes)?;
        }
        Ok(())
    }
    pub(super) async fn request(&self, request: &Request) -> Result<Response> {
        tokio::time::timeout(Duration::from_secs(15), self.request_paced(request))
            .await
            .context("node RPC exceeded its end-to-end queue/pacing/transport deadline")?
    }
    pub(super) async fn request_control(
        &self,
        request: &Request,
        due: Instant,
        interval: Duration,
    ) -> Result<Response> {
        let started = Instant::now();
        let evidence =
            std::env::var_os("CEDEGRID_RPC_EVIDENCE").as_deref() == Some(std::ffi::OsStr::new("1"));
        // The qualification mode supplies an explicit scheduled 10-second
        // control deadline while preserving the normal transport and queues.
        let result = if evidence {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(due + Duration::from_secs(10)),
                self.request(request),
            )
            .await
            .context("scheduled control RPC deadline exceeded")
            .and_then(|r| r)
        } else {
            self.request(request).await
        };
        if evidence {
            let operation = match request {
                Request::Heartbeat { .. } => "heartbeat",
                Request::Renew { .. } => "renew",
                _ => "other",
            };
            let assignment = match request {
                Request::Renew { assignment_id, .. } => Some(assignment_id.as_str()),
                _ => None,
            };
            let success = result
                .as_ref()
                .is_ok_and(|r| !matches!(r, Response::Error { .. }));
            control_evidence(operation, assignment, due, started, interval, success)?;
        }
        result
    }
    /// One lease batch on the control lane (FR-074). Under `CEDEGRID_RPC_EVIDENCE=1` each
    /// renewal reports its `control_rpc` evidence as a single renewal would, so the counts do
    /// not change.
    pub(super) async fn request_leases(
        &self,
        items: &[LeaseItem],
        dues: &BTreeMap<String, Instant>,
        interval: Duration,
    ) -> Result<Response> {
        let started = Instant::now();
        let evidence =
            std::env::var_os("CEDEGRID_RPC_EVIDENCE").as_deref() == Some(std::ffi::OsStr::new("1"));
        let request = Request::Leases {
            items: items.to_vec(),
        };
        let result = match dues.values().min().filter(|_| evidence) {
            Some(due) => tokio::time::timeout_at(
                tokio::time::Instant::from_std(*due + Duration::from_secs(10)),
                self.request(&request),
            )
            .await
            .context("scheduled control RPC deadline exceeded")
            .and_then(|r| r),
            None => self.request(&request).await,
        };
        if evidence {
            for (index, item) in items.iter().enumerate() {
                if let LeaseItem::Renew { assignment_id, .. } = item
                    && let Some(due) = dues.get(assignment_id)
                {
                    let success = matches!(
                        &result,
                        Ok(Response::Leases { results })
                            if matches!(results.get(index), Some(LeaseResult::Lease { .. }))
                    );
                    control_evidence(
                        "renew",
                        Some(assignment_id),
                        *due,
                        started,
                        interval,
                        success,
                    )?;
                }
            }
        }
        result
    }
    pub(super) async fn request_paced(&self, request: &Request) -> Result<Response> {
        let artifact = matches!(
            request,
            Request::BeginUpload { .. }
                | Request::UploadChunk { .. }
                | Request::CommitUpload { .. }
                | Request::ReadArtifact { .. }
                | Request::PublishCheckpoint { .. }
                | Request::Complete { .. }
        );
        let wrapped;
        let wire_request = if let Some(session_id) = &self.replay_session {
            wrapped = Request::NodeSession {
                session_id: session_id.clone(),
                request: Box::new(request.clone()),
            };
            &wrapped
        } else {
            request
        };
        let upload = serde_json::to_vec(wire_request)?.len() as u64;
        let estimated_reply = if let Request::ReadArtifact { max_bytes, .. } = request {
            u64::from(*max_bytes) * 2
        } else {
            0
        };
        let wait = self.lanes.wait_at(
            artifact,
            wire_charge(upload.saturating_add(estimated_reply)),
            tokio::time::Instant::now(),
        );
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let response = self.transport.request(wire_request).await?;
        if estimated_reply == 0 {
            let wait = self.lanes.wait_at(
                artifact,
                wire_charge(serde_json::to_vec(&response)?.len() as u64),
                tokio::time::Instant::now(),
            );
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        Ok(response)
    }
}

/// One control request's schedule evidence for the release gates.
fn control_evidence(
    operation: &str,
    assignment: Option<&str>,
    due: Instant,
    started: Instant,
    interval: Duration,
    success: bool,
) -> Result<()> {
    let lateness = started.saturating_duration_since(due);
    let finished = Instant::now();
    emit(
        &serde_json::json!({"event":"control_rpc","operation":operation,"assignment_id":assignment,"scheduled_due_monotonic_ms":local_clock_ms()?.saturating_sub(finished.saturating_duration_since(due).as_millis() as u64),"enqueued_late_ms":lateness.as_secs_f64()*1000.0,"elapsed_ms":finished.saturating_duration_since(due).as_secs_f64()*1000.0,"request_elapsed_ms":finished.duration_since(started).as_secs_f64()*1000.0,"missed_schedule_slots":lateness.as_millis()/interval.as_millis().max(1),"success":success,"deadline_ms":10000}),
    )
}

pub(super) fn next_control_due(last: &mut Instant, interval: Duration) -> Instant {
    next_control_due_at(last, interval, Instant::now())
}

pub(super) fn next_control_due_at(last: &mut Instant, interval: Duration, now: Instant) -> Instant {
    let due = *last + interval;
    let missed = now.saturating_duration_since(due).as_millis() / interval.as_millis().max(1);
    *last = due + interval.saturating_mul(missed.min(u32::MAX as u128) as u32);
    due
}

pub(super) fn next_periodic_wake(due: Instant, interval: Duration, now: Instant) -> Instant {
    if now < due {
        return due;
    }
    let elapsed = now.saturating_duration_since(due).as_nanos();
    let intervals = elapsed / interval.as_nanos().max(1) + 1;
    due + interval.saturating_mul(intervals.min(u32::MAX as u128) as u32)
}

pub(super) fn wire_charge(json_bytes: u64) -> u64 {
    json_bytes
        .saturating_add(json_bytes / 20)
        .saturating_add(4096)
}

/// The agent's two transfer lanes: control requests and artifact transfer.
pub(super) struct Lanes {
    pub(super) control: RateLimiter,
    pub(super) artifacts: RateLimiter,
    /// The most the control lane keeps owed: one heartbeat interval.
    pub(super) cap: Duration,
}

impl Lanes {
    /// How long a request waits for its lane at `now`, having booked `bytes` on it.
    pub(super) fn wait_at(
        &self,
        artifact: bool,
        bytes: u64,
        now: tokio::time::Instant,
    ) -> Duration {
        if artifact {
            self.artifacts.book_at(bytes, now)
        } else {
            // Control requests never wait for their lane: the loop's cadence must not depend on
            // the transfer budget (FR-034). They are still charged, so artifacts cannot starve them.
            self.control.charge_at(bytes, now, self.cap);
            Duration::ZERO
        }
    }
}

pub(super) struct RateLimiter {
    pub(super) rate: u64,
    next: std::sync::Mutex<tokio::time::Instant>,
}

impl RateLimiter {
    pub(super) fn new(rate: u64) -> Self {
        Self {
            rate,
            next: std::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }
    fn span(&self, bytes: u64) -> Duration {
        Duration::from_secs_f64(bytes as f64 / self.rate.max(1) as f64)
    }
    /// Books `bytes` from `now` and returns how far away their turn is.
    pub(super) fn book_at(&self, bytes: u64, now: tokio::time::Instant) -> Duration {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let start = (*next).max(now);
        *next = start + self.span(bytes);
        start.saturating_duration_since(now)
    }
    /// Books `bytes` from `now` with no wait, keeping at most `cap` owed.
    pub(super) fn charge_at(&self, bytes: u64, now: tokio::time::Instant, cap: Duration) {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        *next = ((*next).max(now) + self.span(bytes)).min(now + cap);
    }
    /// How far the lane's booked bytes reach past `now`.
    pub(super) fn behind_at(&self, now: tokio::time::Instant) -> Duration {
        self.next
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .saturating_duration_since(now)
    }
}

/// How long one wait for work is held: within the client's 15 s deadline (006 SC-001).
pub(super) const AWAIT_WORK_MS: u64 = 10_000;
/// How often the wait loop looks again while it may not wait.
const AWAIT_IDLE: Duration = Duration::from_millis(250);
/// The first retry after a failed wait, doubled up to `AWAIT_RETRY_MAX`.
const AWAIT_RETRY_MIN: Duration = Duration::from_millis(500);
const AWAIT_RETRY_MAX: Duration = Duration::from_secs(30);
/// After an unchanged answer that was not held (the coordinator's waiter cap), the next wait:
/// the default heartbeat interval, which then finds the work anyway.
const AWAIT_UNHELD: Duration = Duration::from_secs(2);

/// What the agent loop and its wait for work share (006 SC-001).
#[derive(Debug, Default)]
pub(super) struct WorkWatch {
    /// The coordinator holds waits (`await_work/1`).
    pub(super) enabled: std::sync::atomic::AtomicBool,
    /// The node has room for more work and is not stopping.
    pub(super) wanted: std::sync::atomic::AtomicBool,
    /// New work was queued since the loop last looked: its next heartbeat comes early.
    pub(super) arrived: std::sync::atomic::AtomicBool,
    /// Wakes the loop when `arrived` is set, so the early heartbeat does not wait for the next
    /// regular interval.
    pub(super) arrival: tokio::sync::Notify,
}

/// Waits for new work at the coordinator while the loop wants it, so a node with room hears
/// of a submission at once instead of at its next heartbeat (006 SC-001). It only marks the
/// arrival and wakes the loop; the loop's heartbeat takes the work, under the usual checks.
pub(super) async fn await_work<F, Fut>(watch: std::sync::Arc<WorkWatch>, mut ask: F)
where
    F: FnMut(Request) -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    use std::sync::atomic::Ordering;
    let mut seen: Option<u64> = None;
    let mut retry = AWAIT_RETRY_MIN;
    loop {
        if !(watch.enabled.load(Ordering::SeqCst) && watch.wanted.load(Ordering::SeqCst)) {
            tokio::time::sleep(AWAIT_IDLE).await;
            continue;
        }
        let asked = Instant::now();
        let request = Request::AwaitWork {
            // Nothing seen yet: answered at once, which sets the baseline.
            seen: seen.unwrap_or(u64::MAX),
            wait_ms: AWAIT_WORK_MS,
        };
        match ask(request).await {
            Ok(Response::Work { generation }) => {
                retry = AWAIT_RETRY_MIN;
                let unchanged = seen == Some(generation);
                if seen.is_some() && !unchanged {
                    watch.arrived.store(true, Ordering::SeqCst);
                    watch.arrival.notify_one();
                    crate::supervision::launch_trace("work_arrived", "-");
                }
                seen = Some(generation);
                // An unchanged answer that came at once was not held: ask again later.
                if unchanged && asked.elapsed() < AWAIT_RETRY_MIN {
                    tokio::time::sleep(AWAIT_UNHELD).await;
                }
            }
            _ => {
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(AWAIT_RETRY_MAX);
            }
        }
    }
}
