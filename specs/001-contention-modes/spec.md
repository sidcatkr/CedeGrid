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
  → A: The default mode is Neutral on every resource. The legacy `gpu.execution_mode` values
  are interpreted as FR-031 lists, with a warning for every value except `auto`.
- Q: May a no-harm floor that only a Guaranteed allocation's memory can restore drain that
  allocation? → A: Yes (floor policy A, the only floor policy): after all opportunistic work,
  with a hint first. Keeping Guaranteed memory against a floor, and exempting Guaranteed
  processes from the OOM preference, were removed (constitution 2.0.0, Principle II).
- Q: May no-harm floors push Neutral's memory below its fair share? → A: Yes. Floors always
  take precedence over fair share for RAM and GPU memory.
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

### Session 2026-10-25

- Q: How does Performance treat external activity whose amount is unknown, such as unsampled GPU
  compute? → A: While the amount is unknown, Performance holds own work to Neutral's fair
  share, counting that activity as one active principal. It is never more aggressive than
  Neutral under unknown activity.
- Q: Which opt-in options may exist? → A: Only options whose risk falls on own work. An option
  whose risk falls on other workloads (external OOM kills, swap thrash, failed external GPU
  allocations) does not exist, not even as an opt-in (constitution 2.0.0). Floor policies B and
  C and Neutral memory policy B are removed; a configuration that sets them fails to load with
  a message naming the replacement.
- Q: What may an operator tune for Guaranteed memory instead? → A: The Guaranteed hint deadline,
  and for RAM a floor-breach persistence window before Guaranteed work is drained. GPU memory
  breaches act without a window. Floor sizes stay configurable above an enforced minimum.
- Q: Must the modes be ordered on every resource? → A: Yes. For every resource and observation,
  own work's target in Yield First is at most Neutral's, which is at most Performance's.
  Performance never yields compute below the Neutral fair share under contention and takes all
  idle capacity minus its small margin. Floors override fair share for memory in every mode.
- Q: How does the policy know whether a worker reads hints? → A: Each supervisor reports, per
  allocation, whether the worker has acknowledged the target file (hint-ready) and whether it
  observes the drain file. The policy hints only hint-ready allocations; a refused hint is no
  longer the probe.
- Q: Who counts as a principal? → A: User IDs at or above a configurable `uid_min` (default
  1000) that stay above `principal_min_percent` for a sustained window, plus the CedeGrid
  account. A principal leaves with hysteresis. Usage by lower user IDs is background external
  demand, subtracted from capacity. Unreadable owners still count as one external principal.
- Q: How fast must capacity come back? → A: CPU within 2 monitor intervals in Yield First and
  Neutral; Performance follows what external work takes, at least its margin per interval
  (revised 2026-10-25, SC-002). RAM
  and GPU memory within hint deadline + drain timeout + termination grace, with a shorter bound
  on the critical floor path. These bounds are provisional and configurable.
- Q: What may happen to a Guaranteed allocation? → A: It is never paused. Its CPU is never
  capped below its request; usage above the request may be capped. Its memory is released only
  under floor policy A, as the last step. Yield First's contended target applies to
  opportunistic work only: for CPU, paused, then the minimum duty after `pause_bound_s`.
- Q: When does a worker see a target? → A: At once: the SDKs deliver the current target when a
  handler subscribes, and a managed child receives it when it starts.
- Q: What justifies holding capacity back from own work? → A: Only how slowly the resource is
  reclaimed. Capacity no workload wants must not sit idle, and no other workload may be harmed.
  CPU and GPU compute, reclaimed within an interval, keep near-zero idle reserves; RAM and GPU
  memory keep the floors (constitution 2.1.0).
- Q: When is a memory floor breached? → A: When observed free memory is below the floor.
  Running RAM work is judged on usage; GPU memory work on usage when it takes a hint, else at
  max(request, observed). A charge-only shortfall blocks admission and steps nothing.
- Q: What does a small steady external user cost own work? → A: Little. Under saturation a
  contending principal whose unknown demand is below one margin step is probed: own work steps
  down by the mode's margin per response window, and a principal that does not grow into the
  freed capacity is recorded as satisfied. A larger demand gets the fair share at once.
- Q: How does Performance behave under contention? → A: It keeps the larger of the fair share
  and capacity minus observed external usage minus its margin, so it gives back what external
  demand takes and never goes below the fair share.
- Q: How is CPU charged for admission? → A: Running opportunistic work at its p95 usage over
  60 s; pending work at its request and Guaranteed work at max(request, observed). RAM and GPU
  memory stay request-based.

## Terms

- **Own work**: allocations that CedeGrid launched and verified, including SDK-managed children.
- **External demand**: all other use of the host's CPU, RAM and GPUs. This includes other users'
  processes and unmanaged processes of the account that runs CedeGrid.
- **Resource**: one of CPU, RAM, GPU compute (per device) or GPU memory (per device).
- **Principal**: a user ID at or above `uid_min` whose usage of a resource has stayed above
  `principal_min_percent` for the principal window. The account that runs CedeGrid is always one
  principal, whatever its user ID; its share covers own work plus its unmanaged processes. Usage
  by lower user IDs is *background* demand, subtracted from capacity.
- **Fair share**: a work-conserving, equal split of a contended resource among active
  principals. Capacity a principal does not use is available to the others.
- **Margin**: capacity own work leaves unused for external demand to grow into, justified only
  by how slowly the resource is reclaimed. Margins are set per mode and per resource: small on
  CPU and GPU compute, and on memory the floors plus Yield First's 20%.
- **Saturated**: busy at or above `saturation_percent` of capacity (default 98%).
- **Contended**: a CPU or GPU compute resource is contended when it is saturated and an external
  principal, or the unattributed principal, uses at least `principal_min_percent` of it without
  its demand being known to be satisfied, or when it has external activity of unknown amount.
- **Satisfied demand**: a principal's demand found by probing: the usage that did not grow into
  capacity own work freed, plus hysteresis equal to the contention floor. It holds while the
  principal's usage stays within it.
- **Held back**: capacity kept from own work or from admission, by cause: floor, reserve,
  margin, contention, peak, noise tolerance, unknown, cooldown, backoff and charge shortfall.
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
- **Floor policy A**: the only floor policy. Floors restore by releasing opportunistic work
  first and Guaranteed memory last.
- **Hint-ready**: the worker has acknowledged its resource-target file, so a hint can reach it.
- **Critical floor breach**: free memory on a resource has fallen below the critical share of
  its floor (default 50%).
- **Risk marker**: the label setup shows on an option that lets own work hold more capacity
  within the floors and reclaim bounds. The only marker is *caution*. No option may put another
  workload at risk.

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
   not by ID order alone. Once jobs carry their own modes, Yield First jobs yield first, then
   Neutral, then Performance, with the cost order inside each tier.
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
   **Then** own work steps down promptly, keeping the mode's generous margin free. **When** the
   resource is contended, **Then** opportunistic CPU work is paused within 2 monitor intervals,
   then runs at the minimum duty after `pause_bound_s`, while Guaranteed work keeps its CPU
   request; own work that saturated the resource itself is first capped to the margin, and a
   demand smaller than one margin step is probed instead (FR-020). **When** external demand fits
   in the margin, **Then** nothing is paused. **When** activity is unknown, **Then** it counts as
   external demand.
2. **Given** Neutral and one contending principal, **When** both principals want more than the
   capacity, **Then** own work converges to its fair share and does not yield below it, except
   where a no-harm floor requires it (see FR-021). External demand can always reclaim anything
   above own work's fair share. Unknown activity counts as one active principal but does not
   block admission.
3. **Given** Performance on an idle host, **When** own work ramps up, **Then** it uses all
   capacity external demand is not using, minus a small safety margin, and scales up quickly.
   **When** external demand grows, **Then** own work yields what is needed to restore the margin
   and floors. **When** the resource is contended, **Then** compute keeps what external demand
   does not take, minus the margin, and never goes below the Neutral fair share.
4. **Given** any mode, **When** a no-harm floor would be crossed, **Then** own work steps down
   until the floor holds, whatever the mode would otherwise allow.
5. **Given** any mode, **When** CedeGrid acts, **Then** it acts only on own processes. No external
   process is signalled, reprioritized, throttled, paused, moved or adopted.
6. **Given** any resource and any observation, **When** the three modes compute own work's
   target, **Then** Yield First's target is at most Neutral's, which is at most Performance's.

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
   **When** the policy must free memory, **Then** the allocation is reported as not hint-ready,
   no hint is sent, and the worker keeps today's drain behavior.
4. **Given** a worker that acknowledges the target file only after it starts, **When** the next
   monitor interval passes, **Then** its allocation is reported as hint-ready and later memory
   contention hints it first.
5. **Given** a current target, **When** a handler subscribes or a managed child starts, **Then**
   the handler or child receives that target at once.

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
   legacy value is interpreted as FR-031 lists, a warning names the replacement setting, and
   `doctor` shows the interpretation.
3. **Given** a node capped at Neutral, **When** a job requests Performance, **Then** the job runs
   at Neutral and the effective mode is reported.
4. **Given** a coordinator and an agent on different releases, **When** they exchange heartbeats
   and assignments, **Then** both keep operating. Features the older peer lacks stay off, and no
   message is rejected because of a field the peer does not know.
5. **Given** a Guaranteed job, **When** contention requires capacity back, **Then** the job may
   be lowered in share, capped above its CPU request or hinted, and its memory is released only
   as the last step of floor policy A, as FR-024 defines.
6. **Given** a configuration that sets a removed floor policy (`b` or `c`) or Neutral memory
   policy `b`, **When** it is loaded, **Then** loading fails with a message that names the
   replacement settings.

---

### User Story 7 - Set up a node interactively or from the command line (Priority: P2)

An operator runs setup and is guided through the choices: node role, per-resource modes, node cap,
floor and margin sizes, and the Guaranteed hint deadline. On a terminal the default is an
interactive, keyboard-driven interface. Scripts, agents and non-TTY sessions pass the same choices
as command-line flags. Either way, the result is written to the configuration file and can be
changed later.

**Why this priority**: modes and caution options are only safe if operators see what each
choice does before choosing it. Convenient setup is also the main way new nodes get configured.

**Independent Test**:
- Run setup on a TTY using only the keyboard, and check the written configuration.
- Run it again with the same choices as flags in a non-TTY session; the configuration must be
  identical.
- Check that the interactive run shows the risk markers and that a caution choice asks for
  confirmation.

**Acceptance Scenarios**:

1. **Given** a TTY, **When** the operator runs setup without flags, **Then** the interactive
   interface opens:
   - Arrow keys move the selection, Space toggles or selects, Enter confirms.
   - No mouse is needed.
   - Defaults are preselected.
2. **Given** the mode, floor and hint-deadline choices, **When** they are displayed, **Then**:
   - Each option shows a one-line consequence.
   - Default options are uncolored and unmarked.
   - Performance mode and floors near their minimum are yellow and labelled `[caution]`.
3. **Given** `NO_COLOR` is set, or the terminal cannot show color, **When** the options are
   displayed, **Then** no color is used and the `[caution]` labels still appear.
4. **Given** the operator selects a caution option, **When** they press Enter, **Then** a separate
   confirmation step states the consequence. The choice is kept only after explicit
   confirmation; declining returns to the selection with the default still in place.
5. **Given** a non-TTY session, **When** setup runs, **Then** it never prompts or waits for
   input. It uses the flags and any existing configuration. If a required choice is missing, or a
   caution option lacks its explicit acknowledgement flag, it exits with an error that names the
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
  That demand counts as one additional principal for fair-share purposes. A host that mounts
  `/proc` with `hidepid=2` always takes this path for other accounts.
- **CedeGrid runs under a user ID below `uid_min`.** The account is still the owner principal.
- **A new user's demand appears.** Until the user has stayed above `principal_min_percent` for
  the principal window, its usage counts toward the single unattributed principal, so the
  response does not wait for the window.
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
- **A job is a plain command, not SDK-based.** It is never hint-ready. Memory contention
  escalates from the steps that do not free memory straight to drain. On the critical floor
  path, a worker that does not observe the drain file is sent TERM without the drain-file
  wait.
- **A Guaranteed job exceeds its own request.** It is charged at its observed usage. The response
  follows its class rules and FR-057, not a node-wide stop.
- **A finer-grained control is unavailable on the host** (for example, no delegated control
  subtree). The ladder uses the steps that are available and reports which ones it skipped.
- **Several own jobs share one GPU device.** Steps and targets are per allocation. The device's
  memory floor covers all of them together.
- **Caution option chosen non-interactively without its acknowledgement flag.** Setup refuses
  and writes nothing.
- **Configuration edited by hand to select Performance.** Loading fails unless the file also
  contains the acknowledgement entry `performance_mode`. With the entry, CedeGrid loads it and
  reports the caution option as active.
- **Configuration edited by hand to lower a margin or relax detection.** A margin below its
  default, or a detection setting beyond its default (FR-045: `saturation_percent`,
  `principal_min_percent`, `noise_tolerance_mib` or `uid_min` above it, `memory_peak_window_s`
  or `quiet_window_intervals` below it), fails to load unless the file contains the entry
  `reduced_margin` or `relaxed_detection`. With the entry, CedeGrid reports the caution option as
  active.
- **A floor set near its minimum, or below it.** A floor is "near" its minimum below twice the
  minimum. A floor below the minimum is raised to it. A floor near the minimum without the
  `small_floor` entry still loads, so 0.2 files keep loading, but is raised to twice the minimum
  with a warning; it applies as configured only once confirmed. The warning comes at load, or,
  for a floor a percentage sets, when the agent first samples the host. Setup always records the
  entry.
- **A job requested far more memory than it uses.** Free memory above the floor is no breach.
  A RAM job, or a GPU job that takes hints, is never stepped for it, and admission on that
  resource stays blocked while its charge exceeds the budget. A GPU job without a hint channel
  is judged at its charge, since its growth cannot be bounded (FR-066).
- **A small steady external user beside own work that keeps the host busy.** It is probed once
  and recorded as satisfied; own work keeps its target instead of cycling down to the fair
  share.
- **A configuration sets a removed option.** Floor policy `b` or `c`, or Neutral memory policy
  `b`, fails to load with a message that names the replacement settings.
- **Terminal too small or lacking cursor control for the interface.** Setup falls back to
  line-based prompts on a TTY; it never hangs.
- **A RAM floor breach that clears within the breach window.** Guaranteed work is hinted but not
  drained. A breach that reaches the critical level skips the window.

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
- **FR-059**: Principals MUST be the CedeGrid account plus user IDs at or above `uid_min`
  (default 1000) whose usage of a resource has stayed at or above `principal_min_percent` for
  `principal_window_intervals`. Until a user joins, its usage counts toward the single
  unattributed principal, which unreadable owners share; that principal contends once its usage
  reaches `principal_min_percent`. A principal MUST leave only after its usage has stayed below
  that threshold for `principal_leave_s` (default 60 s). Usage by user IDs below `uid_min` MUST
  count as background external demand: it is subtracted from capacity and never forms a
  principal.
- **FR-060**: Admission headroom for RAM and GPU memory MUST use the peak of external usage over
  `memory_peak_window_s` (default 300 s), including the memory of quiet GPU contexts. Peaks are
  kept per external process, identified by PID and start time: a live process keeps its peak for
  the window, and its peak is dropped as soon as it exits. External usage that no listed process
  accounts for keeps one peak. Yield decisions use current usage.

**Graduated yielding**

- **FR-011**: Yielding MUST use the ladder in order: lower share or priority, cap, pause, resource
  hint, cooperative drain, termination.
  - A step that cannot free the contended resource, or is unavailable for the allocation or
    host, MUST be skipped, and the skip MUST be reported.
  - Memory contention MUST start at the hint step for hint-ready allocations, and at drain for
    the others. Pause MUST NOT be used to resolve GPU memory contention.
- **FR-012**: Drain MUST be used only when the lower steps cannot free the contended resource
  within a bounded time. Termination MUST be used only after drain fails within its deadline,
  except on the critical floor path for a worker that does not observe the drain file (FR-063).
- **FR-013**: Steps 1–4 MUST be reversible. When external demand recedes past a reversal
  threshold with hysteresis, steps MUST be undone in reverse order at the mode's scale-up rate.
  A step taken for an envelope breach or an unknown measurement MUST reverse once that cause has
  stayed clear, whatever the external level. A step taken for a floor breach MUST reverse only
  after free memory has stayed above the floor by at least what the step had to free, or
  `noise_tolerance_mib` if larger, for the calm window, and its reversal MUST also wait the
  episode's reclaim backoff (FR-061), so own work that regrows into the floor backs off.
- **FR-014**: Pause MUST be bounded in time. A pause that does not resolve contention within the
  bound MUST escalate to the next step. For CPU alone that is the hint, whose target also caps
  CPU, at the 5% minimum duty for a target of zero; a worker that is not hint-ready gets a cap at
  the same minimum duty instead. Either is the last CPU step while the contention lasts: it does
  not pause again, and CPU contention alone never drains (SC-007).
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

  Remaining ties MUST break deterministically. Once jobs carry their own modes (FR-030), the
  owner's aggregate share comes from the node mode and cap, and allocations yield in mode tiers
  inside it: Yield First, then Neutral, then Performance, with this cost order within a tier.
  Class rules still win across tiers: every opportunistic allocation is drained before any
  Guaranteed one (FR-024), and a Guaranteed allocation keeps its CPU request in every tier.
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
    tolerance;
  - on a contended compute resource (see Terms), take opportunistic work down: for CPU, paused,
    then the minimum duty after `pause_bound_s` (FR-014); Guaranteed work keeps its CPU request
    (FR-024). With own work held to its margin, the resource saturates only once external demand
    outgrows the margin. While own work holds more than capacity minus the margin, saturation
    may be its own doing: it is first capped to the margin, within the Neutral fair share, and
    paused only if the resource stays contended. While a small demand is probed (FR-021), own
    work steps down with the probe instead. External activity that fits in the margin only
    lowers the cap by its amount;
  - count unknown activity as external demand;
  - scale up slowly.
- **FR-021**: Neutral MUST:
  - let own work use idle capacity minus the Neutral margin (default 5%, between the
    Performance and Yield First margins) on CPU and GPU compute; memory keeps only the floors;
  - converge own work to its fair share under contention, and not step it below fair share
    except to keep a no-harm floor;
  - under saturation, probe a contending principal whose unknown demand is below one margin
    step: step own work down by the margin per response window, and record a principal whose
    usage does not grow into the freed capacity as satisfied, after which it no longer contends
    until its usage exceeds that demand; a larger demand gets the fair share at once, and is
    then watched for one response window from the interval after: if the host has capacity to
    spare at its end and the principal's usage stayed within its baseline plus the contention
    floor, it is recorded as satisfied the same way, else it keeps its share. A satisfied
    principal that leaves keeps its record and rejoins at once when it returns. Unknown
    activity, including GPU compute activity whose amount is not measured, is never recorded;
  - let external demand reclaim anything above fair share;
  - count unknown activity as one active principal without blocking admission.

  No-harm floors take precedence over fair share for RAM and GPU memory. On GPU compute, a share
  is approximated by pausing own GPU work for part of each duty cycle; the cycle length, and so
  the granularity, is documented.
- **FR-022**: Performance MUST:
  - let own work use all capacity external demand is not using, minus a small safety margin
    (default 2%) on CPU and GPU compute; memory keeps only the floors;
  - scale up quickly;
  - when external demand grows, yield what is needed to restore the margin and floors within the
    mode's response time;
  - on a contended compute resource, keep the larger of the Neutral fair share and capacity
    minus observed external usage minus the margin: give back what external demand takes, and
    never go below the fair share;
  - while external activity of unknown amount exists, hold own work to Neutral's fair share,
    counting that activity as one active principal.
- **FR-065**: For every resource and every observation, own work's target MUST satisfy
  Yield First ≤ Neutral ≤ Performance. Configuration enforces it by construction: the margins
  are validated as Yield First ≥ Neutral ≥ Performance, with Yield First above Performance,
  Neutral and Performance share the fair share under contention, and floors bound every mode on
  memory.
- **FR-023**: No-harm floors MUST apply in every mode:
  - a minimum available host RAM;
  - a minimum free memory per GPU device, so that other workloads' allocations do not fail;
  - every own process, Guaranteed included, marked as a preferred out-of-memory victim where the
    platform allows an unprivileged process to mark itself.

  Floor sizes are configurable, but never below an enforced minimum (512 MiB of host RAM, 512 MiB
  per GPU device, provisional). The GPU floor is also at least `gpu.reserve_vram_percent` (5%) of
  the device; `gpu.reserve_vram_mib` defaults to twice the minimum, so a default floor is never
  near the minimum, and a fixed 3 GiB applies only when configured. `doctor` shows every floor as
  it applies on the host, and a floor raised to twice its minimum carries the reason code
  `small_floor_raised`. A smaller configured floor is raised to the minimum with a load-time
  warning, and to twice the minimum unless the `small_floor` entry confirms it (FR-054). A floor
  sized by `ram.reserve_percent` or `gpu.reserve_vram_percent` is judged on the host, after the
  percentage is applied; its warning comes when the agent first samples the host.
- **FR-024**: Guaranteed allocations:
  - MUST NOT be paused, which also rules out the pause duty cycle that approximates a GPU compute
    share: GPU compute steps are unavailable for Guaranteed work;
  - MAY be lowered in share and hinted;
  - MAY have CPU usage above their request capped, but MUST NOT be capped below their request;
  - MUST NOT be drained by automatic yielding, except to restore a RAM or GPU memory floor or the
    envelope under floor policy A, as the last step: after all opportunistic work on the
    resource that anyone can act on has been released, after a hint whose
    `guaranteed_hint_deadline_s`, counted from that hint, has passed when the allocation is
    hint-ready, and, for RAM, after the breach has lasted `ram.guaranteed_breach_window_s`,
    counted from the start of the breach. GPU memory breaches have no window. On the critical floor
    path (FR-063), the critical hint deadline replaces the Guaranteed hint deadline and the
    window is skipped, and a Guaranteed allocation that does not observe the drain file may be
    terminated as that path's last step.
  - The envelope exception applies to RAM and GPU memory only, with the same deadlines and
    window; a CPU envelope breach caps usage above the request and never drains Guaranteed
    work.

  Explicit operator drain keeps today's behavior.
- **FR-025**: Every threshold that steers yielding MUST be a configuration value, not a
  hard-coded constant. This covers:
  - margins, response times and scale-up rates
  - tolerances, windows and deadlines, including the Guaranteed hint deadline, the RAM breach
    window, the critical floor share and hint deadline, the principal window,
    `principal_leave_s` and `uid_min`, the memory peak window and the CPU admission usage window
  - convergence targets
  - memory scale-up cooldowns and reclaim backoff

  Each is set per resource where it applies, defaults to the provisional value in this spec, and
  is documented. Margins are ordered as FR-065 requires. A margin below its default, or a
  detection setting beyond its default (FR-045), is a caution option. Fixed mechanism constants
  are exempt and documented in the data model: a cap binds at 95% of its target and counts as
  effective up to 105%, and FR-014's minimum duty is 5%.

**GPU sharing and placement**

- **FR-026**: GPU work MUST be admissible to a device that hosts external contexts when the mode
  allows it and the requested memory fits above the device's floor. An empty device MUST NOT be
  required under Neutral or Performance.
- **FR-027**: Under Yield First, new own GPU work MUST NOT be admitted to a device with active or
  unknown external compute activity.
- **FR-028**: Node reports MUST state admission per resource. Coordinator placement MUST honor it.
  Admission for a resource MUST be blocked while own work on that resource is above step zero.
- **FR-029**: Nodes MUST advertise only the control capabilities they can actually apply.
- **FR-064**: `contention.never_share_occupied_gpus` (default false) MUST, when set, block new own
  GPU work on any device with an external context or unknown external processes, in every mode.
  Own work already running on a device that becomes occupied is not drained for that alone;
  the device's mode and floors govern it.

**Configuration and compatibility**

- **FR-030**: A node MUST have a default mode per resource. A job or task MAY override it per
  resource. A node MAY set a cap on the most aggressive mode allowed (Yield First < Neutral <
  Performance); without one, the cap is the node's most aggressive configured mode, so no job
  runs in Performance unless the operator chose it. A job runs at the less aggressive of its
  requested mode and the node cap, and the effective mode MUST be reported. A requested mode
  the cap refuses MUST be reported with the reason code `mode_capped` and the node's cap.
- **FR-031**: Existing `config_version = 1` files MUST load unchanged.
  - Without mode settings, every resource MUST run in Neutral.
  - `gpu.execution_mode` MUST still be accepted and is interpreted for the GPU resources that
    have no explicit mode:

    | Legacy value | Interpretation | Warning |
    |---|---|---|
    | absent or `auto` | Neutral | none |
    | `contention_aware` | Neutral | names `contention.gpu_compute.mode` and `contention.gpu_memory.mode` |
    | `conservative_non_sharing` | `contention.never_share_occupied_gpus = true`, orthogonal to the mode | names `contention.never_share_occupied_gpus` |
    | `best_effort_occupied` | Neutral | states that the PID list is no longer used; it is still validated as in 0.2 so such files load unchanged |

  - A non-empty `gpu.best_effort_external_processes` MUST warn that the list is no longer used.
  - `doctor` MUST show how a legacy value was interpreted.
- **FR-032**: Agents, coordinators and SDK workers from 0.2 and from this release MUST
  interoperate in any combination. A peer MUST NOT reject a message because of fields it does not
  know. Features unsupported by either peer MUST stay off.
- **FR-033**: Recorded snapshots and policy inputs from 0.2 MUST remain replayable.
- **FR-034**: Scale-up timing MUST be configured per resource and counted from the time that
  resource became stable. CPU and RAM use `scale_up_cooldown_ms`, by default the mode's scale-up
  interval (30, 10 or 2 s). GPU memory uses `contention.gpu_memory.scale_up_cooldown_ms`, else a
  set legacy `gpu.scale_up_cooldown_ms`, else the mode's interval, per device. RAM and GPU
  memory add the reclaim backoff (FR-061). Each resource's admission opens on its own cooldown.
  For CPU admission, running opportunistic work is charged at its p95 usage over
  `contention.cpu.admission_usage_window_s` (default 60 s) once it has a full window of samples,
  and at max(request, observed) before that, so work in its startup phase is never
  under-charged; pending work is charged in full, Guaranteed work at max(request, observed), and
  RAM and GPU memory admission stays request-based. Coordinator placement and the agent's launch
  check MUST admit CPU the same way. RAM admission and the agent's worker limit bound how many
  jobs this CPU credit can admit.
- **FR-061**: A reclaim episode on a memory resource lasts while it has a floor breach or own
  usage beyond its budget. With n episodes started within the last `memory_peak_window_s`, it adds
  `reclaim_backoff_ms` × 2^(n−1) to that resource's cooldown, at most `reclaim_backoff_max_ms`
  (default 1 s doubling to 30 s); the first episode adds 1 s. A window without an episode clears
  the count.

**Cooperative resource targets**

- **FR-035**: Both SDKs MUST let a worker read its current per-resource targets and be notified
  when they change, including in SDK-managed children. Targets can tighten and relax. A handler
  MUST receive the current target when it subscribes, and a managed child MUST find the current
  target when it starts.
- **FR-036**: Workers that do not support targets MUST keep today's drain behavior unchanged.
- **FR-037**: A hint MUST carry the deadline the policy enforces for it: the resource's hint
  deadline, the Guaranteed hint deadline for Guaranteed work, or the critical hint deadline on the
  critical path. When the deadline passes without compliance, the ladder escalates; a memory hint
  waits for its deadline in every mode.
- **FR-062**: Each supervisor MUST report, per allocation, whether the worker has acknowledged its
  target file (`hint_ready`) and whether it observes the drain file (`observes_drain`), in its
  `adjusted` and `state` events and when either changes. The agent MUST include both in the
  allocation's node report once negotiated. The policy MUST use these facts: only hint-ready
  allocations receive hints, and a refused hint is not a capability probe.
- **FR-063**: A floor breach is critical when free memory on the resource falls below the
  critical level: the larger of `critical_floor_percent` of its floor (default 50%, 10–90%) and
  the minimum floor (FR-023). On the critical path:
  - hint-ready allocations get `critical_hint_deadline_s` (default 1 s) instead of their hint
    deadline;
  - allocations that are not hint-ready are drained at once;
  - allocations that do not observe the drain file are sent TERM without the drain-file wait,
    then KILL after the grace period, acting on own processes only.

  The out-of-memory preference stays the final backstop.

**Observability**

- **FR-038**: For each resource, `observe`, `doctor`, `status` and node reports MUST show:
  - configured and effective mode
  - own usage
  - external demand
  - unattributed usage
  - target share
  - current yield step per allocation
  - concise reasons
  The negotiated node report carries mode, admission, own, external, target share and step
  summary today; the remaining fields arrive with the observability story (US8).
- **FR-039**: Every reason MUST be one sentence and carry a stable code that tools and tests can
  rely on.

**Defect regression**

- **FR-040**: The characterization tests for D1–D6 MUST be inverted to assert the corrected
  behavior. Each MUST pass.

**Removed options and risk markers**

- **FR-041**: Floor policy A is the only floor policy. `contention.floor_policy = "b"` or `"c"`
  MUST fail to load with a message that names the replacement: the Guaranteed hint deadline and
  the RAM breach window (FR-024). Every own process stays a preferred out-of-memory victim.
- **FR-042**: Neutral memory policy B is removed. `contention.neutral_memory_policy = "b"` MUST
  fail to load with a message that names the replacement: the floor sizes (FR-023).
- **FR-043**: Caution options:
  - MUST be off by default;
  - MUST NOT be enabled by upgrades, migrations or defaults;
  - MUST be recorded in configuration;
  - MUST appear in `doctor`, `status` and node reports while active, with their marker.
- **FR-044**: No option MAY permit acting on an external process, raising own priority, or
  putting another workload at risk of an out-of-memory kill, swap thrash or a failed GPU
  allocation.
- **FR-045**: The risk markers are justified as follows.

  | Option | Marker | Justification |
  |---|---|---|
  | Yield First, Neutral, default floors | none | Floors hold in every mode. Own work is released before others are affected. |
  | Performance mode | *caution* (yellow) | Own work keeps all idle capacity minus a small margin and grows quickly. Other workloads still get their fair share within the reclaim bounds, and floors still hold. |
  | A floor near its minimum | *caution* (yellow) | Less headroom is kept for other workloads' bursts. The enforced minimum and the out-of-memory preference still protect them. |
  | A margin below its default (`reduced_margin`) | *caution* (yellow) | Own work leaves less idle capacity for external demand to grow into. Contention handling and floors still hold. |
  | A detection setting beyond its default (`relaxed_detection`): `saturation_percent`, `principal_min_percent`, `noise_tolerance_mib` or `uid_min` above it, `memory_peak_window_s` or `quiet_window_intervals` below it | *caution* (yellow) | Contention and external growth are recognized later or forgotten sooner, so own work holds capacity longer. Floors still hold. |

**Setup**

- **FR-046**: Interactive setup MUST be the default way to configure a node. Running setup on a
  TTY without flags MUST open the terminal interface. On a TTY, a command that needs a missing
  configuration file MUST offer to start setup.
- **FR-047**: Setup MUST cover:
  - node role
  - per-resource default modes
  - node mode cap
  - floor and margin sizes
  - the Guaranteed hint deadline
  - the thresholds an operator is likely to tune (tolerances)

  The remaining thresholds keep their defaults unless changed by flags or in the file.
- **FR-048**: The interface MUST be usable with the keyboard alone: arrow keys move, Space toggles
  or selects, Enter confirms. It MUST NOT require a mouse.
- **FR-049**: Each mode, floor, margin, threshold and hint-deadline option MUST show a one-line
  consequence.
  - Default options MUST be uncolored and unmarked.
  - *Caution* options MUST be yellow.
  - Every marked option MUST also show the text label `[caution]`, so color is never the only
    signal.
- **FR-050**: Setup MUST NOT emit color in any of these cases: `NO_COLOR` is set to a non-empty
  value, output is not a terminal, or the terminal does not support color. Text labels MUST
  remain.
- **FR-051**: Choosing a *caution* option MUST require a separate, explicit confirmation that
  restates the consequence:
  - in the interface, a separate confirmation step;
  - in non-interactive use, an acknowledgement flag that names the option;
  - in a hand-edited file, an `acknowledge_caution` entry that names the option
    (`performance_mode`, `small_floor`, `reduced_margin` or `relaxed_detection`).
- **FR-052**: Setup MUST offer every interactive choice as a command-line flag. On a non-TTY it
  MUST NOT prompt or block. It MUST:
  - take values from flags and the existing configuration;
  - exit with a non-zero status and name the missing flag when a required choice is absent;
  - offer machine-readable output for scripts and agents.
- **FR-053**: Before writing, setup MUST show a summary that includes every active caution option
  and its marker, and MUST validate the result like any configuration load. It MUST write the file
  atomically and preserve all settings it does not manage.
- **FR-054**: Re-running setup MUST start from the current configuration. Hand edits MUST have
  the same effect as setup choices. Performance selected by hand without the `performance_mode`
  entry, and a margin or detection threshold beyond its default without `reduced_margin` or
  `relaxed_detection`, MUST fail to load with an error naming the missing entry. A floor near its
  minimum without the `small_floor` entry MUST load, so that 0.2 files keep loading (FR-031), and
  MUST apply as twice the minimum, with a warning naming the entry (FR-023).
- **FR-055**: Cancelling setup MUST write nothing and MUST restore the terminal. If the interface
  cannot drive the terminal, setup MUST fall back to line-based prompts.

**Documentation**

- **FR-056**: `docs/guarantees.md` MUST describe the no-harm floors and their minimums, floor
  policy A, the caution options with their markers, the removed options, the envelope and spool
  behavior, and the interpretation of `gpu.execution_mode`, in the same change that implements
  them.

**Operator limits**

- **FR-057**: When own work exceeds the operator's configured resource envelope, CedeGrid MUST
  restore the envelope per resource through the yield ladder.
  - Opportunistic work is released first, by least cost.
  - Guaranteed work MUST NOT be drained except as floor policy A allows (FR-024).
  - An envelope breach MUST NOT stop every allocation at once.
- **FR-058**: When the node's spool budget is exhausted, CedeGrid MUST block new admission and
  report the condition. It MUST NOT drain allocations because of it. Explicit operator drain
  keeps draining every allocation.

**Work conservation**

- **FR-066**: A memory floor breach MUST be observed free memory below the floor (RAM: available
  memory; GPU: total minus used). Running RAM work is judged on usage. Running GPU memory work
  is judged on usage when it is hint-ready, since its hint bounds its growth, and at
  max(request, observed) otherwise, since its growth cannot be bounded. A charge-only shortfall
  MUST block admission on that resource and MUST NOT hint, drain or terminate running work.
  Envelope breaches of running memory work use the same rule.
- **FR-067**: The CPU idle reserve MUST be the larger of `cpu.reserve_physical_cores` (default
  0) and the mode's margin of total capacity; the two never stack.
- **FR-068**: Every configuration key MUST be classified as safe, bounded or caution, and a
  test MUST fail for an unclassified key. `doctor` and setup MUST list, for each active caution
  option, the keys and values it covers.
- **FR-069**: `doctor` and `status` MUST show, per resource, how much is held back and why.
- **FR-070**: `noise_tolerance_mib` MUST be at most 1024 MiB at load and at most half of each
  device's effective floor when the agent starts; otherwise the agent refuses to start.
  Settings that lengthen reclaim or let stale telemetry pass are bounded: `monitor.interval_ms`
  and `kernel.monitor_interval_ms` at most 10 s, `kernel.freshness_intervals` at most 20,
  `gpu.process_sample_max_age_ms` at most 60 s, `lifecycle.drain_timeout_ms` and
  `lifecycle.term_grace_ms` at most 60 s.
- **FR-071**: The resource-target file MUST carry an optional `allowance` per resource,
  published continuously: what own work may grow to now. Both SDKs MUST offer
  `on_resource_allowance`. Workers that ignore it keep working (implemented after US5).

### Key Entities

- **Contention mode**: Yield First, Neutral or Performance. Configured per resource as a node
  default, a job override and a node cap. The effective mode is derived per allocation.
- **Resource view**: per node and resource, the capacity, own usage, external demand by
  principal, unattributed usage, floor, margin, fair share and admission state.
- **Principal**: a user ID at or above `uid_min` with sustained demand on a resource, or the
  CedeGrid account.
- **Yield step**: per allocation and resource, the current ladder level, its cause, when it was
  entered, and which steps are unavailable.
- **Resource target**: per allocation and resource, the "use less" amount, its sequence number
  and deadline, as delivered to cooperative workers.
- **Yield cause**: why a step was taken, such as external growth, floor breach, unknown
  measurement, operator drain or envelope breach. It travels with the decision to the
  coordinator.
- **Setup choices**: the operator's selections and their risk markers, written to configuration.
  They include role, modes, node cap, floor and margin sizes, the Guaranteed hint deadline and
  tuned thresholds.
- **Allocation readiness**: per allocation, whether the worker is hint-ready and whether it
  observes the drain file, as reported by its supervisor.

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
- **SC-002**: In Yield First and Neutral, CPU demanded by external work is returned within 2
  monitor intervals of the demand appearing, including on a host that own work saturates: own
  CPU usage reaches the mode's contended target, which in Yield First is opportunistic work
  paused, then at the minimum duty after `pause_bound_s`, and in Neutral the fair share. Where own
  work saturated the resource before its first cap, Yield First's first step is a cap to the
  margin within the fair share, so at least the fair share is back within the same bound; a
  demand smaller than one margin step is probed instead (FR-021). In Performance, own CPU usage
  falls by at least its margin per interval while external demand grows into the freed
  capacity, so it reaches the fair share within ⌈(own usage − fair share) ÷ margin⌉ intervals
  (24 intervals, 12 s, from 98% with the 2% margin), and never goes below it. With demand that
  fits in the margin, Yield First keeps its 15% margin free. No work is drained or terminated
  when CPU is the only contended resource.
- **SC-003**: In Neutral, with one contending principal, own share converges to the fair share
  within the Neutral convergence time. The fair share is 50% here; the defaults are a tolerance
  of ±15 percentage points and a convergence time of 60 s.
- **SC-004**: In Performance:
  - On an idle host, own work reaches the Performance ramp target within the ramp time. The
    defaults are 80% of available capacity minus the margin, within 30 s.
  - When an external workload starts, it gets its demanded usage, up to its fair share, within
    the reclaim bounds of SC-012.
- **SC-005**: In every mode and with every caution option active:
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
  - Every *caution* choice needs an explicit confirmation or acknowledgement flag.
  - With `NO_COLOR` set, no color escape sequence is emitted.
- **SC-012**: Reclaim bounds, each measured from the first observation of the demand or breach to
  the step that frees the resource, for memory the forced termination of own work that has not
  released it:
  - CPU: 2 monitor intervals in Yield First and Neutral; Performance as SC-002 states.
  - RAM and GPU memory, opportunistic work: at most hint deadline + drain timeout + termination
    grace + 1 monitor interval (defaults 5 + 3 + 2 + 0.5 = 10.5 s); without a hint, drain
    timeout + grace + 1 interval (5.5 s).
  - Guaranteed memory alone on the resource: the longer of the Guaranteed hint deadline and, for
    RAM, the breach window, plus drain timeout, grace and 1 interval (defaults 10 + 3 + 2 + 0.5 =
    15.5 s). With opportunistic work on the resource, the Guaranteed drain also waits for that
    work's release.
  - Critical floor path: critical hint deadline + drain timeout + grace + 1 interval
    (defaults 1 + 3 + 2 + 0.5 = 6.5 s); without a hint, 5.5 s; for a worker that does not observe
    the drain file, grace + 1 interval (2.5 s).
- **SC-013**: Over generated observations of every resource, own work's target satisfies
  Yield First ≤ Neutral ≤ Performance in 100% of cases.
- **SC-014**: On an idle host, own CPU utilization is at least 97% in Performance, 94% in
  Neutral and 84% in Yield First, and own RAM and GPU memory are within 1% of capacity minus the
  floor, minus Yield First's margin too.
- **SC-015**: Beside a steady external user of 1–2%, own work's 5-minute average is at least 90%
  of capacity minus that user and the margin, with no cap or pause cycling.
- **SC-016**: `doctor` and `status` show, per resource, how much is held back and why: floor,
  reserve, margin, contention, peak, noise tolerance, unknown, cooldown, backoff and charge
  shortfall.

### Provisional thresholds

All values are **provisional, to validate on real hardware**. Each is a configuration value; the
times assume the default 500 ms monitor interval.

| Threshold | Default | Rationale |
|---|---|---|
| GPU memory noise tolerance | 256 MiB per device | Larger than typical driver and context jitter, and small next to common device sizes. It was the maintainer's proposal. |
| Yield First margin | 15% of compute, 20% of memory beyond the floor | Generous by design, yet compute is reclaimed within an interval, so it needs less than memory. |
| Yield First response time | 3 monitor intervals (1.5 s) for escalating past a step that did not free the excess | One interval to observe, one to confirm, one to apply and measure. The first step applies at once, so contended CPU returns within 2 intervals (SC-002). |
| CPU reclaim | 2 monitor intervals in Yield First and Neutral; in Performance at least the margin per interval down to the fair share | One interval to observe the demand (a per-interval average), one with the step applied. On a saturated host the contended target applies at once instead of shrinking one margin per interval, except for Performance, which keeps what external demand does not take, and a probed small demand. |
| Yield First scale-up | 1 step per 30 s | Matches today's 30 s scale-up cooldown, which is the most conservative existing default. |
| Neutral scale-up | 1 step per 10 s | Reaches the fair share well inside the 60 s convergence time while staying damped. |
| Performance scale-up | 1 step per 2 s | One heartbeat interval per step, which supports the 30 s ramp target. |
| Neutral convergence time and tolerance | 60 s, ±15 percentage points | Share changes go through damped steps; memory relies on cooperative hints. Twice the originally proposed 30 s and ±10 leaves room for both until measured. |
| Neutral margin | 5% of capacity (CPU, GPU compute), plus the floors (memory) | Between the other two margins, so the modes stay ordered; it leaves external demand room to show growth without idling compute that is reclaimed within an interval. |
| Performance margin | 2% of capacity (CPU, GPU compute), plus the floors (memory) | The smallest room that still shows external growth; under contention Performance keeps what external demand does not take. |
| CPU idle reserve | max(`cpu.reserve_physical_cores`, the mode's margin); reserve default 0 | A physical-core reserve and a margin both idle compute; one is enough. |
| Probe hysteresis | The contention floor (1% of capacity) | A satisfied principal gets that much room to show growth before it contends again. |
| CPU admission usage window | p95 over 60 s | CPU is compressible: caps and the kernel share it if everyone spikes, so admission follows recent usage rather than requests. |
| Performance ramp | 80% of available minus margin within 30 s | Admission advances on the 2 s heartbeat and scale-up is damped. The originally proposed 90% in 10 s is unproven. |
| Reclaim bound: RAM, GPU memory | Hint deadline + drain timeout + termination grace + 1 interval (10.5 s) | The sum of the existing deadlines; nothing else waits. |
| Hint deadline | 5 s | Long enough for a cooperative worker to reach a safe point; short enough to bound memory reclaim. |
| Guaranteed hint deadline | 5 s, 1–30 s | The same safe-point argument as the hint deadline. Raising it only delays Guaranteed release above the critical level. |
| RAM breach window before draining Guaranteed work | 10 s, 0–60 s | Rides out short RAM spikes, which page cache and allocator churn cause, without losing Guaranteed progress. GPU memory has no reclaimable cache, so its breaches act at once. |
| Critical floor share | 50% of the floor | Half the floor is still free headroom; below it, other workloads are close to allocation failures. |
| Critical hint deadline | 1 s | Two monitor intervals: enough for a polling worker to see the target, short enough that the critical path stays within 6.5 s. |
| Floor minimums | 512 MiB host RAM, 512 MiB per GPU device | Below these, ordinary processes and driver contexts fail to start. The GPU minimum must be at least what a fresh CUDA context takes on the target GPUs, which T088 measures. |
| Principal leave | 60 s below `principal_min_percent` | Entry stays fast; a principal that pauses briefly keeps its share instead of rejoining the unattributed principal. |
| `uid_min` | 1000 | The usual first regular user ID (`UID_MIN` in `login.defs`). |
| Principal window | 4 monitor intervals (2 s) | Long enough to ignore short commands, short enough that a new user's own share follows quickly; meanwhile its usage counts toward the unattributed principal. |
| Memory peak window | 300 s | Covers typical allocation cycles of interactive and batch work, so admission does not fill memory another workload just released. |
| Memory reclaim backoff | 1 s doubling to 30 s | The coordinator's retry backoff pattern. |
| Principal threshold (`principal_min_percent`) | 1% of capacity | Above measurement noise, low enough that any real external use counts. |
| Saturation (`saturation_percent`) | 98% busy | Within the few percent of sampling error of full use; below it, external demand is still visible as usage. |
| Neutral and Performance escalation (`reclaim_s`) | 5 s, compute only | How long a step has to free the contended amount before the next one; the first step applies at once. Memory moves on at the hint deadline. |
| Drain timeout and termination grace | 3 s and 2 s | The existing `lifecycle.drain_timeout_ms` and `lifecycle.term_grace_ms`, which SC-012 adds up. |
| Upper bounds | `hint_deadline_s` 30 s, `unknown_window_intervals` 20, `min_step_dwell_intervals` 10, `yield_first_response_intervals` 10, `reclaim_s` 60 s, `pause_bound_s` 60 s, `guaranteed_hint_deadline_s` 30 s, `ram.guaranteed_breach_window_s` 60 s, `monitor.interval_ms` and `kernel.monitor_interval_ms` 10 s, `kernel.freshness_intervals` 20, `gpu.process_sample_max_age_ms` 60 s, `lifecycle.drain_timeout_ms` and `term_grace_ms` 60 s, `noise_tolerance_mib` 1024 MiB and half of each GPU floor | Keep every escalation bounded in time (constitution III) and every delay above the critical level. |
| Unknown-measurement window | 3 monitor intervals | Matches today's freshness limit of 3 intervals for kernel observations. |
| Step hysteresis | Reverse only below 50% of the trigger threshold; at least 3 intervals between step changes | Damps oscillation (SC-009) without delaying a real response by more than one confirmation interval. |
| Pause bound | 10 s | Longer pauses risk worker and lease timeouts, so contention escalates to the next step instead. |
| GPU quiet window | 20 monitor intervals (10 s) of healthy observation with no activity | Activity samples are only produced for non-zero use, so quiet must be shown by sustained absence over a complete inventory. |
| No-harm floors | `ram.reserve_mib` / `ram.reserve_percent` (8 GiB / 5%); per GPU the larger of `gpu.reserve_vram_mib` (twice the minimum, 1 GiB) and `gpu.reserve_vram_percent` (5%) | Reusing the existing settings keeps 0.2 configurations meaningful. The defaults meet the recommended sizing of at least max(2 GiB, 5% of RAM) and max(1 GiB, 5% of device memory), above the enforced minimums; a fixed 3 GiB per GPU applies only when configured. |

## Assumptions

- Graduated controls and their native evidence target Linux. Other platforms keep today's
  CPU-only local supervision, with the ladder limited to the steps they support, and report
  which steps are unavailable.
- CedeGrid runs unprivileged. Finer-grained controls are used only where the account already has
  them, such as an explicitly delegated control subtree. Without them, the ladder relies on the
  steps an unprivileged process can apply to its own processes.
- External demand is attributed to principals by process owner where it is visible. Demand whose
  owner cannot be determined counts as one additional principal. Demand by user IDs below
  `uid_min` is background, not a principal.
- The account running CedeGrid is one principal. Own work's fair share is that principal's share
  minus the account's unmanaged usage.
- Mode settings take effect when the agent restarts or reloads its configuration. A job's
  effective mode is fixed when it is admitted.
- Interactive setup targets Linux and macOS terminals. On Windows, setup configures client
  settings only, consistent with today's platform scope.
- Agent-facing help documentation and MCP servers are specified separately, in
  `specs/002-agent-integration`. They build on the setup command-line flags defined here.
- The default memory-noise tolerance is 256 MiB per GPU device. The other defaults are in the
  Provisional thresholds table.
- Out of scope:
  - hard GPU partitioning (hardware partitioning or multi-process service configuration)
  - privileged controls
  - disk and network I/O contention
  - acting on external processes in any form
- The existing lease, drain-deadline and retry rules stay in force except where a requirement
  above changes them.
