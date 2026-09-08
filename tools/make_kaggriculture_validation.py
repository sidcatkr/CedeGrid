#!/usr/bin/env python3
"""Generate bounded manifests from installed, verified application paths; execute nothing."""
import argparse
import json
import os
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from cedegrid import command_task, sha256_file
from validation_runtime import atomic_json, inside_home, home_executable

IMPLEMENTATION_FILES = ('training/self_play.py', 'training/train.py',
                        'integration/cedegrid/workflow.py', 'integration/cedegrid/worker.py')


def generate(root, run_id, python, candidate, gpu_uuid, client_config, node_id='anchor', games=12, device='cuda:0', seed_id=None):
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,95}', run_id):
        raise ValueError('run ID must be an explicit safe identifier')
    seed_id = run_id if seed_id is None else seed_id
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,95}', seed_id):
        raise ValueError('seed ID must be an explicit safe identifier')
    if device not in ('cpu', 'cuda:0') or not 1 <= games <= 32:
        raise ValueError('device must be cpu or cuda:0, with 1..32 games')
    if device == 'cuda:0' and (not gpu_uuid or not gpu_uuid.startswith('GPU-') or ',' in gpu_uuid):
        raise ValueError('one approved GPU UUID is required for CUDA')
    if device == 'cpu' and gpu_uuid:
        raise ValueError('CPU validation must not request a GPU UUID')
    root = inside_home(root)
    application = inside_home(root / 'source/Kaggriculture')
    manager = inside_home(root / 'source/ResourceManager')
    python=home_executable(python)
    candidate, client_config = map(inside_home, (candidate, client_config))
    for path in (python, candidate, client_config, application / 'training/self_play.py', manager / 'python/cedegrid/__init__.py'):
        if not path.is_file():
            raise ValueError(f'required installed path missing: {path}')
    snapshot = candidate.parent / 'snapshot_manifest.json'
    model = candidate.parent / 'weights/policy_value.ts'
    from importlib.util import spec_from_file_location, module_from_spec
    worker_spec = spec_from_file_location('validation_adapter_worker', application / 'integration/cedegrid/worker.py')
    worker = module_from_spec(worker_spec)
    worker_spec.loader.exec_module(worker)
    worker.verify_snapshot(candidate, sha256_file(snapshot), sha256_file(model))
    league = application / 'config/league_bases_r004.json'
    opponents = []
    for record in json.loads(league.read_text())['opponents']:
        path = (league.parent / record['path']).resolve()
        if not path.is_relative_to(application) or sha256_file(path) != record['sha256']:
            raise ValueError('opponent hash or source boundary mismatch')
        opponents.append({**record, 'path': str(path)})
    output = root / 'runs' / run_id
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {'PYTHONPATH': os.pathsep.join([str(manager / 'python'), str(application)]),
           'PYTHONDONTWRITEBYTECODE': '1', 'OMP_NUM_THREADS': '1', 'MKL_NUM_THREADS': '1', 'OPENBLAS_NUM_THREADS':'1',
           'CUDA_VISIBLE_DEVICES': gpu_uuid if device == 'cuda:0' else '', 'TMPDIR': str(output / 'tmp'), 'XDG_CACHE_HOME': str(output / 'cache')}
    for name in ('tmp', 'cache'):
        (output / name).mkdir(mode=0o700)
    def command(path):
        return [str(python), str(application / 'training/self_play.py'), '--candidate', str(candidate),
                '--league', str(league), '--output', str(path), '--games', str(games), '--workers', '1',
                '--run-seed', seed_id, '--inference-device', device, '--act-timeout', '5.0', '--exploration']
    task = command_task(run_id + '.command', command(output / 'command-managed'), str(application), env=env,
                        gpu_vram_mib={gpu_uuid: 768} if device == 'cuda:0' else {}, ram_mib=2048, replay_safe=False,
                        single_process=True, no_escape=True)
    pool = {'pool_id': run_id + '.command', 'node_ids': [node_id], 'class': 'guaranteed', 'min_workers': 1, 'max_workers': 1}
    atomic_json(output / 'command-pool.json', pool)
    atomic_json(output / 'command-job.json', {'job_id': run_id + '.command', 'pool_id': pool['pool_id'], 'priority': 0, 'tasks': [task]})
    atomic_json(output / 'unmanaged-command.json', {'argv': command(output / 'command-unmanaged'), 'cwd': str(application), 'env': env,
        'single_process': True, 'wall_time_seconds': 900, 'expected_backend': 'torchscript', 'expected_device': device})
    profile = {'node_id': node_id, 'class': 'guaranteed', 'max_workers': 2, 'python': str(python),
        'source_root': str(application), 'sdk_root': str(manager / 'python'), 'candidate': str(candidate),
        'snapshot_sha256': sha256_file(snapshot), 'model_sha256': sha256_file(model), 'gpu_uuid': gpu_uuid,
        'device': device, 'actor_vram_mib': 768 if device == 'cuda:0' else 0, 'opponents': opponents}
    cooperative = {'experiment_id': run_id + '.cooperative', 'generation': 0, 'run_seed': seed_id,
        'games': games, 'output': str(output / 'cooperative'), 'client_config': str(client_config),
        'validation_dataset': str(root / 'bootstrap-dataset'), 'wall_time_seconds': 1800,
        'learning_wall_time_seconds': 1800, 'opportunistic_task_timeout_seconds': 60,
        'poll_seconds': 2, 'max_output_bytes': 2 * 1024**3, 'nodes': [profile]}
    atomic_json(output / 'cooperative.json', cooperative)
    manifest = {'schema_version': 1, 'run_id': run_id, 'seed_id': seed_id, 'output': str(output), 'node_id': node_id,
        'gpu_uuid': gpu_uuid, 'device': device, 'model_sha256': profile['model_sha256'], 'snapshot_sha256': profile['snapshot_sha256'],
        'league_sha256': sha256_file(league), 'games': games, 'workers': 1,
        'implementation_files': {name: sha256_file(application / name) for name in IMPLEMENTATION_FILES},
        'executed': False, 'commands': {'pool': ['cedegrid', 'pool', '--deployment', str(client_config), '--spec', str(output / 'command-pool.json')],
            'submit': ['cedegrid', 'submit', '--deployment', str(client_config), '--job', str(output / 'command-job.json')],
            'cooperative': [str(python), '-m', 'integration.cedegrid', 'run', '--config', str(output / 'cooperative.json')]}}
    atomic_json(output / 'manifest.json', manifest)
    return manifest


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', default=str(Path.home() / '.local/share/cedegrid-validation'))
    for name in ('run-id', 'python', 'candidate', 'client-config'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--gpu-uuid')
    parser.add_argument('--device', choices=['cpu', 'cuda:0'], default='cuda:0')
    parser.add_argument('--node-id', default='anchor')
    parser.add_argument('--games', type=int, default=12)
    args = parser.parse_args()
    print(json.dumps(generate(args.root, args.run_id, args.python, args.candidate, args.gpu_uuid, args.client_config, args.node_id, args.games, args.device), indent=2))
