# Native gates deferred from batches 1 and 2

Batch 1 (T001–T032) and batch 2 (US2, T033–T055) were implemented and verified on macOS only.
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

## B. Batch 2 (US2) checks that can run now

1. Compile and lint the Linux-only paths. `CgroupActuator` over the pinned leaf descriptors
   (`LeafHandle`) and `CgroupBackend::actuator` have never been compiled, because macOS has no
   Linux C toolchain for the dependencies:

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

## C. Deferred gates that need later work

| Task | Needs | Command once that code exists |
|---|---|---|
| T054 cgroup gate | The native test `runtime_adjust_requires_verified_membership`, which T054 adds to `tests/cgroup.rs` | `CEDEGRID_TEST_CGROUP_ROOT=<delegated-subtree> cargo test --locked --test cgroup -- --ignored --exact runtime_adjust_requires_verified_membership`, then quickstart §4 |
| GPU compute pause | US5 (T083–T087) | With T088: pause a test-owned CUDA job and record that submissions stop and its VRAM stays allocated (research R4). |
| T088 GPU gate | US5 (T083–T087) | Quickstart §5 with `CEDEGRID_TEST_GPU_UUID=<GPU-UUID>` and an idle external context, in Neutral and in Performance, including 100 consecutive launches. T083–T087 add the exact tests. |
| T117 thresholds | US2 and US3 | Validate SC-002–SC-004 with test-owned loads, and change defaults in `src/config.rs` only with that evidence. |
