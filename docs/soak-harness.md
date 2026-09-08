# Bounded operational validation harnesses

The latest user instruction replaces the required24-hour acceptance run with a
bounded stress test. Existing long-duration tools below are retained as optional
future tools; no24-hour completion, schedule or monitoring is claimed. The current
required test runs the real selected Kaggriculture workload under CedeGrid,
introduces a separately owned same-UID external CPU process, verifies that the
external process receives resources without being signaled by the manager, and
verifies the same experiment continues through a fresh accepted attempt and
checkpoint continuation. Shared-server load still needs its approved window.

`tools/local_smoke.py --stress --manager-root FROZEN_MANAGER_ROOT` implements this
test using the existing four-game real simulator/dataset/learning/export cycle.
Pass the normal explicit `--application`, `--dataset`, `--binary`, `--python`,
fresh `--output` and `--execute` arguments. `--manager-root` selects the frozen
SDK/helper tree while an isolated application copy supplies the small adapter
overlay; the Rust binary is copied and hashed inside the new output directory.
The optional application `actor_class: opportunistic` uses a distinct actor pool;
the anchor profile and learner remain guaranteed. Original adapter defaults and
original datasets/checkpoints remain unchanged.
Before stress injection, the same invocation runs one additional real CPU game
through the ordinary `training/self_play.py` command in its own guaranteed pool,
with one worker and automatic retry disabled. It verifies positive TorchScript
inference, simulator success, replay SHA256, accepted result and explicit release.
The four-game cooperative stress cycle follows within the same900s total bound.
This is generic command integration evidence, not a matched performance claim.

The Linux stress envelope is two physical cores with their permitted SMT siblings,
one real actor/thread, one reserved physical core and at most two external busy
threads in one independently supervised process for45s. Usage and capacity share
the selected CPU set. Total observed owned-family RSS remains bounded at4GiB,
available system RAM at16GiB, and whole-run time at900s. No CUDA work occurs.
Pressure begins only after a real registered actor is running. Passing requires
the actor's actual yielded outcome and explicit release, at least five seconds
of post-release external CPU samples averaging at least one logical CPU, normal
unsignaled external completion and verified registry exclusion, and a newer
accepted attempt of the same task in the same experiment. The same four-game
cohort then constructs the real dataset and continues the learner from step2 to4.
Missing evidence is a failed test, never a substituted synthetic success.

Creating the run's `STOP` file or signaling its living foreground harness stops
this test. Its live child handle cleans the separately owned external process;
its authenticated client cancels only this run's jobs, drains its private node
and waits for explicit releases before reaping owned services. Uncertain
allocations remain retained. The external process also has its own finite timer.
No numeric-PID adoption, name-based signaling, privileged control or host changes
are used. The native execution result remains separate from focused tool tests.

These tools default to plan-only output and require explicit execution approval
in their home-local configurations. Their existence is not shared-server load or
24-hour-window authorization. Current Mac tests verify contracts and owned child
cleanup; Linux demand and actual24h behavior remain native validation gates.

- `tools/soak.py` supervises a foreground anchor run against a private validation
  deployment. It tracks actual accepted experiment results and their nodes,
  handles declared drain/rejoin and direct-child start/stop events, checks anchor
  telemetry freshness, resource headroom and progress gaps (at most120s for
  accepted anchor progress with backlogged work, separate from idle rate limiting), and writes durable
  observations. Event dispatch is separate from verified effect.
- `tools/pressure.py` executes one finite same-UID external demand process. CPU
  demand uses at most two explicitly allowed CPU IDs and lasts at most60s. GPU
  demand uses one exact UUID, lasts at most30s, allocates at most512MiB of tensors,
  and observes its total process VRAM against a1GiB abort cap. RSS cap is2GiB.
  Global free-memory guards remain configurable. GPU memory limits are observation
  policies, not partitions. No other process is signaled or modified.
- `tools/pressure_schedule.py` owns separately reaped finite pressure children.
  GPU contexts therefore exit between events; tensor deletion alone is not
  described as complete VRAM release. Read-only NVML checks confirm the reaped
  test PID no longer appears before another event. Unknown readings abort. The
  node-local schedule has its own STOP file and live direct-child cleanup.
- `tools/connection_proxy.py` forwards opaque bytes between explicit loopback
  ports, with bounded connections and lifetime. TLS and client authentication
  remain at the coordinator. Only the burst validation transport is routed
  through this optional owned proxy; the anchor agent uses its direct endpoint.
  Stopping/restarting the proxy disrupts owned connections without changing host
  networking, SSH configuration, firewall rules, or unrelated processes.
- `tools/make_soak_config.py` generates the actual application loop, anchor harness,
  burst pressure schedule, and proxy configurations from supplied deployment
  paths. All execution/window approval flags are initially false.

Run generation on the anchor using actual deployed paths:

```sh
python tools/make_soak_config.py \
  --application-config TWO_NODE_APPLICATION.json \
  --coordinator-config PRIVATE_COORDINATOR.json \
  --bootstrap READ_ONLY_CHECKPOINT.pt --output NEW_HOME_DIRECTORY \
  --burst-output ACTUAL_BURST_HOME_OUTPUT \
  --burst-cpu-ids APPROVED_CPU_ID_1 APPROVED_CPU_ID_2 \
  --proxy-listen-port OWNED_LOOPBACK_PORT --proxy-target-port COORDINATOR_PORT
```

The generator reads the actual retry limit to reserve worst-case execution starts.
Generated CPU/GPU timings require a permitted short calibration to overlap real
active work; missed overlaps cannot satisfy the protection criteria. CPU tests
must compare usage/capacity in the same configured CPU scope. Two busy threads on
a64-CPU host are not proof of a two-CPU managed-budget reduction. Record effective
CPU sets, topology, model/seed/task/attempt versions, pressure-node event clocks,
and actual active worker counts. Pressure under an inherited shared cgroup is
not attributable to an external workload without accounting evidence.

For protected workload comparison, `pressure.py` also has `protected_cpu` and
`protected_gpu` modes. Each schedules one fixed request every20ms and records
latency from arrival, completed units, p95/p99 and raw samples. Compare matched
alone/managed idle/managed contention trials with the same event definition.
No score is inferred from utilization alone. GPU telemetry uses runtime NVML
symbols and refuses missing/unsupported readings; Linux interfaces are not
claimed verified by the Mac ABI tests.
The connected single-node counterpart is `tools/pressure_comparison.py`; see
`docs/comparison.md` for actual private-deployment commands and its bounded
three-repetition protocol. It adds the previously missing link from real agent
policy decisions and release confirmation to post-yield protected latency.
Pressure request records now include monotonic arrival/completion timestamps,
so later review need not infer post-yield samples from aggregate percentiles.

Stop an active run by creating its output `STOP` file or signaling the live
foreground harness with SIGINT/SIGTERM. `soak.py` cancels only jobs under its
configured experiment prefix, drains only the two isolated validation node IDs,
reaps its own children, and waits a configured bounded interval for explicit
released allocations. A missing allocation in later status remains unresolved;
no stored numeric PID is adopted. Preserve state, uncertainty reports, descriptors
and logs for reconciliation. Supervisor death is distinct from agent failure;
no harness claims a dead supervisor can enforce its former deadlines.

`status.json` distinguishes measurement elapsed time from cleanup elapsed time.
A full86400-second measurement may set `actual_24_hour_soak:true`, but status stays
`elapsed_complete_review_required` and operational acceptance stays pending.
Review node-local pressure reports, useful anchor progress during burst removal,
rejoin/model ownership, attempt fencing, fault outcomes, matched throughput/tail
latency, resource-release observations and cleanup before accepting the soak.
A short trial, configured duration, proxy restart, or dispatched event is not a
completed operational validation.

Observation storage uses keyed deltas plus a full snapshot every300s by default,
so growing assignment history is not copied every2s. Read both current and legacy
records with `tools/soak.py`'s `read_observations(path)` helper. This changes evidence
encoding, not the accounting ledger: disappearance still never proves release.
Generated headroom guards retain16GiB RAM on the anchor and64GiB on the shared
node. Backlogged work requires accepted anchor progress within120s; the longer
idle allowance only accommodates the conservative application rate limit.
Cancelled job membership comes from `Status.jobs[].task_ids`, independently of
directory names or job/task naming conventions. Cancelled work no longer counts
towards requested-work backlog, while its uncertain allocations remain reserved
until explicit release. Missing membership in older status responses is handled
conservatively as possible backlog.
