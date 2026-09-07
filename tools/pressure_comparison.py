#!/usr/bin/env python3
"""Opt-in one-node Linux comparison through real coordinator/agent supervision.

Uses a fresh private home-local deployment, finite synthetic work and an
independently owned same-UID protected process. Does not authorize shared hosts.
"""
import argparse
import copy
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import time
import uuid

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'python'))
from resmgr import command_task,sha256_file
from anchor_validation import deployment,manager_samples,manager_metrics
from native_validation import physical_cpus
from pressure import Nvml,quantile
from soak import AllocationLedger,StatusJournal
from validation_runtime import OwnedProcess,atomic_json,inside_home,home_executable,local_guard

TARGETS={'matched_idle_throughput_ratio_min':.90,'decision_p95_seconds_max':1.,
    'release_p95_seconds_max':10.,'post_yield_p99_ratio_max':1.20,
    'manager_peak_rss_bytes_max':512*1024**2,'manager_active_cpu_cores_max':.5,
    'minimum_post_yield_requests':100,'repetitions':3}


def protected_samples(event,after=None):
    return [request['latency_ms'] for row in event.get('results',[]) for request in row.get('requests',[])
        if after is None or request['arrival_monotonic']>=after]


def evaluate(cases):
    ratios=[];decisions=[];releases=[];tails=[];missing=[]
    for repetition in range(TARGETS['repetitions']):
        group={case['scenario']:case for case in cases if case['repetition']==repetition and case.get('status')=='completed'}
        if len(group)!=5:missing.append(f'repetition{repetition}: incomplete matched cases');continue
        before=group['unmanaged_idle']['probe'];after=group['managed_idle']['probe']
        ratios.append((after['completed_units']/after['elapsed_seconds'])/(before['completed_units']/before['elapsed_seconds']))
        alone=protected_samples(group['protected_alone']['protected'])
        active=group['managed_contention'];released=active.get('released_monotonic')
        if active.get('clock_consistency_error_seconds',0)>.25:
            missing.append(f'repetition{repetition}: wall/monotonic clocks diverged');continue
        if released is None or not active.get('yielded'):
            missing.append(f'repetition{repetition}: no verified pressure-triggered yield');continue
        post=protected_samples(active['protected'],released)
        if len(post)<TARGETS['minimum_post_yield_requests'] or not alone:
            missing.append(f'repetition{repetition}: insufficient post-yield requests');continue
        tails.append(quantile(post,.99)/quantile(alone,.99))
        if active.get('decision_delay_seconds') is None:missing.append(f'repetition{repetition}: no policy decision timestamp')
        else:decisions.append(active['decision_delay_seconds'])
        if active.get('decision_to_release_seconds') is None:missing.append(f'repetition{repetition}: no release timing')
        else:releases.append(active['decision_to_release_seconds'])
    values={'matched_idle_throughput_ratio_min':quantile(ratios,.5),'decision_p95_seconds_max':quantile(decisions,.95),
        'release_p95_seconds_max':quantile(releases,.95),'post_yield_p99_ratio_max':quantile(tails,.5)}
    return {'missing':missing,'metrics':{name:{'observed':value,'target':TARGETS[name],
        'status':'inconclusive' if value is None or missing else ('passed' if
            (value>=TARGETS[name] if name.endswith('_min') else value<=TARGETS[name]) else 'failed')}
        for name,value in values.items()}}


def policy_decision(state,assignment,after_unix):
    database=state/'state.sqlite3'
    with sqlite3.connect(database.resolve().as_uri()+'?mode=ro',uri=True) as connection:
        for at,payload in connection.execute('SELECT observed_at_unix_ms,decision_json FROM observations WHERE observed_at_unix_ms>=? ORDER BY id',(int(after_unix*1000),)):
            value=json.loads(payload)
            if assignment in value.get('would_drain',[]):return {'observed_unix':at/1000,'decision':value}
    return None


def verify_external_registry(state,identity):
    with sqlite3.connect((state/'state.sqlite3').resolve().as_uri()+'?mode=ro',uri=True) as connection:
        for payload, in connection.execute('SELECT record_json FROM executions'):
            owned=json.loads(payload).get('identity') or {}
            if (owned.get('pid'),owned.get('boot_id'),owned.get('start_time'))==(identity['pid'],identity['boot_id'],identity['start_ticks']):
                raise RuntimeError('protected workload appeared in managed registry')
    return True


def confirm_gpu_release(gpu,pid):
    deadline=time.monotonic()+5
    while True:
        reading=gpu.sample(target_pid=pid)
        if not reading['own_pid_observed']:return reading
        if time.monotonic()>=deadline:raise RuntimeError('reaped external/probe GPU PID remains observable; retain uncertain release evidence')
        time.sleep(.25)


def probe_task(task_id,argv,manager,env,mode,gpu_uuid):
    return command_task(task_id,argv,str(manager),env=env,cpu_millicores=500 if mode=='cpu' else 1000,
        ram_mib=2048,gpu_vram_mib={gpu_uuid:1024} if mode=='gpu' else {},
        allocation_class='opportunistic',single_process=True,no_escape=True,replay_safe=True,max_attempts=1)


def run(args):
    if not args.execute or sys.platform!='linux':raise ValueError('explicit execution on the authorized Linux anchor is required')
    if args.mode not in {'cpu','gpu'} or not args.gpu_uuid.startswith('GPU-'):raise ValueError('explicit mode and GPU UUID required')
    root=inside_home(args.root);manager=Path(__file__).resolve().parents[1]
    output=inside_home(args.output);output.mkdir(mode=0o700,parents=True,exist_ok=False)
    python=home_executable(root/'venv-isolated/bin/python');binary=inside_home(manager/'target/release/resmgr')
    cpus=physical_cpus(2 if args.mode=='cpu' else 3)
    if len(cpus)!=(2 if args.mode=='cpu' else 3):raise RuntimeError('required physical CPU scope unavailable')
    os.sched_setaffinity(0,cpus)
    run_id='pressure-'+uuid.uuid4().hex;node_id='pressure-node-'+uuid.uuid4().hex
    worker_env=dict(TMPDIR=str(root/'tmp'),XDG_CACHE_HOME=str(root/'cache'),
        PYTHONPATH=str(manager/'python'),PYTHONDONTWRITEBYTECODE='1',OMP_NUM_THREADS='1',MKL_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1',
        CUDA_VISIBLE_DEVICES=args.gpu_uuid if args.mode=='gpu' else '',CUDA_CACHE_PATH=str(root/'cache/cuda'))
    env={**os.environ,**worker_env}
    report={'schema_version':1,'run_id':run_id,'mode':args.mode,'targets':TARGETS,'cases':[],
        'started_unix':time.time(),'status':'running','manager_binary_sha256':sha256_file(binary),
        'cpu_ids':cpus,'gpu_uuid':args.gpu_uuid,'wall_time_seconds':1200,'actual_24_hour_soak':False,
        'scope':'one-node native synthetic agent-policy comparison; not two-node application acceptance',
        'bounds':{'managed_workers':1,'cpu_scope':len(cpus),'managed_ram_mib':2048,'protected_ram_mib':2048,
            'managed_vram_mib':1024 if args.mode=='gpu' else 0,'protected_vram_mib':1024 if args.mode=='gpu' else 0,
            'min_ram_free_bytes':16*1024**3,'min_gpu_free_bytes':4*1024**3,'max_output_bytes':2*1024**3}}
    services={};children={};client=None;ledger=AllocationLedger([node_id]);samples=[];stop=[]
    start=time.monotonic();latest=None;gpu=None;journal=StatusJournal();observations=None
    previous={sig:signal.signal(sig,lambda sig,_:stop.append(sig)) for sig in (signal.SIGINT,signal.SIGTERM)}
    def tick():
        nonlocal latest
        if stop or (output/'STOP').exists() or time.monotonic()-start>1200:raise InterruptedError('comparison stop/deadline reached')
        if any(value.poll() is not None for value in services.values()):raise RuntimeError('owned manager service failed')
        guard=local_guard(output,{'min_ram_free_bytes':16*1024**3,'min_disk_free_bytes':16*1024**3,'max_output_bytes':2*1024**3})
        if guard['errors']:raise RuntimeError(','.join(guard['errors']))
        latest=client.status();ledger.observe(latest)
        samples.append(manager_samples(services,output/'agent-state',ledger))
        observations.write(json.dumps({'observed_unix':time.time(),'observed_monotonic':time.monotonic(),**journal.encode(latest,time.monotonic()-start),'guard':guard})+'\n')
        observations.flush()
        if gpu is not None:
            reading=gpu.sample()
            if reading['free_bytes']<4*1024**3:raise RuntimeError('GPU free-memory reserve below4GiB')
        time.sleep(.2)
    try:
        client=deployment(output,binary,cpus,node_id,args.gpu_uuid,1200,env)
        node=json.loads((output/'node.json').read_text());node['cpu']['reserve_physical_cores']=1
        node['node_mode']='opportunistic';node['gpu']['scale_up_cooldown_ms']=3000
        atomic_json(output/'node.json',node)
        agent=json.loads((output/'agent.json').read_text());agent.update(max_workers=1,
            capacity={'cpu_millicores':len(cpus)*1000,'ram_mib':4096,'gpu_memory_mib':{args.gpu_uuid:1024} if args.mode=='gpu' else {}})
        atomic_json(output/'agent.json',agent)
        doctor=subprocess.run([str(binary),'--config',str(output/'node.json'),'doctor'],capture_output=True,text=True,timeout=30,check=True,env=env)
        atomic_json(output/'doctor.json',json.loads(doctor.stdout))
        observations=(output/'observations.jsonl').open('x')
        services['coordinator']=OwnedProcess([str(binary),'coordinator','--deployment',str(output/'coordinator.json')],manager,output/'coordinator.log',env)
        deadline=time.monotonic()+15
        while True:
            try:client.status();break
            except Exception:
                if time.monotonic()>deadline:raise
                time.sleep(.1)
        services['agent']=OwnedProcess([str(binary),'--config',str(output/'node.json'),'agent','--deployment',str(output/'agent.json')],manager,output/'agent.log',env)
        deadline=time.monotonic()+30
        while not latest or not latest.get('nodes'):
            if time.monotonic()>deadline:raise TimeoutError('agent did not register')
            tick()
        client.put_pool(run_id,[node_id],max_workers=1,min_workers=0,allocation_class='opportunistic')
        if args.mode=='gpu':gpu=Nvml(args.gpu_uuid)
        for repetition in range(3):
            order=['protected_alone','unmanaged_idle','managed_idle','unmanaged_contention','managed_contention']
            if repetition%2:order=['protected_alone','managed_idle','unmanaged_idle','managed_contention','unmanaged_contention']
            for scenario in order:
                case={'repetition':repetition,'scenario':scenario,'status':'running','started_monotonic':time.monotonic()}
                report['cases'].append(case);folder=output/f'r{repetition}-{scenario}';folder.mkdir()
                managed=scenario.startswith('managed');assignment=None;task_id=run_id+'.'+uuid.uuid4().hex
                if scenario!='protected_alone':
                    probe=folder/'probe';argv=[str(python),str(manager/'tools/pressure_probe.py'),'--mode',args.mode,
                        '--seconds','40' if 'contention' in scenario else '20','--cpu',str(cpus[0]),'--output',str(probe)]
                    if managed:
                        task=probe_task(task_id,argv,manager,worker_env,args.mode,args.gpu_uuid)
                        client.submit(task_id,run_id,[task]);case['task_id']=task_id
                    else:children['probe']=OwnedProcess(argv,manager,folder/'probe.log',env)
                    deadline=time.monotonic()+60
                    while not (probe/'ready.json').exists():
                        if time.monotonic()>deadline:raise TimeoutError('probe readiness was not achieved')
                        if 'probe' in children and children['probe'].poll() is not None:raise RuntimeError('unmanaged probe failed before readiness')
                        tick()
                    if managed:
                        record=next(task for task in latest['tasks'] if task['task_id']==task_id)
                        assignment=record['assignment_id'];case['assignment_id']=assignment
                if scenario=='protected_alone' or 'contention' in scenario:
                    external=folder/'protected'
                    event={'id':'protected','mode':'protected_'+args.mode,'at_seconds':0,'duration_seconds':20,
                        'cpu_iterations':100000,'allocate_mib':128}
                    pressure={'execution_approved':True,'shared_server':False,'duration_seconds':20,'output':str(external),
                        'cpu_ids':[cpus[-1]],'gpu_uuid':args.gpu_uuid,'events':[event],
                        'limits':{'min_ram_free_bytes':16*1024**3,'min_disk_free_bytes':16*1024**3,'max_output_bytes':256*1024**2}}
                    atomic_json(folder/'pressure.json',pressure)
                    case['pressure_started_unix']=time.time();case['pressure_started_monotonic']=time.monotonic()
                    children['protected']=OwnedProcess([str(python),str(manager/'tools/pressure.py'),'--config',str(folder/'pressure.json'),'--execute'],manager,folder/'pressure.log',env)
                    case['protected_identity']=children['protected'].identity
                deadline=time.monotonic()+65
                while True:
                    tick()
                    for name,child in children.items():
                        code=child.poll()
                        if code is not None and code!=0:raise RuntimeError(f'{name} guard failed or process exited abnormally ({code})')
                    if assignment:
                        record=ledger.allocations.get(assignment)
                        if record and record['phase']=='released':
                            case.setdefault('released_monotonic',time.monotonic())
                            if not (probe/'result.json').exists():raise RuntimeError('managed probe released without its bounded-work result')
                    if gpu is not None and scenario!='protected_alone' and (probe/'ready.json').exists() and not (probe/'result.json').exists():
                        pid=json.loads((probe/'ready.json').read_text())['pid'];reading=gpu.sample(target_pid=pid)
                        if reading['own_memory_bytes'] is None or reading['own_memory_bytes']>1024**3:raise RuntimeError('probe VRAM unavailable or exceeds1GiB')
                    external_done='protected' not in children or children['protected'].poll() is not None
                    probe_done=scenario=='protected_alone' or (probe/'result.json').exists()
                    if external_done and probe_done and (not managed or case.get('released_monotonic')):break
                    if time.monotonic()>deadline:raise TimeoutError('finite comparison case exceeded its bound')
                case['owned_cleanup']={name:child.stop() for name,child in children.items()};children.clear()
                if any(value['exit_code']!=0 for value in case['owned_cleanup'].values()):raise RuntimeError('owned protected/probe process failed or was signaled')
                if scenario!='protected_alone':
                    case['probe']=json.loads((probe/'result.json').read_text())
                    if scenario.endswith('_idle') and case['probe']['drained']:raise RuntimeError('idle baseline drained; profile lacks usable idle capacity')
                if scenario=='protected_alone' or 'contention' in scenario:
                    case['protected']=json.loads((external/'event-protected.json').read_text())
                    if case['protected']['status']!='completed':raise RuntimeError('protected workload did not complete')
                    case['clock_consistency_error_seconds']=abs((time.time()-case['pressure_started_unix'])-(time.monotonic()-case['pressure_started_monotonic']))
                    case['protected_registry_membership_verified']=verify_external_registry(output/'agent-state',case['protected_identity'])
                if gpu is not None:
                    case['gpu_release_observations']={name:confirm_gpu_release(gpu,value['pid']) for name,value in case['owned_cleanup'].items()}
                if managed:
                    outcome=json.loads((output/'agent-state/attempts'/assignment/'execution-outcome.json').read_text())
                    case['outcome']=outcome;case['yielded']=outcome['yielded']
                    if 'contention' in scenario:
                        onset=case['protected']['demand_started_unix']
                        decision=policy_decision(output/'agent-state',assignment,onset)
                        case['policy_decision']=decision
                        if decision:
                            delay=decision['observed_unix']-onset
                            case['decision_delay_seconds']=delay
                            case['decision_to_release_seconds']=case['released_monotonic']-(case['protected']['demand_started_monotonic']+delay)
                case['status']='completed';atomic_json(folder/'case.json',case);atomic_json(output/'report.json',report)
                deadline=time.monotonic()+4
                while time.monotonic()<deadline:tick()
        report.update(status='completed_pending_metric_review',target_evaluation=evaluate(report['cases']))
    except BaseException as error:report.update(status='failed',error=type(error).__name__+': '+str(error))
    finally:
        report['cleanup']={}
        for name,child in children.items():
            try:report['cleanup'][name]=child.stop()
            except Exception as error:report.setdefault('unresolved_children',{})[name]=str(error)
        if client is not None:
            try:
                latest=client.status();ledger.observe(latest)
                for job in latest.get('jobs',[]):
                    if job['job_id'].startswith(run_id):client.cancel(job['job_id'])
                if latest.get('nodes'):client.drain_node(node_id)
                deadline=time.monotonic()+30
                while ledger.unresolved() and time.monotonic()<deadline:
                    time.sleep(.2);latest=client.status();ledger.observe(latest)
                if services:samples.append(manager_samples(services,output/'agent-state',ledger))
            except Exception as error:report['cleanup_error']=str(error)
        for name in ['agent','coordinator']:
            if name in services:
                try:report['cleanup'][name]=services[name].stop(grace=20)
                except Exception as error:report.setdefault('unresolved_services',{})[name]=str(error)
        if gpu is not None:
            try:report['final_owned_gpu_release']={name:confirm_gpu_release(gpu,value['pid']) for name,value in report['cleanup'].items() if name in {'protected','probe'}}
            except Exception as error:report['cleanup_error']=str(error)
            finally:gpu.close()
        if observations is not None:observations.flush();os.fsync(observations.fileno());observations.close()
        report.update(elapsed_seconds=time.monotonic()-start,unresolved_allocations=ledger.unresolved(),manager_metrics=manager_metrics(samples),manager_samples=samples)
        if report['unresolved_allocations'] or report.get('cleanup_error') or report.get('unresolved_services') or report.get('unresolved_children'):
            report['status']='failed_cleanup_requires_reconciliation'
        atomic_json(output/'report.json',report)
        if latest is not None:atomic_json(output/'final-status.json',latest)
        for sig,handler in previous.items():signal.signal(sig,handler)
    return report


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',required=True);parser.add_argument('--output',required=True)
    parser.add_argument('--mode',choices=['cpu','gpu'],required=True);parser.add_argument('--gpu-uuid',required=True)
    parser.add_argument('--execute',action='store_true');args=parser.parse_args()
    if not args.execute:print(json.dumps({'mode':'plan_only','configuration':vars(args),'targets':TARGETS}));sys.exit(0)
    result=run(args);print(json.dumps(result,indent=2));sys.exit(0 if result['status']=='completed_pending_metric_review' else 1)
