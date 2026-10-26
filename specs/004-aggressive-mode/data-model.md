# Data Model: Aggressive Mode for CPU and GPU Compute

## ContentionMode (extended)

| Value | Order | Label | Margin (compute) | Scale-up interval | Caution |
|---|---|---|---|---|---|
| `yield_first` | 0 | Yield First | 15% | 30 s | no |
| `neutral` | 1 | Neutral | 5% | 10 s | no |
| `performance` | 2 | Performance (yellow) | 2% | one monitor interval (compute), 2 s (memory) | `performance_mode` |
| `aggressive` | 3 | `[Aggressive]` (red) | 0% | 0 (compute), 2 s (memory) | `aggressive_mode` |

- Validation: the configured margins stay ordered YF ≥ N ≥ P; Aggressive's 0% is fixed, not a
  configuration key.
- `memory_allowance_step_percent(Aggressive)` = Performance's 2.

## ModeInput (extended)

Existing fields as in 001, plus:

| Field | Type | Meaning |
|---|---|---|
| `waiting` | list of (UID or unattributed, demand or unknown) | Other principals waiting for this resource this interval, with a demand where run-queue wait (003 FR-050) or the probe gives one (R3). |
| `unprotected` | bool | The allocation runs under an unprotected grant (R5). |

## Targets (per compute resource and observation)

- `performance` = `own_allowance(mode = Performance)`, unchanged from 001.
- `protected` = max-min share of `capacity - background` for own work (unbounded) against:
  waiting principals at their demand (unbounded when unknown), other principals at their usage,
  the unattributed usage, and one unbounded principal while activity of unknown amount exists;
  minus owner-unmanaged usage.
- Protected Aggressive target = `max(performance, protected)`.
- Unprotected Aggressive target = `max(protected Aggressive target, own usage)`.
- Memory, both forms: Performance's target.

Invariants: Yield First ≤ Neutral ≤ Performance ≤ protected Aggressive ≤ unprotected Aggressive
for every resource and observation; protected Aggressive ≥ own work's fair share.

## Protected share (per other principal and compute resource)

| Field | Meaning |
|---|---|
| principal | UID, or the unattributed principal |
| usage | its observed usage |
| demand | known demand, or unknown |
| evidence | `usage`, `run_queue_wait`, `probe` or `psi_only` |
| protected share | the lesser of its demand and its fair share (its fair share when the demand is unknown) |

## Unprotected grant (per run)

| Field | Meaning |
|---|---|
| `run_id` | the run it belongs to; it ends with the run |
| `confirmed` | `interactive_phrase` or `flag_and_allowance` |
| `challenge` | the agent's per-run challenge for an interactive grant (valid 120 s, used once) |
| `started_unix_ms` | when the run started unprotected |
| `allowance_set_at_unix_ms` | for `flag_and_allowance`, the allowance record's time |

State: requested → confirmed → active → ended (with the run, including its retries). A refused
request never becomes active. A drained and retried run keeps its grant; a new run needs a new
grant.

## Global allowance (per host, in the agent's `state.sqlite3`)

| Field | Meaning |
|---|---|
| `enabled` | off by default |
| `set_at_unix_ms` | when a person turned it on |
| `set_by` | always `interactive_setup` |

Only the interactive setup writes it. Turning it off leaves active grants running.

## Decision output (extended)

- `resources.<key>.mode` and `effective_mode` may be `aggressive`.
- `held_back.protected_share`: when the protected target is the larger, the available capacity
  own work leaves for waiting principals' protected shares (0 while Aggressive equals
  Performance, whose own held-back reasons apply).
- Reason code `aggressive_protected_share`.
- Per-mode targets gain an `aggressive` entry.
- Per run: `unprotected` (bool) and the grant fields above.
