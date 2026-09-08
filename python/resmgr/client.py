"""Version-one RPC client. Workloads never receive operator credentials."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import ssl
import math
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from .process import _argv, _boolean, _cwd, _environment, _identity, _interval, _text, _uint


class RemoteError(RuntimeError):
    pass


def _artifact(value):
    if not isinstance(value, dict) or set(value) != {"sha256", "size"}:
        raise ValueError("artifact must contain sha256 and size")
    digest = value["sha256"]
    if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        raise ValueError("artifact sha256 must be 64 lowercase hexadecimal characters")
    _uint(value["size"], "artifact size")
    return dict(value)


def command_task(task_id: str, argv: list[str], cwd: str, *, cpu_millicores: int = 1000,
                 ram_mib: int = 512, gpu_vram_mib: dict[str, int] | None = None,
                 env: dict[str, str] | None = None, replay_safe: bool = False,
                 allocation_class: str = "guaranteed", required_controls: list[str] | None = None,
                 single_process: bool = False, no_escape: bool = False, managed_child_limit: int = 0,
                 input_artifacts: list[dict] | None = None, max_attempts: int | None = None) -> dict:
    """Build a task after validating its transport-independent input contract."""
    _identity(task_id, "task_id")
    args, directory, environment = _argv(argv), _cwd(cwd), _environment(env)
    _uint(cpu_millicores, "cpu_millicores")
    _uint(ram_mib, "ram_mib")
    for name, value in (("replay_safe", replay_safe), ("single_process", single_process), ("no_escape", no_escape)):
        _boolean(value, name)
    if not isinstance(allocation_class, str) or allocation_class not in {"guaranteed", "opportunistic"}:
        raise ValueError("allocation_class must be guaranteed or opportunistic")
    _uint(managed_child_limit, "managed_child_limit", maximum=8)
    if managed_child_limit and (single_process or not no_escape):
        raise ValueError("mediated children require no_escape and single_process=False")
    if gpu_vram_mib is not None and not isinstance(gpu_vram_mib, dict):
        raise ValueError("gpu_vram_mib must be a GPU-identity-to-integer mapping")
    gpu = {}
    for key, value in (gpu_vram_mib or {}).items():
        gpu[_identity(key, "GPU identity")] = _uint(value, "GPU memory MiB")
    if required_controls is not None and not isinstance(required_controls, (list, tuple)):
        raise ValueError("required_controls must be a list")
    controls = [_text(value, "required control") for value in (required_controls or [])]
    if len(set(controls)) != len(controls):
        raise ValueError("required_controls must be unique")
    artifacts = []
    names = set()
    if input_artifacts is not None:
        if not isinstance(input_artifacts, list):
            raise ValueError("input_artifacts must be a list")
        for value in input_artifacts:
            if not isinstance(value, dict) or set(value) != {"name", "sha256", "size"}:
                raise ValueError("named artifact must contain name, sha256 and size")
            name = _identity(value["name"], "artifact name")
            if name in names:
                raise ValueError("artifact names must be unique")
            names.add(name)
            artifacts.append({"name": name, **_artifact({"sha256": value["sha256"], "size": value["size"]})})
    task = {"task_id": task_id, "assignment_id": "", "argv": args, "cwd": directory,
            "env": environment, "resources": {"cpu_millicores": cpu_millicores,
            "ram_mib": ram_mib, "gpu_memory_mib": gpu}, "replay_safe": replay_safe,
            "class": allocation_class, "no_escape": no_escape, "single_process": single_process,
            "required_controls": controls, "allow_fallback": True}
    if max_attempts is not None:
        task["max_attempts"] = _uint(max_attempts, "max_attempts", minimum=1, maximum=(1 << 32) - 1)
    if artifacts:
        task["input_artifacts"] = artifacts
    if managed_child_limit:
        task["managed_child_limit"] = managed_child_limit
    return task


class _TransferPacer:
    """Aggregate application-byte pacing, including hex expansion and 10% headroom."""
    def __init__(self, bytes_per_second, *, clock=time.monotonic, sleep=time.sleep):
        _interval(bytes_per_second, "max_transfer_bytes_per_second")
        self.rate = bytes_per_second * 0.9
        self.clock, self.sleep = clock, sleep
        self.lock, self.next = threading.Lock(), 0.0

    def account(self, byte_count):
        if byte_count <= 0:
            return
        with self.lock:
            now = self.clock()
            self.next = max(now, self.next) + byte_count / self.rate
            delay = self.next - now
        self.sleep(delay)


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate configuration field: " + key)
        result[key] = value
    return result


class Client:
    def __init__(self, endpoint: str, *, ca: str | Path, certificate: str | Path,
                 private_key: str | Path, timeout: float = 15.0,
                 max_transfer_bytes_per_second: float = 10 * 1024 * 1024):
        _text(endpoint, "endpoint")
        if any(ord(c) <= 32 or ord(c) == 127 for c in endpoint) or "\\" in endpoint:
            raise ValueError("endpoint must not contain whitespace, controls or backslashes")
        parsed = urllib.parse.urlsplit(endpoint)
        if parsed.scheme != "https" or not parsed.hostname or parsed.username is not None or parsed.password is not None:
            raise ValueError("an HTTPS endpoint without embedded credentials is required")
        if "?" in endpoint or "#" in endpoint or parsed.path not in ("", "/"):
            raise ValueError("endpoint must be an HTTPS origin without a path, query or fragment")
        if parsed.port is not None and not 1 <= parsed.port <= 65535:
            raise ValueError("endpoint port must be in 1..65535")
        _interval(timeout, "timeout")
        self.url = endpoint.rstrip("/") + "/v1/rpc"
        self.timeout = timeout
        self.pacer = _TransferPacer(max_transfer_bytes_per_second)
        self.context = ssl.create_default_context(cafile=str(ca))
        self.context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.context.load_cert_chain(str(certificate), str(private_key))
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                urllib.request.HTTPSHandler(context=self.context), _NoRedirect())

    @classmethod
    def from_config(cls, path: str | Path) -> "Client":
        # This remains the legacy configuration reader during the scoped repair;
        # the release's TOML-only, cross-language schema is a separate pending gate.
        config_path = Path(path).resolve(strict=True)
        with config_path.open("rb") as stream:
            raw = stream.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise ValueError("client configuration exceeds 1 MiB")
        value = json.loads(raw, object_pairs_hook=_unique_object)
        if not isinstance(value, dict) or not {"endpoint", "tls"} <= set(value) or set(value) - {"endpoint", "tls", "timeout_seconds", "max_transfer_bytes_per_second"}:
            raise ValueError("invalid client configuration fields")
        tls = value["tls"]
        if not isinstance(tls, dict) or set(tls) != {"ca_cert", "certificate", "private_key"}:
            raise ValueError("invalid TLS configuration fields")
        def resolve(key):
            configured = Path(_text(tls[key], "TLS path"))
            # Do not expand '~', environment variables or shell expressions.
            return configured if configured.is_absolute() else config_path.parent / configured
        return cls(value["endpoint"], ca=resolve("ca_cert"), certificate=resolve("certificate"),
                   private_key=resolve("private_key"), timeout=value.get("timeout_seconds", 15),
                   max_transfer_bytes_per_second=value.get("max_transfer_bytes_per_second", 10 * 1024 * 1024))

    def request(self, op: str, **payload) -> dict:
        _identity(op, "operation")
        for field in ("task_id", "job_id", "pool_id", "node_id", "assignment_id", "upload_id"):
            if field in payload and payload[field] is not None:
                _identity(payload[field], field)
        if "generation" in payload:
            _uint(payload["generation"], "generation")
        if "offset" in payload:
            _uint(payload["offset"], "offset")
        if op == "read_artifact":
            _uint(payload.get("max_bytes"), "max_bytes", minimum=1, maximum=1024 * 1024)
        data = json.dumps({"op": op, **payload}, allow_nan=False, separators=(",", ":")).encode()
        reserved_reply = 2 * payload["max_bytes"] + 512 if op == "read_artifact" else 0
        self.pacer.account(len(data) + reserved_reply)
        req = urllib.request.Request(self.url, data=data, headers={"Content-Type": "application/json"}, method="POST")
        try:
            with self.opener.open(req, timeout=self.timeout) as stream:
                response_limit = (8 if op in {"status", "status_page"} else 3) * 1024 * 1024
                raw = stream.read(response_limit + 1)
                self.pacer.account(max(0, len(raw) - reserved_reply))
                if len(raw) > response_limit:
                    raise RemoteError("RPC response exceeds the client bound")
                response = json.loads(raw)
        except urllib.error.HTTPError as error:
            raise RemoteError(f"RPC HTTP failure: {error.code}") from error
        except (ValueError, UnicodeError) as error:
            raise RemoteError("malformed RPC response JSON") from error
        if not isinstance(response, dict) or not isinstance(response.get("kind"), str):
            raise RemoteError("malformed RPC response structure")
        if response["kind"] == "error":
            raise RemoteError(response.get("message", "RPC rejected"))
        return response

    def put_pool(self, pool_id: str, node_ids: list[str], *, max_workers: int,
                 min_workers: int = 0, allocation_class: str = "guaranteed") -> dict:
        _identity(pool_id, "pool_id")
        if not isinstance(node_ids, list) or not node_ids:
            raise ValueError("node_ids must be a nonempty list")
        nodes = [_identity(value, "node_id") for value in node_ids]
        _uint(max_workers, "max_workers", maximum=(1 << 32) - 1)
        _uint(min_workers, "min_workers", maximum=max_workers)
        if not isinstance(allocation_class, str) or allocation_class not in {"guaranteed", "opportunistic"}:
            raise ValueError("invalid allocation class")
        return self.request("put_pool", pool={"pool_id": pool_id, "node_ids": nodes,
            "class": allocation_class, "min_workers": min_workers, "max_workers": max_workers})

    def submit(self, job_id: str, pool_id: str, tasks: list[dict], *, priority: int = 0) -> dict:
        _identity(job_id, "job_id")
        _identity(pool_id, "pool_id")
        if type(priority) is not int or not -(1 << 31) <= priority < (1 << 31):
            raise ValueError("priority must be a signed 32-bit integer")
        if not isinstance(tasks, list) or not tasks or any(not isinstance(task, dict) for task in tasks):
            raise ValueError("tasks must be a nonempty list of task objects")
        return self.request("submit", job={"job_id": job_id, "pool_id": pool_id, "priority": priority, "tasks": tasks})

    def status(self, job_id: str | None = None) -> dict:
        if job_id is not None:
            _identity(job_id, "job_id")
        return self.request("status", job_id=job_id)

    def cancel(self, job_id: str) -> dict:
        _identity(job_id, "job_id")
        return self.request("cancel", job_id=job_id)

    def drain_node(self, node_id: str, drain: bool = True) -> dict:
        _identity(node_id, "node_id")
        _boolean(drain, "drain")
        return self.request("drain_node", node_id=node_id, drain=drain)

    def result(self, task_id: str) -> dict | None:
        _identity(task_id, "task_id")
        return self.request("get_result", task_id=task_id)["submission"]

    def resume(self, task_id: str, *, side_effects_reconciled: bool = False) -> dict:
        """Retry after proven release; unsafe side effects require acknowledgement."""
        _identity(task_id, "task_id")
        _boolean(side_effects_reconciled, "side_effects_reconciled")
        return self.request("retry", task_id=task_id, confirm_side_effects_reconciled=side_effects_reconciled)

    def upload(self, path: Path, assignment_id: str, generation: int) -> dict:
        _identity(assignment_id, "assignment_id")
        _uint(generation, "generation")
        path = Path(path)
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
            size = stream.tell()
        artifact = {"sha256": digest.hexdigest(), "size": size}
        reply = self.request("begin_upload", assignment_id=assignment_id, generation=generation, artifact=artifact)
        upload_id = _identity(reply.get("upload_id"), "upload_id")
        offset = _uint(reply.get("offset"), "upload offset", maximum=size)
        with path.open("rb") as stream:
            stream.seek(offset)
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                reply = self.request("upload_chunk", upload_id=upload_id, offset=offset, data_hex=chunk.hex())
                offset += len(chunk)
                if type(reply.get("offset")) is not int or reply["offset"] != offset or offset > size:
                    raise RemoteError("upload offset mismatch")
        return self.request("commit_upload", upload_id=upload_id)["artifact"]

    def download(self, artifact: dict, destination: Path) -> Path:
        from .worker import durable_mkdir, durable_publish
        artifact = _artifact(artifact)
        destination = Path(destination)
        durable_mkdir(destination.parent)
        temporary = destination.with_name("." + destination.name + "." + uuid.uuid4().hex + ".download")
        digest = hashlib.sha256()
        offset = 0
        try:
            with temporary.open("xb") as stream:
                while offset < artifact["size"]:
                    requested = min(1024 * 1024, artifact["size"] - offset)
                    reply = self.request("read_artifact", sha256=artifact["sha256"], offset=offset, max_bytes=requested)
                    try:
                        chunk = bytes.fromhex(reply["data_hex"])
                    except (KeyError, TypeError, ValueError) as error:
                        raise RemoteError("malformed artifact chunk") from error
                    if not chunk or len(chunk) > requested:
                        raise RemoteError("invalid artifact chunk")
                    stream.write(chunk)
                    digest.update(chunk)
                    offset += len(chunk)
                stream.flush()
                os.fsync(stream.fileno())
            if digest.hexdigest() != artifact["sha256"]:
                raise RemoteError("artifact integrity failure")
            durable_publish(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
        return destination


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise RemoteError("RPC redirects are forbidden")
