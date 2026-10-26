#!/usr/bin/env python3
"""Opt-in one-node Linux gate for the Performance ramp (SC-004, SC-012).

Starts a fresh private home-local coordinator and agent in Performance, submits enough
small duty-cycled CPU tasks to fill the scope, and reads the agent's decisions back to
time own CPU reaching the ramp target. With --contender-at, an unmanaged busy loop the
gate itself launches starts mid-ramp; CedeGrid must never act on it. With
--contender-uid, the contender is another account's process, started by that account and
found by its marker; the gate only reads its /proc entries. --device gpu times own GPU
compute instead, with CUDA busy tasks.
"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import sys
import time
import uuid

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'python'))
from cedegrid import command_task
from anchor_validation import deployment
from native_validation import physical_cpus
from validation_runtime import OwnedProcess,atomic_json,inside_home,home_executable,local_guard
from runtime_config import atomic_runtime_config,read_runtime_config

TASK_MILLICORES=250
GPU_TASK_MIB=512
# Holds the process's CPU time at `duty` percent of wall time until the deadline, so
# usage tracks the request whatever sleep overshoots.
DUTY_LOOP=('import sys,time\n'
    'start=time.monotonic();end=start+float(sys.argv[1]);duty=float(sys.argv[2])/100;cpu=time.process_time()\n'
    'while time.monotonic()<end:\n'
    '    if time.process_time()-cpu<duty*(time.monotonic()-start):\n'
    '        until=time.monotonic()+0.001\n'
    '        while time.monotonic()<until:pass\n'
    '    else:time.sleep(0.002)\n')


# Spins on the device until the deadline, so GPU compute is busy while it runs.
CUDA_LOOP=('import sys,time,torch\n'
    'a=torch.ones(2048,2048,device="cuda");end=time.monotonic()+float(sys.argv[1])\n'
    'while time.monotonic()<end:\n'
    '    a.matmul(a);torch.cuda.synchronize()\n')


def cpu_series(rows):
    """(observed seconds, own lower bound, external, target share) per CPU decision."""
    series=[]
    for at_ms,decision in rows:
        cpu=decision.get('resources',{}).get('cpu')
        if not cpu or cpu.get('own') is None:continue
        series.append((at_ms/1000,cpu['own'][0],cpu.get('external'),cpu.get('target_share')))
    return series


def device_series(rows,kind,uuid):
    """(observed seconds, own lower bound, external, target share) per decision for one
    device resource, such as gpu_compute."""
    series=[]
    for at_ms,decision in rows:
        status=decision.get('resources',{}).get(f'{kind}:{uuid}')
        if not status or status.get('own') is None:continue
        series.append((at_ms/1000,status['own'][0],status.get('external'),status.get('target_share')))
    return series


def ramp_target(series,start,percent=90):
    """Percent of the first target share after `start`, for the report: capacity minus
    the margin and external use when work starts."""
    share=next((share for at,_,_,share in series if at>=start and share),None)
    if share is None:raise ValueError('no CPU target share was observed after submission')
    return share*percent//100


def ramp_seconds(series,start,percent=90):
    """Seconds until own CPU reaches `percent` of that sample's target share. The share
    follows external use, which own work's supervisors and the services add to."""
    return seconds_to_share(series,start,lambda own,share:own>=share*percent//100)


def seconds_to_share(series,start,predicate):
    return next((round(at-start,3) for at,own,_,share in series if at>=start and share and predicate(own,share)),None)


def cpu_ticks(pid,proc=Path('/proc')):
    """utime plus stime of a process, from /proc/<pid>/stat, read only."""
    fields=(proc/str(pid)/'stat').read_text().rsplit(')',1)[1].split()
    return int(fields[11])+int(fields[12])


def external_contender(proc,uid,marker):
    """The process of `uid` whose arguments include `marker`, by PID, UID and start
    ticks, found by reading /proc only. The gate never acts on it."""
    for folder in proc.iterdir():
        if not folder.name.isdigit():continue
        try:
            status=(folder/'status').read_text()
            real=next(line.split()[1] for line in status.splitlines() if line.startswith('Uid:'))
            args=(folder/'cmdline').read_bytes().split(b'\0')
            start=int((folder/'stat').read_text().rsplit(')',1)[1].split()[19])
        except (OSError,StopIteration,IndexError,ValueError):continue
        if int(real)==uid and marker.encode() in args:
            return {'pid':int(folder.name),'uid':uid,'start_ticks':start}
    return None


def check_options(args):
    if args.contender_uid is not None and (not args.contender_marker or args.contender_at is None):
        raise ValueError('--contender-uid needs --contender-marker and --contender-at')
    if args.device=='gpu' and not args.gpu_uuid:
        raise ValueError('--device gpu needs --gpu-uuid')


def served_seconds(samples,start,threshold=0.9):
    """Seconds from `start` until the contender's CPU over one sample interval reaches
    `threshold` of a core. `samples` holds (seconds, CPU seconds) pairs."""
    for (t0,c0),(t1,c1) in zip(samples,samples[1:]):
        if t1>start and t1>t0 and (c1-c0)/(t1-t0)>=threshold:
            return round(t1-start,3)
    return None


def seconds_to(series,start,predicate):
    return next((round(at-start,3) for at,own,external,share in series if at>=start and predicate(own,external)),None)


def foreign_actions(rows,assignments):
    """Every action on an allocation the gate did not submit; there must be none."""
    return [action for _,decision in rows for action in decision.get('actions',[])
        if action.get('allocation_id') not in assignments]


def decisions(state,after_unix):
    with sqlite3.connect((state/'state.sqlite3').resolve().as_uri()+'?mode=ro',uri=True) as connection:
        return [(at,json.loads(payload)) for at,payload in connection.execute(
            'SELECT observed_at_unix_ms,decision_json FROM observations WHERE observed_at_unix_ms>=? ORDER BY id',
            (int(after_unix*1000),))]


def busy_task(task_id,python,seconds,manager,env):
    return command_task(task_id,[str(python),'-c',DUTY_LOOP,str(seconds),str(TASK_MILLICORES/10)],str(manager),
        env=env,cpu_millicores=TASK_MILLICORES,ram_mib=64,gpu_vram_mib={},allocation_class='opportunistic',
        single_process=True,no_escape=True,replay_safe=True,max_attempts=1)


def performance_defaults(node):
    """Performance at its defaults: no cooldown override, no CPU reserve. The deployment's
    legacy GPU cooldown would otherwise hold GPU admission for 30 s after the agent starts."""
    node['node_mode']='opportunistic'
    node['cpu']['reserve_physical_cores']=0
    node.get('gpu',{}).pop('scale_up_cooldown_ms',None)
    node['contention']={'default_mode':'performance','acknowledge_caution':['performance_mode']}
    return node


def gpu_task(task_id,python,seconds,manager,env,gpu_uuid):
    return command_task(task_id,[str(python),'-c',CUDA_LOOP,str(seconds)],str(manager),
        env=env,cpu_millicores=TASK_MILLICORES,ram_mib=2048,gpu_vram_mib={gpu_uuid:GPU_TASK_MIB},
        allocation_class='opportunistic',single_process=True,no_escape=True,replay_safe=True,max_attempts=1)


def run(args):
    if not args.execute or sys.platform!='linux':raise ValueError('explicit execution on an authorized Linux test host is required')
    check_options(args)
    gpu=args.device=='gpu'
    root=inside_home(args.root);manager=Path(__file__).resolve().parents[1]
    output=inside_home(args.output);output.mkdir(mode=0o700,parents=True,exist_ok=False)
    python=home_executable(root/'venv-isolated/bin/python');binary=inside_home(manager/'target/release/cedegrid')
    cpus=physical_cpus(args.cpus)
    if len(cpus)!=args.cpus:raise RuntimeError('required physical CPU scope unavailable')
    os.sched_setaffinity(0,cpus)
    run_id='ramp-'+uuid.uuid4().hex;node_id='ramp-node-'+uuid.uuid4().hex
    workers=args.gpu_tasks if gpu else len(cpus)*1000//TASK_MILLICORES
    worker_env=dict(TMPDIR=str(root/'tmp'),XDG_CACHE_HOME=str(root/'cache'),PYTHONNOUSERSITE='1')
    env={**os.environ,**worker_env}
    report={'run_id':run_id,'device':args.device,'cpus':cpus,'workers':workers,'task_millicores':TASK_MILLICORES,
        'contender_at_seconds':args.contender_at,'contender_pinned':not args.contender_unpinned,
        'contender_external':args.contender_uid is not None,'status':'running'}
    services={};contender=None
    try:
        report['guard']=local_guard(output,{'min_ram_free_bytes':8*1024**3,'min_disk_free_bytes':8*1024**3,'max_output_bytes':1024**3})
        client=(deployment(output,binary,cpus,node_id,args.gpu_uuid,args.seconds+120,env) if gpu
            else deployment(output,binary,cpus,node_id,'',args.seconds+120,env,device='cpu'))
        node=performance_defaults(read_runtime_config(output/'node.toml'))
        atomic_runtime_config(output/'node.toml',node,'node')
        agent=read_runtime_config(output/'agent.toml')
        agent.update(max_workers=workers,capacity={'cpu_millicores':len(cpus)*1000,'ram_mib':4096 if not gpu else 8192,
            'gpu_memory_mib':{args.gpu_uuid:GPU_TASK_MIB*workers} if gpu else {}})
        atomic_runtime_config(output/'agent.toml',agent,'agent')
        services['coordinator']=OwnedProcess([str(binary),'coordinator','--deployment',str(output/'coordinator.toml')],manager,output/'coordinator.log',env)
        deadline=time.monotonic()+15
        while True:
            try:client.status();break
            except Exception:
                if time.monotonic()>deadline:raise
                time.sleep(.1)
        services['agent']=OwnedProcess([str(binary),'--config',str(output/'node.toml'),'agent','--deployment',str(output/'agent.toml')],manager,output/'agent.log',env)
        deadline=time.monotonic()+30
        while not client.status().get('nodes'):
            if time.monotonic()>deadline:raise TimeoutError('agent did not register')
            time.sleep(.2)
        # Let the agent settle on an idle scope before work arrives.
        time.sleep(args.settle)
        client.put_pool(run_id,[node_id],max_workers=workers,min_workers=0,allocation_class='opportunistic')
        tasks=[gpu_task(f'{run_id}.{index}',python,args.seconds,manager,worker_env,args.gpu_uuid) if gpu
            else busy_task(f'{run_id}.{index}',python,args.seconds,manager,worker_env) for index in range(workers)]
        # A fixed submission time lets another account start its contender on the same clock.
        if args.submit_at is not None:time.sleep(max(0.,args.submit_at-time.time()))
        report['submitted_unix']=submitted=time.time();client.submit(run_id,run_id,tasks)
        ticks=os.sysconf('SC_CLK_TCK');contender_cpu=[];external=None
        while time.time()<submitted+args.seconds:
            if args.contender_uid is not None:
                if external is None and time.time()>=submitted+args.contender_at:
                    external=external_contender(Path('/proc'),args.contender_uid,args.contender_marker)
                    if external:
                        report['contender_started_unix']=time.time();report['contender_identity']=external
                if external:
                    try:contender_cpu.append((time.time(),cpu_ticks(external['pid'])/ticks))
                    except (OSError,IndexError,ValueError):pass
            elif args.contender_at is not None and contender is None and time.time()>=submitted+args.contender_at:
                report['contender_started_unix']=time.time()
                # Pinned to one CPU of the scope by default (the per-core case); unpinned,
                # it may run on any CPU of the scope.
                pin=[] if args.contender_unpinned else ['taskset','-c',str(cpus[-1])]
                contender=OwnedProcess([*pin,str(python),'-c',DUTY_LOOP,str(args.seconds),'100'],manager,output/'contender.log',env)
                report['contender_identity']=contender.identity
            if contender is not None and contender.poll() is None:
                contender_cpu.append((time.time(),cpu_ticks(contender.child.pid)/ticks))
            for name,child in services.items():
                if child.poll() is not None:raise RuntimeError(f'{name} exited during the ramp')
            time.sleep(.5)
        rows=decisions(output/'agent-state',submitted-1)
        series=device_series(rows,'gpu_compute',args.gpu_uuid) if gpu else cpu_series(rows)
        target=ramp_target(series,submitted)
        assignments={task['assignment_id'] for task in client.status(run_id).get('tasks',[]) if task.get('assignment_id')}
        report.update(target_millicores=target,series=series,
            ramp_seconds=ramp_seconds(series,submitted),
            foreign_actions=foreign_actions(rows,assignments))
        if contender is not None or external:
            # The contender wants one core, within its fair share, so it should get it.
            report['contender_cpu']=contender_cpu
            report['contender_served_seconds']=served_seconds(contender_cpu,report['contender_started_unix'])
        report['status']='completed'
    except BaseException as error:
        report['status']='failed';report['error']=repr(error);raise
    finally:
        report['owned_cleanup']={name:child.stop() for name,child in [('contender',contender),*services.items()] if child}
        atomic_json(output/'report.json',report)
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',required=True);parser.add_argument('--output',required=True)
    parser.add_argument('--cpus',type=int,default=4);parser.add_argument('--seconds',type=int,default=40)
    parser.add_argument('--settle',type=float,default=10.)
    parser.add_argument('--contender-at',type=float,help='seconds after submission to start an unmanaged contender')
    parser.add_argument('--contender-unpinned',action='store_true',help='let the contender run on any CPU of the scope instead of the last one')
    parser.add_argument('--contender-uid',type=int,help="measure another account's contender, found by --contender-marker, without launching or acting on it")
    parser.add_argument('--contender-marker',help='an argument the other account\'s contender carries')
    parser.add_argument('--submit-at',type=float,help='Unix time at which to submit, so another account can start its contender on the same clock')
    parser.add_argument('--device',choices=['cpu','gpu'],default='cpu')
    parser.add_argument('--gpu-uuid',help='the device for --device gpu')
    parser.add_argument('--gpu-tasks',type=int,default=1)
    parser.add_argument('--execute',action='store_true')
    report=run(parser.parse_args())
    print(json.dumps({key:report.get(key) for key in ['status','target_millicores','ramp_seconds','contender_served_seconds','foreign_actions']}))


if __name__=='__main__':main()
