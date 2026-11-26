# Contract: `cedegrid setup`

## Invocation

```text
cedegrid [--config PATH] setup [--non-interactive] [--json]
    [--role node|coordinator|client]
    [--default-mode yield_first|neutral|performance]
    [--mode RESOURCE=MODE]...            # RESOURCE: cpu|ram|gpu_compute|gpu_memory; MODE `default` follows the default mode
    [--max-mode MODE]                    # `default` removes the explicit cap
    [--ram-floor-mib N] [--gpu-floor-mib N]
    [--margin RESOURCE.MODE=PERCENT]...  # MODE: yield_first|neutral|performance; YF >= N >= P
    [--guaranteed-hint-deadline-s N]
    [--set RESOURCE.KEY=VALUE]...        # tuned thresholds, e.g. gpu_memory.noise_tolerance_mib=512
    [--acknowledge-caution small_floor|reduced_margin|relaxed_detection]...  # performance_mode is accepted and needs no use
    [--endpoint URL]                     # client
    [--listen ADDRESS] [--state-dir PATH] [--operator-fingerprint HEX]   # coordinator
    [--ca-cert PATH] [--certificate PATH] [--private-key PATH]           # client and coordinator
```

## Roles

`--role` picks which configuration file setup writes. An existing file keeps its role: a file
with `endpoint` is a client configuration, one with `listen` a coordinator configuration, and
any other a node configuration. A new file needs `--role`.

| Role | Setup manages | A new file needs |
|---|---|---|
| node | The node choices below | nothing more; it starts as `config_version = 1` |
| client | `endpoint`, `[tls]` | `--endpoint`, `--ca-cert`, `--certificate`, `--private-key` |
| coordinator | `listen`, `state_dir`, `[tls]`, one operator entry in `[clients]` | `--listen`, `--state-dir`, `--operator-fingerprint`, the three TLS flags |

Node flags with another role, or connection flags with a node role, are refused. On Windows
setup configures client settings only; node and coordinator setup is refused.

## Modes

| Situation | Behavior |
|---|---|
| A TTY on stdin and stdout, no `--non-interactive` | The terminal interface opens |
| Anything else | No prompt and no blocking. Values come from flags and the existing config |
| A command that needs a missing config, on a TTY | Offers to start setup. Never on a non-TTY |

## Terminal interface

| Key | Action |
|---|---|
| ↑ / ↓ | Move the selection |
| ← / → | Change the selected value in place (003 FR-031) |
| Enter | On "Review and write", review; on the review, write; on a confirmation, apply the choice |
| Esc, `q`, Ctrl-C | Cancel; nothing is written and the terminal is restored. On a confirmation or the review, Esc goes back instead |

- **Rows.** Role, default mode, the mode of each resource, the node cap, the RAM and GPU memory
  floors, the three margins of CPU and GPU compute and the Yield First margin of RAM and GPU
  memory (memory keeps only the floor under Neutral and Performance, so
  `--margin ram.neutral=…` and the like exit 2), the Guaranteed hint deadline, the GPU memory
  noise tolerance and the GPU compute attribution tolerance, then "Review and write" and
  "Cancel", grouped under Modes, Safety floors and Advanced below a header with this host's CPU,
  RAM and GPU memory (003 FR-030). Up from the first row wraps to the last. Left and Right step
  the selected row through its options in place. A side pane explains the selected row with
  this host's numbers: what it is, its value, its consequence and, for a caution value, why.
  Each row starts at the current value; on a new file that is Neutral everywhere.
- **Options.** Modes, plus "same as default" per resource and "no explicit cap" for the node cap.
  The cap offers only modes at least as aggressive as the most aggressive configured mode, since
  a lower cap does not load. Floors, margins, the deadline and the tolerances offer fixed
  values around their defaults and the current value. Every option maps to a key
  `Config::load` accepts.
- **Consequence lines** use this host's numbers: CPU millicores kept free, the effective RAM
  floor, and the effective floor of the first GPU.
- **Side pane.** It shows the selected value's one-line consequence. The default value has no
  color and no label.
- **Markers.**

  | Marker | Appearance | Options |
  |---|---|---|
  | caution | `[caution]`, yellow (palette color 3, written as SGR 33 or 38;5;3) | a floor below twice its minimum; a margin below its default; `saturation_percent`, `principal_min_percent`, `noise_tolerance_mib`, `uid_min` or `attribution_tolerance_percent` above its default, a range in `system_accounts.uid_ranges` beyond the default ones, `memory_peak_window_s` or `quiet_window_intervals` below it |

- **No color** when `NO_COLOR` is set to a non-empty value, when output is not a TTY, or when
  `TERM=dumb`. The labels always remain. Without color the interface writes no escape sequence
  at all: each screen is printed as plain text after a blank line.
- **Caution confirmation.** Choosing a caution option opens a separate screen that restates the
  consequence and needs an explicit "yes". Declining restores the default. Confirming records the
  option in `acknowledge_caution`.
- **Fallback.** If the terminal cannot be driven (`TERM=dumb`, no window size, fewer rows than
  the interface plus 8, or fewer than 72 columns), setup uses line-based prompts on the TTY with
  the same options, markers and a typed `yes` for caution options. Client and coordinator
  settings are free text and always use line prompts.
- **Signals.** The terminal is restored by a drop guard, also on a panic, and before SIGTERM,
  SIGINT, SIGHUP or SIGQUIT take their default action.
- **Summary.** Shown before writing, listing every active caution option with its marker and
  the keys and values it covers (FR-068).

## Writes

- Setup edits only `[contention]`, the `role`-related tables, `ram.reserve_mib` and
  `gpu.reserve_vram_mib`, and preserves everything else byte for byte, including comments and
  the inline comment of an edited value. A key is edited only where the choice differs from the
  current value, in one fixed order.
- `contention.acknowledge_caution` is written as the set of caution options the result calls
  for that are acknowledged, by an earlier file entry, a flag or the confirmation screen. An
  option the file does not acknowledge yet needs a flag or the confirmation when this run brings
  it or changes what it covers; an entry the file already has covers the option, as it does for
  hand edits. An unacknowledged floor near its minimum that the file already had stays
  unacknowledged, so it keeps applying as twice the minimum. `ram.reserve_percent` and
  `gpu.reserve_vram_percent` are left as they are; the summary shows the resulting floors on this
  host.
- It validates the result through the normal config load, then renames atomically.

## Exit status and JSON

| Exit | Code | Meaning |
|---|---|---|
| 0 | — | Written, or unchanged |
| 2 | `ERR_CEDEGRID_SETUP_MISSING_CHOICE` | A required choice is missing in non-interactive mode; the message names the flag |
| 2 | `ERR_CEDEGRID_ARGUMENT` | A malformed value, or a flag that does not apply to the role |
| 3 | `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` | A caution option was chosen without `--acknowledge-caution <option>` |
| 4 | `ERR_CEDEGRID_CONFIG` | The result failed validation, as configuration loading reports it; nothing was written |
| 5 | `ERR_CEDEGRID_UNSUPPORTED_PLATFORM` | Node or coordinator setup on Windows |
| 130 | `ERR_CEDEGRID_SETUP_CANCELLED` | Cancelled interactively |

`--json` prints:

```json
{"written": true, "path": "...", "choices": {...}, "exceptions": [{"name": "performance_mode", "marker": "caution"}]}
```

`choices` maps each edited key, dotted, to its new value, or to `null` where a key was removed.
`exceptions` lists the caution options active in the result.

Errors print `{"error": {"code": ..., "message": ...}}`.

## Equivalence

The same choices made through flags and through the interface produce byte-identical files
(SC-011).
