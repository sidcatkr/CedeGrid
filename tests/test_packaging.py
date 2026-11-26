"""The Python wheel and the TypeScript SDK tarball hold the SDK only (003 FR-073, T095).

The release gate runs `sdk_only` on the candidate's real wheel and on one rebuilt from its sdist;
these cases pin what it refuses, and that the package source itself declares no command.
"""
from pathlib import Path
import io
import json
import re
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / 'tools'))
import build_packages  # noqa: E402
import verify_python_packages as packages  # noqa: E402

VERSION = '9.8.7'
INFO = f'cedegrid-{VERSION}.dist-info/'


def wheel(directory, files):
    path = Path(directory) / f'cedegrid-{VERSION}-py3-none-any.whl'
    with zipfile.ZipFile(path, 'w') as archive:
        for name, data in files.items():
            archive.writestr(name, data)
    return path


SDK = {
    'cedegrid/__init__.py': b'',
    'cedegrid/client.py': b'',
    INFO + 'METADATA': b'Name: cedegrid\n',
    INFO + 'WHEEL': b'Root-Is-Purelib: true\nTag: py3-none-any\n',
    INFO + 'RECORD': b'',
}


class WheelHoldsTheSdkOnly(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.directory.cleanup()

    def test_the_sdk_files_pass(self):
        packages.sdk_only(wheel(self.directory.name, SDK), VERSION)

    def test_a_binary_or_script_is_refused(self):
        for extra in ('cedegrid/cedegrid', 'cedegrid/bin/cedegrid', 'cedegrid/launcher.js',
                      f'cedegrid-{VERSION}.data/scripts/cedegrid', 'cedegrid_cli/__init__.py'):
            with self.subTest(extra=extra):
                path = wheel(self.directory.name, {**SDK, extra: b'\x7fELF'})
                with self.assertRaisesRegex(RuntimeError, 'more than the SDK'):
                    packages.sdk_only(path, VERSION)

    def test_a_console_entry_point_is_refused(self):
        entry = {INFO + 'entry_points.txt': b'[console_scripts]\ncedegrid = cedegrid.cli:main\n'}
        path = wheel(self.directory.name, {**SDK, **entry})
        with self.assertRaisesRegex(RuntimeError, 'installs a command'):
            packages.sdk_only(path, VERSION)

    def test_a_wheel_without_the_package_is_refused(self):
        path = wheel(self.directory.name, {k: v for k, v in SDK.items() if not k.startswith('cedegrid/')})
        with self.assertRaisesRegex(RuntimeError, 'no cedegrid package'):
            packages.sdk_only(path, VERSION)

    def test_the_package_source_declares_no_command(self):
        pyproject = (ROOT / 'python' / 'pyproject.toml').read_text()
        for table in ('project.scripts', 'project.gui-scripts', 'project.entry-points'):
            self.assertNotIn('[' + table, pyproject)
        self.assertRegex(pyproject, r'(?m)^packages = \["cedegrid"\]$')
        self.assertNotRegex(pyproject, r'(?m)^(package-data|data-files|script-files)\b')
        files = [path for path in (ROOT / 'python' / 'cedegrid').rglob('*')
                 if path.is_file() and '__pycache__' not in path.parts]
        self.assertTrue(files)
        self.assertEqual([path.name for path in files if path.suffix != '.py'], [])
        manifest = (ROOT / 'python' / 'MANIFEST.in').read_text()
        self.assertEqual(re.findall(r'recursive-include cedegrid (\S+)', manifest), ['*.py'])


class NpmTarballHoldsTheSdkOnly(unittest.TestCase):
    def tarball(self, manifest, extra=()):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / 'cedegrid.tgz'
        files = {'package/package.json': json.dumps(manifest).encode(), 'package/dist/index.js': b''}
        files.update({name: b'' for name in extra})
        with tarfile.open(path, 'w:gz') as archive:
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        return path

    def test_the_sdk_passes(self):
        build_packages.sdk_tarball_only(self.tarball({'name': 'cedegrid', 'scripts': {'build': 'tsc'}}))

    def test_a_command_native_package_launcher_or_install_hook_is_refused(self):
        cases = [({'bin': {'cedegrid': 'dist/launcher.js'}}, (), 'declares bin'),
                 ({'optionalDependencies': {'cedegrid-linux-x64-gnu': '0.2.0'}}, (), 'optionalDependencies'),
                 ({'scripts': {'postinstall': 'node x'}}, (), 'install scripts'),
                 ({}, ('package/dist/launcher.js',), 'holds a launcher')]
        for manifest, extra, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(RuntimeError, message):
                    build_packages.sdk_tarball_only(self.tarball({'name': 'cedegrid', **manifest}, extra))

    def test_the_package_manifest_declares_no_command(self):
        manifest = json.loads((ROOT / 'npm' / 'package.json').read_text())
        for key in ('bin', 'optionalDependencies'):
            self.assertNotIn(key, manifest)
        self.assertFalse({'preinstall', 'install', 'postinstall'} & set(manifest.get('scripts', {})))
        self.assertFalse((ROOT / 'npm' / 'src' / 'launcher.ts').exists())


if __name__ == '__main__':
    unittest.main()
