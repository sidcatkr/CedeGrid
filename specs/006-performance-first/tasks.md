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
- [x] T003 **(001)** Rename `Decision.unprotected_budget` to `admit_immediately_budget` in
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
- [x] T013 [US2] Measure identification's cost at 200 processes against SC-008, in a unit
  benchmark-style test in `tests/telemetry.rs`.

## Phase 3: Admission (001 thread)

- [x] T020 **(001)** Tests first in `tests/policy.rs` and `tests/contention.rs`, then in
  `src/policy/measure.rs` and `src/contention.rs`: Performance and Aggressive admit as plain
  Linux (capacity less reserve, within the envelope, no margin, no deduction, CedeGrid's own
  CPU not subtracted) when no first-comer contends; hold as today counting only first-comers;
  unknown activity holds only for a first-comer. Produce `admit_immediately_budget`, counting
  only protected first-comers.
- [x] T024 **(001)** FR-007a, policy: add `Decision.cpu_beside_plain`, true in Performance and
  Aggressive while a plain principal contends for the CPU and no first-comer holds own work;
  in those modes the envelope step leaves CPU overage alone (memory and GPU envelope as before).
  Tests first.
- [x] T025 FR-007a, agent: under `cpu_beside_plain` the agent sets the CPU budget to the
  envelope's CPU times its task limit (`max_workers`), caps neither the budget nor the headroom
  at the envelope's CPU, and `envelope_exceeded` ignores CPU; once the flag clears, new CPU work
  waits until requests fit again. Tests first in `src/agent/service.rs`.
- [x] T026 SC-001, SC-004, start latency: the agent's heartbeat comes early (at most every
  250 ms) when room opens, a slot ends or a reply placed work (`src/agent/service.rs`); and a
  node with room waits at the coordinator for the next queued task behind `await_work/1`
  (`src/protocol.rs`, `src/coordinator.rs`, `src/agent/client.rs`), so new work starts within
  one monitor interval of its submission. Tests first in `tests/coordinator.rs` and
  `src/agent.rs`.
- [x] T021 **(001)** GPU compute: the time-slice admission hold counts only first-comers in
  Performance and Aggressive (`src/policy/measure.rs`, `src/policy.rs`), with tests for both
  orders and both options.
- [x] T022 **(001)** Reason codes `admitted_as_plain`, `held_for_first_comer` and
  `admit_immediately_ignored_for_protected_first_comer` in `src/contention.rs`; `explain.rs`
  rows for principals; decision snapshots re-recorded and reviewed.
- [x] T023 **(001)** 001's spec and `docs/guarantees.md` text for admission in Performance and
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
- [x] T033 [US2] `[caution]` for admit immediately and `UNPROTECTED` with the raise in force in
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
- [x] T043 [US3] Container fixtures for the broker request in `tests/system_mode.rs`, and the
  security review package in `specs/005-system-mode/security-review.md` and
  `specs/004-aggressive-mode/security-review.md`. The raise also ends at the broker's start and
  stop (`ExecStopPost`), within 5 s of the key turning off or the member leaving, and is never
  set over a weight the broker did not set.

## Phase 6: Gates (this thread)

- [x] T050 Add the `first`, `first-gpu`, `batch`, `vram`, `cg-later-gpu` and `cg-first-gpu`
  shapes, the `admit` arm, the second account's `--role peer` and plain arms for every shape to
  `tools/aggressive_gates.py`, with tests in `tests/test_aggressive_gates.py`. CedeGrid against
  CedeGrid runs on GPU compute: where the host splits CPU by account, the kernel gives each
  account its share whatever the other starts, so a CPU shape would show nothing. The `ram`
  shape waits for the user's approval, since its plain arm drives the host into OOM.
- [ ] T051 Run FR-023's gates on the test host and record them in `docs/validation.md` under
  feature 006.
  - *Done in part (2026-11-18, `f13b429`, `05d5c04`):* F2c and own-first on CPU and GPU, fill,
    batch, VRAM, RAM (one plain run: its OOM also killed the account's desktop session, so the
    other two were stopped) and CedeGrid against CedeGrid, with plain arms. Open: the
    same-cgroup layout and the System mode raise (both need the user's sudo steps); the own-first
    CPU gap (start and reaction latency) and the batch's total time (handoff between tasks).

## Phase 6b: Priority over plain users (US6, maintainer decisions of 2026-11-19)

- [x] T070 Amend the constitution to 9.2.0 in `.specify/memory/constitution.md`: Principle II's
  priority rule names two exceptions, priority over plain users (install option, both the
  administrator's permission and the user's choice) and unprotected; Principle V lets the broker
  set the account slice weight for either.
- [x] T071 [US6] Test first, then `modes.plain_priority` (bool, default false) and
  `modes.plain_priority_cpu_weight` (101 to 10000, default 200) in
  `src/broker/admin_policy.rs`, `docs/system-mode.md` and
  `specs/005-system-mode/contracts/admin-policy.md`.
- [x] T072 [US6] Test first, then the broker's `raise` request with a `level` (`plain` or
  `unprotected`, default `unprotected` for compatibility) in `src/broker/request.rs`,
  `src/broker/systemd.rs` and `src/broker/server.rs`: the level's policy key and weight, one lease
  per caller at the higher level asked, and every end of FR-027. Container fixture in
  `tests/system_mode.rs`.
- [x] T073 [US6] Test first, then the user's choice `contention.priority_over_plain` (bool,
  default false) in `src/config.rs`, validated once, and the setup question with Yes preselected
  and `--priority-over-plain` / `--no-priority-over-plain` in `src/setup.rs` and
  `src/setup/interface.rs`; setup says when the host cannot apply it.
- [x] T074 [US6] Test first, then the agent's steering in `src/system_mode.rs` and
  `src/agent/service.rs`: wanted while the choice is on, own Performance or Aggressive CPU work
  runs and every contending CedeGrid principal's account slice reads at the weight or more
  (read-only, from the cgroup tree; unreadable counts as not raised); withdrawn within two
  monitor intervals; the higher of the two raises when unprotected also applies. System mode
  units count as CedeGrid work while they run work (FR-028), in `src/telemetry.rs`.
- [x] T075 [US6] `doctor` and `observe` rows for the option: chosen, permitted, in force, weight,
  reason (`src/main.rs`, `src/explain.rs` with the 001 thread).
- [ ] T076 [US6] Docs: `docs/system-mode.md`, `docs/guarantees.md` (with the 001 thread),
  `CHANGELOG.md`, both security review packages.
- [ ] T077 [US6] SC-009 gate on the test host from installed packages: F2c and own-first on CPU
  against a plain account, and against a second CedeGrid account with and without the option,
  with plain arms, three runs each; record in `docs/validation.md`.

## Phase 6c: Placement of held-back tasks (FR-007b, maintainer go of 2026-11-22)

- [x] T078 Tests first: the window rule and the CPU choice (`src/agent.rs`), the last-CPU read
  (`src/kernel.rs`), and on Linux a supervised task that starts on its CPU and is back on the
  whole scope after the release, with `cpu.placement` evidence (`tests/supervision.rs`).
- [x] T079 The agent's window, choice and bookkeeping (`src/agent/service.rs`), the spec field
  (`src/agent.rs`, `src/agent/launch.rs`, `src/agent/supervisor.rs`).
- [x] T080 The gate's placement and the supervisor's verification and release
  (`src/execution_model.rs`, `src/supervision.rs`, `src/supervision/unix.rs`), mediated children
  on the whole scope.
- [x] T081 Own work first and F2c gates on the test host, ten rounds against plain arms, and the
  batch, recorded in `docs/validation.md`; `CHANGELOG.md` and the security review packages.

## Phase 7: Docs and checks

- [ ] T060 Update `docs/guarantees.md` (with T023), the 004 and 005 docs and specs for the
  rename and the new admission, and `CHANGELOG.md`.
- [ ] T061 Full checks: fmt, clippy, the serial suite, both Python suites, npm, MSRV.

## Dependencies

T002 and T003 come first and are pushed before the 001 thread starts T020. T020 to T022 and T010
to T012 run in parallel. T032 needs T020. T040 to T042 need T031. T051 needs everything else.
