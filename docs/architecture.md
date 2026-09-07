# Architecture and delivery sequence

## Separation of concerns

The Rust library defines configuration, snapshots, resource accounting, policy
decisions, and durable task/attempt state. The CLI collects telemetry, explains
those decisions, and runs explicitly authorized coordinator, agent and supervised workload services. No core API depends on SSH, a machine name, a particular subnet,
a competition, or a Python machine-learning framework.

Strict storage profiles use the same SQLite state abstraction and filesystem gate. The
default `wal_full` profile retains WAL with FULL synchronization; the explicit
`delete_extra` profile uses rollback journaling with EXTRA synchronization. The
profile is persisted and checked at every open, including migration, agent and
supervisor state, backup and recovery. Neither choice qualifies an unsupported
filesystem. Linked-library qualification tests record process-crash compatibility
separately from namespace durability. The explicit `burst_replay_delete_extra` profile
adds non-authoritative replayable agent storage, session fencing, quarantine and
coordinator-backed reservation reconstruction. It cannot host the coordinator or
claim durable local results. See [storage operations](coordinator-work.md#storage-profiles-and-qualification).

CPU/RAM collection uses sysinfo; GPU collection is an optional NVML vendor adapter. Future
collectors can produce the same versioned snapshot contract for AMD, Intel, other
operating systems, or administrator-provided telemetry. Unknown capability remains
unknown until an adapter supplies defensible evidence.

GPU configuration selects `auto`, `contention_aware`, or
`conservative_non_sharing`, with explicit `best_effort_occupied` for authorized
external identities under the narrower [GPU contract](coordinator-work.md#explicit-occupied-gpu-mode). The collector records exact NVML status, driver cursor
and sample timestamp in microseconds, observation age, and the resulting capability.
Auto uses fresh activity evidence when it is available; otherwise it can use a
reliably observed empty device. Non-sharing mode admits only an empty external
inventory and drains all opportunistic work on the affected device when an external
context appears. Missing basic inventory/memory observation blocks that device.
Explicit contention-aware mode does not silently fall back, and occupied devices
with unknown activity remain blocked in default modes. These are admission and reactive protection
policies, not VRAM partitions or guarantees against every external allocation race.

Non-sharing full yield conflicts with guaranteed GPU continuity. Auto/non-sharing
GPU work must therefore be opportunistic; CPU-only guaranteed work is unaffected.
The coordinator receives a separate GPU-guaranteed eligibility flag, with absent
older fields interpreted as false, and the agent rechecks the contract before
preparation. Local `supervise` remains CPU-only; GPU execution uses the continuously
observed agent path. See [GPU operation](coordinator-work.md#gpu-capability-modes).

The implemented version-one coordinator-agent API uses HTTPS mutual TLS with
certificate-bound node/operator identities. It carries stable task/assignment IDs,
attempt generations, allocation class, durable preparation evidence and bounded
leases. The coordinator extends the original SQLite task/attempt ledger. Local
agents own execution journals; independent supervisors enforce authorized deadlines
and local policy while alive. Supervisor death retains uncertain reservations.
SSH is only an optional operator bootstrap/tunnel mechanism, never job semantics.

The Python SDK implements readiness, draining, artifact/checkpoint publication,
resume metadata and completion without embedding scheduler policy. Workloads
receive private local lifecycle descriptors, not coordinator operator credentials.
The separate Kaggriculture adapter resides in the application checkout and uses
the real simulator, dataset builder, learner and exporter. Generic core code has
no application imports or model knowledge. See [SDK contracts](sdk-adapter-work.md).

## Milestone 1 — delivered boundary

- Strict configuration and a pure, deterministic policy engine.
- Capability preflight, live observe-only collection, and synthetic trace replay.
- Local durable observations and the task/attempt result acceptance ledger.
- Failure-oriented tests and documented safety/portability limits.

The original milestone remains intact. New services reuse configuration, policy,
SQLite, attempt fencing and execution backends; see [service contracts](coordinator-work.md)
and [execution evidence](execution-work.md). Implementation presence is distinct
from Linux-native or two-server acceptance.

## Completion and validation order

1. Preserve source/configuration manifests and home-only deployment inputs.
2. Native diagnostics, observe-only and launch/cleanup/reconciliation faults.
3. Actual coordinator-agent CPU scheduling, pools and local safety behavior.
4. Durable artifacts and SDK, then bounded real application command and cooperative
   self-play-to-learning/checkpoint continuation on the anchor.
5. Authorized two-node useful work, CPU/GPU contention, full opportunistic drain,
   anchor continuity, rejoin and distributed recovery.
6. Bounded stress with reviewed evidence and cleanup. The user replaced required
   24-hour acceptance with this stage; the long-soak harness is optional and unrun.

The precise status and remaining commands live in [validation summary](validation.md) and
[validation matrix](validation.md). A blocked shared-server gate does not stop
independent implementation or authorized anchor validation. The Mac is a development
host; cross-compilation and Mac tests do not establish Linux runtime behavior.

All deployment-created state, environment, cache, temporary and output paths must
remain inside the approved user home. A home path is not evidence of a supported
filesystem. FUSE/mergerfs remains rejected by the current strict production
preflight. Changing the SQLite journal mode or passing process-crash diagnostics
does not supply a missing directory synchronization mechanism. No underlying mount
path, global configuration change or privilege escalation is permitted.

Optional cgroup controls operate only inside an explicitly delegated authorized
subtree. Relative cpu.weight has no percentage or allocation guarantee; ancestor
limits and competing placement affect its behavior. Memory controls do not enforce
GPU VRAM. Configuration and actual enforcement are reported separately.

The source license is Apache-2.0. Source034 is authorized for GitHub source
publication with remaining physical deployment gates disclosed. A published source
revision is not operational acceptance. Persistent autostart requires separate
explicit approval.

## Deferred kernel research

The coordinator and policy remain in user space. No custom kernel module, CPU
scheduler replacement, sched_ext scheduler, BPF program, global sysctl change,
kernel upgrade, or boot change is part of this release. Existing `LaunchBackend`
and telemetry contracts provide a narrow extension boundary.

eBPF is a later observation-only experiment only if ordinary measurements cannot
answer a specific performance question. sched_ext requires evidence of a scheduler
bottleneck, an explicit optimization objective, compatible kernel and permissions,
an administrator-approved isolated test environment, regression tests, and a tested
rollback. Credentials are not deployment authorization.
