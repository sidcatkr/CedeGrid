#!/bin/sh
# Runs inside the system-mode image, as root, once systemd is up. The source is mounted
# read-only at /src; the test account builds into its home, and the System mode container
# fixtures then run as root.
# Usage: run.sh [test name filter]
set -eu
filter=${1:-container::}
# docker exec starts this shell in the container's root cgroup, where no controller can be
# enabled while it holds processes. Move it to a leaf first; on a host, systemd's root is empty.
mkdir -p /sys/fs/cgroup/harness
echo $$ > /sys/fs/cgroup/harness/cgroup.procs
echo "+cpu +memory +pids" > /sys/fs/cgroup/cgroup.subtree_control || true
runuser -u test -- env CEDEGRID_TEST_RELEASE="${CEDEGRID_TEST_RELEASE:-}" sh -eu -c '
    mkdir -p "$HOME/work"
    tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C "$HOME/work" -xf -
    cd "$HOME/work"
    find src tests -type f -exec touch {} +
    . "$HOME/.cargo/env"
    if [ "${CEDEGRID_TEST_RELEASE:-}" = 1 ]; then
        # Thin LTO keeps the build within the test VM'"'"'s memory; the cost figures need it.
        CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            cargo build --locked --release --bin cedegrid
    else
        cargo build --locked --bin cedegrid
    fi
    profile=debug
    if [ "${CEDEGRID_TEST_RELEASE:-}" = 1 ]; then profile=release; fi
    rm -rf "$HOME/debs"
    python3 tools/build_deb.py --binary "target/$profile/cedegrid" --packaging packaging/deb \
        --version 0.0.0~test --arch "$(dpkg --print-architecture)" --output "$HOME/debs"
    cargo test --locked --test system_mode --no-run --message-format=json \
        | python3 -c "import json,sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get(\"reason\") == \"compiler-artifact\" and m.get(\"executable\") and m[\"target\"][\"name\"] == \"system_mode\":
        print(m[\"executable\"])" > "$HOME/system_mode_test"
'
test_bin=$(cat /home/test/system_mode_test)
profile=debug
if [ "${CEDEGRID_TEST_RELEASE:-}" = 1 ]; then profile=release; fi
CEDEGRID_TEST_SYSTEM_CONTAINER=1 \
CEDEGRID_TEST_RELEASE="${CEDEGRID_TEST_RELEASE:-}" \
CEDEGRID_TEST_BIN=/home/test/work/target/$profile/cedegrid \
CEDEGRID_TEST_PACKAGING=/home/test/work/packaging/deb \
CEDEGRID_TEST_DEBS=/home/test/debs \
    "$test_bin" --test-threads=1 --ignored "$filter"
