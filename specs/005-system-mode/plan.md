# Implementation Plan: System Mode

**Branch**: planned on `specs-057-059`; implementation on `005-system-mode` once D4 is approved |
**Date**: 2026-11-14 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `specs/005-system-mode/spec.md`

## Summary

An administrator installs `cedegrid-system`. It adds one root broker behind
`/run/cedegrid/broker.sock` (root:cedegrid, 0660), two slices below the root, and the `cedegrid`
group. A group member's `cedegrid run` asks the broker for the member's units. The broker judges
the caller only by the socket's peer credentials, builds every unit property itself, and has
systemd start `cedegrid-u<uid>.service` and `cedegrid-idle-u<uid>.service` with `User=<uid>` and
`Delegate=yes`. Inside them, the present agent runs the delegated Full path with the user's rights.

Root does three things only:
- it starts and stops the units it made;
- it moves a caller's frozen, verified job between that caller's two units;
- it writes OOM scores for processes inside those units, from memory counters it reads itself.

Policy, measurement and running work stay the present code. Without the package, nothing changes.

Research ([research.md](research.md)) settled the open points as far as the documentation
allows. Three need a host run (R2, R3, R10), two need the user (N4 placement, N5 Principle II's
OOM clause), and decisions N1 to N3 from the clarify session stand.

## Technical Context

**Language/Version**: Rust, edition 2024, MSRV 1.88, in the one `cedegrid` crate.

**Primary Dependencies**: none new. The broker calls `systemd-run` and `systemctl` at fixed paths
(R8); `libc` covers peer credentials, pidfds and `/proc` directory handles.

**Storage**: none for the broker. It rebuilds its state from systemd's units and their marker at
start (stateless across restarts); the agent keeps 001's SQLite ledger inside the unit.

**Testing**: `cargo test` (unit tests for the request parser, the property builder, the policy
loader and the score function); opt-in container fixtures behind
`CEDEGRID_TEST_SYSTEM_CONTAINER`, run in a CI job on `ubuntu-22.04` with systemd as PID 1 (R17);
host runs on the test host by the other session (research, "Runs").

**Target Platform**: Linux, cgroup v2 unified, systemd as PID 1; glibc 2.35 and kernel 5.15
floor (both have what System mode needs: `cpu.idle` since 5.15). macOS and Windows: client paths
only, no System mode.

**Project Type**: CLI and daemon (one binary), plus Debian packages.

**Performance Goals**: another member answered within 1 s while one member floods (SC-003,
provisional); the OOM loop at one pass per second with no write when nothing changed (SC-012).

**Constraints**:
- Requests are 64 KiB at most.
- The broker uses no Tokio runtime and no threads beyond a bounded worker set: a small
  synchronous server is easier to review.
- No filesystem writes except scores, moves and, where needed, `cpu.idle` (FR-009).
- No user input reaches a command line root runs.

**Scale/Scope**: up to 50 group members per host (provisional; SC-012), two units each.

## Constitution Check

*GATE: checked before Phase 0 and again after Phase 1.*

| Principle or constraint | How this feature meets it | Status |
|---|---|---|
| I. External processes untouchable | The broker writes scores and makes moves only for processes inside cedegrid units it made, each held by a pidfd, and checks the caller's user ID on each (FR-023, FR-018, R9, R10). It signals, renices and throttles nothing | Pass |
| II. No harm in any mode | System mode adds no give-back (FR-036); its effects follow D1 and D2 in both cases. **Conflict found**: lowering an in-share OOM score contradicts Principle II's "MUST NOT be set below it" (R15). Raised as N5; the Principle V draft carries the proposed clause | Needs N5 |
| III. Unknown is never idle | Unreadable memory lowers no score, and lowered scores return to 0 after a window (FR-026, R10) | Pass |
| IV. Pure policy | The score function and the fair-share computation are pure functions with their own tests; the broker's loop only reads and writes. `policy.rs` is unchanged | Pass |
| V. Rootless | **Violated by design**: a root broker. Allowed only after the MAJOR amendment (D4, `drafts/amendment-principle-v.md`). Default install stays rootless (FR-001) | Needs D4 |
| VI. Tests first | Security suite, score tests and container fixtures first (tasks.md) | Pass |
| VII. Compatibility | No wire, SDK, worker-context or config change for existing paths. The broker protocol is new, local and versioned (FR-035). The administrator policy is a new file with its own `policy_version = 1` | Pass |
| VIII. Concise public docs | A System mode section in `docs/guarantees.md` and an administrator page, each claim one sentence, including the OOM limit (FR-027) and what `cpu.idle` does not cover (FR-021) | Pass |
| Launch barrier | Unchanged inside the unit: the agent's barrier runs as today | Pass |
| Hidden dispatch | The broker's subcommand joins the hidden dispatch that runs before argument parsing, threads and terminal handoff, ahead of the root refusal | Pass |
| Source export | Every new shipped file goes into `ALLOWLIST` and Cargo `include`; the packaging files too | Pass |
| Tools | Any packaging script in `tools/` uses the Python standard library only | Pass |

**Gate result**: planning may continue. Implementation may not start until the user approves D4,
and N5 with it. Both are listed in the report.

## Project Structure

### Documentation (this feature)

```text
specs/005-system-mode/
├── spec.md
├── plan.md                 # this file
├── research.md             # Phase 0
├── data-model.md           # Phase 1
├── quickstart.md           # Phase 1
├── contracts/
│   ├── broker-protocol.md  # requests, replies, limits, errors
│   ├── units-and-slices.md # what the broker builds and installs
│   └── admin-policy.md     # /etc/cedegrid/policy.toml
├── checklists/requirements.md
├── drafts/
│   ├── amendment-principle-v.md
│   └── 059-report.md
└── tasks.md                # /speckit-tasks
```

### Source Code (repository root)

```text
src/
├── broker.rs               # the broker process: listener loop, dispatch to the parts below
├── broker/
│   ├── peer.rs             # peer credentials, group check, refusals
│   ├── request.rs          # the closed request set, size and time limits, parsing
│   ├── units.rs            # unit property builder, systemd-run/systemctl calls, marker
│   ├── admin_policy.rs     # /etc/cedegrid/ loader and checks
│   ├── moves.rs            # freeze, verify, move, thaw (FR-018)
│   ├── oom.rs              # the OOM loop: counters, fair share, writes
│   └── score.rs            # pure score function (FR-024, FR-025, N1, D2)
├── system_mode.rs          # client side: detect the socket, ask for units, doctor line
├── main.rs                 # hidden dispatch for the broker and the idle executor
├── kernel.rs               # pidfd and /proc directory helpers
├── cgroup.rs               # delegated root detection inside a unit
└── explain.rs              # doctor and observe lines for System mode
packaging/deb/
├── cedegrid-broker.service, cedegrid-broker.socket
├── cedegrid.slice, cedegrid-idle.slice
├── cedegrid-system.sysusers   # the group
└── postinst, prerm, postrm
tests/
├── system_mode.rs          # unit and container fixtures (opt-in)
└── support/system_container.rs
docs/
├── guarantees.md           # System mode section
└── system-mode.md          # administrator page
.github/workflows/ci.yml    # the container job (R17)
```

**Structure Decision**: one crate, as today. The broker lives in `src/broker/`, declared from
`src/broker.rs` (CLAUDE.md: modules declared in their own file, no `#[path]`). `system_mode.rs` is
the unprivileged client. Packaging files sit in `packaging/deb/` and ship through the release
workflow.

## Design notes by area

**Broker loop (FR-002 to FR-011).**
- The socket comes from `cedegrid-broker.socket` with `SocketUser=root`, `SocketGroup=cedegrid`
  and `SocketMode=0660`. At start, the broker checks the received socket's owner and mode, and
  stops if they differ.
- Each connection: read peer credentials, check the group in the database, read at most 64 KiB
  within the read timeout, parse into the closed set, apply the policy, act, reply, log one
  journal line.
- Limits: a per-user token bucket for requests, at most 2 units per user, a bounded worker pool,
  and a timeout on each systemd call.

**Units (FR-006, FR-007, R8).**
- Transient units via `systemd-run --unit=cedegrid-u<uid> --slice=… --uid=<uid> --gid=<gid>
  --property=Delegate=yes --property=TasksMax=<policy> …` and the fixed executable path.
- The marker property is checked with `systemctl show` before every stop or move. The exact
  property set is in [contracts/units-and-slices.md](contracts/units-and-slices.md).
- Ordinary-unit placement follows N4 (research R2).

**Moves (FR-018, R9).** Freeze, verify by unit membership and user ID, pidfd, move into the
agent-made empty leaf in the other unit, thaw. A failure refuses and thaws the source.

**OOM loop (FR-022 to FR-027, R10).**
- Counters per host layout feed 001's fair share with the policy's thresholds.
- The pure score function takes the mode rule (D2) and N1's option as inputs.
- Writes go through a `/proc/<pid>` directory handle checked against a pidfd and the start time.
- The broker is the one writer in System mode; the agent's own OOM marks are off there.

**Client (FR-012 to FR-015).**
- `run` sees the socket, sends start, waits for the unit, and hands the job to the agent in it.
- Without an answer within 2 s (provisional), it falls back with one line, or refuses with
  `--require-system`.
- `doctor` prints the level and why, the effective policy, the `cpu.idle` path (R1), whether
  `systemd-oomd` reaches the slices (R11), and which limits apply (N3).

**Margin (FR-020, R3).** Yield First's CPU margin gets a System mode value, set from R3-run. It
stays at today's value until the run sets it.

## Complexity Tracking

| Violation | Why needed | Simpler alternative rejected because |
|---|---|---|
| Principle V: a root process | Gives Full to users on hosts without delegation, and places Yield First where `cpu.idle` works (at the level of other users' groups) | polkit cannot see unit properties, so a rule would let the group become root (R5). An administrator enabling per-user delegation and lingering changes host-wide login settings, which the user ruled out on the shared host, and still gives no root-level idle group |
| Principle II's OOM sentence (N5) | Protecting in-share own work in an OOM others caused, the user's principle of 2026-10-29 | Without lowering a score, in-share own work is protected only up to everyone's baseline (today's limit, Principle II) |
| A second CedeGrid process per user (the idle executor) | One agent per user (003 FR-002), while its idle work must be born inside another cgroup it cannot reach | Two agents per user break FR-002; moving every Yield First task in through root would make root act on every launch |

## Post-design Constitution Check

Rechecked after Phase 1 ([data-model.md](data-model.md), [contracts/](contracts/),
[quickstart.md](quickstart.md)). No new violation was found. The two flagged rows are unchanged:
V needs D4, and II needs N5.
