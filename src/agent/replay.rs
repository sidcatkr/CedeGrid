//! Replay sessions: replayable storage, its quarantine and the session start.
use super::*;

pub(super) fn validate_replay_launch(config: &Config, request: &LaunchRequest) -> Result<()> {
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

pub(super) fn replay_parent(config: &Config, home: &Path) -> Result<PathBuf> {
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

pub(super) fn quarantine_replay_state(
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

pub(super) async fn start_replay_session(
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
            yield_cause: None,
            hint_ready: None,
            observes_drain: None,
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
        features: Vec::new(),
        contention: None,
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
