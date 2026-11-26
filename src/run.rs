//! `cedegrid run` (003 US1, contracts/cli.md): one command starts work under CedeGrid through
//! this user's local agent, starting the agent first when none runs.

use crate::execution_model::{AllocationClass, LaunchRequest};
use crate::model::Resources;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The run's flags; their names follow the task schema (`crate::task_schema`).
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// CPU to reserve, in thousandths of a core.
    #[arg(long, default_value_t = 1000)]
    pub cpu_millicores: u64,
    /// RAM to reserve, in MiB.
    #[arg(long, default_value_t = 512)]
    pub ram_mib: u64,
    /// GPU memory to reserve on a GPU, as GPU=MiB; repeat for more GPUs.
    #[arg(long = "gpu-vram-mib", value_name = "GPU=MIB")]
    pub gpu_vram_mib: Vec<String>,
    /// An environment value for the run, as NAME=VALUE, or NAME to pass this shell's value.
    #[arg(long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
    /// Whether the run may start again from the beginning after a loss.
    #[arg(long)]
    pub replay_safe: bool,
    /// guaranteed or opportunistic.
    #[arg(long, default_value = "guaranteed")]
    pub allocation_class: String,
    /// A control the run needs; it never starts without it.
    #[arg(long = "required-control")]
    pub required_controls: Vec<String>,
    /// The run is one process with no children.
    #[arg(long)]
    pub single_process: bool,
    /// How many children the supervisor may start for the run.
    #[arg(long, default_value_t = 0)]
    pub managed_child_limit: u16,
    /// The most attempts the run may take.
    #[arg(long)]
    pub max_attempts: Option<u32>,
    /// Return at once instead of waiting for the run to end.
    #[arg(short = 'd', long)]
    pub detach: bool,
    /// Print the result as JSON.
    #[arg(long)]
    pub json: bool,
    /// The command and its arguments.
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    pub command: Vec<String>,
}

/// Environment variables a run takes from this shell without being named, so it runs as the
/// command would here (FR-016).
const PASSED: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "PYTHONPATH",
    "CUDA_VISIBLE_DEVICES",
];

/// The user's configuration for the local agent: `$XDG_CONFIG_HOME/cedegrid/cedegrid.toml`, else
/// `~/.config/cedegrid/cedegrid.toml`. It may not exist; the agent then takes every default.
pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("cedegrid").join("cedegrid.toml"))
}

/// The task this run submits.
pub fn task(args: &RunArgs, cwd: &Path) -> Result<(LaunchRequest, BTreeMap<String, String>)> {
    let class = match args.allocation_class.as_str() {
        "guaranteed" => AllocationClass::Guaranteed,
        "opportunistic" => AllocationClass::Opportunistic,
        other => bail!("--allocation-class must be guaranteed or opportunistic, not {other}"),
    };
    let mut gpu_memory_mib = BTreeMap::new();
    for item in &args.gpu_vram_mib {
        let (gpu, mib) = item
            .split_once('=')
            .with_context(|| format!("--gpu-vram-mib takes GPU=MIB, not {item}"))?;
        gpu_memory_mib.insert(gpu.to_owned(), mib.parse().context("--gpu-vram-mib MiB")?);
    }
    let mut env = BTreeMap::new();
    for name in PASSED {
        if let Some(value) = std::env::var_os(name) {
            env.insert((*name).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    for item in &args.env {
        match item.split_once('=') {
            Some((name, value)) => {
                env.insert(name.to_owned(), value.to_owned());
            }
            None => {
                let value = std::env::var(item)
                    .with_context(|| format!("--env {item}: this shell has no such value"))?;
                env.insert(item.clone(), value);
            }
        }
    }
    ensure!(
        env.keys()
            .all(|name| !name.is_empty() && !name.contains('=')),
        "an environment name is empty or holds '='"
    );
    let task: LaunchRequest = serde_json::from_value(serde_json::json!({
        "task_id": "",
        "assignment_id": "",
        "argv": args.command,
        "cwd": cwd,
        "env": {},
        "resources": Resources {
            cpu_millicores: args.cpu_millicores,
            ram_mib: args.ram_mib,
            gpu_memory_mib,
        },
        "replay_safe": args.replay_safe,
        "class": class,
        // A run's processes stay in its tree; work started outside it is not tracked (FR-054).
        "no_escape": true,
        "single_process": args.single_process,
        "managed_child_limit": args.managed_child_limit,
        "max_attempts": args.max_attempts,
        "required_controls": args.required_controls,
        "allow_fallback": true,
    }))?;
    Ok((task, env))
}

/// Sends one request to the local agent and reads its reply.
#[cfg(unix)]
pub fn ask(socket: &Path, request: &serde_json::Value) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = Vec::new();
    BufReader::new(stream.take(16 * 1024 * 1024)).read_until(b'\n', &mut reply)?;
    let reply: serde_json::Value = serde_json::from_slice(&reply).context("the agent's reply")?;
    if let Some(error) = reply.get("error") {
        bail!(
            "{}",
            error["message"]
                .as_str()
                .unwrap_or("the agent refused the request")
        );
    }
    Ok(reply)
}

/// Connects to this user's local agent, starting one in its own session when none answers, and
/// waits up to 3 s for its socket (contracts/local-agent.md).
#[cfg(unix)]
pub fn ensure_agent(executable: &Path) -> Result<PathBuf> {
    use std::os::unix::process::CommandExt;
    let socket = crate::agent::local::socket_path()?;
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        return Ok(socket);
    }
    let config = config_path()?;
    let log_dir = socket
        .parent()
        .context("the socket path has no directory")?;
    crate::agent::local::private_socket_dir(log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("agent.log"))?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--config")
        .arg(&config)
        .args(["agent", "--local"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and the closure touches nothing else.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("the local agent could not start")?;
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return Ok(socket);
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "the local agent ended at start ({status}); see {}",
                log_dir.join("agent.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "the local agent did not answer within 3 s; see {}",
        log_dir.join("agent.log").display()
    )
}

/// `cedegrid run`: the exit code to leave with.
#[cfg(unix)]
pub fn main(args: RunArgs) -> Result<i32> {
    let executable = std::env::current_exe()?;
    let socket = ensure_agent(&executable)?;
    let cwd = std::env::current_dir()?;
    let (task, env) = task(&args, &cwd)?;
    let reply = ask(
        &socket,
        &serde_json::json!({"op": "run", "task": task, "env": env}),
    )?;
    let run_id = reply["run_id"]
        .as_str()
        .context("the agent's reply names no run")?
        .to_owned();
    let level = crate::control_level::detect_now();
    if !args.json {
        eprintln!(
            "cedegrid: run {run_id} started through this account's local agent ({}: {})",
            level.level.name(),
            level.reason
        );
    }
    if args.detach {
        if args.json {
            println!(
                "{}",
                serde_json::json!({"run_id": run_id, "control_level": level.level, "control_reason": level.reason})
            );
        }
        return Ok(0);
    }
    // Follow the run until it ends.
    let status = loop {
        let runs = ask(&socket, &serde_json::json!({"op": "ps"}))?;
        let status = runs["runs"]
            .as_array()
            .and_then(|runs| runs.iter().find(|run| run["task_id"] == run_id.as_str()))
            .and_then(|run| run["status"].as_str())
            .map(str::to_owned);
        match status.as_deref() {
            Some("queued" | "assigned" | "running") | None => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Some(_) => break status.unwrap_or_default(),
        }
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({"run_id": run_id, "status": status, "control_level": level.level, "control_reason": level.reason})
        );
    } else {
        eprintln!("cedegrid: run {run_id} {status}");
    }
    Ok(if status == "completed" { 0 } else { 1 })
}

#[cfg(not(unix))]
pub fn main(_args: RunArgs) -> Result<i32> {
    bail!("cedegrid run needs Linux or macOS")
}
