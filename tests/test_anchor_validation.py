"""Evidence-reader tests only: no Linux workloads, GPU use or remote access."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import anchor_validation as anchor
from soak import AllocationLedger


class AnchorEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
    def tearDown(self):
        self.temp.cleanup()

    def game(self):
        replay = self.root / 'replay.json'
        replay.write_text('{"real":true}')
        row = {'game_id': 1, 'seed': 8, 'backend': 'torchscript', 'device': 'cuda:0',
               'inference_count': 5, 'inference_errors': [], 'replay': str(replay),
               'replay_sha256': anchor.sha256_file(replay)}
        self.write('games.json', [row])
        self.write('run_manifest.json', {})
        self.write('summary.json', dict(bad_status_games=0, invalid_action_games=0, exception_games=0))
        return row
    def write(self, name, value):
        (self.root / name).write_text(json.dumps(value))

    def test_real_inference_and_replay_integrity_required(self):
        row = self.game()
        self.assertEqual(anchor.validate_games(self.root, 1)['games'], 1)
        row['inference_errors'] = ['fallback after prediction error']
        self.write('games.json', [row])
        with self.assertRaisesRegex(RuntimeError, 'inference'):
            anchor.validate_games(self.root, 1)
        row['inference_errors'] = []; row['replay_sha256'] = '0' * 64
        self.write('games.json', [row])
        with self.assertRaisesRegex(RuntimeError, 'integrity'):
            anchor.validate_games(self.root, 1)

    def test_missing_summary_is_not_success(self):
        self.game(); self.write('summary.json', {})
        with self.assertRaises(RuntimeError):
            anchor.validate_games(self.root, 1)

    def test_protocol_status_field_and_unknown_fail_closed(self):
        anchor.verify_task_status([{'status': 'assigned'}])
        for tasks in ([], [{'state': 'assigned'}], [{'status': 'needs_reconciliation'}]):
            with self.assertRaises(RuntimeError):
                anchor.verify_task_status(tasks)

    def test_identity_mismatch_cannot_be_attributed(self):
        boot = self.root / 'sys/kernel/random'
        boot.mkdir(parents=True); (boot / 'boot_id').write_text('boot')
        process = self.root / '7'; process.mkdir()
        fields = ['S'] + ['0'] * 21
        fields[19] = '42'; fields[21] = '8'; fields[11] = '100'
        (process / 'stat').write_text('7 (test with spaces) ' + ' '.join(fields))
        identity = {'pid': 7, 'boot_id': 'boot', 'start_time': 42}
        self.assertIsNotNone(anchor.identity_sample(identity, self.root))
        identity['start_time'] = 43
        self.assertIsNone(anchor.identity_sample(identity, self.root))

    def test_missing_manager_measurement_is_inconclusive_not_zero(self):
        measured = anchor.manager_metrics([{'processes': {'agent': None},
                                           'observed_monotonic': 1, 'missing': ['agent']}])
        self.assertEqual(measured['status'], 'inconclusive')
        self.assertIsNone(measured['sampled_peak_rss_bytes'])
        self.assertIsNone(measured['sampled_peak_active_cpu_cores'])

    def test_process_churn_prevents_full_overhead_pass(self):
        process = {'pid': 1, 'start_ticks': 2, 'boot_id': 'boot', 'cpu_seconds': 0, 'rss_bytes': 100}
        samples = [{'processes': {'agent': process, 'supervisor:x': dict(process, pid=2)},
                    'missing': [], 'observed_monotonic': 1},
                   {'processes': {'agent': process}, 'missing': [], 'observed_monotonic': 2}]
        self.assertEqual(anchor.manager_metrics(samples)['status'], 'inconclusive')

    def test_final_supervisor_accounting_closes_short_process_coverage(self):
        identity = {'pid': 2, 'start_time': 3, 'boot_id': 'boot', 'assignment_id': 'x', 'generation': 1}
        usage = {'identity': identity, 'scope': 'supervisor_self_excludes_workers',
                 'user_cpu_us': 100000, 'system_cpu_us': 50000, 'peak_rss_bytes': 1024,
                 'observed_monotonic_ms': 1900}
        final = anchor.final_supervisor_usage(identity, usage)
        process = {'pid': 1, 'start_ticks': 2, 'boot_id': 'boot', 'cpu_seconds': 0, 'rss_bytes': 100}
        samples = [{'processes': {'agent': process}, 'missing': [], 'observed_monotonic': 1},
                   {'processes': {'agent': dict(process, cpu_seconds=.1)},
                    'finalized': {'supervisor:x': final}, 'missing': [], 'observed_monotonic': 2}]
        metrics = anchor.manager_metrics(samples)
        self.assertEqual(metrics['status'], 'passed')
        self.assertAlmostEqual(metrics['sampled_peak_active_cpu_cores'], .25)
        self.assertEqual(metrics['sampled_peak_rss_bytes'], 1124)
        with self.assertRaises(ValueError):
            anchor.final_supervisor_usage(dict(identity, generation=2), usage)

    def test_real_reservation_shape_joins_and_retains_attempt_generation(self):
        # Actual coordinator reservation JSON omits generation; task JSON carries it.
        ledger = AllocationLedger(['node'])
        reservation = {'assignment_id': 'x', 'node_id': 'node', 'phase': 'running'}
        ledger.observe({'allocations': [reservation], 'tasks': [
            {'task_id': 'task', 'assignment_id': 'x', 'generation': 1}]})
        identity = {'pid': 2, 'start_time': 3, 'boot_id': 'boot', 'assignment_id': 'x', 'generation': 1}
        directory = self.root / 'attempts/x'; directory.mkdir(parents=True)
        (directory / 'supervisor-identity.json').write_text(json.dumps(identity))
        sample = {'pid': 2, 'start_ticks': 3, 'boot_id': 'boot', 'cpu_seconds': .1, 'rss_bytes': 1024}
        with patch.object(anchor, 'identity_sample', return_value=sample):
            observed = anchor.manager_samples({}, self.root, ledger)
        self.assertEqual(observed['missing'], [])
        self.assertEqual(observed['processes']['supervisor:x'], sample)
        ledger.observe({'allocations': [{**reservation, 'phase': 'released'}],
                        'tasks': [{'task_id': 'task', 'assignment_id': 'new-attempt', 'generation': 2}]})
        self.assertEqual(ledger.allocations['x']['generation'], 1)
        with self.assertRaisesRegex(RuntimeError, 'generation changed'):
            ledger.observe({'allocations': [{**reservation, 'phase': 'released'}],
                            'tasks': [{'assignment_id': 'x', 'generation': 3}]})

    def test_sample_binds_a_new_supervisor_with_one_authenticated_refresh(self):
        before = {'allocations': [], 'tasks': []}
        after = {'allocations': [{'assignment_id': 'x', 'node_id': 'node', 'phase': 'running'}],
                 'tasks': [{'assignment_id': 'x', 'generation': 1}]}
        client = Mock(); client.status.side_effect = [before, after]
        ledger = AllocationLedger(['node'])
        def sample(_services, _state, current):
            known = current.allocations.get('x', {}).get('generation') == 1
            return {'missing': [] if known else ['unmatched_supervisor_identity']}
        with patch.object(anchor, 'manager_samples', side_effect=sample):
            status, observed = anchor.current_manager_sample(client, {}, self.root, ledger)
        self.assertEqual(client.status.call_count, 2)
        self.assertEqual(status, after)
        self.assertEqual(observed['missing'], [])
        client.status.side_effect = [before, before]
        with patch.object(anchor, 'manager_samples', return_value={'missing': ['unmatched_supervisor_identity']}):
            _, observed = anchor.current_manager_sample(client, {}, self.root, AllocationLedger(['node']))
        self.assertEqual(observed['missing'], ['unmatched_supervisor_identity'])


if __name__ == '__main__':
    unittest.main()
