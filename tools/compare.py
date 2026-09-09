#!/usr/bin/env python3
"""Controlled synthetic CPU comparison. Default: print the protocol, execute nothing.

Native Linux execution is deliberately opt-in and needs an operator-provided execution-enabled rootless profile and an optional
authorized cgroup profile. This tool never enables controllers, invokes sudo,
or alters existing profiles. Retained artifacts include state needed to reconcile
an interrupted allocation. This is not a shared-server or 24-hour soak test.
"""
from __future__ import annotations

import argparse
import copy
import json
import math
import os
from pathlib import Path
from runtime_config import atomic_runtime_config
import subprocess
import sys
import tempfile
import time
import uuid


def quantile(values, probability):
    """Linear interpolation over ordered observations; absent data stays absent."""
    if not 0 <= probability <= 1:
        raise ValueError("probability must be in [0, 1]")
    samples = sorted(float(v) for v in values if v is not None and math.isfinite(v))
    if not samples:
        return None
    index = (len(samples) - 1) * probability
    lower, upper = math.floor(index), math.ceil(index)
    return samples[lower] + (samples[upper] - samples[lower]) * (index - lower)


def slowdown(observed, alone):
    if observed is None or alone is None or alone <= 0 or not math.isfinite(observed) or not math.isfinite(alone):
        return None
    return observed / alone - 1.0


def protocol(args):
    return {
        "schema_version": 1,
        "mode": "plan_only",
        "execution_authorized": False,
        "linux_runtime_verified": False,
        "repetitions": args.repetitions,
        "warmup_seconds": args.warmup,
        "measurement_seconds": args.measure,
        "seed": args.seed,
        "sequence_per_repetition": [
            "protected-alone reference",
            "managed-idle unmanaged/rootless and optional cgroup, deterministic balanced order",
            "managed+protected contention unmanaged/rootless and optional cgroup, same paired order",
        ],
        "workload": {
            "managed": "single process, continuous fixed-size arithmetic batches",
            "protected": "single process, one arithmetic request every 20 ms; latency includes scheduler delay",
            "placement": "both synthetic workers pinned to one explicitly selected allowed CPU; manager remains unpinned",
            "synchronization": "ready files then common monotonic start; warmup excluded from measurements",
        },
        "requirements": [
            "--execute on Linux with existing explicitly authorized execution-enabled profiles",
            "rootless baseline; optional cgroup profile requires configured/authorized cpu.weight and no fallback",
            "identical non-backend policy settings; existing profiles remain unchanged",
            "guaranteed node profile or explicitly configured lease covering the experiment",
            "read-only validate and doctor; unsupported or unpermitted controls fail execution",
        ],
        "recorded": [
            "managed batches/s; protected completed requests/s, p95/p99 and slowdown against protected-alone",
            "manager-only /proc CPU time and sampled peak RSS; worker CPU accounted separately",
            "system and worker-cgroup PSI plus CPU throttling counters before/after measurement",
            "effective affinity, niceness, /proc cgroup membership, ancestor limits, competitor placement",
            "supervisor drain/termination/exit/release timestamps where exposed; missing events remain null",
            "raw reports, config overrides, exact commands, errors, and retained reconciliation state",
        ],
        "configuration_overrides": [
            "state_dir points to a fresh retained case directory",
            "gpu.scale_up_cooldown_ms=0 for controlled CPU admission; explicitly recorded per case",
        ],
        "pre_registered_targets": {"idle_managed_throughput_ratio_min": .9, "protected_p99_ratio_max": 1.2,
            "manager_mean_cpu_cores_max": .5, "manager_peak_rss_bytes_max": 512*1024**2},
        "max_wall_seconds": args.max_wall_seconds,
        "limits": [
            "cpu.weight is relative and hierarchical; no percentage or allocation guarantee",
            "this CPU comparison does not verify GPU yielding, distributed recovery, or the 24-hour soak",
            "local CLI currently provides lease escalation, not contention-to-yield decision instrumentation",
            "system/rootless shared-cgroup pressure does not identify external workload causation",
            "no speedup is inferred from CPU utilization; protection can reduce opportunistic throughput",
        ],
    }


def write_json(path, value):
    path = Path(path).expanduser().resolve()
    path.relative_to(Path.home().resolve())
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp-" + uuid.uuid4().hex)
    try:
        with temporary.open("x") as output:
            output.write(json.dumps(value, indent=2, allow_nan=False) + "\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        fd=os.open(path.parent,os.O_RDONLY)
        try: os.fsync(fd)
        finally: os.close(fd)
    finally:
        temporary.unlink(missing_ok=True)


def read_text(path):
    try:
        return {"value": Path(path).read_text(), "error": None}
    except OSError as error:
        return {"value": None, "error": str(error)}


def cgroup_directory():
    """Resolve this process's visible v2 mount without assuming its mountpoint."""
    membership = Path("/proc/self/cgroup").read_text()
    entries = [line[3:] for line in membership.splitlines() if line.startswith("0::")]
    if len(entries) != 1 or ".." in Path(entries[0]).parts:
        return None
    own = Path(entries[0])
    candidates = []
    def unescape(value):
        for code, char in [("040", " "), ("011", "\t"), ("012", "\n"), ("134", "\\")]:
            value = value.replace("\\" + code, char)
        return value
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        before, separator, after = line.partition(" - ")
        fields = before.split()
        if separator and after.split()[0] == "cgroup2" and len(fields) >= 5:
            root, mount = Path(unescape(fields[3])), Path(unescape(fields[4]))
            if own.is_relative_to(root):
                candidates.append((len(root.parts), mount / own.relative_to(root)))
    return max(candidates, default=(0, None), key=lambda value: value[0])[1]


def pressure_snapshot():
    result = {"monotonic_ns": time.monotonic_ns(), "system": {}, "workload_cgroup": None}
    for resource in ["cpu", "memory", "io"]:
        result["system"][resource] = read_text(Path("/proc/pressure") / resource)
    try:
        group = cgroup_directory()
        if group is not None:
            result["workload_cgroup"] = {
                "scope": str(group),
                "counters": {name: read_text(group / name) for name in [
                    "cpu.stat", "cpu.pressure", "memory.pressure", "io.pressure",
                    "memory.current", "memory.events", "cpu.weight", "cpu.max",
                    "cpuset.cpus.effective",
                ]},
            }
    except (OSError, ValueError) as error:
        result["cgroup_error"] = str(error)
    return result


def work_batch(value, count):
    for _ in range(count):
        value = ((value * 1664525) + 1013904223) & 0xFFFFFFFF
    return value


def measurement_eligible(mode, batch_start, finished, scheduled_arrival, measured_start, end):
    """Exclude warmup arrivals even when their service spills into measurement."""
    arrival = scheduled_arrival if mode == "protected" else batch_start
    return arrival >= measured_start and batch_start >= measured_start and finished <= end


def worker(args):
    if not sys.platform.startswith("linux"):
        raise RuntimeError("synthetic execution requires Linux")
    os.sched_setaffinity(0, {args.cpu})  # Only this harness-owned worker.
    if os.getpriority(os.PRIO_PROCESS, 0) != args.nice:
        os.setpriority(os.PRIO_PROCESS, 0, args.nice)
    placement = {
        "pid": os.getpid(), "allowed_cpus": sorted(os.sched_getaffinity(0)),
        "nice": os.getpriority(os.PRIO_PROCESS, 0), "cgroup": read_text("/proc/self/cgroup"),
    }
    write_json(args.ready, placement)
    wait_until = time.monotonic() + args.ready_timeout
    while not Path(args.start).exists():
        if time.monotonic() >= wait_until:
            raise RuntimeError("no authorized benchmark start arrived before ready deadline")
        time.sleep(0.01)
    start = json.loads(Path(args.start).read_text())["monotonic_start"]
    time.sleep(max(0.0, start - time.monotonic()))
    measured_start, end = start + args.warmup, start + args.warmup + args.measure
    value, count, latencies = args.seed, 0, []
    pressure_before = None
    cpu_before = None
    next_request = start
    while time.monotonic() < end:
        if args.worker == "protected":
            time.sleep(max(0.0, next_request - time.monotonic()))
        batch_start = time.monotonic()
        if batch_start >= end:
            break
        if batch_start >= measured_start and pressure_before is None:
            pressure_before, cpu_before = pressure_snapshot(), time.process_time()
        value = work_batch(value, 4_000 if args.worker == "protected" else 20_000)
        finished = time.monotonic()
        if measurement_eligible(args.worker, batch_start, finished, next_request, measured_start, end):
            count += 1
            if args.worker == "protected":
                latencies.append((finished - next_request) * 1000)
        if args.worker == "protected":
            next_request += 0.020
    outcome = {
        "placement": placement, "mode": args.worker, "completed_batches": count,
        "throughput_per_second": count / args.measure,
        "p95_ms": quantile(latencies, .95), "p99_ms": quantile(latencies, .99),
        "latency_samples": len(latencies), "worker_cpu_seconds": None if cpu_before is None else time.process_time() - cpu_before,
        "pressure_before": pressure_before, "pressure_after": pressure_snapshot(),
        "checksum": value,
    }
    write_json(args.result, outcome)


def manager_sample(pid):
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
        fields = text.rsplit(")", 1)[1].split()
        return {
            "monotonic_ns": time.monotonic_ns(),
            "cpu_seconds": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
            "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
        }
    except (OSError, ValueError, IndexError):
        return None


def counter_deltas(before, after):
    """Retain exact scope; unavailable/reset counters are not zero activity."""
    if before is None or after is None:
        return None
    def values(raw):
        parsed = {}
        for line in (raw.get("value") or "").splitlines():
            fields = line.split()
            if len(fields) == 2 and fields[1].isdigit():
                parsed[fields[0]] = int(fields[1])
            elif len(fields) > 2:
                for field in fields[1:]:
                    if field.startswith("total=") and field[6:].isdigit():
                        parsed[fields[0] + ".total_us"] = int(field[6:])
        return parsed
    def delta(a, b):
        if a.get("value") is None or b.get("value") is None:
            return None
        a, b = values(a), values(b)
        return {key: None if key not in a or key not in b or b[key] < a[key] else b[key] - a[key] for key in a.keys() | b.keys()}
    result = {"elapsed_ms": (after["monotonic_ns"] - before["monotonic_ns"]) / 1e6, "system": {}, "workload_cgroup": None}
    for resource in ["cpu", "memory", "io"]:
        result["system"][resource] = delta(before["system"][resource], after["system"][resource])
    # System CPU full is not a meaningful zero-pressure signal.
    if result["system"]["cpu"] is not None:
        result["system"]["cpu"].pop("full.total_us", None)
    a, b = before.get("workload_cgroup"), after.get("workload_cgroup")
    if a and b and a["scope"] == b["scope"]:
        result["workload_cgroup"] = {"scope": a["scope"], "counters": {key: delta(a["counters"][key], b["counters"][key]) for key in ["cpu.stat", "cpu.pressure", "memory.pressure", "io.pressure", "memory.events"]}}
    return result


def summarize(cases):
    """Report variability across the paired cases without asserting speedup."""
    groups = {}
    for case in cases:
        if case.get("status") != "completed" or "managed" not in case:
            continue
        name = case["scenario"] + ":" + case["profile"]
        metrics = groups.setdefault(name, {})
        values = {
            "managed_batches_per_second": case["managed"]["throughput_per_second"],
            "protected_p95_ms": case.get("protected", {}).get("p95_ms"),
            "protected_p99_ms": case.get("protected", {}).get("p99_ms"),
            "protected_p99_slowdown_fraction": case.get("protected_p99_slowdown_fraction"),
            "manager_cpu_seconds": case.get("manager_cpu_seconds"),
            "manager_peak_rss_bytes": case.get("manager_peak_rss_bytes"),
            "drain_to_release_ms": case.get("drain_to_release_ms"),
            "release_confirmation_delay_ms": case.get("release_confirmation_delay_ms"),
        }
        for metric, value in values.items():
            metrics.setdefault(metric, [])
            if value is not None:
                metrics[metric].append(value)
    return {
        group: {metric: {"count": len(values), "median": quantile(values, .5), "p10": quantile(values, .1), "p90": quantile(values, .9), "min": min(values, default=None), "max": max(values, default=None)} for metric, values in metrics.items()}
        for group, metrics in groups.items()
    }



def evaluate_targets(cases, targets):
    """Evaluate pre-registered goals; absent comparisons remain pending."""
    def median(scenario, profile, field):
        values=[]
        for case in cases:
            if case.get("status")=="completed" and case["scenario"]==scenario and case["profile"]==profile:
                value=case
                for key in field:
                    value=value.get(key) if isinstance(value,dict) else None
                if value is not None: values.append(value)
        return quantile(values,.5)
    unmanaged=median("idle","unmanaged",["managed","throughput_per_second"])
    rootless=median("idle","rootless",["managed","throughput_per_second"])
    throughput=None if not unmanaged or rootless is None else rootless/unmanaged
    # The protected workload's own matched-alone reference is the baseline.
    # Comparing against an already-slow unmanaged contention run can falsely
    # pass severe interference merely because the unmanaged case was worse.
    plain_tail=median("protected_alone","rootless",["protected","p99_ms"])
    managed_tail=median("contention","rootless",["protected","p99_ms"])
    tail=None if not plain_tail or managed_tail is None else managed_tail/plain_tail
    cpu=median("idle","rootless",["manager_mean_cpu_cores"])
    rss=max((case.get("manager_peak_rss_bytes") or 0 for case in cases if case.get("profile")=="rootless"),default=0) or None
    readings={"idle_managed_throughput_ratio_min":throughput,"protected_p99_ratio_max":tail,
              "manager_mean_cpu_cores_max":cpu,"manager_peak_rss_bytes_max":rss}
    return {key:{"observed":value,"target":targets[key],"status":"pending" if value is None else
        ("passed" if (value>=targets[key] if key.endswith("_min") else value<=targets[key]) else "failed")}
        for key,value in readings.items()}


def checked_json(command):
    completed = subprocess.run(command, capture_output=True, text=True, timeout=60, check=False)
    if completed.returncode:
        raise RuntimeError(f"read-only preflight failed: {command!r}: {completed.stderr.strip()}")
    return json.loads(completed.stdout)


def run_case(args, directory, profile, profile_name, cpu, scenario, seed):
    directory.mkdir()
    config = copy.deepcopy(profile)
    execution_id = "comparison-" + uuid.uuid4().hex
    config["state_dir"] = str(directory / "state")
    original_cooldown = config["gpu"]["scale_up_cooldown_ms"]
    config["gpu"]["scale_up_cooldown_ms"] = 0
    config_path, job_path = directory / "config.toml", directory / "job.json"
    atomic_runtime_config(config_path, config, 'node')
    case = {
        "scenario": scenario, "profile": profile_name, "artifact_directory": str(directory),
        "execution_id": execution_id,
        "config_overrides": {"state_dir": config["state_dir"], "gpu.scale_up_cooldown_ms": {"before": original_cooldown, "after": 0}},
        "doctor_before": checked_json([args.binary, "--config", str(config_path), "doctor"]),
        "commands": [], "status": "running", "manager_samples": [],
        "yield_decision_latency_ms": None,
        "yield_decision_unavailable_reason": "No contention-event timestamp in this local execution interface.",
    }
    start_file = directory / "start.json"
    processes, streams = {}, []
    deadline = time.monotonic() + config["execution"]["admission_timeout_ms"] / 1000 + config["execution"]["prepare_timeout_ms"] / 1000 + args.warmup + args.measure + 60
    script = str(Path(__file__).resolve())
    try:
        modes = ["protected"] if scenario == "protected_alone" else ["managed"]
        if scenario == "contention":
            modes.append("protected")
        for mode in modes:
            command = [sys.executable, script, "--execute", "--worker", mode, "--cpu", str(cpu), "--nice", str(config["cpu"]["nice"] if mode == "managed" else 0),
                       "--ready", str(directory / f"{mode}.ready.json"), "--start", str(start_file), "--result", str(directory / f"{mode}.result.json"),
                       "--warmup", str(args.warmup), "--measure", str(args.measure), "--seed", str(seed), "--ready-timeout", str(max(120, deadline - time.monotonic()))]
            if mode == "managed" and profile_name != "unmanaged":
                request = {"task_id": execution_id, "assignment_id": execution_id, "argv": command,
                           "cwd": str(directory), "env": {}, "resources": {"cpu_millicores": 1000, "ram_mib": 64, "gpu_memory_mib": {}},
                           "replay_safe": True, "class": "guaranteed" if config["node_mode"] == "guaranteed" else "opportunistic",
                           "no_escape": True, "single_process": True, "required_controls": ["cpu.weight"] if profile_name == "cgroup" else [], "allow_fallback": False}
                write_json(job_path, request)
                command = [args.binary, "--config", str(config_path), "supervise", str(job_path)]
            case["commands"].append(command)
            stdout = (directory / f"{mode}.stdout").open("w")
            stderr = (directory / f"{mode}.stderr").open("w")
            streams.extend([stdout, stderr])
            processes[mode] = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        while not all((directory / f"{mode}.ready.json").exists() for mode in modes):
            if time.monotonic() >= deadline or any(process.poll() is not None for process in processes.values()):
                raise RuntimeError("worker failed to become ready; inspect retained logs and reservation state")
            time.sleep(.02)
        write_json(start_file, {"monotonic_start": time.monotonic() + 1})
        while any(process.poll() is None for process in processes.values()):
            if time.monotonic() >= deadline:
                raise RuntimeError("comparison deadline exceeded; reservation state retained for reconciliation")
            if "managed" in processes and profile_name != "unmanaged" and processes["managed"].returncode is None:
                sample = manager_sample(processes["managed"].pid)
                if sample is not None:
                    case["manager_samples"].append(sample)
            time.sleep(.2)
        for mode, process in processes.items():
            if process.returncode != 0:
                raise RuntimeError(f"{mode} exited {process.returncode}; inspect retained logs and reservation state")
            case[mode] = json.loads((directory / f"{mode}.result.json").read_text())
            case[mode]["counter_deltas"] = counter_deltas(case[mode]["pressure_before"], case[mode]["pressure_after"])
        if "managed" in processes and profile_name != "unmanaged":
            outcome = json.loads((directory / "managed.stdout").read_text())
            case["supervisor"] = outcome
            if profile_name == "cgroup" and not any(e["control"] == "cpu.weight" and e["applied"] and not e["fallback"] for e in outcome["record"]["evidence"]):
                raise RuntimeError("cgroup weight was not verified applied; comparison is invalid")
            released, exited, drained = outcome.get("release_confirmed_ms"), outcome.get("process_exit_ms"), outcome.get("drain_started_ms")
            case["release_confirmation_delay_ms"] = None if released is None or exited is None else released - exited
            case["drain_to_release_ms"] = None if released is None or drained is None else released - drained
            case["cleanup_confirmed"] = outcome["record"]["phase"] == "released"
        if profile_name == "unmanaged":
            case["cleanup_confirmed"] = all(child.returncode is not None for child in processes.values())
        case["manager_cpu_seconds"] = max((s["cpu_seconds"] for s in case["manager_samples"]), default=None)
        case["manager_mean_cpu_cores"] = None if case["manager_cpu_seconds"] is None else case["manager_cpu_seconds"] / (args.warmup + args.measure)
        case["manager_peak_rss_bytes"] = max((s["rss_bytes"] for s in case["manager_samples"]), default=None)
        case["manager_measurement_scope"] = "Manager process only: sampled cumulative CPU and peak RSS after worker readiness, including warmup; excludes worker CPU. Sampling can miss a final short interval or RSS peak."
        case["status"] = "completed"
    except BaseException as error:
        case["status"], case["error"] = "failed", str(error)
        case["cleanup_confirmed"] = False
        for process in processes.values():
            if process.poll() is None:
                process.terminate()  # Only directly owned Popen children.
        for process in processes.values():
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                case.setdefault("surviving_owned_child_pids", []).append(process.pid)
        raise
    finally:
        for stream in streams:
            stream.close()
        write_json(directory / "case.json", case)
    return case


def execute(args, report):
    if not sys.platform.startswith("linux"):
        raise RuntimeError("Linux execution is not tested or supported by this harness on macOS/Windows")
    if not args.baseline_config or not args.output or args.output == "-":
        raise RuntimeError("execution requires --baseline-config and a retained --output path; cgroup is optional")
    profile_count = 3 if args.cgroup_config else 2
    expected_seconds = args.repetitions * (1 + 2 * profile_count) * (args.warmup + args.measure + 10)
    if expected_seconds > args.max_wall_seconds:
        raise RuntimeError("comparison estimate exceeds approved maximum wall time; choose shorter explicit matched trials")
    profiles, doctors = {}, {}
    selected_profiles = [("rootless", args.baseline_config)]
    if args.cgroup_config:
        selected_profiles.append(("cgroup", args.cgroup_config))
    for name, path in selected_profiles:
        profiles[name] = checked_json([args.binary, "--config", path, "validate"])["config"]
        doctors[name] = checked_json([args.binary, "--config", path, "doctor"])
        profile = profiles[name]
        if not profile["execution"]["enabled"]:
            raise RuntimeError(f"{name} must already explicitly enable local execution")
        duration_ms = (args.warmup + args.measure + 30) * 1000 + profile["execution"]["prepare_timeout_ms"]
        duration_ms += profile["lifecycle"]["drain_timeout_ms"] + profile["lifecycle"]["term_grace_ms"]
        if profile["node_mode"] != "guaranteed" and profile["lifecycle"]["allocation_lease_ms"] < duration_ms:
            raise RuntimeError(f"{name} opportunistic lease must be explicitly configured >= {duration_ms:.0f} ms for this comparison")
    if profiles["rootless"]["cgroup"]["enabled"]:
        raise RuntimeError("rootless baseline must disable cgroup controls")
    if "cgroup" in profiles:
        if not profiles["cgroup"]["cgroup"]["enabled"] or profiles["cgroup"]["cgroup"]["cpu_weight"] is None:
            raise RuntimeError("cgroup comparison requires enabled authorized cpu.weight")
        for section in ["cpu", "ram", "monitor", "gpu", "lifecycle", "execution"]:
            if profiles["rootless"][section] != profiles["cgroup"][section]:
                raise RuntimeError(f"non-backend policy differs in {section}; align profiles to avoid an uncontrolled comparison")
    profiles["unmanaged"] = copy.deepcopy(profiles["rootless"])
    allowed = set(os.sched_getaffinity(0))
    for doctor in doctors.values():
        effective = (doctor.get("snapshot", {}).get("kernel") or {}).get("cpu", {}).get("effective_cpu_ids")
        if effective is None:
            raise RuntimeError("effective CPU set unavailable; cannot validate shared experiment placement")
        allowed.intersection_update(effective)
    allowed = sorted(allowed)
    if not allowed:
        raise RuntimeError("no common permitted CPU for rootless and delegated profiles")
    cpu = args.cpu if args.cpu is not None else allowed[0]
    if cpu not in allowed:
        raise RuntimeError("requested CPU is outside the harness's permitted affinity")
    output = Path(args.output).expanduser().resolve()
    output.relative_to(Path.home().resolve())
    output.parent.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="cedegrid-comparison-", dir=output.parent))
    report.update(mode="executed", execution_authorized=True, artifact_directory=str(directory), profiles=profiles,
                  preflight=doctors, allowed_cpus=allowed, selected_cpu=cpu, cases=[], status="running")
    # A successful harness run is local evidence only, not broad kernel verification.
    report["linux_runtime_verified"] = False
    run_started = time.monotonic()
    report["optional_cgroup"] = "configured_requires_verified_application" if args.cgroup_config else "not_configured_unverified"
    try:
        for repetition in range(args.repetitions):
            prefix, seed = f"r{repetition:02d}", args.seed + repetition
            alone = run_case(args, directory / f"{prefix}-protected-alone", profiles["rootless"], "rootless", cpu, "protected_alone", seed)
            report["cases"].append(alone)
            order = ["unmanaged", "rootless"] + (["cgroup"] if args.cgroup_config else [])
            if (args.seed + repetition) % 2:
                order.reverse()
            for scenario in ["idle", "contention"]:
                for name in order:
                    if time.monotonic() - run_started + args.warmup + args.measure + 10 > args.max_wall_seconds:
                        raise RuntimeError("comparison wall-time bound reached; retain completed matched evidence")
                    case = run_case(args, directory / f"{prefix}-{scenario}-{name}", profiles[name], name, cpu, scenario, seed)
                    if scenario == "contention":
                        case["protected_p95_slowdown_fraction"] = slowdown(case["protected"]["p95_ms"], alone["protected"]["p95_ms"])
                        case["protected_p99_slowdown_fraction"] = slowdown(case["protected"]["p99_ms"], alone["protected"]["p99_ms"])
                    report["cases"].append(case)
                    write_json(output, report)
        report["status"] = "completed"
        report["comparison_runs_completed"] = len(report["cases"])
        report["summary"] = summarize(report["cases"])
        report["target_evaluation"] = evaluate_targets(report["cases"], report["pre_registered_targets"])
        report["protection_measurement_scope"] = "Whole measured contention interval versus protected-alone. This local CLI comparison has no post-yield segmentation; the separate operational post-yield p99 acceptance remains pending."
        report["operational_post_yield_acceptance"] = "pending_no_post_yield_segmentation"
        report["interpretation"] = "Compare protected slowdown/tail latency alongside managed throughput and manager overhead. Stronger protection may intentionally lower managed throughput; utilization alone establishes no speedup."
    except BaseException as error:
        report["status"], report["error"] = "failed", str(error)
        raise
    finally:
        write_json(output, report)


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--binary", default="cedegrid")
    result.add_argument("--baseline-config")
    result.add_argument("--cgroup-config")
    result.add_argument("--output", default="-")
    result.add_argument("--execute", action="store_true", help="explicitly authorize the synthetic Linux experiment")
    result.add_argument("--repetitions", type=int, default=5)
    result.add_argument("--warmup", type=float, default=30)
    result.add_argument("--measure", type=float, default=120)
    result.add_argument("--seed", type=int, default=1729)
    result.add_argument("--max-wall-seconds", type=float, default=1800)
    result.add_argument("--cpu", type=int)
    for field in ["ready", "start", "result"]:
        result.add_argument("--" + field, help=argparse.SUPPRESS)
    result.add_argument("--worker", choices=["managed", "protected"], help=argparse.SUPPRESS)
    result.add_argument("--nice", type=int, default=0, help=argparse.SUPPRESS)
    result.add_argument("--ready-timeout", type=float, default=120, help=argparse.SUPPRESS)
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    if args.repetitions < 1 or args.warmup < 0 or args.measure <= 0 or not math.isfinite(args.warmup + args.measure) or not 0 < args.max_wall_seconds <= 1800:
        raise ValueError("repetitions/measurement must be positive and warmup nonnegative, with finite durations")
    if args.worker:
        # Only created by the opted-in runner, with case-specific readiness files.
        if not args.execute or not all([args.ready, args.start, args.result, args.cpu is not None]):
            raise ValueError("internal worker requires explicit case paths and CPU")
        for value in [args.ready,args.start,args.result]:
            Path(value).expanduser().resolve().relative_to(Path.home().resolve())
        worker(args)
        return 0
    report = protocol(args)
    if args.execute:
        execute(args, report)
    elif args.output == "-":
        print(json.dumps(report, indent=2))
    else:
        write_json(args.output, report)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"comparison not completed: {error}", file=sys.stderr)
        raise SystemExit(1)
