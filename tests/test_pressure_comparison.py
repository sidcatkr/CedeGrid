"""Post-yield analysis and ownership guards, without native/GPU workload launch."""
import json
from pathlib import Path
import sqlite3
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
