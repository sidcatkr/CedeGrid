# CedeGrid 0.2.0 qualification evidence

This record accompanies the actual 0.2.0 binaries, npm archives and Python wheel
and source distribution. The [gate manifest](release-0.2-gates.json) is the
machine-readable authority for source hashes, artifact hashes and qualification.
Publication is disabled while any required stable gate is unqualified.

## Audit corrections and regression coverage

| Finding | Implemented correction | Regression coverage |
| --- | --- | --- |
| D01 | Native immutable publication, exact final-byte descriptor limit, transactional quota and original-ID uncertainty recovery | `publication::tests` covers descriptor boundaries, predecessor preservation, interrupted file/SQLite boundaries, stream retries and quarantine quota |
| D02 | Component-wise no-follow, nonblocking regular-file opens and bounded reads | `bounded_open_rejects_fifo_and_intermediate_symlink_without_blocking`; publication reads, artifact replay and immutable-input checks |
| D03 | Canonical config-relative TLS with explicit legacy client interpretation | `tests/configuration.rs` canonical/symlink/relocated-output and Rust/Python legacy-equivalence cases; installed SDK mTLS transport checks |
| D04 | Stable namespace admission/lifecycle locks, retained writer guards, recovery intent and fenced fresh sessions | `tests/namespace.rs`, `tests/replay_state.rs`, `tests/distributed_service.rs`; state hot-journal and real-process writer tests |
| D05 | Windows refuses execution/recovery before state access; portable clients remain available | `windows_execution_and_recovery_refuse_before_reading_inputs_or_mutating_state`; minimum Windows runtime gate remains independently required |
| D06 | Early SDK/protocol argument validation with exact request identities and integer domains | shared numeric/config fixtures; installed Python/TypeScript tests and native cross-SDK publication/restart tests |
| D07 | Strict client download file and ancestor/parent synchronization; explicit uncertain replacement errors | Python and TypeScript transport/download fault regressions and installed full-artifact readback |
| D08 | Shared TOML versioned configuration, origin and timeout/pacing rules | `tests/configuration.rs`, `tests/tls_transport.rs`, Python/TypeScript transport tests and original-package mTLS checks |
| D09 | Indexed bounded pages, finite high-water traversal, explicit inventory limits and scope | `tests/pagination.rs`, coordinator regressions and the 100k/1m native status benchmark |
| D10 | Doctor separates strict storage support, selected-profile admission and requested-role admission | `doctor_separates_selected_profile_from_requested_role_without_creating_state` |

## Evidence interpretation

Builds and test runs are attached to their immutable candidate hashes. An earlier
run can qualify an unchanged package archive, but it does not qualify a replaced
native binary. Failed intermediate candidates remain retained in the private
qualification workspace. Public evidence includes only reviewed summaries and
hashes; it excludes private endpoints, TLS credentials, user paths and raw workloads.

Physical three-host work, RTX 5060 Ti and L4 scenarios require authenticated access
to the approved hosts. The access preflight reached SSH authentication, but no
authenticated session remained available for these runs. Cross compilation does
not establish Linux kernel, macOS 14 or Windows 11 24H2 runtime qualification.

Actual npm/PyPI upload and trusted-publisher authentication are separate from local
package validation. Registry name availability is advisory and does not establish
ownership. See [release operations](release.md) for the exact-byte publish workflow.
