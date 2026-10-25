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

- A floor breach, observed free VRAM below the device floor, and explicit node drain lead
  to yielding through the ladder. Observe-only records the decision; an explicitly
  enabled agent acts only on verified allocations.
- New external workload identities and external or unattributed VRAM growth beyond the
  noise tolerance block GPU admission on that device. Running work yields only for a real
  floor breach or own usage beyond the budget.
- External compute activity blocks GPU admission on that device under Yield First;
  Neutral and Performance admit and share compute. It never drains own work by itself.
- Unsupported, failed, absent-fresh-sample, or otherwise uncertain activity never
  becomes evidence of idleness: Yield First admits nothing on that device, Neutral and
  Performance count it as one principal, the current steps hold, and escalation waits
  for the bounded unknown window. Missing basic observation blocks the device.
- Stable headroom permits incremental expansion only after the configured cooldown.
  The coordinator reserves each launch before dispatch, only where every requested
  resource's admission is open, and the agent rechecks locally. Held or blocked
  admission never preempts other work, nor do launch slots an agent holds back while it
  paces its growth; one scheduling pass never lets two waiting tasks claim the same victim.

GPU admission follows each device's GPU compute mode, by one rule the policy and the
launch recheck share: Yield First admits only on a quiet device; Neutral and Performance
admit on quiet or active devices and under activity of unknown amount, which counts as
one principal; unknown GPU memory blocks; and `contention.never_share_occupied_gpus`
refuses new work on a device with external contexts or an unknown process list. Own work
already running on a device that becomes occupied is not drained for that alone. The
launch recheck also refuses a request that would leave less than the device's floor
free. Missing GPU observation holds the current steps as an unknown measurement instead
of yielding fully, so Guaranteed GPU work needs no special contract. Under external
compute activity, own GPU work gets a compute share by pausing it for part of each
period: the period is one monitor interval (500 ms by default) and the minimum duty 5%,
and kernels already submitted keep running, so the effective granularity is the period
plus the longest kernel. A process with a CPU cap and a GPU compute share runs one cycle
at the smaller duty; Guaranteed work takes no GPU compute step. External compute is
measured when fresh per-process samples, summed over every process on the device, are
within `contention.gpu_compute.attribution_tolerance_percent` (15) points of device
utilization; a measured amount takes the CPU paths, with external processes grouped into
principals by UID as on CPU and device time nobody accounts for counted as one more
external principal while an external process is listed, so own work keeps capacity minus
that amount minus the mode's margin under Performance, and its fair share or a learned
satisfied demand under Neutral. A device leaves the measured path at once and returns
only after `quiet_window_intervals` consecutive measured intervals. Unsupported queries,
stale samples, an incomplete process list or MPS leave it activity of unknown amount,
one principal, under which Neutral and Performance hold the fair share: once the
supervisor reports a duty cycle applied, own work counts as holding at most that duty,
so the cap at the share is met and does not escalate. Unknown activity from a telemetry
failure or a missing observation, and a duty cycle not reported applied, first hold the
steps and escalate after the unknown window. Each device's held-back breakdown says
which path it took and why. An empty-device pass is not evidence of occupied-device
sharing. See
[GPU operation](coordinator-work.md#gpu-modes).

All process snapshots and attributed allocation usage are trusted observations from
a collector/registry, not arbitrary workload assertions in the execution path.
Replay accepts synthetic observations purely for testing. GPU memory is accounted
per UUID; memory is never pooled across devices.

## Resource accounting

CPU uses millicores of logical CPU time: 1,000 millicores equals one logical CPU.
Physical-core reserves are mapped conservatively using available topology.
For admission, each running or draining allocation is charged the greater of its request
and observed usage once, except that running opportunistic CPU is charged at its p95
usage over `contention.cpu.admission_usage_window_s` (60 s) once it has a full window of
samples, so work in its startup phase is charged its request: CPU is compressible, so
caps and the kernel share it if everyone spikes. RAM admission and the agent's worker
limit bound how many jobs this admits. The reported schedulable CPU budget adds what
running opportunistic work is charged above that usage, so the coordinator and the
launch check, which charge requests, admit the same. A pending allocation consumes its
full reservation. Yielding judges running work on what it holds: CPU and RAM usage, and
GPU memory usage for work that takes a hint, else its charge, since its growth cannot be
bounded.
Subtract attributed managed usage from total system usage before classifying the
remaining usage as external; do not subtract both the usage and the entire reservation
again from free capacity. Unknown observations prohibit optimistic admission.

Each resource runs in a contention mode, and for every resource and observation the
targets are ordered: Yield First ≤ Neutral ≤ Performance. Capacity no workload wants
does not sit idle: a reserve is held back only for how slowly its resource is reclaimed,
so CPU and GPU compute keep small margins and memory keeps the floors. CPU and GPU
compute are *contended* when they are saturated (busy at or above `saturation_percent`,
98% by default, of capacity) and an external principal uses at least
`principal_min_percent` of them without its demand being known to be satisfied, or when
external activity of unknown amount exists. Principals are the account CedeGrid runs as,
whatever its UID, and users at or above `contention.uid_min` (1000) that have used at
least `principal_min_percent` for `contention.principal_window_intervals` (4)
observations; they leave after `contention.principal_leave_s` (60 s) below it, and
meanwhile their unused share goes to the others. Until a user joins, its usage counts
toward one unattributed principal, together with processes whose owner cannot be read,
so contention is seen at once. A host that mounts `/proc` with `hidepid=2` always takes
this path for other accounts. Usage by lower UIDs is background demand: it comes off the
capacity and never contends.

Under saturation, a contending principal whose unknown demand is below one margin step
is probed: own work steps down by the mode's margin per response window, and a principal
whose usage does not grow into the freed capacity is recorded as satisfied at its usage
plus the contention floor. It then no longer contends until its usage exceeds that
demand. A larger demand gets the fair share at once and is then watched for one response
window: if the host has capacity to spare and its usage stayed within its baseline plus
the contention floor, it is recorded the same way, so own work scales back up at the
normal rate and stays up; a recorded user that returns rejoins at once. Activity of
unknown amount is never recorded. On contended compute, Yield First pauses opportunistic
work, and after `pause_bound_s` (10 s) keeps it at the 5% minimum duty while the
contention lasts; while a small demand is probed it steps down with the probe instead.
While own work holds more than capacity minus Yield First's margin, saturation may be
its own doing, so Yield First first caps it to the margin within the fair share and
pauses only if the contention remains. Neutral holds the max-min fair share. Performance
keeps the larger of the fair share and capacity minus observed external usage minus its
margin: it gives back what external demand takes and never goes below the fair share;
activity of unknown amount holds it to the fair share. Otherwise Yield First keeps 15%
of capacity free, Neutral 5% and Performance 2%; the margins are validated as Yield
First ≥ Neutral ≥ Performance, and Neutral and Performance also differ in how fast they
scale up (10 s and 2 s per step). On memory, Yield First keeps 20% free beyond the floor
and the other modes only the floor. The CPU idle reserve is the larger of
`cpu.reserve_physical_cores` (default 0) and the mode's margin; the two never stack. The
margins are configurable per resource. In every mode, contended CPU returns within 2
monitor intervals except in Performance, which follows what external work takes: the
first step applies at once. While a resource is not contended, a held cap or hint rises
again toward the mode's target at the mode's scale-up rate. A Guaranteed allocation's
CPU is never capped below its request, by the policy or by its supervisor.

CPU and RAM each wait for their own cooldown, counted from the time each became stable:
`contention.cpu.scale_up_cooldown_ms` and `contention.ram.scale_up_cooldown_ms`. Unset,
each is its mode's scale-up interval: 30 s in Yield First, 10 s in Neutral and 2 s in
Performance. Each resource's admission opens on its own, and placement needs every
requested resource open; every launch reserves CPU and RAM. GPU memory waits for
`contention.gpu_memory.scale_up_cooldown_ms`, else a set legacy
`gpu.scale_up_cooldown_ms`, else its mode's interval. On RAM and GPU memory, the n-th
reclaim episode (a floor breach or own usage beyond the budget) within
`memory_peak_window_s` adds `reclaim_backoff_ms` × 2^(n−1) to the cooldown, at most
`reclaim_backoff_max_ms` (1 s doubling to 30 s by default); a peak window without one
resets it. Admission headroom for memory subtracts the peak of external usage over
`memory_peak_window_s` (300 s), including the memory of quiet GPU contexts. Each
external GPU context and each external process holding at least 1% of RAM keeps its own
peak while it lives, identified by PID and start time, and its peak is dropped when it
exits; other external usage keeps one peak. Yielding follows current usage.


Each running allocation's resource-target file also carries its allowance: what it may
grow to now without causing a step, per CPU, RAM and GPU memory. It is its usage plus an
equal part of the room under the mode's allowance after pending reservations, at most a
current cap or hint target and the operator envelope, never below a Guaranteed request,
and only current usage while a measurement is unknown. If every allocation grows to its
allowance at once, the totals stay within the mode's allowance. A fall publishes at once;
a rise at most one step, the mode's margin of capacity, per scale-up interval.

Memory contention starts with a resource target for workers that read targets
(see [worker protocol](worker-protocol.md#resource-targets)); usage still above the
target at the hint deadline, 5 s by default, leads to a drain. Each supervisor reports
whether its worker has acknowledged the target file (hint-ready) and whether it
observes the drain file; only hint-ready workers are hinted, and the others are
drained without the hint. When free memory falls below `contention.critical_floor_percent`
(50%) of a floor, never below the minimum floor, the breach is critical: hint-ready
workers get `contention.critical_hint_deadline_s` (1 s), others are drained at once,
and workers that observe neither file are sent TERM without the drain-file wait.
Each hint carries the deadline the policy enforces for it. A hint taken for a floor
breach is lifted only after free memory has stayed above the floor by what the hint had
to free, or `noise_tolerance_mib` if larger, for the calm window plus the episode's
reclaim backoff, so work that regrows into the floor backs off. The provisional reclaim
target, not yet validated on hardware, is hint deadline + drain timeout + termination
grace + one interval until own work that has not released the memory is terminated
(10.5 s by default), 15.5 s for Guaranteed memory alone on the resource, and 6.5 s on
the critical path. Automatic yielding releases opportunistic allocations first. Guaranteed allocations
remain fully charged, including pending and uncertain reservations. They may be
lowered in share or capped, are never paused, and their memory is drained only as the
last step of floor policy A. Explicit node drain retains its all-allocation stop semantics.

When own work exceeds the operator's node envelope, the policy restores the envelope per
resource through the yield ladder, releasing opportunistic work first. Guaranteed work is
drained for it only as floor policy A allows, and a breach never stops every allocation at
once. An exhausted node spool budget blocks admission and is reported; nothing is drained
because of it.

No-harm floors hold in every mode: `ram.reserve_mib`/`ram.reserve_percent` of host RAM
and `gpu.reserve_vram_mib`/`gpu.reserve_vram_percent` per GPU stay free, by default
max(8 GiB, 5% of RAM) and max(1 GiB, 5% of the device), never less than 512 MiB of RAM
and 512 MiB per GPU (provisional); a fixed 3 GiB per GPU applies only when configured. A
floor is breached when observed free memory falls below it. For RAM, and for GPU work
that takes a hint, a charge beyond the budget only blocks admission and never hints,
drains or terminates running work; GPU work without a hint channel is judged at its
charge, since its growth cannot be bounded. A smaller setting is raised to the minimum
with a load-time warning. `doctor` shows every floor as it applies on the host, and a
raised floor carries the reason code `small_floor_raised`. Floors always come before the
fair share. A breached floor blocks admission on that resource and releases
opportunistic work holding it. Floor policy A, the only floor policy, then drains
Guaranteed work that holds the memory, as the last step: after
`contention.guaranteed_hint_deadline_s` (5 s) and, for RAM, after the breach has lasted
`contention.ram.guaranteed_breach_window_s` (10 s). GPU memory breaches have no window.
A Guaranteed allocation that is not hint-ready waits only for the window, and on the
critical path the window is skipped. On Linux, every own process, Guaranteed included,
marks itself as a preferred out-of-memory victim (`oom_score_adj` 1000) before it runs.
Other platforms offer no unprivileged equivalent and claim none.

No option puts another workload at risk of an out-of-memory kill, swap thrash or a
failed GPU allocation. The former floor policies `b` and `c` and Neutral memory policy
`b` were removed, and a configuration that sets them fails to load with a message that
names the replacement settings. Every configuration key is classified as safe, bounded
or caution. Four options are marked *caution* because they let own work hold more
capacity within the floors: Performance mode, a floor below twice its minimum, a margin
below its default, and a detection setting beyond its default (`saturation_percent`,
`principal_min_percent`, `noise_tolerance_mib` or `contention.uid_min` above it,
`memory_peak_window_s` or `quiet_window_intervals` below it). Performance needs
`contention.acknowledge_caution = ["performance_mode"]` to load, a lower margin needs
`"reduced_margin"` and a higher detection threshold needs `"relaxed_detection"`. A floor
below twice its minimum without `"small_floor"` loads, so 0.2 files keep loading, but is
raised to twice the minimum with a warning, given at load or, for a floor a percentage
sets, when the agent first samples the host; it applies as configured only once
confirmed. `doctor` and node reports list active caution options (`status` will show
them from the node reports, T112), and `doctor` lists the keys and values each covers.
`noise_tolerance_mib` is at most 1024 MiB, and the agent refuses to start when it
exceeds half of any GPU's floor. Settings that lengthen reclaim are bounded: monitor
intervals at most 10 s, kernel freshness at most 20 intervals, GPU sample age, drain
timeout and termination grace at most 60 s. Decisions and node reports show, per
resource, how much is held back and why. Without `contention.max_mode`, a job may
request at most the node's most aggressive configured mode, so no job runs in
Performance unless the operator chose it. Policy recordings without an allocation class
retain the legacy opportunistic default; live observation copies the required class from
the durable execution record.

The legacy `gpu.execution_mode` is read at load time, with a warning on stderr that
names the replacement: `contention_aware` and `best_effort_occupied` set GPU compute and
GPU memory to Neutral unless their mode is set, `conservative_non_sharing` sets
`contention.never_share_occupied_gpus = true`, and `auto` changes nothing. A non-empty
`gpu.best_effort_external_processes` is reported as unused. `doctor` shows the
interpretation and every load-time warning. `gpu.execution_mode` has no other effect.

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

Cancelling a job is terminal. Its queued tasks become `cancelled` at once, and running tasks
become `cancelled` when their drain releases them. A cancelled job's tasks are never retried or
assigned again, including after an agent restart or a replaced replay session. Interrupted unsafe
work still enters `needs_reconciliation` so its side effects stay visible.

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
