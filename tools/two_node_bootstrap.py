#!/usr/bin/env python3
"""Prepare private two-node validation; own only bounded anchor services.

Transport and burst service lifetime remain the invoking operator's responsibility.
Preparation never starts a workload. Runtime never adopts persisted PIDs.
"""
import argparse
import importlib.util
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import subprocess
import sys
import time
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from resmgr import Client, sha256_file
from make_test_pki import generate as generate_pki
from soak import AllocationLedger
from validation_runtime import OwnedProcess, atomic_json, home_executable, inside_home, local_guard

SERVICE_SECONDS = 1400
WORK_SECONDS = 1200
DEFAULT_DOCKER_ROOT = '/home/resmgr/validation'
STORAGE_PROFILES = ('wal_full', 'delete_extra')


def safe_id(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,63}', value):
        raise ValueError('explicit safe run/node identifier required')
    return value


def required(path):
    path = inside_home(path)
    if not path.is_file():
        raise ValueError('required input is absent: ' + str(path))
    return path


def remote_path(root, *parts, home='/home/resmgr'):
    """Lexical remote bounds only; the burst owner must verify canonical paths."""
    values = [str(home), str(root), *map(str, parts)]
    if any(not value or any(ord(char) < 32 for char in value) for value in values):
        raise ValueError('remote home and paths must be explicit without control characters')
    home, root = PurePosixPath(home), PurePosixPath(root)
    if not home.is_absolute() or str(home) in ('/', '//') or '..' in home.parts:
        raise ValueError('explicit absolute remote runtime home required')
    if not root.is_absolute() or '..' in root.parts or not root.is_relative_to(home):
        raise ValueError('remote validation paths must remain inside the declared runtime home')
    if any(PurePosixPath(part).is_absolute() or '..' in PurePosixPath(part).parts for part in parts):
        raise ValueError('remote path traversal refused')
    result = root.joinpath(*parts)
    return str(result)


def cpu_ids(value):
    cpus = [int(item) for item in value.split(',')]
    if not 2 <= len(cpus) <= 6 or len(set(cpus)) != len(cpus) or min(cpus) < 0:
        raise ValueError('provide two to six distinct permitted logical CPUs')
    return cpus


def cpu_list(value):
    cpus = cpu_ids(value)
    if hasattr(os, 'sched_getaffinity') and not set(cpus).issubset(os.sched_getaffinity(0)):
        raise ValueError('CPU selection exceeds current permitted affinity')
    return cpus


def storage_profile(value):
    if value not in STORAGE_PROFILES:
        raise ValueError('storage profile must be wal_full or delete_extra')
    return value


def authenticated_endpoint(value):
    """Validate a remote mTLS address, without establishing its transport."""
    if not isinstance(value, str) or any(char.isspace() or ord(char) < 32 for char in value):
        raise ValueError('burst endpoint must be an explicit HTTPS address')
    endpoint = urlsplit(value)
    if (endpoint.scheme != 'https' or not endpoint.hostname or endpoint.username is not None
            or endpoint.password is not None or endpoint.path not in ('', '/')
            or endpoint.query or endpoint.fragment or not 1 <= (443 if endpoint.port is None else endpoint.port) <= 65535):
        raise ValueError('burst endpoint requires HTTPS without credentials, path, query or fragment')
    host = endpoint.hostname
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        if len(host) > 253 or any(not re.fullmatch(r'[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?', part)
                                for part in host.split('.')):
            raise ValueError('burst endpoint has an invalid TLS hostname')
        san = 'DNS:' + host
    else:
        if address.is_unspecified or address.is_multicast or '%' in host:
            raise ValueError('burst endpoint requires an exact unicast address')
        san = 'IP:' + str(address)
    return value.rstrip('/'), host, san


def burst_settings(args):
    """Separate node-local placement from the existing authenticated transport."""
    names = ('root', 'python', 'binary', 'source_root', 'sdk_root', 'cpus')
    explicit = {name: getattr(args, 'burst_' + name, None) for name in names}
    home = getattr(args, 'burst_home', None)
    if not home and any(value is not None for value in explicit.values()):
        raise ValueError('explicit burst paths and CPUs require --burst-home')
    if home:
        missing = [name for name, value in explicit.items() if not value]
        if missing:
            raise ValueError('physical burst requires explicit ' + ', '.join('--burst-' + name.replace('_', '-') for name in missing))
        if getattr(args, 'docker_root', DEFAULT_DOCKER_ROOT) != DEFAULT_DOCKER_ROOT:
            raise ValueError('choose explicit burst paths or a custom --docker-root')
        if not getattr(args, 'burst_endpoint', None):
            raise ValueError('physical burst requires --burst-endpoint for its established authenticated transport')
        result = {name: remote_path(explicit[name], home=home) for name in names if name != 'cpus'}
        # Remote Linux CPU IDs must not be checked against the anchor's affinity.
        result.update(kind='physical', home=str(PurePosixPath(home)), cpus=cpu_ids(explicit['cpus']),
                      reserve_ram_mib=16384)
    else:
        root = remote_path(getattr(args, 'docker_root', DEFAULT_DOCKER_ROOT))
        result = {'kind': 'docker', 'home': '/home/resmgr', 'root': root, 'cpus': [0, 1], 'reserve_ram_mib': 1024,
                  'python': remote_path(root, 'venv-isolated/bin/python'),
                  'binary': remote_path(root, 'source/ResourceManager/target/release/resmgr'),
                  'source_root': remote_path(root, 'source/Kaggriculture'),
                  'sdk_root': remote_path(root, 'source/ResourceManager/python')}
    result['storage_profile'] = storage_profile(getattr(args, 'burst_storage_profile', 'wal_full'))
    return result


def node_settings(node_id, mode, state, reserve_ram, reserve_cpu, profile='wal_full'):
    return {'schema_version': 2, 'node_id': node_id, 'node_mode': mode, 'state_dir': state,
            # Omission preserves the established WAL/FULL default and old readers.
            **({'storage_profile': storage_profile(profile)} if profile != 'wal_full' else {}),
            'execution': {'enabled': True, 'prepare_timeout_ms': 10000, 'admission_timeout_ms': 60000,
                          'release_confirm_timeout_ms': 10000},
            'monitor': {'interval_ms': 500}, 'cpu': {'reserve_physical_cores': reserve_cpu, 'nice': 10},
            'ram': {'reserve_mib': reserve_ram, 'reserve_percent': 5},
            'gpu': {'scale_up_cooldown_ms': 3000},
            'lifecycle': {'drain_timeout_ms': 3000, 'term_grace_ms': 2000,
                          'heartbeat_interval_ms': 2000, 'allocation_lease_ms': 10000},
            'cgroup': {'enabled': False}}


def agent_settings(endpoint, tls, cpus, ram_mib=4096):
    return {'coordinator_url': endpoint, 'tls': tls, 'cpu_affinity': cpus,
            'capacity': {'cpu_millicores': 1000, 'ram_mib': ram_mib, 'gpu_memory_mib': {}},
            'max_workers': 1, 'max_transfer_bytes_per_second': 10 * 1024**2,
            'max_spool_bytes': 2 * 1024**3, 'max_runtime_seconds': SERVICE_SECONDS}


def prepare(args):
    root = inside_home(args.root)
    run_id, anchor, burst = map(safe_id, (args.run_id, args.anchor_node_id, args.burst_node_id))
    if anchor == burst:
        raise ValueError('distinct node identities required')
    cpus = cpu_list(args.anchor_cpus)
    python = home_executable(args.python); binary = home_executable(args.binary)
    source = inside_home(getattr(args, 'source_root', None) or root / 'source/Kaggriculture')
    sdk = inside_home(getattr(args, 'sdk_root', None) or root / 'source/ResourceManager/python')
    manager = sdk.parent
    placement = burst_settings(args)
    anchor_profile = storage_profile(getattr(args, 'anchor_storage_profile', 'wal_full'))
    coordinator_profile = storage_profile(getattr(args, 'coordinator_storage_profile', 'wal_full'))
    candidate, bootstrap = required(args.candidate), required(args.bootstrap)
    dataset = inside_home(args.dataset)
    for path in (dataset / 'manifest.json', source / 'integration/resmgr/workflow.py',
                 sdk / 'resmgr/__init__.py', source / 'training/self_play.py'):
        required(path)
    spec = importlib.util.spec_from_file_location('two_node_snapshot_verifier', required(source / 'integration/resmgr/worker.py'))
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    snapshot_hash = sha256_file(required(candidate.parent / 'snapshot_manifest.json'))
    model_hash = sha256_file(required(candidate.parent / 'weights/policy_value.ts'))
    module.verify_snapshot(candidate, snapshot_hash, model_hash)
    snapshot = json.loads((candidate.parent / 'snapshot_manifest.json').read_text())
    # Copy only verified snapshot members, not arbitrary adjacent files.
    snapshot_files = {'snapshot_manifest.json', *(item['path'] for item in snapshot['files'])}
    if 'main.py' not in snapshot_files or candidate.name != 'main.py':
        raise ValueError('real candidate main.py must be included in its snapshot manifest')
    league = required(source / 'config/league_bases_r004.json')
    opponents = []
    for item in json.loads(league.read_text())['opponents']:
        path = required(league.parent / item['path']); relative = path.relative_to(source)
        if sha256_file(path) != item['sha256']:
            raise ValueError('opponent integrity failure')
        opponents.append(({**item, 'path': str(path)}, relative.as_posix()))
    if not opponents:
        raise ValueError('actual bounded league is empty')
    output = root / 'evidence' / (run_id + '-services')
    harness_output = root / 'evidence' / run_id
    if output.exists() or harness_output.exists():
        raise ValueError('fresh run and service output paths required')
    def burst_path(root, *parts):
        return remote_path(root, *parts, home=placement['home'])
    burst_bootstrap = burst_path(placement['root'], 'runs', run_id, 'bootstrap')
    burst_candidate = burst_path(burst_bootstrap, 'snapshot', 'main.py')
    listen_host = getattr(args, 'proxy_listen_host', None)
    source_ip = getattr(args, 'burst_source_ip', None)
    if bool(listen_host) != bool(source_ip):
        raise ValueError('public transport requires both proxy listen host and approved burst source IP')
    if listen_host:
        from connection_proxy import public_ip
        listen_host, source_ip = public_ip(listen_host), public_ip(source_ip)
    default_endpoint = (('https://' + ('[' + listen_host + ']' if ':' in listen_host else listen_host) + ':45670')
                        if listen_host else 'https://host.docker.internal:45672')
    burst_endpoint, tls_name, endpoint_san = authenticated_endpoint(getattr(args, 'burst_endpoint', None) or default_endpoint)
    output.mkdir(parents=True, mode=0o700)
    os.chmod(output, 0o700)
    sans = ['DNS:localhost', 'DNS:host.docker.internal', 'IP:127.0.0.1']
    if listen_host: sans.append('IP:' + listen_host)
    if endpoint_san not in sans: sans.append(endpoint_san)
    pki = generate_pki(output / 'pki', sans, [anchor, burst])
    def tls(name, base=None):
        folder = str(output / 'pki') if base is None else burst_path(base, 'pki')
        return {'ca_cert': folder + '/ca.pem', 'certificate': folder + '/' + name + '.pem',
                'private_key': folder + '/' + name + '.key'}
    endpoint = 'https://127.0.0.1:45671'
    configs = {
        'coordinator.json': {'listen': '127.0.0.1:45671', 'state_dir': str(output / 'coordinator-state'),
                             **({'storage_profile': coordinator_profile} if coordinator_profile != 'wal_full' else {}),
                             'tls': tls('server'), 'clients': pki['clients'], 'lease_ms': 10000,
                             'telemetry_ttl_ms': 3000, 'retry_limit': 3, 'max_artifact_bytes': 256 * 1024**2,
                             'artifact_quota_bytes': 4 * 1024**3},
        'operator.json': {'endpoint': endpoint, 'tls': tls('operator')},
        'anchor-agent.json': agent_settings(endpoint, tls('node-0'), cpus),
        'anchor-node.json': node_settings(anchor, 'guaranteed', str(output / 'anchor-state'), 16384, 1, anchor_profile),
        'burst-agent.json': agent_settings(burst_endpoint, tls('node-1', burst_bootstrap), placement['cpus'], ram_mib=2048),
        # Docker retains its outer 2-CPU cap. A physical owner must instead
        # verify two physical cores of affinity for one actor plus manager
        # headroom; rootless affinity/accounting is not an enforced CPU quota.
        'burst-node.json': node_settings(burst, 'opportunistic', burst_path(burst_bootstrap, 'agent-state'),
                                         placement['reserve_ram_mib'], 0,
                                         placement['storage_profile']),
    }
    for name, value in configs.items():
        atomic_json(output / name, value)
    common = {'max_workers': 1, 'max_attempts': 4, 'snapshot_sha256': snapshot_hash,
              'model_sha256': model_hash, 'device': 'cpu'}
    application = {'experiment_id': run_id, 'generation': 0, 'run_seed': run_id, 'games': 12,
                   'output': str(harness_output / 'application'), 'client_config': str(output / 'operator.json'),
                   'validation_dataset': str(dataset), 'learner_ram_mib': 4096,
                   'nodes': [{**common, 'node_id': anchor, 'class': 'guaranteed', 'python': str(python),
                              'source_root': str(source), 'sdk_root': str(sdk), 'candidate': str(candidate),
                              'opponents': [item for item, _ in opponents]},
                             {**common, 'node_id': burst, 'class': 'opportunistic',
                              'python': placement['python'], 'source_root': placement['source_root'],
                              'sdk_root': placement['sdk_root'], 'candidate': burst_candidate,
                              'opponents': [{**item, 'path': burst_path(placement['source_root'], relative)}
                                            for item, relative in opponents]}]}
    atomic_json(output / 'application.json', application)
    harness = {'execution_approved': True, 'output': str(harness_output),
               'application_config': str(output / 'application.json'), 'bootstrap': str(bootstrap), 'python': str(python),
               'anchor_node_id': anchor, 'burst_node_id': burst, 'wall_seconds': WORK_SECONDS,
               'cleanup_seconds': 120, 'phase_seconds': 120, 'disconnect_seconds': 25,
               'proxy_config': {'execution_approved': True, 'listen': ['127.0.0.1', 45670],
                                'target': ['127.0.0.1', 45671], 'duration_seconds': WORK_SECONDS,
                                'max_connections': 16, 'output': str(harness_output / 'proxy')}}
    if listen_host:
        harness['proxy_config'].update(listen=[listen_host, 45670], max_forward_bytes=2 * 1024**3,
            public_listener={'enabled': True, 'source_ips': [source_ip],
                             'coordinator_deployment': str(output / 'coordinator.json'),
                             'coordinator_sha256': sha256_file(output / 'coordinator.json'),
                             'ca_certificate_sha256': sha256_file(output / 'pki/ca.pem'),
                             'server_certificate_sha256': sha256_file(output / 'pki/server.pem')})
    from two_node_validation import validate
    validate(harness, application)
    atomic_json(output / 'harness.json', harness)
    transfer = []
    for name in ('ca.pem', 'node-1.pem', 'node-1.key'):
        transfer.append({'source': str(output / 'pki' / name), 'destination': burst_path(burst_bootstrap, 'pki', name)})
    for name in ('burst-node.json', 'burst-agent.json'):
        transfer.append({'source': str(output / name), 'destination': burst_path(burst_bootstrap, name)})
    for name in sorted(snapshot_files):
        path = required(candidate.parent / name)
        path.relative_to(candidate.parent)
        transfer.append({'source': str(path), 'destination': burst_path(burst_bootstrap, 'snapshot', name)})
    for item in transfer:
        item.update(sha256=sha256_file(Path(item['source'])), size=Path(item['source']).stat().st_size,
                    mode='0600' if item['source'].endswith('.key') else '0644')
    manifest = {'schema_version': 1, 'status': 'prepared_not_executed', 'run_id': run_id,
                'services_output': str(output), 'harness_output': str(harness_output), 'python': str(python),
                'binary': str(binary), 'binary_sha256': sha256_file(binary), 'manager_root': str(manager),
                'source_root': str(source), 'sdk_root': str(sdk), 'bootstrap_sha256': sha256_file(bootstrap),
                'burst_cpu_policy': ('0 additional reserved physical cores inside the explicit 2-CPU container cap; one 1000-millicore actor plus manager overhead. No CPU allocation guarantee.'
                                     if placement['kind'] == 'docker' else
                                     'One 1000-millicore opportunistic actor; operator must verify two physical cores of affinity for actor plus manager headroom. 0 additional reserved physical cores. No enforced CPU quota or allocation guarantee.'),
                'model_sha256': model_hash, 'snapshot_sha256': snapshot_hash,
                'anchor_node_id': anchor, 'burst_node_id': burst, 'service_seconds': SERVICE_SECONDS,
                'start_marker': str(output / 'START'), 'stop_marker': str(output / 'STOP'),
                'burst_transfer': transfer, 'burst_bootstrap': burst_bootstrap,
                # Retain the old key as an alias for existing transfer readers.
                'docker_bootstrap': burst_bootstrap, 'burst_placement': placement,
                'storage_profiles': {'anchor': anchor_profile, 'burst': placement['storage_profile'],
                                     'coordinator': coordinator_profile},
                'burst_env': {'TMPDIR': burst_bootstrap, 'XDG_CACHE_HOME': burst_path(burst_bootstrap, 'cache'),
                              'PYTHONDONTWRITEBYTECODE': '1', 'PYTHONPATH': placement['sdk_root'] + ':' + placement['source_root'],
                              'OMP_NUM_THREADS': '1', 'MKL_NUM_THREADS': '1', 'OPENBLAS_NUM_THREADS': '1', 'CUDA_VISIBLE_DEVICES': ''},
                'burst_preflight_required': 'Verify runtime HOME, canonical paths, executable and source integrity, CPU affinity and qualified storage profile on the burst host before launch.',
                'burst_argv': [placement['binary'], '--config', burst_path(burst_bootstrap, 'burst-node.json'), 'agent',
                               '--deployment', burst_path(burst_bootstrap, 'burst-agent.json')],
                'transport': {'coordinator_loopback_port': 45671, 'owned_proxy_listen': harness['proxy_config']['listen'],
                              'owned_proxy_loopback_port': None if listen_host else 45670,
                              'mac_tunnel_loopback_port': None if listen_host or getattr(args, 'burst_endpoint', None) else 45672,
                              'burst_endpoint': burst_endpoint, 'tls_name': tls_name,
                              'route_requirement': 'Existing operator-owned transport must route the burst endpoint through the owned proxy for disconnect/rejoin validation.',
                              'source_allowlist': [source_ip] if source_ip else None},
                'inputs': {str(path): sha256_file(path) for path in (candidate, bootstrap, dataset / 'manifest.json', league)}}
    atomic_json(output / 'manifest.json', manifest)
    return manifest


def owned_jobs(status, experiment):
    return [job['job_id'] for job in status.get('jobs', []) if job['job_id'].startswith(experiment + '.')]


def request_owned_cleanup(client, status, experiment, configured_nodes):
    for job in owned_jobs(status, experiment): client.cancel(job)
    if any(not task['task_id'].startswith(experiment + '.') for task in status.get('tasks', [])):
        raise RuntimeError('unrelated task in private deployment; node-wide drain refused')
    registered = {node['report']['node_id'] for node in status.get('nodes', [])}
    selected = set(configured_nodes) & registered
    for node in sorted(selected): client.drain_node(node)
    # Lack of registration is not evidence that any retained allocation released.
    return {'registered_drained': sorted(selected),
            'configured_not_registered': sorted(set(configured_nodes) - registered)}


def run(manifest_path, execute=False):
    if not execute or sys.platform != 'linux':
        raise ValueError('explicit Linux execution approval required')
    path = required(manifest_path); manifest = json.loads(path.read_text())
    output = inside_home(manifest['services_output'])
    if path != output / 'manifest.json' or (output / 'runtime-report.json').exists():
        raise ValueError('fresh prepared service manifest required; no persisted PID adoption')
    binary = home_executable(manifest['binary']); python = home_executable(manifest['python'])
    if sha256_file(binary) != manifest['binary_sha256']:
        raise ValueError('frozen manager binary changed')
    for name, digest in manifest['inputs'].items():
        if sha256_file(required(name)) != digest:
            raise ValueError('prepared input changed')
    manager = inside_home(manifest['manager_root']); source = inside_home(manifest['source_root'])
    sdk = inside_home(manifest.get('sdk_root', manager / 'python'))
    env = dict(os.environ, TMPDIR=str(output), XDG_CACHE_HOME=str(output / 'cache'),
               PYTHONDONTWRITEBYTECODE='1', PYTHONPATH=os.pathsep.join([str(sdk), str(source)]),
               OMP_NUM_THREADS='1', MKL_NUM_THREADS='1', OPENBLAS_NUM_THREADS='1', CUDA_VISIBLE_DEVICES='')
    started = time.monotonic(); limit = started + SERVICE_SECONDS
    report = {'status': 'starting', 'started_unix': time.time(), 'services': {}, 'cleanup': [],
              'max_seconds': SERVICE_SECONDS, 'unresolved_allocations': [], 'operational_completion': False}
    services = {}; driver = None; client = Client.from_config(output / 'operator.json')
    # This controller uses small local status/control RPCs; artifact transfer
    # retains the SDK's ordinary timeout in the separate application controller.
    client.timeout = 3
    ledger = AllocationLedger([manifest['anchor_node_id'], manifest['burst_node_id']]); latest = None
    stopped = []; previous = {sig: signal.signal(sig, lambda sig, frame: stopped.append(sig)) for sig in (signal.SIGTERM, signal.SIGINT)}
    def tick():
        if stopped or (output / 'STOP').exists():
            raise InterruptedError('operator stop requested')
        if time.monotonic() >= limit - 60:
            raise TimeoutError('service envelope reached cleanup reserve')
        guard = local_guard(output, {'min_ram_free_bytes': 16 * 1024**3, 'min_disk_free_bytes': 16 * 1024**3,
                                     'max_output_bytes': 4 * 1024**3})
        if guard['errors']:
            raise RuntimeError('anchor safety guard: ' + ', '.join(guard['errors']))
        for name, process in services.items():
            if process.poll() is not None:
                raise RuntimeError('owned service exited: ' + name)
    try:
        doctor = subprocess.run([str(binary), '--config', str(output / 'anchor-node.json'), 'doctor'],
                                env=env, text=True, capture_output=True, check=True, timeout=30)
        atomic_json(output / 'anchor-doctor.json', json.loads(doctor.stdout))
        services['coordinator'] = OwnedProcess([str(binary), 'coordinator', '--deployment', str(output / 'coordinator.json')],
                                               manager, output / 'coordinator.log', env)
        until = time.monotonic() + 20
        while True:
            tick()
            try: latest = client.status(); break
            except Exception:
                if time.monotonic() >= until: raise
                time.sleep(.2)
        if latest.get('tasks') or latest.get('jobs') or latest.get('allocations'):
            raise RuntimeError('prepared coordinator is not a fresh private deployment')
        services['anchor'] = OwnedProcess([str(binary), '--config', str(output / 'anchor-node.json'), 'agent', '--deployment',
                                         str(output / 'anchor-agent.json')], manager, output / 'anchor.log', env)
        report.update(status='waiting_for_start', services={name: process.identity for name, process in services.items()})
        atomic_json(output / 'runtime-report.json', report)
        # A bounded setup interval reserves the full workload and cleanup budget.
        gate_deadline = min(started + 120, limit - WORK_SECONDS - 60)
        while not (output / 'START').exists():
            tick()
            if time.monotonic() >= gate_deadline:
                raise TimeoutError('START gate absent within bounded setup interval')
            time.sleep(.5)
        driver = OwnedProcess([str(python), str(Path(__file__).with_name('two_node_validation.py')), '--config',
                               str(output / 'harness.json'), '--execute'], manager, output / 'harness.log', env)
        report.update(status='running', driver_identity=driver.identity)
        atomic_json(output / 'runtime-report.json', report)
        while driver.poll() is None:
            tick(); latest = client.status(); ledger.observe(latest); time.sleep(.5)
        report['cleanup'].append({'stage': 'harness', **driver.stop()}); code = driver.child.returncode; driver = None
        inner = json.loads(required(Path(manifest['harness_output']) / 'report.json').read_text())
        report['harness_status'] = inner['status']
        if code or inner['status'] != 'passed_two_node_application_scenarios':
            raise RuntimeError('two-node harness failed; preserve its full report and allocations')
        report['status'] = 'passed_pending_service_cleanup'
    except BaseException as error:
        report.update(status='failed', error=type(error).__name__ + ': ' + str(error))
    finally:
        if driver is not None:
            # Let its own cancel/drain/release protocol run before any escalation.
            Path(manifest['harness_output']).mkdir(parents=True, exist_ok=True)
            (Path(manifest['harness_output']) / 'STOP').touch()
            try: report['cleanup'].append({'stage': 'harness', **driver.stop(grace=min(110, max(0, limit - time.monotonic() - 30)))})
            except Exception as error: report['cleanup'].append({'stage': 'harness', 'error': str(error)})
        try:
            latest = client.status(); ledger.observe(latest)
            report['node_cleanup'] = request_owned_cleanup(client, latest, manifest['run_id'], ledger.nodes)
            deadline = min(limit - 25, time.monotonic() + 30)
            while time.monotonic() < deadline:
                latest = client.status(); ledger.observe(latest)
                if not ledger.unresolved(): break
                time.sleep(.5)
        except Exception as error: report['cleanup'].append({'stage': 'release_review', 'error': str(error)})
        # These handles identify only our direct services, not their descendants.
        # Any unknown worker remains reserved in durable state after service exit.
        for name in ('anchor', 'coordinator'):
            if name not in services: continue
            try:
                item = services[name].stop(grace=15 if name == 'anchor' else 2)
                item['descendant_contract'] = 'direct service only; worker release requires separate evidence'
                report['cleanup'].append({'stage': name, **item})
            except Exception as error: report['cleanup'].append({'stage': name, 'error': str(error)})
            if name == 'anchor':
                try: latest = client.status(); ledger.observe(latest)
                except Exception as error: report['cleanup'].append({'stage': 'final_status', 'error': str(error)})
        try:
            result = subprocess.run([str(binary), '--config', str(output / 'anchor-node.json'), 'executions'],
                                    env=env, text=True, capture_output=True, check=True, timeout=10)
            records = json.loads(result.stdout); atomic_json(output / 'anchor-executions.json', records)
            report['local_execution_phases'] = [item['phase'] for item in records]
            if any(item['phase'] != 'released' for item in records):
                report['cleanup'].append({'stage': 'local_execution_review', 'error': 'local allocations require reconciliation'})
        except Exception as error: report['cleanup'].append({'stage': 'local_execution_review', 'error': str(error)})
        report.update(elapsed_seconds=time.monotonic() - started, unresolved_allocations=ledger.unresolved(),
                      burst_cleanup='invoking burst owner must collect executions, confirm release, and stop/reap its owned service')
        if manifest.get('burst_placement', {}).get('kind', 'docker') == 'docker':
            report['docker_cleanup'] = 'invoking Docker owner must collect burst executions and verify container cleanup'
        if latest is not None: atomic_json(output / 'final-status.json', latest)
        if report['status'] == 'passed_pending_service_cleanup' and not report['unresolved_allocations'] and not any('error' in row or not row.get('reaped', False) for row in report['cleanup']):
            report['status'] = 'passed_anchor_services_and_two_node_driver'
        elif report['unresolved_allocations'] or any('error' in row for row in report['cleanup']):
            report['status'] = 'failed_cleanup_requires_reconciliation'
        if report['elapsed_seconds'] > SERVICE_SECONDS:
            report['status'] = 'failed_service_wall_envelope'
        atomic_json(output / 'runtime-report.json', report)
        for sig, handler in previous.items(): signal.signal(sig, handler)
    return report


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__); commands = parser.add_subparsers(dest='mode', required=True)
    prep = commands.add_parser('prepare')
    for name in ('root', 'run-id', 'python', 'binary', 'candidate', 'bootstrap', 'dataset', 'anchor-cpus', 'anchor-node-id', 'burst-node-id'):
        prep.add_argument('--' + name, required=True)
    prep.add_argument('--docker-root', default=DEFAULT_DOCKER_ROOT)
    prep.add_argument('--source-root', help='existing anchor application source; default ROOT/source/Kaggriculture')
    prep.add_argument('--sdk-root', help='existing anchor Python SDK; default ROOT/source/ResourceManager/python')
    prep.add_argument('--burst-home', help='explicit physical burst runtime home; requires all burst paths, CPUs and endpoint')
    for name in ('root', 'python', 'binary', 'source-root', 'sdk-root', 'cpus'):
        prep.add_argument('--burst-' + name)
    prep.add_argument('--burst-endpoint', help='HTTPS endpoint reached through the established operator-owned transport and owned proxy')
    for name in ('anchor', 'burst', 'coordinator'):
        prep.add_argument('--' + name + '-storage-profile', choices=STORAGE_PROFILES, default='wal_full',
                          help='profile already qualified on this host; default wal_full')
    prep.add_argument('--proxy-listen-host'); prep.add_argument('--burst-source-ip')
    execute = commands.add_parser('run'); execute.add_argument('--manifest', required=True); execute.add_argument('--execute', action='store_true')
    return parser


def main():
    args = argument_parser().parse_args()
    if args.mode == 'prepare':
        result = prepare(args)
        print(json.dumps({key: result[key] for key in ('status', 'services_output', 'harness_output', 'burst_bootstrap', 'docker_bootstrap')}, indent=2)); return 0
    result = run(args.manifest, args.execute)
    print(json.dumps({key: result.get(key) for key in ('status', 'error', 'elapsed_seconds', 'unresolved_allocations')}, indent=2))
    return 0 if result['status'] == 'passed_anchor_services_and_two_node_driver' else 1


if __name__ == '__main__':
    sys.exit(main())
