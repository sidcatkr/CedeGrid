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
