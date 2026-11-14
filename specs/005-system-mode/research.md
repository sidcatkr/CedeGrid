# Research: System Mode

**Status**: Phase 0 of the plan. No host was reachable. Each finding comes from the repository, the
brief, or documented kernel and systemd behavior, and says which. A figure is *measured* only
where the brief or `docs/validation.md` gives it; everything else is reasoned. Where a finding
needs a run, the run is listed under "Runs for the other session" with no result estimated.

## R1. `CPUWeight=idle` on systemd 249 (open point 1)

- **What is documented.** The current `systemd.resource-control` manual (systemd 262) says
  `CPUWeight=` takes a weight or the string `idle`, which "corresponds to the `cpu.idle` cgroup
  attribute" and gives the cgroup CPU "only when there are no processes not marked in this way to
  execute in this cgroup or its siblings". The manual's only version note there is the one for
  `CPUWeight=` itself (232). It gives none for the `idle` value. So whether systemd 249 accepts
  it is **not settled by the documentation**.
- **The kernel side.** `cpu.idle` came with "sched: Cgroup SCHED_IDLE support" (commit
  `304000390f88`), which is in 5.15. So the kernel floor has it.
- **How it is settled without touching the shared host.** Ubuntu 22.04, the repository's CI image
  (`ubuntu-22.04`), ships systemd 249. The container test (R17) starts `cedegrid-idle.slice` with
  `CPUWeight=idle` and reads `cpu.idle` back.
- **Decision.** At install and at each broker start, the broker's setup checks
  `/sys/fs/cgroup/cedegrid-idle.slice/cpu.idle`:
  1. `CPUWeight=idle` in the slice file, where systemd accepts it and `cpu.idle` reads 1;
  2. else the broker writes `cpu.idle` = 1 itself, as root, and reads it back. That write is the
     only cgroup write the broker makes outside a move (FR-009). systemd does not manage
     `cpu.idle` on that version, so it does not undo it, which the container test confirms;
  3. else (no `cpu.idle`, a kernel below 5.15 that the floor excludes): the system package
     refuses to install.
  `CPUWeight=1` as a stand-in is rejected: weight 1 against 100 still takes about 1% of a
  contended CPU (reasoned from the weights) and does not give way at once, so Yield First's
  margin could not go to 0%. `doctor` says which path applies.
- **Alternatives considered.** Writing `cpu.idle` always (simpler, but fights systemd where it
  manages the attribute); `CPUWeight=1` (above).

## R2. Where ordinary work sits, and `cedegrid.slice`'s weight (open point 2)

The user's design puts `cedegrid.slice` directly below the root, beside `system.slice` and
`user.slice`. A root-level group changes how the kernel splits CPU between whole groups, so
the baseline rule (FR-019) has to be checked on each layout. The figures below are reasoned from
the weights. Every busy group is taken to want more than its share.

**The test host** (CPU split by account below `user.slice`, weight 100 each; brief):

| Busy groups | Plain Linux, own account's share | Root-level `cedegrid.slice`, weight 100 | Effect on others |
|---|---|---|---|
| own + the second account, `system.slice` idle | 1/2 | 1/2 | none |
| own + the second account + `system.slice` | 1/4 (half of `user.slice`'s 1/2) | 1/3 | `system.slice` 1/2 → 1/3; the second account 1/4 → 1/3 |
| own + four other accounts | 1/5 | 1/2 | each other account 1/5 → 1/8 |

So a root-level slice at a fixed weight gives own work more than the baseline as soon as more
than one other account is busy. A weight that follows the counts (`100 × k ÷ N`, `k` CedeGrid
users and `N` other busy accounts) fixes the split against `user.slice`, but not against
`system.slice`, whose share still falls from 1/2. Holding both needs the broker to change
`user.slice`'s weight, which FR-016 forbids.

**The alternative the plan found: the ordinary unit in the caller's own account slice.** Where the
host splits CPU by account, systemd can place `cedegrid-u<uid>.service` in `user-<uid>.slice`,
beside the caller's login sessions. The account's total share is then what it is without
CedeGrid, by construction, and nobody else's share changes. Only the split inside the account
changes, between the login sessions and the unit. Yield First stays in the root-level
`cedegrid-idle.slice`, which `cpu.idle` needs. Open for the host run: whether `systemd-logind`
stops `user-<uid>.slice`, with the unit in it, when the user's last session ends on a host
without lingering. The documentation read does not settle it. If it does, this placement fails
003 FR-015 (a run continues after the SSH session ends).

**The shared host** (SSH logins in `system.slice/ssh.service`, split by thread there; brief).
No placement CedeGrid may make reproduces the baseline. Every SSH user's threads compete one by
one inside one group, and moving own work out of that group at all makes it a group of its own.

| Own work, against other users' threads (reasoned) | Plain Linux, own share | Root-level `cedegrid.slice`, weight 100, `user.slice` idle |
|---|---|---|
| 8 own threads, 2 other threads | about 8/10 | about 1/2 |
| 1 own thread, 10 other threads | about 1/11 | about 1/2 |

A weight that followed thread counts would need the broker to read every user's runnable threads
and rewrite the weight continually. It would still be approximate, and it would make root code
follow other users' activity. The shared host is outside this feature's validation (FR-031).

**Decision for the plan (pending the user, decision N4 in the report):**

- **N4 A: the ordinary unit in `user-<uid>.slice` where the host has per-account CPU slices**
  (recommended, if the host run shows it survives logout). Otherwise, root-level
  `cedegrid.slice` at weight 100, and `doctor` says the split changed and how.
- **N4 B: always root-level `cedegrid.slice` (the design as decided)**, with the weight chosen by
  measurement. On the test host that gives own work more than the baseline whenever more than one
  other account is busy.
- **N4 C: a weight that follows the counts**, with the limits above.

Rationale for N4 A: it is the only one that meets FR-019 on the test host by construction, with no
root code following other users. Alternatives considered: B and C above.

## R3. What `cpu.idle` does not cover (open point 3)

- **Documented.** `cpu.idle` gives a group the idle class's treatment on each CPU's run queue. A
  busy sibling group always runs first on that CPU, and the idle group's weight is the minimum.
  It does not reach the other hardware thread of the same core (SMT), the shared caches, or
  memory bandwidth. Own Yield First work may still run on the SMT sibling of a CPU where the
  second account runs, and slow it through the shared core (reasoned).
- **Decision.** The Yield First CPU margin in System mode starts at today's value and is lowered
  step by step in the host run R3-run. It stops at the lowest value at which the second account's
  throughput and p99 stay within the spread of its runs with no own work, for CPU-bound and for
  cache-sensitive work (SC-005). The step that measures it decides whether 0% holds. If it does
  not, the remaining options for the user are a margin above 0%, or keeping SMT siblings of a
  CPU the second account uses free through affinity on own threads, under 003 FR-043's rules.
  That second option is a new lever and needs its own decision.
- **Alternatives considered.** Assuming 0% without measuring (rejected: the brief says measure).

## R4. A PAM session for the units (open point 4, decision N3)

- **Documented.** `PAMName=` makes systemd open a PAM session for the unit's process, as root,
  through the named PAM service. Without it, no PAM module runs, so login limits configured
  through PAM (for example `pam_limits`) do not apply to System mode work.
- **Decision.** Pending N3; recommended N3 A: no PAM session, with the administrator policy able
  to set the units' process limits (`LimitNOFILE=`, `LimitNPROC=`, `LimitMEMLOCK=`, alongside
  `TasksMax=`). `doctor` says that login limits do not apply. Under N3 B, the package ships one
  PAM service file with only the limits module. Under N3 C, the units use the host's login stack.

## R5. A polkit rule instead of a broker (open point 5)

- **Documented.** A polkit rule can allow a group to manage systemd units. systemd passes the unit
  name and the verb to polkit, but not the properties of a transient unit. A rule therefore
  cannot stop a group member from starting a transient unit with `User=root` and any command,
  which is root.
- **Not checked.** Older polkit releases read `.pkla` files, not JavaScript rules, and cannot match
  on unit names at all; the shared host's polkit was not read.
- **Decision.** No polkit path. The broker builds every property itself (FR-006). Rationale: the
  security goal (the group must not become root) cannot be met by a rule that cannot see
  properties.

## R6. One shared agent for all users (open point 6)

- **Decision.** Phase 1 runs one agent per user inside that user's ordinary unit (003 FR-002). A
  shared agent would put several users' policy in one process and is out of scope. The overhead
  of one agent per user is measured instead (SC-012; deferred findings EFF-6 to EFF-10, R16).

## R7. Who is calling

- **Documented.** `SO_PEERCRED` on a unix stream socket gives the peer's PID, user ID and group ID
  as they were at `connect()`. Linux 6.5 adds `SO_PEERPIDFD`, a pidfd of the peer, so the PID
  cannot be reused under the broker. The socket file's mode 0660 and group `cedegrid` already let
  only processes holding that group connect.
- **Decision.** The user ID comes from `SO_PEERCRED` only. Group membership is read from the
  host's group database at each request (so a removal takes effect at once), not from the peer's
  credentials. `SO_PEERPIDFD` is used where present (newest kernel feature, detected at run time)
  for the log line; the broker's decisions never depend on the peer's PID. User ID 0 and IDs below
  `UID_MIN` from `/etc/login.defs` (1000 if unreadable) are refused.

## R8. How the broker talks to systemd

- **Options.** A D-Bus client crate (a new dependency, and a large one); linking `libsystemd`
  (a C dependency at build time); or running `systemd-run` and `systemctl` at their fixed paths
  with an argument vector the broker builds, no shell, and a timeout.
- **Decision.** Fixed-path `systemd-run` and `systemctl`, with argument vectors built from the
  caller's user ID and the policy only. This adds no dependency (CLAUDE.md) and keeps the
  boundary small enough to review line by line. Units are transient, so the broker writes no unit
  files (FR-009). Its marker is a fixed `Description=` prefix together with a property only the
  broker sets, checked with `systemctl show` before every stop or move (FR-007).
- **Alternatives considered.** The two client libraries above, rejected for dependency size and
  for review surface.

## R9. Two units, one agent, and moving a job between them

- **The constraint.** 003 FR-002 allows one agent per user per host. A process in the ordinary
  unit cannot start a process inside the idle unit: a move or a `CLONE_INTO_CGROUP` birth needs
  write access to the two cgroups' common ancestor, the root cgroup (cgroup v2 "delegation
  containment", documented).
- **Decision.** The idle unit runs a fixed CedeGrid executor subcommand, with no policy of its
  own. The user's agent drives it over a socket inside the user's runtime directory, and it starts
  supervisors in its own cgroup. The agent stays the one policy owner.
- **Moving a job (FR-018, "verify, then put it in").** The caller asks for a move. The broker:
  1. resolves the job's leaf only inside the caller's source unit's cgroup;
  2. freezes it, as root;
  3. reads its members, and checks that each one runs with the caller's user ID and that the
     leaf holds nothing else;
  4. opens a pidfd on each;
  5. moves them one by one into an empty leaf that the agent made beforehand in the destination
     unit, which the broker checks is inside the caller's other unit;
  6. thaws.
  A frozen leaf cannot fork, so no process starts between the check and the move. Anything that
  fails these checks refuses the move and thaws the source. Every process in a unit's cgroup can
  only have been forked inside that unit, or moved there by root. So membership of the caller's
  unit plus the caller's user ID is the broker's own proof that a process is the caller's
  CedeGrid work. It trusts no list from the agent.

## R10. The OOM loop

- **Counters.**
  - On the test host, each account's slice has memory counters.
  - On a host without per-account slices, the broker sums per-process resident and swapped
    memory by user ID from `/proc`, read-only. That is what root can read for every process.
  - The measure is the one 001 uses for RAM. The plan maps it to the same counters, and a unit
    test pins the mapping.
- **Principals and the fair share.** 001 FR-059's rules, with `uid_min`, `principal_min_percent`
  and the windows from the administrator policy (clarify session, 2026-11-14).
- **Scores** (`mm/oom_kill.c`, documented in the kernel source).
  - The kernel's score is a task's resident, swap and page-table pages, plus
    `oom_score_adj × total pages ÷ 1000`.
  - To leave only the part above the share, the broker sets each in-share task's value to
    `−round(1000 × in-share pages ÷ total pages)`, clamped at −999.
  - The user's in-share amount is handed out oldest task first. The broker takes no order or
    amount from the agent.
  - A task wholly above the share gets 1000 where FR-025 says.
  - When N1 B is chosen, lowering happens only while another principal's memory is above that
    principal's own share.
- **Kernel rules relied on** (`fs/proc/base.c`; SC-008 and the container test confirm them):
  - a writer with `CAP_SYS_RESOURCE` also sets `oom_score_adj_min` to the value it writes, and
    an unprivileged write below it fails;
  - both values are inherited at fork, so a child carries its parent's value until the next
    pass.
- **Writing safely.** The broker opens `/proc/<pid>` as a directory and a pidfd on the same PID,
  and checks that the start time read through the directory is the one it scored. It then writes
  `oom_score_adj` through that directory, so a reused PID gets nothing.
- **One writer.** In System mode, the user's agent stops writing OOM scores (001's marks). The
  broker is the only writer, so two writers cannot fight, and the user cannot undo a protection
  by accident.
- **Pace.** One pass per second (reasoned bound for SC-012). A value is written only when it
  changes.
- **Unknown.** A user whose counters cannot be read gets no lowering. After 10 s of unknown
  (a *provisional* window), their lowered scores go back to 0 (FR-026).

## R11. The userspace OOM killer on the test host

- **Measured** (brief, F4): on the test host `systemd-oomd` is active and took nothing in the OOM
  shape; the kernel killed the second account's allocator.
- **Documented.** `systemd-oomd` acts on cgroups below units that set `ManagedOOMSwap=` or
  `ManagedOOMMemoryPressure=` (`ManagedOOMSwap=` since systemd 247) and kills a whole cgroup.
  Whether root-level `cedegrid.slice` falls under a setting on the root slice depends on the
  host's defaults, which were not read.
- **Decision.** The broker sets neither property on its slices or units, and changes no other
  unit's. `doctor` reads and shows whether the cedegrid slices are within `systemd-oomd`'s reach.
  The OOM host run repeats F4 with `systemd-oomd` active, as it was.

## R12. Caps inside a unit, and the agent's delegated root

- **The delegated root.** Inside a unit, the agent reads its own cgroup from `/proc/self/cgroup`.
  It accepts it as `delegated_root` only if three things hold: the path is
  `/cedegrid.slice/cedegrid-u<uid>.service` (or the N4 A placement, or the idle unit for the
  executor); its `cgroup.procs` and `cgroup.subtree_control` belong to the agent's user ID; and
  the broker's marker reads back through `systemctl show`. This keeps Principle V's rule that
  cgroup control stays inside an explicitly delegated subtree. The explicit grant is the unit's
  `Delegate=yes`.
- **Caps (D3).** System mode gives every member the Full level, so item 057 Part C's Full design
  is what they get. Case A: `cpu.max` behind the fork gate if approved, else whole-task holds.
  Case B: 001's Full steps as today.
- **The joiner case.** Inside a unit, a process that joins a job's leaf can only be another
  process of the same user already inside the same unit, since the user cannot move anything in
  from outside (R9). The 50 ms lift-on-sight of Part C still applies.

## R13. Every command that needs root on the test host

The user runs these themselves; the work session never uses `sudo` (FR-030). `<you>` is the
user's own account and `<second>` the second account; `<v>` is the release version.

1. Install both packages from the release files:
   `sudo apt install ./cedegrid_<v>_amd64.deb ./cedegrid-system_<v>_amd64.deb`
2. Add both accounts to the group (the second only for the two-member security tests):
   `sudo usermod -aG cedegrid <you>` and `sudo usermod -aG cedegrid <second>`. Then log in again
   (no root needed).
3. Only if a test needs a policy other than the default:
   `sudo install -o root -g root -m 0644 <file> /etc/cedegrid/policy.toml`. The broker reads the
   policy at each request, so nothing restarts.
4. Read the broker's log after a run:
   `sudo journalctl -u cedegrid-broker.service --since "<start time>"`. This is read-only.
5. For the removed-member test (N2): `sudo gpasswd -d <second> cedegrid`.
6. Remove everything at the end: `sudo apt purge cedegrid-system cedegrid`.

Nothing else needs root: the slices' state, `cpu.idle`, unit properties and scores are readable
by the user (`systemctl show`, `/sys/fs/cgroup`, `/proc/<pid>/oom_score_adj`). The apt repository
path (adding its key and source list) also needs root. It is not used on the test host; the
release `.deb` files are.

## R14. What the pre-release security review covers (FR-032)

Every file that runs as root, or decides what root does:

- `src/broker.rs` and `src/broker/`: the listener, peer credentials, limits and timeouts; the
  closed request set and its parser; the unit property builder and the systemd calls; the
  administrator policy loader; the job move; the OOM loop and its score function.
- `src/main.rs`: the broker's hidden dispatch, which runs before argument parsing, and the root
  refusal it is the exception to (003 FR-082).
- `src/kernel.rs`: the pidfd and `/proc` directory helpers the broker uses as root.
- The packaging: the broker's service and socket units, the two slice units, the group creation,
  and the maintainer scripts run at install, upgrade and purge.
- The release workflow that builds and signs the `.deb` files and the apt repository.

The user-side files that face the kernel also go in, as decided for 001: `src/cgroup.rs`
(delegated root detection in a unit) and, under D3 case A, item 057 Part C's list (the fork-gate
module, `src/supervision/unix.rs`, `src/supervision/family.rs`). Before release, separate from
the development loop.

## R15. The dependencies, both cases

| Id | Case A | Case B | What the plan does now |
|---|---|---|---|
| D1 057 Part B | no give-back in Performance inside a unit | Performance holds the fair share inside a unit | Nothing in the broker depends on it; the agent's mode code is 001's either way |
| D2 057 decision 1 | broker marks over-share at 1000 in Yield First and Neutral only | in every mode | the score function takes the mode rule as an input, with a test for each case |
| D3 057 Part C | Part C's Full design inside units | 001's Full steps | no broker code depends on it; the gate's tasks are 057's, not 005's |
| D4 Principle V amendment | build | build nothing | every implementation task waits on D4 (tasks.md, Phase 0) |

A dependency the brief did not list, found here: **Principle II's OOM bullet.** Today it says own
work's OOM score "MUST NOT be set below" the baseline (7.0.0, and also 057's draft). Lowering an
in-share score in System mode contradicts that sentence, so Principle V alone cannot allow it.
The Principle V draft therefore proposes a one-clause cross-reference in Principle II ("except in
System mode, as Principle V allows"). The brief said Principle II stays as it is, so this needs
the user's approval (decision N5 in the report).

## R16. Findings deferred to this feature (FR-038)

| Finding | Decision |
|---|---|
| 029 Part 0f (score each System mode task only by what exceeds its share) | Done by FR-024 and R10 |
| EFF-6 to EFF-10 (per-task `/proc` reads, static discovery, statement cache, quadratic lookups, heartbeat reopen) | In scope. One agent per user multiplies them. SC-012's host run measures the agents' total CPU at the user count the plan tests, against 001 SC-014's 1% bound. Item 049's *measured* 111 m at 128 tasks on 14 CPUs is the starting point. The worst offenders are fixed only if the bound fails |
| SUP-9 (a busy journal blocks the supervisor in an off phase) | In scope: many users' agents share the host's journal. Bounded lock wait, with a test |
| AGT-15 (a failed Authorize send is not resent) | In scope: a unit restart makes it likelier. Resend until delivered, as AGT-16 was fixed |
| AGT-17 (replay mode stop without `shutdown()`) | Moved to 002 (agent integration): replay is not used in System mode |

## R17. Tests that need root

- **Decision.** A CI job on `ubuntu-22.04` (systemd 249, matching the shared host's version) builds
  the packages and runs a container with systemd as PID 1. Inside it: install, the security
  suite (FR-029), R1's `cpu.idle` check, the move test, the score tests (SC-008), and purge
  (SC-009). The tests are `#[ignore]` subprocess fixtures behind an opt-in variable
  (`CEDEGRID_TEST_SYSTEM_CONTAINER`), like the existing hardware gates, and run only in that job.
  No server is touched (FR-028).

## Runs for the other session (test host, after the user's R13 steps 1 and 2)

| Run | What it decides | Shape |
|---|---|---|
| R2-run | N4 and FR-019 | F1 and F2 of item 057 in plain Linux and in System mode with each placement (N4 A, N4 B); at least 3 runs each way; own CPU and the second account's rate and p99. Also: log out the last session and check whether the unit in `user-<uid>.slice` survives |
| R3-run | the Yield First margin (FR-020, SC-004, SC-005) | Yield First own work against the second account's CPU-bound and cache-sensitive work, and the second account alone; margin from today's down to 0% |
| R10-run | SC-006, SC-007, SC-008 | item 054's OOM shape with own work in share, then over share; `systemd-oomd` left as it is; plus a user write below a root-written score |
| R16-run | SC-012 and EFF-6 to EFF-10 | on the test host, the broker's and both accounts' agents' CPU and memory, idle and busy. The 50-user bound is measured in the container (R17), where accounts can be made without touching a host |
