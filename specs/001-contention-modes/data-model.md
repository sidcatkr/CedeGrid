# Data Model: Contention Modes with Graduated Yielding

Phase 1 output. These are logical entities, with the Rust module that owns each.
Serialization contracts are in [contracts/](contracts/).

## ResourceKey (`src/contention.rs`)

- One of `cpu`, `ram`, `gpu_compute{uuid}` or `gpu_memory{uuid}`.
- Ordering is stable for deterministic output: cpu, ram, then GPU keys by UUID with compute
  before memory.

## ContentionMode (`src/contention.rs`)

- `yield_first`, `neutral` or `performance`.
- Aggressiveness order: `yield_first < neutral < performance`, used for the node cap.

## ModeConfig (`src/config.rs`, `[contention]`)

| Field | Type | Default | Rule |
|---|---|---|---|
| `default_mode` | ContentionMode | `neutral` | Q1 |
| `max_mode` | ContentionMode | `performance` | `default_mode ≤ max_mode` |
| `floor_policy` | `a` \| `b` \| `c` | `a` | FR-041 |
| `neutral_memory_policy` | `a` \| `b` | `a` | FR-042 |
| `acknowledge_danger` | set of strings | `[]` | Must contain `floor_policy_c` when `floor_policy = "c"`; otherwise load fails (FR-051, FR-054) |
| `[contention.<resource>]` | ResourceThresholds | per-resource defaults | Validation below |

- Legacy `gpu.execution_mode` and `gpu.best_effort_external_processes` are parsed but have no
  effect, with one warning per load (FR-031).
- A negative `cpu.nice` warns and is treated as 0 (R5).

## ResourceThresholds (`src/config.rs`)

These are provisional defaults (spec table). All are configuration values (FR-025).

| Field | Applies to | Default | Validation |
|---|---|---|---|
| `mode` | all | inherits `default_mode` | ≤ `max_mode` |
| `yield_first_margin_percent` | all | 25 | 1–90; > `performance_margin_percent` |
| `performance_margin_percent` | cpu, gpu_compute | 10 | 0–50 |
| `yield_first_response_intervals` | cpu, gpu_compute | 3 | ≥ 1 |
| `scale_up_step_interval_s` | all | 30 (Yield First), 10 (Neutral), 2 (Performance) | ≥ 1 |
| `neutral_convergence_s` | all | 60 | ≥ 1 |
| `neutral_tolerance_points` | all | 15 | 1–50 |
| `performance_ramp_s`, `performance_ramp_percent` | all | 30, 80 | percent 1–100 |
| `reclaim_s` | all | 5 (cpu, gpu_compute), 15 (ram, gpu_memory) | ≥ `hint_deadline_s` for memory |
| `hint_deadline_s` | all | 5 | ≥ 1 |
| `unknown_window_intervals` | all | 3 | ≥ 1 |
| `hysteresis_percent`, `min_step_dwell_intervals` | all | 50, 3 | percent 1–99 |
| `pause_bound_s` | cpu, gpu_compute | 10 | ≥ 1 |
| `noise_tolerance_mib` | gpu_memory | 256 | ≥ 0 |
| `quiet_window_intervals` | gpu_compute | 20 | ≥ 1, and the window is longer than `gpu.process_sample_max_age_ms` |
| `cpu_ram_scale_up_cooldown_ms` | cpu, ram | 30000 | replaces the GPU key for CPU/RAM (FR-034) |
| `principal_min_percent` | all | 1 | 1–50; usage, in percent of capacity, from which a principal contends for a fair share under saturation |

Floors reuse existing keys: `ram.reserve_mib`/`ram.reserve_percent` and
`gpu.reserve_vram_mib`.

## Principal (`src/contention.rs`)

- Fields: `uid: Option<u32>`. `None` stands for the single principal whose owner is unknown.
  `is_owner: bool` marks the CedeGrid account.
- Demand per resource is aggregated from observed processes (R12).

## ResourceView (`src/contention.rs`, built from `Snapshot` + `PolicyInput`)

| Field | Meaning |
|---|---|
| `capacity` | Schedulable capacity after reserves |
| `own` | Lower and upper bound of attributed own usage |
| `external` | Usage not attributed to own work, per principal |
| `unattributed` | Measurement error band (R8, R9) |
| `floor` | No-harm floor |
| `margin` | From the effective mode |
| `fair_share` | Max-min share for the owner principal (Neutral) |
| `measurement` | `known`, `unknown{since}` or `inconsistent_within_tolerance` |
| `activity` | GPU compute only: `quiet`, `active` or `unknown` |
| `admission` | `open`, `held{reason}` or `blocked{reason}` |

## YieldStep (`src/contention.rs`)

- Ordered: `none(0)`, `lower_share(1)`, `cap(2)`, `pause(3)`, `hint(4)`, `drain(5)`,
  `terminate(6)`.
- Per allocation and resource:
  - `step`
  - `entered_at` (monotonic ms)
  - `cause: YieldCause`
  - `unavailable: set<YieldStep>`
  - `target: Option<u64>` (resource units)

### Transitions (pure, in `contention::plan_steps`)

- **Escalate.**
  - Move to the next available step when the contended amount is not freed within the mode's
    response time and `min_step_dwell_intervals` has passed.
  - Memory resources skip steps 1–3 (FR-011).
  - Guaranteed allocations never enter `pause`. They enter `drain` only under floor policy A
    for a floor breach (FR-024, FR-057).
- **Hold.** Unknown measurement or authority loss keeps the current step (FR-008, FR-015).
- **Reverse.**
  - When external demand falls below `hysteresis_percent` of the trigger for the dwell time,
    step down one level (FR-013).
  - `drain` and `terminate` do not reverse; the coordinator retry rules apply.

## YieldCause

- `external_growth`, `floor_breach`, `unknown_measurement`, `envelope_breach`,
  `operator_drain`, `lease_expired` or `coordinator_drain`.
- It travels in the Decision, the supervisor control message, `AllocationReport.yield_cause`
  (when negotiated) and `drain_requests.cause` (FR-018).

## YieldAction (Decision output, `src/model.rs`)

- Fields: `allocation_id`, `resource: ResourceKey`, `step: YieldStep`, `target: Option<u64>`,
  `cause: YieldCause`, `code: ReasonCode`.
- Replaces `would_drain` as the authority's output. `would_drain` is still derived, for
  recordings and compatibility.

## ReasonCode

- A stable `snake_case` code plus one sentence of text. For example: `first_observation_baseline`,
  `vram_noise_below_tolerance`, `inconsistent_within_tolerance`, `allocation_unknown_hold`,
  `floor_breach`.
- Tests assert on codes, never on the text (FR-039).

## ResourceTarget (supervisor ↔ worker, `src/actuation.rs`)

- Fields:
  - `sequence: u64`, monotonic per allocation
  - `deadline_unix_ms`
  - `step`
  - `targets: {cpu_millicores?, ram_mib?, gpu_memory_mib?{uuid: mib}}`
- Written atomically. The format is in
  [contracts/resource-target-file.md](contracts/resource-target-file.md).

## AllocationContention (supervisor journal, `src/execution_state.rs`)

- The current step, target and cause per resource, persisted by the supervisor.
- A restarted agent learns the current state over the control socket instead of guessing
  (R2).

## SetupChoices (`src/setup.rs`)

- Fields: `role`, `default_mode`, per-resource modes, `max_mode`, `floor_policy`,
  `neutral_memory_policy`, tuned thresholds, `acknowledge_danger`.
- `risk(choice) -> none | caution | danger`, with the FR-045 mapping. A combination takes its
  highest marker.

## Wire additions (negotiated, `src/protocol.rs`)

| Field | Purpose |
|---|---|
| `HeartbeatReply.features` | Coordinator advertises its features |
| `NodeReport.features` | Agent advertises its features, after negotiation |
| `NodeReport.contention` | Per-resource admission and mode summary, after negotiation |
| `LaunchRequest.contention` | Per-task mode overrides |
| `AllocationReport.yield_cause` | Why an allocation yielded |

See [contracts/wire-negotiation.md](contracts/wire-negotiation.md).

## Ledger (coordinator distributed schema 4)

- `drain_requests.cause TEXT NOT NULL DEFAULT 'coordinator_drain'`
- `reservations.yield_cause TEXT`
- A CHECK constraint on `reservations.phase`
- Triggers are recreated by the migration
