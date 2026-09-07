#!/usr/bin/env python3
"""Approved foreground two-node soak; time completion always requires evidence review.

Runs on the anchor with a private validation deployment. SSH is not a job
protocol. Node-local pressure can be supervised separately within its approved
window. This harness never adopts saved PIDs or removes uncertain reservations.
"""
import argparse
import json
import math
import os
from pathlib import Path
import re
import signal
import sys
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]/'python'))
from resmgr import Client
from validation_runtime import OwnedProcess, atomic_json, inside_home, local_guard


class AllocationLedger:
    """Disappearance is unknown; only an explicit released phase frees capacity."""
    def __init__(self, nodes):
        self.nodes, self.allocations = set(nodes), {}

    def observe(self, status):
        # Reservation rows do not carry attempt generations. Join only explicit
        # assignment identities from the authenticated task/node reports, and
        # retain that binding after the current task advances to another attempt.
        generations = {}
        reports = [*status.get('tasks', []),
                   *(row for node in status.get('nodes', [])
                     for row in node.get('report', {}).get('allocations', []))]
        for report in reports:
            assignment, generation = report.get('assignment_id'), report.get('generation')
            if assignment is None or generation is None:
                continue
            if assignment in generations and generations[assignment] != generation:
                raise RuntimeError('conflicting assignment generations; retain evidence for reconciliation')
            generations[assignment] = generation
        seen = set()
        for record in status.get('allocations', []):
            if record['node_id'] not in self.nodes:
                continue
            identity = record['assignment_id']
            seen.add(identity)
            old = self.allocations.get(identity)
            if old and old['phase']=='released' and record['phase']!='released':
                raise RuntimeError('released reservation regressed; preserve all state for review')
            generation = generations.get(identity, (old or {}).get('generation'))
            if old and old.get('generation') is not None and generation != old['generation']:
                raise RuntimeError('assignment generation changed; preserve all state for review')
            self.allocations[identity] = {**record, 'generation':generation, 'present_in_latest_status':True}
        for identity, record in self.allocations.items():
            if identity not in seen:
                record['present_in_latest_status']=False

    def unresolved(self):
        return [dict(record) for record in self.allocations.values() if record['phase']!='released']


class StatusJournal:
    """Lossless keyed deltas avoid rewriting growing histories every sample."""
    def __init__(self, snapshot_seconds=300):
        self.previous=None
        self.last_snapshot=-float('inf')
        self.snapshot_seconds=snapshot_seconds

    @staticmethod
    def normalize(status):
        result={"other":{key:value for key,value in status.items() if key not in {"tasks","jobs","pools","nodes","allocations"}}}
        for table,key in [("tasks","task_id"),("jobs","job_id"),("pools","pool_id"),("allocations","assignment_id")]:
            result[table]={record[key]:record for record in status.get(table,[])}
        result['nodes']={};result['node_allocations']={}
        for node in status.get('nodes',[]):
            node_id=node['report']['node_id']
            report={key:value for key,value in node['report'].items() if key!='allocations'}
            result['nodes'][node_id]={**node,'report':report}
            for record in node['report'].get('allocations',[]):
                result['node_allocations'][json.dumps([node_id,record['assignment_id']],separators=(',',':'))]={'node_id':node_id,'allocation':record}
        return result

    @staticmethod
    def restore(value):
        result=dict(value['other'])
        for table in ['tasks','jobs','pools','allocations']:
            result[table]=list(value[table].values())
        result['nodes']=[]
        for node_id,node in value['nodes'].items():
            allocations=[item['allocation'] for item in value['node_allocations'].values() if item['node_id']==node_id]
            result['nodes'].append({**node,'report':{**node['report'],'allocations':allocations}})
        return result

    def encode(self,status,elapsed):
        normalized=self.normalize(status)
        if self.previous is None or elapsed-self.last_snapshot>=self.snapshot_seconds:
            record={'encoding':'snapshot','payload':normalized}
            self.last_snapshot=elapsed
        else:
            changes={}
            for table,records in normalized.items():
                previous=self.previous.get(table,{})
                changed={key:value for key,value in records.items() if key not in previous or previous[key]!=value}
                removed=[key for key in previous if key not in records]
                if changed or removed:changes[table]={'set':changed,'remove':removed}
            record={'encoding':'delta','payload':changes}
        self.previous=normalized
        return record


def read_observations(path):
    """Decode both old full-status frames and current lossless status journals."""
    current=None
    with Path(path).open() as stream:
        for line in stream:
            record=json.loads(line)
            if 'status' in record:
                yield record;continue
            if record['encoding']=='snapshot':current=record['payload']
            elif record['encoding']=='delta':
                if current is None:raise ValueError('status delta appeared before a snapshot')
                for table,changes in record['payload'].items():
                    current[table].update(changes['set'])
                    for key in changes['remove']:current[table].pop(key,None)
            else:raise ValueError('unknown observation encoding')
            yield {**{key:value for key,value in record.items() if key not in {'encoding','payload'}},'status':StatusJournal.restore(current)}


def validate(config):
    seconds=float(config['duration_seconds'])
    if not math.isfinite(seconds) or not 0<seconds<=86400:
        raise ValueError('duration must be finite and in (0,86400]')
    if not config.get('execution_approved') or not config.get('two_node_window_approved'):
        raise ValueError('execution envelope and shared two-node budget/window must be approved')
    if not config.get('isolated_validation_deployment'):
        raise ValueError('node drain requires an isolated validation deployment')
    if len(set(config['node_ids']))!=2 or config['anchor_node_id'] not in config['node_ids']:
        raise ValueError('two distinct nodes and an explicit anchor are required')
    if not config.get('experiment_id') or not config.get('task_prefix') or not config.get('job_prefix'):
        raise ValueError('experiment identity and owned task/job prefixes are required')
    inside_home(config['output']); inside_home(config['connection'])
    if not 0<float(config.get('sample_seconds',2))<=30 or not 0<float(config.get('release_wait_seconds',30))<=120:
        raise ValueError('sample/release waits must be positive and bounded')
    backlog_limit=float(config.get('max_backlog_progress_gap_seconds',120))
    if not math.isfinite(backlog_limit) or not 0<backlog_limit<=120:
        raise ValueError('backlog progress target must stay positive and at most120 seconds')
    commands={}
    for item in config.get('owned_commands',[]):
        name=item['name']
        if name in commands or not re.fullmatch(r'[A-Za-z0-9_.-]+',name):
            raise ValueError('owned command names must be unique safe identifiers')
        commands[name]=item
        inside_home(item['cwd'])
        if not item.get('single_process') or not item.get('no_daemon'):
            raise ValueError('owned commands require explicit single-process/no-daemon contracts')
        if not item.get('argv') or not 0<float(item['max_seconds'])<=seconds+120:
            raise ValueError('each owned command needs argv and a bounded lifetime')
    for event in config.get('events',[]):
        if event['op'] not in {'drain','rejoin','start_owned','stop_owned'}:
            raise ValueError('unsupported event; inject faults only through owned processes/connections')
        if not math.isfinite(event['at_seconds']) or not 0<=event['at_seconds']<seconds:
            raise ValueError('event outside operating window')
        if event['op'] in {'drain','rejoin'} and event['node_id'] not in config['node_ids']:
            raise ValueError('event targets an unapproved node')
        if event['op'] in {'start_owned','stop_owned'} and event['name'] not in commands:
            raise ValueError('event targets a command not declared in the approved envelope')
    return seconds


def anchor_fresh(status, anchor, now, ttl_seconds):
    for node in status.get('nodes',[]):
        if node.get('report',{}).get('node_id')==anchor:
            received=node.get('received_ms')
            observed=node['report'].get('observed_at_unix_ms')
            return (isinstance(received,(int,float)) and isinstance(observed,(int,float)) and
                    0<=now*1000-received<=ttl_seconds*1000 and
                    0<=now*1000-observed<=ttl_seconds*1000)
    return False


def active_backlog(status, task_prefix):
    """Cancellation ends requested work, but never releases its allocation."""
    cancelled=set()
    active=set()
    for job in status.get('jobs',[]):
        target=cancelled if job.get('cancelled') else active
        target.update(job.get('task_ids',[]))
    # Older status readers omit membership. Such tasks stay conservatively
    # backlogged; neither matching job names nor missing records proves release.
    excluded=cancelled-active
    return sum(1 for task in status.get('tasks',[])
        if task['task_id'].startswith(task_prefix) and task['task_id'] not in excluded
        and task['status'] in {'queued','assigned','needs_reconciliation'})


def run(config, client=None):
    duration=validate(config); client=client or Client.from_config(config['connection'])
    output=inside_home(config['output']); output.mkdir(parents=True,exist_ok=False)
    control=output/'STOP'; children={}; histories=[]; cleanup=[]
    commands={item['name']:item for item in config.get('owned_commands',[])}
    events=sorted(config.get('events',[]),key=lambda event:event['at_seconds'])
    ledger=AllocationLedger(config['node_ids']); journal=StatusJournal(config.get('full_snapshot_seconds',300)); interrupted=[]; previous={}
    for sig in [signal.SIGINT,signal.SIGTERM]:
        previous[sig]=signal.signal(sig,lambda sig,_frame:interrupted.append(sig))
    start=time.monotonic(); last_progress=start; last_anchor_progress=start; backlog_since=None; accepted={}; latest=None
    report={'schema_version':2,'status':'running','requested_seconds':duration,'started_unix':time.time(),
            'experiment_id':config['experiment_id'],'events':[],'cleanup':cleanup,'owned_processes':histories,
            'operational_acceptance':'pending_review','actual_24_hour_soak':False,
            'stop_procedure':f'Create {control} or signal this live foreground harness with SIGINT; do not adopt a stored PID.'}
    atomic_json(output/'config.json',config)
    def start_child(name):
        if name in children:
            raise RuntimeError('owned command is already running')
        item=commands[name]; instance=uuid.uuid4().hex
        env=dict(os.environ,TMPDIR=str(output),XDG_CACHE_HOME=str(output/'cache'),**item.get('env',{}))
        child=OwnedProcess(item['argv'],item['cwd'],output/(name+'-'+instance+'.log'),env)
        children[name]=child
        histories.append({'name':name,'instance_id':instance,'started_seconds':time.monotonic()-start,
                          'identity':child.identity,'handle_mode':child.handle_mode,'state':'running'})
    def stop_child(name):
        child=children[name]
        result={'name':name,**child.stop()}
        cleanup.append(result)
        if result.get('reaped'):
            del children[name]
            next(item for item in reversed(histories) if item['name']==name)['state']='reaped'
        return result
    try:
        for name,item in commands.items():
            if item.get('autostart',True): start_child(name)
        with (output/'observations.jsonl').open('x') as observations:
            index=0
            while time.monotonic()-start<duration:
                elapsed=time.monotonic()-start
                if interrupted or control.exists(): raise RuntimeError('operator stop requested')
                while index<len(events) and events[index]['at_seconds']<=elapsed:
                    event=events[index]
                    if event['op']=='start_owned': start_child(event['name'])
                    elif event['op']=='stop_owned': stop_child(event['name'])
                    else: client.drain_node(event['node_id'],event['op']=='drain')
                    report['events'].append({**event,'actual_seconds':time.monotonic()-start,'dispatch_acknowledged':True,
                        'effect_verified':False})
                    index+=1
                for name,child in list(children.items()):
                    code=child.poll()
                    if code is not None:
                        stop_child(name)
                        if code!=0 or commands[name].get('must_remain_running',False):
                            raise RuntimeError(f'owned command {name} exited unexpectedly ({code})')
                    elif time.monotonic()-child.started>commands[name]['max_seconds']:
                        stop_child(name)
                        raise RuntimeError(f'owned command {name} exceeded its lifetime')
                latest=client.status(); ledger.observe(latest)
                guard=local_guard(output,config.get('limits',{}))
                if guard['errors']: raise RuntimeError(','.join(guard['errors']))
                if elapsed>config.get('startup_grace_seconds',30) and not anchor_fresh(latest,config['anchor_node_id'],time.time(),config.get('max_anchor_silence_seconds',30)):
                    raise RuntimeError('anchor telemetry missing, stale, or clock-inconsistent')
                for task in latest.get('tasks',[]):
                    task_id=task['task_id']
                    if task_id.startswith(config['task_prefix']) and task['status']=='completed' and task_id not in accepted:
                        submission=client.result(task_id)
                        metadata=(submission or {}).get('result',{}).get('metadata',{})
                        if metadata.get('experiment_id')!=config['experiment_id']:
                            raise RuntimeError('accepted result experiment identity mismatch')
                        allocation=ledger.allocations.get(submission['assignment_id'])
                        accepted[task_id]={'accepted_seconds':elapsed,'node_id':None if allocation is None else allocation['node_id'],
                            'assignment_id':submission['assignment_id'],'generation':submission['generation'],
                            'receipt_hash':task.get('receipt_hash'),'model_sha256':metadata.get('model_sha256')}
                        last_progress=time.monotonic()
                        if allocation is not None and allocation['node_id']==config['anchor_node_id']:
                            last_anchor_progress=last_progress
                backlog=active_backlog(latest,config['task_prefix'])
                backlog_since=(time.monotonic() if backlog_since is None else backlog_since) if backlog else None
                report['backlog']={'tasks':backlog,'since_seconds':None if backlog_since is None else backlog_since-start,
                    'last_anchor_progress_seconds':last_anchor_progress-start,'target_seconds':config.get('max_backlog_progress_gap_seconds',120)}
                if backlog_since is not None and time.monotonic()-max(backlog_since,last_anchor_progress)>config.get('max_backlog_progress_gap_seconds',120):
                    raise RuntimeError('anchor useful progress exceeded120-second target while work remained backlogged')
                if time.monotonic()-last_progress>config.get('max_progress_gap_seconds',600):
                    raise RuntimeError('accepted useful progress exceeded its declared maximum gap')
                observations.write(json.dumps({'elapsed_seconds':elapsed,'observed_unix':time.time(),**journal.encode(latest,elapsed),'guard':guard})+'\n')
                observations.flush(); os.fsync(observations.fileno())
                report.update(elapsed_seconds=elapsed,accepted_progress=accepted,unresolved_allocations=ledger.unresolved())
                atomic_json(output/'status.json',report)
                time.sleep(min(config.get('sample_seconds',2),max(0,duration-(time.monotonic()-start))))
            report['status']='elapsed_complete_review_required'
            report['measurement_elapsed_seconds']=time.monotonic()-start
            report['actual_24_hour_soak']=duration==86400 and report['measurement_elapsed_seconds']>=86400
    except BaseException as error:
        report['status']='aborted'; report['error']=type(error).__name__+': '+str(error)
    finally:
        report.setdefault('measurement_elapsed_seconds',time.monotonic()-start)
        cleanup_start=time.monotonic()
        # Cancel owned jobs first, so re-enabling a test node later cannot revive
        # a previously queued task. Failure retains explicit reconciliation work.
        try:
            latest=client.status(); ledger.observe(latest)
            for job in latest.get('jobs',[]):
                if job['job_id'].startswith(config['job_prefix']): client.cancel(job['job_id'])
        except Exception as error:
            cleanup.append({'action':'cancel_owned_jobs','error':str(error),'reconciliation_required':True})
        for node in config['node_ids']:
            try: client.drain_node(node,True)
            except Exception as error: cleanup.append({'node':node,'drain_error':str(error),'reconciliation_required':True})
        # Stop workload/controller/connection test children before waiting for
        # release. Coordinator/agent services should be externally supervised.
        for name in list(children):
            try: stop_child(name)
            except Exception as error: cleanup.append({'name':name,'error':str(error),'reconciliation_required':True})
        deadline=time.monotonic()+config.get('release_wait_seconds',30)
        latest_observed=False
        while time.monotonic()<deadline:
            try:
                latest=client.status(); ledger.observe(latest); latest_observed=True
                if not ledger.unresolved(): break
            except Exception as error:
                cleanup.append({'action':'observe_release','error':str(error),'reconciliation_required':True})
            time.sleep(min(config.get('sample_seconds',2),max(0,deadline-time.monotonic())))
        report.update(cleanup_elapsed_seconds=time.monotonic()-cleanup_start,finished_unix=time.time(),
            accepted_progress=accepted,unresolved_allocations=ledger.unresolved(),
            cleanup_confirmed=latest_observed and not ledger.unresolved() and not children and
                not any(item.get('reconciliation_required') for item in cleanup))
        report['contributing_nodes']=sorted({item['node_id'] for item in accepted.values() if item['node_id']})
        report['review_requirements']=['Review actual pressure-node event files and protected slowdown.',
            'Confirm CPU/GPU yield effects and decision-to-release timing from agent records.',
            'Confirm anchor progress during burst removal, reconnect, and current model ownership.',
            'Review fault outcomes, retained uncertain reservations, and cleanup evidence.',
            'Compare registered numeric targets and matched baseline runs; elapsed time is insufficient.']
        atomic_json(output/'status.json',report)
        for sig,handler in previous.items(): signal.signal(sig,handler)
    return report


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config',required=True); parser.add_argument('--execute',action='store_true')
    args=parser.parse_args(argv); config=json.loads(inside_home(args.config).read_text())
    if not args.execute:
        print(json.dumps({'mode':'plan_only','configuration':config,'actual_24_hour_soak':False},indent=2)); return 0
    report=run(config); print(json.dumps(report,indent=2))
    return 1 if report['status']=='aborted' or not report['cleanup_confirmed'] else 0

if __name__=='__main__': sys.exit(main())
