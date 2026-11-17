//! The administrator policy, `/etc/cedegrid/policy.toml` (contracts/admin-policy.md, FR-010).
//!
//! The file is optional; without it the defaults apply. A file or directory that is not owned by
//! root, or that group or others can write, is refused, and the broker then serves no request.

use super::ModeName;
use super::units::{PamSession, SliceChoice};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;

/// Where the policy lives.
pub const POLICY_DIR: &str = "/etc/cedegrid";

/// The policy file's name inside `POLICY_DIR`.
pub const POLICY_FILE: &str = "policy.toml";

/// The only policy version this build reads.
pub const POLICY_VERSION: u32 = 1;

/// Read at most this much of the policy file.
const MAX_POLICY_BYTES: u64 = 64 * 1024;

/// The first ordinary user ID when `/etc/login.defs` gives none.
pub const DEFAULT_UID_MIN: u32 = 1000;

const DEFAULT_TASKS_PER_USER: u32 = 64;
const DEFAULT_TASKS_MAX: u32 = 4096;
const DEFAULT_PRINCIPAL_MIN_PERCENT: u8 = 1;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyFile {
    policy_version: Option<u32>,
    modes: ModesSection,
    limits: LimitsSection,
    principals: PrincipalsSection,
    units: UnitsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ModesSection {
    allowed: Option<Vec<ModeName>>,
    unprotected_aggressive: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LimitsSection {
    tasks_per_user: Option<u32>,
    tasks_max: Option<u32>,
    open_files: Option<u64>,
    processes: Option<u64>,
    locked_memory_mib: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct UnitsSection {
    pam_session: Option<PamSession>,
    ordinary_slice: Option<SliceChoice>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PrincipalsSection {
    uid_min: Option<u32>,
    principal_min_percent: Option<u8>,
}

/// Where a policy value came from, for `doctor` (FR-010).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    File,
    Default,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::File => "policy file",
            Source::Default => "default",
        }
    }
}

/// One effective value and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Setting<T> {
    fn pick(value: Option<T>, default: T) -> Self {
        match value {
            Some(value) => Setting {
                value,
                source: Source::File,
            },
            None => Setting {
                value: default,
                source: Source::Default,
            },
        }
    }
}

/// The effective administrator policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminPolicy {
    pub policy_version: u32,
    pub allowed_modes: Setting<Vec<ModeName>>,
    /// Whether members may request unprotected Aggressive (004 FR-015a); off by default.
    pub unprotected_aggressive: Setting<bool>,
    pub tasks_per_user: Setting<u32>,
    pub tasks_max: Setting<u32>,
    /// `LimitNOFILE=`; `None` keeps systemd's default.
    pub open_files: Setting<Option<u64>>,
    /// `LimitNPROC=`; `None` keeps systemd's default.
    pub processes: Setting<Option<u64>>,
    /// `LimitMEMLOCK=` in MiB; `None` keeps systemd's default.
    pub locked_memory_mib: Setting<Option<u64>>,
    pub uid_min: Setting<u32>,
    pub principal_min_percent: Setting<u8>,
    /// Whether the units open a PAM session (N3); `none` by default.
    pub pam_session: Setting<PamSession>,
    /// Where the ordinary unit sits (N4); `auto` by default.
    pub ordinary_slice: Setting<SliceChoice>,
}

impl AdminPolicy {
    /// The policy without a file. `uid_min` is the host's first ordinary user ID.
    pub fn defaults(uid_min: u32) -> Result<Self> {
        Self::from_file(PolicyFile::default(), uid_min)
    }

    /// Parses a policy file's text. `host_uid_min` is the default for `principals.uid_min`.
    pub fn parse(text: &str, host_uid_min: u32) -> Result<Self> {
        let file: PolicyFile =
            toml::from_str(text).context("the administrator policy could not be read")?;
        Self::from_file(file, host_uid_min)
    }

    fn from_file(file: PolicyFile, host_uid_min: u32) -> Result<Self> {
        let policy_version = file.policy_version.unwrap_or(POLICY_VERSION);
        ensure!(
            policy_version == POLICY_VERSION,
            "policy_version must be {POLICY_VERSION}; this build cannot read version {policy_version}"
        );
        let allowed_modes = Setting::pick(
            file.modes.allowed,
            vec![
                ModeName::YieldFirst,
                ModeName::Neutral,
                ModeName::Performance,
            ],
        );
        ensure!(
            !allowed_modes.value.is_empty(),
            "modes.allowed must list at least one mode"
        );
        let tasks_per_user = Setting::pick(file.limits.tasks_per_user, DEFAULT_TASKS_PER_USER);
        ensure!(
            tasks_per_user.value >= 1,
            "limits.tasks_per_user must be at least 1"
        );
        let tasks_max = Setting::pick(file.limits.tasks_max, DEFAULT_TASKS_MAX);
        ensure!(tasks_max.value >= 1, "limits.tasks_max must be at least 1");
        if let Some(open_files) = file.limits.open_files {
            ensure!(
                open_files >= 1,
                "limits.open_files must be at least 1; leave it out for systemd's default"
            );
        }
        let open_files = Setting {
            value: file.limits.open_files,
            source: source_of(file.limits.open_files.is_some()),
        };
        let processes = Setting {
            value: file.limits.processes.filter(|&value| value != 0),
            source: source_of(file.limits.processes.is_some()),
        };
        let locked_memory_mib = Setting {
            value: file.limits.locked_memory_mib.filter(|&value| value != 0),
            source: source_of(file.limits.locked_memory_mib.is_some()),
        };
        let uid_min = Setting::pick(file.principals.uid_min, host_uid_min);
        ensure!(uid_min.value >= 1, "principals.uid_min must be at least 1");
        let principal_min_percent = Setting::pick(
            file.principals.principal_min_percent,
            DEFAULT_PRINCIPAL_MIN_PERCENT,
        );
        ensure!(
            (1..=50).contains(&principal_min_percent.value),
            "principals.principal_min_percent must be between 1 and 50"
        );
        let pam_session = Setting::pick(file.units.pam_session, PamSession::None);
        let unprotected_aggressive = Setting::pick(file.modes.unprotected_aggressive, false);
        Ok(Self {
            policy_version,
            allowed_modes,
            unprotected_aggressive,
            tasks_per_user,
            tasks_max,
            open_files,
            processes,
            locked_memory_mib,
            uid_min,
            principal_min_percent,
            pam_session,
            ordinary_slice: Setting::pick(file.units.ordinary_slice, SliceChoice::Auto),
        })
    }

    pub fn allows(&self, mode: ModeName) -> bool {
        self.allowed_modes.value.contains(&mode)
    }

    /// Each effective value as `doctor` shows it: key, value and where it came from.
    pub fn entries(&self) -> Vec<(&'static str, String, Source)> {
        let limit = |value: Option<u64>| match value {
            Some(value) => value.to_string(),
            None => "systemd default".to_owned(),
        };
        vec![
            (
                "modes.allowed",
                self.allowed_modes
                    .value
                    .iter()
                    .map(|mode| mode.name())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.allowed_modes.source,
            ),
            (
                "modes.unprotected_aggressive",
                self.unprotected_aggressive.value.to_string(),
                self.unprotected_aggressive.source,
            ),
            (
                "limits.tasks_per_user",
                self.tasks_per_user.value.to_string(),
                self.tasks_per_user.source,
            ),
            (
                "limits.tasks_max",
                self.tasks_max.value.to_string(),
                self.tasks_max.source,
            ),
            (
                "limits.open_files",
                limit(self.open_files.value),
                self.open_files.source,
            ),
            (
                "limits.processes",
                limit(self.processes.value),
                self.processes.source,
            ),
            (
                "limits.locked_memory_mib",
                limit(self.locked_memory_mib.value),
                self.locked_memory_mib.source,
            ),
            (
                "principals.uid_min",
                self.uid_min.value.to_string(),
                self.uid_min.source,
            ),
            (
                "principals.principal_min_percent",
                self.principal_min_percent.value.to_string(),
                self.principal_min_percent.source,
            ),
            (
                "units.pam_session",
                self.pam_session.value.name().to_owned(),
                self.pam_session.source,
            ),
            (
                "units.ordinary_slice",
                self.ordinary_slice.value.name().to_owned(),
                self.ordinary_slice.source,
            ),
        ]
    }
}

fn source_of(in_file: bool) -> Source {
    if in_file {
        Source::File
    } else {
        Source::Default
    }
}

/// `UID_MIN` from the text of `/etc/login.defs`, if it sets one.
pub fn uid_min_from_login_defs(text: &str) -> Option<u32> {
    // The last setting wins, as in login.defs readers.
    text.lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("UID_MIN"))
                .then(|| words.next())
                .flatten()
        })
        .find_map(|value| value.parse::<u32>().ok())
}

/// The host's first ordinary user ID: `UID_MIN` from `/etc/login.defs`, else 1000.
pub fn host_uid_min() -> u32 {
    std::fs::read_to_string("/etc/login.defs")
        .ok()
        .and_then(|text| uid_min_from_login_defs(&text))
        .unwrap_or(DEFAULT_UID_MIN)
}

/// Loads the policy from `POLICY_DIR`, which root must own.
#[cfg(unix)]
pub fn load() -> Result<AdminPolicy> {
    load_from(std::path::Path::new(POLICY_DIR), 0, host_uid_min())
}

/// Loads the policy from `dir`, which `owner` must own and only `owner` may write.
///
/// The broker passes root's user ID; tests pass their own to reach the same checks.
#[cfg(unix)]
pub fn load_from(dir: &std::path::Path, owner: u32, host_uid_min: u32) -> Result<AdminPolicy> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let dir_meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("the policy directory {} could not be read", dir.display()))?;
    ensure!(
        dir_meta.file_type().is_dir(),
        "the policy directory {} is not a plain directory",
        dir.display()
    );
    check_owner_and_mode(dir, dir_meta.uid(), dir_meta.mode(), owner)?;

    let path = dir.join(POLICY_FILE);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return AdminPolicy::defaults(host_uid_min);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("the policy file {} could not be opened", path.display())
            });
        }
    };
    // The checks read the opened file, so a swap after them cannot change what is loaded.
    let meta = file
        .metadata()
        .with_context(|| format!("the policy file {} could not be read", path.display()))?;
    ensure!(
        meta.file_type().is_file(),
        "the policy file {} is not a regular file",
        path.display()
    );
    check_owner_and_mode(&path, meta.uid(), meta.mode(), owner)?;
    let mut text = String::new();
    (&mut file)
        .take(MAX_POLICY_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("the policy file {} is not UTF-8 text", path.display()))?;
    ensure!(
        text.len() as u64 <= MAX_POLICY_BYTES,
        "the policy file {} is larger than {MAX_POLICY_BYTES} bytes",
        path.display()
    );
    AdminPolicy::parse(&text, host_uid_min)
        .with_context(|| format!("the policy file {} was refused", path.display()))
}

#[cfg(unix)]
fn check_owner_and_mode(path: &std::path::Path, uid: u32, mode: u32, owner: u32) -> Result<()> {
    if uid != owner {
        anyhow::bail!(
            "{} must be owned by user ID {owner}, not {uid}",
            path.display()
        );
    }
    if mode & 0o022 != 0 {
        anyhow::bail!(
            "{} must not be writable by group or others (mode {:o})",
            path.display(),
            mode & 0o7777
        );
    }
    Ok(())
}
