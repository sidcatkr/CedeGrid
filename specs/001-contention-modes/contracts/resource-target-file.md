# Contract: resource-target file and SDK API

## Environment

| Variable | Status | Meaning |
|---|---|---|
| `CEDEGRID_DRAIN_FILE` | Unchanged | Existence means drain; contents are ignored. |
| `CEDEGRID_RESOURCE_TARGET_FILE` | New | Path of the target file, in the same private directory as the drain file. |

Both variables are set for the leader and for every SDK-managed child.

## File format

The supervisor writes the file by writing a temporary file in the same directory, calling
fsync, and renaming it over the target.

```json
{
  "version": 1,
  "sequence": 7,
  "deadline_unix_ms": 1793000000000,
  "step": "hint",
  "targets": {"ram_mib": 6144, "gpu_memory_mib": {"GPU-…": 10240}, "cpu_millicores": 2000},
  "allowance": {"ram_mib": 20480, "gpu_memory_mib": {"GPU-…": 18432}, "cpu_millicores": 7000},
  "allowance_sequence": 4
}
```

- **Version.** Readers ignore unknown keys. They reject `version > 1` by treating the file as
  absent.
- **Deadline.** `deadline_unix_ms` is the earliest deadline the policy enforces among the
  current hints (FR-037): the resource's hint deadline, the Guaranteed hint deadline, or the
  critical hint deadline.
- **Relaxing.** A relaxed target is a new file with a higher `sequence`. An absent file means
  no target, and so does `sequence` 0: the first published target has sequence 1.
- **Allowance.** Optional, published also without a target: per resource (CPU, RAM, each GPU's
  memory; no GPU compute in version 1), what the allocation may grow to now without causing a
  step. `allowance_sequence` counts allowance changes apart from `sequence`, so an
  allowance-only rewrite keeps `sequence`, `step`, `targets` and `deadline_unix_ms`. Readers
  that do not know the keys ignore them (FR-071). The policy sets it as follows:
  - its usage, plus its equal part of the room under the mode's allowance after pending
    reservations, less one publish threshold in total, shared over the running allocations on
    that resource; the mode's allowance is `own_allowance` for CPU, and for RAM and GPU memory
    the budget above the floor, less Yield First's margin;
  - at most what its own mode's target leaves it under per-job modes; the room that bound
    leaves is shared over the others, and each rises at its own mode's step and interval;
  - at most its cap or hint target while one applies, and its usage while paused;
  - at most the operator envelope; for Guaranteed work, never below the request;
  - only its current usage while the resource's measurement is unknown.

  If every running allocation grows to its allowance at once, the totals stay within the mode's
  allowance, apart from what running work already holds. The threshold is the contention floor
  (CPU) or `noise_tolerance_mib` (memory). Falls publish together, at once, when the sum over
  the allocations of their published values above their current allowances passes one
  threshold. A rise publishes at most once per scale-up interval, by at most one scale-up step
  (the mode's margin of capacity on CPU; on memory a mechanism constant of 20%, 5% or 2% of
  capacity per mode, since Neutral and Performance keep only the floor there), and only beyond
  one threshold.
- **Acknowledgement.** The SDK writes the highest sequence it has seen to
  `resource-target.ack` in the same directory, starting with `0` on its first poll. The ack only
  shows that the worker can read hints; the supervisor reports it as `hint_ready`, and
  compliance is judged from observed usage.

## SDK API

Both SDKs add the same API.

**Python** (`python/cedegrid/worker.py`):
- `WorkerContext.resource_targets() -> ResourceTargets | None`
- `WorkerContext.on_resource_target(callback)`: called at once with the current target when
  one exists, then once per new sequence, during `draining()` and `safe_point()` polls. Hook
  errors are isolated and logged.
- `WorkerContext.resource_allowance() -> ResourceAllowance | None` and
  `WorkerContext.on_resource_allowance(callback)`: called at once with the current allowance
  when one exists, then once per new `allowance_sequence`. An allowance-only change never calls
  `on_resource_target`, and a target change never calls `on_resource_allowance`.
- `ResourceSignals.from_env()`: needs only the two environment variables. It is usable in
  managed children, which have no context.

**TypeScript** (`npm/src/worker.ts`): `resourceTargets()`, `onResourceTarget(cb)`,
`resourceAllowance()`, `onResourceAllowance(cb)`, `ResourceSignals.fromEnv()`, with the same
immediate delivery. The cases in `tests/contracts/cedegrid-0.2.json` (`target_file_cases`)
are what the supervisor writes and both SDKs read.

## Managed children

When the supervisor creates a managed child's private directory, it writes the current target
and allowance there before the child starts, with the same sequences and deadline, so the child
sees them on its first read (FR-035).

## Compatibility

- The v2 context gains optional keys only: `resource_target_file` and
  `capabilities: ["resource_targets"]`.
- Both SDKs keep accepting `schema_version == 2`.
- 0.2 workers ignore the new variable and keep their drain behavior (FR-036).
