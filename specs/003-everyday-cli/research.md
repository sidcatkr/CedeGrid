# Research: Everyday CLI for a Single User and Host

Each entry gives the decision, the rationale and the alternatives considered. Native gates
verify every mechanism on the target kernels. The host check records what each finds, and
`native-gates.md` keeps the commands.

## R1. Local agent: one per user, over a Unix socket

- **Decision**: the agent listens on `$XDG_RUNTIME_DIR/cedegrid/agent.sock`, falling back to
  `$HOME/.local/state/cedegrid/agent.sock`, in a 0700 directory owned by the user. It holds the
  existing namespace lifetime lock, so a second agent cannot start. A cluster-mode agent serves
  the same socket. A local agent exits after `local.idle_exit_s` (default 600) with no runs.
- **Rationale**: two agents of one user would each count the other's work as external and both
  yield (D1). A per-user socket with directory permissions needs no TLS on one host.
- **Alternatives**: a loopback TCP port with tokens (needs secrets, exposes a port); one
  agent per run (double yielding, no shared view for `ps`).

## R2. Full: a per-run cgroup through the delegating user manager

- **Decision**: Full requires a reachable user manager whose `user@<uid>.service` delegates the
  `cpu` and `memory` controllers. Detection reads `cgroup.controllers` and `cgroup.subtree_control`
  of the manager's cgroup and checks that `systemctl --user` answers. Each run starts in a
  transient scope (`systemd-run --user --scope -p Delegate=yes --unit cedegrid-<run>`), and the
  existing cgroup backend controls the scope's subtree.
- **Launch barrier**: `systemd-run --scope` executes the command in place, so the trusted worker
  gate keeps the supervisor's PID relationship and the launch barrier is unchanged. Gate G1
  verifies this.
- **Rationale**: a session cgroup is not delegated even when the user manager is. A transient
  scope gives each run a delegated cgroup wherever the login session sits, with no privilege.
- **Alternatives**: writing under `user@<uid>.service` directly (owned by the manager; races with
  it); calling the D-Bus API (adds a dependency for what `systemd-run` already does).

## R3. Tracked: subreaper tree and verified descendants

- **Decision**: the supervisor sets `PR_SET_CHILD_SUBREAPER` before EXEC. Each monitor interval,
  it walks `/proc/<pid>/task/*/children` from the leader and from known members. For each new
  PID, it follows Principle I (3.0.0):
  1. `pidfd_open` first;
  2. read the parent chain and the real, effective, saved and filesystem user and group IDs
     from `/proc/<pid>/status`;
  3. confirm that the chain reaches the leader and that the IDs match the run's;
  4. check liveness through the pidfd.

  Only then does the process join the verified set, and every action uses its pidfd. Orphans
  are reaped with `waitid(P_PIDFD)`, and their rusage is added to the run's accounting.
- **Rationale**: the subreaper keeps daemonizing descendants in the tree. Opening the pidfd
  first and checking liveness after the reads means a reused PID can never pass the checks. A
  setuid program changes IDs and so leaves the set.
- **Alternatives**: process groups or sessions (a child can leave them, and signaling by PGID
  reaches unverified processes); ptrace (intrusive, and it changes program behavior).

## R4. Tracked CPU lever: per-thread affinity in whole cores

- **Decision**: `sched_setaffinity` on every thread of every verified process, checked every
  interval (new threads inherit the mask, but threads may change their own). Cores shrink and
  grow by whole physical cores, SMT siblings together. GPU runs keep the cores of the GPU's NUMA
  node, from `/sys/bus/pci/devices/<bdf>/numa_node` and `local_cpulist`. A process that changed
  its mask to one CedeGrid did not set is reported, not overridden.
- **Rationale**: affinity is the only unprivileged CPU lever that also works against work in
  other cgroups, it is reversible, and children inherit it. Nice cannot be lowered again by a
  non-root user, and SIGSTOP duty cycles hurt latency-sensitive threads.
- **Reviewer measurements, 4 CPUs**:
  - same cgroup: a 4-thread run pinned to 2 cores beside a 4-thread contender got 196–197%,
    because the contender's threads moved to the free cores;
  - across user groups: with the run limited to one core, the contender got 290% under
    `cpu.max` and 296% under affinity.
- **Alternatives**: nice steps (irreversible without privilege); SIGSTOP duty cycling (stalls
  threads); `RLIMIT_CPU` (kills rather than yields).
- **Principle I (5.0.0)**: `sched_setaffinity` takes a thread ID, and no pidfd form of the call
  exists. Constitution 5.0.0 allows such a call on a verified member's thread only under its
  thread-ID rule, which resolves Decision 1 of report 016:
  - before the call, the member's pidfd shows it alive, or the call is skipped;
  - the thread's identity is fixed before the call: a thread pidfd (`pidfd_open` with
    `PIDFD_THREAD`, Linux 6.9 and later), else the thread's `/proc/<pid>/task/<tid>` entry with
    its start time from `stat`, so older kernels keep the lever;
  - after the call, the same check finds the same thread alive; otherwise the change is
    reported as possibly misdirected and that thread is never touched again;
  - a TID the member's task directory does not list is never touched.

  The window that remains is a thread exiting during the call, with its ID reused before the
  kernel's cyclic ID space wraps; even then the kernel refuses the call for other users'
  processes.
  - **Alternatives**: no per-thread lever before 6.9 (older hosts lose the lever); signalling
    instead of affinity (stalls threads, research R6); a cpuset controller (not delegated to
    unprivileged users).

## R5. Thread pools

- **Decision**: SDK workers receive a `granted_cores` event when their core count changes
  (contract `worker-events.md`), so apps can resize pools (for example `torch.set_num_threads`).
  For runs without the SDK, a native gate measures two launch strategies:
  - start wide and shrink;
  - start at the current grant.

  The default follows the measurement. Start-at-grant is the starting candidate, because spin-waiting
  runtimes lose more than the removed cores when oversubscribed.
- **Rationale**: most runtimes size pools from the visible core count once, at start.
- **Alternatives**: hiding cores through a fake `/proc/cpuinfo` (a runtime layer; violates
  FR-006).

## R6. SCHED_IDLE and autogroup

- **Decision**:
  - **SCHED_IDLE**:
    - a Yield First run uses it only where contenders share its CPU cgroup (detected through
      `/proc/<pid>/cgroup` for contending threads);
    - it is never used for runs holding a GPU, by default;
    - it is set at launch for the whole tree;
    - with RLIMIT_NICE 0 it cannot be undone, so moving to a higher mode needs a restart;
    - starvation (near-zero CPU while runnable) is shown with its duration.
  - **Autogroup nice**: used only where the run's tasks sit in the root CPU cgroup, detected
    from `/proc/<pid>/cgroup` and `/proc/<pid>/autogroup`, and off by default.
- **Rationale**: SCHED_IDLE yields only to threads in the same CPU cgroup.
- **Reviewer measurements**:
  - Linux 6.18, 1 CPU: in the same cgroup, the SCHED_IDLE job got 0.3% and the other 98.4%; in
    separate cgroups, both got 49.6%.
  - 4 CPUs, same cgroup: a 2-thread contender got 197% beside the SCHED_IDLE job's 194%; a
    4-thread contender got 392% and the job 1%. With plain priority, a 4-thread job took 264%
    and a 2-thread contender kept 131%.

  A starved feeder thread leaves a GPU idle while its memory stays held.
- **Alternatives**: SCHED_IDLE always (useless across cgroups and harmful to GPU runs);
  `cpu.idle` on the run's own cgroup (no effect across user groups in measurement); an idle flag
  on the whole user group (an administrator setting; not used or required).

## R7. Contention evidence: per-thread run-queue wait by user

- **Decision**: read `run_delay` from `/proc/<pid>/task/<tid>/schedstat` for other users'
  threads, keyed by (pid, tid, start time). Take per-interval deltas, drop threads that exit
  instead of counting negative changes, and sum by UID.
  - The scan visits at most `evidence.max_threads_per_interval` threads (default 4096) and
    rotates through the rest over later intervals, so the cost stays bounded (SC-009).
  - Wait marks a principal as contending for 001's probe; a principal whose CPU use (from
    `/proc/<pid>/stat`) does not grow after a yield is recorded as satisfied.
  - With hidepid, the host-level signal (PSI, FR-072) goes through the same probe, and `doctor`
    says so.
- **Rationale**: `/proc/<pid>/schedstat` covers the main thread only; reviewer check: 0.02 s
  there against 2.6 s summed over the threads of a 4-thread process. Wait tells users apart even
  in one shared cgroup. A user with more threads than cores always waits, so wait alone must not
  drive yielding.
- **Alternatives**: main-thread schedstat (misses worker threads); PSI only (cannot tell users
  apart within one cgroup).

## R8. Fair share and its limits

- **Decision**: the fair share is the level own work does not yield below. CedeGrid never raises
  a run's thread count or priority to win share back. `ps` and `top` show "below fair share" with
  the reason, for example a contender's thread count. Across cgroups at Tracked, Yield First
  keeps about one core under full contention: affinity cannot be empty, and SCHED_IDLE does not
  cross groups. Docs state this, and drains happen only where the mode's rules already call for
  them.
- **Rationale**: across cgroups, the kernel already splits CPU between sibling user groups
  (measured: both wanting 4 cores, 195% and 197%; a 2-thread contender kept 181%). In one shared
  cgroup, CPU is split per thread, so a contender with more threads takes more.
- **Alternatives**: raising own threads to match (an arms race that harms others).

## R9. RAM at the Tracked level

- **Decision**: no cap and no address-space limits. Each process keeps the `oom_score_adj` it
  launched with; only in Yield First and Neutral are the tasks above own memory's fair share
  marked as preferred victims, through 001's family marks (Principle II's OOM order). In Yield
  First and Neutral, SDK hints start one hint deadline earlier than at Full, and a drain
  checkpoints and the run resumes automatically when available memory clears the floor for the
  calm window. Performance and Aggressive get no hint, drain or TERM for a floor breach (floors
  by mode; converged 2026-11-26).
- **Rationale**: there is no unprivileged RAM cap. `RLIMIT_AS` breaks the address-space
  reservations of GPU frameworks.
- **Alternatives**: `RLIMIT_AS` or `RLIMIT_DATA` (break GPU runtimes); cgroup v1 tricks (need
  privilege).

## R10. Logs and the viewer

- **Decision**: each run writes output to `logs/<run>/output.log`, rotated at
  `logs.max_bytes` (default 64 MiB) with `logs.keep` (default 3) files. The agent reads the run's
  pipes on a dedicated task with a bounded ring and never blocks the writer; if the disk is slow,
  the oldest buffered chunk is dropped and counted.
  - The foreground terminal tails the log.
  - Ctrl-C once sends a drain; twice force-stops the verified processes.
  - `-d` detaches.
  - `--tty` allocates a pseudo-terminal for the job.
- **Rationale**: D2 and D3. The agent's 4 MiB prefix capture loses the tail, which users need.
- **Alternatives**: the terminal owning the job (an SSH drop kills it); blocking pipes
  (backpressure slows the job).

## R11. TUI library

- **Decision**: `ratatui` 0.30 with `crossterm` 0.29, for the setup interface and `top`.
  - The published `rust-version` is 1.88.0 for ratatui and 1.63 for crossterm; the first task
    confirms `cargo +1.88.0 check` with the full dependency tree.
  - `NO_COLOR`, non-terminal flag behavior and comment-preserving writes keep their 001 tests.
- **Rationale**: grouped sections, a side pane and in-place editing; one look shared by setup and
  `top`; crossterm keeps Windows client paths compiling.
- **Alternatives**: extending the raw-termios interface (a hand-written layout engine and no
  Windows backend); `cursive` (larger, its own event model).

## R12. One task schema

- **Decision**: `src/task_schema.rs` defines the fields once. It generates the clap arguments
  for `run` and exports `tests/contracts/task-schema.json`. The Python and TypeScript tests read
  the fixture and compare it with `command_task` and `commandTask`.
- **Rationale**: FR-004 requires a test that fails when the option sets differ, and one source
  keeps them in step.
- **Alternatives**: hand-written flags with a comparison test only (drifts until the test fails);
  generating the SDKs from Rust (a build step in two ecosystems).

## R13. Single-core bottleneck detection

- **Decision**: per run, track the busiest thread's CPU share and the idle share of the granted
  cores over a window of `bottleneck.window_intervals` (default 10). Flag a bottleneck when one
  thread stays at or above 90% of a core while the run uses less than half of its other granted
  cores. Likely causes come from the environment (`OMP_NUM_THREADS=1` and similar) and from the
  busiest thread being the main thread.
- **Rationale**: these signals are cheap to read and explain the usual causes without
  instrumenting the program.
- **Alternatives**: sampling stacks (a runtime layer and costly).

## R14. Overhead criteria

- **Decision**: a native gate runs a CPU benchmark and a GPU benchmark bare and under `run`, with
  no contention, five times each. It passes when the median throughput under `run` is within 2%
  of the bare median (SC-003). The same gate compares Tracked with Full (SC-005).
- **Rationale**: D8; the median of five runs damps noise.
- **Alternatives**: a single run (noise exceeds 2%).

## R15. Per-core contention

- **Decision**: the run-queue scan also reads `Cpus_allowed_list` from each contending process's
  `/proc/<pid>/status`, read-only, and keeps each principal's allowed CPUs.
  - While a principal's wait stays at the level that marks it as contending (T043) for the
    probe's response window, and its CPUs overlap own work's granted cores or are unknown, the
    probe does not record it as satisfied. This covers the saturation probe and the PSI-only
    probe of 001 (FR-021, FR-072), which share one function.
  - At the Tracked level, the policy then moves own work's granted cores off a known overlap,
    never below own work's fair share in cores, through the affinity lever under the thread-ID
    rule (R4). At the Full level no per-thread call is made, since Full verifies no members per
    thread; own work yields through its cgroup as in 001.
  - It lands in 003, beside T043's change to the same probe code, so 001 needs no follow-up
    after the merge.
- **Rationale**: a contender pinned to cores own work uses gains nothing from a global step, so
  its CPU use does not grow, and the probe would wrongly record it satisfied while it waits.
- **Alternatives**: treating pinned contenders like others (records them satisfied while they
  wait); stepping own work down globally until the pinned contender grows (gives away capacity
  on cores the contender cannot use).

## R16. Install script verification

- **Decision**: the release publishes per-target tarballs, `SHA256SUMS` and `install.sh` with its
  version written in. The script refuses at UID 0, downloads over HTTPS, checks the tarball
  against `SHA256SUMS` with `sha256sum`, or else `shasum -a 256`, before unpacking, and installs
  into `~/.local/bin`. On a mismatch, a missing checksum or a missing tool it stops with nothing
  installed.
- **Rationale**: a pinned version and a checked hash make a reinstall reproducible. A user
  directory needs no root (Principle V).
- **Alternatives**:
  - signatures (minisign, sigstore): stronger, but they need a key or tool on the host; a later
    item may add them;
  - an unpinned "latest" URL: a reinstall could change the version silently;
  - installing into `/usr/local/bin`: needs root.
- **Limit**: the script itself is trusted through HTTPS from the release. A user who wants more
  can check it against `SHA256SUMS` first.

## R17. Contended-CPU placement order

- **Decision**: move own threads off a contended CPU before any duty cycle. At the Tracked
  level this uses affinity. At the Full level it uses `cpu.weight` where it is authorized and
  the contender shares the delegated parent. The duty cycle applies only when own usage still
  exceeds its target afterwards. A CPU step changes at most once per change in the contender's
  measured load.
- **Rationale**: report 029 run 4 kept the other user's median within 0.02 ms beside duty-capped
  own work, but its p99 grew from 0.356 ms to 3.33 ms with 188 step changes in 300 s. A duty
  cycle shares the contended CPU's run queue in time slices; moving off leaves the contender's
  CPU to it.
- **Alternatives**:
  - `cpuset.cpus` on own leaves at the Full level: the per-leaf form of affinity, where the
    cpuset controller is delegated. FR-057 names `cpu.weight`, so this would need a spec change
    and the user's decision.
  - duty cycles alone with a longer period: fewer switches, but the contender still waits behind
    own threads within each period.
- **Limit**: `cpu.weight` orders own leaves only against their siblings. Against another user's
  slice it does nothing, so at the Full level placement helps only same-parent contenders.
  Cross-slice contention keeps 001's `cpu.max` step.

## R18. Root refusal placement

- **Decision**: one `geteuid` read at the top of `main`, before the hidden dispatch. The hidden
  supervisor commands refuse too.
- **Rationale**: the dispatch must come before clap, Tokio and any thread; a syscall starts none.
  Refusing in the hidden commands keeps user code from running as root even if a parent did.
- **Alternatives**: a check per subcommand (one can be missed); allowing `doctor` as root (it
  would describe a level the user's own runs never get).

## R19. Dropping the registries

- **Decision**: publish nothing new to npm or PyPI; the CLI ships as the GitHub Release binaries
  with the install script; the SDKs stay in the repository with their tests.
- **Rationale**: the user decided on 2026-10-27, and again on 2026-11-14, that the registries do
  not fit CedeGrid; one install channel that verifies the binary's sha256 needs no registry
  account, and the SDK tests already hold the wire contract without publishing.
- **Alternatives**: keep the SDK packages on the registries (the user declined); attach the SDK
  files to the GitHub Release and install them by URL (the reviewer's recommendation, open with
  the user).

