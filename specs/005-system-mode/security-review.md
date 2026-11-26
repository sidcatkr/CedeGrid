# Feature 005: security review package

The files the pre-release security review covers (FR-032, research R14), as they stand on the
branch. The review runs separately from the development loop, before release, at the release
candidate's commit; this list is updated at that commit. Tests are in `tests/system_mode.rs`
unless named otherwise; `container::` fixtures run as root under systemd 249 with the packages
installed by dpkg.

## What the review checks

1. Only a member of the `cedegrid` group reaches the broker, checked per request from
   `SO_PEERCRED` and the group database as it is now; root and system accounts, below `UID_MIN`
   or above `UID_MAX`, are refused.
2. The request set is closed: framing, size and time limits, no identity in a request, no field
   echoed back, one reply line. A caller reaches only its own two units, by name and by marker.
3. Every systemd call is a fixed path with fixed arguments, a cleared environment and a timeout,
   and touches no unit without the broker's marker.
4. `oom_score_adj` is written only through a pidfd-held process inside a member's unit, read back,
   and a root-written value is the floor for the user. Unknown memory lowers nothing.
5. A job move freezes, checks every process's owner, moves, reads back and puts back on failure.
6. The administrator policy is read only from a root-owned, private, regular file, and an
   unreadable policy refuses every request and launch.
7. A member removed from the group is refused at once; its units stop once no job runs in them.
8. Install, upgrade and purge leave the host as documented, and nothing else runs as root.
9. The raise, at either level (unprotected, 006 FR-016 and FR-018; priority over plain users,
   FR-024 to FR-027), changes only the caller's own account slice's `CPUWeight` to that level's
   policy weight, only while that level's key (`modes.unprotected`, `modes.plain_priority`) is
   true and the caller's ordinary unit runs in that slice, never over a weight the broker did
   not set, and reads it back. It resets it on withdrawal, on the units' stop, on a 30 s
   lease's expiry, within 5 s of the level's key turning off or the caller leaving the group,
   and when the broker stops or next starts (a runtime drop-in at either policy weight,
   `ExecStopPost`). Whether other CedeGrid users run at the weight is the agent's check, not a
   boundary: a member can ask for a level the policy permits.

## Runs as root, or decides what root does

| File | What it holds | Tests that pin it |
|---|---|---|
| `src/broker.rs` | module root, the protocol version, socket path, group, installed binary and hidden subcommand names | `protocol_must_match`, `replies_from_another_protocol_or_garbage` |
| `src/broker/server.rs` | the listener (socket activation, socket owner and mode check), workers, per-user rate limit and per-caller serialization, request handling, `cpu.idle`, the OOM loop thread, the pass that stops a removed member's units once no job runs, and the raise's lease and its end (`expire_raises`, `end_left_raises` at start and from `ExecStopPost`) | `the_listener_serves_real_connections_with_real_peer_credentials`, `handle_checks_the_caller_before_the_request`, `rate_limit_is_per_user_and_refills`, `read_frame_stops_at_the_newline_the_limit_or_the_deadline`, `a_connection_gets_one_reply_line`, `a_refused_policy_file_stops_every_request`, `start_sets_cpu_idle_or_stops_the_idle_unit`, `a_removed_members_units_stop_once_no_job_runs` (unit and `container::`), `container::hostile_requests_are_refused_logged_and_change_nothing`, `container::a_flooding_member_is_rate_limited_and_others_are_still_served`, `container::fifty_members_cost_the_broker_little`, `broker_service::a_raise_ends_once_the_key_is_off_or_the_member_left`, `broker_service::raises_a_stopped_broker_left_are_reset_and_nothing_else` |
| `src/broker/peer.rs` | `SO_PEERCRED`, the group and passwd database reads, the caller check (user IDs from `UID_MIN` to `UID_MAX`) | `callers_are_checked_against_the_group_now`, `container::one_member_cannot_reach_another_members_units`, `container::socket_is_root_group_cedegrid_0660` |
| `src/broker/request.rs` | the closed request set (with `raise`), framing and size limit, identity-field refusal, replies | `closed_request_set_parses`, `request_size_limit_counts_the_newline`, `framing_errors_are_malformed`, `unknown_kinds_and_fields_are_refused`, `identity_fields_refuse_the_whole_request`, `refusal_messages_never_echo_odd_field_names`, `job_names_are_bounded`, `mode_and_job_only_where_their_kind_takes_them`, `replies_are_one_json_line` |
| `src/broker/admin_policy.rs` | `/etc/cedegrid/policy.toml` loading: owner, mode, no symlink, size, `deny_unknown_fields`; `modes.unprotected`, `modes.unprotected_cpu_weight`, `modes.plain_priority` and `modes.plain_priority_cpu_weight` (006); `principals.uid_min` to `uid_max` and the host's `UID_MIN` and `UID_MAX` | `priority_over_plain_users_is_the_administrators_to_permit`, `policy_defaults`, `policy_file_values_and_sources`, `policy_refusals`, `the_user_id_range_comes_from_login_defs`, `policy_files::*`, `unprotected_needs_its_own_policy_key`, `the_raise_weight_is_the_administrators_and_bounded` |
| `src/broker/units.rs` | the unit property builder and the `systemd-run` argument vector; slice choice | `units_are_named_and_owned_by_the_caller`, `ordinary_slice_follows_n4_and_idle_never_moves`, `pam_and_limits_follow_n3`, `builder_depends_only_on_caller_policy_and_placement`, `container::a_member_start_makes_both_units_with_the_contract_properties`, `container::units_lay_out_an_agent_cgroup_and_a_jobs_cgroup` |
| `src/broker/systemd.rs` | fixed-path `systemd-run` and `systemctl` calls with a cleared environment and timeouts; the marker checks before start, stop and status; a stop resets a unit it left failed; the account slice weight at either raise level (`raise_account_slice`, never over another weight) and the runtime drop-ins a stopped broker left (`raised_slices`) | `systemd_run_arguments_are_fixed_and_shell_free`, `show_output_parses_and_the_marker_is_checked`, `a_unit_without_the_marker_is_never_touched`, `a_failed_unit_of_ours_is_reset_then_started`, `a_unit_left_failed_by_a_stop_is_cleared`, `stop_and_status_reach_only_the_callers_units`, `systemd_failures_are_reported`, `broker_service::the_raise_weights_only_the_callers_account_slice_while_permitted`, `broker_service::a_raise_never_replaces_another_weight_on_the_account_slice`, `broker_service::the_plain_level_has_its_own_key_and_weight`, `container::an_unprotected_raise_weights_only_the_account_slice_and_every_end_resets_it` |
| `src/broker/moves.rs` | freeze, check, move, read back, thaw; putting back on failure | `job_moves::*`, `container::the_confined_broker_moves_a_members_job` |
| `src/broker/oom.rs` | the `/proc` and cgroup readers, principals, fair shares, task grouping, the pass that writes scores | `oom_pass::*`, `oom_loop::*` |
| `src/broker/score.rs` | the pure score function | `in_share_pages_lower_the_score_by_their_part_of_the_total`, `lowest_value_is_minus_999`, `share_goes_to_the_oldest_task_first`, `marking_follows_the_mode_and_d2`, `a_task_partly_in_share_is_not_marked`, `lowering_follows_n1`, `lowered_scores_return_when_lowering_stops`, `unchanged_values_are_not_written`, `unknown_memory_lowers_nothing_and_returns_after_the_window`, `an_empty_task_is_never_marked` |
| `src/broker/process_handle.rs` | pidfd plus `/proc/<pid>` directory handle; the only path for `oom_score_adj` writes (the broker does not use `src/kernel.rs`) | `process_handles::*`, `container::a_score_written_as_root_is_the_floor_for_the_user` |
| `src/main.rs` | the broker's hidden dispatch, before argument parsing, and the root refusal it is the exception to; the agent in a unit loads its configuration through `system_mode::unit_table` | `root_is_refused_except_for_the_broker`, `the_unit_agent_reads_its_configuration_from_the_users_config_dir` |
| `src/system_mode.rs` | the root refusal check, the client and its fallback; inside a unit, the agent's cgroup table and controls (`unit_table`, `apply_unit`, `UNIT_CONTROLS`), the administrator policy at launch (`launch_refusal_with`) and the unprotected grant (004) | `client::*`, `socket::*`, `agent_in_unit::*`, `policy_in_unit::*`, `container::a_silent_broker_falls_back_and_running_units_survive_a_restart`, `container::twenty_starts_all_land_in_the_members_units`, `container::without_a_configuration_the_unit_holds_and_says_why` |
| `packaging/deb/cedegrid-broker.service`, `cedegrid-broker.socket` | the broker's confinement and socket ownership; the stop hook that ends raises (`ExecStopPost`, 006) | `container::socket_is_root_group_cedegrid_0660`, `container::the_confined_broker_moves_a_members_job`, `container::an_unprotected_raise_weights_only_the_account_slice_and_every_end_resets_it` |
| `packaging/deb/cedegrid.slice`, `cedegrid_idle.slice` | the two root-level slices | `container::the_idle_slice_has_cpu_idle` |
| `packaging/deb/cedegrid-system.sysusers` | the group | `container::socket_is_root_group_cedegrid_0660` |
| `packaging/deb/postinst`, `prerm`, `postrm` | install, upgrade (a running broker is restarted) and purge (both slices stopped, `/run/cedegrid` removed), run as root by dpkg | `container::an_upgrade_restarts_a_running_broker_and_keeps_the_units`, `container::purge_leaves_the_host_as_before_the_install` |
| `tools/build_deb.py` | what goes into each package, with which modes | `tests/test_build_deb.py` |

## User side, facing the kernel (as decided for 001)

`src/cgroup.rs` keeps 001's delegated-root rules; inside a unit, the agent's jobs root is the
unit's own `jobs` cgroup, accepted only inside the user's own unit (`system_mode::apply_unit`,
`agent_in_unit::the_jobs_root_is_accepted_only_inside_the_users_own_unit`). Under D3 case A, item
057 Part C's files also go in: the fork-gate module, `src/supervision/unix.rs` and
`src/supervision/family.rs`.

006 FR-007b adds one thread-ID call on the user side: 0.5 s after a placed task starts, the
supervisor puts every thread of its unreaped leader back on the whole CPU scope
(`OwnedChild::release_threads`, `ThreadIdentity` in `src/supervision/unix.rs`). The leader's pidfd
shows it alive first; each thread is fixed by a thread pidfd, or by its start time under the
leader's `/proc/<pid>/task` where the kernel has none, and the same check runs after the call. A
failed check leaves the thread where it is and reports it. Only the supervisor's own child is
reached, and the kernel refuses the call on other users' threads
(`a_placed_task_starts_on_its_cpu_and_goes_back_on_the_scope`).

006's nice raise (FR-016, T041) adds a second one, outside System mode: where accounts share a CPU
cgroup and the account has a granted `RLIMIT_NICE`, an unprotected run's supervisor sets each
thread of its verified family to the raise and back (`renice_family`). Each member's start time
is checked before and after, and each thread is fixed and checked as above; a thread at another
value than the one moved from is left as the run set it
(`the_nice_raise_moves_every_thread_and_returns_to_the_launch_value`).

## Evidence to hand the review

- The container suite (`tools/docker-validation/system-mode/`, module `container`): eighteen
  fixtures, all passing at the commit this list was last updated at.
- The host runs T057 to T060, recorded in `docs/validation.md`, with the broker's journal over
  those runs: 244 requests answered, 2 refused `not_a_member`, 130 score writes, none failed.
- The source pack (`tools/pack_source_review.py --public-candidate`) passes its disclosure checks.

## Open before the review

- T072: rerun the OOM runs (T058) at the release candidate's commit.
- T055, T056: the release workflow's `.deb` build, checksums and apt repository signing, which
  R14 includes, are not built yet.
- The idle unit runs a holder until the idle executor (follow-up SYS-3, a later version); T061
  waits with it.
