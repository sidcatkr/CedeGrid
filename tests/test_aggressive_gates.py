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
