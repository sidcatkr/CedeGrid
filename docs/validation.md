# Validation

This page summarizes what automated checks cover, which operational gates remain open,
and the rules for validation runs. The qualification status of the current release is
recorded in the [release record](release-0.2-gates.json) and the
[release evidence](release-0.2-evidence.md).

## Automated checks

Continuous integration runs on every push and pull request:

- Linux x86_64 and arm64, macOS arm64 and x86_64:
  - formatting, Clippy with warnings denied and the serial Rust test suite
  - the Python SDK and tool tests
  - `doctor` and `observe` smoke runs
- Windows: client-path compilation, plus the `policy`, `telemetry` and `cli` tests.
- Rust 1.88: all-target check.
- SDKs: Python 3.10–3.14 and Node 22.14 and later.

Passing on macOS, or cross-compiling, does not establish Linux runtime behavior.

## Requirement and test matrix

| Requirement | Implementation | Tests |
|---|---|---|
| Versioned config, configurable deadlines, execution opt-in and storage/GPU modes | `src/config.rs`, `src/main.rs` | `tests/policy.rs`, `tests/cli.rs` |
| SQLite storage profiles, schema fence, private state and filesystem gate | `src/state.rs`, `src/storage_qualification.rs` | `tests/state.rs`, `tests/storage_qualification.rs`, `tests/backup.rs` |
| Matching CPU/RAM scopes and per-device GPU capability and freshness | `src/kernel.rs`, `src/telemetry.rs`, `src/policy.rs` | `tests/kernel.rs`, `tests/telemetry.rs`, `tests/policy.rs` |
| Scoped PSI and capability/enforcement reporting | `src/kernel.rs`, `src/cgroup.rs` | `tests/kernel.rs`, `tests/cgroup.rs` |
| Durable launch barrier, verified identity, stable signaling and reaping | `src/supervision.rs`, `src/supervision/unix.rs`, `src/rootless.rs`, `src/execution_state.rs` | `tests/supervision.rs`, `tests/agent.rs` |
| Mediated children, common deadlines, no signals to unrelated processes | `src/child_supervision.rs`, `src/managed_children.rs`, `python/cedegrid/process.py` | `tests/managed_supervision.rs`, `tests/managed_children.rs`, `tests/family_contract.rs` |
| Pending, live and uncertain resource accounting | `src/execution_state.rs`, `src/coordinator.rs` | `tests/execution_state.rs`, `tests/coordinator.rs` |
| One coordinator, versioned mTLS, operator/node role isolation | `src/coordinator.rs`, `src/protocol.rs` | `tests/coordinator.rs`, `tests/tls_transport.rs` |
| Priority/FIFO, pool bounds and minimum protection, cancellation | `src/coordinator.rs`, `src/agent/service.rs` | `tests/coordinator.rs`, `tests/agent.rs` |
| Yielding, gradual growth, CPU-only admission despite GPU uncertainty | `src/policy.rs`, `src/agent/service.rs` | `tests/policy.rs`, `tests/coordinator.rs`, `tests/agent.rs` |
| Distinct disconnect, agent and supervisor failure; profile-specific recovery | `src/agent/reconcile.rs`, `src/supervision/unix.rs` | `tests/agent.rs`, `tests/agent_reconciliation.rs`, `tests/distributed_service.rs` |
| Attempt fencing, bounded retries and yield backoff, idempotent acceptance | `src/coordinator.rs`, `src/state.rs` | `tests/coordinator.rs`, `tests/state.rs` |
| Checksummed uploads, atomic publication, checkpoints and results | `src/artifacts.rs`, `src/coordinator.rs` | `tests/artifacts.rs`, `tests/coordinator.rs` |
| Python worker lifecycle, drain, resume, completion and spool cleanup | `python/cedegrid/worker.py`, `python/cedegrid/client.py`, `src/agent/launch.rs`, `src/agent/publishing.rs` | `python/tests`, `tests/agent.rs`, `tests/tls_transport.rs` |
| CLI lifecycle commands, storage diagnostic, offline backup and restore | `src/main.rs`, `src/backup.rs` | `tests/cli.rs`, `tests/backup.rs`, `tests/storage_qualification.rs` |
| Generic command example | `python/examples/counter.py` | `tests/agent.rs` |
| Matched throughput, protected latency, manager overhead and release timing | `tools/compare.py`, `tools/pressure_comparison.py`, `tools/pressure_probe.py` | `tests/test_compare.py`, `tests/test_pressure_comparison.py` |
| Performance ramp time and a contender started mid-ramp | `tools/ramp_gate.py` | `tests/test_ramp_gate.py`, `tests/contention.rs` |
| Bounded stress and optional long-duration soak | `tools/pressure.py`, `tools/soak.py`, `tools/make_soak_config.py`, `tools/pressure_schedule.py` | `tests/test_local_smoke_stress.py`, `tests/test_operations.py`, `tests/test_validation_tools.py` |

## Open operational gates

Each gate below is open. It says what is not yet shown and which requirement or release gate it
blocks. Measured results are in [Native gate results](#native-gate-results-feature-001).

- **Multiple physical nodes.** Not yet verified on physical hosts:
  - useful work from two nodes
  - loss and rejoin of a node
  - checkpoint transfer between nodes

  This blocks the physical multi-host gate of the [release record](release-0.2-gates.json).
- **GPU devices.** Verified on one consumer device only, with no MPS or MIG. Other devices and
  drivers are not verified, so SC-006 and SC-017 hold for that device only. The GPU attribution
  tolerance (FR-007) stays 15 points until a second device is measured; the first device's
  largest gap was 5. Full's leaf freeze on a time-sliced device is measured in Performance
  beside a saturating user only. A zero GPU compute margin, Aggressive's GPU behavior, leaves
  the saturating user 0.3–6.5 requests a second short of half its rate alone; for feature 001
  that is a record, not a gate.

  Freshness is never inferred from aggregate utilization or from a driver `NotFound` response.
- **Memory.**
  - No native result exists for SC-012's GPU memory reclaim.
  - The kernel's preference for the marked task is shown by `oom_score` only (1343–1385 against
    the allocator's 746–800 at each run's first marked sample): no own task was killed, since
    CedeGrid gave the excess back first each time. FR-023's out-of-memory order by fault is not
    passed natively.
  - An in-share own task killed by an OOM that others caused has not happened natively either.
    Its classification as not own fault and its requeue without an attempt (FR-076) are covered
    by tests only: `a_kill_in_an_interval_where_oom_kill_rose_is_an_oom_kill`
    (`src/agent/oom.rs`), `an_oom_kill_own_work_did_not_cause_requeues_without_an_attempt` and
    `uncounted_oom_requeues_stop_at_three_times_max_attempts` (`tests/coordinator.rs`).
- **An over-share task that ignores its hint while its need pauses and returns.** A held hint's
  deadline starts again each time the need returns. So a task that ignores its hint is not
  drained while its need pauses and returns within each deadline, and nobody else is asked for
  that excess meanwhile. This stays open until the memory ladder's design settles one deadline
  per breach (FR-037).
- **Hosts.** CedeGrid has run natively on one test host only, with one consumer GPU and no MPS or
  MIG. It has never run on the shared host.
- **Launch bursts without lease batches.** A node renews each running assignment's lease every
  heartbeat (2 s by default). Each renewal is a synced coordinator write, about 9 ms on the test
  host. Against a coordinator without `lease_batch/1` (FR-032), renewals share half of each
  monitor interval, so one node keeps about 117 leases renewed. Both figures are derived from a
  loop profile and the heartbeat period; no kept evidence states them. Where synced writes cost
  50–90 ms (a container, macOS) the limit is lower. With `lease_batch/1` (FR-074) the 150-task
  burst passes.
- **Storage.** A FUSE-backed filesystem without a supported directory-sync barrier is not
  qualified for durable namespace publication. The replayable profile tolerates process crashes
  only.
- **Endurance.** A long-duration soak has not been run. Bounded stress is the release
  requirement.

## Native gate results (feature 001)

These results come from one authorized Linux host, the test host, with the commands in
`specs/001-contention-modes/native-gates.md`. They hold for this host only. Each row is the
current result of its gate: the requirement it measures, the commit and date of the run, the
figures and the result. A later run of a gate replaces the earlier one; git history keeps the
earlier ones. Loads of "the other user" ran under a second unprivileged account, the second
account, which started and stopped them; CedeGrid took no action on them. Dates are UTC.

| Host role | Kernel | CPUs | RAM | GPU | Driver |
|---|---|---|---|---|---|
| test host | Linux 7.0.0 (Ubuntu 26.04), cgroup v2 with PSI, `cpu memory pids` delegated to the user manager | 20 logical (Intel Core i5-13500), 1 NUMA node | 31 GiB, 8 GiB swap | NVIDIA GeForce GTX 1060 6 GB | 580.178.04, CUDA 13.0 |

Every GPU row below ran on that GTX 1060. By 2026-11-17 the test host's GPU had been replaced with
an NVIDIA GeForce RTX 2070 8 GB on the same driver; no GPU gate has run on it yet.

Rows measured before `9916e59` read principal usage from a refresh that listed every thread as a
process of its own. A process's CPU counted once for the process and again for each thread, its
resident memory once per thread, and own tasks' threads, not in the managed set, as the owner's
unmanaged usage. Since `9916e59` each process counts once. The contender and SC-015 rows at
`07c2d8b` and `770651f` ran with the old count.

### Controls and pressure

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| Delegated cgroup steps | FR-029 | `4d631d0`, `893c1c2` | 2026-10-26 | `cpu.weight`, `cpu.max` and `cgroup.freeze` written, read back and restored; with a foreign test-owned process in the leaf, LowerShare is refused and Cap falls back to the rootless duty cycle ("cgroup write refused"), leaving the leaf unchanged | pass |
| RAM cap | FR-011 | `4d631d0`, `893c1c2` | 2026-10-26 | `memory.high` written, read back and restored to `max`; swap is on, so the swap-off `memory.stat` check does not apply | pass |
| CPU pressure, own session | FR-072 | `4d631d0`, `893c1c2` | 2026-10-26 | status available, 10 external cgroups, external avg10 0; 40 busy loops on 20 CPUs in own session: own avg10 up to 67.7, external 0, busy 20,000 of 20,000 m, no `saturated_by_psi` in 30 samples | pass |
| CPU pressure under another account | FR-072 | `07c2d8b` | 2026-10-27 | eight busy loops of the second account on 2 CPUs: external some avg10 67.9–92.0 in all 30 samples, `saturated_by_psi` on CPU in all 30, busy 10.1–11.2% (`saturation_percent` 98) | pass |

### CPU

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| Performance ramp, a contender pinned to a scope CPU (4 CPUs, 16 tasks of 250 m) | SC-004 | `1c6ca7e` | 2026-10-30 | `ramp_seconds` 2.668 (target 10 s); the second account's one-core contender served 0.51 s after it started; `foreign_actions` 0 | pass |
| Performance ramp, the contender unpinned | SC-004 | `07c2d8b` | 2026-10-27 | `ramp_seconds` 1.173; the contender reaches 0.9 core in 2.0 s, with a median of 0.92 core after 10 s and 0.9 core or more in 75% of half-second samples; `foreign_actions` 0 | pass |
| The contender under the account that runs CedeGrid | SC-002 | `770651f` | 2026-10-26 | a median of 0.70 core after 10 s pinned and 0.62 unpinned, 0.9 core in 2% and 0% of half-second samples; that account's other programs share its fair share, as SC-002 allows | as specified |
| Performance ramp on GPU compute (1 task) | SC-004 | `770651f` | 2026-10-26 | own GPU compute reaches 90% 2.6 s after submission once the deployment's 30 s GPU cooldown has passed; submitted earlier, it waits for that cooldown (22.6 s) | pass |
| CPU reclaim in Yield First, Neutral and Performance (protection profile, `reserve_physical_cores = 1`) | SC-012 | `1c8a850` | 2026-10-26 | matched throughput 0.933, 0.942, 0.933; decision p95 0.81, 0.78, 0.77 s; post-yield p99 1.05, 1.03, 1.07×; own usage at the target share 2 intervals (1.0 s) after the demand is first observed in Yield First and Neutral, 1–2 in Performance (not in the kept evidence, which has no per-interval timing); one cap and no drain per case; manager peak 0.14–0.19 core and 94 MiB | pass |
| No action outside own work, CPU and GPU, at the defaults and with Performance and a 512 MiB floor (protection profile) | SC-005 | `1c8a850` (CPU), `c0779d3` (GPU) | 2026-10-26 | 0 actions and 0 drains outside own allocations, protected snapshots unchanged, every protected run completed, no failed external GPU allocation, every owned GPU release confirmed, 0 OOM lines | pass |
| Return after the second account's 30 s one-core load, Yield First (4 CPUs, 16 steady tasks of 250 m) | SC-002 | `0da6118` | 2026-11-13 | 13 tasks admitted; own work at the contended target in the interval that first shows the load; no drain; 273 pause samples; the target back 1.5 s and own work 3.01 s after the load stopped, so the harness's 3 s return check read false; in the 22 intervals in which all 13 tasks were capped, own usage was 1.002 of the caps' sum on average and at most 1.069 | SC-002 pass |
| Return after a 30 s one-core contender, Performance (4 CPUs) | SC-002, FR-034 | `cd077e9` | 2026-10-31 | the target and own usage back 1.0 s after the contender's last usage; no pause | pass |
| Idle fill, Performance (14 CPUs, 100 m tasks, the steady loop, no login during the run) | SC-014 | `743312b` | 2026-11-03 | 128 tasks, all admitted after 14 s; own tasks 96.9% of capacity, 97.7% with CedeGrid's 111 m; no step | pass (97%) |
| Idle fill, Neutral | SC-014 | `743312b` | 2026-11-03 | 123 tasks, all admitted after 24 min; own tasks 93.8% of capacity, 94.6% with CedeGrid's 108 m; `charge_shortfall` 571 m at the median; no step | pass (94%) |
| Idle fill, Yield First | SC-014 | `743312b` | 2026-11-03 | 110 tasks, the last admitted after 59 min; own tasks 83.5% of capacity, 84.2% with CedeGrid's 94 m; `charge_shortfall` 473 m; no step | pass (84%) |
| Idle fill, Yield First, with a login from each account every 60 s through the warm-up and the window | SC-014, FR-034 | `743312b` | 2026-11-12 | 73 login rounds, none failed, longest gap 62 s. 111 tasks, the last admitted after 62 min; own tasks 84.4% of capacity, 85.1% with CedeGrid's 103 m; `charge_shortfall` 533 m. One login burst read 1363 m external for one interval, 6.8 s before the window: all 111 tasks were capped at 93 m, and the caps were relaxed in steps and gone 19 s into the window. No pause; the later logins read 260–290 m and caused no step | pass (84%) |
| Idle fill, Performance, 4 CPUs, 100 m tasks | SC-014 | `9916e59` | 2026-10-29 | 36 tasks, own tasks 96.6% of capacity, 97.1% with CedeGrid's 19 m, no step | pass |
| Own CPU against the tasks' CPU, in the three 14-CPU idle-fill runs | FR-073 | `743312b` | 2026-11-03 | tasks' CPU from `/proc` 13568 m, 13133 m and 11696 m; the decisions' own-CPU bound 13575–13812 m, 13130–13358 m and 11669–11872 m; the tasks are 244 m, 225 m and 176 m below the upper bound | pass |
| Own-CPU clock tests | FR-073 | `770651f` | 2026-10-26 | 44 telemetry tests pass natively, including the gated process-clock tests (the kept log names two; the count of 3 is not confirmed) | pass |
| External estimate at idle, at p95 at most 1% of capacity, 4 CPUs, Tracked and Full | FR-073 | `64999b1` | 2026-11-02 | 30 m in Tracked and 33 m in Full, 0.75% and 0.82% of capacity (medians 23 m and 24 m); CedeGrid's own 9–10 m; 120 decisions each, every measurement known | pass (1%) |
| The same, 14 CPUs | FR-073 | `9916e59` | 2026-10-29 | Tracked 99 m (0.71%), Full 95 m (0.68%); CedeGrid 8–9 m at the median | pass (1%) |
| Beside a steady external user, Neutral and Performance (4 CPUs, a 60 m user, the catch-up loop) | SC-015 | `770651f` | 2026-10-26 | own tasks 3500 m in both modes (targets 3366 and 3474 m); no step change | pass |
| Beside a steady external user, Yield First (4 CPUs, 250 m steady tasks, a 150 s window, the second account's steady light load at 101 m median) | SC-015 | `44f8257` | 2026-11-13 | 12 tasks, the last admitted 373.9 s after submission; own work 3006 m, 91.2% of capacity less the second account's 103 m and the margin (target 90%) | pass |
| Yield First's cooldown in the same run: a one-core burst of 1.5 s on the owner's account every 60 s, and the second account's load raised to about 400 m 90 s into the window | FR-034 | `44f8257` | 2026-11-13 | the bursts read 950–1060 m; five of them, once tasks were admitted, left own work outside its target for one or two decisions each, with a cap for the burst's length and no pause; none restarted the cooldown, and CPU and RAM admission stayed allowed through every burst; the only closed stretch before the raise was the agent's start cooldown; the second account's rise read 407 m 0.56 s after it began, and in that decision the cooldown restarted (closed for 30.5 s) and own work's target fell from about 3110 m to 2897 m, with own work's caps lowered in the same decision | pass |
| No drain when CPU is the only contended resource | SC-007 | `9916e59` | 2026-10-29 | in the idle-fill, SC-015 and return runs at this commit, every drain was the harness's own cancel after its measurement window | pass |
| Loop cadence with 129 tasks (the 14-CPU Performance idle-fill run) | FR-080 | `9916e59` | 2026-10-29 | observation interval p50 498 ms, p95 533 ms, maximum 609 ms; CedeGrid 104 m (0.74% of capacity); agent `loop_cost` per minute: sample 3.6 s, slots 1.08 s, policy 0.18 s, heartbeat 0.15 s | pass |
| CedeGrid's own cost at 117 tasks, in a Linux container (6 CPUs, 117 light tasks, release build with fat LTO, 60 s) | SC-014 | `9916e59` | 2026-10-29 | the agent 74 m and CedeGrid about 109 m in all, 0.78% of 14 CPUs. The agent's sampling stage took 30.2 ms per second against 55.7 ms at `abbcd63`, measured back to back: own tasks are re-verified through the pidfd held since their first check and their counters read from `/proc` directly (from 5.7 to 0.4 ms per interval), and principals come from one `/proc/<pid>/stat` read per process (from 10.9 to 2.9 ms per interval). The rest of the stage is reading each task's clock and anonymous memory (about 7 ms per interval) | pass (1%) |
| CedeGrid's own cost inside a task, by the process's CPU time (`python/tests/test_own_cost.py`, `npm/tests/own-cost.test.ts`) on the development host | SC-014 | `6d1e871` | 2026-11-13 | a drain check, which also checks the target file's identity, costs 3.5 µs in the Python SDK and 4.5–5.4 µs in the TypeScript one; reading and parsing a changed target file costs 36 µs and 42–49 µs; at 20 checks a second, the matched probe's rate, that is 0.007% and 0.009–0.011% of one core (TypeScript ranges over the runs of `6d1e871` and later); both tests fail at 1% | pass |

### Own throughput beside another user

Own throughput is with CedeGrid against the same probe without it. These runs use the
`own-efficiency` profile: every default but the mode, the coordinator and the harness on CPUs
outside the scope, and the agent on the scope's CPUs. For Performance, only loss beyond its 2%
margin and the agent's measured CPU counts.

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| Idle, 2 CPUs, the half-core probe checking for a drain every 50 ms | FR-020, FR-021, FR-022 | `1c6ca7e` | 2026-10-30 | Performance 0.995 (0.995, 0.995, 0.996), Neutral 0.999, Yield First 0.995; the agent 9 m (0.43% of the scope), so no Performance loss counts | pass |
| A light other user on the probe's core (100 requests a second) | FR-020, FR-021, FR-022 | `1c6ca7e` | 2026-10-30 | no step in any mode; own throughput 0.998 in Performance, 1.010 in Neutral, 0.998 in Yield First; the other user's p99 0.68 ms alone and 0.66–0.69 ms beside own work, with or without CedeGrid; the agent 8 m | pass |
| A saturating other user on the probe's core, Performance | FR-022 | `10b784f` | 2026-11-17 | glibc's per-thread allocator cache off in both probes: own throughput 1.003 and 1.003 of without CedeGrid; no window refused (even/odd at most 1.005); no CPU step; probe CPU per unit 161.3–163.2 µs; the managed probe got 1.006–1.012 of the unmanaged probe's CPU | pass |
| The same, Neutral | FR-021 | `10b784f` | 2026-11-17 | the same setup: 1.009 and 0.998; no window refused (even/odd at most 1.005); no CPU step; probe CPU per unit 161.3–163.6 µs; the managed probe got 1.004–1.013 of the unmanaged probe's CPU | pass |
| The same, Neutral, the allocator's per-thread cache at 0 and at its default 7 (control) | FR-021 | `4f6830d` | 2026-11-17 | at 0: 1.002, no window refused; at 7: 0.995, both unmanaged windows refused (even/odd 1.012–1.013), as in every gate run at the default, managed and unmanaged alike. The cache sets the probe's even/odd unit cost, so the gate runs both probes with it off | control |
| The same, Yield First | FR-020 | `1c6ca7e` | 2026-10-30 | caps, hints and pauses; own throughput 0.174 | as specified |
| A fixed allocation against CedeGrid in Performance: 4 CPUs, the other user saturating two of them 30 s on and 30 s off for 300 s | FR-022 | `1c6ca7e` | 2026-10-30 | own work with CedeGrid 1.24 times that of the probes pinned with taskset to the other two CPUs; the other user's p50 while on 0.325 ms beside the fixed allocation and 0.347 ms beside CedeGrid, p99 0.356 ms and 3.33 ms; 188 CPU step changes in 300 s; the agent 9 m | own work pass; the other user's p99 is the cost |

In the fixed-allocation run the other user's median stays within 0.02 ms, but their p99 grows
while capped own tasks share their CPUs: rootless (Tracked) caps are duty cycles that stop and
resume own work within each period. The loss rule protects the other user's demand and fair
share, not their tail latency. Keeping own threads off a contended CPU, by affinity or
`cpu.weight` before a duty cycle, is not part of this result.

### Launch and stop

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| 150 tasks at once, an operator drain, then TERM, at the test's request budget of 1 MiB/s | FR-074, FR-080 | `10b784f` | 2026-11-17 | 150 of 150 ran. Longest gap between observations 548 ms (bound 1 s). None running 1619 ms after the drain began, all released by 4276 ms; 150 of 150 recorded Draining. Longest supervisor journal wait 292 ms, no `journal_busy` event. No lease expired, no supervisor error | pass |
| The same at the default 10 MiB/s | FR-074, FR-080 | `10b784f` | 2026-11-17 | 150 of 150 ran. Longest gap 552 ms. None running 1620 ms after the drain began, all released by 4274 ms; 150 of 150 recorded Draining. Longest supervisor journal wait 288 ms. No `coordinator_disconnected` event (the run directories were kept to read one). No lease expired, no supervisor error | pass |
| Descriptors in a 150-task burst | FR-077 | `fc5e330` | 2026-10-30 | the agent's soft descriptor limit raised to 524288, 1079 open at the peak; all 150 ran, no agent exit, no lease expired, no refusal and no supervisor error | pass |

A supervisor writes a drain's notice and signals before its Draining record. A run whose
Draining record has not landed reads running until its Released, so the test reports the time
until no run reads running and the time until every run reads Released apart.

### Families and supervision

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| A multiprocessing stand-in, Tracked: a leader filling 1 GiB, then 4 forked workers reading it and spinning for 120 s, 4 CPUs, Performance | FR-083, FR-085, FR-086 | `5c49ee2` | 2026-10-31 | a family of 5, nothing outside it; own memory 1035 MiB against the buffer plus the workers' private pages, 1029 MiB; beside the saturating user own CPU 1914 m against a target of 1849 m, and 17 of 18 stopped samples with all 5 members stopped; the agent 22 m idle and 21 m beside the load (0.56% and 0.53% of the scope) | pass |
| The same in Full | FR-083, FR-085, FR-086 | `cd077e9` | 2026-10-31 | a family of 5 in every decision (`family_members`), nothing outside it, no `cap_at_minimum_duty`, every member at the launch baseline `oom_score_adj` 0; own memory 1035 MiB against 1029 MiB; beside the other user saturating the 4 CPUs own CPU 1685 m against a target of 1671 m, the leader idle in its join; the agent 12 m | pass |
| A data loader with 4 forked workers (`DataLoader(num_workers=4)`) over a synthetic dataset, the same way | FR-083, FR-085, FR-086 | `cd077e9` | 2026-10-31 | a family of 5, nothing outside it; own CPU beside the load 1954 m against 1917 m (Tracked) and 1901 m against 1887 m (Full); own memory 303 MiB (anonymous and shared PSS) against 606 MiB of total PSS with the library pages. In Tracked, 8 of the 10 samples that caught a stop had all 5 members stopped at once, and every stop was followed by all 5 running. The agent 35 m in Tracked (0.9%), 11–12 m in Full | pass |
| Leftovers: the leader exits after 5 s while its 4 workers run on | FR-087 | `cd077e9` | 2026-10-31 | Tracked and Full, Guaranteed and Opportunistic: `members_ended` 4, the release confirmed 7–8 ms after the leader's exit, no member, zombie or leaf left | pass |
| Supervisor loss in Tracked: a leader and 2 CPU workers held in an off phase by a Yield First cap beside a light load of the test's own account, the run's supervisor killed while all 3 were stopped | FR-088 | `cd077e9` | 2026-10-31 | Guaranteed: no SIGHUP; the leader running at once (its parent-death signal is SIGCONT), every member running 0.45 s after the kill, the output complete; `family_recovered` continued 3. Opportunistic: every member gone 0.45 s after the kill, none left stopped; `family_recovered` ended 2, already gone 1 (the leader, by its parent-death signal) | pass |

### GPU

The light other GPU user runs 200 small multiplies a second. In the two-probe runs each own
probe is its own job with 512 MiB, submitted 40 s before each managed window; a window counts
when both probes ran together inside the other user's load. The other user's bound is the
smaller of its rate without CedeGrid and half its rate alone, less 3% (SC-017).

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| GPU work on an occupied device | SC-006 | `893c1c2` (Performance), `770651f` (Neutral) | 2026-10-26 | the owned-context test passes; a fresh external GPU context takes 74 MiB, so the 512 MiB minimum stays. Performance: 100 of 100 GPU tasks complete beside an idle external context, admission open, no GPU step, unattributed device memory 203 MiB. Neutral: 100 of 100 beside an idle external context and 50 of 50 without it, admission open, no GPU step | pass |
| GPU pause, duty cycle and per-process attribution | FR-011, FR-007 | `893c1c2` | 2026-10-26 | paused: 0 completions, VRAM unchanged; a 50% duty cycle at 500 ms gives 0.50 of baseline, longest gap 258 ms, median time between kernel completions 4.4 ms; summed per-process SM differs from device utilization by at most 2, 2 and 5 points with 1, 2 and 4 processes | pass |
| GPU reclaim, Neutral and Performance (protection profile) | SC-012 | `770651f` | 2026-10-26 | Performance: matched throughput 0.923, decision p95 0.41 s, post-yield p99 0.76×. Neutral: matched throughput 0.722; each new job's first interval has no fresh per-process sample baseline, so Neutral counts that activity as one more principal and caps the job at half the device until its next scale-up step | Performance pass, Neutral fail |
| Idle device, 3 CPUs (`own-efficiency`) | FR-021, FR-022 | `1c6ca7e` | 2026-10-30 | own throughput Performance 1.002, Neutral 1.000; the agent 16.5–17.1 m (0.55–0.57%) in the idle cases | pass |
| Yield First on an idle device, three comparisons with a 180 s readiness wait | FR-027 | `6e3fb89` | 2026-11-13 | all 18 managed probes were admitted and finished, 37.3–38.8 s after their submission; while the owner's own unmanaged probe was busy, GPU admission read open in every sample | pass |
| Yield First while the second account holds an idle context on the device | FR-027 | `44f8257` | 2026-11-13 | all 15 cases completed; GPU compute admission was open in every sample, also while the owner's unmanaged probe was busy; the managed probes were ready 2.1 to 9.2 s after their start, the longer waits being the GPU memory cooldown (the readiness times are not in the kept evidence) | pass |
| Yield First while the second account computes | FR-027 | `44f8257` | 2026-11-13 | GPU compute and memory admission held on `gpu_yield_first_needs_quiet_device` from the start, and the managed probe was not admitted within 60 s | pass (held) |
| One own probe beside the light user, Performance | SC-017 | `36bfe68` | 2026-11-01 | the probe alone 1572 units/s; without CedeGrid 1319 and 1303 (0.834 of alone); with it 1306 and 1298 (0.831 and 0.826 against a bound of 0.793 at the target 823); the old ratio 0.993; no GPU compute step. The light user ran 200 requests a second; p50 0.77 ms with and without CedeGrid, p99 0.88–0.89 ms without and 1.08–1.10 ms with | pass |
| One own probe beside the saturating user, Performance | SC-017 | `36bfe68` | 2026-11-01 | the probe alone 1560; without 736 and 731 (0.470); with 736 and 731 (0.472 and 0.469 against 0.44); the old ratio 1.000; no step. The other user 1555 requests a second alone, 749–755 without, 775 and 767 with (bound 729); p50 1.36–1.37 ms and p99 1.37–1.38 ms with and without | pass |
| One own probe beside the light user, Neutral | SC-017 | `36bfe68` | 2026-11-01 | the probe alone 1552; without 1304 and 1295 (0.837); with 1298 and 1290 (0.836 and 0.831 against 0.762 at the target 792); the old ratio 0.996; no step. The light user 200 requests a second; p50 0.77–0.78 ms, p99 0.88–0.89 ms without and 0.94–0.98 ms with | pass |
| One own probe beside the saturating user, Neutral | SC-017 | `36bfe68` | 2026-11-01 | the probe alone 1557; without 736 and 731 (0.471); with 731 and 731 (0.469 and 0.469 against 0.441); the old ratio 0.997; no step. The other user 1552 requests a second alone, 749–754 without, 761 and 838 with (bound 729); p50 1.37 ms and p99 1.37–1.38 ms with and without | pass |
| Two own probes beside the saturating user, Performance, Tracked | SC-017 | `a0a7bea` | 2026-11-13 | the pair alone 1496 units/s; without CedeGrid 0.660 of it, the other user 512 requests/s of its 1571 alone. With it: own work 0.486 and 0.475 (bound 0.45); the other user 785.1 and 797.0 (½ × alone × 0.97 = 761.8). Both windows ran both probes together (overlap 39.8 s, m = 2 in 78 decisions of 80 each); no pause; the duty held 0.734 and 0.718; the probes' off phases overlapped 0.998 and 0.997. Both probes counted, and the first duty sent, in each window's first decision (1.1 s and 1.3 s in) | pass |
| The same in Full, the off phase being the leaf freeze | SC-017 | `a0a7bea` | 2026-11-13 | the pair alone 1479 units/s; without CedeGrid 0.664, the other user 509 of its 1555. Own work 0.480 and 0.477; the other user 792.5 and 794.2 (bound 754.2); m = 2 in 78 and 79 decisions; frozen 0.28 of the time in each window; both probes counted and the first duty sent at 1.2 s and 1.0 s | pass |
| Two own probes beside the saturating user, Neutral, Tracked | SC-017 | `a0a7bea` | 2026-11-13 | the pair alone 1480 units/s; without CedeGrid 0.663, the other user 509 of its 1554. Own work 0.447 and 0.447 (bound 0.42); the other user 840.0 and 840.4 (bound 753.4); m = 2 in 79 and 78 decisions; the duty held 0.672 and 0.673; the first duty at 1.0 s and 1.2 s | pass |
| Two own probes beside the light user, Performance | SC-017 | `f81a7c0` | 2026-11-01 | the pair alone 1474; without 0.864, the light user 200 requests a second, p99 2.73 ms. With it: own work 0.864 and 0.863 (bound 0.791), never capped (no principal slice-bound in any decision); the light user 200 and 200, p99 2.728 and 2.730 ms (bound 3.005) | pass |
| A zero GPU compute margin (Aggressive's GPU behavior), two own probes beside the saturating user | SC-017 | `23e386e` | 2026-11-01 | the pair alone 1497 units/s; without CedeGrid 0.658 of it, the other user 511 (0.33 of its 1569 alone). With it: own work 0.498 and 0.493 (bound 0.47); the other user 760.7 and 754.5 (½ × alone × 0.97 = 761.0); the duty held 0.754 in both windows | own work passes; the other user falls short by 0.3 and 6.5 requests a second (0.485 and 0.481 of alone); a record, not a feature 001 gate |
| Full, a process of the test's own account joins a probe's frozen leaf | FR-001 | `a0a7bea` | 2026-11-13 | the leaf read frozen at the join and thawed 49.8 ms later and stayed thawed; `cpu.max` stayed `max 100000`; the process kept nice 0, saw no signal, and its largest gap was 35.7 ms | pass |
| Full, the same join, and the probe's task cancelled 19.5 ms after it | FR-001 | `a0a7bea` | 2026-11-13 | the probe was gone 1.52 s after the join. 3 s later the leaf held only the joined process, with `cgroup.freeze` 0 and `frozen` 0; the process was running (`S`) and `cpu.max` read `max 100000`. The leaf, kept by that process, went with the run's scope | pass |
| A GPU job whose three loader threads want more CPU than its fair share, beside a saturating other user on its two CPUs | FR-022 | `1c6ca7e` | 2026-10-30 | Tracked: units 1.01 and CPU 1.00 of the job without CedeGrid, GPU 69% against 70%, with caps. Full: units 0.95, CPU 0.93, GPU 69% against 71%, with a lower share. In neither does GPU throughput fall more than the CPU share explains | as specified |

### Memory

The test deployment's floors are 16 GiB of RAM and 4096 MiB of GPU memory. The GPU memory
`noise_tolerance` withholds nothing: it is a growth threshold and a reported field, pinned by
`an_idle_device_withholds_no_noise_tolerance_from_gpu_memory` (`tests/policy.rs`).

| Gate | Requirement | Commit | Measured | Figures | Result |
|---|---|---|---|---|---|
| Idle GPU memory fill: capacity 6144 MiB less the 4096 MiB floor and 285 MiB of others' use, 1763 MiB; 1354 MiB less Yield First's margin | SC-014 | `6e3fb89` | 2026-11-13 | what CedeGrid offered before the last task matched the target to the MiB in both modes; with tasks that hold their request, own GPU memory 1756 MiB (99.6%) in Performance and 1346 MiB (99.4%) in Yield First | pass |
| Idle RAM fill: capacity less the 16 GiB floor, others' use and their peak | SC-014 | `6e3fb89` | 2026-11-13 | Performance: own RAM 11540 MiB of 11569 (99.7%), with 264 MiB of others' peak kept back. Yield First: 8831 MiB of 8859 (99.7%), less its 2976 MiB margin. Own RAM was constant over each 60 s window | pass |
| RAM reclaim: after a Performance fill the second account grows by 4 GiB in about 1.1 s | SC-012 | `9c01943` | 2026-11-02 | from the breach to the floor restored: 4.43 s with plain tasks (bound 5.5 s), 5.99 s with hint-ready tasks (bound 10.5 s), 1.04 s with Guaranteed hint-ready tasks (bound 15.5 s). The second account got all its memory each time | pass |
| A floor breach the second account causes, growing by 10 GiB, Performance and Yield First | FR-023 | `9c01943` | 2026-11-02 | Performance: own RAM fell from 12764 MiB to 6172 MiB in 2.3 s and stopped at its fair share of about 6370 MiB, with the floor breached until the second account released 90 s later. Yield First: from 9912 MiB to 760 MiB in 3.2 s and then 0; free memory stayed 1318 MiB above the floor | pass |
| The same, Neutral | FR-023, FR-037 | `10b784f` | 2026-11-17 | own RAM fell from 12828 MiB to 2396 MiB in 3.3 s, every task freeing on its hint, and the floor was restored 2.86 s after the breach; no own task was drained, and the 13 tasks got the save-progress signal at 0.4–2.9 s. No OOM kill | pass |
| The out-of-memory order without an OOM (a 3 GiB capacity) | FR-023 | `9c01943` | 2026-11-02 | the second account's 1.5 GiB took the fair share from 1139 MiB to 558 MiB. Only the task launched last went to `oom_score_adj` 1000; the in-share task kept its launch value 0. When the second account released 3 s later, the share was back at 1038 MiB, and the same process went back to 0 before any drain. With `ram.prefer_own_oom_victim` every own task was at 1000 from launch | pass |
| An own task past its leaf's `memory.max` (Full, 256 MiB) | FR-076 | `9c01943` | 2026-11-02 | with host swap, the kernel swapped the excess, and the task ran at 262 MiB resident to its end. With `memory.swap.max = 0` on the test's own delegated root it was killed three times in its leaf (host `oom_kill` +3). Each kill was recorded as `oom_kill` with `own_fault`, the fair share being unknown before its first memory reading, and counted as a failure. After three attempts the task stood at `needs_reconciliation`. No leaf was left | pass |
| A host-wide OOM the second account causes, beside a 3 GiB over-share task that ignores hints and six 1 GiB in-share tasks that read them (Performance, Guaranteed) | FR-023, FR-037, FR-075 | `10b784f` | 2026-11-17 | counted from the start of the second account's growth: the floor breached at 1.3 s, the six in-share tasks got the save-progress signal at 1.5–1.6 s, and the over-share task was sent TERM at 4.0 s. It was marked at `oom_score_adj` 1000, and so was one task launched within its share while the victim order counted it in the excess; no other own task was drained. The floor stayed breached until the OOM killer took one process, the second account's allocator (28.9 GB resident), at 21.4 s; no own task was killed | pass for the order |
| Host-wide OOMs the second account causes, beside six in-share tasks and one over-share task, with SDK tasks and with no SDK task | FR-023, FR-075 | `9c01943` | 2026-11-02 | in each of three runs at this commit the kernel took one process, the second account's allocator (24–28 GB resident), and no own task; CedeGrid gave the excess back 7–9 s before each OOM. With SDK tasks, all seven got the save-progress signal at the breach, while own memory was within its share, and the task over share was marked and freed on its hint. With no SDK task, the over-share task, marked at `oom_score` 1385 against the allocator's 756, was drained 2.4 s later and started again after the OOM. The second account's processes kept `oom_score_adj` 0 | in-share work pass; no own task was killed, so the kernel's preference and the requeue after a kill were not exercised |

### Own-work ratios in the protection runs

The CPU and GPU reclaim and SC-005 runs use the protection profile of
`tools/pressure_comparison.py`: a 2-CPU scope with `cpu.reserve_physical_cores = 1` (the default
is 0), built to test reclaim and protection, not own-work efficiency. Their own-work ratios,
0.93–0.94 of the same probe without CedeGrid while idle and 0.70–0.80 under contention, come
from that setup:

- **Idle.** No CPU `managed_idle` case stepped (`yielded: false` in all 15). The managed probe
  asked `draining()` before every unit, and the SDK then re-read and parsed the target file on
  each call; the unmanaged probe asked nothing. A per-unit check of that cost ran a half-core
  pbkdf2 loop at 0.893 of the same loop without it. The launch nice of 10 and the agent sharing
  the scope's two cores may add to the loss.
- **Under contention.** The reserve, not contention, set the budget. The decisions show
  `capacity` 2000, `reserve` 1000, `external` 594 and `target_share` 406 against own usage of
  424–524 m, so own work was capped while about 880 m stood idle. With the defaults, Neutral's
  5% margin leaves a budget of about 1306 m, and the case would not step.
- The probe and the protected load run on different physical cores, so the protected load's
  p99 without CedeGrid is not same-core contention either.

From `1c6ca7e` on, the SDKs re-read the target file only when its stat identity changes, both
probes check for a drain every 50 ms (the unmanaged one through a no-op context), Neutral and
Performance tasks launch at nice 0, and the `own-efficiency` profile keeps every default, with
the coordinator and the harness outside the probe's scope and the placement in the report. The
own-throughput rows above use that profile.

## Rules for validation runs

- Report these statuses separately:
  - implementation
  - Linux-native rootless execution
  - optional controls
  - multi-node work
  - fault and recovery
  - bounded stress
  - soak
- Uncertain allocations stay reserved until verified reconciliation. Failure injection affects
  only test-owned processes and connections. Unrelated processes are never signalled.
- Use an isolated run directory under the operator's home. Preserve original datasets,
  checkpoints and failed evidence.
- Targets:
  - zero ownership, accounting or result violations
  - matched managed throughput ≥ 90%
  - manager RSS ≤ 512 MiB per node
  - manager CPU ≤ 0.1 logical CPU idle and ≤ 0.5 active
  - decision p95 ≤ 1 s
  - release p95 ≤ 10 s after the decision
  - protected post-yield p99 ≤ 1.20× baseline
  - coordinator recovery ≤ 60 s
  - node rejoin ≤ 90 s
- Report detection, drain, termination and release separately. Insufficient samples are
  inconclusive. Higher CPU utilization alone is not a speedup.
- Every run manifest records:
  - the exact source and environment
  - seeds and worker counts
  - time bounds and scope
  - raw measurements and cleanup
- After a failure or a scoped change, rerun only the affected checks and append the results.

See [comparison](comparison.md) and the [soak and pressure harness](soak-harness.md) for the
tools, and [guarantees](guarantees.md) for the support boundary.
