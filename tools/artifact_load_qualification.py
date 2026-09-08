#!/usr/bin/env python3
"""Exercise artifact/control concurrency against isolated real native services.

Three native agents execute bounded SDK workers and own all measured leases.
Authenticated test clients generate artifact traffic for those exact attempts.
This local-kernel check is separate from physical-host and GPU qualification.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import json
import math
import multiprocessing
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
from status_benchmark import digest, source_identity
from validation_runtime import atomic_json, inside_home

MIB = 1024 * 1024
NODES = ['load-0', 'load-1', 'load-2']
WORKER = r'''
from cedegrid import WorkerContext
from pathlib import Path
import json, os, time
c = WorkerContext.from_env()
root = Path(os.environ['TEST_GATE_DIR'])
node = os.environ['TEST_NODE']
temporary = root / (node + '.ready.tmp')
temporary.write_text(json.dumps(c.identity))
temporary.replace(root / (node + '.ready.json'))
deadline = time.monotonic() + 240
while not (root / 'finish.json').exists():
    if c.draining(): raise RuntimeError('unexpected drain during artifact/control load')
    if time.monotonic() >= deadline: raise RuntimeError('bounded artifact worker expired')
    time.sleep(.02)
expected = json.loads((root / 'finish.json').read_text())[node]
c.complete({'gate': 'artifact-control', 'node': node, 'artifact': expected,
            'observations': [18.400785964971874, 31.859559996519238]})
'''


class OwnedService:
    def __init__(self, argv, directory, name, env=None):
        self.stdout_path = directory / (name + '.stdout.log')
        self.stdout = self.stdout_path.open('xb')
        self.stderr = (directory / (name + '.stderr.log')).open('xb')
        try:
            self.process = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                stdout=self.stdout, stderr=self.stderr, cwd=directory, env=env,
                start_new_session=True)
        except BaseException:
            self.stdout.close()
            self.stderr.close()
            raise

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
        forced = False
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            forced = True
            self.process.kill()
            self.process.wait(timeout=3)
        self.stdout.close()
        self.stderr.close()
        return {'pid': self.process.pid, 'owned_child_reaped': True,
                'exit_code': self.process.returncode, 'forced': forced}


def wait_for(callback, deadline, description):
    last = None
    while time.monotonic() < deadline:
        try:
            result = callback()
            if result:
                return result
        except (OSError, ValueError) as error:
            last = str(error)
        time.sleep(.05)
    raise TimeoutError(description + (': ' + last if last else ''))


def make_artifact(path, repetition, node, size):
    # Distinct deterministic content; generation/hashing precedes measurement.
    block = hashlib.shake_256(f'cedegrid-artifact-gate/{repetition}/{node}'.encode()).digest(MIB)
    value = hashlib.sha256()
    with path.open('xb') as stream:
        for offset in range(0, size, len(block)):
            chunk = block[:min(len(block), size - offset)]
            stream.write(chunk)
            value.update(chunk)
        stream.flush()
        os.fsync(stream.fileno())
    return {'sha256': value.hexdigest(), 'size': size}


class ObservedClient:
    def __init__(self, client, records, lane):
        self.client, self.records, self.lane = client, records, lane

    def request(self, op, **payload):
        started = time.monotonic()
        row = {'operation': op, 'lane': self.lane, 'scheduled_monotonic': started,
               'started_monotonic': started, 'retry': 0, 'status': 'RUNNING'}
        self.records.append(row)
        try:
            value = self.client.request(op, **payload)
            row['status'] = 'PASS'
            return value
        except Exception as error:
            row['status'] = 'FAIL'
            row['error'] = type(error).__name__ + ': ' + str(error)
            raise
        finally:
            row['finished_monotonic'] = time.monotonic()
            row['elapsed_ms'] = (row['finished_monotonic'] - started) * 1000


def artifact_lane(node, client, identity, artifact, source, output, barrier, records, deadline):
    observed = ObservedClient(client, records, node)
    upload = observed.request('begin_upload', assignment_id=identity['assignment_id'],
                              generation=identity['generation'], artifact=artifact)
    if upload['offset'] != 0:
        raise RuntimeError('fresh repetition unexpectedly resumed an upload')
    with source.open('rb') as stream:
        for offset in range(0, artifact['size'], MIB):
            if time.monotonic() >= deadline:
                raise TimeoutError('repetition bound exceeded during upload')
            reply = observed.request('upload_chunk', upload_id=upload['upload_id'],
                                     offset=offset, data_hex=stream.read(MIB).hex())
            if reply['offset'] != min(offset + MIB, artifact['size']):
                raise RuntimeError('upload offset mismatch')
    barrier.wait(timeout=max(.001, deadline - time.monotonic()))
    committed = observed.request('commit_upload', upload_id=upload['upload_id'])['artifact']
    if committed != artifact:
        raise RuntimeError('committed artifact identity changed')
    submission = {key: identity[key] for key in ('task_id', 'assignment_id', 'generation')}
    submission.update(result={'kind': 'artifact-load-checkpoint', 'checkpoint_sequence': 1, 'node': node,
                              'artifact': artifact}, artifacts=[artifact])
    receipt = observed.request('publish_checkpoint', submission=submission)
    atomic_json(output / (node + '.checkpoint-receipt.json'), receipt)
    # Read via the same authenticated lane and pacer, retaining every operation.
    target = output / (node + '.download')
    value = hashlib.sha256()
    offset = 0
    with target.open('xb') as stream:
        while offset < artifact['size']:
            if time.monotonic() >= deadline:
                raise TimeoutError('repetition bound exceeded during download')
            reply = observed.request('read_artifact', sha256=artifact['sha256'], offset=offset,
                                     max_bytes=min(MIB, artifact['size'] - offset))
            chunk = bytes.fromhex(reply['data_hex'])
            if not chunk or len(chunk) > min(MIB, artifact['size'] - offset):
                raise RuntimeError('invalid whole-artifact download chunk')
            stream.write(chunk)
            value.update(chunk)
            offset += len(chunk)
        stream.flush()
        os.fsync(stream.fileno())
    if value.hexdigest() != artifact['sha256']:
        raise RuntimeError('whole-artifact download digest mismatch')
    return {'node': node, 'artifact': artifact, 'whole_download_verified': True,
            'checkpoint_receipt': receipt, 'assignment': identity}


def status_attempt(client, row):
    scheduled = row['scheduled_monotonic']
    row['started_monotonic'] = time.monotonic()
    try:
        # Evidence adapter uses the installed SDK's exact transport and
        # pacer, with the deadline anchored before any scheduler delay.
        from cedegrid.transport import exchange, remaining
        deadline = scheduled + 10
        if not client._request_lock.acquire(timeout=remaining(deadline)):
            raise TimeoutError('status deadline expired in SDK client queue')
        try:
            response = exchange(client, 'status_page', {'collection': 'allocations',
                'limit': 100, 'cursor': None, 'job_id': None, 'node_id': None, 'pool_id': None}, deadline)
        finally:
            client._request_lock.release()
        if len(response['items']) != 3 or any(item.get('phase') not in ('running', 'authorized')
                                              for item in response['items']):
            raise RuntimeError('unexpected allocation inventory/phase during measured load')
        row['status'] = 'PASS'
    except Exception as error:
        row['status'] = 'FAIL'
        row['error'] = type(error).__name__ + ': ' + str(error)
    row['finished_monotonic'] = time.monotonic()
    row['elapsed_ms'] = (row['finished_monotonic'] - scheduled) * 1000


def status_schedule(client_args, stop, start, output):
    from cedegrid import Client
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    client = Client(**client_args)
    records = []
    # The scheduler has its own process, so artifact hex serialization cannot
    # monopolize its Python interpreter. Slow requests retain their scheduled
    # deadlines in the executor/client queue; no slot is dropped or reset.
    with ThreadPoolExecutor(max_workers=8, thread_name_prefix='status-request') as executor:
        slot = 0
        while not stop.is_set():
            scheduled = start + slot / 6
            if stop.wait(max(0, scheduled - time.monotonic())):
                break
            enqueued = time.monotonic()
            row = {'operation': 'status', 'scheduled_monotonic': scheduled,
                   'enqueued_monotonic': enqueued,
                   'missed_schedule_slots': int((enqueued - scheduled) * 6),
                   'retry': 0, 'status': 'RUNNING'}
            records.append(row)
            executor.submit(status_attempt, client, row)
            slot += 1
    atomic_json(output, records)


def metrics(rows, operation):
    selected = [row for row in rows if row['operation'] == operation]
    elapsed = sorted(row['elapsed_ms'] for row in selected)
    return {'samples': len(selected), 'failures': sum(row['status'] != 'PASS' for row in selected),
        'missed_schedule_slots': sum(row.get('missed_schedule_slots', 0) for row in selected),
        'p99_ms': elapsed[math.ceil(.99 * len(elapsed)) - 1] if elapsed else None,
        'max_ms': max(elapsed) if elapsed else None, 'method': 'nearest rank; scheduled enqueue to response decode'}


def control_events(services, begin, end):
    rows = []
    for node, service in services.items():
        for line in service.stdout_path.read_text().splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get('event') != 'control_rpc':
                continue
            # Native evidence supplies a common OS monotonic timestamp. A
            # missing scheduled timestamp never substitutes request start.
            if 'scheduled_due_monotonic_ms' not in event:
                raise RuntimeError('native control evidence omits scheduled enqueue timestamp')
            if event.get('deadline_ms') != 10000:
                raise RuntimeError('native control evidence has the wrong absolute deadline')
            scheduled = event['scheduled_due_monotonic_ms'] / 1000
            if not begin <= scheduled <= end:
                continue
            rows.append({'operation': event['operation'], 'node': node,
                'scheduled_monotonic': scheduled, 'elapsed_ms': event['elapsed_ms'],
                'missed_schedule_slots': event.get('missed_schedule_slots', 0),
                'status': 'PASS' if event['success'] else 'FAIL', 'native': event})
    return rows


def repetition(args, output, number):
    from cedegrid import Client, command_task
    output.mkdir(mode=0o700)
    gate = output / 'gate'
    gate.mkdir(mode=0o700)
    artifacts = {node: make_artifact(output / (node + '.source'), number, node, args.size_mib * MIB)
                 for node in NODES}
    info = generate_pki(output / 'pki', ['DNS:localhost', 'IP:127.0.0.1'], NODES)
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    endpoint = f'https://127.0.0.1:{port}'
    def tls(name):
        return {'ca_cert': str(output / 'pki/ca.pem'), 'certificate': str(output / ('pki/' + name + '.pem')),
                'private_key': str(output / ('pki/' + name + '.key'))}
    def client(name, rate=10 * MIB):
        identity = tls(name)
        return Client(endpoint, ca=identity['ca_cert'], certificate=identity['certificate'],
                      private_key=identity['private_key'], timeout=10,
                      max_transfer_bytes_per_second=rate)
    atomic_runtime_config(output / 'coordinator.toml', {'listen': f'127.0.0.1:{port}',
        'state_dir': str(output / 'coordinator'), 'tls': tls('server'), 'clients': info['clients'],
        'lease_ms': 10000, 'max_artifact_bytes': 256 * MIB, 'artifact_quota_bytes': 4 * 1024 * MIB}, 'coordinator')
    result = {'number': number, 'status': 'RUNNING', 'artifacts': artifacts, 'raw_control': [],
              'raw_artifact': [], 'fresh_coordinator_state': True, 'cache_dropping': False}
    atomic_json(output / 'report.json', result)
    server = None
    agents = {}
    process_context = multiprocessing.get_context('spawn')
    stop = process_context.Event()
    scheduler = None
    operator = client('operator')
    begin = end = native_begin = native_end = None
    deadline = time.monotonic() + 240
    try:
        server = OwnedService([str(args.binary), 'coordinator', '--deployment', str(output / 'coordinator.toml')], output, 'coordinator')
        def ready():
            try:
                return operator.status_page('tasks')
            except Exception:
                return None
        wait_for(ready, min(deadline, time.monotonic() + 10), 'coordinator readiness timeout')
        for index, node in enumerate(NODES):
            atomic_runtime_config(output / (node + '.toml'), {'node_id': node, 'node_mode': 'guaranteed',
                'state_dir': str(output / (node + '-state')), 'execution': {'enabled': True},
                'cpu': {'reserve_physical_cores': 0}, 'ram': {'reserve_mib': 0, 'reserve_percent': 0},
                'gpu': {'scale_up_cooldown_ms': 0},
                'monitor': {'interval_ms': 500}, 'lifecycle': {'heartbeat_interval_ms': 500,
                    'allocation_lease_ms': 10000, 'drain_timeout_ms': 1000, 'term_grace_ms': 1000}}, 'node')
            atomic_runtime_config(output / (node + '-agent.toml'), {'coordinator_url': endpoint,
                'tls': tls('node-' + str(index)), 'capacity': {'cpu_millicores': 1000, 'ram_mib': 512,
                    'gpu_memory_mib': {}}, 'max_workers': 1, 'max_runtime_seconds': 260,
                'max_spool_bytes': 1024 * MIB, 'max_transfer_bytes_per_second': 10 * MIB}, 'agent')
            operator.put_pool(node + '-pool', [node], max_workers=1, min_workers=1)
            env = dict(os.environ, CEDEGRID_RPC_EVIDENCE='1', TMPDIR=str(output), PYTHONDONTWRITEBYTECODE='1')
            agents[node] = OwnedService([str(args.binary), '--config', str(output / (node + '.toml')),
                'agent', '--deployment', str(output / (node + '-agent.toml'))], output, node, env)
            task = command_task(node + '-task', [sys.executable, '-c', WORKER], str(output),
                env={'TEST_GATE_DIR': str(gate), 'TEST_NODE': node, 'PYTHONDONTWRITEBYTECODE': '1',
                     **({'PYTHONPATH': str(args.source_root / 'python')} if args.source_sdk else {})},
                replay_safe=True, single_process=True, no_escape=True, max_attempts=1)
            operator.submit(node + '-job', node + '-pool', [task])
        identities = {node: wait_for(lambda node=node: json.loads((gate / (node + '.ready.json')).read_text()),
            min(deadline, time.monotonic() + 20), node + ' workload readiness timeout') for node in NODES}
        result['workload_identities'] = identities
        begin = time.monotonic() + 1
        result['loaded_begin_monotonic'] = begin
        operator_tls = tls('operator')
        scheduler = process_context.Process(target=status_schedule, args=({'endpoint': endpoint,
            'ca': operator_tls['ca_cert'], 'certificate': operator_tls['certificate'],
            'private_key': operator_tls['private_key'], 'timeout': 10,
            'max_transfer_bytes_per_second': 10 * MIB}, stop, begin, output / 'raw-status.json'),
            name='fixed-status-schedule')
        scheduler.start()
        time.sleep(max(0, begin - time.monotonic()))
        native_begin = time.clock_gettime(time.CLOCK_MONOTONIC)
        result['loaded_begin_native_clock_monotonic'] = native_begin
        barrier = threading.Barrier(3)
        with ThreadPoolExecutor(max_workers=3, thread_name_prefix='artifact-lane') as executor:
            futures = [executor.submit(artifact_lane, node, client('node-' + str(index), 9 * MIB),
                identities[node], artifacts[node], output / (node + '.source'), output,
                barrier, result['raw_artifact'], deadline) for index, node in enumerate(NODES)]
            result['lanes'] = [future.result(timeout=max(.001, deadline - time.monotonic())) for future in futures]
        end = time.monotonic()
        native_end = time.clock_gettime(time.CLOCK_MONOTONIC)
        result['loaded_end_monotonic'] = end
        result['loaded_end_native_clock_monotonic'] = native_end
        stop.set()
        scheduler.join(timeout=11)
        if scheduler.is_alive():
            raise RuntimeError('status scheduler did not respect absolute deadline')
        atomic_json(gate / 'finish.json', artifacts)
        def finished():
            submissions = {node: operator.result(node + '-task') for node in NODES}
            return submissions if all(submissions.values()) else None
        result['final_results'] = wait_for(finished, deadline, 'native final result acceptance timeout')
        for node, submission in result['final_results'].items():
            if submission['generation'] != 1 or submission['assignment_id'] != identities[node]['assignment_id']:
                raise RuntimeError('unintended replacement attempt')
            if submission['result']['metadata']['artifact'] != artifacts[node]:
                raise RuntimeError('native final result metadata mismatch')
    except BaseException as error:
        result['error'] = type(error).__name__ + ': ' + str(error)
        result['interrupted'] = isinstance(error, KeyboardInterrupt)
    finally:
        cleanup_started = time.monotonic()
        cleanup_deadline = cleanup_started + 30
        end = end or time.monotonic()
        native_end = native_end or time.clock_gettime(time.CLOCK_MONOTONIC)
        stop.set()
        if scheduler:
            scheduler.join(timeout=11)
            if scheduler.is_alive():
                scheduler.terminate()
                scheduler.join(timeout=2)
                if scheduler.is_alive():
                    scheduler.kill()
                    scheduler.join(timeout=2)
                result['scheduler_error'] = 'owned status process exceeded its request deadlines'
            if (output / 'raw-status.json').is_file():
                result['raw_control'].extend(json.loads((output / 'raw-status.json').read_text()))
            else:
                result['scheduler_error'] = 'status process produced no complete raw evidence'
        try:
            # Independent short cleanup clients cannot inherit a saturated
            # transfer pacer. Only the three known test job IDs are cancelled.
            def cancel_owned(node):
                cleanup_client = client('operator')
                cleanup_client.timeout = 1.5
                return cleanup_client.cancel(node + '-job')
            with ThreadPoolExecutor(max_workers=3) as executor:
                for future in [executor.submit(cancel_owned, node) for node in agents]:
                    future.result(timeout=2)
            cleanup_client = client('operator')
            cleanup_client.timeout = 1.5
            def released():
                allocations = cleanup_client.status_page('allocations')['items']
                return all(item['phase'] == 'released' for item in allocations)
            result['all_allocations_released'] = bool(wait_for(released,
                min(cleanup_deadline - 12, time.monotonic() + 12), 'release cleanup timeout'))
        except Exception as error:
            result['cleanup_error'] = type(error).__name__ + ': ' + str(error)
        with ThreadPoolExecutor(max_workers=3) as executor:
            cleanup = {node: executor.submit(service.stop) for node, service in agents.items()}
            result['cleanup'] = {node: future.result(timeout=7) for node, future in cleanup.items()}
        if server:
            result['cleanup']['coordinator'] = server.stop()
        result['cleanup_elapsed_seconds'] = time.monotonic() - cleanup_started
        if result['cleanup_elapsed_seconds'] > 30:
            result['cleanup_error'] = 'cleanup exceeded its independent 30-second bound'
        if native_begin is not None:
            try:
                result['raw_control'].extend(control_events(agents, native_begin, native_end))
            except Exception as error:
                result['evidence_error'] = str(error)
    result['metrics'] = {operation: metrics(result['raw_control'], operation) for operation in ('heartbeat', 'renew', 'status')}
    result['native_samples_by_node'] = {node: {operation: sum(row.get('node') == node and
        row['operation'] == operation for row in result['raw_control']) for operation in ('heartbeat', 'renew')}
        for node in NODES}
    qualified = all(value['samples'] >= 300 and value['failures'] == 0 and
        value['missed_schedule_slots'] == 0 and value['p99_ms'] < 2500 for value in result['metrics'].values()) and all(
            count >= 100 for per_node in result['native_samples_by_node'].values() for count in per_node.values())
    result['status'] = 'PASS' if qualified and result.get('all_allocations_released') and not any(
        key in result for key in ('error', 'cleanup_error', 'evidence_error', 'scheduler_error')) else 'FAIL'
    atomic_json(output / 'report.json', result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--source-sdk', action='store_true', help='development only; final gate requires installed SDK')
    parser.add_argument('--size-mib', type=int, default=256, help='smaller smoke runs never qualify the gate')
    parser.add_argument('--repetitions', type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.size_mib <= 256 or not 1 <= args.repetitions <= 3:
        parser.error('size must be 1..256 MiB and repetitions 1..3')
    args.source_root = args.source_root.resolve(strict=True)
    args.binary = args.binary.resolve(strict=True)
    args.output = inside_home(args.output)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    original = args.binary
    binary_hash = digest(original)
    args.binary = args.output / 'cedegrid'
    shutil.copyfile(original, args.binary)
    args.binary.chmod(0o700)
    if digest(args.binary) != binary_hash or digest(original) != binary_hash:
        raise RuntimeError('native candidate bytes changed while pinning')
    report = {'schema_version': 1, 'status': 'RUNNING', 'started_at': datetime.now(timezone.utc).isoformat(),
        'scope': 'three native agents and SDK workers on one local kernel; authenticated artifact test lanes',
        'final_candidate_qualification': False, 'source_before': source_identity(args.source_root),
        'native_sha256': binary_hash, 'native_input': str(original), 'source_sdk': args.source_sdk,
        'environment': {'platform': platform.platform(), 'machine': platform.machine(), 'python': sys.version},
        'limits': {'lease_ms': 10000, 'deadline_ms': 10000, 'heartbeat_interval_ms': 500,
            'renew_interval_ms': 500, 'status_hz': 6, 'chunk_bytes': MIB, 'lane_wire_budget_bytes_per_second': 10 * MIB,
            'artifact_lane_budget_bytes_per_second': 9 * MIB, 'native_control_budget_bytes_per_second': MIB,
            'required_samples_per_operation': 300, 'p99_limit_ms': 2500, 'repetition_seconds': 240,
            'cleanup_seconds': 30}, 'repetitions': []}
    atomic_json(args.output / 'report.json', report)
    for number in range(args.repetitions):
        result = repetition(args, args.output / ('repetition-' + str(number + 1)), number)
        report['repetitions'].append(result)
        atomic_json(args.output / 'report.json', report)
        print(json.dumps({'repetition': number + 1, 'status': result['status'], 'metrics': result['metrics'],
                          'error': result.get('error'), 'evidence_error': result.get('evidence_error')}), flush=True)
        if result.get('interrupted'):
            break
    report['source_after'] = source_identity(args.source_root)
    report['source_unchanged'] = report['source_before']['tree_sha256'] == report['source_after']['tree_sha256']
    report['native_unchanged'] = digest(args.binary) == binary_hash
    hashes = [artifact['sha256'] for item in report['repetitions'] for artifact in item['artifacts'].values()]
    report['distinct_artifacts'] = len(hashes) == len(set(hashes))
    report['full_protocol'] = args.size_mib == 256 and args.repetitions == 3
    report['status'] = 'PASS' if report['full_protocol'] and report['distinct_artifacts'] and all(
        item['status'] == 'PASS' for item in report['repetitions']) else 'FAIL'
    report['finished_at'] = datetime.now(timezone.utc).isoformat()
    atomic_json(args.output / 'report.json', report)
    print(args.output / 'report.json')
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
