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
  "targets": {"ram_mib": 6144, "gpu_memory_mib": {"GPU-…": 10240}, "cpu_millicores": 2000}
}
```

- **Version.** Readers ignore unknown keys. They reject `version > 1` by treating the file as
  absent.
- **Relaxing.** A relaxed target is a new file with a higher `sequence`. An absent file means
  no target.
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
- `ResourceSignals.from_env()`: needs only the two environment variables. It is usable in
  managed children, which have no context.

**TypeScript** (`npm/src/worker.ts`): `resourceTargets()`, `onResourceTarget(cb)`,
`ResourceSignals.fromEnv()`, with the same immediate delivery.

## Managed children

When the supervisor creates a managed child's private directory, it writes the current target
there before the child starts, with the same sequence and deadline, so the child sees it on its
first read (FR-035).

## Compatibility

- The v2 context gains optional keys only: `resource_target_file` and
  `capabilities: ["resource_targets"]`.
- Both SDKs keep accepting `schema_version == 2`.
- 0.2 workers ignore the new variable and keep their drain behavior (FR-036).
