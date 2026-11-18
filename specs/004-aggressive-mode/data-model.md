# Data Model: Aggressive Mode for CPU and GPU Compute

## ContentionMode (extended)

| Value | Order | Label | Admission margin (compute) | Scale-up interval | Caution |
|---|---|---|---|---|---|
| `yield_first` | 0 | Yield First | 15% | 30 s | no |
| `neutral` | 1 | Neutral | 5% | 10 s | no |
| `performance` | 2 | Performance | 2% | one monitor interval (compute), 2 s (memory) | no (001's default) |
| `aggressive` | 3 | `[Aggressive]` (red) | 0% | 0 (compute), 2 s (memory) | `aggressive_mode` |

- Validation: the configured margins stay ordered YF ≥ N ≥ P; Aggressive's 0% is fixed, not a
  configuration key.
- Running compute steps: none at Performance or above (001 item 057 Part B).

## ModeInput (extended)

Existing fields as in 001, plus:

| Field | Type | Meaning |
|---|---|---|
| `unprotected` | bool | The allocation runs under an unprotected grant (R5). |

## Allowances (per compute resource and observation)

See [contracts/policy-protection.md](contracts/policy-protection.md): Aggressive is Performance's
allowance with a 0% margin; unprotected is `capacity` without unknown activity, and Aggressive's
otherwise. Memory, both forms: Performance's.

## Unprotected grant (per run)

| Field | Meaning |
|---|---|
| `run_id` | the run it belongs to; it ends with the run |
| `confirmed` | `true` only when both SDK fields were set |
| `requested_unix_ms` | when the submission asked for it |
| `allowance_set_at_unix_ms` | the allowance record's time at the launch check |
| `started_unix_ms` | when the run started unprotected |

States: requested → checked at launch → active → ended. Active ends with the run, including its
retries. A refused request never becomes active. A drained and retried run keeps its grant. A new
run needs new fields.

## Allowance (per host, in the agent's `state.sqlite3`, its own table)

| Field | Meaning |
|---|---|
| `enabled` | off by default |
| `set_at_unix_ms` | when a person turned it on |
| `set_by` | always `interactive_setup` |

- Only interactive setup sets it. Any client clears it, without confirmation.
- Reads fail closed. A missing or corrupt database, a missing table, a read error, or a schema
  this build doesn't read all count as off.
- A restore from backup resets it to off.
- Turning it off leaves active grants running and refuses new ones.

## Administrator policy (System mode, extended)

| Key | Default | Meaning |
|---|---|---|
| `modes.unprotected` (named `unprotected_aggressive` before 006) | `false` | whether members may request the grant; the agent in the unit refuses it while false |

## Decision output (extended)

- `resources.<key>.mode` and `effective_mode` may be `aggressive`.
- Per-mode targets gain an `aggressive` entry.
- Per run: `unprotected` (bool) and the grant fields above.
