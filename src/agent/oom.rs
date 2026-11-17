//! Out-of-memory kills of own tasks: whether the kernel's OOM killer ended a task, and
//! whether own memory was within its fair share then (FR-076).

/// RAM as the agent last saw it while a task ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RamView {
    pub own_mib: u64,
    pub fair_share_mib: u64,
    /// External principals above the fair share: UID, or none for unattributed usage, and MiB.
    pub above: Vec<(Option<u32>, u64)>,
    /// Set where RAM's governing mode keeps own memory at a floor breach (Performance and
    /// Aggressive): what decides fault there instead of the fair share.
    pub keeps_memory: Option<Breach>,
}

/// Own memory's growth over the current floor breach, in a mode that gives no memory back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Breach {
    /// Own MiB now less own MiB when the breach began; `None` outside a breach or unknown.
    pub growth_mib: Option<u64>,
    pub tolerance_mib: u64,
}

/// What an ended task's outcome says about an out-of-memory kill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OomVerdict {
    /// No out-of-memory kill, or one no count shows: a normal outcome.
    None,
    /// Killed while own memory was within its fair share: requeued without an attempt.
    NotOwnFault(String),
    /// Killed while own memory was above its fair share, or with the share unknown.
    OwnFault(String),
}

/// What ended a task, as its outcome and the agent's samples show it.
pub(crate) struct Ending<'a> {
    pub termination_signal: Option<i32>,
    /// The supervisor itself sent TERM or KILL, as at the end of a drain.
    pub supervisor_terminated: bool,
    /// The leaf's `memory.events` `oom_kill` count at release, in Full.
    pub leaf_oom_kills: Option<u64>,
    /// The host's `/proc/vmstat` `oom_kill` count at the agent's last samples, oldest first.
    pub host_oom_kills: &'a [Option<u64>],
    pub view: Option<&'a RamView>,
}

/// SIGKILL, which the kernel's OOM killer sends.
const SIGKILL: i32 = 9;

/// Whether the OOM killer ended the task, and who caused it. A KILL counts as the OOM
/// killer's only when the leaf's count shows a kill or, without a leaf count, the host's
/// count rose over the last samples; a known leaf count decides alone, since any process
/// can raise the host's. A missing count leaves it unclassified (Principle III).
pub(crate) fn classify(ending: &Ending) -> OomVerdict {
    if ending.termination_signal != Some(SIGKILL) || ending.supervisor_terminated {
        return OomVerdict::None;
    }
    let host_rose = match ending.host_oom_kills {
        [first, .., last] => first.zip(*last).is_some_and(|(first, last)| last > first),
        _ => false,
    };
    if !ending.leaf_oom_kills.map_or(host_rose, |kills| kills > 0) {
        return OomVerdict::None;
    }
    let Some(view) = ending.view else {
        return OomVerdict::OwnFault(
            "killed by the out-of-memory killer while own memory's fair share was unknown".into(),
        );
    };
    if let Some(breach) = &view.keeps_memory {
        return match breach.growth_mib {
            Some(growth) if growth <= breach.tolerance_mib => OomVerdict::NotOwnFault(format!(
                "killed by an out-of-memory condition it did not cause: own memory grew by {growth} MiB over the floor breach, within the {} MiB tolerance",
                breach.tolerance_mib
            )),
            Some(growth) => OomVerdict::OwnFault(format!(
                "killed by the out-of-memory killer after own memory grew by {growth} MiB over the floor breach"
            )),
            None => OomVerdict::OwnFault(
                "killed by the out-of-memory killer with no floor breach seen before it, or own memory's growth over it unknown".into(),
            ),
        };
    }
    if view.own_mib > view.fair_share_mib {
        return OomVerdict::OwnFault(format!(
            "killed by the out-of-memory killer while own memory, {} MiB, was above its {} MiB fair share",
            view.own_mib, view.fair_share_mib
        ));
    }
    let above: Vec<String> = view
        .above
        .iter()
        .map(|(uid, mib)| match uid {
            Some(uid) => format!("uid {uid} at {mib} MiB"),
            None => format!("unattributed usage at {mib} MiB"),
        })
        .collect();
    OomVerdict::NotOwnFault(format!(
        "killed by an out-of-memory condition it did not cause: own memory, {} MiB, was within its {} MiB fair share; above theirs: {}",
        view.own_mib,
        view.fair_share_mib,
        if above.is_empty() {
            "none seen".to_string()
        } else {
            above.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KILL: i32 = 9;

    fn view(own_mib: u64) -> RamView {
        RamView {
            own_mib,
            fair_share_mib: 15_500,
            above: vec![(Some(1001), 17_500)],
            keeps_memory: None,
        }
    }

    /// In Performance own memory is not given up at a floor breach, so a kill there is not own
    /// fault when own memory grew by no more than the noise tolerance over the breach, whatever
    /// its fair share; a larger or unknown growth is (FR-076).
    #[test]
    fn a_performance_kill_is_judged_by_growth_over_the_breach() {
        let host = [Some(5), Some(6)];
        let judged = |growth: Option<u64>| {
            let mut above_share = view(16_000);
            above_share.keeps_memory = Some(Breach {
                growth_mib: growth,
                tolerance_mib: 64,
            });
            classify(&ending(KILL, &host, None, Some(&above_share)))
        };
        assert!(matches!(judged(Some(0)), OomVerdict::NotOwnFault(_)));
        assert!(matches!(judged(Some(64)), OomVerdict::NotOwnFault(_)));
        assert!(matches!(judged(Some(65)), OomVerdict::OwnFault(_)));
        assert!(matches!(judged(None), OomVerdict::OwnFault(_)));
    }

    fn ending<'a>(
        signal: i32,
        host: &'a [Option<u64>],
        leaf: Option<u64>,
        view: Option<&'a RamView>,
    ) -> Ending<'a> {
        Ending {
            termination_signal: Some(signal),
            supervisor_terminated: false,
            leaf_oom_kills: leaf,
            host_oom_kills: host,
            view,
        }
    }

    #[test]
    fn a_kill_in_an_interval_where_oom_kill_rose_is_an_oom_kill() {
        let within = view(14_000);
        let above = view(16_000);
        let verdict = classify(&ending(
            KILL,
            &[Some(5), Some(5), Some(6)],
            None,
            Some(&within),
        ));
        let OomVerdict::NotOwnFault(detail) = verdict else {
            panic!("{verdict:?}");
        };
        assert!(detail.contains("did not cause"), "{detail}");
        assert!(detail.contains("uid 1001"), "{detail}");
        assert!(matches!(
            classify(&ending(
                KILL,
                &[Some(5), Some(6), Some(6)],
                None,
                Some(&above)
            )),
            OomVerdict::OwnFault(_)
        ));
        // In Full the leaf's own count decides, whatever the host shows.
        assert!(matches!(
            classify(&ending(
                KILL,
                &[Some(5), Some(5), Some(5)],
                Some(1),
                Some(&within)
            )),
            OomVerdict::NotOwnFault(_)
        ));
        // A kill the share cannot judge is own fault: a normal failure.
        assert!(matches!(
            classify(&ending(KILL, &[Some(5), Some(6)], None, None)),
            OomVerdict::OwnFault(_)
        ));
    }

    #[test]
    fn other_endings_are_no_oom_kill() {
        let within = view(14_000);
        // No rise, another signal, or a missing count: unclassified, a normal outcome. A known
        // leaf count of 0 overrides a rise of the host's, which any process can cause.
        for (signal, host, leaf) in [
            (KILL, vec![Some(5), Some(5), Some(5)], None),
            (KILL, vec![Some(5), Some(5), Some(5)], Some(0)),
            (KILL, vec![Some(5), Some(6), Some(6)], Some(0)),
            (15, vec![Some(5), Some(6), Some(6)], None),
            (KILL, vec![None, Some(6), Some(6)], None),
            (KILL, vec![Some(5)], None),
            (KILL, vec![], None),
        ] {
            assert_eq!(
                classify(&ending(signal, &host, leaf, Some(&within))),
                OomVerdict::None,
                "{signal} {host:?} {leaf:?}"
            );
        }
        // A KILL the supervisor sent after a drain's grace is no OOM kill.
        let mut drained = ending(KILL, &[Some(5), Some(6), Some(6)], Some(1), Some(&within));
        drained.supervisor_terminated = true;
        assert_eq!(classify(&drained), OomVerdict::None);
        let mut exited = ending(KILL, &[Some(5), Some(6)], None, Some(&within));
        exited.termination_signal = None;
        assert_eq!(classify(&exited), OomVerdict::None);
    }
}
