#!/usr/bin/env python3
"""Verify and unpack a private source bundle under home without replacing files."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import tarfile
import uuid
from validation_runtime import inside_home, atomic_json


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def unpack(archive, sha256, destination, activate_in=None, expected_previous=None):
    archive, destination = map(inside_home, (archive, destination))
    if archive.stat().st_size > 16 * 1024**2 or hashlib.sha256(archive.read_bytes()).hexdigest() != sha256:
        raise ValueError('archive size or SHA256 mismatch')
    if destination.exists():
        raise ValueError('destination must be new; retain previous bundles')
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = destination.with_name('.' + destination.name + '.unpack-' + uuid.uuid4().hex)
    staging.mkdir(mode=0o700)
    try:
        with tarfile.open(archive, 'r:gz') as source:
            members = source.getmembers()
            if len(members) > 5000 or sum(item.size for item in members) > 128 * 1024**2:
                raise ValueError('source bundle exceeds count/expanded size bound')
            names = set()
            for item in members:
                path = PurePosixPath(item.name)
                if not item.isfile() or path.is_absolute() or '..' in path.parts or '\\' in item.name or item.name in names:
                    raise ValueError('nonregular, duplicate or escaping archive member')
                if str(path) != item.name or not path.parts:
                    raise ValueError('noncanonical archive path')
                names.add(item.name)
            if 'source-manifest.json' not in names:
                raise ValueError('source manifest missing')
            manifest = json.load(source.extractfile('source-manifest.json'))
            if manifest.get('classification') != 'private_validation_source_and_derived_dataset':
                raise ValueError('unexpected bundle classification')
            expected = manifest['files']
            if set(expected) != names - {'source-manifest.json'}:
                raise ValueError('manifest does not account for every archive file')
            for item in members:
                if item.name != 'source-manifest.json' and not item.name.startswith(('source/ResourceManager/', 'source/Kaggriculture/', 'bootstrap-dataset/')):
                    raise ValueError('unexpected source component')
                stream = source.extractfile(item)
                data = stream.read(item.size + 1)
                if len(data) != item.size:
                    raise ValueError('truncated archive member')
                if item.name in expected:
                    evidence = expected[item.name]
                    if len(data) != evidence['bytes'] or hashlib.sha256(data).hexdigest() != evidence['sha256']:
                        raise ValueError('source file integrity failure: ' + item.name)
                path = staging / item.name
                path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
                with path.open('xb') as output:
                    os.chmod(path, 0o600)
                    output.write(data); output.flush(); os.fsync(output.fileno())
        for path in sorted([staging, *[p for p in staging.rglob('*') if p.is_dir()]], key=lambda p: len(p.parts), reverse=True):
            sync_directory(path)
        os.rename(staging, destination)
        sync_directory(destination.parent)
    except BaseException:
        # Only this invocation's private staging tree is eligible for cleanup.
        import shutil
        shutil.rmtree(staging)
        raise
    result = {'schema_version': 1, 'archive_sha256': sha256, 'destination': str(destination),
              'verified_files': len(expected), 'activated': False}
    if activate_in is not None:
        root = inside_home(activate_in)
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        previous = inside_home(expected_previous) if expected_previous else None
        for name in ('source', 'bootstrap-dataset'):
            if not (destination / name).is_dir():
                raise ValueError('verified bundle component missing')
            link = root / name
            if previous is not None:
                if not link.is_symlink() or link.resolve() != (previous / name).resolve():
                    raise ValueError('activation previous bundle identity mismatch: ' + str(link))
            elif os.path.lexists(link):
                raise ValueError('activation refuses to replace existing path: ' + str(link))
        # Exclusive symlink creation cannot replace a concurrent existing path.
        # If interrupted between links, preserve evidence and finish only after
        # verifying both existing link targets against this immutable bundle.
        for name in ('source', 'bootstrap-dataset'):
            if previous is None:
                os.symlink(destination / name, root / name, target_is_directory=True)
            else:
                link = root / name
                if not link.is_symlink() or link.resolve() != (previous / name).resolve():
                    raise ValueError('activation link changed; preserve both bundles')
                temporary = root / ('.' + name + '.activate-' + uuid.uuid4().hex)
                os.symlink(destination / name, temporary, target_is_directory=True)
                os.replace(temporary, link)
            sync_directory(root)
        result['activated'] = True
    atomic_json(destination / 'verification.json', result)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('archive', 'sha256', 'destination'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--activate-in')
    parser.add_argument('--expected-previous', help='replace only verified links to this exact prior bundle; stop owned validation services first')
    print(json.dumps(unpack(**vars(parser.parse_args())), indent=2))
