# Contract: Aggressive targets and the protection

The pure policy contract that `own_allowance` and `protected_allowance` implement (plan,
"Protection rule: where it goes"). Each invariant names the test that pins it.

## Rule

For each compute resource (CPU, GPU compute per device) and observation:

```text
performance = own_allowance(input with mode = Performance)        # 001, unchanged
protected   = max-min share for own work of (capacity - background) against:
                waiting principals at their demand (unbounded if unknown),
                other principals at their usage,
                the unattributed usage,
                one unbounded principal while activity of unknown amount exists
              minus owner-unmanaged usage
aggressive  = max(performance, protected)
unprotected = max(aggressive, own usage)
```

Memory (RAM, GPU memory), both forms: Performance's target.

## Invariants

| ID | Invariant | Pinned by |
|---|---|---|
| P1 | Yield First ≤ Neutral ≤ Performance ≤ Aggressive for every resource and observation, in random `ModeInput`s, in the policy and contention scenarios and in the random decision scenarios. | property test in `tests/contention.rs`; scenario checks in `tests/policy.rs` and `tests/decision_snapshot.rs` |
| P2 | Report 018's case: 8000 m, one light user using 200 m and wanting 1000 m waits and grows by 160 m per interval. Aggressive is 7640, 7480, 7320, 7160, 7000, 7000 m and holds 7000 m; Performance ends at 6840 m. | `tests/contention.rs` |
| P3 | When Performance's target is at or below the protected point, Aggressive equals the protected target: no more than the protection leaves. | `tests/contention.rs` |
| P4 | Several waiting light principals each keep their protected share; Aggressive is never below Performance or own work's fair share. | `tests/contention.rs`, `tests/policy.rs` |
| P5 | A protected principal stops waiting, then waits again: Aggressive follows the rule in each observation and returns to Performance's pace. | `tests/policy.rs` |
| P6 | No cap and uncap cycling: under load that switches every 4 monitor intervals, Aggressive changes step at most once per switch (001 SC-009). | `tests/policy.rs` |
| P7 | RAM and GPU memory targets, hints and drains equal Performance's in the same observation, apart from 004's overrides (none apply to memory). | `tests/contention.rs`, `tests/policy.rs` |
| P8 | With nobody waiting and no unknown activity, Aggressive = capacity − external (0% margin), and a held compute cap rises to it once calm. | `tests/contention.rs`, `tests/policy.rs` |
| P9 | Activity of unknown amount holds protected Aggressive at the fair share, as Performance does. | `tests/contention.rs` |
| P10 | Unprotected Aggressive never steps compute down for external demand and is ≥ protected Aggressive; under unknown activity it does not grow past the larger of the fair share and current usage. Item 022's rule does not change it. | `tests/contention.rs`, `tests/policy.rs` |
| P11 | Performance, Neutral and Yield First targets are byte-identical to 001's in the decision snapshot, with the new `aggressive` entries filtered out (FR-020). | `tests/decision_snapshot.rs` |
| P12 | A waiting principal with a known demand below its fair share keeps exactly that demand; with its demand unknown or above its fair share, it keeps its fair share; wait alone never takes own work below its fair share (FR-004). | `tests/contention.rs` |
