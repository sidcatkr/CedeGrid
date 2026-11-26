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
{"protocol": 1, "kind": "raise", "on": true, "level": "plain"}
```

- `protocol`: required, must equal the broker's.
- `kind`: required, one of `start`, `stop`, `status`, `move`, `raise` (006).
- `mode`: `start` and `move` only; one of 001's mode names, and allowed by the policy.
- `job`: `move` only; at most 128 bytes of `[A-Za-z0-9_-]`; resolved only inside the caller's
  own units.
- `on`: `raise` only, required: raise or end the raise of the caller's account slice weight.
- `level`: `raise` only, `plain` (priority over plain users, `modes.plain_priority` and its
  weight) or `unprotected` (`modes.unprotected` and its weight); absent means `unprotected`. One
  lease per caller, 30 s, at the level last asked.
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
| `refused_account` | user ID 0, below `UID_MIN`, or in `principals.system_accounts.uid_ranges` |
| `request_too_large`, `request_timeout`, `malformed` | framing |
| `unknown_field`, `identity_in_request`, `unknown_kind` | content |
| `mode_not_allowed`, `task_limit` | administrator policy |
| `policy_refused` | the policy file is not root's, is writable by others, or does not parse; the broker serves no request until it is fixed |
| `rate_limited` | the caller's request rate is over its bound |
| `unit_not_ours` | a unit with the caller's name exists without the broker's marker |
| `job_not_found`, `move_refused` | a move whose job is not in the caller's source unit, or whose members fail a check |
| `raise_refused` | the level's policy key is false, the caller's ordinary unit is not in its account slice, or the slice has a weight the broker did not set |
| `systemd_timeout`, `systemd_failed` | the systemd call did not finish or failed |

## Journal

One line per request: caller user ID, peer PID (and pidfd where present), kind, result code,
duration. One line per score written: PID, start time, unit, old and new value.
