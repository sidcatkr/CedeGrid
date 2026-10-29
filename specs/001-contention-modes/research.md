# Research: Contention Modes with Graduated Yielding

Phase 0 output. Each item records a decision, its rationale and the alternatives considered.

- Items marked **verify natively** rest on kernel or driver documentation. They need a
  recorded run on the target Linux kernel or NVIDIA driver before any claim (constitution VI).
- Line references are to `dev` at `55a068b`; the code is unchanged from `6be4cea`.

## R1. One policy authority per node

- **Decision.** The agent's `PolicyEngine` is the only component that decides admission and
  yield steps.
  - Supervisors no longer construct a collector or a `PolicyEngine`. Today they do both
    (`src/agent.rs:402-435, 549-550`).
  - Supervisors receive explicit per-allocation steps from the agent, and enforce only:
    - deadlines (lease, drain, TERM);
    - actuation of the step they were given, on their own child only;
    - a hold-current-step fallback while the agent is unreachable.
- **Rationale.** Drain is irreversible. With N+1 independent engines (different sample
  windows, frozen configs, fresh histories), the node acts on the union of their
  over-estimates. One authority also removes N NVML scans per interval.
- **Alternatives considered.**
  - Seed each supervisor engine from the agent's state. Rejected: it still allows divergent
    decisions after seeding, and still costs N scans.
  - A shared-memory policy state. Rejected: it adds complexity and a new failure mode.

## R2. Re-attachable supervisor control channel

- **Decision.** Each supervisor additionally listens on a private Unix socket,
  `control.sock`, in its owner-only output directory.
  - The socket is authenticated by a per-supervisor random token stored in the spec file
    (mode 0600), plus a same-UID peer-credential check.
  - It serves the same control messages as the stdin pipe.
  - After an agent restart, the new agent finds live supervisors through
    `supervisor-identity.json`, reconnects, and re-establishes steps.
  - The agent no longer rewrites the journal of a verifiably live supervisor to
    NeedsReconciliation. That rewrite is why a later drain falls through to SIGKILL today
    (`src/agent.rs:865-932`, `src/execution_state.rs:614-628`).
- **Rationale.** FR-015 and the authority-restart edge case require steps to reach supervisors
  that outlive the agent. A stdin pipe cannot be reopened.
- **Alternatives considered.**
  - Abstract-namespace sockets. Rejected: Linux-only, and visible to every process in the net
    namespace.
  - A file-based mailbox. Rejected: polling latency and harder atomicity.

## R3. Error handling after EXEC

- **Decision.** Post-EXEC errors are classified, and none of them leads straight to
  `kill_and_confirm` (`src/supervision.rs:1088, 1167-1186`):

  | Class | Handling |
  |---|---|
  | Observation | Moves out of the supervisor with R1 |
  | Control channel | Authority lost: hold the current step until the existing lease rules apply |
  | Actuation | Mark the step unavailable for this allocation and report it |
  | Drain-file or target-file write | Retry, then escalate through the ladder |
  | Journal | Retry with bounded backoff. Opportunistic work then goes through the drain chain (drain → TERM → KILL with its deadlines); Guaranteed work stays supervised and is reported as needing reconciliation |

  SIGKILL remains only as the last ladder step after the TERM grace, or when the operator stops
  the node.
- **Rationale.** FR-017 says observation or control errors never terminate Guaranteed work. A
  transient error must not kill any work without the drain deadline.
- **Alternatives considered.** Keep fail-fast but exempt Guaranteed work. Rejected: it still
  SIGKILLs opportunistic work without drain on transient errors.

## R4. Actuation mechanisms (owned processes only)

Each mechanism applies only to verified own pidfds or to a delegated cgroup leaf whose
membership equals the verified PID set.

| Step | Delegated cgroup available | Rootless fallback |
|---|---|---|
| Lower share (CPU) | `cpu.weight` written at runtime | Unavailable at runtime: launch-time nice only (see R5) |
| Cap (CPU) | `cpu.max` written at runtime | SIGSTOP/SIGCONT duty cycle on owned pidfds, period = monitor interval |
| Cap (RAM) | `memory.high` on the own leaf, read back; never below unevictable memory plus anonymous memory without swap plus the noise tolerance, nor below a Guaranteed request; raised to current usage when `memory.events` `high` rises for a calm window while `memory.current` does not fall; `memory.max`, `memory.low` and `memory.min` are never written (T138) | Unavailable: go to hint |
| Pause (CPU, GPU compute) | `cgroup.freeze` with `cgroup.events` readback | SIGSTOP on owned pidfds, bounded by the pause limit |
| Hint (RAM, GPU memory, CPU) | Resource-target file (R6) | Same |
| Drain / terminate | Existing drain chain | Same |
| GPU compute share or cap | No rootless mechanism. Treated as unavailable; the ladder goes from pause to hint to drain | Same |

- **Decision.** Use the mechanisms in the table.
- **Rationale.**
  - cgroup v2 controls are reversible and work-conserving (`cpu.weight`), and readable back
    ([kernel cgroup-v2 docs](https://docs.kernel.org/admin-guide/cgroup-v2.html)).
  - SIGSTOP/SIGCONT through an owned pidfd needs no privilege and cannot reach other processes.
  - **Verify natively:**
    - whether `cgroup.freeze` and runtime writes are permitted in the delegated subtree;
    - SIGSTOP's effect on in-flight GPU kernels. Submissions stop, kernels in flight may
      finish, and VRAM stays allocated.
- **Alternatives considered.**
  - MPS or MIG for GPU shares. Rejected: privileged configuration, out of scope.
  - `ioprio`. Rejected: disk I/O is out of scope.

## R5. Priority changes are launch-time only

- **Decision.** Keep `nice` (default 10) and add optional launch-time `SCHED_IDLE`, used for
  Yield First CPU when no cgroup is delegated. Neither changes at runtime.
  - `cpu.nice < 0` in existing configs is accepted with a warning and treated as 0. Principle
    II forbids raising own priority, and `supervise()` already refused negative values at
    launch (`src/config.rs:666-669` vs `src/supervision.rs:858-861`).
- **Rationale.**
  - Per [sched(7)](https://man7.org/linux/man-pages/man7/sched.7.html) and
    [setpriority(2)](https://man7.org/linux/man-pages/man2/setpriority.2.html), an
    unprivileged thread can lower its priority. Raising it back is bounded by `RLIMIT_NICE`
    (usually 0).
  - On Linux, nice and the scheduling policy are per thread, so a runtime change would miss
    existing worker threads.
  - **Verify natively:** the `RLIMIT_NICE` defaults on the target hosts.
- **Alternatives considered.** A runtime renice ladder. Rejected: one-way, and per-thread.

## R6. Resource-target channel for workers

- **Decision.** Add a JSON target file next to the drain file, in the same private
  per-execution directory (`src/supervision.rs:660-718`).
  - **Delivery.**
    - The path is passed as `CEDEGRID_RESOURCE_TARGET_FILE`.
    - It is passed to managed children through `GateRequest`, as the drain file is.
    - The supervisor writes it by atomic rename.
  - **Content:** a sequence number, a deadline, and per-resource targets.
  - **Acknowledgement.** SDK watchers write the highest sequence they have seen to
    `resource-target.ack`. Without an ack, the hint step is recorded as unavailable for that
    allocation.
  - **Compliance** is judged by observed usage, never by the ack.
  - **Unchanged:**
    - The drain file stays existence-only.
    - The v2 context gains only optional keys.
    - Both SDKs keep accepting `schema_version == 2`.
- **Rationale.**
  - Deployed 0.2 workers treat any drain-file existence as a full drain. The socket cannot push
    messages (`src/child_supervision.rs:114-363`).
  - Managed children lose the socket and context, and the SDKs require version 2 exactly.
- **Alternatives considered.**
  - A socket push message. Rejected: no server push exists, and children have no socket.
  - Reusing the drain file. Rejected: every 0.2 worker would drain.

## R7. Mixed-version wire compatibility

- **Decision.** Use feature negotiation. Nothing new is sent to a peer that has not advertised
  support.
  - **Coordinator → agent.** `HeartbeatReply.features: Vec<String>` is ignored by old agents,
    because the type has no `deny_unknown_fields`.
  - **Agent → coordinator.** The agent sends optional `NodeReport.features` and
    `NodeReport.contention` only after the coordinator has advertised `contention/1`. Old
    coordinators reject unknown `NodeReport` fields (`src/protocol.rs:121-139`).
  - **Per-task overrides.**
    - They live in `LaunchRequest.contention`, which is optional and
      `skip_serializing_if = "Option::is_none"`.
    - The coordinator places a task that has overrides only on agents that advertised
      `contention/1`.
    - For other agents it serializes the request without the field. `LaunchRequest` rejects
      unknown fields in 0.2 agents (`src/execution_model.rs:22-24`).
  - `AllocationReport.yield_cause` is optional, and is sent only after negotiation.
  - `protocol::API_VERSION` becomes the `api/1` feature entry; it is currently unused.
- **Rationale.** FR-032: no peer rejects a message because of an unknown field, and features
  stay off unless both sides support them.
- **Alternatives considered.**
  - Bump the API version and refuse old peers. Rejected: it violates FR-032.
  - Drop `deny_unknown_fields` everywhere. Rejected: it hides real protocol errors.

## R8. GPU observation fixes

- **Decision:**
  - Carry NVML `reserved` and `free` (from `memory_info()`, which already calls
    `nvmlDeviceGetMemoryInfo_v2`). Unavailable memory = `total − free`.
  - Read device memory before and after process enumeration. A disagreement larger than the
    tolerance marks that tick's attribution unknown.
  - Report an `unattributed` amount separately from external memory.
  - Add a `Quiet` activity class. It requires `quiet_window_intervals` of healthy observation:
    successful or NOT_FOUND utilization queries with an advancing cursor, over a complete
    compute and graphics inventory, with no sample above 0 for external PIDs.
  - Separate "observation healthy" from "fresh activity evidence", so an idle device no longer
    becomes InsufficientObservability (`src/telemetry.rs:592-598`).
- **Rationale.** These fix D3, the reserved-memory headroom error, and the
  freshness-flip-to-full-yield problem.
  - **Verify natively:** that NOT_FOUND is returned (rather than zero-valued samples) for idle
    external contexts on the target drivers.
- **Alternatives considered.** Treat any external PID as Active. Rejected: it blocks sharing
  forever, the same as today.

## R9. CPU and RAM measurement error

- **Decision.** Report lower and upper bounds for managed CPU and host busy, and treat
  inconsistency inside the known error as consistent.
  - Managed CPU includes the per-allocation quantization (`src/telemetry.rs:1108-1129`).
  - Host busy includes USER_HZ rounding and iowait (`src/kernel.rs:287-311`).
  - Inside the error: clamp external to ≥ 0 and record a reason.
  - Outside it: unknown measurement for that resource, which holds (FR-008).
  - RAM uses the same pattern: anonymous PSS lower bound and RSS upper bound.
- **Rationale.** This fixes D5. It replaces "managed > busy → budget = 0"
  (`src/policy.rs:337-343, 360-378`).
- **Alternatives considered.** A fixed percentage tolerance. Rejected: the error depends on
  allocation count and interval.
- **Update (item 023, 2026-10-27): own CPU sources.** The `/proc/<pid>/stat` bound (2 ticks +
  2 ms per process per 500 ms interval) dropped up to about 44 m per busy process, and the
  policy charged the gap as external use (report 021: about 430 m with 7 workers). Sources now,
  in order: the leaf's `cpu.stat` `usage_usec` when leaf membership is verified at both reads;
  the process CPU clock under a pidfd; `/proc/<pid>/stat` as before.
  - Kernel facts (torvalds/linux master): `pid_for_clock` in `kernel/time/posix-cpu-timers.c`
    checks only that the PID is a thread-group leader, with no ptrace or UID check, so the agent
    may read an own process's clock. `thread_group_cputime` (`kernel/sched/cputime.c`) adds
    `sig->sum_sched_runtime`, so exited threads count, and reads other running threads'
    `sum_exec_runtime` without folding in their pending time. A per-thread clock of another
    process is refused. `cpu.stat` `usage_usec` is the same runtime, accounted in `update_se`,
    in microseconds, and lags the same way. `/proc/<pid>/schedstat` is the leader thread only.
  - Bound: resolution plus one scheduler tick per thread that can be running (the smaller of
    the threads read and the scope's CPUs), the tick being `1/CONFIG_HZ` from
    `/boot/config-<release>`, else 10 ms. `nohz_full` CPUs fold pending time in only once a
    second, so such hosts keep the `/proc` source.
  - Not `/proc/<pid>/schedstat` alone (leader thread only), and not a sum over
    `/proc/<pid>/task/*/schedstat` (misses exited threads, grows with the thread count).
- **Update (item 024, 2026-10-27): the summed bound.** Subtracting a tick per running thread for
  every process counts pending time once per process, but only a task running at the read has
  any, so across own work it is at most one tick per scope CPU at each read side, plus the read
  pass's length because the reads are sequential. Own CPU summed for a decision uses that bound
  (`Snapshot.managed_cpu_lower_millicores`), clamped between the per-allocation sums; each
  allocation keeps its own. 38 tasks of 100 m on 4 CPUs at a 1 ms tick: 114 m low per process
  (each bound also truncates to a whole millicore), 9 m low summed.

## R10. Missing observation of one allocation

- **Decision.** Charge that allocation at max(request, last known observation) and mark it
  unknown.
  - Hold expansion for the affected resource.
  - Escalate only that allocation after the unknown window.
  - Never zero the resource budget (`src/policy.rs:243-244, 371-378`).
- **Rationale.** This fixes D6 and FR-008.
- **Alternatives considered.** Zero the budget (today's behavior). Rejected: it is
  all-or-nothing.

## R11. Victim selection

- **Decision.** Order candidates by a deterministic cost tuple:
  1. Is not controllable. Excluded: NeedsReconciliation, preparations and authority-lost
     allocations.
  2. Would not free the contended resource.
  3. Class: opportunistic before Guaranteed.
  4. Phase: pending before running.
  5. Progress lost: time since the last accepted checkpoint, else since start.
  6. Fit: smallest amount at or above the needed reduction; otherwise largest.
  7. Assignment ID, as the final tie-break.

  Checkpoint times come from the agent's publication records.
- **Rationale.** FR-016. It also fixes victim slots being "spent" on allocations nobody can
  control (`src/policy.rs:596-615`).
- **Alternatives considered.** A weighted score. Rejected: harder to explain in reasons and to
  test.

## R12. Fair share and principals

- **Decision.**
  - Principals are process owner UIDs. CPU and RAM read the owner from sysinfo or
    `/proc/<pid>/status`; GPU maps each NVML PID to its UID.
  - The CedeGrid account is one principal. Own work's share is that principal's share minus
    the account's unmanaged usage.
  - Demand whose owner is unreadable (`hidepid`, permission) counts as one extra principal.
  - Fair share uses equal weights and is work-conserving (max-min fairness over current
    demand).
- **Rationale.** This matches the spec terms and assumption, using only read-only observation.
- **Alternatives considered.** Per-process fairness. Rejected: one user with many processes
  would dominate.

## R13. Configuration and setup writes

- **Decision:**
  - Add a `[contention]` section with per-resource tables. All fields are additive with serde
    defaults, under `deny_unknown_fields`.
  - `gpu.execution_mode` and `gpu.best_effort_external_processes` stay parseable, have no
    effect, and warn at load time.
  - Setup edits only the tables it manages, using `toml_edit` as a direct dependency:
    comments and other tables are preserved, and the file is validated with `Config::load`
    before an atomic rename.
  - The terminal interface uses raw mode through `libc` termios plus ANSI escapes, with no new
    crate.
- **Rationale.**
  - `toml_edit` 0.22.27 is already in `Cargo.lock` as a dependency of `toml` 0.8, so making it
    direct adds no new crate. This is the written justification the constitution asks for.
  - `crossterm` would add a new crate tree.
- **Alternatives considered.**
  - Rewriting the whole file with `toml` serialization. Rejected: it loses comments.
  - `crossterm` or `ratatui`. Rejected: new dependencies.

## R14. Ledger and schema changes

- **Decision:**
  - The coordinator distributed schema moves 3 → 4 through one explicit, versioned migration.
  - The migration adds:
    - `drain_requests.cause`;
    - `reservations.yield_cause`;
    - a CHECK constraint on `reservations.phase`.
  - It recreates the affected triggers (today `CREATE TRIGGER IF NOT EXISTS` never updates
    them).
  - The snapshot model moves 2 → 3, with additive fields. Readers accept 1–3 and keep their
    original interpretations.
  - Backup, restore and upgrade learn the new versions.
- **Rationale.** FR-018 needs a recorded yield cause. The audit (M12) showed that ad hoc DDL
  leaves `user_version` meaningless.
- **Alternatives considered.** Encoding the cause in the free-text detail. Rejected: it is
  parsed with `contains()` today, which is fragile.

## R15. Threshold defaults

- **Decision.** Use the provisional defaults from the spec's table as config defaults. Each is
  a field in `[contention.<resource>]`.
- **Rationale.** FR-025 and the maintainer decision. Hardware validation (quickstart §5)
  replaces them.
- **Alternatives considered.** Constants. Rejected by the maintainer.

## Revisions after US4 (2026-10-25)

The maintainer decisions B1–B14 (plan.md, "Revisions before US5") supersede parts of this
research:

- **R4.** GPU compute gets a share through the rootless SIGSTOP/SIGCONT duty cycle (T087d), and
  RAM takes a runtime cap through `memory.high` on delegated cgroups (T138).
- **R11.** Victim order puts pending before class, as FR-016 and `victim_order` do.
- **R12.** Principals need `uid_min` and the principal window; lower user IDs are background
  demand (FR-059).
- **R13.** The legacy GPU keys are interpreted with a warning, not ignored (FR-031).
- **R5.** The optional launch-time `SCHED_IDLE` for Yield First CPU is not adopted in this
  feature; launch keeps the existing non-negative nice, and caps and pauses carry the response.

## Utilization revisions (2026-10-25)

Constitution 2.1.0 and plan.md, "Utilization revisions before US5", supersede further parts:

- **R12.** Under saturation, a contending principal whose unknown demand is below one margin
  step is probed; a principal that does not grow into the freed capacity is satisfied at its
  usage plus the contention floor, and max-min bounds it there.
- **R15.** The margins are 15/5/2% on compute and 20% on memory for Yield First; the CPU
  reserve defaults to 0 and never stacks on the margin; CPU admission charges running
  opportunistic work at its p95 usage over 60 s. Real hosts still have to confirm SC-014 and
  SC-015 (T117).

## Review revisions (items 002–013, 2026-10-25 to 2026-10-26)

plan.md, "Revisions from review items 002–013", supersedes further parts:

- **R8.** External GPU compute is measured from per-process samples when their sum is within
  `attribution_tolerance_percent` of device utilization, and is grouped into principals by UID.
  Unknown activity remains the fallback (T152, T165).
- **R12.** A larger external demand is watched for one response window and can be recorded as
  satisfied. Records are bounded at 64 per resource and never expire (T155, T156).
- **R15.** CPU admission uses the p95 credit only after a full window (T157).
- **R4.** RAM takes a `memory.high` cap on delegated leaves (T138); see the table above.

## R16. PSI as CPU saturation evidence (T141)

- **Decision.** External PSI counts: the highest `some` avg10 of the cgroups outside own
  work's user slice, above `contention.cpu.psi_some_percent` (10, provisional), judged per
  observation. Own and system-wide PSI never count; missing PSI adds nothing.
- **Rationale.**
  - System-wide `/proc/pressure/cpu` also counts stalls between own threads. A job with more
    threads than its share beside a small external user would raise it with nobody external
    waiting, and cut own work for nothing.
  - Per-cgroup `cpu.pressure` outside the own subtree shows external tasks waiting.
  - `some` avg10 is already a 10-second kernel average, so no second window is needed, and the
    ladder's calm window stops cycling.
- **Decision (item 015).** Under PSI-only saturation, probe every contending principal. Own work
  steps down by the mode's margin step per response window, and a principal that does not grow
  into the freed capacity is satisfied at its usage plus the contention floor. A cgroup shared
  by own work and other users' processes counts as own and is never PSI evidence.
- **Rationale (item 015).** PSI can stay high after own work yields, for example when the
  external cgroup is held back by its own `cpu.max`. Without probing, Neutral keeps a larger
  external demand (above its 5% step) at the fair share for as long as PSI stays high. Yield
  First, probing up to its 15% step, learns the demand is satisfied and keeps more than
  Neutral: not monotonic, and wasteful. Probing every principal ends that in every mode.
- **Alternatives considered.**
  - Stop counting PSI once own work has held the fair share for N response windows with flat
    external usage: rejected, because it adds per-episode state that probing already provides.
  - Accept the non-monotonic state: rejected (constitution, monotonic modes).
  - System-wide PSI as a fallback: rejected, because it cannot separate own from external stalls.
  - Own PSI as a signal: rejected for the same reason.
  - A consecutive-interval window or a separate clear threshold: not needed beyond the ladder's
    hysteresis; T117 can revisit it with evidence.
- **Verify natively** (T141 gate): which slices expose `cpu.pressure` to a non-root user, and how
  external avg10 behaves under a CPU-bound load in another slice.

## R17. Faster Performance ramp (item 017)

- **Decision**:
  - Performance compute scales up one step per monitor interval, while memory keeps 2 s and the
    reclaim backoff.
  - The ramp target becomes 90% of available minus the margin within 10 s.
  - In Performance, one heartbeat admits as much as fits within the allowance (launch slots up
    to the worker limit, bounded by placement).
- **Rationale**:
  - Compute is reclaimed within one interval, so its rise can follow what each interval
    observes; SC-012's per-interval reclaim already bounds the cost of a wrong rise.
  - The measured limit was the agent's admission pace: one worker per 2 s cooldown on the 2 s
    heartbeat, about 16 s for eight workers.
  - Memory reclaim waits on hints and deadlines, so its damping stays.
- **Result** (simulation in `tests/contention.rs`, 8000 m, 500 m jobs, 2 s heartbeat): Performance
  reached its 7056 m target at 30.5 s before the change and at 2.5 s after it; Yield First
  (390.5 s) and Neutral (140.5 s) did not change. A held compute cap still rises no faster than
  the step dwell, so a cap set 1.5 s earlier rises after 1.5 s instead of 2 s.
- **Alternatives**:
  - A shorter heartbeat (rejected by the decision; it costs coordinator load on every node).
  - Faster memory scale-up (rejected: memory is slow to reclaim).
  - A larger scale-up step instead of a shorter interval (it would change the margin-sized step
    that FR-071 and the ladder share).

## R18. Own-only GPU intervals (item 023)

- **Decision**: an interval is own-only when both process queries succeed, at least one PID is
  listed, every listed PID is an own verified PID checked through a pidfd (no PID 0, no
  unresolvable entry), no MPS control daemon or server is present, device utilization is
  available (own compute is the device utilization), and device memory attributed to no listed
  PID is no higher than at the last measured, own-only or empty interval plus
  `contention.gpu_memory.noise_tolerance_mib`. External compute is then 0 and own compute is the
  device utilization, without fresh samples, and the policy takes the measured path at once
  without restarting the measured streak. Any failing condition keeps the existing path.
- **Rationale**: report 021 measured GPU activity Unknown in 88% of a 100-task session and a
  0.72 matched throughput for 20 s jobs in Neutral, because each new own process restarted the
  per-process baseline and the 20-interval quiet window. With every listed PID own and verified,
  no unlisted context can be computing unless it holds unattributed memory, which the memory
  condition catches.
- **Memory attributed to no listed PID**: in an own-only interval this is the memory not
  attributed to own PIDs, the item's condition. The baseline uses the same measure, so memory of
  an external PID listed at a measured interval is not in it, and a context that later drops out
  of the list while keeping its memory counts as growth.
- **Empty interval as memory baseline**: an interval that lists no process, with both queries
  successful, also sets the memory baseline; otherwise a device that starts empty could never
  have an own-only interval. Its unattributed memory is at least as clean a baseline as a
  measured interval's. An empty interval itself stays on the existing path.
- **Alternatives**: let Neutral lift a cap caused only by Unknown (report 021 option B; rejected,
  a policy change); state that short GPU jobs run at the fair share (option C; rejected,
  work-conserving test).

## R19. Launch pacing and the agent loop's cost (item 025)

- **Decision**: launches run on the blocking pool, at most 4 at a time; a heartbeat takes at most
  32 new assignments; a slot pins its attempt only when a checkpoint head is new or its result is
  released; lease requests share half the monitor interval per iteration, longest waiting first,
  and the longest-waiting one always goes.
- **Rationale**: report 024 saw 134 tasks launched at once stall the agent 4.2 s and every
  initial authorization refused for stale evidence. A scratch profile (150 one-minute tasks,
  default cadences, telemetry TTL 3 s) found: a launch blocks the current-thread runtime 31 ms on
  the test host, 42–59 ms in Docker and 233 ms on macOS, and every await in the loop then runs the
  waiting launches (one authorization request took 16.3 s with 89 launches waiting); `pin_attempt`
  for every slot in every interval (a new state connection and two synced writes; 5–8 s per
  interval with 50 slots on macOS); lease requests one at a time at 50–90 ms each where synced
  writes are slow. An iteration longer than the interval made both the coordinator's evidence
  (TTL refusals) and the kernel's CPU accounting window stale (every allocation's usage unknown,
  admission held).
- **Alternatives**: queue offers in the agent (rejected: the coordinator releases an offer not
  authorized within `lease_ms`); a smaller fixed per-heartbeat bound alone (rejected: macOS would
  need 1–2 per heartbeat, which breaks SC-004's ramp); concurrent lease requests (rejected for
  now: the requests are paced and the coordinator serializes synced writes, so total time does
  not shrink).

## R20. Cap duty from the measured rate (item 025)

- **Decision**: the rootless actuator keeps the family's last measured natural rate and starts a
  changed CPU cap at target ÷ rate; without a measured rate it starts at target ÷ request as
  before.
- **Rationale**: in report 024's Yield First runs with a task loop that makes up lost CPU, held
  caps rose together, each changed cap restarted at duty target ÷ request, so a cap at or above
  the request ran uncapped for one period, and own work jumped from 2225 m to 3838 m against a
  3238 m share; the excess then paused or capped most tasks to zero about every 30 s. A scratch
  policy simulation reproduces the sawtooth only when it models this burst.
- **Alternatives**: count raised caps at their targets in the spare (rejected: the simulation
  showed no gain with the burst modelled and no gain without it); bound rises tighter than the
  spare (rejected: the existing rule already keeps a round within the spare).


## R21. A contender's share of a CPU own work also runs on (item 025, analysis)

- **Finding**: on the test host (rootless backend: CPU caps are SIGSTOP/SIGCONT duty cycles, no
  `cpu.max`), a one-core contender under the account that runs CedeGrid got 689 m pinned and
  637 m unpinned while own work held its share (2961 m of 2999 m, 2996 m of 3046 m) and own plus
  external stayed at capacity minus the margin. Performance's target, capacity minus external
  minus margin, holds at any split where own plus external fills that much, so the split is the
  kernel's: own work's duty-cycled tasks keep running on the contender's CPU during their run
  phases. Report 023's simulation pools capacity, so freed CPU reaches the contender in full and
  it reaches its core in 5 intervals; on the host only part of what own work frees lands on the
  contender's CPU. With q cores of own work spread over 4 CPUs and an even split while both run,
  the contender gets about 1 − q/8 of its CPU: 0.63 at q = 2.96, as measured.
- **What would let it reach its demand**: for the owner's own programs, a lower weight for own
  work within the owner's slice (the leaf's `cpu.weight`, or nice in a rootless lower-share
  step), which SC-002 as amended does not require. For another account's contender, its slice
  and own work's split a contended CPU evenly, and its waiting shows as external CPU pressure
  (FR-072), so Performance keeps reclaiming toward the fair share; at q = 2 that gives about
  0.75 of its CPU, and the rest needs own work to leave that CPU.
- **Prediction for item 026**: under a second account, the pinned contender gets about 0.6–0.75
  core, external `cpu.pressure` some avg10 stays above `psi_some_percent` while it waits, and own
  work is reclaimed toward its fair share; SC-004's second bullet would still miss pinned.

## R22. Authorizing a burst's own reservations (item 026)

- **Decision**: the node states, per resource, that its admission is closed only by its own
  never-measured new allocations or the cooldown only they restarted; at the first authorization
  of an existing reservation, and only there, the coordinator counts such a CPU or RAM resource
  as open.
- **Rationale**: in report 025's Docker burst, 59–63 of 150 reserved tasks were refused because
  the burst's own launches had closed admission (`allocation_unknown_hold`, then the cooldown),
  although every one was placed against the budget while admission was open. A structured field
  lets the coordinator decide without parsing reason text, and an older report, which lacks it,
  fails closed. Only the first authorization is exempt: new placement and preemption must still
  see the node's state, and renewals don't check admission.
- **Refused reservations as the code stands**: the slot keeps asking each iteration; after
  `prepare_timeout_ms` (10 s) the supervisor gives up ("execution authorization absent", verified
  release), the agent sends `Fail` with an execution failure, and `record_failure` counts it, so a
  replay-safe task is requeued with backoff while failures stay under `retry_limit` (3) and its
  `max_attempts` allow, otherwise it needs reconciliation. FR-018 tells a yield from a failure
  only by a recorded yield cause, so a refusal at the barrier counts as the task's failure;
  requeueing without counting it would need the spec to say that a refusal at the barrier is not
  the task's failure.
- **Alternatives**: infer the cause from reason codes (rejected: reason text is not an interface);
  let the agent hide its own unmeasured allocations from the admission it reports (rejected: new
  placement would then see a node more open than it is).

## R23. Own CPU over its own read interval, and a window with one bound (item 027)

- **Decision**: In Full, own CPU is the delegated root's `cpu.stat`, read next to `/proc/stat`.
  In Tracked, an established process's delta is divided by its own read interval and the summed
  bound keeps the tick lag but not the read-pass term. The allowance, targets and admission budget
  use own CPU and host busy over a window of at least 8 intervals with one bound, while the
  per-interval estimate stays within its own bound of it.
- **Rationale**: in report 026's run 5 the bound on own CPU was 1441 m at 116 tasks on 14 CPUs and
  counted as external, so 7.6% of the host stayed idle. Two terms made it: each delta divided by
  the whole aggregate interval although its process was read for about 30 ms less of it (about
  6% of own usage), and the read-pass term, one tick plus the pass length per scope CPU (14 ×
  26 ms per 500 ms). Host busy adds one USER_HZ tick of idle rounding per CPU per interval,
  2% of each CPU at 500 ms, which a 4 s window cuts to 0.25%.
- **The first interval**: a worker started mid-interval was once scaled to a full rate and read
  above host busy natively. Its first delta keeps the aggregate divisor; later deltas
  cover a process that ran the whole interval.
- **Pending time over a read pass**: a read lags by the runtime not yet folded at the last tick.
  Summed over processes read one after another, the strict worst case is the smaller of one tick
  per own thread and one tick plus the pass length per scope CPU (117 ms per interval at 117
  tasks). The per-interval bound uses one tick per scope CPU, as decided; runtime pending at one
  read is folded and counted at a later read, so over the window the overcount is that of the
  window's first reads only, and the strict worst case stays at 29 m (0.2% of 14 CPUs) over 4 s.
- **Alternatives**: chain each interval's end read as the next baseline (rejected: the baseline
  would precede the aggregate interval, which the unknown rule forbids); keep the read-pass term
  and shorten the pass (rejected: 117 reads cannot be made simultaneous).

## R24. Batched lease requests (item 027)

- **Decision**: one request per agent iteration carries every due renewal and initial
  authorization, renewals first by deadline; the coordinator answers in one transaction with a
  savepoint per item and one synced commit.
- **Rationale**: each renewal cost a synced coordinator write of about 9 ms natively, and renewals
  shared half of each monitor interval, so a node kept about 117 leases renewed; at 150, 123
  leases expired (report 026, run 0). R19 rejected concurrent requests because the coordinator
  serializes synced writes; a batch removes the writes instead, one commit per iteration whatever
  the count. Bounding a batch (128 items, 1 MiB) keeps it under the 3 MiB request limit.
- **Compatibility**: `Request` denies unknown fields and an older coordinator rejects an unknown
  `op` before any state change, so the batch is a new variant behind `lease_batch/1`; on a batch
  error the agent forgets the feature and sends single requests. Per-item events keep their
  bytes, so the 0.2 gate's `renew` counts do not change.
- **Alternatives**: renew at a fraction of the lease (rejected as the only change: the ceiling
  stays linear); concurrent single requests (R19).

## R25. Yield First over its target (item 027)

- **Decision**: own usage over its target without external growth is enforced first and stepped
  down only after `min_step_dwell_intervals` with caps applied; its cause is `own_over_target`.
- **Finding from run 6** (report 026, `run6-trace.txt`): every burst above the target (to 3420,
  3685 and 3208 m against about 3150) came within 1.5 s after a `step_reversed_contention_ended`
  reversal, and none after a raise alone. Those reversals were driven by the external reading,
  168–385 m beside a 60 m user, as CedeGrid's own 41 m, the measurement bound and noise counted as
  external; once an `external_growth` cap's trigger fell below the hysteresis, the cap was removed
  and catch-up tasks spent their banked time at a full core. The actuator's start after a raise
  (R20) held its caps. So the overshoot is catch-up work after reversals, set off by the
  measurement; each burst then stepped own work down and restarted the 30 s interval.
- **Rationale**: a cap the actuator already enforces needs no new step, and a step taken for
  noise is what reverses and sets off the next burst. Steps for external growth keep their path
  and bounds (SC-002).
- **Alternatives**: rerun only (rejected by the reviewer); longer hysteresis for every cause
  (rejected: slows the response to real external growth).

## R26. Principals counted threads as processes (item 027, decisions on report 027-1)

- **Decision**: on Linux, principals come from a process table that reads each
  `/proc/<pid>/stat` once per interval and a process's real UID once from `status`, cached by PID
  and start time; CPU is computed as sysinfo computes it. Own tasks are re-verified through the
  pidfd held since their first verification.
- **Finding**: sysinfo 0.38's `ProcessRefreshKind::nothing()` keeps `tasks` on, so a refresh of
  all processes also enumerates `/proc/<pid>/task/*` and adds every thread as its own process
  entry. The principal sums iterated those entries, so a process's CPU was counted once for the
  process and again for each thread, its resident memory once per thread, and own tasks' threads
  (not in the managed PID set) as the owner's unmanaged usage. The process table counts each
  process once.
- **Rationale**: one file per process per interval instead of a process and thread walk with
  several files each; the per-process values stay those sysinfo computed for the process entry.
- **Alternatives**: sysinfo with `without_tasks()` (rejected: still reads several files per
  process and re-reads names); principals sampled less often (rejected: changes FR-059's windows).


## R27. The GPU compute share in Full (item 028)

- **Decision**: in Full, the GPU compute duty's off phase freezes the leaf through
  `cgroup.freeze`, confirmed by `cgroup.events` as Pause is, when that control is authorized;
  otherwise it uses the rootless SIGSTOP duty. The on phase never thaws a freeze another
  resource holds. Where the CPU cap is also a duty cycle, both share one cycle at the smaller
  duty.
- **Rationale**: a freeze covers every process in the leaf, forked children included; it is how
  Full already pauses; and frozen processes show as sleeping, not stopped. Kernels already
  submitted keep running in both mechanisms, so the granularity is the same: the period plus
  the longest kernel.
- **Alternatives**: keep GPU compute Cap unavailable in Full (rejected: Full then pauses where
  Tracked holds a share, so Full does less than Tracked); always use SIGSTOP in Full (rejected:
  it reaches only the verified family's listed processes, and Full already has the freeze).
