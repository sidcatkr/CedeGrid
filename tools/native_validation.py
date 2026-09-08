#!/usr/bin/env python3
"""Linux-native bounded validation stages using only private home paths.

Run under the approved envelope. This does not authorize shared-server load.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from validation_runtime import OwnedProcess, atomic_json, inside_home


def physical_cpus(limit=6):
    allowed=os.sched_getaffinity(0)
    groups={}
    for cpu in sorted(allowed):
        root=Path('/sys/devices/system/cpu')/('cpu'+str(cpu))/'topology'
        key=((root/'physical_package_id').read_text().strip(),(root/'core_id').read_text().strip())
        groups.setdefault(key,cpu)
    return list(groups.values())[:limit]


def execute(argv,cwd,output,env,seconds,ram_mib=24576,gpu_mib=6144):
    output=inside_home(output);output.mkdir(parents=True,exist_ok=False)
    process=OwnedProcess(argv,cwd,output/'stdout.log',env)
    start=time.monotonic();peak_ram=0;peak_gpu=0;reason=None;samples=0
    atomic_json(output/'manifest.json',{'argv':argv,'cwd':str(cwd),'timeout_seconds':seconds,
        'ram_mib_limit':ram_mib,'gpu_mib_limit':gpu_mib,'cpu_affinity':sorted(os.sched_getaffinity(0)),
        'started_unix':time.time(),'pid':process.child.pid,'scope':'linux_native_owned_stage'})
    try:
        while process.poll() is None:
            elapsed=time.monotonic()-start
            if elapsed>seconds:
                reason='stage_wall_time_exceeded';break
            path=Path('/proc')/str(process.child.pid)/'status'
            if path.exists():
                lines=path.read_text().splitlines()
                rss=next((int(x.split()[1])/1024 for x in lines if x.startswith('VmRSS:')),0)
                peak_ram=max(peak_ram,rss)
                if rss>ram_mib:reason='owned_process_ram_budget_exceeded';break
            if samples%2==0:
                try:
                    q=subprocess.run(['nvidia-smi','--query-compute-apps=pid,used_gpu_memory','--format=csv,noheader,nounits'],capture_output=True,text=True,timeout=2)
                    if q.returncode != 0 and env.get('CUDA_VISIBLE_DEVICES',''):
                        reason='required_gpu_telemetry_unavailable';break
                    memory=0
                    for line in q.stdout.splitlines():
                        parts=[s.strip() for s in line.split(',')]
                        if len(parts)==2 and parts[0]==str(process.child.pid) and parts[1].isdigit():memory+=int(parts[1])
                    peak_gpu=max(peak_gpu,memory)
                    if memory>gpu_mib:reason='owned_process_gpu_budget_exceeded';break
                except (OSError,subprocess.TimeoutExpired):
                    if env.get('CUDA_VISIBLE_DEVICES',''):
                        reason='required_gpu_telemetry_unavailable';break
            samples+=1;time.sleep(.5)
    finally:
        cleanup=process.stop()
    result={'status':'passed' if reason is None and cleanup['exit_code']==0 else 'failed',
            'reason':reason,'elapsed_seconds':time.monotonic()-start,'cleanup':cleanup,
            'peak_direct_process_ram_mib':peak_ram,'peak_direct_process_gpu_mib':peak_gpu,
            'scope_note':'Direct process RAM/GPU samples; tests may own child processes. No claim of whole-host attribution.'}
    atomic_json(output/'report.json',result)
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root',default=str(Path.home()/'.local/share/cedegrid-validation'))
    p.add_argument('--stage',choices=['tests','sdk-tests','build','gpu-release','bootstrap','cpu-integration'],required=True)
    p.add_argument('--run-id',required=True)
    p.add_argument('--gpu-uuid')
    a=p.parse_args()
    if sys.platform!='linux':raise SystemExit('native validation requires Linux')
    root=inside_home(a.root);rm=root/'source/ResourceManager';kg=root/'source/Kaggriculture'
    cpus=physical_cpus();os.sched_setaffinity(0,cpus)
    env=dict(os.environ,TMPDIR=str(root/'tmp'),XDG_CACHE_HOME=str(root/'cache'),
        CARGO_HOME=str(root/'tools/cargo'),RUSTUP_HOME=str(root/'tools/rustup'),CARGO_BUILD_JOBS='2',
        CEDEGRID_TEST_PYTHON=str(root/'venv-isolated/bin/python'),
        PYTHONPATH=str(rm/'python')+os.pathsep+str(kg),PYTHONDONTWRITEBYTECODE='1',
        OMP_NUM_THREADS='1',MKL_NUM_THREADS='1',OPENBLAS_NUM_THREADS='1',
        CUDA_VISIBLE_DEVICES='')
    env['PATH']=str(root/'tools/cargo/bin')+os.pathsep+env['PATH']
    out=root/'evidence'/a.run_id
    if a.stage=='build':
        result=execute([str(root/'tools/cargo/bin/cargo'),'build','--locked','--release'],rm,out,env,1800,ram_mib=4096,gpu_mib=0)
    elif a.stage=='tests':
        result=execute([str(root/'tools/cargo/bin/cargo'),'test','--locked','--all-targets','--no-fail-fast','--','--test-threads=2'],rm,out,env,1800,ram_mib=4096,gpu_mib=0)
    elif a.stage=='sdk-tests':
        result=execute([str(root/'venv-isolated/bin/python'),'-m','pytest',str(rm/'python/tests'),str(rm/'tests'),str(kg/'tests'),'--basetemp',str(root/'tmp'/a.run_id),'--junitxml',str(root/'evidence'/(a.run_id+'.xml'))],rm,out,env,1800,ram_mib=4096,gpu_mib=0)
    elif a.stage=='cpu-integration':
        result=execute([str(root/'venv-isolated/bin/python'),str(rm/'tools/local_smoke.py'),
            '--application',str(kg),'--dataset',str(root/'bootstrap-dataset'),
            '--binary',str(rm/'target/release/cedegrid'),'--python',str(root/'venv-isolated/bin/python'),
            '--output',str(root/'runs'/a.run_id),'--execute'],rm,out,env,650,ram_mib=4096,gpu_mib=0)
    elif a.stage=='gpu-release':
        if not a.gpu_uuid or not a.gpu_uuid.startswith('GPU-'):raise SystemExit('explicit approved GPU UUID required')
        env['CEDEGRID_TEST_GPU_UUID']=a.gpu_uuid
        env['CEDEGRID_GPU_EVIDENCE']=str(root/'evidence'/(a.run_id+'-gpu.json'))
        result=execute([str(root/'tools/cargo/bin/cargo'),'test','--locked','--test','gpu_native','--','--ignored','--test-threads=1'],rm,out,env,120,ram_mib=4096,gpu_mib=1024)
    else:
        if not a.gpu_uuid or not a.gpu_uuid.startswith('GPU-'):raise SystemExit('explicit approved GPU UUID required')
        env['CUDA_VISIBLE_DEVICES']=a.gpu_uuid
        model=root/'runs'/a.run_id/'bootstrap'
        result=execute([str(root/'venv-isolated/bin/python'),'training/train.py','--dataset',str(root/'bootstrap-dataset'),'--output',str(model),'--workers','0','--device','cuda:0','--batch-size','32','--epochs','2','--max-steps','100','--experiment-id',a.run_id],kg,out,env,1800)
    print(json.dumps(result,indent=2));return 0 if result['status']=='passed' else 1

if __name__=='__main__':sys.exit(main())
