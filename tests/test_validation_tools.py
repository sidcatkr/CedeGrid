"""Home-only harness fault tests; no remote access, Linux load, or GPU execution."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import unittest
from unittest.mock import patch

TOOLS=Path(__file__).parents[1]/'tools'
sys.path.insert(0,str(TOOLS))
import pressure
import pressure_schedule
import soak
from validation_runtime import OwnedProcess, inside_home, local_guard


class LedgerTests(unittest.TestCase):
    def test_absent_uncertain_allocation_stays_reserved(self):
        ledger=soak.AllocationLedger(['burst'])
        ledger.observe({'allocations':[{'assignment_id':'a','node_id':'burst','phase':'uncertain'}]})
        ledger.observe({'allocations':[]})
        self.assertEqual(len(ledger.unresolved()),1)
        self.assertFalse(ledger.unresolved()[0]['present_in_latest_status'])
        ledger.observe({'allocations':[{'assignment_id':'a','node_id':'burst','phase':'released'}]})
        self.assertEqual(ledger.unresolved(),[])
        with self.assertRaisesRegex(RuntimeError,'regressed'):
            ledger.observe({'allocations':[{'assignment_id':'a','node_id':'burst','phase':'running'}]})

    def test_missing_or_future_anchor_telemetry_is_not_fresh(self):
        self.assertFalse(soak.anchor_fresh({},'anchor',10,1))
        self.assertFalse(soak.anchor_fresh({'nodes':[{'report':{'node_id':'anchor','observed_at_unix_ms':10001},'received_ms':10000}]},'anchor',10,1))


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.root=Path(self.temp.name)
        self.root.relative_to(Path.home())
    def tearDown(self): self.temp.cleanup()
    def config(self):
        return {'execution_approved':True,'two_node_window_approved':True,'isolated_validation_deployment':True,
                'duration_seconds':.005,'node_ids':['anchor','burst'],'anchor_node_id':'anchor',
                'experiment_id':'experiment','task_prefix':'experiment.','job_prefix':'experiment.',
                'output':str(self.root/'out'),'connection':str(self.root/'connection.json'),
                'sample_seconds':.001,'release_wait_seconds':.003}

    def test_short_completed_timer_is_never_a_completed_soak(self):
        class Client:
            def status(self): return {'allocations':[],'tasks':[],'jobs':[]}
            def drain_node(self,*_): pass
        report=soak.run(self.config(),Client())
        self.assertEqual(report['status'],'elapsed_complete_review_required')
        self.assertFalse(report['actual_24_hour_soak'])
        self.assertEqual(report['operational_acceptance'],'pending_review')
        self.assertTrue(report['cleanup_confirmed'])
        self.assertGreaterEqual(report['measurement_elapsed_seconds'],.005)

    def test_interrupt_cleans_up_but_does_not_free_unknown_reservations(self):
        class Client:
            count=0
            def status(self):
                self.count+=1
                if self.count==1: return {'allocations':[{'assignment_id':'a','node_id':'burst','phase':'uncertain'}]}
                if self.count==2: raise KeyboardInterrupt()
                return {'allocations':[]}
            def drain_node(self,*_): pass
        config=self.config();config['duration_seconds']=.03
        report=soak.run(config,Client())
        self.assertEqual(report['status'],'aborted')
        self.assertFalse(report['cleanup_confirmed'])
        self.assertEqual(report['unresolved_allocations'][0]['assignment_id'],'a')

    def test_window_and_isolation_must_both_be_authorized(self):
        for key in ['execution_approved','two_node_window_approved','isolated_validation_deployment']:
            config=self.config();config[key]=False
            with self.assertRaises(ValueError): soak.validate(config)

    def test_owned_child_reaping_and_repeat_cleanup(self):
        child=OwnedProcess([sys.executable,'-c','import time; time.sleep(10)'],self.root,self.root/'child.log')
        result=child.stop(grace=.2)
        self.assertTrue(result['reaped'])
        self.assertEqual(result,child.stop())
        self.assertIn(result['handle_mode'],['pidfd','exclusive_unreaped_direct_child_fallback'])

    def test_pidfd_failure_uses_explicit_direct_child_fallback(self):
        with patch('validation_runtime.os.pidfd_open',side_effect=OSError(38,'unavailable'),create=True), patch('validation_runtime.signal.pidfd_send_signal',create=True):
            child=OwnedProcess([sys.executable,'-c','pass'],self.root,self.root/'fallback.log')
        result=child.stop(grace=.2)
        self.assertTrue(result['reaped'])
        self.assertEqual(result['handle_mode'],'exclusive_unreaped_direct_child_fallback')
        self.assertIn('38',result['handle_error'])

    def test_missing_ram_is_unknown_when_required(self):
        with patch('validation_runtime.Path.read_text',side_effect=PermissionError()):
            reading=local_guard(self.root,{'min_ram_free_bytes':1})
        self.assertIsNone(reading['ram_available_bytes'])
        self.assertIn('ram_headroom_unavailable_or_below_bound',reading['errors'])

    def test_outside_home_path_rejected(self):
        with self.assertRaises(ValueError): inside_home('/tmp/unapproved')

    def test_pressure_caps_and_linux_gate(self):
        config={'execution_approved':True,'duration_seconds':1,'output':str(self.root/'p'),
                'cpu_ids':[0],'events':[{'id':'cpu','mode':'cpu','at_seconds':0,'duration_seconds':1}]}
        with patch.object(pressure.sys,'platform','darwin'):
            with self.assertRaisesRegex(RuntimeError,'unverified'): pressure.run(config)
        config['shared_server']=True
        with self.assertRaisesRegex(ValueError,'window'): pressure.validate(config)
        config['shared_window_approved']=True
        config['events'][0]['duration_seconds']=61;config['duration_seconds']=62
        with self.assertRaises(ValueError): pressure.validate(config)

    def test_gpu_context_must_end_between_events(self):
        config={'execution_approved':True,'duration_seconds':80,'output':str(self.root/'p'),
                'gpu_uuid':'GPU-explicit','events':[{'id':'a','mode':'gpu','at_seconds':0,'duration_seconds':20},
                {'id':'b','mode':'gpu','at_seconds':40,'duration_seconds':20}]}
        with self.assertRaisesRegex(ValueError,'one finite event'): pressure.validate(config)
        config.update(python=sys.executable)
        self.assertEqual(pressure_schedule.validate(config),80)

    def test_nvml_abi_and_unknown_are_explicit(self):
        self.assertEqual(pressure.C.sizeof(pressure.ProcessInfo),24)
        self.assertEqual(pressure.ProcessInfo.usedGpuMemory.offset,8)
        with self.assertRaisesRegex(RuntimeError,'unknown is not zero'): pressure.Nvml.check(3)

class ProxyContractTests(unittest.TestCase):
    def test_proxy_rejects_public_or_privileged_endpoints(self):
        import connection_proxy
        base={'execution_approved':True,'listen':['127.0.0.1',19001],'target':['127.0.0.1',19002],
              'duration_seconds':10,'output':str(Path.home()/'proxy-unused')}
        connection_proxy.validate(base)
        for key,value in [('listen',['0.0.0.0',19001]),('target',['192.0.2.1',19002]),('listen',['127.0.0.1',22])]:
            with self.assertRaises(ValueError): connection_proxy.validate({**base,key:value})

class BacklogAndEnvelopeTests(unittest.TestCase):
    setUp=HarnessTests.setUp
    tearDown=HarnessTests.tearDown
    config=HarnessTests.config
    def test_cancelled_job_membership_does_not_hide_uncertain_capacity(self):
        status={'tasks':[{'task_id':'experiment.pending','status':'assigned'}],
            'jobs':[{'job_id':'distinct-job','cancelled':True,'task_ids':['experiment.pending']}],
            'allocations':[{'assignment_id':'a','node_id':'burst','phase':'uncertain'}]}
        self.assertEqual(soak.active_backlog(status,'experiment.'),0)
        ledger=soak.AllocationLedger(['burst']);ledger.observe(status)
        self.assertEqual(len(ledger.unresolved()),1)
        status['jobs'][0].pop('task_ids')
        self.assertEqual(soak.active_backlog(status,'experiment.'),1)

    def test_backlog_target_cannot_be_relaxed(self):
        config=self.config();config['max_backlog_progress_gap_seconds']=121
        with self.assertRaisesRegex(ValueError,'120'):soak.validate(config)

    def test_rate_limit_idle_allowance_does_not_relax_backlog_progress(self):
        clock=[0.]
        class Client:
            def status(self):return {'tasks':[{'task_id':'experiment.pending','status':'assigned'}],'allocations':[],'jobs':[]}
            def drain_node(self,*_):pass
        config=self.config();config.update(duration_seconds=200,sample_seconds=10,startup_grace_seconds=999,
            max_progress_gap_seconds=7200,max_backlog_progress_gap_seconds=120)
        with patch.object(soak.time,'monotonic',side_effect=lambda:clock[0]),patch.object(soak.time,'sleep',side_effect=lambda seconds:clock.__setitem__(0,clock[0]+seconds)):
            report=soak.run(config,Client())
        self.assertEqual(report['status'],'aborted')
        self.assertIn('backlogged',report['error'])
        self.assertEqual(report['measurement_elapsed_seconds'],130)

    def test_generator_preserves_approved_ram_headroom_and_never_approves(self):
        import make_soak_config
        application={'experiment_id':'validation','output':str(self.root/'app'),'client_config':str(self.root/'client.json'),
            'run_seed':'seed','games':12,'nodes':[
                {'node_id':'anchor','class':'guaranteed','source_root':str(self.root),'sdk_root':str(self.root/'sdk'),
                 'python':sys.executable,'device':'cuda:0'},
                {'node_id':'burst','class':'opportunistic','source_root':'/home/test/app','sdk_root':'/home/test/ResourceManager/python',
                 'python':'/home/test/venv/bin/python','gpu_uuid':'GPU-actual'}]}
        source=self.root/'application.json';source.write_text(json.dumps(application))
        coordinator=self.root/'coordinator.json';coordinator.write_text(json.dumps({'retry_limit':3}))
        output=self.root/'generated'
        manifest=make_soak_config.generate(source,coordinator,self.root/'readonly.pt',output,
            burst_output='/home/test/validation/run',burst_cpu_ids=[2,4],proxy_listen_port=19001,proxy_target_port=19002)
        harness=json.loads((output/'harness.json').read_text());pressure_config=json.loads((output/'burst-pressure.json').read_text())
        self.assertEqual(harness['limits']['min_ram_free_bytes'],16*1024**3)
        self.assertEqual(pressure_config['limits']['min_ram_free_bytes'],64*1024**3)
        self.assertFalse(harness['execution_approved']);self.assertFalse(pressure_config['shared_window_approved'])
        self.assertEqual(manifest['worst_case_starts_reserved_per_logical_game'],8)
        self.assertFalse(manifest['actual_24_hour_soak'])

class JournalTests(unittest.TestCase):
    def test_status_deltas_preserve_release_disappearance_and_legacy_reader(self):
        with tempfile.TemporaryDirectory() as temporary:
            journal=soak.StatusJournal(300)
            frames=[]
            for phase in ['running','released',None]:
                allocations=[] if phase is None else [{'assignment_id':'a','node_id':'node','phase':phase}]
                status={'kind':'status','tasks':[],'jobs':[],'pools':[],
                    'nodes':[{'report':{'node_id':'node','allocations':allocations},'received_ms':123}],
                    'allocations':allocations}
                frames.append(journal.encode(status,len(frames)))
            self.assertEqual([frame['encoding'] for frame in frames],['snapshot','delta','delta'])
            path=Path(temporary)/'records.jsonl';path.write_text(''.join(json.dumps(frame)+'\n' for frame in frames))
            decoded=list(soak.read_observations(path))
            self.assertEqual(decoded[0]['status']['allocations'][0]['phase'],'running')
            self.assertEqual(decoded[1]['status']['nodes'][0]['report']['allocations'][0]['phase'],'released')
            self.assertEqual(decoded[2]['status']['allocations'],[])
            self.assertEqual(journal.encode(status,4)['payload'],{})
            self.assertEqual(journal.encode(status,301)['encoding'],'snapshot')

class InterpreterPathTests(unittest.TestCase):
    def test_venv_symlink_invocation_is_preserved(self):
        from validation_runtime import home_executable
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);executable=root/'venv/bin/python';executable.parent.mkdir(parents=True)
            executable.symlink_to(sys.executable)
            self.assertEqual(home_executable(executable),executable)
            self.assertNotEqual(home_executable(executable),executable.resolve())

class RetainedArtifactReviewTests(unittest.TestCase):
    def test_string_paths_verify_checksum_and_refuse_escapes_or_corruption(self):
        from review_local_smoke import checked_file
        from resmgr import sha256_file
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);artifact=root/'replay.json.gz';artifact.write_bytes(b'retained bytes')
            digest=sha256_file(artifact)
            self.assertEqual(checked_file(root,str(artifact),digest,14),artifact)
            artifact.write_bytes(b'changed bytes')
            with self.assertRaisesRegex(ValueError,'checksum'):checked_file(root,str(artifact),digest)
            with self.assertRaises(ValueError):checked_file(root,str(root.parent/'unrelated'),digest)
