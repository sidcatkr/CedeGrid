# Contract: `cedegrid setup`

## Invocation

```text
cedegrid [--config PATH] setup [--non-interactive] [--json]
    [--role node|coordinator|client]
    [--default-mode yield_first|neutral|performance]
    [--mode RESOURCE=MODE]...            # RESOURCE: cpu|ram|gpu_compute|gpu_memory
    [--max-mode MODE]
    [--ram-floor-mib N] [--gpu-floor-mib N]
    [--margin RESOURCE.MODE=PERCENT]...  # MODE: yield_first|performance
    [--guaranteed-hint-deadline-s N]
    [--set RESOURCE.KEY=VALUE]...        # tuned thresholds, e.g. gpu_memory.noise_tolerance_mib=512
    [--acknowledge-caution performance_mode|small_floor]...
```

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
| Space | Toggle or select |
| Enter | Confirm |
| Esc, `q`, Ctrl-C | Cancel; nothing is written and the terminal is restored |

- **Option lines.** Each shows a one-line consequence. The default option has no color and no
  label.
- **Markers.**

  | Marker | Appearance | Options |
  |---|---|---|
  | caution | `[caution]`, yellow (SGR 33) | Performance on any resource or as the node cap; a floor below twice its minimum |

- **No color** when `NO_COLOR` is set to a non-empty value, when output is not a TTY, or when
  `TERM=dumb`. The labels always remain.
- **Caution confirmation.** Choosing a caution option opens a separate screen that restates the
  consequence and needs an explicit "yes". Declining restores the default. Confirming records the
  option in `acknowledge_caution`.
- **Fallback.** If the terminal cannot be driven (no cursor control or too small), setup uses
  line-based prompts on the TTY.
- **Summary.** Shown before writing, listing every active caution option with its marker.

## Writes

- Setup edits only `[contention]`, the `role`-related tables, `ram.reserve_mib` and
  `gpu.reserve_vram_mib`, and preserves everything else byte for byte. `ram.reserve_percent` is
  left as it is; the summary shows the resulting floor on this host.
- It validates the result through the normal config load, then renames atomically.

## Exit status and JSON

| Exit | Code | Meaning |
|---|---|---|
| 0 | — | Written, or unchanged |
| 2 | `ERR_CEDEGRID_SETUP_MISSING_CHOICE` | A required choice is missing in non-interactive mode; the message names the flag |
| 3 | `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` | A caution option was chosen without `--acknowledge-caution <option>` |
| 4 | `ERR_CEDEGRID_CONFIG` | The result failed validation, as configuration loading reports it; nothing was written |
| 130 | — | Cancelled interactively |

`--json` prints:

```json
{"written": true, "path": "...", "choices": {...}, "exceptions": [{"name": "performance_mode", "marker": "caution"}]}
```

Errors print `{"error": {"code": ..., "message": ...}}`.

## Equivalence

The same choices made through flags and through the interface produce byte-identical files
(SC-011).
