#!/usr/bin/env python3
"""Run installed SDKs through native workers, durable coordinator state and mTLS.

The npm directory must be the unmodified local install retained by
verify-packages.ts --keep-local. The Python SDK is installed from the original
candidate wheel in a fresh environment outside the checkout. No SDK source path
is added to either runtime's module search path.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(argv, cwd, env, timeout=90):
    result = subprocess.run([str(item) for item in argv], cwd=cwd, env=env,
                            text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{argv[0]} failed ({result.returncode})\n{result.stdout}\n{result.stderr}')
    return result.stdout


def installed_run(args, env):
    import cedegrid
    from cedegrid import Client, command_task, parse_json, stringify_json
    if not Path(cedegrid.__file__).resolve().is_relative_to(Path(sys.prefix).resolve()):
        raise RuntimeError('Python SDK did not resolve from the isolated installed environment')
    # These are fixture helpers only; the checkout's Python SDK is never added.
    source = Path(__file__).resolve().parents[1]
    sys.path.insert(0, str(source / 'tools'))
    from make_test_pki import generate
    from runtime_config import atomic_runtime_config

    candidate = args.candidate.resolve()
    installation = args.npm_install.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, mode=0o700, exist_ok=False)
    target = 'darwin-arm64' if sys.platform == 'darwin' and os.uname().machine == 'arm64' else args.target
    if not target:
        raise RuntimeError('--target is required for this native host')
    binary = installation / 'node_modules' / ('cedegrid-' + target) / 'bin' / ('cedegrid.exe' if os.name == 'nt' else 'cedegrid')
    manifest_path = candidate / 'manifest.json'
    if not manifest_path.exists(): manifest_path = candidate / 'manifest.partial.json'
    manifest = json.loads(manifest_path.read_text())
    expected = manifest['native_inputs'][target]['sha256']
    if digest(binary) != expected:
        raise RuntimeError('installed native binary is not the original candidate binary')
    command([args.node, '-e', 'console.log(require.resolve("cedegrid"))'], installation, env)
    js_worker = installation / 'numeric-interop.cjs'
    py_worker = Path(sys.prefix).parent / 'numeric_interop.py'
    shutil.copyfile(source / 'npm/build/tests/numeric-interop.js', js_worker)
    shutil.copyfile(source / 'python/tests/numeric_interop.py', py_worker)
    c_worker = output / 'counter'
    command(['cc', '-std=c11', '-O2', '-Wall', '-Wextra', '-Werror', source / 'examples/counter.c', '-o', c_worker], installation, env)
    node_id = 'package-interop-node'
    pki = generate(output / 'pki', ['DNS:localhost', 'IP:127.0.0.1'], [node_id])

    def tls(name):
        return {'ca_cert': str(output / 'pki/ca.pem'),
                'certificate': str(output / ('pki/' + name + '.pem')),
                'private_key': str(output / ('pki/' + name + '.key'))}

    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    endpoint = f'https://127.0.0.1:{port}'
    atomic_runtime_config(output / 'coordinator.toml', {
        'listen': f'127.0.0.1:{port}', 'state_dir': str(output / 'coordinator-state'),
        'tls': tls('server'), 'clients': pki['clients'], 'lease_ms': 10000,
        'telemetry_ttl_ms': 3000, 'retry_limit': 1,
        'max_artifact_bytes': 4 * 1024**2, 'artifact_quota_bytes': 32 * 1024**2}, 'coordinator')
    atomic_runtime_config(output / 'operator.toml', {'endpoint': endpoint, 'tls': tls('operator')}, 'client')
    atomic_runtime_config(output / 'agent.toml', {
        'coordinator_url': endpoint, 'tls': tls('node-0'),
        'capacity': {'cpu_millicores': 1000, 'ram_mib': 512, 'gpu_memory_mib': {}},
        'max_workers': 1, 'max_transfer_bytes_per_second': 10 * 1024**2,
        'max_spool_bytes': 64 * 1024**2, 'max_runtime_seconds': 180}, 'agent')
    atomic_runtime_config(output / 'node.toml', {
        'schema_version': 2, 'node_id': node_id, 'node_mode': 'guaranteed',
        'state_dir': str(output / 'agent-state'),
        'execution': {'enabled': True, 'prepare_timeout_ms': 10000, 'admission_timeout_ms': 30000},
        'monitor': {'interval_ms': 500}, 'cpu': {'reserve_physical_cores': 1, 'nice': 10},
        'ram': {'reserve_mib': 1024, 'reserve_percent': 10},
        'gpu': {'scale_up_cooldown_ms': 0},
        'lifecycle': {'drain_timeout_ms': 3000, 'term_grace_ms': 2000,
                      'heartbeat_interval_ms': 1000, 'allocation_lease_ms': 10000},
        'cgroup': {'enabled': False}}, 'node')
    services = {}
    cleanup = []
    client = Client.from_config(output / 'operator.toml')
    report = {'schema_version': 1, 'status': 'RUNNING', 'native_sha256': expected,
              'npm_sha256': digest(candidate / 'npm/cedegrid-0.2.0.tgz'),
              'python_wheel_sha256': digest(candidate / 'pypi/cedegrid-0.2.0-py3-none-any.whl'),
              'python_version': sys.version.split()[0], 'node_version': command([args.node, '--version'], installation, env).strip(),
              'worker_resources': {'cpu_millicores': 1000, 'ram_mib': 512, 'gpu_memory_mib': {}},
              'registry_publication': 'NOT_RUN', 'cleanup': cleanup}
    last_status = None

    def save():
        (output / 'report.json').write_text(stringify_json(report) + '\n', encoding='utf-8')

    def start(name, argv):
        stream = (output / (name + '.log')).open('ab')
        services[name] = (subprocess.Popen([str(item) for item in argv], cwd=installation,
                                          env=env, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream), stream)

    def stop(name):
        pair = services.pop(name, None)
        if pair is None:
            return
        child, stream = pair
        forced = False
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
            forced = True
        stream.close()
        cleanup.append({'service': name, 'exit_code': child.returncode, 'forced': forced, 'reaped': True})

    def status_until(predicate, seconds=40):
        nonlocal last_status
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            for name, (child, _) in services.items():
                if child.poll() is not None:
                    raise RuntimeError(name + ' exited; inspect its retained log')
            try:
                last_status = client.status()
                if predicate(last_status):
                    return last_status
            except OSError:
                pass
            except cedegrid.RemoteError as error:
                if error.code != 'ERR_CEDEGRID_TRANSPORT':
                    raise
            time.sleep(.2)
        raise TimeoutError('native integration deadline exceeded')

    coordinator_argv = [binary, 'coordinator', '--deployment', output / 'coordinator.toml']
    try:
        start('coordinator', coordinator_argv)
        status_until(lambda _: True, 15)
        start('agent', [binary, '--config', output / 'node.toml', 'agent', '--deployment', output / 'agent.toml'])
        status_until(lambda status: bool(status.get('nodes')))
        client.put_pool('package-interop', [node_id], max_workers=1, min_workers=1)
        workers = [('python', [sys.executable, '-I', py_worker, 'worker']),
                   ('typescript', [args.node, js_worker, 'worker']),
                   ('c', [c_worker, '1000000'])]
        tasks = [command_task('package-' + name, [str(item) for item in argv], str(installation),
                              single_process=True, no_escape=True, max_attempts=1)
                 for name, argv in workers]
        client.submit('package-interop', 'package-interop', tasks)

        def all_completed(status):
            rows = {row['task_id']: row for row in status.get('tasks', [])}
            if any(row['status'] in ('needs_reconciliation', 'failed') for row in rows.values()):
                raise RuntimeError('a worker failed or requires reconciliation')
            allocations = status.get('allocations', [])
            return (all(rows.get(task['task_id'], {}).get('status') == 'completed' for task in tasks)
                    and len(allocations) == len(tasks) and all(row['phase'] == 'released' for row in allocations))

        status_until(all_completed, 120)
        published = {name: client.result('package-' + name)['artifacts'][0]
                     for name in ('python', 'typescript')}
        inputs = [{'name': name, **artifact} for name, artifact in published.items()]
        inputs.append({'name': 'python-alias', **published['python']})
        input_code = '''from pathlib import Path
from cedegrid import WorkerContext,parse_json,sha256_file
worker=WorkerContext.from_env()
assert len(worker.inputs)==3
assert len({entry['path'] for entry in worker.inputs})==2
for entry in worker.inputs:
    path=Path(entry['path'])
    assert path.stat().st_size==entry['size']
    assert sha256_file(path)==entry['sha256']
    assert parse_json(path.read_text(encoding='utf-8').splitlines()[0])['integer53']==2**53+1
metadata={'verified_inputs':3,'unique_files':2}
worker.checkpoint(metadata)
worker.complete(metadata)
'''
        input_task = command_task('package-inputs', [sys.executable, '-I', '-c', input_code],
                                  str(installation), single_process=True, no_escape=True,
                                  max_attempts=1, input_artifacts=inputs)
        tasks.append(input_task)
        client.submit('package-input-interop', 'package-interop', [input_task])
        status_until(all_completed, 60)
        report['native_input_materialization'] = {'status': 'PASS', 'named_inputs': 3, 'unique_files': 2}
        report['released_allocations'] = last_status['allocations']
        report['accepted_results'] = {task['task_id']: client.result(task['task_id']) for task in tasks}
        result = report['accepted_results']['package-c']['result']['metadata']
        if result != {'completed': 1000000, 'sum': 500000500000}:
            raise RuntimeError('native C example result differs from its finite counter calculation')
        client.drain_node(node_id)
        stop('agent')
        stop('coordinator')
        databases = list((output / 'coordinator-state').glob('*.sqlite3'))
        if len(databases) != 1:
            raise RuntimeError('expected one persistent coordinator database')
        with sqlite3.connect(databases[0].as_uri() + '?mode=ro', uri=True) as database:
            checkpoints = {task_id: parse_json(payload) for task_id, payload in
                           database.execute('SELECT task_id,submission_json FROM checkpoints')}
        if set(checkpoints) != {task['task_id'] for task in tasks}:
            raise RuntimeError('a native worker checkpoint was not persisted')
        report['persisted_checkpoints'] = checkpoints
        start('coordinator-restarted', coordinator_argv)
        status_until(lambda _: True, 15)
        receivers = []
        for argv in ([args.node, js_worker, 'verify', output / 'operator.toml', 'package-python'],
                     [sys.executable, '-I', py_worker, 'verify', output / 'operator.toml', 'package-typescript']):
            receivers.append(parse_json(command(argv, installation, env)))
        report.update(status='PASS', coordinator_restarted_before_receivers=True,
                      cross_language_receivers=receivers)
    except BaseException as error:
        report.update(status='FAIL', error=type(error).__name__ + ': ' + str(error), last_status=last_status)
        raise
    finally:
        if 'agent' in services:
            try:
                client.cancel('package-interop')
                client.cancel('package-input-interop')
                client.drain_node(node_id)
                status_until(lambda status: all(row['phase'] == 'released' for row in status.get('allocations', [])), 20)
            except Exception as error:
                report['release_cleanup_error'] = str(error)
        for name in list(services):
            stop(name)
        if any(item['forced'] for item in cleanup):
            report['status'] = 'FAIL'
        save()
    if report['status'] != 'PASS':
        raise RuntimeError('integration cleanup did not complete normally')
    (candidate / 'package-interop-evidence.json').write_text(stringify_json(report) + '\n', encoding='utf-8')
    print(json.dumps({'status': report['status'], 'evidence': str(output / 'report.json')}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidate', type=Path)
    parser.add_argument('--npm-install', type=Path, required=True)
    parser.add_argument('--node', type=Path, required=True)
    parser.add_argument('--python', default=sys.executable)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--target')
    parser.add_argument('--installed', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    env = dict(os.environ)
    env.pop('PYTHONPATH', None)
    env.pop('NODE_PATH', None)
    env.setdefault('UV_CACHE_DIR', '/private/tmp/cedegrid-uv-cache')
    if args.installed:
        installed_run(args, env)
        return
    with tempfile.TemporaryDirectory(prefix='cedegrid-installed-interop-') as directory:
        root = Path(directory)
        environment = root / 'env'
        command(['uv', 'venv', '--python', args.python, environment], root, env)
        python = environment / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
        command(['uv', 'pip', 'install', '--python', python,
                 args.candidate.resolve() / 'pypi/cedegrid-0.2.0-py3-none-any.whl'], root, env)
        invocation = [python, '-I', Path(__file__).resolve(), args.candidate.resolve(), '--installed',
                      '--npm-install', args.npm_install.resolve(), '--node', args.node.resolve(),
                      '--output', args.output.resolve()]
        if args.target:
            invocation += ['--target', args.target]
        print(command(invocation, root, env, 240).strip())


if __name__ == '__main__':
    main()
