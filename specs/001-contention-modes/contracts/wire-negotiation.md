# Contract: coordinator ↔ agent feature negotiation (protocol v1, additive)

## Feature names

| Feature | Meaning |
|---|---|
| `api/1` | Today's protocol |
| `contention/1` | Everything below |

## Sequence

1. **First heartbeat.** The agent sends a 0.2-shaped `NodeReport`, with no new fields.
2. **Coordinator reply.** A coordinator that supports the feature replies with
   `HeartbeatReply.features = ["api/1", "contention/1"]`.
   - A 0.2 agent ignores the field: `HeartbeatReply` has no `deny_unknown_fields`.
3. **Later heartbeats.** After it sees `contention/1`, the agent includes these fields:
   - `NodeReport.features = ["api/1", "contention/1"]`
   - `NodeReport.contention = {resources: {<ResourceKey>: {mode, admission, own, external, target_share, step_summary, held_back}}, exceptions: [..], max_mode}`
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
   - uses the 0.2 admission fields for it.
6. **Coordinator restart.** The agent re-learns features from every reply. If an advertised
   feature disappears, the agent drops the new fields on the next heartbeat.

## Placement

With `contention/1`, the coordinator places work using per-resource admission:
`NodeReport.contention.resources[*].admission` must be `open` for every requested resource
(FR-028).

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
