# Guarantee boundaries

## Replayable burst storage

`storage_profile: burst_replay_delete_extra` is an explicit weaker local-storage
contract for authenticated opportunistic agents. It is disabled by default and
uses DELETE journaling, EXTRA synchronization, foreign keys, private-path checks,
locking checks and persisted schema 4 / `replayable_local` assurance. Recognized
FUSE storage may be admitted under this profile; this does not qualify its backing
namespace durability. I/O and locking errors remain fatal. Use only replay-safe
opportunistic work that does not require `storage.durable_local`.

The coordinator and its accepted result/checkpoint authority require a strict
profile. A replayable agent opens a fresh authenticated coordinator session before
replacing any local state. The coordinator fences previous sessions and attempts;
the agent preserves the previous directory in a private sibling quarantine and
imports every authoritative outstanding reservation with its original generation,
resource charge and preparation identity. Its first heartbeat must account for all
outstanding reservations. Stale sessions cannot renew, upload or publish.

Local progress, uncommitted uploads/spools and resume files may be lost across a
host or filesystem failure. Only a matching committed coordinator receipt confirms
final publication. Quarantine is retained for inspection; there is no automatic
backup restore, live database recreation, journal-error bypass or blind state reset.
Detected namespace loss or a missing active attempt stops the agent. Full detection
of every possible in-place storage rollback is not claimed.

Recovery never adopts or signals a stored numeric PID. Release requires verified
original-process absence and GPU release observations. Missing preparation,
unknown survivors or lost mediated-child inventories retain the reservation;
same-boot leader absence cannot prove an unknown child family has exited. A new
node session alone does not free capacity. Rejoin can remain blocked until absence
is established. Strict nodes advertise `storage.durable_local`; replayable nodes
advertise `storage.replayable_local`.

## GPU sharing

GPU sharing is best-effort opportunistic sharing. It guarantees neither
interference-free external execution nor a fixed level of external performance.

The policy does not equate 80% utilization, two polls, or an external PID's presence
with proven compute contention. NVIDIA's general utilization is the fraction of a
sampling period with at least one kernel executing. Products can have different
sample periods; reading every 500 ms need not produce independent samples.
[NVIDIA utilization definition](https://docs.nvidia.com/deploy/nvml-api/structnvmlUtilization__t.html)

- Reserve violations and explicit node drain request immediate reduction. Observe-only
  records the decision; an explicitly enabled agent drains verified allocations.
- New external workload identities or increasing external/unattributed VRAM stop
  expansion and cause a protective reduction.
- Reliable external compute activity causes additional policy-directed reduction,
  even if aggregate utilization is low.
- Unsupported, failed, absent-fresh-sample, or otherwise uncertain activity never
  becomes evidence of idleness. Default modes block expansion and yield conservatively.
  The explicit occupied mode below accepts a narrower best-effort contract.
- Stable headroom permits incremental expansion only after the configured cooldown.
  The coordinator reserves each launch before dispatch and the agent rechecks locally.

GPU capability modes make these limits explicit. Auto selects only fresh activity
telemetry or conservative non-sharing behavior on a reliably observed empty device.
Explicit contention-aware mode requires the former; non-sharing mode freezes
expansion and drains affected opportunistic work when external contexts appear.
Unknown basic observations block the device. Auto/non-sharing GPU assignments must
be opportunistic because full yield conflicts with guaranteed continuity; CPU-only
guaranteed work remains independent. The coordinator rejects incompatible placement
and the agent verifies the contract before authorization. An empty-device pass is
not evidence of occupied-device sharing. See [GPU operation](coordinator-work.md#gpu-capability-modes).

All process snapshots and attributed allocation usage are trusted observations from
a collector/registry, not arbitrary workload assertions in the execution path.
Replay accepts synthetic observations purely for testing. GPU memory is accounted
per UUID; memory is never pooled across devices.

## Resource accounting

CPU uses millicores of logical CPU time: 1,000 millicores equals one logical CPU.
Physical-core reserves are mapped conservatively using available topology.
For each running or draining allocation, charge the greater of its request and
observed usage once. A pending allocation consumes its full reservation.
Subtract attributed managed usage from total system usage before classifying the
remaining usage as external; do not subtract both the usage and the entire reservation
again from free capacity. Unknown observations prohibit optimistic admission.

Automatic yielding selects only opportunistic allocations. Guaranteed allocations
remain fully charged, including pending and uncertain reservations, and cannot
stand in for an opportunistic allocation that must yield. Explicit node drain and
operator-envelope violations retain their existing all-allocation stop semantics.
Policy recordings without an allocation class retain the legacy opportunistic
default; live observation copies the required class from the durable execution record.

A drain decision does not release a reservation. The durable execution ledger keeps it
charged until process exit and resource-release observations confirm the release.
CPU/RAM reserves without applied cgroup controls are scheduling policies, not kernel
enforcement. Available, configured, and applied capabilities are distinct facts.

## Process-control contract

Trusted commands must remain inside the supervisor's lifecycle and must not create
escaping daemons. Workloads requiring that behavior need an explicit adapter or
a supported containment backend. The system is not a sandbox for hostile programs.

The Linux supervisor acquires a pidfd before any child reaping, verifies boot ID,
start time, assignment ID and generation, and uses the stable handle for signaling
and exit observation. On failure or unsupported platforms it can supervise only its
exclusively owned, unreaped direct child, with weaker guarantees explicitly recorded.
It refuses a required stable handle if unavailable. This fallback depends on no
competing reaper or SIGCHLD changes. Restart does not trust a stored numeric PID:
there is no automatic adoption/signaling path. A leader pidfd does not identify or
contain descendants. Rootless execution supports single-process commands and opt-in
SDK-mediated direct children. Each mediated child has its own durable identity,
verified handle and launch barrier. The shared parent resource envelope covers the
whole family, with each live usage counted once. Arbitrary forked descendants are
unsupported; uncertainty in any registered child prevents family release.
[Linux pidfd lifetime and reaping requirements](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)

Preparation reserves capacity, starts only the trusted manager gate, verifies
identity and backend membership/control readbacks, persists Prepared and Authorized,
and then sends EXEC. EOF before authorization exits the gate. Partial preparation
failures preserve applied/failed evidence and require confirmed cleanup. User code
cannot run merely because spawning the gate succeeded.

Cgroup v2 operates only within a configured delegated subtree using pinned directory
handles. CPU/memory settings are explicit and read back before EXEC. `cpu.weight`
is a relative hierarchical weight: it is neither a CPU percentage nor a guaranteed
allocation. `cpu.max` is a separate opt-in bandwidth cap; there is no default quota.
Memory controls do not enforce GPU VRAM. Ancestor limits and permitted CPU sets
remain applicable. This increment removes only a confirmed-empty owned cgroup;
nonempty cleanup and cgroup.kill are unavailable pending stronger ownership proof.
[Versioned Linux 6.6 cgroup v2 interfaces](https://docs.kernel.org/6.6/admin-guide/cgroup-v2.html)

PSI reports system or cgroup scope, timestamps, sample interval, availability and
freshness. System pressure cannot identify the workload causing it. Unknown, stale,
inaccessible or unsupported readings remain unknown. PSI is observation-only in
this increment, not an unexplained drain threshold.
[Linux PSI definitions](https://docs.kernel.org/accounting/psi.html)

## Deadlines and leases

These are distinct milestones, not a single resource-release guarantee:

1. Drain deadline: wait for cooperative completion (`drain_timeout_ms`, default 3,000).
2. Termination deadline: after draining, wait through SIGTERM grace
   (`term_grace_ms`, default 2,000), then attempt SIGKILL.
3. Release confirmation: independently confirm exit and returned resources;
   `execution.release_confirm_timeout_ms` bounds forced-cleanup observation, not
   kernel resource-return latency.

The defaults reach the forced termination stage approximately five seconds after
local detection, or fifteen seconds after the last valid lease renewal when a
10-second opportunistic lease expires. Sampling, scheduling, actual exit, and
release confirmation add separate latency. No setting promises VRAM is returned in
three seconds. Hung kernels/drivers can prevent timely release.

A living independent supervisor can retain deadlines without an agent. The local
CLI currently combines the caller and supervisor, and has no remote channel. The
lifecycle reducer distinguishes coordinator disconnection (existing lease unchanged),
agent failure (opportunistic drain), and supervisor failure (needs reconciliation).
A dead supervisor cannot enforce deadlines after its own death; no external reaper
is claimed. Any uncertain surviving allocation remains reserved until verified
reconciliation, even if its task is replay-safe.
An agent heartbeat alone cannot renew a coordinator-issued remote lease. Lease
renewals carry assignment generation and fresh coordinator authorization; receiving
stale messages cannot extend them. Enforce local durations with monotonic clocks,
not wall-clock timestamps. Reboot invalidates process and lease identity.

Guaranteed allocations survive coordinator loss. Opportunistic allocations drain
when authorization expires. This distinction is an explicit allocation contract,
not inferred from a node's name or network location.

## Results, retries, and storage

Execution may happen more than once. Only one final result per task is accepted.
An exact retransmission of an accepted attempt/receipt returns the same successful
receipt; stale attempts and conflicting results cannot replace it. This does not
deduplicate external side effects. Automatic retry requires an explicit replay-safe
workload declaration. Uncertain unsafe work enters `needs_reconciliation`.

Authoritative and strict-profile SQLite state must reside on an actual supported local durable filesystem. A path
under a home directory proves nothing about the filesystem. Inspect the target
filesystem before opening it. The default `wal_full` profile requires WAL with
synchronous=FULL; the explicit `delete_extra` profile requires rollback journaling
with synchronous=EXTRA. Both require SQLite >=3.51.3, enabled foreign keys and
successful effective-setting readbacks. The persisted profile is immutable across
ordinary opens, migration and recovery. Neither profile bypasses filesystem
qualification or turns a no-op directory synchronization into a real barrier.
The project deliberately rejects older versions rather than trusting an unverified
backport. [SQLite WAL requirements and fix](https://sqlite.org/wal.html).

Bounded linked-library process-crash tests establish only their measured recovery
scope. They do not prove namespace persistence across host power loss or filesystem
daemon failure, and they never enable an otherwise unsupported storage root.
See [profile operations and diagnostics](coordinator-work.md#storage-profiles-and-qualification).

The original ledger remains the result-fencing foundation. The implemented artifact
service follows this order before calling result acceptance:

1. Upload to an attempt-specific temporary file on the target local filesystem.
2. Verify checksum and synchronize the file contents.
3. Publish atomically without overwriting a conflicting artifact.
4. Synchronize the parent directory containing the published entry.
5. Commit the publication/receipt in the database, then send a success ACK.

Successful uploads retain metadata and staging bytes so duplicate commit requests
or a lost success ACK can replay safely. Initial publication hardlinks staging to
the blob; an already-present identical blob is verified and reused. These committed
records are distinct from unfinished uploads; an empty upload directory is not a
cleanup invariant. Explicit abort removes the failed upload metadata and staging
pair. This describes the existing artifact implementation, not a new retention-
policy change.

Recovery must handle an orphan published file without a database record. File fsync
alone does not make the directory entry durable.
[Linux fsync semantics](https://man7.org/linux/man-pages/man2/fsync.2.html)

Named input artifacts are immutable, checksummed coordinator publications. Agents
verify them before execution and may reuse a read-only, hash-verified local cache.
Only acknowledged, released attempt bytes are eligible for local reclamation;
descriptors and receipts remain. Uncertain or unpublished data cannot be discarded
to fabricate spare disk capacity. Offline backup/restore preserves the durable
ledger and fences surviving reservations; its point-in-time RPO and source-retirement
requirement are described in [coordinator operations](coordinator-work.md).
