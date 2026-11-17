import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('build_deb', ROOT / 'tools' / 'build_deb.py')
build_deb = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build_deb)


def tree(root):
    return sorted(
        (str(path.relative_to(root)), path.stat().st_mode & 0o777)
        for path in root.rglob('*')
        if path.is_file()
    )


class BuildDebTest(unittest.TestCase):
    def test_cli_package_holds_only_the_binary(self):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            binary = work / 'cedegrid'
            binary.write_bytes(b'\x7fELF GLIBC_2.17\0GLIBC_2.39\0GLIBC_2.4\0')
            old = os.umask(0o002)
            try:
                build_deb.stage_cli(work / 'cli', binary, '0.3.0', 'amd64')
            finally:
                os.umask(old)
            self.assertEqual(
                tree(work / 'cli'),
                [('DEBIAN/control', 0o644), ('usr/bin/cedegrid', 0o755)],
            )
            control = (work / 'cli' / 'DEBIAN' / 'control').read_text()
            self.assertIn('Package: cedegrid\n', control)
            self.assertIn('Version: 0.3.0\n', control)
            self.assertIn('Architecture: amd64\n', control)
            self.assertIn('Depends: libc6 (>= 2.39)\n', control)
            for path in (work / 'cli').rglob('*'):
                if path.is_dir():
                    self.assertEqual(path.stat().st_mode & 0o777, 0o755, path)

    def test_system_package_holds_the_contract_files_and_scripts(self):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            build_deb.stage_system(work / 'system', ROOT / 'packaging' / 'deb', '0.3.0', 'arm64')
            self.assertEqual(
                tree(work / 'system'),
                [
                    ('DEBIAN/control', 0o644),
                    ('DEBIAN/postinst', 0o755),
                    ('DEBIAN/postrm', 0o755),
                    ('DEBIAN/prerm', 0o755),
                    ('usr/lib/systemd/system/cedegrid-broker.service', 0o644),
                    ('usr/lib/systemd/system/cedegrid-broker.socket', 0o644),
                    ('usr/lib/systemd/system/cedegrid.slice', 0o644),
                    ('usr/lib/systemd/system/cedegrid_idle.slice', 0o644),
                    ('usr/lib/sysusers.d/cedegrid-system.conf', 0o644),
                ],
            )
            control = (work / 'system' / 'DEBIAN' / 'control').read_text()
            self.assertIn('Depends: cedegrid (= 0.3.0), systemd (>= 249)\n', control)


if __name__ == '__main__':
    unittest.main()


class GlibcFloorTest(unittest.TestCase):
    def test_floor_is_the_newest_named_version_and_never_below_2_35(self):
        with tempfile.TemporaryDirectory() as work:
            binary = Path(work) / 'b'
            binary.write_bytes(b'GLIBC_2.2.5\0GLIBC_2.17\0')
            self.assertEqual(build_deb.glibc_floor(binary), '2.35')
            binary.write_bytes(b'GLIBC_2.38\0GLIBC_2.39\0')
            self.assertEqual(build_deb.glibc_floor(binary), '2.39')
