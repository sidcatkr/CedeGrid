#!/usr/bin/env python3
"""Feature 004's host gates (quickstart section 7) on a scope of four physical CPUs, with the agent
started directly and the coordinator and this harness outside the scope.

  fill  an idle scope: eight own CPU-bound tasks of 500 m; admission sampled every 0.25 s.
  f2c   eight own CPU-bound tasks of 1000 m while a second account runs two saturating loops
        tied to two of the scope's CPUs.
  gpu   two own GPU tasks (1024x1024 multiplies) while a second account saturates the same GPU
        from a CPU outside the scope.

Arms: alone (the second account's load with no own work), plain (own tasks started by hand on the
scope without CedeGrid, the harm baseline; --plain-tasks sets how many), performance, aggressive
(protected) and unprotected (Aggressive with the host's allowance turned on through setup on a
terminal, and both SDK fields set). For f2c and gpu the harness writes schedule.json, from which the
operator starts the second account's load at its epoch, then report.json. It acts only on
processes it starts, and prints one summary line."""
import argparse
import json
import os
import pty
import select
import subprocess
import sys
import time
import uuid
from pathlib import Path

CPU_LOOP = r'''
import hashlib, json, os, sys, time
end, path = float(sys.argv[1]), sys.argv[2]
open(path + '.pid', 'w').write(json.dumps({'pid': os.getpid(), 'started': time.time()}))
units = 0; cpu0 = time.process_time(); t0 = time.time()
while time.time() < end:
    hashlib.pbkdf2_hmac('sha256', b'cedegrid-004', b'fixed-seed', 1500); units += 1
json.dump({'units': units, 'cpu_seconds': time.process_time() - cpu0, 'seconds': time.time() - t0,
           'started': t0}, open(path, 'w'))
'''
GPU_LOOP = r'''
import json, os, sys, time
end, path = float(sys.argv[1]), sys.argv[2]
open(path + '.pid', 'w').write(json.dumps({'pid': os.getpid(), 'started': time.time()}))
import torch
a = torch.full((1024, 1024), .001, device='cuda:0'); b = torch.full_like(a, .002); c = torch.empty_like(a)
torch.cuda.synchronize()
units = 0; t0 = time.time()
while time.time() < end:
    torch.mm(a, b, out=c); torch.cuda.synchronize(); units += 1
json.dump({'units': units, 'cpu_seconds': time.process_time(), 'seconds': time.time() - t0, 'started': t0},
          open(path, 'w'))
'''
SHAPES = {'fill': (8, 500), 'f2c': (8, 1000), 'gpu': (2, 500)}
PHRASE = "slow other users' compute on this host"


def peer_summary(requests, window):
    """The second account's rate and p99 over the window's steady part (10 s after own work's
    submission, which comes 5 s after the epoch), from its (start offset, latency ms) rows."""
    steady = sorted(latency for start, latency in requests if 15 <= start < 5 + window)
    if not steady:
        return {'rate': 0.0, 'p99_ms': None}
    return {'rate': round(len(steady) / (window - 10), 2),
            'p99_ms': steady[int(.99 * (len(steady) - 1))]}


def admitted_at(samples, times):
    """How many own tasks were admitted at each of `times` seconds after submission."""
    return [next((count for at, count, _ in samples if at >= t), None) for t in times]


def allow(binary, config):
    """Turns the allowance on the way a person does: setup on a terminal, yes, then the phrase."""
    master, slave = pty.openpty()
    proc = subprocess.Popen([str(binary), '--config', str(config), 'setup', '--allow-unprotected'],
                            stdin=slave, stdout=slave, stderr=slave, close_fds=True,
                            env={**os.environ, 'TERM': 'xterm', 'NO_COLOR': '1'})
    os.close(slave)
    seen = b''

    def until(text, timeout=30):
        nonlocal seen
        end = time.time() + timeout
        while text.encode() not in seen and time.time() < end:
            if select.select([master], [], [], .1)[0]:
                try:
                    seen += os.read(master, 65536)
                except OSError:
                    break
    until('[y/n]'); os.write(master, b'y\r')
    until('to confirm:'); os.write(master, PHRASE.encode() + b'\r')
    until('allowed on this host', 15)
    proc.wait(timeout=30)
    return proc.returncode


class Cluster:
    """A coordinator and an agent for one run; the agent on `cpus`, everything else outside."""

    def __init__(self, repo, out, cpus, mode, workers, lifetime, gpu_uuid=None):
        from anchor_validation import deployment
        from runtime_config import atomic_runtime_config, read_runtime_config
        from validation_runtime import OwnedProcess
        self.repo, self.out, self.cpus = Path(repo), Path(out), list(cpus)
        self.binary = self.repo / 'target/release/cedegrid'
        self.env = {**os.environ}
        self.worker_env = {'PYTHONNOUSERSITE': '1', 'PYTHONPATH': str(self.repo / 'python'),
                           'PYTHONDONTWRITEBYTECODE': '1', 'TMPDIR': os.environ.get('TMPDIR', '/tmp')}
        outside = [c for c in sorted(os.sched_getaffinity(0)) if c not in self.cpus][-2:]
        os.sched_setaffinity(0, outside)
        self.node_id = 'aggressive-gate-' + uuid.uuid4().hex
        self.run_id = 'aggressive-gate-' + uuid.uuid4().hex
        self.jobs = []
        self.client = deployment(self.out, self.binary, self.cpus, self.node_id, gpu_uuid or '', lifetime,
                                 self.env, device='gpu' if gpu_uuid else 'cpu')
        node = read_runtime_config(self.out / 'node.toml')
        node['node_mode'] = 'opportunistic'
        node['cpu'].pop('reserve_physical_cores', None)
        node['contention'] = {'default_mode': mode}
        if mode == 'aggressive':
            node['contention'].update(max_mode='aggressive', acknowledge_caution=['aggressive_mode'])
        atomic_runtime_config(self.out / 'node.toml', node, 'node')
        agent = read_runtime_config(self.out / 'agent.toml')
        agent.update(max_workers=workers, capacity={'cpu_millicores': len(self.cpus) * 1000,
                     'ram_mib': max(4096, workers * 1024 * 2),
                     'gpu_memory_mib': {gpu_uuid: 2048} if gpu_uuid else {}})
        atomic_runtime_config(self.out / 'agent.toml', agent, 'agent')
        self.services = {'coordinator': OwnedProcess(
            [str(self.binary), 'coordinator', '--deployment', str(self.out / 'coordinator.toml')],
            self.repo, self.out / 'coordinator.log', self.env)}
        deadline = time.monotonic() + 15
        while True:
            try:
                self.client.status()
                break
            except Exception:
                if time.monotonic() > deadline:
                    raise
                time.sleep(.1)
        self.services['agent'] = OwnedProcess(
            ['taskset', '-c', ','.join(map(str, self.cpus)), str(self.binary), '--config',
             str(self.out / 'node.toml'), 'agent', '--deployment', str(self.out / 'agent.toml')],
            self.repo, self.out / 'agent.log', self.env)
        deadline = time.monotonic() + 30
        while not self.client.status().get('nodes'):
            if time.monotonic() > deadline:
                raise TimeoutError('agent did not register')
            time.sleep(.2)
        self.client.put_pool(self.run_id, [self.node_id], max_workers=workers, min_workers=0,
                             allocation_class='opportunistic')

    def check(self):
        for name, child in self.services.items():
            if child.poll() is not None:
                raise RuntimeError(f'{name} exited')

    def decisions(self, after_unix):
        import sqlite3
        uri = (self.out / 'agent-state/state.sqlite3').resolve().as_uri() + '?mode=ro'
        with sqlite3.connect(uri, uri=True) as db:
            return [(at / 1000, json.loads(d)) for at, d in db.execute(
                'SELECT observed_at_unix_ms, decision_json FROM observations WHERE observed_at_unix_ms>=? ORDER BY id',
                (int(after_unix * 1000),))]

    def stop(self):
        """Cancels the run's jobs and waits for its work to end before stopping the agent: an agent
        stopped by signal leaves running work to the next agent."""
        for job in self.jobs:
            try:
                self.client.cancel(job)
            except Exception as error:
                print(f'cancel {job}: {error!r}', file=sys.stderr)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline and any(
                t['status'] == 'assigned' for j in self.jobs for t in self.client.status(j).get('tasks', [])):
            time.sleep(.5)
        result = {}
        for name in ['agent', 'coordinator']:
            try:
                result[name] = self.services[name].stop(grace=20)
            except Exception as error:
                result[name] = repr(error)
        return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--repo', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--python', required=True, help='the Python own tasks run with (torch for gpu)')
    p.add_argument('--shape', choices=sorted(SHAPES), required=True)
    p.add_argument('--arm', choices=['alone', 'plain', 'performance', 'aggressive', 'unprotected'], required=True)
    p.add_argument('--plain-tasks', type=int, help="own tasks the plain arm starts (default: the shape's)")
    p.add_argument('--window', type=int, default=60)
    p.add_argument('--lead', type=int, default=40)
    a = p.parse_args()
    if a.shape == 'fill' and a.arm in ('alone', 'plain', 'unprotected'):
        p.error('fill runs performance or aggressive')
    if a.plain_tasks is not None and (a.arm != 'plain' or a.plain_tasks < 1):
        p.error('--plain-tasks takes a positive count, with --arm plain')
    repo = Path(a.repo).resolve()
    out = Path(a.output).resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    for sub in ('tools', 'python'):
        sys.path.insert(0, str(repo / sub))
    from native_validation import physical_cpus
    from validation_runtime import atomic_json
    from cedegrid import command_task
    cpus = physical_cpus(4)
    tasks, millicores = SHAPES[a.shape]
    if a.arm == 'plain' and a.plain_tasks:
        tasks = a.plain_tasks
    peer_cpus, gpu = cpus[:2], None
    if a.shape == 'gpu':
        gpu = subprocess.run(['nvidia-smi', '--query-gpu=uuid', '--format=csv,noheader'], capture_output=True,
                             text=True, check=True).stdout.split()[0]
        peer_cpus = [c for c in sorted(os.sched_getaffinity(0)) if c not in cpus][:1]
    report = {'shape': a.shape, 'arm': a.arm, 'cpus': cpus, 'tasks': tasks, 'task_millicores': millicores,
              'window_seconds': a.window, 'status': 'running'}
    cluster = None
    plain = []
    try:
        if a.arm not in ('alone', 'plain'):
            mode = 'performance' if a.arm == 'performance' else 'aggressive'
            cluster = Cluster(repo, out, cpus, mode, tasks, a.lead + a.window + 240, gpu)
            if a.arm == 'unprotected' and allow(cluster.binary, out / 'node.toml') != 0:
                raise RuntimeError('the allowance was not set')
            time.sleep(3)
        else:
            os.sched_setaffinity(0, [c for c in sorted(os.sched_getaffinity(0)) if c not in cpus][-2:])
        lead = 5 if a.shape == 'fill' else a.lead
        epoch = int(time.time()) + lead
        start, end = epoch + 5, epoch + 5 + a.window
        if a.shape != 'fill':
            atomic_json(out / 'schedule.json', {'epoch': epoch, 'seconds': a.window + 15,
                                                'cpus': ','.join(map(str, peer_cpus)), 'kind': 'saturating',
                                                'tied': 1, 'gpu': gpu})
        paths = [out / f'own-{i}.json' for i in range(tasks)]
        while time.time() < start:
            if cluster:
                cluster.check()
            time.sleep(.05)
        # Own work is submitted at the start of the window, so admission meets the second
        # account's load already running, or an idle scope.
        submitted = time.time()
        if cluster:
            extra = {}
            if a.arm == 'unprotected':
                extra = dict(contention_modes={'cpu': 'aggressive', 'gpu_compute': 'aggressive'},
                             unprotected=True, confirm_unprotected=True)
            for i, path in enumerate(paths):
                task_id = f'{cluster.run_id}.{i}'
                task = command_task(task_id, [a.python, '-c', GPU_LOOP if gpu else CPU_LOOP, str(end), str(path)],
                                    str(repo), env=cluster.worker_env, cpu_millicores=millicores,
                                    ram_mib=1024 if gpu else 256, gpu_vram_mib={gpu: 512} if gpu else {},
                                    allocation_class='opportunistic', single_process=True, no_escape=True,
                                    replay_safe=True, max_attempts=1, **extra)
                cluster.client.submit(task_id, cluster.run_id, [task])
                cluster.jobs.append(task_id)
        elif a.arm == 'plain':
            # The baseline: the same loops started by hand where CedeGrid would run them.
            for path in paths:
                child = subprocess.Popen([a.python, '-c', GPU_LOOP if gpu else CPU_LOOP, str(end), str(path)],
                                         cwd=str(repo), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL)
                os.sched_setaffinity(child.pid, cpus)
                plain.append(child)
        samples = []
        while time.time() < end + 3:
            if cluster:
                cluster.check()
                states = [t['status'] for j in cluster.jobs for t in cluster.client.status(j).get('tasks', [])]
                running = sum(1 for p in paths if Path(f'{p}.pid').exists() and not p.exists())
                samples.append([round(time.time() - submitted, 2),
                                states.count('assigned') + states.count('completed'), running])
            elif plain:
                running = sum(1 for p in paths if Path(f'{p}.pid').exists() and not p.exists())
                samples.append([round(time.time() - submitted, 2), len(plain), running])
            time.sleep(.25)
        deadline = time.time() + 60
        started = [p for p in paths if Path(f'{p}.pid').exists()]
        while not all(p.exists() for p in started) and time.time() < deadline:
            time.sleep(.5)
        own = [json.loads(p.read_text()) for p in paths if p.exists()]
        report['own'] = {'started': len(started), 'units': sum(o['units'] for o in own),
                         'cpu_millicores': round(1000 * sum(o['cpu_seconds'] for o in own) / a.window)}
        report['samples'] = samples
        report['admitted_at'] = admitted_at(samples, (0.5, 1, 2, 4, 8))
        if cluster:
            decisions = cluster.decisions(submitted)
            # SC-002: no compute cap, pause or drain on own work in any run.
            report['own_steps'] = [[round(at - submitted, 2), action.get('code'), action.get('resource')]
                                   for at, decision in decisions for action in decision.get('actions', [])]
            log = (out / 'agent.log').read_text(errors='replace')
            report['warnings'] = log.count('this run is UNPROTECTED')
            report['refusals'] = log.count('unprotected_refused')
        report.update(submitted_unix=submitted, start_unix=start, end_unix=end, status='completed')
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = repr(error)[:2000]
        raise
    finally:
        if cluster is not None:
            report['cluster_cleanup'] = cluster.stop()
        for child in plain:
            if child.poll() is None:
                child.kill()
            child.wait()
        atomic_json(out / 'report.json', report)
        print(json.dumps({k: report.get(k) for k in ('shape', 'arm', 'status', 'own', 'admitted_at', 'error')}))


if __name__ == '__main__':
    main()
