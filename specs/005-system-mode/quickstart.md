# Quickstart: validating System mode

Three places, in this order. Nothing runs on the shared host (FR-031).

## 1. Unit tests (any Linux or macOS machine, no root)

```sh
cargo test --locked --test system_mode
```

What they prove:
- the request parser refuses every case in [broker-protocol.md](contracts/broker-protocol.md);
- the property builder's output depends only on the user ID and the policy;
- the policy loader refuses bad owners and modes;
- the score function gives the values in [data-model.md](data-model.md) for each N1 option and D2
  case, including the −999 clamp and no write when nothing changed.

## 2. The container (CI, systemd as PID 1, no server touched)

The CI job on `ubuntu-22.04` builds both packages, starts a systemd container, and runs:

```sh
CEDEGRID_TEST_SYSTEM_CONTAINER=1 cargo test --locked --test system_mode -- --ignored --test-threads=1
```

This runs only `tests/system_mode.rs`'s opt-in fixtures, never `--ignored` across the suite
(the repository's contributor notes). Expected results:

| Check | Expected |
|---|---|
| install, then the file and unit inventory | only what [units-and-slices.md](contracts/units-and-slices.md) lists was added |
| `cpu.idle` on `cedegrid-idle.slice` | reads 1; `doctor` names the path R1 took on systemd 249 |
| a member's run | lands in `cedegrid-u<uid>.service` with the member's IDs; `doctor` says System (SC-001) |
| the security suite (FR-029) | every case refused or held, logged, nothing else changed (SC-002) |
| a flood from one member | another member answered within SC-003's bound |
| a move between the two units | the job's processes are in the other unit, nothing else moved; a forged job is refused |
| scores | in-share tasks lowered per N1, never below −999; a user write below a root-written value fails (SC-008); a process outside the units keeps its score |
| purge, then the inventory | equal to before the install (SC-009) |
| without the package | the existing suites pass unchanged (SC-010) |

## 3. The test host (the other session; the user does the root steps)

1. The user reads and runs research R13's steps 1 and 2 (install, group). The work session uses
   no `sudo`.
2. The other session runs R2-run, R3-run, R10-run and R16-run as listed in
   [research.md](research.md), with plain Linux runs for every comparison, at least 3 each way.
3. The user runs R13 step 4 to hand over the broker's journal for the runs, and step 6 to remove
   everything at the end.

Results go to `docs/validation.md` as measured, with the commit. Nothing is called done while a
gate is unmeasured at the commit under test.
