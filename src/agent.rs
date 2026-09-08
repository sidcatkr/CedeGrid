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
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

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
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Prepared { record: ExecutionRecord },
    Finished { outcome: ExecutionOutcome },
    Failed { detail: String },
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
    Drain,
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
struct PipeControl {
    rx: mpsc::Receiver<std::result::Result<Control, String>>,
    assignment: Assignment,
    config: Config,
    capacity: Resources,
    store: StateStore,
    collector: ManagedCollector,
    policy: PolicyEngine,
    last_sample: Instant,
    start: Instant,
    authorized_at: Option<Instant>,
    remaining_ms: Option<u64>,
    agent_lost: bool,
}
impl SupervisorControl for PipeControl {
    fn prepared(&mut self, record: &ExecutionRecord) -> Result<()> {
        self.collector.prime(&self.store.executions()?);
        // Establish a local observation baseline while user code is still gated.
        // The coordinator's sample is never substituted for this supervisor's check.
        let mut snapshot = self
            .collector
            .sample_with_children(
                &self.store.executions()?,
                &self.store.all_unreleased_managed_children()?,
            )?
            .0;
        if !self.assignment.request.resources.gpu_memory_mib.is_empty()
            && self.config.gpu.execution_mode == crate::config::GpuExecutionMode::ContentionAware
        {
            ensure!(
                self.config.monitor.interval_ms < self.config.execution.prepare_timeout_ms,
                "GPU baseline interval cannot fit inside preparation deadline"
            );
            std::thread::sleep(Duration::from_millis(self.config.monitor.interval_ms));
            snapshot = self
                .collector
                .sample_with_children(
                    &self.store.executions()?,
                    &self.store.all_unreleased_managed_children()?,
                )?
                .0;
        }
        validate_gpu_launch_contract(
            &self.config,
            &snapshot,
            &self.assignment.request.resources,
            self.assignment.request.class,
        )?;
        emit(&Event::Prepared {
            record: record.clone(),
        })?;
        match self.rx.recv_timeout(Duration::from_millis(
            self.config.execution.prepare_timeout_ms,
        )) {
            Ok(Ok(Control::Authorize {
                lease,
                expires_monotonic_ms,
            })) => {
                let remaining_ms = expires_monotonic_ms.saturating_sub(local_clock_ms()?);
                validate_lease(&lease, &self.assignment)?;
                ensure!(
                    !lease.drain && remaining_ms > 0 && remaining_ms <= lease.valid_for_ms,
                    "authorization expired or draining"
                );
                // Admission may have changed while awaiting remote authorization.
                // A newly visible external context closes the non-sharing gate.
                let snapshot = self
                    .collector
                    .sample_with_children(
                        &self.store.executions()?,
                        &self.store.all_unreleased_managed_children()?,
                    )?
                    .0;
                validate_gpu_launch_contract(
                    &self.config,
                    &snapshot,
                    &self.assignment.request.resources,
                    self.assignment.request.class,
                )?;
                let remaining_ms = expires_monotonic_ms.saturating_sub(local_clock_ms()?);
                ensure!(
                    remaining_ms > 0,
                    "authorization expired during telemetry verification"
                );
                self.remaining_ms = Some(remaining_ms);
                self.authorized_at = Some(Instant::now());
                // The authorization recheck just refreshed this collector.
                // Starting the periodic clock from preparation would sample it
                // again immediately, before its CPU accounting window advances.
                self.last_sample = Instant::now();
                Ok(())
            }
            _ => bail!("agent lost or execution authorization absent; gate stays blocked"),
        }
    }
    fn initial_remaining_ms(&self) -> Option<u64> {
        self.remaining_ms.map(|r| {
            r.saturating_sub(
                self.authorized_at
                    .map_or(0, |at| at.elapsed().as_millis() as u64),
            )
        })
    }
    fn authorize_managed_child(
        &mut self,
        request: &LaunchRequest,
    ) -> Result<Vec<SupervisorCommand>> {
        if !request.resources.gpu_memory_mib.is_empty() {
            let snapshot = self
                .collector
                .sample_with_children(
                    &self.store.executions()?,
                    &self.store.all_unreleased_managed_children()?,
                )?
                .0;
            validate_gpu_launch_contract(
                &self.config,
                &snapshot,
                &request.resources,
                request.class,
            )?;
        }
        // Read cancellation and fresh grants after the potentially slow driver
        // query without taking another sample that supersedes this launch check.
        self.pending_commands()
    }
    fn poll(&mut self) -> Result<Vec<SupervisorCommand>> {
        let mut commands = self.pending_commands()?;
        if self.last_sample.elapsed() >= Duration::from_millis(self.config.monitor.interval_ms) {
            self.last_sample = Instant::now();
            let records = self.store.executions()?;
            let (mut snapshot, allocations) = self
                .collector
                .sample_with_children(&records, &self.store.all_unreleased_managed_children()?)?;
            restrict_gpu_scope(&mut snapshot, &self.capacity);
            let envelope_exceeded = !allocations_fit(&allocations, &self.capacity);
            let decision = self.policy.evaluate(
                &self.config,
                &snapshot,
                &PolicyInput {
                    allocations: allocations.clone(),
                    explicit_drain: false,
                },
                self.start.elapsed().as_millis() as u64,
            )?;
            if envelope_exceeded
                || (self.assignment.request.class == AllocationClass::Opportunistic
                    && decision
                        .would_drain
                        .contains(&self.assignment.request.assignment_id))
            {
                eprintln!(
                    "{}",
                    serde_json::json!({"event":"local_policy_drain","assignment_id":self.assignment.request.assignment_id,"envelope_exceeded":envelope_exceeded,"capacity":self.capacity,"allocations":allocations,"decision":decision})
                );
                commands.push(SupervisorCommand::Drain);
            }
        }
        Ok(commands)
    }
}
impl PipeControl {
    fn pending_commands(&mut self) -> Result<Vec<SupervisorCommand>> {
        let mut commands = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Ok(Control::Renewal {
                    lease,
                    expires_monotonic_ms,
                })) => {
                    let remaining_ms = expires_monotonic_ms.saturating_sub(local_clock_ms()?);
                    if validate_lease(&lease, &self.assignment).is_ok()
                        && remaining_ms > 0
                        && remaining_ms <= lease.valid_for_ms
                    {
                        self.assignment.coordinator_epoch = lease.coordinator_epoch;
                        if lease.drain {
                            commands.push(SupervisorCommand::Drain)
                        } else {
                            commands.push(SupervisorCommand::Renew {
                                assignment_id: lease.assignment_id,
                                generation: lease.generation,
                                sequence: lease.sequence,
                                remaining_ms,
                            })
                        }
                    }
                }
                Ok(Ok(Control::Drain)) => commands.push(SupervisorCommand::Drain),
                Ok(Ok(Control::Authorize { .. })) => {}
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.agent_lost {
                        commands.push(SupervisorCommand::AgentLost);
                        self.agent_lost = true;
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        Ok(commands)
    }
}
fn validate_lease(lease: &Lease, assignment: &Assignment) -> Result<()> {
    ensure!(
        lease.assignment_id == assignment.request.assignment_id
            && lease.generation == assignment.generation
            && lease.coordinator_epoch >= assignment.coordinator_epoch
            && lease.sequence > 0,
        "lease identity/epoch mismatch"
    );
    Ok(())
}
/// Called before a Tokio runtime. Reader thread only handles the inherited pipe;
/// the main thread remains the exclusive child reaper throughout pidfd acquisition.
pub fn assignment_supervisor(spec_path: &Path) -> Result<()> {
    let lifecycle_started = Instant::now();
    let state_root = PathBuf::from(
        std::env::var_os("CEDEGRID_SUPERVISOR_STATE_ROOT")
            .context("native supervisor namespace bootstrap missing")?,
    );
    let expected: NamespaceIdentity = serde_json::from_str(
        &std::env::var("CEDEGRID_SUPERVISOR_NAMESPACE")
            .context("native supervisor identity bootstrap missing")?,
    )?;
    let namespace_guard =
        Namespace::new(&state_root)?.acquire(Some(&expected), Duration::from_secs(30))?;
    let spec: SupervisorSpec = serde_json::from_slice(&crate::publication::read_bounded(
        spec_path,
        3 * 1024 * 1024,
    )?)?;
    namespace_guard.validate_root(&spec.config.state_dir)?;
    ensure!(spec.config.execution.enabled, "execution opt-in missing");
    validate_replay_launch(&spec.config, &spec.assignment.request)?;
    let supervisor_identity = supervision::process_identity(
        std::process::id(),
        &spec.assignment.request.assignment_id,
        spec.assignment.generation,
    )?;
    write_json_new(
        &spec.output_dir.join("supervisor-identity.json"),
        &supervisor_identity,
    )?;
    if let Some(cpus) = spec.assignment.request.env.get("CEDEGRID_CPU_AFFINITY") {
        supervision::apply_current_cpu_affinity(&serde_json::from_str::<Vec<u32>>(cpus)?)?;
    }

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(std::io::stdin()).lines() {
            let message = line
                .map_err(|e| e.to_string())
                .and_then(|l| serde_json::from_str(&l).map_err(|e| e.to_string()));
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    let mut backend = execution_backend(&spec.config, &spec.assignment.request)?;
    let journal = AssignedJournal {
        store: open_existing_node_store_guarded(&spec.config, namespace_guard.clone())?,
        generation: spec.assignment.generation,
    };
    let start = Instant::now();
    let mut control = PipeControl {
        rx,
        assignment: spec.assignment.clone(),
        config: spec.config.clone(),
        capacity: spec
            .envelope
            .clone()
            .unwrap_or_else(|| spec.capacity.clone()),
        store: open_existing_node_store_guarded(&spec.config, namespace_guard.clone())?,
        collector: ManagedCollector::new(&spec.config)?,
        policy: PolicyEngine::new(),
        last_sample: start,
        start,
        authorized_at: None,
        remaining_ms: None,
        agent_lost: false,
    };
    let options = SupervisorOptions {
        nice: spec.config.cpu.nice,
        drain_timeout_ms: spec.config.lifecycle.drain_timeout_ms,
        term_grace_ms: spec.config.lifecycle.term_grace_ms,
        lease_ms: spec.config.lifecycle.allocation_lease_ms,
        prepare_timeout_ms: spec.config.execution.prepare_timeout_ms,
        release_confirm_timeout_ms: spec.config.execution.release_confirm_timeout_ms,
    };
    let result = supervision::supervise_controlled(
        &spec.assignment.request,
        &spec.capacity,
        &options,
        &journal,
        backend.as_mut(),
        &std::env::current_exe()?,
        &mut control,
    );
    let result = match result {
        Ok(outcome) => {
            write_json_new(&spec.output_dir.join("execution-outcome.json"), &outcome)?;
            emit(&Event::Finished { outcome })
        }
        Err(error) => {
            let detail = format!("{error:#}");
            let _ = write_json_new(
                &spec.output_dir.join("execution-error.json"),
                &serde_json::json!({"detail":detail}),
            );
            emit(&Event::Failed { detail })?;
            Err(error)
        }
    };
    write_supervisor_usage(&spec.output_dir, &supervisor_identity, lifecycle_started)?;
    result
}

fn write_supervisor_usage(
    output: &Path,
    identity: &ProcessIdentity,
    started: Instant,
) -> Result<()> {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0,
            "supervisor self resource accounting unavailable"
        );
        let user = usage.ru_utime.tv_sec as i128 * 1_000_000 + usage.ru_utime.tv_usec as i128;
        let system = usage.ru_stime.tv_sec as i128 * 1_000_000 + usage.ru_stime.tv_usec as i128;
        ensure!(
            user >= 0 && system >= 0 && usage.ru_maxrss >= 0,
            "invalid supervisor resource accounting"
        );
        let peak_rss = usage.ru_maxrss as u64;
        #[cfg(not(target_os = "macos"))]
        let peak_rss = peak_rss
            .checked_mul(1024)
            .context("supervisor RSS overflow")?;
        write_json_new(
            &output.join("supervisor-usage.json"),
            &serde_json::json!({
                "schema_version":1,"identity":identity,"scope":"supervisor_self_excludes_workers",
                "runtime_os":std::env::consts::OS,"user_cpu_us":u64::try_from(user)?,"system_cpu_us":u64::try_from(system)?,
                "peak_rss_bytes":peak_rss,"elapsed_since_entry_ms":started.elapsed().as_millis() as u64,
                "observed_monotonic_ms":local_clock_ms()?,"cutoff":"after workload outcome/event publication, before this accounting file and process exit"
            }),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (output, identity, started);
        bail!("supervisor self accounting unsupported")
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
                let mut backend = crate::rootless::RootlessBackend::new(config.cpu.nice);
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
        config.cpu.nice,
    )))
}
struct ChildSlot {
    child: Child,
    input: ChildStdin,
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
}
impl Drop for ChildSlot {
    fn drop(&mut self) {
        // Native output work must finish before the slot releases its namespace.
        // Drain uses the authenticated owned control pipe and never signals a PID.
        let _ = send(self, &Control::Drain);
        let _ = self.child.wait();
        for thread in self.capture_threads.drain(..) {
            let _ = thread.join();
        }
    }
}
fn send(slot: &mut ChildSlot, message: &Control) -> Result<()> {
    serde_json::to_writer(&mut slot.input, message)?;
    slot.input.write_all(b"\n")?;
    slot.input.flush()?;
    Ok(())
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

/// Reconciliation never sends signals. A different boot or absent/reused leader
/// proves release only under the persisted single-process rootless contract.
pub fn reconcile(config: &Config) -> Result<Vec<ExecutionRecord>> {
    ensure!(
        !config.storage_profile.is_replayable(),
        "replayable node recovery requires an authenticated agent session and authoritative inventory"
    );
    ensure!(
        config
            .state_dir
            .join(crate::state::DATABASE_FILENAME)
            .is_file(),
        "no existing node state to reconcile"
    );
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.state_dir.join("agent.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("cannot reconcile while a live agent owns node state")?;
    reconcile_unlocked(config, &BTreeSet::new())
}
#[cfg(target_os = "macos")]
fn native_pid_absent(pid: u32) -> bool {
    // Query only: ESRCH from libproc proves that this PID is absent.
    // Permission errors or unavailable boot telemetry remain uncertain.
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: initialized storage of the exact libproc ABI size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as i32,
        )
    };
    read == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
pub(crate) fn identity_absent(id: &ProcessIdentity) -> bool {
    match supervision::process_identity(id.pid, &id.assignment_id, id.generation) {
        Ok(current) => {
            if current != *id {
                return true;
            }
            #[cfg(target_os = "linux")]
            {
                fs::read_to_string(format!("/proc/{}/status", id.pid)).is_ok_and(|s| {
                    s.lines().any(|l| {
                        l.starts_with("State:")
                            && l.split_whitespace()
                                .nth(1)
                                .is_some_and(|v| matches!(v, "Z" | "X"))
                    })
                })
            }
            #[cfg(target_os = "macos")]
            {
                native_pid_absent(id.pid)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                false
            }
        }
        Err(_) => {
            #[cfg(target_os = "linux")]
            {
                fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .is_ok_and(|b| b.trim() != id.boot_id)
                    || matches!(fs::metadata(format!("/proc/{}",id.pid)),Err(e) if e.kind()==std::io::ErrorKind::NotFound)
            }
            #[cfg(target_os = "macos")]
            {
                native_pid_absent(id.pid)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                false
            }
        }
    }
}
pub(crate) fn recovery_allows_release(record: &ExecutionRecord) -> bool {
    // A lost journal may have omitted a gated child or mediated descendants.
    // The leader's absence alone cannot prove that this family is gone.
    let incomplete = record.evidence.iter().any(|e| {
        matches!(
            e.control.as_str(),
            "recovery.unknown_children" | "recovery.unauthorized"
        )
    });
    if !incomplete {
        return true;
    }
    record.identity.as_ref().is_some_and(|old| {
        supervision::process_identity(std::process::id(), "recovery", 0)
            .is_ok_and(|current| current.boot_id != old.boot_id)
    })
}
fn reconcile_unlocked(config: &Config, exclude: &BTreeSet<String>) -> Result<Vec<ExecutionRecord>> {
    let guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    reconcile_guarded(config, exclude, guard)
}
fn reconcile_guarded(
    config: &Config,
    exclude: &BTreeSet<String>,
    guard: NamespaceGuard,
) -> Result<Vec<ExecutionRecord>> {
    crate::backup::ensure_runnable_state(&config.state_dir)?;
    let store = open_existing_node_store_guarded(config, guard)?;
    for mut record in store.executions()? {
        if record.phase == ExecutionPhase::Released || exclude.contains(&record.assignment_id) {
            continue;
        }
        let request = store.execution_request(&record.assignment_id)?;
        if config.storage_profile.is_replayable()
            && request.managed_child_limit > 0
            && !record
                .evidence
                .iter()
                .any(|e| e.control == "recovery.unknown_children")
        {
            record.evidence.push(ControlEvidence {
                control: "recovery.unknown_children".into(), available: None, permitted: None,
                configured: true, applied: false, fallback: false, scope: "recovery".into(),
                requested: None, effective: None,
                detail: "Replayable child registry may have rolled back; without a live owning supervisor, same-boot family absence is unverified".into(),
            });
        }
        let mut released = false;
        if (request.single_process
            || (request.no_escape && (1..=8).contains(&request.managed_child_limit)))
            && record.backend == "rootless"
            && let Some(id) = &record.identity
        {
            let absent = identity_absent(id);
            if absent && recovery_allows_release(&record) {
                released = crate::telemetry::GpuReleaseGuard::prepare(&record.resources)
                    .and_then(|g| {
                        g.confirm(
                            id.pid,
                            Duration::from_millis(config.execution.release_confirm_timeout_ms),
                        )
                    })
                    .is_ok();
            }
        }
        for child in store.managed_children(&record.assignment_id)? {
            use crate::managed_children::ManagedChildPhase;
            if child.phase == ManagedChildPhase::Released {
                continue;
            }
            let gone = child.identity.as_ref().is_some_and(|id| {
                identity_absent(id)
                    && crate::telemetry::GpuReleaseGuard::prepare(&record.resources)
                        .and_then(|g| {
                            g.confirm(
                                id.pid,
                                Duration::from_millis(config.execution.release_confirm_timeout_ms),
                            )
                        })
                        .is_ok()
            });
            store.transition_managed_child(&child.child_id,if gone {ManagedChildPhase::Released}else{ManagedChildPhase::NeedsReconciliation},None,None,
                if gone {"Read-only reconciliation verified original child absent and GPU context released"}else{"Supervisor handle lost; child identity/release unconfirmed; family reservation retained"})?;
            released &= gone;
        }
        record.phase = if released {
            ExecutionPhase::Released
        } else {
            ExecutionPhase::NeedsReconciliation
        };
        record.detail = if released {
            "read-only reconciliation proved original rootless leader and all registered mediated children absent, with GPU contexts released".into()
        } else {
            "supervisor ownership unavailable; allocation retained until identity/backend release is proven".into()
        };
        store.transition(&record)?;
    }
    store.executions()
}
async fn download_checkpoint(
    client: &NodeClient,
    assignment: &Assignment,
    output: &Path,
) -> Result<serde_json::Value> {
    let Some(checkpoint) = &assignment.checkpoint else {
        return Ok(serde_json::Value::Null);
    };
    let directory = output.join("resume");
    private_dir(&directory)?;
    let mut files = Vec::new();
    for artifact in &checkpoint.artifacts {
        ensure!(
            artifact.size <= 256 * 1024 * 1024,
            "resume artifact exceeds bounded transfer"
        );
        let path = directory.join(&artifact.sha256);
        ensure!(
            artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid artifact identity"
        );
        download_verified(client, artifact, &path).await?;
        let name = checkpoint.result["artifacts"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["sha256"] == artifact.sha256))
            .and_then(|v| v["name"].as_str())
            .unwrap_or(&artifact.sha256);
        files.push(serde_json::json!({"name":name,"sha256":artifact.sha256,"size":artifact.size,"path":path}));
    }
    File::open(&directory)?.sync_all()?;
    Ok(
        serde_json::json!({"metadata":checkpoint.result["metadata"],"artifacts":files,"checkpoint_sequence":checkpoint.result["checkpoint_sequence"]}),
    )
}
async fn download_verified(client: &NodeClient, artifact: &ArtifactRef, path: &Path) -> Result<()> {
    let charge = client.download_charge(path)?;
    // Reserve the entire download before creating or extending its file. The
    // same transaction ledger is used by concurrent native worker publishers.
    charge.reserve(artifact.size)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut offset = 0u64;
    while offset < artifact.size {
        let Response::Chunk { data_hex, eof } = client
            .request(&Request::ReadArtifact {
                sha256: artifact.sha256.clone(),
                offset,
                max_bytes: 128 * 1024,
            })
            .await?
        else {
            bail!("invalid artifact response");
        };
        let bytes = hex::decode(data_hex)?;
        ensure!(
            !bytes.is_empty() && offset + bytes.len() as u64 <= artifact.size,
            "invalid resumed artifact length"
        );
        file.write_all(&bytes)?;
        offset += bytes.len() as u64;

        if eof {
            break;
        }
    }
    ensure!(offset == artifact.size, "incomplete checkpoint artifact");
    file.sync_all()?;
    File::open(path.parent().context("download parent missing")?)?.sync_all()?;
    ensure!(
        sha256_file(path)?.0 == artifact.sha256,
        "checkpoint integrity mismatch"
    );
    Ok(())
}
async fn materialize_cached(
    client: &NodeClient,
    artifact: &ArtifactRef,
    path: &Path,
) -> Result<()> {
    let _lock = client.cache_lock.lock().await;
    let cached = client.cache_dir.join(&artifact.sha256);
    if cached.exists() {
        ensure!(
            fs::symlink_metadata(&cached)?.file_type().is_file()
                && sha256_file(&cached)? == (artifact.sha256.clone(), artifact.size),
            "cached immutable input integrity mismatch"
        );
    } else {
        // Remove only manager cache files with no live attempt hardlinks. Active
        // models and all checkpoint/result storage are outside this eviction rule.
        for entry in fs::read_dir(&client.cache_dir)? {
            if spool_size(&client.spool_root)?.saturating_add(artifact.size)
                <= client.max_spool_bytes
            {
                break;
            }
            let entry = entry?;
            #[cfg(not(unix))]
            let _ = &entry;
            #[cfg(unix)]
            {
                let meta = entry.metadata()?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                use std::os::unix::fs::MetadataExt;
                if meta.is_file()
                    && meta.nlink() == 1
                    && name.len() == 64
                    && name.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    fs::remove_file(entry.path())?;
                    client.download_charge(&entry.path())?.reclaim_removed()?;
                }
            }
        }
        ensure!(
            spool_size(&client.spool_root)?.saturating_add(artifact.size) <= client.max_spool_bytes,
            "new model exceeds node spool budget with active content retained"
        );
        let temporary = client
            .cache_dir
            .join(format!(".download-{}", uuid::Uuid::new_v4()));
        if let Err(error) = download_verified(client, artifact, &temporary).await {
            if fs::remove_file(&temporary).is_ok() {
                client.download_charge(&temporary)?.reclaim_removed()?;
            }
            return Err(error);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o400))?;
        }
        fs::hard_link(&temporary, &cached)?;
        fs::remove_file(&temporary)?;
        File::open(&client.cache_dir)?.sync_all()?;
        client.download_charge(&temporary)?.reclaim_removed()?;
    }
    let charge = client.download_charge(path)?;
    charge.reserve(artifact.size)?;
    fs::hard_link(&cached, path)?;
    File::open(path.parent().context("input parent missing")?)?.sync_all()?;
    Ok(())
}
async fn download_inputs(
    client: &NodeClient,
    assignment: &Assignment,
    output: &Path,
) -> Result<Vec<serde_json::Value>> {
    let directory = output.join("inputs");
    private_dir(&directory)?;
    let mut files = Vec::new();
    let mut downloaded = BTreeSet::new();
    for input in &assignment.request.input_artifacts {
        ensure!(
            input.size <= 256 * 1024 * 1024
                && input.sha256.len() == 64
                && input.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid or oversized named input"
        );
        let path = directory.join(&input.sha256);
        if downloaded.insert(input.sha256.clone()) {
            materialize_cached(
                client,
                &ArtifactRef {
                    sha256: input.sha256.clone(),
                    size: input.size,
                },
                &path,
            )
            .await?;
        } else {
            ensure!(
                sha256_file(&path)? == (input.sha256.clone(), input.size),
                "duplicate input identity has conflicting size"
            );
        }
        files.push(serde_json::json!({"name":input.name,"path":path,"sha256":input.sha256,"size":input.size}));
    }
    File::open(&directory)?.sync_all()?;
    File::open(output)?.sync_all()?;
    Ok(files)
}
async fn start_assignment(
    config: &Config,
    agent: &AgentConfig,
    assignment: Assignment,
    capacity: &Resources,
    executable: &Path,
    client: &NodeClient,
) -> Result<ChildSlot> {
    validate_replay_launch(config, &assignment.request)?;
    ensure!(
        !config.storage_profile.is_replayable() || client.replay_session.is_some(),
        "replayable launch requires an authenticated coordinator session"
    );
    ensure!(assignment.node_id == config.node_id, "offer node mismatch");
    ensure!(
        config.node_mode == NodeMode::Guaranteed
            || assignment.request.class == AllocationClass::Opportunistic,
        "guaranteed work on opportunistic node refused"
    );
    let mut assignment = assignment;
    ensure!(
        !assignment.request.assignment_id.is_empty()
            && assignment
                .request
                .assignment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "unsafe assignment identity"
    );
    let output = config
        .state_dir
        .join("attempts")
        .join(&assignment.request.assignment_id);
    ensure!(
        !output.exists(),
        "assignment output already exists; reconciliation required"
    );
    let input_bytes = assignment
        .request
        .input_artifacts
        .iter()
        .filter(|artifact| !client.cache_dir.join(&artifact.sha256).exists())
        .try_fold(0u64, |sum, a| sum.checked_add(a.size))
        .context("input byte count overflow")?;
    let resume_bytes = assignment
        .checkpoint
        .as_ref()
        .map(|c| {
            c.artifacts
                .iter()
                .try_fold(0u64, |sum, a| sum.checked_add(a.size))
        })
        .unwrap_or(Some(0))
        .context("resume byte count overflow")?;
    ensure!(
        input_bytes
            .checked_add(resume_bytes)
            .and_then(|n| n.checked_add(spool_size(&config.state_dir.join("attempts")).ok()?))
            .is_some_and(|n| n <= agent.max_spool_bytes),
        "named inputs and resume exceed remaining node spool budget"
    );
    private_dir(&output)?;
    let workspace = Namespace::new(&config.state_dir)?
        .control_dir()
        .join("workspaces")
        .join(&client.namespace_guard.identity().namespace_id)
        .join(&assignment.request.assignment_id);
    ensure!(!workspace.exists(), "attempt workspace already exists");
    private_dir(&workspace)?;
    for name in ["output", "tmp", "cache"] {
        private_dir(&workspace.join(name))?;
    }
    let inputs = download_inputs(client, &assignment, &workspace).await?;
    let resume = download_checkpoint(client, &assignment, &workspace).await?;
    let metadata = assignment
        .request
        .env
        .get("CEDEGRID_METADATA")
        .map(|s| serde_json::from_str::<serde_json::Value>(s))
        .transpose()?
        .unwrap_or(serde_json::json!({}));
    let context = serde_json::json!({"schema_version":2,"namespace_id":client.namespace_guard.identity().namespace_id,"session_id":client.namespace_guard.identity().session_id.as_ref().unwrap_or(&client.namespace_guard.identity().namespace_id),"task_id":assignment.request.task_id,"assignment_id":assignment.request.assignment_id,"generation":assignment.generation,"output_dir":workspace.join("output"),"metadata":metadata,"resume":resume,"inputs":inputs,"max_spool_bytes":agent.max_spool_bytes});
    client.write_json_new(&workspace.join("context.json"), &context)?;
    assignment.request.env.insert(
        "CEDEGRID_CONTEXT".into(),
        workspace.join("context.json").display().to_string(),
    );
    assignment.request.env.insert(
        "CEDEGRID_OUTPUT_DIR".into(),
        workspace.join("output").display().to_string(),
    );
    assignment
        .request
        .env
        .insert("TMPDIR".into(), workspace.join("tmp").display().to_string());
    assignment.request.env.insert(
        "XDG_CACHE_HOME".into(),
        workspace.join("cache").display().to_string(),
    );
    if let Some(cpus) = &agent.cpu_affinity {
        assignment
            .request
            .env
            .insert("CEDEGRID_CPU_AFFINITY".into(), serde_json::to_string(cpus)?);
    } else {
        assignment.request.env.remove("CEDEGRID_CPU_AFFINITY");
    }
    if !assignment.request.resources.gpu_memory_mib.is_empty() {
        ensure!(
            assignment.request.resources.gpu_memory_mib.len() == 1,
            "v1 GPU worker requires one explicitly selected UUID"
        );
        assignment.request.env.insert(
            "CUDA_VISIBLE_DEVICES".into(),
            assignment
                .request
                .resources
                .gpu_memory_mib
                .keys()
                .next()
                .unwrap()
                .clone(),
        );
    } else {
        assignment
            .request
            .env
            .insert("CUDA_VISIBLE_DEVICES".into(), String::new());
    }
    let native_publication = crate::publication::NativePublisherSettings {
        state_dir: config.state_dir.clone(),
        storage_profile: config.storage_profile,
        namespace_id: client.namespace_guard.identity().namespace_id.clone(),
        session_id: client
            .namespace_guard
            .identity()
            .session_id
            .clone()
            .unwrap_or_else(|| client.namespace_guard.identity().namespace_id.clone()),
        output_dir: output.clone(),
        task_id: assignment.request.task_id.clone(),
        assignment_id: assignment.request.assignment_id.clone(),
        generation: assignment.generation,
        max_spool_bytes: agent.max_spool_bytes,
        max_artifact_bytes: 256 * 1024 * 1024,
    };
    assignment.request.env.insert(
        "CEDEGRID_PUBLICATION_SPEC".into(),
        serde_json::to_string(&native_publication)?,
    );
    let spec = SupervisorSpec {
        config: config.clone(),
        assignment: assignment.clone(),
        capacity: capacity.clone(),
        envelope: Some(agent.capacity.clone()),
        output_dir: output.clone(),
    };
    let path = output.join("supervisor-spec.json");
    client.write_json_new(&path, &spec)?;
    let mut child = Command::new(executable)
        .arg("__assignment-supervisor")
        .arg(path)
        .env("TMPDIR", workspace.join("tmp"))
        .env("CEDEGRID_SUPERVISOR_STATE_ROOT", &config.state_dir)
        .env(
            "CEDEGRID_SUPERVISOR_NAMESPACE",
            serde_json::to_string(client.namespace_guard.identity())?,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().context("supervisor stderr absent")?;
    let stderr_path = output.join("supervisor.stderr");
    let capture_guard = client.namespace_guard.clone();
    let capture_settings = native_publication.clone();
    let stderr_thread = std::thread::spawn(move || {
        let result: Result<()> = (|| {
            let charge = crate::publication::CaptureCharge::new(
                &capture_settings,
                capture_guard,
                &stderr_path,
            )?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(stderr_path)?;
            let mut input = BufReader::new(stderr);
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                charge.reserve(count as u64)?;
                file.write_all(&buffer[..count])?;
            }
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("supervisor stderr capture failed: {error:#}");
        }
    });
    let input = child.stdin.take().context("supervisor input absent")?;
    let stdout = child.stdout.take().context("supervisor output absent")?;
    let (tx, events) = mpsc::channel();
    let events_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let event = line
                .map_err(|e| e.to_string())
                .and_then(|l| serde_json::from_str(&l).map_err(|e| e.to_string()));
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    Ok(ChildSlot {
        child,
        input,
        events,
        assignment,
        output_dir: output,
        sequence: 0,
        last_renew: Instant::now(),
        prepared: None,
        reported: false,
        checkpoint_hash: None,
        exited: false,
        capture_threads: vec![stderr_thread, events_thread],
    })
}
fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = crate::publication::open_regular(path, false)?;
    let expected = file.metadata()?.len();
    ensure!(
        expected <= 256 * 1024 * 1024,
        "artifact exceeds maximum supported bytes"
    );
    hash_open_file(&mut file, expected)
}
fn hash_open_file(file: &mut File, expected: u64) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("artifact size overflow")?;
        ensure!(size <= expected, "artifact grew while reading");
        hash.update(&buffer[..n]);
    }
    ensure!(size == expected, "artifact changed length while reading");
    Ok((hex::encode(hash.finalize()), size))
}

async fn publish(
    client: &NodeClient,
    assignment: &Assignment,
    output: &Path,
    checkpoint: bool,
    descriptor_bytes: Option<Vec<u8>>,
) -> Result<()> {
    let path = output.join(if checkpoint {
        "checkpoint.json"
    } else {
        "result.json"
    });
    let descriptor_hash = {
        use sha2::{Digest, Sha256};
        descriptor_bytes
            .as_ref()
            .map(|b| hex::encode(Sha256::digest(b)))
            .unwrap_or_else(|| "command".into())
    };
    let descriptor: serde_json::Value = if let Some(bytes) = descriptor_bytes.as_deref() {
        crate::numeric::from_slice(bytes)?
    } else {
        ensure!(!checkpoint, "checkpoint descriptor missing");
        ensure!(
            !path.try_exists()?,
            "legacy outbox requires explicit reconciliation; automatic conversion refused"
        );
        let outcome: ExecutionOutcome = serde_json::from_slice(&crate::publication::read_bounded(
            &output.join("execution-outcome.json"),
            2 * 1024 * 1024,
        )?)?;
        ensure!(
            outcome.exit_code == Some(0) && !outcome.yielded,
            "command did not complete normally"
        );
        serde_json::json!({"schema_version":2,"kind":"result","task_id":assignment.request.task_id,"assignment_id":assignment.request.assignment_id,"generation":assignment.generation,"artifacts":[],"metadata":{"exit_code":0}})
    };
    ensure!(
        descriptor["task_id"] == assignment.request.task_id
            && descriptor["assignment_id"] == assignment.request.assignment_id
            && descriptor["generation"] == assignment.generation,
        "worker descriptor attempt mismatch"
    );
    let mut artifacts = Vec::new();
    for item in descriptor["artifacts"]
        .as_array()
        .context("artifact list missing")?
    {
        let relative = Path::new(item["path"].as_str().context("artifact path missing")?);
        ensure!(
            relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "artifact path must remain within attempt output"
        );
        let path = output.join(relative);
        let mut file = crate::publication::open_regular(&path, false)?;
        let expected = item["size"].as_u64().context("artifact size missing")?;
        ensure!(
            expected <= 256 * 1024 * 1024,
            "artifact exceeds maximum supported bytes"
        );
        let (sha256, size) = hash_open_file(&mut file, expected)?;
        ensure!(
            item["sha256"] == sha256,
            "worker artifact integrity mismatch"
        );
        let artifact = ArtifactRef { sha256, size };
        let Response::Upload {
            upload_id,
            mut offset,
        } = client
            .request(&Request::BeginUpload {
                assignment_id: assignment.request.assignment_id.clone(),
                generation: assignment.generation,
                artifact: artifact.clone(),
            })
            .await?
        else {
            bail!("unexpected upload response")
        };
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; 128 * 1024];
        while offset < size {
            let remaining = usize::try_from(size - offset)?.min(buf.len());
            let n = file.read(&mut buf[..remaining])?;
            ensure!(n > 0, "artifact truncated during upload");
            let response = client
                .request(&Request::UploadChunk {
                    upload_id: upload_id.clone(),
                    offset,
                    data_hex: hex::encode(&buf[..n]),
                })
                .await?;
            let Response::Upload { offset: ack, .. } = response else {
                bail!("unexpected chunk response")
            };
            ensure!(
                ack == offset + n as u64,
                "unexpected upload acknowledgement"
            );
            offset = ack;
        }
        ensure!(
            hash_open_file(&mut file, artifact.size)? == (artifact.sha256.clone(), artifact.size),
            "artifact changed during publication"
        );
        ensure!(
            matches!(
                client.request(&Request::CommitUpload { upload_id }).await?,
                Response::Artifact { .. }
            ),
            "artifact publication failed"
        );
        artifacts.push(artifact);
    }
    let submission = ResultSubmission {
        task_id: assignment.request.task_id.clone(),
        assignment_id: assignment.request.assignment_id.clone(),
        generation: assignment.generation,
        result: descriptor,
        artifacts,
    };
    let published = submission.clone();
    let response = client
        .request(&if checkpoint {
            Request::PublishCheckpoint { submission }
        } else {
            Request::Complete { submission }
        })
        .await?;
    verify_publication_receipt(&response, &published)?;
    let receipt_path = output.join(if checkpoint {
        format!("checkpoint-{descriptor_hash}.receipt.json")
    } else {
        "result.receipt.json".into()
    });
    if !receipt_path.exists() {
        client.write_json_new(
            &receipt_path,
            &serde_json::json!({"response":response,"submission":published}),
        )?;
    }
    if std::env::var_os("CEDEGRID_PUBLICATION_EVIDENCE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        emit(
            &serde_json::json!({"event":"publication_accepted","checkpoint":checkpoint,"submission":published,"receipt":response}),
        )?;
    }
    Ok(())
}

fn validate_replay_launch(config: &Config, request: &LaunchRequest) -> Result<()> {
    if config.storage_profile.is_replayable() {
        ensure!(
            config.node_mode == NodeMode::Opportunistic
                && request.replay_safe
                && request.class == AllocationClass::Opportunistic
                && !request
                    .required_controls
                    .iter()
                    .any(|control| control == "storage.durable_local"),
            "replayable storage is restricted to replay-safe opportunistic work without strong local durability requirements"
        );
    }
    Ok(())
}

pub(crate) fn verify_publication_receipt(
    response: &Response,
    submission: &ResultSubmission,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let Response::Receipt { receipt } = response else {
        bail!("authoritative result acceptance missing durable commit receipt")
    };
    let expected = hex::encode(Sha256::digest(serde_json::to_vec(submission)?));
    ensure!(
        receipt.task_id == submission.task_id
            && receipt.assignment_id == submission.assignment_id
            && receipt.generation == submission.generation
            && receipt.receipt_hash == expected,
        "authoritative publication receipt identity or content mismatch"
    );
    Ok(())
}

fn owned_agent_lock(path: &Path) -> Result<File> {
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
    fs2::FileExt::try_lock_exclusive(&lock).context("another agent owns this node state")?;
    Ok(lock)
}

fn replay_parent(config: &Config, home: &Path) -> Result<PathBuf> {
    ensure!(
        config.node_mode == NodeMode::Opportunistic,
        "replayable storage requires an opportunistic node"
    );
    let parent = config
        .state_dir
        .parent()
        .context("node state parent missing")?;
    ensure!(
        parent.starts_with(home),
        "replay state parent outside runtime home"
    );
    let mut checked = home.to_path_buf();
    for part in parent.strip_prefix(home)?.components() {
        ensure!(
            matches!(part, std::path::Component::Normal(_)),
            "invalid replay state path"
        );
        checked.push(part);
        match fs::symlink_metadata(&checked) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "replay state ancestor is not a real directory"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    ensure!(
                        meta.uid() == unsafe { libc::geteuid() },
                        "replay state ancestor not owned by runtime user"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => private_dir(&checked)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(parent.to_path_buf())
}

fn quarantine_replay_state(
    state: &Path,
    session_id: &str,
    guard: &NamespaceGuard,
) -> Result<Option<PathBuf>> {
    guard.validate_root(state)?;
    ensure!(
        guard.is_exclusive(),
        "quarantine requires exclusive namespace lifecycle"
    );
    // Called only after the coordinator has durably fenced all older sessions.
    let meta = match fs::symlink_metadata(state) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "replay state must be an owned real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() },
            "replay state not owned by runtime user"
        );
    }
    let parent = state.parent().context("replay state parent missing")?;
    let leaf = state
        .file_name()
        .context("replay state leaf missing")?
        .to_string_lossy();
    let destination = parent.join(format!(".{leaf}.quarantine-{session_id}"));
    ensure!(
        !destination.try_exists()?,
        "replay quarantine already exists"
    );
    fs::rename(state, &destination)?;
    File::open(parent)?.sync_all()?;
    Ok(Some(destination))
}

async fn start_replay_session(
    config: &Config,
    agent: &AgentConfig,
    home: &Path,
) -> Result<(NamespaceOwner, NamespaceGuard, ReplayRecoverySnapshot)> {
    replay_parent(config, home)?;
    let namespace = Namespace::new(&config.state_dir)?;
    let maintenance = namespace.begin_maintenance(Duration::ZERO)?;
    let session_id = uuid::Uuid::new_v4().to_string();
    maintenance.record_intent(&session_id)?;
    let boot_id = supervision::process_identity(std::process::id(), "agent", 0)?.boot_id;
    let rpc = RpcClient::new(&agent.coordinator_url, &agent.tls)?;
    let Response::ReplayRecovery { snapshot } = rpc
        .request(&Request::OpenReplaySession {
            node_id: config.node_id.clone(),
            boot_id: boot_id.clone(),
            session_id: session_id.clone(),
        })
        .await?
    else {
        bail!("coordinator did not provide authoritative recovery inventory")
    };
    ensure!(
        snapshot.session_id == session_id,
        "recovery session mismatch"
    );
    for allocation in &snapshot.allocations {
        ensure!(
            allocation.assignment.node_id == config.node_id
                && allocation.assignment.coordinator_epoch == snapshot.coordinator_epoch,
            "recovery inventory node or epoch mismatch"
        );
    }
    let guard = maintenance.exclusive(Duration::from_secs(30))?;
    crate::upgrade::require_replay_upgrade(&namespace, &guard)?;
    // Legacy service ownership complements the new namespace locks: old binaries
    // do not participate in namespace protection and cannot run concurrently.
    let _old_lock = if config.state_dir.join("agent.lock").try_exists()? {
        Some(owned_agent_lock(&config.state_dir.join("agent.lock"))?)
    } else {
        None
    };
    quarantine_replay_state(&config.state_dir, &session_id, &guard)?;
    let guard = maintenance.initialize_session(guard, &session_id)?;
    let store = StateStore::open_with_profile_guarded(
        &config.state_dir,
        config.storage_profile,
        guard.clone(),
    )?;
    for allocation in &snapshot.allocations {
        store.import_replay_reservation(allocation)?;
    }
    write_json_new(&config.state_dir.join("replay-recovery.json"), &snapshot)?;
    // The first report retains every historical charge; it may not infer release
    // while recovery is still incomplete. Later ordinary reconciliation proves it.
    let mut allocations: Vec<AllocationReport> = snapshot
        .allocations
        .iter()
        .map(|a| AllocationReport {
            assignment_id: a.assignment.request.assignment_id.clone(),
            generation: a.assignment.generation,
            phase: RemotePhase::Uncertain,
            observed: None,
            detail: "authenticated recovery retains original reservation".into(),
        })
        .collect();
    allocations.extend(snapshot.unrecognized.iter().cloned().map(|mut a| {
        a.phase = RemotePhase::Uncertain;
        a
    }));
    let report = NodeReport {
        node_id: config.node_id.clone(),
        boot_id,
        observed_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
        managed_budget: Resources::default(),
        expansion_allowed: false,
        gpu_expansion_allowed: false,
        gpu_guaranteed_allowed: false,
        launch_slots: 0,
        available_controls: vec!["storage.replayable_local".into()],
        allocations,
    };
    maintenance.set_phase("awaiting_readiness")?;
    let response = rpc
        .request(&Request::NodeSession {
            session_id,
            request: Box::new(Request::Heartbeat { report }),
        })
        .await?;
    ensure!(
        matches!(response, Response::Heartbeat { ref reply } if reply.coordinator_epoch == snapshot.coordinator_epoch && reply.assignments.is_empty()),
        "recovery readiness was not acknowledged at the fenced coordinator epoch"
    );
    drop(store);
    let (owner, guard) = maintenance.activate(guard, Duration::from_secs(30))?;
    Ok((owner, guard, snapshot))
}

/// Run a node service until shutdown or its configured finite envelope. Every
/// managed workload has a separate supervisor; disconnects do not stop its clock.
pub async fn run(config: &Config, agent: &AgentConfig, executable: &Path) -> Result<()> {
    ensure!(
        config.execution.enabled,
        "agent execution requires execution.enabled=true"
    );
    ensure!(
        agent.max_workers > 0 && agent.max_workers <= 256,
        "invalid maximum worker count"
    );
    ensure!(
        agent.capacity.cpu_millicores > 0 && agent.capacity.ram_mib > 0,
        "explicit node CPU/RAM budget required"
    );
    ensure!(
        config.state_dir.is_absolute(),
        "state directory must be resolved before service startup"
    );
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?).canonicalize()?;
    ensure!(
        config.state_dir.starts_with(&home),
        "agent-created state must be within runtime home"
    );
    let mut existing = config.state_dir.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .context("state path has no existing ancestor")?;
    }
    ensure!(
        existing.canonicalize()?.starts_with(&home),
        "state ancestor resolves outside runtime home"
    );
    ensure!(
        !config
            .state_dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "parent traversal in state directory is refused"
    );
    if let Some(cpus) = &agent.cpu_affinity {
        #[cfg(target_os = "linux")]
        ensure!(
            unsafe { libc::syscall(libc::SYS_gettid) } == i64::from(std::process::id()),
            "configured affinity requires agent main-thread service entrypoint"
        );
        supervision::apply_current_cpu_affinity(cpus)?;
    }
    ensure!(
        agent.max_transfer_bytes_per_second > 0,
        "transfer rate must be positive"
    );
    let replay = if config.storage_profile.is_replayable() {
        Some(start_replay_session(config, agent, &home).await?)
    } else {
        None
    };
    let namespace = Namespace::new(&config.state_dir)?;
    let _owner = if replay.is_none() {
        Some(namespace.owner(Duration::ZERO)?)
    } else {
        None
    };
    let namespace_guard = if let Some((_, guard, _)) = &replay {
        guard.clone()
    } else {
        namespace.acquire(None, Duration::from_secs(30))?
    };
    crate::backup::ensure_runnable_state(&config.state_dir)?;
    let store = StateStore::open_with_profile_guarded(
        &config.state_dir,
        config.storage_profile,
        namespace_guard.clone(),
    )?;
    if let Some((_, _, snapshot)) = &replay {
        emit(
            &serde_json::json!({"event":"replay_recovery_started", "session_id":snapshot.session_id,
            "coordinator_epoch":snapshot.coordinator_epoch, "retained_allocations":snapshot.allocations.len(),
            "storage_assurance":config.storage_profile.assurance(),
            "detail":"Authenticated recovery ready; uncertain allocations remain charged."}),
        )?;
    }
    let replay_journal = replay
        .as_ref()
        .map(|_| ReplayJournalGuard::capture(config))
        .transpose()?;
    ensure!(
        config.state_dir.canonicalize()?.starts_with(&home),
        "state path resolves outside runtime home"
    );
    for name in ["attempts", "tmp", "cache", "refusals", "failure-receipts"] {
        private_dir(&config.state_dir.join(name))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.state_dir.join("agent.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another agent already owns this node state")?;
    let _ = reconcile_guarded(config, &BTreeSet::new(), namespace_guard.clone())?;
    let mut client = NodeClient::new(
        agent,
        &config.state_dir,
        config.storage_profile,
        namespace_guard.clone(),
    )?;
    client.replay_session = replay
        .as_ref()
        .map(|(_, _, snapshot)| snapshot.session_id.clone());
    let client = std::sync::Arc::new(client);
    let mut collector = ManagedCollector::new(config)?;
    let mut policy = PolicyEngine::new();
    let mut slots: BTreeMap<String, ChildSlot> = BTreeMap::new();
    let mut refusals: BTreeMap<String, (Assignment, String)> = BTreeMap::new();
    for item in fs::read_dir(config.state_dir.join("refusals"))? {
        let item = item?;
        ensure!(
            item.file_type()?.is_file() && item.metadata()?.len() <= 2 * 1024 * 1024,
            "invalid refusal record"
        );
        let pair: (Assignment, String) = serde_json::from_slice(
            &crate::publication::read_bounded(&item.path(), 3 * 1024 * 1024)?,
        )?;
        if !config
            .state_dir
            .join("failure-receipts")
            .join(format!("{}.json", pair.0.request.assignment_id))
            .is_file()
        {
            refusals.insert(pair.0.request.assignment_id.clone(), pair);
        }
    }
    type Publication = tokio::task::JoinHandle<(String, bool, String, Result<()>)>;
    let mut publications: Vec<Publication> = Vec::new();
    let mut publishing: BTreeSet<(String, bool)> = BTreeSet::new();
    let start = Instant::now();
    let observation_interval = Duration::from_millis(config.monitor.interval_ms);
    let mut next_observation = start;
    let mut last_heartbeat = start - Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
    let mut stopping: Option<Instant> = None;
    let mut stop_signal = Box::pin(shutdown_signal());
    let mut last_spool_check = Instant::now();
    type Preparation = (Assignment, tokio::task::JoinHandle<Result<ChildSlot>>, bool);
    let mut preparations: BTreeMap<String, Preparation> = BTreeMap::new();
    let mut spool_full = false;
    let mut admitted_parallelism = 0usize;
    let mut last_growth = start;
    loop {
        // Anchor observations to their scheduled cadence. Processing and RPC
        // time consume this interval instead of extending it. After an overrun,
        // run once immediately and return to the original cadence.
        next_observation =
            next_periodic_wake(next_observation, observation_interval, Instant::now());
        if let Some(guard) = &replay_journal {
            guard.verify(config)?;
        }
        let ready: Vec<_> = preparations
            .iter()
            .filter(|(_, (_, h, _))| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let (assignment, handle, revoked) =
                preparations.remove(&id).expect("listed preparation");
            match handle.await.context("assignment preparation task failed")? {
                Ok(mut slot) => {
                    if revoked || stopping.is_some() {
                        let _ = send(&mut slot, &Control::Drain);
                    }
                    slots.insert(id, slot);
                }
                Err(error) => {
                    let pair = (
                        assignment,
                        format!("launch refused before execution: {error:#}"),
                    );
                    let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                    if !proof.exists() {
                        write_json_new(&proof, &pair)?;
                    }
                    refusals.insert(id.clone(), pair);
                    emit(
                        &serde_json::json!({"event":"launch_refused","assignment_id":id,"detail":format!("{error:#}")}),
                    )?;
                }
            }
        }
        for handle_index in (0..publications.len()).rev() {
            if publications[handle_index].is_finished() {
                let (id, checkpoint, hash, result) = publications.swap_remove(handle_index).await?;
                publishing.remove(&(id.clone(), checkpoint));
                match result {
                    Ok(()) => {
                        if let Some(slot) = slots.get_mut(&id) {
                            if checkpoint {
                                slot.checkpoint_hash = Some(hash)
                            } else {
                                slot.reported = true;
                            }
                        }
                    }
                    Err(error) => {
                        emit(
                            &serde_json::json!({"event":"publication_retry","assignment_id":id,"checkpoint":checkpoint,"detail":format!("{error:#}")}),
                        )?;
                    }
                }
            }
        }
        for slot in slots.values_mut() {
            let supervisor_exited = slot.child.try_wait()?.is_some();
            slot.exited = supervisor_exited;
            while let Ok(event) = slot.events.try_recv() {
                match event {
                    Ok(Event::Prepared { record }) => {
                        collector.prime(std::slice::from_ref(&record));
                        slot.prepared = Some(record);
                    }
                    Ok(Event::Finished { .. }) => {}
                    Ok(Event::Failed { detail }) | Err(detail) => {
                        emit(
                            &serde_json::json!({"event":"supervisor_error","assignment_id":slot.assignment.request.assignment_id,"detail":detail}),
                        )?;
                    }
                }
            }
            if let Some(record) = slot
                .prepared
                .as_ref()
                .filter(|_| slot.sequence == 0 && !supervisor_exited)
            {
                let requested = Instant::now();
                match client
                    .request(&Request::Prepared {
                        assignment_id: record.assignment_id.clone(),
                        generation: record.generation,
                        coordinator_epoch: slot.assignment.coordinator_epoch,
                        record: record.clone(),
                    })
                    .await
                {
                    Ok(Response::Lease { lease }) => {
                        let remaining_ms = lease
                            .valid_for_ms
                            .saturating_sub(requested.elapsed().as_millis() as u64);
                        if remaining_ms > 0 {
                            let seq = lease.sequence;
                            let _ = send(
                                slot,
                                &Control::Authorize {
                                    lease,
                                    expires_monotonic_ms: local_clock_ms()?
                                        .saturating_add(remaining_ms),
                                },
                            );
                            slot.sequence = seq;
                            slot.last_renew = Instant::now();
                        }
                    }
                    result => {
                        emit(
                            &serde_json::json!({"event":"authorization_unavailable","assignment_id":slot.assignment.request.assignment_id,"detail":format!("{result:?}")}),
                        )?;
                    }
                }
            }
            let record = store.execution_record(&slot.assignment.request.assignment_id)?;
            ensure!(
                !config.storage_profile.is_replayable()
                    || record.is_some()
                    || (!supervisor_exited && slot.prepared.is_none() && slot.sequence == 0),
                "replayable execution journal lost an owned attempt; reservation remains uncertain until authenticated recovery"
            );
            if supervisor_exited && record.is_none() {
                // The only route to EXEC first persists an execution row. An
                // exited owned supervisor with no row could not authorize code.
                let id = slot.assignment.request.assignment_id.clone();
                let pair = (
                    slot.assignment.clone(),
                    "owned supervisor exited before reservation/authorization".to_string(),
                );
                let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                if !proof.exists() {
                    write_json_new(&proof, &pair)?;
                }
                refusals.insert(id, pair);
                slot.reported = true;
                continue;
            }
            let running = record.as_ref().is_some_and(|r| {
                matches!(
                    r.phase,
                    ExecutionPhase::Authorized | ExecutionPhase::Running
                )
            });
            if running
                && slot.sequence > 0
                && slot.last_renew.elapsed()
                    >= Duration::from_millis(config.lifecycle.heartbeat_interval_ms)
            {
                let requested = Instant::now();
                let interval = Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
                let due = next_control_due(&mut slot.last_renew, interval);
                match client
                    .request_control(
                        &Request::Renew {
                            assignment_id: slot.assignment.request.assignment_id.clone(),
                            generation: slot.assignment.generation,
                            coordinator_epoch: slot.assignment.coordinator_epoch,
                            previous_sequence: slot.sequence,
                        },
                        due,
                        interval,
                    )
                    .await
                {
                    Ok(Response::Lease { lease }) => {
                        let remaining_ms = lease
                            .valid_for_ms
                            .saturating_sub(requested.elapsed().as_millis() as u64);
                        if remaining_ms > 0 {
                            let seq = lease.sequence;
                            let _ = send(
                                slot,
                                &Control::Renewal {
                                    lease,
                                    expires_monotonic_ms: local_clock_ms()?
                                        .saturating_add(remaining_ms),
                                },
                            );
                            slot.sequence = seq;
                        }
                    }
                    result => {
                        emit(
                            &serde_json::json!({"event":"lease_unavailable","assignment_id":slot.assignment.request.assignment_id,"detail":format!("{result:?}")}),
                        )?;
                    }
                }
            }
            for checkpoint in [true, false] {
                let id = slot.assignment.request.assignment_id.clone();
                if publishing.contains(&(id.clone(), checkpoint))
                    || slot.reported
                    || (checkpoint && publishing.contains(&(id.clone(), false)))
                {
                    continue;
                }
                let publication_pin = crate::publication_gc::pin_attempt(&store, &slot.output_dir)?;
                let descriptor_bytes = match crate::publication::read_head(
                    &store,
                    &slot.output_dir,
                    if checkpoint { "checkpoint" } else { "result" },
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        // A publisher reserves its candidate before durable insertion.
                        // Pending or damaged candidates must retain this attempt and
                        // must never turn into a plain-command success or stop leases
                        // for other workloads. A later poll can observe reconciliation.
                        if record
                            .as_ref()
                            .is_some_and(|r| r.phase == ExecutionPhase::Released)
                        {
                            emit(
                                &serde_json::json!({"event":"publication_retained","assignment_id":id,"detail":error.to_string()}),
                            )?;
                        }
                        continue;
                    }
                };
                let has_descriptor = descriptor_bytes.is_some();
                let hash = descriptor_bytes
                    .as_ref()
                    .map(|bytes| {
                        use sha2::{Digest, Sha256};
                        hex::encode(Sha256::digest(bytes))
                    })
                    .unwrap_or_default();
                if checkpoint && (hash.is_empty() || slot.checkpoint_hash.as_ref() == Some(&hash)) {
                    continue;
                }
                if !checkpoint {
                    // Checkpoint and final acceptance update the same attempt.
                    // Final acceptance fences later checkpoints, so do not race
                    // it against an outstanding checkpoint publication.
                    if publishing.contains(&(id.clone(), true)) {
                        continue;
                    }
                    if !record
                        .as_ref()
                        .is_some_and(|r| r.phase == ExecutionPhase::Released)
                    {
                        continue;
                    }
                    let outcome = fs::read(slot.output_dir.join("execution-outcome.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<ExecutionOutcome>(&b).ok());
                    if outcome.is_none() && !supervisor_exited {
                        continue;
                    }
                    if outcome
                        .as_ref()
                        .is_none_or(|o| o.exit_code != Some(0) || o.yielded)
                    {
                        if publishing.contains(&(id.clone(), true)) {
                            continue;
                        }
                        if has_descriptor { /* Native immutable result committed before a racing drain. */
                        } else {
                            if matches!(
                                client
                                    .request(&Request::Fail {
                                        assignment_id: id.clone(),
                                        generation: slot.assignment.generation,
                                        detail: record.as_ref().unwrap().detail.clone(),
                                        failure_kind: if outcome.as_ref().is_some_and(|o| o.yielded)
                                        {
                                            FailureKind::Yielded
                                        } else {
                                            FailureKind::ExecutionFailure
                                        }
                                    })
                                    .await,
                                Ok(Response::Ok)
                            ) {
                                let receipt = slot.output_dir.join("failure.receipt.json");
                                if !receipt.exists() {
                                    write_json_new(
                                        &receipt,
                                        &serde_json::json!({"assignment_id":id,"generation":slot.assignment.generation,"accepted_failure":true}),
                                    )?;
                                }
                                slot.reported = true;
                            }
                            continue;
                        }
                    }
                }
                let task_client = client.clone();
                let assignment = slot.assignment.clone();
                let output = slot.output_dir.clone();
                publishing.insert((id.clone(), checkpoint));
                publications.push(tokio::spawn(async move {
                    let _pin = publication_pin;
                    let result = publish(
                        &task_client,
                        &assignment,
                        &output,
                        checkpoint,
                        descriptor_bytes,
                    )
                    .await;
                    (id, checkpoint, hash, result)
                }));
            }
            if let Some(status) = slot.child.try_wait()?
                && let Some(mut record) =
                    store.execution_record(&slot.assignment.request.assignment_id)?
                && record.phase != ExecutionPhase::Released
            {
                // Reaping synchronizes with the supervisor's last durable write.
                // The earlier loop snapshot may predate its Released commit.
                record.phase = ExecutionPhase::NeedsReconciliation;
                record.detail =
                    format!("supervisor exited {status}; existing allocation remains charged");
                store.transition(&record)?;
            }
        }
        let records = store.executions()?;
        collect_released_spool(config, &store, &records, &publishing)?;
        let mut released_families = BTreeSet::new();
        for record in records
            .iter()
            .filter(|r| r.phase == ExecutionPhase::Released && slots.contains_key(&r.assignment_id))
        {
            if store
                .managed_children(&record.assignment_id)?
                .iter()
                .all(|c| c.phase == crate::managed_children::ManagedChildPhase::Released)
            {
                released_families.insert(record.assignment_id.clone());
            }
        }
        slots.retain(|id, slot| {
            !(slot.exited
                && slot.reported
                && !publishing.iter().any(|(published, _)| published == id)
                && (released_families.contains(id)
                    || refusals.contains_key(id)
                    || config
                        .state_dir
                        .join("failure-receipts")
                        .join(format!("{id}.json"))
                        .exists()))
        });

        // Durable outbox recovery after agent loss or a lost final ACK. Only
        // previously journaled attempts are considered, never directory names as IDs.
        for record in records.iter().filter(|r| {
            r.phase == ExecutionPhase::Released && !slots.contains_key(&r.assignment_id)
        }) {
            let output = config
                .state_dir
                .join("attempts")
                .join(&record.assignment_id);
            if output.join("result.receipt.json").exists()
                || config
                    .state_dir
                    .join("failure-receipts")
                    .join(format!("{}.json", record.assignment_id))
                    .exists()
                || output.join("failure.receipt.json").exists()
                || publishing.contains(&(record.assignment_id.clone(), false))
            {
                continue;
            }
            if upgrade_attempt_held(&store, &record.assignment_id)? {
                continue;
            }
            if let Ok(bytes) = crate::publication::read_bounded(
                &output.join("supervisor-spec.json"),
                3 * 1024 * 1024,
            ) {
                let spec: SupervisorSpec = serde_json::from_slice(&bytes)?;
                ensure!(
                    spec.assignment.request.assignment_id == record.assignment_id
                        && spec.assignment.generation == record.generation,
                    "outbox identity mismatch"
                );
                let normal_exit = fs::read(output.join("execution-outcome.json"))
                    .ok()
                    .and_then(|v| serde_json::from_slice::<ExecutionOutcome>(&v).ok())
                    .is_some_and(|o| o.exit_code == Some(0) && !o.yielded);
                let publication_pin = crate::publication_gc::pin_attempt(&store, &output)?;
                let descriptor_bytes = match crate::publication::read_head(
                    &store, &output, "result",
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        emit(
                            &serde_json::json!({"event":"publication_retained","assignment_id":record.assignment_id,"detail":error.to_string()}),
                        )?;
                        continue;
                    }
                };
                if descriptor_bytes.is_some()
                    || (normal_exit && !output.join("result.json").try_exists()?)
                {
                    let task_client = client.clone();
                    let id = record.assignment_id.clone();
                    publishing.insert((id.clone(), false));
                    publications.push(tokio::spawn(async move {
                        let _pin = publication_pin;
                        let result = publish(
                            &task_client,
                            &spec.assignment,
                            &output,
                            false,
                            descriptor_bytes,
                        )
                        .await;
                        (id, false, String::new(), result)
                    }));
                } else {
                    // Confirmed released failure after an agent crash still needs
                    // its idempotent Fail acknowledgement, never result publication.
                    let pair = (spec.assignment, record.detail.clone());
                    let proof = config
                        .state_dir
                        .join("refusals")
                        .join(format!("{}.json", record.assignment_id));
                    if !proof.exists() {
                        write_json_new(&proof, &pair)?;
                    }
                    refusals.insert(record.assignment_id.clone(), pair);
                }
            }
        }
        let (mut snapshot, allocations) =
            collector.sample_with_children(&records, &store.all_unreleased_managed_children()?)?;
        restrict_gpu_scope(&mut snapshot, &agent.capacity);
        if last_spool_check.elapsed() >= Duration::from_secs(5) {
            spool_full = spool_size(&config.state_dir.join("attempts"))? > agent.max_spool_bytes;
            last_spool_check = Instant::now();
        }
        let mut decision = policy.evaluate(
            config,
            &snapshot,
            &PolicyInput {
                allocations: allocations.clone(),
                explicit_drain: stopping.is_some() || spool_full,
            },
            start.elapsed().as_millis() as u64,
        )?;
        decision.managed_budget = minimum(&decision.managed_budget, &agent.capacity);
        decision.observe_only = false;
        decision.admission_headroom = minimum(
            &decision.admission_headroom,
            &remaining_capacity(&decision.managed_budget, &allocation_charge(&allocations)),
        );
        // Re-evaluate victims against the explicit operator envelope by cumulative
        // reservation/actual charges, independent of a larger idle host budget.
        let envelope_exceeded = !allocations_fit(&allocations, &agent.capacity);
        if envelope_exceeded {
            decision.expansion_allowed = false;
            decision.cpu_ram_expansion_allowed = false;
            decision.would_drain.extend(slots.keys().cloned());
            decision.reasons.push(format!(
                "operator-approved node envelope exceeded: capacity={} allocations={}",
                serde_json::to_string(&agent.capacity)?,
                serde_json::to_string(&allocations)?
            ));
        }
        for id in &decision.would_drain {
            if let Some(slot) = slots.get_mut(id)
                && (stopping.is_some()
                    || spool_full
                    || envelope_exceeded
                    || slot.assignment.request.class == AllocationClass::Opportunistic)
            {
                let _ = send(slot, &Control::Drain);
            }
        }
        store.append_observation(&snapshot, &decision)?;
        if stopping.is_some()
            || last_heartbeat.elapsed()
                >= Duration::from_millis(config.lifecycle.heartbeat_interval_ms)
        {
            let interval = Duration::from_millis(config.lifecycle.heartbeat_interval_ms);
            let owned = slots
                .iter()
                .filter(|(_, slot)| !slot.exited)
                .map(|(id, _)| id.clone())
                .collect();
            let _ = reconcile_guarded(config, &owned, namespace_guard.clone())?;
            let boot_id = supervision::process_identity(std::process::id(), "agent", 0)?.boot_id;
            let reconciled_records = store.executions()?;
            let mut reports: Vec<AllocationReport> = reconciled_records
                .iter()
                .map(|r| AllocationReport {
                    assignment_id: r.assignment_id.clone(),
                    generation: r.generation,
                    phase: phase(&r.phase),
                    observed: allocations
                        .iter()
                        .find(|a| a.id == r.assignment_id)
                        .and_then(|a| a.observed.clone()),
                    detail: r.detail.clone(),
                })
                .collect();
            reports.extend(
                refusals
                    .values()
                    .filter(|(assignment, _)| {
                        !reconciled_records
                            .iter()
                            .any(|record| record.assignment_id == assignment.request.assignment_id)
                    })
                    .map(|(assignment, detail)| AllocationReport {
                        assignment_id: assignment.request.assignment_id.clone(),
                        generation: assignment.generation,
                        phase: RemotePhase::Released,
                        observed: None,
                        detail: detail.clone(),
                    }),
            );
            let mut available_controls = vec!["cpu.nice".into()];
            available_controls.push(if config.storage_profile.is_replayable() {
                "storage.replayable_local".into()
            } else {
                "storage.durable_local".into()
            });
            if agent.cpu_affinity.is_some() {
                available_controls.push("cpu.affinity".into());
            }
            if crate::telemetry::pidfd_capability().status
                == crate::model::CapabilityStatus::Available
            {
                available_controls.push("process_handle".into());
            }
            if config.cgroup.enabled
                && let Some(kernel) = &snapshot.kernel
            {
                available_controls.extend(
                    kernel
                        .controls
                        .iter()
                        .filter(|c| {
                            c.available == Some(true) && c.permitted == Some(true) && c.configured
                        })
                        .map(|c| c.control.clone()),
                );
            }
            let active_workers =
                slots.values().filter(|s| !s.reported).count() + preparations.len();
            let draining_workers = decision
                .would_drain
                .iter()
                .filter(|id| slots.get(*id).is_some_and(|s| !s.reported))
                .count();
            if draining_workers > 0 {
                admitted_parallelism =
                    admitted_parallelism.min(active_workers.saturating_sub(draining_workers));
                last_growth = Instant::now();
            }
            let may_grow =
                last_growth.elapsed() >= Duration::from_millis(config.gpu.scale_up_cooldown_ms);
            let admission_safe = decision.expansion_allowed || decision.cpu_ram_expansion_allowed;
            let launch_slots = if admission_safe && stopping.is_none() && !spool_full {
                agent.max_workers.saturating_sub(active_workers).min(
                    admitted_parallelism.saturating_sub(active_workers) + usize::from(may_grow),
                ) as u32
            } else {
                0
            };
            let report = NodeReport {
                node_id: config.node_id.clone(),
                launch_slots,
                boot_id,
                observed_at_unix_ms: snapshot.observed_at_unix_ms,
                managed_budget: schedulable_budget(&decision),
                gpu_guaranteed_allowed: config.gpu.execution_mode
                    == crate::config::GpuExecutionMode::ContentionAware,
                expansion_allowed: admission_safe && stopping.is_none() && !spool_full,
                gpu_expansion_allowed: decision.expansion_allowed
                    && stopping.is_none()
                    && !spool_full,
                available_controls,
                allocations: reports,
            };
            // Report preparation can cross a scheduled slot. Advance at actual
            // enqueue so the next due time agrees with recorded lateness.
            let due = next_control_due(&mut last_heartbeat, interval);
            match client
                .request_control(&Request::Heartbeat { report }, due, interval)
                .await
            {
                Ok(Response::Heartbeat { reply }) => {
                    for slot in slots.values_mut() {
                        slot.assignment.coordinator_epoch = reply.coordinator_epoch;
                    }
                    let remote_drains: BTreeSet<_> = reply.drain.iter().cloned().collect();
                    for id in reply.drain {
                        if let Some((_, _, revoked)) = preparations.get_mut(&id) {
                            *revoked = true;
                        }
                        if let Some(slot) = slots.get_mut(&id) {
                            let _ = send(slot, &Control::Drain);
                        }
                    }
                    let mut acknowledged = vec![];
                    for (id, (assignment, detail)) in &refusals {
                        let yielded = fs::read(
                            config
                                .state_dir
                                .join("attempts")
                                .join(id)
                                .join("execution-outcome.json"),
                        )
                        .ok()
                        .and_then(|b| serde_json::from_slice::<ExecutionOutcome>(&b).ok())
                        .is_some_and(|o| o.yielded);
                        if matches!(
                            client
                                .request(&Request::Fail {
                                    assignment_id: id.clone(),
                                    generation: assignment.generation,
                                    detail: detail.clone(),
                                    failure_kind: if detail.contains("revoked") || yielded {
                                        FailureKind::Yielded
                                    } else {
                                        FailureKind::ExecutionFailure
                                    }
                                })
                                .await,
                            Ok(Response::Ok)
                        ) {
                            let receipt = config
                                .state_dir
                                .join("failure-receipts")
                                .join(format!("{id}.json"));
                            if !receipt.exists() {
                                write_json_new(
                                    &receipt,
                                    &serde_json::json!({"assignment_id":id,"generation":assignment.generation,"accepted_failure":true}),
                                )?;
                            }
                            acknowledged.push(id.clone());
                        }
                    }
                    for id in acknowledged {
                        refusals.remove(&id);
                    }
                    for assignment in reply.assignments {
                        let id = assignment.request.assignment_id.clone();
                        if remote_drains.contains(&id)
                            && !slots.contains_key(&id)
                            && !preparations.contains_key(&id)
                            && !store.executions()?.iter().any(|r| r.assignment_id == id)
                        {
                            ensure!(
                                !config.storage_profile.is_replayable(),
                                "revoked replayable offer without local identity requires authoritative recovery; absence is not release proof"
                            );
                            let pair=(assignment,"coordinator revoked never-launched offer; no local execution journal row".to_string());
                            let proof =
                                config.state_dir.join("refusals").join(format!("{id}.json"));
                            if !proof.exists() {
                                write_json_new(&proof, &pair)?;
                            }
                            refusals.insert(id, pair);
                            continue;
                        }
                        if slots.contains_key(&id)
                            || preparations.contains_key(&id)
                            || refusals.contains_key(&id)
                            || records.iter().any(|r| r.assignment_id == id)
                        {
                            continue;
                        }
                        let active =
                            slots.values().filter(|s| !s.reported).count() + preparations.len();
                        if active >= agent.max_workers || stopping.is_some() {
                            continue;
                        }
                        let task_config = config.clone();
                        let task_agent = agent.clone();
                        let task_assignment = assignment.clone();
                        let task_capacity = schedulable_budget(&decision);
                        let task_snapshot = snapshot.clone();
                        let task_executable = executable.to_path_buf();
                        let task_client = client.clone();
                        let handle = tokio::spawn(async move {
                            validate_gpu_launch_contract(
                                &task_config,
                                &task_snapshot,
                                &task_assignment.request.resources,
                                task_assignment.request.class,
                            )?;
                            start_assignment(
                                &task_config,
                                &task_agent,
                                task_assignment,
                                &task_capacity,
                                &task_executable,
                                &task_client,
                            )
                            .await
                        });
                        preparations.insert(id, (assignment, handle, false));
                        if active >= admitted_parallelism {
                            admitted_parallelism = active + 1;
                            last_growth = Instant::now();
                        }
                    }
                }
                result => {
                    emit(
                        &serde_json::json!({"event":"coordinator_disconnected","detail":format!("{result:?}")}),
                    )?;
                }
            }
        }
        if agent.max_runtime_seconds > 0
            && start.elapsed() >= Duration::from_secs(agent.max_runtime_seconds)
            && stopping.is_none()
        {
            stopping = Some(Instant::now());
        }
        if let Some(stop) = stopping {
            if records.iter().all(|r| r.phase == ExecutionPhase::Released)
                && publications.is_empty()
                && preparations.is_empty()
                && slots.is_empty()
            {
                break;
            }
            let limit = config.lifecycle.drain_timeout_ms
                + config.lifecycle.term_grace_ms
                + config.execution.release_confirm_timeout_ms
                + config.execution.prepare_timeout_ms;
            if stop.elapsed() >= Duration::from_millis(limit) {
                emit(
                    &serde_json::json!({"event":"stop_uncertain","detail":"release deadline elapsed; retained allocations require reconciliation"}),
                )?;
                break;
            }
        }
        tokio::select! {_ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_observation))=>{}, _=&mut stop_signal,if stopping.is_none()=>{stopping=Some(Instant::now());}}
    }
    for (id, (assignment, handle, _)) in preparations {
        handle.abort();
        match handle.await {
            Ok(Ok(mut slot)) => {
                let _ = send(&mut slot, &Control::Drain);
                slots.insert(id, slot);
            }
            Ok(Err(_)) | Err(_) => {
                // No await exists after supervisor spawn, so cancellation either
                // returns the owned slot or proves the future stopped before spawn.
                let pair = (
                    assignment,
                    "agent stopped during pre-execution checkpoint preparation".to_string(),
                );
                let proof = config.state_dir.join("refusals").join(format!("{id}.json"));
                if !proof.exists() {
                    write_json_new(&proof, &pair)?;
                }
            }
        }
    }
    // Closing pipes is agent failure for any remaining opportunistic supervisor.
    // We deliberately never signal an uncertain supervisor/workload by PID.
    for slot in slots.values_mut() {
        let _ = send(slot, &Control::Drain);
        let _ = slot.child.try_wait()?;
    }
    let retained: Vec<_>=slots.iter().map(|(id,slot)|serde_json::json!({"assignment_id":id,"exited":slot.exited,"reported":slot.reported,"publishing":publishing.iter().filter(|(publishing_id,_)|publishing_id==id).collect::<Vec<_>>(),"result_receipt":slot.output_dir.join("result.receipt.json").is_file(),"failure_receipt":slot.output_dir.join("failure.receipt.json").is_file()})).collect();
    let status = serde_json::json!({"event":"agent_stopped","node_id":config.node_id,"elapsed_ms":start.elapsed().as_millis(),"retained_slots":slots.len(),"retained_slot_details":retained,"executions":store.executions()?});
    emit(&status)?;
    drop(lock);
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn upgrade_attempt_held(store: &StateStore, assignment: &str) -> Result<bool> {
    let exists: bool = store.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='upgrade_attempt_holds')", [], |r| r.get(0))?;
    if !exists {
        return Ok(false);
    }
    Ok(store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM upgrade_attempt_holds WHERE assignment_id=?1)",
        [assignment],
        |r| r.get(0),
    )?)
}

fn collect_released_spool(
    config: &Config,
    store: &StateStore,
    records: &[ExecutionRecord],
    publishing: &BTreeSet<(String, bool)>,
) -> Result<()> {
    for record in records
        .iter()
        .filter(|r| r.phase == ExecutionPhase::Released)
    {
        if upgrade_attempt_held(store, &record.assignment_id)?
            || publishing.iter().any(|(id, _)| id == &record.assignment_id)
        {
            continue;
        }
        let output = config
            .state_dir
            .join("attempts")
            .join(&record.assignment_id);
        if output.join("spool-reclaimed.json").exists() {
            continue;
        }
        let Some(reclaimed) = crate::publication_gc::reclaim_released(store, &output)? else {
            continue;
        };
        write_json_new(
            &output.join("spool-reclaimed.json"),
            &serde_json::json!({"assignment_id":record.assignment_id,"generation":record.generation,"logical_unlinked_bytes":reclaimed,"basis":"confirmed Released, exact accepted final receipt/head, absent supervisor and publication pins; coordinator retains published content"}),
        )?;
    }
    Ok(())
}
/// Restrict admission telemetry to the explicit GPU UUID resource scope. An
/// empty scope permits CPU-only admission without asserting host GPU idleness.
pub fn restrict_gpu_scope(snapshot: &mut crate::model::Snapshot, capacity: &Resources) {
    snapshot
        .gpus
        .retain(|g| capacity.gpu_memory_mib.contains_key(&g.uuid));
    if capacity.gpu_memory_mib.is_empty() {
        // Known-empty operator-authorized GPU set, not a claim about host inventory.
        snapshot.gpu_inventory = crate::model::CapabilityStatus::Available;
    }
    snapshot.capabilities.insert("gpu_admission_scope".into(),crate::model::Capability{status:crate::model::CapabilityStatus::Available,enforced:false,detail:format!("Only explicitly authorized GPU UUIDs are eligible: {:?}; other device activity is not a placement signal for this node envelope",capacity.gpu_memory_mib.keys().collect::<Vec<_>>())});
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

fn allocations_fit(allocations: &[crate::model::Allocation], capacity: &Resources) -> bool {
    let used = allocation_charge(allocations);
    used.cpu_millicores <= capacity.cpu_millicores
        && used.ram_mib <= capacity.ram_mib
        && used
            .gpu_memory_mib
            .iter()
            .all(|(id, n)| *n <= capacity.gpu_memory_mib.get(id).copied().unwrap_or(0))
}

fn remaining_capacity(capacity: &Resources, used: &Resources) -> Resources {
    Resources {
        cpu_millicores: capacity.cpu_millicores.saturating_sub(used.cpu_millicores),
        ram_mib: capacity.ram_mib.saturating_sub(used.ram_mib),
        gpu_memory_mib: capacity
            .gpu_memory_mib
            .iter()
            .map(|(id, n)| {
                (
                    id.clone(),
                    n.saturating_sub(used.gpu_memory_mib.get(id).copied().unwrap_or(0)),
                )
            })
            .collect(),
    }
}

fn allocation_charge(allocations: &[crate::model::Allocation]) -> Resources {
    let mut used = Resources::default();
    for allocation in allocations {
        used.cpu_millicores = used.cpu_millicores.saturating_add(
            allocation
                .requested
                .cpu_millicores
                .max(allocation.observed.as_ref().map_or(0, |o| o.cpu_millicores)),
        );
        used.ram_mib = used.ram_mib.saturating_add(
            allocation
                .requested
                .ram_mib
                .max(allocation.observed.as_ref().map_or(0, |o| o.ram_mib)),
        );
        let mut gpu = allocation.requested.gpu_memory_mib.clone();
        if let Some(observed) = &allocation.observed {
            for (id, n) in &observed.gpu_memory_mib {
                let entry = gpu.entry(id.clone()).or_default();
                *entry = (*entry).max(*n);
            }
        }
        for (id, n) in gpu {
            let entry = used.gpu_memory_mib.entry(id).or_default();
            *entry = entry.saturating_add(n);
        }
    }
    used
}

/// Two bounded lanes reserve control traffic so artifact queues cannot starve
/// authoritative renewals. Their rates sum to the configured per-agent budget.
struct NodeClient {
    rpc: RpcClient,
    replay_session: Option<String>,
    control: RateLimiter,
    artifacts: RateLimiter,
    cache_dir: PathBuf,
    cache_lock: tokio::sync::Mutex<()>,
    spool_root: PathBuf,
    max_spool_bytes: u64,
    namespace_guard: NamespaceGuard,
    storage_profile: crate::state::StorageProfile,
}
impl NodeClient {
    fn new(
        config: &AgentConfig,
        state_dir: &Path,
        storage_profile: crate::state::StorageProfile,
        namespace_guard: NamespaceGuard,
    ) -> Result<Self> {
        ensure!(
            config.max_transfer_bytes_per_second >= 10,
            "transfer budget must be at least ten bytes/second"
        );
        let control = config.max_transfer_bytes_per_second / 10;
        let cache_dir = state_dir.join("attempts").join(".input-cache");
        private_dir(&cache_dir)?;
        Ok(Self {
            rpc: RpcClient::with_settings(&config.coordinator_url, &config.tls, 15.0, 0)?,
            replay_session: None,
            control: RateLimiter::new(control),
            artifacts: RateLimiter::new(config.max_transfer_bytes_per_second - control),
            cache_dir,
            cache_lock: tokio::sync::Mutex::new(()),
            spool_root: state_dir.join("attempts"),
            max_spool_bytes: config.max_spool_bytes,
            namespace_guard,
            storage_profile,
        })
    }
    fn download_charge(&self, path: &Path) -> Result<crate::publication::CaptureCharge> {
        crate::publication::CaptureCharge::for_download(
            self.spool_root.parent().context("spool parent missing")?,
            self.storage_profile,
            self.namespace_guard.clone(),
            path,
            self.max_spool_bytes,
        )
    }
    fn write_json_new(&self, path: &Path, value: &impl Serialize) -> Result<()> {
        let charge = self.download_charge(path)?;
        // Receipt/control bytes share the node ledger with artifacts and logs.
        // After reservation, any write/sync uncertainty retains the full charge.
        write_json_new_reserved(path, value, Some(&charge))
    }
    async fn request(&self, request: &Request) -> Result<Response> {
        tokio::time::timeout(Duration::from_secs(15), self.request_paced(request))
            .await
            .context("node RPC exceeded its end-to-end queue/pacing/transport deadline")?
    }
    async fn request_control(
        &self,
        request: &Request,
        due: Instant,
        interval: Duration,
    ) -> Result<Response> {
        let started = Instant::now();
        let lateness = started.saturating_duration_since(due);
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
                Request::Renew { assignment_id, .. } => Some(assignment_id),
                _ => None,
            };
            let finished = Instant::now();
            let success = result
                .as_ref()
                .is_ok_and(|r| !matches!(r, Response::Error { .. }));
            emit(
                &serde_json::json!({"event":"control_rpc","operation":operation,"assignment_id":assignment,"scheduled_due_monotonic_ms":local_clock_ms()?.saturating_sub(finished.saturating_duration_since(due).as_millis() as u64),"enqueued_late_ms":lateness.as_secs_f64()*1000.0,"elapsed_ms":finished.saturating_duration_since(due).as_secs_f64()*1000.0,"request_elapsed_ms":finished.duration_since(started).as_secs_f64()*1000.0,"missed_schedule_slots":lateness.as_millis()/interval.as_millis().max(1),"success":success,"deadline_ms":10000}),
            )?;
        }
        result
    }
    async fn request_paced(&self, request: &Request) -> Result<Response> {
        let artifact = matches!(
            request,
            Request::BeginUpload { .. }
                | Request::UploadChunk { .. }
                | Request::CommitUpload { .. }
                | Request::ReadArtifact { .. }
                | Request::PublishCheckpoint { .. }
                | Request::Complete { .. }
        );
        let limiter = if artifact {
            &self.artifacts
        } else {
            &self.control
        };
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
        limiter
            .acquire(wire_charge(upload.saturating_add(estimated_reply)))
            .await;
        let response = self.rpc.request(wire_request).await?;
        if estimated_reply == 0 {
            limiter
                .acquire(wire_charge(serde_json::to_vec(&response)?.len() as u64))
                .await;
        }
        Ok(response)
    }
}

fn next_control_due(last: &mut Instant, interval: Duration) -> Instant {
    next_control_due_at(last, interval, Instant::now())
}
fn next_control_due_at(last: &mut Instant, interval: Duration, now: Instant) -> Instant {
    let due = *last + interval;
    let missed = now.saturating_duration_since(due).as_millis() / interval.as_millis().max(1);
    *last = due + interval.saturating_mul(missed.min(u32::MAX as u128) as u32);
    due
}
fn next_periodic_wake(due: Instant, interval: Duration, now: Instant) -> Instant {
    if now < due {
        return due;
    }
    let elapsed = now.saturating_duration_since(due).as_nanos();
    let intervals = elapsed / interval.as_nanos().max(1) + 1;
    due + interval.saturating_mul(intervals.min(u32::MAX as u128) as u32)
}
fn wire_charge(json_bytes: u64) -> u64 {
    json_bytes
        .saturating_add(json_bytes / 20)
        .saturating_add(4096)
}
struct RateLimiter {
    rate: u64,
    next: tokio::sync::Mutex<tokio::time::Instant>,
}
impl RateLimiter {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }
    async fn acquire(&self, bytes: u64) {
        let mut next = self.next.lock().await;
        let start = (*next).max(tokio::time::Instant::now());
        *next = start + Duration::from_secs_f64(bytes as f64 / self.rate as f64);
        drop(next);
        tokio::time::sleep_until(start).await;
    }
}

#[cfg(test)]
mod agent_transport_tests {
    use super::*;
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
        config.gpu.scale_up_cooldown_ms = 0;
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
            total_ram_mib: 16000,
            available_ram_mib: Some(12000),
            gpu_inventory: CapabilityStatus::Unknown,
            gpus: vec![],
            capabilities: BTreeMap::new(),
            kernel: None,
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
        assert!(!allocations_fit(&[exceeded], &capacity));
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
