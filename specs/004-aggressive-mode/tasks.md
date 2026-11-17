---

description: "Task list for 004: Aggressive mode for CPU and GPU compute"
---

# Tasks: Aggressive Mode for CPU and GPU Compute

**Input**: Design documents from `specs/004-aggressive-mode/` (reworked 2026-11-17 against
constitution 8.1.0)

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Required. Constitution VI applies to every group: each test task is written first and
must fail before its implementation task.

**Organization**: Tasks are grouped by user story. Each task carries one of two marks:
- **(now)**: it can start on the branch while 001 lands T622, because it touches none of 001's
  held files;
- **(after T622)**: it touches `src/policy.rs`, `src/policy/*`, `src/contention.rs`, `src/model.rs`
  or `src/explain.rs`, or needs `ContentionMode::Aggressive`. It waits until the coordinator says
  T622 has landed and 001 is merged into the branch.

003's CLI parts are out of this release; they are listed at the end as later work.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: US1 (protected Aggressive, P1) or US2 (unprotected Aggressive, P2)

---

## Phase 1: Setup

- [x] T001 (after T622) Merge 001 at the coordinator's commit into the branch. Then write the
  decision snapshot of the merged tip into a scratch directory with
  `CEDEGRID_DECISION_SNAPSHOT=<dir> cargo test --locked --test decision_snapshot -- --ignored --exact decision_snapshot`,
  and record the commit in `specs/004-aggressive-mode/tasks.md` under this task. This is the
  baseline for P9.
  - *Done:* the baseline is `4e01e93`, the merged tip before any 004 policy change.

---

## Phase 2: Foundational (blocks both stories)

- [x] T002 [P] (now) Write the allowance tests in `tests/state.rs`. Cover: no table, no row, a
  corrupt database file, an unreadable file, a schema version this build does not read. Each
  case reads as off and produces a one-line reason. Also: set with `set_by = "interactive_setup"`
  reads on, with its `set_at_unix_ms`; clearing works from any caller; a restore through
  `cedegrid::backup::restore` reads off afterwards. Data model: "`enabled`: off by default",
  "`set_by`: always `interactive_setup`".
- [x] T003 (now) Implement the allowance in `src/state.rs`: an additive table
  `unprotected_allowance(enabled INTEGER NOT NULL, set_at_unix_ms INTEGER, set_by TEXT)` created
  if absent, a read that returns `Off` with a reason on every error, `set_on(at)` for interactive
  setup only, and `clear()`. In `src/backup.rs` `restore`, reset it to off after the restore.
  Passes T002.
  - *Done:* `state::{Allowance, read_allowance, set_allowance, clear_allowance}`. Reads also
    count a row not set by interactive setup, or a database reached through a link, as off.
    `backup::restore` clears it, and `tests/backup.rs` fails without that.
- [x] T004 [P] (after T622) Write the mode-value tests in `tests/contention.rs` and
  `tests/configuration.rs`:
  - `aggressive` parses and names in configuration, jobs and the wire, with order 3 and the red
    `[Aggressive]` label, and is the last entry of `ContentionMode::ALL`;
  - `margin_percent(Aggressive)` is 0 on compute and is no configuration key;
  - `scale_up_interval_ms(Aggressive)` is 0 on compute and 2000 on memory;
  - `aggressive` without `aggressive_mode` in `contention.acknowledge_caution` fails to load, and
    with it, loads;
  - `max_mode` stops at `performance` unless it is set to `aggressive` explicitly.
- [x] T005 (after T622) Implement `ContentionMode::Aggressive` in `src/contention.rs` (name,
  label, order, `ALL`, margin, scale-up interval, Performance's response timing). In
  `src/config.rs`, implement the `aggressive_mode` acknowledgement and the explicit `max_mode`
  rule, with `mode_capped` below Aggressive. Fix every `match` the compiler flags. Passes T004.
  - *Done:* `ContentionConfig::mode` is the configured mode within the cap
    (`configured_mode` is the raw one), the derived cap stops at Performance with a load warning
    naming `mode_capped`, and setup's mode and margin lists stay at the three margined modes
    until T014.

**Checkpoint**: the allowance and the mode value exist; both stories can proceed.

---

## Phase 3: User Story 1 - Protected Aggressive (Priority: P1) 🎯 MVP

**Goal**: own work in Aggressive fills idle compute at once with no margin, and new own work never
takes another principal below the lesser of its demand and its fair share. Running own work is
never capped, paused or drained for another's compute.

**Independent Test**: quickstart §1 to §4 and §6 (protected run); native gate §7 F2c in protected
Aggressive.

### Tests for User Story 1 (write first, confirm they fail)

- [ ] T006 [P] [US1] (after T622) In `tests/contention.rs`, write invariants P1 to P5 and P10 of
  `contracts/policy-protection.md` as unit and property tests against `own_allowance`, including
  the ordering property with Aggressive in the random generator.
- [ ] T007 [P] [US1] (after T622) In `tests/policy.rs`, write:
  - the F2c scenario: own tasks submitted while another principal saturates two of four CPUs, in
    Performance and in Aggressive. Aggressive admits up to `max(fair share, available)` and the
    other principal keeps the lesser of its demand and its fair share;
  - P6: no compute cap, pause or drain on an Aggressive allocation for external demand;
  - P7: no hint, drain, TERM or OOM mark at a floor breach in Aggressive;
  - FR-017: neither form changes own priority or thread counts: every launch in Aggressive keeps
    the nice value and scheduling class it would have in Performance.
  - *Done in part:* P6 and P7 extend Performance's no-step and floor-breach tests to Aggressive,
    and FR-017 is `tests/configuration.rs` `an_aggressive_job_launches_at_the_default_priority`.
    The F2c arithmetic is `tests/contention.rs` `aggressive::contended_it_is_performance_without_the_margin`;
    the engine-level F2c scenario is the native gate's (T035).
- [x] T008 [P] [US1] (now) In `python/tests` and `npm/tests`, test that the per-resource mode
  value `"aggressive"` is accepted by both SDKs and serialized as in
  `contracts/cli-sdk-wire.md`.

### Implementation for User Story 1

- [x] T009 [US1] (after T622) In `src/contention.rs` `own_allowance`, give Aggressive
  Performance's arms with the 0% margin. Contended compute is `max(neutral_share, available,
  probe)`, uncontended compute is `available`, unknown activity goes to the shared unknown arm,
  and memory shares Performance's arm. Passes T006.
- [ ] T010 [US1] (after T622) In `src/policy.rs`, `src/policy/ladder.rs` and
  `src/policy/admission.rs`, treat Aggressive as Performance in every "no running step" path.
  Admission fills what fits in one heartbeat (scale-up 0), and `targets_of` in
  `src/policy/measure.rs` gains the `aggressive` target. Passes T007.
- [x] T011 [US1] (after T622) In `src/agent/service.rs`, `fill` is true for an effective mode at
  or above Performance, Aggressive included.
- [x] T012 [P] [US1] (after T622) In `src/protocol.rs` and `src/coordinator.rs`, accept the job
  mode `aggressive`, with the node cap deciding the effective mode. Flip contract case JC-05 to
  accepted in `tests/contracts/cedegrid-0.2.json`, with `mode_capped` below Aggressive.
- [x] T013 [P] [US1] (now) In `python/cedegrid/` and `npm/src/`, add the mode value
  `"aggressive"`. Passes T008.
- [ ] T014 [US1] (after T622) In `src/setup.rs` and `src/setup/interface.rs`, add the cap step to
  Aggressive with its own confirmation and the red `[Aggressive]` label. Non-interactively, it
  needs `--max-mode aggressive --acknowledge aggressive_mode`.
  - *Done in part:* the flags path (`--default-mode aggressive --max-mode aggressive
    --acknowledge-caution aggressive_mode`) and the caution mapping (`risk` returns
    `aggressive_mode` for Aggressive in every mode field). The interactive interface still lists
    the three margined modes; offering Aggressive there means re-pinning the interface tests'
    row and option positions.
- [x] T015 [US1] (after T622) In `src/explain.rs`, show the red `[Aggressive]` in `observe`,
  `doctor` and `status` and their JSON (`"mode": "aggressive"`), plain under `NO_COLOR`.
  - *Done:* `[Aggressive]` in the mode lines, and doctor's caution list names `aggressive_mode`
    with the settings that call for it. Decision JSON carries `"aggressive"`. `explain.rs` has no
    color today, so the marker is the bracketed text; color comes with 003's terminal outputs.
- [x] T016 [US1] (after T622) Compare the decision snapshot against T001's baseline. With the
  `aggressive` entries filtered out, every decision is byte-identical (P9, FR-020).
  - *Done:* the policy, contention, observability and decision_snapshot scenarios at `4e01e93`
    and at `860f82d`: 42,449 lines, none different once the `aggressive` entries are removed.

**Checkpoint**: protected Aggressive works through configuration, setup, the SDKs and the wire.

---

## Phase 4: User Story 2 - Unprotected Aggressive for one run (Priority: P2)

**Goal**: one submission, with both SDK fields, the host's allowance, the node cap at Aggressive
and, in System mode, the administrator key, may start new own compute work where another
principal contends. Nothing else changes.

**Independent Test**: quickstart §3 to §6; native gate §7 F2c unprotected.

### Tests for User Story 2 (write first, confirm they fail)

- [ ] T017 [P] [US2] (now) In `tests/configuration.rs`, test that `unprotected_aggressive` and
  `allow_unprotected_aggressive` at any table depth fail to load with
  `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE: <key> cannot be stored; unprotected Aggressive is requested per submission only`,
  and that the same happens after `config_migration` migrates a 0.2 file that holds one.
- [x] T018 [P] [US2] (now) In `tests/cli.rs`, test interactive setup with a scripted terminal:
  - "yes" plus the exact phrase `slow other users' compute on this host` turns the allowance on;
  - "no", an empty answer or a wrong phrase leaves it off and prints `cancelled: the phrase did not match; the allowance stays off` where it applies;
  - non-interactive setup refuses to turn it on with `ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY`;
  - `cedegrid setup --disallow-unprotected-aggressive` turns it off without confirmation.
- [ ] T019 [P] [US2] (now) In `tests/agent.rs`, test the launch grant check. Each missing
  condition (the confirmation field, the allowance, an unreadable allowance, the node cap) is
  refused before Authorized with its line from `contracts/cli-sdk-wire.md` "Refusals at
  launch", and nothing starts. With every condition met, the run is Authorized and logs the
  warning line once. A drained and retried attempt keeps the grant, and a new run without the
  fields is protected. Turning the allowance off while the run is active keeps its grant and
  refuses new grants. Lowering the node cap below Aggressive ends the grant for tasks admitted
  after it. The node-cap cases need `ContentionMode::Aggressive`, so add them with T005.
- [ ] T020 [P] [US2] (now) In `tests/system_mode.rs`, test:
  - `admin_policy` parses `modes.unprotected_aggressive` (default `false`, `deny_unknown_fields`);
  - `launch_refusal_with` refuses a grant while it is false or absent, with
    `ERR_CEDEGRID_UNPROTECTED_POLICY`, and allows it when true;
  - a container fixture `container::unprotected_aggressive_follows_the_policy_key`.
- [x] T021 [P] [US2] (now) In `python/tests`, `npm/tests` and `tests/contracts/cedegrid-0.2.json`,
  test the two SDK fields `unprotected_aggressive` and `confirm_unprotected_aggressive` (bool,
  default false). The wire object `unprotected_aggressive: { "confirmed": true,
  "requested_unix_ms": <u64> }` is set only when both are true. Add the new contract cases:
  the grant without `confirmed` is rejected at submission, the grant on a node capped at
  Performance, the grant with the allowance off. Add the SDK parity test (FR-019).
- [x] T022 [P] [US2] (after T622) In `tests/contention.rs`, test the unprotected allowance: it is
  `capacity` on compute without unknown activity (P8), Aggressive's under unknown activity (P5),
  and Performance's on memory.

### Implementation for User Story 2

- [ ] T023 [US2] (now) In `src/config.rs` and `src/config_migration.rs`, refuse every key that
  would enable unprotected Aggressive. Passes T017.
- [x] T024 [US2] (now) In `src/setup.rs` and `src/setup/interface.rs`, add the allowance step.
  The numbers screen (R11) shows other active principals, their CPU use and an estimated
  light-user slowdown, labelled "estimate", from the agent's latest observation, and says that
  memory floors and OOM handling are unchanged by it (FR-006). Then a yes/no
  question with no default, then the fixed phrase. Add `--disallow-unprotected-aggressive`.
  Writes go through T003. Passes T018.
- [x] T025 [US2] (now) In `src/protocol.rs` and `src/coordinator.rs`, add the grant object. The
  coordinator rejects a grant without `confirmed: true` at submission and otherwise passes it to
  the agent unchanged. Passes the Rust half of T021.
- [ ] T026 [US2] (now) In `src/agent/launch.rs`, check the grant before the launch barrier
  persists Authorized: both fields, the allowance (T003, fail closed), the node cap at
  Aggressive, and inside a System mode unit the administrator key (T027). Record the grant with
  the attempt so retries keep it, and log `warning: this run is UNPROTECTED: other users' compute may slow down until it ends`
  once at start. Passes T019. Add the node-cap condition after T005.
  - *Done in part:* `system_mode::grant_refusal` names every missing condition in one line, and
    the agent calls it before launching, with the administrator key inside a unit, and emits
    the warning event. The pure function is tested in `tests/system_mode.rs`. The end-to-end
    agent test (T019) and the attempt record for retries are open.
- [x] T027 [US2] (now) In `src/broker/admin_policy.rs`, add the key `modes.unprotected_aggressive`
  (default `false`). In `src/system_mode.rs` `launch_refusal_with`, refuse a grant while it is
  false. Update `specs/005-system-mode/contracts/admin-policy.md` and the policy section of
  `docs/system-mode.md`. Passes T020.
  - *Done in part:* the key, `system_mode::unprotected_refusal_with` and its unit test. The launch
    check calls it in T026; the container fixture comes with T026.
- [x] T028 [P] [US2] (now) In `python/cedegrid/` and `npm/src/`, add the two fields and the wire
  object. Passes the SDK half of T021.
- [x] T029 [US2] (after T622) In `src/contention.rs` (`ModeInput.unprotected`, the unprotected
  arm of `own_allowance`) and `src/policy/admission.rs` and `src/policy/measure.rs` (set from
  the allocation's grant), raise admission for granted allocations only. Passes T022.
  - *Done as R12:* the policy computes `Decision.unprotected_budget` where the cap is Aggressive;
    the agent reports it while the allowance (and, in a unit, the key) permits; the coordinator
    places a granted request against it. Tests: `tests/policy.rs`
    `the_unprotected_budget_exists_only_under_an_aggressive_cap`, `tests/coordinator.rs`
    `a_granted_request_is_placed_against_the_unprotected_budget`.
- [ ] T030 [US2] (after T622) In `src/explain.rs`, show the red `UNPROTECTED` marker in
  `observe`, `doctor` and `status`. JSON gains `"unprotected": true` and the grant; the marker is
  plain under `NO_COLOR` and absent once the run ends (SC-006).

**Checkpoint**: an unprotected run starts only through the permitted path; every refusal path
refuses (SC-005).

---

## Phase 5: Polish & Cross-Cutting Concerns

- [x] T031 [P] (now) In `docs/guarantees.md`, replace "which is not implemented" with the mode,
  admission-only protection and the unprotected exception, generically. In `docs/system-mode.md`,
  add the administrator key.
- [x] T032 [P] (now) For any new file that ships (none is planned), add it to `ALLOWLIST` in
  `tools/pack_source_review.py` and to `include` in `Cargo.toml`. Run
  `python3 tools/pack_source_review.py` and confirm it passes.
  - *Done:* 004 added no file that ships.
- [ ] T033 (after T622) Run the full checks from `CLAUDE.md`: `cargo fmt`, `cargo clippy
  --all-targets -D warnings`, the serial Rust suite, `cargo +1.88.0 check`, both Python suites,
  `npm test`, the Windows client paths, and the system-mode container suite.
- [ ] T034 (after T622) Hand the security review of the unprotected admission path to the review
  before release. List the files in `specs/004-aggressive-mode/security-review.md`: the grant
  check in `src/agent/launch.rs`, the allowance in `src/state.rs` and `src/backup.rs`, the
  refusals in `src/config.rs`, `src/config_migration.rs` and `src/coordinator.rs`, the setup step,
  and the administrator key. Add the tests that pin each one.
- [ ] T035 (after T622) Run the native gates in quickstart §7 on the test host, at a time the
  coordinator schedules after 001's E9. Record the results in `docs/validation.md` under a
  feature 004 section, at least 3 runs each way.

---

## Later work (003, not this release)

- `run --mode aggressive`, `--unprotected-aggressive` and `--confirm-unprotected-aggressive` on
  003's `run`, with no short aliases, joining the parity test.
- `--save` refusing to store the unprotected flag (`ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE`).
- Per-job entries and profiles with `aggressive`.
- `ps` and `top` showing `[Aggressive]` and `UNPROTECTED`.
- 003's per-user run-queue wait as demand evidence (R3).

---

## Dependencies & Execution Order

### Phase dependencies

- Phase 2's (now) tasks T002 and T003 start at once. T004 and T005 wait for T622 and T001.
- US1 and US2 depend on Phase 2. Within each story, tests come before implementation.
- US2's (now) tasks depend only on T003, not on US1.
- Polish comes after both stories, except T031 and T032, which can run any time.

### Within each story

- The tests must fail before their implementation.
- `src/protocol.rs` tasks T012 and T025 touch the same file; do T025 first, since it is (now).
- `src/setup.rs` tasks T014 and T024: T024 first.
- `src/explain.rs` tasks T015 and T030: one change.

### Parallel opportunities

- Now: T002, T008, T017, T018, T019, T020 and T021 are tests in different files, so they run in
  parallel. T013 and T028 (SDKs) run beside the Rust tasks.
- After T622: T006, T007 and T022 are tests in parallel; T012 runs beside T009 and T010.

---

## Parallel Example: (now) work while 001 lands T622

```text
Task: "T002 allowance tests in tests/state.rs"
Task: "T017 stored-key refusal tests in tests/configuration.rs"
Task: "T019 launch grant tests in tests/agent.rs"
Task: "T020 administrator key tests in tests/system_mode.rs"
Task: "T021 SDK and contract tests in python/tests, npm/tests, tests/contracts/cedegrid-0.2.json"
```

---

## Implementation Strategy

### MVP first

1. Phase 2's (now) part, then US2's (now) part: the allowance, refusals, the grant check, the
   administrator key and the SDK fields. All of these are testable without the policy arm.
2. After T622: Phase 2's mode value, US1's policy and markers, then US2's allowance arm.
3. Polish: docs, full checks, the security review and the native gates.

### What "done" means

Every task checked, the full checks green, the security review's findings closed, and the native
gates recorded. Nothing is claimed done while a gate is open.
