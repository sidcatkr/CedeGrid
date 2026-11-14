# Draft: how a cap is applied after Part B (item 057 Part C)

**Status**: design note, draft. Nothing here is implemented or approved. Two measurements by the
other session decide parts of it, and the gates below that need a host are listed for that
session, with no result estimated here.

Figures marked *measured* come from the brief's table of 2026-11-13 or from `docs/validation.md`.
Everything else is reasoned.

## The answer in plain terms

After Part B, only Yield First and Neutral hold running own work back for another user's sake
(and Performance too, if open point 3 keeps a cap on own usage above its target). For those caps:

- **With a delegated cgroup (Full), on CPU: `cpu.max` on the job's own leaf, behind a fork gate.**
  The kernel holds own work at the quota each period, so own work gets exactly the lesser of the
  quota and what it would get uncapped. The fork gate makes the job wait at each process creation
  until CedeGrid has lifted the cap; the cap goes back only once the new process is verified. So no
  child of the job is ever born under a cap, and nothing outside the job shares the limit. This is
  recommended only if the other session's per-interval measurement shows `cpu.max` clearly closer
  to its target than stop-and-resume, and the gate's cost per process start is small.
- **Without one (Tracked), on CPU: no rate cap.** Nothing available without privilege is clearly
  more exact than stop-and-resume and also free of loss. Yield First and Neutral then hold own work
  back by whole tasks only: admission, and a whole task paused only while what keeps running stays
  at or above own work's target.
- **GPU compute, at both levels: no rate cap either; whole tasks only.** No driver offers an
  unprivileged per-process quota on a time-sliced GPU, so the only cap is a stop-and-resume duty.
  Whole-task holds look close to the duty's numbers in the two-against-one shape, by reasoning from
  the measured one-probe row. If the run shows otherwise, keeping the measured duty in Neutral and
  Yield First is the user's call, as an exception to their rule.
- **If neither condition for `cpu.max` holds, there is no rate cap at either level.** The section
  "What each mode does without a cap" says what follows.

## The requirement, as three tests

| Test | What it asks | From |
|---|---|---|
| R1 exact | Own usage per interval is clearly closer to its target than under the stop-and-resume duty. The other session's second measurement is the yardstick | 23:42Z |
| R2 no loss | Own work is never held below its target, and its limit is never shared with a process that is not part of the job | 23:42Z |
| R3 verify, then put it in | No process is under a restriction before CedeGrid has verified it | 21:01Z, 21:02Z |

Plus the fixed constraints: no privilege (Principle V), the glibc 2.35 / kernel 5.15 floor with a
run-time fallback, and a security review of every file that faces the kernel before release.

**What "verified" is today** (`src/supervision/family.rs`): a descendant joins the run's family
only after a pidfd is open on it, its parent chain leads to the leader or a member, it is alive
through the pidfd, and its real, effective and saved IDs are the run's. Discovery runs once per
duty period, which is `monitor.interval_ms` (500 ms by default).

**The newborn problem.** A child is in its parent's cgroup from birth. It also inherits its
parent's CPU affinity, scheduling class and nice value. So any restriction that sits on a cgroup or
on a thread reaches a new child before CedeGrid can verify it, unless the child's birth is ordered
after a lift. Only a mechanism that reaches processes one at a time through their pidfds escapes
this by construction. The stop-and-resume duty is one: a stopped process cannot fork, and a child
born while its parent runs is not stopped until it is verified.

**Today, in Full** (`src/cgroup.rs`, `docs/guarantees.md`): a restriction is written only while the
leaf's members are exactly the verified family. A freeze thaws within 50 ms of a join (*measured*,
49.8 to 50.1 ms, items 051 and 053). A `cpu.max`, `cpu.weight` or `memory.high` cap stays on a leaf
a process joined until the step ends. A job's own new child is born under a held `cpu.max` and is
verified within one duty period. That second case is what the user called a loss on 2026-11-13
(21:01Z).

## Why stop-and-resume misses its target (the yardstick)

A duty `d` over a period lets own work run for `d` of each period. While it runs, own work gets
whatever rate `r` the kernel gives it beside the other user. So own usage is `d × r`, and the duty
is set from the rate measured in the last interval (`RootlessActuator`: `duty = target / rate`).
When `r` changes, because the other user's load changes or because own work was held in the
previous interval, own usage lands off target until the next correction, below it as often as above
it. That can hold own work below its target (fails R2). It also adds a stop of up to `(1 − d)` of a
period to every own request in flight.

*Measured* in F2: Tracked made 44 to 54 cap changes in 60 s, and Full (`cpu.max`) made 22 to 36. That
shows how often each corrected. It does not show how close each stayed to the target per interval,
which is the other session's measurement.

## The ways weighed

### 1. `cpu.max` on the leaf, lifted on sight of a newcomer, written back once it is verified

- **How exact (reasoned).** The kernel's bandwidth control lets the leaf run until its quota for
  the period is used, then throttles it to the end of the period. Own work gets
  `min(what it would get uncapped, quota)` each period, so the cap alone never holds it below the
  quota. It overshoots by at most the per-CPU runtime slices the kernel hands out (5 ms by default,
  `sched_cfs_bandwidth_slice_us`). Under contention it is kernel-enforced and fractional, with no
  correction loop. Where a node's target is split across several own leaves, a leaf that uses less
  than its quota strands it for up to one interval until the agent re-splits from measured use; the
  plan bounds that. The other session's measurement (E1) decides R1.
- **Window.** Every child the job forks is born under the cap and stays under it until the next
  membership check sees it and lifts the cap. A process of the same account that writes itself
  into the leaf is in the same position. Checking the leaf's members every 50 ms while a cap holds,
  as the freeze does now, bounds the window at one check plus the lift write. No other account can
  enter: the leaf belongs to the user's account. **Fails R3** (bounded, not none).
- **Cost against plain Linux.** Own work: none beyond the target in Yield First and Neutral, which
  may cost own work. A job that forks often keeps lifting the cap, so it gives back less than the
  target; that is no loss to anyone against plain Linux. Another user: never less than plain Linux,
  since the cap only gives back. *Measured* in F2 (Performance today, which held the fair share):
  the other user's p99 in Full was 0.52 and 3.33 ms against plain's 3.33 ms.
- **Needs.** The `cpu` controller delegated (Full). Present on 5.15. No privilege. Code: a
  membership check in the actuator's watch loop for every held control, not only the freeze
  (`src/cgroup.rs`).

### 2. A capped child cgroup that holds only processes CedeGrid moved in after verifying them

- **How exact.** As `cpu.max` (1), on the capped group.
- **Window.** cgroup v2 lets processes sit only in leaves once a controller is enabled below a
  group, so the job needs an open group and a capped group side by side. A child of a process in
  the capped group is born in the capped group, under the cap, before it is verified. Moving it
  out first would act on a process CedeGrid has not verified, which Principle I forbids. So the
  newborn window is the same as in (1). A process of the same account can still write itself into
  the capped group. **Fails R3** as (1) does.
- **Cost.** As (1), plus a write to `cgroup.procs` for every move in, which is the operation the
  launch barrier and the ownership checks guard most closely.
- **Needs.** As (1), plus a second level of groups in the delegated subtree.
- **Verdict.** No gain over (1). Rejected.

### 3. Gating each process creation until CedeGrid has lifted its restrictions (seccomp user notification)

This is not a cap. It is what makes (1) meet R3: it orders every birth after a lift.

- **How it works.** After the launch barrier persists Authorized and before the job's program runs,
  the worker gate sets `no_new_privs` and installs a seccomp filter with a notification listener,
  which it hands to the supervisor. The filter sends a notification for `fork`, `vfork` and `clone`
  without `CLONE_THREAD`. It lets `clone` with `CLONE_THREAD` through (a new thread belongs to an
  already verified process). It answers `clone3` with `ENOSYS`, because a filter cannot read
  `clone3`'s flags; glibc then falls back to `clone`. It kills a call made in another system call
  ABI. On a notification:
  1. The supervisor checks that the notification is still valid.
  2. If it holds no restriction on the leaf, it lets the call continue at once.
  3. Otherwise it returns every restriction it wrote on the leaf (`cpu.max`, `memory.high`) to its
     launch value, reads each one back, and only then lets the call continue. A frozen leaf
     cannot fork, so a freeze never meets a notification.
  4. It runs discovery at once and verifies the child as today.
  5. Once the leaf's members are exactly the verified family again, it writes the restriction back.
     Another birth cannot slip in between that check and that write, because each birth waits for
     the supervisor. That also closes the check-then-write race item 053 left (SUP-11) for the
     job's own children.
- **Window.** None for the job's own children. A process of the same account that joins the leaf
  by writing to its `cgroup.procs` is not gated. After the leader is placed, the leaf's
  `cgroup.procs` is made read-only, so such a process must first make it writable again. CedeGrid
  watches for that mode change and for any member it has not verified, and lifts the cap on either,
  within one 50 ms check. So the remaining window is at most one check plus the lift write, and
  only a process of the user's own account that changes the file's mode and writes itself in can be
  in it. Without root, a window of zero for that case cannot be had: the account owns its own
  leaf.
- **Cost to own work against plain Linux** (each reasoned; E2 to E6 measure them):
  - Every process creation waits for one round trip to the supervisor, and for a lift and readback
    while a cap holds.
  - `no_new_privs` stops the job from gaining privileges through set-user-ID programs or file
    capabilities. A job that needs one (for example to mount a FUSE file system or to map user IDs
    for rootless containers) fails and must opt out.
  - A program that calls `clone3` itself, with no fallback to `clone`, fails.
  - A job cannot install a seccomp listener of its own: the seccomp manual gives `EBUSY` once one is
    installed. Such a job must opt out.
  - If the supervisor dies, the listener closes and every later process creation in the job fails
    with `ENOSYS`. A gated job therefore ends with its supervisor (parent-death `SIGKILL`) and is
    reported and requeued where it can be. Today a job may outlive its supervisor in some setups.
  - A job that forks often keeps the cap lifted much of the time and gives back less than its
    target. That is no loss to anyone against plain Linux.
- **Cost to another user.** None against plain Linux.
- **Needs.** Seccomp user notification (Linux 5.0) with "continue" (Linux 5.5), both on the 5.15
  floor. Where present (Linux 5.19), the wait that ignores non-fatal signals, so a program that
  signals itself often does not restart its fork many times. No privilege: `no_new_privs` is what
  lets an unprivileged process install a filter. Fallback: where the filter cannot be installed
  (seccomp missing from the kernel or refused by a container), the job runs ungated and gets no
  rate cap (the whole-task holds below); `doctor` says so. No new dependency is planned (the filter
  is a short fixed program); the plan confirms that. This adds the most kernel-facing code of any
  option, and that code goes to the security review.
- **Which jobs get it (my pick, changeable).** Only jobs whose own mode is Yield First or Neutral.
  A Performance job then pays nothing; if a Neutral job later governs the node, the Performance job
  has no gate and is held by whole tasks only. A gated job that later runs under Performance keeps
  the gate, which then only answers "continue"; its measured overhead (E2) is own overhead under
  the amendment's loss rule.
- **What the gate is not.** It is not a security boundary. CedeGrid's checks stay as they are, and
  a job that never forks never sees it. It only orders a child's birth after CedeGrid's writes.

### 4. CPU affinity, or a cpuset, on verified threads

- **How exact (reasoned).** Affinity limits own work to whole CPUs (003 changes it in whole
  physical cores, FR-043), so a fractional target is rounded: down holds own work below its target
  (fails R2), up gives back less. Other users' threads may still run on own work's CPUs, since
  CedeGrid cannot pin them, so own work can get less than the CPUs it keeps. *Measured* reference in
  F2: own work pinned to the two CPUs the other user did not use got 1999 m, and the other user got
  6137 req/s at p99 0.34 ms. That is one case where pinning met a whole-CPU target.
- **Window.** A child inherits its parent's affinity at birth, so it is under the restriction until
  it is verified (one duty period, 500 ms by default). With the gate (3) the parent's mask is widened
  before the birth and narrowed again for both after the child is verified, so the window is none.
  Affinity is per thread, so nothing outside the job ever shares it. A cpuset sits on the cgroup,
  so a cpuset has (1)'s window and needs the `cpuset` controller delegated, which neither host's
  default is known to do.
- **Cost.** As above; with the gate, the gate's costs too.
- **Needs.** `sched_setaffinity` on own threads under 003's thread-ID rule (FR-043). No privilege.
  It works in Tracked.
- **Verdict.** Does not meet R1 or R2 for fractional targets. Kept as Tracked option C.

### 5. A weight (`cpu.weight`, nice, the idle class)

- **How exact.** Not a cap. A weight splits CPU only between siblings and gives way when they are
  idle; it has no target. In Full, the leaf's siblings are own work's other leaves, never another
  account's work: on the test host the kernel splits CPU by account above them (brief), and on the
  shared host there is no delegated subtree. So a lower `cpu.weight` gives another user nothing.
  In Tracked, nice and the idle class reach other users only where everyone shares one CPU group,
  as on the shared host. An unprivileged user cannot lower nice again, and 003 FR-043 forbids it.
  All three are inherited at birth.
- **Verdict.** Rejected as a cap. Weight between sibling groups is what feature 005 uses
  (`cpu.idle` on `cedegrid-idle.slice`).

### 6. A finer or aligned stop-and-resume cycle

- **How exact.** Still `d × r`. A shorter period shortens each stop, so a request in flight waits
  less, and corrections can come more often. It does not remove the error when `r` changes between
  corrections. Aligning all own processes to one phase (as the GPU duty does, item 044) matters for
  GPU slices, not for CPU accuracy. This is the yardstick itself, so it cannot be clearly more exact
  than itself (fails R1 by definition). E1 shows how far it misses.
- **Window.** None: signals go through pidfds to verified members only.
- **Cost.** Own work can be held below its target (fails R2), with stop latency on every request.
  Another user: never less than plain Linux.
- **Needs.** Nothing new. It is today's Tracked mechanism.
- **Verdict.** Kept as Tracked option B, for the user to keep if they choose.

### 7. Others found and rejected

- **Following forks with `ptrace`.** A traced child starts stopped, so CedeGrid could verify it
  first. But the kernel stopping it on CedeGrid's behalf pauses a process CedeGrid has not
  verified (Principle I). It also blocks debuggers and puts every signal through the supervisor.
- **`pids.max` at the current count while a cap holds.** No child is born under the cap, because no
  child is born: the job's fork fails with `EAGAIN`. That breaks the job (fails R2).
- **`CLONE_INTO_CGROUP`.** Places a child in another group at birth, but only the caller of
  `clone3` can ask for it. The job's program is the caller, not CedeGrid.
- **One `cpu.max` over all own leaves.** Exact for own work's total, but each job would then share
  its limit with other jobs (fails R2's second half).
- **`cpu.max.burst`.** Lets a leaf carry unused quota into the next period. It changes how the
  limit is spent, not who is under it. Not needed.

## Summary of the ways

| Way | R1 exact (reasoned) | R2 no loss | R3 window | Level | Kernel |
|---|---|---|---|---|---|
| 1 `cpu.max`, lift on sight | per period, kernel-enforced | yes | ≤ one 50 ms check, for every new child and a same-account joiner | Full | any supported |
| 1 + 3 `cpu.max` behind the fork gate | as 1 | yes; gate costs (NNP, `clone3`, ends with its supervisor) | none for the job's children; ≤ one 50 ms check for a same-account process that changes the file's mode and joins | Full | 5.5+ (floor 5.15), 5.19+ better |
| 2 capped child group | as 1 | yes | as 1 | Full | any |
| 4 affinity (+ 3) | whole CPUs; shares CPUs with others | no for fractional targets | ≤ one duty period; none with 3 | both | any |
| 5 weight | not a cap | — | inherited at birth | both | any |
| 6 stop-and-resume (today) | the yardstick | no, can hold below target | none | both | any |
| no rate cap, whole tasks only | exact per task, rounded toward own work | yes | none (pidfds, verified members; a freeze as today) | both | any |

## What each mode does without a cap

"Whole-task hold": a whole own task is paused, in victim order, only while what keeps running
stays at or above own work's target, so rounding always favours own work. After `pause_bound_s`
(10 s by default) a paused task drains and is requeued, as today. A pause reaches verified members
only: through pidfds in Tracked, and through a freeze in Full, written only to a leaf of exactly the
verified family and thawed within 50 ms of a join.

| Mode | CPU | GPU compute | Memory |
|---|---|---|---|
| Performance, Aggressive | No running step (Part B). Admission as Part B | As CPU | Open point 1 |
| Neutral | Admission at the fair share. Running work over the target: whole-task hold. One task over the target that cannot be paused without going below it is left alone, so the other user gets what plain Linux gives | Whole-task hold. A single multi-process job with several contexts is left alone (plain Linux) | Unchanged in this note. In Full, `memory.high` follows the same gate rule (3) |
| Yield First | Contended with no probe: target 0, so every own task pauses (exact). Otherwise whole-task hold at the Yield First target | Contended: every own task pauses. Otherwise whole-task hold | Unchanged |

What that costs, by shape (reasoned unless marked):

- **F2, eight own tasks against the other user wanting two CPUs.** Whole-task hold can reach the
  fair share to within one task's share, since there are many tasks. The other user lands between
  plain Linux (*measured*: 4911–4924 req/s in 3 of 5 runs) and what a fair-share cap gave
  (*measured*, Performance today: Tracked 5006–5101, Full 5412–5617 req/s). E8 measures where.
- **One big multi-threaded own task over its target.** Nothing is held, so Neutral gives the other
  user what plain Linux gives. On the shared host, where CPU is split by thread, that is in
  proportion to thread counts.
- **F3, two own probes against a saturating user.** Pausing one probe leaves one against one.
  *Measured* for that shape without CedeGrid: own 0.470 of the probe alone, the other user 749–755
  req/s. With the duty in Neutral (*measured*, item 053, Tracked): own 0.447 against its bound
  0.42, the other user 840.0–840.4 against its bound 753.4, with the duty at 0.672–0.673. So
  whole-task hold would give own work more and the other user less, and the other user would sit at
  its bound. Whether a paused probe really leaves the slices to the busy contexts is reasoned from
  the time-slice rule; E9 runs it.

## Recommendation

1. **Full, CPU (decision C1): `cpu.max` behind the fork gate (1 + 3)**, on two conditions the other
   session's runs decide: E1 shows `cpu.max` clearly closer to its target per interval than
   stop-and-resume, and E2 shows the gate's cost per process start small. If either fails, no rate
   cap in Full; whole-task hold. `memory.high` and the freeze follow the same gate rule.
2. **Tracked, CPU (decision C2): no rate cap; whole-task hold.** Option B (keep stop-and-resume)
   fails R1 and R2 by the user's own yardstick. Option C (affinity plus the gate) fails R1 and R2 for
   fractional targets.
3. **GPU compute, both levels (decision C3): whole-task hold**, if E9 shows the other user at or
   above Neutral's bound. If not, the user chooses between keeping the measured aligned duty as an
   approved exception and leaving the GPU to the driver.
4. A cap that open point 3 keeps in Performance follows the same rules.

## Runs for the other session

Each at least 3 times each way unless said otherwise, on the test host, with the spread reported.
Nothing here estimates a result.

| Run | What it measures | Shape and record |
|---|---|---|
| E1 (the brief's second measurement) | R1: per interval, own usage against its target, under stop-and-resume and under `cpu.max` | F2's shape in Neutral, Tracked and Full; per 500 ms interval, `(usage − target) ÷ target`: median, 5th and 95th percentile, and the share of intervals more than 2% below target; also a case with two own leaves splitting one target |
| E2 | The gate's cost per process start | A loop of 10,000 `fork` + `exit`, and a shell job that starts 10,000 short processes: wall time ungated, gated with no cap held, gated with a cap held; per-start latency percentiles |
| E3 | The newborn window without and with the gate | A cap held; each child reads its leaf's `cpu.max` and the time at its first instruction, the supervisor logs the lift. Ungated: time under the cap per child. Gated: every child must read the launch value |
| E4 | A same-account joiner | Make the leaf's `cgroup.procs` read-only; check that a write from another process of the account fails; then change the mode, write, and measure the time to the lift |
| E5 | Compatibility | Under the gate: a set-user-ID helper fails as documented and passes with the opt-out; a glibc program's `posix_spawn`, `fork` and thread creation work; a program calling `clone3` without a fallback fails as documented; a program that installs its own listener gets `EBUSY` |
| E6 | Supervisor death | Kill the supervisor of a gated job: the job ends and is reported; no process of the account outside the job is touched |
| E7 | `cpu.max` when own work wants less than the quota | One own task below its quota beside the other user: own usage equals its uncapped usage within the spread |
| E8 | Whole-task hold on CPU | F2's shape in Neutral, Tracked and Full: own at or above its target in every interval; the other user's rate against plain Linux |
| E9 | Whole-task hold on GPU | F3's two-probe shape in Neutral: one probe paused; own against 0.42 and the other user against 753.4 |

E3 and E4 also check two kernel facts this note assumes: that the account can make its own
`cgroup.procs` read-only and that the kernel then refuses a same-account write, and that a mode change
on that file can be watched.

## What this adds to the code (for the plan, no code written)

- `src/supervision/unix.rs`: the worker gate sets `no_new_privs`, installs the filter after the
  barrier and before the job's program runs, hands the listener to the supervisor, and sets the
  parent-death `SIGKILL` for a gated job. A per-job opt-out in the job's JSON.
- A new module under `src/supervision/` for the filter and the notification loop: a dedicated
  thread, a bounded queue so a job that forks without pause cannot stall the drain or terminate
  path, validity checks on every notification, and the system call ABI check. It goes into
  `ALLOWLIST` in `tools/pack_source_review.py` and `include` in `Cargo.toml`.
- `src/cgroup.rs`: lift and read back before a birth continues; write back once the members are
  verified; the read-only `cgroup.procs`; lift on a mode change or an unknown member, for every
  held control, not only the freeze.
- `src/supervision/family.rs`: discovery on a notification, not only once per duty period.
- `src/kernel.rs`: detect seccomp listener support at run time, with the fallback.
- `src/contention.rs` and `src/policy/ladder.rs`: the Cap step only where the level and the job's
  gate allow it; whole-task hold with rounding toward own work.
- `src/explain.rs`: `doctor` and `observe` say whether a job is gated and which hold applies.

**Security review before release:** the new gate module, `src/supervision/unix.rs`,
`src/cgroup.rs` and `src/supervision/family.rs`.

## Check against the principles

- **I (ownership).** No new act on a process CedeGrid has not verified: the gate answers the job's
  own system calls and lifts only what CedeGrid wrote; a pause reaches verified members only. The
  joiner window is a lift, not an act on the joiner.
- **II (own work first, loss by mode).** Yield First and Neutral may cost own work, but R2 keeps
  them at or above their targets. Performance takes no running step (Part B).
- **III (unknown is not idle).** Unchanged. An unreadable member list still counts as changed.
- **IV (launch barrier).** The filter goes in after Authorized is persisted and before the job's
  program runs; nothing runs earlier.
- **V (no privilege).** `no_new_privs` and a seccomp filter need none.
- **VI (tests first).** E1 to E9 and unit tests for the filter program, the notification order and
  the rounding come first.
- **VII and VIII.** Each `doctor` line and reason code is one sentence; the gate is detected at run
  time with a fallback, as the repository's contributor notes ask of new kernel features.
