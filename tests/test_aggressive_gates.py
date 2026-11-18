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


class ArgumentsTest(unittest.TestCase):
    def test_fill_runs_only_the_two_modes(self):
        for arm in ('alone', 'plain', 'unprotected'):
            result = subprocess.run(
                [sys.executable, str(Path(gates.__file__)), '--repo', '.', '--output', 'unused',
                 '--python', sys.executable, '--shape', 'fill', '--arm', arm],
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn('fill runs performance or aggressive', result.stderr)

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
