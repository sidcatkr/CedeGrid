"""Bounded two-node harness contracts; no remote access or workload execution."""
import copy
import hashlib
import json
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
from two_node_validation import Phases, accepted_receipts, pressure_receipt, validate


def allocation(phase='running', generation=1):
    return {'phase': phase, 'node_id': 'burst', 'generation': generation, 'present_in_latest_status': True}


def task(generation=1, assignment='a1', status='assigned'):
    return {'task_id': 'exp.game3.r0', 'assignment_id': assignment, 'generation': generation, 'status': status}


class PhaseTests(unittest.TestCase):
    def setUp(self):
        self.phases = Phases('anchor', 'burst')
        self.ledger = SimpleNamespace(allocations={'a1': allocation()})
        self.accepted = {'game0': {'node_id': 'anchor'}, 'game1': {'node_id': 'burst'}}

    def step(self, current, now):
        return self.phases.update({'tasks': [current]}, self.ledger, self.accepted, now)

    def disconnect(self):
        self.assertEqual(self.step(task(), 20), 'drain')
        self.ledger.allocations['a1']['phase'] = 'released'
        self.assertIsNone(self.step(task(), 22), 'dispatch/release alone is not anchor continuity')
        self.accepted['game2'] = {'node_id': 'anchor'}
        self.assertEqual(self.step(task(), 25), 'resume')
        self.ledger.allocations['a2'] = allocation(generation=2)
        self.assertEqual(self.step(task(2, 'a2'), 30), 'proxy_stop')

    def test_two_disruptions_require_continuity_reservation_and_fenced_result(self):
        self.disconnect()
        self.ledger.allocations['a2']['phase'] = 'uncertain'
        self.assertIsNone(self.step(task(2, 'a2'), 45))
        self.assertEqual(self.step(task(2, 'a2'), 55), 'proxy_start')
        self.ledger.allocations['a2']['phase'] = 'released'
        self.ledger.allocations['a3'] = allocation('released', 3)
        self.assertIsNone(self.step(task(3, 'a3', 'completed'), 80))
        self.phases.require_done()
        self.assertEqual(self.phases.events[-2]['held_phase'], 'uncertain')

    def test_disappearing_capacity_does_not_count_as_release(self):
        self.disconnect()
        self.ledger.allocations['a2']['present_in_latest_status'] = False
        with self.assertRaisesRegex(RuntimeError, 'disappeared'):
            self.step(task(2, 'a2'), 45)

    def test_fresh_pre_disconnect_release_is_allowed_but_not_fabricated_retry(self):
        self.disconnect()
        self.ledger.allocations['a2']['phase'] = 'released'
        self.assertEqual(self.step(task(2, 'a2', 'completed'), 55), 'proxy_start')
        with self.assertRaisesRegex(RuntimeError, 'without a later fenced attempt'):
            self.step(task(2, 'a2', 'completed'), 56)

    def test_natural_early_finish_and_scenario_timeout_are_failures(self):
        with self.assertRaisesRegex(RuntimeError, 'before required scenarios'):
            self.phases.require_done()
        self.step(task(), 20)
        with self.assertRaisesRegex(TimeoutError, 'phase deadline'):
            self.step(task(), 141)

    def test_one_node_contribution_does_not_start_a_two_node_claim(self):
        self.accepted.pop('game1')
        self.assertIsNone(self.step(task(), 20))
        self.assertEqual(self.phases.phase, 'normal')

    def test_accepted_result_requires_matching_authenticated_attempt(self):
        client = SimpleNamespace(result=lambda _: {'assignment_id': 'a1', 'generation': 2,
                                                    'result': {'metadata': {'experiment_id': 'exp'}}})
        with self.assertRaisesRegex(RuntimeError, 'authenticated allocation'):
            accepted_receipts(client, {'tasks': [task(status='completed')]}, self.ledger, 'exp')


class EnvelopeTests(unittest.TestCase):
    def setUp(self):
        root = Path.home() / 'validation-unit-only'
        self.config = {'execution_approved': True, 'output': str(root), 'bootstrap': str(root / 'bootstrap.pt'),
                       'anchor_node_id': 'anchor', 'burst_node_id': 'burst',
                       'proxy_config': {'execution_approved': True, 'listen': ['127.0.0.1', 45670],
                                        'target': ['127.0.0.1', 45671], 'duration_seconds': 1200,
                                        'output': str(root / 'proxy')}}
        node = {'node_id': 'anchor', 'class': 'guaranteed', 'device': 'cpu', 'max_workers': 1, 'max_attempts': 4,
                'model_sha256': 'a' * 64, 'snapshot_sha256': 'b' * 64, 'python': '/home/example/bin/python',
                'source_root': '/home/example/source', 'sdk_root': '/home/example/sdk', 'candidate': '/home/example/main.py'}
        self.application = {'experiment_id': 'exp', 'run_seed': 'seed', 'games': 12,
                            'output': str(root / 'application'), 'client_config': str(root / 'operator.json'),
                            'nodes': [node, {**node, 'node_id': 'burst', 'class': 'opportunistic'}]}

    def test_valid_profile_preserves_one_actor_per_node(self):
        self.assertEqual(set(validate(self.config, self.application)), {'guaranteed', 'opportunistic'})

    def test_demand_mode_requires_two_explicit_cpus_and_pinned_script(self):
        self.config['cpu_demand'] = {'cpu_ids': [0, 1], 'pressure_script_sha256': 'a' * 64}
        validate(self.config, self.application)
        for demand in ({}, {'cpu_ids': [0, 0], 'pressure_script_sha256': 'a' * 64},
                       {'cpu_ids': [0, 1], 'pressure_script_sha256': 'unverified'}):
            with self.assertRaisesRegex(ValueError, 'CPU demand'):
                validate({**self.config, 'cpu_demand': demand}, self.application)

    def test_gpu_workers_models_roles_and_window_cannot_be_silently_changed(self):
        variants = []
        for key, value in [('max_workers', 2), ('device', 'cuda:0'), ('model_sha256', 'c' * 64), ('class', 'guaranteed')]:
            item = copy.deepcopy(self.application); item['nodes'][1][key] = value; variants.append(item)
        item = copy.deepcopy(self.application); item['actor_class'] = 'opportunistic'; variants.append(item)
        for item in variants:
            with self.assertRaises(ValueError): validate(self.config, item)
        with self.assertRaises(ValueError): validate({**self.config, 'wall_seconds': 1201}, self.application)
        with self.assertRaises(ValueError): validate({**self.config, 'cleanup_seconds': 1}, self.application)


class AutomaticDemandTests(unittest.TestCase):
    def setUp(self):
        self.phases = Phases('anchor', 'burst', cpu_demand=True)
        self.ledger = SimpleNamespace(allocations={'a1': allocation()})
        self.accepted = {'game0': {'node_id': 'anchor'}, 'game1': {'node_id': 'burst'}}
        self.pressure = {'identity': {'pid': 41, 'start_ticks': 50, 'boot_id': 'burst-boot'},
                         'demand_started_unix': 1021, 'demand_ended_unix': 1041, 'report_sha256': 'c' * 64}

    def step(self, now, current=None, pressure=None, drain=False, budget=0, telemetry_unix=None, reported_phase=None):
        status = {'tasks': [current or task()], 'nodes': [{'drain': drain, 'report': {'node_id': 'burst',
            'boot_id': 'burst-boot', 'observed_at_unix_ms': (1000 + now if telemetry_unix is None else telemetry_unix) * 1000,
            'allocations': [{'assignment_id': 'a1', 'generation': 1,
                             'phase': self.ledger.allocations['a1']['phase'] if reported_phase is None else reported_phase}],
            'managed_budget': {'cpu_millicores': budget}}}]}
        return self.phases.update(status, self.ledger, self.accepted, now, pressure=pressure, observed_unix=1000 + now)

    def test_automatic_demand_requires_owned_receipt_release_progress_and_fenced_rejoin(self):
        self.assertEqual(self.step(20), 'pressure_ready')
        self.ledger.allocations['a1']['phase'] = 'draining'
        self.assertIsNone(self.step(22))
        self.ledger.allocations['a1']['phase'] = 'released'
        self.accepted['game2'] = {'node_id': 'anchor'}
        self.assertIsNone(self.step(30))
        self.assertEqual(self.phases.phase, 'cpu_demand', 'release/progress is insufficient without pressure cleanup receipt')
        self.assertIsNone(self.step(45, pressure=self.pressure))
        self.assertEqual(self.phases.phase, 'resuming')
        self.assertTrue(self.phases.events[-1]['automatic_cpu_demand'])
        self.ledger.allocations['a2'] = allocation(generation=2)
        self.assertEqual(self.step(50, task(2, 'a2')), 'proxy_stop')
        self.ledger.allocations['a2']['phase'] = 'uncertain'
        self.assertEqual(self.step(75, task(2, 'a2')), 'proxy_start')
        self.ledger.allocations['a2']['phase'] = 'released'
        self.ledger.allocations['a3'] = allocation('released', 3)
        self.assertIsNone(self.step(90, task(3, 'a3', 'completed')))
        self.phases.require_done()

    def test_explicit_drain_cannot_pass_as_automatic_demand(self):
        self.step(20)
        with self.assertRaisesRegex(RuntimeError, 'explicit node drain'):
            self.step(22, drain=True)

    def test_cached_telemetry_cannot_backdate_late_anchor_progress(self):
        self.step(20)
        self.ledger.allocations['a1']['phase'] = 'draining'
        self.step(27)
        self.ledger.allocations['a1']['phase'] = 'released'
        self.accepted['game2'] = {'node_id': 'anchor'}
        with self.assertRaisesRegex(RuntimeError, 'fresh burst telemetry'):
            self.step(60, pressure=self.pressure, telemetry_unix=1027)
        self.assertEqual(self.phases.phase, 'cpu_demand')

    def test_late_progress_or_coordinator_only_draining_cannot_borrow_telemetry_time(self):
        for late in ('progress', 'draining'):
            with self.subTest(late=late):
                self.setUp(); self.step(20)
                if late != 'draining':
                    self.ledger.allocations['a1']['phase'] = 'draining'
                if late != 'progress':
                    self.accepted['game2'] = {'node_id': 'anchor'}
                self.step(27)
                self.ledger.allocations['a1']['phase'] = 'draining' if late == 'draining' else 'released'
                self.accepted['game2'] = {'node_id': 'anchor'}
                self.step(42, telemetry_unix=1039, reported_phase='running' if late == 'draining' else 'released')
                self.ledger.allocations['a1']['phase'] = 'released'
                self.step(45, pressure=self.pressure)
                self.assertEqual(self.phases.phase, 'cpu_demand')

    def test_prepressure_progress_cannot_be_reused_as_new_in_window_progress(self):
        self.step(20)
        self.accepted['game2'] = {'node_id': 'anchor'}
        self.step(20.5)
        self.ledger.allocations['a1']['phase'] = 'draining'
        self.step(27)
        self.ledger.allocations['a1']['phase'] = 'released'
        self.step(30)
        self.step(45, pressure=self.pressure)
        self.assertEqual(self.phases.phase, 'cpu_demand')

    def test_release_without_cpu_draining_or_without_anchor_progress_cannot_pass(self):
        for missing in ('draining', 'progress'):
            with self.subTest(missing=missing):
                self.setUp()
                self.step(20)
                if missing != 'draining':
                    self.ledger.allocations['a1']['phase'] = 'draining'
                    self.step(27)
                self.ledger.allocations['a1']['phase'] = 'released'
                if missing != 'progress':
                    self.accepted['game2'] = {'node_id': 'anchor'}
                self.step(30, pressure=self.pressure)
                self.assertEqual(self.phases.phase, 'cpu_demand')

    def test_pressure_window_mismatch_and_missing_receipt_time_out(self):
        self.step(20)
        self.ledger.allocations['a1']['phase'] = 'draining'
        self.step(27)
        self.ledger.allocations['a1']['phase'] = 'released'
        self.accepted['game2'] = {'node_id': 'anchor'}
        self.step(30, pressure={**self.pressure, 'demand_started_unix': 1100, 'demand_ended_unix': 1120})
        self.assertEqual(self.phases.phase, 'cpu_demand')
        with self.assertRaisesRegex(TimeoutError, 'phase deadline'):
            self.step(141)


class PressureReceiptTests(unittest.TestCase):
    def setUp(self):
        self.owner = {'pid': 40, 'start_ticks': 49, 'boot_id': 'burst-boot'}
        child = {'pid': 41, 'start_ticks': 50, 'boot_id': 'burst-boot'}
        cleanup = lambda identity: {**identity, 'reaped': True, 'exit_code': 0, 'signals': []}
        actual = {'id': 'exp.cpu-demand', 'mode': 'cpu', 'status': 'completed', 'identity': child,
                  'threads_reaped': True, 'managed_registry_member': False, 'demand_started_unix': 1021,
                  'demand_started_monotonic': 10, 'elapsed_seconds': 20,
                  'results': [{'cpu_id': cpu, 'completed_units': 2,
                               'requests': [{'started_monotonic': 10.1, 'finished_monotonic': 10.2},
                                            {'started_monotonic': 29.8, 'finished_monotonic': 29.9}]}
                              for cpu in [0, 1]]}
        self.report = {'status': 'events_finished_review_required', 'manager_registry_member': False,
                      'events': [{'event': {'id': 'exp.cpu-demand', 'mode': 'cpu', 'duration_seconds': 20, 'at_seconds': 0},
                                  'identity': child, 'cleanup': cleanup(child),
                                  'event_report': {'status': 'events_finished_review_required', 'events': [actual]}}]}
        self.contract = {'cpu_ids': [0, 1], 'pressure_script_sha256': 'a' * 64}
        self.request = {'experiment_id': 'exp', 'node_id': 'burst', 'boot_id': 'burst-boot', 'requested_unix': 1020}
        self.started = {'schema_version': 1, 'request_sha256': 'b' * 64, 'experiment_id': 'exp', 'node_id': 'burst',
                        **self.contract, 'identity': self.owner, 'started_unix': 1020.5}
        self.completed = {**self.started, 'completed_unix': 1042, 'cleanup': cleanup(self.owner)}
        self.rehash()

    def rehash(self):
        self.completed['report_json'] = json.dumps(self.report)
        self.completed['report_sha256'] = hashlib.sha256(self.completed['report_json'].encode()).hexdigest()

    def verify(self):
        return pressure_receipt(self.started, self.completed, self.request, 'b' * 64, self.contract, 1043)

    def test_verified_owner_report_preserves_actual_pressure_window(self):
        result = self.verify()
        self.assertAlmostEqual(result['demand_started_unix'], 1021.1)
        self.assertAlmostEqual(result['demand_ended_unix'], 1040.9)
        self.assertEqual(result['owner_identity'], self.owner)

    def test_cleanup_elapsed_cannot_extend_completed_cpu_work_window(self):
        self.report['events'][0]['event_report']['events'][0]['elapsed_seconds'] = 24
        self.completed['completed_unix'] = 1046
        self.rehash()
        result = self.verify()
        self.assertAlmostEqual(result['demand_ended_unix'], 1040.9)

    def test_request_timestamps_must_match_completed_work_and_stay_finite_bounded(self):
        saved = copy.deepcopy(self.report)
        for mutation in ('missing', 'unordered', 'late', 'nan'):
            with self.subTest(mutation=mutation):
                self.report = copy.deepcopy(saved)
                row = self.report['events'][0]['event_report']['events'][0]['results'][0]
                if mutation == 'missing': row.pop('requests')
                if mutation == 'unordered': row['requests'][1]['started_monotonic'] = 9
                if mutation == 'late': row['requests'][1]['finished_monotonic'] = 31
                if mutation == 'nan': row['requests'][1]['finished_monotonic'] = float('nan')
                self.rehash()
                with self.assertRaisesRegex(ValueError, 'CPU pressure'): self.verify()

    def test_run_identity_hash_freshness_and_clean_reaping_are_required(self):
        for key, value in [('experiment_id', 'other'), ('request_sha256', 'c' * 64),
                           ('report_sha256', 'c' * 64), ('cpu_ids', [2, 3]), ('completed_unix', 1100),
                           ('identity', {**self.owner, 'pid': 99}), ('cleanup', {**self.completed['cleanup'], 'reaped': False})]:
            with self.subTest(key=key):
                saved = self.completed[key]; self.completed[key] = value
                with self.assertRaises(ValueError): self.verify()
                self.completed[key] = saved
        self.started['started_unix'] = 1
        with self.assertRaisesRegex(ValueError, 'stale'): self.verify()

    def test_no_work_wrong_mode_or_unreleased_pressure_cannot_pass(self):
        saved = copy.deepcopy(self.report)
        mutations = [lambda: self.report['events'][0]['event'].update(mode='gpu'),
                     lambda: self.report['events'][0]['cleanup'].update(signals=['KILL']),
                     lambda: self.report['events'][0]['event_report']['events'][0]['results'][0].update(completed_units=0),
                     lambda: self.report['events'][0]['event_report']['events'][0].update(threads_reaped=False)]
        for mutate in mutations:
            self.report = copy.deepcopy(saved); mutate(); self.rehash()
            with self.assertRaises(ValueError): self.verify()


if __name__ == '__main__':
    unittest.main()
