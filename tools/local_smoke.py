#!/usr/bin/env python3
"""Opt-in CPU-only integration; never CUDA or two-node acceptance evidence.

One managed actor, four real games, two optimizer steps per learner chunk, a600s
whole-run deadline and4GiB observed family-RSS abort cap. Uses existing Python and
Rust binaries, actual application entrypoints, private loopback mTLS and home-only
outputs. No software install, remote access, MPS/CUDA workload, or benchmark.
"""
import argparse
import gzip
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
import uuid

_bootstrap=argparse.ArgumentParser(add_help=False)
_bootstrap.add_argument('--manager-root',default=str(Path(__file__).resolve().parents[1]))
_manager_root=Path(_bootstrap.parse_known_args()[0].manager_root).expanduser().resolve()
sys.path.insert(0,str(_manager_root/'tools'))
sys.path.insert(0,str(_manager_root/'python'))
from resmgr import Client,sha256_file,command_task
from make_test_pki import generate as generate_pki
from soak import AllocationLedger
from validation_runtime import OwnedProcess,atomic_json,inside_home,home_executable,local_guard


def stress_cpu_scope():
    """Two permitted physical cores, including their permitted SMT siblings."""
    if sys.platform!='linux':raise RuntimeError('native bounded stress is unverified on this host')
    groups={}
    for cpu in sorted(os.sched_getaffinity(0)):
        directory=Path('/sys/devices/system/cpu')/f'cpu{cpu}'/'topology'
        key=((directory/'physical_package_id').read_text().strip(),(directory/'core_id').read_text().strip())
        groups.setdefault(key,[]).append(cpu)
    chosen=list(groups.values())[:2]
    if len(chosen)!=2 or sum(map(len,chosen))<3:
        raise RuntimeError('two physical cores with sufficient permitted SMT capacity required for one-core reserve and full actor admission')
    return sorted(cpu for group in chosen for cpu in group),[group[0] for group in chosen]


def external_pressure(output,cpus,seconds=45):
    """Independent finite same-UID CPU process; never enters managed registry."""
    import hashlib
    import threading
    from validation_runtime import linux_identity
    output=inside_home(output);output.mkdir(parents=True,exist_ok=False)
    if sys.platform!='linux' or len(cpus)!=2 or len(set(cpus))!=2 or not set(cpus).issubset(os.sched_getaffinity(0)) or not 0<seconds<=60:
        raise ValueError('external pressure requires two permitted CPUs and at most60s')
    stopped=threading.Event();start=time.monotonic();deadline=start+seconds;counts=[0,0];errors=[]
    previous={sig:signal.signal(sig,lambda *_:stopped.set()) for sig in (signal.SIGINT,signal.SIGTERM)}
    def work(index,cpu):
        try:
            os.sched_setaffinity(0,{cpu})
            while not stopped.is_set() and time.monotonic()<deadline:
                hashlib.pbkdf2_hmac('sha256',b'resmgr-external',b'bounded-stress',1500);counts[index]+=1
        except BaseException as error:errors.append(str(error));stopped.set()
    threads=[threading.Thread(target=work,args=(index,cpu)) for index,cpu in enumerate(cpus)]
    report={'identity':linux_identity(os.getpid()),'uid':os.getuid(),'cpu_ids':cpus,'requested_seconds':seconds,
        'started_monotonic':start,'samples':[],'managed_registry_member':False,'status':'running'}
    try:
        for thread in threads:thread.start()
        while time.monotonic()<deadline and not stopped.is_set():
            guard=local_guard(output,{'min_ram_free_bytes':16*1024**3,'min_disk_free_bytes':10*1024**3})
            if guard['errors']:raise RuntimeError('external guard: '+','.join(guard['errors']))
            report['samples'].append({'observed_monotonic':time.monotonic(),'process_cpu_seconds':time.process_time(),'completed_units':sum(counts)})
            atomic_json(output/'progress.json',report);stopped.wait(.25)
        if errors or stopped.is_set():raise RuntimeError('external work interrupted: '+','.join(errors))
        report['status']='completed'
    finally:
        stopped.set()
        for thread in threads:thread.join(timeout=2)
        report.update(elapsed_seconds=time.monotonic()-start,completed_units=sum(counts),threads_reaped=all(not thread.is_alive() for thread in threads))
        atomic_json(output/'result.json',report)
        for sig,handler in previous.items():signal.signal(sig,handler)
    if not report['threads_reaped']:raise RuntimeError('external threads remain uncertain')
    return report


def external_cpu_after_release(report,released):
    values=[sample for sample in report['samples'] if sample['observed_monotonic']>=released+1]
    if len(values)<2 or values[-1]['observed_monotonic']-values[0]['observed_monotonic']<5:return None
    return (values[-1]['process_cpu_seconds']-values[0]['process_cpu_seconds'])/(values[-1]['observed_monotonic']-values[0]['observed_monotonic'])


def outside_registry(state,identity):
    with sqlite3.connect((state/'state.sqlite3').resolve().as_uri()+'?mode=ro',uri=True) as connection:
        for payload, in connection.execute('SELECT record_json FROM executions'):
            managed=json.loads(payload).get('identity') or {}
            if (managed.get('pid'),managed.get('boot_id'),managed.get('start_time'))==(identity['pid'],identity['boot_id'],identity['start_ticks']):
                raise RuntimeError('external process entered managed registry')
    return True


def verify_ordinary_game(output):
    output=Path(output).resolve();games=json.loads((output/'games.json').read_text())
    if len(games)!=1:raise RuntimeError('ordinary command did not produce exactly one real game')
    game=games[0]
    if game.get('backend')!='torchscript' or game.get('device')!='cpu' or game.get('inference_count',0)<=0 or game.get('inference_errors')!=[]:
        raise RuntimeError('ordinary command did not use successful real CPU inference')
    replay=Path(game['replay']).resolve();replay.relative_to(output)
    if sha256_file(replay)!=game['replay_sha256']:raise RuntimeError('ordinary command replay checksum mismatch')
    summary=json.loads((output/'summary.json').read_text())
    if any(summary.get(key)!=0 for key in ['bad_status_games','invalid_action_games','exception_games']):
        raise RuntimeError('ordinary command simulator failed')
    return {'games':games,'games_sha256':sha256_file(output/'games.json'),'summary':summary}


def family_memory(roots):
    """A read-only process-tree snapshot; only live owned roots' descendants count."""
    result=subprocess.run(['/bin/ps','-axo','pid=,ppid=,rss='],capture_output=True,text=True,check=True,timeout=5)
    rows=[]
    for line in result.stdout.splitlines():
        fields=line.split()
        if len(fields)==3:rows.append(tuple(map(int,fields)))
    members=set(roots)
    changed=True
    while changed:
        prior=len(members)
        members.update(pid for pid,parent,_ in rows if parent in members)
        changed=len(members)!=prior
    return [{'pid':pid,'rss_bytes':rss*1024} for pid,_,rss in rows if pid in members]


def runtime_profile(name, stress=False):
    """Explicit container envelope; never silently weaken the native host guard."""
    if name == 'native':
        return {'name':name,'minimum_free_ram':16*1024**3,'family_rss_limit':4*1024**3,'allocation_ram_mib':4096}
    if name != 'docker-cpu' or stress or sys.platform != 'linux':
        raise ValueError('docker-cpu is a separate Linux CPU integration profile, not the native stress profile')
    root=Path('/sys/fs/cgroup')
    memory=int((root/'memory.max').read_text().strip())
    quota,period=(root/'cpu.max').read_text().split()
    if not Path('/.dockerenv').is_file() or os.getuid()==0 or not 3*1024**3<=memory<=4*1024**3:
        raise RuntimeError('non-root Docker container with a verified3..4GiB memory cap required')
    if quota=='max' or int(quota)/int(period)>2 or len(os.sched_getaffinity(0))!=2:
        raise RuntimeError('Docker CPU profile requires exactly two permitted CPUs and at most two CPU quota')
    if (root/'memory.swap.max').read_text().strip()!='0':
        raise RuntimeError('Docker CPU profile requires no additional swap allowance')
    return {'name':name,'minimum_free_ram':1024**3,'family_rss_limit':3*1024**3,
            'allocation_ram_mib':2048,'cgroup_memory_max':memory,'cgroup_cpu_max':[quota,period],
            'cpu_ids':sorted(os.sched_getaffinity(0))}


def run(args):
    if not args.execute:raise ValueError('explicit CPU-development smoke opt-in required')
    application=inside_home(args.application);dataset=inside_home(args.dataset)
    manager=inside_home(getattr(args,'manager_root',_manager_root))
    stress=bool(getattr(args,'stress',False));wall_seconds=900 if stress else 600
    profile=runtime_profile(getattr(args,'runtime_profile','native'),stress)
    cpus,pressure_cpus=stress_cpu_scope() if stress else (None,None)
    if stress:os.sched_setaffinity(0,cpus)
    binary=inside_home(args.binary);python=home_executable(args.python);output=inside_home(args.output)
    if not all(path.is_file() for path in [binary,python,dataset/'manifest.json',application/'training/self_play.py']):
        raise ValueError('existing binary, interpreter, actual application and verified dataset required')
    output.mkdir(mode=0o700,parents=True,exist_ok=False)
    (output/'tmp').mkdir()
    (output/'bin').mkdir()
    frozen=output/'bin/resmgr'
    shutil.copyfile(binary,frozen)
    frozen.chmod(0o700)
    with frozen.open('rb') as file:os.fsync(file.fileno())
    binary=frozen
    run_id='local-cpu-'+uuid.uuid4().hex
    node='local-cpu-anchor'
    env=dict(os.environ,TMPDIR=str(output/'tmp'),XDG_CACHE_HOME=str(output/'cache'),
        PYTHONPATH=str(manager/'python')+os.pathsep+str(application),PYTHONDONTWRITEBYTECODE='1',
        OMP_NUM_THREADS='1',MKL_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1',
        CUDA_VISIBLE_DEVICES='',PYTORCH_ENABLE_MPS_FALLBACK='0')
    report={'schema_version':1,'run_id':run_id,'scope':'local_cpu_development_integration',
        'runtime_platform':sys.platform,'runtime_node':os.uname().nodename if hasattr(os,'uname') else None,
        'started_unix':time.time(),'status':'running','steps':[],'cleanup':[],'rss_samples':[],
        'runtime_profile':profile,
        'bounds':{'actors':1,'games':4,'ordinary_command_games':1 if stress else 0,'learner_steps_per_chunk':2,'wall_seconds':wall_seconds,'rss_bytes':profile['family_rss_limit'],
            'stress':stress,'cpu_ids':cpus,'physical_cores':2 if stress else None,'external_cpu_ids':pressure_cpus,'external_wall_seconds':45 if stress else 0,
            'minimum_external_cpu_after_release':1.0 if stress else None},
        'actual_24_hour_soak':False,'linux_runtime_verified':False,'cuda_or_mps_used':False,
        'binary_sha256':sha256_file(binary),'source_hashes':{name:sha256_file(application/name) for name in
            ['training/self_play.py','training/train.py','integration/resmgr/worker.py','integration/resmgr/workflow.py']},
        'dataset_manifest_sha256':sha256_file(dataset/'manifest.json'),'manager_root':str(manager),
        'harness_sha256':sha256_file(Path(__file__)),'sdk_sha256':sha256_file(manager/'python/resmgr/__init__.py')}
    services={};owned=None;client=None;last_status=None;stop=[];ledger=AllocationLedger([node]);start=time.monotonic()
    pressure_child=None;stress_state={'status':'pending','running_since':{}} if stress else None
    def stress_tick():
        nonlocal pressure_child
        if not stress or last_status is None:return
        now=time.monotonic()
        if stress_state['status']=='pending':
            for task in last_status.get('tasks',[]):
                if not task['task_id'].startswith(run_id) or '.game' not in task['task_id'] or not task.get('assignment_id'):continue
                allocation=ledger.allocations.get(task['assignment_id'])
                if not allocation or allocation['phase']!='running':continue
                began=stress_state['running_since'].setdefault(task['assignment_id'],now)
                if now-began<2:continue
                stress_state.update(status='external_running',task_id=task['task_id'],assignment_id=task['assignment_id'],generation=task['generation'],started_monotonic=now)
                argv=[str(python),str(Path(__file__).resolve()),'--manager-root',str(manager),'--pressure-worker','--execute',
                    '--output',str(output/'external'),'--cpu-ids',*[str(cpu) for cpu in pressure_cpus]]
                pressure_child=OwnedProcess(argv,manager,output/'external.log',env)
                stress_state['external_identity']=pressure_child.identity;stress_state['external_argv']=argv
                break
        assignment=stress_state.get('assignment_id')
        if assignment:
            allocation=ledger.allocations.get(assignment)
            if allocation and allocation['phase']=='released':stress_state.setdefault('released_monotonic',now)
        if pressure_child is not None:
            if now-pressure_child.started>50:raise TimeoutError('external pressure exceeded its45s bound plus cleanup allowance')
            if pressure_child.poll() is not None:
                cleanup=pressure_child.stop();pressure_child=None
                stress_state['external_cleanup']=cleanup
                if cleanup['exit_code']!=0 or cleanup['signals']:raise RuntimeError('external process failed or was signaled')
                result=json.loads((output/'external/result.json').read_text())
                if result['status']!='completed' or not result['threads_reaped']:raise RuntimeError('external finite work did not finish')
                stress_state.update(status='awaiting_same_experiment_rejoin',external_result=result,
                    registry_exclusion_verified=outside_registry(output/'agent-state',result['identity']))
        report['stress']=stress_state
    previous={sig:signal.signal(sig,lambda sig,_:stop.append(sig)) for sig in [signal.SIGINT,signal.SIGTERM]}
    def tick():
        nonlocal last_status
        if stop or (output/'STOP').exists():raise InterruptedError('operator stop requested')
        if time.monotonic()-start>wall_seconds:raise TimeoutError('bounded CPU integration envelope reached')
        if sys.platform=='linux':
            guard=local_guard(output,{'min_ram_free_bytes':profile['minimum_free_ram'],'min_disk_free_bytes':10*1024**3,'max_output_bytes':4*1024**3})
            if profile['name']=='docker-cpu':
                current=int(Path('/sys/fs/cgroup/memory.current').read_text().strip())
                guard['container_memory_current']=current
                if profile['cgroup_memory_max']-current<256*1024**2:
                    guard['errors'].append('container_memory_headroom_below256MiB')
            if guard['errors']:raise RuntimeError('native CPU integration guard: '+', '.join(guard['errors']))
        roots=[child.child.pid for child in services.values() if child.poll() is None]
        if owned is not None and owned.poll() is None:roots.append(owned.child.pid)
        if pressure_child is not None and pressure_child.poll() is None:roots.append(pressure_child.child.pid)
        samples=family_memory(roots)
        reading={'elapsed_seconds':time.monotonic()-start,'processes':samples,'total_rss_bytes':sum(item['rss_bytes'] for item in samples)}
        report['rss_samples'].append(reading)
        if reading['total_rss_bytes']>profile['family_rss_limit']:raise RuntimeError('owned process-family RSS exceeds explicit runtime profile')
        if services and any(child.poll() is not None for child in services.values()):raise RuntimeError('owned manager service exited')
        if client is not None:
            last_status=client.status();ledger.observe(last_status)
            stress_tick()
        atomic_json(output/'report.json',report)
        time.sleep(.5)
    def command(label,argv):
        nonlocal owned
        began=time.monotonic()
        owned=OwnedProcess(argv,application,output/(label+'.log'),env)
        while owned.poll() is None:tick()
        cleanup=owned.stop();owned=None
        report['cleanup'].append({'stage':label,**cleanup})
        report['steps'].append({'stage':label,'argv':argv,'elapsed_seconds':time.monotonic()-began,'exit_code':cleanup['exit_code']})
        if cleanup['exit_code']!=0:raise RuntimeError(label+' failed; inspect retained stage log')
    try:
        checkpoint=output/'bootstrap/latest.pt'
        command('bootstrap',[str(python),'training/train.py','--dataset',str(dataset),'--output',str(checkpoint.parent),
            '--workers','0','--device','cpu','--batch-size','32','--epochs','2','--max-steps','2','--experiment-id',run_id+'.bootstrap'])
        command('export',[str(python),'training/export.py','--checkpoint',str(checkpoint),'--output',str(output/'bootstrap/model.ts'),'--manifest',str(output/'bootstrap/model.json')])
        command('snapshot',[str(python),'training/snapshot_policy.py','--output',str(output/'snapshots'),'--cycle','0',
            '--source-root',str(application),'--torchscript',str(output/'bootstrap/model.ts'),'--model-manifest',str(output/'bootstrap/model.json')])
        candidate=output/'snapshots/cycle-00000/main.py'
        pki=generate_pki(output/'pki',['DNS:localhost','IP:127.0.0.1'],[node])
        def tls(name):return {'ca_cert':str(output/'pki/ca.pem'),'certificate':str(output/('pki/'+name+'.pem')),'private_key':str(output/('pki/'+name+'.key'))}
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1',0));port=reservation.getsockname()[1]
        endpoint=f'https://127.0.0.1:{port}'
        atomic_json(output/'coordinator.json',{'listen':f'127.0.0.1:{port}','state_dir':str(output/'coordinator-state'),
            'tls':tls('server'),'clients':pki['clients'],'lease_ms':10000,'telemetry_ttl_ms':3000,'retry_limit':3,
            'max_artifact_bytes':256*1024**2,'artifact_quota_bytes':2*1024**3})
        atomic_json(output/'operator.json',{'endpoint':endpoint,'tls':tls('operator')})
        agent_config={'coordinator_url':endpoint,'tls':tls('node-0'),
            'capacity':{'cpu_millicores':1000,'ram_mib':profile['allocation_ram_mib'],'gpu_memory_mib':{}},'max_workers':1,
            'max_transfer_bytes_per_second':10*1024**2,'max_spool_bytes':1024**3,'max_runtime_seconds':wall_seconds+20}
        if stress:agent_config['cpu_affinity']=cpus
        atomic_json(output/'agent.json',agent_config)
        atomic_json(output/'node.json',{'schema_version':2,'node_id':node,'node_mode':'guaranteed','state_dir':str(output/'agent-state'),
            'execution':{'enabled':True,'prepare_timeout_ms':10000,'admission_timeout_ms':30000},
            'monitor':{'interval_ms':500},'cpu':{'reserve_physical_cores':0 if profile['name']=='docker-cpu' else 1,'nice':10},
            'ram':{'reserve_mib':16384 if stress else 1024,'reserve_percent':10},'gpu':{'scale_up_cooldown_ms':3000 if stress else 0},
            'lifecycle':{'drain_timeout_ms':3000,'term_grace_ms':2000,'heartbeat_interval_ms':2000,'allocation_lease_ms':10000},
            'cgroup':{'enabled':False}})
        doctor=subprocess.run([str(binary),'--config',str(output/'node.json'),'doctor'],capture_output=True,text=True,check=True,timeout=30,env=env)
        atomic_json(output/'doctor.json',json.loads(doctor.stdout))
        services['coordinator']=OwnedProcess([str(binary),'coordinator','--deployment',str(output/'coordinator.json')],manager,output/'coordinator.log',env)
        client=Client.from_config(output/'operator.json')
        deadline=time.monotonic()+15
        while True:
            try:client.status();break
            except Exception:
                if time.monotonic()>deadline:raise
                time.sleep(.1)
        services['agent']=OwnedProcess([str(binary),'--config',str(output/'node.json'),'agent','--deployment',str(output/'agent.json')],manager,output/'agent.log',env)
        deadline=time.monotonic()+30
        while not (last_status and last_status.get('nodes')):
            if time.monotonic()>deadline:raise TimeoutError('local CPU agent registration missing')
            tick()
        league=application/'config/league_bases_r004.json'
        opponents=[]
        for value in json.loads(league.read_text())['opponents']:
            path=(league.parent/value['path']).resolve()
            if sha256_file(path)!=value['sha256']:raise ValueError('opponent source changed')
            opponents.append({**value,'path':str(path)})
        if stress:
            command_id=run_id+'.command';command_output=output/'ordinary-command'
            command_argv=[str(python),str(application/'training/self_play.py'),'--candidate',str(candidate),
                '--league',str(league),'--output',str(command_output),'--games','1','--workers','1',
                '--run-seed',command_id,'--inference-device','cpu','--act-timeout','5.0','--exploration']
            command_env={key:env[key] for key in ['PYTHONPATH','PYTHONDONTWRITEBYTECODE','OMP_NUM_THREADS','MKL_NUM_THREADS',
                'OPENBLAS_NUM_THREADS','CUDA_VISIBLE_DEVICES','TMPDIR','XDG_CACHE_HOME']}
            task=command_task(command_id,command_argv,str(application),env=command_env,ram_mib=2048,
                single_process=True,no_escape=True,replay_safe=False,max_attempts=1)
            client.put_pool(command_id,[node],max_workers=1,min_workers=1,allocation_class='guaranteed')
            client.submit(command_id,command_id,[task]);began=time.monotonic();deadline=began+120
            while True:
                tick();record=next(value for value in last_status['tasks'] if value['task_id']==command_id)
                allocation=ledger.allocations.get(record.get('assignment_id'))
                if record['status']=='completed' and allocation and allocation['phase']=='released':break
                if record['status']=='needs_reconciliation' or time.monotonic()>deadline:
                    raise RuntimeError('ordinary real command failed or exceeded120s; retain its allocation for reconciliation')
            receipt=client.result(command_id)
            if receipt is None:raise RuntimeError('ordinary real command lacks accepted result receipt')
            report['ordinary_command']={'argv':command_argv,'task_id':command_id,'receipt':receipt,
                'released_allocation':allocation,'elapsed_seconds':time.monotonic()-began,**verify_ordinary_game(command_output)}
        config={'experiment_id':run_id,'generation':0,'run_seed':run_id,'games':4,'output':str(output/'application'),
            'client_config':str(output/'operator.json'),'validation_dataset':str(dataset),'learner_ram_mib':profile['allocation_ram_mib'],
            'wall_time_seconds':wall_seconds-100,'learning_wall_time_seconds':120,'stop_deadline_unix':report['started_unix']+wall_seconds,
            'poll_seconds':.5,'max_output_bytes':1024**3,
            'nodes':[{'node_id':node,'class':'guaranteed','max_workers':1,'python':str(python),
                'source_root':str(application),'sdk_root':str(manager/'python'),'candidate':str(candidate),'max_attempts':4,
                'snapshot_sha256':sha256_file(candidate.parent/'snapshot_manifest.json'),
                'model_sha256':sha256_file(candidate.parent/'weights/policy_value.ts'),'device':'cpu','opponents':opponents}]}
        if stress:config.update(actor_class='opportunistic',opportunistic_task_timeout_seconds=wall_seconds)
        sys.path.insert(0,str(application))
        from integration.resmgr.workflow import logical_identity
        report['planned_cohort']=[logical_identity(run_id,0,index,run_id) for index in range(4)]
        report['split_provenance']='Existing builder hashes actual normalized action families; train membership cannot be guaranteed before execution.'
        atomic_json(output/'application.json',config)
        command('cycle',[str(python),'-m','integration.resmgr','cycle','--config',str(output/'application.json'),
            '--bootstrap',str(checkpoint),'--steps','2','--device','cpu'])
        result=json.loads((output/'application/cycle/result.json').read_text())
        if result['accepted_games']!=4 or result['contributing_nodes']!=[node] or result['continuation']!='optimizer_step_cursor_v2':
            raise RuntimeError('real cycle ownership/continuation evidence mismatch')
        metadata=[]
        for path in (output/'application/receipts').glob('*.json'):
            value=json.loads(path.read_text());submission=value['submission'];record=submission['result']['metadata']
            if record['result'].get('backend')!='torchscript' or record['result'].get('device')!='cpu':
                raise RuntimeError('a game failed to use the actual CPU TorchScript model')
            with gzip.open(value['replay'],'rt') as stream:replay=json.load(stream)
            metadata.append({'task_id':submission['task_id'],'assignment_id':submission['assignment_id'],'model_sha256':record['model_sha256'],
                'seed':record['seed'],'episode_id':record['episode_id'],'replay_sha256':sha256_file(Path(value['replay'])),'steps':len(replay['steps'])})
        report.update(status='passed_local_cpu_integration',cycle=result,games=metadata)
        if stress:
            if pressure_child is not None or stress_state['status']!='awaiting_same_experiment_rejoin':raise RuntimeError('bounded stress injection did not complete')
            task=next(task for task in last_status['tasks'] if task['task_id']==stress_state['task_id'])
            outcome=json.loads((output/'agent-state/attempts'/stress_state['assignment_id']/'execution-outcome.json').read_text())
            if not outcome['yielded'] or outcome['record']['class']!='opportunistic':raise RuntimeError('target real actor did not yield as an opportunistic allocation')
            if task['status']!='completed' or task['generation']<=stress_state['generation']:raise RuntimeError('same task did not return with a fresh accepted attempt')
            released=stress_state.get('released_monotonic')
            acquired=None if released is None else external_cpu_after_release(stress_state['external_result'],released)
            if acquired is None or acquired<1.:raise RuntimeError('external process did not demonstrably receive at least1 CPU after resource release')
            stress_state.update(status='passed_bounded_cpu_stress',rejoined_generation=task['generation'],target_outcome=outcome,
                external_cpu_cores_after_release=acquired,same_experiment_id=run_id)
            report['stress']=stress_state
    except BaseException as error:
        report.update(status='failed',error=type(error).__name__+': '+str(error))
    finally:
        if pressure_child is not None:
            try:report['cleanup'].append({'stage':'external_interrupted',**pressure_child.stop()})
            except Exception as error:report['unresolved_external']=str(error)
        if owned is not None:
            try:report['cleanup'].append({'stage':'interrupted',**owned.stop()})
            except Exception as error:report['unresolved_controller']=str(error)
        if client is not None:
            try:
                last_status=client.status();ledger.observe(last_status)
                for job in last_status.get('jobs',[]):
                    if job['job_id'].startswith(run_id):client.cancel(job['job_id'])
                if any(value.get('report',{}).get('node_id')==node for value in last_status.get('nodes',[])) or ledger.allocations:
                    client.drain_node(node)
                else:
                    report['node_never_registered_no_allocations']=True
                deadline=time.monotonic()+20
                while time.monotonic()<deadline:
                    last_status=client.status();ledger.observe(last_status)
                    if not ledger.unresolved():break
                    time.sleep(.25)
            except Exception as error:report['cleanup_error']=str(error)
        report['unresolved_allocations']=ledger.unresolved()
        for name in ['agent','coordinator']:
            if name in services:
                try:report['cleanup'].append({'service':name,**services[name].stop(grace=15)})
                except Exception as error:report.setdefault('unresolved_services',[]).append({name:str(error)})
        report['elapsed_seconds']=time.monotonic()-start
        report['peak_observed_family_rss_bytes']=max((sample['total_rss_bytes'] for sample in report['rss_samples']),default=None)
        if report['unresolved_allocations'] or report.get('cleanup_error') or report.get('unresolved_services') or report.get('unresolved_controller') or report.get('unresolved_external'):
            report['status']='failed_cleanup_requires_reconciliation'
        atomic_json(output/'report.json',report)
        if last_status is not None:atomic_json(output/'final-status.json',last_status)
        for sig,handler in previous.items():signal.signal(sig,handler)
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['application','dataset','binary','python','output']:parser.add_argument('--'+name)
    parser.add_argument('--manager-root',default=str(_manager_root))
    parser.add_argument('--stress',action='store_true')
    parser.add_argument('--runtime-profile',choices=['native','docker-cpu'],default='native')
    parser.add_argument('--pressure-worker',action='store_true',help=argparse.SUPPRESS)
    parser.add_argument('--cpu-ids',type=int,nargs=2,help=argparse.SUPPRESS)
    parser.add_argument('--execute',action='store_true')
    args=parser.parse_args()
    if not args.execute:print(json.dumps({'mode':'plan_only','configuration':vars(args),'executed':False}));return 0
    if args.pressure_worker:
        if not args.output or args.cpu_ids is None:parser.error('internal finite pressure worker requires output and CPU IDs')
        external_pressure(args.output,args.cpu_ids);return 0
    if not all(getattr(args,name) for name in ['application','dataset','binary','python','output']):
        parser.error('application, dataset, binary, python and fresh output paths are required')
    report=run(args);print(json.dumps({key:report[key] for key in ['run_id','status','elapsed_seconds','peak_observed_family_rss_bytes','unresolved_allocations']},indent=2))
    return 0 if report['status']=='passed_local_cpu_integration' else 1
if __name__=='__main__':sys.exit(main())
