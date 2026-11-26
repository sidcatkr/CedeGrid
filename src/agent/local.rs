//! The local agent (003 research R1, contracts/local-agent.md): one agent per user and host that
//! keeps its own coordinator in the process. `run`, `ps`, `logs`, `stop` and `top` reach it over
//! a socket only the user can open, and its runs take the same launch path as cluster work, so
//! they need no network, no certificates and no second service (FR-003).

use crate::coordinator::{Coordinator, wait_for_work};
use crate::execution_model::{AllocationClass, LaunchRequest};
use crate::protocol::{CoordinatorConfig, JobSpec, PoolSpec, Principal, Request, Response};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// The node ID a local agent takes when its configuration names none.
pub const LOCAL_NODE: &str = "local";

/// The pools its runs go to, one per allocation class.
pub fn local_pool(class: AllocationClass) -> &'static str {
    match class {
        AllocationClass::Guaranteed => "local-guaranteed",
        AllocationClass::Opportunistic => "local-opportunistic",
    }
}

/// The coordinator inside a local agent. Its state lives in the agent's state directory, and it
/// answers only this process: the node's requests and those the user's socket carries.
pub struct LocalCoordinator {
    node_id: String,
    coordinator: Mutex<Coordinator>,
    /// Each run's environment values by task ID. They reach the launch from here and are
    /// never written to the coordinator's state (FR-009).
    env: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    work: tokio::sync::watch::Receiver<u64>,
    waiting: AtomicUsize,
}

impl LocalCoordinator {
    /// Opens the coordinator in `<state_dir>-coordinator`, beside the agent's own state, which
    /// must stay a dedicated directory, and makes the local pools,
    /// each holding the node `node_id` alone with room for `max_workers` tasks.
    pub fn open(
        state_dir: &Path,
        storage_profile: crate::state::StorageProfile,
        node_id: &str,
        max_workers: u32,
    ) -> Result<Self> {
        // Defaults come from the configuration's own serde defaults. The listen address and the
        // TLS files are never used: nothing is served.
        let config: CoordinatorConfig = serde_json::from_value(serde_json::json!({
            "state_dir": coordinator_dir(state_dir)?,
            "storage_profile": storage_profile,
            "listen": "127.0.0.1:0",
            "tls": {"ca_cert": "", "certificate": "", "private_key": ""},
            "clients": {},
        }))
        .context("the local coordinator's configuration")?;
        super::private_dir(&config.state_dir)?;
        let mut coordinator = Coordinator::open(config)?;
        for class in [AllocationClass::Guaranteed, AllocationClass::Opportunistic] {
            let pool = PoolSpec {
                pool_id: local_pool(class).to_owned(),
                class,
                node_ids: vec![node_id.to_owned()],
                min_workers: 0,
                max_workers,
            };
            if let Response::Error { message, .. } =
                coordinator.handle(&Principal::Operator, Request::PutPool { pool })?
            {
                bail!("the local pool was refused: {message}");
            }
        }
        let work = coordinator.subscribe_work();
        Ok(Self {
            node_id: node_id.to_owned(),
            coordinator: Mutex::new(coordinator),
            env: Mutex::new(BTreeMap::new()),
            work,
            waiting: AtomicUsize::new(0),
        })
    }

    /// One request from this agent's node side. A wait for work is held here, as the server
    /// holds it for a remote node (006 SC-001).
    pub(super) async fn node_request(&self, request: &Request) -> Result<Response> {
        if let Request::AwaitWork { seen, wait_ms } = request {
            wait_for_work(
                self.work.clone(),
                *seen,
                Duration::from_millis(*wait_ms),
                &self.waiting,
            )
            .await;
        }
        self.handle(
            &Principal::Node {
                node_id: self.node_id.clone(),
            },
            request.clone(),
        )
    }

    /// One request on the user's behalf, as an operator would send it.
    pub fn operator_request(&self, request: Request) -> Result<Response> {
        self.handle(&Principal::Operator, request)
    }

    /// A run's environment values for its launch, taken once.
    pub(super) fn take_env(&self, task_id: &str) -> Option<BTreeMap<String, String>> {
        self.env
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id)
    }

    /// Submits one run: its task goes to the local pool of its class, with the environment's
    /// names only; the values wait here for the launch.
    pub fn submit_run(
        &self,
        mut task: LaunchRequest,
        env: BTreeMap<String, String>,
    ) -> Result<String> {
        let run_id = format!("run-{}", uuid::Uuid::new_v4().simple());
        task.task_id = run_id.clone();
        task.assignment_id = String::new();
        task.env = env
            .keys()
            .map(|name| (name.clone(), String::new()))
            .collect();
        let job = JobSpec {
            job_id: run_id.clone(),
            pool_id: local_pool(task.class).to_owned(),
            priority: 0,
            tasks: vec![task],
        };
        self.env
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.clone(), env);
        match self.operator_request(Request::Submit { job }) {
            Ok(Response::Error { message }) => {
                self.take_env(&run_id);
                bail!("the run was refused: {message}")
            }
            Ok(_) => Ok(run_id),
            Err(error) => {
                self.take_env(&run_id);
                Err(error)
            }
        }
    }

    fn handle(&self, principal: &Principal, request: Request) -> Result<Response> {
        self.coordinator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handle(principal, request)
    }
}

/// Where a local agent's coordinator keeps its state: beside the agent's own state directory.
pub fn coordinator_dir(state_dir: &Path) -> Result<PathBuf> {
    let name = state_dir
        .file_name()
        .context("the state directory has no name")?
        .to_string_lossy()
        .into_owned();
    Ok(state_dir.with_file_name(format!("{name}-coordinator")))
}

/// Runs this user's local agent: its own coordinator, the user's socket and the usual agent
/// loop, with the whole host as its capacity, which the policy then shares out (003 R1).
#[cfg(unix)]
pub async fn run_local(
    config: &crate::config::Config,
    config_path: Option<&Path>,
    executable: &Path,
) -> Result<()> {
    let host = crate::telemetry::Collector::for_config(config)?.sample()?;
    let max_workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 256);
    let agent = super::AgentConfig {
        coordinator_url: "local".to_owned(),
        tls: crate::protocol::TlsIdentity {
            ca_cert: PathBuf::new(),
            certificate: PathBuf::new(),
            private_key: PathBuf::new(),
        },
        cpu_affinity: None,
        max_transfer_bytes_per_second: 10 * 1024 * 1024,
        capacity: crate::model::Resources {
            cpu_millicores: host.cpu_capacity_millicores,
            ram_mib: host.total_ram_mib,
            gpu_memory_mib: host
                .gpus
                .iter()
                .map(|gpu| (gpu.uuid.clone(), gpu.total_memory_mib))
                .collect(),
        },
        max_workers,
        max_spool_bytes: 2 * 1024 * 1024 * 1024,
        max_runtime_seconds: 0,
    };
    // The socket's directory first: where the state shares it, it must be made private, not
    // left at the default mode by the state directory's creation.
    let socket = socket_path()?;
    private_socket_dir(
        socket
            .parent()
            .context("the socket path has no directory")?,
    )?;
    super::private_dir(&config.state_dir)?;
    let local = std::sync::Arc::new(LocalCoordinator::open(
        &config.state_dir,
        config.storage_profile,
        &config.node_id,
        u32::try_from(max_workers)?,
    )?);
    // A socket left by an agent that ended answers no connection; a live one means another
    // agent of this user already serves, and the state lock below would refuse this one.
    if socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_err() {
        std::fs::remove_file(&socket).with_context(|| {
            format!("the stale socket {} could not be removed", socket.display())
        })?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .with_context(|| format!("{} could not be bound", socket.display()))?;
    let server = tokio::spawn(serve(listener, std::sync::Arc::clone(&local)));
    let result = super::run(config, config_path, &agent, executable, Some(local)).await;
    server.abort();
    // The socket is ours; an error removing it leaves only a stale file the next agent clears.
    let _ = std::fs::remove_file(&socket);
    result
}

/// The configuration a local agent runs with when the user has none: every default, with its
/// state under `~/.local/state/cedegrid/state`. A run needs no setup first (003 US1).
pub fn default_config() -> Result<crate::config::Config> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let mut config = crate::config::Config {
        state_dir: home
            .join(".local")
            .join("state")
            .join("cedegrid")
            .join("state"),
        ..crate::config::Config::default()
    };
    // A local agent exists to run the user's own runs, which keep their reservations unless a
    // run asks to be opportunistic.
    config.execution.enabled = true;
    config.node_mode = crate::config::NodeMode::Guaranteed;
    config.validate()?;
    Ok(config)
}

/// The user's socket: `$XDG_RUNTIME_DIR/cedegrid/agent.sock`, else
/// `~/.local/state/cedegrid/agent.sock` (contracts/local-agent.md).
pub fn socket_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".local")
            .join("state"),
    };
    Ok(base.join("cedegrid").join("agent.sock"))
}

/// Makes the socket's directory, mode 0700 and owned by this user, or refuses: the directory
/// keeps other users out, beside the peer check on each connection.
#[cfg(unix)]
pub fn private_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("{} could not be made", dir.display()))?;
    }
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("{} could not be read", dir.display()))?;
    ensure!(
        meta.file_type().is_dir(),
        "{} is not a plain directory",
        dir.display()
    );
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    ensure!(
        meta.uid() == me,
        "{} is not owned by this user",
        dir.display()
    );
    ensure!(
        meta.mode() & 0o077 == 0,
        "{} is open to other users (mode {:o})",
        dir.display(),
        meta.mode() & 0o777
    );
    Ok(())
}

/// One request on the user's socket: one JSON object per line (contracts/local-agent.md).
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalRequest {
    Run {
        task: Box<LaunchRequest>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Ps,
    Stop {
        run_id: String,
    },
}

/// The most one request line may hold.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Serves the user's socket until the listener fails. Each connection carries one request and
/// gets one reply line; a peer of another user is refused before anything is read.
#[cfg(unix)]
pub async fn serve(
    listener: tokio::net::UnixListener,
    local: std::sync::Arc<LocalCoordinator>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let local = std::sync::Arc::clone(&local);
        tokio::spawn(async move {
            // A connection that fails affects only its own caller.
            let _ = serve_connection(stream, &local).await;
        });
    }
}

#[cfg(unix)]
async fn serve_connection(stream: tokio::net::UnixStream, local: &LocalCoordinator) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    let peer = stream.peer_cred()?;
    let (read, mut write) = stream.into_split();
    let reply = if peer.uid() != me {
        refusal(
            "ERR_CEDEGRID_LOCAL_PEER",
            "this agent serves only its own user".to_owned(),
        )
    } else {
        let mut line = Vec::new();
        BufReader::new(read.take(MAX_REQUEST_BYTES))
            .read_until(b'\n', &mut line)
            .await?;
        match serde_json::from_slice::<LocalRequest>(&line) {
            Ok(request) => answer(local, request),
            Err(error) => refusal(
                "ERR_CEDEGRID_LOCAL_REQUEST",
                format!("the request could not be read: {error}"),
            ),
        }
    };
    let mut text = serde_json::to_vec(&reply)?;
    text.push(b'\n');
    write.write_all(&text).await?;
    Ok(())
}

fn refusal(code: &str, message: String) -> serde_json::Value {
    serde_json::json!({"error": {"code": code, "message": message}})
}

/// The reply to one request, as JSON.
pub fn answer(local: &LocalCoordinator, request: LocalRequest) -> serde_json::Value {
    match request {
        LocalRequest::Run { task, env } => match local.submit_run(*task, env) {
            Ok(run_id) => serde_json::json!({ "run_id": run_id }),
            Err(error) => refusal("ERR_CEDEGRID_RUN_REFUSED", format!("{error:#}")),
        },
        LocalRequest::Ps => match local.operator_request(Request::Status { job_id: None }) {
            Ok(Response::Status { tasks, .. }) => serde_json::json!({ "runs": tasks }),
            Ok(other) => refusal("ERR_CEDEGRID_LOCAL", format!("unexpected reply: {other:?}")),
            Err(error) => refusal("ERR_CEDEGRID_LOCAL", format!("{error:#}")),
        },
        LocalRequest::Stop { run_id } => match local.operator_request(Request::Cancel {
            job_id: run_id.clone(),
        }) {
            Ok(Response::Error { message }) => refusal("ERR_CEDEGRID_STOP_REFUSED", message),
            Ok(_) => serde_json::json!({ "run_id": run_id, "state": "stopping" }),
            Err(error) => refusal("ERR_CEDEGRID_LOCAL", format!("{error:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Response;

    /// The local coordinator answers in process: the pools exist, an operator's status request
    /// is served, and a node request is answered as the node.
    #[test]
    fn the_local_coordinator_answers_without_a_network() {
        let dir = crate::namespace::TestStateDir(
            tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap(),
        );
        let local = LocalCoordinator::open(
            dir.path(),
            crate::state::StorageProfile::default(),
            LOCAL_NODE,
            4,
        )
        .unwrap();
        match local
            .operator_request(Request::Status { job_id: None })
            .unwrap()
        {
            Response::Status { pools, .. } => {
                let ids: Vec<&str> = pools.iter().map(|pool| pool.pool_id.as_str()).collect();
                assert!(ids.contains(&"local-guaranteed") && ids.contains(&"local-opportunistic"));
            }
            other => panic!("{other:?}"),
        }
        // Reopening keeps the same pools.
        drop(local);
        LocalCoordinator::open(
            dir.path(),
            crate::state::StorageProfile::default(),
            LOCAL_NODE,
            4,
        )
        .unwrap();
    }
}
