# Contract: what the package installs and what the broker builds

## Installed by `cedegrid-system`

| Item | Content |
|---|---|
| `cedegrid-broker.socket` | `ListenStream=/run/cedegrid/broker.sock`, `SocketUser=root`, `SocketGroup=cedegrid`, `SocketMode=0660` |
| `cedegrid-broker.service` | runs the installed binary's fixed broker subcommand as root; confined as far as the host's systemd allows (`NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `RestrictAddressFamilies=AF_UNIX`, `CapabilityBoundingSet=` limited to what scores, moves and `cpu.idle` need; the plan lists each against the host's version) |
| `cedegrid.slice` | root-level slice for ordinary work where N4 places it there; weight 100, or under N4 C a weight the broker sets from the count of busy accounts |
| `cedegrid_idle.slice` | root-level slice with `CPUWeight=idle` where systemd accepts it, else `cpu.idle` written by the broker (research R1) |
| the `cedegrid` group | created through `systemd-sysusers` |
| `/etc/cedegrid/` | root-owned directory for the policy, mode 0755; ships with no policy file, so defaults apply |
| the binary | at a root-owned fixed path, not writable by users |

Nothing else changes: no PAM, login, SSH, other unit, sysctl or kernel module setting (FR-034).
Purge stops and removes every unit carrying the broker's marker, then the slices, the socket, the
service and the group, and leaves `/etc/cedegrid/` only if the administrator added files there.

## Built by the broker for a caller with user ID `U` and primary group `G`

| Property | Ordinary unit | Idle unit |
|---|---|---|
| name | `cedegrid-uU.service` | `cedegrid-idle-uU.service` |
| slice | per N4: `user-U.slice` or `cedegrid.slice` | `cedegrid_idle.slice` |
| `User=`, `Group=` | `U`, `G` | `U`, `G` |
| `Delegate=` | `yes` | `yes` |
| `TasksMax=` and process limits | from the policy | from the policy |
| command | the installed binary's agent subcommand | the installed binary's idle executor subcommand |
| marker | the broker's description prefix and marker property | same |
| `OOMScoreAdjust=` | not set (FR-006) | not set |
| `ManagedOOMSwap=`, `ManagedOOMMemoryPressure=` | not set (R11) | not set |
| `PAMName=` | per N3 (recommended: not set) | per N3 |
| `RuntimeDirectory=` | `cedegrid/uU`, mode 0700, owned by `U` (the agent's socket for the idle executor; R18) | not set; it uses the ordinary unit's |
| `Restart=` | `on-failure`, within systemd's default start-rate limit (*provisional*); the agent reconciles at start as 001's does | same |

No property comes from a request. The command takes no argument from the caller; the agent reads
its configuration as the user, after it starts with the user's rights.
