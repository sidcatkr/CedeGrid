# Native gates deferred from batches 1 to 4

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

## C. Deferred gates that need later work

| Task | Needs | Command once that code exists |
|---|---|---|
| T054 cgroup gate | The native test `runtime_adjust_requires_verified_membership`, which T054 adds to `tests/cgroup.rs` | `CEDEGRID_TEST_CGROUP_ROOT=<delegated-subtree> cargo test --locked --test cgroup -- --ignored --exact runtime_adjust_requires_verified_membership`, then quickstart §4 |
| GPU compute pause | US5 (T083–T087) | With T088: pause a test-owned CUDA job and record that submissions stop and its VRAM stays allocated (research R4). |
| T088 GPU gate | US5 (T083–T087) | Quickstart §5 with `CEDEGRID_TEST_GPU_UUID=<GPU-UUID>` and an idle external context, in Neutral and in Performance, including 100 consecutive launches. T083–T087 add the exact tests. |
| T117 thresholds | US2 and US3 | Validate SC-002–SC-004 and the SC-012 reclaim bounds with test-owned loads, and change defaults in `src/config.rs` only with that evidence. |
| GPU compute duty cycle | T087d | Duty-cycle a test-owned CUDA job and record the share's granularity against the monitor interval and the longest kernel. |
| Runtime `memory.high` | T138 | With `CEDEGRID_TEST_CGROUP_ROOT`, apply a RAM cap step to a test-owned leaf and read it back. |
