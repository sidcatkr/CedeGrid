#!/usr/bin/env python3
"""Exercise original installed POSIX launcher bytes in a real controlling PTY."""
from __future__ import annotations
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import pty
import resource
import select
import signal
import subprocess
import tempfile
import time


def invoke(argv, directory):
    return subprocess.run([str(item) for item in argv], cwd=directory, capture_output=True, timeout=20)


def terminal_case(argv, directory, method, expect_execve, expected_exit=None):
    requested_signal = {'terminal-ctrl-c': signal.SIGINT, 'direct-sigint': signal.SIGINT,
                        'direct-sigterm': signal.SIGTERM, 'direct-sighup': signal.SIGHUP,
                        'direct-sigquit': signal.SIGQUIT}[method]
    child, terminal = pty.fork()
    if child == 0:
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        os.chdir(directory)
        os.execv(str(argv[0]), [str(item) for item in argv])
    output = bytearray()
    observed_status = None
    foreground = None
    delivered = None
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([terminal], [], [], .05)
            if ready:
                try:
                    chunk = os.read(terminal, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b''
                output.extend(chunk)
            # The first sample precedes observe's first poll of its shutdown
            # future. A second sample proves its signal listeners were installed.
            if delivered is None and output.count(b'\n') >= 2 and b'{' in output:
                foreground = os.tcgetpgrp(terminal)
                if foreground <= 1 or (foreground == child) != expect_execve:
                    raise RuntimeError('foreground group does not match the runtime launch strategy')
                if not expect_execve and os.getpgid(child) == foreground:
                    raise RuntimeError('Node fallback and native child share terminal signal delivery')
                delivered = time.monotonic()
                if method == 'terminal-ctrl-c':
                    os.write(terminal, b'\x03')
                else:
                    os.kill(child, requested_signal)
            reaped, status = os.waitpid(child, os.WNOHANG)
            if reaped:
                observed_status = status
                break
        if observed_status is None:
            raise TimeoutError('launcher did not exit within the terminal test deadline')
        exit_code = os.waitstatus_to_exitcode(observed_status)
        allowed = (0, -requested_signal) if expected_exit is None else (expected_exit,)
        if delivered is None or exit_code not in allowed:
            raise RuntimeError(f'launcher terminal exit failed ({exit_code}): {output.decode(errors="replace")[-2000:]}')
        return {'method': method, 'status': 'PASS', 'exit_code': exit_code,
                'native_owns_foreground': True, 'fallback_signal_groups_separate': not expect_execve,
                'elapsed_after_signal_seconds': time.monotonic() - delivered,
                'native_observation_received': True}
    finally:
        if observed_status is None:
            # These IDs belong to the child and foreground group created by this
            # PTY only. Never enumerate or signal unrelated host processes.
            for owned in set([child] + ([foreground] if foreground and foreground != child else [])):
                try: os.kill(owned, signal.SIGKILL)
                except ProcessLookupError: pass
            try: os.waitpid(child, 0)
            except ChildProcessError: pass
        os.close(terminal)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidate', type=Path)
    parser.add_argument('--npm-install', required=True, type=Path)
    parser.add_argument('--node', required=True, action='append', type=Path)
    parser.add_argument('--target', default='darwin-arm64')
    args = parser.parse_args()
    candidate, installation = args.candidate.resolve(), args.npm_install.resolve()
    package = installation / 'node_modules/cedegrid'
    binary = installation / ('node_modules/cedegrid-' + args.target + '/bin/cedegrid')
    launcher = package / 'dist/launcher.js'
    manifest_path = candidate / 'manifest.json'
    if not manifest_path.exists(): manifest_path = candidate / 'manifest.partial.json'
    manifest = json.loads(manifest_path.read_text())
    expected = manifest['native_inputs'][args.target]['sha256']
    if hashlib.sha256(binary.read_bytes()).hexdigest() != expected:
        raise RuntimeError('installed native bytes differ from the immutable candidate')
    records = []
    with tempfile.TemporaryDirectory(prefix='cedegrid-launcher-pty-') as temporary:
        directory = Path(temporary)
        config = directory / '설정 with spaces.toml'
        config.write_text('config_version=1\nnode_id="launcher-terminal-test"\n[monitor]\ninterval_ms=100\n', encoding='utf-8')
        methods = ['terminal-ctrl-c', 'direct-sigint', 'direct-sigterm', 'direct-sighup', 'direct-sigquit']
        baseline = {method: terminal_case([binary, '--config', config, 'observe', '--samples', '0', '--no-state'],
                                         directory, method, True) for method in methods}
        for node in args.node:
            node = node.resolve()
            version = invoke([node, '--version'], directory).stdout.decode().strip()
            strategy = invoke([node, '-p', 'typeof process.execve'], directory).stdout.strip() == b'function'
            for argv in (['config-example', '--kind', 'client'], ['--invalid-release-test-option']):
                direct, wrapped = invoke([binary, *argv], directory), invoke([node, launcher, *argv], directory)
                if (direct.returncode, direct.stdout, direct.stderr) != (wrapped.returncode, wrapped.stdout, wrapped.stderr):
                    raise RuntimeError('launcher changed native arguments, output, or exit code')
            cases = [terminal_case([node, launcher, '--config', config, 'observe', '--samples', '0', '--no-state'],
                                   directory, method, strategy, baseline[method]['exit_code'])
                     for method in methods]
            records.append({'node': version, 'strategy': 'execve' if strategy else 'native-child-foreground-handoff',
                            'argument_stdout_stderr_exit_equivalence': 'PASS', 'cases': cases})
    evidence = {'schema_version': 1, 'status': 'PASS', 'platform': os.uname().sysname,
                'native_sha256': expected, 'native_baseline': baseline,
                'node_results': records, 'windows_console': 'NOT_RUN'}
    (candidate / 'launcher-pty-evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
    print(json.dumps(evidence, indent=2))


if __name__ == '__main__':
    main()
