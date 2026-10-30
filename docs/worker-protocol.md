# Language-independent workers

A task names an argv array, an absolute POSIX working-directory path on the
execution host, resource requests, and explicit single-process/no-escape or
managed-family acknowledgement. On Linux a rootless run's forked descendants join its
verified family whatever `single_process` says; elsewhere that contract is required. Submission validates their syntax before opening
transport or consulting worker environment. It does not require the remote cwd to
exist on the submitting computer. Arbitrary executable files need no SDK runtime.

## Plain commands

For an agent-managed plain command, exit zero with confirmed family release
produces a native result with bounded stdout/stderr artifacts. Each stream retains
at most 4 MiB; excess bytes are drained. An unresolved result publication prevents
plain-command fallback. A retained or uncertain process never proves resource
release simply because its numeric PID is absent from an old journal.

## Cooperative context and local RPC

The native supervisor supplies `CEDEGRID_CONTEXT`, a path to a version-2 JSON
worker context, and `CEDEGRID_DRAIN_FILE`. Context includes namespace, session,
task and assignment IDs, exact generation, input descriptors and optional resume
metadata. Context and downloaded inputs live in the session workspace, outside
the authoritative state directory's quarantine namespace. Do not treat an old
path as authority for a replacement session.

The supervisor also supplies `CEDEGRID_SUPERVISOR_SOCKET`,
`CEDEGRID_SUPERVISOR_TOKEN`, `CEDEGRID_NAMESPACE_ID`, `CEDEGRID_SESSION_ID`,
`CEDEGRID_ASSIGNMENT_ID`, and `CEDEGRID_ATTEMPT_GENERATION`. The Unix socket
accepts one newline-terminated JSON request per connection and replies with one
JSON record. A request has `version: 2`, `token`, `namespace_id`, `session_id`,
`assignment_id`, `generation`, `request_id`, and `op`. The native server verifies
the allocation leader's process identity. Clients must connect in-process;
subprocess helpers do not preserve this identity.

Frames are bounded at 3 MiB, idle reads at 5 seconds and end-to-end local requests
at 15 seconds. Stream artifacts with `artifact_begin` (`name`, `size`),
`artifact_chunk` (`upload_id`, byte `offset`, `data_hex`, at most 256 KiB decoded),
and `artifact_finish` (`upload_id`). Repeated offsets must contain identical bytes.
The finished artifact handle contains its identity, size and SHA-256.

`publication_commit` accepts a stable `publication_id`, `kind` (`checkpoint` or
`result`), JSON `metadata`, and `artifact_ids`. Native code serializes the complete
descriptor before checking the 1 MiB byte limit. It reserves global spool quota,
inserts immutable files, synchronizes the namespace, and transactionally selects
the descriptor head. Previous heads survive rejected or uncertain replacements.
Workers never write authoritative spool records directly.

A local committed handle is not yet a coordinator result receipt. The agent
uploads immutable artifacts and submits fenced checkpoint/result records; the
coordinator supplies the accepted receipt. GC retains attempt pins across this
handoff and only reclaims released, acknowledged, unpinned content.

Coordinator requests may reference at most 64 distinct artifacts. Verification
retains open files until the receipt transaction finishes; the limit leaves
descriptors available for control connections. Split larger input submissions
into smaller jobs. Hashing runs outside the coordinator state lock, and the
coordinator rechecks attempt authority before accepting the verified files.

## Ambiguous acknowledgement and draining

Keep the original request ID and publication ID after an ambiguous reply. Query
`publication_status` with the original publication ID to reconcile exact candidate
bytes and durability. Never invent a replacement final result because its ACK was
lost. `publication_abort` cannot retract an already committed head. SDK
`PublicationUncertain` errors preserve the attempt identity and candidate digest.

Check the drain file at a safe boundary, publish a checkpoint, and exit promptly.
Checkpoint metadata describes application progress, and referenced artifacts hold
the necessary application state. A later generation receives the accepted
checkpoint as resume context. Managed children use `spawn`, `status`, and `stop`
on the same authenticated socket and an explicit per-parent child limit. An
ambiguous spawn retains its original request ID for inspection or retry.

## Resource targets

The supervisor also sets `CEDEGRID_RESOURCE_TARGET_FILE` for the leader and for
every managed child. Each process's file sits next to its drain file, in the same
private directory. The file asks a worker to use less of a resource instead of
being drained:

```json
{"version": 1, "sequence": 7, "deadline_unix_ms": 1793000000000, "step": "hint",
 "targets": {"ram_mib": 6144, "gpu_memory_mib": {"GPU-0": 10240}, "cpu_millicores": 2000},
 "allowance": {"ram_mib": 20480, "gpu_memory_mib": {"GPU-0": 18432}, "cpu_millicores": 7000},
 "allowance_sequence": 4}
```

- The supervisor replaces the file atomically. Each change has a higher
  `sequence`; a relaxed target is a new file with `step: "none"` and fewer or no
  targets. An absent file means no target.
- Readers ignore unknown keys and treat `version` above 1, or an invalid file, as
  absent. `sequence` 0 means no target was published yet.
- `allowance`, when present, is what the allocation may grow to now without causing a
  step: its usage plus an equal part of the room the mode leaves, at most a current cap
  or hint target, and only its usage while a measurement is unknown. It changes under its
  own `allowance_sequence`; a fall publishes at once, a rise at most one step per
  scale-up interval.
- A worker that reads targets writes the highest sequence it has seen, `0` before
  any target, to `resource-target.ack` in the same directory. The supervisor reports
  an allocation as hint-ready once the leader or a managed child has acked, and as
  observing the drain file once it has acked or used the supervisor socket. Only
  hint-ready allocations receive hints; the others go straight to drain.
- The ack shows only that the worker reads targets. Compliance is judged from
  observed usage. `deadline_unix_ms` is the deadline the policy enforces. If usage
  is still above the target then, the next step follows: drain for RAM and GPU
  memory; CPU stays capped at its target and is never drained.
- The context may carry `resource_target_file` and lists `resource_targets` in
  `capabilities`. Agent contexts carry only the capability, because the private
  directory is created at launch, so read the environment variable.

The SDKs implement this channel. `WorkerContext.resource_targets()` returns the
current target, and `WorkerContext.on_resource_target(callback)` runs the callback at
once with the current target, when there is one, then once per new sequence during
`draining()` and `safe_point()` polls; a failing callback is logged and never stops
the poll. A managed child finds the current target in its own directory when it
starts. `WorkerContext.resource_allowance()` and
`on_resource_allowance(callback)` do the same for the allowance, once per new
`allowance_sequence`; an allowance-only change never calls `on_resource_target`.
`on_save_progress(callback)` calls `callback(sequence)` once per new
`save_progress_sequence`: free memory fell below a floor through others' growth while
own work was within its fair share, so save progress now. It is not a drain and asks
for no memory back, and a hook registered after a request was handled is not called
for it. `ResourceSignals.from_env()` needs only the two environment variables, so
managed children can use it. The TypeScript names are `resourceTargets()`,
`onResourceTarget()`, `resourceAllowance()`, `onResourceAllowance()`,
`onSaveProgress()` and `ResourceSignals.fromEnv()`.

Call `safe_point()` or `draining()` at natural safe points, such as once per batch or
per step. A call re-reads the target file only when its stat identity changed or the file
was modified within 50 ms of now, since two writes within one modification-time tick can
keep the size and a reused inode. A failed read is not remembered, so the next call reads
again. While nothing changes a call costs a stat of the target file and of the drain file;
calling in a tight loop works but is not needed.

`on_drain(callback)` hooks (`onDrain()` in TypeScript) run once, at the first call that
sees the drain file. A hook that raises is logged like a resource hook; the remaining
hooks still run and `safe_point()` still raises `DrainRequested`.

Drain is unchanged: the drain file's existence means drain, whatever a target says.
After a drain request, publish a checkpoint at a safe boundary and exit; work that
does not exit is terminated after the drain timeout. Workers that do not read
targets keep exactly this drain behavior. When free memory falls below the critical
share of a floor, hint-ready workers get a 1 s hint deadline, and a worker that
observes neither file is sent TERM without the drain-file wait.

## Numeric metadata

Integer tokens range from -2^63 through 2^64-1. Decimal tokens map to finite
IEEE-754 binary64; overflow and nonzero underflow to zero are rejected. Negative
zero and integer-versus-float values are preserved. Arbitrary-precision decimals
are outside this profile. Persisted generation/epoch counters fit signed 64 bits.
TypeScript protocol integers use `bigint`; integer inputs may also be safe integer
`number` values. Use the SDK lossless JSON codec when serializing returned values.

See [the C example](../examples/counter.c), [Python SDK](../python/README.md),
[TypeScript SDK](../npm/README.md), and [shared contract fixtures](../tests/contracts/cedegrid-0.2.json).
