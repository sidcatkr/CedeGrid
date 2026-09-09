# Coordinator, protocol, and artifact implementation record

The dated evidence below describes earlier development snapshots. CedeGrid 0.2.0
qualification is tracked separately in the [current release record](release-0.2-gates.json);
older runs do not qualify the new source or package bytes.

## Replayable burst storage

`storage_profile: burst_replay_delete_extra` is an explicit weaker local-storage
contract for authenticated opportunistic agents. It is disabled by default and
uses DELETE journaling, EXTRA synchronization, foreign keys, private-path checks,
locking checks and persisted schema 4 / `replayable_local` assurance. Recognized
FUSE storage may be admitted under this profile; this does not qualify its backing
namespace durability. I/O and locking errors remain fatal. Use only replay-safe
opportunistic work that does not require `storage.durable_local`.

The coordinator and its accepted result/checkpoint authority require a strict
profile. A replayable agent opens a fresh authenticated coordinator session before
replacing any local state. The coordinator fences previous sessions and attempts;
the agent preserves the previous directory in a private sibling quarantine and
imports every authoritative outstanding reservation with its original generation,
resource charge and preparation identity. Its first heartbeat must account for all
outstanding reservations. Stale sessions cannot renew, upload or publish.

Local progress, uncommitted uploads/spools and resume files may be lost across a
host or filesystem failure. Only a matching committed coordinator receipt confirms
final publication. Quarantine is retained for inspection; there is no automatic
backup restore, live database recreation, journal-error bypass or blind state reset.
Detected namespace loss or a missing active attempt stops the agent. Full detection
of every possible in-place storage rollback is not claimed.

Recovery never adopts or signals a stored numeric PID. Release requires verified
original-process absence and GPU release observations. Missing preparation,
unknown survivors or lost mediated-child inventories retain the reservation;
same-boot leader absence cannot prove an unknown child family has exited. A new
node session alone does not free capacity. Rejoin can remain blocked until absence
is established. Strict nodes advertise `storage.durable_local`; replayable nodes
advertise `storage.replayable_local`.

## Explicit occupied GPU mode

`gpu.execution_mode: best_effort_occupied` allows only opportunistic GPU work
alongside independently tracked, explicitly authorized external competitors. Set
`gpu.best_effort_external_processes` to a map from actual GPU UUID to identity
records containing `pid`, `boot_id`, `start_ticks` and `uid`. The collector verifies
all compute and graphics contexts against those device-scoped identities before
and after collection. Same user, process name or numeric PID alone is insufficient;
competitors stay external and are never adopted or signaled.

Activity may remain active or unknown in this explicit mode. Basic inventory,
memory and device-access observations must still be valid. New or mismatched
contexts block admission and require yielding; headroom, reserves, external memory
growth, cooldown and verified cleanup remain enforced. There is no production
switch that fabricates idle telemetry. Tests may inject unavailable activity and
must identify that as injected evidence. This mode cannot promise interference-free
sharing or prevent every external allocation race. Actual occupied-hardware
validation remains pending; default `auto` is unchanged.

## Source034 readiness

The storage and GPU modes are reusable configuration-driven core behavior. Native
process adapters supply OS-specific identity evidence; node names and deployment
addresses are not embedded in scheduling or recovery. Private route setup and its
restoration belong to the deployment adapter. No route settings were changed during
source034 verification because fresh effective-policy access and SSH authentication
were unavailable. See [current validation](validation.md) for tested scopes and
remaining physical-host checks. Historical records below retain their original
source version and may describe constraints subsequently superseded here.

Implemented in the existing checkout on 2026-09-05. No remote deployment, publication,
SSH changes, external process signaling, or Linux enforcement claim was made by this
work stream. Files and test fixtures were created under the Mac home directory.

## Storage profiles and qualification

A node configuration and a coordinator deployment configuration each accept the
optional top-level `storage_profile` field. Omission retains `wal_full`:

```toml
config_version = 1
storage_profile = "wal_full"
```

`wal_full` requires effective `journal_mode=wal`, `synchronous=2` (FULL) and enabled
foreign keys. `delete_extra` requires `journal_mode=delete`, `synchronous=3` (EXTRA)
and enabled foreign keys. Both use the actual bundled SQLite library and retain
the minimum-version, local-filesystem, private-path, transaction, fencing and launch
barrier requirements. Connection settings are read back; there is no silent fallback.
Choose `delete_extra` only when that profile has passed the relevant deployment's
qualification and its filesystem satisfies the required synchronization contract.
It is not a mergerfs permission switch.

`cedegrid --config node.toml doctor --role agent` reports both strict filesystem
support and admission under the selected profile. Use `--role coordinator` when
checking a coordinator state location: replay storage cannot pass that role even
when it is admitted for an agent. Doctor reads metadata without initializing state.

Initialization is serialized. An existing database's persisted profile cannot be
changed by editing configuration or while another process is active. Version 0.2
requires state schema 5 for all profiles; old WAL schema 2, DELETE schema 3 and
replay schema 4 require the explicit [offline upgrade](migration-0.2.md).
Current read-only operations, offline backup
and restore preserve the stored profile. Backup automatically detects it; existing
`backup` and `restore` commands keep their normal stop, integrity, fencing and
new-destination requirements. No in-place profile conversion is implemented. Keep
existing state, take a verified offline backup, and use a separately qualified fresh
state directory if evaluating another profile; do not copy live DB/WAL files or
retire old state while allocations remain uncertain.

The linked-library diagnostic writes only to a new directory under the current
user's home. Its existing parent must resolve canonically; aliases and reused output
are refused. For example:

```sh
mkdir -p "$HOME/cedegrid-validation"
cedegrid storage-qualify --directory "$HOME/cedegrid-validation/delete-extra-001" --profile delete_extra
```

Run a separate fresh directory with `--profile wal_full` for the default profile.
The diagnostic records the strict production preflight result and exact refusal
before its isolated raw-SQLite checks. It tests effective settings, committed data
across reopen, independent-process writer exclusion, concurrent committed payloads,
interrupted dirty transactions, retained reservation/generation fields, and an
interrupted file-publication/receipt boundary followed by idempotent recovery.
It checks exact expected contents, not just `integrity_check`. Only exclusively
owned diagnostic child processes are interrupted and reaped. Data/logs/report are
bounded to 16 MiB with a 30-second work deadline; kernel-stalled I/O can exceed a
userspace deadline and is not reported as successful cleanup.

The JSON report's process-crash result is separate from namespace durability.
`namespace_durability_qualified=false` deliberately supplies no new directory-barrier
proof. Raw checks never enable production admission or replace application lifecycle
and restart tests. A successful directory `fsync` API return can hide an unsupported
backing operation on some FUSE deployments; changing to DELETE/EXTRA cannot repair
that. Host power loss and filesystem-daemon failure are outside this bounded test.
Known-local filesystems still depend on the OS/storage honoring their documented
synchronization semantics. See [guarantee boundaries](guarantees.md).

## GPU capability modes

Configure the existing node policy; execution still requires explicit opt-in:

```toml
[gpu]
execution_mode = "conservative_non_sharing"
process_sample_max_age_ms = 2000
```

`auto` is the default and chooses only observed capabilities. `contention_aware`
requires fresh process-activity evidence and refuses missing evidence instead of
silently selecting a weaker mode. `conservative_non_sharing` requires reliable
compute and graphics inventory, memory observations and device accessibility; it
admits only devices without external contexts. A new external context freezes
expansion and requests full opportunistic yield on that device. Missing or stale
basic observations block admission and also require conservative yielding. A later
complete empty inventory plus the configured stable-headroom cooldown permits
recovery. Device utilization alone identifies neither external activity nor idle
capacity. Process telemetry `NotFound`, unsupported, permission errors and stale
cursor responses remain distinct diagnostics.

Auto and conservative non-sharing modes reject guaranteed GPU assignments because
full reactive yield cannot preserve that allocation class's continuity contract.
The coordinator's `gpu_guaranteed_allowed` report field defaults false when absent
and prevents incompatible placement. The agent independently checks the selected
GPU and class before launch authorization. Explicit contention-aware mode does not
remove normal headroom, freshness, reserve, or uncertainty checks. Guaranteed CPU
work remains available independently of GPU eligibility.

Inspect `doctor`, `observe --no-state`, and node reports before execution. Each GPU
snapshot's `observation` includes the entry-point/status, cursor and latest driver
sample timestamp in microseconds, observation time, age, freshness decision and
capability. Successful NVML queries are device-access observations, not complete
hardware-health certification. Local `supervise` is CPU-only; submit GPU work
through an authenticated agent for continuous observation and release confirmation.
An empty-device compatibility pass is not occupied-device sharing validation and
does not prove that reactive observation prevents every concurrent allocation or
external GPU OOM. See the [validation matrix](validation.md) for actual runtime
coverage; configuration and compilation alone establish no Linux hardware result.

## Current readiness — source033

Source033 fixes the one-ULP JSON parsing drift proven by the source032 worker
spools and accepted receipts. It enables `serde_json` `float_roundtrip`; coordinator
receipt hashing and storage schemas are unchanged. SDK hash checks added to tests
and application verification establish evidence, not a new runtime validation API.
Both publishing agents and coordinator need the corrected reader. Mixed-version
operation is unverified, and already-rounded stored values cannot be reconstructed.

Legacy accepted result/checkpoint JSON and receipt bytes remain unchanged on
reopen. Canonical stored replay returns the original receipt; a different payload,
including the original higher-precision worker value against an old accepted
receipt, is rejected as a digest conflict. Same-sequence checkpoint conflicts are
also rejected. `GetResult` returns stored legacy data without repairing an embedded
SDK hash. Preserve raw spool and accepted evidence and reconcile using the stored
result and receipt. No fuzzy comparison, automatic rewrite or migration fallback
was introduced. Both legacy regressions failed with the old parser and passed
with the fix.
Legacy compatibility evidence (private evidence retained outside this source review),
original source032 worker proof (private evidence retained outside this source review).

The exact source033 archive SHA256 is
`d4614d21646d3a7a9812c1b6b8f0814a691cb6f436a57e3413f9a00169cd170d`.
Actual native regression passed 234 tests with zero failures and five ignored
fixture entrypoints across 14 suites, plus release compilation, in 92.9283 seconds.
All 116 manifest files matched before and after. Both local storage-profile
scenarios checked precision, real SDK hashes, checkpoint continuation and released
records on one kernel. Four directly owned stage children exited zero and were
pidfd-reaped without harness signals, with no sampled retained descendant or zombie.
Peak observed family RSS was 2,034,544,640 bytes within the 8 GiB bound. GPU tests
use injected observations and CPU child gates; no CUDA or physical two-host
application claim follows.
Native source and cleanup review (private evidence retained outside this source review).

The strict source033 CPU application repeat passed in 179.1798 seconds: four
720-step/719-inference games and two learner tasks, with all six SDK hashes, six
coordinator receipt hashes and four replay hashes independently matched. All six
allocations and node execution records were Released, database integrity/FK checks
passed and native checkpoint continuation progressed from step 2 to 4. The local
review checked recorded bindings without downloading the large checkpoint/model
bytes. Wrapper cycle/controller children exited zero and were reaped without
signals; their controller separately sent TERM to agent/coordinator and reaped both
(exit zero/-15), whose identities were absent. Peak observed family RSS was
1,957,601,280 bytes within 4 GiB. This closes the recorded precision defect on
source033 while preserving source032's successful execution and failed hash audit.

Actual burst hash/ELF/version readback passed for the separate source-matched
2.35-target binary, requiring at most glibc 2.34 on installed 2.35, without state or
workloads. A 60-second transfer-observer timeout preceded its exact late ACK; the
transfer was verified without repeating it and only loader validation resumed.
Local Rust 1.88, formatting and strict Mac/Windows GNU Clippy passed. The final
27-file core/SDK/Cargo audit matched frozen033 with zero configured application/host
patterns in core/SDK/examples. These checks do not establish Windows runtime.
Strict application review (private evidence retained outside this source review),
burst loader readback (private evidence retained outside this source review),
retained transfer timeout (private evidence retained outside this source review),
current source audit (private evidence retained outside this source review).
Portability checks (private evidence retained outside this source review).

Source032 reservation and mediated-child corrections remain integrated: all
unreleased shared CPU/RAM and requested-GPU charges are retained; unrelated GPU
uncertainty cannot block independent work. Children require fresh GPU authority
before reservation and immediately before EXEC, then command-only receipt and
actual lifecycle, owned-parent and deadline checks. The source032 and source033
native tests do not establish occupied GPU sharing or managed GPU protection.

The burst storage gate is a missing supported backing-directory barrier: the
retained mergerfs 2.33.3 `fsyncdir` returns `ENOSYS` and the examined FUSE path
can report success for that unsupported operation. Both raw linked-SQLite crash
profiles passed; changing the journal does not supply namespace durability. No
supported home-only barrier has been established.

Useful physical two-host CPU work, automatic pressure/release/progress, physical
burst restart/rejoin and qualified burst namespace durability remain open. Burst
storage and the authorized private route are prerequisites for this CPU work; GPU
occupancy does not gate independently safe CPU admission. Prepared
bootstrap and demand tooling do not satisfy these runtime gates. The latest bounded eligibility window still had no empty GPU candidate: two
anchor graphics contexts and burst compute counts 3/3/3/1. Burst used approved
null-buffer count-only reads, collecting no process records or memory. No activity
API, CUDA, state or managed work was started; all observers were reaped. The first
count-helper failure and its separately corrected status7 handling are retained,
as is the never-executed full inventory action rejected by automatic approval
review.
Latest eligibility and recovery evidence (private evidence retained outside this source review).

The three narrow storage, occupied-GPU and private-route decisions remain unanswered. Overall
operational completion is false; candidate029 is stale and public release is held.
See [the current validation record and remaining gates](validation.md).

## Historical readiness — source032

The integrated source032 snapshot is SHA256
`c86743a1913a414c43f26a33468580bfbb9b4bf4c0d40fac3b92dcfdcc7aee07`.
Its affected native Linux run passed 75 tests with three ignored owned-fixture
entrypoints across five suites, plus release compilation, in 77.8775 seconds.
Every manifest file matched before and after. Both selected storage profiles ran
through the connected service test with two agents on one kernel. This covers
the affected reservation and mediated-child paths; the GPU contracts use injected
observations and actual CPU gates, without CUDA or physical two-host workloads.
Native source and cleanup review (private evidence retained outside this source review).

Local admission still charges every unreleased reservation, including uncertain
work. It compares total CPU and RAM plus every GPU UUID requested by the new
allocation. An unrelated device's uncertainty therefore cannot block independent
CPU work or another GPU. Requested-device deficits, shared resource pressure and
same-task attempt fencing continue to refuse admission. The original defect was
reproduced before the correction and its 103 focused Mac tests passed before the
native run. No reservation was discarded to create apparent capacity.

For mediated children, the default control hook allows the existing CPU path and
fails closed for GPU work without live authority. The agent-backed hook samples
current managed executions and children and checks the selected GPU launch
contract before reservation and after durable authorization, immediately before
EXEC. It returns pending authenticated commands without taking a later policy
sample that could supersede the final GPU check. The supervisor applies those
commands, then rechecks the actual current lifecycle, owned parent and preparation
deadline. Lease renewals are anchored before the control call so slow observation
cannot extend a grant. Guaranteed parent continuation and existing child
kill/reap/release or retained-uncertainty behavior are unchanged. The final gap was
reproduced in a failing regression; 93 focused Mac tests passed, followed by
the affected native032 tests. Those facts do not establish managed GPU protection.
Reservation correction (private evidence retained outside this source review),
mediated-child correction (private evidence retained outside this source review).

The separate anchor application completed four real 720-step/719-inference CPU
games and two learner tasks in 206.0514 seconds. All six allocations and local
execution records were Released, and database integrity/foreign-key checks passed.
The independent reviewer matched all four replay byte hashes and all six
coordinator receipt hashes, but two embedded SDK result hashes failed after JSON
roundtrip. That numeric integrity finding remains pending; it is not erased by
the successful workload. The native harness checked checkpoint continuation from
step 2 to 4; the local review checked bindings without downloading checkpoints.
The wrapper's cycle/controller children exited zero without signals; the controller
separately terminated and reaped its owned agent/coordinator with TERM. No
mediated-child, GPU, physical two-host or coordinator-outage result follows.
Application execution and hash-audit finding (private evidence retained outside this source review).

Formatting, Rust 1.88 all-target compilation and strict Mac/Windows GNU Clippy
passed for the integrated source. The existing target compiler produced the
separately hashed candidate targeting glibc 2.35. Actual burst hash/ELF/version
readback passed, requiring at most glibc 2.34 against the installed 2.35 runtime,
without opening state or starting work. An initial 30-second response timeout was
resolved by read-only recovery of the completed report; creation and execution
were not repeated. The anchor application harness passed, but independent
verification found an embedded result-hash discrepancy after JSON roundtrip.
That integrity finding and its correction remain pending; coordinator receipt
hashes and replay bytes remain verified within the independent review scope. Physical source-only staging
and exact asset fingerprints prepare those paths but do not establish useful
two-host work, storage qualification or application recovery.

The optional automatic CPU demand driver has 21 passing checkout tests and an
independent evidence review. It ties draining to the target's own fresh burst
report, timestamps newly accepted anchor progress separately and verifies the
common completed-request span of two externally owned CPU pressure threads.
Launch, completion and cleanup must come from the existing outer process owner.
The driver sends no explicit drain during the automatic stage and cannot equate
an owned connection interruption with node-process restart or durable recovery.
Applied tooling checks (private evidence retained outside this source review).

Both storage profiles retain the strict filesystem gate. Burst namespace
durability, useful physical two-host CPU work, managed non-sharing GPU runtime
and occupied-device protection remain open. No requested weaker-storage,
occupied-GPU or private-route exception has been enabled. Overall operational
completion remains false, candidate029 remains stale and public release is held.
See [the current validation record and remaining gates](validation.md).

A later bounded read verified both exact raw worker descriptors and their original
SDK hashes. Each accepted payload differed in one numeric field by one ULP,
confirming the origin of the discrepancy. That later proof and the source033
correction above supersede the initial origin uncertainty while preserving the
original failed audit and accepted bytes.
Raw-spool origin review (private evidence retained outside this source review).

## Historical readiness — source031 and subsequent scoped changes

Both storage profiles are implemented through coordinator, agent, supervisor and
offline operations, with the strict filesystem gate retained. Immutable source031
(`3e8d8bca3793ee4983e3c4c69b092c22070fb500febdfdecb1e86c5a7d218847`) passed 222 native
tests with five ignored, release compilation and both raw linked-SQLite diagnostic
profiles in 79.8894s. A separate DELETE/EXTRA connected-service test overlay passed
on native Linux in 15.5603s, preserving six accepted task results and exact local
execution/receipt/release records after stopped-service writable recovery. It uses
two local agents on one kernel and does not establish physical two-host work.
Source031 evidence (private evidence retained outside this source review),
connected profile add-on (private evidence retained outside this source review).

Actual no-state observations on both physical roles returned reliable basic GPU
inventories but no fresh process-activity samples. No observed device met the empty
inventory requirement for non-sharing mode. The strict production state gate still
refuses unqualified burst storage, although both raw process-crash diagnostics
passed. Namespace durability, useful physical two-host CPU work, managed
non-sharing GPU runtime and occupied-device sharing remain distinct open gates.

Physical bootstrap configuration now takes explicit native paths, affinity,
endpoint and independent storage profiles; 19 bootstrap and 11 source-export tests
passed in the actual checkout. Formatting and strict Windows GNU Clippy passed
after two narrow cfg corrections. The frozen source031 native result excludes
later test/tool overlays, the journal-neutral diagnostic, reservation-isolation
correction and mediated-child GPU freshness/lease-expiry checks; those changes
require separate affected evidence. Public candidate029
is stale and unpublished. See [current validation and required decisions](validation.md).

## Historical readiness delta — source027

Source027 fixes a real automatic-yield planning defect: a guaranteed allocation
could be selected as a victim and consume the apparent required reduction even
though the agent would retain it. `Allocation.class` now carries the durable
execution class from both retained-state and live-collector producers. Automatic
victim selection includes only opportunistic work; guaranteed charges still
reduce admission headroom, and explicit node drain still includes both classes.
Legacy policy recordings with omitted class default to Opportunistic; durable
execution records retain their required class. No reserve, release or unknown-GPU
admission rule is weakened.

The source027 archive SHA256 is
`a5653b27d3aedce06d1be0a2814a7b33d9c76618b86f34ba1836f9318986746e`.
Eighty-six affected Mac tests plus strict Clippy passed, and 109 affected native
Linux tests plus the release build passed in 70.2582s. Connected028 then passed
selective CPU yielding, continued guaranteed progress, accepted completion and
unknown-GPU admission refusal in72.4283s. All recorded test identities were absent,
both allocations released and database integrity checks passed. The independent
supervisor's actual decision is retained; the first two private observer failures
remain failures. This establishes connected CPU behavior and GPU refusal, not
managed GPU protection or physical two-host application work.
Connected028 review (private evidence retained outside this source review).
Formatting, Rust1.88 compatibility and strict Windows GNU compilation also passed
on source027; no new Windows runtime result follows.
Portability027 (private evidence retained outside this source review).
Native027 report (private evidence retained outside this source review).

The original implementation record below preserves its historical test scope.
Later physical server-api021 verification passed in 14.4589s: the real burst
Python SDK reached the unchanged anchor coordinator, checked mTLS roles/certificates/
hostname validation, resumed/replayed artifacts and reconnected an owned transport.
Its one protocol-only offer was never prepared/executed and was cancelled and
Released. It accepted zero workload results and did not exercise a node agent or
useful two-host application work. All recorded test processes were absent/reaped,
SQLite integrity passed, and no execution or uncertainty rows remained.
Physical API review (private evidence retained outside this source review).

Physical-fault023 then exercised a committed artifact response discarded before
the SDK received it: retry returned an identical ACK, the offline database held
one 4,032-byte publication, three stale operations were rejected and two unexecuted
offers Released. Runner time was 7.1837s; server time 110.1785s. No Prepared launch
or workload result occurred. All recorded test PIDs were absent and ShieldsUp/
empty Serve were restored. This does not test packet-level loss or loss of an
accepted workload-completion ACK.
Durable fault audit (private evidence retained outside this source review).

Native024 manual operation passed in 237.1977s with four real CPU games, 720 steps/
719 inferences each, and verified checkpoint continuation from step 2 to 4. During
a five-second coordinator outage, a guaranteed test task progressed7-to 33 while
the agent remained live; verified pidfd termination and manual same-state restart
ended in accepted completion. All eight tasks completed and eight allocations
released, with zero unrecognized allocations, clean database integrity/foreign-key
checks and all saved direct identities absent. Its failed022 predecessor is retained:
two logical CPUs minus the physical reserve could not admit the requested task,
and an empty ledger was mislabeled uncertain by a private assertion. Restoring the
prior approved two-physical-core/SMT affinity and correcting that assertion left
the production core, one-worker/4GiB bounds and protection policy unchanged.
Native operational review (private evidence retained outside this source review).

GPU diagnostics025/026 subsequently observed fresh own-process activity on both
roles: burst025 completed in 22.7157s with 912 units/three positive samples and
anchor026 in 21.2600s with 953 units/one positive sample. Both successful children
exited normally without signals, were pidfd-reaped and released their own GPU
contexts. They remained outside the manager registry, so no authenticated GPU
assignment, useful two-node workload or managed GPU protection is established.
Anchor025's aborted missing-RSS exit observation remains a failure; only that
role was rerun after a private 100ms owned-exit check, which still aborts for a
live child with unknown usage. External contexts remain unknown and the manager
core/admission policy is unchanged.
Final scope and evidence (private evidence retained outside this source review).

The user has delegated sensible bounded server tests; missing budget/window
approval is no longer a gate. Actual run bounds and cleanup still must be recorded.
Read-only022 preflight reconfirmed unsupported burst home fuse.mergerfs and no
qualified home submount. The Mac tailnet map has neither server, blocking the
Docker alternative before application routing. External GPU activity remains
unknown and no authorized delegated cgroup subtree exists. Preserve these gates;
do not replace durable agent state with unsupported storage or infer GPU capacity.
The 24-hour soak remains waived. [Current validation status](validation.md).

CedeGrid and Apache-2.0 are user-approved. The latest instruction defers public
source publication until the required testing gate is satisfied; no GitHub
repository or public push exists yet. At023/026 the 24 production core/SDK files and241
dependency lock entries were unchanged, with only the root package name changed
in Cargo.lock plus package/license metadata. Source027 supersedes that runtime
equivalence, and the old026 candidate is stale. Existing interfaces remain `cedegrid`
(binary/import), `cedegrid` (Rust library) and `cedegrid` (Python
distribution). Offline SDK source/wheel packaging and 14 existing installed tests
passed; this is packaging evidence, not a new Linux or distributed runtime pass.
Final metadata and SDK review (private evidence retained outside this source review).
The 023 Cargo privacy claim was too broad: its documentation glob included the
private worklog, although the separate source exporter excluded it and nothing
was published. The 024 explicit document allowlist excludes it; one offline
113-file package-list check verified 61 source files, the Rust included fixture
and 24 root-document links. Original audit provenance is preserved. This file-list
check does not replace content curation or close the useful-two-host/GPU gates.
Packaging correction (private evidence retained outside this source review).

## Connected implementation

- `src/protocol.rs` defines version-one `POST /v1/rpc` requests/responses, HTTPS-only
  Rust client, and explicit client/server TLS configuration. TLS requires a valid
  client certificate; its SHA256 DER leaf fingerprint must also be listed as an
  operator or one particular node. A CA signature alone does not grant a role.
  Redirects, built-in public CA roots, plaintext URLs and unauthenticated listeners
  are disabled. Request bodies/chunks, handshake time and concurrent connections
  are bounded. Node roles cannot submit jobs, change pools, or act for other nodes.
- `src/coordinator.rs` runs one exclusively locked coordinator on the existing
  SQLite store, retaining its filesystem/version/WAL/FULL safeguards and original
  `tasks`/`assignments` ledger. Additive tables contain specifications, node reports,
  reservations, publications and payloads; existing state migrations/readers remain.
  A durable epoch fences old coordinator authority, including after restart.
- Job submission is exactly idempotent by immutable job specification. Placement
  orders eligible tasks by priority then FIFO. Pending, running and uncertain
  reservations each contribute one max(requested, observed) charge. Pool maxima
  include retained uncertainty. Required unavailable controls block placement;
  actual application and process identity are checked again before authorization.
- Higher-priority work can request draining of lower-priority replay-safe
  opportunistic allocations, preserving configured pool minimum floors and
  guaranteed allocations. It receives no capacity until release confirmation.
  Minimum workers are conditional targets when eligible work/capacity exist,
  not CPU guarantees or a promise to manufacture tasks when a finite job ends.
- An offered assignment cannot authorize execution. `Prepared` checks the local
  durable record, boot/process/assignment/generation identity, resource request,
  and required applied controls. Execution agent performs the actual local barrier.
  Duplicate preparation or renewal responses return the remaining persisted grant;
  lost acknowledgements do not extend it. Agents subtract request round-trip time.
  A durable clock watermark rejects backwards coordinator wall-clock movement.
- Expired opportunistic leases retain uncertain reservations. Only an authenticated
  owning node's verified release report releases capacity. Guaranteed allocations
  retain their coordinator-loss contract. Supervisor failure is not self-enforcing.
  Unknown reported allocations and stale observations block expansion.
- Pool resize, job cancellation, node drain/rejoin, result query, checkpoint retry,
  and local reconciliation are wired into the CLI. Unsafe task retry requires an
  explicit side-effect reconciliation acknowledgement and all allocations released.
- Exact final result payload and receipt commit together. Duplicate accepted final
  delivery returns the same receipt; differing or stale results are rejected.
  Checkpoints do not finalize the task. Their per-attempt `checkpoint_sequence`
  cannot rewind; a new attempt receives the last durable checkpoint metadata.
- `src/artifacts.rs` bounds declared sizes, chunks and reserved upload storage,
  rejects links/path traversal, supports interrupted upload resume and exact chunk
  retries, verifies SHA256, synchronizes the file, atomically links immutable
  content into its final path, and synchronizes its directory before the publication
  database commit and ACK. Interrupted published-but-uncommitted blobs are not
  accepted until a repeated commit establishes their publication record.
  `AbortUpload` removes only an authenticated attempt's staged metadata/link; it
  preserves already published immutable blobs. The quota conservatively reserves
  staging and publication charges even when hard links share physical content.

## Wire/configuration contracts

Coordinator deployment TOML:

```toml
config_version = 1
state_dir = "/home/USER/.local/state/cedegrid/coordinator"
listen = "127.0.0.1:7443"
lease_ms = 10000
telemetry_ttl_ms = 3000
max_artifact_bytes = 268435456
artifact_quota_bytes = 21474836480
[tls]
ca_cert = "pki/ca.pem"
certificate = "pki/server.pem"
private_key = "pki/server.key"
[clients.OPERATOR_CERTIFICATE_SHA256]
role = "operator"
[clients.NODE_CERTIFICATE_SHA256]
role = "node"
node_id = "anchor"
```

The fingerprint placeholders must be replaced with actual lowercase SHA256 values;
this illustrative TOML is not executable as-is. TLS paths and coordinator state
paths resolve relative to their deployment file. Use a certificate SAN matching the
chosen endpoint. Keep the private CA key off shared servers. `tools/make_test_pki.py`
creates short-lived validation credentials under home; these are TLS identities,
not SSH authentication changes. Production credential rotation is an operator task.

Operator clients use [client.toml](../examples/client.toml). A minimal bounded
CPU-only `agent.toml`, matching `AgentConfig` in `src/agent.rs`:

```toml
config_version = 1
coordinator_url = "https://127.0.0.1:7443"
max_workers = 1
max_spool_bytes = 134217728
max_transfer_bytes_per_second = 1048576
max_runtime_seconds = 300
[tls]
ca_cert = "pki/ca.pem"
certificate = "pki/node-0.pem"
private_key = "pki/node-0.key"
[capacity]
cpu_millicores = 1000
ram_mib = 128
```

`tools/make_test_pki.py --output /home/USER/validation/pki --node-id anchor --node-id burst`
maps `node-0` to `anchor` and `node-1` to `burst`; use the returned `node_identities`
and `clients` mappings when preparing each agent and the coordinator. Transfer only
the matching node key/certificate and public CA to a remote agent. These TLS paths
resolve relative to `agent.toml`. The loopback URL is for an agent on the coordinator
host; for a remote node, set its authenticated reachable endpoint with a matching
server certificate SAN.

Copy `examples/node.toml` to the same private deployment directory and set its
`node_id` to the certificate's mapped node ID, its own home-local `state_dir`, the
appropriate node mode/reserves, and `execution.enabled = true` after preflight. The
separate `--config` node policy is required. This example admits at most one worker
inside a one-logical-CPU/128 MiB aggregate policy ceiling and makes no GPU reservation;
it does not enforce hard kernel CPU/RAM limits. Adjust these explicitly chosen
budgets to the authorized workload. The five-minute service envelope initiates
shutdown, not a promise that uncertain allocations are released at that instant.
Omitted `cpu_affinity` leaves placement unchanged; nonzero transfer/spool limits
bound traffic and local staging. A runtime of zero would require an explicit stop.

Actual CLI entry points:

```sh
cedegrid coordinator --deployment coordinator.toml
cedegrid --config node.toml agent --deployment agent.toml
cedegrid pool --deployment operator.toml --spec pool.json
cedegrid submit --deployment operator.toml --job job.json
cedegrid status --deployment operator.toml --job-id experiment-job
cedegrid drain --deployment operator.toml --node-id opportunistic-node
cedegrid drain --deployment operator.toml --node-id opportunistic-node --resume
cedegrid cancel --deployment operator.toml --job-id experiment-job
cedegrid --config node.toml reconcile
cedegrid resume --deployment operator.toml --task-id released-task
cedegrid rpc --deployment operator.toml --request request.json
```

`resume` cannot override retained uncertainty. An unsafe task additionally requires
`--side-effects-reconciled`; this is a declaration by the authorized operator, not
an automatic inference that the workload had no external side effects. Replaying
an unchanged `submit` after a lost ACK is safe. Do not manually modify SQLite rows.

`BeginUpload` → bounded hexadecimal `UploadChunk` requests → `CommitUpload` →
`PublishCheckpoint` or `Complete` connect to the Python SDK and agent spool.
`GetResult` returns the accepted real descriptor and artifact references for the
Kaggriculture collector. Node artifact reads are restricted to assigned work or its
previous checkpoints; operators can retrieve all accepted experiment outputs.

## Evidence and remaining validation scope

The coordinator/artifact regression suite covers exclusivity, restart fencing,
priority/FIFO, reservations, checkpoint ordering, exact receipts, publication gates,
role isolation, required control refusal, stale/unknown observations, preemption,
clock rollback and retained uncertainty. Artifact tests cover restart/resume,
checksum failures, exact retries, incomplete uploads and symlink refusal.

`tests/tls_transport.rs` starts the actual Rust HTTPS server with ephemeral home-only
certificates, verifies positive RPC, and rejects missing/unlisted/wrong-role client
certificates, plaintext requests and wrong server names. It invokes the real Python
SDK against that live server for status/result retrieval. No mock HTTP server is
used. `CEDEGRID_TEST_PYTHON` can select a Python >=3.10 interpreter.

Run from the checkout with `TMPDIR` set to a private directory under home:

```sh
cargo test --test coordinator --test artifacts --test tls_transport
cargo clippy --lib --test coordinator --test artifacts --test tls_transport -- -D warnings
```

These historical Mac tests establish transport and durable scheduling behavior
locally. They do not establish native Linux pidfd/cgroup behavior, GPU release
latency or physical two-server useful work. Subsequent native results and remaining
gates are recorded in [current validation](validation.md). The user replaced the
original required 24-hour run with bounded stress; no endurance pass is claimed.
The burst mergerfs home must not be whitelisted on the basis of these tests.
No underlying `/mnt` path is used.

Final work-stream verification: 18 coordinator tests, three artifact tests and the live TLS/Python SDK integration test passed; strict all-target Clippy passed. Owned source hashes and the verification scope are recorded in `artifacts/validation/mac-regression-20260905/coordinator-final.json`. The execution agent continues its separate SDK checkpoint/resume regression; these counts are not an operational completion claim.

## Adversarial review delta

Fixed delayed node observation rollback; immutable per-attempt prepared process identity; irreversible local drains across coordinator restart; persistent uncertainty fences; and contradictory fresh running reports after release, which restore an uncertain reservation instead of authorizing capacity reuse. Release remains an authenticated owning-node assertion backed by local verification; mTLS is not remote-kernel attestation against a compromised node agent.

Repeated execution failures now use durable idempotent failure receipts and exponential backoff, configured by `retry_limit` (default 3 failed attempts), `retry_backoff_ms` (1000), and `retry_backoff_max_ms` (30000). Exhaustion enters `needs_reconciliation`. Planned yields/preemption/drains do not consume the failure budget; the node reports `failure_kind: yielded` for local cooperative yielding. Operator `resume` resets the failure budget only after all reservations are released and any required unsafe-side-effect acknowledgement. Attempt generations never reset. Rust RPC now ignores proxy environment configuration and bounds streaming response bodies to 8 MiB even without Content-Length.

The review suite passed 23 coordinator tests, three artifact tests and the live TLS/Python SDK test. Native validation, fault-injected durable publication on actual disks, min-pool availability under real contention, and operational latency/throughput targets remain separate. Conditional pool minimums protect existing eligible minimum workers from preemption; they are not hard CPU allocations or a guarantee when a node is unavailable.

## Bounded mediated children and immutable inputs

`LaunchRequest.managed_child_limit` defaults to zero, preserving old single-process
readers. Opted-in parents reserve at most eight supervisor-mediated children;
children themselves cannot request nested spawning or different task/generation,
class, retry, required-control, GPU-visibility, or input-artifact authority. The
`managed_children` table and event journal are additive to schema 2. Request IDs
are durable deduplication keys. Verified child identity and required-control evidence
precede durable authorization; only the supervisor owns actual handles/signaling.
Pending and uncertain children count against the limit, and all family processes
consume the one parent allocation. Parent release and child reservation are fenced
in the same SQLite write transaction. Persistence is not proof of live OS ownership.
`tests/managed_children.rs` verifies seven registry invariants, including concurrent
parent release versus child reservation; native mediated execution/fault coverage
belongs to the execution work stream.

`LaunchRequest.input_artifacts` defaults to `[]`; each entry is
`{"name":"model.bin","sha256":"...","size":123}`. Submission accepts at most 64
unique safe basenames and only durably published, checksum-verified blobs. Total
input size is bounded by coordinator storage configuration and the agent applies
its own download/spool budget. A node can read another task's model blob only while
an assignment currently reserved on that node names the input. The agent verifies
and stages inputs before launch and supplies local paths through the credential-free
worker context. No application model format enters the Rust core. Existing job ACK
replays compare parsed specifications, preserving default-field compatibility with
older stored JSON. The named-input regression checks unpublished/name refusal,
assignment-gated retrieval, immutable resubmission, and access revocation on release.

## Offline coordinator backup and restore

Use `backup` only on coordinator state after its service has stopped. The operation
acquires both coordinator and agent locks, checks local-filesystem support and the
linked SQLite version, checkpoints WAL, and copies the SQLite image with SQLite's
backup API. Integrity and foreign-key checks accompany checksum-verified immutable
blobs and resumable upload metadata/partial bytes. A SHA-256 manifest commits last,
after files and containing directories have been synchronized. Keys, certificates,
deployment files, arbitrary logs, and uncommitted metadata temporary files are not
copied. The manifest is an integrity record in trusted private storage, not a signed
archive or protection against an attacker able to replace the entire snapshot.

This is explicitly **coordinator-only** backup. Any local execution journal entries
or agent attempt/refusal directories cause refusal, including an unpublished
outbox. Do not delete an agent outbox to force backup through. Preserve the original
agent directory and use its existing restart/reconciliation procedure. Remote active
or uncertain allocations remain represented in a coordinator snapshot.

Paths must be absolute, under runtime home, free of symlink traversal, on a supported
local filesystem, and have an existing destination parent. Destination directories
are new mode-0700 directories, files mode 0600; existing or interrupted destinations
are never overwritten. Defaults are 20 GiB payload bytes, 100,000 files, and 300
seconds of checked execution time, with up to 16 MiB of bounded manifest metadata.
Individual OS I/O/sync calls can block beyond the checked time budget. Smaller
explicit ceilings can be supplied through `--max-bytes`, `--max-files`, and
`--timeout-seconds`. A copy interrupted after preparation leaves a durable marker;
coordinator/agent/supervisor startup refuses marked state. Complete snapshot archives
also cannot be used directly as live state.

```sh
# Drain test jobs if they should finish before the snapshot; stop the coordinator
# using its verified foreground/process handle. Keep unrelated services untouched.
cedegrid backup --state-dir "$HOME/validation/coordinator-state" --destination "$HOME/validation/snapshot-001"

# Restore only after retiring the original coordinator; no simultaneous old/new
# coordinator is supported. Use a new destination, then update the private deployment
# state_dir to this output while retaining separately stored TLS credentials.
cedegrid restore --snapshot "$HOME/validation/snapshot-001" --destination "$HOME/validation/restored-state" --confirm-source-stopped
cedegrid coordinator --deployment "$HOME/validation/restored-coordinator.toml"
cedegrid status --deployment "$HOME/validation/operator.toml"
```

Restore checks every manifest hash before creating the target and checks hashes
again during copying. It preserves task IDs, generations, receipts, pool/job state,
checkpoints, retries, and reservation amounts. It advances the stored epoch and fences
every nonreleased reservation as uncertain; no timeout is converted to free capacity.
Owning-node release/reconciliation must resolve retained capacity before replacements
can use it. `restore-receipt.json` records the restored recovery point and fencing.
Never remove a fence by directly editing SQLite.

**Recovery point:** restoring an older snapshot does not retain commits made after
that snapshot. It is different from restarting against the current durable database.
Post-snapshot task execution, accepted results, and external side effects require
separate reconciliation. The source-retired confirmation is an operator deployment
contract; a file lock in a new directory cannot prove that another host or old
coordinator copy will never start. Preserve the original directory until recovery is
reviewed. Backup does not copy identity credentials or change node runtime IDs.

Six `tests/backup.rs` tests cover actual CLI round trips, active-service lock refusal,
outbox refusal, retained uncertainty/artifacts/interrupted uploads, missing/corrupt
content, traversal/symlink refusal, no-overwrite behavior, private permissions and
incomplete-output startup fencing. The targeted Mac regression passed 24 coordinator,
7 managed-child registry, 7 execution-state, 6 backup, 3 artifact, and 1 live TLS/SDK
test. Linux disk crash durability and distributed recovery still require native
operational evidence; these counts are not an operational completion claim.

## Final recovery review and connected local service evidence

A fresh node report that omits a prepared/authorized allocation now creates a
durable uncertainty fence and blocks expansion using its stale observations.
Reappearing `running` state cannot erase that fence. An allocation unknown to the
coordinator also creates a durable node reconciliation entry; omission, reconnect,
and restart cannot forget it. Only a matching owning-node release assertion clears
that entry. Operator node status includes these unrecognized allocations. This
covers allocations known locally after restoring an older coordinator snapshot,
without inventing replacement task identity or treating their capacity as free.

Queued tasks with retained allocations are checked before preemption, preventing
a high-priority but uncertain task on one node from needlessly draining healthy work
on another. Initial authorization additionally requires fresh expansion evidence,
positive process start identity, matching class, and consistent applied/permitted/
available control evidence. Standalone execution shares the node-agent lock so a
new supervisor cannot race an offline snapshot. No arbitrary-PID ownership is
inferred during these operations.

`published_file_without_database_commit_recovers_after_real_restart_and_ack_replay`
constructs the exact boundary where the real artifact publisher has synchronized a
final file but the coordinator publication transaction has not committed. The
coordinator rejects premature completion, resumes publication after a real restart,
and returns identical publication/final ACKs across retransmission and restart.
The same test resumes an interrupted upload and replays its acknowledged first
chunk. This is process-restart/fault-boundary evidence, not a simulated power loss.

The bounded status-shape regression materializes 4,000 completed task records,
4,000 released allocations, and a full node allocation report. Its response is
**3,436,569 bytes**, under the preselected 7 MiB threshold and the matching 8 MiB
Rust/Python status limits. Artifact RPCs retain the smaller Python payload bound.
No status records are silently truncated. This establishes headroom for the approved
soak shape; it does not promise bounded responses for unlimited history or arbitrary
large diagnostic strings. Exceeding transport limits remains an explicit error.

`tests/distributed_service.rs` starts an actual coordinator and two actual independent
agent processes with separate mTLS node identities, configured as `anchor` and
`burst`, on the one local host. In 8.63 seconds, six bounded Python SDK reductions
produced distinct useful task results under `local-sdk-experiment`. The test drains
burst, observes all burst allocations released while anchor commits more results,
restarts the burst agent, resumes its checkpoint at generation 2, transfers a current
immutable input artifact, and restarts the coordinator while anchor work is active.
Previously accepted result contents remain identical. Both local journals show only
released allocations before the owned service processes are stopped. Services have
finite runtime bounds and the test harness retains verified direct child handles.
This is a connected local service test, explicitly not real anchor/burst integration,
Linux kernel-control evidence, or the 24-hour soak.

## Bounded attempt starts and separate yield backoff

The later real Kaggriculture trial exposed repeated cooperative yields, which do
not consume the execution-failure budget. This invalidated deriving a hard start
ceiling from `retry_limit` alone. `LaunchRequest.max_attempts` is now an optional
positive immutable cap on top-level task attempt reservations, counting yields and
preparation failures conservatively as well as executions. Coordinator scheduling
and the local/remote execution journal enforce it. Exhaustion retains uncertain
capacity and enters `needs_reconciliation` after release/failure; operator `Retry`
cannot increase or bypass the declared cap. A new explicitly authorized task is
needed for additional attempts. Existing serialized requests default to no explicit
cap, preserving compatibility. This does not bound the number of SDK-mediated
child processes inside one opted-in allocation; those retain their separate family
contract. The bounded Kaggriculture game tasks use the single-process contract.

Cooperative yielding now has durable exponential delay, configured separately with
`yield_retry_backoff_ms` (default 1000) and `yield_retry_backoff_max_ms` (30000).
Duplicate failure delivery does not count another yield or move its stored delay.
The existing execution-failure counter remains separate. Existing retry tables gain
a `yield_count` column with default zero without changing task generations, failure
counts, receipts, or already stored deadlines. Explicit attempt caps, finite service
windows, and the adapter's finite submission budget must establish a soak start
ceiling; historical uncapped trials cannot retrospectively be described as bounded
by the new policy.

The full regression also exposed descriptor-lifetime lock retention during
concurrent process creation. Coordinator and offline-operation lock guards now
explicitly release ownership when dropped, even if a duplicated file description
remains temporarily open. A focused duplicate-description test exercises this
without timing guesses or weakening active-service refusal.

Live-agent diagnostic decisions now persist their actual `observe_only: false`
mode. The old first-milestone database restriction to `true` would terminate an
otherwise authorized agent after the execution path began reporting honestly.
Observe-only decisions remain `true`; persisting either mode never creates an
allocation or authorizes execution. The regression verifies exact mode round trips
and an empty execution journal.

The final focused rerun after the live-supervision reap fix passed 32 coordinator,
8 execution-state, 19 state, and the connected two-agent service test. The latter
completed in 8.460402334 seconds with both nodes contributing distinct SDK results
to one experiment, generation-2 checkpoint resume, current-model input transfer,
coordinator-restart receipt preservation, and every local allocation Released.
This is same-Mac-kernel service evidence, not anchor/burst Linux validation.

Status now includes the exact ordered task IDs for each job, including cancelled
jobs. Clients can exclude cancelled work from active backlog without equating job
and task names or erasing retained uncertainty. The 4,000-task payload measurement
above includes that mapping. The live service test preserves failed fixture
journals/logs under the project home directory; success removes the private test
fixture only after owned services stop and all local allocations are Released.

The final all-target local regression following these fixes passed **205 Rust tests
with no failures**; two deliberately ignored subprocess fixture entrypoints are
invoked by their owning integration tests. Strict all-target Clippy also passed.
Raw output and exact source-file SHA256 values are recorded in
`artifacts/validation/mac-regression-20260905/coordinator-final2.json`,
`cargo-test-coordinator-final2.log`, and `clippy-coordinator-final2.log`.
This supersedes the earlier local test totals for the recorded source snapshot.
Real Linux runtime, real Kaggriculture two-server contribution, shared-server
pressure experiments, optional delegated controls, and elapsed 24-hour soak each
still require their own native evidence and authorization gates.

## Private durable state without disrupting SQLite locks

Writable state creation no longer inherits public permissions from an ambient
umask. New directory components use `0700`; only the verified existing state leaf
is tightened, with no chmod of home, existing ancestors, or foreign-owned paths.
SQLite DB/WAL/SHM/rollback-journal files must be owned singly-linked regular files.
Writer startup refuses directory symlinks, unrelated nonempty directories without a
state DB, and ambiguous SQLite links before permission changes. Existing readers
still open without chmod or migrations. No ACL hardening or credential isolation
from other processes using the same account is claimed by Unix mode bits.

A new empty database is created `0600` under a private temporary name, synchronized,
and closed before no-overwrite atomic publication. Existing database descriptors
are never opened/closed outside SQLite solely for permission repair: this would
cancel that process's POSIX SQLite locks. Directory-relative `fchmodat` with
`AT_SYMLINK_NOFOLLOW` tightens existing file modes, with identity/mode verification;
already-private files require no such operation. If the runtime cannot securely
apply that interface, permissive existing state is refused instead of following
links. Pinned SQLite Unix VFS source derives WAL/journal/SHM modes from the DB;
a subprocess with umask `0000` verifies all three actual modes are `0600`.

This follows [SQLite's warning about external descriptor closure and POSIX locks](https://www.sqlite.org/howtocorrupt.html)
and the [runtime-dependent nofollow chmod interface](https://man7.org/linux/man-pages/man2/fchmod.2.html).
A separate-process writer remains blocked during an existing SQLite transaction
after mode repair, exercising actual OS locking rather than only SQLite's shared
in-process bookkeeping. Scope results (23 state, 32 coordinator, 6 backup and
8 execution-state tests; strict Clippy and formatting) and source hashes are in
`artifacts/validation/mac-regression-20260905/state-privacy.json`. This does not
convert the separate agent FD-retention assertion failure or pending native
source004 run into a passed test.

## Independent CPU/RAM admission while GPU evidence is unknown

`Decision.cpu_ram_expansion_allowed` has its own continuous-stability timer using
the existing configurable cooldown. It requires known matching CPU/RAM evidence,
positive capacities, no explicit drain, and all retained/pending CPU/RAM charges
within budget. It does not reset merely because GPU activity remains unknown.
The original global `expansion_allowed` stays false under GPU uncertainty; only
CPU/RAM admission headroom can remain positive. GPU admission headroom is zero,
even when a prior protection cap remains positive in the accounting budget after
GPU workloads have released resources. Missing/stale telemetry, excessive charges,
and the operator envelope still fail closed.

The authenticated node report now carries `gpu_expansion_allowed`, default false
for older JSON. Scheduling skips GPU requests without this evidence, and the first
execution grant rechecks it after reservation. Retained GPU accounting budgets are
preserved; this does not turn them into free GPU capacity or alter authorization,
leases, receipt handling, or the allocation ledger. CPU-only work and ordinary
GPU work retain the same scheduling path and launch barrier.

Focused policy (38) and coordinator (33) tests passed, including a 137ms independent
cooldown, unknown GPU evidence after confirmed release, zero and positive retained
GPU caps, stale CPU evidence, missing RAM, excess/unknown managed observations,
explicit drain, and loss of GPU admission evidence between offer and preparation.
Evidence is `artifacts/validation/mac-regression-20260905/cpu-ram-admission.json`.
Following the user's efficient-verification steering, the root agent owns the
single consolidated native source004 pass; these focused results do not replace it.
