# Contract: Everyday CLI commands

Every command accepts `--json`. It then prints one JSON object (or one per line for `logs -f`
and `top --json --follow`) and no other text on stdout. Errors go to stderr as
`{"error": {"code": "...", "message": "..."}}` with a nonzero exit code.

Every command, the hidden supervisor commands included, refuses at effective UID 0 (FR-082): one
line on stderr saying to run the same command as the user without `sudo`, or with `--json` the
error object with code `ERR_CEDEGRID_ROOT`, and exit 3.

## `cedegrid run [options] <script> [args...]` and `cedegrid run [options] -- <command...>`
- Options: one flag per task-schema field ([task-schema.md](task-schema.md)), plus:
  - `--from-json <file>`: the existing job JSON; flags override its fields;
  - `-p NAME`, `--no-setup`, `--save`, `--save-env`, `--explain`;
  - `--gpu N|auto`, `--threads auto`, `--tty`, `-d`, `--require-containment`.
- Prints, in order:
  - the one-line per-job notice when an entry applies;
  - the one-line Tracked notice when the run is Tracked;
  - the output tail.
- Exit codes: the job's exit code once it ends in the foreground; 0 after `-d`; 3 when refused
  (`--require-containment`, invalid flags); 4 when the agent cannot start.
- `--explain`: prints each effective value with its source and exits without starting, unless
  combined with a run (`--explain --` prints, then runs).
- `--json`: `{"run_id", "control_level", "control_reason", "effective": [...], "log"}`.

## `cedegrid ps`
- Lists own runs: id, state, control level, modes, CPU, RAM, GPU, step and reason, and flags
  (starved with duration, below fair share with reason, bottleneck with causes, possibly
  misdirected affinity changes with their thread IDs).
- `--json`: `{"runs": [Run]}` (see [data-model.md](../data-model.md)).

## `cedegrid logs <run> [-f]`
- Prints the log, then follows it with `-f`. The run is unaffected when the viewer exits.

## `cedegrid stop <run> [--force]`
- Drains, then ends the run after the drain timeout. `--force` skips the drain. It acts through
  verified processes only, and refuses another user's run (exit 3).

## `cedegrid top [--json [--follow]]`
- A live view of host resources, own runs and the reason for each step, refreshed every monitor
  interval. `--json` prints one snapshot; `--follow` prints one per interval.

## `cedegrid setup <script>`
- The per-job setup interface (FR-020, FR-026). Floors, the node cap and margins are shown
  read-only. With flags instead of a terminal, it writes the entry non-interactively, as 001's
  setup does.

## `cedegrid doctor`
- One line per check: `✓` or `✗` (`[ok]` or `[fail]` with `NO_COLOR`), the check, and a fix hint
  on failure. Checks include the control level and its reason, per-user evidence availability,
  whether contenders can share the run's CPU cgroup, and that it runs as a user other than root
  (FR-082).
