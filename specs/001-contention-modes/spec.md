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

**Purpose**: CedeGrid distributes a shared server's resources so that no capacity sits idle, most
usefully where other people's processes already run and it is unclear how much to take. Own work
should run at the highest possible performance, efficiently. The modes set how much own work
gives back: Performance, the default, carries this purpose, and Yield First and Neutral give
back more. Performance's goal is to do better for own work than the same work started on plain
Linux, not only to match it: no gate shape may fall behind plain Linux (SC-018), and the gain
gates measure the gain ([006](../006-performance-first/spec.md) SC-004, SC-005).

## Clarifications

### Session 2026-10-24

- Q: What do configurations without mode settings get, and what happens to `gpu.execution_mode`?
  → A: The default mode is Neutral on every resource. The legacy `gpu.execution_mode` values
  are interpreted as FR-031 lists, with a warning for every value except `auto`. (Superseded on
  2026-10-29: the default is Performance.)
- Q: May a no-harm floor that only a Guaranteed allocation's memory can restore drain that
  allocation? → A: Yes (floor policy A, the only floor policy): after all opportunistic work,
  with a hint first. Keeping Guaranteed memory against a floor, and exempting Guaranteed
  processes from the OOM preference, were removed (constitution 2.0.0, Principle II). (On
  2026-10-29 the OOM preference became OOM order by fault, and in Performance and Aggressive
  in-share Guaranteed memory is not drained for a floor breach others' growth caused.)
- Q: May no-harm floors push Neutral's memory below its fair share? → A: Yes. Floors always
  take precedence over fair share for RAM and GPU memory. (Narrowed on 2026-10-29: in
  Performance and Aggressive only own memory above its fair share gives way at the floors.)
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
  (Narrowed on 2026-10-29 for Performance and Aggressive, FR-023.)
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

### Session 2026-10-26

- Q: Which CPU pressure counts as saturation evidence? → A: Only external PSI: `cpu.pressure`
  of the cgroups outside own work's user slice, the highest `some` avg10 among them. Own
  cgroups' pressure is shown but never counts, since a job with more threads than its share
  stalls on itself.
- Q: What if only system-wide PSI is available? → A: It adds no evidence, since it cannot tell
  own from external waiting; `psi_system_only` says so.
- Q: Over what window does PSI saturate the CPU, and how does that clear? → A: As with
  `saturation_percent`, which judges each observation on its own: external `some` avg10, a
  10-second kernel average, above `contention.cpu.psi_some_percent`. When it falls back, the
  ladder's calm window and hysteresis end the steps it caused, so a signal that crosses back
  and forth does not cycle caps.
- Q: What do missing, unreadable or malformed PSI mean? → A: No extra evidence and nothing held
  back; `psi_unknown` says so.
- Q: What threshold, which resources, which modes? → A: `contention.cpu.psi_some_percent`,
  provisional default 10 (1–100), a detection setting whose values above the default need
  `relaxed_detection`; T117 validates it. CPU only, with the same evidence in every mode.
- Q: Under PSI-only saturation, which contending principals are probed? → A: Every contending
  principal, not only those below one margin step. Own work steps down by the mode's margin
  step per response window. A principal that does not grow into the freed capacity within the
  probe window is recorded as satisfied at its usage plus the contention floor, and contention
  ends. A principal that grows gets its share, as today. This keeps modes monotonic and capacity
  in use when PSI stays high while the external user needs no more.
- Q: Does PSI from a cgroup that holds own work and other users' processes count? → A: Never.
  Such a cgroup counts as own, so its pressure is shown only and utilization still decides.
- Q: What ramp target and time does Performance use on an idle host? → A: 90% of available
  capacity minus the margin within 10 s, the new defaults of `performance_ramp_percent` and
  `performance_ramp_s`; the keys' bounds do not change.
- Q: How often does Performance scale CPU and GPU compute up? → A: One step per monitor interval
  (500 ms by default) instead of one per 2 s. Yield First (30 s) and Neutral (10 s) do not
  change, so the modes stay ordered.
- Q: Do RAM and GPU memory scale up faster in Performance too? → A: No. They keep their current
  Performance scale-up cooldown (2 s) and the reclaim backoff, since memory is slow to reclaim.
- Q: How does admission keep pace if the 2 s heartbeat limits the ramp? → A: One Performance
  heartbeat admits as much as fits within the allowance, at most 32 new assignments (Session
  2026-10-27); `lifecycle.heartbeat_interval_ms` keeps its default.
- Q: What stays exactly as it is? → A: Reclaim (SC-002, SC-012, CPU reclaim of at least the margin
  per interval down to the fair share), the margins, the floors, Guaranteed work and the anti-flap
  rules (minimum step dwell, hysteresis, reversal tolerance). FR-071's one rise step per
  scale-up interval now means per Performance compute interval.

### Session 2026-10-27

- Q: When every process on a GPU is verified own work, is external GPU compute measured as zero
  at once, or does it stay Unknown until the quiet window passes? → A: Measured as zero for an
  interval in which all of these hold: the compute and the graphics process queries both
  succeeded; every listed PID is an own verified PID, checked through its pidfd, with no PID 0 or
  unresolvable entry; no MPS control daemon or server is present; and device memory not
  attributed to own PIDs is no higher than at the last measured or own-only interval plus
  `contention.gpu_memory.noise_tolerance_mib`. The device then stays on the measured path and its
  quiet window does not restart. If any one fails, the existing rules apply unchanged.
- Q: Does the GPU attribution tolerance default change after the first device's measurement? →
  A: No. `attribution_tolerance_percent` stays 15 until a second device is measured; the first
  device's largest gap was 5 points.
- Q: Does an interval that lists no process count as the memory baseline for own-only GPU
  intervals? → A: Yes. An external process the driver cannot see is invisible at a measured
  interval too, so an empty interval is no weaker as a baseline.
- Q: Does SC-002's 2-interval bound apply to unmanaged programs of the account that runs
  CedeGrid? → A: No; it is for other principals' demand. The owner's other programs share the
  owner's fair share with own work and are served as own work's margin frees capacity.
- Q: How many new assignments may one heartbeat take, and may launch work delay a node's
  observations or heartbeat? → A: At most 32 per heartbeat in every mode; Performance fills up to
  32 per heartbeat, so a 16-task scope still fills in one heartbeat. Launches and per-allocation
  lease requests never hold the node's observation, heartbeat or telemetry past the monitor
  interval.
- Q: How is CPU charged for admission while a running allocation has a held CPU step? → A: At the
  larger of its request and its p95 usage from before the step, with no credit, while the step
  holds it back (a pause, or a cap or hint whose target its usage reaches); a cap above its usage
  holds nothing back, so demand that fell on its own still frees room (FR-028).
- Q: How much may held caps and hints on one resource rise in one round, and how is the rise
  split? → A: Together at most the spare (the mode's allowance minus current usage), split
  equally among the held steps, the same basis as fair shares, with no per-allocation weights.
- Q: May a reservation be authorized for the first time while its node's admission is closed? →
  A: Only when every cause closing that resource is the node's own new allocations that have no
  first measurement yet, or the stability cooldown that only they restarted, and only for CPU and
  RAM. The node report states that cause per resource as a structured field; a report without it
  stays fail-closed. Any other cause still refuses, and the request must still fit the budget.
  Placement of new reservations and preemption are unchanged.
- Q: How is own CPU measured for the external estimate, and over what window? → A: In Full, when
  every own allocation is under one delegated parent, from that parent's `cpu.stat` read once per
  interval next to the host `/proc/stat` read, with an error of 1 µs plus one scheduler tick per
  own thread that can run, at most one per scope CPU; CedeGrid's own processes under that parent
  are measured apart. In Tracked, each process's rate over its own read-to-read interval (a process
  in its first interval keeps the aggregate divisor), with the tick lag but no read-pass term in
  the summed bound; a baseline outside the aggregate window stays unknown. The steady-state
  allowance and the admission budget, with the p95 that feeds it, use own CPU and host busy over
  a window of at least 8 intervals with one bound for the window; growth detection and the
  per-interval step decision keep the per-interval bound.
- Q: How does a node exchange lease requests with the coordinator? → A: In one batch per agent
  iteration: renewals first, by lease deadline, then initial authorizations, under one clock
  value. The coordinator answers in one transaction with one synced commit, each item in its own
  savepoint, with a result per assignment, so one refusal affects only its item and a preparation
  record persists as before. A batch holds a bounded number of items under the request size limit
  and splits beyond it; a retried batch is idempotent per item; per-item events stay as before.
  The batch is a negotiated feature: against a peer without it, the agent sends single requests.
- Q: What happens to a reservation the policy cancels before it starts? → A: The agent releases it
  and never requests its initial authorization. A CPU drain planned while an allocation was
  pending, for an allocation that has started running, is re-planned on the running pass, which
  never drains for CPU alone.
- Q: How does the ladder respond when own CPU usage is over the target but external usage did not
  rise? → A: Enforcement first: each capped allocation is held at its cap by the actuator from its
  last measured rate, which is not a new step. Own work steps down only when usage stays over the
  target for `min_step_dwell_intervals` with caps applied. The cause is `own_over_target`, recorded
  apart from `external_growth`, and the scale-up interval does not restart for it once enforcement
  holds. A budget that fell because external usage rose keeps the existing path and SC-002's
  bounds.
- Q: How is CedeGrid's own CPU counted and reported? → A: The agent's and supervisors' CPU is shown
  as in use by CedeGrid, attributed to the owner, never as unattributed external usage, so it
  never forms an anonymous principal. SC-014 counts own tasks plus CedeGrid and states both; with
  117 tasks on 14 CPUs, CedeGrid's own processes use at most 1% of capacity.
- Q: When does a system account's context on a GPU count as background rather than unknown
  activity? → A: Only when per-process GPU utilization is readable for that device and the
  context's utilization stays below `principal_min_percent`. Its memory counts as external usage
  in every case. Where per-process utilization is not readable, it stays unknown activity.
- Q: How fast does own CPU return in Yield First and Neutral once every contending principal has
  gone? → A: When every contending external principal (background does not count) has stayed
  below `principal_min_percent` for `principal_window_intervals`, the next scale-up is one move to
  the uncontended target, without waiting for the mode's interval (the user's decision,
  2026-10-27). The interval still applies while a principal is active, and on memory. SC-009 and
  SC-002 are unchanged.

### Session 2026-10-29

- Q: How do caps already held, and own usage over its target, give up a CPU excess? → A: Running
  CPU work yields tier by tier in FR-016's order. Within a tier, caps already held are lowered at
  one level, the highest first, as held caps rise equally under FR-013, and own usage over its
  target is shared the same way, so no single cap falls to the minimum duty and escalates. A
  lowered cap counts what it frees, so held caps give an increase once between them rather than
  each the whole increase. New victims, with no cap held yet, are still chosen in FR-016's cost
  order, each capped by the whole excess left (reviewer decision D3 A on report 027-1; Session
  2026-11-03 has them share it).
- Q: How does a satisfied principal count while the resource is saturated? → A: A principal whose
  demand is on record as satisfied but who uses less than that demand by more than the contention
  floor counts at the recorded demand in own work's allowance, so a recorded contender that comes
  back gets its demand within SC-002's 2 intervals. When the resource is not saturated, its usage
  counts as it is (reviewer decision D4 A on report 027-1).
- Q: At what nice does own Opportunistic work launch in each mode? → A: Only Yield First tasks
  launch at `cpu.nice` (default 10). Neutral and Performance (and Aggressive) launch at nice 0 and
  leave yielding to the caps. A task that moves to Yield First later keeps nice 0, and its caps do
  the work; a task that moves from Yield First to another mode keeps nice 10, because an
  unprivileged process cannot lower it again, and `status` and `explain` show it (item 029,
  constitution 7.0.0).
- Q: How are own processes ordered for the kernel's out-of-memory killer? → A: By fault. Each own
  task's `oom_score_adj` read at launch is its baseline, never written below. Own work within its
  memory fair share stays at its baseline. When own memory is above its fair share, the own tasks
  that make up the excess go to 1000, in the ladder's memory victim order, for every process in
  each task's family including children started later, and return to their baseline once own
  memory is back within its share. The opt-in `ram.prefer_own_oom_victim = true` (default false)
  puts every own task at 1000, a risk to own work only. Each change is recorded. Without
  privileges in-share own work is protected only up to everyone else's baseline (item 029,
  constitution 7.0.0).
- Q: What is the default mode, and is Performance still a caution option? → A: Performance is the
  node default on every resource and no longer a caution option. Configurations that set a mode
  keep it; one with no mode runs Performance, which the migration note and `doctor` state once,
  as for other changed defaults. An existing `acknowledge_caution` entry `performance_mode` still
  loads (item 029, constitution 7.0.0).
- Q: Do the memory floors take precedence over own work's fair share in every mode? → A: Not in
  Performance and Aggressive. There a floor breach steps own memory only down to its fair share:
  own memory within its fair share is not hinted, drained or sent TERM for a floor breach that
  others' growth caused, on the critical path and for Guaranteed memory under floor policy A
  too, and gets the save-progress signal instead; own memory above its fair share is stepped as
  before, hint then drain in the victim order. The envelope is unchanged. Yield First and Neutral
  keep floors first. The memory fair share is the one memory targets use, else the equal split
  compute uses, and the mode order still holds (item 029, constitution 7.0.0).
- Q: What happens when an out-of-memory condition is not own work's fault? → A: When free memory
  falls below a floor while own memory is within its fair share, in-share own tasks get a
  save-progress signal through the SDKs once per episode; it is not a drain and asks for no
  memory back. An own task counts as OOM-killed if it ended by SIGKILL in an interval in which
  the host's `/proc/vmstat` `oom_kill` count rose, or in Full its leaf's `memory.events`
  `oom_kill` rose. Whether own memory was within its fair share at the last observation before
  the kill, and which principals were above theirs, is recorded. A task killed while own memory
  was within its share is requeued automatically once free memory is back above the floor,
  without counting a failure or a retry, and its status says an OOM it did not cause killed it;
  above its share the kill is a normal failure, with no requeue (item 029, constitution 7.0.0).
- Q: In Performance and Aggressive, may a compute step take own work below its fair share or
  pause it? → A: No. On CPU and GPU compute, in Tracked and Full, a step never targets below the
  fair share. A cap that misses its target, own usage still above it after the hold window, is
  recomputed from the measured rate and applied again as often as needed, without escalating to
  pause. Pause comes only when the cap is already at the minimum duty and own usage is still
  above the fair share, for at most one monitor interval, before returning to the cap. Yield
  First and Neutral keep their escalation (item 029).
- Q: May a held cap for own usage over its target be relaxed and set again while own work stays
  over its target? → A: No. Within the hold window a held cap is not removed or raised because one
  task's usage fell; held caps reverse once own work's total is below its target by the
  contention floor, and own work's total stays at its target within that floor (reviewer
  decision A on report 027, run 5).
- Q: What counts as quiet for the return once contention has ended? → A: Each principal's usage
  and this interval's estimate of external usage no principal accounts for, not the window's.
  Where the per-interval measurement band on the scope is larger than `principal_min_percent` of
  capacity, the quiet threshold is the larger of the two (reviewer decision A on report 027,
  run 6).
- Q: What does the agent do when file descriptors run short? → A: At start it raises its own soft
  `RLIMIT_NOFILE` to the hard limit, and restores the original soft limit in the child before
  the user's command runs. A launch whose descriptors do not fit with headroom is refused with a
  reason. `EMFILE` or `ENFILE` never ends the agent's loop: a failed open while sampling reads as
  unknown for that interval, and while launching refuses that launch (reviewer decision A on
  report 027, run 2).

### Session 2026-10-30

- Q: What happens to a run's work when its supervisor dies? → A: Before exec the worker gate sets
  a parent-death signal: SIGKILL for Opportunistic work, which without its supervisor can be
  neither controlled nor drained and whose expired lease would let a second copy start beside
  it, and SIGCONT for Guaranteed work. The supervisor handles SIGTERM, SIGINT and SIGHUP by
  releasing its runtime steps and draining first. Under the cgroup backend a reattaching
  supervisor thaws a leaf whose journal records no Pause, after verifying membership (reviewer
  decision 1 on report 030-1, with 028 Decision 1).
- Q: What does the coordinator do with a stale node report? → A: It grants no renewals and offers
  no new work for that node, and drains happen only when leases expire. Freshness is the agent's
  own monotonic age of the sample when it sends the report, not a comparison of two wall clocks
  (reviewer decision 2 on report 030-1).
- Q: What happens to running work when the agent stops or restarts? → A: SIGTERM, SIGINT and
  restarts leave supervisors running for re-adoption, and an explicit operator drain still drains
  everything. A configuration reload applies a mode change without a restart. Re-adoption is
  retried on later iterations while a supervisor lives, and replay mode re-adopts too. Keeping
  leases through a longer outage is out of scope (reviewer decision 3 on report 030-1).
- Q: How does Performance treat own CPU while CPU telemetry is unknown? → A: In Performance and
  Aggressive own CPU is held at Neutral's fair share, counting the unknown as one active
  principal, as for GPU activity of unknown amount; with no fair share known yet, no new step is
  taken. Yield First and Neutral keep a CPU budget of 0 (reviewer decision 4 on report 030-1).
- Q: How much node history does the agent keep and report? → A: Fixed constants: a heartbeat
  reports unreleased executions plus the last 256 released ones; observations keep the last
  24 h, with a row cap; an attempt's directory is removed after its outcome receipt and
  publication garbage collection; per-iteration work loads only unreleased and not-yet-reclaimed
  records (reviewer decision 5 on report 030-1).
- Q: How is a run that forks outside the SDK detected? → A: Every interval the supervisor checks
  for processes outside the run's verified family: leaf members in Full, the leader's children in
  Tracked. It reports `single_process_violated` in the status, `explain` and the outcome, and the
  README states the limit, naming PyTorch DataLoader workers as the common case. Whether CedeGrid
  measures and steps verified descendants is a separate decision (item 030, R1).
- Q: How does a stop that drains keep the loop's pace? → A: The agent spreads the stop's drains
  over iterations so that no iteration exceeds the 1 s cadence, and leases and heartbeats keep
  their pace; the burst test's bound covers the whole run, stop included (reviewer decision A on
  report 029, Decision 1).
- Q: How do held CPU caps return once contention has ended? → A: Once the quiet count says
  contention has ended, held CPU caps are raised straight to the mode's target in one move, as the
  allowance already is; own usage, not only the target, returns within 3 s of the contender
  stopping, in Yield First and Performance (reviewer decision A on report 029, Decision 2; Yield
  First's bound counts from its quiet count since item 036).
- Q: How is GPU compute measured beside another GPU user? → A: Each process counts at its share of
  the whole interval, so time it was stopped counts as idle; a remainder within the rounding, one
  point per listed process, is not a principal; a disagreement beyond the tolerance still reads
  unknown (reviewer decision A on report 029, Decision 4).

### Session 2026-10-31

- Q: Which descendants of a run join its verified family, and how? → A: On Linux, in both
  backends, the supervisor sets itself as child subreaper before it forks the leader. Every
  interval and in every stop it walks every family member's `/proc/<pid>/task/*/children` and
  its own children, which finds orphans reparented to it; without a `children` file it falls back
  to a ppid scan of `/proc` and `doctor` says so. A process joins in Principle I's order: pidfd
  first, then the parent chain up to a family member (or the supervisor, for an orphan), then
  alive through the pidfd, then real, effective and saved user and group IDs equal to the run's;
  its start time is recorded and it is acted on only through its pidfd. An orphan is in the
  family, since only this run's descendants can be reparented to its supervisor. Descendants of
  an SDK child join that child's family (item 031, A.1 to A.4 and A.7).
- Q: How does the supervisor reap the orphans it inherits as subreaper? → A: On the thread that
  reaps the leader, only by specific PID (or a pidfd), and only PIDs its own `children` list
  shows that are neither the leader nor an SDK child; never a wait for any child (item 031, A.5).
- Q: How many descendants does a run track, and what happens on errors? → A: At most 256 tracked
  members per run, a fixed constant; members past it are reported, never acted on, and keep
  today's counting. An fd or `/proc` error during discovery holds the step and is reported; it
  never drains own work (item 031, A.6).
- Q: Does rootless execution still require the single-process contract? → A: Not on Linux:
  descendants are tracked whatever `single_process` says, and the field stays accepted. On
  platforms with no subreaper or `/proc` the single-process contract stays required (item 031,
  A.8).
- Q: How is a family's usage measured? → A: Own CPU: in Full the leaf reading, now usable since
  the leaf holds only family members; in Tracked the sum of the members' CPU deltas, a member that
  exits counting up to its last sample. Own memory: the family's summed PSS, never RSS; a family
  of one, and Full while its leaf reading is usable, keep today's reading; how often PSS is read
  is the PSS-period answer below (item 034). Verified descendants'
  GPU memory and compute count as own, and they are not the owner's unmanaged use (item 031, B).
- Q: How does a step reach a whole family in Tracked? → A: A stop (the duty's off phase, a Pause,
  the GPU duty's off phase) discovers, stops every member through its pidfd, and repeats until a
  pass finds no new member, at most 3 passes; a member that appears during an off phase is stopped
  at once, and the family is always stopped and continued together. Continue sends SIGCONT to
  every member whatever the state flags say, past a failing member, which is reported. A member
  that has exited is dropped, not an error. Each member's `oom_score_adj` baseline is recorded
  when it joins and restored, and the preferred-victim setting applies to new members as to the
  leader. Full is unchanged; its thaw on reattach checks membership against the family (item
  031, C).
- Q: What happens to a family's other members when the run ends, drains or loses its
  supervisor? → A: The run ends when the leader exits; members still alive then get SIGTERM, then
  SIGKILL after `term_grace_ms`, the outcome counts them, in both classes, with no opt-out. A
  drain signals the leader first and then SIGKILLs members still alive after the grace. On
  supervisor death the parent-death signal reaches only the leader and SDK children: in Full
  leftovers stay in the leaf, kept for reconciliation; in Tracked they cannot be found again. The
  README and guarantees say so, with a job script's background server and its closing `wait` as
  examples (item 031, D).
- Q: How is a family reported? → A: Status and `explain` show its size (leader, SDK children,
  descendants). Processes outside it are reported as `outside_family` with a count and a reason:
  IDs differ, chain unconfirmed, over the bound, or a platform without tracking. Nothing is done
  to them (item 031, E).
- Q: `oom_score_adj` is inherited across fork, so a member forked while its run is marked a
  preferred victim joins carrying CedeGrid's mark. What is that member's baseline? → A: Its own
  baseline, which excludes CedeGrid's write: a member whose start time falls in one of its run's
  marked spells takes its parent member's baseline; any other member's value when it joins is its
  baseline (item 031, C.4; Principle II's in-share rule).
- Q: A config that never sets `gpu.process_sample_max_age_ms` stops loading today when
  `monitor.interval_ms` is 2000 or more, even on a host without GPUs. What value applies when it
  is not set, and what happens to an explicit value at or below the interval? → A: Unset, it is
  derived as max(2000, 2 × `monitor.interval_ms`), and `doctor` and `validate` show the derived
  value and say it was derived. An explicit value at or below `monitor.interval_ms` is refused on
  every host, as now; the error names both keys and the smallest value that would load.
  `config_migration` is unchanged (item 032, report 030's Decision 4).
- Q: When a Tracked run's supervisor dies, what happens to its members? → A: The agent recovers
  them by identity. It keeps each run's last family (leader, SDK children and verified
  descendants with their start times, the run's boot ID and user and group IDs) in its state
  directory, so an agent that was down when the supervisor died still acts when it starts. When
  reconciliation finds a Tracked supervisor gone, with the agent running or at its start, for each
  recorded process it opens a pidfd first; then requires the start time and the real, effective
  and saved user and group IDs, read through the matching `/proc` directory, to equal the record;
  then checks through the pidfd that it is alive; then signals through the pidfd only. Guaranteed
  members get SIGCONT and are left running; Opportunistic members get SIGKILL. The counts
  (continued, ended, identity mismatch, already gone) go to the run's outcome and an event. Full
  keeps its leaf and the thaw on reattach (item 033, C).
- Q: Does the workload share its supervisor's session? → A: No. A pin showed that a Tracked
  supervisor killed in an off phase orphans the workload's process group, so the kernel sends its
  members SIGHUP and SIGCONT. The worker gate calls `setsid()` after setting the parent-death
  signal and before exec, for the leader and for SDK children; the supervisor keeps its own group.
  Full is unaffected (item 033, A and B).
- Q: When does the agent learn of a member the supervisor just found? → A: The supervisor sends a
  family report right after any stop pass that found a new member, without waiting for an
  answer, besides its per-interval report. A member found and stopped in the instant its
  supervisor dies can be missed and stays stopped until an agent runs; the docs say so (item 033,
  C.1).
- Q: How is each process's share of GPU compute counted over an interval? → A: Against the slots
  of the per-process sampling period, not the device's sample count. The period comes from the
  per-process timestamps in the window, the time since the cursor; the expected slots are the
  window divided by the period, and a process with fewer samples than its slots was idle or
  stopped for the rest, so time without a sample counts as idle. While a period is readable, the
  most-sampled process's count is never the slots. When no process has 2 samples in the window, so the timestamps give no
  period, today's path and its unknown reasons apply; a driver that samples processes at the
  device's rate reads as today. Each interval's measurement keeps the device's sample count and
  window and each process's sample count with its first and last timestamps, in the persisted
  observation (item 034, 1; the slot rule is replaced by the sample-span answer below, item 036).
- Q: Does the return after contention change Yield First's quiet count? → A: No. At the quiet
  count every held CPU cap rises to the mode's target in one move, in the same interval, not in
  steps from the minimum duty. Yield First's quiet count stays as it is, since it is that mode's
  hysteresis; Performance and Neutral keep their paths, and their return times do not get worse
  (item 034, 2).
- Q: How often is a Tracked family's PSS read? → A: Every K intervals. Each PSS pass is timed and
  K is chosen so that PSS reading averages at most 0.5% of the scope's CPU, K from 1 to 10, the
  agent's total within the 1% budget. Between reads own memory is the last PSS plus
  max(0, the members' RSS sum now minus their RSS sum at that read), so growth is never missed;
  copy-on-write breaks are missed until the next read, at most K intervals late. Before a
  floor-breach Hint or Drain, or anything else that acts on own work because of own memory, PSS
  is read fresh, and the step is taken only if that reading confirms it. A family of one keeps its
  current reading, and Full keeps `memory.current` (item 034, 3).
- Q: How does one GPU sample count toward its process's share of an interval? → A: A sample at t,
  whose process's previous sample was at p, counts in full in the window where it arrives:
  sm_util × min(t − p, window) ÷ window, so busy time can shift by up to one window but none is
  lost or counted twice. A sample with no previous one (a PID's first, or the first after an agent
  start) covers from the window's start. Each PID's last sample timestamp is kept across
  intervals, per device, and dropped when the PID leaves the device's process list; a process with
  no sample in the window counts 0, so stopped time stays idle (R33). The device side and the
  tolerance check are unchanged; when one interval fails the tolerance, the summed shares and the
  device are also compared over the last 4 intervals, both sides summed over the same 4, and a
  single interval still reads measured when it passes alone; `quiet_window_intervals` does not
  change. Each interval's `compute_sampling` also keeps each sample's `sm_util` and timestamp per
  PID. This replaces the slot rule above: 034's in-window period path and its fallback are
  removed, and a missing baseline, MPS and an incomplete inventory keep their reasons (item 036,
  1).
- Q: How is Yield First's return after contention bounded? → A: From the quiet count, since the
  quiet count is that mode's hysteresis, kept by item 034: in Yield First own usage is back within
  2 intervals of the quiet count being reached. Neutral and Performance keep their bound, within 3
  s of the contender stopping. The measured total from the contender's end stays in
  `docs/validation.md` as the measured return (item 036, 2).

### Session 2026-11-01

- Q: How do Performance, Aggressive and Neutral read external GPU compute demand? → A: Only from
  intervals in which own work ran uncapped on the device: no cap and no pause, on GPU compute or
  CPU, for any own allocation using the device in the decision that set the interval and the one
  before it, since a lifted cap takes effect within one duty period; or no own allocation running
  on the device. Per external principal and in total (the unattributed part included), the demand
  is the average of its amounts over the last 60 such intervals (30 s at the default interval);
  with fewer, the average of what there is, and the decision record says so; capped intervals never
  feed it. While own work is held back by GPU compute steps only, own work's GPU compute caps on
  the device are lifted every 30 s for a window of 6 intervals (3 s, at most 10% of the capped
  time), whose intervals feed the average as any uncapped interval does; when the window's mean
  differs from the average before the window by 200 thousandths or more, the average restarts from
  the window, so a light user that turns saturating is seen within one or two windows. While a CPU
  step of cap or more, or a GPU pause, holds own work on the device, no window is taken and the
  estimate keeps its age. A change in the device's external PIDs, background contexts left out,
  restarts the average, an agent restart loses it, and with no uncapped interval since it started
  the interval's own amount is used, as before; an interval of unknown amount feeds nothing. Yield First keeps the
  interval's amount, or the average where that is higher, so its target stays at or below
  Neutral's (FR-065), and takes no window. VRAM, CPU, each mode's margin, R33, the sample span and
  the 4-interval tolerance are unchanged; Full and Tracked use the same estimate. Each decision
  record says, per device, whether the interval was uncapped and fed the estimate, and keeps the
  estimate, its interval count and the time since the last uncapped interval (item 039, 3; the
  values from the traces in research R48).
- Q: How long does a GPU compute cap run own work in each duty cycle? → A: In Performance,
  Aggressive and Neutral, the duty is the cap's target ÷ own work's uncapped share, clamped to the
  minimum duty and 1, as a CPU cap uses the measured rate (R20). Own work's uncapped share is its
  amount averaged over the same uncapped intervals that feed the external estimate, those in which
  own work ran on the device (60 at most), restarted with that estimate. With no uncapped share
  yet the duty is target ÷ 1000, as before, which yields more, never less; Yield First keeps target
  ÷ 1000. A 15-point dead band is not used, since own work with several contexts could overshoot
  its target by up to 15 points. The targets, margins, the estimate, the windows, R33, VRAM, CPU and
  RAM are unchanged (item 040, 1).
- Q: How is own GPU work's efficiency beside one other GPU user judged (T368)? → A: In the same
  run, "alone" being the same probe or user with nothing else on the device: own work with
  CedeGrid ÷ own work alone is at least min(the mode's target ÷ 1000, own work without CedeGrid ÷
  alone) − 0.03, the 0.03 covering the 2% margin and CedeGrid's cost (Neutral's margin is in its
  target); the other user's rate with CedeGrid is at least min(its rate without CedeGrid, its fair
  share × its rate alone) − 3%; the old ratio, with CedeGrid ÷ without CedeGrid beside the same
  user, is reported too. On a time-sliced device own work without CedeGrid beside one busy context
  already sits near the fair share, so the earlier 0.5 gates rewarded over-yielding (item 040, 2).
- Q: On a time-sliced GPU, when does GPU compute cap own work in Performance, Aggressive and
  Neutral? → A: A device is time-sliced when it is neither under MPS (the existing MPS evidence)
  nor a MIG parent. Over the same averaged window as the external estimate, m is own work's
  processes with compute samples on the device, E the external processes with compute samples
  there and k the external principals with compute samples there, background (system) contexts
  left out as in the estimate. While m × k ≤ E (own share m ÷ (m + E) ≤ fair share 1 ÷ (k + 1))
  there is no GPU compute cap: the time-slicer gives each busy context its slice, so own work
  cannot take more than its fair share, and a cap only costs own work and the other user's
  latency; with one own context this always holds, since every busy principal has at least one
  context. When m × k > E, own work holds more slices than its fair share, and the estimate and
  the duty from own share apply. On the unknown path the rule holds when m ≤ 1, since unseen
  external contexts can only add to E and k; with m > 1 there the duty stays target ÷ 1000. Under
  the rule the GPU compute margin is not held as idle time, because a newcomer gets its slices at
  once. Yield First is unchanged: it may give others more than their slices, the loss its mode
  allows. VRAM is unaffected. Each decision's GPU compute status carries m, E, k and whether the
  rule held. CedeGrid relies on the driver giving busy contexts equal slices; MPS and MIG keep the
  measured path (item 041, the rule).
- Q: When does unattributed GPU compute usage form a principal? → A: On GPU compute, only at or
  above `contention.gpu_compute.attribution_tolerance_percent` (default 15 points), not the 1%
  contention floor; below it the gap is sampling error the tolerance already accepts. The
  estimate keeps the owner's unmanaged usage out of external demand. The unattributed amount
  still counts in external demand, so it is never idle, and the principal counts for the
  m × k > E path, for Yield First's fair share and for k (item 041, Decision 1).
- Q: When does the duty use own work's uncapped share? → A: With fewer than 6 uncapped intervals
  of own work on the device, a re-measure window opens at once instead of 30 s later, and the
  duty stays target ÷ 1000 until the share exists (item 041, Decision 2).
- Q: What duty does a GPU compute cap set on the unknown path use? → A: Target ÷ 1000, unchanged,
  except that on a time-sliced device with m ≤ 1 the time-slice rule holds and there is no cap
  (item 041, Decision 3).
- Q: In SC-017, what is "the mode's target"? → A: The mode's target for the principals actually
  on the device (500 beside one saturating user), not the recorded target, which a phantom
  principal can lower (item 041, Decision 4).
- Q: At m × k > E on a time-sliced device, how is own work's GPU compute capped in Performance,
  Aggressive and Neutral? → A: An external principal j with E_j busy contexts is slice-bound
  when its averaged uncapped reading (the external estimate, from uncapped intervals and
  re-measure windows only) is at least 0.75 × 1000 × E_j ÷ (m + E). With no slice-bound principal
  there is no GPU compute cap: the slicer already gives every external principal what it asks
  for. Otherwise, with E_b and k_b the contexts and principals of the slice-bound ones, the base
  duty per own context is d0 = (m + E_b) ÷ (m × (k_b + 1)), clamped to the minimum duty and 1,
  which gives own work 1 ÷ (k_b + 1) of the time the slice-bound principals want; principals that
  are not slice-bound keep their slices. This path uses no estimate-based target, no ladder
  excess from one interval's own reading and no pause in any of the three modes; the minimum
  duty is the floor. The mode semantics don't change: Performance still gives others
  min(demand, fair share) plus the margin (item 043, Decision 1).
- Q: When does that duty rise above d0, and when does it fall back? → A: From the device's own
  utilization only, never per-process readings: while own work is held below duty 1 and the
  device shows spare time beyond the mode's margin (Aggressive 0, Neutral its own margin), the
  duty rises one step toward 1; while the device is saturated above d0 it steps down toward d0,
  since an external principal may want more. It settles without oscillating more than one step
  and without cap and uncap cycling, with hysteresis or dwell and stated bounds. A non-bound
  principal that turns slice-bound, or a stepped-up partial user that rises, returns own work to
  d0 within a stated bound, using the re-measure windows; a slice-bound principal that leaves ends
  the cap within one averaged window (item 043, Decision 1).
- Q: What does the time-slice duty do when the amounts are unknown? → A: When the process
  listing is known but the amounts are unknown or inconsistent, d0 comes from the listed counts
  with every listed external principal treated as slice-bound, and there is no step-up without a
  device utilization reading; when the listing itself is unknown, Q25 holds (target ÷ 1000), with
  no pause (item 043, Decision 1).
- Q: What does each decision record for the time-slice duty? → A: Which principals are
  slice-bound, with their readings and thresholds, d0, the current duty and the step-up state,
  beside 041's m, E, k and whether the rule held. m × k ≤ E (no cap), Yield First, VRAM, CPU, RAM
  and the MPS and MIG path are unchanged (item 043, Decision 1).
- Q: How do own allocations' GPU duty cycles on one node line up? → A: When an allocation's duty
  comes from a GPU compute cap, its on- and off-phases follow a clock every supervisor on the node
  shares (CLOCK_MONOTONIC modulo the period): own work runs while (now mod period) < duty ×
  period, so all own allocations on a device stop and resume together whatever their start times,
  and nothing drifts. CPU-only duty cycles keep their per-allocation phase. A duty change
  mid-period never adds a second on-phase and never skips an off-phase (item 044).
- Q: How does d0 leave the mode's margin? → A: Own work's share at d0 is 1 ÷ (k_b + 1) less the
  mode's margin: d0 = (m + E_b) ÷ m × (1 ÷ (k_b + 1) − margin), clamped as before (720 in
  Performance and 675 in Neutral at m = 2, E_b = 1; 750 in Aggressive). This is each mode's
  existing margin, held where the mode's target would hold it, and the duty's steps never go
  below this d0 (item 044).
- Q: Which contexts do E, k, E_b and k_b count? → A: While the device's per-process samples are
  readable, even with inconsistent amounts, only the contexts sampled busy over the averaged
  window (041's counts); the full listing only when the samples themselves are unreadable. Each
  decision records which source it used (item 044).
- Q: Does the slice-bound threshold change? → A: No, it stays 0.75 (`slice_bound_percent` 75)
  (item 044).
- Q: How is the device's utilization read for the time-slice duty's steps? → A: The dwell's
  readings are averaged in per mille, rounded to nearest (sum × 10 ÷ len), not floored to whole
  percent. Utilization comes in whole percent, so spare time under one reading's quantum (10 per
  mille) cannot be told from the dips as phases switch: in every mode the duty rises only while
  spare > max(margin, 10) and counts as saturated while spare × 2 < max(margin, 10). Performance
  (20) and Neutral (50) keep their thresholds; with a zero margin CedeGrid takes device time only
  beyond 1% averaged spare, the reading's resolution. The step stays 5% (item 045).
- Q: Which process set restarts the external estimate? → A: The device's external PIDs with the
  background contexts and the owner's unmanaged processes left out, since the owner's usage is
  already out of the estimate (Q23). The owner's unmanaged processes reach the device's external
  PIDs on Linux, so their coming and going no longer restarts the average (item 045).
- Q: Who pays for the re-measure windows on the time-slice duty, and how? → A: Own work does. A
  window holds own work at full duty so CedeGrid can read external demand: that is CedeGrid's own
  cost, and others keep min(demand, fair share) over time. With w the windows' share of the time
  while the duty path caps own work, d0' = (d0 − w) ÷ (1 − w), so the held duty, windows included,
  averages d0. w comes from the window's length and the gap before the next window: the
  `GPU_DEMAND_WINDOW_DECISIONS` decisions of full duty in each cycle of those decisions plus
  `GPU_DEMAND_WINDOW_EVERY_MS` at the monitor interval (6 of 66 decisions at 500 ms, w = 6 ÷ 66).
  d0' is clamped as d0 is and is the floor the steps fall back to; on a duty path that never opens
  a window, d0' = d0. The record shows d0, d0' and w. The slicer's own overhead exists without
  CedeGrid and stays within the gate's tolerance, so there is no fixed allowance for it
  (item 046).

### Session 2026-11-03

- Q: How is running opportunistic work charged for CPU admission when it is alone? → A: At its
  mean usage over the window while no other principal is active on the resource, and at its p95
  once one is, from the interval that principal appears. With no other principal an over-admission
  can only cost own work: its burst raises `own_over_target` on own tasks and touches no one else.
  The p95 still protects other users as soon as one is active. Running tasks are never drained
  because of it. The other charging rules stay. All four modes stay (item 049, T515).
- Q: What restarts Yield First's scale-up cooldown? → A: A change that forms a principal, as
  today. It is not restarted by the owner's unmanaged processes (an SSH login, a shell command),
  which still count in the owner's share and so still shrink own work's room, nor by any user's
  external change that does not form a principal: one that stays under `principal_min_percent`, or
  ends within `principal_window_intervals`. While a short change lasts, admission may hold, but
  the cooldown timer keeps running. Yield First's give-back to other principals is unchanged,
  SC-002's bounds included. Neutral, Performance and Aggressive keep their rule (the user's
  decision of 2026-11-02, item 049).
- Q: What does the record say when an own task's sample is missing, and may that read as external
  growth? → A: The record says which check of the sampler failed, not the one code
  `allocation_unknown_hold` alone. Own work's own measurement never counts as external growth, in
  any mode: an own task's one-interval unknown measurement does not raise the external estimate in
  the next interval (item 049).
- Q: Is a refusal at the authorization barrier the task's failure? → A: No. It uses up no attempt:
  a replay-safe task is requeued without using one, and its status says why (item 049, T511).
- Q: What do the aligned duty phases assume about clocks? → A: That every supervisor shares the
  agent's `CLOCK_MONOTONIC` time namespace, which holds today, since supervisors are the agent's
  children (item 049, T516).
- Q: How do new victims of a contended CPU excess share it? → A: By usage within their tier, at one
  level, as caps already held do: no new victim is capped by the whole excess left, so a cap to 0
  no longer becomes a pause for its sake. Caps already held in a tier still yield first, and the
  cost order still picks the tiers. Memory victims keep their order, each capped by the excess
  left (item 049, T512).

### Session 2026-11-13

- Q: Which own tasks give memory back first when own memory is above its fair share? → A: The
  over-share tasks, in every mode, so that in-share own work does not lose for another own task's
  excess (the user's decision of 2026-11-03, item 051).
  - While an own task over its share still holds the excess, in-share own tasks get no memory
    hint for it.
  - The over-share task gives back first: it is hinted, as today. Whether it also gets the
    save-progress request, which FR-075 sends to in-share tasks only, is settled in Session
    2026-11-14: it gets its hint and no signal.
  - If it has not freed its excess within the ladder's existing wait, that task is drained. No
    more over-share tasks are drained than it takes to bring own memory within its share, and a
    drained task starts again as soon as its memory fits.
  - Only if own memory is still over its share after every over-share task has been handled do
    in-share tasks get hints.
  - When Guaranteed work may be drained (FR-024) and who gets the save-progress request (FR-075)
    stay as they are.
- Q: Against what is SC-014's RAM and GPU memory fill stated? → A: Against what CedeGrid targets:
  capacity less the floor, less others' use and their peak, and less Yield First's margin. On a
  device, others' use includes memory the device reports used that no process accounts for. The
  CPU numbers stay (the user's decision of 2026-11-03, item 051).
- Q: Does the owner's own unmanaged GPU use make a device "not quiet" for Yield First? → A: No.
  GPU use by the account CedeGrid runs as, outside CedeGrid, no longer holds Yield First's GPU
  admission. It still counts in the owner's own share, as on CPU since 2026-11-02. Other accounts'
  GPU use keeps today's rule, and Yield First still yields to it. Activity that is unknown is
  still never quiet (the user's decision of 2026-11-03, item 051).
- Q: How does a reader take an amount that is given as null in a resource-target file's
  `targets` or `allowance`? → A: As unset, as if the key were absent. The supervisor never
  writes a null amount (item 052; `contracts/resource-target-file.md`, fixture case TF-08).
- Q: When does a process own work launched count toward m on a time-sliced device? → A: From the
  interval the device first lists it, sampled or not. It stops counting once one counting window
  has passed with no sample for it, from its first listing or its last sample, and counts again
  from its next sample. The owner's unmanaged processes and external processes count only while
  sampled. The counts come from every interval whose samples are readable, capped or not; the
  amounts still come only from uncapped intervals and re-measure windows. An own task that holds a
  context without computing costs the other own tasks on that device up to one counting window at
  d0', and no other workload (item 053 Part A, report 049 Decision 2).
- Q: What does it take to lift a restriction the supervisor itself wrote on its cgroup leaf, and
  in which phases? → A: Only that the leaf is the one it created, in every phase; returning a
  recorded control to its launch value never tightens it. A new restriction, and a freeze with no
  record, still need the leaf's members verified first. While a recorded freeze holds, the
  members are read every 50 ms in every phase, and a member list unreadable twice running counts
  as changed. Lifting a held cap while running when an unverified member joins is not done: it
  cannot be done without lifting caps on ordinary own processes, and Session 2026-11-14 settles
  that the cap stays while its step holds (item 053 Part B, report 049 Decision 3).
- Q: In Yield First, beside a principal that is already admitted, which intervals in which own
  work does not fit restart the scale-up cooldown? → A: Only those a principal causes: a user
  became a principal in that interval, or unattributed usage reached the principal window; or an
  admitted principal's usage, or on CPU unattributed usage already past the window, rose beyond
  the jitter tolerance since the last interval in which own work fit. Anything else holds
  admission while it lasts and the count keeps running, whether or not other principals are
  admitted. An unreadable list restarts it, as before. Give-back and the other modes are
  unchanged. On RAM, unattributed usage counts from one level, so its "already past the window"
  half is CPU's only, as Session 2026-11-14 settles (item 053 Part C).
- Q: What is own work's fair share while the principal list cannot be read? → A: At most the last
  one computed from a readable list, on each resource; lower when that interval's own computation
  gives less. The first readable list replaces it. With no earlier readable list the present rule
  stands (item 053 Part D, report 050 Decision 3).

### Session 2026-11-14

- Q: How much is each over-share task asked for, and who pays what the floor needs beyond the
  excess? → A: The over-share tasks, in victim order, are asked together for own memory above its
  fair share and no more, the last of them only for the part that completes it; they are asked
  first, and while their hint or drain wait runs that part is not passed to anyone. What the
  floor, or the mode's target, needs beyond that excess is asked in the same interval by the
  victim order, each capped by the excess left, counting each over-share task at what it holds
  less what it was already asked. In-share tasks are asked for this part where FR-023 gives
  in-share memory up, and not otherwise. No in-share task pays any part of another task's
  excess, and no over-share task is emptied for the margin while another task keeps more (item
  054 Part C, report 051 Decision 5).
- Q: Which own tasks get the save-progress signal when a memory floor is breached? → A: The
  in-share own tasks on that resource, once per episode, whoever caused the breach. An over-share
  task gets its hint, and a drain gives it its drain time (item 054 Part D, report 051
  Decision 4).
- Q: Does the owner's own GPU compute keep another account's idle context from reading quiet in
  Yield First? → A: No. The quiet evidence is other accounts' contexts only: the owner's
  unmanaged compute neither counts as their activity nor restarts their quiet window. A context
  of another account quiet for the window does not hold admission while the owner computes; one
  that is active, or whose activity or owner is unknown, holds it; a new one starts its own
  window. Neutral and Performance are unchanged, and the policy and the launch recheck read the
  same evidence (item 054 Part E).
- Q: What happens to a cap when a process the verification refuses joins its leaf? → A: The cap
  stays while its step holds. It was written only to a leaf read as holding exactly the verified
  family; a process that enters afterwards comes under a limit already there. CedeGrid writes
  nothing for it, thaws a freeze within 50 ms, and lifts every control it wrote when the step
  changes or ends, in every phase (item 054 Part F, report 053 Decision 1; replaces item 053's
  open answer 5).
- Q: Does RAM's unattributed usage restart Yield First's cooldown by its level or by its rise? →
  A: By its level, as T519 set: RAM usage no process accounts for restarts the count in the
  interval it reaches the principal window; the rise rule for unattributed usage past the window
  is CPU's only (item 054 Part F, report 053 Decision 2). FR-034 already says so.

- Q: Does the agent's loop wait for its own pacing of coordinator requests? → A: No. The loop
  never sleeps to pay a control request's charge, and its observation cadence does not depend on
  `max_transfer_bytes_per_second`. Control requests are still charged to the control lane, so
  artifact transfer cannot starve them, and artifact transfer paces as before. When the lane is
  behind, the requests that keep work alive still go out on time: a heartbeat once per heartbeat
  interval, each lease renewal (and each waiting initial authorization, which shares its batch)
  before its deadline, and failure reports at no fewer than 16 a second, so a full node's 256
  reports clear inside the 20 s a default stop leaves them. One coordinator request holds the
  loop for at most one monitor interval. No wire change (item 054 Part A, report 051
  Decision 1).
- Q: How often does a stopping agent heartbeat, and when is the request budget checked for a
  failure report? → A: A stopping agent keeps its heartbeat interval. The request budget is
  checked before an attempt is pinned for a failure report; the pin still comes before every read
  that leads to a publication (item 054 Part A).

- Q: When does the estimate kept for a CPU window's refill give way to another user's usage? →
  A: In the interval in which the other principals' measured usage, from the readable principal
  list, has risen by `principal_min_percent` of capacity since the estimate was taken. Where
  other accounts' usage cannot be read per process, or the list is unreadable, no estimate is
  kept and the interval's own estimate applies. Own measurement's looseness is still not another
  user's usage (item 055 Part B, report 052 Decision 3).
- Q: Does `held_back` show the GPU memory noise tolerance? → A: No. The noise tolerance is a
  growth threshold that withholds nothing, so it is not a held-back part and is never shown.
  Every part `held_back` names is an amount withheld; others' use is shown as `external` and
  `unattributed`, not as a part (item 056 Part B, report 054 Decision 1).
- Q: Beyond its excess, how much of what the floor or the mode's target still needs is an
  over-share task asked? → A: Only what FR-016's cost order asks of it, as before item 051: the
  task with the least progress to lose is asked first whoever caused the excess, so an over-share
  task that leads the order may be emptied for the margin while another task keeps more. No
  in-share task pays any part of another task's excess. This replaces item 054's wording "no
  over-share task is emptied for the margin while another task keeps more" (item 056 Part C,
  report 054 Decision 4).
- Q: What happens when a write to the node journal finds it locked? → A: Every process that
  writes the node journal, the agent and each supervisor, retries after short, randomly spread,
  capped sleeps for up to 5 s, except a supervisor's records while its work runs, which wait
  about 50 ms in a pass and are tried again (Session 2026-11-15); what each transition persists
  is unchanged. An attempt's pin waits about 50 ms: when the journal is still locked, that attempt's
  publication or failure report, and every pin after it in that interval, wait for the next
  interval, and the interval goes on; the pin still comes before every read that leads to a
  publication. Any other journal write of the agent's loop that is still locked after 5 s is
  transient, like a failed open: the interval ends there and the loop goes on. What the interval
  had already sent to its supervisors stands, every step holds, nothing later in it runs (for the
  observation write: no record of the observation, no report to the coordinator, no stop check),
  a `journal_busy` event names the stretch once, and the next interval samples and decides again.
  Any other journal error ends the agent as before. No report goes out in such an interval, so
  the coordinator offers the node no new work; it still renews running work on the last report it
  has, as for any missed heartbeat, and a report sent after a wait longer than `telemetry_ttl_ms`
  is stale under FR-079 (item 056 Part A, report 054 Decisions 2 and 3).
- Q: How does the agent know that other accounts' usage can be read per process, so that an
  estimate may be kept for a window's refill? → A: The readable principal list, both when the
  estimate is taken and in the current interval, holds at least one process of another account
  with a readable user ID. If either list is unreadable, or holds only the owner's processes or
  processes whose user ID cannot be read, no estimate is kept and the interval's own estimate
  applies; the decision's CPU note says why. On a host that hides other accounts' processes
  nothing is kept, so there an own task's unknown sample can read as external growth for one
  window (item 058 Part B, report 055 Decisions 2 and 3).

### Session 2026-11-15

- Q: How long does an agent keep its work leased when its intervals end before the sample? → A:
  One lease. The agent asks for a lease renewal only while the newest sample it has acted on
  (sampled, decided, its steps sent to the supervisors) is no older than
  `lifecycle.allocation_lease_ms`, its own lease and an existing setting; `telemetry_ttl_ms` is
  the coordinator's. Past that it asks for none, the one case in which a renewal that is due is
  not sent, and a `renewals_withheld` event names the stretch once. It drains nothing for this:
  leases run out and each supervisor acts as it does today, Opportunistic work draining at its
  deadline and Guaranteed work running on (FR-079). It asks again in the first interval after one
  that acts on a sample. An interval that ends at the observation write has acted on its sample
  and keeps asking; a sample whose readings are unknown is still a sample, and the unknown window
  applies to it. Until the first sample the agent's start counts as one. With the defaults a lease
  can first run out 15 to 20 s after the last such sample, where since item 056 it never did. The
  coordinator still renews on the last report it has when it is asked, and its rule for a silent
  node stays in `follow-ups.md`. No configuration key and no wire change (item 060 Part B, report
  056 Decision 2(e)).
- Q: Does the bound on intervals without a sample withhold renewals of Guaranteed work too? → A:
  No. Past one lease without an acted-on sample the agent asks for no renewal of Opportunistic
  work; it keeps asking for Guaranteed work, whose lease running out drains nothing and whose
  renewal reply carries the coordinator's drain, cancel or preemption. This replaces, for
  Guaranteed work, the answer above (item 061 Part A, report 060 Decision 2(d)).
- Q: Before a Guaranteed task's last step at a memory floor, does an opportunistic task that holds
  none of the resource count as work left? → A: Not in Yield First and Neutral, by the resource's
  governing mode: there an opportunistic allocation that holds none of the resource (by the usage
  reading the ladder judges it by) is not work anyone can act on for it, and it does not keep the
  Guaranteed allocation from its last step. One whose reading for the resource is missing or
  unknown counts by its charge. In Performance and Aggressive it still counts as today (item 062
  Part A, report 061 Decision 1 B).
- Q: On the critical path, does the TERM for a worker that does not observe the drain file wait
  for a journal record? → A: No. The supervisor releases its steps and sends TERM in the pass that
  reads the step, and records the step and the drain after it; KILL follows the grace counted
  from the TERM that was sent (item 062 Part B, report 061 Decision 2 A).
- Q: From when do a drain's deadlines count, and can a journal write come before its notice? →
  A: From the request: the line as the supervisor reads it, the lease's deadline or the stop
  signal. In that pass the supervisor releases its steps and writes the drain notice; it sends
  TERM at `drain_timeout_ms` and KILL after `term_grace_ms`, and records Draining after the
  notice. A locked journal delays the record, never the notice or a signal (item 062 Part B).
- Q: How long does a supervisor's record wait for a locked journal while its work runs? → A:
  About 50 ms in one pass, as an attempt's pin does, and it is tried again on later passes, the
  newest record replacing an older one of its kind. The launch barrier's records and Released
  keep their 5 s wait, and so do an SDK child's launch records. `journal_unavailable` is reported
  once a record has been refused for 20 s, Opportunistic work then drains as before, and a record
  that lands later withdraws neither. This replaces "and when" in the answer of 2026-11-14 for
  those records (item 062 Part B).
- Q: Does a drain in a renewal reply need time left on the lease? → A: No. The agent hands a
  drain or a cancel in a renewal reply to the supervisor with or without time left, and the
  supervisor takes it so; a grant still needs time left. One that cannot be handed on is
  reported once (`drain_undelivered`) (item 062 Part B).

### Session 2026-11-17

- Q: When has a task complied with a memory hint at its deadline? → A: When its usage of the
  resource is at or below the larger of two targets: the one sent when the hint began and the
  current one. There is no tolerance. A target the task cannot reach, such as 0 for a task that
  holds memory it cannot free, is not complied with (report 056 Decision 1 C).
- Q: What happens at the deadline to a task that complied while own memory is still above its
  share, because another own task grew? → A: It is not drained or terminated for that hint. If
  the over-share order still selects it, it gets a new hint with the current target and a new
  deadline counted from that interval; if not, its hint is lifted as before. A task above its
  target at the deadline escalates as before. The same holds for Guaranteed work, which passes
  its excess on (FR-024) only once the new hint's wait is over, and on the critical path with the
  critical hint deadline (report 056 Decision 1 C).
- Q: From when does SC-012's reclaim bound count for the excess a new hint answers? → A: From the
  interval that first shows that excess. Memory own work takes after a hint was sent is new
  demand, so each part of the excess has the bound from when it appeared. The floor can therefore
  stay breached one hint deadline longer for each growth of another own task, in exchange for not
  draining the task that complied (report 056 Decision 1 C).
- Q: Does Performance cap, pause or drain running own work on CPU or GPU compute for another
  principal's demand? → A: No. Running own work in Performance and Aggressive is not stepped for
  another principal, for own usage above its admission target, or for activity of unknown amount,
  in Tracked and Full and on every GPU device; what another principal gets is the kernel's and
  the driver's split beside the same own work without CedeGrid. Performance holds back at
  admission only: its 2% margin, and no new own work where it would take a contending principal
  below the lesser of its demand and its fair share (constitution 8.0.0 Principle II; report 057
  points 3 and 6, A).
- Q: What does Performance do at a memory floor breach? → A: It gives no own memory back: no
  hint, drain or TERM, on the critical path and for Guaranteed memory too, and no OOM mark except
  under `ram.prefer_own_oom_victim`. Every own task gets the save-progress signal, admission stays
  closed while the floor is breached, and the kernel decides at exhaustion, as it would beside
  the same own work without CedeGrid. Yield First and Neutral keep giving memory way (report 057
  point 1 B).
- Q: What does Performance do while CPU telemetry is unknown or a compute resource has activity
  of unknown amount? → A: It admits no new own work there and holds every step; it takes no new
  compute step (report 057 point 2 A).
- Q: Does Performance keep the GPU time-slice duty? → A: No. In Performance and Aggressive the
  context counts, the slice-bound readings and the external estimate are kept for the record and
  for admission, no duty is set and no re-measure window opens, and one more own process is
  admitted on a time-sliced device only while (m + 1) × k ≤ E or no counted principal is
  slice-bound, recorded as `gpu_time_slice_admission_held` otherwise. Neutral keeps its duty
  (report 057 point 4).
- Q: Does this change how jobs of different modes on one node are held, or the caps of Yield
  First and Neutral? → A: Not in Part B. Report 057 point 7 and C1 to C3 were decided afterwards,
  below.
- Q: Is a Performance job held for other workloads on a node whose mode is Neutral or Yield
  First? → A: No. A job's mode within the node cap decides how that job gives back; the node's
  mode is the default for jobs that set none. A job in Performance or Aggressive is not capped,
  paused, drained or memory-stepped for another principal, for unknown activity or at a floor
  breach, and gets no OOM mark beyond `ram.prefer_own_oom_victim`, whatever the node's mode; the
  operator envelope still steps it. The node's aggregate target and admission do not change, and
  the node's other jobs give what it needs as far as they can, so another workload can get less
  than the node's mode promises: a known limitation, to improve in the next version
  (constitution 8.1.0 "Modes are per job"; report 057 point 7 B).
- Q: How do Yield First and Neutral hold running own CPU work back in Tracked? → A: By whole
  tasks, with no rate cap: the stop-and-resume duty cycle goes. A task is paused in FR-016's order
  only while the own tasks left running keep own usage at or above its target, so rounding
  favours own work; a task that cannot be paused without going below the target is left alone. A
  pause still ends at `pause_bound_s` (report 057 C2 A; at the bound the pause moves to another
  task, below).
- Q: How do Yield First and Neutral hold running own GPU compute work back? → A: By whole tasks
  at both levels, with no duty cycle and no rate cap. On a time-sliced device whose counts are
  known, own tasks are paused until the own contexts left running, each own task counted as one,
  are at most what the time-slice rule allows beside the slice-bound principals and never fewer
  than ⌊E_b ÷ k_b⌋; elsewhere a task is paused only while the others keep at least own work's
  target. Yield First pauses every own task on a contended device, down to the first-come floor.
  A pause ends at `pause_bound_s` as on CPU (it moves to another task, below). In every mode, GPU
  admission on a time-sliced device admits one more own process only while (m + 1) × k ≤ E or no
  counted principal is slice-bound (report 057 C3 A).
- Q: What does the first-come GPU floor keep? → A: In Yield First and Neutral, own work that was
  listed on a device before every principal now listed there keeps at least the share that
  leaves no single later principal more than half of the device: on the time-slice path at least
  2 × E_max − E own contexts running, E_max being the most contexts of one principal, and
  elsewhere, beside exactly one contending principal, at least half the device. Arrival is kept
  per UID and, for own work, per device; one absent for more than 30 s arrives again when next
  listed. A listed context whose UID cannot be read, or an unknown listing, means no floor. The
  only cost to the later principal is slower compute (constitution 8.1.0 "First-come GPU floor").
- Q: What decides the CPU cap in Full, and what must hold before Part C ships? → A: Full keeps
  its `cpu.max` cap until runs E1 (own usage against its target per interval, `cpu.max` against
  stop-and-resume) and E2 (the fork gate's cost per process start) decide report 057 C1; if
  either fails, the results are reported before Full changes. Runs E8 (CPU, F2's shape in
  Neutral, Tracked) and E9 (two own GPU probes in Neutral beside a saturating user: own at or
  above 0.42 of alone, the other user at or above 753.4 req/s) gate the whole-task holds
  (SC-020); a miss in E9 is reported before another GPU target is chosen.
- Q: What happens at `pause_bound_s` to a whole-task pause still answering its demand, after
  run E8 showed the drain there costing own work 5-25% against the former cap? → A: It moves to
  another task: the paused task resumes and the next task in the order takes its place, held to
  cover what the paused one used, so no task is drained and none stays stopped past the bound; a
  task that is draining makes no other task hold. A pause for the operator envelope still drains
  at the bound. E8 runs again; if own throughput stays below the former cap's, that is reported
  before going further (user, 2026-11-17, D1 A').
- Q: Does Full keep a CPU rate cap through `cpu.max`? → A: No. Run E1 showed `cpu.max` not
  clearly closer to its target than stop-and-resume, so by the user's rule no rate cap applies at
  either level and Full holds CPU work back by whole tasks, paused through the leaf's freeze; the
  fork gate is not built. Known gap, for the next version: one task larger than the excess is not
  held, so the other user gets what plain Linux gives (user, 2026-11-17, D2 A'; report 057 C1).
- Q: Without any CPU rate cap, how do whole-task holds keep the cap's other properties (found
  while implementing D2)? → A: A pause moved at its bound is covered by the next tasks only as
  far as own work with it running would be over its target, rounding toward own work unless own
  work a task over its target would fill the host; where no running task is left to cover it, as
  when Yield First pauses every task, the moved pauses take turns, one resuming at a time. A
  pause held for own usage over its target holds while own work is not below its target by the
  contention floor. A moved pause does not count as a step down for admission. What own work
  holds beyond the operator envelope pauses a task whatever else the resource waits on. A pause
  that ends because contention has ended leaves no step to dwell on (implementation, 2026-11-17).
- Q: Run E8 at the D2 commit put Tracked below the former cap: covering a moved pause whole when
  the host would fill left own work about 770 m under its target, and rounding the other way
  swings own work by a task every bound. Which is it? → A: Neither: if the other user's share is
  kept, own work's must be too, without swinging. The paused tasks are the ones whose usage comes
  nearest the excess, each side losing at most half the smallest task; the held amount stays when
  a pause moves; an own-over-target pause holds while resuming would overshoot more than it
  undershoots; and paused tasks have the first claim on the room below the target before new work
  is admitted. E8 in Tracked: own 11959-12057 units/s (former cap 10559-11573), the other user
  5977-6114 req/s (former cap 5884-6018), own more than 2% below its target in at most 4 of 100
  intervals, the error's p95 64-112 m (user, 2026-11-17).

### Session 2026-11-19

- Q: Is Performance's goal to match plain Linux or to beat it? → A: To beat it. In the default
  mode own work must do better than the same work started on plain Linux; matching it in a shape
  is the floor (SC-018), and the gain gates measure the gain (006 SC-004, SC-005) (user,
  2026-11-19).

### Session 2026-11-18

- Q: Whom do Performance and Aggressive hold admission for? → A: Only for another account whose
  CedeGrid work arrived first on the resource; beside plain principals and CedeGrid work that
  arrived later they admit as plain Linux would, and admit immediately lifts the hold except for
  a first-comer running Performance or Aggressive work (constitution 9.0.0;
  [006](../006-performance-first/spec.md) FR-006 to FR-013; FR-022, FR-065).

### Session 2026-11-24

- Q: Do accounts with high user IDs, such as a display manager's dynamic login-screen account,
  count as people? → A: No. User IDs above `uid_max` (default 60000, `UID_MAX` in `login.defs`)
  are system accounts, as those below `uid_min` are: their usage is background, they form no
  principal and they arrive nowhere for the first-come floor (FR-059, FR-090). This covers
  systemd's dynamic users and `nobody` too (user, 2026-11-24).

## Terms

- **Own work**: allocations that CedeGrid launched and verified, including SDK-managed children.
- **External demand**: all other use of the host's CPU, RAM and GPUs. This includes other users'
  processes and unmanaged processes of the account that runs CedeGrid.
- **Resource**: one of CPU, RAM, GPU compute (per device) or GPU memory (per device).
- **Principal**: a user ID from `uid_min` to `uid_max` whose usage of a resource has stayed above
  `principal_min_percent` for the principal window. The account that runs CedeGrid is always one
  principal, whatever its user ID; its share covers own work plus its unmanaged processes. Usage
  by system accounts, user IDs outside that range, is *background* demand, subtracted from
  capacity. A change that forms no
  principal does not restart Yield First's scale-up cooldown (FR-034).
- **Baseline**: the same own work started plainly without CedeGrid in the same place: the same
  account, control group and session layout. Harm to other workloads and loss to own work are
  measured against it (constitution Principle II).
- **Fair share**: a work-conserving, equal split of a contended resource among active
  principals. Capacity a principal does not use is available to the others. It is Neutral's
  target under contention and Performance's admission bound; Performance does not cap running
  work to it.
- **Margin**: capacity own work leaves unused for external demand to grow into, justified only
  by how slowly the resource is reclaimed. Margins are set per mode and per resource: small on
  CPU and GPU compute, and on memory the floors plus Yield First's 20%. In Performance and
  Aggressive the compute margin bounds admission only; running own work is not capped for it.
- **Saturated**: busy at or above `saturation_percent` of capacity (default 98%). CPU is also
  saturated when external PSI `some` avg10 is above `contention.cpu.psi_some_percent` (default
  10%). External PSI is the CPU pressure of the cgroups outside own work's user slice; own and
  system-wide pressure are never evidence, and missing, unreadable or system-only PSI adds none.
  Each observation is judged on its own (FR-072).
- **Contended**: a CPU or GPU compute resource is contended when it is saturated and an external
  principal, or the unattributed principal, uses at least `principal_min_percent` of it (on GPU
  compute the unattributed principal needs `attribution_tolerance_percent`, FR-021) without
  its demand being known to be satisfied, or when it has external activity of unknown amount.
  Saturation by PSI alone still needs such a principal. A principal that makes a resource
  contended contends for it. In Performance and Aggressive contention closes admission beyond
  FR-022's admission target and steps nothing.
- **Satisfied demand**: a principal's demand found by probing: the usage that did not grow into
  capacity own work freed, plus hysteresis equal to the contention floor. It holds while the
  principal's usage stays within it.
- **Held back**: capacity kept from own work or from admission, by cause: floor, reserve,
  margin, contention, peak, noise tolerance, unknown, cooldown, backoff and charge shortfall.
- **No-harm floor**: a hard limit that applies in every mode, such as minimum available host RAM
  or minimum free memory per GPU device.
- **Yield step**: one level of the graduated response, applied per allocation and per resource:
  1. lower share or priority
  2. cap (on CPU in Full only; GPU compute and Tracked CPU take no rate cap, FR-089)
  3. pause
  4. resource hint (a "use less" target sent to cooperative workers)
  5. cooperative drain with checkpoint
  6. termination of the own process
- **Contention mode**: per-resource behavior that decides how much capacity own work may use
  and how fast it gives capacity back. The modes are Yield First, Neutral and Performance;
  Performance is the default.
- **Whole-task hold**: holding own work back by pausing whole tasks, in victim order, only while
  the own work left running stays at or above own work's target, so rounding favours own work
  (FR-089). It replaces a rate cap where none applies.
- **Arrival**: when a principal, by UID, or own work was first listed on a GPU device without a
  gap of more than 30 s since (FR-090).
- **Memory fair share**: own work's share of a memory resource beside the contending principals.
  On RAM it is the share RAM targets use (FR-021); on GPU memory, where principals are not
  formed, it is the equal split of the device among own work and the active external
  principals that GPU compute uses.
- **Over-share task**: in Yield First and Neutral, an own task among those that make up own memory
  above its fair share, in the memory victim order (FR-023). The other own tasks on the resource
  are in-share.
- **Counting window**: the last 60 intervals whose per-process GPU samples were readable on a
  device, 30 s at the default interval. It has the external estimate's length but not its
  intervals, which are uncapped only (FR-021).
- **Save-progress signal**: an SDK event that asks own work to save its progress now. It is not
  a drain and asks for no memory back (FR-075).
- **Floor policy A**: the only floor policy. Floors restore by releasing opportunistic work
  first and Guaranteed memory last.
- **Hint-ready**: the worker has acknowledged its resource-target file, so a hint can reach it.
- **Critical floor breach**: free memory on a resource has fallen below the critical share of
  its floor (default 50%).
- **Risk marker**: the label setup shows on an option that lets own work hold more capacity
  within the floors and reclaim bounds. The only marker is *caution*. No option may put another
  workload at risk; the one memory exception, in-share own memory in Performance and Aggressive
  (FR-023), is a rule of those modes, not an option.

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
   and after `pause_bound_s` the pauses take turns (FR-014), while Guaranteed work keeps its CPU
   request; own work that saturated the resource itself is first held to the margin, and a
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
   **When** external demand grows, **Then** running own work is not stepped, and no new own work
   is admitted while it would take that demand below its fair share. **When** a floor is
   breached, **Then** own memory is not given up and the kernel decides, as without CedeGrid.
4. **Given** Yield First or Neutral, **When** a no-harm floor would be crossed, **Then** own work
   steps down until the floor holds; **given** any mode, **when** new work would cross a floor,
   **then** it is not admitted.
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
6. **Given** own memory above its fair share and an over-share task that does not free its
   excess, **When** its hint wait passes, **Then** that task is drained, in-share tasks receive
   no hint, and the task starts again once its memory fits.
7. **Given** an over-share task that reaches its target while another own task grows, so own
   memory stays above its share, **When** its hint wait passes, **Then** it is not drained; it
   receives the current target with a new deadline, and it is drained only if it is above its
   target when that deadline passes.

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
2. **Given** Yield First, **When** a device has another account's compute activity, or unknown
   activity, **Then** no new own GPU work is admitted to that device; the owner's own unmanaged
   compute alone does not hold it.
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
   unchanged and every resource runs in Performance, the default since 2026-10-29.
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
   - Floors near their minimum, margins below their defaults and relaxed detection are yellow
     and labelled `[caution]`.
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
- **CedeGrid runs under a user ID outside `uid_min` to `uid_max`.** The account is still the
  owner principal.
- **A new user's demand appears.** Until the user has stayed above `principal_min_percent` for
  the principal window, its usage counts toward the single unattributed principal, so the
  response does not wait for the window.
- **An external process appears and disappears every interval.** Steps are damped and do not
  oscillate every interval.
- **A GPU device disappears or its observation fails.** Own work on that device steps down under
  the unknown policy. Other devices are unaffected.
- **The per-node authority restarts while supervisors keep running.** Supervisors hold their
  current step. Steps are re-established once the authority returns, and no allocation is
  terminated only because the authority restarted. SIGTERM, SIGINT and a configuration reload
  leave supervisors running too; an explicit operator drain drains everything (FR-080).
- **A run's supervisor dies.** Opportunistic work ends with it, through the parent-death signal;
  Guaranteed work continues (FR-078). In Tracked the agent recovers the run's other members by
  their recorded identity: it continues Guaranteed members and kills Opportunistic ones, at once
  if it is running, else when it starts (FR-088).
- **A run forks helpers outside the SDK** (for example PyTorch DataLoader workers or Python
  `multiprocessing`). On Linux each verified helper joins the run's family and is measured and
  stepped with it; a helper that fails verification, or is past the 256-member bound, is reported
  as outside the family and never acted on (FR-082, FR-083). Elsewhere the single-process
  contract is required (FR-084).
- **A helper's parent exits first.** The orphan is reparented to the run's supervisor, verified
  with the supervisor as its chain's root, kept in the family, and reaped by its own PID
  (FR-083).
- **The leader exits while its workers run on.** The run ends: the workers get SIGTERM, then
  SIGKILL after the grace, and the outcome counts them (FR-087).
- **A node report arrives stale.** The coordinator grants and offers nothing for that node until
  a fresh report, and drains nothing because of it; leases expire as usual (FR-079).
- **The coordinator is unreachable.** The existing lease rules apply. Local graduated yielding
  continues.
- **A worker ignores hints.** After the bounded wait, the ladder escalates to drain, then to
  termination. If it is an over-share task, in-share tasks are not hinted in its place.
- **A job is a plain command, not SDK-based.** It is never hint-ready. Memory contention
  escalates from the steps that do not free memory straight to drain. On the critical floor
  path, a worker that does not observe the drain file is sent TERM without the drain-file
  wait.
- **A Guaranteed job exceeds its own request.** It is charged at its observed usage. The response
  follows its class rules and FR-057, not a node-wide stop.
- **A finer-grained control is unavailable on the host** (for example, no delegated control
  subtree). The ladder uses the steps that are available and reports which ones it skipped.
- **Others' growth breaches a memory floor in Performance.** Own memory is not hinted or
  drained; every own task gets one save-progress signal (FR-075). If memory keeps filling, the
  kernel's out-of-memory killer decides, with own work at its launch baseline (FR-023).
- **An own task is killed by the out-of-memory killer.** Within its fair share it is requeued
  once free memory is back above the floor, without using a retry; above it, it fails as usual
  (FR-076).
- **Several own jobs share one GPU device.** Steps and targets are per allocation. The device's
  memory floor covers all of them together. On a time-sliced device their processes together are
  FR-021's m, so with two single-process jobs beside one external process, Neutral pauses one of
  them while that process is slice-bound and runs both while it is not; one job with two
  processes is left alone (FR-089).
- **A Performance job on a node whose mode is Neutral.** The job is not held for the other user;
  the node's Neutral and Yield First jobs give what is needed as far as they can, and the other
  user can get less than Neutral's fair share (FR-030).
- **Own GPU work was on the device first.** A user who arrives later gets at most half of the
  device from Yield First and Neutral; one that arrived first, or whose UID cannot be read, gets
  the mode's share (FR-090).
- **Caution option chosen non-interactively without its acknowledgement flag.** Setup refuses
  and writes nothing.
- **Configuration edited by hand to select Performance.** It loads without an acknowledgement
  entry; Performance is the default and no caution option. An existing `performance_mode` entry
  still loads.
- **Configuration edited by hand to lower a margin or relax detection.** A margin below its
  default, or a detection setting beyond its default (FR-045: `saturation_percent`,
  `principal_min_percent`, `noise_tolerance_mib`, `uid_min` or `attribution_tolerance_percent`
  above it, `uid_max`, `memory_peak_window_s` or `quiet_window_intervals` below it), fails to load unless the file contains the entry
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
- **FR-073**: Own CPU for the external estimate MUST be measured as follows.
  - In Full, when every own allocation is under one delegated parent, it is that parent's
    `cpu.stat` `usage_usec`, read once per interval next to the host's CPU counters. Its error is
    1 µs plus one scheduler tick per own thread that can run, at most one per scope CPU. The
    parent is used only while its children are exactly the own leaves, so CedeGrid's own
    processes are never inside it; they are measured apart by their process clocks.
  - In Tracked, each process's rate is taken over its own read-to-read interval, except in its
    first interval, which keeps the aggregate interval. The summed bound subtracts each source's
    read error and the tick lag, not the length of the read pass. A baseline outside the aggregate
    interval is unknown.
  - The steady-state allowance and the admission budget, with the p95 usage that feeds it, MUST
    use own CPU and host busy over a window of at least 8 monitor intervals with one bound for
    the window. Growth detection and the per-interval step decision keep the per-interval bound
    (SC-002).
- **FR-008**: An unknown measurement for one allocation or one resource MUST affect only that
  allocation's or resource's admission and steps. In every mode it MUST first hold the current
  step and block expansion. It MAY escalate only after a bounded, configurable unknown window,
  and MUST NOT by itself drain unrelated work. In Performance and Aggressive an unknown compute
  measurement, or activity of unknown amount, blocks admission on that resource and holds every
  step; it does not escalate to a compute step (FR-022). Yield First and Neutral keep a CPU budget
  of 0 while CPU is unknown.
- **FR-009**: GPU memory headroom MUST account for memory the device reserves for itself.
- **FR-010**: Each external GPU context MUST be classified as quiet, active or unknown. Quiet
  requires positive evidence over a sustained, configurable window of healthy observation.
- **FR-059**: Principals MUST be the CedeGrid account plus user IDs from `uid_min` (default
  1000) to `uid_max` (default 60000) whose usage of a resource has stayed at or above `principal_min_percent` for
  `principal_window_intervals`. Until a user joins, its usage counts toward the single
  unattributed principal, which unreadable owners share; that principal contends once its usage
  reaches `principal_min_percent`, on GPU compute `attribution_tolerance_percent` (FR-021). A
  principal MUST leave only after its usage has stayed below
  that threshold for `principal_leave_s` (default 60 s). Usage by system accounts, user IDs below
  `uid_min` or above `uid_max`, MUST count as background external demand: it is subtracted from capacity and never forms a
  principal. CedeGrid's own processes (the agent and supervisors) count toward the CedeGrid
  account, never toward the unattributed principal. While the principal list cannot be read
  (FR-077), users already counted as principals keep their counts, and own work's fair share on
  each resource MUST NOT exceed the last one computed from a readable list; it is lower when that
  interval's own computation gives less. On CPU and GPU compute the share kept is the one under
  contention. The first readable list replaces it. Before any readable list, the present rule
  applies.
- **FR-060**: Admission headroom for RAM and GPU memory MUST use the peak of external usage over
  `memory_peak_window_s` (default 300 s), including the memory of quiet GPU contexts. Peaks are
  kept per external process, identified by PID and start time: a live process keeps its peak for
  the window, and its peak is dropped as soon as it exits. External usage that no listed process
  accounts for keeps one peak. Yield decisions use current usage.

**Graduated yielding**

- **FR-011**: Yielding MUST use the ladder in order: lower share or priority, cap, pause, resource
  hint, cooperative drain, termination.
  - Only Yield First Opportunistic work MUST launch at `cpu.nice` (default 10). Neutral,
    Performance and Aggressive work launches at nice 0 and yields through the caps. A task that
    moves to Yield First later keeps nice 0; a task that moves from Yield First to another mode
    keeps nice 10, which an unprivileged process cannot lower, and `status` and `explain` show
    its launch nice.
  - A step that cannot free the contended resource, or is unavailable for the allocation or
    host, MUST be skipped, and the skip MUST be reported.
  - RAM contention MUST start at a `memory.high` cap where the allocation runs in a delegated
    cgroup leaf with `memory.high` authorized. The cap MUST NOT go below what the leaf cannot
    reclaim (its unevictable memory and, without swap, its anonymous memory, plus the noise
    tolerance) or below a Guaranteed request. A cap that holds above its target for that reason,
    or throttles for a calm window without freeing the noise tolerance, MUST only stop growth
    and leave the rest to the next step.
  - Otherwise memory contention MUST start at the hint step for hint-ready allocations, and at
    drain for the others. GPU memory never takes a cap, and pause MUST NOT be used to resolve
    memory contention.
  - In Yield First and Neutral, when own CPU usage is over its target and the budget did not fall (external usage did not
    rise and no contention lowered the target), the actuator first holds each capped allocation
    at its cap from its last measured rate, which is not a new step. Own work steps down only when usage stays over the target for
    `min_step_dwell_intervals` with caps applied. The cause is `own_over_target`, and the
    scale-up interval does not restart for it once enforcement holds. A budget that fell, because
    external usage rose or contention lowered the target, keeps the steps and bounds above
    (SC-002). In Performance and Aggressive own usage above the admission target takes no step;
    it closes admission until usage is back within it (FR-022).
  - Running CPU work MUST yield an excess tier by tier in FR-016's order, by whole tasks (FR-089):
    the excess and own usage over its target, the latter once usage has stayed over the target
    for `min_step_dwell_intervals`. Only a task that is paused counts toward the excess; one
    whose pause must wait, as for its dwell, leaves its part to the next in the order.
- **FR-012**: Drain MUST be used only when the lower steps cannot free the contended resource
  within a bounded time. Termination MUST be used only after drain fails within its deadline,
  except on the critical floor path for a worker that does not observe the drain file (FR-063).
  A drain's deadlines count from the request: in that pass the supervisor releases its steps and
  writes the drain notice, sends TERM at `drain_timeout_ms` and KILL after `term_grace_ms`, and
  records Draining after the notice; a locked journal delays the record, never the notice or a
  signal.
  A reservation the policy cancels before it starts MUST be released without its initial
  authorization. A CPU drain planned while an allocation was pending MUST NOT drain it once it
  runs: it is re-planned as running work, which CPU contention alone never drains (SC-007).
- **FR-013**: Steps 1–4 MUST be reversible. When external demand recedes past a reversal
  threshold with hysteresis, steps MUST be undone in reverse order at the mode's scale-up rate.
  The held caps and hints on a resource rise together by at most the spare (the mode's allowance
  minus current usage) in one round, split equally among the held steps.
  A step held for own usage over its target (a pause on CPU, FR-089) MUST NOT be removed within
  the hold window (the step dwell, `min_step_dwell_intervals`) because one task's usage fell
  while own work's total stays over its target; such steps reverse once that total is below the
  target by the contention floor.
  A step taken for an envelope breach or an unknown measurement MUST reverse once that cause has
  stayed clear, whatever the external level. A step taken for a floor breach MUST reverse only
  after free memory has stayed above the floor by at least what the step had to free, or
  `noise_tolerance_mib` if larger, for the calm window, and its reversal MUST also wait the
  episode's reclaim backoff (FR-061), so own work that regrows into the floor backs off.
- **FR-014**: Pause MUST be bounded in time. A pause that does not resolve contention within the
  bound MUST escalate to the next step. On CPU and GPU compute, where no rate cap applies at
  either level (FR-089), a pause that reaches the bound still answering its demand moves to
  another task: the paused task resumes and the next task in FR-016's order takes its place, held
  to cover what the paused one used as FR-089 says, so the hold stays, no task stays stopped
  longer than the bound while another can take its place, and CPU or GPU compute contention alone
  never drains (SC-007). Where every task is paused, as in Yield First on a contended resource,
  the moved pauses take turns, one task resuming at a time. A pause for the operator envelope
  drains at the bound instead, as a yield that uses no attempt. Jobs in Performance and Aggressive take no compute step for other principals
  (FR-022, FR-030), so nothing escalates for them.
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

  Remaining ties MUST break deterministically. This order picks the tiers. Within a CPU tier, caps
  already held yield first, at one level (FR-011), then new victims share what is left by usage,
  at one level, so a cap to 0 does not become a pause for its sake. Memory victims follow this
  order, each capped by the excess left; while own memory is above its fair share the over-share
  tasks come first, asked together for the excess and no more, the last of them only for the part
  that completes it. What the floor, or the mode's target, needs beyond the excess is asked in the
  same interval by this order, counting each over-share task at what it holds less what it was
  already asked, and an in-share task is asked for it only where FR-023 gives in-share memory up.
  No in-share task pays any part of another task's excess, and beyond its excess an over-share
  task is asked only what this order asks of it. Where no rate cap applies, the victims are whole
  tasks (FR-089). Once
  jobs carry their own modes (FR-030), the owner's aggregate share comes from the node mode and
  cap, and allocations yield in mode tiers
  inside it: Yield First, then Neutral, with this cost order within a tier. A job in Performance
  or Aggressive is never a victim for another principal, for unknown activity or for a floor
  breach, whatever the governing mode; only the operator envelope steps it (FR-030).
  Where the governing mode is Performance, the aggregate takes no compute step for others, and only
  jobs in Yield First and Neutral are held, each to its own mode's target (FR-022).
  Class rules still win across tiers: every opportunistic allocation is drained before any
  Guaranteed one, in Yield First and Neutral every one that holds any of the resource (FR-024),
  and a Guaranteed allocation keeps its CPU request in every tier.
- **FR-017**: Observation or control errors MUST hold the current step and block expansion. They
  MUST NOT by themselves terminate Guaranteed work. For opportunistic work they MAY escalate only
  through the bounded unknown policy (FR-008).
- **FR-018**: Yield decisions MUST carry a cause that is preserved from the per-node authority to
  the coordinator. This lets the coordinator tell a yield from a failure for retry accounting.
  Own usage over its target (`own_over_target`) MUST be recorded apart from external growth
  (`external_growth`) in decisions, `observe` and `status`. A refusal at the authorization barrier
  MUST NOT count as the task's failure: a replay-safe task is requeued without using an attempt,
  and its status says why. When an own task's sample is missing, the record MUST say which check
  of the sampler failed, and own work's own measurement MUST NOT count as external growth in any
  mode: an own task's one-interval unknown measurement does not raise the external estimate in
  the next interval. The estimate kept for a window's refill MUST give way in the interval in
  which the other principals' measured usage, from the readable principal list, has risen by
  `principal_min_percent` of capacity since it was taken; an estimate is kept only while the
  readable principal list, when it was taken and now, holds a process of another account with a
  readable user ID, so none is kept where the list is unreadable or other accounts' processes are
  hidden.

**Modes**

- **FR-019**: Operators MUST be able to set Yield First, Neutral or Performance independently for
  CPU, RAM, GPU compute and GPU memory. The semantics of each mode MUST be the same across
  resources.
- **FR-020**: Yield First MUST:
  - limit own work to capacity external demand is not using, minus a generous margin;
  - step down within the mode's response time on any external increase beyond the measurement
    tolerance;
  - on a contended compute resource (see Terms), take opportunistic work down: for CPU, paused,
    then after `pause_bound_s` as FR-014 says, with the pauses taking turns; on GPU compute,
    every own task paused down to the first-come floor (FR-090); Guaranteed work keeps its CPU
    request (FR-024). With own work held to its margin, the resource saturates only once external
    demand outgrows the margin. While own work holds more than capacity minus the margin,
    saturation may be its own doing: it is first held to the margin, within the Neutral fair
    share, by whole tasks (FR-089), and all of it is paused only if the resource stays contended. While a small demand is probed (FR-021), own
    work steps down with the probe instead. External activity that fits in the margin only
    lowers the target by its amount;
  - count unknown activity as external demand;
  - scale up slowly, except once contention has fully ended (FR-034).
- **FR-021**: Neutral MUST:
  - let own work use idle capacity minus the Neutral margin (default 5%, between the
    Performance and Yield First margins) on CPU and GPU compute; memory keeps only the floors;
    on a time-sliced GPU while the time-slice rule below holds, no GPU compute margin is held as
    idle time, since a newcomer gets its slices at once;
  - converge own work to its fair share under contention, and not step it below fair share
    except to keep a no-harm floor;
  - under saturation, probe a contending principal whose unknown demand is below one margin
    step, and under PSI-only saturation every contending principal (FR-072): step own work down
    by the margin per response window, and record a principal whose
    usage does not grow into the freed capacity as satisfied, after which it no longer contends
    until its usage exceeds that demand; outside PSI-only saturation, a larger demand gets the
    fair share at once, and is then watched for one response window from the interval after: if the host has capacity to
    spare at its end and the principal's usage stayed within its baseline plus the contention
    floor, it is recorded as satisfied the same way, else it keeps its share. A satisfied
    principal that leaves keeps its record and rejoins at once when it returns. While the
    resource is saturated, a satisfied principal that uses less than its recorded demand by more
    than the contention floor counts at that demand in own work's allowance, so it gets the
    demand back within SC-002's bound; when the resource is not saturated, its usage counts as it
    is. Records do not
    expire, but at most 64 are kept per resource: past that, the record whose principal was seen
    longest ago is dropped. Activity of unknown amount is never recorded;
  - let external demand reclaim anything above fair share;
  - count unknown activity as one active principal without blocking admission. On GPU compute,
    a task whose pause the supervisor reports applied counts as holding nothing, and each other
    running own task as an equal part of the device; FR-008's hold-then-escalate applies to
    unknown activity from a telemetry failure or a missing observation, except that on a
    time-sliced GPU's time-slice path it holds no more own tasks than that path does.

  In Yield First and Neutral, no-harm floors take precedence over fair share for RAM and GPU
  memory; in Performance and Aggressive FR-023's floor rule applies. On GPU compute no rate cap
  and no duty cycle apply at either level: own work gives a share back by whole tasks (FR-089),
  down to the first-come floor (FR-090). A device is time-sliced when it is neither under MPS (an MPS
  control daemon or server is present) nor a MIG parent (MIG mode enabled); a device whose MPS or
  MIG state cannot be read is not taken as time-sliced. On a time-sliced device the driver gives
  each busy context its slice, so in Neutral own work MUST NOT be
  capped on GPU compute while m × k ≤ E (the time-slice rule), m being own work's processes with
  the owner's unmanaged ones (the owner is one principal, FR-059), E the external processes and k
  the external principals, each counted while it is still listed on the device and has a compute
  sample there within the counting window; a process CedeGrid launched and verified also counts
  toward m from the interval the device first lists it, sampled or not, stops counting once one
  counting window has passed with no sample for it, from that listing or from its last sample, and
  counts again from its next sample; background contexts left out and an unattributed principal
  counted as one principal with one process: own work's share m ÷ (m + E) is then at most its fair
  share 1 ÷ (k + 1), and with one own process this always holds. While the amounts are unknown or
  inconsistent but the per-process samples are readable, the counts are the same busy contexts.
  Only when the samples themselves are unreadable do they come from the listing, and then the rule
  holds when m ≤ 1, m counting the processes listed there, since unseen external contexts can only
  add to E and k. VRAM is unaffected,
  and Yield First keeps its targets. When m × k > E, own work holds more slices than its fair
  share, and in Neutral it MUST give slices back by whole tasks from the context counts (the
  time-slice hold):
  - An external principal j with E_j busy contexts is slice-bound when its reading in the
    external estimate (uncapped intervals only) is at least
    `contention.gpu_compute.slice_bound_percent` (default 75) percent of its slice,
    1000 × E_j ÷ (m + E). A lone external principal reads the estimate's total, since usage
    below the attribution tolerance is its sampling error (Q23). An unattributed principal
    counts as one with one context.
  - With no slice-bound principal nothing is held. Otherwise own tasks on the device are paused
    in FR-016's order, each counted as one context, while the own contexts left running stay at
    or above t = ⌊E_b ÷ k_b⌋, E_b and k_b being the slice-bound principals' contexts and number,
    and until they are at most t: own work keeps the slices the rule allows it, and rounding
    favours it. FR-090's floor can raise t. A task whose pause would leave fewer than t running is
    left alone, so a single job with several contexts is not held. Principals that are not
    slice-bound keep their slices.
  - A paused task resumes once no counted principal is slice-bound or the rule holds with it
    running again, and its pause ends at `pause_bound_s` (FR-014). A principal that turns
    slice-bound holds own tasks within the estimate's next window, 33 s, and one that leaves
    releases them within one averaged window. This path uses no estimate-based target and no
    ladder excess from one interval's own reading.
  - While the per-process samples are readable the counts are those of the counting window, from
    every interval whose samples are readable, whether own work is held or not; the amounts,
    which decide slice-bound, come only from uncapped intervals. While the estimate holds intervals its readings decide which principals
    are slice-bound, even in an interval whose amounts are unknown or inconsistent, until uncapped
    intervals have gone without a measured amount for `gpu.process_sample_max_age_ms` since the
    last one that fed it: the estimate then holds only the past, and every counted external
    principal is slice-bound until an uncapped interval is measured again. Intervals in which own
    work there is held do not feed the estimate and leave its readings standing. When the
    estimate has no uncapped interval since it restarted, or when the samples themselves are
    unreadable and the counts are the listed processes, every counted external principal is
    slice-bound. An interval without readable samples within `gpu.process_sample_max_age_ms` of
    the last readable one reads as that one, for the contexts still listed, in the observation
    itself, so the policy and the launch recheck read it alike, and the policy also keeps that
    interval's counts; NVML can have no new sample every other interval. Only past that age do the
    listed processes count. In the same way, external activity read as unknown only for want of a
    new sample stays active within the sample age of an active reading; a quiet reading does not
    carry over, since unknown is never quiet. An own process that holds a context without computing counts for at most one
    counting window after its listing; for that time it counts toward t, so one fewer own task
    may be held beside a slice-bound principal, and no other workload pays beyond that window.
    When the listing itself is unknown on a device last read as time-sliced, nothing new is
    paused there (Q29) and held pauses hold.
  - Each decision's GPU compute status states m, E, k, whether the rule held, the source of its
    counts, for each own process counted whether it counts by its listing or by a sample, the
    slice-bound principals with their readings and thresholds, t, the floor when one applies,
    and the own tasks held.

  On a device not known to be time-sliced, own tasks are held by FR-089 against own work's
  measured amount there, each running task counting at an equal part of it. External GPU compute MUST be measured, in thousandths of
  device time, from each external process's fresh per-process utilization over the whole
  interval, time without a sample (such as while it was stopped) counting as idle, capped at
  100%: a sample at t, whose process's previous sample was at p, counts in full in the window
  since the cursor where it arrives, as sm_util × min(t − p, window) ÷ window, a process's first
  sample (or its first after an agent start) covering from the window's start; each PID's last
  sample timestamp is kept across intervals per device and dropped when the PID leaves the
  device's process list. Each interval's persisted observation keeps the device's sample count
  and window and, per process, its previous sample and each sample's timestamp and `sm_util`. The
  amount is measured when the summed per-process utilization of all processes is within
  `contention.gpu_compute.attribution_tolerance_percent` (default 15, 1–30) points of device
  utilization over the same interval, or, when that interval alone is not, when the shares and the
  device summed over it and the 3 intervals before it are; the amounts are then those 4 intervals'
  averages, so a sample missed in one interval does not read its process as idle. While the device's process inventory lists an external
  process, the external amount MUST be at least device utilization minus own processes' sum,
  the part nobody accounts for being unattributed external usage, except a remainder within the
  rounding (one point per listed process), which is not; with own processes only, the remainder
  is own work. Unattributed usage forms one principal only at or above
  `attribution_tolerance_percent` points of device time, below which it is sampling error the
  tolerance already accepts, and counts in external demand either way. External GPU compute MUST be measured as zero, without fresh samples,
  for an interval in which the compute and the graphics process queries both succeed, at least
  one PID is listed and every listed PID is an own verified PID (checked through its pidfd, with
  no PID 0 or unresolvable entry), no MPS control daemon or server is present, device utilization
  is available, and device memory attributed to no listed PID (here, the memory not attributed
  to own PIDs) is no higher than at the last interval that was measured, own-only, or listed no
  process with both queries successful, plus `contention.gpu_memory.noise_tolerance_mib`; such an
  interval keeps the device on the measured path and does not restart its quiet window. Otherwise it MUST stay activity of unknown amount
  when either query is unsupported, samples are not fresh, the process inventory is incomplete,
  or an MPS control daemon or server is present. A context of a system account (FR-059) counts as
  background, not as unknown activity, when per-process utilization is readable for that device
  (the query succeeds or reports no samples) and the context's utilization stays below
  `principal_min_percent`; its memory still counts as external usage, and where per-process
  utilization is not readable it stays unknown activity. A device MUST switch to the unknown path at once
  and return to the measured path only after `quiet_window_intervals` consecutive measured
  intervals. A measured amount takes the CPU paths: external processes are
  principals by UID with the same `uid_min`, `uid_max`, `principal_min_percent`, join window and leave
  window as CPU, a PID whose UID cannot be read is unattributed, saturation is at
  `saturation_percent` of device utilization, and probing and satisfied records apply;
  admission still follows FR-026 and FR-027. On a time-sliced device a process's reading follows
  where the sampler caught its context, so gaps in own work raise another user's reading at the
  same demand; in Neutral, the external amount that sets own work's
  GPU compute target, per principal and in total (the owner's unmanaged usage left out, the
  interval's own amount of it counting as in the interval's view), MUST therefore be the average
  of the amounts measured in the last 60 intervals in which own work ran uncapped on the device (no
  pause on GPU compute and no CPU step of cap or more for any own allocation using it, in the
  decision that set the interval and the one before it), or had no allocation running there; with
  fewer, the average of what there is, and with none since the average started (the agent's start
  or a restart), the interval's own amount. Held intervals and intervals of unknown amount never
  feed it. No re-measure window is taken: with no GPU compute cap there is none to lift, and a
  paused task resumes only as FR-089 says. A change in the device's
  external PIDs, background contexts and the owner's unmanaged processes left out, restarts the
  average, and an agent restart loses it. Yield First uses the
  interval's amount, or the average where that is higher. The decision record
  states per device whether the interval was uncapped and fed the estimate, the estimate, its
  interval count and the time since the last uncapped interval. The node report's held-back breakdown
  states, per device, whether the amount was measured or why it is unknown.
- **FR-022**: Performance MUST:
  - beside plain principals and CedeGrid principals that arrived after own work
    ([006](../006-performance-first/spec.md) FR-006, FR-007), admit new own work on CPU and GPU
    compute as plain Linux would start it: up to the capacity less the operator reserve, within
    the envelope and the ancestor quota, with no admission margin, no deduction of their usage,
    CedeGrid's own CPU left in, and without the time-slice admission hold, recording
    `admitted_as_plain`; own CPU then counts at no more than each request, so usage above the
    requests takes no room from tasks still to start (006 SC-002). The bullets on admission below apply only beside a CedeGrid principal
    that arrived first, counting only such first-comers, and record `held_for_first_comer`;
    while the principal list cannot be read, they count every principal. The decision also
    carries an admit immediately budget that counts only protected first-comers, those running
    Performance or Aggressive work there, and on a device held for one carries no VRAM
    (`admit_immediately_ignored_for_protected_first_comer`). Each compute resource's status lists
    own work's arrival and each other account that contends or runs CedeGrid work there, with
    its kind, arrival, first-comer and protected flags and whether admission holds for it;
  - beside a first-comer, let own work use all capacity its demand is not using, minus a small
    admission margin (default 2%) on CPU and GPU compute; running own work is not capped for the
    margin. Memory keeps the floors as FR-023 states for this mode;
  - scale up quickly;
  - not cap, pause or drain running own work on CPU or GPU compute for another principal's
    demand, for own usage above its admission target, or for activity of unknown amount, in
    Tracked and Full and on every GPU device. What another principal gets beside it is the
    kernel's and the driver's split, as beside the same own work without CedeGrid;
  - admit new own work on a compute resource only within max(S, A − margin), S being own work's
    fair share counting each contending principal at the lesser of its demand and its share, so
    that new own work never takes a contending principal below that. On a time-sliced GPU it
    admits one more own process only while (m + 1) × k ≤ E (FR-021's counts) or no counted
    principal is slice-bound by its latest readings as well as the estimate's (FR-089), and
    records `gpu_time_slice_admission_held` otherwise;
  - while CPU telemetry is unknown, or a compute resource has a first-comer's activity of
    unknown amount, admit no new own work there and hold every step (FR-008);
  - keep the operator envelope (FR-057) on memory and GPU memory, the Guaranteed rules (FR-024)
    and explicit operator drain, as every mode does; own CPU above the envelope takes no step
    (006 FR-007a).
- **FR-065**: For every resource and every observation, own work's target MUST satisfy
  Yield First ≤ Neutral ≤ Performance. Configuration enforces it by construction: the margins
  are validated as Yield First ≥ Neutral ≥ Performance, with Yield First above Performance.
  Under contention Neutral holds the fair share and Performance takes no compute step, so its
  running target is the whole capacity and its admission target, max(S, A − m_P·C) beside a
  first-comer and the capacity less the reserve otherwise (006 FR-007), is at least Neutral's S. On memory, Yield First and Neutral give way at the floors and Performance does not
  (FR-023), so Yield First ≤ Neutral ≤ Performance still holds.
- **FR-023**: No-harm floors MUST apply in every mode:
  - a minimum available host RAM;
  - a minimum free memory per GPU device, so that other workloads' allocations do not fail;
  - out-of-memory order by fault, where the platform lets an unprivileged process set its
    `oom_score_adj`: each own task's value at launch, and each family member's when it joins, is
    its baseline, never written below; a member that started while its run was marked takes its
    parent member's baseline instead, since its value then is CedeGrid's mark. Own
    work within its memory fair share stays at its baseline. In Yield First and Neutral, while own
    memory is above its fair share, the own tasks that make up the excess, in the ladder's memory victim order, are at
    1000 for every process of their family, children started later included, and return to
    their baseline once own memory is back within its share. `ram.prefer_own_oom_victim = true`
    (default false; setup marks it as a risk to own work) puts every own task at 1000. Every
    change is recorded with the task, the old and new values, the cause, and own memory beside
    its fair share. Without privileges, in-share own work is protected only up to everyone
    else's baseline: the kernel can still take it as the largest process.

  In Yield First and Neutral own memory gives way at the floors: the over-share tasks first, as
  below, then the in-share ones. In Performance and Aggressive own memory MUST NOT be hinted,
  drained or sent TERM for a floor breach, on the critical path (FR-063) and for Guaranteed
  memory (FR-024) too, and no own task is marked for the OOM killer, except under
  `ram.prefer_own_oom_victim`. Every own task gets the save-progress signal (FR-075), and the
  kernel decides at exhaustion, as it would beside the same own work without CedeGrid. The floors
  still close admission (FR-066), and the operator's envelope (FR-057) applies in every mode. In
  Yield First and Neutral an over-share task gives back first: it
  is hinted, and drained when its wait passes and it has not complied (FR-037). No more
  over-share tasks are drained than bring own memory within its share, counting drains already decided, and
  in-share tasks are not capped, hinted or drained for that excess. The over-share tasks are asked
  for the excess and no more, the last of them only for the part that completes it, and while
  their hint or drain wait runs that part is not passed to anyone. What the floor or the mode's
  target needs beyond the excess is asked in the same interval as FR-016 says. The one exception is FR-024's
  class rule: an over-share Guaranteed task that may not be drained yet passes its excess on once
  its hint wait is over.

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
  - MUST NOT be paused, so GPU compute steps, which are whole-task pauses (FR-089), are
    unavailable for Guaranteed work;
  - MAY be lowered in share and hinted;
  - MAY have CPU usage above their request given a lower share, but MUST NOT be paused, and CPU
    takes no rate cap (FR-089);
  - MUST NOT be drained by automatic yielding, except to restore a RAM or GPU memory floor or the
    envelope under floor policy A (never for a floor in Performance and Aggressive,
    FR-023), as the last step: after all opportunistic work on the
    resource that anyone can act on has been released (in Yield First and Neutral, by the
    resource's governing mode, an opportunistic allocation that holds none of the resource is not
    such work, and one whose reading is missing counts by its charge), after a hint whose
    `guaranteed_hint_deadline_s`, counted from that hint, has passed when the allocation is
    hint-ready, and, for RAM, after the breach has lasted `ram.guaranteed_breach_window_s`,
    counted from the start of the breach. GPU memory breaches have no window. On the critical floor
    path (FR-063), the critical hint deadline replaces the Guaranteed hint deadline and the
    window is skipped, and a Guaranteed allocation that does not observe the drain file may be
    terminated as that path's last step.
  - The envelope exception applies to RAM and GPU memory only, with the same deadlines and
    window; a CPU envelope breach caps usage above the request where a rate cap applies, and
    never drains Guaranteed work.

  Explicit operator drain keeps today's behavior.
- **FR-025**: Every threshold that steers yielding MUST be a configuration value, not a
  hard-coded constant. This covers:
  - margins, response times and scale-up rates
  - the GPU compute time-slice hold's slice-bound share (FR-021); `slice_step_percent`, the
    former duty's step, still loads and has no effect
  - tolerances, windows and deadlines, including the Guaranteed hint deadline, the RAM breach
    window, the critical floor share and hint deadline, the principal window,
    `principal_leave_s`, `uid_min` and `uid_max`, the memory peak window and the CPU admission usage window
  - convergence targets
  - memory scale-up cooldowns and reclaim backoff

  Each is set per resource where it applies, defaults to the provisional value in this spec, and
  is documented. Margins are ordered as FR-065 requires. A margin below its default, or a
  detection setting beyond its default (FR-045), is a caution option. Fixed mechanism constants
  are exempt and documented in the data model: a cap binds at 95% of its target and counts as
  effective up to 105%.

**GPU sharing and placement**

- **FR-026**: GPU work MUST be admissible to a device that hosts external contexts when the mode
  allows it and the requested memory fits above the device's floor. An empty device MUST NOT be
  required under Neutral or Performance.
- **FR-027**: Under Yield First, new own GPU work MUST NOT be admitted to a device with active or
  unknown external compute activity. Compute by the account CedeGrid runs as, outside CedeGrid,
  does not hold this admission: it counts in the owner's own share (FR-059), not as activity
  here. Other accounts' activity holds it as before, and activity whose owner or amount cannot be
  read stays unknown. The quiet evidence Yield First waits for is other accounts' contexts only:
  the owner's unmanaged compute neither counts as their activity nor restarts their quiet window.
  Another account's context that has been quiet for the window does not hold admission while the
  owner computes; one that is active, or whose activity or owner is unknown, holds it, and a new
  one starts its own window. The policy and the launch recheck MUST read the same evidence.
- **FR-028**: Node reports MUST state admission per resource. Coordinator placement MUST honor it.
  Placement is blocked while own work on that resource is being reclaimed, meaning above its
  target or with a step still moving down. A step holding at its target does not block. A move
  within the noise tolerance (memory) or within the reversal hysteresis of the external growth a
  compute step answered is jitter and counts as neither.
  - Reports MUST also state, per resource, when its admission is closed only by the node's own
    new allocations that have no first measurement yet, or by the stability cooldown only they
    restarted. At the first authorization of an existing reservation, and only there, the
    coordinator MUST count such a CPU or RAM resource as open, so a burst's own launches do not
    refuse the rest of the burst. Any other cause keeps it closed: contention or external growth,
    an own step that holds work back or the cooldown it restarted, a floor breach, unknown host
    measurement, a full spool, an unknown or closed GPU device, or a report without the field.
    The request MUST still fit the budget, and placement of new reservations and preemption keep
    the unchanged rule.
- **FR-029**: Nodes MUST advertise only the control capabilities they can actually apply.
- **FR-064**: `contention.never_share_occupied_gpus` (default false) MUST, when set, block new own
  GPU work on any device with an external context or unknown external processes, in every mode.
  Own work already running on a device that becomes occupied is not drained for that alone;
  the device's mode and floors govern it.

**Configuration and compatibility**

- **FR-030**: A node MUST have a default mode per resource, Performance unless configured. A job
  or task MAY override it per resource. A node MAY set a cap on the most aggressive mode allowed
  (Yield First < Neutral < Performance); without one, the cap is the node's most aggressive
  configured mode. A job runs at the less aggressive of its
  requested mode and the node cap, and the effective mode MUST be reported. A requested mode
  the cap refuses MUST be reported with the reason code `mode_capped` and the node's cap.
  A job's effective mode decides how that job gives back (constitution 8.1.0): a job in
  Performance or Aggressive is not capped, paused, drained or memory-stepped for another
  principal, for unknown activity or at a floor breach, and gets no OOM mark beyond
  `ram.prefer_own_oom_victim`, whatever the node's mode; the operator envelope still steps it,
  and an OOM kill of it at a floor breach is judged as FR-076 judges one in Performance. The
  node's aggregate target and admission do not change, and the node's other jobs give what it
  needs as far as they can, so another workload can get less than the node's mode promises:
  a known limitation, to improve in the next version.
- **FR-031**: Existing `config_version = 1` files MUST load unchanged.
  - Without mode settings, every resource MUST run in Performance. The migration note and
    `doctor` state once that the default changed from Neutral, as for other changed defaults.
  - `gpu.execution_mode` MUST still be accepted and is interpreted for the GPU resources that
    have no explicit mode:

    | Legacy value | Interpretation | Warning |
    |---|---|---|
    | absent or `auto` | the default mode (Performance) | none |
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
  resource became stable. In Yield First an interval in which own work does not fit restarts
  that count only when a principal is the cause: a user became a principal in that interval, or
  unattributed usage reached `principal_window_intervals` in it; or the usage of a user that is
  already a principal, or on CPU of unattributed usage already past the window, has risen by more
  than the jitter tolerance since the last interval in which own work fit. The tolerance is
  `noise_tolerance_mib` on RAM, and on CPU the larger of `principal_min_percent` of capacity and
  the scope's per-interval measurement band. Anything else holds admission while it lasts and
  the count keeps running, whether or not other principals are present: the owner's unmanaged
  processes, a change by a user that is not a principal, and unattributed usage shorter than the
  window. On RAM, unattributed usage counts from its level, so a rise restarts the count in the
  interval it reaches the window. An unreadable principal list, an unknown measurement or a drain
  restarts it. Yield First's give-back to other principals is
  unchanged. CPU and RAM use `scale_up_cooldown_ms`, by default the mode's scale-up
  interval: 30 s in Yield First, 10 s in Neutral, and in Performance one monitor interval for CPU
  and GPU compute but 2 s for RAM and GPU memory. GPU memory uses `contention.gpu_memory.scale_up_cooldown_ms`, else a
  set legacy `gpu.scale_up_cooldown_ms`, else the mode's interval, per device. RAM and GPU
  memory add the reclaim backoff (FR-061). Each resource's admission opens on its own cooldown.
  For CPU admission, running opportunistic work is charged, once it has a full window of samples
  over `contention.cpu.admission_usage_window_s` (default 60 s), measured over the window of
  FR-073, at its mean usage while no other principal is active on CPU, and at its p95 usage from
  the interval one appears. Before a full window it is charged at max(request, observed), so work
  in its startup phase is never under-charged. An over-admission while alone can only cost own
  work: its burst raises `own_over_target` on own tasks, and running tasks are never drained
  because of it. While a CPU step holds it back (a pause, or a cap or hint whose target its usage
  reaches) it is charged at the larger of its request and its p95 usage from before the step,
  with no credit, so usage a cap holds back never becomes admission room; that charge closes
  admission but does not count as own usage beyond the budget. Pending work is charged in full, Guaranteed work at max(request, observed), and
  RAM and GPU memory admission stays request-based. Coordinator placement and the agent's launch
  check MUST admit CPU the same way. RAM admission and the agent's worker limit bound how many
  jobs this CPU credit can admit. In Performance, one heartbeat MUST admit as much as fits within
  the allowance, up to 32 new assignments; in the other modes the agent adds one worker per CPU
  and RAM cooldown. Launches and lease requests MUST NOT hold the node's observation, heartbeat or
  telemetry past the monitor interval; lease requests go as one batch per iteration (FR-074).
  The loop MUST NOT sleep to pay a coordinator request's charge on the control lane, and its
  cadence MUST NOT depend on `max_transfer_bytes_per_second`; one coordinator request holds it
  for at most one monitor interval. While the lane is behind, a heartbeat still goes once per
  heartbeat interval, each lease renewal and waiting initial authorization before its deadline,
  and failure reports at no fewer than 16 a second.
  On CPU in every mode, once every contending external principal (background does not count)
  has stayed below `principal_min_percent` for `principal_window_intervals`, the next scale-up is
  one move to the uncontended target, without waiting for the mode's interval, for the allowance
  and for held caps alike, so own usage, not only the target, returns within 3 s of the
  contender stopping in Neutral and Performance, and in Yield First, whose quiet count is that
  mode's hysteresis, within 2 intervals of the quiet count being reached: in that interval, while
  own work holds no excess, every CPU pause held for external growth ends; steps held for other
  causes keep their own paths. The quiet count is each mode's hysteresis and does not change with
  it, and Performance's and Neutral's return times do not get worse.
  Quiet intervals count each principal's usage and this interval's unattributed external
  estimate, not the window's, against the larger of `principal_min_percent` of capacity and the
  scope's per-interval measurement band. The
  interval still applies while a principal is active, and on memory; SC-009 and SC-002 hold.
- **FR-074**: A node MUST send each iteration's lease requests as one batch where the coordinator
  negotiates it: renewals first, by lease deadline, then initial authorizations, under one clock
  value. The coordinator MUST answer a batch in one transaction with one synced commit and a result
  per assignment. Each item runs in its own savepoint, so a refusal or a failed grant affects only
  that item, and a preparation record persists as for a single request. A batch MUST hold at most a
  fixed number of items under the request size limit, larger sets split, and a retried batch MUST
  be idempotent per item. Per-item events MUST be the same as for single requests. Against a peer
  without the feature, the node MUST send single requests (FR-032).
- **FR-075**: When free memory on a resource falls below its floor, the node MUST send a
  save-progress signal through both SDKs, once per episode, whoever caused the breach, to the
  in-share own tasks on that resource in Yield First and Neutral and to every own task on it in
  Performance and Aggressive. It is not a drain and asks for no memory back. In Yield First and
  Neutral tasks above the fair share get no signal: they get their hint, and a drain gives them
  their drain time.
- **FR-076**: An own task MUST count as killed by the out-of-memory killer when it ended by
  SIGKILL in an interval in which the host's `/proc/vmstat` `oom_kill` count rose, or, in Full,
  its leaf's `memory.events` `oom_kill` count rose. The node MUST record whether own memory was
  within its fair share at the last observation before the kill, and which principals were above
  theirs.
  - Within its share: the task MUST be requeued automatically, started again only once free
    memory is back above the floor, and the kill MUST NOT count as a failure or against its
    retries. Its status and report MUST say that an out-of-memory condition it did not cause
    killed it.
  - Above its share: the kill is own fault and a normal failure, with no requeue.
  - In Performance and Aggressive, which give no memory back at a floor breach (FR-023), a kill
    counts as within its share when free memory was below the floor before the kill and own
    memory grew by no more than `noise_tolerance_mib` over that breach episode; otherwise, or with
    that growth unknown, it is own fault.
- **FR-077**: The agent MUST raise its own soft `RLIMIT_NOFILE` to the hard limit at start, and the
  child MUST restore the original soft limit before the user's command runs. A launch whose
  descriptors do not fit under the limit with headroom MUST be refused with a reason. `EMFILE` or
  `ENFILE` MUST NOT end the agent's loop: a failed open while sampling reads as unknown for that
  interval, and a failed open while launching refuses that launch. A journal write that is still
  locked after its wait MUST NOT end the agent's loop: the interval ends there, every step holds,
  and the next interval decides again. While intervals have ended before a sample was acted on for
  longer than one lease (`lifecycle.allocation_lease_ms`), the agent MUST ask for no lease
  renewals of Opportunistic work, and MUST ask again once an interval has acted on one; it keeps
  asking for Guaranteed work, whose renewal reply carries the coordinator's drain.
- **FR-078**: Before exec, the worker gate MUST set a parent-death signal: SIGKILL for
  Opportunistic work and SIGCONT for Guaranteed work, so no own work stays stopped or frozen, and
  no Opportunistic copy runs uncontrolled beside a replacement, after its supervisor dies. The
  supervisor MUST handle SIGTERM, SIGINT and SIGHUP by releasing its runtime steps and draining
  first. Under the cgroup backend, a restriction on a leaf (a freeze, a lower CPU quota or weight,
  a memory cap) MUST be written only after the leaf's members are verified to be exactly the
  run's family, and MUST be refused when they differ. Returning a control the supervisor recorded
  writing to its launch value MUST need only that the leaf is the one it created, in every phase:
  while running, through a drain, after the leader's exit, after an error and when an agent
  reattaches; it MUST NOT write a value tighter than the one in place, and one that fails MUST be
  retried every 50 ms while the supervisor lives. While a recorded freeze holds, the members MUST
  be read every 50 ms in every phase, and a difference MUST thaw the leaf at once; a member list
  that cannot be read on two consecutive checks counts as changed. A freeze the supervisor has no
  record of (a reattaching supervisor whose journal records no Pause) MUST be thawed only after
  the leaf's membership is verified against the family, and refused when it differs. A cap stays
  while its step holds, even when a process the verification would refuse joins the leaf: it was
  written to the verified family, and a process that enters afterwards comes under a limit
  already there. CedeGrid writes nothing for that process, thaws a freeze within 50 ms, and lifts
  every control it wrote when the step changes or ends. A supervisor
  that is killed lifts nothing: its leaf keeps what it wrote. The gate MUST start the
  workload in a session of its own (`setsid`), for the leader and SDK children, so that no exit
  orphans its process group into SIGHUP. The parent-death signal reaches only the leader and SDK
  children: in Full a dead supervisor's other members stay in the leaf, kept for reconciliation;
  in Tracked the agent recovers them (FR-088).
- **FR-079**: A node report MUST carry the agent's monotonic age of its sample when it is sent.
  For a report older than the telemetry freshness limit (`telemetry_ttl_ms`), the coordinator MUST
  grant no renewals and offer no new work for that node; it MUST NOT drain on that report, and
  drains happen only when leases expire. The agent MUST ask for a lease renewal of Opportunistic
  work only while the newest sample it has acted on is no older than its lease
  (`lifecycle.allocation_lease_ms`), and for Guaranteed work always; it
  MUST NOT drain because that sample is older, and an interval that ends at the observation write
  has acted on its sample. The agent MUST hand a drain or a cancel in a renewal reply to the
  supervisor with or without time left on the lease, and the supervisor MUST take it so; a grant
  still needs time left.
- **FR-080**: SIGTERM, SIGINT and restarts of the agent MUST leave supervisors running for
  re-adoption; an explicit operator drain, and the end of `max_runtime_seconds`, drain every
  allocation. The agent MUST reload its configuration on request and apply a mode change without
  a restart. It MUST retry re-adoption
  on later iterations while a supervisor lives, in replay mode too. A stop that drains MUST
  spread its drains over iterations so that no iteration exceeds the 1 s cadence, and leases and
  heartbeats keep their pace: a stopping agent heartbeats at its heartbeat interval, not every
  iteration. The request budget MUST be checked before an attempt is pinned for a failure
  report, and the pin MUST still come before every read that leads to a publication. A pin that
  finds the journal locked for its wait defers that attempt's report to the next interval.
- **FR-081**: Node history MUST be bounded by fixed constants: a heartbeat reports unreleased
  executions plus the last 256 released ones; observations keep the last 24 h, with a row cap;
  an attempt's directory is removed after its outcome receipt and publication garbage
  collection. Per-iteration work MUST load only unreleased and not-yet-reclaimed records.
- **FR-082**: Every interval, the supervisor MUST check for processes outside the run's verified
  family: leaf members in Full, discovered descendants that fail verification or are past the
  bound in Tracked (FR-083). It MUST report them as `single_process_violated` in the status,
  `explain` and the outcome as `outside_family`, with a count and a reason (IDs differ, chain
  unconfirmed, over the bound, platform without tracking), and act on no process outside the
  family. Status and `explain` MUST show the family's size: leader, SDK children, descendants.
- **FR-083**: On Linux, in both backends, a run's family MUST include its verified descendants.
  - The supervisor MUST set itself as child subreaper before it forks the leader.
  - Discovery MUST run every interval and in every stop: each member's
    `/proc/<pid>/task/*/children`, all threads, and the supervisor's own children for orphans;
    without a `children` file, a ppid scan of `/proc`, reported in `doctor`.
  - A descendant joins only in Principle I's order: pidfd first; the parent chain up to a family
    member, or to the supervisor for an orphan; alive through the pidfd; real, effective and saved
    user and group IDs equal to the run's. Its start time is recorded, and it is acted on only
    through its pidfd, or the `/proc` directory path with start-time checks for `oom_score_adj`.
  - Descendants of an SDK child join that child's family.
  - The supervisor MUST reap inherited orphans on the leader's reaping thread, only by specific PID
    or pidfd, only PIDs its own `children` list shows that are neither the leader nor an SDK child,
    and never by a wait for any child.
  - At most 256 members per run are tracked; members past it are reported and never acted on. An
    fd or `/proc` error during discovery MUST hold the step and be reported, never drain own work.
  - The family is reported each interval it changes and right after any stop pass that found a
    new member, without waiting for an answer.
- **FR-084**: On Linux the rootless launch MUST NOT require `single_process`, which stays accepted
  for compatibility. On platforms without a subreaper or `/proc` the single-process contract MUST
  stay required, and the docs say so.
- **FR-085**: A family's usage MUST be measured as own work.
  - CPU: in Full the leaf reading, usable while the leaf holds only family members; in Tracked the
    sum of the members' CPU deltas, a member that exits counting up to its last sample.
  - Memory: the family's summed PSS, never RSS. A family of one, and Full while its leaf reading is
    usable (`memory.current`), keep the existing reading. In Tracked, family PSS MUST be read every
    K intervals: each PSS pass is timed and K chosen so that PSS reading averages at most 0.5% of
    the scope's CPU, K from 1 to 10, the agent's total within SC-014's budget. Between reads own
    memory is the last PSS plus max(0, the members' RSS sum now minus their RSS sum at that read),
    so growth is never missed and a copy-on-write break is counted at most K intervals late.
    Before a floor-breach Hint or Drain, or any other step on own work because of own memory, PSS
    MUST be read fresh, and the step is taken only if that reading confirms it; a step taken that
    way and held unchanged needs no new read.
  - GPU: verified descendants' per-process GPU memory and compute count as own, including in
    FR-021's interval shares.
  - Verified descendants are in the managed set, so they are not the owner's unmanaged use and the
    fair share does not subtract them.
- **FR-086**: In Tracked, a step MUST reach the whole family.
  - A stop (a Pause) MUST discover, stop every member
    through its pidfd, and repeat until a pass finds no new member, at most 3 passes. A member that
    appears while the family is stopped is stopped at once; the family is stopped and continued
    together.
  - Continue MUST send SIGCONT to every member whatever the state flags say, continue past a
    failing member, and report it. A member that has exited is dropped, not an error.
  - Each member's `oom_score_adj` baseline is recorded when it joins (its parent member's
    baseline if it started while the run was marked) and restored; the preferred-victim setting
    applies to new members as to the leader (FR-023).
  - Full keeps `cpu.max` and `cgroup.freeze` unchanged.
- **FR-087**: A run MUST end when its leader exits. Members still alive then get SIGTERM, then
  SIGKILL after `term_grace_ms`, and the outcome counts them, in both classes and with no opt-out.
  A drain MUST signal the leader first and SIGKILL members still alive after the grace. The README
  MUST say that a job script which starts work in the background ends it with `wait`, and that a
  process meant to outlive its run is started as its own run.
- **FR-088**: When a Tracked run's supervisor is gone, the agent MUST recover the run's members
  from the last family it recorded in its state directory (leader, SDK children and verified
  descendants with their start times, and the run's boot ID and user and group IDs), when its
  reconciliation finds the supervisor gone and when it starts.
  - For each recorded process: a pidfd first; then its start time and real, effective and saved
    user and group IDs, read through the matching `/proc` directory, equal to the record; then
    alive through the pidfd; then a signal through the pidfd only, never by PID alone. A process
    that fails a check is never signalled, and is counted.
  - Guaranteed members get SIGCONT and are left running; Opportunistic members get SIGKILL. One
    member's failed signal does not stop the others.
  - The counts (continued, ended, identity mismatch, already gone) go to the run's outcome and an
    event. A member found and stopped in the instant its supervisor dies can be missed, and stays
    stopped until an agent runs. Full keeps its leaf and the thaw on reattach.
- **FR-089**: Yield First and Neutral MUST hold running own work back by whole tasks on CPU and
  GPU compute (report 057 C2, C3 and D2). No rate cap applies to either at either level: in Full,
  `cpu.max` is a launch setting only, and a pause freezes the leaf.
  - On CPU, the tasks paused are those whose usage together comes nearest what own work holds
    over its target, so that neither own work nor the other principals lose more than half the
    smallest task's usage: in FR-016's order and its mode tiers, each task that fits what is left,
    then, if a part is left, the smallest other task whose pause leaves own work less below its
    target than running leaves it above. A pause already held counts first, and a task whose step
    changed within the step dwell waits. On GPU compute a task is paused only while the own work
    left running stays at or above its target: the own contexts left running on the time-slice
    path (FR-021), else own work's measured amount less the task's equal part of it. Yield First
    on a contended resource pauses every own task, down to the first-come floor on GPU compute
    (FR-090).
  - A pause ends as FR-014 says: at the bound it moves to other tasks, chosen the same way to
    cover what the paused one used as far as own work with it running would be over its target, so
    the held amount stays while the excess does; a task that takes its place holds for the cause
    and the demand the moved pause answered. Where too few running tasks are left to cover it, the
    moved pauses take turns, one resuming at a time. A paused task resumes when the demand it
    answered falls back past the reversal threshold, at the mode's pace, or at once when
    contention has ended (FR-034), and a returning contender is answered without the step dwell.
    A pause held for own usage over its target holds while resuming it would leave own work further
    over its target than the pause leaves it below, and at least while own work is not below its
    target by the contention floor (FR-013). A pause at its bound moves even while own usage over
    its target waits its dwell. While own tasks are paused on CPU, the room below the target is
    theirs first: admission counts what they used, and a moved pause is not a step down (FR-028).
    What own work holds beyond the operator envelope pauses a task whatever else the resource waits
    on (FR-057). A task that is draining makes no other task hold.
  - No duty cycle runs: the supervisor neither stops nor continues own work by period, and a
    worker's CPU hint is a request only.
  - On a time-sliced GPU, in every mode, one more own process is admitted only while
    (m + 1) × k ≤ E (FR-021's counts) or no counted principal is slice-bound; otherwise admission
    is held with `gpu_time_slice_admission_held`. For this a principal reads the larger of the
    estimate's average and its most in the uncapped intervals within
    `gpu.process_sample_max_age_ms`, so one that has just become busy is not read by its quiet
    past; beside one saturating process one own process is the fair share, and admission is held
    there whether or not the amounts can be read. An own task placed on the device but not yet
    listed there counts toward m as one context, and an open GPU compute admission is room for
    one more own process: the coordinator places at most one new GPU task on each device per
    heartbeat, and the next report counts it (FR-028). Where the node reports plain admission on
    the device (`plain_admission`: Performance or Aggressive with no account whose CedeGrid work
    came first there, [006](../006-performance-first/spec.md) FR-007), the rule does not apply,
    and one heartbeat places every task the device's memory budget holds.
  - `observe`, `status` and the decision say, per resource, that whole-task holds apply and which
    tasks are held.
- **FR-090**: On GPU compute, in Yield First and Neutral, own work that arrived on a device before
  every principal now listed there MUST keep at least the share that leaves no single later
  principal more than half of the device (constitution 8.1.0 "First-come GPU floor").
  - On the time-slice path (FR-021) that is at least 2 × E_max − E own contexts running, E_max
    being the most contexts of one principal and E all external contexts counted; elsewhere,
    beside exactly one contending principal, half the device (500 thousandths), and beside more
    than one, no floor.
  - Arrival is kept per UID and, for own work, per device: the interval in which it was first
    listed there, kept while it is listed again within 30 s. One absent for longer arrives again
    when next listed. An agent restart starts the record again, which only withholds the floor.
  - A system account's context (FR-059), such as a login screen's, is no principal's: it neither
    arrives first nor counts in E_max or E.
  - A listed external context whose UID cannot be read, an unknown or incomplete listing, or an
    arrival in the same interval as own work's means no floor on that device.
  - The floor never pauses own work or starts it; it only limits what Yield First and Neutral
    hold back. The decision states per device whether the floor applies and, if not, why.
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
  waits for its deadline in every mode. A memory hint is complied with when, at its deadline, the
  task's usage is at or below the larger of the target sent when the hint began and the current
  target; a target the task cannot reach is not complied with. A task that complied is not
  drained or terminated for that hint: while own memory is still above its share and the
  over-share order still selects it, it gets a new hint with the current target and a new
  deadline, and otherwise its hint is lifted. While an over-share task's hint or drain wait runs,
  its excess is not passed to in-share tasks.
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
    then KILL after the grace period, acting on own processes only. The supervisor sends TERM in
    the pass that reads the step and records the step and the drain after it; KILL follows the
    grace counted from the TERM that was sent.

  In Performance and Aggressive the critical path takes no step on own memory; every own task
  gets the save-progress signal (FR-075). The over-share order (FR-016) holds on the critical path. Out-of-memory order by fault
  (FR-023) stays the final backstop.

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
  the RAM breach window (FR-024). Out-of-memory order by fault (FR-023) applies to every own
  process.
- **FR-042**: Neutral memory policy B is removed. `contention.neutral_memory_policy = "b"` MUST
  fail to load with a message that names the replacement: the floor sizes (FR-023).
- **FR-043**: Caution options:
  - MUST be off by default;
  - MUST NOT be enabled by upgrades, migrations or defaults;
  - MUST be recorded in configuration;
  - MUST appear in `doctor`, `status` and node reports while active, with their marker.
- **FR-044**: No option MAY permit acting on an external process, raising own priority, or
  putting another workload at risk of an out-of-memory kill, swap thrash or a failed GPU
  allocation. FR-023's floor rule in Performance and Aggressive is a rule of those
  modes, not an option.
- **FR-045**: The risk markers are justified as follows.

  | Option | Marker | Justification |
  |---|---|---|
  | Yield First, Neutral, default floors | none | Floors hold in every mode. Own work is released before others are affected. |
  | Performance mode (the default) | none | Own work keeps all idle capacity minus a small admission margin and grows quickly. Running own work gives others nothing beyond what the kernel and the GPU driver give them beside it; new own work does not start where another principal is short of its fair share; the floors limit admission, and at a floor breach the kernel decides, as without CedeGrid (FR-022, FR-023). |
  | A floor near its minimum | *caution* (yellow) | Less headroom is kept for other workloads' bursts. The enforced minimum and out-of-memory order by fault still protect them. |
  | A margin below its default (`reduced_margin`) | *caution* (yellow) | Own work leaves less idle capacity for external demand to grow into. Contention handling and floors still hold. |
  | A detection setting beyond its default (`relaxed_detection`): `saturation_percent`, `principal_min_percent`, `noise_tolerance_mib`, `uid_min` or `attribution_tolerance_percent` above it, `uid_max`, `memory_peak_window_s` or `quiet_window_intervals` below it | *caution* (yellow) | Contention and external growth are recognized later or forgotten sooner, so own work holds capacity longer. Floors still hold. |

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
- **FR-048**: The interface MUST be usable with the keyboard alone: arrow keys move and Enter
  chooses; where several options may be chosen at once, Space toggles each. It MUST NOT require a
  mouse, and MUST NOT mix typed numbers with arrow keys (user, 2026-11-17).
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
  - in a hand-edited file, an `acknowledge_caution` entry that names the option (`small_floor`,
    `reduced_margin` or `relaxed_detection`). The entry `performance_mode` still loads, though
    Performance needs no confirmation.
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
  cannot drive the terminal, setup MUST stop, write nothing and name the flags for non-interactive
  use; it offers no second, typed interface (user, 2026-11-17). A list taller than the terminal
  scrolls.

**Documentation**

- **FR-056**: `docs/guarantees.md` MUST describe the no-harm floors and their minimums, floor
  policy A, the caution options with their markers, the removed options, the envelope and spool
  behavior, and the interpretation of `gpu.execution_mode`, in the same change that implements
  them.

**Operator limits**

- **FR-057**: When own work exceeds the operator's configured resource envelope, CedeGrid MUST
  restore the envelope per resource through the yield ladder. On CPU it steps only work in Yield
  First and Neutral, and nothing where the node's CPU mode is Performance or Aggressive: own CPU
  above the envelope is left alone there, as plain Linux would run it, and a decision whose CPU
  work contends with a plain principal with no first-comer holding it sets `cpu_beside_plain`
  ([006](../006-performance-first/spec.md) FR-007a): after three intervals in a row of that
  contention, held 10 s after it ends. A named plain account contends on a CPU full in this
  interval; unattributed CPU stands for a plain principal only on the saturation Performance
  judges, after the principal window, and never within two monitor intervals after an own task
  stops. There, own charges over the CPU budget restart no cooldown and release no admitted
  reservation: new work waits through the headroom.
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
  CedeGrid's own CPU (the agent and supervisors) MUST be shown as in use by CedeGrid, attributed
  to the owner, never as unattributed external usage.
- **FR-070**: `noise_tolerance_mib` MUST be at most 1024 MiB at load and at most half of each
  device's effective floor when the agent starts; otherwise the agent refuses to start.
  Settings that lengthen reclaim or let stale telemetry pass are bounded: `monitor.interval_ms`
  and `kernel.monitor_interval_ms` at most 10 s, `kernel.freshness_intervals` at most 20,
  `gpu.process_sample_max_age_ms` at most 60 s, `lifecycle.drain_timeout_ms` and
  `lifecycle.term_grace_ms` at most 60 s. `gpu.process_sample_max_age_ms` MUST be above
  `monitor.interval_ms`: when it is not set it is max(2000, 2 × `monitor.interval_ms`), which
  `doctor` and `validate` show as derived; an explicit value at or below the interval MUST be
  refused on every host, with an error naming both keys and the smallest value that loads.
- **FR-071**: The resource-target file MUST carry an optional `allowance` per resource (CPU,
  RAM, each GPU's memory): what the allocation may grow to now without causing a step, with its
  own `allowance_sequence`. If every running allocation grows to its allowance at once, the
  totals MUST stay within the mode's allowance, apart from what running work already holds;
  falls MUST publish once their summed drift passes one publish threshold, the only reserve, and
  a rise at most one scale-up step per scale-up interval. Both
  SDKs MUST offer `on_resource_allowance`, which an allowance-only change calls instead of the
  target callback. Workers that ignore it keep working.
- **FR-072**: CPU saturation MUST also count external CPU pressure (PSI).
  - The agent MUST read `cpu.pressure` read-only from the cgroups outside own work's user slice,
    or outside the agent's own cgroup when it runs in no user slice: the siblings of each cgroup
    on the path from the root down to it. It MUST use the highest `some` avg10 among them.
  - Own cgroups' pressure MUST be shown in `observe` and `doctor` and MUST NOT be saturation
    evidence.
  - A cgroup that holds own work together with other users' processes counts as own: its pressure
    MUST NOT be counted.
  - Under PSI-only saturation (busy below `saturation_percent`), every contending principal MUST
    be probed, not only those below one margin step (FR-021). A principal that does not grow
    into the freed capacity within the probe window is recorded as satisfied at its usage plus
    the contention floor, and contention ends; a principal that grows gets its share.
  - When only system-wide PSI exists, or PSI is missing, unreadable or malformed, it MUST add no
    evidence and hold nothing back, reported as `psi_system_only` or `psi_unknown`.
  - PSI MUST feed CPU saturation only, never RAM, GPU or floor breaches, give the same evidence
    in every mode, and write nothing anywhere.
  - `observe` and `doctor` MUST show external and own PSI and which signal made the CPU
    saturated (`saturated_by_psi`), and `held_back` MUST show capacity held for PSI-only
    contention as `contention_psi`.

### Key Entities

- **Contention mode**: Yield First, Neutral or Performance. Configured per resource as a node
  default, a job override and a node cap. The effective mode is derived per allocation.
- **Resource view**: per node and resource, the capacity, own usage, external demand by
  principal, unattributed usage, floor, margin, fair share and admission state.
- **Principal**: a user ID from `uid_min` to `uid_max` with sustained demand on a resource, or
  the CedeGrid account.
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
- **SC-002**: In Yield First and Neutral, CPU demanded by another principal's external work is
  returned within 2 monitor intervals of the demand appearing, including on a host that own work
  saturates: own CPU usage reaches the mode's contended target, which in Yield First is
  opportunistic work paused, then as FR-014 says after `pause_bound_s`, and in Neutral the fair
  share to within half the smallest task's usage (FR-089). Where own work saturated the resource before its
  first step, Yield First's first step holds it to the margin within the fair share, so at least
  the fair share is back within the same bound; a demand smaller than one margin step is probed
  instead (FR-021). Where whole tasks left own work just over Yield First's margin, the user's
  first interval can fit the room left and is probed, and every task is paused one interval
  later. In Performance own CPU is not
  stepped for another principal's demand (SC-019), and no new own work is admitted while it
  would take that principal below the lesser of its demand and its fair share. With demand that
  fits in the margin, Yield First keeps its 15% margin free, to within half a task. No work is drained or terminated
  when CPU is the only contended resource, except as SC-007 allows. A principal whose demand is on record as satisfied
  gets that demand back within the same bound when it returns. These bounds are for other
  principals' demand: unmanaged processes of the account that runs CedeGrid share that account's
  fair share with own work, so they are served as own work's margin frees capacity.
- **SC-003**: In Neutral, with one contending principal, own share converges to the fair share,
  to within half the smallest task's usage (FR-089), within the Neutral convergence time. The fair share is
  50% here; the defaults are a tolerance of ±15 percentage points and a convergence time of 60 s.
- **SC-004**: In Performance:
  - On an idle host, own work reaches the Performance ramp target within the ramp time, on CPU
    and on GPU compute. The defaults are 90% of available capacity minus the margin, within
    10 s.
  - When an external workload starts, no new own work is admitted on that resource while it
    would take the workload below the lesser of its demand and its fair share, and the workload
    gets no less from running own work than beside the same own work without CedeGrid (SC-019).
- **SC-005**: In every mode and with every caution option active:
  - Zero signals, priority changes or control-group changes are applied to external processes,
    as verified by an audit of every actuation in the test and pressure suites.
  - Zero external out-of-memory kills or external GPU allocation failures attributable to
    CedeGrid in the pressure suite. A kill in Performance or Aggressive at a floor breach
    own growth did not cause is not attributable to CedeGrid, nor is a kill the baseline run
    shows too (FR-023).
- **SC-006**: A GPU job runs to completion on a device already occupied by an external context,
  under Neutral and under Performance, with native evidence on at least one supported NVIDIA
  device.
- **SC-007**: When CPU is the only contended resource, zero drains or terminations occur in any
  mode, except for the operator envelope (FR-014).
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
  released it. Memory own work takes after a hint was sent is new demand, measured from the
  interval that first shows it (FR-037):
  - CPU: 2 monitor intervals in Yield First and Neutral; Performance takes no CPU step for others (FR-022).
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
- **SC-014**: On an idle host, own CPU utilization, own tasks plus CedeGrid's own processes, is at
  least 97% in Performance, 94% in Neutral and 84% in Yield First, and own RAM and GPU memory are
  within 1% of what CedeGrid targets: capacity minus the floor, minus others' use and their peak,
  and minus Yield First's margin too. On a device, others' use includes memory the device reports
  used that no process accounts for. Both CPU numbers are
  stated; with 117 tasks on 14 CPUs, CedeGrid's own processes use at most 1% of capacity.
- **SC-015**: Beside a steady external user of 1–2%, own work's 5-minute average is at least 90%
  of capacity minus that user and the margin, with no cap or pause cycling.
- **SC-016**: `doctor` and `status` show, per resource, how much is held back and why: floor,
  reserve, margin, an ancestor CPU ceiling (never as contention), contention, contention by PSI
  alone, peak, unknown, cooldown, backoff and charge shortfall.
- **SC-017**: Beside one other GPU user, measured in the same run with "alone" the same probe or
  user with nothing else on the device: own work with CedeGrid ÷ own work alone is at least
  min(the mode's target ÷ 1000, own work without CedeGrid ÷ alone) − 0.03, the target being the
  mode's for the principals actually on the device (beside one saturating user, half less the
  mode's margin: 480 in Performance, 450 in Neutral, 500 in Aggressive), not the recorded one, and
  the other user's
  rate with CedeGrid is at least min(its rate without CedeGrid, its fair share × its rate alone) −
  3%. The ratio with CedeGrid ÷ without CedeGrid beside the same user is reported beside them. In Performance and Aggressive SC-018 and SC-019 replace this
  bound.
- **SC-018**: In Performance, against the baseline (the same own work started without CedeGrid
  in the same place): in each gate shape, in Tracked and in Full, at least 3 repeats with
  CedeGrid and 3 without, own work's throughput with CedeGrid is at least its throughput without
  less the spread, the spread being the larger of the two sets' max − min. This is the floor;
  the gain over the baseline that Performance must show is measured by 006's gain gates
  (SC-004, SC-005). At a floor breach no
  own task is hinted, drained or sent TERM.
- **SC-019**: In the same runs, the other user's rate with CedeGrid is at least its rate without
  less the spread, and its p99 at most its p99 without plus the spread. A gain beyond the spread
  is reported, since it means own work gave something back. At a host-wide OOM, the kernel's
  victim is the one the baseline shows, or own work.
- **SC-020**: Whole-task holds (FR-089), at least 3 runs each way on the test host. On CPU (run
  E8, F2's shape in Neutral, Tracked and Full): own usage is within 2% of its target or above it
  in at least 95% of intervals, own throughput is at least the former stop-and-resume cap's
  less its spread, and the other user's rate is at its fair-share rate, reported against plain
  Linux and against the former cap; one run with a single task larger than the excess records
  the known gap (a task that cannot be paused without going below the target is left alone). On GPU
  compute (run E9, two own probes beside one saturating user in Neutral): own work ÷ alone is at
  least 0.42 and the other user's rate at least 753.4 req/s, SC-017's bounds for that shape. A
  miss in E9 is reported before another GPU target is chosen. Beside a later user in Yield First,
  own work keeps at least half the device (FR-090).

### Provisional thresholds

All values are **provisional, to validate on real hardware**. Each is a configuration value; the
times assume the default 500 ms monitor interval.

| Threshold | Default | Rationale |
|---|---|---|
| GPU memory noise tolerance | 256 MiB per device | Larger than typical driver and context jitter, and small next to common device sizes. It was the maintainer's proposal. |
| Yield First margin | 15% of compute, 20% of memory beyond the floor | Generous by design, yet compute is reclaimed within an interval, so it needs less than memory. |
| Yield First response time | 3 monitor intervals (1.5 s) for escalating past a step that did not free the excess | One interval to observe, one to confirm, one to apply and measure. The first step applies at once, so contended CPU returns within 2 intervals (SC-002). |
| CPU reclaim | 2 monitor intervals in Yield First and Neutral; in Performance at least the margin per interval down to the fair share | One interval to observe the demand (a per-interval average), one with the step applied. On a saturated host the contended target applies at once instead of shrinking one margin per interval, except for Performance, which keeps what external demand does not take, and a probed small demand. |
| Yield First scale-up | 1 step per 30 s | Matches today's 30 s scale-up cooldown, which is the most conservative existing default. One move once contention has fully ended (FR-034). |
| Neutral scale-up | 1 step per 10 s | Reaches the fair share well inside the 60 s convergence time while staying damped. One move once contention has fully ended (FR-034). |
| Performance scale-up | CPU and GPU compute: 1 step per monitor interval (500 ms); RAM and GPU memory: 1 step per 2 s; a held step still waits the step dwell (3 intervals) | Compute is reclaimed within an interval, so it can rise as fast as it is observed; memory keeps its damping because it is slow to reclaim. |
| Neutral convergence time and tolerance | 60 s, ±15 percentage points | Share changes go through damped steps; memory relies on cooperative hints. Twice the originally proposed 30 s and ±10 leaves room for both until measured. |
| Neutral margin | 5% of capacity (CPU, GPU compute), plus the floors (memory) | Between the other two margins, so the modes stay ordered; it leaves external demand room to show growth without idling compute that is reclaimed within an interval. |
| Performance margin | 2% of capacity (CPU, GPU compute), plus the floors (memory) | The smallest room that still shows external growth; under contention Performance keeps what external demand does not take. |
| CPU idle reserve | max(`cpu.reserve_physical_cores`, the mode's margin); reserve default 0 | A physical-core reserve and a margin both idle compute; one is enough. |
| Probe hysteresis | The contention floor (1% of capacity) | A satisfied principal gets that much room to show growth before it contends again. |
| CPU admission usage window | p95 over 60 s | CPU is compressible: caps and the kernel share it if everyone spikes, so admission follows recent usage rather than requests. |
| Performance ramp | 90% of available minus margin within 10 s | Compute scales up every interval and one heartbeat admits as much as fits, up to 32 new assignments; reclaim, margins, floors and the anti-flap rules are unchanged. |
| Reclaim bound: RAM, GPU memory | Hint deadline + drain timeout + termination grace + 1 interval (10.5 s) | The sum of the existing deadlines; nothing else waits. |
| Hint deadline | 5 s | Long enough for a cooperative worker to reach a safe point; short enough to bound memory reclaim. |
| Guaranteed hint deadline | 5 s, 1–30 s | The same safe-point argument as the hint deadline. Raising it only delays Guaranteed release above the critical level. |
| RAM breach window before draining Guaranteed work | 10 s, 0–60 s | Rides out short RAM spikes, which page cache and allocator churn cause, without losing Guaranteed progress. GPU memory has no reclaimable cache, so its breaches act at once. |
| Critical floor share | 50% of the floor | Half the floor is still free headroom; below it, other workloads are close to allocation failures. |
| Critical hint deadline | 1 s | Two monitor intervals: enough for a polling worker to see the target, short enough that the critical path stays within 6.5 s. |
| Floor minimums | 512 MiB host RAM, 512 MiB per GPU device | Below these, ordinary processes and driver contexts fail to start. The GPU minimum must be at least what a fresh CUDA context takes on the target GPUs, which T088 measures. |
| Principal leave | 60 s below `principal_min_percent` | Entry stays fast; a principal that pauses briefly keeps its share instead of rejoining the unattributed principal. |
| `uid_min` | 1000 | The usual first regular user ID (`UID_MIN` in `login.defs`). |
| `uid_max` | 60000 | The usual last regular user ID (`UID_MAX` in `login.defs`). Above it are a display manager's dynamic login-screen accounts, systemd's dynamic users and `nobody`. |
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
  `uid_min` or above `uid_max` is background, not a principal.
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
