# Item 059, feature 005 System mode: report

**Status**: specified, clarified, planned, broken into tasks and cross-checked, through Spec Kit.
Nothing is approved and nothing is built. No product code, constitution, existing spec or doc was
changed. Every figure here is *reasoned*; none was measured, because no host was reachable from
this session. Implementation waits on your approval of the amendment (D4) and your choice on N5
and N6.

## Summary

1. An administrator installs `cedegrid-system`. Members of the `cedegrid` group then get the Full
   level, even on hosts that give users no delegated cgroup.
2. One small broker runs as root. It only starts and stops the caller's two units, moves the
   caller's own job between them, and writes OOM scores for processes inside them.
3. Yield First runs in a root-level idle slice, so it gives way to other users within
   milliseconds, through the kernel.
4. An OOM that someone else caused should no longer take your in-share work first.
5. The default install, without the package, stays exactly as today.
6. The constitution goes from 7.0.0 to 8.0.0 (MAJOR: Principle V allows one root process). It
   shares one revision with item 057's draft if both are approved together.
7. Two conflicts the brief did not foresee need your choice: Principle II's OOM sentence (N5) and
   Principle I's verified set (N6).
8. The work is 73 tasks. The first usable step (the MVP) is tasks T001 to T026.
9. Six root steps on the test host are yours to run; the work session never uses `sudo`.

## The design in plain terms

**What you do.** You run `cedegrid system start` (this feature adds it, because 003's
`cedegrid run` is not built yet). The client asks the broker over `/run/cedegrid/broker.sock`,
which only the `cedegrid` group can open. The broker reads who you are from the socket itself,
never from your message. It has systemd start two units for you, `cedegrid-u<uid>.service` and
`cedegrid-idle-u<uid>.service`. Both run as you, and each has its own cgroup handed to you. Inside
them, the CedeGrid agent you have today runs on the Full path, with your rights. When `doctor`
says "System", you are on this path. Without the package, or when the broker does not answer
within 2 s (provisional), CedeGrid runs as today and says so in one line. With
`--require-system`, it refuses instead.

**What root does, and only this.**
- It starts and stops the units it made for the caller, built from the caller's user ID and the
  administrator's policy. Each unit runs a fixed CedeGrid command at a root-owned path, so no
  input of yours ever runs as root.
- It moves one of your running jobs between your two units when its mode changes between Yield
  First and another mode. It freezes the job, checks each process, moves them, reads back where
  they landed, and thaws the job.
- It writes OOM scores for processes inside those units, from memory counters it reads itself.

A request that names a user ID, unit, path, cgroup, mount, device, capability or program is
refused whole and logged. Every request and every score write goes to the journal.

**Why Yield First gets better.** `cedegrid_idle.slice` sits next to the other users' groups at the
top of the cgroup tree, with `cpu.idle` set. The kernel then runs it only when its neighbours
leave a CPU free, about 3/1027 of each contended CPU (reasoned from the kernel's weights). So the
15% CPU margin Yield First keeps today may go down toward 0%. A run decides how far: `cpu.idle`
does not reach the other hardware thread of a core, caches or memory bandwidth (host run R3-run).

**OOM.** The broker works out each user's fair share of RAM, as 001 does. It lowers the score of
the part of your memory within your share, never below −999, so the kernel's choice falls on
memory above someone's share first. Under D2 it marks your over-share tasks at 1000. A score the
broker writes also sets the process's floor, so you cannot lower it further yourself. Processes
outside the units are never touched. One limit remains, and the docs say it: if a small process
outside CedeGrid causes the OOM, the kernel still kills a bigger one.

**What it changes on each host, against plain Linux** (reasoned, research R2). On the test host,
CPU is split by account. A root-level `cedegrid.slice` would give your work more than plain Linux
as soon as two or more other accounts are busy: with four others busy, about 1/2 instead of 1/5.
So the plan recommends keeping your ordinary unit in your own `user-<uid>.slice` there (N4), if
it survives your last logout. On the shared host no placement reproduces plain Linux's per-thread
split, and that host is outside this feature's validation until you say so.

**Install and removal.** Two packages, `cedegrid` and `cedegrid-system`, as `.deb` files in the
releases and from the project's own apt repository. Installing adds the broker's service and
socket, the two slices, the group and an empty `/etc/cedegrid/`, and changes no PAM, login, SSH,
sysctl or kernel module setting. A purge stops every unit the broker made and leaves the host as
it was. Tests that need root run first in a CI container with systemd as PID 1.

## What needs your approval

### 1. The amendment text

[amendment-principle-v.md](amendment-principle-v.md) has the diff:
- **D4**: Principle V is redefined. CedeGrid stays rootless by default; one root broker is allowed
  in System mode, with the rules above written into the principle.
- **N5**: one clause in Principle II.
- **N6**: one bullet in Principle I.
- The hidden-dispatch constraint gets two more names.

Your approval lets implementation start. The text lands in the constitution with the
implementation's pull request (T069), with `docs/guarantees.md` and the tests, as Governance
requires.

### 2. Every root command on the test host (research R13)

You run these yourself; `<you>` is your account, `<second>` the second account, and `<v>` the
release version.

1. `sudo apt install ./cedegrid_<v>_amd64.deb ./cedegrid-system_<v>_amd64.deb`
2. `sudo usermod -aG cedegrid <you>` and `sudo usermod -aG cedegrid <second>`, then log in again
3. only if a run needs a policy other than the default:
   `sudo install -o root -g root -m 0644 <file> /etc/cedegrid/policy.toml`
4. after the runs, read-only: `sudo journalctl -u cedegrid-broker.service --since "<start time>"`
5. for the removed-member run: `sudo gpasswd -d <second> cedegrid`
6. at the end: `sudo apt purge cedegrid-system cedegrid`

### 3. The open points

Each is below under "Decisions needed": N1 to N6, the provisional defaults, and the dependencies
D1 to D3 on item 057.

## Size of the work

73 tasks ([tasks.md](../tasks.md)):

| Kind | Tasks |
|---|---|
| code, packaging or CI | 35 |
| tests written first (with the container harness) | 24 |
| host runs for the other session (T057 to T061, T072) | 6 |
| documentation and records | 4 |
| the constitution change (T069) | 1 |
| checks and the security review package | 3 |

- **MVP**: Phases 1 to 3 (T001 to T026). A member gets Full through the broker, and nothing
  changes elsewhere.
- **Next**: the security suite (US2) passes before anything is installed outside the container.
- **Then**: Yield First's idle slice and moves (US3), OOM (US4) and packaging (US5), which can run
  in parallel, then the host runs.
- **001's merge** waits for this feature to be built and verified on the test host (your rule of
  2026-11-14).

## Decisions needed

**D4. The Principle V amendment.**
- A: approve. Feature 005 can be built.
- B: do not approve. Nothing of 005 is built, and members keep Tracked on hosts without
  delegation.
- *Recommended: A*, with the rules written into the principle, so a later change cannot widen
  what root does without another MAJOR amendment.

**N5. Principle II's OOM sentence.** Principle II says in-share own work's OOM score "MUST NOT be
set below" its launch value, and that CedeGrid must not drive another workload into an OOM kill.
Lowering in-share scores contradicts both.
- A: one clause and one exception bullet. The kernel may then kill another process in place of
  your in-share work, but only for the in-share part and only while the lowering is in force.
- B: no change. The broker only marks over-share tasks at 1000. Your in-share work keeps plain
  Linux's OOM risk, which goes against your rule of 10-29.
- *Recommended: A*. It is what your OOM rule needs. Its cost falls on another user only when
  their process is bigger than everything of the user who is over their share.

**N6. What the broker counts as your verified work.** Principle I lets CedeGrid act only on
processes its supervisor holds through their parent chain. The broker checks unit membership
instead. This was found by the cross-artifact check after the tasks.
- A: one bullet in Principle I. A process inside a unit the broker made, with that unit's user and
  group IDs, held by a pidfd and checked by its start time, counts as verified for the broker. No
  other workload's process can be inside such a unit.
- B: no change. The broker acts only where the agent's verified list agrees. A job's processes
  that left the supervisor's tree, such as a daemon it started, are then never scored or marked,
  so "the over-share side dies first" (SC-007) can fail. Root also takes input from your agent.
- *Recommended: A*. It is a stronger identity than a parent chain, and it keeps Principle I's
  purpose.

**N1. When in-share scores are lowered.**
- A: always. Cost: another user's process can be killed in place of yours even when nobody is
  over their share, which your rule of 11-13 counts as harm.
- B: only while another user is over their fair share. Cost: when a third user causes the OOM, a
  bystander's in-share process can still die in place of yours, because CedeGrid cannot score
  processes outside its units.
- C: never; only mark over-share tasks. Cost: your in-share work keeps plain Linux's risk.
- *Recommended: B*. It protects you exactly when the OOM is not your fault.

**N2. A member removed from the group while work runs.** New requests are refused at once in
every option.
- A: running units go on until their work ends.
- B: the units stop now, and their tasks drain and requeue.
- C: the administrator's policy chooses.
- *Recommended: A*. Removal is not an emergency, and the administrator can still stop a unit with
  `systemctl`.

**N3. A PAM session for the units.**
- A: none. The administrator's policy sets the units' process limits, and `doctor` and the docs
  say that login limits do not apply.
- B: a CedeGrid PAM service with only the limits module. It adds one PAM file.
- C: the host's login stack. Its modules are meant for logins and run more as root.
- *Recommended: A*. It changes no PAM setting, as your brief requires.

**N4. Where your ordinary unit sits.**
- A: in your own `user-<uid>.slice` where the host splits CPU by account, else in
  `cedegrid.slice`. Everyone's CPU share stays at plain Linux's, and the administrator's
  per-account limits keep applying.
- B: always in root-level `cedegrid.slice`, as first designed. On the test host your work gets
  more than plain Linux when two or more other accounts are busy, and per-account limits stop
  covering it.
- C: `cedegrid.slice` with a weight that follows the number of busy accounts. Root code then
  follows other users' activity, and `system.slice`'s share still changes.
- *Recommended: A*, if host run R2-run shows the unit survives your last logout; if not, it falls
  back to B.

**Provisional defaults.**
- Limits: a 64 KiB request limit, a 2 s read timeout, and a 2 s client fallback.
- Bounds: another member answered within 1 s during a flood; Yield First gives way within 10 ms.
- Broker cost: below 0.1% of one CPU and 32 MiB with 50 users.
- Policy: `tasks_per_user` 64 and `tasks_max` 4096.
- OOM and restarts: lowered scores return to the default 10 s after memory becomes unreadable;
  units restart on failure.
- *Recommended*: accept them as provisional. The container and host runs confirm or replace each
  one, and you confirm the result.

**D1 to D3, dependencies on item 057** (decided in [057-report.md](../../001-contention-modes/drafts/057-report.md),
not here). The spec writes both cases. Only D2 changes broker code, and only as one parameter of
the score function: it decides whether the broker marks over-share tasks at 1000 in Performance.

## What waits on the other session

The host runs need a built package and your root steps 1 and 2:
- **R2-run (T057)**: N4 and the plain-Linux comparison; whether the unit survives logout;
  `RemoveIPC`.
- **R10-run (T058)**: the OOM gates.
- **R16-run (T059)**: the broker's and the agents' overhead.
- **The removed-member run (T060)**.
- **R3-run (T061)**: Yield First's margin.
- **The rerun at the release candidate (T072)**.

Each run is recorded against plain Linux, at least 3 runs each way, in `docs/validation.md`.

## What I could not check

- **No host reached.** Every split, share and timing figure here is reasoned, not measured.
- **Whether systemd 249 accepts `CPUWeight=idle`.** The manual gives no version for the `idle`
  value. The CI container (systemd 249) settles it, and the broker writes `cpu.idle` itself if
  not (research R1).
- **Whether `user-<uid>.slice` survives your last logout without lingering**, and the hosts'
  effective `RemoveIPC=`. With `yes`, logind removes your shared memory and IPC objects at
  logout even while your units run; this feature may not change that setting (research R18).
- **`oom_score_adj_min`.** That a CAP_SYS_RESOURCE writer sets the floor and your lower write then
  fails comes from the documentation and my knowledge of the kernel. The kernel source was not
  read in this session. Test T044 checks it in the container.
- **The smallest capability set** the broker's unit needs (task T033), and whether GitHub's
  `ubuntu-22.04` runners can start a container with systemd as PID 1 (task T004).
- **polkit on the shared host** was not read. The rejection of polkit rests on the documentation
  only (research R5).
- **No code was compiled or tested.** This is documents only.

## Commits

On `specs-057-059`, after item 057's `f07d6a0` and `e914a8d`:
- `85d5322` spec and clarify session;
- `1ba0e17` and `a72afb7` plan, research, data model, contracts and quickstart;
- `1fb59c0` tasks and the `cedegrid system` entry point;
- this report's commit: the cross-artifact fixes, the amendment draft, this report, and one
  paragraph in `057-amendment-draft.md` on how the two drafts fit.

## What the cross-artifact check found and what was changed

These were fixed in the 005 files:
- **Constitution, critical**:
  - Principle I's verified set, raised as N6;
  - Principle VI's Python and npm suites, added to T070;
  - Governance's rule that the amendment lands with `docs/guarantees.md` and its tests, now T069.
- **High**:
  - Principle V's read-back added to every broker write;
  - the spec's "Principles I and II stay" corrected;
  - the ordinary unit's slice made to follow N4;
  - the move writes added to the broker's allowed writes;
  - the idle executor's socket moved out of the user's runtime directory, which logout removes
    (R18).
- **Medium**:
  - the policy's allowed modes are applied by the agent, so they are not a security boundary
    (stated, plus new task T054);
  - units get a restart policy;
  - per-account limits added to N4's trade-offs;
  - the tested modules made public;
  - missing edge-case tests and the 50-user bound added;
  - [P] markers on tasks that share a file removed;
  - `ps` and `top` tied to 003;
  - the idle executor added to the spec;
  - the idle level in SC-004 defined.

Left as is: FR-020 and FR-021 overlap on deciding the margin by measurement.

## Pointers

- [spec.md](../spec.md): stories, FR-001 to FR-038, SC-001 to SC-012, decisions N1 to N6.
- [plan.md](../plan.md): Constitution Check (I needs N6, II needs N5, V needs D4), structure,
  design notes.
- [research.md](../research.md): R1 to R18, the root commands (R13), the security review's files
  (R14), the dependencies (R15), the runs table.
- [data-model.md](../data-model.md), [contracts/](../contracts/): the protocol, the units and
  slices, the administrator policy.
- [quickstart.md](../quickstart.md): how it is validated, in the container and on the test host.
- [tasks.md](../tasks.md): the 73 tasks and the gate.
- [amendment-principle-v.md](amendment-principle-v.md): the constitution diff.
