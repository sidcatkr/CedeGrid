# CedeGrid Python SDK

The CedeGrid 0.2 operator and cooperative worker SDK requires Python 3.10 or later.
Python 3.10 uses `tomli`; Python 3.11 and later use the standard TOML parser. This
pure Python package contains the SDK. The native `cedegrid` CLI ships separately.

```sh
python -m pip install cedegrid
```

```python
from cedegrid import Client, command_task
client = Client.from_config('/absolute/path/operator.toml')
task = command_task('counter', ['python3', '/work/counter.py'], '/work',
                    single_process=True, no_escape=True)
client.submit('job', 'pool', [task])
for task in client.iter_status('tasks', job_id='job'):
    print(task)
```

Runtime client configuration is TOML 1.0 with `config_version = 1`, `endpoint`, and
`[tls]` keys `ca_cert`, `certificate`, and `private_key`. TLS paths resolve from the
canonical config file. Defaults are `timeout_seconds = 15` and
`max_transfer_bytes_per_second = 10485760`; explicit rate 0 disables pacing.
Requests verify the hostname, use mutual TLS, ignore proxy environment variables,
refuse redirects, and enforce an end-to-end monotonic deadline including pacing.

`parse_json` and `stringify_json` preserve exact integer tokens from -2^63 through
2^64-1 and finite binary64 floats, including negative zero. Integer and float kinds
remain distinct. Nonfinite values, overflowing integers/floats, and nonzero decimal
underflow fail. Protocol integer fields reject Boolean and floating substitutes.

`WorkerContext.from_env()` reads a private version 2 lifecycle descriptor. Worker
artifact and publication calls use the authenticated native supervisor socket in
the worker process. Native components own quotas, artifacts and durable records.
`checkpoint()` and `complete()` return local publication handles; they do not wait
for coordinator acceptance. `PublicationUncertain` retains the original publication
ID, attempt identity and candidate digest. Reconcile with
`worker.publication_status(id)`; never substitute a new ID after an uncertain commit.
The SDK does not write an authoritative spool if the endpoint is unavailable.

Workers call `safe_point()` at bounded safe boundaries, handle `DrainRequested` by
checkpointing, and exit. Explicit `spawn_managed` children require the supported
single-process/no-escape contract. `SpawnUncertain` preserves the original request
ID. Errors expose stable `ERR_CEDEGRID_*` codes and causes; `is_cedegrid_error` permits
structural checks.

Operator `client.download(artifact, destination)` uses strict POSIX file and
directory synchronization and preserves conflicting existing files. A failure
after publication raises `DownloadUncertain` with the destination, operation ID
and artifact digest. Windows callers must explicitly pass `durability="portable"`,
which flushes the file and publishes atomically without a directory durability
promise; strict mode rejects before changing the filesystem on Windows.

License: [Apache-2.0](LICENSE). Attribution: [NOTICE](NOTICE).
