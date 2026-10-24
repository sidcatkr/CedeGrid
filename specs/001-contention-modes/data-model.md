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
| `max_mode` | ContentionMode | the most aggressive node mode | `default_mode ≤ max_mode`; `performance` needs `performance_mode` (FR-054) |
| `floor_policy` | `a` | `a` | `b` and `c` fail to load, naming the Guaranteed hint deadline and the RAM breach window (FR-041) |
| `neutral_memory_policy` | `a` | `a` | `b` fails to load, naming the floor sizes (FR-042) |
| `acknowledge_danger` | set of `performance_mode`, `small_floor` | `[]` | Performance on any resource or as `max_mode` without `performance_mode` fails to load; a floor near its minimum without `small_floor` is raised to twice the minimum with a warning (FR-051, FR-054). The key keeps its name, as the maintainer asked; it now confirms caution options |
| `guaranteed_hint_deadline_s` | u64 | 5 | 1–30 (FR-024) |
| `critical_floor_percent` | u8 | 50 | 10–90; the critical level never goes below the minimum floor (FR-063) |
| `critical_hint_deadline_s` | u64 | 1 | 1 to every memory resource's `hint_deadline_s` and to `guaranteed_hint_deadline_s` (FR-063) |
| `uid_min` | u32 | 1000 | FR-059 |
| `principal_window_intervals` | u32 | 4 | ≥ 1 (FR-059) |
| `never_share_occupied_gpus` | bool | false | FR-064 |
| `[contention.<resource>]` | ResourceThresholds | per-resource defaults | Validation below |

- Legacy `gpu.execution_mode` is interpreted as FR-031 lists, for GPU resources without an
  explicit mode, with one warning per load for every value except `auto`. A non-empty
  `gpu.best_effort_external_processes` warns that the list is unused. `doctor` shows the
  interpretation.
- Floors below the minimum (512 MiB host RAM, 256 MiB per GPU device) are raised to it with a
  warning. A floor below twice its minimum is "near" it and, without `small_floor`, applies as
  twice the minimum.
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
| `reclaim_s` | all | 5 (cpu, gpu_compute), 15 (ram, gpu_memory) | ≥ 1; how long a Neutral or Performance step has to free the contended amount before the next step |
| `hint_deadline_s` | all | 5 | 1–30 |
| `unknown_window_intervals` | all | 3 | 1–20 |
| `hysteresis_percent`, `min_step_dwell_intervals` | all | 50, 3 | percent 1–99 |
| `pause_bound_s` | cpu, gpu_compute | 10 | ≥ 1 |
| `noise_tolerance_mib` | gpu_memory | 256 | ≥ 0 |
| `quiet_window_intervals` | gpu_compute | 20 | ≥ 1, and the window is longer than `gpu.process_sample_max_age_ms` |
| `scale_up_cooldown_ms` | cpu, ram | the mode's scale-up interval: 30000 (Yield First), 10000 (Neutral), 2000 (Performance) | How long the resource must stay stable before own work is admitted to it, counted from that resource's own stable time (FR-034). GPU memory keeps `gpu.scale_up_cooldown_ms`. Not to be confused with `scale_up_step_interval_s`, the ladder's reversal rate |
| `reclaim_backoff_ms`, `reclaim_backoff_max_ms` | ram, gpu_memory | 1000, 30000 | backoff ≥ 1, max ≥ backoff; extra cooldown per repeated reclaim episode (FR-061) |
| `memory_peak_window_s` | ram, gpu_memory | 300 | ≥ 1 (FR-060) |
| `guaranteed_breach_window_s` | ram | 10 | 0–60 (FR-024) |
| `principal_min_percent` | all | 1 | 1–50; usage, in percent of capacity, from which a principal contends for a fair share under saturation |
| `saturation_percent` | cpu, gpu_compute | 98 | 50–100; busy at or above this share of capacity is saturated |

Floors reuse existing keys: `ram.reserve_mib`/`ram.reserve_percent` and
`gpu.reserve_vram_mib`.

## Principal (`src/contention.rs`)

- Fields: `uid: Option<u32>`. `None` stands for the single principal whose owner is unknown.
  `is_owner: bool` marks the CedeGrid account.
- Demand per resource is aggregated from observed processes (R12).
- Classification per resource, kept by the policy engine (FR-059):
  - the owner is always a principal;
  - a user ID below `uid_min` is background;
  - a user ID at or above it is admitted after `principal_window_intervals` at or above
    `principal_min_percent`, and leaves after the same window below half of it;
  - usage of user IDs not admitted yet, and of unreadable owners, forms the unattributed
    principal.

## AllocationFacts (`src/model.rs`, agent → policy)

| Field | Meaning |
|---|---|
| `uncontrollable` | Nobody can act on the allocation |
| `progress_lost_ms` | Time since the last accepted checkpoint, else since start |
| `unavailable` | Steps the backend or supervisor cannot apply, per resource |
| `hint_ready` | The worker acknowledged its target file (FR-062) |
| `observes_drain` | The worker acknowledged its target file or used the supervisor socket, so it polls the drain file (FR-062) |

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
| `contended` | CPU, GPU compute: saturated with an active external principal, or unknown activity |
| `external_peak` | RAM, GPU memory: peak external usage over `memory_peak_window_s`, for admission headroom |
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
  - The first step applies at once. Move to the next available step when the contended amount is
    not freed within the mode's response time and `min_step_dwell_intervals` has passed; on the
    critical floor path the dwell is at most the critical hint deadline.
  - A cap whose target is zero for external demand is a pause instead, at once.
  - Memory resources skip steps 1–3 (FR-011). `hint` is available only when the allocation is
    hint-ready.
  - Guaranteed allocations never enter `pause`, and their CPU cap never goes below their
    request. They enter `drain` only as the last step of floor policy A (FR-024, FR-057).
  - On the critical floor path, the hint deadline is `critical_hint_deadline_s`, and an
    allocation that does not observe the drain file goes to `terminate` instead of `drain`
    (FR-063).
- **Relax.** While a resource is not contended, a held cap rises toward the mode's target at the
  mode's scale-up rate, so own work recovers capacity that external demand no longer uses.
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

## Mode targets (`contention::own_allowance`)

`C` is schedulable capacity (after reserves and, for memory, the floor), `A = C − E` with `E`
current external usage, `S` the max-min share, `m_YF` and `m_P` the margins (FR-065).

| Resource and state | Yield First | Neutral | Performance |
|---|---|---|---|
| CPU, GPU compute, contended | 0 for opportunistic work | `S` | `S` |
| CPU, GPU compute, otherwise | `A − m_YF·C` | `A − m_P·C` | `A − m_P·C` |
| RAM, GPU memory | `A − m_YF·C` | `A` | `A` |

Targets never go below 0. Guaranteed CPU is never capped below its request. Neutral and
Performance differ in scale-up rate (10 s and 2 s per step), not in target.

## AllocationContention (supervisor journal, `src/execution_state.rs`)

- The current step, target and cause per resource, persisted by the supervisor.
- A restarted agent learns the current state over the control socket instead of guessing
  (R2).

## SetupChoices (`src/setup.rs`)

- Fields: `role`, `default_mode`, per-resource modes, `max_mode`, floor sizes, margins,
  `guaranteed_hint_deadline_s`, tuned thresholds, `acknowledge_danger`.
- `risk(choice) -> none | caution`, with the FR-045 mapping: Performance on any resource and a
  floor near its minimum are caution.

## Wire additions (negotiated, `src/protocol.rs`)

| Field | Purpose |
|---|---|
| `HeartbeatReply.features` | Coordinator advertises its features |
| `NodeReport.features` | Agent advertises its features, after negotiation |
| `NodeReport.contention` | Per-resource admission and mode summary, after negotiation |
| `LaunchRequest.contention` | Per-task mode overrides |
| `AllocationReport.yield_cause` | Why an allocation yielded |
| `AllocationReport.hint_ready`, `AllocationReport.observes_drain` | Allocation readiness (FR-062) |

See [contracts/wire-negotiation.md](contracts/wire-negotiation.md).

## Ledger (coordinator distributed schema 4)

- `drain_requests.cause TEXT NOT NULL DEFAULT 'coordinator_drain'`
- `reservations.yield_cause TEXT`
- A CHECK constraint on `reservations.phase`
- Triggers are recreated by the migration
