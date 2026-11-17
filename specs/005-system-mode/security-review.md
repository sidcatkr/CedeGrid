# Feature 005: security review package

The files the pre-release security review covers (FR-032, research R14), as they stand on the
branch. The review runs separately from the development loop, before release, at the release
candidate's commit; this list is updated at that commit.

## Runs as root, or decides what root does

| File | What it holds |
|---|---|
| `src/broker.rs` | module root, the protocol version, socket path, group, installed binary and hidden subcommand names |
| `src/broker/server.rs` | the listener (socket activation, socket owner and mode check), workers, per-user rate limit and per-caller serialization, request handling, `cpu.idle`, the OOM loop thread |
| `src/broker/peer.rs` | `SO_PEERCRED`, the group and passwd database reads, the caller check |
| `src/broker/request.rs` | the closed request set, framing and size limit, identity-field refusal, replies |
| `src/broker/admin_policy.rs` | `/etc/cedegrid/policy.toml` loading: owner, mode, no symlink, size, `deny_unknown_fields` |
| `src/broker/units.rs` | the unit property builder and the `systemd-run` argument vector; slice choice |
| `src/broker/systemd.rs` | fixed-path `systemd-run` and `systemctl` calls with a cleared environment and timeouts; the marker checks before start, stop and status |
| `src/broker/moves.rs` | freeze, check, move, read back, thaw; putting back on failure |
| `src/broker/oom.rs` | the `/proc` and cgroup readers, principals, fair shares, task grouping, the pass that writes scores |
| `src/broker/score.rs` | the pure score function |
| `src/broker/process_handle.rs` | pidfd plus `/proc/<pid>` directory handle; the only path for `oom_score_adj` writes |
| `src/main.rs` | the broker's hidden dispatch, before argument parsing, and the root refusal it is the exception to |
| `src/system_mode.rs` | the root refusal check and the client |
| `packaging/deb/cedegrid-broker.service`, `cedegrid-broker.socket` | the broker's confinement and socket ownership |
| `packaging/deb/cedegrid.slice`, `cedegrid_idle.slice` | the two root-level slices |
| `packaging/deb/cedegrid-system.sysusers` | the group |
| `packaging/deb/postinst`, `prerm`, `postrm` | install, removal and purge, run as root by dpkg |
| `tools/build_deb.py` | what goes into each package, with which modes |

## User side, facing the kernel (as decided for 001)

`src/cgroup.rs` (delegated root detection inside a unit, T023, not built yet) and, under D3
case A, item 057 Part C's files (the fork-gate module, `src/supervision/unix.rs`,
`src/supervision/family.rs`).

## Evidence to hand the review

- The container suite (`tools/docker-validation/system-mode/`, `tests/system_mode.rs`, module
  `container`): eleven fixtures as root under systemd 249 with the packages installed by dpkg.
- The host runs T057 to T061 and T072, once recorded in `docs/validation.md`.

## Open before the review

- `CapabilityBoundingSet=` on the broker's service (T033) is not set yet.
- The idle executor and the agent inside the units (T022) are not built; the units run a holder.
