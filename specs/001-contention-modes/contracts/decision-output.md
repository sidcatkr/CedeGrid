# Contract: decision and status output

## Policy `Decision` (`src/model.rs`)

The decision gains additive fields; `Decision` has no `deny_unknown_fields`.

```json
{
  "schema_version": 3,
  "resources": {
    "cpu": {"mode": "neutral", "effective_mode": "neutral", "own": [3800, 4000], "external": 2900,
            "unattributed": 120, "target_share": 4000, "floor": 0,
            "admission": {"state": "held", "code": "admission_cooling_down",
                          "reason": "The resource is cooling down after its last change."},
            "measurement": {"state": "known"},
            "held_back": {"margin": 400, "contention": 2700, "capacity": 8000,
                          "percent": {"margin": 5.0, "contention": 33.8}},
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
  "reasons": ["..."],
  "reason_codes": [{"code": "cooldown_pending", "text": "..."}]
}
```

- `allocation_modes` is each allocation's effective mode per resource, `min(requested,
  max_mode)`. A resource's `effective_mode` is the node's, which sets the aggregate; per-job
  modes order yielding inside it. A request above the cap adds `mode_capped` to the resource's
  reasons.
- Item 029 adds, per allocation, `oom_preferred` (RAM: own memory above its fair share, the
  allocations that make up the excess in victim order; FR-023) and `save_progress` (in-share
  allocations at a floor breach others' growth caused, once per episode; FR-075). Both are
  omitted when empty.
- `granted_cores` maps each run with a core lever to the logical CPUs it may use (003 FR-043),
  omitted when empty; the CPU status then carries `cpu_cores_granted`.
- `would_drain` and the prose `reasons` remain for 0.2 readers and recorded replays.
  `would_drain` is derived from `actions` with step ≥ `drain`.
- `held_back` lists, per resource and in resource units, what is kept from own work or from
  admission: `floor`, `reserve`, `margin`, `ceiling` (CPU a visible ancestor ceiling keeps
  from the budget beyond the reserve and margin; never contention), `contention`, `peak`,
  `noise_tolerance`, `unknown`, `cooldown`, `backoff`, `charge_shortfall`. Zero amounts are
  left out (FR-069). `capacity` is
  the resource's capacity and `percent` each cause's share of it, to one decimal. On GPU
  compute, `compute_path` is `measured` or `unknown` with a `code`
  (`gpu_compute_unknown_activity`, `gpu_compute_amount_unknown` or
  `gpu_compute_recently_inconsistent`). The node report carries the same object.
- Every code the decision reports is in the catalog `REASON_CODES` (`src/contention.rs`),
  listed in `docs/guarantees.md`: resource `reasons`, `admission.code`, action codes and
  causes, `held_back` causes as `held_back_<cause>`, the compute path, `exceptions` and
  `reason_codes`. `reason_codes` carries the prose `reasons` with their codes. A debug
  build asserts that every reported code is in the catalog.
- An action's `cause` is `own_over_target` when own usage is over its target without external
  growth, apart from `external_growth` (FR-011, FR-018). CPU's resource status adds `cedegrid`,
  the CPU in use by CedeGrid's own processes, which `doctor` and `status` show as in use by
  CedeGrid, never as external usage (FR-069).

## CLI

| Command | Shows |
|---|---|
| `observe` | One JSON object per sample: `snapshot` and `decision`, with `resources`, `actions` and `exceptions` always present |
| `doctor` | Configured and effective modes, each floor on this host (`floors`: configured, minimum, effective, near the minimum, confirmed, raised), active caution options judged on this host with their markers and the keys and values each covers (`caution_options`: name, marker, settings), each resource's mode, usage, demand, target share, admission, steps, reasons and `held_back` (`resources`, `actions`, `reason_codes`, `max_mode`), the steps the configured backend offers per resource (`available_steps`), the interpretation of legacy GPU keys and their warnings. `--text` prints the same as readable text |
| `status` (operator, coordinator) | Per-node `contention`: the negotiated summary with each resource's admission and `held_back`, or `"not negotiated"`; `status_page` node items carry the same |

## Reason codes

- Reason codes are stable `snake_case` strings, documented in `docs/guarantees.md`.
- The text is one sentence and is not stable.
- Tests assert on codes only (FR-039).
