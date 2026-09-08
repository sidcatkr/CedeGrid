"""Supervisor-mediated child handles; never numeric-PID control."""
from __future__ import annotations

import math
import json
import os
from pathlib import Path
import re
import socket
import time
import uuid


def _text(value, name, *, empty=False):
    if not isinstance(value, str) or "\x00" in value or (not empty and not value):
        raise ValueError(f"{name} must be a string without NUL and with the required nonempty value")
    return value


def _identity(value, name):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", value) or value in {".", ".."}:
        raise ValueError(f"{name} must be a portable identifier of 1..128 ASCII characters")
    return value


def _uint(value, name, *, maximum=(1 << 64) - 1, minimum=0):
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"{name} must be an integer in {minimum}..{maximum}")
    return value


def _boolean(value, name):
    if type(value) is not bool:
        raise ValueError(f"{name} must be a boolean")
    return value


def _interval(value, name, *, zero=False):
    if type(value) not in (int, float):
        raise ValueError(f"{name} must be a finite number")
    try:
        valid = math.isfinite(value) and (value >= 0 if zero else value > 0)
    except OverflowError:
        valid = False
    if not valid:
        raise ValueError(f"{name} must be finite and {'nonnegative' if zero else 'positive'}")
    return value


def _argv(value):
    if not isinstance(value, (list, tuple)) or not value:
        raise ValueError("argv must be a nonempty list or tuple, not a string")
    return [_text(item, "argv entry", empty=index != 0) for index, item in enumerate(value)]


def _cwd(value):
    if not isinstance(value, (str, Path)):
        raise ValueError("cwd must be an absolute path")
    text = _text(str(value), "cwd")
    if not Path(text).is_absolute():
        raise ValueError("cwd must be an absolute path")
    return text


def _environment(value):
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise ValueError("environment must be a string-to-string mapping")
    result = {}
    for key, item in value.items():
        _text(key, "environment key")
        if "=" in key or key.startswith(("RESMGR_", "CEDEGRID_")):
            raise ValueError("environment keys must not contain '=' or override manager-owned variables")
        result[key] = _text(item, "environment value", empty=True)
    return result


def _spawn_fields(argv, *, cwd=None, env=None, no_escape=False, single_process=False, request_id=None):
    args = _argv(argv)
    _boolean(no_escape, "no_escape")
    _boolean(single_process, "single_process")
    if not no_escape or not single_process:
        raise ValueError("explicit single_process/no_escape acknowledgement is required")
    directory = None if cwd is None else _cwd(cwd)
    environment = None if env is None else _environment(env)
    if request_id is not None:
        _identity(request_id, "request_id")
    request_id = str(uuid.uuid4()) if request_id is None else request_id
    fields = {"request_id": request_id, "argv": args, "no_escape": True, "single_process": True}
    if directory is not None:
        fields["cwd"] = directory
    if environment is not None:
        fields["env"] = environment
    return fields


class SpawnUncertain(RuntimeError):
    def __init__(self, request_id):
        super().__init__(f"spawn acknowledgement missing; retry the same request_id={request_id}")
        self.request_id = request_id


class _InvalidReply(RuntimeError):
    pass


class _SupervisorClient:
    def __init__(self, endpoint, token, timeout=5):
        _text(endpoint, "supervisor endpoint")
        _text(token, "supervisor token")
        _interval(timeout, "supervisor timeout")
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
        # Keep RPC in this process because the supervisor verifies the socket peer.
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
        if not isinstance(result, dict) or type(result.get("ok")) is not bool:
            raise _InvalidReply("invalid supervisor reply structure")
        if not result["ok"]:
            raise RuntimeError(result.get("error", "supervisor rejected request"))
        return result


class ManagedChild:
    def __init__(self, client, child_id):
        _identity(child_id, "child_id")
        self._client, self.child_id = client, child_id

    def status(self):
        return self._client.request("status", child_id=self.child_id)

    def stop(self):
        """Request verified termination; wait separately for release evidence."""
        return self._client.request("stop", child_id=self.child_id)

    def wait(self, timeout=None, poll_interval=0.1):
        _interval(poll_interval, "poll_interval")
        if timeout is not None:
            _interval(timeout, "timeout", zero=True)
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            result = self.status()
            if not isinstance(result, dict) or result.get("state") not in {"reserved", "prepared", "authorized", "running", "draining", "needs_reconciliation", "released"}:
                raise _InvalidReply("invalid child status reply")
            if result["state"] == "released":
                return result
            if result["state"] == "needs_reconciliation":
                raise RuntimeError("child remains reserved and requires reconciliation")
            if deadline is not None and time.monotonic() >= deadline:
                raise TimeoutError("child wait expired; the child was not signaled")
            time.sleep(poll_interval if deadline is None else min(poll_interval, max(0, deadline - time.monotonic())))


def spawn_managed(argv, *, cwd=None, env=None, no_escape=False, single_process=False, request_id=None):
    """Spawn a single managed child; ambiguous acknowledgements retain request_id."""
    fields = _spawn_fields(argv, cwd=cwd, env=env, no_escape=no_escape,
                           single_process=single_process, request_id=request_id)
    client = _SupervisorClient.from_env()
    request_id = fields["request_id"]
    try:
        result = client.request("spawn", **fields)
        if not isinstance(result, dict) or result.get("ok") is not True:
            raise _InvalidReply("invalid spawn acknowledgement")
        try:
            child_id = _identity(result.get("child_id"), "child_id")
        except ValueError as error:
            raise _InvalidReply("spawn acknowledgement lacks a valid child identity") from error
        return ManagedChild(client, child_id)
    except (OSError, _InvalidReply) as error:
        raise SpawnUncertain(request_id) from error
