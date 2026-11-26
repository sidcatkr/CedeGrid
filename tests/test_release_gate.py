"""Release failures must stop before uploads; tests never contact a registry."""
from __future__ import annotations
import contextlib
import copy
import hashlib
import io
import json
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
import release_version

V = release.VERSION


def tar(path, files):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(path, 'w:gz') as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))


def deb(path, files):
    """A Debian package: an ar archive holding debian-binary, control and an xz data archive."""
    def xz(members):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w:xz') as archive:
            for name, data in members.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        return buffer.getvalue()
    parts = [('debian-binary', b'2.0\n'), ('control.tar.xz', xz({'./control': b'Package: x\n'})),
             ('data.tar.xz', xz(files))]
    out = bytearray(b'!<arch>\n')
    for name, body in parts:
        out += (name + '/').ljust(16).encode() + b'0'.ljust(12) + b'0'.ljust(6) + b'0'.ljust(6)
        out += b'100644'.ljust(8) + str(len(body)).ljust(10).encode() + b'`\n' + body
        if len(body) % 2:
            out += b'\n'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))


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
            native[target] = {'sha256': hashlib.sha256(binary).hexdigest(), 'size': len(binary)}
            standalone = self.root / release.standalone(target)
            member = 'cedegrid-' + V + '-' + target + '/' + executable
            if target == 'win32-x64':
                standalone.parent.mkdir(exist_ok=True)
                with zipfile.ZipFile(standalone, 'w') as archive:
                    archive.writestr(member, binary)
            else:
                tar(standalone, {member: binary})
            if target in release.DEB_ARCH:
                arch = release.DEB_ARCH[target]
                deb(self.root / ('deb/cedegrid_' + V + '_' + arch + '.deb'), {'./usr/bin/cedegrid': binary})
                deb(self.root / ('deb/cedegrid-system_' + V + '_' + arch + '.deb'),
                    {'./lib/systemd/system/cedegrid-broker.service': b'[Unit]\n'})
        tar(self.root / ('npm/cedegrid-' + V + '.tgz'),
            {'package/package.json': json.dumps({'name': 'cedegrid', 'version': V}).encode()})
        (self.root / 'pypi').mkdir()
        for name in release.sdk_files():
            if name.startswith('pypi/'):
                (self.root / name).write_bytes(('original ' + name).encode())
        (self.root / 'install').mkdir()
        (self.root / release.INSTALL_SCRIPT).write_text('#!/bin/sh\nversion=${CEDEGRID_VERSION:-' + V + '}\n')
        tar(self.root / release.source_archive(), {'CedeGrid/source.txt': b'candidate source\n'})
        artifacts = [{'path': name, 'sha256': release.digest(self.root / name), 'size': (self.root / name).stat().st_size}
                     for name in sorted(release.expected() | {release.source_archive()})]
        self.manifest = {'version': V, 'artifacts': artifacts, 'native_inputs': native,
                         'missing_native_targets': []}
        self.write_manifest()
        source = release.source_snapshot()
        (self.root / 'source-snapshot.json').write_text(json.dumps(source))
        (self.root / 'SHA256SUMS').write_text(release.checksums(artifacts))
        evidence = self.root / 'evidence' / 'qualification.json'
        evidence.parent.mkdir(); evidence.write_text('{"status":"PASS"}\n')
        hashes = sorted({item['sha256'] for item in artifacts} | {item['sha256'] for item in native.values()})
        self.gates = {'target_release': V, 'stable_readiness': True,
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

    def gate(self, gate_id):
        return next(row for row in self.gates['required_gates'] if row['id'] == gate_id)


class ReleaseGateTests(CandidateFixture):
    def test_complete_coherent_candidate_passes(self):
        self.assertTrue(self.validate()['stable_readiness'])

    def test_missing_gate_rejected(self):
        self.gates['required_gates'].pop()
        with self.assertRaisesRegex(ValueError, 'complete fixed'):
            self.validate()

    def test_artifact_changed_after_manifest_rejected(self):
        (self.root / ('pypi/cedegrid-' + V + '-py3-none-any.whl')).write_bytes(b'different bytes')
        with self.assertRaisesRegex(ValueError, 'artifact hash or size'):
            self.validate()

    def test_platform_gate_cannot_substitute_sdk_hash(self):
        gate = self.gate('PLATFORM-LINUX-X64')
        gate['artifact_sha256'] = [release.digest(self.root / ('npm/cedegrid-' + V + '.tgz'))]
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

    def test_native_input_hash_must_match_its_archive(self):
        self.manifest['native_inputs']['linux-x64-gnu']['sha256'] = '0' * 64
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'packaged bytes'):
            self.validate()

    def test_a_debian_package_must_hold_the_qualified_binary(self):
        deb(self.root / ('deb/cedegrid_' + V + '_amd64.deb'), {'./usr/bin/cedegrid': b'another binary'})
        for item in self.manifest['artifacts']:
            if item['path'] == 'deb/cedegrid_' + V + '_amd64.deb':
                path = self.root / item['path']
                item['sha256'], item['size'] = release.digest(path), path.stat().st_size
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'Debian member|native input hash'):
            self.validate()

    def test_native_member_hashing_works_without_python_3_11_file_digest(self):
        python_3_10_hashlib = type('hashlib', (), {'sha256': staticmethod(hashlib.sha256)})
        with patch.object(release, 'hashlib', python_3_10_hashlib):
            self.assertTrue(self.validate()['stable_readiness'])

    def test_checksum_file_must_cover_exact_artifact_list(self):
        (self.root / 'SHA256SUMS').write_text('')
        with self.assertRaisesRegex(ValueError, 'SHA256SUMS'):
            self.validate()

    def test_artifact_symlink_is_rejected(self):
        path = self.root / ('pypi/cedegrid-' + V + '-py3-none-any.whl')
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


class ReleaseFileTests(CandidateFixture):
    """0.3 ships files on GitHub Releases only (003 FR-073)."""

    def test_the_registry_gates_are_gone_and_the_file_gates_are_required(self):
        self.assertFalse({'PACKAGES-NPM', 'PACKAGES-PYPI'} & release.REQUIRED)
        self.assertLessEqual({'SDK-FILES', 'INSTALL-SCRIPT'}, release.REQUIRED)
        self.gate('INSTALL-SCRIPT')['id'] = 'PACKAGES-NPM'
        with self.assertRaisesRegex(ValueError, 'complete fixed'):
            self.validate()

    def test_no_native_npm_package_is_a_release_file(self):
        self.assertEqual([name for name in release.expected() if name.startswith('npm/')],
                         ['npm/cedegrid-' + V + '.tgz'])
        extra = self.root / ('npm/cedegrid-linux-x64-gnu-' + V + '.tgz')
        tar(extra, {'package/bin/cedegrid': b'native original linux-x64-gnu'})
        self.manifest['artifacts'].append({'path': extra.relative_to(self.root).as_posix(),
                                           'sha256': release.digest(extra), 'size': extra.stat().st_size})
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'unexpected release artifact'):
            self.validate()

    def test_sha256sums_names_each_file_as_the_release_does(self):
        lines = (self.root / 'SHA256SUMS').read_text().splitlines()
        names = [line.split('  ', 1)[1] for line in lines]
        self.assertFalse([name for name in names if '/' in name])
        # The line install.sh looks for: two spaces, then the tarball's own name.
        tarball = 'cedegrid-' + V + '-linux-x64-gnu.tar.gz'
        self.assertEqual([line for line in lines if re.search('  ' + re.escape(tarball) + '$', line)],
                         [release.digest(self.root / release.standalone('linux-x64-gnu')) + '  ' + tarball])
        self.assertIn('install.sh', names)
        for name in ('cedegrid-' + V + '-py3-none-any.whl', 'cedegrid-' + V + '.tar.gz',
                     'cedegrid-' + V + '.tgz'):
            self.assertIn(name, names)

    def test_two_release_files_with_one_name_are_refused(self):
        twin = dict(self.manifest['artifacts'][0], path='evidence/' + Path(self.manifest['artifacts'][0]['path']).name)
        with self.assertRaisesRegex(ValueError, 'share a name'):
            release.checksums(self.manifest['artifacts'] + [twin])

    def test_the_install_script_gate_covers_the_script_and_both_linux_tarballs(self):
        gate = self.gate('INSTALL-SCRIPT')
        gate['artifact_sha256'] = [release.digest(self.root / release.INSTALL_SCRIPT)]
        self.gates['stable_readiness'] = False
        with self.assertRaisesRegex(ValueError, 'blocked by: INSTALL-SCRIPT$'):
            self.validate()

    def test_the_sdk_files_gate_covers_the_wheel_sdist_and_npm_tarball(self):
        gate = self.gate('SDK-FILES')
        gate['artifact_sha256'] = [release.digest(self.root / ('npm/cedegrid-' + V + '.tgz'))]
        self.gates['stable_readiness'] = False
        with self.assertRaisesRegex(ValueError, 'blocked by: SDK-FILES$'):
            self.validate()


class ReleaseVersionTests(unittest.TestCase):
    def root(self, cargo, python, npm):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / 'python').mkdir(); (root / 'npm').mkdir()
        (root / 'Cargo.toml').write_text('[package]\nname = "cedegrid"\nversion = "' + cargo + '"\n\n'
                                         '[dependencies]\nversion = "9.9.9"\n')
        (root / 'python/pyproject.toml').write_text('[project]\nname = "cedegrid"\nversion = "' + python + '"\n')
        (root / 'npm/package.json').write_text(json.dumps({'name': 'cedegrid', 'version': npm}))
        return root

    def test_the_version_is_cargo_tomls_package_version(self):
        self.assertEqual(release_version.check(self.root('1.4.2', '1.4.2', '1.4.2')), '1.4.2')
        self.assertEqual(release_version.gates_path('1.4.2', REPOSITORY).name, 'release-1.4-gates.json')
        self.assertEqual(release.VERSION, release_version.version())

    def test_an_sdk_manifest_on_another_version_stops_the_build(self):
        with self.assertRaisesRegex(ValueError, 'npm/package.json has 1.4.1'):
            release_version.check(self.root('1.4.2', '1.4.2', '1.4.1'))
        with self.assertRaisesRegex(ValueError, 'python/pyproject.toml has 1.3.0'):
            release_version.check(self.root('1.4.2', '1.3.0', '1.4.2'))

    def test_the_0_3_gates_file_lists_exactly_the_required_gates(self):
        gates = json.loads((REPOSITORY / 'docs/release-0.3-gates.json').read_text())
        self.assertEqual(gates['target_release'], '0.3.0')
        self.assertEqual({row['id'] for row in gates['required_gates']}, release.REQUIRED)
        self.assertEqual(len(gates['required_gates']), len(release.REQUIRED))
        self.assertIs(gates['stable_readiness'], False)

    def test_a_malformed_version_is_refused(self):
        with self.assertRaisesRegex(ValueError, 'MAJOR.MINOR.PATCH'):
            release_version.version(self.root('1.4', '1.4', '1.4'))


class WorkflowTests(unittest.TestCase):
    def test_release_and_candidate_actions_are_immutable(self):
        for name in ('release.yml', 'release-candidate.yml'):
            source = (REPOSITORY / '.github/workflows' / name).read_text()
            uses = re.findall(r'uses:\s+([^\s#]+)', source)
            self.assertTrue(uses)
            self.assertTrue(all(re.search(r'@[0-9a-f]{40}$', item) for item in uses), uses)

    def test_the_candidate_builds_both_debian_architectures_once(self):
        # 005 FR-033: the packages come from the candidate's own Linux binaries, never rebuilt.
        source = (REPOSITORY / '.github/workflows/release-candidate.yml').read_text()
        self.assertEqual(source.count('tools/build_deb.py'), 1)
        for pair in ('linux-x64-gnu:amd64', 'linux-arm64-gnu:arm64'):
            self.assertIn(pair, source)

    def test_publish_workflow_does_not_rebuild_or_repack(self):
        source = (REPOSITORY / '.github/workflows/release.yml').read_text()
        for forbidden in ('npm run build', 'npm pack', 'uv build', 'build_packages.py', 'build_native.py',
                          'build_deb.py'):
            self.assertNotRegex(source, r'\b' + re.escape(forbidden) + r'\b')
        self.assertIn('artifact-ids:', source)
        self.assertIn('id-token: write', source)

    def test_no_workflow_publishes_to_npm_or_pypi(self):
        # 003 FR-073: the release is GitHub Release files only.
        for path in sorted((REPOSITORY / '.github/workflows').glob('*.yml')):
            source = path.read_text()
            for forbidden in ('npm publish', 'publish_packages', 'gh-action-pypi-publish', 'registry-url',
                              'twine', 'NPM_TOKEN', 'NODE_AUTH_TOKEN'):
                with self.subTest(workflow=path.name, forbidden=forbidden):
                    self.assertFalse(forbidden in source, forbidden + ' in ' + path.name)

    def test_release_workflows_take_the_version_from_cargo_toml(self):
        for name in ('release.yml', 'release-candidate.yml'):
            source = (REPOSITORY / '.github/workflows' / name).read_text()
            self.assertTrue('tools/release_version.py' in source, name)
            literal = re.findall(r'\b\d+\.\d+\.\d+-(?:source|gnu)|\bv\d+\.\d+\.\d+\b|qualified-\d|--version \d', source)
            self.assertEqual(literal, [], name)


if __name__ == '__main__':
    unittest.main()
