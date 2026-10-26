# Contract: granted-core events for SDK workers

An SDK worker learns its granted cores through the existing resource-target file, without any
new channel:
- **New field**: `granted_cores`, the number of CPUs the run may use now, and `granted_cpus`,
  their IDs. Both are written by the supervisor when the grant changes.
- **Python**: `on_granted_cores(callback)` calls `callback(count, cpu_ids)` once at start and
  after each change.
- **TypeScript**: `onGrantedCores(callback)`, with the same behavior.
- The callback is advisory: a worker that ignores it keeps running. Example use:
  `torch.set_num_threads(count)`.

## Rules
- The field lands in Rust, Python and TypeScript together, with the contract fixture
  (Principle VII). Readers ignore it when absent, so older agents stay compatible.
- At the Full level, the count follows the cgroup's CPU limit rounded down to whole cores; at the
  Tracked level, it follows the affinity mask.
