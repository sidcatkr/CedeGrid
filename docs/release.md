# CedeGrid 0.1 release scope

CedeGrid implements the version-one coordinator, node-agent, policy, supervision,
durable-state and Python SDK contracts. Version 0.1.0 is an experimental source
release. GitHub source publication is authorized with the operational gaps below
explicitly retained; publication does not certify a deployment.
The command remains `resmgr`, the Rust library `resource_manager`, and the Python
distribution/import `resmgr-sdk`/`resmgr` to preserve existing integrations.

Build from a checked-out revision with Rust 1.88 or later and a C compiler:

```sh
cargo build --locked --release
python3 -m venv "$HOME/.venvs/cedegrid"
"$HOME/.venvs/cedegrid/bin/python" -m pip install ./python
./target/release/resmgr --config examples/node.yaml doctor
```

The SDK build backend requires setuptools 77.0.3 or later; runtime has no third-party
Python dependencies. Keep state, credentials and temporary files in private directories
on a supported local filesystem. Run observe-only before explicitly enabling execution.
The [coordinator operations](coordinator-work.md) give start, submission, drain,
resume, reconciliation and offline backup/restore commands. No autostart is installed.

Native Linux CPU supervision, local-agent recovery, real application CPU stress and
matched CPU comparisons have passed in the private validation environment. Physical
mTLS/API, bounded artifact transfer and owned-connection reconnect also passed.
Useful multi-host application continuity and actual GPU sharing/protection remain
required operational acceptance gates. Applied delegated cgroup controls remain
optional and unverified; their validation does not block a rootless release. Unknown
GPU activity never becomes idle evidence. The explicit `best_effort_occupied`
mode accepts activity uncertainty only for verified authorized external identities.
Strict profiles still refuse unqualified FUSE/network state. The explicit
`burst_replay_delete_extra` profile permits weaker local state for replay-safe
opportunistic agents, with coordinator authority, quarantine and retained uncertain
reservations. See [storage and recovery guarantees](guarantees.md#replayable-burst-storage).
Consult the [validation record](validation.md) for exact scopes and retained failures.

A standard source-security review of the pre-branding candidate found zero validated
issues in 59 of 107 inventoried files, including all Rust production modules, the SDK
and Python tools. Forty-eight test/documentation/metadata paths were not fully audited;
this is partial source coverage, not a security certification or deployment test.

The source is Apache-2.0 with its LICENSE and NOTICE. Locked upstream dependencies
retain their own licenses. Building a binary links additional third-party code;
redistributors must preserve applicable upstream license/notice texts, including
conjunctive notices in dependencies. This source distribution does not bundle NVIDIA driver
libraries or application datasets/models. PyPI and crates.io publication are separate
from GitHub source publication and are not performed by the source exporter.
