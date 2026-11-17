# Ubuntu 22.04 (systemd 249, glibc 2.35) with systemd as PID 1, for System mode's container
# fixtures (specs/005-system-mode, T004 and T005). CedeGrid is built here by the unprivileged "test"
# account; the fixtures run as root and act as the test accounts they create.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv dbus ca-certificates \
        curl build-essential pkg-config python3 procps passwd \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1500 test \
    && su test -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.98.1' \
    && systemctl mask getty.target console-getty.service systemd-udevd.service \
        systemd-remount-fs.service sys-kernel-debug.mount sys-kernel-tracing.mount
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
