#!/usr/bin/env python3
"""Feature 004's host gates (quickstart section 7) on a scope of four physical CPUs, with the agent
started directly and the coordinator and this harness outside the scope.

  fill  an idle scope: eight own CPU-bound tasks of 500 m; admission sampled every 0.25 s.
  f2c   eight own CPU-bound tasks of 1000 m while a second account runs two saturating loops
        tied to two of the scope's CPUs.
  gpu   two own GPU tasks (1024x1024 multiplies) while a second account saturates the same GPU
        from a CPU outside the scope.
  first, first-gpu
        as f2c and gpu, but own work starts on an idle scope and the second account arrives 20 s
        later (006: own work first).
  batch sixteen own tasks of 1000 m, each a fixed amount of CPU work, on an idle scope; records
        each task's completion (006 SC-004).
  vram  six own GPU tasks of 2048 MiB each on one device, more than its memory, for --window
        seconds each; records which ones could allocate (006 SC-005).
  ram   twelve own tasks of 4096 MiB each, more than the host's RAM and swap, holding their memory
        for --window seconds; records which ones finished and which the kernel killed. Its plain
        arm drives the host into OOM: run it only with the host's owner's approval (006 SC-005).
  cg-later-gpu, cg-first-gpu
        two own GPU tasks beside the second account's one GPU task run through its own CedeGrid
        (--peer-arm cedegrid, in --peer-mode) or by hand (--peer-arm plain), the second account
        first or second, 20 s apart (006 SC-003). The second account's side runs this script
        with --role peer from a copy it can read.

Arms: alone (the second account's load with no own work), plain (own tasks started by hand on the
scope without CedeGrid, the harm baseline; --plain-tasks sets how many), performance, admit
(Performance with admit immediately), aggressive (protected) and unprotected (Aggressive with the
host's allowance turned on through setup on a terminal, and both SDK fields set). For f2c and gpu the harness writes schedule.json, from which the
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
CPU_WORK = r'''
import hashlib, json, os, sys, time
target, path = int(sys.argv[1]), sys.argv[2]
t0 = time.time()
open(path + '.pid', 'w').write(json.dumps({'pid': os.getpid(), 'started': t0}))
units = 0; cpu0 = time.process_time()
while units < target:
    hashlib.pbkdf2_hmac('sha256', b'cedegrid-006', b'fixed-seed', 1500); units += 1
json.dump({'units': units, 'cpu_seconds': time.process_time() - cpu0, 'seconds': time.time() - t0,
           'started': t0, 'finished': time.time()}, open(path, 'w'))
'''
VRAM_LOOP = r'''
import json, os, sys, time
seconds, mib, path = float(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
t0 = time.time()
open(path + '.pid', 'w').write(json.dumps({'pid': os.getpid(), 'started': t0}))
import torch
try:
    held = torch.empty(mib * 2**20 // 4, dtype=torch.float32, device='cuda:0')
    a = torch.full((1024, 1024), .001, device='cuda:0'); b = torch.full_like(a, .002); c = torch.empty_like(a)
    torch.cuda.synchronize()
except RuntimeError as error:
    json.dump({'units': 0, 'cpu_seconds': time.process_time(), 'seconds': time.time() - t0, 'started': t0,
               'finished': time.time(), 'allocated': False, 'error': str(error)[:200]}, open(path, 'w'))
    raise SystemExit(1)
units = 0; end = time.time() + seconds
while time.time() < end:
    torch.mm(a, b, out=c); torch.cuda.synchronize(); units += 1
json.dump({'units': units, 'cpu_seconds': time.process_time(), 'seconds': time.time() - t0, 'started': t0,
           'finished': time.time(), 'allocated': True}, open(path, 'w'))
'''
SHAPES = {'fill': (8, 500), 'f2c': (8, 1000), 'gpu': (2, 500), 'first': (8, 1000), 'first-gpu': (2, 500),
          'batch': (16, 1000), 'vram': (6, 500)}
SHAPES.update({'cg-later-gpu': (2, 500), 'cg-first-gpu': (2, 500), 'ram': (12, 500)})
RAM_LOOP = r'''
import json, os, sys, time
seconds, mib, path = float(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
t0 = time.time()
open(path + '.pid', 'w').write(json.dumps({'pid': os.getpid(), 'started': t0}))
held = bytearray(mib * 2**20)
for i in range(0, len(held), 4096):
    held[i] = 1
end = time.time() + seconds
while time.time() < end:
    time.sleep(.5)
json.dump({'units': 1, 'cpu_seconds': time.process_time(), 'seconds': time.time() - t0, 'started': t0,
           'finished': time.time(), 'allocated': True}, open(path, 'w'))
'''
RAM_TASK_MIB = 4096
# Fixed-work shapes run until every task finished or was killed.
FIXED_SHAPES = {'batch', 'vram', 'ram'}
GPU_SHAPES = {'gpu', 'first-gpu', 'vram', 'cg-later-gpu', 'cg-first-gpu'}
# Shapes with the second account's load, started by the operator from schedule.json.
PEER_SHAPES = {'f2c', 'gpu', 'first', 'first-gpu', 'cg-later-gpu', 'cg-first-gpu'}
# Own work arrives first; the second account 20 s later.
FIRST_SHAPES = {'first', 'first-gpu', 'cg-first-gpu'}
# The second account runs its work through its own CedeGrid, or by hand, from this script.
CEDEGRID_PEER_SHAPES = {'cg-later-gpu', 'cg-first-gpu'}
# Each batch task's CPU work: about 15 s of one CPU on the test host.
BATCH_UNITS = 90_000
VRAM_TASK_MIB = 2048
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

    def __init__(self, repo, out, cpus, mode, workers, lifetime, gpu_uuid=None, gpu_mib=2048,
                 gpu_reserve_mib=None, ram_mib=None):
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
        # CedeGrid's own default, as an operator gets it: the validation deployment's 30 s GPU
        # scale-up cooldown would hold own work back where plain Linux starts at once.
        node.get('gpu', {}).pop('scale_up_cooldown_ms', None)
        if gpu_reserve_mib is not None:
            node['gpu']['reserve_vram_mib'] = gpu_reserve_mib
        if ram_mib is not None:
            # The host's floors decide; the agent's own reserve stays small.
            node['ram'] = {**node.get('ram', {}), 'reserve_mib': 2048}
        if mode == 'aggressive':
            node['contention'].update(max_mode='aggressive', acknowledge_caution=['aggressive_mode'])
        atomic_runtime_config(self.out / 'node.toml', node, 'node')
        agent = read_runtime_config(self.out / 'agent.toml')
        agent.update(max_workers=workers, capacity={'cpu_millicores': len(self.cpus) * 1000,
                     'ram_mib': ram_mib or max(4096, workers * 1024 * 2),
                     'gpu_memory_mib': {gpu_uuid: gpu_mib} if gpu_uuid else {}})
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


def ended(path):
    """Whether a fixed-work task finished, or started and is gone without a result."""
    if path.exists():
        return True
    pid_file = Path(f'{path}.pid')
    if not pid_file.exists():
        return False
    return not Path(f'/proc/{json.loads(pid_file.read_text())["pid"]}').exists()


def service_ticks(binary, skip=(), root=Path('/proc')):
    """CPU ticks so far of each process of the run's `binary` but `skip`: the agent and its
    supervisors, without the coordinator, which runs outside the scope. Keyed by (pid, start
    ticks), so a reused PID is another process. Reads only."""
    target = Path(binary).resolve()
    ticks = {}
    for entry in Path(root).iterdir():
        if not entry.name.isdigit() or int(entry.name) in skip:
            continue
        try:
            if Path(os.readlink(entry / 'exe')).resolve() != target:
                continue
            fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
        except OSError:
            continue
        ticks[(int(entry.name), int(fields[19]))] = int(fields[11]) + int(fields[12])
    return ticks


def ticks_since(start, samples):
    """The ticks these processes used after `start`: each one's largest sample less its ticks at
    the start, all of them for one that started later."""
    last = {}
    for sample in samples:
        for key, value in sample.items():
            last[key] = max(last.get(key, 0), value)
    return sum(max(0, value - start.get(key, 0)) for key, value in last.items())


def completion(own, submitted):
    """Each finished task's completion, in seconds after submission: the mean and the last."""
    times = sorted(round(o['finished'] - submitted, 2) for o in own if 'finished' in o)
    if not times:
        return {'completion_s': [], 'mean_s': None, 'makespan_s': None}
    return {'completion_s': times, 'mean_s': round(sum(times) / len(times), 2), 'makespan_s': times[-1]}


def peer_role(a):
    """The second account's side of the cg shapes: one GPU task through its own CedeGrid in
    --peer-mode, or by hand, submitted at --at and running until --until. Writes report.json with
    its units; acts only on processes it starts."""
    repo = Path(a.repo).resolve()
    out = Path(a.output).resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    for sub in ('tools', 'python'):
        sys.path.insert(0, str(repo / sub))
    from validation_runtime import atomic_json
    from cedegrid import command_task
    gpu = subprocess.run(['nvidia-smi', '--query-gpu=uuid', '--format=csv,noheader'], capture_output=True,
                         text=True, check=True).stdout.split()[0]
    cpus = [int(c) for c in a.peer_cpus.split(',')]
    path = out / 'peer-0.json'
    report = {'role': 'peer', 'arm': a.peer_arm, 'mode': a.peer_mode, 'status': 'running'}
    cluster, child = None, None
    try:
        if a.peer_arm == 'cedegrid':
            cluster = Cluster(repo, out, cpus, a.peer_mode, 1, int(a.until - time.time()) + 240, gpu)
        while time.time() < a.at:
            if cluster:
                cluster.check()
            time.sleep(.05)
        submitted = time.time()
        argv = [a.python, '-c', GPU_LOOP, str(a.until), str(path)]
        if cluster:
            task_id = f'{cluster.run_id}.0'
            cluster.client.submit(task_id, cluster.run_id, [command_task(
                task_id, argv, str(repo), env=cluster.worker_env, cpu_millicores=500, ram_mib=1024,
                gpu_vram_mib={gpu: 512}, allocation_class='opportunistic', single_process=True,
                no_escape=True, replay_safe=True, max_attempts=1)])
            cluster.jobs.append(task_id)
        else:
            child = subprocess.Popen(argv, cwd=str(repo), stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            os.sched_setaffinity(child.pid, cpus)
        while time.time() < a.until + 3:
            if cluster:
                cluster.check()
            time.sleep(.25)
        deadline = time.time() + 60
        while Path(f'{path}.pid').exists() and not path.exists() and time.time() < deadline:
            time.sleep(.5)
        own = json.loads(path.read_text()) if path.exists() else {}
        started = json.loads(Path(f'{path}.pid').read_text())['started'] if Path(f'{path}.pid').exists() else None
        report.update(units=own.get('units', 0), seconds=own.get('seconds'), submitted_unix=submitted,
                      started_unix=started, status='completed')
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = repr(error)[:2000]
        raise
    finally:
        if cluster is not None:
            report['cluster_cleanup'] = cluster.stop()
        if child is not None:
            if child.poll() is None:
                child.kill()
            child.wait()
        atomic_json(out / 'report.json', report)
        print(json.dumps({k: report.get(k) for k in ('role', 'arm', 'mode', 'status', 'units', 'error')}))


# The plain arm's launcher: it moves itself onto the scope, then starts every task, so each one
# forks there as from a user's shell on the scope and the kernel places it as it places
# CedeGrid's tasks. Moving each task in from outside instead spread them over the scope's CPUs.
PLAIN_LAUNCHER = r'''
import json, os, subprocess, sys
os.sched_setaffinity(0, json.loads(sys.argv[1]))
children = [subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL) for argv in json.loads(sys.argv[2])]
for child in children:
    child.wait()
'''


def plain_launcher(python, cpus, argvs):
    """The argv of one launcher that starts `argvs` from the scope `cpus`."""
    return [python, '-c', PLAIN_LAUNCHER, json.dumps(list(cpus)), json.dumps(argvs)]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--role', choices=['own', 'peer'], default='own')
    p.add_argument('--repo', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--python', required=True, help='the Python own tasks run with (torch for gpu)')
    p.add_argument('--shape', choices=sorted(SHAPES))
    p.add_argument('--arm', choices=['alone', 'plain', 'performance', 'admit', 'aggressive', 'unprotected'])
    p.add_argument('--plain-tasks', type=int, help="own tasks the plain arm starts (default: the shape's)")
    p.add_argument('--window', type=int, default=60)
    p.add_argument('--lead', type=int, default=40)
    p.add_argument('--peer-arm', choices=['cedegrid', 'plain'], default='cedegrid')
    p.add_argument('--peer-mode', choices=['performance', 'neutral'], default='performance')
    p.add_argument('--peer-cpus', help='peer role: the CPUs its work runs on')
    p.add_argument('--at', type=float, help='peer role: when to submit, in Unix seconds')
    p.add_argument('--until', type=float, help='peer role: when its work ends, in Unix seconds')
    a = p.parse_args()
    if a.role == 'peer':
        if a.peer_cpus is None or a.at is None or a.until is None:
            p.error('the peer role takes --peer-cpus, --at and --until')
        return peer_role(a)
    if a.shape is None or a.arm is None:
        p.error('--shape and --arm are required')
    if a.arm == 'admit' and a.shape not in CEDEGRID_PEER_SHAPES:
        p.error('admit runs the cg shapes')
    if a.shape == 'fill' and a.arm in ('alone', 'plain', 'unprotected'):
        p.error('fill runs performance or aggressive')
    if a.shape in FIXED_SHAPES and a.arm not in ('plain', 'performance'):
        p.error(f'{a.shape} runs plain or performance')
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
    peer_cpus, gpu, gpu_mib = cpus[:2], None, 2048
    if a.shape in GPU_SHAPES:
        gpu = subprocess.run(['nvidia-smi', '--query-gpu=uuid', '--format=csv,noheader'], capture_output=True,
                             text=True, check=True).stdout.split()[0]
        peer_cpus = [c for c in sorted(os.sched_getaffinity(0)) if c not in cpus][:1]
    if a.shape == 'vram':
        total = subprocess.run(['nvidia-smi', '--query-gpu=memory.total', '--format=csv,noheader,nounits'],
                               capture_output=True, text=True, check=True).stdout.split()[0]
        gpu_mib = int(total)
    report = {'shape': a.shape, 'arm': a.arm, 'cpus': cpus, 'tasks': tasks, 'task_millicores': millicores,
              'window_seconds': a.window, 'status': 'running'}
    cluster = None
    plain = []
    try:
        if a.arm not in ('alone', 'plain'):
            mode = 'performance' if a.arm in ('performance', 'admit') else 'aggressive'
            lifetime = a.lead + a.window + 240 + (300 if a.shape in FIXED_SHAPES else 0)
            total_ram = os.sysconf('SC_PAGE_SIZE') * os.sysconf('SC_PHYS_PAGES') // 2**20
            cluster = Cluster(repo, out, cpus, mode, tasks, lifetime, gpu, gpu_mib,
                              1024 if a.shape == 'vram' else None,
                              total_ram if a.shape == 'ram' else None)
            if a.arm == 'unprotected' and allow(cluster.binary, out / 'node.toml') != 0:
                raise RuntimeError('the allowance was not set')
            time.sleep(3)
        else:
            os.sched_setaffinity(0, [c for c in sorted(os.sched_getaffinity(0)) if c not in cpus][-2:])
        if a.shape in FIRST_SHAPES:
            # Own work on an idle scope first; the second account 20 s after it.
            start = int(time.time()) + 5
            epoch = start + 20
        elif a.shape == 'cg-later-gpu':
            # The second account's CedeGrid first; own work 20 s after it.
            epoch = int(time.time()) + a.lead
            start = epoch + 20
        else:
            lead = a.lead if a.shape in PEER_SHAPES else 5
            epoch = int(time.time()) + lead
            start = epoch + 5
        end = (start if a.shape == 'cg-later-gpu' else epoch + 5) + a.window
        if a.shape in PEER_SHAPES:
            schedule = {'epoch': epoch, 'seconds': a.window + 15, 'cpus': ','.join(map(str, peer_cpus)),
                        'kind': 'saturating', 'tied': 1, 'gpu': gpu}
            if a.shape in CEDEGRID_PEER_SHAPES:
                schedule.update(peer_arm=a.peer_arm, peer_mode=a.peer_mode, until=end + 5)
            atomic_json(out / 'schedule.json', schedule)
            report.update(peer_arm=a.peer_arm, peer_mode=a.peer_mode)
        paths = [out / f'own-{i}.json' for i in range(tasks)]

        def argv(path):
            if a.shape == 'batch':
                return [a.python, '-c', CPU_WORK, str(BATCH_UNITS), str(path)]
            if a.shape == 'vram':
                return [a.python, '-c', VRAM_LOOP, str(a.window), str(VRAM_TASK_MIB), str(path)]
            if a.shape == 'ram':
                return [a.python, '-c', RAM_LOOP, str(a.window), str(RAM_TASK_MIB), str(path)]
            return [a.python, '-c', GPU_LOOP if gpu else CPU_LOOP, str(end), str(path)]
        while time.time() < start:
            if cluster:
                cluster.check()
            time.sleep(.05)
        # Own work is submitted at the start: after the second account's load in f2c and gpu,
        # before it in the first shapes, or on an idle scope.
        submitted = time.time()
        if cluster:
            extra = {}
            if a.arm == 'unprotected':
                extra = dict(contention_modes={'cpu': 'aggressive', 'gpu_compute': 'aggressive'},
                             unprotected=True, confirm_unprotected=True)
            if a.arm == 'admit':
                extra = dict(admit_immediately=True, confirm_admit_immediately=True)
            vram = VRAM_TASK_MIB if a.shape == 'vram' else 512
            for i, path in enumerate(paths):
                task_id = f'{cluster.run_id}.{i}'
                ram = RAM_TASK_MIB + 256 if a.shape == 'ram' else 1024 if gpu else 256
                task = command_task(task_id, argv(path), str(repo), env=cluster.worker_env, cpu_millicores=millicores,
                                    ram_mib=ram, gpu_vram_mib={gpu: vram} if gpu else {},
                                    allocation_class='opportunistic', single_process=True, no_escape=True,
                                    replay_safe=True, max_attempts=1, **extra)
                cluster.client.submit(task_id, cluster.run_id, [task])
                cluster.jobs.append(task_id)
        elif a.arm == 'plain':
            # The baseline: the same work started by hand where CedeGrid would run it, from a
            # process already on the scope. Its own session, so cleanup reaches every task.
            plain.append(subprocess.Popen(
                plain_launcher(a.python, cpus, [argv(path) for path in paths]), cwd=str(repo),
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                start_new_session=True))
        samples = []
        # CedeGrid's own CPU on the scope, the overhead SC-001 allows for.
        coordinator = {cluster.services['coordinator'].child.pid} if cluster else set()
        service = [service_ticks(cluster.binary, coordinator)] if cluster else []
        service_start = service[0] if service else {}
        # Fixed work runs until every task has finished, within a bound.
        until = submitted + 600 if a.shape in FIXED_SHAPES else end + 3
        while time.time() < until:
            running = sum(1 for p in paths if Path(f'{p}.pid').exists() and not p.exists())
            if cluster:
                cluster.check()
                states = [t['status'] for j in cluster.jobs for t in cluster.client.status(j).get('tasks', [])]
                samples.append([round(time.time() - submitted, 2),
                                states.count('assigned') + states.count('completed'), running])
                service = [{**service[0], **{k: max(v, service[0].get(k, 0)) for k, v in
                                             service_ticks(cluster.binary, coordinator).items()}}]
            elif plain:
                samples.append([round(time.time() - submitted, 2), len(paths), running])
            if a.shape in FIXED_SHAPES and all(ended(p) for p in paths):
                break
            time.sleep(.25)
        deadline = time.time() + 60
        started = [p for p in paths if Path(f'{p}.pid').exists()]
        while not all(ended(p) for p in started) and time.time() < deadline:
            time.sleep(.5)
        own = [json.loads(p.read_text()) for p in paths if p.exists()]
        span = (max(o['finished'] for o in own) - submitted) if a.shape in FIXED_SHAPES and own else a.window
        report['own'] = {'started': len(started), 'units': sum(o['units'] for o in own),
                         'cpu_millicores': round(1000 * sum(o['cpu_seconds'] for o in own) / max(span, 1))}
        if cluster:
            hz = os.sysconf('SC_CLK_TCK')
            report['cedegrid_cpu_millicores'] = round(
                1000 * ticks_since(service_start, service) / hz / max(span, 1))
        if a.shape == 'batch':
            report['own'].update(completion(own, submitted))
        if a.shape == 'vram':
            report['own'].update(allocated=sum(1 for o in own if o.get('allocated')),
                                 failed=sum(1 for o in own if o.get('allocated') is False),
                                 **completion(own, submitted))
        if a.shape == 'ram':
            # A task that started and left no result was killed, by the kernel at exhaustion.
            report['own'].update(finished=len(own), killed=len(started) - len(own),
                                 **completion(own, submitted))
        report['samples'] = samples
        report['admitted_at'] = admitted_at(samples, (0.5, 1, 2, 4, 8))
        if cluster:
            decisions = cluster.decisions(submitted)
            # SC-002: no compute cap, pause or drain on own work in any run.
            report['own_steps'] = [[round(at - submitted, 2), action.get('code'), action.get('resource')]
                                   for at, decision in decisions for action in decision.get('actions', [])]
            log = (out / 'agent.log').read_text(errors='replace')
            report['warnings'] = log.count('this run is UNPROTECTED')
            report['cautions'] = log.count('"event":"admit_immediately"')
            report['refusals'] = log.count('unprotected_refused')
        report.update(submitted_unix=submitted, start_unix=start, end_unix=end, epoch_unix=epoch,
                      status='completed')
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = repr(error)[:2000]
        raise
    finally:
        if cluster is not None:
            report['cluster_cleanup'] = cluster.stop()
        for child in plain:
            # The launcher's session holds only it and the tasks it started.
            if child.poll() is None:
                try:
                    os.killpg(child.pid, 9)
                except ProcessLookupError:
                    pass
            child.wait()
        atomic_json(out / 'report.json', report)
        print(json.dumps({k: report.get(k) for k in ('shape', 'arm', 'status', 'own', 'admitted_at', 'error')}))


if __name__ == '__main__':
    main()
