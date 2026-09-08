#!/usr/bin/env python3
"""Mirror original 0.2.0 payloads under the GitHub npm scope.

Only package.json naming, registry, and native dependency aliases change. These
are separate mirror archives, not replacements for the original release files.
Mirroring does not upgrade the original release's qualification status.
"""
from __future__ import annotations
import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile

REGISTRY = 'https://npm.pkg.github.com'
SCOPE = '@sidcatkr/'
VERSION = '0.2.0'
TARGETS = ['darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu', 'win32-x64']
NAMES = ['cedegrid-' + target for target in TARGETS] + ['cedegrid']


def sha(data):
    return hashlib.sha256(data).hexdigest()


def members(path):
    result = {}
    with tarfile.open(path) as archive:
        for item in archive:
            if item.isdir():
                continue
            if (not item.isfile() or not item.name.startswith('package/')
                    or '..' in Path(item.name).parts or item.name in result):
                raise ValueError('unsafe or duplicate package member')
            result[item.name] = (archive.extractfile(item).read(), item.mode)
    if 'package/package.json' not in result:
        raise ValueError('package manifest absent')
    return result


def prepare(assets, output):
    manifest = json.loads((assets / 'manifest.json').read_text())
    if manifest['version'] != VERSION:
        raise ValueError('unexpected original release version')
    output.mkdir(exist_ok=False, parents=True)
    records = []
    for name in NAMES:
        relative = 'npm/' + name + '-' + VERSION + '.tgz'
        rows = [a for a in manifest['artifacts'] if a['path'] == relative]
        if len(rows) != 1:
            raise ValueError('one original artifact required: ' + name)
        original = assets / relative
        if not original.exists():
            original = assets / Path(relative).name
        if sha(original.read_bytes()) != rows[0]['sha256']:
            raise ValueError('original release archive checksum mismatch')
        files = members(original)
        package = json.loads(files['package/package.json'][0])
        if package['name'] != name or package['version'] != VERSION:
            raise ValueError('unexpected original package identity')
        package['name'] = SCOPE + name
        package['publishConfig'] = {'registry': REGISTRY}
        if name == 'cedegrid':
            expected = {native: VERSION for native in NAMES[:-1]}
            if package.get('optionalDependencies') != expected:
                raise ValueError('unexpected original optional dependencies')
            package['optionalDependencies'] = {
                native: 'npm:' + SCOPE + native + '@' + VERSION for native in NAMES[:-1]
            }
        files['package/package.json'] = ((json.dumps(package, indent=2) + '\n').encode(), 0o644)
        destination = output / ('sidcatkr-' + name + '-' + VERSION + '.tgz')
        with destination.open('xb') as raw:
            with gzip.GzipFile(filename='', fileobj=raw, mode='wb', mtime=0) as zipped:
                with tarfile.open(fileobj=zipped, mode='w', format=tarfile.USTAR_FORMAT) as archive:
                    for filename, (data, mode) in sorted(files.items()):
                        info = tarfile.TarInfo(filename)
                        info.size, info.mode, info.mtime = len(data), mode, 0
                        archive.addfile(info, io.BytesIO(data))
        copied = members(destination)
        if copied != files:
            raise ValueError('mirror archive payload changed')
        records.append({'name': package['name'], 'file': destination.name,
                        'sha256': sha(destination.read_bytes()),
                        'original_sha256': rows[0]['sha256'],
                        'payloads': {k: {'sha256': sha(v[0]), 'mode': v[1]}
                                     for k, v in files.items() if k != 'package/package.json'}})
    record = {'version': VERSION, 'registry': REGISTRY, 'packages': records,
              'qualification': 'Original release qualification unchanged',
              'original_manifest_sha256': sha((assets / 'manifest.json').read_bytes())}
    (output / 'mirror-manifest.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps({'prepared': len(records), 'payload_bytes_preserved': True}))


def run(args, cwd, capture=False):
    return subprocess.run(args, cwd=cwd, check=True, text=True,
                          capture_output=capture, timeout=180)


def load_mirror(root):
    manifest = json.loads((root / 'mirror-manifest.json').read_text())
    if [p['name'] for p in manifest['packages']] != [SCOPE + name for name in NAMES]:
        raise ValueError('unexpected mirror package set/order')
    for package in manifest['packages']:
        path = root / package['file']
        if path.parent != root or sha(path.read_bytes()) != package['sha256']:
            raise ValueError('mirror bytes changed')
    return manifest


def publish(root):
    manifest = load_mirror(root)
    if not os.environ.get('NODE_AUTH_TOKEN'):
        raise ValueError('NODE_AUTH_TOKEN required; use repository GITHUB_TOKEN in Actions')
    with tempfile.TemporaryDirectory(prefix='cedegrid-github-publish-') as temporary:
        cwd = Path(temporary)
        existing = {}
        # Detect every existing-version conflict before the first upload.
        for package in manifest['packages']:
            result = subprocess.run(['npm', 'view', package['name'] + '@' + VERSION,
                                     'dist', '--json', '--registry=' + REGISTRY],
                                    cwd=cwd, capture_output=True, text=True, timeout=60)
            if result.returncode:
                if 'E404' not in result.stderr:
                    raise RuntimeError('registry preflight failed: ' + result.stderr)
                existing[package['name']] = False
                continue
            dist = json.loads(result.stdout)
            data = (root / package['file']).read_bytes()
            integrity = 'sha512-' + base64.b64encode(hashlib.sha512(data).digest()).decode()
            if dist.get('integrity') != integrity and dist.get('shasum') != hashlib.sha1(data).hexdigest():
                raise ValueError('existing mirror version has different bytes: ' + package['name'])
            existing[package['name']] = True
        for package in manifest['packages']:
            if not existing[package['name']]:
                run(['npm', 'publish', str(root / package['file']), '--ignore-scripts',
                     '--registry=' + REGISTRY], cwd)
            result = run(['npm', 'pack', package['name'] + '@' + VERSION, '--ignore-scripts',
                          '--registry=' + REGISTRY, '--pack-destination', str(cwd), '--json'], cwd, True)
            downloaded = cwd / json.loads(result.stdout)[0]['filename']
            if sha(downloaded.read_bytes()) != package['sha256']:
                raise ValueError('published archive does not match mirror: ' + package['name'])
            print(package['name'] + '@' + VERSION + ' verified')


def verify_install(root):
    manifest = load_mirror(root)
    system = {'Darwin': 'darwin', 'Linux': 'linux', 'Windows': 'win32'}[platform.system()]
    arch = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64', 'AMD64': 'x64'}[platform.machine()]
    target = system + '-' + arch + ('-gnu' if system == 'linux' else '')
    with tempfile.TemporaryDirectory(prefix='cedegrid-github-install-') as temporary:
        cwd = Path(temporary)
        (cwd / 'package.json').write_text('{"private":true}')
        run(['npm', 'install', SCOPE + 'cedegrid@' + VERSION, '--ignore-scripts', '--no-audit', '--no-fund'], cwd)
        main = cwd / 'node_modules/@sidcatkr/cedegrid'
        resolve = run(['node', '-e', 'const {createRequire}=require("node:module");const r=createRequire(process.argv[1]);console.log(r.resolve(process.argv[2]+"/package.json"))',
                       str(main / 'dist/launcher.js'), 'cedegrid-' + target], cwd, True)
        native = Path(resolve.stdout.strip()).parent
        for name, directory in [(SCOPE + 'cedegrid', main), (SCOPE + 'cedegrid-' + target, native)]:
            package = next(p for p in manifest['packages'] if p['name'] == name)
            for filename, expected in package['payloads'].items():
                if sha((directory / filename.removeprefix('package/')).read_bytes()) != expected['sha256']:
                    raise ValueError('installed payload differs: ' + filename)
        run(['node', '-e', 'const s=require("@sidcatkr/cedegrid");if(typeof s.Client!=="function")process.exit(1)'], cwd)
        result = run(['node', str(main / 'dist/launcher.js'), '--version'], cwd, True)
        if VERSION not in result.stdout:
            raise ValueError('installed launcher failed')
    print(json.dumps({'installed_alias_and_original_payloads': 'PASS', 'target': target}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('phase', choices=['prepare', 'publish', 'verify-install'])
    parser.add_argument('--assets', type=Path)
    parser.add_argument('--mirror', required=True, type=Path)
    args = parser.parse_args()
    root = args.mirror.resolve()
    if args.phase == 'prepare':
        prepare(args.assets.resolve(), root)
    elif args.phase == 'publish':
        publish(root)
    else:
        verify_install(root)


if __name__ == '__main__':
    main()
