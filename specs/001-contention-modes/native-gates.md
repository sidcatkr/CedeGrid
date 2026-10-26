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
{yield_first,neutral,performance}` (Performance is acknowledged as a caution option) and
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
