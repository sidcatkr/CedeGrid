# Feature Specification: Contention Modes with Graduated Yielding

**Feature Branch**: `001-contention-modes`

**Created**: 2026-10-24

**Status**: Draft

**Input**: Maintainer feature description: contention modes (Yield First, Neutral, Performance)
with graduated, reversible yielding for CPU, RAM and GPU (compute and memory, per device), plus
fixes for six reproduced yielding defects. Implementation candidates are recorded separately in
[plan-notes.md](plan-notes.md). Updated the same day with maintainer decisions (see
Clarifications), an interactive-by-default setup with a CLI equivalent, and provisional
thresholds.

## Clarifications

### Session 2026-10-24

- Q: What do configurations without mode settings get, and what happens to `gpu.execution_mode`?
  → A: The default mode is Neutral on every resource. The legacy `gpu.execution_mode` key, and
  `gpu.best_effort_external_processes` with it, are ignored with a deprecation warning that
  names the replacement mode settings.
- Q: May a no-harm floor that only a Guaranteed allocation's memory can restore drain that
  allocation? → A: Yes, by default (floor policy A): after all opportunistic work, and with a
  hint first. Policies B (keep Guaranteed memory) and C (keep Guaranteed memory, and do not mark
  Guaranteed processes as preferred OOM victims) are opt-in choices made during setup.
- Q: May no-harm floors push Neutral's memory below its fair share? → A: Yes, by default (Neutral
  memory policy A). Holding the fair share even against floors (policy B) is an opt-in setup
  toggle. Constitution 1.1.0 allows these opt-in exceptions while keeping A as the default.
- Q: What are the success-criteria thresholds? → A: No measured numbers exist yet. The spec
  proposes conservative defaults, each a configuration value, all provisional until validated on
  real hardware (see Provisional thresholds).
- Q: How is a node set up? → A: Interactive setup is the default way to configure CedeGrid. A
  terminal interface opens on a TTY, and command-line flags provide the same choices for
  scripts, agents and non-TTY use.
- Q: What happens when own work exceeds the operator's resource envelope, or the node's spool
  budget is exhausted? → A:
  - An envelope breach is handled per resource through the yield ladder, opportunistic work
    first. Guaranteed work is lowered, capped and hinted, and drained only as floor policy A
    allows.
  - Spool exhaustion blocks new admission and drains nothing.
  - Explicit operator drain keeps draining every allocation.
- Q: May Guaranteed allocations be paused? → A: No. Guaranteed allocations may be lowered in
  share, capped and hinted, but not paused, because a pause can break their continuity.

## Terms

- **Own work**: allocations that CedeGrid launched and verified, including SDK-managed children.
- **External demand**: all other use of the host's CPU, RAM and GPUs. This includes other users'
  processes and unmanaged processes of the account that runs CedeGrid.
- **Resource**: one of CPU, RAM, GPU compute (per device) or GPU memory (per device).
- **Principal**: a user ID that currently has demand on a resource. The account that runs
  CedeGrid is one principal; its share covers own work plus its unmanaged processes.
- **Fair share**: a work-conserving, equal split of a contended resource among active
  principals. Capacity a principal does not use is available to the others.
- **Margin**: capacity own work leaves unused for external demand to grow into. Margins are set
  per mode and per resource.
- **No-harm floor**: a hard limit that applies in every mode, such as minimum available host RAM
  or minimum free memory per GPU device.
- **Yield step**: one level of the graduated response, applied per allocation and per resource:
  1. lower share or priority
  2. cap
  3. pause
  4. resource hint (a "use less" target sent to cooperative workers)
  5. cooperative drain with checkpoint
  6. termination of the own process
- **Contention mode**: per-resource behavior that decides how much capacity own work may use
  and how fast it gives capacity back. The modes are Yield First, Neutral and Performance.
- **Floor policy**: how no-harm floors treat Guaranteed allocations. A is the default; B and C
  are opt-in exceptions.
- **Neutral memory policy**: whether no-harm floors may push Neutral's memory below its fair
  share. A is the default; B is an opt-in exception.
- **Risk marker**: the label setup shows on an opt-in exception.
  - *caution*: it can make other workloads' allocations fail or slow them, but never ends an
    external process.
  - *danger*: it can let an external process be killed or starved.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Own work no longer yields to itself (Priority: P1)

An operator runs opportunistic GPU and CPU jobs on a node that other workloads also use. Today a
job can drain itself right after launch, drain because of measurement noise, or drain because
another task's measurement is briefly missing. After this change, a job yields only when external
demand needs the capacity.

**Why this priority**: today these failures throw away work and requeue it in a loop. Every other
story depends on correct inputs to yielding.

**Independent Test**: replay the six reproduced scenarios (D1–D6, pinned in the policy test
suite) and launch a GPU job on a device with existing unattributed memory. No drain or
termination results.

**Acceptance Scenarios**:

1. **Given** an empty device with a few hundred MiB of memory not attributed to any process,
   **When** a job launches and is observed for the first time, **Then** that observation does not
   count as external growth and the job keeps running.
2. **Given** a running job on a device with ample free memory, **When** unattributed device
   memory changes by less than the configured tolerance (default 256 MiB), **Then** no protection
   event occurs and the job's step does not change.
3. **Given** a running job, **When** its own driver context or allocator memory grows, **Then**
   that memory is attributed to own work, not to external demand.
4. **Given** two running jobs whose summed CPU measurement is slightly higher than the host
   measurement for one interval, **When** the policy evaluates, **Then** neither job is drained
   and the inconsistency is reported.
5. **Given** several running jobs, **When** one job's usage is unknown for one interval, **Then**
   unrelated jobs are not drained. Expansion is held until the measurement recovers or the
   bounded unknown window ends.
6. **Given** a Guaranteed job, **When** an observation or control error occurs, **Then** the job
   is not terminated because of that error.

---

### User Story 2 - Graduated, reversible yielding (Priority: P1)

When external demand rises, own work gives back capacity in the smallest effective step instead
of being drained whole. When demand recedes, own work gets the capacity back.

**Why this priority**: a full drain is today's only response, so short bursts of external demand
cost whole jobs. Graduated steps keep own work running and give external demand its capacity
quickly.

**Independent Test**: on a CPU-only node, start an external CPU load next to a running job and
watch the job's step move through lower share and cap (and pause, if needed), then reverse
after the load stops. The job is never drained or terminated.

**Acceptance Scenarios**:

1. **Given** a running job and rising external CPU demand, **When** the contended amount can be
   freed by lowering share, capping or pausing, **Then** the job is neither drained nor
   terminated.
2. **Given** a running job holding RAM or GPU memory that external demand needs, **When** the
   policy must free memory, **Then** the job first receives a resource hint, and is drained only
   if the memory is not freed within a bounded time.
3. **Given** a job at a yield step above zero, **When** external demand falls and stays below the
   reversal threshold, **Then** its steps are undone in reverse order at the mode's scale-up rate.
4. **Given** external demand that oscillates around a threshold, **When** the policy evaluates
   repeatedly, **Then** step changes are damped and do not flap every interval.
5. **Given** several own jobs and a contended resource, **When** capacity must be freed, **Then**
   the policy picks the jobs whose reduction frees the contended resource at the least cost:
   least progress lost, best fit, opportunistic before Guaranteed. Ties break deterministically,
   not by ID order alone.
6. **Given** one node, **When** the policy decides on yield steps, **Then** exactly one per-node
   authority makes the decision and no supervisor acts on a contradicting local decision.

---

### User Story 3 - Choose a contention mode per resource (Priority: P1)

An operator sets each resource to Yield First, Neutral or Performance, and the node behaves the
same way across CPU, RAM and GPU.

**Why this priority**: different nodes and jobs need different trade-offs between own throughput
and the room left for other workloads. The trade-off has to be explicit and predictable.

**Independent Test**: for each mode on a single resource, apply a scripted external load and
compare own usage over time with that mode's definition, including unknown and occupied states.

**Acceptance Scenarios**:

1. **Given** Yield First, **When** external demand rises by any measurable amount beyond noise,
   **Then** own work steps down promptly, to zero if needed, keeping the mode's generous margin
   free. **When** activity is unknown, **Then** it counts as external demand.
2. **Given** Neutral and one contending principal, **When** both principals want more than the
   capacity, **Then** own work converges to its fair share and does not yield below it, except
   where a no-harm floor requires it (see FR-021). External demand can always reclaim anything
   above own work's fair share. Unknown activity counts as one active principal but does not
   block admission.
3. **Given** Performance on an idle host, **When** own work ramps up, **Then** it uses all
   capacity external demand is not using, minus a small safety margin, and scales up quickly.
   **When** external demand grows, **Then** own work yields what is needed to restore the margin
   and floors.
4. **Given** any mode, **When** a no-harm floor would be crossed, **Then** own work steps down
   until the floor holds, whatever the mode would otherwise allow.
5. **Given** any mode, **When** CedeGrid acts, **Then** it acts only on own processes. No external
   process is signalled, reprioritized, throttled, paused, moved or adopted.

---

### User Story 4 - Cooperative workers receive "use less" targets (Priority: P2)

A worker built on the Python or TypeScript SDK learns a per-resource target, such as "keep RAM
below X", and can shrink itself instead of being drained.

**Why this priority**: memory can only be released by the process holding it. Hints turn many
memory drains into in-place reductions.

**Independent Test**: run an SDK worker under rising external memory demand. Check that it
receives a target, that a worker which complies is not drained, and that a worker which ignores
the target is drained after the bounded wait.

**Acceptance Scenarios**:

1. **Given** an SDK worker, **When** the policy sets a resource target for its allocation,
   **Then** the worker can read the target and be notified of changes, in both SDKs, including in
   SDK-managed children.
2. **Given** a target that later relaxes, **When** the worker next checks, **Then** it sees the
   relaxed target.
3. **Given** a worker built against the previous SDK release, or a plain command without the SDK,
   **When** the policy would send a hint, **Then** the worker keeps today's drain behavior and the
   hint step is recorded as unavailable for that allocation.

---

### User Story 5 - GPU work on shared, occupied devices (Priority: P2)

An operator can run GPU work on a device that already hosts other users' idle or active
contexts, when the mode and the free memory allow it.

**Why this priority**: today, admission effectively requires an empty device. Occupied devices
never qualify, so their free capacity goes unused.

**Independent Test**: on a device with an idle external context holding part of its memory, a
GPU job is admitted and runs to completion under Neutral and under Performance.

**Acceptance Scenarios**:

1. **Given** a device with external contexts whose memory leaves enough room above the floor,
   **When** a GPU job requests memory that fits under Neutral or Performance, **Then** the job is
   admitted even though the device is occupied.
2. **Given** Yield First, **When** a device has external compute activity, or unknown activity,
   **Then** no new own GPU work is admitted to that device.
3. **Given** a device with an external context that has shown no compute activity for a
   sustained window while observation was healthy, **When** the policy classifies the device,
   **Then** the context counts as quiet. Activity is reported as quiet, active or unknown, never
   as idle by assumption.
4. **Given** a node, **When** the coordinator places work, **Then** placement honors each
   resource's admission state on that node, not only a single node-wide flag.

---

### User Story 6 - Configure modes safely across versions (Priority: P2)

An operator sets a node default mode per resource, lets jobs override it, and can cap the most
aggressive mode a node allows. Existing configurations and mixed-version deployments keep
working.

**Why this priority**: the feature has to be adopted in place without breaking running
deployments.

**Independent Test**: load every 0.2 example config unchanged. Submit jobs with and without
overrides to a node with a cap. Run the existing distributed scenarios with one node on the
previous release.

**Acceptance Scenarios**:

1. **Given** a 0.2 configuration without mode settings, **When** it is loaded, **Then** it loads
   unchanged and every resource runs in Neutral.
2. **Given** a configuration that sets `gpu.execution_mode` or
   `gpu.best_effort_external_processes`, **When** it is loaded, **Then** it still loads, the
   legacy keys have no effect, and a deprecation warning names the replacement mode settings.
3. **Given** a node capped at Neutral, **When** a job requests Performance, **Then** the job runs
   at Neutral and the effective mode is reported.
4. **Given** a coordinator and an agent on different releases, **When** they exchange heartbeats
   and assignments, **Then** both keep operating. Features the older peer lacks stay off, and no
   message is rejected because of a field the peer does not know.
5. **Given** a Guaranteed job, **When** contention requires capacity back, **Then** the job may
   be lowered in share, capped or hinted, and is drained only under floor policy A, as FR-024
   defines.

---

### User Story 7 - Set up a node interactively or from the command line (Priority: P2)

An operator runs setup and is guided through the choices: node role, per-resource modes, node
cap, floor policy and Neutral memory policy. On a terminal the default is an interactive,
keyboard-driven interface. Scripts, agents and non-TTY sessions pass the same choices as
command-line flags. Either way, the result is written to the configuration file and can be
changed later.

**Why this priority**: modes and opt-in exceptions are only safe if operators see what each
choice does before choosing it. Convenient setup is also the main way new nodes get configured.

**Independent Test**:
- Run setup on a TTY using only the keyboard, and check the written configuration.
- Run it again with the same choices as flags in a non-TTY session; the configuration must be
  identical.
- Check that the interactive run shows the risk markers and that a danger choice asks for
  confirmation.

**Acceptance Scenarios**:

1. **Given** a TTY, **When** the operator runs setup without flags, **Then** the interactive
   interface opens:
   - Arrow keys move the selection, Space toggles or selects, Enter confirms.
   - No mouse is needed.
   - Defaults are preselected.
2. **Given** the floor-policy and Neutral-memory-policy choices, **When** they are displayed,
   **Then**:
   - Each option shows a one-line consequence.
   - Default options are uncolored and unmarked.
   - Caution options are yellow and labelled `[caution]`; danger options are red and labelled
     `[danger]`.
3. **Given** `NO_COLOR` is set, or the terminal cannot show color, **When** the options are
   displayed, **Then** no color is used and the `[caution]` and `[danger]` labels still appear.
4. **Given** the operator selects a danger option, **When** they press Enter, **Then** a separate
   confirmation step states the consequence. The choice is kept only after explicit
   confirmation; declining returns to the selection with the default still in place.
5. **Given** a non-TTY session, **When** setup runs, **Then** it never prompts or waits for
   input. It uses the flags and any existing configuration. If a required choice is missing, or a
   danger option lacks its explicit acknowledgement flag, it exits with an error that names the
   missing flag.
6. **Given** a completed setup, **When** the operator confirms the summary, **Then** the choices
   are validated and written to the configuration file atomically. Other settings are preserved.
   Re-running setup starts from the current values, and editing the file by hand has the same
   effect as setup.
7. **Given** setup is cancelled (Escape, `q` or Ctrl-C), **When** it exits, **Then** nothing is
   written and the terminal is restored.
8. **Given** a TTY and no configuration file, **When** the operator runs a command that needs
   one, **Then** CedeGrid offers to start setup. It never does this on a non-TTY.

---

### User Story 8 - See what the node is doing (Priority: P3)

An operator inspects a node and sees, per resource, the mode, own usage, external demand, the
target share, the current yield step and a short reason.

**Why this priority**: graduated yielding is only trustworthy if it can be explained. It is also
not needed to deliver the behavior itself.

**Independent Test**: during a scripted contention run, compare `observe`, `doctor`, `status` and
node reports with the expected per-resource values at each step.

**Acceptance Scenarios**:

1. **Given** a node under contention, **When** an operator runs `observe`, `doctor` or `status`,
   or reads a node report, **Then** each resource shows: mode (configured and effective), own
   usage, external demand, target share, current yield step per allocation, and concise reasons.
2. **Given** a step change, **When** the next report interval passes, **Then** the change and its
   reason are visible.

---

### Edge Cases

- **Own process exits but the driver still shows its memory for one interval.** The release is
  not counted as external growth.
- **External demand exceeds capacity even with own work at zero.** Own work stays at zero,
  admission is blocked, and the condition is reported. No further action is possible or taken.
- **A floor cannot be met even with own work at zero.** Admission is blocked for that resource
  and the condition is reported.
- **Owners of external demand cannot be identified** (for example, process ownership is hidden).
  That demand counts as one additional principal for fair-share purposes.
- **An external process appears and disappears every interval.** Steps are damped and do not
  oscillate every interval.
- **A GPU device disappears or its observation fails.** Own work on that device steps down under
  the unknown policy. Other devices are unaffected.
- **The per-node authority restarts while supervisors keep running.** Supervisors hold their
  current step. Steps are re-established once the authority returns, and no allocation is
  terminated only because the authority restarted.
- **The coordinator is unreachable.** The existing lease rules apply. Local graduated yielding
  continues.
- **A worker ignores hints.** After the bounded wait, the ladder escalates to drain, then to
  termination.
- **A job is a plain command, not SDK-based.** The hint step is unavailable. Memory contention
  escalates from the steps that do not free memory straight to drain.
- **A Guaranteed job exceeds its own request.** It is charged at its observed usage. The response
  follows its class rules and FR-057, not a node-wide stop.
- **A finer-grained control is unavailable on the host** (for example, no delegated control
  subtree). The ladder uses the steps that are available and reports which ones it skipped.
- **Several own jobs share one GPU device.** Steps and targets are per allocation. The device's
  memory floor covers all of them together.
- **Danger option chosen non-interactively without its acknowledgement flag.** Setup refuses and
  writes nothing.
- **Configuration edited by hand to enable a danger option.** Loading fails unless the file
  also contains the acknowledgement entry that names the option. With the entry, CedeGrid loads
  it, logs a warning that names the exception, and reports it as active.
- **Terminal too small or lacking cursor control for the interface.** Setup falls back to
  line-based prompts on a TTY; it never hangs.
- **Caution and danger options combined** (for example, Neutral memory policy B with floor
  policy C). The summary shows the combination with the highest marker, danger.

## Requirements *(mandatory)*

### Functional Requirements

**Process safety (all modes)**

- **FR-001**: CedeGrid MUST act only on processes it launched and verified. It MUST NOT signal,
  change the priority of, throttle, pause, move into a control group, or adopt any other
  process, including other users' processes and unmanaged processes of the same account.
- **FR-002**: Every actuation mechanism added by this feature MUST have automated tests showing
  it cannot reach a process outside the verified set, including after PID reuse and supervisor
  restart.
- **FR-003**: CedeGrid MUST NOT raise own work's priority above the default.

**Measurement correctness**

- **FR-004**: The first observation of an allocation after launch MUST establish a baseline. It
  MUST NOT count as external growth.
- **FR-005**: Changes in unattributed GPU memory smaller than a configurable tolerance (default
  256 MiB per device) MUST NOT count as a protection event.
- **FR-006**: Memory that belongs to own processes, including driver context and allocator
  overhead, MUST be attributed to own work. Measurement error MUST be reported as a separate
  "unattributed" quantity, not as external demand. Any yield step triggered by external growth
  MUST require growth beyond the known measurement error.
- **FR-007**: Inconsistencies between own and host measurements within the known sampling error
  MUST NOT zero a resource's budget or trigger a drain. They MUST be reported.
- **FR-008**: An unknown measurement for one allocation or one resource MUST affect only that
  allocation's or resource's admission and steps. In every mode it MUST first hold the current
  step and block expansion. It MAY escalate only after a bounded, configurable unknown window,
  and MUST NOT by itself drain unrelated work.
- **FR-009**: GPU memory headroom MUST account for memory the device reserves for itself.
- **FR-010**: Each external GPU context MUST be classified as quiet, active or unknown. Quiet
  requires positive evidence over a sustained, configurable window of healthy observation.

**Graduated yielding**

- **FR-011**: Yielding MUST use the ladder in order: lower share or priority, cap, pause, resource
  hint, cooperative drain, termination.
  - A step that cannot free the contended resource, or is unavailable for the allocation or
    host, MUST be skipped, and the skip MUST be reported.
  - Memory contention MUST start at the hint step.
- **FR-012**: Drain MUST be used only when the lower steps cannot free the contended resource
  within a bounded time. Termination MUST be used only after drain fails within its deadline.
- **FR-013**: Steps 1–4 MUST be reversible. When external demand recedes past a reversal
  threshold with hysteresis, steps MUST be undone in reverse order at the mode's scale-up rate.
- **FR-014**: Pause MUST be bounded in time. A pause that does not resolve contention within the
  bound MUST escalate to the next step.
- **FR-015**: Each node MUST have exactly one authoritative policy state that decides yield steps.
  Supervisors MUST carry out that authority's steps and enforce deadlines. They MUST NOT make
  contradicting yield decisions. When the authority is lost, supervisors MUST hold the current
  step until the existing lease rules apply.
- **FR-016**: Victim selection MUST minimize cost. In order of preference:
  1. allocations whose reduction frees the contended resource
  2. pending before running
  3. opportunistic before Guaranteed
  4. least progress lost since the last checkpoint
  5. best fit to the amount that must be freed

  Remaining ties MUST break deterministically.
- **FR-017**: Observation or control errors MUST hold the current step and block expansion. They
  MUST NOT by themselves terminate Guaranteed work. For opportunistic work they MAY escalate only
  through the bounded unknown policy (FR-008).
- **FR-018**: Yield decisions MUST carry a cause that is preserved from the per-node authority to
  the coordinator. This lets the coordinator tell a yield from a failure for retry accounting.

**Modes**

- **FR-019**: Operators MUST be able to set Yield First, Neutral or Performance independently for
  CPU, RAM, GPU compute and GPU memory. The semantics of each mode MUST be the same across
  resources.
- **FR-020**: Yield First MUST:
  - limit own work to capacity external demand is not using, minus a generous margin;
  - step down within the mode's response time on any external increase beyond the measurement
    tolerance, to zero if needed;
  - count unknown activity as external demand;
  - scale up slowly.
- **FR-021**: Neutral MUST:
  - let own work use idle capacity;
  - converge own work to its fair share under contention, and not step it below fair share
    except to keep a no-harm floor;
  - let external demand reclaim anything above fair share;
  - count unknown activity as one active principal without blocking admission.

  Under the default Neutral memory policy A, no-harm floors take precedence over fair share for
  RAM and GPU memory. Under the opt-in policy B, Neutral keeps its fair share of memory even when
  a floor is breached (see FR-042).
- **FR-022**: Performance MUST:
  - let own work use all capacity external demand is not using, minus a small safety margin;
  - scale up quickly;
  - when external demand grows, yield what is needed to restore the margin and floors within the
    mode's response time.
- **FR-023**: No-harm floors MUST apply in every mode:
  - a minimum available host RAM;
  - a minimum free memory per GPU device, so that other workloads' allocations do not fail;
  - own processes marked as the preferred out-of-memory victims where the platform allows an
    unprivileged process to mark itself.
- **FR-024**: Guaranteed allocations MAY be lowered in share, capped and hinted, and MUST NOT be
  paused. Automatic yielding MUST NOT drain them, except under the default floor policy A. In
  that case a Guaranteed allocation MAY be drained when a no-harm floor can only be restored by releasing
  its memory, but only after all opportunistic work has been released and a hint's deadline has
  passed. Floor policies B and C are defined in FR-041. Explicit operator drain keeps today's
  behavior.
- **FR-025**: Every threshold that steers yielding MUST be a configuration value, not a
  hard-coded constant. This covers:
  - margins, response times and scale-up rates
  - tolerances, windows and deadlines
  - convergence targets

  Each is set per resource where it applies, defaults to the provisional value in this spec, and
  is documented. The Yield First margin MUST be larger than the Performance margin.

**GPU sharing and placement**

- **FR-026**: GPU work MUST be admissible to a device that hosts external contexts when the mode
  allows it and the requested memory fits above the device's floor. An empty device MUST NOT be
  required under Neutral or Performance.
- **FR-027**: Under Yield First, new own GPU work MUST NOT be admitted to a device with active or
  unknown external compute activity.
- **FR-028**: Node reports MUST state admission per resource. Coordinator placement MUST honor it.
  Admission for a resource MUST be blocked while own work on that resource is above step zero.
- **FR-029**: Nodes MUST advertise only the control capabilities they can actually apply.

**Configuration and compatibility**

- **FR-030**: A node MUST have a default mode per resource. A job or task MAY override it per
  resource. A node MAY set a cap on the most aggressive mode allowed (Yield First < Neutral <
  Performance). A job runs at the less aggressive of its requested mode and the node cap, and
  the effective mode MUST be reported.
- **FR-031**: Existing `config_version = 1` files MUST load unchanged.
  - Without mode settings, every resource MUST run in Neutral.
  - `gpu.execution_mode` and `gpu.best_effort_external_processes` MUST still be accepted. They
    MUST have no effect, and MUST produce a deprecation warning at load time that names the
    replacement mode settings.
- **FR-032**: Agents, coordinators and SDK workers from 0.2 and from this release MUST
  interoperate in any combination. A peer MUST NOT reject a message because of fields it does not
  know. Features unsupported by either peer MUST stay off.
- **FR-033**: Recorded snapshots and policy inputs from 0.2 MUST remain replayable.
- **FR-034**: The CPU/RAM scale-up timing MUST be configured independently of GPU settings.

**Cooperative resource targets**

- **FR-035**: Both SDKs MUST let a worker read its current per-resource targets and be notified
  when they change, including in SDK-managed children. Targets can tighten and relax.
- **FR-036**: Workers that do not support targets MUST keep today's drain behavior unchanged.
- **FR-037**: A hint MUST carry a deadline. When the deadline passes without compliance, the
  ladder escalates.

**Observability**

- **FR-038**: For each resource, `observe`, `doctor`, `status` and node reports MUST show:
  - configured and effective mode
  - own usage
  - external demand
  - unattributed usage
  - target share
  - current yield step per allocation
  - concise reasons
- **FR-039**: Every reason MUST be one sentence and carry a stable code that tools and tests can
  rely on.

**Defect regression**

- **FR-040**: The characterization tests for D1–D6 MUST be inverted to assert the corrected
  behavior. Each MUST pass.

**Opt-in floor exceptions**

- **FR-041**: Floor policy MUST be selectable as:
  - **A** (default): FR-024 applies.
  - **B** (opt-in, *caution*): Guaranteed allocations are never drained to restore a floor.
    CedeGrid releases opportunistic work, hints Guaranteed work, blocks admission and reports
    the breach. Own processes remain preferred OOM victims.
  - **C** (opt-in, *danger*): as B, and Guaranteed processes are not marked as preferred OOM
    victims.
- **FR-042**: Neutral memory policy MUST be selectable as:
  - **A** (default): FR-021 applies, with floors first.
  - **B** (opt-in, *caution*): Neutral keeps its fair share of RAM and GPU memory even when a
    floor is breached. Own processes remain preferred OOM victims unless floor policy C is also
    chosen.
- **FR-043**: Opt-in exceptions:
  - MUST be off by default;
  - MUST NOT be enabled by upgrades, migrations or defaults;
  - MUST be recorded in configuration;
  - MUST appear in `doctor`, `status` and node reports while active, with their risk marker.
- **FR-044**: No exception MAY permit acting on an external process or raising own priority.
- **FR-045**: The risk markers are justified as follows. A marker is assigned by the worst
  plausible effect on other workloads.

  | Option | Marker | Justification |
  |---|---|---|
  | Floor policy A, Neutral memory policy A | none | Floors hold in every mode. Own work is released before others are affected. |
  | Floor policy B | *caution* (yellow) | Host RAM stays protected: own processes are still preferred OOM victims, so memory exhaustion ends own work first. GPU memory has no such backstop, so other workloads' new GPU allocations can fail while Guaranteed work keeps its memory. No external process is ended by this choice. |
  | Neutral memory policy B | *caution* (yellow) | Other workloads always keep at least their fair share. Allocations above it can fail or face memory pressure, but the OOM preference still ends own work first. |
  | Floor policy C | *danger* (red) | With own Guaranteed processes no longer preferred OOM victims, the kernel can kill another user's process or leave it starved of memory. |
  | Any combination including C | *danger* (red) | A combination takes its highest marker. |

**Setup**

- **FR-046**: Interactive setup MUST be the default way to configure a node. Running setup on a
  TTY without flags MUST open the terminal interface. On a TTY, a command that needs a missing
  configuration file MUST offer to start setup.
- **FR-047**: Setup MUST cover:
  - node role
  - per-resource default modes
  - node mode cap
  - floor policy
  - Neutral memory policy
  - the thresholds an operator is likely to tune (margins and tolerances)

  The remaining thresholds keep their defaults unless changed by flags or in the file.
- **FR-048**: The interface MUST be usable with the keyboard alone: arrow keys move, Space toggles
  or selects, Enter confirms. It MUST NOT require a mouse.
- **FR-049**: Each floor-policy and Neutral-memory-policy option MUST show a one-line consequence.
  - Default options MUST be uncolored and unmarked.
  - *Caution* options MUST be yellow, and *danger* options red.
  - Every marked option MUST also show the text label `[caution]` or `[danger]`, so color is
    never the only signal.
- **FR-050**: Setup MUST NOT emit color in any of these cases: `NO_COLOR` is set to a non-empty
  value, output is not a terminal, or the terminal does not support color. Text labels MUST
  remain.
- **FR-051**: Choosing a *danger* option MUST require a separate, explicit confirmation that
  restates the consequence:
  - in the interface, a separate confirmation step;
  - in non-interactive use, an acknowledgement flag that names the option;
  - in a hand-edited file, an acknowledgement entry that names the option.
- **FR-052**: Setup MUST offer every interactive choice as a command-line flag. On a non-TTY it
  MUST NOT prompt or block. It MUST:
  - take values from flags and the existing configuration;
  - exit with a non-zero status and name the missing flag when a required choice is absent;
  - offer machine-readable output for scripts and agents.
- **FR-053**: Before writing, setup MUST show a summary that includes every active exception and
  its marker, and MUST validate the result like any configuration load. It MUST write the file
  atomically and preserve all settings it does not manage.
- **FR-054**: Re-running setup MUST start from the current configuration. Hand edits MUST have
  the same effect as setup choices. A hand-enabled *danger* option without its acknowledgement
  entry MUST fail to load with an error naming the missing entry. With the entry, it MUST
  produce a load-time warning naming the option.
- **FR-055**: Cancelling setup MUST write nothing and MUST restore the terminal. If the interface
  cannot drive the terminal, setup MUST fall back to line-based prompts.

**Documentation**

- **FR-056**: `docs/guarantees.md` MUST describe the no-harm floors, the floor and Neutral memory
  policies with their markers, the envelope and spool behavior, and the deprecation of
  `gpu.execution_mode`, in the same change that implements them.

**Operator limits**

- **FR-057**: When own work exceeds the operator's configured resource envelope, CedeGrid MUST
  restore the envelope per resource through the yield ladder.
  - Opportunistic work is released first, by least cost.
  - Guaranteed work MUST NOT be drained except as floor policy A allows (FR-024).
  - An envelope breach MUST NOT stop every allocation at once.
- **FR-058**: When the node's spool budget is exhausted, CedeGrid MUST block new admission and
  report the condition. It MUST NOT drain allocations because of it. Explicit operator drain
  keeps draining every allocation.

### Key Entities

- **Contention mode**: Yield First, Neutral or Performance. Configured per resource as a node
  default, a job override and a node cap. The effective mode is derived per allocation.
- **Resource view**: per node and resource, the capacity, own usage, external demand by
  principal, unattributed usage, floor, margin, fair share and admission state.
- **Principal**: a user ID with current demand on a resource. The CedeGrid account is one
  principal.
- **Yield step**: per allocation and resource, the current ladder level, its cause, when it was
  entered, and which steps are unavailable.
- **Resource target**: per allocation and resource, the "use less" amount, its sequence number
  and deadline, as delivered to cooperative workers.
- **Yield cause**: why a step was taken, such as external growth, floor breach, unknown
  measurement, operator drain or envelope breach. It travels with the decision to the
  coordinator.
- **Setup choices**: the operator's selections and their risk markers, written to configuration.
  They include role, modes, node cap, floor policy, Neutral memory policy and tuned thresholds.

## Success Criteria *(mandatory)*

### Measurable Outcomes

Every threshold below is a configuration value (FR-025). The listed defaults are **provisional,
to validate on real hardware**; see Provisional thresholds for the rationale.

- **SC-001**:
  - Across 100 consecutive launches on a shared GPU device and the D1/D2 replay scenarios: zero
    self-drains caused by the first observation after launch or by unattributed memory changes
    below the tolerance (default 256 MiB).
  - In the D5/D6 replay scenarios: zero drains caused by a single interval of measurement skew
    or one missing allocation measurement.
- **SC-002**: In Yield First, own CPU usage falls to within the Yield First margin within the
  Yield First response time of external CPU demand appearing. The defaults are a 25% margin and
  3 monitor intervals. No work is drained or terminated when CPU is the only contended resource.
- **SC-003**: In Neutral, with one contending principal, own share converges to the fair share
  within the Neutral convergence time. The fair share is 50% here; the defaults are a tolerance
  of ±15 percentage points and a convergence time of 60 s.
- **SC-004**: In Performance:
  - On an idle host, own work reaches the Performance ramp target within the ramp time. The
    defaults are 80% of available capacity minus the margin, within 30 s.
  - When an external workload starts, it gets its demanded usage within the reclaim time. The
    defaults are 5 s for CPU and GPU compute and 15 s for RAM and GPU memory.
- **SC-005**: In every mode with default policies:
  - Zero signals, priority changes or control-group changes are applied to external processes,
    as verified by an audit of every actuation in the test and pressure suites.
  - Zero external out-of-memory kills or external GPU allocation failures attributable to
    CedeGrid in the pressure suite.
- **SC-006**: A GPU job runs to completion on a device already occupied by an external context,
  under Neutral and under Performance, with native evidence on at least one supported NVIDIA
  device.
- **SC-007**: When CPU is the only contended resource, zero drains or terminations occur in any
  mode.
- **SC-008**:
  - 100% of 0.2 example configurations and recorded policy inputs load and replay.
  - Every combination of 0.2 and new agent and coordinator completes the existing distributed
    scenarios.
- **SC-009**: Under external load that switches on and off every 4 monitor intervals, own work
  changes step at most once per switch. Drains occur only when a lower step cannot free the
  resource.
- **SC-010**: Every step change and its reason appear in `observe`, `status` and the node report
  within one report interval.
- **SC-011**: Setup with flags and setup through the keyboard interface produce byte-identical
  configuration for the same choices.
  - On a non-TTY, setup never waits for input.
  - Every *danger* choice needs an explicit confirmation or acknowledgement flag.
  - With `NO_COLOR` set, no color escape sequence is emitted.

### Provisional thresholds

All values are **provisional, to validate on real hardware**. Each is a configuration value; the
times assume the default 500 ms monitor interval.

| Threshold | Default | Rationale |
|---|---|---|
| GPU memory noise tolerance | 256 MiB per device | Larger than typical driver and context jitter, and small next to common device sizes. It was the maintainer's proposal. |
| Yield First margin | 25% of schedulable capacity per resource | Generous by design: it leaves a quarter of each resource free for other workloads to burst into. |
| Yield First response time | 3 monitor intervals (1.5 s) | One interval to observe new demand (CPU usage is a per-interval average), one to confirm it beyond noise, one to apply the step and measure it. The originally proposed 2 intervals leave no confirmation interval. |
| Yield First scale-up | 1 step per 30 s | Matches today's 30 s scale-up cooldown, which is the most conservative existing default. |
| Neutral scale-up | 1 step per 10 s | Reaches the fair share well inside the 60 s convergence time while staying damped. |
| Performance scale-up | 1 step per 2 s | One heartbeat interval per step, which supports the 30 s ramp target. |
| Neutral convergence time and tolerance | 60 s, ±15 percentage points | Share changes go through damped steps; memory relies on cooperative hints. Twice the originally proposed 30 s and ±10 leaves room for both until measured. |
| Performance margin | 10% of schedulable capacity (CPU, GPU compute), plus the floors (memory) | Small, but above the known sampling error of a few percent, so measurement noise does not cause yields. |
| Performance ramp | 80% of available minus margin within 30 s | Admission advances on the 2 s heartbeat and scale-up is damped. The originally proposed 90% in 10 s is unproven. |
| Reclaim time: CPU, GPU compute | 5 s | A few intervals plus step application. The response does not depend on worker cooperation. |
| Reclaim time: RAM, GPU memory | 15 s | Hint deadline (5 s), drain timeout (3 s) and termination grace (2 s), plus margin for observation. |
| Hint deadline | 5 s | Long enough for a cooperative worker to reach a safe point; short enough to bound memory reclaim. |
| Unknown-measurement window | 3 monitor intervals | Matches today's freshness limit of 3 intervals for kernel observations. |
| Step hysteresis | Reverse only below 50% of the trigger threshold; at least 3 intervals between step changes | Damps oscillation (SC-009) without delaying a real response by more than one confirmation interval. |
| Pause bound | 10 s | Longer pauses risk worker and lease timeouts, so contention escalates to the next step instead. |
| GPU quiet window | 20 monitor intervals (10 s) of healthy observation with no activity | Activity samples are only produced for non-zero use, so quiet must be shown by sustained absence over a complete inventory. |
| No-harm floors | Existing reserves: `ram.reserve_mib` / `ram.reserve_percent` (8 GiB / 5%) and `gpu.reserve_vram_mib` (3 GiB) | Reusing the existing settings keeps 0.2 configurations meaningful and their values unchanged. |

## Assumptions

- Graduated controls and their native evidence target Linux. Other platforms keep today's
  CPU-only local supervision, with the ladder limited to the steps they support, and report
  which steps are unavailable.
- CedeGrid runs unprivileged. Finer-grained controls are used only where the account already has
  them, such as an explicitly delegated control subtree. Without them, the ladder relies on the
  steps an unprivileged process can apply to its own processes.
- External demand is attributed to principals by process owner where it is visible. Demand whose
  owner cannot be determined counts as one additional principal.
- The account running CedeGrid is one principal. Own work's fair share is that principal's share
  minus the account's unmanaged usage.
- Mode settings take effect when the agent restarts or reloads its configuration. A job's
  effective mode is fixed when it is admitted.
- Interactive setup targets Linux and macOS terminals. On Windows, setup configures client
  settings only, consistent with today's platform scope.
- Agent-facing help documentation and MCP servers are specified separately, in
  `specs/002-agent-integration`. They build on the setup command-line flags defined here.
- The default memory-noise tolerance is 256 MiB per GPU device. Other defaults (margins, response
  times, windows) are chosen in the plan and documented.
- Out of scope:
  - hard GPU partitioning (hardware partitioning or multi-process service configuration)
  - privileged controls
  - disk and network I/O contention
  - acting on external processes in any form
- The existing lease, drain-deadline and retry rules stay in force except where a requirement
  above changes them.
