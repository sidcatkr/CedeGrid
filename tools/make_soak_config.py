#!/usr/bin/env python3
"""Generate an unapproved 24h envelope from actual two-node application config.

Generated files do not authorize execution. The burst home, CPU IDs, GPU UUID,
proxy ports, and paths are supplied explicitly; no machine identity is inferred
from a directory. Review budgets/window before setting execution approval flags.
"""
import argparse
import copy
import json
from pathlib import Path
import sys
from validation_runtime import atomic_json, inside_home


def generate(application_config, coordinator_config, bootstrap, output, *, burst_output,
             burst_cpu_ids, proxy_listen_port, proxy_target_port):
    application=json.loads(inside_home(application_config).read_text())
    coordinator=json.loads(inside_home(coordinator_config).read_text())
    nodes=application['nodes']
    anchors=[node for node in nodes if node['class']=='guaranteed'];bursts=[node for node in nodes if node['class']=='opportunistic']
    if len(nodes)!=2 or len(anchors)!=1 or len(bursts)!=1:
        raise ValueError('actual application config must contain exactly one anchor and one burst node')
    anchor,burst=anchors[0],bursts[0]
    if not burst.get('gpu_uuid','').startswith('GPU-') or not 1<=len(set(burst_cpu_ids))<=2:
        raise ValueError('approved burst GPU UUID and one/two allowed pressure CPU IDs are required')
    if not Path(burst_output).is_absolute() or not str(burst_output).startswith('/home/'):
        raise ValueError('explicit burst home path is required; remote validity remains a deployment check')
    output=inside_home(output);output.mkdir(parents=True,exist_ok=False)
    retry=coordinator.get('retry_limit',3)
    if not 0<=retry<=3: raise ValueError('verified retry_limit exceeds bounded application rate policy')
    factor=2*(retry+1);games=min(application.get('games',12),128//factor)
    app={**copy.deepcopy(application),'output':str(output/'application'),'duration_seconds':86400,
        'execution_approved':False,'two_node_window_approved':False,'games':games,
        'max_game_starts_per_hour':128,'coordinator_retry_limit':retry,'coordinator_retry_limit_verified':True,
        'min_cycle_gap_seconds':5,'max_total_bytes':20*1024**3,'min_disk_free_bytes':16*1024**3}
    app_path=output/'application.json';atomic_json(app_path,app)
    proxy={'execution_approved':False,'listen':['127.0.0.1',proxy_listen_port],'target':['127.0.0.1',proxy_target_port],
        'duration_seconds':86460,'max_connections':16,'output':str(output/'proxy')}
    from connection_proxy import validate as validate_proxy
    validate_proxy({**proxy,'execution_approved':True})
    proxy_path=output/'proxy.json';atomic_json(proxy_path,proxy)
    script=Path(__file__).resolve().parent
    controller=[anchor['python'],'-m','integration.resmgr','soak','--config',str(app_path),'--bootstrap',str(inside_home(bootstrap)),
        '--steps','50','--device',anchor.get('device','cpu')]
    events=[];pressure_events=[]
    for hour in range(24):
        base=hour*3600
        pressure_events.extend([
            {'id':f'h{hour:02d}-cpu','mode':'cpu','at_seconds':base+60,'duration_seconds':30},
            {'id':f'h{hour:02d}-gpu','mode':'gpu','at_seconds':base+180,'duration_seconds':20,'allocate_mib':256}])
        events.extend([
            {'op':'drain','node_id':burst['node_id'],'at_seconds':base+300,'scenario':'burst_removal'},
            {'op':'rejoin','node_id':burst['node_id'],'at_seconds':base+600,'scenario':'burst_rejoin'},
            {'op':'stop_owned','name':'burst-connection','at_seconds':base+900,'scenario':'owned_connection_loss'},
            {'op':'start_owned','name':'burst-connection','at_seconds':base+960,'scenario':'owned_connection_reconnect'}])
    harness={'execution_approved':False,'two_node_window_approved':False,'isolated_validation_deployment':True,
        'duration_seconds':86400,'output':str(output/'harness'),'connection':application['client_config'],
        'node_ids':[node['node_id'] for node in nodes],'anchor_node_id':anchor['node_id'],
        'experiment_id':application['experiment_id'],'task_prefix':application['experiment_id']+'.',
        'job_prefix':application['experiment_id']+'.','sample_seconds':2,'release_wait_seconds':60,
        'startup_grace_seconds':30,'max_anchor_silence_seconds':30,'max_progress_gap_seconds':7200,'max_backlog_progress_gap_seconds':120,
        'limits':{'min_disk_free_bytes':16*1024**3,'min_ram_free_bytes':16*1024**3,'max_output_bytes':2*1024**3},
        'owned_commands':[
            {'name':'burst-connection','argv':[anchor['python'],str(script/'connection_proxy.py'),'--config',str(proxy_path),'--execute'],
             'cwd':str(script),'single_process':True,'no_daemon':True,'max_seconds':86460,'must_remain_running':True},
            {'name':'application','argv':controller,'cwd':anchor['source_root'],'single_process':True,'no_daemon':True,
             'max_seconds':86460,'must_remain_running':False,'env':{'PYTHONPATH':anchor['sdk_root']+':'+anchor['source_root'],
                 'OMP_NUM_THREADS':'1','MKL_NUM_THREADS':'1','OPENBLAS_NUM_THREADS':'1','CUDA_VISIBLE_DEVICES':'','PYTHONDONTWRITEBYTECODE':'1'}}],
        'events':events}
    atomic_json(output/'harness.json',harness)
    pressure={'execution_approved':False,'shared_server':True,'shared_window_approved':False,
        'duration_seconds':86400,'output':str(burst_output),'python':burst['python'],
        'cpu_ids':burst_cpu_ids,'gpu_uuid':burst['gpu_uuid'],'max_rss_bytes':2*1024**3,'max_vram_bytes':1024**3,
        'min_gpu_free_bytes':8*1024**3,'limits':{'min_disk_free_bytes':16*1024**3,'min_ram_free_bytes':64*1024**3,'max_output_bytes':256*1024**2},
        'events':pressure_events}
    atomic_json(output/'burst-pressure.json',pressure)
    manifest={'schema_version':1,'executed':False,'actual_24_hour_soak':False,'logical_games_per_cohort':games,
        'worst_case_starts_reserved_per_logical_game':factor,'actual_attempt_count':'must be measured from coordinator assignment history',
        'required_review':['Approve both resource budgets and full two-node time window before any execution.',
            'Keep agent/pressure CPU usage and capacity in the same approved CPU set; do not compare2CPUpressure with64CPUhostcapacity.',
            'Point only the burst transport/tunnel at the owned loopback proxy; anchor retains direct authenticated coordinator contact.',
            'Start the independent burst schedule at the recorded anchor start; retain both clocks and raw events.',
            'Scheduled pressure timing is a proposed envelope. Calibrate permitted overlap with real active games before24h; missed contention is not a pass.',
            'Keep independent coordinator/agent supervision active through cleanup; rootless supervisors cannot enforce after their own death.',
            'Review actual24h elapsed observations, protected-workload latency, useful progress, faults, and unresolved reservations.'],
        'commands':{'anchor':[anchor['python'],str(script/'soak.py'),'--config',str(output/'harness.json'),'--execute'],
            'burst':[burst['python'],str(Path(burst['sdk_root']).parent/'tools/pressure_schedule.py'),'--config','<reviewed home-local copy of burst-pressure.json>','--execute']}}
    atomic_json(output/'manifest.json',manifest)
    return manifest


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['application-config','coordinator-config','bootstrap','output','burst-output']: parser.add_argument('--'+name,required=True)
    parser.add_argument('--burst-cpu-ids',nargs='+',type=int,required=True)
    parser.add_argument('--proxy-listen-port',type=int,required=True);parser.add_argument('--proxy-target-port',type=int,required=True)
    args=parser.parse_args(); print(json.dumps(generate(**vars(args)),indent=2))
if __name__=='__main__':main()
