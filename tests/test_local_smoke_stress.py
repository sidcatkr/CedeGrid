"""Bounded stress analysis guards; no actual CPU/GPU stress in this suite."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).parents[1]/'tools'))
import local_smoke


class StressContracts(unittest.TestCase):
    def test_ordinary_command_reader_requires_real_inference_and_replay_integrity(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);replay=root/'replay.json.gz';replay.write_bytes(b'actual-replay')
            game={'backend':'torchscript','device':'cpu','inference_count':1,'inference_errors':[],
                'replay':str(replay),'replay_sha256':local_smoke.sha256_file(replay)}
            (root/'games.json').write_text(json.dumps([game]));(root/'summary.json').write_text(json.dumps(
                {'bad_status_games':0,'invalid_action_games':0,'exception_games':0}))
            self.assertEqual(len(local_smoke.verify_ordinary_game(root)['games']),1)
            replay.write_bytes(b'corrupt')
            with self.assertRaisesRegex(RuntimeError,'checksum'):local_smoke.verify_ordinary_game(root)

    def test_post_release_cpu_excludes_previous_contention_and_requires_duration(self):
        report={'samples':[{'observed_monotonic':0,'process_cpu_seconds':0},
            {'observed_monotonic':11,'process_cpu_seconds':4},
            {'observed_monotonic':17,'process_cpu_seconds':16}]}
        self.assertEqual(local_smoke.external_cpu_after_release(report,10),2.)
        self.assertIsNone(local_smoke.external_cpu_after_release(report,16))

    def test_mac_stress_never_claims_native_scope(self):
        with patch.object(local_smoke.sys,'platform','darwin'):
            with self.assertRaisesRegex(RuntimeError,'unverified'):local_smoke.stress_cpu_scope()

    def test_external_same_uid_process_cannot_be_a_managed_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);root.relative_to(Path.home())
            connection=sqlite3.connect(root/'state.sqlite3');connection.execute('CREATE TABLE executions(record_json TEXT)')
            connection.execute('INSERT INTO executions VALUES(?)',(json.dumps({'identity':{'pid':4,'boot_id':'boot','start_time':6}}),));connection.commit();connection.close()
            self.assertTrue(local_smoke.outside_registry(root,{'pid':4,'boot_id':'boot','start_ticks':7}))
            with self.assertRaisesRegex(RuntimeError,'external process'):local_smoke.outside_registry(root,{'pid':4,'boot_id':'boot','start_ticks':6})
