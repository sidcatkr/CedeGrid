# Contract: decision and status output

## Policy `Decision` (`src/model.rs`)

The decision gains additive fields; `Decision` has no `deny_unknown_fields`.

```json
{
  "schema_version": 3,
  "resources": {
    "cpu": {"mode": "neutral", "effective_mode": "neutral", "own": [3800, 4000], "external": 2900,
            "unattributed": 120, "target_share": 4000, "floor": 0, "admission": "held",
            "measurement": "known", "reasons": [{"code": "contention_converging", "text": "..."}]},
    "gpu_memory:GPU-…": {"mode": "neutral", "activity": "quiet", "...": "..."}
  },
  "actions": [{"allocation_id": "a1", "resource": "cpu", "step": "cap", "target": 3000,
               "cause": "external_growth", "code": "yield_step_cap"},
              {"allocation_id": "a2", "resource": "ram", "step": "hint", "target": 6144,
               "cause": "floor_breach", "code": "yield_step_hint", "deadline_ms": 5000}],
  "exceptions": [{"name": "performance_mode", "marker": "caution"}],
  "max_mode": "neutral",
  "would_drain": ["..."],
  "reasons": ["..."]
}
```

- `would_drain` and the prose `reasons` remain for 0.2 readers and recorded replays.
  `would_drain` is derived from `actions` with step ≥ `drain`.

## CLI

| Command | Shows |
|---|---|
| `observe` | Per-resource `resources` from the decision for each sample, in the JSON output |
| `doctor` | Configured and effective modes, each floor on this host (`floors`: configured, minimum, effective, near the minimum, confirmed, raised), active caution options judged on this host, active caution options with markers, available actuation steps per resource, the interpretation of legacy GPU keys and their warnings |
| `status` (operator, coordinator) | Per-node `contention` summary from negotiated node reports |

## Reason codes

- Reason codes are stable `snake_case` strings, documented in `docs/guarantees.md`.
- The text is one sentence and is not stable.
- Tests assert on codes only (FR-039).
