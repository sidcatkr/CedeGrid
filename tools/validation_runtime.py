"""Owned-process validation helpers. No persisted-PID adoption or process-name kill."""
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time
import uuid


def inside_home(path):
    path = Path(path).expanduser().resolve()
    path.relative_to(Path.home().resolve())
    return path


def home_executable(path):
    """Keep venv interpreter spelling; resolving its final symlink loses the venv."""
    path=Path(path).expanduser().absolute()
    parent=inside_home(path.parent)
    executable=parent/path.name
    if not executable.is_file() or not os.access(executable,os.X_OK):
        raise ValueError('home-local executable is missing or not executable')
    return executable


def atomic_json(path, value):
    atomic_text(path, json.dumps(value, indent=2, allow_nan=False) + '\n')


def atomic_text(path, value):
    path = inside_home(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.tmp-' + uuid.uuid4().hex)
    try:
        with temporary.open('x', encoding='utf-8') as out:
            out.write(value)
            out.flush()
            os.fsync(out.fileno())
        os.replace(temporary, path)
        fd = os.open(path.parent, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        temporary.unlink(missing_ok=True)


def linux_identity(pid):
    """Read-only identity evidence; never an authorization to adopt a PID."""
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return {'pid': pid, 'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                'start_ticks': int(fields[19])}
    except (OSError, IndexError, ValueError):
        return {'pid': pid, 'boot_id': None, 'start_ticks': None}


class OwnedProcess:
    """Direct unreaped child under one exclusive reaper; pidfd signaling where available.

    The caller must honor the no-daemon/single-process contract. A leader handle
    does not identify arbitrary descendants. A persisted report cannot reopen this
    live capability after a harness crash.
    """
    def __init__(self, argv, cwd, log, env=None):
        if hasattr(signal, 'SIGCHLD') and signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
            raise RuntimeError('exclusive child reaping requires default SIGCHLD handling')
        self.argv = list(argv)
        self.log = inside_home(log)
        self.log.parent.mkdir(parents=True, exist_ok=True)
        self.stream = self.log.open('xb')
        self.lock = threading.Lock()
        self.pidfd = None
        self.handle_mode = 'exclusive_unreaped_direct_child_fallback'
        self.handle_error = None
        try:
            self.child = subprocess.Popen(argv, cwd=inside_home(cwd), env=env,
                                          stdin=subprocess.DEVNULL, stdout=self.stream,
                                          stderr=subprocess.STDOUT)
        except BaseException:
            self.stream.close()
            raise
        self.started = time.monotonic()
        # No poll/wait occurs before pidfd acquisition. A child that exited is
        # still our unreaped zombie under the default SIGCHLD contract.
        if hasattr(os, 'pidfd_open') and hasattr(signal, 'pidfd_send_signal'):
            try:
                self.pidfd = os.pidfd_open(self.child.pid)
                signal.pidfd_send_signal(self.pidfd, 0)
                self.handle_mode = 'pidfd'
            except OSError as error:
                self.handle_error = type(error).__name__ + ':' + str(error.errno)
                if self.pidfd is not None:
                    os.close(self.pidfd)
                    self.pidfd = None
        self.identity = linux_identity(self.child.pid)
        self._outcome = None

    def poll(self):
        with self.lock:
            return self.child.poll()

    def _signal(self, sig):
        if self.pidfd is not None:
            try:
                signal.pidfd_send_signal(self.pidfd, sig)
            except ProcessLookupError:
                pass
        else:
            # Under this lock, no competing wait or poll can reap and release
            # the numeric PID between inspection and signal delivery.
            self.child.send_signal(sig)

    def stop(self, grace=2, kill_wait=5):
        if grace < 0 or kill_wait <= 0:
            raise ValueError('termination waits must be bounded and nonnegative')
        with self.lock:
            if self._outcome is not None:
                return dict(self._outcome)
            signals = []
            try:
                if self.child.poll() is None:
                    self._signal(signal.SIGTERM)
                    signals.append('TERM')
                    try:
                        self.child.wait(timeout=grace)
                    except subprocess.TimeoutExpired:
                        self._signal(signal.SIGKILL)
                        signals.append('KILL')
                        self.child.wait(timeout=kill_wait)
                self._outcome = {**self.identity, 'exit_code': self.child.returncode,
                    'elapsed_seconds': time.monotonic()-self.started, 'reaped': self.child.returncode is not None,
                    'signals': signals, 'handle_mode': self.handle_mode, 'handle_error': self.handle_error,
                    'descendant_contract': 'single_process_no_daemon'}
                return dict(self._outcome)
            finally:
                if self.child.returncode is not None:
                    self.stream.close()
                    if self.pidfd is not None:
                        os.close(self.pidfd)
                        self.pidfd = None


def local_guard(output, limits):
    """Conservative local guard evidence; missing required readings abort."""
    output = inside_home(output)
    status = {'observed_unix': time.time(), 'scope': 'local_host', 'errors': []}
    disk = os.statvfs(output)
    status['disk_available_bytes'] = disk.f_bavail * disk.f_frsize
    if status['disk_available_bytes'] < limits.get('min_disk_free_bytes', 0):
        status['errors'].append('disk_headroom_below_bound')
    status['output_bytes'] = sum(path.stat().st_size for path in output.rglob('*') if path.is_file())
    if status['output_bytes'] > limits.get('max_output_bytes', 2**63-1):
        status['errors'].append('output_budget_exceeded')
    try:
        memory = {line.split(':', 1)[0]: int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines()}
        status['ram_available_bytes'] = memory.get('MemAvailable')
    except (OSError, ValueError, IndexError):
        status['ram_available_bytes'] = None
    minimum = limits.get('min_ram_free_bytes')
    if minimum is not None and (status['ram_available_bytes'] is None or status['ram_available_bytes'] < minimum):
        status['errors'].append('ram_headroom_unavailable_or_below_bound')
    return status
