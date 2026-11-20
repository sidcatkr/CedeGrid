//! Starting an assignment: inputs and checkpoints downloaded and verified, then
//! its supervisor launched.
use super::*;

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

/// What a supervisor's event wakes the agent for between intervals (006 SC-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    /// The launch reached its barrier: ask for its authorization now.
    Prepared,
    /// The task ended: run the next interval early, to report it and take the next task.
    Finished,
}

/// The wake an event calls for, if any.
pub(super) fn wake_for(event: &Event) -> Option<Wake> {
    match event {
        Event::Prepared { .. } => Some(Wake::Prepared),
        Event::Finished { .. } => Some(Wake::Finished),
        _ => None,
    }
}

/// The agent loop's wakes, which a supervisor's event reader signals.
#[derive(Clone, Default)]
pub(super) struct Wakes {
    pub(super) prepared: std::sync::Arc<tokio::sync::Notify>,
    pub(super) finished: std::sync::Arc<tokio::sync::Notify>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn start_assignment(
    config: &Config,
    agent: &AgentConfig,
    assignment: Assignment,
    capacity: &Resources,
    executable: &Path,
    client: &NodeClient,
    arrival_unix_ms: u64,
    wakes: Wakes,
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
    let context = serde_json::json!({"schema_version":2,"namespace_id":client.namespace_guard.identity().namespace_id,"session_id":client.namespace_guard.identity().session_id.as_ref().unwrap_or(&client.namespace_guard.identity().namespace_id),"task_id":assignment.request.task_id,"assignment_id":assignment.request.assignment_id,"generation":assignment.generation,"output_dir":workspace.join("output"),"metadata":metadata,"resume":resume,"inputs":inputs,"max_spool_bytes":agent.max_spool_bytes,"capabilities":["resource_targets"]});
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
                .expect("one GPU UUID was checked above")
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
        control_token: new_control_token()?,
        nofile_soft: STARTING_DESCRIPTOR_LIMIT.get().copied(),
        launched_at_unix_ms: Some(super::reconcile::now_unix_ms()),
    };
    let path = output.join("supervisor-spec.json");
    client.write_json_new(&path, &spec)?;
    // Other accounts' agents read the task's modes and its run's arrival from this argument,
    // read-only (006 FR-001); the hidden dispatch reads only the spec path before it.
    let marker = work_marker(config, &assignment.request, arrival_unix_ms);
    let mut command = Command::new(executable);
    command
        .arg("__assignment-supervisor")
        .arg(path)
        .arg(marker.to_arg())
        .env("TMPDIR", workspace.join("tmp"))
        .env("CEDEGRID_SUPERVISOR_STATE_ROOT", &config.state_dir)
        .env(
            "CEDEGRID_SUPERVISOR_NAMESPACE",
            serde_json::to_string(client.namespace_guard.identity())?,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A terminal's SIGINT or SIGHUP to the agent's group must not reach supervisors, which
    // outlive an agent stop for re-adoption (FR-080).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
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
            write_diagnostic(
                std::io::stderr(),
                format_args!("supervisor stderr capture failed: {error:#}"),
            );
        }
    });
    let input = Some(child.stdin.take().context("supervisor input absent")?);
    let stdout = child.stdout.take().context("supervisor output absent")?;
    let (tx, events) = mpsc::channel();
    let events_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let event = line
                .map_err(|e| e.to_string())
                .and_then(|l| serde_json::from_str(&l).map_err(|e| e.to_string()));
            let wake = event.as_ref().ok().and_then(wake_for);
            if tx.send(event).is_err() {
                break;
            }
            // After the send, so the woken loop finds the event.
            match wake {
                Some(Wake::Prepared) => wakes.prepared.notify_one(),
                Some(Wake::Finished) => wakes.finished.notify_one(),
                None => {}
            }
        }
    });
    Ok(ChildSlot {
        child: SupervisorProcess::Owned(child),
        input: input.map(|i| Box::new(i) as Box<dyn Write + Send>),
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
        // A new family launches at its baseline.
        sent_oom: Some(false),
        oom_context: None,
        ram_view: None,
        oom_verdict: None,
        seed: None,
        journal_seen: None,
        outside_family: 0,
        outside_reason: None,
        family: None,
    })
}

/// The marker for a task: its modes on CPU and GPU compute under the node cap, and its run's
/// arrival (006 FR-001).
fn work_marker(
    config: &Config,
    request: &crate::execution_model::LaunchRequest,
    arrival_unix_ms: u64,
) -> crate::execution_model::WorkMarker {
    let modes = crate::system_mode::job_modes(&config.contention, request.contention.as_ref());
    crate::execution_model::WorkMarker {
        cpu: modes[0],
        gpu: modes[2],
        arrival_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contention::{ContentionMode, ResourceKind};
    use crate::execution_model::{JobContention, LaunchRequest};

    fn request(modes: &[(ResourceKind, ContentionMode)]) -> LaunchRequest {
        let mut request: LaunchRequest = serde_json::from_value(serde_json::json!({
            "task_id": "t", "assignment_id": "a", "argv": ["unused"], "cwd": "/",
            "resources": {"cpu_millicores": 1000, "ram_mib": 64},
            "class": "opportunistic",
        }))
        .unwrap();
        request.contention = (!modes.is_empty()).then(|| JobContention {
            modes: modes.iter().copied().collect(),
            ..JobContention::default()
        });
        request
    }

    /// The marker carries the modes the task runs at, within the node cap, and the arrival.
    #[test]
    fn the_marker_carries_the_tasks_modes_under_the_cap() {
        let mut config = Config::default();
        assert_eq!(
            work_marker(&config, &request(&[]), 7).to_arg(),
            "cedegrid-work=v1;cpu=performance;gpu=performance;arrival=7"
        );
        let yielding = request(&[
            (ResourceKind::Cpu, ContentionMode::Neutral),
            (ResourceKind::GpuCompute, ContentionMode::YieldFirst),
        ]);
        assert_eq!(
            work_marker(&config, &yielding, 7).to_arg(),
            "cedegrid-work=v1;cpu=neutral;gpu=yield_first;arrival=7"
        );
        // A request beyond the node cap runs, and is marked, at the cap.
        config.contention.max_mode = Some(ContentionMode::Performance);
        let aggressive = request(&[(ResourceKind::Cpu, ContentionMode::Aggressive)]);
        assert_eq!(
            work_marker(&config, &aggressive, 7).cpu,
            ContentionMode::Performance
        );
    }
}
