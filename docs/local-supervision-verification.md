# Local-supervision verification record

Historical record of the earlier local-supervision increment. The counts and
cross-target caveats below are preserved as evidence of those checks. Subsequent
coordinator, agent, artifacts, SDK and live accounting implementations are recorded
in [current validation](validation.md), [execution](execution-work.md), and
[coordinator operations](coordinator-work.md); the old missing-feature list below
does not describe the current checkout.

Date: 2026-09-05. Development host: macOS/Darwin arm64. Rust 1.98.1;
minimum supported Rust checked at 1.88.0. Python 3.9.6. Existing uncommitted
milestone-one work was preserved; this repository remains unpublished.

## Executed checks

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --locked` | 129 passed on macOS |
| `cargo +1.88.0 check --locked --all-targets` | Passed, native Rust/C type/build check |
| Linux x86_64 all-target Clippy | Passed Rust cross-target analysis; no Linux linking/runtime claim |
| Windows x86_64 all-target Clippy | Passed Rust cross-target analysis; no Windows linking/runtime claim |
| `python3 -m unittest discover -s tests -p test_compare.py` | 10 passed; pure metrics/protocol/guard tests |
| Example `validate`, `doctor`, two `observe --no-state` samples, synthetic replay | Passed; JSON output checked |
| Default comparison invocation | Produced plan-only output; no workloads launched |

Cross-target analysis used installed Rust targets and this explicit override:

```sh
LIBSQLITE3_SYS_USE_PKG_CONFIG=1 PKG_CONFIG_ALLOW_CROSS=1 \
  cargo clippy --locked --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
LIBSQLITE3_SYS_USE_PKG_CONFIG=1 PKG_CONFIG_ALLOW_CROSS=1 \
  cargo clippy --locked --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

The override bypasses bundled SQLite target-C compilation for analysis. It does
not validate target SQLite linking, target system libraries, syscall behavior,
permissions, delegation or native execution. Native checks use bundled SQLite and
the existing runtime version gate. Linux/macOS native CI remains configured; CI
execution is not claimed by these local results.

## Fault coverage

The new tests preserve the original policy/durability suite and add scoped CPU/PSI
freshness, unavailable/denied controls, readback mismatch, partial cgroup writes,
required-control fallback rejection, durable barrier ordering, immutable boot/start/
assignment/generation/backend identity, atomic capacity reservation, restart-held
uncertainty, and v1 database migration preserving existing tasks/observations.

Owned-process tests exercise preparation failures, malformed/unresponsive gates,
cooperative drain, TERM/KILL escalation, release-confirmation failure and actual
supervisor death at Prepared and Authorized-before-EXEC. Unrelated same-user
processes remain untouched. CLI tests verify disabled execution, successful local
receipts and safe retry only after confirmed cleanup. Linux-only pidfd and cgroup
gate tests are type-checked, not run on macOS.

A final parallel run exposed a real collision in clock-based temporary drain
names. The fix adds an atomic sequence and exclusive creation retries; deterministic
same-clock concurrent allocation and existing-directory/symlink preservation tests
now pass. Final verification was rerun after this fix.

## Not tested or not implemented

- Native Linux pidfd signaling/reaping and actual kernel permission failures.
- Real delegated cgroup controls/ancestor interactions and the opt-in ignored
  cgroup integration test; no authorized Linux test subtree was used here.
- Native rootless/cgroup performance comparison; no speedup claim.
- Nonempty cgroup cleanup/cgroup.kill: deliberately unavailable pending complete
  ownership/containment support; uncertain reservations remain charged.
- Continuous contention-driven execution and per-allocation live usage collection.
- Independent agent/coordinator processes, authenticated renewal, two-node CPU
  recovery, restart reconciliation, artifacts and Python workload SDK.
- GPU execution/release validation and the authorized 24-hour server soak.

These remain explicit delivery/acceptance gates. No server deployment, sudo,
kernel/global-system change, BPF program, sched_ext scheduler or unrelated workload
experiment occurred during this increment. See [plan delta](kernel-assisted-delta.md)
and [comparison protocol](comparison.md) for the next supported validation steps.
