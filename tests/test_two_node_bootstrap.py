"""Preparation and ownership boundaries; no service/container or workload launch."""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import two_node_bootstrap as bootstrap


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='.resmgr-bootstrap-test-', dir=Path.home())
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / 'source/Kaggriculture'
        manager = self.root / 'source/ResourceManager'
        for path, contents in {
            manager / 'python/resmgr/__init__.py': '',
            self.source / 'training/self_play.py': '',
            self.source / 'integration/resmgr/workflow.py': '',
            self.source / 'integration/resmgr/worker.py': '''from pathlib import Path
from resmgr import sha256_file
import json
def verify_snapshot(candidate, expected, model):
    root = candidate.parent
    for item in json.loads((root/'snapshot_manifest.json').read_text())['files']:
        path=(root/item['path']).resolve()
        if not path.is_relative_to(root) or sha256_file(path)!=item['sha256']:
            raise ValueError('snapshot integrity failure')
''',
            self.root / 'bootstrap.pt': 'existing checkpoint',
            self.root / 'dataset/manifest.json': '{}',
            self.source / 'opponents/baseline.py': 'existing opponent',
            self.root / 'snapshot/main.py': 'existing candidate',
            self.root / 'snapshot/weights/policy_value.ts': 'existing model',
        }.items():
            path.parent.mkdir(parents=True, exist_ok=True); path.write_text(contents)
        files = [{'path': name, 'sha256': bootstrap.sha256_file(self.root / 'snapshot' / name)}
                 for name in ('main.py', 'weights/policy_value.ts')]
        (self.root / 'snapshot/snapshot_manifest.json').write_text(json.dumps({'files': files}))
        league = self.source / 'config/league_bases_r004.json'; league.parent.mkdir()
        league.write_text(json.dumps({'opponents': [{'name': 'existing', 'path': '../opponents/baseline.py',
            'weight': 1, 'sha256': bootstrap.sha256_file(self.source / 'opponents/baseline.py')}]}))
        for name in ('python', 'resmgr'):
            path = self.root / name; path.write_text('test executable never invoked'); path.chmod(0o700)
        self.args = argparse.Namespace(root=str(self.root), run_id='private-run', anchor_node_id='anchor-stable',
            burst_node_id='burst-stable', anchor_cpus='0,1', python=str(self.root / 'python'), binary=str(self.root / 'resmgr'),
            candidate=str(self.root / 'snapshot/main.py'), bootstrap=str(self.root / 'bootstrap.pt'),
            dataset=str(self.root / 'dataset'), docker_root='/home/resmgr/validation')
        def fake_pki(output, hosts, nodes):
            output.mkdir()
            for name in ('ca.pem', 'server.pem', 'node-1.pem', 'node-1.key'):
                (output / name).write_text(name)
            self.sans, self.identities = hosts, nodes
            return {'clients': {'0' * 64: {'role': 'operator'}, 'a' * 64: {'role': 'node', 'node_id': nodes[0]},
                                'b' * 64: {'role': 'node', 'node_id': nodes[1]}}}
        self.pki = patch.object(bootstrap, 'generate_pki', side_effect=fake_pki).start()
        self.addCleanup(patch.stopall)
        patch.object(bootstrap, 'cpu_list', return_value=[0, 1]).start()

    def physical_burst(self):
        home = '/home/USER'
        self.args.burst_home = home
        self.args.burst_root = home + '/validation/frozen-source-identifier'
        self.args.burst_python = home + '/environments/isolated/bin/python'
        self.args.burst_binary = home + '/binaries/frozen/resmgr'
        self.args.burst_source_root = home + '/bundles/source/Kaggriculture'
        self.args.burst_sdk_root = home + '/bundles/source/ResourceManager/python'
        self.args.burst_cpus = '22,54'
        self.args.burst_storage_profile = 'delete_extra'
        self.args.burst_endpoint = 'https://127.0.0.1:45673'
        return home

    def test_preparation_preserves_identity_assets_and_credential_scope(self):
        with patch.object(bootstrap.subprocess, 'Popen', side_effect=AssertionError('no services during preparation')):
            result = bootstrap.prepare(self.args)
        output = Path(result['services_output'])
        application = json.loads((output / 'application.json').read_text())
        anchor, burst = application['nodes']
        self.assertEqual([anchor['node_id'], burst['node_id']], ['anchor-stable', 'burst-stable'])
        self.assertEqual([anchor['class'], burst['class']], ['guaranteed', 'opportunistic'])
        self.assertNotIn('actor_class', application)
        self.assertEqual(application['games'], 12)
        self.assertEqual(anchor['model_sha256'], burst['model_sha256'])
        self.assertEqual(anchor['snapshot_sha256'], burst['snapshot_sha256'])
        self.assertEqual(burst['candidate'], '/home/resmgr/validation/runs/private-run/bootstrap/snapshot/main.py')
        self.assertEqual({Path(item['source']).name for item in result['burst_transfer'] if '/pki/' in item['source']},
                         {'ca.pem', 'node-1.pem', 'node-1.key'})
        self.assertIn('DNS:host.docker.internal', self.sans)
        self.assertIn('IP:127.0.0.1', self.sans)
        self.assertEqual(output.stat().st_mode & 0o777, 0o700)
        self.assertFalse(Path(result['harness_output']).exists())
        node = json.loads((output / 'burst-node.json').read_text())
        agent = json.loads((output / 'burst-agent.json').read_text())
        self.assertEqual(node['ram']['reserve_mib'], 1024)
        self.assertEqual(node['cpu']['reserve_physical_cores'], 0)
        self.assertEqual(json.loads((output / 'anchor-node.json').read_text())['cpu']['reserve_physical_cores'], 1)
        self.assertEqual(agent['max_workers'], 1)
        self.assertEqual(agent['capacity']['gpu_memory_mib'], {})
        self.assertEqual(agent['capacity']['cpu_millicores'], 1000)
        self.assertEqual(agent['capacity']['ram_mib'], 2048)
        self.assertEqual(json.loads((output / 'anchor-agent.json').read_text())['capacity']['ram_mib'], 4096)
        self.assertEqual(agent['coordinator_url'], 'https://host.docker.internal:45672')
        self.assertEqual(agent['max_runtime_seconds'], 1400)
        self.assertEqual(result['docker_bootstrap'], result['burst_bootstrap'])
        self.assertEqual(result['burst_placement']['kind'], 'docker')
        self.assertEqual(result['burst_argv'][0], '/home/resmgr/validation/source/ResourceManager/target/release/resmgr')
        for name in ('anchor-node', 'burst-node', 'coordinator'):
            self.assertNotIn('storage_profile', json.loads((output / (name + '.json')).read_text()))
        self.assertEqual(result['storage_profiles'], dict(anchor='wal_full', burst='wal_full', coordinator='wal_full'))

    def test_physical_preparation_wires_explicit_paths_cpu_profile_and_existing_workflow(self):
        home = self.physical_burst()
        with patch.object(bootstrap.subprocess, 'Popen', side_effect=AssertionError('no service launch')):
            result = bootstrap.prepare(self.args)
        output = Path(result['services_output'])
        read = lambda name: json.loads((output / (name + '.json')).read_text())
        anchor, burst = read('application')['nodes']
        self.assertEqual([anchor['node_id'], burst['node_id']], ['anchor-stable', 'burst-stable'])
        self.assertEqual(result['burst_placement']['home'], home)
        self.assertEqual(result['burst_placement']['kind'], 'physical')
        for key in ('python', 'source_root', 'sdk_root'):
            self.assertEqual(burst[key], getattr(self.args, 'burst_' + key))
        self.assertEqual(burst['opponents'][0]['path'], self.args.burst_source_root + '/opponents/baseline.py')
        self.assertEqual(burst['snapshot_sha256'], anchor['snapshot_sha256'])
        self.assertEqual(burst['candidate'], result['burst_bootstrap'] + '/snapshot/main.py')
        self.assertEqual(read('burst-node')['state_dir'], result['burst_bootstrap'] + '/agent-state')
        self.assertEqual(read('burst-node')['storage_profile'], 'delete_extra')
        self.assertEqual(read('burst-node')['ram']['reserve_mib'], 16384)
        self.assertEqual(read('burst-node')['cpu']['reserve_physical_cores'], 0)
        self.assertEqual(read('anchor-node').get('storage_profile', 'wal_full'), 'wal_full')
        self.assertEqual(read('coordinator').get('storage_profile', 'wal_full'), 'wal_full')
        self.assertEqual(read('burst-agent')['cpu_affinity'], [22, 54])
        self.assertEqual(read('burst-agent')['coordinator_url'], self.args.burst_endpoint)
        self.assertEqual(read('burst-agent')['tls']['private_key'], result['burst_bootstrap'] + '/pki/node-1.key')
        self.assertEqual(result['burst_argv'], [self.args.burst_binary, '--config', result['burst_bootstrap'] + '/burst-node.json',
                                             'agent', '--deployment', result['burst_bootstrap'] + '/burst-agent.json'])
        self.assertEqual(result['burst_env']['TMPDIR'], result['burst_bootstrap'])
        self.assertNotIn('HOME', result['burst_env'])
        self.assertEqual(result['transport']['owned_proxy_listen'], ['127.0.0.1', 45670])
        self.assertIsNone(result['transport']['mac_tunnel_loopback_port'])
        self.assertEqual(read('harness')['proxy_config']['target'], ['127.0.0.1', 45671])
        self.assertEqual(read('harness')['application_config'], str(output / 'application.json'))
        self.assertNotIn('container', result['burst_cpu_policy'])
        self.assertIn('No enforced CPU quota', result['burst_cpu_policy'])
        for item in result['burst_transfer']:
            self.assertTrue(Path(item['destination']).is_relative_to(home))
            self.assertEqual(item['sha256'], bootstrap.sha256_file(Path(item['source'])))
            self.assertNotIn('/home/resmgr', item['destination'])
        self.assertEqual({Path(item['source']).name for item in result['burst_transfer'] if '/pki/' in item['source']},
                         {'ca.pem', 'node-1.pem', 'node-1.key'})

    def test_anchor_assets_and_profiles_can_use_existing_separate_home_paths(self):
        self.args.source_root = str(self.root / 'existing-application')
        self.source.rename(self.args.source_root)
        self.args.sdk_root = str(self.root / 'existing-sdk')
        (self.root / 'source/ResourceManager/python').rename(self.args.sdk_root)
        self.args.anchor_storage_profile = 'delete_extra'
        self.args.coordinator_storage_profile = 'delete_extra'
        result = bootstrap.prepare(self.args)
        output = Path(result['services_output'])
        application = json.loads((output / 'application.json').read_text())
        self.assertEqual(application['nodes'][0]['sdk_root'], self.args.sdk_root)
        self.assertEqual(application['nodes'][0]['source_root'], self.args.source_root)
        self.assertEqual(result['sdk_root'], self.args.sdk_root)
        for name in ('anchor-node', 'coordinator'):
            self.assertEqual(json.loads((output / (name + '.json')).read_text())['storage_profile'], 'delete_extra')
        self.assertEqual(json.loads((output / 'burst-node.json').read_text()).get('storage_profile', 'wal_full'), 'wal_full')

    def test_physical_mode_requires_complete_explicit_configuration_before_credentials(self):
        self.physical_burst()
        for name in ('home', 'root', 'python', 'binary', 'source_root', 'sdk_root', 'cpus', 'endpoint'):
            with self.subTest(name=name):
                field = 'burst_' + name; value = getattr(self.args, field)
                setattr(self.args, field, None)
                with self.assertRaises(ValueError): bootstrap.prepare(self.args)
                setattr(self.args, field, value)
        self.args.docker_root = '/home/resmgr/custom'
        with self.assertRaisesRegex(ValueError, 'choose'): bootstrap.prepare(self.args)
        self.pki.assert_not_called()
        self.assertFalse((self.root / 'evidence').exists())

    def test_each_physical_path_stays_in_declared_home(self):
        self.physical_burst()
        for name in ('root', 'python', 'binary', 'source_root', 'sdk_root'):
            with self.subTest(name=name):
                field = 'burst_' + name; value = getattr(self.args, field)
                setattr(self.args, field, '/home/user/asset')
                with self.assertRaisesRegex(ValueError, 'runtime home'): bootstrap.prepare(self.args)
                setattr(self.args, field, value)
        for root, parts, home in (('/home/test/run', ('/home/test/other',), '/home/test'),
                                  ('/home/test/run', ('../outside',), '/home/test'),
                                  ('/home/test' + 'suffix/run', (), '/home/test'),
                                  ('/home/test/run', (), '/'),
                                  ('/home/test/run', (), '/home/test/../b'),
                                  ('/home/test/run\nother', (), '/home/test')):
            with self.subTest(root=root, parts=parts, home=home):
                with self.assertRaises(ValueError): bootstrap.remote_path(root, *parts, home=home)
        self.pki.assert_not_called()

    def test_remote_cpu_ids_are_validated_without_anchor_affinity(self):
        self.physical_burst()
        with patch.object(bootstrap.os, 'sched_getaffinity', return_value={0, 1}, create=True) as affinity:
            self.assertEqual(bootstrap.burst_settings(self.args)['cpus'], [22, 54])
            affinity.assert_not_called()
        for cpus in ('0', '1,1', '-1,0', '0,1,2,3,4,5,6', '0,x', ''):
            with self.subTest(cpus=cpus):
                self.args.burst_cpus = cpus
                with self.assertRaises(ValueError): bootstrap.prepare(self.args)
        self.pki.assert_not_called()

    def test_unknown_storage_profiles_fail_before_credentials(self):
        for field in ('anchor_storage_profile', 'burst_storage_profile', 'coordinator_storage_profile'):
            with self.subTest(field=field):
                setattr(self.args, field, 'unsafe')
                with self.assertRaisesRegex(ValueError, 'storage profile'): bootstrap.prepare(self.args)
                delattr(self.args, field)
        self.pki.assert_not_called()

    def test_explicit_endpoint_preserves_loopback_listener_and_mtls_identity(self):
        self.args.burst_endpoint = 'https://existing-route.example:45673/'
        result = bootstrap.prepare(self.args)
        output = Path(result['services_output'])
        agent = json.loads((output / 'burst-agent.json').read_text())
        self.assertEqual(agent['coordinator_url'], 'https://existing-route.example:45673')
        self.assertEqual(result['transport']['tls_name'], 'existing-route.example')
        self.assertEqual(result['transport']['owned_proxy_listen'], ['127.0.0.1', 45670])
        self.assertIn('DNS:existing-route.example', self.sans)
        self.assertEqual(bootstrap.authenticated_endpoint('https://[::1]:45673'), ('https://[::1]:45673', '::1', 'IP:::1'))
        self.assertEqual(self.identities, ['anchor-stable', 'burst-stable'])

    def test_invalid_endpoint_never_creates_credentials(self):
        for endpoint in ('http://localhost:45673', 'https://operator:secret@localhost:45673',
                         'https://localhost:0', 'https://localhost:65536', 'https://localhost:45673/rpc',
                         'https://localhost:45673?target=other', 'https://localhost/#fragment',
                         'https://0.0.0.0:45673', 'https://localhost\nmalformed', 'https://a,b:45673'):
            with self.subTest(endpoint=endpoint):
                self.args.burst_endpoint = endpoint
                with self.assertRaises(ValueError): bootstrap.prepare(self.args)
        self.pki.assert_not_called()
        self.assertFalse((self.root / 'evidence').exists())

    def test_cli_exposes_physical_configuration_and_preserves_docker_defaults(self):
        required = ['prepare']
        for field in ('root', 'run_id', 'python', 'binary', 'candidate', 'bootstrap', 'dataset',
                      'anchor_cpus', 'anchor_node_id', 'burst_node_id'):
            required.extend(['--' + field.replace('_', '-'), getattr(self.args, field)])
        defaults = bootstrap.argument_parser().parse_args(required)
        self.assertEqual(defaults.docker_root, '/home/resmgr/validation')
        self.assertEqual(defaults.burst_storage_profile, 'wal_full')
        self.assertIsNone(defaults.burst_home)
        self.physical_burst()
        explicit = list(required)
        for field in ('home', 'root', 'python', 'binary', 'source_root', 'sdk_root', 'cpus', 'endpoint', 'storage_profile'):
            explicit.extend(['--burst-' + field.replace('_', '-'), getattr(self.args, 'burst_' + field)])
        args = bootstrap.argument_parser().parse_args(explicit)
        self.assertEqual(bootstrap.burst_settings(args)['cpus'], [22, 54])
        self.assertEqual(args.burst_endpoint, self.args.burst_endpoint)

    def test_missing_or_changed_snapshot_fails_before_creating_credentials(self):
        (self.root / 'snapshot/weights/policy_value.ts').write_text('changed')
        with self.assertRaisesRegex(ValueError, 'snapshot integrity'):
            bootstrap.prepare(self.args)
        self.pki.assert_not_called()
        self.assertFalse((self.root / 'evidence').exists())

    def test_public_transport_is_explicit_pinned_and_source_restricted(self):
        self.args.proxy_listen_host = '192.0.2.10'
        with self.assertRaisesRegex(ValueError, 'both'):
            bootstrap.prepare(self.args)
        self.pki.assert_not_called()
        self.args.burst_source_ip = '198.51.100.20'
        result = bootstrap.prepare(self.args)
        output = Path(result['services_output'])
        agent = json.loads((output / 'burst-agent.json').read_text())
        harness = json.loads((output / 'harness.json').read_text())
        self.assertEqual(agent['coordinator_url'], 'https://192.0.2.10:45670')
        self.assertIn('IP:192.0.2.10', self.sans)
        proxy = harness['proxy_config']
        self.assertEqual(proxy['public_listener']['source_ips'], ['198.51.100.20'])
        self.assertEqual(proxy['target'], ['127.0.0.1', 45671])
        self.assertEqual(proxy['max_forward_bytes'], 2 * 1024**3)
        self.assertEqual(proxy['public_listener']['coordinator_sha256'], bootstrap.sha256_file(output / 'coordinator.json'))
        self.assertIsNone(result['transport']['mac_tunnel_loopback_port'])

    def test_missing_checkpoint_fails_without_any_deployment(self):
        (self.root / 'bootstrap.pt').unlink()
        with self.assertRaisesRegex(ValueError, 'absent'):
            bootstrap.prepare(self.args)
        self.pki.assert_not_called()

    def test_existing_output_is_never_reused_or_overwritten(self):
        bootstrap.prepare(self.args)
        with self.assertRaisesRegex(ValueError, 'fresh'):
            bootstrap.prepare(self.args)
        self.assertEqual(self.pki.call_count, 1)

    def test_home_boundary_and_explicit_distinct_ids(self):
        for value in ('/tmp/validation', '/home/resmgr/../root/validation', '/home/resmgr/validation/../../root'):
            with self.assertRaises(ValueError): bootstrap.remote_path(value)
        self.args.burst_node_id = self.args.anchor_node_id
        with self.assertRaisesRegex(ValueError, 'distinct'):
            bootstrap.prepare(self.args)
        self.pki.assert_not_called()

    def test_fault_cleanup_selects_owned_job_identity_not_name_substring(self):
        status = {'jobs': [{'job_id': name} for name in ('exp.selfplay', 'exp.learner', 'experiment.other', 'other.exp') ]}
        self.assertEqual(bootstrap.owned_jobs(status, 'exp'), ['exp.selfplay', 'exp.learner'])
        with self.assertRaisesRegex(ValueError, 'execution approval'):
            bootstrap.run(self.root / 'absent.json', execute=False)

    def test_transport_startup_failure_does_not_invent_missing_node_cleanup_error(self):
        prepared = bootstrap.prepare(self.args)
        # A previously prepared manifest has no additive native placement fields.
        for field in ('sdk_root', 'burst_placement', 'burst_env', 'burst_preflight_required', 'burst_bootstrap', 'storage_profiles'):
            prepared.pop(field)
        bootstrap.atomic_json(Path(prepared['services_output']) / 'manifest.json', prepared)
        result = self.transport_startup_failure(prepared)
        self.assertIn('docker_cleanup', result)

    def test_physical_runtime_preserves_sdk_path_and_reports_owned_burst_cleanup(self):
        self.physical_burst()
        self.args.sdk_root = str(self.root / 'existing-sdk')
        (self.root / 'source/ResourceManager/python').rename(self.args.sdk_root)
        prepared = bootstrap.prepare(self.args)
        result = self.transport_startup_failure(prepared)
        self.assertNotIn('docker_cleanup', result)
        self.assertIn('stop/reap its owned service', result['burst_cleanup'])

    def transport_startup_failure(self, prepared):
        output = Path(prepared['services_output']); (output / 'START').touch()
        harness = Path(prepared['harness_output']); harness.mkdir()
        (harness / 'report.json').write_text(json.dumps({'status': 'failed', 'error': 'transport registration timeout'}))
        status = {'nodes': [{'report': {'node_id': self.args.anchor_node_id}}], 'tasks': [], 'jobs': [], 'allocations': []}
        drained = []; owned = []
        def drain(node):
            if node != self.args.anchor_node_id: raise RuntimeError('unknown node')
            drained.append(node)
        client = SimpleNamespace(status=lambda: status, cancel=lambda _: self.fail('no jobs to cancel'), drain_node=drain)
        def process(argv, cwd, log, env):
            expected_sdk = getattr(self.args, 'sdk_root', str(self.root / 'source/ResourceManager/python'))
            self.assertEqual(env['PYTHONPATH'], os.pathsep.join([expected_sdk, str(self.source)]))
            stage = Path(log).stem; owned.append(stage)
            code = 1 if stage == 'harness' else None
            return SimpleNamespace(identity={'pid': 100 + len(owned)}, child=SimpleNamespace(returncode=code),
                poll=lambda: code, stop=lambda **_: {'reaped': True, 'exit_code': code or 0, 'signals': []})
        def read_only_command(argv, **kwargs):
            return SimpleNamespace(stdout='[]' if argv[-1] == 'executions' else '{}')
        with patch.object(bootstrap.sys, 'platform', 'linux'), \
             patch.object(bootstrap.Client, 'from_config', return_value=client), \
             patch.object(bootstrap, 'OwnedProcess', side_effect=process), \
             patch.object(bootstrap.subprocess, 'run', side_effect=read_only_command), \
             patch.object(bootstrap, 'local_guard', return_value={'errors': []}):
            result = bootstrap.run(output / 'manifest.json', execute=True)
        self.assertEqual(result['status'], 'failed')
        self.assertEqual(result['harness_status'], 'failed')
        self.assertEqual(drained, [self.args.anchor_node_id])
        self.assertEqual(result['node_cleanup']['configured_not_registered'], [self.args.burst_node_id])
        self.assertEqual(result['unresolved_allocations'], [])
        self.assertEqual(owned, ['coordinator', 'anchor', 'harness'])
        self.assertTrue(all(row['reaped'] and 'error' not in row for row in result['cleanup']))
        return result

    def test_missing_node_cleanup_retains_unknown_allocation(self):
        ledger = bootstrap.AllocationLedger(['anchor', 'burst'])
        ledger.observe({'allocations': [{'assignment_id': 'attempt', 'node_id': 'burst', 'phase': 'uncertain'}]})
        status = {'nodes': [{'report': {'node_id': 'anchor'}}], 'allocations': []}
        ledger.observe(status)
        drained = []
        client = SimpleNamespace(cancel=lambda _: self.fail('no jobs'), drain_node=drained.append)
        bootstrap.request_owned_cleanup(client, status, 'exp', ledger.nodes)
        self.assertEqual(drained, ['anchor'])
        self.assertEqual(len(ledger.unresolved()), 1)
        self.assertEqual(ledger.unresolved()[0]['phase'], 'uncertain')
        self.assertFalse(ledger.unresolved()[0]['present_in_latest_status'])


if __name__ == '__main__':
    unittest.main()
