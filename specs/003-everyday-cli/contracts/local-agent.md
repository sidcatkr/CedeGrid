# Contract: the local agent socket

## Location and access
- Socket: `$XDG_RUNTIME_DIR/cedegrid/agent.sock`, else `$HOME/.local/state/cedegrid/agent.sock`.
- The directory has mode 0700 and is owned by the user. The agent refuses to listen otherwise,
  and checks the peer's UID (`SO_PEERCRED`) on every connection, refusing any other user.
- One agent per user per host: the agent holds the namespace lifetime lock for its life.

## Lifecycle
1. `run` connects. If nothing listens and the lock is free, it starts `cedegrid agent --local`
   in a new session and waits up to 3 s for the socket.
2. A cluster-mode agent serves the same socket. Local runs are its own work.
3. A local agent exits after `local.idle_exit_s` (default 600) with no runs. A cluster agent
   never idles out.

## Requests (one JSON object per line; one response per request)
| Request | Response |
|---|---|
| `{"op":"run","task":{…},"env":{…},"tty":bool}` | `{"run_id", "control_level", "control_reason"}` or an error. `env` is used at launch and never stored. |
| `{"op":"ps"}` | `{"runs":[…]}` |
| `{"op":"logs","run_id","follow":bool,"offset"}` | output chunks, then `{"end":true}` |
| `{"op":"stop","run_id","force":bool}` | `{"state"}` |
| `{"op":"top","follow":bool}` | host and run snapshots |
| `{"op":"explain","task":{…}}` | effective values with sources |

Requests for a run owned by another user cannot occur (the peer check refuses the connection),
and a run ID is resolved only within this agent's state.
