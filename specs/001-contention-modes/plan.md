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
  `execution_state.rs`, and the coordinator distributed schema, which moves 3 → 4 through an
  explicit migration (R14).
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
  - Success criteria SC-002 to SC-004 use the provisional, configurable defaults.
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
| II. No harm in any mode (1.1.0) | Floors are on by default. Policies B and C are opt-in, marked, and confirmed (danger). `oom_score_adj` is only raised. Negative nice is clamped to 0. No priority is ever raised. | Pass |
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
| **cgroup controls written once** (H7) | `LaunchBackend::actuator()` with runtime `cpu.weight`, `cpu.max` and `cgroup.freeze` (no runtime `memory.high`: FR-011 starts memory at the hint). Each write re-verifies leaf identity and that membership equals the verified PIDs, reads back and journals. A `Limit::{Max, Value}` type, and `assert_empty` replaced for adjustable controls. Advertised controls come from actual capability (H8). | `cgroup.rs`, `execution_model.rs`, `rootless.rs`, `agent.rs` |
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
| `src/setup.rs` | New: setup choices, risk mapping, flag parsing, terminal interface (termios through `libc`), `toml_edit` writer |
| `src/policy.rs` | Budget and admission keep their shape. Calls `contention`. D5 and D6 fixes. Removes the protection cap |
| `src/telemetry.rs`, `src/kernel.rs` | Reserved/free, double-read memory, quiet class, bounds, health versus freshness. One collector per node |
| `src/model.rs` | Snapshot schema 3; `Decision.resources`, `Decision.actions`, `Decision.exceptions`; `YieldAction`; `ReasonCode` |
| `src/config.rs` | `[contention]`, deprecation warnings, `cpu.nice` clamp, cross-validation of staleness knobs |
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
6. **GPU sharing and placement (US5).** Launch recheck by mode; coordinator per-resource
   admission.
7. **Configuration and compatibility (US6).** `[contention]`, deprecations, negotiation,
   schema migrations, mixed-version matrix.
8. **Setup (US7).** Flags first (shared with agents in spec 002), then the terminal
   interface.
9. **Observability (US8).** `observe`, `doctor`, `status` and node reports; reason codes;
   `docs/guarantees.md`.
10. **Native evidence.** cgroup and GPU gates (quickstart §4–5), after which thresholds may be
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
├── agent.rs  supervision.rs  cgroup.rs  coordinator.rs  cli.rs  configuration.rs
└── fixtures/0.2/        # recorded 0.2 heartbeat request/response fixtures
python/cedegrid/worker.py   python/tests/test_worker.py
npm/src/worker.ts           npm/tests/worker-drain.test.ts (new)
docs/guarantees.md  docs/worker-protocol.md  CHANGELOG.md
```

**Structure decision.** Keep the existing single-crate layout. Add three top-level modules,
not subdirectories, because the exporter requires top-level `mod x;`. `tests/fixtures/0.2/`
holds recorded 0.2 protocol fixtures, which must be added to the export allowlist (they are
loaded with `include_str!`).

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| Re-attachable supervisor `control.sock` | Steps must reach supervisors that outlive an agent restart (FR-015). Without it the new agent cannot release capacity without draining. | The stdin pipe cannot be reopened. Re-seeding supervisor engines keeps N+1 authorities. |
| `toml_edit` as a direct dependency | Setup must preserve comments and unmanaged settings (FR-053). | Re-serializing with `toml` loses comments. `toml_edit` is already in `Cargo.lock`, so no new crate is added. |
