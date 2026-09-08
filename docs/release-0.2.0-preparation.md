# CedeGrid 0.2.0 preparation status

**Status: incomplete preparation, not a stable release or a 0.2.0 release candidate.**

The selected 0.2.0 requirements are authoritative. The earlier audit is evidence to verify, not permission to retain legacy public names, legacy runtime configuration, or narrower release gates. This document records the limited implementation completed in this branch and the work still required. It must not be used as release approval.

## Scope and preservation

The preparation branch started from `c2e60e335581276e3c00b53dd31d3808718f50b7`. Changes were made on `release/0.2.0-preparation-20260908`, not on `main`. No release tag, GitHub Release, registry publication, or live-host deployment was performed.

The maintainer's working checkout was not available. Uncommitted local changes were neither inspected nor overwritten. They must be reconciled before integrating this branch; do not reset, clean, or replace that checkout with this snapshot. No private test-host credentials or identifiers belong in public summaries.

Local execution initially permitted the baseline Python tests, then became unavailable with execution-service errors. The subsequent committed repairs were tested through GitHub-hosted CI. This is not evidence that private hosts, real GPUs, or every declared architecture were tested.

## Implemented subset

The Python client now resolves relative TLS paths against the canonical configuration file's directory, without expanding environment variables or tildes. It rejects unknown/duplicate legacy JSON configuration fields, malformed HTTPS origins, invalid timeout/rate inputs, and malformed RPC response structures. This is still the legacy JSON reader: **TOML-only cross-language configuration is not implemented by this repair**.

Task construction validates argv, cwd, environment, IDs, integer resource bounds, boolean acknowledgements, managed-child limits, and named artifact fields. Managed-child spawning validates inputs before reading supervisor credentials, retains supplied or generated request IDs after ambiguous acknowledgements, and keeps requests in the caller process. Its wire-state validation follows `child_response()` rather than the internal journal enum. This is not a claim that every nested payload of every generic RPC has received complete cross-language validation.

Downloads use the existing durable directory-creation helper, validate artifact metadata before creating directories, reject malformed/oversized chunks, and clean temporary files after interrupted transfer. Regression tests cover preserved previous destinations and directory-sync failure before transfer. The worker descriptor-size/quota/publication repair is still pending.

The SDK workflow builds wheels and sdists, installs into fresh virtual environments, tests outside the checkout with Python isolated mode, independently rebuilds/reinstalls the sdist, runs `pip check`, and records source commit and artifact hashes. It also verifies the source-export closure and regenerates an export manifest. The exact source allowlist includes these new regression files and this status document; bootstrap toolchain archives are not part of that export.

Public product names and versions remain the existing `resmgr`/`resource_manager`/`resmgr-sdk` and `0.1.0` in this scoped branch. Its test wheels are **not** the requested `cedegrid` 0.2.0 artifacts. No compatibility aliases or replacement registry names have been introduced as a release solution.

## Reproducible evidence and failures

At source `f1130f93a0ce3234d75c442e1418507bfed6810c`, the installed SDK matrix passed on Ubuntu 22.04 and macOS, each with Python 3.10 and 3.13. There are 32 SDK tests, run once against the installed wheel and again against an independently rebuilt sdist wheel in each matrix entry. These are repeated executions of the same tests, not 256 distinct scenarios.

- [Installed SDK checks at f1130f93](https://github.com/sidcatkr/CedeGrid/actions/runs/34185485412).
- [Full project checks at f1130f93](https://github.com/sidcatkr/CedeGrid/actions/runs/34185485408): consult the job-level results; the SDK matrix does not substitute for these checks.
- [Fresh baseline checks](https://github.com/sidcatkr/CedeGrid/actions/runs/34183505009): Linux and Rust 1.88 checks passed; macOS and Windows tests failed.
- [Intermediate failure retained](https://github.com/sidcatkr/CedeGrid/actions/runs/34184908551): a macOS no-expansion fixture expected a noncanonical temporary path. The fixture was corrected without weakening canonical-path semantics.
- [Intermediate integration regression retained](https://github.com/sidcatkr/CedeGrid/actions/runs/34184908561): overly restrictive SDK child-state validation rejected the real `preparing` wire state. The repair and a regression test are in f1130f93.

The fresh baseline macOS failure occurred in `replayable_agent_quarantines_corrupt_cache_recovers_reservations_and_committed_checkpoint`, with a release timeout and SQLite error 5898. The logical-rollback recovery test passed in that same run. Neither that individual pass nor the six local passes reported during planning proves that the historical writer/recovery race is fixed. No namespace-lock repair is claimed here.

CI artifacts include build logs, installed-wheel and installed-sdist logs, and source/artifact hashes, including logs from failed runs where the workflow reached artifact retention. The SDK artifacts currently have a 14-day retention period; durable release evidence retention is still a release-pipeline task. The existing full-project workflow does not yet archive every private service log and cleanup record on failure.

## Remaining implementation gates

| Gate | Required completion | Current status |
| --- | --- | --- |
| Clean identity and upgrades | Rename executable, Rust library, Python distribution/import, supervisor launches, all environment prefixes, examples, fixtures and manifests together. Use `cedegrid.toml` and `.cedegrid-state`; preserve explicit paths and durable identities; require drained/stopped upgrades; no aliases or mixed-version rolling upgrades. | Not implemented in this branch. |
| TOML runtime and converter | TOML 1.0 for all four runtime roles; config_version 1; strict identical Rust/Python/TypeScript client schema; conditional tomli for Python 3.10; canonical path targets; HTTPS origins; shared timeout/rate settings; three-way CPU weight; config-example and equivalence-checked, no-overwrite legacy converter. | Not implemented. Existing TLS-path repair is only one prerequisite. |
| Descriptor publication | Enforce 1 MiB serialized UTF-8 descriptors including metadata; account for existing files and temporary replacement space; preserve earlier results/checkpoints on every failure. | Not implemented. |
| Shared bounded loader | One open, no symlink/nonregular files, limit+1 maximum read, same bytes for hash and parse; shared by publication, checkpoint polling and reclamation. | Not implemented. |
| Recovery ordering | Stable-parent namespace lock covers SQLite and output writers; fence, quiesce, exclusively quarantine/reinitialize; delayed supervisors validate namespace/session before opening; retain uncertain reservations on failure. | Not implemented; deterministic regressions required. |
| Platform and doctor | Windows execution/recovery rejected before mutation; strict storage support and profile admission reported separately; profile admission used for readiness without weakening coordinator storage. | Not implemented. |
| Scalable status | Indexed keyset status_page RPC for six separate collections, bounded and filter/epoch-bound cursors, job isolation, inventory counts, Python/TypeScript iterators and CLI NDJSON streaming; legacy oversize error before constructing snapshots. | Not implemented. Defaults 100, allowed 1..1000, serialized pages at most 7 MiB under the 8 MiB client limit. |
| TypeScript and npm | TypeScript-authored SDK/launcher/tests, strict compilation, lossless JSON/bigint interop, ESM/CJS/declarations, exact-version optional native dependencies, scripts-disabled installation, independent SDK imports, signal/stdio/exit preservation. | Not implemented; no npm artifact prepared. |
| Native and PyPI release artifacts | Five native OS/architecture targets, Linux glibc 2.35, standalone execution without Python/Node; renamed Python-only SDK; Rust 1.88, Python 3.10, Node 22.14+ minimums. | Only old-name Python test artifacts have been built; declared native release artifacts are not qualified. |
| Language-independent onboarding | Arbitrary commands/plain results/context/spool/managed-child RPC; generic Python, TypeScript and compiled C examples without private application dependencies. | Not implemented as the selected 0.2.0 onboarding. |
| OSS release pipeline | Updated Cargo/package closure, dependency notices, advisory/license checks, comprehensive release-content secret scan, exact validated-artifact promotion, checksums/provenance, protected OIDC workflows, ownership checks. Keep crates.io disabled. | Source allowlist and scoped evidence workflow only; release pipeline and registry prerequisites remain open. |

## External qualification gates

The authorized three-host topology must be tested using isolated directories, temporary credentials and test-owned workloads. No host in that topology was accessed during this preparation. Public documentation uses role descriptions rather than hostnames, addresses or credentials.

The required ext4 coordinator/CPU agent, second ext4 GPU agent and explicit mergerfs replayable agent must all perform useful work. Retain evidence for accepted results, checkpoint/resume, cancellation, coordinator and agent restart, interruption of test-owned connections, stale-attempt rejection and confirmed resource release. Client and worker independence must be tested from installed packages: Python without Node, TypeScript without Python, native supervisor self-launch outside the source checkout, and npm installation with scripts disabled.

RTX 5060 Ti and L4 qualification are both outstanding. Start with one selected GPU, at most 1 GiB per test-owned workload and scenarios bounded to 60 seconds. Qualify admission, useful progress, controlled competition, draining and observed VRAM release; never infer release from a process exit alone.

Three-agent load testing at the default 256 MiB artifact limit is outstanding. It must show no unintended lease expiry and control-RPC p99 below one-quarter of the configured lease. Move immutable-blob hashing outside the coordinator lock only if this gate demonstrates the need, and revalidate fencing before committing afterward.

## Publication prerequisites and readiness

Registry ownership and trusted-publisher configuration for the exact npm and PyPI name `cedegrid` have not been verified. Public registry lookup is not proof of ownership. No alternative package name is authorized as a silent substitute. Build and validation workflows must not publish merely because one SDK job passes.

For the eventual GitHub-hosted publishing jobs, consult [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) and [PyPI trusted publishing](https://packaging.python.org/en/latest/guides/publishing-package-distribution-releases-using-github-actions-ci-cd-workflows/). npm currently documents Node 22.14.0+ and npm CLI 11.5.1+ for trusted publishing. Pin and verify the publishing toolchain separately from the supported SDK runtime floor. Publish the exact validated artifacts, with protected environments and scoped OIDC permissions, only after separate publication authorization.

Stable 0.2.0 readiness requires every declared platform and both GPU gates to pass against the selected source and artifact hashes. None of the pending gates above is waived by this document. Unsupported process containment, hostile-code isolation, hard VRAM partitioning and unqualified optional controls remain outside stable support claims.
