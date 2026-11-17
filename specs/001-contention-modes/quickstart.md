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
the job is paused (in Tracked) or capped (in Full), that the supervisor journals the step, that
the step reverses after the load stops, and that the job completes. The bounds, the margin case,
the whole-task holds and the pause bound come from the policy tests on the second line.

**Expected:**
- A load that fits in the 15% margin leaves own work running, with whole tasks paused at most
  to keep the margin free, to within one task.
- A load that saturates the CPU (contended) pauses the opportunistic job within 2 monitor
  intervals (SC-002); in Neutral, `observe` shows whole tasks paused only while the rest stays at
  or above the fair share, in Tracked and in Full; in Performance no step.
- No drain or termination occurs (SC-007): a pause still contended at the bound moves to another
  task, and where every task is paused the pauses take turns.
- After the load shrinks, paused tasks resume at the mode's rate; after it stops, the steps
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
cargo run --locked -- --config "$dir/node.toml" setup --non-interactive --json --role node --ram-floor-mib 600
cargo run --locked -- --config "$dir/node.toml" setup --non-interactive --json --role node --ram-floor-mib 600 --acknowledge-caution small_floor
cargo test --locked --test cli -- no_color_writes the_interface_and_flags
```

The directory must exist, and a new file needs `--role`.

**Expected:**
- The first command exits 3 with `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` and writes nothing.
- The second writes the file and reports the `small_floor` option as `caution`. Performance, the
  default since item 029, needs no acknowledgement.
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

## 9. Item 030 fixes (any OS for the unit tests; Linux for the signal, `/proc` and cgroup parts)

- `cargo test --locked --test supervision` and `--test managed_supervision`: a supervisor killed
  in an off phase leaves no stopped Opportunistic workload (it is killed) and a running
  Guaranteed one; SIGTERM to a supervisor releases and drains.
- `cargo test --locked --test agent`: SIGTERM to the agent leaves supervisors running and a new
  agent re-adopts them; a heartbeat stays accepted after 2100 released executions; SIGHUP applies
  a mode change and refuses a path change.
- `cargo test --locked --test policy --test contention`: CPU unknown in Performance holds
  Neutral's fair share; caps return to the target in one move at the quiet count.
- `cargo test --locked --test telemetry`: run 5's two shapes measure without unknown and without
  a phantom principal.
- On the test host, after the user's typed approval: run 5 again beside the light and saturating
  GPU users, in Performance and Neutral (`native-gates.md`).

## 10. Item 031 families (Linux for discovery, signals and PSS; any OS for the contract pins)

1. `cargo test --locked --test managed_supervision` in the Docker Linux check: a leader with
   forked and `multiprocessing` workers reports a family of 5 and no process outside it; an
   orphan joins, is stopped and continued with the family, and is reaped by its PID; leftovers at
   leader exit get TERM, then KILL, and `members_ended` counts them.
2. `cargo test --locked --test policy -- family`: a family's CPU counts as own, so in Performance
   its total lands at the target beside a synthetic external principal.
3. On macOS the rootless launch still refuses a multi-process command without SDK children.

## 11. Item 032, the derived GPU sample age (any OS)

1. `cargo test --locked --test configuration -- sample_age`: an interval of 3000 with no sample
   age loads and derives 6000; an explicit 2000 at interval 3000 is refused and names both keys
   and 3001; the defaults derive 2000.
2. `cedegrid --config <file with monitor.interval_ms = 3000> validate` prints
   `derived."gpu.process_sample_max_age_ms".value` 6000, and `doctor --text` lists it under
   "Derived settings".

