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
| `acknowledge_caution` | set of `performance_mode`, `small_floor`, `reduced_margin`, `relaxed_detection` | `[]` | Performance on any resource or as `max_mode` without `performance_mode` fails to load; a margin below its default without `reduced_margin`, or a detection setting beyond its default without `relaxed_detection` (`saturation_percent`, `principal_min_percent`, `noise_tolerance_mib`, `uid_min` or `attribution_tolerance_percent` above it, `memory_peak_window_s` or `quiet_window_intervals` below it), fails to load; a floor near its minimum without `small_floor` is raised to twice the minimum with a warning (FR-051, FR-054). |
| `guaranteed_hint_deadline_s` | u64 | 5 | 1–30 (FR-024) |
| `critical_floor_percent` | u8 | 50 | 10–90; the critical level never goes below the minimum floor (FR-063) |
| `critical_hint_deadline_s` | u64 | 1 | 1 to every memory resource's `hint_deadline_s` and to `guaranteed_hint_deadline_s` (FR-063) |
| `uid_min` | u32 | 1000 | FR-059; above 1000 needs `relaxed_detection` |
| `principal_window_intervals` | u32 | 4 | ≥ 1 (FR-059) |
| `principal_leave_s` | u64 | 60 | 1–3600; a principal leaves after this long below the threshold (FR-059) |
| `never_share_occupied_gpus` | bool | false | FR-064 |
| `[contention.<resource>]` | ResourceThresholds | per-resource defaults | Validation below |

- Legacy `gpu.execution_mode` is interpreted as FR-031 lists, for GPU resources without an
  explicit mode, with one warning per load for every value except `auto`. A non-empty
  `gpu.best_effort_external_processes` warns that the list is unused. `doctor` shows the
  interpretation.
- `gpu.reserve_vram_percent` (u8, default 5, 0–100) sets the per-device floor to at least that
  share of the device; `gpu.reserve_vram_mib` defaults to twice the minimum (1024 MiB).
- `cpu.reserve_physical_cores` defaults to 0 and is a minimum under the mode's margin, never
  added to it (FR-067). `Config::floors` reports each floor on the host: configured, minimum,
  effective, near the minimum, confirmed and raised.
- Floors below the minimum (512 MiB host RAM, 512 MiB per GPU device) are raised to it with a
  warning. A floor below twice its minimum is "near" it and, without `small_floor`, applies as
  twice the minimum. The load warns when the settings alone decide this; for a floor a
  percentage sets, the agent warns once when it first samples the host
  (`Config::host_floor_warnings`).
- A negative `cpu.nice` warns and is treated as 0 (R5).

## ResourceThresholds (`src/config.rs`)

These are provisional defaults (spec table). All are configuration values (FR-025).

| Field | Applies to | Default | Validation |
|---|---|---|---|
| `mode` | all | inherits `default_mode` | ≤ `max_mode` |
| `yield_first_margin_percent` | all | 15 (cpu, gpu_compute), 20 (ram, gpu_memory) | 1–90; on cpu and gpu_compute > `performance_margin_percent`; below its default needs `reduced_margin` |
| `neutral_margin_percent` | cpu, gpu_compute | 5 | between `performance_margin_percent` and `yield_first_margin_percent`; below 5 needs `reduced_margin` |
| `performance_margin_percent` | cpu, gpu_compute | 2 | 0–50; below 2 needs `reduced_margin` |
| `yield_first_response_intervals` | cpu, gpu_compute | 3 | 1–10 |
| `scale_up_step_interval_s` | all | unset: 30 s (Yield First), 10 s (Neutral); in Performance one monitor interval on CPU and GPU compute and 2 s on RAM and GPU memory | ≥ 1; when set, applies to every mode |
| `neutral_convergence_s` | all | 60 | ≥ 1; a success-criteria parameter read by the scenario tests and validation tools, not by the policy |
| `neutral_tolerance_points` | all | 15 | 1–50; success-criteria parameter |
| `performance_ramp_s`, `performance_ramp_percent` | all | 10, 90 | percent 1–100; success-criteria parameters |
| `reclaim_s` | cpu, gpu_compute | 5 | 1–60; how long a Neutral or Performance step has to free the contended amount before the next step; memory moves on at the hint deadline |
| `hint_deadline_s` | all | 5 | 1–30 |
| `unknown_window_intervals` | all | 3 | 1–20 |
| `hysteresis_percent`, `min_step_dwell_intervals` | all | 50, 3 | percent 1–99; dwell 1–10 |
| `pause_bound_s` | cpu, gpu_compute | 10 | 1–60 |
| `noise_tolerance_mib` | gpu_memory | 256 | 0–1024, and at agent start at most half of each device's floor; above 256 needs `relaxed_detection` |
| `quiet_window_intervals` | gpu_compute | 20 | ≥ 1, and the window is longer than `gpu.process_sample_max_age_ms`; below 20 needs `relaxed_detection` |
| `attribution_tolerance_percent` | gpu_compute | 15 | 1–30; summed per-process utilization within this many points of device utilization makes the external amount measured (FR-021); above 15 needs `relaxed_detection`; provisional until T088 |
| `scale_up_cooldown_ms` | cpu, ram, gpu_memory | the mode's scale-up interval: 30000 (Yield First), 10000 (Neutral); in Performance the monitor interval (500) on CPU and 2000 on RAM and GPU memory | How long the resource must stay stable before own work is admitted to it, counted from that resource's own stable time (FR-034). On GPU memory a set legacy `gpu.scale_up_cooldown_ms` applies when this key is unset. Not to be confused with `scale_up_step_interval_s`, the ladder's reversal rate |
| `reclaim_backoff_ms`, `reclaim_backoff_max_ms` | ram, gpu_memory | 1000, 30000 | backoff ≥ 1, max ≥ backoff; extra cooldown per repeated reclaim episode (FR-061) |
| `memory_peak_window_s` | ram, gpu_memory | 300 | ≥ 1 (FR-060); below 300 needs `relaxed_detection` |
| `admission_usage_window_s` | cpu | 60 | 1–3600; running opportunistic CPU is charged for admission at its p95 usage over this window, once it has a full window of samples (FR-034) |
| `guaranteed_breach_window_s` | ram | 10 | 0–60 (FR-024) |
| `principal_min_percent` | all | 1 | 1–50; usage, in percent of capacity, from which a principal contends for a fair share under saturation; above 1 needs `relaxed_detection` |
| `saturation_percent` | cpu, gpu_compute | 98 | 50–100; busy at or above this share of capacity is saturated; above 98 needs `relaxed_detection` |
| `psi_some_percent` | cpu | 10 | 1–100; external PSI `some` avg10 above it saturates the CPU (FR-072); above 10 needs `relaxed_detection` |

Floors reuse existing keys: `ram.reserve_mib`/`ram.reserve_percent` and
`gpu.reserve_vram_mib`, plus the new `gpu.reserve_vram_percent`.

## Principal (`src/contention.rs`)

- Fields: `uid: Option<u32>`. `None` stands for the single principal whose owner is unknown.
  `is_owner: bool` marks the CedeGrid account.
- Demand per resource is aggregated from observed processes (R12).
- Classification per resource, kept by the policy engine (FR-059):
  - the owner is always a principal;
  - a user ID below `uid_min` is background;
  - a user ID at or above it is admitted after `principal_window_intervals` at or above
    `principal_min_percent`, and leaves after `principal_leave_s` below it;
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
| `applied`, `applied_targets` | The steps and the cap or hint targets the supervisor reports applied; a RAM cap reports the `memory.high` in effect |
| `ram_cap` | The backend can cap RAM: a delegated cgroup leaf with `memory.high` authorized. Without it the RAM cap step is unavailable (T138) |
| `step_codes` | Catalog codes the supervisor reports for a step held above its target or found unavailable: `ram_cap_growth_stop`, `ram_cap_ineffective`, `ram_cap_refused` |

## ExternalProcessMemory (`src/model.rs`, `Snapshot.external_processes`)

- Fields: `pid`, `start_time` (seconds since the epoch, 0 when unknown), `ram_mib`,
  `gpu_memory_mib{uuid: mib}`. PID and start time identify the process, so a reused PID is
  another process.
- The collector lists every external GPU context and every external process holding at least
  1% of host RAM. A listed process stays listed while it lives, so its peak lasts until it
  exits; at most the 64 largest are kept. The field is optional, so older recordings replay
  with one peak for all external usage.

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
| `contended` | CPU, GPU compute: saturated (for CPU, by utilization or external PSI) with an active external principal whose demand is not known to be satisfied, or unknown activity |
| `satisfied` | CPU, GPU compute: demands probing found satisfied, by principal: usage that did not grow into freed capacity plus the contention floor |
| `probe` | CPU, GPU compute: while a small external demand is probed, the allowance stepping down by the mode's margin per response window (`DemandProbe`) |
| `external_peak` | RAM, GPU memory: over `memory_peak_window_s`, the sum of each live listed external process's peak plus the peak of the external usage no listed process accounts for, for admission headroom |
| `measurement` | `known`, `unknown{since}` or `inconsistent_within_tolerance` |
| `activity` | GPU compute only: `quiet`, `active` or `unknown` |
| `admission` | `open`, `held{reason}` or `blocked{reason}`; `blocked{admission_own_work_yielding}` only while own work on the resource is being reclaimed: above its target, or a step still moving down, by more than the noise tolerance (memory) or the reversal hysteresis of the external growth a compute step answered (FR-028) |

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
  - Memory resources skip `lower_share` and `pause` (FR-011). RAM takes `cap` only where the
    allocation's facts report `ram_cap` (a delegated leaf with `memory.high` authorized), and GPU
    memory never does. `hint` is available only when the allocation is hint-ready.
  - Guaranteed allocations never enter `pause`, and their CPU or RAM cap never goes below their
    request. They enter `drain` only as the last step of floor policy A (FR-024, FR-057).
  - On the critical floor path, the hint deadline is `critical_hint_deadline_s`, and an
    allocation that does not observe the drain file goes to `terminate` instead of `drain`
    (FR-063). A Guaranteed hint waits `guaranteed_hint_deadline_s` off that path.
  - A hint waits for its deadline, never only the response time.
  - A pause ends at `pause_bound_s` even when nothing is contended: at the next step, else as a
    cap at the minimum duty marked `after_pause`, which does not escalate again while the
    contention lasts (FR-014).
- **Relax.** While a resource is not contended, a held cap or hint rises toward the mode's
  target at the mode's scale-up rate, so own work recovers capacity that external demand no
  longer uses.
- **Effective cap.** A cap whose usage follows its target, within 105% of it, holds while the
  target moves down, as in Performance and probing; it escalates only when usage stays above
  the cap past the response time. A cap binds while usage is at least 95% of its target. These
  tolerances and FR-014's 5% minimum duty are fixed mechanism constants (FR-025).
- **Hold.** Unknown measurement or authority loss keeps the current step (FR-008, FR-015).
- **Reverse.**
  - A step taken for external growth: when external demand falls below `hysteresis_percent` of
    the trigger for the dwell time, step down one level (FR-013).
  - A step taken for an envelope breach or an unknown measurement: once its cause has stayed
    clear for the dwell time, whatever the external level.
  - A step taken for a floor breach: once free memory has stayed above the floor by at least
    `StepState.to_free` (what the step had to free when it began), or `noise_tolerance_mib` if
    larger, for the dwell time plus the episode's reclaim backoff (FR-061).
  - `drain` and `terminate` do not reverse; the coordinator retry rules apply.

## YieldCause

- `external_growth`, `floor_breach`, `unknown_measurement`, `envelope_breach`,
  `operator_drain`, `lease_expired` or `coordinator_drain`.
- It travels in the Decision, the supervisor control message, `AllocationReport.yield_cause`
  (when negotiated) and `drain_requests.cause` (FR-018).

## YieldAction (Decision output, `src/model.rs`)

- Fields: `allocation_id`, `resource: ResourceKey`, `step: YieldStep`, `target: Option<u64>`,
  `cause: YieldCause`, `code: ReasonCode`, and for a hint `deadline_ms`, the deadline the ladder
  enforces (FR-037). The agent passes it to the supervisor in the `adjust` step request.
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
  - `allowance?`: the same shape, what the allocation may grow to now (FR-071); policy output
    `Decision.allowances` per allocation
  - `allowance_sequence`: counts allowance changes apart from `sequence`; `sequence` 0 means no
    target was published
- Written atomically. The format is in
  [contracts/resource-target-file.md](contracts/resource-target-file.md).

## Mode targets (`contention::own_allowance`)

`C` is schedulable capacity (after reserves and, for memory, the floor), `A = C − E` with `E`
current external usage, `S` the max-min share, `m_YF`, `m_N` and `m_P` the margins, validated
as `m_YF ≥ m_N ≥ m_P` (FR-065).

| Resource and state | Yield First | Neutral | Performance |
|---|---|---|---|
| CPU, GPU compute, contended | 0 for opportunistic work | `S` | `max(S, A − m_P·C)` |
| CPU, GPU compute, contended, own usage above `C − m_YF·C` | `min(A − m_YF·C, S)` | `S` | `max(S, A − m_P·C)` |
| CPU, GPU compute, contended while probing at `p` | `min(A − m_YF·C, p)` | `max(S, p)` | `max(S, A − m_P·C, p)` |
| CPU, GPU compute, unknown activity | 0 | `S` | `S` |
| CPU, GPU compute, otherwise | `A − m_YF·C` | `A − m_N·C` | `A − m_P·C` |
| RAM, GPU memory | `A − m_YF·C` | `A` | `A` |

`C` is the whole capacity for CPU; the explicit physical-core reserve applies as
`min(target, C − reserve − E)`, so the idle reserve is the larger of the reserve and the margin
(FR-067). While own work holds more than `C − m_YF·C`, saturation may be its own doing, so
Yield First caps to its margin first; activity of unknown amount still takes it to 0. `S`
counts a satisfied principal at its satisfied demand. Targets never go below 0. Guaranteed CPU
is never capped below its request. Neutral and Performance differ in idle margin (5% and 2%),
in their contended target and in scale-up rate (10 s per step, and one monitor interval per step on CPU and GPU compute).

## DemandProbe (`src/contention.rs`, kept by `PolicyEngine` per compute resource)

- `target`: the allowance while probing; it starts one margin step below own usage and steps
  down once per response window while a probed principal grows into the freed capacity.
- `stepped_at_ms`, `usage_at_step`: when it last stepped and the probed principals' usage then.
- `watch`, `baselined`: a watch of a larger demand, and whether it has taken its baseline.
- A probe starts only under saturation, for contending principals whose unknown demand is below
  one margin step, and under PSI-only saturation for every contending principal (FR-072). At the end of a window, a principal whose usage did not grow beyond
  `usage_at_step + floor` is satisfied at its usage plus the floor.
- Outside PSI-only saturation, a demand of a margin step or more gets the fair share at once
  and starts a watch, which sets
  no target. Its baseline is the external principals at or above the floor one interval later,
  taken again whenever that set changes. At the end of one response window, if the host is not
  saturated, each whose usage stayed within `usage_at_step + floor` is satisfied at its usage
  plus the floor.
- A record holds while the principal's usage stays within it, also while it is not listed. A
  user with a record rejoins as a principal at once, without the principal window. At most 64
  records are kept per resource key; past that, those whose principals were seen longest ago
  are dropped first (`PolicyEngine` keeps when each recorded principal was last seen).

## CpuPressure (`src/kernel.rs`, `KernelSnapshot.cpu_pressure`)

Read-only CPU PSI for FR-072, collected with the kernel snapshot:

| Field | Meaning |
|---|---|
| `status` | `available` (external cgroups read), `system_only` (only `/proc/pressure/cpu`), or `unknown` (missing, unreadable or malformed) |
| `external_some_avg10` | The highest `some` avg10 among the cgroups outside own work's user slice |
| `external_cgroups` | How many external cgroups were read |
| `own_some_avg10` | `some` avg10 of own work's cgroup, shown only |
| `system_some_avg10` | `some` avg10 of `/proc/pressure/cpu`, shown only |
| `detail` | Why the status is not `available` |

## HeldBack (`src/model.rs`, `ResourceStatus.held_back`, node report)

Per resource, in resource units, zero amounts left out: `floor`, `reserve` (the CPU reserve
where it exceeds the margin), `margin`, `ceiling` (CPU a visible ancestor CPU ceiling keeps
from the budget beyond the reserve; `doctor` shows it on its own line, never as contention),
`contention` (left to contending external demand),
`peak`, `contention_psi` (the contention hold when only external PSI saturates the CPU),
`noise_tolerance` (external GPU memory growth within this much goes unseen; reported
for information, not subtracted from the budget), `unknown`, `cooldown`, `backoff`,
`charge_shortfall` (admission capacity held by charges above usage) (FR-069). GPU compute also
reports `compute_path`: `measured`, or `unknown` with its reason (FR-021). Each resource also
reports its `admission`: open, held or blocked, with a reason (FR-028, FR-038).

## GpuSnapshot compute amount (`src/model.rs`)

`external_compute_milli` and `own_compute_milli`, in thousandths of device time, when fresh
per-process samples account for device utilization; else `compute_amount_unknown` with the
reason. While an external PID is listed, external is max(external PIDs' sum, device
utilization minus own PIDs' sum); with own PIDs only, external is 0 and own is the device
utilization. The policy takes the measured path only after `quiet_window_intervals`
consecutive measured intervals (`PreviousGpu.measured_streak`), and the unknown path at once. Older recordings have none of them and take the unknown path. `PrincipalUsage`
carries `gpu_compute_milli` per device, the measured compute of that UID's external
processes; a PID whose UID cannot be read stays in the unattributed remainder.

## Judged amounts (`src/policy.rs`)

- Yielding judges running work on what it holds: CPU and RAM usage; GPU memory usage when the
  allocation is hint-ready, else max(request, observed) (FR-066).
- Admission charges: running opportunistic CPU at its p95 usage over
  `admission_usage_window_s`, once it has a full window of samples; everything else at
  max(request, observed); pending reservations in full (FR-034).
- A floor breach is observed free memory below the floor; a charge beyond the budget only blocks
  admission.

## AllocationContention (supervisor journal, `src/execution_state.rs`)

- The current step, target and cause per resource, persisted by the supervisor.
- A restarted agent learns the current state over the control socket instead of guessing
  (R2).

## SetupChoices (`src/setup.rs`)

- Fields: `role`, `default_mode`, per-resource modes, `max_mode`, floor sizes, margins,
  `guaranteed_hint_deadline_s`, tuned thresholds, `acknowledge_caution`.
- `risk(choice) -> none | caution`, with the FR-045 mapping: Performance on any resource, a
  floor near its minimum, a margin below its default and a detection setting beyond its default
  are caution. Setup lists, per acknowledged caution option, the keys and values it covers
  (`Config::caution_details`).

## Key classes (`src/config.rs`)

`KEY_CLASSES` classifies every configuration key as `safe`, `bounded` or `caution(entry)`;
`contention.*` stands for each resource section. A test enumerates every key from the
configuration types and fails for one without a class (FR-068).

## Wire additions (negotiated, `src/protocol.rs`)

| Field | Purpose |
|---|---|
| `HeartbeatReply.features` | Coordinator advertises its features |
| `NodeReport.features` | Agent advertises its features, after negotiation |
| `NodeReport.contention` | Per-resource admission and mode summary, after negotiation |
| `LaunchRequest.contention` | Per-task mode overrides |
| `AllocationReport.yield_cause` | Why an allocation yielded |
| `AllocationReport.hint_ready`, `AllocationReport.observes_drain` | Allocation readiness (FR-062) |
| `ResourceContentionReport.held_back` | What the resource holds back and why (FR-069) |

See [contracts/wire-negotiation.md](contracts/wire-negotiation.md).

## Ledger (coordinator distributed schema 4)

- `drain_requests.cause TEXT NOT NULL DEFAULT 'coordinator_drain'`
- `reservations.yield_cause TEXT`
- A CHECK constraint on `reservations.phase`
- Triggers are recreated by the migration
