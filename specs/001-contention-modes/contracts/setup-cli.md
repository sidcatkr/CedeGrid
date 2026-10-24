# Contract: `cedegrid setup`

## Invocation

```text
cedegrid [--config PATH] setup [--non-interactive] [--json]
    [--role node|coordinator|client]
    [--default-mode yield_first|neutral|performance]
    [--mode RESOURCE=MODE]...            # RESOURCE: cpu|ram|gpu_compute|gpu_memory
    [--max-mode MODE]
    [--floor-policy a|b|c]
    [--neutral-memory-policy a|b]
    [--set RESOURCE.KEY=VALUE]...        # tuned thresholds, e.g. gpu_memory.noise_tolerance_mib=512
    [--acknowledge-danger floor_policy_c]
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

  | Marker | Appearance |
  |---|---|
  | caution | `[caution]`, yellow (SGR 33) |
  | danger | `[danger]`, red (SGR 31) |

- **No color** when `NO_COLOR` is set to a non-empty value, when output is not a TTY, or when
  `TERM=dumb`. The labels always remain.
- **Danger confirmation.** Choosing a danger option opens a separate screen that restates the
  consequence and needs an explicit "yes". Declining restores the default.
- **Fallback.** If the terminal cannot be driven (no cursor control or too small), setup uses
  line-based prompts on the TTY.
- **Summary.** Shown before writing, listing every active exception with its marker.

## Writes

- Setup edits only `[contention]` and the `role`-related tables, and preserves everything else
  byte for byte.
- It validates the result through the normal config load, then renames atomically.

## Exit status and JSON

| Exit | Code | Meaning |
|---|---|---|
| 0 | — | Written, or unchanged |
| 2 | `ERR_CEDEGRID_SETUP_MISSING_CHOICE` | A required choice is missing in non-interactive mode; the message names the flag |
| 3 | `ERR_CEDEGRID_DANGER_UNACKNOWLEDGED` | A danger option was chosen without `--acknowledge-danger <option>` |
| 4 | `ERR_CEDEGRID_CONFIG_INVALID` | The result failed validation; nothing was written |
| 130 | — | Cancelled interactively |

`--json` prints:

```json
{"written": true, "path": "...", "choices": {...}, "exceptions": [{"name": "floor_policy_b", "marker": "caution"}]}
```

Errors print `{"error": {"code": ..., "message": ...}}`.

## Equivalence

The same choices made through flags and through the interface produce byte-identical files
(SC-011).
