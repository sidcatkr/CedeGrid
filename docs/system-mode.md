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
- moves one of the caller's jobs between those two units on the caller's request (used by a
  later version, in which Yield First work runs in the idle unit);
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

The package adds the broker's socket and service, `cedegrid.slice` and `cedegrid_idle.slice`,
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
unprotected = false
unprotected_cpu_weight = 400
plain_priority = false
plain_priority_cpu_weight = 200

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
| `modes.unprotected` | `false` | whether a member may request unprotected for a run; checked by the agent in the unit, separately from `modes.allowed` |
| `modes.plain_priority` | `false` | whether members may choose priority over plain users in `cedegrid setup`; each member's own choice is needed too |
| `modes.plain_priority_cpu_weight` | 200 | the `CPUWeight=` (101 to 10000) the broker gives a member's account slice under priority over plain users, while the member's Performance or Aggressive work runs and every competing CedeGrid member runs at that weight or more; it ends as the unprotected raise does |
| `modes.unprotected_cpu_weight` | 400 | the `CPUWeight=` (101 to 10000) the broker gives an unprotected member's account slice while the run is active, where the host splits CPU by account and the member's ordinary unit sits in that slice; never set over a weight the broker did not set; reset when the run ends, after 30 s without renewal, within 5 s of this key turning off or the member leaving the group, when the units stop, and when the broker stops or restarts |
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

In this version all work, Yield First included, runs in the ordinary unit, and Yield First yields
with the same steps as on a delegated Full host. `cedegrid_idle.slice` (with `cpu.idle` set) and
the idle unit are installed and started for a later version that runs Yield First work there;
`cpu.idle` gives way on CPU time only, not on SMT siblings, caches or memory bandwidth.

## Priority over plain users

With `modes.plain_priority = true`, each member chooses in `cedegrid setup` whether their CedeGrid
work runs above users who run no CedeGrid work. Setup asks on a terminal with Yes preselected;
`--priority-over-plain` and `--no-priority-over-plain` set it unattended, and without either an
unattended setup leaves it off. Running setup again changes it. The choice is
`contention.priority_over_plain` in the member's configuration.

While the choice is on and the member's Performance or Aggressive CPU work runs, the agent asks
the broker to set the member's account slice to `modes.plain_priority_cpu_weight`. It does so
only while every other account with CedeGrid work on the CPU runs at that weight or more, read
from each account slice's `cpu.weight`; a slice that cannot be read counts as not raised. When
another account falls below, the raise ends within two monitor intervals; an account that chose
the option too sees the raise in that time and raises as well, so both keep it. The broker ends
the raise as it ends the unprotected one. Where unprotected also applies, the higher weight is
asked for.

A plain user's CPU share then shrinks while the member's work runs: with one plain user at the
default weight of 100 and the member at 200, the member gets about two thirds of a contended CPU.
Where the member's tasks run on more CPUs than the plain user's, the kernel spreads the
slice's weight over all of them, so the member gets less of the plain user's CPUs: about two
fifths in the F2c gate, where by hand it gets a fifth.
Between CedeGrid users the first-comer rule decides, as without the option: the later user's new
work waits at admission rather than take the first-comer below its demand or its fair share. An account whose unit runs work counts as a
CedeGrid user even where its processes are hidden from other users.

`cedegrid doctor` shows whether the option is chosen, permitted and in force, and why it is not.

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
