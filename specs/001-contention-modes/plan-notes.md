# Plan notes: contention modes

Input for `/speckit-plan`. These are candidates and verified facts about the current code, not
decisions. Before relying on any mechanism, check it against the man pages and the target kernel
and driver, and record native evidence. Line numbers refer to `dev` at `6be4cea`.

## Current code facts that constrain the design

### Policy and authority

- **Two kinds of policy engine run at once.** The agent runs one `PolicyEngine`
  (`src/agent.rs:1904`), and every assignment supervisor runs another (`src/agent.rs:550`), each
  with its own collector. A supervisor drains itself when its own decision lists it
  (`src/agent.rs:412-431`). A drain cannot be undone, so the node behaves as the union of N+1
  decisions. FR-015 requires one authority.
- **Supervisor engines start with no history.** `prepared()` never evaluates
  (`src/agent.rs:290-368`). This is D1 at runtime: `src/policy.rs:443-447` treats any
  unattributed memory as growth on the first evaluation, and `:442` treats every external PID as
  new.
- **Unknown or inconsistent measurements zero the whole dimension.** See
  `src/policy.rs:243-244, 337-343, 360-378` (D5, D6), with victims chosen at `:600-616`. Victim
  order is pending first, then lexical ID (`:609`).
- **The protection cap only lowers a budget.** `protective_shrink_percent` and
  `active_shrink_percent` lower a cap (`src/policy.rs:495-512`). The only thing the agent can do
  with that cap is drain whole allocations.
- **The GPU cooldown also gates CPU and RAM.** `gpu.scale_up_cooldown_ms` gates CPU/RAM
  expansion (`src/policy.rs:633-634`) and the agent's launch-slot ramp (`src/agent.rs:2497-2518`).
  FR-034 removes this coupling.
- **Errors after EXEC SIGKILL immediately, whatever the class.** Any error in the supervisor's
  running loop goes straight to `kill_and_confirm` (`src/supervision.rs:1088, 1167-1186`).
  FR-017 forbids this. The path includes `control.poll()?` (telemetry plus `evaluate`),
  `drain.notify()?` and a rejected journal transition.
- **Supervisor drain logging could panic.** Fixed on this branch (`54ce12c`): diagnostics use a
  non-panicking write. A pidfd kill guard or `panic=abort` for the hidden commands is still
  worth considering, because any other panic would also orphan the workload.
- **Orphaned supervisors cannot be drained.** After an agent restart, the agent marks a live
  orphaned supervisor's record NeedsReconciliation, and NeedsReconciliation → Draining is
  rejected (`src/execution_state.rs:614-628`). A re-attachable control channel is needed for any
  step to reach existing supervisors.
- **Sampling shares the deadline thread.** `PipeControl::poll` samples NVML/sysinfo on the
  thread that reaps and enforces deadlines (`src/agent.rs:404-433`).

### Telemetry

- **NVML memory reads already use v2.** `nvml-wrapper` 0.11 `memory_info()` calls
  `nvmlDeviceGetMemoryInfo_v2` (wrapper `device.rs:2595`). Its `used` excludes the
  driver-reserved bytes, and CedeGrid reads only `total` and `used` (`src/telemetry.rs:262-282`),
  so headroom is overstated by the reserved amount (FR-009). Carry `reserved` and `free`, and
  compute unavailable memory as `total − free`.
- **Device memory and the process list are read at different times.** `src/telemetry.rs:262`
  reads device memory before the process lists (`:318-363`). An own allocation or free between
  the two reads is misattributed. Read memory before and after enumeration, and widen the
  tolerance when the reads disagree.
- **Idle external contexts look unknown.** `classify_activity` returns `Idle` only for an empty
  external PID list (`src/telemetry.rs:627-655`). NVML process-utilization samples exist only
  for non-zero utilization, so an idle external context reads as Unknown forever (D3). "Quiet"
  needs positive evidence instead: healthy queries with an advancing cursor and a complete
  inventory over a window (FR-010). Verify the NOT_FOUND versus zero-sample behavior on the
  target drivers.
- **`ContentionAware` freshness depends on any sample at all.** It needs a new sample from any
  process (`src/telemetry.rs:592-598`, `src/policy.rs:36-56`), so an idle device flips to
  InsufficientObservability, which sets `cap = 0`. Separate "observation healthy" from "fresh
  activity evidence".
- **Managed attribution is a lower bound.** RAM counts `Pss_Anon` only, and macOS returns 0. CPU
  subtracts quantization. GPU uses only per-process `usedGpuMemory`. Report lower and upper
  bounds plus an unattributed amount (FR-006).
- **The conservative busy bound overstates idle CPU.** It counts USER_HZ rounding and iowait as
  busy (`src/kernel.rs:287-311`). Expose lower and upper bounds, and don't yield on rounding.
  The failing macos-15-intel CI test `full_single_cpu_does_not_yield_from_counter_rounding`
  is the same class of bug.
- **PSI is parsed but never read by policy** (`src/kernel.rs:964-1031`). Only the delegated
  root is read, not per leaf.

### Actuation

- **Cgroup controls are written once.** They are written in `prepare()` and read back once.
  `ControlIo::write` refuses writes after the gate joins (`assert_empty()`,
  `src/cgroup.rs:394-474`), and `LaunchBackend` has no adjust method
  (`src/execution_model.rs:170-179`). `Option<u64>`/`CpuMax` cannot express `max`.
- **The agent never advertises cgroup controls.** `inspect_control` never sets
  `permitted=Some(true)`, and `configured` is forced false (`src/kernel.rs:893-899, 1131`;
  `src/agent.rs:2484-2496`). FR-029 requires fixing this.
- **Priority is set once and only for opportunistic work.** The gate applies it before exec
  (`src/supervision.rs:1252-1255`, `src/rootless.rs:29-33`). `cpu.nice` config accepts −20..=19,
  but `supervise()` accepts 0..=19 (`src/config.rs:666-669` vs `src/supervision.rs:858-861`).
  Constitution Principle II forbids negative values.
- **Lifecycle controls are Renew, Drain and AgentLost only.** Drain is irreversible and runs on
  timers (`src/supervision.rs:52-60, 144-173`). Reversible pressure states need their own state
  machine, separate from the terminal drain chain.

### Wire protocol and ledger

- **Unknown fields are handled differently per direction.** Agent→coordinator types reject
  unknown fields. `HeartbeatReply`, `Lease` and `Assignment` ignore them, but the embedded
  `LaunchRequest` rejects them (`src/protocol.rs:98-182`, `src/execution_model.rs:22-24`). So one
  new field on a job breaks an old agent's whole heartbeat. `protocol::API_VERSION` exists but is
  never sent or checked. Add a feature handshake and capability-gated optional sub-objects
  (FR-032).
- **`NodeReport` cannot express per-resource state.** It has one `expansion_allowed`, one
  `gpu_expansion_allowed` and a total `managed_budget`, and the coordinator ignores CPU/RAM
  headroom (`src/coordinator.rs:1082, 1288, 1995-2004`). FR-028 needs per-resource admission.
- **The ledger and wire treat yield as binary.** It is a `drain_requests` row and
  `Lease.drain: bool` (`src/coordinator.rs:75-76, 1549-1572`). `reservations.phase` is free TEXT
  compared by string.
  - Schema changes are made ad hoc: the `yield_count` column is added at open with no version
    bump, and triggers use `CREATE TRIGGER IF NOT EXISTS` (`src/coordinator.rs:87-99`).
  - Plan one versioned migration.
- **Yields and failures are inferred, not recorded.** The coordinator decides with
  `should_drain()` after release and with `detail.contains("revoked")`
  (`src/coordinator.rs:1417-1430`). FR-018 replaces this with a recorded cause.
- **Some recorded phases are not controllable.** `execution_allocations` maps NeedsReconciliation
  and Authorized to Running (`src/execution_state.rs:43-60`). Add a `controllable` flag or an
  `Uncertain` phase.

### Worker protocol and SDKs

- **Drain is the drain file's existence; its contents are never read.** This holds in
  `python/cedegrid/worker.py:125-131`, `npm/src/worker.ts:193` and `examples/counter.c:99-102`.
  Reusing `CEDEGRID_DRAIN_FILE` for targets would look like a full drain to every 0.2 worker.
- **The socket cannot carry pushed messages.** It handles one request per connection and never
  pushes (`src/child_supervision.rs:114-363`). Managed children lose the socket, token and
  context (`:127-138, 247`).
- **Both SDKs require context `schema_version == 2` exactly** (`worker.py:87`,
  `process.py:67`, `worker.ts:111,179`). A version bump would break 0.2 workers.
- **Candidate channel.** A separate target file with its own env var, carrying JSON with
  `{sequence, deadline_ms, targets{cpu_millicores, ram_mib, gpu_memory_mib{uuid}}}` and written
  by atomic rename.
  - Pass it to children through `GateRequest`, as the drain file is today.
  - Add optional v2 context keys only.
  - Add a context-free watcher in both SDKs.
  - An optional polled socket op for the leader, behind a capability flag.
  - Document all of it in `docs/worker-protocol.md`.
- **TypeScript has no drain or managed-child tests** (`npm/tests/`). Add them before the target
  hooks.

## Mechanism candidates

Each needs verification on the target kernel and driver.

| Candidate | Step | Notes to verify |
|---|---|---|
| cgroup v2 `cpu.weight` updated at runtime, in the delegated subtree | lower share | Reversible, and work-conserving (fits Neutral). Requires `cpu` delegated to the leaf. Re-verify leaf membership equals the verified PIDs before each write, then read back. |
| cgroup v2 `cpu.max` updated at runtime | cap | Hard cap. Can it be restored to `max`? Watch the period/quota granularity at a 500 ms cadence. |
| `SCHED_IDLE` / `SCHED_BATCH` via `sched_setscheduler` on own processes | lower share | Lowering to SCHED_IDLE is unprivileged. Can it be raised back to SCHED_OTHER without `CAP_SYS_NICE` (RLIMIT_RTPRIO / RLIMIT_NICE rules)? Is it per thread? |
| `setpriority` (nice) | lower share | One-way for unprivileged processes: RLIMIT_NICE usually forbids lowering nice again. On Linux it is per thread, so it would miss existing threads of a multi-threaded worker. Treat it as launch-time only. |
| SIGSTOP/SIGCONT duty-cycling of own processes | cap / pause (rootless fallback) | Only for verified own pidfds. Interacts with the worker's own timers and leases. GPU: SIGSTOP stops new submissions but frees no VRAM and may not preempt running kernels. |
| cgroup v2 `cgroup.freeze` | pause | Read back `cgroup.events` `frozen 1`. Frees CPU, not memory. |
| cgroup v2 `memory.high` updated at runtime | RAM cap / reclaim pressure | Throttles and reclaims; does not kill. Risk of thrashing own work, so pair it with a hint. `memory.max` stays a launch-time hard limit. |
| `oom_score_adj = 1000` on own processes | no-harm floor | An unprivileged process may raise its own value. Set it in the gate before exec so it is inherited, and verify it is inherited by managed children. |
| `ioprio_set` idle class | optional | Disk I/O is out of scope for this feature, so record only. |
| PSI `cpu.pressure` / `memory.pressure` `some avg10` | contention signal | Read per leaf and at system level. Use as corroboration, not as the sole trigger. Classify `EOPNOTSUPP` as Unsupported. |
| NVML `nvmlDeviceGetMemoryInfo_v2` (already used) | measurement | Use `reserved` and `free`, not only `used`. |
| NVML per-process `usedGpuMemory` (`_v3` process lists, already used) | attribution | Can be `NotAvailable`, for example under MIG or in containers. Context overhead attribution varies by driver. |
| NVML `nvmlDeviceGetProcessUtilization` (already used) | activity | Samples only for non-zero utilization; NOT_FOUND otherwise. The cursor is in CPU microseconds. Define "quiet" from healthy NOT_FOUND windows over a complete inventory. |
| Resource-target file plus SDK watcher | hint | See the worker protocol section above. |

## Already fixed on this branch (do not redo)

| Commit | Fix | Regression test |
|---|---|---|
| `8df6ff4` | Hidden gate/supervisor dispatch runs before the npm-launcher terminal handoff | `tests/cli.rs::npm_launcher_identity_does_not_reach_internal_gate_or_supervisor` |
| `77f66ed` | `tools/release_manifest.py` hashes without Python 3.11 `file_digest` | `tests/test_release_gate.py` |
| `54ce12c` | Supervisor diagnostics never panic on a closed stderr pipe | `agent::diagnostic_tests` |
| `6eea095` | The coordinator retries accept errors with backoff | `coordinator::tests::accept_errors_do_not_stop_the_coordinator` |
| `6c0ddc8` | Lease expiry is atomic; a superseded attempt is fenced without failing requests | `tests/coordinator.rs::lease_expiry_*` |
| `b3baf14` | Assignment-scoped requests require the owning node certificate (behavior change, in `CHANGELOG.md`) | `tests/coordinator.rs::operator_certificate_cannot_act_for_a_node_assignment` |

The D1–D6 characterization tests (`17d33cc`) still assert the defective behavior. The plan must
invert them (FR-040).

## Maintainer decisions (2026-10-24)

- The default mode is Neutral on every resource.
  - `gpu.execution_mode` and `gpu.best_effort_external_processes` stay accepted by the
    `deny_unknown_fields` loader, have no effect, and warn once at load time naming the
    replacement keys.
  - Behavior change: 0.2 nodes running `auto`/`conservative_non_sharing` move from "block
    occupied devices" to Neutral sharing. Record it in `CHANGELOG.md` and the migration notes.
- Floor policy A is the default; B (*caution*) and C (*danger*) are opt-in. Neutral memory
  policy A is the default; B (*caution*) is opt-in. Superseded on 2026-10-25 by constitution
  2.0.0: B, C and Neutral memory policy B are removed (spec FR-041, FR-042).
- Thresholds are configuration values with provisional defaults (spec table). No constants.

## Candidate configuration shape

To validate in the plan. This is TOML 1.0 with `config_version = 1` kept; all keys are
additive with serde defaults.

```toml
[contention]
default_mode = "neutral"          # yield_first | neutral | performance
max_mode = "performance"          # node cap; superseded: unset is the most aggressive node mode
floor_policy = "a"                # only "a" since 2026-10-25
neutral_memory_policy = "a"       # only "a" since 2026-10-25
acknowledge_caution = []           # "performance_mode", "small_floor" since 2026-10-25

[contention.cpu]                  # also: ram, gpu_compute, gpu_memory
mode = "neutral"                  # optional per-resource override of default_mode
yield_first_margin_percent = 25
yield_first_response_intervals = 3
performance_margin_percent = 10
neutral_convergence_s = 60
neutral_tolerance_points = 15

[contention.gpu_memory]
noise_tolerance_mib = 256
quiet_window_intervals = 20
```

- Per-job overrides belong in the job/task spec, which is JSON, as optional per-resource `mode`
  fields. They are capability-gated on the wire (see the compatibility notes).
- Floors reuse `ram.reserve_mib` / `ram.reserve_percent` and `gpu.reserve_vram_mib`
  (superseded: `gpu.reserve_vram_percent` was added, spec FR-023).
- `gpu.scale_up_cooldown_ms` stays for GPU. CPU and RAM get their own cooldown keys.

## Setup implementation candidates

### Terminal UI

- **No new dependency.** Raw mode through `termios` (via `libc`, already a dependency) plus ANSI
  escapes. Restore the terminal from a drop guard and on SIGINT/SIGTERM/SIGTSTP. Detect a TTY
  with `isatty` on stdin and stdout.
- **Or add `crossterm`.** It brings cross-platform key events and raw mode. That is a new
  dependency, so it needs written justification (constitution, CLAUDE.md). It weighs against the
  rootless, minimal-dependency stance.

### Color

- No color when `NO_COLOR` is non-empty, output is not a TTY, or `TERM=dumb`.
- Labels `[caution]` always print (`[danger]` was removed on 2026-10-25).
- Yellow is SGR 33 and red is SGR 31; defaults use no SGR.

### Writing the configuration

- **Comment-preserving edits** need a TOML editing library (`toml_edit`; also a new
  dependency), or setup owns a delimited `[contention]` table that it rewrites textually and
  leaves the rest of the file byte-for-byte.
- Validate the result by loading it through `Config::load` before an atomic rename.

### Command-line flags

- One flag per choice, for example `--default-mode`, `--max-mode`, `--floor-policy` and
  `--neutral-memory-policy`, plus per-resource overrides.
- `--acknowledge-caution performance_mode|small_floor` is required for caution options.
- `--non-interactive` forces flag-only mode even on a TTY.
- `--json` gives machine-readable output.
- The exit status is non-zero when a required choice or acknowledgement is missing.

### Scope

- Agent docs and MCP reuse these flags. See `specs/002-agent-integration`.
- Windows: client settings only, because nodes don't run there.

## Mapping questions for the plan

- Precise per-mode semantics of each legacy `gpu.execution_mode` value for the deprecation
  message and migration notes (they are ignored, but operators need to know what changes).
- How fair share is computed per principal from visible process ownership. What CedeGrid does
  when process ownership is hidden.
- A hardware validation plan for each provisional threshold in the spec, and how measured values
  replace them.
- Where per-resource cooldowns live (a new section, or the resource sections).
- How existing D1–D6 characterization tests (`tests/policy.rs`, `known_defect_*`) become
  regression tests (FR-040).
- An actuation audit hook for SC-005, recording every signal, priority and control write with
  its target identity, so that tests can prove that no external process is ever touched.
