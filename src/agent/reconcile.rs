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
    let _lock =
        super::AgentLock::acquire(lock, "cannot reconcile while a live agent owns node state")?;
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
/// Re-adopt every live supervisor of an authorized run. Returns the slots, and the records
/// whose supervisor is live but did not answer, for the loop to try again (FR-080).
pub(super) fn reattach_supervisors(
    config: &Config,
    store: &StateStore,
) -> Result<(Vec<ChildSlot>, Vec<ExecutionRecord>)> {
    let mut slots = Vec::new();
    let mut missed = Vec::new();
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
            Err(error) => {
                emit(&serde_json::json!({"event":"supervisor_not_reattached",
                "assignment_id":record.assignment_id,"detail":format!("{error:#}")}))?;
                if live_supervisor(config, &record).is_some() {
                    missed.push(record);
                }
            }
        }
    }
    Ok((slots, missed))
}

/// Try a missed re-adoption again: the slot once the supervisor answers, `None` while it is
/// live and silent, and an error once it is gone.
#[cfg(unix)]
pub(super) fn reattach_again(
    config: &Config,
    record: &ExecutionRecord,
) -> Result<Option<ChildSlot>> {
    match reattach_supervisor(config, record) {
        Ok(slot) => {
            emit(
                &serde_json::json!({"event":"supervisor_reattached","assignment_id":record.assignment_id,
                "lease_sequence":slot.sequence,"applied":slot.applied,"retried":true}),
            )?;
            Ok(Some(slot))
        }
        Err(_) if live_supervisor(config, record).is_some() => Ok(None),
        Err(error) => Err(error),
    }
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
    let (started_at, checkpoint) = restored_times(spec.launched_at_unix_ms, &output);
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
        // The checkpoint already published is not published again.
        checkpoint_hash: checkpoint.as_ref().map(|(_, hash)| hash.clone()),
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
        started_at,
        last_checkpoint_at: checkpoint.as_ref().map(|(at, _)| *at),
        adjust_sequence: 0,
        sent_steps: BTreeMap::new(),
        sent_allowance: None,
        // A reattached family's mark is unknown until the next decision sends it.
        sent_oom: None,
        oom_context: None,
        ram_view: None,
        oom_verdict: None,
        seed: None,
        journal_seen: None,
        outside_family: 0,
        outside_reason: None,
        family: None,
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
                causes,
                outside_family,
                outside_reason,
                family,
                ..
            })) => {
                slot.note_outside(outside_family, outside_reason);
                slot.family = family;
                // Continue above the supervisor's sequences and renew its lease. Only authorized
                // runs are re-adopted, and an initial authorization is sequence 1, which the
                // supervisor does not count until a renewal; 0 would ask to authorize again.
                slot.sequence = lease_sequence.max(1);
                slot.adjust_sequence = sequence;
                let steps = reattached_steps(&applied, &targets, &causes);
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

/// The steps a reattached supervisor holds, as the policy's seed. An older supervisor reports
/// no causes; its steps count as external growth.
pub(super) fn reattached_steps(
    applied: &BTreeMap<crate::contention::ResourceKey, crate::contention::YieldStep>,
    targets: &BTreeMap<crate::contention::ResourceKey, u64>,
    causes: &BTreeMap<crate::contention::ResourceKey, crate::contention::YieldCause>,
) -> BTreeMap<crate::contention::ResourceKey, crate::contention::StepRequest> {
    applied
        .iter()
        .filter(|(_, step)| **step != crate::contention::YieldStep::None)
        .map(|(key, step)| {
            (
                key.clone(),
                crate::contention::StepRequest {
                    step: *step,
                    target: targets.get(key).copied(),
                    cause: causes
                        .get(key)
                        .copied()
                        .unwrap_or(crate::contention::YieldCause::ExternalGrowth),
                    deadline_ms: None,
                },
            )
        })
        .collect()
}

fn reconcile_unlocked(config: &Config, exclude: &BTreeSet<String>) -> Result<Vec<ExecutionRecord>> {
    let guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    reconcile_guarded(config, exclude, guard, true)
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
    reconcile_guarded(config, &owned, guard, false)
}

pub(super) fn reconcile_guarded(
    config: &Config,
    exclude: &BTreeSet<String>,
    guard: NamespaceGuard,
    history: bool,
) -> Result<Vec<ExecutionRecord>> {
    crate::backup::ensure_runnable_state(&config.state_dir)?;
    let store = open_existing_node_store_guarded(config, guard)?;
    for mut record in store.active_executions()? {
        if exclude.contains(&record.assignment_id) {
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
                released = gpu_released_now(&record.resources, id.pid);
            }
        }
        for child in store.managed_children(&record.assignment_id)? {
            use crate::managed_children::ManagedChildPhase;
            if child.phase == ManagedChildPhase::Released {
                continue;
            }
            let gone = child.identity.as_ref().is_some_and(|id| {
                identity_absent(id) && gpu_released_now(&record.resources, id.pid)
            });
            if !gone && child.phase == ManagedChildPhase::NeedsReconciliation {
                released = false;
                continue;
            }
            store.transition_managed_child(&child.child_id,if gone {ManagedChildPhase::Released}else{ManagedChildPhase::NeedsReconciliation},None,None,
                if gone {"Read-only reconciliation verified original child absent and GPU context released"}else{"Supervisor handle lost; child identity/release unconfirmed; family reservation retained"})?;
            released &= gone;
        }
        let was_uncertain = record.phase == ExecutionPhase::NeedsReconciliation;
        record.phase = if released {
            ExecutionPhase::Released
        } else {
            ExecutionPhase::NeedsReconciliation
        };
        let detail = if released {
            "read-only reconciliation proved original rootless leader and all registered mediated children absent, with GPU contexts released"
        } else {
            "supervisor ownership unavailable; allocation retained until identity/backend release is proven"
        };
        // An uncertain record that stays so is not journaled again each heartbeat.
        if was_uncertain && !released && record.detail == detail {
            continue;
        }
        record.detail = detail.into();
        store.transition(&record)?;
    }
    // The node's own callers need only unreleased work; the operator command gets it all.
    if history {
        store.executions()
    } else {
        store.active_executions()
    }
}

/// One look at whether a GPU context of `pid` is gone; never a wait, since this runs on the
/// agent's loop every heartbeat (030, AGT-3). An unreadable device counts as not released.
fn gpu_released_now(resources: &crate::model::Resources, pid: u32) -> bool {
    crate::telemetry::GpuReleaseGuard::prepare(resources)
        .and_then(|guard| guard.is_released(pid))
        .unwrap_or(false)
}

/// The agent's record of the last checkpoint it published for an attempt.
pub(super) const CHECKPOINT_PUBLISHED: &str = "checkpoint-published.json";

pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis().min(u64::MAX as u128) as u64)
}

/// An instant `unix_ms` ago by the wall clock, or now when it cannot be represented.
fn instant_at(unix_ms: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_millis(now_unix_ms().saturating_sub(unix_ms)))
        .unwrap_or_else(Instant::now)
}

/// A re-adopted run's start and last published checkpoint, from its spec's launch time and
/// the agent's checkpoint record, so a restart does not reset the progress victim order
/// weighs (FR-016, 030 AGT-11). Missing ones start now.
pub(super) fn restored_times(
    launched_at_unix_ms: Option<u64>,
    output: &Path,
) -> (Instant, Option<(Instant, String)>) {
    let started = launched_at_unix_ms.map_or_else(Instant::now, instant_at);
    let checkpoint = std::fs::read(output.join(CHECKPOINT_PUBLISHED))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|record| {
            Some((
                instant_at(record["unix_ms"].as_u64()?),
                record["hash"].as_str()?.to_owned(),
            ))
        });
    (started, checkpoint)
}

#[cfg(test)]
mod restored_time_tests {
    use super::*;

    /// A re-adopted run keeps the start and the last published checkpoint it had, so victim
    /// order sees the progress a restart would lose (FR-016, 030 AGT-11).
    #[test]
    fn a_readopted_run_keeps_its_start_and_checkpoint_times() {
        let dir = tempfile::tempdir().unwrap();
        let now = now_unix_ms();
        std::fs::write(
            dir.path().join(CHECKPOINT_PUBLISHED),
            serde_json::to_vec(&serde_json::json!({"unix_ms": now - 30_000, "hash": "abc"}))
                .unwrap(),
        )
        .unwrap();
        let (started, checkpoint) = restored_times(Some(now - 60_000), dir.path());
        let age = |at: Instant| Instant::now().duration_since(at).as_secs();
        assert!((59..=61).contains(&age(started)), "{}", age(started));
        let (at, hash) = checkpoint.unwrap();
        assert!((29..=31).contains(&age(at)));
        assert_eq!(hash, "abc");
        // An older spec without a launch time, and no checkpoint, start the clock now.
        let empty = tempfile::tempdir().unwrap();
        let (started, checkpoint) = restored_times(None, empty.path());
        assert!(age(started) == 0 && checkpoint.is_none());
    }
}
