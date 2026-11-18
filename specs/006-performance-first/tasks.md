# Tasks: Performance First

**Input**: [spec.md](spec.md), [plan.md](plan.md); constitution 9.0.0.
**Tests**: tests first for every policy, telemetry, supervision and broker change (Principle VI).
**Owners**: tasks marked **(001)** go to the 001 thread, which holds the policy files; the rest are
this thread's.

## Phase 1: Foundation

- [x] T001 Amend the constitution to 9.0.0 in `.specify/memory/constitution.md` (Principle II:
  Principals, Arrival order, Admission, Loss by mode, Admit immediately, Unprotected, Who
  carries the risk, Work-conserving; Principle V: the unit weight).
- [x] T002 Add `CedeGridWork { arrival_unix_ms, protected_arrival_unix_ms }`, `PrincipalUsage.cedegrid_cpu`,
  `PrincipalUsage.cedegrid_gpu` and `PolicyInput.arrivals` (allocation ID to arrival) to
  `src/model.rs`, optional and serde-defaulted, with a round-trip test there. Arrivals sit on
  the input, not on `Allocation`, so no allocation literal changes.
- [ ] T003 **(001)** Rename `Decision.unprotected_budget` to `admit_immediately_budget` in
  `src/model.rs`, `src/policy.rs` and its consumers (`src/protocol.rs`, `src/agent/report.rs`,
  `src/agent/service.rs`, `src/coordinator.rs`) and their tests, with T020.

## Phase 2: User Story 1 and 2, identification and arrival (this thread)

- [x] T010 [US1] Test first, then put the marker `cedegrid-work=v1;cpu=<mode>;gpu=<mode>;arrival=<ms>`
  on each supervisor's command line in `src/agent/launch.rs`; check that the hidden dispatch in
  `src/main.rs` still reads only the spec path.
- [x] T011 [US1] Keep each run's arrival per assignment in the execution journal
  (`src/execution_state.rs`): the earliest arrival of the run's running assignments, else the
  launch time; restore it after an agent restart. Fill `PolicyInput.arrivals` in the
  agent's policy input.
- [x] T012 [US2] Test first, then identify other accounts' CedeGrid work in `src/telemetry.rs`:
  supervisors' command lines and markers, parent chains within 32 levels, the System mode unit
  check from `/proc/<pid>/cgroup`, and per-resource `CedeGridWork` (CPU from busy processes,
  GPU per device from NVML process lists). Unreadable means plain.
- [ ] T013 [US2] Measure identification's cost at 200 processes against SC-008, in a unit
  benchmark-style test in `tests/telemetry.rs`.

## Phase 3: Admission (001 thread)

- [ ] T020 **(001)** Tests first in `tests/policy.rs` and `tests/contention.rs`, then in
  `src/policy/measure.rs` and `src/contention.rs`: Performance and Aggressive admit as plain
  Linux (capacity less reserve, within the envelope, no margin, no deduction, CedeGrid's own
  CPU not subtracted) when no first-comer contends; hold as today counting only first-comers;
  unknown activity holds only for a first-comer. Produce `admit_immediately_budget`, counting
  only protected first-comers.
- [ ] T021 **(001)** GPU compute: the time-slice admission hold counts only first-comers in
  Performance and Aggressive (`src/policy/measure.rs`, `src/policy.rs`), with tests for both
  orders and both options.
- [ ] T022 **(001)** Reason codes `admitted_as_plain`, `held_for_first_comer` and
  `admit_immediately_ignored_for_protected_first_comer` in `src/contention.rs`; `explain.rs`
  rows for principals; decision snapshots re-recorded and reviewed.
- [ ] T023 **(001)** 001's spec and `docs/guarantees.md` text for admission in Performance and
  Aggressive.

## Phase 4: Options (this thread)

- [x] T030 [US2] Test first, then add `admit_immediately` and `confirm_admit_immediately` to the
  job contention fields in Rust (`src/execution_model.rs`, `src/protocol.rs`), Python
  (`python/cedegrid/`), TypeScript (`npm/src/`) and `tests/contracts/cedegrid-0.2.json`; they
  apply to Performance and Aggressive work only.
- [x] T031 [US3] Rename 004's `unprotected_aggressive` and `confirm_unprotected_aggressive` to
  `unprotected` and `confirm_unprotected` in the same places, the System mode key
  `modes.unprotected_aggressive` to `modes.unprotected`, the setup flag to
  `--allow-unprotected`, and the docs; allow Performance as well as Aggressive.
- [x] T032 [US2] The coordinator places, and the agent's launch check reserves, a run with
  either option against `admit_immediately_budget` (`src/coordinator.rs`,
  `src/agent/service.rs`).
- [ ] T033 [US2] `[caution]` for admit immediately and `UNPROTECTED` with the raise in force in
  `observe`, `doctor`, the coordinator status and the log (`src/main.rs`, `src/pagination.rs`,
  `src/agent/service.rs`).

## Phase 5: Raise (this thread)

- [x] T040 [US3] Test first, then the broker request `raise { on }` in `src/broker/request.rs`,
  `src/broker/server.rs` and `src/broker/systemd.rs`: only while `modes.unprotected` is true and
  the caller's ordinary unit sits in its account slice; `user-<uid>.slice` `CPUWeight` set and
  read back; a 30 s lease the agent renews; reset on withdrawal, expiry, the unit's stop, removal
  from the group and broker start (its own drop-in only). The administrator policy's
  `unprotected_cpu_weight` (default 400, 101 to 10000) goes in `src/broker/admin_policy.rs`.
- [ ] T041 [US3] (open: per-thread nice with Principle I's thread checks, and its withdrawal
  when a protected first-comer arrives) Test first, then the supervisor's nice raise in
  `src/supervision/unix.rs`:
  read `RLIMIT_NICE`, set the lowest permitted value no lower than −10 on the run's verified
  processes, and report none when not permitted.
- [x] T042 [US3] The agent asks for and withdraws the raise per FR-016 and FR-018
  (`src/agent/service.rs`, `src/system_mode.rs`).
- [ ] T043 [US3] Container fixtures for the broker request in `tests/system_mode.rs`, and the
  security review package in `specs/005-system-mode/security-review.md` and
  `specs/004-aggressive-mode/security-review.md`.

## Phase 6: Gates (this thread)

- [x] T050 Add the `first`, `first-gpu`, `batch`, `vram`, `cg-later-gpu` and `cg-first-gpu`
  shapes, the `admit` arm, the second account's `--role peer` and plain arms for every shape to
  `tools/aggressive_gates.py`, with tests in `tests/test_aggressive_gates.py`. CedeGrid against
  CedeGrid runs on GPU compute: where the host splits CPU by account, the kernel gives each
  account its share whatever the other starts, so a CPU shape would show nothing. The `ram`
  shape waits for the user's approval, since its plain arm drives the host into OOM.
- [ ] T051 Run FR-023's gates on the test host and record them in `docs/validation.md` under
  feature 006.

## Phase 7: Docs and checks

- [ ] T060 Update `docs/guarantees.md` (with T023), the 004 and 005 docs and specs for the
  rename and the new admission, and `CHANGELOG.md`.
- [ ] T061 Full checks: fmt, clippy, the serial suite, both Python suites, npm, MSRV.

## Dependencies

T002 and T003 come first and are pushed before the 001 thread starts T020. T020 to T022 and T010
to T012 run in parallel. T032 needs T020. T040 to T042 need T031. T051 needs everything else.
