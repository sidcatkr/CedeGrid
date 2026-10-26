# Contract: the task schema

`src/task_schema.rs` is the single source of task fields. For each field it gives:

| Attribute | Example |
|---|---|
| JSON name | `cpu_millicores` |
| CLI flag | `--cpu-millicores` |
| Python parameter | `cpu_millicores` |
| TypeScript option | `cpuMillicores` |
| Type and range | integer ≥ 1 |
| Default | 1000 |
| Help text | one sentence |

Fields today (from `command_task`): `task_id`, `argv`, `cwd`, `cpu_millicores`, `ram_mib`,
`gpu_vram_mib`, `env`, `replay_safe`, `allocation_class`, `required_controls`, `single_process`,
`no_escape`, `managed_child_limit`, `input_artifacts`, `max_attempts` and `contention_modes`.

New fields:
- `threads`: `auto` or absent (FR-061);
- `gpu`: a device index or `auto` (FR-027).

Both are added to the Python and TypeScript SDKs in the same change (Principle VII).

## Generated surfaces
- **CLI**: `run` flags are generated from the schema; `argv` and `cwd` come from the command
  line.
- **Fixture**: `tests/contracts/task-schema.json` is exported from the schema by a test that
  fails when the file is stale.
- **SDK checks**:
  - Python: a test compares `command_task`'s parameters with the fixture;
  - TypeScript: a test compares `CommandOptions` and `commandTask`'s output fields with it.

  Any field missing on either side fails.

## Rules
- A schema change lands in Rust, Python and TypeScript together, with the fixture.
- `env` is passed at launch and never stored. `--save` writes names only (FR-009).
