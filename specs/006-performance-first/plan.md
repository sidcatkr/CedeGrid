# Implementation Plan: Performance First

**Branch**: `claude/project-thread-ttyiyp` | **Date**: 2026-11-18 | **Spec**: [spec.md](spec.md)

## Summary

Performance and Aggressive start new compute work as plain Linux would, except beside another
CedeGrid account that arrived first, where 001's hold stays. Other accounts' CedeGrid work is
identified read-only from a marker in its supervisors' command lines, which also carries each
run's arrival on the node. Admit immediately lifts the hold beside first-comers that only yield.
Unprotected, which replaces 004's unprotected Aggressive and keeps its consent path, also raises
own CPU share where the host gives a way that reaches other users' work: the account slice's
weight through the System mode broker, or a granted nice limit where accounts share a CPU
cgroup.

## Technical Context

**Language/Version**: Rust 2024 (MSRV 1.88); Python 3.10 SDK and tools; TypeScript SDK.
**Primary Dependencies**: existing only (sysinfo for process command lines and parents, NVML).
**Storage**: the SQLite state database (the allowance and grants, as 004).
**Testing**: `cargo test` serial; Python and npm suites; decision snapshots; host gates with plain
arms through `tools/aggressive_gates.py`.
**Target Platform**: Linux (glibc 2.35, kernel 5.15 floor); macOS and Windows compile only.
**Constraints**: policy stays pure; telemetry stays read-only; no new dependency.

## Constitution Check

- **I. External processes**: identification reads `/proc` command lines, parent chains and
  cgroups, and NVML process lists. Nothing is written or signalled. The nice raise acts only on
  the run's verified processes, through the supervisor's existing pidfd-held handles. FR-007b's
  release is a thread-ID call on threads of the unreaped verified leader only, under the rule
  for such calls (thread pidfd or start time, checked before and after). Pass.
- **II. No harm** (9.0.0): FR-007 to FR-013 implement Principals, Arrival order, Admission and
  Admit immediately as amended. The floors, OOM order and the first-come GPU floor are
  unchanged. Unprotected is the priority exception, with real-time classes excluded. Pass, with
  amendment 9.0.0 in the same change.
- **III. Unknown**: an unknown arrival makes the other account a first-comer, so new work holds;
  an unidentifiable account is plain by decision. Each mode's unknown behavior stays tested.
  Pass.
- **IV. Pure policy**: principal kinds and arrivals enter through the snapshot and the policy
  input; the policy decides from them alone. Pass.
- **V. Broker**: one new request, the caller's account slice weight under unprotected, limited
  as the amended Principle V states; it needs the security review before release. Pass with
  review.
- **VI. Tests first**: each FR gets a failing test first; decision snapshots are re-recorded and
  every changed decision is reviewed.
- **VII. Compatibility**: the new SDK fields and the renamed 004 fields land in Rust, Python and
  TypeScript with the contract fixture, before 0.2.0 ships. The snapshot gains optional fields.
- **VIII. Docs**: `docs/guarantees.md` and the 001, 004 and 005 docs are rewritten for the new
  rules in the same change.

## Design

### Marker and arrival (FR-001, FR-005)

- The agent launches each supervisor as `cedegrid __assignment-supervisor <spec>
  cedegrid-work=<marker>`. The hidden dispatch reads only the first two arguments, so the extra
  argument changes nothing there.
- `<marker>` is `v1;cpu=<mode>;gpu=<mode>;arrival=<unix ms>`, with the modes resolved under the
  node cap.
- A run's arrival on the node is the earliest arrival of its assignments still running there;
  a new assignment of a run with nothing running gets the launch time. The agent keeps it with
  each assignment in the execution journal, so it survives an agent restart.
- `PolicyInput` gains `arrivals`, each own allocation's run arrival by allocation ID. Own work's
  arrival on a resource is the earliest among running allocations there, or unknown if one of
  them has none.

### Identification (FR-002 to FR-004)

- The process sampler already reads every visible process. For processes of other accounts it
  also keeps the parent PID, and the command line of any process whose second argument is
  `__assignment-supervisor`. A process is CedeGrid work when it, or an ancestor within 32 levels,
  is a marked supervisor of the same account.
- On a host with System mode (`/run/cedegrid/broker.sock` present), a marked supervisor counts
  only when its `/proc/<pid>/cgroup` path lies inside one of that account's broker units
  (its ordinary or idle unit, wherever N4 placed them).
- `PrincipalUsage` gains `cedegrid_cpu: Option<CedeGridWork>` and
  `cedegrid_gpu: BTreeMap<String, CedeGridWork>`, where `CedeGridWork { arrival_unix_ms,
  protected_arrival_unix_ms }`: the earliest arrival among the account's CedeGrid work on that
  resource, and the earliest among its Performance or Aggressive work there (None if none).

### Admission (FR-007 to FR-011), in the policy

- First-comers per resource: CedeGrid principals whose arrival is not later than own work's
  (FR-006).
- Performance and Aggressive, with no first-comer contending: the budget is the scope's capacity
  less the reserve, within the envelope, with no margin, no deduction of others' usage, and
  CedeGrid's own CPU not subtracted. On GPU compute, the time-slice hold does not apply.
- With first-comers contending: today's computation (kept equal share, margin, unknown activity,
  time-slice rule), counting only first-comers as other principals.
- Every decision carries two budgets: the default one, and the admit immediately one, which
  counts only protected first-comers. The latter replaces `unprotected_budget` (renamed
  `admit_immediately_budget` in the decision, the node report and the coordinator's placement).
- New reason codes: `admitted_as_plain`, `held_for_first_comer`, and
  `admit_immediately_ignored_for_protected_first_comer`.

### Placement of held-back tasks (FR-007b), in the agent and the supervisor

- The agent opens a 3 s window when an interval finds a plain principal newly contending while
  the previous interval left no CPU headroom. Within it, a single-process task on a node with a
  CPU scope starts placed: on the scope CPU with the fewest own tasks, counting running leaders
  by the CPU each last ran on (`/proc/<pid>/stat` field 39, read-only) and tasks placed but not
  yet running. The window closes when the plain principal stops contending.
- The supervisor spec and the gate setup carry the CPU; the gate pins itself to it after the
  scope affinity and before exec, and the supervisor verifies the leader's affinity is that CPU
  and records `cpu.placement`. Mediated children start on the whole scope.
- 0.5 s after exec the supervisor puts every thread of the unreaped leader back on the scope:
  the leader's pidfd shows it alive; each thread is fixed by a thread pidfd (`PIDFD_THREAD`,
  Linux 6.9 and later) or else its `/proc/<pid>/task/<tid>` start time, and the same check runs
  after the call. A thread that fails it is reported and not touched again.
- Evidence: the harness proof of 2026-11-22 (`docs/validation.md`): pinned for good, own work took
  2% more and the plain principal got 7.7% less than beside plain Linux; placed and released,
  both matched plain Linux within 0.4%.

### Options (FR-012 to FR-019)

- SDK and wire: `admit_immediately` with `confirm_admit_immediately`, and `unprotected` with
  `confirm_unprotected` (renamed from 004's `unprotected_aggressive` and
  `confirm_unprotected_aggressive`). Unprotected implies admit immediately. Both apply to
  Performance and Aggressive work.
- Admit immediately needs no host allowance; unprotected keeps 004's allowance, phrase, per-run
  grant record and System mode administrator key (`modes.unprotected`, renamed).
- The coordinator places a run that takes either option against the node's admit immediately
  budget. The agent's launch check does the same.
- A raise must act where other users' work competes with own work. Where `user.slice` splits
  CPU by account, that is the account slice: a nice value inside it reaches nobody else, and only
  root can weight it, so only System mode can raise there. Where accounts share a CPU cgroup, a
  nice value reaches them, and the account's granted nice limit bounds it. `actuation::Raise`
  decides which applies on the host.
- Raise, System mode: a new broker request `raise { on: bool }`, served only while the
  administrator policy's `modes.unprotected` is true and the caller's ordinary unit sits in its
  account slice. The broker sets `user-<uid>.slice`'s `CPUWeight` to the policy's
  `unprotected_cpu_weight` (default 400, range 101 to 10000) through `systemctl set-property
  --runtime`, reads it back, and records the expiry of the caller's lease (30 s; the agent
  renews it each monitor interval). It resets the weight when the caller withdraws, when the
  lease expires, when the caller's ordinary unit stops, or when the caller leaves the group; on
  start it resets any weight it set before (its own drop-in, recognized by content).
- Raise, shared CPU cgroup: the supervisor of an unprotected run reads `RLIMIT_NICE`. If it
  permits a value below 0, it sets the run's verified processes to the lowest permitted nice
  value, no lower than −10, and back to 0 when told. Otherwise no raise is available.
- The agent asks for the raise only while an unprotected run is active and no protected
  first-comer is on the CPU, and withdraws it within two monitor intervals of either changing.

### Visibility (FR-020, FR-021)

- `observe` gains `principals`: per resource, each other principal's kind, arrival, first-comer
  flag and hold flag. `doctor` shows the same in a table, plus own work's arrival, the options
  in force and any raise.

## Who builds what

Policy files (`src/policy*`, `src/contention.rs`, `src/explain.rs`, `tests/policy.rs`,
`tests/contention.rs`) are with the 001 thread. The model fields come first from this thread so
both sides build against them.

- **001 thread**: first-comer selection; the two budgets; dropping the margin, the deduction of
  others' usage and the CedeGrid holdout when no first-comer contends; counting only first-comers
  in the kept share and the GPU time-slice rule; reason codes; `explain.rs` rows; decision
  snapshots; 001's spec and guarantees text for admission.
- **This thread**: the constitution; the model fields; the marker, arrival bookkeeping and
  journal; identification in telemetry; the options in the SDKs, wire, contract fixture,
  coordinator placement and launch check; the 004 rename; the raise (broker request, supervisor
  nice); `observe` and `doctor` wiring; the gate harness and the gates; 004, 005 and 006 docs.

## Gates

`tools/aggressive_gates.py` gains:
- `first` and `first-gpu`: own work starts on an idle scope and the second account arrives 20 s
  later;
- `batch`: sixteen 1000 m tasks of fixed work on four CPUs, with completion times;
- `vram`: six 2048 MiB GPU tasks on one 8 GiB device, with which ones could allocate;
- `cg-later-gpu` and `cg-first-gpu`: the second account runs one GPU task through its own
  CedeGrid (`--role peer`, from a copy in `/tmp` it can read) or by hand, before or after own
  work. On CPU, where the host splits by account, the kernel gives each account its share
  whatever the other starts, so the CedeGrid-against-CedeGrid gate runs on GPU compute;
- an `admit` arm (Performance with admit immediately), and plain arms for every shape.

A `ram` shape (more tasks than RAM) waits for the user's approval: its plain arm drives the host
into OOM.

The System mode raise gate runs from the installed packages, with the administrator key set by
the user.

## Risks

- Same-cgroup hosts: plain Linux's per-thread split favors oversubscription (spec Assumptions).
  The gate decides whether a layout rule is needed.
- Identification depends on readable `/proc`. On hosts that hide it, everyone is plain, which
  matches plain Linux.
- A modified agent outside System mode can fake a marker and so be held for as a first-comer. It
  can only make other CedeGrid users hold, which is the default between CedeGrid users anyway.
