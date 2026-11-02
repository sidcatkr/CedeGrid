# Follow-ups deferred by feature 001's reports

One line per deferred piece of work: its source (item and decision), what it is, its target
feature, and what it blocks. Collected in item 048 from reports 001–047 and the item 030
findings. Lines move to "Done" with a pointer once a later item does them.

## Open

| Source | What | Target | Blocks |
|---|---|---|---|
| 026 Decision 5 (A, the first step after the merge) | A refusal at the authorization barrier uses up one of the task's attempts. FR-018 should say such a refusal is not the task's failure, so a replay-safe task is requeued without using an attempt. | 001, after the merge | nothing; a task with `max_attempts = 1` ends in `needs_reconciliation` |
| 024 Decision 3 (A, after the merge, its own item) | SC-014 attribution: supervisor CPU attributed to its allocation, and the 173 m gap, which is not yet explained. Item 027 measures CedeGrid's CPU apart and dropped the read-pass term. | 001, after the merge | nothing |
| 024 Decision 5 (research after the merge) | Whether a listed GPU process with no samples and no memory change over the quiet window can count as quiet, without making unknown telemetry free. | new | nothing |
| 027 Decision 2 B (029 Part 0h-2) | Tighten the window's busy bound with per-CPU nanoseconds from `/proc/schedstat`, then rerun report 027's 4-CPU idle run. | 001, after the merge | nothing |
| 019 analyze C1 | Full moves cores too: since item 031 Full verifies its leaf members, so it can verify them per thread through `cgroup.procs` plus a pidfd and take the core move. 003's plan still says Full makes no per-thread call. | 003 | nothing |
| 019 analyze A1 | The numeric default of the wait level that marks a principal as contending. | 003 | T043, FR-056 |
| 015, 019 Part 2 | A pinned contender is not recorded as satisfied, and Tracked moves own cores off it. Tasked as 003 FR-056, T079–T085. | 003 | nothing |
| 029 Decision 3 B (report 029 run 4) | Keep own threads off a contended CPU before a duty cycle, and bound the step churn. Tasked as 003 FR-057, T099–T105, with SC-013's bound at 1.2× the p99 beside a fixed allocation. | 003 | nothing |
| 035 Part 3 | `cedegrid state purge` removes a state directory together with its control directory (003 tasks, "Backlog"). | 003 | nothing |
| 044 Q31 | `CLOCK_MONOTONIC` is per time namespace. The aligned GPU duty phases assume every supervisor shares the agent's namespace, which holds today; `node_clock` states it, and no spec does. | 001, after the merge | nothing |
| 046 runs Decision 1 A (047: C for 001) | Measure the duty cycle's switch cost and fold it into d0 if it is CedeGrid's. Tasked as 004 T049. | 004 | shipping Aggressive |
| 018, 022 | Build 004 after 001 merges, in the agreed order against 003, then run its native gate T048. | 004 | nothing |
| 010 item 6 and later reports | T117: validate the provisional thresholds, on the user's server, by the user. | 001 | closing 001's thresholds |
| 029 Part 0f | Score each System-mode task's OOM risk only by what exceeds its fair share. | 005 | nothing |
| 048 | A CI leg on the latest Ubuntu image next to `ubuntu-22.04`. | new | nothing |
| 048 | Remove the leftover test directories at the repository root once deletion is allowed: 2935 `..*.cedegrid-control` directories, none newer than 2026-11-01, and 7 `.tmp*` directories. `.tmp*/` is ignored since item 048. | new | nothing |
| 030 "Deferred low findings" | Item 030's 32 low findings, listed below. POL-7 belongs to 004. | 001, after the merge | nothing |

### Feature 005, System mode (not yet specified)

Decided so far:
- a root broker, like Docker's or Slurm's;
- a `cedegrid` group, with peer-UID checks on the socket;
- Full for every group member;
- Yield First in a `cpu.idle` cgroup;
- OOM scoring: the root loop writes `oom_score_adj` only in `cedegrid-*` units, scoring each task
  by its part over its fair share, with a floor of −999; the root-written value becomes
  `oom_score_adj_min`;
- two apt packages, `cedegrid` and `cedegrid-system`: a `.deb` in releases and its own apt
  repository first;
- a MAJOR amendment to constitution Principle V.

### Item 030's low findings

None was fixed by a later item. The reviewer's lows R5 and R6 were fixed in item 030, and R7 in
item 031.

| ID | Finding |
|---|---|
| SUP-8 | An adjust during an off phase cuts the phase short. Since item 044 a cycle with a GPU cap follows the node clock; CPU-only duty still restarts. |
| SUP-9 | A busy journal can block the supervisor for about 20 s in an off phase. |
| SUP-10 | A task launched at nice 10 in Yield First keeps it in Performance (by design, 029 Part 0b). |
| SUP-11 | A same-UID process that joins the leaf between the check and the write could be capped or frozen. |
| SUP-12 | A failed readback plus a failed restore leaves `memory.high` at the cap. |
| SUP-13 | A GPU cap without a target after a pause-to-cap reversal gets a fixed 500 duty. |
| SUP-14 | Errors removing the target, ack or drain files are discarded. |
| AGT-13 | A `drain_timeout_ms` longer than the lease expires the reservation mid-drain. |
| AGT-14 | A transient failure reading `execution-outcome.json` reports a success or yield as a failure. |
| AGT-15 | A failed Authorize or renewal send is ignored and never resent. |
| AGT-16 | A failed policy Drain or Terminate send is never retried. |
| AGT-17 | In replay mode, a revoked offer with no journal row stops the agent without `shutdown()`. |
| POL-6 | With some rise-rate settings, a more yielding mode's allowance can rise faster than Performance's. |
| POL-7 | Aggressive exists only in comments (004). |
| GPU-6 | The GPU margin switches on at once at the smallest external amount; recheck it against the time-slice rule of items 041–046. |
| TEL-6 | `freshness_intervals = 1`, or an interval under one tick, is accepted. |
| TEL-7 | The busy interval uses a timestamp taken before discovery. |
| TEL-8 | With `/boot/config-*` unreadable, the 10 ms tick fallback over-subtracts own CPU. |
| TEL-9 | Guest time is counted twice, and a failed `/proc` read gives a known empty list. |
| TEL-10 | PSI `avg10` keeps PSI-only probing on for about 8 s after pressure ends. |
| EFF-6 | About 7 `/proc` reads per task per interval. |
| EFF-7 | Static discovery is read again every interval. |
| EFF-8 | No prepared-statement cache, and the schema is queried on every call. |
| EFF-9 | Lookups are quadratic (about 117² at 117 tasks). |
| EFF-10 | Every heartbeat reopens the state store, rebuilds the backend and repeats the probes. |
| SDK-3 | EACCES or EIO on the drain path hides the drain (TypeScript) or raises (Python 3.10–3.12). |
| SDK-4 | An oversize request is reported as uncertain in TypeScript. |
| SDK-5 | `request_digest` key order differs between the SDKs. |
| SDK-6 | An explicit null amount makes TypeScript drop the whole file. |
| SDK-7 | Python's `ManagedChild.wait` raises builtin errors, not the `ERR_CEDEGRID_*` codes. |
| SDK-8 | A failed ack write is retried and logged on every read. |
| SDK-9 | The SDKs disagree on drain-path precedence for a hand-built context. |

## Done

| Source | Where |
|---|---|
| 024 Decision 2(c): new victims of a contended CPU excess share it by usage at one level | item 049 (T512), FR-016 |
| 027 Decision 5 B: charge running work at its mean while no other principal is active, at its p95 once one is | item 049 (T515), decided apart from any change to the modes |
| 045 note: the re-measure window's 3 s at full duty raised the held duty 0.015–0.04 above d0 | item 046: d0' = (d0 − w) ÷ (1 − w); the held duty fell to 0.754 against d0 0.75, and the rest is 004 T049 |
| 028: Full's GPU share, natively | item 047, Performance beside the saturating user (`docs/validation.md`, "Item 047") |
| 028 Decision 1: a leaf left frozen | item 030 (T286–T287) |
| 029 Decision 1: stop drains | item 030; item 048 Part B measures the stop again |
| 029 Decision 2, 030 Decision 3: the return after contention | item 034 (T412 closes T339) |
| 024 Decisions 1, 2(a), 2(b), 4; 025 Decision 1; 026 Decisions 1–3 | items 025–027 |
| 030 Decision 4 (TEL-3); 031 Decision 3 B; R1 multi-process | items 032, 033, 031 |
