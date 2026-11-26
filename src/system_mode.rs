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

/// Whether an unprotected raise may be in force now: a run started unprotected is unreleased,
/// and no protected first-comer is on the CPU (006 FR-016, FR-018). `own_arrival` is own work's
/// earliest arrival there; without one, every other account with Performance or Aggressive
/// CedeGrid work on the CPU counts as a protected first-comer.
pub fn raise_wanted(
    unprotected_runs: usize,
    own_arrival: Option<u64>,
    principals: &[crate::model::PrincipalUsage],
) -> bool {
    unprotected_runs > 0
        && !principals.iter().any(|principal| {
            !principal.is_owner
                && principal
                    .cedegrid_cpu
                    .and_then(|work| work.protected_arrival_unix_ms)
                    .is_some_and(|at| own_arrival.is_none_or(|own| at <= own))
        })
}

/// What this host offers for priority over plain users (006 FR-025, FR-029).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlainPriorityHost {
    /// System mode permits it, at this account slice CPU weight.
    Permitted(u64),
    /// No System mode here, so no account slice can be raised.
    Rootless,
    /// The administrator policy leaves `modes.plain_priority` off.
    NotPermitted,
    /// The administrator policy cannot be read, which permits nothing.
    Unreadable(String),
}

/// What this host offers for priority over plain users. The policy is read only where System
/// mode is installed.
pub fn plain_priority_host(
    installed: bool,
    policy: impl FnOnce() -> anyhow::Result<crate::broker::admin_policy::AdminPolicy>,
) -> PlainPriorityHost {
    if !installed {
        return PlainPriorityHost::Rootless;
    }
    match policy() {
        Ok(policy) if policy.plain_priority.value => {
            PlainPriorityHost::Permitted(policy.plain_priority_cpu_weight.value)
        }
        Ok(_) => PlainPriorityHost::NotPermitted,
        Err(error) => PlainPriorityHost::Unreadable(format!("{error:#}")),
    }
}

/// What this host offers now; System mode counts as installed where its policy file exists.
#[cfg(unix)]
pub fn plain_priority_host_now() -> PlainPriorityHost {
    let installed = Path::new(crate::broker::admin_policy::POLICY_DIR)
        .join(crate::broker::admin_policy::POLICY_FILE)
        .exists();
    plain_priority_host(installed, crate::broker::admin_policy::load)
}

#[cfg(not(unix))]
pub fn plain_priority_host_now() -> PlainPriorityHost {
    PlainPriorityHost::Rootless
}

/// Priority over plain users as `doctor` and `observe` show it (006 FR-029).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PlainPriorityStatus {
    /// The user's choice, `contention.priority_over_plain`.
    pub chosen: bool,
    /// Whether this host's System mode policy permits it.
    pub permitted: bool,
    /// The account slice weight it sets, where the policy can be read.
    pub weight: Option<u64>,
    /// Whether this account's slice runs at that weight now.
    pub in_force: bool,
    /// Why it is not in force, in one sentence.
    pub reason: Option<String>,
}

/// The status from the choice, what the host offers, this account's slice weight and the number
/// of other accounts with CedeGrid work on the CPU below the weight.
pub fn plain_priority_status(
    chosen: bool,
    host: &PlainPriorityHost,
    own_weight: Option<u64>,
    below: usize,
) -> PlainPriorityStatus {
    let weight = match host {
        PlainPriorityHost::Permitted(weight) => Some(*weight),
        _ => None,
    };
    let in_force = chosen && weight.is_some() && own_weight == weight;
    let reason = match (host, weight) {
        _ if in_force => None,
        _ if !chosen => {
            Some("not chosen; run cedegrid setup --priority-over-plain to choose it".to_owned())
        }
        (PlainPriorityHost::Rootless, _) => {
            Some("this host runs CedeGrid rootless; it needs System mode".to_owned())
        }
        (PlainPriorityHost::NotPermitted, _) => {
            Some("the System mode policy does not permit it (modes.plain_priority)".to_owned())
        }
        (PlainPriorityHost::Unreadable(why), _) => {
            Some(format!("the System mode policy cannot be read: {why}"))
        }
        (_, Some(weight)) if own_weight.is_some_and(|own| own > weight) => Some(format!(
            "the unprotected raise is in force at weight {}",
            own_weight.unwrap_or_default()
        )),
        (_, Some(weight)) if below > 0 => Some(format!(
            "another account's CedeGrid work on the CPU runs below weight {weight}"
        )),
        _ => Some("it holds only while own Performance or Aggressive CPU work runs".to_owned()),
    };
    PlainPriorityStatus {
        chosen,
        permitted: weight.is_some(),
        weight,
        in_force,
        reason,
    }
}

/// `doctor`'s section for priority over plain users.
pub fn plain_priority_text(status: &PlainPriorityStatus) -> String {
    let yes_no = |on: bool| if on { "yes" } else { "no" };
    let permitted = match status.weight {
        Some(weight) => format!("yes, at weight {weight}"),
        None => "no".to_owned(),
    };
    let in_force = match &status.reason {
        Some(reason) if !status.in_force => format!("no: {reason}"),
        _ => yes_no(status.in_force).to_owned(),
    };
    let mut out = String::from("\nPriority over plain users\n");
    for line in crate::setup::key_values(
        &[
            ("chosen", yes_no(status.chosen).to_owned()),
            ("permitted", permitted),
            ("in force", in_force),
        ],
        2,
    ) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The status on this host now: the policy where System mode is installed, this account's slice
/// weight and the other accounts in `principals` below the weight.
pub fn plain_priority_status_now(
    chosen: bool,
    principals: &[crate::model::PrincipalUsage],
) -> PlainPriorityStatus {
    let host = plain_priority_host_now();
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions and cannot fail.
    let own = crate::broker::units::account_slice_weight(unsafe { libc::getuid() });
    #[cfg(not(unix))]
    let own = None;
    let below = match host {
        PlainPriorityHost::Permitted(weight) => below_plain_weight(
            weight,
            principals,
            crate::broker::units::account_slice_weight,
        )
        .len(),
        _ => 0,
    };
    plain_priority_status(chosen, &host, own, below)
}

/// The accounts with CedeGrid work on the CPU whose account slice is below `weight`, which keep
/// priority over plain users from holding (006 FR-026). `slice_weight` reads one account
/// slice's CPU weight; an unreadable slice or an unknown owner counts as below.
pub fn below_plain_weight(
    weight: u64,
    principals: &[crate::model::PrincipalUsage],
    slice_weight: impl Fn(u32) -> Option<u64>,
) -> std::collections::BTreeSet<Option<u32>> {
    principals
        .iter()
        .filter(|principal| !principal.is_owner && principal.cedegrid_cpu.is_some())
        .filter(|principal| {
            !principal
                .uid
                .and_then(&slice_weight)
                .is_some_and(|current| current >= weight)
        })
        .map(|principal| principal.uid)
        .collect()
}

/// Priority over plain users across intervals (006 FR-026). It is raised only while no other
/// CedeGrid account on the CPU is below its weight. When one falls below a held raise, the raise
/// is kept for the withdrawal window of two monitor intervals: an account that chose it too sees
/// the raise and follows, and one that did not gets its share back when the window ends. An
/// account whose owner is unknown can never follow, so the raise ends at once for it.
#[derive(Clone, Debug, Default)]
pub struct PlainSteer {
    /// Whether the last answer held the raise.
    held: bool,
    /// When an account first fell below the held raise.
    below_since: Option<u64>,
}

impl PlainSteer {
    /// Whether the plain raise is wanted at `now_ms`. `eligible` means the user chose it, the
    /// policy permits it and own Performance or Aggressive CPU work runs; `below` is
    /// `below_plain_weight`; `window_ms` is two monitor intervals.
    pub fn wanted(
        &mut self,
        eligible: bool,
        below: &std::collections::BTreeSet<Option<u32>>,
        now_ms: u64,
        window_ms: u64,
    ) -> bool {
        if eligible && below.is_empty() {
            self.held = true;
            self.below_since = None;
            return true;
        }
        if eligible && self.held && !below.contains(&None) {
            let since = *self.below_since.get_or_insert(now_ms);
            if now_ms < since.saturating_add(window_ms) {
                return true;
            }
        }
        self.held = false;
        self.below_since = None;
        false
    }
}

/// The raise to ask for: the one wanted, or of two the one whose policy weight is higher,
/// unprotected on a tie (006 FR-026).
pub fn pick_raise(
    unprotected: bool,
    plain: bool,
    unprotected_weight: u64,
    plain_weight: u64,
) -> Option<crate::broker::request::RaiseLevel> {
    use crate::broker::request::RaiseLevel;
    match (unprotected, plain) {
        (true, true) if plain_weight > unprotected_weight => Some(RaiseLevel::Plain),
        (true, _) => Some(RaiseLevel::Unprotected),
        (false, true) => Some(RaiseLevel::Plain),
        (false, false) => None,
    }
}

/// One condition an unprotected grant lacks: its code, what is missing and how to
/// fix it. `doctor` lists them as bullets; a refusal joins them into one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    pub code: Option<&'static str>,
    pub what: String,
    pub fix: String,
}

impl Gap {
    fn new(code: &'static str, what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            what: what.into(),
            fix: fix.into(),
        }
    }

    /// The gap as a refusal names it: `CODE: what`.
    pub fn line(&self) -> String {
        match self.code {
            Some(code) => format!("{code}: {}", self.what),
            None => self.what.clone(),
        }
    }
}

/// What this host lacks for any unprotected grant (006 FR-015): the allowance and, where it
/// applies, the administrator policy. Empty when a confirmed grant may start. Unprotected
/// applies to Performance and Aggressive work, so the node cap is no condition.
pub fn host_gaps(allowance: crate::state::Allowance, policy: Option<Gap>) -> Vec<Gap> {
    let mut gaps = Vec::new();
    match allowance {
        crate::state::Allowance::On { .. } => {}
        crate::state::Allowance::Off { reason: None } => gaps.push(Gap::new(
            "ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED",
            "the host's \"allow unprotected\" setting is off",
            "run cedegrid setup --allow-unprotected on a terminal",
        )),
        crate::state::Allowance::Off {
            reason: Some(reason),
        } => gaps.push(Gap::new(
            "ERR_CEDEGRID_UNPROTECTED_ALLOWANCE_UNREADABLE",
            format!("{reason}; treated as off"),
            "repair the agent's state database, then allow it again in setup",
        )),
    }
    gaps.extend(policy);
    gaps
}

/// Why an unprotected grant may not start (004 FR-014, FR-015, FR-015a), naming every
/// missing condition in one line, or `None` when it may (or carries no grant). The allowance and,
/// in a System mode unit, the administrator policy are read only when a grant is present.
pub fn grant_refusal(
    contention: Option<&crate::execution_model::JobContention>,
    allowance: impl FnOnce() -> crate::state::Allowance,
    unit_policy: impl FnOnce() -> Option<(&'static str, String)>,
) -> Option<(&'static str, String)> {
    let grant = contention.and_then(|contention| contention.unprotected)?;
    let mut gaps = Vec::new();
    if !grant.confirmed {
        gaps.push(Gap::new(
            "ERR_CEDEGRID_UNPROTECTED_CONFIRMATION",
            "confirm_unprotected is missing",
            "send confirm_unprotected with the submission",
        ));
    }
    let policy = unit_policy().map(|(_, line)| Gap {
        code: None,
        what: line,
        fix: String::new(),
    });
    gaps.extend(host_gaps(allowance(), policy));
    let lines: Vec<String> = gaps.iter().map(Gap::line).collect();
    (!lines.is_empty()).then(|| ("unprotected_refused", lines.join("; ")))
}

/// The administrator policy's gap for unprotected in a System mode unit (004
/// FR-015a), with its event code, or `None` when it permits one. A policy that cannot be read
/// refuses.
pub fn unprotected_policy_gap(
    policy: anyhow::Result<crate::broker::admin_policy::AdminPolicy>,
) -> Option<(&'static str, Gap)> {
    match policy {
        Ok(policy) if policy.unprotected.value => None,
        Ok(_) => Some((
            "unprotected_policy",
            Gap::new(
                "ERR_CEDEGRID_UNPROTECTED_POLICY",
                "this host's System mode policy does not permit unprotected (modes.unprotected)",
                "an administrator sets modes.unprotected = true in /etc/cedegrid/policy.toml",
            ),
        )),
        Err(error) => Some((
            "policy_refused",
            Gap {
                code: None,
                what: format!("/etc/cedegrid/policy.toml rejected: {error:#}"),
                fix: "an administrator corrects /etc/cedegrid/policy.toml".into(),
            },
        )),
    }
}

/// Why the administrator policy refuses an unprotected grant in a System mode unit
/// (004 FR-015a), or `None` when it permits one. A policy that cannot be read refuses.
pub fn unprotected_refusal_with(
    policy: anyhow::Result<crate::broker::admin_policy::AdminPolicy>,
) -> Option<(&'static str, String)> {
    unprotected_policy_gap(policy).map(|(code, gap)| (code, gap.line()))
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
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return silent(timeout);
    }
    exchange(&mut stream, request, timeout)
}

#[cfg(unix)]
fn silent(timeout: Duration) -> Outcome {
    Outcome::NoAnswer(format!("did not answer within {} s", timeout.as_secs_f64()))
}

/// Writes the request and reads the one reply line. A broker that refuses before reading, as a
/// rate-limited or busy one does, has already written its reply and closed, so a broken pipe on
/// the write still reads that reply.
#[cfg(unix)]
fn exchange<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    request: &Request,
    timeout: Duration,
) -> Outcome {
    use std::io::{BufRead, BufReader, ErrorKind, Read};

    if let Err(error) = stream.write_all(request.to_line().as_bytes())
        && !matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
        )
    {
        return silent(timeout);
    }
    let mut line = Vec::new();
    let mut reader = BufReader::new(stream.take(MAX_REPLY_BYTES));
    match reader.read_until(b'\n', &mut line) {
        Ok(_) if line.ends_with(b"\n") => {}
        Ok(_) => return Outcome::NoAnswer("closed the connection without a reply".to_owned()),
        Err(_) => return silent(timeout),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind, Read, Write};

    /// A connection whose write fails as `write` says and whose read returns `reply`.
    struct Closed {
        write: ErrorKind,
        reply: std::io::Cursor<Vec<u8>>,
    }

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(Error::from(self.write))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for Closed {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    /// A rate-limited broker answers and closes before reading, so the client's write breaks;
    /// the reply is still read, and the caller learns it was rate limited, not that the broker
    /// was silent.
    #[test]
    fn a_reply_sent_before_the_request_was_read_is_still_read() {
        let busy = Reply::refused(&Refusal::new(RefusalCode::RateLimited, "busy")).to_line();
        let timeout = Duration::from_secs(2);
        for kind in [ErrorKind::BrokenPipe, ErrorKind::ConnectionReset] {
            let mut stream = Closed {
                write: kind,
                reply: std::io::Cursor::new(busy.clone().into_bytes()),
            };
            match exchange(&mut stream, &Request::Start { mode: None }, timeout) {
                Outcome::Refused(refusal) => assert_eq!(refusal.code, RefusalCode::RateLimited),
                other => panic!("{kind:?}: {other:?}"),
            }
        }
        // A write that times out is a silent broker.
        let mut stream = Closed {
            write: ErrorKind::TimedOut,
            reply: std::io::Cursor::new(busy.into_bytes()),
        };
        assert!(matches!(
            exchange(&mut stream, &Request::Start { mode: None }, timeout),
            Outcome::NoAnswer(_)
        ));
    }
}
