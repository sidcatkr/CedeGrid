# Validation and deployment gates

Source distribution: linked private deployment evidence is withheld.
All reported failures, unsupported capabilities and pending gates remain applicable.


## Current evidence — source034, 2026-09-07

Source034 adds explicit replayable burst storage, authenticated session fencing,
reservation reconstruction and quarantine, strict coordinator receipt validation,
and an occupied-GPU mode restricted to verified authorized external identities.
The earlier storage/GPU/temporary-route decisions were approved for these bounded
contracts. GitHub source publication is authorized. Physical operational completion
remains false; historical source033 and earlier records below are preserved as
versioned evidence, including their failures and earlier publication holds.

The integrated local regression run passed 223 tests with two ignored fixture
entrypoints before the final process-absence and family-recovery corrections.
Final affected checks passed 47 library, 9 replay-state and 3 reconciliation tests;
strict Clippy and Rust 1.88 all-target checks passed. Two real mTLS node services on one macOS
kernel passed both new fault lifecycles: in-place deletion of the active local
journal row and replacement with a corrupt cache. Each retained authority, fenced
the old session, quarantined state, restored reservations, preserved the accepted
result/checkpoint, resumed at a higher generation and finished six CPU tasks across
a coordinator restart. These are local service tests, not two physical hosts or
power-loss/filesystem-daemon-failure evidence.

The tests exposed two implementation defects that were corrected: a duplicate CPU
sample immediately after authorization caused premature conservative draining;
macOS reconciliation did not recognize a reaped process as absent. The new query
uses native libproc ESRCH, with unknown/permission errors still retaining capacity.
Lost mediated-child inventories also retain same-boot family reservations.

Source034 native Linux execution, physical two-host useful work/loss/rejoin and
actual occupied-GPU sharing/protection remain pending. Both existing SSH sessions
ended and require reauthentication; fresh effective private-route policy could not
be read. No route settings were changed. The independent temporary-route restorer
passed 25 local fixtures, which do not establish live restoration. Prior source033
Linux and application results retain their original scope; they do not validate
new source034 paths. GPU policy tests use injected observations and do not claim
current third-party GPU use was authorized.

## Historical evidence — source033, 2026-09-07

**Overall operational completion remains false.** Source033 corrects the JSON
precision defect discovered during independent review of the source032 CPU
application. The affected native regression, strict source033 application repeat and actual
burst binary loader check passed. Useful work on two
physical hosts, physical burst loss/rejoin and managed GPU protection remain
unverified. The three storage, occupied-GPU and private-route decisions remain open.

| Dimension | Current status |
|---|---|
| Implementation and native correctness | Recorded native, precision, CPU application and loader checks passed |
| Burst durable storage | Both process-crash profiles passed; namespace durability unqualified |
| Physical two-host CPU work | Useful work, automatic demand/release/progress and physical restart/rejoin pending; storage and route are prerequisites |
| Mode B managed GPU runtime | Implemented and regression tested; actual eligible-device SDK/supervisor execution pending |
| Occupied GPU sharing and protection | Unknown activity remains excluded; sharing/protection and any weaker opt-in policy remain pending |
| Recovery and cleanup | Recorded local scopes passed; physical recovery remains unverified and uncertain charges stay retained |

Independent acceptance review (private evidence retained outside this source review).

The retained mergerfs 2.33.3 assessment identifies an `fsyncdir` handler that
returns `ENOSYS`, while the examined Linux FUSE path converts unsupported
directory-sync responses to success. A successful API call therefore does not
establish backing-directory durability. No supported namespace barrier has been
established within the allowed burst home. Both linked-SQLite journal profiles
passed their bounded process-crash probes; changing the journal cannot repair
the missing directory barrier. The gate concerns that missing supported mechanism,
not a demand to perform a destructive power-loss experiment.
Filesystem assessment (private evidence retained outside this source review).

The frozen source033 archive is SHA256
`d4614d21646d3a7a9812c1b6b8f0814a691cb6f436a57e3413f9a00169cd170d`.
All 116 manifest files matched before and after native execution, with no extra
source files. The archive has 117 members and includes the source032 reservation,
mediated-child, storage and automatic CPU demand corrections. A final audit matched all 27 current core/SDK/Cargo files to the frozen source and
found zero configured application/host disclosure patterns in core/SDK/examples.
This is a bounded source review, not publication or an exhaustive security audit.
Earlier evidence retains its original source and scope.
Frozen source review (private evidence retained outside this source review),
current core and SDK audit (private evidence retained outside this source review).

Source032 completed four real CPU games and two learner tasks, but two accepted
results retained SDK hashes that no longer matched their numeric metadata. A
subsequent read of the two exact worker descriptors proved both original hashes
valid: each accepted payload differed in one timing field by one binary64 ULP.
The original execution report, failed integrity audit, raw worker descriptors and
accepted receipts remain unchanged. All four replay byte hashes and all six
coordinator receipt hashes matched; those checks did not detect the embedded SDK
hash discrepancy.
Original application review (private evidence retained outside this source review),
raw worker origin proof (private evidence retained outside this source review).

Source033 enables the `serde_json` `float_roundtrip` reader so binary64 values
survive the tested spool, protocol, persistence and retrieval paths without that
drift. It changes parsing, with no database/schema migration or change to the
coordinator receipt digest algorithm. Strict SDK `result_hash` checks were added
to regression and application verification; this is not a new runtime coordinator
hash-validation API. The original protocol defect was reproduced before the fix.
The corrected library and two connected storage-profile tests passed locally,
as did all 40 coordinator tests and Rust 1.88 all-target compilation, formatting
and strict Mac/Windows GNU Clippy. Compilation does not establish Windows runtime.
Red protocol regression (private evidence retained outside this source review),
corrected protocol and connected checks (private evidence retained outside this source review),
portability checks (private evidence retained outside this source review).

Previously accepted result/checkpoint JSON bytes and receipt hashes remain
unchanged on reopen. Replaying the stored canonical legacy result, or the same
canonical checkpoint at the same sequence, returns its original receipt.
`GetResult` returns the stored legacy value; it does not reconstruct the original
worker float or repair an embedded SDK hash already inconsistent with that value.
Replaying the original higher-precision worker payload against that accepted
receipt is a digest conflict and is rejected. A different same-sequence checkpoint
is likewise rejected. Preserve accepted evidence and raw spool bytes and reconcile
through `GetResult` and receipt evidence. No alternate digest comparison,
automatic rewrite or migration fallback was added. Both legacy regressions fail
with the old parser and pass with the corrected parser. Both coordinator and
publishing agents must use the corrected reader for this precision guarantee;
mixed-version operation has not been validated and the fix cannot recover values
already rounded in stored JSON.
Legacy compatibility regression (private evidence retained outside this source review).

Actual native033 passed **234 tests, zero failures and five ignored fixture
entrypoints across 14 suites**, plus release compilation and ELF/libc inspection,
in **92.9283 seconds**. Named precision, legacy result/checkpoint and both-profile
connected-service tests all ran. The connected agents share one kernel; GPU paths
use injected observations and actual CPU gates. Standalone raw storage probes
were not repeated and retain their source031 process-crash scope.

All four directly owned stages exited zero and were reaped through pidfds without
harness signals. No sampled retained live descendant or zombie remained. Peak
observed stage-family RSS was 2,034,544,640 bytes within 8 GiB, using two physical
cores and at least 16 GiB RAM and disk reserves. These observations establish the
recorded direct ownership and sampled trusted lineage, not containment of arbitrary
descendants. The anchor binary SHA256 is
`4725127bf01ea6358b6e236c94a72998935d08082a8d6a8c0d1cdb83d520c87b`, requiring glibc 2.39.
Native report (private evidence retained outside this source review),
source and cleanup review (private evidence retained outside this source review).

The separate existing target compiler produced the glibc 2.35 candidate in
55.6542 seconds from 27 source-matched core/SDK/Cargo files. Its SHA256 is
`c92c483e715947e19d32ba0545200726c93f6cadbab94e970ab65022791cd3a7`.
Actual burst readback verified this exact hash, maximum required glibc 2.34
against installed glibc 2.35, and `cedegrid 0.1.0` with exit zero, without opening
state or starting work. The transfer observer timed out after 60 seconds before
the exact final ACK appeared. The completed transfer was verified, and only the
loader stage resumed; the transfer was not repeated.
Cross-build provenance (private evidence retained outside this source review),
actual target readback (private evidence retained outside this source review),
retained timeout and recovery (private evidence retained outside this source review).

The strict source033 application repeat completed four real CPU games and two
learner tasks in **179.1798 seconds**; its wrapper took 179.2061 seconds and the
real cycle 147.1223 seconds. Independent review recomputed all **six SDK hashes
and six coordinator receipt hashes** and verified all four replay gzip hashes.
Each replay contains 720 sequential frames and 719 inferred actions, with exact
episode, paired seed, seat, model and attempt bindings. All six allocations and
six node execution records were Released, with zero managed-child records; both
database integrity checks returned `ok` with zero foreign-key errors. The native
harness inspected learner continuation from global step 2 to 4. The local reviewer
checked its receipt/report bindings without downloading checkpoint, model or
dataset bytes.

The wrapper-owned cycle and controller exited zero and were reaped without signals.
The controller separately sent TERM to its owned agent and coordinator and reaped
both, with exit codes zero and -15; their recorded identities were then absent.
Peak observed family RSS was 1,957,601,280 bytes within 4 GiB, with one
1,000-millicore worker on two allowed physical cores; affinity is not a hard quota.
This repeat closes the observed result-hash defect for the corrected source and
recorded application path. It does not rewrite source032 evidence or establish
CUDA, mediated-child application work, physical two-host recovery, comparison or
a repeated ordinary-command/coordinator-outage test.
Application report (private evidence retained outside this source review),
independent receipt, replay and cleanup review (private evidence retained outside this source review).

A final bounded eligibility read again found no empty-device candidate: anchor
reported two graphics contexts, while burst null-buffer count-only queries reported
3, 3, 3 and 1 compute contexts and zero graphics contexts. Those counts need not
identify or deduplicate processes; nonzero occupancy alone excludes an empty
candidate. Burst collected no process records, IDs, owners, names or per-process/
device memory. No process-activity API was retried, and no CUDA, state or workload
was started. All observers were reaped. The first count-only helper expected the
wrong insufficient-size status (5); the installed binding established 7, and a
separately named bounded correction passed. Its failed report remains unchanged.
The original full burst inventory action was rejected by automatic approval review
for process-metadata concerns and never ran; the approved count-only alternative
completed. No new eligible Mode B workload became executable in these windows.
Latest eligibility read, original failure and safer recovery (private evidence retained outside this source review).

Physical source-only staging, exact asset fingerprints and 21 automatic CPU demand
driver tests remain preparation evidence. The automatic stage requires actual
externally owned pressure, fresh target-specific draining, reservation release,
new anchor progress and recovery; explicit drain or a readiness file cannot pass
it. No physical pressure or process-restart result has been produced. Strict burst
storage refusal, unknown GPU activity handling and private-route restrictions
remain in effect. Public release is held, with no public repository or push;
optional cgroup application remains unverified and the 24-hour soak remains waived.
The acceptance record (private evidence retained outside this source review)
retains all 29 requirements and earlier evidence within their original scopes.

## Historical evidence — source032, 2026-09-07

**Overall operational completion remains false.** The integrated source032
corrections passed their affected native Linux regression. Useful work on two
physical hosts, physical burst loss/rejoin and managed GPU protection remain
unverified. The source032 anchor application harness passed, but independent
verification found an embedded result-hash discrepancy after JSON roundtrip.
The following records describe source032 before the source033 correction above;
the original application report is retained.

The immutable source archive is SHA256
`c86743a1913a414c43f26a33468580bfbb9b4bf4c0d40fac3b92dcfdcc7aee07`. All 116 manifest
files matched before compilation and after the run, with no extra source files.
The archive has 117 members; the private source review checked 58 documentation
links, two Rust fixtures and 18 module dependencies. This is source provenance and
bounded export review, not publication or an exhaustive security audit.
Frozen source review (private evidence retained outside this source review).

The actual native run passed **75 tests, zero failures and three ignored fixture
entrypoints across five suites**, plus release compilation and ELF/libc inspection,
in **77.8775 seconds**. The suites were library tests, agent, execution state,
managed supervision and connected services for both storage profiles. The ignored
entries are invoked by their owning subprocess tests. GPU checks use injected
observations and actual CPU child gates; no CUDA workload was admitted. The two
connected agents share one kernel. The earlier raw linked-SQLite probes were not
repeated and retain their source031 process-crash scope.

All four directly owned stage processes exited zero, were reaped through their
pidfd handles and received no harness signals. No sampled retained descendant or
zombie remained. Peak observed stage-family RSS was 2,056,036,352 bytes, within the
8 GiB bound, using two physical cores and at least 16 GiB RAM and disk reserves.
This establishes the recorded direct ownership and sampled trusted lineage;
it does not prove containment of arbitrary descendants.
Native report (private evidence retained outside this source review),
source and cleanup review (private evidence retained outside this source review).

The source032 anchor application harness completed four real CPU games and two
learner tasks in 206.0514 seconds; its wrapper took 206.0776 seconds and the real
cycle 173.9706 seconds. Each replay has 720 sequential frames and 719 inferences.
Independent review verified all four compressed replay hashes and their episode,
model, attempt, seat and seed identities, and all six coordinator receipt hashes.
However, two embedded SDK result hashes could not be reproduced from the returned
metadata. A one-ULP alternative for one numeric field in each reproduced the
declared hash; the raw worker result bytes had not yet been pulled at that initial review. This is a newly found
integrity gap, with correction and follow-up evidence pending, not an all-hashes
pass. The original successful execution report and failed hash audit are retained.

All six task allocations and six node execution records were Released, with zero
managed-child records. Both databases reported integrity `ok` and zero foreign-key
errors. The cycle and controller processes exited zero and were reaped without
signals; the controller separately sent TERM to its owned agent and coordinator
and reaped both, with exit codes zero and -15 respectively. Their recorded service
identities were absent. The native harness inspected checkpoint continuation from
global step 2 to 4; the local reviewer checked the recorded receipt/hash bindings
without downloading or loading checkpoint bytes. Peak family RSS was
1,948,213,248 bytes within the 4 GiB bound, with one 1,000-millicore worker and
two allowed physical cores; affinity is not a hard CPU quota. No mediated GPU
child, physical two-host work, pressure comparison or repeated coordinator outage
was exercised by this run.
Application report (private evidence retained outside this source review),
independent replay, receipt and integrity review (private evidence retained outside this source review).

The reservation correction retains all unreleased charges, including shared CPU
and RAM, but compares GPU capacity only for UUIDs requested by the new allocation.
An unrelated GPU deficit therefore cannot block independent CPU or other-device
work. Requested-device uncertainty, pending charges and same-task fencing remain.
The original behavior failed the new regression; 103 focused Mac tests and Clippy
then passed. Mediated children now check current GPU authority before reservation
and immediately before EXEC. The final check receives pending authenticated
commands without another policy sample, then rechecks the current lifecycle,
owned parent and child preparation deadline. Renewal timing starts before the
potentially slow control call. The original follow-up gap was reproduced; 93
focused Mac tests passed with two unchanged fixture entrypoints ignored. Native032
subsequently covered the affected paths. None of these tests establishes occupied
GPU sharing, external protection or guaranteed GPU admission.
Reservation correction (private evidence retained outside this source review),
mediated-child correction (private evidence retained outside this source review).

Rust 1.88 all-target compilation, formatting, strict Mac Clippy and strict Windows
GNU all-target Clippy passed for the integrated core. An offline build using the
existing compiler target produced a candidate targeting glibc 2.35 in 49.2946
seconds from 27 source-matched core/SDK/Cargo inputs. Its SHA256 is
`2a4f20f489d392872a727f3227678e806e7ff7256a4503d6c18eb4e5cb436cfd`.
Actual burst readback verified that binary hash, a maximum required glibc version
of 2.34 against installed glibc 2.35, and `cedegrid 0.1.0` with exit zero. It opened
no state and launched no workload. The initial control response timed out after
30 seconds; its completed report was recovered read-only without repeating the
binary creation or version command. The anchor-built binary has SHA256
`5216e12fa53ab642fd1a625249fbc8504ede402f57f0fd4ad8bc4d880d36d424` and requires glibc
2.39, so its anchor execution does not establish burst loader compatibility.
Portability checks (private evidence retained outside this source review),
cross-build provenance (private evidence retained outside this source review),
actual target readback and retained timeout (private evidence retained outside this source review).

Physical preparation verified that the selected affinities represent two physical
cores on each role. It fingerprinted the exact listed application, SDK, dataset,
snapshot, model and binary inputs within the recorded read bounds. The first
anchor inspection stopped at the checkpoint's 32 MiB per-file limit, and planned
source directories were initially absent; those incomplete reports remain intact.
Separate hash-verified staging installed seven anchor tool files and 104 burst
application/SDK files. An isolated bounded read then fingerprinted the exact
38,505,923-byte checkpoint without loading it. Source staging started no application,
changed no network setting and did not admit unqualified storage.
Physical preparation evidence (private evidence retained outside this source review).

The optional automatic CPU demand driver passed 21 tests in the actual checkout
and independent review. It requires the target assignment and generation to report
draining in the same fresh burst report as the reduced CPU budget. New anchor
progress uses its first-seen controller timestamp with a five-second clock-skew
margin. The pressure interval comes from the common span of both CPUs' completed
requests; cleanup time is excluded. Actual pressure launch and reaping belong to
the outer owner. A readiness file, explicit drain, stale telemetry or a cleanup-only
interval cannot establish an automatic demand pass. This prepared tooling has not
produced physical pressure evidence or process-restart recovery.
Applied driver tests (private evidence retained outside this source review),
independent review (private evidence retained outside this source review).

The three storage, occupied-GPU and private-route decisions remain unanswered.
No weaker storage contract, unknown-as-idle interpretation or route exception has
been enabled. The acceptance record (private evidence retained outside this source review)
retains all 29 requirements and earlier evidence with its original scopes. Public
release remains held; no public repository or push exists. Optional cgroup
application remains unverified, and the 24-hour soak remains waived.

A later bounded read verified both exact raw worker descriptors and their original
SDK hashes. Each accepted payload differed in one numeric field by one ULP,
confirming the origin of the discrepancy. That later proof and the source033
correction above supersede the initial origin uncertainty while preserving the
original failed audit and accepted bytes.
Raw-spool origin review (private evidence retained outside this source review).

## Historical evidence — source031 and subsequent scoped changes, 2026-09-07

Deployment nodes are described by their anchor/burst roles here. Actual machine
identities, device details and measurement provenance remain in private evidence.
**Overall operational completion remains false.** Storage profiles and GPU
capability handling are implemented with new native regression evidence; useful
physical two-host work and managed GPU runtime protection remain unverified.

The immutable source031 archive is SHA256
`3e8d8bca3793ee4983e3c4c69b092c22070fb500febdfdecb1e86c5a7d218847`. Its 116-file source manifest
was checked before and after native execution. The native run passed 222 tests
with five ignored across 14 suites, a release build and both linked-SQLite
diagnostic profiles in 79.8894s. Mac regressions passed 214 tests with four ignored
across 13 suites after an initial missing temporary directory was corrected; the
original failed run remains retained. Strict Mac Clippy and Rust 1.88 all-target
checks passed. Source031 evidence (private evidence retained outside this source review),
native report (private evidence retained outside this source review).

`wal_full` remains the default (`WAL`, synchronous `FULL`, schema 2).
`delete_extra` uses `DELETE`, synchronous `EXTRA` and schema 3 to fence older
readers. Configuration, coordinator, agents, independent supervisors, reconciliation,
read-only inspection and offline backup/restore preserve the selected profile.
Initialization requires a serialized fresh owned directory; reopening an
incompatible profile refuses before changing its journal. The existing filesystem
gate remains independent of journal selection.

Both raw linked-SQLite profiles passed the burst process-crash diagnostic: five
checks and five reaped owned children per profile, all 33 committed records retained,
128 dirty uncommitted records absent, a charged generation-7 reservation preserved,
and orphan publication recovered with a single receipt. This establishes the
recorded process-crash behavior only. Both reports explicitly retain
`namespace_durability_qualified: false`, and ordinary state opening still refuses
the unqualified FUSE filesystem. Changing WAL to DELETE does not supply a missing
backing-parent synchronization barrier. No production storage override was enabled.
WAL/FULL probe (private evidence retained outside this source review),
DELETE/EXTRA probe (private evidence retained outside this source review).

A separate test-only overlay passed the connected DELETE/EXTRA scenario on native
Linux in 15.5603s. It used one local kernel, one coordinator and two local agents,
accepted three tasks per node, resumed at generation 2 and released every local
allocation. After stopping all owned services, normal writable recovery preserved
the exact accepted payloads, receipts and local execution records; persisted
journal/profile settings were verified before and after. The first Mac test
correctly refused read-only inspection of a hot rollback journal; that failure and
the untouched failed state remain retained. The repaired test also passed on Mac.
Only `tests/distributed_service.rs` changed in this native overlay; all other 115
manifest files and the retained release binary were unchanged. The fixture ran a
debug binary built from the frozen core. This is local connected recovery coverage.
Connected profile evidence (private evidence retained outside this source review).

Actual no-state GPU observations on both roles completed eight samples at a
configured 500ms interval without CUDA work or state databases. Basic memory and
complete process inventories were reliable; process-activity queries returned
`NotFound`, with no new samples or valid freshness baseline. External compute
remained unknown and no device met the empty-inventory condition for conservative
non-sharing admission. The implementation records NVML entry point/status, raw
microsecond cursor/sample timestamps and monotonic baseline age. Buffered, repeated,
slow or stale samples do not become fresh idle evidence. Auto and conservative
non-sharing modes prohibit guaranteed GPU work and fully yield opportunistic GPU
work when required; guaranteed CPU work is evaluated independently. Initial
admission to an occupied device still requires an eligible activity state, including
in explicit contention-aware mode. These observations establish refusal and
capability reporting, without a managed GPU protection or sharing result.
GPU observation and remaining scenarios (private evidence retained outside this source review).

The physical bootstrap now accepts explicit native paths, CPU affinity, endpoint
and independent coordinator/agent storage profiles while preserving legacy defaults.
Its 19 tests and the 11 source-export dependency/allowlist tests passed in the actual
checkout. The new bootstrap has not yet run useful work on two physical hosts.
Current formatting and strict Windows GNU Clippy passed after narrow Unix-only
constant/builder corrections; the initial warnings are retained. These are
compilation checks. Later journal-neutral diagnostic, reservation-isolation and
mediated-child GPU freshness/lease-expiry checks require their own affected native
validation and immutable source record; source031 does not cover those deltas.
Tool checks (private evidence retained outside this source review),
portability (private evidence retained outside this source review).

The acceptance record (private evidence retained outside this source review)
separates implementation/native correctness, burst storage qualification, useful
two-host CPU work, non-sharing GPU runtime, occupied-device sharing, and recovery,
cleanup and uncertainty. The private source029 candidate is stale. Public release
remains held until the required operational evidence exists; no public repository
or push has been created. Optional cgroup application remains unverified and the
24-hour soak remains waived; neither is substituted for a required runtime gate.

## Historical evidence through source030

The records below retain their original source, machine and measurement scopes.
Earlier source008 equivalence and source027 current-runtime statements describe
those snapshots. Previously passed CPU stress/comparison, native application
operation and physical API/fault checks are not repeated or relabeled as current
GPU sharing or useful physical two-host application evidence.

- **Source027 mixed-class correction passed affected regressions:** policy
  allocations now retain their durable execution class. Automatic yielding selects
  opportunistic victims while guaranteed pending/running/uncertain charges remain
  fully accounted; explicit node drain still includes both classes. Legacy policy
  recordings with no class retain their Opportunistic default. This prevents a
  guaranteed allocation from consuming a planned victim slot that the agent would
  not actually drain. Both state and live-collector producers preserve class.
  Eighty-six affected Mac tests and strict Clippy passed, followed by 109 native
  Linux tests and release compilation in 70.2582s total. Release binary SHA256:
  `141b151679c7ef37c2d357265e69c311ec15962492a2ceac85cbe76497ee692a`.
  Native027 evidence (private evidence retained outside this source review).
  Source027 also passed formatting, Rust1.88 all-target checks and strict Windows
  GNU Clippy with the existing compiler. These are compilation checks, not Windows
  runtime evidence. Portability027 (private evidence retained outside this source review),
  affected Mac regressions (private evidence retained outside this source review).
- **Connected mixed-class028 passed in72.4283s:** the independent supervisor
  selected only opportunistic work under CPU pressure. Its release was confirmed
  0.41664s after pressure readiness; the guaranteed worker progressed451-to703
  afterward and completed2,950units with an accepted result. The independent
  same-UID process completed31,471units over20.00055s, exited normally without
  signals and remained outside the registry. Both allocations Released, all
  recorded identities were absent, and both databases passed integrity/foreign-key
  checks with unchanged database/WAL hashes. Peak sampled familyRSS was227,659,776bytes.
  A GPU marker stayed queued5.28087s despite free slots and CPU/RAM capacity;
  fresh unknown GPU evidence kept its GPU admission capacity zero. Cancellation
  left no preparation, execution or GPU context. This verifies refusal, not
  managed GPU protection. The first two private harness failures remain retained;
  the final test checks both actual decision producers without lowering targets.
  Connected028 (private evidence retained outside this source review),
  independent cleanup/integrity audit (private evidence retained outside this source review).
- **Bounded own-process GPU observation025/026 passed with limited scope:**
  burst025 completed in 22.7157s with 912 finite GPU work units and three positive
  fresh own-process activity samples; anchor026 completed in 21.2600s with 953 units
  and one positive fresh sample. Both exited normally with no signals, were reaped
  using pidfds, disappeared from `/proc` and released their own GPU contexts.
  Their processes stayed outside the manager registry. External activity remains
  unknown, so this does not pass managed GPU admission or protection. Anchor025's
  earlier observer-exit failure remains retained; only that role was rerun after
  a private bounded exit-check correction.
  Burst025 review (private evidence retained outside this source review),
  anchor026 review (private evidence retained outside this source review).
- **Native manual operations024 passed in 237.1977s:** one CPU worker completed
  four real games with 720 steps and 719 model inferences each; replay/model checks
  and learner checkpoint continuation from step 2 to 4 passed. During a five-second
  verified coordinator outage, a guaranteed test task progressed from 7 to 33 while
  the agent stayed live. Manual restart against the same state accepted its result.
  All eight tasks completed and all eight allocations released; both databases
  passed integrity and foreign-key checks with no unrecognized allocations.
  All recorded direct test identities were absent afterward. This is native
  single-host operation, not two-host useful work or GPU protection.
  Native024 review (private evidence retained outside this source review).
- **Physical protocol-fault023 passed:** the 7.1837s runner discarded a committed
  artifact response before the SDK caller received it. Retrying returned the same
  ACK; the stopped coordinator's database retained exactly one 4,032-byte publication.
  Three stale-attempt operations were rejected and both unexecuted offers Released.
  No offer was Prepared and no workload result was accepted. This is application
  response loss, not packet-level loss or a lost workload-completion ACK. Server
  runtime was 110.1785s; all three anchor and 24 burst recorded PIDs were absent,
  ShieldsUp was restored and Serve configuration was empty.
  Protocol-fault evidence (private evidence retained outside this source review),
  durable publication audit (private evidence retained outside this source review).
- **Linux core and SDK paths passed:** one consolidated source004 suite ran 217
  tests, with no failures. Four ignored entrypoints are fixtures invoked by owning
  tests; optional experiments remain separate. Source008 then passed only the
  eleven affected CPU checks and 36 affected tooling checks. Two original Python
  bundle/path failures were fixed and passed targeted native verification; their
  original failed reports remain retained.
- **CPU sampling correction passed:** native stress exposed mismatched windows and
  missing aggregate busy ticks. Raw independent process/scheduler/idle counters
  established the cause. The conservative elapsed-minus-idle envelope changes no
  reserve or policy threshold. Ten successive native startup trials completed
  without false drains. See startup evidence (private evidence retained outside this source review).
- **Real Kaggriculture CPU stress passed in 230.77s:** one ordinary managed game,
  four cooperative games, accepted results, selected actor yield, and generation 2
  rejoin under the same experiment. The independent same-UID process received
  2.0027 CPU cores after release, completed its 45s interval without any signal,
  and stayed outside the managed registry. Managed learning continued from step 2
  to 4; peak observed family RSS was 1,829.88MiB. All eight allocations released and all
  owned processes were reaped. Run report (private evidence retained outside this source review),
  final durable status (private evidence retained outside this source review).
  The separate artifact review (private evidence retained outside this source review)
  passed actual replay/model/dataset integrity and checkpoint step 2-to-4 checks.
- **Portability checks passed:** formatting, Rust 1.88 check and strict Windows
  GNU Clippy with an actual target C compiler. These establish compilation,
  not Windows execution or Linux kernel enforcement. Evidence (private evidence retained outside this source review).
- **Owned CUDA lifecycle passed separately:** source004 observed a real CUDA
  context, used pidfd supervision, and confirmed process/GPU release. It does not
  establish managed CUDA application admission or protected-workload GPU latency.
  Evidence (private evidence retained outside this source review).
- **Connected CPU pressure comparison passed:** 15 cases across five scenarios
  and three repetitions completed in 445.15s. The original goals passed: matched
  idle probe throughput was 1.0115x baseline, decision p95 was 0.3132s, release p95
  was 1.3891s, and protected post-yield p99 latency was 1.0026x baseline. Sampled
  manager peak CPU was 0.1922 logical CPU and conservative peak RSS was 86.35MiB;
  all three idle CPU intervals remained below 0.1 logical CPU. No accounting
  coverage gaps or uncertain allocations remained. All nine independent protected
  processes completed normally with zero signals, and all owned processes were
  reaped. These results cover synthetic CPU pressure on anchor, not GPU sharing or
  matched Kaggriculture throughput. Raw report (private evidence retained outside this source review),
  completed metric and cleanup review (private evidence retained outside this source review).
- **Matched real Kaggriculture CPU throughput passed:** three pairs of twelve games
  completed with only the missing cases rerun. Median managed/unmanaged throughput
  was 0.9868897 against the predeclared >=0.90 target; total active execution was
  1479.0233s across two segments. A fresh read-only audit verified all 72 games and
  replay hashes, 18 input paths, three Released managed assignments and absence of
  16 recorded owned identities. Continuation manager overhead passed with no gaps;
  the original segment retains four missing samples and inconclusive overhead.
  Reviewed evidence (private evidence retained outside this source review),
  integrity/cleanup audit (private evidence retained outside this source review).
- **Docker ARM64 Linux runtime passed:** the home-installed Docker Desktop 4.89.0
  image built in 320.48s with explicit 2 CPU/4 GiB build limits. The actual VM
  exposes 8 CPUs and 8,319,770,624 bytes RAM. The non-root validation container
  used two CPUs, 4 GiB RAM, no additional swap, PID limit 256, dropped capabilities
  and a qualified ext4 named volume. Twenty-six focused runtime tests passed:
  three reconciliation tests, one connected two-local-agent service test, six
  managed-supervision tests and sixteen supervision tests. One ignored fixture
  was exercised by its owning test. The first service test's stale-observation
  failure remains retained; a fixture-only 500ms observation cadence fixed its
  unrealistic 50ms polling interval without changing production freshness,
  leases, drain deadlines or acceptance targets. Runtime review (private evidence retained outside this source review),
  original failure diagnosis (private evidence retained outside this source review).
- **Real Docker Kaggriculture CPU cycle passed in 288.10s:** four real 720-step
  games produced accepted replay artifacts, followed by managed learning and
  checkpoint continuation from step 2 to 4. All six allocations released and
  owned services were reaped. Actual artifact review passed; peak sampled family
  RSS was 1,457,795,072 bytes. This is one Linux-container application cycle, not
  a matched performance comparison, a second ordinary-command test or physical
  two-node evidence. Artifact review (private evidence retained outside this source review),
  run report (private evidence retained outside this source review).
- **anchor+Docker startup failed before work:** run016 could not establish its TCP
  route; the source-restricted owned proxy accepted zero connections. No jobs or
  allocations were created. The exact five anchor process identities were verified
  absent after cleanup; Docker cleanup is recorded separately. The raw bootstrap
  report retains an `unknown node` cleanup-classification defect, corrected only
  in subsequent tooling with two focused fault tests. The failed run remains a
  failure. That historical startup did not establish physical useful work, continuity or
  reconnect. The later API021 transport pass is separate. Failure and cleanup evidence (private evidence retained outside this source review).
- **Direct server diagnostics completed through the existing SSH sessions:**
  anchor and burst reached each other's SSH listeners on port 1022 in 1.476ms and
  1.988ms respectively. However, the actual burst SDK request to anchor's bounded
  API endpoint timed out in 3.012s; the source-restricted proxy accepted zero
  connections during its 100.383s lifetime. Local mTLS operator status and
  wrong-node-role rejection passed, while remote authentication and the remote
  missing-certificate check were not reached. This identifies an API-path gate,
  not general server unreachability or a proven firewall/Mac cause. Three exact
  owned identities were absent afterward; all nine private scheduling tables
  remained empty with an unchanged database hash and successful integrity check.
  API summary (private evidence retained outside this source review),
  independent integrity/cleanup review (private evidence retained outside this source review).
- **Actual NVIDIA observation preserves the uncertainty gate:** anchor's production
  `doctor` and six-frame, 500ms `observe --no-state` run succeeded in 2.587s without
  state creation, workloads or signals. Every frame retained unknown external
  GPU activity and zero GPU admission budget despite reported 0% utilization.
  On burst, six rounds across four L4 UUIDs returned NVML `Not Found` and no valid
  process-activity samples. Existing compute contexts were present on all four
  devices. These are missing activity observations, not idle-capacity evidence
  or GPU-sharing validation. anchor observer (private evidence retained outside this source review),
  burst NVML (private evidence retained outside this source review).
- **Historical private packaging018 passed focused checks:** that Cargo
  root-anchored include list contained 101 files, excluded raw/runtime/private
  key paths and retains all 65 required fixtures. Git artifact candidates are
  excluded; three Docker-profile tests now run through the existing unittest CI
  discovery. An offline SDK wheel build and isolated Mac install passed 14
  existing tests against the installed wheel in 0.903s. The 24 core/SDK files and
  Cargo.lock were unchanged at018; Cargo.toml differed only in package metadata. This
  CI now selects the same verified 500ms fixture cadence; its YAML/configuration
  check passed, with no new full-CI runtime claim. This audit did not publish
  anything or select a license. Deployment-specific docs
  were subsequently curated in the private source-review021 export. The later
  CedeGrid023 license/metadata checks are recorded separately below.
  Packaging audit (private evidence retained outside this source review),
  installed wheel evidence (private evidence retained outside this source review).
- **Home-only Tailscale installation and peer connectivity passed:** both user-owned
  userspace instances are enrolled for the anchor and burst roles. Three pings in each
  direction passed, reaching direct UDP at approximately 1ms. Private directories
  and sockets remain beneath each user's home; the existing burst system service
  was unchanged. This is the explicitly authorized Tailscale installation exception.
  Authenticated ResourceManager API validation020 timed out with ShieldsUp
  preserved; no remote authentication pass is claimed. Peer pings do not establish application authentication or distributed
  correctness. Installation and peer status (private evidence retained outside this source review).
- **CedeGrid023 metadata and installed SDK packaging passed:** the user selected
  CedeGrid and Apache-2.0 and authorized public release. Root and SDK license/NOTICE
  files match. The original 112-file Cargo list excluded runtime/key paths but its
  broad docs glob included the private worklog; its earlier blanket privacy claim
  was too broad. The separate source exporter excluded that worklog and nothing
  was published. Offline SDK source distribution, a wheel rebuilt from it, and 14
  existing tests against the installed wheel passed. The runtime binary remains
  `resmgr`, Rust library `resource_manager`, Python distribution `resmgr-sdk`, and
  import `resmgr`; no runtime API rename or dependency upgrade occurred. Crates.io
  publication remains disabled. Public source publication is deferred pending the required testing gate; no
  repository or public push exists yet. Packaging does not establish operational completion. Final package and SDK review (private evidence retained outside this source review).
- **Cargo boundary correction024 passed one offline check:** an explicit document
  list now excludes the private worklog, and the root README points to validation.
  The 113-file package retained 61 required source files, the Rust include fixture,
  and 24 root-document links. No package build or installation was repeated. This
  is file-list/link closure, not a content-redacted public source export; the
  separate exporter still curates private deployment links and evidence.
  Packaging024 review (private evidence retained outside this source review).

The completed validation runs, including physical-fault023, native024 and GPU025/026, left no
workload or ResourceManager service active. Native024 confirmed all eight allocations
Released and no unrecognized allocations; physical-fault023 retained only its
single committed artifact and two Released unexecuted offers.
The separate API diagnostic020 also finished with verified cleanup; its physical
request timed out and its focused native tests passed. Both owned
Docker containers are stopped without OOM and with restart policy `no`; their
named volume and raw checkpoints remain available. Docker Desktop may remain
idle. Existing SSH/tmux sessions are retained, the extra tunnel session is absent
and no tunnel is listening. Container cleanup (private evidence retained outside this source review),
transport status (private evidence retained outside this source review).
The later direct-server API diagnostic also stopped and verified its owned
services. During that017 diagnostic no software was installed on burst, and no
agent, CUDA context or application workload was started there. Diagnostic copies
and run-specific mTLS credentials stayed in its private home; exported reports
contain no private keys. Subsequently authorized Tailscale019 installed only
home-local binaries and intentionally left the two private daemons running.
There is no persistent autostart or unattended ResourceManager monitoring.

The user replaced required 24-hour acceptance with bounded stress. The actual
bounded CPU stress above passed; the long-soak harness is optional and unstarted.
There is no 24-hour endurance or unattended monitoring claim.

## Requirement, code and evidence matrix

Paths prefixed `Kaggriculture:` belong to the separate application's checkout,
`integration/resmgr`; no application imports or server identities enter the core.
Each row applies only to its stated evidence scope.

| Requirement | Implementation and executable tests | Current evidence or gate |
|---|---|---|
| Versioned config, configurable deadlines, execution opt-in and selected storage/GPU modes | `src/config.rs`, `src/main.rs`; `tests/policy.rs`, `tests/cli.rs` | Source033 native configuration, policy, CLI and affected child-launch regressions passed; selected storage/GPU modes retain their explicit contracts |
| SQLite WAL/FULL or DELETE/EXTRA, schema fence, private state and independent filesystem gate | `src/state.rs`, `src/storage_qualification.rs`; `tests/state.rs`, `tests/storage_qualification.rs`, `tests/backup.rs` | Source031 native profile/recovery tests and both burst raw process-crash probes passed; connected DELETE/EXTRA add-on passed locally. Burst namespace durability remains unqualified and production open refuses |
| CPU/RAM matching scopes and per-UUID GPU capability/freshness | `src/kernel.rs`, `src/telemetry.rs`, `src/policy.rs`; corresponding tests | Source031 native regressions and eight no-state samples per role passed; activity stayed unknown and no empty device was eligible.  Native004/source008 CPU and pressure evidence preserved. Diagnostic025/026 fresh own-process GPU activity/release observed on both roles; other contexts remain unknown. Managed GPU admission/protection and GPU pressure benefit remain unverified |
| Scoped PSI and capability/enforcement reporting | `src/kernel.rs`, `src/cgroup.rs`; `tests/kernel.rs`, `tests/cgroup.rs` | Native system CPU/memory/IO PSI baseline and fresh deltas verified; applied delegated controls, workload PSI and external attribution unverified |
| Durable launch barrier, verified identity, stable signaling/reaping | `src/supervision.rs`, `src/rootless.rs`, `src/execution_state.rs`; `tests/supervision.rs`, `tests/agent.rs` | Source032 and source033 passed affected child authorization and lease/deadline regressions with injected GPU observations and CPU child gates. Native004 runtime/fault and separate CUDA-release evidence, and Docker ARM64 supervision coverage, retain their original scopes |
| Mediated descendants, common deadlines, no unrelated same-UID signals | `src/child_supervision.rs`, `src/managed_children.rs`, `python/resmgr/process.py`; `tests/managed_supervision.rs`, `tests/managed_children.rs`, `tests/family_contract.rs` | Source032 and source033 passed affected managed-child deadline, fresh-authority, cancellation and CPU runtime checks. Native004 supported-family history remains valid; arbitrary forks/daemonization remain unsupported |
| Pending/live/uncertain resource accounting | `src/execution_state.rs`, `src/coordinator.rs`; `tests/execution_state.rs`, `tests/coordinator.rs` | Source031 transactional/profile/per-UUID placement regressions passed. Source032 and source033 passed the local reservation-isolation regressions while retaining uncertain charges. Physical two-node application remains pending |
| One coordinator, versioned mTLS, operator/node role isolation | `src/coordinator.rs`, `src/protocol.rs`; `tests/coordinator.rs`, `tests/tls_transport.rs` | Native004 real services and SDK transport passed; server017 local role checks passed but burst remote request timed out before authentication. Tailscale019 peer checks passed; mTLS020 timeout preserved; physical API021 mTLS/role/certificate/hostname checks passed |
| Priority/FIFO, pool bounds/minimum protection, cancellation | `src/coordinator.rs`, `src/agent.rs`; `tests/coordinator.rs`, `tests/agent.rs` | Native004 correctness passed; source008 real workload stress and three matched CPU application pairs passed |
| Continuous yielding, gradual growth, CPU-only admission despite GPU uncertainty | `src/policy.rs`, `src/agent.rs`; `tests/policy.rs`, `tests/coordinator.rs`, `tests/agent.rs` | Native004/source008 CPU actor yield/release/generation2 and15-case decision/release goals passed. Own-process GPU activity025/026 is diagnostic only; no managed GPU scale or protection pass follows while external contexts remain unknown |
| Distinct disconnect/agent/supervisor failure and selected-profile recovery contracts | `src/agent.rs`, `src/supervision.rs`; `tests/agent.rs`, `tests/agent_reconciliation.rs`, `tests/distributed_service.rs` | Native004/Docker fault coverage preserved; native024 guaranteed task progressed7-to 33 during a five-second coordinator outage and completed after manual same-state restart. Physical burst loss/rejoin remains pending; no supervisor-death enforcement claim |
| Attempt fencing, bounded retries/yield backoff, idempotent acceptance | `src/coordinator.rs`, `src/state.rs`; `tests/coordinator.rs`, `tests/state.rs` | Source033 exact protocol/legacy accepted-result/checkpoint regressions passed, preserving canonical old bytes and refusing conflicting retries. Native004 passed; physical-fault023 rejected three stale operations and replayed one committed artifact ACK after application-response discard. No accepted workload-result ACK fault was injected; external side effects remain workload responsibility |
| Checksummed uploads, atomic publication, checkpoints and results | `src/artifacts.rs`, `src/coordinator.rs`; `tests/artifacts.rs`, `tests/coordinator.rs` | Source033 strict application review verified six SDK hashes, six coordinator receipt hashes and four real replay hashes; native checkpoint2-to4 continuation and DB integrity passed. Native004/real application history preserved; physical API021 resumable artifacts passed and fault023 offline audit verified one 4,032-byte publication after lost-response retry. Native024 checkpoint2-to 4 and database integrity passed; not power-loss testing |
| Python lifecycle, drain, resume, completion, immutable inputs and spool cleanup | `python/resmgr/worker.py`, `python/resmgr/client.py`, `src/agent.rs`; `python/tests`, `tests/agent.rs`, `tests/tls_transport.rs` | Source033 both-profile connected SDK hash/restart checks and strict six-submission real application review passed. Historical Native004 SDK and Python repaired tests retain their scopes |
| CLI start/submit/status/drain/resume/reconcile, raw storage diagnostic and offline backup/restore | `src/main.rs`, `src/backup.rs`; `tests/cli.rs`, `tests/backup.rs`, `tests/storage_qualification.rs` | Source031 selected-profile operations, history and backup/restore passed; local connected DELETE/EXTRA recovery passed.  Native004 passed; native024 manual start, idle drain/resume, ordinary command, real application and same-state restart passed with eight Released allocations. Snapshot RPO/source retirement and uncertainty retained |
| Generic non-application command example | `python/examples/counter.py`; `tests/agent.rs` | Native004 ordinary-command/SDK execution passed |
| Real Kaggriculture command and finite cooperative learning continuation | `tools/local_smoke.py`, `tools/review_local_smoke.py`; `Kaggriculture:integration/resmgr/worker.py`, `workflow.py`; application `tests/resmgr/test_resume.py`, `test_workflow.py` | Source033 repeated four real720-step/719-inference CPU games and checkpoint2-to4 continuation, with six exact SDK/coordinator hashes, four replay hashes and all6allocations/records Released. Native008/Docker015/native024 evidence retains its scope. One host contributed; no useful two-node claim |
| One experiment across anchor+burst, anchor continuity, rejoin/current model | `Kaggriculture:integration/resmgr/workflow.py`, `src/agent.rs`, `src/coordinator.rs`, `tools/two_node_validation.py`, `tools/two_node_bootstrap.py`; `tests/distributed_service.rs` | Source033 passed both selected-profile connected scenarios with exact SDK hashes and restart/checkpoint checks using two local agents on one kernel. Physical source staging and 21 automatic-demand tooling tests passed; no useful physical run is claimed. Historical startup failures retained. API021 physical mTLS/artifacts/owned-connection reconnect passed with zero accepted workload results. Physical useful work remains blocked by burst unsupported home state storage; the Docker alternative lacks Mac peer visibility. Bounded test limits are delegated to the agent |
| Matched throughput, protected latency, manager overhead and release timing | `tools/compare.py`, `tools/pressure_comparison.py`, `tools/pressure_probe.py`, `tools/anchor_validation.py`; corresponding tests | Native008 synthetic15-case and three real CPU pairs passed; original overhead segment retains gaps. Unmatched GPU025/026 diagnostic units/samples do not establish speedup or protected latency; GPU comparison remains pending |
| Required bounded real-workload stress | `tools/local_smoke.py`, `tools/pressure.py`, application `workflow.py`; `tests/test_local_smoke_stress.py`, application workflow tests | Actual native008 passed in 230.77s; all eight managed allocations released; independent artifact review passed |
| Optional long-duration soak | `tools/soak.py`, `tools/make_soak_config.py`, `tools/pressure_schedule.py`; `tests/test_operations.py`, `tests/test_validation_tools.py` | Required:false after user waiver; actual run not started |
| Portable core and optional backends | `Cargo.toml`, `src/lib.rs`; source031 portability and tool reports, historical Docker013 review | Integrated source033 Rust 1.88 all-target check, formatting and strict Mac/Windows GNU Clippy passed; actual source-matched burst hash/ELF/version readback is recorded separately from state/workload evidence. Historical ARM64 container runtime retains its scope. No Windows runtime, NVIDIA-on-Mac or applied ResourceManager cgroup claim |

## Remaining operational gates

The current required gaps are independent. Burst storage passed bounded linked-
SQLite process-crash compatibility but remains unqualified for durable namespace
publication. The retained mergerfs 2.33.3 directory-sync handler returns `ENOSYS`,
and the examined Linux FUSE path can turn that unsupported operation into success.
No supported backing-directory barrier is established in the permitted home; a
journal-mode change does not repair it. This is a missing mechanism, not a request
for a destructive power-loss experiment. Both SSH roles have been reauthenticated;
closed anchor SSH from
source030 is a historical access state. Prior authenticated physical API/artifact
checks remain valid within their transport scope. The private route is currently
restored to ShieldsUp enabled and empty Serve, so reactivating the previous route
needs the requested narrow exception under the current network-change restriction.

Useful work from two physical nodes, anchor continuity, physical burst loss/rejoin
and current-model transfer are still unverified. The local connected profile add-on
and physical bootstrap tests do not satisfy this gate. Managed non-sharing GPU
runtime is also unverified. The final bounded inventory/count-only observation
again found every device occupied, with no eligible empty candidate. This latest
window did not retry activity APIs or start CUDA/managed work. Occupied-device GPU sharing and measured protection remain separate
pending scenarios. Freshness cannot be inferred from aggregate utilization or an
NVML `NotFound` response.

Three narrow decisions remain unanswered: whether to allow explicit process-crash-
only burst storage with possible acknowledged-state loss across host/filesystem
failure; whether to allow opt-in bounded best-effort admission despite unknown
occupied-device compute; and whether to temporarily restore the previously tested
private route. No weaker storage, unknown-as-idle interpretation or route exception
has been enabled. Budget/window approval is already delegated for conservative
bounded tests; it does not supply these distinct guarantee or network exceptions.
The source033 native regression, strict anchor application hash/replay/cleanup
review and actual burst loader readback passed. They do not substitute for useful
physical two-host or managed GPU work. Earlier completed stress and
matched CPU comparisons retain their evidence and should not be rerun without a
specific change or failure requiring them.

### Historical gate investigations through source030

The following older observations explain previous decisions and failures; the
current source033 gates and access state above supersede their current-tense wording.

The completed bounded anchor CPU stress run was capped at 900s and 4GiB observed owned-family RSS,
with 16GiB available-RAM reserve. It included one ordinary managed real CPU game,
then four cooperative games and learner continuation. The independently supervised
45s, two-thread same-UID external workload shared the selected two physical CPU
cores and permitted SMT siblings. Reviewed evidence includes managed yield and
confirmed release, external CPU acquisition without manager registry ownership or
signals, and a fresh accepted attempt under the same experiment/task identity.
See [stress/optional-soak procedures](soak-harness.md) and
[comparison protocol](comparison.md) for manifests, bounds and cleanup. The native
application stress and synthetic CPU pressure comparison passed. The separate
three-pair real Kaggriculture CPU throughput comparison also passed, with its own
input/output and cleanup evidence. Original-segment overhead coverage remains
inconclusive; no GPU performance or physical two-node claim follows.

burst still requires a qualified home-only state filesystem. The user delegated
conservative bounded test limits and real-server operation in022, so an absent
budget/window approval is no longer a current blocker. Every actual run still
requires recorded resource, duration, abort and cleanup bounds. A successful
bounded fsync/readback probe does not
establish mergerfs crash durability. Read-only audit012 found no qualified local
filesystem under home, and the user cannot obtain an administrator-approved
alternative. Do not use underlying `/mnt` storage. anchor managed CUDA admission
remains blocked by unknown desktop GPU activity; the user cannot log out the
session. Delegated test authority does not permit treating unknown activity as
spare capacity or weakening that policy. The approved Mac Docker alternative passed its local Linux
runtime and real CPU application cycle. anchor+Docker016 failed TCP startup without
launching work. The user's subsequent direct-server diagnostic used the existing
SSH sessions: peer SSH was reachable, but burst's actual SDK request to anchor's
API timed out. Tailscale019 subsequently passed enrollment and bidirectional peer
pings through the existing sessions. The physical authenticated API/artifact route passed021 after live filter review;
no useful two-node workload or agent recovery was exercised. The earlier failures remain
retained without attributing them to a particular network component. The unused
optional Mac tunnel is separate; no additional tmux session is required.
These Docker results cannot satisfy the physical burst or NVIDIA GPU gates.

The current burst boundary permits the explicitly requested home-only Tailscale
installation; other software installations and all outside-home writes remain
unauthorized. Existing Python/stdlib and NVML diagnostics are complete; no
shared-server agent, application load or CUDA experiment is validated. Its private
Tailscale daemon is a transport service, not a workload-validation result. A
reachable transport does not qualify home storage or establish GPU activity.
Read-only022 preflight reconfirmed anchor ext4 and burst home fuse.mergerfs with no
qualified home submount; the Mac tailnet map still contains neither intended server.
The latter blocks the Docker alternative before application routing and does not
invalidate the independent burst-to-anchor API021 pass. These observations used
aggregate GPU data only; no shared-server per-process GPU query was performed.

No delegated cgroup subtree is authorized. Rootless validation proceeds
independently; ResourceManager cgroup weight/quota/memory application remains
unverified. Docker's observed outer container quotas are not backend application
evidence. Weights
are relative hierarchical controls, GPU budgets are admission policies, and no
zero-interference or arbitrary external GPU-OOM prevention is claimed.

Fresh027 diagnostics narrowed the storage explanation. The running mergerfs
version is2.33.3, and a64KiB shared-mmap/flush/readback probe passed in0.004804s;
mmap incompatibility or an assumed cache-off setting is not the blocker. Exact
upstream2.33.3 returns ENOSYS for directory fsync, which the examined Linux6.8
FUSE path can acknowledge without backing-directory synchronization. No verified
vendor patch establishes the required parent-directory publication durability
for the unmodified home path. This is a source-based contract assessment, not observed
corruption. Home storage remains unqualified under the unchanged contract.
Runtime evidence and assessment (private evidence retained outside this source review).

A separate8ms home-only diagnostic found an optional synchronous-directory
attribute can be applied to a fresh owned folder and inherited by its child.
Both flags were restored after the probe. This is a possible storage refinement,
not qualification: the actual mergerfs directory already has two backing copies,
and safe branch/mapping stability, synchronization and WAL behavior across the
whole publication/recovery path have not been validated by this attribute probe.
The generic FUSE gate remains unchanged; no qualified production path is declared.
Attribute probe (private evidence retained outside this source review).

The same read-only027 diagnostics found nonempty external context inventories
on all five GPUs and no fresh process-activity sample in three queries per device
(return code6). No GPU load was started and no per-process identifiers/metrics
were exported. Prior own-process025/026 successes do not change this unresolved
external activity. The Mac alternative still lacks its intended server peers.
Anchor GPU predicates (private evidence retained outside this source review),
burst GPU predicates (private evidence retained outside this source review),
Mac route status (private evidence retained outside this source review).

## Completion and measurement rules

Report implementation, Linux-native rootless, optional controls, physical two-node
application, fault/recovery, bounded stress and optional-soak statuses separately.
Uncertain allocations remain reserved until verified reconciliation. Never signal
unrelated processes; failure injection affects only owned test processes and
connections. Use a home-only isolated run ID and preserve original datasets,
checkpoints and failed evidence. Public source publication under CedeGrid/Apache-2.0
is deferred until the required testing gate is satisfied. Privileged changes, outside-home writes and
persistent autostart remain unauthorized; publication is separate from runtime
readiness and must have its own completed evidence.

The preregistered goals remain: zero ownership/accounting/result violations;
matched managed throughput>=90%; manager RSS<=512MiB/node; idle/active manager
CPU<=0.1/0.5 logical CPU; decision p95<=1s; release p95<=10s after decision;
protected post-yield p99<=1.20x baseline; anchor accepted progress<=120s with
backlog; coordinator recovery<=60s and node rejoin<=90s. Report detection, drain,
termination and release separately. Insufficient samples are inconclusive; higher
CPU utilization alone is not speedup. Exact source/environment/model/seed and
worker counts, time bounds, scope, raw measurements and cleanup belong in every
manifest. Direct-process monitor peaks do not establish whole-family overhead.

One consolidated pass verifies each frozen core. After a real failure or a scoped
change, rerun only affected checks and then append actual results. Optional soak
waiver does not waive bounded stress, two-node acceptance or other outstanding
authorization gates, and creates no future-monitoring or endurance claim.

The original `kaggriculture-pairs010` work-deadline failure is retained in its
raw report (private evidence retained outside this source review).
The subsequently authorized `kaggriculture-pairs011` continuation reused its three
completed cases and ran only the missing three, within 1800s total active execution.
The full matched-throughput/integrity/cleanup review passed. The original segment's
four manager-accounting gaps remain inconclusive; no retrospective coverage pass
is claimed. Existing passed stress, pressure and matched trials are not rerun.

The final read-only native check also verified three continuous system PSI samples
in 1.0391s: a baseline followed by fresh 505ms and 506ms CPU/memory/IO intervals.
Scope, timestamps and availability remained explicit; system CPU full was omitted.
Reviewed readings (private evidence retained outside this source review)
do not establish workload attribution or performance benefit. Through026 the 24
production core/SDK files were byte-identical to source008; that is historical
equivalence, superseded by source027's policy/model/producer correction above.
CedeGrid023 renamed only the root package in Cargo.lock without changing the 241
dependency entries. Historical package checks remain scoped to their snapshots;
the old026 source candidate must not be published as the current implementation.


## Targeted completion verification020

The reviewed020 evidence (private evidence retained outside this source review)
closes two tooling defects and direct public SDK artifact coverage. The fixed
command transport remains a deployment helper; core/SDK source and Cargo.lock
matched tested source008 at020. The later023 root package rename is documented
above. No passed stress/comparison suite was repeated.

On anchor,14 proxy tests and6 bootstrap-diagnostic tests passed, followed by the
real Python Client upload/download fixture against the unchanged production
coordinator. Total native runtime was5.4038s. Checks cover interrupted upload
resume, receipt replay, byte-exact and empty downloads, checksum failures,
immutable destinations and failed temporary-file cleanup. The protocol fixture's
one offer was never prepared or executed and was released; there were zero
execution and uncertain-allocation rows. This is native API evidence, not a new
workload or two-node application result. Mac focused proxy/bootstrap compatibility
checks and the expanded Rust mTLS integration also passed.

The proxy now closes active clients before waiting for server shutdown, fixing a
Python3.12 deadlock, and reaps its bounded fixed-command helpers on exit/failure.
Bootstrap errors no longer include command/payload text and oversized commands
are rejected before tmux submission. An unused run-specific test PKI was rotated
after a transfer diagnostic; no host credential was changed.

Historical physical API020 failed (later resolved for API scope by021). A foreground tailnet TCP Serve endpoint was
configured only on the private home daemon, with ShieldsUp left enabled. The
actual burst SDK TLS handshake timed out after4s while the anchor coordinator and
Serve process were independently confirmed alive. The earlier attempt overlapped
the server's160s deadline and is retained without a root-cause claim. The second
attempt removes that ambiguity. All owned test processes and helper children are
absent/reaped, both Serve configurations are empty, and both intentional home
Tailscale daemons remain Running with ShieldsUp enabled.

The installed version's [primary netstack source](https://github.com/tailscale/tailscale/blob/v1.102.3/wgengine/netstack/netstack.go)
forwards unmatched ordinary tailnet TCP ports to host loopback. Disabling
ShieldsUp without a restrictive policy could therefore expose unrelated services.
At the end of020 the tailnet policy was still needed for scoped ingress.
The later user clarification intentionally retained trusted personal-to-shared
access;021 verified the shared-server peer restriction below. No global firewall,
routing, SSH or system-service change occurred.
Storage, GPU observation and optional cgroup gates remain independent. Later022
delegated bounded test limits; absent budget/window approval is no longer a gate.
The 24-hour run remains waived; bounded real stress already passed.


## Physical API and private source review021

The actual burst SDK reached the unchanged anchor coordinator through the home-only
Tailscale transport. mTLS status, operator/node authorization, rejection of missing
certificates and wrong hostnames, resumable/replayed uploads, empty/byte-exact
artifacts, corruption rejection and an owned connection stop/reconnect passed.
Duplicate receipt replay covers the lost-ACK recovery path; no packet-level
commit-ACK loss was deliberately injected in021. The owned disconnect was separate.
Client runtime was **14.4589s**, server runtime **15.2833s**, with **181,793 bytes**
forwarded. Reconnect preserved coordinator state and artifact bytes. The single
protocol-only offer was never prepared/executed and was cancelled and Released;
zero workload results were accepted. This resolves physical API verification,
not useful two-node Kaggriculture work or coordinator/agent/worker recovery.
API review and retained evidence (private evidence retained outside this source review).

The user explicitly retained broad personal-to-shared access for trusted personal
devices. Live effective filters limited shared-server peer ingress to TCP45670.
The separate owned port45671 timed out while45670 remained usable, consistent with
the intended restriction without proving the timeout's exact cause. Both nodes
returned to ShieldsUp with empty Serve configurations. All3 recorded anchor and43
burst PIDs were absent; owned children/helpers reaped; SQLite integrity/foreign
keys passed with zero executions, uncertainty or uncommitted uploads. Two committed
replay receipts and blob hardlinks intentionally remain. The initial collector
incorrectly expected the upload directory to be empty; read-only review corrected
that assumption and verified the existing contract without a workload rerun or
production change. Historical020 failures remain failures.

The private source-review exporter additionally passed six boundary tests and
an offline Cargo file-list check. It includes106 exact source/SDK/test/tool/example/
CI files plus a manifest, verifies38 documentation links/anchors and Rust included
fixtures, and retains identical25 core/SDK/lock files. Only exported documentation
is curated; original private worklog and raw evidence are preserved. Application
assets, runtime/credentials and the private combined Docker recipe are excluded.
At021 no public remote, publication or license was selected; common-disclosure checks
are bounded hygiene, not an exhaustive security audit. The initial pre021 snapshot remains retained. The
final source-review record (private evidence retained outside this source review)
and per-file manifest (private evidence retained outside this source review)
correspond to the subsequent snapshot with current API/Mac-route status.
Export scope and earlier checks (private evidence retained outside this source review).

Independent two-host application validation remains pending. A fresh Mac route021
check found neither intended Linux peer in its live Tailscale map; one bounded ping
failed "no matching peer" in0.039656s, before application routing. This does not
supersede the burst-to-anchor API021 pass or establish a ShieldsUp cause. Both owned
Docker containers remain stopped with their prior bounds and volume retained. No
workload or configuration was changed. Mac route gate (private evidence retained outside this source review).
The022 preflight reconfirmed missing Mac server peers and unsupported burst home
state storage. GPU activity/protection and optional cgroup gates remain;022 user
delegation removed missing bounded-test authorization as a blocker. CedeGrid023
public source preparation and licensing do not satisfy these runtime gates.
The 24-hour soak remains waived, not completed; passed bounded CPU stress remains valid.

## Native operational correction and final gate024

The original022 manual trial remains a failure before launch. Its two-logical-CPU
affinity, one-physical-core reserve and measured external overhead left only
913–921 millicores for a 1,000-millicore task. No assignment executed and no
reservation remained held. Its private test incorrectly classified an empty
execution ledger as uncertainty; the separate reconciliation review confirmed
zero executions and held reservations. Only that private assertion was corrected.
The 024 trial restored the previously approved two-physical-core affinity including
permitted SMT siblings (`0,1,8,9`), with the same one-worker limit, 4GiB family-RSS
bound and reserve policy. No core defect was inferred and no protection threshold
was lowered. The new configuration and separate successful024 evidence do not
rewrite the failed022 report.
Original failure (private evidence retained outside this source review),
reconciliation review (private evidence retained outside this source review).

The 024 reviewed run took 237.1977s including its operational checks; the real
four-game/checkpoint cycle occupied 161.5765s. Peak observed family RSS was
1,973,473,280 bytes within the 4GiB bound. Coordinator failure used a newly verified
pidfd and SIGTERM; the guaranteed test task continued during a five-second outage,
and manual restart against the same database accepted its completion. This
establishes the observed coordinator-loss contract, not enforcement after a
supervisor's own death. All eight tasks completed, all eight allocations released,
both databases had integrity `ok` and zero foreign-key errors, and the saved direct
process identities from022/024 were absent. Scope and raw outputs remain in the
native review (private evidence retained outside this source review).

The remaining required operational gates are distinct useful work from two
physical hosts with burst loss/rejoin, and actual managed GPU admission/protection
measurements. Unsupported burst home storage, missing Mac server peers and unknown
external GPU activity still prevent those claims. Optional cgroup application
requires delegation and does not block rootless release. The later025/026
own-process activity results below do not close managed GPU protection. Publication remains held: no public repository
or push is authorized before the required testing gate is satisfied.
Completion024 status (private evidence retained outside this source review).

## Bounded own-process GPU activity025/026

These diagnostics used existing home environments and one independently tracked
foreground CUDA process outside the manager registry on each role. Each requested
20s low-duty work with at most two logical CPUs, a 30s total deadline, 512MiB observed
VRAM and 2GiB RSS limits, and 4GiB GPU/16GiB system-RAM and disk reserves. These
observation/abort guards are not kernel-enforced partitions. No durable agent
state was placed on the burst filesystem and no unrelated process metadata was
exported.

Burst025 completed in 22.7157s with 912 units, three positive fresh own-process GPU
activity samples, peak observed VRAM 295,698,432 bytes and RSS 619,552,768 bytes.
Anchor026 completed in 21.2600s with 953 units, one positive fresh sample, peak
VRAM 228,589,568 bytes and RSS 977,342,464 bytes. Its inner guards had no errors.
Both successful children exited 0 with no signals, were pidfd-reaped, were absent
from `/proc`, and had their own GPU release confirmed. These unmatched runs do
not measure throughput improvement or protected-workload slowdown.

Anchor025 remains **aborted** at 21.2917s. The CUDA event completed 953 units, but
the outer observer encountered missing `VmRSS` during exit. Its owned child was
reaped with exit 0 after TERM and GPU release was confirmed. The private026 helper
waits at most 100ms for that same owned child's normal exit; a still-live child with
unknown RSS still aborts. It never treats missing usage as zero. Only the failed
anchor diagnostic was repeated, with no manager-core or protection-policy change.
Retained anchor025 failure (private evidence retained outside this source review),
successful anchor026 raw report (private evidence retained outside this source review).

Positive own-process activity demonstrates observation of those bounded workloads;
it does not classify every external context or turn unknown observations into
free capacity. Managed GPU admission, yielding/protection measurements and useful
two-host application loss/rejoin remain pending. All diagnostic processes are
stopped; publication remains held and the 24-hour requirement remains waived.
Completion026 status (private evidence retained outside this source review),
readable final evidence report (private evidence retained outside this source review).

At handoff no validation controller, agent, workload, proxy or helper remains active.
Both owned Docker containers are stopped. Only the two intentional private home
Tailscale daemons remain; no unattended validation monitoring or autostart was configured.


Review029 retained the storage refusal after an independent audit of directory
attributes, future branch copies and SQLite namespace requirements. A fresh burst
GPU read again found occupied devices without fresh external activity; no managed
GPU run was admitted. The anchor SSH connection had closed, and bootstrap now
rejects a non-SSH pane before sending any payload while retaining its remote guard.
Nine focused bootstrap tests and actual pane-metadata checks passed. No Rust/SDK
runtime changed. Remaining gate assessment (private evidence retained outside this source review).
