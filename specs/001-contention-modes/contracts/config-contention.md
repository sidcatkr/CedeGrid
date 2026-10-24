# Contract: `[contention]` configuration (node TOML, `config_version = 1`)

All keys are optional and additive. Unknown keys are rejected (`deny_unknown_fields`).
Defaults are provisional, to validate on real hardware.

```toml
[contention]
default_mode = "neutral"            # "yield_first" | "neutral" | "performance"
max_mode = "performance"            # node cap; jobs run at min(requested, max_mode)
floor_policy = "a"                  # "a" (default) | "b" [caution] | "c" [danger]
neutral_memory_policy = "a"         # "a" (default) | "b" [caution]
acknowledge_danger = []             # must include "floor_policy_c" when floor_policy = "c"

[contention.cpu]                    # also [contention.ram], [contention.gpu_compute], [contention.gpu_memory]
mode = "neutral"                    # optional override of default_mode, must be <= max_mode
yield_first_margin_percent = 25
yield_first_response_intervals = 3
performance_margin_percent = 10
neutral_convergence_s = 60
neutral_tolerance_points = 15
performance_ramp_s = 30
performance_ramp_percent = 80
reclaim_s = 5                       # 15 for ram and gpu_memory
hint_deadline_s = 5
unknown_window_intervals = 3
hysteresis_percent = 50
min_step_dwell_intervals = 3
pause_bound_s = 10
cpu_ram_scale_up_cooldown_ms = 30000  # cpu and ram only; unset, the mode's scale-up interval
principal_min_percent = 1           # usage from which a principal contends for a fair share

[contention.gpu_memory]
noise_tolerance_mib = 256

[contention.gpu_compute]
quiet_window_intervals = 20
```

## CPU/RAM scale-up cooldown

`contention.cpu.cpu_ram_scale_up_cooldown_ms` and `contention.ram.cpu_ram_scale_up_cooldown_ms`
set how long CPU and RAM must stay stable before own work grows on them, at startup and after
either was unsafe. The longer of the two applies.

- Unset, each takes its resource's mode scale-up interval: 30000 in Yield First, 10000 in Neutral
  and 2000 in Performance, or `scale_up_step_interval_s` × 1000 when that is set.
- `gpu.scale_up_cooldown_ms` no longer affects CPU or RAM (FR-034). Set this key to keep a 0.2
  timing.

## Load-time rules

- **Missing `[contention]`.** Every resource runs in Neutral, with policies A.
- **Legacy GPU keys.** `gpu.execution_mode` or `gpu.best_effort_external_processes` is
  accepted, has no effect, and produces one warning:
  `deprecated: gpu.execution_mode has no effect; set contention.gpu_compute.mode and contention.gpu_memory.mode`.
- **`floor_policy = "c"` without `acknowledge_danger` containing `floor_policy_c`.** Loading
  fails with `ERR_CEDEGRID_DANGER_UNACKNOWLEDGED`.
- **An active exception (B or C).** Loading succeeds with a warning that names the exception
  and its marker. `doctor` and `status` show it (FR-043).
- **Negative `cpu.nice`.** Warns, and is treated as 0.
- **Modes.** Any per-resource `mode` above `max_mode` fails validation.

## Per-job override (job spec JSON, `LaunchRequest.contention`)

```json
{"contention": {"modes": {"cpu": "performance", "gpu_memory": "neutral"}}}
```

- The effective mode is `min(requested, node max_mode)` and is reported per allocation.
- Overrides are placed only on agents that advertise `contention/1`
  ([wire-negotiation.md](wire-negotiation.md)).
