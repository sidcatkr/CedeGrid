//! Durable observations and attempt-fenced result receipts on one local host.
//!
//! Receipt acceptance is not artifact publication. The caller must first durably
//! publish content (including its parent directory) and verify its digest. This
//! module accepts one final receipt per task; it cannot prevent duplicate execution
//! or make a workload's external side effects idempotent.

use crate::model::{Decision, Snapshot};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DATABASE_FILENAME: &str = "state.sqlite3";
#[cfg(unix)]
const INITIALIZATION_LOCK_FILENAME: &str = ".storage-profile.lock";
/// Default WAL schema stays readable by previous compatible releases.
pub const SCHEMA_VERSION: i64 = 2;
/// DELETE/EXTRA uses version 3 to make old binaries refuse before journal changes.
pub const DELETE_EXTRA_SCHEMA_VERSION: i64 = 3;
/// Replayable local state uses version 4 to fence previous strong-only readers.
pub const MAX_SCHEMA_VERSION: i64 = 4;

/// SQLite transaction and storage-assurance profile. The replayable variant is
/// an explicit deployment opt-in for non-authoritative, replay-safe local state.
/// It admits the FUSE namespace limitation, not known broken locking or I/O.
/// No profile permits a live change or ignores synchronization errors.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageProfile {
    #[default]
    WalFull,
    DeleteExtra,
    BurstReplayDeleteExtra,
}
impl StorageProfile {
    pub fn name(self) -> &'static str {
        match self {
            Self::WalFull => "wal_full",
            Self::DeleteExtra => "delete_extra",
            Self::BurstReplayDeleteExtra => "burst_replay_delete_extra",
        }
    }
    pub fn journal_mode(self) -> &'static str {
        match self {
            Self::WalFull => "wal",
            Self::DeleteExtra | Self::BurstReplayDeleteExtra => "delete",
        }
    }
    pub fn schema_version(self) -> i64 {
        match self {
            Self::WalFull => SCHEMA_VERSION,
            Self::DeleteExtra => DELETE_EXTRA_SCHEMA_VERSION,
            Self::BurstReplayDeleteExtra => MAX_SCHEMA_VERSION,
        }
    }
    pub fn synchronous(self) -> i64 {
        match self {
            Self::WalFull => 2,
            Self::DeleteExtra | Self::BurstReplayDeleteExtra => 3,
        }
    }
    pub fn is_replayable(self) -> bool {
        self == Self::BurstReplayDeleteExtra
    }
    pub fn assurance(self) -> StorageAssurance {
        if self.is_replayable() {
            StorageAssurance::ReplayableLocal
        } else {
            StorageAssurance::DurableLocal
        }
    }
    fn parse(name: &str) -> Result<Self> {
        match name {
            "wal_full" => Ok(Self::WalFull),
            "delete_extra" => Ok(Self::DeleteExtra),
            "burst_replay_delete_extra" => Ok(Self::BurstReplayDeleteExtra),
            _ => bail!("unknown persisted storage profile {name:?}"),
        }
    }
}

/// The local state's permitted role, independent of successful SQLite commits.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageAssurance {
    #[default]
    DurableLocal,
    ReplayableLocal,
}
impl StorageAssurance {
    pub fn name(self) -> &'static str {
        match self {
            Self::DurableLocal => "durable_local",
            Self::ReplayableLocal => "replayable_local",
        }
    }
    pub fn detail(self) -> &'static str {
        match self {
            Self::DurableLocal => {
                "Known local filesystem; durability depends on reliable OS/storage sync and locking"
            }
            Self::ReplayableLocal => {
                "Non-authoritative replay-safe local state only; no host/power-loss or filesystem-daemon-failure durability and no namespace barrier claimed; local state may be lost or corrupt and requires authoritative reconciliation"
            }
        }
    }
}

/// Requires the upstream WAL-reset race fix; see <https://www.sqlite.org/wal.html>.
pub const MIN_SQLITE_VERSION_NUMBER: i32 = 3_051_003;

pub fn sqlite_version() -> String {
    rusqlite::version().to_owned()
}

pub fn sqlite_version_supported(version_number: i32) -> bool {
    version_number >= MIN_SQLITE_VERSION_NUMBER
}

pub fn runtime_sqlite_supported() -> bool {
    sqlite_version_supported(rusqlite::version_number())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoragePreflight {
    pub requested_path: PathBuf,
    /// Canonical nearest existing ancestor; a path under home is not assumed local.
    pub inspected_path: PathBuf,
    pub filesystem: String,
    pub supported: bool,
    pub detail: String,
}

impl StoragePreflight {
    /// Explicit deployment selection can accept a FUSE namespace with no proven
    /// backing directory barrier. The operator must first verify actual locking
    /// and process-crash behavior; this classification supplies no such proof.
    /// Unknown/network/volatile filesystems are never blanket-admitted.
    pub fn admitted_by(&self, profile: StorageProfile) -> bool {
        self.supported || (profile.is_replayable() && self.filesystem == "fuse")
    }
}

/// Inspects filesystem capability without creating directories or opening SQLite.
/// Unknown, network, FUSE, overlay, and memory-backed storage fail closed. A known
/// local filesystem still depends on the OS and storage honoring sync requests.
pub fn preflight(path: &Path) -> Result<StoragePreflight> {
    ensure!(
        !path.as_os_str().is_empty(),
        "state directory must not be empty"
    );
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .context("no existing ancestor for state directory")?;
            }
            Err(error) => return Err(error).context("cannot inspect state directory ancestor"),
        }
    }
    // A missing component followed by `..` could traverse into an existing mount
    // which the nearest-ancestor check never inspected. Require that portion of
    // a not-yet-created path to be direct child names instead of interpreting it.
    let missing_suffix = absolute.strip_prefix(ancestor)?;
    ensure!(
        !missing_suffix
            .components()
            .any(|component| component == std::path::Component::ParentDir),
        "parent traversal after a missing path component is unsupported; use the resolved destination path"
    );
    let inspected_path = ancestor
        .canonicalize()
        .context("cannot resolve state directory ancestor")?;
    ensure!(
        inspected_path.is_dir(),
        "state directory ancestor is not a directory"
    );
    let (filesystem, supported, detail) = inspect_filesystem(&inspected_path)?;
    Ok(StoragePreflight {
        requested_path: absolute,
        inspected_path,
        filesystem,
        supported,
        detail,
    })
}

/// Name-based classification is intentionally conservative, including on systems
/// where a filesystem has a local-looking name but is not marked local by the OS.
pub fn filesystem_supported(name: &str, locally_mounted: bool) -> bool {
    locally_mounted
        && matches!(
            name,
            "apfs" | "hfs" | "ufs" | "ext" | "xfs" | "btrfs" | "zfs" | "f2fs" | "jfs" | "nilfs"
        )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_filesystem(path: &Path) -> Result<(String, bool, String)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).context("state path contains NUL")?;
    let mut storage = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: c_path is NUL-terminated and storage points to enough writable memory.
    let result = unsafe { libc::statfs(c_path.as_ptr(), storage.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("statfs failed for state directory");
    }
    // SAFETY: a successful statfs initialized the entire output structure.
    let storage = unsafe { storage.assume_init() };
    #[cfg(target_os = "macos")]
    let (name, local) = {
        use std::ffi::CStr;
        // SAFETY: macOS statfs returns a NUL-terminated f_fstypename field.
        let name = unsafe { CStr::from_ptr(storage.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        (name, (storage.f_flags & libc::MNT_LOCAL as u32) != 0)
    };
    #[cfg(target_os = "linux")]
    let (name, local) = {
        // Filesystem magic is a 32-bit value even where statfs stores it in a
        // signed machine word; normalize away sign extension on 32-bit Linux.
        let name = match storage.f_type as u32 {
            0xef53 => "ext",
            0x5846_5342 => "xfs",
            0x9123_683e => "btrfs",
            0x2fc1_2fc1 => "zfs",
            0xf2f5_2010 => "f2fs",
            0x3153_464a => "jfs",
            0x3434 => "nilfs",
            0x6969 => "nfs",
            0x517b | 0xff53_4d42 => "smb",
            0x6573_5546 => "fuse",
            0x794c_7630 => "overlay",
            0x0102_1994 => "tmpfs",
            0x8584_58f6 => "ramfs",
            0x00c3_6400 => "ceph",
            0x0bd0_0bd0 => "lustre",
            _ => "unknown",
        };
        (name.to_owned(), true)
    };
    let supported = filesystem_supported(&name, local);
    let detail = if supported {
        "Known local filesystem; durability still depends on reliable OS/storage sync and locking"
            .to_owned()
    } else {
        "Durable state storage requires a verified local disk filesystem for strong profiles; network, FUSE, overlay, volatile, and unknown filesystems are unsupported".to_owned()
    };
    Ok((name, supported, detail))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_filesystem(_path: &Path) -> Result<(String, bool, String)> {
    Ok(("unknown".to_owned(), false, "Local filesystem verification is not implemented on this platform; storage writes are disabled".to_owned()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Assigned,
    Completed,
    NeedsReconciliation,
}

impl TaskStatus {
    fn from_database(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "assigned" => Ok(Self::Assigned),
            "completed" => Ok(Self::Completed),
            "needs_reconciliation" => Ok(Self::NeedsReconciliation),
            _ => bail!("unknown task status {value:?}; refusing to infer recovery behavior"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: String,
    pub replay_safe: bool,
    pub status: TaskStatus,
    pub generation: u64,
    pub assignment_id: Option<String>,
    pub receipt_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultReceipt {
    pub task_id: String,
    pub generation: u64,
    pub receipt_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurabilitySettings {
    #[serde(default)]
    pub profile: StorageProfile,
    #[serde(default)]
    pub assurance: StorageAssurance,
    #[serde(default)]
    pub assurance_detail: String,
    #[serde(default)]
    pub busy_timeout_ms: u64,
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: bool,
    pub schema_version: i64,
}

pub struct StateStore {
    pub(crate) connection: Connection,
    path: PathBuf,
    profile: StorageProfile,
}

impl StateStore {
    pub fn open(state_dir: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(state_dir, Duration::from_secs(5))
    }

    /// The SQLite lock wait is independently adjustable; it is not a workload
    /// drain, termination, lease, or release-confirmation deadline.
    pub fn open_with_busy_timeout(state_dir: &Path, busy_timeout: Duration) -> Result<Self> {
        Self::open_with_profile_and_busy_timeout(state_dir, StorageProfile::default(), busy_timeout)
    }

    pub fn open_with_profile(state_dir: &Path, profile: StorageProfile) -> Result<Self> {
        Self::open_with_profile_and_busy_timeout(state_dir, profile, Duration::from_secs(5))
    }

    /// Open a provisioned journal for an existing session. This path never
    /// creates directories or database files, changes permissions, adopts a
    /// legacy profile, or migrates schemas. SQLite may maintain its own normal
    /// WAL/rollback bookkeeping for an already initialized database.
    pub fn open_existing_with_profile(state_dir: &Path, profile: StorageProfile) -> Result<Self> {
        ensure!(
            runtime_sqlite_supported(),
            "SQLite {} is unsupported: require >=3.51.3",
            sqlite_version()
        );
        let capability = preflight(state_dir)?;
        ensure!(
            capability.admitted_by(profile),
            "unsafe existing-state filesystem {}: {}",
            capability.filesystem,
            capability.detail
        );
        validate_private_state_leaf(state_dir)?;
        let canonical_dir = state_dir
            .canonicalize()
            .context("existing state directory is missing")?;
        let before = existing_private_state_identity(state_dir, &canonical_dir)?;
        let path = canonical_dir.join(DATABASE_FILENAME);
        // Do not call InitializationLock/prepare_private_state: both have creation
        // behavior. The absence of SQLITE_OPEN_CREATE closes the check/open race.
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .context("cannot open existing private state database")?;
        ensure!(
            existing_private_state_identity(state_dir, &canonical_dir)? == before,
            "existing state identity changed while opening SQLite"
        );
        verify_sqlite_named_inode(&connection)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "synchronous", profile.synchronous())?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        #[cfg(target_os = "macos")]
        {
            connection.pragma_update(None, "fullfsync", "ON")?;
            connection.pragma_update(None, "checkpoint_fullfsync", "ON")?;
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == profile.schema_version(),
            "existing state schema {version} differs from required schema {}; initialization or migration is forbidden",
            profile.schema_version()
        );
        let persisted = persisted_profile(&connection)?
            .context("existing state has no persisted storage profile; adoption is forbidden")?;
        ensure!(
            persisted == profile,
            "existing state storage profile mismatch: database uses {}, requested {}",
            persisted.name(),
            profile.name()
        );
        let store = Self {
            connection,
            path,
            profile,
        };
        store.verify_durability()?;
        ensure!(
            preflight(state_dir)?.admitted_by(profile),
            "existing state filesystem changed while opening SQLite"
        );
        ensure!(
            existing_private_state_identity(state_dir, &canonical_dir)? == before,
            "existing state identity changed during validation"
        );
        verify_sqlite_named_inode(&store.connection)?;
        Ok(store)
    }

    /// Existing databases have an immutable persisted profile. A different
    /// requested profile is refused before any journal-mode change; migration
    /// and recovery use the original SQLite connection/transaction abstraction.
    /// Provision a fresh store before starting services. The schema fence
    /// rejects old binaries on reopen; an old binary concurrently entering
    /// its own first-open path does not participate in this initialization lock.
    pub fn open_with_profile_and_busy_timeout(
        state_dir: &Path,
        profile: StorageProfile,
        busy_timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            busy_timeout.as_millis() <= i32::MAX as u128,
            "SQLite busy timeout exceeds supported range"
        );
        ensure!(
            runtime_sqlite_supported(),
            "SQLite {} is unsupported: require >=3.51.3 for the WAL-reset fix",
            sqlite_version()
        );
        let capability = preflight(state_dir)?;
        ensure!(
            capability.admitted_by(profile),
            "unsafe state filesystem {}: {}",
            capability.filesystem,
            capability.detail
        );
        validate_private_state_leaf(state_dir)?;
        create_directory_with_profile(state_dir, profile, &mut sync_directory)
            .context("cannot durably create state directory")?;
        // Resolve again after creation: nested mounts and symlinks must not turn
        // an accepted ancestor into an unchecked database destination.
        let capability = preflight(state_dir)?;
        ensure!(
            capability.admitted_by(profile),
            "state filesystem changed or is unsupported: {}",
            capability.filesystem
        );
        let canonical_dir = state_dir.canonicalize()?;
        let path = canonical_dir.join(DATABASE_FILENAME);
        // Scope permissions to the verified state leaf and its SQLite files;
        // never change the process umask or permissions of existing ancestors.
        let _initialization = InitializationLock::acquire(&canonical_dir, busy_timeout)?;
        prepare_private_state(state_dir, &canonical_dir)?;
        reject_database_links(&canonical_dir)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .context("cannot open private state database")?;
        connection.busy_timeout(busy_timeout)?;
        let existing_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            existing_version >= 0,
            "invalid negative database schema {existing_version}"
        );
        ensure!(
            existing_version <= profile.schema_version(),
            "database schema {existing_version} is newer than supported schema {} for profile {}",
            profile.schema_version(),
            profile.name()
        );
        let current_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let persisted = persisted_profile(&connection)?;
        let table_count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table'",
            [],
            |row| row.get(0),
        )?;
        if let Some(existing) = persisted {
            ensure!(
                existing == StorageProfile::WalFull
                    || existing_version == existing.schema_version(),
                "persisted rollback profile schema fence is invalid; refusing automatic repair"
            );
            ensure!(
                existing == profile,
                "storage profile mismatch: database uses {}, requested {}; live profile transitions are forbidden",
                existing.name(),
                profile.name()
            );
            ensure!(
                current_mode.eq_ignore_ascii_case(existing.journal_mode()),
                "persisted storage profile and actual journal mode disagree; refusing automatic repair"
            );
        } else if existing_version != 0 || table_count != 0 {
            // Pre-profile releases only supported WAL/FULL. An unmarked database
            // with another journal mode is not silently adopted or converted.
            ensure!(
                profile == StorageProfile::WalFull && current_mode.eq_ignore_ascii_case("wal"),
                "legacy state must retain WAL/FULL; profile adoption requires a verified offline transition"
            );
        } else if !current_mode.eq_ignore_ascii_case(profile.journal_mode()) {
            // Only the serialized, empty first-open path changes journal mode.
            connection.pragma_update(None, "journal_mode", profile.journal_mode())?;
        }
        connection.pragma_update(None, "synchronous", profile.synchronous())?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        #[cfg(target_os = "macos")]
        {
            connection.pragma_update(None, "fullfsync", "ON")?;
            connection.pragma_update(None, "checkpoint_fullfsync", "ON")?;
        }
        let store = Self {
            connection,
            path,
            profile,
        };
        store.migrate()?;
        store.verify_durability()?;
        // Request the strongest available namespace sync under both assurances.
        // Replayable storage does not claim that this supplies a backing barrier.
        sync_directory(&canonical_dir).context("cannot synchronize database directory")?;
        Ok(store)
    }

    /// Opens an existing database without migrations or logical state changes.
    /// SQLite may maintain WAL shared-memory bookkeeping on the local filesystem.
    pub fn open_read_only(state_dir: &Path) -> Result<Self> {
        Self::open_read_only_checked(state_dir, None)
    }

    /// Pins the expected profile while preserving a strictly read-only database
    /// handle. Missing legacy metadata is interpreted as WAL/FULL only.
    pub fn open_read_only_with_profile(state_dir: &Path, profile: StorageProfile) -> Result<Self> {
        Self::open_read_only_checked(state_dir, Some(profile))
    }

    fn open_read_only_checked(state_dir: &Path, expected: Option<StorageProfile>) -> Result<Self> {
        ensure!(
            runtime_sqlite_supported(),
            "SQLite {} is unsupported: require >=3.51.3",
            sqlite_version()
        );
        let capability = preflight(state_dir)?;
        ensure!(
            capability.admitted_by(expected.unwrap_or_default()),
            "unsupported state filesystem: {}",
            capability.filesystem
        );
        let canonical_dir = state_dir
            .canonicalize()
            .context("state directory does not exist")?;
        reject_database_links(&canonical_dir)?;
        let path = canonical_dir.join(DATABASE_FILENAME);
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let profile = persisted_profile(&connection)?.unwrap_or_default();
        ensure!(
            !profile.is_replayable() || expected == Some(profile),
            "replayable local state requires explicit profile selection"
        );
        ensure!(
            expected.is_none_or(|expected| expected == profile),
            "storage profile mismatch during read-only open"
        );
        connection.pragma_update(None, "synchronous", profile.synchronous())?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            connection,
            path,
            profile,
        };
        store.verify_durability()?;
        Ok(store)
    }

    pub fn storage_profile(&self) -> StorageProfile {
        self.profile
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }

    pub fn sqlite_version(&self) -> String {
        sqlite_version()
    }

    pub fn durability_settings(&self) -> Result<DurabilitySettings> {
        Ok(DurabilitySettings {
            profile: self.profile,
            assurance: self.profile.assurance(),
            assurance_detail: self.profile.assurance().detail().into(),
            busy_timeout_ms: u64::try_from(self.connection.pragma_query_value(
                None,
                "busy_timeout",
                |row| row.get::<_, i64>(0),
            )?)?,
            journal_mode: self
                .connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))?,
            synchronous: self
                .connection
                .pragma_query_value(None, "synchronous", |row| row.get(0))?,
            foreign_keys: self
                .connection
                .pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
            schema_version: self
                .connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?,
        })
    }

    fn verify_durability(&self) -> Result<()> {
        verify_assurance(&self.connection, self.profile)?;
        let actual = self.durability_settings()?;
        ensure!(
            actual
                .journal_mode
                .eq_ignore_ascii_case(self.profile.journal_mode()),
            "SQLite did not enable requested journal mode {}: {:?}",
            self.profile.journal_mode(),
            actual.journal_mode
        );
        ensure!(
            actual.synchronous == self.profile.synchronous(),
            "SQLite did not enable requested synchronous setting {}",
            self.profile.synchronous()
        );
        ensure!(actual.foreign_keys, "SQLite did not enable foreign keys");
        ensure!(
            actual.schema_version == self.profile.schema_version(),
            "unsupported database schema {}",
            actual.schema_version
        );
        Ok(())
    }

    fn transaction(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.connection,
            TransactionBehavior::Immediate,
        )?)
    }

    fn migrate(&self) -> Result<()> {
        let transaction = self.transaction()?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version <= self.profile.schema_version(),
            "database schema {version} is newer than supported schema {} for selected profile",
            self.profile.schema_version()
        );
        if version == 0 {
            transaction.execute_batch(
                "CREATE TABLE observations (
                    id INTEGER PRIMARY KEY,
                    node_id TEXT NOT NULL,
                    observed_at_unix_ms INTEGER NOT NULL CHECK(observed_at_unix_ms >= 0),
                    snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
                    decision_json TEXT NOT NULL CHECK(json_valid(decision_json))
                ) STRICT;
                CREATE TABLE tasks (
                    task_id TEXT PRIMARY KEY NOT NULL,
                    replay_safe INTEGER NOT NULL CHECK(replay_safe IN (0, 1)),
                    status TEXT NOT NULL CHECK(status IN ('queued','assigned','completed','needs_reconciliation')),
                    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
                    assignment_id TEXT,
                    receipt_hash TEXT,
                    CHECK((status = 'completed') = (receipt_hash IS NOT NULL))
                ) STRICT;
                CREATE TABLE assignments (
                    assignment_id TEXT PRIMARY KEY NOT NULL,
                    task_id TEXT NOT NULL REFERENCES tasks(task_id),
                    generation INTEGER NOT NULL CHECK(generation > 0),
                    UNIQUE(task_id, generation)
                ) STRICT;
                PRAGMA user_version = 1;",
            )?;
        }
        if version < 2 {
            transaction.execute_batch(
                "CREATE TABLE executions (
                    assignment_id TEXT PRIMARY KEY REFERENCES assignments(assignment_id),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
                    record_json TEXT NOT NULL CHECK(json_valid(record_json))
                ) STRICT;
                CREATE TABLE execution_events (
                    id INTEGER PRIMARY KEY,
                    assignment_id TEXT NOT NULL REFERENCES executions(assignment_id),
                    record_json TEXT NOT NULL CHECK(json_valid(record_json))
                ) STRICT;
                PRAGMA user_version = 2;",
            )?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS state_storage_profile (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                profile TEXT NOT NULL CHECK(profile IN ('wal_full','delete_extra','burst_replay_delete_extra'))
            ) STRICT;",
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO state_storage_profile(singleton,profile) VALUES (1,?1)",
            [self.profile.name()],
        )?;
        ensure!(
            persisted_profile(&transaction)? == Some(self.profile),
            "storage profile changed during migration"
        );
        if self.profile.is_replayable() && version == 0 {
            transaction.execute_batch(
                "CREATE TABLE state_storage_assurance (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    assurance TEXT NOT NULL CHECK(assurance='replayable_local')
                ) STRICT;
                INSERT INTO state_storage_assurance VALUES (1,'replayable_local');",
            )?;
        }
        verify_assurance(&transaction, self.profile)?;
        if self.profile != StorageProfile::WalFull {
            // Earlier strong-only readers refuse the replayable schema before
            // journal configuration, including DELETE/EXTRA readers.
            transaction.pragma_update(None, "user_version", self.profile.schema_version())?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Observation and the exact policy decision are one atomic record under the
    /// selected storage assurance. Replayable local records are not authority.
    pub fn append_observation(&self, snapshot: &Snapshot, decision: &Decision) -> Result<i64> {
        ensure!(
            snapshot.node_id == decision.node_id,
            "snapshot and decision node IDs differ"
        );
        ensure!(
            crate::model::supported_schema(snapshot.schema_version)
                && crate::model::supported_schema(decision.schema_version),
            "unsupported observation schema version"
        );
        // Persist the caller's actual mode. Recording a live-agent decision is
        // diagnostic state only; this method never authorizes or launches work.
        let timestamp = i64::try_from(snapshot.observed_at_unix_ms)
            .context("observation timestamp exceeds SQLite integer range")?;
        let snapshot_json = serde_json::to_string(snapshot)?;
        let decision_json = serde_json::to_string(decision)?;
        let transaction = self.transaction()?;
        transaction.execute(
            "INSERT INTO observations(node_id, observed_at_unix_ms, snapshot_json, decision_json) VALUES (?1,?2,?3,?4)",
            params![snapshot.node_id, timestamp, snapshot_json, decision_json],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(id)
    }

    /// Newest-first, bounded by the caller; zero returns no observations.
    pub fn observations(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let limit = i64::try_from(limit).context("observation limit is too large")?;
        let mut statement = self.connection.prepare(
            "SELECT id,snapshot_json,decision_json FROM observations ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (id, snapshot, decision) = row?;
            output.push(serde_json::json!({
                "id": id,
                "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot)?,
                "decision": serde_json::from_str::<serde_json::Value>(&decision)?,
            }));
        }
        Ok(output)
    }

    /// Idempotent submission may not change the original replay-safety contract.
    pub fn submit(&self, task_id: &str, replay_safe: bool) -> Result<()> {
        ensure!(
            !self.profile.is_replayable() || replay_safe,
            "replayable local storage requires replay-safe tasks"
        );
        ensure!(!task_id.trim().is_empty(), "task ID must not be empty");
        let transaction = self.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT replay_safe FROM tasks WHERE task_id=?1",
                [task_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing == replay_safe,
                "task already exists with a different replay-safety contract"
            );
        } else {
            transaction.execute(
                "INSERT INTO tasks(task_id,replay_safe,status) VALUES (?1,?2,'queued')",
                params![task_id, replay_safe],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// A retried assignment request returns its existing active generation.
    /// Assignment IDs are globally unique and cannot be recycled after uncertainty.
    pub fn assign(&self, task_id: &str, assignment_id: &str) -> Result<u64> {
        ensure!(
            !assignment_id.trim().is_empty(),
            "assignment ID must not be empty"
        );
        let transaction = self.transaction()?;
        let current = read_task(&transaction, task_id)?;
        let existing: Option<(String, i64)> = transaction
            .query_row(
                "SELECT task_id,generation FROM assignments WHERE assignment_id=?1",
                [assignment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((existing_task, generation)) = existing {
            ensure!(
                existing_task == task_id
                    && generation as u64 == current.generation
                    && current.status == TaskStatus::Assigned,
                "assignment ID already belongs to another or inactive attempt"
            );
            transaction.commit()?;
            return Ok(current.generation);
        }
        let unreleased: i64 = transaction.query_row(
            "SELECT count(*) FROM executions e JOIN assignments a USING(assignment_id) WHERE a.task_id=?1 AND json_extract(e.record_json,'$.phase') IS NOT 'released'",
            [task_id], |row| row.get(0),
        )?;
        ensure!(
            unreleased == 0,
            "prior local allocation remains reserved; reconciliation required before replacement assignment"
        );
        ensure!(
            current.status == TaskStatus::Queued,
            "task is not queued: {:?}",
            current.status
        );
        let generation = i64::try_from(current.generation)?
            .checked_add(1)
            .context("attempt generation exhausted")?;
        transaction.execute(
            "INSERT INTO assignments(assignment_id,task_id,generation) VALUES (?1,?2,?3)",
            params![assignment_id, task_id, generation],
        )?;
        transaction.execute(
            "UPDATE tasks SET status='assigned',generation=?2,assignment_id=?3 WHERE task_id=?1",
            params![task_id, generation, assignment_id],
        )?;
        transaction.commit()?;
        Ok(generation as u64)
    }

    /// Loss of execution certainty queues only declared replay-safe tasks.
    /// Unsafe tasks require explicit operator reconciliation in a future executor.
    pub fn mark_uncertain(&self, task_id: &str, generation: u64) -> Result<TaskStatus> {
        let transaction = self.transaction()?;
        let current = read_task(&transaction, task_id)?;
        ensure!(
            current.generation == generation && generation > 0,
            "stale or invalid attempt generation"
        );
        let status = if current.status == TaskStatus::Assigned {
            let (database_status, status) = if current.replay_safe {
                ("queued", TaskStatus::Queued)
            } else {
                ("needs_reconciliation", TaskStatus::NeedsReconciliation)
            };
            transaction.execute(
                "UPDATE tasks SET status=?2 WHERE task_id=?1",
                params![task_id, database_status],
            )?;
            status
        } else {
            current.status
        };
        transaction.commit()?;
        Ok(status)
    }

    /// Accepts a receipt only after the caller has durably published verified
    /// content. A lost-ACK resend of the same accepted generation/digest returns
    /// the same success receipt; any different or stale final result is rejected.
    pub fn accept_result(
        &self,
        task_id: &str,
        generation: u64,
        receipt_hash: &str,
    ) -> Result<ResultReceipt> {
        ensure!(
            !self.profile.is_replayable(),
            "final result acceptance requires strong authoritative storage; replayable local state cannot accept results"
        );
        ensure!(
            !receipt_hash.trim().is_empty(),
            "receipt hash must not be empty"
        );
        let transaction = self.transaction()?;
        let current = read_task(&transaction, task_id)?;
        ensure!(
            current.generation == generation && generation > 0,
            "stale or invalid attempt generation"
        );
        if current.status == TaskStatus::Completed {
            ensure!(
                current.receipt_hash.as_deref() == Some(receipt_hash),
                "task already completed with a different result receipt"
            );
        } else {
            ensure!(
                current.status == TaskStatus::Assigned,
                "task is not accepting results: {:?}",
                current.status
            );
            transaction.execute(
                "UPDATE tasks SET status='completed',receipt_hash=?2 WHERE task_id=?1",
                params![task_id, receipt_hash],
            )?;
        }
        transaction.commit()?;
        Ok(ResultReceipt {
            task_id: task_id.to_owned(),
            generation,
            receipt_hash: receipt_hash.to_owned(),
        })
    }

    pub fn task(&self, task_id: &str) -> Result<TaskRecord> {
        read_task(&self.connection, task_id)
    }

    pub fn status(&self, task_id: &str) -> Result<TaskStatus> {
        Ok(self.task(task_id)?.status)
    }
}

fn read_task(connection: &Connection, task_id: &str) -> Result<TaskRecord> {
    let raw = connection.query_row("SELECT replay_safe,status,generation,assignment_id,receipt_hash FROM tasks WHERE task_id=?1", [task_id], |row| {
        Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?))
    }).optional()?.with_context(|| format!("unknown task {task_id:?}"))?;
    Ok(TaskRecord {
        task_id: task_id.to_owned(),
        replay_safe: raw.0,
        status: TaskStatus::from_database(&raw.1)?,
        generation: u64::try_from(raw.2).context("negative task generation in database")?,
        assignment_id: raw.3,
        receipt_hash: raw.4,
    })
}

/// Reject unrelated existing targets before even synchronizing their directory.
#[cfg(unix)]
fn validate_private_state_leaf(requested: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let direct_requested: PathBuf = requested.components().collect();
    let metadata = match std::fs::symlink_metadata(&direct_requested) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("cannot inspect state leaf"),
    };
    ensure!(
        metadata.file_type().is_dir(),
        "writable state directory must be a direct directory, not a symlink"
    );
    // SAFETY: geteuid only reads this process's effective user identity.
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "state directory must belong to the effective user"
    );
    let directory = direct_requested.canonicalize()?;
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home)
            .canonicalize()
            .context("cannot verify runtime home boundary")?;
        ensure!(
            !home.starts_with(&directory),
            "state must use a dedicated directory, never home or its ancestors"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_state_leaf(_requested: &Path) -> Result<()> {
    bail!("private durable state permissions are not implemented on this platform")
}

/// Permission changes use a verified directory handle and nofollow operations.
/// Existing state remains readable through the unchanged read-only API. Writers
/// require an owned dedicated leaf and singly linked, owned SQLite files.
#[cfg(unix)]
fn prepare_private_state(requested: &Path, directory: &Path) -> Result<()> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    validate_private_state_leaf(requested)?;
    let leaf = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?;
    let leaf_metadata = leaf.metadata()?;
    // SAFETY: geteuid takes no pointers and does not alter credentials.
    let owner = unsafe { libc::geteuid() };
    ensure!(
        leaf_metadata.is_dir() && leaf_metadata.uid() == owner,
        "state directory must belong to the effective user"
    );
    let by_name = std::fs::symlink_metadata(directory)?;
    ensure!(
        by_name.dev() == leaf_metadata.dev()
            && by_name.ino() == leaf_metadata.ino()
            && by_name.file_type().is_dir(),
        "state directory identity changed during preparation"
    );
    let database = directory.join(DATABASE_FILENAME);
    if !database.exists() {
        // The caller holds and has verified the initialization lock. It is the
        // only permitted entry before first database publication; unrelated
        // files still fail before permission changes or database creation.
        for entry in std::fs::read_dir(directory)? {
            ensure!(
                entry?.file_name() == INITIALIZATION_LOCK_FILENAME,
                "new state requires an empty dedicated directory; refusing to change an unrelated directory"
            );
        }
    }
    let mut sqlite_files = Vec::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = directory.join(format!("{DATABASE_FILENAME}{suffix}"));
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot inspect private SQLite file"),
        };
        ensure!(
            metadata.file_type().is_file() && metadata.uid() == owner && metadata.nlink() == 1,
            "SQLite files must be owned singly linked regular files: {}",
            path.display()
        );
        sqlite_files.push((path, metadata));
    }
    // Validate every entry before changing permissions. Keep the directory
    // handle, but never open/close existing DB or SHM file handles outside SQLite:
    // close() would discard this process's existing SQLite POSIX locks.
    leaf.set_permissions(Permissions::from_mode(0o700))?;
    leaf.sync_all()?;
    for (path, metadata) in sqlite_files {
        if metadata.mode() & 0o7777 == 0o600 {
            continue;
        }
        let name = std::ffi::CString::new(path.file_name().unwrap().as_encoded_bytes())?;
        // SAFETY: leaf is an open directory, name is NUL terminated, and the
        // nofollow flag prevents chmod from following a substituted symlink.
        let changed = unsafe {
            libc::fchmodat(
                leaf.as_raw_fd(),
                name.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        ensure!(
            changed == 0,
            "runtime cannot securely tighten SQLite permissions without following links: {}",
            std::io::Error::last_os_error()
        );
        let after = std::fs::symlink_metadata(&path)?;
        ensure!(
            after.file_type().is_file()
                && after.uid() == owner
                && after.nlink() == 1
                && after.dev() == metadata.dev()
                && after.ino() == metadata.ino()
                && after.mode() & 0o7777 == 0o600,
            "SQLite identity or permissions changed during preparation"
        );
    }
    if !database.exists() {
        let temporary = directory.join(format!(".state-create-{}.tmp", uuid::Uuid::new_v4()));
        // Close the creation descriptor before exposing the inode under SQLite's
        // database name; even a concurrent opener cannot lose its database lock.
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            file.sync_all()?;
        }
        let published = std::fs::hard_link(&temporary, &database);
        std::fs::remove_file(&temporary)?;
        leaf.sync_all()?;
        if let Err(error) = published {
            ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "cannot publish private SQLite file: {error}"
            );
        }
    }

    // SQLite's pinned Unix VFS derives WAL/journal/SHM modes from the main DB.
    // Verify rather than relying on the ambient umask for future sidecars.
    let private = std::fs::symlink_metadata(&database)?;
    ensure!(
        private.file_type().is_file()
            && private.uid() == owner
            && private.nlink() == 1
            && private.mode() & 0o7777 == 0o600,
        "private SQLite mode or identity not established"
    );
    Ok(())
}

#[cfg(not(unix))]
fn prepare_private_state(_requested: &Path, _directory: &Path) -> Result<()> {
    bail!("private durable state permissions are not implemented on this platform")
}

/// Metadata queries do not open/close another DB or SHM descriptor: closing an
/// unrelated descriptor would discard this process's SQLite POSIX locks.
#[cfg(unix)]
fn existing_private_state_identity(
    requested: &Path,
    directory: &Path,
) -> Result<((u64, u64), (u64, u64))> {
    use std::os::unix::fs::MetadataExt;
    let direct: PathBuf = requested.components().collect();
    let requested_meta =
        std::fs::symlink_metadata(&direct).context("existing state directory is missing")?;
    let leaf = std::fs::symlink_metadata(directory)?;
    // SAFETY: geteuid reads the current credential without side effects.
    let owner = unsafe { libc::geteuid() };
    ensure!(
        requested_meta.file_type().is_dir()
            && leaf.file_type().is_dir()
            && requested_meta.dev() == leaf.dev()
            && requested_meta.ino() == leaf.ino()
            && direct.canonicalize()? == directory
            && leaf.uid() == owner
            && leaf.mode() & 0o7777 == 0o700,
        "existing state requires an unchanged owned private directory with mode 0700"
    );
    let mut database_identity = None;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = directory.join(format!("{DATABASE_FILENAME}{suffix}"));
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if !suffix.is_empty() && error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => {
                return Err(error).context("existing SQLite file is missing or inaccessible");
            }
        };
        ensure!(
            metadata.file_type().is_file()
                && metadata.uid() == owner
                && metadata.nlink() == 1
                && metadata.mode() & 0o7777 == 0o600,
            "existing SQLite files must be owned singly linked private regular files with mode 0600: {}",
            path.display()
        );
        if suffix.is_empty() {
            ensure!(
                metadata.len() > 0,
                "existing SQLite database is empty; initialization is forbidden"
            );
            database_identity = Some((metadata.dev(), metadata.ino()));
        }
    }
    Ok((
        (leaf.dev(), leaf.ino()),
        database_identity.context("existing SQLite database is missing")?,
    ))
}

#[cfg(not(unix))]
fn existing_private_state_identity(
    _requested: &Path,
    _directory: &Path,
) -> Result<((u64, u64), (u64, u64))> {
    bail!("existing private state verification is not implemented on this platform")
}

fn verify_sqlite_named_inode(connection: &Connection) -> Result<()> {
    let mut moved: std::os::raw::c_int = 1;
    // SAFETY: the connection owns a live SQLite handle; main and moved remain
    // valid for the synchronous file-control call, which only inspects its VFS.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut std::os::raw::c_int).cast(),
        )
    };
    ensure!(
        result == rusqlite::ffi::SQLITE_OK && moved == 0,
        "SQLite cannot verify that the existing database still names its opened inode"
    );
    Ok(())
}

fn reject_database_links(directory: &Path) -> Result<()> {
    for name in [
        DATABASE_FILENAME.to_owned(),
        format!("{DATABASE_FILENAME}-wal"),
        format!("{DATABASE_FILENAME}-shm"),
        format!("{DATABASE_FILENAME}-journal"),
    ] {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_file(),
                "database and WAL files must be regular files, not links: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect database file"),
        }
    }
    Ok(())
}

fn verify_assurance(connection: &Connection, profile: StorageProfile) -> Result<()> {
    if profile.is_replayable() {
        let (count, assurance): (i64, Option<String>) = connection
            .query_row(
                "SELECT count(*),min(assurance) FROM state_storage_assurance WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context(
                "replayable local assurance metadata missing or invalid; refusing automatic repair",
            )?;
        ensure!(
            count == 1 && assurance.as_deref() == Some(profile.assurance().name()),
            "replayable local assurance metadata invalid; refusing automatic repair"
        );
        let total: i64 =
            connection.query_row("SELECT count(*) FROM state_storage_assurance", [], |row| {
                row.get(0)
            })?;
        ensure!(total == 1, "invalid persisted storage assurance row count");
    }
    Ok(())
}

/// Absence is the legacy WAL/FULL format, not permission to change journal mode.
fn persisted_profile(connection: &Connection) -> Result<Option<StorageProfile>> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='state_storage_profile')",
        [], |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let rows: i64 =
        connection.query_row("SELECT count(*) FROM state_storage_profile", [], |row| {
            row.get(0)
        })?;
    ensure!(rows == 1, "invalid persisted storage profile row count");
    let profile: String = connection.query_row(
        "SELECT profile FROM state_storage_profile WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    Ok(Some(StorageProfile::parse(&profile)?))
}

/// Serializes only initialization, not transactions. Its descriptor is separate
/// from SQLite inodes, so closing it cannot discard existing POSIX database locks.
struct InitializationLock {
    file: std::fs::File,
}
impl InitializationLock {
    #[cfg(unix)]
    fn acquire(directory: &Path, timeout: Duration) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let path = directory.join(INITIALIZATION_LOCK_FILENAME);
        let database = directory.join(DATABASE_FILENAME);
        // Reject an unrelated target before creating even our lock file. A
        // concurrent initializer may publish the lock/database during read_dir;
        // recheck those names, then serialize and verify the contents below.
        if !path.exists() && !database.exists() {
            let nonempty = std::fs::read_dir(directory)?.next().transpose()?.is_some();
            ensure!(
                !nonempty || path.exists() || database.exists(),
                "new state requires an empty dedicated directory; refusing to modify an unrelated directory"
            );
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        ensure!(
            metadata.is_file()
                && metadata.uid() == owner
                && metadata.nlink() == 1
                && metadata.mode() & 0o7777 == 0o600,
            "storage initialization lock must be a private owned regular file"
        );
        let started = std::time::Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < timeout =>
                {
                    std::thread::sleep(
                        Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                    );
                }
                Err(error) => return Err(error).context("storage initialization lock unavailable"),
            }
        }
        // Recheck the named inode after locking. No namespace substitution is accepted.
        let named = std::fs::symlink_metadata(&path)?;
        ensure!(
            named.dev() == metadata.dev()
                && named.ino() == metadata.ino()
                && !named.file_type().is_symlink(),
            "storage initialization lock identity changed"
        );
        Ok(Self { file })
    }
    #[cfg(not(unix))]
    fn acquire(_directory: &Path, _timeout: Duration) -> Result<Self> {
        bail!("private storage initialization locking is unsupported on this platform")
    }
}
impl Drop for InitializationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Creates missing directories from the existing ancestor outward. Synchronizing
/// each child and its parent persists both the child inode and its directory entry
/// before a deeper child (or database) can be acknowledged. The callback permits
/// testing ordering/failures without pretending to simulate power loss.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn create_directory_durably(path: &Path, sync: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    create_directory_with_profile(path, StorageProfile::default(), sync)
}

fn create_directory_with_profile(
    path: &Path,
    profile: StorageProfile,
    sync: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut ancestor = absolute.as_path();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.to_path_buf());
                ancestor = ancestor
                    .parent()
                    .context("state path has no existing ancestor")?;
            }
            Err(error) => {
                return Err(error).context("cannot inspect directory for durable creation");
            }
        }
    }
    // Also covers a previous interrupted/failed creation of the nearest ancestor.
    sync(ancestor)?;
    if let Some(parent) = ancestor.parent() {
        sync(parent)?;
    }
    for directory in missing.into_iter().rev() {
        let parent = directory
            .parent()
            .context("missing directory has no parent")?;
        let capability = preflight(parent)?;
        ensure!(
            capability.admitted_by(profile),
            "unsupported filesystem while creating state directory: {}",
            capability.filesystem
        );
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    directory.is_dir(),
                    "state path is not a directory: {}",
                    directory.display()
                );
            }
            Err(error) => return Err(error).context("cannot create state directory component"),
        }
        let capability = preflight(&directory)?;
        ensure!(
            capability.admitted_by(profile),
            "unsupported newly created state directory filesystem: {}",
            capability.filesystem
        );
        sync(&directory)?;
        sync(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("cannot open directory for sync: {}", path.display()))?
        .sync_all()
        .with_context(|| format!("cannot sync directory: {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    bail!("durable directory synchronization is not implemented on this platform")
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod directory_tests {
    use super::*;

    #[test]
    fn nested_creation_syncs_each_directory_then_parent_before_next_child() {
        let directory = tempfile::Builder::new()
            .prefix(".directory-sync-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let root = directory.path();
        let child = root.join("a");
        let grandchild = child.join("b");
        let mut calls = Vec::new();
        create_directory_durably(&grandchild, &mut |path: &Path| {
            assert!(path.is_dir(), "sync may only follow directory creation");
            if path == child && !calls.contains(&child) {
                assert!(!grandchild.exists());
            }
            calls.push(path.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls,
            vec![
                root.to_owned(),
                root.parent().unwrap().to_owned(),
                child.clone(),
                root.to_owned(),
                grandchild,
                child
            ]
        );
    }

    #[test]
    fn failed_directory_sync_stops_before_creating_deeper_children() {
        let directory = tempfile::Builder::new()
            .prefix(".directory-sync-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let child = directory.path().join("a");
        let grandchild = child.join("b");
        let result = create_directory_durably(&grandchild, &mut |path: &Path| {
            if path == child {
                bail!("injected sync failure");
            }
            Ok(())
        });
        assert!(result.is_err());
        assert!(child.is_dir());
        assert!(!grandchild.exists());
    }

    #[test]
    fn replayable_directory_creation_still_propagates_sync_failure() {
        let directory = tempfile::Builder::new()
            .prefix(".replay-sync-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let child = directory.path().join("a");
        let grandchild = child.join("b");
        let result = create_directory_with_profile(
            &grandchild,
            StorageProfile::BurstReplayDeleteExtra,
            &mut |path: &Path| {
                if path == child {
                    bail!("injected I/O failure");
                }
                Ok(())
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("injected I/O failure"));
        assert!(child.is_dir());
        assert!(!grandchild.exists());
    }
}
