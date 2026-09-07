"""CPU comparison contract checks; these never execute application workloads."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import anchor_validation as anchor
from make_kaggriculture_validation import generate


class CpuComparisonTests(unittest.TestCase):
    def test_cpu_is_explicit_bounded_and_has_no_gpu_admission(self):
        envelope = anchor.comparison_envelope('command', 'cpu', None)
        self.assertEqual(envelope['wall_seconds'], 900)
        self.assertLess(envelope['work_seconds'], 900)
        self.assertEqual(envelope['family_rss_bytes'], 4 * 1024**3)
        self.assertEqual(envelope['gpu_memory_mib'], 0)
        for stage, device, uuid in [('cooperative', 'cpu', None), ('command', 'cpu', 'GPU-x'), ('command', 'cuda:0', None)]:
            with self.assertRaises(ValueError):
                anchor.comparison_envelope(stage, device, uuid)

    def test_cpu_manifest_cannot_hide_a_gpu_request(self):
        with self.assertRaisesRegex(ValueError, 'must not request a GPU'):
            generate(Path.home(), 'run', Path.home()/'python', Path.home()/'candidate',
                     'GPU-x', Path.home()/'operator', device='cpu')

    def test_reader_requires_the_requested_real_model_device(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); replay = root/'replay.json'; replay.write_text('{}')
            row = {'game_id': 1, 'backend': 'torchscript', 'device': 'cpu', 'inference_count': 1,
                   'inference_errors': [], 'replay': str(replay), 'replay_sha256': anchor.sha256_file(replay)}
            (root/'games.json').write_text(json.dumps([row])); (root/'run_manifest.json').write_text('{}')
            (root/'summary.json').write_text(json.dumps(dict(bad_status_games=0, invalid_action_games=0, exception_games=0)))
            self.assertEqual(anchor.validate_games(root, 1, 'cpu')['games'], 1)
            with self.assertRaises(RuntimeError):
                anchor.validate_games(root, 1, 'cuda:0')


if __name__ == '__main__':
    unittest.main()
