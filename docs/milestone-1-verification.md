# Milestone 1 verification

Validated locally on 2026-09-05. This record describes the initial milestone source;
later changes must run the appropriate checks again.

| Check | Result |
|---|---|
| Native host | macOS, aarch64, APFS |
| Rust toolchain | 1.98.1 |
| Linked bundled SQLite | 3.53.2; WAL-reset version gate passed |
| Formatting | Passed |
| Clippy, all targets, warnings denied | Passed |
| Native tests | 66 passed, 0 failed |
| Minimum Rust 1.88.0, all-target check | Passed |
| Linux x86_64 Rust type check | Passed |
| Windows x86_64 GNU Rust type check | Passed |
| Live observe-only smoke | Four samples; CPU known after warmup; no enforcement |
| Synthetic GPU-pressure replay | Expansion blocked, then allowed; blocked under external activity; allowed after recovery |

Native tests comprise 10 unit tests, 29 policy/configuration tests, 18 state tests,
6 CLI tests, and 3 telemetry integration tests. They include property-based resource
accounting, concurrent SQLite connections, lost-ACK retry, unclean writer exit,
directory-sync ordering/failure, stale GPU samples, missing sensors, and the absence
of an execution command.

Cross-target commands used `LIBSQLITE3_SYS_USE_PKG_CONFIG=1` with `cargo check` to
type-check the Rust platform branches without a cross C linker/toolchain. These
checks do **not** validate target SQLite compilation, binary linking, or native
Linux/Windows behavior. Native builds use bundled SQLite; no such override is
needed or recommended for deployment. The checked-in CI workflow has not run on a
remote service because the repository has not been published.

Actual NVIDIA hardware telemetry, Linux pidfd behavior, cgroup permissions,
shared-server observation, distributed execution, and the authorized 24-hour soak
remain unverified. No server connection, resource-consuming test workload, process
termination, root-level control, or public repository publication occurred.

Rust was installed in the local user toolchain directory without editing shell
startup configuration. The repository was initialized locally with no remote.
