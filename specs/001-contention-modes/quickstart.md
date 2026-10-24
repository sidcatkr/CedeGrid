# Quickstart: validating contention modes

These are validation scenarios that prove the feature end to end. Commands assume the test
environment from `CLAUDE.md`:
- `TMPDIR` under `$HOME`
- `CEDEGRID_TEST_PYTHON` set to Python 3.10+
- `CEDEGRID_SERVICE_TEST_MONITOR_MS=500`

## 1. Policy regressions (any OS)

```sh
cargo test --locked --test policy -- d1_ d2_ d3_ d4_ d5_ d6_
```

- **Expected.** The six former `known_defect_*` tests, renamed `d1_` to `d6_`, assert corrected
  behavior and pass:
  - D1: no drain on first observation
  - D2: no protection event below 256 MiB
  - D3: quiet external context admits GPU work
  - D4: CPU contention yields by cap, never drain
  - D5: skew within tolerance keeps the budget
  - D6: one unknown allocation does not drain others
- **Also expected.** Contention-mode property tests (`tests/contention.rs`) show:
  - Yield First margin
  - Neutral convergence
  - Performance floors
  - no `pause` for Guaranteed work
  - envelope breach handled per resource

## 2. Single authority and error handling (Linux; macOS for the rootless parts)

```sh
cargo test --locked --test agent -- --test-threads=1
cargo test --locked --test supervision -- --test-threads=1
```

**Expected:**
- Supervisors run no policy engine.
- An `adjust` control applies only to owned processes.
- An injected telemetry or control error holds the step.
- Guaranteed work survives every injected observation error.
- After an agent restart, the new agent reconnects through `control.sock`, reads the applied
  steps, and does not rewrite the live supervisor's journal.

## 3. CPU-only contention, rootless (Linux and macOS)

1. Start a local agent with `[contention.cpu] mode = "yield_first"`, then run one CPU job.
2. Start a test-owned external CPU load.

**Expected:**
- Within the Yield First response time (default 3 intervals), `observe` shows step `cap`
  (duty cycle) or `lower_share` (cgroup).
- Own CPU falls within the margin.
- No drain or termination occurs (SC-002, SC-007).
- After the load stops, the steps reverse.

## 4. Delegated cgroup actuation (Linux, authorized only)

Run with `CEDEGRID_TEST_CGROUP_ROOT` set to an explicitly delegated subtree, as authorized:

```sh
cargo test --locked --test cgroup -- --ignored --exact runtime_adjust_requires_verified_membership
```

**Expected:**
- Runtime writes of `cpu.weight`, `cpu.max` and `cgroup.freeze` are read back.
- A write is refused when leaf membership differs from the verified PID set.

## 5. GPU on an occupied device (Linux + NVIDIA, authorized only)

1. With an external idle context on the device and `CEDEGRID_TEST_GPU_UUID` set, run the GPU
   pressure scenario in Neutral and in Performance.
2. Record native evidence in `docs/validation.md`.

**Expected:**
- The job is admitted on the occupied device and completes (SC-006).
- No external GPU allocation fails.
- Unattributed device memory below 256 MiB never triggers a step (SC-001).

## 6. Worker resource targets (both SDKs)

```sh
PYTHONPATH="$PWD/python" python3 -m unittest discover -s python/tests -p 'test_worker*.py'
(cd npm && npm test)
```

**Expected:**
- Targets are delivered to the leader and to managed children, and an ack is written.
- A compliant worker is not drained.
- A non-compliant worker is drained after the hint deadline.
- 0.2-style workers keep their drain behavior.

## 7. Setup (any OS for flags; a TTY for the interface)

```sh
cargo run --locked -- --config /tmp/x/node.toml setup --non-interactive --json --default-mode neutral --floor-policy c
cargo run --locked -- --config /tmp/x/node.toml setup --non-interactive --json --default-mode neutral --floor-policy c --acknowledge-danger floor_policy_c
```

**Expected:**
- The first command exits 3 with `ERR_CEDEGRID_DANGER_UNACKNOWLEDGED` and writes nothing.
- The second writes the file and reports the `floor_policy_c` exception as `danger`.
- With `NO_COLOR=1`, the interface's output contains no escape sequences but keeps the
  `[danger]` label.
- The interface and flags produce byte-identical files (SC-011).

## 8. Mixed versions (any OS)

```sh
cargo test --locked --test coordinator mixed_version -- --test-threads=1
```

**Expected:**
- Recorded 0.2 agent heartbeats are accepted.
- Replies to 0.2 agents never contain new `LaunchRequest` fields.
- A new agent sends no new fields before `contention/1` is advertised (SC-008).
