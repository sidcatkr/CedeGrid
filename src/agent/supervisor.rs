//! The per-assignment supervisor process: its control pipe to the agent and its
//! run of one assignment.
use super::*;

struct PipeControl {
    rx: mpsc::Receiver<Incoming>,
    /// Turns readable when a line arrives on `rx`.
    #[cfg(unix)]
    wake: supervision::Wake,
    /// Writes events to an authenticated control-socket controller as well.
    #[cfg(unix)]
    sink: Option<supervision::control_socket::Sink>,
    stdin_open: bool,
    controller: bool,
    assignment: Assignment,
    config: Config,
    /// The latest agent GPU observation and when it arrived.
    observation: Option<(crate::model::Snapshot, Instant)>,
    authorized_at: Option<Instant>,
    remaining_ms: Option<u64>,
    agent_lost: bool,
}

impl SupervisorControl for PipeControl {
    fn prepared(&mut self, record: &ExecutionRecord) -> Result<()> {
        let deadline =
            Instant::now() + Duration::from_millis(self.config.execution.prepare_timeout_ms);
        let gpu = !self.assignment.request.resources.gpu_memory_mib.is_empty();
        if gpu {
            // Check against an observation the agent sent after this gate was
            // prepared; never one from before it.
            let snapshot = loop {
                match self.recv_until(deadline)? {
                    Control::Observation { snapshot } => break *snapshot,
                    Control::Drain { .. } => {
                        bail!("drain requested before authorization; gate stays blocked")
                    }
                    _ => {}
                }
            };
            self.observation = Some((snapshot, Instant::now()));
            self.validate_gpu(
                &self.assignment.request.resources,
                self.assignment.request.class,
            )?;
        }
        crate::supervision::launch_trace("prepared", &record.assignment_id);
        emit(&Event::Prepared {
            record: record.clone(),
        })?;
        loop {
            match self.recv_until(deadline)? {
                Control::Observation { snapshot } => {
                    self.observation = Some((*snapshot, Instant::now()));
                }
                // Nothing runs before authorization, so there is nothing to adjust.
                Control::Adjust { .. }
                | Control::Query
                | Control::Allowance { .. }
                | Control::Oom { .. }
                | Control::SaveProgress => {}
                Control::Authorize {
                    lease,
                    expires_monotonic_ms,
                } => {
                    let remaining_ms = expires_monotonic_ms.saturating_sub(local_clock_ms()?);
                    validate_lease(&lease, &self.assignment)?;
                    ensure!(
                        !lease.drain && remaining_ms > 0 && remaining_ms <= lease.valid_for_ms,
                        "authorization expired or draining"
                    );
                    // Admission may have changed while awaiting remote authorization.
                    // A newly visible external context closes the non-sharing gate.
                    if gpu {
                        self.validate_gpu(
                            &self.assignment.request.resources,
                            self.assignment.request.class,
                        )?;
                    }
                    self.remaining_ms = Some(remaining_ms);
                    self.authorized_at = Some(Instant::now());
                    crate::supervision::launch_trace(
                        "authorized",
                        &self.assignment.request.assignment_id,
                    );
                    return Ok(());
                }
                Control::Drain { .. } | Control::Renewal { .. } => {
                    bail!("execution authorization absent; gate stays blocked")
                }
            }
        }
    }
    #[cfg(unix)]
    fn wake(&self) -> Option<&supervision::Wake> {
        Some(&self.wake)
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
        // Queued commands include the newest agent observation.
        let commands = self.pending_commands()?;
        if !request.resources.gpu_memory_mib.is_empty() {
            self.validate_gpu(&request.resources, request.class)?;
        }
        Ok(commands)
    }
    fn poll(&mut self) -> Result<Vec<SupervisorCommand>> {
        self.pending_commands()
    }
    fn report(&mut self, report: supervision::SupervisorReport) -> Result<()> {
        let event = match report {
            supervision::SupervisorReport::Adjusted {
                sequence,
                applied,
                unavailable,
                detail,
                hint_ready,
                observes_drain,
                applied_targets,
                step_codes,
            } => Event::Adjusted {
                sequence,
                applied,
                unavailable,
                detail,
                hint_ready,
                observes_drain,
                applied_targets,
                step_codes,
            },
            supervision::SupervisorReport::State {
                applied,
                targets,
                lease_sequence,
                sequence,
                hint_ready,
                observes_drain,
                causes,
                outside_family,
                outside_reason,
                family,
            } => Event::State {
                applied,
                targets,
                lease_sequence,
                sequence,
                authority: if self.agent_lost { "lost" } else { "connected" }.into(),
                hint_ready,
                observes_drain,
                causes,
                outside_family,
                outside_reason,
                family,
            },
            supervision::SupervisorReport::OutsideFamily { count, reason } => {
                Event::OutsideFamily { count, reason }
            }
            supervision::SupervisorReport::Family { family } => Event::Family { family },
            supervision::SupervisorReport::Readiness {
                hint_ready,
                observes_drain,
            } => Event::Readiness {
                hint_ready,
                observes_drain,
            },
            supervision::SupervisorReport::Oom {
                preferred,
                changes,
                detail,
            } => Event::Oom {
                preferred,
                changes,
                detail,
            },
        };
        self.send_event(&event)
    }
}

pub(super) fn new_control_token() -> Result<Option<String>> {
    #[cfg(unix)]
    {
        Ok(Some(supervision::control_socket::token()?))
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

/// A control line this supervisor does not understand, such as one from a newer agent.
fn unknown_control(detail: &str) {
    write_diagnostic(
        std::io::stderr(),
        format_args!(
            "{}",
            serde_json::json!({"event":"unknown_control","detail":detail})
        ),
    );
}

impl PipeControl {
    /// Events go to the inherited pipe and to an attached controller; either may
    /// be gone after an agent restart.
    fn send_event(&self, event: &Event) -> Result<()> {
        let line = serde_json::to_string(event)?;
        let piped = emit(event).is_ok();
        #[cfg(unix)]
        let attached = self
            .sink
            .as_ref()
            .is_some_and(|sink| sink.write_line(&line));
        #[cfg(not(unix))]
        let attached = {
            let _ = line;
            false
        };
        ensure!(piped || attached, "no agent receives supervisor events");
        Ok(())
    }
    fn recv_until(&mut self, deadline: Instant) -> Result<Control> {
        loop {
            match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Incoming::Control(control)) => return Ok(control),
                Ok(Incoming::Unknown(detail)) => unknown_control(&detail),
                Ok(Incoming::Attached | Incoming::Detached) => {}
                _ => bail!("agent lost or execution authorization absent; gate stays blocked"),
            }
        }
    }
    /// GPU launch recheck against the agent's observation, which must be fresh.
    fn validate_gpu(
        &self,
        resources: &Resources,
        class: crate::execution_model::AllocationClass,
    ) -> Result<()> {
        let fresh = Duration::from_millis(self.config.monitor.interval_ms.saturating_mul(3));
        let Some((snapshot, _)) = self
            .observation
            .as_ref()
            .filter(|(_, received)| received.elapsed() <= fresh)
        else {
            bail!("no fresh agent GPU observation for the launch recheck");
        };
        validate_gpu_launch_contract(&self.config, snapshot, resources, class)
    }
    fn pending_commands(&mut self) -> Result<Vec<SupervisorCommand>> {
        let mut commands = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Incoming::Control(Control::Renewal {
                    lease,
                    expires_monotonic_ms,
                })) => {
                    let remaining_ms = expires_monotonic_ms.saturating_sub(local_clock_ms()?);
                    // A drain needs no time left; a grant does (FR-079, R85).
                    if validate_lease(&lease, &self.assignment).is_ok()
                        && (lease.drain || (remaining_ms > 0 && remaining_ms <= lease.valid_for_ms))
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
                Ok(Incoming::Control(Control::Drain { .. })) => {
                    commands.push(SupervisorCommand::Drain)
                }
                Ok(Incoming::Control(Control::Observation { snapshot })) => {
                    self.observation = Some((*snapshot, Instant::now()));
                }
                Ok(Incoming::Control(Control::Adjust {
                    sequence,
                    resources,
                })) => commands.push(SupervisorCommand::Adjust {
                    sequence,
                    resources,
                }),
                Ok(Incoming::Control(Control::Query)) => commands.push(SupervisorCommand::Query),
                Ok(Incoming::Control(Control::Allowance { allowance })) => {
                    commands.push(SupervisorCommand::Allowance { allowance })
                }
                Ok(Incoming::Control(Control::Oom { preferred })) => {
                    commands.push(SupervisorCommand::Oom { preferred })
                }
                Ok(Incoming::Control(Control::SaveProgress)) => {
                    commands.push(SupervisorCommand::SaveProgress)
                }
                Ok(Incoming::Control(Control::Authorize { .. })) => {}
                Ok(Incoming::Unknown(detail)) => unknown_control(&detail),
                // Authority returns when a controller reattaches (research R2).
                Ok(Incoming::Attached) => {
                    self.controller = true;
                    self.agent_lost = false;
                }
                Ok(Incoming::Detached) => {
                    self.controller = false;
                    if !self.stdin_open && !self.agent_lost {
                        commands.push(SupervisorCommand::AgentLost);
                        self.agent_lost = true;
                    }
                }
                Ok(Incoming::Lost) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.stdin_open = false;
                    if !self.controller && !self.agent_lost {
                        commands.push(SupervisorCommand::AgentLost);
                        self.agent_lost = true;
                    }
                    if matches!(self.rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                        break;
                    }
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
    crate::supervision::install_stop_handlers()?;
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
    crate::supervision::launch_trace(
        "supervisor_spec_read",
        &spec.assignment.request.assignment_id,
    );
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
    let parse = |line: &str| match serde_json::from_str(line) {
        Ok(control) => Incoming::Control(control),
        Err(error) => Incoming::Unknown(error.to_string()),
    };
    #[cfg(unix)]
    let wake = supervision::Wake::new()?;
    let stdin_tx = tx.clone();
    #[cfg(unix)]
    let stdin_wake = wake.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(std::io::stdin()).lines() {
            let Ok(line) = line else { break };
            if stdin_tx.send(parse(&line)).is_err() {
                return;
            }
            #[cfg(unix)]
            stdin_wake.notify();
        }
        let _ = stdin_tx.send(Incoming::Lost);
        #[cfg(unix)]
        stdin_wake.notify();
    });
    // A restarted agent reattaches through this socket (research R2).
    #[cfg(unix)]
    let sink = match &spec.control_token {
        Some(token) => {
            let listener = supervision::control_socket::Listener::bind(token.clone())?;
            write_json_new(
                &spec.output_dir.join("control-socket.json"),
                &serde_json::json!({"path": listener.path()}),
            )?;
            let socket_tx = std::sync::Mutex::new(tx.clone());
            let socket_wake = wake.clone();
            Some(listener.serve(move |line| {
                let message = match line {
                    supervision::control_socket::Line::Attached => Incoming::Attached,
                    supervision::control_socket::Line::Detached => Incoming::Detached,
                    supervision::control_socket::Line::Text(line) => parse(&line),
                };
                let _ = socket_tx.lock().map(|tx| tx.send(message));
                socket_wake.notify();
            }))
        }
        None => None,
    };
    drop(tx);
    let mut backend = execution_backend(&spec.config, &spec.assignment.request)?;
    let journal = AssignedJournal {
        store: open_existing_node_store_guarded(&spec.config, namespace_guard.clone())?,
        generation: spec.assignment.generation,
    };
    let mut control = PipeControl {
        rx,
        #[cfg(unix)]
        wake,
        #[cfg(unix)]
        sink,
        stdin_open: true,
        controller: false,
        assignment: spec.assignment.clone(),
        config: spec.config.clone(),
        observation: None,
        authorized_at: None,
        remaining_ms: None,
        agent_lost: false,
    };
    let options = SupervisorOptions {
        nice: spec.config.launch_nice(&spec.assignment.request),
        drain_timeout_ms: spec.config.lifecycle.drain_timeout_ms,
        term_grace_ms: spec.config.lifecycle.term_grace_ms,
        lease_ms: spec.config.lifecycle.allocation_lease_ms,
        prepare_timeout_ms: spec.config.execution.prepare_timeout_ms,
        release_confirm_timeout_ms: spec.config.execution.release_confirm_timeout_ms,
        duty_period_ms: spec.config.monitor.interval_ms,
        hint_deadline_ms: spec.config.contention.hint_deadline_ms(),
        prefer_own_oom_victim: spec.config.ram.prefer_own_oom_victim,
        ram_noise_tolerance_mib: spec.config.ram_cap_check().0,
        ram_calm_window_ms: spec.config.ram_cap_check().1,
        nofile_soft: spec.nofile_soft,
        cpu_placement: spec.cpu_placement,
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
    // The outcome files are authoritative; an event nobody receives is not an error.
    let result = match result {
        Ok(outcome) => {
            write_json_new(&spec.output_dir.join("execution-outcome.json"), &outcome)?;
            let _ = control.send_event(&Event::Finished { outcome });
            Ok(())
        }
        Err(error) => {
            let detail = format!("{error:#}");
            let _ = write_json_new(
                &spec.output_dir.join("execution-error.json"),
                &serde_json::json!({"detail":detail}),
            );
            let _ = control.send_event(&Event::Failed { detail });
            Err(error)
        }
    };
    write_supervisor_usage(
        &spec.output_dir,
        &supervisor_identity,
        lifecycle_started,
        journal.store.lock_waits(),
    )?;
    result
}

fn write_supervisor_usage(
    output: &Path,
    identity: &ProcessIdentity,
    started: Instant,
    (journal_waits, journal_longest_wait): (u64, std::time::Duration),
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
                "observed_monotonic_ms":local_clock_ms()?,
                "journal_waits":journal_waits,"journal_longest_wait_ms":journal_longest_wait.as_millis() as u64,
                "cutoff":"after workload outcome/event publication, before this accounting file and process exit"
            }),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (
            output,
            identity,
            started,
            journal_waits,
            journal_longest_wait,
        );
        bail!("supervisor self accounting unsupported")
    }
}
