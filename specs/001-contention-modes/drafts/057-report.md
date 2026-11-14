# Item 057 Parts B and C: report

**Status**: the four drafts are written. Nothing is approved and nothing is implemented. The
constitution, the 001 spec and the code are untouched. Part C's recommendation for Full waits on
two measurements by the other session (E1 and E2 in the cap design), and every gate below still
needs a run on the test host.

## Summary

1. Performance stops giving anything back to running work on CPU and GPU compute. It holds back
   only at admission: a 2% margin, and no new own work where another principal contends.
2. Per-account fairness moves to Neutral. Yield First is unchanged.
3. Recommended for memory: at a floor breach others caused, Performance gives nothing back and
   the kernel decides, as it does without CedeGrid.
4. The constitution goes from 7.0.0 to 8.0.0 (MAJOR: Principle II is redefined). It shares one
   revision with feature 005's draft if both are approved together.
5. The cap, where one remains (Yield First, Neutral): in Full, `cpu.max` behind a fork gate, if
   the measurements bear it out. In Tracked and on the GPU, no rate cap; whole tasks are held
   instead.
6. Ten decisions are yours, each below with a recommendation.

## The designs in plain terms

### Part B: Performance gives nothing beyond what plain Linux gives

- **What stays.** CedeGrid still fills idle capacity. It still never starts new own work where
  another user is already short of their fair share, or into activity it cannot measure, or into
  the memory floors. Yield First and Neutral still give way to others, as today.
- **What goes, in Performance only.** It no longer slows, pauses or ends running own work so that
  someone else gets more CPU or GPU time than the kernel and the GPU driver would give them beside
  the same work started by hand. Today it does, and it cost own work the following (*measured*,
  test host, 2026-11-13): −3% to −11% CPU in the many-against-one shape, −27% with two GPU
  probes against one, and about 4 GiB of 9 in the OOM shape. One against one, there was no
  difference.
- **"Contending", exactly.** Another principal contends for a resource when the resource is
  saturated and that principal gets less than the lesser of its demand and its max-min fair share,
  or when activity there is of unknown amount. New own work is admitted only within
  `max(S, A − margin)`, where `S` is own work's fair share counting each contending principal at
  the lesser of its demand and its share. On a time-sliced GPU, one more own process is admitted
  only while `(m + 1) × k ≤ E`, or while no counted principal is slice-bound.
- **What it costs.** A user who arrives after own work is running gets what they would get beside
  the same work started by hand: in the two-probe GPU shape, 509–512 req/s instead of today's
  785–797 (*measured*). Own work that is queued may wait longer at admission than it would if
  started by hand.

The text change is in [057-amendment-draft.md](057-amendment-draft.md). Every FR, SC, term,
config key, contract, guarantee section, 003 and 004 requirement, code path and gate that changes
is in [057-spec-delta.md](057-spec-delta.md).

### Part C: how a cap is applied where one remains

After Part B, only Yield First and Neutral hold running own work back for someone else. Your rule
was: clearly more exact than stop-and-resume, no loss, and nothing restricted before it is
verified; otherwise no cap.

- **Full, CPU: `cpu.max` on the job's own leaf, behind a fork gate.** The kernel then holds own work
  at exactly the lesser of the quota and what it would get uncapped, so the cap itself never holds
  it below the target. A new process of the job is always born inside the job's cgroup, so it
  would be born under the cap before CedeGrid could verify it. The fork gate prevents that: each
  process creation waits until CedeGrid has lifted the cap, and the cap goes back only once the
  new process is verified. The gate costs the job the following: it cannot use set-user-ID helpers,
  it cannot install its own seccomp listener, a program that calls `clone3` directly with no
  fallback fails, and the job ends if its supervisor dies. Each process start also waits one round
  trip. A job that needs any of those opts out and gets whole-task holds instead.
- **Tracked, CPU: no rate cap.** Without a cgroup, nothing is clearly more exact than
  stop-and-resume and also free of loss. Own work is held back by whole tasks: a task is paused only
  while what keeps running stays at or above the target, so the rounding always favours own work.
- **GPU compute: no rate cap either.** No unprivileged per-process GPU quota exists, so the only
  "cap" is a stop-and-resume duty. Whole-task holds reach similar numbers by reasoning: pausing
  one of two own probes leaves the one-against-one shape, measured without CedeGrid at own 0.470
  and the other user 749–755 req/s. A run checks that before it is relied on.
- **What no cap means.** Neutral can then do nothing about one big task over its share, so the
  other user gets what plain Linux gives. Yield First still pauses every own task when it is
  contended.

Details, the ways rejected, and the runs are in [057-cap-design.md](057-cap-design.md).

## Decisions needed

Each lists who pays, in numbers where they exist. *Measured* numbers come from the brief's table
or `docs/validation.md`. The rest is reasoned.

### 1. Memory at a floor breach that others caused

- **A: keep today.** In every mode, own memory above its fair share gives way and its tasks are
  marked as the OOM killer's first victims. In F4 (*measured*), Performance sent TERM to the 3 GiB
  over-share task at +3.8 s and drained the next at +3.9 s, giving back about 4 GiB of 9. The
  other user's allocator was still killed, at about +37 s after reaching 31.2 GiB. In item 048
  C3, own RAM went from 12764 to 6172 MiB in 2.3 s. Cost: own work loses tasks the kernel would
  not have taken, and the other user is not saved.
- **B: Performance gives nothing back; the kernel decides** (recommended). No hint, drain, TERM or
  OOM mark for the breach (marks only under `ram.prefer_own_oom_victim`). Every own task gets the
  save-progress signal, and admission stays closed while the floor is breached. Plain Linux in F4
  (*measured*): no own process touched, and the kernel killed the other user's allocator at +52 s,
  at 20.5 GiB. Who can be killed: whatever the kernel's OOM score picks, usually the largest task,
  which can be an own task if it is the largest. Cost: own work carries the same risk as without
  CedeGrid. The other user carries the same risk as without CedeGrid.
- **C: the kernel decides, except own growth during the breach.** Own memory that grows while the
  floor is breached gives way and is marked, back to what it held when the breach began. Cost:
  own work loses only what it grew during the breach (reasoned). The rule is harder to explain
  and to test.
- Yield First and Neutral keep giving way under every option.
- **Why B:** it matches your rule that only harm CedeGrid adds counts. It also matches your OOM
  principle that own work must not die first for an OOM it did not cause. A did not save the other
  user in F4.

### 2. Activity of unknown amount, in Performance

- **A: admit nothing new there and hold every step** (recommended). Running own work is untouched,
  as without CedeGrid. Queued own work waits until the amount is known again. This is a graduated,
  bounded response under Principle III (block expansion, then hold).
- **B: keep today.** Running own work is held to its fair share under unknown activity. Cost: own
  work gives back to what may be nobody, which is more than plain Linux gives.
- **Why A:** B is a give-back to running work, which Part B removes.

### 3. Own usage above its own target (the margin, a burst after an over-admission)

- **A: the margin is a rule for admission only** (recommended). Running work is never capped
  back. Usage above the admission target closes admission until it is back within it. Cost to
  others: the kernel's split beside the burst, as without CedeGrid.
- **B: keep capping running work back to the target.** Cost: own work loses up to the 2% margin
  plus any burst, against plain Linux. The cap would then follow Part C's rules in Performance too.
- **Why A:** plain Linux caps nothing, and B is a loss by your rule.

### 4. The GPU time-slice duty

The brief fixes it: off in Performance, kept in Neutral. What follows, for you to confirm:

- **In Performance:**
  - no duty and no re-measure windows (`w = 0`), so every interval feeds the external estimate;
  - the context counts and slice-bound readings are kept for the record and for admission;
  - a second own process waits at admission while `(m + 1) × k > E` and a counted principal is
    slice-bound (new reason code `gpu_time_slice_admission_held`).
- **In Neutral:** d0' stays 643 at `m = 2, E_b = 1`. The Performance figures 692 and 725 are
  dropped.
- **What it costs, two own probes started before a saturating user** (*measured*): own work goes
  back to 0.660 of the pair alone, from 0.475–0.486. The other user goes back to 509–512 req/s,
  from 785–797.
- **What it costs, two own probes submitted while that user runs:** the second waits (gate F3c).
- Decision C3 below may replace Neutral's duty with whole-task holds.

### 5. What Aggressive (feature 004) is

After Part B, Performance already never gives compute back, so today's unprotected clause ("declines
to give compute back") no longer adds anything.

- **A: no unprotected form.** Aggressive is Performance with a 0% admission margin that fills at
  once. 004's FR-010 to FR-016 and the unprotected tasks go. Cost: nothing to others beyond the
  baseline. You lose the "take from light users" option you asked to keep behind a flag.
- **B: keep an unprotected form, redefined as admission** (recommended). Protected is as A.
  Unprotected may also start new own work where another principal contends, never into unknown
  activity or the floors. It is turned on only by an explicit per-run flag after a two-step
  confirmation, and it is reported while active. Cost to the contending user, the same as starting
  the work by hand (*measured* without CedeGrid): on the GPU, from 749–755 req/s beside one own
  probe to 509–512 beside two.
- **C: withdraw 004.** `reduced_margin` and the ramp settings cover what is left.
- **Why B:** it keeps your earlier instruction (plan B behind a flag and a double confirmation),
  and its only risk is the one the constitution already names: slower compute, opt-in only.

### 6. The baseline itself

CedeGrid fills idle capacity, so it may run more own processes than you would start by hand. A
user who arrives later then shares with all of them.

- **On a host that splits CPU by thread, as the shared host does** (reasoned from its layout,
  nothing was run there): a later user with 2 threads beside 8 own processes on 4 CPUs gets about
  0.8 CPU. Beside 2 own processes they would get about 2.
- **On a time-sliced GPU** (*measured* without CedeGrid): 509–512 req/s beside two own probes,
  749–755 beside one.
- **On the test host,** which splits CPU by account first, the effect is smaller. F2's plain runs
  still varied with where the kernel placed own tasks, which the other session is measuring.

Options:

- **A: the baseline is the same own work CedeGrid runs, however many processes that is**
  (recommended, if you accept the numbers above knowingly). Cost: later users on such hosts get a
  per-thread or per-context share.
- **B: a narrower baseline on hosts that split by thread or context.** CedeGrid starts no more own
  processes there than keep a later arrival at its fair share by count. Cost: own work leaves
  capacity idle on such hosts even when nobody else is there, against your goal of no idle
  capacity.
- **Why A:** B gives up idle capacity at all times to protect a user who may never come. Neutral
  remains the mode for hosts where that matters.

### 7. A Performance job on a node set to Neutral

Today the node's mode sets own work's total, and each job's mode only orders who yields inside it
(`governing()` in `src/policy.rs`).

- **A: keep that** (recommended). On a Neutral node, Yield First and Neutral jobs are held first,
  and a Performance job is held last, only if the others are not enough. Cost: the Performance job
  can lose against plain Linux on a node you set to Neutral.
- **B: a Performance job is never held, whatever the node's mode.** Cost: the node's Neutral
  promise is kept only as far as the other jobs can give, so the other user can get less than
  Neutral's fair share.
- **Why A:** the node's mode is your setting for the whole node, and `ps` shows the job being
  held.

### C1. Full, CPU cap

- **A: `cpu.max` behind the fork gate** (recommended, if E1 shows it clearly closer to its target
  per interval than stop-and-resume, and E2 shows the gate's cost per process start is small).
  Own work never goes below its target. Cost to own work: the gate's costs above, for jobs in Yield
  First or Neutral only; a job that needs set-user-ID helpers or its own seccomp listener opts out.
  Cost to others: none against plain Linux.
- **B: `cpu.max` lifted on sight, no gate.** Every new process of the job is under the cap for up
  to one 50 ms check. That fails "verify, then put it in", though the window is bounded.
- **C: no rate cap in Full; whole-task holds.** Cost: Neutral cannot hold one big task to its share.
- If A's conditions fail, C applies.

### C2. Tracked, CPU cap

- **A: no rate cap; whole-task holds** (recommended).
- **B: keep today's stop-and-resume duty.** It fails your yardstick by definition. It can hold own
  work below its target, and it stops every request in flight for part of each period.
- **C: CPU affinity plus the fork gate.** It changes in whole cores, so a fractional target is
  missed: rounded down, own work loses, and rounded up, it gives back less. Other users' threads
  can still use own work's cores.
- Feature 003's Tracked affinity lever (FR-043, FR-051, FR-056, FR-057) is a cap under this rule.
  Under A it applies to Yield First only, or goes, which the 003 revision decides.

### C3. GPU compute cap in Neutral and Yield First

- **A: whole-task holds** (recommended, if run E9 shows the other user at or above Neutral's
  bound). Reasoned from the one-probe row: own about 0.47, the other user about 749–755 req/s.
- **B: keep the aligned duty, as an exception you approve.** *Measured* (item 053, Tracked): own
  0.447 against its bound 0.42, the other user 840.0–840.4 against its bound 753.4.
- **C: leave the GPU to the driver in Neutral.** *Measured* without CedeGrid: own 0.660, the other
  user 509–512 req/s, below Neutral's bound.

## What waits on the other session

- **Its first measurement** (why plain Linux lands in two places in F2) decides how gate F2
  compares runs: like placement with like, or medians of at least 10 runs (spec delta, section 10).
- **Its second measurement** (per interval, own usage against the target under stop-and-resume
  and `cpu.max`) is run E1. It decides C1.
- **Runs E2 to E9** (cap design) and **gates F1 to F4, SC-018 and SC-019** (spec delta) are listed
  for it. No result is estimated here.

## What I could not check

- Nothing was run on a host. Every gate and run above is unrun.
- These kernel behaviours are taken from the manuals and reasoning, not tried; E3 to E5 check
  them:
  - the account can make its own `cgroup.procs` read-only and the kernel then refuses its writes;
  - a mode change on that file can be watched;
  - a second seccomp listener gets `EBUSY`;
  - glibc falls back from `clone3` to `clone` on `ENOSYS`.
- No build or test was run, since no code changed.
- The F2 fair share used in the reasoning is not stated in the brief. The cap design does not rely
  on it.

## Commits

On branch `specs-057-059`, cut from `001-contention-modes` at `6c40d74`:

- `f07d6a0`: the amendment draft, the spec delta and the cap design;
- this report's commit, which follows it.

## Pointers

- [057-amendment-draft.md](057-amendment-draft.md): the constitution diff, 7.0.0 → 8.0.0, with
  variants per open point.
- [057-spec-delta.md](057-spec-delta.md): held-back table before and after, terms, FRs, SCs, config,
  contracts, guarantees, 003 and 004, code paths by function, tests and gates, principle check.
- [057-cap-design.md](057-cap-design.md): the requirement as three tests, the seven ways weighed,
  what each mode does without a cap, runs E1 to E9, code and review list.
- Feature 005's Principle V draft: `specs/005-system-mode/drafts/amendment-principle-v.md` (Task 2).
