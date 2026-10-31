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

- **Multiple physical nodes.** Not yet verified on physical hosts:
  - useful work from two nodes
  - loss and rejoin of a node
  - checkpoint transfer between nodes
- **GPU.** Verified on one consumer device only (see
  [Native gate results](#native-gate-results-feature-001)). Other devices and drivers are not
  verified. An interval that lists only own verified processes is measured, but on the test
  host the display manager always lists a GPU process, so a short GPU job in Neutral ran
  capped at half the device for about 10 s after it started. Since item 027 an idle context of
  a user ID below `uid_min` is background while per-process utilization is readable. On the
  test host per-process utilization is readable, but the greeter runs under a user ID at or above
  `uid_min`, so its context still counts and GPU activity stays unknown. Since item 028 a
  delegated cgroup gives own GPU work the same compute share as the rootless backend, with a
  leaf freeze for the off phase where `cgroup.freeze` is authorized; before, it paused instead.
  Tested with fixture leaves only; the native GPU confirmation goes with the next approved native
  batch. The GPU attribution tolerance stays 15 points until a second device is measured;
  the first device's largest gap was 5.

  Freshness is never inferred from aggregate utilization or from an NVML `NotFound` response.
- **Performance ramp.** On one Linux host the ramp reaches 90% in under 3 s on CPU and on GPU
  compute. A one-core contender started mid-ramp under a second unprivileged account reaches
  0.9 core 2–3 s after it starts, pinned to one scope CPU or not, and holds a median of
  0.92–0.94 core. Under the account that runs CedeGrid the same contender gets about 0.7 core,
  as SC-002 allows for that account's own programs (see
  [Reruns with launch pacing](#reruns-with-launch-pacing)). At item 027's commit the pinned
  contender held a median of 0.90 core, 0.9 core or more in 61% of samples, while own work
  stayed 80 m over its target and its caps were relaxed and set again every few intervals (see
  [Reruns after item 027](#reruns-after-item-027)).
- **Return after contention.** In Yield First the target returns one interval after a
  contender's last usage, but held caps wait until the external estimate that no principal
  accounts for has stayed below `principal_min_percent` for `principal_window_intervals`. The
  8-interval window keeps that estimate up for about 4 s, so on the test host own work was back
  at its uncontended target 7.5 s after the contender stopped.
- **Idle utilization.** At item 027's commit, on 14 CPUs with 100 m tasks, own tasks reach
  97.4% of capacity in Performance (98.2% with CedeGrid), 92.3% in Neutral (92.9%) and 81.8% in
  Yield First (82.4%), against SC-014's 97%, 94% and 84%; on 4 CPUs Performance reaches 97.1%
  with CedeGrid. The bound on own measurement now takes 16–35 m and CedeGrid 77–104 m. In
  Neutral and Yield First, admission charges each running task at its p95 usage (FR-034), and
  the steady tasks' p95 is above their mean, so the charges fill the share while the tasks' mean
  use stays 242 m and 304 m below the target (`held_back` reports it as `charge_shortfall`); the
  external estimate, mostly its measurement band, takes another 88 m and 155 m. Before item 027
  the same scope reached 91.4%, 87.0% and 73.0% (see
  [Reruns with exact own measurement](#reruns-with-exact-own-measurement)).
- **CedeGrid's own cost.** SC-014 counts own tasks plus CedeGrid with a 1% bound for CedeGrid
  at 117 tasks on 14 CPUs (140 m). In the Docker Linux check (6 CPUs, 117 light tasks, release
  build with fat LTO, 60 s), with supervisors that wait on events, the agent used 108 m and the
  117 supervisors 34 m in all, 142 m; supervisors that passed every 100 ms used 85–125 m. The
  agent's sampling stage then took 55.7 ms per second. Two changes that leave every decision as
  it was cut it to 30.2 ms per second and the agent to 74 m, CedeGrid about 109 m in all
  (0.78% of 14 CPUs), measured back to back: own tasks are re-verified through the pidfd held
  since their first check and their counters read from `/proc` directly instead of through two
  sysinfo refreshes per interval (from 5.7 to 0.4 ms per interval), and principals come from one
  `/proc/<pid>/stat` read per process (from 10.9 to 2.9 ms per interval). The rest of the stage
  is reading each task's clock and anonymous memory (about 7 ms per interval). Fat LTO lowered
  the agent's cost by about 12% (127 to 112 m). Resident memory: the agent 52 MiB proportional,
  a supervisor 1.6 MiB proportional (9–11 MiB counting shared pages). On the test host, with
  129 tasks on 14 CPUs, CedeGrid used 104 m (0.74% of capacity), and the agent's sample stage
  took 60 ms per second.
- **Principals counted threads.** Until item 027's decisions on report 027-1, principal usage
  came from a sysinfo refresh that lists every thread as a process of its own, so a process's
  CPU counted once for the process and again for each thread, its resident memory once per
  thread, and own tasks' threads, not in the managed set, as the owner's unmanaged usage
  (research R26). Each process now counts once. Earlier native results that read principal
  usage, such as T178's contender share and SC-015, ran with the old count.
- **Victims of an excess.** Caps already held give a contended CPU excess between them at one
  level within their tier, and own usage over its target is shared the same way; a new victim
  is still capped by the whole excess left, in cost order, and a cap to zero becomes a pause.
- **Launch bursts.** A node renews each running assignment's lease every heartbeat (2 s by
  default). Each renewal is a synced coordinator write, about 9 ms on the test host. Without
  lease batches, renewals share half of each monitor interval, so one node keeps about 117
  leases renewed; since item 027 a coordinator that offers `lease_batch/1` takes one request
  per iteration in one transaction. In the native rerun at item 027's commit no lease expired
  and the longest gap between observations was 905 ms, but the agent exited with "Too many open
  files" after 137 of the 150 tasks had started: the host's soft limit is 1024 descriptors, and
  the agent holds about 7 per running task (three supervisor pipes, a state database connection
  and its WAL, the pidfd held since the task's first check, and the stderr capture), plus pipes
  for each launch in progress. Each started task's supervisor drained it once the agent was
  gone. Before item 027, in
  the native burst gate, 150 light tasks admitted at once, 123 leases expired before their first
  authorization or a renewal: 67 tasks never started, 81 were drained after starting, and the
  longest gap between observations was 1.25 s against a 1 s bound. Where synced writes cost
  50–90 ms (Docker, macOS) the limit is lower. A reservation is no longer refused because its
  own burst closed admission (see [Reruns with launch pacing](#reruns-with-launch-pacing)).
- **Cancelled reservations.** Under contention a node may cancel a reservation that has not
  started. Before item 027, if the coordinator authorized it before the cancellation landed,
  the node drained it once it ran, even when CPU was the only contended resource: on the test
  host, 4 of 16 tasks in a Yield First run and 1 of 117 in a Performance run. A cancelled
  reservation is now released before authorization, and a CPU drain planned while pending is
  planned again as running work. In item 027's native runs no task was drained except by each
  harness's own cancel at its end.
- **Storage.** A FUSE-backed filesystem without a supported directory-sync barrier is not
  qualified for durable namespace publication. The replayable profile tolerates process crashes
  only.
- **Endurance.** A long-duration soak has not been run. Bounded stress is the release
  requirement.

## Native gate results (feature 001)

Run on 2026-10-26 on one authorized Linux host, the test host, with the commands in
`specs/001-contention-modes/native-gates.md`. These results hold for this host only. `src/`
and `Cargo.lock` are the same at every commit listed, so each gate ran the same product code.

| Host role | Kernel | CPUs | RAM | GPU | Driver |
|---|---|---|---|---|---|
| test host | Linux 7.0.0 (Ubuntu 26.04), cgroup v2 with PSI, `cpu memory pids` delegated to the user manager | 20 logical (Intel Core i5-13500), 1 NUMA node | 31 GiB, 8 GiB swap | NVIDIA GeForce GTX 1060 6 GB | 580.178.04, CUDA 13.0 |

| Gate | Commit | Values | Result |
|---|---|---|---|
| T054, delegated cgroup steps (quickstart §4) | `4d631d0`, `893c1c2` | `cpu.weight`, `cpu.max` and `cgroup.freeze` written, read back and restored; with a foreign test-owned process in the leaf, LowerShare is refused and Cap falls back to the rootless duty cycle ("cgroup write refused"), leaving the leaf unchanged | pass |
| T138, RAM cap | `4d631d0`, `893c1c2` | `memory.high` written, read back and restored to `max`; swap is on, so the swap-off `memory.stat` check does not apply | pass |
| T141e, PSI | `4d631d0`, `893c1c2` | (1) status available, 10 external cgroups, external avg10 0; (3) 40 busy loops on 20 CPUs in own session: own avg10 up to 67.7, external 0, busy 20,000 of 20,000 m, no `saturated_by_psi` in 30 samples; (2) under another account on 2026-10-27, in [Reruns with launch pacing](#reruns-with-launch-pacing) | pass |
| T178, Performance ramp (4 CPUs, 16 tasks of 250 m) | `893c1c2`, `1c8a850` | `ramp_seconds` 2.784 idle, 2.728 and 2.762 with a contender (target 10 s); the contender, one busy loop started at 3 s, gets a median of 0.72 core after 10 s pinned and unpinned, and 0.9 core in 7% and 0% of half-second samples; `foreign_actions` 0 | ramp pass, contender fail (SC-004) |
| T088, GPU on an occupied device | `893c1c2` | owned-context test passes; a fresh external CUDA context takes 74 MiB, so the 512 MiB minimum stays; with an idle external context, 100 of 100 GPU tasks complete in Neutral and in Performance, admission open, no GPU step, unattributed device memory 203 MiB; GPU activity reads Unknown in 88% of those sessions' samples; paused: 0 completions, VRAM unchanged; 50% duty cycle at 500 ms gives 0.50 of baseline, longest gap 258 ms, longest kernel 4.4 ms; T152: summed per-process SM differs from device utilization by at most 2, 2 and 5 points with 1, 2 and 4 processes | pass |
| T117, CPU reclaim (Yield First, Neutral, Performance; protection profile, `reserve_physical_cores = 1`) | `1c8a850` | matched throughput 0.933, 0.942, 0.933; decision p95 0.81, 0.78, 0.77 s; post-yield p99 1.05, 1.03, 1.07×; own usage at the target share 2 intervals (1.0 s) after the demand is first observed in Yield First and Neutral, 1–2 in Performance; one cap and no drain per case; manager peak 0.14–0.19 core and 94 MiB | pass |
| T117, GPU (Neutral, Performance; protection profile, `reserve_physical_cores = 1`) | `c0779d3` | GPU compute at the target share 2 intervals after the protected GPU load starts, after up to 6 caps and one pause, no drain; decision p95 0.07 and 0.60 s; post-yield p99 0.77×; matched throughput 0.923 in Performance but 0.723 in Neutral, where each new 20 s job reads as Unknown GPU activity at start, is capped at the fair share and stays capped through the cooldown | Performance pass, Neutral fail |
| T117, SC-014 (4 CPUs, 500 m tasks, idle) | `374637d`, `c0779d3` | own CPU 87.5% of capacity in Neutral and Performance, 75.0% in Yield First (targets 94%, 97% and 84%); the decisions read own work about 330–430 m low and count that as external; with 500 m tasks own use moves in 500 m steps, so this run cannot reach the targets even with exact measurement | fail |
| T117, SC-015 (beside a 60 m external user) | `374637d`, `c0779d3` | own CPU as a share of capacity minus the user and the margin: 93.6% in Neutral, 90.7% in Performance, 89.8% in Yield First (target 90%); no step change in 5 minutes | Neutral and Performance pass, Yield First fail |
| T118, SC-005 audit (protection profile, `reserve_physical_cores = 1`) | `1c8a850` (CPU), `c0779d3` (GPU) | CPU and GPU, at the defaults and with Performance and a 512 MiB floor: 0 actions and 0 drains outside own allocations, protected snapshots unchanged, every protected run completed, no failed external GPU allocation, every owned GPU release confirmed, 0 OOM lines | pass |

### Reruns with exact own measurement

Run on 2026-10-26 on the same test host at `770651f`, which reads own CPU from the process
clock or the leaf `cpu.stat` with a summed bound and measures an interval that lists only own
verified GPU processes. The kernel has `CONFIG_HZ=1000` and an empty `nohz_full` list, so the
precise own-CPU sources apply. The 14-CPU scope is the host's 14 physical cores, one logical
CPU each. SC runs name their task loop: the catch-up loop (as in the first runs) makes up CPU
lost to a cap or pause, so a task runs a full core when its cap rises; the steady loop does not.

| Gate | Values | Result |
|---|---|---|
| Own-CPU clock tests | 44 telemetry tests pass natively, including the 3 gated process-clock tests | pass |
| T178, Performance ramp (4 CPUs, 16 tasks of 250 m) | `ramp_seconds` 2.755 idle, 2.754 and 2.756 with a contender; `foreign_actions` 0; a contender under the account that runs CedeGrid, started at 3 s, gets a median of 0.70 core after 10 s pinned and 0.62 unpinned, and 0.9 core in 2% and 0% of half-second samples (that account's other programs share its fair share, per SC-002); the contender under a second account was not run, because that account's SSH key is not accepted | ramp pass, contender open |
| SC-004, ramp on GPU compute (1 task) | own GPU compute reaches 90% 2.6 s after submission once the deployment's 30 s GPU cooldown has passed; submitted earlier, it waits for that cooldown (22.6 s) and then reaches 1000 within 0.5 s | pass |
| T117, GPU (Neutral, Performance) | Performance: matched throughput 0.923, decision p95 0.41 s, post-yield p99 0.76×. Neutral: matched throughput 0.722; GPU measurement stays known, but the device always lists the display manager's greeter (another account, a 2 MiB graphics context), so the own-only path does not apply; each new job's first interval has no fresh per-process sample baseline, Neutral counts that activity as one more principal and caps the job at half the device until the next scale-up move about 10 s later | Performance pass, Neutral fail |
| T088 (Neutral, beside an idle external CUDA context and without it) | 100 of 100 and 50 of 50 GPU tasks complete, admission open, no GPU step; GPU compute reads as an unknown amount in 94% and 91% of the retained intervals with own work, mostly for lack of a fresh per-process sample baseline; the greeter is listed in both | pass |
| SC-014, 4 CPUs, 250 m tasks (comparison) | Neutral 87.5% (no step change); Performance 91.8% with 163 step changes (catch-up loop), 89.0% with 1 (steady); Yield First 49.4% with 21 step changes and 61 paused samples (catch-up), 69.7% with none (steady); targets 94%, 97% and 84% | fail |
| SC-014, 14 CPUs, steady loop | Performance, 250 m tasks: 91.4% of capacity, 93.7% with CedeGrid's own processes, 130 step changes and 9 paused samples; Neutral, 100 m tasks: 87.0% and 90.5%, no step change; Yield First, 100 m tasks: 73.0% and 76.4%, no step change; targets 97%, 94% and 84%. With 100 m tasks Performance admitted 134 at once and every task failed (below) | fail |
| SC-015, 4 CPUs beside a 60 m external user | own tasks 3500 m in Neutral and in Performance (targets 3366 and 3474 m, catch-up loop, no step change); Yield First 1978 m with 100 step changes (catch-up), 3051 m of 3006 m with none (steady) | pass with the steady loop |

What holds own CPU work below the SC-014 targets on this host:

- CedeGrid's agent and supervisors (56–65 m on 4 CPUs, 325–489 m with 53–113 tasks) and the
  gap between own work's measured lower bound and its use (95–144 m on 4 CPUs, 513–907 m on 14)
  count as external, so they come out of the share. With 113 tasks the summed own-CPU bound
  equals the per-allocation sum, as the read pass over that many processes is long, and the
  tasks' CPU read from `/proc` was 173 m above the decisions' upper bound; that gap is not yet
  explained.
- With the catch-up loop, held caps that rise together let every task burst at once; Yield
  First then pauses or caps most tasks to zero about every 30 s. Admission also counted capped
  usage as room: a fourteenth task started with all thirteen capped. With the steady loop,
  neither mode cycles on 4 CPUs.
- The agent's loop slows as tasks are added: about 1.4 s per 500 ms interval with 113 tasks.
  Launching 134 tasks at once stalled it 4.2 s, past the coordinator's 3 s telemetry limit, so
  every initial authorization was refused and all 140 tasks failed. The launch barrier held
  and every release was verified.

### Reruns with launch pacing

Run on 2026-10-27 on the same test host at `07c2d8b`, which launches assignments off the
agent's loop, at most 32 per heartbeat, with lease requests sharing half of each monitor
interval, and authorizes a burst's own reservations while admission is closed only by that
burst. Loads under another account ran as a second unprivileged account, which started and
stopped them; CedeGrid did not act on them. Loop costs come from a copy of the same commit with
stage timing added.

| Gate | Values | Result |
|---|---|---|
| Launch-burst gate (150 light tasks at once, Performance) | longest gap between observations 1253 ms (bound 1000 ms); 8 of 150 tasks running at the stop; no refusal for stale evidence or for admission closed by the burst's own allocations, as admission stayed open until leases expired; 123 leases expired before the first authorization or a renewal, so 67 tasks never started and 81 were drained after starting; later, 36 requests for the expired tasks were refused pending reconciliation and 8 for admission closed while those tasks were released. Loop with 150 slots: 659 ms per iteration at the median and 1112 ms at p95, of which lease requests 575 ms at the median and heartbeats up to 515 ms | fail |
| T178, Performance ramp with a contender under a second unprivileged account (4 CPUs, 16 tasks of 250 m) | `ramp_seconds` 2.591 idle, 2.686 pinned, 1.173 unpinned; the contender, one busy loop started about 3.6 s after submission, reaches 0.9 core in 3.0 s pinned and 2.0 s unpinned, with a median of 0.94 and 0.92 core after 10 s and 0.9 core or more in 81% and 76% of half-second samples; its account's CPU pressure some avg10 stays near 5.5, below `psi_some_percent` (10), so no decision reports `saturated_by_psi`, and Performance's target is capacity minus external minus the margin (about 2730 m of 4000); `foreign_actions` 0 | pass |
| T141e step 2, PSI under another account | eight busy loops of a second unprivileged account on 2 CPUs: external some avg10 67.9–92.0 in all 30 samples, `saturated_by_psi` on CPU in all 30, busy 10.1–11.2% (`saturation_percent` 98) | pass |
| SC-014, 14 CPUs, 100 m tasks, steady loop (Performance) | 116 tasks admitted in 17.3 s, at most 32 per heartbeat; longest gap between observations 749 ms; no refusal; own tasks 89.6% of capacity, 92.3% with CedeGrid's own processes (target 97%); CedeGrid's 386 m and the own-measurement bound's 1441 m count as external; one running task drained after its cancelled reservation was authorized. Loop with 117 slots: 341 ms of work per 500 ms interval at the median, of which lease renewals 263 ms, sampling 58 ms and heartbeats up to 59 ms | burst pass, SC-014 fail |
| SC-015, 4 CPUs beside a 60 m external user (Yield First, catch-up loop) | own tasks 2661 m, 84.0% of capacity minus the external user and the margin (target 90%); 65 step changes and 105 paused samples; 4 of 16 running tasks drained after their cancelled reservations were authorized | fail |

### Reruns after item 027

Run on 2026-10-29 on the same test host at `9916e59`, a release build with fat LTO, with the
commands under "Item 027 native gates" in `specs/001-contention-modes/native-gates.md`. Loads
under another account ran as a second unprivileged account, which started and stopped them;
CedeGrid did not act on them. In runs 3, 4 and 6 every drain was the harness's own cancel after
its measurement window.

| Gate | Values | Result |
|---|---|---|
| Run 1, idle 60 s (external estimate at p95 at most 1% of capacity) | Tracked 42 m on 4 CPUs (1.05%) and 99 m on 14 (0.71%); Full 41 m (1.02%) and 95 m (0.68%); CedeGrid 8–9 m at the median. The estimate is the window's upper bound on host busy minus own work and CedeGrid: on 4 CPUs the window's busy bounds were 7 m and 41 m at the median, while the host's busy with no CedeGrid process was 2.5 m (7.4 m at p95) | 14 CPUs pass, 4 CPUs fail |
| Run 2, launch burst (150 light tasks at once, Performance) | longest gap between observations 905 ms (879 ms in a second run); no lease expired, no refusal, no supervisor error; 142 tasks reserved and 137 started, then the agent exited with "Too many open files" (soft limit 1024), and each supervisor drained its task | fail |
| Run 3, SC-014, 14 CPUs, 100 m tasks, steady loop | Performance: 129 tasks, own tasks 97.4% of capacity, 98.2% with CedeGrid's 104 m, no step. With warm-ups long enough for one-worker growth (all admitted after 24 and 57 minutes): Neutral 121 tasks, 92.3% and 92.9% with CedeGrid's 95 m; Yield First 108 tasks, 81.8% and 82.4% with 77 m; no step in either. Admission's p95 charges for running tasks exceed the share by 170 m and 226 m, so no further task fits | Performance pass, Neutral and Yield First fail |
| Run 3, SC-014, 4 CPUs, 100 m tasks, Performance | 36 tasks, own tasks 96.6% of capacity, 97.1% with CedeGrid's 19 m, no step | pass |
| Run 4, SC-015, Yield First, 4 CPUs, 250 m tasks beside a 60 m user | 13 tasks; own tasks 96.0% (catch-up loop) and 97.2% (steady) of capacity minus the user and the margin; no step, no pause | pass |
| Run 5, T178 pinned, contender under a second account (4 CPUs, 16 tasks of 250 m) | `ramp_seconds` 2.554; the contender reaches 0.9 core 2.5 s after it starts (8.5 s by the gate's own sampler) and holds a median of 0.90 core after 10 s, 0.9 core or more in 61% of samples; own work stays 80 m over its target; 4–14 tasks hold `own_over_target` caps of 239–253 m, about 4 change target each interval, and over 51 intervals 32 caps were relaxed to none and 35 added; `foreign_actions` 0 | contender pass, caps cycle |
| Run 6, return after contention (Yield First, 4 CPUs, 13 tasks of 250 m, one-core contender for 30 s) | own work at the contended target in the interval the contender is first observed; after its last usage the target returns in 1 interval, held caps rise in one move 5.5 s later, and own work is at 98% of the uncontended target 7.5 s after the last usage (bound 3 s); no drain | SC-002 pass, return fail |
| Run 7, loop with 129 tasks (run 3's 14-CPU Performance run) | observation interval p50 498 ms, p95 533 ms, maximum 609 ms; CedeGrid 104 m (0.74% of capacity); agent `loop_cost` per minute: sample 3.6 s, slots 1.08 s, policy 0.18 s, heartbeat 0.15 s | pass |
| GPU (Part 5) | per-process utilization readable (`NVML_SUCCESS`); the one listed context is the greeter's graphics context, under a user ID at or above `uid_min`, so it is not background; GPU activity stays unknown and admission open | as specified |

### Item 029 runs

Run on 2026-10-30 on the same test host with the commands under "Item 029 native gates" in
`specs/001-contention-modes/native-gates.md`: the launch burst at `fc5e330`, the rest at
`1c6ca7e`, whose Rust code is the same. Own throughput is with CedeGrid against the same probe
without it. Runs 1 and 5 use the `own-efficiency` profile: every default but the mode, the
coordinator and the harness on CPUs outside the scope, and the agent on the scope's CPUs, since
its scope is its own affinity. Loads of the other user ran as a second unprivileged account,
which started and stopped them; CedeGrid took no action on them. For Performance, only loss
beyond its 2% margin and the agent's measured CPU counts.

| Run | Values | Result |
|---|---|---|
| 1, idle, 2 CPUs, the half-core probe checking for a drain every 50 ms | Performance 0.995 (0.995, 0.995, 0.996), Neutral 0.999, Yield First 0.995; the agent 9 m (0.43% of the scope), so no Performance loss counts | pass |
| 2, a light other user on the probe's core (100 requests a second) | no step in any mode; own throughput 0.998 in Performance, 1.010 in Neutral, 0.998 in Yield First; the other user's p99 0.68 ms alone and 0.66–0.69 ms beside own work, with or without CedeGrid; the agent 8 m | pass |
| 3, a saturating other user on the probe's core | Performance: no step, own throughput 0.998, target 1370 m against capacity minus their 570 m minus the 40 m margin (1390 m). Neutral: no step, 0.996, target 861 m (the fair share). Yield First: caps, hints and pauses, 0.174. The other user's p99 0.33 ms alone and 0.66 ms beside the probe, with or without CedeGrid, in every mode | as specified |
| 4, a fixed allocation against CedeGrid in Performance, 4 CPUs, the other user saturating two of them 30 s on and 30 s off for 300 s | own work with CedeGrid 1.24 times that of the probes pinned with taskset to the other two CPUs; the other user's p50 while on 0.325 ms beside the fixed allocation and 0.347 ms beside CedeGrid, p99 0.356 ms and 3.33 ms; 188 CPU step changes in 300 s; the agent 9 m | own work pass; the other user's p99 is the cost |
| 5, GPU idle, 3 CPUs | Performance 1.002, Neutral 1.000; Yield First 1.001 and 1.005 in two repetitions, then its probe waited over 60 s with `gpu_yield_first_activity_unknown`: the device lists another account's idle graphics context, so GPU activity reads unknown and Yield First holds admission; the agent 16 m (0.53%) | Performance and Neutral pass |
| 5, a light (200 small multiplies a second) or saturating other GPU user on the same device, its CPU outside the scope | Performance: light 0.505, saturating 0.665. Neutral: light 0.501, saturating 0.469. Yield First admits nothing while the other user is active (`gpu_yield_first_needs_quiet_device` in every interval). Beside the light user, GPU compute reads unknown from the probe's start, when its per-process samples lag the device (35% against 72%), so own work is capped at its fair share (500); averaged over only the samples it has, the duty-capped probe then reads 56% against a device at 13–36%, so the measurement never comes back. Beside the saturating user, per-process averages truncated to whole percent leave 1% of the device unattributed, at the 1% principal floor, so it counts as a third contending principal and the fair share is 334 instead of 500 in most intervals; in Neutral the 500 duty cap leaves own work 165. The other user's p99: light 1.21 ms alone, 0.80–0.81 ms beside the probe without CedeGrid and 1.06–1.07 ms with it; saturating 0.68 ms alone and 1.37–1.38 ms beside the probe with or without CedeGrid. The agent 18–21 m (0.6–0.7%) | Performance fail; Neutral loses by the same faults; Yield First as specified |
| 6, a GPU job whose three loader threads want more CPU than its fair share, beside a saturating other user on its two CPUs | Tracked: units 1.01 and CPU 1.00 of the job without CedeGrid, GPU 69% against 70%, with caps. Full: units 0.95, CPU 0.93, GPU 69% against 71%, with a lower share. In neither does GPU throughput fall more than the CPU share explains | as specified |
| 7, report 027's run 2, 150 tasks at once | no agent exit, all 150 ran, no lease expired, no refusal and no supervisor error; the agent's soft descriptor limit raised to 524288, 1079 open at the peak; longest observation gap while launching 839 ms (p95 799 ms); after SIGTERM, the iteration that drains all 150 at once took 4236 ms, and the stop iterations after it about 1 s | launching pass; stop fail |
| 7, report 027's run 5, T178 pinned with the other user's contender | `ramp_seconds` 2.668; the contender at its core 0.51 s after it started; `foreign_actions` 0 | pass |
| 7, report 027's run 6, the return after a 30 s one-core contender, 4 CPUs | Yield First: own work at the contended target in 1 interval; after the contender's last usage the target is back in 1.5 s and own usage at 98% of its usage before in 4.0 s; 273 pause samples, no drain. Performance: the same interval, the target back in 1.0 s, own usage in 6.0 s; 7 pause samples, no drain | SC-002 pass; the target within 3 s, own usage not |

Known limit from run 4: in Performance, own work gains 24% over a fixed half-scope allocation
and the other user's median stays within 0.02 ms (0.347 ms against 0.325 ms), but their p99
grows from 0.356 ms to 3.33 ms while capped own tasks share their CPUs: rootless (Tracked)
caps are duty cycles that stop and resume own work within each period, with 188 CPU step
changes in 300 s. The loss rule protects the other user's demand and fair share, not their
tail latency. Keeping own threads off a contended CPU (affinity or `cpu.weight` before a duty
cycle) and the step churn are left to a later item.

Item 030 changed what rows 5 and 7 measured: GPU compute now counts each process over the
whole interval with rounding not a principal, and a draining stop spreads its drains over
intervals (a signal stop no longer drains). Those rows stand as recorded; the batch after item
033 below runs row 5's GPU users and row 7's return again.

### Batch after item 033

Run on 2026-10-31 on the same test host at `cd077e9`, a release build, with the commands under
"Item 029 native gates" (row 5), "Item 031 native gates", "Item 032" and "Item 033" in
`specs/001-contention-modes/native-gates.md`. Loads of the other user ran as the second
unprivileged account, which started and stopped them; the supervisor-loss runs used a light
load of the test's own account. The agent's cost is its own CPU, against 1% of its scope, the
029 own-efficiency budget.

| Run | Values | Result |
|---|---|---|
| T368, run 5 again, the GPU probe on 3 CPUs beside a light (200 small multiplies a second) or saturating other GPU user | Performance: light 0.497, saturating 0.501. Neutral: light 0.498, saturating 0.500. Own work is capped at its fair share (500) in every managed window: GPU compute reads unknown (`gpu_compute_amount_unknown`) whenever the probe or the saturating user is on the device, because the summed per-process utilization is about a third of the device's (33% against 99% beside the unmanaged probe, 19% against 55% beside the capped one, 11% against 33% with the saturating user alone). With the light user alone it is 5% against 15%, within the 15-point tolerance, and reads measured. The other user's p99: light 1.21–1.22 ms alone, 0.88 ms beside the probe without CedeGrid, 1.15–1.16 ms with it; saturating 0.68 ms alone, 1.37–1.38 ms beside the probe with or without CedeGrid. The agent 18–21 m (0.6–0.7%) | saturating as specified; light fail in both modes |
| T387 1, a multiprocessing stand-in: a leader filling 1 GiB, then 4 forked workers reading it and spinning for 120 s, 4 CPUs, Performance | Tracked and Full: a family of 5 in every decision (`family_members`), nothing outside it, no `cap_at_minimum_duty`, every member at the launch baseline `oom_score_adj` 0. Own memory 1035 MiB against the buffer plus the workers' private pages, 1029 MiB. Beside the other user saturating the 4 CPUs: own CPU 1877 m against a target of 1804 m (Tracked) and 1685 m against 1671 m (Full); the leader idle in its join. The agent 83–86 m in Tracked (2.1–2.2% of the scope, 0.43% of the host), 12 m in Full | pass, but Tracked's agent cost is over budget |
| T387 2, a PyTorch `DataLoader(num_workers=4)` (fork) over a synthetic dataset, the same way | a family of 5, nothing outside it; own CPU beside the load 1954 m against 1917 m (Tracked) and 1901 m against 1887 m (Full); own memory 303 MiB (anonymous and shared PSS) against 606 MiB of total PSS with the library pages. In Tracked, 8 of the 10 samples that caught a stop had all 5 members stopped at once, and every stop was followed by all 5 running. The agent 35 m in Tracked (0.9%), 11–12 m in Full | pass |
| T387 3, leftovers: the leader exits after 5 s while its 4 workers run on | Tracked and Full, Guaranteed and Opportunistic: `members_ended` 4, the release confirmed 7–8 ms after the leader's exit, no member, zombie or leaf left | pass |
| T396, run 7's return after a 30 s one-core contender, 4 CPUs, with the agent's decision records | Yield First: the target back 2.5 s after the contender's last usage, own usage at 98% of its level before in 9.0 s; the held caps stay at the minimum duty for 3 intervals, the quiet count then reverses them to 107–131 m against a 250 m task target, and they rise in four more steps 2 s apart; at the start own work is within 80 m of the contended target after 4 intervals (120 m above it after 1). Performance: the target and own usage back in 1.0 s, no pause | Yield First's return fails (T339 open); Performance pass |
| T406, supervisor loss in Tracked: a leader and 2 CPU workers held in an off phase by a Yield First cap beside a light load of the test's own account, the run's supervisor killed while all 3 were stopped | Guaranteed: no SIGHUP; the leader running at once (its parent-death signal is SIGCONT), every member running 0.45 s after the kill, the output complete; `family_recovered` continued 3. Opportunistic: every member gone 0.45 s after the kill, none left stopped; `family_recovered` ended 2, already gone 1 (the leader, by its parent-death signal) | pass |

The per-process GPU reading in T368 is a steady third of the device's in every window, which
fits each process's summed samples being divided by the device's sample count while the driver
returns fewer per-process samples per interval; the records do not hold the device's sample
count, so this is the likely cause, not a proven one. Tracked's agent cost in T387 1 is the
per-interval `smaps_rollup` read of each member, which walks its mapping: five processes
sharing 1 GiB cost about 43 ms per interval, where T385's 64 MiB family cost 3 ms. Full reads
the leaf instead. In T387 1's Tracked runs the harness read each member's `smaps_rollup`
between the state reads, so its samples rarely show all members stopped at once; T387 2's
read every state first.

### Rerun after item 034

Run on 2026-10-31 on the same test host at `5c49ee2`, with the commands under "Item 034: the
rerun after the batch's fixes" in `specs/001-contention-modes/native-gates.md` and the same
host rules.

| Run | Values | Result |
|---|---|---|
| T368, run 5 beside the light and saturating GPU users | Performance: light 0.497, saturating 0.501. Neutral: light 0.497, saturating 0.500 over the windows that finished; its last managed probe launched late and the run failed. `compute_sampling` shows each process with exactly 1 sample in every interval against the device's 3, so no process period was readable and every interval kept the device's count | unchanged; the fix's fallback applies on this host |
| T396, run 7's return in Yield First | only 3 of 16 tasks were admitted: the previous run's late probe, left running after its agent's signal stop (FR-080), was still on a scope CPU when admission began; own usage back in 0.51 s and the target in 1.0 s, with no held cap | not the batch's shape; no result |
| T387, the Tracked multiprocessing run with the 1 GiB family | a family of 5, nothing outside it; own memory 1035 MiB against 1029 MiB; beside the saturating user own CPU 1914 m against a target of 1849 m, and 17 of 18 stopped samples with all 5 members stopped; the agent 22 m idle and 21 m beside the load (0.56% and 0.53% of the scope, 86 m and 83 m in the batch) | pass |

On this driver the per-process samples come once per process per query, at the device's sample
times, so each process's period can only be read across intervals.

### Rerun after item 035

Run on 2026-10-31 on the same test host at `113067a` (the code of `5c49ee2`), with the return
harness ending its own run before stopping its agent and no test process of either account left
before or after the run. Run 5 waits for item 035's GPU decision.

| Run | Values | Result |
|---|---|---|
| T396, run 7's return in Yield First beside a 30 s one-core contender, 13 tasks admitted | own work at the contended target after 1 interval; after the contender's last usage the target is back in 2.5 s; at the quiet count, 2.5 s after it, every held cap rose in one move and own usage was at 97.96% of its level before 0.5 s later and at 98% in 3.5 s (9.0 s in the batch); 273 pause samples, no drain | pass under FR-034's Yield First bound since item 036: the quiet count at 32.7 s, own usage at 98% at 33.7 s, 2 intervals after it; the measured return from the contender's last usage is 3.5 s |

### Own-work ratios in the protection runs

The T117 and T118 runs use the protection profile of `tools/pressure_comparison.py`: a 2-CPU
scope with `cpu.reserve_physical_cores = 1` (the default is 0), built to test reclaim and
protection, not own-work efficiency. Their own-work ratios, 0.93–0.94 of the same probe
without CedeGrid while idle and 0.71–0.80 under contention, come from that setup:

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

Since item 029 the SDKs re-read the target file only when its stat identity changes, both
probes check for a drain every 50 ms (the unmanaged one through a no-op context), Neutral and
Performance tasks launch at nice 0, and the `own-efficiency` profile keeps every default, with
the coordinator and the harness outside the probe's scope and the placement in the report.
Report 027's run 1 and run 3 rows above stand as recorded: run 1's 4-CPU estimate is the
window's upper bound, and run 3's Neutral and Yield First fill stays below SC-014 on this host
because admission's p95 charges exceed the share (`charge_shortfall`).

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
