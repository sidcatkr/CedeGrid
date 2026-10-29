#!/bin/sh
# Runs inside the linux-check image with a `git archive` export mounted read-only
# at /src and a named volume at $HOME/work: state stores refuse overlay
# filesystems, and test fixtures must live under $HOME.
# Usage: run.sh [cargo test arguments...]
set -eu
tar -C /src --exclude=./target --exclude='./.agent-test-*' --exclude='./.distributed-service-*' \
    --exclude=./npm/node_modules --exclude=./.git -cf - . | tar -C "$HOME/work" -xf -
cd "$HOME/work"
# A `git archive` stamps files with its commit time, which can be older than a build kept in the
# target volume; newer stamps make cargo rebuild what changed.
find src tests -type f -exec touch {} +
mkdir -p .runtime/tmp
export TMPDIR="$HOME/work/.runtime/tmp"
python3 -m venv .runtime/test-python
export CEDEGRID_TEST_PYTHON="$HOME/work/.runtime/test-python/bin/python"
rustc --version && python3 --version && uname -a
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-fail-fast "$@" -- --test-threads=1
PYTHONPATH="$HOME/work/python" "$CEDEGRID_TEST_PYTHON" -m unittest discover -s tests -p 'test_*.py'
PYTHONPATH="$HOME/work/python" "$CEDEGRID_TEST_PYTHON" -m unittest discover -s python/tests -p 'test_*.py'
