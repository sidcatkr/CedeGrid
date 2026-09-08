# Upgrade to CedeGrid 0.2

Version 0.2 is a clean public rename. Replace `resmgr` and `resmgr-sdk` with
`cedegrid`, the Rust crate `resource_manager` with `cedegrid`, and `RESMGR_`
environment variables with `CEDEGRID_`. No compatibility aliases are installed.
The new defaults are `cedegrid.toml` and `.cedegrid-state`.

## Convert configuration

Runtime configuration is TOML 1.0 with `config_version = 1`. Job specifications,
RPC requests, results, checkpoints, and external-tool files remain JSON or their
external tool's required format. The converter is the only runtime component that
reads old YAML/JSON configuration.

```sh
cedegrid config migrate --kind node --input /absolute/old/node.yaml --output /absolute/new/node.toml
cedegrid config migrate --kind coordinator --input /absolute/old/coordinator.json --output /absolute/new/coordinator.toml
cedegrid config migrate --kind agent --input /absolute/old/agent.json --output /absolute/new/agent.toml
cedegrid config migrate --kind client --input /absolute/old/client.json --output /absolute/new/client.toml --legacy-client-semantics rust
```

Node and service conversion use the legacy Rust effective settings. For a legacy
Python client, select `--legacy-client-semantics python --legacy-cwd /absolute/old/cwd`.
This explicitly resolves the old implementations' differing TLS path semantics.
An ambiguous legacy client file is refused instead of guessing its path base.

The converter canonicalizes the input file, resolves old path targets and default
values, writes their explicit effective values, and re-parses the TOML to check
equivalence. This includes a previously omitted old state directory; conversion
must not start a new empty ledger. A not-yet-created state path is allowed.
The output uses exclusive no-clobber publication and refuses an existing file,
including a competing converter's output. Review its reported effective settings.

The new client default is a 15-second end-to-end request deadline and 10 MiB/s
aggregate serialized-wire pacing with 10% headroom. Migration records older
effective values explicitly, including the Rust client's unlimited pacing as zero.
CPU weight omission uses the existing default; `{ mode = "off" }` disables it;
`{ mode = "set", value = 10 }` supplies an explicit value.

## Upgrade the offline ledger

Drain and stop the old coordinator, every agent, its supervisors, and supported
workload writers. Mixed-version rolling upgrades are unsupported. A stopped agent
alone does not establish that all writers have stopped. The confirmation flag is an
operator assertion; native locks and persisted live identities add refusal checks.

Point the converted node configuration at the existing state location and run:

```sh
cedegrid --config /absolute/new/node.toml state upgrade --backup /absolute/private/upgrade-bundle --confirm-legacy-stopped
```

For coordinator state, use a node configuration containing that coordinator's
exact `state_dir` and `storage_profile`. Do not launch a node agent with this file.
The command makes a new immutable, private upgrade bundle before mutation. It
includes the database, sidecars, outboxes, receipts, and relevant local files; it
does not replace your separate credential backup. Destination reuse requires an
exact matching interrupted upgrade and is checked before proceeding.

Audited legacy WAL schema 2 and DELETE schema 3 state upgrade to schema 5;
distributed schema becomes 3. Replay schema 4 preserves local bytes but requires
authenticated coordinator reconstruction before renewed authority. Unsupported
old or future schemas are refused without changing the original state.

The upgrade preserves node identities and historical hashes and receipt bytes.
Nonterminal legacy work is held for explicit retry/reconciliation. Valid legacy
result descriptors and receipts are indexed without rewriting their hashed bytes;
unresolved outboxes remain retained. A clean rename never performs string
replacement inside durable records.

## Recovery and rollback

Every database connection and native output writer participates in the stable
parent namespace lock. Replay recovery first fences the old session and then
waits for exclusive lifecycle access before quarantine. A delayed supervisor must
verify its namespace/session before opening outputs. Failure to establish
quiescence retains uncertain reservations and does not promote a replacement.

Do not run 0.1 on state modified by 0.2. To roll back, stop all 0.2 processes and
restore the complete verified offline upgrade bundle into a separate private
location using the old version's documented procedure. Never mix files from the
old bundle with the upgraded directory. Consult the [release contract](release-0.2-spec.md)
and [storage guarantees](guarantees.md) before resuming retained work.

Within 0.2.x, the project intends compatible documented CLI/SDK behavior and
versioned storage/wire readers. “Stable” means the declared qualification gates
passed, not that SemVer 0.y grants an automatic compatibility promise across minors.
