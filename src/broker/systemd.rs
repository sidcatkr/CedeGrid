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
use std::path::Path;
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
        self.description.starts_with(DESCRIPTION_PREFIX) && self.environment.contains(&marker)
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
    /// A slice's `CPUWeight`; `None` when it is at the default.
    fn slice_weight(&self, slice: &str) -> Result<Option<u64>, Refusal>;
    /// Sets a slice's `CPUWeight` for this boot only, or back to the default with `None`.
    fn set_slice_weight(&self, slice: &str, weight: Option<u64>) -> Result<(), Refusal>;
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

    fn slice_weight(&self, slice: &str) -> Result<Option<u64>, Refusal> {
        let output = self.run(
            SYSTEMCTL,
            vec![
                "show".into(),
                "--property=CPUWeight".into(),
                "--".into(),
                slice.into(),
            ],
        )?;
        Ok(parse_weight(&output))
    }

    fn set_slice_weight(&self, slice: &str, weight: Option<u64>) -> Result<(), Refusal> {
        let value = weight.map(|weight| weight.to_string()).unwrap_or_default();
        self.run(
            SYSTEMCTL,
            vec![
                "set-property".into(),
                "--runtime".into(),
                "--".into(),
                slice.into(),
                format!("CPUWeight={value}").into(),
            ],
        )
        .map(|_| ())
    }
}

/// A `CPUWeight=` line of `systemctl show`; anything but a number is the default.
pub fn parse_weight(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("CPUWeight="))
        .and_then(|value| value.trim().parse().ok())
}

/// The caller's account slice, which only an unprotected raise weights (006 FR-016).
pub fn account_slice(uid: u32) -> String {
    format!("user-{uid}.slice")
}

/// Where `systemctl set-property --runtime` writes its drop-ins; cleared at boot.
pub const RUNTIME_CONTROL_DIR: &str = "/run/systemd/system.control";

/// The accounts whose slice has a runtime `CPUWeight` drop-in in `dir` set to `weight`, the
/// form a raise leaves (006 FR-018). The last assignment in the file decides, as for systemd.
pub fn raised_slices(dir: &Path, weight: u64) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let wanted = weight.to_string();
    let mut uids: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let digits = name.strip_prefix("user-")?.strip_suffix(".slice.d")?;
            let uid: u32 = digits.parse().ok()?;
            // Only the slice's own spelling; `user-01.slice` is another unit.
            if uid.to_string() != digits {
                return None;
            }
            let text = std::fs::read_to_string(entry.path().join("50-CPUWeight.conf")).ok()?;
            let last = text
                .lines()
                .rev()
                .find_map(|line| line.trim().strip_prefix("CPUWeight="))?;
            (last.trim() == wanted).then_some(uid)
        })
        .collect();
    uids.sort_unstable();
    uids
}

/// Raises, or resets, the CPU weight of the caller's account slice under unprotected (006
/// FR-016). A raise needs the administrator's `modes.unprotected` and the caller's ordinary unit
/// in that slice, where the host splits CPU by account; a reset needs neither. Either is read
/// back.
pub fn raise_account_slice(
    systemd: &dyn Systemd,
    caller: Caller,
    policy: &AdminPolicy,
    on: bool,
) -> Result<Vec<UnitStatus>, Refusal> {
    let slice = account_slice(caller.uid);
    let units = status_units(systemd, caller);
    let weight = if on {
        if !policy.unprotected.value {
            return Err(Refusal::new(
                RefusalCode::RaiseRefused,
                "policy modes.unprotected is false; ask an administrator to permit unprotected",
            ));
        }
        let prefix = format!("/user.slice/{slice}/");
        let placed = units.as_ref().is_ok_and(|units| {
            units.iter().any(|unit| {
                unit.kind == UnitKind::Ordinary
                    && unit.state == "active"
                    && unit.cgroup.starts_with(&prefix)
            })
        });
        if !placed {
            return Err(Refusal::new(
                RefusalCode::RaiseRefused,
                "your ordinary unit is not running in your account slice, so no raise reaches other accounts",
            ));
        }
        let weight = policy.unprotected_cpu_weight.value;
        // The policy's own weight is a raise this broker or an earlier one set; any other is
        // someone else's, and a reset would lose it.
        if let Some(current) = systemd.slice_weight(&slice)?
            && current != weight
        {
            return Err(Refusal::new(
                RefusalCode::RaiseRefused,
                format!(
                    "your account slice already has CPU weight {current}, which the broker did not set; ask an administrator"
                ),
            ));
        }
        Some(weight)
    } else {
        None
    };
    systemd.set_slice_weight(&slice, weight)?;
    if systemd.slice_weight(&slice)? != weight {
        return Err(failed());
    }
    Ok(units.unwrap_or_default())
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
        let mut after = systemd.show(&name)?;
        // An agent stopped by signal can exit non-zero; the stop was asked for, so the unit is
        // cleared rather than left listed as failed.
        if after.active_state == "failed" {
            systemd.reset_failed(&name)?;
            after = systemd.show(&name)?;
        }
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

/// The cgroups of the caller's two units for a move: both must carry the marker and run.
pub fn caller_units(
    systemd: &dyn Systemd,
    caller: Caller,
) -> Result<super::moves::CallerUnits, Refusal> {
    let shows = checked_shows(systemd, caller)?;
    let running = |kind: UnitKind| {
        shows
            .iter()
            .find(|(found, _, show)| {
                *found == kind
                    && !show.absent()
                    && show.active_state == "active"
                    && show.control_group.starts_with('/')
            })
            .map(|(_, _, show)| show.control_group.clone())
    };
    match (running(UnitKind::Ordinary), running(UnitKind::Idle)) {
        (Some(ordinary), Some(idle)) => Ok(super::moves::CallerUnits { ordinary, idle }),
        _ => Err(Refusal::new(
            RefusalCode::MoveRefused,
            "both of your units must be running to move a job; run cedegrid system start",
        )),
    }
}
