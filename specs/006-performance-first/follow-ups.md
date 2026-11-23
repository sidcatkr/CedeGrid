# Follow-ups deferred by feature 006

One line per piece of work left for a later version: its source, what it is, its target, and
what it blocks. The format follows [001's follow-ups](../001-contention-modes/follow-ups.md).
Lines move to "Done" with a pointer once a later version does them.

## Open

| Source | What | Target | Blocks |
|---|---|---|---|
| SC-001, GPU start (2026-11-23, `cad924c`) | A GPU gate waits for a device reading taken at least half a monitor interval after the agent's last sample, about 200 ms at 500 ms, which leaves GPU F2c 0.57% below the same work by hand. NVML's per-process samples come about six a second, so a GPU-only floor near one sample period (about 170 ms) could gain about 0.1%, if 001's admission review finds that window still trustworthy. A reading taken at once has too few samples and must not be used. | next version, with 001 | nothing |
| SC-001, start path (2026-11-22, `562154c`, `35016ed`) | A launch's synced writes queue on the one state database across a burst, so a busy disk stretches every start. Two adjacent commits could merge without changing what is durable: the download charge's accounting with its reservation, and the publisher's pin with its accounting. About 1–7 ms per launch, more under disk load. | next version | nothing |
| SC-001, start path | Each task submitted as its own job is one synced coordinator commit, about 9 ms apart, and the agent waits 50 ms after the last arrival so the burst goes in one heartbeat. A burst submitted as one job would arrive at once and start that much sooner; worth a line in the SDK guidance or a batch submit. | next version, SDKs | nothing |
| Agent and supervisor versions (2026-11-23) | The agent counts a supervisor event it cannot parse as the run's failure, so a newer supervisor's new event would fail a launch under an older agent. A capability field in the supervisor spec, or ignoring unknown events, would let later versions add events, such as a GPU gate saying it waits for a reading. | next version | adding supervisor events |
| SC-006 (2026-11-23) | The System mode unprotected raise comes 0.8–5.5 s after submission, at the agent's next raise check while no raise is in force; it could come with the run's start. | next version | nothing |
| T051 | Own work first still trails the same work by hand by about 1% on CPU, and the batch's total time by 1–6%: the start (0.16–0.38 s against 0.04 s) and the handoff between tasks. | next version | nothing |

## Done

None yet.
