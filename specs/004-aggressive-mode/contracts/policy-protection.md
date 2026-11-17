# Contract: Aggressive admission and its invariants

The pure policy contract that `own_allowance` implements for Aggressive (plan, "Admission rule:
where it goes"). Since 001 item 057 Part B, the allowance decides admission only; no mode at or
above Performance takes a running compute step. Each invariant names the test that pins it.

## Rule

For each compute resource (CPU, GPU compute per device) and observation:

```text
available   = capacity − expected_external(input)                 # 001, unchanged
performance = own_allowance(input with mode = Performance)        # 001, unchanged
aggressive  = if unknown_activity:           fair share (Performance's unknown arm)
              elif contended:                max(fair share, available, probe)
              else:                          available
unprotected = if unknown_activity:           aggressive
              else:                          capacity
```

Memory, both forms: Performance's allowance (the floors are already outside `capacity`).

## Invariants

| ID | Invariant | Test |
|---|---|---|
| P1 | Yield First ≤ Neutral ≤ Performance ≤ Aggressive ≤ unprotected, every resource and observation | `tests/contention.rs` ordering property; decision snapshot random scenarios |
| P2 | Aggressive − Performance ≤ Performance's margin (2% of capacity) on compute, and 0 on memory | `tests/contention.rs` |
| P3 | Uncontended with no unknown activity: Aggressive = `available` | `tests/contention.rs` |
| P4 | Contended: Aggressive leaves every other principal at least the lesser of its demand and its fair share, unless own work's fair share alone takes it lower | `tests/contention.rs`, F2c scenario in `tests/policy.rs` |
| P5 | Unknown activity: Aggressive = unprotected = Performance's unknown arm | `tests/contention.rs` |
| P6 | No running step: a decision for an allocation in Aggressive, either form, carries no compute cap, pause or drain for external demand | `tests/policy.rs`, decision snapshot |
| P7 | Memory: no hint, drain, TERM or OOM mark for a floor breach in either form | `tests/policy.rs` |
| P8 | Unprotected never above `capacity`, and never above the floors on memory | `tests/contention.rs` |
| P9 | Performance's decisions are byte-identical to the 001 snapshot with Aggressive entries filtered out | decision snapshot |
| P10 | An idle host: Aggressive admits all available capacity within one heartbeat | `tests/policy.rs` |
