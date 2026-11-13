# Native gates deferred from batches 1 to 4 and Phase 6a

Batch 1 (T001–T032), batch 2 (US2, T033–T055), batch 3 (US3, T056–T071) and batch 4 (US4,
T072–T082) were implemented and verified on macOS, with the Linux container check below.
Passing tests on macOS does not establish Linux runtime behavior, and no gate below is recorded
as qualified. Run these only on an authorized Linux host, and record results in
`docs/validation.md`.

## Environment

Every command below assumes a checkout of `001-contention-modes` and this environment:

```sh
mkdir -p "$HOME/.cache/cedegrid-tests"
export TMPDIR="$HOME/.cache/cedegrid-tests"
export CEDEGRID_TEST_PYTHON="$(command -v python3)"   # Python 3.10+
export CEDEGRID_SERVICE_TEST_MONITOR_MS=500
```

## A. Batch 1 checks that can run now

1. Full serial suite:

   ```sh
   cargo test --locked -- --test-threads=1
   ```

2. Host CPU busy bounds on real `/proc/stat` (T019, T023). This is read-only and writes no
   state.

   ```sh
   cargo run --locked -- --config examples/node.toml observe --samples 4 --no-state \
     | python3 -c 'import json,sys; [print(c.get("busy_status"), c.get("busy_lower_millicores"), c.get("busy_millicores")) for c in (json.loads(l)["snapshot"]["kernel"]["cpu"] for l in sys.stdin)]'
   ```

   Expected: from the second sample on, `available`, with the lower bound at or below the
   upper bound.

3. GPU memory fields and Quiet activity (T022, T024). This needs an NVIDIA device and is
   read-only toward every process. Start a test-owned idle CUDA context, observe for longer
   than the quiet window (20 intervals of 500 ms), then stop the context:

   ```sh
   "$CEDEGRID_TEST_PYTHON" -c 'import torch,time; x=torch.ones(1,device="cuda"); time.sleep(900)' &
   cargo run --locked -- --config examples/node.toml observe --samples 30 --no-state > "$HOME/cedegrid-gpu-quiet.jsonl"
   kill %1
   python3 - "$HOME/cedegrid-gpu-quiet.jsonl" <<'EOF'
   import json, sys
   for n, line in enumerate(open(sys.argv[1]), 1):
       for g in json.loads(line)["snapshot"]["gpus"]:
           o = g.get("observation") or {}
           print(n, g["uuid"], g["external_compute"], g.get("reserved_memory_mib"),
                 g.get("free_memory_mib"), g.get("unattributed_memory_mib"),
                 o.get("activity_api_status"), o.get("capability"))
   EOF
   ```

   Expected:
   - `activity_api_status` is `NotFound`, or `NVML_SUCCESS` with no positive sample, for the
     idle context. This answers research R8's open question.
   - `external_compute` changes from `unknown` to `quiet` after about 20 samples.
   - `capability` stays `contention_aware`.
   - `reserved_memory_mib` is present.

4. The existing opt-in gates, as a regression check. Run them only when authorized:

   ```sh
   CEDEGRID_TEST_CGROUP_ROOT=<delegated-subtree> \
     cargo test --locked --test cgroup -- --ignored --exact authorized_native_empty_leaf_preparation_and_cleanup
   CEDEGRID_TEST_GPU_UUID=<GPU-UUID> \
   CEDEGRID_GPU_EVIDENCE="$HOME/cedegrid-gpu-evidence-$(date +%Y%m%d%H%M%S).json" \
     cargo test --locked --test gpu_native -- --ignored --exact owned_cuda_context_is_observed_then_released_after_drain
   ```

   `gpu_native` needs PyTorch with CUDA in `CEDEGRID_TEST_PYTHON`.

## Linux container check

[`tools/docker-validation/linux-check`](../../tools/docker-validation/linux-check/Dockerfile) builds a
non-root Linux image and runs fmt, clippy, the full serial suite and both Python suites on a
`git archive` export of `HEAD`. It needs Docker and has no cgroup delegation or GPU, so it never
replaces sections A to C. From the repository root:

```sh
src="$HOME/.cache/cedegrid-linux-src"
rm -rf "$src" && mkdir -p "$src" && git archive HEAD | tar -x -C "$src"
docker build -t cedegrid-linux-check tools/docker-validation/linux-check
docker volume rm -f cedegrid-linux-work
docker run --rm --cpus 6 --memory 6g -v "$src":/src:ro \
  -v cedegrid-linux-cargo:/home/cedegrid/.cargo \
  -v cedegrid-linux-target:/home/cedegrid/target \
  -v cedegrid-linux-work:/home/cedegrid/work \
  cedegrid-linux-check sh /src/tools/docker-validation/linux-check/run.sh
```

The work volume must stay at the same path: test binaries embed it, and Cargo does not rebuild
them when it moves. Remove `cedegrid-linux-target` after changing the image's paths.

## B. Batch 2 (US2) checks that can run now

Container evidence, not host qualification: items 1 and 2 passed at commit 015d9c1 in a Docker
Desktop Linux container (aarch64, kernel 7.0.12-linuxkit, Rust 1.98.1, Python 3.11, non-root
UID 1000). fmt, clippy and the full serial suite passed with 470 tests and 9 ignored, and both
Python suites passed. The ignored tests are subprocess fixtures run by their owning tests, one
opt-in measurement, and the two hardware gates, `authorized_native_empty_leaf_preparation_and_cleanup`
(T054) and `owned_cuda_context_is_observed_then_released_after_drain` (T088). The container had
no delegated cgroup, so `CgroupActuator` over `LeafHandle` compiled but did not run; T054 covers it.
State and test fixtures must be on a local disk filesystem under `$HOME`, so the checkout was
copied to a named volume there; Docker's overlay filesystem is refused by design.

1. Compile and lint the Linux-only paths. `CgroupActuator` over the pinned leaf descriptors
   (`LeafHandle`) and `CgroupBackend::actuator` compile only for Linux, and macOS has no Linux C
   toolchain for a cross-check:

   ```sh
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked --test cgroup --test supervision --test managed_supervision --test agent -- --test-threads=1
   ```

2. Rootless CPU contention (quickstart §3) and the actuation audit (T036). On Linux, signals go
   through pidfds; the macOS runs used the verified direct-child fallback:

   ```sh
   cargo test --locked --test agent -- --exact rootless_cpu_contention_caps_the_job_and_reverses_without_drain
   cargo test --locked --test managed_supervision -- --exact runtime_steps_reach_only_the_verified_family
   cargo test --locked --test agent -- --exact a_reconnected_controller_still_reaches_only_the_verified_family
   cargo test --locked --test agent -- --exact a_restarted_agent_refuses_a_reused_supervisor_pid
   ```

   Expected: all pass, with no `T` (stopped) state on the unmanaged fork or the foreign process.

3. Whether the delegated subtree permits runtime `cgroup.freeze` (research R4). This uses a
   test-created leaf and a test-owned `sleep` only:

   ```sh
   leaf=<delegated-subtree>/cedegrid-freeze-check
   mkdir "$leaf"
   sh -c 'echo $$ > "$1/cgroup.procs"; exec sleep 30' _ "$leaf" &
   sleep 0.2
   echo 1 > "$leaf/cgroup.freeze"; grep frozen "$leaf/cgroup.events"   # frozen 1
   echo 0 > "$leaf/cgroup.freeze"; grep frozen "$leaf/cgroup.events"   # frozen 0
   kill %1; wait; rmdir "$leaf"
   ```

   If the write is refused, authorize only `cpu.weight` and `cpu.max`; pause then uses the
   rootless SIGSTOP fallback.

## US3 checks that can run now

1. Principals on a real host (T063), read-only. Expected: the owner principal, one entry per
   other UID with usage, and, under a `hidepid` mount, unreadable owners grouped under `None`:

   ```sh
   cargo run --locked -- --config examples/node.toml observe --samples 3 --no-state | tail -1 \
     | python3 -c 'import json,sys; [print(p["uid"], p["is_owner"], p["cpu_millicores"], p["ram_mib"], p.get("gpu_memory_mib")) for p in json.loads(sys.stdin.read())["snapshot"]["principals"]]'
   ```

2. GPU principals (T063), on an NVIDIA host. Start an idle CUDA context under a second,
   test-owned account, observe, then stop it. Expected: that account's UID lists the context's
   memory under `gpu_memory_mib` for `<GPU-UUID>`, and no step acts on it:

   ```sh
   sudo -u <test-account> "$CEDEGRID_TEST_PYTHON" -c 'import torch,time; x=torch.ones(1,device="cuda"); time.sleep(120)' &
   cargo run --locked -- --config examples/node.toml observe --samples 5 --no-state | tail -1 \
     | python3 -c 'import json,sys; [print(p["uid"], p.get("gpu_memory_mib")) for p in json.loads(sys.stdin.read())["snapshot"]["principals"]]'
   kill %1
   ```

3. The OOM preference on the host kernel (T061, T067). The Linux container check already runs
   these; a host run records host evidence:

   ```sh
   cargo test --locked --test supervision -- --exact the_gate_marks_own_work_as_the_preferred_oom_victim_before_exec
   cargo test --locked --test managed_supervision -- --exact managed_children_carry_the_oom_preference
   ```

## US4 checks that can run now

1. Quickstart §6 and the supervisor target tests on a Linux host. The Linux container check
   already runs them:

   ```sh
   PYTHONPATH="$PWD/python" "$CEDEGRID_TEST_PYTHON" -m unittest discover -s python/tests -p 'test_worker*.py'
   (cd npm && npm ci --ignore-scripts && npm test)
   cargo test --locked --test managed_supervision -- hint managed_children_receive_the_target_file_and_its_targets
   ```

2. A GPU memory hint on an NVIDIA host needs US5's occupied-device scenarios and runs with T088:
   an SDK worker holding memory on `<GPU-UUID>` receives a `gpu_memory_mib` target when external
   memory crosses the device floor, and is drained after the hint deadline only if it ignores it.

## Phase 6a (revisions before US5) checks

Container evidence, not host qualification, at 8128cbf in the same Docker Desktop container
(aarch64, kernel 7.0.12-linuxkit, Rust 1.98.1, Python 3.11): fmt and clippy passed, the serial
suite ran 533 tests with 532 passed, 1 failed and 9 ignored, and both Python suites passed. The
failure, `pause_is_refused_for_guaranteed_work_and_unavailable_steps_are_reported`, was a timing
flake of the actuation tests in `tests/supervision.rs`, which sampled a 1 s counter worker at
fixed offsets and failed about one run in four here (2 of 8 runs at 0ad75e1). They now wait for
observed conditions with a deadline, and the worker runs until the test stops it. The
supervision test binary then passed 20 consecutive runs of 25 tests in this container. Full
runs passed at c710915 (527 passed, 9 ignored) and at 384e726 (541 passed, 9 ignored, with fmt,
clippy and both Python suites). The readiness, terminate and target-deadline paths need no
delegated cgroup or GPU, so they are covered here:

```sh
cargo test --locked --test managed_supervision -- readiness hint_carries guaranteed_cpu_target child_started_after
cargo test --locked --lib terminate_sends_term_at_once_and_kill_after_the_grace
```

## Phase 6b and US5 checks

Container evidence, not host qualification, at 6d8e8c5 in the same Docker Desktop container
(aarch64, kernel 7.0.12-linuxkit, Rust 1.98.1, Python 3.11): fmt and clippy passed, the serial
suite passed 575 tests with 9 ignored, and both Python suites passed (184 and 52). GPU
admission, duty-cycling and placement are covered by unit and integration tests only; the
native GPU evidence stays with T088.

## Polish checks (T120, T121)

T120 at 9851fd5. On macOS: fmt, clippy and the 1.88 check passed; the serial suite passed 644
tests with 6 ignored; both Python suites passed (57 and 184), and so did npm (31). Container
evidence, not host qualification, at the same commit in the same Docker Desktop container
(aarch64, kernel 7.0.12-linuxkit, Rust 1.98.1, Python 3.11): fmt and clippy passed, the serial
suite passed 650 tests with 9 ignored, and both Python suites passed. The
`x86_64-pc-windows-msvc` target is installed, but `cargo check --target x86_64-pc-windows-msvc`
stops in `ring`'s C build because this macOS host has no Windows C headers; Windows CI covers
the client paths.

T121 on macOS at 9851fd5: every command in quickstart §1–3 and §6–8 passed. §1: d1–d6 (6), the
contention tests (67), SC-014/SC-015 (2), `charge_beyond` (1), and admission and `held_back`
(11). §2: `agent` (17, 2 ignored fixtures) and `supervision` (25). §3: the agent run (1) and
the policy bounds (5). §6: the worker tests (18) and npm (31). §7: the first setup command exits
3 with `ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED` and writes nothing; the second writes the file and
reports `performance_mode` as `caution`; the terminal tests pass (2). §8: `mixed_version` (4).
§4 and §5 are the native gates T054 and T088.

## Performance ramp gate (T173)

Written, not run. It runs only on the authorized Linux test host, with the user's approval for
that run. It needs the host preparation `docs/comparison.md` describes (the release binary and
`venv-isolated` under `$cedegrid_validation_root`), at least 4 physical CPUs that nothing else
uses during the run, 8 GiB of available RAM and 8 GiB of free disk. `tools/ramp_gate.py` starts
a fresh private coordinator and agent in Performance at its defaults (no cooldown override, no
CPU reserve), submits 250 m duty-cycled tasks, 4 per CPU, and reads the agent's decisions back.
With `--contender-at`, it also starts an unmanaged one-core busy loop it owns, pinned to the
last CPU of the scope. Each run writes `report.json` under a new output directory.

```sh
cargo build --locked --release
cedegrid_validation_root="$HOME/.local/share/cedegrid-validation"
ramp() { # ramp <name> [gate options...]
  name="$1"; shift
  "$cedegrid_validation_root/venv-isolated/bin/python" tools/ramp_gate.py \
    --root "$cedegrid_validation_root" \
    --output "$cedegrid_validation_root/evidence/$name-$(date -u +%Y%m%dT%H%M%SZ)" "$@" --execute
}
ramp ramp-idle --cpus 4
ramp ramp-contender --cpus 4 --contender-at 3
ramp ramp-contender-unpinned --cpus 4 --contender-at 3 --contender-unpinned
```

The pinned contender runs on one CPU of the scope, where own threads can still run, so it
also measures the per-core case that 003 FR-056 addresses. The unpinned contender can run on
any CPU of the scope and measures 001's SC-004 alone.

A contender under a second unprivileged account measures SC-004 for another principal. That
account starts it, timed to a fixed submission, with a marker among its arguments; the gate
finds it by UID and marker and only reads its `/proc` entries, never acting on it:

```sh
at=$(( $(date +%s) + 60 ))
ramp ramp-contender-other --cpus 4 --submit-at "$at" --contender-at 3 \
  --contender-uid <uid> --contender-marker cedegrid-contender
# As the second account, on the same host:
sleep $(( at + 3 - $(date +%s) )); exec taskset -c <cpu> python3 -c "<one-core loop>" 40 cedegrid-contender
```

`--device gpu --gpu-uuid <uuid>` times own GPU compute instead (SC-004, first bullet), with
CUDA busy tasks requesting `GPU_TASK_MIB` of device memory each.

Record from each `report.json` in `docs/validation.md`:

- `ramp_seconds` (idle run): seconds from submission until own CPU reaches 90% of that
  sample's target share. The share is capacity minus the margin and external use, which the
  coordinator, agent and supervisors on the same CPUs add to. SC-004 expects at most 10. `null`
  means the target was never reached within `--seconds`. `target_millicores` reports 90% of the
  share when work starts.
- `contender_served_seconds` (contender run): seconds from the contender's start until its own
  CPU time, read from `/proc/<pid>/stat`, reaches 0.9 of a core over one sample interval. Its
  one core is within its fair share, so SC-004 expects it within SC-002's Performance bound,
  ⌈(own − (capacity − 1000 m − margin)) ÷ margin⌉ intervals: about 11 intervals (5.5 s) on 4
  CPUs.
- `foreign_actions`: must be empty, so no step reached a process the gate did not submit,
  including the contender (SC-005).
- `owned_cleanup`: every exit code, and `guard`.

## Item 027 native gates (T228)

Runs only on the authorized test host, with the user's typed approval for item 027, under the
host rules of items 021 and 026: user-space only, `nohup` with logs, an idle check before and
after each run, nothing on the shared host, and loads of the second unprivileged account started
and stopped by that account under `timeout`, with a marker among the loop's arguments.
`$repo` is a `git archive` of the final commit built with `cargo build --locked --release`;
`$H` holds the validation harness kept with the evidence (`sc014_015.py` from item 024, and
`idle_027.py`, `idle-full-027.sh`, `return_027.py`, `gpu-027.sh`, `run-peer-027.sh`,
`peer-contender-027.sh` and `sc014-rerun-027.sh`); `$py` is `venv-isolated/bin/python`.

```sh
# 0. Second account: it is no admin and `sudo -n` fails.
ssh <second account>@<test host> 'id -u; id -Gn; sudo -n true; echo sudo=$?'
# 1. Idle, 60 s, Tracked and Full (an empty delegated root inside a delegated user scope).
$py $H/idle_027.py --repo $repo --root $root --output <dir> --cpus 4 --backend tracked   # and --cpus 14
systemd-run --user --scope -p Delegate=yes --quiet --unit cedegrid-027-idle-<n> \
  bash $H/idle-full-027.sh $repo 4 <dir>                                              # and 14
# 2. Launch burst (150 light tasks, Performance).
CEDEGRID_TEST_LAUNCH_BURST=1 cargo test --locked --release --test launch_burst -- --ignored --nocapture
# 3. SC-014, steady loop, 100 m tasks: 14 CPUs in each mode, then 4 CPUs in Performance.
$py $H/sc014_015.py --repo $repo --root $root --output <dir> --mode <mode> --cpus 14 \
  --task-millicores 100 --seconds 90 --warmup-max 1200 --task-loop steady
#    Neutral and Yield First add one worker per 10 s and 30 s, so on 14 CPUs they need
#    --warmup-max 2400 and 4800 (sc014-rerun-027.sh).
# 4. SC-015, Yield First, 4 CPUs, 250 m tasks beside a 60 m external user, both loops.
$py $H/sc014_015.py ... --mode yield_first --cpus 4 --task-millicores 250 --seconds 330 \
  --external-millicores 60 --warmup-max 1200 --task-loop catch-up    # and steady
# 5. T178 pinned, contender under the second account (as item 026 run 2).
bash $H/run-peer-027.sh <commit> pinned <epoch> <uid>
ssh <second account>@<test host> 'T=<epoch+3> CPUS=<last scope CPU> S=40 sh -s' < $H/peer-contender-027.sh
# 6. Return after contention, Yield First, 4 CPUs: the harness writes ready.json with the epoch;
#    the second account's one-core contender runs 30 s from it.
$py $H/return_027.py --repo $repo --root $root --output <dir> --contender-uid <uid>
ssh <second account>@<test host> 'T=<epoch> CPUS=<cpu> S=30 sh -s' < $H/peer-contender-027.sh
# GPU (Part 5): per-process utilization, listed contexts and their user IDs, the policy's view.
bash $H/gpu-027.sh $repo
```

Gates, from each run's `report.json` or log: run 1, the decisions' CPU external estimate (which
no longer counts CedeGrid) at p95 at most 1% of capacity; run 2, longest observation gap at most
1 s, no lease expiry, no drain, all 150 tasks run or complete; run 3, own tasks at least 97%,
94% and 84% of capacity in Performance, Neutral and Yield First on 14 CPUs and 97% in
Performance on 4, with CedeGrid's CPU stated apart; run 4, own work at least 90% of capacity
minus the user and the margin, no pause, no cap cycling, no drain; run 5, the contender at 0.9
core within SC-012's bound; run 6, own work at the contended target within 2 intervals of the
contender's first observed usage (SC-002) and back at the uncontended target within 3 s of its
last; run 7 (from run 3's Performance run on 14 CPUs), the observation interval's p50, p95 and
maximum, the agent's `loop_cost` stages and CedeGrid's CPU.

## Item 029 native gates (T275)

Runs only on the authorized test host, with the user's typed approval for item 029 Part 3,
under the host rules of items 021, 026 and 027: user-space only, `nohup` with logs, an idle
check before and after each run, nothing on the shared host, and loads of the second
unprivileged account started and stopped by that account under `timeout`, with a marker among
the loop's arguments. `$repo`, `$root`, `$py` and the `gate` function are as above, from a
`git archive` of the final commit. Harness pieces not in the repository are written for the run
and kept with the evidence in `$H` (`same-core-029.py`, `fixed-baseline-029.py`,
`gpu-own-029.py`, `gpu-cpu-029.py`, with item 027's `run-peer-027.sh`,
`peer-contender-027.sh` and `return_027.py`).

```sh
# 1. Idle: own throughput with vs without CedeGrid in each mode, at the 50 ms cadence.
gate e029-idle-performance cpu --profile own-efficiency
gate e029-idle-neutral cpu --profile own-efficiency --contention-mode neutral
gate e029-idle-yield_first cpu --profile own-efficiency --contention-mode yield_first
# 2. A light other user on the probe's cores (the second account, below its fair share),
#    and 3. a saturating one, each mode, with and without CedeGrid.
$py $H/same-core-029.py --repo $repo --root $root --output <dir> --mode <mode> --load light
ssh <second account>@<test host> 'T=<epoch> CPUS=<probe cpus> S=60 LOAD=light sh -s' < $H/peer-contender-027.sh
$py $H/same-core-029.py ... --load saturating        # the same with LOAD=saturating
# 4. Fixed allocation: taskset to half the scope vs CedeGrid in Performance, beside the
#    second account's load on 30 s / off 30 s for 5 minutes.
$py $H/fixed-baseline-029.py --repo $repo --root $root --output <dir> --seconds 300
# 5. GPU in each mode: idle, a light and a saturating other user on the same device.
gate e029-gpu-idle-<mode> gpu --profile own-efficiency --contention-mode <mode>
$py $H/gpu-own-029.py --repo $repo --root $root --output <dir> --mode <mode> --load <light|saturating>
# 6. A GPU job whose loader threads use more CPU than its fair share, beside the second
#    account saturating the same CPUs, in Tracked (forced) and in Full.
$py $H/gpu-cpu-029.py --repo $repo --root $root --output <dir> --backend <tracked|full>
# 7. Report 027's failed runs again: run 2, the launch burst of 150 tasks; run 5, the
#    pinned contender; run 6, the return after contention on 4 CPUs in Yield First and in
#    Performance.
CEDEGRID_TEST_LAUNCH_BURST=1 cargo test --locked --release --test launch_burst -- --ignored --nocapture
bash $H/run-peer-027.sh <commit> pinned <epoch> <uid>
ssh <second account>@<test host> 'T=<epoch+3> CPUS=<last scope CPU> S=40 sh -s' < $H/peer-contender-027.sh
$py $H/return_027.py --repo $repo --root $root --output <dir> --contender-uid <uid> --mode <yield_first|performance>
ssh <second account>@<test host> 'T=<epoch> CPUS=<cpu> S=30 sh -s' < $H/peer-contender-027.sh
```

Gates, from each run's `report.json` or log: run 1, `matched_idle_throughput_ratio_min` at
least 0.99 in each mode, with the placement recorded; run 2, no step and own throughput at least
0.99 of without; run 3, own work at capacity minus the other user's demand minus the mode's
margin, with the other user's p99 against alone and own throughput, each with and without
CedeGrid; run 4, total own work and the other user's p99 for both; run 5, at least 0.99 idle,
and per mode whether own GPU work alone is capped at its start for another account's idle
context, and for how long; run 6, own GPU utilization and units per second with and without
CedeGrid in Tracked and in Full; run 7, run 2 with no agent exit, no lease expiry, the longest
observation gap within 1 s and the agent's open descriptors at the peak, run 5 with the
contender's median, own work against its target and cap changes per minute, and run 6 back at
the uncontended target within 3 s in both modes.

## Item 031 native gates (T387)

Runs only on the authorized test host, with the user's typed approval in the session, asked
together with T368, under the host rules of items 021, 026, 027 and 029. `$repo`, `$root`,
`$py` and the `gate` function are as above, from a `git archive` of the final commit. The
harness `family-031.py` is written for the run and kept with the evidence in `$H`; it launches
one job through the agent, reads the supervisor's `family` reports, `status` and `explain`,
and samples each member's state, CPU, `smaps_rollup` and `oom_score_adj` every 100 ms.

```sh
# 1. A multiprocessing stand-in: a leader that fills a 1 GiB buffer, then 4 forked workers
#    that read it and spin for 120 s, in Performance, Tracked (forced) and Full, idle and
#    beside the second account saturating the same CPUs.
$py $H/family-031.py --repo $repo --root $root --output <dir> --backend <tracked|full> \
    --workload multiprocessing --workers 4 --buffer-mib 1024 --seconds 120 --load <none|saturating>
ssh <second account>@<test host> 'T=<epoch> CPUS=<probe cpus> S=120 LOAD=saturating sh -s' < $H/peer-contender-027.sh
# 2. Where PyTorch is installed: a DataLoader(num_workers=4) loop over a synthetic dataset,
#    the same way.
$py $H/family-031.py --repo $repo --root $root --output <dir> --backend <tracked|full> \
    --workload dataloader --workers 4 --seconds 120 --load <none|saturating>
# 3. Leftovers: the leader exits while its 4 workers run on, in each class.
$py $H/family-031.py --repo $repo --root $root --output <dir> --backend <tracked|full> \
    --workload leftovers --workers 4 --class <guaranteed|opportunistic>
```

Gates: runs 1 and 2, a family of 5 in `status` and `explain` (`family_members`), nothing outside
it, own CPU at the mode's target beside the load with the leader not held at the minimum duty,
own memory within 10% of one buffer plus the workers' private pages, every member stopped
and continued with the family under a step, and the agent's own cost per interval against the
029 budget; run 3, `members_ended` 4, no member or zombie left, in both classes.

## Item 032: run 7's return with the batch (T396)

Runs in the same batch as T368 and T387, under the same typed approval, and the same host
rules. Report 029's run 7 return after contention (029 Decision 2, T339) again, on 4 CPUs in
Yield First and in Performance, as item 029's run 7 runs it, keeping the agent's decision records
for the run so the lag between the quiet count and own usage coming back can be located.

```sh
$py $H/return_027.py --repo $repo --root $root --output <dir> --contender-uid <uid> --mode <yield_first|performance>
ssh <second account>@<test host> 'T=<epoch> CPUS=<cpu> S=30 sh -s' < $H/peer-contender-027.sh
# Before the run's state is removed: the agent's observation and decision records.
$repo/target/release/cedegrid --config <the run's node config> history --limit 10000 > <dir>/decisions.json
```

Gate: from the contender's end, per interval, the quiet count, each held cap and its target,
and own usage, with the interval at which own usage is back at the uncontended target (within
3 s in both modes). If the lag reproduces, the records name the interval and the step where own
usage stayed behind its target, and the report proposes the fix without changing policy. If it
does not, `docs/validation.md` records that and T339 closes. The evidence copy redacts host
names, user names, GPU UUIDs, IPs and home paths, as every evidence copy does.

## Item 033: supervisor loss in Tracked with the batch (T406)

Runs in the same batch as T368, T387 and T396, under the same typed approval and host rules,
with light, test-owned loads only. The agent runs as on the test host (in its own session under
the user's service manager). A family of a leader and 2 CPU workers runs in Tracked, held in an
off phase by a cap beside a test-owned light load, and the run's own supervisor is then killed,
once per class.

```sh
$py $H/family-031.py --repo $repo --root $root --output <dir> --backend tracked \
    --workload supervisor-loss --workers 2 --class <guaranteed|opportunistic>
# The agent's events for the run, before its state is removed:
grep family_recovered <the agent's log> > <dir>/recovery.json
```

Gate, per class: which signals each member received (the workload logs SIGHUP and SIGCONT),
whether any was still stopped 2 s after the kill, and how it ended. Guaranteed: no SIGHUP,
every member running again within one reconciliation interval, the workload's output complete.
Opportunistic: every member ended, none left stopped. `family_recovered` counts match.

## Item 034: the rerun after the batch's fixes (T422)

Runs once on the authorized test host after `034.done`, with the user's typed approval in the
session, under the 033 batch's host rules: light test-owned loads and the second account's own
loads only, nothing on the shared host, no installs. The harness is the 033 batch's
(`same_gpu_029.py`, `return_033.py`, `family_031.py`, `gpu_trace_033.py` and the Mac-side
`runs-033.sh`), from a `git archive` of the final commit.

```sh
# 1. T368: run 5 in Performance and Neutral beside the light and saturating GPU users.
bash runs-033.sh <commit> <second account uid> t368
# Then, read only, each run's GPU trace, which now holds `compute_sampling`:
$py $H/gpu_trace_033.py <each e033-r5 output>
# 2. T396's Yield First return, with the agent's decision records.
$py $H/return_033.py --repo $repo --root $root --output <dir> --contender-uid <uid> --mode yield_first
ssh <second account>@<test host> 'T=<epoch> CPUS=<cpu> S=30 sh -s' < $H/peer-contender-027.sh
# 3. T387's Tracked multiprocessing run with the 1 GiB family, idle and beside the load.
$py $H/family_031.py --repo $repo --output <dir> --backend tracked --workload multiprocessing \
    --workers 4 --buffer-mib 1024 --seconds 120 --load <none|saturating> --peer-uid <uid>
```

Gates: run 1, managed throughput against unmanaged at least 0.9 beside the light user in
Performance and Neutral, and about 0.5 beside the saturating one, with `compute_sampling` showing
the slots each interval used (`process_period` where a PID held 2 samples, else the device's
count, in which case the report says the fallback applied and why); run 2, own usage back at 98%
of its level within 3 s of the contender's end, in one move; run 3, the agent's own CPU within 1%
of the scope, own memory within 10% of the buffer plus the workers' private pages, and the PSS
period K the agent chose.

## Item 036: T368 with the sample span (T433)

Runs once on the authorized test host after `036.done`, with the user's typed approval in the
session, under the 033 batch's host rules: light test-owned loads and the second account's own
loads only, nothing on the shared host, no installs. The harness is `batch-035`'s, which ends each
run before stopping its agent and stops the batch if a test process of either account is left;
its trace tool also writes each window's GPU snapshots for a replay.

```sh
bash runs-035.sh <commit> <second account uid> t368
# Read only, after each run: the GPU trace and the replay input from the agent's state database.
$py $H/gpu_trace_035.py <each e035-r5 output>
# On the Mac, the redacted copy replayed through the code:
CEDEGRID_GPU_REPLAY=<the run's gpu-replay.json> cargo test --locked --lib replay_gpu_trace -- --ignored --nocapture
```

Gates: managed throughput against unmanaged at least 0.9 beside the light user in Performance
and Neutral, and about 0.5 beside the saturating one; per managed window, how many intervals read
measured alone, over the last 4, or unknown, and whether the replay equals the recorded reading;
if the windows stay unknown at their start, how many intervals `quiet_window_intervals` holds.

## Item 037: the light-user diagnostic and T368's Neutral saturating case

Runs once on the authorized test host after `037.done`, with the user's typed approval in the
session, under the same host rules. The harness is `batch-035`'s with item 037's changes: run 5
waits for each probe's `result.json`, up to the probe's length after its `ready.json`, and moves
later windows by any delay; the leftover check also names the probe.

```sh
bash runs-037.sh <commit> <second account uid> diag t368ns
# diag: the second account's light load (200 multiplies a second) beside the GPU probe run
# unmanaged at a fixed duty of 100%, 70%, 40%, 0% (that user alone) and 100% again, 60 s each,
# with an agent recording every interval and no managed run. The probe sets its duty itself:
"$py" tools/pressure_probe.py --mode gpu --seconds 60 --cpu <scope CPU> --output <dir> --duty 0.7 --period-ms 500
# t368ns: T368's Neutral saturating case, as under "Item 036".
```

Record per duty level: the probe's measured duty (`busy_seconds` over `elapsed_seconds`) and
throughput; the light user's mean per-interval reading and its samples' `sm_util`; the device's
utilization; and the light user's request rate and p50 and p99 latency. The 500 ms period is
CedeGrid's own duty-cycle period (the monitor interval), so the probe repeats the on and off
pattern own work had under T368's caps; it is 3 of the device's samples. If the light user's
reading rises with own duty while its request rate stays flat, the extra reading is time-slice
inflation, not demand.

## Item 039 and 040: T368 with GPU demand from uncapped intervals and the duty from own share (T445)

Runs once on the authorized test host after `040.done`, with the user's typed approval in the
session, under the same host rules. The harness is `batch-037`'s with the fixed probe: run 5 now
starts with the probe alone, before the second account's load starts, then that user alone beside
no process of own work's, then the unmanaged and managed windows in turn; it waits for each
probe's result and moves later windows by any delay.

```sh
bash runs-037.sh <commit> <second account uid> t368   # Performance and Neutral, light and saturating
# Read only, after each run: the GPU trace, with each decision's gpu_demand record and duty.
$py $H/gpu_trace_035.py <each e037-r5 output>
```

Gates, per run, in SC-017's form ("alone" the same probe or user with nothing else on the device,
in the same run): own work with CedeGrid ÷ the probe alone is at least min(the mode's target ÷
1000, own work without CedeGrid ÷ alone) − 0.03; the other user's rate with CedeGrid is at least
min(its rate without CedeGrid, its fair share × its rate alone) − 3%. Report beside them the old
ratio, own work with CedeGrid ÷ without beside the same user, and per managed window the other
user's rate, p50 and p99, the duty CedeGrid set, how many decisions fed the estimate, how many
were in a re-measure window, and the estimate and own work's uncapped share.

## Item 041: T368 and two own processes under the time-slice rule (T456)

Runs once on the authorized test host after `041.done`, with the user's typed approval in the
session, under the same host rules and `batch-037`'s harness with the fixed probe and the
probe-alone and other-user-alone windows. The `two` part runs two probe processes per window,
each its own job when managed, beside the saturating user. That is m = 2 against E = 1, the path
that still caps.

```sh
bash runs-037.sh <commit> <second account uid> t368 two   # T368's four runs, then the two-process runs
# Read only, after each run: the GPU trace, with each decision's time_slice record.
$py $H/gpu_trace_035.py <each e037-r5 output>
```

Gates, per run, in SC-017's form, with the mode's target for the principals actually on the
device (500 beside the saturating user). In the two-process runs the other user must also reach
½ × its rate alone × 0.97, since there the caps exist to give it the fair share that unmanaged
own work does not leave it. Report beside them the old ratio, per managed window the other user's
rate, p50 and p99, the duty CedeGrid set, and m, E, k and whether the rule held in each decision.

## Item 043: T456's remaining runs under the time-slice duty

Runs once on the authorized test host after `043.done`, under the user's standing approval and
the same host rules, with `batch-037`'s harness: two own probe processes per window, each its
own job with a 512 MiB VRAM request when managed, submitted 40 s before its window
(`--managed-lead 40`), so a launch cooldown cannot offset them. That is m = 2 against E = 1, the
time-slice duty's path.

```sh
bash runs-037.sh <commit> <second account uid> two twolight   # Performance and Neutral beside the saturating user, then Performance beside the light user
$py $H/gpu_trace_035.py <each e037-r5 output>                  # read only: each decision's time_slice record
```

Gates, per valid managed window:

- Own work, in SC-017's form: managed ÷ pair alone ≥ min(target ÷ 1000, unmanaged ÷ pair alone) −
  0.03, with the target 500 beside the saturating user and 1000 − the light user's reading
  alone − the mode's margin beside the light one.
- The saturating user: at least ½ × its rate alone × 0.97.
- The light user: its rate within 3% of its rate beside unmanaged own work, and its p99 no worse
  than that run's p99 + 10%.

A window counts when both probes ran together, m = 2 held through it, and it lay inside the other
user's load (042's checks). Report per decision m, E, k, the slice-bound principals, d0, the
duty and the device's utilization; both forms of own work's ratio; and the other user's rate,
p50 and p99. A failed gate changes no product code in that item; the report gives the cause
and a recommended fix.

## Item 044: T456 under aligned duty phases and the margin at d0

Runs once on the authorized test host after `044.done`, under the user's standing approval and
the same host rules, with `batch-037`'s harness: two own probe processes per window, each its
own job with a 512 MiB VRAM request when managed, submitted 40 s before its window. Aggressive
runs as Performance with no GPU compute margin (`reduced_margin` acknowledged), since no mode or
key names it, and is reported for the record without blocking T456.

```sh
bash runs-037.sh <commit> <second account uid> two twolight twoaggr
$py $H/gpu_trace_035.py <each e037-r5 output>   # read only: each decision's time_slice record
```

Gates, per valid managed window (042's three checks):

- Own work, in SC-017's form: managed ÷ pair alone ≥ min(target ÷ 1000, unmanaged ÷ pair alone) −
  0.03, the target beside the saturating user 480 in Performance and 450 in Neutral (500 in
  Aggressive), and beside the light user 1000 − its reading alone − the margin.
- The saturating user: at least ½ × its rate alone × 0.97.
- The light user: its rate within 3% of its rate beside unmanaged own work, and its p99 no worse
  than that p99 + 10%.

Also report the phases: the harness reads each managed probe's run state from `/proc/<pid>/stat`
about every 10 ms, read-only, and gives both probes' off-phase overlap (both stopped ÷ either
stopped), the duty held (1 − the mean stopped share), and own work's share against m × d ÷ (m +
E) at that duty. The gap left after alignment is the drain; report it with what compensating it
would take, without adding compensation. A failed gate changes no product code in that item. If
every gate passes, T456 is checked off.

## Item 045: the step floor with a zero margin

Runs once on the authorized test host after `045.done`, under the user's standing approval and
the same host rules, with `batch-037`'s harness: two own probe processes per window, each its
own job with a 512 MiB VRAM request when managed, submitted 40 s before its window. A zero margin
runs as Performance with no GPU compute margin (`reduced_margin` acknowledged).

```sh
bash runs-037.sh <commit> <second account uid> twoaggr twoperf
$py $H/gpu_trace_035.py <each e037-r5 output>   # read only: each decision's time_slice record
```

Gates, per valid managed window (042's three checks):

- Zero margin beside the saturating user, now gated:
  - the saturating user gets at least ½ × its rate alone × 0.97;
  - own work passes SC-017 at the target 500: managed ÷ pair alone ≥ min(0.5, unmanaged ÷ pair
    alone) − 0.03.
- Performance beside the saturating user, as a regression check under T456's gates (target 480,
  the saturating user at least ½ × its rate alone × 0.97).

Report per window each rise and fall of the duty with the dwell's average and the utilization
readings before it, and the probes' off-phase overlap. A failed gate changes no product code in
that item.

## Item 046: own work pays for the re-measure windows

Runs once on the authorized test host after `046.done`, under the user's standing approval and
the same host rules, with `batch-037`'s harness: two own probe processes per window, each its
own job with a 512 MiB VRAM request when managed, submitted 40 s before its window.

```sh
bash runs-037.sh <commit> <second account uid> twoaggr two   # zero margin, then Performance and Neutral
$py $H/gpu_trace_035.py <each e037-r5 output>   # read only: each decision's time_slice record
```

Gates, per valid managed window (042's three checks):

- Zero margin beside the saturating user, as in 045: the saturating user gets at least ½ × its
  rate alone × 0.97, and own work passes SC-017 at the target 500.
- Performance and Neutral beside the saturating user, under T456's gates: own work by SC-017 at
  480 and 450, the saturating user at least ½ × its rate alone × 0.97.

Report per window the duty held, d0, w and d0', and the steps. A failed gate changes no product
code in that item.

## Item 047: SC-014's fill at HEAD and Full's GPU path

Runs once on the authorized test host after `047.taken`, under the user's standing approval and
the same host rules, with `batch-037`'s harness. `full-047.sh` runs a harness script inside a
delegated user scope: the shell joins a leaf of its own, and the agent gets an empty delegated
root with `cgroup.procs`, `cpu.weight`, `cpu.max` and `cgroup.freeze` authorized.

```sh
# Full: Performance, two own probe processes beside the saturating user; the harness reads each
# probe's leaf cgroup.events `frozen` and its /proc state about every 10 ms, read only.
bash runs-037.sh <commit> <second account uid> twofull
#   = systemd-run --user --scope -p Delegate=yes --quiet --unit cedegrid-047-<n> \
#       bash $H/full-047.sh $repo same_gpu_029.py --mode performance --load saturating \
#       --processes 2 --gpu-mib 512 --peer-uid <uid> --gpu-uuid <GPU-UUID> --output <dir>
# SC-014: report 027's run 3, 14 CPUs, 100 m tasks, steady loop; Performance as the control.
# Run on the host itself under nohup, with no login to the host until it ends: a login's
# processes are the test account's unmanaged usage and restart Yield First's admission cooldown.
nohup bash $H/sc014-quiet-047.sh <commit> \
  "performance:1200:0 neutral:2400:300 yield_first:4800:600" > <log> 2>&1 &
#   each = $py $H/sc014_015.py --repo $repo --root $root --mode <mode> --cpus 14 \
#       --task-millicores 100 --seconds 90 --external-millicores 0 --warmup-max <s> \
#       --stable-seconds <s> --task-loop steady
# Each window's decision records, read only: $py $H/sc014_held_047.py x <run> ... (WARMUP=1 for
# the warm-up).
```

Gates:

- Full: T456's gates per valid managed window (042's three checks): own work by SC-017 at 480,
  the saturating user at least ½ × its rate alone × 0.97. Report the duty held, the off-phase
  overlap, the frozen and `/proc`-stopped shares, and the leaves left after the run.
- SC-014: own tasks at least 97%, 94% and 84% of capacity in Performance, Neutral and Yield First,
  and with CedeGrid's own CPU stated apart. A miss changes no product code in item 047: report
  its cause from the decision records (`held_back`).

## Item 048: native checks nobody had run at HEAD

On the authorized test host only, never the shared host, with `batch-037`'s harness. Part B runs
under the user's standing approval. Part C runs under the user's typed approval of 2026-11-02,
which includes a real host-wide OOM caused by the second account. Neither part uses sudo,
installs, system settings or swap changes.

Runs that need no second account run from a host-side script under nohup, with no login during
them. A login's processes are the test account's unmanaged usage, and they restart Yield First's
admission cooldown.

Runs with the second account start its allocator at once. The allocator waits for the harness's
world-readable trigger file and grows memory only from the trigger's epoch. Before each run the
host's other processes are counted per account, in the raw evidence only. After it come
`/proc/vmstat` `oom_kill`, the kernel log where it is readable without sudo, and the leftover check.

```sh
# Part B
bash $H/partb-048.sh <commit>    # B1 launch burst, then B2 Yield First on the idle GPU, three times
#   B1 = CEDEGRID_TEST_LAUNCH_BURST=1 cargo test --locked --release --test launch_burst -- --ignored --nocapture
#   B2 = $py tools/pressure_comparison.py --root $root --output <dir> --mode gpu --gpu-uuid <GPU-UUID> \
#          --profile own-efficiency --contention-mode yield_first --execute
bash $H/partbc-048.sh <commit> b gpufill-performance gpufill-yield_first leafoom   # B3, B5
bash runs-037.sh <commit> <second account uid> b4                                  # B4
# Part C
bash $H/partbc-048.sh <commit> c1 ramfill-performance ramfill-yield_first           # C1
bash runs-037.sh <commit> <second account uid> c2 c3 c4                            # C2, C3, C4
#   each = $py $H/mem_048.py --repo $repo --run <ramfill|gpufill|reclaim|order|leafoom|oom> \
#            --mode <mode> [--class guaranteed] [--sdk] [--peer-mib N] [--all-preferred] \
#            [--trigger /tmp/cedegrid-048-<name>.json] --output <dir>
```

Gates:

- **B1, the burst stop.** 150 light tasks in Performance, then a draining stop: an operator drain,
  then TERM. A signal stop alone leaves running work to the next agent (FR-080). Every task is
  drained or released, and no gap between observations exceeds 1 s. Report the drain's duration
  and its longest gap, against the 4236 ms iteration at `fc5e330`.
- **B2, Yield First on the idle GPU.** Three times: Yield First admits and finishes the probe while
  nothing is busy on the device. Report the reason codes per interval.
- **B3, SC-014's GPU memory fill.** Performance: own GPU memory within 1% of capacity minus the
  floor. Yield First: within 1% of capacity minus the floor and its margin. Report the floor and
  the device's other contexts.
- **B4, the OOM order (FR-023).** A 3 GiB capacity. Own tasks of about 2 GiB in all sit beside
  the second account's 1.5 GiB, with no memory pressure. In-share own tasks keep their launch
  `oom_score_adj`, only the over-share tasks are at 1000, and a task back in share is lowered back.
  Then the same with `ram.prefer_own_oom_victim`.
- **B5, the own-leaf OOM (FR-076).** Full, with `cgroup.memory_max_mib = 256`. An own task asks
  for 128 MiB and takes 512 MiB, past its leaf's limit. The leaf's `memory.events` `oom_kill`
  counts. The gate: the node records the kill as specified, counts retries as FR-076 says, and
  touches nothing outside the leaf.
- **C1, SC-014's RAM fill.** Performance: own RAM within 1% of capacity minus the floor. Yield
  First: within 1% of capacity minus the floor and its margin.
- **C2, SC-012's reclaim.** After a Performance RAM fill the second account grows by 4 GiB. Own
  work frees the room and the second account gets it. Bounds, from the breach to the floor
  restored: opportunistic 10.5 s with a hint and 5.5 s without; Guaranteed 15.5 s.
- **C3, a floor breach others cause.** The second account grows by 10 GiB, about 0.7 of own
  capacity under the deployment's 16 GiB RAM floor. In Performance own
  memory gives way only down to its fair share; in Neutral and Yield First the floor comes first.
- **C4, a host-wide OOM the second account causes.** Own in-share tasks (Guaranteed, SDK; six of
  1 GiB) and one over-share task of 3 GiB launched last run in Performance. The second account then allocates until the
  kernel's OOM killer acts. Gates:
  - the in-share tasks are not the first victims;
  - an in-share task that is killed got the save-progress signal before, its kill is classified
    as caused by others, and it is requeued without using a retry;
  - the over-share task is the own task the kernel prefers;
  - CedeGrid takes no action on the second account's processes.

  Report every process the OOM killer took.

## Item 049: the gates Phase 37 touches, at its HEAD (T520)

Runs once on the authorized test host, each alone, with the leftover check after every run, under
the user's standing approval and the same host rules, with `batch-037`'s harness at the commit
that ends Phase 37's product code. Nothing runs on the shared host.

```sh
# 1. Report 027's run 1, idle, 4 CPUs, after T514, in Tracked and in Full.
bash runs-037.sh <commit> <second account uid> idle4 idle4full
# 2. Performance with two own probes beside the saturating user, in Tracked and in Full, after
#    T509 and T510; then Full with a process of the test's own account joining the first probe's
#    leaf while it is frozen, 15 s into the last managed window (same_gpu_029.py --join-at 15). The
#    joining process is the harness's own child, moved from the shell's leaf; it wakes every 5 ms
#    and writes its largest gap and any SIGCONT or SIGTERM it saw.
bash runs-037.sh <commit> <second account uid> twoperf twofull twojoin
# 3. After T519: Yield First, 4 CPUs, 16 steady tasks of 250 m; the second account's own one-core
#    loop runs 30 s from the harness's epoch.
bash runs-037.sh <commit> <second account uid> yfpeer
#   = $py $H/return_033.py --repo $repo --root $root --contender-uid <uid> --mode yield_first
#     ssh <second account>@<test host> 'T=<epoch> CPUS=<cpu> S=30 sh -s' < peer-contender-027.sh
# 4. After T512: item 029's run 3, a saturating other user on the probe's core, Performance and
#    Neutral.
bash runs-037.sh <commit> <second account uid> core
#   = $py $H/same_core_029.py --repo $repo --mode <mode> --load saturating --peer-uid <uid>
# 5. SC-014, as item 047: on the host itself under nohup, with no login until it ends.
nohup bash $H/sc014-quiet-047.sh <commit> \
  "performance:1200:0 neutral:2400:300 yield_first:4800:600" > <log> 2>&1 &
# 6. Yield First once more, with a login from each account every 60 s through its warm-up and
#    window, from outside the host.
MARKER=sc014-logins-049.done nohup bash $H/sc014-quiet-047.sh <commit> "yield_first:4800:600" > <log> 2>&1 &
bash logins-049.sh sc014-logins-049.done 3
# Read only, from each SC-014 run: held_back per window, and T513's gap.
$py $H/sc014_held_047.py x <run> ...       # WARMUP=1 for the warm-up
$py $H/sc014_gap_049.py <run> ...
```

Gates:

- Idle: the decisions' external estimate at p95 at most 1% of the 4 CPUs' capacity, in each backend.
- Two probes: T456's gates per valid managed window (042's three checks): own work by SC-017 at 480,
  the saturating user at least ½ × its rate alone × 0.97. In Full, the duty held, the off-phase
  overlap, the frozen and `/proc`-stopped shares and the leaves left.
- The join: the leaf reads frozen when the process joins, and thawed within 200 ms; it stays thawed
  for the rest of the window; `cpu.max` stays `max`; the process keeps its nice value, sees no
  SIGCONT or SIGTERM and no gap over 200 ms after the thaw. The window's own-work rate is not gated.
- Yield First beside the one-core load: own work at the contended target within 2 intervals of the
  load's first observed usage (SC-002).
- Item 029's run 3: own work at capacity minus the other user's demand minus the mode's margin, no
  pause, with the other user's p99 against alone and own throughput, with and without CedeGrid, each
  against item 029's values.
- SC-014: own tasks at least 97%, 94% and 84% of capacity in Performance, Neutral and Yield First,
  and with CedeGrid's own CPU stated apart; Yield First at least 84% with the logins too.
- T513: tasks' CPU from `/proc` against the decisions' upper bound in each SC-014 run; the code
  changes only if the tasks' CPU is above that bound.

A miss changes no product code here: report its cause from the decision records, with a fix under
"Decisions needed", marking which fixes would change a mode's semantics.

## Item 051: reruns after report 048's fixes and the 2026-11-03 answers (T532)

On the authorized test host only, never the shared host, under the user's standing approval and
item 048's host rules, with `batch-037`'s harness at item 051's last product commit. Run on
2026-11-13, after the user allowed old build trees on the host to be deleted; results are in
`docs/validation.md`, "Item 051". Each run is alone, with the leftover check after it; runs
without the second account start from a host-side script under nohup, with no login during them.

```sh
# B1, the burst stop, after the draining stop's share counts two journal writes (T521).
nohup bash $H/partb-051.sh <commit> b1 > <log> 2>&1 &
# B2, Yield First on the idle GPU, three comparisons with a 180 s readiness wait: once at the
# commit of T526, once at the commit of T530, so the two effects can be told apart.
TAG=a READINESS=180 nohup bash $H/partb-051.sh <commit of T526> b2 > <log> 2>&1 &
TAG=b READINESS=180 nohup bash $H/partb-051.sh <commit of T530> b2 > <log> 2>&1 &
#   each = $py tools/pressure_comparison.py --root $root --output <dir> --mode gpu --gpu-uuid <GPU-UUID> \
#            --profile own-efficiency --contention-mode yield_first --readiness-seconds 180 --execute
# B3 and C1, SC-014's memory fill as restated on 2026-11-13. SLACK is what the last fill task
# leaves, below 1% of the smallest target. A GPU fill task allocates its request less the 72 MiB
# its CUDA context takes on this device; with 80 MiB less, B3's first run fell 8 MiB a task short.
SLACK=8 bash $H/partbc-048.sh <commit> b gpufill-performance gpufill-yield_first
SLACK=8 bash $H/partbc-048.sh <commit> c1 ramfill-performance ramfill-yield_first
# C4 run 2: the over-share task ignores hints, the in-share tasks read them.
bash runs-037.sh <commit> <second account uid> c4b
#   = $py $H/mem_048.py --repo $repo --run oom --mode performance --class guaranteed \
#       --over-no-sdk --in-sdk --trigger /tmp/cedegrid-048-<name>.json --output <dir>
```

Gates:

- **B1.** As item 048's: every task drained or released, and no gap between observations over
  1 s. Report the drain's duration, its longest iteration and the drains per iteration, against
  9.3 s and 1.135 s at `9c01943`.
- **B2.** Each of the three managed probes is admitted and finishes while nothing but the
  greeter's context is on the device. Report, per comparison, the time from the probe's
  submission to its admission, the reason codes per interval and the named failed check wherever
  activity reads unknown (T525). At T526's commit a device the collector found quiet admits
  without waiting for a driver sample; the 30 s GPU memory cooldown after the compute admission
  opens is expected and is reported apart.
- **B3 and C1.** Own GPU memory and own RAM within 1% of capacity less the floor, less others'
  use and their peak, and less Yield First's margin in that mode; on the device, others' use
  includes used memory that no process accounts for. Report the target's terms from the decision
  records, the fill as charged and as observed, and the harness's slack apart.
- **C4 run 2.** The over-share task is asked first and alone. Being Guaranteed and deaf to
  hints, it is drained when the breach window ends or sent TERM when the breach turns critical.
  The in-share tasks get no hint for its excess and keep their memory, and the task is admitted
  again once its memory fits. Report the time the floor stays breached, against item 048's run,
  every step by task, and every process the OOM killer took.

A miss changes no product code here: report its cause from the decision records, with a fix under
"Decisions needed".

## Item 052: the same-core loss again

On the authorized test host only, under the user's standing approval and item 049's host rules,
with `batch-037`'s harness. Each run is item 029's run 3 alone, with the leftover check after it;
`same_core_029.py` records, per window and read-only from `/proc`, the probe's user and system
ticks, minor faults, run time and the byte lengths of its environment and arguments.

```sh
# Item 049's two repeats of run 3, kept here for the record (report 049's analyze C1):
bash runs-037.sh <commit> <second account uid> corep    # Performance alone, as e049-r3b-performance
bash runs-037.sh <commit> <second account uid> corec    # Performance, then Neutral, as e049-r3c-<mode>
# Item 052: the names' lengths swapped. Performance runs as e052-r3-perf-16c (16 characters, Neutral's
# length), Neutral as e052-r3-neutral-20ch (20 characters, Performance's length).
bash runs-037.sh <commit> <second account uid> coreswap
#   each = $py $H/same_core_029.py --repo $repo --mode <mode> --load saturating --peer-uid <uid>
#     ssh <second account>@<test host> 'T=<epoch> S=230 CPUS=<cpu> KIND=saturating RATE=100 sh -s' < peer-load-029.sh
# Controls made on 2026-11-13: `corec` at HEAD, `corep` at 743312b, and `corep` at a test-only
# copy of 743312b's build tree with HEAD's python/cedegrid/worker.py and process.py.
# Item 050's owed rerun, with the caps read from the report's per-interval detail:
bash runs-037.sh <commit> <second account uid> yfpeer
# Read only: per window, CPU per unit and the other user's one-preemption latency modes.
python3 $H/peer_modes.py <run directory> <second account's latencies>
```

Gates: none of its own. The runs decide between causes: the loss follows the name's length, the
mode, or the commit. Report, per window, the managed-to-unmanaged ratio, the probe's CPU per unit
and the other user's one-preemption latency modes. Item 029's own gate for run 3 stays as stated
under "Item 049".

## Item 053: own GPU processes in the counts, lifts in every phase, Yield First beside a principal (T542)

On the authorized test host only, under the user's standing approval and item 049's host rules,
with `batch-037`'s harness, at the item's final commit. A leftover check of both accounts follows
each run.

```sh
# Part A: two probes beside the saturating user. Performance in Tracked, Performance in Full,
# Neutral in Tracked; each has two managed windows.
bash runs-037.sh <commit> <second account uid> twoperf twofull twoneutral
# Part B: the join run in Full. A process of the test's own account joins the first probe's
# leaf while it is frozen.
bash runs-037.sh <commit> <second account uid> twojoin
# Part B, then: the same join, and the first probe's task cancelled at once after it
# (--join-cancel); the leaf is read 3 s after the probe has gone.
bash runs-037.sh <commit> <second account uid> twojoincancel
# Part C: SC-015's shape in Yield First on 4 CPUs (250 m steady tasks) beside the second account's
# steady light load on the same CPUs (300 requests/s for 2400 s), with an owner login every 60 s
# through the warm-up and the window and no other login. The host side is sc015-peer-053.sh,
# which writes each decision's CPU admission and the listed users' usage to timeline.jsonl.
bash peer-run-053.sh <commit> 0,2,4,6 300 2400 <local dir>
```

Gates:
- Part A: T456's, with 042's validity checks (`m2_decisions` at least 75 of 80) and 047's
  off-phase measure. Report `m2_decisions` and the time from the second probe's first listing to
  the first duty.
- Part B: as in item 049: thawed within 50 ms, no signal, `cpu.max` unchanged. Join then cancel:
  after the supervisor exits, `cgroup.freeze` reads 0 or the leaf is gone, and no process is left
  frozen. The cancel goes through the coordinator, so where in the drain the join lands is not
  chosen; `tests/cgroup.rs` carries the lift after a drain, after the leader's exit and on the
  error path.
- Part C: SC-015's 90% of capacity less that user and the margin, with no cap or pause cycling.
  Report every cooldown restart in the warm-up with its cause, and the share of decisions with
  admission open. A restart whose cause is a login is a miss.

## Item 054: the loop without pacing waits, the over-share ask, the owner's GPU use (T556)

On the authorized test host only, under the user's standing approval and item 049's host rules,
with `batch-037`'s harness at the item's last product commit. A leftover check of both accounts
follows each run; the B1 runs start from a host-side script with no login during them.

```sh
# B1, the burst stop, at the test's 1 MiB/s and at the default 10 MiB/s, then once more at 1 MiB/s
# with each control request timed (CEDEGRID_RPC_EVIDENCE=1 gives control requests a 10 s
# deadline, so that run is a record, not a gate).
ITEM=054 nohup bash $H/partb-051.sh <commit> b1 > <log> 2>&1 &
ITEM=054 BURST_BYTES=10485760 nohup bash $H/partb-051.sh <commit> b1 > <log> 2>&1 &
ITEM=054 RPC=1 nohup bash $H/partb-051.sh <commit> b1 > <log> 2>&1 &
#   each = CEDEGRID_TEST_LAUNCH_BURST=1 CEDEGRID_TEST_BURST_TRANSFER_BYTES=<bytes> \
#            cargo test --locked --release --test launch_burst -- --ignored --nocapture
# C4 run 2 and C3's floor breach in Neutral.
bash runs-037.sh <commit> <second account uid> c4b c3n
# Part E: one of B2's comparisons in Yield First while the second account holds a context on the
# device: idle (one multiply, then nothing), then computing.
bash parte-mac-054.sh <commit> idle light 0.001 600 180 <local dir>
bash parte-mac-054.sh <commit> computing saturating 200 300 60 <local dir>
#   host side: parte-054.sh, which runs tools/pressure_comparison.py --mode gpu \
#     --profile own-efficiency --contention-mode yield_first --readiness-seconds <s> --execute
# Part F: SC-015's shape in Yield First (4 CPUs, 250 m steady tasks, a 150 s window) beside the
# second account's steady light load; each minute the owner's account runs a one-core CPU burst for
# 1.5 s on the run's first CPU, and 90 s into the window the second account adds a second load
# beyond the tolerance for 60 s. Host side: sc015-peer-054.sh.
bash peer-run-054.sh <commit> 0,2,4,6 300 2400 900 <local dir>
```

Gates:
- B1, both runs: every gap between observations within 1 s, every task drained or released, and
  no run ended by lease expiry after the drain. Report each phase's longest gap and the
  heartbeat's time with 150 allocations (from the timed run).
- C4 run 2: as in item 051. C3 in Neutral: the floor restored within item 048's time.
- Part E: beside the idle context, admission opens after the quiet window while the owner's probe
  is busy; beside the computing one, it holds.
- Part F: no cooldown restart whose cause is the owner's burst, with each interval own work did not
  fit and its cause listed; after the raise the cooldown restarts and Yield First steps within
  SC-002's two intervals.

## Item 055: the same-core probe checks itself (T560)

On the authorized test host only, under the user's standing approval and item 049's host rules,
with `batch-037`'s harness. Each run is item 029's run 3 alone, with the leftover check after it.
`same_core_029.py` takes `--probe` (a probe file other than the build's own), `--probe-env
KEY=VALUE` (added to both probes' environments) and `--probe-holds N` (the probe's
`--self-check-holds`), and reports each window's verdict, the refused windows and the ratio over
the measured ones.

```sh
# Step 1, the mechanism: the kept 743312b build, the allocator's per-thread cache off in both
# probes, item 055's probe only measuring its even/odd units.
bash runs-037.sh 743312b <second account uid> coretc
#   = $py $H/same_core_029.py --repo $repo --mode performance --load saturating --peer-uid <uid> \
#       --probe $H/probe055/pressure_probe.py --probe-env GLIBC_TUNABLES=glibc.malloc.tcache_count=0 --probe-holds 0
# Step 4, the gate: run 3 in Performance and in Neutral at the item's commit (its own probe).
bash runs-037.sh <commit> <second account uid> corec
# And once in Performance on 743312b with item 055's probe, which removes or refuses a bias.
bash runs-037.sh 743312b <second account uid> corep055
```

Gates: at the item's commit, own throughput with CedeGrid at least the mode's bound of without it
(item 029's run 3), with no refused window. On `743312b` the new probe must either pass or refuse
the windows as biased; a clean-looking 0.945 means the self-check misses the case it was built
for.

## C. Deferred gates that need later work

| Task | Needs | Command once that code exists |
|---|---|---|
| T054 cgroup gate | The native test `runtime_adjust_requires_verified_membership`, which T054 adds to `tests/cgroup.rs` | `CEDEGRID_TEST_CGROUP_ROOT=<delegated-subtree> cargo test --locked --test cgroup -- --ignored --exact runtime_adjust_requires_verified_membership`, then quickstart §4 |
| GPU compute pause and duty cycle | T087d | With T088: pause a test-owned CUDA job and record that submissions stop and its VRAM stays allocated (research R4); then duty-cycle it and record the share's granularity against the monitor interval and the longest kernel. |
| T138 RAM cap gate | An authorized Linux host with a delegated subtree whose `cgroup.subtree_control` enables `memory` | `CEDEGRID_TEST_CGROUP_ROOT=<delegated-subtree> cargo test --locked --test cgroup -- --ignored --exact authorized_native_ram_cap_reads_back_memory_high`: a test-owned child joins the leaf the backend created, the RAM cap writes `memory.high` and reads it back (at least the 1024 MiB target, never below what the leaf cannot reclaim), and reversal restores `max`. Record the readback, and with swap off, that the cap stayed at or above anon + unevictable + 256 MiB from `memory.stat` |
| T141 PSI gate | An authorized Linux host with cgroup v2 and PSI, and a second account or another slice for the external load | (1) `CEDEGRID_TEST_PSI=1 cargo test --locked --test kernel -- --ignored --exact native_external_cpu_pressure_is_read`, recording `status`, `external_cgroups` and the three avg10 values; (2) with a CPU-bound external load in another top-level slice or another user's slice, `cedegrid --config <node.toml> observe --no-state --samples 30`, recording `cpu_pressure.external_some_avg10` rising above `psi_some_percent` and `saturated_by_psi` on CPU while busy stays below `saturation_percent`; (3) with no external load and a test-owned job running more threads than its share, the same `observe`, recording own PSI high and no `saturated_by_psi` |
| T088 GPU gate | US5 (T083–T087) | Quickstart §5 with `CEDEGRID_TEST_GPU_UUID=<GPU-UUID>` and an idle external context, in Neutral and in Performance, including 100 consecutive launches. Also record the GPU memory a fresh external CUDA context takes (`nvidia-smi --query-compute-apps=pid,used_memory --format=csv` while `"$CEDEGRID_TEST_PYTHON" -c 'import torch,time; torch.ones(1,device="cuda"); time.sleep(60)'` runs), and raise the 512 MiB minimum if it is larger. Over a real session, record how often GPU activity is Unknown (the share of samples whose `gpu_compute` measurement or activity is `unknown` in `cedegrid observe` output). Commands: (1) the owned-context test, `CEDEGRID_TEST_GPU_UUID=<GPU-UUID> CEDEGRID_TEST_PYTHON=<python-with-cuda-torch> CEDEGRID_GPU_EVIDENCE="$HOME/.cache/cedegrid-evidence/t088-owned.json" cargo test --locked --test gpu_native -- --ignored --exact owned_cuda_context_is_observed_then_released_after_drain`; (2) the idle external context, `CUDA_VISIBLE_DEVICES=<GPU-UUID> "$CEDEGRID_TEST_PYTHON" -c 'import torch,time; torch.ones(1,device="cuda"); time.sleep(3600)' &`; (3) with that context alive, `cedegrid --config <node.toml> observe --no-state` under `[contention.gpu_compute] mode = "neutral"` and then `"performance"`, recording that the device's `gpu_memory` and `gpu_compute` admission is `open`, then 100 GPU tasks through a coordinator as in `docs/coordinator-work.md`. (4) for T152, with 1, 2 and 4 concurrent test-owned CUDA processes, record summed per-process `sm_util` from `nvidia-smi pmon -c 30 -s u` beside device utilization from `nvidia-smi --query-gpu=utilization.gpu --format=csv -lms 500`, and set `attribution_tolerance_percent` from the largest gap; over a real session, record how often devices report `compute_path` unknown with an inconsistency reason (`cedegrid observe --no-state`). US5 added no hardware test: its admission, duty-cycle and placement rules are covered by `tests/policy.rs`, `tests/contention.rs`, `tests/coordinator.rs` and the actuation unit tests. |
| T117 thresholds, with SC-014/SC-015 | An authorized Linux host; the runner is ready (T153) | See "T117 and T118" below |
| T118 pressure audit | An authorized Linux host; the runner is ready (T153) | See "T117 and T118" below |

### T117 and T118

**Host.** One authorized Linux host, prepared as `docs/comparison.md` describes for
`tools/pressure_comparison.py`. It needs the release binary in the checkout, the isolated
virtual environment under `$cedegrid_validation_root`, cgroup v2, at least 4 online CPUs (the
runner pins 2 for CPU runs and 3 for GPU runs), 16 GiB of available RAM and 16 GiB of free disk.
It also needs one NVIDIA GPU with at least 4 GiB free for the GPU run; the runner takes its UUID
for CPU runs too. Read access to the kernel log is needed for the OOM check (`journalctl -k`
works for members of `adm` or `systemd-journal`). Nothing else may run on the pinned CPUs.

**The runner (T153).** `tools/pressure_comparison.py` takes `--contention-mode
{yield_first,neutral,performance}` (Neutral when unset in this, the protection profile) and
`--ram-floor-mib N` (a floor of exactly N MiB, at least 512, with `small_floor` acknowledged
below 1024). Each run's `report.json` carries:

- per managed contention case, `step_timing` from `cedegrid history`: per resource, the seconds
  from the demand onset to the first step on the probe (`first_step_seconds`), to own usage at
  or below the target share (`reclaim_seconds`), and from the demand's end to the step's
  reversal (`reversal_seconds`), with how often each step began (`steps`). A graduated step
  counts as a yield, and its post-yield cutoff is the reclaim, so `release_p95_seconds_max` is
  `not_applicable` when no repetition drained (SC-007 expects no CPU drain);
- `sc005`: the `cedegrid history` audit (`actions_outside_own_allocations` and
  `drains_outside_own_allocations`, both 0) and whether every protected snapshot was unchanged.
  Each protected process is read before and while it runs: its nice value, start time, cgroup
  path, and that cgroup's `cpu.max`, `memory.high` and `memory.max`. Any change, or an action
  outside own allocations, fails the run.

Every command below uses this setup and a new output directory:

```sh
cargo build --locked --release
cedegrid_validation_root="$HOME/.local/share/cedegrid-validation"
gate() { # gate <name> <cpu|gpu> [runner options...]
  name="$1"; mode="$2"; shift 2
  run="$cedegrid_validation_root/evidence/$name-$(date -u +%Y%m%dT%H%M%SZ)"
  "$cedegrid_validation_root/venv-isolated/bin/python" tools/pressure_comparison.py \
    --root "$cedegrid_validation_root" --output "$run" --mode "$mode" \
    --gpu-uuid "$CEDEGRID_VALIDATION_GPU_UUID" "$@" --execute
  journalctl -k --since "@$(python3 -c 'import json,sys;print(int(json.load(open(sys.argv[1]))["started_unix"]))' "$run/report.json")" \
    | grep -c -i -e 'out of memory' -e 'oom-kill'
}
```

**T117 commands.** One per mode, at the default floor; GPU for Neutral and Performance:

```sh
gate t117-cpu-yield_first cpu --contention-mode yield_first
gate t117-cpu-neutral cpu --contention-mode neutral
gate t117-cpu-performance cpu --contention-mode performance
gate t117-gpu-neutral gpu --contention-mode neutral
gate t117-gpu-performance gpu --contention-mode performance
```

Compare each resource's `first_step_seconds`, `reclaim_seconds` and `reversal_seconds` with
SC-002–SC-004 and SC-012, and check `steps` for no CPU drain (SC-007). For SC-014, run
`./target/release/cedegrid --config "$run/node.toml" observe --no-state --samples 120` while the
managed probe runs alone, and record own CPU usage against own target for each mode. For
SC-015, run the probe beside a steady external user of 1–2% of one CPU for 5 minutes. Use five
consecutive 60 s `protected_cpu` events in one `tools/pressure.py` file, at 0, 60, 120, 180 and
240 s, with `cpu_iterations` calibrated so that `observe` shows 1–2% external CPU. Record own
5-minute average usage, and the number of cap and pause steps from `cedegrid history`. Record
each result in `docs/validation.md`, and change defaults in `src/config.rs` only with that
evidence.

**T118 commands.** At the defaults, then with Performance and a confirmed near-minimum floor:

```sh
gate t118-cpu-defaults cpu
gate t118-gpu-defaults gpu
gate t118-cpu-performance-floor cpu --contention-mode performance --ram-floor-mib 512
gate t118-gpu-performance-floor gpu --contention-mode performance --ram-floor-mib 512
```

Each run passes SC-005 only with `report.json` `status` `completed_pending_metric_review`,
`sc005.history_audit` showing 0 actions and 0 drains outside own allocations,
`sc005.protected_snapshots_unchanged` true, only `completed` protected statuses (a failed GPU
allocation fails the protected event), and an OOM count of 0 from the `journalctl` line. The
actuation side of the audit is `runtime_steps_reach_only_the_verified_family` in the Rust suite.
