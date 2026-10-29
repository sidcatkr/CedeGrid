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
| T117, CPU reclaim (Yield First, Neutral, Performance) | `1c8a850` | matched throughput 0.933, 0.942, 0.933; decision p95 0.81, 0.78, 0.77 s; post-yield p99 1.05, 1.03, 1.07×; own usage at the target share 2 intervals (1.0 s) after the demand is first observed in Yield First and Neutral, 1–2 in Performance; one cap and no drain per case; manager peak 0.14–0.19 core and 94 MiB | pass |
| T117, GPU (Neutral, Performance) | `c0779d3` | GPU compute at the target share 2 intervals after the protected GPU load starts, after up to 6 caps and one pause, no drain; decision p95 0.07 and 0.60 s; post-yield p99 0.77×; matched throughput 0.923 in Performance but 0.723 in Neutral, where each new 20 s job reads as Unknown GPU activity at start, is capped at the fair share and stays capped through the cooldown | Performance pass, Neutral fail |
| T117, SC-014 (4 CPUs, 500 m tasks, idle) | `374637d`, `c0779d3` | own CPU 87.5% of capacity in Neutral and Performance, 75.0% in Yield First (targets 94%, 97% and 84%); the decisions read own work about 330–430 m low and count that as external; with 500 m tasks own use moves in 500 m steps, so this run cannot reach the targets even with exact measurement | fail |
| T117, SC-015 (beside a 60 m external user) | `374637d`, `c0779d3` | own CPU as a share of capacity minus the user and the margin: 93.6% in Neutral, 90.7% in Performance, 89.8% in Yield First (target 90%); no step change in 5 minutes | Neutral and Performance pass, Yield First fail |
| T118, SC-005 audit | `1c8a850` (CPU), `c0779d3` (GPU) | CPU and GPU, at the defaults and with Performance and a 512 MiB floor: 0 actions and 0 drains outside own allocations, protected snapshots unchanged, every protected run completed, no failed external GPU allocation, every owned GPU release confirmed, 0 OOM lines | pass |

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
