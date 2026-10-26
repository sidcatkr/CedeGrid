"""Ramp gate analysis, without launching a coordinator, an agent or native work."""
from pathlib import Path
import sys
import unittest
sys.path.insert(0,str(Path(__file__).parents[1]/'tools'))
import ramp_gate


def row(at_ms,own,external=0,share=3920,actions=()):
    return (at_ms,{'resources':{'cpu':{'own':[own,own],'external':external,'target_share':share}},
        'actions':list(actions)})


class RampGateTests(unittest.TestCase):
    def test_ramp_time_counts_from_submission_to_ninety_percent_of_the_target_share(self):
        rows=[row(9000,0),row(10000,0),row(12000,1750),row(12500,3500),row(13000,3750)]
        series=ramp_gate.cpu_series(rows)
        target=ramp_gate.ramp_target(series,10.)
        self.assertEqual(target,3528)
        self.assertEqual(ramp_gate.seconds_to(series,10.,lambda own,external:own>=target),3.0)

    def test_a_ramp_that_never_reaches_the_target_has_no_time(self):
        series=ramp_gate.cpu_series([row(10000,0),row(11000,3500)])
        self.assertIsNone(ramp_gate.seconds_to(series,10.,lambda own,external:own>=3528))

    def test_decisions_without_a_cpu_reading_are_skipped(self):
        rows=[(1000,{'resources':{}}),(1500,{'resources':{'cpu':{'own':None}}}),row(2000,100)]
        self.assertEqual(ramp_gate.cpu_series(rows),[(2.0,100,0,3920)])

    def test_no_target_share_after_submission_is_an_error(self):
        with self.assertRaises(ValueError):
            ramp_gate.ramp_target(ramp_gate.cpu_series([row(1000,0)]),5.)

    def test_ramp_time_follows_each_samples_target_share(self):
        # The share falls as the services and supervisors add external use.
        rows=[row(10000,0,share=3780),row(12000,2400,share=3078),row(13000,2800,share=3078)]
        series=ramp_gate.cpu_series(rows)
        self.assertEqual(ramp_gate.ramp_seconds(series,10.),3.0)
        self.assertIsNone(ramp_gate.ramp_seconds(series[:2],10.))

    def test_contender_service_counts_only_its_own_cpu(self):
        # CPU seconds of the contender itself, sampled every half second.
        samples=[(10.0,0.0),(10.5,0.2),(11.0,0.65),(11.5,1.12)]
        self.assertEqual(ramp_gate.served_seconds(samples,10.),1.0)
        self.assertIsNone(ramp_gate.served_seconds(samples[:2],10.))

    def test_the_duty_loop_holds_cpu_time_at_its_share(self):
        self.assertIn('process_time',ramp_gate.DUTY_LOOP)

    def test_an_action_on_any_allocation_the_gate_did_not_submit_is_reported(self):
        own={'allocation_id':'mine','step':'cap'};other={'allocation_id':'other','step':'cap'}
        rows=[row(1000,0,actions=[own]),row(1500,0,actions=[own,other])]
        self.assertEqual(ramp_gate.foreign_actions(rows,{'mine'}),[other])

    def test_tasks_request_their_duty_cycle(self):
        task=ramp_gate.busy_task('t',Path('/usr/bin/python3'),40,Path.home(),{})
        self.assertEqual(task['resources']['cpu_millicores'],ramp_gate.TASK_MILLICORES)
        self.assertEqual(task['class'],'opportunistic')
        self.assertEqual(task['argv'][-2:],['40','25.0'])

    def test_execution_needs_the_explicit_flag_on_linux(self):
        args=type('Args',(),{'execute':False})()
        with self.assertRaises(ValueError):
            ramp_gate.run(args)


if __name__=='__main__':
    unittest.main()
