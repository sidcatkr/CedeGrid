# Controlled rootless/cgroup comparison

Source distribution: linked private deployment evidence is withheld.
All reported failures, unsupported capabilities and pending gates remain applicable.


`tools/compare.py` is a Python-standard-library harness for synthetic, single-process
CPU workloads. Its default invocation prints a protocol and launches nothing:

```sh
python3 tools/compare.py
python3 tools/compare.py --output comparison-plan.json
```

Linux execution requires `--execute`, an existing operator-authorized rootless profile,
and an output file whose adjacent artifact directory can retain recovery state.
The harness does not authorize a deployment merely because a path or credential
is available. Use an isolated or explicitly approved time window and only these
synthetic workers; unrelated users' jobs are not experimental subjects.

## Profiles and preflight

The unmanaged/rootless comparison works without delegated controls. Optionally
prepare a cgroup profile with identical CPU, RAM, monitoring,
GPU, lifecycle and execution settings. The harness rejects differences in these
sections. The rootless profile must have `cgroup.enabled = false`; the cgroup profile
must already enable and explicitly authorize the selected controls. For example,
these are configuration fragments to merge into otherwise matching profiles:

```toml
# Both profiles: explicitly opted-in local experiment.
config_version = 1
node_mode = "opportunistic"
[execution]
enabled = true
prepare_timeout_ms = 10000
admission_timeout_ms = 60000
[lifecycle]
allocation_lease_ms = 600000
```

```toml
# Only the cgroup profile. Replace with an actually delegated, authorized subtree.
[cgroup]
enabled = true
delegated_root = "/sys/fs/cgroup/my-authorized-subtree"
authorized_controls = ["cgroup.procs", "cpu.weight"]
cpu_weight = 10
allow_kill = false
# Omitted cpu_max and memory limits remain disabled.
```

`cgroup.procs` authorizes only the new gate's self-join. The manager creates its own
leaf under this subtree and never enables controllers, moves existing processes,
changes ancestors, or invokes sudo. The CPU controller must already be enabled for
children. The example adds no CPU bandwidth cap or memory limit. Adding either
requires its own explicit whitelist entry and policy; memory limits affect RAM,
not GPU VRAM. Required cgroup weight must be read back successfully before execution;
fallback is disabled for comparison jobs.

The profile must already provide enough CPU/RAM policy budget for a request of
1,000 millicores and 64 MiB. Reserves are not lowered by the harness. It accepts a
guaranteed node profile for a guaranteed test allocation, or an opportunistic lease
long enough for warmup, measurement, preparation and escalation. It refuses a short
lease rather than renewing it or silently changing allocation class. For default
timings, the example ten-minute lease exceeds the required experiment interval.
This test-specific lease must be suitable for the authorized node/time window.
For example, a single effective CPU with any observed use may have less than 1,000
millicores available and reject admission. This is reported as blocked, not fixed
by inventing capacity or suppressing resource accounting.

The harness reads effective configuration through `cedegrid validate` and runs
`cedegrid doctor` on each supplied profile. Runtime profiles use TOML. Source profiles
remain unchanged. Each case gets a retained configuration copy with exactly these
recorded overrides:

- `state_dir` points to a new case directory on storage accepted by preflight.
- `gpu.scale_up_cooldown_ms` becomes `0` for this controlled CPU experiment.

## Execute an authorized experiment

Build the native Linux binary first, then run on that authorized Linux host:

```sh
python3 tools/compare.py \
  --binary ./target/release/cedegrid \
  --baseline-config ./rootless-benchmark.toml \
  --cgroup-config ./cgroup-benchmark.toml \
  --output ./results/comparison.json \
  --repetitions 3 --warmup 2 --measure 5 \
  --execute
```

Defaults are five repetitions, 30 seconds of warmup, 120 seconds of measurement,
and seed `1729`. Change them with `--repetitions`, `--warmup`, `--measure`, and
`--seed`. `--cpu` selects one CPU from the intersection of the harness's and both
profiles' reported effective CPU sets; otherwise the smallest common CPU is used.
An unavailable CPU-set report blocks the experiment.

Each repetition runs a protected-alone reference, an idle managed worker under
the unmanaged baseline, rootless supervision, and optional cgroup backend, then
both workers together under each supplied backend. Backend order alternates
deterministically across repetitions. Workers announce readiness, then share a
monotonic start time. Warmup observations are excluded from workload measurements.
Protected requests scheduled during warmup remain excluded even if queueing delays
their execution until the measurement interval.
The historical timing defaults remain visible in plan-only output. Execution now
checks its estimate against a maximum 1,800-second window; choose explicit shorter
matched runs, as above. The wall-time guard also runs between cases. This does not establish long-duration endurance; the user replaced the required
24-hour milestone with bounded stress.

Only the harness's own synthetic workers are pinned. The manager is not pinned.
Managed workers use the profile's niceness under both backends, and protected
workers request nice `0`; an unavailable priority change fails rather than silently
weakening the comparison. Managed workers perform continuous fixed-size arithmetic
batches. The protected worker schedules one request every 20 ms, measuring latency
from the scheduled request time, including scheduler delay. Tail percentiles describe
completed requests within the measurement window; unfinished requests reduce
throughput and have no invented latency sample.

## Measurements and interpretation

The report and per-case artifacts retain:

- Managed arithmetic batches/second; protected completed requests/second, p95/p99
  latency, and slowdown relative to that repetition's protected-alone reference.
- Manager-only sampled cumulative CPU time and peak RSS, with raw `/proc` samples.
  Sampling starts after worker readiness and includes warmup. It excludes worker
  CPU, which is recorded separately, and may miss short terminal intervals or peaks.
- System and worker-cgroup PSI, cgroup throttling/accounting before and after the
  measurement, scope-preserving counter deltas, and missing/reset-counter status.
- Exact commands, effective configurations and overrides, `doctor` output, workload
  CPU affinity/niceness/cgroup membership, and visible hierarchy/ancestor restrictions.
- Supervisor drain, termination, exit and release timestamps when present, natural
  release-confirmation delay, cleanup status, and metric variability across repeats.

`cpu.weight` is a **relative hierarchical weight**, not a percentage, hard CPU cap,
or guaranteed allocation. Its effect depends on which sibling groups compete and
their ancestor restrictions. Protected workers remain in the harness's inherited
cgroup; the managed cgroup worker joins its explicitly delegated leaf. Their exact
placements are recorded, but this does not guarantee sibling competition or a useful
isolation arrangement. Inspect these placements, effective CPU sets and visible
ancestor limits before interpreting the result. Namespace-hidden ancestors remain
unknown; the harness does not rearrange the hierarchy to manufacture a comparison.

Compare protected slowdown/tail latency alongside managed throughput and manager
overhead. A stronger sharing policy may intentionally reduce opportunistic
throughput. Higher CPU utilization alone establishes no speedup. System pressure
or pressure in a shared rootless cgroup is not attributed to an external workload.

The direct-supervisor comparison in `tools/compare.py` exercises lease escalation
without running the agent's continuous contention loop. Its contention-to-yield
decision latency is therefore `null`; drain-to-release latency is also `null`
unless an actual drain is observed. Natural release-confirmation delay is reported
separately. The connected `tools/pressure_comparison.py` runner described below
reads actual agent policy decisions and confirmed releases. Unavailable telemetry
and absent metrics remain missing, never invented as zero.

Interrupted or failed cases retain logs, configuration, job specification and the
SQLite state directory. Surviving/uncertain allocations require reconciliation;
artifacts are not deleted to make the next run appear clean. The harness terminates
only its direct child processes on error and does not claim that killing a supervisor
also kills its workload. A failed run is not evidence of verified cleanup.

## Verification status

Only pure protocol/analysis/guard tests have run on the current macOS development
host. No synthetic workload, Linux cgroup control, privileged operation or native
comparison has run here. Run the pure tests with:

```sh
python3 -m unittest discover -s tests -p test_compare.py -v
```

This harness does not complete pidfd/cgroup native fault validation, distributed
recovery, GPU sharing/release, or the physical two-server milestone. The 24-hour soak is optional after the user waiver.
The future eBPF/sched_ext research gate remains unchanged.


## Added completion comparisons

Omit `--cgroup-config` to compare equivalent unmanaged and rootless runs. The
unmanaged worker has the same arithmetic, seed, CPU affinity and niceness. Only
the managed cases run under CedeGrid. Runtime task IDs are generated UUIDs,
not artifact-directory names. Optional cgroup absence is reported as unverified,
not as a successful kernel-control check.

Targets are registered before execution: median idle managed/unmanaged throughput
ratio >=0.90; protected p99 during managed contention divided by its matched
protected-alone reference <=1.20; sampled
manager mean CPU <=0.5 cores; sampled peak RSS <=512MiB. Missing metrics remain
pending. These synthetic targets do not establish application speedup; real
Kaggriculture matched trials remain a separate required comparison.

The standalone CLI comparison measures the whole contention interval. It has no
post-yield segmentation, so the required operational post-yield p99 goal remains
pending even if this local comparison passes. An already-slow unmanaged contention
case is never the protected reference; a regression test rejects that false pass.

## Real agent policy and post-yield comparison

`tools/pressure_comparison.py` closes the standalone comparison's policy-path
gap. It creates a fresh private mTLS coordinator and agent through the existing
deployment helper, then runs three matched repetitions of protected-alone,
unmanaged idle, managed idle, unmanaged contention and managed contention. Its
synthetic producer is `tools/pressure_probe.py`; real Kaggriculture comparisons
remain a separate required measurement. These commands are for the already
authorized Linux anchor after native installation/testing, using its actual UUID:

```sh
cedegrid_validation_root="$HOME/.local/share/cedegrid-validation"
"$cedegrid_validation_root/venv-isolated/bin/python" \
  "$cedegrid_validation_root/source/CedeGrid/tools/pressure_comparison.py" \
  --root "$cedegrid_validation_root" \
  --output "$cedegrid_validation_root/evidence/pressure-cpu-UNIQUE-RUN-ID" \
  --mode cpu --gpu-uuid "$CEDEGRID_VALIDATION_GPU_UUID" --execute
```

For a separately authorized GPU trial, use a new output directory and `--mode gpu`
with the same exact UUID. GPU observation or admission failures remain failures;
this runner never ignores desktop processes or overrides unknown telemetry.
Omitting `--execute` only prints the envelope. There is no shared-server approval
flag: this command does not extend authorization to burst.

CPU trials use two permitted physical CPU IDs, reserve one core and run one
bounded half-core producer. Protected CPU requests run on the other selected
core, with the same100,000-iteration request every20ms in alone and contention
cases. Agent usage and capacity therefore share the same two-CPU scope. GPU
trials use three physical CPUs and one exact GPU; protected GPU requests use the
same fixed matrix operation every20ms and128MiB allocation in each matched case.
The relative placement, private configuration, worker count and commands remain
in the run artifacts. These are controlled synthetic workloads, not simulator
optimizations or claims about unrelated research workloads.

The whole run has a1,200s deadline. Each producer lasts20s when idle or at most40s
during contention; each protected process lasts20s and is separately reaped.
Each workload has a2GiB observed RSS abort limit; GPU mode additionally observes
a1GiB per-process VRAM limit,4GiB device free-memory reserve,16GiB available system
RAM and16GiB free disk reserve. The output cap is2GiB. These are rootless
observation/abort policies, not kernel-enforced partitions. Artifacts, state,
logs, temporary paths and caches remain under home. No optional cgroup controller
is silently required or described as applied.

`pressure.py` records the actual CPU-loop/GPU-allocation start and each request's
arrival and completion on the local monotonic clock. The runner reads the agent's
durable policy decisions and records when the coordinator first confirms the
allocation Released. That observed release is a conservative post-yield cutoff;
requests arriving before it stay excluded even if they finish later. The1s
decision and10s release targets retain their original values. Post-yield p99
must be at most1.20 times protected-alone p99, with at least100 post-yield requests
in each of three repetitions. Missing yield, insufficient samples, clock mismatch
or missing decision evidence is inconclusive. Whole-contention unmanaged results
remain visible and never replace the protected-alone reference.

The same-UID protected process must stay outside the managed identity registry,
finish normally and be reaped through the runner's live child handle. GPU release
is independently observed after reaping. `STOP` in the run directory or SIGINT/
SIGTERM cancels only this run's jobs, drains its private node, reaps owned children
and services and retains every uncertain allocation. A supervisor death does not
imply workload release. The runner records manager process/final-supervisor usage
using the existing validated sampling helpers. Native results and final metric
review remain required; local analysis/ownership tests are not Linux enforcement
or performance evidence.

## CPU-only real application comparison

The same `tools/anchor_validation.py` now accepts `--stage command --device cpu`,
with no GPU UUID or GPU reservation. After the native stress produces a verified
snapshot, use the active verified source/binary under the private anchor root:

```sh
cedegrid_validation_root="$HOME/.local/share/cedegrid-validation"
"$cedegrid_validation_root/venv-isolated/bin/python" \
  "$cedegrid_validation_root/source/CedeGrid/tools/anchor_validation.py" \
  --root "$cedegrid_validation_root" --run-id UNIQUE-MATCHED-RUN \
  --node-id YOUR-CONFIGURED-VALIDATION-NODE \
  --candidate "$cedegrid_validation_root/runs/YOUR-STRESS-RUN/snapshots/cycle-00000/main.py" \
  --binary "$cedegrid_validation_root/source/CedeGrid/target/release/cedegrid" \
  --stage command --device cpu --execute
```

It runs three matched pairs of twelve real games with one actor at a time, identical
model/seed/league/inference/thread/niceness settings and at most six permitted
physical CPUs. Output and private lifecycle paths intentionally differ. The total
900s includes a120s cleanup allowance;4GiB observed owned-family RSS and16GiB
available RAM are abort bounds. Touch the run's `STOP` file for protocol drain and
verified direct-service cleanup. Retain uncertain allocations and all failed
outputs. The >=0.90 throughput and existing overhead goals remain unchanged.
This CPU path is independent of the blocked CUDA admission; neither a CPU
pass nor compilation establishes GPU sharing or physical two-node acceptance.

## Continue an interrupted comparison without repeating completed trials

`anchor_validation.py --resume-report` accepts an original CPU command comparison
that stopped at its work deadline. It verifies the explicit run ID, prior process
absence, completed tasks and Released assignments, source/input hashes, actual
replay checksums, CPU scope, controls, environment settings, and the original
case order. It preserves old reports and partial outputs. New outputs and services
use a fresh run ID; the original explicit seed IDs remain unchanged. Completed
trials are reused once; interrupted trials are excluded. It does not adopt
persisted processes or resume a chain of previously continued comparisons.

`--wall-seconds` sets the approved total active execution budget, including the
original run's elapsed time, between 900 and 1800 seconds. Cleanup retains its
120-second allowance. An extension needs operator authorization; the option
itself grants none. The original 12-game, three-pair targets and all CPU/RAM/GPU
bounds remain unchanged. Separate segments are identified in the report; gaps
between them are not counted as continuous execution or endurance evidence.
Each segment's overhead coverage is reported independently. Old missing samples
remain inconclusive, and installed packages at the same interpreter path are
assumed unchanged rather than cryptographically attested.

The actual `kaggriculture-pairs010` run completed three cases before its limit,
then released both managed allocations and reaped every directly owned process.
The user subsequently authorized the 1800-second total envelope. The completed
`kaggriculture-pairs011` continuation ran only the three missing cases, retaining
the original source, model, seeds and ordering. Do not execute these used run IDs
again or repeat the completed measurements.

The reviewed three-pair median managed/unmanaged throughput is **0.9868896563**,
above the original **0.90** target. Total active execution was **1479.0233s** across
two segments. All 72 real game outputs and replay hashes, 18 current input paths,
three completed tasks/Released assignments and absence of 16 owned identities
passed a separate read-only Linux audit. Pair 1 spans segments; matched inputs do
not exclude every possible background change. The unmanaged baseline retains an
idle coordinator/agent, and timing includes process startup/admission.

Continuation-only manager overhead passed with no missing samples: peak measured
CPU was 0.084034 cores and RSS 88,977,408 bytes. **The original segment's four gaps
remain inconclusive, so combined accounting coverage is not passed.** The separate
15-case CPU pressure/overhead comparison remains independent. No result is a
speedup, GPU-sharing, cgroup-enforcement or physical two-node claim.

Full reviewed record (private evidence retained outside this source review),
machine-readable metrics (private evidence retained outside this source review),
and integrity/cleanup audit (private evidence retained outside this source review)
retain exact commands, raw original reports, outputs, seeds, hashes and limitations.
No comparison workload remains active. Other authorized Docker/two-node validation
is tracked separately in the work log.
