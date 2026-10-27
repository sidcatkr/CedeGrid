# Implementation Plan: Contention Modes with Graduated Yielding

**Branch**: `001-contention-modes` | **Date**: 2026-10-24 | **Spec**: [spec.md](spec.md)

**Input**:
- Feature specification: `specs/001-contention-modes/spec.md`
- Hard inputs: [plan-notes.md](plan-notes.md) and the local code-quality audit (D1–D6, H2,
  H3, H5–H9, M1–M18)

## Summary

The change moves CedeGrid from all-or-nothing draining to per-resource contention modes with a
graduated, reversible yield ladder. Six pieces carry it:

1. **One policy authority per node.** The agent's `PolicyEngine` delegates to a new pure
   module, `contention.rs`. That module turns telemetry into per-resource views (own,
   external, unattributed, floor, fair share) and per-allocation yield steps.
2. **Supervisors as actuators.** Supervisors stop deciding. They apply steps to their own
   verified processes through a new `Actuator` on `LaunchBackend`, and they stay reachable
   after an agent restart through an authenticated control socket.
3. **Workers learn targets** through a new resource-target file. The drain file stays
   existence-only.
4. **Mixed versions keep working** through additive feature negotiation.
5. **Setup.** A rootless terminal interface, with equivalent flags, writes the new
   `[contention]` configuration.
6. **No hard-coded thresholds.** Every threshold is a configuration value with a provisional
   default.

## Technical Context

- **Language/Version.** Rust 2024 with MSRV 1.88 (`cargo +1.88.0 check` in CI). The Python SDK
  supports 3.10+ and the TypeScript SDK Node 22.14+.
- **Primary dependencies.** Unchanged: tokio, axum/rustls, rusqlite (bundled), sysinfo 0.38,
  nvml-wrapper 0.11, libc, serde_json, toml 0.8 and clap.
  - One new direct dependency: `toml_edit` 0.22.27, which is already in `Cargo.lock` through
    `toml` 0.8 (research R13).
- **Storage.** SQLite: the node ledger and supervisor journal in `state.rs` and
  `execution_state.rs`, whose node-state schema moves 5 → 6 for the terminal cancelled status
  (T052a), and the coordinator distributed schema, which moves 3 → 4 through an explicit
  migration (R14).
- **Testing.**
  - `cargo test` run serially, with proptest for `contention.rs`.
  - Python `unittest`, and `node --test` for the SDKs.
  - Recorded 0.2 fixtures for the mixed-version tests.
  - Native evidence gates for cgroup and GPU (quickstart §4–5).
- **Target platforms.**
  - Linux x86_64 and arm64: the full ladder.
  - macOS: CPU and RAM local supervision, with a rootless ladder (duty-cycle cap, pause, hint,
    drain).
  - Windows: client only.
- **Project type.** One Rust crate (library plus `cedegrid` binary) and two SDKs.
- **Performance goals.**
  - Decisions complete within one monitor interval (500 ms default).
  - One NVML scan per interval per node, instead of N+1.
  - Validation targets from `docs/validation.md`: manager CPU ≤ 0.1/0.5 logical CPU, RSS ≤
    512 MiB.
  - Success criteria SC-002 to SC-004 and SC-012 to SC-016 use the provisional, configurable
    defaults.
- **Constraints.**
  - Rootless operation.
  - Act only on owned processes.
  - No new dependency without justification.
  - Compatibility with `config_version = 1`, snapshot schemas 1–2, 0.2 peers and 0.2 SDK
    workers.
- **Scale.** Up to 2,048 allocations per node report (existing numeric limit), a few GPUs per
  node, and multi-node coordinators.
- **Unknowns.** None open; Phase 0 resolved each one (research R1–R15). The remaining
  native-only facts are marked **verify natively** and become quickstart gates.

## Constitution Check

*Gate before Phase 0, re-checked after Phase 1.*

| Principle | How the plan complies | Result |
|---|---|---|
| I. External processes untouchable | Every actuator targets verified own pidfds, or a cgroup leaf whose membership equals the verified PID set (R4). An actuation audit hook records each target identity. Tests cover PID reuse, foreign PIDs in a leaf, and supervisor restart (FR-002). | Pass |
| II. No harm in any mode (2.1.0) | Floors hold in every mode, above enforced minimums. Floor policies B and C and Neutral memory policy B are removed, and configurations that set them fail to load. Every own process gets `oom_score_adj` 1000. Delay tunables are bounded (hint deadlines ≤ 30 s, RAM breach window ≤ 60 s, unknown window ≤ 20 intervals, step dwell and Yield First response ≤ 10 intervals, `reclaim_s` and pause bound ≤ 60 s) and act only above the critical level, which never drops below the minimum floor; below it the critical path takes over. Performance, small floors, margins below their defaults and detection thresholds above theirs are caution options that apply only once confirmed; the tuning knobs were classed in the third analysis pass (T143), and delay tunables stay bounded rather than caution because they are the reclaim bounds themselves. Negative nice is clamped to 0. No priority is ever raised. Work-conserving (2.1.0): the default Neutral keeps 5% of compute and Performance 2%; Yield First's 15% of compute and 20% of memory are a more yielding mode the operator chooses, whose cost falls only on own work; the CPU reserve never stacks on the margin; a floor breach needs observed free memory below the floor; and every configuration key has a class (safe, bounded, caution) checked by a test. | Pass |
| III. Unknown never silently idle | Unknown measurements hold the step and then escalate after a bounded, configurable window. GPU activity is classified quiet, active or unknown, and quiet needs positive evidence (R8). | Pass |
| IV. Pure, deterministic policy | `contention.rs` is pure and deterministic. Effects stay in the agent and the supervisors. Decisions carry reason codes. | Pass |
| V. Rootless and user space | No privilege. cgroup writes stay in the delegated subtree, with readback. Without cgroups the ladder uses steps that need no privilege. | Pass |
| VI. Tests first | D1–D6 tests are inverted first. Every FR has a test task, and hardware claims are gated on native evidence (quickstart §4–5). | Pass |
| VII. Compatibility | Config is additive and the legacy keys are tolerated. The snapshot schema moves to 3 and still reads 1–2. Wire changes are negotiated. SDKs gain optional keys and an environment variable. The ledger migration is explicit. | Pass |
| VIII. Concise docs | `docs/guarantees.md` is the single normative place for floors, exceptions, modes and reason codes (FR-056). | Pass |
| Execution constraints | The launch barrier is unchanged. `oom_score_adj` is applied in the gate before EXEC. The hidden dispatch order is kept (fixed in `8df6ff4`). New modules are top-level `mod x;` in `src/`, added to `ALLOWLIST` and the Cargo include. | Pass |

Post-design re-check: Pass. Complexity Tracking lists the single justified addition.

## Resolution of hard inputs

| Input | Resolution | Where |
|---|---|---|
| **D1** First sample counted as growth | No growth or new-PID event without a prior observation of the same device by the same authority. An allocation's first observation is a baseline. The supervisor engines that started without history are removed (R1). | `contention.rs`, `policy.rs`, `agent.rs` |
| **D2** 1 MiB of VRAM noise drains a job | A growth event needs growth above `noise_tolerance_mib` (256) beyond the unattributed band. A step is applied per allocation and is reversible; the protection cap is removed. | `contention.rs`, `telemetry.rs` |
| **D3** Idle external context blocks forever | A `Quiet` class backed by positive evidence (R8). Neutral and Performance admit on quiet or active devices within the floors; Yield First requires quiet. Launch recheck: `validate_gpu_launch_contract` follows the mode, not `Idle`. | `telemetry.rs`, `policy.rs`, `contention.rs` |
| **D4** CPU contention drains instead of sharing | The CPU ladder is lower share, cap, pause and hint. Drain is never used when CPU is the only contended resource (SC-007). | `contention.rs`, `actuation.rs`, `cgroup.rs` |
| **D5** Sampling skew zeroes the budget | Bounds on managed and host usage. Inconsistency inside the bounds is clamped and reported; outside them the resource is unknown and holds (R9). | `telemetry.rs`, `kernel.rs`, `contention.rs` |
| **D6** One missing observation drains everything | Charge that allocation at max(request, last known), hold the resource and escalate only that allocation (R10). | `policy.rs`, `contention.rs` |
| **N+1 policy authorities** (H2) | The agent is the only engine. Supervisors receive `adjust` and enforce deadlines plus a hold-on-loss fallback. A re-attachable `control.sock` reaches supervisors after agent restarts, and a live supervisor's journal is never rewritten (R1, R2). | `agent.rs`, `supervision.rs`, `child_supervision.rs`, contract `supervisor-control.md` |
| **SIGKILL on any post-EXEC error** (H3) | Errors are classified: observation → gone from the supervisor; control → hold; actuation → step unavailable; journal → bounded retry, then the drain chain (opportunistic) or a hold with report (Guaranteed). SIGKILL happens only after the TERM grace (R3). | `supervision.rs` |
| **cgroup controls written once** (H7) | `LaunchBackend::actuator()` with runtime `cpu.weight`, `cpu.max` and `cgroup.freeze` and, since T138, runtime `memory.high` for the RAM cap step (FR-011): own leaf only, never below what the leaf cannot reclaim or a Guaranteed request, released to current usage when it throttles without freeing memory, and restored to the launch value on reversal. Each write re-verifies leaf identity and that membership equals the verified PIDs, reads back and journals. A `Limit::{Max, Value}` type, and `assert_empty` replaced for adjustable controls. Advertised controls come from actual capability (H8). | `cgroup.rs`, `execution_model.rs`, `rootless.rs`, `agent.rs` |
| **Drain-file-only worker signal** (H9) | `CEDEGRID_RESOURCE_TARGET_FILE`, written by atomic rename, with sequence, deadline and ack. Passed to children through `GateRequest`. The drain file is unchanged and the v2 context gains optional keys only (R6). | `supervision.rs`, `child_supervision.rs`, `actuation.rs`, `python/cedegrid/worker.py`, `npm/src/worker.ts`, `docs/worker-protocol.md` |
| **Mixed-version heartbeat breakage** (H6) | Feature negotiation through `HeartbeatReply.features`. The agent adds new `NodeReport` fields only after `contention/1`. `LaunchRequest.contention` is `skip_serializing_if` None and stripped for 0.2 agents, and overrides are placed only on capable agents. `API_VERSION` is reused as `api/1` (R7). | `protocol.rs`, `coordinator.rs`, `agent.rs`, contract `wire-negotiation.md` |
| Also folded in | The GPU cooldown no longer gates CPU/RAM (M1). Cost-based victims (M2, M3). Envelope and spool rules (M4, FR-057, FR-058). Agent `Drop` sends no Drain (M5). Minimum TERM grace (M6). Sampling off the deadline thread (M7). Drain accounting (M8). Preemption fixes (M9, M10). Yield cause in the ledger (M11, M12). Structured reasons (M18). Reserved VRAM (Y1). Freshness versus health (Y4). | as listed |

## Architecture

```text
agent (sole authority)
  Collector ──► Snapshot (schema 3: reserved/free/unattributed/quiet/bounds)
  PolicyEngine::evaluate ──► contention::plan(views, steps, config) ──► Decision{resources, actions}
  actuation stage ──► per supervisor: Control::Adjust{seq, steps}   (pipe, or control.sock after restart)
  heartbeat ──► NodeReport (+contention when negotiated)

supervisor (per assignment, deadlines + actuation only)
  Control::Adjust ──► Actuator (cgroup leaf | rootless pidfd) ──► Event::Adjusted
  hint step ──► resource-target file (atomic) ──► SDK watcher ──► ack file
  drain chain: drain file → TERM → KILL (deadlines; never on observation/control error)

coordinator
  features negotiation, per-resource admission placement, yield cause (schema 4)
```

### New and changed modules

Each new module is a top-level `mod x;` and is added to `ALLOWLIST`.

| Module | Change |
|---|---|
| `src/contention.rs` | New, pure: resource views, fair share, modes, ladder state machine, victim cost, reason codes |
| `src/actuation.rs` | New: `Actuator` trait, rootless implementation (duty cycle and pause through owned pidfds, `oom_score_adj`), resource-target file writer and ack reader |
| `src/setup.rs` | New: setup choices, caution mapping, flag parsing, terminal interface (termios through `libc`), `toml_edit` writer |
| `src/policy.rs` | Budget and admission keep their shape. Calls `contention`. D5 and D6 fixes. Removes the protection cap |
| `src/telemetry.rs`, `src/kernel.rs` | Reserved/free, double-read memory, quiet class, bounds, health versus freshness. One collector per node |
| `src/model.rs` | Snapshot schema 3; `Decision.resources`, `Decision.actions`, `Decision.exceptions`; `YieldAction`; `ReasonCode` |
| `src/config.rs` | `[contention]`, the legacy GPU interpretation and load-time warnings, removed options, caution acknowledgements, floor minimums, `cpu.nice` clamp, cross-validation of staleness knobs |
| `src/agent.rs` | `run()` decomposition first (slots, outbox, heartbeat, actuation). Removes supervisor-side policy. Control-socket reconnect. Envelope and spool rules |
| `src/supervision.rs`, `src/child_supervision.rs` | Adjust and Query controls, control socket, error classification, minimum TERM grace, target file for children, `oom_score_adj` in the gate |
| `src/cgroup.rs`, `src/execution_model.rs`, `src/rootless.rs` | `Actuator` implementations and the runtime write path |
| `src/protocol.rs`, `src/coordinator.rs`, `src/state.rs` | Negotiation, placement, yield cause, schema 4 migration, cancelled-task fix |
| `python/cedegrid/worker.py`, `npm/src/worker.ts` | Resource-target API and context-free watcher; TypeScript drain tests first (M22) |
| `src/main.rs` | `setup` subcommand. Offers setup on a TTY when the config is missing |

### Implementation order

1. **Foundations.** Invert D1–D6 as failing tests. Add the `contention.rs` skeleton with
   property tests. Decompose `agent::run()`. Extract shared resource arithmetic.
2. **Correct yielding (US1).** Telemetry bounds, quiet class, reserved memory; the D1–D6
   fixes; single authority; error classification.
3. **Ladder and actuation (US2).** Actuator trait, rootless implementation, cgroup runtime
   path, control socket, victim cost, reversal.
4. **Modes (US3).** Yield First, Neutral and Performance; floors; envelope and spool; Guaranteed
   rules.
5. **Targets (US4).** Target file, SDK APIs, children, ack.
6. **Revisions before US5 (Phase 6a).** B1–B14 below.
7. **Utilization revisions (Phase 6b).** U1–U12 below, then US5.
8. **GPU sharing and placement (US5).** Launch recheck by mode; coordinator per-resource
   admission.
9. **Configuration and compatibility (US6).** `[contention]`, deprecations, negotiation,
   schema migrations, mixed-version matrix.
10. **Setup (US7).** Flags first (shared with agents in spec 002), then the terminal
   interface.
11. **Observability (US8).** `observe`, `doctor`, `status` and node reports; reason codes;
   `docs/guarantees.md`.
12. **Native evidence.** cgroup and GPU gates (quickstart §4–5), after which thresholds may be
    retuned.

## Project Structure

### Documentation (this feature)

```text
specs/001-contention-modes/
├── spec.md
├── plan.md              # this file
├── plan-notes.md        # hard inputs and candidates
├── research.md          # Phase 0
├── data-model.md        # Phase 1
├── quickstart.md        # Phase 1
├── contracts/           # Phase 1
│   ├── config-contention.md
│   ├── supervisor-control.md
│   ├── resource-target-file.md
│   ├── wire-negotiation.md
│   ├── setup-cli.md
│   └── decision-output.md
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

### Source code (repository root)

```text
src/
├── contention.rs        # new, pure
├── actuation.rs         # new, supervisor-side
├── setup.rs             # new
├── policy.rs  telemetry.rs  kernel.rs  model.rs  config.rs
├── agent.rs  supervision.rs  child_supervision.rs
├── cgroup.rs  execution_model.rs  rootless.rs
├── protocol.rs  coordinator.rs  state.rs  execution_state.rs
└── main.rs
tests/
├── policy.rs            # D1–D6 inverted
├── contention.rs        # new: mode and ladder properties
├── agent.rs  supervision.rs  managed_supervision.rs  cgroup.rs  coordinator.rs
├── cli.rs  configuration.rs  kernel.rs  state.rs  upgrade.rs  backup.rs
└── fixtures/0.2/        # recorded 0.2 heartbeat request/response fixtures
python/cedegrid/worker.py   python/tests/test_worker.py
npm/src/worker.ts           npm/tests/worker-drain.test.ts  npm/tests/worker-targets.test.ts
docs/guarantees.md  docs/worker-protocol.md  CHANGELOG.md
```

**Structure decision.** Keep the existing single-crate layout. Add three top-level modules,
not subdirectories, because the exporter requires top-level `mod x;`. `tests/fixtures/0.2/`
holds recorded 0.2 protocol fixtures, which must be added to the export allowlist (they are
loaded with `include_str!`).

## Interim rules

- **Mixed modes.** Until jobs could request a mode (T093), the least aggressive effective mode
  among the running allocations on a resource governed that resource's target. T093a replaced it
  with the target model below.

## Target model with per-job modes (T093a, US6)

- **Aggregate share.** The owner's aggregate target on a resource comes from the node mode and the
  node cap, exactly as today's single target: `own_allowance` with the node's effective mode.
- **Yield order.** Per-job modes do not raise the aggregate. They decide who yields inside it:
  when the aggregate must shrink, Yield First allocations yield first, then Neutral, then
  Performance. Within a tier, `victim_order` keeps the cost order (frees the resource, pending,
  opportunistic before Guaranteed, least progress lost, best fit, ID).
- **Per-job ceiling.** A job's own mode also bounds it: a Yield First job on a contended resource
  goes to zero even when the aggregate would allow more, because its mode's target is zero. A
  Guaranteed job keeps its CPU request in every tier.
- **Class first for drains.** Across tiers, every opportunistic allocation is drained before any
  Guaranteed one (FR-024); tiers order the caps, pauses and hints.
- **Test.** Two jobs in different modes on one resource: the Yield First job yields first and is
  held to its own target; the Performance job keeps the rest of the aggregate.
- **Pace.** Each job's steps respond and rise at its own mode's pace (response window and
  scale-up interval).
- **Allowance.** A job's allowance is also bounded by its own mode's target: the room is shared
  equally, no job is invited above its own target, and the room that bound leaves goes to the
  others. Each allowance rises at its own mode's step and interval.

## Revisions before US5 (2026-10-25)

Decided by the maintainer after the review in [review-pre-us5.md](review-pre-us5.md).

- **B1 Risk rule.** Constitution 2.0.0, Principle II: options whose risk falls only on own work
  may be offered; options whose risk falls on other workloads do not exist.
- **B2 Floor policy A only.** `FloorPolicy` and `NeutralMemoryPolicy` keep only `a`; `b` and `c`
  parse so that validation can name the replacement. `SupervisorOptions.guaranteed_oom_victim`
  is removed: the gate sets `oom_score_adj` 1000 for every own process. New keys:
  `guaranteed_hint_deadline_s` and `ram.guaranteed_breach_window_s`. Floor minimums are a
  runtime clamp with a warning, so 0.2 files keep loading; an unconfirmed floor below twice its
  minimum applies as twice the minimum. `acknowledge_danger` is renamed `acknowledge_caution`,
  with `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED`, without an alias since it was never released, and
  accepts `performance_mode` and `small_floor`. An unset `max_mode` is the most
  aggressive node mode, so no job reaches Performance unless the operator chose it.
- **B3 Setup.** The interface offers modes, the node cap, floor and margin sizes, and the
  Guaranteed hint deadline. Performance, small floors, margins below their defaults and detection
  thresholds above theirs are caution options with confirmation.
- **B4 Ordered targets.** `own_allowance` uses the table in
  [data-model.md](data-model.md#mode-targets-contentionown_allowance), with a Neutral idle margin
  of its own (between Yield First's and Performance's, decided after the review; 5% since U3).
  "Contended" replaces "saturated": saturation plus an active external principal, or unknown
  activity. Under contention Yield First targets zero for opportunistic work, which the ladder
  applies as a pause first, then after `pause_bound_s` as a cap or hint at the 5% minimum duty
  (FR-014). While own work holds more than capacity minus the Yield First margin
  (`ModeInput.own`), saturation may be its own doing, so Yield First caps to the margin within
  the fair share first. While a resource is not contended, a held cap rises toward the target at
  the mode's scale-up rate. A proptest checks `YF ≤ N ≤ P` over generated `ModeInput`s.
- **B5 Readiness.** The supervisor tracks `hint_ready` (any acked family directory) and
  `observes_drain` (acked, or the worker used the supervisor socket), sends `readiness` on change
  and includes both in `adjusted` and `state`. The agent keeps them in `AllocationFacts` and the
  negotiated `AllocationReport`. The policy offers `hint` only to hint-ready allocations, which
  removes the refused-hint retry. The critical floor path uses
  `critical_hint_deadline_s`, drains non-ready allocations at once, and sends `terminate` to
  allocations that do not observe the drain file.
- **B6 Principals.** `PolicyEngine` keeps per-UID admission state (joining after
  `principal_window_intervals`, leaving after `principal_leave_s` below `principal_min_percent`).
  `ModeInput` gains `background`, the usage of UIDs below `uid_min`, which max-min subtracts from
  capacity. UIDs not admitted yet fall into the unattributed principal, so contention is detected
  on the first observation.
- **B7 Memory headroom.** `PolicyEngine` keeps a max-deque per listed external process (PID and
  start time, from `Snapshot.external_processes`) and one for the rest of external usage per memory
  resource over `memory_peak_window_s`; a process's deque goes when it exits. Admission headroom
  uses the summed peaks, yielding uses current usage. CPU and RAM cooldowns are separate
  (`scale_up_cooldown_ms` per resource, each from its own stable time), and each resource's
  admission opens on its own; the 0.2 expansion flag still needs both, since every launch
  reserves CPU and RAM. RAM and GPU memory add
  `reclaim_backoff_ms × 2^(n−1)`, at most `reclaim_backoff_max_ms`, after the n-th reclaim
  episode within the peak window, the coordinator's retry pattern.
- **B8 Reclaim bounds.** SC-002 and SC-012, with scenario tests in `tests/contention.rs`.
- **B9 Legacy GPU keys.** `Config::legacy_gpu()` returns the interpretation and warnings
  (FR-031); `doctor` prints them. The admission effect of `never_share_occupied_gpus`, and
  removing the legacy `GpuExecutionMode` steering, belong to T085.
- **B10 Guaranteed.** Never paused. A CPU cap target is raised to the request, and a Guaranteed
  victim's CPU amount is its usage above the request. Memory is released only as the last step of
  floor policy A.
- **B11 Target model.** Above, implemented in T093a.
- **B12 US5 additions.** T087b, T087c and T087d.
  - GPU compute shares use the rootless SIGSTOP/SIGCONT duty cycle. Its period is one monitor
    interval (500 ms by default) and its minimum duty 5%. Kernels already submitted keep running,
    so the effective granularity is the period plus the longest kernel.
  - Pause is never used for GPU memory.
  - The launch recheck calls the same pure admission function as the policy, on a fresh
    observation.
- **B13 Immediate targets.** Both SDKs call a new handler with the current target. The
  supervisor writes the current target into a managed child's directory before the child starts.
- **B14 Runtime `memory.high`.** Done in T138: on a delegated cgroup, a RAM cap step that sets
  `memory.high` before the hint, verified by readback (FR-011 as amended).

## Utilization revisions before US5 (2026-10-25)

Constitution 2.1.0 adds the work-conserving rule to Principle II. Each item lands with tests.

- **U1 Real breaches.** `floor_breaches` holds keys whose observed free memory is below the
  floor; running memory work is judged by `judged()`: RAM usage, GPU memory usage when
  hint-ready, else the charge. Running-pass excess and envelope breaches use the judged total;
  the pending pass and admission keep charges.
- **U2 CPU reserve.** `own_allowance` runs on the whole CPU capacity, and the result is bounded
  by capacity minus reserve minus external usage, so the idle reserve is the larger of the two.
  `cpu.reserve_physical_cores` defaults to 0.
- **U3 Margins.** 15/5/2% on compute; Yield First 20% on memory.
- **U4 Performance.** Contended target `max(S, A − m_P·C)`; unknown activity keeps `S`. A cap
  whose usage follows its target no longer escalates while the target moves down.
- **U5 Probing.** `contention::probe_demand` keeps `DemandProbe` and satisfied records per
  compute resource in `PolicyEngine`; `contended()` ignores satisfied principals and
  `neutral_share` bounds them at their demand. A demand of a margin step or more skips probing,
  which keeps SC-002 for real demand.
- **U6 CPU admission.** `PolicyEngine.cpu_usage` keeps each running opportunistic allocation's
  CPU samples over `admission_usage_window_s`; its CPU charge is their p95.
- **U8 GPU floor.** `gpu.reserve_vram_mib` defaults to `2 × MIN_VRAM_FLOOR_MIB`, so the default
  floor is max(twice the minimum, 5%) and never a near-minimum caution floor.
- **U9 Backoff and leave.** Reclaim episodes span real breaches or usage excess; a held hint's
  target rises while calm. Leaving principals are bounded at their usage in max-min.
- **U10 Held back.** `HeldBack` per resource in `ResourceStatus` and the node report.
- **U12 Test cleanup.** `namespace::control_dir_for` names the control directory; four test
  files remove it with their temporary state roots unless the test failed.
- **Group 1.** `config::KEY_CLASSES` with an enumeration test over the derived `Deserialize`
  impls; `Config::caution_details`; `Config::validate_host` at the agent's first sample; upper
  bounds for settings that lengthen reclaim.
- **U7, U11, U13.** The allowance signal is specified now and implemented in T150; the
  constitution amendment is above; T088 also measures how often GPU activity is Unknown.

## Revisions from review items 002–013 (2026-10-25 to 2026-10-26)

Direct amendments made during review, checked through Spec Kit in item 014. Each lists its
requirement, the code it changed and its task.

- **Reversal and probing (T154–T156).** A floor-breach step reverses with room of at least
  `to_free` or the noise tolerance for the calm window plus the reclaim backoff (FR-013). A
  larger external demand gets the fair share and is watched for one response window before it
  is recorded as satisfied. Records do not expire, and at most 64 are kept per resource (FR-021).
- **Admission (T157, T162, T163).** CPU is charged at its p95 only after a full window. Each
  GPU's memory admission cools down on its own, and the node cooldown stays behind the 0.2
  flags (FR-034). Admission blocks for own work only while it is being reclaimed, and a request
  past own target is held (FR-028).
- **Placement (T164).** The coordinator follows the node's admission. Jitter within the noise
  tolerance or the reversal hysteresis toggles nothing (FR-028 as amended).
- **GPU compute (T152, T158, T165).** The external amount is measured when per-process samples
  account for the device, and principals are grouped by UID. An applied duty counts own work
  under an unknown amount (FR-021). A looser attribution tolerance is `relaxed_detection`
  (T159, FR-045).
- **Allowance and memory margins (T150, T160, T161).** FR-071 is implemented with one publish
  threshold held back in total. Memory keeps only the floor under Neutral and Performance.
- **RAM cap (T138)** and **PSI (T141)**: see H7, B14 and the next section. **Gate runner
  (T153)**: `tools/pressure_comparison.py` gains the mode, floor, step-timing and snapshot
  options that the T117 and T118 entries in `native-gates.md` use.

## PSI as a second CPU saturation signal (T141, 2026-10-26)

FR-072, from item 013's pinned rules:

- **Read.** `KernelCollector` reads `cpu.pressure` read-only from every sibling of each cgroup on
  the path from the cgroup v2 root down to own work's user slice (`user-<uid>.slice`, else the
  agent's own cgroup). It reports the highest `some` avg10 as `KernelSnapshot.cpu_pressure`,
  with own and system-wide avg10 for display. No external cgroup readable means `system_only`
  when `/proc/pressure/cpu` exists, else `unknown`.
- **Rule.** `policy.rs` computes `cpu_saturated` as utilization at or above
  `saturation_percent`, or external avg10 above `contention.cpu.psi_some_percent`. Each
  observation is judged on its own, like utilization. The ladder's calm window and hysteresis
  stop cycling, and `contended()` is unchanged.
- **Probing (item 015, report 013 Decision 1 = A).** Under PSI-only saturation, utilization below
  `saturation_percent`, `contention::probe_demand` probes every contending principal, not only
  those below one margin step: own work steps down by the mode's margin step per response
  window, a principal that does not grow into the freed capacity is recorded as satisfied at its
  usage plus the contention floor, and one that grows gets its share. PSI that stays high while
  the external user needs no more then ends in satisfied records. Every mode returns to its
  uncontended target, so modes stay monotonic and capacity stays in use. The record bound (64)
  and rejoin-on-growth rules are unchanged.
- **Shared cgroups.** A cgroup holding own work together with other users' processes counts as
  own, so its pressure is never counted; utilization still decides there.
- **Visible.** The codes are `saturated_by_psi`, `psi_system_only` and `psi_unknown`, and
  `held_back.contention_psi` holds the contention amount when only PSI saturated the CPU.
  `doctor` prints a PSI line under CPU.
- **Constitution.**
  - I and V: reads only, nothing written outside own cgroups.
  - II: evidence of external waiting only, never of own oversubscription. A host where others
    wait yields sooner; own stalls cost nothing. Probing every principal under PSI-only
    saturation keeps the rule work-conserving and modes monotonic when PSI stays high for reasons
    own work cannot change, such as an external load held back by its own limits.
  - III: unknown or system-only PSI adds no evidence, so utilization still decides, as before.
  - IV: the policy stays pure and reads the snapshot.
  - VII: the config key is additive. `held_back.contention_psi` is an optional field of the
    unreleased `contention/1` report.

## Faster Performance ramp (item 017, 2026-10-26)

Spec: Session 2026-10-26, SC-004, FR-034 and the defaults table.
- **Target**: on an idle host, Performance reaches 90% of available capacity minus the margin
  within 10 s. These are the new defaults of `performance_ramp_percent` and `performance_ramp_s`.
- **Scale-up interval**: in Performance, CPU and GPU compute rise one step per monitor interval
  (500 ms by default). RAM and GPU memory keep 2 s and the reclaim backoff (FR-034, FR-061).
  Yield First (30 s) and Neutral (10 s) are unchanged.
  - `ResourceThresholds` records whether its resource is compute.
  - `scale_up_interval_ms(mode, interval_ms)` and `cooldown_ms(mode, interval_ms)` replace the
    seconds-only forms.
  - A set `scale_up_step_interval_s` still overrides every mode.
  - The ladder (`LadderTiming::scale_up_ms`), the allowance's rise rate (FR-071) and the CPU and
    RAM admission cooldowns all read the same interval, so a rise still publishes at most one
    step per interval.
  - A held step still reverses no faster than the minimum step dwell, which is unchanged: the
    ladder waits for the larger of the dwell and the scale-up interval. The idle-host ramp
    target concerns admission and uncapped growth, where no step is held.
- **Admission (R4)**:
  - The limit today is the agent's parallelism. It grows by one worker per the larger of the CPU
    and RAM cooldowns, and offers arrive on the 2 s heartbeat, so a host that needs eight workers
    takes about 16 s.
  - In Performance, the agent reports launch slots up to its worker limit once its growth timer
    allows. The coordinator's placement then admits every task that fits within the allowance in
    one heartbeat.
  - The rule becomes a pure function, `launch_slots`, in `src/agent/service.rs` so the ramp
    simulation can use it. `lifecycle.heartbeat_interval_ms` keeps its default.
- **Unchanged**: reclaim (SC-002, SC-012), margins, floors, Guaranteed work, minimum step dwell,
  hysteresis, reversal tolerance, and the memory cooldowns and backoff.
- **Tests first** (tests/contention.rs):
  - the idle ramp reaches the target within 10 s for CPU and for GPU compute;
  - Yield First and Neutral ramps are unchanged, with the modes ordered at every interval;
  - an external workload started mid-ramp gets its demand up to its fair share within SC-012;
  - a process that appears and disappears every interval causes no cap/uncap cycling;
  - RAM and GPU memory scale-up timing is unchanged.
- **Gate**: the time to 90% on an idle test host, and a contender started mid-ramp, through
  `tools/ramp_gate.py` (`native-gates.md`). It is written, not run.
- **Constitution**:
  - II: more work-conserving; reclaim and floors are unchanged.
  - III and IV: no change; the policy stays pure.
  - VII: the defaults change in config docs and the contract; no wire field changes.

## Item 017 fixes (2026-10-26)

- **Held-back ceiling (T171, SC-016)**: CPU `held_back` gains `ceiling`, what a visible ancestor
  CPU ceiling keeps from the budget beyond the reserve and margin. It is never counted as
  contention. `doctor` shows it on its own line, and observe and the node report carry it as
  `held_back.ceiling`. Decisions do not change.
- **RAM cap through the supervisor (T174, FR-011, FR-024)**: a fake backend in
  `tests/managed_supervision.rs` wraps the rootless one with a `memory.high`-like cap. It covers
  the Guaranteed clamp, the journaled target, the unprompted report, the cap kept under a hint
  and reconcile on query.
- **Source manifest (T175)**: nothing reads a root copy, so it is untracked. The pack step writes
  the only manifest inside its output, and `/source-export/` is ignored.
- **Test directories (T176)**: `tests/agent.rs` kills and reaps the processes it starts, stops
  the coordinator, and removes its `.agent-test-*` directory with retries after success, failure
  or panic. The home-only state guard keeps the directory under the repository root.
- **Agent lock (T177)**: an `AgentLock` guard unlocks on drop, because a child another thread
  forks shares the lock's open file until it executes, even though the descriptor is
  close-on-exec.
- **Constitution**:
  - I: product code acts on no new process. The test guards kill only processes the test
    itself started, and `AgentLock` touches only the agent's own lock file.
  - VII: `held_back.ceiling` is a new wire field in the node report. The contract fixture
    carries it, and neither SDK models `held_back`, so the SDKs need no change.

## Item 023: own CPU and own-only GPU (2026-10-27)

Measurement only: the policy rules and the decision snapshot do not change.

- **Own CPU sources (research R9)**: `src/telemetry/managed.rs` reads each allocation's CPU from
  the first source that applies, and records the source in the `managed_cpu_attribution`
  evidence:
  1. *Full*: the allocation's leaf `cpu.stat` `usage_usec` (the leaf path is the scope of the
     record's `cgroup.procs` evidence), only when the leaf's `cgroup.procs`, read before and after
     `cpu.stat`, is the same set, contains the leader and lies within the allocation's own
     verified family PIDs. Family members inside the leaf are counted once, by the leaf.
  2. *Process clock*: `clock_getcpuclockid(pid)` and `clock_gettime`, the whole thread group
     including exited threads, read only while a pidfd opened before the read, checked against
     the verified identity, shows the process alive before and after.
  3. *Fallback*: `/proc/<pid>/stat` ticks with the existing 2-tick bound.

  The lower and upper bound structure, the aligned window and every identity check stay. A
  change of source between two reads gives no reading for that interval, like a changed
  baseline.
- **Bounds**: resolution (1 µs for `cpu.stat`, 1 ns for the clock) plus the read skew. The
  kernel folds a running thread's pending runtime into these counters only at its next tick, so
  each read can lag by one scheduler tick per thread that can be running: the smaller of the
  threads read (summed over leaf members) and the scope's CPUs. The tick is `1/CONFIG_HZ` from
  `/boot/config-<release>` when readable, else 10 ms (the kernel's minimum HZ is 100). A host
  with `nohz_full` CPUs keeps the `/proc` source, since a tick-stopped CPU folds pending time in
  only once a second.
- **Own-only GPU (FR-021, research R18)**: `src/telemetry.rs` marks a device interval own-only
  when both process queries succeed, at least one PID is listed, every listed PID is an own
  verified PID checked through a pidfd (no PID 0, no unresolvable entry), no MPS is present,
  device utilization is available, and device memory attributed to no listed PID is no higher
  than at the last measured, own-only or empty interval plus
  `contention.gpu_memory.noise_tolerance_mib`. The managed collector passes the verified
  identities, not only their PIDs, so each listed PID's pidfd is checked against its identity.
  It then reports external 0 and own = device utilization, and sets `GpuSnapshot.own_only`. `gpu_compute_path` in `src/policy/measure.rs`
  takes the measured path at once for such an interval and keeps the measured streak counting.
  Any failing condition keeps today's path.
- **Tests first**: the bound math per source; a gated Linux test (`CEDEGRID_TEST_CPU_CLOCK=1`)
  that a busy child reads within 2% of wall time from the clock and within 2 ticks of
  `/proc/<pid>/stat`, that short-lived threads are counted in full and that an exited or reused
  PID gives no reading; the leaf source with fake leaf files; the own-only cases and each failing
  condition alone; the decision snapshot against `65166b5`.
- **Constitution**:
  - I: every read is of own verified processes or their own leaf. The clock is read only while
    the pidfd shows the verified process alive, so a reused PID gives no reading. A foreign leaf
    member makes the leaf source unusable. Nothing is signalled.
  - III: a missing source, a source change or a failed check is no reading (unknown), never
    zero. The own-only rule needs every condition; any failure keeps Unknown.
  - IV: the policy reads `own_only` as data; telemetry stays read-only.
  - VI: each change starts with a failing test.
  - VII: `GpuSnapshot.own_only` is optional with default false and skipped when false, so
    recordings and replays keep their meaning.

## Item 024: summed own CPU bound and reruns (2026-10-27)

- **Summed own CPU bound (research R9)**: pending runtime exists only on a task running at the
  read, so across all own processes it is at most one tick per scope CPU at each read side. The
  collector's reads are sequential, and on one CPU the pending stretches of processes read in
  turn are disjoint and lie between its last tick and the last read, so the bound per read side
  is min(Σ threads, scope CPUs) × (tick + read pass length). `summed_lower_bound`
  (`src/telemetry/managed.rs`) subtracts that, the precise sources' resolutions and the tick
  counters' rounding in full; it is never below the sum of per-allocation bounds nor above the
  raw delta. The collector passes it as `Snapshot.managed_cpu_lower_millicores`, and the
  policy's CPU measurement (`src/policy/measure.rs`) uses it clamped between the sums of the
  per-allocation lower and upper bounds. Each allocation keeps its own bound in its evidence.
  38 tasks of 100 m on 4 CPUs at a 1 ms tick read 3686 m per process and 3791 m summed (3800 m
  used).
- **Ramp gate**: `--contender-uid` and `--contender-marker` measure a contender that a second
  unprivileged account starts, timed by `--submit-at`, by reading only its `/proc` entries;
  `--device gpu` times own GPU compute (SC-004's first bullet).
- **SC-002**: its 2-interval bound is for other principals' demand; the owner's unmanaged
  programs share the owner's fair share (Session 2026-10-27).
- **Constitution**:
  - I: the gate never acts on the second account's processes; the agent reads only own
    processes and own leaves, as before.
  - III: the summed bound needs every part's delta; an allocation without one is left out of
    both sums, as before.
  - IV: the policy reads the summed bound as data.
  - VI: tests first for the bound, the clamp and the gate's read-only contender.
  - VII: `managed_cpu_lower_millicores` is optional and skipped when absent, so recordings and
    replays keep their meaning.

## Item 025: launch pacing, admission under held caps and cap duty (2026-10-27)

- **Launches off the observation thread (research R19)**: the agent runs a current-thread
  runtime, and one launch blocks it 31 ms on the test host, 42–59 ms in the Docker Linux check and
  233 ms on macOS (synced context and spec writes, the spool walk, the spawn); while the loop
  awaits any request, the runtime runs the waiting launches. Each launch's preparation now runs
  on the blocking pool (`spawn_blocking` driving the existing async start through the runtime
  handle), at most 4 at a time.
- **32 new assignments per heartbeat** (FR-034): `launch_slots` is capped in every mode, so
  Performance still fills a 16-task scope in one heartbeat. The agent can't queue offers instead:
  the coordinator releases an offer that is not authorized within `lease_ms`.
- **Publication check**: a slot takes an attempt pin only when its checkpoint head is new or its
  result is released. `pin_attempt` opens a state connection and makes two synced writes, and
  ran for every slot in every interval (about 100 ms per slot on macOS; the publication stage
  fell from 5–8 s to 5 ms per interval with 50 slots).
- **Lease requests**: initial authorizations and renewals share a time budget of half the monitor
  interval per iteration, longest waiting first; the rest go out in the next iteration, and the
  longest-waiting request always goes, even once the budget is spent. They run
  one at a time and cost 50–90 ms each in Docker and on macOS (the coordinator's synced writes).
- **Heartbeat reply**: executions are read once, not once per offered assignment.
- **Admission under a held CPU step** (FR-034): `charge` (`src/policy/measure.rs`) stops adding
  usage samples to an allocation's window while a CPU step holds it back (a pause, or a cap or
  hint whose target its usage reaches, the ladder's binding test) and charges it at
  max(request, p95 of the frozen window) with no credit. A cap above its usage holds nothing
  back, so FR-028's room below a holding cap stays. The extra charge closes admission but is
  left out of the "charges exceed budget" safety check, so one job held at its mode's target on
  an idle host does not keep the node unsafe.
- **Cap rises** (FR-013): the existing rule, a held step's target rising to its usage plus the
  spare divided by the held steps, already bounds one round by the spare and splits it equally;
  tests pin it.
- **Cap duty (research R20)**: the 024 sawtooth came from actuation. `apply` (`src/actuation.rs`)
  reset the duty to target ÷ request on every cap change, so a raised cap at or above the request
  ran uncapped for one period and a task that makes up lost CPU ran a full core. The actuator now
  keeps the family's last measured natural rate and starts a changed cap at target ÷ rate, so a
  cap step (FR-011) limits usage from its first period.
- **Known limits, recorded in `docs/validation.md`**: the coordinator refuses initial
  authorization while the node's report shows admission closed (new allocations not yet
  measured), a decision for the reviewer; lease request throughput is bound by the
  coordinator's synced writes; SC-014's shortfall; Neutral GPU startup beside an idle listed
  process; how an excess is split across victims (report 024 Decision 2c); report 024 Decision 3's
  attribution questions.
- **Constitution**:
  - I: launches, pins and lease requests concern only own attempts; actuation still acts only on
    own verified families.
  - II: admission no longer counts capped usage as room, and a cap holds from its first period, so
    own work stops overshooting its share; the launch bound keeps telemetry fresh, so other
    principals' demand keeps being observed on time.
  - III: an iteration that outlasts its interval made the kernel CPU window and the coordinator's
    evidence stale; pacing keeps both fresh rather than treating stale data as known.
  - IV: the admission rule stays in the pure policy; pacing and duty are agent and supervisor
    effects.
  - V: no new privilege; rootless duty cycles as before.
  - VI: tests first for the launch bound, the lease budget order, the frozen charge, the rise
    bound and the duty rule; the 150-task burst is an opt-in native gate.
  - VII: no wire, config or recording change; `launch_slots` stays the same field with a smaller
    maximum.

## Item 026: authorize a burst's own reservations (2026-10-27)

- **Cause, per resource** (FR-028, research R22): the policy sets `ResourceStatus.admission_own_new`
  when CPU or RAM admission is closed and every cause is the node's own new allocations: an
  allocation with unknown usage and no earlier observation (its charge goes to an `unknown_new`
  total beside `unknown_charge`), or the stability cooldown that only they restarted
  (`PolicyState` keeps, per resource, whether the last reset of its stability had only that
  cause). The CPU hold from an inconsistent measurement (own above host busy) is tracked apart,
  so it never counts. Steps are held while a measurement is unknown, so contention is read
  directly: own charges, the new allocations at theirs, must fit the last target share computed,
  and external usage must be no higher than beside it, within the jitter tolerance; no step may
  be held on the resource, and RAM must not be in a floor breach. The node contention report carries the flag as
  `ResourceContentionReport.admission_own_new`, default false and skipped when false.
- **Coordinator**: the first authorization of an existing reservation (`grant`, sequence 0)
  calls `authorization_placement`, which counts a CPU or RAM resource closed with that flag as
  open and otherwise applies `placement` unchanged, so `Exhausted` still refuses. New placement and
  preemption keep `placement` (audit M10); its doc comment says so. GPU resources are never
  exempt, and a report without the field stays fail-closed.
- **Refused reservations today**: the agent retries the request each iteration; after
  `execution.prepare_timeout_ms` the supervisor gives up with a verified release, the agent
  reports an execution failure, and the coordinator counts it: a replay-safe task is requeued
  with backoff while failures stay under `retry_limit` and attempts remain, else it needs
  reconciliation (research R22).
- **Constitution**:
  - I: no process is touched; the change is in reporting and authorization.
  - II: work is authorized only within the budget it was placed against; own steps that hold
    work back, contention and floor breaches still refuse, so own work cannot grow past others.
  - III: only never-measured own allocations count, and an unknown host measurement still
    refuses; a missing field fails closed.
  - IV: the cause is computed in the pure policy; the coordinator reads a structured field.
  - VI: tests first for each cause alone, the burst, and placement unchanged.
  - VII: the field is optional and skipped when false, so 0.2 and earlier 001 reports keep their
    meaning (fail-closed), and the contract fixture adds a case.

## Item 027: exact own measurement, batched leases and 026's failures (2026-10-27)

- **Own CPU and host busy** (FR-073, research R23):
  - The kernel CPU tracker keeps the last 8 or more counter samples and reports host busy over
    that window, upper and lower with one tick of rounding per window, beside the per-interval
    values (`CpuTopology.busy_window_millicores`, `busy_window_lower_millicores`,
    `busy_window_ms`).
  - Full: when the cgroup backend runs every own allocation in a leaf directly under
    `delegated_root`, the root holds no process and its child set equals the own leaves, the
    kernel collector reads the root's `cpu.stat` next to `/proc/stat` and own CPU comes from it,
    with an error of 1 µs plus one tick per own thread that can run, at most one per scope CPU.
    CedeGrid's agent and supervisors never sit under the root (the backend refuses a root with
    processes and supervisors stay outside their leaves), so nothing needs subtracting there.
  - Tracked: an established process's delta is divided by its own read interval; a process in
    its first interval keeps the aggregate divisor, as the native first-interval fix requires. `summed_lower_bound` drops
    the read-pass term and keeps each source's resolution and the tick lag. Each allocation keeps
    its baselines for the window, so the window delta comes from cumulative counters with one
    bound (`Allocation.observed_window_cpu`, `Snapshot.managed_cpu_window_lower_millicores`).
    All new fields are optional, default absent and skipped when absent.
  - Policy: `cpu_external` computes the external estimate per interval, as today, and over the
    window. While the per-interval value stays within its own bound of the window's (host busy
    upper minus lower, plus own upper minus lower), the window's value sets the allowance, the
    targets and the admission budget; beyond that bound, growth or fall, the per-interval value
    applies at once, so SC-002's 2-interval bound and growth detection are unchanged. The
    admission p95 samples take `observed_window_cpu` when present.
- **Batched leases** (FR-074, FR-034, research R24):
  - `Request::Leases {items}` and `Response::Leases {results}` behind the feature
    `lease_batch/1`, advertised in `COORDINATOR_FEATURES` and learned in `NegotiatedFeatures`.
  - Coordinator: one session check, one `advance_clock` and `expire`, then one Immediate
    transaction: renewals first, by lease deadline, then initial authorizations. Each item runs in
    its own savepoint; a preparation row is written before the item's grant savepoint, so it
    survives a failed grant as it does today. One commit, a result per item.
  - Agent: at most 128 items and 1 MiB per request, split beyond; on a batch error it forgets the
    feature and sends single requests. Per-item events (`authorization_unavailable`,
    `lease_unavailable`, the `renew` evidence of `control_rpc`) keep their bytes.
  - Slot pass: journal and checkpoint-head reads are skipped for a slot with no new events while
    the agent connection's `PRAGMA data_version` is unchanged.
- **Cancelled reservations** (FR-012, SC-007):
  - A prepared, unauthorized slot is controllable, so the pending pass may cancel it; `actuate`
    sends its drain before any `Authorize`, and `service_slots` sends no `Prepared` for a slot
    with a drain sent. An authorized slot whose journal still reads Prepared is uncontrollable to
    the pending pass.
  - `StepState.pending` marks a step planned on the pending pass. The running pass re-plans a
    pending-pass CPU drain of opportunistic work instead of holding it.
- **Own usage over its target** (FR-011, FR-018, research R25):
  - The ladder's cause is `own_over_target` when own usage is over the budget and the budget did
    not fall beyond its per-interval bound since the last target (`PolicyState.cpu_target`):
    external usage did not rise and no contention lowered the target. Any fall keeps
    `external_growth` and today's path.
  - Over the target without external growth, held caps keep their targets, which the actuator
    enforces from the measured rate, and no step is entered until usage has stayed over the
    target for `min_step_dwell_intervals` with every held cap reported applied; then steps take
    `own_over_target`. Such steps reverse like other non-external causes, and a binding cap
    holds. An `own_over_target` hold restarts neither the scale-up interval nor CPU stability.
  - `YieldCause::OwnOverTarget` joins the catalog and the guarantees table, with reversal code
    `step_reversed_own_usage_fell`, the drain-permission matches and the reconcile reseed.
    `ResourceContentionReport.cause_summary` (optional, skipped when empty) counts steps by cause,
    so `status` shows `own_over_target` apart.
- **CedeGrid's own cost** (FR-059, FR-069, SC-014):
  - The agent measures its own CPU (process clock) and its supervisors' (verified children) as
    `Snapshot.cedegrid_cpu_lower_millicores`, per interval and over the window. The policy
    subtracts it from the external estimate and from the owner's unmanaged principal usage,
    reports it as `ResourceStatus.cedegrid`, and lowers own tasks' targets and budget by it, so
    it never forms an unattributed principal. `doctor` and `status` show it as in use by
    CedeGrid.
  - The supervisor's running loop waits on its control descriptor, the gate's pidfd and the
    child server, with a timeout at its next deadline (actuator phase, lifecycle, readiness),
    instead of a 5 ms sleep. One supervisor per task stays.
  - `[profile.release]`: `lto = "fat"`, `codegen-units = 1`. `panic` stays `unwind`: the
    supervisor turns a panicked log-drain thread into an error with cleanup, the agent's
    blocking launches report panics as join errors, and `Drop` cleanups run on unwinding.
  - An opt-in Linux test measures the agent's and supervisors' CPU and resident memory at 117
    tasks. No runtime path starts Python.
- **System accounts on a GPU** (FR-021): the GPU collector receives `uid_min` and
  `principal_min_percent`. A listed external context whose user ID is below `uid_min` is
  background when per-process utilization is readable (the query succeeds or reports no samples)
  and its sampled utilization stays below `principal_min_percent` of the device
  (`GpuSnapshot.background_process_ids`, default empty and skipped when empty). Background
  contexts make no activity unknown and count as neither external compute nor unknown in the
  compute amount and the own-only rule; their memory stays external.
- **CPU return once contention ends** (FR-034): `PolicyState` counts consecutive intervals in
  which every contending external CPU principal, the unattributed remainder included and
  background excluded, is below `principal_min_percent`. From `principal_window_intervals` on,
  Yield First and Neutral raise held CPU caps and the CPU allowance in one move to the
  uncontended target, without the mode's interval; the dwell still applies. Memory, partial
  falls and admission cooldowns are unchanged.
- **Snapshot**: recorded scenarios carry none of the new fields, so measurement changes leave
  them unchanged; the cause and enforcement rule, the pending re-plan and the CPU return change
  scenarios where own usage was over its target without external growth or a contender left.
- **Constitution**:
  - I: no process outside the verified set is read for control or acted on; the root `cpu.stat`
    is read only when its children are exactly own leaves, and supervisors are verified children.
  - II: own work still gives back first: external growth keeps today's path and bounds; the hold
    applies only when no external usage rose, and PSI saturation still reclaims. CedeGrid's own
    CPU comes out of own targets.
  - III: a baseline outside its window, a root with foreign children, or unreadable per-process
    GPU utilization stays unknown; the window never replaces a per-interval reading beyond its
    bound.
  - IV: windows and causes are computed in the pure policy from snapshot fields.
  - V: only read-only files and the delegated subtree are read; no new privilege.
  - VI: tests first for each part; the run 6 shape replays in the simulator.
  - VII: every new field is optional; the batch is a negotiated feature with a single-request
    fallback, and the contract fixture adds a case.

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| Re-attachable supervisor `control.sock` | Steps must reach supervisors that outlive an agent restart (FR-015). Without it the new agent cannot release capacity without draining. | The stdin pipe cannot be reopened. Re-seeding supervisor engines keeps N+1 authorities. |
| `toml_edit` as a direct dependency | Setup must preserve comments and unmanaged settings (FR-053). | Re-serializing with `toml` loses comments. `toml_edit` is already in `Cargo.lock`, so no new crate is added. |
