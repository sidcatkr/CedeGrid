# CedeGrid 0.2.0 implementation and release specification

Status: revised handoff specification. Product implementation and final-artifact qualification are not claimed complete.

This document replaces the earlier release plan. It incorporates the conditional review and retains the selected products: a clean CedeGrid rename, TOML-only runtime configuration with a legacy converter, an npm CLI plus TypeScript SDK, a Python SDK-only PyPI package, and stable qualification on all declared platforms and both NVIDIA GPU models. All project work and specifications are in English.

## 1. Baseline, scope, and evidence

The audited source baseline is `c2e60e335581276e3c00b53dd31d3808718f50b7`. Existing local changes are separate work and must be preserved. A private recovery copy contains the initial tracked/untracked changes, index and working-tree patches, HEAD, and file hashes. Never package that copy.

Planning observations are not final-release results: the supported Python SDK suite had 24 passes and five errors; four errors involved TLS fixtures and one involved validation ordering. The historical macOS rollback test passed six local repetitions against the baseline plus local changes. Those passes neither resolve the historical intermittent failure nor qualify 0.2.0. Fresh host preflight established two ext4 homes and one mergerfs home; it did not execute the final release tests.

Stable means the explicitly documented 0.2.0 capabilities passed the required gates against the exact release artifacts. The project additionally promises no intentional incompatible public API, configuration, or persisted-format changes within 0.2.x. A necessary breaking correction requires 0.3.0 and a migration notice. SemVer itself makes a weaker promise for 0.y.z; this is a project-level commitment. [SemVer](https://semver.org/)

Preserve the existing trust boundary: trusted workloads, explicit execution enablement, generation/session fencing, retained uncertain reservations, and verified process ownership. Arbitrary daemonization, hostile-code isolation, hard GPU memory partitioning, and unqualified optional controls are not stable capabilities. No registry upload is authorized by this specification.

## 2. Namespace ownership and recovery

### Chosen worker architecture

Native components own publication. Python, TypeScript, and C workers do not directly open the SQLite state, outboxes, authoritative descriptors, commit records, or authoritative output files. SDK publication methods are in-process clients of the native supervisor's authenticated local protocol. A C example implements that same protocol; no Python helper, CLI subprocess, Node native addon, or cross-language flock implementation is required.

Worker context and read-only input material live in a never-reused, session/attempt-specific external workspace. Native staging also uses session/attempt-specific names. Workspaces contain no operator credentials. They are not authoritative recovery state. A stale worker may retain its old workspace but cannot submit to a new session or cause a native publisher to open a new namespace. Workspace GC requires proven writer/process quiescence.

The native local endpoint exists even when `managed_child_limit = 0`. Publication and managed-child requests retain existing verified-leader peer-identity checks, a private token, and exact namespace/session/assignment/generation validation. Reject stale requests before opening any protected data. Managed-child requests remain in the calling workload process. Plain executables need no SDK and retain native exit-code result handling.

### Stable lock identity and participants

For a configured state directory, create a private control directory in its stable parent, outside anything quarantine renames. It contains permanently named owner, admission, and lifecycle lock files plus namespace identity/recovery metadata. Never unlink or replace lock files during ordinary operation, quarantine, or cleanup. Verify owner, regular-file status, single link, and file identity. Bind the namespace identity to one canonical configured location; reject aliases and live relocation.

All native state access participates:

| Participant | Protection |
|---|---|
| Coordinator, agent, local supervisor | Service ownership where applicable; shared lifecycle guard covering all data access |
| Assignment supervisors and publisher workers | Their own shared guard, acquired after exec and before reading protected specs/data |
| SQLite connections, outbox readers, output collectors, artifact/GC work | Borrow/retain the owning lifecycle guard through their last access |
| History, executions, reconciliation and diagnostic commands that open state | Shared guard; metadata-only doctor preflight does not open data |
| Offline backup, restore destination, upgrade and quarantine | Owner exclusion and exclusive admission/lifecycle access |
| SDK/direct protocol workers | No authoritative filesystem access; native endpoint performs guarded operations |

Acquire protection before opening SQLite, descriptor, outbox, output, checkpoint, or other namespace data files. Release only after connections, handles, asynchronous publication work, and capture threads have closed/joined. A process exiting is not a substitute for joining its surviving native work while it is alive.

Use close-on-exec on lock/DB/output descriptors. Native children reopen their own guards after exec; user workload stdio uses pipes and never inherits authoritative output file descriptors. Supervisor bootstrap stdio is pipes or external staging, because its own namespace guard does not exist yet.

Do not reuse a guard whose destructor explicitly unlocks a flock shared through a duplicated open-file description. Lifetime guards release through the final close of their owned reference. Do not carry SQLite connections across fork. Test duplicate descriptors and exec inheritance explicitly. [flock](https://man7.org/linux/man-pages/man2/flock.2.html), [SQLite locking and rename hazards](https://www.sqlite.org/howtocorrupt.html)

### Admission and recovery sequence

Lock order is owner exclusion, admission gate, lifecycle guard, then initialization/publication locks and SQLite transactions. Ordinary readers briefly hold shared admission while acquiring shared lifecycle, then release admission. Nested accesses reuse the existing guard; they must not reacquire admission while retaining a lifecycle guard.

The following coordinator-fenced sequence applies to replayable-agent/local-cache recovery. Offline coordinator backup/restore/upgrade uses the same ownership/admission/lifecycle exclusion and the separate offline protocol in section 5; it cannot require acknowledgement from the coordinator being restored. Operations involving multiple roots acquire their namespace lock bundles in canonical-path order.

Replay recovery performs this sequence:

1. Acquire exclusive owner and admission locks. Admission remains exclusive until recovery either safely activates or exits; new readers/supervisors cannot enter while existing users drain.
2. Record a bounded recovery intent in the stable control area before requesting durable coordinator fencing for the exact new replay session. The record is progress evidence, never authority. No authoritative acknowledgement means no quarantine or initialization. Retain the request/session identity for status/retry within this invocation after an ambiguous acknowledgement.
3. Acquire exclusive lifecycle access without upgrading a held shared lock. Wait at most 30 seconds; timeout reports recovery busy and changes no namespace or reservations.
4. Advance the recorded intent to the exclusive phase. Quarantine only after exclusivity, then initialize/import the authenticated inventory under the same exclusive guard. Preserve uncertain allocations and all original quarantine files.
5. Persist namespace/session identity and recovery progress. A crash at any stage leaves the node non-ready. A new recovery process opens a fresh authenticated coordinator session, treats cached snapshots as stale, and preserves any partial namespace before rebuilding. Never infer release or rerun an old assignment merely because a new directory exists.
6. While still excluding admission, release exclusive lifecycle, acquire a new shared lifecycle guard, and revalidate identity/session. Only then open ordinary shared store handles and release admission. No atomic promotion/demotion assumption is permitted.
7. Activate placement only after authenticated recovery readiness is acknowledged. Lost connectivity after fencing, after quarantine, or before readiness retains reservations and prevents execution.

Every delayed supervisor validates the expected namespace and session after obtaining its own guard and before reading its spec or opening output/DB files. It rejects stale identity even when the configured pathname now exists again.

Deterministic tests pause an old supervisor with both DB connections and output handles held, verify recovery cannot rename, close/join those participants, then allow recovery. Also test two recovery contenders, new readers during the wait, inherited handles, delayed supervisors, coordinator loss after fencing, and recovery-process death at every persisted phase. Assert namespace identity, reservation charge, and absence of unauthorized execution, not just eventual service startup.

## 3. Publication, durability, quotas, and bounded I/O

### Local protocol and immutable records

Worker context/native publication protocol version is 2; public mTLS RPC remains version 1. The bounded local protocol exposes artifact begin/chunk/finish and publication commit/status/abort operations. Every operation carries a caller-generated request/publication ID and exact attempt identity. Reusing an ID with different content is a conflict.

Use newline-delimited JSON, a 3 MiB complete-frame limit, at most 256 KiB decoded artifact data per chunk, monotonic offsets, and a 5-second incomplete-frame idle deadline. Each local request has a 15-second end-to-end deadline; an artifact consists of multiple bounded requests. Socket servicing and hashing must not block supervision/lease deadlines. SDK artifact methods open and stream the caller's source in-process; the native endpoint writes staging and computes/checks digests. Workers supply artifact IDs, not paths to authoritative files.

Every request includes version 2, op, request_id, token, namespace_id, session_id, assignment_id and generation. Artifact begin declares exact size; chunk carries upload_id, offset and data_hex; finish returns a sealed artifact_id/sha256/size. Retrying an acknowledged offset with identical bytes succeeds; conflicting bytes fail. Publication commit supplies publication_id, kind, metadata and sealed artifact IDs. The native publisher allocates the checkpoint sequence. Status returns pending, committed, rejected or uncertain plus the operation identity, candidate/head information and any known remote receipt; abort succeeds only for a provably uncommitted operation. WorkerContext.publicationStatus exposes this query in both SDKs. SDKs never fall back to direct spool writes if the endpoint is unavailable.

Native publication stores immutable generation files. A transactional SQLite head/publication record is the commit marker; mutable `checkpoint.json` replacement is not the authority in format 2. File and parent-directory synchronization precede the transaction selecting the new head. Keep the current and previous confirmed checkpoint generations; keep an uncertain candidate and its predecessor until reconciliation. A final result is immutable.

Checkpoint sequences are allocated transactionally and never reused. A commit older than an already committed head is rejected as stale; it cannot move the head backward. Prepared/gap sequences do not count as committed checkpoints.

`complete()` and `checkpoint()` return a local publication handle, not a promise of coordinator acceptance. Do not wait for final-result remote acceptance inside a worker that must exit before release. Operator clients query coordinator results/receipts using the original identity and hashes.

### Failure contract

| Failure stage | Required outcome |
|---|---|
| Validation, final-byte limit, quota reservation, or temporary write before commit attempt | Existing committed head and descriptor bytes/hash remain unchanged; release only reservations proven unused |
| Namespace insertion/sync or SQLite commit durability becomes ambiguous | Return `ERR_CEDEGRID_PUBLICATION_UNCERTAIN` with publication ID, attempt identity and candidate digest; preserve the predecessor and candidate; do not report unchanged state |
| Local commit succeeds but its reply is lost | Query/retry the same publication ID; return the recorded outcome without creating another generation |
| Coordinator accepts but acknowledgement is lost | Query/retry the same result/checkpoint identity and receipt hash; no new result or generation is invented |

Add `PublicationUncertain` in both SDKs. Status/reconciliation resolves a specific operation to committed, rejected, or still uncertain; only a confirmed durable commit permits local success. Preserve all data needed to recover the previous valid head after any ambiguous transition. A directory sync error cannot be treated as proof that a preceding rename did not happen. [fsync](https://man7.org/linux/man-pages/man2/fsync.2.html)

For the replayable profile, distinguish completed local synchronization from durable authority: publication handles state `assurance = replayable_local`. They never claim strong local durability. Only the strict coordinator's accepted receipt is authoritative after local-state loss.

### Quota and recovery accounting

The 1 MiB limit is the final descriptor's UTF-8 byte count, including identity, sequence, metadata, artifact references, and all persisted descriptor fields. Native code checks the final serialization; SDK prechecks are convenience checks. JavaScript string length is not a byte limit.

Native publishers share a node-wide quota ledger. Atomically reserve capacity in a transaction before writing, counting retained descriptors/artifacts, staging, output captures, receipt/control files, unfinished reservations and crash leftovers. Logical persisted file bytes and conservatively reserved future bytes are charged; this is not a guarantee about filesystem block allocation. DB/WAL and separately created offline backup archives have separate storage budgets.

Do not subtract the predecessor until commit certainty and retention policy permit reclamation. Concurrent writers cannot independently spend the same remaining capacity. Per-attempt limits also apply. Never hold a quota transaction while waiting for a worker or copying/hashing a large artifact.

On restart, exclude writers/GC, inspect the quota ledger and actual files, reconcile incomplete writes conservatively, and retain uncertain reservations. Remove an orphan only after its writer and publication references are proven absent; synchronize deletion before releasing its charge. Count retained quarantine spool content until explicitly reclaimed. Local quota reclamation never proves execution-resource release.

### File-open and reclamation rules

For legacy imports and every native descriptor read, traverse from a verified directory handle using no-follow directory opens for each component. Open the final entry with no-follow, close-on-exec and nonblocking flags, then verify with fstat that it is a regular file. This prevents a FIFO from blocking before type inspection. On platforms lacking the required safe backend, reject the operation.

Read at most limit+1 bytes through the same handle; hash and parse exactly those bytes. Only a genuine absent result permits plain-command fallback. Permission, invalid type, malformed data and size errors remain errors.

GC pins immutable objects while upload, validation, publication or recovery uses them. Before deleting, revalidate the exact object identity/generation and its unreferenced state under the publication/GC lock. A descriptor read earlier cannot authorize deleting a newer replacement. Test intermediate symlinks, FIFOs, growth, replacement, unlinks, concurrent GC and crash cleanup.

Client downloads have a separate durability contract. On supported POSIX filesystems, synchronize each newly created ancestor and its parent entry in creation order, the completed temporary file, and the destination parent after atomic publication. Failure before replacement preserves the old destination. Failure after replacement but before directory durability is confirmed raises DownloadUncertain / ERR_CEDEGRID_DOWNLOAD_UNCERTAIN with destination, operation ID and artifact digest; do not promise that the old pathname is unchanged. The download API defaults to durability=strict. Windows clients can explicitly select durability=portable for file flush plus atomic publication without a parent-directory durability promise; strict is rejected before filesystem mutation when unavailable.

## 4. Configuration and migration

Separate runtime-config loaders from JSON-data loaders. The current `read_deployment()` actually uses `serde_yaml::from_slice()` and is reused for deployment, RPC, job and pool inputs. Replace its call sites according to content category, never replace the shared parser wholesale with TOML.

Runtime node/coordinator/agent/client files are TOML 1.0 with required `config_version = 1`. JSON remains required for jobs, pools, RPC, replay frames, context and results. External build/workflow formats remain unchanged.

Commands:
- `cedegrid config-example --kind node|coordinator|agent|client`
- `cedegrid config migrate --kind ... --input ... --output ...`
- Client conversion additionally requires `--legacy-client-semantics rust|python`; Python semantics requires an absolute `--legacy-cwd`.

Reference legacy semantics are the audited Rust loader/defaults for node/coordinator/agent and Rust-selected client conversion. Python-selected client conversion uses the old Python TLS-CWD rule and supplied legacy CWD. The converter never guesses the originating client or searches for whichever certificate happens to exist.

Compare fully normalized effective settings after old defaults/old path resolution against new defaults/output-file path resolution. Materialize changed defaults, particularly legacy `.resource-manager-state` even when omitted in the input. Emit absolute resolved targets so a different output directory cannot redirect state or certificates. Do not require a future state directory to exist. Canonicalize the input config itself, preserving the specified symlink-target directory semantics.

The CPU-weight contract is omission -> existing default 10, `{ mode = "off" }` -> None, and `{ mode = "set", value = 10 }` -> Some(10). Off with a value, set without a value, and unknown variant fields are errors.

All clients use HTTPS origins with optional root slash and valid ports/IPv6. Reject userinfo, query, fragment and non-root paths. Resolve TLS paths relative to the canonical config location. Do not perform shell/environment/tilde interpolation. Remote workload cwd is validated for the target platform's syntax and contract; its existence is checked on the executing node, not the submitting machine.

An explicit port is in 1..65535. Even an empty query, fragment or userinfo component is rejected; absence and an empty component are not equivalent.

Use signed-64-bit-preserving TOML parsing before domain validation. TypeScript must not first convert integer tokens to Number. Reject unsupported/missing versions, unknown/duplicate keys, Boolean numeric substitutes, out-of-range values, dates in scalar fields, NaN and infinity. Python 3.10 uses conditional tomli; newer supported versions may use tomllib. [TOML 1.0](https://toml.io/en/v1.0.0)

Shared client defaults are 15-second per-request timeout and 10 MiB/s aggregate serialized RPC request/response pacing. Timeout is a monotonic end-to-end deadline including client queue/pacing, DNS, connection/TLS, request and complete response decoding, not merely a socket inactivity timeout. Rate excludes TLS framing/retransmission; preserve the existing 10% framing headroom and apply it consistently. Retried attempts are separate requests with explicit identities and separately charged bytes.

Conversion publishes through an atomic no-clobber primitive after validation and file sync, then synchronizes the output parent. Two converters targeting the same output cannot overwrite one another. Failure before publication creates no target; uncertainty after namespace publication is reported explicitly. Input files are never edited.

Shared fixtures cover schema, CPU weight, exact integers, URLs, Unicode/spaces/symlink paths, absent state paths, moved outputs, legacy default state, explicit legacy-client bases, and concurrent output creation.

## 5. Persistent state and clean rename

Public names become `cedegrid`, Python import `cedegrid`, Rust library `cedegrid`, and `CEDEGRID_` environment variables. Update supervisor self-launching and all active documentation/tooling together. No public compatibility aliases are shipped.

Do not substitute old strings inside saved requests, hashes, node IDs, receipts, replay records, backup manifests or historical evidence. New default state is `.cedegrid-state`; converted configs explicitly preserve the old effective location.

Version decisions:
- Configuration: 1; public mTLS RPC: 1, with additive operations/errors.
- SQLite `user_version`: 5 for every 0.2 storage profile; persisted profile/assurance remain explicit and immutable.
- Coordinator distributed schema: 3; remove code paths that unconditionally reset its marker to 2.
- Local state-layout/worker-context/publication format: 2.
- New coordinator snapshots and full offline upgrade bundles: format 2. Existing coordinator snapshot format 1 remains an offline import format.

Provide `cedegrid state upgrade --config ... --backup ... --confirm-legacy-stopped`. Startup against legacy state returns upgrade-required rather than silently migrating it.

Upgrade is offline. The new lock cannot certify that old processes participate: require legacy coordinator/agent service locks, explicit operator confirmation, verified supervisor/workload identities, and finalized output capture. Any live or unresolved writer identity blocks filesystem replacement/migration. No automatic PID-based killing is permitted.

Before mutation, inspect versions/profile/role non-mutatingly. If effective schema inspection requires recovery of WAL/hot-journal contents, inspect a private complete copy while the original is exclusively quiescent. Reject unsupported future versions before original DB/schema/profile/permissions/namespace changes.

Create and hash a private full upgrade bundle including DB sidecars, replay/session records, outboxes, receipts and local execution data. The existing coordinator-only backup cannot substitute for this bundle; it excludes node outboxes. Retain external PKI separately. Never ship upgrade bundles in release artifacts.

Audited WAL schema 2 and DELETE/EXTRA schema 3 migrate transactionally to 5 without changing profile, IDs, generations or stored hashed payload bytes. Replay schema 4 is preserved in the bundle/quarantine and rebuilt as 5 only through authenticated coordinator recovery; its local ledger is not promoted to authority.

Index valid legacy finalized descriptors and receipts without rewriting their bytes. Unresolved/corrupt/oversized outboxes are preserved and reported; they cannot be silently converted into a new publication. Match any existing authoritative receipt by its original identity. Block activation of affected work until reconciled.

Nonterminal imported tasks receive explicit upgrade holds respected by placement and automatic retry. Retain every uncertain reservation. Explicit retry can clear a hold only after existing release/side-effect checks and validation of the unchanged request against the new execution contract. If an old request no longer runs under the new public names, submit a separately identified corrected job; never mutate its historical identity.

Interrupted upgrades resume the recorded operation or remain blocked, never initialize an empty replacement as success. Test every migration commit/manifest boundary.

In-place downgrade is unsupported. Schema 5 must make all 0.1 writable opens fail; do not promise that every old diagnostic command is harmless. Rollback uses the untouched pre-upgrade bundle in a fresh location with the matching old binary, after all 0.2 processes are stopped and post-upgrade work/side effects are reconciled. Restoring old bytes is not rollback of external side effects.

## 6. TypeScript/Python numeric and transport contracts

Author npm SDK, launcher and tests in TypeScript. Public types include Client, commandTask, WorkerContext, ManagedChild and stable error codes.

| Numeric category | Contract |
|---|---|
| Protocol u64 fields | Exact JSON integer tokens; TypeScript output bigint; input bigint or safe integer Number, checked against the field range |
| Persisted generation/epoch/sequence counters | Additionally limited to SQLite i64::MAX; reject overflow/exhaustion before mutation |
| Declared floating fields | Finite IEEE-754 binary64 in the declared range |
| Metadata integer tokens | Exact integers from -2^63 through 2^64-1; TS bigint, Python int |
| Metadata decimal/exponent tokens | Finite binary64 values; TS Number, Python float; preserve negative zero |

Decimal/exponent tokens use IEEE-754 binary64 rounding, so ordinary rounding such as 0.1 is permitted. Reject nonfinite/overflowing conversion and any nonzero decimal that underflows to zero. Arbitrary-precision decimal objects/guarantees are unsupported; integer tokens outside the declared range are rejected. Treat the special JSON token -0 as floating negative zero, matching Rust, and re-emit it as -0.0. Serialize metadata Number values as float tokens (decimal point or exponent), including 1.0 and -0.0; serialize bigint as integer tokens. A Number that already lost an intended integer is not recoverable; protocol integer inputs outside the safe Number range must fail.

Use lossless-json with numeric-token hooks on supported Node 22.14+. Expose parseJson/stringifyJson for callers. Validate Rust metadata numeric lexemes using RawValue before serde_json can coerce overflowing integer tokens into f64. Preserve the existing serde_json feature profile and authoritative receipt-hash algorithm; do not globally enable arbitrary precision and inadvertently change historical hashes. Apply the same profile in Python. [lossless-json](https://github.com/josdejong/lossless-json)

Cross-language tests are TS -> Rust acceptance/storage -> Python and Python -> Rust acceptance/storage -> TS, including 2^53+1, u64::MAX metadata, adjacent rejected values, nested metadata, Unicode, subnormal/finite extreme floats, 1 versus 1.0, and negative zero. Test counter codec ranges separately from their narrower SQLite persistence range.

Preserve hostname/CA verification, mTLS role authorization, no redirects, and no environment proxies. Negative tests cover wrong CA, wrong hostname, missing certificate, unauthorized leaf, wrong node identity/role, redirects and proxy variables. Error responses remain bounded; uncertain spawn/publication errors preserve their original IDs.

Compile one CommonJS implementation and an ESM wrapper re-exporting its constructors/functions, with matching .d.cts/.d.mts entry declarations. Mixed import/require must share constructors. All errors expose stable `ERR_CEDEGRID_*` codes and cause; expose a structural error guard so callers do not depend only on instanceof.

## 7. Bounded and terminating status traversal

Add status_page on /v1/rpc. Request fields are collection, optional job_id/node_id/pool_id, limit and opaque cursor. Response includes items, next_cursor, coordinator_epoch and observed_at_unix_ms. Default limit 100, range 1..1000. The full serialized JSON response, including envelope/cursor, must be <=7 MiB; clients retain the 8 MiB transport bound.

Collections are tasks, jobs, pools, nodes, allocations, pool_nodes, reported_allocations and unrecognized_allocations. Jobs omit embedded task IDs; pools omit membership arrays; nodes omit embedded inventories. Provide counts and bounded summary fields; obtain detailed results through result/artifact operations.

A single legacy/summary item that cannot fit returns a small `ERR_CEDEGRID_ITEM_TOO_LARGE` identifying collection/key. Never emit an unchanged cursor or silently truncate data. New report detail strings are limited to 1024 UTF-8 bytes and control names/counts are validated.

Use durable immutable sequence keys and indexed keyset queries. Preserve existing task sequences; add monotonic sequence indexes for the other collections/memberships during schema upgrade, including delete/reinsert cases. Capture a collection high-water sequence on the first page. Each cursor binds version, exact filters, epoch, high-water and last emitted sequence; its decoded payload is <=4096 bytes. Read only `last < sequence <= high_water`.

Traversal is finite even while inserts continue. Returned values and membership predicates are live at each read; disappearing rows may be absent, and changed membership may affect subsequent pages. It is not a transactional snapshot. Invalid/mismatched cursors fail; restart produces cursor-expired. SDKs never silently restart a partially consumed traversal.

Job scope:
- tasks/allocations: only that job.
- jobs: the requested job; pools: that job's pool.
- nodes: nodes currently eligible through the job's pool plus nodes retaining its historical reservations. Capacity/telemetry fields remain explicitly node-global; reservation/report counts are job-filtered.
- pool_nodes: requires pool_id; a supplied job must reference that pool.
- reported_allocations: requires node_id; optional job filter follows known assignment ownership.
- unrecognized_allocations: requires node_id and rejects job_id, because unknown ownership cannot be truthfully assigned to a job.

Report inventory is bounded separately from historical state: max 2048 allocation entries and 2 MiB serialized NodeReport, under the 3 MiB request envelope. No chunked reporting in 0.2. Overflow refuses the report and new placement, retains all reservations, and yields an actionable inventory-limit error. Never discard entries to fit. Existing leases follow ordinary expiry/drain safety rules; overflow is not proof of release. Historical DB reservations/unknown inventory remain retained and paginable.

Legacy status preserves its small response shape but uses incremental bounded queries/construction. It returns pagination-required before materializing an oversized snapshot. CLI status defaults to one jobs page (tasks with job_id); --all emits page NDJSON and stops at the traversal high-water.

Tests check EXPLAIN QUERY PLAN/index use, no full-state collection or unbounded json_group_array, page bytes, cursor progress, continuous insertion termination, job isolation and inventory overflow. On a pinned qualification runner, use 100k and 1m task datasets, fixed SQLite cache <=16 MiB, 30 first-page measurements per dataset: p95 <1 second and incremental peak RSS <=64 MiB over idle coordinator. Retain runner details and raw measurements; unsupported measurements cannot pass.

## 8. Packaging and publication preparation

Products: npm `cedegrid` (SDK + CLI launcher), exact-version native packages `cedegrid-linux-x64-gnu`, `cedegrid-linux-arm64-gnu`, `cedegrid-darwin-x64`, `cedegrid-darwin-arm64`, and `cedegrid-win32-x64`, PyPI `cedegrid` (pure Python wheel/sdist), and standalone native archives. Keep crates.io publishing disabled.

Baseline targets are Linux GNU x86_64/aarch64 on glibc 2.35/kernel 5.15, macOS 14 x86_64/arm64, and Windows 11 24H2 x86_64 client use. Runtime minima remain Rust 1.88 for source builds, Python 3.10, and Node 22.14. Test Python 3.10..3.14 and Node 22/24/26. Do not claim newer platforms from compilation alone.

Inspect final ELF symbol/library requirements and execute on the baseline OS/architecture; containers on a newer kernel do not prove kernel 5.15 support. Execute final macOS/Windows artifacts on their stated minimums. Windows client commands must avoid execution/storage initialization and run without NVIDIA installed; local execution/recovery/publication is explicitly unsupported.

SDK import must not resolve/load a native binary. Missing optional packages fail only CLI invocation with a missing-binary/unsupported-target error. All compiled JS, declarations, native bytes and executable permissions are present in tarballs; installation requires no download script, compiler or code generation. Launcher argument/stdio/exit behavior is exact. POSIX terminal Ctrl-C and directly delivered INT/TERM/HUP/QUIT are tested separately, including duplicate-signal prevention; Windows promises tested console Ctrl-C/cancellation and exit propagation, not POSIX signal equivalence.

Test unmodified unpublished tarballs through disposable loopback-only Verdaccio, no uplinks. Seed original native and locked runtime-dependency tarballs, use fresh cache/config, install original main tarball locally/globally with --ignore-scripts, and verify resolved hashes. Also test --omit=optional SDK import and CLI errors. Never rewrite optionalDependencies to file: paths or repack a test-only variant.

Build wheel/sdist once, install the original wheel outside the checkout, and independently rebuild/install from the original sdist. The rebuilt wheel validates source closure; the original validated wheel remains the publish artifact. Run actual mTLS/worker tests against installed SDKs without source-path overrides.

Update Cargo/export/package allowlists, required LICENSE/NOTICE/dependency notices, documentation and generic examples together. Scan the extracted contents of every archive and public evidence bundle, not just their summaries.

Signing policy for 0.2: SHA-256 manifests and CI provenance are mandatory; native archives do not claim Developer ID/notarization or Authenticode. Validate required macOS ad-hoc signatures and record exact signing status. If authenticated signing is later configured, it occurs before final packaging/hash/testing and creates a newly qualified candidate.

Track four separate states: registry name/ownership, publisher configuration, artifact publish readiness, and actual registry publication. PyPI pending publishers do not reserve names; npm whoami does not validate OIDC. This work leaves live publication NOT_RUN. Check every npm native name and PyPI project separately.

Publish job defaults: GitHub-hosted runner, Node 24.14.0 and npm 11.16.0, exact locked action revisions. Revalidate these tool versions at implementation/bootstrap rather than substitute an unpinned latest. OIDC's documented minimum is Node 22.14.0/npm 11.5.1. [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/), [PyPI trusted publishers](https://docs.pypi.org/trusted-publishers/)

Future publish ordering: verify/upload exact native npm tarballs, verify/upload exact PyPI files, then publish the main npm tarball with --tag latest. Jobs download validated artifacts and never rebuild/repack. Retries compare existing npm SHA-512 integrity and fetched SHA-256/PyPI per-file SHA-256 against the immutable manifest. Identical files count as already published; missing files can be retried; any mismatch stops the release and requires a new coordinated version. No blind skip-existing, same-version overwrite, unpublish, or separate token-dependent dist-tag promotion.

## 9. Physical, GPU, and load acceptance

Use private mapping to three roles: durable coordinator + CPU agent on the new ext4 host; second durable agent and RTX 5060 Ti on the other ext4 host; replayable agent and one L4 on the mergerfs host. All are Linux x86_64 and do not satisfy ARM64/macOS/Windows gates. Provision missing test runtimes only in user-owned isolated locations. Hardware eligibility is rechecked before execution.

Physical tests use installed final artifacts and generic C, Python and TypeScript workloads: useful work on all nodes, exact accepted results, checkpoints, cancellation, higher-generation resume, coordinator and owned-agent restart, bounded disruption of only test connections, stale-attempt refusal, and confirmed release. Ordinary CPU scenarios use one worker/CPU and 512 MiB per node; load/GPU scenarios use their explicit envelopes.

GPU gate, separately for RTX 5060 Ti and L4:
- Use a minimal native CUDA Driver API C workload with embedded integer PTX, not an implicit PyTorch dependency. Check actual GPU results against deterministic CPU results.
- Require admitted execution -> useful progress -> accepted checkpoint -> cooperative drain -> verified release -> higher-generation resume -> accepted correct final result.
- Add separately identified lease-loss and test-owned competition scenarios for the advertised modes. Unqualified modes stay outside stable support.
- Scenario deadline is 60 seconds starting before submission; cleanup has a separate 30-second deadline. Timing out fails even if cleanup later succeeds.
- An independent watchdog owns verified native process handles, survives harness disconnect, and cleans only registered test-owned processes. It never adopts a rediscovered numeric PID.
- Sample owned-process VRAM every 100 ms, including context/library/workload allocations, with sum <=1 GiB per selected GPU. Start with small buffers. Record the sampled nature of the cap; it is not a hardware-enforced instantaneous partition.
- Require owned process/attempt identity, result/checkpoint hashes and receipts, process reaping, observed owned-context absence, and Released reservations. Device-wide baseline memory alone is insufficient.
- Missing prerequisites/eligibility before execution are BLOCKED. Telemetry loss, bound violations or unverified cleanup after execution starts are FAIL. Do not manipulate unrelated processes.

Artifact/control gate:
- Three measured repetitions, fresh coordinator state and empty verification cache each time; no host cache-dropping.
- Three concurrent lanes, one per agent, each with a distinct 256 MiB artifact; all nine digests differ. Use 1 MiB chunks and fixed 10 MiB/s serialized-wire pacing per lane. Synchronize final commits, submit result references, then download/hash each whole artifact.
- Hold lease at 10,000 ms. During the loaded interval schedule heartbeat and renewal attempts every 500 ms per node and status at 6 Hz. Require >=300 scheduled samples per operation per repetition.
- Measure monotonic end-to-end latency from scheduled enqueue through queue/pacer/TLS/server/body/decode. Enforce a 10-second absolute deadline. Record raw attempts, retries, errors, timeouts and missed schedule slots.
- Every repetition independently requires heartbeat, renewal and status nearest-rank p99 <2,500 ms. Do not pool repetitions or mix idle samples into acceptance.
- Require zero control errors/timeouts/missed samples, unintended expiry or replacement attempts. Timeout latency is retained at actual elapsed time, at least 10 seconds, and independently fails. Planned fault-test expiry is evaluated separately.
- Each repetition is bounded to 240 seconds plus 30 seconds cleanup. Insufficient samples cannot pass.
- If hashing violates the gate, pin immutable blobs against GC, verify outside the coordinator mutex, then revalidate identity/fencing and pin validity in the commit transaction. Never release pins before validation/commit completes.

## 10. Gate states, delivery stages, and audit traceability

Each gate has PASS, FAIL, BLOCKED or NOT_RUN:
- PASS: executed on exact candidate bytes and all correctness/bound/cleanup assertions passed.
- FAIL: execution began and an assertion, bound, provenance or cleanup check failed.
- BLOCKED: a required prerequisite prevented execution.
- NOT_RUN: not attempted.

Doctor emits strict_durability_supported and selected_profile_admitted separately, along with role, selected profile and linked-SQLite qualification. Its readiness exit status is successful only if the selected profile, requested role and SQLite requirement are admitted. A replayable agent can therefore pass without claiming strict durability; a coordinator still cannot pass on replayable storage. Metadata-only doctor must not create or open state. Windows client use does not call a local execution-state doctor as an installation prerequisite.

Stable readiness is true only when every required qualification gate is PASS and its source/artifact hashes match the frozen candidate. Registry publication readiness is tracked separately; no actual upload is required to prepare artifacts. New code, signing, packaging or bytes invalidate affected results. Preserve failed runs alongside later successes.

Private evidence retains commands, environment, raw observations and service logs. Public evidence is generated from an explicit allowlist with neutral host roles and necessary version/capability data; never recursively copy private evidence or include original logs inside a sanitized ZIP. Scan all exported members for private identities, credentials, routes and unintended workload data.

| Stage | Completion requirement |
|---|---|
| 0. Preserve/baseline | Recovery copy, inventory of local work, baseline source/tests and limits recorded |
| 1. Contracts | This specification and shared conformance/gate fixtures fixed before runtime changes |
| 2. Reliability | Native publication/quota, SDK validation/durability, deterministic recovery regressions pass |
| 3. Rename/config/state | Clean public names, separated loaders, migration equivalence and offline upgrade/rollback tests |
| 4. TypeScript/artifacts | Installed original ESM/CJS/declarations/CLI/wheel/sdist/native artifacts pass |
| 5. Qualification | Exact final candidate passes all platforms, three hosts, both GPUs, load and cleanup gates |
| 6. Publish-ready handoff | Original files, hashes, provenance, support matrix and registry prerequisites recorded; no live upload |

| Audit | Required regression/evidence linkage |
|---|---|
| D01 | PUB-01..05: final-byte boundaries, quota concurrency, immutable predecessor, uncertain commit and restart |
| D02 | IO-01..04: nonblocking safe open, bounded growth read, all call sites and reclamation identity |
| D03 | CFG-03..05: canonical config-relative TLS, explicit legacy bases and relocated output |
| D04 | REC-01..06: held writers, competing recovery, delayed supervisor, inherited FDs and interrupted fencing/recovery |
| D05 | PLATFORM-WIN: refusal before mutation plus Windows client tests |
| D06 | SDK-01..03: early validation, field ranges and request ID preservation |
| D07 | IO-05: ordered ancestor/file/parent sync, prior destination and uncertain replacement |
| D08 | CFG-01..06 / TLS-01: shared schema, URL, timeout/pacer and transport negative cases |
| D09 | PAGE-01..07: finite bounded indexed traversal, single-item errors, scope, inventory and performance |
| D10 | DOCTOR-01: strict support versus selected-profile admission, retaining coordinator requirements |

Implementers must attach changed code references, regression names, actual commands and immutable evidence hashes to every D01–D10 row. A script that succeeds when a defect is reproduced is historical observation, not a passing fix regression. Do not mark implementation or any required gate complete from this specification or its declarative fixtures alone.

The companion [conformance catalog](../tests/contracts/cedegrid-0.2.json) fixes shared inputs/outcomes and scenario assertions. The [gate manifest](release-0.2-gates.json) lists the required gates and exact acceptance constants; its initial NOT_RUN entries deliberately carry no candidate hashes or success evidence.
