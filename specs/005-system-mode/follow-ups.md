# Feature 005 follow-ups

Items deferred from feature 005, each with the reason it waits.

| ID | Item | Target | Reason |
|---|---|---|---|
| SYS-1 | `units.ordinary_slice = "cedegrid-weighted"` (decision N4 C): the broker sets `cedegrid.slice`'s CPU weight from per-account CPU counters it reads, within a clamped range, and falls back to the lowest weight when a counter is unknown. | first release after 005 | The user's decision of 2026-11-17. Principle V 8.0.0 lists what the broker may do, and changing a slice's weight from other accounts' activity is not on that list, so it needs its own amendment. `auto` (N4 A) and `cedegrid` (N4 B) ship in 005. |
| SYS-2 | Per-account cgroup memory counters for the OOM pass (FR-022, "where the host has them"). | 005, before the host run T058 | The pass sums per-process resident and swapped pages by user ID on every host for now; the counters must be mapped to 001's RAM measure and pinned by a test first (research R10). |
