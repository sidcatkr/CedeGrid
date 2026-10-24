# Review before US5

Facts checked against commit 0ad75e1 before the US5 revisions. Line numbers refer to that
commit plus the two tests added for A5 and A6. Part C changes these behaviors; the findings
below describe the code as it was reviewed.

## A1. CPU demand under saturation (deviation 15)

Yield First sees external CPU demand only as measured usage. `own_allowance` takes
`available = capacity - external` and subtracts the margin (`src/contention.rs:650-657`).
`external` is host busy minus own usage (`src/policy.rs:489-493`). PSI is parsed for the
kernel snapshot (`src/kernel.rs:533`) but neither `src/policy.rs` nor `src/contention.rs`
reads it.

On a saturated host, external usage shows only what own work leaves free, so Yield First
releases at most about one margin per interval. A probe in the `scenarios` world of
`tests/contention.rs` (own work with 64 threads and 8000m of demand, then an external process
with 5 threads and 5000m of demand) gave:

| Margin | Own usage per interval after the external start | Margin restored after |
|---|---|---|
| 25% (default) | 6000, 4000, 2000, 1000 | 1500 ms (3 intervals) |
| 10% | 7200, 6400, 5600, 4800, then pause to 0 | 2000 ms (4 intervals) |

The default just meets SC-002's 3-interval bound. A smaller margin misses it, and the 10%
case recovers only through the pause escalation. Neither meets the 2-interval bound of B8.

**Gap.** Part C fixes it: under contention, which is saturation plus an active external
principal, Yield First's target for opportunistic work is zero (B4).

## A2. Unmanaged processes of the CedeGrid account

`aggregate_principals` puts every unmanaged process into a principal keyed by owner UID. The
account's UID becomes the single `is_owner` principal. Managed PIDs are skipped
(`src/telemetry.rs:64-110`, lines 77 and 85; test `src/telemetry.rs:1574`).
`neutral_share` gives the owner principal one unbounded demand. It subtracts the account's
unmanaged usage from that share and never adds it as another principal
(`src/contention.rs:696-716`). Test: `tests/contention.rs:527`, "the account's unmanaged usage
comes out of own work's share": 4000 − 500 = 3500. A second principal would give 2666, and
ignoring the usage would give 4000. Yield First and Performance count it in `external`, since
it is busy minus managed.

**No gap.** One constraint for B6: the owner must stay a principal even when its UID is below
`uid_min`, because CedeGrid can run under a system account. Part C keeps it.

## A3. Victim selection (T043)

The order is cost-based. `victim_order` drops uncontrollable allocations, then sorts by:

1. frees the contended resource;
2. pending before running;
3. opportunistic before Guaranteed;
4. least progress lost since the last checkpoint;
5. best fit for the needed amount;
6. assignment ID, as the final tie-break only.

Code: `src/contention.rs:549-576`. The agent sets progress lost from the slot's last
checkpoint or start (`src/agent.rs:2956-2958`). Tests: `tests/contention.rs:326-400`.

**No gap.** Progress lost is compared in milliseconds, so best fit rarely decides between two
running allocations. B11 keeps this order within each yield tier.

## A4. Node cap on the most aggressive mode

The spec has it. FR-030 (`spec.md:509-512`) defines the per-resource default, the job override
and the node cap; the effective mode is the less aggressive of the request and the cap.

Code:

- `contention.max_mode` (`src/config.rs:518`, default Performance at :532);
- validation that no default exceeds the cap (`src/config.rs:718`, `:773`);
- `effective_mode` (`src/contention.rs:739-745`).

Test: `tests/policy.rs:2609`.

**No gap.**

## A5. Re-negotiation after a fallback (deviation 11)

`NegotiatedFeatures::learn` runs on every successful heartbeat reply (`src/agent.rs:3125`), and
`forget` runs on any failed or rejected heartbeat (`src/agent.rs:3250`)
(`src/agent.rs:2216-2236`). After a fallback the next report is a plain 0.2 report. A current
coordinator accepts it and advertises `contention/1`, and `learn` turns the negotiated fields
back on for the following report.

The existing test covered learn → learn(none) and learn → forget, but not recovery. The
recovery case is now added to `features_are_relearned_from_every_reply_and_dropped_after_a_rejection`
(`src/agent.rs:4471`). **No behavior gap; test added.**

## A6. CPU telemetry fully lost (deviation 4)

With CPU busy telemetry missing, the budget is zero for admission (`src/policy.rs:510-512`,
`:551-552`). The ladder treats the resource as held: `measurement_known` stays false until
`unknown_window_intervals × interval` has passed since the loss (`src/policy.rs:888-891`,
`:950-953`). Until then `plan_step` holds (`src/contention.rs:424-426`). After the window it
escalates one step at a time with cause `unknown_measurement`, starting with a cap, and never
drains running CPU work.

There was no policy-level test. `lost_cpu_telemetry_holds_for_the_unknown_window_then_escalates`
(`tests/policy.rs:2649`) now shows no step for 3 intervals, then a cap with cause
`unknown_measurement`, and no drain. **Bounded; test added.**

## A7. Success criteria and mode targets

Current SC defaults (`spec.md:667-703`, thresholds table `:714-731`):

| SC | Default |
|---|---|
| SC-001 | 0 self-drains over 100 launches and the D1/D2/D5/D6 replays; 256 MiB tolerance |
| SC-002 | Yield First margin 25%, restored within 3 intervals; no drain for CPU alone |
| SC-003 | Neutral: 50% ±15 points within 60 s with one contending principal |
| SC-004 | Performance ramp to 80% of available minus margin within 30 s; reclaim 5 s compute, 15 s memory |
| SC-005 | 0 actuations on external processes; 0 external OOM kills or GPU allocation failures |
| SC-006 | GPU job completes on an occupied device in Neutral and Performance |
| SC-007 | 0 drains when CPU alone is contended |
| SC-008 | 100% of 0.2 configs and recordings replay; all version combinations interoperate |
| SC-009 | At most one step change per load switch every 4 intervals |
| SC-010 | Step and reason visible within one report interval |
| SC-011 | Byte-identical setup output for flags and keyboard |

Targets as reviewed (`src/contention.rs:650-717`). `C` is schedulable capacity after reserves
(and floors for memory), `E` external usage, `A = C − E`, `S` the Neutral max-min share,
`m_YF = 25%` and `m_P = 10%`.

| State | Yield First | Neutral | Performance |
|---|---|---|---|
| CPU, GPU compute, not saturated, known | `A − m_YF·C` | `A` | `A − m_P·C` |
| CPU saturated | `A − m_YF·C` | `S` | `A − m_P·C` |
| GPU compute, unknown activity | `0` | `S` | `min(A − m_P·C, S)` |
| RAM, VRAM, policy A | `A − m_YF·C` | `A` | `A` |
| RAM, VRAM, policy B | `A − m_YF·C` | `max(A, S)` | `A` |

`YF ≤ N ≤ P` does not hold:

1. Idle compute: N = `A` > P = `A − m_P·C`.
2. Saturated CPU with a demanding external principal: N = `S` can exceed P. For example,
   C 8000 and E 6000 give S 4000 and P 1200.
3. Saturated CPU with little external usage: YF can exceed N. For example, C 8000 and E 1500
   give YF 4500 and S 4000.
4. Unknown GPU activity: P = `min(A − m_P·C, S)` < N = `S` whenever `A − m_P·C < S`.
5. Memory under Neutral policy B: N = `max(A, S)` > P = `A`.

**Gap.** Part C replaces the targets (B4) and adds a property test over generated
observations.
