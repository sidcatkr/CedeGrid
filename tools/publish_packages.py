#!/usr/bin/env python3
"""Plan exact registry uploads; --apply requires complete gates and GitHub OIDC.

Every candidate registry file is checked before the first upload or staging
mutation. The native, Python staging, and main phases never build or repack.
"""
from __future__ import annotations
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.error
import urllib.request
from release_manifest import EXPECTED, NATIVE, validate, member, digest

NPM_NAMES = {'cedegrid'} | {'cedegrid-' + target for target in NATIVE}
PYTHON_FILES = {'cedegrid-0.2.0-py3-none-any.whl', 'cedegrid-0.2.0.tar.gz'}


def fetch(url):
    request = urllib.request.Request(url, headers={'User-Agent': 'CedeGrid-release/0.2'})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read(8 * 1024 * 1024 + 1)
            if len(data) > 8 * 1024 * 1024:
                raise ValueError('registry response exceeds bound')
            return json.loads(data)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def fetch_hashes(url, expected_size):
    sha256, sha512, size = hashlib.sha256(), hashlib.sha512(), 0
    request = urllib.request.Request(url, headers={'User-Agent': 'CedeGrid-release/0.2'})
    with urllib.request.urlopen(request, timeout=30) as response:
        if response.geturl() != url:
            raise ValueError('registry tarball redirected away from its canonical URL')
        while True:
            chunk = response.read(min(1024 * 1024, expected_size - size + 1))
            if not chunk:
                break
            size += len(chunk)
            if size > expected_size:
                raise ValueError('published tarball is larger than the original candidate')
            sha256.update(chunk)
            sha512.update(chunk)
    return size, sha256.hexdigest(), 'sha512-' + base64.b64encode(sha512.digest()).decode()


def npm_state(path):
    with tarfile.open(path) as archive:
        entries = [item for item in archive.getmembers() if item.name == 'package/package.json']
        if len(entries) != 1 or not entries[0].isfile() or entries[0].size > 65536:
            raise ValueError('npm tarball must contain one bounded package manifest')
        package = json.load(archive.extractfile(entries[0]))
    name = package.get('name')
    if (name not in NPM_NAMES or package.get('version') != '0.2.0'
            or path.name != name + '-0.2.0.tgz'):
        raise ValueError('wrong npm candidate identity or filename')
    value = fetch('https://registry.npmjs.org/' + name + '/0.2.0')
    if value is None:
        return name, 'new'
    integrity = 'sha512-' + base64.b64encode(hashlib.sha512(path.read_bytes()).digest()).decode()
    url = 'https://registry.npmjs.org/' + name + '/-/' + name + '-0.2.0.tgz'
    if (value.get('name') != name or value.get('version') != '0.2.0'
            or value.get('dist', {}).get('integrity') != integrity
            or value.get('dist', {}).get('tarball') != url):
        raise ValueError('already published npm version has different bytes/identity: ' + name)
    actual = fetch_hashes(url, path.stat().st_size)
    if actual != (path.stat().st_size, digest(path), integrity):
        raise ValueError('fetched npm tarball differs from the original candidate: ' + name)
    return name, 'identical'


def python_state(path):
    if path.name not in PYTHON_FILES:
        raise ValueError('unexpected Python candidate filename')
    value = fetch('https://pypi.org/pypi/cedegrid/0.2.0/json')
    if value is None:
        return 'new'
    if (value.get('info', {}).get('name', '').lower() != 'cedegrid'
            or value.get('info', {}).get('version') != '0.2.0'
            or any(item.get('filename') not in PYTHON_FILES for item in value['urls'])):
        raise ValueError('existing PyPI release is not this coordinated candidate')
    matches = [item for item in value['urls'] if item['filename'] == path.name]
    if not matches:
        return 'new'
    if len(matches) != 1 or matches[0]['digests']['sha256'] != digest(path):
        raise ValueError('already published PyPI filename has different bytes: ' + path.name)
    return 'identical'


def require_oidc():
    if (os.environ.get('GITHUB_ACTIONS') != 'true'
            or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted'
            or not os.environ.get('ACTIONS_ID_TOKEN_REQUEST_URL')
            or not os.environ.get('ACTIONS_ID_TOKEN_REQUEST_TOKEN')):
        raise ValueError('--apply requires the configured GitHub-hosted OIDC publish job')


def npm_publish(path):
    # Isolated config prevents local settings or registry tokens from silently
    # replacing this workflow's OIDC authentication.
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith('NPM_CONFIG_') and key not in {'NODE_AUTH_TOKEN', 'NPM_TOKEN'}}
    with tempfile.TemporaryDirectory(prefix='cedegrid-npm-publish-') as directory:
        root = Path(directory)
        user, global_config = root / 'npmrc', root / 'global-npmrc'
        user.write_text('registry=https://registry.npmjs.org\nignore-scripts=true\nprovenance=true\n')
        global_config.write_text('')
        subprocess.run(['npm', 'publish', str(path), '--ignore-scripts', '--provenance',
                        '--access', 'public', '--tag', 'latest', '--registry', 'https://registry.npmjs.org',
                        '--userconfig', str(user), '--globalconfig', str(global_config)],
                       cwd=root, env=env, check=True)


def publish_phase(root, gates, phase, *, apply=False, python_stage=None):
    validate(root, gates, require_stable=True)
    if phase not in {'native', 'python-stage', 'main'}:
        raise ValueError('unknown publish phase')
    if apply:
        require_oidc()
    if phase == 'python-stage' and (python_stage is None or python_stage.exists() or python_stage.is_symlink()):
        raise ValueError('Python staging requires an explicitly new directory')
    manifest = json.loads(member(root, 'manifest.json').read_text())
    files = {item['path']: member(root, item['path']) for item in manifest['artifacts']}
    # Fully preflight every package before the first irreversible upload.
    states = {}
    for name in sorted(EXPECTED):
        if name.startswith('npm/'):
            states[name] = npm_state(files[name])[1]
        elif name.startswith('pypi/'):
            states[name] = python_state(files[name])
    native = sorted(name for name in states if name.startswith('npm/') and name != 'npm/cedegrid-0.2.0.tgz')
    python = sorted(name for name in states if name.startswith('pypi/'))
    if phase in {'python-stage', 'main'} and any(states[name] != 'identical' for name in native):
        raise ValueError('every exact native npm package must be published before Python/main')
    if phase == 'main' and any(states[name] != 'identical' for name in python):
        raise ValueError('every exact Python artifact must be published before main npm')
    selected = native if phase == 'native' else python if phase == 'python-stage' else ['npm/cedegrid-0.2.0.tgz']
    actions = [{'file': files[name].name, 'sha256': digest(files[name]), 'registry_state': states[name]}
               for name in selected]
    # Network preflight can be slow. Recheck all original bytes before mutation.
    validate(root, gates, require_stable=True)
    pending = [name for name in selected if states[name] == 'new']
    if phase == 'python-stage' and pending:
        python_stage.mkdir(parents=True, exist_ok=False)
    expected = {item['path']: item['sha256'] for item in manifest['artifacts']}
    for name in pending:
        path = files[name]
        if digest(path) != expected[name]:
            raise ValueError('original artifact changed after registry preflight')
        if phase == 'python-stage':
            target = python_stage / path.name
            with target.open('xb') as stream, path.open('rb') as source:
                shutil.copyfileobj(source, stream)
            if digest(target) != expected[name]:
                raise ValueError('staged Python bytes changed')
        elif apply:
            npm_publish(path)
            if npm_state(path)[1] != 'identical':
                raise ValueError('published npm bytes failed verification')
    return {'phase': phase, 'apply': apply, 'actions': actions,
            'registry_preflight': states, 'rebuild_or_repack': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packages', type=Path, required=True)
    parser.add_argument('--gates', type=Path, required=True)
    parser.add_argument('--phase', choices=('native', 'python-stage', 'main'), required=True)
    parser.add_argument('--python-stage', type=Path, help='new directory of original files not yet present on PyPI')
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    result = publish_phase(args.packages.resolve(strict=True), json.loads(args.gates.read_text()),
                           args.phase, apply=args.apply, python_stage=args.python_stage)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
