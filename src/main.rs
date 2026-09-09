use anyhow::{Context, Result, ensure};
use cedegrid::{
    config::Config,
    model::{PolicyInput, Snapshot},
    policy::PolicyEngine,
    state::{self, StateStore},
    telemetry::Collector,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    version,
    about = "Durable resource scheduling, authenticated node services, and explicit workload supervision."
)]
struct Cli {
    #[arg(long, global = true, default_value = "cedegrid.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorRole {
    Agent,
    Coordinator,
}

#[derive(Subcommand)]
enum Command {
    /// Bounded linked-SQLite process-crash diagnostics in a new home directory.
    /// Does not override production filesystem durability admission.
    StorageQualify {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value = "wal_full", value_parser = ["wal_full", "delete_extra", "burst_replay_delete_extra"])]
        profile: String,
    },
    #[command(hide = true)]
    StorageQualificationChild {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, value_parser = ["wal_full", "delete_extra", "burst_replay_delete_extra"])]
        profile: String,
        #[arg(long)]
        stage: String,
        #[arg(long)]
        token: String,
    },
    /// Create an immutable offline coordinator snapshot (no PKI or agent outboxes).
    Backup {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long, default_value_t = 21474836480)]
        max_bytes: u64,
        #[arg(long, default_value_t = 100000)]
        max_files: usize,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Restore into a new private directory; retain and fence uncertain allocations.
    Restore {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        confirm_source_stopped: bool,
        #[arg(long, default_value_t = 21474836480)]
        max_bytes: u64,
        #[arg(long, default_value_t = 100000)]
        max_files: usize,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Run the single active coordinator with mandatory client-certificate authentication.
    Coordinator {
        #[arg(long)]
        deployment: PathBuf,
    },
    /// Run an explicitly enabled node agent with an operator-approved resource ceiling.
    Agent {
        #[arg(long)]
        deployment: PathBuf,
    },
    /// Send a version-one authenticated JSON RPC request.
    Rpc {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit an immutable, idempotent job specification.
    Submit {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        job: PathBuf,
    },
    /// Query jobs, pools, node reports, and retained reservations.
    Status {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        job_id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, value_enum)]
        collection: Option<cedegrid::pagination::Collection>,
        #[arg(long)]
        node_id: Option<String>,
        #[arg(long)]
        pool_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Create or update an elastic pool specification.
    Pool {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        spec: PathBuf,
    },
    /// Stop future placement and request draining of a job's active allocations.
    Cancel {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        job_id: String,
    },
    /// Retry a released task from its last durable checkpoint; unsafe side effects require acknowledgement.
    Resume {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        side_effects_reconciled: bool,
    },
    /// Drain a node, or permit placement again with --resume.
    Drain {
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        resume: bool,
    },
    /// Reconcile local reservations without signaling or adopting numeric PIDs.
    Reconcile,

    /// Explicit local CPU-only execution behind a durable preparation barrier.
    Supervise { job: PathBuf },
    /// Inspect retained local execution reservations; never reattach or signal a PID.
    Executions,
    /// Print a portable example configuration to stdout.
    ConfigExample {
        #[arg(long, value_enum, default_value_t = cedegrid::config::RuntimeConfigKind::Node)]
        kind: cedegrid::config::RuntimeConfigKind,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// Validate configuration and show effective settings, without writing state.
    Validate,
    /// Inspect platform, telemetry, and actual storage filesystem without writing state.
    Doctor {
        #[arg(long, value_enum, default_value = "agent")]
        role: DoctorRole,
    },
    /// Sample resources, explain policy, and optionally persist observations.
    Observe {
        /// Number of samples; 0 continues until Ctrl-C or SIGTERM.
        #[arg(long, default_value_t = 1)]
        samples: u64,
        /// Print observations without creating or opening a database.
        #[arg(long)]
        no_state: bool,
    },
    /// Evaluate a JSON array of synthetic frames. No real workloads are managed.
    Replay { input: PathBuf },
    /// Read recent persisted observations.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    Migrate {
        #[arg(long, value_enum)]
        kind: cedegrid::config::RuntimeConfigKind,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum)]
        legacy_client_semantics: Option<cedegrid::config_migration::LegacyClientSemantics>,
        #[arg(long)]
        legacy_cwd: Option<PathBuf>,
    },
}
#[derive(Subcommand)]
enum StateCommand {
    Upgrade {
        #[arg(long)]
        backup: PathBuf,
        #[arg(long)]
        confirm_legacy_stopped: bool,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayFrame {
    elapsed_ms: u64,
    snapshot: Snapshot,
    #[serde(default)]
    input: PolicyInput,
}

fn read_config(path: &Path) -> Result<Config> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve configuration {}", path.display()))?;
    let mut config = Config::load(&canonical)?;
    config.kernel.monitor_interval_ms = config.monitor.interval_ms;
    if config.kernel.delegated_root.is_none() {
        config.kernel.delegated_root = config.cgroup.delegated_root.clone();
    }
    if config.state_dir.is_relative() {
        config.state_dir = canonical
            .parent()
            .context("configuration has no parent directory")?
            .join(&config.state_dir);
    }
    config.validate()?;
    Ok(config)
}

fn emit(value: &impl Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let _foreground = cedegrid::launcher_terminal::Foreground::enter()?;
    #[cfg(windows)]
    ensure!(
        !matches!(
            std::env::args_os()
                .nth(1)
                .and_then(|v| v.into_string().ok())
                .as_deref(),
            Some("__worker-gate" | "__assignment-supervisor")
        ),
        "ERR_CEDEGRID_UNSUPPORTED_PLATFORM: Windows supports client commands only"
    );
    // Preparation code starts before any runtime/worker threads or child reaper.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__worker-gate")) {
        return cedegrid::supervision::worker_gate();
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("__assignment-supervisor"))
    {
        let spec = std::env::args_os()
            .nth(2)
            .context("missing supervisor spec")?;
        return cedegrid::agent::assignment_supervisor(Path::new(&spec));
    }
    let cli = Cli::parse();
    #[cfg(windows)]
    ensure!(
        matches!(
            &cli.command,
            Command::Rpc { .. }
                | Command::Submit { .. }
                | Command::Status { .. }
                | Command::Pool { .. }
                | Command::Cancel { .. }
                | Command::Resume { .. }
                | Command::Drain { .. }
                | Command::ConfigExample { .. }
                | Command::Config { .. }
                | Command::Validate
                | Command::Doctor { .. }
                | Command::Observe { no_state: true, .. }
                | Command::Replay { .. }
        ),
        "ERR_CEDEGRID_UNSUPPORTED_PLATFORM: Windows supports client commands only"
    );
    match &cli.command {
        Command::StorageQualify { directory, profile } => {
            let profile = serde_json::from_value(serde_json::Value::String(profile.clone()))?;
            let report = cedegrid::storage_qualification::run(
                directory,
                profile,
                &std::env::current_exe()?,
            )?;
            emit(&report)?;
            ensure!(
                report.process_crash_compatibility_passed,
                "bounded storage qualification failed; inspect the emitted report"
            );
            return Ok(());
        }
        Command::StorageQualificationChild {
            directory,
            profile,
            stage,
            token,
        } => {
            let profile = serde_json::from_value(serde_json::Value::String(profile.clone()))?;
            return cedegrid::storage_qualification::child_main(directory, profile, stage, token);
        }
        _ => {}
    }
    if let Command::Supervise { job } = &cli.command {
        return run_supervisor(&read_config(&cli.config)?, job);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    if let Command::ConfigExample { kind } = &cli.command {
        print!("{}", cedegrid::config::example(*kind)?);
        return Ok(());
    }
    match &cli.command {
        Command::Config {
            command:
                ConfigCommand::Migrate {
                    kind,
                    input,
                    output,
                    legacy_client_semantics,
                    legacy_cwd,
                },
        } => {
            return emit(&cedegrid::config_migration::migrate(
                &cedegrid::config_migration::MigrationOptions {
                    kind: *kind,
                    input: input.clone(),
                    output: output.clone(),
                    legacy_client_semantics: *legacy_client_semantics,
                    legacy_cwd: legacy_cwd.clone(),
                },
            )?);
        }
        Command::State {
            command:
                StateCommand::Upgrade {
                    backup,
                    confirm_legacy_stopped,
                },
        } => {
            let config = read_config(&cli.config)?;
            let backup = if backup.is_absolute() {
                backup.clone()
            } else {
                std::env::current_dir()?.join(backup)
            };
            return emit(&cedegrid::upgrade::upgrade(
                &config.state_dir,
                &backup,
                config.storage_profile,
                *confirm_legacy_stopped,
            )?);
        }
        Command::Backup {
            state_dir,
            destination,
            max_bytes,
            max_files,
            timeout_seconds,
        } => {
            return emit(&cedegrid::backup::create(
                state_dir,
                destination,
                cedegrid::backup::Limits {
                    max_bytes: *max_bytes,
                    max_files: *max_files,
                    timeout: Duration::from_secs(*timeout_seconds),
                },
            )?);
        }
        Command::Restore {
            snapshot,
            destination,
            confirm_source_stopped,
            max_bytes,
            max_files,
            timeout_seconds,
        } => {
            return emit(&cedegrid::backup::restore(
                snapshot,
                destination,
                cedegrid::backup::Limits {
                    max_bytes: *max_bytes,
                    max_files: *max_files,
                    timeout: Duration::from_secs(*timeout_seconds),
                },
                *confirm_source_stopped,
            )?);
        }
        Command::Coordinator { deployment } => {
            let settings = cedegrid::config::load_runtime(
                deployment,
                cedegrid::config::RuntimeConfigKind::Coordinator,
            )?;
            return cedegrid::coordinator::serve(settings).await;
        }
        Command::Rpc {
            deployment,
            request,
        } => {
            let request = read_json_data(request)?;
            return remote(deployment, request).await;
        }
        Command::Submit { deployment, job } => {
            return remote(
                deployment,
                cedegrid::protocol::Request::Submit {
                    job: read_json_data(job)?,
                },
            )
            .await;
        }
        Command::Status {
            deployment,
            job_id,
            all,
            collection,
            node_id,
            pool_id,
            limit,
            cursor,
        } => {
            let settings: cedegrid::config::RuntimeClientConfig = cedegrid::config::load_runtime(
                deployment,
                cedegrid::config::RuntimeConfigKind::Client,
            )?;
            let client = cedegrid::protocol::RpcClient::with_settings(
                &settings.endpoint,
                &settings.tls,
                settings.timeout_seconds,
                settings.max_transfer_bytes_per_second,
            )?;
            let mut query = cedegrid::pagination::PageQuery {
                collection: collection.clone().unwrap_or(if job_id.is_some() {
                    cedegrid::pagination::Collection::Tasks
                } else {
                    cedegrid::pagination::Collection::Jobs
                }),
                job_id: job_id.clone(),
                node_id: node_id.clone(),
                pool_id: pool_id.clone(),
                limit: *limit,
                cursor: cursor.clone(),
            };
            loop {
                let response = client
                    .request(&cedegrid::protocol::Request::StatusPage {
                        query: query.clone(),
                    })
                    .await?;
                emit(&response)?;
                let cedegrid::protocol::Response::StatusPage { page } = response else {
                    anyhow::bail!("unexpected status page response")
                };
                if !all || page.next_cursor.is_none() {
                    break;
                }
                query.cursor = page.next_cursor;
            }
            return Ok(());
        }
        Command::Pool { deployment, spec } => {
            return remote(
                deployment,
                cedegrid::protocol::Request::PutPool {
                    pool: read_json_data(spec)?,
                },
            )
            .await;
        }
        Command::Cancel { deployment, job_id } => {
            return remote(
                deployment,
                cedegrid::protocol::Request::Cancel {
                    job_id: job_id.clone(),
                },
            )
            .await;
        }
        Command::Resume {
            deployment,
            task_id,
            side_effects_reconciled,
        } => {
            return remote(
                deployment,
                cedegrid::protocol::Request::Retry {
                    task_id: task_id.clone(),
                    confirm_side_effects_reconciled: *side_effects_reconciled,
                },
            )
            .await;
        }
        Command::Drain {
            deployment,
            node_id,
            resume,
        } => {
            return remote(
                deployment,
                cedegrid::protocol::Request::DrainNode {
                    node_id: node_id.clone(),
                    drain: !resume,
                },
            )
            .await;
        }
        _ => {}
    }
    let config = read_config(&cli.config)?;
    match cli.command {
        Command::Backup { .. }
        | Command::Restore { .. }
        | Command::Coordinator { .. }
        | Command::Rpc { .. }
        | Command::Submit { .. }
        | Command::Status { .. }
        | Command::Pool { .. }
        | Command::Cancel { .. }
        | Command::Drain { .. }
        | Command::Resume { .. } => unreachable!(),
        Command::Agent { deployment } => {
            let settings = cedegrid::config::load_runtime(
                &deployment,
                cedegrid::config::RuntimeConfigKind::Agent,
            )?;
            cedegrid::agent::run(&config, &settings, &std::env::current_exe()?).await
        }
        Command::Reconcile => emit(&cedegrid::agent::reconcile(&config)?),
        Command::Supervise { .. }
        | Command::StorageQualify { .. }
        | Command::StorageQualificationChild { .. } => unreachable!(),
        Command::Executions => {
            let store =
                StateStore::open_read_only_with_profile(&config.state_dir, config.storage_profile)?;
            emit(&store.executions()?)
        }
        Command::ConfigExample { .. } | Command::Config { .. } | Command::State { .. } => {
            unreachable!()
        }
        Command::Validate => emit(&json!({"valid": true, "config": config})),
        Command::Doctor { role } => {
            let storage = state::preflight(&config.state_dir)?;
            let role_admitted = storage.admitted_by(config.storage_profile)
                && (role != DoctorRole::Coordinator
                    || config.storage_profile != state::StorageProfile::BurstReplayDeleteExtra);
            let mut collector = Collector::for_config(&config)?;
            let snapshot = sample_with_intent(&mut collector, &config)?;
            emit(&json!({
                "schema_version": cedegrid::model::SCHEMA_VERSION,
                "mode": "observe_only",
                "os": std::env::consts::OS,
                "architecture": std::env::consts::ARCH,
                "sqlite_version": rusqlite::version(),
                "sqlite_wal_fix_present": state::runtime_sqlite_supported(),
                "storage": storage,
                "strict_durability_supported": storage.supported,
                "selected_profile_admitted": storage.admitted_by(config.storage_profile),
                "configured_storage_profile": config.storage_profile,
                "requested_role": role,
                "role_admitted": role_admitted,
                "requested_journal_mode": config.storage_profile.journal_mode(),
                "requested_synchronous": config.storage_profile.synchronous(),
                "snapshot": snapshot,
                "execution_available": cfg!(any(target_os="linux",target_os="macos")),
                "execution_configured": config.execution.enabled,
                "kernel_controls_applied": false
            }))?;
            ensure!(
                role_admitted,
                "storage or requested-role preflight failed: {}",
                storage.detail
            );
            ensure!(
                state::runtime_sqlite_supported(),
                "linked SQLite must contain the WAL-reset fix (>=3.51.3)"
            );
            Ok(())
        }
        Command::Observe { samples, no_state } => {
            let store = if no_state {
                None
            } else {
                Some(StateStore::open_with_profile(
                    &config.state_dir,
                    config.storage_profile,
                )?)
            };
            let mut collector = Collector::for_config(&config)?;
            let mut engine = PolicyEngine::new();
            let start = Instant::now();
            let mut count = 0u64;
            let shutdown = shutdown_signal();
            tokio::pin!(shutdown);
            loop {
                let snapshot = sample_with_intent(&mut collector, &config)?;
                // Reservations are retained without adopting or signaling persisted PIDs.
                let input = PolicyInput {
                    allocations: store
                        .as_ref()
                        .map(StateStore::execution_allocations)
                        .transpose()?
                        .unwrap_or_default(),
                    explicit_drain: false,
                };
                let decision = engine.evaluate(
                    &config,
                    &snapshot,
                    &input,
                    start.elapsed().as_millis().try_into()?,
                )?;
                let id = store
                    .as_ref()
                    .map(|s| s.append_observation(&snapshot, &decision))
                    .transpose()?;
                emit(&json!({"observation_id": id, "snapshot": snapshot, "decision": decision}))?;
                count = count.saturating_add(1);
                if samples != 0 && count >= samples {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(config.monitor.interval_ms)) => {},
                    signal = &mut shutdown => { signal?; break; }
                }
            }
            Ok(())
        }
        Command::Replay { input } => {
            let frames: Vec<ReplayFrame> = serde_json::from_slice(
                &std::fs::read(&input)
                    .with_context(|| format!("cannot read replay {}", input.display()))?,
            )?;
            let mut engine = PolicyEngine::new();
            let mut previous = None;
            for frame in frames {
                ensure!(
                    previous.is_none_or(|p| frame.elapsed_ms >= p),
                    "replay elapsed_ms must be monotonic"
                );
                previous = Some(frame.elapsed_ms);
                let decision =
                    engine.evaluate(&config, &frame.snapshot, &frame.input, frame.elapsed_ms)?;
                emit(&json!({"elapsed_ms": frame.elapsed_ms, "decision": decision}))?;
            }
            Ok(())
        }
        Command::History { limit } => {
            ensure!(
                (1..=10_000).contains(&limit),
                "history limit must be between 1 and 10000"
            );
            ensure!(
                config.state_dir.join(state::DATABASE_FILENAME).is_file(),
                "no existing observation database"
            );
            let store =
                StateStore::open_read_only_with_profile(&config.state_dir, config.storage_profile)?;
            emit(&store.observations(limit)?)
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("Ctrl-C listener failed"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("Ctrl-C listener failed")
    }
}

fn run_supervisor(config: &Config, job: &Path) -> Result<()> {
    ensure!(
        !config.storage_profile.is_replayable(),
        "replayable storage requires an authenticated coordinator agent session"
    );
    cedegrid::backup::ensure_runnable_state(&config.state_dir)?;
    use cedegrid::{
        cgroup::CgroupBackend,
        execution_model::*,
        rootless::RootlessBackend,
        supervision::{self, SupervisorOptions},
    };
    ensure!(
        config.execution.enabled,
        "execution is disabled; explicitly set execution.enabled in configuration"
    );
    let request: LaunchRequest = serde_json::from_slice(&std::fs::read(job)?)?;
    ensure!(
        request.resources.gpu_memory_mib.is_empty(),
        "local supervise supports CPU-only commands; GPU workloads require the continuously observing agent service"
    );
    ensure!(
        config.node_mode == cedegrid::config::NodeMode::Guaranteed
            || request.class == AllocationClass::Opportunistic,
        "opportunistic nodes cannot host a guaranteed allocation"
    );
    let mut backend: Box<dyn LaunchBackend> = if config.cgroup.enabled {
        match CgroupBackend::new(config.cgroup.clone()) {
            Ok(backend) => Box::new(backend),
            Err(error) if request.allow_fallback => {
                ensure!(
                    request
                        .required_controls
                        .iter()
                        .all(|c| !c.starts_with("cgroup.")
                            && !["cpu.weight", "cpu.max", "memory.high", "memory.max"]
                                .contains(&c.as_str())),
                    "required cgroup control unavailable: {error:#}"
                );
                let mut baseline = RootlessBackend::new(config.cpu.nice);
                baseline.fallback_evidence.push(ControlEvidence {
                    control: "cgroup_backend".into(),
                    available: None,
                    permitted: None,
                    configured: true,
                    applied: false,
                    fallback: true,
                    scope: config
                        .cgroup
                        .delegated_root
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    requested: Some("cgroup_v2".into()),
                    effective: Some("rootless".into()),
                    detail: format!("Explicitly permitted pre-preparation fallback: {error:#}"),
                });
                Box::new(baseline)
            }
            Err(error) => return Err(error),
        }
    } else {
        Box::new(RootlessBackend::new(config.cpu.nice))
    };
    let store = StateStore::open_with_profile(&config.state_dir, config.storage_profile)?;
    let _execution_lock = cedegrid::backup::execution_lock(&config.state_dir)?;
    let mut collector = Collector::for_config(config)?;
    let mut engine = PolicyEngine::new();
    let start = Instant::now();
    let capacity = loop {
        let mut snapshot = sample_with_intent(&mut collector, config)?;
        cedegrid::agent::restrict_gpu_scope(&mut snapshot, &request.resources);
        let input = PolicyInput {
            allocations: store.execution_allocations()?,
            explicit_drain: false,
        };
        let decision = engine.evaluate(
            config,
            &snapshot,
            &input,
            start.elapsed().as_millis().try_into()?,
        )?;
        if decision.expansion_allowed {
            break decision.managed_budget;
        }
        ensure!(
            start.elapsed() < Duration::from_millis(config.execution.admission_timeout_ms),
            "admission remains blocked: {}",
            decision.reasons.join("; ")
        );
        std::thread::sleep(Duration::from_millis(config.monitor.interval_ms));
    };
    let options = SupervisorOptions {
        nice: config.cpu.nice,
        drain_timeout_ms: config.lifecycle.drain_timeout_ms,
        term_grace_ms: config.lifecycle.term_grace_ms,
        lease_ms: config.lifecycle.allocation_lease_ms,
        prepare_timeout_ms: config.execution.prepare_timeout_ms,
        release_confirm_timeout_ms: config.execution.release_confirm_timeout_ms,
    };
    let outcome = match supervision::supervise(
        &request,
        &capacity,
        &options,
        &store,
        backend.as_mut(),
        &std::env::current_exe()?,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            // Only confirmed release permits retry. Uncertain allocations keep their
            // reservation and cannot be displaced by a replacement attempt.
            if let Some(record) = store.executions()?.into_iter().find(|e| {
                e.assignment_id == request.assignment_id && e.phase == ExecutionPhase::Released
            }) {
                store.mark_uncertain(&record.task_id, record.generation)?;
            }
            return Err(error);
        }
    };
    if outcome.exit_code == Some(0) {
        // This is a durable command-exit receipt, not artifact publication.
        store.accept_result(
            &outcome.record.task_id,
            outcome.record.generation,
            &format!(
                "local-exit:0:{}:{}",
                outcome.record.assignment_id, outcome.record.generation
            ),
        )?;
    } else {
        store.mark_uncertain(&outcome.record.task_id, outcome.record.generation)?;
    }
    emit(&outcome)?;
    ensure!(
        outcome.exit_code == Some(0),
        "workload did not exit successfully"
    );
    Ok(())
}

/// Intent is separate from read-only capability evidence and actual launch readbacks.
fn sample_with_intent(collector: &mut Collector, config: &Config) -> Result<Snapshot> {
    let mut snapshot = collector.sample()?;
    if let Some(kernel) = &mut snapshot.kernel {
        for evidence in &mut kernel.controls {
            let requested = match evidence.control.as_str() {
                "cpu.weight" => config.cgroup.cpu_weight.map(|v| v.to_string()),
                "cpu.max" => config
                    .cgroup
                    .cpu_max
                    .as_ref()
                    .map(|v| format!("{} {}", v.quota_us, v.period_us)),
                "memory.high" => config.cgroup.memory_high_mib.map(|v| format!("{v} MiB")),
                "memory.max" => config.cgroup.memory_max_mib.map(|v| format!("{v} MiB")),
                "cgroup.procs" => Some("verified workload membership".into()),
                "cgroup.kill" if config.cgroup.allow_kill => Some("explicit cleanup policy".into()),
                _ => None,
            };
            if config.cgroup.enabled && requested.is_some() {
                evidence.configured = true;
                evidence.requested = requested;
                evidence.applied = false;
            }
        }
    }
    Ok(snapshot)
}

fn read_json_data<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    cedegrid::numeric::from_slice(
        &std::fs::read(path)
            .with_context(|| format!("cannot read JSON data {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON data {}", path.display()))
}
async fn remote(deployment: &Path, request: cedegrid::protocol::Request) -> Result<()> {
    let client: cedegrid::config::RuntimeClientConfig =
        cedegrid::config::load_runtime(deployment, cedegrid::config::RuntimeConfigKind::Client)?;
    emit(
        &cedegrid::protocol::RpcClient::with_settings(
            &client.endpoint,
            &client.tls,
            client.timeout_seconds,
            client.max_transfer_bytes_per_second,
        )?
        .request(&request)
        .await?,
    )
}
