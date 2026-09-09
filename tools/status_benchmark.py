#!/usr/bin/env python3
"""Measure first-page status latency and RSS against a real native coordinator.

The companion Rust example seeds an isolated offline database with valid cancelled
jobs, then traces the actual native pagination reader for EXPLAIN evidence. This
tool launches no agents or workloads and never publishes an artifact or package.
"""
from __future__ import annotations

import argparse
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time

from make_test_pki import generate as generate_pki
from runtime_config import atomic_runtime_config
from validation_runtime import atomic_json, inside_home


def digest(path):
    value = hashlib.sha256()
    with Path(path).open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


def source_identity(root):
    files = [root / name for name in ('Cargo.toml', 'Cargo.lock')]
    for folder, pattern in [('src', '*.rs'), ('examples', 'status_benchmark_probe.rs'),
                            ('python/cedegrid', '*.py'), ('tools', 'status_benchmark.py')]:
        files.extend((root / folder).rglob(pattern))
    records = [{'path': path.relative_to(root).as_posix(), 'sha256': digest(path)}
               for path in sorted(set(files)) if path.is_file()]
    return {'tree_sha256': hashlib.sha256(json.dumps(records, sort_keys=True,
            separators=(',', ':')).encode()).hexdigest(), 'files': records}


class CoordinatorProcess:
    """One directly spawned, unreaped child, with one exclusive wait4 reaper.

    wait4 retains that child's exact lifetime peak RSS. No persisted PID is read,
    no process name is matched, and no unrelated process or process group is used.
    """
    def __init__(self, argv, output):
        if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
            raise RuntimeError('benchmark requires exclusive child reaping with default SIGCHLD')
        self.stream = (output / 'coordinator.log').open('xb')
        try:
            self.process = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                stdout=self.stream, stderr=subprocess.STDOUT, cwd=output)
        except BaseException:
            self.stream.close()
            raise
        self.pid = self.process.pid
        self.exit_code = None
        self.usage = None
        self.lock = threading.Lock()

    def _reap(self):
        if self.exit_code is None:
            child, status, usage = os.wait4(self.pid, os.WNOHANG)
            if child:
                self.exit_code = os.waitstatus_to_exitcode(status)
                self.process.returncode = self.exit_code
                self.usage = usage
        return self.exit_code

    def poll(self):
        with self.lock:
            return self._reap()

    def stop(self):
        with self.lock:
            if self._reap() is None:
                os.kill(self.pid, signal.SIGTERM)
        deadline = time.monotonic() + 5
        while self.poll() is None and time.monotonic() < deadline:
            time.sleep(.02)
        with self.lock:
            if self._reap() is None:
                os.kill(self.pid, signal.SIGKILL)
                child, status, usage = os.wait4(self.pid, 0)
                assert child == self.pid
                self.exit_code = os.waitstatus_to_exitcode(status)
                self.process.returncode = self.exit_code
                self.usage = usage
        self.stream.close()
        peak = int(self.usage.ru_maxrss) if self.usage else None
        if peak is not None and sys.platform.startswith('linux'):
            peak *= 1024
        return {'owned_child_reaped': True, 'exit_code': self.exit_code,
                'lifetime_peak_rss_bytes': peak, 'source': 'wait4.rusage.ru_maxrss'}


def rss_reader(pid):
    if sys.platform.startswith('linux'):
        def read():
            for line in Path(f'/proc/{pid}/status').read_text().splitlines():
                if line.startswith('VmRSS:'):
                    return int(line.split()[1]) * 1024
            raise RuntimeError('owned coordinator RSS is unavailable')
        return read, 'Linux /proc/<owned-pid>/status VmRSS'
    if sys.platform == 'darwin':
        # Darwin SDK sys/proc_info.h: PROC_PIDTASKINFO=4, six uint64 fields
        # followed by twelve int32 fields. Resident size is expressed in bytes.
        class TaskInfo(ctypes.Structure):
            _fields_ = [('virtual_size', ctypes.c_uint64), ('resident_size', ctypes.c_uint64)] + [
                (f'time_{i}', ctypes.c_uint64) for i in range(4)] + [
                (f'counter_{i}', ctypes.c_int32) for i in range(12)]
        library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        library.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                        ctypes.c_void_p, ctypes.c_int]
        library.proc_pidinfo.restype = ctypes.c_int

        def read():
            info = TaskInfo()
            size = library.proc_pidinfo(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info))
            if size != ctypes.sizeof(info):
                raise OSError(ctypes.get_errno(), 'proc_pidinfo did not return complete RSS data')
            return int(info.resident_size)
        return read, 'Darwin proc_pidinfo(PROC_PIDTASKINFO).resident_size'
    raise RuntimeError('benchmark RSS backend requires Linux or macOS')


class RssSampler:
    def __init__(self, read):
        self.read = read
        self.stop_event = threading.Event()
        self.started = time.monotonic()
        self.samples = []
        self.errors = []
        self.phase = 'idle'
        self.thread = threading.Thread(target=self._run, name='benchmark-rss', daemon=True)

    def _run(self):
        while not self.stop_event.is_set():
            try:
                self.samples.append({'elapsed_seconds': time.monotonic() - self.started,
                                     'rss_bytes': self.read(), 'phase': self.phase})
            except (OSError, RuntimeError) as error:
                self.errors.append(str(error))
            self.stop_event.wait(.005)

    def start(self):
        self.thread.start()

    def stop(self):
        self.stop_event.set()
        self.thread.join()


def helper(probe, operation, deployment, output, tasks=None):
    argv = [str(probe), operation, str(deployment)]
    if tasks is not None:
        argv.append(str(tasks))
    result = subprocess.run(argv, capture_output=True, text=True, timeout=600)
    (output / (operation + '.stderr.log')).write_text(result.stderr)
    if result.returncode:
        raise RuntimeError(f'native {operation} failed ({result.returncode}); inspect retained stderr')
    value = json.loads(result.stdout)
    atomic_json(output / (operation + '.json'), value)
    return value


def query_plans_are_bounded(explanation):
    checked = []
    for query in explanation['queries']:
        selected = [entry for entry in query['plans']
                    if entry['executed_sql'].startswith('SELECT s.sequence')]
        if len(selected) != 1:
            raise RuntimeError('expected exactly one traced native page statement')
        entry = selected[0]
        descriptions = [item['detail'] for item in entry['plan']]
        if not any('SEARCH s ' in value for value in descriptions):
            raise RuntimeError('native page query is missing an indexed sequence search')
        if any('SCAN ' in value or 'USE TEMP B-TREE' in value for value in descriptions):
            raise RuntimeError('native page query scans/sorts instead of using its bounded index')
        if 'LIMIT 101' not in entry['executed_sql']:
            raise RuntimeError('native reader did not enforce page size plus one')
        checked.append({'job_scoped': query['query']['job_id'] is not None,
                        'indexed_bounded_page': True, 'plan': descriptions})
    return checked


def measure_dataset(args, output, count, credentials):
    from cedegrid import Client

    output.mkdir(mode=0o700)
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    endpoint = f'https://127.0.0.1:{port}'

    def tls(name):
        return {'ca_cert': str(credentials / 'ca.pem'), 'certificate': str(credentials / (name + '.pem')),
                'private_key': str(credentials / (name + '.key'))}

    clients = json.loads((credentials.parent / 'pki-index.json').read_text())['clients']
    deployment = output / 'coordinator.toml'
    atomic_runtime_config(deployment, {'listen': f'127.0.0.1:{port}',
        'state_dir': str(output / 'state'), 'tls': tls('server'), 'clients': clients}, 'coordinator')
    atomic_runtime_config(output / 'operator.toml', {'endpoint': endpoint, 'tls': tls('operator')}, 'client')
    result = {'tasks': count, 'status': 'RUNNING', 'measurements': [], 'workloads_started': False}
    server = sampler = None
    try:
        result['seed'] = helper(args.native_probe, 'seed', deployment, output, count)
        result['explain_before'] = helper(args.native_probe, 'explain', deployment, output)
        result['query_plan_checks'] = query_plans_are_bounded(result['explain_before'])
        server = CoordinatorProcess([str(args.binary), 'coordinator', '--deployment', str(deployment)], output)
        client = Client.from_config(output / 'operator.toml')
        deadline = time.monotonic() + 30
        while True:
            if server.poll() is not None:
                raise RuntimeError('coordinator exited before readiness; inspect coordinator.log')
            try:
                # Do not warm the first tasks page while checking readiness.
                client.status_page('nodes', limit=1)
                break
            except Exception:
                if time.monotonic() >= deadline:
                    raise RuntimeError('coordinator readiness deadline elapsed') from None
                time.sleep(.05)
        read_rss, source = rss_reader(server.pid)
        sampler = RssSampler(read_rss)
        sampler.start()
        time.sleep(.2)
        idle = [sample['rss_bytes'] for sample in sampler.samples if sample['phase'] == 'idle']
        if not idle:
            raise RuntimeError('idle coordinator RSS was not observed')
        result['idle_rss_bytes'] = min(idle)
        result['rss_source'] = source
        last_job = f'benchmark-job-{count // 1000 - 1:06}'
        for scope, job in [('unfiltered', None), ('job_scoped', last_job)]:
            sampler.phase = scope
            for sequence in range(args.measurements):
                began = time.monotonic()
                sample = {'scope': scope, 'sequence': sequence, 'limit': 100, 'cursor': None}
                try:
                    page = client.status_page('tasks', job_id=job, limit=100)
                    sample['elapsed_seconds'] = time.monotonic() - began
                    if len(page['items']) != 100 or not page.get('next_cursor'):
                        raise RuntimeError('first page did not contain 100 items and a continuation')
                    offset = count - 1000 if job else 0
                    expected = [f'benchmark-task-{index:09}' for index in range(offset, offset + 100)]
                    if [item['task_id'] for item in page['items']] != expected:
                        raise RuntimeError('native page ordering or task identity mismatch')
                    if job and any(item['job_id'] != job for item in page['items']):
                        raise RuntimeError('job-scoped page included unrelated tasks')
                    payload = json.dumps(page, separators=(',', ':'), ensure_ascii=False).encode()
                    sample.update(status='PASS', items=100, response_bytes=len(payload),
                                  response_sha256=hashlib.sha256(payload).hexdigest())
                    if len(payload) > 7 * 1024 * 1024:
                        raise RuntimeError('serialized page exceeds its limit')
                except Exception as error:
                    sample.update(status='FAIL', error=type(error).__name__ + ': ' + str(error),
                                  elapsed_seconds=time.monotonic() - began)
                result['measurements'].append(sample)
                time.sleep(.005)
        result['latency'] = {}
        for scope in ('unfiltered', 'job_scoped'):
            rows = [row for row in result['measurements'] if row['scope'] == scope]
            durations = sorted(row['elapsed_seconds'] for row in rows)
            result['latency'][scope] = {'samples': len(rows), 'failures': sum(row['status'] != 'PASS' for row in rows),
                'p95_seconds': durations[math.ceil(.95 * len(durations)) - 1], 'max_seconds': durations[-1],
                'method': 'nearest rank; end-to-end mTLS request; failures/timeouts retained'}
    except BaseException as error:
        result['error'] = type(error).__name__ + ': ' + str(error)
        if isinstance(error, KeyboardInterrupt):
            result['interrupted'] = True
    finally:
        if sampler:
            sampler.stop()
            atomic_json(output / 'rss-samples.json', sampler.samples)
            result['rss_sample_count'] = len(sampler.samples)
            result['rss_read_errors'] = sampler.errors
            result['sampled_peak_rss_bytes'] = max((item['rss_bytes'] for item in sampler.samples), default=0)
        if server:
            result['cleanup'] = server.stop()
    if 'idle_rss_bytes' in result:
        lifetime = result.get('cleanup', {}).get('lifetime_peak_rss_bytes')
        if lifetime is not None:
            result['incremental_peak_rss_bytes'] = max(0, max(lifetime,
                result['sampled_peak_rss_bytes']) - result['idle_rss_bytes'])
            result['memory_method'] = 'Conservative max(lifetime wait4 peak, sampled peak) minus minimum idle RSS; includes startup peak'
    latency_ok = all(value['samples'] == args.measurements and value['failures'] == 0 and value['p95_seconds'] < 1
                     for value in result.get('latency', {}).values()) and len(result.get('latency', {})) == 2
    memory_ok = result.get('incremental_peak_rss_bytes', 2**63) <= 64 * 1024 * 1024 and not result.get('rss_read_errors', ['missing'])
    result['status'] = 'PASS' if latency_ok and memory_ok and 'error' not in result else 'FAIL'
    atomic_json(output / 'report.json', result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--native-probe', type=Path)
    parser.add_argument('--tasks', type=int, action='append', help='default: 100000 and 1000000')
    parser.add_argument('--measurements', type=int, default=30)
    args = parser.parse_args()
    if sys.platform not in ('darwin', 'linux') or not hasattr(os, 'wait4'):
        parser.error('this benchmark requires Linux or macOS with wait4 RSS accounting')
    if not 3 <= args.measurements <= 100:
        parser.error('measurements must be in 3..100')
    args.tasks = args.tasks or [100_000, 1_000_000]
    if any(not 1000 <= count <= 1_000_000 or count % 1000 for count in args.tasks) or len(set(args.tasks)) != len(args.tasks):
        parser.error('task counts must be unique multiples of 1000 in 1000..1000000')
    args.source_root = args.source_root.resolve(strict=True)
    args.binary = (args.binary or args.source_root / 'target/debug/cedegrid').resolve(strict=True)
    args.native_probe = (args.native_probe or args.source_root / 'target/debug/examples/status_benchmark_probe').resolve(strict=True)
    args.output = inside_home(args.output)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    source_before = source_identity(args.source_root)
    artifact_before = {name: digest(path) for name, path in [('coordinator', args.binary), ('native_probe', args.native_probe)]}
    artifact_inputs = {'coordinator': args.binary, 'native_probe': args.native_probe}
    pinned = args.output / 'bin'
    pinned.mkdir(mode=0o700)
    for name, source in artifact_inputs.items():
        destination = pinned / name
        shutil.copyfile(source, destination)
        destination.chmod(0o700)
        if digest(destination) != artifact_before[name] or digest(source) != artifact_before[name]:
            raise RuntimeError('native artifact changed while pinning benchmark bytes; retain this run and retry')
    args.binary, args.native_probe = pinned / 'coordinator', pinned / 'native_probe'
    report = {'schema_version': 1, 'started_at': datetime.now(timezone.utc).isoformat(),
        'status': 'RUNNING', 'phase': 'development worktree; final candidate must rerun after freeze',
        'final_candidate_qualification': False, 'source_before': source_before, 'artifact_hashes_before': artifact_before,
        'artifact_inputs': {name: str(path) for name, path in artifact_inputs.items()},
        'artifact_execution': 'private byte-identical pinned copies; concurrent builds cannot replace running benchmark binaries',
        'environment': {'platform': platform.platform(), 'machine': platform.machine(), 'python': platform.python_version(),
                        'cpu_count': os.cpu_count(), 'runner_exclusive': False},
        'limits': {'sqlite_cache_bytes': 16 * 1024 * 1024, 'incremental_peak_rss_bytes': 64 * 1024 * 1024,
                   'p95_seconds': 1, 'first_page_items': 100, 'measurements_per_scope_per_dataset': args.measurements},
        'protocol_dataset_sizes': args.tasks == [100_000, 1_000_000], 'datasets': []}
    atomic_json(args.output / 'report.json', report)
    credentials = args.output / 'pki'
    identity = generate_pki(credentials, ['DNS:localhost', 'IP:127.0.0.1'])
    atomic_json(args.output / 'pki-index.json', identity)
    for count in args.tasks:
        result = measure_dataset(args, args.output / f'tasks-{count}', count, credentials)
        report['datasets'].append(result)
        atomic_json(args.output / 'report.json', report)
        print(json.dumps({'tasks': count, 'status': result['status'], 'latency': result.get('latency'),
                          'incremental_peak_rss_bytes': result.get('incremental_peak_rss_bytes')}), flush=True)
        if result.get('interrupted'):
            break
    report['source_after'] = source_identity(args.source_root)
    report['artifact_hashes_after'] = {name: digest(path) for name, path in [('coordinator', args.binary), ('native_probe', args.native_probe)]}
    report['source_unchanged'] = report['source_before']['tree_sha256'] == report['source_after']['tree_sha256']
    report['artifacts_unchanged'] = artifact_before == report['artifact_hashes_after']
    report['artifact_inputs_changed'] = artifact_before != {name: digest(path) for name, path in artifact_inputs.items()}
    report['status'] = 'PASS' if len(report['datasets']) == len(args.tasks) and all(item['status'] == 'PASS' for item in report['datasets']) else 'FAIL'
    report['finished_at'] = datetime.now(timezone.utc).isoformat()
    atomic_json(args.output / 'report.json', report)
    print(args.output / 'report.json')
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
