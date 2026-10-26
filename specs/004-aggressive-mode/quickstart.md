# Quickstart: validating Aggressive mode

Run after 004 is implemented on its branch. Setup as in `CLAUDE.md` (`TMPDIR` under `$HOME`,
`CEDEGRID_TEST_PYTHON`, `CEDEGRID_SERVICE_TEST_MONITOR_MS=500`).

## 1. Policy (any OS)

```sh
cargo test --locked --test contention aggressive
cargo test --locked --test policy aggressive
```

**Expected:** every invariant in [contracts/policy-protection.md](contracts/policy-protection.md)
passes, including report 018's case (Aggressive 7640 → … → 7000 m, then 7000 m held) and the
ordering property with Aggressive included.

## 2. Existing modes unchanged (any OS)

```sh
CEDEGRID_DECISION_SNAPSHOT=<dir-from-001> cargo test --locked --test decision_snapshot -- --ignored --exact decision_snapshot
```

**Expected:** with the `aggressive` entries filtered out, every decision is byte-identical to the
snapshot taken on 001 after item 020 (FR-020, P11); the random scenarios also check P1.

## 3. Configuration and refusals (any OS)

```sh
cargo test --locked --test configuration aggressive
cargo test --locked --test cli unprotected
```

**Expected:**
- `aggressive` without `aggressive_mode` fails to load; with it, loads; `max_mode` stops at
  `performance` unless set to `aggressive` explicitly, and a job asking for Aggressive then runs
  at Performance with `mode_capped`.
- Every refusal path in [contracts/cli-sdk-wire.md](contracts/cli-sdk-wire.md) refuses with its
  message and starts nothing unprotected (SC-005).

## 4. SDK and wire parity (any OS)

```sh
PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_*.py'
(cd npm && npm test)
cargo test --locked --test coordinator aggressive
```

**Expected:** the contract cases (JC-05 and the unprotected cases) pass in Rust, Python and
TypeScript; the CLI/SDK parity test passes (FR-019).

## 5. Labels and markers (a terminal; `NO_COLOR` too)

Start a protected and an unprotected run, then `cedegrid ps`, `cedegrid top`, `cedegrid doctor`,
`cedegrid status` and the JSON forms.

**Expected:** `[Aggressive]` in red on both runs, `UNPROTECTED` in red on the unprotected run
only, one warning line at its start, and neither marker once the run ends (SC-006).

## 6. Native gates (authorized test host only, SC-008)

As in `specs/001-contention-modes/native-gates.md`, with the user's approval in the session:
- **Ramp and idle:** `tools/ramp_gate.py` in Performance and in Aggressive on the same scope:
  `ramp_seconds` and the idle capacity own work uses; Aggressive ≥ Performance at every sample
  (SC-003).
- **Contender slowdown:** a light contender (below its fair share) and a heavy one, with the
  protection on and off; record each contender's served CPU and slowdown against running alone.
  Protected: the light contender reaches its demand no later than under Performance (SC-001);
  the heavy one keeps its fair share (SC-002). Unprotected: zero compute step-downs (SC-004).
- **Both placements:** the contender in the same cgroup as own work and in a separate cgroup
  (another account or a system slice, which needs the user's setup).
- For every run: idle check first, `nohup` with a log, only processes the gate starts are acted
  on, cleanup at the end, and evidence redacted of host identifiers.
