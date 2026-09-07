use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use resource_manager::{
    config::Config,
    model::{PolicyInput, Snapshot},
    policy::PolicyEngine,
    state::{self, StateStore},
    telemetry::Collector,
};
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
    #[arg(long, global = true, default_value = "resmgr.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
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
    ConfigExample,
    /// Validate configuration and show effective settings, without writing state.
    Validate,
    /// Inspect platform, telemetry, and actual storage filesystem without writing state.
    Doctor,
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
    // Preparation code starts before any runtime/worker threads or child reaper.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__worker-gate")) {
        return resource_manager::supervision::worker_gate();
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("__assignment-supervisor"))
    {
        let spec = std::env::args_os()
            .nth(2)
            .context("missing supervisor spec")?;
        return resource_manager::agent::assignment_supervisor(Path::new(&spec));
    }
    let cli = Cli::parse();
    match &cli.command {
        Command::StorageQualify { directory, profile } => {
            let profile = serde_json::from_value(serde_json::Value::String(profile.clone()))?;
            let report = resource_manager::storage_qualification::run(
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
            return resource_manager::storage_qualification::child_main(
                directory, profile, stage, token,
            );
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
    if matches!(cli.command, Command::ConfigExample) {
        print!("{}", serde_yaml::to_string(&Config::default())?);
        return Ok(());
    }
    match &cli.command {
        Command::Backup {
            state_dir,
            destination,
            max_bytes,
            max_files,
            timeout_seconds,
        } => {
            return emit(&resource_manager::backup::create(
                state_dir,
                destination,
                resource_manager::backup::Limits {
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
            return emit(&resource_manager::backup::restore(
                snapshot,
                destination,
                resource_manager::backup::Limits {
                    max_bytes: *max_bytes,
                    max_files: *max_files,
                    timeout: Duration::from_secs(*timeout_seconds),
                },
                *confirm_source_stopped,
            )?);
        }
        Command::Coordinator { deployment } => {
            let mut settings: resource_manager::protocol::CoordinatorConfig =
                read_deployment(deployment)?;
            resolve_deployment_path(deployment, &mut settings.state_dir)?;
            resolve_tls(deployment, &mut settings.tls)?;
            return resource_manager::coordinator::serve(settings).await;
        }
        Command::Rpc {
            deployment,
            request,
        } => {
            let request = read_deployment(request)?;
            return remote(deployment, request).await;
        }
        Command::Submit { deployment, job } => {
            return remote(
                deployment,
                resource_manager::protocol::Request::Submit {
                    job: read_deployment(job)?,
                },
            )
            .await;
        }
        Command::Status { deployment, job_id } => {
            return remote(
                deployment,
                resource_manager::protocol::Request::Status {
                    job_id: job_id.clone(),
                },
            )
            .await;
        }
        Command::Pool { deployment, spec } => {
            return remote(
                deployment,
                resource_manager::protocol::Request::PutPool {
                    pool: read_deployment(spec)?,
                },
            )
            .await;
        }
        Command::Cancel { deployment, job_id } => {
            return remote(
                deployment,
                resource_manager::protocol::Request::Cancel {
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
                resource_manager::protocol::Request::Retry {
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
                resource_manager::protocol::Request::DrainNode {
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
            let mut settings: resource_manager::agent::AgentConfig = read_deployment(&deployment)?;
            resolve_tls(&deployment, &mut settings.tls)?;
            resource_manager::agent::run(&config, &settings, &std::env::current_exe()?).await
        }
        Command::Reconcile => emit(&resource_manager::agent::reconcile(&config)?),
        Command::Supervise { .. }
        | Command::StorageQualify { .. }
        | Command::StorageQualificationChild { .. } => unreachable!(),
        Command::Executions => {
            let store =
                StateStore::open_read_only_with_profile(&config.state_dir, config.storage_profile)?;
            emit(&store.executions()?)
        }
        Command::ConfigExample => unreachable!(),
        Command::Validate => emit(&json!({"valid": true, "config": config})),
        Command::Doctor => {
            let storage = state::preflight(&config.state_dir)?;
            let mut collector = Collector::for_config(&config)?;
            let snapshot = sample_with_intent(&mut collector, &config)?;
            emit(&json!({
                "schema_version": resource_manager::model::SCHEMA_VERSION,
                "mode": "observe_only",
                "os": std::env::consts::OS,
                "architecture": std::env::consts::ARCH,
                "sqlite_version": rusqlite::version(),
                "sqlite_wal_fix_present": state::runtime_sqlite_supported(),
                "storage": storage,
                "configured_storage_profile": config.storage_profile,
                "requested_journal_mode": config.storage_profile.journal_mode(),
                "requested_synchronous": config.storage_profile.synchronous(),
                "snapshot": snapshot,
                "execution_available": cfg!(any(target_os="linux",target_os="macos")),
                "execution_configured": config.execution.enabled,
                "kernel_controls_applied": false
            }))?;
            ensure!(
                storage.supported,
                "storage preflight failed: {}",
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
    resource_manager::backup::ensure_runnable_state(&config.state_dir)?;
    use resource_manager::{
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
        config.node_mode == resource_manager::config::NodeMode::Guaranteed
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
    let _execution_lock = resource_manager::backup::execution_lock(&config.state_dir)?;
    let mut collector = Collector::for_config(config)?;
    let mut engine = PolicyEngine::new();
    let start = Instant::now();
    let capacity = loop {
        let mut snapshot = sample_with_intent(&mut collector, config)?;
        resource_manager::agent::restrict_gpu_scope(&mut snapshot, &request.resources);
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

fn read_deployment<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_yaml::from_slice(
        &std::fs::read(path)
            .with_context(|| format!("cannot read deployment {}", path.display()))?,
    )
    .with_context(|| format!("invalid deployment {}", path.display()))
}
fn resolve_deployment_path(deployment: &Path, path: &mut PathBuf) -> Result<()> {
    if path.is_relative() {
        *path = deployment
            .canonicalize()?
            .parent()
            .context("deployment has no parent")?
            .join(&*path);
    }
    Ok(())
}
fn resolve_tls(deployment: &Path, tls: &mut resource_manager::protocol::TlsIdentity) -> Result<()> {
    for path in [&mut tls.ca_cert, &mut tls.certificate, &mut tls.private_key] {
        resolve_deployment_path(deployment, path)?;
    }
    Ok(())
}
async fn remote(deployment: &Path, request: resource_manager::protocol::Request) -> Result<()> {
    let mut client: resource_manager::protocol::ClientConfig = read_deployment(deployment)?;
    resolve_tls(deployment, &mut client.tls)?;
    emit(&resource_manager::protocol::rpc(&client.endpoint, &client.tls, &request).await?)
}
