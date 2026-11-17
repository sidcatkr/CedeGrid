//! The broker's OOM pass (FR-022 to FR-026, research R10, T047, T048).
//!
//! This file holds the pure steps of one pass: memory by user, principals, fair shares, tasks
//! inside the units and the writes for each user. Reading `/proc` and the units' cgroups, and
//! writing through process handles, is the Linux loop's job.

use super::ModeName;
use super::score::{
    Lowering, OverShareMarking, ProcessKey, ScoreInput, ScoreWrite, ScoredProcess, ScoredTask,
    UserMemory, score_writes,
};
use super::units::UnitKind;
use crate::contention::max_min_shares;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// How long a user's memory must stay at or above the threshold to become a principal
/// (*provisional*; 001 FR-059's window at the broker's one pass per second).
pub const PRINCIPAL_JOIN: Duration = Duration::from_secs(3);

/// How long a principal's memory must stay below the threshold to leave (001 FR-059 default).
pub const PRINCIPAL_LEAVE: Duration = Duration::from_secs(60);

/// One process as one pass read it from `/proc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSample {
    /// The real user ID, or `None` when it could not be read.
    pub uid: Option<u32>,
    /// Resident plus swapped pages, or `None` when they could not be read.
    pub pages: Option<u64>,
}

/// Memory per user in pages. A user with any unreadable process is unknown (`None`).
/// Processes whose owner could not be read are summed apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserMemoryMap {
    pub by_user: BTreeMap<u32, Option<u64>>,
    /// Pages of processes whose owner could not be read; `None` if any of them was unreadable.
    pub unattributed: Option<u64>,
}

/// Sums samples by user.
pub fn memory_by_user(samples: &[ProcessSample]) -> UserMemoryMap {
    let mut map = UserMemoryMap {
        by_user: BTreeMap::new(),
        unattributed: Some(0),
    };
    for sample in samples {
        let slot = match sample.uid {
            Some(uid) => map.by_user.entry(uid).or_insert(Some(0)),
            None => &mut map.unattributed,
        };
        *slot = match (*slot, sample.pages) {
            (Some(total), Some(pages)) => Some(total.saturating_add(pages)),
            _ => None,
        };
    }
    map
}

/// Principal membership over time (001 FR-059): user IDs at or above `uid_min` whose memory has
/// stayed at or above `principal_min_percent` of capacity join after `PRINCIPAL_JOIN`, and leave
/// after `PRINCIPAL_LEAVE` below it. Unknown memory counts as at the threshold (Principle III).
#[derive(Clone, Debug, Default)]
pub struct Principals {
    above_since: BTreeMap<u32, Instant>,
    below_since: BTreeMap<u32, Instant>,
    members: BTreeSet<u32>,
}

impl Principals {
    pub fn members(&self) -> &BTreeSet<u32> {
        &self.members
    }

    pub fn update(
        &mut self,
        now: Instant,
        memory: &UserMemoryMap,
        capacity_pages: u64,
        uid_min: u32,
        principal_min_percent: u8,
    ) -> &BTreeSet<u32> {
        let threshold = capacity_pages.saturating_mul(u64::from(principal_min_percent)) / 100;
        let mut seen = BTreeSet::new();
        for (&uid, &pages) in &memory.by_user {
            if uid < uid_min {
                continue;
            }
            seen.insert(uid);
            let above = pages.is_none_or(|pages| pages >= threshold);
            if above {
                self.below_since.remove(&uid);
                let since = *self.above_since.entry(uid).or_insert(now);
                if now.saturating_duration_since(since) >= PRINCIPAL_JOIN {
                    self.members.insert(uid);
                }
            } else {
                self.above_since.remove(&uid);
                if self.members.contains(&uid) {
                    let since = *self.below_since.entry(uid).or_insert(now);
                    if now.saturating_duration_since(since) >= PRINCIPAL_LEAVE {
                        self.members.remove(&uid);
                        self.below_since.remove(&uid);
                    }
                }
            }
        }
        // A user with no process left has no memory and leaves at once.
        self.members.retain(|uid| seen.contains(uid));
        self.above_since.retain(|uid, _| seen.contains(uid));
        self.below_since.retain(|uid, _| seen.contains(uid));
        &self.members
    }
}

/// Each principal's memory fair share in pages: 001's max-min over current use, of the capacity
/// left after every non-principal's memory. An unknown principal is taken to want without bound,
/// so it never makes another principal's share larger.
pub fn fair_shares(
    capacity_pages: u64,
    memory: &UserMemoryMap,
    principals: &BTreeSet<u32>,
) -> BTreeMap<u32, u64> {
    let others: u64 = memory
        .by_user
        .iter()
        .filter(|(uid, _)| !principals.contains(uid))
        .map(|(_, pages)| pages.unwrap_or(0))
        .chain(std::iter::once(memory.unattributed.unwrap_or(0)))
        .fold(0u64, u64::saturating_add);
    let capacity = capacity_pages.saturating_sub(others);
    let order: Vec<u32> = principals.iter().copied().collect();
    let demands: Vec<Option<u64>> = order
        .iter()
        .map(|uid| memory.by_user.get(uid).copied().flatten())
        .collect();
    order
        .into_iter()
        .zip(max_min_shares(capacity, &demands))
        .collect()
}

/// Whether a principal other than `uid` holds more memory than its fair share. Unknown memory is
/// not "over": lowering is allowed only on a known reason (N1 B).
pub fn another_over_share(uid: u32, memory: &UserMemoryMap, shares: &BTreeMap<u32, u64>) -> bool {
    shares.iter().any(|(&other, &share)| {
        other != uid
            && memory
                .by_user
                .get(&other)
                .copied()
                .flatten()
                .is_some_and(|pages| pages > share)
    })
}

/// A process found inside one of a user's units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitProcess {
    pub uid: u32,
    pub kind: UnitKind,
    /// The first cgroup below the unit's own, which is one task's leaf; `None` for a process in
    /// the unit's own cgroup.
    pub leaf: Option<String>,
    pub key: ProcessKey,
    pub pages: u64,
    pub current: i32,
}

/// Where `process_cgroup` sits relative to `unit_cgroup`: `None` when outside the unit, else the
/// first component below it (`Some(None)` for the unit's own cgroup).
pub fn leaf_in_unit(unit_cgroup: &str, process_cgroup: &str) -> Option<Option<String>> {
    if process_cgroup == unit_cgroup {
        return Some(None);
    }
    let rest = process_cgroup
        .strip_prefix(unit_cgroup)?
        .strip_prefix('/')?;
    let first = rest.split('/').next().filter(|part| !part.is_empty())?;
    Some(Some(first.to_owned()))
}

/// A task's mode. The idle unit holds Yield First work only. In the ordinary unit the agent names
/// each task's leaf `cedegrid-<mode>-<id>`; a leaf without a readable mode counts as Neutral, so
/// it is marked when over its share and the cost stays with own work.
pub fn task_mode(kind: UnitKind, leaf: Option<&str>) -> ModeName {
    if kind == UnitKind::Idle {
        return ModeName::YieldFirst;
    }
    leaf.and_then(parse_leaf)
        .map(|(mode, _)| mode)
        .unwrap_or(ModeName::Neutral)
}

/// The mode and job of a task leaf named `cedegrid-<mode>-<job>.<nonce>` (the nonce optional).
/// Mode names hold no `-` and job IDs no `.`, so both split points are unambiguous.
pub fn parse_leaf(leaf: &str) -> Option<(ModeName, &str)> {
    let (mode, rest) = leaf.strip_prefix("cedegrid-")?.split_once('-')?;
    let job = rest.split_once('.').map_or(rest, |(job, _)| job);
    let mode = ModeName::from_name(mode)?;
    super::request::JobId::parse(job).map(|_| (mode, job))
}

/// Groups one user's unit processes into tasks, one per leaf, each started at its oldest process.
pub fn group_tasks(processes: &[UnitProcess]) -> Vec<ScoredTask> {
    let mut tasks: BTreeMap<(UnitKind, Option<String>), Vec<&UnitProcess>> = BTreeMap::new();
    for process in processes {
        tasks
            .entry((process.kind, process.leaf.clone()))
            .or_default()
            .push(process);
    }
    tasks
        .into_iter()
        .map(|((kind, leaf), members)| ScoredTask {
            started: members
                .iter()
                .map(|process| process.key.start_time)
                .min()
                .unwrap_or(0),
            mode: task_mode(kind, leaf.as_deref()),
            processes: members
                .iter()
                .map(|process| ScoredProcess {
                    key: process.key,
                    pages: process.pages,
                    current: process.current,
                })
                .collect(),
        })
        .collect()
}

/// What one pass knows about the host.
#[derive(Clone, Debug)]
pub struct PassView<'a> {
    /// The kernel's total pages for the OOM score: RAM plus swap.
    pub total_pages: u64,
    pub memory: &'a UserMemoryMap,
    pub shares: &'a BTreeMap<u32, u64>,
    pub lowering: Lowering,
    pub marking: OverShareMarking,
}

/// The writes for one user's unit processes.
///
/// `unknown_for` is how long this user's memory has been unknown, if it is unknown now. A user
/// who is not a principal has no fair share; none of their memory is lowered, and none is marked.
pub fn user_writes(
    view: &PassView<'_>,
    uid: u32,
    processes: &[UnitProcess],
    unknown_for: Option<Duration>,
) -> Vec<ScoreWrite> {
    let tasks = group_tasks(processes);
    let memory = match (unknown_for, view.shares.get(&uid)) {
        (Some(for_how_long), _) => UserMemory::Unknown { for_how_long },
        (None, Some(&fair_share_pages)) => UserMemory::Known { fair_share_pages },
        // Not a principal: the whole of its use is within what it may hold.
        (None, None) => UserMemory::Known {
            fair_share_pages: u64::MAX,
        },
    };
    let lowering = if view.shares.contains_key(&uid) {
        view.lowering
    } else {
        Lowering::Never
    };
    score_writes(&ScoreInput {
        total_pages: view.total_pages,
        memory,
        another_over_share: another_over_share(uid, view.memory, view.shares),
        lowering,
        marking: view.marking,
        tasks: &tasks,
    })
}

/// A process the loop holds while it reads and writes it.
pub trait HeldProcess {
    fn start_time(&self) -> u64;
    fn cgroup(&self) -> anyhow::Result<String>;
    fn ids(&self) -> anyhow::Result<super::process_handle::ProcessIds>;
    fn oom_score_adj(&self) -> anyhow::Result<i32>;
    fn set_oom_score_adj(&self, value: i32) -> anyhow::Result<()>;
}

/// Opens processes for the loop.
pub trait ProcessAccess {
    /// Opens `pid`; with `start_time`, refuses a process that started at another time.
    fn open(&self, pid: u32, start_time: Option<u64>) -> anyhow::Result<Box<dyn HeldProcess>>;
}

#[cfg(target_os = "linux")]
impl HeldProcess for super::process_handle::ProcessHandle {
    fn start_time(&self) -> u64 {
        super::process_handle::ProcessHandle::start_time(self)
    }

    fn cgroup(&self) -> anyhow::Result<String> {
        super::process_handle::ProcessHandle::cgroup(self)
    }

    fn ids(&self) -> anyhow::Result<super::process_handle::ProcessIds> {
        super::process_handle::ProcessHandle::ids(self)
    }

    fn oom_score_adj(&self) -> anyhow::Result<i32> {
        super::process_handle::ProcessHandle::oom_score_adj(self)
    }

    fn set_oom_score_adj(&self, value: i32) -> anyhow::Result<()> {
        super::process_handle::ProcessHandle::set_oom_score_adj(self, value)
    }
}

/// Processes held through pidfds and their `/proc/<pid>` directories.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
pub struct PidfdAccess;

#[cfg(target_os = "linux")]
impl ProcessAccess for PidfdAccess {
    fn open(&self, pid: u32, start_time: Option<u64>) -> anyhow::Result<Box<dyn HeldProcess>> {
        Ok(Box::new(super::process_handle::ProcessHandle::open(
            pid, start_time,
        )?))
    }
}

/// One process in the host scan: its start time and memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScannedProcess {
    pub start_time: u64,
    pub sample: ProcessSample,
}

/// What one pass read from the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostReadings {
    /// RAM plus swap in pages, the kernel's total for the OOM score.
    pub total_pages: u64,
    /// RAM in pages, the capacity the fair shares divide.
    pub capacity_pages: u64,
    pub processes: BTreeMap<u32, ScannedProcess>,
}

/// `MemTotal` and `SwapTotal` from `/proc/meminfo`, in kB.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    Some((field("MemTotal:")?, field("SwapTotal:").unwrap_or(0)))
}

/// Whether memory is plentiful: at least a quarter of RAM available and swap use under 5% of
/// RAM. Then total use is below RAM, so no principal can be over its share. Unreadable counts
/// are not plentiful.
pub fn memory_is_plentiful(meminfo: &str) -> bool {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    let (Some(total), Some(available)) = (field("MemTotal:"), field("MemAvailable:")) else {
        return false;
    };
    let swap_used = field("SwapTotal:")
        .unwrap_or(0)
        .saturating_sub(field("SwapFree:").unwrap_or(0));
    total > 0 && available.saturating_mul(4) >= total && swap_used.saturating_mul(20) <= total
}

/// The real user ID and resident plus swapped kB from a `/proc/<pid>/status` text. A process
/// without `VmRSS` (a kernel thread) has no memory of its own.
pub fn parse_status_memory(text: &str) -> (Option<u32>, Option<u64>) {
    let uid = text
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u32>().ok());
    let kb = |name: &str| -> Option<Option<u64>> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
            })
    };
    let memory = match (kb("VmRSS:"), kb("VmSwap:")) {
        (None, _) => Some(0),
        (Some(Some(rss)), None) => Some(rss),
        (Some(Some(rss)), Some(Some(swap))) => Some(rss.saturating_add(swap)),
        _ => None,
    };
    (uid, memory)
}

/// Reads the host: memory totals and every process's owner, start time and memory, from
/// `proc_root` (normally `/proc`). A process that exits during the scan is left out.
pub fn read_host(proc_root: &std::path::Path, page_kb: u64) -> anyhow::Result<HostReadings> {
    use anyhow::Context;
    let page_kb = page_kb.max(1);
    let meminfo =
        std::fs::read_to_string(proc_root.join("meminfo")).context("cannot read meminfo")?;
    let (mem_kb, swap_kb) = parse_meminfo(&meminfo).context("meminfo has no MemTotal")?;
    let mut processes = BTreeMap::new();
    for entry in std::fs::read_dir(proc_root).context("cannot list processes")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let stat = match std::fs::read_to_string(dir.join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => String::new(),
        };
        let Some(start_time) = super::process_handle::parse_start_time(&stat) else {
            // Gone, or unreadable: without a start time the process cannot be matched later.
            continue;
        };
        let (uid, kb) = match std::fs::read_to_string(dir.join("status")) {
            Ok(status) => parse_status_memory(&status),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => (None, None),
        };
        processes.insert(
            pid,
            ScannedProcess {
                start_time,
                sample: ProcessSample {
                    uid,
                    pages: kb.map(|kb| kb / page_kb),
                },
            },
        );
    }
    Ok(HostReadings {
        total_pages: (mem_kb + swap_kb) / page_kb,
        capacity_pages: mem_kb / page_kb,
        processes,
    })
}

/// A unit of the broker's the loop found in the cgroup tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundUnit {
    pub kind: UnitKind,
    pub uid: u32,
    /// The unit's cgroup path, as `/proc/<pid>/cgroup` shows it.
    pub cgroup: String,
    /// The cgroup directory's inode, the kernel's cgroup ID: a new unit of the same name has
    /// another, so its marker is checked again.
    pub id: u64,
}

/// The broker's units under `cgroup_root` (normally `/sys/fs/cgroup`), by their fixed names and
/// slices. Whether each one carries the broker's marker is checked separately.
pub fn find_units(cgroup_root: &std::path::Path) -> Vec<FoundUnit> {
    let names = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let id_of = |relative: &str| -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(cgroup_root.join(relative.trim_start_matches('/')))
                .map_or(0, |meta| meta.ino())
        }
        #[cfg(not(unix))]
        {
            let _ = relative;
            0
        }
    };
    let uid_of = |name: &str, prefix: &str| -> Option<u32> {
        name.strip_prefix(prefix)?
            .strip_suffix(".service")?
            .parse()
            .ok()
    };
    let mut units = Vec::new();
    for name in names(&cgroup_root.join("cedegrid_idle.slice")) {
        if let Some(uid) = uid_of(&name, "cedegrid-idle-u") {
            let cgroup = format!("/cedegrid_idle.slice/{name}");
            units.push(FoundUnit {
                kind: UnitKind::Idle,
                uid,
                id: id_of(&cgroup),
                cgroup,
            });
        }
    }
    for name in names(&cgroup_root.join("cedegrid.slice")) {
        if let Some(uid) = uid_of(&name, "cedegrid-u") {
            let cgroup = format!("/cedegrid.slice/{name}");
            units.push(FoundUnit {
                kind: UnitKind::Ordinary,
                uid,
                id: id_of(&cgroup),
                cgroup,
            });
        }
    }
    for slice in names(&cgroup_root.join("user.slice")) {
        let Some(slice_uid) = slice
            .strip_prefix("user-")
            .and_then(|rest| rest.strip_suffix(".slice"))
            .and_then(|uid| uid.parse::<u32>().ok())
        else {
            continue;
        };
        for name in names(&cgroup_root.join("user.slice").join(&slice)) {
            if uid_of(&name, "cedegrid-u") == Some(slice_uid) {
                let cgroup = format!("/user.slice/{slice}/{name}");
                units.push(FoundUnit {
                    kind: UnitKind::Ordinary,
                    uid: slice_uid,
                    id: id_of(&cgroup),
                    cgroup,
                });
            }
        }
    }
    units
}

/// Every PID in a unit's cgroup and the cgroups below it, read from `cgroup.procs`.
pub fn unit_pids(cgroup_root: &std::path::Path, unit_cgroup: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut stack = vec![(
        cgroup_root.join(unit_cgroup.trim_start_matches('/')),
        0usize,
    )];
    while let Some((dir, depth)) = stack.pop() {
        if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            pids.extend(
                text.lines()
                    .filter_map(|line| line.trim().parse::<u32>().ok()),
            );
        }
        if depth >= 16 {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                // Directories only, never through a link.
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    stack.push((entry.path(), depth + 1));
                }
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// A unit the pass may score: found in the tree, carrying the broker's marker, with the user's
/// primary group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoredUnit {
    pub unit: FoundUnit,
    pub gid: u32,
}

/// One write the pass made, for the journal.
#[derive(Debug)]
pub struct PassWrite {
    pub uid: u32,
    pub key: ProcessKey,
    pub old: i32,
    pub new: i32,
    pub result: anyhow::Result<()>,
}

/// State the loop keeps between passes.
#[derive(Debug, Default)]
pub struct OomLoop {
    principals: Principals,
    unknown_since: BTreeMap<u32, Instant>,
    /// Users whose processes may hold a value other than 0, so they get a full pass even when
    /// no principal is over its share.
    dirty: BTreeSet<u32>,
    /// Whether the last pass found a principal over its share.
    anyone_over: bool,
}

impl OomLoop {
    /// Whether the last pass found nothing to hold: nobody over its share and no value other
    /// than 0 left on any unit process.
    pub fn quiet(&self) -> bool {
        !self.anyone_over && self.dirty.is_empty()
    }

    /// One pass: computes each unit user's writes and makes them through `access`.
    ///
    /// Only processes inside a scored unit, with the unit's user and group IDs, matching the scan
    /// by start time, are read or written (Principle I, N6).
    #[allow(clippy::too_many_arguments)]
    pub fn pass(
        &mut self,
        now: Instant,
        host: &HostReadings,
        units: &[ScoredUnit],
        pids_of: &dyn Fn(&ScoredUnit) -> Vec<u32>,
        access: &dyn ProcessAccess,
        uid_min: u32,
        principal_min_percent: u8,
        lowering: Lowering,
        marking: OverShareMarking,
    ) -> Vec<PassWrite> {
        let samples: Vec<ProcessSample> = host
            .processes
            .values()
            .map(|process| process.sample)
            .collect();
        let memory = memory_by_user(&samples);
        let principals = self
            .principals
            .update(
                now,
                &memory,
                host.capacity_pages,
                uid_min,
                principal_min_percent,
            )
            .clone();
        let shares = fair_shares(host.capacity_pages, &memory, &principals);
        let view = PassView {
            total_pages: host.total_pages,
            memory: &memory,
            shares: &shares,
            lowering,
            marking,
        };
        let users: BTreeSet<u32> = units.iter().map(|unit| unit.unit.uid).collect();
        self.unknown_since.retain(|uid, _| users.contains(uid));
        self.dirty.retain(|uid| users.contains(uid));
        // While no principal is over its share every target is 0, so only users who may still
        // hold another value need their processes read.
        let anyone_over = shares.iter().any(|(uid, &share)| {
            memory
                .by_user
                .get(uid)
                .copied()
                .flatten()
                .is_some_and(|pages| pages > share)
        });
        self.anyone_over = anyone_over;
        let mut writes = Vec::new();
        for uid in users {
            // The unknown clock runs on every pass, read or not.
            let unknown_for = match memory.by_user.get(&uid) {
                Some(None) => {
                    let since = *self.unknown_since.entry(uid).or_insert(now);
                    Some(now.saturating_duration_since(since))
                }
                _ => {
                    self.unknown_since.remove(&uid);
                    None
                }
            };
            if !anyone_over && !self.dirty.contains(&uid) {
                continue;
            }
            let mut held: BTreeMap<ProcessKey, Box<dyn HeldProcess>> = BTreeMap::new();
            let mut processes = Vec::new();
            for scored in units.iter().filter(|scored| scored.unit.uid == uid) {
                for pid in pids_of(scored) {
                    let Some(scan) = host.processes.get(&pid) else {
                        continue;
                    };
                    let Ok(process) = access.open(pid, Some(scan.start_time)) else {
                        continue;
                    };
                    let Ok(cgroup) = process.cgroup() else {
                        continue;
                    };
                    let Some(leaf) = leaf_in_unit(&scored.unit.cgroup, &cgroup) else {
                        continue;
                    };
                    if !process.ids().is_ok_and(|ids| ids.all_are(uid, scored.gid)) {
                        continue;
                    }
                    let Ok(current) = process.oom_score_adj() else {
                        continue;
                    };
                    let key = ProcessKey {
                        pid,
                        start_time: process.start_time(),
                    };
                    processes.push(UnitProcess {
                        uid,
                        kind: scored.unit.kind,
                        leaf,
                        key,
                        pages: scan.sample.pages.unwrap_or(0),
                        current,
                    });
                    held.insert(key, process);
                }
            }
            let planned = user_writes(&view, uid, &processes, unknown_for);
            let mut settled_zero = true;
            for process in &processes {
                let target = planned
                    .iter()
                    .find(|write| write.key == process.key)
                    .map_or(process.current, |write| write.value);
                settled_zero &= target == 0;
            }
            if settled_zero {
                self.dirty.remove(&uid);
            } else {
                self.dirty.insert(uid);
            }
            for write in planned {
                let Some(process) = held.get(&write.key) else {
                    continue;
                };
                let old = processes
                    .iter()
                    .find(|process| process.key == write.key)
                    .map_or(0, |process| process.current);
                writes.push(PassWrite {
                    uid,
                    key: write.key,
                    old,
                    new: write.value,
                    result: process.set_oom_score_adj(write.value),
                });
            }
        }
        writes
    }
}
