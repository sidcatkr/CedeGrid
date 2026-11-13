# Follow-ups deferred by feature 001's reports

One line per deferred piece of work: its source (item and decision), what it is, its target
feature, and what it blocks. Collected in item 048 from reports 001–047 and the item 030
findings. Lines move to "Done" with a pointer once a later item does them.

## Open

| Source | What | Target | Blocks |
|---|---|---|---|
| 024 Decision 5 (research after the merge) | Whether a listed GPU process with no samples and no memory change over the quiet window can count as quiet, without making unknown telemetry free. | new | nothing |
| 019 analyze C1 | Full moves cores too: since item 031 Full verifies its leaf members, so it can verify them per thread through `cgroup.procs` plus a pidfd and take the core move. 003's plan still says Full makes no per-thread call. | 003 | nothing |
| 019 analyze A1 | The numeric default of the wait level that marks a principal as contending. | 003 | T043, FR-056 |
| 015, 019 Part 2 | A pinned contender is not recorded as satisfied, and Tracked moves own cores off it. Tasked as 003 FR-056, T079–T085. | 003 | nothing |
| 029 Decision 3 B (report 029 run 4) | Keep own threads off a contended CPU before a duty cycle, and bound the step churn. Tasked as 003 FR-057, T099–T105, with SC-013's bound at 1.2× the p99 beside a fixed allocation. | 003 | nothing |
| 035 Part 3 | `cedegrid state purge` removes a state directory together with its control directory (003 tasks, "Backlog"). | 003 | nothing |
| 046 runs Decision 1 A (047: C for 001) | Measure the duty cycle's switch cost and fold it into d0 if it is CedeGrid's. Tasked as 004 T049. | 004 | shipping Aggressive |
| 018, 022 | Build 004 after 001 merges, in the agreed order against 003, then run its native gate T048. | 004 | nothing |
| 010 item 6 and later reports | T117: validate the provisional thresholds, on the user's server, by the user. | 001 | closing 001's thresholds |
| 029 Part 0f | Score each System-mode task's OOM risk only by what exceeds its fair share. | 005 | nothing |
| 048 | A CI leg on the latest Ubuntu image next to `ubuntu-22.04`. | new | nothing |
| 048 | Remove the leftover test directories at the repository root once deletion is allowed: 2935 `..*.cedegrid-control` directories, none newer than 2026-11-01, and 7 `.tmp*` directories. `.tmp*/` is ignored since item 048. | new | nothing |
| 030 "Deferred low findings", item 050 | The 15 low findings item 050 moved to later features, POL-7, and SUP-11's remaining half, each listed below with its target. | 003, 004, 005, item 053 | nothing |

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

Item 050 went through the 31 low findings for 001 at `93fe984`. It fixed 12 and closed 3, which
are under "Done". The rest are here: none takes anything from another workload, and each cost
named falls on own work only. The reviewer's lows R5 and R6 were fixed in item 030, and R7 in
item 031.

| ID | Finding | Target | Why 001's guarantees hold without it |
|---|---|---|---|
| SUP-9 | A busy journal can block the supervisor for about 20 s in an off phase. | 005 | It needs a journal lock held for 5 s; own work stays stopped longer, and nothing else changes. |
| SUP-11 | A same-UID process that joins the leaf between the check and the write could be capped or frozen. | item 053 | T510 (item 049) covers the freeze; item 053 decides the caps. |
| SUP-13 | A pause relaxed to a cap after calm, on work that is no longer a victim, gets a cap with no target: half the request on CPU, a 500 duty on GPU compute. | 004 | No other principal contends then. It costs own work one scale-up interval, in Neutral and Yield First; Performance's pause returns to a cap with a target at its one-interval bound. |
| SUP-14 | Errors removing the target, ack or drain files, or their directory, are discarded. | 003 | Each attempt creates its own private directory, so a leftover file is never read as a drain. Listing leftovers belongs with `doctor`. |
| AGT-13 | A `drain_timeout_ms` longer than the lease expires the reservation mid-drain. | 003 | Not with the defaults. The reservation then ends uncertain and stays charged, the fail-safe outcome; a config check belongs with `doctor` and `validate`. |
| AGT-15 | A failed Authorize or renewal send is ignored and never resent. | 005 | The task stays unauthorized, so nothing runs; it ends at the prepare timeout and is requeued without an attempt (FR-018, item 049). |
| AGT-17 | In replay mode, a revoked offer with no journal row stops the agent without `shutdown()`. | 005 | The stop is deliberate: without a journal row, absence is not proof of release. The supervisors then follow the agent-loss rules and the reservations stay charged. |
| POL-6 | With `scale_up_step_interval_s` set, or `monitor.interval_ms` above 4 s, a more yielding mode's allowance can rise faster than Performance's. | 004 | The allowance never exceeds its own mode's target and is advisory, so mode order holds (FR-065) and only the pace of invited regrowth differs. Ordering the rates changes the documented step sizes, and 004 redefines the step for a 0% margin anyway. |
| POL-7 | Aggressive exists only in comments. | 004 | It is feature 004. |
| TEL-6 | `freshness_intervals = 1`, or an interval under one tick, is accepted. | 003 | Readings then go stale, which reads as unknown and holds own work back. A lower bound changes the config contract. |
| TEL-8 | With `/boot/config-*` unreadable, the 10 ms tick fallback over-subtracts own CPU. | 003 | The fallback assumes the slowest tick, so own CPU's lower bound is lower and own work holds back more. `/proc/config.gz` needs a gzip decoder, a new dependency; `doctor` can report the assumed tick. |
| TEL-10 | PSI `avg10` keeps PSI-only probing on for about 8 s after pressure ends. | 003 | It delays own work's return by seconds and gives others nothing less; 003 already specifies the PSI-only probe further. |
| EFF-6 | About 7 `/proc` reads per task per interval. | 005 | With these five, CedeGrid's own CPU measured 111 m at 128 tasks on 14 CPUs (item 049), inside SC-014's 1% bound of 140 m. |
| EFF-7 | Static discovery is read again every interval. | 005 | As EFF-6. |
| EFF-8 | No prepared-statement cache, and the schema is queried on every call. | 005 | As EFF-6. |
| EFF-9 | Lookups are quadratic (about 117² at 117 tasks). | 005 | As EFF-6. |
| EFF-10 | Every heartbeat reopens the state store, rebuilds the backend and repeats the probes. | 005 | As EFF-6. |
| 049 Decision 4 A (item 052) | A task whose launch is never authorized is requeued without an attempt, with no bound of its own: show the count of such requeues in status or cap them, and prune their coordinator rows. | 001 after the merge | nothing |
| 049 Decision 5 B and C (item 052) | Yield First pauses own work for the whole pause bound after one saturated interval. The user's decision of 2026-11-13: keep it for the merge and revisit it afterwards. | after the merge | nothing |
| 049 Decision 5, the ladder (item 052) | A lone held cap takes the whole excess and can become a pause while new victims keep their usage (`src/policy/ladder.rs`, the held group before the new one). Let held and new victims of a tier share at one level. | 001 after the merge | nothing |
| 049 Decision 3 C (item 052) | A Full leaf stays frozen when its supervisor is killed in an off phase or a Pause; nothing thaws it. | 001 after the merge | nothing |
| 050, last review pointer (item 052) | A failed `/proc/<pid>/status` read is kept as "owner unreadable" for the process's lifetime, so one transient failure counts that process toward the unattributed principal until it exits. | 001 after the merge | nothing |
| 050, last review pointer (item 052) | A Drain already delivered is not raised to Terminate when the breach turns critical. SC-012's critical bound still holds: a delivered Drain ends at most the drain timeout plus the termination grace (5 s by default) after its delivery, inside the 6.5 s counted from the critical moment. What is lost is up to the drain timeout (3 s) for a worker that does not poll the drain file. | 001 after the merge | nothing |

## Done

| Source | Where |
|---|---|
| 030 SUP-8: an adjust restarted a CPU-only duty cycle's phase | item 050 (`4cc5761`): the same cap on a running cycle keeps its phase |
| 030 SUP-10: a task launched at nice 10 keeps it in Performance | closed in item 050: by design (029 Part 0b, `launch_nice_kept`) |
| 030 SUP-12: a failed readback plus a failed restore left `memory.high` at the cap | item 050 (`062348b`): the restore's failure is reported and retried each tick |
| 030 AGT-14: a failed read of `execution-outcome.json` reported a success or yield as a failure | item 050 (`23a965d`): a read error other than a missing file is read again |
| 030 AGT-16: a failed policy Drain or Terminate send was never retried | item 050 (`8e0531e`): sent again until delivered, the slot counting as draining meanwhile |
| 030 GPU-6: the GPU compute margin switches on at the smallest external amount | closed in item 050: on a time-sliced device Neutral and Performance are set back to the whole device (`src/policy/measure.rs`, the time-slice rule of items 041–046); elsewhere it is SC-015's target, capacity minus the user minus the margin |
| 030 TEL-7: the busy interval used a timestamp taken before discovery | closed in item 050: fixed by `af531c0` (047 Decision 2) |
| 030 TEL-9: guest time counted twice; a failed `/proc` read gave a known empty list | item 050 (`bd4c735`, `0618858`, `62e555e`): eight fields in the total; an unreadable table is unknown, and admitted principals and per-process peaks stand through it, a GPU memory peak only while the device still lists its process |
| 030 SDK-3: an error on the drain path hid the drain (TypeScript) or raised (Python) | item 050 (`89de57d`): no drain for that call, logged once, checked again |
| 030 SDK-4: an oversize request was reported as uncertain in TypeScript | item 050 (`665a998`) |
| 030 SDK-5: `request_digest` key order differed between the SDKs | item 050 (`6c8d2e3`): sorted keys in both, one pinned value |
| 030 SDK-6: an explicit null amount made TypeScript drop the whole file | item 050 (`41fa664`), contract fixture TF-08 |
| 030 SDK-7: Python's `ManagedChild.wait` raised builtin errors | item 050 (`ba564e3`): the `ERR_CEDEGRID_*` codes |
| 030 SDK-8: a failed ack write was retried and logged on every read | item 050 (`c385ae3`): logged once, retried no sooner than a second later |
| 030 SDK-9: the SDKs disagreed on drain-path precedence | item 050 (`6e7e614`): the context's value, then the environment |
| 044 Q31: the aligned duty phases assume every supervisor shares the agent's `CLOCK_MONOTONIC` time namespace | item 049 (T516), FR-021 and spec Session 2026-11-03 |
| 026 Decision 5 A: a refusal at the authorization barrier uses no attempt | item 049 (T511), FR-018 |
| 027 Decision 2 B: the window's busy bound tightened with per-CPU run time from `/proc/schedstat` | item 049 (T514); the idle rerun is T520 |
| 024 Decision 2(c): new victims of a contended CPU excess share it by usage at one level | item 049 (T512), FR-016 |
| 027 Decision 5 B: charge running work at its mean while no other principal is active, at its p95 once one is | item 049 (T515), decided apart from any change to the modes |
| 024 Decision 3: SC-014 attribution, the supervisor's CPU and the 173 m gap | item 049 (T513): at `743312b` the tasks' CPU from `/proc` is 176–244 m below the decisions' upper bound in all three SC-014 runs, so the gap is gone and no code changed (`docs/validation.md`, "Item 049") |
| 045 note: the re-measure window's 3 s at full duty raised the held duty 0.015–0.04 above d0 | item 046: d0' = (d0 − w) ÷ (1 − w); the held duty fell to 0.754 against d0 0.75, and the rest is 004 T049 |
| 028: Full's GPU share, natively | item 047, Performance beside the saturating user (`docs/validation.md`, "Item 047") |
| 028 Decision 1: a leaf left frozen | item 030 (T286–T287) |
| 029 Decision 1: stop drains | item 030; item 048 Part B measures the stop again |
| 029 Decision 2, 030 Decision 3: the return after contention | item 034 (T412 closes T339) |
| 024 Decisions 1, 2(a), 2(b), 4; 025 Decision 1; 026 Decisions 1–3 | items 025–027 |
| 030 Decision 4 (TEL-3); 031 Decision 3 B; R1 multi-process | items 032, 033, 031 |
