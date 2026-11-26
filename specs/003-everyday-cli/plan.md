# Implementation Plan: Everyday CLI for a Single User and Host

**Branch**: specified on `001-contention-modes`; implemented on its own branch after 001 is
merged into `dev` | **Date**: 2026-10-26 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `specs/003-everyday-cli/spec.md`

## Summary

`cedegrid run` starts work through the user's own local agent, over a Unix socket with no
coordinator and no TLS, along the same supervisor and launch-barrier path that SDK tasks and
cluster assignments use. `ps`, `logs`, `stop` and `top` view and control own runs. Per-job
entries in `cedegrid.toml` and `--explain` make repeated runs flag-free and traceable. One task
schema generates the `run` flags and is checked against both SDKs.

At launch, CedeGrid detects a control level for every launch path:
- **Full**: a delegating user manager gives each run its own cgroup.
- **Tracked**: the run's supervisor is the child subreaper of the run's tree. It verifies
  descendants under Principle I (5.0.0) and gives CPU back through per-thread affinity in whole
  cores, each call made under Principle I's rule for calls that take only a thread ID.

Contention evidence adds per-user run-queue wait at both levels, fed through 001's probe, and
each contender's allowed CPUs, so a contender pinned to own work's cores is not recorded as
satisfied while it still waits. The setup interface moves to ratatui and crossterm, and `top`
shares its style.

## Technical Context

**Language/Version**: Rust 1.88 (MSRV, edition 2024); Python SDK 3.10+; TypeScript SDK.

**Primary Dependencies**: existing crates (clap 4 derive, tokio, serde, toml_edit, libc,
sysinfo, nvml-wrapper). New: `ratatui` 0.30 (rust-version 1.88.0) and `crossterm` 0.29
(rust-version 1.63), for US4 and `top` only (research R11).

**Storage**: the existing SQLite ledger and journals; per-run rotating log files under the
agent's state directory; `cedegrid.toml` per-job entries (TOML, comment-preserving writes).

**Testing**: `cargo test` (serial), the Python and npm suites, the schema contract test across
Rust, Python and TypeScript, and native gates on a Linux test host (`native-gates.md`, written
with the tasks).

**Target Platform**: Linux for control levels and contention evidence. macOS keeps today's
rootless limits for `run`. Windows client paths keep compiling.

**Project Type**: CLI with a per-user agent and supervisor processes; SDK libraries.

**Performance Goals**:
- SC-001: first run visible in `ps` within 5 s, including the agent's start.
- SC-003: throughput within 2% of a bare run with no contention.
- SC-009: the evidence scan stays under 5% of one core with 10,000 threads.

**Constraints**:
- no privilege and no host change (Principle V);
- act only on verified processes (Principle I);
- no own priority raised (Principle II);
- environment values never stored (FR-009).

**Scale/Scope**: one user and one host per local agent. Run trees up to hundreds of processes;
hosts with up to tens of thousands of threads.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | How the plan complies |
|---|---|
| I. External processes untouchable (5.0.0) | Tracked acts only on descendants verified as Principle I requires:<br>- the pidfd is opened before the parent chain is read;<br>- the chain leads to the launched process;<br>- liveness is confirmed through the pidfd afterwards;<br>- user and group IDs match the run's;<br>- every action goes through the pidfd, except thread-ID calls under the next rule.<br>Per-thread affinity follows the rule for calls that take only a thread ID (research R4): the member's pidfd shows it alive first, or the call is skipped; the thread's identity is fixed first (a thread pidfd on Linux 6.9 and later, else its `/proc/<pid>/task/<tid>` entry with its start time); the same check finds the same thread alive afterwards, or the change is reported as possibly misdirected and that thread is never touched again; a TID the member's task directory does not list is never touched. Contenders' allowed CPUs are only read.<br>`ps`, `stop` and `top` resolve runs through the agent, never by PID. Tests: T030–T035, T039, T074–T077. |
| II. No harm in any mode | Floors and modes come from 001 unchanged.<br>- Own priority is never raised: no negative nice, no real-time class.<br>- SCHED_IDLE and autogroup nice only lower priority.<br>- Tracked RAM has no cap, so SDK hints come earlier and a drain checkpoints (FR-052).<br>- Fair share is a floor for own yielding, not a guarantee; no thread count is raised (FR-049). |
| III. Unknown is never idle | The control level, the cgroup placement of contenders and per-user evidence are each detected. Hidden process details fall back to the host-level signal with a `doctor` note (FR-050). Detection failure means Tracked, not Full. |
| IV. Pure policy | The evidence summary enters `Snapshot` as data. Policy decides as in 001 (probe, ladder), and actuation stays in the supervisor (`supervision/tracked.rs`). |
| V. Rootless (5.0.0) | No privilege, no sysctl, no host change.<br>- Full uses a user-manager-delegated subtree only.<br>- Tracked supports descendants in the subreaper tree. Work outside the tree is not tracked or presented as contained (FR-054).<br>- Tracked is labeled "not containment": no RAM cap, no CPU limit below one core across cgroups (FR-048). |
| VI. Tests first | Every policy, telemetry, supervision and protocol task starts with a failing test. Hardware claims rest on native gates for Full and Tracked and for both contender placements. |
| VII. Compatibility | The task schema and the core-count event are wire and worker-protocol changes. They land in Rust, Python and TypeScript with the contract fixture. `supervise job.json`, the coordinator and `config_version = 1` stay. |
| VIII. Public docs | Docs describe the two levels generically, with no host descriptions. The optional linger and delegate setup is mentioned as optional (FR-055). |

Result: pass. Constitution 5.0.0 resolved the open item on the Tracked affinity actuator (report 016 Decision 1, row I). Complexity Tracking lists the two new dependencies.

**Re-check against 9.2.0 (2026-11-26, before implementation).** Rows I, III, IV, VI, VII and
VIII hold as written. Row II changes with the spec (Session 2026-11-26): the core lever belongs
to `run` launches only (001 FR-091), Performance and Aggressive keep every core, so running own
work is never capped there, and Tracked RAM follows the OOM order and floors by mode (FR-052:
launch score kept, Yield First and Neutral over-share tasks marked, nothing hinted or drained in
Performance and Aggressive). Row V gains System mode: where this account runs in its broker
unit, `run` hands work to that agent (FR-040), and the install script stays user-level while the
`.deb` packages go to administrators (FR-073). Own priority stays at or below the default; the
broker's two raises are 006's and are not reached from `run`. Result: pass.

## Project Structure

### Documentation (this feature)

```text
specs/003-everyday-cli/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   ├── cli.md             # commands, flags, exit codes, --json shapes
│   ├── task-schema.md     # the single task schema and its generated surfaces
│   ├── local-agent.md     # socket location, lifecycle and requests
│   └── worker-events.md   # granted-core events for SDK workers
├── checklists/requirements.md
└── tasks.md
```

### Source Code (repository root)

```text
src/
├── main.rs              # adds run, ps, logs, stop, top subcommands (dispatch stays first)
├── task_schema.rs       # NEW: the one task schema; clap args and JSON export derive from it
├── run.rs               # NEW: `cedegrid run` front end
├── run/
│   ├── client.rs        # local socket client; starts the agent when absent
│   ├── scope.rs         # per-job entry lookup, profiles, precedence, --explain
│   ├── env.rs           # environment capture (names stored, values never)
│   └── view.rs          # foreground viewer: log tail, Ctrl-C drain/force, -d
├── control_level.rs     # NEW: Full/Tracked detection and its evidence
├── agent/
│   ├── local.rs         # NEW: Unix socket server, idle exit, single-agent lock
│   └── logs.rs          # NEW: rotating per-run logs, no backpressure
├── supervision/
│   ├── tracked.rs       # NEW: subreaper tree, descendant verification, affinity actuator
│   └── full.rs          # NEW: per-run cgroup through the delegating user manager
├── telemetry/
│   └── runqueue.rs      # NEW: per-thread schedstat by UID, bounded scan
├── top.rs               # NEW: live view (ratatui), shared style with setup
├── setup/interface.rs   # redesigned with ratatui; per-job scope
└── explain.rs           # doctor ✓/✗ checks, control level, evidence availability

python/cedegrid/          # task schema check, granted-core event, threads field
npm/src/                  # same
tests/
├── run_cli.rs            # NEW: run, ps, logs, stop, explain, precedence, --json
├── task_schema.rs        # NEW: schema contract across CLI and both SDKs
├── tracked.rs            # NEW: descendant verification and actuator ownership
├── control_level.rs      # NEW: Full/Tracked detection with fake cgroup and manager roots
├── runqueue.rs           # NEW: evidence summing, deltas, scan bound
└── contracts/task-schema.json  # NEW: exported schema fixture for Python and TypeScript
```

**Structure Decision**: one crate, as today. New files are child modules of their area (as
`agent/`, `policy/` and `telemetry/` already are), listed in `ALLOWLIST`, with no `#[path]`.
The run path is a client (`run.rs` and `run/`) and an agent side (`agent/local.rs`,
`agent/logs.rs`) that hands work to the existing supervisor. `supervision/tracked.rs` and
`supervision/full.rs` are the two control levels behind the existing `LaunchBackend`.

## Design highlights

### Run path
1. `cedegrid run` parses flags generated from `task_schema.rs`, then resolves the effective
   values: flags, then the per-job entry, then the global setup, then defaults.
2. It captures the environment (FR-009) and connects to the local socket, starting the agent if
   absent.
3. It submits the task. The agent reserves, launches through the supervisor and the launch
   barrier, and streams output to the per-run log.
4. The terminal tails the log as a viewer (FR-015).

`--json` prints the run record instead of the tail.

### Local agent lifecycle
- **Location**: the socket is `$XDG_RUNTIME_DIR/cedegrid/agent.sock`, or
  `$HOME/.local/state/cedegrid/agent.sock` without a runtime directory. Its directory has mode
  0700 and is owned by the user; the agent refuses to listen in any other directory.
- **One per user per host**: the existing namespace lifetime lock (`namespace.rs`) is held for
  the agent's life. A second start finds the lock and connects instead.
- **Cluster mode**: a cluster-mode agent also serves the local socket, so `run` reuses it (D1).
  Local runs then appear in its reports as own work.
- **Idle exit**: with no runs for `local.idle_exit_s` (default 600), the agent exits. A cluster
  agent never idles out.
- **Start**: `run` starts `cedegrid agent --local` detached (new session, standard streams to a
  log). It waits up to 3 s for the socket and fails with the agent's log tail otherwise.

### One task schema
`task_schema.rs` lists every task field with:
- its type, default and help text;
- its JSON name;
- its CLI flag;
- its SDK names.

clap arguments are generated from it, and so is `tests/contracts/task-schema.json`. Python and
TypeScript tests load the fixture and compare it with `command_task` and `commandTask`
parameters and fields. A difference in either direction fails CI (FR-004). New fields from this
feature (`threads`, `gpu` selection) enter through the schema.

### Control levels
- **Full detection**: a running user manager (`systemctl --user` reachable over its private
  socket) whose `user@<uid>.service` delegates the cpu and memory controllers. The run gets a
  transient scope through the user manager (`systemd-run --user --scope -p Delegate=yes`,
  research R2), and the existing cgroup backend drives it.
- **Tracked**: otherwise, including detection errors. The supervisor sets
  `PR_SET_CHILD_SUBREAPER` before EXEC.
  - Every monitor interval it walks `/proc/<pid>/task/*/children` from the leader.
  - For each new PID it opens a pidfd first, then confirms the parent chain and the user and
    group IDs, then checks liveness through the pidfd (Principle I).
  - It reaps orphans with `waitid` on their pidfds and adds their rusage to accounting, with
    live sums and `cutime`/`cstime`.
- **CPU lever (Tracked)**: `sched_setaffinity` on every thread (TID) of every verified process,
  checked every interval.
  - Each call follows Principle I's thread-ID rule (research R4). Before it, the member's pidfd
    must show it alive (else skip), and the thread's identity is fixed: a thread pidfd
    (`pidfd_open` with `PIDFD_THREAD`) where the kernel supports it, else the
    `/proc/<pid>/task/<tid>` entry and its start time from `stat`. After it, the same check
    must find the same thread; otherwise the run records the thread as possibly misdirected,
    shows it in `ps --json` and the log, and never touches that TID again. A TID the member's
    task directory does not list is never touched. Support is detected once per agent start.
  - Cores shrink and grow by whole physical cores (SMT siblings together).
  - GPU runs keep the cores of the GPU's NUMA node first.
  - A process that changed its own mask is reported, not overridden.
- **Thread pools**: the SDK worker gets a `granted_cores` event (contract `worker-events.md`).
  For runs without the SDK, the native gate measures start-wide and start-narrow, and the
  default follows the result (research R5).
- **SCHED_IDLE** (Yield First, same-cgroup contenders only, never for GPU runs by default):
  set at launch for the run's tree. With RLIMIT_NICE 0 it cannot be undone, so mode changes up
  need a restart. Starvation shows in `ps` and `top`.
- **Autogroup nice**: only when the run's tasks sit in the root CPU cgroup (detected); off by
  default.
- **Limits stated in docs**:
  - across cgroups, Tracked has no lever below one core (FR-048);
  - the fair share is a floor for own yielding, not a guarantee (FR-049).

### Contention evidence
`telemetry/runqueue.rs` reads `/proc/<pid>/task/<tid>/schedstat` (run_delay) for other users'
threads:
- it takes per-thread deltas, drops threads that exit, and sums by UID;
- the scan is bounded per interval, by a thread budget and rotation (research R7).

The summary joins the snapshot as per-principal wait. Wait never yields by itself: it marks a
principal as contending for 001's probe (FR-021, FR-072), and a principal whose CPU use does
not grow after a yield is recorded as satisfied. Where other users' details are hidden, the
host-level signal goes through the same probe and `doctor` says so.

**Per-core contention (FR-056, research R15)**: the same scan reads `Cpus_allowed_list` from
each contending process's `/proc/<pid>/status`, read-only, and the summary carries each
principal's allowed CPUs.
- In `src/contention.rs`, the probe does not record a principal as satisfied while its wait
  stays at the level that marks it as contending (T043) for the probe's response window and its
  allowed CPUs overlap own work's granted cores. Unknown allowed CPUs count as overlapping
  (Principle III). This applies to the saturation probe (001 FR-021) and the PSI-only probe (001
  FR-072), which share that code.
- At the Tracked level, the policy then asks for own work's granted cores without a known
  overlap, never fewer than own work's fair share in cores, and the supervisor applies it
  through the affinity lever under the thread-ID rule. Without a known overlap, no core moves.
  At the Full level no per-thread call is made, because Full verifies no members per thread:
  own work yields through its cgroup's CPU limit as in 001.
- Placement: it lands in 003. T043 already changes the probe's satisfied rule in
  `src/contention.rs` after 001 is merged, and the PSI-only path is the same function, so 001
  needs no follow-up.

### Native gates (both levels, both placements)
- **Levels side by side**: a test-only switch (`CEDEGRID_TEST_FORCE_TRACKED=1`) forces Tracked
  on a host with delegation, so both are measured on the same host.
- **Cross-cgroup contender**: a contender under a second account, which has its own cgroup.
- **Pinned contender (FR-056)**: at the Tracked level, a contender started under `taskset` on
  two of own work's cores. It reaches its demand after own work moves off those cores, and it is
  never recorded as satisfied while it waits.
- **Thread-ID calls (FR-043)**: on a kernel with thread pidfds, and through the `/proc`
  fallback (a test-only switch, `CEDEGRID_TEST_FORCE_PROC_THREADS=1`, forces it on a newer
  kernel), a Tracked run's affinity changes reach only its verified threads, and no change is
  reported as possibly misdirected in a normal run.
- **Same-cgroup contender**: on hosts that give each user a separate cgroup, a contender started
  by the same account from the run's own session, with Tracked forced, shares the run's CPU
  cgroup without any host change. It counts as the owner's unmanaged usage, which 001 already
  treats as a contending principal. Per-user evidence for a second account uses the
  cross-cgroup contender. If no second account exists, creating one is a host change and needs
  the user's approval first.

## Item 048: root refusal, install channel and contended-CPU placement (2026-11-02)

Spec Session 2026-11-02 adds FR-057, FR-073 and FR-082 and SC-011 to SC-013. No product code
changes in item 048; these land with the rest of 003 after 001 is merged and the user says to
start.

**Technical Context delta**: no new crate. The install script is POSIX `sh` and uses
`sha256sum` or `shasum -a 256`, whichever the host has (research R16). The release workflow
gains Linux binaries (x86_64 and aarch64, glibc 2.35 floor) with a `SHA256SUMS` file and the
script as release assets. No wire, SDK, config or contract fixture change.

**Root refusal (FR-082, SC-011, research R18)**:
- `main.rs` reads the effective UID once, at the top of `main`, before the hidden
  `__worker-gate` / `__assignment-supervisor` dispatch. A `geteuid` call starts no thread, so
  the dispatch still runs before clap, Tokio, any thread and the launcher terminal handoff.
- At UID 0 every command, the hidden ones included, prints one line to stderr (`--json`: the
  contract's error object, code `ERR_CEDEGRID_ROOT`) and exits 3. The line says to run the same
  command as the user, without `sudo`. Feature 005's broker will be the only exception.
- Control level detection reads only the host's state: the user manager, delegation and the
  authorized controls. It never reads `SUDO_*` variables, the install path or the binary's
  owner. A test pins that the detection's inputs carry none of them.
- The check is a pure function of the effective UID, unit-tested. A CI step on the Ubuntu
  runner, whose user has passwordless `sudo`, runs every subcommand under `sudo -n` and expects
  the line and exit 3.
- `doctor` shows `✓ runs as a user other than root`. At UID 0 it refuses before any check runs.

**Install channel (FR-073, SC-012, research R16)**:
- `tools/install.sh` is published with each release, with its version written in at release
  time, so the one-line command `curl -fsSL <release URL>/install.sh | sh` installs that
  version.
- It refuses at effective UID 0. It downloads the target's tarball and `SHA256SUMS` over HTTPS,
  verifies the tarball's sha256 before unpacking, and on a mismatch stops with nothing
  installed. It installs `cedegrid` into `~/.local/bin`, creating it if missing, and says so
  when that directory is not on `PATH`.
- Homebrew (a tap) and `cargo install --locked cedegrid` are documented after the script as
  secondary channels.
- The README leads with the script. The npm launcher packages keep working (FR-080) and move
  below the script in the README.
- The pip wheel stays SDK-only. A packaging test fails if the wheel's file list holds a
  `cedegrid` binary.
- The native check runs on a clean account (SC-012): an account with no earlier install. Using
  an empty home under an existing account approximates it. Creating a new account is a host
  change and needs the user's approval first (FR-053).

**Contended-CPU placement (FR-057, SC-013, research R17)**:
- Contended CPUs are found per CPU: host busy per CPU from `/proc/stat`, less own verified
  threads' per-CPU use from their `stat` (`processor` and CPU times), with each contender's
  `Cpus_allowed_list` from FR-056's scan. Unknown per-CPU use counts as contended
  (Principle III).
- Order:
  1. When the ladder would cap own work on a contended CPU, the policy first asks for own work's
     granted cores without that CPU. At the Tracked level the supervisor applies it through
     FR-043's affinity lever under the thread-ID rule. At the Full level it sets `cpu.weight`
     on own leaves, never above the default of 100 (Principle II), where `cpu.weight` is
     authorized and the contender shares the delegated parent.
  2. The duty cycle applies only when own usage still exceeds its target in the next interval.
  3. Moving off never takes own work below its fair share in cores. Then the ladder steps as in
     001.
- Step churn: a CPU step on own work changes at most once per change in the contender's
  measured load (001 SC-009). It holds while the contender's demand stays within the
  measurement band.
- Native gate (SC-013): report 029 run 4's shape again: 4 CPUs, Performance, the second account
  saturating two of them 30 s on and 30 s off for 300 s. Own work runs beside it once with
  CedeGrid and once as a fixed `taskset` allocation of the same own share, in the same run. It
  reports the contender's p50 and p99 and own work's step changes. It passes when the
  contender's p99 beside Tracked caps is at most 1.2× its p99 beside the fixed allocation.

**Constitution Check (item 048)**:

| Principle | How it complies |
|---|---|
| I. External processes untouchable | Placement changes affinity or `cpu.weight` only on own verified threads and leaves, under the thread-ID rule. Contenders' allowed CPUs and per-CPU use are only read. The install script touches only `~/.local/bin` and its own temporary files. |
| II. No harm | Placement moves own work only and never below its fair share. `cpu.weight` is never raised above the default. The duty cycle stays the fallback, so own work still gives back what the mode requires. |
| V. Rootless | Root is refused, and the control level comes from the host's state. The install needs no root and changes no host setting. |
| VII. Compatibility | The npm CLI packages, the pip SDK, config files and the wire are unchanged. The refusal adds one exit-code use (3) and one error code. |
| VIII. Public docs | The README leads with the script and stays generic, with no host names. |

Result: pass. The user set SC-013's p99 bound on 2026-11-02: at most 1.2× the p99 beside a
fixed allocation of the same own share, in the same run.

## Item 058: no distribution through npm or PyPI (2026-11-14)

**Input**: spec Session 2026-11-14 and FR-073; the user's decision of 2026-10-27, restated on
2026-11-14 (review item 058 Part E).

**What changes**: nothing is removed in this item. FR-073 now says that no new version of anything
goes to npm or PyPI. The removal is its own set of tasks, done only on the user's go-ahead and
after the install script exists:
- the npm launcher (`npm/src/launcher.ts`, `src/launcher_terminal.rs` and its use in
  `src/main.rs`, and `CEDEGRID_LAUNCHER_PID`'s removal in `src/supervision/unix.rs`) and the
  native npm packages;
- `tools/publish_packages.py`, `tools/publish_github_packages.py`, the registry steps in
  `.github/workflows/release.yml`, `.github/workflows/github-packages.yml`,
  `tools/verify_launcher_pty.py`, and the parts of `tools/build_packages.py`,
  `tools/verify_package_interop.py`, `tools/verify_python_packages.py`, `npm/scripts/build.ts`
  and `npm/scripts/verify-packages.ts` that exist only for the registries;
- `docs/release.md`'s publication section, the matching entries of `docs/release-0.2-gates.json`,
  the launcher line in `CLAUDE.md` and the `ALLOWLIST` in `tools/pack_source_review.py`.

Kept: everything that builds and checks the release binaries, and the SDK code, its tests and any
SDK file attached to the release. By a search the removal touches 13 files of about 1680 lines,
7 tool tests in `tests/test_release_gate.py` and one Rust test
(`npm_launcher_identity_does_not_reach_internal_gate_or_supervisor` in `tests/cli.rs`). A
migration note covers 0.2.0 on the registries and CLIs installed through npm.

**Constitution Check**: Principle VII holds: the SDK tests keep running in every check and hold
the wire contract in three languages. Principle V: no install path needs root. Nothing is
deprecated or removed on a registry without the user's word.

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| `ratatui` 0.30 + `crossterm` 0.29 | US4's grouped layout, side pane and in-place editing, and `top`'s live view, on terminals including Windows client paths | The current raw-termios interface would need its own layout, widgets and Windows backend; the crates fit MSRV 1.88 and are MIT |
