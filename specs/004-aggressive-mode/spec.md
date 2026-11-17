# Feature Specification: Aggressive Mode for CPU and GPU Compute

**Feature Branch**: `001-contention-modes`, specified alongside it. The spec directory is
independent of the branch. Implementation starts on its own branch, `004-aggressive-mode`, once
001 is merged.

**Created**: 2026-10-26

**Status**: Draft, reworked on 2026-11-17 against constitution 8.1.0

**Input**: Maintainer feature description: add an Aggressive contention mode for CPU and GPU
compute, protected by default, with an unprotected form behind an explicit per-run flag and a
two-step confirmation.

This feature builds on [001-contention-modes](../001-contention-modes/spec.md): its modes, node
cap and per-job modes (FR-030), Performance (FR-022), probing (FR-021, FR-072), floors (FR-023),
caution options (FR-043, FR-051) and the mode ordering (FR-065). It uses
[003-everyday-cli](../003-everyday-cli/spec.md) for `run`, per-job entries and profiles
(FR-020–FR-026), `ps` and `top` (FR-010, FR-013), the setup interface (FR-030–FR-033) and
per-user run-queue wait (FR-050). The constitution (8.1.0) applies, in particular Principle II's
Aggressive mode and its one named exception, unprotected Aggressive.

**Rework (2026-11-17, constitution 8.1.0)**: under 001 item 057, Performance takes no running
steps on compute, and Aggressive follows it. In both forms, running own work is never capped,
paused or drained to give another workload more CPU or GPU compute time. Protected Aggressive
protects other principals at admission only. Unprotected Aggressive is the one admission
exception. Memory in both forms is as in Performance: the floors bound what is admitted, own
memory gets no hint, drain or TERM for a floor breach, and no own task is marked as an OOM
victim. GPU compute has no duty cycle (item 057 Part C), and modes are per job. This release
builds 004 without 003's CLI surfaces (`run`, `--save`, per-job entries, profiles, `ps`, `top`).
A per-run request for unprotected Aggressive goes through the SDK and the wire. 001's `setup`,
`status`, `observe` and `doctor` carry the rest. Where the text below and this note differ,
this note applies.

**Note (2026-10-29, constitution 7.0.0, 001 item 029)**: in both forms, memory floors are kept
down to the fair share (001 FR-023): at a breach others' growth caused, own memory above its
fair share takes the floor path and in-share own memory gets the save-progress signal (001
FR-075). Out-of-memory order by fault replaces the OOM-victim rule, and Performance is 001's
default mode and no caution option. Where this spec says otherwise, the note applies.

## Clarifications

### Session 2026-10-26

- Q: May protected Aggressive be stored in configuration, saved setup and per-job entries the
  way Performance can? → A: Yes, as a caution option with its acknowledgement; only unprotected
  Aggressive is never stored.
- Q: If a node's default mode is Aggressive but nobody raised the node cap to Aggressive, what
  mode do its jobs run in? → A: Performance, with `mode_capped`, until the cap is raised to
  Aggressive in its own confirmed step.
- Q: Does "scale up at once with no ramp" remove the minimum step dwell, hysteresis and reversal
  tolerance for Aggressive? → A: No. They stay Performance's; only the scale-up interval and the
  admission pace change.
- Q: Does unprotected Aggressive change anything for RAM, GPU memory or GPU compute beyond
  declining to give compute back? → A: No. Memory floors, VRAM floors, Principle I and the
  OOM-victim rule are unchanged; GPU compute follows the CPU rules.
- Q: Does the global "allow unprotected Aggressive" setting affect interactive starts? → A: No.
  It is off by default and only gates non-interactive starts.
- (Superseded on 2026-11-17 by constitution 8.1.0: protection acts at admission only.) Q: How does protected Aggressive give CPU and GPU compute back to a waiting principal below its
  protected share, compared with Performance? → A: It never holds less than Performance in the
  same observation (report 018 Decision 1, answer B). For each compute resource and observation,
  its target is the larger of Performance's target for that observation and its own target (0%
  margin, scale-up at once) limited so that every other active principal keeps its protected
  share. While Performance holds more than the protection leaves, Aggressive equals Performance
  and gives back exactly as Performance does; once Performance has stepped down to the protected
  point or below, Aggressive holds no more than the protection leaves. The mode ordering stays
  strict per observation, with no exception.
- (Superseded on 2026-11-17.) Q: How quickly must a light contender below its fair share reach its demand in protected
  Aggressive? → A: No later than under Performance in the same scenario: 001's SC-012 reclaim
  bounds apply unchanged.
- (Superseded on 2026-11-17.) Q: How much does own work give back when several light principals wait at once? → A: Each keeps
  its protected share; own work goes no lower than the protection needs and never below
  Performance in the same observation.
- (Superseded on 2026-11-17.) Q: What happens when a protected principal stops waiting and later waits again? → A: The same
  rule applies in every observation: own work gives back again as Performance would, never below
  Performance, and Performance's step dwell and hysteresis prevent cap and uncap cycling.

### Session 2026-11-17

- Q: How does protected Aggressive protect another principal now that Performance takes no
  running steps? → A: At admission only (constitution 8.1.0, Principle II, Admission). New own
  work starts on a compute resource only where every other principal keeps the lesser of its
  demand and its fair share. Running own work is never capped, paused or drained for another's
  compute.
- Q: What does Aggressive still change from Performance? → A: An admission margin of 0% instead
  of 2%, and admission fills what fits in one heartbeat. Running behaviour, floors, unknown
  handling and Guaranteed rules are Performance's.
- Q: What does Aggressive do at a memory floor breach? → A: What Performance does (Floors by
  mode). Own memory gets no hint, drain or TERM, and no own task is marked as a preferred OOM
  victim. The floors bound what is admitted, and the kernel decides at exhaustion, as it would
  beside the same own work without CedeGrid.
- Q: What does unprotected Aggressive add? → A: Only the admission exception. It may start new
  compute work where another principal contends, never into activity of unknown amount or the
  floors.
- Q: How does GPU compute behave? → A: As CPU does, with no duty cycle (item 057 Part C). The
  first-come GPU floor is an exception for Yield First and Neutral and is unchanged here.
- Q: What ships in this release? → A: Both forms (user, 2026-11-17, choice 1 A). Unprotected
  Aggressive goes through the SDK and the wire only and gets the security review before release.
- Q: Where is the typed confirmation, with no `run` command yet? → A: In interactive setup, when a
  person turns the host's allowance on: the numbers screen, then the fixed phrase. Each SDK
  submission carries two explicit fields (user, choice 2 A).
- Q: Where is the allowance kept? → A: In the agent's state database, written only by
  interactive setup, so no configuration file can turn it on (user, choice 3 A). It fails closed:
  a missing, corrupt or unreadable database, or one on another schema, counts as off. Turning it
  off needs no confirmation, and restoring a backup resets it to off.
- Q: May a System mode administrator forbid unprotected Aggressive? → A: Yes, with its own
  administrator policy key, off by default, separate from the allowed modes (user, choice 4 A).

## Terms

- **Aggressive**: a fourth contention mode, after Performance. Its default form is
  **protected Aggressive**; **unprotected Aggressive** is the same mode with the protection off.
- **Principal**: a user whose processes use a resource, as in 001. Own work is one principal.
- **Fair share**: a principal's max-min fair share of a compute resource, as in 001.
- **Protected share**: for another active principal, the lesser of its demand and its fair share.
- **Waiting**: a principal that wants more compute than it gets: run-queue wait for CPU, or
  demand the growth probe reveals.
- **Shared-cgroup host**: a host where all users share one CPU cgroup. On a **per-user-cgroup
  host**, each user has their own cgroup.
- **Unprotected grant**: the per-run permission that turns the protection off, with its
  confirmation record. It ends with the run.
- **Allowance**: the host setting "allow unprotected Aggressive", turned on only in interactive
  setup, without which no submission may request unprotected Aggressive.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Use all idle compute and still protect light users (Priority: P1)

An operator with a deadline chooses Aggressive. Own work fills all idle CPU and GPU compute at
once, with no admission margin. Another user who needs only a little compute, or no more than
their fair share, is not crowded out by new own work: new own work starts only where every other
principal keeps the lesser of its demand and its fair share. Work already running is never
capped, paused or drained for another user's compute, as in Performance.

**Why this priority**: it gives the most throughput the constitution allows without new own
work taking anyone below what they need or their fair share.

**Independent Test**: on a simulated and then a test host, submit own work in Aggressive while a
light contender (below its fair share) and then a heavy one (above it) run. With the light
contender, own work is admitted only up to what leaves it its demand; with the heavy one, up to
own work's fair share. On an idle host, own work fills all available capacity within one
heartbeat.

**Acceptance Scenarios**:

1. **Given** an idle host and Aggressive allowed by the node cap, **When** own work is submitted,
   **Then** it is admitted up to all available CPU and GPU compute, with no margin, within the
   first admitting heartbeat.
2. **Given** another principal below its protected share on a compute resource, **When** own work
   in protected Aggressive is submitted, **Then** new own work starts only up to what leaves that
   principal its protected share; the rest waits for admission.
3. **Given** another principal at or above its protected share, **When** own work in protected
   Aggressive is submitted, **Then** it is admitted up to the larger of own work's fair share and
   what the other principals leave, with no margin.
4. **Given** own work already running in either form, **When** another principal's demand grows,
   **Then** no running own task is capped, paused or drained for it: the kernel and the GPU
   driver split the time, as beside the same own work without CedeGrid.
5. **Given** unknown external CPU or GPU activity, **When** own work in Aggressive is submitted,
   **Then** no new own work starts into it, as in Performance.
6. **Given** a memory floor breach, **When** Aggressive runs, **Then** own memory gets no hint,
   drain or TERM and no own task is marked; the floors still bound what is admitted, and the
   kernel decides at exhaustion.
7. **Given** setup, `status`, `observe`, `doctor` and their JSON, **When** Aggressive is chosen or
   active, **Then** each shows a red `[Aggressive]` label. `ps` and `top` show it once 003 adds
   them.
8. **Given** a node cap left at its default, **When** a job requests Aggressive, **Then** it runs
   at the cap with the reason code `mode_capped`; raising the cap to Aggressive is its own
   confirmed step.

---

### User Story 2 - Unprotected Aggressive for one run, explicitly confirmed (Priority: P2)

For one run, an operator turns the protection off. New own work may then start on CPU or GPU
compute that other principals contend for, so they can get slower compute; nothing else
changes. Turning it on takes an explicit per-submission SDK field, a second explicit
confirmation field, and the host's allowance, which a person turns on in interactive setup
after seeing the cost to other users on this host and typing a fixed phrase.

**Why this priority**: some deadlines justify slowing others' compute. The choice must stay
explicit, rare, visible and limited to compute time.

**Independent Test**: submit runs through every path: the SDK with both fields and the allowance
on; with either field missing; with the allowance off; and through a configuration file, an
environment variable and a migrated configuration. Turn the allowance on through interactive
setup with the right phrase, a wrong phrase and a "no", and try to turn it on non-interactively.
Only the permitted path starts an unprotected run.

**Acceptance Scenarios**:

1. **Given** interactive setup, **When** a person answers yes on the numbers screen and types the
   fixed phrase, **Then** the host's allowance turns on and is recorded with its time.
2. **Given** the numbers screen, **When** the person answers no or types anything other than the
   fixed phrase, **Then** the allowance stays off.
3. **Given** an SDK submission with the unprotected field, **When** the second confirmation
   field is missing or the allowance is off, **Then** the submission is refused with a message
   naming what is missing.
4. **Given** a configuration file, an environment variable, an upgrade or a migration that tries
   to enable it, **When** a run starts, **Then** it does not start unprotected; a configuration
   that tries fails to load with a message naming the key.
5. **Given** an unprotected run and a contender at any demand, **When** own work is submitted,
   **Then** it may start where the contender contends, never into activity of unknown amount or
   the floors, and raises no priority and adds no threads.
6. **Given** an unprotected run and a memory floor breach, **When** it happens, **Then** memory
   behaves as in protected Aggressive.
7. **Given** an unprotected run, **When** it starts and while it runs, **Then** one warning line
   is logged at its start, and logs, `status`, `observe` and their JSON show a red `UNPROTECTED`
   marker until the run ends; `ps` and `top` show it once 003 adds them.

---

### Edge Cases

- A contender's demand is unknown on a shared-cgroup host where run-queue wait is unavailable:
  the probe applies as in 001, and admission treats the unknown as 001 does.
- Several light principals contend at once: new own work starts only where each keeps its
  protected share.
- Unknown external activity during an unprotected run: no new own work starts into it.
- The node cap is lowered while an Aggressive run is active: new tasks of the run are admitted at
  the new cap's mode from the next interval, running tasks are not stopped for it, and an
  unprotected grant ends.
- The allowance is turned off while an unprotected run is active: that run keeps its grant until
  it ends; new submissions are refused.
- An unprotected run is drained and retried: its attempts keep the grant; a new run needs new
  fields.
- An upgrade or a migrated configuration contains a key that would enable unprotected
  Aggressive: loading fails with a message naming the key.
- Guaranteed work in Aggressive: it is never paused and keeps its request, as in every mode.

## Requirements *(mandatory)*

### Functional Requirements

**The mode (user story 1)**

- **FR-001**: Aggressive MUST be a fourth contention mode for every resource. For every resource
  and observation, own work's admitted capacity MUST satisfy Yield First ≤ Neutral ≤
  Performance ≤ Aggressive, with no exception (001 FR-065).
- **FR-002**: Aggressive MUST be Performance with two overrides, and otherwise Performance's
  rules (no running compute step, floors, unknown handling, Guaranteed rules):
  - an admission margin of 0% on CPU and GPU compute, where Performance keeps 2%;
  - admission fills what fits in one heartbeat, with no ramp. Performance's anti-flap rules for
    admission stay.
- **FR-003**: Protected Aggressive MUST start new own work on a compute resource only where every
  other active principal keeps the lesser of its demand and its fair share (its protected
  share), and MUST NOT start new own work into activity of unknown amount (constitution 8.1.0,
  Principle II, Admission). Within that, own work is admitted up to the larger of its own fair
  share and what the other principals leave, with no margin.
- **FR-004**: In both forms, CedeGrid MUST NOT cap, pause or drain running own work to give
  another workload more CPU or GPU compute time than the kernel and the GPU driver would give it
  beside the same own work.
- **FR-005**: A principal's demand for FR-003 MUST come from 001's evidence: usage plus the
  growth probe (001 FR-021). On a shared-cgroup host, 003's run-queue wait (003 FR-050) adds to
  it once 003 provides it. Wait alone MUST NOT hold own work below its own fair share.
- **FR-006**: Memory in both forms MUST be Performance's (Principle II, Floors by mode and OOM
  order): the floors bound what is admitted; own memory gets no hint, drain or TERM for a floor
  breach; no own task is marked as a preferred OOM victim; and the kernel decides at exhaustion.
  The confirmation text for unprotected Aggressive MUST say that memory is unchanged by it.
- **FR-007**: Aggressive MUST be a caution option, off by default, with a red `[Aggressive]`
  label in setup, `doctor`, `status`, `observe` and their JSON, and in `ps` and `top` once 003
  adds them. Choosing it MUST use the caution flow with its own confirmation (001 FR-051), with
  its own acknowledgement name for files and non-interactive use.
- **FR-008**: The node cap MUST NOT include Aggressive unless an operator raised it to Aggressive
  in its own confirmed step. Without that step, a node's cap is at most Performance, even when
  its default mode is Aggressive, and a request for Aggressive runs at the cap with
  `mode_capped` (001 FR-030).
- **FR-009**: A job or an SDK submission MAY request protected Aggressive per resource, within
  the node cap. Like Performance, protected Aggressive MAY be stored in configuration and saved
  setup with its acknowledgement; only unprotected Aggressive is never stored (FR-012). `run`
  flags and per-job entries come with 003.

**Unprotected Aggressive (user story 2)**

- **FR-010**: Unprotected Aggressive MUST be protected Aggressive with FR-003's protected-share
  limit off: new own compute work MAY start where another principal contends. It MUST NOT start
  into activity of unknown amount or into the floors, and FR-004, FR-006, Principle I, the OOM
  order, the no-priority-raise rule, Guaranteed rules and the operator envelope apply in full.
- **FR-011**: Only an explicit per-submission SDK field, sent over the wire, MAY turn it on, and
  only where the node cap allows Aggressive. The field MUST have a long, explicit name; final
  names are set in the plan. 003 adds the same per-run flag to `run`.
- **FR-012**: Configuration files, saved setup, profiles, environment variables, upgrades,
  migrations and the node cap MUST NOT enable it. A configuration that tries MUST fail to load
  with a message naming the key.
- **FR-013**: The host's allowance MUST be turned on only in interactive setup, in two steps:
  1. a screen with this host's current numbers (the number of other active principals, their
     current CPU use, and how much a light user would slow down, stated as an estimate) and a
     yes/no question with no default;
  2. typing a fixed phrase. Anything else cancels.
- **FR-014**: A submission requesting unprotected Aggressive MUST be refused unless all hold: a
  second explicit confirmation field is present, and the host's allowance is on. A refusal MUST
  name what is missing.
- **FR-015**: The allowance MUST be off by default and MUST be recorded with the time a person
  turned it on, in the agent's state database. Non-interactive setup, the SDK and agent tooling
  MUST NOT turn it on; any of them MAY turn it off, with no confirmation. It MUST count as off
  when the database is missing, corrupt or unreadable, or its schema is not one this build
  reads, and a restored backup MUST reset it to off.
- **FR-015a**: On a System mode host, the administrator policy MUST have its own key, off by
  default, that permits members to request unprotected Aggressive; while it is off, the agent
  in a member's unit refuses such a request in one line naming the policy, whatever the
  allowance says. The allowed modes list governs protected Aggressive as it governs every mode.
- **FR-016**: While a run is unprotected, logs, `status`, `observe` and their JSON MUST show a red
  `UNPROTECTED` marker, and `ps` and `top` once 003 adds them; one warning line MUST be logged at
  its start. The grant MUST end with the run; a new run needs new fields.

**Both forms**

- **FR-017**: Neither form MAY raise own priority, add threads or do anything else to win share.
  The split under contention is whatever the kernel and the GPU driver decide.
- **FR-018**: GPU compute MUST follow the CPU rules in both forms, with no duty cycle. VRAM floors,
  Principle I and the OOM order MUST be unchanged in both forms.
- **FR-019**: The Python and TypeScript SDKs and the wire contract MUST offer the same Aggressive
  and unprotected fields, and a test MUST fail when they differ; 003's CLI joins that test when
  it adds `run`.
- **FR-020**: Performance MUST NOT change in this feature.

### Key Entities

- **Aggressive mode**: a mode value after Performance, with the protection on by default.
- **Protected share**: per other active principal and compute resource: the lesser of its demand
  and its fair share, with the evidence behind the demand (usage, run-queue wait, probe).
- **Unprotected grant**: per run: the two submission fields and the allowance it was granted
  under, its start time, and its end with the run.
- **Allowance**: on or off, with the time a person turned it on in interactive setup.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In protected Aggressive, own work submitted while another principal contends
  (001's F2c shape) is admitted up to the larger of its fair share and what the other principals
  leave, with no margin, and the other principal stays at or above the lesser of its demand and
  its fair share, in 3 of 3 runs on CPU and on GPU compute.
- **SC-002**: In both forms, running own work gets zero compute caps, pauses or drains for
  another principal's demand, in every scripted scenario and every test-host run.
- **SC-003**: On an idle host, Aggressive admits at least as much CPU and GPU compute as
  Performance at every interval, and reaches all available capacity within one heartbeat after
  admission opens.
- **SC-004**: In unprotected Aggressive, new own work starts where another principal contends
  (and never into unknown activity or the floors), and the memory floors bound admission as in
  001.
- **SC-005**: Every refusal path (configuration, environment, migration, a missing field, the
  allowance off, a missing, corrupt or other-schema state database, a restored backup, the
  System mode policy key off, a wrong phrase, a non-interactive attempt to turn the allowance
  on) refuses
  with a message naming the missing condition, and no run starts unprotected through it.
- **SC-006**: The `[Aggressive]` label and the `UNPROTECTED` marker appear in every listed output
  for every active run, and nowhere when the run is not in that mode.
- **SC-007**: Yield First ≤ Neutral ≤ Performance ≤ Aggressive holds for every resource and
  observation in the policy, contention and random scenarios, with no exception.
- **SC-008**: Test-host gates record Aggressive against Performance (time to fill and idle
  capacity admitted) and the contender's rate and p99 with and without the protection, for a
  same-cgroup and a cross-cgroup contender.

## Assumptions

- Implementation runs in parallel with 001's last tasks and lands with this release. The CLI
  parts (the `run` flags, `--save`, per-job entries, profiles, `ps`, `top`) and run-queue wait
  evidence come with 003; until then, configuration, setup, the SDKs, the wire and the coordinator
  carry the feature.
- Aggressive on RAM and GPU memory behaves as Performance does there: floors at admission only.
- The fixed confirmation phrase, the field names and the acknowledgement name are set in the plan.
- The light-user slowdown on the numbers screen is an estimate from current usage and the fair
  share, stated as an estimate.
