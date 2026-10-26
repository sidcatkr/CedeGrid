"""Post-yield analysis and ownership guards, without native/GPU workload launch."""
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
sys.path.insert(0,str(Path(__file__).parents[1]/'tools'))
import pressure_comparison as comparison


class PressureAnalysisTests(unittest.TestCase):
    def test_cpu_probe_matches_its_opportunistic_pool_without_gpu_admission(self):
        task=comparison.probe_task('task',['python','probe.py'],Path.home(),{},'cpu','GPU-unused')
        self.assertEqual(task['class'],'opportunistic')
        self.assertEqual(task['resources']['gpu_memory_mib'],{})
        self.assertEqual(task['resources']['cpu_millicores'],500)
        self.assertEqual(task['max_attempts'],1)

    def test_pre_release_queued_requests_are_not_post_yield_samples(self):
        event={'results':[{'requests':[{'arrival_monotonic':1.,'finished_monotonic':4.,'latency_ms':3000},
            {'arrival_monotonic':3.,'finished_monotonic':3.001,'latency_ms':1}]}]}
        self.assertEqual(comparison.protected_samples(event,2.),[1])

    def test_missing_yield_does_not_pass_even_with_fast_protected_requests(self):
        event={'results':[{'requests':[{'arrival_monotonic':10+i*.02,'latency_ms':1} for i in range(101)]}]}
        cases=[]
        for repetition in range(3):
            for scenario in ['protected_alone','unmanaged_idle','managed_idle','unmanaged_contention','managed_contention']:
                cases.append({'repetition':repetition,'scenario':scenario,'status':'completed','protected':event,
                    'probe':{'completed_units':100,'elapsed_seconds':10},'released_monotonic':9,'yielded':False})
        result=comparison.evaluate(cases)
        self.assertTrue(result['missing'])
        self.assertTrue(all(item['status']=='inconclusive' for item in result['metrics'].values()))
        for case in cases:case.update(yielded=True,decision_delay_seconds=.5,decision_to_release_seconds=2)
        result=comparison.evaluate(cases)
        self.assertEqual(result['missing'],[])
        self.assertTrue(all(item['status']=='passed' for item in result['metrics'].values()))
        for case in cases:
            if case['scenario']=='managed_contention':case['protected']={'results':[{'requests':[{'arrival_monotonic':10+i*.02,'latency_ms':10} for i in range(101)]}]}
        self.assertEqual(comparison.evaluate(cases)['metrics']['post_yield_p99_ratio_max']['status'],'failed')

    def test_external_identity_is_never_an_owned_registry_member(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.relative_to(Path.home())
            connection=sqlite3.connect(root/'state.sqlite3');connection.execute('CREATE TABLE executions(record_json TEXT)')
            connection.execute('INSERT INTO executions VALUES(?)',(json.dumps({'identity':{'pid':3,'boot_id':'boot','start_time':4}}),));connection.commit();connection.close()
            self.assertTrue(comparison.verify_external_registry(root,{'pid':3,'boot_id':'boot','start_ticks':5}))
            with self.assertRaisesRegex(RuntimeError,'protected workload'):comparison.verify_external_registry(root,{'pid':3,'boot_id':'boot','start_ticks':4})

    def test_unavailable_or_stuck_gpu_release_never_passes(self):
        class Gpu:
            def sample(self,**_):raise RuntimeError('unknown')
        with self.assertRaisesRegex(RuntimeError,'unknown'):comparison.confirm_gpu_release(Gpu(),12)


def observation(at, actions=(), resources=None, would_drain=()):
    """One `cedegrid history` row at `at` seconds."""
    return {'id':int(at*10),'snapshot':{'observed_at_unix_ms':int(at*1000)},
        'decision':{'actions':list(actions),'resources':resources or {},'would_drain':list(would_drain)}}


def cap(assignment, target, resource='cpu'):
    return {'allocation_id':assignment,'resource':resource,'step':'cap','target':target,'code':'yield_step_cap'}


class GateRunnerTests(unittest.TestCase):
    def test_each_mode_and_floor_writes_its_node_settings(self):
        from runtime_config import runtime_toml,tomllib
        base={'node_id':'n','ram':{'reserve_percent':5},'contention':{'cpu':{'scale_up_cooldown_ms':0}}}
        neutral=comparison.node_contention(base,'neutral')
        self.assertEqual(neutral['contention']['default_mode'],'neutral')
        self.assertNotIn('acknowledge_caution',neutral['contention'])
        self.assertEqual(neutral['ram'],{'reserve_percent':5})
        performance=comparison.node_contention(base,'performance',ram_floor_mib=512)
        self.assertEqual(performance['contention']['acknowledge_caution'],['performance_mode','small_floor'])
        self.assertEqual(performance['ram'],{'reserve_percent':0,'reserve_mib':512})
        self.assertNotIn('acknowledge_caution',comparison.node_contention(base,'yield_first',ram_floor_mib=4096)['contention'])
        self.assertEqual(base['ram'],{'reserve_percent':5},'the input stays unchanged')
        parsed=tomllib.loads(runtime_toml(performance,'node'))
        self.assertEqual(parsed['contention']['default_mode'],'performance')
        for mode,floor in [('fast',None),('neutral',100)]:
            with self.assertRaises(ValueError):comparison.node_contention(base,mode,ram_floor_mib=floor)

    def test_step_timing_reads_first_step_reclaim_and_reversal_per_resource(self):
        over={'cpu':{'own':[1500,1500],'target_share':1000}}
        held={'cpu':{'own':[1000,1000],'target_share':1000}}
        history=[observation(9.5),observation(10.0,resources=over),observation(10.5,[cap('a',1000),cap('other',1)],over),
            observation(11.0,[cap('a',1000)],held),observation(30.0,[cap('a',1000)],held),
            observation(31.0,[cap('a',1200)],held),observation(31.5,resources=held)]
        timing=comparison.step_timing(list(reversed(history)),{'a'},onset_unix=10.0,ended_unix=30.5)
        self.assertEqual(timing,{'cpu':{'first_step_seconds':.5,'reclaim_seconds':1.0,
            'reversal_seconds':1.0,'steps':{'cap':1}}})

    def test_a_graduated_step_counts_as_a_yield(self):
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            state=Path(temporary)
            connection=sqlite3.connect(state/'state.sqlite3')
            connection.execute('CREATE TABLE observations(id INTEGER PRIMARY KEY, observed_at_unix_ms INTEGER, decision_json TEXT)')
            for row in [observation(9.0,[cap('a',1)]),observation(10.5,[cap('b',1)]),observation(11.0,[cap('a',1000)])]:
                connection.execute('INSERT INTO observations(observed_at_unix_ms,decision_json) VALUES(?,?)',
                    (row['snapshot']['observed_at_unix_ms'],json.dumps(row['decision'])))
            connection.commit();connection.close()
            decision=comparison.policy_decision(state,'a',10.0)
            self.assertEqual((decision['observed_unix'],decision['kind']),(11.0,'step'))

    def test_the_history_audit_finds_actions_outside_own_allocations(self):
        history=[observation(1,[cap('a',1),cap('b',2,'ram')]),observation(2,[cap('stranger',3)],would_drain=['other'])]
        self.assertEqual(comparison.history_audit(history,{'a','b'}),
            {'observations':2,'own_actions':2,'actions_outside_own_allocations':1,'drains_outside_own_allocations':1})

    def test_a_protected_snapshot_reads_nice_cgroup_limits_and_start_time(self):
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            proc,cgroup=self.fake_host(Path(temporary))
            self.assertEqual(comparison.protected_snapshot(4242,proc,cgroup),{'pid':4242,'nice':0,'start_ticks':123456,
                'cgroup':'/user.slice/protected.scope','cpu.max':'max 100000','memory.high':'max','memory.max':None})

    def test_any_change_in_a_protected_snapshot_fails_the_run(self):
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            proc,cgroup=self.fake_host(Path(temporary))
            before=comparison.protected_snapshot(4242,proc,cgroup)
            self.assertEqual(comparison.verify_protected_unchanged(before,comparison.protected_snapshot(4242,proc,cgroup)),[])
            for path,text,field in [('memory.high','8G\n','memory.high'),('cpu.max','50000 100000\n','cpu.max')]:
                (cgroup/'user.slice/protected.scope'/path).write_text(text)
                with self.assertRaisesRegex(RuntimeError,field):
                    comparison.verify_protected_unchanged(before,comparison.protected_snapshot(4242,proc,cgroup))
            proc,cgroup=self.fake_host(Path(temporary)/'again',nice=5)
            with self.assertRaisesRegex(RuntimeError,'nice'):
                comparison.verify_protected_unchanged(before,comparison.protected_snapshot(4242,proc,cgroup))
            proc,cgroup=self.fake_host(Path(temporary)/'moved',scope='other.scope')
            with self.assertRaisesRegex(RuntimeError,'cgroup'):
                comparison.verify_protected_unchanged(before,comparison.protected_snapshot(4242,proc,cgroup))

    def test_usage_names_the_gate_options(self):
        usage=subprocess.run([sys.executable,str(Path(__file__).parents[1]/'tools/pressure_comparison.py'),'--help'],
            capture_output=True,text=True,check=True).stdout
        for option in ['--contention-mode','{yield_first,neutral,performance}','--ram-floor-mib']:
            self.assertIn(option,usage)

    @staticmethod
    def fake_host(root,nice=0,scope='protected.scope'):
        proc=root/'proc';cgroup=root/'cgroup';leaf=cgroup/'user.slice'/scope
        (proc/'4242').mkdir(parents=True);leaf.mkdir(parents=True)
        fields=['S','1','1','1','0','-1','0']+['0']*9+[str(nice),'1','0','123456']+['0']*30
        (proc/'4242/stat').write_text('4242 (python3 (x)) '+' '.join(fields)+'\n')
        (proc/'4242/cgroup').write_text('0::/user.slice/'+scope+'\n')
        (leaf/'cpu.max').write_text('max 100000\n');(leaf/'memory.high').write_text('max\n')
        return proc,cgroup
