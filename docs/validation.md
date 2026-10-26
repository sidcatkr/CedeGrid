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
  [Native gate results](#native-gate-results-feature-001)). Other devices and drivers, and a
  native Performance ramp on GPU compute, are not verified. In Neutral, a short GPU job runs
  capped at the fair share after it starts, because its new process reads as Unknown GPU
  activity.

  Freshness is never inferred from aggregate utilization or from an NVML `NotFound` response.
- **Performance ramp.** On CPU, the ramp reaches 90% in under 3 s on one Linux host, but a
  CPU-bound contender started mid-ramp gets about 0.72 of the core it wants, so SC-004's second
  bullet is open (see [Native gate results](#native-gate-results-feature-001)).
- **Idle utilization.** SC-014 is not met on the test host: own CPU work reaches 87.5% of the
  scope in Neutral and Performance and 75% in Yield First, and the decisions read own CPU
  about 12% low (see [Native gate results](#native-gate-results-feature-001)).
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
| T141e, PSI | `4d631d0`, `893c1c2` | (1) status available, 10 external cgroups, external avg10 0; (3) 40 busy loops on 20 CPUs in own session: own avg10 up to 67.7, external 0, busy 20,000 of 20,000 m, no `saturated_by_psi` in 30 samples; (2) not run: it needs a load under another account or in a system slice | open |
| T178, Performance ramp (4 CPUs, 16 tasks of 250 m) | `893c1c2`, `1c8a850` | `ramp_seconds` 2.784 idle, 2.728 and 2.762 with a contender (target 10 s); the contender, one busy loop started at 3 s, gets a median of 0.72 core after 10 s pinned and unpinned, and 0.9 core in 7% and 0% of half-second samples; `foreign_actions` 0 | ramp pass, contender fail (SC-004) |
| T088, GPU on an occupied device | `893c1c2` | owned-context test passes; a fresh external CUDA context takes 74 MiB, so the 512 MiB minimum stays; with an idle external context, 100 of 100 GPU tasks complete in Neutral and in Performance, admission open, no GPU step, unattributed device memory 203 MiB; GPU activity reads Unknown in 88% of those sessions' samples; paused: 0 completions, VRAM unchanged; 50% duty cycle at 500 ms gives 0.50 of baseline, longest gap 258 ms, longest kernel 4.4 ms; T152: summed per-process SM differs from device utilization by at most 2, 2 and 5 points with 1, 2 and 4 processes | pass |
| T117, CPU reclaim (Yield First, Neutral, Performance) | `1c8a850` | matched throughput 0.933, 0.942, 0.933; decision p95 0.81, 0.78, 0.77 s; post-yield p99 1.05, 1.03, 1.07×; own usage at the target share 2 intervals (1.0 s) after the demand is first observed in Yield First and Neutral, 1–2 in Performance; one cap and no drain per case; manager peak 0.14–0.19 core and 94 MiB | pass |
| T117, GPU (Neutral, Performance) | `c0779d3` | GPU compute at the target share 2 intervals after the protected GPU load starts, after up to 6 caps and one pause, no drain; decision p95 0.07 and 0.60 s; post-yield p99 0.77×; matched throughput 0.923 in Performance but 0.723 in Neutral, where each new 20 s job reads as Unknown GPU activity at start, is capped at the fair share and stays capped through the cooldown | Performance pass, Neutral fail |
| T117, SC-014 (4 CPUs, 500 m tasks, idle) | `374637d`, `c0779d3` | own CPU 87.5% of capacity in Neutral and Performance, 75.0% in Yield First (targets 94%, 97% and 84%); the decisions read own work about 330–430 m low and count that as external; with 500 m tasks own use moves in 500 m steps, so this run cannot reach the targets even with exact measurement | fail |
| T117, SC-015 (beside a 60 m external user) | `374637d`, `c0779d3` | own CPU as a share of capacity minus the user and the margin: 93.6% in Neutral, 90.7% in Performance, 89.8% in Yield First (target 90%); no step change in 5 minutes | Neutral and Performance pass, Yield First fail |
| T118, SC-005 audit | `1c8a850` (CPU), `c0779d3` (GPU) | CPU and GPU, at the defaults and with Performance and a 512 MiB floor: 0 actions and 0 drains outside own allocations, protected snapshots unchanged, every protected run completed, no failed external GPU allocation, every owned GPU release confirmed, 0 OOM lines | pass |

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
