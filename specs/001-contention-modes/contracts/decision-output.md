# Contract: decision and status output

## Policy `Decision` (`src/model.rs`)

The decision gains additive fields; `Decision` has no `deny_unknown_fields`.

```json
{
  "schema_version": 3,
  "resources": {
    "cpu": {"mode": "neutral", "effective_mode": "neutral", "own": [3800, 4000], "external": 2900,
            "unattributed": 120, "target_share": 4000, "floor": 0,
            "admission": {"state": "held", "reason": "the resource is cooling down"},
            "measurement": {"state": "known"}, "held_back": {"margin": 400, "contention": 2700},
            "reasons": [{"code": "contention_converging", "text": "..."}]},
    "gpu_memory:GPU-…": {"mode": "neutral", "activity": "quiet", "...": "..."}
  },
  "actions": [{"allocation_id": "a1", "resource": "cpu", "step": "cap", "target": 3000,
               "cause": "external_growth", "code": "yield_step_cap"},
              {"allocation_id": "a2", "resource": "ram", "step": "hint", "target": 6144,
               "cause": "floor_breach", "code": "yield_step_hint", "deadline_ms": 5000}],
  "exceptions": [{"name": "performance_mode", "marker": "caution"}],
  "max_mode": "performance",
  "allocation_modes": {"a1": {"cpu": "yield_first", "ram": "neutral", "gpu_compute": "neutral",
                              "gpu_memory": "neutral"}},
  "would_drain": ["..."],
  "reasons": ["..."]
}
```

- `allocation_modes` is each allocation's effective mode per resource, `min(requested,
  max_mode)`. A resource's `effective_mode` is the node's, which sets the aggregate; per-job
  modes order yielding inside it. A request above the cap adds `mode_capped` to the resource's
  reasons.
- `would_drain` and the prose `reasons` remain for 0.2 readers and recorded replays.
  `would_drain` is derived from `actions` with step ≥ `drain`.
- `held_back` lists, per resource and in resource units, what is kept from own work or from
  admission: `floor`, `reserve`, `margin`, `contention`, `peak`, `noise_tolerance`, `unknown`,
  `cooldown`, `backoff`, `charge_shortfall`. Zero amounts are left out (FR-069). The node
  report carries the same object.

## CLI

| Command | Shows |
|---|---|
| `observe` | Per-resource `resources` from the decision for each sample, in the JSON output |
| `doctor` | Configured and effective modes, each floor on this host (`floors`: configured, minimum, effective, near the minimum, confirmed, raised), active caution options judged on this host with their markers and the keys and values each covers (`caution_options`: name, marker, settings), each resource's admission and `held_back` (T111), available actuation steps per resource, the interpretation of legacy GPU keys and their warnings |
| `status` (operator, coordinator) | Per-node `contention` summary from negotiated node reports, including `held_back` (T112) |

## Reason codes

- Reason codes are stable `snake_case` strings, documented in `docs/guarantees.md`.
- The text is one sentence and is not stable.
- Tests assert on codes only (FR-039).
