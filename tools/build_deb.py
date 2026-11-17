#!/usr/bin/env python3
"""Build the cedegrid and cedegrid-system Debian packages from a built binary (005 FR-033)."""
import argparse
import shutil
import subprocess
import tempfile
from pathlib import Path

MAINTAINER = 'CedeGrid <noreply@cedegrid.invalid>'

# Package file -> (path in the package, mode). Nothing else is installed (FR-034).
SYSTEM_FILES = {
    'cedegrid-broker.socket': ('usr/lib/systemd/system/cedegrid-broker.socket', 0o644),
    'cedegrid-broker.service': ('usr/lib/systemd/system/cedegrid-broker.service', 0o644),
    'cedegrid.slice': ('usr/lib/systemd/system/cedegrid.slice', 0o644),
    'cedegrid_idle.slice': ('usr/lib/systemd/system/cedegrid_idle.slice', 0o644),
    'cedegrid-system.sysusers': ('usr/lib/sysusers.d/cedegrid-system.conf', 0o644),
}
SCRIPTS = ('postinst', 'prerm', 'postrm')


def control(package, version, arch, depends, description):
    return (
        f'Package: {package}\n'
        f'Version: {version}\n'
        f'Architecture: {arch}\n'
        f'Maintainer: {MAINTAINER}\n'
        f'Depends: {depends}\n'
        'Section: admin\n'
        'Priority: optional\n'
        f'Description: {description}\n'
    )


def place(root, relative, source, mode):
    target = root / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(mode)


def write_control(root, text):
    target = root / 'DEBIAN' / 'control'
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    target.chmod(0o644)


def stage_cli(root, binary, version, arch):
    place(root, 'usr/bin/cedegrid', binary, 0o755)
    write_control(root, control(
        'cedegrid', version, arch, 'libc6 (>= 2.35)',
        'resource manager for shared Linux servers'))


def stage_system(root, packaging, version, arch):
    for name, (relative, mode) in SYSTEM_FILES.items():
        place(root, relative, packaging / name, mode)
    for script in SCRIPTS:
        place(root, f'DEBIAN/{script}', packaging / script, 0o755)
    write_control(root, control(
        'cedegrid-system', version, arch, f'cedegrid (= {version}), systemd (>= 249)',
        'root broker that gives cedegrid group members System mode'))


def build(root, output, name):
    target = output / name
    subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(root), str(target)],
                   check=True)
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--packaging', required=True, type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--arch', required=True, choices=['amd64', 'arm64'])
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as work:
        work = Path(work)
        stage_cli(work / 'cli', args.binary, args.version, args.arch)
        stage_system(work / 'system', args.packaging, args.version, args.arch)
        for path in (
            build(work / 'cli', args.output, f'cedegrid_{args.version}_{args.arch}.deb'),
            build(work / 'system', args.output,
                  f'cedegrid-system_{args.version}_{args.arch}.deb'),
        ):
            print(path)


if __name__ == '__main__':
    main()
