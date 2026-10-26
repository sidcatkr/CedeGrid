//! Restart reconciliation: what durable execution records mean after a restart,
//! and reattaching live supervisors.
use super::*;

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
pub(super) fn native_pid_absent(pid: u32) -> bool {
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

/// Whether the attempt's supervisor is verifiably the same live process.
fn live_supervisor(config: &Config, record: &ExecutionRecord) -> Option<ProcessIdentity> {
    let output = config
        .state_dir
        .join("attempts")
        .join(&record.assignment_id);
    let bytes =
        crate::publication::read_bounded(&output.join("supervisor-identity.json"), 64 * 1024)
            .ok()?;
    let identity: ProcessIdentity = serde_json::from_slice(&bytes).ok()?;
    (identity.assignment_id == record.assignment_id
        && identity.generation == record.generation
        && supervision::process_identity(
            identity.pid,
            &identity.assignment_id,
            identity.generation,
        )
        .ok()
        .as_ref()
            == Some(&identity)
        && !identity_absent(&identity))
    .then_some(identity)
}

/// Reconnect to live supervisors after an agent restart (research R2). Each one is
/// verified by identity and authenticated by the token from its spec, and reports
/// its applied steps, lease sequence and adjust sequence.
#[cfg(unix)]
pub(super) fn reattach_supervisors(config: &Config, store: &StateStore) -> Result<Vec<ChildSlot>> {
    let mut slots = Vec::new();
    for record in store.executions()?.into_iter().filter(|r| {
        matches!(
            r.phase,
            ExecutionPhase::Authorized | ExecutionPhase::Running | ExecutionPhase::Draining
        )
    }) {
        match reattach_supervisor(config, &record) {
            Ok(slot) => {
                emit(
                    &serde_json::json!({"event":"supervisor_reattached","assignment_id":record.assignment_id,
                    "lease_sequence":slot.sequence,"applied":slot.applied}),
                )?;
                slots.push(slot);
            }
            Err(error) => emit(&serde_json::json!({"event":"supervisor_not_reattached",
                "assignment_id":record.assignment_id,"detail":format!("{error:#}")}))?,
        }
    }
    Ok(slots)
}

#[cfg(unix)]
fn reattach_supervisor(config: &Config, record: &ExecutionRecord) -> Result<ChildSlot> {
    let output = config
        .state_dir
        .join("attempts")
        .join(&record.assignment_id);
    let spec: SupervisorSpec = serde_json::from_slice(&crate::publication::read_bounded(
        &output.join("supervisor-spec.json"),
        3 * 1024 * 1024,
    )?)?;
    ensure!(
        spec.assignment.request.assignment_id == record.assignment_id
            && spec.assignment.generation == record.generation,
        "supervisor spec identity mismatch"
    );
    let token = spec
        .control_token
        .clone()
        .context("supervisor has no control socket")?;
    let identity = live_supervisor(config, record).context("supervisor is not live")?;
    let socket: serde_json::Value = serde_json::from_slice(&crate::publication::read_bounded(
        &output.join("control-socket.json"),
        64 * 1024,
    )?)?;
    let path = PathBuf::from(
        socket["path"]
            .as_str()
            .context("control socket path missing")?,
    );
    let stream = supervision::control_socket::connect(&path, &token)?;
    let reader = stream.try_clone()?;
    let (tx, events) = mpsc::channel();
    let events_thread = std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let event = line
                .map_err(|e| e.to_string())
                .and_then(|l| serde_json::from_str(&l).map_err(|e| e.to_string()));
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    let mut slot = ChildSlot {
        child: SupervisorProcess::Attached(identity),
        input: Some(Box::new(stream)),
        events,
        assignment: spec.assignment,
        output_dir: output,
        sequence: 0,
        last_renew: Instant::now(),
        prepared: None,
        reported: false,
        checkpoint_hash: None,
        exited: false,
        capture_threads: vec![events_thread],
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
        seed: None,
    };
    send(&mut slot, &Control::Query)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match slot
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(Ok(Event::State {
                applied,
                targets,
                lease_sequence,
                sequence,
                ..
            })) => {
                // Continue above the supervisor's sequences and renew its lease.
                slot.sequence = lease_sequence;
                slot.adjust_sequence = sequence;
                let steps: BTreeMap<_, _> = applied
                    .iter()
                    .filter(|(_, step)| **step != crate::contention::YieldStep::None)
                    .map(|(key, step)| {
                        (
                            key.clone(),
                            crate::contention::StepRequest {
                                step: *step,
                                target: targets.get(key).copied(),
                                cause: crate::contention::YieldCause::ExternalGrowth,
                                deadline_ms: None,
                            },
                        )
                    })
                    .collect();
                slot.sent_steps = steps.clone();
                slot.seed = Some(steps);
                slot.applied = applied;
                slot.applied_targets = targets;
                return Ok(slot);
            }
            Ok(_) => {}
            Err(_) => bail!("reattached supervisor reported no state"),
        }
    }
}

fn reconcile_unlocked(config: &Config, exclude: &BTreeSet<String>) -> Result<Vec<ExecutionRecord>> {
    let guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    reconcile_guarded(config, exclude, guard)
}

pub(super) fn reconcile_agent_owned<'a>(
    config: &Config,
    running: impl Iterator<Item = &'a String>,
    preparing: impl Iterator<Item = &'a String>,
    guard: NamespaceGuard,
) -> Result<Vec<ExecutionRecord>> {
    // A preparation owns its durable reservation before it returns a ChildSlot.
    // Retain that ownership until the JoinHandle is harvested, including when a
    // completed preparation is waiting for the next service-loop iteration.
    let owned = running.chain(preparing).cloned().collect();
    reconcile_guarded(config, &owned, guard)
}

pub(super) fn reconcile_guarded(
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
        // A verifiably live supervisor owns its journal; rewriting it would turn its
        // later drain into a rejected transition (audit H5).
        if live_supervisor(config, &record).is_some() {
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
