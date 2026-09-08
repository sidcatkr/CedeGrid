"""Regression checks for the scoped SDK repairs, not full release qualification."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from resmgr.client import Client, RemoteError, command_task
from resmgr.worker import sync_directory


class ClientValidationTests(unittest.TestCase):
    def test_invalid_task_fields_fail_during_construction(self):
        invalid = [
            {"task_id": "../bad"}, {"task_id": "a" * 129},
            {"argv": "echo"}, {"argv": ["echo", "a\0b"]}, {"argv": [1]},
            {"cwd": "relative"}, {"cwd": 1}, {"env": {"A=B": "x"}},
            {"env": {"A": 2}}, {"cpu_millicores": -1}, {"cpu_millicores": True},
            {"cpu_millicores": 1 << 64}, {"ram_mib": 1 << 64}, {"ram_mib": 0.5},
            {"gpu_vram_mib": {"GPU-test": -1}}, {"gpu_vram_mib": []},
            {"managed_child_limit": True}, {"managed_child_limit": 9},
            {"max_attempts": 1 << 32}, {"max_attempts": True}, {"max_attempts": 0},
            {"single_process": 1}, {"required_controls": "pidfd"},
            {"required_controls": ["x", "x"]}, {"input_artifacts": {}},
        ]
        for override in invalid:
            with self.subTest(override=override):
                values = dict(task_id="task", argv=["echo"], cwd="/tmp")
                values.update(override)
                with self.assertRaises(ValueError):
                    command_task(**values)

    def test_unsigned_integer_boundaries_are_exact(self):
        task = command_task("task", ["echo", ""], "/tmp", cpu_millicores=(1 << 64) - 1,
                            ram_mib=9007199254740993, max_attempts=(1 << 32) - 1)
        self.assertEqual(json.loads(json.dumps(task))["resources"]["ram_mib"], 9007199254740993)
        self.assertEqual(task["resources"]["cpu_millicores"], (1 << 64) - 1)

    def test_invalid_origin_and_settings_fail_before_tls_access(self):
        invalid = [
            {"endpoint": "http://example.test"}, {"endpoint": "https://@example.test"},
            {"endpoint": "https://user:pass@example.test"}, {"endpoint": "https://example.test/path"},
            {"endpoint": "https://example.test?"}, {"endpoint": "https://example.test#"},
            {"endpoint": "https://example.test/#fragment"}, {"endpoint": "https://example.test/?query"},
            {"endpoint": "https://example.test:0"}, {"endpoint": "https://example.test:65536"},
            {"endpoint": "https://exam\tple.test"}, {"endpoint": "https://example.test\\path"},
            {"timeout": True}, {"timeout": float("nan")}, {"timeout": 0},
            {"max_transfer_bytes_per_second": False}, {"max_transfer_bytes_per_second": float("inf")},
        ]
        for override in invalid:
            with self.subTest(override=override), patch("resmgr.client.ssl.create_default_context") as tls:
                args = dict(endpoint="https://example.test", ca="unused", certificate="unused", private_key="unused")
                args.update(override)
                with self.assertRaises(ValueError):
                    Client(**args)
                tls.assert_not_called()

    def test_config_relative_tls_paths_use_canonical_config_parent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config_dir = root / "config with spaces 한글"
            config_dir.mkdir()
            config = config_dir / "client.json"
            config.write_text(json.dumps({"endpoint": "https://example.test", "tls": {
                "ca_cert": "keys/ca.pem", "certificate": "keys/client.pem", "private_key": "keys/key.pem"}}))
            with patch.object(Client, "__init__", return_value=None) as constructor:
                Client.from_config(config)
            self.assertEqual(constructor.call_args.kwargs["ca"], config_dir.resolve() / "keys/ca.pem")
            alias = root / "elsewhere.json"
            alias.symlink_to(config)
            with patch.object(Client, "__init__", return_value=None) as constructor:
                Client.from_config(alias)
            self.assertEqual(constructor.call_args.kwargs["private_key"], config_dir.resolve() / "keys/key.pem")

    def test_config_does_not_expand_environment_or_tilde(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "client.json"
            config.write_text(json.dumps({"endpoint": "https://example.test", "tls": {
                "ca_cert": "~/ca.pem", "certificate": "$HOME/cert.pem", "private_key": "${HOME}/key.pem"}}))
            with patch.object(Client, "__init__", return_value=None) as constructor:
                Client.from_config(config)
            self.assertEqual(constructor.call_args.kwargs["ca"], config.parent / "~/ca.pem")
            self.assertEqual(constructor.call_args.kwargs["certificate"], config.parent / "$HOME/cert.pem")

    def test_unknown_and_duplicate_config_fields_fail_before_constructor(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "client.json"
            for value in ('{"endpoint":"x","endpoint":"y","tls":{}}',
                          '{"endpoint":"x","tls":{},"unknown":1}'):
                config.write_text(value)
                with patch.object(Client, "__init__", return_value=None) as constructor:
                    with self.assertRaises(ValueError):
                        Client.from_config(config)
                    constructor.assert_not_called()

    def test_bad_public_method_arguments_do_not_reach_transport(self):
        client = object.__new__(Client)
        actions = [lambda: client.cancel("../bad"), lambda: client.status(1),
                   lambda: client.resume("task", side_effects_reconciled=1),
                   lambda: client.drain_node("node", drain=1),
                   lambda: client.put_pool("pool", ["node"], max_workers=True),
                   lambda: client.put_pool("pool", ["node"], max_workers=1, min_workers=2),
                   lambda: client.submit("job", "pool", [], priority=True)]
        for action in actions:
            with self.subTest(action=action), patch.object(client, "request") as request:
                with self.assertRaises(ValueError):
                    action()
                request.assert_not_called()


class DownloadDurabilityTests(unittest.TestCase):
    def test_new_ancestors_are_synced(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "a" / "b" / "output.bin"
            data = b"test data"
            artifact = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}
            client = object.__new__(Client)
            seen = []
            def sync(path):
                seen.append(Path(path))
                sync_directory(path)
            with patch.object(client, "request", return_value={"kind": "chunk", "data_hex": data.hex()}), patch("resmgr.worker.sync_directory", side_effect=sync):
                client.download(artifact, destination)
            self.assertEqual(destination.read_bytes(), data)
            self.assertIn(root, seen)
            self.assertIn(root / "a", seen)
            self.assertIn(root / "a" / "b", seen)

    def test_interrupted_download_preserves_old_file_and_cleans_temporary(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "output.bin"
            destination.write_bytes(b"old")
            artifact = {"sha256": hashlib.sha256(b"new").hexdigest(), "size": 3}
            client = object.__new__(Client)
            with patch.object(client, "request", side_effect=OSError("test interruption")):
                with self.assertRaises(OSError):
                    client.download(artifact, destination)
            self.assertEqual(destination.read_bytes(), b"old")
            self.assertEqual(list(destination.parent.glob("*.download")), [])
            self.assertEqual(list(destination.parent.glob(".*.download")), [])

    def test_bad_artifact_does_not_create_output_directories(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "must-not-exist" / "output.bin"
            client = object.__new__(Client)
            with self.assertRaises(ValueError):
                client.download({"sha256": "bad", "size": True}, output)
            self.assertFalse(output.parent.exists())

    def test_directory_sync_failure_stops_before_transfer(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "a" / "b" / "output.bin"
            client = object.__new__(Client)
            artifact = {"sha256": hashlib.sha256(b"data").hexdigest(), "size": 4}
            with patch("resmgr.worker.sync_directory", side_effect=OSError("test fsync failure")), patch.object(client, "request") as request:
                with self.assertRaises(OSError):
                    client.download(artifact, output)
                request.assert_not_called()
            self.assertFalse(output.exists())
            self.assertFalse(output.parent.exists())


if __name__ == "__main__":
    unittest.main()
