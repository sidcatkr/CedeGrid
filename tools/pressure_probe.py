#!/usr/bin/env python3
"""Finite synthetic probe for matched execution and protection comparisons."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import time
from validation_runtime import atomic_json,inside_home,local_guard


def run(args):
    if args.mode not in {'cpu','gpu'} or not 0<args.seconds<=60:
        raise ValueError('explicit CPU/GPU probe must finish within60s')
    output=inside_home(args.output);output.mkdir(parents=True,exist_ok=False)
    os.sched_setaffinity(0,{args.cpu})
    context=None
    if os.environ.get('RESMGR_CONTEXT'):
        from resmgr import WorkerContext
        context=WorkerContext.from_env()
    gpu=None
    if args.mode=='gpu':
        import torch
        torch.set_num_threads(1);torch.set_num_interop_threads(1)
        if not os.environ.get('CUDA_VISIBLE_DEVICES','').startswith('GPU-') or torch.cuda.device_count()!=1:
            raise RuntimeError('one exact visible CUDA UUID required')
        a=torch.full((1024,1024),.001,device='cuda:0');b=torch.full_like(a,.002);c=torch.empty_like(a)
        torch.cuda.synchronize()
        from pressure import Nvml
        gpu=Nvml(os.environ['CUDA_VISIBLE_DEVICES'])
    atomic_json(output/'ready.json',{'pid':os.getpid(),'monotonic':time.monotonic()})
    deadline=time.monotonic()+args.seconds;started=time.monotonic();units=0;drained=False;next_guard=0
    while time.monotonic()<deadline:
        if (context is not None and context.draining()) or (output/'STOP').exists():
            drained=True;break
        if time.monotonic()>=next_guard:
            guard=local_guard(output,{'min_ram_free_bytes':16*1024**3,'min_disk_free_bytes':16*1024**3})
            rss=next((int(line.split()[1])*1024 for line in Path('/proc/self/status').read_text().splitlines() if line.startswith('VmRSS:')),None)
            if guard['errors'] or rss is None or rss>2*1024**3:raise RuntimeError('probe RAM/disk guard failed')
            if gpu is not None:
                reading=gpu.sample(True)
                if reading['own_memory_bytes']>1024**3 or reading['free_bytes']<4*1024**3:raise RuntimeError('probe GPU budget/reserve guard failed')
            next_guard=time.monotonic()+.5
        began=time.monotonic()
        if args.mode=='cpu':
            hashlib.pbkdf2_hmac('sha256',b'resmgr-matched-probe',b'fixed-seed',1500)
            # A bounded half-core producer leaves meaningful CPU room before
            # pressure; this is identical under unmanaged and managed execution.
            time.sleep(time.monotonic()-began)
        else:
            torch.mm(a,b,out=c);torch.cuda.synchronize()
        units+=1
    result={'mode':args.mode,'completed_units':units,'elapsed_seconds':time.monotonic()-started,
        'drained':drained,'finished_monotonic':time.monotonic(),'managed':context is not None}
    atomic_json(output/'result.json',result)
    if context is not None:context.complete(result)
    if gpu is not None:gpu.close()
    return result


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode',choices=['cpu','gpu'],required=True)
    parser.add_argument('--seconds',type=float,required=True);parser.add_argument('--cpu',type=int,required=True)
    parser.add_argument('--output',required=True)
    print(json.dumps(run(parser.parse_args())))
