"""Private bundle verification faults; no remote deployment or workload execution."""
import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from unpack_validation import unpack


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.root.relative_to(Path.home())

    def tearDown(self):
        self.temp.cleanup()

    def bundle(self, corrupt=False, escape=False):
        files = {'source/ResourceManager/src/main.rs': b'fn main() {}',
                 'source/Kaggriculture/main.py': b'pass',
                 'bootstrap-dataset/manifest.json': b'{}'}
        manifest = {'classification': 'private_validation_source_and_derived_dataset',
                    'files': {name: {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()} for name, data in files.items()}}
        files['source-manifest.json'] = json.dumps(manifest).encode()
        if corrupt:
            files['source/ResourceManager/src/main.rs'] = b'x'
        if escape:
            files['../unrelated'] = b'bad'
        archive = self.root / 'source.tar.gz'
        with tarfile.open(archive, 'w:gz') as output:
            for name, data in files.items():
                info = tarfile.TarInfo(name); info.size = len(data)
                output.addfile(info, io.BytesIO(data))
        return archive, hashlib.sha256(archive.read_bytes()).hexdigest()

    def test_exact_verification_and_exclusive_activation(self):
        archive, digest = self.bundle()
        result = unpack(archive, digest, self.root / 'bundle', self.root / 'runtime')
        self.assertTrue(result['activated'])
        self.assertEqual((self.root / 'runtime/source/ResourceManager/src/main.rs').read_bytes(), b'fn main() {}')
        with self.assertRaisesRegex(ValueError, 'destination must be new'):
            unpack(archive, digest, self.root / 'bundle')

    def test_corrupted_content_cannot_publish(self):
        archive, digest = self.bundle(corrupt=True)
        with self.assertRaisesRegex(ValueError, 'integrity'):
            unpack(archive, digest, self.root / 'bundle')
        self.assertFalse((self.root / 'bundle').exists())
        self.assertEqual(list(self.root.glob('*.unpack-*')), [])

    def test_escape_and_existing_source_preserved(self):
        archive, digest = self.bundle(escape=True)
        with self.assertRaises(ValueError):
            unpack(archive, digest, self.root / 'bundle')
        self.assertFalse((self.root / 'unrelated').exists())
        archive.unlink()
        archive, digest = self.bundle()
        (self.root / 'runtime/source').mkdir(parents=True)
        original = self.root / 'runtime/source/precious.txt'; original.write_text('retain')
        with self.assertRaisesRegex(ValueError, 'refuses to replace'):
            unpack(archive, digest, self.root / 'bundle', self.root / 'runtime')
        self.assertEqual(original.read_text(), 'retain')

    def test_wrong_archive_hash_rejected_before_extraction(self):
        archive, _ = self.bundle()
        with self.assertRaisesRegex(ValueError, 'SHA256'):
            unpack(archive, '0' * 64, self.root / 'bundle')
        self.assertFalse((self.root / 'bundle').exists())

    def test_reviewed_bundle_switch_preserves_previous_source_and_rejects_guessing(self):
        archive, digest = self.bundle()
        unpack(archive, digest, self.root / 'one', self.root / 'runtime')
        with self.assertRaisesRegex(ValueError, 'identity mismatch'):
            unpack(archive, digest, self.root / 'wrong', self.root / 'runtime', self.root / 'not-current')
        unpack(archive, digest, self.root / 'two', self.root / 'runtime', self.root / 'one')
        self.assertEqual((self.root / 'runtime/source').resolve(), self.root / 'two/source')
        self.assertEqual((self.root / 'one/source/ResourceManager/src/main.rs').read_bytes(), b'fn main() {}')


if __name__ == '__main__':
    unittest.main()
