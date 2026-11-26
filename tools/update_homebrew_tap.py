#!/usr/bin/env python3
"""Write a published release's Homebrew formula into a clone of the tap.

Takes the release's own cedegrid.rb and SHA256SUMS, as downloaded from the GitHub Release, checks
the formula against its SHA256SUMS line and against the release it names, and writes
Formula/cedegrid.rb and README.md into the tap clone. Committing and pushing are left to the
caller. Standard library only.
"""
from __future__ import annotations
import argparse
import hashlib
from pathlib import Path
import re

import release_version

ROOT = Path(__file__).resolve().parent.parent
TAP_README = ROOT / 'packaging' / 'homebrew' / 'tap-README.md'


def checked_formula(release: Path, version: str) -> str:
    """The formula text, once it matches SHA256SUMS and installs only this release's files."""
    formula = (release / 'cedegrid.rb').read_bytes()
    lines = [line.split('  ', 1) for line in (release / 'SHA256SUMS').read_text().splitlines()]
    listed = [digest for digest, *name in lines if name == ['cedegrid.rb']]
    if listed != [hashlib.sha256(formula).hexdigest()]:
        raise ValueError('cedegrid.rb does not match its SHA256SUMS line')
    text = formula.decode('utf-8')
    if not re.search(r'(?m)^  version "' + re.escape(version) + r'"$', text):
        raise ValueError(f'cedegrid.rb is not for version {version}')
    urls = re.findall(r'url "([^"]+)"', text)
    prefix = release_version.release_url(version) + '/'
    if not urls or any(not url.startswith(prefix) or '/' in url[len(prefix):] for url in urls):
        raise ValueError(f'cedegrid.rb downloads from outside the {version} release')
    sums = {name[0] for digest, *name in lines if name}
    missing = sorted(url[len(prefix):] for url in urls if url[len(prefix):] not in sums)
    if missing:
        raise ValueError('cedegrid.rb downloads files SHA256SUMS does not list: ' + ', '.join(missing))
    return text


def write_tap(tap: Path, formula: str) -> list:
    """Write the formula and README into the tap clone; the paths that changed."""
    changed = []
    for relative, text in (('Formula/cedegrid.rb', formula),
                           ('README.md', TAP_README.read_text(encoding='utf-8'))):
        path = tap / relative
        if path.exists() and path.read_text(encoding='utf-8') == text:
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')
        changed.append(relative)
    return changed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release', type=Path, required=True,
                        help="a directory holding the release's cedegrid.rb and SHA256SUMS")
    parser.add_argument('--tap', type=Path, required=True, help='a clone of the tap repository')
    parser.add_argument('--version', default=None, help="the release's version; Cargo.toml's by default")
    args = parser.parse_args()
    version = args.version or release_version.version()
    changed = write_tap(args.tap, checked_formula(args.release, version))
    print('\n'.join(changed) if changed else 'the tap already holds this release')


if __name__ == '__main__':
    main()
