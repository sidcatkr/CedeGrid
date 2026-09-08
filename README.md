# CedeGrid

CedeGrid source distribution. See [LICENSE](LICENSE), [release scope](docs/release.md) and [guarantees](docs/guarantees.md).


CedeGrid runs explicitly selected compute jobs on spare CPU, RAM, and NVIDIA GPU
capacity. A standalone Rust coordinator and native node agents own scheduling,
leases, process supervision, checkpoints, artifacts, and result receipts. Python
and TypeScript SDKs submit work and provide cooperative worker hooks. Any executable
that meets the process contract can run; no application framework is required.

Version **0.2.0** introduces the clean `cedegrid` name and TOML configuration.
The release is being qualified: [required gates](docs/release-0.2-gates.json) are
mandatory, and stable readiness remains false until every platform and both GPU
gates pass. Packages are prepared locally; registry uploads have not been performed.
Historical measurements in [validation](docs/validation.md) are scoped to their
recorded versions and do not qualify these new binaries.

## Install

Once 0.2.0 is published, install the CLI and TypeScript SDK with:

```sh
npm install --global cedegrid@0.2.0
# Or add the SDK and CLI to a project:
npm install cedegrid@0.2.0
```

Installation works with `--ignore-scripts`. Native binaries are supplied by exact
version platform packages, without an install-time download or compiler. The SDK
can also be imported when optional native packages are omitted. Node 22.14+ is
required by the npm package.

The Python distribution contains the SDK only:

```sh
python -m pip install cedegrid==0.2.0
```

Python 3.10+ is supported; Python 3.10 uses `tomli`. Standalone release binaries
require neither Python nor Node. Source builds need Rust 1.88+ and a C compiler:

```sh
cargo build --release --locked
./target/release/cedegrid --version
```

Unpublished candidate files can be installed directly from their wheel/tarball.
See [release preparation](docs/release.md) for checksums and the isolated npm
registry check, which uses the original tarball bytes.

## Build and use

```sh
cedegrid config-example --kind node > cedegrid.toml
cedegrid --config cedegrid.toml validate
cedegrid --config cedegrid.toml doctor
cedegrid --config cedegrid.toml observe --samples 2 --no-state
```

Observation is the default. Enable `execution.enabled = true` only for the work
and resource envelope you intend to manage. `doctor` reports strict durability
support separately from admission for the selected storage profile.

Node, coordinator, agent, and client configuration are TOML 1.0 with
`config_version = 1`. Generate each kind with `config-example --kind KIND` or use
[the node](examples/node.toml), [coordinator](examples/coordinator.toml),
[agent](examples/agent.toml), and [client](examples/client.toml) examples.
Relative paths resolve against the canonical configuration file directory.
Configuration performs no environment interpolation or shell expansion. Jobs,
RPC messages, worker context, checkpoint metadata, and results remain JSON.

## Select which work is managed

Pool, job, and task IDs select submitted work. They are not process-name filters.
CedeGrid supervises the verified processes it starts. Other processes, including
those owned by the same account, remain external demand; CedeGrid yields its own
work without adopting or signaling them. Repository names and installation paths
do not become durable node, task, or job identities.

```python
from cedegrid import Client, command_task

client = Client.from_config("/absolute/config/client.toml")
client.put_pool("selected-workers", ["worker-a"], max_workers=1,
                allocation_class="opportunistic")
task = command_task(
    "counter-001", ["/opt/work/counter", "1000000"], "/opt/work",
    cpu_millicores=1000, ram_mib=128, allocation_class="opportunistic",
    replay_safe=True, single_process=True, no_escape=True, max_attempts=3,
)
client.submit("selected-analysis", "selected-workers", [task])
for row in client.iter_status("tasks", job_id="selected-analysis"):
    print(row)
```

The command can be the [compiled C counter](examples/counter.c), a
[Python worker](python/examples/counter.py), or a
[TypeScript worker](npm/examples/counter.ts). Worker publication goes through the
native supervisor socket in the calling process. Plain commands need no SDK:
a successful exit produces a native result with bounded stdout/stderr artifacts.
See [Python](python/README.md), [TypeScript](npm/README.md), and the
[language-independent protocol](docs/worker-protocol.md).

## Distributed services and Python workers

Configure distinct mTLS identities and explicit client roles. The server verifies
client certificates and roles; clients verify its CA and hostname, reject redirects,
and ignore proxy environment settings. Client endpoints must be HTTPS origins.
The default request timeout is 15 seconds, including pacing and response decoding.
The default aggregate transfer rate is 10 MiB/s with framing headroom.

```sh
cedegrid coordinator --deployment /absolute/config/coordinator.toml
cedegrid --config /absolute/config/node.toml agent --deployment /absolute/config/agent.toml
cedegrid submit --deployment /absolute/config/client.toml --job /absolute/work/job.json
cedegrid status --deployment /absolute/config/client.toml --collection tasks --job-id selected-analysis --all
cedegrid cancel --deployment /absolute/config/client.toml --job-id selected-analysis
cedegrid drain --deployment /absolute/config/client.toml --node-id worker-a
cedegrid drain --deployment /absolute/config/client.toml --node-id worker-a --resume
```

`status --all` streams finite pages as NDJSON. Each traversal has an initial upper
key bound; record contents remain live. Restart expires cursors explicitly. Pages
contain 100 items by default, at most 1000 and 7 MiB including the response envelope.
Tasks, jobs, nodes, pools, allocations, and inventories are separate collections;
summary records contain counts instead of nested inventory arrays.

`tools/make_test_pki.py` creates isolated short-lived credentials for tests. Keep
keys private and transfer only each node's own identity and the public CA. SSH may
help deploy or tunnel the service, but is not part of job semantics.

## Portability and capability reporting

| Target | Declared 0.2 support | Minimum qualification baseline |
|---|---|---|
| Linux GNU x86_64 / ARM64 | Coordinator, execution, CLI, SDKs | glibc 2.35, kernel 5.15 |
| macOS Intel / Apple Silicon | Coordinator, native execution, CLI, SDKs | macOS 14 |
| Windows x86_64 | Client CLI and SDKs | Windows 11 24H2 |
| NVIDIA GPU | Optional NVML observation and controlled execution | RTX 5060 Ti and L4 gates required |

This is the target matrix; consult the gate manifest for actual qualification.
Windows execution, agents, recovery, and storage mutation are refused before
initialization. macOS uses a verified direct-child fallback. Linux pidfd, delegated
cgroup v2 controls, CPU affinity, and PSI require runtime capability evidence.

Durable coordinator storage requires a qualified local filesystem. An explicitly
selected replayable burst-agent profile can use weaker storage with authenticated
recovery and retained uncertain reservations. `doctor` never treats that as strict
durability. GPU sharing is best effort and does not guarantee an external allocation
or performance. Hostile-code isolation, arbitrary descendants, hard VRAM partitioning,
and unqualified optional controls are outside the support claim. See
[guarantees](docs/guarantees.md).

## Local execution boundary

Start with `doctor` and `observe`. For a controlled local CPU trial, copy
`examples/node.toml`, explicitly enable `execution.enabled`, set reserves appropriate
to the test host, and edit `examples/cpu-job.json` to name your own command and
absolute working directory. Then run:

```sh
cedegrid --config /path/to/test-node.toml supervise /path/to/job.json
cedegrid --config /path/to/test-node.toml executions
```

Rootless v1 supports a single-process command with `single_process: true` and
`no_escape: true`, or explicitly mediated children created through the Python or TypeScript SDK's
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

## Upgrading from 0.1

There are no old executable, import, or environment aliases. Drain and stop old
coordinators, agents, supervisors, and workload writers before changing binaries.
Use the [configuration and offline-state migration guide](docs/migration-0.2.md).
Keep the immutable upgrade bundle for rollback; running 0.1 against changed 0.2
state is unsupported. Explicit state paths, durable identities, historical receipt
bytes, and hashes are preserved.

## Verification

```sh
mkdir -p "$HOME/.cache/cedegrid-tests"
export TMPDIR="$HOME/.cache/cedegrid-tests"
export CEDEGRID_TEST_PYTHON="$(command -v python3)"
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_*.py'
cd npm
npm ci --ignore-scripts
npm run build
npm test
```

Use Python 3.10+ for process fixtures, including `tomli` on 3.10. Native process
identity checks require ordinary host process visibility. Test fixture state must
be on supported storage under the test account's home. Installed-artifact checks
run outside the checkout and do not import its SDK sources.

CedeGrid is [Apache-2.0](LICENSE). Preserve [NOTICE](NOTICE) and dependency license
texts. Read [release scope](docs/release.md), [contributing](CONTRIBUTING.md), and
[security reporting](SECURITY.md). Private deployment credentials, application
assets, and raw host evidence are excluded from public packages.
