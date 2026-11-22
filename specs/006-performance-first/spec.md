# Feature Specification: Performance First

**Feature Branch**: `claude/project-thread-ttyiyp`, alongside 004 and 005. The spec directory is
independent of the branch.

**Created**: 2026-11-18

**Status**: Draft

**Input**: Maintainer decisions of 2026-11-18 and 2026-11-19. Performance, the default mode, must
measurably do better for own work than the same work started on plain Linux, and never worse. Between
CedeGrid users, the one that arrived first keeps its fair share. An opt-in "admit immediately"
lifts that hold only beside a first-comer that chose to yield, and a separate opt-in at the
highest warning level raises own work's CPU share above other users' where the host provides a
way.

This feature changes [001-contention-modes](../001-contention-modes/spec.md) admission in
Performance and Aggressive (FR-020 to FR-022, FR-065, FR-089, and the kept equal share of
2026-11-17), and replaces [004-aggressive-mode](../004-aggressive-mode/spec.md)'s unprotected
Aggressive with the two options below. It uses [005-system-mode](../005-system-mode/spec.md)'s
broker and units. The constitution (9.0.0) applies, in particular Principle II's Principals,
Arrival order, Admission, Admit immediately and Unprotected rules, and Principle V's slice weight.

## Clarifications

### Session 2026-11-18

- Q: Must Performance ever do worse than plain Linux for own work? → A: No. Yield First and
  Neutral may do worse and yield more; Performance, by its name, must never fall behind plain
  Linux and must beat it.
- Q: When own work was there first and another user arrives? → A: Performance never steps
  aside, and keeps starting new work as plain Linux would.
- Q: When own work arrives after another user? → A: beside a user who does not run CedeGrid,
  start as plain Linux would, since that user's own default is to start work at once. Beside
  another CedeGrid user who arrived first, hold new work at the fair share by default.
- Q: What does "admit immediately" do beside a CedeGrid user who arrived first? → A: it lifts
  the hold, except beside a first-comer running in Performance (or Aggressive): that one is
  always protected, and the option is ignored for it.
- Q: How is a user CedeGrid cannot identify treated? → A: as a user who does not run CedeGrid.
- Q: May own priority be raised above other users'? → A: yes, as an opt-in at the highest
  warning level, never by default, and only where the host provides a way.
- Q: Beside a competing plain user, may Performance start more CPU tasks than the CPUs hold? →
  A: yes. Plain Linux starts every task and the kernel gives more threads more CPU, and CedeGrid
  wins against non-CedeGrid users (gate F2c showed own work 13% below plain at the CPU limit).
- Q: Is 004's unprotected Aggressive kept? → A: its consent path is kept for the priority
  option, renamed unprotected; its admission effect is now Performance's default beside plain
  users and admit immediately's beside CedeGrid users.

### Session 2026-11-22

- Q: Own work first loses 2–3% of its CPU after a plain principal arrives, because the tasks
  held back for capacity start then and the kernel puts them off that principal's CPUs. How is
  that recovered without giving up the batch's mean completion? → A: those tasks start evenly
  placed and are released to the kernel 0.5 s later (FR-007b). By hand, the same placement
  matched plain Linux on both sides: own CPU after the arrival within 0.4%, the plain
  principal's rate within 0.4%. Pinned for good, own work took 2% more and the plain principal
  got 7.7% less than beside plain Linux, which only priority over plain users may do.

### Session 2026-11-19

- Q: Is matching plain Linux enough for Performance in the default mode? → A: No. The goal is a
  measurable gain over plain Linux in the default mode, not only no loss; a result within the
  run-to-run spread does not yet meet it.
- Q: Where the kernel splits CPU evenly per account, how does Performance get more CPU than plain
  Linux beside a plain user? → A: by an install option, priority over plain users: CedeGrid work
  runs at a higher CPU weight than the work of accounts with no CedeGrid work, while CedeGrid
  users never take each other's share.
- Q: What weight? → A: 200 against the default 100, so own work gets about two thirds of a
  contended CPU against one plain user; the administrator policy may set 101 to 10000, and the
  unprotected run option stays at its own weight (400 by default).
- Q: Who turns it on? → A: both. The administrator permits it in the System mode policy (at
  install, changeable later), and each user chooses it in `cedegrid setup` (changeable by running
  setup again).
- Q: What does setup default to? → A: on a terminal the question has Yes preselected; an
  unattended setup leaves it off unless given its flag.
- Q: How does a principal whose CedeGrid work cannot be identified count? → A: as plain, so the
  raise applies; in System mode other CedeGrid users are recognized by their broker units in the
  cgroup tree, which stays readable where `/proc` is hidden.

## Terms

- **Plain principal**: another account whose work on a resource CedeGrid does not identify as
  CedeGrid work, including usage it cannot attribute or identify.
- **CedeGrid principal**: another account while CedeGrid identifies, read-only, that work it is
  running on the resource runs under CedeGrid.
- **Arrival**: for a run on a node, the time its first task started there. A principal's arrival
  on a resource is the earliest arrival among its CedeGrid work running there.
- **First-comer**: a CedeGrid principal whose arrival on a resource is earlier than own work's.
  Where either arrival is unknown, or both are equal, the other principal counts as the
  first-comer.
- **Protected first-comer**: a first-comer whose Performance or Aggressive work on the resource
  itself arrived first, by the same comparison.
- **Hold**: 001's admission protection in Performance (the kept equal share and the margin on
  CPU, the time-slice rule on GPU compute), applied only for first-comers.
- **Admit immediately**: a per-run caution option that lifts the hold for first-comers that are
  not protected.
- **Unprotected**: a per-run option at the highest warning level that implies admit immediately
  and raises own work's CPU share where the host provides a way.
- **Raise**: CPU share above the kernel's even split, set where other users' work competes with
  own work: a higher CPU weight on the caller's account slice in System mode, where the host
  splits CPU by account, or a negative nice value within a limit the administrator granted the
  account, where own work shares a CPU cgroup with other users' work.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Performance never loses to plain Linux (Priority: P1)

An operator runs jobs in Performance, the default, on a host where other people start work by
hand. Whether own work arrived first or second, it gets at least what the same work started by
hand would get, and CedeGrid adds what plain Linux does not do.

**Why this priority**: it is the reason to run CedeGrid at all.

**Independent Test**: run each host gate shape with a plain arm (the same own work started by
hand in the same place) and compare own work's throughput.

**Acceptance Scenarios**:

1. **Given** another account saturating two of four CPUs, **When** eight 1000 m own tasks are
   submitted in Performance, **Then** own work starts up to the scope's capacity at once and
   gets at least what the eight tasks started by hand get, less CedeGrid's measured overhead.
2. **Given** another account's process saturating a GPU, **When** two own GPU tasks are
   submitted in Performance, **Then** both start, and own work gets at least what two processes
   started by hand get, less the overhead.
3. **Given** own work running first, **When** another account arrives and saturates the same
   resource, **Then** no running own task is stepped, and queued own tasks keep starting as they
   would by hand.
4. **Given** an idle scope, **When** eight 500 m own tasks are submitted, **Then** all eight
   start, as by hand.

### User Story 2 - Two CedeGrid users do not fight (Priority: P1)

Two accounts on one host both run CedeGrid. Whoever arrived first keeps its fair share; the
later one holds new work at the fair share instead of piling on. A later user's "admit
immediately" is ignored beside a first-comer running Performance.

**Why this priority**: without it, two CedeGrid users would both start everything, and CedeGrid
would give its users nothing over plain Linux.

**Independent Test**: run a second CedeGrid agent as the second account, in each order, with and
without admit immediately, and read both agents' decisions and rates.

**Acceptance Scenarios**:

1. **Given** account A running Performance work first, **When** account B's CedeGrid work
   arrives in Performance, **Then** B starts new work only up to the hold, and A keeps starting
   new work as plain Linux would.
2. **Given** the same, **When** B's run takes admit immediately, **Then** B still holds beside A,
   and B's decision says the option was ignored for a protected first-comer.
3. **Given** A running only Neutral work first, **When** B's Performance run takes admit
   immediately, **Then** B starts as plain Linux would beside A.
4. **Given** A and B whose work arrived in the same millisecond, or whose arrival cannot be
   read, **Then** each counts the other as the first-comer and both hold, at equal shares, with
   no admission flapping.

### User Story 3 - Unprotected raises own CPU share where the host allows (Priority: P2)

An operator who needs a deadline met turns on unprotected for one run, through the host's
allowance and the typed confirmation. Where System mode or an administrator-granted nice limit
provides a way, own work's CPU share rises above the even split, never while a protected
first-comer is on the CPU. Every output marks the run in red.

**Why this priority**: an explicit, rare escape the maintainer asked to have available.

**Independent Test**: in System mode with the administrator key on, run F2c with an unprotected
run and read the unit's CPU weight, own work's share and the other account's rate; repeat with a
protected first-comer present.

**Acceptance Scenarios**:

1. **Given** System mode with the ordinary unit in the account slice, the allowance, the
   administrator key and a granted run, **When** another account saturates the CPU, **Then** the
   caller's account slice runs at the raised CPU weight while the run is active, and own work's
   CPU share is above the even split.
2. **Given** the same with a protected first-comer on the CPU, **Then** no raise applies.
3. **Given** a Tracked node without a granted nice limit, **Then** the run is admitted
   immediately, no raise applies, and every output says no raise is available on this host.
4. **Given** the run ends, is drained, or the allowance is turned off, **Then** the weight or
   nice value returns to the default within two monitor intervals.

### User Story 4 - Measured gains over plain Linux (Priority: P2)

The maintainer can point to measured cases where own work does better under CedeGrid than by
hand.

**Why this priority**: CedeGrid has no point without a gain.

**Independent Test**: the gain gates below, each with a plain arm.

**Acceptance Scenarios**:

1. **Given** a batch of twice the scope's capacity, **Then** under CedeGrid its tasks finish
   earlier on average than when all start by hand, with the same total time.
2. **Given** more jobs than the host's RAM holds, **Then** under CedeGrid no own job is killed
   for memory, where by hand at least one is.
3. **Given** more GPU jobs than the device's memory holds, **Then** under CedeGrid no own job
   fails to allocate, where by hand at least one does.

### User Story 6 - Priority over plain users, chosen at install (Priority: P1)

An operator in System mode, with the administrator's permission, answers yes in setup. When a
user who runs no CedeGrid work competes for the CPU, own Performance work gets about two thirds
of it, where plain Linux would give each account half. Another CedeGrid user is never pushed
below their fair share by it.

**Why this priority**: where the kernel splits CPU per account, it is the only way Performance
gets more CPU than plain Linux by default, the maintainer's goal of 2026-11-19.

**Independent Test**: in System mode with the policy key on and the user's choice on, run F2c
and own-first on CPU with a plain arm; then again beside a second CedeGrid account with and
without the option.

**Acceptance Scenarios**:

1. **Given** the policy permits it, the user chose it and own Performance work runs, **When** a
   plain account saturates the same CPUs, **Then** the account slice runs at weight 200 and own
   work's CPU share is above the even split by at least 10 points.
2. **Given** the same, **When** a second CedeGrid account without the option competes, **Then**
   no raise applies while it competes, and both get the even split.
3. **Given** both CedeGrid accounts chose it, **When** they compete with each other and a plain
   account, **Then** both run at 200, the even split holds between them and both get more than
   the plain account.
4. **Given** the policy forbids it, or the user did not choose it, or the host is rootless,
   **Then** no raise applies, and `doctor` says which condition is missing.
5. **Given** a raise in force, **When** own work ends, the user turns it off, the policy turns it
   off or the user leaves the group, **Then** the weight returns to the default within two
   monitor intervals, or within the broker's 5 s check.

### User Story 5 - See who holds whom (Priority: P3)

`doctor` and `observe` show, for each other principal on each compute resource, whether it is
plain or CedeGrid, its arrival, whether it is a first-comer, and whether admission holds for it;
and for own work, its arrival and the options in force.

### Edge Cases

- A host that hides other users' `/proc` entries: other accounts' work cannot be identified, so
  they count as plain principals; `doctor` says identification is unavailable.
- A principal with both CedeGrid and hand-started work on a resource is a CedeGrid principal
  there; the hold counts all its usage.
- On a host with System mode, work outside the broker's units does not count as CedeGrid work,
  whatever its command line says.
- Own work with nothing running on the resource has no arrival there: any CedeGrid principal on
  it is a first-comer.
- A run's arrival stays the same across its tasks while any of them runs on the node, so a
  queue does not lose its place between tasks. A run that has nothing left running there
  arrives anew with its next task.
- An agent restart keeps every arrival, which lives in the running supervisors' command lines.
- The wall clock steps: arrivals already recorded keep their order; a later run gets a time
  from the stepped clock. All agents on a host read one clock.
- Admit immediately or unprotected on Yield First or Neutral work has no effect, and its
  decision says so.
- Unprotected on a host with no way to raise: admit immediately only, reported as such.
- GPU compute never gets a raise.

## Requirements *(mandatory)*

### Functional Requirements

**Identification and arrival**

- **FR-001**: Each own task supervisor's command line MUST carry a CedeGrid marker with the
  task's mode on each compute resource and its run's arrival on the node, in milliseconds since
  the epoch. The marker carries nothing that names the account, the run or the task's arguments.
- **FR-002**: Another account MUST count as a CedeGrid principal on a resource while it has
  CedeGrid work there: on the CPU, a live process that is a marked supervisor of that account or
  descends from one; on a GPU device, such a process listed on the device. The agent reads command
  lines, parent chains and cgroups from `/proc`, and GPU process lists, read-only, and acts on
  none of them (Principle I).
- **FR-003**: On a host with System mode installed, only processes inside units the broker
  made MUST count as CedeGrid work.
- **FR-004**: Every other account, including usage CedeGrid cannot attribute and processes whose
  command line or parent chain cannot be read, MUST count as a plain principal.
- **FR-005**: A CedeGrid principal's arrival on a resource MUST be the earliest arrival among its
  CedeGrid work running there; on GPU compute, among its work listed on that device. Own work's
  arrival is computed the same way from own running work.
- **FR-006**: A CedeGrid principal MUST be a first-comer when its arrival is earlier than own
  work's, when either is unknown, when they are equal, or when own work has nothing running on
  the resource. It is a protected first-comer when the same holds for the earliest arrival among
  its Performance or Aggressive work there.

**Admission in Performance and Aggressive**

- **FR-007**: Beside plain principals and CedeGrid principals that are not first-comers, new own
  work on CPU and GPU compute MUST be admitted as plain Linux would start it: up to the scope's
  capacity less the operator reserve and envelope, without deducting their usage, without an
  admission margin, without holding out CedeGrid's own CPU, and on GPU compute without the
  time-slice hold, within VRAM headroom.
- **FR-007a**: While a plain principal contends for the CPU and no first-comer holds own work,
  Performance and Aggressive MUST NOT bound new own CPU work by the scope's capacity or the CPU
  part of the operator's envelope: they start every task the node's task limit, the memory
  floors and the RAM and GPU envelope allow, as plain Linux starts every task, since the kernel
  gives more threads more CPU (maintainer, 2026-11-18). Own work started this way MUST NOT be
  stepped when that principal leaves; new CPU work then waits until own CPU requests fit the
  capacity again. On an idle scope and beside first-comers the CPU-sized limit stays.
- **FR-007b**: Own CPU tasks that waited for the scope's capacity and start because a plain
  principal began to contend (FR-007a) MUST start where plain Linux would already have had them:
  each pinned to the scope CPU holding the fewest own tasks, then put back on the whole scope
  0.5 s after it starts, every thread of the member and of its managed children, under the
  constitution's rule for calls that take only a thread ID. Tasks submitted while a plain
  principal already contends start on the whole scope as today: placing those evenly takes more
  CPU from that principal than plain Linux does. A thread whose release cannot be verified is
  reported and not touched again.
- **FR-008**: Beside a first-comer, new own work MUST be held as 001 holds it today, counting only
  first-comers: the kept equal share with the mode's margin on CPU, and the time-slice rule on
  GPU compute. New own work MUST NOT start into a first-comer's activity of unknown amount.
- **FR-009**: Running own work MUST NOT be capped, paused or drained for any principal (001
  FR-022, unchanged).
- **FR-010**: Yield First and Neutral MUST keep 001's admission and give-back for every
  principal, plain or CedeGrid.
- **FR-011**: Memory floors and VRAM headroom MUST bound admission in every mode, unchanged.

**Admit immediately**

- **FR-012**: A run MUST be able to take admit immediately only through an explicit
  per-submission SDK field and its confirmation field. Configuration, profiles, environment,
  upgrades, migrations and the node cap MUST NOT set it.
- **FR-013**: Admit immediately MUST lift FR-008's hold for first-comers that are not protected,
  and MUST NOT lift it for a protected first-comer; the decision names the first-comer kind for
  which it was ignored.
- **FR-014**: Admit immediately is a caution option: shown with `[caution]` in `observe`,
  `doctor` and the log line at the run's start.

**Unprotected**

- **FR-015**: Unprotected MUST replace 004's unprotected Aggressive and keep its consent path:
  the host allowance in the state database, turned on only in interactive setup with the typed
  phrase and failing closed; the per-submission SDK field and its confirmation; on a System mode
  host, the administrator key. It applies to Performance and Aggressive work and implies admit
  immediately.
- **FR-016**: Under unprotected, while no protected first-comer is on the CPU, own work's CPU
  share MUST be raised where the host provides a way:
  - in System mode, where the caller's ordinary unit sits in its account slice, the broker
    raises that slice's CPU weight to the administrator policy's value while the agent asks, and
    returns it to the default when the agent stops asking, stops or is removed from the group;
  - where own work shares a CPU cgroup with other users' work, the supervisor sets a negative
    nice value on the run's verified processes, no lower than the account's granted nice limit
    allows;
  - anywhere else no raise reaches other users' work, and none is applied: a nice value does not
    cross CPU cgroups, and only an administrator can weight one account's slice over another.
- **FR-017**: No raise MUST use real-time scheduling classes, act on another process, or apply
  to GPU compute.
- **FR-018**: A raise MUST end within two monitor intervals after the run ends or is drained,
  the allowance turns off, the administrator key turns off, or a protected first-comer arrives.
- **FR-019**: An unprotected run MUST be marked `UNPROTECTED` in red in `observe`, `doctor`, the
  coordinator status and the log, with the raise in force (slice weight, nice value, or none
  available).

**Priority over plain users**

- **FR-024**: The administrator policy MUST gain `modes.plain_priority` (bool, default false) and
  `modes.plain_priority_cpu_weight` (101 to 10000, default 200). Only with both the key true and
  the user's choice on does the agent ask the broker for this raise.
- **FR-025**: `cedegrid setup` MUST ask, on a terminal, whether CedeGrid work gets priority over
  users who run no CedeGrid work, with Yes preselected; `--priority-over-plain` and
  `--no-priority-over-plain` set it unattended, and without either an unattended setup leaves it
  off. Running setup again changes it. Setup MUST say when the host cannot apply it (rootless,
  or the policy forbids it).
- **FR-026**: While the choice is on, own Performance or Aggressive CPU work runs and every
  CedeGrid principal contending on the CPU runs at the policy's weight or more, the agent MUST
  ask the broker to set the caller's account slice to `modes.plain_priority_cpu_weight`, and
  MUST withdraw within two monitor intervals once any of these stops holding. A CedeGrid
  principal's weight is read from its account slice in the cgroup tree; one whose slice cannot
  be read counts as not raised. A principal that falls below a held raise sees it during those
  two intervals, so one that chose the option too follows and the raise holds; one that did not
  gets its share back when they end. Where the unprotected raise also applies, the higher weight
  wins.
- **FR-027**: The broker MUST serve this raise as it serves the unprotected one (FR-016, FR-018):
  only for the caller's own account slice, only while the policy key is true and the caller's
  ordinary unit sits in that slice, read back, with a 30 s lease, and ended on withdrawal, the
  units' stop, the lease's expiry, the key turning off, the caller leaving the group, and the
  broker's stop or start. It MUST never set a weight over one the broker did not set.
- **FR-028**: A principal whose CedeGrid work cannot be identified counts as plain. In System
  mode an account with a broker unit in the cgroup tree counts as CedeGrid on the CPU while its
  unit runs work, whether or not its processes are visible.
- **FR-029**: `doctor` and `observe` MUST show whether priority over plain users is chosen,
  permitted and in force, with the weight, and the reason when it is not.

**Visibility and decisions**

- **FR-020**: Each admission decision MUST state, in one sentence, whether it admitted as plain
  Linux would or held for a first-comer, and which.
- **FR-021**: `doctor` and `observe` MUST list, per compute resource, each other principal's kind,
  its arrival when CedeGrid, whether it is a first-comer, and whether admission holds for it,
  and own work's arrival and options.

**Wire, SDKs and compatibility**

- **FR-022**: The admit immediately and unprotected fields MUST land in Rust, Python and
  TypeScript together, with the contract fixture. 004's `unprotected_aggressive` fields are
  renamed before 0.2.0 ships.

**Gates**

- **FR-023**: The test host MUST run, with a plain arm and at least three runs each:
  - CPU and GPU F2c with a plain contender that arrived first, per-user and same-cgroup;
  - own work first, with a plain contender arriving later, CPU and GPU;
  - fill on an idle scope;
  - two CedeGrid accounts in each order, with and without admit immediately;
  - batch completion time, more jobs than RAM, and more GPU jobs than VRAM;
  - the unprotected raise in System mode, with and without a protected first-comer;
  - priority over plain users in System mode: F2c and own-first on CPU against a plain account,
    and against a second CedeGrid account with and without the option.

### Key Entities

- **Marker**: a task's modes per compute resource and its run's arrival, in its supervisor's
  command line.
- **Principal kind**: plain or CedeGrid, per resource, with arrival and first-comer status.
- **Admit immediately grant** and **unprotected grant**: per-run records, as 004's grant.
- **Raise**: the account slice weight or nice value in force, or none, per unprotected run.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Beside a plain principal, own work's throughput in Performance is at least the same
  work's started by hand in the same place, less CedeGrid's measured overhead, in 3 of 3 runs of
  each FR-023 CPU and GPU shape on a host with per-user CPU cgroups.
- **SC-002**: On an idle scope, Performance starts every task that the scope's capacity holds,
  as many as by hand.
- **SC-003**: With two CedeGrid accounts, the first-comer's throughput is at least what it gets
  beside the later account's work started by hand, and the later account's admission changes
  state at most once per arrival change, in 3 of 3 runs of each order and option.
- **SC-004**: For a batch of twice the scope's capacity, the mean task completion time under
  CedeGrid is at least 20% lower than by hand, with the total time within 5%.
- **SC-005**: With more jobs than RAM, no own job is killed for memory under CedeGrid; by hand at
  least one is. With more GPU jobs than VRAM, no own job fails to allocate under CedeGrid; by
  hand at least one does.
- **SC-006**: In System mode, an unprotected run's CPU share beside a saturating plain account is
  above the even split by at least 10 points, never while a protected first-comer is present,
  and returns to the even split within two monitor intervals after the run.
- **SC-007**: No test or gate shows CedeGrid acting on a process outside its verified set.
- **SC-008**: Identification adds less than 0.2% of one CPU to the agent at a 500 ms monitor
  interval with 200 other processes.
- **SC-009**: In System mode with priority over plain users permitted and chosen, own work's CPU
  share beside a saturating plain account is above the even split by at least 10 points, and its
  throughput above the same work by hand, in 3 of 3 runs of F2c and own-first; beside a second
  CedeGrid account without the option, both stay at the even split.

## Assumptions

- On a host without per-user CPU cgroups, plain Linux splits CPU per thread, so work started all
  at once beyond the scope's capacity gets more than work started up to it. Performance starts up
  to the scope's capacity; the same-cgroup gate measures the difference, and the result decides
  whether that layout needs its own rule.
- GPU drivers on consumer devices offer no per-process priority, so GPU compute has no raise.
- An administrator grants a nice limit through the host's login limits; CedeGrid never changes
  them.
- The CLI parts (003) are out of scope; the SDKs, the wire, setup, `doctor` and `observe` carry
  the feature.
