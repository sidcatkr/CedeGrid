import importlib.util
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
            binary.write_bytes(b'\x7fELF')
            build_deb.stage_cli(work / 'cli', binary, '0.3.0', 'amd64')
            self.assertEqual(
                tree(work / 'cli'),
                [('DEBIAN/control', 0o644), ('usr/bin/cedegrid', 0o755)],
            )
            control = (work / 'cli' / 'DEBIAN' / 'control').read_text()
            self.assertIn('Package: cedegrid\n', control)
            self.assertIn('Version: 0.3.0\n', control)
            self.assertIn('Architecture: amd64\n', control)

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
