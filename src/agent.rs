//! Rootless node service and independent per-assignment supervisor process.
//! Transport is authenticated by RpcClient; child control pipes are inherited,
//! never reopened by PID/name. Supervisor loss leaves the durable reservation.
use crate::{
    config::{Config, NodeMode},
    execution_model::*,
    model::{PolicyInput, Resources},
    namespace::{Namespace, NamespaceGuard, NamespaceIdentity, NamespaceOwner},
    policy::{PolicyEngine, schedulable_budget, validate_gpu_launch_contract},
    protocol::*,
    state::StateStore,
    supervision::{
        self, ExecutionOutcome, SupervisorCommand, SupervisorControl, SupervisorOptions,
    },
    telemetry::ManagedCollector,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

mod client;
mod launch;
mod publishing;
mod reconcile;
mod replay;
mod report;
mod service;
mod supervisor;
use client::*;
use launch::*;
pub(crate) use publishing::verify_publication_receipt;
use publishing::*;
pub(crate) use reconcile::identity_absent;
pub use reconcile::reconcile;
pub(crate) use reconcile::recovery_allows_release;
use reconcile::*;
use replay::*;
pub use report::advertised_controls;
pub use report::node_contention_report;
use report::*;
pub use service::envelope_exceeded;
pub use service::restrict_gpu_scope;
pub use service::run;
pub use service::{LAUNCHES_PER_HEARTBEAT, LaunchPacing, launch_slots};
pub use supervisor::assignment_supervisor;
use supervisor::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub coordinator_url: String,
    pub tls: TlsIdentity,
    /// Optional exact Linux CPU-ID placement, selected from verified topology.
    #[serde(default)]
    pub cpu_affinity: Option<Vec<u32>>,
    /// Aggregate artifact wire-byte budget, including hexadecimal expansion.
    #[serde(default = "default_transfer_rate")]
    pub max_transfer_bytes_per_second: u64,
    /// Additional operator-approved ceiling; never inferred from idle telemetry.
    pub capacity: Resources,
    #[serde(default = "default_workers")]
    pub max_workers: usize,
    #[serde(default = "default_spool")]
    pub max_spool_bytes: u64,
    /// Optional finite service envelope. Zero runs until explicit stop/drain.
    #[serde(default)]
    pub max_runtime_seconds: u64,
}
fn default_transfer_rate() -> u64 {
    10 * 1024 * 1024
}
fn default_workers() -> usize {
    4
}
fn default_spool() -> u64 {
    2 * 1024 * 1024 * 1024
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorSpec {
    pub config: Config,
    pub assignment: Assignment,
    pub capacity: Resources,
    #[serde(default)]
    pub envelope: Option<Resources>,
    pub output_dir: PathBuf,
    /// Token for the re-attachable control socket; older specs have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Prepared {
        record: ExecutionRecord,
    },
    Finished {
        outcome: ExecutionOutcome,
    },
    Failed {
        detail: String,
    },
    Adjusted {
        sequence: u64,
        applied: BTreeMap<crate::contention::ResourceKey, crate::contention::YieldStep>,
        unavailable: BTreeMap<crate::contention::ResourceKey, Vec<crate::contention::YieldStep>>,
        detail: String,
        /// Readiness (FR-062); an older supervisor reports none, which is not ready.
        #[serde(default)]
        hint_ready: bool,
        #[serde(default)]
        observes_drain: bool,
        /// The targets applied; an older supervisor reports none.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        applied_targets: BTreeMap<crate::contention::ResourceKey, u64>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        step_codes: BTreeMap<crate::contention::ResourceKey, String>,
    },
    State {
        applied: BTreeMap<crate::contention::ResourceKey, crate::contention::YieldStep>,
        targets: BTreeMap<crate::contention::ResourceKey, u64>,
        /// The last coordinator lease sequence, so a reattached agent can renew.
        lease_sequence: u64,
        /// The last applied `adjust` sequence; a reattached agent continues above it.
        #[serde(default)]
        sequence: u64,
        /// "connected" or "lost"
        authority: String,
        #[serde(default)]
        hint_ready: bool,
        #[serde(default)]
        observes_drain: bool,
        /// Each applied step's cause; an older supervisor reports none.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        causes: BTreeMap<crate::contention::ResourceKey, crate::contention::YieldCause>,
    },
    Readiness {
        hint_ready: bool,
        observes_drain: bool,
    },
    /// The family's OOM preference changed (FR-023).
    Oom {
        preferred: bool,
        changes: Vec<crate::supervision::OomChange>,
        #[serde(default)]
        detail: String,
    },
}
/// One line from the agent. A line that does not parse is reported and ignored;
/// only EOF or a read error is agent loss (supervisor-control contract).
enum Incoming {
    Control(Control),
    Unknown(String),
    /// The inherited pipe closed.
    Lost,
    /// A controller authenticated on the control socket, or left it.
    Attached,
    Detached,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Control {
    Authorize {
        lease: Lease,
        expires_monotonic_ms: u64,
    },
    Renewal {
        lease: Lease,
        expires_monotonic_ms: u64,
    },
    Drain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<crate::contention::YieldCause>,
    },
    /// One step per resource, from the agent's policy authority.
    Adjust {
        sequence: u64,
        resources: BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>,
    },
    /// Ask for the applied steps.
    Query,
    /// What the allocation may grow to now, for its resource-target file (FR-071).
    Allowance {
        allowance: crate::model::ResourceAmounts,
    },
    /// Make the family preferred OOM victims, or return it to its baseline (FR-023).
    Oom { preferred: bool },
    /// The agent's latest GPU observation. Supervisors run no collector of
    /// their own; GPU launch rechecks use this (research R1).
    Observation {
        snapshot: Box<crate::model::Snapshot>,
    },
}
fn emit(value: &impl Serialize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "private directory is a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            fs::metadata(path)?.uid() == unsafe { libc::geteuid() },
            "private directory not owned by runtime user"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    write_json_new_reserved(path, value, None)
}
fn write_json_new_reserved(
    path: &Path,
    value: &impl Serialize,
    charge: Option<&crate::publication::CaptureCharge>,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if let Some(charge) = charge {
        charge.reserve(bytes.len().try_into()?)?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}
struct AssignedJournal {
    store: StateStore,
    generation: u64,
}
impl ExecutionJournal for AssignedJournal {
    fn namespace_guard(&self) -> Option<NamespaceGuard> {
        Some(self.store.namespace_guard())
    }
    fn storage_control_evidence(&self) -> Result<Vec<ControlEvidence>> {
        self.store.storage_control_evidence()
    }
    fn reserve(&self, request: &LaunchRequest, capacity: &Resources) -> Result<ExecutionRecord> {
        self.store
            .reserve_assigned(request, capacity, self.generation)
    }
    fn record_contention(
        &self,
        assignment_id: &str,
        generation: u64,
        contention: &crate::contention::AllocationContention,
    ) -> Result<()> {
        self.store
            .record_contention(assignment_id, generation, contention)
    }
    fn transition(&self, record: &ExecutionRecord) -> Result<()> {
        self.store.transition(record)
    }
    fn reserve_managed_child(
        &self,
        assignment_id: &str,
        generation: u64,
        child_id: &str,
        request_id: &str,
        request: &LaunchRequest,
    ) -> Result<crate::managed_children::ManagedChildRecord> {
        self.store
            .reserve_managed_child(assignment_id, generation, child_id, request_id, request)
    }
    fn managed_child(
        &self,
        assignment_id: &str,
        request_id: &str,
    ) -> Result<Option<crate::managed_children::ManagedChildRecord>> {
        self.store.managed_child(assignment_id, request_id)
    }
    fn managed_children(
        &self,
        assignment_id: &str,
    ) -> Result<Vec<crate::managed_children::ManagedChildRecord>> {
        self.store.managed_children(assignment_id)
    }
    fn prepare_managed_child(
        &self,
        child_id: &str,
        identity: &ProcessIdentity,
        evidence: &[ControlEvidence],
    ) -> Result<()> {
        self.store
            .prepare_managed_child(child_id, identity, evidence)
    }
    fn transition_managed_child(
        &self,
        child_id: &str,
        phase: crate::managed_children::ManagedChildPhase,
        exit_code: Option<i32>,
        signal: Option<i32>,
        detail: &str,
    ) -> Result<()> {
        self.store
            .transition_managed_child(child_id, phase, exit_code, signal, detail)
    }
}

#[cfg(test)]
fn open_existing_node_store(config: &Config) -> Result<StateStore> {
    let guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    open_existing_node_store_guarded(config, guard)
}
fn open_existing_node_store_guarded(config: &Config, guard: NamespaceGuard) -> Result<StateStore> {
    if config.storage_profile.is_replayable() {
        ensure!(
            fs::symlink_metadata(config.state_dir.join(crate::state::DATABASE_FILENAME))
                .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink()),
            "replayable journal missing or substituted; restart authenticated recovery before opening node state"
        );
    }
    if config.storage_profile.is_replayable() {
        StateStore::open_existing_with_profile_guarded(
            &config.state_dir,
            config.storage_profile,
            guard,
        )
    } else {
        StateStore::open_with_profile_guarded(&config.state_dir, config.storage_profile, guard)
    }
}

/// Metadata-only checks avoid opening/closing an extra SQLite inode descriptor,
/// which could discard process-wide POSIX locks held by SQLite on Unix.
struct ReplayJournalGuard {
    #[cfg(unix)]
    state_identity: (u64, u64),
    #[cfg(unix)]
    database_identity: (u64, u64),
}
impl ReplayJournalGuard {
    fn capture(config: &Config) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let state = fs::symlink_metadata(&config.state_dir)?;
            let database =
                fs::symlink_metadata(config.state_dir.join(crate::state::DATABASE_FILENAME))?;
            ensure!(
                state.is_dir()
                    && database.is_file()
                    && state.uid() == unsafe { libc::geteuid() }
                    && database.uid() == unsafe { libc::geteuid() }
                    && database.nlink() == 1,
                "replayable journal namespace or ownership invalid"
            );
            Ok(Self {
                state_identity: (state.dev(), state.ino()),
                database_identity: (database.dev(), database.ino()),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = config;
            bail!("replayable journal namespace verification unavailable on this platform")
        }
    }
    fn verify(&self, config: &Config) -> Result<()> {
        let current = Self::capture(config)?;
        #[cfg(unix)]
        ensure!(
            current.state_identity == self.state_identity
                && current.database_identity == self.database_identity,
            "replayable journal namespace changed; stop and restart authenticated recovery"
        );
        #[cfg(not(unix))]
        let _ = current;
        Ok(())
    }
}

pub fn execution_backend(
    config: &Config,
    request: &LaunchRequest,
) -> Result<Box<dyn LaunchBackend>> {
    if config.cgroup.enabled {
        match crate::cgroup::CgroupBackend::new(config.cgroup.clone()) {
            Ok(backend) => return Ok(Box::new(backend)),
            Err(error) if request.allow_fallback => {
                ensure!(
                    request.required_controls.iter().all(|c| ![
                        "cpu.weight",
                        "cpu.max",
                        "memory.high",
                        "memory.max",
                        "cgroup.kill"
                    ]
                    .contains(&c.as_str())
                        && !c.starts_with("cgroup.")),
                    "required delegated control unavailable: {error:#}"
                );
                let mut backend = crate::rootless::RootlessBackend::new(config.effective_nice());
                backend.fallback_evidence.push(ControlEvidence {
                    control: "cgroup_backend".into(),
                    available: None,
                    permitted: None,
                    configured: true,
                    applied: false,
                    fallback: true,
                    scope: "rootless".into(),
                    requested: Some("cgroup_v2".into()),
                    effective: Some("rootless".into()),
                    detail: format!("explicit fallback: {error:#}"),
                });
                return Ok(Box::new(backend));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Box::new(crate::rootless::RootlessBackend::new(
        config.effective_nice(),
    )))
}
/// The supervisor behind a slot.
enum SupervisorProcess {
    /// Spawned by this agent: its direct child.
    Owned(Child),
    /// Reattached after an agent restart; exit is seen through its identity.
    Attached(ProcessIdentity),
}
impl SupervisorProcess {
    /// The exit status when this agent reaped it, or a description once gone.
    fn exited(&mut self) -> Result<Option<String>> {
        match self {
            SupervisorProcess::Owned(child) => Ok(child.try_wait()?.map(|s| s.to_string())),
            SupervisorProcess::Attached(identity) => {
                Ok(identity_absent(identity).then(|| "after reattachment".to_string()))
            }
        }
    }
}
struct ChildSlot {
    child: SupervisorProcess,
    /// None once closed; closing it without Drain is agent loss, never a drain.
    input: Option<Box<dyn Write + Send>>,
    events: mpsc::Receiver<std::result::Result<Event, String>>,
    assignment: Assignment,
    output_dir: PathBuf,
    sequence: u64,
    last_renew: Instant,
    prepared: Option<ExecutionRecord>,
    reported: bool,
    checkpoint_hash: Option<String>,
    exited: bool,
    capture_threads: Vec<std::thread::JoinHandle<()>>,
    /// Steps the supervisor reported unavailable, per resource.
    unavailable: BTreeMap<crate::contention::ResourceKey, BTreeSet<crate::contention::YieldStep>>,
    /// Steps the supervisor reported applied, and the last acknowledged sequence.
    applied: BTreeMap<crate::contention::ResourceKey, crate::contention::YieldStep>,
    /// The targets the supervisor reported applied.
    applied_targets: BTreeMap<crate::contention::ResourceKey, u64>,
    /// Catalog codes for steps the supervisor holds above target or found unavailable.
    step_codes: BTreeMap<crate::contention::ResourceKey, String>,
    applied_sequence: u64,
    /// Readiness as the supervisor last reported it (FR-062).
    hint_ready: bool,
    observes_drain: bool,
    /// When the first Drain was sent, by local policy or for the coordinator.
    drain_sent_at: Option<Instant>,
    /// Why it was drained, reported to the coordinator once negotiated.
    drain_cause: Option<crate::contention::YieldCause>,
    started_at: Instant,
    last_checkpoint_at: Option<Instant>,
    /// The last `adjust` sent and its sequence.
    adjust_sequence: u64,
    sent_steps: BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>,
    /// The allowance last sent to the supervisor (FR-071).
    sent_allowance: Option<crate::model::ResourceAmounts>,
    /// The OOM preference last sent, unknown for a reattached supervisor (FR-023).
    sent_oom: Option<bool>,
    /// Why it was sent, with own RAM and its fair share, for the evidence line.
    oom_context: Option<serde_json::Value>,
    /// Steps learned from a reattached supervisor, to seed the policy authority.
    seed: Option<BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>>,
    /// What the slot pass last read from the journal for this slot.
    journal_seen: Option<JournalSeen>,
}

/// What the slot pass last read from the journal for a slot, at which change key; while the
/// key holds and the slot delivered nothing new, the reads are not repeated.
#[derive(Clone)]
struct JournalSeen {
    key: (i64, i64),
    record: Option<ExecutionRecord>,
    /// The checkpoint head's hash, once read: `None` inside when it could not be read.
    head: Option<Option<Option<String>>>,
}

impl ChildSlot {
    /// Readiness only turns true: an ack or a served request cannot be undone.
    fn note_readiness(&mut self, hint_ready: bool, observes_drain: bool) {
        self.hint_ready |= hint_ready;
        self.observes_drain |= observes_drain;
    }

    /// What observation cannot show the policy about this allocation.
    fn facts(&self) -> crate::model::AllocationFacts {
        let since = self.last_checkpoint_at.unwrap_or(self.started_at);
        crate::model::AllocationFacts {
            // A prepared reservation not yet authorized may still be cancelled (FR-012).
            uncontrollable: self.exited || (self.sequence == 0 && self.prepared.is_none()),
            progress_lost_ms: since.elapsed().as_millis() as u64,
            unavailable: self.unavailable.clone(),
            hint_ready: self.hint_ready,
            launch_nice: self.prepared.as_ref().and_then(|record| {
                record
                    .evidence
                    .iter()
                    .find(|evidence| evidence.control == "cpu.nice" && evidence.applied)
                    .and_then(|evidence| evidence.effective.as_deref()?.parse().ok())
            }),
            observes_drain: self.observes_drain,
            applied: self.applied.clone(),
            applied_targets: self.applied_targets.clone(),
            ram_cap: false,
            step_codes: self.step_codes.clone(),
        }
    }
}
impl Drop for ChildSlot {
    fn drop(&mut self) {
        // Closing the pipe is agent loss: the supervisor holds its steps and the
        // lease rules apply. Only a deliberate stop sends Drain (audit M5).
        drop(self.input.take());
        // Never block on a supervisor that outlives this agent; join output work
        // only once it has finished.
        if matches!(self.child.exited(), Ok(Some(_))) {
            for thread in self.capture_threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
}
fn send(slot: &mut ChildSlot, message: &Control) -> Result<()> {
    let input = slot
        .input
        .as_mut()
        .context("supervisor control pipe closed")?;
    serde_json::to_writer(&mut *input, message)?;
    input.write_all(b"\n")?;
    input.flush()?;
    Ok(())
}
/// Send Drain and record it for the admission back-off.
fn drain(slot: &mut ChildSlot, cause: Option<crate::contention::YieldCause>) {
    slot.drain_sent_at.get_or_insert_with(Instant::now);
    if slot.drain_cause.is_none() {
        slot.drain_cause = cause;
    }
    // A closed pipe already means agent loss for this supervisor.
    let _ = send(slot, &Control::Drain { cause });
}
/// Ask for TERM without the drain-file wait (FR-063); a supervisor that does not
/// know the step drains instead.
fn terminate(
    slot: &mut ChildSlot,
    key: crate::contention::ResourceKey,
    cause: Option<crate::contention::YieldCause>,
) {
    slot.drain_sent_at.get_or_insert_with(Instant::now);
    if slot.drain_cause.is_none() {
        slot.drain_cause = cause;
    }
    slot.adjust_sequence += 1;
    let message = Control::Adjust {
        sequence: slot.adjust_sequence,
        resources: BTreeMap::from([(
            key,
            crate::contention::StepRequest {
                step: crate::contention::YieldStep::Terminate,
                target: None,
                cause: cause.unwrap_or(crate::contention::YieldCause::FloorBreach),
                deadline_ms: None,
            },
        )]),
    };
    // A closed pipe already means agent loss for this supervisor.
    let _ = send(slot, &message);
}
/// Drains sent since the last call, local or for the coordinator, each counted once.
fn new_drains<'a>(
    slots: impl Iterator<Item = (&'a String, Option<Instant>)>,
    counted: &mut BTreeSet<String>,
) -> usize {
    let mut new = 0;
    for (id, sent) in slots {
        if sent.is_some() && counted.insert(id.clone()) {
            new += 1;
        }
    }
    new
}
fn minimum(left: &Resources, right: &Resources) -> Resources {
    Resources {
        cpu_millicores: left.cpu_millicores.min(right.cpu_millicores),
        ram_mib: left.ram_mib.min(right.ram_mib),
        gpu_memory_mib: left
            .gpu_memory_mib
            .iter()
            .filter_map(|(id, n)| {
                right
                    .gpu_memory_mib
                    .get(id)
                    .map(|r| (id.clone(), (*n).min(*r)))
            })
            .collect(),
    }
}
fn phase(phase: &ExecutionPhase) -> RemotePhase {
    match phase {
        ExecutionPhase::Reserved => RemotePhase::Offered,
        ExecutionPhase::Prepared => RemotePhase::Prepared,
        ExecutionPhase::Authorized => RemotePhase::Authorized,
        ExecutionPhase::Running => RemotePhase::Running,
        ExecutionPhase::Draining => RemotePhase::Draining,
        ExecutionPhase::NeedsReconciliation => RemotePhase::Uncertain,
        ExecutionPhase::Released => RemotePhase::Released,
    }
}

/// The exclusive `agent.lock`. Its descriptor is close-on-exec, but a child that
/// another thread forks shares the open file until it executes, and the lock
/// belongs to the open file, so dropping the guard releases the lock itself.
struct AgentLock(File);

impl AgentLock {
    fn acquire(file: File, held: &'static str) -> Result<Self> {
        fs2::FileExt::try_lock_exclusive(&file).context(held)?;
        Ok(Self(file))
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        // A failed unlock only keeps the lock until the last copy closes.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn owned_agent_lock(path: &Path) -> Result<AgentLock> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options.open(path)?;
    ensure!(
        lock.metadata()?.is_file(),
        "agent lock is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = lock.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.nlink() == 1,
            "agent lock ownership or link count invalid"
        );
        let named = fs::symlink_metadata(path)?;
        ensure!(
            named.dev() == meta.dev() && named.ino() == meta.ino(),
            "agent lock identity changed"
        );
    }
    AgentLock::acquire(lock, "another agent owns this node state")
}

fn local_clock_ms() -> Result<u64> {
    #[cfg(unix)]
    {
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0,
            "monotonic clock unavailable"
        );
        ensure!(
            time.tv_sec >= 0 && time.tv_nsec >= 0,
            "invalid monotonic clock"
        );
        Ok((time.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add(time.tv_nsec as u64 / 1_000_000))
    }
    #[cfg(not(unix))]
    {
        bail!("cross-process monotonic authority clock unavailable")
    }
}
fn spool_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    #[cfg(unix)]
    let mut inodes = BTreeSet::new();
    let mut directories = vec![path.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if metadata.is_dir() {
                directories.push(entry.path());
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if !inodes.insert((metadata.dev(), metadata.ino())) {
                        continue;
                    }
                }
                total = total
                    .checked_add(metadata.len())
                    .context("spool size overflow")?;
            }
        }
    }
    Ok(total)
}

/// Supervisor diagnostics go to a pipe the agent reads. That reader can vanish
/// (agent exit, exhausted spool budget); a failed write must never unwind a
/// supervisor that still owns a running child.
fn write_diagnostic(mut output: impl Write, line: std::fmt::Arguments<'_>) {
    let _ = writeln!(output, "{line}");
}

#[cfg(test)]
mod diagnostic_tests {
    #[test]
    #[ignore = "subprocess fixture for diagnostics_survive_a_closed_stderr_pipe"]
    fn closed_stderr_diagnostic_fixture() {
        super::write_diagnostic(std::io::stderr(), format_args!("supervisor_diagnostic"));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_survive_a_closed_stderr_pipe() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "agent::diagnostic_tests::closed_stderr_diagnostic_fixture",
                "--nocapture",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(writer)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "diagnostic write aborted the process: {status}"
        );
    }
}

#[cfg(test)]
mod agent_transport_tests {
    use super::service::*;
    use super::*;
    #[cfg(unix)]
    #[test]
    fn reconciliation_preserves_a_preparation_between_reserve_and_prepared() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: temporary.path().canonicalize().unwrap().join("state"),
            ..Config::default()
        };
        let store = StateStore::open(&config.state_dir).unwrap();
        let resources = Resources {
            cpu_millicores: 100,
            ram_mib: 32,
            gpu_memory_mib: BTreeMap::new(),
        };
        let request = crate::execution_model::LaunchRequest {
            task_id: "preparing-task".into(),
            assignment_id: "preparing".into(),
            argv: vec!["unused".into()],
            cwd: temporary.path().into(),
            env: BTreeMap::new(),
            resources: resources.clone(),
            replay_safe: true,
            class: AllocationClass::Guaranteed,
            no_escape: true,
            single_process: true,
            managed_child_limit: 0,
            max_attempts: None,
            input_artifacts: vec![],
            required_controls: vec![],
            allow_fallback: true,
            contention: None,
        };
        let capacity = Resources {
            cpu_millicores: 200,
            ram_mib: 64,
            gpu_memory_mib: BTreeMap::new(),
        };
        let mut preparing = store.reserve(&request, &capacity).unwrap();
        let mut orphan = request.clone();
        orphan.task_id = "orphan-task".into();
        orphan.assignment_id = "orphan".into();
        store.reserve(&orphan, &capacity).unwrap();
        let pending = [request.assignment_id.clone()];
        let records = reconcile_agent_owned(
            &config,
            std::iter::empty(),
            pending.iter(),
            store.namespace_guard().clone(),
        )
        .unwrap();
        assert_eq!(
            records
                .iter()
                .find(|r| r.assignment_id == "preparing")
                .unwrap()
                .phase,
            ExecutionPhase::Reserved
        );
        assert_eq!(
            records
                .iter()
                .find(|r| r.assignment_id == "orphan")
                .unwrap()
                .phase,
            ExecutionPhase::NeedsReconciliation
        );
        preparing.backend = "rootless".into();
        preparing.identity = Some(
            supervision::process_identity(std::process::id(), "preparing", preparing.generation)
                .unwrap(),
        );
        preparing.phase = ExecutionPhase::Prepared;
        store.transition(&preparing).unwrap();
    }
    #[test]
    fn observation_cadence_does_not_accumulate_control_processing_time() {
        let start = Instant::now();
        let interval = Duration::from_millis(500);
        let mut wake = start;
        let mut last_control = start - interval;
        // Sixty milliseconds of normal work used to be added to every sleep,
        // eventually dropping slots despite every RPC completing promptly.
        for tick in 0..300 {
            let scheduled = start + interval * tick;
            wake = next_periodic_wake(wake, interval, scheduled);
            let enqueue = scheduled + Duration::from_millis(60);
            let control_due = next_control_due_at(&mut last_control, interval, enqueue);
            assert_eq!(control_due, scheduled);
            assert_eq!(wake, scheduled + interval);
            assert_eq!(wake.duration_since(enqueue), Duration::from_millis(440));
        }
    }

    #[test]
    fn observation_overrun_preserves_original_due_and_missed_control_slots() {
        let start = Instant::now();
        let interval = Duration::from_millis(500);
        let first_wake = next_periodic_wake(start, interval, start);
        let late_enqueue = start + Duration::from_millis(2300);
        let mut last_control = start;
        let due = next_control_due_at(&mut last_control, interval, late_enqueue);
        assert_eq!(due, start + interval);
        assert_eq!(late_enqueue.duration_since(due).as_millis() / 500, 3);
        assert_eq!(last_control, start + Duration::from_millis(2000));
        assert_eq!(
            next_periodic_wake(first_wake, interval, late_enqueue),
            start + Duration::from_millis(2500)
        );
        assert_eq!(
            next_control_due_at(
                &mut last_control,
                interval,
                start + Duration::from_millis(2560)
            ),
            start + Duration::from_millis(2500)
        );
        // A shutdown signal may wake the loop early without shifting its clock.
        assert_eq!(
            next_periodic_wake(first_wake, interval, start + Duration::from_millis(100)),
            first_wake
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepted_receipt_reserves_complete_utf8_bytes_before_writing_and_retains_uncertainty() {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temp.path().join("state");
        let store = StateStore::open(&root).unwrap();
        let path = root.join("attempts/attempt/result.receipt.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let charge = |quota| {
            crate::publication::CaptureCharge::for_download(
                &root,
                store.storage_profile(),
                store.namespace_guard(),
                &path,
                quota,
            )
            .unwrap()
        };
        drop(charge(u64::MAX));
        let baseline: i64 = store
            .connection
            .query_row("SELECT SUM(bytes) FROM local_spool_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let receipt = serde_json::json!({"response":{"kind":"receipt","receipt_hash":"a".repeat(64)},"submission":{"result":{"metadata":{"text":"수치 값"}}}});
        let mut expected = serde_json::to_vec(&receipt).unwrap();
        expected.push(b'\n');
        let required = expected.len() as u64;
        let too_small = charge(baseline as u64 + required - 1);
        assert!(
            format!(
                "{:#}",
                write_json_new_reserved(&path, &receipt, Some(&too_small)).unwrap_err()
            )
            .contains("SPOOL_QUOTA")
        );
        assert!(!path.exists());
        let exact = charge(baseline as u64 + required);
        write_json_new_reserved(&path, &receipt, Some(&exact)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
        let collision = charge(baseline as u64 + required * 2);
        assert!(write_json_new_reserved(&path, &receipt, Some(&collision)).is_err());
        assert_eq!(fs::read(&path).unwrap(), expected);
        let retained: i64 = store
            .connection
            .query_row(
                "SELECT bytes FROM local_spool_entries WHERE path=?1",
                [path.to_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained as u64, required * 2);
    }
    #[test]
    fn publication_requires_matching_authoritative_commit_receipt() {
        use sha2::{Digest, Sha256};
        let submission = ResultSubmission {
            task_id: "task".into(),
            assignment_id: "attempt".into(),
            generation: 7,
            result: serde_json::json!({"answer": 0.1}),
            artifacts: vec![],
        };
        assert!(verify_publication_receipt(&Response::Ok, &submission).is_err());
        let mut receipt = Receipt {
            task_id: submission.task_id.clone(),
            assignment_id: submission.assignment_id.clone(),
            generation: 7,
            receipt_hash: hex::encode(Sha256::digest(serde_json::to_vec(&submission).unwrap())),
        };
        assert!(
            verify_publication_receipt(
                &Response::Receipt {
                    receipt: receipt.clone()
                },
                &submission
            )
            .is_ok()
        );
        receipt.assignment_id = "old-attempt".into();
        assert!(
            verify_publication_receipt(
                &Response::Receipt {
                    receipt: receipt.clone()
                },
                &submission
            )
            .is_err()
        );
        receipt.assignment_id = submission.assignment_id.clone();
        receipt.receipt_hash = "00".repeat(32);
        assert!(verify_publication_receipt(&Response::Receipt { receipt }, &submission).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn corrupt_and_missing_replay_state_are_preserved_or_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("node");
        let maintenance = Namespace::new(&state)
            .unwrap()
            .begin_maintenance(Duration::ZERO)
            .unwrap();
        let guard = maintenance.exclusive(Duration::ZERO).unwrap();
        assert!(
            quarantine_replay_state(&state, "missing", &guard)
                .unwrap()
                .is_none()
        );
        fs::create_dir(&state).unwrap();
        fs::write(
            state.join(crate::state::DATABASE_FILENAME),
            b"corrupt-owned-test-data",
        )
        .unwrap();
        fs::create_dir(state.join("spool")).unwrap();
        fs::write(state.join("spool/upload"), b"uncommitted").unwrap();
        let quarantined = quarantine_replay_state(&state, "fresh-session", &guard)
            .unwrap()
            .unwrap();
        assert!(!state.exists());
        assert_eq!(
            fs::read(quarantined.join(crate::state::DATABASE_FILENAME)).unwrap(),
            b"corrupt-owned-test-data"
        );
        assert_eq!(
            fs::read(quarantined.join("spool/upload")).unwrap(),
            b"uncommitted"
        );
    }
    #[cfg(unix)]
    #[test]
    fn lost_replay_namespace_refuses_existing_open_without_recreation() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: tmp.path().join("state"),
            storage_profile: crate::state::StorageProfile::BurstReplayDeleteExtra,
            ..Config::default()
        };
        let store =
            StateStore::open_with_profile(&config.state_dir, config.storage_profile).unwrap();
        let guard = ReplayJournalGuard::capture(&config).unwrap();
        guard.verify(&config).unwrap();
        drop(store);
        let db = config.state_dir.join(crate::state::DATABASE_FILENAME);
        fs::rename(&db, config.state_dir.join("lost-journal-fixture")).unwrap();
        assert!(guard.verify(&config).is_err());
        assert!(open_existing_node_store(&config).is_err());
        assert!(
            !db.exists(),
            "live replay journal loss must not create an empty replacement"
        );
        fs::write(&db, b"owned corrupt replacement").unwrap();
        assert!(guard.verify(&config).is_err());
        assert!(open_existing_node_store(&config).is_err());
        assert_eq!(fs::read(db).unwrap(), b"owned corrupt replacement");
    }
    #[cfg(unix)]
    #[test]
    fn replay_state_and_lock_reject_links_and_keep_exclusive_owner() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("owned");
        fs::create_dir(&source).unwrap();
        let link = tmp.path().join("alias");
        symlink(&source, &link).unwrap();
        assert!(Namespace::new(&link).is_err());
        let target = tmp.path().join("lock");
        let lock = owned_agent_lock(&target).unwrap();
        assert!(owned_agent_lock(&target).is_err());
        let alias = tmp.path().join("lock-alias");
        symlink(&target, &alias).unwrap();
        assert!(owned_agent_lock(&alias).is_err());
        drop(lock);
        assert!(owned_agent_lock(&target).is_ok());
    }
    /// A child another thread forks holds a copy of every descriptor until it
    /// executes, close-on-exec ones included; a dropped lock is free all the same.
    #[cfg(unix)]
    #[test]
    fn a_dropped_agent_lock_is_free_while_another_thread_spawns() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("lock");
        let lock = owned_agent_lock(&target).unwrap();
        let spawner = std::thread::spawn(|| {
            let mut command = std::process::Command::new("true");
            // SAFETY: nanosleep is async-signal-safe; the closure allocates nothing.
            unsafe {
                command.pre_exec(|| {
                    let pause = libc::timespec {
                        tv_sec: 1,
                        tv_nsec: 0,
                    };
                    libc::nanosleep(&pause, std::ptr::null_mut());
                    Ok(())
                });
            }
            command.status().unwrap()
        });
        // The child has forked and is paused before its exec.
        std::thread::sleep(Duration::from_millis(300));
        drop(lock);
        let reacquired = owned_agent_lock(&target);
        assert!(spawner.join().unwrap().success());
        assert!(reacquired.is_ok(), "{:?}", reacquired.err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn lost_family_registry_requires_verified_boot_change() {
        let identity = supervision::process_identity(std::process::id(), "owned-test", 3).unwrap();
        let mut record = ExecutionRecord {
            task_id: "task".into(),
            assignment_id: "owned-test".into(),
            generation: 3,
            class: AllocationClass::Opportunistic,
            phase: ExecutionPhase::NeedsReconciliation,
            resources: Resources::default(),
            identity: Some(identity),
            backend: "rootless".into(),
            evidence: vec![ControlEvidence {
                control: "recovery.unknown_children".into(),
                available: None,
                permitted: None,
                configured: true,
                applied: false,
                fallback: false,
                scope: "recovery".into(),
                requested: None,
                effective: None,
                detail: "lost child inventory".into(),
            }],
            detail: "owned fixture".into(),
            modes: Default::default(),
        };
        assert!(!recovery_allows_release(&record));
        record.identity.as_mut().unwrap().boot_id = "verified-previous-test-boot".into();
        assert!(recovery_allows_release(&record));
        record.identity = None;
        assert!(!recovery_allows_release(&record));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn native_reconciliation_distinguishes_live_and_reaped_owned_child() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let id = supervision::process_identity(child.id(), "owned-recovery-test", 1).unwrap();
        assert!(!identity_absent(&id));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(identity_absent(&id));
        assert!(!native_pid_absent(0));
        assert!(!native_pid_absent(u32::MAX));
    }
    #[test]
    fn unrelated_gpu_uncertainty_does_not_block_cpu_only_scope() {
        use crate::model::*;
        let mut config = Config::default();
        config.gpu.scale_up_cooldown_ms = Some(0);
        config.contention.cpu.scale_up_cooldown_ms = Some(0);
        config.contention.ram.scale_up_cooldown_ms = Some(0);
        config.cpu.reserve_physical_cores = 0;
        config.ram.reserve_mib = 0;
        config.ram.reserve_percent = 0;
        let mut snapshot = Snapshot {
            schema_version: 2,
            node_id: config.node_id.clone(),
            observed_at_unix_ms: 1,
            cpu_capacity_millicores: 8000,
            physical_cores: Some(4),
            cpu_busy_millicores: Some(1000),
            cpu_busy_lower_millicores: None,
            managed_cpu_lower_millicores: None,
            managed_cpu_window_lower_millicores: None,
            cedegrid_cpu_lower_millicores: None,
            cedegrid_cpu_window_lower_millicores: None,
            total_ram_mib: 16000,
            available_ram_mib: Some(12000),
            gpu_inventory: CapabilityStatus::Unknown,
            gpus: vec![],
            capabilities: BTreeMap::new(),
            kernel: None,
            principals: None,
            external_processes: None,
        };
        restrict_gpu_scope(&mut snapshot, &Resources::default());
        let decision = PolicyEngine::new()
            .evaluate(&config, &snapshot, &PolicyInput::default(), 0)
            .unwrap();
        assert!(decision.expansion_allowed);
        assert!(decision.managed_budget.gpu_memory_mib.is_empty());
        snapshot.cpu_busy_millicores = None;
        let decision = PolicyEngine::new()
            .evaluate(&config, &snapshot, &PolicyInput::default(), 0)
            .unwrap();
        assert!(
            !decision.expansion_allowed,
            "CPU uncertainty must still block CPU admission"
        );
    }
    #[test]
    fn operator_headroom_keeps_pending_and_observed_in_one_charge() {
        let capacity = Resources {
            cpu_millicores: 1000,
            ram_mib: 4096,
            gpu_memory_mib: BTreeMap::new(),
        };
        let allocation = crate::model::Allocation {
            id: "a".into(),
            class: AllocationClass::Opportunistic,
            phase: crate::model::AllocationPhase::Running,
            requested: Resources {
                cpu_millicores: 1000,
                ram_mib: 2048,
                gpu_memory_mib: BTreeMap::new(),
            },
            observed: Some(Resources {
                cpu_millicores: 999,
                ram_mib: 256,
                gpu_memory_mib: BTreeMap::new(),
            }),
            observed_upper: None,
            observed_window_cpu: None,
            modes: Default::default(),
        };
        let used = allocation_charge(std::slice::from_ref(&allocation));
        assert_eq!(used.cpu_millicores, 1000);
        assert_eq!(remaining_capacity(&capacity, &used).cpu_millicores, 0);
        assert_eq!(remaining_capacity(&capacity, &used).ram_mib, 2048);
        assert!(allocations_fit(
            std::slice::from_ref(&allocation),
            &capacity
        ));
        let mut exceeded = allocation;
        exceeded.observed.as_mut().unwrap().cpu_millicores = 1001;
        assert!(!allocations_fit(std::slice::from_ref(&exceeded), &capacity));
        // CPU admitted by recent usage is judged that way: a credit of the CPU held
        // above usage keeps it inside the envelope, a debit does not.
        assert!(!envelope_exceeded(
            std::slice::from_ref(&exceeded),
            &capacity,
            1
        ));
        assert!(envelope_exceeded(&[exceeded], &capacity, 0));
        let over_usage = Resources {
            cpu_millicores: 1000,
            ..Resources::default()
        };
        assert!(envelope_exceeded(
            &[crate::model::Allocation {
                requested: over_usage.clone(),
                observed: Some(over_usage),
                ..allocation_template()
            }],
            &capacity,
            -1
        ));
    }
    #[test]
    fn an_exceeded_envelope_blocks_every_resource_admission() {
        use crate::contention::{Admission, ResourceKey};
        let mut config = Config::default();
        config.cpu.reserve_physical_cores = 0;
        config.contention.cpu.scale_up_cooldown_ms = Some(0);
        config.contention.ram.scale_up_cooldown_ms = Some(0);
        let snapshot = crate::model::Snapshot {
            schema_version: crate::model::SCHEMA_VERSION,
            node_id: config.node_id.clone(),
            observed_at_unix_ms: 1,
            cpu_capacity_millicores: 8000,
            physical_cores: Some(4),
            cpu_busy_millicores: Some(2000),
            cpu_busy_lower_millicores: None,
            managed_cpu_lower_millicores: None,
            managed_cpu_window_lower_millicores: None,
            cedegrid_cpu_lower_millicores: None,
            cedegrid_cpu_window_lower_millicores: None,
            total_ram_mib: 32_000,
            available_ram_mib: Some(28_000),
            gpu_inventory: crate::model::CapabilityStatus::Unsupported,
            gpus: vec![],
            capabilities: Default::default(),
            kernel: None,
            principals: None,
            external_processes: None,
        };
        let usage = Resources {
            cpu_millicores: 2000,
            ram_mib: 2000,
            gpu_memory_mib: BTreeMap::new(),
        };
        let allocations = vec![crate::model::Allocation {
            requested: usage.clone(),
            observed: Some(usage),
            ..allocation_template()
        }];
        let mut decision = crate::policy::PolicyEngine::new()
            .evaluate(
                &config,
                &snapshot,
                &crate::model::PolicyInput {
                    allocations: allocations.clone(),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        let open = |decision: &crate::model::Decision| {
            [ResourceKey::Cpu, ResourceKey::Ram]
                .map(|key| decision.resources[&key].admission == Some(Admission::Open))
        };
        let mut within = decision.clone();
        let roomy = Resources {
            cpu_millicores: 4000,
            ram_mib: 4000,
            gpu_memory_mib: BTreeMap::new(),
        };
        block_admission_beyond_envelope(&mut within, &allocations, &roomy).unwrap();
        assert_eq!(open(&within), [true, true]);
        let tight = Resources {
            ram_mib: 1000,
            ..roomy
        };
        block_admission_beyond_envelope(&mut decision, &allocations, &tight).unwrap();
        assert_eq!(open(&decision), [false, false]);
        assert!(!decision.expansion_allowed);
    }
    fn allocation_template() -> crate::model::Allocation {
        crate::model::Allocation {
            id: "t".into(),
            class: AllocationClass::Opportunistic,
            phase: crate::model::AllocationPhase::Running,
            requested: Resources::default(),
            observed: None,
            observed_upper: None,
            observed_window_cpu: None,
            modes: Default::default(),
        }
    }
    #[tokio::test]
    async fn concurrent_transfers_share_one_budget() {
        let limiter = std::sync::Arc::new(RateLimiter::new(10_000));
        let begin = Instant::now();
        let mut tasks = vec![];
        for _ in 0..3 {
            let limiter = limiter.clone();
            tasks.push(tokio::spawn(async move { limiter.acquire(1000).await }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(begin.elapsed() >= Duration::from_millis(195));
        assert!(wire_charge(256 * 1024) > 256 * 1024);
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;

    fn reply(features: &[&str]) -> HeartbeatReply {
        HeartbeatReply {
            coordinator_epoch: 1,
            assignments: vec![],
            drain: vec![],
            uncertain: vec![],
            features: features.iter().map(|f| f.to_string()).collect(),
        }
    }

    /// The node report carries every resource the decision reports, with its
    /// admission and what it holds back.
    #[test]
    fn the_contention_report_carries_each_resource() {
        let config = Config::default();
        let snapshot = crate::model::Snapshot {
            schema_version: crate::model::SCHEMA_VERSION,
            node_id: config.node_id.clone(),
            observed_at_unix_ms: 1,
            cpu_capacity_millicores: 8000,
            physical_cores: Some(4),
            cpu_busy_millicores: Some(1000),
            cpu_busy_lower_millicores: None,
            managed_cpu_lower_millicores: None,
            managed_cpu_window_lower_millicores: None,
            cedegrid_cpu_lower_millicores: None,
            cedegrid_cpu_window_lower_millicores: None,
            total_ram_mib: 32_000,
            available_ram_mib: Some(30_000),
            gpu_inventory: crate::model::CapabilityStatus::Unsupported,
            gpus: vec![],
            capabilities: Default::default(),
            kernel: None,
            principals: None,
            external_processes: None,
        };
        let decision = crate::policy::PolicyEngine::new()
            .evaluate(&config, &snapshot, &Default::default(), 0)
            .unwrap();
        let report = node_contention_report(&config, &decision, 4);
        assert_eq!(report.max_workers, Some(4));
        for key in [
            crate::contention::ResourceKey::Cpu,
            crate::contention::ResourceKey::Ram,
        ] {
            let resource = report.resources.get(&key).expect("resource missing");
            assert!(resource.held_back.is_some(), "{key}");
        }
    }

    fn report() -> NodeReport {
        NodeReport {
            node_id: "node".into(),
            boot_id: "boot".into(),
            observed_at_unix_ms: 1,
            managed_budget: Resources::default(),
            expansion_allowed: false,
            gpu_expansion_allowed: false,
            gpu_guaranteed_allowed: false,
            launch_slots: 0,
            available_controls: vec![],
            allocations: vec![AllocationReport {
                assignment_id: "a".into(),
                generation: 1,
                phase: RemotePhase::Draining,
                observed: None,
                detail: String::new(),
                yield_cause: Some(crate::contention::YieldCause::ExternalGrowth),
                hint_ready: Some(true),
                observes_drain: Some(true),
            }],
            features: vec![],
            contention: None,
        }
    }

    fn plain(report: &NodeReport) -> bool {
        let value = serde_json::to_value(report).unwrap();
        value.get("features").is_none()
            && value.get("contention").is_none()
            && ["yield_cause", "hint_ready", "observes_drain"]
                .iter()
                .all(|field| value["allocations"][0].get(field).is_none())
    }

    /// Lease batches go only to a coordinator that advertises them, and a refused batch turns
    /// them off until the next reply advertises them again (FR-074).
    #[test]
    fn lease_batches_follow_the_coordinators_features() {
        let mut negotiated = NegotiatedFeatures::default();
        assert!(!negotiated.lease_batch);
        negotiated.learn(&reply(&[FEATURE_API, FEATURE_CONTENTION]));
        assert!(!negotiated.lease_batch);
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        assert!(negotiated.lease_batch && negotiated.contention);
        negotiated.forget_lease_batch();
        assert!(!negotiated.lease_batch && negotiated.contention);
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        negotiated.forget();
        assert!(!negotiated.lease_batch && !negotiated.contention);
    }

    #[test]
    fn new_fields_are_sent_only_after_the_coordinator_advertises_them() {
        let mut negotiated = NegotiatedFeatures::default();
        let mut first = report();
        negotiated.apply(&mut first, NodeContentionReport::default);
        assert!(plain(&first), "the first heartbeat must stay 0.2-shaped");

        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        let mut later = report();
        negotiated.apply(&mut later, NodeContentionReport::default);
        assert_eq!(later.features, COORDINATOR_FEATURES);
        assert!(later.contention.is_some());
        assert!(later.allocations[0].yield_cause.is_some());
    }

    #[test]
    fn features_are_relearned_from_every_reply_and_dropped_after_a_rejection() {
        let mut negotiated = NegotiatedFeatures::default();
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        negotiated.learn(&reply(&[]));
        let mut restarted = report();
        negotiated.apply(&mut restarted, NodeContentionReport::default);
        assert!(
            plain(&restarted),
            "a coordinator without the feature gets 0.2 reports"
        );

        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        negotiated.forget();
        let mut rejected = report();
        negotiated.apply(&mut rejected, NodeContentionReport::default);
        assert!(
            plain(&rejected),
            "a rejected heartbeat falls back to 0.2 reports"
        );

        // The next accepted 0.2 report learns the features again (deviation 11).
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        let mut recovered = report();
        negotiated.apply(&mut recovered, NodeContentionReport::default);
        assert!(!plain(&recovered) && recovered.contention.is_some());
    }
}

#[cfg(all(test, unix))]
mod drain_tests {
    use super::*;

    /// The RAM cap step applies only to work in a delegated cgroup leaf with
    /// `memory.high` authorized; rootless work never takes it.
    #[test]
    fn only_a_delegated_leaf_with_memory_high_offers_the_ram_cap() {
        let mut config = Config::default();
        assert!(!ram_cap_available(&config, Some("rootless")));
        assert!(!ram_cap_available(&config, Some("cgroup_v2")));
        config.cgroup.enabled = true;
        config.cgroup.authorized_controls = vec!["cgroup.procs".into(), "cpu.weight".into()];
        assert!(!ram_cap_available(&config, Some("cgroup_v2")));
        config.cgroup.authorized_controls.push("memory.high".into());
        assert!(ram_cap_available(&config, Some("cgroup_v2")));
        assert!(!ram_cap_available(&config, Some("rootless")));
        assert!(!ram_cap_available(&config, None));
    }

    #[test]
    fn supervisor_readiness_reaches_the_policy_facts() {
        // An older supervisor reports no readiness, which is not ready.
        let older: Event = serde_json::from_str(
            r#"{"event":"adjusted","sequence":1,"applied":{},"unavailable":{},"detail":""}"#,
        )
        .unwrap();
        assert!(matches!(
            older,
            Event::Adjusted {
                hint_ready: false,
                observes_drain: false,
                ..
            }
        ));
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut slot = slot_with(child, input);
        slot.sequence = 1;
        assert!(!slot.facts().hint_ready && !slot.facts().observes_drain);
        slot.note_readiness(false, true);
        slot.note_readiness(true, true);
        // Readiness never turns false again within one supervisor.
        slot.note_readiness(false, false);
        let facts = slot.facts();
        assert!(facts.hint_ready && facts.observes_drain);
    }

    fn slot_with(child: Child, input: std::process::ChildStdin) -> ChildSlot {
        let (_tx, events) = mpsc::channel();
        let request = crate::execution_model::LaunchRequest {
            task_id: "t".into(),
            assignment_id: "a".into(),
            argv: vec!["unused".into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            resources: Resources::default(),
            replay_safe: true,
            class: AllocationClass::Guaranteed,
            no_escape: true,
            single_process: true,
            managed_child_limit: 0,
            max_attempts: None,
            input_artifacts: vec![],
            required_controls: vec![],
            allow_fallback: true,
            contention: None,
        };
        ChildSlot {
            child: SupervisorProcess::Owned(child),
            input: Some(Box::new(input)),
            events,
            assignment: Assignment {
                node_id: "node".into(),
                generation: 1,
                coordinator_epoch: 1,
                request,
                checkpoint: None,
            },
            output_dir: std::env::temp_dir(),
            sequence: 0,
            last_renew: Instant::now(),
            prepared: None,
            reported: false,
            checkpoint_hash: None,
            exited: false,
            capture_threads: vec![],
            unavailable: BTreeMap::new(),
            applied: BTreeMap::new(),
            applied_sequence: 0,
            applied_targets: BTreeMap::new(),
            step_codes: BTreeMap::new(),
            hint_ready: false,
            observes_drain: false,
            drain_sent_at: None,
            drain_cause: None,
            started_at: Instant::now(),
            last_checkpoint_at: None,
            adjust_sequence: 0,
            sent_steps: BTreeMap::new(),
            sent_allowance: None,
            sent_oom: None,
            oom_context: None,
            seed: None,
            journal_seen: None,
        }
    }

    /// A prepared reservation the supervisor has not been authorized for may be cancelled:
    /// the pending pass may pick it, and once its drain is sent no initial authorization is
    /// requested for it. An authorized one whose journal still reads Prepared is no longer the
    /// pending pass's to cancel (FR-012).
    #[test]
    fn a_cancelled_prepared_slot_is_released_without_authorization() {
        use super::service::{wants_authorization, withhold_authorized_pending};
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut slot = slot_with(child, input);
        assert!(slot.facts().uncontrollable, "nothing prepared yet");
        assert!(!wants_authorization(&slot));
        slot.prepared = Some(ExecutionRecord {
            task_id: "t".into(),
            assignment_id: "a".into(),
            generation: 1,
            class: AllocationClass::Opportunistic,
            phase: ExecutionPhase::Prepared,
            resources: Resources::default(),
            identity: None,
            backend: "rootless".into(),
            evidence: vec![],
            detail: String::new(),
            modes: BTreeMap::new(),
        });
        assert!(
            !slot.facts().uncontrollable,
            "a prepared reservation may be cancelled"
        );
        assert!(wants_authorization(&slot));
        drain(
            &mut slot,
            Some(crate::contention::YieldCause::ExternalGrowth),
        );
        assert!(
            !wants_authorization(&slot),
            "a cancelled slot is not authorized"
        );
        // Authorized, but its journal still reads Prepared: the pending pass leaves it alone.
        let mut facts = BTreeMap::from([("a".to_string(), slot.facts())]);
        let pending = crate::model::Allocation {
            id: "a".into(),
            class: AllocationClass::Opportunistic,
            phase: crate::model::AllocationPhase::Pending,
            requested: Resources::default(),
            observed: None,
            observed_upper: None,
            observed_window_cpu: None,
            modes: BTreeMap::new(),
        };
        withhold_authorized_pending(&mut facts, std::slice::from_ref(&pending), |_| true);
        assert!(facts["a"].uncontrollable);
    }

    /// The agent loop's own CPU by stage is reported once a period, then starts again
    /// (SC-014).
    #[test]
    fn loop_cost_reports_each_stage_once_a_period() {
        use super::service::LoopCost;
        let start = Instant::now();
        let mut cost = LoopCost::new(start, Duration::from_secs(60));
        cost.record("observe", 3_000_000);
        cost.record("heartbeat", 1_000_000);
        cost.iteration();
        cost.record("observe", 2_000_000);
        cost.iteration();
        assert_eq!(cost.take_report(start + Duration::from_secs(59)), None);
        let report = cost.take_report(start + Duration::from_secs(60)).unwrap();
        assert_eq!(report["event"], "loop_cost");
        assert_eq!(report["iterations"], 2);
        assert_eq!(report["thread_cpu_us"]["observe"], 5000);
        assert_eq!(report["thread_cpu_us"]["heartbeat"], 1000);
        assert_eq!(report["period_ms"], 60_000);
        assert_eq!(cost.take_report(start + Duration::from_secs(61)), None);
    }

    /// A reattached supervisor reports each step's cause, so a restarted agent reseeds an
    /// `own_over_target` step with its own cause; an older supervisor reports none, and its
    /// steps count as external growth (FR-018).
    #[test]
    fn a_reattached_step_keeps_its_cause() {
        use crate::contention::{ResourceKey, YieldCause, YieldStep};
        let line = r#"{"event":"state","applied":{"cpu":"cap"},"targets":{"cpu":200},"causes":{"cpu":"own_over_target"},"lease_sequence":3,"sequence":2,"authority":"connected"}"#;
        let Event::State {
            applied,
            targets,
            causes,
            ..
        } = serde_json::from_str(line).unwrap()
        else {
            panic!("not a state event");
        };
        let steps = super::reconcile::reattached_steps(&applied, &targets, &causes);
        assert_eq!(steps[&ResourceKey::Cpu].step, YieldStep::Cap);
        assert_eq!(steps[&ResourceKey::Cpu].target, Some(200));
        assert_eq!(steps[&ResourceKey::Cpu].cause, YieldCause::OwnOverTarget);
        let older = r#"{"event":"state","applied":{"cpu":"cap"},"targets":{"cpu":200},"lease_sequence":3,"authority":"connected"}"#;
        let Event::State {
            applied,
            targets,
            causes,
            ..
        } = serde_json::from_str(older).unwrap()
        else {
            panic!("not a state event");
        };
        let steps = super::reconcile::reattached_steps(&applied, &targets, &causes);
        assert_eq!(steps[&ResourceKey::Cpu].cause, YieldCause::ExternalGrowth);
    }

    #[test]
    fn dropping_a_slot_closes_the_pipe_without_drain_and_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let received = dir.path().join("received");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("cat > '{}'; sleep 1", received.display()))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let slot = slot_with(child, input);
        let started = Instant::now();
        drop(slot);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "drop must not wait for the supervisor"
        );
        let end = Instant::now() + Duration::from_secs(3);
        while !received.exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::read_to_string(&received).unwrap(),
            "",
            "no Drain on drop"
        );
    }

    #[test]
    fn every_sent_drain_is_counted_once_for_back_off() {
        let now = Instant::now();
        let mut counted = BTreeSet::new();
        // A local drain sent between heartbeats and a coordinator drain both count.
        let sent = BTreeMap::from([
            ("local".to_string(), Some(now)),
            ("coordinator".to_string(), Some(now)),
            ("running".to_string(), None),
        ]);
        assert_eq!(
            new_drains(sent.iter().map(|(k, v)| (k, *v)), &mut counted),
            2
        );
        // The same drains are not counted again on the next heartbeat.
        assert_eq!(
            new_drains(sent.iter().map(|(k, v)| (k, *v)), &mut counted),
            0
        );
        let mut more = sent.clone();
        more.insert("later".into(), Some(now));
        assert_eq!(
            new_drains(more.iter().map(|(k, v)| (k, *v)), &mut counted),
            1
        );
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::service::{lease_order, lease_turn, needs_pin};
    use std::time::{Duration, Instant};

    /// Lease requests go out longest waiting first, whatever kind they are.
    #[test]
    fn lease_requests_go_out_longest_waiting_first() {
        let now = Instant::now();
        let mut waiting = vec![
            (now, "renewal due now".to_string()),
            (
                now - Duration::from_millis(900),
                "authorization".to_string(),
            ),
            (
                now - Duration::from_millis(300),
                "older renewal".to_string(),
            ),
        ];
        lease_order(&mut waiting);
        let order: Vec<&str> = waiting.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(order, ["authorization", "older renewal", "renewal due now"]);
    }

    /// Requests share half the monitor interval, and the longest-waiting one
    /// always goes, so an iteration stays bounded and requests still progress.
    #[test]
    fn lease_requests_stop_at_the_budget_but_the_first_always_goes() {
        let budget = Duration::from_millis(250);
        assert!(lease_turn(0, Duration::from_secs(5), budget));
        assert!(lease_turn(3, Duration::from_millis(100), budget));
        assert!(!lease_turn(1, Duration::from_millis(250), budget));
        assert!(!lease_turn(4, Duration::from_millis(400), budget));
    }

    fn renew(id: &str) -> crate::protocol::LeaseItem {
        crate::protocol::LeaseItem::Renew {
            assignment_id: id.into(),
            generation: 1,
            coordinator_epoch: 1,
            previous_sequence: 1,
        }
    }

    /// Renewals go first, earliest lease deadline first, then initial authorizations, longest
    /// waiting first; a batch holds at most 128 items (FR-074).
    #[test]
    fn lease_batches_put_renewals_first_and_split_at_their_limit() {
        use super::service::lease_batches;
        let now = Instant::now();
        let at = |ms: u64| now - Duration::from_millis(ms);
        let renewals = vec![(at(100), renew("late")), (at(900), renew("early"))];
        let initial = vec![(at(50), renew("new")), (at(500), renew("older"))];
        let batches = lease_batches(renewals, initial);
        let ids: Vec<&str> = batches[0].iter().map(|item| item.assignment_id()).collect();
        assert_eq!(ids, ["early", "late", "older", "new"]);
        let many: Vec<_> = (0..300).map(|i| (at(i), renew(&format!("r{i}")))).collect();
        let sizes: Vec<usize> = lease_batches(many, vec![]).iter().map(Vec::len).collect();
        assert_eq!(sizes, [128, 128, 44]);
    }

    /// A batch item's refusal reads exactly as a single request's, so per-item events keep
    /// their bytes.
    #[test]
    fn a_batch_item_result_reads_as_a_single_request() {
        use super::service::item_result;
        let single: anyhow::Result<crate::protocol::Response> = Err(anyhow::anyhow!(
            "coordinator rejected request: lease expired"
        ));
        let item = item_result(crate::protocol::LeaseResult::Error {
            message: "lease expired".into(),
        });
        assert_eq!(format!("{item:?}"), format!("{single:?}"));
    }

    /// Simulated: 300 slots renewing every 2 s against a coordinator whose commit takes 9 ms,
    /// batched, keep every 10 s lease with under 100 ms of lease work per iteration; one request
    /// per lease would not (FR-074).
    #[test]
    fn batches_keep_300_leases_renewed_against_a_9_ms_commit() {
        use super::service::{lease_batches, lease_turn};
        let run = |batched: bool| {
            let mut last = vec![0u64; 300];
            let (mut worst_gap, mut worst_work) = (0u64, 0u64);
            let mut now = 0u64;
            while now <= 60_000 {
                let due: Vec<usize> = (0..300).filter(|i| now - last[*i] >= 2_000).collect();
                let origin = Instant::now();
                let items: Vec<_> = due
                    .iter()
                    .map(|i| {
                        (
                            origin + Duration::from_millis(last[*i]),
                            renew(&i.to_string()),
                        )
                    })
                    .collect();
                // Each request costs one 9 ms commit plus 0.1 ms per item served.
                let groups = if batched {
                    lease_batches(items, vec![])
                } else {
                    items.into_iter().map(|item| vec![item.1]).collect()
                };
                let mut work = 0u64;
                for (sent, group) in groups.iter().enumerate() {
                    if !lease_turn(
                        sent,
                        Duration::from_micros(work),
                        Duration::from_millis(250),
                    ) {
                        break;
                    }
                    work += 9_000 + 100 * group.len() as u64;
                    for item in group {
                        last[item.assignment_id().parse::<usize>().unwrap()] = now;
                    }
                }
                worst_work = worst_work.max(work);
                worst_gap = worst_gap.max((0..300).map(|i| now - last[i]).max().unwrap());
                now += 500;
            }
            (worst_gap, worst_work)
        };
        let (gap, work) = run(true);
        assert!(gap < 10_000, "a lease went {gap} ms unrenewed");
        assert!(work < 100_000, "lease work {work} µs per iteration");
        let (gap, _) = run(false);
        assert!(gap >= 10_000, "single requests kept up: {gap} ms");
    }

    /// A slot with nothing new to publish takes no attempt pin; a released
    /// result, a new checkpoint head or an unreadable head does.
    #[test]
    fn only_a_possible_publication_takes_an_attempt_pin() {
        assert!(!needs_pin(false, false, Some(None), None));
        assert!(needs_pin(false, true, Some(None), None));
        assert!(!needs_pin(true, false, Some(None), None));
        assert!(!needs_pin(true, false, Some(Some("a")), Some("a")));
        assert!(needs_pin(true, false, Some(Some("b")), Some("a")));
        assert!(needs_pin(true, false, Some(Some("a")), None));
        assert!(needs_pin(true, false, None, Some("a")));
    }
}
