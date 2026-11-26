# CedeGrid

CedeGrid source distribution. See [LICENSE](LICENSE), [release scope](docs/release.md) and [guarantees](docs/guarantees.md).


CedeGrid runs explicitly selected compute jobs on spare CPU, RAM, and NVIDIA GPU
capacity, most usefully on a shared server where other people's processes already run, so
that no capacity sits idle. Own work runs as fast as it can, and the contention mode says how
much it gives back when others need capacity: Performance, the default, starts work as plain
Linux would beside other users' work and is built to finish it sooner than plain Linux does;
Neutral and Yield First give back more. A standalone Rust coordinator and native node agents own scheduling,
leases, process supervision, checkpoints, artifacts, and result receipts. Python
and TypeScript SDKs submit work and provide cooperative worker hooks. Any executable
that meets the process contract can run; no application framework is required.

Version **0.2.0** introduced the clean `cedegrid` name and TOML configuration, and is the last
version on npm and PyPI. Later versions ship only as files on
[GitHub Releases](https://github.com/sidcatkr/CedeGrid/releases), each with its sha256 in
`SHA256SUMS`; see [moving off npm and PyPI](docs/migration-registries.md).
Historical measurements in [validation](docs/validation.md) are scoped to their
recorded versions and do not qualify newer binaries.

## Install

On Linux x86_64 or aarch64, install the command-line tool for your user, with no root, from the
release you want:

```sh
curl -fsSL https://github.com/sidcatkr/CedeGrid/releases/download/v<version>/install.sh | sh
```

Replace `<version>` with the release's version, as the release page shows it. The script
installs that version into `~/.local/bin/cedegrid`, checks the download against the release's
`SHA256SUMS` before installing it, and stops with nothing installed when the check fails. It
refuses to run as root and says when `~/.local/bin` is not on `PATH`. Then:

```sh
cedegrid run python3 train.py
```

Other ways to install the same version:

- **Homebrew**, on macOS or Linux: `brew install sidcatkr/cedegrid/cedegrid`. The formula
  installs the release tarball by its sha256; each release also carries it as `cedegrid.rb`.
- **Cargo:** `cargo install --locked cedegrid` builds it from source with Rust 1.88 or later and
  a C compiler.
- **Release files:** each release has a tarball per platform; check it against `SHA256SUMS`
  (`sha256sum --check --ignore-missing SHA256SUMS`) and put `cedegrid` on your `PATH`.

The Python and TypeScript SDKs are attached to each release as a wheel and an npm tarball, also
listed in `SHA256SUMS`. Install them from the downloaded files:

```sh
sha256sum --check --ignore-missing SHA256SUMS
python -m pip install ./cedegrid-<version>-py3-none-any.whl   # Python 3.10+; 3.10 uses tomli
npm install ./cedegrid-<version>.tgz                           # Node 22.14+
```

The SDKs need no CLI to be installed beside them, and the CLI needs neither Python nor Node.
A CLI or SDK already installed from npm or PyPI at 0.2.0 keeps working; see
[moving off npm and PyPI](docs/migration-registries.md). Source builds need Rust 1.88+ and a C
compiler:

```sh
cargo build --release --locked
./target/release/cedegrid --version
```

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

## Set up a node

On a terminal, `setup` opens a keyboard interface:

```sh
cedegrid --config cedegrid.toml setup
```

Up and Down move, Space selects, Enter confirms, and Esc, `q` or Ctrl-C cancel without
writing. Each option shows its consequence on this host. Setup starts from the current file
and edits only the settings it manages, keeping comments and other settings byte for byte.
It covers the role, the default mode and each resource's mode, the node cap, the RAM and GPU
memory floors, the margins of each mode, the Guaranteed hint deadline and two tolerances.
Without a terminal it never prompts, and the same choices come from flags:

```sh
cedegrid --config cedegrid.toml setup --non-interactive --json --role node \
  --default-mode neutral --mode gpu_memory=yield_first --ram-floor-mib 16384 \
  --margin cpu.neutral=7 --set gpu_memory.noise_tolerance_mib=128
```

The flags are `--role`, `--default-mode`, `--mode RESOURCE=MODE`, `--max-mode`,
`--ram-floor-mib`, `--gpu-floor-mib`, `--margin RESOURCE.MODE=PERCENT`,
`--guaranteed-hint-deadline-s`, `--set RESOURCE.KEY=VALUE` and `--acknowledge-caution`.
Client and coordinator files take `--endpoint`, `--listen`, `--state-dir`,
`--operator-fingerprint` and the TLS paths instead. The exit status is 0 when the file is
written or unchanged, 2 for a missing or invalid choice, 3 for an unacknowledged caution
option, 4 when the result does not load, and 130 when cancelled.

`[caution]` marks an option that lets own work hold more capacity within the floors: a floor
below twice its minimum, a margin below its default, and a detection setting beyond its
default. Performance is the default mode and no caution option. In the interface it needs a separate confirmation, and with flags
`--acknowledge-caution` naming the option. The choice is recorded in
`contention.acknowledge_caution`. See [guarantees](docs/guarantees.md) for what each mode,
floor and caution option does, and `cedegrid setup --help` for every flag.

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

| Target | Intended functionality | Runtime requirements |
|---|---|---|
| Linux GNU x86_64 / ARM64 | Coordinator, execution, CLI, SDKs | glibc 2.35, kernel 5.15 |
| macOS Intel / Apple Silicon | Coordinator, native execution, CLI, SDKs | macOS 14 |
| Windows x86_64 | Client CLI and SDKs | Windows 11 24H2 |
| NVIDIA GPU | Optional NVML observation and controlled execution | Compatible NVIDIA driver/NVML, fresh activity and memory evidence, and verified release of owned contexts |

This is the target matrix; consult the gate manifest for actual qualification.
GPU admission depends on detected capabilities and current activity. Specific
GPU models used in validation are recorded in the evidence rather than defining
minimum hardware requirements. The Windows native npm package is currently
unavailable because npm rejected its name through spam detection; the SDK and
standalone release files remain separate distribution options.
Windows execution, agents, recovery, and storage mutation are refused before
initialization. macOS uses a verified direct-child fallback. Linux pidfd, delegated
cgroup v2 controls, CPU affinity, and PSI require runtime capability evidence.

Durable coordinator storage requires a qualified local filesystem. An explicitly
selected replayable burst-agent profile can use weaker storage with authenticated
recovery and retained uncertain reservations. `doctor` never treats that as strict
durability. GPU sharing is best effort and does not guarantee an external allocation
or performance. On a time-sliced GPU, CedeGrid relies on the driver's time-slicing. In Yield
First and Neutral, while own jobs hold more processes than their share of the busy ones and
another user wants its slices, whole own tasks are paused down to that share; a single job is
never paused for it, and no duty cycle runs. Own work that was on the device before another user
keeps at least half of it. This rests on the driver giving busy contexts equal slices, and MPS
and MIG devices keep the measured path. Hostile-code isolation, arbitrary descendants, hard VRAM
partitioning, and unqualified optional controls are outside the support claim. See
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

On Linux a rootless run is its command and every process it forks that stays in its family.
The supervisor is the run's child subreaper, and a forked descendant whose parent chain,
liveness and user and group IDs check out joins the run's verified family. The family is
measured as own work (CPU summed over its members, memory by their summed proportional set
size, GPU per process) and stepped together: a stop reaches every member, those forked during
it included. PyTorch's `DataLoader` with `num_workers` above 0 and Python `multiprocessing`
work this way. A descendant that fails a check, for example after running a setuid program,
or that is past the bound of 256 members, is reported as `single_process_violated` with its
count and reason and is never acted on. Elsewhere a run is a single process
(`single_process: true`) or has children created through the Python or TypeScript SDK's
`spawn_managed` with a configured `managed_child_limit`; forks there are outside the
contract and reported the same way. Every mediated child has its own verified handle and
durable launch barrier; a leader handle is not containment.

A run ends when its leader exits. Members still alive then get SIGTERM, then SIGKILL after
`term_grace_ms`, in both classes, and the outcome counts them as `members_ended`. So a job
script that starts a server in the background (`server & ...`) ends it with the script, and
a script that starts work in the background must end with `wait`, or it exits at once and
its work is ended with it. A process meant to outlive its run is started as its own run. A
drain signals the leader first, so the workload can stop its own workers; members still
alive after the grace get SIGKILL. Processes outside the family outlive the run, and in Full
they keep its cgroup leaf nonempty. In Tracked, a helper that starts and exits between two
discoveries is not counted as own CPU. Under the subreaper an orphan's `getppid()` is the
supervisor, not 1.
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
reconciliation, without adopting or signaling a persisted numeric PID. Its parent-death
signal (SIGKILL for Opportunistic work, SIGCONT for Guaranteed work) reaches only the leader
and SDK children. In Full the run's other members stay in its leaf, kept for
reconciliation. In Tracked the agent recovers them from the family it recorded: as soon as
it sees the supervisor gone, or when it next starts, it re-opens each recorded process by
its identity and continues Guaranteed members or ends Opportunistic ones. A process whose
start time or IDs no longer match the record is never signalled. A member the supervisor
found and stopped in the instant it died can be missed and stays stopped until an agent
runs. The workload runs in a session of its own, so a supervisor's death never hangs it up,
and `supervise` at a terminal drains its run on Ctrl-C.

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

Any fixed loopback transport command can serve as the target. Keep any private
socket or credential it needs in an owner-only directory under your home, and
admit inbound application traffic only behind a reviewed, restrictive access
policy. No installation or policy change is implied by this example.

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
