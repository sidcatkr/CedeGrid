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

### Session 2026-11-19

- Q: Is matching plain Linux enough for Performance in the default mode? → A: No. The goal is a
  measurable gain over plain Linux in the default mode, not only no loss; a result within the
  run-to-run spread does not yet meet it.

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
  - the unprotected raise in System mode, with and without a protected first-comer.

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
