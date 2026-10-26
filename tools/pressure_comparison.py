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
from cedegrid import command_task,sha256_file
from anchor_validation import deployment,manager_samples,manager_metrics
from native_validation import physical_cpus
from pressure import Nvml,quantile
from soak import AllocationLedger,StatusJournal
from validation_runtime import OwnedProcess,atomic_json,inside_home,home_executable,local_guard
from runtime_config import atomic_runtime_config, scale_up_cooldowns, read_runtime_config

CONTENTION_MODES=('yield_first','neutral','performance')
# Mirrors MIN_RAM_FLOOR_MIB in src/config.rs; below twice it a floor is a caution option.
MIN_RAM_FLOOR_MIB=512
TARGETS={'matched_idle_throughput_ratio_min':.90,'decision_p95_seconds_max':1.,
    'release_p95_seconds_max':10.,'post_yield_p99_ratio_max':1.20,
    'manager_peak_rss_bytes_max':512*1024**2,'manager_active_cpu_cores_max':.5,
    'minimum_post_yield_requests':100,'repetitions':3}


def protected_samples(event,after=None):
    return [request['latency_ms'] for row in event.get('results',[]) for request in row.get('requests',[])
        if after is None or request['arrival_monotonic']>=after]


def node_contention(node,mode,ram_floor_mib=None):
    """The node settings for one gate run: the contention mode and, when set, a RAM
    floor of exactly that size, with the caution options both need acknowledged."""
    if mode not in CONTENTION_MODES:raise ValueError('unknown contention mode')
    node=copy.deepcopy(node);caution=[]
    contention={'default_mode':mode,**scale_up_cooldowns(3000)}
    if mode=='performance':caution.append('performance_mode')
    if ram_floor_mib is not None:
        if type(ram_floor_mib) is not int or ram_floor_mib<MIN_RAM_FLOOR_MIB:
            raise ValueError(f'RAM floor must be at least {MIN_RAM_FLOOR_MIB} MiB')
        node['ram']={**node.get('ram',{}),'reserve_percent':0,'reserve_mib':ram_floor_mib}
        if ram_floor_mib<2*MIN_RAM_FLOOR_MIB:caution.append('small_floor')
    if caution:contention['acknowledge_caution']=caution
    node['contention']=contention
    return node


def step_timing(history,assignments,onset_unix,ended_unix=None):
    """Per resource, from `cedegrid history` rows: seconds from the demand onset to
    the first step on own work and to own usage at or below the target share, and
    from the demand's end to the step's reversal, with how often each step began."""
    timing={};last={}
    for row in sorted(history,key=lambda row:row['snapshot']['observed_at_unix_ms']):
        at=row['snapshot']['observed_at_unix_ms']/1000;decision=row['decision'];steps={}
        for action in decision.get('actions',[]):
            if action['allocation_id'] in assignments:steps.setdefault(action['resource'],set()).add(action['step'])
        for key in set(steps)|set(timing):
            entry=timing.setdefault(key,{'first_step_seconds':None,'reclaim_seconds':None,'reversal_seconds':None,'steps':{}})
            current=steps.get(key,set())
            for step in current-last.get(key,set()):entry['steps'][step]=entry['steps'].get(step,0)+1
            last[key]=current
            if current and at>=onset_unix and entry['first_step_seconds'] is None:entry['first_step_seconds']=at-onset_unix
            if entry['first_step_seconds'] is None:continue
            status=decision.get('resources',{}).get(key,{});own=status.get('own');target=status.get('target_share')
            if entry['reclaim_seconds'] is None and own and target is not None and own[0]<=target:
                entry['reclaim_seconds']=at-onset_unix
            if ended_unix is not None and at>=ended_unix and not current and entry['reversal_seconds'] is None:
                entry['reversal_seconds']=at-ended_unix
    return timing


def history_audit(history,own):
    """SC-005 from `cedegrid history`: every action and drain names own work."""
    actions=[action['allocation_id'] for row in history for action in row['decision'].get('actions',[])]
    drains=[allocation for row in history for allocation in row['decision'].get('would_drain',[])]
    return {'observations':len(history),'own_actions':sum(1 for item in actions if item in own),
        'actions_outside_own_allocations':sum(1 for item in actions if item not in own),
        'drains_outside_own_allocations':sum(1 for item in drains if item not in own)}


def read_history(binary,config,env):
    done=subprocess.run([str(binary),'--config',str(config),'history','--limit','10000'],
        capture_output=True,text=True,timeout=60,check=True,env=env)
    return json.loads(done.stdout)


PROTECTED_LIMITS=('cpu.max','memory.high','memory.max')


def protected_snapshot(pid,proc_root=Path('/proc'),cgroup_root=Path('/sys/fs/cgroup')):
    """A read-only view of a test-owned external process: nice value, start time,
    cgroup path and that cgroup's limits (an absent file is None)."""
    stat=(Path(proc_root)/str(pid)/'stat').read_text()
    fields=stat[stat.rindex(')')+2:].split()
    path=next((line[3:].strip() for line in (Path(proc_root)/str(pid)/'cgroup').read_text().splitlines()
        if line.startswith('0::')),None)
    snapshot={'pid':pid,'nice':int(fields[16]),'start_ticks':int(fields[19]),'cgroup':path}
    for name in PROTECTED_LIMITS:
        file=Path(cgroup_root)/path.lstrip('/')/name if path is not None else None
        snapshot[name]=file.read_text().strip() if file is not None and file.is_file() else None
    return snapshot


def verify_protected_unchanged(before,after):
    """SC-005: CedeGrid changed nothing about a process it did not launch."""
    changed=[name for name in before if before[name]!=after.get(name)]
    if changed:raise RuntimeError('protected process changed: '+', '.join(changed))
    return changed


def evaluate(cases):
    ratios=[];decisions=[];releases=[];tails=[];missing=[];graduated=0
    for repetition in range(TARGETS['repetitions']):
        group={case['scenario']:case for case in cases if case['repetition']==repetition and case.get('status')=='completed'}
        if len(group)!=5:missing.append(f'repetition{repetition}: incomplete matched cases');continue
        before=group['unmanaged_idle']['probe'];after=group['managed_idle']['probe']
        ratios.append((after['completed_units']/after['elapsed_seconds'])/(before['completed_units']/before['elapsed_seconds']))
        alone=protected_samples(group['protected_alone']['protected'])
        active=group['managed_contention']
        # A graduated step yields without a release; its post-yield cutoff is the reclaim.
        stepped=active.get('yielded') and active.get('drained') is False
        released=active.get('reclaimed_monotonic' if stepped else 'released_monotonic')
        if active.get('clock_consistency_error_seconds',0)>.25:
            missing.append(f'repetition{repetition}: wall/monotonic clocks diverged');continue
        if released is None or not active.get('yielded'):
            missing.append(f'repetition{repetition}: no verified pressure-triggered yield');continue
        graduated+=bool(stepped)
        post=protected_samples(active['protected'],released)
        if len(post)<TARGETS['minimum_post_yield_requests'] or not alone:
            missing.append(f'repetition{repetition}: insufficient post-yield requests');continue
        tails.append(quantile(post,.99)/quantile(alone,.99))
        if active.get('decision_delay_seconds') is None:missing.append(f'repetition{repetition}: no policy decision timestamp')
        else:decisions.append(active['decision_delay_seconds'])
        if stepped:pass
        elif active.get('decision_to_release_seconds') is None:missing.append(f'repetition{repetition}: no release timing')
        else:releases.append(active['decision_to_release_seconds'])
    values={'matched_idle_throughput_ratio_min':quantile(ratios,.5),'decision_p95_seconds_max':quantile(decisions,.95),
        'release_p95_seconds_max':quantile(releases,.95),'post_yield_p99_ratio_max':quantile(tails,.5)}
    def status(name,value):
        if name=='release_p95_seconds_max' and value is None and graduated==TARGETS['repetitions'] and not missing:
            return 'not_applicable'
        if value is None or missing:return 'inconclusive'
        return 'passed' if (value>=TARGETS[name] if name.endswith('_min') else value<=TARGETS[name]) else 'failed'
    return {'missing':missing,'metrics':{name:{'observed':value,'target':TARGETS[name],'status':status(name,value)}
        for name,value in values.items()}}


def policy_decision(state,assignment,after_unix):
    """The first decision after `after_unix` that yields the assignment: a drain, or
    a graduated step on any resource."""
    database=state/'state.sqlite3'
    with sqlite3.connect(database.resolve().as_uri()+'?mode=ro',uri=True) as connection:
        for at,payload in connection.execute('SELECT observed_at_unix_ms,decision_json FROM observations WHERE observed_at_unix_ms>=? ORDER BY id',(int(after_unix*1000),)):
            value=json.loads(payload)
            if assignment in value.get('would_drain',[]):return {'observed_unix':at/1000,'decision':value,'kind':'drain'}
            if any(action['allocation_id']==assignment for action in value.get('actions',[])):
                return {'observed_unix':at/1000,'decision':value,'kind':'step'}
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


def case_finished(scenario,probe,children,managed,released):
    """The protected load and the probe are done. An unmanaged probe must also have exited:
    a GPU probe still tears down its CUDA context after writing its result."""
    external_done='protected' not in children or children['protected'].poll() is not None
    probe_done=scenario=='protected_alone' or ((probe/'result.json').exists()
        and ('probe' not in children or children['probe'].poll() is not None))
    return external_done and probe_done and (not managed or bool(released))


def probe_task(task_id,argv,manager,env,mode,gpu_uuid):
    return command_task(task_id,argv,str(manager),env=env,cpu_millicores=500 if mode=='cpu' else 1000,
        ram_mib=2048,gpu_vram_mib={gpu_uuid:1024} if mode=='gpu' else {},
        allocation_class='opportunistic',single_process=True,no_escape=True,replay_safe=True,max_attempts=1)


def run(args):
    if not args.execute or sys.platform!='linux':raise ValueError('explicit execution on the authorized Linux anchor is required')
    if args.mode not in {'cpu','gpu'} or not args.gpu_uuid.startswith('GPU-'):raise ValueError('explicit mode and GPU UUID required')
    root=inside_home(args.root);manager=Path(__file__).resolve().parents[1]
    output=inside_home(args.output);output.mkdir(mode=0o700,parents=True,exist_ok=False)
    python=home_executable(root/'venv-isolated/bin/python');binary=inside_home(manager/'target/release/cedegrid')
    cpus=physical_cpus(2 if args.mode=='cpu' else 3)
    if len(cpus)!=(2 if args.mode=='cpu' else 3):raise RuntimeError('required physical CPU scope unavailable')
    os.sched_setaffinity(0,cpus)
    run_id='pressure-'+uuid.uuid4().hex;node_id='pressure-node-'+uuid.uuid4().hex
    worker_env=dict(TMPDIR=str(root/'tmp'),XDG_CACHE_HOME=str(root/'cache'),
        PYTHONPATH=str(manager/'python'),PYTHONDONTWRITEBYTECODE='1',OMP_NUM_THREADS='1',MKL_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1',
        CUDA_VISIBLE_DEVICES=args.gpu_uuid if args.mode=='gpu' else '',CUDA_CACHE_PATH=str(root/'cache/cuda'))
    env={**os.environ,**worker_env}
    report={'schema_version':1,'run_id':run_id,'mode':args.mode,'targets':TARGETS,'cases':[],
        'contention_mode':args.contention_mode,'ram_floor_mib':args.ram_floor_mib,
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
        node=read_runtime_config(output/'node.toml');node['cpu']['reserve_physical_cores']=1
        node['node_mode']='opportunistic';node['gpu']['scale_up_cooldown_ms']=3000
        node=node_contention(node,args.contention_mode,args.ram_floor_mib)
        atomic_runtime_config(output/'node.toml',node, 'node')
        agent=read_runtime_config(output/'agent.toml');agent.update(max_workers=1,
            capacity={'cpu_millicores':len(cpus)*1000,'ram_mib':4096,'gpu_memory_mib':{args.gpu_uuid:1024} if args.mode=='gpu' else {}})
        atomic_runtime_config(output/'agent.toml',agent, 'agent')
        doctor=subprocess.run([str(binary),'--config',str(output/'node.toml'),'doctor'],capture_output=True,text=True,timeout=30,check=True,env=env)
        atomic_json(output/'doctor.json',json.loads(doctor.stdout))
        observations=(output/'observations.jsonl').open('x')
        services['coordinator']=OwnedProcess([str(binary),'coordinator','--deployment',str(output/'coordinator.toml')],manager,output/'coordinator.log',env)
        deadline=time.monotonic()+15
        while True:
            try:client.status();break
            except Exception:
                if time.monotonic()>deadline:raise
                time.sleep(.1)
        services['agent']=OwnedProcess([str(binary),'--config',str(output/'node.toml'),'agent','--deployment',str(output/'agent.toml')],manager,output/'agent.log',env)
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
                    case['protected_snapshots']={'before':protected_snapshot(children['protected'].child.pid)}
                deadline=time.monotonic()+65
                while True:
                    tick()
                    # The last view of the protected process while it still runs.
                    if 'protected' in children and children['protected'].poll() is None:
                        try:case['protected_snapshots']['after']=protected_snapshot(children['protected'].child.pid)
                        except (OSError,ValueError,IndexError):pass
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
                    if case_finished(scenario,probe,children,managed,case.get('released_monotonic')):break
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
                    snapshots=case['protected_snapshots']
                    if 'after' not in snapshots:raise RuntimeError('no snapshot of the protected process while it ran')
                    snapshots['changed']=verify_protected_unchanged(snapshots['before'],snapshots['after'])
                if gpu is not None:
                    case['gpu_release_observations']={name:confirm_gpu_release(gpu,value['pid']) for name,value in case['owned_cleanup'].items()}
                if managed:
                    outcome=json.loads((output/'agent-state/attempts'/assignment/'execution-outcome.json').read_text())
                    case['outcome']=outcome;case['drained']=outcome['yielded'];case['yielded']=outcome['yielded']
                    if 'contention' in scenario:
                        onset=case['protected']['demand_started_unix']
                        ended=case['protected']['started_unix']+case['protected']['elapsed_seconds']
                        decision=policy_decision(output/'agent-state',assignment,onset)
                        case['policy_decision']=decision
                        case['step_timing']=step_timing(read_history(binary,output/'node.toml',env),{assignment},onset,ended)
                        if decision:
                            # A graduated step is a yield too, not only a drain.
                            case['yielded']=True
                            delay=decision['observed_unix']-onset
                            case['decision_delay_seconds']=delay
                            if case['drained']:
                                case['decision_to_release_seconds']=case['released_monotonic']-(case['protected']['demand_started_monotonic']+delay)
                            reclaimed=[entry['reclaim_seconds'] for entry in case['step_timing'].values() if entry['reclaim_seconds'] is not None]
                            if reclaimed:case['reclaimed_monotonic']=case['protected']['demand_started_monotonic']+max(reclaimed)
                case['status']='completed';atomic_json(folder/'case.json',case);atomic_json(output/'report.json',report)
                deadline=time.monotonic()+4
                while time.monotonic()<deadline:tick()
        # SC-005 needs both: every action names own work, and no protected process changed.
        own={case['assignment_id'] for case in report['cases'] if case.get('assignment_id')}
        audit=history_audit(read_history(binary,output/'node.toml',env),own)
        report['sc005']={'history_audit':audit,'protected_snapshots_unchanged':all(
            case['protected_snapshots']['changed']==[] for case in report['cases'] if 'protected_snapshots' in case)}
        if audit['actions_outside_own_allocations'] or audit['drains_outside_own_allocations']:
            raise RuntimeError('history audit found actions outside own allocations')
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
    parser.add_argument('--contention-mode',choices=CONTENTION_MODES,default='neutral',
        help='node default mode for the run; performance is acknowledged as a caution option')
    parser.add_argument('--ram-floor-mib',type=int,
        help=f'RAM floor of exactly this many MiB (reserve_percent 0), at least {MIN_RAM_FLOOR_MIB}; below twice that, small_floor is acknowledged')
    parser.add_argument('--execute',action='store_true');args=parser.parse_args()
    try:node_contention({},args.contention_mode,args.ram_floor_mib)
    except ValueError as error:parser.error(str(error))
    if not args.execute:print(json.dumps({'mode':'plan_only','configuration':vars(args),'targets':TARGETS}));sys.exit(0)
    result=run(args);print(json.dumps(result,indent=2));sys.exit(0 if result['status']=='completed_pending_metric_review' else 1)
