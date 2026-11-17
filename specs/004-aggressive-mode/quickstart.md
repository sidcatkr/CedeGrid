# Quickstart: validating Aggressive mode

Run after 004 is implemented. Setup as in `CLAUDE.md` (`TMPDIR` under `$HOME`,
`CEDEGRID_TEST_PYTHON`, `CEDEGRID_SERVICE_TEST_MONITOR_MS=500`).

## 1. Policy (any OS)

```sh
cargo test --locked --test contention aggressive
cargo test --locked --test policy aggressive
```

**Expected:** every invariant in [contracts/policy-protection.md](contracts/policy-protection.md)
passes, including the F2c scenario (own work submitted while another principal contends is
admitted up to `max(fair share, available)`, and the other principal keeps the lesser of its
demand and its fair share) and the ordering property with Aggressive included.

## 2. Existing modes unchanged (any OS)

```sh
CEDEGRID_DECISION_SNAPSHOT=<dir-from-001> cargo test --locked --test decision_snapshot -- --ignored --exact decision_snapshot
```

**Expected:** with the `aggressive` entries filtered out, every decision is byte-identical to
001's snapshot at the merge commit (FR-020, P9); the random scenarios also check P1 and P6.

## 3. Configuration, the allowance and refusals (any OS)

```sh
cargo test --locked --test configuration aggressive
cargo test --locked --test agent unprotected
```

**Expected:**
- `aggressive` without `aggressive_mode` fails to load; with it, it loads. `max_mode` stops at
  `performance` unless it is set to `aggressive` explicitly, and a job asking for Aggressive then
  runs at Performance with `mode_capped`.
- The allowance reads off with a missing, corrupt or other-schema state database, and after a
  restore. Non-interactive setup cannot turn it on but can turn it off.
- Every refusal in [contracts/cli-sdk-wire.md](contracts/cli-sdk-wire.md) refuses with its message
  and starts nothing unprotected (SC-005).

## 4. SDK and wire parity (any OS)

```sh
PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_*.py'
(cd npm && npm test)
cargo test --locked --test coordinator aggressive
```

**Expected:** the contract cases (JC-05 and the grant cases) pass in Rust, Python and TypeScript,
and the SDK parity test passes (FR-019).

## 5. System mode key (the system-mode container)

```sh
tools/docker-validation/system-mode/run.sh container::unprotected
```

**Expected:** with `modes.unprotected_aggressive` absent or false, a member's grant is refused in
one line naming the policy; with it true and the allowance on, the run starts unprotected.

## 6. Labels and markers (a terminal; `NO_COLOR` too)

Start a protected and an unprotected run through the SDK, then `cedegrid status`,
`cedegrid observe`, `cedegrid doctor` and their JSON forms.

**Expected:** `[Aggressive]` in red on both runs, `UNPROTECTED` in red on the unprotected run
only, one warning line in the log at its start, and neither marker once the run ends (SC-006).

## 7. Native gates (test host, scheduled through the coordinator, SC-008)

- **Fill and idle:** Performance and Aggressive on the same scope, idle host: time to fill and
  the idle capacity admitted; Aggressive ≥ Performance at every sample (SC-003).
- **F2c shape:** own work submitted while the second account saturates part of the scope, in
  Performance, protected Aggressive and unprotected Aggressive. Record own admitted CPU and the
  second account's rate and p99. Protected: the second account stays at or above the lesser of
  its demand and its fair share (SC-001). Unprotected: own work starts beside it (SC-004).
- **No running step:** in every run, zero compute caps, pauses or drains on own work (SC-002).
- **Both placements:** the second account in a separate cgroup (its own user slice) and, where
  the host allows, in the same cgroup as own work.
- For every run: an idle check first, `nohup` with a log, action only on processes the gate
  starts, cleanup at the end, and evidence redacted of host identifiers.
