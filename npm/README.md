# CedeGrid

TypeScript SDK and native CLI launcher for CedeGrid 0.2. Node.js 22.14 or later is required.

```sh
npm install cedegrid
npx cedegrid --help
```

The SDK imports without optional native packages. CLI execution requires the matching
exact-version native package. Linux uses glibc; supported native targets are Linux
x64/arm64, macOS x64/arm64, and Windows x64 client commands. Installation does not
run downloads, compilation, or code generation.

```ts
import { Client, commandTask, stringifyJson } from 'cedegrid';
const client = Client.fromConfig('/absolute/path/operator.toml');
const task = commandTask('counter', ['python3', '/work/counter.py'], '/work', {
  cpuMillicores: 1000n, ramMib: 512n, singleProcess: true, noEscape: true,
});
await client.submit('job', 'pool', [task]);
for await (const task of client.iterStatus('tasks', { jobId: 'job' })) {
  console.log(stringifyJson(task));
}
```

Client files are strict TOML 1.0:

```toml
config_version = 1
endpoint = "https://coordinator.example:9443"
timeout_seconds = 15
max_transfer_bytes_per_second = 10485760
[tls]
ca_cert = "tls/ca.pem"
certificate = "tls/operator.pem"
private_key = "tls/operator.key"
```

TLS paths are relative to the canonical configuration file. Requests verify the
server hostname, require mutual TLS, ignore proxy environment variables, and refuse
redirects. Timeout covers pacing, DNS, TLS, and complete response decoding. Explicit
pacing rate 0 disables application-byte pacing; the default applies 10% framing
headroom to a 10 MiB/s aggregate budget. Pagination captures a finite high-water
bound and never silently restarts after an expired cursor.

`parseJson` and `stringifyJson` preserve JSON integer tokens as `bigint`. Protocol
integer inputs accept `bigint` or safe integer `number`. Metadata `number` values
are finite binary64 floats and serialize with a decimal point or exponent, including
`1.0` and `-0.0`; metadata `bigint` values range from -2^63 to 2^64-1. Overflow and
nonzero decimal underflow are rejected. Use these codecs when storing SDK replies;
ordinary `JSON.stringify` cannot serialize `bigint`.

```ts
import { WorkerContext, DrainRequested } from 'cedegrid';
const worker = WorkerContext.fromEnv();
try {
  worker.safePoint();
  const artifact = await worker.artifact('output', '/work/output.bin');
  const handle = await worker.complete({ processed: 100n }, [artifact]);
  console.log(handle.publication_id);
} catch (error) {
  if (!(error instanceof DrainRequested)) throw error;
  await worker.checkpoint({ cursor: 100n });
}
```

Worker methods use the authenticated native supervisor socket in the calling
process. Native components own artifact files, quotas, checkpoint sequences and
durable publication. `checkpoint` and `complete` return local publication handles;
the coordinator may accept them later. `PublicationUncertain` retains the original
publication ID, attempt identity, and candidate digest. Query
`worker.publicationStatus(id)` to reconcile that exact operation. The SDK never
falls back to writing an authoritative spool. `spawnManaged` requires explicit
single-process/no-escape acknowledgement and preserves a request ID after a lost
spawn acknowledgement.

Errors provide stable `ERR_CEDEGRID_*` codes and causes. `isCedeGridError` supports
structural checks across module/package boundaries. ESM and CommonJS imports share
the same implementation and constructor identity.

Operator `client.download(artifact, destination)` uses strict POSIX file and
directory synchronization and preserves conflicting existing files. A failure
after publication raises `DownloadUncertain` with the destination, operation ID
and artifact digest. Windows callers must explicitly pass
`{ durability: 'portable' }`, which flushes the file and publishes atomically
without a directory durability promise; strict mode rejects before changing the
filesystem on Windows.

License: Apache-2.0. See LICENSE, NOTICE, and THIRD_PARTY_NOTICES.md.
