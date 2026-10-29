# Feature Specification: Aggressive Mode for CPU and GPU Compute

**Feature Branch**: `001-contention-modes`, specified alongside it. The spec directory is
independent of the branch. Implementation starts on its own branch, `004-aggressive-mode`, once
001 is merged.

**Created**: 2026-10-26

**Status**: Draft

**Input**: Maintainer feature description: add an Aggressive contention mode for CPU and GPU
compute, protected by default, with an unprotected form behind an explicit per-run flag and a
two-step confirmation.

This feature builds on [001-contention-modes](../001-contention-modes/spec.md): its modes, node
cap and per-job modes (FR-030), Performance (FR-022), probing (FR-021, FR-072), floors (FR-023),
caution options (FR-043, FR-051) and the mode ordering (FR-065). It uses
[003-everyday-cli](../003-everyday-cli/spec.md) for `run`, per-job entries and profiles
(FR-020–FR-026), `ps` and `top` (FR-010, FR-013), the setup interface (FR-030–FR-033) and
per-user run-queue wait (FR-050). The constitution (7.0.0) applies, in particular Principle II's
Aggressive mode and its one named exception, unprotected Aggressive.

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
- Q: How does protected Aggressive give CPU and GPU compute back to a waiting principal below its
  protected share, compared with Performance? → A: It never holds less than Performance in the
  same observation (report 018 Decision 1, answer B). For each compute resource and observation,
  its target is the larger of Performance's target for that observation and its own target (0%
  margin, scale-up at once) limited so that every other active principal keeps its protected
  share. While Performance holds more than the protection leaves, Aggressive equals Performance
  and gives back exactly as Performance does; once Performance has stepped down to the protected
  point or below, Aggressive holds no more than the protection leaves. The mode ordering stays
  strict per observation, with no exception.
- Q: How quickly must a light contender below its fair share reach its demand in protected
  Aggressive? → A: No later than under Performance in the same scenario: 001's SC-012 reclaim
  bounds apply unchanged.
- Q: How much does own work give back when several light principals wait at once? → A: Each keeps
  its protected share; own work goes no lower than the protection needs and never below
  Performance in the same observation.
- Q: What happens when a protected principal stops waiting and later waits again? → A: The same
  rule applies in every observation: own work gives back again as Performance would, never below
  Performance, and Performance's step dwell and hysteresis prevent cap and uncap cycling.

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
- **Global allowance**: the host setting "allow unprotected Aggressive", which only
  non-interactive use needs.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Use all idle compute and still protect light users (Priority: P1)

An operator with a deadline chooses Aggressive. Own work uses all idle CPU and GPU compute at
once, with no margin and no ramp. Another user who needs only a little compute, or no more than
their fair share, still gets it when they wait for it, no slower than Performance would give it
back.

**Why this priority**: it gives the most throughput the constitution allows without slowing
anyone below what they need or their fair share.

**Independent Test**: on a simulated and then a test host, run own work in Aggressive beside a
light contender (below its fair share) and a heavy one (above it). The light contender reaches its
demand; the heavy one gets its fair share, not more.

**Acceptance Scenarios**:

1. **Given** an idle host and Aggressive allowed by the node cap, **When** own work starts,
   **Then** it uses all available CPU and GPU compute, with no idle margin, from the first
   admitting heartbeat.
2. **Given** own work in Aggressive and another principal below its protected share and waiting,
   **When** the policy observes it, **Then** own work gives back as Performance would, never
   holding less than Performance in the same observation, until that principal reaches its
   protected share; once Performance has stepped down to that point or below, own work holds no
   more than the protection leaves.
3. **Given** another principal at or above its protected share, **When** it wants more, **Then**
   own work does not give back, and own work never goes below its own fair share.
4. **Given** a shared-cgroup host, **When** a contender's usage understates its demand, **Then**
   its run-queue wait and the growth probe reveal the demand, and the protection uses it.
5. **Given** unknown external GPU or CPU activity, **When** Aggressive runs, **Then** own work
   holds at its fair share, as Performance does.
6. **Given** a memory floor breach, **When** Aggressive runs, **Then** memory floors are kept
   down to the fair share: own memory above it takes the floor path (hint, then drain), and
   in-share own memory gets the save-progress signal; above the floors, memory gets no hints or
   drains.
7. **Given** setup, `top` and `ps`, **When** Aggressive is chosen or active, **Then** each shows
   a red `[Aggressive]` label.
8. **Given** a node cap left at its default, **When** a job requests Aggressive, **Then** it runs
   at the cap with the reason code `mode_capped`; raising the cap to Aggressive is its own
   confirmed step.

---

### User Story 2 - Unprotected Aggressive for one run, explicitly confirmed (Priority: P2)

For one run, an operator turns the protection off. Own work then never gives CPU or GPU compute
back, while the memory floors and every other rule still hold. Turning it on takes an explicit
per-run flag and a two-step confirmation that shows the cost to other users on this host.

**Why this priority**: some deadlines justify slowing others' compute. The choice must stay
explicit, rare, visible and limited to compute time.

**Independent Test**: start runs through every path: interactive with both steps, interactive
with a wrong phrase, non-interactive with and without the second flag and the global allowance,
and from saved setup, a profile, a config file and an environment variable. Only the two
permitted paths start an unprotected run.

**Acceptance Scenarios**:

1. **Given** an interactive terminal and the unprotected flag, **When** the user answers yes on
   the numbers screen and types the fixed phrase, **Then** the run starts unprotected.
2. **Given** the numbers screen, **When** the user answers no, or types anything other than the
   fixed phrase, **Then** nothing starts unprotected.
3. **Given** a non-interactive start (no terminal, `--json`, the SDK or agent tooling), **When**
   the second confirmation flag is missing or the global allowance is off, **Then** the start is
   refused with a message naming what is missing.
4. **Given** saved setup, a profile, a configuration file or an environment variable that tries to
   enable it, **When** a run starts, **Then** it does not start unprotected; `--save` refuses to
   store it.
5. **Given** an unprotected run and a contender at any demand, **When** the contender waits,
   **Then** own work does not step CPU or GPU compute down, raises no priority and adds no threads.
6. **Given** an unprotected run and a memory floor breach, **When** it happens, **Then** memory
   floors are kept down to the fair share, as in protected Aggressive.
7. **Given** an unprotected run, **When** it starts and while it runs, **Then** one warning line
   prints at start, and `ps`, `top`, logs and JSON show a red `UNPROTECTED` marker until the run
   ends.

---

### Edge Cases

- A protected principal stops waiting, then waits again: own work gives back again as Performance
  would, never below Performance in the same observation; Performance's step dwell and
  hysteresis still prevent cap and uncap cycling.
- Several light principals wait at once: each keeps its protected share. Own work goes no lower
  than the protection needs, never below its own fair share, and never below Performance in the
  same observation.
- A contender's demand is unknown on a shared-cgroup host where run-queue wait is unavailable:
  the probe applies as in 001, and wait alone never drives own work below its fair share.
- Unknown external activity during an unprotected run: own work does not grow into it and does
  not give back.
- The node cap is lowered while an Aggressive run is active: the run continues at the new cap's
  mode from the next interval, and an unprotected grant ends.
- The global allowance is turned off while a non-interactively started unprotected run is
  active: that run keeps its grant until it ends; new non-interactive starts are refused.
- An unprotected run is drained and retried: its attempts keep the grant; a new run needs a new
  flag and confirmation.
- An upgrade or a migrated configuration contains a key that would enable unprotected
  Aggressive: loading fails with a message naming the key.
- Guaranteed work in Aggressive: it is never paused and keeps its request, as in every mode.

## Requirements *(mandatory)*

### Functional Requirements

**The mode (user story 1)**

- **FR-001**: Aggressive MUST be a fourth contention mode for every resource. For every resource
  and observation, own work's target MUST satisfy Yield First ≤ Neutral ≤ Performance ≤
  Aggressive, with no exception (001 FR-065), including while another principal waits below its
  protected share (FR-003).
- **FR-002**: Aggressive MUST be Performance with three overrides, and otherwise Performance's
  rules (reclaim bounds, floors, unknown handling, anti-flap rules, Guaranteed rules):
  - an idle margin of 0% on CPU and GPU compute;
  - scale-up to all available capacity at once, with no ramp: admission fills what fits in one
    heartbeat, and a held compute cap rises straight to what the mode allows once calm. The
    minimum step dwell, hysteresis and reversal tolerance stay Performance's;
  - memory: no hints or drains while above the floors.
- **FR-003**: For each compute resource and observation, protected Aggressive's target MUST be the
  larger of:
  - Performance's target for the same observation;
  - Aggressive's own target (FR-002's 0% margin and scale-up at once), limited so that every
    other active principal that is below its protected share and waiting keeps that share.

  So a waiting principal gets that capacity back no slower than Performance would give it back:
  while Performance holds more than the protection leaves, Aggressive equals Performance and gives
  back exactly as Performance does; once Performance has stepped down to the protected point or
  below, Aggressive holds no more than the protection leaves. Protected Aggressive MUST NOT give
  back to a principal at or above its protected share, and own work MUST NOT go below its own fair
  share.
- **FR-004**: On a shared-cgroup host, a principal's demand MUST come from its run-queue wait
  (003 FR-050) plus the growth probe (001 FR-021). On a per-user-cgroup host, it MUST come from
  its usage plus the probe. Wait alone MUST NOT drive own work below its fair share.
- **FR-005**: While external activity of unknown amount exists on a compute resource, protected
  Aggressive MUST hold own work at its fair share, as Performance does (001 FR-022).
- **FR-006**: At a memory floor breach, both forms MUST keep memory floors down to the fair share
  (001 FR-023): own memory above its fair share takes the floor path (hint, then drain), and
  in-share own memory gets the save-progress signal (001 FR-075). Out-of-memory order by fault
  applies, so the kernel may take another workload's process, or own work as the largest
  process, before own work checkpoints. The confirmation text MUST say so.
- **FR-007**: Aggressive MUST be a caution option, off by default, with a red `[Aggressive]`
  label in setup, `top`, `ps`, `doctor`, `status` and JSON.
  Choosing it MUST use the caution flow with its own confirmation (001 FR-051), with its own
  acknowledgement name for files and non-interactive use.
- **FR-008**: The node cap MUST NOT include Aggressive unless an operator raised it to Aggressive
  in its own confirmed step. Without that step, a node's cap is at most Performance, even when
  its default mode is Aggressive, and a request for Aggressive runs at the cap with
  `mode_capped` (001 FR-030).
- **FR-009**: A job, an SDK submission, a `run` flag or a per-job entry MAY request protected
  Aggressive per resource, within the node cap. Like Performance, protected Aggressive MAY be
  stored in configuration, saved setup and per-job entries, with its acknowledgement; only
  unprotected Aggressive is never stored (FR-012).

**Unprotected Aggressive (user story 2)**

- **FR-010**: Unprotected Aggressive MUST be protected Aggressive with the protection off: own
  work MUST NOT step CPU or GPU compute down for external demand. Floors, Principle I, the
  out-of-memory order by fault, the no-priority-raise rule, Guaranteed rules and the operator envelope still
  apply in full. While external activity of unknown amount exists, own work MUST NOT grow into
  it.
- **FR-011**: Only an explicit per-run flag, or the same explicit per-submission SDK field, MAY
  turn it on, and only where the node cap allows Aggressive. The flag MUST have a long, explicit
  name and no short alias; final names are set in the plan.
- **FR-012**: Configuration files, saved setup, per-job entries, profiles, environment variables,
  upgrades, migrations and the node cap MUST NOT enable it. A configuration that tries MUST fail
  to load with a message naming the key, and `--save` MUST refuse to store it.
- **FR-013**: An interactive start MUST take two steps:
  1. a screen with this host's current numbers (the number of other active principals, their
     current CPU use, and how much a light user would slow down) and a yes/no question with no
     default;
  2. typing a fixed phrase. Anything else cancels.
- **FR-014**: A non-interactive start (no terminal, `--json`, the SDK, agent tooling) MUST be
  refused unless both hold: a second explicit confirmation flag (or SDK field) is present, and
  the global allowance is on. A refusal MUST name what is missing.
- **FR-015**: The global allowance MUST be off by default and MUST gate only non-interactive
  starts. A person MUST turn it on in the interactive setup, with its own confirmation, and it
  MUST be recorded with its time. Non-interactive setup, the SDK and agent tooling MUST NOT turn
  it on.
- **FR-016**: While a run is unprotected, `ps`, `top`, logs and JSON output MUST show a red
  `UNPROTECTED` marker, and one warning line MUST print at its start. The grant MUST end with the
  run; a new run needs a new flag and confirmation.

**Both forms**

- **FR-017**: Neither form MAY raise own priority, add threads or do anything else to win share.
  The split under contention is whatever the kernel and the GPU driver decide.
- **FR-018**: GPU compute MUST follow the CPU rules in both forms. Memory floors (kept down to
  the fair share), VRAM floors, Principle I and the out-of-memory order by fault MUST be
  unchanged in both forms.
- **FR-019**: The CLI and the SDKs MUST offer the same Aggressive and unprotected options, and a
  test MUST fail when they differ.
- **FR-020**: Performance MUST NOT change in this feature.

### Key Entities

- **Aggressive mode**: a mode value after Performance, with the protection on by default.
- **Protected share**: per other active principal and compute resource: the lesser of its demand
  and its fair share, with the evidence behind the demand (usage, run-queue wait, probe).
- **Unprotected grant**: per run: how it was confirmed (interactive phrase, or non-interactive
  flag plus the global allowance), its start time, and its end with the run.
- **Global allowance**: on or off, with the time a person turned it on.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In protected Aggressive, a light contender below its fair share reaches its demand
  no later than it would under Performance in the same scenario, within 001's SC-012 reclaim
  bounds, on CPU and on GPU compute; from then on, own work holds no more than the protection
  leaves.
- **SC-002**: In protected Aggressive, own work never goes below its fair share for a contender
  above its protected share.
- **SC-003**: On an idle host, Aggressive uses at least as much CPU and GPU compute as
  Performance at every interval, and reaches all available capacity within one heartbeat after
  admission opens.
- **SC-004**: In unprotected Aggressive, own work's CPU and GPU compute are stepped down zero
  times for external demand, and the memory floors hold as in 001 (SC-012).
- **SC-005**: Every refusal path (saved setup, profile, per-job entry, configuration,
  environment, missing confirmation, wrong phrase, non-interactive without the allowance or the
  second flag) refuses with a message naming the missing condition, and no run starts
  unprotected through it.
- **SC-006**: The `[Aggressive]` label and the `UNPROTECTED` marker appear in every listed output
  for every active run, and nowhere when the run is not in that mode.
- **SC-007**: Yield First ≤ Neutral ≤ Performance ≤ Aggressive holds for every resource and
  observation in the policy, contention and random scenarios, with no exception, including the
  observations in which another principal waits below its protected share.
- **SC-008**: Test-host gates record Aggressive against Performance (ramp time and idle capacity
  used) and the contender slowdown with and without the protection, for a same-cgroup and a
  cross-cgroup contender.

## Assumptions

- Implementation starts after 001 is merged. The CLI parts (the `run` flags, `--save`, per-job
  entries, profiles, `ps`, `top` and the setup interface) and run-queue wait evidence come from
  003; without them, only the configuration, SDK and coordinator parts apply.
- Aggressive on RAM and GPU memory behaves as Performance does there: floors only.
- The fixed confirmation phrase, the flag names and the acknowledgement name are set in the plan.
- Protected Aggressive gives compute back on Performance's timing: while Performance holds more
  than the protection leaves, the two targets are equal, so 001's SC-012 reclaim bounds apply to
  Aggressive unchanged.
- The light-user slowdown on the numbers screen is an estimate from current usage and the fair
  share, stated as an estimate.
