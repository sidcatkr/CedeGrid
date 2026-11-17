# Contract: the administrator policy

File: `/etc/cedegrid/policy.toml`. It is optional; without it, the defaults below apply. Loaded at
each request. It is refused (the broker then serves no request, and says why in the journal and
in each refusal) when the file or its directory is not owned by root, or is writable by group or
others.

```toml
policy_version = 1

[modes]
allowed = ["yield_first", "neutral", "performance"]   # default: all of 001's modes except Aggressive

[limits]
tasks_per_user = 64          # CedeGrid tasks one user may run at once; default 64
tasks_max = 4096             # TasksMax= on each unit; default 4096
open_files = 65536           # LimitNOFILE=; omitted means systemd's default (N3 A)
processes = 0                # LimitNPROC=; 0 means systemd's default
locked_memory_mib = 0        # LimitMEMLOCK=; 0 means systemd's default

[principals]
uid_min = 1000               # 001 FR-059, used by the OOM loop; default from login.defs UID_MIN
principal_min_percent = 1    # 001 FR-059's default

[units]
pam_session = "none"         # N3: "none" (default), "cedegrid" or "login"
```

- Every section uses `deny_unknown_fields`. An unknown key refuses the file.
- `units.pam_session` (user decision N3 of 2026-11-17): `none` opens no PAM session and the
  `limits` section sets the units' process limits; `cedegrid` sets `PAMName=cedegrid`, a PAM
  service the administrator provides with only `pam_limits`; `login` uses the host's `login`
  stack. Under `cedegrid` and `login` the session's limits apply instead of the `limits` section,
  except `tasks_max`.
- A user's setting above a limit is refused in one line naming the policy key (FR-010).
- `doctor` shows the effective values and whether each comes from the file or the default.
- Aggressive is allowed only when the administrator lists it, and unprotected Aggressive's
  per-run confirmation (004) still applies on top.
- Who applies what: the broker applies `modes.allowed` to its `start` and `move` requests. The
  agent in the unit, which runs the root-installed binary, applies `modes.allowed` and
  `tasks_per_user` to each job, since the broker never sees jobs. The kernel enforces only the
  unit's properties (`tasks_max`, `open_files`, `processes`, `locked_memory_mib`). The docs say
  that the allowed modes are a rule for CedeGrid's own behavior, not a security boundary: work
  inside a unit can still create processes up to the unit's limits.
- Defaults marked here are *provisional*, set by the plan and confirmed by the user.
