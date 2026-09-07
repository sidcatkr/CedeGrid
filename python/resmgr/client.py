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


class RemoteError(RuntimeError):
    pass


def command_task(task_id: str, argv: list[str], cwd: str, *, cpu_millicores: int = 1000,
                 ram_mib: int = 512, gpu_vram_mib: dict[str, int] | None = None,
                 env: dict[str, str] | None = None, replay_safe: bool = False,
                 allocation_class: str = "guaranteed", required_controls: list[str] | None = None,
                 single_process: bool = False, no_escape: bool = False, managed_child_limit: int = 0,
                 input_artifacts: list[dict] | None = None, max_attempts: int | None = None) -> dict:
    """Build a task; callers must explicitly acknowledge the workload contract."""
    if not task_id or not argv or allocation_class not in {"guaranteed", "opportunistic"}:
        raise ValueError("task identity, argv and a valid allocation class are required")
    if not 0 <= managed_child_limit <= 8 or (managed_child_limit and (single_process or not no_escape)):
        raise ValueError("mediated children require no_escape, single_process=False, and a limit in 1..8")
    task = {"task_id": task_id, "assignment_id": "", "argv": argv, "cwd": cwd,
            "env": env or {}, "resources": {"cpu_millicores": cpu_millicores,
            "ram_mib": ram_mib, "gpu_memory_mib": gpu_vram_mib or {}},
            "replay_safe": replay_safe, "class": allocation_class, "no_escape": no_escape,
            "single_process": single_process, "required_controls": required_controls or [], "allow_fallback": True}
    if max_attempts is not None:
        if type(max_attempts) is not int or max_attempts < 1:
            raise ValueError("max_attempts must be a positive explicit reservation bound")
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
        if not math.isfinite(bytes_per_second) or bytes_per_second <= 0:
            raise ValueError("max_transfer_bytes_per_second must be finite and positive")
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


class Client:
    def __init__(self, endpoint: str, *, ca: str | Path, certificate: str | Path,
                 private_key: str | Path, timeout: float = 15.0,
                 max_transfer_bytes_per_second: float = 10 * 1024 * 1024):
        parsed = urllib.parse.urlsplit(endpoint)
        if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
            raise ValueError("an HTTPS endpoint without embedded credentials is required")
        self.url = endpoint.rstrip("/") + "/v1/rpc"
        self.timeout = timeout
        self.pacer = _TransferPacer(max_transfer_bytes_per_second)
        self.context = ssl.create_default_context(cafile=str(ca))
        self.context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.context.load_cert_chain(str(certificate), str(private_key))
        # No environment proxy may receive client-authenticated workload traffic.
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                urllib.request.HTTPSHandler(context=self.context), _NoRedirect())

    @classmethod
    def from_config(cls, path: str | Path) -> "Client":
        value = json.loads(Path(path).read_text())
        return cls(value["endpoint"], ca=value["tls"]["ca_cert"],
                   certificate=value["tls"]["certificate"], private_key=value["tls"]["private_key"],
                   timeout=value.get("timeout_seconds", 15),
                   max_transfer_bytes_per_second=value.get("max_transfer_bytes_per_second", 10 * 1024 * 1024))

    def request(self, op: str, **payload) -> dict:
        data = json.dumps({"op": op, **payload}, allow_nan=False, separators=(",", ":")).encode()
        # Reserve a bounded artifact reply before requesting it. Other replies
        # are charged after receipt; each RPC remains bounded by the server.
        reserved_reply = 2 * int(payload.get("max_bytes", 0)) + 512 if op == "read_artifact" else 0
        self.pacer.account(len(data) + reserved_reply)
        req = urllib.request.Request(self.url, data=data, headers={"Content-Type": "application/json"}, method="POST")
        try:
            with self.opener.open(req, timeout=self.timeout) as stream:
                response_limit = (8 if op == "status" else 3) * 1024 * 1024
                raw = stream.read(response_limit + 1)
                self.pacer.account(max(0, len(raw) - reserved_reply))
                if len(raw) > response_limit:
                    raise RemoteError("RPC response exceeds the client bound")
                response = json.loads(raw)
        except urllib.error.HTTPError as error:
            raise RemoteError(f"RPC HTTP failure: {error.code}") from error
        if response.get("kind") == "error":
            raise RemoteError(response.get("message", "RPC rejected"))
        return response

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

    def download(self, artifact: dict, destination: Path) -> Path:
        from .worker import durable_publish
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_name("." + destination.name + "." + uuid.uuid4().hex + ".download")
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
            durable_publish(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
        return destination


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise RemoteError("RPC redirects are forbidden")
