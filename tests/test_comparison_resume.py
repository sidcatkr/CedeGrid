"""Continuation evidence checks; no services, application workers or benchmarks run."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import anchor_validation as anchor


class ComparisonContinuationTests(unittest.TestCase):
    def setUp(self):
        temporary_root = Path.home() / '.local/share/resmgr-test-tmp'
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.old_id = 'comparison-original'
        self.path = self.root / 'evidence' / self.old_id / 'report.json'
        self.application = self.root / 'source/Kaggriculture'
        self.sources = {}
        for name in ('training/self_play.py', 'training/train.py',
                     'integration/resmgr/workflow.py', 'integration/resmgr/worker.py'):
            source = self.application / name
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text('# immutable fixture ' + name)
            self.sources[name] = anchor.sha256_file(source)
        league = self.application / 'config/league_bases_r004.json'
        self.write(league, {'opponents': []})
        self.expected = {'targets': copy.deepcopy(anchor.TARGETS), 'cpu_ids': [0, 1],
                         'device': 'cpu', 'actors_per_trial': 1,
                         'manager_binary_sha256': 'a' * 64,
                         'python_executable': str(self.root / 'venv/bin/python'),
                         'candidate_sha256': 'b' * 64, 'bootstrap_sha256': None}
        self.prior = {**copy.deepcopy(self.expected), 'schema_version': 1,
                      'run_id': self.old_id, 'stage': 'command', 'status': 'failed',
                      'error_type': 'TimeoutError', 'error': 'stage envelope elapsed',
                      'elapsed_seconds': 785.0, 'envelope': anchor.comparison_envelope('command', 'cpu', None),
                      'uncertain_allocations': [], 'cleanup': [
                          {'service': 'agent', 'pid': 2000000001, 'boot_id': 'old-boot',
                           'start_ticks': 10, 'reaped': True, 'exit_code': 0},
                          {'service': 'coordinator', 'pid': 2000000002, 'boot_id': 'old-boot',
                           'start_ticks': 11, 'reaped': True, 'exit_code': 0}],
                      'cases': []}
        self.final = {'tasks': [], 'allocations': []}
        for repetition, mode in ((0, 'unmanaged'), (0, 'managed'), (1, 'managed')):
            case_id = self.old_id + '.pair' + str(repetition)
            case_root = self.root / 'runs' / case_id
            generated = {'run_id': case_id, 'seed_id': case_id, 'model_sha256': 'c' * 64,
                         'games': 12, 'workers': 1, 'implementation_files': self.sources,
                         'league_sha256': anchor.sha256_file(league)}
            self.write(case_root / 'manifest.json', generated)
            output = case_root / ('command-' + mode)
            records = []
            for index in range(12):
                replay = output / ('replay-' + str(index) + '.json')
                self.write(replay, {'seed': index, 'completed': True})
                records.append({'game_id': index, 'seed': index, 'candidate_seat': index % 4,
                                'opponent': 'fixed-opponent', 'backend': 'torchscript',
                                'device': 'cpu', 'inference_count': 1, 'inference_errors': [],
                                'replay': str(replay), 'replay_sha256': anchor.sha256_file(replay)})
            self.write(output / 'games.json', records)
            self.write(output / 'run_manifest.json', {'run_seed': case_id, 'games': 12,
                       'workers': 1, 'act_timeout': 5.0, 'inference_device': 'cpu',
                       'resolved_inference_devices': ['cpu'], 'engine_version': 'fixture-v1',
                       'exploration': True, 'candidate': 'fixed-candidate', 'league': 'fixed-league'})
            self.write(output / 'summary.json', {'bad_status_games': 0,
                       'invalid_action_games': 0, 'exception_games': 0})
            self.prior['cases'].append({'repetition': repetition, 'mode': mode,
                'elapsed_seconds': 230.0, 'cpu_ids': [0, 1], 'device': 'cpu',
                'model_sha256': 'c' * 64, 'configured_environment': {'OMP_NUM_THREADS': '1'},
                'output': anchor.validate_games(output, 12, 'cpu')})
            if mode == 'managed':
                assignment = 'assignment-' + str(repetition)
                self.final['tasks'].append({'task_id': case_id + '.command', 'status': 'completed',
                    'generation': 1, 'assignment_id': assignment, 'receipt_hash': 'd' * 64})
                self.final['allocations'].append({'assignment_id': assignment, 'phase': 'released'})
        self.save()
        # Only the OS observation is mocked. File parsing, path checks, result,
        # replay and source integrity all use the production readers above.
        self.observation = patch.object(anchor, 'prior_process_absent', return_value=True)
        self.observation.start()
        self.addCleanup(self.observation.stop)

    @staticmethod
    def write(path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def save(self):
        self.write(self.path, self.prior)
        self.write(self.path.with_name('final-status.json'), self.final)

    def read(self):
        return anchor.resume_cases(self.path, self.root, self.expected)

    def assert_refused(self):
        self.save()
        with self.assertRaises((ValueError, RuntimeError, KeyError, TypeError)):
            self.read()

    def test_completed_prefix_reused_without_modifying_original_evidence(self):
        before = self.path.read_bytes()
        report, provenance = self.read()
        self.assertEqual(provenance['reused_cases'], 3)
        self.assertEqual(provenance['elapsed_seconds'], 785.0)
        self.assertEqual(provenance['sha256'], anchor.sha256_file(self.path))
        self.assertEqual(report['cases'][2]['output']['manifest']['run_seed'], self.old_id + '.pair1')
        self.assertEqual(self.path.read_bytes(), before)
        self.assertTrue(all(Path(case['output_path']).is_relative_to(self.root / 'runs')
                            for case in report['cases']))

    def test_changed_context_and_relaxed_acceptance_targets_are_refused(self):
        original = copy.deepcopy(self.prior)
        changes = {'manager_binary_sha256': 'e' * 64, 'python_executable': '/different/python',
                   'candidate_sha256': 'f' * 64, 'bootstrap_sha256': 'f' * 64,
                   'actors_per_trial': 2, 'cpu_ids': [0, 2], 'device': 'cuda:0',
                   'targets': {**anchor.TARGETS, 'matched_throughput_ratio_min': .5}}
        for key, value in changes.items():
            with self.subTest(key=key):
                self.prior = {**copy.deepcopy(original), key: value}
                self.assert_refused()

    def test_only_original_deadline_failure_may_resume(self):
        original = copy.deepcopy(self.prior)
        for change in ({'status': 'passed'}, {'error_type': 'RuntimeError'},
                       {'error': 'guard abort'}, {'resumed_from': {'report': 'older.json'}},
                       {'run_id': '../other'}, {'run_id': 'different-safe-id'}):
            with self.subTest(change=change):
                self.prior = {**copy.deepcopy(original), **change}
                self.assert_refused()

    def test_uncertain_unreaped_or_unresolved_services_block_reuse(self):
        original = copy.deepcopy(self.prior)
        changes = ({'uncertain_allocations': [{'assignment_id': 'uncertain'}]},
                   {'cleanup_error': 'unavailable'}, {'measurement_cleanup_error': 'unavailable'},
                   {'unresolved_owned_services': [{'name': 'agent'}]}, {'cleanup': []},
                   {'cleanup': [original['cleanup'][0]]},
                   {'cleanup': [{**row, 'reaped': False} for row in original['cleanup']]})
        for change in changes:
            with self.subTest(change=change):
                self.prior = {**copy.deepcopy(original), **change}
                self.assert_refused()

    def test_verified_live_prior_identity_blocks_reuse(self):
        with patch.object(anchor, 'prior_process_absent', return_value=False):
            with self.assertRaisesRegex(ValueError, 'still live'):
                self.read()

    def test_unreleased_or_missing_allocation_and_task_evidence_is_refused(self):
        original = copy.deepcopy(self.final)
        for mutate in (
                lambda final: final['allocations'].clear(),
                lambda final: final['allocations'][0].update(phase='uncertain'),
                lambda final: final['tasks'][0].update(status='assigned'),
                lambda final: final['tasks'][0].update(assignment_id='different-assignment')):
            self.final = copy.deepcopy(original)
            mutate(self.final)
            with self.subTest(final=self.final):
                self.assert_refused()

    def test_prefix_order_duplicates_and_invalid_case_durations_are_refused(self):
        original = copy.deepcopy(self.prior)
        for mutate in (
                lambda p: p['cases'].reverse(),
                lambda p: p['cases'].append(copy.deepcopy(p['cases'][0])),
                lambda p: p['cases'][0].update(repetition=True),
                lambda p: p['cases'][0].update(elapsed_seconds=0),
                lambda p: p['cases'][0].update(elapsed_seconds=float('inf')),
                lambda p: p['cases'][0].update(elapsed_seconds=float('nan'))):
            self.prior = copy.deepcopy(original)
            mutate(self.prior)
            with self.subTest(cases=self.prior['cases']):
                self.assert_refused()

    def test_corrupt_replay_or_recorded_result_cannot_be_reused(self):
        replay = self.root / 'runs' / (self.old_id + '.pair0') / 'command-unmanaged/replay-0.json'
        before = replay.read_bytes()
        replay.write_text('{"tampered":true}')
        with self.assertRaisesRegex(RuntimeError, 'integrity'):
            self.read()
        replay.write_bytes(before)
        self.prior['cases'][0]['output']['identities'][0]['seed'] = 999
        self.assert_refused()

    def test_application_or_league_changes_cannot_be_mixed(self):
        for relative in ('training/self_play.py', 'config/league_bases_r004.json'):
            with self.subTest(relative=relative):
                path = self.application / relative
                before = path.read_bytes()
                path.write_text('changed')
                with self.assertRaises(ValueError):
                    self.read()
                path.write_bytes(before)

    def test_empty_or_escaping_source_manifest_cannot_skip_integrity_checks(self):
        manifest_path = self.root / 'runs' / (self.old_id + '.pair0') / 'manifest.json'
        generated = json.loads(manifest_path.read_text())
        outside = self.root / 'outside.py'
        outside.write_text('outside')
        for files in ({}, {'../../outside.py': anchor.sha256_file(outside)}):
            with self.subTest(files=files):
                self.write(manifest_path, {**generated, 'implementation_files': files})
                with self.assertRaises(ValueError):
                    self.read()

    def test_cpu_budget_extension_preserves_all_non_wall_targets(self):
        targets = copy.deepcopy(anchor.TARGETS)
        baseline = anchor.comparison_envelope('command', 'cpu', None)
        extended = anchor.comparison_envelope('command', 'cpu', None, 1800)
        for key in ('family_rss_bytes', 'gpu_memory_mib', 'cleanup_reserve_seconds'):
            self.assertEqual(extended[key], baseline[key])
        self.assertEqual(extended['work_seconds'], 1680)
        self.assertEqual(anchor.TARGETS, targets)
        for invalid in (899, 1801, True, 900.0, float('inf')):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                anchor.comparison_envelope('command', 'cpu', None, invalid)


class PriorProcessAbsenceTests(unittest.TestCase):
    def setUp(self):
        temporary_root = Path.home() / '.local/share/resmgr-test-tmp'
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.addCleanup(self.temporary.cleanup)
        self.proc = Path(self.temporary.name)
        boot = self.proc / 'sys/kernel/random/boot_id'
        boot.parent.mkdir(parents=True)
        boot.write_text('same-boot\n')
        self.identity = {'pid': 123, 'start_ticks': 42, 'boot_id': 'same-boot'}

    def stat(self, start=42, state='S'):
        process = self.proc / '123'
        process.mkdir(exist_ok=True)
        fields = [state] + ['0'] * 21
        fields[19] = str(start)
        (process / 'stat').write_text('123 (command with ) delimiter) ' + ' '.join(fields))

    def test_reused_pid_is_absence_of_original_identity_but_live_and_zombie_are_not(self):
        self.assertTrue(anchor.prior_process_absent(self.identity, self.proc))
        self.stat()
        self.assertFalse(anchor.prior_process_absent(self.identity, self.proc))
        self.stat(state='Z')
        self.assertFalse(anchor.prior_process_absent(self.identity, self.proc))
        self.stat(start=43)
        self.assertTrue(anchor.prior_process_absent(self.identity, self.proc))

    def test_changed_boot_or_incomplete_identity_requires_new_review(self):
        for change in ({'boot_id': 'different-boot'}, {'boot_id': ''}, {'pid': True},
                       {'pid': 0}, {'start_ticks': None}, {'start_ticks': 0}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                anchor.prior_process_absent({**self.identity, **change}, self.proc)

    def test_missing_or_malformed_stat_does_not_mean_an_existing_process_is_gone(self):
        process = self.proc / '123'
        process.mkdir()
        with self.assertRaises(ValueError):
            anchor.prior_process_absent(self.identity, self.proc)
        (process / 'stat').write_text('malformed')
        with self.assertRaises((ValueError, IndexError)):
            anchor.prior_process_absent(self.identity, self.proc)

    def test_permission_failure_is_not_absence(self):
        self.stat()
        read_text = Path.read_text

        def observe(path, *args, **kwargs):
            if path == self.proc / '123/stat':
                raise PermissionError('fixture access denied')
            return read_text(path, *args, **kwargs)

        with patch.object(Path, 'read_text', observe), self.assertRaises(PermissionError):
            anchor.prior_process_absent(self.identity, self.proc)


if __name__ == '__main__':
    unittest.main()
