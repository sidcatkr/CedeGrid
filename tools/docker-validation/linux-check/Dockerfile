# Linux build and test image for the checks in specs/001-contention-modes/native-gates.md.
# It has no cgroup delegation and no GPU; those gates stay with T054 and T088.
FROM rust:1.98.1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv procps \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt \
    && useradd -m -u 1000 cedegrid \
    && mkdir -p /home/cedegrid/.cargo /home/cedegrid/target /home/cedegrid/work \
    && chown cedegrid:cedegrid /home/cedegrid/.cargo /home/cedegrid/target /home/cedegrid/work
USER cedegrid
ENV CARGO_HOME=/home/cedegrid/.cargo \
    CARGO_TARGET_DIR=/home/cedegrid/target \
    PATH=/home/cedegrid/.cargo/bin:/usr/local/cargo/bin:$PATH \
    PYTHONDONTWRITEBYTECODE=1 \
    CEDEGRID_SERVICE_TEST_MONITOR_MS=500
WORKDIR /home/cedegrid/work
