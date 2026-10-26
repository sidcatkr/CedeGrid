# Research: Aggressive Mode for CPU and GPU Compute

All Technical Context items are known; these are the design decisions behind the plan.

## R1. Aggressive reuses Performance's path

- **Decision**: `ContentionMode::Aggressive` after `Performance`. It takes Performance's arms in
  `own_allowance`, Performance's response timing (`LadderTiming::new`), dwell, hysteresis and
  reversal tolerance, and three overrides: `margin_percent` 0 for compute, `scale_up_interval_ms`
  0 for compute (memory keeps 2 s), and admission `fill` as in Performance.
- **Rationale**: the spec says "Performance plus overrides, not a separate policy path" (FR-002)
  and Performance must not change (FR-020). A shared path keeps every Performance guarantee.
- **Alternatives**: a separate Aggressive ladder (rejected: duplicates reclaim, floors and
  unknown handling, and risks drifting from Performance).

## R2. The protection rule (report 018 Decision 1, answer B)

- **Decision**: per compute resource and observation, protected Aggressive's target is
  `max(performance, protected)`, where `performance` is `own_allowance` for the same `ModeInput`
  with `mode = Performance`, and `protected` is `protected_allowance(input)`: the max-min share
  of `capacity - background` for own work against each other principal's demand (a waiting
  principal's known demand, or unbounded; others at their usage; one unbounded principal for
  unknown activity).
- **Rationale**: it makes Aggressive ≥ Performance by construction, so 001 FR-065 and SC-007
  hold per observation with no exception (constitution 6.0.0). While Performance holds more than
  the protection leaves, the two are equal and the SC-012 reclaim bounds apply unchanged; after
  Performance steps down to the protected point, Aggressive holds the protected target.
- **Worked case** (report 018): 8000 m, own work and one light user who uses 200 m and wants
  1000 m and waits. `protected` = 7000 m. Performance (2% margin, 160 m) holds capacity minus the
  user's usage minus 160 m, so as the user grows by 160 m per interval it goes 7640, 7480, 7320,
  7160, 7000, and 6840 m once the user takes its 1000 m. Aggressive is 7640, 7480, 7320, 7160,
  7000, 7000 m, and holds 7000 m.
- **Alternatives**: A, give back at once and allow Aggressive < Performance for a few intervals
  (rejected by the user's answer B); C, change Performance too (rejected: FR-020).

## R3. Where waiting and demand come from

- **Decision**: `ModeInput.waiting` lists waiting principals with an optional demand.
  - Every host: a principal 001 counts as contending (saturated, at least the contention floor,
    demand not known to be satisfied) waits, demand unknown. 001's PSI-only state (FR-072)
    counts too.
  - With 003's per-user run-queue wait (003 FR-050): a principal whose wait rate is above the
    threshold waits, with demand = usage + unserved runnable time over the window.
  - The growth probe (001 FR-021) ends waiting when it finds a demand satisfied.
- **Rationale**: an unknown demand gives the fair share (unbounded in max-min), the most a
  protected share can be, so unknown never under-protects (Principle III). Wait alone never
  drives own work below its fair share (FR-004), since max-min never gives own work less.
- **Consequence**: before 003's wait evidence lands, Aggressive equals Performance under
  contention (the protected target is at most own work's fair share, which Performance never
  goes below). It still gains on Performance when idle (0% margin, scale-up at once) and when
  the probe or wait evidence gives a light principal's demand.

## R4. Memory

- **Decision**: memory targets are Performance's: floors only, scale-up every 2 s at 2% of
  capacity, hints and drains only at a floor breach.
- **Rationale**: FR-002's memory override ("no hints or drains while above the floors") is
  already Performance's behavior: `contended` is false for memory, so memory steps only at a
  breach. The task pins it with a test instead of changing code. FR-018 keeps floors and VRAM
  floors unchanged.

## R5. Unprotected Aggressive

- **Decision**: a per-run grant, not a mode value. `ModeInput.unprotected` is set for the
  run's allocations; the target is `max(protected Aggressive target, own usage)`, so own work
  never steps down for external demand, and under unknown activity it is at most the larger of
  the fair share and current usage, so it does not grow into the activity. Floors, Guaranteed
  rules and the operator envelope apply as in every mode.
- **Rationale**: keeps four modes for ordering, labels and the node cap; the grant ends with
  the run (FR-016). Unprotected ≥ protected ≥ Performance, so ordering holds.

## R6. Names

- **Decision**:
  - mode value `aggressive` in configuration, per-job entries, SDKs and the wire;
  - acknowledgement `aggressive_mode` in `contention.acknowledge_caution`, required whenever any
    mode or `max_mode` is `aggressive`;
  - node cap: `contention.max_mode = "aggressive"` only when set explicitly; the derived cap (the
    most aggressive node mode) stops at `performance`, with `mode_capped`;
  - per-run flags: `--mode aggressive` (003's `run`), `--unprotected-aggressive`, and for
    non-interactive starts `--confirm-unprotected-aggressive`; no short aliases;
  - SDK fields per submission: `unprotected_aggressive` and `confirm_unprotected_aggressive`
    (booleans);
  - fixed phrase: `slow other users for this run`, typed exactly;
  - global allowance: "Allow unprotected Aggressive for non-interactive runs", shown in the
    interactive setup only.
- **Rationale**: long, explicit names that say what they do (FR-011); the phrase names the
  consequence and the scope.

## R7. Where confirmation is checked

- **Decision**: the launching agent checks the grant before the launch barrier persists
  Authorized. An interactive grant carries the agent's per-run challenge (issued with the
  numbers screen, valid 120 s) and the typed phrase; the client attests a terminal on stdin and
  stdout, and `--json` and SDK clients never take the interactive path. A non-interactive grant
  needs `confirm_unprotected_aggressive` and the allowance record in the agent's state.
- **Rationale**: no client, file or profile can skip the check. The phrase prevents accidental
  use; it does not stop an operator who scripts their own terminal, which stays their explicit
  per-run choice within the constitution's exception.

## R8. The global allowance record

- **Decision**: a row in the agent's `state.sqlite3`: `enabled`, `set_at_unix_ms`,
  `set_by = "interactive_setup"`. Only `cedegrid setup` on a terminal can set it, after its own
  confirmation; non-interactive setup, the SDK and agent tooling are refused. Turning it off
  never ends a running grant (spec edge case).
- **Rationale**: FR-012 forbids configuration files from enabling unprotected Aggressive; the
  allowance only gates non-interactive starts and is recorded with its time (FR-015).

## R9. Observability

- **Decision**: decisions add `held_back.protected_share` (when the protected target is the
  larger, the available capacity own work leaves for waiting principals' protected shares) and
  the reason code `aggressive_protected_share`; a
  run's JSON gains `"unprotected": true` and its grant (how confirmed, start time). `[Aggressive]`
  is red in setup, `top`, `ps`, `doctor`, `status`; `UNPROTECTED` is red in `ps`, `top`, logs and
  JSON. `NO_COLOR` keeps the text without color (001 SC-011).
- **Rationale**: SC-006, and 001 SC-016's held-back table extended by one row.
