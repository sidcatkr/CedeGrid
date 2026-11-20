# Changelog

## Unreleased

### Added

- **Contention modes.** CPU, RAM, GPU compute and GPU memory each run in Yield First, Neutral
  or Performance (`contention.default_mode`, `contention.<resource>.mode`). Own work yields in
  graduated steps (lower share, cap, pause, hint, drain) instead of being drained.
  `contention.max_mode` caps the modes jobs may request; unset, it is the node's most
  aggressive configured mode. Performance needs `contention.acknowledge_caution =
  ["performance_mode"]`.
- **Per-job modes.** A task may set `contention.modes` per resource (Python
  `command_task(..., contention_modes=...)`, TypeScript `commandTask(..., { contentionModes })`).
  It runs at the less aggressive of that mode and the node cap; a capped request is reported
  with `mode_capped`. A job's mode decides how that job gives back: a job in Performance is
  never held for another user, for unknown activity or at a memory floor breach, whatever the
  node's mode; only the operator envelope steps it. The node mode still sets the aggregate and
  admission, and the node's Yield First and Neutral jobs give what it needs as far as they can,
  so another user can get less than the node's mode promises (known limitation). Such tasks are
  placed only on agents that negotiated `contention/1`.
- **Measured GPU compute.** External GPU compute is measured from per-process samples when
  they account for the device within `contention.gpu_compute.attribution_tolerance_percent`
  (15) points; external users are grouped by UID. While an external process is listed,
  device time not attributed to own processes counts as external. Otherwise, and under MPS,
  the amount is unknown. A device returns to the measured path only after
  `contention.gpu_compute.quiet_window_intervals` consecutive measured intervals. Under an
  unknown amount, Neutral holds the fair share and Performance admits nothing new there.
- **Whole-task holds.** Yield First and Neutral hold own CPU and GPU compute work back by
  pausing whole tasks, in Tracked and in Full, with no rate cap and no stop-and-resume duty
  cycle; in Full a pause freezes the leaf, and `cpu.max` is a launch setting only. On CPU the
  paused tasks are those whose usage comes nearest the excess, so neither own work nor the other
  user loses more than half the smallest task; on GPU compute a task is paused only while the
  own work left running stays at or above the target. A pause still answering its demand after
  `pause_bound_s` (10 s) moves to other tasks with the held amount kept, so no task is drained
  for CPU; where every task is paused, as in Yield First, the pauses take turns. Guaranteed CPU
  work above its request only gets a lower share. On a time-sliced GPU, own tasks are paused
  down to the share of the busy
  processes the time-slice rule allows beside a principal that reads at least
  `contention.gpu_compute.slice_bound_percent` (75) percent of its slices.
  `contention.gpu_compute.slice_step_percent` still loads and has no effect. An own GPU task placed
  but not yet running on the device counts toward that rule, and one heartbeat places at most one
  new GPU task on each device. Admission reads another user's latest GPU use as well as its
  average, so a user that has just become busy holds it, and amounts that cannot be measured for
  the sample age count every listed user as busy.
- **First-come GPU floor.** Own GPU work that was on a device before every other user listed
  there keeps, in Yield First and Neutral, at least the share that leaves none of them more
  than half of the device. An unreadable user ID or an unknown process list withholds it.
- **Satisfied demand.** Under saturation, a small external demand is probed and a larger one
  is watched for one response window. A user whose usage stays within its demand is recorded
  as satisfied and no longer contends until its usage grows past it. At most 64 records are
  kept per resource; the least recently seen user is dropped first.
- **Allowance signal.** The resource-target file carries `allowance`, per CPU, RAM and GPU
  memory, with its own `allowance_sequence`: what the allocation may grow to now without
  causing a step. The SDKs add `resource_allowance()` / `on_resource_allowance()` (Python) and
  `resourceAllowance()` / `onResourceAllowance()` (TypeScript).
- **Resource targets.** `CEDEGRID_RESOURCE_TARGET_FILE` and the worker-context keys
  `resource_target_file` and `capabilities` deliver hint targets. Both SDKs keep accepting
  0.2 contexts, and 0.2 workers keep their drain behavior.
- **Protocol negotiation.** Coordinators advertise `["api/1", "contention/1"]` in heartbeat
  replies. Agents send the new report fields only after that, and coordinators send requests
  without `contention` to agents that did not negotiate it.

- **Performance first.** Performance, the default, starts new compute work as plain Linux would
  beside users who do not run CedeGrid, and beside CedeGrid users that arrived after own work.
  Beside a CedeGrid user whose work arrived first it holds new work at the fair share. Other
  users' CedeGrid work is recognized read-only from a marker on each task supervisor's command
  line, which also carries the run's arrival; a user CedeGrid cannot identify counts as plain.
  While a plain user competes for the CPU, Performance starts every task the node's task limit
  and memory allow, beyond the CPU count, as plain Linux does.
- **Admit immediately and unprotected.** Two per-run SDK options for Performance and Aggressive
  work (`admit_immediately`, `unprotected`, each with its confirmation field). Admit immediately
  also starts beside a CedeGrid user that arrived first and yields; it is ignored beside one that
  arrived first in Performance. Unprotected, at the highest warning level and behind the host's
  allowance (`cedegrid setup --allow-unprotected`) and, in System mode, `modes.unprotected`,
  also raises the account slice's CPU weight through the broker where the host splits CPU by
  account (`modes.unprotected_cpu_weight`, 400 by default). The raise never replaces a weight
  the broker did not set, and ends within 5 s of the key turning off or the member leaving the
  group, and when the broker stops or restarts.
- **Priority over plain users.** An install option that runs own CPU work above users who run
  no CedeGrid work, never above another CedeGrid user. It needs both the administrator's
  permission in System mode (`modes.plain_priority`, weight `modes.plain_priority_cpu_weight`,
  200 by default) and the user's choice in `cedegrid setup`, which asks on a terminal with Yes
  preselected (`contention.priority_over_plain`; `--priority-over-plain` and
  `--no-priority-over-plain` set it unattended, where it is otherwise off). The broker raises
  the account slice while own Performance or Aggressive CPU work runs and no other CedeGrid
  user on the CPU is below the weight, and ends it as it ends the unprotected raise. `doctor`
  and `observe` show whether it is chosen, permitted and in force. In System mode, an account
  whose unit runs work counts as CedeGrid even where its processes are hidden.
- **Waiting for work.** A node with room waits at the coordinator for the next queued task
  (`await_work/1`), and the answer wakes the agent's loop, so new work is placed within half a
  monitor interval of its submission instead of at the next 2 s heartbeat.

### Changed

- **Validation layout.** The application validation manifest generator is
  `tools/make_application_validation.py`, and validation bundles hold the application checkout
  under `source/application`. Repack an existing validation root, or rename its application
  directory to match.
- **Neutral is the default.** A configuration without mode settings runs every resource in
  Neutral, and `config_version = 1` files load unchanged.
- **Legacy GPU keys are interpreted.** `gpu.execution_mode` applies to GPU resources without an
  explicit mode: absent, `auto`, `contention_aware` and `best_effort_occupied` mean Neutral;
  `conservative_non_sharing` sets `contention.never_share_occupied_gpus = true`. Every value
  but `auto` warns at load time with the replacement keys, and `doctor` shows the
  interpretation. A non-empty `gpu.best_effort_external_processes` warns that the list is no
  longer used. `gpu.scale_up_cooldown_ms` applies to GPU memory only when
  `contention.gpu_memory.scale_up_cooldown_ms` is unset.
- **Floor and Neutral memory policies.** `contention.floor_policy` and
  `contention.neutral_memory_policy` accept only `a`. `b` and `c` fail to load with a message
  that names the replacement settings.
- **Envelope and spool.** Own work beyond the operator envelope is brought back per resource
  through the yield steps, opportunistic work first; Guaranteed memory is drained only as floor
  policy A allows, and a breach no longer stops every allocation. An exhausted spool budget
  blocks admission and drains nothing. Explicit operator drain still drains every allocation.
- **`cpu.nice` clamp.** A negative `cpu.nice` warns and is treated as 0.
- **Margins and floors.** CPU and GPU compute keep 15% free in Yield First, 5% in Neutral and
  2% in Performance; on memory, Yield First keeps 20% free beyond the floor. The GPU memory
  floor defaults to max(1 GiB, 5% of the device) instead of a fixed 3 GiB. RAM and GPU memory
  floors are at least 512 MiB, and a smaller setting is raised with a warning.

- **Operator certificates can no longer act for a node's assignment.** Assignment-scoped
  requests now require the certificate of the node that owns the assignment. Before, an
  operator certificate could also send them. The affected requests are `prepared`, `renew`,
  `complete`, `publish_checkpoint`, `fail`, `begin_upload`, `upload_chunk`, `commit_upload` and
  `abort_upload`. Operator requests such as `submit`, `status`, `cancel`, `retry`, `get_result`
  and artifact reads are unchanged.
  - **Migration:** a tool that sent those requests with an operator certificate must now use
    the owning node's certificate.

### Fixed

- Internal gates and supervisors no longer fail with "launcher parent identity mismatch" when
  CedeGrid is started through the npm launcher on Node versions without `process.execve`.
- A supervisor no longer aborts, and leaves its workload unsupervised, when its diagnostic
  stderr pipe has no reader.
- The coordinator retries transient connection-accept errors instead of stopping.
- Lease expiry is atomic. A failed requeue no longer leaves a task assigned to an expired
  lease.
- `tools/release_manifest.py` runs on Python 3.10.
