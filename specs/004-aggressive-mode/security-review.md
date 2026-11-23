# Feature 004: security review package

The files the pre-release review of the unprotected admission path covers (tasks T034), as they
stand on the branch. The review runs separately from the development loop, at the release
candidate's commit, and this list is updated at that commit.

## What the review checks

1. No configuration file, saved setup, environment variable, migration or node cap can turn
   unprotected on (FR-012). A file that tries fails to load, naming the key.
2. The host's allowance is set only by interactive setup on a terminal, after the numbers, a yes
   with no default and the fixed phrase (FR-013). Every read failure counts as off, and a restore
   clears it (FR-015).
3. A grant needs both SDK fields. The coordinator rejects an unconfirmed one at submission, and
   the agent re-checks every condition before the launch barrier persists Authorized: the
   confirmation, the node cap at Aggressive, the allowance and, in a System mode unit, the
   administrator key (FR-014, FR-015a). A refusal names each missing condition and starts
   nothing.
4. The unprotected budget exists only under an Aggressive cap and while CPU is known; the agent
   reports it only while the allowance and, in a unit, the key permit it; the coordinator places
   only granted requests against it (R12), with no less CPU than the node's own budget, as the
   agent's launch capacity has it (006 FR-015, `8635f60`).
5. A run that started unprotected on this host keeps the allowance it started under for its
   later attempts here, after the allowance is turned off. The cap and the key are still checked
   on every attempt, a database that cannot be read refuses, and a restore forgets every started
   grant. A new run reads the host's allowance.
6. Memory, the floors, unknown activity, the no-priority-raise rule and the operator envelope are
   unchanged by either form (FR-006, FR-010, FR-017). 006's priority over plain users is a
   separate install option with its own policy key and weight; no unprotected field reaches it.

## The unprotected admission path

| File | What it holds | Tests that pin it |
|---|---|---|
| `src/state.rs` (`Allowance`, `read_allowance`, `set_allowance`, `clear_allowance`) | the host's allowance: one row in its own table; every read failure counts as off, and a missing database reads as never set; never through a link; schema checked | `tests/state.rs` `unprotected_allowance::*` |
| `src/state.rs` (`ActiveGrant`, `record_grant`, `active_grant`, `allowance_for_run`, `clear_grants`) | a started grant, per run (task and request time), in its own table; the first start's record stays; consulted only while the allowance reads cleanly off; every read failure finds none | `tests/state.rs` `unprotected_allowance::a_started_grant_is_kept_per_run_and_reads_fail_closed`, `a_later_attempt_of_a_started_run_keeps_the_allowance_it_started_under` |
| `src/backup.rs` (`restore`) | a restore clears the allowance and every started grant | `tests/backup.rs` `snapshot_restore_preserves_ledger_artifacts_and_fences_uncertain_capacity` (fails without either clear) |
| `src/setup.rs` (`allowance`, `allowance_dialog`, `ALLOWANCE_PHRASE`) | turning the allowance on only on a terminal, after the numbers (no slowdown figure), a yes with no default and the fixed phrase; off anywhere; an allowance already on is reported, not set again | `tests/cli.rs` `the_allowance_needs_a_yes_and_the_exact_phrase_on_a_terminal`, `the_allowance_never_turns_on_without_a_terminal_and_turns_off_anywhere` |
| `src/config.rs` (`refuse_unprotected_keys`, `aggressive_mode` acknowledgement, the cap) | no file can store unprotected; Aggressive needs its acknowledgement and an explicit cap | `tests/configuration.rs` `a_file_can_never_store_unprotected`, `a_migrated_file_that_holds_unprotected_is_refused`, `aggressive_needs_its_acknowledgement_and_an_explicit_cap` |
| `src/execution_model.rs` (`JobContention.unprotected`, `UnprotectedGrant`) | the grant on the wire; unknown fields denied | contract cases UG-01 to UG-04 |
| `src/protocol.rs` (`validate_launch_request`) | an unconfirmed grant, or one without Aggressive on CPU and GPU compute, is refused | `tests/configuration.rs` `unprotected_grant_cases_parse_and_validate_alike` |
| `python/cedegrid/client.py`, `npm/src/client.ts` (`command_task`) | the grant is sent only when both fields are set | `python/tests/test_transport.py`, `npm/tests/sdk.test.ts` (UG cases) |
| `src/system_mode.rs` (`grant_refusal`, `host_gaps`, `Gap`, `unprotected_policy_gap`, `unprotected_refusal_with`) | every missing condition named in one line; the System mode administrator key; the same conditions, each with its fix, for `doctor` | `tests/system_mode.rs` `an_unprotected_grant_starts_only_with_every_condition_met`, `unprotected_needs_its_own_policy_key` |
| `src/broker/admin_policy.rs` (`modes.unprotected`) | the administrator key, off by default | `tests/system_mode.rs` `policy_file_values_and_sources`, `unprotected_needs_its_own_policy_key` |
| `src/agent/service.rs` (the launch check) | a grant is refused before launch unless every condition holds, reading the run's allowance; a start records the grant and logs the warning once | `tests/system_mode.rs` `container::an_unprotected_grant_starts_only_under_the_policy_key_and_the_allowance` (refusals, a start, a drained and retried attempt after the allowance is turned off, and a new run refused; without the per-run allowance the retry is refused and the task ends in `needs_reconciliation`) |
| `src/contention.rs` (`own_allowance`, `ModeInput.unprotected`) | the unprotected allowance: capacity on compute, never into unknown activity or on memory | `tests/contention.rs` `unprotected::*`, `aggressive::*` |
| `src/policy/measure.rs`, `src/policy.rs`, `src/model.rs` (`unprotected_budget`) | the grant's budget, only under an Aggressive cap and never while CPU is unknown | `tests/policy.rs` `the_unprotected_budget_exists_only_under_an_aggressive_cap` |
| `src/agent/report.rs` (`unprotected_permitted`) | the budget is reported only while the allowance reads on and, in a unit, the key permits | `tests/policy.rs` `the_report_carries_the_unprotected_budget_only_under_the_allowance` |
| `src/main.rs` (`doctor --text`), `src/explain.rs` (`unprotected_text`) | read-only: shows whether a grant could start here and how to fix each missing condition; reads the allowance and, where the policy file exists, the administrator key | `tests/cli.rs` `doctor_marks_aggressive_and_its_caution` |
| `src/coordinator.rs` (`placement`) | a granted request is placed against the admit immediately budget, with no less CPU than the node's own budget; every other request as before | `tests/coordinator.rs` `a_granted_request_is_placed_against_the_admit_immediately_budget`, `a_granted_request_keeps_the_cpu_a_plain_principal_opened` |
| `src/agent/service.rs` (`steer_nice`, `nice_target`), `src/actuation.rs` (`Raise`) | 006's nice raise, only where accounts share a CPU cgroup, outside a unit, with a granted `RLIMIT_NICE`: sent to an unprotected run's supervisor while no protected first-comer is on the CPU, withdrawn on drain, at the first-comer's arrival and when the run ends; a journal that cannot be read raises nothing | `src/agent.rs` `only_a_wanted_undrained_unprotected_run_gets_the_nice_raise`, `src/actuation.rs` raise tests |
| `src/supervision/unix.rs` (`NiceRaise`, `renice_family`) | the supervisor moves only its verified family's threads that run at the launch value to the raise, never below -10 nor at or above the launch value, and back; each call takes only a thread ID, under the thread checks below | `nice_tests::a_nice_raise_moves_between_the_launch_value_and_the_raise`, `tests/supervision.rs` `the_nice_raise_moves_every_thread_and_returns_to_the_launch_value` |

## Open before the review

- T033: the full checks at the release candidate's commit.

## Host gates

T035 is done (`docs/validation.md`, feature 004): fill and F2c on CPU with a cross-cgroup
contender (2026-11-17), F2c with a same-cgroup contender (2026-11-18, `7ee0a23`), and GPU compute
after 001's fix (2026-11-18, `b4dda15`). Every protected run admitted own work only within its
share beside the second account; every unprotected run started beside it with its warning; no
run stepped own work.
