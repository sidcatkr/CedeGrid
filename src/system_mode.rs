//! System mode's client side (feature 005).
//!
//! The root refusal every command starts with (FR-015), and the one request a member's
//! `cedegrid system` sends the broker, with the fallback when the broker is missing or silent
//! (FR-013). Nothing here needs privilege.

use crate::broker::BROKER_SUBCOMMAND;
use crate::broker::request::{Refusal, RefusalCode, Reply, Request, UnitStatus};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

/// Set once when this agent runs in a System mode unit, after `unit_jobs_root` checked it.
static IN_UNIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Records that this process is the agent in a System mode unit.
pub fn set_in_unit() {
    let _ = IN_UNIT.set(true);
}

/// Whether this process is the agent in a System mode unit, where the broker is the only
/// writer of OOM scores (005 FR-023, T049).
pub fn in_unit() -> bool {
    IN_UNIT.get().copied().unwrap_or(false)
}

/// The cgroup files the agent in a unit may write: the gate's self-join and Full's runtime steps.
pub const UNIT_CONTROLS: &[&str] = &[
    "cgroup.procs",
    "cpu.weight",
    "cpu.max",
    "cgroup.freeze",
    "memory.high",
];

/// The unit's settings in the configuration file's table, before it is validated: the cgroup
/// backend on, the unit's jobs cgroup as its delegated root and Full's controls authorized. The
/// file needs no `delegated_root` of its own, and one it has is replaced.
pub fn unit_table(table: &mut toml::Table, jobs: &Path) -> anyhow::Result<()> {
    let jobs = jobs
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("the unit's jobs cgroup path is not UTF-8"))?;
    let cgroup = table
        .entry("cgroup")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[cgroup] must be a table"))?;
    cgroup.insert("enabled".into(), toml::Value::Boolean(true));
    cgroup.insert("delegated_root".into(), toml::Value::String(jobs.into()));
    let controls = cgroup
        .entry("authorized_controls")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("cgroup.authorized_controls must be an array"))?;
    for control in UNIT_CONTROLS {
        if !controls.iter().any(|c| c.as_str() == Some(control)) {
            controls.push(toml::Value::String((*control).into()));
        }
    }
    Ok(())
}

/// Points the agent's cgroup backend and kernel readback at the unit's jobs cgroup and turns the
/// Full level on (FR-012): the unit's subtree is CedeGrid's own, so no operator opt-in is needed.
/// Returns a warning for each setting that has no effect in a unit.
pub fn apply_unit(config: &mut crate::config::Config, jobs: std::path::PathBuf) -> Vec<String> {
    let mut warnings = Vec::new();
    config.cgroup.enabled = true;
    let controls = &mut config.cgroup.authorized_controls;
    for control in UNIT_CONTROLS {
        if !controls.iter().any(|c| c == control) {
            controls.push((*control).to_owned());
        }
    }
    config.kernel.delegated_root = Some(jobs.clone());
    config.cgroup.delegated_root = Some(jobs);
    // The broker is the only writer of OOM scores in a unit (T049).
    if config.ram.prefer_own_oom_victim {
        warnings.push(
            "ram.prefer_own_oom_victim has no effect in System mode; the broker sets OOM scores"
                .to_owned(),
        );
        config.ram.prefer_own_oom_victim = false;
    }
    warnings
}

/// The modes a job would run at, one per resource: its request or the node's mode, capped by
/// the node's most aggressive allowed mode (001 FR-030).
pub fn job_modes(
    contention: &crate::config::ContentionConfig,
    request: Option<&crate::execution_model::JobContention>,
) -> Vec<crate::contention::ContentionMode> {
    use crate::contention::{ResourceKind, effective_mode};
    let kinds = [
        ResourceKind::Cpu,
        ResourceKind::Ram,
        ResourceKind::GpuCompute,
        ResourceKind::GpuMemory,
    ];
    let most = kinds
        .iter()
        .map(|kind| contention.mode(*kind))
        .max()
        .unwrap_or(contention.default_mode);
    let cap = contention.max_mode.unwrap_or(most);
    kinds
        .iter()
        .map(|kind| {
            let requested = request.and_then(|request| request.modes.get(kind).copied());
            effective_mode(contention.mode(*kind), requested, cap)
        })
        .collect()
}

/// Why an unprotected Aggressive grant may not start (004 FR-014, FR-015, FR-015a), naming every
/// missing condition in one line, or `None` when it may (or carries no grant). The allowance and,
/// in a System mode unit, the administrator policy are read only when a grant is present.
pub fn grant_refusal(
    contention: Option<&crate::execution_model::JobContention>,
    cap: crate::contention::ContentionMode,
    allowance: impl FnOnce() -> crate::state::Allowance,
    unit_policy: impl FnOnce() -> Option<(&'static str, String)>,
) -> Option<(&'static str, String)> {
    let grant = contention.and_then(|contention| contention.unprotected_aggressive)?;
    let mut missing = Vec::new();
    if !grant.confirmed {
        missing.push(
            "ERR_CEDEGRID_UNPROTECTED_CONFIRMATION: confirm_unprotected_aggressive is missing"
                .to_owned(),
        );
    }
    if cap < crate::contention::ContentionMode::Aggressive {
        missing.push(
            "ERR_CEDEGRID_UNPROTECTED_CAPPED: the node cap does not include Aggressive".to_owned(),
        );
    }
    match allowance() {
        crate::state::Allowance::On { .. } => {}
        crate::state::Allowance::Off { reason: None } => missing.push(
            "ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED: the host's \"allow unprotected Aggressive\" setting is off"
                .to_owned(),
        ),
        crate::state::Allowance::Off { reason: Some(reason) } => missing.push(format!(
            "ERR_CEDEGRID_UNPROTECTED_ALLOWANCE_UNREADABLE: {reason}; treated as off"
        )),
    }
    if let Some((_, line)) = unit_policy() {
        missing.push(line);
    }
    (!missing.is_empty()).then(|| ("unprotected_refused", missing.join("; ")))
}

/// Why the administrator policy refuses an unprotected Aggressive grant in a System mode unit
/// (004 FR-015a), or `None` when it permits one. A policy that cannot be read refuses.
pub fn unprotected_refusal_with(
    policy: anyhow::Result<crate::broker::admin_policy::AdminPolicy>,
) -> Option<(&'static str, String)> {
    match policy {
        Ok(policy) if policy.unprotected_aggressive.value => None,
        Ok(_) => Some((
            "unprotected_policy",
            "ERR_CEDEGRID_UNPROTECTED_POLICY: this host's System mode policy does not permit unprotected Aggressive (modes.unprotected_aggressive)".to_owned(),
        )),
        Err(error) => Some((
            "policy_refused",
            format!("/etc/cedegrid/policy.toml rejected: {error:#}"),
        )),
    }
}

/// Why the administrator policy refuses a launch, as an event code and one line naming the
/// policy key (FR-010); `None` when it may start. `active` counts the tasks already running or
/// preparing. A policy that cannot be read refuses every launch.
pub fn launch_refusal_with(
    policy: anyhow::Result<crate::broker::admin_policy::AdminPolicy>,
    modes: &[crate::contention::ContentionMode],
    active: usize,
) -> Option<(&'static str, String)> {
    let policy = match policy {
        Ok(policy) => policy,
        Err(error) => {
            return Some((
                "policy_refused",
                format!("/etc/cedegrid/policy.toml rejected: {error:#}"),
            ));
        }
    };
    if let Some(mode) = modes.iter().find(|mode| {
        crate::broker::ModeName::from_name(mode.name()).is_none_or(|name| !policy.allows(name))
    }) {
        let allowed: Vec<&str> = policy
            .allowed_modes
            .value
            .iter()
            .map(|mode| mode.name())
            .collect();
        return Some((
            "mode_not_allowed",
            format!(
                "mode {} not allowed by policy modes.allowed (allowed: {})",
                mode.name(),
                allowed.join(", ")
            ),
        ));
    }
    let limit = policy.tasks_per_user.value as usize;
    (active >= limit).then(|| {
        (
            "task_limit",
            format!("{active} tasks running reach policy limits.tasks_per_user ({limit})"),
        )
    })
}

/// `launch_refusal_with` for this host's policy and a job's request.
#[cfg(unix)]
pub fn launch_refusal(
    config: &crate::config::Config,
    request: &crate::execution_model::LaunchRequest,
    active: usize,
) -> Option<(&'static str, String)> {
    let modes = job_modes(&config.contention, request.contention.as_ref());
    launch_refusal_with(crate::broker::admin_policy::load(), &modes, active)
}

/// Without unix sockets there is no System mode, so nothing is refused here.
#[cfg(not(unix))]
pub fn launch_refusal(
    _config: &crate::config::Config,
    _request: &crate::execution_model::LaunchRequest,
    _active: usize,
) -> Option<(&'static str, String)> {
    None
}

/// How long the client waits for the broker before it falls back (*provisional*, FR-013).
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the client waits for a stop: the broker waits for systemd to stop both units, each
/// call bounded by `SYSTEMD_TIMEOUT`, and a stop has nothing to fall back to.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(25);

/// The reply timeout for a request.
pub fn reply_timeout(request: &Request) -> Duration {
    match request {
        Request::Stop => STOP_TIMEOUT,
        _ => CLIENT_TIMEOUT,
    }
}

/// The most the client reads of a reply.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// The line a command run as root refuses with, or `None` when it may run (FR-015, 003 FR-082).
///
/// The broker's own subcommand, started by its unit as root, is the one exception.
pub fn root_refusal(euid: u32, first_arg: Option<&OsStr>) -> Option<String> {
    if euid != 0 || first_arg == Some(OsStr::new(BROKER_SUBCOMMAND)) {
        return None;
    }
    Some("refusing to run as root; run cedegrid as your own user, without sudo".to_owned())
}

/// What came back from asking the broker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No broker socket: the system package is not installed (FR-001).
    NotInstalled,
    /// The socket exists but no usable answer came within the timeout; the reason is for the
    /// one line the client prints.
    NoAnswer(String),
    /// The broker served the request.
    Units(Vec<UnitStatus>),
    /// The broker refused the request.
    Refused(Refusal),
}

/// What the client does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run in System mode, in these units.
    System(Vec<UnitStatus>),
    /// Run as without the package, after printing the line if there is one.
    Fallback(Option<String>),
    /// Stop with this line.
    Refuse(String),
}

/// Decides between System mode, the fallback and a refusal (FR-010, FR-013, FR-035).
pub fn decide(outcome: Outcome, require_system: bool) -> Decision {
    let fallback = |line: String| {
        if require_system {
            Decision::Refuse(format!(
                "System mode required (--require-system) but unavailable: {line}"
            ))
        } else {
            Decision::Fallback(Some(format!(
                "System mode unavailable, continuing without it: {line}"
            )))
        }
    };
    match outcome {
        Outcome::Units(units) => Decision::System(units),
        Outcome::NotInstalled if require_system => Decision::Refuse(
            "System mode required (--require-system) but cedegrid-system is not installed on this host"
                .to_owned(),
        ),
        // Without the package, nothing changes and nothing is printed (FR-013).
        Outcome::NotInstalled => Decision::Fallback(None),
        Outcome::NoAnswer(reason) => fallback(format!("broker {reason}")),
        Outcome::Refused(refusal) => match refusal.code {
            // The administrator's policy is refused, never bypassed by the fallback (FR-010).
            RefusalCode::ModeNotAllowed | RefusalCode::TaskLimit | RefusalCode::UnitNotOurs => {
                Decision::Refuse(refusal.message)
            }
            _ => fallback(refusal.message),
        },
    }
}

/// Sends `request` to the broker at `socket` and reads its one reply within `timeout`.
#[cfg(unix)]
pub fn ask(socket: &Path, request: &Request, timeout: Duration) -> Outcome {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream;

    match std::fs::symlink_metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Outcome::NotInstalled,
        Err(_) => return Outcome::NoAnswer("socket cannot be read".to_owned()),
        Ok(meta) if !meta.file_type().is_socket() => {
            return Outcome::NoAnswer("socket path is not a socket".to_owned());
        }
        Ok(_) => {}
    }
    // A unix-socket connect does not wait for the server; the timeouts below bound the rest.
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Outcome::Refused(Refusal::new(
                RefusalCode::NotAMember,
                "not a member of group cedegrid; ask an administrator to add your account",
            ));
        }
        Err(_) => return Outcome::NoAnswer("is not running".to_owned()),
    };
    let silent = || Outcome::NoAnswer(format!("did not answer within {} s", timeout.as_secs_f64()));
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return silent();
    }
    if stream.write_all(request.to_line().as_bytes()).is_err() {
        return silent();
    }
    let mut line = Vec::new();
    let mut reader = BufReader::new((&stream).take(MAX_REPLY_BYTES));
    match reader.read_until(b'\n', &mut line) {
        Ok(_) if line.ends_with(b"\n") => {}
        Ok(_) => return Outcome::NoAnswer("closed the connection without a reply".to_owned()),
        Err(_) => return silent(),
    }
    reply_outcome(&line)
}

/// Without unix sockets there is no System mode.
#[cfg(not(unix))]
pub fn ask(_socket: &Path, _request: &Request, _timeout: Duration) -> Outcome {
    Outcome::NotInstalled
}

/// Reads one reply line.
pub fn reply_outcome(line: &[u8]) -> Outcome {
    let Ok(reply) = serde_json::from_slice::<Reply>(line) else {
        return Outcome::NoAnswer("sent an unreadable reply".to_owned());
    };
    if reply.protocol != crate::broker::PROTOCOL {
        let side = if reply.protocol > crate::broker::PROTOCOL {
            "upgrade the cedegrid client"
        } else {
            "ask an administrator to upgrade cedegrid-system"
        };
        return Outcome::Refused(Refusal::new(
            RefusalCode::ProtocolMismatch,
            format!(
                "protocol mismatch: broker {}, client {}; {side}",
                reply.protocol,
                crate::broker::PROTOCOL
            ),
        ));
    }
    match (reply.ok, reply.code) {
        (true, None) => Outcome::Units(reply.units.unwrap_or_default()),
        (false, Some(code)) => Outcome::Refused(Refusal::new(
            code,
            reply
                .message
                .unwrap_or_else(|| "request refused by the broker".to_owned()),
        )),
        _ => Outcome::NoAnswer("sent an unreadable reply".to_owned()),
    }
}

/// What `cedegrid system` prints.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SystemReport {
    /// Whether the request was served in System mode.
    pub system: bool,
    pub units: Vec<UnitStatus>,
}

/// Runs one `cedegrid system` request against the installed broker.
pub fn run(request: &Request, require_system: bool) -> anyhow::Result<SystemReport> {
    let outcome = ask(
        Path::new(crate::broker::SOCKET_PATH),
        request,
        reply_timeout(request),
    );
    match decide(outcome, require_system) {
        Decision::System(units) => Ok(SystemReport {
            system: true,
            units,
        }),
        Decision::Fallback(line) => {
            if let Some(line) = line {
                use std::io::Write;
                // A lost notice does not change what runs.
                let _ = writeln!(std::io::stderr().lock(), "cedegrid: {line}");
            }
            Ok(SystemReport {
                system: false,
                units: Vec::new(),
            })
        }
        Decision::Refuse(line) => anyhow::bail!(line),
    }
}

/// What the two units run until the agent and the idle executor are built into them (T022):
/// the process lays out the unit's cgroups, then holds the unit, and with it the user's
/// delegated cgroup, until systemd stops it.
#[cfg(target_os = "linux")]
pub fn hold_unit(kind: crate::broker::units::UnitKind) -> anyhow::Result<()> {
    let jobs = prepare_unit(kind)?;
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "cedegrid: System mode unit ready; jobs go to {}",
        jobs.display()
    );
    loop {
        std::thread::park();
    }
}

/// Where the agent in the ordinary unit reads its configuration, as the user: `cedegrid.toml`
/// and `deployment.toml` in `$XDG_CONFIG_HOME/cedegrid`, else `~/.config/cedegrid`.
pub fn unit_config_dir(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<std::path::PathBuf> {
    let base = match xdg_config_home.filter(|value| !value.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(home.filter(|value| !value.is_empty())?).join(".config"),
    };
    base.is_absolute().then(|| base.join("cedegrid"))
}

/// The ordinary unit's process (T022): lays out the unit, then runs the user's agent with the
/// user's own configuration, its jobs in the unit's jobs cgroup. Without a configuration it holds
/// the unit and says so once.
#[cfg(target_os = "linux")]
pub fn run_system_agent() -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    prepare_unit(crate::broker::units::UnitKind::Ordinary)?;
    let dir = unit_config_dir(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let files = dir.map(|dir| (dir.join("cedegrid.toml"), dir.join("deployment.toml")));
    match files {
        Some((node, deployment)) if node.is_file() && deployment.is_file() => {
            let error = std::process::Command::new(std::env::current_exe()?)
                .arg("--config")
                .arg(&node)
                .arg("agent")
                .arg("--deployment")
                .arg(&deployment)
                .exec();
            Err(anyhow::Error::new(error).context("cannot start the agent in the System mode unit"))
        }
        _ => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "cedegrid: no agent configuration in ~/.config/cedegrid (cedegrid.toml and deployment.toml); the unit holds its cgroups"
            );
            loop {
                std::thread::park();
            }
        }
    }
}

/// The jobs' delegated root when this process is the agent in a System mode unit, checked as
/// `cgroup::system_unit_jobs_root` checks it; `None` outside System mode.
#[cfg(target_os = "linux")]
pub fn unit_jobs_root() -> anyhow::Result<Option<std::path::PathBuf>> {
    use anyhow::Context;
    use std::os::unix::fs::MetadataExt;
    let Some(marker) = std::env::var_os(crate::broker::units::MARKER_VARIABLE) else {
        return Ok(None);
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let own = std::fs::read_to_string("/proc/self/cgroup").context("cannot read own cgroup")?;
    let own = crate::broker::process_handle::parse_cgroup_v2(&own)
        .context("not in a cgroup v2 hierarchy")?;
    let jobs = crate::cgroup::system_unit_jobs_root(
        &own,
        uid,
        marker.to_str(),
        Path::new("/sys/fs/cgroup"),
        |path| std::fs::metadata(path).ok().map(|meta| meta.uid()),
    )?;
    Ok(Some(jobs))
}

/// Lays out a System mode unit's cgroups as the user (T022, T023): this process moves into
/// `<unit>/agent`, the cpu, memory and pids controllers are passed down, and `<unit>/jobs` is
/// made for the jobs' leaves. Returns the jobs' directory.
#[cfg(target_os = "linux")]
pub fn prepare_unit(kind: crate::broker::units::UnitKind) -> anyhow::Result<std::path::PathBuf> {
    use crate::broker::units::{IDLE_SLICE, UnitKind, marker_value, unit_name};
    use anyhow::{Context, ensure};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let mount = Path::new("/sys/fs/cgroup");
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let own = std::fs::read_to_string("/proc/self/cgroup").context("cannot read own cgroup")?;
    let own = crate::broker::process_handle::parse_cgroup_v2(&own)
        .context("not in a cgroup v2 hierarchy")?;
    let marker = std::env::var(crate::broker::units::MARKER_VARIABLE).ok();
    let owner = |path: &Path| std::fs::metadata(path).ok().map(|meta| meta.uid());
    let jobs = match kind {
        UnitKind::Ordinary => {
            crate::cgroup::system_unit_jobs_root(&own, uid, marker.as_deref(), mount, owner)?
        }
        UnitKind::Idle => {
            ensure!(
                marker.as_deref() == Some(marker_value(UnitKind::Idle, uid).as_str()),
                "not started by the System mode broker for this user"
            );
            let unit = format!("/{IDLE_SLICE}/{}", unit_name(UnitKind::Idle, uid));
            let base = own.strip_suffix("/agent").unwrap_or(&own);
            ensure!(base == unit, "cgroup {own} is not this user's idle unit");
            let dir = mount.join(unit.trim_start_matches('/'));
            ensure!(
                owner(&dir) == Some(uid),
                "{} is not delegated to user ID {uid}",
                dir.display()
            );
            dir.join(crate::broker::oom::JOBS_DIR)
        }
    };
    let unit_dir = jobs
        .parent()
        .context("jobs directory has no parent")?
        .to_owned();
    let agent = unit_dir.join("agent");
    for dir in [&agent, &jobs] {
        match std::fs::DirBuilder::new().mode(0o755).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", dir.display()));
            }
        }
    }
    if !own.ends_with("/agent") {
        // "0" moves the writing process; no other process is named.
        std::fs::write(agent.join("cgroup.procs"), "0")
            .context("cannot move into the unit's agent cgroup")?;
    }
    for dir in [&unit_dir, &jobs] {
        let available = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
        let wanted: Vec<String> = ["cpu", "memory", "pids"]
            .into_iter()
            .filter(|name| available.split_whitespace().any(|have| have == *name))
            .map(|name| format!("+{name}"))
            .collect();
        if !wanted.is_empty() {
            std::fs::write(dir.join("cgroup.subtree_control"), wanted.join(" "))
                .with_context(|| format!("cannot pass controllers down in {}", dir.display()))?;
        }
    }
    Ok(jobs)
}
