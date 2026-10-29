# Contract: agent ↔ supervisor control (internal, versioned)

The transport is newline-delimited JSON. It travels over the inherited stdin and stdout pipes,
and over a re-attachable Unix socket.

## Re-attachable socket

- **Location.** `<assignment output dir>/control.sock`. The directory is mode 0700 and the
  socket mode 0600.
- **Authentication.** The first line is `{"kind":"hello","token":"<hex>","version":1}`.
  - The token is 32 random bytes recorded in the supervisor spec (mode 0600).
  - The peer credential UID must equal the supervisor's UID.
- **Connections.** One active controller at a time. A newer authenticated connection replaces
  the older one.

## Agent → supervisor (`Control`, `kind`-tagged)

These variants exist today:

| Kind | Fields |
|---|---|
| `authorize` | `lease`, `expires_monotonic_ms` |
| `renewal` | `lease`, `expires_monotonic_ms` |
| `drain` | none. New optional field `cause` (YieldCause) |

These are new:

| Kind | Fields | Purpose |
|---|---|---|
| `adjust` | `sequence` (monotonic), `resources: {<ResourceKey>: {step, target?, cause, deadline_ms?}}` | Apply one step per resource; a hint's `deadline_ms` is the deadline the policy enforces |
| `query` | none | Ask for the current applied state |
| `allowance` | `allowance: {cpu_millicores?, ram_mib?, gpu_memory_mib?}` | Publish what the allocation may grow to now in its resource-target file, under the next `allowance_sequence` (FR-071) |
| `oom` | `preferred` (bool) | Set every verified family member's `oom_score_adj` to 1000, or back to the launch baseline when false, through each member's pidfd and `/proc/<pid>` directory (FR-023, item 029) |
| `save_progress` | none | Raise the resource-target file's `save_progress_sequence` once (FR-075, item 029) |

## Supervisor → agent (`Event`)

These events exist today: `prepared`, `finished`, `failed`.

These are new:

| Event | Fields |
|---|---|
| `adjusted` | `sequence`, `applied: {<ResourceKey>: step}`, `applied_targets: {<ResourceKey>: target}` (optional; the cap or hint targets applied), `unavailable: {<ResourceKey>: [step]}`, `hint_ready`, `observes_drain`, `detail` |
| `state` | `applied`, `targets`, `hint_ready`, `observes_drain`, `authority: "connected" \| "lost"` |
| `readiness` | `hint_ready`, `observes_drain`; sent when either changes |

- `hint_ready` is true once any target directory of the family holds an acknowledgement.
- `observes_drain` is true once `hint_ready` is true or the worker has used the supervisor
  socket, since both SDKs poll the drain file. It is false for plain commands.

## Rules

- **Ownership.** The supervisor applies steps only to its verified leader and managed children:
  owned pidfds, or a cgroup leaf whose membership equals the verified PID set (R4). Any other
  target refuses the step and reports it.
- **Guaranteed class.**
  - `adjust` with `pause` is refused for Guaranteed allocations.
  - A CPU `cap` below the allocation's request is raised to the request.
  - `drain` for Guaranteed work is accepted only with cause `floor_breach`, with cause
    `envelope_breach` on RAM or GPU memory (floor policy A), or as an explicit operator drain.
- **Terminate.** `adjust` with step `terminate` and cause `floor_breach` (the critical floor
  path, FR-063) sends TERM to the verified family without waiting on the drain file, then KILL
  after the grace period, for either class; for Guaranteed work it is the critical path's last
  step. A supervisor that does not know the step drains instead.
- **Unknown control lines.** An unknown `kind` is reported in `detail` and ignored. Only EOF or
  an I/O error counts as agent loss (fixes audit A22).
- **Agent loss.** On stdin EOF with no socket controller, the supervisor holds the applied
  steps. The existing lease-expiry rules then apply.
- **Errors.** No control or actuation error leads straight to SIGKILL (R3).
