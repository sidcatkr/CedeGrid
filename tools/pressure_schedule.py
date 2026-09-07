#!/usr/bin/env python3
"""Node-local approved schedule of finite external demand processes.

Each event is a separately reaped child. CUDA contexts end between events. The
harness records its direct-child capability; persisted PIDs cannot be adopted.
After supervisor death a finite child retains its own wall-time/abort loop, but
no surviving watchdog is claimed for a hung kernel call.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import sys
import time
import uuid
from pressure import Nvml, validate as validate_event
from validation_runtime import OwnedProcess, atomic_json, inside_home, home_executable, local_guard


def validate(config):
    if not config.get('execution_approved') or (config.get('shared_server') and not config.get('shared_window_approved')):
        raise ValueError('node-local schedule and shared window require explicit approval')
    duration=float(config['duration_seconds'])
    if not 0<duration<=86400: raise ValueError('schedule duration must be in (0,86400]')
    inside_home(config['output']); home_executable(config['python'])
    end=0
    for event in sorted(config['events'],key=lambda value:value['at_seconds']):
        if event['at_seconds']<end or event['at_seconds']+event['duration_seconds']+15>duration:
            raise ValueError('events plus cleanup intervals must not overlap or exceed the window')
        end=event['at_seconds']+event['duration_seconds']+15
        value={**config,'duration_seconds':event['duration_seconds'],'events':[{**event,'at_seconds':0}]}
        validate_event(value)
    return duration


def run(config):
    duration=validate(config)
    if sys.platform!='linux': raise RuntimeError('Linux-native pressure scheduling is unverified on this host')
    output=inside_home(config['output']); output.mkdir(parents=True,exist_ok=False)
    report={'schema_version':1,'status':'running','started_unix':time.time(),'events':[],
        'operational_acceptance':'pending_review','manager_registry_member':False,'requested_seconds':duration}
    atomic_json(output/'config.json',config)
    stopped=[]; previous={}; child=None; start=time.monotonic()
    for sig in [signal.SIGINT,signal.SIGTERM]: previous[sig]=signal.signal(sig,lambda sig,_:stopped.append(sig))
    try:
        for event in sorted(config['events'],key=lambda value:value['at_seconds']):
            while time.monotonic()-start<event['at_seconds']:
                if stopped or (output/'STOP').exists(): raise RuntimeError('operator stop requested')
                guard=local_guard(output,config.get('limits',{}))
                if guard['errors']: raise RuntimeError(','.join(guard['errors']))
                time.sleep(min(.5,event['at_seconds']-(time.monotonic()-start)))
            identity=uuid.uuid4().hex; event_output=output/identity
            value={**config,'output':str(event_output),'duration_seconds':event['duration_seconds'],
                'events':[{**event,'at_seconds':0}]}
            event_config=output/(identity+'.json'); atomic_json(event_config,value)
            env=dict(os.environ,TMPDIR=str(output),XDG_CACHE_HOME=str(output/'cache'),PYTHONDONTWRITEBYTECODE='1')
            argv=[str(home_executable(config['python'])),str(Path(__file__).with_name('pressure.py')),'--config',str(event_config),'--execute']
            child=OwnedProcess(argv,Path(__file__).parent,output/(identity+'.log'),env)
            result={'event':event,'started_seconds':time.monotonic()-start,'identity':child.identity,'argv':argv}
            deadline=time.monotonic()+event['duration_seconds']+10
            while child.poll() is None:
                if stopped or (output/'STOP').exists(): raise RuntimeError('operator stop requested')
                if time.monotonic()>deadline: raise RuntimeError('pressure child exceeded its event wall-time bound')
                time.sleep(.2)
            result['cleanup']=child.stop(); pid=child.child.pid; child=None
            result['event_report']=json.loads((event_output/'status.json').read_text())
            result['gpu_release_confirmed']=None
            if 'gpu' in event['mode']:
                gpu=Nvml(config['gpu_uuid'])
                try:
                    release_deadline=time.monotonic()+5
                    while True:
                        reading=gpu.sample(target_pid=pid)
                        result['gpu_release_observation']=reading
                        if not reading['own_pid_observed']:
                            result['gpu_release_confirmed']=True; break
                        if time.monotonic()>=release_deadline:
                            result['gpu_release_confirmed']=False; break
                        time.sleep(.25)
                finally: gpu.close()
            result['elapsed_seconds']=time.monotonic()-start-result['started_seconds']
            report['events'].append(result); atomic_json(output/'status.json',report)
            if result['cleanup']['exit_code']!=0 or result['gpu_release_confirmed'] is False:
                raise RuntimeError('pressure event or resource release failed; inspect retained evidence')
        report['status']='events_finished_review_required'
    except BaseException as error:
        report['status']='aborted'; report['error']=type(error).__name__+': '+str(error)
    finally:
        if child is not None:
            try: report['interrupted_child_cleanup']=child.stop()
            except Exception as error: report['unresolved_child']={'identity':child.identity,'error':str(error)}
        report.update(elapsed_seconds=time.monotonic()-start,finished_unix=time.time())
        atomic_json(output/'status.json',report)
        for sig,handler in previous.items(): signal.signal(sig,handler)
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config',required=True); parser.add_argument('--execute',action='store_true')
    args=parser.parse_args(); config=json.loads(inside_home(args.config).read_text())
    if not args.execute: print(json.dumps({'mode':'plan_only','configuration':config})); return 0
    report=run(config);print(json.dumps(report,indent=2));return 1 if report['status']=='aborted' else 0
if __name__=='__main__':sys.exit(main())
