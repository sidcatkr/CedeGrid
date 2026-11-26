# Guarantee boundaries

CedeGrid lets own work use a shared server's spare CPU, RAM and GPU capacity, most usefully
where other people's processes already run and it is unclear how much to take, so that no
capacity sits idle. Own work runs as fast as it can, efficiently, and the contention mode says
how much it gives back when others need capacity. Performance is the default and gives back
nothing beyond what the kernel and the GPU driver give beside the same work started without
CedeGrid; Yield First and Neutral give back more. Each section below states
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

Each state directory has a private control directory beside it in the same parent,
`.<name>.cedegrid-control`, which holds the namespace's owner and lifetime locks outside
anything quarantine renames. There is no uninstall or purge command: removing a stopped
node's or coordinator's state means removing both directories.

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
  Neutral and Performance admit and share compute, Performance only while no counted principal
  is slice-bound on a time-sliced device. It never drains own work by itself.
- Unsupported, failed, absent-fresh-sample, or otherwise uncertain activity never
  becomes evidence of idleness: Yield First admits nothing on that device, Neutral counts
  it as one principal, the current steps hold, and escalation waits for the bounded unknown
  window; Performance admits nothing new there and holds every step. Missing basic observation blocks the device.
- Stable headroom permits incremental expansion only after the configured cooldown.
  The coordinator reserves each launch before dispatch, only where every requested
  resource's admission is open, and the agent rechecks locally. Held or blocked
  admission never preempts other work, nor do launch slots an agent holds back while it
  paces its growth; one scheduling pass never lets two waiting tasks claim the same victim.

GPU admission follows each device's GPU compute mode, by one rule the policy and the
launch recheck share: Yield First admits only on a quiet device; Neutral and Performance
admit on quiet or active devices and under activity of unknown amount, which counts as
one principal; unknown GPU memory blocks; and `contention.never_share_occupied_gpus`
refuses new work on a device with external contexts or an unknown process list. For Yield
First, compute by the account CedeGrid runs as, outside CedeGrid, is not activity to wait
out: it is the owner's own share. A device whose busy contexts are all the owner's admits
when the driver's per-process samples were read and every other account's listed context has
been quiet for the quiet window. The owner's compute neither counts as their activity nor
restarts that window; another account's new context starts its own. Another account's context
that is busy or not yet quiet for the window holds admission as before, and so does a context
whose owner cannot be read, activity the collector reads as unknown, and a driver that does not
report the contexts. Own work
already running on a device that becomes occupied is not drained for that alone. The
launch recheck also refuses a request that would leave less than the device's floor
free. Missing GPU observation holds the current steps as an unknown measurement instead
of yielding fully, so Guaranteed GPU work needs no special contract. No GPU compute
rate cap or duty cycle runs, at either level: no unprivileged per-process quota exists on a
time-sliced GPU, and stopping and resuming own work by period misses its target. Yield First and
Neutral hold own GPU work back by whole tasks instead. A task is paused, in victim order, only
while the own work left running stays at or above own work's target, so rounding favours own
work; a lone job, and one whose pause would go below the target, is left alone. A pause reaches
only the verified family: through pidfds without a delegated cgroup, and through a freeze of the
workload's leaf, confirmed through `cgroup.events`, where `cgroup.freeze` is authorized. A pause
still answering its demand at `pause_bound_s` (10 s) moves to another task, so no task is drained
for GPU compute. Guaranteed work takes no GPU compute step. On a
time-sliced device, one neither under MPS nor a MIG parent, CedeGrid relies on the driver's
time-slicing: in Neutral own work is not capped while it holds no
more than its share of the device's busy contexts, m × k ≤ E, with m its busy processes and the
owner's unmanaged ones, E the external busy processes and k their principals, each counted while
it has a sample within the counting window: the last 60 intervals whose per-process samples
were readable, 30 s at the default interval, own work capped or not. A single-process job always meets it, and on an amount of unknown size it
holds with at most one own process listed. This rests on the driver giving busy contexts equal
slices, so MPS, a MIG parent and a device whose MPS or MIG state cannot be read keep the
measured path, and Yield First is unchanged; each decision records the counts (`time_slice`).
When own work holds more than its share there, m × k > E, an external principal with E_j busy
processes is slice-bound when its uncapped reading, averaged as below, is at least
`contention.gpu_compute.slice_bound_percent` (75) percent of their equal slices, 1000 × E_j ÷
(m + E). With none, nothing is held, since the driver already gives every other principal what
it asks for. Otherwise, in Neutral, own tasks on the device are paused while the own tasks left
running stay at or above ⌊E_b ÷ k_b⌋ (`gpu_time_slice_hold`), with E_b and k_b the slice-bound
principals' processes and number and each own task counted as one context: beside one saturating
process, two own tasks become one, so each side gets half. Principals that are not slice-bound
keep their slices. A held task resumes once no counted principal is slice-bound or the rule holds
with it running. One more own process is admitted on the device only while (m + 1) × k ≤ E or
no counted principal is slice-bound (`gpu_time_slice_admission_held`), a principal reading the
larger of its average and its most within the sample age: beside one saturating process, one
own process is the fair share, and no second one is admitted. While the amounts go unmeasured
for the sample age, every counted principal is slice-bound. Yield First and Neutral count every
principal; Performance and Aggressive count only accounts whose CedeGrid work on the device
arrived first, as below, and beside the others admit as plain Linux would. One heartbeat places
at most one new GPU task on a device, since an open admission is room for one more own process
by this rule; where the node reports plain admission on the device, the rule does not apply and
the device's memory budget alone bounds what one heartbeat places.
Performance and Aggressive hold no task: own work keeps the driver's slices, as beside the same
work started by hand. The counts are the
processes sampled busy within the counting window, and own work's processes also from the
interval the device first lists them, before their first sample. An own process stops counting
once 60 intervals with readable samples (30 s at the default interval) pass without a sample for
it, from its listing or its last sample. While CedeGrid keeps one of its tasks paused on the
device, every own process still listed there counts: a stopped process has no samples, but it
computes again once released. So an own task that holds a GPU context without computing
counts toward the tasks left running beside a slice-bound user for at most that long after its
listing or an agent restart; no other workload pays. The owner's unmanaged processes and
other users' processes count only while sampled busy in that window, so an idle one never counts.
The estimate's readings decide which principals are slice-bound even in an interval whose amounts
are inconsistent. Only while the per-process samples are unreadable do the listed processes
count. Without an uncapped interval in the estimate every counted principal is slice-bound; on a
device last read as time-sliced whose listing is unknown, nothing new is paused and held pauses
hold. Each decision records where its counts came from, for each own process counted whether by
its listing or by a sample, the slice-bound principals with their readings and thresholds, and
the hold. On other devices a task is paused only while own work's measured amount less that
task's equal part stays at or above the target, and a lone job is never paused for it. Own GPU
work that was listed on a device before every other principal listed there keeps, in Yield First
and Neutral, at least the share that leaves none of them more than half of the device
(`gpu_first_come_floor`): on the time-slice path 2 × E_max − E running own tasks, E_max being the
most processes of one principal, and elsewhere, beside exactly one listed principal, half the
device. Arrival is kept per UID and, for own work, per device; one absent for more than 30 s
arrives again when next listed, and an agent restart starts the record again, which only withholds
the floor. A listed process whose UID cannot be read, or an unknown listing, withholds it, and
each decision records whether it applied and why not (`gpu_demand.first_come`). External compute
is measured when fresh per-process samples,
summed over every process on the device, are within
`contention.gpu_compute.attribution_tolerance_percent` (15) points of device utilization. A
sample counts its whole span since its process's previous sample, at most one interval, in the
interval where it arrives, a process's first sample covering from the interval's start and time
without a sample counting as idle, and each observation records the samples
(`compute_sampling`). When one interval misses the tolerance, as when a busy process's sample is
late, the shares and the device summed over it and the 3 intervals before it decide, and the
amounts are then those 4 intervals' averages. On a time-sliced device another user's reading
rises when own work leaves gaps, at the same demand, so in Neutral the external amount that sets own work's target, the owner's unmanaged usage left out, is the
average over the last 60 intervals in which own work ran unheld on the device (no pause on GPU
compute and no CPU step of cap or more sent by the decision that set the interval or the one
before it), or had no allocation there. No re-measure window is taken: with no cap there is none
to lift. A change in the device's external processes, the background ones and the owner's
unmanaged ones left out, restarts the average; Yield First takes the larger of the interval's
amount and the average, and each decision records how the demand was read (`gpu_demand`). A measured
amount takes the CPU paths, with external processes grouped into principals by UID as on CPU and
device time nobody accounts for counted as external usage while an external process is listed,
and as one more external principal from the attribution tolerance's 15 points, below which it is
sampling error, so own work's admission keeps capacity minus that amount minus the margin under
Performance, and its target is its fair share or a learned satisfied demand under Neutral. A device leaves the
measured path at once and returns only after `quiet_window_intervals` consecutive measured
intervals. An interval in which both process queries succeed, every listed process is an own
verified process checked through its pidfd, no MPS is present, device utilization is known and
memory attributed to no listed process has not grown beyond the last measured, own-only or empty
interval plus the noise tolerance is measured at once with no external compute, so a new own
process neither restarts the window nor turns the device Unknown. A context of a system account,
a user ID below `contention.uid_min` (1000) or above `contention.uid_max` (60000), such as a
display server's or a display manager's login screen, is background rather than activity while per-
process utilization is readable for the device (the query succeeds or reports no samples) and
its own utilization stays below `principal_min_percent`: it is neither external compute nor
unknown activity, its share of device time is nobody's, and its memory still counts as external
usage, as `never_share_occupied_gpus` still counts the context. Where per-process utilization is
not readable it stays unknown activity. Unsupported queries, stale samples, an incomplete
process list or MPS leave it activity of unknown amount, one principal, under which Neutral
holds the fair share and Performance admits nothing new and holds: a task whose pause the
supervisor reports applied counts as holding nothing. Unknown activity from a telemetry failure
or a missing observation first holds the steps and escalates after the unknown window. Each device's held-back breakdown says which
path it took and why. An empty-device pass is not evidence of occupied-device sharing. See [GPU
operation](coordinator-work.md#gpu-modes).
In Full a restriction (a freeze, a lower `cpu.weight`, a `memory.high` cap) is
written only while the leaf's members are exactly the verified family. A process that joins a
frozen leaf, between the check and the write or while it holds, is thawed at once, within
50 ms, and the step moves to the rootless mechanism, which reaches only the verified family; a
member list that cannot be read twice in a row counts as changed. Lifting what the supervisor
wrote needs only its own leaf, whatever its members, and never tightens a control: a drain, the
leader's exit, a supervisor error and a stop signal return every control it wrote to its launch
value, and a restore that fails is reported and retried every 50 ms while the supervisor lives.
A lower `cpu.weight` or a `memory.high` cap stays on a leaf a foreign process joined while the
step holds, until the step changes or ends. The cap was written only to a leaf read as holding
exactly the verified family; a process that enters afterwards comes under a limit already there,
and CedeGrid writes nothing for it. A supervisor that is killed lifts nothing: its leaf keeps
what it wrote.

All process snapshots and attributed allocation usage are trusted observations from
a collector/registry, not arbitrary workload assertions in the execution path.
Replay accepts synthetic observations purely for testing, and reads them as the live agent
reads its own: a frame that omits `external_processes` means the process list was not
observed, so peaks and principals stand through it, and an external process's exit is
written as an empty list. GPU memory is accounted per UUID; memory is never pooled across
devices.

## Resource accounting

CPU uses millicores of logical CPU time: 1,000 millicores equals one logical CPU.
Physical-core reserves are mapped conservatively using available topology.
For admission, each running or draining allocation is charged the greater of its request
and observed usage once, except where CPU admission is as plain Linux would start work, in
Performance and Aggressive with no first-comer holding it (the decision's
`cpu_plain_admission`): there own CPU counts at no more than each request, in the policy and in
any bound the agent adds, so usage above the requests takes no room from tasks still to start
(006 SC-002).
Running opportunistic CPU, once it has a full window of
samples over `contention.cpu.admission_usage_window_s` (60 s), is charged at its mean usage
while no other principal is active on CPU, and at its p95 from the interval one appears, so work
in its startup phase is charged its request. An over-admission while alone can only cost own
work: a burst caps own work on itself and drains nothing. CPU is compressible, so caps and the
kernel share it if everyone spikes. While a pause, or a cap or hint its usage
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
each carry one bound for the whole window, and where `/proc/schedstat` gives a run time that
fits the window, each CPU's busy upper bound is the larger of its busy ticks and its run time,
plus one tick, never above what its idle ticks allow: the window's external estimate sets the
allowance, the targets, the admission budget and the p95 usage samples while this
interval's estimate stays within its own bound of it, and an estimate beyond that bound,
growth or fall, applies at once, so the 2-interval reclaim bound is unchanged. While a reset
window refills, for at most one window, the last window's estimate stands by the same rule, so
a measurement's wider bound does not read as external growth. Another user's measured usage is
not that looseness: the kept estimate gives way in the interval in which the other principals'
CPU, read from the principal list, has risen by `principal_min_percent` of capacity since it was
taken. It is kept only while that list, when it was taken and now, shows a process of another
account with a readable user ID; with no list, or one that shows only the owner, nothing is kept
and the interval's estimate applies (`cpu_window_estimate_not_kept`). The price: on a host that
hides other accounts' processes nothing is ever kept, so there an own task's unknown sample can
read as external growth for one window. When an own task's sample is
missing, `allocation_unknown_hold`'s reason names the check that failed. In Performance and
Aggressive, a measured task whose process is gone (`not_alive` or `not_verified`), as when it
ends, holds neither CPU nor RAM admission and starts no cooldown: it is charged at the larger of
its request and last known usage, and admission starts the next tasks as plain Linux would. Any
other unknown usage, and on CPU any task while an account whose CedeGrid work came first uses
the CPU, holds as before (006 FR-007).

CedeGrid's own CPU, the agent's process clock and its supervisors', is in use by CedeGrid on
the owner's side: it comes out of external usage and the owner's unmanaged usage, never
forms an unattributed principal, and lowers own tasks' budget and targets by its amount, so
own tasks and CedeGrid together stay within the owner's share. Where the agent runs in a CPU
cgroup of the owner's own (its user slice, or its System mode unit, with the `cpu`
controller), the kernel splits CPU between users and charges CedeGrid and the owner's other
processes to the owner's slice, so own work's kept share below is counted on own tasks'
requests alone and is not lowered by them a second time. `doctor` shows it as
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
whatever its UID, and users from `contention.uid_min` (1000) to `contention.uid_max` (60000),
as `UID_MIN` and `UID_MAX` in `login.defs` have them, that have used at
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
process's threads are part of it, not processes of their own. A process list that cannot be
read in an interval, for want of a file descriptor for example, is unknown, not empty: users
already counted as principals keep their counts, own work's fair share on each resource is at
most the last one a readable list gave (`fair_share_held` when that holds it down), and each
external process keeps its memory peak, until the list reads again. With no readable list before
it, the share is computed as for one unattributed principal.

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
work; while a small demand is probed it steps down with the probe instead. While own work
holds more than capacity minus Yield First's margin, saturation may be its own doing, so
Yield First first holds it to the margin within the fair share and pauses all of it only if
the contention remains. Neutral holds the max-min fair share. No CPU rate cap applies, in
Tracked or in Full (where `cpu.max` is a launch setting only): Yield First and Neutral pause
the whole tasks whose usage together comes nearest what own work holds over its target, so
neither own work nor the other user loses more than half the smallest task; a task more than
twice the excess is therefore not held, and the other user then gets what plain Linux gives it.
A pause still answering its demand at `pause_bound_s` (10 s) moves to other tasks chosen the
same way, so the held amount stays and no task is drained for CPU. While own tasks are paused,
new own work is admitted only beyond the room they would take back. Where every task is
paused, as in Yield First, the pauses take turns, one task running at a time. A task that is
draining makes no other task hold. A pause for the operator envelope drains at the bound. A job's own mode, within
the node cap, decides how that job gives back: a job in Performance or Aggressive is never held
for another principal, for unknown activity or at a floor breach, whatever the node's mode, and
only the operator's envelope steps it, on memory and GPU memory only. The node's other jobs then give what its target needs as
far as they can, so another workload can get less than the node's mode promises; this is a known
limitation of per-job modes. Performance, the default, is held to doing better for own work
than the same work started on plain Linux: no gate shape may fall behind it (SC-018), and the
gain gates measure the gain (006 SC-004, SC-005). Performance
takes no step on running work for another principal, for own usage above its admission target
or for activity of unknown amount, and only the operator's envelope still steps it, on memory
and GPU memory only: own CPU above the envelope's CPU takes no yield, cap or drain, as plain
Linux would run it (006 FR-007a). What another principal gets is the kernel's and the driver's
split beside the same work. It holds
back at admission only, and only for another account whose CedeGrid work arrived first on the
resource (feature 006). Another account is a CedeGrid principal on a resource while the agent
identifies, read-only, that its work running there runs under CedeGrid; every other account,
and usage CedeGrid cannot attribute or identify, is a plain principal. Own work's arrival on a
resource is the earliest among its running allocations there. A CedeGrid principal is a
first-comer when its work there arrived before own work's, at the same time, or when either
time is unknown or own work runs nothing there; it is a protected first-comer when it runs
Performance or Aggressive work there. Beside plain principals and CedeGrid work that arrived
later, Performance and Aggressive start new work as plain Linux would: up to the capacity less
the operator reserve, within the envelope and the ancestor quota, with no margin, no deduction
of others' usage and CedeGrid's own CPU left in, and on GPU compute without the time-slice
admission hold; VRAM headroom and the memory floors still apply (`admitted_as_plain`). Beside
a first-comer, admission holds as below, counting only first-comers: new own work starts only
within the larger of the fair share and capacity minus their observed usage minus the margin,
and nothing new starts while their activity is of unknown amount (`held_for_first_comer`).
Accounts' CPU is read host-wide, so an agent scoped to fewer CPUs counts each account's CPU at
the part of the scope's measured external usage it can be: work on the host's other CPUs does
not hold the scope.
While the principal list cannot be read, no account is known to be plain, and the hold counts
every principal. Every decision in Performance and Aggressive also carries an admit immediately
budget, which holds only for protected first-comers (on a device held for one, it carries no
VRAM); a run with admit immediately is placed against it, and the decision says when a
protected first-comer kept the hold (`admit_immediately_ignored_for_protected_first_comer`).
Each compute resource's status lists own work's arrival and, for each other account that
contends or runs CedeGrid work there, its kind, arrival, whether it came first, whether it is
protected and whether admission holds for it. In Neutral, and in Performance and Aggressive
beside a first-comer, while another principal contends and the others leave own work its
equal share of the capacity less the operator reserve, within a principal's floor (1% of
capacity), own work keeps that equal share for admission: the margin is room for others'
growth past an equal share, which no mode owes, and readings that close to the share do not
move it. Running own work keeps that equal share whatever room the others leave, wherever the
principal list reads: a principal that took the room own work left by running below its share
does not hold its paused tasks below it (SC-003), while admission still gets only what is left.
CPU that no readable account accounts for stands for no principal while it is within how far the
account list itself read above the scope over the last four principal windows, or the scope's
measurement band, and has not lasted the principal window; a larger gap, or one that lasts, is one
more principal at once.
Otherwise Yield First keeps 15% of capacity free, Neutral 5%, and Performance 2%
where it holds for a first-comer and nothing elsewhere; the margins are validated as Yield
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
margins are configurable per resource. In Yield First and Neutral, contended CPU returns
within 2 monitor intervals; Performance takes no CPU step for others. Own CPU usage over its target while the budget did not fall
(external usage did not rise beyond its per-interval bound and no contention lowered the
target) is enforced first: held caps keep their targets, which supervisors hold from the
measured rate, and no step is entered until usage has stayed over the target for
`min_step_dwell_intervals` with every held cap applied; then own work steps with cause
`own_over_target`, shared at one level within each mode tier, and such a step reverses
(`step_reversed_own_usage_fell`) once usage falls back and the cap no longer binds. A
budget that fell steps at once with `external_growth`. Caps already held give an external
increase between them, lowered at one level within their tier, rather than each by the
whole increase. While a resource is not contended, a held cap or hint rises again toward
the mode's target at the mode's scale-up rate. On CPU, once every external principal, the
unattributed remainder included and background left out, has stayed below
`principal_min_percent` for `principal_window_intervals`, the CPU allowance returns in one
move, and in that interval, while own work holds no excess, every binding cap, hint or pause
held for external growth rises in one move without the step dwell, the lowest first and none
lowered, so that together they hold the target; later rises, and steps held for other causes,
wait for the step dwell alone. Own usage is back within 3 s of the contender stopping in Neutral
and Performance, and in Yield First, whose quiet count is that mode's hysteresis, within 2
intervals of the quiet count. While contention only falls, the scale-up interval applies. A Guaranteed allocation's
CPU is never capped below its request, by the policy or by its supervisor.

CPU and RAM each wait for their own cooldown, counted from the time each became stable:
`contention.cpu.scale_up_cooldown_ms` and `contention.ram.scale_up_cooldown_ms`. Unset,
each is its mode's scale-up interval: 30 s in Yield First, 10 s in Neutral, and in
Performance one monitor interval for CPU and 2 s for RAM. In Yield First an interval in which own
work does not fit restarts that count only when a principal is the cause: a user became a
principal in it, or unattributed usage reached `principal_window_intervals`; or the principals
already admitted rose together, or on CPU unattributed usage already past the window rose, by more
than `noise_tolerance_mib` on RAM or, on CPU, the larger of `principal_min_percent` of capacity and
the per-interval measurement band, since the last interval in which own work fit. Anything else
does not, whether or not other principals are admitted: the owner's unmanaged processes, a change
by a user that is not a principal, and unattributed usage shorter than the window. Such a change
still counts in its share and holds admission through the headroom while it lasts, and an
unreadable principal list restarts the count. A process that starts and ends between two samples
is in no user's usage, so on a host that hides other accounts' processes, where their usage is
unattributed and past the window, an owner's login can still restart the count on CPU. Each resource's admission opens on its own, and placement needs every
requested resource open; every launch reserves CPU and RAM. Own work blocks admission on a
resource (`admission_own_work_yielding`) only while it is being reclaimed there: while it
holds more than its target, or a step is still moving down. A cap or hint that holds at its
target does not block, and coordinator placement follows the node's admission without
counting steps. When CPU or RAM admission is closed only by the node's own new allocations
that have no first measurement yet, or by the cooldown only they restarted, or GPU compute
admission only for one more own process beyond tasks placed there and not yet listed, which
the time-slice rule allows to run, the report says so for that resource, and the coordinator
still authorizes reservations it placed earlier while they fit the budget; it places nothing
new there, and any other cause or a report without that statement still refuses. A move within the noise tolerance on memory, or within the reversal hysteresis
of the external growth a compute step answered (`hysteresis_percent` of that demand), is
jitter: it blocks no admission, and a step that holds within it resets no cooldown. A pause
that moves to another task at its bound is the hold staying, not a step down. Admission then follows headroom, cooldown and reclaim backoff: own
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
resource through the yield ladder, releasing opportunistic work first; on CPU, only for work in
Yield First and Neutral, and not at all where the node's CPU mode is Performance or Aggressive.
Where such a node's CPU work contends with a plain principal and no first-comer holds it, the
decision says so (`cpu_beside_plain`), and the agent may start tasks past the envelope's CPU as
plain Linux would, up to its worker limit and memory. A plain principal contends only on a
saturated CPU: a named account at the contention floor on a CPU full in this interval, even
while own work holds more than half of it; usage no account accounts for only on the saturation
Performance judges, after the principal window, and not within two monitor intervals after an
own task stops, when it is that task's last usage. The
flag sets after three intervals in a row of such contention, or at once, before own work has
filled the CPU, for a named account taking at least a whole CPU, so a batch starts at once
beside it as plain Linux starts it; it holds 10 s after the contention ends.
Performance and Aggressive release no CPU reservation that was admitted, and own charges over
the CPU budget restart no cooldown: new work waits through the headroom until they fit, and
work already placed is still authorized. Guaranteed work is
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
critical path the window is skipped. In Yield First and Neutral, by the resource's mode, an
opportunistic allocation that holds none of the memory does not hold that drain back, and one
whose reading is missing counts by its charge; in Performance and Aggressive every
opportunistic allocation charged on the resource still does.

Floors hold by mode. In Yield First and Neutral own memory gives way at the floors: memory
above its memory fair share (the max-min share beside the RAM principals, or beside the
device's GPU memory principals) first, then the rest. In Performance and Aggressive own memory
is not hinted, drained or sent TERM for a floor breach, on the critical path and for Guaranteed
work too, and no own task is marked for the OOM killer except under
`ram.prefer_own_oom_victim`: the kernel decides at exhaustion, as beside the same work started
without CedeGrid, and the floors still close admission. The operator's envelope applies in
every mode. In Yield First and Neutral each in-share task, and in Performance and Aggressive
every own task, gets one save-progress request per breach episode,
whoever caused the breach: the target file's `save_progress_sequence` rises and both SDKs call
`on_save_progress`, which is no drain and asks for no memory back. An over-share task gets its
hint instead. The kernel's out-of-memory killer may then take another
workload's process, as it would without CedeGrid. This is the one memory exception, a rule of
those modes and not an option; the operator envelope applies in every mode.

Own tasks give memory back by fault as well. While own memory is above its fair share, the
over-share tasks, those that make up the excess in the memory victim order, come first in every
mode. They are asked together for the excess and no more, the last only for the part that
completes it. Each is hinted if it takes hints and drained when its hint deadline passes with the
excess still held, or drained without the hint if it takes none. A task whose usage at the
deadline is at or below the larger of the target its hint began with and the current target has
complied and is not drained for that hint: while own memory is still above its share, as when
another own task grew meanwhile, and the victim order still selects it, it gets a new hint with
the current target and a new deadline. The reclaim bound then counts that new excess from the
interval that first shows it, so the floor can stay breached one more hint deadline for each
such growth. A target a task cannot reach, such as 0, is not complied with. A drain already decided counts
in full while the task still runs, and a hint within its deadline, or a Guaranteed task whose
drain waits for the breach window, counts what was asked, so the excess is passed to nobody
meanwhile. A hint's deadline runs from the later of its last change and the need's return, for
counting as for its next step. What a task was asked and did not count, once its wait is over
with no drain (a Guaranteed task that may not be drained yet), passes to the tasks after it in
the victim order and is never asked of that task again. So no more over-share tasks are drained than bring own memory within its share. What
the floor, or the mode's target, needs beyond the excess (in Yield First its margin) is asked in
the same interval by the victim order, each over-share task counted at what it holds less what
it was already asked, and in-share tasks give it only where in-share memory gives way: in Yield
First and Neutral, and in Performance and Aggressive for a breach own work caused. Beyond its
excess an over-share task is asked only what that order asks of it, so one that has the least
progress to lose may be emptied for the margin while another task keeps more. A drained task is admitted again once
its memory fits. The cost is time: free memory can stay below the floor for the over-share
task's wait, the hint deadline plus the drain for a task that ignores its hint and the breach
window for a Guaranteed RAM task off the critical path, where the in-share tasks were hinted
one interval later before. The reclaim targets above do not change. The order applies per
device for GPU memory and not while the fair share is unknown.

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
failed GPU allocation beyond what the same work started without CedeGrid would cause; the
floor rule of Performance and Aggressive above belongs to those modes themselves. The constitution allows one risk to other workloads' compute, slower compute, and
only under unprotected.

Aggressive is the fourth mode, after Performance. It is Performance with no admission margin:
it admits up to all idle CPU and GPU compute at once, where Performance keeps 2% free. Like
Performance, it takes no compute step for others, and its memory is Performance's. A
configuration names it only with `contention.acknowledge_caution = ["aggressive_mode"]`, and it
runs only where `contention.max_mode = "aggressive"` is set explicitly. Without that setting,
the node cap stops at Performance, and an Aggressive setting runs as Performance with a
`mode_capped` warning.

Protected Aggressive, the default form, keeps Performance's admission rule. Beside an account
whose CedeGrid work arrived first, new own work starts only where that account keeps the lesser
of its demand and its fair share, and never into its activity of unknown amount; beside every
other account it starts as plain Linux would.

Two per-run options change this for Performance and Aggressive work; on Yield First or Neutral
work they have no effect.
- Admit immediately (`admit_immediately` with `confirm_admit_immediately`, a caution option)
  starts new work beside a CedeGrid user that arrived first and runs only Yield First or Neutral
  work. Beside one that arrived first in Performance or Aggressive it is ignored.
- Unprotected (`unprotected` with `confirm_unprotected`, at the highest warning level) implies
  admit immediately and raises own work's CPU share above other users' where this host gives a
  way that reaches their work: the account slice's CPU weight through the System mode broker,
  where the host splits CPU by account, or a nice limit the administrator granted the account,
  where own work shares a CPU cgroup with theirs. It never raises while a CedeGrid user that arrived first runs Performance or
  Aggressive on the CPU, and GPU compute gets no raise.

Unprotected starts only when all of these hold:
- the submission sets both `unprotected` and `confirm_unprotected`;
- a person turned on the host's allowance in interactive `cedegrid setup`, after the host's
  numbers and a fixed phrase;
- on a System mode host, the administrator policy sets `modes.unprotected = true`.

The allowance lives in the agent's state database. It counts as off whenever that database
cannot be read, and a restore resets it. No configuration file, environment variable,
migration or saved setting can turn it on, and a file that tries fails to load. Neither option
starts work past the floors or uses real-time scheduling classes, and neither changes anything
about memory. A refusal names every missing condition. A run that starts logs one line: a
caution for admit immediately, a warning for unprotected.
In Performance and Aggressive no compute step is taken for others. The former floor policies `b` and `c` and Neutral memory policy
`b` were removed, and a configuration that sets them fails to load with a message that
names the replacement settings. Every configuration key is classified as safe, bounded
or caution. Three options are marked *caution* because they let own work hold more
capacity within the floors: a floor below twice its minimum, a margin below its default,
and a detection setting beyond its default (`saturation_percent`,
`principal_min_percent`, `noise_tolerance_mib`, `contention.uid_min`,
`contention.gpu_compute.attribution_tolerance_percent` or
`contention.gpu_compute.slice_bound_percent` above it, `contention.uid_max`,
`memory_peak_window_s` or `quiet_window_intervals` below it). Performance is the default mode and needs no
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
timeout and termination grace at most 60 s. The GPU sample age must exceed the monitor
interval: unset, it is max(2000 ms, twice the interval), which `doctor` and `validate` show as
derived; set at or below the interval, the configuration is refused on every host, with an
error that names both keys and the smallest value that loads. Decisions and node reports show, per
resource, how much is held back and why. Without `contention.max_mode`, a job may
request at most the node's most aggressive configured mode, up to Performance, so no job runs
in a mode beyond what the node's configuration allows. A file that sets no mode runs Performance and gets one
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
  work, `unknown` while a measurement is unknown or held, and `peak`, `cooldown`, `backoff`
  and `charge_shortfall` from admission. The parts kept from own work add up to capacity less
  others' use (`external`, which includes `unattributed`) less the budget. The GPU memory noise
  tolerance is a growth threshold and withholds nothing, so it is not a part. `ceiling` is what a visible
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
| `cpu_window_estimate_not_kept` | The principal list does not show another account's process, so no CPU estimate is kept for a window's refill. |
| `inconsistent_within_tolerance` | Own usage exceeds what the host reports within the known error, so external usage is clamped. |
| `first_observation_baseline` | External GPU memory at the first observation is the baseline, not growth. |
| `vram_noise_below_tolerance` | External GPU memory grew within the noise tolerance. |
| `mode_capped` | A job's requested mode is above the node cap, so it runs at the cap. |
| `probing_demand` | Own work steps down by one margin step to learn whether a small external demand is satisfied. |
| `fair_share_held` | The principal list cannot be read, so own work's fair share stays at the last one a readable list gave. |
| `launch_nice_kept` | A task launched at the Yield First nice keeps it in another CPU mode, since it cannot be lowered again; its caps do the yielding. |
| `watching_larger_demand` | Own work holds the fair share while a larger external demand is watched for one response window. |
| `satisfied_demand` | An external user's demand is recorded as satisfied, so it no longer contends below that demand. |
| `gpu_time_slice_fair` | Own work holds no more than its fair share of the time-sliced device's busy contexts, so the driver's slices keep it fair without a GPU compute cap. |
| `gpu_time_slice_unbound` | Own work holds more than its share of the time-sliced device's busy contexts, but no other principal reads slice-bound, so the driver's slices already give each what it asks for without a GPU compute cap. |
| `gpu_time_slice_hold` | Own tasks on the time-sliced device are paused down to the running ones the time-slice rule allows beside the slice-bound principals. |
| `gpu_first_come_floor` | Own GPU work was on the device before every other principal listed there, so it keeps at least the share that leaves none of them more than half of the device. |
| `gpu_time_slice_admission_held` | One more own process on the time-sliced device would take a slice-bound principal below its fair share of the busy contexts, so GPU admission holds. |
| `admitted_as_plain` | No account that arrived first with CedeGrid work contends, so Performance starts new work as plain Linux would. |
| `held_for_first_comer` | Another account's CedeGrid work arrived first and contends, so admission holds at the fair share for it. |
| `admit_immediately_ignored_for_protected_first_comer` | An account that arrived first runs Performance or Aggressive work, so admit immediately does not lift the hold for it. |
| `applied_duty_credit` | Own GPU work is credited at what its supervisor applied: a paused task at nothing. |
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
| `config_reloaded` | The agent applied a reloaded configuration's contention settings from the next interval. |
| `config_reload_refused` | The agent kept its configuration because the reloaded one needs a restart or could not be read. |
| `report_stale` | The coordinator renews no lease and offers no work while the node's last report is stale. |
| `single_process_violated` | Processes outside the run's verified family were found; they are not measured as own work or stepped. |
| `family_members` | The run has more processes than its leader; its verified family is measured and stepped as own work. |
| `cpu_unknown_fair_share_hold` | No longer emitted; kept for recorded decisions from before Performance stopped stepping for unknown CPU. |
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
| `held_back_noise_tolerance` | Never shown: the GPU memory noise tolerance is a growth threshold that withholds nothing. |
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
and its decisions say so (`launch_nice_kept`). Own priority is raised above the default only
by the unprotected option and by priority over plain users, below; neither uses real-time
scheduling classes.

Priority over plain users. With the administrator's `modes.plain_priority` and the user's
`contention.priority_over_plain`, in System mode on a host that splits CPU by account, the
broker sets the user's account slice to `modes.plain_priority_cpu_weight` (200 by default)
while own Performance or Aggressive CPU work runs and every other account with CedeGrid work on
the CPU runs at that weight or more. A plain user's CPU share then shrinks: one plain user at
weight 100 against 200 gets about a third of a contended CPU. Between CedeGrid users the option
changes nothing and the first-comer rule decides, as without it: the later user's new work waits
at admission rather than take the first-comer below the lesser of its demand and its fair share.
Another account with CedeGrid work on the CPU that falls below that weight ends the raise within
two monitor intervals. Rootless hosts never raise.

At start the agent raises its own soft `RLIMIT_NOFILE` to its hard limit, and the worker
gate gives each task back the soft limit the agent started with before the user's command
runs. The agent takes a launch only while its open descriptors, 8 for each launch in
progress and 64 kept free fit under that limit; otherwise it advertises fewer launch slots
and reports `launch_refused` with code `descriptor_limit`. Running out of descriptors
(`EMFILE` or `ENFILE`) never ends the agent's loop: that interval decides nothing new and
every step holds. A journal write still locked after its 5 s wait does not end the loop either:
the interval ends there, what it had already sent to its supervisors stands, no observation is
recorded and no report is sent, and the next interval samples again (`journal_busy`, once per
stretch). While intervals have ended before a sample was acted on for longer than
`lifecycle.allocation_lease_ms`, the agent asks for no lease renewals of Opportunistic work and
drains nothing, so those leases run out as for a stale report (`renewals_withheld`, once per
stretch), and it asks again after the next interval that samples; an interval that ends at the
observation write has its sample and keeps asking. It keeps asking for Guaranteed work, whose
lease running out drains nothing and whose renewal reply carries a drain, cancel or preemption.
An allocation whose supervisor has exited is asked no renewal in any class.
A supervisor that drains releases its steps, writes the drain notice and sends TERM and KILL
when they are due, counted from the request, and records the drain after the notice; on the
critical floor path TERM also comes before the step's record. A record that finds the journal
locked waits about 50 ms and is tried again on later passes, so a locked journal delays the
record and not the drain; until it lands the journal still shows the run as running. After 20 s
of refusals the supervisor reports `journal_unavailable` and Opportunistic work drains, as
before, each time a record that drains is refused past those 20 s or given up. An SDK child's
Running and Draining records follow the same rule: a refused child record never stops the child
itself, and one given up drains Opportunistic work as the leader's does. A record still owed
when the run is released is not written: Released supersedes it.

A running supervisor waits for a control line, its leader's exit through the pidfd or a
new SDK connection until its next deadline: the lease, drain,
TERM and KILL deadlines, the readiness check and the release confirmation, and passes at
least once a second. Without a pidfd, while SDK connections or mediated children are
active, or while a control or drain-file write fails, it passes every 5 ms. While a journal
record is outstanding it passes again when the record is next tried, every 250 ms.

The Linux supervisor acquires a pidfd before any child reaping, verifies boot ID,
start time, assignment ID and generation, and uses the stable handle for signaling
and exit observation. On failure or unsupported platforms it can supervise only its
exclusively owned, unreaped direct child, with weaker guarantees explicitly recorded.
It refuses a required stable handle if unavailable. This fallback depends on no
competing reaper or SIGCHLD changes. Restart does not trust a stored numeric PID:
there is no automatic adoption/signaling path. A leader pidfd does not identify or
contain descendants. On Linux the supervisor is the run's child subreaper, set before it
forks the gate, and the run's family is the leader, its SDK children and its verified
descendants, at most 256 members. Discovery runs every interval and in every stop, over the
`children` list of every thread of every member and of the supervisor, or a scan of `/proc`
where the kernel keeps no such lists, which `doctor` names. The supervisor forks nothing but
its gates, so a child of the supervisor that is not one of them, and was not there at EXEC,
is an orphan of this run. Orphans are reaped by their own PID, never by a wait for any child.
Elsewhere rootless execution supports single-process commands and opt-in SDK-mediated
direct children. Each mediated child has its own durable identity,
verified handle and launch barrier. The shared parent resource envelope covers the
whole family, with each live usage counted once. Uncertainty in any registered child
prevents family release.

No actuator reaches a process outside the verified family: signals go only through the
leader's, a mediated child's or a verified descendant's own handle. Every interval the
supervisor counts, read-only, the processes outside the run's verified family: the leaf's
members in Full, and in Tracked the descendants that failed a check or are past the bound.
When it finds more than before it reports `single_process_violated` with the count and the
reason (`ids_differ`, `chain_unconfirmed`, `over_bound`) in its status, `explain` and the
run's outcome. Those processes are not measured as own work and never stepped (FR-082). A
descendant is acted on only when it is verified: its pidfd is opened before its parent chain
is read; the chain leads to a member, or to the supervisor for an orphan; it is confirmed
alive through that pidfd afterwards; and its real, effective and saved user and group IDs
match the run's. It is acted on only through that pidfd, or for `oom_score_adj` through its
`/proc` directory checked against its start time, so a reused PID is never acted on. A
verified family is measured as own work: CPU summed over its members (a member that exits
counts up to its last sample), memory by the members' summed proportional set size, and GPU
memory and compute per process, so none of it is the owner's unmanaged use (FR-085). In
Tracked a family of more than its leader has its proportional set size read every K intervals,
K chosen from the timed read so it averages at most 0.5% of the scope's CPU, from 1 to 10;
between reads its memory is the last reading plus the members' resident growth since it, and a
memory step, preferred-victim mark or save-progress signal on that estimate is taken only after
a fresh read confirms it. In
Tracked a stop reaches every member and repeats discovery until a pass finds no new member,
at most three passes; a member that joins while the family is stopped is stopped at once,
and continue reaches every member past one that fails. Each member's `oom_score_adj`
baseline is its own value when it joins, or its parent member's when it started while the
run was marked (FR-086, FR-023). A descendant
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
allocation. `cpu.max` is a separate opt-in launch bandwidth cap; there is no default quota,
and no runtime step writes it.
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
three seconds. Hung kernels/drivers can prevent timely release. A locked node journal does
not delay these milestones: the supervisor's records follow its signals.

A living independent supervisor can retain deadlines without an agent. The local
CLI currently combines the caller and supervisor, and has no remote channel. The
lifecycle reducer distinguishes coordinator disconnection (existing lease unchanged),
agent failure (opportunistic drain), and supervisor failure (needs reconciliation).
A dead supervisor cannot enforce deadlines after its own death; no external reaper
is claimed. On Linux the worker gate sets a parent-death signal on its own process before
exec: SIGKILL for Opportunistic work, so no copy runs uncontrolled beside a replacement,
and SIGCONT for Guaranteed work, so it is never left stopped. The supervisor handles
SIGTERM, SIGINT and SIGHUP by releasing its runtime steps and draining, and a supervisor
that reattaches under the cgroup backend thaws a leaf whose journal records no pause,
after verifying the leaf's membership against the family (FR-078). The parent-death signal
reaches only the leader and SDK children: in Full the other members stay in the leaf, kept
for reconciliation. In Tracked the agent recovers them, when it sees the supervisor gone or
at its next start, from the family it recorded in the run's attempt directory: for each
process a pidfd first, then its start time and real, effective and saved IDs through `/proc`
equal to the record and its boot unchanged, then alive through the pidfd, then SIGCONT for
Guaranteed work or SIGKILL for Opportunistic work through that pidfd. A process that fails a
check is never signalled; the counts go to the run's recovery record and an event (FR-088).
A member the supervisor found and stopped in the instant it died can be missed, and stays
stopped until an agent runs. The workload leads a session of its own, so no exit orphans its
process group into SIGHUP, and standalone `supervise` drains its run on SIGINT. Any
uncertain surviving allocation remains reserved until verified reconciliation, even if its
task is replay-safe; a dead Tracked run is released only once its recorded descendants are
gone too.

A run ends when its leader exits: members still alive get SIGTERM, then SIGKILL after
`term_grace_ms`, in both classes, and the outcome counts them as `members_ended`. A drain
signals the leader first and SIGKILLs members still alive after the grace; a leader that
exits during a drain leaves its members to the drain's deadline, with no second grace
(FR-087). Processes outside the family outlive the run, and in Full keep its leaf nonempty.

SIGTERM, SIGINT and a restart stop the agent and leave its supervisors running; the next
agent re-adopts them, retrying on later intervals while a supervisor lives, in replay
mode too. The end of `max_runtime_seconds` drains every allocation a share at a time: each
interval as many as the agent's last synced journal write fits in 250 ms, and at least one. An
explicit operator drain reaches each allocation with its next lease renewal or heartbeat reply
(an Opportunistic allocation whose renewals are withheld for want of a sample, with the first
reply after its node samples),
within one heartbeat interval, or, while the agent's intervals end at a locked journal, with
the first renewal after the interval that was waiting, about 5 s; it is not spread, and its
lease is not extended from then on. A drain in a renewal reply is handed to the supervisor
with or without time left on the lease; one that finds the supervisor gone is reported once
(`drain_undelivered`).
Every process that writes the node journal retries a locked journal after randomly spread
sleeps of at most 16 ms, for at most 5 s. A supervisor's records while its work runs wait
about 50 ms in a pass and are tried again; its launch and release records wait 5 s. The
agent's loop never waits for its own pacing of coordinator requests: heartbeats go at their
interval, renewals before their deadlines (except Opportunistic ones while they are withheld for
want of a sample)
and failure reports at least 16 a second, whatever
`max_transfer_bytes_per_second` is, stopping or not. The one exception is an interval in which
an attempt's pin finds the journal locked for 50 ms: the reports still waiting go in the next
interval. SIGHUP reloads the configuration: contention settings,
modes included, apply from the next interval (`config_reloaded`); a change to a setting
that needs a restart keeps the running configuration (`config_reload_refused`) (FR-080).

A node report carries the agent's monotonic age of its sample. While the node's last report
arrived with a sample older than `telemetry_ttl_ms`, the coordinator renews no lease and offers no work to that
node (`report_stale`); it drains nothing on that report, and only lease expiry drains
(FR-079).
An agent heartbeat alone cannot renew a coordinator-issued remote lease. Lease
renewals carry assignment generation and fresh coordinator authorization; receiving
stale messages cannot extend them. Enforce local durations with monotonic clocks,
not wall-clock timestamps. Reboot invalidates process and lease identity.

Guaranteed allocations survive coordinator loss. Opportunistic allocations drain
when authorization expires. This distinction is an explicit allocation contract,
not inferred from a node's name or network location.

## Results, retries, and storage

Node history is bounded by fixed constants: a heartbeat reports unreleased executions and the
newest 256 released ones; observations are kept for 24 hours and at most 200,000 rows; an
attempt's directory is removed once its outcome is receipted and its spool reclaimed, outside
the reported window. Each interval reads only unreleased and not yet settled runs (FR-081).

A first authorization the coordinator refuses is not the task's failure: nothing ran, so a
replay-safe task is requeued with the yield backoff, without a failure or an attempt, and the
allocation's detail says so; a task that isn't replay-safe needs reconciliation as before.
A task killed by an out-of-memory condition it did not cause is requeued without an attempt
up to three times `max_attempts`; later such kills count as attempts. A known leaf
`oom_kill` count decides whether a kill was the OOM killer's, whatever the host count shows
(FR-076).

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

## System mode

System mode applies only where an administrator installed `cedegrid-system`; the default install
runs without privileges. The administrator page is [System mode](system-mode.md).

What runs as root:

- One process, the broker, runs as root. The agent, the supervisors and all user work run with
  the user's rights.
- The broker identifies a caller only by the socket's peer credentials and serves only current
  members of the `cedegrid` group, never user ID 0 or system accounts.
- A request that names a user, unit, path, cgroup, program, mount, device or capability is
  refused; every request and its result is written to the journal.
- The broker starts and stops only the two units it made for the caller, recognized by name and
  by a marker, and builds every property from the caller's user ID and the administrator policy.
- It moves only the caller's job between the caller's two units, frozen from the check to the
  move, and only when every process in it has the caller's user and group IDs.
- It writes `oom_score_adj` only for processes inside those units, through a pidfd-held
  `/proc/<pid>` directory, and reads every value back.
- It writes no other file except the moved job's cgroup files and `cpu.idle` on the Yield First
  slice, and changes no PAM, login, sysctl or kernel module setting.
- Purging the package stops every unit the broker made and removes what the package installed.

Limits:

- The allowed modes in the administrator policy are a CedeGrid setting, not a security boundary;
  the limits the kernel enforces are the units' properties (`TasksMax=` and the process limits).
- With `units.pam_session = "none"`, the default, login limits from PAM do not apply to System
  mode work.
- Yield First work runs in the ordinary unit in this version and yields with the delegated Full
  steps; the idle slice and unit run no work yet.
- Where the host does not split CPU by account, or with `units.ordinary_slice = "cedegrid"`,
  System mode work competes in `cedegrid.slice` beside `user.slice` and `system.slice`, which
  changes the CPU split between those groups.
- With logind's default `RemoveIPC=yes`, a user's System V IPC objects and POSIX shared memory
  are removed at the user's last logout, also while their units run.
- In-share own memory is protected only while another user is above their fair share, and only
  for processes inside the units.
- When a small process outside CedeGrid causes an out-of-memory condition, the kernel can still
  kill a larger process.
