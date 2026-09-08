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


from .errors import RemoteError, ValidationError, DeadlineExceeded
from .codec import integer, U64_MAX
from .config import endpoint_origin, positive_number, load_client_config
from .transport import exchange, remaining


def _validate_int_field(name, value, *, minimum=None, maximum=U64_MAX):
    if type(value) is not int:
        raise ValidationError(f"{name} must be an integer")
    if minimum is not None and value < minimum:
        raise ValidationError(f"{name} out of range")
    if maximum is not None and value > maximum:
        raise ValidationError(f"{name} out of range")
    return value


def _validate_text_field(name, value):
    if type(value) is not str:
        raise ValidationError(f"{name} must be a string")
    if "\x00" in value:
        raise ValidationError(f"{name} cannot include NUL bytes")
    return value


def _resolve_config_tls_path(config_path: Path, value: str | Path) -> Path:
    value = Path(value)
    if value.is_absolute():
        return value
    return config_path.parent.joinpath(value)


def command_task(task_id: str, argv: list[str], cwd: str, *, cpu_millicores: int = 1000,
                 ram_mib: int = 512, gpu_vram_mib: dict[str, int] | None = None,
                 env: dict[str, str] | None = None, replay_safe: bool = False,
                 allocation_class: str = "guaranteed", required_controls: list[str] | None = None,
                 single_process: bool = False, no_escape: bool = False, managed_child_limit: int = 0,
                 input_artifacts: list[dict] | None = None, max_attempts: int | None = None) -> dict:
    """Build a task; callers must explicitly acknowledge the workload contract."""
    _validate_text_field("task_id", task_id)
    _validate_text_field("cwd", cwd)
    if not task_id or not (cwd.startswith("/") or (len(cwd) > 2 and cwd[1:3] in (":\\", ":/")) or cwd.startswith("\\\\")):
        raise ValidationError("task_id and an absolute target cwd are required")
    for flag, value in (("replay_safe", replay_safe), ("single_process", single_process), ("no_escape", no_escape)):
        if type(value) is not bool: raise ValidationError(f"{flag} must be boolean")
    if type(argv) not in (list, tuple): raise ValidationError("argv must be an explicit list")
    if env is not None:
        if type(env) is not dict: raise ValidationError("env must be a string mapping")
        for key, value in env.items():
            _validate_text_field("env key", key)
            _validate_text_field("env value", value)
            if not key or "=" in key: raise ValidationError("invalid environment key")
    if required_controls is not None and (type(required_controls) is not list or any(type(v) is not str or "\x00" in v for v in required_controls)):
        raise ValidationError("required_controls must be a string array")
    if not argv or allocation_class not in {"guaranteed", "opportunistic"}:
        raise ValidationError("task identity, argv and a valid allocation class are required")
    if any(type(value) is not str for value in argv):
        raise ValidationError("argv items must be strings")
    if any("\x00" in value for value in argv):
        raise ValidationError("argv entries cannot include NUL bytes")
    if not argv[0]:
        raise ValidationError("argv executable name must be non-empty")
    _validate_int_field("cpu_millicores", cpu_millicores, minimum=1)
    _validate_int_field("ram_mib", ram_mib, minimum=1)
    if type(managed_child_limit) is not int or not 0 <= managed_child_limit <= 8 or (
            managed_child_limit and (single_process or not no_escape)):
        raise ValidationError("mediated children require no_escape, single_process=False, and a limit in 1..8")
    if gpu_vram_mib is not None:
        if type(gpu_vram_mib) is not dict:
            raise ValidationError("gpu_vram_mib must be a string-keyed mapping")
        for key, value in gpu_vram_mib.items():
            _validate_text_field("gpu_vram_mib key", key)
            _validate_int_field(f"gpu_vram_mib[{key}]", value, minimum=0)
    task = {"task_id": task_id, "assignment_id": "", "argv": argv, "cwd": cwd,
            "env": env or {}, "resources": {"cpu_millicores": cpu_millicores,
            "ram_mib": ram_mib, "gpu_memory_mib": gpu_vram_mib or {}},
            "replay_safe": replay_safe, "class": allocation_class, "no_escape": no_escape,
            "single_process": single_process, "required_controls": required_controls or [], "allow_fallback": True}
    if max_attempts is not None:
        if type(max_attempts) is not int or not 1 <= max_attempts <= 4294967295:
            raise ValidationError("max_attempts must be a positive explicit reservation bound")
        task["max_attempts"] = max_attempts
    if input_artifacts:
        task["input_artifacts"] = input_artifacts
    if managed_child_limit:
        task["managed_child_limit"] = managed_child_limit
    return task


class _TransferPacer:
    """Per-client aggregate application-byte pacing with 10% framing headroom.

    Counts serialized request and response bytes, including hexadecimal expansion.
    This is not kernel traffic shaping and cannot measure TLS retransmissions.
    """
    def __init__(self, bytes_per_second, *, clock=time.monotonic, sleep=time.sleep):
        if type(bytes_per_second) is not int or not 0 <= bytes_per_second <= U64_MAX:
            raise ValidationError("max_transfer_bytes_per_second must be finite and positive")
        self.rate = bytes_per_second * 0.9
        self.clock, self.sleep = clock, sleep
        self.lock, self.next = threading.Lock(), 0.0

    def account(self, byte_count, *, deadline=None):
        if byte_count <= 0 or self.rate == 0:
            return
        with self.lock:
            now = self.clock()
            next_time = max(now, self.next) + byte_count / self.rate
            delay = next_time - now
            if deadline is not None and delay >= remaining(deadline):
                raise DeadlineExceeded("RPC deadline expires while pacing")
            self.next = next_time
        self.sleep(delay)


class Client:
    def __init__(self, endpoint: str, *, ca: str | Path, certificate: str | Path,
                 private_key: str | Path, timeout: float = 15.0,
                 max_transfer_bytes_per_second: float = 10 * 1024 * 1024):
        self.url = endpoint_origin(endpoint) + "/v1/rpc"
        self.timeout = positive_number(timeout, "timeout")
        self.pacer = _TransferPacer(max_transfer_bytes_per_second)
        self._request_lock = threading.Lock()
        self.context = ssl.create_default_context(cafile=str(ca))
        self.context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.context.load_cert_chain(str(certificate), str(private_key))

    @classmethod
    def from_config(cls, path: str | Path) -> "Client":
        return cls(**load_client_config(path))

    def request(self, op: str, **payload) -> dict:
        _validate_text_field("op", op)
        if "op" in payload:
            raise ValidationError("payload cannot override the operation")
        deadline = time.monotonic() + self.timeout
        if not self._request_lock.acquire(timeout=remaining(deadline)):
            raise DeadlineExceeded("RPC deadline expired in client queue")
        try:
            return exchange(self, op, payload, deadline)
        finally:
            self._request_lock.release()

    def status_page(self, collection: str, *, job_id=None, node_id=None, pool_id=None, limit=100, cursor=None) -> dict:
        _validate_int_field("limit", limit, minimum=1, maximum=1000)
        return self.request("status_page", collection=collection, job_id=job_id, node_id=node_id,
                            pool_id=pool_id, limit=limit, cursor=cursor)

    def iter_status(self, collection: str, **filters):
        """Traverse a fixed high-water bound; cursor errors are never restarted."""
        cursor = filters.pop("cursor", None)
        while True:
            page = self.status_page(collection, cursor=cursor, **filters)
            yield from page["items"]
            next_cursor = page.get("next_cursor")
            if next_cursor is None: return
            if type(next_cursor) is not str or not next_cursor or next_cursor == cursor:
                raise RemoteError("status cursor did not advance", code="ERR_CEDEGRID_PROTOCOL")
            cursor = next_cursor

    def put_pool(self, pool_id: str, node_ids: list[str], *, max_workers: int,
                 min_workers: int = 0, allocation_class: str = "guaranteed") -> dict:
        return self.request("put_pool", pool={"pool_id": pool_id, "node_ids": node_ids,
            "class": allocation_class, "min_workers": min_workers, "max_workers": max_workers})

    def submit(self, job_id: str, pool_id: str, tasks: list[dict], *, priority: int = 0) -> dict:
        return self.request("submit", job={"job_id": job_id, "pool_id": pool_id, "priority": priority, "tasks": tasks})

    def status(self, job_id: str | None = None) -> dict:
        return self.request("status", job_id=job_id)

    def cancel(self, job_id: str) -> dict:
        return self.request("cancel", job_id=job_id)

    def drain_node(self, node_id: str, drain: bool = True) -> dict:
        return self.request("drain_node", node_id=node_id, drain=drain)

    def result(self, task_id: str) -> dict | None:
        return self.request("get_result", task_id=task_id)["submission"]

    def resume(self, task_id: str, *, side_effects_reconciled: bool = False) -> dict:
        """Retry after proven release; unsafe side effects require explicit acknowledgement."""
        return self.request("retry", task_id=task_id, confirm_side_effects_reconciled=side_effects_reconciled)

    def upload(self, path: Path, assignment_id: str, generation: int) -> dict:
        path = Path(path)
        integer(generation, "generation", 1, (1 << 63) - 1)
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        artifact = {"sha256": digest.hexdigest(), "size": path.stat().st_size}
        reply = self.request("begin_upload", assignment_id=assignment_id, generation=generation, artifact=artifact)
        with path.open("rb") as stream:
            offset = reply["offset"]
            if not 0 <= offset <= artifact["size"]:
                raise RemoteError("invalid resumable upload offset")
            stream.seek(offset)
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                reply = self.request("upload_chunk", upload_id=reply["upload_id"], offset=offset, data_hex=chunk.hex())
                offset += len(chunk)
                if reply["offset"] != offset:
                    raise RemoteError("upload offset mismatch")
        return self.request("commit_upload", upload_id=reply["upload_id"])["artifact"]

    def download(self, artifact: dict, destination: Path, *, durability="strict") -> Path:
        from .worker import durable_mkdir, sync_directory, sha256_file
        from .errors import DownloadUncertain
        if durability not in ("strict", "portable"):
            raise ValidationError("download durability must be strict or portable")
        if durability == "strict" and os.name == "nt":
            raise ValidationError("strict download durability is unavailable on Windows; explicitly select portable")
        _validate_int_field("artifact size", artifact["size"], minimum=0)
        if type(artifact["sha256"]) is not str or len(artifact["sha256"]) != 64 or any(c not in "0123456789abcdef" for c in artifact["sha256"]):
            raise ValidationError("artifact SHA256 must be lowercase hexadecimal")
        destination = Path(destination)
        durable_mkdir(destination.parent, durability=durability)
        operation_id = uuid.uuid4().hex
        temporary = destination.with_name("." + destination.name + "." + operation_id + ".download")
        digest = hashlib.sha256()
        offset = 0
        try:
            with temporary.open("xb") as stream:
                while offset < artifact["size"]:
                    reply = self.request("read_artifact", sha256=artifact["sha256"], offset=offset, max_bytes=min(1024 * 1024, artifact["size"] - offset))
                    chunk = bytes.fromhex(reply["data_hex"])
                    if not chunk or offset + len(chunk) > artifact["size"]:
                        raise RemoteError("invalid artifact chunk")
                    stream.write(chunk)
                    digest.update(chunk)
                    offset += len(chunk)
                stream.flush()
                os.fsync(stream.fileno())
            if digest.hexdigest() != artifact["sha256"]:
                raise RemoteError("artifact integrity failure")
            published = False
            try:
                try:
                    os.link(temporary, destination)
                    published = True
                except FileExistsError:
                    if destination.is_symlink() or not destination.is_file() or sha256_file(destination) != artifact["sha256"]:
                        raise ValidationError("immutable download conflict")
                temporary.unlink()
                if durability == "strict":
                    sync_directory(destination.parent)
            except OSError as cause:
                if published:
                    raise DownloadUncertain(destination, operation_id, artifact["sha256"], cause=cause) from cause
                raise
        finally:
            try: temporary.unlink(missing_ok=True)
            except OSError: pass
        return destination


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise RemoteError("RPC redirects are forbidden")
