# CedeGrid Constitution

## Core Principles

### I. External Processes Are Untouchable (NON-NEGOTIABLE)

- CedeGrid MUST act only on processes it launched and verified. On Linux that means a pidfd
  bound to boot ID, start time, assignment and generation. Elsewhere it means an exclusively
  owned, unreaped direct child. SDK-mediated managed children each carry their own verified
  handle.
- A descendant of a verified own process is in the verified set only when all of these hold:
  - the run's supervisor, as its child subreaper, opened a pidfd for it before reading its
    parent chain;
  - the parent chain leads to the launched process;
  - after reading the chain, the supervisor checked through that pidfd that the process is
    still alive;
  - its user and group IDs match the run's. A descendant whose IDs differ, for example after
    running a setuid program, is not in the verified set.

  Such a descendant carries the run's boot ID, assignment and generation, and CedeGrid acts on
  it only through that pidfd, or through a thread-ID call under the next rule, so a reused PID is
  never acted on. A process that leaves the tree, or whose chain cannot be confirmed, is never
  acted on.
- **Calls that take only a thread ID.** A call that accepts only a thread ID, such as CPU
  affinity, MAY be made on a thread of a verified member only when all of these hold:
  - the member's pidfd shows it alive before the call;
  - the thread's identity is fixed before the call: a thread pidfd where the kernel supports one
    (Linux 6.9 and later), else the thread's entry under the member's `/proc/<pid>/task/<tid>`
    with its start time;
  - after the call, the same check shows the same thread still alive: the thread pidfd, or the
    same entry with the same start time.

  Otherwise the change is reported as possibly misdirected, and that thread is not touched
  again. One window remains: a thread that exits during the call frees its ID, and that ID can
  be reused inside the window only after the kernel's cyclic ID space wraps. A misdirected call
  can only reach a process of the same account, because the kernel refuses such calls on other
  users' processes.
- CedeGrid MUST NOT signal, renice, reprioritize, throttle, pause, move into a cgroup, adopt or
  otherwise act on any other process. This includes other users' processes and unmanaged
  processes of the same account, in every mode and for every actuator.
- Restarts MUST reconcile against verified handles only. CedeGrid MUST NOT adopt or signal a
  process by a persisted numeric PID, name or path. A dead or uncertain supervisor leaves its
  reservation charged until verified reconciliation.
- Every actuator MUST have tests showing it cannot reach a process outside the verified set.

Rationale: other workloads are only ever observed. A recycled PID or a lost supervisor must
never let CedeGrid act on a process it does not own.

### II. No Harm in Any Mode

- Own work MUST be the first to give capacity back when external demand needs it.
- CedeGrid MUST NOT drive another workload into OOM kills, swap thrash, failed GPU allocations
  or loss of capacity that workload is already using. Every mode keeps explicit floors, such as
  minimum available host RAM and per-device VRAM headroom, each with an enforced minimum.
- The only exception to the two rules above is unprotected Aggressive, below, and it covers CPU
  and GPU compute time only.
- Own processes MUST be the preferred OOM victims. Where the platform allows an unprivileged
  process to mark itself, this happens before user code runs, for every own process.
- Own priority MUST NOT be raised above the default: no negative nice values and no real-time
  scheduling classes. No exception relaxes this rule.
- **Modes.** The modes are ordered Yield First ≤ Neutral ≤ Performance ≤ Aggressive. Aggressive
  is a *caution* option with a red label, stronger than Performance's yellow. Its default form,
  protected Aggressive, keeps every rule of this principle: it gives CPU and GPU compute back at
  once to any other active principal that is below the lesser of its demand and its fair share.
- **Unprotected Aggressive, the one named exception.** It MAY decline to give back CPU and GPU
  compute time, as an exception to "Own work MUST be the first to give capacity back" and to
  "loss of capacity that workload is already using". It covers compute time only:
  - the floors, Principle I, the OOM-victim rule and the no-priority-raise rule hold in full;
  - CedeGrid only declines to give compute back. It MUST NOT raise priority, inflate thread
    counts or do anything else to win share; the split is whatever the kernel and the GPU
    driver decide;
  - it is never a default. Configuration files, saved per-job setup, profiles, environment
    variables, upgrades, migrations and the node cap MUST NOT enable it. Only an explicit
    per-run flag, or the same explicit per-submission SDK field, turns it on, and only after
    the two-step confirmation the feature specification defines;
  - while active it MUST be reported in `ps`, `top`, logs and JSON output.
- **Who carries the risk.** An option whose risk falls only on own work, such as a shorter hint
  deadline or draining Guaranteed work sooner, MAY be offered. An option whose risk is OOM kills,
  swap thrash, failed GPU allocations or any other failure of external workloads MUST NOT exist,
  not even as an opt-in. The only risk to other workloads that any option may carry is slower
  compute, and only under unprotected Aggressive.
- Options that let own work hold more capacity within the floors and the reclaim bounds, such as
  Performance mode, protected Aggressive or a floor near its minimum, are marked *caution*. Each
  such option MUST:
  - be off by default and never enabled implicitly, including by upgrades, migrations or
    non-interactive defaults;
  - be chosen explicitly, with its consequence shown and a separate confirmation;
  - be recorded in configuration and reported in observability output while it is active;
  - leave Principle I and the floors intact.
- **Work-conserving.** Within these limits, capacity no workload wants MUST NOT sit idle. A
  reserve held back from own work is justified only by how slowly its resource can be
  reclaimed: by default, CPU and GPU compute, reclaimed within a monitor interval, keep only
  small idle margins, and RAM and GPU memory keep the floors. A more yielding mode the operator
  chooses MAY keep more idle, since that cost falls only on own work.

Rationale: CedeGrid runs only on spare capacity, so running it must never make another workload
fail. An operator may trade away their own work's progress, but never another workload's
safety. Capacity held back without a reclaim reason protects no workload. Some operators accept
slowing other workloads' compute to meet a deadline; that choice stays explicit, rare, visible
and limited to compute time, and it never threatens another workload's safety.

### III. Unknown Is Never Silently Idle

- Missing, stale, inaccessible, ambiguous or unsupported telemetry MUST be recorded as unknown.
  It MUST NOT be treated as idle, empty or free capacity. A GPU without basic inventory or
  memory observation is blocked.
- Each contention mode MUST define explicit, tested behavior for unknown and for occupied
  state.
- Conservative responses MUST be graduated, not all-or-nothing. For example: block expansion,
  then hold current shares, then reduce, then drain. Escalation MUST be bounded in time and
  explained.
- Capability reports MUST keep configured intent, observed capability and actual enforcement
  separate.

Rationale: treating unknown as free charges real demand to nobody. Treating every unknown as an
emergency throws away work for no benefit.

### IV. Pure, Deterministic Policy

- `src/policy.rs` MUST stay deterministic and effect-free. The same configuration, snapshot,
  input and clock always produce the same decision.
- Effects MUST happen only in the agent and the supervisor. Effects include signals, priority
  and cgroup changes, resource hints, drains and termination. Telemetry and kernel diagnostics
  stay read-only.
- Every decision MUST be explainable through concise reasons, one sentence each.

Rationale: a pure policy can be replayed from recordings and tested exhaustively. Separating it
from effects keeps every action auditable.

### V. Rootless and User Space

- CedeGrid MUST run without privileges. It uses no setuid binaries, added capabilities, kernel
  modules or sysctl changes.
- cgroup control MUST stay inside an explicitly delegated cgroup v2 subtree. Every write MUST be
  read back. Nonempty cgroups are kept for reconciliation.
- Rootless execution supports single-process commands, SDK-mediated managed children and, at
  the Tracked control level, descendants that stay in the run's subreaper tree, each verified
  under Principle I. Work started outside the tree, such as through another daemon, is not
  tracked and MUST NOT be presented as contained. Tracked is not containment: a run has no RAM
  cap, and no CPU limit below one core across cgroups.
- CedeGrid MUST NOT be described as a sandbox for hostile code or as a hard VRAM partition.

Rationale: every guarantee has to hold with ordinary user rights and without host-level
configuration.

### VI. Tests First for Policy and Telemetry

- A change to policy, telemetry, yielding or supervision MUST start with a test that fails
  without the change and states the intended behavior. Every fixed defect gets a regression
  test.
- `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, the serial Rust suite, both
  Python suites and the npm suite MUST pass.
- Ownership checks, launch barriers, retained reservations and unknown-telemetry handling MUST
  NOT be weakened to make a test pass.
- Claims about hardware behavior (Linux runtime, cgroups, GPUs, NVML) MUST rest on native
  evidence. Compiling, cross-compiling or passing on macOS is not such evidence. A gate MUST NOT
  be recorded as qualified without evidence, and failed evidence is kept.

Rationale: the stated guarantees are the product. A claim without a test or evidence behind it
is a defect.

### VII. Compatibility

- `config_version = 1` files, the snapshot schema, the coordinator wire protocol, the worker
  protocol and the SDK APIs MUST stay compatible or ship with an explicit migration.
- Wire, worker-context, configuration and numeric changes MUST land in Rust, Python and
  TypeScript together and update the shared contract fixture.
- Rust code MUST build on the MSRV (1.88). Windows client paths keep compiling.

Rationale: agents, coordinators, SDK workers and stored state are upgraded at different times.

### VIII. Concise, Factual Public Docs

- Public docs MUST be concise, factual and written for users and operators.
- They MUST NOT contain work logs, private deployment details or project-motivation narrative.
  Private deployment details include hosts, ports, network posture, internal project names,
  evidence paths and run identifiers.
- Docs describe current behavior. Each normative rule lives in one place, and other docs link
  to it.

Rationale: public docs are a contract with users. Anything else in them goes stale or discloses
private information.

## Execution and Export Constraints

These existing constraints apply to every feature.

- **Launch barrier.** User code MUST NOT run until all of the following have happened, in order:
  1. capacity is reserved
  2. only the trusted worker gate is started
  3. its identity and backend controls are verified
  4. Prepared and then Authorized are durably persisted
  5. EXEC is sent

  A partial preparation failure keeps its evidence and requires confirmed cleanup.
- **Hidden dispatch.** The `__worker-gate` and `__assignment-supervisor` dispatch MUST run before
  argument parsing, before any Tokio runtime or thread starts, and before any terminal handoff.
- **Source export.** Public source candidates come only from `tools/pack_source_review.py` and
  its exact `ALLOWLIST`. A shipped file is listed there and in the Cargo `include`.
  - Exported text uses placeholder home paths and loopback or RFC 5737 IPv4 addresses, and
    contains no key material.
  - Relative links resolve.
  - `Cargo.toml` keeps `publish = false`.
  - `source-manifest.json` is generated, not edited.
- **Tools.** Scripts in `tools/` use only the Python standard library and the in-repo SDK, and
  run on Python 3.10.

## Development Workflow

- Feature work runs through Spec Kit: specify, clarify, plan, tasks, implement.
  - Specs describe behavior in operational terms.
  - Implementation candidates belong in the plan.
  - Each plan's Constitution Check names the principles the feature touches.
  - A proposed relaxation is raised as an amendment, not handled inside the feature.
- Opt-in hardware tests (`CEDEGRID_TEST_CGROUP_ROOT`, `CEDEGRID_TEST_GPU_UUID`) run only with
  explicit authorization. Subprocess fixtures marked `#[ignore]` are never run as a blanket
  `--ignored` suite.
- `CLAUDE.md` holds the runtime development guidance. `docs/guarantees.md` is the detailed
  statement of the support boundary and MUST NOT claim more than this constitution allows.

## Governance

- This constitution governs specs, plans, tasks and reviews in this repository. Where it
  conflicts with another document, it wins until an amendment resolves the conflict.
- An amendment goes through a reviewed pull request that states the version change and the
  reason for it.
  - Relaxing a principle also updates `docs/guarantees.md` and the affected tests in the same
    change.
  - It also requires explicit maintainer approval.
- Versions follow semantic versioning:
  - MAJOR: a principle is removed or redefined
  - MINOR: a principle or section is added, or guidance is materially expanded
  - PATCH: clarifications and wording
- Every pull request review checks the change against Principles I–VIII and the constraints
  above. A deviation is documented with its justification, or rejected.

**Version**: 5.0.0 | **Ratified**: 2026-10-24 | **Last Amended**: 2026-10-26
