# Feature Specification: Contention Modes with Graduated Yielding

**Feature Branch**: `001-contention-modes`

**Created**: 2026-10-24

**Status**: Draft

**Input**: Maintainer feature description: contention modes (Yield First, Neutral, Performance)
with graduated, reversible yielding for CPU, RAM and GPU (compute and memory, per device), plus
fixes for six reproduced yielding defects. Implementation candidates are recorded separately in
[plan-notes.md](plan-notes.md).

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
   unchanged and the node runs in the documented default mode [NEEDS CLARIFICATION: default mode
   and mapping of `gpu.execution_mode` — see Q1].
2. **Given** a node capped at Neutral, **When** a job requests Performance, **Then** the job runs
   at Neutral and the effective mode is reported.
3. **Given** a coordinator and an agent on different releases, **When** they exchange heartbeats
   and assignments, **Then** both keep operating. Features the older peer lacks stay off, and no
   message is rejected because of a field the peer does not know.
4. **Given** a Guaranteed job, **When** contention requires capacity back, **Then** the job may
   be lowered in share, capped or hinted, but is drained only in the case defined in FR-024.

---

### User Story 7 - See what the node is doing (Priority: P3)

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
  follows its class rules, not a node-wide stop.
- **A finer-grained control is unavailable on the host** (for example, no delegated control
  subtree). The ladder uses the steps that are available and reports which ones it skipped.
- **Several own jobs share one GPU device.** Steps and targets are per allocation. The device's
  memory floor covers all of them together.

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
    except to keep a no-harm floor [NEEDS CLARIFICATION: whether no-harm floors may push Neutral
    memory below fair share — see Q3];
  - let external demand reclaim anything above fair share;
  - count unknown activity as one active principal without blocking admission.
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
- **FR-024**: Guaranteed allocations MAY be lowered in share, capped, paused and hinted, and MUST
  NOT be drained by automatic yielding [NEEDS CLARIFICATION: whether a no-harm floor that only
  releasing a Guaranteed allocation's memory can restore may drain it — see Q2]. Explicit
  operator drain keeps today's behavior.
- **FR-025**: Mode margins, response times, scale-up rates, tolerances and windows MUST be
  configurable per resource, with documented defaults. The Yield First margin MUST be larger than
  the Performance margin.

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
- **FR-031**: Existing `config_version = 1` files MUST load unchanged. The relationship of modes
  to `gpu.execution_mode` and the default for configs without modes MUST be documented [see Q1].
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

## Success Criteria *(mandatory)*

### Measurable Outcomes

Thresholds marked † are proposals that need maintainer confirmation.

- **SC-001**:
  - Across 100 consecutive launches on a shared GPU device and the D1/D2 replay scenarios: zero
    self-drains caused by the first observation after launch or by unattributed memory changes
    below the tolerance.
  - In the D5/D6 replay scenarios: zero drains caused by a single interval of measurement skew
    or one missing allocation measurement.
- **SC-002**: In Yield First, own CPU usage falls to within the configured margin within 2
  monitor intervals of external CPU demand appearing.† No work is drained or terminated when CPU
  is the only contended resource.
- **SC-003**: In Neutral, with one contending principal, own share of the contended resource
  converges to 50% ± 10% within 30 seconds.†
- **SC-004**: In Performance:
  - On an idle host, own work reaches at least 90% of available capacity minus the margin within
    10 seconds.†
  - When an external workload starts, it gets its demanded usage within 5 seconds.†
- **SC-005**: In every mode:
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
- The default memory-noise tolerance is 256 MiB per GPU device. Other defaults (margins, response
  times, windows) are chosen in the plan and documented.
- Out of scope:
  - hard GPU partitioning (hardware partitioning or multi-process service configuration)
  - privileged controls
  - disk and network I/O contention
  - acting on external processes in any form
- The existing lease, drain-deadline and retry rules stay in force except where a requirement
  above changes them.
