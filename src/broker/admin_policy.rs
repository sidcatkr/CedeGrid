//! The administrator policy, `/etc/cedegrid/policy.toml` (contracts/admin-policy.md, FR-010).
//!
//! The file is optional; without it the defaults apply. A file or directory that is not owned by
//! root, or that group or others can write, is refused, and the broker then serves no request.

use super::ModeName;
use super::units::{PamSession, SliceChoice};
use crate::config::SystemAccounts;
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
    unprotected: Option<bool>,
    unprotected_cpu_weight: Option<u64>,
    plain_priority: Option<bool>,
    plain_priority_cpu_weight: Option<u64>,
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
    system_accounts: Option<SystemAccounts>,
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

/// The account slice's CPU weight under unprotected when the policy names none: four times the
/// default of 100 (006 FR-016).
pub const DEFAULT_UNPROTECTED_CPU_WEIGHT: u64 = 400;

/// The account slice's CPU weight under priority over plain users when the policy names none:
/// twice the default of 100, so own work gets about two thirds of a CPU contended by one plain
/// account (006 FR-024).
pub const DEFAULT_PLAIN_PRIORITY_CPU_WEIGHT: u64 = 200;

/// The effective administrator policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminPolicy {
    pub policy_version: u32,
    pub allowed_modes: Setting<Vec<ModeName>>,
    /// Whether members may request unprotected (006 FR-015); off by default.
    pub unprotected: Setting<bool>,
    /// The CPU weight the broker gives an unprotected caller's account slice (006 FR-016).
    pub unprotected_cpu_weight: Setting<u64>,
    /// Whether members may ask for priority over plain users (006 FR-024); off by default.
    pub plain_priority: Setting<bool>,
    /// The CPU weight the broker gives the account slice under it (006 FR-024).
    pub plain_priority_cpu_weight: Setting<u64>,
    pub tasks_per_user: Setting<u32>,
    pub tasks_max: Setting<u32>,
    /// `LimitNOFILE=`; `None` keeps systemd's default.
    pub open_files: Setting<Option<u64>>,
    /// `LimitNPROC=`; `None` keeps systemd's default.
    pub processes: Setting<Option<u64>>,
    /// `LimitMEMLOCK=` in MiB; `None` keeps systemd's default.
    pub locked_memory_mib: Setting<Option<u64>>,
    pub uid_min: Setting<u32>,
    /// Which user IDs at or above `uid_min` are system accounts, and how subordinate user IDs
    /// count: 001's `[contention.system_accounts]` type and defaults (001 FR-059).
    pub system_accounts: Setting<SystemAccounts>,
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
        let system_accounts =
            Setting::pick(file.principals.system_accounts, SystemAccounts::default());
        system_accounts
            .value
            .validate("principals.system_accounts")?;
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
        let unprotected = Setting::pick(file.modes.unprotected, false);
        let unprotected_cpu_weight = Setting::pick(
            file.modes.unprotected_cpu_weight,
            DEFAULT_UNPROTECTED_CPU_WEIGHT,
        );
        ensure!(
            (101..=10_000).contains(&unprotected_cpu_weight.value),
            "modes.unprotected_cpu_weight must be between 101 and 10000"
        );
        let plain_priority = Setting::pick(file.modes.plain_priority, false);
        let plain_priority_cpu_weight = Setting::pick(
            file.modes.plain_priority_cpu_weight,
            DEFAULT_PLAIN_PRIORITY_CPU_WEIGHT,
        );
        ensure!(
            (101..=10_000).contains(&plain_priority_cpu_weight.value),
            "modes.plain_priority_cpu_weight must be between 101 and 10000"
        );
        Ok(Self {
            policy_version,
            allowed_modes,
            unprotected,
            unprotected_cpu_weight,
            plain_priority,
            plain_priority_cpu_weight,
            tasks_per_user,
            tasks_max,
            open_files,
            processes,
            locked_memory_mib,
            uid_min,
            system_accounts,
            principal_min_percent,
            pam_session,
            ordinary_slice: Setting::pick(file.units.ordinary_slice, SliceChoice::Auto),
        })
    }

    pub fn allows(&self, mode: ModeName) -> bool {
        self.allowed_modes.value.contains(&mode)
    }

    /// A user, who can be a principal: 001's one test, with `principals.uid_min` (001 FR-059).
    pub fn is_user_uid(&self, uid: u32) -> bool {
        self.system_accounts.value.is_user(self.uid_min.value, uid)
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
                "modes.unprotected",
                self.unprotected.value.to_string(),
                self.unprotected.source,
            ),
            (
                "modes.unprotected_cpu_weight",
                self.unprotected_cpu_weight.value.to_string(),
                self.unprotected_cpu_weight.source,
            ),
            (
                "modes.plain_priority",
                self.plain_priority.value.to_string(),
                self.plain_priority.source,
            ),
            (
                "modes.plain_priority_cpu_weight",
                self.plain_priority_cpu_weight.value.to_string(),
                self.plain_priority_cpu_weight.source,
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
                "principals.system_accounts.uid_ranges",
                ranges(&self.system_accounts.value.uid_ranges),
                self.system_accounts.source,
            ),
            (
                "principals.system_accounts.subordinate_uid_file",
                subordinate_file(&self.system_accounts.value),
                self.system_accounts.source,
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

/// Ranges as `doctor` shows them, such as `60578-60705, 65534-65535`.
fn ranges(ranges: &[[u32; 2]]) -> String {
    if ranges.is_empty() {
        return "none".to_owned();
    }
    ranges
        .iter()
        .map(|[first, last]| format!("{first}-{last}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn subordinate_file(accounts: &SystemAccounts) -> String {
    if accounts.subordinate_uid_file.as_os_str().is_empty() {
        "off".to_owned()
    } else {
        accounts.subordinate_uid_file.display().to_string()
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
    let mut policy = AdminPolicy::parse(&text, host_uid_min)
        .with_context(|| format!("the policy file {} was refused", path.display()))?;
    // A relative path resolves against the policy's directory, as the node's against its file.
    let subuid = &mut policy.system_accounts.value.subordinate_uid_file;
    if subuid.is_relative() && !subuid.as_os_str().is_empty() {
        *subuid = dir.join(&*subuid);
    }
    Ok(policy)
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

/// Read at most this much of the subordinate user ID file.
const MAX_SUBORDINATE_BYTES: u64 = 1024 * 1024;

/// The file's device, inode, size, and modification and status change times, to see when its
/// text may have changed.
type FileStamp = (u64, u64, u64, i64, i64, i64, i64);

/// The subordinate user ID file for the memory shares, checked each pass and read again when it
/// changes. The broker trusts it as it trusts the policy: a regular file that root owns and only
/// root may write.
/// While it is missing, refused or turned off, it maps nothing and each such user ID counts as
/// itself (001 FR-059).
#[derive(Debug, Default)]
pub struct SubordinateFile {
    seen: Option<(std::path::PathBuf, FileStamp)>,
    ids: Option<crate::telemetry::subuid::SubordinateIds>,
}

impl SubordinateFile {
    /// Checks `accounts.subordinate_uid_file` on every call and reads it again when it changed.
    /// `owner` is the user ID that must own it: root's for the broker, the test's own in tests.
    #[cfg(unix)]
    pub fn refresh(&mut self, accounts: &SystemAccounts, owner: u32) -> Result<()> {
        let path = &accounts.subordinate_uid_file;
        if path.is_relative() && !path.as_os_str().is_empty() {
            *self = Self::default();
            anyhow::bail!(
                "{} is relative; the policy loader resolves it against /etc/cedegrid",
                path.display()
            );
        }
        let seen = self
            .seen
            .as_ref()
            .filter(|(seen_path, _)| seen_path == path)
            .map(|(_, stamp)| *stamp);
        let read = if path.as_os_str().is_empty() {
            Ok(Subordinate::Missing)
        } else {
            read_subordinate_file(path, owner, seen)
        };
        match read {
            Ok(Subordinate::Unchanged) => Ok(()),
            Ok(Subordinate::Read(text, stamp)) => {
                self.ids = Some(crate::telemetry::subuid::SubordinateIds::from_text(&text));
                self.seen = Some((path.clone(), stamp));
                Ok(())
            }
            Ok(Subordinate::Missing) => {
                *self = Self::default();
                Ok(())
            }
            Err(error) => {
                *self = Self::default();
                Err(error)
            }
        }
    }

    /// The user ID a process counts as for the shares: its range's owner, else itself.
    pub fn owner(&self, uid: u32) -> u32 {
        self.ids.as_ref().map_or(uid, |ids| ids.owner(uid))
    }
}

#[cfg(unix)]
fn file_stamp(meta: &std::fs::Metadata) -> FileStamp {
    use std::os::unix::fs::MetadataExt;
    (
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

/// What a check of the subordinate user ID file found.
#[cfg(unix)]
enum Subordinate {
    Missing,
    Unchanged,
    Read(String, FileStamp),
}

/// Opens and checks the file on every call, so a change of owner or mode is seen at once even
/// when the file's times do not move; reads it only when its stamp differs from `seen`.
#[cfg(unix)]
fn read_subordinate_file(
    path: &std::path::Path,
    owner: u32,
    seen: Option<FileStamp>,
) -> Result<Subordinate> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Subordinate::Missing);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("{} could not be opened", path.display()));
        }
    };
    let meta = file
        .metadata()
        .with_context(|| format!("{} could not be read", path.display()))?;
    ensure!(
        meta.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    check_owner_and_mode(path, meta.uid(), meta.mode(), owner)?;
    let stamp = file_stamp(&meta);
    if seen == Some(stamp) {
        return Ok(Subordinate::Unchanged);
    }
    let mut text = String::new();
    (&mut file)
        .take(MAX_SUBORDINATE_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
    ensure!(
        text.len() as u64 <= MAX_SUBORDINATE_BYTES,
        "{} is larger than {MAX_SUBORDINATE_BYTES} bytes",
        path.display()
    );
    Ok(Subordinate::Read(text, stamp))
}
