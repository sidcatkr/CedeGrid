//! The OOM score function (FR-024 to FR-026, data-model.md "Score record").
//!
//! Pure: it takes one user's memory, fair share and processes inside the cedegrid units, and
//! returns the `oom_score_adj` writes to make. Two choices still open with the user are inputs:
//! when in-share scores are lowered (N1; N5 B is the same as N1 C) and whether the mode marks
//! over-share tasks (D2).

use super::ModeName;
use std::time::Duration;

/// The lowest value the broker writes; −1000 would exempt a process from the OOM killer.
pub const LOWEST_SCORE: i32 = -999;

/// The value that marks a task wholly above the user's fair share.
pub const MARK_SCORE: i32 = 1000;

/// The kernel's default.
pub const DEFAULT_SCORE: i32 = 0;

/// How long lowered scores stay while the user's memory is unknown (*provisional*, FR-026).
pub const UNKNOWN_WINDOW: Duration = Duration::from_secs(10);

/// When in-share scores are lowered (decision N1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lowering {
    /// At all times (N1 A).
    Always,
    /// Only while another principal's memory is above its own fair share (N1 B).
    WhileAnotherOverShare,
    /// Never; only over-share tasks are marked (N1 C, and N5 B).
    Never,
}

/// How item 057's D2 decides which modes mark over-share tasks at 1000 (FR-025).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverShareMarking {
    /// Yield First and Neutral mark; Performance and Aggressive do not (D2 case A).
    YieldFirstAndNeutral,
    /// Every mode marks (D2 case B).
    EveryMode,
}

impl OverShareMarking {
    pub fn marks(self, mode: ModeName) -> bool {
        match self {
            OverShareMarking::EveryMode => true,
            OverShareMarking::YieldFirstAndNeutral => {
                matches!(mode, ModeName::YieldFirst | ModeName::Neutral)
            }
        }
    }
}

/// A process inside one of the user's units, as the loop identified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessKey {
    pub pid: u32,
    /// Start time in clock ticks since boot, from `/proc/<pid>/stat`; a reused PID differs here.
    pub start_time: u64,
}

/// One process and its memory, with the value the loop just read for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoredProcess {
    pub key: ProcessKey,
    /// The process's memory in pages, in the measure 001 uses for RAM.
    pub pages: u64,
    /// `oom_score_adj` as read in this pass.
    pub current: i32,
}

/// One CedeGrid task: its processes, all in the same unit, and its mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoredTask {
    /// Orders tasks oldest first: the task's start time in clock ticks since boot.
    pub started: u64,
    pub mode: ModeName,
    pub processes: Vec<ScoredProcess>,
}

/// One user's memory, as the loop read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserMemory {
    /// Readable: the user's fair share in pages, from 001's max-min share.
    Known { fair_share_pages: u64 },
    /// A counter could not be read; `for_how_long` is how long it has been unknown.
    Unknown { for_how_long: Duration },
}

/// Everything the score function reads for one user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoreInput<'a> {
    /// The kernel's total pages for the OOM score: RAM plus swap.
    pub total_pages: u64,
    pub memory: UserMemory,
    /// Whether another principal's memory is above its own fair share now.
    pub another_over_share: bool,
    pub lowering: Lowering,
    pub marking: OverShareMarking,
    pub tasks: &'a [ScoredTask],
}

/// One write the loop should make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoreWrite {
    pub key: ProcessKey,
    pub value: i32,
}

/// The writes for one user's processes: only where the target differs from the current value.
///
/// The user's fair share is handed out oldest task first, and inside a task oldest process
/// first. A process's in-share pages lower its score by that much of the kernel's total. A task
/// that got none of the share is marked when the mode marks over-share tasks.
pub fn score_writes(input: &ScoreInput<'_>) -> Vec<ScoreWrite> {
    let targets = match input.memory {
        UserMemory::Unknown { for_how_long } if for_how_long < UNKNOWN_WINDOW => return Vec::new(),
        UserMemory::Unknown { .. } => all_default(input.tasks),
        UserMemory::Known { fair_share_pages } => known_targets(input, fair_share_pages),
    };
    targets
        .into_iter()
        .filter(|(process, value)| process.current != *value)
        .map(|(process, value)| ScoreWrite {
            key: process.key,
            value,
        })
        .collect()
}

fn all_default(tasks: &[ScoredTask]) -> Vec<(ScoredProcess, i32)> {
    tasks
        .iter()
        .flat_map(|task| task.processes.iter())
        .map(|process| (*process, DEFAULT_SCORE))
        .collect()
}

fn known_targets(input: &ScoreInput<'_>, fair_share_pages: u64) -> Vec<(ScoredProcess, i32)> {
    let lower = match input.lowering {
        Lowering::Always => true,
        Lowering::WhileAnotherOverShare => input.another_over_share,
        Lowering::Never => false,
    };
    let mut tasks: Vec<&ScoredTask> = input.tasks.iter().collect();
    tasks.sort_by_key(|task| task.started);
    let mut remaining = fair_share_pages;
    let mut targets = Vec::new();
    for task in tasks {
        let mut processes = task.processes.clone();
        processes.sort_by_key(|process| process.key.start_time);
        let mut task_share = 0u64;
        let mut values = Vec::with_capacity(processes.len());
        for process in &processes {
            let in_share = process.pages.min(remaining);
            remaining -= in_share;
            task_share += in_share;
            let value = if lower && in_share > 0 {
                lowered(in_share, input.total_pages)
            } else {
                DEFAULT_SCORE
            };
            values.push((*process, value));
        }
        let task_pages: u64 = processes.iter().map(|process| process.pages).sum();
        let wholly_over = task_share == 0 && task_pages > 0;
        if wholly_over && input.marking.marks(task.mode) {
            values.iter_mut().for_each(|(_, value)| *value = MARK_SCORE);
        }
        targets.extend(values);
    }
    targets
}

/// `−round(1000 × in_share ÷ total)`, no lower than −999.
fn lowered(in_share: u64, total_pages: u64) -> i32 {
    if total_pages == 0 {
        return DEFAULT_SCORE;
    }
    let total = u128::from(total_pages);
    let scaled = (u128::from(in_share) * 1000 + total / 2) / total;
    let scaled = i32::try_from(scaled.min(999)).unwrap_or(999);
    (-scaled).max(LOWEST_SCORE)
}
