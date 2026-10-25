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
| **cgroup controls written once** (H7) | `LaunchBackend::actuator()` with runtime `cpu.weight`, `cpu.max` and `cgroup.freeze` (no runtime `memory.high` yet: FR-011 starts memory at the hint; T138 adds it later and amends FR-011). Each write re-verifies leaf identity and that membership equals the verified PIDs, reads back and journals. A `Limit::{Max, Value}` type, and `assert_empty` replaced for adjustable controls. Advertised controls come from actual capability (H8). | `cgroup.rs`, `execution_model.rs`, `rootless.rs`, `agent.rs` |
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

- **Mixed modes.** Until jobs can request a mode (T093), the least aggressive effective mode among
  the running allocations on a resource governs that resource's target. Because targets are
  ordered (FR-065), this is the most conservative target. T093a replaces it with the target model
  below.

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
- **B14 Runtime `memory.high`.** A Polish task (T138): on a delegated cgroup, a RAM cap step that
  sets `memory.high` before the hint, verified by readback.

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
- **U7, U11, U13.** The allowance signal is specified now and implemented after US5 (T150); the
  constitution amendment is above; T088 also measures how often GPU activity is Unknown.

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| Re-attachable supervisor `control.sock` | Steps must reach supervisors that outlive an agent restart (FR-015). Without it the new agent cannot release capacity without draining. | The stdin pipe cannot be reopened. Re-seeding supervisor engines keeps N+1 authorities. |
| `toml_edit` as a direct dependency | Setup must preserve comments and unmanaged settings (FR-053). | Re-serializing with `toml` loses comments. `toml_edit` is already in `Cargo.lock`, so no new crate is added. |
