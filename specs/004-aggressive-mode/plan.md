# Implementation Plan: Aggressive Mode for CPU and GPU Compute

**Branch**: specified on `001-contention-modes`; implemented on `004-aggressive-mode` after 001 is
merged into `dev` | **Date**: 2026-10-26 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `specs/004-aggressive-mode/spec.md`

## Summary

Aggressive is a fourth `ContentionMode` after Performance. It reuses Performance's policy path
with three overrides: a 0% idle margin on CPU and GPU compute, compute scale-up at once (the
scale-up interval is 0, and admission fills every free slot as Performance does), and memory at
Performance's floors only. Performance's dwell, hysteresis, reversal tolerance, reclaim bounds,
floors and unknown handling apply unchanged.

The protection keeps the mode ordering strict per observation (report 018 Decision 1, answer B).
For each compute resource and observation, protected Aggressive's target is the larger of:
- Performance's target for the same observation;
- the protected target: all available capacity, limited so that every other active principal
  that is waiting keeps the lesser of its demand and its fair share.

So while Performance holds more than the protection leaves, Aggressive equals Performance and
gives back exactly as Performance does, within 001's SC-012 reclaim bounds. Once Performance
has stepped down to the protected point or below, Aggressive holds the protected target.

Unprotected Aggressive is protected Aggressive with the protection off: it never steps compute
down for external demand and does not grow into activity of unknown amount. It is turned on
only per run, by `--unprotected-aggressive` or the matching SDK field, after a two-step
confirmation the launching agent checks. The CLI surfaces (`run`, `ps`, `top`, `--save`,
per-job entries, profiles, the setup interface) and per-user run-queue wait come from 003.

## Technical Context

**Language/Version**: Rust 1.88 (MSRV, edition 2024); Python SDK 3.10+; TypeScript SDK.

**Primary Dependencies**: none new. Existing crates only (serde, axum/rustls for the wire,
ratatui/crossterm from 003 for the setup interface).

**Storage**: the agent's SQLite state database (`state.sqlite3`) holds the global allowance
record; nothing new in configuration files beyond the mode value and its acknowledgement.

**Testing**: `cargo test` (policy unit and property tests in `tests/contention.rs`,
`tests/policy.rs`, `tests/configuration.rs`, `tests/cli.rs`, the decision snapshot in
`tests/decision_snapshot.rs`), the contract fixture `tests/contracts/cedegrid-0.2.json`,
`python/tests`, `npm/tests`, and native gates on the authorized test host (SC-008).

**Target Platform**: Linux for the protection and actuation; the configuration, SDK, wire and
refusal paths on every platform 001 supports.

**Project Type**: library + CLI + agent and coordinator services, with Python and TypeScript SDKs.

**Performance Goals**: Aggressive ≥ Performance on every resource in every observation
(SC-003, SC-007); a waiting light contender reaches its demand no later than under
Performance (SC-001, 001 SC-012); policy evaluation stays within 001's decision p95 (≤ 1 s).

**Constraints**: pure policy (Principle IV); no priority raise or thread inflation (FR-017);
Performance unchanged (FR-020); unprotected Aggressive never stored (FR-012); no new
dependency.

**Scale/Scope**: one new mode value, one new `ModeInput` field set, one new policy function,
the unprotected grant and the global allowance, label and marker in every output 003 adds.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | How the plan complies |
|---|---|
| I. External processes untouchable | Aggressive acts only through 001's actuators on own verified processes. The protection reads per-principal usage, run-queue wait and the probe; it never acts on a contender. |
| II. No harm in any mode (6.0.0) | Protected Aggressive's target is `max(Performance, protected)` per observation, so it never holds less than Performance and a waiting principal gets its share back no slower than Performance gives it (the 6.0.0 Modes rule). Floors, the OOM-victim rule and the no-priority-raise rule hold in both forms. Unprotected Aggressive is the one named exception: compute time only, per run, flag plus two-step confirmation, never stored, marked while active. Aggressive and its node cap are *caution* options with their own acknowledgement. |
| III. Unknown is never idle | Activity of unknown amount holds protected Aggressive at the fair share, as Performance does (the protected target includes an unbounded unknown principal). Unprotected Aggressive does not grow into it. A waiting principal whose demand is unknown keeps its fair share. |
| IV. Pure policy | `protected_allowance` is a pure function of `ModeInput`; the wait evidence and the grant enter as data. Confirmation and the allowance are checked in the agent, effects stay in the supervisor. |
| V. Rootless | No privilege. The allowance lives in the agent's own state database. |
| VI. Tests first | Every task group starts with a failing test: ordering, report 018's case, the protection cases, refusal paths, label and marker, CLI and SDK parity. SC-008 rests on native gates. |
| VII. Compatibility | `aggressive` in job and wire modes and the unprotected fields land in Rust, Python and TypeScript with the contract fixture (JC-05 flips to accepted with the acknowledgement; new cases for the unprotected fields). Older agents refuse an `aggressive` job, as JC-05 does today. `config_version = 1` stays. |
| VIII. Public docs | `docs/guarantees.md` replaces "Aggressive is not implemented" with the mode, its protection and the unprotected exception, generically. |

Result: pass. No violations, so Complexity Tracking is empty.

## Protection rule: where it goes

- `src/contention.rs:1325`, in `own_allowance`, right after the memory arm
  (`_ if input.kind.is_memory() => available`) and before Performance's contended arm (line
  1326): a `ContentionMode::Aggressive` arm that computes Performance's target for the same
  `ModeInput` (`own_allowance(&ModeInput { mode: Performance, ..input.clone() }, thresholds)`)
  and returns `performance.max(protected_allowance(input, thresholds))`. With
  `input.unprotected`, it returns the larger of that and own work's current usage, so own work
  never steps down for external demand; under activity of unknown amount that is the larger of
  the fair share and current usage, so it does not grow into the activity.
- `src/contention.rs:1655`, beside `neutral_share`: `protected_allowance`, the max-min share of
  `capacity - background` for own work (unbounded demand) against every other principal's
  demand: a waiting principal's known demand, or unbounded when unknown; a principal that is not
  waiting at its usage; the unattributed usage as in `neutral_share`; one unbounded principal for
  activity of unknown amount. Owner-unmanaged usage comes off as in `neutral_share`. With nobody
  waiting and no unknown activity, it equals `capacity - external`, the 0% margin target.
- `src/contention.rs:1234`, `ModeInput`: `waiting: Vec<(Option<u32>, Option<u64>)>` (waiting
  principals, with a demand where wait evidence or the probe gives one) and `unprotected: bool`.
- `src/contention.rs:715` and `:728`: `margin_percent(Aggressive)` is 0 for compute;
  `scale_up_interval_ms(Aggressive)` is 0 for compute and 2000 for memory.
  `src/contention.rs:839`: Aggressive takes Performance's response timing.
- `src/policy/measure.rs:641`, `:1087` and `:1201`: fill `waiting` from 001's contended state
  (a saturated principal without satisfied demand waits, demand unknown) and, once 003 lands,
  from per-user run-queue wait (demand = usage plus unserved runnable time).
- `src/policy/measure.rs:1271` (`targets_of`) iterates `ContentionMode::ALL`, which gains
  Aggressive, so every decision's mode targets gain an `aggressive` entry.
- `src/agent/service.rs:928`: `fill` is true for an effective mode at or above Performance.

## Project Structure

### Documentation (this feature)

```text
specs/004-aggressive-mode/
├── plan.md              # This file
├── research.md          # Phase 0: decisions R1-R9
├── data-model.md        # Phase 1: entities and fields
├── quickstart.md        # Phase 1: validation scenarios and native gates
├── contracts/
│   ├── policy-protection.md   # The target rule and its invariants
│   └── cli-sdk-wire.md        # Names, fields, refusals, markers
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks output
```

### Source Code (repository root)

```text
src/
├── contention.rs          # ContentionMode::Aggressive, margins, scale-up, own_allowance arm,
│                          # protected_allowance, ModeInput fields, reason codes
├── policy/measure.rs      # waiting evidence into ModeInput; targets_of
├── policy/admission.rs    # Aggressive admission pace (fills like Performance)
├── config.rs              # "aggressive" mode value, aggressive_mode acknowledgement,
│                          # max_mode explicit raise, refusal of unprotected keys
├── setup.rs, setup/interface.rs  # red [Aggressive] label, node-cap step, global allowance
├── state.rs               # global allowance record
├── protocol.rs            # job/wire mode value and the unprotected grant fields
├── coordinator.rs         # accepts aggressive within the node cap
├── agent/service.rs       # fill pacing; grant check at launch; marker and warning
├── agent/launch.rs        # unprotected grant verification before Authorized
├── explain.rs             # observe/doctor label, marker, held_back.protected_share
└── (003) run, ps, top     # flags, numbers screen, phrase, label, marker, --save refusal

python/cedegrid/, npm/src/ # mode value, unprotected_aggressive and confirm fields
tests/
├── contention.rs          # ordering property, protection cases, report 018's case
├── policy.rs              # scenario ordering, several light users, wait again, no cycling
├── decision_snapshot.rs   # random scenarios with Aggressive; existing modes unchanged
├── configuration.rs       # acknowledgement, cap, refusal of stored unprotected keys
├── cli.rs                 # confirmation steps, refusals, label and marker
└── contracts/cedegrid-0.2.json  # JC-05 and new unprotected cases
```

**Structure Decision**: the existing single crate and SDK layout. No new module: the policy work
is one function and one arm in `src/contention.rs`, the grant check sits in the agent's launch
path, and the CLI parts extend 003's commands.

## Order against 003

Report 018's recommendation, kept: after 001 merges,
1. 003's setup and foundational phases (task schema, local agent, per-user wait evidence);
2. 004's policy, configuration, SDK and wire core (US1 policy tasks, the grant model);
3. the rest of 003 (`ps`, `top`, setup interface);
4. 004's CLI parts (numbers screen and phrase, label and marker, `--save` refusal).

Running all of 003 before all of 004 also works. Without 003's wait evidence, a waiting
principal's demand is unknown, so its protected share is its fair share and Aggressive equals
Performance under contention; it gains on Performance when idle and when wait evidence gives a
light principal's demand (R3).

## Complexity Tracking

No violations.
