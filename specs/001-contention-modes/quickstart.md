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
  - idle-host utilization and a steady small external user (SC-014, SC-015):
    `cargo test --locked --test contention -- sc_014 sc_015`
  - real floor breaches only: `cargo test --locked --test contention -- charge_beyond`
  - each resource's admission and `held_back`, GPU memory cooling down on its own, and
    admission blocked only while own work is reclaimed:
    `cargo test --locked --test policy -- each_resource_reports decisions_show gpu_cooldown_alone reclaim_admission`
  - an ancestor CPU ceiling held back on its own line, never as contention (SC-016):
    `cargo test --locked --test policy -- held_back_shows_an_ancestor_ceiling`
  - the Performance ramp: 90% within 10 s for CPU and GPU compute, Yield First and Neutral
    unchanged, a mid-ramp external workload served and memory timing unchanged (SC-004):
    `cargo test --locked --test contention -- ramp:: mid_ramp flapping_every_interval`

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

```sh
cargo test --locked --test agent -- --exact rootless_cpu_contention_caps_the_job_and_reverses_without_drain
cargo test --locked --test contention -- sc_002 yield_first_pauses_only a_pause_
```

The agent test starts a coordinator and an agent with `[contention.cpu] mode = "yield_first"`,
runs one CPU job, then starts a test-owned external load on every logical CPU. It checks that
the job is capped or paused, that the supervisor journals the step, that the step reverses after
the load stops, and that the job completes without a drain. The bounds, the margin case and the
minimum duty come from the policy tests on the second line.

**Expected:**
- A load that fits in the 15% margin leaves own work running, with any cap at most keeping the
  margin free.
- A load that saturates the CPU (contended) pauses the opportunistic job within 2 monitor
  intervals (SC-002); in Neutral, `observe` shows a `cap` at the fair share instead, and in
  Performance a cap that follows what the load takes, never below the fair share.
- No drain or termination occurs (SC-007). After the pause bound, the job resumes at the 5%
  minimum duty, as a hint for an SDK worker or a cap for a plain command, and stays there while
  the load lasts.
- After the load shrinks, a held cap rises again at the mode's rate; after it stops, the steps
  reverse.

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
- An allowance-only change calls only the allowance callback.

A self-sizing worker grows while the host has room and shrinks before a hint is needed:

```python
from cedegrid import WorkerContext

context = WorkerContext.from_env()
cache_limit_mib = 1024

def resize(allowance):
    global cache_limit_mib
    if allowance.ram_mib is not None:
        cache_limit_mib = max(256, allowance.ram_mib - 512)  # keep room for working memory

context.on_resource_allowance(resize)
for batch in batches():
    process(batch, cache_limit_mib)
    context.safe_point()  # polls the allowance, targets and drain
```

## 7. Setup (any OS for flags; a TTY for the interface)

```sh
dir="$(mktemp -d)"
cargo run --locked -- --config "$dir/node.toml" setup --non-interactive --json --role node --default-mode performance
cargo run --locked -- --config "$dir/node.toml" setup --non-interactive --json --role node --default-mode performance --acknowledge-caution performance_mode
cargo test --locked --test cli -- no_color_writes the_interface_and_flags
```

The directory must exist, and a new file needs `--role`.

**Expected:**
- The first command exits 3 with `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` and writes nothing.
- The second writes the file and reports the `performance_mode` option as `caution`.
- With `NO_COLOR=1`, the interface's output contains no escape sequences but keeps the
  `[caution]` label.
- The interface and flags produce byte-identical files (SC-011).

## 8. Mixed versions (any OS)

```sh
cargo test --locked --test coordinator mixed_version -- --test-threads=1
```

**Expected:**
- Recorded 0.2 agent heartbeats are accepted.
- Replies to 0.2 agents never contain new `LaunchRequest` fields.
- A new agent sends no new fields before `contention/1` is advertised (SC-008).
