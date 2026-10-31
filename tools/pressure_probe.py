#!/usr/bin/env python3
"""Finite synthetic probe for matched execution and protection comparisons."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import time
from validation_runtime import atomic_json,inside_home,local_guard

# Both probes look for a drain this often, so the check costs them the same.
CHECK_SECONDS=.05


class NoContext:
    """The unmanaged probe's context: the managed probe's drain checks, which never drain."""
    def draining(self):return False
    def complete(self,result):return None


def checker(context,output,clock=time.monotonic):
    """A stop check that asks the context and the STOP file at most every CHECK_SECONDS."""
    due=[0.]
    def check():
        now=clock()
        if now<due[0]:return False
        due[0]=now+CHECK_SECONDS
        return context.draining() or (output/'STOP').exists()
    return check


def duty_pause(now,phase,duty,period):
    """Seconds to idle before the next unit, and the start of the period `now` falls in: at a
    duty below 1 the probe is busy for duty x period at the start of each period and idles for
    the rest. The period starts only when it is reached, so every idle step of an off phase
    still sees that phase."""
    if duty>=1:return 0.,phase
    if now>=phase+period:phase+=period*((now-phase)//period)
    if now<phase+duty*period:return 0.,phase
    return phase+period-now,phase


def run(args):
    if args.mode not in {'cpu','gpu'} or not 0<args.seconds<=60:
        raise ValueError('explicit CPU/GPU probe must finish within60s')
    if not 0<args.duty<=1 or not 0<args.period_ms<=10000:
        raise ValueError('probe duty must be in (0, 1] and its period in (0, 10] s')
    output=inside_home(args.output);output.mkdir(parents=True,exist_ok=False)
    os.sched_setaffinity(0,{args.cpu})
    context=NoContext()
    if os.environ.get('CEDEGRID_CONTEXT'):
        from cedegrid import WorkerContext
        context=WorkerContext.from_env()
    stopped=checker(context,output)
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
    period=args.period_ms/1000;phase=started;busy=0.
    while time.monotonic()<deadline:
        if stopped():
            drained=True;break
        if time.monotonic()>=next_guard:
            guard=local_guard(output,{'min_ram_free_bytes':16*1024**3,'min_disk_free_bytes':16*1024**3})
            rss=next((int(line.split()[1])*1024 for line in Path('/proc/self/status').read_text().splitlines() if line.startswith('VmRSS:')),None)
            if guard['errors'] or rss is None or rss>2*1024**3:raise RuntimeError('probe RAM/disk guard failed')
            if gpu is not None:
                reading=gpu.sample(True)
                if reading['own_memory_bytes']>1024**3 or reading['free_bytes']<4*1024**3:raise RuntimeError('probe GPU budget/reserve guard failed')
            next_guard=time.monotonic()+.5
        idle,phase=duty_pause(time.monotonic(),phase,args.duty,period)
        if idle:
            # Idle in steps no longer than the drain check, so a duty probe drains as fast.
            time.sleep(min(idle,CHECK_SECONDS));continue
        began=time.monotonic()
        if args.mode=='cpu':
            hashlib.pbkdf2_hmac('sha256',b'cedegrid-matched-probe',b'fixed-seed',1500)
            # A bounded half-core producer leaves meaningful CPU room before
            # pressure; this is identical under unmanaged and managed execution.
            time.sleep(time.monotonic()-began)
        else:
            torch.mm(a,b,out=c);torch.cuda.synchronize()
        units+=1;busy+=time.monotonic()-began
    result={'mode':args.mode,'completed_units':units,'elapsed_seconds':time.monotonic()-started,
        'drained':drained,'finished_monotonic':time.monotonic(),'managed':not isinstance(context,NoContext),
        'check_seconds':CHECK_SECONDS,'duty':args.duty,'period_seconds':period,'busy_seconds':busy}
    atomic_json(output/'result.json',result)
    context.complete(result)
    if gpu is not None:gpu.close()
    return result


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode',choices=['cpu','gpu'],required=True)
    parser.add_argument('--seconds',type=float,required=True);parser.add_argument('--cpu',type=int,required=True)
    parser.add_argument('--output',required=True)
    parser.add_argument('--duty',type=float,default=1.,help='busy fraction of each period (default 1)')
    parser.add_argument('--period-ms',type=float,default=500.,help="duty period in ms (default 500, the agent's duty-cycle period)")
    print(json.dumps(run(parser.parse_args())))
