# Feature Specification: Everyday CLI for a Single User and Host

**Feature Branch**: `001-contention-modes`, specified alongside it. The spec directory is
independent of the branch. Implementation starts on its own branch once 001 is merged.

**Created**: 2026-10-26

**Status**: Draft

**Input**: Maintainer feature description: make CedeGrid easy for a single user on a single
Linux host, and keep the SDK, the coordinator cluster mode and the agent tooling fully capable.

This feature builds on [001-contention-modes](../001-contention-modes/spec.md): its modes,
yield ladder, probing (FR-021, FR-072), setup (user story 7) and observability (user story 8).
It does not redefine them. The constitution (5.0.0) applies, in particular Principle I's
verified set for descendants and its rule for calls that take only a thread ID, and
Principle V's Tracked control level.

## Clarifications

### Session 2026-10-26

- Q: How does `run` handle a script that forks on a host with no delegated cgroup subtree? →
  A: The run is Tracked automatically and says so in one line at start; `--require-containment`
  refuses instead. Its supervisor, as child subreaper, follows the run's tree through pidfds,
  reaps orphans and counts their usage. CPU is reduced through CPU affinity only, never through
  nice steps or stopping processes. There is no RAM cap, no per-process address-space limit, and
  nothing outside the run's tree is acted on.
- Q: Which parts of the user's environment does `run` pass to the work, and may environment
  values be stored? → A: It passes the working directory, PATH, the active venv or conda
  environment and `CUDA_VISIBLE_DEVICES`. Values are never written to state, logs or reports;
  `--save` stores names only, and values only with `--save-env`.
- Q: How many local agents may a user have on one host, and what happens when one already runs
  in cluster mode? → A: One agent per user per host. `run` reuses an existing agent, including a
  cluster-mode one, and an idle local agent exits after a configurable idle time.
- Q: What happens to a run when the user's terminal closes or they press Ctrl-C? → A: The
  terminal is a viewer: the run continues under the agent and `logs -f` reattaches. Ctrl-C once
  drains gracefully, twice force-stops own verified processes only, and `-d` detaches from the
  start.
- Q: How is a run's output kept, and what happens when output comes faster than it can be
  stored? → A: A rotating log file per run keeps output including the tail, with no fixed prefix
  limit. Output never backpressures the job, and `--tty` gives the job a pseudo-terminal.
- Q: When may a Tracked run change CPU affinity on a thread of one of its verified processes,
  given that the affinity call accepts only a thread ID? → A: Only when the member's pidfd shows
  it alive first, the thread's identity is fixed first (a thread pidfd on Linux 6.9 and later,
  else its `/proc/<pid>/task/<tid>` entry with its start time), and the same check finds the
  same thread alive afterwards. Otherwise the change is reported as possibly misdirected and the
  thread is never touched again; the call is skipped when the pre-check fails.
- Q: On kernels older than 6.9, which have no thread pidfds, does a Tracked run keep the
  affinity lever? → A: Yes, through the `/proc/<pid>/task/<tid>` entry with its start time.
- Q: What happens to a thread ID that no longer appears under the member's `/proc/<pid>/task`
  directory, for example because it now belongs to another thread group? → A: It is never
  touched.
- Q: How should the probe treat a contender pinned to cores that own work also uses, which a
  global step may not help? → A: Its pinning is read read-only (`Cpus_allowed_list`). While its
  wait rate stays high and its CPUs overlap own work's, it is not recorded as satisfied, and own
  work's affinity moves off the overlapping cores (own processes only, under the thread-ID
  rule), never below own work's fair share.
- Q: Does that per-core rule also apply to 001's PSI-only probe? → A: Yes. The plan says whether
  that part lands in 003 or needs a 001 follow-up.

### Session 2026-11-02

- Q: How does CedeGrid treat a command run as root, and where does the control level come from?
  → A: Every `cedegrid` command run with effective UID 0 refuses with one line that says why and
  what to run instead: the same command as the user, without `sudo`. The only exception is the
  System broker of feature 005, not yet specified. The control level (Tracked or Full, later
  System) comes from the host's state, never from `sudo` or from how CedeGrid was installed. A
  test covers the refusal, and `doctor` shows a line for it.
- Q: What is the CLI's install channel? → A: The GitHub Release binaries with a one-line install
  script that installs into `~/.local/bin`, needs no root, pins the version and verifies the
  sha256 before installing. Homebrew and `cargo install` are secondary channels. The README leads
  with the script, not with `npm install --global`. pip carries the Python SDK only and npm the
  TypeScript SDK; the CLI is never bundled into the pip wheel. The existing npm CLI packages keep
  working under FR-080. A native check installs on a clean account.
- Q: How does own work avoid raising a contender's tail latency when CPU caps are duty cycles,
  as in 001's Tracked level (report 029 run 4: the other user's p99 rose from 0.356 ms to
  3.33 ms beside capped own work, with 188 step changes in 300 s)? → A: Before a duty cycle caps
  own work on a CPU a contender also uses, own threads move off that CPU: affinity at the Tracked
  level, under FR-043's thread-ID rule, or `cpu.weight` where the Full level allows it. Step
  changes stay within 001's SC-009 bound. A native gate checks it; the contender's p99 bound is
  the user's decision and is not set yet.

## Terms

- **Run**: one piece of work started with `cedegrid run`, an SDK task or a cluster assignment,
  with its processes and its output.
- **Local agent**: the user's own CedeGrid agent on this host. It supervises runs, and
  commands reach it over a Unix socket that only the user can open.
- **Control level**: how strongly CedeGrid can control a run on this host.
  - **Full**: the run gets its own cgroup in a subtree delegated to the user, so its whole
    process tree is contained, with a CPU cap, a memory limit and its own pressure readings.
  - **Tracked**: no delegated subtree exists. CedeGrid follows the run's processes and gives
    capacity back through levers an unprivileged user has. Tracked is not containment.
- **Verified process**: a process CedeGrid may act on under Principle I: the launched process,
  an SDK-mediated child, or, at the Tracked level, a descendant verified through its pidfd.
- **Contender**: another user's process that wants the CPU. It is a **same-cgroup contender**
  when it shares the run's CPU cgroup, and a **cross-cgroup contender** otherwise.
- **Granted cores**: the CPUs a run may use at this moment.
- **Per-job entry**: saved settings for one script, in a `cedegrid.toml` under
  `[run."<script>"]`. A **profile** is a named per-job entry chosen with `-p NAME`.
- **Effective value**: the value a run uses for a setting, together with its **source**: a
  flag, a per-job entry, the global setup or the built-in default.
- **Starved**: a run that is runnable but gets near-zero CPU.
- **Single-core bottleneck**: a run whose busiest thread holds one core near 100% while its
  other granted cores sit idle.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Run a script with one command (Priority: P1)

A user runs `cedegrid run train.py`, or `cedegrid run -- <command...>`, on a host where
CedeGrid was never set up. The work starts at once under CedeGrid's modes and yields when
other users need the capacity. It runs exactly as it would without CedeGrid.

**Why this priority**: today a user writes a job file, configures an agent and starts a
coordinator before the first run. One command makes CedeGrid usable for everyday work.

**Independent Test**: on a fresh account, run `cedegrid run -- <cpu benchmark>` and check:
- it starts with no configuration file;
- `ps` lists it with its mode and control level;
- it yields when a contender starts;
- it finishes with the same result as a bare run.

**Acceptance Scenarios**:

1. **Given** no CedeGrid state for the user, **When** they run `cedegrid run train.py`,
   **Then** a local agent starts and the script runs under the default mode (Neutral).
2. **Given** a running local agent, **When** the user starts a second run, **Then** it uses the
   same agent.
3. **Given** a job JSON file used with the SDK today, **When** the user runs
   `cedegrid run --from-json job.json`, **Then** the run uses the same fields.
4. **Given** a script that imports the SDK, **When** it runs under `cedegrid run`, **Then**
   resource targets, the allowance and drain reach it exactly as for an SDK-submitted task.
5. **Given** any SDK task field, **When** the user lists `cedegrid run --help`, **Then** a flag
   for that field is listed.
6. **Given** a run started in the foreground, **When** the user's SSH session drops, **Then** the
   run continues and `cedegrid logs <run> -f` reattaches to its output.

---

### User Story 2 - See and control own runs (Priority: P1)

The user lists their runs, reads and follows their output, stops one, and watches a live view
of host resources, runs and why each run is stepped.

**Why this priority**: a run the user cannot see or stop is not usable day to day.

**Independent Test**: start two runs and a contender. Check `ps`, `logs -f`, `top` and `stop`.
Check that no command can reach a process outside the user's verified runs.

**Acceptance Scenarios**:

1. **Given** two own runs, **When** the user runs `cedegrid ps`, **Then** each shows its state,
   mode, control level, usage and current step with its reason.
2. **Given** a running job, **When** the user runs `cedegrid logs <run> -f`, **Then** new output
   appears as it is written.
3. **Given** a run, **When** the user runs `cedegrid stop <run>`, **Then** it drains first and
   its processes end, and no other process is signaled.
4. **Given** a contender, **When** the user opens `cedegrid top`, **Then** it shows the step
   taken and its reason within one monitor interval.
5. **Given** a run started by another user, **When** the user runs `cedegrid stop` on it,
   **Then** the command refuses.

---

### User Story 6 - Give capacity back without privileges (Priority: P1)

On a host where the user has no delegated cgroup subtree and no administrator help, CedeGrid
still uses idle capacity and gives it back when others need it. Where delegation exists,
CedeGrid always uses it. Both levels are designed, tested and gated as primary paths.

**Why this priority**: hosts without delegation are common. If CedeGrid only protected others
with delegation, it could not be used safely on them.

**Independent Test**: on a test host with delegation, measure the same benchmark and
contenders under Full and under Tracked (forced by a test-only switch). Use a contender in the
run's CPU cgroup and one in another cgroup.

**Acceptance Scenarios**:

1. **Given** a host with delegation, **When** a run starts, **Then** it runs at the Full level.
2. **Given** a host without delegation, **When** a run starts, **Then** it runs at the Tracked
   level without any flag, needs no privilege or host change, and prints one line saying it is
   tracked rather than contained, and why.
3. **Given** `--require-containment` on a host without delegation, **When** the user starts a
   run, **Then** it refuses to start.
4. **Given** a Tracked run and a contender, **When** the contender wants CPU, **Then** the run's
   granted cores shrink by whole physical cores and grow back when the contender leaves.
5. **Given** a same-cgroup contender with more threads than own work, **When** own work holds
   its fair share, **Then** `top` and `ps` show that own work gets less than its fair share and
   why, and CedeGrid adds no threads and raises no priority.
6. **Given** a Tracked run under memory pressure, **When** the floor is threatened, **Then** the
   SDK hint comes first, then a drain with a checkpoint, and the run resumes when memory frees.

---

### User Story 3 - Save per-job settings and see where values come from (Priority: P2)

The user opens the setup interface for one script, saves its settings next to the script, and
later runs it with no flags. `--explain` shows each value the run uses and where it came from.

**Why this priority**: repeated runs should not need repeated flags, and a user must be able to
tell which setting won.

**Independent Test**: save a per-job entry, run with and without flags, `-p`, `--no-setup`,
`--save` and `--explain`, and check the precedence.

**Acceptance Scenarios**:

1. **Given** no per-job entry, **When** the user runs `cedegrid setup train.py` and saves,
   **Then** `./cedegrid.toml` holds a `[run."train.py"]` entry.
2. **Given** a saved entry, **When** the user runs `cedegrid run train.py`, **Then** the entry is
   applied and one line names the file used.
3. **Given** a flag and a per-job value for the same field, **When** the run starts, **Then**
   the flag wins and `--explain` names it as the source.
4. **Given** a per-job mode above the node cap, **When** the run starts, **Then** it runs at the
   cap and says so.
5. **Given** the per-job setup interface, **When** the user reaches floors, the node cap or
   margins, **Then** they are shown read-only.

---

### User Story 4 - A clearer setup interface (Priority: P2)

The setup interface shows the host at a glance, groups its settings, explains the selected
setting with this host's numbers, and changes values in place.

**Why this priority**: setup decides how much CedeGrid takes and gives back; a user must
understand each choice.

**Independent Test**: open setup on a terminal and with `NO_COLOR`, and run it non-interactively
with flags. Check the layout, the explanations and that writes keep comments.

**Acceptance Scenarios**:

1. **Given** the interactive setup, **When** it opens, **Then** a header summarizes the host and
   sections group Modes, Safety floors and Advanced.
2. **Given** a selected setting, **When** the user moves to it, **Then** a side pane explains it
   with this host's numbers.
3. **Given** a caution setting, **When** the user changes it, **Then** it is marked, and a
   separate confirmation is still required.
4. **Given** `NO_COLOR` or a non-terminal, **When** setup runs, **Then** it behaves as today.

---

### User Story 7 - Find and ease single-core bottlenecks (Priority: P2)

A run that cannot use the cores it was granted is shown with likely causes. The user can let
CedeGrid set common thread-count variables, and an SDK app can grow its thread pools when cores
free up.

**Why this priority**: idle granted cores are capacity nobody uses, and the cause is often a
setting the user can change.

**Independent Test**: run a benchmark limited to one thread on several granted cores, and check
detection, the named causes and `--threads auto`.

**Acceptance Scenarios**:

1. **Given** a run whose busiest thread holds one core while other granted cores idle, **When**
   it continues for the detection window, **Then** `top`, `ps` and the end-of-run summary show it
   with likely causes.
2. **Given** `--threads auto`, **When** the run starts with an inherited thread-count variable,
   **Then** the variable is set to the granted core count and `--explain` shows it.
3. **Given** a thread-count variable set in the run's own flags or per-job entry, **When**
   `--threads auto` is on, **Then** that value is kept.
4. **Given** an SDK app, **When** its granted cores change, **Then** it receives an event with
   the new core count.

---

### User Story 5 - Install, first run and a readable doctor (Priority: P3)

A user installs CedeGrid, runs a first job and checks the host with `cedegrid doctor`, which
shows each check as passed or failed with a fix hint.

**Why this priority**: the other stories assume CedeGrid is installed and its limits are clear.

**Independent Test**: install on a clean account, run a first job, run `doctor` with and
without delegation, and check the output and `--json`.

**Acceptance Scenarios**:

1. **Given** a clean account, **When** the user follows the install guide, **Then** a first
   `cedegrid run` works with no further step.
2. **Given** a host without delegation, **When** the user runs `cedegrid doctor`, **Then** it
   shows the Tracked level, what it means, and which evidence is available.
3. **Given** any `cedegrid` command, **When** the user runs it with `sudo`, **Then** it refuses
   with one line that says why and to run it as the user (FR-082).

---

### Edge Cases

- A local agent is already running for the user in cluster mode: `run` reuses it (FR-002).
- The user's SSH session ends while a run is in its first seconds: the run continues (FR-015).
- Output arrives faster than it can be stored: the job is never slowed; the log rotates
  (FR-016).
- A script forks workers, and one of them starts a daemon: the daemon stays in the run's tree
  and is tracked (FR-042). One runs a setuid program: that process leaves the verified set and
  is never acted on (Principle I).
- A tracked process changes its own CPU affinity: it is reported, not overridden (FR-043).
- A thread exits between the checks around an affinity change: the change is reported as
  possibly misdirected and that thread is not touched again. A thread ID the member's
  `/proc/<pid>/task` no longer lists is never touched (FR-043).
- A contender is pinned to cores own work also uses and keeps waiting after a global step: it is
  not recorded as satisfied, and own work moves off those cores, never below its fair share
  (FR-056).
- Own work would be capped by a duty cycle on a CPU a contender also uses: its threads move off
  that CPU first (FR-057).
- A user runs a `cedegrid` command with `sudo`: it refuses and says to run it as the user
  (FR-082).
- Work started through another daemon (for example a terminal multiplexer server) is outside
  the run's tree and is not tracked (FR-054).
- Other users' process details are hidden from the user.
- Thousands of threads exist on the host.
- A Yield First run is starved for minutes.
- Every core is granted away except one under cross-cgroup contention at the Tracked level.
- Two `cedegrid.toml` files apply to one script.
- A per-job entry names a GPU that is absent or held.

## Requirements *(mandatory)*

### Functional Requirements

**Run (user story 1)**

- **FR-001**: `cedegrid run [options] <script>` and `cedegrid run [options] -- <command...>`
  MUST start work with no prior setup and no configuration file.
- **FR-002**: Each user MUST have at most one agent per host. `run` MUST start the user's local
  agent when none runs and MUST reuse a running one, including an agent in cluster mode. A local
  agent MUST accept commands only over a Unix socket that only the user can open, with no
  coordinator and no TLS, and MUST exit after a configurable idle time with no runs.
- **FR-003**: `run` MUST launch through the same agent, supervisor and launch barrier as
  SDK-submitted tasks. Ownership and launch-barrier rules MUST NOT differ by launch path.
- **FR-004**: Every SDK task field MUST have a `run` flag. Flags MUST come from one task schema
  shared with the Python and TypeScript SDKs, and a test MUST fail when the SDK and CLI option
  sets differ.
- **FR-005**: `--from-json <file>` MUST accept the existing job JSON. Flags given beside it MUST
  override its fields.
- **FR-006**: `run` MUST add no runtime layer between the work and the kernel: the work's
  processes are ordinary processes of the user, with no interposer, emulation or container.
- **FR-007**: Resource targets, the allowance and drain MUST reach a script that imports the SDK
  exactly as they reach an SDK-submitted task.
- **FR-008**: Every command in this feature MUST offer `--json` output.
- **FR-009**: `run` MUST pass the working directory, PATH, the active venv or conda environment
  and `CUDA_VISIBLE_DEVICES` to the work at launch. Environment values MUST NOT be written to
  state, logs or reports.

**Status and control (user story 2)**

- **FR-010**: `cedegrid ps` MUST list the user's runs with state, mode, control level, CPU, RAM
  and GPU usage, the current step and its reason, and the starved and below-fair-share flags.
- **FR-011**: `cedegrid logs <run>` MUST print a run's output, and `-f` MUST follow it.
- **FR-012**: `cedegrid stop <run>` MUST drain the run first and end it after the drain timeout,
  through verified processes only. `--force` MUST skip the drain.
- **FR-013**: `cedegrid top` MUST show host resources, the user's runs and the reason for each
  step, refreshed every monitor interval.
- **FR-014**: These commands MUST act only on the invoking user's runs, through verified
  processes, and MUST refuse any other process or run.
- **FR-015**: The terminal MUST only view a run: closing it or losing the connection MUST NOT
  stop the run, and `logs -f` MUST reattach. Ctrl-C once MUST drain the run gracefully; Ctrl-C a
  second time MUST force-stop its verified processes only. `-d` MUST detach from the start.
- **FR-016**: Each run MUST keep its output in a rotating log file that includes the latest
  output, with no fixed prefix limit. Output MUST NOT slow or block the job. `--tty` MUST give
  the job a pseudo-terminal.

**Per-job setup and explain (user story 3)**

- **FR-020**: `cedegrid setup <script>` MUST open the setup interface scoped to that script and
  save to `./cedegrid.toml` under `[run."<script>"]`, keeping comments.
- **FR-021**: `run` MUST apply the matching per-job entry and print one line naming the file
  and entry used.
- **FR-022**: A script's per-job entry MUST be keyed by the script's path relative to the
  `cedegrid.toml` found in the working directory or its parents, searching up to the home
  directory. For `--` commands, the key MUST be the first argument after the interpreter.
- **FR-023**: `-p NAME` MUST pick a named profile, `--no-setup` MUST ignore saved settings, and
  `--save` MUST write the current flags to the per-job entry, storing environment variable names
  only. Environment values MUST be saved only with `--save-env`.
- **FR-024**: `--explain` MUST show each effective value with its source.
- **FR-025**: Precedence MUST be flags, then the per-job entry, then the global setup, then the
  built-in defaults (Neutral).
- **FR-026**: A per-job entry MAY set modes, GPU selection, requests, class, retries (`max_attempts`)
  and environment variable names. It MUST NOT set floors, the node cap or margins, which the per-job
  interface shows read-only. A per-job mode MUST NOT exceed the node cap.
- **FR-027**: A run MUST select a GPU with `--gpu N` (a device index) or `--gpu auto`, which
  picks the least-loaded device that admission allows.

**Setup interface (user story 4)**

- **FR-030**: The interactive setup MUST show a host summary header and group its settings into
  Modes, Safety floors and Advanced, with a side pane that explains the selected setting with
  this host's numbers.
- **FR-031**: Values MUST change in place with the left and right keys. Caution settings MUST be
  marked (yellow where color is allowed) and MUST keep their separate confirmation.
- **FR-032**: `NO_COLOR`, non-terminal flag behavior and comment-preserving writes MUST stay as
  001 defines them.
- **FR-033**: `top` MUST use the same visual style.

**Control levels (user story 6)**

- **FR-040**: At every launch (`run`, SDK tasks and cluster assignments), CedeGrid MUST detect
  the control level without privilege and without changing any host setting. Full MUST be used
  wherever the user's service manager delegates the CPU and memory controllers, even when the
  login session itself sits outside the user's slice; Tracked otherwise, with no flag needed.
  A Tracked run MUST print one line at start saying it is tracked rather than contained, and
  why. `ps`, `top` and `doctor` MUST label the level, and `--require-containment` MUST refuse
  to start instead of running Tracked.
- **FR-041**: Full and Tracked MUST each have their own native gates on a test host. A test-only
  switch MUST force Tracked on a host with delegation, so both levels can be measured side by
  side.
- **FR-042**: At the Tracked level, the run's supervisor MUST be the child subreaper of its
  tree, so descendants that daemonize stay in it. It MUST track every descendant through pidfds
  and a tree walk, reap orphans and add their resource usage to the run's accounting. It MUST
  act only on verified processes as defined by Principle I, never on a process outside the
  run's tree.
- **FR-043**: At the Tracked level, the CPU ceiling MUST be the run's granted cores, applied to
  every thread of every verified process, checked again every monitor interval, changed in
  whole physical cores (SMT siblings together) and reversible. For a GPU run, the cores nearest
  the GPU's NUMA node MUST be kept. CPU MUST NOT be reduced by raising nice values, which an
  unprivileged user cannot lower again, or by stopping and resuming processes. A tracked process
  that changes its own CPU affinity MUST be reported, not overridden. Each per-thread change MUST
  follow Principle I's rule for calls that take only a thread ID:
  - the member's pidfd shows it alive before the call, or the call is skipped;
  - the thread's identity is fixed before the call: a thread pidfd on Linux 6.9 and later, else
    its entry under the member's `/proc/<pid>/task/<tid>` with its start time, so hosts on older
    kernels keep the lever;
  - after the call, the same check finds the same thread alive; otherwise the change is reported
    as possibly misdirected and that thread is never touched again;
  - a thread ID that the member's `/proc/<pid>/task` does not list is never touched.
- **FR-044**: CedeGrid MUST tell an SDK app when its granted core count changes, so it can
  resize its thread pools. For runs without the SDK, the launch strategy (start wide or narrow)
  MUST be chosen from measurements recorded in the plan.
- **FR-045**: A CPU weight through the run session's autogroup MAY be used only where the run's
  tasks sit in the root CPU cgroup. It MUST be detected, not assumed, and MUST be off by
  default.
- **FR-046**: A Yield First run MAY start in the idle scheduling class where contenders share
  its CPU cgroup. Such a run MUST NOT move to a higher mode without a restart. Runs holding a GPU
  MUST NOT use it by default, for any of their processes. `top` and `ps` MUST show when a Yield
  First run has been starved, and for how long.
- **FR-047**: CedeGrid MUST detect whether contenders share the run's CPU cgroup and report
  which levers apply.
- **FR-048**: Across cgroups at the Tracked level, a Yield First run under full contention MUST
  keep about one core's share, since no lever goes lower. This limit MUST be documented, and
  drains happen only where the mode's existing rules call for them.
- **FR-049**: The fair share MUST be treated as the level own work does not yield below, not a
  guarantee. CedeGrid MUST NOT raise a run's thread count or priority to win share back. `top`
  and `ps` MUST show when own work gets less than its fair share, and why.
- **FR-050**: At both levels, contention evidence MUST include run-queue wait grouped by user,
  summed over threads, from per-thread changes that drop exited threads, with a bounded scan
  cost. Wait alone MUST NOT drive yielding: it MUST feed the probe of 001 FR-021 and FR-072, so
  a user whose CPU use does not grow after a yield is recorded as satisfied. Where other users'
  process details are hidden, the host-level signal MUST be used through the same probe, and
  `doctor` MUST say so.
- **FR-051**: Where users share one CPU cgroup, Neutral and Performance MUST reach the fair share
  by limiting cores and correcting from measured usage.
- **FR-052**: At the Tracked level, RAM MUST have no cap, and per-process address-space limits
  MUST NOT be used, since they break the address-space reservations of GPU frameworks. Own processes MUST stay the preferred
  OOM victims, SDK hints MUST come earlier than at the Full level, and a drain MUST checkpoint
  and resume automatically when memory frees.
- **FR-053**: Native gates MUST cover a same-cgroup contender and a cross-cgroup contender, at
  both levels. Any host change a gate needs MUST have the user's approval first.
- **FR-054**: Work started outside the run's tree, such as through another daemon or a terminal
  multiplexer server, MUST NOT be tracked, and the docs MUST say so.
- **FR-055**: Docs MAY mention that an administrator can enable per-user lingering and
  controller delegation to give the Full level. CedeGrid MUST NOT require it, and no document
  MAY require a system-wide change.
- **FR-056**: A contender pinned to cores own work also uses may not gain from a global step.
  CedeGrid MUST read each contender's allowed CPUs read-only (`Cpus_allowed_list` in
  `/proc/<pid>/status`). While a contender's wait stays at the level that marks it as contending
  (FR-050) for the probe's response window, and its allowed CPUs overlap own work's or are
  unknown, the probe (001 FR-021, and FR-072's PSI-only probe) MUST NOT record it as satisfied.
  At the Tracked level, own work's affinity MUST then move off a known overlap, on own processes
  only and under FR-043's thread-ID rule, never below own work's fair share. At the Full level,
  no per-thread call is made; own work yields through its cgroup's CPU limit as in 001.
- **FR-057**: Before a duty cycle caps own work on a CPU a contender also uses, own threads MUST
  move off that CPU: through affinity at the Tracked level, on own processes only and under
  FR-043's thread-ID rule, or through `cpu.weight` where the Full level authorizes it. Duty
  cycles are 001's rootless CPU caps on SDK and cluster tasks and the Full level's `cpu.max`;
  `run`'s Tracked path has none (FR-043), so affinity is its only lever. Moving off MUST NOT take own work below its fair share. Own work's CPU step
  changes MUST stay within 001's SC-009 bound, at most one per change in the contender's load.
  A native gate MUST measure the contender's p99 latency beside own work under this rule
  (SC-013).

**Single-core bottlenecks (user story 7)**

- **FR-060**: CedeGrid MUST detect a single-core bottleneck over a detection window and show it
  in `top`, `ps` and the end-of-run summary, with likely causes such as a thread-count variable
  set to 1 or data loading on the main thread.
- **FR-061**: `--threads auto`, off by default, MUST set `OMP_NUM_THREADS`, `MKL_NUM_THREADS`
  and `OPENBLAS_NUM_THREADS` to the granted core count at launch. It MUST override values
  inherited from the shell and MUST NOT override values set in the run's own flags or per-job
  entry. `--explain` MUST show it, and the SDK task schema MUST carry the same field.
- **FR-062**: Docs and the bottleneck message MUST point users with many independent pieces of
  work to submitting them as separate tasks.

**Install and doctor (user story 5)**

- **FR-070**: The install guide MUST lead to a working first `cedegrid run` with no further
  step.
- **FR-071**: `cedegrid doctor` MUST show each check as passed (✓) or failed (✗) with a
  one-line fix hint, including the control level, which contention evidence is available, and
  that it runs as a user other than root (FR-082).
- **FR-073**: The CLI's main install channel MUST be the GitHub Release binaries with a one-line
  install script. The script MUST install into `~/.local/bin`, need no root, pin the version and
  verify the binary's sha256 before installing it. Homebrew and `cargo install` are secondary
  channels. The README MUST lead with the script, not with `npm install --global`. The pip
  package MUST carry the Python SDK only and the npm SDK package the TypeScript SDK; the CLI MUST
  NOT be bundled into the pip wheel. The existing npm CLI packages keep working (FR-080).

**Compatibility and safety**

- **FR-080**: The SDK APIs, the coordinator mode, the worker protocol and `config_version = 1`
  files MUST stay compatible or ship with an explicit migration.
- **FR-081**: No command in this feature MAY act on a process outside the run's verified set,
  raise own priority, or change a host setting.
- **FR-082**: Every `cedegrid` command run with effective UID 0 MUST refuse with one line that
  says why and what to run instead: the same command as the user, without `sudo`. The only
  exception is the System broker of feature 005. The control level (Tracked or Full) MUST come
  from the host's state, never from `sudo` or from how CedeGrid was installed.

### Key Entities

- **Run**: identifier, owner, launch path, command, control level, mode, granted cores, state,
  output log, and current step with its reason.
- **Local agent**: one per user per host; socket path, start time, idle timeout, mode (local
  or cluster).
- **Control level**: Full or Tracked, with the detection evidence behind it.
- **Per-job entry**: script key, fields it sets, environment variable names (values only with
  `--save-env`), file it lives in, optional profile name.
- **Effective value**: field, value, source.
- **Contention evidence**: per-user run-queue wait and CPU use, with its availability (per user
  or host level), and each contender's allowed CPUs, read read-only.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user with no prior setup starts a script with one command, and it appears in
  `ps` within 5 seconds, including the local agent's start.
- **SC-002**: 100% of SDK task fields have a `run` flag, and the schema test passes in Rust,
  Python and TypeScript.
- **SC-003**: With no contention, the throughput of a CPU benchmark and of a GPU benchmark under
  `run` is within measurement noise (at most 2%) of a bare run, as measured by a native gate.
- **SC-004**: On the same host, Tracked gives CPU back within one monitor interval of Full's
  response time, and gives back at least 90% of what Full gives back, for both contender
  placements.
- **SC-005**: With no contention, Tracked's overhead is within 2% of Full's on the same
  benchmark.
- **SC-006**: Tests show that no signal, affinity change or scheduling change reaches a process
  outside the verified set, for every command and both control levels.
- **SC-007**: `--explain` names a source for every effective value.
- **SC-008**: A single-core bottleneck is shown within three monitor intervals of the detection
  window, with at least one named cause.
- **SC-009**: The contention evidence scan uses less than 5% of one core with 10,000 threads on
  the host.
- **SC-010**: `doctor` shows ✓ or ✗ with a fix hint for every check, and a new user can name
  their control level from one `doctor` run.
- **SC-011**: Run with effective UID 0, every `cedegrid` command refuses with the one-line
  message, as a test shows, and `doctor` shows the check.
- **SC-012**: On a clean account without root, the install script installs the pinned,
  sha256-verified CLI into `~/.local/bin`, and a first `cedegrid run` works with no further step,
  as a native check shows; a checksum mismatch stops the install with nothing installed.
- **SC-013**: In a native gate with a contender on a CPU own work uses, own threads leave that
  CPU before any duty cycle caps them, own work's CPU step changes stay within 001's SC-009
  bound, and the contender's p99 latency stays within a bound the user sets
  [NEEDS CLARIFICATION: the p99 bound is the user's decision].

## Assumptions

- Control levels, contention evidence and the Tracked levers are Linux features. On other
  platforms `run` keeps today's rootless limits.
- `supervise job.json` stays, and the coordinator mode and the SDK keep working unchanged
  (Principle VII). This feature adds a path; it removes none.
- The TUI library choice (US4) depends on its minimum Rust version fitting 1.88; the plan checks
  it.
- Thresholds such as the single-core detection window come from 001's monitor interval unless
  the plan measures better values.
- Implementation waits until 001 is merged into `dev` and the user says to start.
