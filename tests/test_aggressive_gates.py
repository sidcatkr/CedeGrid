"""The pure parts of tools/aggressive_gates.py, feature 004's host gate harness."""
from pathlib import Path
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import aggressive_gates as gates


class PeerSummaryTest(unittest.TestCase):
    def test_counts_only_the_steady_part_of_the_window(self):
        # Own work is submitted 5 s after the epoch; the first 10 s after it are left out.
        rows = [(1.0, 9.0), (14.9, 9.0)] + [(15 + i * .5, 1.0 + i) for i in range(100)] + [(65.0, 9.0)]
        summary = gates.peer_summary(rows, 60)
        self.assertEqual(summary['rate'], 2.0)
        self.assertEqual(summary['p99_ms'], 99.0)

    def test_an_empty_window_has_no_latency(self):
        self.assertEqual(gates.peer_summary([], 60), {'rate': 0.0, 'p99_ms': None})


class AdmittedAtTest(unittest.TestCase):
    def test_reads_the_first_sample_at_or_after_each_time(self):
        samples = [[0.1, 0, 0], [0.6, 1, 0], [1.4, 2, 1], [3.0, 2, 2]]
        self.assertEqual(gates.admitted_at(samples, (0.5, 1, 2, 4)), [1, 2, 2, None])


class CompletionTest(unittest.TestCase):
    def test_reads_each_finished_tasks_completion_after_submission(self):
        own = [{'finished': 115.0}, {'finished': 130.5}, {'units': 0}, {'finished': 160.0}]
        self.assertEqual(gates.completion(own, 100.0),
                         {'completion_s': [15.0, 30.5, 60.0], 'mean_s': 35.17, 'makespan_s': 60.0})
        self.assertEqual(gates.completion([], 100.0),
                         {'completion_s': [], 'mean_s': None, 'makespan_s': None})


class ServiceTicksTest(unittest.TestCase):
    """CedeGrid's own CPU in a run: the agent and its supervisors, from the run's binary."""

    def proc(self, directory, pid, exe, utime, stime, start, comm='cedegrid'):
        entry = Path(directory) / str(pid)
        entry.mkdir()
        (entry / 'exe').symlink_to(exe)
        rest = ['S', '1', '1', '1', '0', '-1', '0', '0', '0', '0', '0', str(utime), str(stime),
                '0', '0', '20', '0', '1', '0', str(start)]
        (entry / 'stat').write_text(f'{pid} ({comm}) ' + ' '.join(rest) + ' 0 0\n')

    def test_counts_only_the_binarys_processes_and_skips_the_coordinator(self):
        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'cedegrid'
            binary.write_text('')
            proc = Path(directory) / 'proc'
            proc.mkdir()
            self.proc(proc, 10, binary, 70, 30, 500)
            self.proc(proc, 11, binary, 5, 5, 600, comm='cedegrid sup')
            self.proc(proc, 12, binary, 900, 100, 400)
            self.proc(proc, 13, Path(directory) / 'python', 400, 0, 700, comm='python')
            (proc / 'self').mkdir()
            ticks = gates.service_ticks(binary, skip={12}, root=proc)
            self.assertEqual(ticks, {(10, 500): 100, (11, 600): 10})

    def test_the_runs_cpu_is_each_processs_growth_from_the_start(self):
        start = {(10, 500): 100}
        later = {(10, 500): 150, (11, 600): 30}
        self.assertEqual(gates.ticks_since(start, [start, later]), 80)
        self.assertEqual(gates.ticks_since(start, [later, {(10, 500): 120}]), 80)


class EndedTest(unittest.TestCase):
    def test_a_task_ends_with_a_result_or_when_its_process_is_gone(self):
        import json, os, tempfile
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'own-0.json'
            self.assertFalse(gates.ended(path))
            Path(f'{path}.pid').write_text(json.dumps({'pid': os.getpid(), 'started': 0}))
            if Path('/proc').exists():
                self.assertFalse(gates.ended(path))
            path.write_text('{}')
            self.assertTrue(gates.ended(path))


class PollDueTest(unittest.TestCase):
    def test_polls_fast_after_submission_then_once_a_second(self):
        self.assertTrue(gates.poll_due(0.3, 0.25))
        self.assertTrue(gates.poll_due(1.9, 0.0))
        self.assertFalse(gates.poll_due(2.5, 0.5))
        self.assertTrue(gates.poll_due(2.5, 1.0))
        self.assertFalse(gates.poll_due(40.0, 0.75))


class LayoutPadsTest(unittest.TestCase):
    def test_each_task_gets_its_own_length_within_the_range(self):
        import random
        pads = gates.layout_pads(400, random.Random(7))
        self.assertEqual(len(pads), 400)
        self.assertTrue(all(0 <= pad < 1024 for pad in pads))
        # Spread over the range, not one length for every task.
        self.assertGreater(len(set(pads)), 200)
        self.assertLess(min(pads), 64)
        self.assertGreater(max(pads), 960)

    def test_only_the_hashing_shapes_are_padded(self):
        self.assertEqual(gates.HASH_SHAPES, {'fill', 'f2c', 'first', 'batch'})
        self.assertFalse(gates.HASH_SHAPES & gates.GPU_SHAPES)


class PlainLauncherTest(unittest.TestCase):
    """The plain arm starts its tasks from one process already on the scope, as a user's shell or
    taskset there would, so the kernel places them as it places CedeGrid's forked tasks."""

    def test_the_launcher_moves_itself_then_starts_every_task(self):
        argv = gates.plain_launcher('/usr/bin/python3', [0, 2], [['a', '1'], ['b', '2']])
        self.assertEqual(argv[:2], ['/usr/bin/python3', '-c'])
        self.assertEqual(argv[3:], ['[0, 2]', '[["a", "1"], ["b", "2"]]'])

    @unittest.skipUnless(hasattr(__import__('os'), 'sched_setaffinity'), 'Linux only')
    def test_tasks_inherit_the_scope(self):
        import os, tempfile
        cpus = sorted(os.sched_getaffinity(0))[:1]
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / 'aff'
            task = [sys.executable, '-c', f'import os; open({str(out)!r}, "w").write(str(sorted(os.sched_getaffinity(0))))']
            subprocess.run(gates.plain_launcher(sys.executable, cpus, [task]), check=True, timeout=30)
            self.assertEqual(out.read_text(), str(cpus))


class ArgumentsTest(unittest.TestCase):
    def test_fill_runs_only_the_two_modes(self):
        for arm in ('alone', 'plain', 'unprotected'):
            result = subprocess.run(
                [sys.executable, str(Path(gates.__file__)), '--repo', '.', '--output', 'unused',
                 '--python', sys.executable, '--shape', 'fill', '--arm', arm],
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn('fill runs performance or aggressive', result.stderr)

    def test_fixed_work_shapes_run_plain_or_performance(self):
        for shape in ('batch', 'vram', 'ram'):
            result = subprocess.run(
                [sys.executable, str(Path(gates.__file__)), '--repo', '.', '--output', 'unused',
                 '--python', sys.executable, '--shape', shape, '--arm', 'aggressive'],
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn(f'{shape} runs plain or performance', result.stderr)

    def test_the_peer_role_needs_its_times_and_admit_needs_a_cg_shape(self):
        for args, message in ((['--role', 'peer'], 'the peer role takes'),
                              (['--shape', 'gpu', '--arm', 'admit'], 'admit runs the cg shapes'),
                              (['--shape', 'gpu'], '--shape and --arm are required')):
            result = subprocess.run(
                [sys.executable, str(Path(gates.__file__)), '--repo', '.', '--output', 'unused',
                 '--python', sys.executable, *args], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn(message, result.stderr)

    def test_plain_tasks_go_only_with_the_plain_arm(self):
        for arm, count in (('performance', '1'), ('plain', '0')):
            result = subprocess.run(
                [sys.executable, str(Path(gates.__file__)), '--repo', '.', '--output', 'unused',
                 '--python', sys.executable, '--shape', 'gpu', '--arm', arm, '--plain-tasks', count],
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn('--plain-tasks takes a positive count', result.stderr)


if __name__ == '__main__':
    unittest.main()
