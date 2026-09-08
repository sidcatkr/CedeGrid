#!/usr/bin/env python3
"""Bounded, opt-in GPU release qualification using installed Cedegrid artifacts.

Run ``--help``, ``build --help``, and ``run --help`` for the public interface.
The run command is Linux-only; build/self-test need no CUDA or GPU. Never run it
against a shared node: --dedicated-test-node acknowledges exclusive use of the
selected Cedegrid node for the scenario, including the drain operation.

Private evidence contains deployment identities and logs. Only public-report.json
is suitable for release publication. A successful invocation qualifies exactly
one model/mode/class/scenario tuple; it does not qualify other tuples or hosts.
"""
from __future__ import annotations

import argparse
import array
import ctypes
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import select
import selectors
import shutil
import signal
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile

SCHEMA = 1
CAP_BYTES = 1024 * 1024 * 1024
PERIOD = 0.100
MAX_SAMPLE_GAP = 0.250
SCENARIO_SECONDS = 60.0
CLEANUP_SECONDS = 30.0
ELEMENTS, ROUNDS, BATCHES = 16384, 4096, 64
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
GPU_UUID = re.compile(r"GPU-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\Z")
SAFE_ID = re.compile(r"[a-zA-Z0-9_.-]{1,128}\Z")
MODELS = {"rtx-5060-ti": "RTX 5060 Ti", "l4": "L4"}


class GateError(Exception):
    """The code is public; detailed diagnostics are kept private."""

    def __init__(self, code, detail=""):
        super().__init__(detail or code)
        self.code = code


def require(condition, code, detail=""):
    if not condition:
        raise GateError(code, detail)


def encode(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def read_bytes(path, limit=8 * 1024 * 1024):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and 0 <= info.st_size <= limit,
                "INVALID_INPUT_FILE")
        chunks, size = [], 0
        while True:
            chunk = os.read(fd, min(1024 * 1024, limit + 1 - size))
            if not chunk:
                return b"".join(chunks)
            size += len(chunk)
            require(size <= limit, "INPUT_TOO_LARGE")
            chunks.append(chunk)
    finally:
        os.close(fd)


def sha256(path):
    digest = hashlib.sha256()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode), "INVALID_ARTIFACT")
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        after = os.fstat(fd)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns),
                "ARTIFACT_CHANGED")
        return digest.hexdigest()
    finally:
        os.close(fd)


def atomic_json(path, value):
    path = Path(path)
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(encode(value) + b"\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def private_directory(path):
    path = Path(path).absolute()
    path.mkdir(mode=0o700)
    require(not path.is_symlink() and path.stat().st_uid == os.getuid(),
            "PRIVATE_DIRECTORY_REQUIRED")
    os.chmod(path, 0o700)
    return path.resolve()


def toml_file(path):
    try:
        import tomllib
    except ImportError:
        try:
            import tomli as tomllib
        except ImportError as exc:
            raise GateError("TOML_READER_REQUIRED", "Use Python 3.11+, or install tomli") from exc
    return tomllib.loads(read_bytes(path).decode("utf-8"))


def reference_checksum(seed, completed):
    require(type(seed) is int and 0 <= seed <= 0xffffffff and
            type(completed) is int and 0 <= completed <= BATCHES,
            "INVALID_REFERENCE_PARAMETERS")
    a, c, m, b, rounds, mask = 1664525, 1013904223, 1, 0, ROUNDS, 0xffffffff
    while rounds:
        if rounds & 1:
            b, m = (a * b + c) & mask, (a * m) & mask
        c, a, rounds = (a * c + c) & mask, (a * a) & mask, rounds >> 1
    checksum = sum((m * ((seed + batch * 17 + index) & mask) + b) & mask
                   for batch in range(completed) for index in range(ELEMENTS))
    return f"{checksum:016x}"


def validate_submission(event, task_id, seed):
    require(type(event) is dict and event.get("event") == "publication_accepted",
            "INVALID_ACCEPTANCE_EVENT")
    submission = event["submission"]
    receipt = event["receipt"]
    require(receipt.get("kind") == "receipt", "MISSING_ACCEPTANCE_RECEIPT")
    receipt = receipt["receipt"]
    for key in ("task_id", "assignment_id", "generation"):
        require(receipt.get(key) == submission.get(key), "RECEIPT_IDENTITY_MISMATCH")
    require(submission["task_id"] == task_id and
            SAFE_ID.fullmatch(submission["assignment_id"]) and
            type(submission["generation"]) is int and submission["generation"] > 0,
            "SUBMISSION_IDENTITY_MISMATCH")
    require(set(submission) == {"task_id", "assignment_id", "generation", "result", "artifacts"}
            and submission["artifacts"] == [], "UNEXPECTED_SUBMISSION_SHAPE")
    # Rust's ResultSubmission field order is fixed; serde_json::Value object keys
    # use sorted maps. All workload metadata numbers are small exact integers.
    descriptor = submission["result"]
    require(type(descriptor) is dict and descriptor.get("schema_version") == 2 and
            descriptor.get("kind") == ("checkpoint" if event.get("checkpoint") else "result") and
            descriptor.get("artifacts") == [] and
            all(descriptor.get(key) == submission[key] for key in ("task_id", "assignment_id", "generation")) and
            type(descriptor.get("checkpoint_sequence")) is int and descriptor["checkpoint_sequence"] > 0,
            "INVALID_NATIVE_DESCRIPTOR")
    canonical_descriptor = json.loads(json.dumps(descriptor, sort_keys=True, ensure_ascii=False, allow_nan=False))
    canonical = {"task_id": submission["task_id"], "assignment_id": submission["assignment_id"],
                 "generation": submission["generation"],
                 "result": canonical_descriptor, "artifacts": []}
    digest = hashlib.sha256(encode(canonical)).hexdigest()
    require(receipt.get("receipt_hash") == digest, "RECEIPT_HASH_MISMATCH")
    result = descriptor["metadata"]
    require(result.get("algorithm") == "lcg32-v1" and result.get("seed") == seed and
            result.get("elements") == ELEMENTS and result.get("rounds") == ROUNDS and
            result.get("total_batches") == BATCHES, "REFERENCE_METADATA_MISMATCH")
    completed, resumed = result.get("completed_batches"), result.get("resumed_from")
    require(type(completed) is int and type(resumed) is int and
            0 <= resumed < completed <= BATCHES, "INVALID_PROGRESS")
    require(result.get("gpu_verified_values") == (completed - resumed) * ELEMENTS and
            result.get("checksum_hex") == reference_checksum(seed, completed),
            "GPU_CPU_RESULT_MISMATCH")
    require(type(event.get("checkpoint")) is bool and
            (completed < BATCHES if event["checkpoint"] else completed == BATCHES),
            "INVALID_PUBLICATION_KIND")
    return {"assignment_id": submission["assignment_id"], "generation": submission["generation"],
            "receipt_hash": digest, "checksum_hex": result["checksum_hex"],
            "descriptor_sha256": hashlib.sha256(encode(canonical_descriptor)).hexdigest(),
            "completed_batches": completed, "resumed_from": resumed,
            "gpu_verified_values": result["gpu_verified_values"]}


class Nvml:
    # https://docs.nvidia.com/deploy/nvml-api/structnvmlProcessInfo__t.html
    class Process(ctypes.Structure):
        _fields_ = [("pid", ctypes.c_uint), ("usedGpuMemory", ctypes.c_ulonglong),
                    ("gpuInstanceId", ctypes.c_uint), ("computeInstanceId", ctypes.c_uint)]

    class Memory(ctypes.Structure):
        _fields_ = [("total", ctypes.c_ulonglong), ("free", ctypes.c_ulonglong), ("used", ctypes.c_ulonglong)]

    def __init__(self, gpu_uuid):
        self.library = ctypes.CDLL("libnvidia-ml.so.1")
        for name in ("nvmlInit_v2", "nvmlShutdown"):
            getattr(self.library, name).argtypes = []
            getattr(self.library, name).restype = ctypes.c_int
        self.check(self.library.nvmlInit_v2())
        self.device = ctypes.c_void_p()
        get = self.library.nvmlDeviceGetHandleByUUID
        get.argtypes, get.restype = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)], ctypes.c_int
        self.check(get(gpu_uuid.encode(), ctypes.byref(self.device)))
        self.memory_query = self.library.nvmlDeviceGetMemoryInfo
        self.memory_query.argtypes = [ctypes.c_void_p, ctypes.POINTER(self.Memory)]
        self.memory_query.restype = ctypes.c_int
        self.queries = []
        for name in ("nvmlDeviceGetComputeRunningProcesses_v3", "nvmlDeviceGetGraphicsRunningProcesses_v3"):
            method = getattr(self.library, name)
            method.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(self.Process)]
            method.restype = ctypes.c_int
            self.queries.append(method)
        self.name = self.string("nvmlDeviceGetName", device=True)
        self.driver = self.string("nvmlSystemGetDriverVersion", device=False)
        self.version = self.string("nvmlSystemGetNVMLVersion", device=False)

    @staticmethod
    def check(code):
        require(code == 0, "NVML_UNAVAILABLE", f"NVML return code {code}")

    def string(self, name, *, device):
        buffer = ctypes.create_string_buffer(256)
        function = getattr(self.library, name)
        function.argtypes = ([ctypes.c_void_p] if device else []) + [ctypes.c_char_p, ctypes.c_uint]
        function.restype = ctypes.c_int
        self.check(function(self.device, buffer, len(buffer)) if device else function(buffer, len(buffer)))
        return buffer.value.decode("ascii", "strict")

    def processes(self):
        combined = {}
        for query in self.queries:
            count = ctypes.c_uint(4096)
            values = (self.Process * count.value)()
            self.check(query(self.device, ctypes.byref(count), values))
            require(count.value <= len(values), "NVML_INVENTORY_OVERFLOW")
            for value in values[:count.value]:
                memory = int(value.usedGpuMemory)
                require(memory != (1 << 64) - 1, "NVML_MEMORY_UNKNOWN")
                pid = int(value.pid)
                require(pid > 0, "NVML_IDENTITY_UNKNOWN")
                # Compute and graphics may name the same context owner.
                combined[pid] = max(combined.get(pid, 0), memory)
        return combined

    def close(self):
        self.check(self.library.nvmlShutdown())

    def memory(self):
        value = self.Memory()
        self.check(self.memory_query(self.device, ctypes.byref(value)))
        require(0 <= value.free <= value.total and 0 <= value.used <= value.total,
                "NVML_MEMORY_UNKNOWN")
        return {"total": int(value.total), "free": int(value.free), "used": int(value.used)}


def sampler_main(gpu_uuid):
    try:
        nvml = Nvml(gpu_uuid)
        next_time = time.monotonic()
        first = True
        while True:
            now = time.monotonic()
            if now < next_time:
                time.sleep(next_time - now)
            started = time.monotonic()
            processes = nvml.processes()
            output = {"event": "sample", "scheduled": next_time, "started": started,
                      "processes": processes, "device_memory": nvml.memory()}
            output["completed"] = time.monotonic()
            if first:
                output.update(device_name=nvml.name, driver_version=nvml.driver, nvml_version=nvml.version)
                first = False
            sys.stdout.buffer.write(encode(output) + b"\n")
            sys.stdout.buffer.flush()
            next_time += PERIOD
    except BrokenPipeError:
        return 2
    except Exception as exc:
        print(json.dumps({"event": "telemetry_error", "code": getattr(exc, "code", "NVML_UNAVAILABLE")} ), flush=True)
        return 2


class OwnedHandle:
    """Opened only for a direct unreaped child, or received from the live owner.

    Never open a pidfd using a PID found in NVML, a log, or the process table.
    pidfd signals cannot target a recycled PID:
    https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html
    """

    def __init__(self, fd, pid, role, *, process=None, attempt=None):
        self.fd, self.pid, self.role = fd, pid, role
        self.process, self.attempt = process, attempt
        self.signals = []
        os.set_inheritable(fd, False)

    @classmethod
    def child(cls, process, role):
        # Popen has just returned and nobody has waited on this direct child.
        return cls(os.pidfd_open(process.pid, 0), process.pid, role, process=process)

    def alive(self):
        return not select.select([self.fd], [], [], 0)[0]

    def send(self, sig):
        if self.alive():
            try:
                signal.pidfd_send_signal(self.fd, sig, None, 0)
                self.signals.append(int(sig))
            except ProcessLookupError:
                pass

    def reaped(self):
        if self.process is None:
            return None
        return self.process.poll() is not None

    def close(self):
        os.close(self.fd)


def pidfd_pid(fd):
    # This is identity verification of a transferred handle, never rediscovery.
    text = Path(f"/proc/self/fdinfo/{fd}").read_text()
    match = re.search(r"^Pid:\s+(\d+)\s*$", text, re.MULTILINE)
    require(match is not None, "INVALID_TRANSFERRED_HANDLE")
    return int(match.group(1))


def live_executable_hash(pid, handle):
    require(handle.alive(), "REGISTRATION_PROCESS_EXITED")
    # /proc/<pid>/exe must be followed intentionally, while the held pidfd proves
    # the owner stayed alive throughout the read. No authority derives from PID.
    with open(f"/proc/{pid}/exe", "rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest() if hasattr(hashlib, "file_digest") else None
        if digest is None:
            digest_object = hashlib.sha256()
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest_object.update(chunk)
            digest = digest_object.hexdigest()
    require(handle.alive(), "REGISTRATION_PROCESS_EXITED")
    return digest


class Watchdog:
    def __init__(self, configuration, control_fd):
        self.config = configuration
        self.root = Path(configuration["private_dir"])
        self.selector = selectors.DefaultSelector()
        self.control = socket.socket(fileno=control_fd)
        self.control.setblocking(False)
        self.selector.register(self.control, selectors.EVENT_READ, ("control", None))
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket_dir = Path(tempfile.mkdtemp(prefix="cgq-", dir="/tmp"))
        os.chmod(self.socket_dir, 0o700)
        self.socket_path = self.socket_dir / "register.sock"
        self.listener.bind(str(self.socket_path))
        os.chmod(self.socket_path, 0o600)
        self.listener.listen(8)
        self.listener.setblocking(False)
        self.selector.register(self.listener, selectors.EVENT_READ, ("listener", None))
        self.handles, self.buffers, self.pending = [], {}, {}
        self.events = []
        self.samples, self.maximum, self.overlap_samples, self.zero_samples = 0, 0, 0, 0
        self.last_sample, self.maximum_gap = None, 0.0
        self.armed, self.cleanup_started, self.finish_received = None, None, False
        self.failure, self.ready, self.agent, self.competitor = None, False, None, None
        self.pause_until, self.stop_agent_at = None, None
        self.started = time.monotonic()
        self.metadata, self.completed_competition, self.control_buffer = {}, False, b""
        self.sampled_attempts = set()
        self.foreign_process_count, self.peak_foreign_process_count, self.free_device_bytes = 0, 0, None
        self.log = open(self.root / "watchdog-private.jsonl", "xb", buffering=0)
        self.launch([sys.executable, "-I", str(Path(__file__).resolve()), "_sampler", configuration["gpu_uuid"]], "sampler")

    def record(self, value):
        self.log.write(encode({"monotonic": time.monotonic(), **value}) + b"\n")

    def launch(self, command, role, env=None):
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, cwd=self.root, env=env,
                                   start_new_session=True, close_fds=True)
        handle = OwnedHandle.child(process, role)
        self.handles.append(handle)
        for stream, suffix in ((process.stdout, "stdout"), (process.stderr, "stderr")):
            os.set_blocking(stream.fileno(), False)
            self.selector.register(stream, selectors.EVENT_READ, ("pipe", (role, suffix)))
            self.buffers[stream.fileno()] = b""
        self.record({"event": "owned_child", "role": role, "pid": process.pid})
        return handle

    def start_agent(self):
        env = os.environ.copy()
        for key in ("PYTHONPATH", "PYTHONHOME"):
            env.pop(key, None)
        env["CEDEGRID_PUBLICATION_EVIDENCE"] = "1"
        self.agent = self.launch([self.config["native_binary"], "--config", self.config["node_config"],
                                  "agent", "--deployment", self.config["agent_deployment"]], "agent", env)
        self.ready = True

    def fail(self, code):
        if self.failure is None:
            self.failure = code
            self.record({"event": "failure", "code": code})
        self.begin_cleanup()

    def begin_cleanup(self):
        if self.cleanup_started is not None:
            return
        self.cleanup_started = time.monotonic()
        self.zero_samples = 0
        self.pause_until = None
        if self.agent:
            self.agent.send(signal.SIGCONT)
        # Keep the agent alive briefly to report supervisor-confirmed release.
        for handle in self.handles:
            if handle.role in ("worker", "competition"):
                handle.send(signal.SIGTERM)
        self.stop_agent_at = self.cleanup_started + 5.0

    def accept_registration(self):
        connection, _ = self.listener.accept()
        connection.setblocking(False)
        credentials = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
        pid, uid, _ = struct.unpack("3i", credentials)
        self.pending[connection.fileno()] = {"connection": connection, "pid": pid, "uid": uid,
                                             "buffer": b"", "fds": [], "deadline": time.monotonic() + 2.0}
        self.selector.register(connection, selectors.EVENT_READ, ("registration", connection.fileno()))

    def receive_registration(self, index):
        pending = self.pending[index]
        connection = pending["connection"]
        data, ancillary, flags, _ = connection.recvmsg(4096, socket.CMSG_SPACE(4 * array.array("i").itemsize))
        for level, kind, body in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                fds = array.array("i")
                fds.frombytes(body[:len(body) - len(body) % fds.itemsize])
                pending["fds"].extend(fds)
        pending["buffer"] += data
        require(not flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC) and len(pending["buffer"]) <= 4096,
                "REGISTRATION_OVERFLOW")
        require(data, "REGISTRATION_DISCONNECTED")
        if b"\n" not in pending["buffer"]:
            return
        message = json.loads(pending["buffer"])
        require(self.armed is not None and self.cleanup_started is None and
                message.get("op") == "register" and message.get("token") == self.config["token"] and
                pending["uid"] == os.getuid() and len(pending["fds"]) == 1,
                "REGISTRATION_DENIED")
        fd = pending["fds"][0]
        require(pidfd_pid(fd) == pending["pid"], "REGISTRATION_HANDLE_MISMATCH")
        attempt = {key: message[key] for key in ("task_id", "assignment_id", "generation")}
        standalone = message["task_id"] == "competition"
        if standalone:
            require(self.competitor is not None and self.competitor.pid == pending["pid"] and
                    attempt == {"task_id": "competition", "assignment_id": "competition", "generation": 0},
                    "UNOWNED_COMPETITION")
        else:
            require(message["task_id"] == self.config["task_id"] and
                    SAFE_ID.fullmatch(message["assignment_id"]) and
                    type(message["generation"]) is int and 1 <= message["generation"] <= 4 and
                    not any(h.role == "worker" and h.attempt == attempt for h in self.handles),
                    "UNEXPECTED_WORKER_IDENTITY")
        candidate = OwnedHandle(fd, pending["pid"], "competition" if standalone else "worker", attempt=attempt)
        require(live_executable_hash(pending["pid"], candidate) == self.config["worker_sha256"],
                "WORKER_ARTIFACT_MISMATCH")
        if standalone:
            self.competitor.attempt = attempt
            candidate.close()
        else:
            self.handles.append(candidate)
        pending["fds"] = []
        self.record({"event": "registration_verified", "pid": pending["pid"], **attempt})
        connection.sendall(b'{"registered":true}\n')
        self.close_pending(index)

    def close_pending(self, index):
        pending = self.pending.pop(index)
        for fd in pending["fds"]:
            os.close(fd)
        self.selector.unregister(pending["connection"])
        pending["connection"].close()

    def sample(self, event):
        require(event.get("event") == "sample", event.get("code", "TELEMETRY_LOST"))
        now, completed = time.monotonic(), event["completed"]
        require(type(completed) in (float, int) and abs(now - completed) <= MAX_SAMPLE_GAP,
                "TELEMETRY_STALE")
        require(completed - event["scheduled"] <= MAX_SAMPLE_GAP and
                completed >= event["started"] >= event["scheduled"] - 0.001,
                "TELEMETRY_SCHEDULE_MISSED")
        if self.last_sample is not None:
            gap = completed - self.last_sample
            self.maximum_gap = max(self.maximum_gap, gap)
            require(0 < gap <= MAX_SAMPLE_GAP, "TELEMETRY_SCHEDULE_MISSED")
        self.last_sample = completed
        processes = {int(pid): memory for pid, memory in event["processes"].items()}
        require(all(type(memory) is int and 0 <= memory < 1 << 64 for memory in processes.values()),
                "NVML_MEMORY_UNKNOWN")
        device_memory = event["device_memory"]
        require(type(device_memory.get("free")) is int and type(device_memory.get("total")) is int and
                0 <= device_memory["free"] <= device_memory["total"], "NVML_MEMORY_UNKNOWN")
        self.free_device_bytes = device_memory["free"]
        if not self.ready:
            wanted, name = self.config["model"], event.get("device_name", "")
            require((wanted == "l4" and name in ("NVIDIA L4", "L4")) or
                    (wanted == "rtx-5060-ti" and "RTX 5060 Ti" in name), "GPU_MODEL_MISMATCH")
            require(self.free_device_bytes >= CAP_BYTES, "INSUFFICIENT_FREE_GPU_MEMORY")
            self.metadata = {key: event[key] for key in ("driver_version", "nvml_version")}
            self.start_agent()
        registered = [h for h in self.handles if h.role != "sampler"]
        active = [h for h in registered if h.alive()]
        allowed = {h.pid for h in registered}
        foreign = {pid: memory for pid, memory in processes.items() if pid not in allowed}
        self.foreign_process_count = len(foreign)
        self.peak_foreign_process_count = max(self.peak_foreign_process_count, len(foreign))
        # A driver inventory may lag a process exit. Continue charging every
        # registered PID conservatively until it disappears; never reopen or
        # signal that numeric PID. If recycled, its context prevents cleanup
        # proof rather than granting authority over the new process.
        owned = sum(processes.get(pid, 0) for pid in allowed)
        require(owned <= CAP_BYTES, "OWNED_VRAM_CAP_EXCEEDED")
        self.zero_samples = self.zero_samples + 1 if not (set(processes) & allowed) else 0
        if self.armed is not None:
            self.samples += 1
            self.maximum = max(self.maximum, owned)
            worker_present = any(h.role == "worker" and h.pid in processes for h in active)
            competitor_present = any(h.role == "competition" and h.pid in processes for h in active)
            self.sampled_attempts.update(h.attempt["assignment_id"] for h in active
                                         if h.role == "worker" and h.pid in processes and h.attempt)
            self.overlap_samples += int(worker_present and competitor_present)
        self.record({"event": "sample", "scheduled": event["scheduled"], "completed": completed,
                     "processes": processes, "foreign_processes": foreign,
                     "device_memory": device_memory, "owned_bytes": owned})

    def pipe_data(self, stream, identity):
        role, suffix = identity
        data = os.read(stream.fileno(), 65536)
        if not data:
            self.selector.unregister(stream)
            self.buffers.pop(stream.fileno(), None)
            stream.close()
            return
        buffer = self.buffers[stream.fileno()] + data
        require(len(buffer) <= 1024 * 1024, "CHILD_OUTPUT_BOUND_EXCEEDED")
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            if role == "sampler" and suffix == "stdout":
                self.sample(json.loads(line))
            else:
                self.record({"event": "child_output", "role": role, "stream": suffix,
                             "line": line.decode("utf-8", "replace")})
                if suffix == "stdout":
                    try:
                        value = json.loads(line)
                    except (ValueError, UnicodeError):
                        continue
                    if role == "agent" and value.get("event") == "publication_accepted":
                        require(len(self.events) < 64, "ACCEPTANCE_EVENT_BOUND_EXCEEDED")
                        self.events.append(value)
                    elif role == "competition" and value.get("event") == "competition_complete":
                        require(type(value.get("gpu_verified_values")) is int and value["gpu_verified_values"] > 0,
                                "COMPETITION_NOT_USEFUL")
                        self.completed_competition = True
        self.buffers[stream.fileno()] = buffer

    def command(self, value):
        op = value.get("op")
        if op == "arm":
            require(self.ready and self.armed is None and self.cleanup_started is None,
                    "WATCHDOG_NOT_READY")
            require(self.last_sample is not None and time.monotonic() - self.last_sample <= MAX_SAMPLE_GAP and
                    self.free_device_bytes >= CAP_BYTES, "INSUFFICIENT_FRESH_GPU_MEMORY")
            self.armed = time.monotonic()
            self.samples, self.maximum_gap = 0, 0.0
        elif op == "pause_agent":
            duration = value.get("seconds")
            require(self.armed is not None and self.cleanup_started is None and self.agent.alive() and
                    type(duration) in (int, float) and 0 < duration <= 15 and self.pause_until is None,
                    "INVALID_LEASE_LOSS_REQUEST")
            self.pause_until = time.monotonic() + duration
            self.agent.send(signal.SIGSTOP)
            self.record({"event": "owned_agent_paused", "seconds": duration})
        elif op == "competition":
            require(self.armed is not None and self.cleanup_started is None and self.competitor is None,
                    "INVALID_COMPETITION_REQUEST")
            env = os.environ.copy()
            env.update(CEDEGRID_QUALIFIER_SOCKET=str(self.socket_path), CEDEGRID_QUALIFIER_TOKEN=self.config["token"])
            self.competitor = self.launch([self.config["worker_binary"], "--competition", self.config["gpu_uuid"], "5"],
                                          "competition", env)
        elif op == "finish":
            self.finish_received = True
            self.begin_cleanup()
        elif op == "fail":
            self.fail("HARNESS_SCENARIO_FAILED")
        elif op != "status":
            raise GateError("INVALID_WATCHDOG_COMMAND")
        self.control.sendall(encode(self.status()) + b"\n")

    def status(self):
        return {"ready": self.ready, "armed": self.armed, "failure": self.failure,
                "socket": str(self.socket_path), "metadata": self.metadata,
                "events": self.events, "samples": self.samples,
                "peak_owned_vram_bytes": self.maximum, "max_sample_gap_ms": self.maximum_gap * 1000,
                "overlap_samples": self.overlap_samples, "competition_complete": self.completed_competition,
                "sampled_attempts": sorted(self.sampled_attempts),
                "foreign_process_count": self.foreign_process_count,
                "peak_foreign_process_count": self.peak_foreign_process_count,
                "free_device_bytes": self.free_device_bytes,
                "handles": [{"pid": h.pid, "role": h.role, "attempt": h.attempt,
                             "alive": h.alive(), "reaped": h.reaped(), "signals": h.signals}
                            for h in self.handles if h.role != "sampler"],
                "owned_contexts_absent": self.zero_samples >= 2,
                "cleanup_started": self.cleanup_started}

    def run(self):
        try:
            while True:
                now = time.monotonic()
                if self.armed is None and now - self.started > 20:
                    self.fail("PREFLIGHT_DEADLINE")
                if self.armed is not None and self.cleanup_started is None and now - self.armed >= SCENARIO_SECONDS:
                    self.fail("SCENARIO_DEADLINE")
                if self.ready and self.last_sample is not None and now - self.last_sample > MAX_SAMPLE_GAP:
                    self.fail("TELEMETRY_LOST")
                if self.agent and self.cleanup_started is None and not self.agent.alive():
                    self.fail("OWNED_AGENT_EXITED")
                if self.pause_until is not None and now >= self.pause_until:
                    self.agent.send(signal.SIGCONT)
                    self.pause_until = None
                for index in list(self.pending):
                    if now > self.pending[index]["deadline"]:
                        self.close_pending(index)
                        self.fail("REGISTRATION_DEADLINE")
                if self.cleanup_started is not None:
                    elapsed = now - self.cleanup_started
                    if elapsed >= 2:
                        for handle in self.handles:
                            if handle.role in ("worker", "competition"):
                                handle.send(signal.SIGKILL)
                    if now >= self.stop_agent_at and self.agent:
                        self.agent.send(signal.SIGTERM if elapsed < 7 else signal.SIGKILL)
                    non_sampler = [h for h in self.handles if h.role != "sampler"]
                    complete = all(not h.alive() and h.reaped() is not False for h in non_sampler)
                    if complete and (self.zero_samples >= 2 or (self.armed is None and self.agent is None)):
                        break
                    if elapsed >= CLEANUP_SECONDS:
                        self.failure = self.failure or "CLEANUP_UNVERIFIED"
                        break
                for key, _ in self.selector.select(0.025):
                    kind, identity = key.data
                    try:
                        if kind == "listener":
                            self.accept_registration()
                        elif kind == "registration":
                            self.receive_registration(identity)
                        elif kind == "pipe":
                            self.pipe_data(key.fileobj, identity)
                        else:
                            data = self.control.recv(65536)
                            if not data:
                                self.selector.unregister(self.control)
                                self.fail("HARNESS_DISCONNECTED")
                                continue
                            self.control_buffer += data
                            require(len(self.control_buffer) <= 65536, "CONTROL_FRAME_TOO_LARGE")
                            while b"\n" in self.control_buffer:
                                line, self.control_buffer = self.control_buffer.split(b"\n", 1)
                                self.command(json.loads(line))
                    except (BlockingIOError, InterruptedError):
                        continue
                    except Exception as exc:
                        if kind == "registration" and identity in self.pending:
                            self.close_pending(identity)
                        self.record({"event": "private_error", "detail": str(exc)})
                        self.fail(getattr(exc, "code", "WATCHDOG_OPERATION_FAILED"))
                require(self.log.tell() <= 32 * 1024 * 1024, "PRIVATE_LOG_BOUND_EXCEEDED")
        except BaseException as exc:
            self.failure = self.failure or getattr(exc, "code", "WATCHDOG_FAILED")
            # Fail-safe acts only through already-owned handles. A runtime error
            # cannot justify process-table discovery or process-group signalling.
            for handle in self.handles:
                if handle.role != "sampler":
                    handle.send(signal.SIGCONT)
                    handle.send(signal.SIGKILL)
            until = (self.cleanup_started or time.monotonic()) + CLEANUP_SECONDS
            while time.monotonic() < until:
                remaining = [h for h in self.handles if h.role != "sampler" and
                             (h.alive() or h.reaped() is False)]
                if not remaining:
                    break
                for handle in remaining:
                    handle.send(signal.SIGCONT)
                    handle.send(signal.SIGKILL)
                time.sleep(0.025)
        finally:
            for handle in self.handles:
                if handle.role == "sampler":
                    handle.send(signal.SIGKILL)
                    try:
                        handle.process.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        self.failure = self.failure or "SAMPLER_REAP_UNVERIFIED"
            final = self.status()
            final["final"] = True
            final["watchdog_completed"] = True
            atomic_json(self.root / "watchdog-final-private.json", final)
            for index in list(self.pending):
                self.close_pending(index)
            for handle in self.handles:
                handle.close()
            self.listener.close()
            self.control.close()
            self.socket_path.unlink(missing_ok=True)
            self.socket_dir.rmdir()
            self.log.close()
        return 1 if self.failure else 0


class WatchdogClient:
    def __init__(self, root, configuration):
        configuration_file = root / "watchdog-input-private.json"
        atomic_json(configuration_file, configuration)
        self.socket, child = socket.socketpair()
        self.socket.settimeout(1)
        self.log = open(root / "watchdog-bootstrap-private.log", "xb", buffering=0)
        self.process = subprocess.Popen([sys.executable, "-I", str(Path(__file__).resolve()), "_watchdog",
                                         str(configuration_file), str(child.fileno())],
                                        stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
                                        close_fds=True, pass_fds=(child.fileno(),), start_new_session=True, cwd=root)
        child.close()
        self.buffer, self.root = b"", root

    def request(self, op="status", **payload):
        self.socket.sendall(encode({"op": op, **payload}) + b"\n")
        while b"\n" not in self.buffer:
            data = self.socket.recv(1024 * 1024)
            require(data, "WATCHDOG_DISCONNECTED")
            self.buffer += data
            require(len(self.buffer) <= 1024 * 1024, "WATCHDOG_RESPONSE_TOO_LARGE")
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def close(self):
        self.socket.close()
        self.log.close()


def installed_sdk(wheel, expected):
    require(sha256(wheel) == expected, "PYTHON_WHEEL_HASH_MISMATCH")
    require(not os.environ.get("PYTHONPATH") and not os.environ.get("PYTHONHOME"),
            "SOURCE_IMPORT_OVERRIDE_PRESENT")
    distribution = importlib.metadata.distribution("cedegrid")
    import cedegrid
    installed = Path(cedegrid.__file__).resolve().parent
    repository = Path(__file__).resolve().parents[1]
    checkout = (repository / "Cargo.toml").is_file() and (repository / "src" / "lib.rs").is_file()
    require(not (checkout and installed.is_relative_to(repository)), "SOURCE_TREE_SDK_FORBIDDEN")
    direct_url = distribution.read_text("direct_url.json")
    require(not direct_url or not json.loads(direct_url).get("dir_info", {}).get("editable"),
            "EDITABLE_SDK_FORBIDDEN")
    require(distribution.version == "0.2.0", "SDK_VERSION_MISMATCH")
    expected_paths = set()
    with zipfile.ZipFile(wheel) as archive:
        require(len(archive.infolist()) < 10000, "WHEEL_ENTRY_BOUND_EXCEEDED")
        for entry in archive.infolist():
            if entry.filename.startswith("cedegrid/") and not entry.is_dir():
                relative = Path(entry.filename).relative_to("cedegrid")
                require(".." not in relative.parts and entry.file_size <= 16 * 1024 * 1024,
                        "INVALID_WHEEL_MEMBER")
                require(not (installed / relative).is_symlink(), "INSTALLED_SDK_LINK")
                actual = read_bytes(installed / relative, 16 * 1024 * 1024)
                require(hashlib.sha256(actual).digest() == hashlib.sha256(archive.read(entry)).digest(),
                        "INSTALLED_SDK_ARTIFACT_MISMATCH")
                expected_paths.add(relative)
    actual_paths = {p.relative_to(installed) for p in installed.rglob("*")
                    if p.is_file() and "__pycache__" not in p.parts and p.suffix != ".pyc"}
    require(expected_paths and actual_paths == expected_paths, "INSTALLED_SDK_CONTENT_MISMATCH")
    return cedegrid, distribution.version


def preflight(args, root):
    require(sys.platform == "linux" and hasattr(os, "pidfd_open") and
            hasattr(signal, "pidfd_send_signal") and hasattr(socket, "SO_PEERCRED"),
            "LINUX_PIDFD_REQUIRED")
    require(args.dedicated_test_node, "DEDICATED_TEST_NODE_REQUIRED")
    require(signal.getsignal(signal.SIGCHLD) == signal.SIG_DFL,
            "EXCLUSIVE_CHILD_REAPING_REQUIRED")
    require(type(args.seed) is int and 0 <= args.seed <= 0xffffffff, "INVALID_REFERENCE_PARAMETERS")
    require(GPU_UUID.fullmatch(args.gpu_uuid), "FULL_GPU_UUID_REQUIRED")
    require(SAFE_ID.fullmatch(args.host_role), "INVALID_NEUTRAL_HOST_ROLE")
    require(args.host_role in ("gpu-ext4", "gpu-replay"), "INVALID_NEUTRAL_HOST_ROLE")
    require(all(HEX64.fullmatch(value) for value in (args.native_sha256, args.worker_sha256, args.wheel_sha256)),
            "EXPECTED_ARTIFACT_HASH_REQUIRED")
    native, worker, wheel = [Path(value).resolve(strict=True) for value in (args.native_binary, args.worker_binary, args.wheel)]
    require(sha256(native) == args.native_sha256 and sha256(worker) == args.worker_sha256,
            "EXECUTABLE_ARTIFACT_MISMATCH")
    repository = Path(__file__).resolve().parents[1]
    checkout = (repository / "Cargo.toml").is_file() and (repository / "src" / "lib.rs").is_file()
    require(not (checkout and native.is_relative_to(repository)), "INSTALLED_NATIVE_ARTIFACT_REQUIRED")
    module, sdk_version = installed_sdk(wheel, args.wheel_sha256)
    report = json.loads(read_bytes(args.build_report))
    require(report.get("status") == "PASS" and report.get("scope") == "build_and_cpu_self_test_only" and
            report.get("worker_sha256") == args.worker_sha256 and
            report.get("source_sha256") == sha256(Path(__file__).resolve().parents[1] / "examples" / "gpu_counter.c"),
            "WORKER_BUILD_PROVENANCE_MISMATCH")
    output = subprocess.run([str(worker), "--self-test"], capture_output=True, timeout=5, check=True, cwd=root)
    worker_check = json.loads(output.stdout)
    require(worker_check.get("checksum_hex") == reference_checksum(7, BATCHES) and
            worker_check.get("gpu_status") == "NOT_RUN", "WORKER_CPU_REFERENCE_FAILED")
    native_check = subprocess.run([str(native), "--version"], capture_output=True, timeout=5, check=True, cwd=root)
    require(native_check.stdout.strip() == b"cedegrid 0.2.0", "NATIVE_VERSION_MISMATCH")
    node = toml_file(args.node_config)
    deployment = toml_file(args.agent_deployment)
    require(node.get("config_version") == 1 and deployment.get("config_version") == 1 and
            node.get("execution", {}).get("enabled") is True, "NODE_EXECUTION_NOT_ENABLED")
    require(deployment.get("max_workers") == 1 and deployment.get("capacity") == {
        "cpu_millicores": 1000, "ram_mib": 512, "gpu_memory_mib": {args.gpu_uuid: 512}},
        "EXPLICIT_NODE_ENVELOPE_REQUIRED")
    require(deployment.get("max_runtime_seconds", 0) == 0, "UNBOUNDED_AGENT_LIFETIME_REQUIRED")
    require(node.get("gpu", {}).get("execution_mode") == args.execution_mode, "GPU_MODE_MISMATCH")
    # This gate does not create authorizations for occupied third-party contexts.
    require(args.execution_mode != "best_effort_occupied", "BEST_EFFORT_OCCUPIED_NOT_QUALIFIED")
    require(not node.get("gpu", {}).get("best_effort_external_processes"), "EXTERNAL_GPU_AUTHORIZATION_PRESENT")
    require(args.allocation_class != "guaranteed" or args.execution_mode == "contention_aware",
            "GUARANTEED_GPU_MODE_UNSUPPORTED")
    if args.scenario == "competition":
        require(args.competition_expect == ("continue" if args.allocation_class == "guaranteed" else "yield"),
                "COMPETITION_EXPECTATION_MISMATCH")
    lease_ms = node.get("lifecycle", {}).get("allocation_lease_ms", 10000)
    require(type(lease_ms) is int and 1000 <= lease_ms <= 12000, "BOUNDED_LEASE_REQUIRED")
    require(node.get("monitor", {}).get("interval_ms", 500) <= 500 and
            node.get("lifecycle", {}).get("heartbeat_interval_ms", 2000) <= 1000,
            "QUALIFICATION_SAMPLE_INTERVAL_REQUIRED")
    require(node.get("gpu", {}).get("scale_up_cooldown_ms", 30000) <= 1000,
            "BOUNDED_RESUME_COOLDOWN_REQUIRED")
    client = module.Client.from_config(args.client_config)
    client.timeout = 2.0
    node_id = node["node_id"]
    existing = list(client.iter_status("allocations", node_id=node_id, limit=100))
    require(not any(value.get("phase") != "released" for value in existing), "NODE_HAS_ACTIVE_RESERVATIONS")
    unknown = list(client.iter_status("unrecognized_allocations", node_id=node_id, limit=100))
    require(not unknown, "NODE_HAS_UNRECOGNIZED_ALLOCATIONS")
    node_status = list(client.iter_status("nodes", node_id=node_id, limit=100))
    require(not any(value.get("drain") for value in node_status), "NODE_ALREADY_DRAINED")
    return module, client, {"node_id": node_id, "lease_ms": lease_ms, "native_binary": str(native),
                            "worker_binary": str(worker), "sdk_version": sdk_version}


def public_report(args, status, reason, proof, metrics, versions):
    # Allowlist construction. Never copy raw host data, errors, results, records,
    # UUIDs, route/config/path details, or logs into release evidence.
    reason = reason if type(reason) is str and re.fullmatch(r"[A-Z0-9_]{1,80}", reason) else "GATE_FAILURE"
    hashes = {key: value if type(value) is str and HEX64.fullmatch(value) else None
              for key, value in (("native_sha256", args.native_sha256), ("worker_sha256", args.worker_sha256),
                                 ("wheel_sha256", args.wheel_sha256))}
    safe_versions = {key: value for key, value in versions.items()
                     if key in ("native", "python_sdk", "python", "driver_version", "nvml_version")
                     and type(value) is str and re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,3}", value)}
    if type(versions.get("cuda_driver_api")) is int:
        safe_versions["cuda_driver_api"] = versions["cuda_driver_api"]
    capability = versions.get("compute_capability")
    if type(capability) is list and len(capability) == 2 and all(type(v) is int and 0 <= v < 100 for v in capability):
        safe_versions["compute_capability"] = capability
    return {"schema": SCHEMA, "gate": "gpu", "status": status, "reason_code": reason,
            "host_role": args.host_role, "gpu_model": MODELS[args.model],
            "execution_mode": args.execution_mode, "allocation_class": args.allocation_class,
            "scenario": args.scenario, "competition_expect": args.competition_expect if args.scenario == "competition" else None,
            "model_aggregate_status": "NOT_RUN", "versions": safe_versions,
            "artifacts": hashes,
            "bounds": {"scenario_seconds": 60, "cleanup_seconds": 30, "sample_interval_ms": 100,
                       "sampled_owned_vram_cap_bytes": CAP_BYTES, "hardware_enforced_memory_partition": False},
            "metrics": metrics, "proof": proof}


def run_gate(args):
    root = private_directory(args.evidence_dir)
    status, reason, proof, metrics, versions = "BLOCKED", "PREREQUISITES", {}, {}, {}
    watchdog = None
    client = None
    submitted = armed = drained = False
    cleanup_released = False
    deadline = None
    private_error = None
    task_id = "gpu-" + uuid.uuid4().hex
    job_id, pool_id = task_id + "-job", task_id + "-pool"
    configuration = {}
    try:
        module, client, configuration = preflight(args, root)
        versions = {"native": "0.2.0", "python_sdk": configuration["sdk_version"], "python": sys.version.split()[0]}
        configuration.update(private_dir=str(root), gpu_uuid=args.gpu_uuid, model=args.model,
                             worker_sha256=args.worker_sha256, task_id=task_id, token=uuid.uuid4().hex,
                             node_config=str(Path(args.node_config).resolve()),
                             agent_deployment=str(Path(args.agent_deployment).resolve()))
        watchdog = WatchdogClient(root, configuration)
        ready_deadline = time.monotonic() + 20
        while True:
            state = watchdog.request()
            require(not state["failure"], state["failure"] or "WATCHDOG_PREFLIGHT_FAILED")
            if state["ready"]:
                break
            require(time.monotonic() < ready_deadline, "PREFLIGHT_DEADLINE")
            time.sleep(0.05)
        versions.update(state["metadata"])
        # Probe creates no CUDA context. NVML is already watched independently.
        environment = os.environ.copy()
        environment["CUDA_VISIBLE_DEVICES"] = args.gpu_uuid
        probe = subprocess.run([configuration["worker_binary"], "--probe", args.gpu_uuid], env=environment,
                               cwd=root, capture_output=True, check=True, timeout=5)
        probe = json.loads(probe.stdout)
        require(probe.get("context_created") is False and probe.get("status") == "READY", "CUDA_DRIVER_PROBE_FAILED")
        versions["cuda_driver_api"] = probe["driver_api"]
        versions["compute_capability"] = probe["compute_capability"]
        # Native admission decides whether existing foreign GPU activity is
        # compatible with this opportunistic tuple. Never whitelist those
        # processes or substitute free memory for native policy evidence.
        admission_until = time.monotonic() + 8
        while True:
            node_page = client.status_page("nodes", node_id=configuration["node_id"], limit=100)
            require(node_page.get("next_cursor") is None, "NODE_EVIDENCE_BOUND_EXCEEDED")
            eligible = [value for value in node_page["items"] if
                        value.get("report", {}).get("gpu_expansion_allowed") is True and
                        value["report"].get("launch_slots", 0) > 0 and
                        value["report"].get("managed_budget", {}).get("gpu_memory_mib", {}).get(args.gpu_uuid, 0) >= 512 and
                        node_page["observed_at_unix_ms"] - value.get("received_ms", 0) <= 2000]
            if eligible:
                proof["native_gpu_admission_verified"] = True
                break
            require(time.monotonic() < admission_until, "GPU_POLICY_ADMISSION_UNAVAILABLE")
            state = watchdog.request()
            require(not state["failure"], state["failure"] or "WATCHDOG_PREFLIGHT_FAILED")
            time.sleep(0.1)
        # Deadline and watchdog arm precede every pool/submission RPC.
        state = watchdog.request("arm")
        armed = True
        deadline = state["armed"] + SCENARIO_SECONDS
        status, reason = "FAIL", "SCENARIO_INCOMPLETE"
        task = module.command_task(task_id, [configuration["worker_binary"], "--seed", str(args.seed)], str(root),
                                   cpu_millicores=1000, ram_mib=512, gpu_vram_mib={args.gpu_uuid: 512},
                                   env={"CEDEGRID_QUALIFIER_SOCKET": state["socket"],
                                        "CEDEGRID_QUALIFIER_TOKEN": configuration["token"]},
                                   replay_safe=True, allocation_class=args.allocation_class,
                                   single_process=True, no_escape=True, max_attempts=4)
        client.put_pool(pool_id, [configuration["node_id"]], max_workers=1, allocation_class=args.allocation_class)
        # A failed submit can still be accepted remotely; cleanup treats it as owned.
        submitted = True
        client.submit(job_id, pool_id, [task])
        first, final, action_done, first_released = None, None, False, False
        admitted, running, drained_seen = set(), set(), set()
        accepted = {}
        while True:
            now = time.monotonic()
            require(now < deadline, "SCENARIO_DEADLINE")
            state = watchdog.request()
            require(not state["failure"], state["failure"] or "WATCHDOG_FAILED")
            client.timeout = min(2.0, max(0.1, deadline - now))
            allocations = list(client.iter_status("allocations", job_id=job_id, limit=100))
            records = list(client.iter_status("reported_allocations", node_id=configuration["node_id"], job_id=job_id, limit=100))
            tasks = list(client.iter_status("tasks", job_id=job_id, limit=100))
            for allocation in allocations + records:
                key, phase = allocation["assignment_id"], allocation["phase"]
                if phase in ("prepared", "authorized", "running", "draining", "released"):
                    admitted.add(key)
                if phase in ("running", "draining"):
                    running.add(key)
                if phase == "draining":
                    drained_seen.add(key)
            for event in state["events"]:
                receipt = event.get("receipt", {}).get("receipt", {}).get("receipt_hash")
                if receipt in accepted:
                    continue
                checked = validate_submission(event, task_id, args.seed)
                require(event["submission"]["result"]["metadata"].get("gpu_uuid") == args.gpu_uuid,
                        "RESULT_GPU_IDENTITY_MISMATCH")
                require(any(h["role"] == "worker" and h["attempt"] and
                            h["attempt"]["assignment_id"] == checked["assignment_id"] and
                            h["attempt"]["generation"] == checked["generation"] for h in state["handles"]),
                        "ACCEPTANCE_WITHOUT_OWNED_PROCESS")
                accepted[receipt] = checked
                if event["checkpoint"] and first is None:
                    first = checked
                if not event["checkpoint"]:
                    final = checked
            if first and not action_done:
                require(first["assignment_id"] in admitted, "CHECKPOINT_WITHOUT_ADMISSION")
                if args.scenario == "lifecycle":
                    client.drain_node(configuration["node_id"], True)
                    drained = True
                elif args.scenario == "lease-loss":
                    lease_page = client.status_page("allocations", job_id=job_id, limit=100)
                    require(lease_page.get("next_cursor") is None, "LEASE_EVIDENCE_BOUND_EXCEEDED")
                    lease = next(value for value in lease_page["items"]
                                 if value["assignment_id"] == first["assignment_id"])
                    remaining_ms = lease["lease_deadline_ms"] - lease_page["observed_at_unix_ms"]
                    require(0 < remaining_ms <= 12000, "BOUNDED_AUTHORITATIVE_LEASE_REQUIRED")
                    # A renewal may have been in flight at the status read.
                    # Pause for the whole bounded maximum, not just its observed
                    # remaining time. The watchdog always sends SIGCONT itself.
                    watchdog.request("pause_agent", seconds=15.0)
                    proof["planned_lease_loss"] = True
                else:
                    watchdog.request("competition")
                action_done = True
            if first and action_done and not first_released:
                released = any(a["assignment_id"] == first["assignment_id"] and a["phase"] == "released" for a in allocations)
                dead = any(h["role"] == "worker" and h["attempt"] and
                           h["attempt"]["assignment_id"] == first["assignment_id"] and not h["alive"]
                           for h in state["handles"])
                if released and dead:
                    first_released = True
                    if drained:
                        client.drain_node(configuration["node_id"], False)
                        drained = False
            if final and first:
                require(any(t.get("status") == "completed" and t.get("receipt_hash") == final["receipt_hash"]
                            for t in tasks), "FINAL_RECEIPT_NOT_IN_COORDINATOR")
                result = client.result(task_id)
                event = {"event": "publication_accepted", "checkpoint": False, "submission": result,
                         "receipt": {"kind": "receipt", "receipt": {"task_id": task_id,
                                    "assignment_id": final["assignment_id"], "generation": final["generation"],
                                    "receipt_hash": final["receipt_hash"]}}}
                require(validate_submission(event, task_id, args.seed) == final, "ACCEPTED_RESULT_MISMATCH")
                continuity = args.scenario == "competition" and args.competition_expect == "continue"
                if continuity:
                    require(final["generation"] == first["generation"] and final["resumed_from"] == 0,
                            "GUARANTEED_CONTINUITY_FAILED")
                else:
                    require(first_released and final["generation"] > first["generation"] and
                            final["resumed_from"] >= first["completed_batches"], "HIGHER_GENERATION_RESUME_MISSING")
                    if args.scenario != "lease-loss":
                        require(any(value["generation"] == first["generation"] and
                                    value["completed_batches"] > first["completed_batches"] for value in accepted.values()),
                                "COOPERATIVE_DRAIN_CHECKPOINT_MISSING")
                require(first["assignment_id"] in running, "RUNNING_PROGRESS_NOT_OBSERVED")
                require(first["assignment_id"] in state["sampled_attempts"] and
                        final["assignment_id"] in state["sampled_attempts"], "OWNED_CUDA_CONTEXT_NOT_OBSERVED")
                if args.scenario == "competition":
                    require(state["overlap_samples"] > 0 and state["competition_complete"],
                            "USEFUL_OWNED_COMPETITION_NOT_VERIFIED")
                require(time.monotonic() < deadline, "SCENARIO_DEADLINE")
                proof.update(task_id=task_id, admitted=True, useful_gpu_progress=True,
                             first_checkpoint=first, accepted_final=final,
                             higher_generation_resume=not continuity, guaranteed_continuity=continuity,
                             accepted_checkpoint_count=sum(1 for e in state["events"] if e["checkpoint"]),
                             cooperative_drain_checkpoint=not continuity and args.scenario != "lease-loss",
                             first_attempt_released=first_released,
                             observed_draining=first["assignment_id"] in drained_seen)
                metrics["scenario_elapsed_seconds"] = time.monotonic() - state["armed"]
                status, reason = "PASS", "SCENARIO_AND_CLEANUP_VERIFIED"
                break
            time.sleep(0.075)
    except BaseException as exc:
        reason = getattr(exc, "code", "SCENARIO_FAILED" if armed else "PREREQUISITE_UNAVAILABLE")
        status = "FAIL" if armed else "BLOCKED"
        private_error = {"type": type(exc).__name__, "detail": str(exc)}
    finally:
        cleanup_start = time.monotonic()
        cleanup_deadline = cleanup_start + CLEANUP_SECONDS
        if watchdog:
            try:
                watchdog.request("finish" if status == "PASS" else "fail")
            except Exception:
                pass  # The watchdog detects control EOF and independently cleans.
        if client and submitted:
            try:
                client.timeout = min(2.0, CLEANUP_SECONDS)
                client.cancel(job_id)
                while time.monotonic() < cleanup_deadline:
                    values = list(client.iter_status("allocations", job_id=job_id, limit=100))
                    if values and all(value["phase"] == "released" for value in values):
                        cleanup_released = True
                        break
                    time.sleep(0.1)
                if drained:
                    others = list(client.iter_status("allocations", node_id=configuration["node_id"], limit=100))
                    require(all(value["job_id"] == job_id or value["phase"] == "released" for value in others),
                            "NODE_EXCLUSIVITY_LOST")
                    client.drain_node(configuration["node_id"], False)
                    drained = False
            except Exception as exc:
                private_error = private_error or {"type": type(exc).__name__, "detail": str(exc)}
        final_state = None
        if watchdog:
            try:
                # Keep the control endpoint alive while awaiting autonomous cleanup.
                remaining = max(0.01, cleanup_deadline - time.monotonic())
                watchdog.process.wait(timeout=remaining)
                final_state = json.loads(read_bytes(root / "watchdog-final-private.json"))
            except Exception as exc:
                private_error = private_error or {"type": type(exc).__name__, "detail": str(exc)}
            finally:
                watchdog.close()
        metrics["cleanup_elapsed_seconds"] = time.monotonic() - cleanup_start
        if final_state:
            metrics.update({key: final_state[key] for key in ("samples", "peak_owned_vram_bytes", "max_sample_gap_ms", "overlap_samples", "peak_foreign_process_count")})
            handles = final_state["handles"]
            proof.update(watchdog_completed=True, owned_contexts_absent=final_state["owned_contexts_absent"],
                         all_registered_processes_exited=all(not handle["alive"] for handle in handles),
                         direct_children_reaped=all(handle["reaped"] is not False for handle in handles),
                         reservations_released=cleanup_released,
                         workload_reaping_verified_by_released=cleanup_released,
                         registered_worker_count=sum(handle["role"] == "worker" for handle in handles))
        verified_cleanup = bool(final_state and not final_state["failure"] and
                                all(not h["alive"] and h["reaped"] is not False for h in final_state["handles"]) and
                                final_state["owned_contexts_absent"] and cleanup_released and not drained and
                                metrics["cleanup_elapsed_seconds"] <= CLEANUP_SECONDS)
        if armed and (status != "PASS" or not verified_cleanup):
            status = "FAIL"
            if reason == "SCENARIO_AND_CLEANUP_VERIFIED":
                reason = final_state["failure"] if final_state and final_state["failure"] else "CLEANUP_UNVERIFIED"
        atomic_json(root / "diagnostic-private.json", {"error": private_error, "submitted": submitted,
                    "armed": armed, "drain_remaining": drained, "cleanup_verified": verified_cleanup})
        public = public_report(args, status, reason, proof, metrics, versions)
        atomic_json(root / "public-report.json", public)
        print(json.dumps(public, sort_keys=True))
    return {"PASS": 0, "BLOCKED": 2, "FAIL": 1}[status]


def build(args):
    source = Path(__file__).resolve().parents[1] / "examples" / "gpu_counter.c"
    target = Path(args.output).absolute()
    require(not target.exists(), "BUILD_OUTPUT_ALREADY_EXISTS")
    target.parent.mkdir(parents=True, exist_ok=True)
    compiler = shutil.which(args.cc)
    require(compiler is not None, "C_COMPILER_REQUIRED")
    command = [compiler, "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", str(source)]
    if sys.platform.startswith("linux"):
        command.append("-ldl")
    command += ["-o", str(target)]
    subprocess.run(command, check=True, timeout=60, capture_output=True)
    result = subprocess.run([str(target), "--self-test"], check=True, timeout=5, capture_output=True)
    check = json.loads(result.stdout)
    require(check.get("checksum_hex") == reference_checksum(7, BATCHES) and check.get("gpu_status") == "NOT_RUN",
            "NATIVE_REFERENCE_MISMATCH")
    version = subprocess.run([compiler, "--version"], check=True, timeout=5, capture_output=True).stdout.decode("utf-8", "replace").splitlines()[0]
    report = {"schema": SCHEMA, "status": "PASS", "scope": "build_and_cpu_self_test_only",
              "gpu_status": "NOT_RUN", "source_sha256": sha256(source), "worker_sha256": sha256(target),
              "compiler": version, "flags": ["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"],
              "cpu_reference": check}
    atomic_json(args.report, report)
    print(json.dumps(report, sort_keys=True))
    return 0


def self_test(args):
    # Deliberately does not instantiate NVML or launch an agent.
    require(reference_checksum(7, BATCHES) == "0008000f21e00000", "CPU_REFERENCE_REGRESSION")
    result = {"algorithm": "lcg32-v1", "seed": 7, "elements": ELEMENTS, "rounds": ROUNDS,
              "total_batches": BATCHES, "completed_batches": 2, "resumed_from": 0,
              "checksum_hex": reference_checksum(7, 2), "gpu_uuid": "private-test-uuid",
              "gpu_verified_values": 2 * ELEMENTS}
    descriptor = {"schema_version": 2, "publication_id": "test-publication", "kind": "checkpoint",
                  "task_id": "test-task", "assignment_id": "test-attempt", "generation": 1,
                  "namespace_id": "private-namespace", "session_id": "private-session",
                  "checkpoint_sequence": 1, "metadata": result, "artifacts": []}
    submission = {"task_id": "test-task", "assignment_id": "test-attempt", "generation": 1,
                  "result": json.loads(json.dumps(descriptor, sort_keys=True)), "artifacts": []}
    digest = hashlib.sha256(encode(submission)).hexdigest()
    event = {"event": "publication_accepted", "checkpoint": True, "submission": submission,
             "receipt": {"kind": "receipt", "receipt": {"task_id": "test-task", "assignment_id": "test-attempt",
                         "generation": 1, "receipt_hash": digest}}}
    require(validate_submission(event, "test-task", 7)["receipt_hash"] == digest, "RECEIPT_FIXTURE_FAILED")
    for mutation in ("wrong_hash", "wrong_generation", "wrong_result", "wrong_kind"):
        broken = json.loads(json.dumps(event))
        if mutation == "wrong_hash":
            broken["receipt"]["receipt"]["receipt_hash"] = "0" * 64
        elif mutation == "wrong_generation":
            broken["receipt"]["receipt"]["generation"] = 2
        elif mutation == "wrong_result":
            broken["submission"]["result"]["metadata"]["checksum_hex"] = "0" * 16
            # A receipt can faithfully hash an incorrect GPU result. The CPU
            # reference, not merely the transport digest, must reject it.
            broken["receipt"]["receipt"]["receipt_hash"] = hashlib.sha256(encode(broken["submission"])).hexdigest()
        else:
            broken["checkpoint"] = False
        try:
            validate_submission(broken, "test-task", 7)
        except GateError:
            pass
        else:
            raise GateError("INVALID_RECEIPT_ACCEPTED", mutation)
    # Check the public allowlist with private material in otherwise valid input.
    values = argparse.Namespace(host_role="gpu-ext4", model="rtx-5060-ti", execution_mode="contention_aware",
                                allocation_class="opportunistic", scenario="lifecycle", competition_expect=None,
                                native_sha256="a" * 64, worker_sha256="b" * 64, wheel_sha256="c" * 64,
                                gpu_uuid="SENSITIVE_UUID", evidence_dir="SENSITIVE_PATH", node_id="SENSITIVE_NODE")
    public = encode(public_report(values, "BLOCKED", "TEST", {}, {}, {"driver_version": "SENSITIVE_DRIVER_PATH"}))
    require(b"SENSITIVE" not in public and b"gpu_uuid" not in public and b"node_id" not in public,
            "PUBLIC_ALLOWLIST_FAILED")
    telemetry_checks = telemetry_self_test()
    if args.worker_binary:
        native = subprocess.run([str(Path(args.worker_binary).resolve()), "--self-test"], check=True,
                                timeout=5, capture_output=True)
        require(json.loads(native.stdout)["checksum_hex"] == reference_checksum(7, BATCHES),
                "NATIVE_REFERENCE_MISMATCH")
    print(json.dumps({"status": "PASS", "scope": "cpu_reference_receipts_and_public_allowlist_only",
                      "gpu_status": "NOT_RUN", "watchdog_live_status": "NOT_RUN", "checks": 7 + telemetry_checks,
                      "mock_telemetry_checks": telemetry_checks}))
    return 0


def telemetry_self_test():
    """Exercise decision logic using synthetic samples; no GPU status is earned."""
    class Handle:
        def __init__(self, pid, role, alive=True):
            self.pid, self.role, self.live = pid, role, alive
            self.attempt = {"assignment_id": "test-attempt"} if role == "worker" else None
        def alive(self):
            return self.live

    def fixture():
        watched = object.__new__(Watchdog)
        watched.config, watched.ready, watched.armed = {}, True, time.monotonic() - 1
        watched.handles = [Handle(11, "worker"), Handle(12, "competition")]
        watched.last_sample, watched.maximum_gap = None, 0.0
        watched.samples = watched.maximum = watched.overlap_samples = watched.zero_samples = 0
        watched.sampled_attempts = set()
        watched.foreign_process_count = watched.peak_foreign_process_count = 0
        watched.free_device_bytes = None
        watched.record = lambda _: None
        return watched

    def sample(memory):
        now = time.monotonic()
        return {"event": "sample", "scheduled": now, "started": now, "completed": now, "processes": memory,
                "device_memory": {"total": 16 * CAP_BYTES, "free": 8 * CAP_BYTES, "used": 8 * CAP_BYTES}}

    watched = fixture()
    watched.sample(sample({"11": 1024, "12": 2048}))
    require(watched.maximum == 3072 and watched.overlap_samples == 1 and
            watched.sampled_attempts == {"test-attempt"}, "MOCK_OWNERSHIP_ACCOUNTING_FAILED")
    for processes, code in (({"11": CAP_BYTES + 1}, "OWNED_VRAM_CAP_EXCEEDED"),
                            ({"11": None}, "NVML_MEMORY_UNKNOWN")):
        try:
            fixture().sample(sample(processes))
        except GateError as exc:
            require(exc.code == code, "MOCK_FAILURE_CLASSIFICATION_FAILED")
        else:
            raise GateError("MOCK_UNSAFE_SAMPLE_ACCEPTED")
    watched = fixture()
    watched.sample(sample({"99": 4 * CAP_BYTES}))
    require(watched.maximum == 0 and watched.foreign_process_count == 1 and watched.zero_samples == 1,
            "MOCK_FOREIGN_CONTEXT_CHANGED_OWNED_PROOF")
    watched.sample(sample({"99": 4 * CAP_BYTES, "11": 1024}))
    require(watched.maximum == 1024 and watched.foreign_process_count == 1 and watched.zero_samples == 0,
            "MOCK_FOREIGN_MEMORY_CHARGED_TO_OWNED_CAP")
    watched = fixture()
    watched.handles[0].live = False
    watched.sample(sample({"11": 1024}))
    require(watched.maximum == 1024 and not watched.sampled_attempts and watched.zero_samples == 0,
            "MOCK_EXITED_PID_ACCOUNTING_FAILED")
    watched = fixture()
    watched.last_sample = time.monotonic() - MAX_SAMPLE_GAP - 1
    try:
        watched.sample(sample({}))
    except GateError as exc:
        require(exc.code == "TELEMETRY_SCHEDULE_MISSED", "MOCK_GAP_CLASSIFICATION_FAILED")
    else:
        raise GateError("MOCK_TELEMETRY_GAP_ACCEPTED")

    inventory = object.__new__(Nvml)
    inventory.device = None
    def query(memory):
        def fill(_device, count, array_values):
            count._obj.value = 1
            array_values[0].pid, array_values[0].usedGpuMemory = 11, memory
            return 0
        return fill
    inventory.queries = [query(12), query(24)]
    require(inventory.processes() == {11: 24}, "MOCK_DUPLICATE_CONTEXT_DOUBLE_COUNTED")
    inventory.queries = [query((1 << 64) - 1)]
    try:
        inventory.processes()
    except GateError as exc:
        require(exc.code == "NVML_MEMORY_UNKNOWN", "MOCK_UNKNOWN_MEMORY_CLASSIFICATION_FAILED")
    else:
        raise GateError("MOCK_UNKNOWN_MEMORY_ACCEPTED")
    return 9


def parser():
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = result.add_subparsers(dest="command", required=True)
    compile_parser = sub.add_parser("build", help="Compile embedded PTX workload; validate CPU reference without GPU")
    compile_parser.add_argument("--cc", default="cc")
    compile_parser.add_argument("--output", required=True)
    compile_parser.add_argument("--report", required=True)
    checks = sub.add_parser("self-test", help="CPU/reference/receipt/privacy checks; never a GPU qualification")
    checks.add_argument("--worker-binary")
    run = sub.add_parser("run", help="Run one explicitly authorized tuple on a dedicated Linux GPU test node")
    run.add_argument("--native-binary", required=True, help="Installed final native executable outside the checkout")
    run.add_argument("--native-sha256", required=True)
    run.add_argument("--worker-binary", required=True)
    run.add_argument("--worker-sha256", required=True)
    run.add_argument("--build-report", required=True)
    run.add_argument("--wheel", required=True, help="Original final wheel whose installed code is checked byte for byte")
    run.add_argument("--wheel-sha256", required=True)
    run.add_argument("--node-config", required=True)
    run.add_argument("--agent-deployment", required=True)
    run.add_argument("--client-config", required=True)
    run.add_argument("--gpu-uuid", required=True)
    run.add_argument("--model", choices=MODELS, required=True)
    run.add_argument("--host-role", choices=("gpu-ext4", "gpu-replay"), required=True)
    run.add_argument("--scenario", choices=("lifecycle", "lease-loss", "competition"), required=True)
    run.add_argument("--execution-mode", choices=("auto", "contention_aware", "conservative_non_sharing", "best_effort_occupied"), required=True)
    run.add_argument("--allocation-class", choices=("opportunistic", "guaranteed"), required=True)
    run.add_argument("--competition-expect", choices=("yield", "continue"))
    run.add_argument("--seed", type=int, default=7)
    run.add_argument("--evidence-dir", required=True, help="New private directory; publish only public-report.json")
    run.add_argument("--dedicated-test-node", action="store_true", help="Authorize test-owned agent and node drain; no other users/jobs on this node")
    return result


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "_sampler":
        return sampler_main(sys.argv[2])
    if len(sys.argv) == 4 and sys.argv[1] == "_watchdog":
        os.umask(0o077)
        return Watchdog(json.loads(read_bytes(sys.argv[2])), int(sys.argv[3])).run()
    args = parser().parse_args()
    os.umask(0o077)
    try:
        return {"build": build, "self-test": self_test, "run": run_gate}[args.command](args)
    except Exception as exc:
        # Raw toolchain/config exceptions can contain local paths. Keep CLI errors
        # neutral; detailed run errors are emitted only to the private directory.
        print(json.dumps({"status": "BLOCKED", "reason_code": getattr(exc, "code", "LOCAL_CHECK_FAILED"),
                          "gpu_status": "NOT_RUN"}))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
