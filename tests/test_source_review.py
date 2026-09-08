"""Private/public source-export boundaries; no publication or workload execution."""
import hashlib
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import pack_source_review as review


class SourceReviewTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.root.relative_to(Path.home())
        self.source = self.root / 'original'
        self.source.mkdir()
        self.files = {
            'Cargo.toml': b'[package]\nname="example"\npublish = false\n',
            'README.md': b'# Example\n[log](docs/work-log.md)\n',
            'docs/validation.md': b'# Validation\nPending GPU and two-node tests. [evidence](../artifacts/private.json)\n',
            'docs/guarantees.md': b'# Guarantees\nNo unverified claims.\n',
            'src/main.rs': b'fn main() {}\n',
            'tests/fixture.rs': b'const WORKER: &str = include_str!("worker.py");\n',
            'tests/worker.py': b'print("fixture")\n',
        }
        for name, data in self.files.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        # This private file is never in the exact export selection.
        (self.source / 'docs/work-log.md').write_text('retain private work log')
        self.allowlist = patch.object(review, 'ALLOWLIST', tuple(self.files))
        self.allowlist.start()

    def tearDown(self):
        self.allowlist.stop()
        self.temp.cleanup()

    def test_deterministic_verified_export_preserves_originals(self):
        first = review.pack(self.source, self.root / 'first')
        second = review.pack(self.source, self.root / 'second')
        self.assertEqual(first['archive_sha256'], second['archive_sha256'])
        tree = self.root / 'first/source'
        self.assertFalse((tree / 'docs/work-log.md').exists())
        self.assertNotIn('artifacts/', (tree / 'docs/validation.md').read_text())
        self.assertIn('Pending GPU and two-node tests', (tree / 'docs/validation.md').read_text())
        manifest = json.loads((tree / 'source-manifest.json').read_text())
        for name, record in manifest['files'].items():
            self.assertEqual(hashlib.sha256((tree / name).read_bytes()).hexdigest(), record['sha256'])
            self.assertEqual((self.source / name).read_bytes(), self.files[name])
        self.assertEqual(first['checks']['rust_include_fixtures_checked'], 1)
        self.assertEqual((self.source / 'docs/work-log.md').read_text(), 'retain private work log')

    def test_existing_output_is_not_overwritten(self):
        output = self.root / 'existing'
        output.mkdir(); (output / 'precious').write_text('retain')
        with self.assertRaisesRegex(ValueError, 'output must be new'):
            review.pack(self.source, output)
        self.assertEqual((output / 'precious').read_text(), 'retain')

    def test_symlink_selected_file_is_refused(self):
        path = self.source / 'src/main.rs'
        path.unlink(); path.symlink_to(self.source / 'README.md')
        with self.assertRaisesRegex(ValueError, 'symlinks'):
            review.pack(self.source, self.root / 'output')
        self.assertFalse((self.root / 'output').exists())

    def test_bad_link_and_missing_include_fixture_fail_before_output(self):
        for value in ('# Example\n[broken](missing.md)\n', '# Example\n[broken](docs/guarantees.md#missing)\n'):
            (self.source / 'README.md').write_text(value)
            with self.assertRaisesRegex(ValueError, 'documentation'):
                review.pack(self.source, self.root / 'output')
        (self.source / 'README.md').write_bytes(self.files['README.md'])
        (self.source / 'tests/fixture.rs').write_text('const X: &str = include_str!("missing.py");')
        with self.assertRaisesRegex(ValueError, 'Rust include fixture'):
            review.pack(self.source, self.root / 'output')
        self.assertFalse((self.root / 'output').exists())

    def test_common_disclosures_are_rejected(self):
        examples = ['-----BEGIN ' + 'PRIVATE KEY-----', '100.' + '64.0.1',
                    '/Users/' + 'a-private-account/source',
                    'https://login.tailscale.com/' + 'a/private-enrollment']
        for value in examples:
            with self.subTest(kind=value.split('/')[0]):
                with self.assertRaises(ValueError):
                    review.inspect_text('candidate.md', value.encode())
        for value in ('127.0.0.1', '192.0.2.10', '/home/USER/source', '/home/test/fixture'):
            review.inspect_text('example.md', value.encode())

    def test_external_rust_module_closure_and_visibility(self):
        files = {name: review.curate(name, data) for name, data in self.files.items()}
        files['src/lib.rs'] = b'pub(crate) mod worker;\n'
        with patch.object(review, 'ALLOWLIST', tuple(files)):
            with self.assertRaisesRegex(ValueError, 'missing Rust module dependency'):
                review.validate_files(files)
        files['src/worker.rs'] = b'pub mod child;\n'
        files['src/worker/child.rs'] = b'pub fn work() {}\n'
        with patch.object(review, 'ALLOWLIST', tuple(files)):
            result = review.validate_files(files)
        self.assertEqual(result['rust_module_dependencies_checked'], 2)
        files['src/worker/mod.rs'] = b'pub fn ambiguous() {}\n'
        with patch.object(review, 'ALLOWLIST', tuple(files)):
            with self.assertRaisesRegex(ValueError, 'ambiguous Rust module dependency'):
                review.validate_files(files)

    def test_rust_include_source_is_required_as_well_as_string_fixture(self):
        files = {name: review.curate(name, data) for name, data in self.files.items()}
        files['src/main.rs'] = b'include!("implementation.rs");\n'
        with patch.object(review, 'ALLOWLIST', tuple(files)):
            with self.assertRaisesRegex(ValueError, 'missing Rust include fixture'):
                review.validate_files(files)
        files['src/implementation.rs'] = b'fn main() {}\n'
        with patch.object(review, 'ALLOWLIST', tuple(files)):
            result = review.validate_files(files)
        self.assertEqual(result['rust_include_fixtures_checked'], 2)

    def test_unhandled_rust_module_layout_fails_explicitly(self):
        for body in (b'mod inline {\n    mod external;\n}\n',
                     b'#[path = "alternate.rs"]\nmod external;\n'):
            files = {name: review.curate(name, data) for name, data in self.files.items()}
            files['src/main.rs'] = body
            with patch.object(review, 'ALLOWLIST', tuple(files)):
                with self.assertRaisesRegex(ValueError, 'requires explicit export review'):
                    review.validate_files(files)

    def test_unknown_file_and_enabled_publication_are_rejected(self):
        files = dict(self.files)
        files['surprise.txt'] = b'unreviewed'
        with self.assertRaisesRegex(ValueError, 'allowlist'):
            review.validate_files(files)
        (self.source / 'Cargo.toml').write_text('[package]\npublish = true\n')
        with self.assertRaisesRegex(ValueError, 'publication'):
            review.pack(self.source, self.root / 'output')


    def public_fixture(self):
        license_bytes = (Path(__file__).resolve().parents[1] / 'LICENSE').read_bytes()
        self.files.update({
            'Cargo.toml': b'[package]\nname="cedegrid"\nlicense = "Apache-2.0"\npublish = false\n',
            'LICENSE': license_bytes,
            'python/LICENSE': license_bytes,
            'python/pyproject.toml': b'[project]\nname="cedegrid"\nlicense = "Apache-2.0"\n',
            'docs/release.md': b'# Release scope\nHardware validation remains separate.\n',
        })
        for name, data in self.files.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)

    def test_public_candidate_has_cedegrid_identity_and_keeps_license(self):
        self.public_fixture()
        with patch.object(review, 'ALLOWLIST', tuple(self.files)):
            result = review.pack(self.source, self.root / 'public', public=True)
        self.assertEqual(result['classification'], 'public_source_candidate_not_published')
        self.assertEqual(result['archive'], 'cedegrid-source.tar.gz')
        self.assertTrue(result['checks']['crates_publication_disabled'])
        tree = self.root / 'public/source'
        readme = (tree / 'README.md').read_text()
        self.assertIn('CedeGrid source distribution', readme)
        self.assertNotIn('This is a private source review', readme)
        self.assertNotIn('not a published release', readme)
        self.assertEqual((tree / 'LICENSE').read_bytes(), self.files['LICENSE'])
        self.assertEqual((tree / 'python/LICENSE').read_bytes(), self.files['LICENSE'])
        self.assertIn('Pending GPU and two-node tests', (tree / 'docs/validation.md').read_text())
        with tarfile.open(self.root / 'public' / result['archive'], 'r:gz') as archive:
            self.assertTrue(all(item.name.startswith('CedeGrid/') for item in archive.getmembers()))
            self.assertEqual(archive.extractfile('CedeGrid/LICENSE').read(), self.files['LICENSE'])
        self.assertFalse((tree / 'docs/work-log.md').exists())

    def test_public_candidate_refuses_missing_or_conflicting_license_before_output(self):
        self.public_fixture()
        variants = [
            ('missing_license', 'LICENSE', None),
            ('mismatched_sdk_license', 'python/LICENSE', b'different license'),
            ('missing_cargo_metadata', 'Cargo.toml', b'[package]\npublish = false\n'),
            ('commented_cargo_metadata', 'Cargo.toml', b'[package]\n# license = "Apache-2.0"\npublish = false\n'),
            ('mismatched_sdk_metadata', 'python/pyproject.toml', b'[project]\nlicense = "MIT"\n'),
            ('wrong_metadata_table', 'python/pyproject.toml', b'[tool.example]\nlicense = "Apache-2.0"\n'),
        ]
        with patch.object(review, 'ALLOWLIST', tuple(self.files)):
            for case, name, data in variants:
                with self.subTest(case=case):
                    for filename, contents in self.files.items():
                        (self.source / filename).write_bytes(contents)
                    changed = self.source / name
                    if data is None:
                        changed.unlink()
                    else:
                        changed.write_bytes(data)
                    output = self.root / case
                    with self.assertRaises(ValueError):
                        review.pack(self.source, output, public=True)
                    self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
