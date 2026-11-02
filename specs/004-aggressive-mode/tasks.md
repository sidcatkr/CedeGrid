---

description: "Task list for 004: Aggressive mode for CPU and GPU compute"
---

# Tasks: Aggressive Mode for CPU and GPU Compute

**Input**: Design documents from `specs/004-aggressive-mode/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Required. Constitution VI (tests first for policy and telemetry) applies to every
group: each test task is written first and must fail before its implementation task.

**Organization**: Tasks are grouped by user story. Implementation is a separate item after 001
is merged into `dev`, on the branch `004-aggressive-mode`. Tasks marked *(needs 003)* wait for
003's surfaces (`run`, `ps`, `top`, per-job entries, profiles, the setup interface, per-user
run-queue wait); the rest stand alone.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: US1 (protected Aggressive, P1) or US2 (unprotected Aggressive, P2)

---

## Phase 1: Setup

- [ ] T001 Create the branch `004-aggressive-mode` from `dev` after 001 is merged, and write the
  baseline decision snapshot from `dev` into a scratch directory with
  `CEDEGRID_DECISION_SNAPSHOT=<dir> CEDEGRID_DECISION_SNAPSHOT_WRITE=1` for the tests
  `decision_snapshot`, `observability`, `policy` and `contention` (quickstart §2), recording the
  directory in the item report.
- [ ] T002 Record which of 003's phases are merged (task schema, local agent, per-user run-queue
  wait, `ps`/`top`, setup interface) in the item report, and follow the plan's order against 003:
  003 foundational → 004 core (Phases 2–3 without *(needs 003)*, Phase 4 core) → rest of 003 →
  004's *(needs 003)* tasks.

---

## Phase 2: Foundational (blocks both stories)

- [ ] T003 Write failing tests in `tests/contention.rs` that `ContentionMode::Aggressive` exists
  after `Performance` in `Ord` and in `ContentionMode::ALL` (four values), with name
  `aggressive`, label `Aggressive`, `memory_allowance_step_percent` 2, a serde round trip, and
  `margin_percent(Aggressive)` 0 and `scale_up_interval_ms(Aggressive)` 0 on compute and 2000
  on memory (data-model "ContentionMode").
- [ ] T004 Add `ContentionMode::Aggressive` in `src/contention.rs` (enum at line 10, `name`,
  `label`, `memory_allowance_step_percent`, `ALL`), make `margin_percent` (line 715) return 0 and
  `scale_up_interval_ms` (line 728) return 0 on compute and 2000 on memory for it, and give it
  Performance's response timing in `LadderTiming::new` (line 839). In `own_allowance`, let it
  take Performance's arms for now (US1 replaces this). Keep configuration refusing `aggressive`
  until T022, so `no_configuration_can_name_aggressive` stays green.
- [ ] T005 Write a failing test in `tests/decision_snapshot.rs` that compares a snapshot with the
  T001 baseline after removing every `aggressive` entry, byte for byte (contract P11, FR-020),
  and a test in `tests/contention.rs` that `ModeInput` with `waiting` empty and `unprotected`
  false gives the same Yield First, Neutral and Performance targets as before.
- [ ] T006 Add `waiting: Vec<(Option<u32>, Option<u64>)>` and `unprotected: bool` to `ModeInput`
  in `src/contention.rs` (line 1234), set them empty and false in every constructor in
  `src/policy/measure.rs` (lines 641, 683, 1087, 1118, 1201, 1273) and in the tests, and add the
  filtered comparison mode to `tests/support/snapshot.rs`.

**Checkpoint**: four modes compile everywhere; existing modes' decisions are byte-identical to
the baseline with `aggressive` entries removed.

---

## Phase 3: User Story 1 - Protected Aggressive (Priority: P1) 🎯 MVP

**Goal**: own work uses all idle compute with no margin and no ramp, never holds less than
Performance in the same observation, and leaves every waiting principal its protected share.

**Independent Test**: quickstart §1–§3: the invariants P1–P9 and P11–P12 in
[contracts/policy-protection.md](contracts/policy-protection.md) pass, configuration and the
node cap behave as in [contracts/cli-sdk-wire.md](contracts/cli-sdk-wire.md), and a light
contender reaches its demand no later than under Performance.

### Tests for User Story 1 (write first, confirm they fail)

- [ ] T007 [P] [US1] In `tests/contention.rs` `ordered_targets`, extend `targets()` to four modes
  and the property to `performance <= aggressive` for all four resource kinds, with random
  `waiting` lists (known and unknown demands), `unknown_activity`, `saturated`, `satisfied` and
  `probe` (contract P1).
- [ ] T008 [P] [US1] In `tests/contention.rs`, report 018's case (contract P2): capacity 8000 m,
  own work and one light principal using 200 m that waits with demand 1000 m and grows by 160 m
  per interval; assert Aggressive targets 7640, 7480, 7320, 7160, 7000, 7000 m and that
  Performance ends at 6840 m. Repeat the case on a GPU compute resource in device units (SC-001).
- [ ] T009 [P] [US1] In `tests/contention.rs`, the single-observation cases: Performance at or
  below the protected point gives exactly the protected target (P3); a known demand below the
  fair share is kept exactly, an unknown or larger demand keeps the fair share, and wait never
  takes own work below its fair share (P12); nobody waiting and no unknown activity gives
  capacity − external (P8); unknown activity holds the fair share (P9).
- [ ] T010 [P] [US1] In `tests/contention.rs`, several waiting light principals each keep their
  protected share and Aggressive is never below Performance or own work's fair share (P4); RAM
  and GPU memory targets equal Performance's in the same `ModeInput` (P7).
- [ ] T011 [P] [US1] In `tests/policy.rs`, scenario tests over intervals: Aggressive ≥ Performance
  per resource and observation in the existing monotonic scenarios (`modes_admit_monotonically`,
  `modes_respond_to_psi_monotonically` and the contention scenarios) (P1); several light users
  waiting at once (P4); a protected user stops waiting, then waits again (P5); load switching
  every 4 monitor intervals changes Aggressive's step at most once per switch (P6, 001 SC-009);
  a held compute cap rises straight to capacity − external once calm (P8); memory hints and
  drains only at a floor breach, as Performance (P7, R4); every action under Aggressive is one of
  001's step kinds, never a nice decrease or a thread change (FR-017); Guaranteed work is never
  paused; a node cap lowered below Aggressive moves the run to the new cap's mode from the next
  interval.
- [ ] T012 [P] [US1] In `tests/decision_snapshot.rs`, assert in every generated random sequence
  that each decision's Aggressive target is at least its Performance target per resource (P1).
- [ ] T013 [P] [US1] In `tests/agent.rs`, a `launch_slots` test that an effective mode of
  Aggressive fills every free slot in one heartbeat, as Performance does (SC-003).
- [ ] T014 [P] [US1] In `tests/observability.rs`, decisions show `held_back.protected_share` and
  reason `aggressive_protected_share` when the protected target is the larger, and 0 while
  Aggressive equals Performance (data-model "Decision output"); `doctor` and `observe` show the
  red `[Aggressive]` label, plain with `NO_COLOR` (FR-007).
- [ ] T015 [P] [US1] In `tests/configuration.rs`, replace `no_configuration_can_name_aggressive`:
  `aggressive` as `default_mode`, a resource mode or `max_mode` fails without `aggressive_mode`
  in `contention.acknowledge_caution` and loads with it; with `default_mode = "aggressive"` and
  no explicit `max_mode`, the cap is `performance` and a job asking for Aggressive runs at
  Performance with `mode_capped` (FR-008). Keep the unprotected keys refusing (moved to T034).
- [ ] T016 [P] [US1] In `tests/cli.rs`, `cedegrid setup --mode aggressive --acknowledge
  aggressive_mode` and `--max-mode aggressive --acknowledge aggressive_mode` write the same
  configuration as the keyboard interface (001 SC-011), show a red `[Aggressive]` label, and
  plain text with `NO_COLOR`; the caution confirmation says that own work stays the preferred
  OOM victim, so a sudden external allocation may kill it before it checkpoints (FR-006).
- [ ] T017 [P] [US1] In `tests/contracts/cedegrid-0.2.json`, flip JC-05 (`"cpu": "aggressive"`)
  to accepted, and run it in `tests/coordinator.rs`, `python/tests/test_transport.py` and
  `npm/tests/sdk.test.ts`; the coordinator accepts Aggressive within the node cap (FR-009).

### Implementation for User Story 1

- [ ] T018 [US1] Implement `protected_allowance` in `src/contention.rs` beside `neutral_share`
  (line 1655): the max-min share of `capacity - background` for own work against waiting
  principals at their demand (unbounded when unknown), other principals at their usage, the
  unattributed usage, and one unbounded principal while `unknown_activity`, minus owner-unmanaged
  usage (contract "Rule").
- [ ] T019 [US1] Replace the T004 placeholder with the Aggressive arm in `own_allowance`
  (`src/contention.rs:1325`, after the memory arm): Performance's target for the same `ModeInput`
  (`mode = Performance`), and `performance.max(protected_allowance(input, thresholds))`; update
  the function's doc to Yield First ≤ Neutral ≤ Performance ≤ Aggressive.
- [ ] T020 [US1] Fill `ModeInput.waiting` in `src/policy/measure.rs` (CPU at line 641, GPU compute
  at 1087 and 1201): every principal 001 counts as contending (saturated, at least the
  contention floor, demand not satisfied) or PSI-only (FR-072) waits with an unknown demand; a
  demand the probe found satisfied ends waiting (R3). Leave the run-queue-wait demand to T026.
- [ ] T021 [US1] In `src/agent/service.rs` (line 928), make `fill` true for an effective mode at
  or above Performance.
- [ ] T022 [US1] In `src/config.rs`, add `aggressive_mode` to the caution acknowledgements (line
  574), require it for any `aggressive` mode or `max_mode` (validation near line 814), and stop
  the derived node cap (`max_mode`, line 747) at `performance` unless `max_mode` is set
  explicitly; in `src/setup.rs`, accept `aggressive` (line 255), its margin key and label (lines
  751, 1441) and the picks (lines 1671–1812), with the red `[Aggressive]` label and the FR-006
  confirmation text.
- [ ] T023 [US1] Add `held_back.protected_share` and the catalog reason `aggressive_protected_share`
  in `src/contention.rs` and `src/policy/measure.rs`, and show both, with the `[Aggressive]`
  label, in `src/explain.rs` (`observe`, `doctor`).
- [ ] T024 [US1] Accept `aggressive` in job modes in `src/protocol.rs` and `src/coordinator.rs`
  within the node cap, and in `python/cedegrid/` and `npm/src/` (mode value only).

### User Story 1 parts that need 003

- [ ] T025 [US1] *(needs 003)* Write failing tests in `tests/run_cli.rs`: `cedegrid run --mode
  aggressive` runs within the node cap; `ps`, `top`, `status` and JSON show the red
  `[Aggressive]` label (`NO_COLOR`: plain); a per-job entry or profile may store protected
  Aggressive with `aggressive_mode` (FR-009), and refuses it without.
- [ ] T026 [US1] *(needs 003)* Feed 003's per-user run-queue wait (`src/telemetry/runqueue.rs`)
  into `ModeInput.waiting` in `src/policy/measure.rs`: a principal whose wait rate is above the
  threshold waits, with demand = usage + unserved runnable time over the window (R3); test first
  in `tests/policy.rs` that report 018's case then holds 7000 m on a shared-cgroup scenario.
- [ ] T027 [US1] *(needs 003)* Implement the label in `src/run.rs`, `src/run/scope.rs` (stored
  protected Aggressive with its acknowledgement) and `src/top.rs`, until T025 passes.

**Checkpoint**: protected Aggressive is usable from configuration, the SDKs and the wire, and
satisfies P1–P9 and P11–P12.

---

## Phase 4: User Story 2 - Unprotected Aggressive for one run (Priority: P2)

**Goal**: for one explicitly confirmed run, own work never steps compute down for external
demand; nothing stored can turn it on; it is marked while active.

**Independent Test**: quickstart §3–§5: every refusal path refuses with its message, the
permitted paths start an unprotected run, P10 holds, and the marker appears only while the run
is active.

### Tests for User Story 2 (write first, confirm they fail)

- [ ] T028 [P] [US2] In `tests/contention.rs`, contract P10: with `unprotected`, the target is at
  least protected Aggressive's and never below own usage; under unknown activity it is at most
  the larger of the fair share and own usage; item 022's rule leaves it unchanged (it equals
  `max(max(performance, protected), own)`).
- [ ] T029 [P] [US2] In `tests/policy.rs`, an unprotected run beside a contender that grows at
  every demand level gets zero compute step-downs, while a RAM floor breach still takes the
  normal floor path (SC-004, 001 SC-012); Guaranteed work is never paused; no action raises
  priority or changes thread counts (FR-017).
- [ ] T030 [P] [US2] In `tests/state.rs`, the global allowance record: `enabled` is off by
  default, `set_at_unix_ms` is recorded, `set_by` is always `interactive_setup`, and turning it
  off leaves active grants running (data-model "Global allowance").
- [ ] T031 [P] [US2] In `tests/agent.rs`, the grant check before Authorized: an
  `interactive_phrase` grant needs the agent's challenge ("valid 120 s, used once"); a
  `flag_and_allowance` grant needs `confirm_unprotected_aggressive` and the allowance; a refused
  request never becomes active; a drained and retried run keeps its grant; a new run needs a new
  grant; lowering the node cap below Aggressive ends the grant from the next interval; a request
  is refused when the node cap is below Aggressive, naming the cap (FR-011) (data-model
  "Unprotected grant").
- [ ] T032 [P] [US2] In `tests/contracts/cedegrid-0.2.json`, add the cases: an unprotected request
  without confirmation, with the flag but no allowance, and with an expired challenge, each
  rejected; run them in `tests/coordinator.rs`, `python/tests/test_transport.py` and
  `npm/tests/sdk.test.ts`.
- [ ] T033 [P] [US2] In `python/tests/` and `npm/tests/`, a parity test that fails when the CLI's
  Aggressive and unprotected options and either SDK's fields differ (FR-019).
- [ ] T034 [P] [US2] In `tests/configuration.rs`, `unprotected_aggressive` and
  `allow_unprotected_aggressive` in any configuration section fail to load with
  `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE` naming the key, also after `src/config_migration.rs`
  migrates a 0.2 configuration that contains one; environment variables cannot enable it
  (FR-012).
- [ ] T035 [P] [US2] In `tests/observability.rs`, a run's JSON shows `"unprotected": true` with its
  grant while active, logs carry one warning line at its start, and neither appears after the
  run ends (FR-016).

### Implementation for User Story 2

- [ ] T036 [US2] In `own_allowance` (`src/contention.rs`), return `max(aggressive target, own)`
  when `input.unprotected`, and set `unprotected` for the granted run's allocations in
  `src/policy/measure.rs`.
- [ ] T037 [US2] Add the global allowance record to `src/state.rs` (`enabled`, `set_at_unix_ms`,
  `set_by`), written only through the interactive setup path.
- [ ] T038 [US2] Add the job field `unprotected_aggressive { confirmed, challenge,
  requested_unix_ms }` to `src/protocol.rs`, verify it in `src/agent/launch.rs` before the launch
  barrier persists Authorized, and keep the grant's lifetime (ends with the run; retries keep it;
  a lowered node cap ends it) in `src/agent/service.rs`.
- [ ] T039 [US2] Refuse the unprotected keys in `src/config.rs` (and after migration in
  `src/config_migration.rs`) with
  `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE: <key> cannot be stored; unprotected Aggressive is
  turned on per run only`.
- [ ] T040 [US2] Add `unprotected_aggressive` and `confirm_unprotected_aggressive` (booleans,
  default false) to `python/cedegrid/` and `npm/src/`.
- [ ] T041 [US2] Show `"unprotected": true` and the grant in `src/explain.rs` and status JSON, and
  print the warning line `warning: this run is UNPROTECTED: other users' compute may slow down
  until it ends` once at start from `src/agent/service.rs`.

### User Story 2 parts that need 003

- [ ] T042 [US2] *(needs 003)* Write failing tests in `tests/run_cli.rs`: the interactive start
  shows the agent's numbers screen (other active principals, their CPU use, an estimated light-user
  slowdown labeled as an estimate, and the FR-006 sentence that own work may be killed by a
  sudden external allocation before it checkpoints) and `Start this run unprotected? [y/n]` with
  no default, then
  requires `slow other users for this run`; anything else cancels with `cancelled: the phrase did
  not match; nothing started`; a non-interactive start without `--confirm-unprotected-aggressive`
  or with the allowance off is refused naming each missing condition; `--save` refuses with
  `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE`; per-job entries and profiles cannot store it; `ps` and
  `top` show a red `UNPROTECTED` marker only while the run is active.
- [ ] T043 [US2] *(needs 003)* Implement `--unprotected-aggressive` and
  `--confirm-unprotected-aggressive` (no short aliases) in `src/run.rs` and `src/run/client.rs`,
  the numbers screen and phrase over the local agent socket, the refusals in `src/run/scope.rs`,
  and the marker in `src/top.rs`, until T042 passes.
- [ ] T044 [US2] *(needs 003)* Write failing tests in `tests/cli.rs`, then add to
  `src/setup/interface.rs` and `src/setup.rs` the item "Allow unprotected Aggressive for
  non-interactive runs" with its own confirmation on a terminal, recorded with its time;
  non-interactive setup, the SDK and agent tooling are refused with
  `ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY`.

**Checkpoint**: both stories work; every refusal path in
[contracts/cli-sdk-wire.md](contracts/cli-sdk-wire.md) refuses.

---

## Phase 5: Polish & Cross-Cutting Concerns

- [ ] T045 [P] In `docs/guarantees.md`, replace "an unprotected Aggressive mode, which is not
  implemented" and "no configuration key or job field can name Aggressive" with the mode, its
  rule (never less than Performance in the same observation; a waiting principal gets its share
  back no slower than Performance gives it), the unprotected exception and its markers, and the
  `held_back.protected_share` row; keep host descriptions generic.
- [ ] T046 [P] Add `--mode {performance,aggressive}` and a protected/unprotected contender run to
  `tools/ramp_gate.py`, tests first in `tests/test_ramp_gate.py`, and the exact commands to
  `specs/004-aggressive-mode/quickstart.md` §6.
- [ ] T047 Run the item-end checks (fmt, clippy, the 1.88 check, the serial suite with
  `CEDEGRID_SERVICE_TEST_MONITOR_MS=500`, Python, npm, the Docker Linux check on a `git archive`)
  and quickstart §1–§5, and compare the decision snapshot with the T001 baseline (P11).
- [ ] T048 Native gates on the authorized test host (SC-008), only with the user's approval in the
  session: Aggressive against Performance (ramp time, idle capacity used), contender slowdown
  with and without the protection, a same-cgroup and a cross-cgroup contender; record results in
  `docs/validation.md`. *Deferred: needs the authorized host and, for the cross-cgroup contender,
  another account or a system slice.*
- [ ] T049 Before Aggressive ships, measure the GPU time-slice duty's switch cost at a zero
  margin (feature 001 item 046, Decision C): keep each managed window's raw phase samples and
  compare the other user's request rate while own work runs with its rate while own work is
  stopped, beside the saturating user with two own probes, on the authorized test host. If the
  cost is CedeGrid's, fold it into d0 as item 046 did for the re-measure windows; if it is the
  time slicer's own, record it as within the gate's tolerance. Record the result in
  `docs/validation.md`. *Blocks shipping Aggressive; host runs need the user's approval.*

---

## Dependencies & Execution Order

### Phase dependencies

- Setup (T001–T002) → Foundational (T003–T006) → US1 → US2 → Polish.
- US2 uses US1's Aggressive target (T019) as its floor, so it starts after T019.
- T049 needs US1's GPU compute path and comes before Aggressive ships.
- *(needs 003)* tasks wait for the matching 003 phase: T025–T027 and T042–T044 for `run`, `ps`,
  `top`, per-job entries, profiles and the setup interface; T026 for per-user run-queue wait.

### Order against 003 (plan)

1. 003's setup and foundational phases (task schema, local agent, per-user wait evidence).
2. 004's core: T001–T024 and T028–T041.
3. The rest of 003 (`ps`, `top`, setup interface).
4. 004's *(needs 003)* tasks: T025–T027, T042–T044, then T045–T048.

Running all of 003 before all of 004 also works.

### Within each story

Tests first and failing, then policy (`src/contention.rs`), then measurement and agent, then
configuration, wire and SDKs, then the CLI parts.

### Parallel opportunities

- T007–T017 touch different test files or independent sections and can be written together.
- T028–T035 likewise.
- T045 and T046 are independent of each other.

## Parallel Example: User Story 1

```text
T007 tests/contention.rs ordering property     T011 tests/policy.rs scenarios
T012 tests/decision_snapshot.rs random check   T013 tests/agent.rs launch_slots
T014 tests/observability.rs held_back          T015 tests/configuration.rs acknowledgement
T016 tests/cli.rs setup                        T017 contract fixture JC-05
```

## Implementation Strategy

### MVP first

Phases 1–3 without the *(needs 003)* tasks: protected Aggressive from configuration, SDKs and the
wire, with the ordering and the protection proven by tests. Stop and validate with quickstart
§1–§4.

### Incremental delivery

1. MVP (US1 core) → 2. US2 core (grant, refusals, markers in JSON and logs) → 3. the
*(needs 003)* CLI parts → 4. Polish and native gates.

## Task count

| Phase | Tasks |
|---|---|
| Setup | 2 (T001–T002) |
| Foundational | 4 (T003–T006) |
| US1 (P1) | 21 (T007–T027; 3 need 003) |
| US2 (P2) | 17 (T028–T044; 3 need 003) |
| Polish | 5 (T045–T049; T048 and T049 on the test host, T049 before shipping) |
| **Total** | **49** |

## Phase 6: Convergence (item 048)

Item 048's converge against constitution 7.0.0 and feature 001's GPU time-slice duty. Spec changes go through `/speckit-clarify` first; each code task starts with a failing test.

- [ ] T050 CRITICAL: Rewrite plan.md's Constitution Check row II and research R2 against 7.0.0: the OOM order by fault, the memory floors down to the fair share, and the loss rule by mode (Aggressive: a 0% margin plus CedeGrid's measured overhead). Per Constitution II (contradicts)
- [ ] T051 CRITICAL: Change T016 and T042 to check FR-006's confirmation sentence (the kernel may take another workload's process, or own work as the largest process, before own work checkpoints), not "own work stays the preferred OOM victim". Per Constitution II, FR-006 (contradicts)
- [ ] T052 CRITICAL: In data-model.md, mark Performance as the default mode with no caution acknowledgement (`performance_mode`); Aggressive stays the caution option. Per Constitution II (contradicts)
- [ ] T053 Specify in spec.md and plan.md how unprotected Aggressive treats 001's time-slice duty, which today replaces every mode's GPU compute target but Yield First's with the duty share (`src/policy/measure.rs`), from FR-010's unprotected rule and with the user's decision where FR-010 does not settle it; then test-first tasks in tests/policy.rs. Per FR-010, SC-004 (missing)
- [ ] T054 State Aggressive's zero-margin duty (d0 750, d0' 725 at m = 2, E = 1) in FR-018 and plan.md, add T049 to "Order against 003", and add a test-first task after T049 that folds the measured switch cost into d0 if it is CedeGrid's. Per FR-002, FR-003, FR-018, SC-001, 001 FR-021 (partial)
- [ ] T055 Make 001's time-slice duty an exception to FR-002's "rises straight to what the mode allows once calm", or state its step rule, in spec.md, contracts/policy-protection.md P8 and T011. Per FR-002, 001 FR-021 (contradicts)
- [ ] T056 Add GPU compute light and saturating contender runs to T048 and quickstart.md §6. Per SC-001, SC-002, Constitution VI (partial)
- [ ] T057 Add to T031 or T042 a case where an interactive start succeeds with the allowance off. Per FR-015 (partial)
- [ ] T058 Extend T010 and T029 with the OOM order by fault, the VRAM floors and the operator envelope under Aggressive. Per FR-010, FR-018 (partial)
- [ ] T059 Add an absence check for the `[Aggressive]` label to T014 and T025 when the run is not in that mode. Per SC-006 (partial)
- [ ] T060 Limit T026 to shared-cgroup hosts, or add a per-user-cgroup case in tests/policy.rs where the contender's demand comes from its usage plus the probe. Per FR-004 (partial)
