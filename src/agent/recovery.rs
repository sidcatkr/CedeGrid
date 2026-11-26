//! Recovery of a Tracked run's members after its supervisor died (FR-088, research R42). The
//! agent re-opens each process its supervisor verified, by the identity it recorded: a pidfd
//! first, then the start time and the real, effective and saved IDs through `/proc`, then alive
//! through the pidfd, and it signals only through that pidfd. A reused PID has another start
//! time and is never signalled; neither is a process whose IDs or boot differ.
use crate::execution_model::{AllocationClass, ExecutionRecord};
use crate::supervision::FamilyMember;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// A run's last family as its supervisor reported it, in its attempt directory.
const FAMILY_FILE: &str = "family.json";
/// What recovery did, written once it ran, so it runs once per run.
const RECOVERY_FILE: &str = "family-recovery.json";

/// The part of a run's family the store does not hold: its verified descendants, the boot and
/// the run's user and group IDs. The leader's and SDK children's identities are in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FamilyRecord {
    pub class: AllocationClass,
    pub boot_id: String,
    pub uid: u32,
    pub gid: u32,
    pub descendants: Vec<FamilyMember>,
}

/// What recovery did to a run's recorded members (FR-088).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Recovered {
    pub continued: u64,
    pub ended: u64,
    /// The PID now names another process, or its IDs or boot differ: never signalled.
    pub mismatched: u64,
    pub gone: u64,
    /// A read or signal failed for another reason; the others were still handled.
    pub failed: u64,
}

impl Recovered {
    pub fn summary(&self) -> String {
        format!(
            "{} continued, {} ended, {} identity mismatches, {} already gone, {} failed",
            self.continued, self.ended, self.mismatched, self.gone, self.failed
        )
    }
}

/// How recovery reads and signals processes: the host's pidfds and `/proc`, or a fake in tests.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) trait Processes {
    type Handle;
    fn open(&self, pid: u32) -> io::Result<Self::Handle>;
    /// The start time and the real, effective and saved user and group IDs.
    fn identity(&self, pid: u32) -> io::Result<(u64, [u32; 3], [u32; 3])>;
    fn boot_id(&self) -> io::Result<String>;
    fn alive(&self, handle: &Self::Handle) -> io::Result<bool>;
    fn signal(&self, handle: &Self::Handle, signal: i32) -> io::Result<()>;
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gone(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT))
}

/// Continue or end each recorded process whose identity still matches, in Principle I's order;
/// one that fails a check or a signal does not stop the others.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn recover_members<P: Processes>(
    processes: &P,
    members: &[FamilyMember],
    boot_id: &str,
    (uid, gid): (u32, u32),
    signal: i32,
) -> Recovered {
    let mut recovered = Recovered::default();
    // Records from another boot name no process that still runs.
    if processes.boot_id().ok().as_deref() != Some(boot_id) {
        recovered.gone = members.len() as u64;
        return recovered;
    }
    for member in members {
        let handle = match processes.open(member.pid) {
            Ok(handle) => handle,
            Err(error) if gone(&error) => {
                recovered.gone += 1;
                continue;
            }
            Err(_) => {
                recovered.failed += 1;
                continue;
            }
        };
        let (start, uids, gids) = match processes.identity(member.pid) {
            Ok(identity) => identity,
            Err(error) if gone(&error) => {
                recovered.gone += 1;
                continue;
            }
            Err(_) => {
                recovered.failed += 1;
                continue;
            }
        };
        if start != member.start_time || uids != [uid; 3] || gids != [gid; 3] {
            recovered.mismatched += 1;
            continue;
        }
        match processes.alive(&handle) {
            Ok(true) => {}
            Ok(false) => {
                recovered.gone += 1;
                continue;
            }
            Err(_) => {
                recovered.failed += 1;
                continue;
            }
        }
        match processes.signal(&handle, signal) {
            Ok(()) if signal == libc::SIGKILL => recovered.ended += 1,
            Ok(()) => recovered.continued += 1,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => recovered.gone += 1,
            Err(_) => recovered.failed += 1,
        }
    }
    recovered
}

fn write_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Keep a run's last family where an agent started later finds it (FR-088).
pub(crate) fn record_family(
    attempt: &Path,
    record: &ExecutionRecord,
    family: &crate::supervision::FamilyReport,
) -> Result<()> {
    let leader = record
        .identity
        .as_ref()
        .context("a family without a leader")?;
    write_atomic(
        &attempt.join(FAMILY_FILE),
        &FamilyRecord {
            class: record.class,
            boot_id: leader.boot_id.clone(),
            uid: family.uid,
            gid: family.gid,
            descendants: family.descendants.clone(),
        },
    )
}

/// The run's recorded descendants, if its supervisor reported any.
pub(crate) fn recorded_family(attempt: &Path) -> Option<FamilyRecord> {
    let bytes = crate::publication::read_bounded(&attempt.join(FAMILY_FILE), 256 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Recover a dead Tracked supervisor's members once: Guaranteed members are continued and left
/// running, Opportunistic ones killed. `children` are the SDK children's recorded identities.
/// Nothing is done for a Full run, which keeps its leaf, or for a run never launched.
pub(crate) fn recover_run(
    attempt: &Path,
    record: &ExecutionRecord,
    children: &[FamilyMember],
) -> Result<Option<Recovered>> {
    if record.backend != "rootless" || attempt.join(RECOVERY_FILE).exists() {
        return Ok(None);
    }
    let Some(leader) = record.identity.as_ref() else {
        return Ok(None);
    };
    recover_family(attempt, record, leader, children)
}

/// Signals the run's recorded family; only Linux has the pidfds this needs.
#[cfg(target_os = "linux")]
fn recover_family(
    attempt: &Path,
    record: &ExecutionRecord,
    leader: &crate::execution_model::ProcessIdentity,
    children: &[FamilyMember],
) -> Result<Option<Recovered>> {
    let family = recorded_family(attempt);
    // Without a report, the run's IDs are this agent's, which its supervisors share.
    // SAFETY: getuid and getgid cannot fail.
    let ids = family.as_ref().map_or_else(
        || unsafe { (libc::getuid(), libc::getgid()) },
        |f| (f.uid, f.gid),
    );
    let mut members = vec![FamilyMember {
        pid: leader.pid,
        start_time: leader.start_time,
    }];
    members.extend_from_slice(children);
    members.extend(family.into_iter().flat_map(|f| f.descendants));
    let signal = match record.class {
        AllocationClass::Guaranteed => libc::SIGCONT,
        AllocationClass::Opportunistic => libc::SIGKILL,
    };
    let recovered = recover_members(&HostProcesses, &members, &leader.boot_id, ids, signal);
    // After the signals: a repeat after a crash in between sends the same signal again.
    write_atomic(&attempt.join(RECOVERY_FILE), &recovered)?;
    Ok(Some(recovered))
}

#[cfg(not(target_os = "linux"))]
fn recover_family(
    _attempt: &Path,
    _record: &ExecutionRecord,
    _leader: &crate::execution_model::ProcessIdentity,
    _children: &[FamilyMember],
) -> Result<Option<Recovered>> {
    Ok(None)
}

/// Recover a run whose supervisor is gone, if it is Tracked and not yet recovered: its SDK
/// children come from the store, its descendants from its family record. The counts go to the
/// run's recovery record and a `family_recovered` event (FR-088).
pub(crate) fn recover(
    state_dir: &Path,
    store: &crate::state::StateStore,
    record: &ExecutionRecord,
) -> Result<Option<Recovered>> {
    let children: Vec<FamilyMember> = store
        .managed_children(&record.assignment_id)?
        .into_iter()
        .filter(|c| c.phase != crate::managed_children::ManagedChildPhase::Released)
        .filter_map(|c| c.identity)
        .map(|id| FamilyMember {
            pid: id.pid,
            start_time: id.start_time,
        })
        .collect();
    let attempt = state_dir.join("attempts").join(&record.assignment_id);
    let recovered = recover_run(&attempt, record, &children)?;
    if let Some(recovered) = &recovered {
        let _ = super::emit(&serde_json::json!({
            "event": "family_recovered",
            "assignment_id": record.assignment_id,
            "class": record.class,
            "continued": recovered.continued,
            "ended": recovered.ended,
            "identity_mismatched": recovered.mismatched,
            "already_gone": recovered.gone,
            "failed": recovered.failed,
        }));
    }
    Ok(recovered)
}

#[cfg(target_os = "linux")]
struct HostProcesses;

#[cfg(target_os = "linux")]
impl Processes for HostProcesses {
    type Handle = std::os::fd::OwnedFd;
    fn open(&self, pid: u32) -> io::Result<Self::Handle> {
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful pidfd_open returns a newly owned descriptor.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
    }
    fn identity(&self, pid: u32) -> io::Result<(u64, [u32; 3], [u32; 3])> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid /proc entry");
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let start = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(19)?.parse().ok())
            .ok_or_else(invalid)?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let ids = |name: &str| -> io::Result<[u32; 3]> {
            let mut values = status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .ok_or_else(invalid)?
                .split_whitespace()
                .map(str::parse::<u32>);
            let mut next = || values.next().and_then(|v| v.ok()).ok_or_else(invalid);
            Ok([next()?, next()?, next()?])
        };
        Ok((start, ids("Uid:")?, ids("Gid:")?))
    }
    fn boot_id(&self) -> io::Result<String> {
        Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned())
    }
    fn alive(&self, handle: &Self::Handle) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: handle.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and a zero timeout.
        match unsafe { libc::poll(&mut poll, 1, 0) } {
            0 => Ok(true),
            1 if poll.revents & libc::POLLIN != 0 => Ok(false),
            _ => Err(io::Error::other("pidfd poll failed")),
        }
    }
    fn signal(&self, handle: &Self::Handle, signal: i32) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the handle names the recorded process; no PID lookup is used.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                handle.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    /// Each PID's start time and IDs now, the PIDs whose signal fails, and the signals sent.
    struct Table {
        procs: BTreeMap<u32, (u64, u32, u32)>,
        exited: BTreeSet<u32>,
        refusing: BTreeSet<u32>,
        signals: RefCell<Vec<(u32, i32)>>,
    }
    impl Processes for Table {
        type Handle = u32;
        fn open(&self, pid: u32) -> io::Result<u32> {
            self.procs
                .contains_key(&pid)
                .then_some(pid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
        }
        fn identity(&self, pid: u32) -> io::Result<(u64, [u32; 3], [u32; 3])> {
            let (start, uid, gid) = self.procs[&pid];
            Ok((start, [uid; 3], [gid; 3]))
        }
        fn boot_id(&self) -> io::Result<String> {
            Ok("boot".into())
        }
        fn alive(&self, handle: &u32) -> io::Result<bool> {
            Ok(!self.exited.contains(handle))
        }
        fn signal(&self, handle: &u32, signal: i32) -> io::Result<()> {
            if self.refusing.contains(handle) {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            self.signals.borrow_mut().push((*handle, signal));
            Ok(())
        }
    }
    fn table() -> Table {
        Table {
            procs: BTreeMap::from([(10, (100, 1000, 100)), (11, (101, 1000, 100))]),
            exited: BTreeSet::new(),
            refusing: BTreeSet::new(),
            signals: RefCell::new(Vec::new()),
        }
    }
    fn member(pid: u32, start_time: u64) -> FamilyMember {
        FamilyMember { pid, start_time }
    }
    const IDS: (u32, u32) = (1000, 100);

    /// A Guaranteed family is continued, an Opportunistic one killed, each through its handle.
    #[test]
    fn matching_members_are_continued_or_killed() {
        let members = [member(10, 100), member(11, 101)];
        let table = table();
        let recovered = recover_members(&table, &members, "boot", IDS, libc::SIGCONT);
        assert_eq!(recovered.continued, 2);
        assert_eq!(
            *table.signals.borrow(),
            [(10, libc::SIGCONT), (11, libc::SIGCONT)]
        );
        let table = self::table();
        assert_eq!(
            recover_members(&table, &members, "boot", IDS, libc::SIGKILL).ended,
            2
        );
    }

    /// A record whose PID now has another start time, or whose IDs or boot differ, is never
    /// signalled and is counted (Principle I).
    #[test]
    fn a_reused_pid_or_changed_ids_is_never_signalled() {
        let mut table = table();
        table.procs.insert(12, (999, 1000, 100));
        table.procs.insert(13, (103, 0, 100));
        let members = [member(12, 102), member(13, 103), member(10, 100)];
        let recovered = recover_members(&table, &members, "boot", IDS, libc::SIGKILL);
        assert_eq!((recovered.mismatched, recovered.ended), (2, 1));
        assert_eq!(*table.signals.borrow(), [(10, libc::SIGKILL)]);
        let recovered = recover_members(&table, &members, "another boot", IDS, libc::SIGKILL);
        assert_eq!(recovered.gone, 3);
        assert_eq!(table.signals.borrow().len(), 1);
    }

    /// One member's failed signal leaves the others handled; one that exited is counted gone.
    #[test]
    fn a_failing_member_leaves_the_others_handled() {
        let mut table = table();
        table.procs.insert(12, (102, 1000, 100));
        table.refusing.insert(10);
        table.exited.insert(11);
        let members = [
            member(10, 100),
            member(11, 101),
            member(12, 102),
            member(14, 1),
        ];
        let recovered = recover_members(&table, &members, "boot", IDS, libc::SIGCONT);
        assert_eq!(
            recovered,
            Recovered {
                continued: 1,
                ended: 0,
                mismatched: 0,
                gone: 2,
                failed: 1
            }
        );
        assert_eq!(*table.signals.borrow(), [(12, libc::SIGCONT)]);
    }

    fn record(backend: &str) -> ExecutionRecord {
        ExecutionRecord {
            task_id: "t".into(),
            assignment_id: "a".into(),
            generation: 1,
            class: AllocationClass::Guaranteed,
            phase: crate::execution_model::ExecutionPhase::NeedsReconciliation,
            resources: crate::model::Resources::default(),
            identity: Some(crate::execution_model::ProcessIdentity {
                pid: u32::MAX,
                boot_id: "boot".into(),
                start_time: 1,
                assignment_id: "a".into(),
                generation: 1,
            }),
            backend: backend.into(),
            evidence: vec![],
            detail: String::new(),
            modes: Default::default(),
        }
    }

    /// A Full run keeps its leaf and is never touched; recovery runs once per run, and the
    /// family it reads is the one recorded (FR-088).
    #[test]
    fn recovery_runs_once_and_never_for_a_full_run() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            recover_run(dir.path(), &record("cgroup_v2"), &[]).unwrap(),
            None
        );
        assert!(!dir.path().join(RECOVERY_FILE).exists());
        let family = crate::supervision::FamilyReport {
            leader: 1,
            sdk_children: 0,
            descendants: vec![member(u32::MAX - 1, 5)],
            uid: 1000,
            gid: 100,
        };
        record_family(dir.path(), &record("rootless"), &family).unwrap();
        let recorded = recorded_family(dir.path()).unwrap();
        assert_eq!((recorded.uid, recorded.gid), (1000, 100));
        assert_eq!(recorded.descendants, family.descendants);
        if cfg!(target_os = "linux") {
            let first = recover_run(dir.path(), &record("rootless"), &[]).unwrap();
            // The recorded boot is not this one, so nothing is opened or signalled.
            assert!(first.is_some_and(|r| r.gone == 2));
            assert_eq!(
                recover_run(dir.path(), &record("rootless"), &[]).unwrap(),
                None
            );
        }
    }
}
