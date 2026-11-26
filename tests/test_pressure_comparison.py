"""Post-yield analysis and ownership guards, without native/GPU workload launch."""
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
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
        self.assertEqual(performance['contention']['acknowledge_caution'],['small_floor'])
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

    def test_a_case_waits_for_an_unmanaged_probe_to_exit_after_its_result(self):
        class Child:
            def __init__(self,code):self.code=code
            def poll(self):return self.code
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            probe=Path(temporary)
            running={'probe':Child(None)}
            self.assertFalse(comparison.case_finished('unmanaged_idle',probe,running,False,None))
            (probe/'result.json').write_text('{}')
            # The result is written, but the probe is still tearing down.
            self.assertFalse(comparison.case_finished('unmanaged_idle',probe,running,False,None))
            self.assertTrue(comparison.case_finished('unmanaged_idle',probe,{'probe':Child(0)},False,None))
            self.assertFalse(comparison.case_finished('unmanaged_contention',probe,
                {'probe':Child(0),'protected':Child(None)},False,None))
            self.assertFalse(comparison.case_finished('managed_idle',probe,{},True,None))
            self.assertTrue(comparison.case_finished('managed_idle',probe,{},True,12.5))
            self.assertTrue(comparison.case_finished('protected_alone',probe,{'protected':Child(0)},False,None))

    def test_usage_names_the_gate_options(self):
        usage=subprocess.run([sys.executable,str(Path(__file__).parents[1]/'tools/pressure_comparison.py'),'--help'],
            capture_output=True,text=True,check=True).stdout
        for option in ['--contention-mode','{yield_first,neutral,performance}','--ram-floor-mib',
                '--profile','{protection,own-efficiency}','--readiness-seconds']:
            self.assertIn(option,usage)

    def test_the_readiness_wait_is_an_option_with_the_old_default(self):
        """Yield First waits for a quiet device before it admits, which can pass 60 s."""
        plan=lambda *extra:json.loads(subprocess.run([sys.executable,str(Path(__file__).parents[1]/'tools/pressure_comparison.py'),
            '--root','/unused','--output','/unused','--mode','gpu','--gpu-uuid','GPU-test',*extra],
            capture_output=True,text=True,check=True).stdout)['configuration']['readiness_seconds']
        self.assertEqual(plan(),60)
        self.assertEqual(plan('--readiness-seconds','180'),180)
        refused=subprocess.run([sys.executable,str(Path(__file__).parents[1]/'tools/pressure_comparison.py'),
            '--root','/unused','--output','/unused','--mode','gpu','--gpu-uuid','GPU-test','--readiness-seconds','0'],
            capture_output=True,text=True)
        self.assertNotEqual(refused.returncode,0)

    def test_both_probes_check_for_a_drain_every_50_ms(self):
        import pressure_probe
        class Counting:
            calls=0
            def draining(self):Counting.calls+=1;return False
        clock=[0.]
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            for context in [Counting(),pressure_probe.NoContext()]:
                check=pressure_probe.checker(context,Path(temporary),clock=lambda:clock[0])
                stops=[]
                for at in [0,.01,.049,.05,.07,.12]:
                    clock[0]=at;stops.append(check())
                self.assertEqual(stops,[False]*6)
            self.assertEqual(Counting.calls,3,'checked at 0, .05 and .12 s')
            # The unmanaged probe's context never drains and completes nothing.
            self.assertFalse(pressure_probe.NoContext().draining())
            self.assertIsNone(pressure_probe.NoContext().complete({}))
            (Path(temporary)/'STOP').write_text('')
            clock[0]=1.
            self.assertTrue(pressure_probe.checker(pressure_probe.NoContext(),Path(temporary),clock=lambda:clock[0])())

    def test_the_probe_times_even_against_odd_units_and_records_what_it_did(self):
        import pressure_probe
        # Even units cost 10% more than odd ones until two blocks are held, as when the
        # allocator hands the same block back every other call.
        held=[0];calls=[0];clock=[0.]
        def unit():
            calls[0]+=1
            clock[0]+=1.1e-3 if calls[0]%2==0 and held[0]<2 else 1e-3
        def hold():held[0]+=1
        check=pressure_probe.self_check(unit,clock=lambda:clock[0],hold=hold,units=200)
        self.assertGreater(check['initial_ratio'],1.09)
        self.assertLessEqual(check['ratio'],1+pressure_probe.BIAS_LIMIT)
        self.assertEqual(check['held'],2)
        self.assertFalse(check['biased'])
        # A bias that holding cannot remove is recorded as such, after a bounded number of tries.
        held[0]=-10**6
        stuck=pressure_probe.self_check(unit,clock=lambda:clock[0],hold=hold,units=200)
        self.assertTrue(stuck['biased'])
        self.assertLessEqual(stuck['held'],pressure_probe.BIAS_HOLDS)
        # With no holds allowed it only measures.
        held[0]=0
        measured=pressure_probe.self_check(unit,clock=lambda:clock[0],hold=hold,units=200,holds=0)
        self.assertEqual(measured['held'],0);self.assertTrue(measured['biased'])

    def test_the_same_core_gate_refuses_a_biased_or_unchecked_window(self):
        import pressure_probe
        verdict=pressure_probe.window_verdict
        self.assertEqual(verdict({'completed_units':10}),'refused: no in-window even/odd check recorded')
        # The window's own check decides, whatever the warm-up found.
        self.assertEqual(verdict({'self_check':{'ratio':1.05,'biased':True},'window_check':{'ratio':1.004,'biased':False}}),'measured')
        self.assertTrue(verdict({'self_check':{'ratio':1.0,'biased':False},'window_check':{'ratio':1.05,'biased':True}}).startswith('refused'))
        # A ratio over the limit is refused even if the probe did not mark it.
        self.assertTrue(verdict({'window_check':{'ratio':1.02,'biased':False}}).startswith('refused'))
        self.assertTrue(verdict({'window_check':{'ratio':None,'biased':True}}).startswith('refused'))

    def test_a_unit_that_alternates_only_once_the_loop_runs_is_refused(self):
        # The probe's units cost the same through the warm-up check and the loop's first 20
        # units, then alternate 1.0 and 1.5 ms. A check made before `ready.json` cannot see it;
        # the window's own even/odd sums must, and the window is refused. Counting from the
        # warm-up's end keeps the even half from being diluted on a slow host, where a 2 s window
        # completes few units.
        import argparse,pressure_probe
        calls=[0]
        def unit():
            calls[0]+=1
            spin=1.5e-3 if calls[0]>pressure_probe.WARM_UNITS+20 and calls[0]%2==0 else 1e-3
            end=time.thread_time()+spin
            while time.thread_time()<end:pass
        saved=(pressure_probe.cpu_unit,pressure_probe.local_guard,pressure_probe.rss_bytes)
        pressure_probe.cpu_unit=unit
        pressure_probe.local_guard=lambda *_:{'errors':[]}
        pressure_probe.rss_bytes=lambda:0
        try:
            with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
                result=pressure_probe.run(argparse.Namespace(mode='cpu',seconds=2.,cpu=0,
                    output=str(Path(temporary)/'probe'),duty=1.,period_ms=500.,no_self_check=False,
                    self_check_holds=0))
        finally:
            pressure_probe.cpu_unit,pressure_probe.local_guard,pressure_probe.rss_bytes=saved
        self.assertGreater(result['window_check']['ratio'],1.2)
        self.assertTrue(result['window_check']['biased'])
        self.assertTrue(pressure_probe.window_verdict(result).startswith('refused'))

    def test_a_duty_probe_is_busy_for_its_duty_of_each_period(self):
        import argparse,pressure_probe
        pause=pressure_probe.duty_pause
        self.assertEqual(pause(.15,0.,1.,.2),(0.,0.),'a full duty never idles')
        self.assertEqual(pause(0.,0.,.4,.2),(0.,0.))
        self.assertEqual(pause(.079,0.,.4,.2),(0.,0.))
        for now,idle in [(.081,.119),(.199,.001)]:
            self.assertEqual(pause(now,0.,.4,.2)[1],0.,'the off phase still belongs to its period')
            self.assertAlmostEqual(pause(now,0.,.4,.2)[0],idle)
        self.assertEqual(pause(.21,0.,.4,.2),(0.,.2))
        seconds,phase=pause(.65,0.,.4,.2)
        self.assertEqual(seconds,0.);self.assertAlmostEqual(phase,.6,msg='a late unit starts the period it falls in')
        def busy(duty,unit=.001,seconds=20.,period=.5):
            # The probe's loop: idle in steps no longer than the drain check, else run a unit.
            now=phase=spent=0.
            while now<seconds:
                idle,phase=pause(now,phase,duty,period)
                if idle:now+=min(idle,pressure_probe.CHECK_SECONDS);continue
                now+=unit;spent+=unit
            return spent/now
        for duty in [1.,.7,.4]:
            self.assertAlmostEqual(busy(duty),duty,delta=.01,msg=f'duty {duty}')
        for duty,period_ms in [(0.,200),(1.5,200),(.5,0),(.5,20000)]:
            with self.assertRaises(ValueError):
                pressure_probe.run(argparse.Namespace(mode='gpu',seconds=1.,cpu=0,output='unused',duty=duty,period_ms=period_ms))
        usage=subprocess.run([sys.executable,str(Path(__file__).parents[1]/'tools/pressure_probe.py'),'--help'],
            capture_output=True,text=True,check=True).stdout
        for option in ['--duty','--period-ms']:
            self.assertIn(option,usage)

    def test_the_own_efficiency_profile_uses_the_defaults_outside_the_probes_scope(self):
        node={'node_mode':'guaranteed','cpu':{'reserve_physical_cores':1,'nice':0},
            'gpu':{'scale_up_cooldown_ms':30000},'contention':{'default_mode':'neutral'}}
        agent={'cpu_affinity':[2,3],'max_workers':3}
        tuned,deployed,placement=comparison.profile_settings('own-efficiency',node,agent,None,None,[2,3],[0,1,2,3],'cpu','GPU-x')
        self.assertNotIn('contention',tuned,'the default mode, Performance')
        neutral,_,_=comparison.profile_settings('own-efficiency',node,agent,'neutral',None,[2,3],[0,1,2,3],'cpu','GPU-x')
        self.assertEqual(neutral['contention'],{'default_mode':'neutral'},'a given mode and nothing else')
        with self.assertRaisesRegex(ValueError,'floors'):
            comparison.profile_settings('own-efficiency',node,agent,None,1024,[2,3],[0,1,2,3],'cpu','GPU-x')
        self.assertNotIn('reserve_physical_cores',tuned['cpu'])
        self.assertEqual(tuned['gpu'],{'scale_up_cooldown_ms':30000})
        self.assertEqual(deployed['cpu_affinity'],[2,3])
        self.assertEqual(placement,{'profile':'own-efficiency','probe_cpu':2,'agent_scope_cpus':[2,3],
            'coordinator_and_harness_cpus':[0,1],'reserve_physical_cores':0})
        with self.assertRaisesRegex(ValueError,'outside'):
            comparison.profile_settings('own-efficiency',node,agent,'performance',None,[2,3],[2,3],'cpu','GPU-x')
        tuned,_,placement=comparison.profile_settings('protection',node,agent,None,None,[2,3],[0,1,2,3],'cpu','GPU-x')
        self.assertEqual(tuned['cpu']['reserve_physical_cores'],1)
        self.assertEqual(tuned['contention']['default_mode'],'neutral')
        self.assertEqual(placement['reserve_physical_cores'],1)
        self.assertEqual(placement['coordinator_and_harness_cpus'],[2,3])
        self.assertEqual(node['contention'],{'default_mode':'neutral'},'the input stays unchanged')

    def test_the_agent_starts_on_its_scope_wherever_the_harness_runs(self):
        argv=comparison.agent_argv(Path('/opt/cedegrid'),Path('/run'),[2,3])
        self.assertEqual(argv[:3],['taskset','-c','2,3'])
        self.assertEqual(argv[3:],['/opt/cedegrid','--config','/run/node.toml','agent','--deployment','/run/agent.toml'])
        # The protected load and the unmanaged probe start there too.
        self.assertEqual(comparison.on_scope(['python','probe.py'],[2,3]),['taskset','-c','2,3','python','probe.py'])

    @staticmethod
    def fake_host(root,nice=0,scope='protected.scope'):
        proc=root/'proc';cgroup=root/'cgroup';leaf=cgroup/'user.slice'/scope
        (proc/'4242').mkdir(parents=True);leaf.mkdir(parents=True)
        fields=['S','1','1','1','0','-1','0']+['0']*9+[str(nice),'1','0','123456']+['0']*30
        (proc/'4242/stat').write_text('4242 (python3 (x)) '+' '.join(fields)+'\n')
        (proc/'4242/cgroup').write_text('0::/user.slice/'+scope+'\n')
        (leaf/'cpu.max').write_text('max 100000\n');(leaf/'memory.high').write_text('max\n')
        return proc,cgroup
