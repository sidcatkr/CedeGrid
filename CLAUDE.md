# CLAUDE.md

CedeGrid runs explicitly submitted compute jobs on spare CPU, RAM and NVIDIA GPU capacity and
yields that capacity when other workloads need it.

## Layout

- `src/`: one Rust crate, `cedegrid` (lib + bin, edition 2024, MSRV 1.88).
  - `main.rs`: the CLI. The hidden `__worker-gate` / `__assignment-supervisor` dispatch runs
    before clap, Tokio, any thread and the launcher terminal handoff. Keep it first.
  - `agent.rs`: the rootless node agent. Each loop samples, evaluates policy, drains, reports
    and launches.
  - `supervision.rs`, `child_supervision.rs`, `managed_children.rs`: the per-assignment
    supervisor, launch barrier, drain/terminate lifecycle and SDK-managed children.
  - `policy.rs`: pure, deterministic admission and yield decisions. `contention.rs`: modes,
    mode targets, the yield ladder, victim order and the reason-code catalog.
  - `actuation.rs`: supervisor-side steps on owned processes (duty-cycle caps, pauses) and the
    resource-target file.
  - `telemetry.rs` (sysinfo, optional NVML) and `kernel.rs` (pidfd, PSI, cgroup readback):
    read-only observation.
  - `execution_model.rs`: the `LaunchBackend` and `ExecutionJournal` traits, implemented by
    `rootless.rs` and `cgroup.rs` (delegated cgroup v2 subtree only).
  - `state.rs`, `execution_state.rs`, `namespace.rs`: the SQLite ledger, supervisor journal
    and lifetime locks.
  - `coordinator.rs`, `protocol.rs`, `pagination.rs`: the axum coordinator behind mandatory
    rustls mTLS, the v1 wire types and bounded cursors.
  - `artifacts.rs`, `publication.rs` (worker RPC socket), `publication_gc.rs`, `config.rs`
    (TOML, `config_version = 1`), `config_migration.rs`, `backup.rs`, `upgrade.rs`, `numeric.rs`.
  - `setup.rs`: `cedegrid setup`, its flags and keyboard interface. `explain.rs`: the
    `observe` and `doctor` renderings.
  - `storage_qualification.rs` (storage diagnostics), `launcher_terminal.rs` (npm launcher
    terminal handoff); `lib.rs` declares the modules.
- `tests/`: Rust integration tests (`*.rs`), tool tests (`test_*.py`) and the contract fixture
  `tests/contracts/cedegrid-0.2.json`.
- `python/cedegrid/`, `npm/src/`: the Python and TypeScript SDKs, which mirror each other.
- `tools/`: operator, validation and release scripts.
- `docs/`: user and operator docs. `docs/guarantees.md` defines the support boundary.
- `specs/`, `.specify/`: Spec Kit specs, templates and the constitution.

## Build and test

Test fixtures need state under `$HOME`. The macOS default `TMPDIR` does not qualify.

```sh
mkdir -p "$HOME/.cache/cedegrid-tests"
export TMPDIR="$HOME/.cache/cedegrid-tests"
export CEDEGRID_TEST_PYTHON="$(command -v python3)"   # Python 3.10+; 3.10 needs tomli
export CEDEGRID_SERVICE_TEST_MONITOR_MS=500           # CI cadence for distributed_service

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked -- --test-threads=1               # serial, as CI runs it
cargo test --locked --test policy                     # one integration test file
cargo +1.88.0 check --locked --all-targets            # MSRV

PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_*.py'
PYTHONPATH="$PWD/python" python3 -m unittest discover -s tests -p 'test_*.py'
(cd npm && npm ci --ignore-scripts && npm test)
```

- Run the Rust suite serially. In parallel, the multi-process service tests can time out.
- `#[ignore]` tests are either subprocess fixtures that other tests re-launch, or opt-in
  hardware gates (`CEDEGRID_TEST_CGROUP_ROOT`, `CEDEGRID_TEST_GPU_UUID`). Never run
  `--ignored` across the suite, and run hardware gates only when explicitly authorized.
- A failed integration test keeps its `.agent-test-*`, `.distributed-service-*` or
  `..*.cedegrid-control` directory as evidence. Don't bulk-delete them.
- Windows CI checks client paths only: `cargo check --all-targets` and
  `cargo test --lib --test policy --test telemetry --test cli`.

## Invariants

The [constitution](.specify/memory/constitution.md) is binding. `docs/guarantees.md` details
the support boundary. In short:

- CedeGrid acts only on processes it launched and verified. It never signals, renices,
  throttles, pauses, moves or adopts any other process, and never acts on a persisted PID.
- Own work gives capacity back first. Own priority is never raised above the default.
- Unknown or stale telemetry is never idle or free. Responses to it are graduated.
- User code runs only after the launch barrier persists Authorized.
- `policy.rs` stays pure; telemetry and kernel code are read-only; effects live in the agent
  and supervisor.
- Never weaken ownership checks, launch barriers, retained reservations or unknown handling to
  make a test pass. macOS results are not Linux runtime evidence.

## Conventions

- Errors: use `anyhow::Result` with `ensure!`, `bail!` and `.context()`. Fail closed. Don't
  discard errors with `let _ =` or `.ok()` where they decide safety.
- Config: every section uses `#[serde(default, deny_unknown_fields)]` and is validated once,
  in `Config::validate`. Relative paths resolve against the config file, with no env
  interpolation. Jobs, RPC and metadata stay JSON.
- Wire, worker-context and numeric changes land in Rust, Python and TypeScript together, with
  the contract fixture.
- Dependencies: add one only with a written justification. Code must build on Rust 1.88.
  `tools/` uses only the standard library plus the in-repo SDK, on Python 3.10.
- Comments: short and factual. Explain why, not what. Keep each decision reason and error
  message to one sentence.
- Export: modules are top-level `mod x;` in `src/`. A new file that ships goes into `ALLOWLIST`
  in `tools/pack_source_review.py` and into `include` in `Cargo.toml`. Exported text uses
  placeholder home paths and loopback or RFC 5737 addresses. Don't hand-edit
  `source-manifest.json`.

## Spec Kit workflow

Run feature work through `/speckit-specify`, `/speckit-clarify`, `/speckit-plan`,
`/speckit-tasks` and `/speckit-implement`, in that order (`/speckit-analyze` cross-checks).
Specs in `specs/NNN-name/` state behavior in operational terms; implementation candidates go in
the plan, whose Constitution Check names the principles it touches.
