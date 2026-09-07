import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from resmgr import WorkerContext, DrainRequested, canonical_json, command_task


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.context = WorkerContext({"task_id": "task", "assignment_id": "assignment", "generation": 2,
                                      "output_dir": str(self.root / "out")}, self.root / "drain")

    def tearDown(self):
        self.temp.cleanup()

    def test_result_is_immutable_and_repeatable(self):
        source = self.root / "data"
        source.write_bytes(b"useful result")
        artifact = self.context.artifact("payload", source)
        first = self.context.complete({"value": 42}, [artifact])
        self.assertEqual(first, self.context.complete({"value": 42}, [artifact]))
        with self.assertRaises(ValueError):
            self.context.complete({"value": 43}, [artifact])

    def test_checkpoint_sequence_and_durable_descriptor_hash(self):
        first = self.context.checkpoint({"cursor": 4})
        reopened = WorkerContext(self.context.context)
        second = reopened.checkpoint({"cursor": 8})
        self.assertEqual((first["checkpoint_sequence"], second["checkpoint_sequence"]), (1, 2))
        expected = dict(second)
        expected.pop("result_hash")
        self.assertEqual(second["result_hash"], hashlib.sha256(canonical_json(expected)).hexdigest())

    def test_drain_hooks_run_once_and_safe_point_raises(self):
        calls = []
        self.context.on_drain(lambda _: calls.append(1))
        self.assertFalse(self.context.draining())
        (self.root / "drain").write_text("drain")
        self.assertTrue(self.context.draining())
        with self.assertRaises(DrainRequested):
            self.context.safe_point()
        self.assertEqual(calls, [1])

    def test_tampered_and_outside_artifacts_are_rejected(self):
        source = self.root / "outside"
        source.write_text("outside")
        with self.assertRaises(ValueError):
            self.context.complete({}, [{"name": "bad", "path": "../outside", "sha256": hashlib.sha256(b"outside").hexdigest(), "size": 7}])
        artifact = self.context.artifact("data", source)
        (self.context.output / artifact["path"]).write_text("corrupted")
        with self.assertRaises(ValueError):
            self.context.complete({}, [artifact])

    def test_publication_failure_cannot_create_receipt(self):
        with patch("resmgr.worker.os.fsync", side_effect=OSError("sync failed")):
            with self.assertRaises(OSError):
                self.context.complete({})
        self.assertFalse((self.context.output / "result.json").exists())

    def test_attempt_directory_cannot_be_reused(self):
        self.context.checkpoint({})
        with self.assertRaises(ValueError):
            WorkerContext({**self.context.context, "generation": 3})

    def test_resource_wire_names_match_rust(self):
        task = command_task("t", ["python", "worker.py"], "/home/user/work", gpu_vram_mib={"GPU-test": 100})
        self.assertEqual(task["resources"]["gpu_memory_mib"], {"GPU-test": 100})
        self.assertFalse(task["replay_safe"])
        self.assertFalse(task["single_process"])
        self.assertFalse(task["no_escape"])


if __name__ == "__main__":
    unittest.main()
