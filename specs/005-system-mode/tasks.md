---
description: "Task list for feature 005, System mode"
---

# Tasks: System Mode

**Input**: Design documents from `specs/005-system-mode/` ([plan.md](plan.md), [spec.md](spec.md),
[research.md](research.md), [data-model.md](data-model.md), [contracts/](contracts/),
[quickstart.md](quickstart.md))

**Tests**: required. Principle VI asks for tests first, and the spec names the security suite
(FR-029) and the gates (SC-001 to SC-012). Each test task comes before the code it covers and must
fail first.

**Organization**: by user story, after a shared setup and foundation. Tasks that write the same
test file are not marked [P] against each other.

## Gate before any task

**Status, 2026-11-17**: the user decided D4 A, N5 A, N6 A, N1 B, N2 A, N3 (A by default; B and
C as the policy's `units.pam_session`), N4 (A by default as `units.ordinary_slice = "auto"`; B as
`"cedegrid"`; C deferred to the first release after 005, follow-up SYS-1) and D1 to D3 (057's
recommendations). The combined 8.0.0 amendment is applied (T069's constitution part).

No task below starts until the user has approved D4 and decided N5 and N6:

- **D4**: the Principle V amendment ([drafts/amendment-principle-v.md](drafts/amendment-principle-v.md));
- **N5**: Principle II's OOM clause in the same revision (research R15);
- **N6**: Principle I's clause on what the broker counts as verified own work, in the same
  revision (research R15).

The tasks are written for option A of N5 and N6. Under N5 B, T046's lowering is switched off (as
N1 C); under N6 B, T039 and T048 also require the agent's verified list, and FR-012 changes.

Tasks that depend on another open decision name it (N1 to N4, D1 to D3). Such a task is written
for the recommended option and has a test for each option, so the choice changes a parameter, not
the task list.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an unfinished task)
- **[Story]**: US1 to US5 from spec.md

---

## Phase 1: Setup

**Purpose**: module and packaging skeletons, export lists, CI job.

- [ ] T001 Declare `pub mod broker;` and `pub mod system_mode;` in `src/lib.rs`, and create `src/broker.rs` declaring `pub mod request; pub mod admin_policy; pub mod units; pub mod score; pub mod oom;` (reached from `tests/`) and `mod peer; mod moves;`, with empty files under `src/broker/` (no `#[path]`)
- [ ] T002 [P] Add every new shipped file (`src/broker.rs`, `src/broker/*.rs`, `src/system_mode.rs`, `packaging/deb/*`) to `ALLOWLIST` in `tools/pack_source_review.py` and to `include` in `Cargo.toml`
  - *In progress:* every file added so far is listed; the list grows with each task.
- [x] T003 [P] Create `packaging/deb/` with skeletons of `cedegrid-broker.socket`, `cedegrid-broker.service`, `cedegrid.slice`, `cedegrid-idle.slice`, `cedegrid-system.sysusers`, `postinst`, `prerm` and `postrm`, as listed in `contracts/units-and-slices.md`
- [ ] T004 [P] Add a CI job on `ubuntu-22.04` in `.github/workflows/ci.yml` that builds both `.deb` files and starts a container with systemd as PID 1, running only `tests/system_mode.rs` with `CEDEGRID_TEST_SYSTEM_CONTAINER=1` (never `--ignored` across the suite)
- [ ] T005 [P] Write the container harness in `tests/support/system_container.rs`: start and stop the container, run a command as a named test account, install and purge the packages, and take the inventory SC-009 compares (units, slices, groups, files under `/etc`, `/run`, `/usr`)

---

## Phase 2: Foundational

**Purpose**: the pure parts every story uses. They are tested first.

### Tests first

- [x] T006 [P] Request parser tests in `tests/system_mode.rs`, for `contracts/broker-protocol.md`:
  - one JSON object per connection, "at most 65,536 bytes", so 65,537 bytes or no newline gives `request_too_large`;
  - `protocol` differing gives `protocol_mismatch`;
  - `kind` outside `start`, `stop`, `status`, `move` gives `unknown_kind`;
  - an unknown field gives `unknown_field`;
  - each identity name (`uid`, `user`, `unit`, `path`, `cgroup`, `exec`, `mount`, `device`, `capability`) gives `identity_in_request`;
  - `job` must be "at most 128 bytes of `[A-Za-z0-9_-]`";
  - `mode` is accepted only on `start` and `move`.
- [x] T007 Administrator policy tests in `tests/system_mode.rs`, for `contracts/admin-policy.md`:
  - a file or directory not owned by root, or writable by group or others, is refused;
  - an unknown key is refused (`deny_unknown_fields`);
  - the defaults are `policy_version = 1`, `tasks_per_user = 64`, `tasks_max = 4096`, `uid_min` from `UID_MIN` (else 1000) and `principal_min_percent = 1`;
  - Aggressive is not allowed unless listed.
- [x] T008 Unit property builder tests in `tests/system_mode.rs`, for `contracts/units-and-slices.md`:
  - the output depends only on the user ID, the primary group and the policy;
  - the names are `cedegrid-u<uid>.service` and `cedegrid-idle-u<uid>.service`, with `Delegate=yes`;
  - there is no `OOMScoreAdjust=`, `ManagedOOMSwap=` or `ManagedOOMMemoryPressure=`;
  - the command is the fixed installed path with the fixed subcommand and no caller argument;
  - the ordinary unit's slice is parameterized by N4 (`user-<uid>.slice` or `cedegrid.slice`);
  - `PAMName=` and the process limits are parameterized by N3.
- [ ] T009 [P] Tests for the pidfd and `/proc` directory helpers in `tests/kernel.rs`: a write through a directory handle whose start time no longer matches reaches nothing; an exited PID fails closed.
  - *Written, not yet run:* the tests are in `tests/system_mode.rs` (Linux only), not `tests/kernel.rs`, beside T014's new location.
- [x] T010 [P] CLI tests in `tests/cli.rs`: every command run with effective user ID 0 refuses in one line saying to run it as the user without `sudo` (003 FR-082, built here first); the broker's hidden subcommand is dispatched before argument parsing and is the one exception.
  - The refusal is `system_mode::root_refusal`, called first in `main()`; tested in `tests/system_mode.rs`, not `tests/cli.rs`.

### Implementation

- [x] T011 Implement the request parser and its limits in `src/broker/request.rs` (passes T006)
- [x] T012 [P] Implement the policy loader in `src/broker/admin_policy.rs` (passes T007)
- [x] T013 [P] Implement the pure property builder in `src/broker/units.rs`, taking N3 and N4 as inputs, with `RuntimeDirectory=cedegrid/u<uid>` (0700) on the ordinary unit and `Restart=on-failure` on both (passes T008)
- [ ] T014 [P] Implement the pidfd and `/proc/<pid>` directory helpers with the start-time check in `src/kernel.rs` (passes T009)
  - *Moved:* written as `src/broker/process_handle.rs`, because kernel code stays read-only (contributor notes) and this handle writes `oom_score_adj`. Its Linux tests have not run yet.
- [x] T015 Add the hidden dispatch for the broker and the idle executor subcommands, and the root refusal for every other command, in `src/main.rs`, before argument parsing, threads and terminal handoff (passes T010)
  - Done for the broker's dispatch and the root refusal; the idle executor's dispatch comes with T022.

**Checkpoint**: parsing, policy, properties and kernel helpers pass their tests; nothing runs as root yet.

---

## Phase 3: User Story 1 - A group member gets Full where no delegation exists (P1) 🎯 MVP

**Goal**: a member's work runs in their own unit at the Full level; without the package nothing changes.

**Independent Test**: quickstart §2, rows "install", "a member's run" and "without the package".

### Tests first

- [ ] T016 [P] [US1] Container fixture in `tests/system_mode.rs` (opt-in): after install, 20 of 20 member starts land in `cedegrid-u<uid>.service` with the member's IDs at the Full level, and `doctor` says System (SC-001); two starts at once start each unit once
- [ ] T017 [US1] Container fixture in `tests/system_mode.rs`: with the package removed, 20 of 20 starts behave as before the install (SC-001); a non-member is refused with `not_a_member` and falls back; with the broker stopped and the socket present, the client says so in one line and falls back, and `--require-system` refuses; after a broker restart with units running, the units and their work keep running and `status` lists them by marker
- [ ] T018 [P] [US1] Delegated-root detection tests in `tests/cgroup.rs`. The agent accepts its cgroup only when all of these hold:
  - the cgroup is its own unit's path (both N4 placements);
  - it is owned by its user ID;
  - the broker's marker reads back.
  Every other path is refused.

### Implementation

- [x] T019 [US1] Implement peer credentials in `src/broker/peer.rs`:
  - `SO_PEERCRED` only;
  - group membership from the group database at each request;
  - refuse user ID 0 and IDs below `UID_MIN`;
  - `SO_PEERPIDFD` where present (Linux 6.5), for the log only.
- [x] T020 [US1] Implement the listener in `src/broker.rs`:
  - take the socket from socket activation and stop if its owner, group and mode are not root, `cedegrid`, 0660;
  - a bounded worker pool, a 2 s read timeout, one reply per connection;
  - one journal line per request (FR-011).
  - The broker's entry (`broker::server::run`) checks root, socket activation and the socket's owner and mode; it has not run under systemd yet (container, T016).
- [x] T021 [US1] Implement start, stop and status in `src/broker/units.rs`:
  - `systemd-run` and `systemctl` at fixed paths, argument vectors only, a timeout on each call;
  - the marker is set at start and checked with `systemctl show` before every stop;
  - `unit_not_ours` when a unit with the caller's name has no marker;
  - under N4 C only, `cedegrid.slice`'s weight set from the count of busy accounts at each start and stop, read back.
  - Both units' markers are checked before either is touched. Tested against a systemd stand-in only; the container run is T031.
- [ ] T022 [US1] Implement the idle executor subcommand: a fixed CedeGrid subcommand with no policy of its own, started in the idle unit and driven by the user's agent over a socket in the ordinary unit's runtime directory `/run/cedegrid/u<uid>/`, never the user's own runtime directory (research R9, R18). In `src/agent/` and `src/main.rs`.
- [ ] T023 [US1] Implement delegated-root detection inside a unit in `src/cgroup.rs` (passes T018)
- [x] T024 [US1] Implement the client in `src/system_mode.rs`, with `cedegrid system start|stop|status` added to `src/main.rs`:
  - detect the socket and check the protocol;
  - start the units and wait;
  - fall back after 2 s (provisional) with one line, or refuse under `--require-system` (FR-013).
  - `src/system_mode.rs` has the request, the 2 s timeout and the fallback decision; `cedegrid system start|stop|status` is in `main.rs`. Not run against a real broker yet.
- [ ] T025 [US1] Add System mode lines to `doctor` in `src/explain.rs` (FR-014):
  - the level and why;
  - the effective policy;
  - the `cpu.idle` path (R1);
  - that `cpu.idle` gives way on CPU time only, not on SMT siblings, caches or memory bandwidth (FR-021);
  - whether `systemd-oomd` reaches the slices (R11);
  - which limits apply (N3), and which one a unit has reached;
  - logind's effective `RemoveIPC=`, and what it means at the user's last logout (R18).
- [ ] T026 [US1] Label System runs in `observe` and the status output in `src/explain.rs` (FR-014)

**Checkpoint**: US1 passes in the container. This is the MVP: Full for members, unchanged elsewhere.

---

## Phase 4: User Story 2 - The group cannot become root (P1)

**Goal**: every attempt to act as another user or as root fails, is logged, and changes nothing.

**Independent Test**: quickstart §2, row "the security suite" (FR-029, SC-002, SC-003).

### Tests first

- [ ] T027 [P] [US2] Container fixtures in `tests/system_mode.rs`: a request with each identity field; odd and overlong names; 65,536 and 65,537 bytes; a message that never ends; user ID 0; a non-member; a member inside a user namespace where it maps to user ID 0, which is served as its real user ID. Each is refused with its code, logged, and nothing changes (SC-002)
- [ ] T028 [US2] Container fixture in `tests/system_mode.rs`: one member asks to stop, start, status or move the second member's units by every request form; each is refused, and the second member's units are unchanged
- [ ] T029 [US2] Container fixture in `tests/system_mode.rs`: the socket reads root, group `cedegrid`, mode 0660; a non-member account cannot connect
- [ ] T030 [US2] Container fixture in `tests/system_mode.rs`: one member floods requests; another member is answered within 1 s (provisional, SC-003); the flooder gets `rate_limited`; with 50 member accounts, the broker's CPU stays below 0.1% of one CPU and its memory below 32 MiB (provisional, SC-012)
- [ ] T031 [US2] Container fixture in `tests/system_mode.rs`: every unit the broker starts has exactly the properties of `contracts/units-and-slices.md`, read back with `systemctl show`, whatever the request said

### Implementation

- [x] T032 [US2] Implement the per-user token bucket, the per-user unit count (2) and the bounded worker queue in `src/broker.rs` and `src/broker/peer.rs` (passes T030)
- [ ] T033 [US2] Confine the broker's unit in `packaging/deb/cedegrid-broker.service`:
  - set `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes` and `RestrictAddressFamilies=AF_UNIX`;
  - add the smallest `CapabilityBoundingSet=` that lets it write scores, make moves and write `cpu.idle`;
  - record each setting against systemd 249 in a comment.
- [x] T034 [US2] Log every refusal at warning level with the caller's user ID and code in `src/broker.rs` (FR-011)

**Checkpoint**: US1 and US2 pass. These files are the core of the security review package (T073).

---

## Phase 5: User Story 3 - Yield First gives way at once (P2)

**Goal**: Yield First work sits in the idle slice, gives way within milliseconds, and moves between the two units as a whole.

**Independent Test**: quickstart §2 rows "`cpu.idle`" and "a move"; host run T061.

### Tests first

- [ ] T035 [P] [US3] Container fixture in `tests/system_mode.rs`: `cedegrid-idle.slice` reads `cpu.idle` = 1 after install on systemd 249, and `doctor` names the path taken: `CPUWeight=idle`, or `cpu.idle` written by the broker (research R1)
- [ ] T036 [US3] Container fixture in `tests/system_mode.rs`: a move of a job between the caller's two units moves exactly that job's processes, holds it frozen so no process starts between the check and the move, and refuses a job outside the caller's source unit, a job with a member of another user ID, and a non-empty destination leaf (FR-018)
- [ ] T037 [P] [US3] Tests in `tests/configuration.rs` and `tests/policy.rs` (FR-020):
  - in System mode, CPU's default `yield_first_margin_percent` is the System mode default, equal to today's 15 until T061 sets it;
  - an explicit setting still wins;
  - the CPU range admits the value T061 measures (today it is 1 to 90, so 0% needs the range widened, in System mode only);
  - at every other level nothing changes.

### Implementation

- [ ] T038 [US3] Write `cedegrid-idle.slice` with `CPUWeight=idle` in `packaging/deb/cedegrid-idle.slice`, and implement the broker's fallback write of `cpu.idle` with readback at start in `src/broker/units.rs` (passes T035)
- [ ] T039 [US3] Implement the move in `src/broker/moves.rs`, as in research R9 (passes T036):
  - resolve the job's leaf inside the caller's source unit;
  - freeze it;
  - check each member's user and group IDs and that nothing else is there (the broker's verified set, N6), and open a pidfd on each;
  - move each member into the agent-made empty leaf in the other unit, and read back each member's cgroup;
  - thaw. Refuse and thaw the source on any failure; the job stays in its unit.
  - Under (c), agreed 2026-11-17: the agent names each task leaf `cedegrid-<mode>-<id>` (with T023), and the OOM pass reads each ordinary-unit task's mode from it.
- [ ] T040 [US3] Make the agent ask for a move when a running job's mode changes between Yield First and another, and make an empty destination leaf first; on a refusal, apply the new mode where the job is with 001's Full steps and say so in one line. In `src/agent/` (FR-017, FR-018)
- [ ] T041 [US3] Implement the level-dependent CPU default and range of `yield_first_margin_percent` in `src/contention.rs` (`ResourceThresholds::defaults`) and `src/config.rs` (where defaults are applied and validated), chosen once the agent knows it runs in a broker-made unit; no new key (Principle VII) (passes T037)

**Checkpoint**: US3 passes in the container; its margin waits on T061.

---

## Phase 6: User Story 4 - Own work is not the first to die for an OOM it did not cause (P2)

**Goal**: the broker's loop scores processes inside the units from counters it reads itself.

**Independent Test**: quickstart §2 row "scores"; host run T072.

### Tests first

- [x] T042 [P] [US4] Score function tests in `tests/system_mode.rs`, for `data-model.md`'s score record:
  - value `−round(1000 × in_share_pages ÷ total_pages)`, "clamped to −999", never −1000;
  - 1000 for a task wholly over the share in Yield First and Neutral, and in Performance only under D2 case B;
  - the in-share amount is handed out oldest task first;
  - no write when the value is unchanged;
  - one case per N1 option (A at all times, B only while another principal is over its share, C never).
- [x] T043 [US4] Fair-share tests in `tests/system_mode.rs`:
  - principals follow 001 FR-059 with the policy's `uid_min` and `principal_min_percent`;
  - user IDs below `uid_min` are background;
  - an unreadable counter makes the user's memory unknown: no score is lowered, and lowered scores return to 0 after 10 s (provisional, FR-026).
  - Principal windows and fair shares are in `src/broker/oom.rs` with their tests; 001's per-resource window settings are fixed values there for now.
- [ ] T044 [US4] Container fixture in `tests/system_mode.rs`:
  - a score written by the broker sets `oom_score_adj_min`, and the user's lower write fails (SC-008);
  - a process outside the units keeps its score;
  - a reused PID gets no write;
  - a child inherits its parent's score until the next pass;
  - after a broker restart, the loop resumes and writes nothing for an unchanged value.
- [ ] T045 [P] [US4] Test in `tests/agent.rs`: in System mode the agent writes no OOM score; outside it, 001's marks are unchanged.

### Implementation

- [x] T046 [US4] Implement the pure score function in `src/broker/score.rs`, taking the mode's floor rule (D2) and N1's option as inputs (passes T042)
  - The share is handed out over the units' tasks only; the user's processes outside the units do not use it up. T047 may revisit this with the measured counters.
- [ ] T047 [US4] Implement memory reading in `src/broker/oom.rs` (passes T043):
  - per-account cgroup counters where present, else per-process sums by user ID from `/proc`;
  - the measure 001 uses for RAM;
  - 001's max-min fair share with the policy's thresholds.
  - *In part:* the pure steps (memory by user, principals, fair shares, tasks by leaf, writes per user) are in `src/broker/oom.rs`; the `/proc` and cgroup reading is not written yet. Per-account counters are follow-up SYS-2.
- [ ] T048 [US4] Implement the loop in `src/broker/oom.rs` (passes T044):
  - one pass per second;
  - only processes in the broker's verified set (N6), each checked by unit cgroup, user and group IDs, pidfd and start time;
  - writes through the `/proc/<pid>` directory handle, only on change, each read back;
  - one journal line per write.
- [ ] T049 [US4] Switch off the agent's own OOM score writes in System mode in `src/agent/oom.rs` and the supervisor's mark path in `src/supervision/unix.rs` (passes T045)

**Checkpoint**: US4 passes in the container; SC-006 and SC-007 wait on T072.

---

## Phase 7: User Story 5 - An administrator installs and removes it cleanly (P3)

**Goal**: two packages; install adds only what the contract lists; purge restores the host.

**Independent Test**: quickstart §2 rows "install … inventory" and "purge".

### Tests first

- [ ] T050 [P] [US5] Container fixture in `tests/system_mode.rs`: the inventory after install differs from before only by `contracts/units-and-slices.md`'s list; after runs and purge it equals the inventory before install (SC-009)
- [ ] T051 [US5] Container fixture in `tests/system_mode.rs`: a policy allowing only Yield First and Neutral refuses Performance with `mode_not_allowed`, naming the key; the agent refuses a job in a mode the policy does not allow and a job past `tasks_per_user`, each in one line naming the key; `doctor` shows the effective policy and where each value came from (FR-010)
- [ ] T052 [US5] Container fixture in `tests/system_mode.rs`: a broker and a client of different protocol versions refuse with `protocol_mismatch`, naming the side to upgrade (FR-035)

### Implementation

- [ ] T053 [US5] Write `postinst`, `prerm` and `postrm` in `packaging/deb/` (passes T050):
  - create the group through `cedegrid-system.sysusers`;
  - enable the socket;
  - on purge, stop and remove every unit with the broker's marker, then the slices, the socket, the service and the group.
  - Touch no PAM, login, SSH, other unit, sysctl or kernel module setting.
- [ ] T054 [US5] Make the agent apply the administrator policy to each job in System mode, in `src/agent/` and `src/config.rs`: read `/etc/cedegrid/policy.toml` with the broker's loader, refuse a disallowed mode and a job past `tasks_per_user` (passes T051)
- [ ] T055 [US5] Add the `.deb` build for `cedegrid` and `cedegrid-system` to the release workflow in `.github/workflows/release.yml`, with checksums in the release (FR-033)
- [ ] T056 [US5] Add the project's own apt repository publication and its signing to `.github/workflows/release.yml`, behind a separate approval in the workflow (FR-033)

**Checkpoint**: every story passes in the container.

---

## Phase 8: Host runs (the other session, after the user's root steps)

**The user's root steps on the test host** (research R13; the work session uses no `sudo`):

1. `sudo apt install ./cedegrid_<v>_amd64.deb ./cedegrid-system_<v>_amd64.deb`
2. `sudo usermod -aG cedegrid <you>` and `sudo usermod -aG cedegrid <second>`, then log in again
3. only if a run needs it: `sudo install -o root -g root -m 0644 <file> /etc/cedegrid/policy.toml`
4. after the runs: `sudo journalctl -u cedegrid-broker.service --since "<start time>"`
5. for the removed-member run: `sudo gpasswd -d <second> cedegrid`
6. at the end: `sudo apt purge cedegrid-system cedegrid`

Each run below records plain Linux too, at least 3 times each way, with the spread. No result is
estimated in advance. Results go to `docs/validation.md` with the commit.

- [ ] T057 Host run R2-run on the test host (N4, FR-019, SC-011), recorded in `docs/validation.md`:
  - F1 and F2 of item 057 in plain Linux and in System mode under N4 A and N4 B;
  - own CPU, and the second account's rate and p99;
  - whether the unit in `user-<uid>.slice` survives the last logout;
  - after that logout, whether the agent still reaches the idle executor, and whether a job using POSIX shared memory keeps it under the host's `RemoveIPC=` value (R18).
- [ ] T058 Host run R10-run on the test host (SC-006, SC-007, SC-008, N1 and D2 cases), recorded in `docs/validation.md`:
  - item 054's OOM shape with own work in share, then over share;
  - `systemd-oomd` left as it is;
  - a user write below a root-written score.
- [ ] T059 Host run R16-run on the test host (SC-012, EFF-6 to EFF-10), recorded in `docs/validation.md`: the broker's and both agents' CPU and memory, idle and busy. The 50-user bound runs in the container (T030's harness).
- [ ] T060 Host run for the removed-member case (N2) on the test host, after root step 5, recorded in `docs/validation.md`: the next request is refused, and the running units behave as N2's chosen option says
- [ ] T061 Host run R3-run on the test host (FR-020, SC-004, SC-005), recorded in `docs/validation.md`: Yield First own work against the second account's CPU-bound and cache-sensitive work, and the second account alone, with the margin stepped from today's value down to 0%. Then set the System mode CPU default to the measured value in `src/contention.rs`

---

## Phase 9: Polish and cross-cutting

- [ ] T062 [P] Write the System mode section in `docs/guarantees.md`, one sentence per claim. It covers:
  - what root does and does not do;
  - the OOM limit (FR-027);
  - what `cpu.idle` does not cover (FR-021);
  - login limits under N3;
  - that the allowed modes are CedeGrid's own rule, and the unit's properties are the limits the kernel enforces;
  - what logind's `RemoveIPC=` does at the user's last logout (R18);
  - the CPU split change on hosts without per-account slices (R2).
- [ ] T063 [P] Write the administrator page in `docs/system-mode.md`: install, policy, group, removal, and the exact root commands
  - *Drafted:* `docs/system-mode.md` describes the finished feature; recheck it against moves (T039), the OOM loop (T048), doctor (T025) and purge (T053) before release.
- [ ] T064 [P] Fix SUP-9 (a busy journal blocks the supervisor in an off phase) with a bounded lock wait and a test in `src/supervision/unix.rs` and `tests/supervision.rs`
- [ ] T065 [P] Fix AGT-15 (a failed Authorize or renewal send is never resent): resend until delivered, as AGT-16 was, with a test in `src/agent/service.rs` and `tests/agent.rs`
- [ ] T066 Change AGT-17's target from 005 to 002 in `specs/001-contention-modes/follow-ups.md`, with research R16's reason (replay is not used in System mode), and start `specs/005-system-mode/follow-ups.md` for this feature's own deferred items
- [ ] T067 After T059: if the agents' total CPU breaks 001 SC-014's 1% bound, fix the worst of EFF-6, EFF-7 and EFF-10 in `src/telemetry/managed.rs` and `src/agent/service.rs`, and record the result in `specs/005-system-mode/follow-ups.md`
- [ ] T068 Update `CHANGELOG.md` and `README.md`'s install section for the two packages
- [ ] T069 Apply the approved amendment (D4, N5, N6) to `.specify/memory/constitution.md` with its version change, in the same pull request as `docs/guarantees.md`'s System mode section (T062) and the tests it relies on (Governance)
- [ ] T070 Run `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked -- --test-threads=1` and `cargo +1.88.0 check --locked --all-targets` once, both Python suites and the npm suite (Principle VI), and the Windows client checks (`cargo check --all-targets`, `cargo test --lib --test policy --test telemetry --test cli`)
- [ ] T071 Run the source pack (`tools/pack_source_review.py`) and check that every new file is exported and no exported text names a host, an account or a private path
- [ ] T072 Rerun T058 at the release candidate's commit, and record SC-006 to SC-008 there in `docs/validation.md`
- [ ] T073 Prepare the pre-release security review package listing the files of research R14 at the release candidate's commit, in `specs/005-system-mode/security-review.md`. The review runs separately, before release.

---

## Dependencies and execution order

- **Gate**: D4, N5 and N6 approved → Phase 1.
- **Phase 1** → **Phase 2** → **US1** (MVP) → **US2**. US2's fixtures need US1's broker running.
- **US3**, **US4** and **US5** each need US1 and can then run in parallel with each other.
- **Host runs** (Phase 8, and T072) need a built package and the user's root steps 1 and 2.
- **Decisions**:
  - T013, T021 and T057 depend on N3 and N4.
  - T042, T046 and T058 depend on N1 and D2.
  - T039, T044 and T048 depend on N6.
  - T060 depends on N2.
  - T040 and caps inside a unit follow D1 and D3, with no task here; they are 001's and 057's.
- **001**: merges only after this feature is built and verified on the test host (user, 2026-11-14).

## Parallel examples

- Phase 2 tests: T006, T009 and T010 together (T007 and T008 follow T006 in the same file);
  then T012, T013 and T014 together.
- US1: T016 and T018 together; T017 follows T016 in the same file.
- US2: T027 to T031 share one file and are written in turn, beside T033 (packaging).
- After US1: US3 (T035 to T041), US4 (T042 to T049) and US5 (T050 to T056) by three people at once.

## Implementation strategy

1. **MVP**: Phases 1 to 3. A member gets Full through the broker, and nothing changes elsewhere.
   Stop and check in the container.
2. **US2** next, before anything is installed anywhere but the container: the security suite is
   the condition for a host install.
3. **US3, US4, US5**, then the host runs, then polish and the security review package.
4. Nothing is called done while a gate is unmeasured at the commit under test.

**Size**: 73 tasks: 35 code, packaging or CI; 24 tests written first (with the container
harness); 6 host runs for the other session (T057 to T061, T072); 4 documentation and records;
1 constitution change (T069); 3 checks and review preparation.
