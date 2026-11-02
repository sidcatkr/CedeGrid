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

### Item 027 decisions on report 027-1 (2026-10-29)

- **D3 A and D4 A** (FR-011, FR-016, FR-021, SC-002, Session 2026-10-29): the amendments record
  what the code already does. Running CPU yields tier by tier; within a tier held caps and own
  usage over its target are lowered at one level and a lowered cap counts what it frees
  (`src/policy/ladder.rs`), and new victims follow the cost order. Under saturation, a satisfied
  principal using less than its recorded demand by more than the contention floor counts at that
  demand (`expected_external` in `src/contention.rs`). Work: tests that pin both rules, and the
  guarantees text already written for item 027.
- **Sampling cost, no change to what the agent decides** (D2 A, SC-014):
  - Own tasks: the managed collector keeps the pidfd it opened when it first verified an
    allocation's process identity. Each interval it re-verifies through that pidfd (alive means
    the PID still names the verified process) instead of re-reading `/proc` identity, and the
    process clock is read under the held pidfd. A dead pidfd, a changed identity or a new
    allocation goes through the full identity check once; released allocations drop their
    handle. The two sysinfo refreshes of own PIDs per interval go too: their `/proc` tick
    counters are read only when that last source applies, and resident memory from `statm`, the
    values sysinfo returned. Linux only; other platforms keep today's path.
  - Principals: on Linux a process table reads each `/proc/<pid>/stat` once per interval for CPU
    ticks, start time and resident pages, and a process's real UID once from `status`, cached by
    PID and start time. Per-process CPU is computed as sysinfo computes it (ticks over the global
    `/proc/stat` ticks per CPU, at most the CPU count, zero on first sight). It replaces sysinfo's
    full refresh, which also lists every thread as a process (research R26), for the principals
    and for the external processes whose memory peaks are tracked. Other platforms keep sysinfo.
  - The Docker cost test reports the sample stage before (3.86 s per 60.6 s, 64 m, at `abbcd63`)
    and after.
- **Test fix**: `a_hint_carries_the_deadline_the_policy_enforces` raced the harness's `stop`
  marker, written one 5 ms supervisor pass after the hint is reported, against the worker's 5 ms
  poll. The test control writes `stop` only after the worker has written a `seen` marker, bounded
  by 5 s, so the assertion is unchanged.
- **Constitution**:
  - I: the held pidfd is the verified process's own handle; a PID is never trusted once its pidfd
    is gone. The process table only reads `/proc`; it acts on nothing.
  - II: unchanged; principals now count each process once, so external demand is not inflated by
    its thread count and the owner's unmanaged usage no longer includes own tasks' threads.
  - III: an unreadable `stat` or `status` leaves that process out, as an unlisted process was
    before; a dead handle falls back to the full check, never to an assumed identity.
  - IV: telemetry stays read-only; the policy is unchanged.
  - V, VII: no new privilege, field or dependency.
  - VI: tests first for the handle cache, the process table and the test fix.

## Item 028: Full applies the GPU compute share (2026-10-29)

- **Defect**: `CgroupActuator::unavailable` (`src/cgroup.rs`) marks `LowerShare` and `Cap`
  unavailable for GPU compute in every case. That dates from T042, before T087d gave
  `RootlessActuator` the GPU duty cycle. So in Full, GPU contention goes straight to Pause
  (`cgroup.freeze`), then Hint, then Drain, and own GPU work falls to zero instead of holding
  its share, while Tracked holds it. The spec's GPU compute share names no backend, so it needs
  no amendment (clarify found none).
- **Rule**: in Full, GPU compute `Cap` is available to Opportunistic work as in Tracked: a duty
  cycle of one monitor interval with the target's share of the device and a 5% minimum.
  Guaranteed work keeps no GPU compute step (FR-024).
- **Mechanism** (research R27):
  - With `cgroup.freeze` authorized, the duty's off phase freezes the leaf and confirms it
    through the same `cgroup.events` readback as Pause. Its on phase thaws the leaf only when no
    other resource holds the freeze, so a CPU or GPU Pause stays frozen through the cycle.
  - Without that authorization, the existing SIGSTOP duty runs through the rootless fallback.
  - A refused freeze write or readback falls back to the rootless duty and keeps the refusal as
    the step's detail, as other refused cgroup steps do. The duty's readback is bounded by its
    off phase, not Pause's 1 s, so a slow freeze cannot stretch the cycle or hold the supervisor.
  - A duty starts in its on phase. Its reversal and `release` thaw only a freeze the duty
    alone holds.
- **One duty cycle per family**: where the CPU cap is `cpu.max`, which is not a duty cycle, the
  GPU duty runs on its own. Where the CPU cap is itself the rootless duty (`cpu.max` not
  authorized, or its write refused), the GPU share joins that cycle at the smaller duty, as in
  Tracked. Reversing either step leaves the other in place.
- **Policy**: unchanged. `applied_duty_credit` credits the duty the supervisor reports applied,
  whatever the backend.
- **Audit**, `CgroupActuator::unavailable` against `RootlessActuator::unavailable` per resource:
  GPU compute `Cap` is the only step Tracked offers and Full does not. On CPU, Full adds
  LowerShare, Cap and Pause through `cpu.weight`, `cpu.max` and `cgroup.freeze`, or falls back.
  On RAM, Full adds Cap through `memory.high`, and neither backend pauses. On GPU memory neither
  actuator offers a step. In both, the hint goes through the resource-target file, not the
  actuator.
- **Docs**: `docs/guarantees.md` and `docs/validation.md` state that Full applies the share and
  how. A native GPU confirmation goes with the next native batch the user approves.
- **Constitution**:
  - I: a duty freeze writes only a leaf whose members are exactly the verified family, checked
    before each write as for Pause; the fallback signals only that family.
  - II: own GPU work gives back its share under external compute instead of everything, which
    keeps Full at least as work-conserving as Tracked; external demand gets the same capacity
    back.
  - III: a duty not reported applied is still credited as the whole device (FR-008).
  - IV: the policy is untouched; the mechanism lives in the supervisor's actuator.
  - V: no new control or privilege; it uses the authorized `cgroup.freeze` or owned signals.
  - VI: tests first for availability, the duty's off phase, held steps together and the credit.
  - VII: no wire, config or SDK change.

## Item 029: own-work efficiency, OOM order by fault, floors by mode (2026-10-29)

Constitution 7.0.0 (Principle II redefined, approved by the maintainer on 2026-10-29) and
Session 2026-10-29's nine answers. Parts 0–2 are local; Part 3 (test-host runs) waits for the
maintainer's typed approval after report 029-2.

- **SDK drain and target check (Part 0)**: `ResourceSignals.poll()` (Python) and the TS
  `ResourceSignals.poll()` re-read and parse the target file only when its stat identity (inode,
  size, `mtime_ns`) changes, else return the cached result; `draining()` is that stat plus the
  drain-file check it already made. Hook semantics are unchanged: a new sequence fires each hook
  once, and a file replaced by an atomic rename within one mtime tick is seen through its new
  inode. An opt-in microbenchmark (`CEDEGRID_SDK_BENCH=1`) reports µs per call before and after
  (target ≥ 10× cheaper unchanged). The SDK docs say to call `safe_point()` or `draining()` at
  natural safe points.
- **Launch nice by mode (Part 0b, FR-011)**: the supervisor's gate setup applies `cpu.nice` only
  when the task's effective CPU mode (`contention::effective_mode` of the node mode, the job's
  request and the node cap) is Yield First; the rootless backend's `prepare` stops adding the
  nice by class alone. The `cpu.nice` control evidence records the launch nice, and `status` and
  `explain` show it for a task now in another mode. Research R5 gains the by-mode rule.
- **OOM order by fault (Part 0c, FR-023)**: the worker gate no longer writes 1000 at launch; it
  reads `oom_score_adj` before exec as the baseline, carried in the execution record. The policy
  marks, per memory resource RAM, the allocations whose memory makes up own usage above its fair
  share, in the ladder's memory victim order (FR-016), as `oom_preferred` in the decision; the
  agent sends the flag to the supervisor with the next adjust line, and the supervisor writes
  1000, or the baseline when the flag clears, to every verified family member. The write takes
  only a PID path, so it follows Principle I's rule for such calls: the member's pidfd shows it
  alive, the supervisor opens its `/proc/<pid>` directory and reads the start time through it,
  which must match the verified identity, writes `oom_score_adj` through that directory, and
  checks the pidfd again after the write; any failed check skips the member and reports it as
  possibly misdirected, as the thread-ID rule requires. Children inherit the value at fork. `ram.prefer_own_oom_victim = true`
  (default false; key class `Safe`, as it affects only own work, and setup shows it as a risk to
  own work) keeps every own task at 1000 from launch.
  Every change is evidence (`oom_score_adj` control: task, from, to, cause, own memory and fair
  share). GPU memory has no kernel OOM killer, so the order applies to RAM.
- **Performance is the default (Part 0d, FR-030, FR-031, FR-043–051)**: `contention.default_mode`
  defaults to Performance; Performance leaves the caution list (no acknowledgement needed; an
  existing `performance_mode` entry still loads); setup preselects Performance; `config
  example`, docs and the tests that assumed Neutral set Neutral where they test Neutral. A file
  with no mode setting gets one `default_mode_changed` notice in `doctor` and the migration note.
  The spec overview, README and `docs/guarantees.md` lead with the purpose.
- **Floors by mode (Part 0e, FR-023, FR-024, FR-063)**: in Performance and Aggressive a memory
  floor breach's excess is at most own memory above its memory fair share: the frame's floor
  excess is capped at `judged_total − fair_share`, so in-share own memory is not hinted, drained
  or sent TERM, on the critical path and for Guaranteed memory too, while own memory above the
  fair share steps as before. Memory targets use no fair share today (outside Yield First,
  `own_allowance` is capacity minus external), so the memory fair share is the equal split
  compute uses: `neutral_share` of the RAM view `ram_target` builds beside the RAM principals,
  and for GPU memory, which forms no principals of its own, the same split over the device's
  GPU compute principals. Yield First and Neutral are unchanged. The envelope breach path is
  unchanged in every mode.
- **Save-progress and OOM recovery (Part 0f, FR-075, FR-076)**:
  - The policy keeps a per-resource breach episode; when a floor breach starts while own memory
    is within its fair share, the decision lists the in-share allocations once as
    `save_progress`. The agent forwards it, and the supervisor's target file carries a
    `save_progress_sequence`; both SDKs fire an `on_save_progress` hook once per new sequence
    (worker-context change in Rust, Python and TypeScript with the contract fixture).
  - The agent reads the host's `/proc/vmstat` `oom_kill` each interval (read-only telemetry) and,
    in Full, the leaf's `memory.events` `oom_kill`. A task that ended by SIGKILL in an interval
    where either rose is OOM-killed; the agent records the last observation's own memory against
    its fair share and the principals above theirs.
  - In-share kills are reported to the coordinator as a yield with cause `oom_not_own_fault`,
    which requeues without a failure or an attempt; the agent's memory admission, closed while
    the floor is breached, keeps it from starting again until free memory is above the floor.
    Own-fault kills stay failures. Wire change in Rust, Python and TypeScript with the fixture.
- **No pause below the fair share in Performance (Part 0g, FR-008, FR-014, FR-022)**: the ladder
  input carries a compute floor (the fair share) for Performance and Aggressive. A cap's target
  never goes below it; a cap that misses its target after the hold window is re-entered at a
  target recomputed from the measured rate instead of escalating; pause is chosen only when the
  cap is at the minimum duty and usage is still above the fair share, with a pause bound of one
  monitor interval, then back to the cap. Yield First and Neutral keep their ladder.
- **The decisions on report 027 (Part 0h)**:
  - Run 2, descriptors (FR-077): the agent raises its soft `RLIMIT_NOFILE` to the hard limit at
    start; the supervisor spec carries the original soft limit and the worker gate restores it
    before exec. Before a launch, the agent checks its open descriptors plus the task's need
    against the limit and refuses the launch with `descriptor_limit` otherwise: 8 per running
    task (measured on the test host at 7: three supervisor pipes, a state connection and its
    WAL, the held pidfd, the stderr capture), 8 more for each launch in progress, and 64 kept
    free for the agent's own reads. An `EMFILE` or
    `ENFILE` while sampling marks the interval's affected readings unknown; while launching, it
    refuses the launch; the loop never exits on it. The held pidfd stays. A shared state
    connection is used only if the decision snapshots stay identical.
  - Run 1: recorded in `docs/validation.md` (done in item 027); no measurement change.
  - Run 5, hysteresis (FR-013): a cap held with cause `own_over_target` is not removed or raised
    while own work's total stays over its target, within the hold window (the step dwell,
    `min_step_dwell_intervals`, which the own-over-target hold already uses); held caps reverse once
    the total is below the target by the contention floor.
  - Run 6, the quiet count (FR-034): `note_cpu_quiet` uses this interval's unattributed external
    estimate, not the window's, against the larger of `principal_min_percent` of capacity and
    the scope's per-interval measurement band.
  - Run 3: recorded in `docs/validation.md` (done in item 027); the p95 charge is unchanged.
- **Harness (Part 1)**: `tools/pressure_probe.py` checks every 50 ms in both probes, with a no-op
  context when unmanaged; `tools/pressure_comparison.py` gains an `own-efficiency` profile with
  defaults only and the agent and coordinator pinned outside the probe's scope, recorded in the
  report; the T117/T118 profiles keep their reserve, labelled in `docs/validation.md`.
- **Record (Part 2)**: `docs/validation.md` explains the old 0.93–0.94 and 0.71–0.80 ratios as
  profile effects from the decision fields; `docs/guarantees.md`, README, data-model and the 004
  Aggressive note ("memory floors kept down to the fair share") follow the amendment.
- **Constitution**:
  - I: the runtime `oom_score_adj` write reaches only verified family members, through the
    member's pidfd and its `/proc` directory; no other process is touched.
  - II (7.0.0): OOM order by fault, floors by mode, the loss rule and Performance as the default
    are what this item implements; launching Neutral and Performance at nice 0 is the default
    priority, never above it.
  - III: an `EMFILE` reading is unknown, never free; a missing `oom_kill` count leaves a kill
    unclassified, so it is a normal failure.
  - IV: the fair-share caps, `oom_preferred`, `save_progress` and the quiet count are pure policy
    outputs; the writes stay in the supervisor.
  - V: no privilege: raising one's own soft limit to the hard limit and raising or restoring
    one's own `oom_score_adj` need none.
  - VI: tests first in every group.
  - VII: the worker-context and wire additions are optional fields, in all three languages with
    the contract fixture; old configurations load, with the default-mode notice.

## Item 030: the cross-checked fixes (2026-10-30)

Spec Session 2026-10-30's nine answers (amended FR-008, FR-021, FR-022, FR-034; new FR-078 to
FR-082) and the item 030 addendum's list, in its order. The problem statements are the verified
findings of the two audits of `a2f68aa` (`030-findings.md`, `030-reviewer-findings.md` in the
review outbox). Each finding: a failing test first, the fix, one commit. A medium finding no
test reproduces moves to the report's appendix instead. The 32 low findings are deferred.

**Technical Context**: Rust 1.88, no new dependency. `libc::prctl(PR_SET_PDEATHSIG)` in the
worker gate; std `CommandExt::process_group` for supervisors; `nvml-wrapper` 0.11's
`Device::samples(Sampling::GpuUtilization, ..)` for device utilization over the interval;
`tokio::time::timeout` for coordinator RPCs. Linux-only parts (parent-death signal, `/proc`
children, cgroup leaf members, NVML) are tested on Linux in the Docker check and with fakes on
macOS. No NEEDS CLARIFICATION remains.

1. **Retention and history loading (FR-081; AGT-1, EFF-1, EFF-3, EFF-5, R4)**:
   - `ExecutionJournal` gains `active_executions()`: rows not Released, in SQL. The loop's
     per-iteration passes (slot service, outbox recovery, `allocation_facts`, sampling) use it.
     Released rows are scanned once at start for spool reclamation and outbox recovery; a record
     reaching Released during the run is reclaimed when it gets there, then never again.
   - The heartbeat reports active rows plus the last 256 Released ones (by release time), and
     the coordinator's 2048 bound stays.
   - `observations`: after each insert, every 600th iteration deletes rows older than 24 h and
     rows beyond the newest 200,000, in one statement.
   - An attempt's directory under `attempts/` is removed once its outcome receipt is written and
     publication GC has nothing left in it; `spool_size` keeps a running total updated on
     reservation, release and removal instead of walking the tree every 5 s.
   - `upgrade_attempt_held`'s schema check runs once per agent start.
2. **Supervisor death and stop state (FR-078; SUP-1 with 028 Decision 1, SUP-5/R2, SUP-4,
   SUP-6)**:
   - The worker gate calls `prctl(PR_SET_PDEATHSIG, SIGKILL)` for Opportunistic work and
     `SIGCONT` for Guaranteed work before exec, then checks `getppid()` against the supervisor
     it was started by and exits if it already changed. The signal reaches only the gate's own
     process, a verified own process, and is set before the user's command runs.
   - The supervisor installs SIGTERM, SIGINT and SIGHUP handlers that set a flag; its loop then
     releases runtime steps and drains, as for a drain request. Supervisors are started in their
     own process group, so a terminal's SIGINT or SIGHUP to the agent's group does not reach
     them.
   - `CgroupActuator::reconcile` (on a reattaching agent's Query) thaws a leaf found frozen when
     the applied steps record no Pause, after verifying membership; a recorded Pause stays.
   - Rootless stop state: `stopped` is set before the family signal, `resume` and `release`
     send SIGCONT to every member whatever the flag, and the family signal continues past a
     failing member and returns the first error after signalling the rest.
   - A failed `release` is reported (`step_release_failed`), never discarded, and a refused thaw
     is retried every tick while `cgroup.freeze` stays recorded.
   - After the leader exits, the actuator keeps ticking while children run, and releases when
     the children's drain starts.
3. **Log capture and supervisor errors (SUP-2, R3, SUP-7, AGT-9)**:
   - The capture thread never closes the pipe's read end before the writer is gone: a quota,
     write or reservation error switches it to read-and-discard, counting dropped bytes in the
     outcome. The spool reservation is taken in chunks of at least 1 MiB, not per read.
   - Errors in the supervisor's own bookkeeping after EXEC (`try_wait`, the child server's
     `poll` and `accept`, `logs.finish`) hold the current step and report, as `control.poll`
     errors already do; a capture error after a clean leader exit is recorded in the outcome,
     not a failure.
   - The error path drains managed children before SIGKILL, and keeps a running Guaranteed
     leader supervised.
   - Every supervisor message goes through the non-panicking `report` helper; no `eprintln!`
     remains on the supervisor's runtime paths.
4. **Heartbeat freshness and loop health (FR-079; AGT-2, AGT-3, AGT-4, AGT-6)**:
   - The node report carries `sample_age_ms`, the agent's monotonic time from the sample to the
     send (negotiated; an older agent's report keeps the wall-clock check). A stale report
     marks the node stale in the coordinator: no renewals and no new offers until a fresh
     report, and no drain because of it.
   - Reconciliation checks a NeedsReconciliation record's GPU release with one NVML read per
     heartbeat, never a sleep loop, and backs off to every 10th heartbeat after 3 checks.
   - Each coordinator RPC in the loop runs under `tokio::time::timeout` of half the monitor
     interval; on timeout the iteration goes on to sampling and yielding and the request is
     retried next iteration (lease items are idempotent per item, FR-074).
   - Transient iteration errors (`EPIPE` on stdout, `NotFound` while walking the spool, a
     panicked preparation or publication task, `ENOSPC` on a refusal proof) are reported and
     skip the interval, like `EMFILE`; other errors still end the loop.
5. **Agent stop, reload and re-adoption (FR-080; AGT-5, AGT-7, AGT-8, AGT-10, AGT-11)**:
   - SIGTERM and SIGINT end the agent's loop without draining; supervisors keep running for
     re-adoption, and Opportunistic work drains at its lease deadline if no agent re-adopts it.
     An explicit operator drain, and `max_runtime_seconds`, still drain everything.
   - SIGHUP reloads the configuration: the file is parsed and validated; if only `[contention]`
     sections differ, the new settings apply from the next iteration (`config_reloaded`);
     otherwise nothing changes and the agent reports `config_reload_refused` with the keys that
     need a restart.
   - Re-adoption: a live supervisor whose Query failed is retried on later iterations, with
     backoff, until it answers or exits; replay mode re-adopts as well.
   - The coordinator extends an Opportunistic offer's deadline whenever the agent reports it
     preparing (heartbeat phase), so a long preparation does not expire it.
   - The journal keeps each run's start and last checkpoint times; re-adoption restores them,
     so victim order sees the progress a restart would lose.
6. **Policy (FR-008, FR-022; POL-1 to POL-5)**:
   - CPU unknown in Performance and Aggressive: the policy keeps the last known CPU view; while
     CPU is unknown the budget is Neutral's fair share of that view with one more contending
     principal of unbounded demand, and with no view yet no new step is taken (current steps
     hold). Yield First and Neutral keep budget 0.
   - Performance pauses only on a cap the supervisor reports at the minimum duty
     (`cap_at_minimum_duty`, from the actuator's duty) with own usage above the fair share.
   - While RAM is held, the last known memory fair share stays in force, so in-share own memory
     keeps FR-023's protection; a breach episode's save-progress request goes out when the
     protection applies.
   - External memory peaks, CPU satisfied records, probes, the quiet count and allocations' p95
     windows carry across held and unknown intervals.
   - POL-5 (PLAUSIBLE): a test of a satisfied record above current usage under own-caused
     saturation first; if it cycles, saturation is judged with own work at its Neutral share.
7. **GPU (FR-021; GPU-1 to GPU-5)**:
   - `measure_compute` averages device utilization from `Device::samples` over the same
     interval as the per-process samples, and counts each PID at its sum of samples divided by
     the device's sample count, so periods without a sample count as idle. Means round to the
     nearest point; a remainder within one point per listed process is not unattributed usage.
     A disagreement beyond the tolerance still reads unknown.
   - Device telemetry unknown: the GPU compute ladder entries are held, not dropped.
   - Activity of unknown amount: applied duties are credited as on the unsettled measured path.
   - GPU memory joins `unknown_windows`, with RAM's window.
8. **029 Decisions 1 and 2 (FR-034, FR-080)**:
   - A draining stop sends at most as many Drain lines per iteration as fit within 250 ms of
     the iteration's budget, the rest the next iteration, so no iteration exceeds the 1 s
     cadence; renewals and heartbeats go first.
   - Once the quiet count says contention has ended, held CPU caps move to the mode's target in
     one move, in every mode, as the allowance does.
9. **cgroup, telemetry and cost (SUP-3, TEL-1 to TEL-5, EFF-2, EFF-4, AGT-12/R6)**:
   - A managed child under the cgroup backend is verified against its own identity, and the
     leaf's expected membership is the leader plus its verified children.
   - The busy upper bound counts iowait as idle. Own RAM is the leaf's `memory.current` in
     Full; on the rootless path it adds each verified process's `Pss_Shmem`.
   - Validation requires `monitor.interval_ms` below `gpu.process_sample_max_age_ms`; the GPU
     sample cursor's age refreshes when its timestamp is unchanged.
   - The CPU set is intersected with `/sys/devices/system/cpu/online`.
   - GPU attribution unknown marks only the allocation's GPU reading unknown; CPU and RAM
     bounds stay.
   - The journal cache key is taken again after the agent's own observation insert, so only
     other writes invalidate it. Messages to supervisors are serialized to a buffer and
     written once.
   - An OOM verdict trusts a known leaf `oom_kill` count over the host count; uncounted OOM
     requeues stop at 3 × `max_attempts`, then count as attempts.
10. **Forks outside the SDK and SDK reads (FR-082; R1, SDK-1 with R5, SDK-2)**:
    - Every interval the supervisor lists processes outside the verified family: the leaf's
      `cgroup.procs` in Full; the children named in `/proc/<leader>/task/*/children`, read
      through the leader's verified `/proc` directory, in Tracked. Any process outside the
      family sets `single_process_violated` (status, `explain`, the outcome), once per run,
      with the count. Nothing is done to those processes. The README states the limit.
    - SDKs: the stat identity is stored only after a successful read, and the file is parsed
      again while its mtime is within 50 ms of now. Drain hooks are isolated like resource
      hooks; `DrainRequested` is raised after them.

**Constitution Check (7.0.0)**:
- I: the parent-death signal is set by the gate on its own process before exec, a verified own
  process; R1's detection reads `/proc` and `cgroup.procs` only and acts on nothing outside the
  family; the cgroup thaw on reattach verifies membership first. Pass.
- II: Opportunistic work killed with its dead supervisor gives its capacity back, and a second
  copy never runs beside an uncontrolled first; Performance's CPU-unknown hold at Neutral's fair
  share and the GPU measurement fixes remove losses the loss rule does not allow. No priority is
  raised. Pass.
- III: CPU unknown still counts as busy (Neutral's share with the unknown as a principal, or no
  new step), and GPU memory unknown gets a bounded window, not an instant drain; a stale report
  stops grants, a graduated response. Pass.
- IV: the policy changes stay pure; signals, thaws and file removal stay in the agent and the
  supervisor; detection is read-only telemetry. Pass.
- VI: tests first per finding. Pass.
- VII: `sample_age_ms` is a negotiated optional field; old agents and coordinators keep working.
  Pass.
No violation; no Complexity Tracking entry.

## Item 031: multi-process families (2026-10-31)

Spec Session 2026-10-31's eight answers (new FR-083 to FR-087; amended FR-023's OOM order,
FR-078 and FR-082) and item 031's parts in order. Workloads that fork their own helpers, PyTorch
`DataLoader(num_workers>0)` most of all, are measured and stepped as one family on Linux, in both
backends. Item 030's detection (FR-082) turns into membership: a descendant that passes
Principle I's checks joins the family, and only those that fail stay reported outside it. The
three small fixes from the review of 030's commits and the Full pin (a forked worker refuses the
leaf reading today) are committed before this section (`fe3ef3f`, `93c4db5`).

**Technical Context**: Rust 1.88, no new dependency. `libc::prctl(PR_SET_CHILD_SUBREAPER)` on
the supervisor before it forks the leader; `pidfd_open` through `libc::syscall` as the existing
`OwnedChild` does; reaping by `waitpid(pid, WNOHANG)` for one listed PID, or `waitid(P_PIDFD)`
where the kernel has it; `/proc/<pid>/task/*/children`, `/proc/<pid>/status` (`Uid:`, `Gid:`,
`PPid:`), `/proc/<pid>/stat` (start time) and `smaps_rollup`. Linux only: other platforms keep
the single-process contract, and their tests pin that. The Linux parts get their red and green
runs in the Docker Linux check, batched per group. No NEEDS CLARIFICATION remains.

1. **Membership (FR-083, FR-084; A.1 to A.8)**:
   - The supervisor sets `PR_SET_CHILD_SUBREAPER` on itself before it forks the gate; the
     attribute is not inherited across fork, so the workload is unchanged.
   - A `Family` in the supervisor holds at most 256 members (a constant), the leader and SDK
     children included, each with its pidfd, start time, parent and `oom_score_adj` baseline.
     Discovery runs once per duty period (the monitor interval) and inside every stop: each
     member's `/proc/<pid>/task/*/children` over all threads, plus the supervisor's own children
     over all its threads (the kernel reparents an orphan to a live thread of the subreaper),
     which are the leader, the SDK child gates and orphans. Without a `children` file it falls
     back to a ppid scan of `/proc`, and `doctor` names the fallback.
   - A candidate joins in Principle I's order: `pidfd_open`; then the parent chain from
     `/proc/<pid>/status` `PPid:` up to a member, or to the supervisor for an orphan; then the
     pidfd shows it alive; then real, effective and saved UIDs and GIDs equal the run's. A
     candidate that fails one check is counted outside the family with its reason and never
     acted on; one past the bound is counted `over_bound`.
   - Orphans: a child of the supervisor that is neither the gate nor an SDK child gate can only
     descend from this run, so the supervisor is the chain's root for it (R34). This holds
     because the supervisor forks nothing else; the module comment says so, so a later spawn
     in the supervisor process is not taken into the family.
   - Reaping: the supervisor reaps an orphan on the loop thread that reaps the leader, by its own
     PID and only when it is listed in the supervisor's `children` and is neither the leader nor
     an SDK child; never `waitpid(-1)`. The module comment in `supervision.rs` says so (R36).
   - Descendants of an SDK child join that child's family, found from the child's pidfd.
   - A discovery error (an fd or `/proc` read) holds the run's steps and is reported once per
     spell, as item 030's bookkeeping errors are; it never drains.
   - Launch contract: on Linux `RootlessBackend::prepare` and the supervisor's own check accept
     any `single_process` value (the field stays accepted); elsewhere the single-process or
     bounded-SDK-children contract stays required.
2. **Measurement (FR-085; B)**:
   - Own CPU: in Full `leaf_reading_with` takes the family as its verified set, so a leaf of
     family members is read again; in Tracked each member's CPU delta is read as the leader's is
     and summed, a member that exits counting up to its last sample.
   - Own memory: the family's summed `Pss_Anon` + `Pss_Shmem` (item 030's per-process reading),
     never RSS, so pages shared copy-on-write count once. A family of one keeps today's reading;
     Full keeps its leaf reading while usable. If `smaps_rollup` reads are over the efficiency
     budget, PSS is read every N intervals and held between (R37).
   - GPU: the agent's managed PID set includes verified descendants, so their per-process GPU
     memory and compute samples count as own, including in R33's interval shares.
   - Principals: the managed set includes descendants, so they leave the owner's unmanaged use.
   - The supervisor reports the family's members (PIDs and start times) to the agent through the
     `family` report (contract below), which the telemetry uses as the verified set.
3. **Actuation in Tracked (FR-086; C)**:
   - A stop (duty off phase, Pause, GPU duty off phase) discovers, stops every member through its
     pidfd, discovers again and stops new members, at most 3 passes (R38); a member that appears
     during an off phase is stopped when found. Continue sends SIGCONT to every member, continues
     past a failing one and reports it. `ESRCH`, or a pidfd that reads as exited, drops the
     member silently.
   - `oom_score_adj`: each member's baseline is recorded when it joins and restored at release;
     the preferred-victim value applies to new members (R7 comes in here). The value is inherited
     across fork, so a member whose start time falls in one of the run's marked spells takes its
     parent member's baseline (spec Q9); the supervisor keeps the run's spells in boot-time
     ticks, the clock of `/proc/<pid>/stat` start times.
   - Full: no change to `cpu.max` or `cgroup.freeze`. Every leaf write, the reattach thaw
     included, already requires the leaf's membership to equal `Family::identities()`
     (`verify_runtime_target`); once that includes descendants a leaf of family members is
     stepped again, and a leaf holding a process outside the family still refuses.
4. **Run end, drains and supervisor loss (FR-087, FR-078; D)**:
   - When the leader exits, members still alive get SIGTERM through their pidfds, then SIGKILL
     after `term_grace_ms`; the outcome records `members_ended`. A drain signals the leader
     first, as today, then SIGKILLs members still alive after the grace. If the leader exits
     during a drain's grace, the members get SIGTERM at once and SIGKILL at the drain's deadline,
     which no second grace extends (item 031 D.2).
   - The parent-death signal still reaches only the leader and SDK children: in Full a dead
     supervisor's other members stay in the leaf for reconciliation; in Tracked they cannot be
     found again, and a member stopped by an off phase when the supervisor died stays stopped
     (analysis H1, a decision in the report). `docs/guarantees.md` and the README say so, with
     PyTorch DataLoader workers exiting when their parent dies, a job script's background server,
     and its closing `wait`. They also name the known limits: processes outside the family
     outlive the run (in Full they keep the leaf nonempty), helpers that start and exit between
     two discoveries are not counted as own CPU in Tracked, and an orphan's `getppid()` is the
     supervisor, not 1.
5. **Reporting (FR-082; E)**:
   - `outside_family` keeps its name and gains a `reason`: `ids_differ`, `chain_unconfirmed`,
     `over_bound` or `untracked_platform`.
   - Status and `explain` show the family size (leader, SDK children, descendants) through a
     policy note, `family_members`, from the facts the agent passes on. `state` carries the
     family too, so an agent that re-adopts a supervisor learns a family that no longer
     changes. `untracked_platform` is reserved: no platform emits it while non-Linux launches
     refuse multi-process work.
6. **Tests (item 031 F)**: counting with plain `fork` and `multiprocessing` workers (and a
   synthetic external principal in Performance: the family's total at the target, the leader not
   at the minimum duty); shared copy-on-write memory read as about one buffer; an orphan
   verified, stopped, continued and ended with no zombie left; a fork during a stop; differing IDs
   through a fake identity source; leftovers at exit ended and counted; the reaping race with the
   leader; a partial signal failure; 300 descendants with 256 tracked; Full's leaf reading used
   with forked workers (the pin turned around); and the held-back table comparing a 4-worker
   family with one process of the same demand in Performance.
7. **Afterwards**: the own-efficiency gate (029) with a family of 16 members of about 50 threads
   each, reporting the cost of discovery and of PSS reads per interval; if discovery is over
   budget, the leaf in Full and, in Tracked, a full thread walk every K intervals with a
   main-thread walk between (stops always walk fully) (R35). `native-gates.md` gains exact
   commands for the test host in Tracked and Full: a `multiprocessing` stand-in with a shared
   buffer, and a `DataLoader(num_workers=4)` loop where PyTorch is present. Running them needs
   the user's typed approval, asked together with T368.

**Constitution Check (7.0.0)**:
- I: a descendant joins only through Principle I's checks, in its order, with the supervisor as
  child subreaper; it is acted on only through its pidfd (or the start-time-checked `/proc` path
  for `oom_score_adj`). An orphan's chain ends at the supervisor, which Principle I names as the
  subreaper and Principle V's "the run's subreaper tree" covers; nothing is reaped by a wait for
  any child. Processes that fail a check, or exceed the bound, are never acted on. Pass.
- II: helpers count as own work instead of the owner's unmanaged use, so the fair share no
  longer subtracts own work's own workers and the leader is not held to the minimum duty while
  they run; PSS keeps shared pages from tripping a floor breach. No priority is raised. Pass.
- III: a discovery error holds the step and is reported, never drains; members past the bound
  keep today's counting. Pass.
- V: rootless Tracked supports descendants that stay in the run's subreaper tree, as the
  principle says; work started outside the tree is not tracked and not presented as contained;
  non-Linux platforms keep the single-process contract. Pass.
- IV, VI, VII: the policy stays pure (the family size is a fact); tests first; the new supervisor
  report and outcome fields are additive within one binary. Pass.
No violation; no Complexity Tracking entry.

## Item 032: the derived GPU sample age (2026-10-31)

Spec Session 2026-10-31 Q10 (FR-070; contracts/config-contention.md "The GPU sample age"),
report 030's Decision 4, and Decision 3's local part. A config that never mentions GPUs keeps
loading at any interval the bounds allow.

**Technical Context**: Rust 1.88, no new dependency. `GpuConfig::process_sample_max_age_ms`
becomes `Option<u64>`, unset in TOML and in the default. One accessor,
`Config::gpu_sample_max_age_ms()`, returns the explicit value or max(2000, 2 ×
`monitor.interval_ms`); every reader uses it (telemetry's sample cursor, the policy's GPU quiet
window, the quiet-window validation). No wire, journal or SDK change; `config_migration` is
unchanged, since an unset key needs nothing written. No NEEDS CLARIFICATION remains.

1. **Validation (FR-070)**: an explicit value outside 1 to 60000 fails as now; an explicit value
   at or below `monitor.interval_ms` fails on every host with
   `gpu.process_sample_max_age_ms (<value>) must be above monitor.interval_ms (<interval>); set it to at least <interval + 1> or leave it unset`.
   The derived value is at most 20000 at the 10000 ms interval bound, so it always loads.
2. **Showing it**: `validate` prints a `derived` map beside the config,
   `{"gpu.process_sample_max_age_ms": {"value": N, "rule": "max(2000, 2 × monitor.interval_ms)"}}`;
   `doctor` prints a "Derived settings" section with the same line and carries the map in its
   JSON. An explicit value is not listed there.
3. **Decision 3, A (local part)**: run 7's return joins the test-host batch with T368 and T387 in
   `native-gates.md`, keeping the agent's decision records for that run in the evidence copy
   (redacted) so the lag between the quiet count and own usage coming back can be located. The
   run itself waits for the user's typed approval of the batch; T339 stays open until then.
4. **Tests**: an interval of 3000 with no sample age loads and derives 6000; an explicit 2000 at
   interval 3000 is refused and the error names both keys and 3001; the defaults derive 2000;
   `validate` and `doctor` show the derived value.

**Constitution Check (7.0.0)**:
- III: the GPU sample cursor still goes stale only after more than one interval, since the
  derived value is at least twice the interval; unknown readings keep their handling. Pass.
- VII: an explicit value keeps loading or failing as before; a config without the key that
  failed at an interval of 2000 or more now loads; the defaults are unchanged. Pass.
- I, II, IV, V, VI: no process, policy or launch change; tests first. Pass.
No violation; no Complexity Tracking entry.

## Item 033: supervisor loss in Tracked (2026-10-31)

Spec Session 2026-10-31 Q11 to Q13 (FR-078, FR-083, new FR-088), report 031's Decision 3 (B,
built now), and item 033's pin. The pin (Docker Linux, the agent in a session of its own, a
Tracked supervisor killed with its family stopped) showed the kernel's orphaned-process-group
rule: members that handle SIGHUP received SIGHUP and SIGCONT; with the agent stopped first, the
workers stayed stopped for good.

**Technical Context**: Rust 1.88, no new dependency; Linux only for the new behaviour. `setsid`
through `libc`; recovery with `pidfd_open`, `pidfd_send_signal` and the `/proc/<pid>` directory as
the supervisor's `oom_score_adj` path reads it. The agent keeps a run's descendants, boot ID and
IDs in `attempts/<assignment>/family.json`, written atomically, beside the per-run files it already
keeps there (`supervisor-spec.json`, `control-socket.json`); the leader's and SDK children's
identities are already in the store. No schema, wire or SDK change beyond additive report fields.

1. **The workload's session (B)**: the worker gate calls `setsid()` right after
   `PR_SET_PDEATHSIG` and before exec, for the leader and SDK children. The supervisor keeps its
   own process group. Since the workload no longer shares a terminal session, the standalone
   `cedegrid supervise` installs the supervisor's stop handlers too, so Ctrl-C at its terminal
   drains the run, as FR-078 asks of a supervisor, instead of leaving a Guaranteed workload to its
   parent-death SIGCONT. Full is unaffected.
2. **Recording (C.1)**: `FamilyReport` gains the run's `uid` and `gid`. The supervisor reports the
   family right after a stop pass that found a new member, besides each interval it changes; the
   pipe write does not wait for an answer. The agent writes `family.json` on every `family` report
   and on `state` (re-adoption).
3. **Recovery (C.2 to C.4)**: `agent::recovery` runs once per run (a `family-recovery.json`
   marker), from the slot pass when its supervisor exits and from `reconcile_guarded` for a run
   with no live supervisor (agent start and each heartbeat), only for `rootless` runs. Members:
   the leader and SDK children from the store, descendants from `family.json`. Per member: pidfd;
   `/proc/<pid>` start time and real, effective and saved IDs equal to the record, with the boot
   ID equal to this boot; alive through the pidfd; then SIGCONT (Guaranteed) or SIGKILL
   (Opportunistic) through the pidfd. Counts: continued, ended, identity mismatch, already gone,
   failed. They go to `family-recovery.json`, the record's detail and a `family_recovered` event.
   The Guaranteed run stays NeedsReconciliation with its reservation charged, as a dead supervisor
   leaves it today. Recovery runs from the slot pass only when the supervisor exits without having
   released the run. The marker is written after the signals; a repeat after a crash in between
   is harmless, since SIGCONT and SIGKILL are idempotent. `reconcile_guarded` releases a dead
   rootless run only when its recorded descendants are verifiably absent too, besides the leader
   and SDK children, so no reservation is released while a member of the run lives.
4. **Tests (D)**: Linux service tests kill a Tracked supervisor with its family stopped, the agent
   in a session of its own: Guaranteed (family, family with handlers, single process) receives no
   SIGHUP and finishes with its output; Opportunistic ends with no stopped process left; with the
   agent stopped first, the next agent does the same at its start. Unit tests with a fake source:
   a reused PID (another start time) and changed IDs are never signalled and are counted; one
   member's failed signal leaves the others handled; a Full run is never touched.
5. **Docs and the native check (E)**: `docs/guarantees.md` and the README name what remains (a
   member found and stopped in the instant its supervisor dies stays stopped until an agent runs);
   `native-gates.md` adds the supervisor-loss check to the test-host batch.

**Constitution Check (7.0.0)**:
- I: the agent acts only on processes whose identity a supervisor verified while alive, through a
  pidfd it binds to that recorded identity: boot ID, start time, assignment and generation (from
  the record) and the run's IDs, then alive. That is Principle I's verified handle; it is never by
  a persisted PID alone, as the restart rule forbids. The chain is not re-read, since its root is
  gone; its result is the recorded identity. The supervisor's own re-adoption already reopens a
  persisted identity this way. A reviewer reading "only through that pidfd" as the supervisor's own
  descriptor would need the `SCM_RIGHTS` alternative; the report says so. Pass.
- II: Guaranteed members are never ended for CedeGrid's failure; Opportunistic members end as
  their leader's parent-death signal ends it. Pass.
- III, IV, V, VI, VII: no policy change; tests first; additive fields. Pass.

## Item 034: the batch's three findings (2026-10-31)

Spec Session 2026-10-31 Q14 to Q16 (FR-021, FR-034, FR-085), the 033 batch report's Decisions 1
to 3. The batch at `cd077e9`: beside a light GPU user own GPU work stayed at its fair share,
since the summed per-process reading was a steady third of the device's; Yield First's return
brought own usage back in 9 s; a Tracked 1 GiB family cost the agent 86 m on a 4-CPU scope.

**Technical Context**: Rust 1.88, no new dependency. No wire or SDK change: the GPU snapshot and
its observation are persisted with each observation and shown by `history`, `observe` and
`doctor`, never sent to the coordinator, and `history` returns them as stored. Linux only for the
PSS path; the GPU path is unit-tested with fixtures.

1. **GPU slots (Q14)**: in `src/telemetry/gpu.rs` `measure_compute`, each PID's summed samples
   count against the slots of the per-process sampling period. The window runs from the cursor to
   the newest per-process timestamp; the period is the shortest positive gap between one PID's
   consecutive timestamps in the window; the slots are the window divided by the period, rounded,
   and at least each PID's own count. With no PID holding 2 samples in the window, today's slots
   (the device's sample count, else the most-sampled PID) and today's reasons apply. `GpuSnapshot`
   gains `compute_sampling`: the device's sample count with its first and last timestamps, the
   window, the period and slots used and which path chose them, and per PID its sample count and
   first and last timestamps; optional, left out when there is nothing to show.
2. **The return (Q15, T339)**: at the interval the quiet count reaches
   `principal_window_intervals`, the first interval contention has ended, and while own work holds
   no excess, every binding CPU cap, hint or pause held for external growth rises in one move to a
   common level L, so that those steps together hold the mode's target: L fills from the lowest
   upwards, each step counted at the smaller of its target and its usage (water-filling), and no
   step is lowered; steps held for other causes and caps their work does not reach keep their
   paths. That one move skips the dwell; later rises keep today's path (usage plus an equal part
   of the spare, after the dwell). The frame in `src/policy/ladder.rs` computes L, and
   `src/contention.rs` `plan_step` applies it through the calm raise. The one move applies in
   every mode, as FR-034 says; the quiet count and every mode's other steps are unchanged, and a
   test pins that Performance's and Neutral's return times do not get worse.
3. **The PSS period (Q16)**: in `src/telemetry/managed.rs`, for a Tracked run with more than its
   leader, each PSS pass over its members is timed and K = ceil(pass cost ÷ 0.5% of the scope's CPU
   time per interval), clamped to 1..10. Between passes each member keeps its PSS from the last
   pass and the run adds max(0, its members' RSS sum now minus their RSS sum at that pass). A
   change in the member set forces a pass. A family of one and Full's leaf reading are unchanged.
   The collector reports which allocations' RAM is an estimate this interval. In the agent
   (`src/agent/service.rs`), when a decision adds or changes a RAM step, marks a new OOM
   preferred victim, or sends a new save-progress signal (FR-075), for an allocation whose RAM was
   estimated, the agent reads those families'
   PSS fresh, puts the fresh readings into the allocations and evaluates again on the policy state
   from before the interval (`PolicyEngine` becomes `Clone`), so only a step the fresh reading
   confirms is applied.
4. **Small things**: a test helper removes a state directory's sibling
   `.<name>.cedegrid-control` (`src/namespace.rs`) with the state directory, and every test that
   makes state with `tempdir_in(current_dir)` uses it; a test checks that `uninstall` (or purge)
   removes a real user's sibling. R41 records the pin's default-SIGHUP observation as observed and
   unexplained. Existing directories are left for the user.
5. **Afterwards**: the efficiency gate's family figures again (031's 16-member family, and a
   1 GiB shared buffer), the full suites, the macOS serial suite and the Docker Linux check; the
   test-host rerun waits for the user's typed approval.

**Constitution Check (7.0.0)**:
- I: no new action on any process; the fresh PSS read reads only verified members. Pass.
- II: Performance no longer loses own GPU work to a sampling-rate mismatch where the period is
  readable; own memory is acted on only from a fresh PSS reading, so an estimate cannot cost own
  work; the agent stays within its measured-overhead budget (SC-014). Pass.
- III: with no readable period the GPU path keeps today's unknown reasons; the RSS-growth estimate
  never undercounts growth, and a copy-on-write break is at most K intervals late. Pass.
- IV: `policy.rs` stays pure; the engine is cloned and evaluated again by the agent, with no I/O
  in the policy. Pass.
- V, VI, VII: user space only; tests first; `compute_sampling` is additive and optional. Pass.

## Item 036: the GPU sample span and Yield First's bound (2026-10-31)

Spec Session 2026-10-31 Q17 and Q18 (FR-021, FR-034), answering report 035's Decision 1 and the
035 rerun report's Decision 1. The 034 rerun showed the driver returning one sample per process
per query, the duty-capped probe's 1 or 2 device periods before the window's end.

**Technical Context**: Rust 1.88, no new dependency. No wire or SDK change: `compute_sampling` is
persisted with each observation and shown by `history`, `observe` and `doctor`, which return it as
stored. Telemetry stays read-only; the policy is untouched.

1. **The sample span (Q17)**: in `src/telemetry/gpu.rs`, a per-device `GpuSampleState` that the
   collector keeps per UUID holds each PID's last sample timestamp and the last 4 intervals' summed
   shares, device utilization and per-side amounts. Each PID's share is the sum over its fresh
   samples of sm_util × min(t − p, W) ÷ W: p is its previous sample (from the state, or the earlier
   sample in this window, or the window's start), and W the window from the cursor to the newest
   per-process sample; the share is rounded and capped at 100%. The tolerance check runs on the
   interval alone, then on the last 4 intervals summed on both sides. A 4-interval pass reports the
   4 intervals' average amounts (external, own, per PID), with the remainder rule applied to the
   averages. An interval that cannot be compared (no baseline, MPS, an incomplete inventory, a
   failed per-process query) clears the history. After each interval each listed PID's newest
   timestamp is kept and unlisted PIDs are dropped. `compute_slots`, `SlotsFrom` and 034's fallback
   go.
2. **The record**: `ComputeSampling` in `src/model.rs` keeps the device's count and first and last
   timestamps, the window's start and end, whether the amount came from the interval alone or the
   last 4, and per process its previous sample, each sample's timestamp and sm_util (at most 32
   per process per interval) and the time counted. `slots`, `period_us` and `slots_from` go.
3. **Yield First's bound (Q18)**: the return tests bound Yield First's own usage at 2 intervals
   after the quiet count, which is `principal_window_intervals` after the contender's last usage,
   and Neutral and Performance at 3 s as before. T396 is recorded as passing on the 035 rerun in
   `docs/validation.md`: at `eef1b4d`'s run the quiet count came at 32.7 s and own usage was at
   98% at 33.7 s.
4. **Afterwards**: the full suites, the macOS serial suite, the Docker Linux check and the decision
   snapshot comparison; T368 on the test host waits for the user's typed approval.

**Constitution Check (7.0.0)**:
- II: Performance loses no own GPU work to a counting error where samples arrive; the 4-interval
  amounts are averages, so a sample missed in one interval does not read a process as idle. Pass.
- III: a missing baseline, MPS and an incomplete inventory keep their unknown reasons; over 4
  intervals a missed sample is not idle time; `quiet_window_intervals` is unchanged. Pass.
- IV: the policy is untouched and telemetry stays read-only. Pass.
- VI, VII: tests first; `compute_sampling` changes shape before any release, and stored rows are
  returned as stored. Pass.

## Item 039: GPU demand from uncapped intervals (2026-11-01)

Spec Session 2026-11-01 Q19 (FR-021, FR-022), answering the 038 run report's Decisions 1 and 2.
The diagnostic at `c52ba5c` showed a light user, at a constant 200 requests a second, reading 158 m
alone, 77–130 m beside own work at full duty and 251–264 m beside own work at 70% and 40% duty: on
a time-sliced device own work's gaps raise another user's reading, and the caps that follow make
the gaps.

**Technical Context**: Rust 1.88, no new dependency. No wire or SDK change: the estimate's record is
a new field of the decision's per-resource status, which the contention report
(`src/agent/report.rs`) does not copy; it is persisted with each decision. Telemetry is untouched;
the change is in the pure policy.

1. **State** (`src/policy.rs`): per device, a `GpuDemand` in the `PolicyEngine`: the amounts of the
   last 60 uncapped intervals (the external total and each principal UID's amount), whether the
   last two decisions sent a cap or pause to own work on the device, the external PID list at the
   last interval, the time of the last uncapped interval, and an open window's progress.
2. **The estimate** (`src/policy/measure.rs`): an interval is uncapped when neither the decision
   that set it nor the one before it sent a Cap or Pause, on CPU or on that device's GPU compute,
   to an allocation running on the device, or when no allocation runs there. A measured uncapped
   interval feeds its amounts; an interval of unknown amount feeds nothing. A changed list of
   external PIDs, background contexts left out, clears the estimate. `compute_budget_for` builds Neutral's, Performance's and Aggressive's
   view from the estimate (per principal through `principals`' usage closure, the total from the
   averaged totals), and Yield First's target from the larger of the interval's amount and the
   estimate, so FR-065 holds. With no uncapped interval since the estimate started (the agent's
   start or a restart), the interval's amount is used, as before.
3. **The window** (`src/policy/ladder.rs`): when every running allocation on the device runs in
   Neutral, Performance or Aggressive, at least one holds a GPU compute cap, none holds a CPU step
   of cap or more or a GPU pause, and the last uncapped interval is 30 s old, a window opens for 6
   decisions. During it the device's GPU compute steps hold as they are and their actions are left
   out, so each supervisor lifts the duty and the cap returns unchanged after the window. Its 5
   uncapped intervals (the first is not, by the two-decision rule) feed the estimate; if their mean
   differs from the estimate before the window by 200 or more, the estimate restarts from them.
4. **The record** (`src/model.rs`): `ResourceStatus.gpu_demand` on GPU compute: whether the
   interval was uncapped and fed the estimate, the estimate (the external total), its interval
   count, the time since the last uncapped interval, whether a window is open, and which amount set
   the target (the estimate, the interval's amount, or for Yield First the larger).
5. **Tests first** (`tests/contention.rs`): the loop from T368's Performance light trace (a cap, a
   higher reading, a deeper cap) fails on today's code and holds own work's target near capacity
   less the estimate after the fix; a light user turning saturating while own work is capped is
   seen at the next window and the target falls to the fair share; the edges (no uncapped interval
   yet, own work idle on the device, a PID that leaves, a new policy engine, Yield First's targets
   pinned on the same fixtures). Replays through the policy of 036's Performance and Neutral light
   runs and 038's Neutral saturating run report the targets with the estimate. The decision
   snapshots are compared with the code at `15aa96e`; those with a GPU compute status gain the
   record, and those whose target changes are listed.
6. **Afterwards**: the full suites, the macOS serial suite and the Docker Linux check; T368 on the
   test host waits for the user's typed approval.

**Constitution Check (7.0.0)**:
- II: Performance and Aggressive no longer give own GPU work away to a reading that own work's gaps
  inflate; the other user stays at min(demand, fair share), its demand read where own work ran
  uncapped, and a rise is seen within one or two windows, while the time-slicer keeps giving it
  its slices meanwhile. Pass.
- III: an amount of unknown activity keeps its path and feeds nothing; with no uncapped interval
  yet the interval's amount is used, as before; a new external process restarts the estimate.
  Pass.
- IV: the estimate and the windows live in the policy's own state, from the ladder it already
  keeps; telemetry stays read-only. Pass.
- VI: tests first. FR-065: Yield First takes the larger of the interval's amount and the estimate,
  so its target stays at or below Neutral's. Pass.

## Item 040: the GPU duty from own share (2026-11-01)

Spec Session 2026-11-01 Q20 and Q21 (FR-021's duty-cycle sentence, SC-017), answering report 039's
Decisions 1 and 2. A GPU compute cap was a duty of target ÷ 1000, which assumes own work at full
duty holds the whole device; on a time-sliced device beside another busy context it holds only its
slices, so the cap cut own work's running time by far more than its excess.

**Technical Context**: Rust 1.88, no new dependency. No wire, SDK or supervisor change: the policy
sends a GPU compute cap's target to the supervisor as the duty in thousandths, which the supervisor
already applies as target ÷ 1000; the ladder keeps its targets in device shares.

1. **Own work's uncapped share** (`src/policy.rs`, `src/policy/measure.rs`): each uncapped interval
   in which own work ran on the device also keeps own work's measured amount; the share is their
   average (at most 60), restarted with the external estimate (a changed external PID list, a
   window's restart) and when the set of own allocations running on the device changes.
2. **The duty** (`src/policy/ladder.rs`): in `collect_actions`, a GPU compute Cap for an allocation
   that does not run in Yield First, on a device measured this interval and not for unknown
   activity, with an uncapped share S known, goes out with target min(1000, target × 1000 × n ÷ S),
   n the own allocations running on the device, each holding S ÷ n; otherwise as before. The
   supervisor's minimum duty still applies. A Cap sent at 1000 holds nothing back, so it does not
   count as capped for 039's two-decision rule.
3. **The record** (`src/model.rs`): `GpuDemandRecord.own_uncapped` (S) and `duty` (the duty sent,
   in thousandths, the smallest over the device's caps).
4. **Tests first** (`tests/contention.rs`), on a time-sliced device model in which each busy
   context gets an equal slice of the time the device is busy: one own context beside a saturating
   user in Performance and Neutral (duty at least 0.95, own work within 3 points of unmanaged);
   039's light shapes (the duty against capacity less the estimate and the margin); two own contexts
   beside one saturating context in Neutral (duty near 0.75, own work within 3 points of 500);
   convergence across the two-decision rule and the windows; the edges (no share yet, a share
   below the target, a new engine, Yield First pinned to its duties and targets). Replays of 036's
   Performance saturating and light and Neutral light runs and 038's Neutral saturating run report
   the duty each would set. The decision snapshots are compared with the code at `f6ebac7`.
5. **Records**: `docs/validation.md` notes that 036's Performance saturating 0.628 and 038's Neutral
   saturating 0.513 are over-yields under SC-017; `native-gates.md`'s T445 adds the probe-alone and
   saturating-alone windows and reports both forms of each ratio.
6. **Afterwards**: the full suites, the macOS serial suite and the Docker Linux check; T445 waits for
   the user's typed approval.

**Constitution Check (7.0.0)**:
- II: Performance and Aggressive no longer cut own work's running time by more than its excess when
  own work holds only its slices; the other user keeps min(demand, fair share), which the target
  sets. Pass.
- III: with no uncapped share, on a device of unknown amount, or for a cap set for unknown activity,
  the duty stays target ÷ 1000, which yields more. Pass.
- IV: the policy stays pure; the supervisor is unchanged. Pass.
- VI, VII: tests first; a GPU compute Cap action's target is now a duty in thousandths, and the
  decision record carries both the share target (the ladder's step) and the duty. Pass.

## Item 041: the GPU time-slice rule (2026-11-01)

Spec Session 2026-11-01 Q22–Q26 (FR-021's GPU compute paragraph, FR-022's GPU bullets, FR-059,
the Contended term, SC-017), answering T445's report. Four host runs in a row showed caps built on
per-process readings costing own work for no gain; the readings are coarse (steps of about 330),
sparse and biased under time-slicing, while the time-slicer itself already gives each busy
context its slice (research R50).

**Technical Context**: Rust 1.88, no new dependency: `nvml-wrapper` 0.11 already has
`Device::mig_mode`. No wire, SDK or supervisor change. The snapshot's `GpuSnapshot` gains optional
fields (serde default), and the decision record a time-slice record.

1. **Telemetry** (`src/telemetry/gpu.rs`, `src/telemetry.rs`, read-only):
   - `GpuSnapshot.time_sliced`: true when both process queries succeeded with no MPS control
     daemon or server listed and MIG mode is disabled or not supported; false with MPS or MIG
     enabled; unknown when a process query or the MIG query fails.
   - `GpuSnapshot.compute_contexts`, when the device's process inventory is complete: own work's
     verified PIDs listed, the owner's unmanaged PIDs listed, and each non-background PID with a
     nonzero sample this interval, with its UID when readable.
2. **Counting** (`src/policy.rs`, `src/policy/measure.rs`): each uncapped entry of the estimate
   also keeps the interval's sampled contexts, so the counts share its 60-interval window and its
   restarts. A PID no longer listed stops counting at once, and a new external PID restarts the
   estimate.
   - m: own work's sampled PIDs still listed, with the owner's unmanaged ones, since the owner is
     one principal (FR-059).
   - E: the other sampled PIDs still listed, UIDs below `uid_min` left out.
   - k: their distinct principals, a PID whose UID cannot be read counting as the unattributed
     principal.
   - An unattributed principal from the averaged gap (step 4) adds one principal with one context.
3. **The rule** (`src/policy/measure.rs`, `src/policy/ladder.rs`):
   - On a time-sliced device, with contexts known, the rule holds when m × k ≤ E on the measured
     path, or m ≤ 1 on the unknown path (m from own work's and the owner's listed PIDs).
   - While it holds, the device's budget gives Neutral, Performance and Aggressive the whole
     device, and the ladder ends those allocations' GPU compute steps on the device at once: no
     cap, no pause and no hold-then-escalate.
   - Yield First allocations plan as before. Admission and VRAM are unchanged.
4. **Decision 1** (`src/contention.rs`, `src/policy/measure.rs`):
   - GPU compute's unattributed principal needs `attribution_tolerance_percent` of capacity
     (150 at the default) in `contending_principals`, `neutral_share` and `probe_demand`.
   - The estimate's entries leave the owner's unmanaged usage out of the external total. The
     averaged view adds the interval's own amount of it back, as the interval's view counts it.
5. **Decision 2** (`src/policy.rs`, `src/policy/ladder.rs`): `own_share` needs 6 entries. While
   the share has fewer, a re-measure window opens at once rather than 30 s after the last
   uncapped interval, and the duty stays target ÷ 1000.
   - A window ending short opens the next at once, because a 6-decision window feeds only 4–5
     uncapped intervals under the two-decision rule.
   - The bootstrap ends as soon as the share has 6 entries, about two windows, since every
     measured uncapped interval of a running own allocation adds one.
6. **The record** (`src/model.rs`, `src/contention.rs`): `ResourceStatus.time_slice` carries
   `own` (m), `external` (E), `principals` (k) and `holds`, present only on a device known to be
   time-sliced. Reason code `gpu_time_slice_fair`.
7. **Tests first**, in `tests/contention.rs`, on 040's time-sliced model with contexts:
   - one own context beside the saturating and the light shapes, in Performance and Neutral: no
     cap, own work at unmanaged;
   - two own contexts beside one saturating context: 040's duty settles within 3 points of 500,
     and the cap is gone within one window after one own context leaves;
   - two own contexts beside an external principal with two contexts: no cap;
   - the phantom gap: 10 and 14 points form no principal, 15 and 20 do and count as external
     demand;
   - the share bootstrap;
   - the edges: MPS, a background context, a leaving process, the unknown path with m = 1 and
     m = 2, Yield First pinned to its targets and duties.
   Telemetry unit tests cover `time_sliced` and the contexts. Replays of T445's four runs report
   m, E and k. The decision snapshots are compared with `d101610`.
8. **Docs**:
   - `docs/guarantees.md` and the README's GPU compute sentences: on a time-sliced GPU, CedeGrid
     relies on the driver's time-slicing for a job that holds no more than its share of contexts,
     and this rests on the driver giving busy contexts equal slices. MPS and MIG keep the measured
     path.
   - `native-gates.md`'s T456: T368's four runs and a two-probe-process run in Performance and in
     Neutral beside the saturating user. The gate is SC-017, and in the two-process runs the other
     user must also reach ½ × its alone rate × 0.97.
9. **Afterwards**: the full suites, the macOS serial suite and the Docker Linux check, with
   `CARGO_BUILD_JOBS=2` if its linker runs out of memory. T456 waits for the user's typed approval.

**Constitution Check (7.0.0)**:
- I: telemetry only reads MIG mode, the process lists and samples; nothing acts on an external
  process. Pass.
- II:
  - Under the rule own work holds at most its fair share of the busy contexts, 1 ÷ (k + 1), so
    Performance's loss rule and every other principal's min(demand, fair share) hold.
  - When own work holds more, the estimate and 040's duty still cap it.
  - Yield First is unchanged. No margin is held idle under the rule, which is work-conserving:
    a newcomer gets its slices at once.
  - Pass.
- III:
  - On the unknown path the rule holds only with m ≤ 1, where unseen contexts can only lower own
    work's share.
  - An unreadable MPS or MIG state, or an incomplete inventory, is not taken as time-sliced, and
    keeps today's graduated path.
  - The unattributed amount below the tolerance still counts as external demand.
  - Pass.
- IV: the policy stays pure; the counts come from the snapshot. Pass.
- VI: tests first. Pass.
- VII: the new snapshot and record fields are optional, so older snapshots load. Pass.

## Item 043: the time-slice duty from the context counts (2026-11-01)

Spec Session 2026-11-01 Q27–Q30 (FR-021's time-slice duty, FR-022's GPU bullets, FR-025, the
several-own-jobs edge case), answering 042's Decision 1. At m × k > E, 042 showed caps from
single quantized readings, pauses that never reversed beside a saturating user, and a target
read from a slice. This replaces that path on a time-sliced device with a duty from the context
counts, stepped up from the device's own utilization (research R51).

**Technical Context**: Rust 1.88, no new dependency, no wire, SDK or supervisor change.
- **Config:** two keys under `contention.gpu_compute`, `slice_bound_percent` (75, 50–100) and
  `slice_step_percent` (5, 1–50). They are GPU compute only, as `attribution_tolerance_percent`
  is (FR-025).
- **Snapshot:** `GpuComputeContexts` gains the listed external PIDs with their UIDs.
- **Record:** the time-slice record gains the slice-bound principals, d0, the duty, the step state
  and the device's utilization.

1. **Telemetry** (`src/telemetry/gpu.rs`): `compute_contexts` also lists each external
   non-background PID that is not the owner's, with its UID when readable.
2. **The slice-bound check** (`src/policy/measure.rs`):
   - A principal's reading is its average in the external estimate (039). For a lone external
     principal it is the estimate's total, since usage below the attribution tolerance is that
     principal's sampling error (Q23).
   - E_j counts its sampled contexts in the estimate's window that are still listed. An
     unattributed principal counts as one with one context.
   - It is slice-bound when the reading is at least `slice_bound_percent` × 1000 × E_j ÷ (m + E).
3. **The duty state** (`src/policy.rs`, per device in `GpuDemand`):
   - d0 = (m + E_b) ÷ (m × (k_b + 1)), clamped to the minimum duty and 1.
   - The duty rises by `slice_step_percent` points once per max(the mode's scale-up interval, the
     step dwell) while the device's utilization, averaged over the dwell, leaves spare time above
     the mode's margin.
   - It falls a step once per dwell while the spare time is below half the margin (Aggressive:
     while the device is fully busy), and holds between.
   - It is never below d0, starts at d0 and resets when the path ends. A duty of 1000 stays a cap
     at full duty.
4. **The ladder** (`src/policy/ladder.rs`):
   - On such a device, Neutral, Performance and Aggressive allocations are taken out of planning
     and keep a Cap state at the duty; `gpu_duty` returns it, and no step pauses.
   - Re-measure windows run every 30 s, without 042's share bootstrap. One opens at once while the
     estimate has no entries, as after a restart.
5. **The unknown path:**
   - With the listing known but the amounts unknown or inconsistent, d0 comes from the listed
     counts, every listed principal bound, and there is no step-up without a utilization reading.
     The same applies while the estimate's window is empty.
   - With the listing unknown on a device last read as time-sliced, the ladder caps at target ÷ 1000
     with Pause unavailable.
6. **Tests first** (`tests/contention.rs`), on a tick-level time-sliced model: own work's m
   contexts run at the duty; each external principal has a demand, and every busy context gets an
   equal share of each tick. The quantized readings come from that model.
   - m = 2 beside a saturating context: 500 ± 30, no pause, the duty unchanged by readings of 0,
     330, 660 and 990.
   - m = 2 beside a light context: no cap.
   - m = 2 beside a partial context at 40%: the duty steps up, own work lands within 30 of
     1000 − 400 − the margin, and the partial user gets at least 0.97 × 400.
   - m = 3 beside one, and m = 2 beside two saturating contexts.
   - m = 2 beside a saturating and a light context.
   - The rise, the leave, the unknown path, and m × k ≤ E, Yield First and MPS pinned.
   - Config validation tests. Replays of 042's valid windows, the unmanaged two-process windows
     and T368's light traces. The decision snapshots compared with `c78f240`.
7. **Docs:** `docs/guarantees.md` and the README GPU section, as the item words it, and the two
   config keys where the config reference lists `attribution_tolerance_percent`.
8. **Harness (uncommitted):** managed probes submitted 40 s ahead through `start_at.py`.
9. **Afterwards:**
   - the full suites, the macOS serial suite and the Docker Linux check;
   - then T456's remaining runs on the test host: Performance and Neutral with two probes beside
     the saturating user, and Performance with two beside the light user.

**Constitution Check (7.0.0)**:
- I: telemetry only reads; caps act on own processes only. Pass.
- II:
  - A slice-bound principal keeps its fair share of the time it wants, and a principal that isn't
    bound keeps its slices, so others keep min(demand, fair share).
  - The step-up returns time the device leaves idle beyond the margin, which is work-conserving.
  - No pause on this path is a softer response, but d0 already holds own work at its share, and
    FR-022 forbade Performance's long pauses anyway.
  - Pass.
- III:
  - Unknown amounts with a known listing take d0 with every listed principal bound, the strictest
    duty the counts give.
  - An unknown listing keeps Q25's target ÷ 1000, without a pause; there is no relaxation beyond
    that.
  - Pass.
- IV: the policy stays pure; the duty state lives in the engine's carried state. Pass.
- VI: tests first. Pass.
- VII: the snapshot, record and config fields are optional, with defaults. Pass.

## Item 044: aligned GPU duty phases, the margin at d0 and the busy counts (2026-11-01)

Spec Session 2026-11-01 Q31–Q34 (FR-021's duty-cycle sentence, d0 with the margin, the count
source and the record; the several-own-jobs edge case; SC-017's targets), answering 043's
Decisions 1–2 and 043-t456's Decisions 1–2. At d0 = 750 own work took 0.515–0.522 rather than 0.5
because each allocation's duty cycle runs from its own start, and the listing path counted an idle
context (research R52).

**Technical Context**: Rust 1.88, no new dependency (`libc::clock_gettime` is already used), no
wire, SDK or config change.
- **Snapshot:** `GpuComputeContexts` gains whether the interval's per-process samples were
  readable.
- **Record:** the time-slice record gains the source of its counts.

1. **Aligned phases** (`src/actuation.rs`, and `FreezeDuty` in `src/cgroup.rs`):
   - While a GPU compute cap is part of an allocation's duty cycle, its phase follows the node's
     clock, CLOCK_MONOTONIC, modulo the period. The period is the monitor interval, which every
     supervisor on the node uses.
   - Own work runs while the position in the period is below duty × period.
   - A stop holds until the period ends. So a duty change mid-period shortens or lengthens the
     current on-phase, never resumes inside an off-phase, and never skips one.
   - `next_deadline` is the next switch: the end of the on-phase while running, the period's end
     while stopped.
   - The node clock is `clock_gettime(CLOCK_MONOTONIC)` on Unix. Elsewhere, where duty cycles are
     unavailable anyway, it falls back to a process-local monotonic anchor so the crate builds.
   - A CPU-only cycle keeps its per-allocation phase, with the CPU rate measurement unchanged.
     When CPU and GPU caps share one cycle, it follows the aligned phase at the smaller duty.
2. **The margin at d0** (`src/policy/measure.rs`):
   - d0 = (m + E_b) ÷ m × (1 ÷ (k_b + 1) − margin), with the mode's margin: Performance 20,
     Neutral 50, Aggressive 0 thousandths. It is clamped to the minimum duty and 1000.
   - The steps' floor is this d0.
3. **The count source** (`src/telemetry/gpu.rs`, `src/policy/measure.rs`):
   - **Window:** the estimate's window, 041's counts. When the interval's amounts are measured, its
     readings decide which principals are slice-bound.
   - **Interval:** this interval's sampled busy contexts, when the window has none and the samples
     are readable.
   - **Listing:** only when the samples are unreadable.
   - With unknown or inconsistent amounts, or no window, every counted principal is bound, and the
     record carries the source.
4. **Tests first:**
   - `src/actuation.rs` unit tests:
     - the aligned on/off rule;
     - two actuators started 0.3 of a period apart stop and resume together;
     - a duty change mid-period (shorten, lengthen, no resume inside an off-phase);
     - `next_deadline`;
     - a CPU-only cycle keeping its own phase.
   - `src/cgroup.rs`: the freeze duty aligned the same way.
   - `tests/contention.rs`, a phase-level model with own allocations on or off within each period:
     - caps 0.3 of a period apart give own work more than 0.52 at duty 750 on today's per-allocation
       phase, and within 1 point of m × d ÷ (m + E) when aligned;
     - alignment holds across a duty step, a re-measure window, three allocations and a late
       joiner.
   - `tests/contention.rs`, the shapes:
     - with the margin, 480, 450 and 500 at m = 2 beside a saturating context;
     - an idle listed context doesn't count while samples are readable, and does when they aren't;
     - the light shape with an idle listed context stays uncapped.
   - Replays of 043's six managed windows: each decision's source and d0. The recordings predate
     `samples_readable`, so the replay sets it from the recorded `compute_sampling`, readable when
     that lists processes.
   - The decision snapshots, compared with `c2ae762`.
5. **Docs:** `docs/guarantees.md`, covering the aligned phase, the margin at d0 and the busy
   counts.
6. **Harness (uncommitted):** an Aggressive two-process part beside the saturating user, and the
   probes' off-phase overlap. No product record holds the supervisors' stop and resume times, so
   the harness samples its own probe processes' run state from `/proc/<pid>/stat` (stopped or
   running), read-only, about every 10 ms. From that it gives each window's off-phase overlap and
   own work's share against m × d ÷ (m + E) at the duty held.
7. **Afterwards:** the suites, the macOS serial suite and the Docker Linux check. Then the test-host
   runs:
   - Performance and Neutral, two probes beside the saturating user;
   - Performance, two beside the light user;
   - Aggressive, beside the saturating user, for the record.

**Constitution Check (7.0.0)**:
- I: the aligned phase changes only when own processes are stopped and continued, through the same
  owned handles. Pass.
- II:
  - The margin at d0 holds each mode's documented margin where its target would, so others keep
    min(demand, fair share) plus the margin.
  - Aligned phases make own work's share match the counts, rather than exceed them.
  - Pass.
- III:
  - Only contexts seen busy count, and an unreadable sample set still falls back to the full
    listing, every principal bound. Unknown never reads as idle.
  - Pass.
- IV: the policy stays pure, and the clock is read in the supervisor's actuator. Pass.
- VI: tests first. Pass.
- VII: the snapshot and record fields are optional, with defaults. Pass.

## Item 045: the step floor, the per-mille dwell average and the restart set (2026-11-01)

Spec Session 2026-11-01 Q35–Q36 (FR-021's step bullet, FR-022's restart sentence), answering
044-t456's Decision 1 and 044's Decision 1. With a zero margin the duty rose on dwell averages
of 99, floored from 99.7 by a dip as phases switched. The same floor kept Performance from
falling. And the owner's unmanaged processes restarted the external estimate when they came or
went (research R53).

**Technical Context**: Rust 1.88, no new dependency, no wire, SDK or config change. The
time-slice record's dwell average becomes per mille: `utilization_milli` replaces `utilization`.

1. **The step rule** (`slice_duty` in `src/policy/measure.rs`):
   - The dwell's average is (sum × 10 + len ÷ 2) ÷ len per mille, and spare = 1000 − average.
   - floor = max(margin, 10).
   - The duty rises while spare > floor, and counts as saturated while spare × 2 < floor.
   - The step and the dwells are unchanged.
2. **The restart set** (`gpu_demand` in `src/policy/measure.rs`):
   - It leaves out the owner's unmanaged PIDs (`compute_contexts.owner`) beside the background
     ones.
   - A unit test pins that telemetry's external PID list holds the owner's unmanaged PIDs, so they
     do reach the set today.
3. **Tests first:**
   - **The dwell average:** readings 100, 100, 99 hold the duty with a zero margin, where today
     they raise it. Performance above d0 falls on them, where today it holds.
   - **Zero margin beside the saturating shape,** with one reading in three at 99: the duty stays
     at d0, own work is within 30 of 500, and the external context keeps at least 485.
   - **The partial user at 40% with a zero margin** settles within 30 of 1000 − 400 − 10.
     Performance and Neutral stay as before.
   - **The restart set:** an owner's unmanaged process that comes or goes keeps the estimate, and a
     new external PID still restarts it.
   - **Replays of 044-t456's traces:** the recorded steps, from the old rule, against the replay
     at HEAD.
   - **Decision snapshots** compared with `b3b426a`.
4. **Docs:** `docs/guarantees.md` states that with a zero margin CedeGrid takes device time only
   beyond 1% averaged spare. Also the data model.
5. **Afterwards:**
   - the suites, the macOS serial suite and the Docker Linux check;
   - then the test-host runs: zero margin beside the saturating user, gated (the saturating user
     at least ½ × alone × 0.97, own work by SC-017 at 500), and Performance as a regression check.

**Constitution Check (7.0.0)**:
- I: no change to what CedeGrid acts on. Pass.
- II:
  - With a zero margin, own work takes device time only where the averaged reading shows it idle
    beyond its resolution, so others keep min(demand, fair share).
  - Performance and Neutral keep their margins, and now fall back to d0 where the floor had
    blocked them.
  - Pass.
- III: the owner's processes are own, and their usage is already out of the estimate. Leaving
  them out of the restart set reads nothing unknown as idle. Pass.
- IV: the policy stays pure. Pass.
- VI: tests first. Pass.
- VII: the record's field is optional. Pass.

## Item 046: own work pays for the re-measure windows (2026-11-01)

Spec Session 2026-11-01 Q37 (FR-021's d0' bullet, its floor, the bounds and the record),
answering 045-runs' Decision 1, A without the fixed 1%. At a zero margin, a 3 s re-measure window
in each 40 s managed window lifted own work's held duty from 0.75 to about 0.773. The saturating
user got 0.468–0.472 of its rate alone, against 0.485 (research R54).

**Technical Context**: Rust 1.88, no new dependency, no wire, SDK or config change. The
time-slice record gains w (per mille) and d0'.

1. **d0'** (`slice_duty` in `src/policy/measure.rs`):
   - w = D ÷ (D + G), with D = `GPU_DEMAND_WINDOW_DECISIONS` and G =
     ⌈`GPU_DEMAND_WINDOW_EVERY_MS` ÷ the monitor interval⌉.
   - d0' = (d0 × (D + G) − 1000 × D) ÷ G, rounded, clamped to [50, 1000]. At 500 ms: 725 with no
     margin, 692 in Performance, 643 in Neutral.
   - The duty state's floor is d0'.
   - The window's first interval counts as full duty for own work. Its decision lifts the cap at
     once, though the interval feeds no estimate, since the decision before it still sent the cap.
   - Windows can open unless the governing mode or an allocation on the device is in Yield First,
     or an allocation there holds a CPU step of cap or more. Where none can open, d0' = d0.
2. **Tests first** (`tests/contention.rs`):
   - **Zero margin beside the saturating shape, over the whole run, windows included:** the
     context gets at least 485 (it gets 484 on today's code), and own work's held duty averages
     within 5 of 750.
   - **Performance and Neutral:** the context gets at least 490 and 520, and own work is within 30
     of 480 and 450.
   - **No windows**, with a Yield First neighbour on the device: d0' = d0.
   - **The partial user's settle point** is unchanged.
   - **The shapes' first duties** become 725, 692 and 643.
   - **Replays** of 045's four managed windows, with d0, w and d0'.
   - **Decision snapshots** compared with `f56e179`.
3. **Docs:** `docs/guarantees.md` says own work pays for CedeGrid's re-measure windows on a
   time-sliced device, so others keep their share over time.
4. **Afterwards:**
   - the suites, the macOS serial suite and the Docker Linux check;
   - then the test-host runs: zero margin, gated as in 045; Performance and Neutral, under T456's
     gates.

**Constitution Check (7.0.0)**:
- I: no change to what CedeGrid acts on. Pass.
- II:
  - The windows are CedeGrid's measurement cost. Own work now bears it, so others keep min(demand,
    fair share) over time, and Performance and Aggressive cost others nothing beyond that.
  - Own work's loss is the windows' share, as Principle II allows for CedeGrid's own overhead.
  - Pass.
- III: no change to unknown handling. Pass.
- IV: the policy stays pure. Pass.
- VI: tests first. Pass.
- VII: the record's new fields are optional. Pass.

## Item 049: Phase 37 before the merge (2026-11-03)

**Input**: spec Session 2026-11-03; FR-018, FR-021's clock sentence, FR-034's charge and cooldown
text, the Principal term; `tasks.md` Phase 37; report 047's and 048's findings.

**Technical Context**: Rust 1.88, no new dependency. No SDK, config or wire change. The record gains
text in a reason, not a field a client reads. Native reruns on the test host under the standing
approval.

**Done before this section**: T509 (an interval without a utilization reading clears the dwell's
readings), T510 (a process that joins a frozen leaf is thawed at once, and the step moves to the
rootless mechanism), and `af531c0` (the CPU busy interval is measured between the counter reads).

**T515, the charge when alone (R55)**. `charge()` runs first in `evaluate`, before the principal
phase, so whether another principal is active comes from the snapshot's principal list directly:
another user ID at or above `uid_min`, or the unattributed usage, at or above
`principal_min_percent` of capacity in this interval. It does not wait for the principal window, so
the switch to p95 comes in the interval a principal appears. An unreadable list counts as active
(Principle III). Only the charge's statistic changes: the startup charge, the held-back charge,
pending work, Guaranteed work and memory stay as they are. Tests first: a burst while alone
over-admits by at most what own work then caps on itself; the switch happens in the interval a
principal appears; running tasks are never drained because of it.

**T518, an own task's missing sample (R56)**. Two parts.
- *The record.* `sample_records` builds each allocation's bounds through a chain of checks. Each
  failure now returns a short name (`not_verified`, `not_alive`, `scope_changed`,
  `resident_unreadable`, `no_baseline`, `first_interval`, `identity_changed`, `no_end_reading`,
  `counter_went_back`, `no_cpu_window`, `outside_cpu_window`, `ram_unreadable`). `Allocation` carries it,
  and `allocation_unknown_hold`'s reason names it.
- *The external estimate.* When the busy window is missing after a reset, `cpu_external` uses this
  interval's estimate alone, whose bound is wide, so the looseness reads as growth for up to a
  window. The last window estimate is kept in `PolicyState` and used while the window refills, under
  the same rule as the window: while this interval's estimate stays within its own bound of it, it
  applies; beyond that bound, growth applies at once. It ages out after one window.
- *Found by the idle gate.* The estimate is carried through the unknown reading that reset the
  window, which returned before carrying it. And idle ticks may run past the interval between the
  reads' starts by this read's length, since a CPU's counter is read after the read begins; one tick
  of rounding did not always cover that, and the reading went unknown for an interval. More idle than
  the read's length explains stays unknown.

**T519, Yield First's cooldown (R57)**. `cooldowns()` restarts stability when `cpu_safe` is false.
In Yield First, `cpu_fits` going false from a change that forms no principal no longer restarts it:
the timer keeps running while admission holds, since headroom is computed apart. A principal has
formed when a user ID other than the owner is an admitted member on the resource, or the unattributed
usage has stayed at or above `principal_min_percent` for `principal_window_intervals`. An unknown
measurement, an unreadable principal list or an operator drain restarts it as before. RAM gets the
same rule, counted from the level of what no process lists, since kernel memory sits in that
remainder for good; a rise that lasts past the window becomes the next level. Neutral, Performance
and Aggressive keep theirs. The ladder is unchanged, so give-back and SC-002 hold. Tests first: an
owner login-sized burst does not restart it; another account's one-interval 300 m change does not;
another account's change that lasts the principal window does, and Yield First steps down within
SC-002's bound.

**T513, supervisor CPU**. CedeGrid's own CPU already counts the agent and its supervisors apart
(FR-069). T513 measures how much of the 173 m gap of `docs/validation.md` is left at HEAD from the
SC-014 runs' report (tasks' CPU from `/proc` against the decisions' upper bound), and charges each
supervisor's CPU to its allocation if the gap is still there. If it is gone, T513 records that and
changes no code.

**T512, a new victim's excess**. The ladder gives each new victim, in cost order, the whole excess
left, so a cap can reach 0 and become a pause. New victims share the excess by usage within their
tier, as held caps already do, so no new victim is capped below its share of it. Tests first in
`tests/contention.rs`.

**T514, the busy window's bound**. Per-CPU busy nanoseconds from `/proc/schedstat` (the
`cpuN` run time field), where readable, tighten the window's busy upper bound below the idle
complement, whose ticks lag elapsed time on an idle CPU. The counter adds a slice only when it leaves
the CPU, so a task still running shows only in the ticks: each CPU's bound is the larger of its busy
ticks and its run time, plus one tick, and never above the complement. Unreadable, zero or
inconsistent values, and formats other than versions 15 to 17, fall back to the ticks. Telemetry
stays read-only. Report 027's run 1 on 4 CPUs gates it: the external estimate at most 1%.

**T511, a refusal at the barrier**. The coordinator decides from its own record, so no cause value
or wire change is needed: a preparation reached it and no lease ever went out (`lease_sequence` 0),
with no drain or cause recorded. That attempt takes the yield path, adds no failure and leaves a
receipt `authorization_refused`, which `counted_attempts` leaves out, so a replay-safe task is
requeued without using an attempt, and the allocation's detail says why. A launch that failed before
the barrier, or one after a grant, still counts. A task that isn't replay-safe keeps today's end.
Test first in `tests/coordinator.rs`.

**T516**: the spec sentence, done in clarify.

**Constitution Check (7.0.0)**:

| Principle | How it complies |
|---|---|
| I. External processes untouchable | T510 thaws a leaf a process joined and moves the step to the verified family. T519 and T515 only read principals. |
| II. No harm | T515's mean applies only with no other principal active, and p95 returns in the interval one appears. T519 leaves the ladder and SC-002 alone. Over-admission while alone costs own work only. |
| III. Unknown is never free | T509 removes a rise on stale readings. An unreadable principal list keeps p95. The kept window estimate ages out, and growth beyond the bound applies at once. Unknown measurement still restarts the cooldown. |
| IV. Pure policy | The new state is in `PolicyState`; no clock or file reads in `policy/`. T518's names come from telemetry as data. |
| VII. Compatibility | No SDK, config or wire change. T511 decides from the coordinator's own record, and its receipt kind is not a wire value. |

Result: pass.

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| Re-attachable supervisor `control.sock` | Steps must reach supervisors that outlive an agent restart (FR-015). Without it the new agent cannot release capacity without draining. | The stdin pipe cannot be reopened. Re-seeding supervisor engines keeps N+1 authorities. |
| `toml_edit` as a direct dependency | Setup must preserve comments and unmanaged settings (FR-053). | Re-serializing with `toml` loses comments. `toml_edit` is already in `Cargo.lock`, so no new crate is added. |
