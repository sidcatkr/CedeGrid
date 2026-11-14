# Feature Specification: System Mode, a Root Broker for Full Control on Every Group Member's Work

**Feature Branch**: specified on `specs-057-059`, a drafts branch cut from `001-contention-modes`.
The spec directory is independent of the branch. Implementation starts on its own branch,
`005-system-mode`, once the user has approved this specification and the Principle V amendment.
001 is merged only after this feature is built and verified on the test host (user, 2026-11-14).

**Created**: 2026-11-14

**Status**: Draft

**Input**: Maintainer feature description: an optional, administrator-installed root broker that
gives every member of the `cedegrid` group the Full level (a delegated cgroup v2 subtree) on a
host where users otherwise have no delegation, without making the group equivalent to root. The
decisions are the user's plan of 2026-10-27 to 2026-10-29 and `specs/001-contention-modes/follow-ups.md`
("Feature 005, System mode"), reconciled with the rules the user set on 2026-11-13.

This feature builds on [001-contention-modes](../001-contention-modes/spec.md): the Full level and
its delegated root, the modes and their targets (FR-020 to FR-022), the floors (FR-023), the
save-progress signal (FR-075), OOM requeue (FR-076) and Principle I's ownership checks. It keeps
[003-everyday-cli](../003-everyday-cli/spec.md)'s rule that every `cedegrid` command run as root
refuses, with this broker as the one exception (FR-082), and its `run`, `doctor` and control
level reporting. It needs a MAJOR amendment of constitution Principle V
([drafts/amendment-principle-v.md](drafts/amendment-principle-v.md)); Principles I and II stay as
they are.

## Dependencies on decisions not yet made

This specification does not build on anything the user has not approved. Where it depends on such
a decision, both cases are written and the requirement names the dependency.

| Id | Decision | Where | Case A (approved) | Case B (not approved) |
|---|---|---|---|---|
| D1 | Item 057 Part B: Performance gives nothing back beyond plain Linux | `specs/001-contention-modes/drafts/057-amendment-draft.md` | Inside a unit, Performance and Aggressive take no running step for others; System mode adds none | Inside a unit, Performance holds the fair share as 001 does today |
| D2 | Item 057 decision 1, option B: at a floor breach others caused, Performance gives no memory back and marks no own task | `057-report.md`, decision 1 | The broker marks over-share tasks at 1000 in Yield First and Neutral only | Today's rule (option A) stays: the broker marks over-share tasks at 1000 in every mode |
| D3 | Item 057 Part C: how a cap is applied | `057-cap-design.md` | Inside a unit, caps follow the approved design (recommended: `cpu.max` behind a fork gate, else whole-task holds) | Inside a unit, caps are 001's Full steps today |
| D4 | The Principle V amendment | `drafts/amendment-principle-v.md` | This feature may be built | Nothing in this feature is built |

## Clarifications

### Session 2026-11-14

Answered only from the brief and the repository; the user was not asked in this session. Where
neither answers, the question is listed under "Decisions needed" and the spec's behavior for it
is pending the user's choice.

- Q: Should the broker lower the OOM score of a user's in-share CedeGrid tasks at all times, or
  only while another user is over that user's own memory fair share? → A: Pending the user's
  choice (decision N1). The brief gives "at all times" as the plan of 2026-10-27 to 10-29; the
  rule of 2026-11-13 (only harm CedeGrid adds counts) may narrow it.
- Q: When the administrator removes a user from the `cedegrid` group, what happens to that user's
  running System mode work? → A: Pending the user's choice (decision N2).
- Q: Should the broker's units open a PAM session, so that the host's login limits apply to
  System mode work? → A: Pending the user's choice (decision N3). The brief fixes only that no
  existing PAM or login setting changes.
- Q: Which CedeGrid agent runs inside a user's ordinary unit: only the local agent `cedegrid run`
  starts, or also an agent in cluster mode? → A: The user's one agent per host, in local or
  cluster mode, as 003 FR-002 already requires (`run` reuses an agent in cluster mode), and one
  agent per user in phase 1 (brief, open point 6).
- Q: Which users count when the broker computes each user's memory fair share? → A: 001 FR-059's
  principals: user IDs at or above `uid_min` whose usage has stayed at or above
  `principal_min_percent`; lower user IDs are background demand subtracted from capacity. The
  broker takes these thresholds from the administrator policy, never from a user's configuration,
  since it trusts no figure from a user (brief).

**Decisions needed (pending the user's choice)**

- **N1. When in-share OOM scores are lowered.**
  - A: at all times, as the plan of 10-29 says. Own in-share tasks are protected in every OOM.
    Against plain Linux, another user's process can then be killed in place of an own task even
    when nobody is over their share, which the rule of 11-13 counts as harm CedeGrid adds.
  - B: only while some other user's memory is above that user's fair share, so that an OOM would
    not be own work's fault; otherwise the kernel's default score, as in plain Linux.
    *Recommended*: it protects own work exactly where the OOM principle asks and leaves plain
    Linux's choice otherwise. Cost: when a third user causes the OOM, a bystander's in-share
    process can still be killed in place of an own task, because CedeGrid cannot score the
    bystander's processes (Principle I).
  - C: never lower; only mark over-share tasks at 1000 as 001 does. Own work keeps plain Linux's
    risk, which contradicts the OOM principle of 10-29.
- **N2. A user removed from the group while work runs.**
  - A: new requests are refused at once; running units go on until their work ends and are then
    stopped. *Recommended*: removing access is not an emergency, and stopping running work costs
    the user's job; the administrator can still stop a unit with `systemctl`.
  - B: the broker stops the units at once; their tasks drain and requeue as 001 FR-076 does.
  - C: the administrator policy picks A or B.
- **N3. A PAM session for the units.**
  - A: no PAM session. The administrator policy may set the units' process limits (open files,
    processes, locked memory); login limits from the host's PAM configuration do not apply to
    System mode work, and the docs and `doctor` say so. *Recommended*: it changes no PAM setting,
    as the brief requires, and keeps the limits under the administrator's control.
  - B: a PAM session through a CedeGrid PAM service that runs only the limits module. It adds one
    PAM file and changes no existing one.
  - C: the host's login PAM stack. Its modules are meant for logins, not services, and it changes
    what a unit start runs as root.

## Terms

- **System mode**: the control level a `cedegrid` group member gets where the system package is
  installed. Their work runs in units the broker had systemd start for them, each with a
  delegated cgroup, so they get the Full level.
- **Broker**: the one CedeGrid process that runs as root. It judges who asks, has systemd start and
  stop that user's units, moves that user's own work between that user's two units, and writes
  OOM scores for processes inside those units. It runs no user input.
- **Caller**: the user the broker serves on one connection, as given by the socket's peer
  credentials, never by the request.
- **Own units**: `cedegrid-u<uid>.service` (ordinary work) and `cedegrid-idle-u<uid>.service`
  (Yield First) for one user ID, made by the broker. A unit's cgroup can only gain a process by a
  fork inside it, or by a move made by root.
- **The two slices**: `cedegrid.slice` and `cedegrid-idle.slice`, directly below the root cgroup.
  `cedegrid-idle.slice` has `cpu.idle` set, so it gets CPU time only when its siblings at the root
  leave it.
- **Administrator policy**: root-owned settings in `/etc/cedegrid/`. They give the allowed modes,
  each user's task limit and `TasksMax=`. A user's settings never exceed them.
- **User memory fair share**: each user's share of host RAM beside the other principals with
  memory demand, computed by the broker from kernel counters it reads itself. It uses the max-min
  rule 001 uses for memory and 001 FR-059's principals, with the thresholds from the
  administrator policy.
- **Root-written score**: an `oom_score_adj` value the broker wrote. The kernel then also sets that
  process's `oom_score_adj_min` to it, so the process's own user cannot set a lower value.
- **Baseline**: the same own work started plainly, without CedeGrid, from the user's login
  session on the same host (user, 2026-11-13). Harm counts only where CedeGrid adds it.
- **The test host**, **the shared host**, **the second account**: the hosts and account the user
  named for this work. They are never named in this repository.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A group member gets Full where no delegation exists (Priority: P1)

An administrator has installed the system package and added a user to the `cedegrid` group. The
host gives users no delegated cgroup of their own. The user runs `cedegrid run` as before. The run
goes through the broker, lands in the user's own unit, and runs at the Full level with the user's
rights. `doctor` says "System". On a host without the package, nothing changes.

**Why this priority**: without it there is no feature. It is what makes Full available to users
who cannot get delegation themselves.

**Independent Test**: in a container with systemd as PID 1, install the system package, add a test
account to the group, run a job as that account, and read the unit's properties, the job's cgroup
path and `doctor`. Then remove the package and run the same job.

**Acceptance Scenarios**:

1. **Given** the system package installed and the user in the group, **When** the user starts a
   run, **Then** the run's work is inside `cedegrid-u<uid>.service` under `cedegrid.slice`, runs
   with the user's user and group IDs, uses the Full level, and `doctor` says System.
2. **Given** no system package, **When** the user starts the same run, **Then** it runs at the
   delegated Full level or Tracked exactly as without this feature, and `doctor` says which.
3. **Given** the package installed and the user not in the group, **When** the user starts a run,
   **Then** the broker refuses in one line saying why, and the run uses delegated Full or Tracked.
4. **Given** the socket present but the broker not answering, **When** the user starts a run,
   **Then** the run says so in one line and runs as without the package, unless the user asked to
   require System mode, in which case it refuses.
5. **Given** a user running `sudo cedegrid run …`, **When** the command starts, **Then** it refuses
   as 003 FR-082 says. The broker's own fixed subcommand is the only `cedegrid` command that runs
   as root.

---

### User Story 2 - The group cannot become root (Priority: P1)

A member of the `cedegrid` group, or a program they run, tries to use the broker to act as another
user or as root: by sending another user ID, odd unit names, paths, a huge message, a flood of
requests, or a request to stop another user's unit. Every attempt fails, is logged, and changes
nothing.

**Why this priority**: a broker that lets the group gain root is worse than no feature. The goal
is stated by the user: the group must not become what the docker group is.

**Independent Test**: the security suite, run in the container against the installed broker, as a
member, as a non-member and as a second member.

**Acceptance Scenarios**:

1. **Given** a request that carries a user ID, a unit name, a path, a mount, a device, a capability
   or an executable, **When** the broker reads it, **Then** it refuses the whole request, logs it,
   and starts or changes nothing.
2. **Given** a member asking to stop or move the second account's unit, **When** the broker reads
   it, **Then** it refuses, and the second account's units are unchanged.
3. **Given** a 1 MiB message, or a message that never ends, **When** the broker reads it, **Then** it
   stops reading at the size limit or the timeout, refuses, and keeps answering other users.
4. **Given** one member sending requests as fast as possible, **When** another member sends one
   request, **Then** the other member is answered within the bound of SC-003, and the flooding
   member is limited.
5. **Given** the socket file, **When** its owner and mode are read, **Then** they are root, group
   `cedegrid`, mode 0660, and an account outside the group cannot connect.
6. **Given** any request, **When** the broker starts a unit, **Then** the unit's name, user,
   slice, command and limits are those the broker builds from the caller's user ID and the
   administrator policy, and its command is the fixed CedeGrid subcommand at the root-owned
   installed path.

---

### User Story 3 - Yield First gives way at once to other users (Priority: P2)

A user runs work in Yield First. When the second account starts CPU work, the user's work gives
way within milliseconds, and the second account gets what it would get if the user's work were
not there. When it stops, the user's work runs again.

**Why this priority**: it is the clearest gain of System mode for others: the kernel does the
giving way, so the margin Yield First holds today can go toward 0%.

**Independent Test**: on the test host after the user installed the package, a CPU-bound Yield
First job, then the second account's CPU-bound and latency-sensitive work, against the second
account's work with no own work at all.

**Acceptance Scenarios**:

1. **Given** Yield First work in `cedegrid-idle-u<uid>.service`, **When** the second account's
   CPU-bound work starts, **Then** own work's CPU falls as SC-004 says.
2. **Given** a 0% Yield First CPU margin, **When** the second account runs beside Yield First
   work, **Then** its throughput and p99 latency match its runs with no own work, as SC-005 says.
3. **Given** a running job whose mode changes between Yield First and another mode, **When** the
   change is applied, **Then** the broker moves that job, as a whole, between the caller's two
   units, and nothing else.

---

### User Story 4 - Own work is not the first to die for an OOM it did not cause (Priority: P2)

A user's work stays within that user's memory fair share. Another user's allocation grows until
the host runs out of memory. The user's in-share tasks survive. When it is the user who is over
their share, the user's over-share side is what the kernel takes first, as the decision in D2
says. The user cannot lower their tasks' scores below what the broker wrote.

**Why this priority**: the user's principle is that own work is protected as much as others', and
an OOM that is not own work's fault must not take own work first.

**Independent Test**: on the test host, the OOM shape of item 054 (six 1 GiB and one 3 GiB own
tasks, the second account's allocator growing to a host-wide OOM), with only these loads.

**Acceptance Scenarios**:

1. **Given** own memory within the user's fair share and the second account's allocator growing,
   **When** the host runs out of memory, **Then** no in-share own task is killed, as SC-006 says.
2. **Given** own memory above the user's fair share and the second account within its share,
   **When** the host runs out of memory, **Then** the first process killed belongs to the
   over-share side, as SC-007 says.
3. **Given** a root-written score on an own task, **When** the user writes a lower value, **Then**
   the kernel refuses it.
4. **Given** a process outside the cedegrid units, **When** the broker runs its loop, **Then** that
   process's score is never written.

---

### User Story 5 - An administrator installs and removes it cleanly (Priority: P3)

An administrator installs `cedegrid-system` from the project's apt repository or a release `.deb`,
sets the policy in `/etc/cedegrid/`, adds users to the group, and later removes the package. The
host is then as it was.

**Why this priority**: shared hosts are run by administrators who must be able to trust and undo
it, but the feature can be tested before packaging exists.

**Independent Test**: in the container, record the units, slices, groups and files under `/etc`,
`/run` and `/usr`; install; run jobs; purge; record again and compare.

**Acceptance Scenarios**:

1. **Given** a clean host, **When** the package is installed, **Then** the only additions are the
   broker's unit and socket, the two slices, the `cedegrid` group, `/etc/cedegrid/` and the
   installed files, and no PAM, login, SSH, other service, sysctl or kernel module setting changes.
2. **Given** an installed broker with running units, **When** the package is purged, **Then** every
   unit it made is stopped and removed, and SC-009's comparison is empty.
3. **Given** a policy that allows only Yield First and Neutral, **When** a user asks for
   Performance, **Then** the request is refused in one line naming the policy.

---

### Edge Cases

- The broker restarts while units run: it finds the units it made by name and marker, leaves their
  work running, and resumes scoring. A unit it did not make is never touched.
- A user is removed from the group while their units run: the next request is refused. What
  happens to the running units is pending decision N2 (recommended: they run until their work
  ends, then stop).
- A caller with user ID 0, or a system account below the host's first user ID: refused.
- A caller inside a user namespace or container: the peer credentials as the broker's namespace
  sees them decide; a user ID that maps to no account is refused.
- A user's unit reaches `TasksMax=`: new processes in it fail as the kernel decides; `doctor` and
  the run say the unit is at its limit.
- Memory counters cannot be read: that user's memory is unknown, no score is lowered for that
  user's tasks (Principle III), and lowered scores return to the default after a bounded window.
- A process exits and its PID is reused between the broker's read and its write: the broker holds
  a pidfd across both, so the write reaches the same process or none.
- A child forked by an own task inherits its parent's score until the next pass rescores it.
- A userspace OOM killer acts on cgroups (the test host runs one): the broker changes none of its
  settings for other units, and `doctor` says whether the cedegrid slices are within its reach.
- The host's systemd cannot set `cpu.idle` on a slice: the fallback the plan selects (research
  point 1) applies, and `doctor` says which one.
- A job's mode moves between Yield First and another while one of its processes forks: the move
  waits until the job is held still, so no process starts between the check and the move.
- Two runs of one user start at once: both land in the same unit; the unit is started once.
- The agent inside a unit exits or crashes: systemd's restart policy for the unit is the plan's;
  the broker never starts the user's program itself.

## Requirements *(mandatory)*

### Functional Requirements

**The broker and the units**

- **FR-001**: System mode MUST exist only where an administrator installed the system package.
  The default install stays unprivileged and unchanged.
- **FR-002**: The broker MUST listen only on `/run/cedegrid/broker.sock`, owned by root, group
  `cedegrid`, mode 0660. It MUST refuse to serve on a socket with any other owner or mode.
- **FR-003**: The broker MUST take the caller's user ID only from the socket's peer credentials. A
  request that carries a user ID, executable path, unit name, cgroup path, mount, device or
  capability MUST be refused as a whole and logged.
- **FR-004**: The broker MUST serve only callers that are members of the `cedegrid` group in the
  host's group database at the time of the request. It MUST refuse user ID 0 and accounts below
  the host's first ordinary user ID.
- **FR-005**: The broker MUST accept only this closed set of requests, each about the caller's own
  units: start, stop, status, and move a job of the caller's between the caller's two units. Any
  other request MUST be refused.
- **FR-006**: The broker MUST build every unit's name and properties from the caller's user ID and
  the administrator policy only: `cedegrid-u<uid>.service` in `cedegrid.slice` and
  `cedegrid-idle-u<uid>.service` in `cedegrid-idle.slice`, `User=<uid>`, the user's primary group,
  `Delegate=yes`, `TasksMax=` from the policy, and as the command a fixed CedeGrid subcommand at
  the root-owned installed path. No unit MUST get a fixed negative `OOMScoreAdjust=`.
- **FR-007**: The broker MUST start, stop and move work only in units it made, recognized by name
  and by a marker it sets. It MUST NOT act on any other unit.
- **FR-008**: The broker MUST bound every request: a size limit (64 KiB by default), a read
  timeout, a timeout on each systemd call, a per-user request rate and a per-user unit count, so
  that one user cannot delay another's request beyond SC-003's bound.
- **FR-009**: The broker's own unit MUST be confined as far as the host's systemd allows: no
  filesystem writes beyond the OOM score files of processes inside the cedegrid units and, where
  systemd cannot set it, the idle slice's `cpu.idle`; unix sockets only; and D-Bus to systemd
  only. The plan lists each setting and what the host's systemd version supports.
- **FR-010**: The administrator policy MUST be read from root-owned files in `/etc/cedegrid/`. A
  file writable by anyone but root MUST NOT be loaded. The policy gives the allowed modes, each
  user's task limit and `TasksMax=`. A user's request or setting beyond it MUST be refused in one
  line naming the policy, and `doctor` MUST show the effective policy.
- **FR-011**: Every request, its caller and its result, and every score the broker writes, MUST go
  to the system journal.
- **FR-011a**: Whether a unit opens a PAM session is pending decision N3 (recommended: none, with
  the units' process limits from the administrator policy). Under every option, no existing PAM or
  login setting changes, and `doctor` says which limits apply to System mode work.
- **FR-012**: Inside a unit, CedeGrid MUST run as today with the user's rights: the user's one
  agent per host (003 FR-002, in local or cluster mode) uses the present delegated Full path on the
  unit's cgroup, and policy, measurement and running work are 001's and 003's. The broker MUST NOT run the user's program, read the user's job, or take any
  figure from the user's agent.
- **FR-013**: Where no system package is installed, CedeGrid MUST behave exactly as without this
  feature. Where the socket exists but the broker does not answer within a short timeout, `run`
  MUST say so in one line and run as without the package, unless the user asked to require System
  mode, in which case it MUST refuse.
- **FR-014**: `doctor` MUST say which level applies (System, delegated Full or Tracked) and why,
  and `ps` and `top` MUST label System runs.
- **FR-015**: Every `cedegrid` command run with effective user ID 0 MUST refuse, as 003 FR-082
  says, except the broker's fixed subcommand started by its own unit.

**The slices and CPU**

- **FR-016**: Installing MUST create `cedegrid.slice` and `cedegrid-idle.slice` directly below
  the root cgroup, with `cpu.idle` on `cedegrid-idle.slice`. Other users' areas, such as the SSH
  service and login sessions, MUST be left as they are.
- **FR-017**: Yield First work MUST run in the caller's `cedegrid-idle-u<uid>.service`, and every
  other mode's work in `cedegrid-u<uid>.service`.
- **FR-018**: When a running job's mode changes between Yield First and another mode, the broker
  MUST move that job, as a whole, between the caller's two units, and only if each of its
  processes is in the caller's source unit and runs with the caller's user ID. No process MUST
  start in the job between that check and the move. The broker MUST NOT move any other process
  into a cedegrid unit.
- **FR-019**: Moving own work into `cedegrid.slice` MUST NOT give own work more CPU under
  contention than the baseline gives the same work, and MUST NOT give any other account less, on
  each host layout the plan names. The rule for `cedegrid.slice`'s CPU weight, fixed or following
  the number of users, MUST be chosen from measurements in the plan (research point 2).
- **FR-020**: In System mode, Yield First's CPU margin MUST be measured and lowered toward 0%,
  down to the lowest value at which the second account loses nothing measurable (SC-005). Yield
  First's memory and GPU rules stay as in 001.
- **FR-021**: `doctor` and the docs MUST say that `cpu.idle` gives way on CPU time only, not on SMT
  siblings, caches or memory bandwidth. Whether the margin can reach 0% MUST be decided by the
  measurement of research point 3.

**Memory and the OOM order**

- **FR-022**: The broker MUST read each user's memory from kernel counters itself, read-only:
  per-account cgroup counters where the host has them, else per-process counters summed by user
  ID. It MUST NOT use any figure a user's agent sends. A counter it cannot read MUST make that
  user's memory unknown.
- **FR-023**: The broker MUST write `oom_score_adj` only for processes inside cedegrid units it
  made, each identified by a pidfd held across the read and the write, and MUST NOT write it for
  any other process.
- **FR-024**: For each user's processes inside the cedegrid units, the broker MUST score down the
  part of the user's memory within the user's fair share, so that what is left of the kernel's
  score is the part above the share. When it does so is pending decision N1: at all times (N1 A),
  only while another principal's memory is above its own fair share (N1 B, recommended), or never
  (N1 C, which leaves only FR-025). The lowest value MUST be −999; −1000 MUST never be used. A
  value MUST be written only when it changes.
- **FR-025**: A task wholly above the user's fair share MUST get 1000 where the mode's floor rule
  marks over-share tasks (D2): in Yield First and Neutral only under D2 case A, in every mode under
  D2 case B. Otherwise it MUST keep the default score.
- **FR-026**: When the user's memory is unknown, the broker MUST lower no score for that user's
  tasks, and lowered scores MUST return to the default after a bounded window (Principle III).
- **FR-027**: The docs MUST state the remaining limit: when a small process outside CedeGrid
  causes the OOM, the kernel still kills a bigger process.

**Security tests and validation**

- **FR-028**: Tests that need root MUST run first in a container with systemd as PID 1. No server
  is touched for them.
- **FR-029**: The security suite MUST cover, at least: a user ID inside a request, odd and
  overlong names, a message at and above the size limit, a message that never ends, a burst of
  requests, the socket's owner and mode, a non-member, user ID 0, a request outside the closed set,
  an attempt to stop or move the second account's unit, a lower score written by the user, and a
  process outside the units that must keep its score.
- **FR-030**: On the test host the user installs the package themselves. The work session MUST NOT
  use `sudo`. The plan MUST list every command that needs root there, exactly, for the user to
  read first.
- **FR-031**: The shared host MUST NOT be part of this feature's validation until the user says so.
- **FR-032**: Before release, the broker, its policy loader, its OOM loop and every other file that
  runs as root or faces the kernel MUST get a security review. The plan lists the files.

**Install and packaging**

- **FR-033**: There MUST be two apt packages: `cedegrid` (the unprivileged CLI) and
  `cedegrid-system` (the broker, the two slices, the group, the socket and `/etc/cedegrid/`). Each
  ships as a `.deb` in the releases and from the project's own apt repository first.
- **FR-034**: Installing MUST change nothing but what FR-033 lists. Purging MUST stop and remove
  every unit the broker made and leave the host as before the install (SC-009).
- **FR-035**: The broker and the CLI MUST check each other's protocol version and refuse a
  mismatch in one line that says which side to upgrade.

**What System mode does not change**

- **FR-036**: System mode MUST NOT add a give-back of its own. Inside a unit, the modes are 001's:
  under D1 case A as item 057 Part B states, under D1 case B as 001 states today. Caps inside a
  unit follow D3.
- **FR-037**: Principle I holds in full: the broker acts on no process outside the cedegrid units,
  and inside them it writes only scores and makes only the moves FR-018 allows. A process is under
  a restriction only after CedeGrid has verified it ("verify, then put it in", user, 2026-11-13):
  a unit's cgroup can gain a process only by a fork inside it or by the broker's move of verified
  own work, and caps inside it follow D3.
- **FR-038**: The findings deferred to this feature in `follow-ups.md` (029 Part 0f, SUP-9,
  AGT-15, AGT-17, EFF-6 to EFF-10) MUST each be resolved here or moved to a named later feature
  with the reason, in the plan.

### Key Entities

- **Broker request**: kind (start, stop, status, move), the job for a move, the requested mode; no
  identity of any kind. Size-limited.
- **Caller**: user ID from the peer credentials, group membership at request time, policy limits.
- **Own unit**: name, kind (ordinary or idle), user ID, slice, marker, state, task limit.
- **Administrator policy**: allowed modes, per-user task limit, `TasksMax=`, file owner and mode.
- **User memory record**: user ID, memory read, fair share, readable or unknown, time read.
- **Score record**: process (pidfd, PID, start time), unit, value written, time written.

## Host layouts and the baseline

What moving own work into `cedegrid.slice` changes against plain Linux, on each host the user
named. These are read-only facts from the brief and reasoning; nothing here was run, and the
shared host is not validated by this feature (FR-031).

| | The test host | The shared host |
|---|---|---|
| Layout | Kernel 7.0 (EEVDF). The `cpu` controller is enabled below `user.slice` and below each account's slice (weight 100 each); each login has its own session scope. `systemd-oomd` active, 8 GiB swap | Kernel 6.8 (CFS), systemd 249, autogroup on. The `cpu` controller is enabled at the root but not below `user.slice`; an SSH login is placed in `system.slice/ssh.service` |
| Plain Linux splits CPU | by account, then by session, then by thread | among the root's groups, then by thread among every SSH user's threads in one group |
| Own work in `cedegrid.slice` | leaves its account's slice and competes at the root beside `user.slice` and `system.slice`. At equal weights it could get as much as all logged-in accounts together, or as little as one account's share in `user.slice`, depending on how many users each side holds. FR-019 makes the weight keep both sides at their baseline | leaves the SSH users' shared group and competes beside `system.slice` at the root. At equal weights own work would get about half the host whatever its thread count: more than the baseline when own work has few threads, less when it has many. FR-019 applies |
| Yield First in `cedegrid-idle.slice` | gives way to every sibling at the root, so to every other account and to own work in other modes | same |
| Memory | per-account counters exist below `user.slice` | other users' memory has no per-account cgroup, so the broker sums per-process counters by user (FR-022) |

## Success Criteria *(mandatory)*

### Measurable Outcomes

Thresholds marked *provisional* are set by the plan's measurements and confirmed by the user.

- **SC-001**: In the container, 20 of 20 runs by a member land in the member's own unit, with the
  member's IDs, at the Full level, and `doctor` says System; with the package removed, 20 of 20
  runs behave as before the install.
- **SC-002**: Every case of FR-029 is refused or holds as stated, is logged, and leaves every other
  user's units and every process outside the cedegrid units unchanged, in 100% of runs.
- **SC-003**: While one member sends requests as fast as they can, another member's request is
  answered within 1 s (*provisional*), and the broker's memory stays within its bound.
- **SC-004**: When the second account's CPU-bound work starts beside Yield First own work, own work's
  CPU on the contended CPUs falls to the idle level within 10 ms (*provisional*), in each of at
  least 3 runs.
- **SC-005**: At the Yield First margin FR-020 sets, the second account's throughput and p99 latency
  beside Yield First own work are within the spread of its runs with no own work, at least 3 runs
  each way, on CPU-bound and on cache-sensitive work.
- **SC-006**: In the OOM shape of User Story 4, no in-share own task is killed, in 3 of 3 runs
  (under decision N1 A or B; under N1 C the kernel's default choice is recorded instead).
- **SC-007**: With own memory over the user's fair share and the second account within its share,
  the first process the kernel kills belongs to the over-share side, in 3 of 3 runs.
- **SC-008**: A user's attempt to write a score below a root-written one fails, in 100% of tries.
- **SC-009**: After install, runs and purge in the container, the list of units, slices, groups and
  files under `/etc`, `/run` and `/usr` equals the list before the install.
- **SC-010**: With the system package absent, the existing test suites pass unchanged.
- **SC-011**: On the test host, in the shapes F1 and F2 of item 057, own work in System mode gets no
  more CPU than the baseline and the second account no less, within the spread of at least 3 runs
  each way.
- **SC-012**: With memory steady, the OOM loop writes no score after its first pass; the broker's
  CPU stays below 0.1% of one CPU and its memory below 32 MiB with 50 users (*provisional*).

## Assumptions

- The host has cgroup v2 unified and systemd as PID 1; the system package refuses to install
  otherwise. The floor is glibc 2.35 and kernel 5.15, which has `cpu.idle`.
- Users install nothing as root. An administrator installs the system package and manages the
  group; the user installs it themselves on the test host.
- Group membership is read at each request, so a removal takes effect at the next one. What
  happens to a removed user's running units is decision N2.
- Phase 1 runs one agent per user inside that user's unit. One shared agent for all users is later
  (open point 6).
- GPU access is unchanged: System mode changes no device permission, and 001's GPU rules apply
  inside the unit.
- System mode is Linux only. macOS and Windows keep their client paths.
- `run` asked to require System mode uses a flag the plan names; without it, `run` falls back.
