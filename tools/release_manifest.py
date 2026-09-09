#!/usr/bin/env python3
"""Freeze or verify exact release bytes and enforce every stable qualification gate."""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile
import zipfile

REQUIRED = {
    'SOURCE-RUST', 'CONTRACTS', 'SDK-PYTHON', 'SDK-TYPESCRIPT',
    'PLATFORM-LINUX-X64', 'PLATFORM-LINUX-ARM64', 'PLATFORM-MACOS-X64',
    'PLATFORM-MACOS-ARM64', 'PLATFORM-WIN', 'PACKAGES-NPM', 'PACKAGES-PYPI',
    'RENAME-CONFIG-UPGRADE', 'RECOVERY-MACOS', 'PHYSICAL-THREE-HOST',
    'GPU-RTX5060TI', 'GPU-L4', 'ARTIFACT-CONTROL', 'SUPPLY-CHAIN', 'PUBLIC-EVIDENCE',
}
STATUSES = {'PASS', 'FAIL', 'BLOCKED', 'NOT_RUN'}
ROOT = Path(__file__).resolve().parent.parent
NATIVE = {'darwin-arm64','darwin-x64','linux-arm64-gnu','linux-x64-gnu','win32-x64'}
SOURCE_ARCHIVE = 'source/cedegrid-0.2.0-source.tar.gz'
EXPECTED = {'pypi/cedegrid-0.2.0-py3-none-any.whl','pypi/cedegrid-0.2.0.tar.gz','npm/cedegrid-0.2.0.tgz'}
EXPECTED |= {'npm/cedegrid-'+target+'-0.2.0.tgz' for target in NATIVE}
EXPECTED |= {'standalone/cedegrid-0.2.0-'+target+('.zip' if target=='win32-x64' else '.tar.gz') for target in NATIVE}
SHA256 = re.compile(r'[0-9a-f]{64}\Z')
SOURCE_EXCLUDED = {'docs/release-0.2-gates.json', 'docs/release-0.2-evidence.md'}

def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(part)
    return result.hexdigest()

def member(root, name):
    if not isinstance(name, str) or not name or '\\' in name:
        raise ValueError('unsafe artifact member')
    relative = Path(name)
    if relative.is_absolute() or '..' in relative.parts or str(relative) != name:
        raise ValueError('unsafe artifact member')
    path = root
    for part in relative.parts:
        path /= part
        if path.is_symlink():
            raise ValueError('artifact member is a symlink')
    if not path.is_file():
        raise ValueError('missing artifact member: ' + name)
    return path

def source_names():
    from pack_source_review import ALLOWLIST
    return set(ALLOWLIST) - SOURCE_EXCLUDED

def source_digest(files):
    return hashlib.sha256(json.dumps(files, sort_keys=True, separators=(',', ':')).encode()).hexdigest()

def current_commit():
    return subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()

def source_snapshot():
    # Qualification records are outputs and cannot participate in their own digest.
    files = {name: digest(member(ROOT, name)) for name in sorted(source_names())}
    return {'source_commit': current_commit(), 'source_sha256': source_digest(files), 'files': files,
            'excluded_generated_records': sorted(SOURCE_EXCLUDED), 'includes_uncommitted_work': True}

def verify_source_snapshot(packages, candidate):
    snapshot = json.loads(member(packages, 'source-snapshot.json').read_text())
    files = snapshot['files']
    if not isinstance(files, dict) or set(files) != source_names():
        raise ValueError('source snapshot does not cover the exact export allowlist')
    if (snapshot.get('source_commit') != candidate.get('source_commit')
            or snapshot.get('source_commit') != current_commit()
            or snapshot.get('source_sha256') != candidate.get('source_sha256')
            or source_digest(files) != snapshot.get('source_sha256')
            or snapshot.get('excluded_generated_records') != sorted(SOURCE_EXCLUDED)):
        raise ValueError('source snapshot identity or aggregate hash mismatch')
    for name, expected in files.items():
        if not isinstance(expected, str) or not SHA256.fullmatch(expected) or digest(member(ROOT, name)) != expected:
            raise ValueError('source snapshot file hash mismatch: ' + name)

def verify_native_members(packages, manifest, artifact_paths):
    for target, native in manifest.get('native_inputs', {}).items():
        if target not in NATIVE or not SHA256.fullmatch(native.get('sha256', '')):
            raise ValueError('invalid native input identity')
        size = native.get('size')
        if type(size) is not int or not 0 < size <= 512 * 1024 * 1024:
            raise ValueError('invalid native input size')
        executable = 'cedegrid.exe' if target == 'win32-x64' else 'cedegrid'
        paths = [('npm/cedegrid-' + target + '-0.2.0.tgz', 'package/bin/' + executable),
                 ('standalone/cedegrid-0.2.0-' + target + ('.zip' if target == 'win32-x64' else '.tar.gz'),
                  'cedegrid-0.2.0-' + target + '/' + executable)]
        for relative, name in paths:
            if relative not in artifact_paths:
                raise ValueError('native input lacks its original package/standalone artifact')
            path = member(packages, relative)
            if path.suffix == '.zip':
                with zipfile.ZipFile(path) as archive:
                    entries = [item for item in archive.infolist() if item.filename == name]
                    if len(entries) != 1 or entries[0].file_size != size:
                        raise ValueError('native ZIP member identity/size mismatch')
                    with archive.open(entries[0]) as stream:
                        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
            else:
                with tarfile.open(path) as archive:
                    entries = [item for item in archive.getmembers() if item.name == name]
                    if len(entries) != 1 or not entries[0].isfile() or entries[0].size != size:
                        raise ValueError('native tar member identity/size mismatch')
                    with archive.extractfile(entries[0]) as stream:
                        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
            if actual != native['sha256']:
                raise ValueError('native input hash does not match packaged bytes')

def relevant_hashes(gate_id, artifacts, native):
    paths = {item['path']: item['sha256'] for item in artifacts}
    platform = {'PLATFORM-LINUX-X64': 'linux-x64-gnu', 'PLATFORM-LINUX-ARM64': 'linux-arm64-gnu',
                'PLATFORM-MACOS-X64': 'darwin-x64', 'PLATFORM-MACOS-ARM64': 'darwin-arm64',
                'PLATFORM-WIN': 'win32-x64'}
    if gate_id in platform:
        return {native.get(platform[gate_id], {}).get('sha256')}
    if gate_id in {'SDK-PYTHON', 'PACKAGES-PYPI'}:
        return {paths.get(name) for name in EXPECTED if name.startswith('pypi/')}
    if gate_id == 'SDK-TYPESCRIPT':
        return {paths.get('npm/cedegrid-0.2.0.tgz')}
    if gate_id == 'PACKAGES-NPM':
        return {paths.get(name) for name in EXPECTED if name.startswith('npm/')}
    if gate_id in {'GPU-RTX5060TI', 'GPU-L4', 'ARTIFACT-CONTROL'}:
        return {native.get('linux-x64-gnu', {}).get('sha256')}
    if gate_id == 'PHYSICAL-THREE-HOST':
        return {native.get('linux-x64-gnu', {}).get('sha256'), paths.get('npm/cedegrid-0.2.0.tgz'),
                paths.get('pypi/cedegrid-0.2.0-py3-none-any.whl')}
    if gate_id == 'RECOVERY-MACOS':
        return {native.get(target, {}).get('sha256') for target in ('darwin-arm64', 'darwin-x64')}
    if gate_id in {'SUPPLY-CHAIN', 'PUBLIC-EVIDENCE'}:
        return set(paths.values())
    return set()

def verify_evidence(packages, record):
    entries = record.get('evidence')
    if not isinstance(entries, list) or not entries:
        return False
    seen = set()
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {'path', 'sha256'}:
            raise ValueError('PASS evidence requires path/sha256 records: ' + record['id'])
        path, expected = entry['path'], entry['sha256']
        if not isinstance(path, str) or not path.startswith('evidence/') or path in seen:
            raise ValueError('invalid or duplicate PASS evidence path')
        seen.add(path)
        if not isinstance(expected, str) or not SHA256.fullmatch(expected) or digest(member(packages, path)) != expected:
            raise ValueError('PASS evidence hash mismatch: ' + record['id'])
    return True

def verify_checksums(packages, artifacts):
    expected = ''.join(item['sha256'] + '  ' + item['path'] + '\n'
                       for item in sorted(artifacts, key=lambda item: item['path']))
    if member(packages, 'SHA256SUMS').read_text() != expected:
        raise ValueError('SHA256SUMS differs from the exact artifact manifest')

def validate(packages, gates, require_stable=False):
    package_manifest = json.loads(member(packages, 'manifest.json').read_text())
    if package_manifest.get('version') != '0.2.0' or gates.get('target_release') != '0.2.0':
        raise ValueError('wrong package version')
    artifacts = package_manifest['artifacts']
    if len({a['path'] for a in artifacts}) != len(artifacts):
        raise ValueError('duplicate artifact path')
    for artifact in artifacts:
        if (not isinstance(artifact.get('sha256'), str) or not SHA256.fullmatch(artifact['sha256'])
                or type(artifact.get('size')) is not int or artifact['size'] < 0):
            raise ValueError('invalid artifact hash or size')
        if artifact['path'] not in EXPECTED and not artifact['path'].startswith(('source/', 'evidence/')):
            raise ValueError('unexpected release artifact: ' + artifact['path'])
        path = member(packages, artifact['path'])
        if digest(path) != artifact['sha256'] or path.stat().st_size != artifact['size']:
            raise ValueError('artifact hash or size mismatch: ' + artifact['path'])
    verify_native_members(packages, package_manifest, {item['path'] for item in artifacts})
    records = gates['required_gates']
    if len(records) != len(REQUIRED) or {r['id'] for r in records} != REQUIRED:
        raise ValueError('the complete fixed qualification gate set is required')
    if any(r['status'] not in STATUSES for r in records):
        raise ValueError('invalid qualification status')
    candidate = gates['candidate']
    sha = candidate.get('source_sha256')
    artifact_hashes = {a['sha256'] for a in artifacts}
    artifact_hashes.update(a['sha256'] for a in package_manifest.get('native_inputs', {}).values())
    coherent = isinstance(sha, str) and bool(SHA256.fullmatch(sha)) and candidate.get('artifacts') == artifacts
    if sha is not None:
        verify_source_snapshot(packages, candidate)
    reasons = []
    for record in records:
        referenced = record.get('artifact_sha256')
        qualified = (coherent and record['status'] == 'PASS' and record.get('source_sha256') == sha
                     and record.get('source_commit') == candidate.get('source_commit')
                     and isinstance(referenced, list) and bool(referenced)
                     and len(referenced) == len(set(referenced)) and set(referenced) <= artifact_hashes
                     and relevant_hashes(record['id'], artifacts, package_manifest.get('native_inputs', {})) <= set(referenced)
                     and verify_evidence(packages, record))
        if not qualified:
            reasons.append(record['id'])
    complete = (EXPECTED | {SOURCE_ARCHIVE} <= {a['path'] for a in artifacts}
                and set(package_manifest.get('native_inputs',{})) == NATIVE)
    stable = coherent and complete and not reasons and not package_manifest.get('missing_native_targets')
    if stable:
        verify_checksums(packages, artifacts)
    if gates.get('stable_readiness') is not stable:
        raise ValueError('stable_readiness does not match the verified gate calculation')
    if require_stable and not stable:
        raise ValueError('stable release blocked by: ' + ', '.join(reasons))
    return {'artifact_hashes_verified': len(artifacts), 'stable_readiness': stable,
            'unqualified_gates': reasons, 'source_sha256': sha, 'publication': 'NOT_RUN'}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packages', required=True, type=Path)
    parser.add_argument('--gates', required=True, type=Path)
    parser.add_argument('--freeze', action='store_true')
    parser.add_argument('--require-stable', action='store_true')
    args = parser.parse_args()
    packages = args.packages.resolve(strict=True)
    gates = json.loads(args.gates.read_text())
    if args.freeze:
        source = source_snapshot()
        target = packages/'source-snapshot.json'
        with target.open('x') as stream:
            json.dump(source, stream, indent=2); stream.write('\n')
        manifest = json.loads((packages/'manifest.json').read_text())
        checksums = ''.join(item['sha256'] + '  ' + item['path'] + '\n'
                            for item in sorted(manifest['artifacts'], key=lambda item: item['path']))
        with (packages/'SHA256SUMS').open('x') as stream:
            stream.write(checksums)
        gates['candidate'] = {**{k: source[k] for k in ('source_commit','source_sha256')},
                              'artifacts': manifest['artifacts']}
        for gate in gates['required_gates']:
            if gate['status'] == 'PASS' and gate.get('source_sha256') != source['source_sha256']:
                gate.update(status='NOT_RUN', reason='Prior evidence belongs to a different source candidate.')
        gates['stable_readiness'] = False
        args.gates.write_text(json.dumps(gates, indent=2)+'\n')
    print(json.dumps(validate(packages, gates, args.require_stable), indent=2))

if __name__ == '__main__':
    main()
