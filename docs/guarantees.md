# Guarantee boundaries

CedeGrid lets own work use a shared server's spare CPU, RAM and GPU capacity, most usefully
where other people's processes already run and it is unclear how much to take, so that no
capacity sits idle. Own work runs as fast as it can, efficiently, and the contention mode says
how much it gives back when others need capacity. Performance is the default; Yield First and
Neutral give back more, and Yield First is one option among them. Each section below states
what CedeGrid guarantees and where that ends.

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
plus the longest kernel. A changed CPU cap starts at its target over the family's last
measured natural rate, so it holds from its first period. On a delegated cgroup with
`cgroup.freeze` authorized, the off phase freezes the workload's leaf, confirmed through
`cgroup.events` within the off phase, and never thaws a freeze a pause holds; otherwise, or
after a refused freeze, it stops the verified processes, as without a delegated cgroup. Where
the CPU cap is also a duty cycle, the process runs one cycle at the smaller duty; a CPU cap
through `cpu.max` runs beside the GPU share. Guaranteed work takes no GPU compute step. External compute is
measured when fresh per-process samples, summed over every process on the device, are
within `contention.gpu_compute.attribution_tolerance_percent` (15) points of device
utilization; a measured amount takes the CPU paths, with external processes grouped into
principals by UID as on CPU and device time nobody accounts for counted as one more
external principal while an external process is listed, so own work keeps capacity minus
that amount minus the mode's margin under Performance, and its fair share or a learned
satisfied demand under Neutral. A device leaves the measured path at once and returns
only after `quiet_window_intervals` consecutive measured intervals. An interval in which
both process queries succeed, every listed process is an own verified process checked
through its pidfd, no MPS is present, device utilization is known and memory attributed
to no listed process has not grown beyond the last measured, own-only or empty interval
plus the noise tolerance is measured at once with no external compute, so a new own
process neither restarts the window nor turns the device Unknown. A context of a user ID
below `contention.uid_min`, such as a display server's, is background rather than activity
while per-process utilization is readable for the device (the query succeeds or reports no
samples) and its own utilization stays below `principal_min_percent`: it is neither
external compute nor unknown activity, its share of device time is nobody's, and its
memory still counts as external usage, as `never_share_occupied_gpus` still counts the
context. Where per-process utilization is not readable it stays unknown activity. Unsupported queries,
stale samples, an incomplete process list or MPS leave it activity of unknown amount,
one principal, under which Neutral and Performance hold the fair share: once the
supervisor reports a duty cycle applied, own work counts as holding at most that duty,
at the larger of the target the supervisor reports applied and the current one, so the
cap at the share is met and does not escalate. Unknown activity from a telemetry failure
or a missing observation, and a duty cycle not reported applied, first hold the steps
and escalate after the unknown window. Each device's held-back breakdown says which path
it took and why. An empty-device pass is not evidence of occupied-device sharing. See
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
caps and the kernel share it if everyone spikes. While a pause, or a cap or hint its usage
reaches, holds an allocation back, it is charged the greater of its request and its p95
usage from before the step, with no credit, so usage a cap holds back never becomes room
for more work; that charge closes admission but does not by itself mark own usage as beyond
the budget. RAM admission and the agent's worker
limit bound how many jobs this admits. The reported schedulable CPU budget adds what
running opportunistic work is charged above that usage, so the coordinator and the
launch check, which charge requests, admit the same. A pending allocation consumes its
full reservation. A reservation the policy cancels before it is authorized is released: its
supervisor gets the drain before any authorization, and no authorization is requested for
it. A CPU drain planned while an allocation was pending is planned again as running work
once it runs, and CPU contention alone never drains running work. Yielding judges running work on what it holds: CPU and RAM usage, and
GPU memory usage for work that takes a hint, else its charge, since its growth cannot be
bounded.
Subtract attributed managed usage from total system usage before classifying the
remaining usage as external; do not subtract both the usage and the entire reservation
again from free capacity. Unknown observations prohibit optimistic admission.
Own CPU comes from the allocation's cgroup leaf (`cpu.stat`, only while every member is
an own verified process), else the process CPU clock read while a pidfd shows the
verified process alive, else the `/proc` tick counters. Each source's read error is
subtracted: its resolution plus one scheduler tick per thread that can be running, or
two counter ticks for the tick counters. A process that ran the whole interval is divided
by its own read interval, a new one by the whole aggregate interval. A change of source
gives no reading for that interval. Summed over own work for a decision, pending time
counts at most one tick per scope CPU at each read, since only a task running at the read
has any; the sum stays between the sums of the per-allocation bounds. With the cgroup
backend, while every own allocation is a leaf directly under the delegated root, the root
holds no process and its children are exactly those leaves, own CPU comes from the root's
`cpu.stat` read beside `/proc/stat`. Over the last 8 intervals, host busy time and own CPU
each carry one bound for the whole window: the window's external estimate sets the
allowance, the targets, the admission budget and the p95 usage samples while this
interval's estimate stays within its own bound of it, and an estimate beyond that bound,
growth or fall, applies at once, so the 2-interval reclaim bound is unchanged.

CedeGrid's own CPU, the agent's process clock and its supervisors', is in use by CedeGrid on
the owner's side: it comes out of external usage and the owner's unmanaged usage, never
forms an unattributed principal, and lowers own tasks' budget and targets by its amount, so
own tasks and CedeGrid together stay within the owner's share. `doctor` shows it as
`in use (CedeGrid)` and the node report carries it per resource. The agent reports its
loop's own CPU by stage once a minute (`loop_cost`).

Each resource runs in a contention mode, and for every resource and observation the
targets are ordered: Yield First ≤ Neutral ≤ Performance. Capacity no workload wants
does not sit idle: a reserve is held back only for how slowly its resource is reclaimed,
so CPU and GPU compute keep small margins and memory keeps the floors. CPU and GPU
compute are *contended* when they are saturated (busy at or above `saturation_percent`,
98% by default, of capacity, or for CPU external PSI above `psi_some_percent`) and an
external principal uses at least
`principal_min_percent` of them without its demand being known to be satisfied, or when
external activity of unknown amount exists. Principals are the account CedeGrid runs as,
whatever its UID, and users at or above `contention.uid_min` (1000) that have used at
least `principal_min_percent` for `contention.principal_window_intervals` (4)
observations; they leave after `contention.principal_leave_s` (60 s) below it, and
meanwhile their unused share goes to the others. The account CedeGrid runs as is one
principal: its unmanaged processes share its fair share with own work and are served as
own work's margin frees capacity, so the 2-interval reclaim bound applies to other
principals' demand. Until a user joins, its usage counts
toward one unattributed principal, together with processes whose owner cannot be read,
so contention is seen at once. A host that mounts `/proc` with `hidepid=2` always takes
this path for other accounts. Usage by lower UIDs is background demand: it comes off the
capacity and never contends. A principal's usage sums its processes, each counted once: a
process's threads are part of it, not processes of their own.

Under saturation, a contending principal whose unknown demand is below one margin step
is probed: own work steps down by the mode's margin per response window, and a principal
whose usage does not grow into the freed capacity is recorded as satisfied at its usage
plus the contention floor. It then no longer contends until its usage exceeds that
demand. A larger demand gets the fair share at once and is then watched for one response
window: if the host has capacity to spare and its usage stayed within its baseline plus
the contention floor, it is recorded the same way, so own work scales back up at the
normal rate and stays up; a recorded user that returns rejoins at once, and while the
resource is saturated a recorded user using clearly less than its recorded demand counts
at that demand, so it gets it back within 2 intervals. Activity of unknown amount is never
recorded. On contended compute, Yield First pauses opportunistic
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
scale up: 10 s per step in Neutral; in Performance, one monitor interval on CPU and GPU
compute and 2 s on memory. A held step still moves no faster than the minimum step dwell.
On an idle host, Performance reaches `performance_ramp_percent` (default 90%) of available
capacity minus its margin within `performance_ramp_s` (default 10 s): once the agent's
growth cooldown allows, one heartbeat admits as much as fits within the allowance, at most
32 new assignments. Launches run off the agent's observation path. A coordinator that offers
`lease_batch/1` gets one lease request per iteration: renewals first by lease deadline,
then initial authorizations, at most 128 items and 1 MiB per request, each item granted or
refused on its own in one transaction, and a retried batch is idempotent per item. Against
a coordinator without batches, or after a refused batch, per-allocation requests share half
the monitor interval, so launches do not hold observations back; renewals share that half
too, which limits how many running assignments one node keeps renewed: about 117 on the
test host (`docs/validation.md`, Launch bursts). On memory, Yield
First keeps 20% free beyond the floor and the other modes only the floor. The CPU idle reserve is the larger of
`cpu.reserve_physical_cores` (default 0) and the mode's margin; the two never stack. The
margins are configurable per resource. In every mode, contended CPU returns within 2
monitor intervals except in Performance, which follows what external work takes: the
first step applies at once. Own CPU usage over its target while the budget did not fall
(external usage did not rise beyond its per-interval bound and no contention lowered the
target) is enforced first: held caps keep their targets, which supervisors hold from the
measured rate, and no step is entered until usage has stayed over the target for
`min_step_dwell_intervals` with every held cap applied; then own work steps with cause
`own_over_target`, shared at one level within each mode tier, and such a step reverses
(`step_reversed_own_usage_fell`) once usage falls back and the cap no longer binds. A
budget that fell steps at once with `external_growth`. Caps already held give an external
increase between them, lowered at one level within their tier, rather than each by the
whole increase. While a resource is not contended, a held cap or hint rises again toward
the mode's target at the mode's scale-up rate. On CPU in Yield First and Neutral, once
every external principal, the unattributed remainder included and background left out,
has stayed below `principal_min_percent` for `principal_window_intervals`, held caps rise
to the target after the step dwell alone and the CPU allowance in one move; while
contention only falls, the scale-up interval applies. A Guaranteed allocation's
CPU is never capped below its request, by the policy or by its supervisor.

CPU and RAM each wait for their own cooldown, counted from the time each became stable:
`contention.cpu.scale_up_cooldown_ms` and `contention.ram.scale_up_cooldown_ms`. Unset,
each is its mode's scale-up interval: 30 s in Yield First, 10 s in Neutral, and in
Performance one monitor interval for CPU and 2 s for RAM. Each resource's admission opens on its own, and placement needs every
requested resource open; every launch reserves CPU and RAM. Own work blocks admission on a
resource (`admission_own_work_yielding`) only while it is being reclaimed there: while it
holds more than its target, or a step is still moving down. A cap or hint that holds at its
target does not block, and coordinator placement follows the node's admission without
counting steps. When CPU or RAM admission is closed only by the node's own new allocations
that have no first measurement yet, or by the cooldown only they restarted, the report says
so for that resource, and the coordinator still authorizes reservations it placed earlier
while they fit the budget; it places nothing new there, and any other cause, a GPU resource or
a report without that statement still refuses. A move within the noise tolerance on memory, or within the reversal hysteresis
of the external growth a compute step answered (`hysteresis_percent` of that demand), is
jitter: it blocks no admission, and a CPU cap that holds within it resets no cooldown. Admission then follows headroom, cooldown and reclaim backoff: own
charged usage plus a new request stays within own target, the aggregate from the node mode
and cap, and a request beyond it waits (`admission_over_own_target`). After a reclaim the
resource waits for its cooldown again, so admitting, growing and capping cannot cycle. GPU memory waits for
`contention.gpu_memory.scale_up_cooldown_ms`, else a set legacy
`gpu.scale_up_cooldown_ms`, else its mode's interval, counted per device from the time that
device became stable: its own external growth, its own measurement and its own charges reset
it, and CPU or RAM state never does. No supported backend reports a GPU whose memory is host
RAM, so no device is coupled to RAM. The node-wide cooldown remains only behind the 0.2
expansion flags (`cooldown_pending`). On RAM and GPU memory, the n-th
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
allowance at once, the totals stay within the mode's allowance. One publish threshold in
total is held back: falls publish together once the published values exceed the current
allowances by more than it, and a rise moves at most one step per scale-up interval, except
on CPU once contention has fully ended: the mode's margin of capacity on CPU, and a fixed 20%, 5% or 2% of capacity in Yield First,
Neutral or Performance on memory, which keeps only the floor under Neutral and Performance
and so has no margin there.

A job may request a mode per resource (`LaunchRequest.contention`); it runs at the less
aggressive of that mode and `contention.max_mode`. The owner's aggregate on a resource
still comes from the node mode: per-job modes only order who yields inside it, Yield First
first, then Neutral, then Performance, with the cost order within a tier. Each job is also
held to its own mode's target, and steps and allowance rises follow its own mode's pace.
Every opportunistic allocation drains before any Guaranteed one, and a Guaranteed
allocation keeps its CPU request in every tier. An allowance is at most what the job's own
mode's target leaves it, and the room that bound leaves goes to the others.

On a delegated cgroup with `memory.high` authorized, RAM contention starts with a cap:
the supervisor writes `memory.high` on the allocation's own leaf, after the same checks as
every runtime step, and reads it back. The cap is never below what the leaf cannot
reclaim (its unevictable memory and, when the leaf or host has no swap, its anonymous
memory, plus `noise_tolerance_mib`), never below a Guaranteed request, and never above a
launch `memory.high`. A cap held above its target for that reason only stops growth
(`ram_cap_growth_stop`), and the hint frees the rest. If `memory.events` `high` keeps
rising for the step dwell while `memory.current` falls by less than the noise tolerance,
the cap is raised to current usage and the hint follows (`ram_cap_ineffective`). A failed
write or readback makes the cap unavailable for that allocation (`ram_cap_refused`).
Reversal follows the memory rule and restores the launch `memory.high`, `max` unless
configured. `memory.max`, `memory.low` and `memory.min` are never runtime steps; rootless
backends have no RAM cap.

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
raised floor carries the reason code `small_floor_raised`. A breached floor blocks
admission on that resource and releases opportunistic work holding it. Floor policy A, the
only floor policy, then drains Guaranteed work that holds the memory, as the last step: after
`contention.guaranteed_hint_deadline_s` (5 s) and, for RAM, after the breach has lasted
`contention.ram.guaranteed_breach_window_s` (10 s). GPU memory breaches have no window.
A Guaranteed allocation that is not hint-ready waits only for the window, and on the
critical path the window is skipped.

Floors hold by mode. Own memory above its memory fair share (the max-min share beside the
RAM principals, or beside the device's GPU memory principals) gives way at the floors in every
mode, and in Yield First and Neutral all own memory does. In Performance and Aggressive a
floor breach that others' growth caused steps own memory only down to its fair share: memory
within it is not hinted, drained or sent TERM, on the critical path and for Guaranteed work
too. Those in-share tasks get one save-progress request per breach episode instead: the
target file's `save_progress_sequence` rises and both SDKs call `on_save_progress`, which is
no drain and asks for no memory back. The kernel's out-of-memory killer may then take another
workload's process, as it would without CedeGrid. This is the one memory exception, a rule of
those modes and not an option; the operator envelope applies in every mode.

Out-of-memory order follows fault. On Linux each own task's `oom_score_adj` at launch is its
baseline, the same as every other process's, and is never set below it. While own RAM is above
its fair share, the tasks that make up the excess, in the memory victim order, are raised to
1000, every process of their family included, and return to their baseline once own RAM is
back within its share; each change is evidence with the task, the old and new values, the
cause, and own RAM beside its fair share. `ram.prefer_own_oom_victim = true` keeps every own
task at 1000 from launch, a risk to own work only. Without privileges, in-share own work is
protected only up to everyone else's baseline: the kernel can still take it as the largest
process. Other platforms offer no unprivileged equivalent and claim none. A task the
out-of-memory killer ended, shown by SIGKILL in an interval in which the host's
`/proc/vmstat` `oom_kill` count rose, or in Full by its leaf's `memory.events` `oom_kill`, is
requeued while own memory was within its fair share at the last observation, with cause
`oom_not_own_fault`, no failure and no attempt counted; RAM admission stays closed until free
memory is back above the floor. Above its share, or with the share or the count unknown, the
kill is a normal failure.

No option puts another workload at risk of an out-of-memory kill, swap thrash or a
failed GPU allocation; the in-share memory rule above belongs to Performance and Aggressive
themselves. The constitution allows one risk to other workloads' compute, slower compute, and
only under an unprotected Aggressive mode, which is not implemented: the modes are Yield
First, Neutral and Performance, and no configuration key or job field can name Aggressive.
In Performance a compute step never targets own work below its fair share: a cap that misses
its target is applied again from the measured rate, and only a cap at the minimum duty with own
usage still above the fair share pauses, for at most one monitor interval. The former floor policies `b` and `c` and Neutral memory policy
`b` were removed, and a configuration that sets them fails to load with a message that
names the replacement settings. Every configuration key is classified as safe, bounded
or caution. Three options are marked *caution* because they let own work hold more
capacity within the floors: a floor below twice its minimum, a margin below its default,
and a detection setting beyond its default (`saturation_percent`,
`principal_min_percent`, `noise_tolerance_mib`, `contention.uid_min` or
`contention.gpu_compute.attribution_tolerance_percent` above it, `memory_peak_window_s` or
`quiet_window_intervals` below it). Performance is the default mode and needs no
acknowledgement; a file that still lists `"performance_mode"` loads unchanged. A lower
margin needs `contention.acknowledge_caution = ["reduced_margin"]` and a higher detection
threshold needs `"relaxed_detection"`. A floor
below twice its minimum without `"small_floor"` loads, so 0.2 files keep loading, but is
raised to twice the minimum with a warning, given at load or, for a floor a percentage
sets, when the agent first samples the host; it applies as configured only once
confirmed. `doctor`, node reports and the coordinator `status` list active caution
options, and `doctor` lists the keys and values each covers.
`noise_tolerance_mib` is at most 1024 MiB, and the agent refuses to start when it
exceeds half of any GPU's floor. Settings that lengthen reclaim are bounded: monitor
intervals at most 10 s, kernel freshness at most 20 intervals, GPU sample age, drain
timeout and termination grace at most 60 s. Decisions and node reports show, per
resource, how much is held back and why. Without `contention.max_mode`, a job may
request at most the node's most aggressive configured mode, so no job runs in a mode beyond
what the node's configuration allows. A file that sets no mode runs Performance and gets one
`default_mode_changed` notice; `default_mode = "neutral"` keeps the earlier default. Policy recordings without an allocation class
retain the legacy opportunistic default; live observation copies the required class from
the durable execution record.

The legacy `gpu.execution_mode` is read at load time, with a warning on stderr that
names the replacement: `contention_aware` and `best_effort_occupied` set GPU compute and
GPU memory to Neutral unless their mode is set, `conservative_non_sharing` sets
`contention.never_share_occupied_gpus = true`, and `auto` changes nothing. A non-empty
`gpu.best_effort_external_processes` is reported as unused. `doctor` shows the
interpretation and every load-time warning. `gpu.execution_mode` has no other effect.
`gpu.protective_shrink_percent` and `gpu.active_shrink_percent` are still read and validated
so 0.2 files load, and have no effect: graduated steps replaced the protection caps.

A drain decision does not release a reservation. The durable execution ledger keeps it
charged until process exit and resource-release observations confirm the release.
CPU/RAM reserves without applied cgroup controls are scheduling policies, not kernel
enforcement. Available, configured, and applied capabilities are distinct facts.

## What the node shows

Every reason the policy gives carries a stable code from the catalog below; the text
beside a code in an output may add detail and is not stable. Codes appear in each
resource's `reasons`, in `admission` (held or blocked), in `actions` (the step and its
cause), as the causes in `held_back` (`held_back_<cause>`), as the GPU compute path, as
caution options in `exceptions`, and in the node-wide `reason_codes` beside the prose
`reasons`.

- `observe` prints one JSON object per sample: the snapshot and the decision, with
  `resources`, `actions` and `exceptions`.
- `doctor` prints JSON, or text with `--text`. For each resource it shows the configured and
  effective mode, own usage, external demand, unattributed usage, the target share,
  admission, the current steps, the reasons and `held_back`. It also shows the floors on
  this host, the active caution options with the keys and values they cover, the legacy GPU
  interpretation and the steps the configured backend offers per resource.
- The coordinator `status` shows each node's negotiated contention summary under
  `contention`, with each resource's admission and `held_back`; a node that did not
  negotiate `contention/1` shows `"not negotiated"`.
- `held_back` gives each cause in the resource's units and, under `percent`, as a percent
  of `capacity`: `floor`, `reserve`, `margin`, `ceiling` and `contention` are kept from own
  work, `unknown` while a measurement is unknown or held, and `peak`, `noise_tolerance`,
  `cooldown`, `backoff` and `charge_shortfall` from admission. `ceiling` is what a visible
  ancestor CPU limit keeps from the budget beyond the reserve and margin; `doctor` shows it on
  its own line, and it never counts as contention.

| Code | Meaning |
|---|---|
| `yield_step_lower_share` | Own work on the resource runs at a lower share. |
| `yield_step_cap` | Own work on the resource is capped at a target. |
| `yield_step_pause` | Own work on the resource is paused. |
| `yield_step_hint` | Own work is asked through its resource-target file to shrink to a target. |
| `yield_step_drain` | Own work is asked to drain and stop. |
| `yield_step_terminate` | Own work that did not drain in time is terminated. |
| `external_growth` | External demand grew into what own work holds. |
| `own_over_target` | Own usage stayed over its target while the budget did not fall. |
| `floor_breach` | Free memory fell below the no-harm floor. |
| `unknown_measurement` | The resource's measurement stayed unknown past the unknown window. |
| `envelope_breach` | Own work exceeds the operator envelope. |
| `operator_drain` | The operator asked the node to drain. |
| `coordinator_drain` | The coordinator asked for the allocation to drain. |
| `lease_expired` | The allocation's lease expired before it was renewed. |
| `oom_not_own_fault` | The out-of-memory killer ended own work within its fair share, so it is requeued without an attempt. |
| `step_reversed_contention_ended` | A step taken for external growth was relaxed because external demand fell back. |
| `step_reversed_own_usage_fell` | A step taken for own usage over its target was relaxed because own usage fell back. |
| `step_reversed_floor_restored` | A step taken for a floor breach was relaxed because free memory is back above the floor. |
| `step_reversed_measurement_known` | A step taken for an unknown measurement was relaxed because the measurement is known again. |
| `step_reversed_envelope_restored` | A step taken for the operator envelope was relaxed because own work is back within it. |
| `step_reversed_drain_lifted` | A step taken for an operator drain was relaxed because the drain was lifted. |
| `step_skipped_unavailable` | A step this allocation cannot take was skipped for the next one. |
| `saturated_by_psi` | External tasks wait for CPU above psi_some_percent (PSI), so the CPU counts as saturated; a cgroup shared with own work never counts. |
| `psi_system_only` | Only system-wide CPU pressure is readable; it cannot tell own from external waiting, so it adds no evidence. |
| `psi_unknown` | CPU pressure is missing, unreadable or malformed, so it adds no evidence. |
| `ram_cap_growth_stop` | The RAM cap holds above its target, at what the leaf cannot reclaim, so it only stops growth and the hint frees the rest. |
| `ram_cap_ineffective` | The RAM cap throttled without freeing memory, so it was raised to current usage and the hint frees the rest. |
| `ram_cap_refused` | A RAM cap write or its readback failed, so the cap is unavailable for this allocation and the hint follows. |
| `allocation_unknown_hold` | An allocation's usage is unknown, so it is charged at the larger of its request and last known usage and its step holds. |
| `measurement_unknown_hold` | Own usage exceeds what the host reports beyond the known error, so the resource holds. |
| `inconsistent_within_tolerance` | Own usage exceeds what the host reports within the known error, so external usage is clamped. |
| `first_observation_baseline` | External GPU memory at the first observation is the baseline, not growth. |
| `vram_noise_below_tolerance` | External GPU memory grew within the noise tolerance. |
| `mode_capped` | A job's requested mode is above the node cap, so it runs at the cap. |
| `probing_demand` | Own work steps down by one margin step to learn whether a small external demand is satisfied. |
| `launch_nice_kept` | A task launched at the Yield First nice keeps it in another CPU mode, since it cannot be lowered again; its caps do the yielding. |
| `watching_larger_demand` | Own work holds the fair share while a larger external demand is watched for one response window. |
| `satisfied_demand` | An external user's demand is recorded as satisfied, so it no longer contends below that demand. |
| `applied_duty_credit` | Own GPU work is credited at the duty its supervisor applied. |
| `gpu_compute_measured` | External GPU compute is measured from per-process samples. |
| `gpu_compute_unknown_activity` | External GPU activity without per-process amounts counts as one principal of unknown amount. |
| `gpu_compute_amount_unknown` | Per-process GPU samples do not account for the device, so the external amount is unknown. |
| `gpu_compute_recently_inconsistent` | Per-process GPU samples were recently inconsistent or are new, so the measured path waits for the quiet window. |
| `small_floor_raised` | A floor near its minimum applies as twice the minimum until small_floor is acknowledged. |
| `admission_draining` | The node is draining, so nothing is admitted. |
| `admission_spool_exhausted` | The node spool budget is exhausted, so nothing is admitted and nothing is drained. |
| `admission_floor_breached` | The floor is breached, so nothing is admitted on this resource. |
| `admission_measurement_unknown` | The measurement is unknown, so admission waits for it. |
| `admission_own_work_yielding` | Own work on this resource is yielding, so nothing more is admitted. |
| `admission_cooling_down` | The resource is cooling down after its last change. |
| `admission_reclaim_backoff` | The reclaim backoff after recent floor breaches applies. |
| `admission_over_own_target` | Own charged usage plus the request would exceed own target, so the request waits. |
| `admission_envelope_exceeded` | Own work exceeds the operator envelope, so nothing is admitted. |
| `gpu_memory_unknown` | GPU memory or its attribution is unknown, so the device admits nothing. |
| `gpu_external_workload_appeared` | A new external GPU workload or external memory growth appeared. |
| `gpu_charges_exceed_reserve` | Own GPU charges exceed the memory reserve or the allocation budget. |
| `gpu_never_share_occupied` | The never_share_occupied_gpus setting refuses a device with external contexts. |
| `gpu_yield_first_needs_quiet_device` | Yield First admits GPU work only on a quiet device. |
| `gpu_yield_first_activity_unknown` | Yield First admits GPU work only on a quiet device, and the device's activity is unknown. |
| `gpu_observation_unknown` | The GPU observation is unknown. |
| `gpu_inventory_unavailable` | The GPU inventory is unavailable, so GPU admission is blocked. |
| `gpu_absent` | An allocated GPU is absent from the current inventory. |
| `cpu_capacity_unknown` | The CPU capacity is unknown, so the CPU budget is zero. |
| `ram_capacity_unknown` | The RAM capacity is unknown, so the RAM budget is zero. |
| `cpu_usage_unknown` | CPU busy time is unavailable, so CPU admission is blocked. |
| `ram_usage_unknown` | Available RAM is unknown, so RAM admission is blocked. |
| `kernel_cpu_stale` | The kernel CPU snapshot is stale, and repeated readings do not refresh it. |
| `cpu_scoped_budget` | The CPU budget uses the effective CPU set's usage and capacity. |
| `cpu_topology_unknown` | The CPU topology is unknown, so the physical-core reserve cannot be placed. |
| `cpu_set_unknown` | The effective CPU set is unavailable, so scoped CPU admission is blocked. |
| `cpu_scoped_telemetry_unknown` | Scoped CPU telemetry is missing, stale or inconsistent. |
| `cpu_charges_exceed_budget` | Managed CPU charges exceed the scheduling budget. |
| `ram_charges_exceed_budget` | Managed RAM charges exceed the scheduling budget. |
| `cpu_only_admission` | Stable CPU and RAM headroom allows CPU-only admission while GPU admission stays blocked. |
| `cooldown_pending` | The node-wide cooldown behind the 0.2 expansion flags has not elapsed; each resource's admission keeps its own cooldown. |
| `held_back_floor` | Memory kept free as the no-harm floor. |
| `held_back_reserve` | CPU kept free by the explicit reserve where it exceeds the margin. |
| `held_back_margin` | Capacity kept free by the mode's idle margin. |
| `held_back_ceiling` | CPU a visible ancestor CPU ceiling keeps from the budget beyond the reserve and margin; it is not contention. |
| `held_back_contention` | Capacity left to contending external demand. |
| `held_back_contention_psi` | CPU left to external demand while only external pressure (PSI) shows the CPU saturated. |
| `held_back_peak` | Admission headroom kept for the external memory peak. |
| `held_back_noise_tolerance` | External GPU memory growth tolerated as noise. |
| `held_back_unknown` | Capacity withheld while the measurement is unknown or held. |
| `held_back_cooldown` | Admission headroom withheld until the resource has cooled down. |
| `held_back_backoff` | Admission headroom withheld by the reclaim backoff. |
| `held_back_charge_shortfall` | Admission capacity held by charges above usage. |
| `small_floor` | A floor below twice its minimum keeps less headroom for other workloads' bursts. |
| `reduced_margin` | A margin below its default leaves external demand less idle capacity to grow into. |
| `relaxed_detection` | A detection setting beyond its default recognizes contention later or forgets it sooner. |

## Process-control contract

Trusted commands must remain inside the supervisor's lifecycle and must not create
escaping daemons. Workloads requiring that behavior need an explicit adapter or
a supported containment backend. The system is not a sandbox for hostile programs.

Only Yield First tasks launch at `cpu.nice` (10); Neutral and Performance tasks launch at
the default priority and leave yielding to their steps. A task that moves from Yield First
to another mode keeps its launch nice, since an unprivileged process cannot lower it again,
and its decisions say so (`launch_nice_kept`). Own priority is never raised.

At start the agent raises its own soft `RLIMIT_NOFILE` to its hard limit, and the worker
gate gives each task back the soft limit the agent started with before the user's command
runs. The agent takes a launch only while its open descriptors, 8 for each launch in
progress and 64 kept free fit under that limit; otherwise it advertises fewer launch slots
and reports `launch_refused` with code `descriptor_limit`. Running out of descriptors
(`EMFILE` or `ENFILE`) never ends the agent's loop: that interval decides nothing new and
every step holds.

A running supervisor waits for a control line, its leader's exit through the pidfd or a
new SDK connection until its next deadline: a duty cycle's phase switch, the lease, drain,
TERM and KILL deadlines, the readiness check and the release confirmation, and passes at
least once a second. Without a pidfd, while SDK connections or mediated children are
active, or while a control or drain-file write fails, it passes every 5 ms.

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
whole family, with each live usage counted once. Uncertainty in any registered child
prevents family release.

Arbitrary forked descendants are unsupported by the rootless backends, and no
actuator reaches one: signals go only through the leader's or a mediated child's own
verified handle. A descendant is acted on only when it is verified: its pidfd is
opened before its parent chain to the launched process is read, it is confirmed
alive through that pidfd afterwards, its user and group IDs match the run's, and it
is acted on only through that pidfd, so a reused PID is never acted on. A descendant
that leaves the tree, changes its user or group IDs (for example by running a setuid
program) or cannot be confirmed is never acted on. A call that accepts only a thread ID,
such as CPU affinity, is allowed on a verified member's thread only when the member's pidfd
shows it alive first, the thread's identity is fixed first (a thread pidfd on Linux 6.9 and
later, else its `/proc/<pid>/task/<tid>` entry with its start time), and the same check finds
the same thread alive afterwards. Otherwise the change is reported as possibly misdirected
and that thread is not touched again. A thread that exits during the call frees its ID,
which can be reused inside that window only after the kernel's ID space wraps, and only for
a process of the same account. Today CedeGrid sets CPU affinity only on its own calling
thread, before user code runs, and makes no thread-ID call on any other thread.
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
inaccessible or unsupported readings remain unknown. CPU pressure of the cgroups outside
own work's user slice, external PSI, is a second CPU saturation signal: its highest
`some` avg10 above `contention.cpu.psi_some_percent` (10%) saturates the CPU, judged per
observation like `saturation_percent` (FR-072). Saturation still needs an external
principal to make the CPU contended, and a PSI-only hold shows as `contention_psi` in
`held_back` (`saturated_by_psi`). Own cgroups' pressure is shown and never counts, and a
cgroup that holds own work together with other users' processes counts as own, so its
pressure never counts either. Under PSI-only saturation every contending principal is
probed, not only small demands: one that does not grow into the freed capacity is
recorded as satisfied, and contention ends.
System-wide pressure cannot tell own from external waiting and adds no evidence
(`psi_system_only`); missing, unreadable or malformed PSI adds none and holds nothing
back (`psi_unknown`). PSI feeds CPU saturation only, never memory, GPU or floor
breaches, gives every mode the same evidence, and is read without writing anything.
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
