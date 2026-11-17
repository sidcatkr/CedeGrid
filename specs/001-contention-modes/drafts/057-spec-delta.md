# Draft: what changes in 001, the guarantees, 003 and 004 for item 057 Part B

**Status**: draft. Nothing here is applied, and no code is written. It follows the amendment
draft ([057-amendment-draft.md](057-amendment-draft.md)) and the recommended answers in
[057-report.md](057-report.md); where an answer changes a passage, both versions are given.

**The rule**: in Performance, on CPU and GPU compute, running own work is not capped, paused or
drained to give another user more than the kernel and the driver would give them beside the same
own work. Admission is unchanged in kind. Floors still limit admission. Neutral takes over
per-account fairness. Yield First is unchanged. Principle I and the launch barrier are untouched.

Figures marked *measured* come from the brief's table of 2026-11-13 or from
`docs/validation.md`. Everything else is reasoned.

## 1. What each mode holds back, before and after

`C` capacity, `A = C − E` with `E` expected external usage, `S` own work's max-min fair share,
`m_YF`, `m_N`, `m_P` the margins. "Running cap" is what a step may hold running own work to;
"admission" is what new work is admitted within.

**Before** (data-model "Mode targets"; running cap and admission are the same target):

| Resource and state | Yield First | Neutral | Performance |
|---|---|---|---|
| CPU, GPU compute, contended | 0 | `S` | `max(S, A − m_P·C)` |
| CPU, GPU compute, unknown activity | 0 | `S` | `S` |
| CPU, GPU compute, otherwise | `A − m_YF·C` | `A − m_N·C` | `A − m_P·C` |
| Time-sliced GPU, m × k > E, a principal slice-bound | duty from the target | d0' (Neutral margin) | d0' (Performance margin) |
| RAM, GPU memory, floor breach others caused | all own memory gives way | all own memory gives way | own memory above `S` gives way |
| OOM mark (1000) | over-share tasks | over-share tasks | over-share tasks |

**After** (Yield First and Neutral rows unchanged):

| Resource and state | Performance running cap | Performance admission |
|---|---|---|
| CPU, GPU compute, contended | none | `max(S, A − m_P·C)` (unchanged) |
| CPU, GPU compute, unknown activity or unknown CPU telemetry | none; steps hold | none admitted (point 2, A) |
| CPU, GPU compute, otherwise | none | `A − m_P·C` (point 3, A: the margin is admission only) |
| Time-sliced GPU | none; no duty, no re-measure window (point 4) | one more own process only while (m + 1) × k ≤ E or no counted principal is slice-bound |
| RAM, GPU memory, floor breach others caused | nothing given up; save-progress to every own task (point 1, B) | nothing admitted while breached (unchanged) |
| OOM mark (1000) | none, except `ram.prefer_own_oom_victim` (point 1, B) | — |
| Operator envelope, explicit drain, Guaranteed rules | as today | as today |

Under point 1 option A the two memory rows keep their "before" values. Aggressive follows
Performance's column, with `m_P` replaced by 0 and admission filling at once (point 5).

## 2. Terms (spec "Terms")

- **New, Baseline**: the same own work started plainly without CedeGrid in the same place: the
  same account, control group and session layout, each own process in a session of its own where
  that differs. Harm and loss are measured against it.
- **Contended**, old: "…without its demand being known to be satisfied, or when it has external
  activity of unknown amount." New: unchanged text, and one sentence added: "A principal that
  makes a resource contended *contends* for it. In Performance and Aggressive contention closes
  admission beyond FR-022's admission target and steps nothing."
- **Margin**, old: "Capacity own work leaves unused for external demand to grow into…" New: add
  "In Performance and Aggressive the compute margin bounds admission only; running own work is not
  capped for it."
- **Fair share**, old: "a work-conserving, equal split…" New: add "It is Neutral's target under
  contention and Performance's admission bound; Performance does not cap running work to it."
- **Over-share task** (point 1, B): add "In Yield First and Neutral." Under A: unchanged.
- **Risk marker**, old: "…the one memory exception, in-share own memory in Performance and
  Aggressive (FR-023), is a rule of those modes, not an option." New (B): "…the floor rule of
  Performance and Aggressive (FR-023) is a rule of those modes, not an option."
- **Contention mode**, old: "The modes are Yield First, Neutral and Performance; Performance is
  the default." New: add "Yield First gives others first, Neutral gives back down to own work's
  fair share, and Performance gives back nothing beyond the baseline (FR-022)."

## 3. Functional requirements, old against new

Only the changed sentences are quoted; the rest of each requirement stands.

**FR-008** (unknown). Old: "In Performance and Aggressive, compute escalation stops at FR-022's
fair-share limit, and unknown CPU telemetry holds own CPU at Neutral's fair share (FR-022); Yield
First and Neutral keep a CPU budget of 0 while CPU is unknown." New (point 2, A): "In Performance
and Aggressive an unknown compute measurement, or activity of unknown amount, blocks admission on
that resource and holds every step; it does not escalate to a compute step. Yield First and
Neutral keep a CPU budget of 0 while CPU is unknown."

**FR-011** (the `own_over_target` bullet). Old: "When own CPU usage is over its target and the
budget did not fall…" New (point 3, A): "In Yield First and Neutral, when own CPU usage is over
its target and the budget did not fall…". Add: "In Performance and Aggressive own usage above the
admission target takes no step; it closes admission until usage is back within it."

**FR-014**. Old: "In Performance and Aggressive, FR-022's compute rule replaces this escalation."
New: "Performance and Aggressive take no compute step for other principals (FR-022), so nothing
escalates there."

**FR-016** (tiers). Old: "allocations yield in mode tiers inside it: Yield First, then Neutral,
then Performance". New: add "Where the governing mode is Performance, the aggregate takes no
compute step for others, and only jobs in Yield First and Neutral are held, each to its own
mode's target." The case of a Performance job on a node governed by Neutral is report point 7.

**FR-021** (Neutral and the GPU paths). Old, four places: "in Performance, Aggressive and Neutral
own work MUST NOT be capped on GPU compute while m × k ≤ E", "in Performance, Aggressive and
Neutral its GPU compute MUST be capped from the context counts", "the margin the mode's
(Performance 2%, Neutral 5%, Aggressive 0)", "On a device not known to be time-sliced, in
Performance, Aggressive and Neutral, the duty is…", and "in Performance, Aggressive and Neutral,
the external amount that sets own work's GPU compute target… MUST therefore be the average…".
New: "in Neutral" in each, and "the Neutral margin (5%)", with d0' 643 at m = 2, E_b = 1 (692 and
725 are dropped). Added after the time-slice duty's bullets: "In Performance and Aggressive the
counts, the slice-bound readings and the external estimate are kept for the decision record and
for admission (FR-022), and no duty is set: every interval is uncapped, so every measured interval
feeds the estimate and no re-measure window opens (w = 0)."

**FR-022**, new text in full:

- **FR-022**: Performance MUST:
  - let own work use all capacity external demand is not using, minus a small admission margin
    (default 2%) on CPU and GPU compute; running own work is not capped for the margin. Memory
    keeps the floors as FR-023 states for this mode;
  - scale up quickly;
  - not cap, pause or drain running own work on CPU or GPU compute for another principal's
    demand, for own usage above its admission target, or for activity of unknown amount, in
    Tracked and Full and on every GPU device. What another principal gets beside it is the
    kernel's and the driver's split, as beside the same own work without CedeGrid;
  - admit new own work on a compute resource only within max(S, A − margin), S being own work's
    fair share counting each contending principal at the lesser of its demand and its share, so
    that new own work never takes a contending principal below that. On a time-sliced GPU it
    admits one more own process only while (m + 1) × k ≤ E (FR-021's counts) or no counted
    principal is slice-bound, and records `gpu_time_slice_admission_held` otherwise;
  - while CPU telemetry is unknown, or a compute resource has activity of unknown amount, admit
    no new own work there and hold every step (FR-008);
  - keep the operator envelope (FR-057), the Guaranteed rules (FR-024) and explicit operator
    drain, as every mode does.

**FR-065**. Old: "Neutral and Performance share the fair share under contention, and floors bound
every mode on memory: own memory above its fair share in every mode, and all own memory in Yield
First and Neutral". New: "Under contention Neutral holds the fair share and Performance takes no
compute step, so its running target is the whole capacity and its admission target,
max(S, A − m_P·C), is at least Neutral's S. On memory, Yield First and Neutral give way at the
floors and Performance does not (FR-023), so Yield First ≤ Neutral ≤ Performance still holds."
Under point 1 A the old memory clause stays.

**FR-023** (floors). Point 1 B, new second paragraph: "In Yield First and Neutral own memory gives
way at the floors: the over-share tasks first, as below, then the in-share ones. In Performance
and Aggressive own memory is not hinted, drained or sent TERM for a floor breach, on the critical
path (FR-063) and for Guaranteed memory (FR-024) too, and no own task is marked for the OOM
killer, except under `ram.prefer_own_oom_victim`. Every own task gets the save-progress signal
(FR-075), and the kernel decides at exhaustion, as it would beside the same own work without
CedeGrid. The floors still close admission (FR-066), and the operator's envelope (FR-057)
applies in every mode." The out-of-memory bullet: "While own memory is above its fair share" →
"In Yield First and Neutral, while own memory is above its fair share". The over-share
paragraph ("In every mode an over-share task gives back first…") → "In Yield First and Neutral
an over-share task gives back first…". Under A the requirement is unchanged. Under C, add: "In
Performance and Aggressive own memory that grows while the floor is breached gives way, and its
tasks are marked, until own memory is back at what it held when the breach began."

**FR-024**. Old: "(for a floor in Performance and Aggressive, only memory above own work's fair
share, FR-023)". New (B): "(never for a floor in Performance and Aggressive, FR-023)".

**FR-028**. Add to the codes a reported hold: "`gpu_time_slice_admission_held` holds GPU
admission on a time-sliced device in Performance and Aggressive (FR-022). It is not a step and
does not count as own work being reclaimed."

**FR-034** (the return after contention). Old: "so own usage, not only the target, returns
within 3 s of the contender stopping in Neutral and Performance". New: "…within 3 s of the
contender stopping in Neutral; Performance takes no step to return from, and its admission
reopens in the first quiet interval."

**FR-044**. Old: "FR-023's in-share memory rule in Performance and Aggressive is a rule of those
modes, not an option." New (B): "FR-023's floor rule in Performance and Aggressive…".

**FR-045** (the Performance row). Old: "Own work keeps all idle capacity minus a small margin and
grows quickly. Other workloads still get their fair share within the reclaim bounds; own memory
above its fair share gives way at the floors, and in-share own memory is kept at a breach others
caused (FR-023)." New: "Own work keeps all idle capacity minus a small admission margin and grows
quickly. Running own work gives others nothing beyond what the kernel and the GPU driver give
them beside it; new own work does not start where another principal is short of its fair share;
the floors limit admission, and at a floor breach the kernel decides, as without CedeGrid
(FR-023)."

**FR-063** (critical path). Old: "In Performance and Aggressive the critical path acts only on
own memory above its fair share when others' growth caused the breach; in-share allocations get
the save-progress signal (FR-075)." New (B): "In Performance and Aggressive the critical path
takes no step on own memory; every own task gets the save-progress signal (FR-075)."

**FR-075**. Old: "the node MUST send the in-share own tasks on that resource a save-progress
signal… Tasks above the fair share get no signal". New: "…the in-share own tasks… in Yield First
and Neutral, and every own task on that resource in Performance and Aggressive (B)…".

**FR-076** (OOM requeue). Old: requeued "while own memory was within its fair share at the last
observation". New (B): "In Yield First and Neutral, while own memory was within its fair share
at the last observation; in Performance and Aggressive, when free memory was below the floor
before the kill and own memory grew by no more than `noise_tolerance_mib` over that breach
episode. Otherwise, or with the count unknown, the kill is a normal failure."

**User story 3**, scenario 3. Old: "**When** external demand grows, **Then** own work yields what
is needed to restore the margin and floors. **When** the resource is contended, **Then** compute
keeps what external demand does not take, minus the margin, and never goes below the Neutral fair
share." New: "**When** external demand grows, **Then** running own work is not stepped and no
new own work is admitted while it would take that demand below its fair share. **When** a floor
is breached, **Then** own memory is not given up and the kernel decides, as without CedeGrid."
Scenario 4. Old: "**Given** any mode, **When** a no-harm floor would be crossed, **Then** own work
steps down until the floor holds". New: "**Given** Yield First or Neutral…; **Given** any mode,
**When** a floor would be crossed by new work, **Then** it is not admitted."

**Edge case** "Others' growth breaches a memory floor in Performance while own memory is within
its fair share." New (B): "Others' growth breaches a memory floor in Performance. Own memory is
not hinted or drained; every own task gets one save-progress signal. If memory keeps filling, the
kernel decides, with own work at its launch baseline (FR-023)."

## 4. Success criteria, old against new

- **SC-002**. Old: "In Performance, own CPU usage falls by at least its margin per interval while
  external demand grows into the freed capacity, so it reaches the fair share within ⌈(own usage
  − fair share) ÷ margin⌉ intervals (24 intervals, 12 s, from 98% with the 2% margin), and never
  goes below it." New: "In Performance own CPU is not stepped for another principal's demand
  (SC-018, SC-019), and no new own work is admitted while it would take that principal below the
  lesser of its demand and its fair share."
- **SC-004**, second bullet. Old: "When an external workload starts, it gets its demanded usage,
  up to its fair share, within the reclaim bounds of SC-012." New: "When an external workload
  starts, no new own work is admitted on that resource while it would take the workload below the
  lesser of its demand and its fair share, and the workload gets no less from running own work
  than beside the same own work without CedeGrid (SC-019)."
- **SC-005**. Old: "A kill while own memory was within its fair share in Performance or
  Aggressive is not attributable to CedeGrid (FR-023)." New (B): "A kill in Performance or
  Aggressive at a floor breach own growth did not cause is not attributable to CedeGrid, nor is a
  kill the baseline run shows too (FR-023)."
- **SC-012**, CPU. Old: "Performance as SC-002 states." New: "Performance takes no CPU step for
  others (FR-022)."
- **SC-013**. Unchanged text; the generated cases change (Performance's running target is the
  capacity on compute; its admission target is the one compared).
- **SC-017**. Old: the bound uses "the mode's target… 480 in Performance, 450 in Neutral, 500 in
  Aggressive". New: "In Neutral: [the present formula] with 450 beside one saturating user. In
  Performance and Aggressive: own work with CedeGrid ÷ alone is at least own work without
  CedeGrid ÷ alone less the repeats' spread, and the other user's rate is within the spread of
  its rate without CedeGrid (SC-018, SC-019)."
- **New SC-018** (own work against the baseline): in Performance, in each of F1 to F4's shapes,
  in Tracked and in Full, at least 3 repeats with CedeGrid and 3 without, own work's throughput
  with CedeGrid is at least its throughput without less the spread, the spread being the larger
  of the two sets' max − min. In F4's shape no own task is hinted, drained or sent TERM (B).
- **New SC-019** (the other user against the baseline): in the same runs, the other user's rate
  with CedeGrid is at least its rate without less the spread, and its p99 at most its p99 without
  plus the spread. A gain beyond the spread is reported, since it means own work gave something
  back. In F4's shape, the kernel's OOM victim is the one the baseline shows, or own work.
- **Provisional thresholds**: "CPU reclaim" row: "in Performance at least the margin per interval
  down to the fair share" → "Performance takes no CPU step for others". "Performance scale-up"
  row: "a held step still waits the step dwell" → "an envelope step still waits the step dwell".
  "Performance margin" row: "The smallest room that still shows external growth; under contention
  Performance keeps what external demand does not take" → "The smallest room that still shows
  external growth before admitting more; running work is not capped for it". "Neutral and
  Performance escalation (`reclaim_s`)" → "Neutral escalation".

## 5. Configuration (contract `config-contention.md`)

No key is removed or renamed, and every 0.2 and 001 file loads unchanged (Principle VII).

| Key | Old meaning in Performance | New meaning in Performance |
|---|---|---|
| `performance_margin_percent` | idle margin for running work and admission | admission margin only |
| `reclaim_s` | Neutral and Performance escalation | Neutral escalation only; read and unused in Performance |
| `pause_bound_s` | Performance compute pauses at most one interval | not used in Performance |
| `slice_bound_percent` | time-slice duty in N and P | Neutral's duty; in Performance only the admission hold |
| `slice_step_percent` | the duty's step in N and P | Neutral only |
| `scale_up_step_interval_s` | reversal rate of every step | reversal rate of Performance's envelope steps |
| `ram.prefer_own_oom_victim` | every own task at 1000 | unchanged, and the only OOM mark in Performance (B) |

`config::KEY_CLASSES` descriptions and `doctor`'s line for each change with them. No new key.

## 6. Contracts and reason codes

- `decision-output.md`: the `time_slice` record in Performance carries the counts and the
  slice-bound principals, with `duty: null` and the admission result; `gpu_demand` shows every
  interval uncapped. No field is added or removed.
- `resource-target-file.md`: in Performance the CPU and GPU allowance is still published from
  the admission target, and reaching it causes no step. The text "without causing a step" gets "in
  Yield First and Neutral; in Performance it is advisory". No format change.
- Reason catalog (`src/contention.rs`, `docs/guarantees.md`):
  - new `gpu_time_slice_admission_held`: "One more own process on the time-sliced device would
    take a slice-bound principal below its fair share of the busy contexts, so GPU admission
    holds."
  - `cap_at_minimum_duty`: "…so a Performance cap that still misses its target may pause for one
    interval." → "The cap runs at the minimum duty."
  - `cpu_unknown_fair_share_hold`: kept in the catalog for recorded replays, no longer emitted.
  - `gpu_time_slice_fair`, `gpu_time_slice_unbound`, `gpu_time_slice_duty`: "Neutral" only in
    their text.

## 7. `docs/guarantees.md`, by section

- **Opening paragraph**: "the contention mode says how much it gives back when others need
  capacity. Performance is the default" → add "and gives back nothing beyond what the kernel and
  the GPU driver give beside the same work without CedeGrid".
- **GPU sharing**: the time-slice paragraph ("in Neutral, Performance and Aggressive own work is
  not capped while…", d0's margins "Performance 2%, Neutral 5%", "692 in Performance") → Neutral
  only, and one paragraph for Performance's admission hold. "Unsupported… Neutral and Performance
  count it as one principal… under which Neutral and Performance hold the fair share" → "Neutral
  holds the fair share; Performance admits nothing new there and holds".
- **Resource accounting**, the modes paragraph: "Performance keeps the larger of the fair share
  and capacity minus observed external usage minus its margin: it gives back what external demand
  takes and never goes below the fair share; activity of unknown amount holds it to the fair
  share" → FR-022's new text. "In every mode, contended CPU returns within 2 monitor intervals
  except in Performance, which follows what external work takes" → "In Yield First and Neutral…;
  Performance takes no CPU step for others". The `own_over_target` sentences → Yield First and
  Neutral.
- **"Floors hold by mode"**, **"Own tasks give memory back by fault"**, **"Out-of-memory order
  follows fault"**: per point 1 (B: Performance's floor paragraph rewritten as FR-023; the other
  two scoped to Yield First and Neutral, with Performance's OOM requeue test from FR-076).
- **"No option puts another workload at risk"**: "In Performance a compute step never targets own
  work below its fair share…" → "In Performance no compute step is taken for others."
- **Reason table**: the rows in section 6.

## 8. Feature 003 and feature 004

**003** (`specs/003-everyday-cli/spec.md`):

- FR-049: "The fair share MUST be treated as the level own work does not yield below" → "In
  Neutral and Yield First, the fair share…"; `top` and `ps` showing own work below its fair share
  stays.
- FR-051: "Where users share one CPU cgroup, Neutral and Performance MUST reach the fair share by
  limiting cores" → "Neutral MUST…". Performance takes the kernel's split there (point 6).
- FR-056 and FR-057 (core moves off a pinned or contended CPU): Yield First and Neutral only.
  Performance moves no core for others.
- FR-050 (run-queue wait): unchanged; in Performance it feeds the admission bound (who contends)
  and the record, not a step.
- SC-013 (the contender's p99 at 1.2× beside a fixed allocation): Neutral only; Performance's
  contender is gated by SC-019.

**004** (`specs/004-aggressive-mode/`), by point 5:

- Option B (recommended): FR-002 keeps "Performance with three overrides" (0% margin, fill at
  once, memory as Performance's). FR-003's protection reduces to the admission rule: protected
  Aggressive never admits new own work where it would take a contending principal below the
  lesser of its demand and its fair share; it never gives back running work, as Performance.
  FR-005 → "admits nothing new and holds, as Performance". FR-010 → "Unprotected Aggressive MUST be
  protected Aggressive that also admits new own work where another principal contends, never into
  activity of unknown amount or the floors". FR-013's numbers screen shows how much a contending
  user would slow down from the new work. FR-020 ("Performance MUST NOT change in this feature")
  stays, with 057 landing first. SC-001 and SC-002 → admission tests; SC-004 → "in unprotected
  Aggressive own work is admitted where another principal contends, and the floors hold".
- Option A: FR-010–FR-016, SC-004–SC-006 and the unprotected tasks are removed.
- Option C: the feature is withdrawn; `reduced_margin` and the ramp settings cover what is left.

## 9. Code paths that would change (no code written)

- `src/contention.rs`:
  - `own_allowance`: stays the admission target for every mode. A Performance branch for unknown
    activity returns the admission hold instead of `neutral_share`.
  - `LadderTiming::new`: `pause_returns_to_cap` and the one-interval Performance pause bound go;
    `plan_step`'s Performance branches (the recomputed cap, the pause that returns to the cap) and
    `ComputeFloor` go with them.
  - reason catalog: section 6.
- `src/policy/measure.rs`:
  - `cpu_target`: keeps the Performance saturation judgement (own work judged at its Neutral
    share) for who contends; the budget it sets becomes the admission budget only in Performance.
  - `cpu_unknown_hold`: not called in Performance (point 2, A); admission is held instead.
  - `time_slice`, `slice_duty`: in Performance compute the counts and slice-bound readings and
    the admission hold, and set no duty.
  - `gpu_demand`, `windows_can_open`: no re-measure window in Performance (w = 0).
  - `compute_budget_for`: in Performance the budget feeds admission and `held_back` only.
- `src/policy/ladder.rs`:
  - the excess computation (CPU usage against the budget, the cause choice, the
    `own_over_target` switch): in Performance an external-growth or own-over-target excess plans
    no step; envelope and operator-drain excesses do.
  - the floors-by-mode block of the excess computation, `ceiling`, `oom_order` and the
    save-progress listing: per point 1 (B: Performance takes no memory step for a floor breach,
    marks no task and signals every own task).
  - `compute_floor`, `hold_slice_duty`, `time_slice_exempt`, `gpu_duty`, `no_gpu_pause`: Neutral
    only, or removed where Performance was their only user.
- `src/policy/admission.rs`: `admission` and `admissions` gain the GPU time-slice hold; the CPU
  headroom check is unchanged; `held_back` shows Performance's margin as the admission margin.
- `src/agent/oom.rs` `classify`: Performance's fault test (FR-076, B).
- `src/explain.rs`, `src/config.rs`: `doctor` lines and key descriptions.
- `src/actuation.rs`, `src/cgroup.rs`, `src/supervision/`: no change; fewer steps reach them.
- Python and TypeScript SDKs, wire, contract fixture: no change.

## 10. Tests and gates

**Tests first** (Principle VI), each failing on today's code:

- `tests/policy.rs`: Performance with a contending principal on CPU takes no step and holds
  admission at max(S, A − m); Performance with own usage over the admission target takes no step;
  unknown CPU telemetry and unknown GPU activity in Performance hold, block admission and take no
  step; a time-sliced device at m = 2, E = 1 with a slice-bound principal sets no duty in
  Performance and holds a third own process; Neutral at the same input keeps d0' 643.
- Memory (B): a floor breach in Performance hints, drains and marks nothing, signals every own
  task, and closes admission; Yield First and Neutral unchanged.
- `tests/agent.rs` or the OOM unit tests: Performance's requeue test (FR-076).
- The mode-order property test (SC-013) over the new targets; the decision snapshot
  (`decision_snapshot`) regenerated, with every changed decision listed in the review.

**Native gates that move from Performance to Neutral** (`native-gates.md`, `docs/validation.md`):

- T456's form (items 041–053): the saturating user at least ½ × its rate alone × 0.97 beside
  two own probes; SC-017's 480 bound. Both become Neutral-only (450).
- SC-002's Performance clause (⌈(own − S) ÷ margin⌉ intervals) and the ramp gate's
  `contender_served_seconds` (T173): Neutral.
- Item 048's C3 Performance row (own RAM stops at its fair share) and C4 (the over-share task
  first): Yield First and Neutral under B; Performance's new expectation is no own step.

**New native gates** (the other session runs them; nothing is estimated here). Plain Linux is
CedeGrid absent with each own process in a session of its own, as in the brief. Each shape runs
at least 3 times each way, in Tracked and in Full, on the test host:

| Gate | Shape | Own work (SC-018) | Other user (SC-019) |
|---|---|---|---|
| F1 | one own process against one other on a core | throughput with ≥ without − spread | rate and p99 within the spread |
| F2 | 8 own processes wanting 4 CPUs against the other user wanting 2 | CPU and units/s with ≥ without − spread | rate and p99 within the spread |
| F3a | one own probe against a saturating user | ratio to alone with ≥ without − spread | rate within the spread |
| F3b | two own probes against a saturating user, both started before the user | as F3a | as F3a |
| F3c | two own probes submitted while the saturating user runs | one probe admitted; the second held (`gpu_time_slice_admission_held`) until the user stops | rate within the spread of one probe beside it |
| F4 | 6 × 1 GiB and 1 × 3 GiB own tasks, the other user's allocator growing to an OOM | no own task hinted, drained or sent TERM (B); every own task signalled | the kernel's victim as in the baseline, or own work |
| SC-014 and SC-015 | idle host, and beside a 1–2% user | unchanged gates, rerun at the new commit | — |

**F2 depends on the other session's first measurement**: plain Linux lands in two places (own
2397–2400 m in 3 runs, 2859 and 3060 m in 2, *measured*), which makes a max − min spread too wide
to gate. Until the cause is known, F2 records the kernel's placement of own tasks in each run and
compares like with like, or uses the median of at least 10 runs each way; the report says which.

## 11. Check against Principles I and III to VIII

- **I**: no actuator is added; the change only removes steps. The verified-set rules, the
  thread-ID rule and the actuator tests stand.
- **III**: in Performance, unknown and occupied states still have explicit, tested behavior:
  block expansion, then hold. Unknown is never free (admission stays closed). The response is
  graduated and bounded: it never escalates, so nothing waits on an unbounded timer.
- **IV**: the policy stays pure; fewer branches, plus one admission hold.
- **V**: unprivileged; nothing new needs privileges.
- **VI**: failing tests first; native evidence for every gate above; the moved gates' old
  evidence is kept, not rewritten.
- **VII**: no config key, wire field or SDK API changes meaning beyond Performance's narrower
  steps; replays of recorded inputs give different Performance decisions, which the snapshot
  review lists.
- **VIII**: `docs/guarantees.md` states the new rule once; the other docs link to it.
