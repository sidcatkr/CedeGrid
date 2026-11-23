//! Arrival order on the compute resources (006 FR-005 to FR-008, FR-013): own work's arrival
//! there and the other accounts' standing beside it, which decide whom Performance and Aggressive
//! hold admission for.
use super::*;

/// Own work's arrival on a resource: the earliest arrival among the own allocations running there
/// (006 FR-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwnArrival {
    /// No own allocation runs there.
    Nothing,
    /// One running there has no arrival on record.
    Unknown,
    At(u64),
}

impl OwnArrival {
    /// Whether work that arrived at `arrival` came first: it did unless own work is known to
    /// have arrived strictly earlier, so an unknown or equal time favours the other (006 FR-006).
    pub(super) fn yields_to(self, arrival: Option<u64>) -> bool {
        match self {
            OwnArrival::Nothing | OwnArrival::Unknown => true,
            OwnArrival::At(own) => arrival.is_none_or(|arrival| arrival <= own),
        }
    }
}

/// Another account's standing on one compute resource (006 FR-002, FR-006).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Standing {
    /// Its CedeGrid work there, when it runs any; otherwise it is a plain principal.
    pub(super) work: Option<CedeGridWork>,
    /// A CedeGrid principal that arrived first.
    pub(super) first_comer: bool,
    /// A first-comer with Performance or Aggressive work there, which admit immediately does not
    /// lift the hold for (006 FR-013).
    pub(super) protected: bool,
}

/// The standing of an account whose CedeGrid work on a resource is `work`, beside own work's
/// arrival there.
pub(super) fn standing(own: OwnArrival, work: Option<CedeGridWork>) -> Standing {
    let Some(work) = work else {
        return Standing::default();
    };
    let first_comer = own.yields_to(work.arrival_unix_ms);
    Standing {
        work: Some(work),
        first_comer,
        // Its Performance or Aggressive work must itself have come first: Neutral work that
        // arrived first and Performance work that arrived later make no protected first-comer.
        protected: first_comer
            && work
                .protected_arrival_unix_ms
                .is_some_and(|arrival| own.yields_to(Some(arrival))),
    }
}

impl Interval<'_> {
    /// Own work's arrival among the own allocations running where `runs_there` says.
    pub(super) fn own_arrival(&self, runs_there: &dyn Fn(&Allocation) -> bool) -> OwnArrival {
        let mut earliest = None;
        for allocation in self
            .input
            .allocations
            .iter()
            .filter(|a| a.phase == AllocationPhase::Running && runs_there(a))
        {
            let Some(arrival) = self.input.arrivals.get(&allocation.id).copied() else {
                return OwnArrival::Unknown;
            };
            earliest = Some(earliest.map_or(arrival, |e: u64| e.min(arrival)));
        }
        earliest.map_or(OwnArrival::Nothing, OwnArrival::At)
    }

    /// Whether an account whose CedeGrid work came first uses the CPU, or the principal list
    /// cannot be read, so none is known to be plain: before the CPU's own reading, for holds
    /// that only Performance's plain admission lifts (006 FR-008).
    pub(super) fn cpu_first_comer_present(&self) -> bool {
        let own = self.own_arrival(&|_| true);
        let Some(standings) = self.standings(own, &|p| p.cedegrid_cpu) else {
            return true;
        };
        self.snapshot.principals.iter().flatten().any(|p| {
            p.cpu_millicores > 0
                && p.uid
                    .and_then(|uid| standings.get(&uid))
                    .is_some_and(|s| s.first_comer)
        })
    }

    /// Each other account's standing on a resource, by UID, from its CedeGrid work there as
    /// `work` picks it; None while the principal list cannot be read, so no account is known to
    /// be plain. Accounts below `uid_min` and usage without a UID are plain (006 FR-004).
    pub(super) fn standings(
        &self,
        own: OwnArrival,
        work: &dyn Fn(&PrincipalUsage) -> Option<CedeGridWork>,
    ) -> Option<BTreeMap<u32, Standing>> {
        let list = self.snapshot.principals.as_ref()?;
        let uid_min = self.config.contention.uid_min;
        Some(
            list.iter()
                .filter(|p| !p.is_owner)
                .filter_map(|p| p.uid.filter(|uid| *uid >= uid_min).map(|uid| (uid, p)))
                .map(|(uid, p)| (uid, standing(own, work(p))))
                .collect(),
        )
    }
}

impl Interval<'_> {
    /// An account's CPU, which the principal list reads host-wide, counted at the part of the
    /// scope's external usage it can be: an agent scoped to fewer CPUs measures only what runs
    /// in its scope, so work on other CPUs does not count there (006 FR-008).
    pub(super) fn cpu_in_scope(&self, view: &ModeInput) -> impl Fn(u64) -> u64 + use<> {
        let owner: u64 = view
            .principals
            .iter()
            .flatten()
            .filter(|(p, _)| p.is_owner)
            .map(|(_, usage)| *usage)
            .sum();
        let listed = self
            .snapshot
            .principals
            .iter()
            .flatten()
            .filter(|p| !p.is_owner)
            .fold(owner, |sum, p| sum.saturating_add(p.cpu_millicores));
        let external = view.external;
        move |usage| {
            if listed > external {
                let part = u128::from(usage) * u128::from(external) / u128::from(listed);
                u64::try_from(part).unwrap_or(usage)
            } else {
                usage
            }
        }
    }

    /// The CPU budget in Performance and Aggressive beside the accounts `holds_for` names
    /// (006 FR-007, FR-008), with the UIDs it holds for: 001's hold counting only them, or,
    /// where none of them contends, what plain Linux would start, the capacity less the reserve
    /// within the ancestor quota, with no margin, no deduction and CedeGrid's own CPU left in.
    /// An unreadable principal list shows no account to be plain, so every one counts. The
    /// UIDs held for are None where nothing holds.
    pub(super) fn cpu_budget_for(
        &self,
        view: &ModeInput,
        thresholds: &ResourceThresholds,
        holds_for: &dyn Fn(u32) -> bool,
    ) -> (u64, Option<Vec<u32>>) {
        let cedegrid = self.cedegrid_cpu;
        let schedulable = self
            .cpu_capacity
            .saturating_sub(self.cpu_reserve)
            .min(self.cpu_ceiling);
        let mut pending = Vec::new();
        let held_view = match &view.principals {
            Some(list) => {
                let in_scope = self.cpu_in_scope(view);
                let kept: Vec<(Principal, u64)> = list
                    .iter()
                    .filter(|(p, _)| p.is_owner || p.uid.is_some_and(holds_for))
                    .map(|(p, usage)| (*p, in_scope(*usage)))
                    .collect();
                // An account not yet admitted as a principal stays in the unattributed usage,
                // as 001 counts it, while it is one this budget holds for.
                let members: BTreeSet<u32> = list.iter().filter_map(|(p, _)| p.uid).collect();
                pending = self
                    .snapshot
                    .principals
                    .iter()
                    .flatten()
                    .filter(|p| !p.is_owner)
                    .filter_map(|p| {
                        p.uid
                            .filter(|uid| holds_for(*uid) && !members.contains(uid))
                            .map(|uid| (uid, in_scope(p.cpu_millicores)))
                    })
                    .collect();
                let external = kept
                    .iter()
                    .map(|(_, usage)| *usage)
                    .chain(pending.iter().map(|(_, usage)| *usage))
                    .sum();
                ModeInput {
                    external,
                    principals: Some(kept),
                    background: 0,
                    // This view's external is only the accounts it counts.
                    unattributed_hold: 0,
                    ..view.clone()
                }
            }
            None => view.clone(),
        };
        let contending = crate::contention::contending_principals(&held_view, thresholds);
        if contending.is_empty() {
            return (schedulable, None);
        }
        let mut held: Vec<u32> = contending.iter().filter_map(|(uid, _)| *uid).collect();
        if contending.iter().any(|(uid, _)| uid.is_none()) {
            held.extend(
                pending
                    .iter()
                    .filter(|(_, usage)| *usage > 0)
                    .map(|(uid, _)| *uid),
            );
        }
        held.sort_unstable();
        held.dedup();
        let room = self
            .cpu_capacity
            .saturating_sub(self.cpu_reserve)
            .saturating_sub(held_view.external)
            .min(self.cpu_ceiling);
        // As 001 keeps it: where the agent runs in a CPU cgroup of the owner's own, the kept
        // share is counted on own tasks' requests alone.
        let owner_cgroup = super::measure::owner_cpu_cgroup(self.snapshot);
        let owner_other: u64 = held_view
            .principals
            .iter()
            .flatten()
            .filter(|(principal, _)| principal.is_owner)
            .map(|(_, usage)| *usage)
            .sum();
        let requests_only = owner_cgroup.then(|| ModeInput {
            principals: held_view.principals.as_ref().map(|list| {
                list.iter()
                    .map(|(principal, usage)| {
                        (*principal, if principal.is_owner { 0 } else { *usage })
                    })
                    .collect()
            }),
            ..held_view.clone()
        });
        let kept = crate::contention::kept_share(
            requests_only.as_ref().unwrap_or(&held_view),
            thresholds,
            schedulable,
            if owner_cgroup {
                room.saturating_add(owner_other)
            } else {
                room
            },
        );
        let kept = if owner_cgroup {
            kept
        } else {
            kept.saturating_sub(cedegrid)
        };
        let budget = crate::contention::own_allowance(&held_view, thresholds)
            .min(room)
            .saturating_sub(cedegrid)
            .max(kept);
        (budget, Some(held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(arrival: Option<u64>, protected: Option<u64>) -> Option<CedeGridWork> {
        Some(CedeGridWork {
            arrival_unix_ms: arrival,
            protected_arrival_unix_ms: protected,
        })
    }

    #[test]
    fn the_other_account_comes_first_unless_own_work_is_known_earlier() {
        let first = |own, work| standing(own, work).first_comer;
        assert!(!first(OwnArrival::At(10), None), "a plain principal");
        assert!(first(OwnArrival::At(10), work(Some(5), None)));
        assert!(first(OwnArrival::At(10), work(Some(10), None)), "equal");
        assert!(!first(OwnArrival::At(10), work(Some(11), None)));
        assert!(first(OwnArrival::At(10), work(None, None)), "its unknown");
        assert!(
            first(OwnArrival::Unknown, work(Some(11), None)),
            "own unknown"
        );
        assert!(first(OwnArrival::Nothing, work(Some(11), None)), "none own");
    }

    #[test]
    fn a_first_comer_is_protected_only_by_performance_work_that_came_first() {
        let protected = |own, work| standing(own, work).protected;
        assert!(protected(OwnArrival::At(10), work(Some(5), Some(5))));
        assert!(protected(OwnArrival::At(10), work(Some(5), Some(10))));
        assert!(!protected(OwnArrival::At(10), work(Some(5), None)));
        // Neutral work first, Performance work after own work: not protected.
        assert!(!protected(OwnArrival::At(10), work(Some(5), Some(11))));
        assert!(!protected(OwnArrival::At(10), work(Some(11), Some(11))));
        assert!(protected(OwnArrival::Unknown, work(None, Some(11))));
        assert!(protected(OwnArrival::Nothing, work(Some(11), Some(11))));
    }
}
