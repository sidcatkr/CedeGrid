# Contract: the broker protocol

Local only, over `/run/cedegrid/broker.sock` (unix stream, root:cedegrid, 0660). It is not part
of the coordinator's v1 wire and is not exposed to the SDKs.

## Framing

- One request per connection: one JSON object terminated by a newline.
- The broker reads at most 65,536 bytes. A longer message, or one with no newline by then, is
  refused with `request_too_large`.
- Read timeout: 2 s from accept (*provisional*). On expiry the connection is closed with
  `request_timeout`.
- One reply: one JSON object terminated by a newline. Then the broker closes the connection.

## Request

```json
{"protocol": 1, "kind": "start", "mode": "neutral"}
{"protocol": 1, "kind": "stop"}
{"protocol": 1, "kind": "status"}
{"protocol": 1, "kind": "move", "job": "<job id>", "mode": "yield_first"}
```

- `protocol`: required, must equal the broker's.
- `kind`: required, one of `start`, `stop`, `status`, `move`.
- `mode`: `start` and `move` only; one of 001's mode names, and allowed by the policy.
- `job`: `move` only; at most 128 bytes of `[A-Za-z0-9_-]`; resolved only inside the caller's
  own units.
- Unknown fields are refused with `unknown_field`. A field named for an identity (`uid`, `user`,
  `unit`, `path`, `cgroup`, `exec`, `mount`, `device`, `capability`, or any other) is refused
  with `identity_in_request` and logged at warning level.

## Reply

```json
{"protocol": 1, "ok": true, "units": [{"kind": "ordinary", "name": "cedegrid-u1000.service", "state": "active", "cgroup": "/…"}]}
{"protocol": 1, "ok": false, "code": "not_a_member", "message": "<one sentence>"}
```

## Refusal codes

Each `message` is one sentence that says what to do.

| Code | When |
|---|---|
| `protocol_mismatch` | `protocol` differs; the message names which side to upgrade |
| `not_a_member` | the caller is not in the `cedegrid` group now |
| `refused_account` | user ID 0, or below `UID_MIN` |
| `request_too_large`, `request_timeout`, `malformed` | framing |
| `unknown_field`, `identity_in_request`, `unknown_kind` | content |
| `mode_not_allowed`, `task_limit` | administrator policy |
| `rate_limited` | the caller's request rate is over its bound |
| `unit_not_ours` | a unit with the caller's name exists without the broker's marker |
| `job_not_found`, `move_refused` | a move whose job is not in the caller's source unit, or whose members fail a check |
| `systemd_timeout`, `systemd_failed` | the systemd call did not finish or failed |

## Journal

One line per request: caller user ID, peer PID (and pidfd where present), kind, result code,
duration. One line per score written: PID, start time, unit, old and new value.
