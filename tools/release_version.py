#!/usr/bin/env python3
"""The release version, read from Cargo.toml's [package] table, and the checks that tie to it.

Every release tool takes its version from here, so a release changes one line in Cargo.toml (and
the two SDK manifests, which `--check` holds to the same value). Standard library only.
"""
from __future__ import annotations
import argparse
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
VERSION_FORM = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z')
# Where a release's files are downloaded from. tools/install.sh, a standalone POSIX script, spells
# out the same address.
RELEASE_URL = 'https://github.com/sidcatkr/CedeGrid/releases/download/v{version}'


def table_value(text: str, table: str, key: str) -> str:
    """One quoted string value from a TOML table, without a TOML parser (Python 3.10 has none)."""
    current = None
    found = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith('['):
            current = stripped
            continue
        if current == '[' + table + ']':
            match = re.fullmatch(key + r'\s*=\s*"([^"]*)"\s*(#.*)?', stripped)
            if match:
                found.append(match.group(1))
    if len(found) != 1:
        raise ValueError(f'expected one {key} in [{table}], found {len(found)}')
    return found[0]


def version(root: Path = ROOT) -> str:
    value = table_value((root / 'Cargo.toml').read_text(encoding='utf-8'), 'package', 'version')
    if not VERSION_FORM.fullmatch(value):
        raise ValueError(f'Cargo.toml version {value!r} is not MAJOR.MINOR.PATCH')
    return value


def release_url(value: str) -> str:
    return RELEASE_URL.format(version=value)


def series(value: str) -> str:
    """MAJOR.MINOR, which names the release's gates file."""
    return '.'.join(value.split('.')[:2])


def gates_path(value: str, root: Path = ROOT) -> Path:
    return root / 'docs' / f'release-{series(value)}-gates.json'


def sdk_versions(root: Path = ROOT) -> dict:
    python = table_value((root / 'python' / 'pyproject.toml').read_text(encoding='utf-8'), 'project',
                         'version')
    npm = json.loads((root / 'npm' / 'package.json').read_text(encoding='utf-8')).get('version')
    return {'python/pyproject.toml': python, 'npm/package.json': npm}


def check(root: Path = ROOT) -> str:
    """The version, once both SDK manifests carry the same one."""
    value = version(root)
    different = {path: found for path, found in sdk_versions(root).items() if found != value}
    if different:
        raise ValueError('versions differ from Cargo.toml ' + value + ': '
                         + ', '.join(f'{path} has {found}' for path, found in sorted(different.items())))
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true',
                        help='fail unless both SDK manifests match Cargo.toml')
    parser.add_argument('--gates', action='store_true', help="print the release's gates file instead")
    args = parser.parse_args()
    value = check() if args.check else version()
    print(gates_path(value).relative_to(ROOT).as_posix() if args.gates else value)


if __name__ == '__main__':
    main()
