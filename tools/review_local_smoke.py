#!/usr/bin/env python3
"""Review trusted, retained local-smoke artifacts without launching workloads.

Writes a separate review.json; the original run report and its failures remain
unchanged. Checkpoints must be the operator's own outputs: torch.load uses the
application's pickle-based checkpoint format.
"""
import argparse
import gzip
import json
from pathlib import Path
import sys
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'python'))
from cedegrid import sha256_file
from validation_runtime import atomic_json,inside_home


def checked_file(root,path,digest,size=None):
    value=Path(path).resolve()
    value.relative_to(root)
    if sha256_file(value)!=digest or (size is not None and value.stat().st_size!=size):
        raise ValueError('retained artifact checksum/size mismatch: '+str(value))
    return value


def review(output):
    root=inside_home(output)
    report=json.loads((root/'report.json').read_text())
    status=json.loads((root/'final-status.json').read_text())
    cycle=json.loads((root/'application/cycle/result.json').read_text())
    planned={item['logical_task_id']:item for item in report['planned_cohort']}
    if report['scope']!='local_cpu_development_integration' or report['elapsed_seconds']>report['bounds']['wall_seconds']:
        raise ValueError('run scope or wall-time bound was violated')
    if report['peak_observed_family_rss_bytes']>report['bounds']['rss_bytes']:
        raise ValueError('observed memory envelope was violated')
    if report['unresolved_allocations'] or any(not item.get('reaped') for item in report['cleanup']):
        raise ValueError('cleanup remains unresolved')
    if any(item['phase']!='released' for item in status['allocations']):
        raise ValueError('durable allocation is not released')
    ordinary = report.get('ordinary_command')
    expected_ordinary = report['bounds'].get('ordinary_command_games', 0)
    if expected_ordinary not in (0, 1) or bool(ordinary) != bool(expected_ordinary):
        raise ValueError('ordinary-command evidence differs from execution envelope')
    if ordinary:
        from local_smoke import verify_ordinary_game
        verified = verify_ordinary_game(root/'ordinary-command')
        if verified['games_sha256'] != ordinary['games_sha256'] or not any(
                item['task_id']==ordinary['task_id'] and item['status']=='completed' for item in status['tasks']):
            raise ValueError('ordinary command output or accepted task identity mismatch')
    if len(status['tasks'])!=len(planned)+2+expected_ordinary or any(item['status']!='completed' for item in status['tasks']):
        raise ValueError('managed games and two learner tasks did not all complete')
    checked_file(root,root/'bin/cedegrid',report['binary_sha256'])
    games=[]
    for path in sorted((root/'application/receipts').glob('*.json')):
        receipt=json.loads(path.read_text());submission=receipt['submission']
        metadata=submission['result']['metadata'];identity=planned[receipt['logical_task_id']]
        for key in ['experiment_id','logical_task_id','episode_id','seed','generation']:
            if metadata[key]!=identity[key]:raise ValueError('replay identity differs from planned cohort')
        result=metadata['result']
        if result['backend']!='torchscript' or result['device']!='cpu' or result['inference_count']<=0 or result['inference_errors']:
            raise ValueError('actual CPU inference evidence is absent or failed')
        replay_path=checked_file(root,receipt['replay'],result['replay_sha256'])
        with gzip.open(replay_path,'rt') as stream:replay=json.load(stream)
        if not replay['steps']:raise ValueError('empty real game replay')
        games.append({'task_id':submission['task_id'],'assignment_id':submission['assignment_id'],
            'attempt_generation':submission['generation'],'logical_task_id':identity['logical_task_id'],
            'seed':identity['seed'],'model_sha256':metadata['model_sha256'],
            'replay_sha256':result['replay_sha256'],'steps':len(replay['steps']),
            'inference_count':result['inference_count'],'game_elapsed_seconds':result['elapsed_seconds']})
    if len(games)!=len(planned) or len({game['logical_task_id'] for game in games})!=len(planned):
        raise ValueError('accepted replay cohort is incomplete or duplicated')
    if cycle['accepted_games']!=len(games) or cycle['continuation']!='optimizer_step_cursor_v2':
        raise ValueError('cycle completion evidence mismatch')
    import torch
    checkpoints=[];continuation_contract=None
    for name,expected_step in [('learn-first',2),('learn-resumed',4)]:
        folder=root/'application/cycle'/name
        receipt=json.loads((folder/'receipt.json').read_text())
        descriptor=next(item for item in receipt['result']['artifacts'] if item['name']=='learner.pt')
        path=checked_file(root,folder/'latest.pt',descriptor['sha256'],descriptor['size'])
        checkpoint=torch.load(path,map_location='cpu',weights_only=False)
        if checkpoint['global_step']!=expected_step or checkpoint['checkpoint_schema']!=2 or not checkpoint.get('resume_v2'):
            raise ValueError('actual optimizer checkpoint did not continue to the planned step')
        contract=checkpoint['resume_v2']['contract']
        if contract['experiment_id']!=report['run_id'] or contract['workers']!=0 or contract['environment']['device']!='cpu':
            raise ValueError('checkpoint continuation ownership/environment mismatch')
        if continuation_contract is not None and contract!=continuation_contract:
            raise ValueError('checkpoint continuation contract changed')
        continuation_contract=contract
        checkpoints.append({'stage':name,'sha256':descriptor['sha256'],'global_step':checkpoint['global_step'],
            'checkpoint_schema':checkpoint['checkpoint_schema']})
    checked_file(root,cycle['checkpoint_path'],cycle['checkpoint_sha256'])
    checked_file(root,root/'application/cycle/model.ts',cycle['model_sha256'])
    checked_file(root,root/'application/cycle/dataset/manifest.json',cycle['dataset_sha256'])
    snapshot=Path(cycle['candidate']).parent.resolve();snapshot.relative_to(root)
    manifest=json.loads((snapshot/'snapshot_manifest.json').read_text())
    for item in manifest['files']:checked_file(root,snapshot/item['path'],item['sha256'],item['bytes'])
    return {'schema_version':1,'reviewed_unix':time.time(),'artifact_review':'passed',
        'scope':report['scope'],'original_status':report['status'],'original_error':report.get('error'),
        'original_report_sha256':sha256_file(root/'report.json'),'final_status_sha256':sha256_file(root/'final-status.json'),
        'elapsed_seconds':report['elapsed_seconds'],'peak_observed_family_rss_bytes':report['peak_observed_family_rss_bytes'],
        'accepted_games':games,'managed_checkpoints':checkpoints,'cycle':cycle,
        'allocation_count':len(status['allocations']),'all_allocations_released':True,
        'task_attempt_generations':{item['task_id']:item['generation'] for item in status['tasks']},
        'linux_runtime_verified':report.get('runtime_platform')=='linux',
        'runtime_verification_scope':'Retained CPU application/lifecycle artifacts only; optional cgroup/GPU and physical two-node behavior are separate gates',
        'ordinary_command_verified':bool(ordinary),'cuda_or_mps_used':False,'actual_24_hour_soak':False,
        'performance_acceptance':'not_evaluated; local functional run includes yielded/retried attempts'}


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',required=True)
    args=parser.parse_args();result=review(args.output)
    path=inside_home(args.output)/'review.json'
    if path.exists():raise FileExistsError('review exists; preserve previous evidence')
    atomic_json(path,result)
    print(json.dumps({key:result[key] for key in ['artifact_review','scope','original_status','allocation_count','all_allocations_released']},indent=2))
if __name__=='__main__':main()
