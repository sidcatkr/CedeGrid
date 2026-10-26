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

    def test_contender_service_waits_for_its_core(self):
        series=ramp_gate.cpu_series([row(14000,3750,0),row(15000,3500,400),row(16500,2900,950)])
        served=ramp_gate.seconds_to(series,15.,lambda own,external:external>=900)
        self.assertEqual(served,1.5)

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
