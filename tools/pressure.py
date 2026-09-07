#!/usr/bin/env python3
"""Explicitly authorized, bounded node-local external demand; no manager registry.

The foreground process creates only bounded threads, never subprocesses. A
separate live OwnedProcess watchdog may stop this exact child; a saved PID is not
cleanup authority. GPU observation is read-only NVML; no controls are modified.
"""
from __future__ import annotations
import argparse
import ctypes as C
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import sys
import threading
import time

from validation_runtime import atomic_json, inside_home, linux_identity, local_guard


class Memory(C.Structure):
    _fields_ = [('total', C.c_ulonglong), ('free', C.c_ulonglong), ('used', C.c_ulonglong)]


class ProcessInfo(C.Structure):
    # NVIDIA nvmlProcessInfo_t, used by compute/graphics process queries v3.
    _fields_ = [('pid', C.c_uint), ('usedGpuMemory', C.c_ulonglong),
                ('gpuInstanceId', C.c_uint), ('computeInstanceId', C.c_uint)]


class Nvml:
    def __init__(self, uuid):
        self.lib = C.CDLL('libnvidia-ml.so.1')
        self.lib.nvmlInit_v2.restype = C.c_int
        self.check(self.lib.nvmlInit_v2())
        try:
            self.device = C.c_void_p()
            self.lib.nvmlDeviceGetHandleByUUID.argtypes = [C.c_char_p, C.POINTER(C.c_void_p)]
            self.lib.nvmlDeviceGetHandleByUUID.restype = C.c_int
            self.check(self.lib.nvmlDeviceGetHandleByUUID(uuid.encode(), C.byref(self.device)))
            self.lib.nvmlDeviceGetMemoryInfo.argtypes = [C.c_void_p, C.POINTER(Memory)]
            self.lib.nvmlDeviceGetMemoryInfo.restype = C.c_int
            for name in ['nvmlDeviceGetComputeRunningProcesses_v3', 'nvmlDeviceGetGraphicsRunningProcesses_v3']:
                fn = getattr(self.lib, name)
                fn.argtypes = [C.c_void_p, C.POINTER(C.c_uint), C.POINTER(ProcessInfo)]
                fn.restype = C.c_int
        except BaseException:
            self.close()
            raise
        self.uuid = uuid

    @staticmethod
    def check(code):
        if code:
            raise RuntimeError(f'NVML reading unavailable (status {code}); unknown is not zero')

    def sample(self, own_pid_expected=False, target_pid=None):
        memory = Memory()
        self.check(self.lib.nvmlDeviceGetMemoryInfo(self.device, C.byref(memory)))
        own = []
        for name in ['nvmlDeviceGetComputeRunningProcesses_v3', 'nvmlDeviceGetGraphicsRunningProcesses_v3']:
            count, records = C.c_uint(1024), (ProcessInfo * 1024)()
            self.check(getattr(self.lib, name)(self.device, C.byref(count), records))
            if count.value > 1024:
                raise RuntimeError('NVML process reading exceeded bound')
            for record in records[:count.value]:
                if record.pid == (os.getpid() if target_pid is None else target_pid):
                    if record.usedGpuMemory == 2**64 - 1:
                        raise RuntimeError('own GPU memory accounting unavailable')
                    own.append(record.usedGpuMemory)
        if own_pid_expected and not own:
            raise RuntimeError('active test GPU process memory is not observable')
        return {'observed_unix': time.time(), 'scope': 'device_uuid', 'gpu_uuid': self.uuid,
                'total_bytes': memory.total, 'free_bytes': memory.free,
                'own_pid_observed': bool(own), 'own_memory_bytes': max(own) if own else None}

    def close(self):
        self.lib.nvmlShutdown()


def validate(config):
    duration = float(config['duration_seconds'])
    if not math.isfinite(duration) or not 0 < duration <= 86400:
        raise ValueError('duration must be finite and in (0,86400]')
    if not config.get('execution_approved'):
        raise ValueError('pressure execution envelope is not approved')
    if config.get('shared_server') and not config.get('shared_window_approved'):
        raise ValueError('shared-server pressure budget/window is not approved')
    inside_home(config['output'])
    cpus = config.get('cpu_ids', [])
    if len(set(cpus)) != len(cpus) or len(cpus) > 2 or any(type(cpu) is not int or cpu < 0 for cpu in cpus):
        raise ValueError('pressure permits at most two explicit distinct CPU IDs')
    if not 0 < config.get('max_rss_bytes', 2 * 1024**3) <= 2 * 1024**3:
        raise ValueError('pressure RSS cap must be positive and at most 2 GiB')
    if not 0 < config.get('max_vram_bytes', 1024**3) <= 1024**3:
        raise ValueError('pressure VRAM cap must be positive and at most 1 GiB')
    prior_end = 0.
    names = set()
    for event in sorted(config.get('events', []), key=lambda e:e['at_seconds']):
        mode, seconds, at = event['mode'], float(event['duration_seconds']), float(event['at_seconds'])
        if mode not in {'cpu', 'gpu', 'protected_cpu', 'protected_gpu'}:
            raise ValueError('unsupported pressure mode')
        maximum = 30 if 'gpu' in mode else 60
        if not math.isfinite(seconds + at) or not 0 < seconds <= maximum or at < prior_end or at + seconds > duration:
            raise ValueError('pressure events must be bounded, non-overlapping, and inside the approved window')
        if not isinstance(event['id'],str) or not __import__('re').fullmatch(r'[A-Za-z0-9_.-]+',event['id']):
            raise ValueError('event ID must be a safe explicit identifier')
        if event['id'] in names:
            raise ValueError('event IDs must be unique')
        names.add(event['id'])
        if 'cpu' in mode and not cpus:
            raise ValueError('CPU pressure requires explicit allowed CPU IDs')
        if type(event.get('cpu_iterations',1500)) is not int or not 1<=event.get('cpu_iterations',1500)<=100000:
            raise ValueError('CPU request work must be a bounded1..100000 iterations')
        if 'gpu' in mode:
            if not str(config.get('gpu_uuid', '')).startswith('GPU-'):
                raise ValueError('GPU pressure requires one exact GPU UUID')
            if not 0 < event.get('allocate_mib', 256) <= 512:
                raise ValueError('GPU allocation policy must be in (0,512] MiB')
        prior_end = at + seconds
    if any('gpu' in item['mode'] for item in config['events']) and len(config['events']) != 1:
        raise ValueError('GPU demand requires one finite event per process so CUDA context exits between events')
    if not names:
        raise ValueError('at least one finite demand event is required')
    return duration


def quantile(values, fraction):
    if not values:
        return None
    values = sorted(values)
    position = (len(values)-1)*fraction
    lower = math.floor(position)
    return values[lower] + (values[math.ceil(position)]-values[lower])*(position-lower)


def cpu_work(cpu, deadline, stop, output, protected, failures, iterations=1500):
    try:
        os.sched_setaffinity(0, {cpu})
        scheduled = time.monotonic()
        completed, samples, requests = 0, [], []
        while not stop.is_set() and time.monotonic() < deadline:
            if protected and scheduled > time.monotonic():
                stop.wait(min(scheduled-time.monotonic(), max(0, deadline-time.monotonic())))
            if stop.is_set() or time.monotonic() >= deadline:
                break
            began = time.monotonic()
            hashlib.pbkdf2_hmac('sha256', b'resmgr-owned-pressure', b'fixed-work', iterations)
            ended = time.monotonic()
            if ended <= deadline:
                completed += 1
                samples.append(1000*(ended-(scheduled if protected else began)))
                requests.append({'arrival_monotonic':scheduled if protected else began,
                    'started_monotonic':began,'finished_monotonic':ended,'latency_ms':samples[-1]})
            scheduled += .02
        output.append({'cpu_id': cpu, 'completed_units': completed, 'p95_ms': quantile(samples,.95),
                       'p99_ms': quantile(samples,.99), 'latency_samples_ms': samples,'requests':requests})
    except BaseException as error:
        failures.append(type(error).__name__ + ': ' + str(error))
        stop.set()


def pressure_event(config, event, stopped):
    start = time.monotonic()
    deadline = start + event['duration_seconds']
    local_stop = threading.Event()
    threads, results, failures, readings = [], [], [], []
    gpu, torch = None, None
    mode = event['mode']
    result = {'id':event['id'], 'mode':mode, 'started_unix':time.time(), 'status':'running',
              'managed_registry_member':False, 'identity':linux_identity(os.getpid())}
    def guard(own_gpu=False):
        if (inside_home(config['output'])/'STOP').exists():
            stopped.set()
        reading = local_guard(config['output'], config.get('limits', {}))
        fields = Path('/proc/self/status').read_text().splitlines()
        rss = next((int(line.split()[1])*1024 for line in fields if line.startswith('VmRSS:')), None)
        reading['own_rss_bytes'] = rss
        if rss is None or rss > config.get('max_rss_bytes', 2*1024**3):
            reading['errors'].append('own_rss_unavailable_or_above_cap')
        if gpu is not None:
            observed = gpu.sample(own_gpu)
            reading['gpu'] = observed
            if observed['free_bytes'] < config.get('min_gpu_free_bytes', 4*1024**3):
                reading['errors'].append('gpu_headroom_below_bound')
            if observed['own_memory_bytes'] is not None and observed['own_memory_bytes'] > config.get('max_vram_bytes',1024**3):
                reading['errors'].append('own_vram_above_bound')
        readings.append(reading)
        if reading['errors']:
            raise RuntimeError(','.join(reading['errors']))
    try:
        if 'cpu' in mode:
            result.update(demand_started_unix=time.time(),demand_started_monotonic=time.monotonic())
            for cpu in config['cpu_ids']:
                thread = threading.Thread(target=cpu_work, args=(cpu, deadline, local_stop, results, mode=='protected_cpu', failures,event.get('cpu_iterations',1500)))
                threads.append(thread)
                thread.start()
            while time.monotonic() < deadline and not stopped.is_set() and not local_stop.is_set():
                guard()
                stopped.wait(min(.5, max(0,deadline-time.monotonic())))
        else:
            os.environ['CUDA_VISIBLE_DEVICES'] = config['gpu_uuid']
            os.environ.setdefault('CUDA_CACHE_PATH', str(inside_home(config['output'])/'cuda-cache'))
            gpu = Nvml(config['gpu_uuid'])
            guard()
            import torch as pytorch
            torch = pytorch
            torch.set_num_threads(1)
            try:
                torch.set_num_interop_threads(1)
            except RuntimeError:
                pass  # A previous finite event initialized the same process.
            if not torch.cuda.is_available() or torch.cuda.device_count()!=1:
                raise RuntimeError('exactly one visible supported CUDA device is required')
            result.update(demand_started_unix=time.time(),demand_started_monotonic=time.monotonic())
            allocation = torch.empty(event.get('allocate_mib',256)*1024**2, dtype=torch.uint8, device='cuda:0')
            allocation.fill_(1)
            a = torch.full((1024,1024), .001, device='cuda:0')
            b = torch.full_like(a,.002)
            out = torch.empty_like(a)
            torch.cuda.synchronize()
            guard(True)
            completed, latencies, requests, scheduled = 0, [], [], time.monotonic()
            next_guard = scheduled + .5
            while time.monotonic() < deadline and not stopped.is_set():
                if mode=='protected_gpu' and scheduled > time.monotonic():
                    stopped.wait(min(scheduled-time.monotonic(),max(0,deadline-time.monotonic())))
                if stopped.is_set() or time.monotonic() >= deadline:
                    break
                began=time.monotonic()
                torch.mm(a,b,out=out)
                torch.cuda.synchronize()
                ended=time.monotonic()
                if ended<=deadline:
                    completed+=1
                    latencies.append(1000*(ended-(scheduled if mode=='protected_gpu' else began)))
                    requests.append({'arrival_monotonic':scheduled if mode=='protected_gpu' else began,
                        'started_monotonic':began,'finished_monotonic':ended,'latency_ms':latencies[-1]})
                scheduled+=.02
                if ended>=next_guard:
                    guard(True)
                    next_guard=ended+.5
            results.append({'completed_units':completed, 'p95_ms':quantile(latencies,.95),
                            'p99_ms':quantile(latencies,.99), 'latency_samples_ms':latencies,'requests':requests})
            del allocation,a,b,out
            torch.cuda.empty_cache()
        if failures:
            raise RuntimeError('; '.join(failures))
        if not sum(item['completed_units'] for item in results) and not threads:
            raise RuntimeError('no useful GPU work completed inside the event bound')
        result['status']='interrupted' if stopped.is_set() else 'completed'
    except BaseException as error:
        result['status']='failed'
        result['error']=type(error).__name__+': '+str(error)
        raise
    finally:
        local_stop.set()
        for thread in threads:
            thread.join(timeout=2)
        result['threads_reaped']=all(not thread.is_alive() for thread in threads)
        if gpu is not None:
            gpu.close()
        result.update(elapsed_seconds=time.monotonic()-start, results=results, readings=readings)
        atomic_json(inside_home(config['output'])/('event-'+str(event['id'])+'.json'),result)
    if not result['threads_reaped']:
        raise RuntimeError('pressure thread did not finish; outer watchdog must retain the live child')
    return result


def run(config):
    duration=validate(config)
    if not sys.platform.startswith('linux') or not hasattr(os,'sched_getaffinity'):
        raise RuntimeError('native Linux pressure execution is unsupported/unverified on this host')
    if not set(config.get('cpu_ids',[])).issubset(os.sched_getaffinity(0)):
        raise ValueError('pressure CPU IDs are outside the permitted affinity')
    output=inside_home(config['output']); output.mkdir(parents=True, exist_ok=False)
    os.environ.update(TMPDIR=str(output), XDG_CACHE_HOME=str(output/'cache'))
    stopped=threading.Event(); previous={}
    for sig in [signal.SIGTERM,signal.SIGINT]:
        previous[sig]=signal.signal(sig,lambda *_:stopped.set())
    report={'schema_version':1,'status':'running','requested_seconds':duration,'started_unix':time.time(),
            'identity':linux_identity(os.getpid()),'events':[],'operational_acceptance':'pending_review',
            'memory_control':'observed admission/abort policy, not a hard GPU partition'}
    atomic_json(output/'config.json',config)
    start=time.monotonic()
    try:
        for event in sorted(config['events'],key=lambda value:value['at_seconds']):
            while time.monotonic()-start<event['at_seconds']:
                if (output/'STOP').exists(): stopped.set()
                if stopped.is_set(): raise RuntimeError('operator stop requested')
                stopped.wait(min(.5,event['at_seconds']-(time.monotonic()-start)))
            if stopped.is_set() or (output/'STOP').exists(): raise RuntimeError('operator stop requested')
            report['events'].append(pressure_event(config,event,stopped))
            atomic_json(output/'status.json',report)
        report['status']='events_finished_review_required'
    except BaseException as error:
        report['status']='aborted'; report['error']=type(error).__name__+': '+str(error)
    finally:
        stopped.set()
        report.update(elapsed_seconds=time.monotonic()-start, finished_unix=time.time())
        atomic_json(output/'status.json',report)
        for sig,handler in previous.items(): signal.signal(sig,handler)
    return report


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config',required=True); parser.add_argument('--execute',action='store_true')
    args=parser.parse_args(argv); config=json.loads(inside_home(args.config).read_text())
    if not args.execute:
        print(json.dumps({'mode':'plan_only','configuration':config,'executed':False},indent=2)); return 0
    report=run(config); print(json.dumps(report,indent=2)); return 1 if report['status']=='aborted' else 0

if __name__=='__main__': raise SystemExit(main())
