# Contract: `[contention]` configuration (node TOML, `config_version = 1`)

All keys are optional and additive. Unknown keys are rejected (`deny_unknown_fields`).
Defaults are provisional, to validate on real hardware.

```toml
[contention]
default_mode = "neutral"            # "yield_first" | "neutral" | "performance"
max_mode = "neutral"                # node cap; jobs run at min(requested, max_mode); unset, the most aggressive node mode
floor_policy = "a"                  # the only value; "b" and "c" fail to load
neutral_memory_policy = "a"         # the only value; "b" fails to load
acknowledge_danger = []             # "performance_mode", "small_floor" [caution]
guaranteed_hint_deadline_s = 5      # 1-30
critical_floor_percent = 50         # a breach is critical below this share of the floor
critical_hint_deadline_s = 1        # hint deadline on the critical path
uid_min = 1000                      # lower user IDs are background demand
principal_window_intervals = 4      # sustained intervals to join or leave as a principal
never_share_occupied_gpus = false   # true: no new own GPU work on devices with external contexts

[contention.cpu]                    # also [contention.ram], [contention.gpu_compute], [contention.gpu_memory]
mode = "neutral"                    # optional override of default_mode, must be <= max_mode
yield_first_margin_percent = 25
yield_first_response_intervals = 3
performance_margin_percent = 10
neutral_convergence_s = 60
neutral_tolerance_points = 15
performance_ramp_s = 30
performance_ramp_percent = 80
reclaim_s = 5                       # 15 for ram and gpu_memory: Neutral and Performance escalation time
hint_deadline_s = 5
unknown_window_intervals = 3
hysteresis_percent = 50
min_step_dwell_intervals = 3
pause_bound_s = 10
scale_up_cooldown_ms = 30000        # cpu and ram only; unset, the mode's scale-up interval
principal_min_percent = 1           # usage from which a principal contends for a fair share

[contention.ram]                    # also [contention.gpu_memory], except the breach window
memory_peak_window_s = 300          # admission headroom uses the external peak over this window
reclaim_backoff_ms = 1000           # extra cooldown after a repeated reclaim episode, doubling
reclaim_backoff_max_ms = 30000
guaranteed_breach_window_s = 10     # ram only, 0-60: breach duration before Guaranteed drains

[contention.gpu_memory]
noise_tolerance_mib = 256

[contention.gpu_compute]
quiet_window_intervals = 20
```

## Scale-up cooldowns

- `contention.cpu.scale_up_cooldown_ms` sets how long CPU must stay stable before own work grows
  on it, at startup and after it was unsafe. `contention.ram.scale_up_cooldown_ms` does the same
  for RAM. Each resource waits only for its own cooldown.
- Unset, each takes its resource's mode scale-up interval: 30000 in Yield First, 10000 in Neutral
  and 2000 in Performance, or `scale_up_step_interval_s` × 1000 when that is set.
- GPU memory keeps `gpu.scale_up_cooldown_ms`, which no longer affects CPU or RAM (FR-034).
- RAM and GPU memory add a reclaim backoff: after a reclaim episode on the resource, each further
  episode within `memory_peak_window_s` doubles an extra delay, from `reclaim_backoff_ms` up to
  `reclaim_backoff_max_ms` (FR-061).

## Load-time rules

- **Missing `[contention]`.** Every resource runs in Neutral, with floor policy A.
- **Legacy GPU keys.** `gpu.execution_mode` is interpreted for GPU resources without an explicit
  mode (FR-031), and `doctor` shows the interpretation:

  | Value | Interpretation | Warning |
  |---|---|---|
  | absent, `auto` | Neutral | none |
  | `contention_aware` | Neutral | `deprecated: gpu.execution_mode = "contention_aware" is read as Neutral; set contention.gpu_compute.mode and contention.gpu_memory.mode` |
  | `conservative_non_sharing` | `never_share_occupied_gpus = true` | `deprecated: gpu.execution_mode = "conservative_non_sharing" is read as contention.never_share_occupied_gpus = true; set that key` |
  | `best_effort_occupied` | Neutral | `deprecated: gpu.execution_mode = "best_effort_occupied" is read as Neutral; gpu.best_effort_external_processes is no longer used` |

  A non-empty `gpu.best_effort_external_processes` alone warns that the list is no longer used.
- **Removed options.** `floor_policy = "b"` or `"c"` fails with
  `contention.floor_policy = "<v>" was removed: Guaranteed memory is released last under policy A; tune contention.guaranteed_hint_deadline_s and contention.ram.guaranteed_breach_window_s`.
  `neutral_memory_policy = "b"` fails with
  `contention.neutral_memory_policy = "b" was removed: floors always come before the fair share; size them with ram.reserve_mib, ram.reserve_percent and gpu.reserve_vram_mib`.
- **Performance without `acknowledge_danger` containing `performance_mode`.** Loading fails with
  `ERR_CEDEGRID_DANGER_UNACKNOWLEDGED`.
- **Floors.** A floor below its minimum (512 MiB of RAM, 256 MiB per GPU) is raised to it with a
  warning. A floor below twice its minimum without `small_floor` loads with a warning.
- **An active caution option.** `doctor` and `status` show it with its marker (FR-043).
- **Negative `cpu.nice`.** Warns, and is treated as 0.
- **Modes.** Any per-resource `mode` above `max_mode` fails validation.

## Per-job override (job spec JSON, `LaunchRequest.contention`)

```json
{"contention": {"modes": {"cpu": "performance", "gpu_memory": "neutral"}}}
```

- The effective mode is `min(requested, node max_mode)` and is reported per allocation.
- Overrides are placed only on agents that advertise `contention/1`
  ([wire-negotiation.md](wire-negotiation.md)).
