#!/usr/bin/env python3
"""Bounded Linux anchor validation against real coordinator, agent and application.

Uses the installed private validation bundle; no installation, SSH, autostart or
shared-server action. The explicit execution flag acknowledges the approved anchor
envelope. Every run has fresh state and certificates and retains its evidence.
"""
import argparse
import json
import math
import os
from pathlib import Path
import re
import signal
import socket
import statistics
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from resmgr import Client, sha256_file
from make_test_pki import generate as generate_pki
from make_kaggriculture_validation import generate as generate_application, IMPLEMENTATION_FILES
from native_validation import physical_cpus
from local_smoke import family_memory
from soak import AllocationLedger
from validation_runtime import OwnedProcess, atomic_json, inside_home, local_guard, home_executable


TARGETS = {'matched_throughput_ratio_min': .90, 'manager_peak_rss_bytes_max': 512 * 1024**2,
           'manager_active_cpu_cores_max': .5, 'repetitions': 3, 'games_per_trial': 12}


def validate_games(path, count, device='cuda:0'):
    path = Path(path).resolve()
    records = json.loads((path / 'games.json').read_text())
    manifest = json.loads((path / 'run_manifest.json').read_text())
    summary = json.loads((path / 'summary.json').read_text())
    if len(records) != count or len({row['game_id'] for row in records}) != count:
        raise RuntimeError('real game outputs are missing or duplicated')
    if device not in ('cpu', 'cuda:0') or any(row.get('backend') != 'torchscript' or row.get('device') != device for row in records):
        raise RuntimeError('requested real ' + device + ' model was not used for every game')
    if any(not isinstance(row.get('inference_count'), int) or row['inference_count'] <= 0
           or row.get('inference_errors') != [] for row in records):
        raise RuntimeError('real inference was missing or silently fell back after an error')
    for row in records:
        replay = Path(row['replay']).resolve()
        if not replay.is_relative_to(path) or not replay.is_file() or sha256_file(replay) != row['replay_sha256']:
            raise RuntimeError('replay is outside the isolated trial or failed its integrity check')
    if any(summary.get(key) != 0 for key in ('bad_status_games', 'invalid_action_games', 'exception_games')):
        raise RuntimeError('the simulator reported unsuccessful games')
    return {'games': len(records), 'output_sha256': sha256_file(path / 'games.json'),
            'manifest': manifest, 'summary': summary,
            'identities': [{key: row.get(key) for key in ('game_id', 'seed', 'candidate_seat', 'opponent')} for row in records]}


def paired_report(cases):
    pairs = []
    for repetition in range(TARGETS['repetitions']):
        selected = {case['mode']: case for case in cases if case['repetition'] == repetition}
        if set(selected) != {'managed', 'unmanaged'}:
            continue
        first, second = selected['unmanaged'], selected['managed']
        if first['output']['identities'] != second['output']['identities']:
            raise RuntimeError('paired runs differ in game seeds, seats or opponents')
        for key in ('run_seed', 'games', 'workers', 'act_timeout', 'inference_device',
                    'resolved_inference_devices', 'engine_version', 'exploration', 'candidate', 'league'):
            if first['output']['manifest'][key] != second['output']['manifest'][key]:
                raise RuntimeError('paired application inputs differ: ' + key)
        pairs.append({'repetition': repetition,
                      'throughput_ratio': first['elapsed_seconds'] / second['elapsed_seconds']})
    ratio = statistics.median(pair['throughput_ratio'] for pair in pairs) if pairs else None
    return {'pairs': pairs, 'median_throughput_ratio': ratio,
            'status': 'pending' if len(pairs) < TARGETS['repetitions'] else
                      ('passed' if ratio >= TARGETS['matched_throughput_ratio_min'] else 'failed'),
            'timing_scope': 'process-start to verified unmanaged exit, or submit to accepted managed result; includes management admission overhead',
            'baseline_environment': 'coordinator and idle agent remain present during the unmanaged trial'}


def verify_task_status(tasks):
    if not tasks or any(task.get('status') not in ('queued', 'assigned', 'completed') for task in tasks):
        raise RuntimeError('managed task status is missing, unknown or needs reconciliation')


def identity_sample(identity, proc_root=Path('/proc')):
    """Read-only verified measurement; this never authorizes signaling a PID."""
    try:
        root = proc_root / str(identity['pid'])
        boot_id = (proc_root / 'sys/kernel/random/boot_id').read_text().strip()
        stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
        expected_start = identity.get('start_ticks', identity.get('start_time'))
        if boot_id != identity.get('boot_id') or expected_start != int(stat[19]) or stat[0] in ('Z', 'X'):
            return None
        rss = int(stat[21]) * os.sysconf('SC_PAGE_SIZE')
        # Recheck after observing usage; numeric PID reuse is not attribution.
        again = (root / 'stat').read_text().rsplit(')', 1)[1].split()
        if int(again[19]) != expected_start or again[0] in ('Z', 'X'):
            return None
        return {'pid': identity['pid'], 'boot_id': boot_id, 'start_ticks': int(stat[19]),
                'cpu_seconds': (int(stat[11]) + int(stat[12])) / os.sysconf('SC_CLK_TCK'),
                'rss_bytes': rss, 'observed_monotonic': time.monotonic()}
    except (OSError, ValueError, IndexError, KeyError, TypeError):
        return None


def manager_samples(services, state_dir, ledger):
    samples = {name: None if process.poll() is not None else identity_sample(process.identity)
               for name, process in services.items()}
    missing = [name for name, sample in samples.items() if sample is None]
    expected = {row['assignment_id']: row for row in ledger.unresolved()
                if row['phase'] in ('prepared', 'authorized', 'running', 'draining')}
    observed = set(); finalized = {}
    for path in (state_dir / 'attempts').glob('*/supervisor-identity.json'):
        try:
            identity = json.loads(path.read_text())
            assignment = identity['assignment_id']
            registered = ledger.allocations.get(assignment)
            if not registered or registered['generation'] != identity['generation']:
                missing.append('unmatched_supervisor_identity')
                continue
            sample = identity_sample(identity)
            if sample:
                samples['supervisor:' + assignment] = sample
                observed.add(assignment)
            else:
                usage_path = path.with_name('supervisor-usage.json')
                if usage_path.is_file():
                    usage = final_supervisor_usage(identity, json.loads(usage_path.read_text()))
                    finalized['supervisor:' + assignment] = usage
                    observed.add(assignment)
        except (OSError, ValueError, KeyError, TypeError):
            missing.append('unreadable_supervisor_identity')
    missing.extend('supervisor:' + identity for identity in expected if identity not in observed)
    return {'observed_monotonic': time.monotonic(), 'processes': samples,
            'finalized': finalized, 'missing': missing}


def current_manager_sample(client, services, state_dir, ledger):
    """Refresh protocol bindings before measurement, retrying one publication race.

    An agent can publish a new supervisor identity between status and filesystem
    reads. A second authenticated status can bind that identity; persisting gaps
    remain explicit. Earlier samples are never rewritten from later counters.
    """
    last_status = client.status()
    ledger.observe(last_status)
    sample = manager_samples(services, state_dir, ledger)
    if 'unmatched_supervisor_identity' in sample['missing']:
        last_status = client.status()
        ledger.observe(last_status)
        sample = manager_samples(services, state_dir, ledger)
    return last_status, sample


def final_supervisor_usage(identity, usage):
    if usage.get('identity') != identity or usage.get('scope') != 'supervisor_self_excludes_workers':
        raise ValueError('final supervisor accounting identity/scope mismatch')
    for key in ('user_cpu_us', 'system_cpu_us', 'peak_rss_bytes', 'observed_monotonic_ms'):
        if type(usage.get(key)) is not int or usage[key] < 0:
            raise ValueError('missing or invalid final supervisor accounting')
    return {'pid': identity['pid'], 'boot_id': identity['boot_id'], 'start_ticks': identity['start_time'],
            'cpu_seconds': (usage['user_cpu_us'] + usage['system_cpu_us']) / 1e6,
            'rss_bytes': 0, 'peak_rss_bytes': usage['peak_rss_bytes'],
            'observed_monotonic': usage['observed_monotonic_ms'] / 1000,
            'final': True, 'cutoff': usage.get('cutoff')}


def manager_metrics(samples):
    rss = []; cpu = []; gaps = []
    for index, sample in enumerate(samples):
        processes = {**sample.get('finalized', {}), **sample['processes']}
        if sample['missing'] or any(value is None for value in processes.values()):
            gaps.append({'sample': index, 'reason': 'missing_identity_or_usage'})
            continue
        rss.append(sum(value['rss_bytes'] for value in processes.values()))
        if index:
            previous = samples[index - 1]
            before = {**previous.get('finalized', {}), **previous['processes']}
            if previous['missing'] or not set(before).issubset(processes) or any(value is None for value in before.values()):
                gaps.append({'sample': index, 'reason': 'process_set_changed_or_previous_sample_missing'})
                continue
            elapsed = sample['observed_monotonic'] - previous['observed_monotonic']
            same = all((before[key]['pid'], before[key]['start_ticks'], before[key]['boot_id']) ==
                       (processes[key]['pid'], processes[key]['start_ticks'], processes[key]['boot_id']) for key in before)
            # A verified supervisor born between samples contributes its complete
            # CPU counter. A reaped one retains its final self-rusage counter, so
            # short-lived manager processes cannot silently disappear as zero.
            new = set(processes) - set(before)
            if any(not key.startswith('supervisor:') for key in new):
                same = False
            delta = sum(value['cpu_seconds'] - before.get(key, {}).get('cpu_seconds', 0)
                        for key, value in processes.items())
            finalized_now = [value for key, value in processes.items()
                             if value.get('final') and not before.get(key, {}).get('final')]
            # Sum of departing supervisors' individual peaks bounds their unseen
            # RSS during this interval; it is conservative, not simultaneous RSS.
            rss.append(sum(value['rss_bytes'] for value in processes.values()) +
                       sum(value['peak_rss_bytes'] for value in finalized_now))
            if same and elapsed > 0 and delta >= 0:
                cpu.append(delta / elapsed)
            else:
                gaps.append({'sample': index, 'reason': 'identity_changed_or_invalid_interval'})
    peak_rss = max(rss) if rss else None
    peak_cpu = max(cpu) if cpu else None
    return {'scope': 'coordinator, agent and independently identified live supervisors; excludes user workers',
            'sampling_limits': 'service RSS remains sampled; departing supervisors include conservative individual peak RSS and final self CPU counters. Final-accounting publication and exit tail are outside the supervisor counter cutoff. Missing identity/accounting is inconclusive.',
            'sampled_peak_rss_bytes': peak_rss, 'sampled_peak_active_cpu_cores': peak_cpu, 'coverage_gaps': gaps,
            'status': 'inconclusive' if gaps or peak_rss is None or peak_cpu is None else
                      ('passed' if peak_rss <= TARGETS['manager_peak_rss_bytes_max'] and
                       peak_cpu <= TARGETS['manager_active_cpu_cores_max'] else 'failed')}


def comparison_envelope(stage, device, gpu_uuid, wall_seconds=None):
    if device not in ('cpu', 'cuda:0'):
        raise ValueError('device must be cpu or cuda:0')
    if device == 'cpu':
        if stage != 'command' or gpu_uuid:
            raise ValueError('CPU comparison supports the command stage without a GPU UUID')
        seconds = 900 if wall_seconds is None else wall_seconds
        if type(seconds) is not int or not 900 <= seconds <= 1800:
            raise ValueError('CPU comparison total execution must be 900..1800 seconds')
        return {'wall_seconds': seconds, 'work_seconds': seconds - 120, 'cleanup_reserve_seconds': 120,
                'family_rss_bytes': 4 * 1024**3, 'gpu_memory_mib': 0}
    if wall_seconds is not None:
        raise ValueError('explicit comparison wall override is supported only for CPU command trials')
    if not gpu_uuid or not gpu_uuid.startswith('GPU-') or ',' in gpu_uuid:
        raise ValueError('one approved GPU UUID is required for CUDA')
    seconds = 6 * 900 + 120 if stage == 'command' else 3600
    return {'wall_seconds': seconds, 'work_seconds': seconds, 'cleanup_reserve_seconds': None,
            'family_rss_bytes': None, 'gpu_memory_mib': 6144}


def prior_process_absent(identity, proc_root=Path('/proc')):
    """Verified prior identity is gone; unreadable evidence raises, never means gone."""
    if (type(identity.get('pid')) is not int or identity['pid'] <= 0 or
            type(identity.get('start_ticks')) is not int or identity['start_ticks'] <= 0 or
            not isinstance(identity.get('boot_id'), str) or not identity['boot_id']):
        raise ValueError('prior process identity is incomplete')
    boot_id = (proc_root / 'sys/kernel/random/boot_id').read_text().strip()
    if boot_id != identity['boot_id']:
        raise ValueError('host boot changed; comparison environment requires a new review')
    process = proc_root / str(identity['pid'])
    try:
        stat = (process / 'stat').read_text().rsplit(')', 1)[1].split()
    except FileNotFoundError:
        if process.exists():
            raise ValueError('prior process still exists with unreadable identity')
        return True
    return int(stat[19]) != identity['start_ticks']


def resume_cases(path, root, expected):
    """Reuse verified completed trials only; never adopt prior services or overwrite outputs.

    This resumes one time-limited comparison's evidence in a fresh deployment.
    It is not process recovery or permission to extend an execution envelope.
    """
    path = inside_home(path)
    prior = json.loads(path.read_text())
    old_id = prior.get('run_id', '')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,79}', old_id):
        raise ValueError('prior report lacks an explicit safe run ID')
    if path != (root / 'evidence' / old_id / 'report.json').resolve():
        raise ValueError('prior evidence path does not match its explicit run ID')
    if (prior.get('stage') != 'command' or prior.get('device') != 'cpu' or
            prior.get('status') != 'failed' or prior.get('error_type') != 'TimeoutError' or
            prior.get('error') != 'stage envelope elapsed' or prior.get('resumed_from')):
        raise ValueError('only an original time-limited CPU command comparison can be continued')
    for key, value in expected.items():
        if prior.get(key) != value:
            raise ValueError('prior comparison context differs: ' + key)
    if (prior.get('uncertain_allocations') != [] or not prior.get('cleanup') or
            any(not row.get('reaped') for row in prior['cleanup']) or
            any(prior.get(key) for key in ('cleanup_error', 'unresolved_owned_services', 'measurement_cleanup_error')) or
            {row.get('service') for row in prior['cleanup'] if row.get('service')} != {'agent', 'coordinator'}):
        raise ValueError('prior cleanup is not verified; reconcile before continuation')
    if any(not prior_process_absent(row) for row in prior['cleanup']):
        raise ValueError('a prior owned process is still live')
    elapsed = prior.get('elapsed_seconds')
    if (type(elapsed) not in (int, float) or not math.isfinite(elapsed) or not 0 < elapsed <= prior['envelope']['wall_seconds'] or
            prior['envelope']['family_rss_bytes'] != 4 * 1024**3 or prior['envelope']['gpu_memory_mib'] != 0):
        raise ValueError('prior execution envelope is invalid')
    final = json.loads(path.with_name('final-status.json').read_text())
    if not final.get('allocations') or any(row.get('phase') != 'released' for row in final['allocations']):
        raise ValueError('prior durable allocation releases are not verified')
    released = {row['assignment_id'] for row in final['allocations']}
    tasks = {task['task_id']: task for task in final.get('tasks', [])}
    application = inside_home(root / 'source/Kaggriculture')
    cases = []; seen = set()
    for case in prior.get('cases', []):
        key = case['repetition'], case['mode']
        if (key in seen or type(key[0]) is not int or key[0] not in range(TARGETS['repetitions']) or
                key[1] not in ('managed', 'unmanaged') or type(case['elapsed_seconds']) not in (int, float) or
                not math.isfinite(case['elapsed_seconds']) or not 0 < case['elapsed_seconds'] <= 900):
            raise ValueError('prior case identity or duration is invalid')
        seen.add(key)
        case_id = old_id + f'.pair{key[0]}'
        case_root = root / 'runs' / case_id
        generated = json.loads((case_root / 'manifest.json').read_text())
        if (generated['run_id'] != case_id or generated['model_sha256'] != case['model_sha256'] or
                generated['games'] != TARGETS['games_per_trial'] or generated['workers'] != 1 or
                set(generated['implementation_files']) != set(IMPLEMENTATION_FILES)):
            raise ValueError('prior generated input identity mismatch')
        for name, digest in generated['implementation_files'].items():
            source = inside_home(application / name)
            if not source.is_relative_to(application) or sha256_file(source) != digest:
                raise ValueError('application implementation changed since the prior comparison')
        if sha256_file(application / 'config/league_bases_r004.json') != generated['league_sha256']:
            raise ValueError('league changed since the prior comparison')
        if case['cpu_ids'] != expected['cpu_ids'] or case['device'] != 'cpu':
            raise ValueError('prior case CPU scope/device mismatch')
        verified = validate_games(case_root / ('command-' + key[1]), TARGETS['games_per_trial'], 'cpu')
        if verified != case['output'] or verified['manifest']['run_seed'] != case_id:
            raise ValueError('prior result, replay integrity, or seed identity changed')
        if key[1] == 'managed':
            task = tasks.get(case_id + '.command', {})
            if task.get('status') != 'completed' or task.get('assignment_id') not in released:
                raise ValueError('prior managed result lacks durable task completion and allocation release')
        cases.append({**case, 'output_path': str(case_root / ('command-' + key[1]))})
    if not cases or len(cases) >= 2 * TARGETS['repetitions']:
        raise ValueError('prior report has no incomplete comparison to continue')
    # A prefix preserves the preregistered alternating execution order.
    order = [(rep, mode) for rep in range(TARGETS['repetitions'])
             for mode in (['unmanaged', 'managed'] if rep % 2 == 0 else ['managed', 'unmanaged'])]
    if [(case['repetition'], case['mode']) for case in cases] != order[:len(cases)]:
        raise ValueError('prior cases do not preserve the comparison order')
    prior['cases'] = cases
    return prior, {'report': str(path), 'sha256': sha256_file(path), 'run_id': old_id,
                   'elapsed_seconds': elapsed, 'reused_cases': len(cases),
                   'scope': 'completed output reuse after verified cleanup; interrupted trial excluded; separate execution segments'}


def deployment(output, binary, cpus, node_id, gpu_uuid, seconds, env=None, device='cuda:0'):
    pki = generate_pki(output / 'pki', ['DNS:localhost', 'IP:127.0.0.1'], [node_id])
    def tls(name):
        return {'ca_cert': str(output / 'pki/ca.pem'),
                'certificate': str(output / ('pki/' + name + '.pem')),
                'private_key': str(output / ('pki/' + name + '.key'))}
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    endpoint = f'https://127.0.0.1:{port}'
    atomic_json(output / 'coordinator.json', {'listen': f'127.0.0.1:{port}',
        'state_dir': str(output / 'coordinator-state'), 'tls': tls('server'), 'clients': pki['clients'],
        'lease_ms': 10000, 'telemetry_ttl_ms': 3000, 'max_artifact_bytes': 256 * 1024**2,
        'artifact_quota_bytes': 20 * 1024**3, 'retry_limit': 3})
    atomic_json(output / 'operator.json', {'endpoint': endpoint, 'tls': tls('operator')})
    atomic_json(output / 'agent.json', {'coordinator_url': endpoint, 'tls': tls('node-0'),
        'cpu_affinity': cpus, 'max_transfer_bytes_per_second': 10 * 1024**2,
        'capacity': {'cpu_millicores': len(cpus) * 1000 if device == 'cpu' else 6000,
                     'ram_mib': 4096 if device == 'cpu' else 24576,
                     'gpu_memory_mib': {} if device == 'cpu' else {gpu_uuid: 6144}},
        'max_workers': 1 if device == 'cpu' else 3, 'max_spool_bytes': 2 * 1024**3,
        'max_runtime_seconds': seconds if device == 'cpu' else seconds + 120})
    atomic_json(output / 'node.json', {'schema_version': 2, 'node_id': node_id,
        'node_mode': 'guaranteed', 'state_dir': str(output / 'agent-state'),
        'execution': {'enabled': True, 'prepare_timeout_ms': 10000, 'admission_timeout_ms': 60000,
                      'release_confirm_timeout_ms': 10000},
        'monitor': {'interval_ms': 500},
        'cpu': {'reserve_physical_cores': min(2, len(cpus)-1) if device == 'cpu' else 2,
                'nice': max(0, os.getpriority(os.PRIO_PROCESS, 0)) if device == 'cpu' else 10},
        'ram': {'reserve_mib': 16384, 'reserve_percent': 5},
        'gpu': {'reserve_vram_mib': 4096, 'scale_up_cooldown_ms': 30000,
                'protective_shrink_percent': 25, 'active_shrink_percent': 50},
        'lifecycle': {'drain_timeout_ms': 3000, 'term_grace_ms': 2000,
                      'heartbeat_interval_ms': 2000, 'allocation_lease_ms': 10000},
        'cgroup': {'enabled': False}})
    checked = subprocess.run([str(binary), '--config', str(output / 'node.json'), 'doctor'],
                             check=True, capture_output=True, text=True, timeout=30, env=env)
    atomic_json(output / 'doctor.json', json.loads(checked.stdout))
    return Client.from_config(output / 'operator.json')


def run(args):
    if not args.execute or sys.platform != 'linux':
        raise ValueError('explicit execution opt-in on Linux is required; Mac is not a native validation host')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,79}', args.run_id):
        raise ValueError('safe unique run ID required')
    if not args.node_id:
        raise ValueError('explicit node ID required')
    device = getattr(args, 'device', 'cuda:0')
    envelope = comparison_envelope(args.stage, device, args.gpu_uuid, getattr(args, 'wall_seconds', None))
    root = inside_home(args.root)
    application, manager = root / 'source/Kaggriculture', root / 'source/ResourceManager'
    python = home_executable(root / 'venv-isolated/bin/python')
    binary = inside_home(args.binary) if getattr(args, 'binary', None) else manager / 'target/release/resmgr'
    bootstrap = inside_home(args.bootstrap) if args.bootstrap else None
    if bootstrap is None and device != 'cpu':
        raise ValueError('CUDA validation requires the existing bootstrap checkpoint')
    for path in (python, binary, inside_home(args.candidate), *([bootstrap] if bootstrap else [])):
        if not path.is_file():
            raise ValueError('required installed input absent: ' + str(path))
    seconds = envelope['wall_seconds']
    output = root / 'evidence' / args.run_id
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    cpus = physical_cpus(6)
    if not cpus or (device == 'cuda:0' and len(cpus) != 6):
        raise RuntimeError('approved six-core topology unavailable; select a reviewed smaller envelope')
    os.sched_setaffinity(0, cpus)
    env = dict(os.environ, TMPDIR=str(root / 'tmp'), XDG_CACHE_HOME=str(root / 'cache'),
               PYTHONPATH=os.pathsep.join([str(manager / 'python'), str(application)]),
               OMP_NUM_THREADS='1', MKL_NUM_THREADS='1', OPENBLAS_NUM_THREADS='1',
               PYTHONDONTWRITEBYTECODE='1', CUDA_VISIBLE_DEVICES='')
    report = {'schema_version': 1, 'run_id': args.run_id, 'stage': args.stage,
        'status': 'running', 'started_unix': time.time(), 'targets': TARGETS, 'cases': [],
        'node_id': args.node_id, 'gpu_uuid': args.gpu_uuid, 'device': device, 'cpu_ids': cpus,
        'envelope': envelope, 'actors_per_trial': 1, 'family_rss_samples': [],
        'wall_time_seconds': seconds, 'manager_binary_sha256': sha256_file(binary),
        'manager_binary': str(binary), 'python_executable': str(python),
        'candidate_sha256': sha256_file(inside_home(args.candidate)), 'bootstrap_sha256': sha256_file(bootstrap) if bootstrap else None,
        'manager_samples': [], 'cleanup': [], 'actual_24_hour_soak': False,
        'optional_kernel_controls': 'not_configured_unverified'}
    prior = None
    if getattr(args, 'resume_report', None):
        if device != 'cpu' or args.stage != 'command':
            raise ValueError('comparison continuation supports only CPU command trials')
        expected = {key: report[key] for key in ('targets', 'cpu_ids', 'device', 'actors_per_trial',
                    'manager_binary_sha256', 'python_executable', 'candidate_sha256', 'bootstrap_sha256')}
        prior, provenance = resume_cases(args.resume_report, root, expected)
        seconds = math.floor(seconds - prior['elapsed_seconds'])
        if seconds <= envelope['cleanup_reserve_seconds']:
            raise ValueError('approved total execution budget has no work time left')
        envelope = {**envelope, 'work_seconds': seconds - envelope['cleanup_reserve_seconds'],
                    'segment_wall_seconds': seconds, 'prior_execution_seconds': prior['elapsed_seconds']}
        report.update(cases=prior['cases'], resumed_from=provenance, envelope=envelope)
    services = {}; creator = None; client = None; last_status = None
    ledger = AllocationLedger([args.node_id]); started = time.monotonic(); stop = []
    previous = {sig: signal.signal(sig, lambda sig, _frame: stop.append(sig)) for sig in (signal.SIGINT, signal.SIGTERM)}
    last_guard = 0
    def tick():
        nonlocal last_status, last_guard
        if stop or (output / 'STOP').exists():
            raise InterruptedError('operator stop requested')
        if time.monotonic() - started > envelope['work_seconds']:
            raise TimeoutError('stage envelope elapsed')
        if time.monotonic() - last_guard > 2:
            guard = local_guard(root, {'min_ram_free_bytes': 16 * 1024**3,
                'min_disk_free_bytes': 10 * 1024**3, 'max_output_bytes': 60 * 1024**3})
            if device == 'cuda:0':
                gpu = subprocess.run(['nvidia-smi', '-i', args.gpu_uuid,
                    '--query-gpu=memory.free', '--format=csv,noheader,nounits'],
                    check=True, capture_output=True, text=True, timeout=2)
                guard['gpu_free_mib'] = int(gpu.stdout.strip())
                if guard['gpu_free_mib'] < 4096:
                    guard['errors'].append('gpu_headroom_below_bound')
            else:
                roots = [process.child.pid for process in services.values() if process.poll() is None]
                if creator is not None and creator.poll() is None:
                    roots.append(creator.child.pid)
                family = family_memory(roots)
                rss = sum(process['rss_bytes'] for process in family)
                report['family_rss_samples'].append({'elapsed_seconds': time.monotonic()-started,
                    'processes': family, 'rss_bytes': rss, 'scope': 'sampled descendants of live owned roots'})
                if rss > envelope['family_rss_bytes']:
                    guard['errors'].append('owned_family_rss_exceeds4GiB')
            with (output / 'guards.jsonl').open('a') as file:
                file.write(json.dumps(guard) + '\n')
            if guard['errors']:
                raise RuntimeError('guard abort: ' + ','.join(guard['errors']))
            if client:
                last_status, sample = current_manager_sample(client, services, output / 'agent-state', ledger)
            else:
                sample = manager_samples(services, output / 'agent-state', ledger)
            report['manager_samples'].append(sample)
            last_guard = time.monotonic()
        if services and any(child.poll() is not None for child in services.values()):
            raise RuntimeError('owned manager service exited; inspect logs and retained state')
        if client:
            last_status = client.status()
            ledger.observe(last_status)
            atomic_json(output / 'last-status.json', last_status)
        time.sleep(.5)
    try:
        client = deployment(output, binary, cpus, args.node_id, args.gpu_uuid, seconds, env, device)
        services['coordinator'] = OwnedProcess([str(binary), 'coordinator', '--deployment', str(output / 'coordinator.json')], manager, output / 'coordinator.log', env)
        deadline = time.monotonic() + 30
        while True:
            try:
                client.status(); break
            except Exception:
                if time.monotonic() > deadline or services['coordinator'].poll() is not None:
                    raise
                time.sleep(.25)
        services['agent'] = OwnedProcess([str(binary), '--config', str(output / 'node.json'), 'agent', '--deployment', str(output / 'agent.json')], manager, output / 'agent.log', env)
        deadline = time.monotonic() + 60
        while not (last_status and last_status.get('nodes')):
            if time.monotonic() > deadline:
                raise TimeoutError('agent registration missing')
            tick()
        if args.stage == 'command':
            for repetition in range(TARGETS['repetitions']):
                complete = {case['mode'] for case in report['cases'] if case['repetition'] == repetition}
                if complete == {'managed', 'unmanaged'}:
                    continue
                run_id = args.run_id + f'.pair{repetition}'
                seed_id = (prior['run_id'] if prior else args.run_id) + f'.pair{repetition}'
                generated = generate_application(root, run_id, python, args.candidate, args.gpu_uuid, output / 'operator.json', args.node_id, TARGETS['games_per_trial'], device, seed_id)
                case_root = Path(generated['output'])
                if prior:
                    original_node = json.loads((inside_home(args.resume_report).parent / 'node.json').read_text())
                    current_node = json.loads((output / 'node.json').read_text())
                    for key in original_node:
                        if key not in ('node_id', 'state_dir') and original_node[key] != current_node[key]:
                            raise ValueError('comparison policy changed: ' + key)
                    current_env = json.loads((case_root / 'unmanaged-command.json').read_text())['env']
                    for case in prior['cases']:
                        if case['model_sha256'] != generated['model_sha256']:
                            raise ValueError('comparison model changed')
                        if {k:v for k,v in case['configured_environment'].items() if k not in ('TMPDIR', 'XDG_CACHE_HOME')} != {k:v for k,v in current_env.items() if k not in ('TMPDIR', 'XDG_CACHE_HOME')}:
                            raise ValueError('comparison threading/import environment changed')
                pool = json.loads((case_root / 'command-pool.json').read_text())
                client.request('put_pool', pool=pool)
                for mode in (['unmanaged', 'managed'] if repetition % 2 == 0 else ['managed', 'unmanaged']):
                    if mode in complete:
                        continue
                    case_start = time.monotonic()
                    if mode == 'managed':
                        job = json.loads((case_root / 'command-job.json').read_text())
                        client.request('submit', job=job)
                    else:
                        command = json.loads((case_root / 'unmanaged-command.json').read_text())
                        creator = OwnedProcess(command['argv'], command['cwd'], output / f'pair{repetition}-unmanaged.log', dict(env, **command['env']))
                    while True:
                        tick()
                        if time.monotonic() - case_start > 900:
                            raise TimeoutError('individual paired trial exceeded 15 minutes')
                        if mode == 'managed':
                            if client.result(job['tasks'][0]['task_id']):
                                break
                            selected = client.status(job['job_id'])['tasks']
                            verify_task_status(selected)
                        elif creator.poll() is not None:
                            result = creator.stop(); report['cleanup'].append(result); creator = None
                            if result['exit_code'] != 0:
                                raise RuntimeError('unmanaged command failed')
                            break
                    case = {'mode': mode, 'repetition': repetition, 'elapsed_seconds': time.monotonic() - case_start,
                            'output': validate_games(case_root / ('command-' + mode), TARGETS['games_per_trial'], device),
                            'output_path': str(case_root / ('command-' + mode)),
                            'cpu_ids': cpus, 'device': device, 'model_sha256': generated['model_sha256'],
                            'configured_environment': json.loads((case_root / 'unmanaged-command.json').read_text())['env'],
                            'environment_scope': 'matched inference, threading and import settings; private manager lifecycle and output paths differ'}
                    report['cases'].append(case)
                    atomic_json(output / 'report.json', report)
                    # Do not start the paired peer against an unreleased GPU allocation.
                    release_deadline = time.monotonic() + 30
                    while ledger.unresolved():
                        if time.monotonic() > release_deadline:
                            raise RuntimeError('previous trial has an uncertain reservation')
                        tick()
            report['matched_comparison'] = paired_report(report['cases'])
        else:
            generated = generate_application(root, args.run_id + '.cycle', python, args.candidate, args.gpu_uuid, output / 'operator.json', args.node_id, 12)
            config = Path(generated['output']) / 'cooperative.json'
            creator = OwnedProcess([str(python), '-m', 'integration.resmgr', 'cycle', '--config', str(config),
                '--bootstrap', str(inside_home(args.bootstrap)), '--steps', '50', '--device', 'cuda:0'], application, output / 'cycle.log', env)
            while creator.poll() is None:
                tick()
            result = creator.stop(); report['cleanup'].append(result); creator = None
            if result['exit_code'] != 0:
                raise RuntimeError('real cooperative cycle failed')
            cycle = Path(json.loads(config.read_text())['output']) / 'cycle/result.json'
            report['cycle'] = json.loads(cycle.read_text())
            if report['cycle']['accepted_games'] != 12 or report['cycle']['contributing_nodes'] != [args.node_id]:
                raise RuntimeError('cooperative useful result ownership mismatch')
            if report['cycle'].get('continuation') != 'optimizer_step_cursor_v2':
                raise RuntimeError('expected exact checkpoint continuation was not recorded')
            checkpoint = inside_home(report['cycle']['checkpoint_path'])
            if not checkpoint.is_relative_to(cycle.parent) or sha256_file(checkpoint) != report['cycle']['checkpoint_sha256']:
                raise RuntimeError('cooperative checkpoint integrity or output ownership mismatch')
            if sha256_file(cycle.parent / 'model.ts') != report['cycle']['model_sha256']:
                raise RuntimeError('cooperative continued model integrity mismatch')
        report['status'] = 'completed_pending_metric_review'
    except BaseException as error:
        report.update(status='failed', error_type=type(error).__name__, error=str(error))
    finally:
        if creator:
            try:
                report['cleanup'].append(creator.stop())
            except Exception as error:
                report.setdefault('unresolved_owned_services', []).append({'name': 'creator', 'error': str(error)})
        if client:
            try:
                last_status = client.status()
                for job in last_status.get('jobs', []):
                    if job['job_id'].startswith(args.run_id):
                        client.cancel(job['job_id'])
                client.drain_node(args.node_id)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    last_status = client.status(); ledger.observe(last_status)
                    if not ledger.unresolved():
                        break
                    time.sleep(.5)
            except Exception as error:
                report['cleanup_error'] = type(error).__name__ + ': ' + str(error)
        # Ordinary stop after protocol drain; every directly owned service is
        # reaped. Supervisor death is not assumed to release any allocation.
        try:
            report['manager_samples'].append(manager_samples(services, output / 'agent-state', ledger))
        except Exception as error:
            report['measurement_cleanup_error'] = str(error)
        for name in ('agent', 'coordinator'):
            if name in services:
                try:
                    result = services[name].stop(grace=20)
                    result['descendant_contract'] = 'direct service reaped; allocation release requires separate protocol evidence'
                    report['cleanup'].append({'service': name, **result})
                except Exception as error:
                    report.setdefault('unresolved_owned_services', []).append({'name': name, 'error': str(error)})
                if name == 'agent' and client:
                    try:
                        last_status = client.status(); ledger.observe(last_status)
                    except Exception as error:
                        report['cleanup_error'] = 'final allocation state unavailable: ' + str(error)
        report['uncertain_allocations'] = ledger.unresolved()
        if report['uncertain_allocations'] or report.get('unresolved_owned_services') or report.get('cleanup_error'):
            report['status'] = 'failed_cleanup_requires_reconciliation'
        report['elapsed_seconds'] = time.monotonic() - started
        report['total_execution_seconds'] = report['elapsed_seconds'] + (prior['elapsed_seconds'] if prior else 0)
        if device == 'cpu' and report['elapsed_seconds'] > seconds:
            report['status'] = 'failed_stage_wall_envelope'
        report['peak_observed_family_rss_bytes'] = max((s['rss_bytes'] for s in report['family_rss_samples']), default=None)
        report['manager_metrics'] = manager_metrics(report['manager_samples'])
        if report.get('measurement_cleanup_error'):
            report['manager_metrics']['status'] = 'inconclusive'
            report['manager_metrics']['coverage_gaps'].append({'reason': 'final measurement unavailable'})
        if prior:
            segments = [prior['manager_metrics'], report['manager_metrics']]
            report['combined_manager_metrics'] = {'segments': segments,
                'status': 'passed' if all(segment['status'] == 'passed' for segment in segments) else 'not_passed',
                'scope': 'each execution segment reviewed independently; no counter delta across restarted services'}
        report['operational_completion'] = False
        try:
            atomic_json(output / 'report.json', report)
            if last_status:
                atomic_json(output / 'final-status.json', last_status)
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', default=str(Path.home() / '.local/share/resmgr-validation'))
    for name in ('run-id', 'node-id', 'candidate'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--gpu-uuid')
    parser.add_argument('--bootstrap')
    parser.add_argument('--binary', help='existing frozen home-local resmgr binary')
    parser.add_argument('--device', choices=['cpu', 'cuda:0'], default='cuda:0')
    parser.add_argument('--wall-seconds', type=int,
        help='approved CPU comparison total execution budget, including any resumed segment (default 900; maximum 1800)')
    parser.add_argument('--resume-report',
        help='original CPU comparison report stopped at its time limit; verify and reuse completed trials in fresh state')
    parser.add_argument('--stage', choices=['command', 'cooperative'], required=True)
    parser.add_argument('--execute', action='store_true')
    args = parser.parse_args()
    result = run(args)
    print(json.dumps(result, indent=2))
    return 0 if result['status'] == 'completed_pending_metric_review' else 1


if __name__ == '__main__':
    sys.exit(main())
