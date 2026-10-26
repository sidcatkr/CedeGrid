# Contract: names, fields, refusals and markers

## Configuration (`config_version = 1`)

| Key | Values | Rule |
|---|---|---|
| `contention.default_mode`, `contention.<resource>.mode`, per-job entries (003) | adds `aggressive` | needs `aggressive_mode` in `contention.acknowledge_caution` |
| `contention.max_mode` | adds `aggressive` | only when set explicitly, with `aggressive_mode`; the derived cap stops at `performance` |
| `contention.acknowledge_caution` | adds `aggressive_mode` | unknown entries still fail to load |
| any key that would enable unprotected Aggressive (`unprotected_aggressive`, `allow_unprotected_aggressive`, or the same under a per-job entry or profile) | — | fails to load: `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE: <key> cannot be stored; unprotected Aggressive is turned on per run only` |

## CLI (003's `run`, `setup`)

| Flag | Meaning |
|---|---|
| `--mode aggressive` | protected Aggressive for this run, within the node cap; a stored choice needs `aggressive_mode` |
| `--unprotected-aggressive` | turn the protection off for this run; implies `--mode aggressive` on CPU and GPU compute |
| `--confirm-unprotected-aggressive` | the second flag a non-interactive start needs |
| `cedegrid setup --max-mode aggressive --acknowledge aggressive_mode` | raise the node cap (its own confirmed step interactively) |

No short aliases. `--save` with `--unprotected-aggressive` refuses: `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE`.

### Interactive start (terminal on stdin and stdout, no `--json`)

1. Numbers screen from the agent: other active principals, their current CPU use, an estimated
   slowdown for a light user ("estimate"), then `Start this run unprotected? [y/n]` with no
   default.
2. `Type "slow other users for this run" to continue:`. Anything else cancels with
   `cancelled: the phrase did not match; nothing started`.

### Non-interactive start (no terminal, `--json`, SDK, agent tooling)

Refused unless both hold; the message names each missing condition:
- `ERR_CEDEGRID_UNPROTECTED_CONFIRMATION: --confirm-unprotected-aggressive is missing`
- `ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED: the host's "allow unprotected Aggressive" setting is off`

### Global allowance

Interactive `cedegrid setup` only: "Allow unprotected Aggressive for non-interactive runs", with
its own confirmation; recorded with its time. Non-interactive setup refuses:
`ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY`.

## SDKs (Python and TypeScript, the same names)

| Field | Type | Meaning |
|---|---|---|
| per-resource mode | `"aggressive"` | as the CLI |
| `unprotected_aggressive` | bool, default false | request the grant for this submission |
| `confirm_unprotected_aggressive` | bool, default false | the second confirmation (SDK starts are non-interactive) |

A parity test fails when the CLI and either SDK differ (FR-019).

## Wire (protocol v1, job and assignment)

- Job modes accept `aggressive` (contract case JC-05 becomes accepted with the acknowledgement;
  a new case rejects it without).
- Job field `unprotected_aggressive: { "confirmed": "interactive_phrase" | "flag_and_allowance",
  "challenge": <string, interactive only>, "requested_unix_ms": <u64> }`; absent means protected.
  The launching agent verifies it before Authorized; an older agent rejects the job.
- New contract cases: an unprotected request without confirmation (rejected), with the flag but
  no allowance (rejected), with an expired challenge (rejected).

## Markers

| Output | Protected Aggressive | Unprotected |
|---|---|---|
| setup, `top`, `ps`, `doctor`, `status` | red `[Aggressive]` | red `[Aggressive]` and red `UNPROTECTED` (`ps`, `top`) |
| JSON | `"mode": "aggressive"` | also `"unprotected": true` and the grant |
| logs | mode in each decision | one warning line at start: `warning: this run is UNPROTECTED: other users' compute may slow down until it ends` |

With `NO_COLOR`, the same text without color.
