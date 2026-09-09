//! Optional, explicitly authorized cgroup v2 controls.
//!
//! The coordinator and policy remain in user space. This backend only creates
//! private workload leaves under an already delegated, configured subtree. It
//! never enables controllers, changes ancestors, moves existing processes, or
//! treats RAM controls as GPU memory enforcement. Linux runtime behavior must be
//! verified on an authorized Linux host; fixture tests are not runtime evidence.
use crate::execution_model::{
    ControlEvidence, GateSetup, LaunchBackend, LaunchRequest, ProcessIdentity,
};
#[cfg(any(target_os = "linux", test))]
use anyhow::Context;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

const MIB: u64 = 1024 * 1024;
const WRITABLE_CONTROLS: &[&str] = &[
    "cgroup.procs",
    "cpu.weight",
    "cpu.max",
    "memory.high",
    "memory.max",
    "cgroup.kill",
];

/// CPU bandwidth is a quota per period, unlike the relative `cpu.weight`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuMax {
    pub quota_us: u64,
    pub period_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CgroupConfig {
    pub enabled: bool,
    pub delegated_root: Option<PathBuf>,
    /// Exact control-file names the operator permits this backend to write.
    /// `cgroup.procs` authorizes the gate to join its own new workload leaf.
    pub authorized_controls: Vec<String>,
    /// Relative hierarchical weight; neither a CPU percentage nor a guarantee.
    pub cpu_weight: Option<u16>,
    /// No bandwidth limit is introduced unless this is explicitly configured.
    pub cpu_max: Option<CpuMax>,
    pub memory_high_mib: Option<u64>,
    pub memory_max_mib: Option<u64>,
    /// Permission is necessary but not sufficient: every member must also be
    /// proven managed before destructive subtree cleanup can be implemented.
    pub allow_kill: bool,
}

impl Default for CgroupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            delegated_root: None,
            authorized_controls: vec![],
            cpu_weight: Some(10),
            cpu_max: None,
            memory_high_mib: None,
            memory_max_mib: None,
            allow_kill: false,
        }
    }
}

impl CgroupConfig {
    /// Pure validation: this never creates a directory or writes a kernel file.
    pub fn validate(&self) -> Result<()> {
        if let Some(root) = &self.delegated_root {
            validate_absolute_path(root)?;
        }
        let mut seen = BTreeSet::new();
        for control in &self.authorized_controls {
            ensure!(
                WRITABLE_CONTROLS.contains(&control.as_str()),
                "unsupported authorized cgroup control: {control}"
            );
            ensure!(
                seen.insert(control),
                "duplicate authorized cgroup control: {control}"
            );
        }
        if let Some(weight) = self.cpu_weight {
            ensure!(
                (1..=10_000).contains(&weight),
                "cpu.weight must be in 1..=10000"
            );
        }
        if let Some(max) = &self.cpu_max {
            ensure!(
                max.quota_us >= 1_000,
                "cpu.max quota_us must be at least 1000"
            );
            ensure!(
                (1_000..=1_000_000).contains(&max.period_us),
                "cpu.max period_us must be in 1000..=1000000"
            );
        }
        for (name, value) in [
            ("memory.high", self.memory_high_mib),
            ("memory.max", self.memory_max_mib),
        ] {
            if let Some(value) = value {
                ensure!(
                    value > 0 && value.checked_mul(MIB).is_some(),
                    "{name} must be a positive, representable MiB value"
                );
            }
        }
        if let (Some(high), Some(max)) = (self.memory_high_mib, self.memory_max_mib) {
            ensure!(high <= max, "memory.high must not exceed memory.max");
        }
        if self.enabled {
            ensure!(
                self.delegated_root.is_some(),
                "enabled cgroup backend requires an explicitly authorized delegated_root"
            );
            self.require_authorized("cgroup.procs")?;
            for setting in self.settings() {
                self.require_authorized(setting.0)?;
            }
            if self.allow_kill {
                self.require_authorized("cgroup.kill")?;
            }
        }
        Ok(())
    }

    fn require_authorized(&self, control: &str) -> Result<()> {
        ensure!(
            self.authorized_controls.iter().any(|c| c == control),
            "configured cgroup control is not explicitly authorized: {control}"
        );
        Ok(())
    }

    fn settings(&self) -> Vec<(&'static str, String)> {
        let mut settings = Vec::new();
        if let Some(weight) = self.cpu_weight {
            settings.push(("cpu.weight", weight.to_string()));
        }
        if let Some(max) = &self.cpu_max {
            settings.push(("cpu.max", format!("{} {}", max.quota_us, max.period_us)));
        }
        if let Some(value) = self.memory_high_mib {
            settings.push(("memory.high", (value.saturating_mul(MIB)).to_string()));
        }
        if let Some(value) = self.memory_max_mib {
            settings.push(("memory.max", (value.saturating_mul(MIB)).to_string()));
        }
        settings
    }
}

fn validate_absolute_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path != Path::new("/"),
        "delegated_root must be an explicit absolute subtree path"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "delegated_root must not contain traversal components"
    );
    // Path::components normalizes interior '.', so reject it in the original text.
    ensure!(
        !path
            .to_string_lossy()
            .split('/')
            .any(|part| part == "." || part == ".."),
        "delegated_root must not contain traversal components"
    );
    Ok(())
}

/// Readback uses the canonical kernel representation, not string substrings.
#[cfg(any(target_os = "linux", test))]
fn canonical_control(name: &str, value: &str) -> Result<String> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    match name {
        "cpu.weight" => {
            ensure!(fields.len() == 1, "invalid cpu.weight readback");
            Ok(fields[0].parse::<u16>()?.to_string())
        }
        "memory.high" | "memory.max" => {
            ensure!(fields.len() == 1, "invalid {name} readback");
            if fields[0] == "max" {
                Ok("max".into())
            } else {
                Ok(fields[0].parse::<u64>()?.to_string())
            }
        }
        "cpu.max" => {
            ensure!(fields.len() == 2, "invalid cpu.max readback");
            let quota = if fields[0] == "max" {
                "max".into()
            } else {
                fields[0].parse::<u64>()?.to_string()
            };
            Ok(format!("{quota} {}", fields[1].parse::<u64>()?))
        }
        _ => bail!("unsupported cgroup control readback: {name}"),
    }
}

/// A value with explicit scope and availability. Missing files never mean zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupReading {
    pub observed_at_unix_ms: Option<u64>,
    pub scope: PathBuf,
    pub control: String,
    pub value: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct CgroupBackend {
    config: CgroupConfig,
    evidence: Vec<ControlEvidence>,
    assignment: Option<String>,
    identity: Option<ProcessIdentity>,
    #[cfg(target_os = "linux")]
    runtime: linux::Runtime,
}

impl CgroupBackend {
    /// Opens and verifies existing directories only. All writes are deferred to
    /// prepare(), after explicit authorization and job-control validation.
    pub fn new(config: CgroupConfig) -> Result<Self> {
        config.validate()?;
        ensure!(
            config.enabled,
            "cgroup backend is disabled; use the rootless baseline"
        );
        #[cfg(target_os = "linux")]
        {
            let runtime = linux::Runtime::open(&config)?;
            Ok(Self {
                config,
                runtime,
                evidence: vec![],
                assignment: None,
                identity: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!(
                "cgroup v2 execution is unavailable on this platform; Linux runtime behavior is unverified here"
            )
        }
    }

    pub fn control_evidence(&self) -> &[ControlEvidence] {
        &self.evidence
    }

    /// Visible ancestor restrictions and leaf accounting. These observations do
    /// not prove protection from competitors in other hierarchy placements.
    pub fn accounting(&self) -> Vec<CgroupReading> {
        #[cfg(target_os = "linux")]
        {
            self.runtime.accounting()
        }
        #[cfg(not(target_os = "linux"))]
        {
            vec![]
        }
    }

    fn check_required(&self, request: &LaunchRequest) -> Result<()> {
        check_required_controls(&self.config, &request.required_controls)
    }
}

fn check_required_controls(config: &CgroupConfig, required: &[String]) -> Result<()> {
    let settings = config.settings();
    for control in required {
        ensure!(
            control == "cgroup.procs"
                || control == "process_handle"
                || control == "cpu.nice"
                || settings.iter().any(|s| s.0 == control),
            "required control is not configured by cgroup backend: {control}"
        );
    }
    // process_handle and requested cpu.nice are verified by the owning supervisor;
    // accepting the requirement here never supplies backend enforcement evidence.
    Ok(())
}

impl LaunchBackend for CgroupBackend {
    fn name(&self) -> &str {
        "cgroup_v2"
    }

    fn preparation_evidence(&self) -> Vec<ControlEvidence> {
        self.evidence.clone()
    }

    fn prepare(&mut self, request: &LaunchRequest) -> Result<GateSetup> {
        self.check_required(request)?;
        ensure!(
            self.assignment.is_none(),
            "cgroup backend already owns an allocation; reconcile it before reuse"
        );
        #[cfg(target_os = "linux")]
        {
            self.runtime.create_leaf()?;
            self.assignment = Some(request.assignment_id.clone());
            let path = self.runtime.leaf_path()?.to_owned();
            self.evidence.clear();
            self.evidence.push(ControlEvidence {
                control: "cgroup.procs".into(), available: Some(true), permitted: None,
                configured: true, applied: false, fallback: false,
                scope: path.display().to_string(), requested: Some("gate self-join before user exec".into()),
                effective: None, detail: "Owned leaf prepared; membership has not yet been verified. User execution remains forbidden.".into(),
            });
            apply_settings(&self.runtime, &self.config, &path, &mut self.evidence)?;
            Ok(GateSetup {
                cgroup_path: Some(path),
                nice: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }

    fn verify(&mut self, identity: &ProcessIdentity) -> Result<Vec<ControlEvidence>> {
        ensure!(
            self.assignment.as_deref() == Some(&identity.assignment_id),
            "cgroup assignment identity mismatch"
        );
        if let Some(previous) = &self.identity {
            ensure!(
                previous == identity,
                "cgroup process or attempt identity changed"
            );
        }
        #[cfg(target_os = "linux")]
        {
            self.runtime.verify_membership(identity)?;
            verify_settings(&self.runtime, &self.config)?;
            self.identity = Some(identity.clone());
            let scope = self.runtime.leaf_path()?.display().to_string();
            self.evidence.retain(|e| e.control != "cgroup.procs");
            self.evidence.push(ControlEvidence { control: "cgroup.procs".into(), available: Some(true), permitted: Some(true), configured: true, applied: true, fallback: false, scope: scope.clone(), requested: Some("gate self-join before user exec".into()), effective: Some(format!("pid {} in exact workload leaf", identity.pid)), detail: "Boot/start identity and sole pre-exec membership verified. This is not proof of ownership of future descendants.".into() });
            if self.config.allow_kill {
                self.evidence.retain(|e| e.control != "cgroup.kill");
                self.evidence.push(ControlEvidence { control: "cgroup.kill".into(), available: self.runtime.has_leaf_control("cgroup.kill"), permitted: None, configured: true, applied: false, fallback: true, scope: scope.clone(), requested: Some("authorized subtree cleanup".into()), effective: None, detail: "Subtree killing is not implemented without a complete verified member registry. Nonempty cleanup requires reconciliation; authorization alone is insufficient.".into() });
            }
            self.evidence.retain(|e| e.control != "cgroup.hierarchy");
            self.evidence.push(ControlEvidence { control: "cgroup.hierarchy".into(), available: Some(true), permitted: None, configured: false, applied: false, fallback: false, scope, requested: None, effective: Some(serde_json::to_string(&self.accounting())?), detail: "Read-only leaf and visible ancestor observations. Hidden ancestors and competitor placement require deployment-level comparison records; weight is relative within this hierarchy.".into() });
            Ok(self.evidence.clone())
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }

    fn confirm_release(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.runtime.remove_empty_leaf()?;
            self.assignment = None;
            self.identity = None;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("cgroup v2 execution requires Linux")
        }
    }
}

// A private test seam exercises the exact production preparation algorithm.
#[cfg(any(target_os = "linux", test))]
trait ControlIo {
    fn assert_empty(&self) -> Result<()>;
    fn read(&self, control: &str) -> Result<String>;
    fn write(&self, control: &str, value: &str) -> Result<()>;
}

#[cfg(any(target_os = "linux", test))]
fn apply_settings(
    io: &impl ControlIo,
    config: &CgroupConfig,
    path: &Path,
    evidence: &mut Vec<ControlEvidence>,
) -> Result<()> {
    io.assert_empty()?;
    for (control, requested) in config.settings() {
        let mut entry = ControlEvidence {
            control: control.into(),
            available: None,
            permitted: None,
            configured: true,
            applied: false,
            fallback: false,
            scope: path.display().to_string(),
            requested: Some(requested.clone()),
            effective: None,
            detail: String::new(),
        };
        let result: Result<()> = (|| {
            io.assert_empty()?;
            if let Err(error) = io.read(control) {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    entry.available = Some(false);
                }
                return Err(error).context("control must exist before write");
            }
            entry.available = Some(true);
            if let Err(error) = io.write(control, &requested) {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    entry.permitted = Some(false);
                }
                return Err(error);
            }
            entry.permitted = Some(true);
            let actual = canonical_control(control, &io.read(control)?)?;
            entry.effective = Some(actual.clone());
            ensure!(
                actual == canonical_control(control, &requested)?,
                "{control} readback differs from requested setting"
            );
            entry.applied = true;
            entry.detail = match control {
                "cpu.weight" => "Relative hierarchical weight, not a percentage or CPU guarantee; effect depends on ancestors and competitor placement.",
                "cpu.max" => "Explicit CPU bandwidth ceiling; ancestor restrictions still apply.",
                "memory.high" => "Explicit RAM reclaim/throttling threshold; not a hard ceiling or GPU VRAM enforcement.",
                "memory.max" => "Explicit RAM hard limit with possible OOM consequences; not GPU VRAM enforcement.",
                _ => "Verified before workload execution.",
            }.into();
            Ok(())
        })();
        if let Err(error) = result {
            entry.detail =
                format!("Preparation failed; user execution remains forbidden: {error:#}");
            evidence.push(entry);
            return Err(error).with_context(|| format!("preparing {control}"));
        }
        evidence.push(entry);
    }
    io.assert_empty()?;
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn verify_settings(io: &impl ControlIo, config: &CgroupConfig) -> Result<()> {
    for (control, requested) in config.settings() {
        ensure!(
            canonical_control(control, &io.read(control)?)?
                == canonical_control(control, &requested)?,
            "{control} changed before launch authorization"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        ffi::CString,
        fs::{self, File},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    #[derive(Debug)]
    struct Directory {
        file: File,
    }

    impl Directory {
        fn from_fd(fd: libc::c_int) -> Result<Self> {
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: a successful open/openat returns a new owned descriptor.
            Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
            })
        }

        fn open_absolute(path: &Path) -> Result<Self> {
            let root = CString::new("/")?;
            // SAFETY: valid C string; flags request only a directory descriptor.
            let mut current = Self::from_fd(unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            for part in path.components() {
                match part {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        current = current.open_child(name.as_bytes())?;
                    }
                    _ => bail!("unsafe directory path component"),
                }
            }
            Ok(current)
        }

        fn open_child(&self, name: &[u8]) -> Result<Self> {
            ensure!(
                !name.contains(&b'/'),
                "child name must be one path component"
            );
            let name = CString::new(name)?;
            // SAFETY: retained directory fd and valid single-component name.
            Self::from_fd(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })
        }

        fn stat(&self) -> Result<libc::stat> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: valid descriptor and output pointer; initialized on success.
            if unsafe { libc::fstat(self.file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fstat succeeded.
            Ok(unsafe { stat.assume_init() })
        }

        fn same(&self, other: &Self) -> Result<bool> {
            let a = self.stat()?;
            let b = other.stat()?;
            Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
        }

        fn verify_cgroup2(&self) -> Result<()> {
            let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: valid descriptor and output pointer; initialized on success.
            if unsafe { libc::fstatfs(self.file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fstatfs succeeded.
            ensure!(
                unsafe { stat.assume_init() }.f_type == 0x6367_7270,
                "delegated_root is not on a real cgroup v2 filesystem"
            );
            Ok(())
        }

        fn control(&self, name: &str, write: bool) -> Result<File> {
            ensure!(
                !name.contains('/') && !name.contains('\0') && name != "." && name != "..",
                "invalid control file name"
            );
            let name = CString::new(name)?;
            let flags = (if write {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            }) | libc::O_NOFOLLOW
                | libc::O_CLOEXEC;
            // SAFETY: valid retained fd; no O_CREAT, path traversal or symlinks.
            let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: openat returned a newly owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        fn read(&self, name: &str) -> Result<String> {
            let mut output = String::new();
            self.control(name, false)?
                .take(1_048_577)
                .read_to_string(&mut output)?;
            ensure!(
                output.len() <= 1_048_576,
                "cgroup file exceeds bounded read size"
            );
            Ok(output)
        }

        fn empty(&self) -> Result<()> {
            ensure!(
                self.read("cgroup.procs")?.trim().is_empty(),
                "workload cgroup contains live processes; capacity remains reserved"
            );
            let events = self.read("cgroup.events")?;
            ensure!(
                events
                    .lines()
                    .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["populated", "0"]),
                "cgroup populated state is unknown or nonempty; capacity remains reserved"
            );
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Leaf {
        directory: Directory,
        name: CString,
        path: PathBuf,
        membership: PathBuf,
    }

    #[derive(Debug)]
    pub(super) struct Runtime {
        root: Directory,
        root_path: PathBuf,
        mountpoint: PathBuf,
        membership_root: PathBuf,
        leaf: Option<Leaf>,
        unreconciled_leaf: Option<PathBuf>,
    }

    impl Runtime {
        pub(super) fn open(config: &CgroupConfig) -> Result<Self> {
            let root_path = config
                .delegated_root
                .as_ref()
                .context("missing delegated root")?
                .clone();
            let root = Directory::open_absolute(&root_path)
                .context("open explicitly authorized subtree without symlinks")?;
            root.verify_cgroup2()?;
            ensure!(
                root.read("cgroup.type")?.trim() == "domain",
                "delegated subtree must be a valid domain cgroup; no hierarchy rearrangement is performed"
            );
            ensure!(
                root.read("cgroup.procs")?.trim().is_empty(),
                "delegated root contains processes; backend will not relocate them"
            );
            let enabled = root.read("cgroup.subtree_control")?;
            let available = root.read("cgroup.controllers")?;
            for (control, _) in config.settings() {
                let controller = control.split('.').next().context("invalid controller")?;
                ensure!(
                    enabled.split_whitespace().any(|c| c == controller)
                        && available.split_whitespace().any(|c| c == controller),
                    "{controller} must already be delegated and enabled for child cgroups; backend will not write subtree_control"
                );
            }
            let (mountpoint, mountroot) =
                find_mount(&fs::read_to_string("/proc/self/mountinfo")?, &root_path)?;
            let membership_root = mountroot.join(root_path.strip_prefix(&mountpoint)?);
            Ok(Self {
                root,
                root_path,
                mountpoint,
                membership_root,
                leaf: None,
                unreconciled_leaf: None,
            })
        }

        fn verify_paths(&self) -> Result<()> {
            ensure!(
                self.root
                    .same(&Directory::open_absolute(&self.root_path)?)?,
                "delegated root was replaced; refuse mutations"
            );
            if let Some(leaf) = &self.leaf {
                ensure!(
                    leaf.directory
                        .same(&self.root.open_child(leaf.name.as_bytes())?)?,
                    "owned workload leaf was replaced; refuse mutations"
                );
            }
            Ok(())
        }

        pub(super) fn create_leaf(&mut self) -> Result<()> {
            ensure!(
                self.leaf.is_none() && self.unreconciled_leaf.is_none(),
                "existing owned cgroup requires reconciliation"
            );
            self.verify_paths()?;
            ensure!(
                self.root.read("cgroup.procs")?.trim().is_empty(),
                "delegated root acquired processes; backend will not relocate them"
            );
            let mut random = [0_u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let name = format!(
                "cedegrid-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let c_name = CString::new(name.clone())?;
            // SAFETY: retained verified root fd, fresh random component; exclusive
            // mkdir never adopts an existing leaf, even for the same assignment.
            if unsafe { libc::mkdirat(self.root.file.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.unreconciled_leaf = Some(self.root_path.join(&name));
            let opened = self.root.open_child(name.as_bytes());
            let directory = match opened {
                Ok(directory) => directory,
                // We cannot verify ownership through a descriptor after open
                // failed, so do not attempt a guessed path-based deletion.
                Err(error) => return Err(error).context(format!("created leaf {} but could not pin its identity; manual reconciliation required", self.root_path.join(&name).display())),
            };
            self.leaf = Some(Leaf {
                directory,
                name: c_name,
                path: self.root_path.join(&name),
                membership: self.membership_root.join(name),
            });
            self.unreconciled_leaf = None;
            let leaf = self.leaf.as_ref().context("leaf not created")?;
            leaf.directory.verify_cgroup2()?;
            ensure!(
                leaf.directory.read("cgroup.type")?.trim() == "domain",
                "new workload cgroup has invalid hierarchy type"
            );
            leaf.directory.empty()?;
            self.verify_paths()?;
            Ok(())
        }

        pub(super) fn leaf_path(&self) -> Result<&Path> {
            Ok(&self.leaf.as_ref().context("no prepared cgroup")?.path)
        }
        pub(super) fn read_leaf(&self, control: &str) -> Result<String> {
            self.leaf
                .as_ref()
                .context("no prepared cgroup")?
                .directory
                .read(control)
        }
        pub(super) fn has_leaf_control(&self, control: &str) -> Option<bool> {
            match self.leaf.as_ref()?.directory.control(control, false) {
                Ok(_) => Some(true),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Some(false)
                }
                Err(_) => None,
            }
        }

        pub(super) fn verify_membership(&self, identity: &ProcessIdentity) -> Result<()> {
            self.verify_paths()?;
            let leaf = self.leaf.as_ref().context("no prepared cgroup")?;
            ensure!(
                fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() == identity.boot_id,
                "process boot identity changed"
            );
            let stat = fs::read_to_string(format!("/proc/{}/stat", identity.pid))?;
            let fields = stat
                .rsplit_once(')')
                .context("invalid proc stat")?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            ensure!(
                fields
                    .get(19)
                    .context("proc stat lacks starttime")?
                    .parse::<u64>()?
                    == identity.start_time,
                "process start identity changed"
            );
            let membership = fs::read_to_string(format!("/proc/{}/cgroup", identity.pid))?;
            let paths = membership
                .lines()
                .filter_map(|line| line.strip_prefix("0::"))
                .collect::<Vec<_>>();
            ensure!(
                paths.len() == 1 && Path::new(paths[0]) == leaf.membership,
                "gate is not in the exact prepared cgroup"
            );
            let pids = leaf
                .directory
                .read("cgroup.procs")?
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<std::result::Result<BTreeSet<_>, _>>()?;
            ensure!(
                pids == BTreeSet::from([identity.pid]),
                "unfamiliar members in pre-execution cgroup; refuse launch"
            );
            Ok(())
        }

        pub(super) fn remove_empty_leaf(&mut self) -> Result<()> {
            ensure!(
                self.unreconciled_leaf.is_none(),
                "created leaf identity could not be pinned; manual reconciliation required before release"
            );
            let Some(leaf) = &self.leaf else {
                return Ok(());
            };
            self.verify_paths()?;
            leaf.directory.empty()?;
            // SAFETY: retained verified parent fd, exact freshly owned leaf.
            // Kernel rmdir atomically refuses a populated group or child groups.
            if unsafe {
                libc::unlinkat(
                    self.root.file.as_raw_fd(),
                    leaf.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.leaf = None;
            Ok(())
        }

        pub(super) fn accounting(&self) -> Vec<CgroupReading> {
            let mut readings = Vec::new();
            let observed_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok());
            let mut scope = self
                .leaf
                .as_ref()
                .map(|leaf| leaf.path.as_path())
                .unwrap_or(&self.root_path);
            while scope.starts_with(&self.mountpoint) {
                let directory = Directory::open_absolute(scope);
                for control in [
                    "cpu.weight",
                    "cpu.max",
                    "cpuset.cpus.effective",
                    "cpuset.mems.effective",
                    "cpu.stat",
                    "memory.current",
                    "memory.high",
                    "memory.max",
                    "memory.events",
                    "cgroup.events",
                ] {
                    let value = directory
                        .as_ref()
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|dir| dir.read(control).map_err(|e| format!("{e:#}")));
                    readings.push(CgroupReading {
                        observed_at_unix_ms,
                        scope: scope.to_owned(),
                        control: control.into(),
                        value: value.as_ref().ok().cloned(),
                        error: value.err(),
                    });
                }
                if scope == self.mountpoint {
                    break;
                }
                let Some(parent) = scope.parent() else {
                    break;
                };
                scope = parent;
            }
            readings
        }
    }

    impl ControlIo for Runtime {
        fn assert_empty(&self) -> Result<()> {
            self.verify_paths()?;
            self.leaf
                .as_ref()
                .context("no prepared leaf")?
                .directory
                .empty()
        }
        fn read(&self, control: &str) -> Result<String> {
            self.read_leaf(control)
        }
        fn write(&self, control: &str, value: &str) -> Result<()> {
            self.assert_empty()?;
            let directory = &self.leaf.as_ref().context("no prepared leaf")?.directory;
            directory
                .control(control, true)?
                .write_all(value.as_bytes())?;
            Ok(())
        }
    }

    fn unescape_mount(value: &str) -> Result<PathBuf> {
        let mut bytes = Vec::new();
        let input = value.as_bytes();
        let mut offset = 0;
        while offset < input.len() {
            if input[offset] == b'\\' {
                ensure!(offset + 3 < input.len(), "invalid mountinfo escape");
                let octal = &input[offset + 1..offset + 4];
                ensure!(
                    octal.iter().all(|b| (b'0'..=b'7').contains(b)),
                    "invalid mountinfo octal escape"
                );
                let value = (u16::from(octal[0] - b'0') * 64)
                    + (u16::from(octal[1] - b'0') * 8)
                    + u16::from(octal[2] - b'0');
                ensure!(value > 0 && value <= 255, "invalid mountinfo byte");
                bytes.push(value as u8);
                offset += 4;
            } else {
                bytes.push(input[offset]);
                offset += 1;
            }
        }
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }

    fn find_mount(mountinfo: &str, delegated: &Path) -> Result<(PathBuf, PathBuf)> {
        let mut candidates = Vec::new();
        for line in mountinfo.lines() {
            let Some((before, after)) = line.split_once(" - ") else {
                continue;
            };
            if after.split_whitespace().next() != Some("cgroup2") {
                continue;
            }
            let fields: Vec<_> = before.split_whitespace().collect();
            if fields.len() < 6 {
                continue;
            }
            let mountpoint = unescape_mount(fields[4])?;
            let root = unescape_mount(fields[3])?;
            if delegated.starts_with(&mountpoint) {
                candidates.push((mountpoint, root));
            }
        }
        candidates
            .into_iter()
            .max_by_key(|(path, _)| path.as_os_str().len())
            .context("delegated cgroup2 mount not visible in current mount namespace")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn mount_resolution_preserves_bind_root_and_escapes() {
            let (mount, root) = find_mount("1 2 0:3 /tenant /sys/fs/cgroup rw - cgroup2 none rw\n2 3 0:3 /nested /tmp/cg\\040space rw - cgroup2 none rw", Path::new("/tmp/cg space/authorized")).unwrap();
            assert_eq!(mount, Path::new("/tmp/cg space"));
            assert_eq!(root, Path::new("/nested"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };
    #[derive(Default)]
    struct FakeIo {
        values: RefCell<BTreeMap<String, String>>,
        writes: RefCell<Vec<String>>,
        deny: Option<String>,
        corrupt: Option<String>,
        nonempty_after: Option<usize>,
        checks: Cell<usize>,
    }
    impl ControlIo for FakeIo {
        fn assert_empty(&self) -> Result<()> {
            let checks = self.checks.get();
            self.checks.set(checks + 1);
            ensure!(
                self.nonempty_after.is_none_or(|limit| checks < limit),
                "unfamiliar process appeared"
            );
            Ok(())
        }
        fn read(&self, control: &str) -> Result<String> {
            self.values
                .borrow()
                .get(control)
                .cloned()
                .context("control unavailable")
        }
        fn write(&self, control: &str, value: &str) -> Result<()> {
            if self.deny.as_deref() == Some(control) {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
            }
            self.writes.borrow_mut().push(control.into());
            self.values.borrow_mut().insert(
                control.into(),
                if self.corrupt.as_deref() == Some(control) {
                    "999".into()
                } else {
                    value.into()
                },
            );
            Ok(())
        }
    }
    fn configured() -> CgroupConfig {
        CgroupConfig {
            memory_max_mib: Some(64),
            ..CgroupConfig::default()
        }
    }
    fn fake() -> FakeIo {
        FakeIo {
            values: RefCell::new(BTreeMap::from([
                ("cpu.weight".into(), "100".into()),
                ("memory.max".into(), "max".into()),
            ])),
            ..FakeIo::default()
        }
    }
    #[test]
    fn partial_permission_failure_preserves_applied_evidence() {
        let io = FakeIo {
            deny: Some("memory.max".into()),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(
            apply_settings(
                &io,
                &configured(),
                Path::new("/authorized/owned"),
                &mut evidence
            )
            .is_err()
        );
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
        assert!(evidence[0].applied);
        assert!(!evidence[1].applied);
        assert_eq!(evidence[1].permitted, Some(false));
    }
    #[test]
    fn unavailable_control_is_not_reported_applied() {
        let io = FakeIo::default();
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert!(io.writes.borrow().is_empty());
        assert!(!evidence[0].applied);
    }
    #[test]
    fn readback_mismatch_blocks_execution() {
        let io = FakeIo {
            corrupt: Some("cpu.weight".into()),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert_eq!(evidence[0].effective.as_deref(), Some("999"));
        assert!(!evidence[0].applied);
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
    }
    #[test]
    fn appeared_process_prevents_further_writes() {
        let io = FakeIo {
            nonempty_after: Some(2),
            ..fake()
        };
        let mut evidence = Vec::new();
        assert!(apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).is_err());
        assert_eq!(*io.writes.borrow(), ["cpu.weight"]);
    }
    #[test]
    fn verification_detects_later_control_changes() {
        let io = fake();
        let mut evidence = Vec::new();
        apply_settings(&io, &configured(), Path::new("/owned"), &mut evidence).unwrap();
        io.values
            .borrow_mut()
            .insert("cpu.weight".into(), "100".into());
        assert!(verify_settings(&io, &configured()).is_err());
    }
    #[test]
    fn no_default_cpu_bandwidth_or_memory_limit() {
        assert_eq!(
            CgroupConfig::default().settings(),
            vec![("cpu.weight", "10".into())]
        );
    }
    #[test]
    fn supervisor_process_handle_requirement_is_deferred_without_fake_evidence() {
        let config = CgroupConfig::default();
        check_required_controls(
            &config,
            &[
                "process_handle".into(),
                "cpu.nice".into(),
                "cpu.weight".into(),
            ],
        )
        .unwrap();
        assert!(check_required_controls(&config, &["cgroup.kill".into()]).is_err());
        assert!(check_required_controls(&config, &["cpu.max".into()]).is_err());
    }
    #[test]
    fn whitespace_is_canonical_but_extra_fields_are_invalid() {
        assert_eq!(
            canonical_control("cpu.max", " 20000  100000\n").unwrap(),
            "20000 100000"
        );
        assert!(canonical_control("memory.max", "1000 extra").is_err());
    }
}
