"""Supervisor-mediated child handles; never numeric-PID control."""
from __future__ import annotations

from .codec import parse_json, stringify_json, integer, I64_MAX
from .errors import SpawnUncertain, RemoteError, ValidationError, CedeGridError
from .transport import remaining
import os
from pathlib import Path
import socket
import time
import uuid


def _coerce_spawn_argv(argv):
    if isinstance(argv, (str, bytes, bytearray)) or type(argv) not in (list, tuple):
        raise ValidationError("spawn argv must be an explicit list or tuple of strings")
    argv = list(argv)
    if not argv:
        raise ValidationError("child argv is required")
    if any(type(value) is not str for value in argv):
        raise ValidationError("spawn argv entries must be strings")
    if any("\x00" in value for value in argv):
        raise ValidationError("spawn argv entries cannot include NUL bytes")
    if not argv[0]:
        raise ValidationError("child executable path cannot be empty")
    return argv


def _extract_spawn_child_id(result: dict) -> str:
    child_id = result["child_id"]
    if type(child_id) is not str:
        raise _InvalidReply("spawn reply missing or invalid child_id")
    if not child_id:
        raise _InvalidReply("spawn reply child_id is empty")
    return child_id


class _InvalidReply(CedeGridError):
    code = "ERR_CEDEGRID_PROTOCOL"
    pass


class _SupervisorClient:
    def __init__(self, endpoint, token, timeout=5, identity=None):
        if not hasattr(socket, "AF_UNIX"):
            raise RuntimeError("supervisor child API requires Unix-domain sockets")
        self.endpoint, self.token, self.timeout = endpoint, token, timeout
        self.identity = dict(identity or {})

    @classmethod
    def from_env(cls, context=None):
        endpoint, token = os.environ.get("CEDEGRID_SUPERVISOR_SOCKET"), os.environ.get("CEDEGRID_SUPERVISOR_TOKEN")
        if not endpoint or not token:
            raise RuntimeError("this assignment did not authorize supervisor-mediated children")
        if context is None:
            context_file = os.environ.get("CEDEGRID_CONTEXT")
            if context_file:
                context = parse_json(Path(context_file).read_bytes())
            else:
                generation = os.environ.get("CEDEGRID_ATTEMPT_GENERATION", "")
                if not generation.isascii() or not generation.isdigit():
                    raise ValidationError("native supervisor attempt generation is required")
                context = {"schema_version": 2, "generation": int(generation),
                           "namespace_id": os.environ.get("CEDEGRID_NAMESPACE_ID"),
                           "session_id": os.environ.get("CEDEGRID_SESSION_ID"),
                           "assignment_id": os.environ.get("CEDEGRID_ASSIGNMENT_ID")}
        if context.get("schema_version") != 2:
            raise ValidationError("worker context version 2 is required")
        identity = {key: context.get(key) for key in ("namespace_id", "session_id", "assignment_id")}
        if any(type(value) is not str or not value or "\x00" in value for value in identity.values()):
            raise ValidationError("complete native namespace/session/assignment identity is required")
        identity["generation"] = integer(context.get("generation"), "generation", 1, I64_MAX)
        return cls(endpoint, token, identity=identity)

    def request(self, op, **fields):
        if not self.identity:
            raise ValidationError("version 2 supervisor identity is required")
        request_id = fields.pop("request_id", None) or str(uuid.uuid4())
        body = stringify_json({"version": 2, "op": op, "request_id": request_id,
                               "token": self.token, **self.identity, **fields}).encode() + b"\n"
        limit = 3 * 1024 * 1024
        if len(body) > limit:
            raise ValidationError("supervisor request exceeds 3 MiB")
        deadline = time.monotonic() + self.timeout
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(remaining(deadline))
            stream.connect(self.endpoint)
            stream.settimeout(remaining(deadline))
            stream.sendall(body)
            response = bytearray()
            while not response.endswith(b"\n"):
                stream.settimeout(remaining(deadline))
                part = stream.recv(min(65536, limit + 1 - len(response)))
                if not part: break
                response.extend(part)
                if len(response) > limit: break
        if not response.endswith(b"\n") or len(response) > limit:
            raise _InvalidReply("invalid or oversized supervisor reply")
        try:
            result = parse_json(response)
        except (ValueError, TypeError) as error:
            raise _InvalidReply("invalid supervisor reply JSON", cause=error) from error
        if not isinstance(result, dict) or not isinstance(result.get("ok"), bool):
            raise _InvalidReply("invalid supervisor reply structure")
        if not result["ok"]:
            error = RemoteError(result.get("message", "supervisor rejected request"), code=result.get("code", "ERR_CEDEGRID_REMOTE"))
            error.details = result
            raise error
        return result


class ManagedChild:
    def __init__(self, client, child_id):
        self._client, self.child_id = client, child_id

    def status(self):
        return self._client.request("status", child_id=self.child_id)

    def stop(self):
        """Request verified termination; call wait() separately for release evidence."""
        return self._client.request("stop", child_id=self.child_id)

    def wait(self, timeout=None, poll_interval=0.1):
        if poll_interval <= 0 or (timeout is not None and timeout < 0):
            raise ValidationError("wait intervals must be positive and timeout nonnegative")
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            result = self.status()
            if result["state"] == "released":
                return result
            if result["state"] == "needs_reconciliation":
                raise RuntimeError("child remains reserved and requires reconciliation")
            if deadline is not None and time.monotonic() >= deadline:
                raise TimeoutError("child wait expired; the child was not signaled")
            time.sleep(poll_interval if deadline is None else min(poll_interval, max(0, deadline - time.monotonic())))


def spawn_managed(argv, *, cwd=None, env=None, no_escape=False, single_process=False, request_id=None):
    """Spawn only after explicit child contract acknowledgement.

    The root assignment must declare single_process=False and managed_child_limit
    in 1..8. Each child is a single process; arbitrary forks/nested spawning are
    outside this contract. A missing acknowledgement preserves the request ID.
    """
    if not no_escape or not single_process:
        raise ValidationError("child argv and explicit single_process/no_escape acknowledgement are required")
    if cwd is not None and not Path(cwd).is_absolute():
        raise ValidationError("child cwd must be absolute")
    argv = _coerce_spawn_argv(argv)
    client = _SupervisorClient.from_env()
    request_id = request_id or str(uuid.uuid4())
    fields = {"request_id": request_id, "argv": argv, "no_escape": True, "single_process": True}
    if cwd is not None:
        fields["cwd"] = str(cwd)
    if env is not None:
        fields["env"] = dict(env)
    try:
        result = client.request("spawn", **fields)
    except (OSError, _InvalidReply) as error:
        raise SpawnUncertain(request_id, cause=error) from error
    try:
        return ManagedChild(client, _extract_spawn_child_id(result))
    except (TypeError, KeyError, _InvalidReply) as error:
        raise SpawnUncertain(request_id, cause=error) from error
