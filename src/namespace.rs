//! Cooperative namespace lifetime protection. Lock files live outside the state
//! directory and are never replaced; guards release only on the final close.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::collections::BTreeMap;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceIdentity {
    pub namespace_id: String,
    pub session_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryState {
    pub format_version: u32,
    pub root: PathBuf,
    pub identity: NamespaceIdentity,
    pub phase: String,
    pub operation_id: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Namespace {
    root: PathBuf,
    control: PathBuf,
}
#[derive(Debug)]
struct Exclusion {
    _owner: File,
    _admission: File,
}
#[derive(Debug)]
struct GuardInner {
    _lifecycle: File,
    // An exclusive guard must retain admission and owner exclusion as well.
    _exclusive: Option<Arc<Exclusion>>,
    root: PathBuf,
    identity: NamespaceIdentity,
}
#[derive(Debug, Clone)]
pub struct NamespaceGuard(Arc<GuardInner>);
impl NamespaceGuard {
    pub fn identity(&self) -> &NamespaceIdentity {
        &self.0.identity
    }
    pub fn root(&self) -> &Path {
        &self.0.root
    }
    pub fn is_exclusive(&self) -> bool {
        self.0._exclusive.is_some()
    }
    pub fn validate_root(&self, root: &Path) -> Result<()> {
        ensure!(
            canonical_location(root, false)? == self.0.root,
            "namespace guard belongs to another state root"
        );
        Ok(())
    }
}
pub struct Maintenance {
    namespace: Namespace,
    exclusion: Arc<Exclusion>,
}
pub struct NamespaceOwner {
    _file: File,
}
impl Namespace {
    #[cfg(not(unix))]
    pub fn new(_root: &Path) -> Result<Self> {
        anyhow::bail!("local namespace protection is unsupported on this platform")
    }
    #[cfg(unix)]
    pub fn new(root: &Path) -> Result<Self> {
        let root = canonical_location(root, true)?;
        let leaf = root
            .file_name()
            .context("namespace leaf missing")?
            .to_str()
            .context("namespace leaf must be UTF-8")?;
        let control = root
            .parent()
            .context("namespace parent missing")?
            .join(format!(".{leaf}.cedegrid-control"));
        private_directory(&control)?;
        for name in ["owner.lock", "admission.lock", "lifecycle.lock"] {
            let _ = lock_file(&control.join(name))?;
        }
        bind_lock_identities(&control, &root)?;
        File::open(control.parent().unwrap())?.sync_all()?;
        Ok(Self { root, control })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn control_dir(&self) -> &Path {
        &self.control
    }
    pub fn state(&self) -> Result<Option<RecoveryState>> {
        let path = self.control.join("namespace.json");
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        let file = match options.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        verify_file(&file, &path)?;
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024, "namespace metadata exceeds limit");
        let value: RecoveryState = serde_json::from_slice(&bytes)?;
        ensure!(
            value.format_version == 2 && value.root == self.root,
            "unsupported or substituted namespace metadata"
        );
        Ok(Some(value))
    }
    fn initial_state(&self) -> RecoveryState {
        RecoveryState {
            format_version: 2,
            root: self.root.clone(),
            identity: NamespaceIdentity {
                namespace_id: uuid::Uuid::new_v4().to_string(),
                session_id: None,
            },
            phase: "ready".into(),
            operation_id: None,
        }
    }
    fn persist(&self, state: &RecoveryState) -> Result<()> {
        ensure!(state.root == self.root, "namespace metadata root mismatch");
        let temporary = self
            .control
            .join(format!(".namespace-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, state)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, self.control.join("namespace.json"))?;
        File::open(&self.control)?.sync_all()?;
        Ok(())
    }
    pub fn acquire(
        &self,
        expected: Option<&NamespaceIdentity>,
        timeout: Duration,
    ) -> Result<NamespaceGuard> {
        let mut admission = acquire(&self.control.join("admission.lock"), false, timeout)?;
        if self.state()?.is_none() {
            drop(admission);
            admission = acquire(&self.control.join("admission.lock"), true, timeout)?;
            if self.state()?.is_none() {
                self.persist(&self.initial_state())?;
            }
        }
        let lifecycle = acquire(&self.control.join("lifecycle.lock"), false, timeout)?;
        let state = self.state()?.context("namespace metadata missing")?;
        ensure!(
            state.phase == "ready",
            "namespace recovery incomplete: {}",
            state.phase
        );
        ensure!(
            expected.is_none_or(|e| e == &state.identity),
            "stale namespace/session; protected data must not be opened"
        );
        drop(admission);
        Ok(NamespaceGuard(Arc::new(GuardInner {
            _lifecycle: lifecycle,
            _exclusive: None,
            root: self.root.clone(),
            identity: state.identity,
        })))
    }
    pub fn begin_maintenance(&self, timeout: Duration) -> Result<Maintenance> {
        let owner = acquire(&self.control.join("owner.lock"), true, timeout)?;
        let admission = acquire(&self.control.join("admission.lock"), true, timeout)?;
        Ok(Maintenance {
            namespace: self.clone(),
            exclusion: Arc::new(Exclusion {
                _owner: owner,
                _admission: admission,
            }),
        })
    }
    pub fn owner(&self, timeout: Duration) -> Result<NamespaceOwner> {
        Ok(NamespaceOwner {
            _file: acquire(&self.control.join("owner.lock"), true, timeout)?,
        })
    }
}
impl Maintenance {
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }
    /// Persist before a fencing RPC. A failed/lost reply leaves admission closed
    /// on subsequent starts until a fresh authenticated recovery succeeds.
    pub fn record_intent(&self, operation_id: &str) -> Result<()> {
        let mut state = self
            .namespace
            .state()?
            .unwrap_or_else(|| self.namespace.initial_state());
        if state.phase != "ready" {
            // A fresh authenticated process must preserve the prior operation's
            // partial identity and phase before beginning another recovery.
            let history = self
                .namespace
                .control
                .join(format!("recovery-history-{}.json", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
            }
            let mut file = options.open(history)?;
            serde_json::to_writer(&mut file, &state)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            File::open(&self.namespace.control)?.sync_all()?;
        }
        state.phase = "fencing".into();
        state.operation_id = Some(operation_id.into());
        self.namespace.persist(&state)
    }
    pub fn exclusive(&self, timeout: Duration) -> Result<NamespaceGuard> {
        let lifecycle = acquire(
            &self.namespace.control.join("lifecycle.lock"),
            true,
            timeout,
        )?;
        let state = self
            .namespace
            .state()?
            .unwrap_or_else(|| self.namespace.initial_state());
        if self.namespace.state()?.is_none() {
            self.namespace.persist(&state)?;
        }
        Ok(NamespaceGuard(Arc::new(GuardInner {
            _lifecycle: lifecycle,
            _exclusive: Some(self.exclusion.clone()),
            root: self.namespace.root.clone(),
            identity: state.identity,
        })))
    }
    pub fn initialize_session(
        &self,
        guard: NamespaceGuard,
        session_id: &str,
    ) -> Result<NamespaceGuard> {
        guard.validate_root(&self.namespace.root)?;
        ensure!(
            guard.is_exclusive() && Arc::strong_count(&guard.0) == 1,
            "session change requires exclusive lifecycle with no borrowed data handles"
        );
        let mut state = self
            .namespace
            .state()?
            .context("namespace metadata missing")?;
        state.identity = NamespaceIdentity {
            namespace_id: uuid::Uuid::new_v4().to_string(),
            session_id: Some(session_id.into()),
        };
        state.phase = "initializing".into();
        state.operation_id = Some(session_id.into());
        self.namespace.persist(&state)?;
        let mut inner = Arc::try_unwrap(guard.0)
            .map_err(|_| anyhow::anyhow!("exclusive guard still borrowed"))?;
        inner.identity = state.identity;
        Ok(NamespaceGuard(Arc::new(inner)))
    }
    pub fn set_phase(&self, phase: &str) -> Result<()> {
        let mut state = self
            .namespace
            .state()?
            .context("namespace metadata missing")?;
        state.phase = phase.into();
        self.namespace.persist(&state)
    }
    /// Close exclusive lifecycle first, then independently obtain shared access
    /// while admission stays exclusive. No flock conversion is assumed atomic.
    pub fn activate(
        self,
        guard: NamespaceGuard,
        timeout: Duration,
    ) -> Result<(NamespaceOwner, NamespaceGuard)> {
        ensure!(
            guard.is_exclusive() && Arc::strong_count(&guard.0) == 1,
            "activation requires all exclusive data handles closed"
        );
        let identity = guard.identity().clone();
        self.set_phase("ready")?;
        drop(guard);
        let lifecycle = acquire(
            &self.namespace.control.join("lifecycle.lock"),
            false,
            timeout,
        )?;
        ensure!(
            self.namespace
                .state()?
                .is_some_and(|s| s.phase == "ready" && s.identity == identity),
            "namespace changed during activation"
        );
        let Exclusion { _owner, _admission } = Arc::try_unwrap(self.exclusion)
            .map_err(|_| anyhow::anyhow!("maintenance still borrowed"))?;
        let guard = NamespaceGuard(Arc::new(GuardInner {
            _lifecycle: lifecycle,
            _exclusive: None,
            root: self.namespace.root,
            identity,
        }));
        drop(_admission);
        Ok((NamespaceOwner { _file: _owner }, guard))
    }
}
fn canonical_location(root: &Path, create_parent: bool) -> Result<PathBuf> {
    ensure!(
        !root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "namespace parent traversal refused"
    );
    let absolute = if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir()?.join(root)
    };
    let parent = absolute.parent().context("namespace parent missing")?;
    if create_parent && !parent.exists() {
        private_directory(parent)?;
    }
    let parent = parent.canonicalize()?;
    let leaf = absolute.file_name().context("namespace leaf missing")?;
    let result = parent.join(leaf);
    if let Ok(meta) = fs::symlink_metadata(&result) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "namespace must be a real directory"
        );
    }
    Ok(result)
}
fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let parent = path.parent().context("directory parent missing")?;
        if !parent.exists() {
            private_directory(parent)?;
        }
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {
                File::open(path)?.sync_all()?;
                File::open(parent)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "namespace control directory is not a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "namespace control directory must be private and owned"
        );
    }
    Ok(())
}
fn lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    let anchored = path
        .parent()
        .is_some_and(|parent| parent.join("lock-identities.json").exists());
    options
        .read(true)
        .write(true)
        .create(!anchored)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    verify_file(&file, path)?;
    verify_lock_identity(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockIdentities {
    root: PathBuf,
    files: BTreeMap<String, (u64, u64)>,
}
#[cfg(unix)]
fn read_lock_identities(control: &Path) -> Result<Option<LockIdentities>> {
    let path = control.join("lock-identities.json");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !control.join("namespace.json").exists(),
                "namespace lock identity anchor missing"
            );
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    verify_file(&file, &path)?;
    let mut bytes = vec![];
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "lock identity metadata exceeds limit"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}
#[cfg(unix)]
fn bind_lock_identities(control: &Path, root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut expected = LockIdentities {
            root: root.into(),
            files: BTreeMap::new(),
        };
        for name in ["owner.lock", "admission.lock", "lifecycle.lock"] {
            let metadata = fs::symlink_metadata(control.join(name))?;
            expected
                .files
                .insert(name.into(), (metadata.dev(), metadata.ino()));
        }
        if let Some(stored) = read_lock_identities(control)? {
            ensure!(stored == expected, "namespace lock inode set changed");
            return Ok(());
        }
        // Serialize only first installation. The atomic rename exposes complete
        // immutable metadata to nested readers without reacquiring admission.
        let _admission = acquire(
            &control.join("admission.lock"),
            true,
            Duration::from_secs(30),
        )?;
        if let Some(stored) = read_lock_identities(control)? {
            ensure!(stored == expected, "namespace lock inode set changed");
            return Ok(());
        }
        let temporary = control.join(format!(".lock-identities-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, &expected)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, control.join("lock-identities.json"))?;
        File::open(control)?.sync_all()?;
    }
    Ok(())
}
fn verify_lock_identity(file: &File, path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = (file, path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(identities) =
            read_lock_identities(path.parent().context("lock parent missing")?)?
        {
            let metadata = file.metadata()?;
            ensure!(
                identities.files.get(
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .context("lock name invalid")?
                ) == Some(&(metadata.dev(), metadata.ino())),
                "namespace lock inode was replaced"
            );
        }
    }
    Ok(())
}
fn verify_file(file: &File, path: &Path) -> Result<()> {
    let meta = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !named.file_type().is_symlink(),
        "namespace metadata/lock must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() }
                && meta.nlink() == 1
                && meta.mode() & 0o077 == 0
                && meta.dev() == named.dev()
                && meta.ino() == named.ino(),
            "namespace lock ownership or named inode changed"
        );
    }
    Ok(())
}
fn acquire(path: &Path, exclusive: bool, timeout: Duration) -> Result<File> {
    let file = lock_file(path)?;
    let start = Instant::now();
    loop {
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(&file)
        } else {
            fs2::FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => {
                verify_file(&file, path)?;
                verify_lock_identity(&file, path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && start.elapsed() < timeout => {
                std::thread::sleep(
                    Duration::from_millis(5).min(timeout.saturating_sub(start.elapsed())),
                )
            }
            Err(e) => return Err(e).context("namespace recovery busy: lock unavailable"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn duplicated_lifecycle_description_retains_lock_and_is_close_on_exec() {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let namespace = Namespace::new(&temp.path().join("state")).unwrap();
        let guard = namespace.acquire(None, Duration::ZERO).unwrap();
        let duplicate = guard.0._lifecycle.try_clone().unwrap();
        for fd in [guard.0._lifecycle.as_raw_fd(), duplicate.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        drop(guard);
        let maintenance = namespace.begin_maintenance(Duration::ZERO).unwrap();
        assert!(maintenance.exclusive(Duration::ZERO).is_err());
        drop(duplicate);
        assert!(maintenance.exclusive(Duration::ZERO).is_ok());
    }
}
