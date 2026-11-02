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
- **GPU.** Verified on one consumer device only, with no MPS or MIG (see
  [Native gate results](#native-gate-results-feature-001)). Other devices and drivers are not
  verified. On that device, where own work and another user's busy context share the device in
  time slices, own work holds the time-slice duty, and T456's gates pass in Performance and
  Neutral beside a saturating user and in Performance beside a light one. Tracked's duty stops
  and resumes own processes; Full's freezes each own leaf through `cgroup.freeze`. Full ran only
  Performance beside the saturating user, at item 047 (see
  [Item 047: SC-014's fill and Full's GPU path](#item-047-sc-014s-fill-and-fulls-gpu-path)).
  A zero GPU compute margin, Aggressive's GPU behavior, leaves the saturating user 0.3–6.5
  requests a second short of half its rate alone. For feature 001 that is a record, not a gate.
  Since item 027 an idle context of a user ID below `uid_min` is background while per-process
  utilization is readable. The test host's greeter runs at or above `uid_min`, and at item 029
  its idle graphics context held Yield First's admission on
  `gpu_yield_first_activity_unknown`. At item 048 Yield First still missed the comparison's 60 s
  readiness after the harness's own unmanaged probe: admission opened 65–101 s after the agent
  started. The GPU attribution tolerance
  stays 15 points until a second device is measured; the first device's largest gap was 5.

  Freshness is never inferred from aggregate utilization or from an NVML `NotFound` response.
- **Memory.** Item 048 ran the memory paths natively for the first time (see
  [Item 048](#item-048-native-checks-at-head)).
  - SC-012's RAM reclaim and the floor breach by mode pass.
  - The OOM order passes: in-share own work keeps its launch value, and only the marked task is
    at 1000.
  - In three host-wide OOMs the second account caused, the kernel took only its allocator.
  - SC-014's RAM and GPU memory fill miss as written. The host's own 1.9 GiB at idle and the GPU
    memory `noise_tolerance` (about 380 MiB) count against the fill; SC-014 subtracts neither.
  - No native result exists for SC-012's GPU memory reclaim, or for a kernel OOM kill of an own
    task caused by others, since CedeGrid gave the excess back first each time.
- **Burst stop.** At item 048 a draining stop of 150 tasks takes 9.3 s and spreads its drains, with
  a longest gap of 1135 ms between observations against the 1 s bound; at `fc5e330` one iteration
  took 4236 ms.
- **Hosts.** CedeGrid has run natively on one test host only, with one consumer GPU and no MPS or
  MIG. It has never run on the shared host.
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
- **Idle utilization.** At item 047's commit, on 14 CPUs with 100 m tasks, own tasks reach
  96.8% of capacity in Performance (97.6% with CedeGrid), 92.3% in Neutral (93.1%) and 82.7% in
  Yield First (83.3%), against SC-014's 97%, 94% and 84%. In Neutral and Yield First, admission
  charges each running task at its p95 usage (FR-034), and the steady tasks' p95 is above their
  mean, so the charges fill the share 696 m and 609 m above the tasks' use (`held_back` reports it
  as `charge_shortfall`); charged at their use, own tasks plus CedeGrid would reach 94.3% and
  84.1%. On 4 CPUs Performance reached 97.1% with CedeGrid at item 027. Item 027 measured
  92.3% and 81.8% with the same cause, and before it the same scope
  reached 91.4%, 87.0% and 73.0% (see
  [Item 047](#item-047-sc-014s-fill-and-fulls-gpu-path) and
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

### Rerun after item 036

Run on 2026-10-31 on the same test host at `5f729bc`, with the commands under "Item 036: T368
with the sample span (T433)" in `specs/001-contention-modes/native-gates.md`. No test process of
either account was left before the first run or after any run.

| Run | Values | Result |
|---|---|---|
| T368, run 5 beside the light (200 small multiplies a second) and saturating GPU users | GPU compute reads measured in every interval of every managed window (74–80 of 80 alone, 0–6 over the last 4), where the earlier runs read unknown whenever the probe was on the device. Performance: light 0.834 (the two managed windows 0.77 and 0.90), saturating 0.628. Neutral: light 0.642, saturating not measured: its first managed probe launched 9.8 s into its window, held by Neutral's CPU admission cooldown after the unmanaged probe stopped, and the harness read its result before it ended. Beside the light user, own work is duty-capped in 72–75 intervals per managed window. That user reads 13–16% alone and mostly 26–33% beside own work, 61–77% in some intervals, and the target follows it. Its latency: p50 0.77 ms with and without CedeGrid; p99 1.21–1.23 ms alone, 0.88–0.89 ms beside the probe without CedeGrid, 1.15–1.16 ms with it. The saturating user ran 750 requests a second beside the probe without CedeGrid and 1023–1189 with it. Replaying each run's records through the code gives the recorded reading in 1609 of 1610 intervals. The other one is a window's third interval, where the agent compared over intervals before the window that the replay does not carry. The agent 18–20 m (0.6–0.7%) | measured; light fail in both modes; saturating as specified in Performance, Neutral not measured |

### Item 037 and 038 runs

Run on 2026-10-31 on the same test host at `8e16cc1`, with the commands under "Item 037: the
light-user diagnostic and T368's Neutral saturating case" in
`specs/001-contention-modes/native-gates.md`, the probe-alone level and the device's clocks
sampled every 100 ms. No test process of either account was left before the first run or after
either run.

| Run | Values | Result |
|---|---|---|
| The light-user diagnostic: the GPU probe unmanaged at fixed duty levels (500 ms period) beside the light user (200 small multiplies a second), 120 intervals per level, no managed run | The probe alone 1572 units/s; beside the light user at 100% 1312 and, at the end, 1302 (0.835 and 0.828 of alone). The 70% and 40% levels ran at 89.8% busy each, 1172 and 1168 units/s: the probe idled only 50 ms of each period, a fault fixed in `c52ba5c`. The light user's request rate stayed at 200 a second at every level. Its mean per-interval reading: 158 m alone, sampled in every interval at 15–16%; 120 m and 78 m beside the probe at 100%, sampled in 47 and 34 of 120 intervals at 33% or 66%; 170 m and 160 m beside the probe at about 90%, sampled in 63 and 57 intervals. Its p50: 1.11 ms alone, 0.77–0.78 ms beside the probe; p99: 1.22 ms alone, 0.88 ms beside the probe at 100%, 1.01–1.03 ms beside it at about 90%. The device stayed in P2 with its memory clock at 3802 MHz. The SM clock read 1506 MHz with the light user alone (30 W) and 1873–1911 MHz with the probe (80 W) | the light user costs own work 16.5% at full duty; its reading beside own work comes in 33% steps; 70% and 40% not measured |
| The diagnostic again at `c52ba5c`, the probe's duty fixed | The probe alone 1573 units/s; at 100% 1311 and 1304 (0.833 and 0.829 of alone); at 70% (busy 0.699) 912; at 40% (busy 0.399) 520. The light user's request rate stayed at 200 a second. Its mean per-interval reading: 158 m alone; 130 m and 77 m beside the probe at 100%; 251 m at 70% and 264 m at 40%, sampled in 112 and 120 of 120 intervals. Its p99: 1.22 ms alone, 0.88 ms beside the probe at 100%, 1.06 ms at 70% and 1.07 ms at 40%. The SM clock read 1885–1898 MHz at every probe level and 1506 MHz with the light user alone, in P2 throughout | the reading rises as own work's duty falls, at a constant rate: inflation, not demand; the gaps also raise its p99 |
| T368, Neutral beside the saturating GPU user, with run 5 waiting for each probe's result | 0.513: managed 386 and 366 units/s against unmanaged 736 and 729; the managed probes launched 1.7 and 1.8 s into their windows; GPU compute measured in every interval; the saturating user 1132–1146 requests a second beside the managed probe, 747–754 beside the unmanaged one | as specified |

Read under SC-017 (item 040), which judges own work against the same probe alone on the device
and the other user against its rate without CedeGrid and its fair share alone, the saturating
results above are over-yields, not passes. The probe alone ran 1572–1573 units a second on this
device. Beside the saturating user without CedeGrid it ran 729–736, 0.46–0.47 of alone, so
SC-017's bound is about 0.44. With CedeGrid it ran 0.27–0.32 of alone in 036's Performance
saturating run (0.628 against without) and 0.23–0.25 in 038's Neutral saturating run (0.513),
while the saturating user ran 1023–1146 requests a second against 747–755 without CedeGrid: own
work gave that user more than its fair share. The rows keep their values; the gap is a GPU
compute cap's duty of target ÷ 1000, which item 040 replaces.

### T445 after items 039 and 040

Run on 2026-11-01 on the same test host at `c581f4a`, with the commands under "Item 039 and 040:
T368 with GPU demand from uncapped intervals and the duty from own share (T445)" in
`specs/001-contention-modes/native-gates.md`. Each run starts with the probe alone, then the other
user alone, then the unmanaged and managed windows in turn, 80 intervals each. No test process of
either account was left before the first run or after any run. Own work's bound is SC-017's, at
the target the mode sets for the two principals on the device: 500 beside the saturating user,
Neutral's 792 and Performance's recorded targets beside the light user. The other user's bound
is the smaller of its rate without CedeGrid and half its rate alone, less 3%.

| Run | Values | Result |
|---|---|---|
| T368, Performance beside the light user | The probe alone 1573 units/s; without CedeGrid 1320 and 1303 (0.834 of alone); with it 1305 and 1294 (0.830 and 0.823 against bounds of 0.736 and 0.803 at targets 766 and 833); the old ratio 0.991. Duty median 0.998 and 1. GPU compute measured in every interval. The light user ran 200 requests a second in every window; p50 1.10 ms alone, 0.77 ms without and with CedeGrid; p99 1.22 ms alone, 0.82–0.88 ms without, 1.13–1.15 ms with | pass |
| T368, Performance beside the saturating user | The probe alone 1559; without 736 and 731 (0.470); with 546 and 571 (0.350 and 0.366 against 0.44); the old ratio 0.761. The other user 1556 requests a second alone, 749–754 without, 978 and 932 with (bound 729); p50 1.36 ms and p99 1.37–1.38 ms with and without. First managed window: three intervals whose per-process samples summed 77% against the device's 99% put GPU compute on the unknown path for 43 of 80 intervals, 40 of them as recently inconsistent; 41 caps were set for unknown activity at 500 and a duty of 0.5, the others ran at 0.99. Second window: the target fell to 334 at 10.7 s and held for 59 decisions, at a duty of 0.706 | own work fails; the other user passes |
| T368, Neutral beside the light user | The probe alone 1551; without 1303 and 1298 (0.838); with 825 and 759 (0.532 and 0.489 against 0.762); the old ratio 0.609. The light user 200 requests a second; p50 1.10 ms alone, 0.77–0.78 ms without and with; p99 1.21 ms alone, 0.89 ms without, 1.13–1.18 ms with. First window: own work's share was learned as 570 from two intervals, so a cap at the watch's 500 ran at a duty of 0.877; own work read 880–890, above that target, and was paused for 20 intervals. Second window: the target was 334 from the first cap on (54 decisions), with a share of 443 from three intervals and a duty of 0.753 | own work fails; the light user passes |
| T368, Neutral beside the saturating user | The probe alone 1556; without 735 and 729 (0.470); with 654 and 618 (0.420 and 0.397 against 0.44); the old ratio 0.869. The other user 1551 requests a second alone, 747–753 without, 849 and 880 with (bound 728); p50 1.36–1.37 ms and p99 1.37–1.38 ms with and without. First window: the target fell to 334 at 15.8 s and held for 49 decisions, at a duty of 0.839. Second window: a share of 594 from three intervals set a duty of 0.841; after a re-measure window at 34 s the estimate restarted and a share of 660 set 0.757 | own work fails; the other user passes |

Against the earlier runs the old ratio rose from 0.834 to 0.991 (Performance light), from 0.628 to
0.761 (Performance saturating) and from 0.513 to 0.869 (Neutral saturating), and fell from 0.642
to 0.609 (Neutral light). Each decision and the agent's state database show three causes:

- **The 334 targets.** The fair share counts external usage that no UID accounts for as one more
  principal once it reaches the contention floor, 1% on GPU compute, and on a saturated device
  that principal's demand is unknown. In the estimate's averaged view the external total and the
  per-UID averages drift apart. The device-level external amount exceeds the other user's
  per-process samples in time-sliced intervals (770 against 660). The total also keeps the
  owner's unmanaged usage, which the per-UID amounts leave out: Neutral light's second estimate
  opened with the interval in which the unmanaged probe exited, 740 of 880. The mean gap reached
  10 at the decision where Performance saturating's target fell to 334, and about 20 in Neutral
  light's second window. While own work is capped below full duty no interval feeds the estimate,
  so the gap held for the rest of each window.
- **Own work's share.** It is learned from the first two to five uncapped intervals after own
  work starts, whose readings come in steps of about 330, and is kept while own work is capped.
  It read 443–660 in the Neutral runs, where Performance light, with 20 and 65 such intervals,
  learned 767 and 843. Too low a share sets a duty above what the target needs, too high a share
  one below it.
- **The unknown path.** In Performance saturating's first window, 41 caps for unknown activity
  ran at a duty of target ÷ 1000.

The rows keep their values; the causes wait for a decision.

### T456 after item 041

Run on 2026-11-01 on the same test host at `36bfe68`, with the commands under "Item 041: T368
and two own processes under the time-slice rule (T456)" in
`specs/001-contention-modes/native-gates.md`. No test process of either account was left before
the first run or after any run. Own work's bound is SC-017's at the mode's target for the
principals actually on the device (Q26):
- 500 beside the saturating user;
- beside the light user, the device less that user's estimate alone and the mode's margin
  (Neutral 792, Performance 823).

| Run | Values | Result |
|---|---|---|
| T368, Performance beside the light user | The probe alone 1572 units/s; without CedeGrid 1319 and 1303 (0.834 of alone); with it 1306 and 1298 (0.831 and 0.826 against 0.793); the old ratio 0.993. No GPU compute step in either managed window; the time-slice rule held in all 80 decisions of each, mostly at m = 1, E = 1, k = 1. The light user ran 200 requests a second; p50 0.77 ms with and without CedeGrid, p99 0.88–0.89 ms without and 1.08–1.10 ms with | pass |
| T368, Performance beside the saturating user | The probe alone 1560; without 736 and 731 (0.470); with 736 and 731 (0.472 and 0.469 against 0.44); the old ratio 1.000. No step; the rule held in all 80 decisions of each window, 21 of the first window's on the unknown path with m = 1. The other user 1555 requests a second alone, 749–755 without, 775 and 767 with (bound 729); p50 1.36–1.37 ms and p99 1.37–1.38 ms with and without | pass |
| T368, Neutral beside the light user | The probe alone 1552; without 1304 and 1295 (0.837); with 1298 and 1290 (0.836 and 0.831 against 0.762); the old ratio 0.996. No step; the rule held in all 80 decisions of each window. The light user 200 requests a second; p50 0.77–0.78 ms, p99 0.88–0.89 ms without and 0.94–0.98 ms with | pass |
| T368, Neutral beside the saturating user | The probe alone 1557; without 736 and 731 (0.471); with 731 and 731 (0.469 and 0.469 against 0.441); the old ratio 0.997. No step; the rule held in all 80 decisions of each window. The other user 1552 requests a second alone, 749–754 without, 761 and 838 with (bound 729); p50 1.37 ms and p99 1.37–1.38 ms with and without | pass |
| Performance and Neutral, two own probe processes beside the saturating user | Without CedeGrid the two probes ran 0.660–0.662 of the pair alone (973–977 of 1474–1475 units/s), and the other user 504–507 requests a second, 0.33 of its 1548 alone: each busy context's equal slice. With CedeGrid the harness's cluster allowed one worker, the pool's and the agent's `max_workers` of 1, so the second probe's job ran only after the first ended. Each managed window therefore held one own process at a time, at m = 1, and the rule held: no step, the other user 760–761 requests a second, half its rate alone. The late tasks moved the second managed window past the end of the other user's load. A rerun with one worker per probe ran them one after the other again. Each managed probe asked for 1024 MiB of VRAM, and beside the device's 4096 MiB floor, on 6144 MiB, the headroom was 1797 MiB before the first and 773 MiB after it. Each probe used about 122 MiB. The unmanaged windows read 0.655–0.660 of the pair alone, and the other user 0.33. Item 042 ran them again at `71f0874` with 512 MiB requests, twice (the second with a common start for both probes and a longer wait for their results). Every valid managed window ran both probes together at m = 2, E = 1, k = 1, and own work fell below SC-017's 0.47:

- **Performance:** 0.13 and 0.17 of the pair alone over the probes' runs, with the other user at 1319 and 1034 requests a second (bound 751). Caps fell as low as 0 and 5 on single interval readings of 990. From 9.8 s and 24.5 s own work was paused through the rest of the window, so the probes ran 43.5 s and 73.7 s for their 40 s, and the harness stopped each run. Performance's target stayed at 659, the device less the estimate's 330 and the margin.
- **Neutral:** 0.337 in its valid window, with the other user at 1022 (bound 751); caps fell to 15–20 and own work was paused in most decisions. Its other managed windows started the probes 11.8 s and about 12 s apart, so they do not count | fail: own work falls below SC-017 when it holds more than its share of contexts, and the other user passes |

Against T445 the old ratio rose from 0.991 to 0.993 (Performance light), from 0.761 to 1.000
(Performance saturating), from 0.609 to 0.996 (Neutral light) and from 0.869 to 0.997 (Neutral
saturating). The light user's p99 beside own work fell from 1.13–1.18 ms to 0.94–1.10 ms. Its p50
and request rate were unchanged.

### T456 after item 043

Run on 2026-11-01 on the same test host at `9467f8a`, with the commands under "Item 043: T456's
remaining runs under the time-slice duty" in `specs/001-contention-modes/native-gates.md`: two own
probe processes, each its own job with 512 MiB, submitted 40 s before each managed window. No test
process of either account was left before the first run or after any run. Every managed window
passed 042's checks: both probes ran together (overlap 39.8–40.1 s), m = 2 in 72–78 decisions,
and the window lay inside the other user's load. No window paused own work.

| Run | Values | Result |
|---|---|---|
| Performance, beside the saturating user | Pair alone 1497 units/s; without CedeGrid 0.657 of it, the other user 510 requests a second (0.33 of its 1569 alone). With it: own work 0.522 and 0.477 (bound 0.47); the other user 720.5 and 781.2 (bound ½ × alone × 0.97 = 760.9). The first window's duty was d0 = 750 in all 72 decisions; the second's 750 in 34 and 666 in 44 from the listing path | own work passes; the other user fails in the first window (0.459 of alone) |
| Neutral, beside the saturating user | Pair alone 1477; without 0.661, the other user 507 (0.33 of 1550). With it: own work 0.495 and 0.515 (bound 0.47); the other user 761.3 and 724.2 (bound 751.5). The duty was 750 in 51 decisions and 666 from the listing path in 27 in the first window, 750 in 76 of the second | own work passes; the other user fails in the second window (0.467 of alone) |
| Performance, beside the light user | Pair alone 1474; without 0.863, the light user 200 requests a second, p99 2.73 ms. With it: own work 0.813 and 0.806 (bound 0.792, the target 822); the light user 200 and 200, p99 2.727 and 2.728 ms (bound 3.006). No principal was slice-bound in 57 decisions of each window; the listing path set d0 666 in 21, and the duty stepped up to 866 | passes |

At d0 = 750 the duty gave own work 0.515–0.522 of the pair alone rather than the 0.5 that equal
slices predict, and the saturating user 0.459–0.467 of its rate alone against the 0.485 its gate
needs. d0 leaves no part of the mode's margin. The windows that passed spent 27–44 decisions at
666, which the listing path set from an idle context of another principal that the device lists
throughout. T456 stays open.

### T456 after item 044

Run on 2026-11-01 on the same test host at `f81a7c0`, with the commands under "Item 044: T456
under aligned duty phases and the margin at d0" in `specs/001-contention-modes/native-gates.md`:
two own probe processes, each its own job with 512 MiB, submitted 40 s before each managed window.
No test process of either account was left before the first run or after any run. Every managed
window passed 042's checks (overlap 39.8–40.0 s, m = 2 in 75–78 decisions, inside the other
user's load), and no window paused own work. The harness read each managed probe's run state from
`/proc` about every 10 ms.

| Run | Values | Result |
|---|---|---|
| Performance, beside the saturating user | Pair alone 1497 units/s; without CedeGrid 0.657 of it, the other user 511 (0.33 of its 1568 alone). With it: own work 0.490 and 0.488 (bound 0.45); the other user 780.4 and 762.7 (bound 760.6). d0 720 from the window's busy counts in every duty decision; the probes' off-phases overlapped 1.0 and 0.997, the duty held 0.735 and 0.745 | passes |
| Neutral, beside the saturating user | Pair alone 1476; without 0.663, the other user 507 (0.33 of 1550). With it: own work 0.468 and 0.471 (bound 0.42); the other user 804.8 and 793.9 (bound 751.5). d0 675 in every duty decision; overlap 0.996 and 0.998, the duty held 0.704 and 0.713 | passes |
| Performance, beside the light user | Pair alone 1474; without 0.864, the light user 200 requests a second, p99 2.73 ms. With it: own work 0.864 and 0.863 (bound 0.791), never capped (no principal slice-bound in any decision); the light user 200 and 200, p99 2.728 and 2.730 ms (bound 3.005) | passes |
| Aggressive (Performance with no GPU compute margin), beside the saturating user, for the record | Pair alone 1482; without 0.660. With it: own work 0.518 and 0.499 (bound 0.47); the other user 724.6 and 746.8 (½ × 1552 × 0.97 = 752.7). d0 750; the duty stepped up to 850 and 800 on intervals the device read below 100% busy, and back | the other user falls short in both windows; not a T456 gate |

The duty held is 1 − each probe's stopped share, which includes the re-measure window that lifts
the cap for 3 s of each window. Own work's share matched 2 × the duty held ÷ 3 within 0.009, and
never exceeded it, so no drain after a stop shows at this resolution. All of T456's gates pass.

### Item 045: the step floor with a zero margin

Run on 2026-11-01 on the same test host at `2fa6587`, with the commands under "Item 045: the step
floor with a zero margin" in `specs/001-contention-modes/native-gates.md`: two own probe
processes, each its own job with 512 MiB, submitted 40 s before each managed window. No test
process of either account was left before the first run or after any run. All four managed
windows passed 042's checks, no window paused own work, and the probes' off-phases overlapped
0.995–0.998.

| Run | Values | Result |
|---|---|---|
| Zero margin (Performance with no GPU compute margin), beside the saturating user | Pair alone 1497 units/s; without CedeGrid 0.658 of it, the other user 511 (0.33 of its 1569 alone). With it: own work 0.511 and 0.506 (bound 0.47); the other user 740.5 and 734.7 (½ × alone × 0.97 = 760.8). The duty held at d0 = 750 in all 77 duty decisions of each window, with no rise or fall; the probes were stopped 0.226–0.228 of the time, the duty held 0.772–0.774 with the re-measure window | own work passes; the other user falls short in both windows (0.472 and 0.468 of alone) |
| Performance, beside the saturating user (regression check) | Pair alone 1477; without 0.661, the other user 507 (0.33 of 1549). With it: own work 0.496 and 0.497 (bound 0.45); the other user 761.0 and 752.1 (bound 751.4). d0 = 720 held throughout, no step; the duty held 0.748 and 0.752 | passes |

The step floor removed the zero-margin rises: 044's run of the same shape rose 6 times on
dwells floored to 99, this one never. What leaves the other user short at a zero margin is the
re-measure window, 3 s of each 40 s managed window at full duty, which lifts own work's held
duty from 0.75 to about 0.773, and the time-slicing's own cost of about 1%, both of which fall on
the other user while no margin absorbs them.

### Item 046: own work pays for the re-measure windows

Run on 2026-11-01 on the same test host at `23e386e`, with the commands under "Item 046: own work
pays for the re-measure windows" in `specs/001-contention-modes/native-gates.md`: two own probe
processes, each its own job with 512 MiB, submitted 40 s before each managed window. No test
process of either account was left before the first run or after any run, no window paused own
work, no duty stepped, and the probes' off-phases overlapped 0.995–0.999. Neutral's first run had
no valid window: a moment of unknown GPU observation held the second probe's launch until the
first had ended, and the delay took the second managed window past the other user's load. It ran
once more.

| Run | Values | Result |
|---|---|---|
| Zero margin (Performance with no GPU compute margin), beside the saturating user | Pair alone 1497 units/s; without CedeGrid 0.658 of it, the other user 511 (0.33 of its 1569 alone). With it: own work 0.498 and 0.493 (bound 0.47); the other user 760.7 and 754.5 (½ × alone × 0.97 = 761.0). d0 750, w 91, d0' 725 in every duty decision; the duty held 0.754 in both windows | own work passes; the other user falls short by 0.3 and 6.5 requests a second (0.485 and 0.481 of alone) |
| Performance, beside the saturating user | Pair alone 1478; without 0.662, the other user 508 (0.33 of 1550). With it: own work 0.481 and 0.460 (bound 0.45); the other user 784.4 and 809.5 (bound 751.5). d0 720, d0' 692; the duty held 0.725 and 0.697 | passes |
| Neutral, beside the saturating user (the run again) | Pair alone 1487; without 0.659, the other user 508 (0.33 of 1554). With it: own work 0.447 and 0.481 (bound 0.42); the other user 833.1 and 780.5 (bound 753.7). d0 675, d0' 643; the duty held 0.677 and 0.726 | passes |

With the windows paid by own work, the zero-margin duty held 0.754 against d0 0.75, where 045
held 0.773, and the other user rose from 0.468–0.472 to 0.481–0.485 of its rate alone. Own work
and the other user then split the device's time evenly, and the other user still holds 0.962–0.970
of its half by throughput, against 0.98 of its third without CedeGrid.

For feature 001 a zero margin stays a record, not a gate; measuring the duty's switch cost, and
folding it into d0 if it is CedeGrid's, goes to feature 004's backlog before Aggressive ships.

### Item 047: SC-014's fill and Full's GPU path

Run on 2026-11-02 on the same test host with the code of `a7f472e` (the build of `23e386e`, whose
Rust code is the same), with the commands under "Item 047" in
`specs/001-contention-modes/native-gates.md`. No test process of either account was left before
the first run or after any run.

**Full's GPU path.** The agent records of items 044 and 046 show that every T456 run there ran
Tracked (`cgroup.enabled = false`), so the leaf freeze had run with fixture leaves only. This run
put the agent in a delegated user scope with an empty delegated root and `cgroup.freeze`
authorized: two own probe processes, each its own job with 512 MiB, submitted 40 s before each
managed window. The harness read each probe's leaf `cgroup.events` `frozen` and its `/proc`
state about every 10 ms. The first attempt lost the other user's latencies when the harness's
connection to the host dropped; it ran once more.

| Run | Values | Result |
|---|---|---|
| Performance in Full, beside the saturating user | Pair alone 1496 units/s; without CedeGrid 0.657 of it, the other user 510 (0.33 of its 1567 alone). With it: own work 0.480 and 0.470 (bound 0.45); the other user 790.6 and 791.0 (bound 494.4; ½ × alone × 0.97 = 760.2). Both windows pass 042's checks (overlap 39.8 s, m = 2 in 74 and 76 decisions, inside the other user's load), no pause, no step; d0 720, w 91, d0' 692 in every duty decision. Each probe ran in its own leaf under the delegated root; the off phase was the leaf freeze alone (frozen 0.275 and 0.282 of the time, `/proc` stopped 0), the probes' off phases overlapped 0.993 and 0.996, the duty held 0.725 and 0.718, and own work's share matched 2 × the duty held ÷ 3 within 0.009. No leaf was left | passes |

**SC-014 at HEAD.** Report 027's run 3: 14 CPUs, 100 m tasks, the steady loop. Performance is
the control; Neutral and Yield First get warm-ups long enough for one-worker growth. CedeGrid's
own processes are stated apart. The counted runs ran from a host-side script with no login to the
host during them.

A first batch polled the host every 30 s. Each login's processes counted as the test account's
unmanaged usage, about 300 m for an interval, and in Yield First each one restarted the admission
cooldown, so it admitted one task. Run again, the harness's stability windows (25 s and 75 s) then
ended Neutral's and Yield First's warm-ups at 8 tasks and 1 task. The counted Neutral and Yield
First runs waited 300 s and 600 s of unchanged admission; Yield First's warm-up reached its
4800 s cap, 300 s after its last admission.

| Run | Values | Result |
|---|---|---|
| SC-014, Performance | 128 tasks, all admitted after 14 s; own tasks 96.8% and 96.7% of capacity in two runs, 97.6% and 97.5% with CedeGrid's 111 m and 109 m (0.8%); the share was capacity less the 2% margin, CedeGrid and the external estimate (59 m); no step | pass |
| SC-014, Neutral | 121 tasks, all admitted after 24 min; own tasks 92.3% of capacity, 93.1% with CedeGrid's 110 m; target share 13095 m against own use of 12921 m, with the charges above usage (`charge_shortfall`) 696 m at the median, so no further task fits; no step | fail (94%) |
| SC-014, Yield First | 109 tasks, the last admitted after 75 min; own tasks 82.7% of capacity, 83.3% with CedeGrid's 93 m; target share 11676 m against own use of 11549 m, `charge_shortfall` 609 m; no step | fail (84%) |

Item 027 measured 92.3% and 81.8% of capacity (92.9% and 82.4% with CedeGrid) with the same cause.
Charged at their usage, own tasks plus CedeGrid would fill each share to 94.3% and 84.1%. During
the warm-ups admission was open in 81% and 70% of the decisions and cooling down in the rest. An
own task's measurement went unknown in 0.1% and 0.8% of them, mostly while Yield First ran its
first tasks, and each time the external estimate rose by about 250 m for an interval.

### Item 048: native checks at HEAD

Run on 2026-11-02 on the same test host at `9c01943`, with the commands under "Item 048" in
`specs/001-contention-modes/native-gates.md`. The test deployment's floors are 16 GiB of RAM and
4096 MiB of GPU memory, and the host's own processes hold about 1.9 GiB of RAM at idle. No test
process of either account was left after any run.

| Run | Values | Result |
|---|---|---|
| B1, a 150-task burst, then a draining stop (operator drain, then TERM) | All 150 ran; every task drained or released; the drain took 9.3 s; no lease expired, no refusal, no supervisor error. The longest gap between observations was 1135 ms, against one 4236 ms iteration at `fc5e330`. Nine gaps were over 1 s, eight of them during the drain. | fail: 1135 ms against 1 s |
| B2, Yield First on the idle GPU (report 029 run 5's idle row), three times | The probe never became ready within the harness's 60 s. The comparison's own unmanaged probe kept the device busy first (`gpu_yield_first_needs_quiet_device`), then activity read unknown for 18–55 s (`gpu_yield_first_activity_unknown`) although GPU compute read measured. Admission opened 65, 101 and 74 s after the agent started. | fail |
| B3, SC-014's GPU memory fill (capacity less the floor: 2048 MiB) | Performance: own GPU memory 1856 MiB (90.6%). Yield First: 1454 MiB against 1638 MiB less its margin (88.8%). Both are held back by `noise_tolerance` (381–383 MiB), which SC-014 does not subtract. The device's only other context was the greeter's, 2 MiB. | fail |
| B4, the OOM order without an OOM (a 3 GiB capacity) | The second account's 1.5 GiB took the fair share from 1139 MiB to 558 MiB. Only the task launched last went to `oom_score_adj` 1000; the in-share task kept its launch value 0. When the second account released 3 s later, the share was back at 1038 MiB, and the same process went back to 0 before any drain. With `ram.prefer_own_oom_victim` every own task was at 1000 from launch. | pass |
| B5, an own task past its leaf's `memory.max` (Full, 256 MiB) | With host swap, the kernel swapped the excess, and the task ran at 262 MiB resident to its end. With `memory.swap.max = 0` on the test's own delegated root it was killed three times in its leaf (host `oom_kill` +3). Each kill was recorded as `oom_kill` with `own_fault`, the fair share being unknown before its first memory reading, and counted as a failure. After three attempts the task stood at `needs_reconciliation`. No leaf was left. | pass |
| C1, SC-014's RAM fill | Performance: own RAM 12763 MiB, 85.8% of capacity less the floor, 98.3% of its target of 12982 MiB (capacity less the floor and the host's own use), held back by `peak` 175 MiB. Yield First: 9990 MiB, 83.9% of capacity less the floor and margin, 99.9% of its 10001 MiB target. | fail as written; Yield First within 1% of its target |
| C2, SC-012's RAM reclaim: after a Performance fill the second account grows by 4 GiB in about 1.1 s | From the breach to the floor restored: 4.43 s with plain tasks (bound 5.5 s), 5.99 s with hint-ready tasks (bound 10.5 s), 1.04 s with Guaranteed hint-ready tasks (bound 15.5 s). The second account got all its memory each time. | pass |
| C3, a floor breach the second account causes, growing by 10 GiB | Performance: own RAM fell from 12764 MiB to 6172 MiB in 2.3 s and stopped at its fair share of about 6370 MiB, with the floor breached until the second account released 90 s later. Neutral: from 12701 MiB to 2012 MiB in 4.1 s, below its fair share of about 6360 MiB, and the floor restored in 3.1 s. Yield First: from 9912 MiB to 760 MiB in 3.2 s and then 0; free memory stayed 1318 MiB above the floor. | pass |
| C4, a host-wide OOM the second account causes, beside six in-share tasks and one over-share task, three runs | The kernel took one process in each run, the second account's allocator (24–28 GB resident), and no own task. CedeGrid gave the excess back 7–9 s before each OOM. With SDK tasks, all seven got the save-progress signal at the breach, while own memory was within its share, and the task over share was marked and freed on its hint. With an over-share task that ignores hints, the in-share SDK tasks freed instead, so own memory came back within its share and the mark ended while that task still held 3 GB. With no SDK task, the over-share task, marked at `oom_score` 1385 against the allocator's 756, was drained 2.4 s later and started again after the OOM. The second account's processes kept `oom_score_adj` 0. | in-share work pass; no own task was killed, so the kernel's preference and the requeue after a kill were not exercised |

B1's stop spreads its drains over iterations, but iterations during it still run just over 1 s. B2
holds because Yield First waits for a quiet device and then reads activity as unknown after own
work has just used it. In C4 hint-ready tasks pay first, whichever task holds the excess: the
ladder hints the tasks that can hear hints, in victim order, and skips the rest.

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
