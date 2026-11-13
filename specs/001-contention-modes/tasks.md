---
description: "Task list for contention modes with graduated yielding"
---

# Tasks: Contention Modes with Graduated Yielding

**Input**: Design documents from `specs/001-contention-modes/`.

**Prerequisites**: [plan.md](plan.md), [spec.md](spec.md), [research.md](research.md),
[data-model.md](data-model.md), [contracts/](contracts/), [quickstart.md](quickstart.md).

**Tests**: Required. Constitution VI says tests come first for policy, telemetry, supervision
and protocol changes, and every fixed defect gets a regression test. Write each test task
before its implementation tasks and confirm it fails.

**Organization**: Grouped by user story (US1–US8 from spec.md), after Setup and Foundational
phases.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: the user story (US1–US8)

Paths are relative to the repository root: the single crate in `src/` and `tests/`, plus the
SDKs in `python/` and `npm/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: new modules, the one justified dependency, test scaffolding and fixtures.

- [X] T001 Create `src/contention.rs`, `src/actuation.rs` and `src/setup.rs`, each with a
  module doc line. Register them as top-level `pub mod` in `src/lib.rs`, and add the three
  paths to `ALLOWLIST` in `tools/pack_source_review.py`.
- [X] T002 Add `toml_edit = "0.22"` as a direct dependency in `Cargo.toml`, with a one-line
  justification comment: "already in Cargo.lock via toml 0.8; comment-preserving setup writes".
  Confirm that `cargo tree --locked` shows no new crate.
- [X] T003 [P] Create `tests/contention.rs` with a proptest harness and shared builders for
  snapshot, allocation and config, modeled on the helpers in `tests/policy.rs`.
- [X] T004 [P] Record 0.2 protocol fixtures as JSON in `tests/fixtures/0.2/`: a 0.2
  `NodeReport` heartbeat request, and a `HeartbeatReply` carrying an `Assignment`. Add them to
  `ALLOWLIST` in `tools/pack_source_review.py` and to `include` in `Cargo.toml`.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: shared types, configuration, schema and negotiation that every story uses.

**⚠️ CRITICAL**: No user story work begins until this phase is complete.

### Tests first

- [X] T005 [P] Write failing tests in `tests/policy.rs` for snapshot schema 3:
  - v1 and v2 recordings replay with unchanged decisions;
  - the schema 3 fields round-trip: `reserved_memory_mib`, `free_memory_mib`,
    `unattributed_memory_mib`, `ComputeActivity::Quiet`, and the CPU/RAM usage bounds.
- [X] T006 [P] Write failing tests in `tests/configuration.rs` for `[contention]`:
  - the defaults `default_mode = "neutral"`, `max_mode = "performance"`, `floor_policy = "a"`
    and `neutral_memory_policy = "a"` (*`max_mode` is unset by default since T131: the most
    aggressive node mode*);
  - the data-model.md rules: "`default_mode ≤ max_mode`", "`yield_first_margin_percent`
    1–90; > `performance_margin_percent`", "`neutral_tolerance_points` 1–50",
    "`hysteresis_percent` 1–99", "`quiet_window_intervals` ≥ 1";
  - `cpu.nice < 0` warns and is treated as 0;
  - rejection of unknown keys.
- [X] T007 [P] Write failing tests in `tests/coordinator.rs` for feature negotiation:
  - the reply carries `features = ["api/1", "contention/1"]`;
  - a `NodeReport` without new fields is accepted;
  - the fixtures in `tests/fixtures/0.2/` parse.
- [X] T008 [P] Write failing tests in `tests/upgrade.rs` and `tests/backup.rs` for coordinator
  distributed schema 4:
  - migrating from schema 3 adds `drain_requests.cause TEXT NOT NULL DEFAULT
    'coordinator_drain'`, `reservations.yield_cause TEXT` and a CHECK on `reservations.phase`,
    and recreates the triggers;
  - backup and restore round-trip schema 4.

### Implementation

- [X] T009 Move the duplicated resource arithmetic into `impl Resources` in `src/model.rs`:
  charge = max(request, observed), add, subtract and fits. Replace the copies in
  `src/policy.rs`, `src/agent.rs`, `src/coordinator.rs` and `src/execution_state.rs`. Behavior
  must not change; the existing suites must still pass.
- [X] T010 Split `agent::run()` in `src/agent.rs` into functions for slot lifecycle, outbox,
  heartbeat/admission and the actuation stage, with no behavior change. `tests/agent.rs` and
  `tests/distributed_service.rs` must pass serially.
- [X] T011 Add snapshot schema 3 in `src/model.rs`:
  - `SCHEMA_VERSION = 3`, and `supported_schema` accepts 1–3;
  - optional `GpuSnapshot.reserved_memory_mib`, `free_memory_mib` and
    `unattributed_memory_mib`;
  - `ComputeActivity::Quiet`;
  - optional CPU/RAM lower and upper usage bounds, with serde defaults.
- [X] T012 Add the Decision output types in `src/model.rs`: `ReasonCode`, `YieldAction`,
  `Decision.resources`, `Decision.actions` and `Decision.exceptions`, all additive.
  `would_drain` is derived from actions with step ≥ drain (see
  `contracts/decision-output.md`).
- [X] T013 Define the pure core types in `src/contention.rs`:
  - `ResourceKey`, with the stable order cpu, ram, then GPU keys by UUID with compute before
    memory;
  - `ContentionMode`, with the order yield_first < neutral < performance;
  - `YieldStep` (none = 0 … terminate = 6);
  - `YieldCause`, `Principal`, `ResourceView` and `ResourceThresholds`.
- [X] T014 Implement `[contention]` in `src/config.rs` per `contracts/config-contention.md`,
  with the data-model.md validation rules. At load time:
  - `gpu.execution_mode` and `gpu.best_effort_external_processes` are accepted, have no
    effect, and produce one warning naming `contention.gpu_compute.mode` and
    `contention.gpu_memory.mode`;
  - `cpu.nice < 0` warns and is treated as 0.

  Batch 1 note: the legacy GPU key warning moved to T085. Until US5 lands those keys still
  steer GPU admission, so a "no effect" warning would be false. *Superseded by T136: the keys
  are interpreted with a warning (FR-031).*
- [X] T015 Implement coordinator distributed schema 4 as one versioned migration in
  `src/coordinator.rs` and `src/state.rs`, dropping and recreating the triggers. Teach
  `src/backup.rs` and `src/upgrade.rs` the new version.
- [X] T016 Implement feature negotiation in `src/protocol.rs`, `src/coordinator.rs` and
  `src/agent.rs`:
  - `HeartbeatReply.features`;
  - optional `NodeReport.features`, `NodeReport.contention` and
    `AllocationReport.yield_cause`, all with serde defaults and skipped when empty;
  - the agent sends the new fields only after it sees `contention/1`, and re-learns the
    features from every reply (`contracts/wire-negotiation.md`).

**Checkpoint**: shared types, config, schema 4 and negotiation are in place. The suites pass
except the deliberately failing tests above.

---

## Phase 3: User Story 1 - Own work no longer yields to itself (Priority: P1) 🎯 MVP

**Goal**: fix D1–D6, keep one policy authority per node, and stop SIGKILL on observation and
control errors.

**Independent Test**: the six inverted D1–D6 tests and the supervision error tests pass. A GPU
job launched with unattributed device memory is not drained.

### Tests for User Story 1 ⚠️ (write first, must fail)

- [X] T017 [US1] Invert the six `known_defect_*` tests in `tests/policy.rs` and rename them
  `d1_…` to `d6_…`. They assert:
  - D1: no drain on the first observation;
  - D2: no event below 256 MiB;
  - D3: a quiet external context admits GPU work;
  - D4: CPU excess yields by `cap`, not drain;
  - D5: skew within tolerance keeps the budget;
  - D6: one unknown allocation does not drain others.

  They must fail before T025–T028.
- [X] T018 [P] [US1] Add tests in the `src/telemetry.rs` tests module:
  - unavailable memory = total − free;
  - a double-read disagreement beyond tolerance makes attribution unknown;
  - `Quiet` appears only after `quiet_window_intervals` of healthy NOT_FOUND samples with a
    complete inventory;
  - an idle device's health never becomes InsufficientObservability.
- [X] T019 [P] [US1] Add tests in `tests/kernel.rs` for CPU busy bounds: USER_HZ rounding and
  iowait count only in the upper bound.
- [X] T020 [P] [US1] Add tests in `tests/supervision.rs`:
  - an injected control-channel error holds the step;
  - a rejected journal transition is retried and never reaches `kill_and_confirm` for
    Guaranteed work;
  - opportunistic work goes through drain → TERM → KILL with its deadlines;
  - a minimum TERM grace applies after the first SIGTERM actually sent.
- [X] T021 [P] [US1] Add a test in `tests/agent.rs` proving that supervisors construct no
  collector or `PolicyEngine` and never emit `local_policy_drain`.

### Implementation for User Story 1

- [X] T022 [US1] Read NVML `reserved` and `free` in `src/telemetry.rs` (from
  `memory_info()`). Compute unavailable = total − free, read device memory before and after
  process enumeration, and report unattributed memory.
- [X] T023 [US1] Report bounds:
  - managed CPU and RAM lower and upper bounds in `src/telemetry.rs` (per-allocation
    quantization; anonymous PSS vs RSS);
  - host busy bounds in `src/kernel.rs`.
- [X] T024 [US1] Implement `Quiet` activity, and split observation health from fresh activity
  evidence, in `classify_activity` and `describe_gpu_observation` in `src/telemetry.rs`
  (research R8).
- [X] T025 [US1] Fix D1 and D2 in `src/policy.rs`:
  - no growth or new-PID event without a prior observation by the same authority;
  - growth must exceed `noise_tolerance_mib` beyond the unattributed band;
  - remove `protection_cap` and the shrink-percent handling.
- [X] T026 [US1] Fix D5 in `src/policy.rs`:
  - own/host inconsistency within the bounds counts as consistent: clamp external to ≥ 0
    with reason code `inconsistent_within_tolerance`;
  - beyond the bounds, mark the resource unknown and hold.
- [X] T027 [US1] Fix D6 in `src/policy.rs`: charge an allocation whose observation is missing
  at max(request, last known), mark it unknown (`allocation_unknown_hold`), hold the resource,
  and never zero the budget.
- [X] T028 [US1] Fix the D3 and D4 decision paths in `src/policy.rs`:
  - `validate_gpu_launch_contract` follows the effective mode, accepting Quiet or Active per
    mode instead of requiring `Idle`;
  - CPU-only excess produces `cap` actions instead of drain. Actuation follows in US2.
- [X] T029 [US1] Remove the collector and `PolicyEngine` from `PipeControl` in `src/agent.rs`.
  Supervisors keep the lease and drain deadlines only; envelope checks stay in the agent.
- [X] T030 [US1] Classify post-EXEC errors in `src/supervision.rs` per research R3, and remove
  the direct `kill_and_confirm` from the running-loop error path:
  - control → hold;
  - actuation → mark the step unavailable;
  - drain-file or target-file write → retry, then escalate;
  - journal → bounded retry, then the drain chain for opportunistic work, or hold and report
    for Guaranteed work.
- [X] T031 [US1] Enforce a minimum TERM grace after the first SIGTERM actually sent, and record
  both the deadline anchor and the actual send time, in `src/supervision.rs` (audit M6).
- [X] T032 [US1] Emit the reason codes `first_observation_baseline`,
  `vram_noise_below_tolerance`, `inconsistent_within_tolerance` and `allocation_unknown_hold`
  from `src/policy.rs`. Switch the existing prose assertions in `tests/policy.rs` to these
  codes.

**Checkpoint**: US1 delivers correct yielding inputs and one authority. This is the MVP.

---

## Phase 4: User Story 2 - Graduated, reversible yielding (Priority: P1)

**Goal**: a ladder from lower share through cap, pause, hint and drain to terminate, applied by
supervisors to owned processes, reachable after agent restart, with least-cost victims.

**Independent Test**: in a CPU-only contention run, the job moves through lower share, cap and
pause and then reverses, with no drain (quickstart §3, SC-007).

### Tests for User Story 2 ⚠️

- [X] T033 [P] [US2] Add property tests in `tests/contention.rs` for the ladder:
  - steps escalate in order, and memory resources skip steps 1–3 (*since T138, RAM takes a cap
    where the facts report `ram_cap`; `steps_escalate_in_order_and_memory_starts_at_a_ram_cap_or_the_hint`*);
  - `min_step_dwell_intervals` is respected;
  - steps reverse one level at a time below `hysteresis_percent`;
  - drain and terminate never reverse;
  - an unknown measurement holds;
  - a pause escalates after `pause_bound_s`;
  - under a load that switches on and off every 4 monitor intervals, own work changes step at
    most once per switch (SC-009).
- [X] T034 [US2] Add victim-cost tests in `tests/contention.rs`, covering each rule in
  order:
  1. uncontrollable allocations are excluded;
  2. the victim must free the contended resource;
  3. pending before running;
  4. opportunistic before Guaranteed (FR-016 orders pending first);
  5. least progress lost;
  6. best fit;
  7. assignment ID as the tie-break.
- [X] T035 [P] [US2] Add supervisor control tests in `tests/supervision.rs`:
  - `adjust` applies only to owned pidfds;
  - an unknown `kind` is ignored and does not count as AgentLost;
  - `pause` is refused for Guaranteed work;
  - `adjusted` reports unavailable steps.
- [X] T036 [P] [US2] Add actuation audit tests in `tests/managed_supervision.rs`:
  - every signal and control write targets a verified identity;
  - a reused PID and a foreign PID in a cgroup leaf are refused;
  - both checks still hold after an agent restart reconnects to the supervisor (FR-002).
  The restart cases use the supervisor harness in `tests/agent.rs`, and the leaf cases the
  fixture tree in `tests/cgroup.rs`.
- [X] T037 [P] [US2] Add an agent restart test in `tests/agent.rs`: the restarted agent
  reconnects through `control.sock`, reads the applied steps, and never marks the live
  supervisor NeedsReconciliation.
- [X] T038 [US2] Add control-socket tests in `tests/supervision.rs`:
  - a wrong token and an unauthenticated second connection are rejected;
  - the correct token is accepted and replaces the previous controller.
- [X] T039 [US2] Add tests in `tests/agent.rs`:
  - drain back-off counts every sent drain, local and coordinator-issued (audit M8);
  - dropping a `ChildSlot` closes the pipe without sending Drain (audit M5).
- [X] T040 [P] [US2] Add tests in `tests/coordinator.rs`: failures are classified by the
  recorded yield cause regardless of detail text. (The cancelled-task test moved to T052a.)
- [X] T041 [US2] Add a test in `tests/agent.rs`: advertised controls include cgroup controls only
  after successful backend preparation with authorized settings, and never PSI observations.
- [X] T042 [P] [US2] Add non-native tests in `tests/cgroup.rs`, using a temporary fixture tree:
  - a runtime write is refused when leaf membership differs from the verified PIDs or the leaf
    identity changed;
  - `Limit::Max` round-trips.

### Implementation for User Story 2

- [X] T043 [US2] Implement the pure ladder state machine `contention::plan_steps` in
  `src/contention.rs` (data-model.md YieldStep transitions).
- [X] T044 [US2] Implement victim cost ordering (research R11) in `src/contention.rs`, fed with
  checkpoint times from the agent's publication records in `src/agent.rs`.
- [X] T045 [US2] Add an `Actuator` trait to `LaunchBackend` in `src/execution_model.rs`: apply
  a step and target, read it back, and report unavailable steps.
- [X] T046 [US2] Implement the rootless actuator in `src/actuation.rs`:
  - SIGSTOP/SIGCONT duty-cycle cap and bounded pause through owned pidfds (`OwnedChild` in
    `src/supervision.rs`);
  - lower share reported unavailable at runtime.
- [X] T047 [US2] Implement the cgroup actuator in `src/cgroup.rs`:
  - a `Limit::{Max, Value}` type;
  - runtime writes of `cpu.weight`, `cpu.max` and `cgroup.freeze`, with `cgroup.events`
    readback. No runtime `memory.high`: FR-011 starts memory at the hint, so a RAM cap is never
    requested (*superseded by T138: RAM takes a `memory.high` cap where a delegated leaf
    allows it*);
  - a refused write falls back to the rootless mechanism for cap and pause;
  - every write first re-verifies the leaf identity and that membership equals the verified
    PIDs;
  - `assert_empty` replaced for adjustable controls;
  - targets journaled.
- [X] T048 [US2] Add `Control::Adjust`, `Control::Query`, `Event::Adjusted` and `Event::State`:
  - message types in `src/agent.rs`;
  - handling in `src/supervision.rs` and `src/child_supervision.rs`;
  - applied steps persisted as `AllocationContention` in `src/execution_state.rs`.
- [X] T049 [US2] Add the supervisor `control.sock` listener in `src/supervision.rs`:
  - socket mode 0600, inside the 0700 output directory;
  - a `hello` carrying the 32-byte token from the spec;
  - a peer UID check;
  - one active controller at a time.
- [X] T050 [US2] In `src/agent.rs` reconciliation, reconnect to live supervisors through
  `supervisor-identity.json` and `control.sock`, and stop rewriting live supervisors' journals
  (audit H5).
- [X] T051 [US2] Add the actuation stage in `src/agent.rs`:
  - send `adjust` per allocation from `Decision.actions`;
  - track sent drains with timestamps for back-off (audit M8);
  - make `ChildSlot::drop` close the pipe without sending Drain (audit M5).
- [X] T052 [US2] Record the yield cause across `src/agent.rs` and `src/coordinator.rs`:
  - the agent sends `AllocationReport.yield_cause` once negotiated;
  - the coordinator persists `drain_requests.cause` when it issues a drain;
  - failures are classified by recorded cause, removing `detail.contains("revoked")`.
- [X] T052a [US2] Cancelled tasks become terminal instead of being re-queued (audit A18):
  - state schema 6 adds `cancelled` to `tasks.status`; schema 5 migrates in place on open, the
    offline upgrade runs the same step, and backup and restore accept both (`src/state.rs`,
    `src/upgrade.rs`, `src/backup.rs`);
  - cancel, failure, replay-session and retry paths in `src/coordinator.rs`;
  - task-status constants in both SDKs, and the tools that wait on task status;
  - tests in `tests/coordinator.rs`, `tests/agent.rs` (across an agent restart),
    `tests/upgrade.rs`, `tests/backup.rs` and `tests/state.rs`.
- [X] T053 [US2] In `src/agent.rs`, advertise only the controls the backend can apply (a
  successful `CgroupBackend::new` plus authorized settings), and exclude PSI observations
  (audit H8).
- [X] T054 [US2] Native-evidence gate, run only when explicitly authorized. *Deferred: needs an
  authorized Linux host; commands in [native-gates.md](native-gates.md).*
  - add `runtime_adjust_requires_verified_membership` to `tests/cgroup.rs`;
  - run quickstart §4 with `CEDEGRID_TEST_CGROUP_ROOT` on Linux;
  - record the evidence in `docs/validation.md`.

  *Passed on the test host on 2026-10-26 (`docs/validation.md`, "Native gate results").*
- [X] T055 [US2] Add a rootless CPU contention scenario in `tests/agent.rs`: a test-owned
  external CPU load leads to `cap` within the Yield First response time, and the step reverses
  after the load stops (quickstart §3). *SC-002 now requires 2 intervals; T125 covers it.*

**Checkpoint**: graduated yielding works on owned processes, including after an agent restart.

---

## Phase 5: User Story 3 - Choose a contention mode per resource (Priority: P1)

**Goal**: Yield First, Neutral and Performance with the same semantics on every resource, plus
floors, policies A/B/C (*B and C removed by T131*), the envelope and spool rules, and the
Guaranteed rules.

**Independent Test**: scripted external loads per mode match the mode definitions in
`tests/contention.rs`.

### Tests for User Story 3 ⚠️

- [X] T056 [P] [US3] (*margin 15% since T146*) Add tests in `tests/contention.rs` for Yield First:
  a 25% margin, a 3-interval response, unknown activity counted as demand, and slow scale-up.
- [X] T057 [US3] Add tests in `tests/contention.rs` for Neutral:
  - max-min fair share with 1–3 principals plus an unknown-owner principal;
  - own work converges and does not go below its share, except for floors under Neutral
    memory policy A;
  - policy B holds the share. *Superseded by T122/T131: Neutral memory policy B is removed.*
- [X] T058 [US3] Add tests in `tests/contention.rs` for Performance and Guaranteed work:
  - a 10% margin (*2% since T146*), floors and fast scale-up;
  - floor policies A, B and C for Guaranteed work (*B and C superseded by T122/T131*);
  - Guaranteed work is never paused.
- [X] T059 [P] [US3] Add tests in `tests/agent.rs` for operator limits:
  - an envelope breach is restored per resource, opportunistic work first, with no all-stop
    (FR-057);
  - spool exhaustion blocks admission and drains nothing (FR-058);
  - explicit operator drain still drains every allocation.
- [X] T060 [P] [US3] Add tests in the `src/telemetry.rs` tests module: external processes
  aggregate into principals by owner UID, and processes with unreadable owners form one
  principal.
- [X] T061 [P] [US3] Add tests in `tests/supervision.rs`:
  - the worker gate sets `oom_score_adj = 1000` before EXEC;
  - it skips this for Guaranteed work under floor policy C (*superseded by T123: every own
    process gets 1000*);
  - managed children inherit the value.
- [X] T062 [P] [US3] Add tests in `tests/policy.rs`:
  - CPU/RAM expansion follows `cpu_ram_scale_up_cooldown_ms`, independently of
    `gpu.scale_up_cooldown_ms` (*split per resource as `scale_up_cooldown_ms` by T128/T135*);
  - the effective mode is min(requested, `max_mode`) and is reported per allocation.

### Implementation for User Story 3

- [X] T063 [US3] Aggregate principals in `src/telemetry.rs`:
  - the owner UID per external process, from sysinfo or `/proc/<pid>/status`, mapping NVML
    PIDs to UIDs;
  - processes whose owner is unreadable count as one principal.
- [X] T064 [US3] Implement max-min fair share in `src/contention.rs` (research R12).
- [X] T065 [US3] Implement the Yield First, Neutral and Performance target computations per
  resource in `src/contention.rs`, from `ResourceThresholds`.
- [X] T066 [US3] Implement floors and policies in `src/contention.rs`:
  - RAM and VRAM floors from `ram.reserve_mib`/`ram.reserve_percent` and
    `gpu.reserve_vram_mib`;
  - floor policies A, B and C, and Neutral memory policies A and B (*B and C removed by T131*);
  - the Guaranteed rules: no pause, and drain only for a floor breach under A.
- [X] T067 [US3] Set `oom_score_adj = 1000` in the worker gate before EXEC in
  `src/supervision.rs`, skipping it for Guaranteed work under floor policy C, and verify that
  managed children inherit it. *The policy C exemption is removed by T131.*
- [X] T068 [US3] Apply the envelope and spool rules in `src/agent.rs`: replace the
  `slots.keys()` all-drain with per-resource envelope restoration, and make spool exhaustion
  block admission only.
- [X] T069 [US3] (*the key is `scale_up_cooldown_ms` per resource since T135*) Decouple CPU
  and RAM scale-up from `gpu.scale_up_cooldown_ms`, using `cpu_ram_scale_up_cooldown_ms`, in
  `src/policy.rs` and in the launch-slot ramp in `src/agent.rs` (audit M1).
- [X] T070 [US3] Resolve the effective mode per allocation and resource as min(requested,
  `max_mode`), in `src/contention.rs` and `src/agent.rs`.
- [X] T071 [US3] (*SC-002 and SC-004 revised; T125 adds the new bounds*) Add time-stepped
  scenario tests in `tests/contention.rs` for SC-002, SC-003 and SC-004 with the provisional
  defaults.

**Checkpoint**: the three modes behave as specified on CPU, RAM, GPU compute and GPU memory.

---

## Phase 6: User Story 4 - Cooperative workers receive "use less" targets (Priority: P2)

**Goal**: a resource-target file with sequence, deadline and ack, delivered to leaders and
managed children through both SDKs.

**Independent Test**: a compliant SDK worker is not drained, and a non-compliant one is drained
after the hint deadline (quickstart §6).

### Tests for User Story 4 ⚠️

- [X] T072 [P] [US4] Add TypeScript drain, hook and managed-child tests mirroring the Python
  ones, in `npm/tests/worker-drain.test.ts` (audit M22).
- [X] T073 [P] [US4] Add Python tests in `python/tests/test_worker.py`:
  - `resource_targets()`;
  - `on_resource_target` fires once per sequence;
  - hook errors are isolated;
  - `ResourceSignals.from_env()` works;
  - a file with `version > 1` is ignored;
  - the ack is written.
- [X] T074 [P] [US4] Add TypeScript tests mirroring T073, in
  `npm/tests/worker-targets.test.ts`.
- [X] T075 [P] [US4] Add supervisor tests in `tests/managed_supervision.rs`:
  - the target file is written atomically with an increasing sequence;
  - managed children receive `CEDEGRID_RESOURCE_TARGET_FILE`;
  - compliance is judged by observed usage;
  - without an ack, the hint step is unavailable;
  - non-compliance leads to a drain after the deadline.

### Implementation for User Story 4

- [X] T076 [US4] Implement the target-file writer and the ack reader in `src/actuation.rs`, per
  `contracts/resource-target-file.md`.
- [X] T077 [US4] Pass the target-file path through `GateRequest` and the environment in
  `src/supervision.rs`, and to managed children in `src/child_supervision.rs`.
- [X] T078 [US4] Handle the hint step in the agent actuation stage in `src/agent.rs`: the hint
  deadline, the compliance check and escalation.
- [X] T079 [US4] Add the Python API in `python/cedegrid/worker.py`: `ResourceTargets`,
  `resource_targets()`, `on_resource_target()`, `ResourceSignals.from_env()` and ack writing.
- [X] T080 [US4] Add the TypeScript API in `npm/src/worker.ts` (`resourceTargets()`,
  `onResourceTarget()`, `ResourceSignals.fromEnv()`), and export it from `npm/src/index.ts`.
- [X] T081 [US4] Add the optional v2 context keys `resource_target_file` and `capabilities` in
  the native context builder in `src/agent.rs` and in the `supervise` path. The SDKs must keep
  accepting `schema_version == 2`.
- [X] T082 [US4] Document in `docs/worker-protocol.md` the target channel, the ack, deadlines,
  and drain exit semantics.

**Checkpoint**: memory contention can be resolved in place by cooperative workers.

---

## Phase 6a: Revisions before US5 (maintainer decisions B1–B14, 2026-10-25)

**Goal**: apply constitution 2.0.0 and the revised spec before US5: one floor policy, ordered
mode targets, readiness-based hints, principals by `uid_min`, peak-based memory headroom,
reclaim bounds, the legacy GPU interpretation, Guaranteed semantics and immediate targets.

**Independent Test**: the tests below fail on the pre-revision code and pass after it; the
review answers are in [review-pre-us5.md](review-pre-us5.md).

### Tests for the revisions ⚠️ (write first, must fail)

- [X] T122 [P] Add tests in `tests/configuration.rs` (B1, B2):
  - `floor_policy = "b"` and `"c"` and `neutral_memory_policy = "b"` fail to load, and each
    message names its replacement;
  - Performance on any resource without `acknowledge_caution = ["performance_mode"]` fails with
    `ERR_CEDEGRID_CONFIG` carrying `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED`, and loads with it;
  - a floor below its minimum is raised to it with a warning; a floor near its minimum without
    `small_floor` warns and still loads;
  - `guaranteed_hint_deadline_s` (1–30) and `ram.guaranteed_breach_window_s` (0–60, RAM only)
    are validated.
- [X] T123 [P] Add a test in `tests/supervision.rs` (B2): the gate sets `oom_score_adj = 1000`
  for Guaranteed work too, with no option to skip it.
- [X] T124 [P] Add tests in `tests/contention.rs` (B4, B10):
  - a proptest over generated `ModeInput`s on every resource: target(YF) ≤ target(N) ≤
    target(P) (SC-013);
  - the contended targets from data-model.md, including unknown activity;
  - a Guaranteed CPU cap never goes below the request.
- [X] T125 [P] Add scenario tests in `tests/contention.rs` (B8, SC-002, SC-012):
  - CPU is returned within 2 monitor intervals in Yield First, Neutral and Performance,
    including own work with many threads saturating the host;
  - after the external load stops or shrinks, a held cap rises again at the mode's rate;
  - RAM reclaim for a hint-ready, a non-ready and a Guaranteed allocation within the SC-012
    bounds;
  - the critical floor path within its shorter bounds.
- [X] T126 [P] Add readiness tests (B5):
  - in `tests/managed_supervision.rs`: `adjusted`, `state` and `readiness` carry `hint_ready`
    and `observes_drain`; a worker that acks late becomes hint-ready by the next interval; a
    plain command is neither;
  - in `tests/contention.rs`: a non-acking command escalates to drain without a refused-hint
    round trip; on the critical path a hint-ready allocation is drained after
    `critical_hint_deadline_s`, a non-ready one at once, and one that does not observe the drain
    file gets `terminate`;
  - in `src/agent.rs` tests: readiness reaches `AllocationFacts` and the negotiated
    `AllocationReport`.
- [X] T127 [P] Add principal tests in `tests/contention.rs` and `tests/policy.rs` (B6):
  - a user ID below `uid_min` is background and never a principal;
  - a new user joins only after `principal_window_intervals`, and counts toward the unattributed
    principal meanwhile;
  - a principal leaves only after `principal_leave_s` below the threshold (*revised from half
    the threshold for the window; see T142*);
  - the owner stays a principal below `uid_min`; unreadable owners stay one principal.
- [X] T128 [P] Add memory tests in `tests/policy.rs` (B7):
  - RAM and GPU memory headroom use the external peak over `memory_peak_window_s`, including a
    quiet GPU context;
  - CPU expansion waits only for `contention.cpu.scale_up_cooldown_ms` and RAM only for its own;
  - repeated reclaim episodes double the memory cooldown from 1 s to at most 30 s, and a quiet
    window resets it.
- [X] T129 [P] Add tests in `tests/configuration.rs` and `tests/cli.rs` (B9): each
  `gpu.execution_mode` value maps per FR-031 with its warning, and `doctor` prints the
  interpretation.
- [X] T130 [P] Add SDK tests in `python/tests/test_worker.py`, `npm/tests/worker-targets.test.ts`
  and `tests/managed_supervision.rs` (B13): a handler subscribed after a target exists receives
  it at once, exactly once; a managed child started after a hint finds the current target.

### Implementation for the revisions

- [X] T131 Remove floor policies B and C, Neutral memory policy B and the OOM exemption (B1, B2):
  validation messages, `acknowledge_caution` entries, floor minimums, the new Guaranteed keys and
  the implicit node cap in `src/config.rs`; `guaranteed_oom_victim` in
  `src/supervision.rs` and its callers; exceptions and markers in `src/model.rs`;
  `NodeContentionReport` in `src/protocol.rs`; `docs/guarantees.md`.
- [X] T132 Implement ordered targets, the contended state, cap relaxation and the Guaranteed CPU
  rule in `src/contention.rs` and `src/policy.rs` (B4, B10).
- [X] T133 Implement readiness (B5): tracking and events in `src/supervision.rs`, facts and
  report fields in `src/agent.rs` and `src/protocol.rs`, the hint rule, the critical path,
  `terminate` and the Guaranteed wait by readiness (hint deadline, RAM window, none on GPU
  memory, critical deadline on the critical path) in `src/policy.rs` and `src/contention.rs`;
  remove the refused-hint retry.
- [X] T134 Implement principal classification in `src/policy.rs`, `src/contention.rs` and
  `src/config.rs` (B6), and document the `hidepid=2` fallback in `docs/guarantees.md`.
- [X] T135 Implement the memory peak window, the per-resource cooldowns and the reclaim backoff
  in `src/policy.rs`, `src/config.rs` and `src/agent.rs` (B7), and rename the tools helper in
  `tools/runtime_config.py`.
- [X] T136 Implement the legacy GPU interpretation and warnings in `src/config.rs`, and print it
  in `doctor` in `src/main.rs` (B9 parsing; the admission effect is T085).
- [X] T137 Deliver the current target at once (B13): `on_resource_target` in
  `python/cedegrid/worker.py` and `onResourceTarget` in `npm/src/worker.ts`; the child's target
  file in `src/supervision.rs`; `docs/worker-protocol.md`.
- [X] T139 Resolve the code findings of the post-revision `/speckit-analyze` in `src/config.rs`,
  `src/policy.rs` and `tests/contracts/cedegrid-0.2.json`: an unconfirmed near-minimum floor
  applies as twice the minimum; `hint_deadline_s` ≤ 30 and `unknown_window_intervals` ≤ 20;
  `saturation_percent`; the removed-option and unacknowledged-Performance config cases.

- [X] T140 Resolve the second `/speckit-analyze` pass in `src/contention.rs`, `src/policy.rs`,
  `src/config.rs`, `src/actuation.rs` and `src/supervision.rs`, with tests in
  `tests/contention.rs`, `tests/policy.rs`, `tests/configuration.rs` and
  `tests/managed_supervision.rs`:
  - upper bounds for `min_step_dwell_intervals`, `yield_first_response_intervals`,
    `reclaim_s` (compute only) and `pause_bound_s`;
  - a pause ends at its bound even when nothing is contended, as a minimum-duty cap for a
    worker without a hint, which does not pause again;
  - a hint waits for its deadline only, and carries the deadline the policy enforces to the
    target file (`YieldAction.deadline_ms`, `StepRequest.deadline_ms`);
  - a Guaranteed hint waits the Guaranteed hint deadline from the hint; the RAM window counts
    from the breach;
  - uncontrollable opportunistic work does not block Guaranteed release;
  - `never_share_occupied_gpus` uses the existing non-sharing path until T085;
  - the supervisor accepts a Guaranteed envelope drain on memory only.

- [X] T142 Record the pre-US5 decisions, with tests:
  - a mode the node cap refuses is reported with `mode_capped` and the cap (FR-030;
    `tests/policy.rs`);
  - Neutral's own idle margin (20%, 5% since T146), validated YF ≥ N ≥ P (FR-021, FR-065;
    `tests/configuration.rs`, `tests/contention.rs`);
  - the 512 MiB GPU floor minimum and `gpu.reserve_vram_percent` (FR-023;
    `tests/configuration.rs`);
  - `principal_leave_s` (FR-059; `tests/policy.rs`);
  - external memory peaks per external process, dropped when it exits (FR-060;
    `tests/policy.rs`, `examples/gpu-pressure.json`);
  - `acknowledge_danger` renamed `acknowledge_caution`;
  - the actuation tests in `tests/supervision.rs` wait for observed conditions with a deadline.
- [X] T143 Resolve the third `/speckit-analyze` pass in `src/contention.rs`, `src/policy.rs`,
  `src/config.rs` and `src/agent.rs`, with tests in `tests/contention.rs` and
  `tests/configuration.rs`:
  - Yield First caps own work that saturated the resource itself to the margin, within the fair
    share, before it pauses (FR-020; `ModeInput.own`);
  - a margin below its default needs `reduced_margin`, and a `saturation_percent`,
    `principal_min_percent` or `noise_tolerance_mib` above its default needs
    `relaxed_detection` (FR-045, FR-054, constitution II);
  - the agent warns once when a percentage puts a floor near its minimum on the host (FR-023);
  - the removed Neutral memory policy names `gpu.reserve_vram_percent` (FR-042).

**Checkpoint**: US5 starts from ordered targets, one floor policy and readiness-based hints.

---

## Phase 6b: Utilization revisions before US5

**Goal**: work-conserving defaults and accounting (constitution 2.1.0): capacity no workload
wants does not sit idle, and no other workload is harmed.

- [X] T144 Classify every configuration key as safe, bounded or caution (`config::KEY_CLASSES`)
  with a test that enumerates every key and fails for an unclassified one; `doctor` lists each
  active caution option's keys and values; `uid_min`, a shorter memory peak window and a
  shorter GPU quiet window join `relaxed_detection`; `noise_tolerance_mib` ≤ 1024 MiB and, at
  the agent's first sample, ≤ half of each GPU floor; upper bounds for the monitor interval,
  kernel freshness, GPU sample age, drain timeout and termination grace; external usage beyond
  the 64 listed processes keeps its shared peak (`src/config.rs`, `src/kernel.rs`,
  `src/agent.rs`, `src/main.rs`, `src/telemetry.rs`, `tests/configuration.rs`,
  `tests/policy.rs`, `tests/contracts/cedegrid-0.2.json`; FR-068, FR-070).
- [X] T145 A memory floor breach needs observed free memory below the floor; running memory work
  is judged on usage (GPU: when hint-ready, else its charge); a charge-only shortfall only blocks
  admission (`src/policy.rs`, `tests/contention.rs`, `tests/policy.rs`; FR-066).
- [X] T146 The CPU idle reserve is max(reserve, margin) with `cpu.reserve_physical_cores`
  default 0; margins 15/5/2% on compute and Yield First 20% on memory; GPU floor default
  max(twice the minimum, 5%) (`src/config.rs`, `src/contention.rs`, `src/policy.rs`; FR-067).
- [X] T147 Contended Performance keeps max(fair share, capacity − external − margin); small
  external demand is probed and recorded as satisfied; an effective cap does not escalate while
  its target moves down (`src/contention.rs`, `src/policy.rs`, `tests/contention.rs`; FR-021,
  FR-022).
- [X] T148 CPU admission charges running opportunistic work at its p95 usage over
  `admission_usage_window_s`; reclaim episodes span real breaches and the backoff resets after a
  quiet window; a held hint rises while calm; a leaving principal's unused share is
  redistributed (`src/policy.rs`, `tests/policy.rs`, `tests/contention.rs`; FR-034, FR-061).
- [X] T149 `held_back` per resource in decisions and node reports (SC-016); scenario tests for
  SC-014 and SC-015; the four test binaries that left namespace control directories remove them;
  constitution 2.1.0 (`src/model.rs`, `src/policy.rs`, `src/protocol.rs`, `src/namespace.rs`;
  FR-069).
- [X] T151 Report each resource's admission (open, held or blocked, with its reason) so node
  contention reports carry every resource and its `held_back`: blocked while the node drains,
  the spool is full, the floor is breached or own work on the resource yields (FR-028); held
  while its measurement is unknown, a GPU device is guarded, or it cools down or backs off
  (`src/policy.rs`, `src/agent.rs`, `tests/policy.rs`; FR-038).

## Phase 7: User Story 5 - GPU work on shared, occupied devices (Priority: P2)

**Goal**: admission on occupied devices per mode and memory headroom, and coordinator placement
by per-resource admission.

**Independent Test**: a GPU job is admitted and completes on a device with an idle external
context, under Neutral and Performance (quickstart §5).

### Tests for User Story 5 ⚠️

- [X] T083 [P] [US5] Add launch-contract tests by mode in `tests/policy.rs` (including FR-064:
  `never_share_occupied_gpus` blocks an occupied device in every mode):
  - Neutral and Performance admit on quiet or active devices within the floors, and under
    activity of unknown amount, which counts as one principal (FR-021);
  - Yield First requires quiet;
  - unknown memory blocks;
  - VRAM headroom keeps each external process's peak, quiet contexts included.
- [X] T084 [P] [US5] Add placement tests in `tests/coordinator.rs`:
  - placement honors the per-resource admission from a negotiated `NodeReport.contention`;
  - legacy agents use the 0.2 fields;
  - admission for a resource is blocked while own work on it is being reclaimed (FR-028, as amended);
  - preemption keeps a pass-level victim set and runs only on real slot or capacity exhaustion.

### Implementation for User Story 5

- [X] T085 [US5] Implement per-device GPU admission in `src/contention.rs` and `src/policy.rs`
  (`validate_gpu_launch_contract` by mode). Replace the legacy `GpuExecutionMode` steering with
  the FR-031 interpretation from T136, and block devices with external contexts or unknown
  processes when `never_share_occupied_gpus` is set (FR-064); running own work on a device that
  becomes occupied is not drained for that alone.
- [X] T086 [US5] Place work by per-resource admission in `src/coordinator.rs`.
  `fits_requested`, `schedule` and `grant` must share one admission predicate (audit M10).
- [X] T086a [US5] Split the interim combined CPU/RAM admission per resource (plan B7): once
  T086 places by per-resource admission, report CPU and RAM admission separately in
  `NodeReport.contention` from their own cooldowns, and drop the both-cooled rule for
  `cpu_ram_expansion_allowed` in `src/policy.rs`. Test in `tests/policy.rs` and
  `tests/coordinator.rs`: a RAM reclaim backoff blocks only RAM-requesting placement.
  Every launch reserves CPU and RAM, so RAM backoff blocks all placement while CPU admission
  and headroom stay open. A hint that answered a breach reverses once calm, whatever the
  external level, or RAM admission would stay closed. The schedulable CPU budget adds what
  running opportunistic work is charged above its p95 usage, so placement and the launch
  check admit CPU as the policy does (FR-034).
- [X] T087a [US5] Neutral launch recheck admits active GPUs only when US5 floors protect the
  external usage. Batch 2 blocks Active in every mode at the launch recheck in
  `validate_gpu_launch_contract` (`src/policy.rs`). Once T085 admission by mode and the T135 peak
  headroom protect it, invert the
  regression test `launch_recheck_blocks_active_external_compute_in_every_mode_until_floors_exist`
  in `tests/policy.rs` for Neutral and Performance, keeping Yield First blocked. Do not open
  this before the floors exist.
- [X] T087b [US5] Make T087a's launch recheck call the same pure admission function as the policy,
  on a fresh observation, so the two cannot disagree (`src/policy.rs`, `src/agent.rs`). Test in
  `tests/policy.rs`.
- [X] T087c [US5] Regression check that pause is never used for GPU memory contention (memory skips
  steps 1–3 since T043): the ladder skips pause on GPU memory
  for every backend, with a test in `tests/contention.rs` (FR-011).
- [X] T087d [US5] Approximate the GPU compute fair share by pause duty-cycling of own GPU work in
  `src/actuation.rs` and `src/policy.rs`, and document the granularity (one monitor interval,
  5% minimum duty, plus the longest running kernel) in `docs/guarantees.md` (FR-021). A process
  with both a CPU cap and a GPU compute cap runs one duty cycle at the smaller duty; GPU compute
  steps are unavailable for Guaranteed work (FR-024). Tests first in `tests/contention.rs` and
  the actuation unit tests.
- [X] T087e [US5] Apply the utilization rules to GPU compute: targets through `own_allowance`
  with demand probing and Performance's contended target (FR-021, FR-022). GPU memory already
  yields on real breaches and judged usage (T145) and keeps per-process peaks (T128); add a
  regression check. Tests first in `tests/policy.rs`.
- [X] T087f [US5] Replace the legacy 30 s `gpu.scale_up_cooldown_ms` for GPU memory admission
  with the per-mode cooldown (`contention.gpu_memory.scale_up_cooldown_ms`, default the mode's
  scale-up interval) plus the reclaim backoff, in `src/config.rs` (allow the key on gpu_memory)
  and `src/policy.rs`. Precedence: `contention.gpu_memory.scale_up_cooldown_ms` when set, else a
  set `gpu.scale_up_cooldown_ms`, else the mode's interval. GPU admission keeps requiring that
  CPU and RAM are known and safe, but no longer waits for the node-wide cooldown. Amend FR-034,
  `data-model.md`, `contracts/config-contention.md` and `docs/guarantees.md` in the same change.
  Tests first in `tests/policy.rs`.
- [X] T087 [US5] Fix preemption in `src/coordinator.rs`: keep a pass-level projected charge and
  victim set, and preempt only on real exhaustion (audit M9).
  Real exhaustion: every requested resource is open, and the budget, the worker pool or the
  launch slots are used up; zero slots count only at the agent's reported `max_workers`.
- [X] T088 [US5] Native-evidence gate, run only when explicitly authorized. *Deferred: needs an
  authorized Linux host with an NVIDIA device; commands in [native-gates.md](native-gates.md).*
  - run quickstart §5 on Linux + NVIDIA with `CEDEGRID_TEST_GPU_UUID` and an idle external
    context, including 100 consecutive launches on the shared device;
  - record the evidence for SC-006 and the VRAM part of SC-001 in `docs/validation.md`;
  - measure the GPU memory a fresh external CUDA context takes on each target GPU, and raise
    `MIN_VRAM_FLOOR_MIB` in `src/config.rs` (512 MiB, provisional) to at least the largest value,
    updating every place that states the 512 MiB GPU minimum in the same change: FR-023 and its
    rationale in `spec.md`, `data-model.md`, `contracts/config-contention.md`,
    `docs/guarantees.md`, and the tests in `tests/configuration.rs` and `tests/contention.rs`,
    together with what follows from it: the default `gpu.reserve_vram_mib` (twice the minimum),
    the "1 GiB" default text, the critical level's lower bound and the noise-tolerance bound;
  - measure over a real session how often GPU activity is Unknown, and record it with the
    evidence;
  - record how closely summed per-process `sm_util` tracks device utilization under 1, 2 and 4
    concurrent processes on each target GPU, and set the default
    `contention.gpu_compute.attribution_tolerance_percent` (15, provisional) from it, and how
    often the consistency check fails over a real session.

  *Passed on the test host on 2026-10-26, one GPU (`docs/validation.md`, "Native gate
  results"). A fresh external context took 74 MiB, so the 512 MiB minimum stays. The largest
  attribution gap was 5 points, so the 15-point tolerance stays until a second device is
  measured (report 021, Decision 5).*
- [X] T152 [US5] Measure external GPU compute instead of treating all activity as an unknown
  amount (FR-021): each external PID's average fresh `sm_util`, capped at 100, summed in
  thousandths of the device, trusted when all processes' sum is within
  `attribution_tolerance_percent` of device utilization and no MPS daemon or server is present
  (`src/telemetry.rs`); a measured amount takes the CPU paths, and `held_back.compute_path`
  names each device's path (`src/policy.rs`, `src/model.rs`, `src/config.rs`). Tests in the
  telemetry unit tests, `tests/policy.rs` and `tests/configuration.rs`.

**Checkpoint**: GPU work no longer needs an empty device.

---

## Phase 8: User Story 6 - Configure modes safely across versions (Priority: P2)

**Goal**: 0.2 configs and peers keep working; per-job overrides with a node cap; deprecations.

**Independent Test**: all examples load unchanged, the mixed-version matrix passes, and 0.2
workers behave unchanged (quickstart §8).

### Tests for User Story 6 ⚠️

- [X] T089 [P] [US6] Add tests in `tests/configuration.rs`:
  - every `examples/*.toml` and 0.2 config loads unchanged;
  - Neutral is the default;
  - legacy keys are interpreted per FR-031 (covered by T129);
  - removed options fail to load (covered by T122);
  - a per-resource mode above `max_mode` fails.
- [X] T090 [P] [US6] Add the mixed-version matrix in `tests/coordinator.rs` (`mixed_version_*`)
  (SC-008):
  - the old-agent fixture with the new coordinator;
  - a new agent with a coordinator that advertises no features;
  - new with new.
- [X] T091 [P] [US6] Add SDK compatibility tests in `python/tests/test_worker.py` and
  `npm/tests/worker-drain.test.ts`: a drain-only 0.2-style worker behaves unchanged under the
  new supervisor.
- [X] T092 [P] [US6] Add client tests in `python/tests/test_transport.py` and
  `npm/tests/sdk.test.ts`: job specs carry `contention` only when it is set.

### Implementation for User Story 6

- [X] T093 [US6] Add the optional `LaunchRequest.contention` field (skipped when None) in
  `src/execution_model.rs`. In `src/coordinator.rs`, strip it for agents without
  `contention/1`, and place tasks with overrides only on capable agents.
- [X] T093a [US6] Replace the interim mixed-mode rule with the target model in plan.md (B11): the
  owner's aggregate share comes from the node mode and cap, and per-job modes set the yield
  order inside it (Yield First, then Neutral, then Performance), with `victim_order` within a
  tier, in `src/contention.rs` and `src/policy.rs`. Add a regression test in `tests/policy.rs`
  with two jobs in different modes on the same resource: the Yield First job yields first and is
  held to its own mode's target.
- [X] T094 [US6] Let the Python and TypeScript clients accept the optional `contention` field in
  job specs, and send it only when set (`python/cedegrid/client.py`, `npm/src/client.ts`).
- [X] T095 [US6] Make `observe` and `replay` in `src/main.rs` read snapshot schemas 1–3.
- [X] T096 [US6] Add `CHANGELOG.md` entries for:
  - the Neutral default;
  - the interpreted legacy GPU keys;
  - the removed floor and Neutral memory policies;
  - the envelope and spool behavior;
  - the `cpu.nice` clamp.
- [X] T097 [US6] Add the worker-context keys `resource_target_file` and `capabilities`, the
  target-file format, and the negotiated optional fields (including `AllocationReport.hint_ready`
  and `observes_drain` from T133, and `held_back` from T149; T139 and T144 already added the
  removed-option and bound config cases) to the shared contract fixture
  `tests/contracts/cedegrid-0.2.json` and to the cross-SDK interop checks
  (`python/tests/numeric_interop.py`, `npm/tests/numeric-interop.ts`).

**Checkpoint**: in-place adoption without breaking deployments.

---

## Phase 9: User Story 7 - Set up a node interactively or from the command line (Priority: P2)

**Goal**: a keyboard interface by default on a TTY, equivalent flags, caution markers and
confirmation, and atomic writes that preserve everything else.

**Independent Test**: flags and the interface produce byte-identical files. The non-TTY run
never blocks, and caution choices need confirmation (quickstart §7).

### Tests for User Story 7 ⚠️

- [X] T098 [P] [US7] Add unit tests in `src/setup.rs` for the caution mapping (B3): Yield First,
  Neutral, default floors and default margins have no marker; Performance on any resource, a
  floor below twice its minimum, a margin below its default and a detection setting beyond its
  default (FR-045) are `caution`.
- [X] T099 [P] [US7] Add tests in `tests/cli.rs` for `setup --non-interactive`:
  - exit codes 2, 3 and 4 per `contracts/setup-cli.md`;
  - `--margin` for `yield_first`, `neutral` and `performance`, validated as YF ≥ N ≥ P;
  - `--json` output;
  - no prompt or blocking on a non-TTY;
  - a missing config on a non-TTY never offers setup;
  - unmanaged settings and comments preserved byte for byte.
- [X] T100 [US7] Add Unix PTY tests in `tests/cli.rs`:
  - arrow, Space and Enter navigation;
  - the caution confirmation screen;
  - Esc, `q` and Ctrl-C write nothing and restore the terminal;
  - with `NO_COLOR=1`, no escape sequences, while `[caution]` remains;
  - output byte-identical to the flag path (SC-011);
  - a command that needs a missing config offers setup on a TTY.

### Implementation for User Story 7

- [X] T101 [US7] Implement setup choices (role, per-resource modes, node cap, floor sizes, margin
  sizes for every mode, Guaranteed hint deadline, tolerances through `--set`), defaults from the
  current config, validation and the caution mapping in `src/setup.rs` (B3). The summary lists
  each active caution option with the keys and values it covers (`Config::caution_details`).
- [X] T102 [US7] Implement the `toml_edit` writer in `src/setup.rs`: edit only managed tables,
  validate through `Config::load`, then rename atomically.
- [X] T103 [US7] Add the `setup` subcommand and flags in `src/main.rs` per
  `contracts/setup-cli.md`, with JSON output and stable error codes.
- [X] T104 [US7] Implement the terminal interface in `src/setup.rs`:
  - termios raw mode through `libc`, restored by a drop guard and on signals;
  - ANSI rendering;
  - keys, consequence lines, and the caution marker in SGR 33;
  - `NO_COLOR`, `TERM=dumb` and non-TTY detection;
  - the caution confirmation screen and the summary screen.
- [X] T105 [US7] Implement the line-based prompt fallback in `src/setup.rs`, for terminals
  without cursor control or that are too small.
- [X] T106 [US7] In `src/main.rs`, offer setup on a TTY when a command needs a missing config.
  Never do this on a non-TTY.
- [X] T107 [US7] On Windows, setup configures client settings only and refuses node choices
  with a clear error (`src/main.rs`, `src/setup.rs`).
- [X] T108 [US7] Add a short setup section to `README.md` that points to `docs/guarantees.md`
  for the caution marker.

**Checkpoint**: new nodes are configured safely through setup.

---

## Phase 10: User Story 8 - See what the node is doing (Priority: P3)

**Goal**: per-resource mode, usage, demand, target share, step and reason codes in `observe`,
`doctor`, `status` and node reports.

**Independent Test**: during a scripted contention run, the outputs match the expected values at
each step.

### Tests for User Story 8 ⚠️

- [X] T109 [P] [US8] Add tests in `tests/cli.rs`:
  - `observe --samples 2 --no-state` JSON includes `resources`, `actions` and `exceptions`;
  - `doctor` shows the modes, floors, caution options with markers and settings, each
    resource's `held_back`, available steps and the legacy GPU interpretation.
- [X] T110 [P] [US8] Add a test in `tests/coordinator.rs`: the operator `status` shows each
  node's negotiated contention summary, with each resource's admission and `held_back`.

### Implementation for User Story 8

- [X] T111 [US8] Render the decision's resources, actions and exceptions, including each
  resource's `held_back`, in `observe` and `doctor` in `src/main.rs`.
- [X] T112 [US8] Include node contention summaries, with each resource's `held_back`, in the
  coordinator status in `src/coordinator.rs`.
- [X] T113 [US8] Add a reason-code catalog in `src/contention.rs`: stable codes, each with one
  sentence of text.

**Checkpoint**: every step change is explainable within one report interval (SC-010).

---

## Phase 11: Polish & Cross-Cutting Concerns

- [X] T114 Update `docs/guarantees.md` as the single normative source (FR-056); Phase 6a already
  documents the items it implemented, and this task reviews the whole document:
  - floors and their minimums;
  - floor policy A, the removed options and the caution options;
  - the reason-code catalog, including `mode_capped` and `small_floor_raised`;
  - the Guaranteed deadline and RAM window, the critical floor path, principals by `uid_min`,
    peak headroom and reclaim backoff, `never_share_occupied_gpus`;
  - modes;
  - envelope and spool behavior;
  - the deprecation of `gpu.execution_mode`;
  - the reason-code catalog.
- [X] T115 [P] Update the layout map in `CLAUDE.md` with `contention.rs`, `actuation.rs` and
  `setup.rs`.
- [X] T116 [P] Remove code made obsolete by this feature in `src/policy.rs` and `src/agent.rs`:
  the protection-cap fields and the supervisor-side policy plumbing.
- [X] T116a Count external principals for Neutral's CPU fair share (done with T063 and T064:
  principals come from `src/telemetry.rs`, and unattributed external usage counts as one
  principal):
  - in `src/kernel.rs`, read the distinct UIDs other than the agent's own that have CPU usage
    above the noise floor, from `/proc` and read-only;
  - in `src/contention.rs`, divide `cpu_allowance`'s saturated Neutral share by the managed
    principal plus those external principals, falling back to one external principal when UIDs
    cannot be read or the platform has no `/proc`;
  - tests in `tests/contention.rs` and `tests/telemetry.rs`, then remove the known limitation
    from `spec.md`, `plan.md` and `docs/guarantees.md`.

  *T134 adds `uid_min`, the principal window and background demand (FR-059).*
- [ ] T117 Validate the provisional thresholds (SC-002–SC-004, the SC-012 reclaim bounds and the
  SC-014/SC-015 utilization targets) on Linux with test-owned loads.
  *Deferred: needs an authorized Linux host; see [native-gates.md](native-gates.md).*
  Record the evidence in `docs/validation.md`, and change defaults in `src/config.rs` only with
  that evidence.

  *Open after the test host's run on 2026-10-26 (`docs/validation.md`): CPU reclaim (SC-002,
  SC-012) passes; SC-004's contender bullet (T178), SC-014 and Neutral's matched GPU throughput
  miss; SC-003's convergence is not exercised by the runner. Report 021, Decisions 1–3.*
  *On 2026-10-27 SC-004's contender bullet passes under a second unprivileged account (T178);
  SC-014 (89.6% with 100 m tasks) and SC-015 in Yield First with the catch-up loop (84.0%,
  65 step changes) still miss.*
- [X] T118 Audit the pressure suite for SC-005, with defaults and again with Performance and a
  confirmed near-minimum floor:
  - run the `tools/pressure.py` scenario with the actuation audit hook;
  - assert zero actions on external processes;
  - assert zero external OOM kills and zero external GPU allocation failures.

  *Deferred: needs an authorized Linux host; see [native-gates.md](native-gates.md), which gives
  the audit over `cedegrid history` that stands in for the hook.*

  *Passed on the test host on 2026-10-26, CPU and GPU, four runs (`docs/validation.md`).*
- [X] T153 Prepare the T117/T118 gate runner in `tools/pressure_comparison.py`, tests first in
  `tests/test_pressure_comparison.py`:
  - `--contention-mode {yield_first,neutral,performance}` and `--ram-floor-mib`;
  - a step-timing readout over `cedegrid history`: per resource, time to the first step, to
    reclaim and to reversal, with a graduated step counted as a yield, not only a drain;
  - a read-only snapshot of each test-owned external process before and while it runs (nice
    value, cgroup path, that cgroup's `cpu.max`, `memory.high` and `memory.max`, start time),
    which must be unchanged beside the `cedegrid history` audit (SC-005);
  - final commands per mode and floor in [native-gates.md](native-gates.md).
- [X] T119 Check export parity: every new tracked file is in `ALLOWLIST` and the Cargo
  `include`, `tests/test_source_review.py` passes, and the in-memory
  `validate_files(public=True)` passes.
- [X] T120 Run the full verification:
  - `cargo fmt --check`;
  - `cargo clippy --all-targets -D warnings`;
  - the serial Rust suite;
  - `cargo +1.88.0 check --all-targets`;
  - both Python suites;
  - `npm test`;
  - `cargo check --target x86_64-pc-windows-msvc` for the client paths, where the target is
    installed.
- [X] T121 Run quickstart §1–3 and §6–8, and record the results.
- [X] T138 Add a runtime RAM cap step on delegated cgroups (B14): `memory.high` on the allocation's
  leaf before the hint, verified by readback, in `src/cgroup.rs` and `src/actuation.rs`, with a
  native gate in `tests/cgroup.rs` (deferred like T054). Rootless backends keep the cap step
  unavailable for RAM. Amend FR-011, plan H7 and research R4 in the same change, since memory
  then starts at a cap where a delegated cgroup allows it.
  *Done: the cap is max(target, unevictable + anon without swap + `noise_tolerance_mib`), never
  below a Guaranteed request or above a launch `memory.high`; an ineffective cap is raised to
  current usage; the native gate is deferred like T054 (see [native-gates.md](native-gates.md)).*
- [X] T150 After US5, implement the allowance signal (FR-071), tests first: the supervisor
  publishes an optional `allowance` per resource in the resource-target file whenever it changes,
  also without a target; Python `on_resource_allowance` and TypeScript `onResourceAllowance`
  deliver it like targets; add it to the contract fixture and the cross-SDK interop checks. Test in
  `tests/managed_supervision.rs`, `python/tests/test_worker.py` and
  `npm/tests/worker-targets.test.ts`.
- [X] T141 Add PSI `cpu.some` as a secondary saturation signal next to `saturation_percent`
  (follow-up, not blocking). *Spec amendment required first*: amend Saturated and Contended in
  the spec's Terms, `data-model.md` and `docs/guarantees.md`, and add the PSI native gate to
  [native-gates.md](native-gates.md). Then read it in `src/kernel.rs` (read-only), treat the CPU
  as saturated when either signal says so, keep unknown PSI as no extra evidence, and test it in
  `tests/contention.rs` and `tests/kernel.rs`.
  - [X] T141a Amend Saturated and Contended, add FR-072, `psi_some_percent` in
    `data-model.md` and `contracts/config-contention.md`, `docs/guarantees.md`, plan, research
    R16 and the native gate.
  - [X] T141b Read external and own CPU pressure read-only in `src/kernel.rs`
    (`KernelSnapshot.cpu_pressure`), tests first in `tests/kernel.rs` with fake `/proc` and
    cgroup trees: external siblings only, own excluded, system-only, unreadable and malformed.
  - [X] T141c Add `contention.cpu.psi_some_percent` (`KEY_CLASSES`, `relaxed_detection` above the
    default) and the saturation rule in `src/policy.rs`, tests first in `tests/contention.rs`:
    external PSI high below `saturation_percent` saturates and, with external demand,
    contends; own PSI high with external low does not; system-only and unknown add nothing
    with their codes; crossing back and forth does not cycle; modes stay monotonic.
  - [X] T141d Show external PSI, own PSI and the saturating signal in `observe` and `doctor`,
    with `saturated_by_psi`, `psi_system_only`, `psi_unknown` and `held_back.contention_psi` in
    the catalog and `docs/guarantees.md`.
  - [X] T141e Native gate, deferred like T054; see [native-gates.md](native-gates.md).
    *Steps 1 and 3 passed on the test host on 2026-10-26; step 2 passed on 2026-10-27 under a
    second unprivileged account (`docs/validation.md`, Reruns with launch pacing).*

## Phase 12: Revisions from review items 002–013 (done)

**Purpose**: requirement changes made directly during review, each with its commits. They change
no behavior beyond what those commits did.

- [X] T154 FR-013: a floor-breach step reverses only after free memory has stayed above the floor
  by `StepState.to_free`, or `noise_tolerance_mib` if larger, for the calm window plus the
  episode's reclaim backoff (`src/contention.rs`, `src/policy.rs`; `affb281`).
- [X] T155 FR-021: a larger external demand gets the fair share at once and is watched for one
  response window; it is recorded as satisfied when the host has capacity to spare and its usage
  stayed within its baseline plus the contention floor (`src/contention.rs`; `391ef63`).
- [X] T156 FR-021: satisfied records do not expire; at most 64 are kept per resource, those
  whose principals were seen longest ago go first, and a recorded principal rejoins at once
  (`src/contention.rs`, `src/policy.rs`; `ac1e8dc`).
- [X] T157 FR-034: running opportunistic CPU is charged at its p95 only once it has a full
  `admission_usage_window_s` of samples, and at max(request, observed) before that
  (`src/policy.rs`; `a25704b`).
- [X] T158 FR-021: under GPU compute of unknown amount, a duty cycle the supervisor reports
  applied counts as what own work holds, at the larger of the applied and current targets
  (`src/policy.rs`, `src/agent.rs`, `src/model.rs`, `src/supervision.rs`; `b8429b3`, `9852623`).
- [X] T159 FR-045, FR-068: `attribution_tolerance_percent` above its default is
  `relaxed_detection` in `KEY_CLASSES`, which also marks it in setup (`src/config.rs`;
  `3f61b8c`).
- [X] T160 FR-071: the allowance holds back one publish threshold in total, shared over the
  running allocations, not one each (`src/policy.rs`; `1ffd605`).
- [X] T161 FR-020–FR-022, FR-065: memory keeps only the floor under Neutral and Performance;
  their memory margins are refused at load and not offered by setup, and the memory allowance
  step is a per-mode constant (`src/config.rs`, `src/setup.rs`, `src/contention.rs`,
  `src/policy.rs`; `5eeb374`).
- [X] T162 FR-034: each GPU's memory admission cools down on its own; CPU and RAM state never
  reset it, and the node-wide cooldown stays only behind the 0.2 expansion flags
  (`src/policy.rs`, `src/contention.rs`; `9f2b5b5`, `0f3131b`).
- [X] T163 FR-028: the agent blocks admission for own work only while it is being reclaimed;
  `policy::request_admission` holds a request past own target with `admission_over_own_target`
  (`src/policy.rs`, `src/contention.rs`; `97185cd`).
- [X] T164 FR-028 as amended: coordinator placement follows the node's admission without counting
  steps, and moves within the noise tolerance or the reversal hysteresis are jitter
  (`src/coordinator.rs`, `src/policy.rs`; `17bc695`).
- [X] T165 FR-021: measured GPU compute groups external processes into principals by UID,
  counts device time nobody accounts for as external while an external process is listed, and
  returns to the measured path only after `quiet_window_intervals` measured intervals
  (`src/telemetry.rs`, `src/policy.rs`, `src/model.rs`; `cb277fc`, `60c8887`, `9b08de1`).

## Phase 13: Convergence

- [X] T166 Under PSI-only saturation (busy below `saturation_percent`, external PSI above
  `psi_some_percent`), probe every contending principal in `src/contention.rs` and
  `src/policy.rs`, with T141c, tests first in `tests/contention.rs`:
  - `modes_respond_to_psi_monotonically`;
  - with PSI high and flat external usage, every mode regains its uncontended target, with
    Yield First ≤ Neutral ≤ Performance at every interval;
  - a satisfied record holds while usage stays within the noise tolerance, and re-probing
    follows only the record rules;
  - an external user that grows while PSI is high gets its share;
  - floors hold and Guaranteed work is never paused in every PSI case.
  Per FR-072 and FR-021, Session 2026-10-26 (missing).
- [X] T167 With T141d, state in the `doctor` PSI line and `docs/guarantees.md` that a cgroup
  shared by own work and other users' processes counts as own and its pressure is never counted,
  per FR-072 (partial).


## Phase 14: Convergence (item 017)

- [X] T168 Write the ramp tests first in tests/contention.rs:
  - on an idle simulated host, Performance reaches 90% of available capacity minus the margin within 10 s, for CPU and for GPU compute;
  - Yield First and Neutral ramps are unchanged, with Yield First ≤ Neutral ≤ Performance at every interval;
  - an external workload started mid-ramp gets its demand, up to its fair share, within SC-012;
  - an external process that appears and disappears every interval causes no cap/uncap cycling;
  - RAM and GPU memory scale-up timing is unchanged.

  Per SC-004 (missing).
- [X] T169 Scale CPU and GPU compute up one step per monitor interval in Performance, and keep RAM and GPU memory at 2 s, through `ResourceThresholds` in src/contention.rs. The ladder, the allowance's rise (FR-071) and the CPU and RAM admission cooldowns read the same interval. Per FR-034 (missing).
- [X] T170 Make the agent's launch-slot rule a pure function in src/agent/service.rs, so one Performance heartbeat admits as much as fits within the allowance, and test it. Per FR-034 and R4 (missing).
- [X] T171 Measure CPU `held_back` against the ancestor CPU ceiling the budget uses, and show the gap as its own `ceiling` line in `doctor`, observe and the node report. Test first: 4000 m scoped, 2000 m ceiling, no external user, contention 0. Per SC-016 (partial).
- [X] T172 Set the defaults `performance_ramp_s` = 10 and `performance_ramp_percent` = 90 in src/contention.rs, and update docs/guarantees.md (the ramp target, and the scale-up rates at :158). Per the defaults table (partial).
- [X] T173 Add the ramp gate to native-gates.md: time to 90% on an idle test host, and a contender started mid-ramp, with exact commands. It is written, not run; it runs only with the user's approval. Per the plan's ramp gate (missing).
- [X] T174 Add an end-to-end supervisor test with a fake backend that supports the RAM cap, in tests/managed_supervision.rs, covering the Guaranteed clamp, the recorded target, the unprompted report, keeping the cap under a hint, and reconcile on query. Per FR-011 (missing; the D.10 gap in report 011).
- [X] T175 Resolve `source-manifest.json`: remove the stale tracked copy if nothing reads it, and keep generated copies out of the tree; otherwise regenerate it with the pack tool and test that it is current. Per Constitution: source export (contradicts).
- [X] T176 Make the helper that creates `.agent-test-*` directories clean up on success, failure and panic with a drop guard, keeping the home-only guard; then delete the leaked directories. Per CLAUDE.md test hygiene (partial).
- [X] T177 Release the agent lock explicitly, so a descriptor inherited between fork and exec cannot keep it; the lock test must pass 20 runs in a row with the default thread count. Per the agent lock (partial).

## Phase 15: Convergence (item 017 Part 4)

- [X] T178 Run the Performance ramp gate (`tools/ramp_gate.py`, "Performance ramp gate" in native-gates.md) on the authorized test host, idle and with a contender started mid-ramp, and record `ramp_seconds`, `contender_served_seconds` and `foreign_actions` in docs/validation.md. *Deferred: runs only with the user's approval in the session.* Per SC-004 and plan: ramp gate (missing)
  *On 2026-10-26 the ramp passed (2.7–2.8 s) and a contender under the account that runs
  CedeGrid got about 0.72 core. On 2026-10-27, under a second unprivileged account, the ramp
  took 2.6, 2.7 and 1.2 s and the contender reached 0.9 core in 3.0 s pinned and 2.0 s unpinned
  (median 0.94 and 0.92 core), `foreign_actions` 0 (SC-004; `docs/validation.md`).*
- [X] T179 Add the ramp tests (`cargo test --locked --test contention ramp`) and the held-back ceiling test to quickstart §1, and list `tools/ramp_gate.py` in the docs/validation.md matrix and the Performance ramp's hardware evidence under its open gates. Per plan: Faster Performance ramp and Item 017 fixes (partial)

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (Phase 1)**: none.
- **Foundational (Phase 2)**: needs Setup, and blocks every story.
- **US1 (Phase 3)**: needs Foundational. This is the MVP.
- **US2 (Phase 4)**: needs US1 (one authority, error classification).
- **US3 (Phase 5)**: needs US2 (ladder, actuators).
- **US4 (Phase 6)**: needs US2 (actuation stage, hint step).
- **Revisions (Phase 6a)**: needs US3 and US4, and blocks Phase 6b.
- **Utilization revisions (Phase 6b)**: needs Phase 6a, and blocks US5.
- **US5 (Phase 7)**: needs US1 (quiet class, launch contract), Foundational negotiation (T016)
  and Phase 6b.
- **US6 (Phase 8)**: needs Foundational (T014, T016). It can run alongside US2–US5, except T093,
  which needs the effective-mode resolution in T070 (US3), and T089, which builds on T122 and
  T129 (Phase 6a).
- **US7 (Phase 9)**: needs Foundational config (T014) and the caution and floor rules of T131
  (Phase 6a). It can run alongside US1–US6.
- **US8 (Phase 10)**: needs US1–US3 outputs and the `doctor` output of T136 (Phase 6a).
- **Polish (Phase 11)**: needs the stories it documents or validates.

### Within each story

1. Tests first, and confirm they fail.
2. Pure logic (`contention.rs`, `policy.rs`).
3. Telemetry.
4. Supervisor and agent effects.
5. Coordinator.
6. SDKs and docs.

---

## Parallel Example

```text
# Foundational tests together:
T005 tests/policy.rs (schema 3)    T006 tests/configuration.rs    T007 tests/coordinator.rs    T008 tests/upgrade.rs+backup.rs

# US1 tests together:
T018 src/telemetry.rs tests    T019 tests/kernel.rs    T020 tests/supervision.rs    T021 tests/agent.rs
# US1 implementation in different files (after T025–T028):
T029 src/agent.rs    T030 src/supervision.rs

# US4 tests together:
T072 npm drain tests    T073 python targets    T074 npm targets    T075 supervisor targets

# Revision tests together (Phase 6a):
T122 tests/configuration.rs    T123 tests/supervision.rs    T127 tests/policy.rs principals    T130 SDK targets

# US7 (independent of US2-US6); T100 follows T099 in the same file:
T098 src/setup.rs caution mapping tests    T099 tests/cli.rs flag tests
```

---

## Implementation Strategy

### MVP first (US1 only)

1. Phases 1–2.
2. Phase 3 (US1): the D1–D6 fixes, one authority, no SIGKILL on errors.
3. **Stop and validate.** Run quickstart §1–2. This alone stops self-drains and the kill loops.

### Incremental delivery

1. US2 brings graduated yielding.
2. US3 brings modes.
3. US4 brings worker targets.
4. US5 brings GPU sharing, which needs the native GPU gate (T088).
5. US6, US7 and US8 can proceed alongside once Foundational is done.
6. Native gates T054 and T088, and threshold validation T117, need authorized Linux hosts.
   Hardware claims stay open until those run.

## Phase 16: Convergence (item 023)

- [X] T180 [P] Write failing unit tests for the own CPU bounds per source in `src/telemetry/managed.rs`: leaf `usage_usec` quantization = 1 µs + one tick per thread that can be running (the smaller of the summed threads and the scope's CPUs), process clock = 1 ns + the same, `/proc/<pid>/stat` = 2 ticks + 2 ms as now; `CONFIG_HZ` parsed from `/boot/config-<release>` text with a 10 ms fallback; a `nohz_full` list that is not empty keeps the `/proc` source; a source change between two reads gives no reading, per plan "Item 023" and research R9 (missing)
- [X] T181 [P] Write gated Linux tests in `src/telemetry.rs`'s unit tests, which reach the private readers (`CEDEGRID_TEST_CPU_CLOCK=1`, `#[ignore]`): a busy child measured over 2 s reads within 2% of wall time from the process clock and within 2 ticks of `/proc/<pid>/stat`; a child that starts and joins many short-lived busy threads is counted in full by the clock; an exited child, or an identity whose start time does not match, gives no reading, per research R9 (missing)
- [X] T182 [P] Write failing tests with fake leaf files for the leaf source: `usage_usec` is used only when `cgroup.procs` read before and after `cpu.stat` is the same set, contains the leader and lies within the family's own verified PIDs; a foreign member or a membership change falls back to the clock; family members inside the leaf are counted once, per plan "Item 023" and Constitution I (missing)
- [X] T183 Implement the three own CPU sources in `src/telemetry/managed.rs` (leaf `usage_usec`, the process clock under a pidfd checked against the verified identity, `/proc/<pid>/stat`), with the bounds in a comment and the source in use in the `managed_cpu_attribution` evidence, until T180–T182 pass, per research R9 (missing)
- [X] T184 [P] Write failing tests for own-only GPU intervals in `src/telemetry.rs` and `tests/policy.rs`: an inventory of only own verified PIDs, one of them new, is measured with external 0 and own = device utilization, sets `own_only`, takes the measured path at once without restarting the streak and causes no cap from Unknown; each failing condition alone keeps today's path (a graphics process, a PID 0 entry, an unresolvable PID, MPS, unattributed memory growth beyond the baseline plus `noise_tolerance_mib`); a listed external PID keeps today's measured or Unknown path; an empty interval sets the memory baseline but stays on today's path, per FR-021 and research R18 (missing)
- [X] T185 Implement `GpuSnapshot.own_only` (`src/model.rs`, default false, skipped when false), the own-only classification with pidfd identity checks against the verified identities the managed collector passes, and the per-device memory baseline (`src/telemetry.rs`, `src/telemetry/gpu.rs`, `src/telemetry/managed.rs`), and the measured path for own-only intervals in `gpu_compute_path` (`src/policy/measure.rs`), until T184 passes, per FR-021 and data-model "GpuSnapshot compute amount" (missing)
- [X] T186 Update `docs/guarantees.md` (the GPU attribution paragraph: own-only intervals) and `docs/validation.md` (the Neutral short-GPU-job note becomes "fixed, rerun pending"; the 15-point attribution tolerance is kept until a second device is measured), per FR-021 and report 021 Decision 5 (partial)
- [X] T187 Compare the decision snapshot with the one at `65166b5` and explain any change; a measurement-source change must leave the simulated decisions unchanged, per plan "Item 023" (missing)

## Phase 17: Convergence (item 024)

- [X] T188 State in `docs/guarantees.md` (Resource accounting, own CPU) that own CPU summed for a decision counts one tick of pending time per scope CPU at each read, widened by the read pass, within the per-allocation sums, per plan "Item 024" and research R9 (partial)
- [X] T189 State in `docs/guarantees.md` (principals) that the account CedeGrid runs as is one principal whose unmanaged processes share its fair share with own work and are served as own work's margin frees capacity, while the 2-interval reclaim bound is for other principals' demand, per SC-002 (partial)

## Phase 18: Convergence (item 025)

- [X] T190 [P] Write failing tests for launch pacing: `launch_slots` never exceeds 32 in any mode and still fills a 16-task scope in one Performance heartbeat; a lease-request planner orders initial authorizations and renewals longest waiting first and stops when half the monitor interval is spent, but always sends the longest-waiting request; and an opt-in native gate `tests/launch_burst.rs` (`CEDEGRID_TEST_LAUNCH_BURST=1`): 150 tasks admitted at once by a Performance node at default cadences and a 3 s telemetry TTL keep every observation gap within 1 s and get no initial authorization refused for stale evidence, per FR-034 and research R19 (missing)
- [X] T191 Implement launch pacing in `src/agent/service.rs` and `src/agent/launch.rs`: each launch's preparation on the blocking pool through the runtime handle, at most 4 at a time; `launch_slots` capped at 32 per heartbeat; lease requests within half the monitor interval per iteration, longest waiting first; executions read once per heartbeat reply, until T190 passes, per FR-034 and plan "Item 025" (missing)
- [X] T192 Take the attempt pin in `service_slots` only when a checkpoint head is new or the result is released, with a test of the pin decision (a running slot with nothing new to publish takes no pin), per plan "Item 025" (missing)
- [X] T193 [P] Write failing tests in `tests/policy.rs` for admission under a held CPU step: 13 running tasks with a full p95 window, capped by external growth, give no CPU admission credit and leave no room for a 14th task until their caps are released; an allocation without a held step is charged as before, per FR-034 (missing)
- [X] T194 Implement in `src/policy/measure.rs` (`charge`): while an allocation has a held CPU step, add no usage samples to its window and charge it at max(request, p95 of that window) with no credit, until T193 passes, per FR-034 and data-model (missing)
- [X] T195 [P] Write tests in `tests/contention.rs` that pin FR-013's round bound: one round of rises of held CPU caps totals at most the spare and splits it equally; and a policy simulation of 13 tasks that make up lost CPU, with caps that hold from their first period, keeps own work near its share without cap or pause cycling, per FR-013 and SC-015 (partial)
- [X] T196 [P] Write a failing test in `src/actuation.rs`: after a measured running phase, a changed cap at or above the request starts at target ÷ the measured rate rather than an uncapped period; without a measured rate it starts at target ÷ request, per research R20, so a cap step (FR-011) limits usage from its first period (missing)
- [X] T197 Implement research R20 in `src/actuation.rs`: keep the family's last measured natural rate and start a changed cap at target ÷ rate, until T196 passes (missing)
- [X] T198 Update `docs/guarantees.md` (at most 32 new assignments per heartbeat; the admission charge under a held CPU step; a changed CPU cap starts at its target over the family's measured rate) and `docs/validation.md` (known limits: SC-014's shortfall with report 024's breakdown, Neutral GPU startup beside an idle listed process, how an excess is split across victims, report 024 Decision 3's questions, initial authorization refused while the node's admission is closed, lease request throughput), per FR-034 and item 025 Part 4 (partial)
- [X] T199 Compare the decision snapshot with the one at `65166b5`, explain each changed decision class, and confirm no change in scenarios without a held CPU step, per plan "Item 025" (missing)

## Phase 19: Convergence (item 026)

- [X] T200 [P] Write failing coordinator tests for the first authorization of existing reservations: 150 reservations placed while admission was open, then a report with CPU closed and `admission_own_new` set, are all authorized while they fit the budget and none beyond it; each other cause alone still refuses (a held cap with a 14th reservation, external growth, a floor breach, unknown host measurement, a full spool, a report without the field); placement of a new reservation under the same report is still `Closed`, per FR-028 and research R22 (missing)
- [X] T201 [P] Write failing policy tests in `tests/policy.rs`: `admission_own_new` is set on CPU and RAM while admission is closed only by never-measured new allocations or the cooldown only they restarted, and not when an allocation measured before is unknown, own CPU reads above host busy, a step holds work back (or its cooldown runs), external demand grows, or the floor is breached; never on a GPU resource, per FR-028 (missing)
- [X] T202 Implement the cause in the policy: an `unknown_new` total in `charge`, the inconsistent-measurement CPU hold kept apart, the per-resource stability reset cause in `PolicyState`, and `ResourceStatus.admission_own_new` (`src/model.rs`, default false, skipped when false) set in `admissions` (`src/policy/admission.rs`), until T201 passes, per FR-028 and data-model (missing)
- [X] T203 Carry the cause as `ResourceContentionReport.admission_own_new` (`src/protocol.rs`, `src/agent/report.rs`) and add `authorization_placement` in `src/coordinator.rs` for `grant` at sequence 0, with `placement`'s doc comment stating that new placement and preemption keep it unchanged (audit M10), until T200 passes, per FR-028 (missing)
- [X] T204 Add a negotiated report case with `admission_own_new` to `tests/contracts/cedegrid-0.2.json` (round-trip in `tests/configuration.rs`) and state the exemption in `docs/guarantees.md`, per FR-028 and Constitution VII (missing)
- [X] T205 Compare the decision snapshot with the T199 snapshot and explain each change (the policy's decisions must not change apart from the new field), and run the launch-burst gate in Docker (`CEDEGRID_TEST_LAUNCH_BURST=1`): 0 refusals for closed admission caused by own new allocations, per plan "Item 026" and item 026 Part 0 (missing)

## Phase 20: Convergence (item 027)

- [X] T206 [P] Write failing tests for own CPU and host busy over a window: the kernel CPU tracker reports busy over its last 8 samples with one tick of rounding for the window (`src/kernel.rs` tests); the summed bound has no read-pass term, an established process's delta is divided by its own read interval and a first-interval delta by the aggregate interval, and mixed sources (leaf, clock, stat) keep their own errors (`src/telemetry.rs` tests); in `tests/policy.rs`, 117 processes on 14 CPUs read across 46 ms with no external work keep the steady-state external estimate at or below 1% of capacity, a real one-core external start is still a contended CPU within 2 intervals in Yield First and Neutral with the window in place, and own work bursting from 2000 m to 3600 m on 4 CPUs does not read as external growth, per FR-073 and SC-002 (missing)
- [X] T207 Implement host busy over the window in `src/kernel.rs` (`CpuUsageTracker` sample history, `CpuTopology.busy_window_*`) and, in Full, read the delegated root's `cpu.stat` next to `/proc/stat`, used only while the root holds no process and its child set equals the own leaves, until the kernel tests of T206 pass, per FR-073 and research R23 (missing)
- [X] T208 Implement in `src/telemetry/managed.rs` the per-process divisor for established processes, `summed_lower_bound` without the read-pass term, and per-allocation window baselines giving `Allocation.observed_window_cpu` and `Snapshot.managed_cpu_window_lower_millicores` (`src/model.rs`, optional, skipped when absent), until the telemetry tests of T206 pass, per FR-073 (missing)
- [X] T209 Implement the window in `src/policy/measure.rs`: `cpu_external` keeps the per-interval external estimate and adds the window's; the window's value sets the allowance, targets and admission budget while the per-interval value stays within its own bound of it, else the per-interval value applies at once; the admission p95 samples take `observed_window_cpu` when present, until T206 passes, per FR-073 and FR-034 (missing)
- [X] T210 [P] Write failing tests for batched leases: in `tests/coordinator.rs`, one batch of renewals and initial authorizations is served renewals first by deadline at one clock value, a refusal inside a batch affects only its item, a preparation row survives a failed grant, and a retried batch is idempotent per item; in the agent, 300 slots against a coordinator whose commit takes 9 ms see no lease expire over 60 s with lease work per iteration under 100 ms, a batch is split at 128 items or 1 MiB, an older coordinator gets single requests after a refused batch, and per-item events keep their bytes, per FR-074, FR-032 and SC-008 (missing)
- [X] T211 Implement `Request::Leases` and `Response::Leases` behind `lease_batch/1` (`src/protocol.rs`, `COORDINATOR_FEATURES`, `NegotiatedFeatures` in `src/agent/report.rs`) and the coordinator's batch handler in `src/coordinator.rs`: one session check, clock advance and expiry, one Immediate transaction with a savepoint per item and the preparation row written before the grant's savepoint, until the coordinator tests of T210 pass, per FR-074 (missing)
- [X] T212 Implement the agent's batch in `src/agent/service.rs` and `src/agent/client.rs` (renewals first by deadline then initial authorizations, at most 128 items and 1 MiB per request, fallback to single requests, per-item events and `control_rpc` renew evidence) and skip journal and head reads in the slot pass for slots with no new events while `PRAGMA data_version` is unchanged, until T210 passes, per FR-074 and plan "Item 027" (missing)
- [X] T213 Add a lease batch case to `tests/contracts/cedegrid-0.2.json` with its round trip in `tests/configuration.rs`, per FR-074 and Constitution VII (missing)
- [X] T214 [P] Write failing tests for cancelled reservations: in `tests/policy.rs`, a CPU drain planned on the pending pass for an allocation that then runs is re-planned on the running pass and no drain remains; in `src/agent.rs`, a prepared, unauthorized slot the pending pass cancels gets its drain before any `Authorize` and no `Prepared` request; over the run 6 trace shape, zero drains with CPU as the only contended resource, per FR-012 and SC-007 (missing)
- [X] T215 Implement cancelled reservations: agent facts (`src/agent.rs`) make a prepared, unauthorized slot controllable and an authorized slot whose journal still reads Prepared uncontrollable to the pending pass; `actuate` sends a cancelled slot's drain before any `Authorize` and `service_slots` sends it no `Prepared`; `StepState.pending` and the running-pass re-plan in `src/contention.rs` and `src/policy/ladder.rs`, until T214 passes, per FR-012 (missing)
- [X] T216 [P] Write failing tests in `tests/contention.rs` and `tests/policy.rs`: the run 6 shape (16 tasks of 250 m catch-up work on 4 CPUs, a 60 m external user, Yield First, 5 minutes) shows no pause, caps that change only when external usage changes, and own work averaging at least 90% of capacity minus the user and the margin; own usage over its target while the budget did not fall enters no step until it has stayed over for `min_step_dwell_intervals` with caps applied, then steps with cause `own_over_target`; a budget that fell, by external growth or contention, keeps `external_growth` and SC-002's bounds; `observe`, the catalog and the node report's per-cause step count (for `status`) show `own_over_target`, per FR-011, FR-018, SC-015 and SC-016 (missing)
- [X] T217 Implement `YieldCause::OwnOverTarget`, the enforcement hold and `own_over_target` steps in `src/contention.rs`, `src/policy/ladder.rs` and `src/policy/admission.rs` (no restart of the scale-up interval or CPU stability by a hold), with its reversal code, the drain-permission matches (`src/supervision/unix.rs`, `src/agent/service.rs`), the reconcile reseed, `ResourceContentionReport.cause_summary` (`src/protocol.rs`, `src/agent/report.rs`, optional, skipped when empty) and the catalog row in `docs/guarantees.md`, until T216 passes, per FR-011, FR-018 and research R25 (missing)
- [X] T218 [P] Write failing tests: the policy takes `Snapshot.cedegrid_cpu_lower_millicores` out of the external estimate and the owner's unmanaged principal usage and lowers own targets and budget by it, so in Neutral with unreadable principals CedeGrid's CPU forms no unattributed principal; `doctor` and `status` show it as in use by CedeGrid; the supervisor's next deadline covers the actuator's phase, the lifecycle deadlines and the readiness check, per FR-059, FR-069 and SC-014 (missing)
- [X] T219 Implement CedeGrid's own CPU: the agent measures its process clock and its verified supervisor children's CPU per interval and over the window (`src/agent/service.rs`), the policy and reports use it (`src/policy/measure.rs`, `src/model.rs` `ResourceStatus.cedegrid`, `src/explain.rs`), until T218's policy and output tests pass, per FR-059 and FR-069 (missing)
- [X] T220 Replace the supervisor's 5 ms sleep in its running loop with a wait on its control descriptor, the gate's pidfd and the child server, timed to its next deadline (`src/supervision/unix.rs`, an actuator next-deadline in `src/actuation.rs`), one supervisor per task, until T218's deadline tests pass and the existing supervisor, duty-cycle and drain tests still pass, per plan "Item 027" (missing)
- [X] T221 Add `[profile.release]` with `lto = "fat"` and `codegen-units = 1` to `Cargo.toml` (`panic` stays `unwind`), and an opt-in Linux test `tests/own_cost.rs` (`CEDEGRID_TEST_OWN_COST=1`, listed in `ALLOWLIST` and the Cargo `include`) that reports the agent's and supervisors' CPU and resident memory at 117 tasks, with the agent's top costs by loop stage in thread CPU time; measure it in the Docker Linux check before and after the profile, per plan "Item 027" and SC-014 (missing)
- [X] T222 [P] Write failing tests in `src/telemetry.rs` and `tests/policy.rs`: a display-manager-like context (user ID below `uid_min`) on a device with readable per-process utilization is background while idle (no unknown activity, the own-only rule applies, its memory stays external) and not while it uses `principal_min_percent` or more; without readable per-process utilization it stays unknown activity, per FR-021 (missing)
- [X] T223 Implement background GPU contexts: the collector's `uid_min` and `principal_min_percent`, a UID read for external GPU PIDs, `GpuSnapshot.background_process_ids` (`src/model.rs`, default empty, skipped when empty), their exclusion from the compute amount, activity classification and own-only rule (`src/telemetry.rs`, `src/telemetry/gpu.rs`) and from the policy's unknown activity (`src/policy/measure.rs`), until T222 passes, per FR-021 (missing)
- [X] T224 [P] Write failing tests: in Yield First and in Neutral, a one-core contender leaves and own work is at the uncontended target within 2 s plus 1 interval; a contender that drops from 1 core to 0.5 core keeps the mode's interval; SC-009's on/off load with principals changes the step at most once per switch; SC-002 holds when the contender comes back, per FR-034, SC-009 and SC-002 (missing)
- [X] T225 Implement the contention-ended count (`PolicyState`, `src/policy/measure.rs`) and the one-move raise of held CPU caps and the CPU allowance in Yield First and Neutral (`src/contention.rs`, `src/policy/ladder.rs`, `src/policy/admission.rs`), until T224 passes, per FR-034 (missing)
- [X] T226 Update `docs/guarantees.md` for every changed behavior (the measurement window, lease batches, cancelled reservations, `own_over_target`, CedeGrid's usage, background GPU contexts, the CPU return) and the known limits in `docs/validation.md`, per Constitution VIII (missing)
- [X] T227 Compare the decision snapshot with item 026's (T205) and explain each changed decision class, per plan "Item 027" (missing)
- [X] T228 Native gates for item 027 (Part 6, runs 1–7) on the authorized test host, with exact commands in `native-gates.md`. *Deferred: needs the user's typed approval for item 027 in the session.* Ran at `9916e59`, recorded in `docs/validation.md` "Reruns after item 027"; the gates that missed there are followed in "Item 029 runs", T339 and "Item 047: SC-014's fill and Full's GPU path". Per FR-073, FR-074, SC-014, SC-015 and SC-004 (missing)

## Phase 21: Convergence (item 027 decisions on report 027-1)

- [X] T229 [P] Write failing Linux tests in `src/telemetry.rs` for re-verifying own tasks through a held pidfd: the first check of a live child reads its `/proc` identity once and keeps a pidfd; later checks of the same identity read no identity while the pidfd shows it alive; after the child exits the check fails and the handle is dropped; a mismatched identity gets no handle, per plan "Item 027 decisions on report 027-1" and SC-014 (missing)
- [X] T230 Implement the held handles in `src/telemetry/managed.rs`: the collector's verified set and the process-clock read use the pidfd kept since an allocation's first full identity check, and released allocations drop it; other platforms keep today's path, until T229 passes and the telemetry and agent tests still pass, per plan "Item 027 decisions on report 027-1" (missing)
- [X] T231 [P] Write failing Linux tests in `src/telemetry.rs` for the principal process table: one entry per `/proc/<pid>` directory and none for a thread ID; `/proc/<pid>/stat` parsed with spaces and parentheses in the command; per-process CPU as sysinfo computes it (ticks over the global ticks per CPU, at most the CPU count, zero on first sight); a UID read once per PID and start time, per research R26 and FR-059 (missing)
- [X] T232 Implement the process table on Linux in `src/telemetry.rs` (`sample_principals` and the external memory processes), keeping sysinfo elsewhere, until T231 passes and the principal, policy and agent tests still pass, per research R26 (missing)
- [X] T233 Fix the race in `tests/managed_supervision.rs` `a_hint_carries_the_deadline_the_policy_enforces`: its test control writes `stop` only after the worker has written a `seen` marker, bounded by 5 s, with the assertion unchanged, per plan "Item 027 decisions on report 027-1" (missing)
- [X] T234 [P] Pin FR-021's saturated rule with a focused test in `tests/contention.rs`: under saturation a satisfied principal using less than its recorded demand by more than the contention floor counts at that demand in own work's allowance, and without saturation at its usage, per FR-021 and SC-002 (partial)
- [X] T236 Replace the managed collector's two sysinfo refreshes of own PIDs on Linux with the reads they gave: the `/proc` tick counters only when that last CPU source applies, and resident memory from `statm`, with a Linux test that both match sysinfo for the same process, per plan "Item 027 decisions on report 027-1" and SC-014 (missing)
- [X] T235 Measure the agent's sample stage before and after T230–T232 in the Docker cost test (`tests/own_cost.rs`), and record it with research R26's finding in `docs/validation.md` and the principal counting in `docs/guarantees.md`, per SC-014 (missing)

## Phase 22: Convergence (item 028)

- [X] T237 [P] Write failing tests in `tests/cgroup.rs` (`runtime_steps`) for GPU compute steps in Full: an Opportunistic allocation's `unavailable(GpuCompute)` has no `Cap`, with `cgroup.freeze` authorized and without it; a Guaranteed one still has `Cap` and `Pause`; and for every resource, each step `RootlessActuator` offers is offered by `CgroupActuator` with all controls authorized and with none, per plan "Item 028" (rule and audit) and FR-024 (contradicts)
- [X] T238 [P] Write failing tests in `tests/cgroup.rs` for the freeze duty: with `cgroup.freeze` authorized, `apply(GpuCompute, Cap, Some(500))` runs a duty whose off phase (leaf frozen, confirmed through `cgroup.events`) takes half of each period within 0.05 (the CPU duty test's tolerance) over several periods, with no signal sent; without that authorization the off phase is SIGSTOP at the same share; a refused freeze, or a leaf not frozen within the off phase, falls back to the rootless duty with the refusal in the step's detail, per plan "Item 028" (mechanism) and research R27 (missing)
- [X] T239 [P] Write failing tests in `tests/cgroup.rs` for steps held together: a CPU `cpu.max` cap and a GPU share keep the CPU on `cpu.max` while the GPU duty runs alone, and reversing either leaves the other; a CPU Pause (freeze) with a GPU duty keeps the leaf frozen through the duty's on phases, and reversing the Pause resumes the cycle; a duty starts thawed, and its reversal or `release` thaws only a freeze the duty alone holds; with `cpu.max` not authorized the CPU duty and the GPU share run one cycle at the smaller duty, per plan "Item 028" (one duty cycle per family) (missing)
- [X] T240 [P] Write a test that the GPU duty credit does not depend on the backend: in `tests/cgroup.rs` a Full and a Tracked actuator report the same applied state for a GPU compute cap (step, applied target, step code), and in `tests/policy.rs` the policy credits that reported duty with `applied_duty_credit` at the same amount, per plan "Item 028" (policy) (missing)
- [X] T241 Implement GPU compute Cap in `src/cgroup.rs` `CgroupActuator`: available to Opportunistic work (Guaranteed keeps none); with `cgroup.freeze` authorized the duty's off phase freezes the leaf with the `cgroup.events` readback Pause uses and its on phase thaws only when no other resource holds the freeze; without it, or after a refused freeze, the rootless duty with the refusal kept as detail; `tick`, `next_deadline` and `release` cover the cycle, until T237, T238 and T240 pass and the cgroup and supervision tests still pass, per plan "Item 028" and research R27 (contradicts)
- [X] T242 Implement one duty cycle per family in `src/cgroup.rs`: the GPU duty runs alone beside a `cpu.max` cap and joins the rootless CPU duty at the smaller duty where the CPU cap falls back to it, with reversal of either step leaving the other, until T239 passes, per plan "Item 028" (missing)
- [X] T243 Record in `docs/guarantees.md` and `docs/validation.md` that Full applies the GPU compute share and how (a leaf freeze for the off phase where authorized, else the rootless duty), with the native GPU confirmation deferred to the next approved native batch, and confirm the decision snapshots are unchanged, per plan "Item 028" (docs) (missing)

## Phase 23: Convergence (item 029)

Constitution 7.0.0 (Principle II redefined, maintainer approval of 2026-10-29). Tests first in every group. T275 waits for the maintainer's typed approval after report 029-2.

- [X] T244 [P] Write failing Python tests in `python/tests/test_worker.py` for a stat-gated target read: an unchanged file is not re-read or parsed, a new sequence fires each hook once, and a new target of the same size written within one mtime tick by an atomic rename is seen; with `CEDEGRID_SDK_BENCH=1` an opt-in test reports µs per `poll()` and `draining()` call before (forced re-read) and after, at least 10× cheaper unchanged, per plan "Item 029" (Part 0) (missing)
- [X] T245 Implement the stat-gated cache in `python/cedegrid/worker.py` `ResourceSignals` (inode, size, `mtime_ns`), until T244 passes and the SDK tests still pass, per plan "Item 029" (Part 0) (missing)
- [X] T246 [P] Write failing TypeScript tests in `npm/tests/` for the same stat-gated read, hooks and rename case, per plan "Item 029" (Part 0) (missing)
- [X] T247 Implement the stat-gated cache in `npm/src/worker.ts` `ResourceSignals`, until T246 passes, and document in `docs/worker-protocol.md` that `safe_point()` or `draining()` belongs at natural safe points, per plan "Item 029" (Part 0) (missing)
- [X] T248 [P] Write failing tests for the launch nice by mode: the gate setup's nice for each effective CPU mode and class (Yield First Opportunistic at `cpu.nice`, every other mode and Guaranteed at 0) and the `cpu.nice` evidence per mode, with the Neutral and Performance fair-share tests unchanged, per FR-011 and research R5 (contradicts)
- [X] T249 Apply `cpu.nice` only for the effective Yield First CPU mode in `src/supervision/unix.rs` and `src/rootless.rs`, and show the launch nice in `status` and `explain` for a task now in another mode, until T248 passes, per FR-011 (contradicts)
- [X] T250 [P] Write failing policy tests in `tests/policy.rs` for `oom_preferred`: in-share own RAM marks none; own RAM above its fair share marks exactly the allocations that make up the excess, in the memory victim order; back within the share marks none; `ram.prefer_own_oom_victim` marks all, per FR-023 and research R28 (contradicts)
- [X] T251 [P] Write failing supervision tests for OOM order by fault: the launch value is read and kept as the baseline (a baseline of 100 stays 100, never 0); an `oom` line with `preferred` true writes 1000 to every verified family member through the member's pidfd and `/proc/<pid>` directory and children started later inherit it; false restores the baseline; each change is recorded as evidence (task, from, to, cause, own memory and fair share), per FR-023 and research R28 (contradicts)
- [X] T252 Implement OOM order by fault: `oom_preferred` in the policy (`src/policy/ladder.rs`, `src/model.rs` decision), the agent's `oom` control line (`src/agent/service.rs`, `src/agent/supervisor.rs`), the supervisor's writes and launch baseline (`src/supervision/unix.rs`, no write at launch), `ram.prefer_own_oom_victim` in `src/config.rs` with its risk class in `src/setup.rs`, until T250 and T251 pass, per FR-023 (contradicts)
- [X] T253 [P] Write failing tests for Performance as the default: `Config::default()` runs Performance on every resource, a file selecting Performance loads without an acknowledgement and one with the `performance_mode` entry still loads, a file with no mode gets one `default_mode_changed` notice in `doctor`, setup preselects Performance, and `config example` shows it, per FR-030, FR-031 and FR-051 (contradicts)
- [X] T254 Make Performance the default and no caution option in `src/config.rs`, `src/setup.rs`, `src/setup/interface.rs` and `src/explain.rs`; set Neutral explicitly in the tests that test Neutral, until T253 passes and the suites still pass, per FR-030, FR-031, FR-043–051 (contradicts)
- [X] T255 [P] Write failing policy tests for floors by mode: in Performance, with own memory within its fair share and a floor breach that external growth caused, no hint and no drain on RAM and GPU memory; with own memory above its fair share, own memory steps down to the fair share and no further; on the critical path and for Guaranteed memory in Performance, in-share own memory is not hinted, drained or sent TERM; Yield First and Neutral keep floors first; own memory in Yield First ≤ Neutral ≤ Performance for every observation, per FR-023, FR-024, FR-063 and FR-065 (contradicts)
- [X] T256 Cap the floor-breach excess in Performance and Aggressive at own memory above its memory fair share (`neutral_share` of the RAM view, and of the device's GPU compute principals for GPU memory) in `src/policy/ladder.rs` and `src/policy/measure.rs`, until T255 passes, per FR-023 (contradicts)
- [X] T257 [P] Write failing tests for the save-progress signal: the policy lists in-share allocations as `save_progress` once per breach episode that others' growth caused, never above the share or for an own-fault breach (`tests/policy.rs`); the supervisor's `save_progress` line raises the target file's `save_progress_sequence` (`src/actuation.rs`); both SDKs fire `on_save_progress` once per new value (`python/tests/test_worker.py`, `npm/tests/`); the contract fixture carries the field, per FR-075 (missing)
- [X] T258 Implement the save-progress signal in `src/policy/ladder.rs`, `src/model.rs`, `src/agent/service.rs`, `src/agent/supervisor.rs`, `src/actuation.rs`, `python/cedegrid/worker.py`, `npm/src/worker.ts` and `tests/contracts/cedegrid-0.2.json`, until T257 passes, per FR-075 (missing)
- [X] T259 [P] Write failing tests for OOM recovery: a kill is classified from a recorded sequence of observations (SIGKILL in an interval where `/proc/vmstat` `oom_kill` rose, or the leaf's `memory.events` `oom_kill` in Full), an in-share kill is reported with `yield_cause` `oom_not_own_fault` and the coordinator requeues it without a failure or an attempt (`tests/coordinator.rs`), nothing restarts while free memory is below the floor, and an own-fault kill gets no signal and no requeue, per FR-076 (missing)
- [X] T260 Implement OOM detection and recovery: the host `oom_kill` reading in `src/telemetry.rs`, the leaf reading in `src/cgroup.rs`, the classification and report in `src/agent/*.rs`, `oom_not_own_fault` in `src/protocol.rs`, `src/coordinator.rs`, both SDKs and the contract fixture, until T259 passes, per FR-076 (missing)
- [X] T261 [P] Write failing tests in `tests/contention.rs` and `tests/policy.rs` for Performance compute: a cap that misses its target is recomputed and never goes below the fair share or to Pause; a cap at the minimum duty with usage still above the fair share pauses for at most one interval and returns to the cap; Yield First and Neutral are unchanged; the mode order holds, per FR-022, FR-014 and FR-008 (contradicts)
- [X] T262 Implement the compute floor for Performance and Aggressive in `src/contention.rs` (`LadderInput`, `plan_step`) and `src/policy/ladder.rs`, until T261 passes, per FR-022 (contradicts)
- [X] T263 [P] Write failing tests for the descriptor limit: an agent started under a soft limit of 64 that is given more tasks than fit stays up, refuses the excess with `descriptor_limit`, keeps the running tasks running, and each task's command sees the original soft limit; an `EMFILE` while sampling reads as unknown for that interval (`tests/agent.rs`, unit tests), per FR-077 (missing)
- [X] T264 Implement the descriptor limit in `src/agent.rs`, `src/agent/launch.rs`, `src/agent/service.rs`, the worker gate in `src/main.rs` and `src/supervision/unix.rs`, until T263 passes, per FR-077 (missing)
- [X] T265 [P] Write failing tests in `tests/policy.rs` from report 027 run 5's case (16 tasks of 250 m on 4 CPUs in Performance, a pinned one-core contender): no `own_over_target` cap is removed and set again within the hold window while own work stays over its target, own work's total stays at its target within the contention floor, and the contender's share is unchanged or better, per FR-013 (contradicts)
- [X] T266 Implement the hysteresis for `own_over_target` caps in `src/policy/ladder.rs`, until T265 passes, per FR-013 (contradicts)
- [X] T267 [P] Write failing tests in `tests/contention.rs` for the quiet count: it uses this interval's unattributed estimate, not the window's; an idle 4-CPU scope whose measurement band is near 1% does not hold the return back; a contender that stops and comes back within the quiet window restarts the count, per FR-034 (contradicts)
- [X] T268 Implement the quiet count in `src/policy/measure.rs` `note_cpu_quiet`, until T267 passes, per FR-034 (contradicts)
- [X] T269 [P] Write failing tool tests in `tests/test_pressure_comparison.py` for the harness: both probes check every 50 ms, the unmanaged one through a no-op context, and the `own-efficiency` profile uses the defaults with the agent and coordinator pinned outside the probe's scope and their placement recorded, per plan "Item 029" (Part 1) (missing)
- [X] T270 Implement the harness changes in `tools/pressure_probe.py` and `tools/pressure_comparison.py`, until T269 passes, per plan "Item 029" (Part 1) (missing)
- [X] T271 Record in `docs/validation.md` why the old 0.93–0.94 and 0.71–0.80 ratios are profile effects (the probe's per-unit check; the protection profiles' `reserve_physical_cores = 1` with the decision fields), label those profiles' reserve, and confirm report 027's run 1 and run 3 records, per plan "Item 029" (Part 2) (missing)
- [X] T272 Update `docs/guarantees.md`, `README.md` and `specs/004-aggressive-mode/spec.md` for constitution 7.0.0: the purpose first, Performance as the default, OOM order by fault and what it protects without privileges, floors by mode, the save-progress signal and OOM requeue, the launch nice by mode and the descriptor limit; the 004 note reads "memory floors kept down to the fair share", per plan "Item 029" (Part 2) and Constitution II (missing)
- [X] T273 Compare the decision snapshots with those at `b63b59a` and record each class of change, per plan "Item 029" (missing)
- [X] T274 Write item 029's Part 3 native gate commands into `native-gates.md` (runs 1–7 and the reruns of report 027's failed runs), per plan "Item 029" (missing)
- [X] T275 Native gates for item 029 (Part 3) on the authorized test host. *Deferred: needs the maintainer's typed approval after report 029-2.* Per FR-011, FR-022, FR-023, FR-076, FR-077 and SC-014 (missing)

## Phase 24: Convergence (item 030)

The cross-checked list of item 030 Part B (spec Session 2026-10-30; plan "Item 030"), in the addendum's order. Each finding: a failing test first, then the fix, one commit per finding; a medium finding no test reproduces moves to the report's appendix instead. The 32 low findings are deferred to a follow-up item. T368 waits for the user's typed approval in this session.

**Group 1: retention and history loading (FR-081)**

- [X] T276 [P] Write a failing agent test: with 2100 Released executions in the journal, a heartbeat is accepted and reports the active executions plus the last 256 Released ones, per FR-081 (AGT-1) (contradicts)
- [X] T277 Report active executions plus the last 256 Released in `src/agent/service.rs` (heartbeat) and `src/agent/reconcile.rs`, until T276 passes, per FR-081 (AGT-1) (contradicts)
- [X] T278 [P] Write failing tests that the loop's per-iteration passes (slot service, outbox recovery, `allocation_facts`, sampling, released-spool collection) read only active records, with Released rows scanned once at start and on release, and the holds-table schema check once per start, per FR-081 (EFF-3, R4) (missing)
- [X] T279 Add `ExecutionJournal::active_executions()` in `src/execution_state.rs` and use it in `src/agent/service.rs` and `src/telemetry/managed.rs`; reclaim a Released record's spool once, until T278 passes, per FR-081 (EFF-3, R4) (missing)
- [X] T280 [P] Write a failing state test: `observations` keeps 24 h and at most 200,000 rows, pruned every 600 iterations, per FR-081 (EFF-1) (missing)
- [X] T281 Prune `observations` in `src/state.rs` and `src/agent/service.rs`, until T280 passes, per FR-081 (EFF-1) (missing)
- [X] T282 [P] Write failing tests: an attempt's directory is removed once its outcome receipt is written and publication GC has emptied it, and the spool size is kept without walking `attempts/`, per FR-081 (EFF-5) (missing)
- [X] T283 Remove finished attempt directories and keep the spool total incrementally in `src/agent.rs`, `src/agent/service.rs` and `src/publication_gc.rs`, until T282 passes, per FR-081 (EFF-5) (missing)

**Group 2: supervisor death and stop state (FR-078)**

- [X] T284 [P] Write failing Linux tests: a supervisor killed by SIGKILL in a duty off phase leaves no Opportunistic workload (killed through the parent-death signal) and a running Guaranteed one; SIGTERM, SIGINT and SIGHUP to a supervisor release its steps and drain; supervisors run in their own process group, per FR-078 (SUP-1) (missing)
- [X] T285 Set `PR_SET_PDEATHSIG` with the `getppid()` check in the worker gate (`src/supervision/unix.rs`, `src/main.rs`), add the supervisor's signal handling, and start supervisors in their own process group (`src/agent/launch.rs`), until T284 passes, per FR-078 and research R29 (SUP-1) (missing)
- [X] T286 [P] Write failing cgroup tests: a duty interrupted in its off phase, then a reattaching agent's Query, ends with the leaf thawed; a recorded Pause stays frozen; membership is verified first, per FR-078 (028 Decision 1) (missing)
- [X] T287 Thaw a leaf frozen without a recorded Pause in `CgroupActuator::reconcile` (`src/cgroup.rs`), until T286 passes, per FR-078 (028 Decision 1) (missing)
- [X] T288 [P] Write a failing actuator test: a family whose second member's STOP fails, then release, ends with every member continued, per Constitution II (SUP-5, R2) (contradicts)
- [X] T289 Set `stopped` before signalling, send CONT to every member on resume and release, and signal past a failing member in `src/actuation.rs`, `src/supervision/unix.rs` and `src/child_supervision.rs`, until T288 passes (SUP-5, R2) (contradicts)
- [X] T290 [P] Write a failing test: a refused thaw after a tick error is reported (`step_release_failed`) and retried each tick while the freeze is recorded, per CLAUDE.md's error rule (SUP-4) (contradicts)
- [X] T291 Report release failures and retry the thaw in `src/supervision/unix.rs` and `src/cgroup.rs`, until T290 passes (SUP-4) (contradicts)
- [X] T292 [P] Write a failing test: after the leader exits with managed children draining, a CPU cap keeps cycling and a pause is released when the children's drain starts, per FR-014 (SUP-6) (contradicts)
- [X] T293 Keep ticking while children run and release at the children's drain start in `src/supervision/unix.rs`, until T292 passes (SUP-6) (contradicts)

**Group 3: log capture and supervisor errors**

- [X] T294 [P] Write a failing test: a log-capture quota or write error does not close the workload's pipe; the workload keeps writing, and the outcome counts dropped bytes; the spool reservation is taken in chunks, per Constitution II (SUP-2) (contradicts)
- [X] T295 Read and discard after a capture error and reserve in chunks in `src/supervision/unix.rs` and `src/publication.rs`, until T294 passes (SUP-2) (contradicts)
- [X] T296 [P] Write failing tests: a child server `accept` error, a `try_wait` error and a failed child journal transition hold the step and report; `logs.finish` failing after a clean exit leaves the run completed with the error in the outcome; the error path drains managed children before SIGKILL, per research R3 (R3, SUP-7) (contradicts)
- [X] T297 Hold and report bookkeeping errors and drain children on the error path in `src/supervision/unix.rs` and `src/child_supervision.rs`, until T296 passes (R3, SUP-7) (contradicts)
- [X] T298 [P] Write a failing test: with the supervisor's stderr closed, a failing Allowance, SaveProgress or Query reply does not panic the supervisor (AGT-9) (contradicts)
- [X] T299 Route every supervisor message through the `report` helper in `src/supervision/unix.rs`, until T298 passes (AGT-9) (contradicts)

**Group 4: heartbeat freshness and loop health (FR-079)**

- [X] T300 [P] Write failing tests: a report with `sample_age_ms` over the freshness limit gets no renewals and no offers and causes no drain; a fresh one restores grants; wall clocks 5 s apart do not make a fresh report stale; an older agent keeps the wall-clock check, per FR-079 and contracts/wire-negotiation `report_age/1` (AGT-2) (contradicts)
- [X] T301 Add `NodeReport.sample_age_ms` behind `report_age/1` in `src/protocol.rs`, the agent's sample age in `src/agent/service.rs`, and the stale node rule in `src/coordinator.rs`, until T300 passes, per FR-079 and research R30 (AGT-2) (contradicts)
- [X] T302 [P] Write a failing test: a NeedsReconciliation GPU record whose PID stays in NVML costs one NVML read per heartbeat, no sleep, backing off after 3 checks (AGT-3) (contradicts)
- [X] T303 Replace the release-confirm sleep loop on the reconcile path in `src/agent/reconcile.rs` and `src/telemetry/gpu.rs`, until T302 passes (AGT-3) (contradicts)
- [X] T304 [P] Write a failing service test: with a coordinator that never answers, each iteration still samples and yields within the monitor interval, and the requests are retried, per FR-034 (AGT-4) (contradicts)
- [X] T305 Bound each coordinator RPC in the loop by half the monitor interval in `src/agent/service.rs` and `src/agent/client.rs`, until T304 passes (AGT-4) (contradicts)
- [X] T306 [P] Write failing tests: `EPIPE` on stdout, `NotFound` while walking the spool, a panicked preparation task and `ENOSPC` on a refusal proof skip the interval and keep the agent running (AGT-6) (contradicts)
- [X] T307 Classify transient iteration errors in `src/agent/service.rs` and `src/agent.rs`, until T306 passes (AGT-6) (contradicts)

**Group 5: agent stop, reload and re-adoption (FR-080)**

- [X] T308 [P] Write failing agent tests: SIGTERM and SIGINT end the agent without draining, its supervisors keep running, and a new agent re-adopts them; an explicit operator drain and `max_runtime_seconds` still drain everything, per FR-080 (AGT-5, AGT-7) (contradicts)
- [X] T309 Stop without draining on SIGTERM and SIGINT in `src/agent/service.rs` and `src/policy/ladder.rs` (`drain_all` only for an operator drain), until T308 passes, per FR-080 (AGT-5, AGT-7) (contradicts)
- [X] T310 [P] Write failing tests: SIGHUP applies a `[contention]` mode change from the next iteration (`config_reloaded`) and refuses a change to any other section (`config_reload_refused`), per FR-080 and research R31 (missing)
- [X] T311 Implement the SIGHUP reload in `src/agent/service.rs` and `src/config.rs`, until T310 passes, per FR-080 (missing)
- [X] T312 [P] Write failing tests: a live supervisor whose first Query timed out is re-adopted on a later iteration; replay mode re-adopts, per FR-080 (AGT-8) (missing)
- [X] T313 Retry re-adoption with backoff and re-adopt in replay mode in `src/agent/reconcile.rs` and `src/agent/service.rs`, until T312 passes (AGT-8) (missing)
- [X] T314 [P] Write a failing coordinator test: an Opportunistic offer the agent reports preparing for 15 s does not expire, per FR-074 (AGT-10) (contradicts)
- [X] T315 Extend a preparing offer's deadline in `src/coordinator.rs`, until T314 passes (AGT-10) (contradicts)
- [X] T316 [P] Write a failing test: a re-adopted run keeps its start and last checkpoint times, so victim order ranks it by its real progress, per FR-016 (AGT-11) (contradicts)
- [X] T317 Keep start and checkpoint times in the journal and restore them on re-adoption in `src/execution_state.rs` and `src/agent/reconcile.rs`, until T316 passes (AGT-11) (contradicts)

**Group 6: policy (FR-008, FR-022)**

- [X] T318 [P] Write failing policy tests: CPU unknown in Performance holds own CPU at Neutral's fair share of the last known view with one more principal (`cpu_unknown_fair_share_hold`); with no view yet no new step is taken; Yield First and Neutral keep budget 0, per FR-008 and FR-022 (POL-1) (contradicts)
- [X] T319 Keep the last known CPU view and apply the unknown hold in `src/policy/measure.rs` and `src/policy/ladder.rs`, until T318 passes, per FR-022 (POL-1) (contradicts)
- [X] T320 [P] Write failing tests: Performance pauses only a cap the supervisor reports at the minimum duty (`cap_at_minimum_duty`), with own usage above the fair share, per FR-022 (POL-2) (contradicts)
- [X] T321 Report the minimum duty from `src/actuation.rs` through the supervisor events and use it in `src/contention.rs` `plan_step`, until T320 passes, per FR-022 (POL-2) (contradicts)
- [X] T322 [P] Write failing tests: while RAM is held, the last memory fair share keeps in-share own memory protected at a breach others caused, and the save-progress request goes out when protection applies, per FR-023 and FR-075 (POL-3) (contradicts)
- [X] T323 Keep the last memory fair share through held intervals in `src/policy/measure.rs` and `src/policy/ladder.rs`, until T322 passes (POL-3) (contradicts)
- [X] T324 [P] Write failing tests: one unknown RAM interval keeps the 300 s external peaks, a held CPU interval keeps satisfied records, probes and the quiet count, and one missing observation keeps an allocation's p95 window, per FR-060 and FR-021 (POL-4) (contradicts)
- [X] T325 Carry that state across held and unknown intervals in `src/policy.rs`, `src/policy/measure.rs` and `src/policy/admission.rs`, until T324 passes (POL-4) (contradicts)
- [X] T326 [P] Write a test of a satisfied record above current usage under own-caused saturation in Performance; if the target cycles, it fails and T327 follows, else POL-5 moves to the report's appendix (POL-5) (partial)
- [X] T327 Judge CPU saturation with own work at its Neutral share in `src/policy/measure.rs`, until T326 passes, only if T326 reproduces the cycling (POL-5) (partial)

**Group 7: GPU (FR-021, FR-008)**

- [X] T328 [P] Write failing telemetry tests from report 029 run 5's two shapes: a duty-capped own process beside a light user whose samples lag the device reads measured, not unknown; a saturating user whose truncated means leave 1% unattributed creates no third principal; a disagreement beyond the tolerance still reads unknown, per FR-021 and research R33 (GPU-1, GPU-2) (contradicts)
- [X] T329 Measure device utilization from `Device::samples` over the interval and each PID's share over the whole interval, rounded, with the rounding remainder not a principal, in `src/telemetry/gpu.rs` and `src/telemetry.rs`, until T328 passes (GPU-1, GPU-2) (contradicts)
- [X] T330 [P] Write a failing policy test: while device telemetry is unknown, GPU compute caps and pauses hold, per FR-008 (GPU-3) (contradicts)
- [X] T331 Hold GPU compute ladder entries while device telemetry is unknown in `src/policy/measure.rs` and `src/policy/ladder.rs`, until T330 passes (GPU-3) (contradicts)
- [X] T332 [P] Write a failing policy test: under GPU activity of unknown amount a 500 cap is credited at its applied duty and does not escalate to a pause, per FR-022 (GPU-4) (contradicts)
- [X] T333 Credit applied duties under unknown activity in `src/policy/ladder.rs`, until T332 passes (GPU-4) (contradicts)
- [X] T334 [P] Write a failing policy test: a one-interval NVML gap in GPU memory accounting holds steps through RAM's unknown window and does not drain, per FR-008 (GPU-5) (contradicts)
- [X] T335 Add GPU memory to `unknown_windows` in `src/policy/ladder.rs` and `src/policy/measure.rs`, until T334 passes (GPU-5) (contradicts)

**Group 8: report 029 Decisions 1 and 2 (FR-080, FR-034)**

- [X] T336 [P] Write a failing service test: a draining stop of 150 tasks keeps every iteration within 1 s, with renewals and heartbeats first; the burst test's bound covers the whole run, stop included, per FR-080 (029 Decision 1) (contradicts)
- [X] T337 Spread a stop's Drain lines over iterations within a 250 ms budget in `src/agent/service.rs` and `src/agent.rs`, until T336 passes (029 Decision 1) (contradicts)
- [X] T338 [P] Write failing tests on run 7's trace shape: once the quiet count says contention has ended, held CPU caps move to the target in one move, and own usage returns within 3 s of the contender stopping, in Yield First and Performance, per FR-034 (029 Decision 2) (contradicts)
- [X] T339 Raise held CPU caps to the mode's target in one move at the quiet count in `src/policy/ladder.rs` and `src/contention.rs`, until T338 passes, per FR-034 (029 Decision 2) (contradicts)

**Group 9: cgroup, telemetry and cost**

- [X] T340 [P] Write a failing cgroup test: an SDK-managed child starts under the cgroup backend, verified against its own identity, with the leaf's expected membership the leader plus verified children (SUP-3) (contradicts)
- [X] T341 Verify children by their own identity and the leaf as leader plus children in `src/cgroup.rs` and `src/child_supervision.rs`, until T340 passes (SUP-3) (contradicts)
- [X] T342 [P] Write a failing kernel test: iowait on idle CPUs is not booked as external CPU, per FR-073 (TEL-1) (contradicts)
- [X] T343 Count iowait as idle in the busy upper bound in `src/kernel.rs`, until T342 passes (TEL-1) (contradicts)
- [X] T344 [P] Write failing tests: own shared memory counts as own RAM, through the leaf's `memory.current` in Full and `Pss_Shmem` on the rootless path (TEL-2) (contradicts)
- [X] T345 Count own shared memory in `src/telemetry/managed.rs`, until T344 passes (TEL-2) (contradicts)
- [X] T346 [P] Write failing tests: validation refuses `monitor.interval_ms` at or above `gpu.process_sample_max_age_ms`; the GPU sample cursor's age refreshes on an unchanged timestamp (TEL-3) (contradicts)
- [X] T347 Add the validation in `src/config.rs` and the cursor refresh in `src/telemetry.rs`, until T346 passes (TEL-3) (contradicts)
- [X] T348 [P] Write a failing kernel test: a CPU allowed but offline is left out of the CPU set, so CPU telemetry stays known (TEL-4) (contradicts)
- [X] T349 Intersect the CPU set with `/sys/devices/system/cpu/online` in `src/kernel.rs`, until T348 passes (TEL-4) (contradicts)
- [X] T350 [P] Write a failing test: GPU attribution unknown for an allocation keeps its CPU and RAM readings, per FR-008 (TEL-5) (contradicts)
- [X] T351 Mark only the GPU reading unknown in `src/telemetry/managed.rs`, until T350 passes (TEL-5) (contradicts)
- [X] T352 [P] Write a failing service test: an iteration with no journal change reuses the cached execution records and checkpoint heads (EFF-2) (contradicts)
- [X] T353 Take the cache key again after the agent's own observation insert in `src/agent/service.rs`, until T352 passes (EFF-2) (contradicts)
- [X] T354 [P] Write a failing test: a message to a supervisor is written in one write call (EFF-4) (contradicts)
- [X] T355 Serialize supervisor messages to a buffer before writing in `src/agent.rs`, until T354 passes (EFF-4) (contradicts)
- [X] T356 [P] Write failing tests: a known leaf `oom_kill` count of 0 overrides a host rise; uncounted OOM requeues stop at 3 × `max_attempts`, per FR-076 (AGT-12, R6) (contradicts)
- [X] T357 Trust the leaf count and bound uncounted requeues in `src/agent/oom.rs` and `src/coordinator.rs`, until T356 passes (AGT-12, R6) (contradicts)

**Group 10: forks outside the SDK and SDK reads (FR-082)**

- [X] T358 [P] Write failing tests: a run whose leader forks a child outside the SDK is reported `single_process_violated` (status, `explain`, outcome) once, with the count, in Tracked (`/proc/<leader>/task/*/children`) and Full (leaf members); nothing is done to the child, per FR-082 and Constitution I (R1) (missing)
- [X] T359 Detect processes outside the family each interval in `src/supervision/unix.rs`, `src/cgroup.rs` and `src/telemetry/managed.rs`, carry `outside_family` in the supervisor events and report it in `src/explain.rs`, until T358 passes, per FR-082 (R1) (missing)
- [X] T360 [P] Write failing Python and TypeScript tests: a failed read is not cached, and a target rewritten within one mtime tick with an equal length and a reused inode is seen while the mtime is within 50 ms of now, per contracts/resource-target-file (SDK-1, R5) (contradicts)
- [X] T361 Store the stat identity only after a successful read and re-parse a recent file in `python/cedegrid/worker.py` and `npm/src/worker.ts`, until T360 passes (SDK-1, R5) (contradicts)
- [X] T362 [P] Write failing Python and TypeScript tests: a raising `on_drain` hook is isolated, later hooks run, and `DrainRequested` is raised (SDK-2) (contradicts)
- [X] T363 Isolate drain hooks in `python/cedegrid/worker.py` and `npm/src/worker.ts`, until T362 passes (SDK-2) (contradicts)

**Docs, snapshots and the native gate**

- [X] T364 Update `docs/guarantees.md` for FR-078 to FR-082 (supervisor death, stale reports, agent stop and reload, retention, `single_process_violated`), `docs/worker-protocol.md` for the SDK changes, and `README.md` for the single-process limit, naming PyTorch DataLoader workers, per FR-082 (missing)
- [X] T365 Record report 029 Decision 3 in `docs/validation.md` as a known limit, with run 4's numbers (the other user's p99 beside Tracked duty cycles), per report 029 Decision 3 B (missing)
- [X] T366 Add the new reason codes to the catalog in `src/contention.rs` and `docs/guarantees.md` (`single_process_violated`, `config_reloaded`, `config_reload_refused`, `report_stale`, `cap_at_minimum_duty`, `cpu_unknown_fair_share_hold`) (missing)
- [X] T367 Compare the decision snapshots before and after Phase 24 and record each class of change (missing)
- [X] T368 Run 5 again on the test host beside the light and saturating GPU users, in Performance and Neutral, after the GPU fixes, with the commands in `native-gates.md`. *Deferred: needs the user's typed approval in this session.* Per FR-021 and SC-005 (missing)

## Phase 25: Convergence (item 031)

Multi-process families (spec Session 2026-10-31; plan "Item 031"), in item 031's order. Each part: a failing test first, then the code, one commit per group; Linux tests get their red and green runs in the Docker Linux check, batched per group. T228, T339 and T368 stay as they are.

**Group A: membership (FR-083, FR-084)**

- [X] T369 [P] Write failing Linux tests: the supervisor is its run's child subreaper; a leader that forks 4 CPU-busy children (plain `fork` and `multiprocessing`) reports a family of 5 through the new `family` report, all verified in Principle I's order (pidfd, chain, alive, real/effective/saved IDs); a child that forks a grandchild and exits leaves an orphan that joins with the supervisor as its chain root and is reaped by its own PID with no zombie left; a descendant of an SDK child joins that child's family; the leader's exit status is never taken by the orphan reaper; with a leader and 300 descendants the family holds 256 members (the leader and 255 descendants) and the other 45 are reported `over_bound` with no fd exhaustion. Until T373 these tests launch with `single_process = true`, as `dca282c`'s fork test does, per FR-083 (missing)
- [X] T370 [P] Write failing unit tests with a fake identity source: a descendant whose IDs differ is never signalled and is reported `ids_differ`; an unconfirmed chain is reported `chain_unconfirmed`; a discovery read error holds and is reported once, never drains; the ppid scan stands in when no `children` file exists and `doctor` names it; orphans are found in every supervisor thread's `children` file, per FR-083 (missing)
- [X] T371 Build the family in `src/supervision/unix.rs` and `src/supervision.rs` (subreaper, discovery each duty period and in every stop over all threads, the supervisor's included, verification, orphans, reaping by PID, the 256-member bound, SDK children's descendants, the `family` report and `state`, the module comment that the supervisor forks only its gates), and the fallback line in `src/explain.rs`'s `doctor`, until T369 and T370 pass, per FR-083 and R34–R36, R39 (missing)
- [X] T372 [P] Write failing tests: on Linux the rootless launch accepts a multi-process command with any `single_process` value; on other platforms the single-process or bounded-SDK-children contract is still required, per FR-084 (missing)
- [X] T373 Relax the Linux launch contract in `src/rootless.rs` and `src/supervision/unix.rs`, until T372 passes, per FR-084 (missing)

**Group B: measurement (FR-085)**

- [X] T374 [P] Write failing tests: in Full the leaf reading is used with forked workers once they are family members (the pin `a_forked_worker_in_the_leaf_refuses_the_leaf_reading` turned around); in Tracked the family's CPU is the sum of its members' deltas; workers sharing a large copy-on-write buffer with the leader read about one buffer by summed PSS; verified descendants' GPU memory and compute count as own and are not the owner's unmanaged use, so a 4-worker family in Performance lands at the target beside a synthetic external principal and the leader is not held at the minimum duty, per FR-085 (missing)
- [X] T375 Measure families as own work in `src/telemetry/managed.rs`, `src/telemetry.rs` and `src/agent/service.rs` from the reported members, until T374 passes, per FR-085 and R37 (missing)

**Group C: actuation in Tracked (FR-086)**

- [X] T376 [P] Write failing Linux tests: a stop reaches every member and a member forked during a stop is stopped by the pass loop or at the latest the next interval; continue reaches every member when one member's signal fails, which is reported; a member that exits is dropped without error; each member's `oom_score_adj` baseline is restored and the preferred-victim value reaches new members; a worker forked while its run is marked returns to the leader's baseline, not to the inherited mark; a process outside the family (over the bound, or a fake differing identity) is never stopped, continued or given an `oom_score_adj`. In Full, a leaf of the leader and 4 forked workers accepts `cpu.max`, `cgroup.freeze` and the reattach thaw, and a leaf that also holds a process outside the family still refuses them, per FR-086, FR-023 and FR-078, Constitution I (missing)
- [X] T377 Stop, continue and set `oom_score_adj` on the whole family in `src/actuation.rs` and `src/supervision/unix.rs`, with the run's marked spells for inherited marks, and include descendants in `Family::identities()` for Full's leaf checks, until T376 passes, per FR-086, FR-023 and R38 (missing)

**Group D: run end and drains (FR-087, FR-078)**

- [X] T378 [P] Write failing Linux tests: the leader exits while 2 workers run on, and both get SIGTERM, then SIGKILL after the grace, the outcome counts them in `members_ended`, and the run ends; a drain signals the leader first and SIGKILLs members still alive after the grace; a leader that exits during a drain's grace leaves its members TERMed at once and KILLed at the drain's deadline; a process outside the family is neither TERMed nor KILLed, per FR-087, Constitution I (missing)
- [X] T379 End leftover members at leader exit and after a drain's grace in `src/supervision/unix.rs` and `src/supervision.rs`, until T378 passes, per FR-087 (missing)

**Group E: reporting (FR-082)**

- [X] T380 [P] Write failing tests: status and `explain` show the family size (leader, SDK children, descendants) through the `family_members` note; `outside_family` carries its reason; an agent that re-adopts a supervisor learns an unchanging family from `state`, per FR-082 (missing)
- [X] T381 Carry the family into `AllocationFacts`, note `family_members`, and add `outside_family`'s reason in `src/agent.rs`, `src/agent/service.rs`, `src/policy/admission.rs`, `src/contention.rs` and `docs/guarantees.md`, until T380 passes, per FR-082 (missing)

**Docs, snapshots, cost and the native gate**

- [X] T382 Update `docs/guarantees.md` and `README.md` for families: descendants measured and stepped on Linux, leftovers ended at leader exit (a job script's background server, a closing `wait`), a dead supervisor's members (Full keeps the leaf, Tracked cannot find them again, DataLoader workers exit with their parent), and the single-process contract elsewhere; the known limits: a member stopped by an off phase when the supervisor died stays stopped in Tracked, processes outside the family outlive the run (in Full they keep the leaf nonempty), helpers that start and exit between two discoveries are not counted as own CPU in Tracked, and an orphan's `getppid()` is the supervisor; `docs/worker-protocol.md` where it names that contract, per FR-078, FR-084 and FR-087 (missing)
- [X] T383 Compare the decision snapshots with those at `08cb8f2` and record each class of change (missing)
- [X] T384 Record the held-back table with a 4-worker family against a single process of the same total demand in Performance; own throughput should match, per plan "Item 031" 6 (missing)
- [X] T385 Rerun the own-efficiency gate (029) locally with a family of 16 members of about 50 threads each, and report the cost of discovery and of PSS reads per interval; apply R35's fallback order if over budget, per plan "Item 031" 7 (missing)
- [X] T386 Add exact commands to `specs/001-contention-modes/native-gates.md` for the test host, in Tracked and Full: a `multiprocessing` stand-in with a shared buffer, and a `DataLoader(num_workers=4)` loop where PyTorch is present, per plan "Item 031" 7 (missing)
- [X] T387 Run T386's native gate on the test host. *Deferred: needs the user's typed approval in this session, asked together with T368.* (missing)

## Phase 26: Convergence (item 032)

The derived GPU sample age (spec Session 2026-10-31 Q10; plan "Item 032") and report 030's Decision 3 local part. `/speckit-tasks` would regenerate this file, so the tasks are appended here. T228, T339, T368 and T387 stay as they are.

- [X] T388 [P] Write failing tests, named with `sample_age`: an interval of 3000 with no `gpu.process_sample_max_age_ms` loads and its sample age is 6000; the defaults give 2000; the GPU sample cursor and the quiet-window validation use the same value, per FR-070 and R40 (contradicts)
- [X] T389 Make `GpuConfig::process_sample_max_age_ms` optional and add `Config::gpu_sample_max_age_ms()` in `src/config.rs`, used by every reader (`src/telemetry.rs`, `src/policy/measure.rs`, the quiet-window validation, whose error says when the sample age is derived), until T388 passes, per FR-070 (contradicts)
- [X] T390 [P] Write failing tests, named with `sample_age`: an explicit sample age of 2000 with an interval of 3000 is refused on any host, and the error names both keys and 3001, per FR-070 and contracts/config-contention.md (partial)
- [X] T391 Name both keys and the smallest value that loads in the refusal in `src/config.rs`, until T390 passes, per FR-070 (partial)
- [X] T392 [P] Write failing tests: `validate` prints `derived."gpu.process_sample_max_age_ms"` with its value and rule when the key is unset and nothing when it is set, and `doctor` lists it under "Derived settings" and in its JSON, per FR-070 and plan "Item 032" 2 (missing)
- [X] T393 Show derived settings in `src/config.rs`, `src/explain.rs` and `src/main.rs`, until T392 passes, per FR-070 (missing)
- [X] T394 Document the derived default beside the bounds in `docs/guarantees.md`, per FR-070 and R40 (partial)
- [X] T395 Add run 7's return to the test-host batch of T368 and T387 in `specs/001-contention-modes/native-gates.md`, keeping the agent's decision records for that run in the evidence copy, redacted, per plan "Item 032" 3 (missing)
- [X] T396 Run T395's addition with that batch on the test host and report where the return's lag comes from, or record in `docs/validation.md` that it does not reproduce and close T339. *Deferred: needs the user's typed approval of the batch in this session.* (missing)

## Phase 27: Convergence (item 033)

Supervisor loss in Tracked (spec Session 2026-10-31 Q11 to Q13; plan "Item 033"). `/speckit-tasks` would regenerate this file, so the tasks are appended here. The Linux service tests come from item 033's pin (Part A), which they record. T228, T339, T368, T387 and T396 stay as they are.

- [X] T397 [P] Write failing Linux service tests: with the agent in a session of its own, a Tracked supervisor killed while its family is stopped. Guaranteed (a family with and without SIGHUP and SIGCONT handlers, and a single process) receives no SIGHUP and finishes with its output; Opportunistic ends every member with no stopped process left; with the agent stopped first, the next agent does the same at its start. Each run records the signals, states and ends it saw (the pin), per FR-078 and FR-088 (missing)
- [X] T398 [P] Write failing tests: the gate's workload leads a session of its own (its session ID is its PID), an SDK child's too; standalone `supervise` drains its run on SIGINT, per FR-078 and R41 (missing)
- [X] T399 Call `setsid()` in `worker_gate` after the parent-death signal and before exec, and install the stop handlers in `run_supervisor`, in `src/supervision/unix.rs` and `src/main.rs`, until T398 passes, per FR-078 (missing)
- [X] T400 [P] Write failing tests: the family report carries the run's `uid` and `gid` and is sent right after a stop pass that found a member; the agent writes `family.json` with the descendants, boot ID, IDs and class on each `family` and `state`, per FR-083 and FR-088 (missing)
- [X] T401 Report the family after such a stop pass, add the IDs to `FamilyReport`, and write `family.json` atomically in `src/supervision.rs`, `src/supervision/unix.rs`, `src/supervision/family.rs`, `src/agent.rs` and `src/agent/service.rs`, until T400 passes, per FR-083 and FR-088 (missing)
- [X] T402 [P] Write failing unit tests for `agent::recovery` with a fake identity source: a Guaranteed family is continued and an Opportunistic one killed, each through its pidfd; a record whose PID now has another start time, or whose IDs or boot ID differ, is never signalled and is counted; one member's failed signal leaves the others handled; recovery runs once per run and not for a supervisor that exited after releasing its run; a Full run is never touched; a dead rootless run is not released while a recorded descendant lives, per FR-088, Constitution I (missing)
- [X] T403 Build `agent::recovery` in `src/agent/recovery.rs` and call it from the slot pass when a supervisor exits without releasing its run and from `reconcile_guarded` for a run with no live supervisor, which also releases a dead rootless run only once its recorded descendants are absent, with the counts in `family-recovery.json`, the record's detail and a `family_recovered` event, until T402 and T397 pass, per FR-088 and R42 (missing)
- [X] T404 Replace the Tracked limit in `docs/guarantees.md` and the README with recovery and what remains: a member found and stopped in the instant its supervisor dies stays stopped until an agent runs; say that the workload runs in a session of its own, per FR-078 and FR-088 (partial)
- [X] T405 Add a Tracked supervisor-loss check to the test-host batch in `specs/001-contention-modes/native-gates.md`: a leader and 2 workers in an off phase beside a test-owned light load, the run's own supervisor killed, once per class, recording signals and outcomes as T397 does, per plan "Item 033" 5 (missing)
- [X] T406 Run T405's check with the T368, T387 and T396 batch on the test host. *Deferred: needs the user's typed approval of the batch in this session.* (missing)

## Phase 28: Convergence (item 034)

The batch's three findings (spec Session 2026-10-31 Q14 to Q16; plan "Item 034"). `/speckit-tasks` would regenerate this file, so the tasks are appended here. Each pair: a failing test first, then the code, one commit per group. T339 closes with T412. T117 and T228 stay as they are.

- [X] T407 [P] Write a failing test: an interval's `GpuSnapshot.compute_sampling` records the device's sample count with its first and last timestamps, the window, the period and slots used with the path that chose them, and each PID's sample count with its first and last timestamps, for a measured and an unknown interval, per FR-021 (Q14) (missing)
- [X] T408 Record `compute_sampling` in `src/model.rs`, `src/telemetry/gpu.rs` and `src/telemetry.rs`, optional and left out when empty, until T407 passes, per FR-021 (missing)
- [X] T409 [P] Write failing `measure_compute` tests: the batch's shape (the device sampled about 3 times as often as each process, own and external busy) reads measured within the tolerance; own alone, external alone, both, own stopped half the window, a process seen once, NotFound; a driver sampling processes at the device's rate reads as today; with no PID holding 2 samples, today's slots and unknown reasons stay, per FR-021 (Q14, R43) (contradicts)
- [X] T410 Count each PID's summed samples against the slots of the per-process sampling period in `src/telemetry/gpu.rs` `measure_compute`, with today's slots as the fallback, until T409 passes and the existing GPU tests stay green, per FR-021 (contradicts)
- [X] T411 [P] Write a failing simulation test in `tests/contention.rs` on T396's trace shape: held CPU caps from the minimum duty up (1 m to 253 m) when the contender stops; at the quiet count the held steps together reach the mode's target in one move and own usage is back at 98% of its level within 3 s, in Yield First; and pin Performance's and Neutral's return times at HEAD as not worse, per FR-034 (Q15, R44) (contradicts)
- [X] T412 Raise held CPU caps, hints and pauses by water-filling to the mode's target at the first contention-ended interval, skipping the dwell for that move, in `src/policy/ladder.rs` and `src/contention.rs`, until T411 and T338 pass; then check off T339, per FR-034 (contradicts)
- [X] T413 [P] Write failing unit tests for the Tracked PSS period in `src/telemetry/managed.rs`: K from a fake clock and pass cost against 0.5% of the scope's CPU per interval, with its bounds 1 and 10; RSS growth between passes counted; a member set change forces a pass; a family of one and Full's leaf unchanged, per FR-085 (Q16, R45) (missing)
- [X] T414 Read a Tracked family's PSS every K intervals with the RSS-growth estimate between passes, and report which allocations' RAM is an estimate, in `src/telemetry/managed.rs`, until T413 passes, per FR-085 (missing)
- [X] T415 [P] Write failing tests: a decision that adds or changes a RAM step, marks a new OOM preferred victim, or sends a new save-progress signal, for an allocation whose RAM was estimated makes the agent read PSS fresh and evaluate again on the policy state from before the interval; when the fresh reading does not confirm it, no step is applied, per FR-085 (Q16) and Constitution II (missing)
- [X] T416 Make `PolicyEngine` `Clone` in `src/policy.rs` and add the fresh read and re-evaluation in `src/agent/service.rs`, until T415 passes, per FR-085 (missing)
- [X] T417 Add a shared test helper that removes a state directory's sibling `.<name>.cedegrid-control` with the state directory, and use it in every test that makes state with `tempdir_in(current_dir)`, in `tests/support/` and the unit tests in `src/`, per plan "Item 034" 4 (missing)
- [X] T418 Check that `uninstall` or purge removes a real user's control directory; where no such path exists, document the control directory beside the state directory in `docs/` and report it, per plan "Item 034" 4 (missing)
- [X] T419 Compare `decision_snapshot` with the code at `e949e7f` and record the held-back table, per plan "Item 034" 5 (missing)
- [X] T420 Rerun the own-efficiency gate locally with 031's 16-member family and with a 1 GiB shared buffer, and report the PSS pass cost, the K chosen and the agent's cost per interval, per plan "Item 034" 5 and SC-014 (missing)
- [X] T421 Add item 034's rerun to `specs/001-contention-modes/native-gates.md`: T368 in Performance and Neutral with `compute_sampling` in the trace, T396's Yield First return, and T387's Tracked multiprocessing run with the 1 GiB family for the agent's cost, per plan "Item 034" 5 (missing)
- [X] T422 Run T421 on the test host. *Deferred: needs the user's typed approval in this session after 034.done.* (missing)

## Phase 29: Convergence (item 036)

The GPU sample span and Yield First's bound (spec Session 2026-10-31 Q17 and Q18; plan "Item 036"). `/speckit-tasks` would regenerate this file, so the tasks are appended here. Each pair: a failing test first, then the code, one commit per group. T117 and T228 stay as they are.

- [X] T423 [P] Write failing `measure_compute` tests in `src/telemetry.rs` from report 035's shape, the state carried across intervals: one process sampled at the window's end and the duty-capped probe 1 or 2 device periods before it, at the batch's utilizations, reads a third today and measured after; and idle then busy, first seen mid-run, a PID that leaves and is reused, own stopped over half the window, an agent restart costing at most one interval; the device-rate test unchanged; 034's cases updated to the span rule, per FR-021 (Q17, R46) (contradicts)
- [X] T424 Count each GPU sample over its span since its process's previous sample, min(t − p, window), with a per-device `GpuSampleState` in `src/telemetry/gpu.rs` and `src/telemetry.rs`, removing `compute_slots`, `SlotsFrom` and 034's fallback, until T423 passes, per FR-021 (contradicts)
- [X] T425 [P] Write failing tests for the 4-interval comparison from the rerun's sparse-sample shape: an interval that misses a busy process's sample reads measured over the last 4 with the average amounts; an interval that passes alone keeps its own amounts; an interval that cannot be compared clears the history, per FR-021 (Q17, R47) (missing)
- [X] T426 Add the 4-interval comparison to `measure_compute` in `src/telemetry/gpu.rs`, until T425 passes, per FR-021 (missing)
- [X] T427 [P] Write a failing test: `compute_sampling` keeps the window's start and end, whether the amount came from the interval or the last 4, and per process its previous sample, each sample's timestamp and `sm_util` (at most 32 per interval) and the time counted, per FR-021 (Q17) (partial)
- [X] T428 Reshape `ComputeSampling` in `src/model.rs` and `src/telemetry/gpu.rs` and update the guarantees' sentence on how a sample counts, until T427 passes, per FR-021 (partial)
- [X] T429 [P] Bound Yield First's own-usage return at 2 intervals after the quiet count, and Neutral's and Performance's at 3 s from the contender's stop, in `tests/contention.rs`'s return tests, per FR-034 (Q18) (partial)
- [X] T430 Update `docs/guarantees.md` on the return bound and record in `docs/validation.md` that T396 passes under it on the 035 rerun (`eef1b4d`: the quiet count at 32.7 s, own usage at 98% at 33.7 s), per FR-034 (partial)
- [X] T431 Compare `decision_snapshot` with the code at `bff2d27` and record the held-back table, per plan "Item 036" 4 (missing)
- [X] T432 Add item 036's T368 run to `specs/001-contention-modes/native-gates.md`, with a replay of the new trace's samples through the code, per plan "Item 036" 4 (missing)
- [X] T433 Run T432 on the test host. *Deferred: approved by the user for after 036.done.* (missing)

## Phase 30: Convergence (item 039)

GPU demand from uncapped intervals (spec Session 2026-11-01 Q19; plan "Item 039"; research R48). `/speckit-tasks` would regenerate this file, so the tasks are appended here. Each pair: a failing test, then the code until it passes.

- [X] T434 [P] Write a failing closed-loop test in `tests/contention.rs` from T368's Performance light trace: the light user reads its full-duty amounts while own work runs uncapped and its 70%-duty amounts while capped; on today's code the GPU compute target falls with each cap (a cap, a higher reading, a deeper cap), and with the estimate own work's target holds at capacity less the estimate and the margin, per FR-021, FR-022 (Q19) (missing)
- [X] T435 Keep per device the last 60 uncapped intervals' amounts (the total and each principal UID's) in `src/policy.rs`, classify each interval by the two-decision rule or no running allocation on the device in `src/policy/measure.rs`, and set Neutral's, Performance's and Aggressive's GPU compute view from the estimate and Yield First's target from the larger of the interval's amount and the estimate, the interval's amount before the first uncapped interval, until T434 passes, per FR-021, FR-022, FR-065 (missing)
- [X] T436 [P] Write a failing test: while own work is held back by a GPU compute cap only, a light user that turns saturating is seen at the next window (6 decisions without the device's GPU compute cap actions, 30 s after the last uncapped interval) and the target falls to the fair share; no window opens under a CPU cap or a GPU pause or with a Yield First allocation on the device, per FR-021 (Q19) (missing)
- [X] T437 Add the re-measure window to `src/policy/ladder.rs` and `src/policy/measure.rs`: hold the device's GPU compute steps and leave their actions out for 6 decisions, and restart the estimate from the window's intervals when their mean differs from it by 200 or more, until T436 passes, per FR-021 (missing)
- [X] T438 [P] Write a failing test on `ResourceStatus.gpu_demand`: `uncapped`, `fed`, `estimate`, `intervals`, `since_uncapped_ms`, `window` and `source` across an uncapped interval, a capped one, a window and Yield First, and that the contention report does not carry it, per FR-021 (Q19), data-model "Item 039 additions" (missing)
- [X] T439 Add `ResourceStatus.gpu_demand` in `src/model.rs`, fill it in `src/policy/measure.rs`, and add the estimate's sentence to `docs/guarantees.md`, until T438 passes, per FR-021 (missing)
- [X] T440 [P] Write failing edge tests: with no uncapped interval yet the interval's amount is used; own work idle on the device feeds the estimate; an interval of unknown amount feeds nothing and keeps its unknown path; a changed list of external PIDs (background contexts left out) restarts it; a new `PolicyEngine` starts without one; Yield First's targets on T434's and T436's fixtures are those of the interval's amount or lower, per FR-021, FR-065 (missing)
- [X] T441 Make T440 pass in `src/policy/measure.rs`, per FR-021 (partial)
- [X] T442 Replay through the policy 036's Performance and Neutral light runs and 038's Neutral saturating run from their recorded GPU traces, open loop, and report each run's targets with the estimate, per plan "Item 039" 5 (missing)
- [X] T443 Compare `decision_snapshot` with the code at `15aa96e`, list the snapshots that change (the record added, any target changed) and record the held-back table, per plan "Item 039" 5 (missing)
- [X] T444 Add item 039's T368 run to `specs/001-contention-modes/native-gates.md`: Performance and Neutral beside the light and saturating users with `batch-037`'s harness and the fixed probe, each ratio against own work unmanaged beside the same user in the same run, and the light user's rate, p50 and p99 per window, per plan "Item 039" 6 (missing)
- [X] T445 Run T444 on the test host. *Deferred: needs the user's typed approval in this session after 039.done.* (missing)

## Phase 31: Convergence (item 040)

The GPU duty from own share and T368's gates (spec Session 2026-11-01 Q20 and Q21; plan "Item 040"; research R49). `/speckit-tasks` would regenerate this file, so the tasks are appended here. Each pair: a failing test, then the code until it passes.

- [X] T446 [P] Write failing closed-loop tests in `tests/contention.rs` on a time-sliced device model (each busy context an equal slice of the time the device is busy): one own context beside a saturating user in Performance and Neutral keeps a duty of at least 0.95 and own work within 3 points of unmanaged; two own contexts beside one saturating context in Neutral settle near a duty of 0.75 with own work within 3 points of 500; on today's code the duty falls to about 0.5, per FR-021 (Q20) (missing)
- [X] T447 Keep own work's amount in each uncapped interval in which it ran on the device in `src/policy.rs` and `src/policy/measure.rs`, restarted with the estimate and when the own allocations there change, and send a GPU compute Cap outside Yield First, on a measured device and not for unknown activity, as min(1000, target × 1000 × n ÷ S) in `collect_actions` in `src/policy/ladder.rs`, a Cap sent at 1000 not counting as capped, until T446 passes, per FR-021 (missing)
- [X] T448 [P] Write failing tests: 039's light loop keeps the duty at capacity less the estimate and the margin over own work's uncapped share, and across the two-decision rule and the windows the duty settles without moving by more than one step, a window that changes own work's uncapped share updating it within that window, per FR-021 (Q20), Constitution II (missing)
- [X] T449 Make T448 pass in `src/policy/measure.rs` and `src/policy/ladder.rs`, per FR-021 (partial)
- [X] T450 [P] Write failing tests for the edges and the record: no uncapped share yet keeps target ÷ 1000; a share below the target sends 1000; a new engine starts without a share; Yield First's targets and duties on the fixtures are today's; `GpuDemandRecord.own_uncapped` and `duty`, per FR-021 (Q20), data-model "Item 040 additions" (missing)
- [X] T451 Add `own_uncapped` and `duty` to `GpuDemandRecord` in `src/model.rs`, fill them, and add the duty's sentence to `docs/guarantees.md`, until T450 passes, per FR-021 (missing)
- [X] T452 Replay through the policy 036's Performance saturating and light and Neutral light runs and 038's Neutral saturating run, and report the duty each would set, per plan "Item 040" 4 (missing)
- [X] T453 Compare `decision_snapshot` with the code at `f6ebac7` and list the snapshots that change and why, per plan "Item 040" 4 (missing)
- [X] T454 Note in `docs/validation.md` that 036's Performance saturating 0.628 and 038's Neutral saturating 0.513 are over-yields under SC-017, without rewriting the rows, per SC-017 (Q21) (missing)
- [X] T455 Add T445's run to `specs/001-contention-modes/native-gates.md` with the probe-alone and saturating-alone windows, SC-017's two forms and the old ratio, the other user's rate, p50 and p99, and the duty CedeGrid set in each managed window, and add those windows to `batch-037`'s harness, per SC-017 (missing)

## Phase 32: Convergence (item 041)

The GPU time-slice rule and T445's decisions (spec Session 2026-11-01 Q22–Q26; plan "Item 041"; research R50). `/speckit-tasks` would regenerate this file, so the tasks are appended here. The item names the host run T456, so it comes first and stays deferred. Each pair is test first: the test fails on today's code, then the code makes it pass.

- [X] T456 Run T368's four runs again and Performance and Neutral with two own probe processes beside the saturating user on the test host, with `batch-037`'s harness, the fixed probe and the probe-alone and other-user-alone windows, gated by SC-017 and, in the two-process runs, the other user at ½ × its alone rate × 0.97. *Deferred: needs the user's typed approval in this session after 041.done.* per SC-017 (missing)
- [X] T457 [P] Write failing telemetry unit tests in `src/telemetry/gpu.rs` and `src/telemetry.rs`: `time_sliced` is false with MPS or MIG enabled, true with neither, and unknown when a process query or the MIG query fails; `compute_contexts` lists own work's verified PIDs, the owner's unmanaged PIDs and each non-background PID with a nonzero sample and its UID, and is absent with an incomplete inventory, per FR-021 (missing)
- [X] T458 Add `GpuSnapshot.time_sliced` and `compute_contexts` in `src/model.rs`, read MIG mode in `src/telemetry/gpu.rs` and fill both in `src/telemetry.rs` until T457 passes, per FR-021 (missing)
- [X] T459 [P] Write failing closed-loop tests in `tests/contention.rs` on 040's time-sliced model with contexts: one own context beside the saturating and the light shapes in Performance and Neutral sets no GPU compute cap and own work stays at unmanaged; two own contexts beside one saturating context settle within 3 points of 500 under 040's duty, and the cap is gone within one window after one leaves; two own contexts beside an external principal with two contexts set no cap, per FR-021, FR-022 (missing)
- [X] T460 Count m, E and k over the estimate's window in `src/policy.rs` and `src/policy/measure.rs`, add `ResourceStatus.time_slice` and `gpu_time_slice_fair`, and end Neutral, Performance and Aggressive GPU compute steps on a device where the rule holds in `src/policy/ladder.rs`, until T459 passes, per FR-021, FR-022 (missing)
- [X] T461 [P] Write failing tests: a gap of 10 or 14 points between the device and the per-process sum forms no unattributed principal on GPU compute, 15 or 20 points does and counts as external demand; the owner's unmanaged usage in an interval leaves no gap in the averaged view after it ends, per FR-021, FR-059 (missing)
- [X] T462 Make GPU compute's unattributed principal need `attribution_tolerance_percent` in `src/contention.rs`, and keep the owner's unmanaged usage out of the estimate's entries in `src/policy/measure.rs`, until T461 passes, per FR-021, FR-059 (missing)
- [X] T463 [P] Write failing tests: with fewer than 6 uncapped intervals of own work, a cap opens a re-measure window at once and its duty is target ÷ 1000 until the share has 6 entries; a window that ends short waits the usual 30 s, per FR-021 (missing)
- [X] T464 Make `own_share` need 6 entries in `src/policy.rs` and open the bootstrap window in `src/policy/ladder.rs` until T463 passes, per FR-021 (missing)
- [X] T465 [P] Write failing edge tests: MPS and an unknown `time_sliced` keep today's caps; a background context is not counted; an external process that leaves drops E and k within one window; on the unknown path m = 1 sets no cap and m = 2 keeps target ÷ 1000; Yield First's targets and duties stay as today's, per FR-021, FR-020 (missing)
- [X] T466 Make T465 pass in `src/policy/measure.rs` and `src/policy/ladder.rs`, per FR-021 (missing)
- [X] T467 Replay T445's four runs through the policy and report each decision's m, E and k and whether the rule held, per plan "Item 041" 7 (missing)
- [X] T468 Compare `decision_snapshot` with the code at `d101610` and list the snapshots that change and why, per plan "Item 041" 7 (missing)
- [X] T469 Add the time-slice rule and its equal-slice assumption to `docs/guarantees.md` and the README's GPU compute sentences, MPS and MIG keeping the measured path, per plan "Item 041" 8 (missing)
- [X] T470 Add T456 to `specs/001-contention-modes/native-gates.md` with the two-process runs and their gate, and add the two-process runs to `batch-037`'s harness, per SC-017 (missing)
- [X] T471 In T463's and T464's bootstrap, a re-measure window that ends with fewer than 6 entries in own work's share opens the next at once, replacing T463's last clause ("waits the usual 30 s"): one 6-decision window feeds only 4–5 uncapped intervals under the two-decision rule (`/speckit-analyze` I3), per FR-021 (partial)

## Phase 33: Convergence (item 043)

The time-slice duty from the context counts (spec Session 2026-11-01 Q27–Q30; plan "Item 043"; research R51). `/speckit-tasks` would regenerate this file, so the tasks are appended here. Each pair is test first: the test fails on today's code, then the code makes it pass. T456's remaining runs follow 043.done.

- [X] T472 [P] Write failing closed-loop tests in `tests/contention.rs` on a tick-level time-sliced model (own work's m contexts at the duty, external principals with demands, each busy context an equal share of a tick, quantized readings): m = 2 beside a saturating context settles at 500 ± 30 with no pause and a duty unchanged by readings of 0, 330, 660 and 990; m = 2 beside a light context sends no cap; m = 2 beside a partial context of 40% steps up from d0, own work lands within 30 of 1000 − 400 − the margin and the partial user keeps at least 0.97 × 400, per FR-021, FR-022 (missing)
- [X] T473 Implement the slice-bound check, d0 and the per-device duty state in `src/policy.rs` and `src/policy/measure.rs`, and the ladder's Cap states at the duty with no pause in `src/policy/ladder.rs`, until T472 passes, per FR-021, FR-022 (missing)
- [X] T474 [P] Write failing config tests: `contention.gpu_compute.slice_bound_percent` (75, 50–100) and `slice_step_percent` (5, 1–50) load, validate and are refused outside GPU compute, per FR-025 (missing)
- [X] T475 Add the two keys to `src/config.rs` and `ResourceThresholds` in `src/contention.rs`, used by T473's check and steps, until T474 passes, per FR-025 (missing)
- [X] T476 [P] Write failing tests: m = 3 beside one saturating context (d0 0.667, own work 500 ± 30); m = 2 beside two saturating principals (333 ± 30); m = 2 beside a saturating and a light principal (only the saturating one bound, own work within 30 of half what the light one leaves); a light context turning saturating returns own work to d0 within the stated bound; a saturating context leaving ends the cap within one averaged window, per FR-021 (missing)
- [X] T477 Make T476 pass, per FR-021 (partial)
- [X] T478 [P] Write failing telemetry and unknown-path tests: `compute_contexts` lists the external non-background, non-owner PIDs with UIDs; a known listing with unknown or inconsistent amounts takes d0 from the listed counts with every principal bound and no step-up without utilization; an empty estimate window after a restart does the same; an unknown listing on a device last read time-sliced caps at target ÷ 1000 with no pause; the record carries the bound principals, d0, the duty, the step and the utilization; m × k ≤ E, Yield First and MPS stay as before, per FR-021 (missing)
- [X] T479 Add `GpuComputeContexts.listed` in `src/model.rs` and `src/telemetry/gpu.rs`, the record's fields, and the unknown-path behavior, until T478 passes, per FR-021 (missing)
- [X] T480 Replay 042's two valid managed windows, the unmanaged two-process windows from 042 and T456, and T368's light traces through the policy, reporting each decision's slice-bound check and duty, per plan "Item 043" 6 (missing)
- [X] T481 Compare `decision_snapshot` with the code at `c78f240` and list the snapshots that change and why, per plan "Item 043" 6 (missing)
- [X] T482 Add the time-slice duty to `docs/guarantees.md`, the README GPU section and the config reference for the two keys, per plan "Item 043" 7 (missing)
- [X] T483 Add T456's remaining runs to `specs/001-contention-modes/native-gates.md` with their gates, and the 40 s lead and a light two-process part to `batch-037`'s harness, per plan "Item 043" 8 (missing)

## Phase 34: Convergence (item 044)

Aligned GPU duty phases, the margin at d0 and the busy counts (spec Session 2026-11-01 Q31–Q34; plan "Item 044"; research R52). Each pair is test first: the tests fail on today's code, then the code makes them pass. T456 stays open for 044's host runs; T117 and T228 are unchanged.

- [X] T484 [P] Write failing unit tests in `src/actuation.rs`: while a GPU compute cap is in the cycle the actuator runs while (node clock mod period) < duty × period; two actuators capped 0.3 of a period apart stop and resume together; a duty change mid-period shortens or lengthens the on-phase but never resumes inside an off-phase; `next_deadline` is the next switch on the node clock; a CPU-only cycle keeps its per-allocation phase. In `src/cgroup.rs`, the freeze duty follows the same aligned phase. Per FR-021 (missing)
- [X] T485 [P] Write failing phase-level tests in `tests/contention.rs`: own allocations on or off within each period beside one saturating context, every busy context an equal share. Two caps 0.3 of a period apart on per-allocation phases give own work more than 0.52 at duty 750; on the aligned phase it is within 10 thousandths of m × d ÷ (m + E). Alignment holds across a duty step, a re-measure window, three allocations and an allocation that joins later. Per FR-021, SC-017 (missing)
- [X] T486 Add the aligned phase (`AlignedPhase` on the node's CLOCK_MONOTONIC) to `src/actuation.rs` and use it for `RootlessActuator`'s GPU duty cycles and `src/cgroup.rs`'s `FreezeDuty`, until T484 and T485 pass. Per FR-021 (missing)
- [X] T487 [P] Update 043's shape tests in `tests/contention.rs` to the margin at d0, failing on today's code: at m = 2 beside one saturating context the first duty is 720 (Performance), 675 (Neutral) and 750 (Aggressive), and own work's share is within 30 of 480, 450 and 500. The partial, rise and unknown-path tests take the new d0. Per FR-021, SC-017 (contradicts)
- [X] T488 Make d0 leave the mode's margin in `src/policy/measure.rs`, d0 = (m + E_b) ÷ m × (1 ÷ (k_b + 1) − margin), clamped, as the steps' floor, until T487 passes. Per FR-021 (contradicts)
- [X] T489 [P] Write failing busy-count tests:
  - `src/telemetry.rs`: `compute_contexts` marks the samples readable or not.
  - `tests/contention.rs`: an idle listed context, never sampled busy, doesn't count while samples are readable, even with inconsistent amounts, and d0 stays the one-principal value. With unreadable samples the listing counts it. The light shape with an idle listed context stays uncapped. An empty window takes the interval's sampled busy contexts. The record states its source (window, interval, listing).

  Per FR-021 (contradicts)
- [X] T490 Add `GpuComputeContexts.samples_readable` in `src/model.rs` and `src/telemetry/gpu.rs`, the record's `source`, and the count sources in `src/policy/measure.rs`, until T489 passes. Per FR-021 (contradicts)
- [X] T491 Replay 043's six managed windows through the policy, reporting each decision's count source and d0. Per plan "Item 044" 4 (missing)
- [X] T492 Compare `decision_snapshot` with the code at `c2ae762` and list the snapshots that change and why. Per plan "Item 044" 4 (missing)
- [X] T493 Add the aligned phase, the margin at d0 and the busy counts to `docs/guarantees.md`, `README.md` and `CHANGELOG.md`. Per plan "Item 044" 5 (missing)
- [X] T494 Add 044's host runs to `specs/001-contention-modes/native-gates.md` with their gates: the targets 480 and 450, Aggressive for the record, and the phase measurement. Add the Aggressive two-process part and the phase measurement to `batch-037`'s harness. Per plan "Item 044" 6–7 (missing)

## Phase 35: Convergence (item 045)

The step floor, the per-mille dwell average and the restart set (spec Session 2026-11-01 Q35–Q36; plan "Item 045"; research R53). Each pair is test first: the tests fail on today's code, then the code makes them pass. T117 and T228 are unchanged.

- [X] T495 [P] Write failing step-rule tests in `tests/contention.rs`:
  - dwell readings 100, 100, 99 hold the duty at d0 with a zero margin, where today they raise it;
  - Performance above d0 falls on them, where today it holds;
  - zero margin beside the saturating shape with one reading in three at 99 stays at d0, own work within 30 of 500 and the external context at 485 or more;
  - the partial user at 40% with a zero margin settles within 30 of 1000 − 400 − 10, and Performance and Neutral settle as before.

  Per FR-021 (contradicts)
- [X] T496 Average the dwell's utilization in per mille, rounded, and step on max(margin, 10) in `slice_duty` in `src/policy/measure.rs`, with the record's `utilization_milli` in `src/model.rs`, until T495 passes. Per FR-021 (contradicts)
- [X] T497 [P] Write failing restart-set tests:
  - `src/telemetry.rs`: the device's external PID list holds the owner's unmanaged PIDs.
  - `tests/contention.rs`: an owner's unmanaged process that comes or goes keeps the external estimate, and a new external PID still restarts it.

  Per FR-022 (contradicts)
- [X] T498 Leave the owner's unmanaged PIDs out of `gpu_demand`'s restart set in `src/policy/measure.rs`, until T497 passes. Per FR-022 (contradicts)
- [X] T499 Replay 044-t456's traces through the step rule, with Aggressive as Performance with no margin. Report per window the recorded rises and falls against the replay's at HEAD. Per plan "Item 045" 3 (missing)
- [X] T500 Compare `decision_snapshot` with the code at `b3b426a` and list the snapshots that change and why. Per plan "Item 045" 3 (missing)
- [X] T501 State in `docs/guarantees.md` that with a zero margin CedeGrid takes device time only beyond 1% averaged spare, and update the step rule's wording there. Per plan "Item 045" 4 (missing)
- [X] T502 Add 045's host runs to `specs/001-contention-modes/native-gates.md` with their gates: zero margin beside the saturating user, gated, and Performance as a regression check. Add the per-window rises and falls, with the utilization readings around them, to `batch-037`'s analyzer. Per plan "Item 045" 5 (missing)

## Phase 36: Convergence (item 046)

Own work pays for the re-measure windows (spec Session 2026-11-01 Q37; plan "Item 046"; research R54). The tests come first and fail on today's code; the code then makes them pass. T117 and T228 are unchanged.

- [X] T503 [P] Write failing tests in `tests/contention.rs`:
  - zero margin beside the saturating shape, over the whole run with the windows included: the context gets at least 485 (484 today) and own work's held duty averages within 5 of 750;
  - Performance and Neutral: the context gets at least 490 and 520, and own work is within 30 of 480 and 450;
  - a Yield First neighbour on the device opens no window, so d0' = d0;
  - the partial user's settle point is unchanged;
  - the shapes' first duties are 725, 692 and 643;
  - the record shows d0, w and d0'.

  Per FR-021 (missing)
- [X] T504 In `slice_duty` (`src/policy/measure.rs`), compute w from `GPU_DEMAND_WINDOW_DECISIONS`, `GPU_DEMAND_WINDOW_EVERY_MS` and the monitor interval. Start the duty at d0', which is also its floor; d0' = d0 where no window can open. Add the record's `window_share` and `d0_windows` in `src/model.rs`. Done when T503 passes. Per FR-021 (missing)
- [X] T505 Replay 045's four managed windows, reporting d0, w and d0' per decision. Per plan "Item 046" 2 (missing)
- [X] T506 Compare `decision_snapshot` with the code at `f56e179`, listing which snapshots change and why. Per plan "Item 046" 2 (missing)
- [X] T507 State in `docs/guarantees.md` that own work pays for CedeGrid's re-measure windows on a time-sliced device, so others keep their share over time. Per plan "Item 046" 3 (missing)
- [X] T508 Add 046's host runs to `specs/001-contention-modes/native-gates.md` with their gates: zero margin gated as in 045, Performance and Neutral under T456's gates. Have `batch-037`'s analyzer report d0, w and d0' per window. Per plan "Item 046" 4 (missing)

## Phase 37: Convergence (item 048)

Item 048's converge, scoped to `follow-ups.md`'s open rows for 001 and the code paths items 044 to 047 touched. Each code task: a failing test first, then the code. The follow-up rows stay after the merge unless a later item says otherwise.

- [X] T509 CRITICAL: In `slice_duty` (`src/policy/measure.rs`), clear the stored readings, or hold the step, in any interval without a device utilization reading, so the duty never rises on readings an interval without one left behind; first a failing test in `tests/contention.rs` where readings stop after the dwell fills and `rise_ms` then passes. Per Constitution III, FR-021 (contradicts)
- [X] T510 Bound the check-then-write window in the cgroup freeze duty (`src/cgroup.rs`, `verify_runtime_target` then `write_checked`, taken every period since T486): re-read `cgroup.procs` after each freeze write and thaw at once if the members changed, or close the window; first a failing test in `tests/cgroup.rs` with a member that joins between the check and the write. Per Constitution I, item 030 SUP-11 (contradicts)
- [X] T511 Say in FR-018 (through `/speckit-clarify`) that a refusal at the authorization barrier is not the task's failure, then requeue a replay-safe task after one without using an attempt (`src/coordinator.rs`), test first. Per FR-018, follow-ups "026 Decision 5" (missing)
- [X] T512 Split a contended excess across new victims by usage, so no new victim is capped by the whole excess left and a cap to 0 does not become a pause (`src/policy/ladder.rs`), test first in `tests/contention.rs`. Per follow-ups "024 Decision 2(c)" (missing)
- [X] T513 Charge each supervisor's CPU to its allocation and measure the 173 m SC-014 gap (`src/telemetry/managed.rs`, `docs/validation.md`). Per SC-014, follow-ups "024 Decision 3" (missing)
- [X] T514 Tighten the window's busy bound with per-CPU nanoseconds from `/proc/schedstat` (`src/kernel.rs`), test first, then rerun report 027's 4-CPU idle run on the test host. Per follow-ups "027 Decision 2 B" (missing)
- [X] T515 Charge running work at its mean while no other principal is active and at its p95 once one is (`src/policy/measure.rs`, FR-034), decided on its own: the user kept all four modes (spec Session 2026-11-03). Per SC-014, follow-ups "027 Decision 5 B" (missing)
- [X] T516 State in FR-021 (through `/speckit-clarify`) that the aligned duty phases assume every supervisor shares the agent's `CLOCK_MONOTONIC` time namespace. Per FR-021, follow-ups "044 Q31" (missing)
- [X] T517 Give each of item 030's 31 low findings for 001 (SUP-8 to SUP-14, AGT-13 to AGT-17, POL-6, GPU-6, TEL-6 to TEL-10, EFF-6 to EFF-10, SDK-3 to SDK-9) a test-first pair, or move it to a later feature by name in `follow-ups.md`. Per follow-ups "030 Deferred low findings" (missing)
- [X] T518 Name the sampler's failed check and keep own work's measurement out of external growth: in `src/telemetry/managed.rs` each failure of an allocation's bounds returns a short name, `Allocation.usage_unknown` in `src/model.rs` carries it and `allocation_unknown_hold`'s reason in `src/policy/measure.rs` names it; `PolicyState.cpu_window_external` keeps the last busy-window estimate, used while the window refills, within this interval's bound of it, for at most one window. First failing tests in `tests/contention.rs`: after a window reset the external estimate stays at the kept value while this interval's stays within its bound, and real growth beyond the bound applies at once. Per FR-018, plan "Item 049" (missing)
- [X] T519 In Yield First, keep the scale-up cooldown running through a change that forms no principal (`src/policy/admission.rs` `cooldowns`, a per-resource unattributed count in `PolicyState`), on CPU and RAM. First failing tests in `tests/contention.rs`: an owner login-sized burst does not restart it; another account's one-interval 300 m change does not; another account's change that lasts the principal window does, and Yield First steps down within SC-002's bound; Neutral and Performance keep their rule. Per FR-034, research R57 (missing)
- [X] T520 Add item 049's native reruns to `specs/001-contention-modes/native-gates.md` with exact commands, run them on the test host, and record them in `docs/validation.md`: SC-014 in all three modes from a host-side script, then Yield First with a login every 60 s from each account; report 027's run 1 after T514; Performance with two own probes in Tracked and in Full after T509 and T510, a Full run where a member joins the leaf mid-run, and a Yield First run where the second account starts a sustained one-core load; item 029's run 3 after T512. Per SC-014, SC-002, SC-017 (missing)

Item 049's order: T509, T510, T515, T518, T519, T513, T512, T514, T511, T516, then T520. T517 goes to item 050. T515 stands apart from any change to the modes: the user kept all four, and spec Session 2026-11-03 records the charge on its own.

## Phase 38: Item 051, report 048's follow-ups and the user's answers of 2026-11-03

From plan "Item 051" and research R59 to R62 (spec Session 2026-11-13). Each code task: a failing test first, then the code, one commit each. No wire, SDK or config change.

- [X] T521 Count both synced journal writes of a drain, Draining and Released, in a draining stop's share per iteration (`src/agent.rs`, `stop_drain_quota`), test first in its unit tests. Per FR-080, R59
- [X] T522 Keep the two OOM gates open in `docs/validation.md` "Open operational gates" (the kernel's preference for the marked task, shown by `oom_score` only; an in-share task killed by an OOM others caused, not exercised natively) and name the tests that cover the classification and the requeue. Per FR-076
- [X] T523 Give `tools/pressure_comparison.py` a `--readiness-seconds` option, 60 by default, with its test in `tests/test_pressure_comparison.py`. Harness only
- [X] T524 Pin in `tests/policy.rs` that an idle device's GPU memory budget is the capacity less the floor and less memory no process accounts for, with no noise tolerance withheld. Per SC-014
- [X] T525 Name why GPU activity reads unknown in the text of the reason that blocks GPU admission (`src/policy/measure.rs`): the collector's freshness decision, or `no_fresh_sample` for the policy's check; the code `gpu_yield_first_activity_unknown` stays and no field is added, since a resource's status is part of node reports. First a failing test in `tests/policy.rs`. Per FR-010, R60
- [X] T526 Stop the check in `gpu_admission` (`src/policy/measure.rs`) from turning the collector's Quiet into Unknown when a listed context has no newer driver timestamp; the check stays for Idle and Active, which existing tests pin. First failing tests in `tests/policy.rs`: a listed context that is Quiet with no sample admits in Yield First and `validate_gpu_launch_contract` agrees; the collector's Unknown still holds admission; Active still needs a quiet device. Correct the no-sample variant of `d3_a_quiet_external_context_admits_gpu_work`. Per FR-010, FR-027, R60
- [X] T527 Write failing tests in `tests/policy.rs` (`floors_by_mode`) for the over-share order: a hint-deaf over-share task is drained and in-share tasks get no step; one that ignores its hint is drained at the hint deadline, in Yield First, Neutral and Performance; a hint-ready one that frees in time is not drained; no more over-share tasks are drained than bring own memory within its share; in-share tasks are asked only after the over-share tasks were handled; pins that nothing changes with no task over its share and that a drained task is admitted again once its memory fits. Per FR-016, FR-023, FR-037, R61
- [X] T528 Implement the over-share order in `src/policy/ladder.rs`: the over-share set as the victim-order prefix that covers own memory above its fair share, those victims first, counting what each was asked for while its hint or drain wait runs and its whole amount once a drain is decided, in-share tasks only the remainder; off when the share is unknown; per device for GPU memory. `plan_step`, the victim order, the Performance cap, `guaranteed_may_drain` and the OOM mark stay. Per FR-016, FR-023, FR-037, FR-063, R61
- [X] T529 State the over-share order in `docs/guarantees.md` and `specs/001-contention-modes/data-model.md`, with the trade-off that the floor can stay breached for the over-share task's wait, and correct `docs/validation.md` where it says the GPU noise tolerance counts against the fill. Per FR-023, SC-014
- [X] T530 Let the owner's own unmanaged GPU compute not hold Yield First's GPU admission, with one reading for both callers of `contention::gpu_admission` (`src/contention.rs`, `src/policy/measure.rs`, `src/policy.rs`), from the device's compute contexts. First failing tests in `tests/policy.rs`: the owner's unmanaged process does not hold it; another account's does; an unreadable owner stays unknown; the owner's use still counts in the owner's share; the launch recheck agrees in each case. State it in `docs/guarantees.md`. Per FR-027, FR-059, R62
- [X] T531 Compare the decision snapshots (`tests/support/snapshot.rs`) before and after T526, T528 and T530, list which scenarios change and why, and give the held-back table for idle and small external users. Per plan "Item 051"
- [X] T532 Add item 051's native reruns to `specs/001-contention-modes/native-gates.md` with exact commands: B1; B2's three comparisons with `--readiness-seconds`, after T526 and again after T530; B3 and C1 against SC-014's restated fill; C4 with the hint-deaf over-share task. Run them on the test host once it has disk space and record them in `docs/validation.md`. Per SC-012, SC-014, FR-027, FR-080

Item 051's order: T525, T526, T527, T528, T529, T530, T531, then T532. T521 to T524 were done before the phase was written.
