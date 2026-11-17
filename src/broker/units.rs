//! What the broker builds for a caller (contracts/units-and-slices.md, FR-006).
//!
//! Every name and property comes from the caller's user ID, primary group and the administrator
//! policy, plus two choices still open with the user: where the ordinary unit sits (N4) and
//! whether units open a PAM session (N3). Nothing comes from a request.

use super::admin_policy::AdminPolicy;
use super::{AGENT_SUBCOMMAND, IDLE_EXECUTOR_SUBCOMMAND, INSTALLED_BINARY};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

/// The fixed `systemd-run` path the broker calls (research R8).
pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// The fixed `systemctl` path the broker calls (research R8).
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// Root-level slice for ordinary work where N4 places it there.
pub const CEDEGRID_SLICE: &str = "cedegrid.slice";

/// Root-level slice with `cpu.idle` for Yield First work (FR-016).
pub const IDLE_SLICE: &str = "cedegrid-idle.slice";

/// Every unit the broker makes has a description starting with this.
pub const DESCRIPTION_PREFIX: &str = "CedeGrid System mode unit";

/// The environment variable that marks a unit as the broker's; read back with `systemctl show`
/// before every stop or move (FR-007).
pub const MARKER_VARIABLE: &str = "CEDEGRID_SYSTEM_UNIT";

/// The PAM service a unit opens under N3 B.
pub const PAM_SERVICE: &str = "cedegrid";

/// The two units a caller has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// Every mode's work except Yield First, and the user's agent.
    Ordinary,
    /// Yield First work, started there by the idle executor (FR-017).
    Idle,
}

/// Where the ordinary unit sits (decision N4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrdinarySlice {
    /// `user-<uid>.slice`, where the host splits CPU by account (N4 A on such hosts).
    UserSlice,
    /// The root-level `cedegrid.slice` (N4 A elsewhere, N4 B, N4 C).
    CedegridSlice,
}

/// Whether a unit opens a PAM session (decision N3), set by the policy's `units.pam_session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PamSession {
    /// No session; the process limits come from the administrator policy (N3 A, the default).
    #[serde(rename = "none")]
    None,
    /// The `cedegrid` PAM service, which the administrator provides with only `pam_limits`
    /// (N3 B).
    #[serde(rename = "cedegrid")]
    CedegridLimits,
    /// The host's `login` PAM stack (N3 C).
    #[serde(rename = "login")]
    HostLogin,
}

impl PamSession {
    pub fn name(self) -> &'static str {
        match self {
            PamSession::None => "none",
            PamSession::CedegridLimits => "cedegrid",
            PamSession::HostLogin => "login",
        }
    }
}

/// The two open choices the builder takes as inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub ordinary_slice: OrdinarySlice,
    pub pam: PamSession,
}

/// The caller, as the broker resolved it from the socket and the passwd database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub primary_gid: u32,
}

/// A unit the broker would start: its name, slice, properties and fixed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitSpec {
    pub kind: UnitKind,
    pub name: String,
    pub slice: String,
    pub description: String,
    pub uid: u32,
    pub gid: u32,
    /// `--property=` values in the order they are passed.
    pub properties: Vec<(String, String)>,
    /// The marker variable's value.
    pub marker: String,
    pub command: Vec<String>,
}

/// The name of a caller's unit.
pub fn unit_name(kind: UnitKind, uid: u32) -> String {
    match kind {
        UnitKind::Ordinary => format!("cedegrid-u{uid}.service"),
        UnitKind::Idle => format!("cedegrid-idle-u{uid}.service"),
    }
}

/// The marker value a caller's unit carries.
pub fn marker_value(kind: UnitKind, uid: u32) -> String {
    match kind {
        UnitKind::Ordinary => format!("ordinary:{uid}"),
        UnitKind::Idle => format!("idle:{uid}"),
    }
}

/// The ordinary unit's runtime directory under `/run`, where the agent's socket for the idle
/// executor lives (research R9, R18).
pub fn runtime_directory(uid: u32) -> String {
    format!("cedegrid/u{uid}")
}

/// Builds one of a caller's units. Depends only on the caller, the policy and the placement.
pub fn build(
    kind: UnitKind,
    caller: Caller,
    policy: &AdminPolicy,
    placement: Placement,
) -> UnitSpec {
    let uid = caller.uid;
    let slice = match (kind, placement.ordinary_slice) {
        (UnitKind::Idle, _) => IDLE_SLICE.to_owned(),
        (UnitKind::Ordinary, OrdinarySlice::UserSlice) => format!("user-{uid}.slice"),
        (UnitKind::Ordinary, OrdinarySlice::CedegridSlice) => CEDEGRID_SLICE.to_owned(),
    };
    let mut properties = vec![
        ("Delegate".to_owned(), "yes".to_owned()),
        ("TasksMax".to_owned(), policy.tasks_max.value.to_string()),
        ("Restart".to_owned(), "on-failure".to_owned()),
    ];
    if kind == UnitKind::Ordinary {
        properties.push(("RuntimeDirectory".to_owned(), runtime_directory(uid)));
        properties.push(("RuntimeDirectoryMode".to_owned(), "0700".to_owned()));
    }
    match placement.pam {
        PamSession::None => {
            if let Some(open_files) = policy.open_files.value {
                properties.push(("LimitNOFILE".to_owned(), open_files.to_string()));
            }
            if let Some(processes) = policy.processes.value {
                properties.push(("LimitNPROC".to_owned(), processes.to_string()));
            }
            if let Some(mib) = policy.locked_memory_mib.value {
                properties.push((
                    "LimitMEMLOCK".to_owned(),
                    mib.saturating_mul(1024 * 1024).to_string(),
                ));
            }
        }
        PamSession::CedegridLimits => {
            properties.push(("PAMName".to_owned(), PAM_SERVICE.to_owned()));
        }
        PamSession::HostLogin => {
            properties.push(("PAMName".to_owned(), "login".to_owned()));
        }
    }
    let subcommand = match kind {
        UnitKind::Ordinary => AGENT_SUBCOMMAND,
        UnitKind::Idle => IDLE_EXECUTOR_SUBCOMMAND,
    };
    let what = match kind {
        UnitKind::Ordinary => "ordinary",
        UnitKind::Idle => "idle",
    };
    UnitSpec {
        kind,
        name: unit_name(kind, uid),
        slice,
        description: format!("{DESCRIPTION_PREFIX} ({what}) for user ID {uid}"),
        uid,
        gid: caller.primary_gid,
        properties,
        marker: marker_value(kind, uid),
        command: vec![INSTALLED_BINARY.to_owned(), subcommand.to_owned()],
    }
}

/// The argument vector for `systemd-run`, without the program itself. No shell is involved.
pub fn systemd_run_args(spec: &UnitSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        format!("--unit={}", spec.name).into(),
        format!("--slice={}", spec.slice).into(),
        format!("--description={}", spec.description).into(),
        format!("--uid={}", spec.uid).into(),
        format!("--gid={}", spec.gid).into(),
        format!("--setenv={MARKER_VARIABLE}={}", spec.marker).into(),
    ];
    for (key, value) in &spec.properties {
        args.push(format!("--property={key}={value}").into());
    }
    // `--` ends systemd-run's options; the command after it is fixed.
    args.push("--".into());
    args.extend(spec.command.iter().map(OsString::from));
    args
}
