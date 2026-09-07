#!/usr/bin/env python3
"""Finite real application validation against two separately bootstrapped agents.

Run this controller on the guaranteed node. All job operations use the existing
mTLS API. The only owned connection fault is stopping this run's loopback byte
proxy; SSH, host networking, services and authentication are not changed here.
"""
import argparse
import copy
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from resmgr import Client, sha256_file
from connection_proxy import validate as validate_proxy
from soak import AllocationLedger, StatusJournal, anchor_fresh
from validation_runtime import OwnedProcess, atomic_json, home_executable, inside_home, local_guard


def validate(config, application):
    if config.get('execution_approved') is not True:
        raise ValueError('explicit bounded execution approval required')
    output = inside_home(config['output'])
    if not 240 <= config.get('wall_seconds', 1200) <= 1200:
        raise ValueError('wall limit must be in 240..1200 seconds')
    if config.get('cleanup_seconds', 120) != 120:
        raise ValueError('retain the 120-second cleanup reserve')
    if not 10 <= config.get('phase_seconds', 120) <= 120:
        raise ValueError('phase deadline must not exceed 120 seconds')
    if not 20 <= config.get('disconnect_seconds', 25) <= 60:
        raise ValueError('owned disconnection must last 20..60 seconds')
    if config.get('disconnect_seconds', 25) >= config.get('phase_seconds', 120):
        raise ValueError('reconnect must precede its phase deadline')
    nodes = application['nodes']
    if len(nodes) != 2 or len({node['node_id'] for node in nodes}) != 2:
        raise ValueError('exactly two explicitly identified nodes required')
    roles = {node['class']: node for node in nodes}
    if set(roles) != {'guaranteed', 'opportunistic'} or len(roles) != 2:
        raise ValueError('one guaranteed anchor and one opportunistic node required')
    if roles['guaranteed']['node_id'] != config['anchor_node_id'] or roles['opportunistic']['node_id'] != config['burst_node_id']:
        raise ValueError('configured node roles disagree')
    if application.get('actor_class') is not None or application.get('games') not in (6, 12):
        raise ValueError('six or twelve real games with original node roles required')
    for node in nodes:
        if node.get('device') != 'cpu' or node.get('gpu_uuid') or node.get('max_workers') != 1:
            raise ValueError('exactly one CPU actor per node; no GPU admission')
        if not 3 <= node.get('max_attempts', 0) <= 4:
            raise ValueError('three or four explicit attempts required for two bounded disruptions')
        for name in ('python', 'source_root', 'sdk_root', 'candidate'):
            if not Path(node[name]).is_absolute():
                raise ValueError('node-local executable and asset paths must be explicit')
    for key in ('model_sha256', 'snapshot_sha256'):
        values = {node.get(key) for node in nodes}
        if len(values) != 1 or not isinstance(next(iter(values)), str) or len(next(iter(values))) != 64:
            raise ValueError('all nodes require one pinned model and snapshot')
    if not application.get('experiment_id') or not application.get('run_seed'):
        raise ValueError('explicit experiment and seed identities required')
    if inside_home(application['output']) != output / 'application':
        raise ValueError('application outputs must use this fresh run directory')
    inside_home(application['client_config'])
    inside_home(config['bootstrap'])
    proxy = copy.deepcopy(config['proxy_config'])
    validate_proxy(proxy)
    if inside_home(proxy['output']) != output / 'proxy' or proxy['duration_seconds'] > config.get('wall_seconds', 1200):
        raise ValueError('owned proxy must share the run directory and lifetime')
    demand = config.get('cpu_demand')
    if demand is not None:
        cpus = demand.get('cpu_ids', [])
        digest = demand.get('pressure_script_sha256', '')
        if (not isinstance(cpus, list) or len(cpus) != 2 or len(set(cpus)) != 2
                or any(type(cpu) is not int or cpu < 0 for cpu in cpus)
                or not isinstance(digest, str) or len(digest) != 64
                or any(char not in '0123456789abcdef' for char in digest)):
            raise ValueError('CPU demand requires two explicit CPU IDs and the pinned pressure script hash')
    return roles


def same_identity(left, right):
    return (isinstance(left, dict) and isinstance(right, dict)
            and type(left.get('pid')) is int and left['pid'] > 0
            and type(left.get('start_ticks')) is int and left['start_ticks'] > 0
            and isinstance(left.get('boot_id'), str) and bool(left['boot_id'])
            and all(left.get(key) == right.get(key) for key in ('pid', 'boot_id', 'start_ticks')))


def finite_number(value):
    return type(value) in (int, float) and math.isfinite(value)


def pressure_receipt(started, completed, request, request_hash, contract, now):
    """Verify the outer owner's finite pressure receipt; never adopt its PID."""
    for proof in (started, completed):
        if (proof.get('schema_version') != 1 or proof.get('request_sha256') != request_hash
                or proof.get('experiment_id') != request['experiment_id']
                or proof.get('node_id') != request['node_id']
                or proof.get('cpu_ids') != contract['cpu_ids']
                or proof.get('pressure_script_sha256') != contract['pressure_script_sha256']
                or not same_identity(started.get('identity'), proof.get('identity'))
                or proof['identity']['boot_id'] != request['boot_id']):
            raise ValueError('CPU demand receipt run, node, identity, CPU, source or request hash mismatch')
    began, ended = started.get('started_unix'), completed.get('completed_unix')
    if (not finite_number(began) or not finite_number(ended) or not finite_number(now)
            or not request['requested_unix'] - 5 <= began <= ended <= now + 5
            or not 0 <= ended - began <= 40 or not 0 <= now - began <= 120):
        raise ValueError('CPU demand receipt is stale or outside its finite window')
    cleanup = completed.get('cleanup', {})
    if (not same_identity(started['identity'], cleanup) or cleanup.get('reaped') is not True
            or cleanup.get('exit_code') != 0 or cleanup.get('signals') != []):
        raise ValueError('CPU demand owner has not confirmed clean direct-process release')
    raw = completed.get('report_json')
    if (not isinstance(raw, str) or len(raw.encode()) > 16 * 1024**2
            or hashlib.sha256(raw.encode()).hexdigest() != completed.get('report_sha256')):
        raise ValueError('CPU demand raw report hash mismatch or report exceeds 16 MiB')
    report = json.loads(raw)
    if (report.get('status') != 'events_finished_review_required'
            or report.get('manager_registry_member') is not False or len(report.get('events', [])) != 1):
        raise ValueError('one completed unmanaged CPU pressure event is required')
    item = report['events'][0]
    event, child_cleanup, identity = item.get('event', {}), item.get('cleanup', {}), item.get('identity', {})
    if (event.get('id') != request['experiment_id'] + '.cpu-demand' or event.get('mode') != 'cpu'
            or event.get('duration_seconds') != 20 or event.get('at_seconds') != 0
            or not same_identity(identity, child_cleanup) or identity['boot_id'] != request['boot_id']
            or child_cleanup.get('reaped') is not True or child_cleanup.get('exit_code') != 0
            or child_cleanup.get('signals') != []):
        raise ValueError('CPU pressure event identity, bounded schedule or child release mismatch')
    child = item.get('event_report', {})
    if child.get('status') != 'events_finished_review_required' or len(child.get('events', [])) != 1:
        raise ValueError('CPU pressure child did not finish its event')
    actual = child['events'][0]
    results = actual.get('results', [])
    demand_start = actual.get('demand_started_unix')
    demand_monotonic = actual.get('demand_started_monotonic')
    elapsed = actual.get('elapsed_seconds')
    if (actual.get('id') != event['id'] or actual.get('mode') != 'cpu' or actual.get('status') != 'completed'
            or actual.get('managed_registry_member') is not False or actual.get('threads_reaped') is not True
            or not same_identity(identity, actual.get('identity'))
            or len(results) != 2 or {row.get('cpu_id') for row in results} != set(contract['cpu_ids'])
            or any(type(row.get('completed_units')) is not int or row['completed_units'] <= 0 for row in results)
            or not finite_number(demand_start) or not finite_number(demand_monotonic) or not finite_number(elapsed)
            or not began - 5 <= demand_start <= ended or not 19 <= elapsed <= 25
            or demand_start + elapsed > ended + 5):
        raise ValueError('CPU pressure work, freshness or thread release evidence is incomplete')
    first_work, last_work = [], []
    for row in results:
        requests = row.get('requests', [])
        if not isinstance(requests, list) or len(requests) != row['completed_units']:
            raise ValueError('CPU pressure completed work has no matching request timestamps')
        previous_end = demand_monotonic
        for work in requests:
            work_start, work_end = work.get('started_monotonic'), work.get('finished_monotonic')
            if (not finite_number(work_start) or not finite_number(work_end)
                    or not previous_end <= work_start <= work_end <= demand_monotonic + 20):
                raise ValueError('CPU pressure request timestamps are unordered or outside the approved window')
            previous_end = work_end
        first_work.append(requests[0]['started_monotonic'])
        last_work.append(requests[-1]['finished_monotonic'])
    verified_start = demand_start + max(first_work) - demand_monotonic
    verified_end = demand_start + min(last_work) - demand_monotonic
    if verified_end - verified_start <= 10:
        raise ValueError('CPU pressure has insufficient common work interval for bounded clock skew')
    return {'demand_started_unix': verified_start, 'demand_ended_unix': verified_end,
            'window_basis': 'intersection_of_both_cpus_completed_request_spans', 'clock_skew_seconds_max': 5,
            'report_sha256': completed['report_sha256'], 'identity': identity,
            'owner_identity': started['identity'], 'cpu_ids': contract['cpu_ids']}


class Phases:
    """Evidence-driven bounded scenario; dispatch alone never counts as an effect."""
    def __init__(self, anchor, burst, *, phase_seconds=120, disconnect_seconds=25, cpu_demand=False):
        self.anchor, self.burst = anchor, burst
        self.limit, self.offline = phase_seconds, disconnect_seconds
        self.phase, self.since = 'normal', 0.0
        self.events = []
        self.target = None
        self.anchor_before = set()
        self.disconnected_assignment = None
        self.release_received = False
        self.cpu_demand = cpu_demand
        self.demand_trace = []
        self.anchor_seen = set()

    def transition(self, phase, now, **detail):
        self.events.append({'phase': phase, 'elapsed_seconds': now, **detail})
        self.phase, self.since = phase, now

    def update(self, status, ledger, accepted, now, pressure=None, observed_unix=None):
        tasks = {task['task_id']: task for task in status.get('tasks', [])}
        anchored = {key for key, value in accepted.items() if value['node_id'] == self.anchor}
        contributed = {value['node_id'] for value in accepted.values()}
        live = [task for task in tasks.values()
                if task.get('assignment_id') in ledger.allocations
                and ledger.allocations[task['assignment_id']]['node_id'] == self.burst
                and ledger.allocations[task['assignment_id']]['phase'] == 'running'
                and task['status'] != 'completed']
        if self.phase not in ('normal', 'done') and now - self.since > self.limit:
            raise TimeoutError('required scenario exceeded phase deadline: ' + self.phase)
        if self.phase == 'normal' and contributed == {self.anchor, self.burst} and live:
            self.target = copy.deepcopy(live[0])
            self.anchor_before = anchored
            self.anchor_seen = set(anchored)
            self.transition('cpu_demand' if self.cpu_demand else 'draining', now, task_id=self.target['task_id'],
                            assignment_id=self.target['assignment_id'], generation=self.target['generation'])
            return 'pressure_ready' if self.cpu_demand else 'drain'
        if self.phase == 'cpu_demand':
            if not finite_number(observed_unix):
                raise ValueError('CPU demand requires an actual controller observation timestamp')
            newly_anchored = anchored - self.anchor_seen
            self.anchor_seen.update(anchored)
            node = next((node for node in status.get('nodes', []) if node['report']['node_id'] == self.burst), None)
            if node is not None:
                if node.get('drain') is not False:
                    raise RuntimeError('explicit node drain cannot establish automatic CPU contraction')
                report = node['report']
                burst_observed = report['observed_at_unix_ms'] / 1000
                if not finite_number(burst_observed) or abs(observed_unix - burst_observed) > 5:
                    raise RuntimeError('CPU demand requires fresh burst telemetry within five seconds of observation')
                current = tasks.get(self.target['task_id'])
                old = ledger.allocations.get(self.target['assignment_id'])
                reported = next((row for row in report.get('allocations', [])
                                 if row.get('assignment_id') == self.target['assignment_id']
                                 and row.get('generation') == self.target['generation']), None)
                if old and old.get('present_in_latest_status') and current:
                    self.demand_trace.append({'observed_unix': burst_observed, 'controller_observed_unix': observed_unix,
                        'boot_id': report['boot_id'], 'cpu_millicores': report['managed_budget']['cpu_millicores'],
                        'phase': old['phase'], 'reported_phase': (reported or {}).get('phase'),
                        'task_status': current['status'],
                        'anchor_progress': sorted(newly_anchored)})
                    if len(self.demand_trace) > 512:
                        raise RuntimeError('CPU demand observations exceeded bounded phase inventory')
            if pressure is not None:
                causal = [row for row in self.demand_trace
                          if row['boot_id'] == pressure['identity']['boot_id']
                          and pressure['demand_started_unix'] <= row['observed_unix'] <= pressure['demand_ended_unix']
                          and row['cpu_millicores'] < 1000 and row['phase'] == 'draining'
                          and row['reported_phase'] == 'draining'
                          and row['task_status'] != 'completed']
                old = ledger.allocations.get(self.target['assignment_id'])
                progress = [row for row in self.demand_trace if row['anchor_progress']
                            and row['boot_id'] == pressure['identity']['boot_id']
                            and pressure['demand_started_unix'] <= row['observed_unix'] <= pressure['demand_ended_unix']
                            and pressure['demand_started_unix'] + 5 <= row['controller_observed_unix'] <= pressure['demand_ended_unix'] - 5]
                if causal and progress and old and old.get('present_in_latest_status') and old['phase'] == 'released':
                    self.transition('resuming', now, automatic_cpu_demand=True, pressure=pressure,
                                    draining_observation=causal[0], released_assignment=self.target['assignment_id'],
                                    anchor_progress=progress[0]['anchor_progress'])
                    # The policy resumes after external demand ends; no drain/resume RPC.
            return None
        if self.phase == 'draining':
            released = all(row['phase'] == 'released' for row in ledger.allocations.values() if row['node_id'] == self.burst)
            if released and anchored - self.anchor_before:
                self.transition('resuming', now, anchor_progress=sorted(anchored - self.anchor_before),
                                released_assignments=[key for key, row in ledger.allocations.items() if row['node_id'] == self.burst])
                return 'resume'
        if self.phase == 'resuming':
            current = next((task for task in live if task['task_id'] == self.target['task_id']), None)
            if current and current['generation'] > self.target['generation']:
                self.disconnected_assignment = current['assignment_id']
                self.disconnected_generation = current['generation']
                self.anchor_before = anchored
                self.transition('disconnected', now, task_id=current['task_id'], assignment_id=current['assignment_id'],
                                generation=current['generation'])
                return 'proxy_stop'
        if self.phase == 'disconnected':
            row = ledger.allocations.get(self.disconnected_assignment)
            if row is None or not row.get('present_in_latest_status'):
                raise RuntimeError('disconnected reservation disappeared without release evidence')
            # A genuine release received before the owned connection closed is
            # legitimate; record it instead of manufacturing an uncertain phase.
            if row['phase'] == 'released':
                self.release_received = True
            if now - self.since >= self.offline:
                self.transition('rejoining', now, held_phase=row['phase'],
                                reservation_present=True, anchor_progress=sorted(anchored - self.anchor_before))
                return 'proxy_start'
        if self.phase == 'rejoining':
            current = tasks.get(self.target['task_id'])
            old = ledger.allocations.get(self.disconnected_assignment)
            if current and current['status'] == 'completed' and old and old['phase'] == 'released':
                # If the first task legitimately finished before connection loss,
                # it cannot demonstrate a retry. Do not mark that scenario passed.
                if current['generation'] <= self.disconnected_generation:
                    raise RuntimeError('disconnected task completed without a later fenced attempt; scenario inconclusive')
                self.transition('done', now, task_id=current['task_id'], generation=current['generation'],
                                released_assignment=self.disconnected_assignment)
        return None

    def require_done(self):
        if self.phase != 'done':
            raise RuntimeError('finite real cohort finished before required scenarios: ' + self.phase)


def accepted_receipts(client, status, ledger, experiment, previous=None):
    accepted = dict(previous or {})
    for task in status.get('tasks', []):
        if not task['task_id'].startswith(experiment + '.') or task['status'] != 'completed':
            continue
        old = accepted.get(task['task_id'])
        if old is not None:
            if old['assignment_id'] != task.get('assignment_id') or old['generation'] != task['generation'] or old['receipt_hash'] != task.get('receipt_hash'):
                raise RuntimeError('previously accepted task identity or receipt changed')
            continue
        receipt = client.result(task['task_id'])
        if not receipt:
            raise RuntimeError('completed task has no accepted receipt')
        metadata = receipt['result']['metadata']
        if metadata.get('experiment_id') != experiment:
            raise RuntimeError('accepted experiment identity mismatch')
        allocation = ledger.allocations.get(receipt['assignment_id'])
        if allocation is None or allocation.get('generation') != receipt['generation']:
            raise RuntimeError('accepted attempt lacks an authenticated allocation identity')
        accepted[task['task_id']] = {'node_id': allocation['node_id'], 'assignment_id': receipt['assignment_id'],
                                   'generation': receipt['generation'], 'receipt_hash': task.get('receipt_hash'),
                                   'model_sha256': metadata.get('model_sha256')}
    return accepted


def review_application(output, application, status, ledger):
    root = output / 'application'
    result = json.loads((root / 'cycle/result.json').read_text())
    if result['experiment_id'] != application['experiment_id'] or result['accepted_games'] != application['games'] or result['continuation'] != 'optimizer_step_cursor_v2':
        raise ValueError('real cycle identity, game count, or checkpoint continuation mismatch')
    from integration.resmgr.workflow import logical_identity
    planned = {logical_identity(application['experiment_id'], application.get('generation', 0), index,
                                application['run_seed'])['logical_task_id']:
               logical_identity(application['experiment_id'], application.get('generation', 0), index, application['run_seed'])
               for index in range(application['games'])}
    tasks = {task['task_id']: task for task in status['tasks']}
    games = []
    for path in sorted((root / 'receipts').glob('*.json')):
        receipt = json.loads(path.read_text()); submission = receipt['submission']; metadata = submission['result']['metadata']
        expected = planned[receipt['logical_task_id']]
        if any(metadata.get(key) != expected[key] for key in ('experiment_id', 'logical_task_id', 'episode_id', 'generation', 'seed')):
            raise ValueError('accepted game identity differs from planned cohort')
        allocation = ledger.allocations[submission['assignment_id']]
        task = tasks[submission['task_id']]
        if task['status'] != 'completed' or task['generation'] != submission['generation'] or task['assignment_id'] != submission['assignment_id']:
            raise ValueError('stale game attempt receipt')
        model = metadata['result']
        if model['backend'] != 'torchscript' or model['device'] != 'cpu' or model['inference_count'] <= 0 or model['inference_errors']:
            raise ValueError('real CPU model inference evidence absent or failed')
        replay = inside_home(receipt['replay']); replay.relative_to(root)
        if sha256_file(replay) != model['replay_sha256']:
            raise ValueError('accepted replay integrity mismatch')
        with gzip.open(replay, 'rt') as stream:
            steps = len(json.load(stream)['steps'])
        if not steps:
            raise ValueError('empty real replay')
        games.append({'task_id': submission['task_id'], 'logical_task_id': receipt['logical_task_id'],
                      'node_id': allocation['node_id'], 'generation': submission['generation'],
                      'model_sha256': metadata['model_sha256'], 'steps': steps, 'replay_sha256': model['replay_sha256']})
    if len(games) != len(planned) or len({game['logical_task_id'] for game in games}) != len(planned):
        raise ValueError('incomplete or duplicate logical result cohort')
    nodes = sorted({game['node_id'] for game in games})
    if nodes != sorted(node['node_id'] for node in application['nodes']) or result['contributing_nodes'] != nodes:
        raise ValueError('both actual nodes did not contribute useful results')
    if {game['model_sha256'] for game in games} != {application['nodes'][0]['model_sha256']}:
        raise ValueError('accepted games used an unexpected model')
    import torch
    checkpoints = []
    previous = None
    for label, step in [('learn-first', 2), ('learn-resumed', 4)]:
        folder = root / 'cycle' / label
        receipt = json.loads((folder / 'receipt.json').read_text())
        item = next(item for item in receipt['result']['artifacts'] if item['name'] == 'learner.pt')
        path = folder / 'latest.pt'
        if sha256_file(path) != item['sha256'] or path.stat().st_size != item['size']:
            raise ValueError('learner publication integrity mismatch')
        learner = ledger.allocations[receipt['assignment_id']]
        anchor = next(node['node_id'] for node in application['nodes'] if node['class'] == 'guaranteed')
        if learner['node_id'] != anchor or learner['generation'] != receipt['generation']:
            raise ValueError('learner did not execute as an identified anchor allocation')
        checkpoint = torch.load(path, map_location='cpu', weights_only=False)
        contract = checkpoint['resume_v2']['contract']
        if checkpoint['global_step'] != step or checkpoint['checkpoint_schema'] != 2 or contract['experiment_id'] != application['experiment_id']:
            raise ValueError('actual managed checkpoint continuation failed')
        if previous is not None and previous != contract:
            raise ValueError('checkpoint resume contract changed')
        previous = contract
        checkpoints.append({'stage': label, 'step': step, 'sha256': item['sha256']})
    return {'status': 'passed', 'games': games, 'checkpoints': checkpoints, 'cycle': result}


def run(config):
    application = json.loads(inside_home(config['application_config']).read_text())
    roles = validate(config, application)
    if sys.platform != 'linux':
        raise RuntimeError('controller must run on the qualified Linux anchor')
    python = home_executable(config['python'])
    source = inside_home(roles['guaranteed']['source_root'])
    sys.path.insert(0, str(source))
    output = inside_home(config['output']); output.mkdir(parents=True, exist_ok=False)
    work_seconds = config.get('wall_seconds', 1200) - 120
    start = time.monotonic()
    application = copy.deepcopy(application)
    application.update(wall_time_seconds=work_seconds, learning_wall_time_seconds=120,
                       stop_deadline_unix=time.time() + work_seconds, stop_file=str(output / 'STOP'),
                       opportunistic_task_timeout_seconds=work_seconds, max_replacements_per_game=0,
                       poll_seconds=.5, max_output_bytes=2 * 1024**3)
    application['learner_ram_mib'] = min(application.get('learner_ram_mib', 2048), 8192)
    atomic_json(output / 'application.json', application)
    atomic_json(output / 'config.json', config)
    client = Client.from_config(application['client_config'])
    ledger = AllocationLedger([config['anchor_node_id'], config['burst_node_id']])
    phases = Phases(config['anchor_node_id'], config['burst_node_id'],
                    phase_seconds=config.get('phase_seconds', 120), disconnect_seconds=config.get('disconnect_seconds', 25),
                    cpu_demand=config.get('cpu_demand') is not None)
    report = {'schema_version': 1, 'status': 'running', 'experiment_id': application['experiment_id'],
              'started_unix': time.time(), 'targets': {'phase_seconds_max': phases.limit, 'wall_seconds_max': work_seconds + 120},
              'games': application['games'], 'actual_24_hour_soak': False, 'events': phases.events,
              'node_profiles': application['nodes'], 'cleanup': [], 'scope': 'two Linux agents on explicitly bootstrapped hosts; CPU only'}
    stopped = []; previous = {sig: signal.signal(sig, lambda sig, frame: stopped.append(sig)) for sig in (signal.SIGINT, signal.SIGTERM)}
    proxy = None; cycle = None; latest = None; accepted = {}; journal = StatusJournal(); instance = 0
    pressure_request = None; pressure_hash = None; pressure = None; pressure_files = None
    private_verified = False
    env = dict(os.environ, TMPDIR=str(output), XDG_CACHE_HOME=str(output / 'cache'),
               PYTHONPATH=os.pathsep.join([str(inside_home(roles['guaranteed']['sdk_root'])), str(source)]),
               PYTHONDONTWRITEBYTECODE='1', OMP_NUM_THREADS='1', MKL_NUM_THREADS='1', CUDA_VISIBLE_DEVICES='')
    def start_proxy():
        nonlocal instance
        instance += 1
        value = copy.deepcopy(config['proxy_config'])
        value['duration_seconds'] = max(1, config.get('wall_seconds', 1200) - (time.monotonic() - start))
        value['output'] = str(output / 'proxy' / f'instance-{instance}')
        atomic_json(output / f'proxy-{instance}.json', value)
        return OwnedProcess([str(python), str(Path(__file__).with_name('connection_proxy.py')), '--config',
                             str(output / f'proxy-{instance}.json'), '--execute'], source, output / f'proxy-{instance}.log', env)
    try:
        proxy = start_proxy()
        deadline = time.monotonic() + 30
        while True:
            latest = client.status(); ledger.observe(latest)
            if latest.get('tasks') or latest.get('jobs') or ledger.unresolved():
                raise RuntimeError('validation requires fresh private services with no unrelated allocations or jobs')
            node_ids = {node['report']['node_id'] for node in latest.get('nodes', [])}
            if node_ids == ledger.nodes:
                boots = {node['report'].get('boot_id') for node in latest['nodes']}
                if len(boots) != 2 or None in boots:
                    raise RuntimeError('distinct runtime boot identities required; two aliases on one host are insufficient')
                report['node_boot_ids'] = {node['report']['node_id']: node['report']['boot_id'] for node in latest['nodes']}
                private_verified = True
                break
            if time.monotonic() > deadline:
                raise TimeoutError('two agents did not register through their approved transports')
            time.sleep(.5)
        argv = [str(python), '-m', 'integration.resmgr', 'cycle', '--config', str(output / 'application.json'),
                '--bootstrap', str(inside_home(config['bootstrap'])), '--steps', '2', '--device', 'cpu']
        report['cycle_argv'] = argv
        cycle = OwnedProcess(argv, source, output / 'cycle.log', env)
        with (output / 'observations.jsonl').open('x') as observations:
            while True:
                elapsed = time.monotonic() - start
                if stopped or (output / 'STOP').exists():
                    raise InterruptedError('operator stop requested')
                if elapsed > work_seconds:
                    raise TimeoutError('whole finite validation work deadline reached')
                if proxy is not None and proxy.poll() is not None:
                    raise RuntimeError('owned transport proxy exited unexpectedly')
                latest = client.status(); ledger.observe(latest)
                if any(not task['task_id'].startswith(application['experiment_id'] + '.') for task in latest.get('tasks', [])):
                    raise RuntimeError('unrelated task appeared; refuse node-wide fault actions')
                accepted = accepted_receipts(client, latest, ledger, application['experiment_id'], accepted)
                guard = local_guard(output, {'min_ram_free_bytes': 16 * 1024**3, 'min_disk_free_bytes': 16 * 1024**3,
                                            'max_output_bytes': 4 * 1024**3})
                from local_smoke import family_memory
                live_owned = [child.child.pid for child in (cycle, proxy) if child is not None and child.poll() is None]
                family = family_memory(live_owned)
                guard['owned_controller_family_rss_bytes'] = sum(item['rss_bytes'] for item in family)
                if guard['owned_controller_family_rss_bytes'] > 4 * 1024**3:
                    guard['errors'].append('owned_controller_family_RSS_exceeds_4GiB')
                if guard['errors']:
                    raise RuntimeError('anchor guard failed: ' + ', '.join(guard['errors']))
                if not anchor_fresh(latest, config['anchor_node_id'], time.time(), 30):
                    raise RuntimeError('guaranteed anchor telemetry unavailable or stale')
                if phases.phase == 'cpu_demand' and pressure is None:
                    started_path, completed_path = output / 'cpu-demand-started.json', output / 'cpu-demand-completed.json'
                    if started_path.exists() and completed_path.exists():
                        raw_proofs = {}
                        for path in (started_path, completed_path):
                            if path.is_symlink() or not path.is_file() or inside_home(path) != path or path.stat().st_size > 16 * 1024**2:
                                raise ValueError('CPU demand proof must be a bounded regular file in the current output')
                            with path.open('rb') as stream:
                                raw_proofs[path] = stream.read(16 * 1024**2 + 1)
                            if len(raw_proofs[path]) > 16 * 1024**2:
                                raise ValueError('CPU demand proof grew beyond its read bound')
                        pressure = pressure_receipt(json.loads(raw_proofs[started_path]), json.loads(raw_proofs[completed_path]),
                            pressure_request, pressure_hash, config['cpu_demand'], time.time())
                        pressure_files = {str(path): hashlib.sha256(raw).hexdigest() for path, raw in raw_proofs.items()}
                        report['cpu_demand_receipt_files'] = pressure_files
                action = phases.update(latest, ledger, accepted, elapsed, pressure=pressure, observed_unix=time.time())
                if action == 'pressure_ready':
                    burst_report = next(node['report'] for node in latest['nodes'] if node['report']['node_id'] == config['burst_node_id'])
                    pressure_request = {'schema_version': 1, 'experiment_id': application['experiment_id'],
                        'node_id': config['burst_node_id'], 'boot_id': burst_report['boot_id'],
                        'requested_unix': time.time(), 'target': phases.target, 'cpu_demand': config['cpu_demand'],
                        'status': 'pressure_requested_not_started', 'duration_seconds': 20,
                        'owner_contract': 'Existing outer owner must start, observe, stop/reap pressure and supply its exact hashed receipts; request creation is not readiness or completion.'}
                    atomic_json(output / 'cpu-demand-ready.json', pressure_request)
                    pressure_hash = sha256_file(output / 'cpu-demand-ready.json')
                    report['cpu_demand_request_sha256'] = pressure_hash
                if action in ('drain', 'resume'):
                    client.drain_node(config['burst_node_id'], action == 'drain')
                elif action == 'proxy_stop':
                    report['cleanup'].append({'stage': 'owned_partition', **proxy.stop()}); proxy = None
                elif action == 'proxy_start':
                    proxy = start_proxy()
                observations.write(json.dumps({'elapsed_seconds': elapsed, 'phase': phases.phase,
                                               **journal.encode(latest, elapsed), 'guard': guard}) + '\n')
                observations.flush()
                report.update(elapsed_seconds=elapsed, phase=phases.phase, accepted=accepted)
                atomic_json(output / 'report.json', report)
                code = cycle.poll()
                if code is not None:
                    report['cleanup'].append({'stage': 'cycle', **cycle.stop()}); cycle = None
                    if code != 0:
                        raise RuntimeError(f'real workflow failed with exit {code}')
                    phases.require_done()
                    break
                time.sleep(.5)
        report['application_review'] = review_application(output, application, latest, ledger)
        if phases.cpu_demand:
            if pressure_files is None or any(sha256_file(path) != digest for path, digest in pressure_files.items()):
                raise RuntimeError('CPU demand evidence is absent or changed before application review')
            if sha256_file(output / 'cpu-demand-ready.json') != pressure_hash:
                raise RuntimeError('CPU demand request changed after the owned pressure contract was issued')
        report['status'] = 'passed_pending_cleanup'
    except BaseException as error:
        report.update(status='failed', error=type(error).__name__ + ': ' + str(error))
    finally:
        (output / 'STOP').touch()
        if pressure_request is not None and pressure is None:
            report['cleanup'].append({'stage': 'external_cpu_demand',
                'error': 'The outer owner must finish pressure cleanup and retain its report; no completed ownership receipt was accepted.'})
        # Restore only our connection path so the remote agent can report release.
        if proxy is None:
            try: proxy = start_proxy()
            except Exception as error: report['cleanup'].append({'stage': 'restore_owned_transport', 'error': str(error)})
        try:
            latest = client.status(); ledger.observe(latest)
            if private_verified:
                for job in latest.get('jobs', []):
                    if job['job_id'].startswith(application['experiment_id'] + '.'):
                        client.cancel(job['job_id'])
            unrelated = [task for task in latest.get('tasks', []) if not task['task_id'].startswith(application['experiment_id'] + '.')]
            if private_verified and not unrelated:
                for node in ledger.nodes:
                    client.drain_node(node)
            elif unrelated:
                report['cleanup'].append({'stage': 'node_drain_refused', 'error': 'unrelated task appeared in private deployment; only owned jobs cancelled'})
        except Exception as error:
            report['cleanup'].append({'stage': 'cancel_and_drain', 'error': str(error)})
        if cycle is not None:
            try: report['cleanup'].append({'stage': 'interrupted_cycle', **cycle.stop()})
            except Exception as error: report['cleanup'].append({'stage': 'interrupted_cycle', 'error': str(error)})
        deadline = min(start + config.get('wall_seconds', 1200), time.monotonic() + 100)
        release_observed = False
        while time.monotonic() < deadline:
            try:
                latest = client.status(); ledger.observe(latest)
                if not ledger.unresolved(): release_observed = True; break
            except Exception:
                pass
            time.sleep(.5)
        if proxy is not None:
            try: report['cleanup'].append({'stage': 'proxy', **proxy.stop()})
            except Exception as error: report['cleanup'].append({'stage': 'proxy', 'error': str(error)})
        report.update(elapsed_seconds=time.monotonic() - start, unresolved_allocations=ledger.unresolved(),
                      release_observed=release_observed, services_cleanup='external bootstrap owner must stop and verify both agents/coordinator')
        if latest is not None: atomic_json(output / 'final-status.json', latest)
        if report['status'] == 'passed_pending_cleanup' and release_observed and not any('error' in item or not item.get('reaped', False) for item in report['cleanup']):
            report['status'] = 'passed_two_node_application_scenarios'
        atomic_json(output / 'report.json', report)
        for sig, handler in previous.items(): signal.signal(sig, handler)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True); parser.add_argument('--execute', action='store_true')
    args = parser.parse_args(); config = json.loads(inside_home(args.config).read_text())
    if not args.execute:
        application = json.loads(inside_home(config['application_config']).read_text()); validate(config, application)
        print(json.dumps({'mode': 'plan_only', 'config': config}, indent=2)); return 0
    result = run(config)
    print(json.dumps({key: result.get(key) for key in ['status', 'error', 'phase', 'elapsed_seconds', 'unresolved_allocations']}, indent=2))
    return 0 if result['status'] == 'passed_two_node_application_scenarios' else 1


if __name__ == '__main__':
    sys.exit(main())
