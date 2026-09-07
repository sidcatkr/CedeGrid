# CedeGrid

CedeGrid source distribution. See [LICENSE](LICENSE), [release scope](docs/release.md) and [guarantees](docs/guarantees.md).


CedeGrid is a standalone resource manager for guaranteed and opportunistic compute. Rust owns
the core; the Python SDK provides authenticated submission and cooperative lifecycle hooks. Node roles and resource policy are
configuration, not assumptions about a particular project, server, transport, or GPU.

**Current implementation: authenticated coordinator/agent services, local supervision,
durable artifacts and results, elastic pools, and the Python SDK.** Observation
remains the default. Execution requires `execution.enabled: true` and an explicit
workload contract. The launch barrier verifies preparation and persists authority
before user code runs. Linux-native hardware, real distributed workload, and bounded
stress acceptance are separate from local tests; consult the [validation record](docs/validation.md)
for their actual status. Optional Linux controls require verified runtime capability.

Physical two-Linux-host mTLS, artifact transfer and connection reconnect have passed.
Useful two-node application work, GPU protection and applied optional cgroup controls
remain separate pending gates; see the [validation record](docs/validation.md).
`tools/pack_source_review.py` creates private reviews or public source candidates.
It preserves source and the chosen Apache-2.0 license while excluding private
runtime/evidence. It does not publish automatically.

CedeGrid retains the `resmgr` executable, `resource_manager` Rust library and
`resmgr` Python import for compatibility. Repository names and installation paths
never become node, task or experiment identifiers.

## Build and use

Requires Rust 1.88 or later and a C compiler for bundled SQLite. The lockfile pins
dependencies. NVIDIA driver libraries are optional and loaded at runtime.

```sh
cargo build --locked
cargo run --locked -- config-example
cargo run --locked -- --config examples/node.yaml validate
cargo run --locked -- --config examples/node.yaml doctor
cargo run --locked -- --config examples/node.yaml observe --samples 10 --no-state
cargo run --locked -- --config examples/node.yaml observe --samples 10
cargo run --locked -- --config examples/node.yaml history --limit 10
cargo run --locked -- --config examples/node.yaml replay examples/gpu-pressure.json
```

Commands print JSON, one record per observation. `observe --samples 0` continues
until Ctrl-C or SIGTERM. Without `--no-state`, observation and decision records are
committed together to the configured database. Other than that state and logs
redirected by the operator, observation does not alter the system.

Relative `state_dir` values resolve relative to the configuration file. No SSH
configuration, host address, root access, or online service is required. Rename or
move this repository freely; the configured node ID remains stable. Example node
IDs are placeholders and GPU identity comes from telemetry UUIDs.

The first CPU reading may be unknown until a measurement interval has elapsed.
The live collector does not adopt arbitrary processes. Stateful observation includes
retained local execution reservations; unknown surviving usage blocks expansion.
`--no-state` observes without loading the ledger. To inspect hypothetical worker
reductions with attributed usage, use `replay`.

## Select which work is managed

Submit only the work you want ResourceManager to control. You choose explicit pool,
job and task IDs, such as `selected-workers` and `selected-analysis`. These labels
identify submitted work; they are not process-name filters. Agents supervise the
verified processes launched for those assignments. Other processes, including
those owned by the same user, remain external demand: the manager observes their
resource impact and yields its own work without adopting or signaling them.

The Rust services and Python SDK are reusable for ordinary commands and other
applications that meet the [execution contract](#local-execution-boundary).
Kaggriculture uses a separate application adapter for simulator, replay and learner
boundaries; its models and training logic are not part of the generic core.

After the [coordinator and agent are running](#distributed-services-and-python-workers),
this existing SDK API submits the [ordinary counter example](python/examples/counter.py)
to one configured node. Set the worker paths to existing absolute paths on that
node, and replace `burst` with its configured node ID:

```python
from pathlib import Path
from resmgr import Client, command_task

client = Client.from_config(Path.home() / ".config/resmgr/operator.json")
worker_root = "/home/USER/ResourceManager"
worker_python = "/home/USER/.venvs/resmgr/bin/python"

client.put_pool("selected-workers", ["burst"], max_workers=1,
                allocation_class="opportunistic")
task = command_task(
    "selected-analysis-task-001",
    [worker_python, f"{worker_root}/python/examples/counter.py",
     "--steps", "20", "--delay", "0.1"],
    worker_root,
    env={"PYTHONPATH": f"{worker_root}/python"},
    cpu_millicores=1000, ram_mib=128,
    allocation_class="opportunistic", replay_safe=True,
    single_process=True, no_escape=True, max_attempts=3,
)
client.submit("selected-analysis", "selected-workers", [task])
print(client.status("selected-analysis"))
```

Use `client.cancel("selected-analysis")` to cancel that submitted job alone.
A node drain applies to all manager assignments on the selected node. The
[start/status/drain/resume/reconcile commands](#distributed-services-and-python-workers)
and [offline backup/recovery procedure](docs/coordinator-work.md#offline-coordinator-backup-and-restore)
use the same durable identities. Retries require confirmed release and the declared
replay contract; choose new job/task IDs for a separate run. Directory names never
become runtime IDs.

## What is implemented

- Strict YAML validation with effective settings and configurable timeouts.
- Deterministic CPU/RAM/per-GPU accounting, pending reservations, conservative
  external-activity handling, cooldowns, and hypothetical drain selection.
- Portable CPU/RAM telemetry and an optional NVIDIA NVML adapter. Missing sensors
  are explicit, not free resources. Configured reserves are policy, not kernel limits.
- Strict WAL/FULL or DELETE/EXTRA state, plus explicit replayable burst storage
  with authenticated recovery and retained uncertain reservations. Coordinator
  result authority always requires strict durable storage.
- Optional Linux capability diagnostics: pidfd, effective CPU sets/topology, visible
  cgroup hierarchy/limits, and scoped PSI with freshness and availability.
- Local CPU supervision with a durable launch barrier, generation-bound process
  identity, preferred Linux pidfds and an explicitly weaker direct-child fallback.
- Opt-in delegated cgroup v2 membership, CPU weight/bandwidth and memory settings,
  readback evidence, accounting, and confirmed-empty cleanup. No unrestricted kill.
- Retained uncertain allocations, reconciliation, fault tests, and a plan-by-default comparison harness.
- A single durable coordinator and authenticated node agents, priority/FIFO jobs,
  pending reservations, elastic pools, cancellation, scoped leases, and reconnect.
- Checksummed bounded artifact uploads, crash-safe publication, fenced checkpoint
  records, and idempotent final result receipts.
- A dependency-free Python SDK with private worker spools and drain/checkpoint/resume
  hooks. A separate Kaggriculture client demonstrates real simulator/learner integration.

GPU sharing is **best-effort opportunistic sharing**. It does not guarantee zero
interference, an external allocation's success, or consistent external performance.
Aggregate GPU utilization is a kernel-busy-time indicator, not a measure of spare
compute capacity. See [guarantees](docs/guarantees.md).

## Portability and capability reporting

| Component | Support boundary |
|---|---|
| Configuration, policy, protocol models | OS- and transport-independent Rust |
| CPU/RAM observation | Supported `sysinfo` platforms, capability dependent |
| GPU observation | NVIDIA through optional NVML; other vendor adapters are future work |
| Durable state preflight | Recognized local durable filesystems on Linux and macOS |
| Weaker local storage | Explicit replayable burst-agent profile; no local durability claim; [contract](docs/guarantees.md#replayable-burst-storage) |
| Other storage platforms/filesystems | Explicit refusal until a supported adapter exists |
| Local CPU execution | Linux implementation; macOS direct-child fallback tested; Windows unavailable |
| pidfd, cgroup v2, PSI | Optional Linux capabilities; actual pidfd runtime verified, delegated controls remain authorization-dependent; see [current evidence](docs/validation.md) |
| Network transport and Python SDK | Versioned mTLS RPC and credential-free cooperative workers; no SSH dependency |

Linux containers must mount a supported local volume for state when the container
root uses an unverified overlay filesystem. Network homes, RAM-backed volumes,
unknown filesystems, and database symlinks are not silently accepted. The explicit
`burst_replay_delete_extra` exception is described in the
[recovery contract](docs/guarantees.md#replayable-burst-storage). `--no-state`
allows observation without durable storage. Availability is separate from enforcement
in every capability record.

## Local execution boundary

Start with `doctor` and `observe`. For a controlled local CPU trial, copy
`examples/node.yaml`, explicitly enable `execution.enabled`, set reserves appropriate
to the test host, and edit `examples/cpu-job.json` to name your own command and
absolute working directory. Then run:

```sh
resmgr --config /path/to/test-node.yaml supervise /path/to/job.json
resmgr --config /path/to/test-node.yaml executions
```

Rootless v1 supports a single-process command with `single_process: true` and
`no_escape: true`, or explicitly mediated children created through the Python SDK's
`spawn_managed` with a configured `managed_child_limit`. Arbitrary forks, background
children and daemonization are outside the contract. Every mediated child has its
own verified handle and durable launch barrier; a leader handle is not containment.
Agent-managed stdout/stderr retain bounded 4 MiB prefixes per stream while excess
output is drained. Standalone `supervise` retains its original stream behavior.
GPU execution requires fresh NVML attribution and post-exit release confirmation;
missing or ambiguous telemetry refuses release. Hardware behavior remains a native
validation gate.

The standalone local opportunistic command has one configured lease and drains on
expiry. Agent-managed supervisors receive fenced authority updates and independently
enforce authorized deadlines. Guaranteed commands retain their configured continuity
contract on coordinator disconnection. A dead supervisor cannot enforce its own
deadlines: surviving or uncertain reservations remain charged until verified
reconciliation, without adopting or signaling a persisted numeric PID.

Cgroup settings require an existing explicitly delegated subtree and authorized
control names. Missing required controls fail before user execution. Nonempty cgroups
are retained for reconciliation; `cgroup.kill` is intentionally unavailable until
exclusive membership/descendant ownership is enforceable. See the precise
[implementation delta](docs/kernel-assisted-delta.md) and
[comparison protocol](docs/comparison.md).

## Optional bounded transport forwarding

`tools/connection_proxy.py` can forward opaque TLS bytes either to a fixed TCP
endpoint or through a trusted fixed `stream_command`. The coordinator still
verifies client certificates and roles; the SDK and agents verify its certificate
and hostname. The helper never terminates TLS. It is an optional Unix deployment
and fault-test tool, not an SSH or VPN dependency of the core.

For a command stream, configuration must provide `stream_command.argv` (an
absolute home executable and fixed arguments), `executable_sha256`, and
`single_process: true`. The command must not fork helpers or daemonize. The
`authenticated_target` block pins `coordinator_deployment`, `coordinator_sha256`,
`ca_certificate_sha256`, and `server_certificate_sha256`; a remote operator may
copy the public descriptor/certificates and adjust their local paths without
copying the server's private key. `target` names the descriptor's logical
coordinator listener. The operator must verify that the fixed command reaches
that endpoint; no client chooses a destination.

The command listener must be loopback-only, with explicit execution approval,
at most eight simultaneous connections, a positive byte budget <=2GiB, and a
lifetime <=1400 seconds. `stream_command.max_launches` additionally bounds helper
starts (default256, maximum4096). These are validation-tool ceilings, not worker
lifecycle deadlines. Configure a smaller envelope for the actual check.

```sh
python3 tools/connection_proxy.py --config /home/USER/validation/proxy.json
python3 tools/connection_proxy.py --config /home/USER/validation/proxy.json --execute
```

The first command reports the configuration without running it. The second
validates it and runs until its deadline, SIGINT/SIGTERM, or `output/STOP`. Review
`output/status.json`: all helper children must be reaped and
`remaining_owned_connections` must be zero. The launcher owns only its direct
children, uses pidfds on supported Linux, and uses an exclusive unreaped-child
fallback elsewhere. Do not adopt PIDs from an earlier report. Other local accounts
may connect to the fixed loopback endpoint, but must still authenticate with the
coordinator; this is not a general-purpose proxy.

For rootless Tailscale, the existing `tailscale --socket=PRIVATE_SOCKET nc PEER PORT`
is one possible command. Keep its private socket inside an owner-only home
directory. Do not disable ShieldsUp merely because peer pings succeed: userspace
networking can forward other tailnet ports to host loopback. A restrictive,
reviewed access policy is required before enabling inbound application traffic.
The deployment's actual verification status is recorded in
[validation](docs/validation.md); no installation or policy change is implied by
this example.

## Distributed services and Python workers

Private deployment files configure TLS identities, client certificate roles, node
resource ceilings, and state paths. The API always requires mTLS; do not expose a
plaintext or unauthenticated listener. Existing SSH access may bootstrap a deployment
or tunnel transport but is not part of job semantics.

```sh
resmgr coordinator --deployment /home/USER/.config/resmgr/coordinator.json
resmgr --config /home/USER/.config/resmgr/node.yaml agent --deployment /home/USER/.config/resmgr/agent.json
resmgr pool --deployment /home/USER/.config/resmgr/operator.json --spec /home/USER/validation/pool.json
resmgr submit --deployment /home/USER/.config/resmgr/operator.json --job /home/USER/validation/job.json
resmgr status --deployment /home/USER/.config/resmgr/operator.json
resmgr drain --deployment /home/USER/.config/resmgr/operator.json --node-id burst
resmgr drain --deployment /home/USER/.config/resmgr/operator.json --node-id burst --resume
resmgr resume --deployment /home/USER/.config/resmgr/operator.json --task-id TASK-ID
resmgr --config /home/USER/.config/resmgr/node.yaml reconcile
```

Resume requires proven release; replay-unsafe work additionally requires explicit
side-effect reconciliation. These commands do not authorize host installation or
shared-server load. The [SDK and adapter work log](docs/sdk-adapter-work.md) documents
the real RPC schema, worker examples, bounded application pipeline, and validation
commands. Use the coordinator and execution work records for process-level evidence.

`tools/make_test_pki.py --output /home/USER/validation/pki --node-id anchor --node-id burst`
generates separate short-lived identities and the exact certificate-fingerprint role
map for a private test deployment. Keep the CA private key on the orchestration/anchor
host; transfer only each node's own key/certificate and the public CA certificate.

## Verification

```sh
mkdir -p "$HOME/.cache/resmgr-tests"
export TMPDIR="$HOME/.cache/resmgr-tests"
export PYTHONDONTWRITEBYTECODE=1
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
PYTHONPATH="$PWD/python" python3 -m unittest discover -s tests -p 'test_*.py'
PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_*.py'
```

Use an existing Python 3.10+ environment. Test temporary files must be under your
home directory because the deployment fixtures enforce that storage boundary.

CI is configured for Linux and macOS, plus portable policy/telemetry checks on Windows.
Hardware GPU checks, bounded stress and real two-node runs are separate acceptance
gates, not implied by unit tests. The long-soak harness remains optional after the
user's duration waiver. See [validation](docs/validation.md) and the
[remaining implementation](docs/architecture.md).

The [local-supervision verification record](docs/local-supervision-verification.md)
records the earlier 129-test increment; it is historical evidence, not the current
implementation inventory. The initial [verification record](docs/milestone-1-verification.md) distinguishes
native tests from cross-target type checks and outstanding server validation.

CedeGrid is licensed under [Apache-2.0](LICENSE). See the [release scope](docs/release.md),
[contribution guide](CONTRIBUTING.md) and [security reporting policy](SECURITY.md).
Application code, personal deployment credentials and private raw evidence are
excluded from source distributions. GitHub publication does not imply that every
optional backend or deployment has passed operational acceptance.
