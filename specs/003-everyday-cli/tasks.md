---

description: "Task list for the everyday CLI"
---

# Tasks: Everyday CLI for a Single User and Host

**Input**: Design documents from `specs/003-everyday-cli/`

**Prerequisites**: [plan.md](plan.md), [spec.md](spec.md), [research.md](research.md),
[data-model.md](data-model.md), [contracts/](contracts/), [quickstart.md](quickstart.md)

**Tests**: required. Policy, telemetry, supervision and protocol changes start with a failing
test (constitution VI). Hardware claims rest on the native gates in `native-gates.md`.

**Organization**: by user story, in priority order. US1, US2 and US6 are P1; US3, US4 and US7
are P2; US5 is P3. Implementation starts on its own branch after 001 is merged into `dev`, and
only when the user says so.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an unfinished task).
- **[Story]**: the user story the task belongs to.

## Phase 1: Setup

- [ ] T001 Add `ratatui = "0.30"` and `crossterm = "0.29"` to `[dependencies]` in Cargo.toml, with a one-line justification comment (research R11). Confirm `cargo +1.88.0 check --locked --all-targets` and the Windows client check still pass with the full dependency tree.
- [ ] T002 [P] Write specs/003-everyday-cli/native-gates.md with exact commands, requirements and pass criteria for these gates. Any host change a gate needs requires the user's approval first.
  - G1: Full and forced Tracked side by side on one host.
  - G2: a same-cgroup contender.
  - G3: a cross-cgroup contender.
  - G4: descendants (fork, daemonize, setuid program, work outside the tree).
  - G5: overhead.
  - G6: thread-pool launch strategies.
  - G7: GPU utilization under CPU contention with and without SCHED_IDLE.
  - G8: evidence scan cost.
- [ ] T003 [P] Add config keys in src/config.rs, each under `#[serde(default, deny_unknown_fields)]` and validated in `Config::validate`; tests first in tests/configuration.rs.
  - `local.idle_exit_s`: default 600, at least 1.
  - `logs.max_bytes`: default 67108864, at least 1048576.
  - `logs.keep`: default 3, 1–100.
  - `evidence.max_threads_per_interval`: default 4096, at least 64.
  - `bottleneck.window_intervals`: default 10, 2–1000.

## Phase 2: Foundational (blocks every story)

- [ ] T004 [P] Test first in tests/task_schema.rs: exporting `src/task_schema.rs` reproduces tests/contracts/task-schema.json byte for byte, and every field has a JSON name, CLI flag, Python name and TypeScript name (SC-002). It fails before T005.
- [ ] T005 Implement src/task_schema.rs with every field in contracts/task-schema.md, including `threads` ("`auto` or absent") and `gpu` ("a device index or `auto`"), and write tests/contracts/task-schema.json.
- [ ] T006 [P] Python: test python/tests/test_task_schema.py, comparing `command_task` parameters with the fixture. Add `threads` and `gpu` to python/cedegrid/client.py.
- [ ] T007 [P] TypeScript: test npm/tests/task-schema.test.ts, comparing `CommandOptions` and `commandTask` output with the fixture. Add `threads` and `gpu` to npm/src/client.ts.
- [ ] T008 Test first in tests/run_cli.rs:
  - the socket directory is mode 0700 and owned by the user, or the agent refuses to listen;
  - a peer with another UID is refused (peer-credential check);
  - a second agent start finds the namespace lock and connects;
  - a local agent exits after `local.idle_exit_s` with no runs;
  - a cluster-mode agent serves the socket and never idles out.
- [ ] T009 Implement src/agent/local.rs per contracts/local-agent.md: socket location, peer-credential check, namespace lock and idle exit, plus the requests `run`, `ps`, `logs`, `stop`, `top` and `explain`.
- [ ] T010 [P] Test first in tests/control_level.rs, with fake cgroup and user-manager roots:
  - Full only when `systemctl --user` answers and the manager delegates `cpu` and `memory`;
  - any error gives Tracked with a one-sentence reason;
  - `CEDEGRID_TEST_FORCE_TRACKED=1` forces Tracked;
  - `contenders_share_cgroup` is `yes`, `no` or `unknown`.
- [ ] T011 Implement src/control_level.rs (research R2) and add the level and its reason to the run record and the snapshot.
- [ ] T012 [P] Test first in src/agent/logs.rs unit tests:
  - a writer faster than the disk is never blocked;
  - logs rotate at `logs.max_bytes`, keeping `logs.keep` files;
  - the latest output is always kept;
  - dropped chunks are counted.
- [ ] T013 Implement src/agent/logs.rs (research R10).

## Phase 3: User Story 1 - Run a script with one command (P1)

**Goal**: `cedegrid run` starts work through the local agent with no setup (FR-001–FR-009).

**Independent test**: quickstart scenario 1. A fresh account runs `cedegrid run -- <cmd>`, and
the run appears in `ps` within 5 s and finishes like a bare run.

- [ ] T014 [P] [US1] Test first in tests/run_cli.rs: with no state, `cedegrid run -- <cmd>` starts the agent, the run passes the same launch-barrier records as an SDK task (reserved, prepared, authorized), and the job's exit code is returned. The run appears in `ps` within 5 s (SC-001). The job is an ordinary process: the gate executes it in place as the supervisor's child, with no interposer, preload or container (FR-006).
- [ ] T015 [P] [US1] Test first in tests/run_cli.rs:
  - `run --help` lists a flag for every schema field;
  - `--from-json` uses the job JSON, and flags override its fields;
  - `--json` prints `{"run_id", "control_level", "control_reason", "effective", "log"}`.
- [ ] T016 [P] [US1] Test first in tests/run_cli.rs: the working directory, PATH, the venv or conda variables and `CUDA_VISIBLE_DEVICES` reach the job. No environment value appears in state, logs, reports or `ps --json` (FR-009).
- [ ] T017 [US1] Implement src/run.rs and src/run/client.rs:
  - parse the generated flags;
  - connect, or start `cedegrid agent --local` and wait up to 3 s; on failure show the agent's log tail and exit 4;
  - submit the task and print the run record.
- [ ] T018 [US1] Implement src/run/env.rs: pass values at launch only, and record names only.
- [ ] T019 [US1] Wire `run` into src/main.rs after the hidden dispatch, which stays first.
- [ ] T020 [P] [US1] Test first in tests/run_cli.rs:
  - closing the viewer leaves the run running, and `logs -f` reattaches;
  - Ctrl-C once drains; Ctrl-C twice force-stops verified processes only;
  - `-d` detaches;
  - `--tty` gives the job a terminal.
- [ ] T021 [US1] Implement src/run/view.rs, and `--tty` in src/agent/local.rs (FR-015, FR-016).
- [ ] T022 [P] [US1] Test first in tests/run_cli.rs: a script that imports the SDK receives resource targets, the allowance and drain under `run` exactly as an SDK task does.
- [ ] T023 [US1] Give local runs the same worker context as SDK tasks in src/agent/local.rs and src/agent/service.rs.
- [ ] T024 [P] [US1] Test first in tests/run_cli.rs: `--gpu N` uses device N if admission allows; `--gpu auto` picks the least-loaded admitted device; with no admitted device the run is refused.
- [ ] T025 [US1] Implement GPU selection in src/run.rs through the agent's admission (FR-027).

## Phase 4: User Story 2 - See and control own runs (P1)

**Goal**: `ps`, `logs`, `stop` and `top` (FR-010–FR-014).

**Independent test**: quickstart scenario 3, plus `stop` refused on another user's run.

- [ ] T026 [P] [US2] Test first in tests/run_cli.rs:
  - `ps` shows state, mode, control level, usage, step and reason, and flags, also with `--json`;
  - `logs <run> -f` follows;
  - `stop` drains, then ends through verified processes; `stop --force` skips the drain;
  - `stop` on another user's run exits 3;
  - no process outside the run is signaled (SC-006);
  - `logs --json` and `stop --json` print the contract shapes (FR-008).
- [ ] T027 [US2] Implement `ps`, `logs` and `stop` in src/run.rs and their agent requests in src/agent/local.rs.
- [ ] T028 [P] [US2] Test first in tests/run_cli.rs: `top --json` has host resources, runs, steps and reason codes; `--follow` prints one snapshot per interval; `NO_COLOR` is honored.
- [ ] T029 [US2] Implement src/top.rs with ratatui and a style module shared with the setup interface.

## Phase 5: User Story 6 - Give capacity back without privileges (P1)

**Goal**: Full and Tracked as primary paths on every launch path (FR-040–FR-055).

**Independent test**: quickstart scenarios 5 and 6, and gates G1–G4.

Ownership tests come first, in tests/tracked.rs (Linux; the verification logic takes injected
readers, so it is also tested off Linux):
- [ ] T030 [P] [US6] Test first: the pidfd is opened before the parent chain is read (order recorded through the injected opener and reader).
- [ ] T031 [P] [US6] Test first: liveness is checked through the pidfd after the reads. A process that exits between the reads never joins the verified set.
- [ ] T032 [P] [US6] Test first: a descendant whose real, effective, saved or filesystem user or group ID differs from the run's never joins. A member that changes IDs leaves the set.
- [ ] T033 [P] [US6] Test first (SC-006): every signal to a member goes through its pidfd. A member's dead pidfd yields no signal, and the actuator API has no numeric-PID path.
- [ ] T034 [P] [US6] Test first: an unrelated same-user process and work started through another daemon are never acted on. A daemonizing descendant stays in the tree.
- [ ] T035 [P] [US6] Test first: orphans are reaped through their pidfds, and their rusage is added to accounting with live sums and `cutime`/`cstime`.

Then the implementation:
- [ ] T036 [US6] Implement src/supervision/tracked.rs (research R3): `PR_SET_CHILD_SUBREAPER` before EXEC, the tree walk, member verification, reaping and accounting.
  - *Exists in part (converge, 2026-11-26):* the subreaper, tree walk, verified families and reaping are in `src/supervision/unix.rs` (001 FR-088); check them against T033–T035's tests rather than writing `tracked.rs` anew.
- [ ] T037 [US6] Implement src/supervision/full.rs: a transient scope through the user manager, with its subtree handed to the cgroup backend (research R2).
- [ ] T038 [US6] Select the level for every launch path (run, SDK tasks, cluster assignments) in src/agent/service.rs and src/agent/launch.rs:
  - the Tracked one-line notice;
  - `--require-containment` refuses (exit 3);
  - labels in `ps`, `top` and `doctor`.
- [ ] T039 [P] [US6] Test first in tests/tracked.rs, for the affinity actuator:
  - whole physical cores, SMT siblings together;
  - the GPU's NUMA-node cores kept;
  - every thread checked each interval;
  - reversible;
  - a process that changed its own mask reported, not overridden.

  Each call follows Principle I's thread-ID rule (constitution 5.0.0, research R4); its own tests are T074–T077.
- [ ] T040 [US6] Implement the affinity actuator in src/supervision/tracked.rs (research R4), with every per-thread call made through T078's thread-ID call.
  - *Scope (converge, 2026-11-26):* the lever is a launch request flag only `run` sets (001 FR-091); the actuator applies `Decision.granted_cores` and never runs for SDK or cluster tasks.
- [x] T041 [P] [US6] Test first in tests/runqueue.rs, with a fake `/proc`:
  - per-thread `run_delay` deltas are summed by UID;
  - exited threads are dropped, so no change is negative;
  - the scan is bounded by `evidence.max_threads_per_interval`, with rotation.
  - *Done (001 thread, `bdd6ec3`, 2026-11-26):* run-queue wait and allowed CPUs read from a fake `/proc` in `src/telemetry/runqueue.rs`'s tests and `tests/contention.rs`.
- [x] T042 [US6] Implement src/telemetry/runqueue.rs and add per-principal wait to the snapshot (research R7).
  - *Done (001 thread, `bdd6ec3`, 2026-11-26):* `src/telemetry/runqueue.rs`; `PrincipalUsage.cpu_wait_millicores` and `allowed_cpus` in the snapshot, bounded by `[evidence] max_threads_per_interval` (4096).
- [x] T043 [US6] Test first in tests/contention.rs, then implement in src/contention.rs: wait marks a principal as contending for the probe of 001 FR-021 and FR-072. A waiting user whose CPU use does not grow after a yield is recorded as satisfied. With hidden process details, the host-level signal goes through the same probe, with a `doctor` note.
  - *Done (001 thread, `bdd6ec3`, 2026-11-26):* wait marks a principal as contending for the probe (001 FR-091, T637).
- [ ] T044 [P] [US6] Test first in tests/tracked.rs and tests/contention.rs, for SCHED_IDLE:
  - only for Yield First with same-cgroup contenders;
  - never for GPU runs by default;
  - set before EXEC for the whole tree;
  - a mode raise needs a restart;
  - the starved flag carries its duration.

  Also: autogroup is used only when the tasks sit in the root CPU cgroup, and is off by default.
- [ ] T045 [US6] Implement SCHED_IDLE and autogroup in src/supervision/tracked.rs (research R6).
- [ ] T046 [P] [US6] Test first in tests/contention.rs: no thread count or priority is ever raised. `below_fair_share` carries the reason, such as the contender's thread count. In a shared CPU cgroup, Neutral and Performance correct their core count from measured usage (FR-051).
  - *In part (001 thread, `bdd6ec3`, 2026-11-26):* the policy half: `below_fair_share` with its reason and no raised thread count or priority, in `tests/contention.rs`; the agent half waits for the core lever.
- [ ] T047 [US6] Implement fair-share reporting and the shared-cgroup correction in src/policy/ and src/agent/service.rs (research R8).
  - *In part (001 thread, `bdd6ec3`, 2026-11-26):* the policy half: fair-share reporting in `src/policy/`; the agent side in `src/agent/service.rs` waits for the core lever.
- [ ] T048 [P] [US6] Test first in tests/tracked.rs, for Tracked RAM: no cap and no address-space limit; hints start one hint deadline earlier; a drain checkpoints and resumes when available memory clears the floor for the calm window.
- [ ] T049 [US6] Implement Tracked RAM handling (research R9).
- [ ] T050 [US6] The `granted_cores` worker event (contracts/worker-events.md): tests first in Rust, Python and TypeScript with the contract fixture, then implementation in src/actuation.rs, python/cedegrid/worker.py and npm/src/.

Native gates and docs:
- [ ] T051 [US6] Gate G1: Full and forced Tracked side by side on the same host; record the evidence in specs/003-everyday-cli/native-gates.md. Pass: Tracked gives CPU back within one monitor interval of Full and gives back at least 90% of Full's amount (SC-004), and the worker gate stays under the launch barrier with Full's transient scope.
- [ ] T052 [US6] Gate G2: a same-cgroup contender, started from the run's own session with Tracked forced (no host change). Pass criteria as T051 (SC-004).
- [ ] T053 [US6] Gate G3: a cross-cgroup contender under a second account. Creating an account is a host change that needs the user's approval. Pass criteria as T051 (SC-004).
- [ ] T054 [US6] Gate G4: descendants (fork, daemonize, setuid program, work outside the tree).
- [ ] T055 [US6] Gates G6 and G7: measure both thread-pool launch strategies and set the default; measure GPU utilization under CPU contention with and without SCHED_IDLE before changing the GPU default.
- [ ] T056 [US6] Update docs/guarantees.md with the generic description:
  - Full and Tracked; Tracked is not containment;
  - the one-core limit across cgroups;
  - the fair share is not a guarantee;
  - work outside the tree is not tracked;
  - the optional linger and delegate setup.

## Phase 6: User Story 3 - Per-job settings and explain (P2)

**Goal**: per-job entries, profiles, precedence and `--explain` (FR-020–FR-026).

**Independent test**: quickstart scenario 4.

- [ ] T057 [P] [US3] Test first in tests/run_cli.rs: lookup relative to the nearest `cedegrid.toml`, searching up to the home directory; `--` commands keyed by the first argument after the interpreter; the nearest file wins.
- [ ] T058 [P] [US3] Test first in tests/run_cli.rs:
  - precedence: flags, then per-job, then global, then defaults;
  - `--explain` names a source for every field (SC-007);
  - `-p` and `--no-setup`;
  - `--save` writes names only, and `--save-env` writes values;
  - floors, the node cap and margins are refused in entries;
  - a per-job mode above the cap runs at the cap and says so;
  - `--explain --json` and `setup --json` print the contract shapes (FR-008).
- [ ] T059 [US3] Implement src/run/scope.rs.
- [ ] T060 [US3] Implement `cedegrid setup <script>` with a per-job scope in src/setup.rs and src/setup/interface.rs, showing floors, the cap and margins read-only, with comment-preserving writes.

## Phase 7: User Story 4 - A clearer setup interface (P2)

**Goal**: grouped sections, a side pane and in-place values (FR-030–FR-033).

**Independent test**: setup on a terminal, with `NO_COLOR`, and non-interactively.

- [X] T061 [P] [US4] Test first in tests/cli.rs, rendering to ratatui's test backend:
  - the header, the three sections, and the side pane with this host's numbers;
  - values change in place with the left and right keys (FR-031);
  - caution marking and the separate confirmation;
  - `NO_COLOR` and non-terminal behavior unchanged.
- [X] T062 [US4] Reimplement src/setup/interface.rs with ratatui and crossterm, sharing its style module with src/top.rs (the style is `src/tui_style.rs`; the pty tests replay the screen onto a grid).

## Phase 8: User Story 7 - Single-core bottlenecks (P2)

**Goal**: detection, `--threads auto` and the core-count event (FR-060–FR-062).

**Independent test**: quickstart scenario 8.

- [ ] T063 [P] [US7] Test first in tests/run_cli.rs, with fake per-thread usage: a bottleneck is flagged when one thread stays at 90% or more of a core while less than half of the other granted cores are used, for `bottleneck.window_intervals`. Causes are named, the flag appears within three monitor intervals after the window (SC-008), and the end-of-run summary shows it.
- [ ] T064 [US7] Implement detection in src/telemetry/managed.rs, and show it in `ps`, `top` and the summary (research R13).
- [ ] T065 [P] [US7] Test first in tests/run_cli.rs: `--threads auto` sets `OMP_NUM_THREADS`, `MKL_NUM_THREADS` and `OPENBLAS_NUM_THREADS` to the granted core count. It overrides inherited values only, and `--explain` shows the source `threads_auto`. The SDK field matches.
- [ ] T066 [US7] Implement `--threads auto` in src/run/scope.rs and src/run/env.rs, and the separate-tasks hint in the bottleneck message (FR-062).

## Phase 9: User Story 5 - Install, first run and doctor (P3)

**Goal**: a working first run and a readable `doctor` (FR-070, FR-071).

**Independent test**: a clean account and `doctor` with and without delegation.

- [X] T067 [P] [US5] Test first in tests/cli.rs: `doctor` prints ✓/✗ per check with a fix hint, the control level with its reason, and evidence coverage; it falls back to `[ok]`/`[fail]` under `NO_COLOR`, and supports `--json` (SC-010).
- [X] T068 [US5] Implement the checks in src/explain.rs.
  - *The control level line joins when T011's `control_level::detect_now()` lands: `CheckInputs.level`.*
- [ ] T069 [US5] Write the install and first-run guide in README.md and docs/, with generic text only, including when to submit many independent pieces of work as separate tasks (FR-062).

## Phase 10: Polish and cross-cutting

- [ ] T070 Gate G5: overhead, median of five bare and five `run` passes for CPU and GPU benchmarks (SC-003), and Tracked against Full (SC-005).
- [ ] T071 Gate G8: the evidence scan cost with 10,000 threads (SC-009).
- [ ] T072 [P] Add every new file to the ALLOWLIST in tools/pack_source_review.py, update the CLAUDE.md layout map and docs/validation.md rows, and run the export check.
- [ ] T073 Full verification: fmt, clippy, the 1.88 check, the serial suite, Python, npm and the Docker Linux check.

## Phase 11: Convergence (item 019: thread-ID calls and per-core contention)

**Purpose**: Principle I's thread-ID rule (constitution 5.0.0) for the affinity lever, and
per-core contention. Tests first; T039 and T040 build on T074–T078.

- [ ] T074 [P] [US6] Test first in tests/tracked.rs (`thread_id_call_*`), with an injected thread-identity source: when the member's pidfd shows it dead before the call, the affinity call is skipped and nothing is recorded as applied. Per FR-043 and Constitution I (missing)
- [ ] T075 [P] [US6] Test first in tests/tracked.rs (`thread_id_call_*`): a thread that exits between the pre-check and the post-check (its thread pidfd dead, or its task entry gone or with a different start time) is reported as possibly misdirected, added to `misdirected_tids`, and never touched again. Per FR-043 and Constitution I (missing)
- [ ] T076 [P] [US6] Test first in tests/tracked.rs (`thread_id_call_*`), with a fake `/proc`: a TID that the member's `/proc/<pid>/task` does not list, such as one that now belongs to another thread group, is never touched. Per FR-043 and Constitution I (missing)
- [ ] T077 [P] [US6] Test first in tests/tracked.rs (`thread_id_call_*`): with thread pidfds unavailable (or `CEDEGRID_TEST_FORCE_PROC_THREADS=1`), the `/proc/<pid>/task/<tid>` start-time path skips, reports and refuses exactly as T074–T076 require, and `thread_identity` records `proc_start_time`. Per FR-043 (missing)
- [ ] T078 [US6] Implement the thread-ID call in src/supervision/tracked.rs (research R4): detect thread pidfd support (`pidfd_open` with `PIDFD_THREAD`) once per agent start; check the member's pidfd before the call; fix the thread's identity (thread pidfd, else task entry and start time); check the same thread after the call; keep `misdirected_tids` per member; honor `CEDEGRID_TEST_FORCE_PROC_THREADS` in tests and gates only. T040 makes every per-thread affinity change through it. Per FR-043, plan: CPU lever (missing)
  - *Exists (converge, 2026-11-26):* the thread-ID call with its identity checks before and after (`ThreadIdentity`, per-thread `setpriority`) was built for 006's nice raise in `src/supervision/unix.rs`; the affinity actuator reuses it.
- [ ] T079 [P] [US6] Test first in tests/runqueue.rs, with a fake `/proc`: each contending process's `Cpus_allowed_list` is read read-only into the principal's `allowed_cpus`; unreadable entries leave it unknown, not empty. Per FR-056 and research R15 (missing)
- [ ] T080 [P] [US6] Test first in tests/contention.rs (`pinned_contender_*`): a principal whose wait stays at the contending level (T043) for the probe's response window and whose `allowed_cpus` overlap own work's granted cores, or are unknown, is not recorded as satisfied, under the saturation probe and under the PSI-only probe; at the Tracked level own work's requested granted cores drop a known overlap but never fall below its fair share in cores, and an unknown overlap moves no core; at the Full level no core request is made; with no overlap the 001 behavior is unchanged. Per FR-056 and 001 FR-021, FR-072 (missing)
  - *In part (001 thread, `bdd6ec3`, 2026-11-26):* the policy half: a contender pinned to own CPUs is never counted satisfied (`pinned_contender_*`).
- [ ] T081 [US6] Implement per-core contention: `allowed_cpus` and `pinned_overlap` in src/telemetry/runqueue.rs, the satisfied gate in src/contention.rs's probe (beside T043), and the core request to the affinity actuator (T040, T078). Per FR-056 and plan: per-core contention (missing)
  - *In part (001 thread, `bdd6ec3`, 2026-11-26):* the policy half: `allowed_cpus` and the satisfied gate; the core request through the actuator is the agent's, with the core lever.
- [ ] T082 [P] [US6] Test first, then implement: `ps` and `ps --json` show `possibly_misdirected` with the thread IDs, and the run log records each one once. Per FR-043, data-model Run.flags, contracts/cli.md (missing)
- [ ] T083 [US6] Add the "Pinned contender" and "Thread-ID calls" gates to specs/003-everyday-cli/native-gates.md with exact commands and pass criteria (plan: native gates). Per FR-056, FR-043 (missing)
- [ ] T084 [US6] Gate, at the Tracked level: a contender pinned with `taskset` to two of own work's cores reaches its demand after own work moves off them and is never recorded as satisfied while it waits; record the evidence in native-gates.md. Per FR-056, SC-006 (missing)
- [ ] T085 [US6] Gate: on a kernel with thread pidfds and again with `CEDEGRID_TEST_FORCE_PROC_THREADS=1`, a Tracked run's affinity changes reach only verified threads, and no change is reported as possibly misdirected in a normal run; record the evidence in native-gates.md. Per FR-043, SC-006 (missing)

## Phase 12: Item 048 (root refusal, install channel and contended-CPU placement)

Spec Session 2026-11-02 (FR-057, FR-073, FR-082; SC-011 to SC-013); plan "Item 048". Each pair: a
failing test first, then the code.

**Independent test**: quickstart scenarios 10, 11 and 12, and the two gates below.

Root refusal (FR-082, SC-011):
- [ ] T086 [P] [US5] Test first in tests/cli.rs (`root_refusal_*`) and a unit test of the refusal function in src/main.rs: at effective UID 0 every subcommand, the hidden `__worker-gate` and `__assignment-supervisor` included, prints one stderr line saying to run the same command as the user without `sudo` and exits 3; with `--json` it prints `{"error": {"code": "ERR_CEDEGRID_ROOT", ...}}`; at any other UID nothing changes. The refusal is a pure function of the effective UID. Per FR-082, SC-011 (missing)
  - *In part (converge, 2026-11-26):* `system_mode::root_refusal` refuses at effective UID 0 from `main`, before the hidden dispatch, tested in `tests/system_mode.rs`; the `--json` error object and exit 3 of contracts/cli.md remain.
- [ ] T087 [US5] Implement one `geteuid` read at the top of `main` in src/main.rs, before the hidden dispatch, which still runs before clap, Tokio, any thread and the launcher terminal handoff, and add `ERR_CEDEGRID_ROOT` to the CLI's error codes, until T086 passes. Per FR-082, research R18 (missing)
  - *In part (converge, 2026-11-26):* see T086.
- [ ] T088 [P] [US5] Test first in tests/control_level.rs (`control_level_ignores_sudo`): detection gives the same level with and without `SUDO_UID`, `SUDO_USER` and `SUDO_COMMAND` set and for any install path, and its recorded inputs hold none of them. Per FR-082 (missing)
- [ ] T089 [US5] Make src/control_level.rs (T011) take only the user manager, delegation and the authorized controls, until T088 passes. Per FR-082 (missing)
- [ ] T090 [US5] Add a step to .github/workflows/ci.yml on the Ubuntu runner: `sudo -n target/debug/cedegrid <command>` for every subcommand, expecting the one line and exit 3. Per SC-011 (missing)
- [X] T091 [US5] Extend T067's test in tests/cli.rs with `✓ runs as a user other than root`, then add the check in src/explain.rs. Per FR-071, FR-082 (missing)

Install channel (FR-073, SC-012):
- [X] T092 [P] [US5] Test first in tests/test_install_script.py (standard library only): tools/install.sh against local release assets served on loopback, with `HOME` set to an empty temporary directory. It installs `~/.local/bin/cedegrid` at the pinned version. A changed tarball byte, a tarball with no `SHA256SUMS` line, or no `sha256sum` or `shasum` on `PATH` stops it with nothing installed. With an `id -u` stub that prints 0 it refuses. It says when `~/.local/bin` is not on `PATH`. Per FR-073, SC-012 (missing)
- [X] T093 [US5] Write tools/install.sh (POSIX `sh`, research R16) until T092 passes, and add it to `ALLOWLIST` in tools/pack_source_review.py. Per FR-073 (missing)
- [X] T094 [US5] In .github/workflows/release.yml and release-candidate.yml, build Linux x86_64 and aarch64 binaries at the glibc 2.35 floor and publish `cedegrid-<version>-<target>.tar.gz`, `SHA256SUMS` and `install.sh` with the version written in; the candidate checks the sums before publishing. Per FR-073 (missing)
- [X] T095 [P] [US5] Test first in tests/test_packaging.py: the wheel built from python/ holds the SDK only, with no `cedegrid` binary or launcher. Per FR-073 (missing)
- [X] T096 [US5] Lead README.md's install section with the one-line script. Homebrew and `cargo install --locked cedegrid` follow as secondary channels, then the npm launcher packages with a line that they keep working. Update T069's guide to match. Per FR-073, FR-080 (missing)
- [ ] T097 [US5] Add "Install on a clean account" to specs/003-everyday-cli/native-gates.md with exact commands: an empty `HOME` under an existing account (a new account only with the user's approval, FR-053), the one-line install, the checksum stop with a changed tarball, and a first `cedegrid run`. Per SC-012 (missing)
- [ ] T098 [US5] Gate: run "Install on a clean account" on the test host and record the evidence in native-gates.md and docs/validation.md. Per SC-012 (missing)

Contended-CPU placement (FR-057, SC-013):
- [ ] T099 [P] [US6] Test first in tests/telemetry.rs (`contended_cpus_*`): the contended set is per-CPU host busy from `/proc/stat` less own verified threads' per-CPU use, joined with contenders' `Cpus_allowed_list`; unknown per-CPU use counts as contended. Per FR-057, Principle III (missing)
- [ ] T100 [P] [US6] Test first in tests/contention.rs (`contended_cpu_placement_*`): when the ladder would cap own work on a contended CPU, the decision first moves own work's granted cores off it (Tracked), or lowers own leaves' `cpu.weight`, never above 100 (Full, where authorized and the contender shares the delegated parent). The duty cycle applies only when own usage still exceeds its target in the next interval, and own work never goes below its fair share in cores. Per FR-057 (missing)
- [ ] T101 [P] [US6] Test first in tests/contention.rs: with placement, a contender that switches on and off every 4 intervals (001 SC-009's shape) changes own work's CPU step at most once per switch, and so does report 029 run 4's shape (two of four CPUs saturated 30 s on and 30 s off). Per FR-057 (missing)
- [ ] T102 [US6] Implement the contended set in src/telemetry/runqueue.rs and `contended_cpus` in src/model.rs, until T099 passes. Per FR-057 (missing)
- [x] T103 [US6] Implement placement in src/policy/ladder.rs and src/contention.rs, with the record fields `placement` (`none` \| `moved_off` \| `weight`) and `duty_after_placement`, applied through the Tracked affinity lever (src/supervision/tracked.rs, T078) and `cpu.weight` in src/cgroup.rs, until T100 and T101 pass. Per FR-057 (missing)
  - *Done (001 thread, `bdd6ec3`, 2026-11-26):* re-scoped with 001: a run with a core lever (`AllocationFacts.cpu_cores`) is granted whole cores (`Decision.granted_cores`, reason `cpu_cores_granted`) instead of being paused; fewer cores apply at once, more wait the scale-up interval, unknown keeps the last grant, Performance and Aggressive get every core.
- [ ] T104 [US6] Add "Contended-CPU placement" to specs/003-everyday-cli/native-gates.md with exact commands: report 029 run 4's shape beside CedeGrid and beside a fixed `taskset` allocation of the same own share, in the same run, recording the contender's p50 and p99 and own work's step changes. It passes when the contender's p99 beside Tracked caps is at most 1.2× its p99 beside the fixed allocation. Per SC-013 (missing)
- [ ] T105 [US6] Gate: run "Contended-CPU placement" on the test host and record the p99 ratio against the 1.2× bound and the step changes in native-gates.md and docs/validation.md. Per SC-013 (missing)

- [ ] T107 [US5] Add the secondary channels: a Homebrew formula kept in packaging/homebrew/cedegrid.rb that installs the release tarball by its sha256, and a crates.io publish step in .github/workflows/release.yml for `cargo install --locked cedegrid`. Both publish only on the user's release go. Per FR-073 (missing)
- [ ] T106 Update docs/guarantees.md, README.md, the CLAUDE.md layout map, `ALLOWLIST` and Cargo `include` for FR-057, FR-073 and FR-082. Per Constitution VIII (missing)

## Dependencies and execution order

- Setup (T001–T003), then Foundational (T004–T013), then the user stories.
- US1 needs T004–T009, T012 and T013. US2 needs US1's run path (T017).
- US6 needs T010 and T011 and the supervisor path. T039 and T040 need T074–T078, the thread-ID
  rule of constitution 5.0.0. T081 (per-core contention) needs T042, T043 and T078.
- US3 needs T005 and T017. US4 needs T001. US7 needs T017 and T050. US5 needs T011.
- Polish follows the stories it measures.
- Phase 12: T087 needs T086 and T089 needs T011 and T088. T091 needs T067 and T068, and T096
  needs T069. T094 needs T093, and T098 needs T094 and T097. T102 needs T042 and T081, and T103
  needs T040, T078, T100 and T101. T105 needs T103 and T104. T107 needs T094. T106 comes last.

## Parallel examples

- Foundational: T004, T006, T007, T010 and T012 together; then T005, T009, T011 and T013.
- US1: T014, T015, T016, T020, T022 and T024 (all tests) together, then the implementations.
- US6: T030–T035 together, then T036; T041, T044, T046 and T048 together.

## Implementation strategy

1. MVP: Setup, Foundational and US1 give `cedegrid run` on the existing supervisor path.
2. Then US2 (see and stop), then US6 (Full and Tracked with gates), each tested on its own.
3. Then US3, US4 and US7, and finally US5.

## Backlog (notes, not tasks)

- `cedegrid state purge`: remove a stopped node's or coordinator's state directory together with the
  control directory beside it (`.<name>.cedegrid-control`). Until then `docs/guarantees.md` names
  the control directory, and removing state means removing both (feature 001, T418).

## Phase 13: Convergence (item 048)

Item 048's converge against constitution 7.0.0 and `specs/001-contention-modes/follow-ups.md`. Spec changes go through `/speckit-clarify` first; each code task starts with a failing test.

- [x] T108 CRITICAL: Bring FR-052 and research R9 in line with Principle II's OOM order by fault: a Tracked member within own memory's fair share keeps the `oom_score_adj` it launched with, and only the members that make up own memory above its fair share are marked as preferred victims, as 001 FR-023 does; then a failing test beside T048 in tests/tracked.rs. Per Constitution II, FR-052 (contradicts)
  - *Done (converge, 2026-11-26):* FR-052 and research R9 follow Principle II's OOM order and floors by mode.
- [x] T109 Make Performance `run`'s default mode in FR-025 and US1 scenario 1 (now "Neutral"), and make T014 and T058 assert it. Per Constitution II, 001 FR-030 (contradicts)
  - *Done in the spec (converge, 2026-11-26):* FR-025 and US1 scenario 1 say Performance; T014 and T058 assert it when written.
- [x] T110 Make US6 scenario 6 and FR-052 depend on the mode and the memory fair share, as 001's ladder does: in Performance and Aggressive own memory within its fair share is not given up for a floor breach others caused; add that case to T048. Per Constitution II (contradicts)
  - *Done (converge, 2026-11-26):* FR-052, FR-051, FR-056 and FR-057 depend on the mode: Performance and Aggressive keep every core and get no floor hint or drain.
- [x] T111 Re-run plan.md's Constitution Check against 7.0.0 (row II: the OOM order and the memory fair share at the floors), and update the version references in spec.md, research.md R3 and R4, and T039. Per Constitution II, plan: Constitution Check (contradicts)
  - *Done (converge, 2026-11-26):* plan.md's Constitution Check re-checked against 9.2.0; the spec cites 9.2.0.
- [ ] T112 Plan the follow-up "Full moves cores too": whether Full verifies its leaf members per thread through `cgroup.procs` plus a pidfd and takes the core move, through `/speckit-clarify` (FR-056, R15, R17, T080, T081 say Full makes no per-thread call). Per follow-ups "019 analyze C1" (partial)
- [x] T113 Set and validate the numeric default of the wait level that marks a principal as contending (T003, T043; src/config.rs, research R7), test first in tests/contention.rs. Per FR-050, FR-056, follow-ups "019 analyze A1" (missing)
  - *Done (001 thread, `bdd6ec3`, 2026-11-26):* the contending wait level's default is set and validated in `src/config.rs`, with its test in `tests/contention.rs`.
- [ ] T114 Add a Full-level same-cgroup contender case to T052 and native-gates.md, or record in plan.md why Full cannot have one and amend FR-053. Per FR-053 (partial)
- [ ] T115 Add a failing test in tests/run_cli.rs before T038 for the one-line Tracked notice and the `--require-containment` refusal (exit 3). Per FR-040, US6 scenarios 2 and 3 (partial)
- [ ] T116 Add to T058 (tests/run_cli.rs) the `[run."<script>"]` write, the kept comments, the read-only floors and the one-line notice naming the file. Per FR-020, FR-021, FR-026, US3 scenarios 1, 2 and 5 (partial)
- [ ] T117 Add to T039 (tests/tracked.rs) a case showing the Tracked actuator never grants fewer than one core under cross-cgroup contention. Per FR-048, edge case "Every core is granted away except one" (partial)

## Phase 14: Convergence (item 058, no distribution through npm or PyPI)

Recorded, not to be done now: each task waits for the user's go-ahead and for the install script (FR-073). Nothing is deprecated or removed on a registry without the user's word.

- [X] T118 Remove the npm launcher: `npm/src/launcher.ts`, `src/launcher_terminal.rs` and its use in `src/main.rs`, the `CEDEGRID_LAUNCHER_PID` removal in `src/supervision/unix.rs`, and `tests/cli.rs`'s `npm_launcher_identity_does_not_reach_internal_gate_or_supervisor`; and the native npm packages, per FR-073, R19 (unrequested)
- [X] T119 Remove the registry publishing (`tools/publish_packages.py`, `tools/publish_github_packages.py`, the registry steps in `.github/workflows/release.yml`, `.github/workflows/github-packages.yml`) and the gates that exist only for it (`tools/verify_launcher_pty.py`; the registry-only parts of `tools/build_packages.py`, `tools/verify_package_interop.py`, `tools/verify_python_packages.py`, `npm/scripts/build.ts` and `npm/scripts/verify-packages.ts`; the 7 registry tests in `tests/test_release_gate.py`), keeping everything that builds and checks the release binaries and the SDK files, per FR-073, R19 (unrequested)
- [X] T120 Update `docs/release.md`, `docs/release-0.2-gates.json`, `CLAUDE.md` and `tools/pack_source_review.py`'s `ALLOWLIST` for the removal, per FR-073 (partial)
- [X] T121 Write the migration note for 0.2.0 on the registries and for CLIs installed through npm, per FR-073, Clarifications Session 2026-11-14 (missing)
- [X] T122 Once the user confirms how an SDK is installed without a registry, provide it (the reviewer's recommendation: a wheel and an npm tarball attached to the GitHub Release, each with its sha256), per Clarifications Session 2026-11-14 (missing)
