# Data Model: System Mode

The broker keeps no store. Every entity below is rebuilt from the kernel and systemd at each use,
except the in-memory rate limits and the last score written.

## Caller

| Field | Source | Rule |
|---|---|---|
| `uid` | `SO_PEERCRED` | never from a request (FR-003); 0 and IDs below `UID_MIN` refused (FR-004) |
| `gid` | the account's primary group in the passwd database | used for the units' `Group=` |
| `member` | the group database at request time | must be true (FR-004) |
| `peer_pidfd` | `SO_PEERPIDFD` where present (Linux 6.5) | logging only |

## Broker request

| Field | Type | Rule |
|---|---|---|
| `protocol` | integer | must equal the broker's (FR-035) |
| `kind` | `start`, `stop`, `status`, `move` | closed set (FR-005) |
| `mode` | a 001 mode name | `start` and `move` only; must be allowed by the policy (FR-010) |
| `job` | a job identifier the agent gave the run | `move` only; resolved inside the caller's own units only |

Any other field, or a field named like an identity (user, uid, unit, path, cgroup, mount, device,
capability, exec), refuses the whole request (FR-003). The size limit is 64 KiB, and the read
timeout is in the contract.

## Own unit

| Field | Value |
|---|---|
| `name` | `cedegrid-u<uid>.service` or `cedegrid-idle-u<uid>.service` |
| `kind` | ordinary or idle |
| `slice` | ordinary: per N4 (`user-<uid>.slice` or `cedegrid.slice`); idle: `cedegrid-idle.slice` |
| `user`, `group` | the caller's `uid`, `gid` |
| `delegate` | yes |
| `tasks_max`, process limits | from the policy (FR-010, N3) |
| `command` | the fixed subcommand at the installed path: the agent (ordinary) or the idle executor (idle) |
| `marker` | the broker's fixed description prefix and marker property (R8) |
| `state` | systemd's active state |

State transitions: absent → starting → active → stopping → absent. `start` on an active unit is
a no-op that returns its status. The broker never starts a unit whose name exists without the
marker; it refuses instead.

## Job move

| Field | Rule |
|---|---|
| `source_leaf` | inside the caller's source unit's cgroup |
| `destination_leaf` | an empty leaf inside the caller's other unit's cgroup, made by the agent |
| `members` | read from the frozen source leaf; each has the caller's user ID; a pidfd is open on each |
| `result` | moved, or refused with the source thawed |

## Administrator policy

See [contracts/admin-policy.md](contracts/admin-policy.md). It is loaded at each request from a
root-owned file not writable by others (FR-010).

## User memory record

| Field | Rule |
|---|---|
| `uid` | a principal by 001 FR-059 with the policy's thresholds |
| `memory` | per-account cgroup counters, else per-process sums by user ID (R10) |
| `state` | readable or unknown (Principle III) |
| `fair_share` | 001's max-min memory share among the principals |
| `over_share` | whether `memory > fair_share` |

## Score record

| Field | Rule |
|---|---|
| `process` | PID, start time, `/proc/<pid>` directory handle, pidfd |
| `unit` | the cedegrid unit the process is in |
| `in_share_pages` | the part of the user's share given to this task, oldest task first |
| `value` | `−round(1000 × in_share_pages ÷ total_pages)` clamped to −999 when lowered; 1000 when marked (FR-025, D2); 0 otherwise |
| `written` | the last value written; a write happens only when `value` differs |

When a value is lowered depends on N1: A at all times; B only while another principal is over
its share; C never.
