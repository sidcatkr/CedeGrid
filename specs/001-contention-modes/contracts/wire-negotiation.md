# Contract: coordinator ↔ agent feature negotiation (protocol v1, additive)

## Feature names

| Feature | Meaning |
|---|---|
| `api/1` | Today's protocol |
| `contention/1` | Everything below; frozen when released, so a later field needs `contention/2` |
| `lease_batch/1` | `Request::Leases` and `Response::Leases` (FR-074) |

## Lease batches (`lease_batch/1`)

- **Request**: `{"op": "leases", "items": [..]}`; each item is a `prepared` or `renew` request
  with the fields of the single request. At most 128 items and 1 MiB per request; the agent
  splits larger sets.
- **Order and clock**: the coordinator checks the session, advances its clock and expires leases
  once, then serves renewals first, by lease deadline, then initial authorizations, all at one
  clock value, in one transaction with one synced commit.
- **Per item**: each item runs in its own savepoint and gets its own result, `lease` or
  `error` with the single request's message. A preparation row is written before the item's
  grant, so it survives a failed grant, as for a single request. A retried batch is idempotent
  per item (the same sequence rules as single requests).
- **Events**: the agent emits the same per-item events as for single requests
  (`authorization_unavailable`, `lease_unavailable`, `control_rpc` with `operation: "renew"`).
- **Old coordinator**: it never advertises `lease_batch/1`; if a batch is refused anyway, it was
  refused before any state change, and the agent forgets the feature and sends single requests.

## Sequence

1. **First heartbeat.** The agent sends a 0.2-shaped `NodeReport`, with no new fields.
2. **Coordinator reply.** A coordinator that supports the feature replies with
   `HeartbeatReply.features = ["api/1", "contention/1"]`.
   - A 0.2 agent ignores the field: `HeartbeatReply` has no `deny_unknown_fields`.
3. **Later heartbeats.** After it sees `contention/1`, the agent includes these fields:
   - `NodeReport.features = ["api/1", "contention/1"]`
   - `NodeReport.contention = {resources: {<ResourceKey>: {mode, admission, own, external, target_share, step_summary, held_back}}, exceptions: [..], max_mode, max_workers}`
   - `AllocationReport.yield_cause` for yielded or draining allocations
   - `AllocationReport.hint_ready` and `AllocationReport.observes_drain` for running
     allocations (FR-062)
4. **0.2 coordinator.** It never advertises the feature, so the agent never sends the new
   fields. A 0.2 coordinator rejects unknown `NodeReport` fields.
5. **New coordinator, 0.2 agent.** The coordinator:
   - serializes `Assignment.request` without `contention`. `LaunchRequest` is
     `deny_unknown_fields` in 0.2 agents, and the new field is
     `skip_serializing_if = "Option::is_none"`;
   - does not place tasks with explicit `contention` overrides on that agent;
   - does not re-send an offer with an override to a report without `contention/1`, as after an
     agent restart, and does not authorize it then; a revoked offer or a recovery import is sent
     without the field;
   - uses the 0.2 admission fields for it.
6. **Coordinator restart.** The agent re-learns features from every reply. If an advertised
   feature disappears, the agent drops the new fields on the next heartbeat.

## Placement

With `contention/1`, the coordinator places work using per-resource admission:
`NodeReport.contention.resources[*].admission` must be `open` for every requested resource
(FR-028):
- the requested resources are CPU, RAM, and each requested GPU's memory and compute;
- a resource the report leaves out is not open;
- the node blocks a resource's admission while own work on it is being reclaimed (above its
  target, or a step still moving down); a step holding at its target leaves admission to the
  node's headroom and cooldown, so `step_summary` is informational and placement does not read
  it;
- a request is placed only while the node's charge including it fits `managed_budget`;
- the legacy `expansion_allowed` and `gpu_expansion_allowed` flags are not consulted.

Placement, preemption and the first authorization share this predicate. Held or blocked
admission never preempts; only an exhausted budget, worker pool or launch slot with every
requested resource open does. Zero launch slots count as exhausted only at the agent's
reported `max_workers`; below it the agent paces its growth and the task waits. One scheduling pass keeps the victims it chose and the charge
it projects once they release, so no two waiting tasks claim the same release.

`managed_budget.cpu_millicores` adds the CPU that running opportunistic work is charged above
its p95 usage, so request-based charges at the coordinator admit by usage (FR-034).

## Retry accounting

- A failure counts as a yield only when its cause records one:
  - the agent-reported `yield_cause`, or
  - the coordinator's own `drain_requests.cause`, persisted when the drain was issued.
- String matching on `detail` is removed (FR-018).
- A cancelled task becomes terminal and is not requeued (audit A18).

## Tests

A compatibility test matrix covers:
- old coordinator with new agent;
- new coordinator with old agent;
- new coordinator with new agent.

It uses recorded 0.2 request and response fixtures for the old peers (SC-008).
