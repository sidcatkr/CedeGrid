# Implementation Plan: Aggressive Mode for CPU and GPU Compute

**Branch**: built on the 005 branch in parallel with 001's last tasks, for this release |
**Date**: 2026-10-26, reworked 2026-11-17 against constitution 8.1.0 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `specs/004-aggressive-mode/spec.md`

## Summary

Aggressive is a fourth `ContentionMode` after Performance. Since 001 item 057, Performance takes
no running steps on compute; its allowance only decides admission. Aggressive is that path with
two overrides: an admission margin of 0% on CPU and GPU compute (Performance keeps 2%), and
admission that fills what fits in one heartbeat (a compute scale-up interval of 0). Memory is
Performance's: the floors bound admission, and nothing is hinted, drained or marked.

Protected Aggressive keeps 001's admission rule. New own work starts only where every other
principal keeps the lesser of its demand and its fair share, and never into activity of unknown
amount. In `own_allowance` that is Performance's contended arm with the margin at 0:
`max(fair share, available, probe)`, where `available` already holds a starved principal's
recorded demand out (`expected_external`). So protected Aggressive admits at least what
Performance admits in every observation, and the ordering stays strict.

Unprotected Aggressive is the admission exception. With the grant on an allocation and no
unknown activity, its compute allowance is the whole capacity (the kernel and the GPU driver
split it); under unknown activity, it is protected Aggressive's. The grant comes per submission
through two SDK fields, carried on the wire, and is checked by the launching agent before the
launch barrier persists Authorized. It also needs the host's allowance, which a person turns on
in interactive `cedegrid setup` after the numbers screen and the fixed phrase. On a System mode
host, the administrator policy must also permit it.

003's CLI surfaces (`run`, `--save`, per-job entries, profiles, `ps`, `top`) are out of this
release. When 003 lands, it adds the same flags and markers through the parity test.

## Technical Context

**Language/Version**: Rust 1.88 (MSRV, edition 2024); Python SDK 3.10+; TypeScript SDK.

**Primary Dependencies**: none new.

**Storage**: the agent's SQLite state database (`state.sqlite3`) holds the allowance record. It
fails closed: missing, corrupt, unreadable or another schema reads as off. Configuration gains
only the mode value, its acknowledgement and the explicit cap. The System mode administrator
policy gains one key.

**Testing**: `cargo test` (`tests/contention.rs`, `tests/policy.rs`, `tests/configuration.rs`,
`tests/system_mode.rs`, the decision snapshot), the contract fixture
`tests/contracts/cedegrid-0.2.json`, `python/tests`, `npm/tests`, and native gates on the test
host (SC-008), scheduled through the coordinator after 001's E9.

**Target Platform**: Linux for actuation and gates; configuration, SDK, wire and refusal paths
on every platform 001 supports.

**Project Type**: library + CLI + agent and coordinator services, with Python and TypeScript SDKs.

**Performance Goals**: Aggressive admits at least what Performance admits in every observation
(SC-003, SC-007). On an idle host it fills all capacity within one heartbeat. Policy evaluation
stays within 001's decision p95 (≤ 1 s).

**Constraints**: pure policy (Principle IV); no running compute step in either form (FR-004);
no priority raise or thread inflation (FR-017); Performance unchanged (FR-020); unprotected
Aggressive never stored (FR-012); no new dependency. `src/policy*`, `src/contention.rs` and
`src/explain.rs` stay untouched until the coordinator says 001's T622 has landed and 001 is
merged in.

**Scale/Scope**: one mode value, one `ModeInput` field, one arm in `own_allowance`, the grant
check at launch, the allowance record, one administrator key, and labels and markers in the
outputs 001 has.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | How the plan complies |
|---|---|
| I. External processes untouchable | Aggressive acts only through 001's actuators on own verified processes, and in this design only at admission. It reads per-principal usage and the probe; it never acts on a contender. |
| II. No harm in any mode (8.1.0) | Both forms take no running compute step (Performance and Aggressive rule). Protected Aggressive keeps the Admission rule, with 0% margin ("Aggressive 0%" in Loss by mode). Unprotected Aggressive is the one admission exception: new compute work only, per submission, two fields plus the allowance (and the administrator key in System mode), never stored, never into unknown activity or the floors, reported while active. Memory follows Floors by mode and OOM order for Performance and Aggressive: no hint, drain, TERM or mark. Aggressive and its cap are caution options with their own acknowledgement. The first-come GPU floor is a Yield First and Neutral exception and is unchanged. |
| III. Unknown is never idle | Unknown activity keeps protected Aggressive at Performance's unknown arm (the fair share). Unprotected Aggressive takes protected Aggressive's allowance under unknown activity, so it never starts into it. An unreadable allowance counts as off. |
| IV. Pure policy | The arm is a pure function of `ModeInput`; the grant enters as `ModeInput.unprotected`. The allowance, the fields and the administrator key are checked in the agent; effects stay in the supervisor. |
| V. Least privilege | No new privilege. The allowance lives in the agent's own state. In System mode, the broker gains nothing: the agent in the member's unit reads the administrator policy, as 005 T054 does for allowed modes. |
| VI. Tests first | Every task group starts with a failing test: ordering with Aggressive, the 0% margin, the unprotected arm, no running step, refusal paths, the fail-closed allowance, markers, SDK and wire parity. SC-008 rests on native gates. |
| VII. Compatibility | `aggressive` in job and wire modes and the grant fields land in Rust, Python and TypeScript with the contract fixture. JC-05 flips to accepted, with the node cap deciding the effective mode. An older agent refuses an `aggressive` job, as JC-05 does today. `config_version = 1` stays. |
| VIII. Public docs | `docs/guarantees.md` replaces "Aggressive is not implemented" with the mode, admission-only protection and the unprotected exception, generically. |

Result: pass. No violations, so Complexity Tracking is empty.

## Admission rule: where it goes

Function names, not line numbers: 001 is still landing T622 in these files.

- `src/contention.rs`, `ContentionMode`: add `Aggressive` after `Performance` (name
  `aggressive`, label `Aggressive`, order 3), and to `ContentionMode::ALL`. `margin_percent`
  returns 0 for Aggressive on compute; it's a constant, not a configuration key.
  `scale_up_interval_ms` is 0 for Aggressive on compute and Performance's 2000 on memory.
  Response timing (`LadderTiming`) is Performance's.
- `src/contention.rs`, `own_allowance`: Aggressive takes Performance's arms with its 0% margin,
  so contended compute is `max(neutral_share, available, probe)` and uncontended is `available`.
  With `input.unprotected` set and no unknown activity, compute returns `input.capacity`. The
  memory arm is shared with Performance.
- `src/contention.rs`, `ModeInput`: `unprotected: bool`, set from the allocation's grant.
- `src/policy/ladder.rs` and `src/policy.rs`: every place that tests "Performance or above" for
  "no running step" must include Aggressive. Check each match on `ContentionMode` for
  exhaustiveness, which the compiler forces.
- `src/policy/measure.rs`, `targets_of`: iterates `ContentionMode::ALL`, so decisions gain an
  `aggressive` target.
- `src/policy/admission.rs`, `admissions`: an allocation under an unprotected grant uses the
  unprotected allowance; the rest use their job's mode, as 001 FR-030 does.
- `src/agent/service.rs`: `fill` is true for an effective mode at or above Performance (it
  already is for Performance; Aggressive joins).
- `src/agent/launch.rs`: the grant check before Authorized (R7). It verifies both fields, the
  allowance record (fail closed), the node cap at Aggressive, and, inside a System mode unit,
  the administrator key. It logs the one warning line, and refusals name what is missing.
- `src/state.rs`: the allowance table and its reads, which fail closed. `src/backup.rs`:
  restore resets it to off.
- `src/setup.rs`, `src/setup/interface.rs`: the cap step to Aggressive with its confirmation;
  the allowance step (numbers screen, yes/no without default, the fixed phrase); turning it off
  without confirmation, also non-interactively.
- `src/config.rs`, `src/config_migration.rs`: the mode value, `aggressive_mode`, an explicit
  `max_mode = "aggressive"`, and the refusal of every key that would enable unprotected
  Aggressive.
- `src/protocol.rs`, `src/coordinator.rs`: the job mode value and the grant object; the
  coordinator passes the grant to the agent and decides nothing about it.
- `src/broker/admin_policy.rs` and `specs/005-system-mode/contracts/admin-policy.md`: the
  key `modes.unprotected_aggressive = false` by default. `src/system_mode.rs`,
  `launch_refusal_with`: refuse a grant while it is off.
- `src/explain.rs`: `[Aggressive]` and `UNPROTECTED` in `observe` and `doctor`, and in JSON.

## Project Structure

### Documentation (this feature)

```text
specs/004-aggressive-mode/
├── plan.md              # This file
├── research.md          # Phase 0: decisions R1-R11
├── data-model.md        # Phase 1: entities and fields
├── quickstart.md        # Phase 1: validation scenarios and native gates
├── contracts/
│   ├── policy-protection.md   # The admission rule and its invariants
│   └── cli-sdk-wire.md        # Names, fields, refusals, markers
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks output
```

### Source Code (repository root)

```text
src/
├── contention.rs          # ContentionMode::Aggressive, margin 0, scale-up 0, own_allowance arm,
│                          # ModeInput.unprotected, reason codes        (after T622)
├── policy.rs, policy/     # no running step for Aggressive; targets_of; admissions (after T622)
├── config.rs, config_migration.rs  # mode value, acknowledgement, cap, refusals
├── setup.rs, setup/interface.rs    # cap step, allowance on (numbers screen, phrase) and off
├── state.rs, backup.rs    # allowance record (fail closed); restore resets it
├── protocol.rs, coordinator.rs     # job mode value and grant object
├── agent/launch.rs, agent/service.rs  # grant check before Authorized; fill; warning line
├── broker/admin_policy.rs, system_mode.rs  # administrator key; refusal in a unit
└── explain.rs             # label and marker in observe and doctor      (after T622)

python/cedegrid/, npm/src/ # mode value; unprotected_aggressive and confirm fields
tests/
├── contention.rs, policy.rs        # ordering, 0% margin, unprotected arm, no running step
├── decision_snapshot.rs   # existing modes unchanged; Aggressive entries added
├── configuration.rs       # acknowledgement, cap, refusals of stored unprotected keys
├── state.rs (or agent.rs) # allowance fail-closed cases, restore reset
├── system_mode.rs         # administrator key refusal in a unit
└── contracts/cedegrid-0.2.json  # JC-05 and the grant cases
```

**Structure Decision**: the existing single crate and SDK layout. No new module.

## Order in this release

1. Now, while 001 lands T622: the parts outside 001's held files. That covers configuration,
   the state record, setup, protocol and SDK fields, the grant check at launch, the
   administrator key, and the contract fixture. Each starts with failing tests.
2. After the coordinator says T622 has landed, merge 001, then do the policy parts in
   `contention.rs` and `policy*`, and the markers in `explain.rs`.
3. The security review of the unprotected admission path (the grant check, the allowance
   record, the refusals), before release.
4. Native gates on the test host, scheduled through the coordinator after 001's E9.

Without 003's run-queue wait, a contending principal's demand comes from usage and the probe, as
in Performance. Protected Aggressive then differs from Performance by the 2% margin and the fill
pace.

## Complexity Tracking

No violations.
