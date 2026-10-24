"""Credential-free native worker protocol and operator download file helpers."""
from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import json
import logging
import os
from pathlib import Path
import stat
import uuid


from .errors import CedeGridError

class DrainRequested(CedeGridError):
    code = "ERR_CEDEGRID_DRAIN_REQUESTED"
    """Raised only at a workload-declared safe cancellation boundary."""


def canonical_json(value) -> bytes:
    from .codec import stringify_json
    return stringify_json(value, sort_keys=True).encode()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sync_directory(path: Path) -> None:
    if os.name == "nt":
        raise NotImplementedError("strict directory durability is unavailable on Windows")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def durable_mkdir(path: Path, *, durability="strict") -> None:
    missing = []
    cursor = path
    while not cursor.exists():
        missing.append(cursor)
        cursor = cursor.parent
    for directory in reversed(missing):
        directory.mkdir(exist_ok=True)
        if durability == "strict":
            sync_directory(directory)
            sync_directory(directory.parent)


def durable_publish(temporary: Path, final: Path, *, replace: bool = False) -> None:
    if replace:
        os.replace(temporary, final)
    else:
        try:
            os.link(temporary, final)
        except FileExistsError:
            if final.is_symlink() or sha256_file(final) != sha256_file(temporary):
                raise ValueError(f"immutable publication conflict: {final.name}")
        temporary.unlink()
    sync_directory(final.parent)


def atomic_json(path: Path, value: dict, *, replace: bool = False) -> None:
    durable_mkdir(path.parent)
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with temporary.open("xb") as stream:
            stream.write(canonical_json(value))
            stream.flush()
            os.fsync(stream.fileno())
        durable_publish(temporary, path, replace=replace)
    finally:
        temporary.unlink(missing_ok=True)


_log = logging.getLogger("cedegrid")


@dataclass(frozen=True)
class ResourceTargets:
    """A "use less" target from the supervisor. Unset amounts carry no target."""
    sequence: int
    deadline_unix_ms: int
    step: str
    cpu_millicores: int | None = None
    ram_mib: int | None = None
    gpu_memory_mib: dict = field(default_factory=dict)


def _read_targets(path: Path) -> ResourceTargets | None:
    """Parse a target file; a missing, invalid or newer-version file is no target."""
    from .codec import parse_json
    try:
        value = parse_json(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return None
    if not isinstance(value, dict) or type(value.get("version")) is not int or value["version"] != 1:
        return None
    sequence, deadline, step, targets = (value.get(k) for k in ("sequence", "deadline_unix_ms", "step", "targets"))
    def amount(v): return type(v) is int and v >= 0
    if not (amount(sequence) and amount(deadline) and type(step) is str and isinstance(targets, dict)):
        return None
    cpu, ram, gpu = targets.get("cpu_millicores"), targets.get("ram_mib"), targets.get("gpu_memory_mib", {})
    if not ((cpu is None or amount(cpu)) and (ram is None or amount(ram)) and isinstance(gpu, dict)
            and all(type(k) is str and amount(v) for k, v in gpu.items())):
        return None
    return ResourceTargets(sequence, deadline, step, cpu, ram, dict(gpu))


class ResourceSignals:
    """Drain and resource-target signals from CEDEGRID_DRAIN_FILE and
    CEDEGRID_RESOURCE_TARGET_FILE alone, so managed children can use them."""
    def __init__(self, drain_file=None, target_file=None):
        self.drain_file = Path(drain_file) if drain_file else None
        self.target_file = Path(target_file) if target_file else None
        self._hooks, self._notified, self._acked = [], None, None

    @classmethod
    def from_env(cls):
        return cls(os.environ.get("CEDEGRID_DRAIN_FILE") or None,
                   os.environ.get("CEDEGRID_RESOURCE_TARGET_FILE") or None)

    def resource_targets(self) -> ResourceTargets | None:
        targets = _read_targets(self.target_file) if self.target_file else None
        self._acknowledge(targets.sequence if targets else 0)
        return targets

    def on_resource_target(self, callback):
        """Call `callback(targets)` now with the current target, then once per new one."""
        if not callable(callback):
            raise ValueError("resource-target callback must be callable")
        self._hooks.append(callback)
        targets = self.resource_targets()
        if targets is None:
            return
        if self._notified is None or targets.sequence > self._notified:
            self.poll()
        else:
            self._call(callback, targets)

    def poll(self) -> ResourceTargets | None:
        """Read the targets and call each hook once per new sequence."""
        targets = self.resource_targets()
        if targets is not None and (self._notified is None or targets.sequence > self._notified):
            self._notified = targets.sequence
            for callback in list(self._hooks):
                self._call(callback, targets)
        return targets

    def draining(self) -> bool:
        self.poll()
        return self.drain_file is not None and self.drain_file.exists()

    @staticmethod
    def _call(callback, targets):
        """A failing hook is logged and never stops the poll."""
        try:
            callback(targets)
        except Exception:
            _log.warning("resource-target hook failed", exc_info=True)

    def _acknowledge(self, sequence):
        """Record the highest sequence seen; it shows the worker reads targets."""
        if self.target_file is None or (self._acked is not None and sequence <= self._acked):
            return
        ack = self.target_file.parent / "resource-target.ack"
        temporary = ack.with_name(f".resource-target.ack.{os.getpid()}")
        try:
            temporary.write_text(f"{sequence}\n", encoding="utf-8")
            os.replace(temporary, ack)
        except OSError:
            _log.warning("resource-target ack could not be written; hints stay unavailable", exc_info=True)
            return
        self._acked = sequence


class WorkerContext:
    """In-process client of the native supervisor; no authoritative spool writes."""
    def __init__(self, context: dict, drain_file: Path | None = None, *, client=None):
        from .process import _SupervisorClient
        from .codec import integer, I64_MAX
        from .errors import ValidationError
        if context.get("schema_version") != 2:
            raise ValidationError("worker context version 2 is required")
        self.context = dict(context)
        self.context["generation"] = integer(context.get("generation"), "generation", 1, I64_MAX)
        for key in ("namespace_id", "session_id", "task_id", "assignment_id"):
            if type(context.get(key)) is not str or not context[key]:
                raise ValidationError(f"worker context missing {key}")
        self._client = client or _SupervisorClient.from_env(context)
        self.output = Path(context["output_dir"]) if context.get("output_dir") else None
        self.drain_file = drain_file or (Path(context["drain_file"]) if context.get("drain_file") else None)
        self._drain_hooks, self._drain_notified = [], False
        self._artifacts = {}
        self._signals = ResourceSignals(self.drain_file, context.get("resource_target_file")
                                        or os.environ.get("CEDEGRID_RESOURCE_TARGET_FILE"))

    @classmethod
    def from_env(cls):
        from .codec import parse_json
        from .errors import ValidationError
        raw = os.environ.get("CEDEGRID_CONTEXT")
        if not raw:
            raise ValidationError("CEDEGRID_CONTEXT is required")
        return cls(parse_json(Path(raw).read_bytes()), Path(os.environ["CEDEGRID_DRAIN_FILE"]) if os.environ.get("CEDEGRID_DRAIN_FILE") else None)

    @property
    def identity(self):
        return {key: self.context[key] for key in ("namespace_id", "session_id", "task_id", "assignment_id", "generation")}

    @property
    def resume(self):
        return self.context.get("resume")

    @property
    def inputs(self):
        return list(self.context.get("inputs", []))

    def on_drain(self, callback):
        if not callable(callback):
            raise ValueError("drain callback must be callable")
        self._drain_hooks.append(callback)

    def resource_targets(self) -> ResourceTargets | None:
        return self._signals.resource_targets()

    def on_resource_target(self, callback):
        """Call `callback(targets)` now with the current target, then once per new
        target, during drain polls."""
        self._signals.on_resource_target(callback)

    def draining(self):
        self._signals.poll()
        requested = self.drain_file is not None and self.drain_file.exists()
        if requested and not self._drain_notified:
            self._drain_notified = True
            for callback in self._drain_hooks: callback(self)
        return requested

    def safe_point(self):
        if self.draining(): raise DrainRequested("supervisor requested cooperative drain")

    def artifact(self, name, path, *, request_id=None):
        from .errors import ValidationError, RemoteError
        if type(name) is not str or not name or len(name.encode()) > 128 or name in (".", "..") or any(c in name for c in ("/", "\\", "\x00")):
            raise ValidationError("artifact name must be one path component")
        path = Path(path)
        with path.open("rb") as source:
            info = os.fstat(source.fileno())
            if not stat.S_ISREG(info.st_mode): raise ValidationError("artifact source must be a regular file")
            begin = self._client.request("artifact_begin", name=name, size=info.st_size, request_id=request_id or str(uuid.uuid4()))
            upload_id, offset = begin["upload_id"], begin["offset"]
            if not 0 <= offset <= info.st_size: raise RemoteError("invalid artifact offset")
            digest = hashlib.sha256()
            position = 0
            while True:
                chunk = source.read(256 * 1024)
                if not chunk: break
                digest.update(chunk)
                # Replayed begins may acknowledge a durable prefix.
                if position + len(chunk) > offset:
                    tail = chunk[max(0, offset - position):]
                    reply = self._client.request("artifact_chunk", upload_id=upload_id, offset=offset, data_hex=tail.hex())
                    offset += len(tail)
                    if reply.get("offset") != offset: raise RemoteError("artifact offset mismatch")
                position += len(chunk)
            sealed = self._client.request("artifact_finish", upload_id=upload_id)
        if position != info.st_size or sealed.get("size") != info.st_size or sealed.get("sha256") != digest.hexdigest():
            raise RemoteError("artifact integrity failure")
        artifact = {"name": name, **{key: sealed[key] for key in ("artifact_id", "sha256", "size")}}
        self._artifacts[artifact["artifact_id"]] = dict(artifact)
        return artifact

    def _publish(self, kind, metadata, artifacts, publication_id):
        from .codec import stringify_json
        from .errors import PublicationUncertain, RemoteError, ValidationError
        from .process import _InvalidReply
        artifacts = artifacts or []
        if len({item["name"] for item in artifacts}) != len(artifacts): raise ValidationError("artifact names must be unique")
        ids = []
        for item in artifacts:
            if self._artifacts.get(item.get("artifact_id")) != item: raise ValidationError("artifact is not sealed by this worker context")
            ids.append(item["artifact_id"])
        publication_id = publication_id or str(uuid.uuid4())
        fields = {"publication_id": publication_id, "kind": kind, "metadata": metadata, "artifact_ids": ids}
        digest = hashlib.sha256(stringify_json(fields, sort_keys=True).encode()).hexdigest()
        known_digest = None
        try:
            result = self._client.request("publication_commit", request_id=publication_id, **fields)
            known_digest = result.get("sha256")
            if result.get("publication_id") != publication_id or result.get("state") not in ("committed", "rejected"):
                raise _InvalidReply("publication durability acknowledgement missing")
            if result["state"] == "committed" and (result.get("assurance") not in ("durable_local", "replayable_local") or type(known_digest) is not str or len(known_digest) != 64):
                raise _InvalidReply("publication commit evidence missing")
        except RemoteError as error:
            if error.code != "ERR_CEDEGRID_PUBLICATION_UNCERTAIN": raise
            raise PublicationUncertain(publication_id, self.identity, digest=getattr(error, "details", {}).get("sha256"), request_digest=digest, cause=error) from error
        except (OSError, _InvalidReply) as error:
            raise PublicationUncertain(publication_id, self.identity, digest=known_digest, request_digest=digest, cause=error) from error
        if result["state"] == "rejected": raise RemoteError(result.get("message", "publication rejected"))
        return result

    def checkpoint(self, metadata, artifacts=None, *, publication_id=None):
        return self._publish("checkpoint", metadata, artifacts, publication_id)

    def complete(self, metadata, artifacts=None, *, publication_id=None):
        return self._publish("result", metadata, artifacts, publication_id)

    def publication_status(self, publication_id):
        return self._client.request("publication_status", publication_id=publication_id)

    def publication_abort(self, publication_id):
        return self._client.request("publication_abort", publication_id=publication_id)

    def spawn_managed(self, argv, **kwargs):
        from .process import spawn_managed
        self.safe_point()
        return spawn_managed(argv, **kwargs)
