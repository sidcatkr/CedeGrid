"""Supervisor-mediated child handles; never numeric-PID control."""
from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import time
import uuid


class SpawnUncertain(RuntimeError):
    def __init__(self, request_id):
        super().__init__(f"spawn acknowledgement missing; retry the same request_id={request_id}")
        self.request_id = request_id


class _InvalidReply(RuntimeError):
    pass


class _SupervisorClient:
    def __init__(self, endpoint, token, timeout=5):
        if not hasattr(socket, "AF_UNIX"):
            raise RuntimeError("supervisor child API requires Unix-domain sockets")
        self.endpoint, self.token, self.timeout = endpoint, token, timeout

    @classmethod
    def from_env(cls):
        endpoint, token = os.environ.get("RESMGR_SUPERVISOR_SOCKET"), os.environ.get("RESMGR_SUPERVISOR_TOKEN")
        if not endpoint or not token:
            raise RuntimeError("this assignment did not authorize supervisor-mediated children")
        return cls(endpoint, token)

    def request(self, op, **fields):
        body = json.dumps({"version": 1, "token": self.token, "op": op, **fields}, separators=(",", ":"), allow_nan=False).encode() + b"\n"
        if len(body) > 65536:
            raise ValueError("supervisor request exceeds 64 KiB")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(self.timeout)
            stream.connect(self.endpoint)
            stream.sendall(body)
            with stream.makefile("rb") as reader:
                response = reader.readline(65537)
        if not response.endswith(b"\n") or len(response) > 65536:
            raise _InvalidReply("invalid or oversized supervisor reply")
        try:
            result = json.loads(response)
        except (ValueError, TypeError) as error:
            raise _InvalidReply("invalid supervisor reply JSON") from error
        if not isinstance(result, dict) or not isinstance(result.get("ok"), bool):
            raise _InvalidReply("invalid supervisor reply structure")
        if not result.get("ok"):
            raise RuntimeError(result.get("error", "supervisor rejected request"))
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
            raise ValueError("wait intervals must be positive and timeout nonnegative")
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
    if not argv or not all(isinstance(value, str) for value in argv) or not no_escape or not single_process:
        raise ValueError("child argv and explicit single_process/no_escape acknowledgement are required")
    if cwd is not None and not Path(cwd).is_absolute():
        raise ValueError("child cwd must be absolute")
    client = _SupervisorClient.from_env()
    request_id = request_id or str(uuid.uuid4())
    fields = {"request_id": request_id, "argv": list(argv), "no_escape": True, "single_process": True}
    if cwd is not None:
        fields["cwd"] = str(cwd)
    if env is not None:
        fields["env"] = dict(env)
    try:
        result = client.request("spawn", **fields)
    except (OSError, _InvalidReply) as error:
        raise SpawnUncertain(request_id) from error
    return ManagedChild(client, result["child_id"])
