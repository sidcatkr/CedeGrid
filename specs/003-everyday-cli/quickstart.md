# Quickstart validation: Everyday CLI

Each scenario lists its commands and the expected result. Native scenarios run on a Linux test
host; the rest run in CI.

## 1. First run (US1, SC-001)
1. On an account with no CedeGrid state, run `cedegrid run -- python3 -c "print('ok')"`.
2. Expect a local agent start, `ok` printed, exit code 0, and the run listed by `cedegrid ps`
   within 5 seconds of the command.

## 2. Schema parity (FR-004, SC-002)
- `cargo test --test task_schema`, plus the Python and npm suites.
- Expect: the fixture is current, and every `command_task` field has a flag and an SDK
  parameter.

## 3. Viewer and logs (FR-015, FR-016)
1. Start a run that prints every second, close the terminal, then run
   `cedegrid logs <run> -f`.
2. Expect: the run is still running and the output continues.
3. Ctrl-C once in a foreground run: it drains. Twice: it stops.

## 4. Per-job setup (US3)
1. `cedegrid setup train.py`, save; then `cedegrid run --explain train.py`.
2. Expect: one line naming `./cedegrid.toml`, and each value with its source. A flag beats the
   entry, and `--no-setup` ignores it.

## 5. Control levels (US6, native)
1. On a host with delegation, `cedegrid doctor` shows Full. With
   `CEDEGRID_TEST_FORCE_TRACKED=1`, it shows Tracked.
2. Start a CPU benchmark under each level. Start a contender:
   - (a) from the run's own session, so it shares the run's CPU cgroup;
   - (b) under a second account, in another cgroup.
3. Expect, for both levels and both placements: CPU given back within the times of SC-004, no
   process outside the run's tree touched, and `ps` and `top` showing the step and the reason.

## 6. Descendants (Principle I)
1. Run a script that forks workers, one that daemonizes and one that executes a setuid program.
2. Expect:
   - the daemonized worker stays tracked;
   - the setuid process leaves the verified set and is never acted on;
   - a process started outside the tree is not tracked.

## 7. Overhead (SC-003, SC-005, native)
1. Run the CPU and GPU benchmarks five times bare and five times under `run`, with no
   contention.
2. Expect medians within 2%, and Tracked within 2% of Full.

## 8. Bottleneck (US7)
1. `cedegrid run -- env OMP_NUM_THREADS=1 <benchmark>` on four granted cores.
2. Expect a bottleneck flag within three intervals of the window, naming `OMP_NUM_THREADS=1`.
   With `--threads auto`, the inherited value is replaced and `--explain` shows it.
