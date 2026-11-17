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
    leaf.and_then(|leaf| leaf.strip_prefix("cedegrid-"))
        .and_then(|rest| rest.rsplit_once('-'))
        .and_then(|(mode, _)| ModeName::from_name(mode))
        .unwrap_or(ModeName::Neutral)
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
