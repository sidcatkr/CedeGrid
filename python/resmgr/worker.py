"""Credential-free worker spool. A healthy agent publishes its durable records."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import stat
import uuid


class DrainRequested(Exception):
    """Raised only at a workload-declared safe cancellation boundary."""


def canonical_json(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sync_directory(path: Path) -> None:
    # Directory fsync is required on the supported POSIX state backends. An OS
    # that cannot perform it must report the error instead of promising durability.
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def durable_mkdir(path: Path) -> None:
    missing = []
    cursor = path
    while not cursor.exists():
        missing.append(cursor)
        cursor = cursor.parent
    for directory in reversed(missing):
        directory.mkdir(exist_ok=True)
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


class WorkerContext:
    def __init__(self, context: dict, drain_file: Path | None = None):
        if context.get("api_version", 1) != 1:
            raise ValueError("unsupported worker context version")
        for key in ("task_id", "assignment_id", "generation", "output_dir"):
            if key not in context:
                raise ValueError(f"worker context missing {key}")
        if not context["task_id"] or not context["assignment_id"] or int(context["generation"]) < 1:
            raise ValueError("invalid worker identity")
        self.context = context
        self.output = Path(context["output_dir"]).resolve()
        durable_mkdir(self.output)
        self.drain_file = drain_file
        self._drain_hooks = []
        self._drain_notified = False
        self._checkpoint_sequence = 0
        previous = self.output / "checkpoint.json"
        if previous.is_file():
            record = json.loads(previous.read_text())
            if any(record.get(key) != value for key, value in self.identity.items()):
                raise ValueError("worker output directory belongs to another attempt")
            self._checkpoint_sequence = int(record.get("checkpoint_sequence", 0))

    @classmethod
    def from_env(cls) -> "WorkerContext":
        raw = os.environ.get("RESMGR_CONTEXT")
        if raw:
            context = json.loads(Path(raw).read_text())
        else:
            context = {"api_version": 1, "task_id": os.environ["RESMGR_TASK_ID"],
                "assignment_id": os.environ["RESMGR_ASSIGNMENT_ID"],
                "generation": int(os.environ["RESMGR_ATTEMPT_GENERATION"]),
                "output_dir": os.environ["RESMGR_OUTPUT_DIR"]}
        return cls(context, Path(os.environ["RESMGR_DRAIN_FILE"]) if os.environ.get("RESMGR_DRAIN_FILE") else None)

    @property
    def identity(self) -> dict:
        return {key: self.context[key] for key in ("task_id", "assignment_id", "generation")}

    @property
    def resume(self) -> dict | None:
        return self.context.get("resume")

    def on_drain(self, callback) -> None:
        self._drain_hooks.append(callback)

    def draining(self) -> bool:
        requested = self.drain_file is not None and self.drain_file.exists()
        if requested and not self._drain_notified:
            self._drain_notified = True
            for callback in self._drain_hooks:
                callback(self)
        return requested

    def safe_point(self) -> None:
        if self.draining():
            raise DrainRequested("supervisor requested cooperative drain")

    def artifact(self, name: str, path: Path) -> dict:
        if not name or name in {".", ".."} or "/" in name or "\\" in name:
            raise ValueError("artifact name must be one path component")
        path = path.resolve()
        if not stat.S_ISREG(path.stat().st_mode):
            raise ValueError("artifact source must be a regular file")
        blobs = self.output / "blobs"
        blobs.mkdir(exist_ok=True)
        sync_directory(self.output)
        used = sum(item.stat().st_size for item in self.output.rglob("*") if item.is_file())
        temporary = blobs / ("." + uuid.uuid4().hex + ".tmp")
        digest = hashlib.sha256()
        size = 0
        try:
            with path.open("rb") as source, temporary.open("xb") as target:
                for block in iter(lambda: source.read(1024 * 1024), b""):
                    size += len(block)
                    if size > int(self.context.get("max_artifact_bytes", 256 * 1024 * 1024)):
                        raise ValueError("artifact exceeds worker publication limit")
                    if used + size > int(self.context.get("max_spool_bytes", 2 * 1024**3)):
                        raise ValueError("worker spool budget exceeded")
                    digest.update(block)
                    target.write(block)
                target.flush()
                os.fsync(target.fileno())
            sha = digest.hexdigest()
            final = blobs / sha
            durable_publish(temporary, final)
        finally:
            temporary.unlink(missing_ok=True)
        return {"name": name, "path": final.relative_to(self.output).as_posix(), "sha256": sha, "size": size}

    def _publish(self, kind: str, metadata: dict, artifacts: list[dict]) -> dict:
        if len({item["name"] for item in artifacts}) != len(artifacts):
            raise ValueError("artifact names must be unique")
        for item in artifacts:
            path = (self.output / item["path"]).resolve()
            if not path.is_relative_to(self.output) or not path.is_file() or sha256_file(path) != item["sha256"] or path.stat().st_size != item["size"]:
                raise ValueError("artifact identity or integrity mismatch")
        record = {"schema_version": 1, "kind": kind, **self.identity, "artifacts": artifacts, "metadata": metadata}
        if kind == "checkpoint":
            record["checkpoint_sequence"] = self._checkpoint_sequence + 1
        record["result_hash"] = hashlib.sha256(canonical_json(record)).hexdigest()
        atomic_json(self.output / f"{kind}.json", record, replace=kind == "checkpoint")
        if kind == "checkpoint":
            self._checkpoint_sequence = record["checkpoint_sequence"]
        return record

    def checkpoint(self, metadata: dict, artifacts: list[dict] | None = None) -> dict:
        return self._publish("checkpoint", metadata, artifacts or [])

    def complete(self, metadata: dict, artifacts: list[dict] | None = None) -> dict:
        return self._publish("result", metadata, artifacts or [])

    @property
    def inputs(self):
        return list(self.context.get("inputs", []))

    def spawn_managed(self, argv, **kwargs):
        from .process import spawn_managed
        self.safe_point()
        return spawn_managed(argv, **kwargs)
