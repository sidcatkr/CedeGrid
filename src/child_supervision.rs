// Included inside supervision::unix: direct children share its exclusive reaper.
use crate::managed_children::{ManagedChildPhase as ChildPhase, ManagedChildRecord};
use std::collections::BTreeMap;
use std::os::unix::net::{UnixListener, UnixStream};

struct ChildConnection {
    stream: UnixStream,
    bytes: Vec<u8>,
    deadline: Instant,
    peer: Option<u32>,
}
struct MediatedChild {
    id: String,
    request: LaunchRequest,
    generation: u64,
    setup: GateSetup,
    initialized: bool,
    owned: OwnedChild,
    drain: DrainDirectory,
    input: Option<std::process::ChildStdin>,
    output: Option<std::process::ChildStdout>,
    payload: Vec<u8>,
    sent: usize,
    identity_bytes: Vec<u8>,
    identity: Option<ProcessIdentity>,
    deadline: Instant,
    phase: ChildPhase,
    stop_at: Option<Instant>,
    term_sent: bool,
    kill_sent: bool,
    capture: Option<OutputCapture>,
    log_dir: Option<PathBuf>,
    exit: Option<(ExitStatus, Instant)>,
}
struct ChildServer {
    listener: UnixListener,
    path: PathBuf,
    directory: PathBuf,
    token: String,
    connections: Vec<ChildConnection>,
    children: BTreeMap<String, MediatedChild>,
    parent: LaunchRequest,
    generation: u64,
    setup: GateSetup,
}
impl ChildServer {
    fn new(parent: &LaunchRequest, generation: u64, setup: GateSetup) -> Result<Option<Self>> {
        if parent.managed_child_limit == 0 {
            return Ok(None);
        }
        ensure!(
            !parent.single_process && parent.no_escape && parent.managed_child_limit <= 8,
            "mediated children require explicit no-escape family contract and limit 1..=8"
        );
        let directory = std::env::temp_dir().join(format!(
            "rmc-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        ));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("s");
        let listener = match UnixListener::bind(&path) {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_dir(&directory);
                return Err(e.into());
            }
        };
        listener.set_nonblocking(true)?;
        Ok(Some(Self {
            listener,
            path,
            directory,
            token: uuid::Uuid::new_v4().to_string(),
            connections: vec![],
            children: BTreeMap::new(),
            parent: parent.clone(),
            generation,
            setup,
        }))
    }
    fn expose(&self, request: &mut LaunchRequest) {
        request.env.insert(
            "RESMGR_SUPERVISOR_SOCKET".into(),
            self.path.to_string_lossy().into(),
        );
        request
            .env
            .insert("RESMGR_SUPERVISOR_TOKEN".into(), self.token.clone());
    }
    fn all_released(&self) -> bool {
        self.children
            .values()
            .all(|c| c.phase == ChildPhase::Released)
    }
    fn request_stop(&mut self) {
        for child in self.children.values_mut() {
            child.stop_at.get_or_insert_with(Instant::now);
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn poll(
        &mut self,
        parent_identity: &ProcessIdentity,
        accepting: bool,
        journal: &dyn ExecutionJournal,
        backend: &mut dyn LaunchBackend,
        executable: &Path,
        options: &SupervisorOptions,
        gpu: &crate::telemetry::GpuReleaseGuard,
        authorize: &mut dyn FnMut(&LaunchRequest) -> Result<()>,
    ) -> Result<()> {
        // Bounded nonblocking input work; a client that sends no newline never stalls deadlines.
        for _ in 0..4 {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.connections.len() >= 16 {
                        continue;
                    }
                    stream.set_nonblocking(true)?;
                    let peer = socket_peer(&stream).ok();
                    self.connections.push(ChildConnection {
                        stream,
                        bytes: vec![],
                        deadline: Instant::now() + Duration::from_secs(1),
                        peer,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut remaining = Vec::new();
        for mut connection in std::mem::take(&mut self.connections) {
            if Instant::now() >= connection.deadline {
                continue;
            }
            let mut bytes = [0; 4096];
            let mut closed = false;
            loop {
                match connection.stream.read(&mut bytes) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => {
                        connection.bytes.extend_from_slice(&bytes[..n]);
                        if connection.bytes.len() > 65_536 || connection.bytes.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            if connection.bytes.len() > 65_536 {
                continue;
            }
            if let Some(end) = connection.bytes.iter().position(|b| *b == b'\n') {
                let response = (|| -> Result<serde_json::Value> {
                    ensure!(
                        end + 1 == connection.bytes.len(),
                        "trailing child protocol data"
                    );
                    let value: serde_json::Value =
                        serde_json::from_slice(&connection.bytes[..end])?;
                    ensure!(
                        value["version"] == 1 && value["token"] == self.token,
                        "child protocol authentication failed"
                    );
                    ensure!(connection.peer.is_some(), "cannot verify socket peer");
                    #[cfg(target_os = "linux")]
                    ensure!(
                        connection.peer == Some(parent_identity.pid),
                        "socket caller is not the verified allocation leader"
                    );
                    ensure!(
                        identity(
                            parent_identity.pid,
                            &parent_identity.assignment_id,
                            parent_identity.generation
                        )? == *parent_identity,
                        "allocation leader identity is no longer verified"
                    );
                    match value["op"].as_str() {
                        Some("spawn") => {
                            ensure!(accepting, "allocation is draining; child launch refused");
                            let request_id = value["request_id"]
                                .as_str()
                                .context("missing child request_id")?;
                            ensure!(
                                !request_id.is_empty() && request_id.len() <= 128,
                                "invalid child request_id"
                            );
                            let argv: Vec<String> = serde_json::from_value(value["argv"].clone())?;
                            ensure!(
                                !argv.is_empty() && !argv[0].is_empty(),
                                "child executable missing"
                            );
                            ensure!(
                                value["single_process"] == true && value["no_escape"] == true,
                                "child requires explicit single-process/no-escape contract"
                            );
                            let mut request = self.parent.clone();
                            request.argv = argv;
                            request.single_process = true;
                            request.managed_child_limit = 0;
                            request.cwd = value["cwd"]
                                .as_str()
                                .map(PathBuf::from)
                                .unwrap_or_else(|| self.parent.cwd.clone());
                            ensure!(
                                request.cwd.is_absolute() && request.cwd.is_dir(),
                                "child cwd must be an existing absolute directory"
                            );
                            request.env.retain(|key, _| {
                                !key.starts_with("RESMGR_") || key == "RESMGR_CPU_AFFINITY"
                            });
                            if let Some(env) = value.get("env") {
                                let env: BTreeMap<String, String> =
                                    serde_json::from_value(env.clone())?;
                                ensure!(
                                    env.keys().all(|k| !k.starts_with("RESMGR_")
                                        && k != "CUDA_VISIBLE_DEVICES"),
                                    "reserved managed-child environment key"
                                );
                                request.env.extend(env);
                            }
                            if let Some(record) =
                                journal.managed_child(&self.parent.assignment_id, request_id)?
                            {
                                ensure!(
                                    serde_json::to_value(&record.request)?
                                        == serde_json::to_value(&request)?,
                                    "child request ID reused with different command"
                                );
                                return Ok(child_response(&record));
                            }
                            ensure!(
                                self.children.len() < usize::from(self.parent.managed_child_limit),
                                "allocation managed child lifetime limit reached"
                            );
                            authorize(&request)?;
                            ensure!(
                                Instant::now() < connection.deadline,
                                "child request expired during launch verification"
                            );
                            let child_id = uuid::Uuid::new_v4().to_string();
                            let record = journal.reserve_managed_child(
                                &self.parent.assignment_id,
                                self.generation,
                                &child_id,
                                request_id,
                                &request,
                            )?;
                            let created =
                                self.create_child(&child_id, request, executable, options);
                            match created {
                                Ok(child) => {
                                    self.children.insert(child_id, child);
                                    Ok(child_response(&record))
                                }
                                Err(e) => {
                                    journal.transition_managed_child(
                                        &record.child_id,
                                        ChildPhase::Released,
                                        None,
                                        None,
                                        &format!("Gate was never spawned: {e:#}"),
                                    )?;
                                    Err(e)
                                }
                            }
                        }
                        Some("status") | Some("stop") => {
                            let id = value["child_id"].as_str().context("missing child_id")?;
                            if value["op"] == "stop" {
                                let child =
                                    self.children.get_mut(id).context("unknown managed child")?;
                                child.stop_at.get_or_insert_with(Instant::now);
                            }
                            let record = journal
                                .managed_children(&self.parent.assignment_id)?
                                .into_iter()
                                .find(|c| c.child_id == id)
                                .context("unknown managed child")?;
                            Ok(child_response(&record))
                        }
                        _ => bail!("unsupported child operation"),
                    }
                })()
                .unwrap_or_else(|e| serde_json::json!({"ok":false,"error":format!("{e:#}")}));
                let mut encoded = serde_json::to_vec(&response)?;
                encoded.push(b'\n');
                // Replies are small. If a peer cannot receive them, request_id makes retry safe.
                let _ = connection.stream.write_all(&encoded);
            } else if !closed {
                remaining.push(connection);
            }
        }
        self.connections = remaining;
        for child in self.children.values_mut() {
            if child.phase == ChildPhase::Released {
                continue;
            }
            let tick = child.tick(accepting, journal, backend, options, gpu, authorize);
            if let Err(error) = tick {
                let cleanup = child
                    .owned
                    .kill_and_confirm(Duration::from_millis(options.release_confirm_timeout_ms));
                let phase = if cleanup.is_ok()
                    && gpu.is_released(child.owned.child.id()).unwrap_or(false)
                {
                    ChildPhase::Released
                } else {
                    ChildPhase::NeedsReconciliation
                };
                journal.transition_managed_child(
                    &child.id,
                    phase,
                    None,
                    None,
                    &format!("Child launch/lifecycle failed: {error:#}; {cleanup:?}"),
                )?;
                child.phase = phase;
                if child.phase != ChildPhase::Released {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    fn create_child(
        &self,
        id: &str,
        mut request: LaunchRequest,
        executable: &Path,
        options: &SupervisorOptions,
    ) -> Result<MediatedChild> {
        // Every fallible preparation before spawn is completed first. After spawn,
        // ownership is installed immediately; acquisition/verification happens in tick.
        let drain = DrainDirectory::new()?;
        let log_dir = if let Some(parent) = self.parent.env.get("RESMGR_OUTPUT_DIR") {
            let path = Path::new(parent).join(format!("child-{id}"));
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            request
                .env
                .insert("RESMGR_CAPTURE_OUTPUT".into(), "1".into());
            Some(path)
        } else {
            None
        };
        let gate = GateRequest {
            request: request.clone(),
            generation: self.generation,
            setup: self.setup.clone(),
            drain_file: drain.file(),
        };
        let mut payload = serde_json::to_vec(&gate)?;
        ensure!(payload.len() <= 65_536, "oversized child launch");
        payload.push(b'\n');
        let raw = Command::new(executable)
            .arg("__worker-gate")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if log_dir.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .spawn()?;
        let mut owned = OwnedChild::new(raw);
        let input = owned.child.stdin.take();
        let output = owned.child.stdout.take();
        Ok(MediatedChild {
            id: id.into(),
            request,
            generation: self.generation,
            setup: self.setup.clone(),
            initialized: false,
            owned,
            drain,
            input,
            output,
            payload,
            sent: 0,
            identity_bytes: vec![],
            identity: None,
            deadline: Instant::now() + Duration::from_millis(options.prepare_timeout_ms),
            phase: ChildPhase::Reserved,
            stop_at: None,
            term_sent: false,
            kill_sent: false,
            capture: None,
            log_dir,
            exit: None,
        })
    }
    fn cleanup(
        &mut self,
        journal: &dyn ExecutionJournal,
        options: &SupervisorOptions,
        gpu: &crate::telemetry::GpuReleaseGuard,
    ) -> Result<()> {
        let mut failures = vec![];
        let deadline = Instant::now() + Duration::from_millis(options.release_confirm_timeout_ms);
        for child in self
            .children
            .values_mut()
            .filter(|c| c.phase != ChildPhase::Released && !c.owned.reaped)
        {
            if let Err(error) = child.owned.signal(libc::SIGKILL) {
                failures.push(format!("{error:#}"));
            }
        }
        for child in self
            .children
            .values_mut()
            .filter(|c| c.phase != ChildPhase::Released)
        {
            let release = child
                .owned
                .kill_and_confirm(deadline.saturating_duration_since(Instant::now()))
                .and_then(|()| {
                    gpu.confirm(
                        child.owned.child.id(),
                        deadline.saturating_duration_since(Instant::now()),
                    )
                });
            let phase = if release.is_ok() {
                ChildPhase::Released
            } else {
                ChildPhase::NeedsReconciliation
            };
            journal.transition_managed_child(
                &child.id,
                phase,
                None,
                None,
                &format!("Supervisor cleanup: {release:?}"),
            )?;
            child.phase = phase;
            if let Err(e) = release {
                failures.push(format!("{e:#}"));
            }
        }
        ensure!(
            failures.is_empty(),
            "managed children remain uncertain: {failures:?}"
        );
        Ok(())
    }
}
impl Drop for ChildServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}
fn child_response(record: &ManagedChildRecord) -> serde_json::Value {
    let state = match record.phase {
        ChildPhase::Released => "released",
        ChildPhase::Draining => "draining",
        ChildPhase::NeedsReconciliation => "needs_reconciliation",
        ChildPhase::Reserved | ChildPhase::Prepared | ChildPhase::Authorized => "preparing",
        ChildPhase::Running => "running",
    };
    serde_json::json!({"ok":true,"child_id":record.child_id,"state":state,"exit_code":record.exit_code,"signal":record.signal})
}
fn socket_peer(stream: &UnixStream) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut size,
                )
            } == 0,
            "cannot read child socket credentials"
        );
        ensure!(
            credentials.uid == unsafe { libc::geteuid() } && credentials.pid > 0,
            "child socket peer not owned by runtime user"
        );
        Ok(credentials.pid as u32)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut uid = 0;
        let mut gid = 0;
        ensure!(
            unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0
                && uid == unsafe { libc::geteuid() },
            "child socket peer UID not verified"
        );
        Ok(0)
    }
}
impl MediatedChild {
    fn tick(
        &mut self,
        accepting: bool,
        journal: &dyn ExecutionJournal,
        backend: &mut dyn LaunchBackend,
        options: &SupervisorOptions,
        gpu: &crate::telemetry::GpuReleaseGuard,
        authorize: &mut dyn FnMut(&LaunchRequest) -> Result<()>,
    ) -> Result<()> {
        if !self.initialized {
            // Exclusive owner has not reaped; PID cannot be reused before pidfd acquisition.
            self.owned.acquire_handle()?;
            set_nonblocking(
                self.input
                    .as_ref()
                    .context("missing child gate input")?
                    .as_raw_fd(),
            )?;
            set_nonblocking(
                self.output
                    .as_ref()
                    .context("missing child gate output")?
                    .as_raw_fd(),
            )?;
            self.initialized = true;
        }
        if self.exit.is_none()
            && let Some(status) = self.owned.try_wait()?
        {
            self.exit = Some((status, Instant::now()));
        }
        if let Some((status, observed)) = self.exit {
            if !gpu.is_released(self.owned.child.id())? {
                ensure!(
                    observed.elapsed() < Duration::from_millis(options.release_confirm_timeout_ms),
                    "child GPU context release remains uncertain"
                );
                return Ok(());
            }
            if let Some(logs) = self.capture.take() {
                logs.finish()?;
            }
            journal.transition_managed_child(
                &self.id,
                ChildPhase::Released,
                status.code(),
                status.signal(),
                "Owned mediated child reaped and GPU absence confirmed",
            )?;
            self.phase = ChildPhase::Released;
            return Ok(());
        }
        if !accepting {
            self.stop_at.get_or_insert_with(Instant::now);
        }
        if self.phase == ChildPhase::Reserved {
            ensure!(
                accepting && self.stop_at.is_none() && Instant::now() < self.deadline,
                "child preparation canceled or expired before execution"
            );
            let input = self.input.as_mut().context("missing child gate input")?;
            while self.sent < self.payload.len() {
                match input.write(&self.payload[self.sent..]) {
                    Ok(0) => bail!("child gate input closed"),
                    Ok(n) => self.sent += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
            let output = self.output.as_mut().context("missing child gate output")?;
            let mut bytes = [0; 4096];
            loop {
                match output.read(&mut bytes) {
                    Ok(0) => bail!("child gate closed before identity"),
                    Ok(n) => {
                        self.identity_bytes.extend_from_slice(&bytes[..n]);
                        ensure!(
                            self.identity_bytes.len() <= 65_536,
                            "oversized child identity"
                        );
                        if self.identity_bytes.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
            let claimed: ProcessIdentity = serde_json::from_slice(&self.identity_bytes)?;
            let actual = identity(
                self.owned.child.id(),
                &self.request.assignment_id,
                self.generation,
            )?;
            ensure!(claimed == actual, "mediated child identity mismatch");
            let mut evidence = backend.verify(&actual)?;
            evidence.push(ControlEvidence{control:"process_handle".into(),available:Some(true),permitted:Some(true),configured:true,applied:true,fallback:self.owned.pidfd.is_none(),scope:format!("mediated_child:{}",actual.pid),requested:Some("stable owned handle".into()),effective:Some(self.owned.handle_detail.clone()),detail:"Supervisor-created direct child with exclusive reaping; no arbitrary descendant adoption".into()});
            if let Some(cpus) = self.request.env.get("RESMGR_CPU_AFFINITY") {
                #[cfg(target_os = "linux")]
                {
                    let mut expected: Vec<u32> = serde_json::from_str(cpus)?;
                    expected.sort_unstable();
                    ensure!(
                        read_cpu_affinity(actual.pid)? == expected,
                        "mediated child affinity mismatch"
                    );
                    evidence.push(ControlEvidence {
                        control: "cpu.affinity".into(),
                        available: Some(true),
                        permitted: Some(true),
                        configured: true,
                        applied: true,
                        fallback: false,
                        scope: format!("mediated_child:{}", actual.pid),
                        requested: Some(cpus.clone()),
                        effective: Some(cpus.clone()),
                        detail: "Verified exact allowed CPU IDs".into(),
                    });
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = cpus;
                    bail!("child CPU affinity unsupported");
                }
            }
            if let Some(expected) = self.setup.nice {
                let actual_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, actual.pid) };
                ensure!(actual_nice == expected, "child nice mismatch");
                evidence.push(ControlEvidence {
                    control: "cpu.nice".into(),
                    available: Some(true),
                    permitted: Some(true),
                    configured: true,
                    applied: true,
                    fallback: false,
                    scope: format!("mediated_child:{}", actual.pid),
                    requested: Some(expected.to_string()),
                    effective: Some(actual_nice.to_string()),
                    detail: "Verified per-process relative priority".into(),
                });
            }
            for required in &self.request.required_controls {
                ensure!(
                    evidence.iter().any(|e| &e.control == required
                        && e.applied
                        && !e.fallback
                        && e.available == Some(true)
                        && e.permitted == Some(true)),
                    "child required control {required} was not applied"
                );
            }
            if let Some(directory) = &self.log_dir {
                let stdout = self
                    .output
                    .take()
                    .context("missing mediated child stdout")?;
                clear_nonblocking(stdout.as_raw_fd())?;
                self.capture = Some(OutputCapture::start(
                    stdout,
                    self.owned
                        .child
                        .stderr
                        .take()
                        .context("missing mediated child stderr")?,
                    directory,
                )?);
            }
            journal.prepare_managed_child(&self.id, &actual, &evidence)?;
            self.identity = Some(actual);
            self.phase = ChildPhase::Prepared;
        }
        if self.phase == ChildPhase::Prepared {
            ensure!(
                accepting && self.stop_at.is_none() && Instant::now() < self.deadline,
                "child authorization expired"
            );
            journal.transition_managed_child(
                &self.id,
                ChildPhase::Authorized,
                None,
                None,
                "Verified child authorization durable before EXEC",
            )?;
            self.phase = ChildPhase::Authorized;
        }
        if self.phase == ChildPhase::Authorized {
            ensure!(
                accepting && self.stop_at.is_none() && Instant::now() < self.deadline,
                "child authorization revoked before EXEC"
            );
            authorize(&self.request)?;
            ensure!(
                self.stop_at.is_none() && Instant::now() < self.deadline,
                "child preparation deadline expired during launch verification"
            );
            match self
                .input
                .as_mut()
                .context("missing child EXEC gate")?
                .write(b"EXEC\n")
            {
                Ok(5) => {}
                Ok(_) => bail!("incomplete child EXEC authorization"),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            self.input.take();
            self.output.take();
            journal.transition_managed_child(
                &self.id,
                ChildPhase::Running,
                None,
                None,
                "EXEC sent after durable child authorization",
            )?;
            self.phase = ChildPhase::Running;
        }
        if let Some(start) = self.stop_at {
            if self.phase == ChildPhase::Running {
                self.drain.notify()?;
                journal.transition_managed_child(
                    &self.id,
                    ChildPhase::Draining,
                    None,
                    None,
                    "Parent allocation or explicit child stop requested drain",
                )?;
                self.phase = ChildPhase::Draining;
            }
            if start.elapsed() >= Duration::from_millis(options.drain_timeout_ms) && !self.term_sent
            {
                self.owned.signal(libc::SIGTERM)?;
                self.term_sent = true;
            }
            if start.elapsed()
                >= Duration::from_millis(
                    options
                        .drain_timeout_ms
                        .saturating_add(options.term_grace_ms),
                )
                && !self.kill_sent
            {
                self.owned.signal(libc::SIGKILL)?;
                self.kill_sent = true;
            }
            ensure!(
                start.elapsed()
                    < Duration::from_millis(
                        options
                            .drain_timeout_ms
                            .saturating_add(options.term_grace_ms)
                            .saturating_add(options.release_confirm_timeout_ms)
                    ),
                "child release remains uncertain after termination"
            );
        }
        Ok(())
    }
}
