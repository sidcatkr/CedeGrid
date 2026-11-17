//! The broker's calls to systemd (research R8, FR-007, FR-008).
//!
//! Fixed-path `systemd-run` and `systemctl`, argument vectors only, no shell, a cleared
//! environment and a timeout on each call. The unit logic above this is written against the
//! `Systemd` trait so it can be tested without systemd.

use super::admin_policy::AdminPolicy;
use super::request::{Refusal, RefusalCode, UnitStatus};
use super::units::{
    Caller, DESCRIPTION_PREFIX, MARKER_VARIABLE, Placement, SYSTEMCTL, SYSTEMD_RUN, UnitKind,
    UnitSpec, build, marker_value, systemd_run_args, unit_name,
};
use std::ffi::OsString;
use std::time::{Duration, Instant};

/// How long one systemd call may take (*provisional*).
pub const SYSTEMD_TIMEOUT: Duration = Duration::from_secs(10);

/// The properties the broker reads back with `systemctl show`.
const SHOW_PROPERTIES: &str = "LoadState,ActiveState,ControlGroup,Description,Environment";

/// What `systemctl show` says about one unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitShow {
    pub load_state: String,
    pub active_state: String,
    pub control_group: String,
    pub description: String,
    pub environment: Vec<String>,
}

impl UnitShow {
    /// Whether systemd knows no unit of that name.
    pub fn absent(&self) -> bool {
        self.load_state == "not-found" || self.load_state.is_empty()
    }

    /// Whether the unit carries the broker's marker for this kind and caller (FR-007).
    pub fn is_ours(&self, kind: UnitKind, uid: u32) -> bool {
        let marker = format!("{MARKER_VARIABLE}={}", marker_value(kind, uid));
        self.description.starts_with(DESCRIPTION_PREFIX)
            && self.environment.contains(&marker)
    }
}

/// Parses `systemctl show --property=…` output. Unknown lines are ignored.
pub fn parse_show(text: &str) -> UnitShow {
    let mut show = UnitShow::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => show.load_state = value.to_owned(),
            "ActiveState" => show.active_state = value.to_owned(),
            "ControlGroup" => show.control_group = value.to_owned(),
            "Description" => show.description = value.to_owned(),
            "Environment" => {
                show.environment = value.split_whitespace().map(str::to_owned).collect();
            }
            _ => {}
        }
    }
    show
}

/// The systemd operations the broker uses.
pub trait Systemd {
    fn show(&self, unit: &str) -> Result<UnitShow, Refusal>;
    fn start(&self, spec: &UnitSpec) -> Result<(), Refusal>;
    fn stop(&self, unit: &str) -> Result<(), Refusal>;
    fn reset_failed(&self, unit: &str) -> Result<(), Refusal>;
}

/// The real systemd, through its fixed command paths.
#[derive(Clone, Copy, Debug)]
pub struct SystemctlRunner {
    pub timeout: Duration,
}

impl Default for SystemctlRunner {
    fn default() -> Self {
        Self {
            timeout: SYSTEMD_TIMEOUT,
        }
    }
}

impl SystemctlRunner {
    fn run(&self, program: &str, args: Vec<OsString>) -> Result<String, Refusal> {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LANG", "C")
            .env("SYSTEMD_PAGER", "")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| failed())?;
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    // The call is abandoned; killing our own child is all the broker may do here.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Refusal::new(
                        RefusalCode::SystemdTimeout,
                        "systemd did not respond in time; retry later",
                    ));
                }
                Err(_) => return Err(failed()),
            }
        };
        let mut output = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout
                .by_ref()
                .take(64 * 1024)
                .read_to_string(&mut output)
                .map_err(|_| failed())?;
        }
        if status.success() {
            Ok(output)
        } else {
            Err(failed())
        }
    }
}

fn failed() -> Refusal {
    Refusal::new(
        RefusalCode::SystemdFailed,
        "systemd call failed; ask an administrator to check the broker journal",
    )
}

impl Systemd for SystemctlRunner {
    fn show(&self, unit: &str) -> Result<UnitShow, Refusal> {
        let output = self.run(
            SYSTEMCTL,
            vec![
                "show".into(),
                format!("--property={SHOW_PROPERTIES}").into(),
                "--".into(),
                unit.into(),
            ],
        )?;
        Ok(parse_show(&output))
    }

    fn start(&self, spec: &UnitSpec) -> Result<(), Refusal> {
        let mut args: Vec<OsString> = vec!["--quiet".into()];
        args.extend(systemd_run_args(spec));
        self.run(SYSTEMD_RUN, args).map(|_| ())
    }

    fn stop(&self, unit: &str) -> Result<(), Refusal> {
        self.run(SYSTEMCTL, vec!["stop".into(), "--".into(), unit.into()])
            .map(|_| ())
    }

    fn reset_failed(&self, unit: &str) -> Result<(), Refusal> {
        self.run(
            SYSTEMCTL,
            vec!["reset-failed".into(), "--".into(), unit.into()],
        )
        .map(|_| ())
    }
}

const KINDS: [UnitKind; 2] = [UnitKind::Ordinary, UnitKind::Idle];

fn not_ours(name: &str) -> Refusal {
    Refusal::new(
        RefusalCode::UnitNotOurs,
        format!(
            "unit {name} exists but was not created by the broker; ask an administrator to remove it"
        ),
    )
}

fn status_of(kind: UnitKind, name: String, show: &UnitShow) -> UnitStatus {
    UnitStatus {
        kind,
        name,
        state: if show.absent() {
            "absent".to_owned()
        } else {
            show.active_state.clone()
        },
        cgroup: show.control_group.clone(),
    }
}

/// Reads both of the caller's units and refuses before acting if either name belongs to a unit
/// without the broker's marker, so a refusal never leaves half the pair changed (FR-007).
fn checked_shows(
    systemd: &dyn Systemd,
    caller: Caller,
) -> Result<Vec<(UnitKind, String, UnitShow)>, Refusal> {
    KINDS
        .into_iter()
        .map(|kind| {
            let name = unit_name(kind, caller.uid);
            let show = systemd.show(&name)?;
            if !show.absent() && !show.is_ours(kind, caller.uid) {
                return Err(not_ours(&name));
            }
            Ok((kind, name, show))
        })
        .collect()
}

/// Starts the caller's two units. A running unit is left as it is; a failed or inactive one of
/// ours is reset and started again; a unit of that name without the marker refuses (FR-007).
pub fn start_units(
    systemd: &dyn Systemd,
    caller: Caller,
    policy: &AdminPolicy,
    placement: Placement,
) -> Result<Vec<UnitStatus>, Refusal> {
    let shows = checked_shows(systemd, caller)?;
    let mut statuses = Vec::with_capacity(shows.len());
    for (kind, name, show) in shows {
        let spec = build(kind, caller, policy, placement);
        if show.absent() {
            systemd.start(&spec)?;
        } else if !matches!(
            show.active_state.as_str(),
            "active" | "activating" | "reloading"
        ) {
            // A loaded transient unit keeps its name until reset; ours may be failed or inactive.
            systemd.reset_failed(&name)?;
            systemd.start(&spec)?;
        }
        let after = systemd.show(&name)?;
        if !after.absent() && !after.is_ours(kind, caller.uid) {
            return Err(not_ours(&name));
        }
        statuses.push(status_of(kind, name, &after));
    }
    Ok(statuses)
}

/// Stops the caller's two units, checking both markers before either stop (FR-007).
pub fn stop_units(systemd: &dyn Systemd, caller: Caller) -> Result<Vec<UnitStatus>, Refusal> {
    let shows = checked_shows(systemd, caller)?;
    let mut statuses = Vec::with_capacity(shows.len());
    for (kind, name, show) in shows {
        if show.absent() {
            statuses.push(status_of(kind, name, &show));
            continue;
        }
        systemd.stop(&name)?;
        let after = systemd.show(&name)?;
        statuses.push(status_of(kind, name, &after));
    }
    Ok(statuses)
}

/// Reports the caller's two units.
pub fn status_units(systemd: &dyn Systemd, caller: Caller) -> Result<Vec<UnitStatus>, Refusal> {
    Ok(checked_shows(systemd, caller)?
        .into_iter()
        .map(|(kind, name, show)| status_of(kind, name, &show))
        .collect())
}
