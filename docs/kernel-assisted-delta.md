# Additive local-supervision increment

This records the original additive increment. The completion implementation has
since connected the independent agent/coordinator, authenticated renewals, live
per-allocation accounting, conservative reconciliation, durable artifacts, and SDK.
See [execution contracts](execution-work.md) and [current validation](validation.md).
The historical limits below are superseded only where those records give code and
evidence. Native Linux and authorized two-server acceptance remain separate gates.

The original Rust configuration, pure policy, observation adapters, SQLite WAL/FULL
store and attempt ledger are reused. There is no transport change, SSH dependency,
server-specific core logic or kernel scheduler change. Schema 2 adds optional kernel
snapshots and a local execution journal; schema 1 configuration/replay remains
readable, and the database migrates existing tasks and observations in place.

## Delivered changes and limitations at the original increment

| Area | Implementation at that increment | Limit at that increment |
|---|---|---|
| Diagnostics | Runtime kernel, pidfd probe, cgroup mount/controller/permission evidence, visible ancestor limits, effective CPU sets/topology | Read-only probes cannot prove a later write will succeed; namespace-hidden ancestors remain unknown |
| PSI | System and cgroup scope, timestamps, freshness, availability, totals/deltas | Observation-only; system pressure is not attributed to an external workload |
| Launch barrier | Durable reservation → trusted blocked gate → identity/membership/control readbacks → Prepared → Authorized commit → EXEC | Synchronous supervisor is the exclusive child reaper; other reapers/SIGCHLD mutation unsupported |
| Process identity | Boot ID, start time, assignment, generation and preferred Linux pidfd for owned child | Leader handle is not descendant containment; no persisted numeric-PID adoption |
| Rootless | Linux/macOS owned direct-child supervision with explicit fallback evidence, nice readback | Trusted single-process no-escape contract; no hard CPU/RAM reserve; not a hostile-code sandbox |
| cgroup v2 | Explicit delegated root, authorized control whitelist, owned child group, CPU weight/optional quota, explicit memory controls, readbacks/accounting | Ancestors never changed; existing enabled controllers and domain delegation required; no GPU VRAM enforcement |
| Cleanup | Verified direct-child termination/reaping; confirmed-empty owned cgroup removal | Nonempty groups retained; cgroup.kill unavailable until exclusive membership/descendant ownership is enforceable |
| Failure state | Preparation/authorization failures retain evidence; uncertain execution keeps reservations | No automatic restart reconciler or capacity override; a dead supervisor cannot enforce deadlines |
| Leases | Monotonic reducer separates coordinator disconnect, agent failure, supervisor failure; CLI opportunistic lease expiry drains | Local CLI has one initial lease, no authenticated remote renewal channel or ongoing contention executor |
| Comparison | Plan-first synthetic baseline/contention harness | Linux execution requires explicit opt-in; no measured speedup claimed here |

Availability, permission, configuration intent, successful application and fallback
are separate evidence fields. Diagnostics always report applied=false. Launch
readbacks are saved with the assignment. Required controls must have successful,
nonfallback evidence before durable authorization. Optional fallback is explicit;
partial preparation failures do not silently switch backend and start the workload.

CPU weights are relative within the actual hierarchy, not percentages or guaranteed
allocations. CPU bandwidth is a separate opt-in cap; the default is no added quota.
Comparisons must record effective CPU sets, visible ancestor restrictions and where
both managed and protected workloads compete. CPU/RAM budgets remain policy unless
the corresponding kernel setting was actually applied. Read-only accounting is not
control enforcement.

## Failure boundaries

- Coordinator disconnection does not renew an existing lease. Guaranteed work
  survives; opportunistic authorization expires according to its original deadline.
- Agent failure can trigger opportunistic draining in a surviving supervisor.
  A plain agent heartbeat never extends coordinator authority.
- Supervisor failure before EXEC closes the authorization pipe, so the blocked gate
  exits. A failure after authorization may leave user code running. There is no
  claim of deadline enforcement after supervisor death.
- Any non-Released journal entry remains charged after restart. Replay safety does
  not make uncertain capacity free. Replacement of the same task is refused until
  separate reconciliation proves release.

At this original increment the CLI only inspected retained rows; live per-allocation
usage and the independent agent/coordinator pair were still pending. The subsequent
completion implemented both, including authenticated renewals and CLI reconciliation.
Unknown surviving usage still blocks expansion. See the current
[execution implementation](execution-work.md) and [validation evidence](validation.md);
the historical table above is not the present feature inventory.

## Delivery order retained

Capability preflight → observe-only → local supervision and fault tests → connected
CPU services/artifacts/SDK → bounded real application work → authorized two-node
execution/recovery and controlled GPU sharing → bounded stress. The user replaced
the original required 24-hour run with bounded stress; the long-soak harness is
optional and unrun. Local tests do not complete physical distributed recovery or
GPU-sharing acceptance. Use synthetic or explicitly authorized workloads, never
other students' research as experimental load.

Linux pidfd/cgroup/PSI behavior cannot be runtime verified on macOS. Fixture tests
and cross-target Rust type checks remain separate from subsequent Linux-native
runtime evidence. Actual pidfd supervision, system PSI and owned CUDA release
were later verified in their stated scopes; delegated controls and physical
GPU-sharing/two-node application gates remain separate. Consult
[current validation](validation.md) for each result and unresolved requirement.

## Source and version checks

The implementation probes actual interfaces rather than admitting jobs from kernel
version alone. pidfd_open arrived in Linux 5.3; pidfd_send_signal in 5.1. The actual
handle is acquired before reaping and tested with signal zero; runtime syscall and
permission failures are handled. cgroup interfaces are checked against the versioned
6.6 documentation and probed on the deployment host. PSI can be absent or inaccessible;
system CPU `full` is undefined and is not used as a zero-pressure observation.

- [pidfd_open lifetime, reaping and version semantics](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
- [pidfd_send_signal permissions and version](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)
- [Linux 6.6 cgroup v2 interfaces](https://docs.kernel.org/6.6/admin-guide/cgroup-v2.html)
- [Linux 6.8 cgroup v2 interfaces matching the observed shared-server series](https://www.kernel.org/doc/html/v6.8/admin-guide/cgroup-v2.html)
- [Linux PSI scope and availability](https://docs.kernel.org/accounting/psi.html)

## Deferred research gate

No custom module, sched_ext scheduler, kernel/boot upgrade, global sysctl change,
sudo invocation or BPF loading is part of this implementation. A future observation-
only eBPF experiment needs a specific unanswered measurement question. sched_ext is
a separate research milestone requiring a demonstrated scheduler bottleneck, an
optimization objective, compatible kernel/permissions, administrator-approved
isolation, regression tests and verified rollback. Existing backend/telemetry
interfaces remain narrow; no speculative plugin framework is added.
