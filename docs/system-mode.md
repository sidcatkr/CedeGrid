# System mode

System mode lets members of the `cedegrid` group run CedeGrid at the Full control level on hosts
that do not delegate a cgroup to each user. It is an optional package, `cedegrid-system`,
installed by an administrator. Without it, CedeGrid runs unprivileged and nothing here applies.

## What runs as root

One process, the broker (`cedegrid-broker.service`), started on demand by
`cedegrid-broker.socket`. It accepts requests on `/run/cedegrid/broker.sock` (owner root, group
`cedegrid`, mode 0660) and does only the following, for the calling user:

- starts and stops two transient units, `cedegrid-u<uid>.service` and
  `cedegrid-idle-u<uid>.service`, which run the installed `cedegrid` binary as that user with a
  delegated cgroup;
- moves one of the caller's jobs between those two units when its mode changes to or from Yield
  First;
- writes `oom_score_adj` for processes inside those units.

The caller is identified by the socket's peer credentials. A request that names a user, unit,
path, cgroup, program, mount, device or capability is refused. The broker writes no other
files, changes no PAM, login, sysctl or kernel module setting, and logs every request and every
score it writes to the journal.

Group membership is read from the host's group database at each request. Removing a user from
the group refuses their next request; units already running continue until their work ends, and
an administrator can stop them with `systemctl stop`.

## Install

```sh
sudo apt install ./cedegrid_<version>_amd64.deb ./cedegrid-system_<version>_amd64.deb
sudo usermod -aG cedegrid <user>      # then the user logs in again
```

The package adds the broker's socket and service, `cedegrid.slice` and `cedegrid-idle.slice`,
the `cedegrid` group and an empty `/etc/cedegrid/`.

A member starts their units with:

```sh
cedegrid system start
cedegrid system status
cedegrid system stop
```

Without the package the command reports `{"system":false,...}` and exits 0. With
`--require-system` it fails instead.

## Policy

`/etc/cedegrid/policy.toml` is optional. The file and its directory must be owned by root and
not writable by group or others; otherwise the broker refuses every request until it is fixed.

```toml
policy_version = 1

[modes]
allowed = ["yield_first", "neutral", "performance"]

[limits]
tasks_per_user = 64
tasks_max = 4096
# open_files = 65536
# processes = 0
# locked_memory_mib = 0

[principals]
uid_min = 1000
principal_min_percent = 1

[units]
pam_session = "none"
ordinary_slice = "auto"
```

| Key | Default | Meaning |
|---|---|---|
| `modes.allowed` | all modes except `aggressive` | modes a member may run; checked by the broker and by the agent |
| `limits.tasks_per_user` | 64 | CedeGrid tasks one user may run at once |
| `limits.tasks_max` | 4096 | `TasksMax=` on each unit |
| `limits.open_files`, `processes`, `locked_memory_mib` | systemd defaults | `LimitNOFILE=`, `LimitNPROC=`, `LimitMEMLOCK=` when `pam_session = "none"` |
| `principals.uid_min` | `UID_MIN` from `/etc/login.defs`, else 1000 | lowest user ID counted as a user for memory shares |
| `principals.principal_min_percent` | 1 | memory share of the host above which a user counts |
| `units.pam_session` | `none` | `none`: no PAM session; `cedegrid`: `PAMName=cedegrid`, a service you provide with only `pam_limits`; `login`: the host's `login` stack |
| `units.ordinary_slice` | `auto` | `auto`: `user-<uid>.slice` when `user.slice` passes the cpu controller to account slices, else `cedegrid.slice`; `user` or `cedegrid` forces one |

The allowed modes are a CedeGrid setting, not a security boundary. The limits the kernel enforces
are the units' properties.

## CPU placement

With `ordinary_slice = "auto"` on a host that splits CPU by account, a member's ordinary unit
runs inside their own `user-<uid>.slice`, so each account's CPU share is unchanged. In
`cedegrid.slice` (no per-account split, or `ordinary_slice = "cedegrid"`), System mode work
competes as one group beside `user.slice` and `system.slice`, which changes how CPU is split
between those groups; `cedegrid doctor` reports which applies.

Yield First work runs in `cedegrid-idle.slice`, which has `cpu.idle` set. It gets CPU time only
when its neighbours leave some free. `cpu.idle` does not cover SMT siblings, caches or memory
bandwidth.

## Memory and OOM scores

The broker reads each user's memory from `/proc` and computes each user's fair share as the
agent does. While another user is above their fair share, it lowers the OOM score of a member's
processes for the part of their memory within the member's share, down to -999 at most. Tasks
wholly above the share are set to 1000 in Yield First and Neutral. When a user's memory cannot be
read, nothing is lowered, and lowered scores return to 0 after 10 seconds.

A score the broker writes is also the lowest value the user can set afterwards. Processes outside
the two units are never scored.

One limit remains: when a small process outside CedeGrid causes an OOM, the kernel can still
kill a larger process.

## Removal

```sh
sudo apt purge cedegrid-system cedegrid
```

Purging stops every unit the broker made, then removes the slices, the socket, the service and
the group. `/etc/cedegrid/` is kept only if it holds files an administrator added.
