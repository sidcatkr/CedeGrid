//! A run's verified descendants (FR-083, research R34 to R39). The supervisor is its run's
//! child subreaper; a descendant joins only through Principle I's checks, in their order, and
//! is acted on only through its pidfd. The supervisor forks nothing but its gates, so a child
//! of the supervisor that is neither the leader nor an SDK child gate descends from this run:
//! that is what makes an orphan's chain end at the supervisor, and a later spawn in the
//! supervisor process would break it.
use super::{FamilyMember, OutsideReason};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

/// The most members a run tracks, the leader and SDK children included (R39).
pub(crate) const MEMBER_BOUND: usize = 256;
/// Discoveries in one stop: a stopped process cannot fork, so each pass finds only children
/// forked before their parent stopped (R38).
const STOP_PASSES: usize = 3;

/// The parent and the real, effective and saved IDs from a process's `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status {
    pub ppid: u32,
    pub uids: [u32; 3],
    pub gids: [u32; 3],
}

/// What discovery reads and how members are signalled: the host's `/proc` and pidfds, or a
/// fake in tests.
pub(crate) trait Source {
    type Handle;
    /// The children of every thread of `pid`; `Ok(None)` where the kernel keeps no such list.
    fn children(&self, pid: u32) -> io::Result<Option<BTreeSet<u32>>>;
    /// Every process and its parent, for the scan that stands in for missing lists.
    fn parents(&self) -> io::Result<Vec<(u32, u32)>>;
    fn open(&self, pid: u32) -> io::Result<Self::Handle>;
    fn status(&self, pid: u32) -> io::Result<Status>;
    fn start_time(&self, pid: u32) -> io::Result<u64>;
    fn alive(&self, handle: &Self::Handle) -> io::Result<bool>;
    fn signal(&self, handle: &Self::Handle, signal: i32) -> io::Result<()>;
    /// Reap one exited child of the supervisor by its PID; `false` while it runs.
    fn reap(&self, pid: u32) -> io::Result<bool>;
}

/// A process that has exited, or whose entry is gone, between two reads.
fn gone(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT))
}

pub(crate) struct Member<H> {
    pub pid: u32,
    pub start_time: u64,
    handle: H,
    /// A member, a root, or the supervisor for an orphan.
    pub parent: u32,
}

enum Verdict<H> {
    Joined(Member<H>),
    Outside(OutsideReason),
    /// Exited during the checks, or not this run's process.
    Passed,
}

pub(crate) struct Descendants<S: Source> {
    source: S,
    members: Vec<Member<S::Handle>>,
    supervisor: u32,
    /// The run's user and group IDs.
    ids: (u32, u32),
    /// Boot-time ticks just before EXEC: an orphan started earlier is not this run's.
    since: u64,
    /// Children the supervisor already had at EXEC, by PID and start time: an in-process
    /// supervisor's own, started within the same clock tick as EXEC perhaps, never orphans.
    foreign: BTreeSet<(u32, u64)>,
    /// `begin` ran: nothing is discovered or reaped before EXEC.
    started: bool,
    /// The family is stopped, so a member that joins now is stopped at once.
    stopped: bool,
    /// The scan of `/proc` stood in for missing `children` lists.
    pub fallback: bool,
    /// The last discovery's processes outside the family, each with its reason.
    pub outside: BTreeMap<u32, OutsideReason>,
    /// A discovery error inside a stop, for the loop to report.
    error: Option<anyhow::Error>,
}

impl<S: Source> Descendants<S> {
    pub fn new(source: S, supervisor: u32, ids: (u32, u32), since: u64) -> Self {
        Self {
            source,
            members: Vec::new(),
            supervisor,
            ids,
            since,
            foreign: BTreeSet::new(),
            started: false,
            stopped: false,
            fallback: false,
            outside: BTreeMap::new(),
            error: None,
        }
    }

    /// Called just before EXEC: every descendant of the run starts after `ticks`, and none
    /// is a child the supervisor has now, other than its gates.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn begin(&mut self, ticks: u64, gates: &BTreeSet<u32>) -> Result<()> {
        self.since = ticks;
        let children = match self.source.children(self.supervisor)? {
            Some(children) => children,
            None => self
                .source
                .parents()?
                .into_iter()
                .filter(|(_, ppid)| *ppid == self.supervisor)
                .map(|(pid, _)| pid)
                .collect(),
        };
        for pid in children.difference(gates) {
            match self.source.start_time(*pid) {
                Ok(start) => {
                    self.foreign.insert((*pid, start));
                }
                Err(error) if gone(&error) => {}
                Err(error) => return Err(error).context("cannot read a child's start time"),
            }
        }
        self.started = true;
        Ok(())
    }

    /// A child of the supervisor that is not this run's: it started before EXEC, or was
    /// already there.
    fn foreign(&self, pid: u32, start: u64) -> bool {
        start < self.since || self.foreign.contains(&(pid, start))
    }

    pub fn members(&self) -> impl Iterator<Item = &Member<S::Handle>> {
        self.members.iter()
    }

    pub fn reported(&self) -> Vec<FamilyMember> {
        self.members
            .iter()
            .map(|member| FamilyMember {
                pid: member.pid,
                start_time: member.start_time,
            })
            .collect()
    }

    pub fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }

    /// Find and verify new descendants of `roots` (the leader and running SDK children) and
    /// of members, and orphans under the supervisor. `excluded` holds every gate the
    /// supervisor spawned. Returns how many joined; a member that joins while the family is
    /// stopped is stopped at once. An error holds the family as it is (FR-083).
    pub fn discover(&mut self, roots: &[u32], excluded: &BTreeSet<u32>) -> Result<usize> {
        if !self.started {
            return Ok(0);
        }
        self.drop_exited()?;
        // Breadth first, so a member's children are found in the same discovery.
        let mut queue: std::collections::VecDeque<u32> = roots.iter().copied().collect();
        queue.extend(self.members.iter().map(|member| member.pid));
        queue.push_back(self.supervisor);
        let mut listed = BTreeSet::new();
        let mut scan: Option<Vec<(u32, u32)>> = None;
        let mut outside = BTreeMap::new();
        let mut joined = 0;
        while let Some(parent) = queue.pop_front() {
            if !listed.insert(parent) {
                continue;
            }
            let children = match self.source.children(parent) {
                Ok(Some(children)) => children,
                Ok(None) => {
                    self.fallback = true;
                    if scan.is_none() {
                        scan = Some(
                            self.source
                                .parents()
                                .context("cannot scan /proc for children")?,
                        );
                    }
                    scan.iter()
                        .flatten()
                        .filter(|(_, ppid)| *ppid == parent)
                        .map(|(pid, _)| *pid)
                        .collect()
                }
                Err(error) if gone(&error) => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot list children of {parent}"));
                }
            };
            for pid in children {
                if excluded.contains(&pid)
                    || roots.contains(&pid)
                    || outside.contains_key(&pid)
                    || self.members.iter().any(|member| member.pid == pid)
                {
                    continue;
                }
                if roots.len() + self.members.len() >= MEMBER_BOUND {
                    outside.insert(pid, OutsideReason::OverBound);
                    continue;
                }
                match self.verify(pid, roots)? {
                    Verdict::Joined(member) => {
                        if self.stopped {
                            self.signal_one(&member, libc::SIGSTOP)?;
                        }
                        queue.push_back(member.pid);
                        self.members.push(member);
                        joined += 1;
                    }
                    Verdict::Outside(reason) => {
                        outside.insert(pid, reason);
                    }
                    Verdict::Passed => {}
                }
            }
        }
        self.outside = outside;
        Ok(joined)
    }

    /// Principle I's order: a pidfd first; then the parent chain, read after it; then alive
    /// through the pidfd; then the run's real, effective and saved IDs.
    fn verify(&self, pid: u32, roots: &[u32]) -> Result<Verdict<S::Handle>> {
        let read = |result: io::Result<()>| -> Result<bool> {
            match result {
                Ok(()) => Ok(true),
                Err(error) if gone(&error) => Ok(false),
                Err(error) => Err(error).with_context(|| format!("cannot verify process {pid}")),
            }
        };
        let mut handle = None;
        if !read(self.source.open(pid).map(|h| handle = Some(h)))? {
            return Ok(Verdict::Passed);
        }
        let handle = handle.expect("opened");
        let (mut status, mut start_time) = (None, 0);
        if !read(self.source.status(pid).map(|s| status = Some(s)))?
            || !read(self.source.start_time(pid).map(|t| start_time = t))?
        {
            return Ok(Verdict::Passed);
        }
        let status = status.expect("read");
        let chained = if status.ppid == self.supervisor {
            // An orphan: only this run's descendants are reparented to its supervisor. A
            // child it had before EXEC is not one of them.
            if self.foreign(pid, start_time) {
                return Ok(Verdict::Passed);
            }
            true
        } else if roots.contains(&status.ppid) {
            true
        } else {
            match self.members.iter().find(|m| m.pid == status.ppid) {
                Some(parent) => self.source.alive(&parent.handle)?,
                None => false,
            }
        };
        if !self.source.alive(&handle)? {
            return Ok(Verdict::Passed);
        }
        if !chained {
            return Ok(Verdict::Outside(OutsideReason::ChainUnconfirmed));
        }
        let (uid, gid) = self.ids;
        if status.uids != [uid; 3] || status.gids != [gid; 3] {
            return Ok(Verdict::Outside(OutsideReason::IdsDiffer));
        }
        Ok(Verdict::Joined(Member {
            pid,
            start_time,
            handle,
            parent: status.ppid,
        }))
    }

    /// Members whose pidfd shows exit leave the family; that is not an error.
    fn drop_exited(&mut self) -> Result<()> {
        let mut kept = Vec::with_capacity(self.members.len());
        for member in self.members.drain(..) {
            if self.source.alive(&member.handle)? {
                kept.push(member);
            }
        }
        self.members = kept;
        Ok(())
    }

    fn signal_one(&self, member: &Member<S::Handle>, signal: i32) -> Result<bool> {
        match self.source.signal(&member.handle, signal) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("signal {signal} to member {}", member.pid))
            }
        }
    }

    /// Signal every member through its pidfd, past a failing one; a member that has exited
    /// is dropped. The first failure is returned after the rest.
    pub fn signal(&mut self, signal: i32) -> Result<()> {
        let mut result = Ok(());
        let mut exited = BTreeSet::new();
        for member in &self.members {
            match self.signal_one(member, signal) {
                Ok(true) => {}
                Ok(false) => {
                    exited.insert(member.pid);
                }
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        self.members.retain(|member| !exited.contains(&member.pid));
        match signal {
            libc::SIGSTOP => self.stopped = true,
            libc::SIGCONT => self.stopped = false,
            _ => {}
        }
        result
    }

    /// End members at run end: SIGTERM once to each, or SIGKILL to every one left. Each member
    /// signalled is counted in `ended`; one that has exited is dropped (FR-087).
    pub fn end(&mut self, signal: i32, ended: &mut BTreeSet<(u32, u64)>) -> Result<()> {
        let mut result = Ok(());
        let mut exited = BTreeSet::new();
        for member in &self.members {
            let key = (member.pid, member.start_time);
            if signal == libc::SIGTERM && ended.contains(&key) {
                continue;
            }
            match self.signal_one(member, signal) {
                Ok(true) => {
                    ended.insert(key);
                }
                Ok(false) => {
                    exited.insert(member.pid);
                }
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        self.members.retain(|member| !exited.contains(&member.pid));
        result
    }

    /// Stop the whole family: every member, then members found since, until a pass finds
    /// none, at most three passes. A discovery error stops the passes and is kept for the
    /// loop to report; the members already found stay stopped (FR-086).
    pub fn stop(&mut self, roots: &[u32], excluded: &BTreeSet<u32>) -> Result<()> {
        let result = self.signal(libc::SIGSTOP);
        for _ in 0..STOP_PASSES {
            match self.discover(roots, excluded) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => {
                    self.error = Some(error);
                    break;
                }
            }
        }
        result
    }

    /// Reap exited orphans by their own PIDs: only children of the supervisor that are not
    /// one of its gates and started after EXEC; never a wait for any child (R36).
    pub fn reap_orphans(&self, excluded: &BTreeSet<u32>) -> Result<usize> {
        if !self.started {
            return Ok(0);
        }
        let children = match self.source.children(self.supervisor)? {
            Some(children) => children,
            None => self
                .source
                .parents()?
                .into_iter()
                .filter(|(_, ppid)| *ppid == self.supervisor)
                .map(|(pid, _)| pid)
                .collect(),
        };
        let mut reaped = 0;
        for pid in children.difference(excluded) {
            match self.source.start_time(*pid) {
                Ok(start) if !self.foreign(*pid, start) => {}
                Ok(_) => continue,
                Err(error) if gone(&error) => continue,
                Err(error) => return Err(error).context("cannot read an orphan's start time"),
            }
            if self.source.reap(*pid)? {
                reaped += 1;
            }
        }
        Ok(reaped)
    }
}

/// The host's `/proc` and pidfds.
#[derive(Default)]
pub(crate) struct HostSource;

#[cfg(target_os = "linux")]
mod host {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn proc_number(text: &str, field: usize) -> Option<u64> {
        text.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(field)?
            .parse()
            .ok()
    }

    /// Whether this kernel keeps `/proc/<pid>/task/<tid>/children` lists, which need
    /// `CONFIG_PROC_CHILDREN`; without them discovery scans `/proc` (FR-083).
    pub(crate) fn children_lists() -> bool {
        std::path::Path::new(&format!(
            "/proc/self/task/{}/children",
            // SAFETY: gettid has no preconditions.
            unsafe { libc::gettid() }
        ))
        .exists()
    }

    impl Source for HostSource {
        type Handle = OwnedFd;
        fn children(&self, pid: u32) -> io::Result<Option<BTreeSet<u32>>> {
            if !children_lists() {
                return Ok(None);
            }
            let mut children = BTreeSet::new();
            for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
                match std::fs::read_to_string(task?.path().join("children")) {
                    Ok(text) => children.extend(crate::supervision::parse_children(&text)),
                    // A thread that exits between the listing and the read.
                    Err(error) if gone(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(Some(children))
        }
        fn parents(&self) -> io::Result<Vec<(u32, u32)>> {
            let mut parents = Vec::new();
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                    continue;
                };
                match std::fs::read_to_string(entry.path().join("stat")) {
                    Ok(stat) => {
                        if let Some(ppid) = proc_number(&stat, 1) {
                            parents.push((pid, ppid as u32));
                        }
                    }
                    Err(error) if gone(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(parents)
        }
        fn open(&self, pid: u32) -> io::Result<OwnedFd> {
            // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a successful pidfd_open returns a newly owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
        }
        fn status(&self, pid: u32) -> io::Result<Status> {
            let text = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
            let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid /proc status");
            let ids = |name: &str| -> io::Result<[u32; 3]> {
                let line = text
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .ok_or_else(invalid)?;
                let mut values = line.split_whitespace().map(str::parse::<u32>);
                let mut next = || values.next().and_then(|v| v.ok()).ok_or_else(invalid);
                Ok([next()?, next()?, next()?])
            };
            let ppid = text
                .lines()
                .find_map(|line| line.strip_prefix("PPid:"))
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(invalid)?;
            Ok(Status {
                ppid,
                uids: ids("Uid:")?,
                gids: ids("Gid:")?,
            })
        }
        fn start_time(&self, pid: u32) -> io::Result<u64> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
            proc_number(&stat, 19)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat"))
        }
        fn alive(&self, handle: &OwnedFd) -> io::Result<bool> {
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
        fn signal(&self, handle: &OwnedFd, signal: i32) -> io::Result<()> {
            // SAFETY: the handle names the verified member; no PID lookup is used.
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
        fn reap(&self, pid: u32) -> io::Result<bool> {
            let mut status = 0;
            // SAFETY: a wait for one listed child of this process, never for any child.
            match unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) } {
                0 => Ok(false),
                rc if rc > 0 => Ok(true),
                _ => {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                }
            }
        }
    }

    /// Boot-time clock ticks, the clock of `/proc/<pid>/stat` start times.
    pub(crate) fn boot_ticks() -> Result<u64> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: one valid timespec.
        anyhow::ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0,
            "cannot read the boot-time clock"
        );
        // SAFETY: sysconf reads a constant.
        let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })?;
        Ok(u64::try_from(time.tv_sec)? * hz + u64::try_from(time.tv_nsec)? * hz / 1_000_000_000)
    }

    /// The supervisor's own user and group IDs, which its run carries.
    pub(crate) fn run_ids() -> (u32, u32) {
        // SAFETY: getuid and getgid cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }
}
#[cfg(target_os = "linux")]
pub(crate) use host::{boot_ticks, children_lists, run_ids};

/// Elsewhere nothing is tracked: the single-process contract stays required (FR-084).
#[cfg(not(target_os = "linux"))]
impl Source for HostSource {
    type Handle = ();
    fn children(&self, _pid: u32) -> io::Result<Option<BTreeSet<u32>>> {
        Ok(Some(BTreeSet::new()))
    }
    fn parents(&self) -> io::Result<Vec<(u32, u32)>> {
        Ok(Vec::new())
    }
    fn open(&self, _pid: u32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }
    fn status(&self, _pid: u32) -> io::Result<Status> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }
    fn start_time(&self, _pid: u32) -> io::Result<u64> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }
    fn alive(&self, _handle: &()) -> io::Result<bool> {
        Ok(false)
    }
    fn signal(&self, _handle: &(), _signal: i32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ESRCH))
    }
    fn reap(&self, _pid: u32) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A process table: each PID's parent, IDs and start time, and the signals sent.
    #[derive(Default)]
    struct Table {
        procs: BTreeMap<u32, (u32, [u32; 3], [u32; 3], u64)>,
        exited: BTreeSet<u32>,
        /// Members whose signals fail with something other than an exit.
        refusing: BTreeSet<u32>,
        /// Alive at the next look only, then exited.
        dying: RefCell<BTreeSet<u32>>,
        lists: bool,
        broken: bool,
        signals: RefCell<Vec<(u32, i32)>>,
        reaped: RefCell<Vec<u32>>,
    }
    impl Table {
        fn spawn(&mut self, pid: u32, ppid: u32, uid: u32) {
            self.procs.insert(pid, (ppid, [uid; 3], [100; 3], 50));
        }
    }
    impl Source for Table {
        type Handle = u32;
        fn children(&self, pid: u32) -> io::Result<Option<BTreeSet<u32>>> {
            if self.broken {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            Ok(self.lists.then(|| {
                self.procs
                    .iter()
                    .filter(|(_, p)| p.0 == pid)
                    .map(|(c, _)| *c)
                    .collect()
            }))
        }
        fn parents(&self) -> io::Result<Vec<(u32, u32)>> {
            Ok(self.procs.iter().map(|(c, p)| (*c, p.0)).collect())
        }
        fn open(&self, pid: u32) -> io::Result<u32> {
            self.procs
                .contains_key(&pid)
                .then_some(pid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
        }
        fn status(&self, pid: u32) -> io::Result<Status> {
            let p = self.procs[&pid];
            Ok(Status {
                ppid: p.0,
                uids: p.1,
                gids: p.2,
            })
        }
        fn start_time(&self, pid: u32) -> io::Result<u64> {
            Ok(self.procs[&pid].3)
        }
        fn alive(&self, handle: &u32) -> io::Result<bool> {
            if self.dying.borrow_mut().remove(handle) {
                return Ok(true);
            }
            Ok(!self.exited.contains(handle))
        }
        fn signal(&self, handle: &u32, signal: i32) -> io::Result<()> {
            if self.exited.contains(handle) {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if self.refusing.contains(handle) {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            self.signals.borrow_mut().push((*handle, signal));
            Ok(())
        }
        fn reap(&self, pid: u32) -> io::Result<bool> {
            self.reaped.borrow_mut().push(pid);
            Ok(self.exited.contains(&pid))
        }
    }

    const SUPERVISOR: u32 = 1;
    const LEADER: u32 = 10;

    fn family(table: Table) -> Descendants<Table> {
        let mut family = Descendants::new(table, SUPERVISOR, (1000, 100), 40);
        family.started = true;
        family
    }
    fn tree() -> Table {
        let mut table = Table {
            lists: true,
            ..Table::default()
        };
        table.spawn(LEADER, SUPERVISOR, 1000);
        table
    }
    fn pids(family: &Descendants<Table>) -> Vec<u32> {
        let mut pids: Vec<u32> = family.members().map(|m| m.pid).collect();
        pids.sort_unstable();
        pids
    }

    /// Children and grandchildren with the run's IDs join; an orphan under the supervisor
    /// joins with the supervisor as its chain's root (FR-083).
    #[test]
    fn descendants_and_orphans_join() {
        let mut table = tree();
        table.spawn(11, LEADER, 1000);
        table.spawn(12, 11, 1000);
        table.spawn(13, SUPERVISOR, 1000);
        let mut family = family(table);
        let excluded = BTreeSet::from([LEADER]);
        assert_eq!(family.discover(&[LEADER], &excluded).unwrap(), 3);
        assert_eq!(pids(&family), [11, 12, 13]);
        assert_eq!(family.discover(&[LEADER], &excluded).unwrap(), 0);
        assert!(family.outside.is_empty());
    }

    /// A descendant whose IDs differ is outside the family with its reason and is never
    /// signalled; neither is a process whose chain is not confirmed (FR-082, Constitution I).
    #[test]
    fn a_descendant_with_other_ids_is_never_signalled() {
        let mut table = tree();
        table.spawn(11, LEADER, 1000);
        table.spawn(12, LEADER, 0);
        table.procs.get_mut(&11).unwrap().2 = [100, 0, 100];
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        assert!(pids(&family).is_empty());
        assert_eq!(
            family.outside,
            BTreeMap::from([
                (11, OutsideReason::IdsDiffer),
                (12, OutsideReason::IdsDiffer)
            ])
        );
        family.stop(&[LEADER], &BTreeSet::new()).unwrap();
        family.signal(libc::SIGCONT).unwrap();
        family.signal(libc::SIGKILL).unwrap();
        assert!(family.source.signals.borrow().is_empty());
    }

    /// A child found under a member that exits before the check has no confirmed chain; it
    /// joins as an orphan once it is reparented to the supervisor.
    #[test]
    fn a_child_of_an_exiting_member_is_not_confirmed() {
        let mut table = tree();
        table.spawn(11, LEADER, 1000);
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        family.source.spawn(12, 11, 1000);
        family.source.exited.insert(11);
        family.source.dying.borrow_mut().insert(11);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        assert_eq!(pids(&family), [11], "dropped at the next look");
        assert_eq!(
            family.outside,
            BTreeMap::from([(12, OutsideReason::ChainUnconfirmed)])
        );
        family.source.procs.get_mut(&12).unwrap().0 = SUPERVISOR;
        assert_eq!(family.discover(&[LEADER], &BTreeSet::new()).unwrap(), 1);
        assert_eq!(pids(&family), [12]);
    }

    /// A child of the supervisor that started before EXEC, or that it already had then even
    /// within the same clock tick, is not this run's: it neither joins nor is reaped; the
    /// gates the supervisor spawned are never adopted.
    #[test]
    fn the_supervisors_own_children_are_not_orphans() {
        let mut table = tree();
        table.spawn(20, SUPERVISOR, 1000);
        table.procs.get_mut(&20).unwrap().3 = 10;
        table.spawn(22, SUPERVISOR, 1000);
        table.procs.get_mut(&22).unwrap().3 = 40;
        table.spawn(21, SUPERVISOR, 1000);
        table.exited.insert(20);
        table.exited.insert(22);
        let mut family = family(table);
        let gates = BTreeSet::from([LEADER, 21]);
        family.begin(40, &gates).unwrap();
        assert_eq!(family.discover(&[LEADER], &gates).unwrap(), 0);
        assert!(family.outside.is_empty());
        family.reap_orphans(&gates).unwrap();
        assert!(family.source.reaped.borrow().is_empty());
    }

    /// Orphans are reaped by their own PID, the leader and SDK gates never.
    #[test]
    fn orphans_are_reaped_by_pid_only() {
        let mut table = tree();
        table.spawn(13, SUPERVISOR, 1000);
        table.spawn(14, SUPERVISOR, 1000);
        table.spawn(21, SUPERVISOR, 1000);
        table.exited.insert(13);
        let family = family(table);
        assert_eq!(
            family.reap_orphans(&BTreeSet::from([LEADER, 21])).unwrap(),
            1
        );
        assert_eq!(*family.source.reaped.borrow(), [13, 14]);
    }

    /// Without `children` lists a scan of `/proc` stands in, and says so.
    #[test]
    fn the_parent_scan_stands_in_for_missing_lists() {
        let mut table = tree();
        table.lists = false;
        table.spawn(11, LEADER, 1000);
        let mut family = family(table);
        assert_eq!(family.discover(&[LEADER], &BTreeSet::new()).unwrap(), 1);
        assert!(family.fallback);
    }

    /// A discovery error holds the family as it is; inside a stop the members found are
    /// still stopped and the error is kept for the loop to report once (FR-083).
    #[test]
    fn a_discovery_error_holds_the_family() {
        let mut table = tree();
        table.spawn(11, LEADER, 1000);
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        family.source.broken = true;
        assert!(family.discover(&[LEADER], &BTreeSet::new()).is_err());
        assert_eq!(pids(&family), [11]);
        family.stop(&[LEADER], &BTreeSet::new()).unwrap();
        assert_eq!(*family.source.signals.borrow(), [(11, libc::SIGSTOP)]);
        assert!(family.take_error().is_some());
        assert!(family.take_error().is_none());
    }

    /// At most 256 members, the leader included: the rest are over the bound, never opened.
    #[test]
    fn members_past_the_bound_are_reported_not_tracked() {
        let mut table = tree();
        for pid in 100..400 {
            table.spawn(pid, LEADER, 1000);
        }
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        assert_eq!(family.members().count(), MEMBER_BOUND - 1);
        assert_eq!(family.outside.len(), 300 - (MEMBER_BOUND - 1));
        assert!(
            family
                .outside
                .values()
                .all(|reason| *reason == OutsideReason::OverBound)
        );
    }

    /// A stop reaches every member and members forked before their parent stopped; a member
    /// that joins while the family is stopped is stopped at once; continue reaches everyone
    /// past an exited member, which is dropped (FR-086).
    #[test]
    fn a_stop_reaches_the_whole_family() {
        let mut table = tree();
        table.spawn(11, LEADER, 1000);
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        family.source.spawn(12, 11, 1000);
        family.stop(&[LEADER], &BTreeSet::new()).unwrap();
        family.source.spawn(13, 12, 1000);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        assert_eq!(
            *family.source.signals.borrow(),
            [
                (11, libc::SIGSTOP),
                (12, libc::SIGSTOP),
                (13, libc::SIGSTOP)
            ]
        );
        family.source.exited.insert(12);
        family.source.signals.borrow_mut().clear();
        family.signal(libc::SIGCONT).unwrap();
        assert_eq!(
            *family.source.signals.borrow(),
            [(11, libc::SIGCONT), (13, libc::SIGCONT)]
        );
        assert_eq!(pids(&family), [11, 13]);
    }

    /// Continue goes on past a member whose signal fails, which is returned to be reported;
    /// the member stays in the family (FR-086).
    #[test]
    fn continue_goes_past_a_failing_member() {
        let mut table = tree();
        for pid in [11, 12, 13] {
            table.spawn(pid, LEADER, 1000);
        }
        table.refusing.insert(12);
        let mut family = family(table);
        family.discover(&[LEADER], &BTreeSet::new()).unwrap();
        assert!(family.signal(libc::SIGCONT).is_err());
        assert_eq!(
            *family.source.signals.borrow(),
            [(11, libc::SIGCONT), (13, libc::SIGCONT)]
        );
        assert_eq!(pids(&family), [11, 12, 13]);
    }
}
