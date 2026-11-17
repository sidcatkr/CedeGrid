# Feature 004: security review package

The files the pre-release review of the unprotected admission path covers (tasks T034), as they
stand on the branch. The review runs separately from the development loop, at the release
candidate's commit, and this list is updated at that commit.

## The unprotected admission path

| File | What it holds | Tests that pin it |
|---|---|---|
| `src/state.rs` (`Allowance`, `read_allowance`, `set_allowance`, `clear_allowance`) | the host's allowance: one row in its own table; every read failure counts as off; never through a link; schema checked | `tests/state.rs` `unprotected_allowance::*` |
| `src/backup.rs` (`restore`) | a restore clears the allowance | `tests/backup.rs` `snapshot_restore_preserves_ledger_artifacts_and_fences_uncertain_capacity` (fails without the clear) |
| `src/setup.rs` (`allowance`, `allowance_dialog`, `ALLOWANCE_PHRASE`) | turning the allowance on only on a terminal, after the numbers, a yes with no default and the fixed phrase; off anywhere | `tests/cli.rs` `the_allowance_needs_a_yes_and_the_exact_phrase_on_a_terminal`, `the_allowance_never_turns_on_without_a_terminal_and_turns_off_anywhere` |
| `src/config.rs` (`refuse_unprotected_keys`, `aggressive_mode` acknowledgement, the cap) | no file can store unprotected Aggressive; Aggressive needs its acknowledgement and an explicit cap | `tests/configuration.rs` `a_file_can_never_store_unprotected_aggressive`, `a_migrated_file_that_holds_unprotected_aggressive_is_refused`, `aggressive_needs_its_acknowledgement_and_an_explicit_cap` |
| `src/execution_model.rs` (`JobContention.unprotected_aggressive`, `UnprotectedGrant`) | the grant on the wire; unknown fields denied | contract cases UG-01 to UG-04 |
| `src/protocol.rs` (`validate_launch_request`) | an unconfirmed grant, or one without Aggressive on CPU and GPU compute, is refused | `tests/configuration.rs` `unprotected_grant_cases_parse_and_validate_alike` |
| `python/cedegrid/client.py`, `npm/src/client.ts` (`command_task`) | the grant is sent only when both fields are set | `python/tests/test_transport.py`, `npm/tests/sdk.test.ts` (UG cases) |
| `src/system_mode.rs` (`grant_refusal`, `unprotected_refusal_with`) | every missing condition named in one line; the System mode administrator key | `tests/system_mode.rs` `an_unprotected_grant_starts_only_with_every_condition_met`, `unprotected_aggressive_needs_its_own_policy_key` |
| `src/broker/admin_policy.rs` (`modes.unprotected_aggressive`) | the administrator key, off by default | `tests/system_mode.rs` `policy_file_values_and_sources`, `unprotected_aggressive_needs_its_own_policy_key` |
| `src/agent/service.rs` (the launch check) | a grant is refused before launch unless every condition holds; the warning event | end-to-end agent test (T019, open) |
| `src/contention.rs` (`own_allowance`, `ModeInput.unprotected`) | the unprotected allowance: capacity on compute, never into unknown activity or on memory | `tests/contention.rs` `unprotected::*`, `aggressive::*` |
| `src/policy/measure.rs`, `src/policy.rs`, `src/model.rs` (`unprotected_budget`) | the grant's budget, only under an Aggressive cap and never while CPU is unknown | `tests/policy.rs` `the_unprotected_budget_exists_only_under_an_aggressive_cap` |
| `src/agent/report.rs` (`unprotected_permitted`) | the budget is reported only while the allowance reads on and, in a unit, the key permits | open: a report-level test |
| `src/coordinator.rs` (`placement`) | a granted request is placed against the unprotected budget; every other request as before | `tests/coordinator.rs` `a_granted_request_is_placed_against_the_unprotected_budget` |

## Open before the review

- T019: the end-to-end agent test of the launch check, and keeping the grant across a retry.
- A test of `unprotected_permitted` against a real state database, and inside a unit.
- T020's container fixture for the administrator key.
- The native gates (T035).
