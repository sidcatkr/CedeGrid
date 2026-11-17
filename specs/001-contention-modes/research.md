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
- **Item 029.** Only Yield First tasks launch at `cpu.nice`. Where users share one CPU cgroup,
  the kernel weights nice 10 at 110 against 1024 for nice 0, so a contended core gives own work
  about a tenth, far below the fair share Neutral and Performance promise; and with
  `RLIMIT_NICE` 0 the nice can never be lowered again. Neutral and Performance launch at nice 0
  and yield through the caps; a task that later leaves Yield First keeps nice 10.

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

## R28. OOM order by fault and the unprivileged `oom_score_adj` (item 029)

- **Decision**: each own task's `oom_score_adj` at launch is its baseline. In-share own work stays
  there; the tasks that make up own memory above its fair share go to 1000 and return to the
  baseline afterwards. The supervisor writes a member's value through the member's `/proc/<pid>`
  directory under Principle I's rule for calls that take only an ID: pidfd alive before, start
  time read through the directory and matched, pidfd alive after.
- **Kernel facts** (`fs/proc/base.c` `__set_oom_adj`, `mm/oom_kill.c` `oom_badness`, v6.6): the
  killer takes the highest badness, RSS + swap + page tables + `oom_score_adj` × totalpages /
  1000. Without `CAP_SYS_RESOURCE` a process may set any value at or above its
  `oom_score_adj_min`, which only a privileged writer sets and children inherit; so own work can
  be raised and lowered back to the baseline it launched with, never below it.
- **Rationale**: without privileges CedeGrid cannot make the kernel take another user's process
  before a larger own one; it can order own processes, and keep in-share own work at everyone
  else's baseline. A `/proc/<pid>` directory opened while the pidfd shows the process alive stays
  bound to that process, so the write cannot reach a reused PID.
- **Alternatives**: every own process at 1000 (rejected by constitution 7.0.0: in-share own work
  died first for others' growth; kept as the opt-in `ram.prefer_own_oom_victim`); a negative
  value for in-share work (needs privilege; a later System mode).

## R29. Work whose supervisor died (item 030)

- **Decision**: the worker gate sets `PR_SET_PDEATHSIG` before exec: SIGKILL for Opportunistic
  work, SIGCONT for Guaranteed work; it then compares `getppid()` with the supervisor it was
  started by and exits if the parent already changed. The supervisor turns SIGTERM, SIGINT and
  SIGHUP into a release and drain. Supervisors run in their own process group.
- **Kernel facts** (`prctl(2)`): the signal is sent when the thread that created the child
  exits; it is kept across `execve` except for set-user-ID, set-group-ID or file-capability
  binaries, and it is not inherited by the workload's own children. A frozen cgroup v2 task and a
  stopped task both die on SIGKILL; SIGCONT resumes a stopped task.
- **Rationale**: without its supervisor an Opportunistic workload has no owner holding its
  handle, so it can be neither yielded nor drained, and after its lease expires the coordinator
  starts a second copy. Guaranteed work survives coordinator loss already, so it continues.
  A separate process group keeps a terminal's SIGINT or SIGHUP to the agent from reaching
  supervisors, which FR-080 leaves running.
- **Alternatives**: a subreaper that resumes orphans (it would have to act on processes through
  a handle it does not own); thawing from the agent (no handle; Principle I).

## R30. A report's age on the wire (item 030)

- **Decision**: `NodeReport.sample_age_ms`, behind a new feature `report_age/1`: the agent's
  monotonic milliseconds from the sample's start to the send. The coordinator judges freshness
  by it when negotiated, and by its wall clock against `observed_at_unix_ms` otherwise.
- **Rationale**: two hosts' wall clocks can differ by seconds; one monotonic age measured on one
  host cannot. Transit time is not counted, which is the same for either check.
- **Alternatives**: a coordinator-side receive timestamp (still compares sample time on another
  host); a larger `telemetry_ttl_ms` (hides real staleness).

## R31. The reload trigger (item 030)

- **Decision**: SIGHUP reloads the agent's configuration file. A file that validates and differs
  only in `[contention]` sections applies from the next iteration; any other difference is
  refused with `config_reload_refused`, naming the keys, and the running settings stay.
- **Rationale**: SIGHUP is the usual daemon reload signal and needs no new command or socket.
  Mode settings are what operators change at run time (the spec's assumptions); paths, the
  coordinator, TLS and storage profiles are read once and would need a restart to apply safely.
- **Alternatives**: a reload command over a new local socket (more surface); restarting the
  agent, which FR-080 now allows without losing work but which still pauses scheduling.

## R32. Retention constants (item 030)

- **Decision**: heartbeats report active executions plus the last 256 Released ones; the
  `observations` table keeps 24 h and at most 200,000 rows, pruned every 600 iterations; an
  attempt's directory goes once its outcome receipt is written and publication GC has emptied
  it.
- **Rationale**: 256 Released entries cover the coordinator's reconciliation of recent outcomes
  well inside its 2048 bound with 117 running tasks; 200,000 rows hold more than 24 h at the
  500 ms default, and the only reader takes the newest rows; pruning every 600 iterations keeps
  the delete off most iterations.
- **Alternatives**: operator keys for each (a config change; later if needed).

## R33. GPU compute over the whole interval (item 030)

- **Decision**: device utilization is the mean of `Device::samples(GpuUtilization)` since the
  previous read, the same interval the per-process samples cover; each PID's share is its summed
  `sm_util` over those samples divided by the device's sample count, rounded to the nearest
  point. A remainder of device minus listed processes up to one point per listed process is
  rounding, not unattributed usage.
- **Rationale**: a process stopped for part of each period has no samples while stopped; dividing
  by its own sample count read its busy level (run 5: 56% against a device at 13–36%), and
  truncation left a one-point remainder that the 1% principal floor counted as a principal (run
  5: a fair share of 334 instead of 500). Comparing like with like keeps the tolerance check
  meaningful.
- **Alternatives**: crediting own processes at their applied duty (only covers own work);
  raising the principal floor (hides small real users).

## R34. Subreaper and orphans (item 031)

- **Decision**: the supervisor sets `PR_SET_CHILD_SUBREAPER` on itself before forking the gate.
  A child of the supervisor that is neither the gate nor an SDK child gate is an orphan of this
  run, verified with the supervisor as its chain's root.
- **Rationale**: the kernel reparents an orphan to its nearest living subreaper ancestor; the
  supervisor runs one run and forks only its gate and SDK child gates, so no other process can
  become its child. The kernel gives an orphan to a live thread of the subreaper, so the
  supervisor reads every one of its threads' `children` files. Principle I's subreaper clause and Principle V's "the run's subreaper tree"
  describe exactly this. The attribute is per process and not inherited across fork.
- **Alternatives**: leaving orphans outside the family (a DataLoader whose intermediate process
  exits would escape every step); PID-namespace containment (needs privileges).

## R35. Discovery cost and its fallback order (item 031)

- **Decision**: every duty period, read each member's `task/*/children` over all threads; in
  every stop, the same walk inside the pass loop. If the efficiency gate shows it over budget:
  the leaf in Full, and in Tracked a full thread walk every K intervals with a main-thread walk
  between; stops always walk fully.
- **Rationale**: a forked worker's parent is the thread that forked it, which may be any thread;
  the main-thread file alone misses children of worker threads.
- **Alternatives**: a ppid scan of `/proc` every interval (it reads every process on the host;
  kept only as the fallback without `children` files).

## R36. Reaping safety (item 031)

- **Decision**: orphans are reaped by their own PID (`waitpid(pid, WNOHANG)`, or `waitid` with
  `P_PIDFD`), only for PIDs listed in the supervisor's `children` that are neither the leader nor
  an SDK child, on the loop thread that reaps the leader.
- **Rationale**: `waitpid(-1)` could take the leader's status, freeing its PID while the code
  still treats it as an unreaped child, which breaks Principle I's identity guarantee. A member's
  pidfd keeps naming the same process after it is reaped, so a reused PID is never acted on.
- **Alternatives**: a SIGCHLD handler (changes the process-wide disposition the reaping policy
  check forbids).

## R37. PSS over RSS for a family (item 031)

- **Decision**: a family's memory is the sum of each member's `Pss_Anon` + `Pss_Shmem`.
- **Rationale**: forked workers share copy-on-write pages with the leader; RSS counts a shared
  dataset once per worker, so own memory would read several times its size and could trip
  FR-023's floor path in Performance, a loss the rule does not allow. PSS divides each shared
  page among the processes mapping it, so the sum counts it once.
- **Alternatives**: RSS (overcounts); `memory.current` in Tracked (no leaf there).

## R38. The stop's pass loop (item 031)

- **Decision**: a stop discovers and stops every member, then discovers again, at most 3 passes.
- **Rationale**: a stopped process cannot fork, so each pass can find only children forked
  before their parent was stopped; three passes cover a fork racing the first pass and its child
  racing the second. Anything later is found by the next interval's discovery and stopped then.
- **Alternatives**: one pass (misses a fork in flight); an unbounded loop (no bound on a stop).

## R39. The member bound (item 031)

- **Decision**: at most 256 tracked members per run, a constant.
- **Rationale**: every member holds a pidfd; the bound keeps a run's descriptors well under the
  agent's and supervisor's limits (FR-077) and bounds discovery. Members past it keep today's
  counting and are reported `over_bound`.
- **Alternatives**: a config key (a tuning knob with no workload that needs it yet).

## R40. The derived GPU sample age (item 032)

- **Decision**: unset, `gpu.process_sample_max_age_ms` is max(2000, 2 × `monitor.interval_ms`);
  an explicit value must be above the interval.
- **Rationale**: the sample cursor is read once per interval, so an age limit at or below the
  interval would leave GPU compute unknown at every read. Twice the interval tolerates one late
  read, and 2000 keeps the default unchanged at the default 500 ms interval. A config that never
  mentions GPUs, on a host without them, has nothing to refuse.
- **Alternatives**: refuse long intervals everywhere (030's rule: a GPU key blocks a CPU-only
  host); migrate files to write a value (an unset key needs nothing); refuse only on GPU hosts
  (a config would load on one host and not another).

## R41. The workload's own session (item 033)

- **Decision**: the worker gate calls `setsid()` before exec.
- **Rationale**: the gate shared its supervisor's process group, so when a Tracked supervisor died
  with members stopped, its exit orphaned that group and the kernel sent every member SIGHUP and
  SIGCONT (pinned in Docker). A session of its own leaves the workload's group orphaned from the
  start, so no later exit can trigger the rule, and a stopped member stays stopped for the agent.
- **Alternatives**: `setpgid` alone (the group would still be orphaned by the supervisor's exit,
  since its parent stays in the same session); leaving it (Guaranteed work that handles SIGHUP is
  told to hang up, and members are continued by the kernel rather than by CedeGrid).
- **Observed, unexplained**: in the pin, members that left SIGHUP at its default action were not
  ended by it, although that action ends a process. A session of its own makes the rule moot, so
  it was not pursued (item 034).

## R42. Recovery by recorded identity (item 033)

- **Decision**: the agent re-opens each recorded member by identity (pidfd, then start time and
  IDs through `/proc`, then alive) and signals through that pidfd, from `family.json` and the store.
- **Rationale**: it covers an agent that was down when the supervisor died, holds no descriptors
  across agent restarts (FR-080 makes those routine) and adds none to the agent's fd budget. A
  reused PID has another start time and is never signalled.
- **Alternatives**: the supervisor passes pidfds to the agent with `SCM_RIGHTS` (up to 256 fds per
  run, lost on every agent restart, nothing to act with if the agent is down); a watchdog process
  per run (one more process to supervise).

## R43. The per-process sampling period (item 034)

- **Decision**: each PID's summed `sm_util` counts against the slots of the per-process sampling
  period: the window from the cursor to the newest per-process timestamp, divided by the shortest
  positive gap between one PID's consecutive timestamps in it, rounded, and at least each PID's
  own count. With no PID holding 2 samples in the window, R33's slots (the device's sample count)
  stay. Each interval records the device's and each PID's sample counts and timestamps.
- **Rationale**: in the 033 batch the summed per-process reading was a steady third of the
  device's in every window (33% against 99%, 19% against 55%, and 5% against 15% with a light
  user alone), which fits a driver that samples processes at a third of the device's rate while
  R33 divides by the device's count. A process with fewer samples than its slots was idle or
  stopped for the rest, so R33's "stopped time counts as idle" holds. The most-sampled PID's count
  would overstate every share when all were stopped for part of the window.
- **Risk**: the batch's observations returned 1 or 2 per-process samples per interval with 2 or 3
  processes listed, so a PID may hold fewer than 2 samples in a 500 ms window, and the fallback
  then applies. The recorded counts decide that natively.
- **Alternatives**: comparing shares instead of absolute sums (the item's fallback, not the plan);
  dividing by each PID's own count (reads a stopped process at its busy level, R33).
- **Superseded** by R46: the 034 rerun showed one sample per process per interval, so no period was
  readable within a window and the fallback applied in every interval (item 036).

## R44. The one-move return (item 034)

- **Decision**: at the first interval with contention ended, while own work holds no excess, the
  binding CPU steps held for external growth rise to a common level L, filled from the lowest
  upwards with each counted at the smaller of its target and its usage, so that they together hold
  the mode's target; none is lowered, and only that one move skips the dwell. Steps held for other
  causes and caps their work does not reach keep their paths.
- **Why the limits**: the first snapshot comparison showed that with excess left a raised cap frees
  nothing and the excess falls on the next victim, that the dwell skip also moved caps held for
  unknown measurement, and that a cap its work did not reach was kept where the old path released
  it; with the limits every changed decision is a rise of a binding step held for external growth.
- **Rationale**: the batch's records show 13 caps at targets from 1 m to 253 m at the contender's
  end; each rose to its usage plus an equal part of the spare, so caps at the minimum duty got a
  small rise and the rest waited 2 s per step. Report 030's simulation had equal caps, so the same
  rule summed to the target there. Water-filling gives the lowest caps the room and keeps the sum
  at the target, so no step overshoots and is capped again.
- **Alternatives**: releasing every held cap (own usage could overshoot the target and be capped
  again, a cap-uncap cycle); an equal split (would lower caps above it); shortening the quiet
  count (it is each mode's hysteresis).

## R45. The PSS period (item 034)

- **Decision**: a Tracked family's PSS is read every K intervals, K from the timed pass so that PSS
  averages at most 0.5% of the scope's CPU, 1 to 10; between passes own memory is the last PSS plus
  the members' RSS growth since that pass; a member set change forces a pass; a new or changed RAM
  step or OOM preferred mark on an estimated allocation makes the agent read fresh and evaluate
  again on the state from before the interval.
- **Rationale**: `smaps_rollup` walks each member's mappings, so its cost grows with mapped memory:
  five processes sharing 1 GiB cost about 43 ms per interval in the batch, T385's 64 MiB family 3
  ms. New private pages raise RSS and PSS alike, so the estimate does not miss growth; it misses a
  copy-on-write break until the next pass. Reading fresh before acting means an estimate never
  costs own work.
- **Alternatives**: reading only members whose RSS changed (an active worker's RSS changes every
  interval, so it saves little); holding the last PSS (misses growth); RSS in Tracked (overcounts
  shared pages, R37).

## R46. A GPU sample's span (item 036)

- **Decision**: a sample at t whose process's previous sample was at p counts sm_util × min(t − p,
  W) ÷ W in the window W where it arrives; with no previous sample it covers from the window's
  start. Each PID's last timestamp is kept across intervals per device and dropped when the PID
  leaves the device's process list.
- **Rationale**: the driver returns one sample per process per query, covering the time since that
  process's previous sample. The duty-capped probe's sample arrived steadily 1 or 2 device periods
  before the window's end, so part of its span lay in the previous window, which was already
  decided; counting only the overlap credited it 0.46–0.69 of its time (report 035), the whole
  span 0.96–0.98. min(t − p, W) keeps a long gap from crediting more than one window; busy time
  can shift by up to one window, but none is lost or counted twice.
- **Alternatives**: the overlap with the window only (loses the part before the window, report
  035); 034's in-window period (none readable on this driver).

## R47. The 4-interval comparison (item 036)

- **Decision**: when one interval's summed shares miss the tolerance, the shares and the device
  summed over that interval and the 3 before are compared; a pass reports the 4 intervals' average
  amounts. An interval that cannot be compared clears the history.
- **Rationale**: the unmanaged probe was sampled in only 27–71 of 80 intervals, with gaps of up to
  30 device periods. Estimated from the 034 rerun's timestamps, with the span alone the managed
  windows read unknown in 42–74 of 80 intervals, counting `quiet_window_intervals`; with the 4
  intervals as well, in 0–40. Averages, not this interval's amounts, since a sample missed here
  would read its process as idle. What remains is the 20-interval quiet window at each managed
  window's start, which item 036 leaves unchanged.
- **Alternatives**: a longer window (slower to see a real change); lowering the tolerance's bar
  (hides real disagreement).

## R48. GPU demand from uncapped intervals (item 039)

- **Decision**: in Neutral, Performance and Aggressive, external GPU compute demand is the average
  of the amounts measured in the last 60 intervals in which own work ran uncapped on the device. An
  interval is uncapped when neither the decision that set it nor the one before sent a cap or
  pause to own work there. While own work is held back by GPU compute caps only, a window of 6
  decisions lifts them every 30 s; a window whose mean differs from the estimate by 200 or more
  restarts it. Yield First takes the larger of the interval's amount and the estimate.
- **Rationale**: at `c52ba5c` a light user at a constant 200 requests a second read 158 m alone,
  77–130 m beside own work at full duty and 251–264 m beside own work at 70% and 40%, with its p99
  rising from 0.88 to 1.06–1.07 ms: the reading follows where the sampler caught its context, and
  the caps that the reading causes raise it.
  - **N**: beside full duty the light user is sampled in only 28–53% of intervals, at 33% or 66%.
    Over 800 full-duty intervals (038's two runs and 036's unmanaged windows), the mean of N
    consecutive intervals has an sd of 56 m at 10, 37 m at 20, 27 m at 40 and 23 m at 60 (90% of
    60-interval means within 27 m); 60 brings the error near Performance's 20 m margin.
  - **The window**: 5-interval means reach 300 m in 1.6% of the light user's windows beside full
    duty and in 82% of a saturating user's, so a rise is seen at the first window 82% of the time
    and within two windows 97% of the time, while a light user restarts the estimate about once in
    60 windows and the next window corrects it. 6 decisions every 30 s keep own work uncapped 3 s
    in 30, 10% of the capped time. During the window the time-slicer still gives the other user its
    slices, so it loses little.
  - **Two decisions**: a supervisor applies a changed duty at its next phase, up to one duty period
    (one interval) later, so the interval after the first uncapped decision can still hold a gap.
  - **Yield First's larger amount**: a sparse interval can read the other user at 0 while the
    estimate is above it; the larger keeps Yield First's target at or below Neutral's (FR-065)
    and never yields less than before.
- **Alternatives**: reading demand from own work's off phases (a sample spans about 167 ms and
  cannot be split at a phase edge); a per-host inflation factor (no single factor fits: beside full
  duty the reading is below alone, beside gaps above); uncapped windows in Yield First (that mode
  puts the other user first and its over-yield is the loss it allows).
- **What it leaves**: on a time-sliced device at full duty the device reads about 99% busy, so
  Performance's 2% margin can still leave own work's target a little below its usage, and a GPU
  cap is a duty of target ÷ 1000. The replays and T368 measure what that costs.

## R49. The GPU duty from own share (item 040)

- **Decision**: in Performance, Aggressive and Neutral a GPU compute cap's duty is its target ÷
  own work's uncapped share, clamped to the minimum duty and 1; the share is own work's amount
  averaged over the uncapped intervals in which it ran on the device, restarted with the estimate.
  With no share yet, and in Yield First, the duty stays target ÷ 1000.
- **Rationale**: target ÷ 1000 assumes own work at full duty holds the whole device. Beside another
  busy context on a time-sliced device it holds only its slices:
  - Performance beside the saturating user (036): the other user ran about 1555 requests a second
    alone, 750 beside own work without CedeGrid and 1023–1084 with it, about 68/32 where the
    time-slicer gives 50/50: own work 0.628 of without, for a user that already had its fair share.
  - Neutral beside the saturating user (038): own work 386 and 366 units a second against 736
    without CedeGrid and 1573 alone; the other user 1132–1146, about 75/25.
  - Beside the light user (039): a 10–30 m excess cut own work's running time by 17–19%.
  - A CPU cap already divides by the measured rate (R20). Dividing by own work's uncapped share
    turns an excess of e into a duty cut of about e ÷ S.
- **The allocation split**: with n own allocations on the device each is taken to hold S ÷ n, as
  the ladder's frame already splits own work's usage.
- **Alternatives**: a 15-point dead band (own work with several contexts could overshoot its target
  by up to 15 points); a duty from the device's busy share (it counts the other user's slices).


## R50. The GPU time-slice rule (item 041)

- **Decision**: on a time-sliced GPU (no MPS, no MIG), Performance, Aggressive and Neutral cap own
  GPU compute only while own work holds more than its share of the busy contexts, m × k > E, with
  m own work's contexts, E the external ones and k the external principals over the estimate's
  window; on the unknown path the rule holds with m ≤ 1. Yield First is unchanged.
- **Rationale**:
  - **T445**: beside the saturating user, own work at full duty ran 0.47 of the probe alone, and
    that user ran 747–754 requests a second against about 1555 alone, so the time-slicer split the
    device about 50/50 on its own.
  - **The light user** kept its full rate beside own work at full duty, with its best p50 and p99
    there; every cap raised its p99, to 1.13–1.18 ms against 0.82–0.89 ms.
  - **The readings**: caps built on per-process readings failed four host runs in a row. The
    readings are residency, not demand (037–039), come in steps of about 330, are sparse, and gave
    a phantom principal and an own share learned from two to five readings (T445). Each fix
    exposed the next fault.
  - **Counting is enough**: while m ÷ (m + E) ≤ 1 ÷ (k + 1) own work cannot take more than its
    fair share, and with one own context that always holds.
- **Assumption**: the driver gives busy contexts equal slices. It does not hold under MPS (one
  shared context), on a MIG parent (instances, not slices), or with unequal context priorities.
  The docs say so, and MPS and MIG keep the measured path.
- **Alternatives**:
  - more tuning of the estimate and the duty (the inputs stay coarse and biased);
  - a dead band around the target (it still caps one own context for no gain);
  - a duty per context (it needs per-context readings the time-slicer does not give).

## R51. The time-slice duty from the context counts (item 043)

- **Decision:** at m × k > E on a time-sliced device, own work's duty comes from the context
  counts of slice-bound principals, d0 = (m + E_b) ÷ (m × (k_b + 1)). It steps up from the
  device's utilization while the device idles beyond the margin, with no estimate target, no
  single-reading excess and no pause.
- **Rationale:**
  - 042's traces: caps of 0–5 from single readings of 990, pauses held 20–45 s beside a saturating
    user that fills what a pause frees, and a target of 659 read from a slice.
  - Counts don't need readings. The device's utilization shows idle time without per-process
    sampling.
  - The slice-bound check keeps the light and partial cases work-conserving.
- **The 0.75 threshold against the traces:** averages of uncapped readings over each unmanaged
  two-process window (two own probes beside the saturating user), against a threshold of
  0.75 × 1000 ÷ 3 = 250.
  - Saturating side: per-UID 195–508 across 13 windows; the external total, owner left out,
    228–554. So 1 of 13 windows lies up to 9% below the threshold on the total, and 2 on per-UID.
  - Light side: T368's light user read 111–156 (alone 154–156), at least 38% below 250.
  - The readings stay biased toward own work's contexts when own work holds more contexts, so the
    saturating side's margin is thin. A factor of 0.65 (217) would put all 13 windows above and
    keep the light side 28% below.
  - The default stays at 0.75, as item 043 sets it. The report raises the lower value.
- **The step and hysteresis:**
  - It rises 5 points once per max(scale-up interval, dwell) and falls once per dwell. That is
    1.5 s in Performance and Aggressive at the default dwell of 3 intervals; Neutral rises once
    per 10 s.
  - It holds between half the margin and the margin of spare time, so it settles within one step.
  - Rise bound: from 1000 to d0, at most 10 steps (15 s) at m = 2, where d0 ≥ 500, and at most
    19 steps (28.5 s) in general, down to the minimum duty. A principal turning slice-bound is seen
    within the estimate's 60 intervals (30 s), or a window's 3 s once capped: at most 33 s.
- **No pause:** at d0 own work already holds its share, and a pause beside a saturating user never
  reverses.
- **Alternatives:**
  - a fixed duty with no slice-bound check (it leaves idle time beside light and partial users);
  - the 040 duty with smoothed readings (still readings);
  - a pause bound on GPU compute only (the readings problem remains).

## R52. Aligned GPU duty phases, the margin at d0 and the busy counts (item 044)

- **Decision:**
  - While a GPU compute cap sets an allocation's duty, its phase follows CLOCK_MONOTONIC modulo
    the period, so own allocations stop and resume together.
  - d0 leaves the mode's margin.
  - The counts take only contexts sampled busy, unless the samples are unreadable.
- **The phase arithmetic,** at duty 0.75 with m = 2 beside one saturating context. Own work's
  share is 2⁄3 while both run and ½ while one does:
  - aligned off-phases: 0.75 × 2⁄3 = 0.5;
  - random phases: 0.5625 × 2⁄3 + 0.375 × ½ = 0.5625;
  - staggered off-phases that never overlap: 0.5 × 2⁄3 + 0.5 × ½ = 0.583.

  043-t456 measured 0.515–0.522, between the aligned and random cases. Each actuator started its
  phase at its cap and reset it with `Instant::now()` at every switch, so phases began apart and
  drifted.
- **Why CLOCK_MONOTONIC:** every process on the node reads the same clock, which never jumps with
  wall-clock changes, so every supervisor computes the same position in the period without any
  coordination. The period is the monitor interval, which every supervisor on the node uses.
- **The mid-period rule:** own work runs while the position is below duty × period, and a stop
  holds until the period ends.
  - A lower duty ends the on-phase sooner.
  - A higher one lengthens the on-phase, or waits for the next period while stopped.
  - So no period has a second on-phase or a skipped off-phase. A change takes effect within one
    period, the monitor interval.
- **CPU-only cycles keep their phase:** aligned CPU stops would make own work's CPU use burstier
  for others, with nothing to gain. A cycle shared by CPU and GPU caps follows the aligned phase,
  since the GPU share depends on it.
- **The margin at d0:** d0 = (m + E_b) ÷ m × (1 ÷ (k_b + 1) − margin).
  - At m = 2, E_b = 1: 720 in Performance, 675 in Neutral, 750 in Aggressive.
  - On 043-t456's slope, about 0.69 of the pair alone per unit of duty, 720 gives own work about
    0.497 and 675 about 0.466. Aligned phases bring both down toward the counts' 0.48 and 0.45.
- **The busy counts:**
  - The test host's device lists an idle 2 MiB context of another principal throughout.
  - In 043-t456 the listing path counted it in 21–44 decisions per window. That gave d0 666 beside
    the saturating user, and a cap beside the light one, which wants no slices.
  - The samples were readable in those decisions; only the amounts were inconsistent. Counting
    the contexts sampled busy removes the idle one and keeps the listing for unreadable samples.
- **What remains, the drain:** GPU work a stopped process has already queued keeps running after
  the stop. After alignment the gap between own work's measured share and m × d ÷ (m + E) is that
  drain. 044's host runs measure it, and compensation waits for those numbers.
- **Alternatives:**
  - per-device phase coordination through the agent: it needs supervisor messages, and the clock
    gives the same result for free;
  - staggering phases on purpose: that raises own work's share;
  - keeping per-allocation phases and lowering d0 to match: that depends on how far phases happen
    to drift.

## R53. The step floor, the per-mille dwell average and the restart set (item 045)

- **Decision:**
  - The dwell's utilization average is in per mille, rounded.
  - The duty rises only beyond max(margin, 10) per mille of spare time, and counts as saturated
    below half of that.
  - The owner's unmanaged PIDs leave the estimate's restart set.
- **The arithmetic:** with a dwell of 3 readings at 100, 100, 99:
  - **Today** the floored average reads 99, so spare is 10. A zero margin rises, since 10 > 0.
    Performance doesn't fall, since 10 × 2 = 20 is not below 20.
  - **Per mille** the average reads 997, so spare is 3. A zero margin holds, since 3 ≤ 10. Both
    fall above d0, since 6 < 10 and 6 < 20.
  - A reading of 98 among two 100s reads 993, so spare is 7: still no rise with a zero margin.
- **Why one reading's quantum:** utilization comes in whole percent, and own work's phase switches
  dip one reading below 100 without idle time behind it. Below 10 per mille the reading can't tell
  the two apart.
- **Why not one step (5%):** that would raise Performance's rise threshold from 2% to 5%. Beside a
  partial user it would leave up to 5% idle, beyond its margin.
- **The restart set:**
  - Telemetry's external PID list is every listed process less the managed ones, so the owner's
    unmanaged processes are in it.
  - On the test host each run's unmanaged probes left the device at the end of the unmanaged
    window, and the estimate restarted there. The 40 s managed lead refilled it before the managed
    window, which is why 044-t456's managed windows all read from the window.
  - The owner's usage is already out of the estimate (Q23), so its processes have no reason to
    restart it.
- **Alternatives:**
  - a hysteresis count of dwells: it slows real rises too;
  - treating 99 as 100: that is the same floor without its reason;
  - keeping the restart: it costs a window of all-bound counts each time the owner starts or stops
    a process.

## R54. Own work pays for the re-measure windows (item 046)

- **Decision:** the time-slice duty starts at d0' = (d0 − w) ÷ (1 − w), with w the re-measure
  windows' share of the time the duty path caps own work.
- **w's arithmetic:**
  - A window lifts the cap for `GPU_DEMAND_WINDOW_DECISIONS` = 6 decisions.
  - The next window opens once the last uncapped interval is `GPU_DEMAND_WINDOW_EVERY_MS` = 30 s
    old. That interval is fed at the decision after the window, so 60 capped intervals follow at
    500 ms.
  - A cycle is 66 decisions, 6 of them at full duty: w = 6 ÷ 66 ≈ 0.091.
  - d0' = (d0 × 66 − 6000) ÷ 60, rounded: 725 at d0 = 750, 692 at 720, 643 at 675.
- **The window's first interval:** its decision sends no cap, so the supervisor lifts it at once
  and own work runs at full duty through it. It feeds no estimate, because the decision before
  it still sent the cap (the estimate's two-decision rule). For own work's held duty it counts as
  full duty: 6 intervals in each window.
- **Why own work pays:**
  - The windows are CedeGrid's measurement, and Principle II lets own work take CedeGrid's own
    overhead, while others keep min(demand, fair share).
  - On 045's host run the windows took the saturating user from ½ to 0.468–0.472 of its rate
    alone at a zero margin. In Performance they ate most of the 2% margin.
- **Why no fixed 1%:** the slicer's overhead exists without CedeGrid; unmanaged, the parts sum to
  about 0.99. It stays within the gates' 3% tolerance and isn't CedeGrid's cost.
- **Paths with no window:** a window opens only while every own allocation on the device is
  outside Yield First, the governing mode isn't Yield First, and no allocation there holds a CPU
  step of cap or more. Otherwise w = 0 and d0' = d0.
- **Alternatives:**
  - rarer or shorter windows: that slows reading external demand;
  - measuring w from the windows seen: it lags, and moves d0' with every window.

## R55. The mean charge when alone (item 049, T515)

- **Decision**: charge running opportunistic work at its window mean while no other principal is
  active on CPU, at its p95 from the interval one appears, reading the snapshot's principal list
  directly, without waiting for the principal window. An unreadable list counts as active.
- **Rationale**: item 047 measured `charge_shortfall` as what holds SC-014's Neutral and Yield First
  fill below target: 696 m and 609 m of charges above use. With no other principal an
  over-admission can only cost own work, whose burst raises `own_over_target` on own tasks.
- **Alternatives**: a lower percentile always (fills less when alone, and charges less under
  contention, where p95 protects others); lowering SC-014's targets (changes the criterion, not the
  fill).

## R56. An own task's missing sample and the external estimate (item 049, T518)

- **Decision**: name the failed check in the record, and keep the last window's external estimate
  through a window's refill, under the window's own bound rule.
- **Rationale**: item 047's 73 unknown decisions all lacked the aggregate CPU window; `af531c0` fixed
  that cause. But any reset leaves the estimate on one interval's wide bound for up to a window, so
  measurement looseness reads as about 250 m of growth, and in Yield First it restarted the cooldown.
- **Alternatives**: holding the estimate unconditionally (hides real growth, against Principle III);
  smoothing the interval estimate (delays the response to real growth).

## R57. What forms a principal for Yield First's cooldown (item 049, T519)

- **Decision**: another user ID admitted as a member on the resource, or unattributed usage at or
  above `principal_min_percent` for `principal_window_intervals`. Anything else that makes own work
  not fit holds admission while it lasts, and the timer keeps running.
- **Rationale**: the user's decision of 2026-11-02. On a host where people log in, Yield First
  barely filled, because every short change restarted a 30 s timer. The owner's unmanaged processes
  still count in the owner's share, so they still shrink the room.
- **Alternatives**: applying it to every mode (the user kept their rule; report if the same stall
  shows there); ignoring short changes in admission too (admission may hold while they last, which
  is the safe side).

## R58. How the coordinator knows a refusal at the barrier (item 049, T511)

- **Decision**: from its own record: a preparation reached it, no lease ever went out
  (`lease_sequence` 0), and no drain or cause is recorded. The receipt kind `authorization_refused`
  leaves the attempt out of `counted_attempts`, and it adds no failure.
- **Rationale**: a lease is the only way to the launch barrier's Authorized, so with none nothing ran,
  which the coordinator knows without trusting a report. It needs no wire change. A grant whose reply
  was lost still counts as an attempt, the safe side.
- **Alternatives**: a new `YieldCause` from the agent (a wire change in three languages, for what the
  coordinator already knows); any never-granted attempt, preparation or not (a launch that fails
  before the barrier would requeue for ever). OOM's bound of 3 × `max_attempts` is not applied: FR-018
  says a refusal uses no attempt, nothing ran, and the yield backoff paces the requeues.

## R59. What one drain of a draining stop costs the journal (item 051)

- **Decision**: two synced writes, the run's Draining and Released transitions; the stop's share per
  iteration is its 250 ms budget over twice the agent's last synced write.
- **Rationale**: item 048's kept burst run has six journal events a run, two after the stop. At one
  write a drain the stop sent 16 an iteration at a 15.6 ms write, and its iterations ran 1.0 to
  1.135 s against a median of 520 ms.
- **Alternatives**: a cap on runs draining at once (the cost follows the writes, not the count
  held); feedback on the last iteration's length (it needs a native run to tune, and a fixed count of
  writes is what the journal shows).
- **Outcome (item 054)**: reverted. The burst it was sized from was an operator drain, which reaches
  supervisors on renewal replies and never used this share; the iterations it was meant to shorten
  were slowed by the control lane's pacing (R67).

## R60. Why GPU activity read unknown after the last busy phase (item 051)

- **Decision**: the policy's own check, unknown whenever a listed context that is not background
  has no newer driver timestamp, no longer applies to a device the collector found Quiet, and the
  record names why activity is unknown. The check stays for a listed context read Idle or Active.
- **Rationale**: an idle context returns no per-process sample, so the check overrode the
  collector's Quiet until some process produced a timestamp. In item 048's three comparisons every
  change from unknown to open lands on the first reports after a wall-clock minute, seven of seven,
  and an earlier per-interval record on the same host shows the collector quiet at 9.7 s while the
  policy still read unknown. The check predates the Quiet class (R8), any process's timestamp
  satisfied it, own work's included, and the launch recheck never applied it.
- **Alternatives**: remove the check whole (three existing tests pin that a repeated or
  out-of-order sample cannot prove a listed context idle, and they would have to change); keep the
  check and give it a memory of the last fresh timestamp (it still asks for evidence the driver
  does not produce for an idle context); shorten Yield First's quiet requirement (a change to what
  the mode means, and not the cause).

## R61. How over-share tasks give memory back first (item 051)

- **Decision**: the over-share set is the victim-order prefix that covers own memory above its fair
  share. Its members come first and keep counting toward the excess while their wait runs; a decided
  drain counts in full; in-share tasks receive only the remainder.
- **Rationale**: the ladder already asks each victim in order for the excess left, but a step that
  holds counted nothing, so a deaf task's excess moved to the next victim after one interval. The
  OOM mark already names the same prefix, so the ladder and the mark agree on who is over its share.
- **Alternatives**: a per-task share (the constitution defines the excess over own work's share as a
  whole, so it needs an amendment); skipping the Guaranteed breach window for an over-share task (it
  costs Guaranteed progress on short spikes and changes FR-024); asking in-share tasks strictly after
  the over-share ones in Yield First and Neutral (the floor would wait past SC-012's bound).

## R62. Whose GPU activity holds Yield First's admission (item 051)

- **Decision**: from the device's compute contexts, which list the owner's unmanaged processes apart
  from other accounts' and name the busy ones. Activity from the owner's processes alone does not
  hold admission; another account's does; an unreadable owner or list keeps it unknown. Both callers
  of `gpu_admission` take the same reading.
- **Rationale**: the owner's use is already out of the external estimate and inside the owner's share
  (FR-059), as on CPU since item 049. The contexts are already collected for the time-slice rule.
- **Alternatives**: a new activity field on the snapshot (a schema change for what the contexts
  already say); leaving the launch recheck on the device's class (the policy and the recheck would
  disagree, as they did for R60).

## R63. When an own GPU process counts in the time-slice rule (item 053)

- **Decision**: from the interval the device first lists it, sampled or not, until one counting window
  of readable intervals passes with no sample for it; the owner's and other users' processes count
  only while sampled. The counts come from every readable interval, capped or not.
- **Rationale**: a context CedeGrid launched and can see listed is not idle because the driver has not
  sampled it (Principle III); until it counts, another user is under its fair share (Principle II).
  The cost of counting early falls on own work only and is bounded by one window.
- **Alternatives**: wait for a sample, as before (the other user lost 42 requests/s for 13 s in Full
  at `743312b`); count every listed process, external ones included (an idle external context would
  shrink own work for nothing).

## R64. What lifting a restriction CedeGrid wrote needs (item 053)

- **Decision**: the same leaf only, in every phase; never a value tighter than the one in place. New
  restrictions and freezes with no record still verify the members. A recorded freeze's members are
  read every 50 ms in every phase; a list unreadable twice counts as changed. Lifting a held cap while
  running when an unverified member joins is not taken.
- **Rationale**: lifting acts on nothing CedeGrid did not restrict itself; a refusal that nothing
  retries kept a freeze or a cap for good. Every lift rule found for a running cap also lifts caps on
  ordinary own processes.
- **Alternatives**: lift on one failed verification (lifts on ordinary forks); lift after two failed
  refreshes (still lifts in three cases); a persisted record (not needed: the writer is the restorer).

## R65. What restarts Yield First's cooldown beside a principal (item 053)

- **Decision**: in Yield First, only a principal's cause: a user admitted in that interval,
  unattributed usage reaching the window, an admitted user's rise beyond the tolerance since the last
  fitting interval, or on CPU unattributed usage past the window rising beyond it. The tolerance is
  `noise_tolerance_mib` on RAM and on CPU the larger of `principal_min_percent` of capacity and the
  measurement band, FR-034's quiet threshold.
- **Rationale**: the user's decision of 2026-11-02 carries no condition on other principals being
  present; the reference is each user's usage at the last fitting interval, so a change that is not a
  principal's holds admission without restarting the count.
- **Alternatives**: the reversal hysteresis as the tolerance (it is 0 with no step held, which is the
  login case); the literal unattributed rule on RAM (RAM's unattributed usage is one level, so it
  would remove the "shorter than the window" case; left to the report).

## R66. Own work's fair share while the list cannot be read (item 053)

- **Decision**: at most the last share a readable list gave, per resource; lower when that interval's
  own computation gives less; on CPU and GPU compute the share under contention is the one kept. The
  first readable list replaces it.
- **Rationale**: an unknown list is not a reason for a larger share (Principle III), and own memory
  above its fair share gives way at the floors in every mode (Principle II). The counts already stood
  through such an interval (TEL-9); the share did not, on any resource.
- **Alternatives**: carry the whole principal list (more state for the same bound); keep the
  as-computed share of an uncontended interval (capacity less everyone's usage, which does not hold
  the admitted users once the CPU saturates).

## R67. The agent's loop and the control lane (item 054)

- **Decision**: a control request books its charge on the control lane and does not wait; the
  lane's debt is capped at one heartbeat interval. Heartbeats and lease batches keep their
  schedule; failure reports keep a floor of 16 a second against the lane. Artifacts pace as before.
- **Rationale**: at 1 MiB/s a heartbeat for 150 allocations took 420–491 ms, about 2.8 ms an
  allocation, all of it the limiter's wait, and near 156 allocations every heartbeat would be cut at
  the loop's budget. The reclaim bounds are counted in monitor intervals, so the loop cannot wait on
  its own limiter. The floor is in requests, not bytes: the deadlines are in time, and a heartbeat's
  size grows with the node.
- **Alternatives**: a larger control share (still ties the cadence to a budget the docs let be
  small); failure reports bounded by the loop's share only (one constant fewer, but the lane then
  has no say over them at all).

## R68. How much each over-share task is asked for (item 054)

- **Decision**: two parts in the memory pass: the over-share tasks are asked for the excess, the
  last only for what completes it; the rest of the need is asked in the same interval by the cost
  order as before item 051, counting each over-share task at what it holds less what it was asked.
- **Rationale**: item 051 asked the first over-share task for the whole need, so in `random::46`
  (Yield First, 11482 MiB against a 4282 MiB target) a task holding 1000 MiB of excess was capped to
  0 while another kept 4282 MiB. The floors still come first in Yield First and Neutral.
- **Alternatives**: asking the over-share tasks last for the remainder (overrides FR-016's cost
  order); leaving item 051's single pool (the user's rule says own work does not lose for another
  task's excess, and the item calls that excess the task's own).

## R69. Who gets the save-progress signal (item 054)

- **Decision**: the in-share tasks on the breached resource, once each per episode, whoever caused
  the breach; over-share tasks get their hint and, if drained, their drain time.
- **Rationale**: an in-share task loses nothing by saving; an over-share task is being asked for
  memory, which the hint already says.
- **Alternatives**: per resource once per episode (a task in share later in the episode would get
  nothing); only for breaches others caused (FR-075 as before; the review's error).

## R70. The owner's GPU use and other accounts' quiet window (item 054)

- **Decision**: the collector's quiet window counts other accounts' contexts only; a context whose
  UID cannot be read stays in it. The owner rule opens Yield First's GPU admission when the busy
  non-own contexts are all the owner's and the other accounts' contexts have been quiet for the
  window.
- **Rationale**: the owner's activity restarted the device's one quiet window, so another account's
  idle context could never read quiet while the owner computed, and the owner's use held admission
  on any shared device, against the user's answer of 2026-11-03.
- **Alternatives**: a per-context quiet count in the policy (duplicates the collector's window);
  ignoring other accounts' idle contexts (they can start computing at any time; FR-027 keeps them).

## R71. How the same-core probe tells its own bias from a loss (item 055)

- **Decision**: the probe times even against odd units before it is ready, holds up to 8 blocks
  while they differ by more than 1%, and records what it found; the gate refuses a window without
  the record or over the limit.
- **Rationale**: at `743312b` the managed probe's units cost 172–173 µs against 161 µs, all user
  time, and a later SDK change removed it; report 049 traced an alternating cost to the allocator
  handing the same blocks back every other call. A gate that cannot see that reads the next change
  to the SDK's hot path as a loss of the mode.
- **Alternatives**: pinning the allocator for every probe (hides the effect, does not detect it);
  comparing only managed against unmanaged (the bias can sit in either).

## R72. When the refill's kept estimate gives way (item 055)

- **Decision**: keep the other principals' measured CPU usage beside the kept estimate; drop the
  estimate when it has risen by `principal_min_percent` of capacity; keep nothing while the list is
  unreadable.
- **Rationale**: report 052 derived that while a reset window refills, a rise smaller than the
  interval's bound can go unseen for up to 8 intervals beside a wide own bound, against SC-002's 2.
  Own measurement's looseness is not another user's usage, but another user's measured usage is
  not looseness.
- **Alternatives**: keeping the estimate only within one margin step (undoes T518 where bounds are
  wide); accepting one window (the user's SC-002 bound is two intervals).


## R73. How a locked node journal is waited for (item 056)

- **Decision**: a per-connection busy handler with jittered sleeps of 1 to 16 ms up to the
  connection's limit (5 s, an attempt pin 50 ms); a busy pin defers its report; a journal write
  still locked after 5 s holds the interval instead of ending the agent.
- **Rationale**: in B1 about 20 supervisors began their Released commits together and, under
  SQLite's default handler, retried in step at one commit per 100 ms; the agent's pin waited
  1.33 s and two releases ended as supervisor errors. In a local model of one lock with commits
  holding 1.5 to 3.5 ms, 20 writers that begin together finish in 81 ms against 1134 ms, and 150
  in 430 ms where the default times out. rusqlite's safe `busy_handler` passes only the count, so
  a per-connection limit needs the FFI call.
- **Alternatives**: the safe handler with two fixed limits (leaves other limits on SQLite's
  handler); a retry loop in Rust per write (misses statements a read can block on under the
  rollback-journal profiles); writing the observation before actuating (puts the journal's wait
  in front of every yield).

## R74. What `held_back` lists (item 056)

- **Decision**: only amounts withheld; the noise tolerance is not one.
- **Rationale**: the tolerance plus the device's unattributed memory is a growth threshold that
  nothing subtracts from the budget or from admission (report 054 Decision 1); others' use is
  already shown as `external` and `unattributed`.
- **Alternatives**: others' use as a new part (a wire change in three languages); the unattributed
  amount alone in the field (shown twice, and RAM has no such part).

## R75. The remainder when the over-share task leads the cost order (item 056)

- **Decision**: FR-016's cost order, as before item 051; the shape is pinned by a test.
- **Rationale**: the task with the least progress to lose is the cheapest to take down whoever
  caused the excess, and no in-share task pays any part of the excess either way.
- **Alternatives**: asking over-share tasks last for the remainder (the tasks with more progress
  to lose pay the margin).

## R76. One clock for an over-share hint, and its part asked once (item 056)

- **Decision**: count a held hint toward the excess on the clock its step escalates by, and never
  ask a task again for what it was asked and did not count.
- **Rationale**: the ladder counted from the hint's last change while the step escalated from the
  need's return, so after a pause the hint waited again but counted nothing and in-share tasks
  were stepped (seven of the 500 sequences, three of them stopping a task); since item 054 the
  uncounted part also entered the remainder and was asked of the same task again (`random::242`,
  1000 MiB asked twice).
- **Alternatives**: measuring the deadline from the breach's first observation (a renewed wait can
  pass SC-012's bound; needs a clarify); dropping the dwell term (differs only where a hint
  deadline is shorter than the dwell).

## R77. Checking the probe's parity inside the window (item 058)

- **Decision**: sum each unit's time by parity over the window itself, make the holds after a
  warm-up of the real loop, refuse on the in-window ratio, and state the CPU given apart.
- **Rationale**: at `7cc09c8` the pre-ready check read 1.001–1.002 while the managed probe's
  units alternated between 162 and 191 µs in the window (Neutral 0.929); a check outside the
  window cannot see a cost that starts with the loop. The probe already times each unit, so the
  sum costs one addition per unit.
- **Alternatives**: a longer check before `ready.json` (still outside the loop); a unit that
  allocates nothing (changes every recorded rate; its own item if the gate keeps refusing).

## R78. When the kept estimate may be kept (item 058)

- **Decision**: keep it only while the list, when taken and now, shows another account's process
  with a readable user ID.
- **Rationale**: a list that shows only the owner, or nothing, cannot show another user's rise,
  so a kept estimate there could hide one for up to a window (report 055 Decision 3).
- **Alternatives**: a field saying whether other accounts are hidden (the snapshot has none, and
  the item asks for policy only).

## R79. Who owns the busy handler's context (item 060)

- **Decision**: one `Journal` type owns the connection and the context, connection first; the
  context is a raw pointer from `Box::leak`, freed on drop.
- **Rationale**: on two open paths the context was a local declared after the connection, so an
  error return freed it first, and the context was a `Box` moved after SQLite took a pointer into
  it, which `std`'s documentation on `Box` (non-normative) does not allow. SQLite's close path does
  not call the handler, so nothing failed; the new shape does not depend on that.
- **Alternatives**: removing the handler before freeing (changes what close may do); keeping the
  `Box` and pinning it (still two owners to keep in order on every path).

## R80. The limit on intervals without a sample (item 060)

- **Decision**: one lease, `lifecycle.allocation_lease_ms`, against the newest sample acted on.
- **Rationale**: it is the only existing agent setting that fits: a sample's age at the next
  iteration can be the 5 s journal wait after an interval that ended at the observation write, so
  a shorter limit would stop renewals for an interval that did sample. Monitor and heartbeat
  intervals, the GPU sample age, the unknown window and the drain times were weighed and rejected
  (`060-b-design.md`).
- **Alternatives**: the heartbeat interval measured from the end of the last acting iteration; a
  fixed bound, which needs a new constant or key.

## R81. Guaranteed work keeps its renewals (item 061)

- **Decision**: withhold renewals after one lease without an acted-on sample for Opportunistic
  slots only.
- **Rationale**: a Guaranteed lease that runs out drains nothing, so withholding its renewal
  bounded nothing, while its renewal reply is the only path by which a coordinator drain, cancel
  or preemption reaches that work on a node that sends no heartbeat.
- **Alternatives**: withhold for every slot (item 060 as built; closes that path).

## R82. Work left is judged by holding in the modes that give way (item 062)

- **Decision**: in Yield First and Neutral, by the resource's governing mode, an opportunistic
  allocation that holds none of a memory resource does not count as work left before a
  Guaranteed allocation's last step; a missing reading counts by its charge.
- **Rationale**: FR-024 waits for opportunistic work "that anyone can act on"; a task that holds
  none of the resource frees nothing when released, so waiting for it kept a Guaranteed task
  over its share from the floor's last step until the other user left (`random::378`).
- **Alternatives**: every mode (drains a Guaranteed task in Performance in `random::242`; with
  the ladder decision); wait for that decision (the stall stays).

## R83. No renewal for an exited supervisor (item 062)

- **Decision**: the agent asks no renewal for a slot whose supervisor has exited.
- **Rationale**: every other slot path already leaves such a slot alone; the renewal kept a
  lease current for work nobody supervises and sent its reply, drains included, to a closed
  pipe. The capacity stays charged until the release either way.
- **Alternatives**: keep it and document it; also move the dead supervisor's record out of the
  interval (changes which write ends an interval; not taken).

## R84. Effects before records after EXEC (item 062)

- **Decision**: after EXEC a supervisor's records wait at most 50 ms for the journal's lock in a
  pass and are tried again every 250 ms; its drain notice and signals never wait for them. This
  replaces R3's journal row for records after EXEC; the launch barrier's records and Released
  keep R3's handling.
- **Rationale**: a drain takes authority away, so holding it for its own record failed in the
  unsafe direction: behind a locked journal the notice came about 20 s late and spent the
  worker's drain window, and a critical TERM waited behind two records.
- **Alternatives**: one 5 s try (still longer than the default 3 s window); keep the order and
  document it.

## R85. A drain in a renewal reply needs no time left (item 062)

- **Decision**: the agent hands a renewal reply's drain to the supervisor as a drain line, with or
  without time left, and the supervisor takes a renewal that says drain whatever time is left.
- **Rationale**: a drain grants nothing; the lease's time bounds only how long a grant keeps work
  running. The two sides computed the time left on two clocks, so a drain with a few milliseconds
  left could be dropped with no event.
- **Alternatives**: drop only the time test for a lease that says drain, on both sides (leaves an
  older supervisor's gap).

## R86. A memory hint that was complied with is renewed, not escalated (report 056 Decision 1 C)

- **Decision**: at a memory hint's deadline, a task whose reading is at or below the larger of the
  target the hint began with and the current target gets a new hint with the current target and a
  new deadline; any other task escalates as before.
- **Rationale**: the deadline counted time only, so a task that shrank to its target was drained
  when another own task grew during the wait. Over the 500 benchmark sequences 20 of 87 memory
  stops were such tasks. Plain Linux would stop none of them.
- **Alternatives**: keep the order as built (the user declined); return to the order before item
  051 (in-share tasks pay for another task's excess again, ruled out on 2026-11-03); ask the task
  that grew (needs a per-task baseline and redefines the victim order and the OOM mark).
