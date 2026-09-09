"""Release failures must stop before uploads; tests never contact a registry."""
from __future__ import annotations
import base64
import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

REPOSITORY = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPOSITORY / 'tools'))
import release_manifest as release
import publish_packages as publish


def tar(path, files):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(path, 'w:gz') as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))


class CandidateFixture(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='cedegrid-release-test-')
        self.base = Path(self.directory.name)
        self.root, self.source = self.base / 'packages', self.base / 'source'
        self.root.mkdir(); self.source.mkdir()
        (self.source / 'source.txt').write_text('candidate source\n')
        self.context = contextlib.ExitStack()
        self.context.enter_context(patch.object(release, 'ROOT', self.source))
        self.context.enter_context(patch.object(release, 'source_names', return_value={'source.txt'}))
        self.context.enter_context(patch.object(release, 'current_commit', return_value='a' * 40))
        native = {}
        for target in sorted(release.NATIVE):
            binary = ('native original ' + target).encode()
            executable = 'cedegrid.exe' if target == 'win32-x64' else 'cedegrid'
            name = 'cedegrid-' + target
            native[target] = {'sha256': hashlib.sha256(binary).hexdigest(), 'size': len(binary)}
            tar(self.root / ('npm/' + name + '-0.2.0.tgz'), {
                'package/package.json': json.dumps({'name': name, 'version': '0.2.0'}).encode(),
                'package/bin/' + executable: binary})
            standalone = self.root / ('standalone/cedegrid-0.2.0-' + target)
            if target == 'win32-x64':
                standalone.parent.mkdir(exist_ok=True)
                with zipfile.ZipFile(str(standalone) + '.zip', 'w') as archive:
                    archive.writestr('cedegrid-0.2.0-' + target + '/' + executable, binary)
            else:
                tar(Path(str(standalone) + '.tar.gz'), {'cedegrid-0.2.0-' + target + '/' + executable: binary})
        tar(self.root / 'npm/cedegrid-0.2.0.tgz', {'package/package.json': b'{"name":"cedegrid","version":"0.2.0"}'})
        (self.root / 'pypi').mkdir()
        for name in publish.PYTHON_FILES:
            (self.root / 'pypi' / name).write_bytes(('original ' + name).encode())
        tar(self.root / release.SOURCE_ARCHIVE, {'CedeGrid/source.txt': b'candidate source\n'})
        artifacts = [{'path': name, 'sha256': release.digest(self.root / name), 'size': (self.root / name).stat().st_size}
                     for name in sorted(release.EXPECTED | {release.SOURCE_ARCHIVE})]
        self.manifest = {'version': '0.2.0', 'artifacts': artifacts, 'native_inputs': native,
                         'missing_native_targets': []}
        self.write_manifest()
        source = release.source_snapshot()
        (self.root / 'source-snapshot.json').write_text(json.dumps(source))
        (self.root / 'SHA256SUMS').write_text(''.join(item['sha256'] + '  ' + item['path'] + '\n' for item in artifacts))
        evidence = self.root / 'evidence' / 'qualification.json'
        evidence.parent.mkdir(); evidence.write_text('{"status":"PASS"}\n')
        hashes = sorted({item['sha256'] for item in artifacts} | {item['sha256'] for item in native.values()})
        self.gates = {'target_release': '0.2.0', 'stable_readiness': True,
                      'candidate': {key: source[key] for key in ('source_commit', 'source_sha256')},
                      'required_gates': [{'id': name, 'status': 'PASS', 'source_commit': source['source_commit'],
                                         'source_sha256': source['source_sha256'], 'artifact_sha256': hashes,
                                         'evidence': [{'path': 'evidence/qualification.json', 'sha256': release.digest(evidence)}]}
                                        for name in sorted(release.REQUIRED)]}
        self.gates['candidate']['artifacts'] = copy.deepcopy(artifacts)

    def write_manifest(self):
        (self.root / 'manifest.json').write_text(json.dumps(self.manifest))

    def tearDown(self):
        self.context.close()
        self.directory.cleanup()

    def validate(self):
        return release.validate(self.root, self.gates, require_stable=True)

    def registry(self, npm='new', python='new'):
        context = contextlib.ExitStack()
        context.enter_context(patch.object(publish, 'npm_state', side_effect=lambda path: (path.stem, npm)))
        context.enter_context(patch.object(publish, 'python_state', return_value=python))
        context.enter_context(patch.object(publish, 'require_oidc'))
        uploaded = context.enter_context(patch.object(publish, 'npm_publish'))
        return context, uploaded


class ReleaseGateTests(CandidateFixture):
    def test_complete_coherent_candidate_passes(self):
        self.assertTrue(self.validate()['stable_readiness'])

    def test_missing_gate_rejected(self):
        self.gates['required_gates'].pop()
        with self.assertRaisesRegex(ValueError, 'complete fixed'):
            self.validate()

    def test_artifact_changed_after_manifest_rejected(self):
        (self.root / 'pypi/cedegrid-0.2.0-py3-none-any.whl').write_bytes(b'different bytes')
        with self.assertRaisesRegex(ValueError, 'artifact hash or size'):
            self.validate()

    def test_platform_gate_cannot_substitute_sdk_hash(self):
        gate = next(row for row in self.gates['required_gates'] if row['id'] == 'PLATFORM-LINUX-X64')
        gate['artifact_sha256'] = [release.digest(self.root / 'npm/cedegrid-0.2.0.tgz')]
        with self.assertRaises(ValueError):
            self.validate()

    def test_gate_source_commit_and_hash_must_match_snapshot(self):
        for field in ('source_commit', 'source_sha256'):
            with self.subTest(field=field):
                gates = copy.deepcopy(self.gates)
                gates['required_gates'][0][field] = 'b' * (40 if field == 'source_commit' else 64)
                with self.assertRaises(ValueError):
                    release.validate(self.root, gates, require_stable=True)

    def test_source_snapshot_cannot_omit_a_source_file(self):
        path = self.root / 'source-snapshot.json'
        source = json.loads(path.read_text()); source['files'] = {}
        path.write_text(json.dumps(source))
        with self.assertRaisesRegex(ValueError, 'exact export allowlist'):
            self.validate()

    def test_current_source_bytes_must_match_snapshot(self):
        (self.source / 'source.txt').write_text('changed implementation')
        with self.assertRaisesRegex(ValueError, 'source snapshot file hash'):
            self.validate()

    def test_evidence_is_a_hashed_original_file(self):
        (self.root / 'evidence/qualification.json').write_text('{"status":"FAIL"}')
        with self.assertRaisesRegex(ValueError, 'evidence hash mismatch'):
            self.validate()

    def test_unstructured_evidence_does_not_qualify(self):
        self.gates['required_gates'][0]['evidence'] = ['looks good']
        with self.assertRaisesRegex(ValueError, 'path/sha256'):
            self.validate()

    def test_native_input_hash_must_match_both_archives(self):
        self.manifest['native_inputs']['linux-x64-gnu']['sha256'] = '0' * 64
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'packaged bytes'):
            self.validate()

    def test_checksum_file_must_cover_exact_artifact_list(self):
        (self.root / 'SHA256SUMS').write_text('')
        with self.assertRaisesRegex(ValueError, 'SHA256SUMS'):
            self.validate()

    def test_artifact_symlink_is_rejected(self):
        path = self.root / 'pypi/cedegrid-0.2.0-py3-none-any.whl'
        other = self.base / 'moved-wheel'; path.rename(other); path.symlink_to(other)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            self.validate()

    def test_freeze_writes_exact_sha256sums_before_qualification(self):
        (self.root / 'SHA256SUMS').unlink(); (self.root / 'source-snapshot.json').unlink()
        for gate in self.gates['required_gates']:
            gate.update(status='NOT_RUN', source_sha256=None, source_commit=None, evidence=[], artifact_sha256=[])
        self.gates['stable_readiness'] = False
        gates_path = self.root / 'gates.json'; gates_path.write_text(json.dumps(self.gates))
        with patch.object(sys, 'argv', ['release_manifest', '--packages', str(self.root), '--gates', str(gates_path), '--freeze']), contextlib.redirect_stdout(io.StringIO()):
            release.main()
        release.verify_checksums(self.root, self.manifest['artifacts'])


class PublishPhaseTests(CandidateFixture):
    def test_missing_gate_stops_before_network_or_upload(self):
        self.gates['required_gates'].pop()
        with patch.object(publish, 'npm_state') as registry, patch.object(publish, 'npm_publish') as uploaded:
            with self.assertRaises(ValueError):
                publish.publish_phase(self.root, self.gates, 'native', apply=True)
            registry.assert_not_called(); uploaded.assert_not_called()

    def test_late_npm_collision_stops_before_any_upload(self):
        def state(path):
            if 'win32' in path.name:
                raise ValueError('same version different registry bytes')
            return path.stem, 'new'
        context, uploaded = self.registry()
        with context, patch.object(publish, 'npm_state', side_effect=state):
            with self.assertRaisesRegex(ValueError, 'different registry bytes'):
                publish.publish_phase(self.root, self.gates, 'native', apply=True)
            uploaded.assert_not_called()

    def test_late_pypi_collision_stops_before_native_upload(self):
        context, uploaded = self.registry()
        with context, patch.object(publish, 'python_state', side_effect=ValueError('same filename different bytes')):
            with self.assertRaises(ValueError):
                publish.publish_phase(self.root, self.gates, 'native', apply=True)
            uploaded.assert_not_called()

    def test_identical_retry_performs_no_upload(self):
        context, uploaded = self.registry('identical', 'identical')
        with context:
            result = publish.publish_phase(self.root, self.gates, 'native', apply=True)
            uploaded.assert_not_called()
            self.assertTrue(all(item['registry_state'] == 'identical' for item in result['actions']))

    def test_python_requires_all_native_packages_first(self):
        context, uploaded = self.registry()
        stage = self.base / 'stage'
        with context:
            with self.assertRaisesRegex(ValueError, 'native npm package'):
                publish.publish_phase(self.root, self.gates, 'python-stage', python_stage=stage)
            self.assertFalse(stage.exists()); uploaded.assert_not_called()

    def test_python_stages_only_missing_original_file(self):
        context, uploaded = self.registry('identical')
        stage = self.base / 'stage'
        with context, patch.object(publish, 'python_state', side_effect=lambda path: 'new' if path.suffix == '.whl' else 'identical'):
            publish.publish_phase(self.root, self.gates, 'python-stage', python_stage=stage)
            uploaded.assert_not_called()
        self.assertEqual([path.name for path in stage.iterdir()], ['cedegrid-0.2.0-py3-none-any.whl'])
        self.assertEqual(release.digest(stage / 'cedegrid-0.2.0-py3-none-any.whl'), release.digest(self.root / 'pypi/cedegrid-0.2.0-py3-none-any.whl'))

    def test_existing_staging_directory_is_rejected(self):
        stage = self.base / 'stage'; stage.mkdir(); (stage / 'foreign.whl').write_bytes(b'foreign')
        with patch.object(publish, 'npm_state') as registry:
            with self.assertRaisesRegex(ValueError, 'new directory'):
                publish.publish_phase(self.root, self.gates, 'python-stage', python_stage=stage)
            registry.assert_not_called()

    def test_main_requires_both_original_python_files(self):
        context, uploaded = self.registry('identical', 'new')
        with context:
            with self.assertRaisesRegex(ValueError, 'Python artifact'):
                publish.publish_phase(self.root, self.gates, 'main', apply=True)
            uploaded.assert_not_called()

    def test_local_mutation_during_preflight_stops_before_upload(self):
        def state(path):
            if path.suffix == '.whl':
                (self.root / 'npm/cedegrid-0.2.0.tgz').write_bytes(b'changed during registry checks')
            return 'new'
        context, uploaded = self.registry()
        with context, patch.object(publish, 'python_state', side_effect=state):
            with self.assertRaisesRegex(ValueError, 'artifact hash or size'):
                publish.publish_phase(self.root, self.gates, 'native', apply=True)
            uploaded.assert_not_called()

    def test_apply_requires_oidc_before_network(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(publish, 'npm_state') as registry:
            with self.assertRaisesRegex(ValueError, 'GitHub-hosted OIDC'):
                publish.publish_phase(self.root, self.gates, 'native', apply=True)
            registry.assert_not_called()

    def test_fetched_npm_bytes_must_match_even_when_metadata_does(self):
        path = self.root / 'npm/cedegrid-0.2.0.tgz'
        integrity = 'sha512-' + base64.b64encode(hashlib.sha512(path.read_bytes()).digest()).decode()
        metadata = {'name': 'cedegrid', 'version': '0.2.0', 'dist': {'integrity': integrity,
                    'tarball': 'https://registry.npmjs.org/cedegrid/-/cedegrid-0.2.0.tgz'}}
        with patch.object(publish, 'fetch', return_value=metadata), patch.object(publish, 'fetch_hashes', return_value=(path.stat().st_size, '0' * 64, integrity)):
            with self.assertRaisesRegex(ValueError, 'fetched npm tarball'):
                publish.npm_state(path)

    def test_same_pypi_filename_with_other_digest_is_rejected(self):
        path = self.root / 'pypi/cedegrid-0.2.0-py3-none-any.whl'
        metadata = {'info': {'name': 'cedegrid', 'version': '0.2.0'},
                    'urls': [{'filename': path.name, 'digests': {'sha256': '0' * 64}}]}
        with patch.object(publish, 'fetch', return_value=metadata):
            with self.assertRaisesRegex(ValueError, 'different bytes'):
                publish.python_state(path)

    def test_publish_command_uses_original_path_without_registry_tokens(self):
        path = self.root / 'npm/cedegrid-0.2.0.tgz'
        with patch.dict(os.environ, {'NODE_AUTH_TOKEN': 'unused-fixture', 'NPM_TOKEN': 'unused-fixture', 'NPM_CONFIG_REGISTRY': 'https://example.invalid'}), patch.object(publish.subprocess, 'run') as call:
            publish.npm_publish(path)
            argv = call.call_args.args[0]; environment = call.call_args.kwargs['env']
            self.assertEqual(argv[:3], ['npm', 'publish', str(path)])
            self.assertIn('--ignore-scripts', argv); self.assertIn('--provenance', argv)
            self.assertNotIn('NODE_AUTH_TOKEN', environment); self.assertNotIn('NPM_TOKEN', environment)
            self.assertFalse(any(key.upper().startswith('NPM_CONFIG_') for key in environment))


class WorkflowTests(unittest.TestCase):
    def test_release_and_candidate_actions_are_immutable(self):
        for name in ('release.yml', 'release-candidate.yml'):
            source = (REPOSITORY / '.github/workflows' / name).read_text()
            uses = re.findall(r'uses:\s+([^\s#]+)', source)
            self.assertTrue(uses)
            self.assertTrue(all(re.search(r'@[0-9a-f]{40}$', item) for item in uses), uses)

    def test_publish_workflow_does_not_rebuild_or_repack(self):
        source = (REPOSITORY / '.github/workflows/release.yml').read_text()
        for forbidden in ('npm run build', 'npm pack', 'uv build', 'build_packages.py', 'build_native.py'):
            self.assertNotRegex(source, r'\b' + re.escape(forbidden) + r'\b')
        self.assertIn('artifact-ids:', source)
        self.assertIn('id-token: write', source)
        self.assertNotIn('secrets.NPM', source)


if __name__ == '__main__':
    unittest.main()
