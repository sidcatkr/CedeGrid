# Contract: `[contention]` configuration (node TOML, `config_version = 1`)

All keys are optional and additive. Unknown keys are rejected (`deny_unknown_fields`).
Defaults are provisional, to validate on real hardware.

```toml
[contention]
default_mode = "performance"        # "yield_first" | "neutral" | "performance" (default since item 029)
max_mode = "neutral"                # node cap; jobs run at min(requested, max_mode); unset, the most aggressive node mode
floor_policy = "a"                  # the only value; "b" and "c" fail to load
neutral_memory_policy = "a"         # the only value; "b" fails to load
acknowledge_caution = []             # "small_floor", "reduced_margin", "relaxed_detection" [caution]; "performance_mode" still loads
guaranteed_hint_deadline_s = 5      # 1-30
critical_floor_percent = 50         # a breach is critical below this share of the floor
critical_hint_deadline_s = 1        # hint deadline on the critical path
uid_min = 1000                      # lower user IDs are background demand; above 1000 needs "relaxed_detection"
principal_window_intervals = 4      # sustained intervals to join as a principal
principal_leave_s = 60              # below the threshold this long, a principal leaves
never_share_occupied_gpus = false   # true: no new own GPU work on devices with external contexts

[contention.cpu]                    # also [contention.ram], [contention.gpu_compute], [contention.gpu_memory];
                                    # keys marked cpu, ram or memory apply only there, others are rejected
mode = "neutral"                    # optional override of default_mode, must be <= max_mode
yield_first_margin_percent = 15     # 20 on ram and gpu_memory; below the default needs "reduced_margin", as for every margin
yield_first_response_intervals = 3  # cpu and gpu_compute only, 1-10
neutral_margin_percent = 5          # cpu and gpu_compute only; YF >= neutral >= performance
performance_margin_percent = 2      # cpu and gpu_compute only
neutral_convergence_s = 60
neutral_tolerance_points = 15
performance_ramp_s = 10
performance_ramp_percent = 90
reclaim_s = 5                       # cpu and gpu_compute only, 1-60: Neutral and Performance escalation time
hint_deadline_s = 5                 # 1-30
unknown_window_intervals = 3        # 1-20
saturation_percent = 98             # cpu and gpu_compute only; above the default needs "relaxed_detection"
psi_some_percent = 10               # cpu only, 1-100: external PSI some avg10 above it saturates the CPU; above 10 needs "relaxed_detection"
hysteresis_percent = 50
min_step_dwell_intervals = 3        # 1-10
pause_bound_s = 10                  # cpu and gpu_compute only, 1-60
scale_up_step_interval_s = 10       # ladder reversal rate; unset: 30 s, 10 s, or in Performance one interval (compute) or 2 s (memory)
scale_up_cooldown_ms = 10000        # cpu, ram and gpu_memory; admission wait; unset, the mode's scale-up interval
principal_min_percent = 1           # usage from which a principal contends; above 1 needs "relaxed_detection"
admission_usage_window_s = 60       # cpu only, 1-3600: running opportunistic CPU is admitted at its p95 usage over it

[contention.ram]                    # also [contention.gpu_memory], except the breach window
memory_peak_window_s = 300          # admission headroom uses the external peak over this window; shorter needs "relaxed_detection"
reclaim_backoff_ms = 1000           # extra cooldown after a repeated reclaim episode, doubling
reclaim_backoff_max_ms = 30000
guaranteed_breach_window_s = 10     # ram only, 0-60: breach duration before Guaranteed drains

[contention.gpu_memory]
noise_tolerance_mib = 256           # 0-1024 and at most half of each GPU floor; above the default needs "relaxed_detection"

[contention.gpu_compute]
quiet_window_intervals = 20         # shorter needs "relaxed_detection"
attribution_tolerance_percent = 15  # 1-30: per-process sum within this of device utilization is measured; above 15 needs "relaxed_detection"
slice_bound_percent = 75            # 50-100: on a time-sliced device a principal reading at least this share of its equal slices is slice-bound; above 75 needs "relaxed_detection"
slice_step_percent = 5              # 1-50: loads and has no effect; the time-slice duty is gone (FR-089)
```

Related node keys: `cpu.reserve_physical_cores` (default 0) is a minimum idle reserve under the
mode's margin, never added to it; `gpu.reserve_vram_mib` defaults to twice the minimum floor.
Every key has a class, `safe`, `bounded` or `caution(entry)`, in `config::KEY_CLASSES`
(FR-068).

## Scale-up cooldowns

- `contention.cpu.scale_up_cooldown_ms` sets how long CPU must stay stable before own work grows
  on it, at startup and after it was unsafe. `contention.ram.scale_up_cooldown_ms` does the same
  for RAM. Each resource waits only for its own cooldown.
- Unset, each takes its resource's mode scale-up interval: 30000 in Yield First, 10000 in Neutral,
  and in Performance the monitor interval on CPU and 2000 on RAM, or `scale_up_step_interval_s` ×
  1000 when that is set.
- GPU memory uses `contention.gpu_memory.scale_up_cooldown_ms`, else a set legacy
  `gpu.scale_up_cooldown_ms`, else the mode's scale-up interval; neither affects CPU or RAM
  (FR-034).
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
  `contention.neutral_memory_policy = "b" was removed: floors always come before the fair share; size them with ram.reserve_mib, ram.reserve_percent, gpu.reserve_vram_mib and gpu.reserve_vram_percent`.
- **A margin below its default, or a detection setting beyond it.** Without `reduced_margin`
  or `relaxed_detection` respectively, loading fails with `ERR_CEDEGRID_CONFIG`, whose message
  carries `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` and names the settings and the entry. Moving a setting toward leaving more to external demand needs nothing.
- **Noise tolerance.** Above 1024 MiB, loading fails. When the agent starts, a tolerance above
  half of any GPU's effective floor stops it with an error naming the device and its floor.
- **Bounds.** `monitor.interval_ms` and `kernel.monitor_interval_ms` above 10000,
  `kernel.freshness_intervals` above 20, `gpu.process_sample_max_age_ms` above 60000, and
  `lifecycle.drain_timeout_ms` or `lifecycle.term_grace_ms` above 60000 fail to load.
- **The GPU sample age.** Unset, `gpu.process_sample_max_age_ms` is max(2000, 2 ×
  `monitor.interval_ms`); `doctor` and `validate` show the value with `derived`. Set at or
  below `monitor.interval_ms`, loading fails on every host with
  `gpu.process_sample_max_age_ms (<value>) must be above monitor.interval_ms (<interval>); set it to at least <interval + 1> or leave it unset`.
- **Floors.** `gpu.reserve_vram_percent` (default 5) makes each GPU floor at least that share of
  the device. A floor below its minimum (512 MiB of RAM, 512 MiB per GPU) is raised to it with a
  warning. A floor below twice its minimum without `small_floor` loads, but is raised to twice
  the minimum with a warning: at load when the settings alone decide it, or once when the agent
  first samples the host for a floor a percentage sets.
- **Performance.** The default since item 029 and no caution option: it needs no entry, and an
  existing `performance_mode` entry still loads. A file without `default_mode` gets one
  `default_mode_changed` notice in `doctor`.
- **`ram.prefer_own_oom_victim`** (default false): every own task at `oom_score_adj` 1000 from
  launch instead of OOM order by fault (FR-023); setup marks it as a risk to own work.
- **An active caution option.** `doctor` and `status` show it with its marker; `doctor` also
  lists the keys and values it covers (FR-043, FR-068).
- **Negative `cpu.nice`.** Warns, and is treated as 0.
- **Modes.** Any per-resource `mode` above `max_mode` fails validation.

## Per-job override (job spec JSON, `LaunchRequest.contention`)

```json
{"contention": {"modes": {"cpu": "performance", "gpu_memory": "neutral"}}}
```

- The effective mode is `min(requested, node max_mode)` and is reported per allocation. A
  request above the cap adds the reason code `mode_capped`, and the decision and node report
  carry the cap as `max_mode`.
- Overrides are placed only on agents that advertise `contention/1`
  ([wire-negotiation.md](wire-negotiation.md)).
- `modes` needs at least one resource (`cpu`, `ram`, `gpu_compute`, `gpu_memory`). The SDKs set
  it with `command_task(..., contention_modes=...)` (Python) and
  `commandTask(..., { contentionModes })` (TypeScript), and send `contention` only when it is
  set. The cases in `tests/contracts/cedegrid-0.2.json` (`job_contention_cases`) are checked by
  Rust and both SDKs.
