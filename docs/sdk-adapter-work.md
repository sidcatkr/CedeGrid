# Python SDK and Kaggriculture integration work log

The dated evidence below describes earlier development snapshots. CedeGrid 0.2.0
qualification is tracked separately in the [current release record](release-0.2-gates.json);
older runs do not qualify the new source or package bytes.

## Implemented interfaces

`python/cedegrid` is a dependency-free Python 3.10+ SDK. Install with
`python -m pip install /home/USER/CedeGrid/python` in the approved
isolated environment, or set `PYTHONPATH` to that `python` directory.

`Client.from_config(path)` reads `{endpoint, tls:{ca_cert,certificate,private_key}}`.
All requests use HTTPS mutual TLS and the actual version-one `/v1/rpc`
contract in `src/protocol.rs`. Hostname verification remains enabled; redirects
and environment proxies are disabled. The SDK provides `put_pool`, `submit`,
`status`, `cancel`, `drain_node`, `resume`, `result`, `upload`, and checksummed `download`.
`resume(task_id, side_effects_reconciled=False)` requires proven capacity release;
unsafe retry requires explicit side-effect reconciliation.
Submission IDs must be reused after an ambiguous acknowledgement. `command_task`
requires explicit `single_process=True, no_escape=True` acknowledgement for the
rootless contract and defaults to replay-unsafe. The application adapter makes
these declarations for its verified in-process actor/learner structure.

Workers receive no operator credentials. `WorkerContext.from_env()` reads the
agent's private `CEDEGRID_CONTEXT`, `CEDEGRID_OUTPUT_DIR`, and `CEDEGRID_DRAIN_FILE`.
`safe_point()` raises `DrainRequested`; `draining()` permits an application to
finish its own bounded safe operation; `on_drain()` runs callbacks once.
`artifact()`, `checkpoint()`, and `complete()` publish synced immutable blobs and
attempt-fenced descriptors. Result descriptors are immutable; checkpoints have
strictly increasing per-attempt `checkpoint_sequence`. The agent uploads blobs
and submits receipts. Publication is not equivalent to coordinator acceptance.

Resume context includes checkpoint metadata and verified downloaded artifacts
with absolute local `path`, `name`, `sha256`, and `size`. The original accepted
descriptor is never rewritten. Spooling defaults to 2 GiB, single artifacts to
256 MiB; source/descriptor integrity checks reject corruption and path escapes.

`python/examples/counter.py --steps 20 --delay 0.1` demonstrates ordinary
cooperative work, drain, checkpoint cursor, and completion without Kaggriculture.

## Separate application integration

The application code is in Kaggriculture's `integration/cedegrid`, not the generic
manager. Its example `examples/deployment.json` contains explicit per-node paths,
stable configured node IDs, class, worker ceiling, GPU UUID, immutable snapshot
hash, model hash, and opponent hash. Replace placeholders using verified private
deployment data. Run the experiment controller on the anchor: dataset and learner
paths are deliberately local to that host, not assumed shared across machines.

With `PYTHONPATH=<RM>/python:<Kaggriculture>`:

```sh
python -m integration.cedegrid prepare-dataset --source /home/USER/source-dataset --output /home/USER/validation/bootstrap-dataset
python training/train.py --dataset /home/USER/validation/bootstrap-dataset --output /home/USER/validation/bootstrap --workers 0 --device cuda:0 --batch-size 32 --epochs 2 --max-steps 100 --experiment-id RUN-ID
python training/export.py --checkpoint /home/USER/validation/bootstrap/latest.pt --output /home/USER/validation/model.ts --manifest /home/USER/validation/model.json
python training/snapshot_policy.py --output /home/USER/validation/snapshots --cycle 0 --source-root /home/USER/validation/source --torchscript /home/USER/validation/model.ts --model-manifest /home/USER/validation/model.json --fallback /home/USER/validation/source/weights/fallback.npz
python -m integration.cedegrid run --config /home/USER/validation/deployment.json
python -m integration.cedegrid cycle --config /home/USER/validation/deployment.json --bootstrap /home/USER/validation/bootstrap/latest.pt --steps 50 --device cuda:0
```

These commands require their approved execution envelope. They do not grant
shared-server authorization. Bootstrap is bounded preparation; `cycle` schedules
its two learner operations through CedeGrid, including checkpoint resume.
No command invokes the legacy unbounded application supervisor or writes its
shared `weights` directory.

`prepare-dataset` copies the smallest hash-verified train and validation shards,
never the locked split. For the inspected dataset this is 3,192 train samples
(725,614 bytes) and 1,660 validation samples (383,592 bytes). Its derived manifest
records the original manifest SHA256. Original shards remain untouched.

Each game has experiment/generation/logical-task/episode identity, paired seed
allocation, pinned model, seat, and immutable replay ownership. One manager
process calls the existing simulator directly without a nested process pool.
GPU actors verify a TorchScript backend, the selected UUID visibility, and
successful inference activity; silent model fallback cannot pass GPU validation.

The controller queues at most twice the sum of worker ceilings. Jobs contain one
game so timeout/cancellation can fence an individual logical result. An expired
opportunistic task is cancelled and its replay-safe logical game reissued on the
anchor with a new transport task ID. Only the current logical owner is ingested;
the old allocation remains charged until CedeGrid proves release. The
configured timeout is not evidence that a process stopped. Repeated or stale
transport results cannot create duplicate accepted replay inputs.

Completed replays feed the actual dataset builder, two bounded learner tasks
(learning then continuation), the actual exporter, and an immutable next-model
snapshot. Tiny cohorts lacking validation use a hash-verified existing validation
shard. A cohort with no train split fails explicitly instead of relabeling held-out
data. Models are pinned for a whole generation; no cross-node tiny inference calls,
DDP, simulator redesign, or algorithm optimization is included.

## Supervision and checkpoint semantics

`training.self_play.run_game` supports a callback at its entry and public simulator
step boundary, outside the engine's swallowed agent-exception path. Interrupted
games produce no accepted replay and restart from their seed. Cooperative drain
does not promise finishing a game within three seconds. Replay publication uses
file and directory synchronization and refuses overwriting an existing replay.
The old `_play` entry remains a compatibility wrapper.

`training.train.run_training` supports bounded `--max-steps`, safe optimizer-step
drain, and checkpoint publication callbacks. New v2 checkpoints include sampler
position, RNG state, DataLoader generator state, dataset/experiment/objective
identity and actual shard content hashes, model, optimizer, and scheduler. Exact continuation requires workers=0
and a matching dataset/objective/environment; reset-scheduler cannot claim exact
continuation. Legacy checkpoints keep their older epoch resume behavior.

## Validation evidence

- Existing self-play worker and sampler regressions: 14 passed on macOS.
- Full SDK plus existing/new Kaggriculture regression suite: 74 passed on macOS
  (53 existing application tests and 21 new SDK/integration tests), including immutable publication,
  interrupted sync, attempt isolation, actual optimizer tensor equality for
  uninterrupted versus interrupted/resumed execution, pre-first-step drain, and
  real installed simulator safe-step cancellation, logical failover deduplication,
  learner retry preserving its original optimizer-step budget, and CPU checkpointing
  that does not initialize unreserved CUDA contexts. Raw JUnit results and source/
  environment provenance are `.runtime/sdk-validation/python-results.xml` and
  `.runtime/sdk-validation/evidence.json`.
- The actual Python SDK completed status/result calls against the live Rust mTLS
  server in `tests/tls_transport.rs`; certificate/role rejection cases also passed.
- The real agent integration fixture also ran the SDK counter worker, consumed its
  checkpoint and result, and exercised immutable artifact transfer; see
  `docs/execution-work.md` for its native scope and latest results.
- Native Linux/full GPU/distributed application runs are recorded separately by
  the deployment validation work log. These local tests do not establish them.

Tests must set `TMPDIR` and pytest `--basetemp` inside the user's home. Example:

```sh
TMPDIR="$PWD/.runtime/tmp" PYTHONPATH="$PWD/python:/home/USER/Kaggriculture" python -m pytest python/tests /home/USER/Kaggriculture/tests/cedegrid --basetemp "$PWD/.runtime/tmp/sdk-tests"
```

## Remaining operational gates

Use current rootless capability evidence, exact native environment/model checks,
and the approved per-node budgets. Shared-server workload validation still requires
its explicit budget/window. The user replaced the required 24-hour acceptance run
with bounded stress; any future optional long soak requires separate authorization.
See [current validation](validation.md) for completed native/application evidence
and remaining gates. Nothing in this SDK changes storage qualification, process
ownership, SSH configuration, or privileged controls.

The execution agent subsequently passed the complete local mTLS SDK lifecycle:
coordinator -> actual agent and independent supervisors -> checkpoint/artifact
publication -> operator node drain -> cooperative exit -> confirmed release ->
retry generation 2 -> verified checkpoint download -> SDK resume -> accepted
`continued_from: 3` result. All four fixture execution records were released.
This is local macOS integration evidence (13.47 seconds), not Linux pidfd/GPU or
shared-server validation. See the actual fixture and `docs/execution-work.md`.

## Completion extensions: supervised children, model inputs, and bounded soak

The SDK now exposes `spawn_managed`, `ManagedChild.status/stop/wait`, and
`WorkerContext.spawn_managed`. The root assignment must explicitly declare
`single_process=False`, `no_escape=True`, and `managed_child_limit` in 1..8.
Children themselves must declare single-process/no-escape and cannot recursively
spawn. Handles are supervisor child IDs; numeric-PID signaling is not an SDK API.
A lost spawn acknowledgement raises `SpawnUncertain` with the request ID; retry
that same request ID. A wait timeout never signals. The root supervisor's death
still requires reconciliation; its child registry is not a promise of postmortem
deadline enforcement. See `python/cedegrid/process.py` and native execution evidence.

`Client` paces serialized request/reply bodies, including hex expansion, at a
configurable default 10MiB/s with 10% framing headroom. Set
`max_transfer_bytes_per_second` in the Python client configuration or constructor.
This is application traffic pacing, not kernel QoS or a claim about retransmitted
TCP bytes. Workloads retain no operator credentials.

Generic `LaunchRequest.input_artifacts` references are exposed through
`WorkerContext.inputs` after the agent verifies the private input downloads. The
Kaggriculture adapter alone packs policy snapshots, accepts only bounded regular
archive members, rejects links/traversal/duplicates, and verifies every file
against the pinned snapshot/model. Extracted models are private to an attempt and
removed on completion/drain; original snapshots remain untouched. Canonical replay
and learner artifacts stay in the SDK spool while redundant application copies
are removed. This keeps agent storage accounting bounded without discarding
uncertain manager allocations or authoritative checkpoint metadata.

The real application loop is now available:

```sh
python -m integration.cedegrid soak --config APPROVED_HOME_CONFIG.json \
  --bootstrap ORIGINAL_READ_ONLY_CHECKPOINT.pt --steps 50 --device cuda:0
```

It runs the existing finite cycle, exact within-cohort checkpoint continuation,
then the existing exporter. Complete next snapshots are atomically published,
uploaded through the accepted learner assignment, and named as inputs to later
node-local game tasks. One experiment ID persists; generations, task IDs, seeds,
and model versions are explicit. A new cohort warm-starts the last weights on its
new dataset; it does not claim exact optimizer continuation across changed data.
The two learner chunks within each cohort retain the exact resume contract.

Soak configuration must acknowledge its execution/window and the actual coordinator
retry limit. The default128 game-start/hour ceiling conservatively reserves
`2*(retry_limit+1)` starts per logical game for one burst-to-anchor replacement and
coordinator retries. With retry_limit3 this allows at most16 logical games/hour.
Each soak task now sets the durable `max_attempts=retry_limit+1` control, which
counts every assignment generation, including yield and preparation failure;
manual retry cannot bypass the cap. This makes the conservative start allowance
an admission bound rather than an assumption about error-only retries.
The generated12-game cohort reserves96 starts and waits after completion. Charges
remain for one hour after the cohort finishes. A second application replacement
requires reconciliation rather than silently exceeding the allowance. Actual
attempt counts must still be recorded separately. The controller has a maximum
24-hour deadline, positive idle wait, a20GiB local output/state cap, and explicit
stop/cancellation paths; disk-bound failure preserves existing data.

`tools/make_soak_config.py` derives unapproved24h files from actual two-node
application/coordinator configurations. It includes hourly CPU/GPU events,
burst drain/rejoin and owned loopback-connection stop/restart. Initial timings are
an execution envelope; allowed short validation must establish overlap with real
active games before the24h run. A dispatched event or idle interval is not proof
of contention protection or continued useful progress.

Expanded local regression evidence: **132 tests passed on macOS**, including the
original application tests, SDK/model input/soak contracts, rootless comparison
metrics, and validation-harness fault guards. JUnit:
`.runtime/sdk-validation/expanded-python-results.xml`. This does not establish
Linux/CUDA runtime, native cgroup control, two-node application success, or a24-hour
soak; those gates remain separately reported.

## Actual local CPU integration, 2026-09-05

The retained run `.runtime/local-cpu-smoke-20260905-e` completed four real
simulator games through the actual mTLS coordinator, agent, independent
supervisors, SDK and application adapter. Its existing dataset builder produced
the training input; two managed learner tasks continued from optimizer step2 to
step4 with the same v2 resume contract; the existing exporter and immutable
snapshot publisher produced the next model. All four accepted replays identify
the planned experiment, task, seed, model and CPU TorchScript inference.

The cycle took359.72s; setup and cleanup brought total elapsed time to369.03s,
inside the600s envelope. Peak observed owned-family RSS was1,158,479,872bytes,
below4GiB. Six tasks completed across16 attempts, with all16 allocations explicitly
Released and all owned services reaped. The pinned intermediate binary still
produced some near-ceiling CPU measurement yields; subsequent timing corrections
have separate regression evidence. These timings therefore establish connected
application functionality and retry recovery, not overhead or protection targets.

The original `report.json` remains marked failed because its final replay-hash
checker passed a string to a Path-only function after the cycle had succeeded.
That checker is fixed. `tools/review_local_smoke.py --output <retained-run>`
independently verifies the actual artifacts, planned replay ownership, checkpoint
contents, hashes and cleanup, writing a separate immutable `review.json`; it never
rewrites the failed run report or launches a workload. Review passed for this run.
This tool only opens trusted checkpoint files created by this validation run.

Earlier failed runs a–d remain retained, including interpreter selection, CPU
counter precision and obsolete runtime-observation writer failures. No failed
result was deleted or relabelled as a successful workload run. Mac integration
does not validate Linux pidfds/cgroups, CUDA, the two physical nodes, performance
targets or the actual24-hour soak.
