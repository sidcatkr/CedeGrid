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

- [ ] T005 [P] Write failing tests in `tests/policy.rs` for snapshot schema 3:
  - v1 and v2 recordings replay with unchanged decisions;
  - the schema 3 fields round-trip: `reserved_memory_mib`, `free_memory_mib`,
    `unattributed_memory_mib`, `ComputeActivity::Quiet`, and the CPU/RAM usage bounds.
- [ ] T006 [P] Write failing tests in `tests/configuration.rs` for `[contention]`:
  - the defaults `default_mode = "neutral"`, `max_mode = "performance"`, `floor_policy = "a"`
    and `neutral_memory_policy = "a"`;
  - the data-model.md rules: "`default_mode ≤ max_mode`", "`yield_first_margin_percent`
    1–90; > `performance_margin_percent`", "`neutral_tolerance_points` 1–50",
    "`hysteresis_percent` 1–99", "`quiet_window_intervals` ≥ 1";
  - `cpu.nice < 0` warns and is treated as 0;
  - rejection of unknown keys.
- [ ] T007 [P] Write failing tests in `tests/coordinator.rs` for feature negotiation:
  - the reply carries `features = ["api/1", "contention/1"]`;
  - a `NodeReport` without new fields is accepted;
  - the fixtures in `tests/fixtures/0.2/` parse.
- [ ] T008 [P] Write failing tests in `tests/upgrade.rs` and `tests/backup.rs` for coordinator
  distributed schema 4:
  - migrating from schema 3 adds `drain_requests.cause TEXT NOT NULL DEFAULT
    'coordinator_drain'`, `reservations.yield_cause TEXT` and a CHECK on `reservations.phase`,
    and recreates the triggers;
  - backup and restore round-trip schema 4.

### Implementation

- [ ] T009 Move the duplicated resource arithmetic into `impl Resources` in `src/model.rs`:
  charge = max(request, observed), add, subtract and fits. Replace the copies in
  `src/policy.rs`, `src/agent.rs`, `src/coordinator.rs` and `src/execution_state.rs`. Behavior
  must not change; the existing suites must still pass.
- [ ] T010 Split `agent::run()` in `src/agent.rs` into functions for slot lifecycle, outbox,
  heartbeat/admission and the actuation stage, with no behavior change. `tests/agent.rs` and
  `tests/distributed_service.rs` must pass serially.
- [ ] T011 Add snapshot schema 3 in `src/model.rs`:
  - `SCHEMA_VERSION = 3`, and `supported_schema` accepts 1–3;
  - optional `GpuSnapshot.reserved_memory_mib`, `free_memory_mib` and
    `unattributed_memory_mib`;
  - `ComputeActivity::Quiet`;
  - optional CPU/RAM lower and upper usage bounds, with serde defaults.
- [ ] T012 Add the Decision output types in `src/model.rs`: `ReasonCode`, `YieldAction`,
  `Decision.resources`, `Decision.actions` and `Decision.exceptions`, all additive.
  `would_drain` is derived from actions with step ≥ drain (see
  `contracts/decision-output.md`).
- [ ] T013 Define the pure core types in `src/contention.rs`:
  - `ResourceKey`, with the stable order cpu, ram, then GPU keys by UUID with compute before
    memory;
  - `ContentionMode`, with the order yield_first < neutral < performance;
  - `YieldStep` (none = 0 … terminate = 6);
  - `YieldCause`, `Principal`, `ResourceView` and `ResourceThresholds`.
- [ ] T014 Implement `[contention]` in `src/config.rs` per `contracts/config-contention.md`,
  with the data-model.md validation rules. At load time:
  - `gpu.execution_mode` and `gpu.best_effort_external_processes` are accepted, have no
    effect, and produce one warning naming `contention.gpu_compute.mode` and
    `contention.gpu_memory.mode`;
  - `cpu.nice < 0` warns and is treated as 0.
- [ ] T015 Implement coordinator distributed schema 4 as one versioned migration in
  `src/coordinator.rs` and `src/state.rs`, dropping and recreating the triggers. Teach
  `src/backup.rs` and `src/upgrade.rs` the new version.
- [ ] T016 Implement feature negotiation in `src/protocol.rs`, `src/coordinator.rs` and
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

- [ ] T017 [US1] Invert the six `known_defect_*` tests in `tests/policy.rs` and rename them
  `d1_…` to `d6_…`. They assert:
  - D1: no drain on the first observation;
  - D2: no event below 256 MiB;
  - D3: a quiet external context admits GPU work;
  - D4: CPU excess yields by `cap`, not drain;
  - D5: skew within tolerance keeps the budget;
  - D6: one unknown allocation does not drain others.

  They must fail before T025–T028.
- [ ] T018 [P] [US1] Add tests in the `src/telemetry.rs` tests module:
  - unavailable memory = total − free;
  - a double-read disagreement beyond tolerance makes attribution unknown;
  - `Quiet` appears only after `quiet_window_intervals` of healthy NOT_FOUND samples with a
    complete inventory;
  - an idle device's health never becomes InsufficientObservability.
- [ ] T019 [P] [US1] Add tests in `tests/kernel.rs` for CPU busy bounds: USER_HZ rounding and
  iowait count only in the upper bound.
- [ ] T020 [P] [US1] Add tests in `tests/supervision.rs`:
  - an injected control-channel error holds the step;
  - a rejected journal transition is retried and never reaches `kill_and_confirm` for
    Guaranteed work;
  - opportunistic work goes through drain → TERM → KILL with its deadlines;
  - a minimum TERM grace applies after the first SIGTERM actually sent.
- [ ] T021 [P] [US1] Add a test in `tests/agent.rs` proving that supervisors construct no
  collector or `PolicyEngine` and never emit `local_policy_drain`.

### Implementation for User Story 1

- [ ] T022 [US1] Read NVML `reserved` and `free` in `src/telemetry.rs` (from
  `memory_info()`). Compute unavailable = total − free, read device memory before and after
  process enumeration, and report unattributed memory.
- [ ] T023 [US1] Report bounds:
  - managed CPU and RAM lower and upper bounds in `src/telemetry.rs` (per-allocation
    quantization; anonymous PSS vs RSS);
  - host busy bounds in `src/kernel.rs`.
- [ ] T024 [US1] Implement `Quiet` activity, and split observation health from fresh activity
  evidence, in `classify_activity` and `describe_gpu_observation` in `src/telemetry.rs`
  (research R8).
- [ ] T025 [US1] Fix D1 and D2 in `src/policy.rs`:
  - no growth or new-PID event without a prior observation by the same authority;
  - growth must exceed `noise_tolerance_mib` beyond the unattributed band;
  - remove `protection_cap` and the shrink-percent handling.
- [ ] T026 [US1] Fix D5 in `src/policy.rs`:
  - own/host inconsistency within the bounds counts as consistent: clamp external to ≥ 0
    with reason code `inconsistent_within_tolerance`;
  - beyond the bounds, mark the resource unknown and hold.
- [ ] T027 [US1] Fix D6 in `src/policy.rs`: charge an allocation whose observation is missing
  at max(request, last known), mark it unknown (`allocation_unknown_hold`), hold the resource,
  and never zero the budget.
- [ ] T028 [US1] Fix the D3 and D4 decision paths in `src/policy.rs`:
  - `validate_gpu_launch_contract` follows the effective mode, accepting Quiet or Active per
    mode instead of requiring `Idle`;
  - CPU-only excess produces `cap` actions instead of drain. Actuation follows in US2.
- [ ] T029 [US1] Remove the collector and `PolicyEngine` from `PipeControl` in `src/agent.rs`.
  Supervisors keep the lease and drain deadlines only; envelope checks stay in the agent.
- [ ] T030 [US1] Classify post-EXEC errors in `src/supervision.rs` per research R3, and remove
  the direct `kill_and_confirm` from the running-loop error path:
  - control → hold;
  - actuation → mark the step unavailable;
  - drain-file or target-file write → retry, then escalate;
  - journal → bounded retry, then the drain chain for opportunistic work, or hold and report
    for Guaranteed work.
- [ ] T031 [US1] Enforce a minimum TERM grace after the first SIGTERM actually sent, and record
  both the deadline anchor and the actual send time, in `src/supervision.rs` (audit M6).
- [ ] T032 [US1] Emit the reason codes `first_observation_baseline`,
  `vram_noise_below_tolerance`, `inconsistent_within_tolerance` and `allocation_unknown_hold`
  from `src/policy.rs`. Switch the existing prose assertions in `tests/policy.rs` to these
  codes.

**Checkpoint**: US1 delivers correct yielding inputs and one authority. This is the MVP.

---

## Phase 4: User Story 2 - Graduated, reversible yielding (Priority: P1)

**Goal**: a ladder from lower share through cap, pause, hint and drain to terminate, applied by
supervisors to owned processes, reachable after agent restart, with least-cost victims.

**Independent Test**: in a CPU-only contention run, the job moves through lower share, cap and
pause and then reverses, with no drain (quickstart §3).

### Tests for User Story 2 ⚠️

- [ ] T033 [P] [US2] Add property tests in `tests/contention.rs` for the ladder:
  - steps escalate in order, and memory resources skip steps 1–3;
  - `min_step_dwell_intervals` is respected;
  - steps reverse one level at a time below `hysteresis_percent`;
  - drain and terminate never reverse;
  - an unknown measurement holds;
  - a pause escalates after `pause_bound_s`;
  - under a load that switches on and off every 4 monitor intervals, own work changes step at
    most once per switch (SC-009).
- [ ] T034 [US2] Add victim-cost tests in `tests/contention.rs`, covering each rule in
  order:
  1. uncontrollable allocations are excluded;
  2. the victim must free the contended resource;
  3. opportunistic before Guaranteed;
  4. pending before running;
  5. least progress lost;
  6. best fit;
  7. assignment ID as the tie-break.
- [ ] T035 [P] [US2] Add supervisor control tests in `tests/supervision.rs`:
  - `adjust` applies only to owned pidfds;
  - an unknown `kind` is ignored and does not count as AgentLost;
  - `pause` is refused for Guaranteed work;
  - `adjusted` reports unavailable steps.
- [ ] T036 [P] [US2] Add actuation audit tests in `tests/managed_supervision.rs`:
  - every signal and control write targets a verified identity;
  - a reused PID and a foreign PID in a cgroup leaf are refused;
  - both checks still hold after an agent restart reconnects to the supervisor (FR-002).
- [ ] T037 [P] [US2] Add an agent restart test in `tests/agent.rs`: the restarted agent
  reconnects through `control.sock`, reads the applied steps, and never marks the live
  supervisor NeedsReconciliation.
- [ ] T038 [US2] Add control-socket tests in `tests/supervision.rs`:
  - a wrong token and an unauthenticated second connection are rejected;
  - the correct token is accepted and replaces the previous controller.
- [ ] T039 [US2] Add tests in `tests/agent.rs`:
  - drain back-off counts every sent drain, local and coordinator-issued (audit M8);
  - dropping a `ChildSlot` closes the pipe without sending Drain (audit M5).
- [ ] T040 [P] [US2] Add tests in `tests/coordinator.rs`: failures are classified by the
  recorded yield cause regardless of detail text, and a cancelled task becomes terminal instead
  of being re-queued.
- [ ] T041 [US2] Add a test in `tests/agent.rs`: advertised controls include cgroup controls only
  after successful backend preparation with authorized settings, and never PSI observations.
- [ ] T042 [P] [US2] Add non-native tests in `tests/cgroup.rs`, using a temporary fixture tree:
  - a runtime write is refused when leaf membership differs from the verified PIDs or the leaf
    identity changed;
  - `Limit::Max` round-trips.

### Implementation for User Story 2

- [ ] T043 [US2] Implement the pure ladder state machine `contention::plan_steps` in
  `src/contention.rs` (data-model.md YieldStep transitions).
- [ ] T044 [US2] Implement victim cost ordering (research R11) in `src/contention.rs`, fed with
  checkpoint times from the agent's publication records in `src/agent.rs`.
- [ ] T045 [US2] Add an `Actuator` trait to `LaunchBackend` in `src/execution_model.rs`: apply
  a step and target, read it back, and report unavailable steps.
- [ ] T046 [US2] Implement the rootless actuator in `src/actuation.rs`:
  - SIGSTOP/SIGCONT duty-cycle cap and bounded pause through owned pidfds (`OwnedChild` in
    `src/supervision.rs`);
  - lower share reported unavailable at runtime.
- [ ] T047 [US2] Implement the cgroup actuator in `src/cgroup.rs`:
  - a `Limit::{Max, Value}` type;
  - runtime writes of `cpu.weight`, `cpu.max`, `memory.high` and `cgroup.freeze`, with
    `cgroup.events` readback;
  - every write first re-verifies the leaf identity and that membership equals the verified
    PIDs;
  - `assert_empty` replaced for adjustable controls;
  - targets journaled.
- [ ] T048 [US2] Add `Control::Adjust`, `Control::Query`, `Event::Adjusted` and `Event::State`:
  - message types in `src/agent.rs`;
  - handling in `src/supervision.rs` and `src/child_supervision.rs`;
  - applied steps persisted as `AllocationContention` in `src/execution_state.rs`.
- [ ] T049 [US2] Add the supervisor `control.sock` listener in `src/supervision.rs`:
  - socket mode 0600, inside the 0700 output directory;
  - a `hello` carrying the 32-byte token from the spec;
  - a peer UID check;
  - one active controller at a time.
- [ ] T050 [US2] In `src/agent.rs` reconciliation, reconnect to live supervisors through
  `supervisor-identity.json` and `control.sock`, and stop rewriting live supervisors' journals
  (audit H5).
- [ ] T051 [US2] Add the actuation stage in `src/agent.rs`:
  - send `adjust` per allocation from `Decision.actions`;
  - track sent drains with timestamps for back-off (audit M8);
  - make `ChildSlot::drop` close the pipe without sending Drain (audit M5).
- [ ] T052 [US2] Record the yield cause across `src/agent.rs` and `src/coordinator.rs`:
  - the agent sends `AllocationReport.yield_cause` once negotiated;
  - the coordinator persists `drain_requests.cause` when it issues a drain;
  - failures are classified by recorded cause, removing `detail.contains("revoked")`;
  - cancelled tasks become terminal.
- [ ] T053 [US2] In `src/agent.rs`, advertise only the controls the backend can apply (a
  successful `CgroupBackend::new` plus authorized settings), and exclude PSI observations
  (audit H8).
- [ ] T054 [US2] Native-evidence gate, run only when explicitly authorized:
  - add `runtime_adjust_requires_verified_membership` to `tests/cgroup.rs`;
  - run quickstart §4 with `CEDEGRID_TEST_CGROUP_ROOT` on Linux;
  - record the evidence in `docs/validation.md`.
- [ ] T055 [US2] Add a rootless CPU contention scenario in `tests/agent.rs`: a test-owned
  external CPU load leads to `cap` within the Yield First response time, and the step reverses
  after the load stops (quickstart §3).

**Checkpoint**: graduated yielding works on owned processes, including after an agent restart.

---

## Phase 5: User Story 3 - Choose a contention mode per resource (Priority: P1)

**Goal**: Yield First, Neutral and Performance with the same semantics on every resource, plus
floors, policies A/B/C, the envelope and spool rules, and the Guaranteed rules.

**Independent Test**: scripted external loads per mode match the mode definitions in
`tests/contention.rs`.

### Tests for User Story 3 ⚠️

- [ ] T056 [P] [US3] Add tests in `tests/contention.rs` for Yield First: a 25% margin, a
  3-interval response, unknown activity counted as demand, and slow scale-up.
- [ ] T057 [US3] Add tests in `tests/contention.rs` for Neutral:
  - max-min fair share with 1–3 principals plus an unknown-owner principal;
  - own work converges and does not go below its share, except for floors under Neutral
    memory policy A;
  - policy B holds the share.
- [ ] T058 [US3] Add tests in `tests/contention.rs` for Performance and Guaranteed work:
  - a 10% margin, floors and fast scale-up;
  - floor policies A, B and C for Guaranteed work;
  - Guaranteed work is never paused.
- [ ] T059 [P] [US3] Add tests in `tests/agent.rs` for operator limits:
  - an envelope breach is restored per resource, opportunistic work first, with no all-stop
    (FR-057);
  - spool exhaustion blocks admission and drains nothing (FR-058);
  - explicit operator drain still drains every allocation.
- [ ] T060 [P] [US3] Add tests in the `src/telemetry.rs` tests module: external processes
  aggregate into principals by owner UID, and processes with unreadable owners form one
  principal.
- [ ] T061 [P] [US3] Add tests in `tests/supervision.rs`:
  - the worker gate sets `oom_score_adj = 1000` before EXEC;
  - it skips this for Guaranteed work under floor policy C;
  - managed children inherit the value.
- [ ] T062 [P] [US3] Add tests in `tests/policy.rs`:
  - CPU/RAM expansion follows `cpu_ram_scale_up_cooldown_ms`, independently of
    `gpu.scale_up_cooldown_ms`;
  - the effective mode is min(requested, `max_mode`) and is reported per allocation.

### Implementation for User Story 3

- [ ] T063 [US3] Aggregate principals in `src/telemetry.rs`:
  - the owner UID per external process, from sysinfo or `/proc/<pid>/status`, mapping NVML
    PIDs to UIDs;
  - processes whose owner is unreadable count as one principal.
- [ ] T064 [US3] Implement max-min fair share in `src/contention.rs` (research R12).
- [ ] T065 [US3] Implement the Yield First, Neutral and Performance target computations per
  resource in `src/contention.rs`, from `ResourceThresholds`.
- [ ] T066 [US3] Implement floors and policies in `src/contention.rs`:
  - RAM and VRAM floors from `ram.reserve_mib`/`ram.reserve_percent` and
    `gpu.reserve_vram_mib`;
  - floor policies A, B and C, and Neutral memory policies A and B;
  - the Guaranteed rules: no pause, and drain only for a floor breach under A.
- [ ] T067 [US3] Set `oom_score_adj = 1000` in the worker gate before EXEC in
  `src/supervision.rs`, skipping it for Guaranteed work under floor policy C, and verify that
  managed children inherit it.
- [ ] T068 [US3] Apply the envelope and spool rules in `src/agent.rs`: replace the
  `slots.keys()` all-drain with per-resource envelope restoration, and make spool exhaustion
  block admission only.
- [ ] T069 [US3] Decouple CPU and RAM scale-up from `gpu.scale_up_cooldown_ms`, using
  `cpu_ram_scale_up_cooldown_ms`, in `src/policy.rs` and in the launch-slot ramp in
  `src/agent.rs` (audit M1).
- [ ] T070 [US3] Resolve the effective mode per allocation and resource as min(requested,
  `max_mode`), in `src/contention.rs` and `src/agent.rs`.
- [ ] T071 [US3] Add time-stepped scenario tests in `tests/contention.rs` for SC-002, SC-003
  and SC-004 with the provisional defaults.

**Checkpoint**: the three modes behave as specified on CPU, RAM, GPU compute and GPU memory.

---

## Phase 6: User Story 4 - Cooperative workers receive "use less" targets (Priority: P2)

**Goal**: a resource-target file with sequence, deadline and ack, delivered to leaders and
managed children through both SDKs.

**Independent Test**: a compliant SDK worker is not drained, and a non-compliant one is drained
after the hint deadline (quickstart §6).

### Tests for User Story 4 ⚠️

- [ ] T072 [P] [US4] Add TypeScript drain, hook and managed-child tests mirroring the Python
  ones, in `npm/tests/worker-drain.test.ts` (audit M22).
- [ ] T073 [P] [US4] Add Python tests in `python/tests/test_worker.py`:
  - `resource_targets()`;
  - `on_resource_target` fires once per sequence;
  - hook errors are isolated;
  - `ResourceSignals.from_env()` works;
  - a file with `version > 1` is ignored;
  - the ack is written.
- [ ] T074 [P] [US4] Add TypeScript tests mirroring T073, in
  `npm/tests/worker-targets.test.ts`.
- [ ] T075 [P] [US4] Add supervisor tests in `tests/managed_supervision.rs`:
  - the target file is written atomically with an increasing sequence;
  - managed children receive `CEDEGRID_RESOURCE_TARGET_FILE`;
  - compliance is judged by observed usage;
  - without an ack, the hint step is unavailable;
  - non-compliance leads to a drain after the deadline.

### Implementation for User Story 4

- [ ] T076 [US4] Implement the target-file writer and the ack reader in `src/actuation.rs`, per
  `contracts/resource-target-file.md`.
- [ ] T077 [US4] Pass the target-file path through `GateRequest` and the environment in
  `src/supervision.rs`, and to managed children in `src/child_supervision.rs`.
- [ ] T078 [US4] Handle the hint step in the agent actuation stage in `src/agent.rs`: the hint
  deadline, the compliance check and escalation.
- [ ] T079 [US4] Add the Python API in `python/cedegrid/worker.py`: `ResourceTargets`,
  `resource_targets()`, `on_resource_target()`, `ResourceSignals.from_env()` and ack writing.
- [ ] T080 [US4] Add the TypeScript API in `npm/src/worker.ts` (`resourceTargets()`,
  `onResourceTarget()`, `ResourceSignals.fromEnv()`), and export it from `npm/src/index.ts`.
- [ ] T081 [US4] Add the optional v2 context keys `resource_target_file` and `capabilities` in
  the native context builder in `src/agent.rs` and in the `supervise` path. The SDKs must keep
  accepting `schema_version == 2`.
- [ ] T082 [US4] Document in `docs/worker-protocol.md` the target channel, the ack, deadlines,
  and drain exit semantics.

**Checkpoint**: memory contention can be resolved in place by cooperative workers.

---

## Phase 7: User Story 5 - GPU work on shared, occupied devices (Priority: P2)

**Goal**: admission on occupied devices per mode and memory headroom, and coordinator placement
by per-resource admission.

**Independent Test**: a GPU job is admitted and completes on a device with an idle external
context, under Neutral and Performance (quickstart §5).

### Tests for User Story 5 ⚠️

- [ ] T083 [P] [US5] Add launch-contract tests by mode in `tests/policy.rs`:
  - Neutral and Performance admit on quiet or active devices within the floors;
  - Yield First requires quiet;
  - unknown memory blocks.
- [ ] T084 [P] [US5] Add placement tests in `tests/coordinator.rs`:
  - placement honors the per-resource admission from a negotiated `NodeReport.contention`;
  - legacy agents use the 0.2 fields;
  - admission for a resource is blocked while own work on it is above step zero (FR-028);
  - preemption keeps a pass-level victim set and runs only on real slot or capacity exhaustion.

### Implementation for User Story 5

- [ ] T085 [US5] Implement per-device GPU admission in `src/contention.rs` and `src/policy.rs`
  (`validate_gpu_launch_contract` by mode).
- [ ] T086 [US5] Place work by per-resource admission in `src/coordinator.rs`.
  `fits_requested`, `schedule` and `grant` must share one admission predicate (audit M10).
- [ ] T087 [US5] Fix preemption in `src/coordinator.rs`: keep a pass-level projected charge and
  victim set, and preempt only on real exhaustion (audit M9).
- [ ] T088 [US5] Native-evidence gate, run only when explicitly authorized:
  - run quickstart §5 on Linux + NVIDIA with `CEDEGRID_TEST_GPU_UUID` and an idle external
    context, including 100 consecutive launches on the shared device;
  - record the evidence for SC-006 and the VRAM part of SC-001 in `docs/validation.md`.

**Checkpoint**: GPU work no longer needs an empty device.

---

## Phase 8: User Story 6 - Configure modes safely across versions (Priority: P2)

**Goal**: 0.2 configs and peers keep working; per-job overrides with a node cap; deprecations.

**Independent Test**: all examples load unchanged, the mixed-version matrix passes, and 0.2
workers behave unchanged (quickstart §8).

### Tests for User Story 6 ⚠️

- [ ] T089 [P] [US6] Add tests in `tests/configuration.rs`:
  - every `examples/*.toml` and 0.2 config loads unchanged;
  - Neutral is the default;
  - legacy keys warn and have no effect;
  - `floor_policy = "c"` without an acknowledgement fails with
    `ERR_CEDEGRID_DANGER_UNACKNOWLEDGED`;
  - a per-resource mode above `max_mode` fails.
- [ ] T090 [P] [US6] Add the mixed-version matrix in `tests/coordinator.rs` (`mixed_version_*`)
  (SC-008):
  - the old-agent fixture with the new coordinator;
  - a new agent with a coordinator that advertises no features;
  - new with new.
- [ ] T091 [P] [US6] Add SDK compatibility tests in `python/tests/test_worker.py` and
  `npm/tests/worker-drain.test.ts`: a drain-only 0.2-style worker behaves unchanged under the
  new supervisor.
- [ ] T092 [P] [US6] Add client tests in `python/tests/test_transport.py` and
  `npm/tests/sdk.test.ts`: job specs carry `contention` only when it is set.

### Implementation for User Story 6

- [ ] T093 [US6] Add the optional `LaunchRequest.contention` field (skipped when None) in
  `src/execution_model.rs`. In `src/coordinator.rs`, strip it for agents without
  `contention/1`, and place tasks with overrides only on capable agents.
- [ ] T094 [US6] Let the Python and TypeScript clients accept the optional `contention` field in
  job specs, and send it only when set (`python/cedegrid/client.py`, `npm/src/client.ts`).
- [ ] T095 [US6] Make `observe` and `replay` in `src/main.rs` read snapshot schemas 1–3.
- [ ] T096 [US6] Add `CHANGELOG.md` entries for:
  - the Neutral default;
  - the ignored legacy GPU keys;
  - the envelope and spool behavior;
  - the `cpu.nice` clamp.
- [ ] T097 [US6] Add the negotiated optional fields to the shared contract fixture
  `tests/contracts/cedegrid-0.2.json` and to the cross-SDK interop checks
  (`python/tests/numeric_interop.py`, `npm/tests/numeric-interop.ts`).

**Checkpoint**: in-place adoption without breaking deployments.

---

## Phase 9: User Story 7 - Set up a node interactively or from the command line (Priority: P2)

**Goal**: a keyboard interface by default on a TTY, equivalent flags, risk markers, danger
confirmation, and atomic writes that preserve everything else.

**Independent Test**: flags and the interface produce byte-identical files. The non-TTY run
never blocks, and danger choices need confirmation (quickstart §7).

### Tests for User Story 7 ⚠️

- [ ] T098 [P] [US7] Add unit tests in `src/setup.rs` for the risk mapping: policies A have no
  marker, floor policy B and Neutral memory policy B are `caution`, floor policy C is
  `danger`, and a combination takes its highest marker.
- [ ] T099 [P] [US7] Add tests in `tests/cli.rs` for `setup --non-interactive`:
  - exit codes 2, 3 and 4 per `contracts/setup-cli.md`;
  - `--json` output;
  - no prompt or blocking on a non-TTY;
  - a missing config on a non-TTY never offers setup;
  - unmanaged settings and comments preserved byte for byte.
- [ ] T100 [US7] Add Unix PTY tests in `tests/cli.rs`:
  - arrow, Space and Enter navigation;
  - the danger confirmation screen;
  - Esc, `q` and Ctrl-C write nothing and restore the terminal;
  - with `NO_COLOR=1`, no escape sequences, while `[caution]`/`[danger]` remain;
  - output byte-identical to the flag path (SC-011);
  - a command that needs a missing config offers setup on a TTY.

### Implementation for User Story 7

- [ ] T101 [US7] Implement setup choices, defaults from the current config, validation and the
  risk mapping in `src/setup.rs`.
- [ ] T102 [US7] Implement the `toml_edit` writer in `src/setup.rs`: edit only managed tables,
  validate through `Config::load`, then rename atomically.
- [ ] T103 [US7] Add the `setup` subcommand and flags in `src/main.rs` per
  `contracts/setup-cli.md`, with JSON output and stable error codes.
- [ ] T104 [US7] Implement the terminal interface in `src/setup.rs`:
  - termios raw mode through `libc`, restored by a drop guard and on signals;
  - ANSI rendering;
  - keys, consequence lines, and markers in SGR 33 and SGR 31;
  - `NO_COLOR`, `TERM=dumb` and non-TTY detection;
  - the danger confirmation screen and the summary screen.
- [ ] T105 [US7] Implement the line-based prompt fallback in `src/setup.rs`, for terminals
  without cursor control or that are too small.
- [ ] T106 [US7] In `src/main.rs`, offer setup on a TTY when a command needs a missing config.
  Never do this on a non-TTY.
- [ ] T107 [US7] On Windows, setup configures client settings only and refuses node choices
  with a clear error (`src/main.rs`, `src/setup.rs`).
- [ ] T108 [US7] Add a short setup section to `README.md` that points to `docs/guarantees.md`
  for the risk markers.

**Checkpoint**: new nodes are configured safely through setup.

---

## Phase 10: User Story 8 - See what the node is doing (Priority: P3)

**Goal**: per-resource mode, usage, demand, target share, step and reason codes in `observe`,
`doctor`, `status` and node reports.

**Independent Test**: during a scripted contention run, the outputs match the expected values at
each step.

### Tests for User Story 8 ⚠️

- [ ] T109 [P] [US8] Add tests in `tests/cli.rs`:
  - `observe --samples 2 --no-state` JSON includes `resources`, `actions` and `exceptions`;
  - `doctor` shows the modes, policies, exceptions with markers, available steps and
    deprecated-key warnings.
- [ ] T110 [P] [US8] Add a test in `tests/coordinator.rs`: the operator `status` shows each
  node's negotiated contention summary.

### Implementation for User Story 8

- [ ] T111 [US8] Render the decision's resources, actions and exceptions in `observe` and
  `doctor` in `src/main.rs`.
- [ ] T112 [US8] Include node contention summaries in the coordinator status in
  `src/coordinator.rs`.
- [ ] T113 [US8] Add a reason-code catalog in `src/contention.rs`: stable codes, each with one
  sentence of text.

**Checkpoint**: every step change is explainable within one report interval (SC-010).

---

## Phase 11: Polish & Cross-Cutting Concerns

- [ ] T114 Update `docs/guarantees.md` as the single normative source (FR-056):
  - floors;
  - floor and Neutral memory policies with their markers;
  - modes;
  - envelope and spool behavior;
  - the deprecation of `gpu.execution_mode`;
  - the reason-code catalog.
- [ ] T115 [P] Update the layout map in `CLAUDE.md` with `contention.rs`, `actuation.rs` and
  `setup.rs`.
- [ ] T116 [P] Remove code made obsolete by this feature in `src/policy.rs` and `src/agent.rs`:
  the protection-cap fields and the supervisor-side policy plumbing.
- [ ] T117 Validate the provisional thresholds (SC-002–SC-004) on Linux with test-owned loads.
  Record the evidence in `docs/validation.md`, and change defaults in `src/config.rs` only with
  that evidence.
- [ ] T118 Audit the pressure suite for SC-005:
  - run the `tools/pressure.py` scenario with the actuation audit hook;
  - assert zero actions on external processes;
  - assert zero external OOM kills and zero external GPU allocation failures.
- [ ] T119 Check export parity: every new tracked file is in `ALLOWLIST` and the Cargo
  `include`, `tests/test_source_review.py` passes, and the in-memory
  `validate_files(public=True)` passes.
- [ ] T120 Run the full verification:
  - `cargo fmt --check`;
  - `cargo clippy --all-targets -D warnings`;
  - the serial Rust suite;
  - `cargo +1.88.0 check --all-targets`;
  - both Python suites;
  - `npm test`;
  - `cargo check --target x86_64-pc-windows-msvc` for the client paths, where the target is
    installed.
- [ ] T121 Run quickstart §1–3 and §6–8, and record the results.

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (Phase 1)**: none.
- **Foundational (Phase 2)**: needs Setup, and blocks every story.
- **US1 (Phase 3)**: needs Foundational. This is the MVP.
- **US2 (Phase 4)**: needs US1 (one authority, error classification).
- **US3 (Phase 5)**: needs US2 (ladder, actuators).
- **US4 (Phase 6)**: needs US2 (actuation stage, hint step).
- **US5 (Phase 7)**: needs US1 (quiet class, launch contract) and Foundational negotiation
  (T016).
- **US6 (Phase 8)**: needs Foundational (T014, T016). It can run alongside US2–US5, except T093,
  which needs the effective-mode resolution in T070 (US3).
- **US7 (Phase 9)**: needs Foundational config (T014). It can run alongside US1–US6.
- **US8 (Phase 10)**: needs US1–US3 outputs.
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

# US7 (independent of US2-US6); T100 follows T099 in the same file:
T098 src/setup.rs risk mapping tests    T099 tests/cli.rs flag tests
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
