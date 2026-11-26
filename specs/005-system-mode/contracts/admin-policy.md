# Contract: the administrator policy

File: `/etc/cedegrid/policy.toml`. It is optional; without it, the defaults below apply. Loaded at
each request. It is refused (the broker then serves no request, and says why in the journal and
in each refusal) when the file or its directory is not owned by root, or is writable by group or
others.

```toml
policy_version = 1

[modes]
allowed = ["yield_first", "neutral", "performance"]   # default: all of 001's modes except Aggressive
unprotected = false                        # default; true lets members request 006's unprotected option
unprotected_cpu_weight = 400               # default; the account slice's CPUWeight under unprotected, 101 to 10000
plain_priority = false                     # default; true lets members choose 006's priority over plain users
plain_priority_cpu_weight = 200            # default; the account slice's CPUWeight under it, 101 to 10000

[limits]
tasks_per_user = 64          # CedeGrid tasks one user may run at once; default 64
tasks_max = 4096             # TasksMax= on each unit; default 4096
open_files = 65536           # LimitNOFILE=; omitted means systemd's default (N3 A)
processes = 0                # LimitNPROC=; 0 means systemd's default
locked_memory_mib = 0        # LimitMEMLOCK=; 0 means systemd's default

[principals]
uid_min = 1000               # 001 FR-059, used by the OOM loop; default from login.defs UID_MIN
principal_min_percent = 1    # 001 FR-059's default

[principals.system_accounts]  # 001 FR-059 (T635): the same type and defaults as [contention.system_accounts]
uid_ranges = [[60578, 60705], [61184, 65519], [65534, 65535]]  # default: systemd's ranges that hold no people
subordinate_uid_file = "/etc/subuid"  # default; empty turns the owner mapping off

[units]
pam_session = "none"         # N3: "none" (default), "cedegrid" or "login"
ordinary_slice = "auto"      # N4: "auto" (default), "user" or "cedegrid"
```

- Every section uses `deny_unknown_fields`. An unknown key refuses the file.
- `principals.system_accounts` is 001's `SystemAccounts` type, so its defaults and checks are the
  agent's: each range is `[first, last]` with first at most last, and the file path is absolute
  or empty. A user ID at or above `uid_min` and in a range is a system account: background demand
  for the memory shares, and refused as a caller (FR-004). The broker reads the subordinate user
  ID file only when it is a regular file that root owns and only root may write, at most 1 MiB,
  again whenever it changes; otherwise it maps nothing and each such user ID counts as itself.
- `units.pam_session` (user decision N3 of 2026-11-17): `none` opens no PAM session and the
  `limits` section sets the units' process limits; `cedegrid` sets `PAMName=cedegrid`, a PAM
  service the administrator provides with only `pam_limits`; `login` uses the host's `login`
  stack. Under `cedegrid` and `login` the session's limits apply instead of the `limits` section,
  except `tasks_max`.
- `units.ordinary_slice` (user decision N4 of 2026-11-17): `auto` puts the ordinary unit in
  `user-<uid>.slice` where `user.slice` passes the cpu controller to its account slices, else in
  `cedegrid.slice`; `user` and `cedegrid` force one. A weight that follows the number of busy
  accounts (N4 C) is not built; if it is, it reads per-account counters only, writes only
  `cedegrid.slice`'s weight within a clamped range, and falls back to the lowest weight when a
  counter is unknown. The Yield First unit is always in `cedegrid_idle.slice`.
- A user's setting above a limit is refused in one line naming the policy key (FR-010).
- `doctor` shows the effective values and whether each comes from the file or the default.
- Aggressive is allowed only when the administrator lists it, and unprotected's
  per-run confirmation (004) still applies on top.
- `modes.unprotected` (004 FR-015a, user choice of 2026-11-17): the agent in the unit
  refuses an unprotected grant while it is false, with
  `ERR_CEDEGRID_UNPROTECTED_POLICY`, whatever `modes.allowed` lists.
- Who applies what: the broker applies `modes.allowed` to its `start` and `move` requests. The
  agent in the unit, which runs the root-installed binary, applies `modes.allowed` and
  `tasks_per_user` to each job, since the broker never sees jobs. The kernel enforces only the
  unit's properties (`tasks_max`, `open_files`, `processes`, `locked_memory_mib`). The docs say
  that the allowed modes are a rule for CedeGrid's own behavior, not a security boundary: work
  inside a unit can still create processes up to the unit's limits.
- Defaults marked here are *provisional*, set by the plan and confirmed by the user.
