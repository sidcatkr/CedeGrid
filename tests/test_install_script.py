"""tools/install.sh against release files served on loopback (003 FR-073, SC-012).

Each case runs the script with an empty HOME and a PATH of chosen tools: it installs the pinned
version into ~/.local/bin, and stops with nothing installed on a changed tarball, a missing
checksum line, no checksum tool, or root.
"""
import hashlib
import http.server
import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / 'tools' / 'install.sh'
VERSION = '9.8.7'
TARGET = 'x86_64-unknown-linux-gnu'
TARBALL = f'cedegrid-{VERSION}-{TARGET}.tar.gz'
TOOLS = ['sh', 'curl', 'tar', 'gzip', 'mktemp', 'mkdir', 'cp', 'mv', 'chmod', 'rm', 'grep', 'cut',
         'uname', 'id', 'cat', 'env']


def tarball(content: bytes) -> bytes:
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode='w:gz') as archive:
        info = tarfile.TarInfo(f'cedegrid-{VERSION}-{TARGET}/cedegrid')
        info.size = len(content)
        info.mode = 0o755
        archive.addfile(info, io.BytesIO(content))
    return stream.getvalue()


class Release:
    """The release directory on a loopback HTTP server."""

    def __init__(self, files: dict):
        self.directory = tempfile.TemporaryDirectory(dir=Path.home())
        for name, data in files.items():
            (Path(self.directory.name) / name).write_bytes(data)
        handler = lambda *args: http.server.SimpleHTTPRequestHandler(  # noqa: E731
            *args, directory=self.directory.name)
        http.server.SimpleHTTPRequestHandler.log_message = lambda *args: None
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}'

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.directory.cleanup()


class InstallScriptTest(unittest.TestCase):
    def setUp(self):
        self.content = b'#!/bin/sh\necho cedegrid 9.8.7\n'
        self.archive = tarball(self.content)
        self.sums = f'{hashlib.sha256(self.archive).hexdigest()}  {TARBALL}\n'.encode()
        self.home = tempfile.TemporaryDirectory(dir=Path.home())
        self.tools = tempfile.TemporaryDirectory(dir=Path.home())

    def tearDown(self):
        self.home.cleanup()
        self.tools.cleanup()

    def path(self, extra=(), leave_out=()):
        """A PATH with only the tools the script may use."""
        directory = Path(self.tools.name) / f'bin-{len(list(Path(self.tools.name).iterdir()))}'
        directory.mkdir()
        for tool in [*TOOLS, 'sha256sum', 'shasum', *extra]:
            found = shutil.which(tool)
            if found and tool not in leave_out and not (directory / tool).exists():
                (directory / tool).symlink_to(found)
        return directory

    def run_script(self, files, bin_dir, env=None):
        release = Release(files)
        try:
            environment = {
                'HOME': self.home.name,
                'PATH': str(bin_dir),
                'CEDEGRID_VERSION': VERSION,
                'CEDEGRID_TARGET': TARGET,
                'CEDEGRID_RELEASE_URL': release.url,
                **(env or {}),
            }
            return subprocess.run(['sh', str(SCRIPT)], env=environment, capture_output=True,
                                  text=True, timeout=60, executable=shutil.which('sh'))
        finally:
            release.close()

    def installed(self):
        return Path(self.home.name) / '.local' / 'bin' / 'cedegrid'

    def test_installs_the_pinned_version_into_the_users_bin(self):
        result = self.run_script({TARBALL: self.archive, 'SHA256SUMS': self.sums}, self.path())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed().read_bytes(), self.content)
        self.assertTrue(os.access(self.installed(), os.X_OK))
        self.assertIn(f'Installed cedegrid {VERSION}', result.stdout)
        # ~/.local/bin is not on this PATH, and the script says so.
        self.assertIn('is not on PATH', result.stdout)

    def test_a_changed_byte_installs_nothing(self):
        changed = bytearray(self.archive)
        changed[len(changed) // 2] ^= 0xFF
        result = self.run_script({TARBALL: bytes(changed), 'SHA256SUMS': self.sums}, self.path())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('does not match', result.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_tarball_without_a_checksum_line_installs_nothing(self):
        result = self.run_script({TARBALL: self.archive, 'SHA256SUMS': b'0  other.tar.gz\n'},
                                 self.path())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('no line for', result.stderr)
        self.assertFalse(self.installed().exists())

    def test_without_a_checksum_tool_it_installs_nothing(self):
        result = self.run_script({TARBALL: self.archive, 'SHA256SUMS': self.sums},
                                 self.path(leave_out=('sha256sum', 'shasum')))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('sha256sum nor shasum', result.stderr)
        self.assertFalse(self.installed().exists())

    def test_as_root_it_refuses(self):
        bin_dir = self.path(leave_out=('id',))
        (bin_dir / 'id').write_text('#!/bin/sh\necho 0\n')
        (bin_dir / 'id').chmod(0o755)
        result = self.run_script({TARBALL: self.archive, 'SHA256SUMS': self.sums}, bin_dir)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('without sudo', result.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_plain_http_url_off_loopback_is_refused(self):
        result = subprocess.run(
            ['sh', str(SCRIPT)],
            env={'HOME': self.home.name, 'PATH': str(self.path()), 'CEDEGRID_VERSION': VERSION,
                 'CEDEGRID_TARGET': TARGET, 'CEDEGRID_RELEASE_URL': 'http://192.0.2.1/release'},
            capture_output=True, text=True, timeout=60)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('must use https', result.stderr)
        self.assertFalse(self.installed().exists())


if __name__ == '__main__':
    unittest.main()
