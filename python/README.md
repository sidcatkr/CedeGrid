# CedeGrid Python SDK

The Python client and cooperative worker interface for
[CedeGrid](https://github.com/sidcatkr/CedeGrid), a resource manager for selected
trusted workloads. Python 3.10 or later is required; the SDK has no third-party
runtime dependencies.

The distribution remains `resmgr-sdk` and the Python import remains `resmgr` for
compatibility. The manager CLI remains `resmgr`.

## Install from source

From the CedeGrid repository, use an existing isolated Python environment:

```sh
python -m pip install ./python
```

Installing the SDK does not start services or install node agents. Configure an
operator or node certificate with the deployment's matching role; all client API
requests require mutual TLS and verify the coordinator's hostname.

```python
from pathlib import Path
from resmgr import Client

client = Client.from_config(Path.home() / ".config/cedegrid/operator.json")
print(client.status())
```

The configuration and working coordinator must already exist. See the main
repository documentation for coordinator/agent setup, explicit execution opt-in,
submission, cancellation and recovery procedures.

## Cooperative workloads

`WorkerContext.from_env()` reads the agent-provided private lifecycle descriptor.
Workers call `safe_point()` at bounded safe boundaries and handle `DrainRequested`
by flushing results or checkpoints and exiting. Checkpoint publication and final
result acceptance use assignment identities and attempt generations. Automatic
retry requires an explicit replay-safe declaration; external side effects remain
the workload's responsibility.

Only submitted, verified managed work is controlled. Trusted commands must honor
the supported single-process/no-escape contract or use explicitly mediated
`spawn_managed` children. The SDK does not make arbitrary daemonizing descendants
safe, adopt processes by name, or grant permission to signal unrelated processes.
GPU budgets are admission policies, not hard VRAM partitions. Runtime capability
and validation limits are documented in the main repository.

Licensed under [Apache-2.0](LICENSE); see [NOTICE](NOTICE) for attribution.
