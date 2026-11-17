# Research: Aggressive Mode for CPU and GPU Compute

All Technical Context items are known; these are the design decisions behind the plan. Reworked
on 2026-11-17 against constitution 8.1.0; R2, R3 and R5 replace their 2026-10-26 versions,
which protected waiting principals with running steps.

## R1. Aggressive reuses Performance's path

- **Decision**: `ContentionMode::Aggressive` after `Performance`. It takes Performance's arms in
  `own_allowance`, Performance's response timing, and two overrides: `margin_percent` 0 on
  compute, and `scale_up_interval_ms` 0 on compute (memory keeps 2 s). Admission `fill` is as in
  Performance.
- **Rationale**: the constitution defines Aggressive as "Performance with no admission margin,
  filling idle capacity at once", and Performance must not change (FR-020). A shared path keeps
  every Performance guarantee.
- **Alternatives**: a separate Aggressive path (rejected: duplicates floors and unknown handling,
  and risks drifting from Performance).

## R2. Protection is the admission rule

- **Decision**: protected Aggressive's compute allowance is Performance's with the margin at 0.
  Contended, that's `max(fair share, available, probe)`, where `available = capacity −
  expected_external` already holds out a starved principal's recorded demand. Uncontended, it's
  `available`. The allowance decides admission only: since 001 item 057 Part B, neither
  Performance nor Aggressive caps, pauses or drains running work for another's compute.
- **Rationale**: constitution 8.1.0, Principle II: "Admission. In every mode except unprotected
  Aggressive, CedeGrid MUST NOT start new own work … where it would take another principal below
  the lesser of that principal's demand and its fair share", and "In Performance and Aggressive,
  CedeGrid MUST NOT cap, pause or drain running own work …". 001's F2c gate expects own
  admission up to `max(S, A − m)` with the other principal at or above `min(demand, fair
  share)`; with m = 0 that is this arm.
- **Alternatives**: the 2026-10-26 rule `max(Performance, protected)` on running targets
  (superseded: it took running steps, which 8.x forbids in Performance and Aggressive).

## R3. Where demand comes from

- **Decision**: 001's evidence: usage, the satisfied-demand record behind `expected_external`,
  and the growth probe. 003's per-user run-queue wait joins when 003 lands.
- **Rationale**: the admission rule needs a principal's demand; 001 already measures it for
  Performance. Without run-queue wait, an understated demand on a shared-cgroup host is the
  probe's to reveal, as in Performance.

## R4. Memory

- **Decision**: memory is Performance's in both forms. The floors bound admission. A floor breach
  sends no hint, drain or TERM, and no own task is marked as an OOM victim.
- **Rationale**: Principle II, Floors by mode and OOM order, for Performance and Aggressive.

## R5. Unprotected Aggressive

- **Decision**: a per-submission grant, not a mode value. `ModeInput.unprotected` is set for the
  grant's allocations. Its compute allowance is `capacity` while no unknown activity exists, and
  protected Aggressive's allowance otherwise. Running work is never stepped, as in every mode
  at or above Performance. Floors, Guaranteed rules and the operator envelope apply.
- **Rationale**: the constitution's one admission exception covers starting compute work only,
  never into unknown activity or the floors. Unprotected ≥ protected ≥ Performance, so the
  ordering holds.

## R6. Names

- **Decision**:
  - mode value `aggressive` in configuration, SDKs and the wire;
  - acknowledgement `aggressive_mode` in `contention.acknowledge_caution`, required whenever any
    mode or `max_mode` is `aggressive`;
  - node cap: `contention.max_mode = "aggressive"` only when set explicitly; the derived cap
    stops at `performance`, with `mode_capped`;
  - SDK fields per submission: `unprotected_aggressive` and `confirm_unprotected_aggressive`
    (booleans, default false);
  - fixed phrase in setup: `slow other users' compute on this host`, typed exactly;
  - the allowance in setup: "Allow unprotected Aggressive on this host";
  - System mode administrator key: `modes.unprotected_aggressive`, default `false`;
  - 003 adds `--unprotected-aggressive` and `--confirm-unprotected-aggressive` to `run`, with no
    short aliases.
- **Rationale**: long, explicit names that say what they do (FR-011). The phrase names the
  consequence and its scope, the host.

## R7. Where the grant is checked

- **Decision**: the launching agent checks the grant before the launch barrier persists
  Authorized. It needs both submission fields, the allowance record, the node cap at Aggressive,
  and, in a System mode unit, the administrator key. The coordinator only carries the grant. A
  refusal names each missing condition and starts nothing unprotected.
- **Rationale**: no client, file or profile can skip the check. The phrase in setup prevents
  accidental use per host. The two fields keep each submission explicit.

## R8. The allowance record

- **Decision**: one row in its own table in the agent's `state.sqlite3`: `enabled`,
  `set_at_unix_ms`, `set_by = "interactive_setup"`. Only `cedegrid setup` on a terminal sets it,
  after the numbers screen and the phrase. Any client may clear it without confirmation,
  including a non-interactive `cedegrid setup` flag. Reads fail closed: a missing, corrupt or
  unreadable database, a missing table, or a schema this build doesn't read counts as off. A
  restore from backup resets it to off. Turning it off never ends a running grant.
- **Rationale**: FR-012 forbids configuration files from enabling unprotected Aggressive, and
  FR-015 requires it to fail closed (the user's choice 3 A, 2026-11-17). An additive table needs
  no migration of existing rows. An older build ignores the table, and a newer build that finds
  no row reads off.
- **Alternatives**: a configuration key with an acknowledgement record (rejected by the user:
  a file edit could turn it on).

## R9. Observability

- **Decision**: decisions carry `aggressive` among the mode targets. A run's JSON gains
  `"unprotected": true` and its grant (fields, allowance time, start time). `[Aggressive]` is red
  in setup, `doctor`, `status` and `observe`, and `UNPROTECTED` is red in logs, `status` and
  `observe`. `NO_COLOR` keeps the text without color (001 SC-011). `ps` and `top` follow with 003.
- **Rationale**: SC-006.

## R10. System mode

- **Decision**: the administrator policy gains `modes.unprotected_aggressive` (default
  `false`). The agent in a member's unit reads it through `admin_policy::load`, as T054 does for
  allowed modes, and refuses a grant while it's off. An unreadable policy refuses every launch,
  as today. The broker itself gains no code path.
- **Rationale**: on a shared server, an administrator decides whether any member may slow
  others' compute (user's choice 4 A). Keeping the check in the agent leaves the root broker's
  surface unchanged.

## R11. The numbers screen without 003

- **Decision**: interactive setup computes the numbers from the agent's latest observation: the
  number of other active principals, their CPU use, and an estimated slowdown for a light user
  (labelled an estimate). It shows a yes/no question with no default, then asks for the fixed
  phrase.
- **Rationale**: FR-013. 001's setup already has a terminal interface and the agent's state.
