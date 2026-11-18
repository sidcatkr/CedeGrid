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
mod history;
mod launch;
mod oom;
mod publishing;
mod reconcile;
mod recovery;
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
    /// The soft descriptor limit the agent started with, restored for the task (FR-077).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile_soft: Option<u64>,
    /// When the agent launched it, so a re-adopted run keeps its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at_unix_ms: Option<u64>,
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
        /// Processes found outside the verified family; an older supervisor reports none.
        #[serde(default)]
        outside_family: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outside_reason: Option<crate::supervision::OutsideReason>,
        /// The run's family, so a re-adopting agent learns one that no longer changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        family: Option<crate::supervision::FamilyReport>,
    },
    /// More processes were found outside the verified family (FR-082).
    OutsideFamily {
        count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<crate::supervision::OutsideReason>,
    },
    /// The run's family changed (FR-083).
    Family {
        family: crate::supervision::FamilyReport,
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
    /// Ask the worker to save its progress (FR-075).
    SaveProgress,
    /// The agent's latest GPU observation. Supervisors run no collector of
    /// their own; GPU launch rechecks use this (research R1).
    Observation {
        snapshot: Box<crate::model::Snapshot>,
    },
}
/// Evaluates the policy. When the decision takes a new memory step on an allocation whose RAM
/// is an estimate between PSS passes (a new or changed RAM step, a new OOM preferred mark or a
/// save-progress signal), `fresh` reads those allocations now and the policy evaluates again
/// from its state before the interval, so only a step a fresh reading confirms is applied
/// (FR-085, R45). `input` keeps the fresh readings; a run `fresh` could not read is unknown.
pub fn confirm_memory_steps(
    policy: &mut PolicyEngine,
    config: &Config,
    snapshot: &crate::model::Snapshot,
    input: &mut PolicyInput,
    now_ms: u64,
    estimated: &BTreeSet<String>,
    fresh: impl FnOnce(&BTreeSet<String>) -> BTreeMap<String, Option<u64>>,
) -> Result<crate::model::Decision> {
    use crate::contention::ResourceKey;
    let before = (!estimated.is_empty()).then(|| policy.clone());
    let decision = policy.evaluate(config, snapshot, input, now_ms)?;
    let Some(before) = before else {
        return Ok(decision);
    };
    let mut runs: BTreeSet<String> = decision
        .actions
        .iter()
        .filter(|action| {
            action.resource == ResourceKey::Ram
                && estimated.contains(&action.allocation_id)
                && before.step(&action.allocation_id, &ResourceKey::Ram)
                    != Some((action.step, action.target))
        })
        .map(|action| action.allocation_id.clone())
        .collect();
    runs.extend(
        decision
            .oom_preferred
            .iter()
            .filter(|id| estimated.contains(*id) && !before.oom_preferred().contains(*id))
            .cloned(),
    );
    runs.extend(
        decision
            .save_progress
            .iter()
            .filter(|id| estimated.contains(*id))
            .cloned(),
    );
    if runs.is_empty() {
        return Ok(decision);
    }
    for (id, ram) in fresh(&runs) {
        let Some(allocation) = input.allocations.iter_mut().find(|a| a.id == id) else {
            continue;
        };
        match ram {
            Some(ram) => {
                if let Some(observed) = allocation.observed.as_mut() {
                    observed.ram_mib = ram;
                }
                if let Some(upper) = allocation.observed_upper.as_mut() {
                    upper.ram_mib = upper.ram_mib.max(ram);
                }
            }
            None => {
                allocation.observed = None;
                allocation.observed_upper = None;
            }
        }
    }
    *policy = before;
    policy.evaluate(config, snapshot, input, now_ms)
}

fn emit(value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut out = std::io::stdout().lock();
    match out.write_all(&line).and_then(|()| out.flush()) {
        // Events are diagnostics: once their reader is gone they are dropped, and the agent
        // goes on.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
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
    fn within(
        &self,
        wait: std::time::Duration,
        write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.store.within(wait, write)
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

/// Descriptors one running task holds: three supervisor pipes, a state connection and its
/// WAL, the held pidfd and the stderr capture, 7 as measured, with one to spare (FR-077).
#[cfg(unix)]
const TASK_DESCRIPTORS: u64 = 8;
/// Descriptors kept free for the agent's own reads (FR-077).
#[cfg(unix)]
const DESCRIPTOR_HEADROOM: u64 = 64;

/// The soft `RLIMIT_NOFILE` the agent started with, which tasks get back (FR-077).
static STARTING_DESCRIPTOR_LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// How many more launches fit under `soft_limit` with `open` descriptors open and
/// `launching` launches still opening theirs (FR-077).
#[cfg(unix)]
fn launches_that_fit(open: u64, soft_limit: u64, launching: u64) -> u64 {
    (soft_limit.saturating_sub(open.saturating_add(DESCRIPTOR_HEADROOM)) / TASK_DESCRIPTORS)
        .saturating_sub(launching)
}

/// Whether an error came from running out of file descriptors.
fn descriptor_exhausted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE)
    })
}

/// Whether an error came from a full disk or quota: transient, so the interval is skipped.
fn storage_exhausted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| code == libc::ENOSPC || code == libc::EDQUOT)
    })
}

pub(crate) use crate::state::journal_busy;

/// This process's soft and hard `RLIMIT_NOFILE`.
#[cfg(unix)]
// `rlim_t` is not `u64` on every target.
#[allow(clippy::unnecessary_cast)]
fn descriptor_limit() -> Option<(u64, u64)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid output pointer.
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0)
        .then_some((limit.rlim_cur as u64, limit.rlim_max as u64))
}

/// Raise the agent's own soft `RLIMIT_NOFILE` to its hard limit and remember the soft
/// limit it had. A hard limit the platform will not grant whole, such as macOS's
/// unlimited one, is taken as far as the platform allows. Needs no privilege.
#[cfg(unix)]
fn raise_descriptor_limit() -> Result<u64> {
    let (soft, hard) = descriptor_limit().context("RLIMIT_NOFILE is unreadable")?;
    STARTING_DESCRIPTOR_LIMIT.get_or_init(|| soft);
    for target in [hard, 1 << 20, 65_536, 10_240] {
        if target <= soft || target > hard {
            continue;
        }
        let limit = libc::rlimit {
            rlim_cur: target as libc::rlim_t,
            rlim_max: hard as libc::rlim_t,
        };
        // SAFETY: changes only this process's own soft limit, within its hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
            break;
        }
    }
    Ok(soft)
}

/// Descriptors this process has open, counted from its descriptor directory.
#[cfg(unix)]
fn open_descriptors() -> Option<u64> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    // The count includes the directory's own descriptor, which is one to spare.
    Some(std::fs::read_dir(directory).ok()?.count() as u64)
}

/// How many more launches fit now; none when the count or limit is unknown.
#[cfg(unix)]
fn descriptor_room(launching: u64) -> u64 {
    match (open_descriptors(), descriptor_limit()) {
        (Some(open), Some((soft, _))) => launches_that_fit(open, soft, launching),
        _ => 0,
    }
}

/// Elsewhere handles have no per-process limit of this kind.
#[cfg(not(unix))]
fn descriptor_room(_launching: u64) -> u64 {
    u64::MAX
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
    /// When the first Drain was decided and its send tried, by local policy or for the
    /// coordinator.
    drain_sent_at: Option<Instant>,
    /// The last drain or terminate did not reach the supervisor; it is sent again.
    drain_unsent: bool,
    /// A renewal reply's drain found the supervisor's pipe closed; reported once (R85).
    drain_undelivered: bool,
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
    /// RAM as the agent last saw it while the task ran, to judge an OOM kill (FR-076).
    ram_view: Option<oom::RamView>,
    /// What the outcome said about an OOM kill, once classified.
    oom_verdict: Option<oom::OomVerdict>,
    /// Steps learned from a reattached supervisor, to seed the policy authority.
    seed: Option<BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest>>,
    /// What the slot pass last read from the journal for this slot.
    journal_seen: Option<JournalSeen>,
    /// The most processes the supervisor found outside the verified family (FR-082), and why.
    outside_family: u64,
    outside_reason: Option<crate::supervision::OutsideReason>,
    /// The run's family as its supervisor last reported it (FR-083).
    family: Option<crate::supervision::FamilyReport>,
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

/// Move cached journal reads past the agent's own observation writes: entries read at
/// `before` stand at `after` too when no other connection committed in between, since this
/// connection wrote only observations.
fn rekey_journal_cache(
    slots: &mut BTreeMap<String, ChildSlot>,
    before: (i64, i64),
    after: (i64, i64),
) {
    if before.0 != after.0 {
        return;
    }
    for seen in slots
        .values_mut()
        .filter_map(|slot| slot.journal_seen.as_mut())
    {
        if seen.key == before {
            seen.key = after;
        }
    }
}

impl ChildSlot {
    /// The run's verified descendants as identities its leader's record anchors, for
    /// measurement only: telemetry checks each start time and never acts on one (FR-085).
    fn descendants(&self) -> Vec<(String, crate::execution_model::ProcessIdentity)> {
        let (Some(family), Some(leader)) = (
            self.family.as_ref(),
            self.prepared.as_ref().and_then(|r| r.identity.as_ref()),
        ) else {
            return Vec::new();
        };
        family
            .descendants
            .iter()
            .map(|member| {
                (
                    leader.assignment_id.clone(),
                    crate::execution_model::ProcessIdentity {
                        pid: member.pid,
                        boot_id: leader.boot_id.clone(),
                        start_time: member.start_time,
                        assignment_id: leader.assignment_id.clone(),
                        generation: leader.generation,
                    },
                )
            })
            .collect()
    }

    /// Keep the run's last family in its attempt directory, so an agent started after its
    /// supervisor died can still recover the members (FR-088).
    fn keep_family(&self, state_dir: &Path) {
        let (Some(family), Some(record)) = (&self.family, &self.prepared) else {
            return;
        };
        let attempt = state_dir.join("attempts").join(&record.assignment_id);
        if let Err(error) = recovery::record_family(&attempt, record, family) {
            let _ = emit(&serde_json::json!({
                "event": "family_record_failed",
                "assignment_id": record.assignment_id,
                "detail": format!("{error:#}"),
            }));
        }
    }

    /// Keep the most processes found outside the family at once, and why (FR-082).
    fn note_outside(&mut self, count: u64, reason: Option<crate::supervision::OutsideReason>) {
        if count > self.outside_family || (count == self.outside_family && reason.is_some()) {
            self.outside_family = count;
            self.outside_reason = reason.or(self.outside_reason);
        }
    }

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
            outside_family: self.outside_family,
            outside_reason: self.outside_reason.map(|reason| reason.code().into()),
            family: self.family.as_ref().map(|family| crate::model::FamilySize {
                leader: family.leader,
                sdk_children: family.sdk_children,
                descendants: family.descendants.len() as u64,
            }),
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
    let input = slot.input.as_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "supervisor control pipe closed",
        )
    })?;
    // One write for the whole line: the pipe is unbuffered.
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    input.write_all(&line)?;
    input.flush()?;
    Ok(())
}
/// The wall time one iteration's share of a draining stop may cost the supervisors' journal
/// writes, so the stop's iterations stay within the 1 s cadence (FR-080).
const STOP_DRAIN_BUDGET: Duration = Duration::from_millis(250);

/// How many of a draining stop's Drain lines one iteration sends: as many as fit in
/// `STOP_DRAIN_BUDGET` at the agent's last synced journal write each, and at least one.
fn stop_drain_quota(write: Duration) -> usize {
    let each = write.max(Duration::from_millis(1)).as_micros();
    usize::try_from((STOP_DRAIN_BUDGET.as_micros() / each).max(1)).unwrap_or(usize::MAX)
}

/// Send the drains a decision asks for; Guaranteed work drains only in a draining stop or
/// for a breach the policy permits. Every drain sent takes one of the iteration's `quota`;
/// the stop's own drains wait for a later iteration once it is used up, while a drain the
/// policy decides for a breach or for contention goes out at once. Returns how many were
/// sent.
fn dispatch_drains(
    slots: &mut BTreeMap<String, ChildSlot>,
    would_drain: &[String],
    actions: &[crate::model::YieldAction],
    draining_stop: bool,
    quota: &mut usize,
) -> usize {
    use crate::contention::{YieldCause, YieldStep};
    let causes: BTreeMap<&str, YieldCause> = actions
        .iter()
        .filter(|a| a.step >= YieldStep::Drain)
        .map(|a| (a.allocation_id.as_str(), a.cause))
        .collect();
    // The critical floor path terminates workers that do not poll the drain file.
    let terminations: BTreeMap<&str, crate::contention::ResourceKey> = actions
        .iter()
        .filter(|a| a.step == YieldStep::Terminate)
        .map(|a| (a.allocation_id.as_str(), a.resource.clone()))
        .collect();
    let mut sent = 0;
    for id in would_drain {
        let cause = causes.get(id.as_str()).copied();
        let Some(slot) = slots.get_mut(id) else {
            continue;
        };
        // A drain that reached its supervisor is not sent again; one whose send failed is.
        if (slot.drain_sent_at.is_some() && !slot.drain_unsent)
            || !(draining_stop
                || slot.assignment.request.class == AllocationClass::Opportunistic
                || matches!(
                    cause,
                    Some(YieldCause::FloorBreach | YieldCause::EnvelopeBreach)
                ))
        {
            continue;
        }
        let stop_only = cause.is_none_or(|cause| cause == YieldCause::OperatorDrain);
        if draining_stop && stop_only && *quota == 0 {
            continue;
        }
        let delivered = match terminations.get(id.as_str()) {
            Some(key) => terminate(slot, key.clone(), cause),
            None => drain(slot, cause),
        };
        // A send that failed used none of the iteration's share and is tried again.
        if delivered {
            *quota = quota.saturating_sub(1);
            sent += 1;
        }
    }
    sent
}

/// Apply the coordinator's drains from a heartbeat reply. A supervisor that has exited has
/// nothing left to drain and is skipped. One not sent before waits for a later heartbeat, which
/// asks again, once the iteration's `quota` is used up, and takes one of it only when it reaches
/// its supervisor (FR-080). Returns how many reached their supervisor for the first time.
fn apply_coordinator_drains(
    slots: &mut BTreeMap<String, ChildSlot>,
    ids: &[String],
    quota: &mut usize,
) -> usize {
    let mut sent = 0;
    for id in ids {
        let Some(slot) = slots.get_mut(id) else {
            continue;
        };
        if slot.exited {
            continue;
        }
        let first = slot.drain_sent_at.is_none();
        if first && *quota == 0 {
            continue;
        }
        if drain(slot, Some(crate::contention::YieldCause::CoordinatorDrain)) && first {
            *quota -= 1;
            sent += 1;
        }
    }
    sent
}

/// Send Drain and record it for the admission back-off. The drain counts as decided from the
/// first call, whether or not the send reached the supervisor; returns whether it did.
fn drain(slot: &mut ChildSlot, cause: Option<crate::contention::YieldCause>) -> bool {
    slot.drain_sent_at.get_or_insert_with(Instant::now);
    if slot.drain_cause.is_none() {
        slot.drain_cause = cause;
    }
    let sent = send(slot, &Control::Drain { cause });
    note_drain_send(slot, sent)
}

/// Record whether a drain or terminate reached its supervisor. A closed pipe means the
/// supervisor is gone or already handles agent loss, so that drain is left alone: not sent
/// again and not reported. Any other failure leaves decided work running, so the policy's
/// drains are sent again until one lands.
fn note_drain_send(slot: &mut ChildSlot, sent: Result<()>) -> bool {
    let closed = sent.as_ref().err().is_some_and(|error| {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
    });
    if closed {
        slot.drain_unsent = false;
        return false;
    }
    if let Err(error) = &sent
        && !slot.drain_unsent
    {
        // Reported once per failure; a failed report must not stop the loop.
        let _ = emit(&serde_json::json!({
            "event": "drain_unsent",
            "assignment_id": slot.assignment.request.assignment_id,
            "detail": format!("{error:#}"),
        }));
    }
    slot.drain_unsent = sent.is_err();
    sent.is_ok()
}
/// Ask for TERM without the drain-file wait (FR-063); a supervisor that does not
/// know the step drains instead.
fn terminate(
    slot: &mut ChildSlot,
    key: crate::contention::ResourceKey,
    cause: Option<crate::contention::YieldCause>,
) -> bool {
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
    let sent = send(slot, &message);
    note_drain_send(slot, sent)
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
        // A subdirectory removed during the walk, as when an attempt is pruned, holds nothing.
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && directory != path => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
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
        // Each heartbeat reconciles again; a record that stays uncertain is not rewritten, so
        // the journal's events do not grow with every heartbeat.
        let events = || -> i64 {
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM execution_events WHERE assignment_id='orphan'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let before = events();
        for _ in 0..3 {
            reconcile_agent_owned(
                &config,
                std::iter::empty(),
                pending.iter(),
                store.namespace_guard().clone(),
            )
            .unwrap();
        }
        assert_eq!(
            events(),
            before,
            "an unchanged uncertain record is not journaled again"
        );
    }

    /// The reconcile path checks a GPU context's release once per heartbeat; it never waits
    /// for it, which blocked the agent's loop up to the release-confirm timeout per record.
    #[test]
    fn reconciliation_never_waits_for_a_gpu_release() {
        let source = include_str!("agent/reconcile.rs");
        assert!(
            !source.contains(".confirm("),
            "reconcile.rs waits on GpuReleaseGuard::confirm"
        );
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
        let temp = crate::namespace::TestStateDir(
            tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap(),
        );
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
            usage_unknown: None,
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
            usage_unknown: None,
            modes: Default::default(),
        }
    }
    #[test]
    fn concurrent_transfers_share_one_budget() {
        let limiter = RateLimiter::new(10_000);
        let now = tokio::time::Instant::now() + Duration::from_millis(10);
        let waits: Vec<_> = (0..3).map(|_| limiter.book_at(1000, now)).collect();
        assert_eq!(waits[0], Duration::ZERO);
        assert_eq!(waits[2], Duration::from_millis(200));
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
            sample_age_ms: None,
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
        assert!(negotiated.lease_batch && negotiated.contention && negotiated.report_age);
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

        // The next accepted 0.2 report learns the features again.
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        let mut recovered = report();
        negotiated.apply(&mut recovered, NodeContentionReport::default);
        assert!(!plain(&recovered) && recovered.contention.is_some());
    }

    /// 006 SC-001: the agent waits for work only at a coordinator that holds waits, and stops
    /// after a rejected heartbeat until the next reply advertises them again.
    #[test]
    fn waits_for_work_follow_the_coordinators_features() {
        let mut negotiated = NegotiatedFeatures::default();
        assert!(!negotiated.await_work);
        negotiated.learn(&reply(&[FEATURE_API, FEATURE_CONTENTION]));
        assert!(!negotiated.await_work);
        negotiated.learn(&reply(&COORDINATOR_FEATURES));
        assert!(negotiated.await_work);
        negotiated.forget();
        assert!(!negotiated.await_work);
    }
}

#[cfg(test)]
mod await_work_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    type Script = Arc<Mutex<VecDeque<Result<Response>>>>;

    /// Runs the wait loop against scripted answers for `run`, and returns the requests it sent.
    /// An empty script holds the request, as a coordinator does while nothing moves.
    fn waits(
        watch: &Arc<WorkWatch>,
        script: Vec<Result<Response>>,
        run: Duration,
    ) -> Vec<(u64, u64)> {
        let script: Script = Arc::new(Mutex::new(script.into()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (watch, answers, log) = (Arc::clone(watch), Arc::clone(&script), Arc::clone(&sent));
        runtime.block_on(async move {
            let ask = move |request: Request| {
                let (answers, log) = (Arc::clone(&answers), Arc::clone(&log));
                async move {
                    if let Request::AwaitWork { seen, wait_ms } = request {
                        log.lock().unwrap().push((seen, wait_ms));
                    }
                    let next = answers.lock().unwrap().pop_front();
                    match next {
                        Some(answer) => answer,
                        None => std::future::pending().await,
                    }
                }
            };
            let _ = tokio::time::timeout(run, await_work(watch, ask)).await;
        });
        sent.lock().unwrap().clone()
    }

    fn generation(generation: u64) -> Result<Response> {
        Ok(Response::Work { generation })
    }

    #[test]
    fn nothing_is_asked_until_the_coordinator_holds_waits_and_the_node_has_room() {
        let watch = Arc::new(WorkWatch::default());
        assert!(waits(&watch, vec![generation(1)], Duration::from_millis(300)).is_empty());
        watch.enabled.store(true, Ordering::SeqCst);
        assert!(waits(&watch, vec![generation(1)], Duration::from_millis(300)).is_empty());
        watch.wanted.store(true, Ordering::SeqCst);
        assert_eq!(waits(&watch, vec![], Duration::from_millis(300)).len(), 1);
    }

    /// The first answer only sets what the node has seen; a later move marks new work for the
    /// next heartbeat.
    #[test]
    fn a_moved_generation_marks_new_work() {
        let watch = Arc::new(WorkWatch::default());
        watch.enabled.store(true, Ordering::SeqCst);
        watch.wanted.store(true, Ordering::SeqCst);
        let sent = waits(&watch, vec![generation(5)], Duration::from_millis(300));
        assert_eq!(sent, vec![(u64::MAX, AWAIT_WORK_MS), (5, AWAIT_WORK_MS)]);
        assert!(!watch.arrived.load(Ordering::SeqCst));
        let watch = Arc::new(WorkWatch::default());
        watch.enabled.store(true, Ordering::SeqCst);
        watch.wanted.store(true, Ordering::SeqCst);
        let sent = waits(
            &watch,
            vec![generation(5), generation(6)],
            Duration::from_millis(300),
        );
        assert_eq!(sent.last(), Some(&(6, AWAIT_WORK_MS)));
        assert!(watch.arrived.load(Ordering::SeqCst));
    }

    /// Failures back off, and an unchanged answer that came at once (a coordinator at its
    /// waiter cap) is not asked again at once either.
    #[test]
    fn failures_and_unheld_answers_do_not_spin() {
        let watch = Arc::new(WorkWatch::default());
        watch.enabled.store(true, Ordering::SeqCst);
        watch.wanted.store(true, Ordering::SeqCst);
        let errors = (0..50).map(|_| Err(anyhow::anyhow!("refused"))).collect();
        assert!(waits(&watch, errors, Duration::from_millis(1200)).len() <= 3);
        let unheld = (0..50).map(|_| generation(3)).collect();
        assert!(waits(&watch, unheld, Duration::from_millis(1200)).len() <= 3);
        assert!(!watch.arrived.load(Ordering::SeqCst));
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

    /// A draining stop sends its drains a share per iteration, sized so the supervisors'
    /// journal writes fit in 250 ms at the host's synced-write cost (run 7: about 28 ms, so 8
    /// of 150 per iteration); a drain the policy decides for a breach goes out at once
    /// (FR-080).
    #[test]
    fn a_draining_stop_spreads_its_drains_over_iterations() {
        use crate::contention::{ResourceKey, YieldCause, YieldStep};
        assert_eq!(stop_drain_quota(Duration::from_millis(28)), 8);
        assert_eq!(stop_drain_quota(Duration::ZERO), 250);
        assert_eq!(stop_drain_quota(Duration::from_secs(1)), 1);
        let mut slots = BTreeMap::new();
        let mut children = Vec::new();
        for i in 0..20 {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("cat >/dev/null")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let mut slot = slot_with(child, input);
            slot.sequence = 1;
            slot.assignment.request.class = AllocationClass::Opportunistic;
            children.push(format!("s{i:02}"));
            slots.insert(format!("s{i:02}"), slot);
        }
        let stop = |id: &str| crate::model::YieldAction {
            allocation_id: id.into(),
            resource: ResourceKey::Cpu,
            step: YieldStep::Drain,
            target: None,
            cause: YieldCause::OperatorDrain,
            code: "yield_step_drain".into(),
            deadline_ms: None,
            duty: None,
        };
        let mut actions: Vec<crate::model::YieldAction> =
            children.iter().map(|id| stop(id)).collect();
        // A breach drain for the last slot is not held back by the stop's share.
        actions[19].cause = YieldCause::FloorBreach;
        actions[19].resource = ResourceKey::Ram;
        let sent: Vec<usize> = (0..4)
            .map(|_| dispatch_drains(&mut slots, &children, &actions, true, &mut 8))
            .collect();
        assert_eq!(sent, [9, 8, 3, 0]);
        assert!(slots["s19"].drain_sent_at.is_some());
        assert!(slots.values().all(|slot| slot.drain_sent_at.is_some()));
        // Outside a stop nothing is held back.
        let reset = |slots: &mut BTreeMap<String, ChildSlot>| {
            for slot in slots.values_mut() {
                slot.drain_sent_at = None;
            }
        };
        reset(&mut slots);
        assert_eq!(
            dispatch_drains(&mut slots, &children, &actions, false, &mut 0),
            20
        );
        // An operator drain through the coordinator is spread the same way, sharing the
        // iteration's quota with the stop's own drains.
        reset(&mut slots);
        let sent: Vec<usize> = (0..4)
            .map(|_| {
                let mut quota = 8;
                dispatch_drains(&mut slots, &children[..2], &actions, true, &mut quota)
                    + apply_coordinator_drains(&mut slots, &children, &mut quota)
            })
            .collect();
        assert_eq!(sent, [8, 8, 4, 0]);
    }

    /// The agent's own observation write moves `total_changes()` but not `data_version`, so a
    /// cache entry read before it is moved past it and the next iteration reuses it; another
    /// connection's commit, or an entry read at another key, is not moved.
    #[test]
    fn own_observation_writes_keep_the_journal_cache() {
        let dir = crate::namespace::TestStateDir(
            tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap(),
        );
        let store = crate::state::StateStore::open(dir.path()).unwrap();
        let before = store.change_key().unwrap();
        let snapshot: crate::model::Snapshot = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "node_id": crate::config::Config::default().node_id, "observed_at_unix_ms": 1,
            "cpu_capacity_millicores": 1000, "physical_cores": 1, "cpu_busy_millicores": 0,
            "total_ram_mib": 1000, "available_ram_mib": 1000, "gpu_inventory": "unsupported",
            "gpus": [], "capabilities": {}
        }))
        .unwrap();
        let decision = crate::policy::PolicyEngine::new()
            .evaluate(
                &crate::config::Config::default(),
                &snapshot,
                &crate::model::PolicyInput::default(),
                0,
            )
            .unwrap();
        store.append_observation(&snapshot, &decision).unwrap();
        let after = store.change_key().unwrap();
        assert_eq!(before.0, after.0, "no other connection committed");
        assert_ne!(before.1, after.1, "the observation moved total_changes");
        let mut slots = BTreeMap::new();
        for (id, key) in [("a", before), ("b", (before.0, before.1 - 1))] {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("cat >/dev/null")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let mut slot = slot_with(child, input);
            slot.journal_seen = Some(JournalSeen {
                key,
                record: None,
                head: Some(Some(None)),
            });
            slots.insert(id.to_string(), slot);
        }
        rekey_journal_cache(&mut slots, before, (before.0 + 1, after.1));
        assert_eq!(slots["a"].journal_seen.as_ref().unwrap().key, before);
        rekey_journal_cache(&mut slots, before, after);
        assert_eq!(slots["a"].journal_seen.as_ref().unwrap().key, after);
        assert_eq!(
            slots["b"].journal_seen.as_ref().unwrap().key,
            (before.0, before.1 - 1)
        );
    }

    /// A message to a supervisor goes out in one write call, not one per JSON token.
    #[test]
    fn a_supervisor_message_is_one_write() {
        struct Counter(std::sync::Arc<std::sync::Mutex<Vec<usize>>>);
        impl Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().push(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut slot = slot_with(child, input);
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        slot.input = Some(Box::new(Counter(writes.clone())));
        send(
            &mut slot,
            &Control::Drain {
                cause: Some(crate::contention::YieldCause::FloorBreach),
            },
        )
        .unwrap();
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "{writes:?}");
    }

    /// A drain to a closed pipe is left alone: the supervisor is gone or handles agent loss
    /// itself, so nothing is sent again and no failed send is reported; a launch burst once
    /// reported 40 such drains.
    #[test]
    fn a_drain_to_a_closed_pipe_is_not_sent_again() {
        use crate::contention::{ResourceKey, YieldCause, YieldStep};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Closed(Arc<AtomicUsize>);
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writes = Arc::new(AtomicUsize::new(0));
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut slot = slot_with(child, input);
        slot.sequence = 1;
        slot.assignment.request.class = AllocationClass::Opportunistic;
        slot.input = Some(Box::new(Closed(writes.clone())));
        let ids = vec!["a".to_string()];
        let mut slots = BTreeMap::from([("a".to_string(), slot)]);
        let drain = [crate::model::YieldAction {
            allocation_id: "a".into(),
            resource: ResourceKey::Ram,
            step: YieldStep::Drain,
            target: None,
            cause: YieldCause::FloorBreach,
            code: "yield_step_drain".into(),
            deadline_ms: None,
            duty: None,
        }];
        // In a draining stop it takes none of the iteration's share either.
        let mut quota = 1;
        assert_eq!(
            dispatch_drains(&mut slots, &ids, &drain, true, &mut quota),
            0
        );
        assert_eq!(quota, 1);
        assert!(slots["a"].drain_sent_at.is_some(), "the drain is decided");
        assert!(!slots["a"].drain_unsent, "nothing is left to send");
        assert_eq!(
            dispatch_drains(&mut slots, &ids, &drain, true, &mut quota),
            0
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1, "not sent again");
        // The same without a pipe at all.
        slots.get_mut("a").unwrap().input = None;
        slots.get_mut("a").unwrap().drain_sent_at = None;
        assert_eq!(
            dispatch_drains(&mut slots, &ids, &drain, true, &mut quota),
            0
        );
        assert!(!slots["a"].drain_unsent);
    }

    /// A policy drain whose send failed is sent again on the next pass, as what the policy
    /// asks then. Until it is delivered the slot still counts as draining, and a failed send
    /// takes none of a stop's quota.
    #[test]
    fn a_drain_that_failed_to_send_is_sent_again() {
        use crate::contention::{ResourceKey, YieldCause, YieldStep};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        struct Flaky(Arc<AtomicBool>, Arc<Mutex<Vec<String>>>);
        impl Write for Flaky {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.0.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("no buffer space"));
                }
                self.1
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(buf).into());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let slot = |fail: &Arc<AtomicBool>, lines: &Arc<Mutex<Vec<String>>>| {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("cat >/dev/null")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let mut slot = slot_with(child, input);
            slot.sequence = 1;
            slot.assignment.request.class = AllocationClass::Opportunistic;
            slot.input = Some(Box::new(Flaky(fail.clone(), lines.clone())));
            slot
        };
        let action = |id: &str, step: YieldStep, cause: YieldCause| crate::model::YieldAction {
            allocation_id: id.into(),
            resource: ResourceKey::Ram,
            step,
            target: None,
            cause,
            code: "yield_step_drain".into(),
            deadline_ms: None,
            duty: None,
        };
        let (fail, lines) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(Mutex::new(Vec::new())),
        );
        let ids = vec!["a".to_string()];
        let mut slots = BTreeMap::from([("a".to_string(), slot(&fail, &lines))]);
        let drain = [action("a", YieldStep::Drain, YieldCause::FloorBreach)];
        assert_eq!(dispatch_drains(&mut slots, &ids, &drain, false, &mut 0), 0);
        assert!(slots["a"].drain_sent_at.is_some(), "the drain is decided");
        assert!(lines.lock().unwrap().is_empty());
        fail.store(false, Ordering::SeqCst);
        // The policy now asks for a terminate: that is what goes out.
        let terminate = [action("a", YieldStep::Terminate, YieldCause::FloorBreach)];
        assert_eq!(
            dispatch_drains(&mut slots, &ids, &terminate, false, &mut 0),
            1
        );
        assert_eq!(
            dispatch_drains(&mut slots, &ids, &terminate, false, &mut 0),
            0
        );
        let sent = lines.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("terminate"), "{sent:?}");
        // In a draining stop a failed send does not use the iteration's share.
        let (broken, working) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
        );
        let written = Arc::new(Mutex::new(Vec::new()));
        let ids = vec!["s0".to_string(), "s1".to_string()];
        let mut slots = BTreeMap::from([
            ("s0".to_string(), slot(&broken, &lines)),
            ("s1".to_string(), slot(&working, &written)),
        ]);
        let stop = [
            action("s0", YieldStep::Drain, YieldCause::OperatorDrain),
            action("s1", YieldStep::Drain, YieldCause::OperatorDrain),
        ];
        assert_eq!(dispatch_drains(&mut slots, &ids, &stop, true, &mut 1), 1);
        assert_eq!(written.lock().unwrap().len(), 1);
    }

    /// A writer that counts its lines and fails with the given error kind, or succeeds.
    struct Counting(
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        Option<std::io::ErrorKind>,
    );
    impl Write for Counting {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.1 {
                Some(kind) => Err(kind.into()),
                None => Ok(bytes.len()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn counted_slot(
        fail: Option<std::io::ErrorKind>,
    ) -> (ChildSlot, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut slot = slot_with(child, input);
        slot.sequence = 1;
        slot.input = Some(Box::new(Counting(writes.clone(), fail)));
        (slot, writes)
    }

    /// A renewal reply that says drain is handed on with no time left on its lease, as a drain
    /// line, and the slot is marked as drained; to a closed pipe it is latched once (R85).
    #[test]
    fn a_renewal_replys_drain_is_handed_on_without_time_left() {
        use std::sync::atomic::Ordering;
        let lease = |sequence| crate::protocol::Lease {
            assignment_id: "assignment".into(),
            generation: 7,
            coordinator_epoch: 3,
            sequence,
            valid_for_ms: 0,
            drain: true,
        };
        let (mut slot, writes) = counted_slot(None);
        super::service::renewal_reply(
            &mut slot,
            Instant::now(),
            Ok(crate::protocol::Response::Lease { lease: lease(2) }),
        )
        .unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(slot.drain_sent_at.is_some());
        assert_eq!(
            slot.drain_cause,
            Some(crate::contention::YieldCause::CoordinatorDrain)
        );
        assert!(!slot.drain_undelivered);
        let (mut closed, _) = counted_slot(Some(std::io::ErrorKind::BrokenPipe));
        for sequence in [2, 3] {
            super::service::renewal_reply(
                &mut closed,
                Instant::now(),
                Ok(crate::protocol::Response::Lease {
                    lease: lease(sequence),
                }),
            )
            .unwrap();
        }
        assert!(closed.drain_undelivered);
    }

    /// A coordinator drain is not sent to a slot whose supervisor has exited: nothing is written,
    /// no share is taken and no drain is recorded for the run.
    #[test]
    fn a_coordinator_drain_skips_a_supervisor_that_exited() {
        use std::sync::atomic::Ordering;
        let (mut slot, writes) = counted_slot(None);
        slot.exited = true;
        let mut slots = BTreeMap::from([("a".to_string(), slot)]);
        let mut quota = 1;
        assert_eq!(
            apply_coordinator_drains(&mut slots, &["a".to_string()], &mut quota),
            0
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(quota, 1);
        assert!(slots["a"].drain_sent_at.is_none());
        assert!(slots["a"].drain_cause.is_none());
    }

    /// The stop's deadline names what is left: unanswered reports with every run released do
    /// not read as retained allocations, and unreleased allocations still say they stay charged.
    #[test]
    fn the_stop_deadline_names_what_is_left() {
        use super::service::stop_uncertain_detail;
        let reports = stop_uncertain_detail(0, 0, 11, 0);
        assert!(
            reports.contains("11 failure reports or results unanswered"),
            "{reports}"
        );
        assert!(!reports.contains("allocation"), "{reports}");
        let held = stop_uncertain_detail(2, 1, 0, 1);
        assert!(
            held.contains("2 allocations not released, which stay charged"),
            "{held}"
        );
        assert!(held.contains("1 supervisor still running"), "{held}");
        assert!(
            held.contains("1 publication or preparation in flight"),
            "{held}"
        );
    }

    /// A coordinator drain takes one of the iteration's share only when it reaches its
    /// supervisor: a closed pipe and a failed write take none.
    #[test]
    fn a_coordinator_drain_takes_a_share_only_when_delivered() {
        use std::io::ErrorKind;
        let ids: Vec<String> = ["closed", "failing", "live"].map(String::from).to_vec();
        let mut slots = BTreeMap::new();
        for (id, fail) in
            ids.iter()
                .zip([Some(ErrorKind::BrokenPipe), Some(ErrorKind::Other), None])
        {
            slots.insert(id.clone(), counted_slot(fail).0);
        }
        let mut quota = 3;
        assert_eq!(apply_coordinator_drains(&mut slots, &ids, &mut quota), 1);
        assert_eq!(quota, 2, "only the delivered drain takes a share");
        assert!(slots["failing"].drain_unsent);
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
            drain_unsent: false,
            drain_undelivered: false,
            drain_cause: None,
            started_at: Instant::now(),
            last_checkpoint_at: None,
            adjust_sequence: 0,
            sent_steps: BTreeMap::new(),
            sent_allowance: None,
            sent_oom: None,
            oom_context: None,
            ram_view: None,
            oom_verdict: None,
            seed: None,
            journal_seen: None,
            outside_family: 0,
            outside_reason: None,
            family: None,
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
            usage_unknown: None,
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
    /// A reattached agent learns the run's family and why processes are outside it from
    /// `state`, since a family that no longer changes sends no `family` report (FR-083).
    #[test]
    fn a_reattached_agent_learns_the_family_from_state() {
        let line = r#"{"event":"state","applied":{},"targets":{},"lease_sequence":3,"sequence":2,"authority":"connected","outside_family":2,"outside_reason":"ids_differ","family":{"leader":1,"sdk_children":0,"descendants":[{"pid":41,"start_time":7},{"pid":42,"start_time":8}]}}"#;
        let Event::State {
            family,
            outside_reason,
            outside_family,
            ..
        } = serde_json::from_str(line).unwrap()
        else {
            panic!("not a state event");
        };
        assert_eq!(family.unwrap().descendants.len(), 2);
        assert_eq!(
            outside_reason,
            Some(crate::supervision::OutsideReason::IdsDiffer)
        );
        assert_eq!(outside_family, 2);
    }

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

    /// A pin that finds the journal busy gives no pin and ends that iteration's pins, so one
    /// iteration waits at most one pin's bound; the next iteration tries again, and any other
    /// error still fails (FR-080, R73).
    #[cfg(unix)]
    #[test]
    fn a_busy_pin_ends_the_iterations_pins_and_nothing_else() {
        use super::service::PinGate;
        let temporary = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = temporary.path().join("state");
        let store = crate::state::StateStore::open(&root).unwrap();
        let output = root.join("attempts/attempt");
        std::fs::create_dir_all(&output).unwrap();
        let rival = rusqlite::Connection::open(store.database_path()).unwrap();
        rival.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut gate = PinGate::default();
        let pin = gate
            .take(|| crate::publication_gc::pin_attempt_for_report(&store, &output))
            .unwrap();
        assert!(pin.is_none());
        assert!(gate.found_busy());
        let mut called = false;
        let again = gate
            .take(|| {
                called = true;
                crate::publication_gc::pin_attempt_for_report(&store, &output)
            })
            .unwrap();
        assert!(again.is_none() && !called);
        rival.execute_batch("ROLLBACK").unwrap();
        let mut next = PinGate::default();
        assert!(
            next.take(|| crate::publication_gc::pin_attempt_for_report(&store, &output))
                .unwrap()
                .is_some()
        );
        let outside = root.join("elsewhere");
        assert!(
            PinGate::default()
                .take(|| crate::publication_gc::pin_attempt_for_report(&store, &outside))
                .is_err()
        );
    }

    /// Renewals are asked only within one lease of the newest sample the loop acted on: behind a
    /// locked journal ages of 0.5, 5.5, 10.5 and 15.5 s stop at the third; a new sample asks
    /// again; at the observation write every top's sample is 5 s old and keeps asking (R80).
    #[test]
    fn renewals_are_asked_only_within_one_lease_of_the_newest_acted_sample() {
        use super::service::renewals_asked;
        let lease = Duration::from_secs(10);
        for (age, asked) in [
            (0, true),
            (5_500, true),
            (10_000, true),
            (10_001, false),
            (15_500, false),
        ] {
            assert_eq!(
                renewals_asked(Duration::from_millis(age), lease),
                asked,
                "{age} ms"
            );
        }
        let locked: Vec<bool> = [500, 5_500, 10_500, 15_500, 400]
            .iter()
            .map(|&age| renewals_asked(Duration::from_millis(age), lease))
            .collect();
        assert_eq!(locked, [true, true, false, false, true]);
        for lease in [Duration::from_secs(10), Duration::from_secs(8)] {
            assert!(renewals_asked(Duration::from_secs(5), lease));
        }
    }

    /// SQLite's busy code, bare or under context, is transient; a locked table, an I/O error
    /// and a missing file are not (FR-077).
    #[test]
    fn a_locked_journal_is_transient_and_other_journal_errors_are_not() {
        let sqlite = |code: i32| {
            anyhow::Error::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        assert!(super::journal_busy(&sqlite(rusqlite::ffi::SQLITE_BUSY)));
        assert!(super::journal_busy(
            &sqlite(rusqlite::ffi::SQLITE_BUSY).context("cannot append an observation")
        ));
        assert!(!super::journal_busy(&sqlite(rusqlite::ffi::SQLITE_LOCKED)));
        assert!(!super::journal_busy(&sqlite(rusqlite::ffi::SQLITE_IOERR)));
        let missing = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!super::journal_busy(&missing));
    }

    /// A stretch of held intervals is named when it starts and not again until an interval
    /// without it has passed.
    #[test]
    fn a_busy_stretch_is_named_once() {
        let mut stretch = super::service::Stretch::default();
        let named: Vec<bool> = [false, true, true, true, false, true, false]
            .into_iter()
            .map(|busy| stretch.starts(busy))
            .collect();
        assert_eq!(named, [false, true, false, false, false, true, false]);
    }

    fn lanes(budget: u64, cap: Duration) -> super::Lanes {
        let control = budget / 10;
        super::Lanes {
            control: super::RateLimiter::new(control),
            artifacts: super::RateLimiter::new(budget - control),
            cap,
        }
    }

    /// With 150 allocations at the test's 1 MiB/s, the lease batch and its reply before the
    /// sample and the heartbeat and its reply after it are charged to the control lane, and the
    /// loop waits for none of them: the time on either side of the sample is the same as with no
    /// allocation. The lane is behind afterwards, and an artifact request still waits its turn.
    #[test]
    fn a_control_request_charges_its_lane_and_never_waits() {
        use super::wire_charge;
        let mut waited = Vec::new();
        for allocations in [0u64, 150] {
            let lanes = lanes(1_048_576, Duration::from_secs(2));
            let now = tokio::time::Instant::now();
            let mut before = Duration::ZERO;
            // Renewals: about 120 B a lease out and 160 B back, two batches at 128.
            for batch in [allocations.min(128), allocations.saturating_sub(128)] {
                before += lanes.wait_at(false, wire_charge(200 + batch * 120), now + before);
                before += lanes.wait_at(false, wire_charge(200 + batch * 160), now + before);
            }
            let mut after = Duration::ZERO;
            // The heartbeat: about 285 B an allocation's report, and its reply.
            after += lanes.wait_at(false, wire_charge(600 + allocations * 285), now + after);
            after += lanes.wait_at(false, wire_charge(400 + allocations * 40), now + after);
            waited.push((before, after));
            if allocations == 150 {
                assert!(lanes.control.behind_at(now) > Duration::ZERO);
                assert_eq!(lanes.wait_at(true, 512 * 1024, now), Duration::ZERO);
                assert!(lanes.wait_at(true, 512 * 1024, now) > Duration::ZERO);
            }
        }
        assert_eq!(waited[0], (Duration::ZERO, Duration::ZERO));
        assert_eq!(
            waited[1], waited[0],
            "the loop waited for its own control charge"
        );
    }

    /// The control lane keeps at most one heartbeat interval owed, so it does not read behind
    /// long after the load is gone.
    #[test]
    fn the_control_lane_owes_at_most_one_heartbeat_interval() {
        let now = tokio::time::Instant::now();
        let cap = Duration::from_secs(2);
        let lanes = lanes(1_048_576, cap);
        for _ in 0..50 {
            lanes.wait_at(false, 1 << 20, now);
        }
        assert_eq!(lanes.control.behind_at(now), cap);
        assert_eq!(lanes.control.behind_at(now + cap), Duration::ZERO);
    }

    /// A heartbeat goes once per heartbeat interval and every renewal in the iteration it falls
    /// due, though the control lane is behind at every heartbeat: an integer clock of 500 ms
    /// iterations, 150 allocations renewed every 2 s, a budget of 200 kB/s and 9 ms of transport
    /// a request. A request whose wait and transport pass the 500 ms budget is cut.
    #[test]
    fn heartbeats_and_due_renewals_keep_their_schedule_while_the_lane_is_behind() {
        use super::wire_charge;
        let start = tokio::time::Instant::now();
        let budget = Duration::from_millis(500);
        let transport = Duration::from_millis(9);
        let lanes = lanes(200_000, Duration::from_secs(2));
        let (mut renewals_cut, mut heartbeats_cut, mut heartbeats, mut behind) = (0, 0, 0, 0);
        for iteration in 0..120u32 {
            let now = start + budget * iteration;
            let due = (0..150).filter(|slot| slot % 4 == iteration % 4).count() as u64;
            let mut spent = lanes.wait_at(false, wire_charge(200 + due * 120), now) + transport;
            spent += lanes.wait_at(false, wire_charge(200 + due * 160), now + spent);
            if spent > budget {
                renewals_cut += 1;
            }
            if iteration % 4 == 0 {
                heartbeats += 1;
                if lanes.control.behind_at(now) > Duration::ZERO {
                    behind += 1;
                }
                let mut spent = lanes.wait_at(false, wire_charge(600 + 150 * 285), now) + transport;
                spent += lanes.wait_at(false, wire_charge(400 + 150 * 40), now + spent);
                if spent > budget {
                    heartbeats_cut += 1;
                }
            }
        }
        assert_eq!(heartbeats, 30);
        assert!(
            behind >= 25,
            "the lane was behind at only {behind} heartbeats"
        );
        assert_eq!((renewals_cut, heartbeats_cut), (0, 0));
    }

    /// While the control lane is behind, failure reports keep 16 a second; caught up, the loop's
    /// share alone decides, and the share always ends an iteration.
    #[test]
    fn failure_reports_keep_sixteen_a_second_while_the_lane_is_behind() {
        use super::service::{failure_floor, failure_turn};
        assert_eq!(failure_floor(Duration::from_millis(500)), 8);
        assert_eq!(failure_floor(Duration::from_millis(400)), 7);
        assert_eq!(failure_floor(Duration::from_millis(10)), 1);
        let budget = Duration::from_millis(250);
        let behind = Duration::from_millis(800);
        let early = Duration::from_millis(20);
        assert!(failure_turn(3, early, budget, 3, 8, behind));
        assert!(!failure_turn(8, early, budget, 8, 8, behind));
        assert!(failure_turn(8, early, budget, 8, 8, Duration::ZERO));
        assert!(!failure_turn(3, budget, budget, 3, 8, Duration::ZERO));
        assert!(failure_turn(0, budget, budget, 0, 8, behind));
    }

    /// A stopping agent heartbeats at its interval, not every iteration.
    #[test]
    fn a_stopping_agent_keeps_the_heartbeat_interval() {
        use super::service::heartbeat_due;
        let interval = Duration::from_secs(2);
        assert!(!heartbeat_due(Duration::from_millis(100), interval, false));
        assert!(heartbeat_due(interval, interval, false));
    }

    /// When room opens, the next heartbeat comes as soon as 250 ms after the last, so the
    /// coordinator places work without waiting out the interval (006 SC-001, SC-004).
    #[test]
    fn opened_room_brings_the_heartbeat_forward() {
        use super::service::{heartbeat_due, room_opened};
        let interval = Duration::from_secs(2);
        assert!(!heartbeat_due(Duration::from_millis(100), interval, true));
        assert!(heartbeat_due(Duration::from_millis(250), interval, true));
        assert!(!heartbeat_due(Duration::from_millis(250), interval, false));
        // A plain user starting to compete, or CPU room growing by half a core, opens room;
        // readings that move by less do not, so a steady host heartbeats at its interval.
        assert!(room_opened(false, true, 1000, 1000));
        assert!(room_opened(false, false, 1000, 1500));
        assert!(!room_opened(false, false, 1000, 1499));
        assert!(!room_opened(true, true, 1000, 1200));
        assert!(!room_opened(false, false, 1500, 1000));
    }

    /// A slot whose supervisor exited runs nothing, so it frees its worker place at once, before
    /// its release is reported. A Performance node that just filled to its admitted
    /// parallelism then takes the replacement at the next heartbeat, not after its growth
    /// cooldown (006 SC-004).
    #[test]
    fn an_exited_slot_frees_its_place_before_its_release_is_reported() {
        use super::service::{LaunchPacing, active_worker_count, launch_slots};
        // Three slots running, one exited and not yet reported, one reported, no preparation.
        let slots = [
            (false, false),
            (false, false),
            (false, false),
            (false, true),
            (true, true),
        ];
        assert_eq!(active_worker_count(slots, 0), 3);
        assert_eq!(active_worker_count(slots, 2), 5);
        let pacing = |active| {
            launch_slots(LaunchPacing {
                max_workers: 16,
                active,
                admitted: 4,
                may_grow: false,
                fill: true,
            })
        };
        assert_eq!(pacing(active_worker_count(slots, 0)), 1);
        assert_eq!(pacing(4), 0);
    }

    /// A released run's failure report checks its turn before the attempt pin: a failure with
    /// no result descriptor and no turn takes no pin. A success, a descriptor, a head or an
    /// outcome that could not be read, or a turn, does, and every read is made again under it.
    #[test]
    fn a_failure_report_checks_its_turn_before_the_attempt_pin() {
        use super::service::pins_result;
        assert!(!pins_result(Some(true), Some(false), false));
        assert!(pins_result(Some(true), Some(false), true));
        assert!(pins_result(Some(false), Some(false), false));
        assert!(pins_result(Some(true), Some(true), false));
        assert!(pins_result(Some(true), None, false));
        assert!(pins_result(None, Some(false), false));
    }
}

#[cfg(all(test, unix))]
mod descriptor_tests {
    use super::*;

    /// 8 per launch on top of what is open, with 64 kept free (FR-077).
    #[test]
    fn launches_fit_with_eight_descriptors_each_and_sixty_four_free() {
        assert_eq!(launches_that_fit(40, 1024, 0), 115);
        assert_eq!(launches_that_fit(40, 1024, 5), 110);
        assert_eq!(launches_that_fit(40, 120, 0), 2);
        assert_eq!(launches_that_fit(40, 120, 2), 0);
        assert_eq!(launches_that_fit(10, 72, 0), 0);
        assert_eq!(launches_that_fit(2000, 1024, 0), 0);
    }

    /// EMFILE and ENFILE anywhere in an error's chain are descriptor exhaustion.
    #[test]
    fn emfile_and_enfile_are_descriptor_exhaustion() {
        for code in [libc::EMFILE, libc::ENFILE] {
            let error = anyhow::Error::from(std::io::Error::from_raw_os_error(code))
                .context("read /proc/stat");
            assert!(descriptor_exhausted(&error), "{code}");
        }
        let other = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert!(!descriptor_exhausted(&other));
        assert!(!descriptor_exhausted(&anyhow::anyhow!("no descriptors")));
    }

    /// A full disk or quota skips the interval like descriptor exhaustion; a missing file,
    /// which can mean a lost namespace, still ends the loop.
    #[test]
    fn a_full_disk_skips_the_interval_and_a_missing_file_does_not() {
        for code in [libc::ENOSPC, libc::EDQUOT] {
            let error = anyhow::Error::from(std::io::Error::from_raw_os_error(code))
                .context("write refusal proof");
            assert!(storage_exhausted(&error), "{code}");
        }
        let missing = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert!(!storage_exhausted(&missing));
    }
}
