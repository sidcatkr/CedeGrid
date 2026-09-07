//! Bounded, linked-SQLite compatibility diagnostics. This does not override the
//! production filesystem gate or turn process-crash evidence into power-loss proof.
use crate::state::{self, StorageAssurance, StoragePreflight, StorageProfile};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SECONDS: u64 = 30;
const DB: &str = "qualification.sqlite3";

#[derive(Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct QualificationReport {
    pub schema_version: u32,
    pub directory: PathBuf,
    pub profile: StorageProfile,
    #[serde(default)]
    pub assurance: StorageAssurance,
    #[serde(default)]
    pub assurance_detail: String,
    pub linked_sqlite_version: String,
    pub strict_preflight: StoragePreflight,
    pub strict_store_refusal: Option<String>,
    pub process_crash_compatibility_passed: bool,
    pub namespace_durability_qualified: bool,
    pub namespace_durability_basis: String,
    pub production_admission_unchanged: bool,
    pub checks: Vec<Check>,
    pub elapsed_seconds: f64,
    pub retained_bytes: u64,
    pub limits: String,
    pub cleanup_confirmed: bool,
    pub children: Vec<ChildCleanup>,
    pub limitations: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    token: String,
    profile: StorageProfile,
}

fn canonical_home_path(path: &Path, existing: bool) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "qualification requires an absolute home path"
    );
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?).canonicalize()?;
    let parent = path.parent().context("qualification path has no parent")?;
    let canonical_parent = parent.canonicalize()?;
    ensure!(
        canonical_parent == parent,
        "qualification parent must be canonical; aliases are refused"
    );
    ensure!(
        canonical_parent.starts_with(&home),
        "qualification is restricted to home"
    );
    let name = path.file_name().context("qualification path has no leaf")?;
    let result = canonical_parent.join(name);
    if existing {
        ensure!(
            std::fs::symlink_metadata(&result)?.is_dir(),
            "qualification directory must be a real directory"
        );
        ensure!(
            result.canonicalize()? == result,
            "qualification aliases are refused"
        );
    } else {
        ensure!(
            std::fs::symlink_metadata(&result)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "qualification output must be fresh"
        );
    }
    Ok(result)
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn marker(directory: &Path, name: &str) -> Result<()> {
    let mut file = private_file(&directory.join(name))?;
    file.write_all(b"ready\n")?;
    file.sync_all()?;
    Ok(())
}
fn bounded_bytes(directory: &Path) -> Result<u64> {
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            // DELETE commits can remove their journal after read_dir observed
            // it. This is an expected namespace race, not a suppressed I/O error.
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && entry.file_name() == format!("{DB}-journal").as_str() =>
            {
                continue;
            }
            Err(error) => return Err(error).context("cannot inspect qualification file"),
        };
        ensure!(
            meta.is_file(),
            "unexpected non-file in isolated qualification directory"
        );
        bytes = bytes
            .checked_add(meta.len())
            .context("qualification size overflow")?;
        ensure!(bytes <= MAX_BYTES, "qualification exceeded 16 MiB bound");
    }
    Ok(bytes)
}
fn check_deadline(deadline: Instant, directory: &Path) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "qualification exceeded 30-second work deadline"
    );
    bounded_bytes(directory)?;
    Ok(())
}
fn open(directory: &Path, profile: StorageProfile, initialize: bool) -> Result<Connection> {
    ensure!(
        state::runtime_sqlite_supported(),
        "linked SQLite requires upstream WAL fix >=3.51.3"
    );
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | if initialize {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        };
    let db = Connection::open_with_flags(directory.join(DB), flags)?;
    db.busy_timeout(Duration::from_millis(250))?;
    if initialize {
        db.pragma_update(None, "page_size", 4096)?;
        db.pragma_update(None, "journal_mode", profile.journal_mode())?;
    }
    db.pragma_update(None, "synchronous", profile.synchronous())?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "cache_size", 8)?;
    db.pragma_update(None, "max_page_count", 1024)?;
    db.pragma_update(None, "wal_autocheckpoint", 64)?;
    db.pragma_update(None, "journal_size_limit", 1024 * 1024)?;
    #[cfg(target_os = "macos")]
    {
        db.pragma_update(None, "fullfsync", true)?;
        db.pragma_update(None, "checkpoint_fullfsync", true)?;
    }
    let mode: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let sync: i64 = db.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    let fk: bool = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    ensure!(
        mode.eq_ignore_ascii_case(profile.journal_mode()) && sync == profile.synchronous() && fk,
        "effective settings mismatch: mode={mode}, synchronous={sync}, foreign_keys={fk}"
    );
    Ok(db)
}

/// Exclusively owns an unreaped direct child; no competing reaper or child forks.
/// Dropping never targets a PID obtained from persisted state or a process scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildCleanup {
    pub stage: String,
    pub pid: u32,
    pub reaped: bool,
    pub detail: String,
}
type CleanupLedger = Arc<Mutex<Vec<ChildCleanup>>>;
struct OwnedChild {
    child: Child,
    reaped: bool,
    ledger: CleanupLedger,
    index: usize,
}
impl OwnedChild {
    fn spawn(
        executable: &Path,
        directory: &Path,
        profile: StorageProfile,
        token: &str,
        stage: &str,
        ledger: &CleanupLedger,
    ) -> Result<Self> {
        let stdout = private_file(&directory.join(format!("{stage}.stdout")))?;
        let stderr = private_file(&directory.join(format!("{stage}.stderr")))?;
        let child = Command::new(executable)
            .args(["storage-qualification-child", "--directory"])
            .arg(directory)
            .args([
                "--profile",
                profile.name(),
                "--stage",
                stage,
                "--token",
                token,
            ])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()?;
        let mut records = ledger.lock().expect("private cleanup ledger poisoned");
        let index = records.len();
        records.push(ChildCleanup {
            stage: stage.into(),
            pid: child.id(),
            reaped: false,
            detail: "spawned".into(),
        });
        drop(records);
        Ok(Self {
            child,
            reaped: false,
            ledger: Arc::clone(ledger),
            index,
        })
    }
    fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if self.reaped {
            bail!("qualification child was already reaped")
        }
        let status = self.child.try_wait()?;
        self.reaped = status.is_some();
        if let Some(status) = status {
            self.record(true, format!("reaped: {status}"));
        }
        Ok(status)
    }
    fn record(&self, reaped: bool, detail: String) {
        let mut records = self.ledger.lock().expect("private cleanup ledger poisoned");
        records[self.index].reaped = reaped;
        records[self.index].detail = detail;
    }
    fn ready(&mut self, directory: &Path, stage: &str, deadline: Instant) -> Result<()> {
        loop {
            check_deadline(deadline, directory)?;
            if directory.join(format!("{stage}.ready")).try_exists()? {
                return Ok(());
            }
            ensure!(
                self.exited()?.is_none(),
                "qualification {stage} child exited before preparation marker"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn finish(&mut self, directory: &Path, deadline: Instant) -> Result<()> {
        loop {
            check_deadline(deadline, directory)?;
            if let Some(status) = self.exited()? {
                ensure!(status.success(), "qualification child failed: {status}");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn interrupt(&mut self) -> Result<()> {
        ensure!(!self.reaped, "refusing to signal a reaped child");
        self.child.kill()?;
        let status = self.child.wait()?;
        self.reaped = true;
        self.record(true, format!("owned interruption reaped: {status}"));
        ensure!(
            !status.success(),
            "interrupted child unexpectedly succeeded"
        );
        Ok(())
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Still exclusively owned and unreaped: its numeric PID cannot be reused.
            let kill = self.child.kill();
            match self.child.wait() {
                Ok(status) => self.record(
                    true,
                    format!("failure cleanup reaped: {status}; kill={kill:?}"),
                ),
                Err(error) => self.record(
                    false,
                    format!("cleanup wait failed: {error}; kill={kill:?}"),
                ),
            }
        }
    }
}

/// Child-only diagnostic entry point; it never executes user workload code.
pub fn child_main(
    directory: &Path,
    profile: StorageProfile,
    stage: &str,
    token: &str,
) -> Result<()> {
    let directory = canonical_home_path(directory, true)?;
    ensure!(
        std::fs::metadata(directory.join("manifest.json"))?.len() <= 4096,
        "oversize qualification manifest"
    );
    let manifest: Manifest = serde_json::from_reader(File::open(directory.join("manifest.json"))?)?;
    ensure!(
        manifest.token == token && manifest.profile == profile,
        "qualification capability mismatch"
    );
    ensure!(
        matches!(
            stage,
            "hold" | "dirty" | "commit_a" | "commit_b" | "publication"
        ),
        "invalid qualification stage"
    );
    let db = open(&directory, profile, false)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    if stage.starts_with("commit_") {
        let base = if stage == "commit_a" { 100 } else { 200 };
        for offset in 0..16 {
            loop {
                check_deadline(deadline, &directory)?;
                match db.execute(
                    "INSERT INTO records(id,payload) VALUES (?1,?2)",
                    params![base + offset, format!("{stage}:{offset}")],
                ) {
                    Ok(1) => break,
                    Ok(_) => bail!("writer did not insert exactly one record"),
                    Err(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == ErrorCode::DatabaseBusy
                            || e.code == ErrorCode::DatabaseLocked => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        marker(&directory, &format!("{stage}.ready"))?;
        return Ok(());
    }
    if stage == "publication" {
        let mut file = private_file(&directory.join("staging.blob"))?;
        file.write_all(&vec![b'P'; 32768])?;
        file.sync_all()?;
        std::fs::hard_link(
            directory.join("staging.blob"),
            directory.join("published.blob"),
        )?;
        File::open(&directory)?.sync_all()?;
        // This marker observes API completion; a no-op directory fsync does not
        // qualify host/power-loss durability and is reported separately.
    } else {
        db.execute_batch("BEGIN IMMEDIATE")?;
        if stage == "hold" {
            db.execute(
                "INSERT INTO records(id,payload) VALUES (999,'must-rollback')",
                [],
            )?;
        } else {
            for id in 1000..1128 {
                db.execute(
                    "INSERT INTO records(id,payload) VALUES (?1,?2)",
                    params![id, vec![b'D'; 1024]],
                )?;
            }
        }
    }
    marker(&directory, &format!("{stage}.ready"))?;
    loop {
        check_deadline(deadline, &directory)?;
        if stage == "hold" && directory.join("hold.release").try_exists()? {
            db.execute_batch("ROLLBACK")?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn passed(checks: &mut Vec<Check>, name: &str, detail: &str) {
    checks.push(Check {
        name: name.into(),
        passed: true,
        detail: detail.into(),
    });
}
fn exercise(
    directory: &Path,
    profile: StorageProfile,
    executable: &Path,
    token: &str,
    deadline: Instant,
    checks: &mut Vec<Check>,
    ledger: &CleanupLedger,
) -> Result<()> {
    let db = open(directory, profile, true)?;
    db.execute_batch("CREATE TABLE records(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
        CREATE TABLE allocations(task_id INTEGER PRIMARY KEY REFERENCES records(id), generation INTEGER NOT NULL, reserved INTEGER NOT NULL);
        CREATE TABLE publications(digest TEXT PRIMARY KEY, blob TEXT NOT NULL);
        BEGIN IMMEDIATE;
        INSERT INTO records VALUES (1,'acknowledged-before-interruption');
        INSERT INTO allocations VALUES (1,7,1);
        COMMIT;")?;
    drop(db);
    let db = open(directory, profile, false)?;
    ensure!(
        db.query_row("SELECT payload FROM records WHERE id=1", [], |r| r
            .get::<_, String>(0))?
            == "acknowledged-before-interruption",
        "committed payload changed after reopen"
    );
    passed(
        checks,
        "commit_reopen_effective_settings",
        "Linked library reopened exact committed data under the requested journal, synchronous and FK settings",
    );
    let mut holder = OwnedChild::spawn(executable, directory, profile, token, "hold", ledger)?;
    holder.ready(directory, "hold", deadline)?;
    ensure!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))? == 1,
        "reader observed another process's uncommitted row"
    );
    let error = match db.execute("INSERT INTO records VALUES (998,'conflicting-writer')", []) {
        Err(error) => error,
        Ok(_) => bail!("writer exclusion failed: competing write succeeded"),
    };
    ensure!(
        matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked),
        "competing writer failed for a reason other than locking"
    );
    marker(directory, "hold.release")?;
    holder.finish(directory, deadline)?;
    passed(
        checks,
        "multiprocess_writer_exclusion_reader_isolation",
        "Independent writer held a transaction; concurrent reader saw only committed data and competing write returned BUSY/LOCKED; holder rolled back and exited normally",
    );
    drop(db);
    let mut a = OwnedChild::spawn(executable, directory, profile, token, "commit_a", ledger)?;
    let mut b = OwnedChild::spawn(executable, directory, profile, token, "commit_b", ledger)?;
    a.finish(directory, deadline)?;
    b.finish(directory, deadline)?;
    let mut dirty = OwnedChild::spawn(executable, directory, profile, token, "dirty", ledger)?;
    dirty.ready(directory, "dirty", deadline)?;
    dirty.interrupt()?;
    let db = open(directory, profile, false)?;
    ensure!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))? == 33,
        "acknowledged row set changed or incomplete rows survived"
    );
    ensure!(
        db.query_row("SELECT payload FROM records WHERE id=1", [], |r| r
            .get::<_, String>(0))?
            == "acknowledged-before-interruption",
        "initial acknowledged payload changed after interruption"
    );
    for (base, stage) in [(100, "commit_a"), (200, "commit_b")] {
        for offset in 0..16 {
            ensure!(
                db.query_row(
                    "SELECT payload FROM records WHERE id=?1",
                    [base + offset],
                    |r| r.get::<_, String>(0)
                )? == format!("{stage}:{offset}"),
                "acknowledged writer payload changed"
            );
        }
    }
    ensure!(
        db.query_row(
            "SELECT generation*10+reserved FROM allocations WHERE task_id=1",
            [],
            |r| r.get::<_, i64>(0)
        )? == 71,
        "uncertain reservation/generation lost"
    );
    passed(
        checks,
        "concurrent_commits_and_interrupted_transaction",
        "All 33 committed records and exact payloads survived; 128 uncommitted dirty rows and holder row were absent; generation 7 reservation remained charged",
    );
    drop(db);
    let mut publication =
        OwnedChild::spawn(executable, directory, profile, token, "publication", ledger)?;
    publication.ready(directory, "publication", deadline)?;
    publication.interrupt()?;
    let db = open(directory, profile, false)?;
    ensure!(
        std::fs::read(directory.join("published.blob"))? == vec![b'P'; 32768],
        "orphan published bytes changed"
    );
    ensure!(
        db.query_row("SELECT count(*) FROM publications", [], |r| r
            .get::<_, i64>(0))?
            == 0,
        "publication was committed before the injected boundary"
    );
    for _ in 0..2 {
        db.execute("INSERT INTO publications VALUES ('expected-payload','published.blob') ON CONFLICT(digest) DO NOTHING", [])?;
    }
    ensure!(
        db.query_row("SELECT count(*) FROM publications", [], |r| r
            .get::<_, i64>(0))?
            == 1,
        "publication replay was not idempotent"
    );
    ensure!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))? == "ok",
        "SQLite integrity failed"
    );
    ensure!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "foreign-key integrity failed"
    );
    passed(
        checks,
        "interrupted_publication_and_recovery",
        "Owned child interrupted after file publication and before DB receipt; exact orphan bytes recovered and duplicate receipt insertion accepted once; integrity/FK passed",
    );
    drop(db);
    let db = open(directory, profile, false)?;
    ensure!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))? == 33
            && db.query_row("SELECT count(*) FROM publications", [], |r| r
                .get::<_, i64>(0))?
                == 1,
        "final reopen lost acknowledged records/publication"
    );
    check_deadline(deadline, directory)?;
    passed(
        checks,
        "final_reopen",
        "All expected committed records and one publication remained after every child was reaped",
    );
    Ok(())
}

pub fn run(
    directory: &Path,
    profile: StorageProfile,
    child_executable: &Path,
) -> Result<QualificationReport> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(MAX_SECONDS);
    let directory = canonical_home_path(directory, false)?;
    let strict_preflight = state::preflight(&directory)?;
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(&directory)?;
    let strict_store_refusal = if !strict_preflight.supported {
        let probe = directory.join("strict-store-must-not-exist");
        let refusal = match state::StateStore::open_with_profile(
            &probe,
            if profile.is_replayable() {
                StorageProfile::DeleteExtra
            } else {
                profile
            },
        ) {
            Err(error) => format!("{error:#}"),
            Ok(_) => {
                bail!("strict production store unexpectedly admitted an unsupported filesystem")
            }
        };
        ensure!(!probe.exists(), "strict production refusal created state");
        Some(refusal)
    } else {
        None
    };
    let token = uuid::Uuid::new_v4().to_string();
    let mut manifest = private_file(&directory.join("manifest.json"))?;
    serde_json::to_writer(
        &mut manifest,
        &Manifest {
            token: token.clone(),
            profile,
        },
    )?;
    manifest.sync_all()?;
    let mut checks = Vec::new();
    let ledger = Arc::new(Mutex::new(Vec::new()));
    let result = exercise(
        &directory,
        profile,
        child_executable,
        &token,
        deadline,
        &mut checks,
        &ledger,
    );
    let passed = result.is_ok();
    if let Err(error) = result {
        checks.push(Check {
            name: "qualification_failure".into(),
            passed: false,
            detail: format!("{error:#}"),
        });
    }
    let retained_bytes = bounded_bytes(&directory)?;
    let children = ledger
        .lock()
        .expect("private cleanup ledger poisoned")
        .clone();
    let cleanup_confirmed = children.iter().all(|child| child.reaped);
    let mut report = QualificationReport {
        schema_version: 2, directory: directory.clone(), profile,
        assurance: profile.assurance(), assurance_detail: profile.assurance().detail().into(),
        linked_sqlite_version: state::sqlite_version(),
        namespace_durability_qualified: false,
        namespace_durability_basis: if profile.is_replayable() {
            "Explicit replayable local assurance; this diagnostic does not establish namespace or host/power-loss durability".into()
        } else if strict_preflight.supported {
            "Existing strict filesystem assumption remains available; this diagnostic supplies no new namespace-durability proof".into()
        } else {
            "Unsupported strict filesystem; successful directory fsync return and process-crash tests do not provide strong assurance".into()
        },
        strict_preflight, strict_store_refusal,
        process_crash_compatibility_passed: passed && cleanup_confirmed,
        production_admission_unchanged: true, checks,
        elapsed_seconds: started.elapsed().as_secs_f64(), retained_bytes,
        limits: "30-second work deadline, 16 MiB retained-file bound, fixed 33 committed/128 interrupted rows, owned direct children only".into(),
        cleanup_confirmed, children,
        limitations: vec![
            "Process interruption evidence is not a host/power-loss or filesystem-daemon-failure test.".into(),
            "A successful directory fsync API return does not establish a backing namespace barrier; strict storage admission remains unchanged; explicit replayable local storage is never upgraded to strong assurance.".into(),
            "I/O stuck inside the kernel can exceed a userspace deadline; no privileged recovery or unrelated process signals are used.".into(),
            "Raw qualification schema checks the linked library's relevant connection pattern; it is not an application lifecycle or performance benchmark.".into(),
        ],
    };
    let report_bytes = loop {
        let mut encoded = serde_json::to_vec_pretty(&report)?;
        encoded.push(b'\n');
        let total = retained_bytes
            .checked_add(encoded.len() as u64)
            .context("report size overflow")?;
        ensure!(
            total <= MAX_BYTES,
            "report exceeds the 16 MiB retained-file bound"
        );
        if report.retained_bytes == total {
            break encoded;
        }
        report.retained_bytes = total;
    };
    let mut file = private_file(&directory.join("report.json"))?;
    file.write_all(&report_bytes)?;
    file.sync_all()?;
    Ok(report)
}
