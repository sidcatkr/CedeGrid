import hashlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from cedegrid import Client, command_task, spawn_managed, SpawnUncertain
from cedegrid.process import ManagedChild, _SupervisorClient, _InvalidReply
from cedegrid.client import _TransferPacer


class _FakeClient:
    def __init__(self, replies):
        self.replies, self.calls = iter(replies), []
    def request(self, op, **fields):
        self.calls.append((op, fields))
        reply = next(self.replies)
        if isinstance(reply, BaseException):
            raise reply
        return reply


class _FakeReaderSocket:
    def __init__(self, payload):
        self.payload = io.BytesIO(payload)

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc_value, _traceback):
        return False

    def readline(self, length=-1):
        return self.payload.readline(length)


class _FakeSocket:
    def __init__(self, payload):
        self.payload = payload

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc_value, _traceback):
        return False

    def settimeout(self, _timeout):
        pass

    def connect(self, _endpoint):
        pass

    def sendall(self, _data):
        pass

    def recv(self, length):
        part, self.payload = self.payload[:length], self.payload[length:]
        return part


class _RecordingTLSContext:
    def __init__(self, captured, *, require_existing_files=False):
        self.captured = captured
        self.require_existing_files = require_existing_files
        self.minimum_version = None

    def load_cert_chain(self, certificate, private_key):
        self.captured.update(certificate=str(certificate), private_key=str(private_key))
        if self.require_existing_files:
            if not Path(certificate).is_file() or not Path(private_key).is_file():
                raise OSError("missing TLS identity file")


class FakeTLSConfig:
    def __init__(self, require_existing_files=False):
        self.require_existing_files = require_existing_files
        self.received = {}

    def create_context(self, cafile=None):
        self.received["cafile"] = cafile
        return _RecordingTLSContext(self.received, require_existing_files=self.require_existing_files)


class ManagedChildTests(unittest.TestCase):
    def test_explicit_assignment_and_child_contract(self):
        for limit, single, escape in [(1, True, True), (1, False, False), (9, False, True), (-1, False, True)]:
            with self.assertRaises(ValueError):
                command_task('t', ['worker'], '/home/user', managed_child_limit=limit, single_process=single, no_escape=escape)
        task = command_task('t', ['worker'], '/home/user', managed_child_limit=2, single_process=False, no_escape=True)
        self.assertEqual(task['managed_child_limit'], 2)
        with self.assertRaises(ValueError):
            spawn_managed(['child'])
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, 'did not authorize'):
                spawn_managed(['child'], single_process=True, no_escape=True)

    def test_local_supervisor_identity_without_worker_publication_context(self):
        environment = {'CEDEGRID_SUPERVISOR_SOCKET':'/tmp/supervisor.sock',
                       'CEDEGRID_SUPERVISOR_TOKEN':'private-token',
                       'CEDEGRID_NAMESPACE_ID':'namespace','CEDEGRID_SESSION_ID':'session',
                       'CEDEGRID_ASSIGNMENT_ID':'assignment','CEDEGRID_ATTEMPT_GENERATION':'9007199254740993'}
        with patch.dict(os.environ, environment, clear=True):
            client = _SupervisorClient.from_env()
        self.assertEqual(client.identity, {'namespace_id':'namespace','session_id':'session',
                                          'assignment_id':'assignment','generation':9007199254740993})
        for missing in ['CEDEGRID_NAMESPACE_ID','CEDEGRID_SESSION_ID','CEDEGRID_ASSIGNMENT_ID','CEDEGRID_ATTEMPT_GENERATION']:
            with patch.dict(os.environ, {key:value for key,value in environment.items() if key!=missing}, clear=True):
                with self.assertRaises(ValueError): _SupervisorClient.from_env()

    def test_lost_spawn_reply_preserves_deduplication_identity(self):
        for error in [TimeoutError(), _InvalidReply('EOF')]:
            fake = _FakeClient([error, {'child_id': 'c', 'ok': True, 'state': 'running'}])
            with patch.object(_SupervisorClient, 'from_env', return_value=fake):
                with self.assertRaises(SpawnUncertain) as caught:
                    spawn_managed(['child'], single_process=True, no_escape=True)
                child = spawn_managed(['child'], single_process=True, no_escape=True, request_id=caught.exception.request_id)
            self.assertEqual(child.child_id, 'c')
            self.assertEqual(fake.calls[0][1]['request_id'], fake.calls[1][1]['request_id'])
            self.assertNotIn('pid', fake.calls[0][1])

    def test_spawn_managed_rejects_invalid_argv_containers_and_fields(self):
        for argv in ('echo', b'echo', bytearray(b'echo'), [''], ['', 'arg'], ['child', None], ['bad\x00arg']):
            with self.assertRaises(ValueError):
                spawn_managed(argv, single_process=True, no_escape=True)
        fake = _FakeClient([{'child_id': 'c', 'ok': True, 'state': 'running'}])
        with patch.object(_SupervisorClient, 'from_env', return_value=fake):
            child = spawn_managed(('child', ''), single_process=True, no_escape=True)
        self.assertEqual(child.child_id, 'c')
        self.assertEqual(fake.calls[0][1]['argv'], ['child', ''])

    def test_spawn_reply_without_child_id_becomes_uncertain(self):
        fake = _FakeClient([{'state': 'running'}])
        with patch.object(_SupervisorClient, 'from_env', return_value=fake):
            with self.assertRaises(SpawnUncertain):
                spawn_managed(['child'], single_process=True, no_escape=True)

    def test_spawn_request_error_remains_definite_rejection(self):
        fake = _FakeClient([RuntimeError('supervisor rejected request')])
        with patch.object(_SupervisorClient, 'from_env', return_value=fake):
            with self.assertRaises(RuntimeError):
                spawn_managed(['child'], single_process=True, no_escape=True)

    def test_timeout_and_reconciliation_never_signal(self):
        for reply, expected in [({'state': 'running'}, TimeoutError), ({'state': 'needs_reconciliation'}, RuntimeError)]:
            fake = _FakeClient([reply])
            with self.assertRaises(expected):
                ManagedChild(fake, 'c').wait(timeout=0)
            self.assertEqual([op for op, _ in fake.calls], ['status'])

    def test_stop_is_request_and_wait_requires_release(self):
        fake = _FakeClient([{'state': 'draining'}, {'state': 'released', 'exit_code': 0}])
        child = ManagedChild(fake, 'c')
        self.assertEqual(child.stop()['state'], 'draining')
        self.assertEqual(child.wait(timeout=1)['exit_code'], 0)
        self.assertEqual(fake.calls, [('stop', {'child_id': 'c'}), ('status', {'child_id': 'c'})])

    def test_supervisor_reply_parsing_rejects_invalid_reply_payloads(self):
        client = _SupervisorClient('/tmp/supervisor.sock', 'token', identity={'namespace_id':'ns','session_id':'s','assignment_id':'a','generation':1})
        for reply in [b'not-json\n', b'{"ok": "yes"}\n', b'x' * (3 * 1024 * 1024 + 1)]:
            with patch('cedegrid.process.socket.socket', return_value=_FakeSocket(reply)):
                with self.assertRaises(_InvalidReply):
                    client.request('ping')


class PacingTests(unittest.TestCase):
    def test_serialized_hex_expansion_is_charged_without_burst(self):
        clock = [10.]
        waits = []
        def sleep(seconds):
            waits.append(seconds)
            clock[0] += seconds
        pacer = _TransferPacer(1000, clock=lambda: clock[0], sleep=sleep)
        pacer.account(1800)
        pacer.account(900)
        self.assertEqual(waits, [2., 1.])
        self.assertEqual(clock[0], 13.)

    def test_pacing_cannot_be_disabled_by_invalid_configuration(self):
        for rate in [-1, float('inf'), float('nan')]:
            with self.assertRaises(ValueError):
                _TransferPacer(rate)


class AttemptBoundTests(unittest.TestCase):
    def test_explicit_attempt_cap_preserves_legacy_omission(self):
        default=command_task('t',['worker'],'/home/user')
        self.assertNotIn('max_attempts',default)
        bounded=command_task('t',['worker'],'/home/user',max_attempts=4)
        self.assertEqual(bounded['max_attempts'],4)
        for invalid in [0,-1,True,1.5]:
            with self.assertRaises(ValueError):
                command_task('t',['worker'],'/home/user',max_attempts=invalid)

    def test_task_request_fields_reject_boolean_numeric_values(self):
        for field in ['cpu_millicores', 'ram_mib']:
            with self.assertRaises(ValueError):
                if field == 'cpu_millicores':
                    command_task('t',['worker'],'/home/user',cpu_millicores=True)
                else:
                    command_task('t',['worker'],'/home/user',ram_mib=True)
        with self.assertRaises(ValueError):
            command_task('t',['worker'],'/home/user',managed_child_limit=True)
        with self.assertRaises(ValueError):
            command_task('t',['worker'],'/home/user',gpu_vram_mib={'GPU-test': True})


class ClientFromConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()

    def tearDown(self):
        self.temp.cleanup()

    def _write_config(self, path: Path, *, ca: Path, certificate: Path, private_key: Path):
        config = {
            'endpoint': 'https://127.0.0.1:9443',
            'tls': {
                'ca_cert': str(ca),
                'certificate': str(certificate),
                'private_key': str(private_key),
            },
        }
        path.write_text('config_version = 1\nendpoint = ' + json.dumps(config['endpoint']) + '\n[tls]\n' + ''.join(key + ' = ' + json.dumps(value, ensure_ascii=False) + '\n' for key, value in config['tls'].items()), encoding='utf-8')

    def _make_fake_files(self, directory: Path):
        tls = directory / 'tls'
        tls.mkdir(parents=True, exist_ok=True)
        (tls / 'ca.pem').write_text('ca', encoding='utf-8')
        (tls / 'server.pem').write_text('server', encoding='utf-8')
        (tls / 'server.key').write_text('key', encoding='utf-8')
        return tls / 'ca.pem', tls / 'server.pem', tls / 'server.key'

    def _load_client(self, config_path, *, require_existing_files=False):
        captured = {}
        fake = FakeTLSConfig(require_existing_files=require_existing_files)
        with patch('cedegrid.client.ssl.create_default_context', side_effect=fake.create_context), \
                patch('cedegrid.client.urllib.request.build_opener', return_value=object()):
            client = Client.from_config(config_path)
            captured.update(fake.received)
            return client, captured

    def test_from_config_relative_tls_paths_resolve_from_config_file(self):
        workspace = self.root / 'working space 📁' / 'nested'
        workspace.mkdir(parents=True)
        ca, cert, key = self._make_fake_files(workspace)
        config = workspace / 'config.json'
        self._write_config(config, ca=Path('tls/ca.pem'), certificate=Path('tls/server.pem'), private_key=Path('tls/server.key'))
        client, captured = self._load_client(config)
        self.assertIsNotNone(client)
        self.assertIn('cafile', captured)
        self.assertEqual(captured['cafile'], str(ca))

    def test_from_config_missing_config_file(self):
        with self.assertRaises(FileNotFoundError):
            Client.from_config(self.root / 'missing.json')

    def test_from_config_preserves_absolute_tls_paths(self):
        workspace = self.root / 'absolute set'
        workspace.mkdir()
        ca, cert, key = self._make_fake_files(workspace)
        config = self.root / 'absolute.json'
        self._write_config(config, ca=ca, certificate=cert, private_key=key)
        _, captured = self._load_client(config)
        self.assertEqual(captured['cafile'], str(ca))
        self.assertEqual(captured['certificate'], str(cert))
        self.assertEqual(captured['private_key'], str(key))

    def test_from_config_spaces_and_unicode_paths_round_trip(self):
        workspace = self.root / 'unicode 路径 🌟'
        workspace.mkdir()
        ca, cert, key = self._make_fake_files(workspace)
        config = workspace / '配置' / 'connector.json'
        config.parent.mkdir(parents=True)
        self._write_config(config, ca=Path('../tls/ca.pem'), certificate=Path('../tls/server.pem'), private_key=Path('../tls/server.key'))
        _, captured = self._load_client(config)
        self.assertEqual(captured['cafile'], str(ca))

    def test_from_config_reports_missing_files(self):
        workspace = self.root / 'missing'
        workspace.mkdir()
        # Intentionally leave certificate file absent.
        ca = workspace / 'tls' / 'ca.pem'
        cert = workspace / 'tls' / 'server.pem'
        key = workspace / 'tls' / 'server.key'
        ca.parent.mkdir(parents=True)
        ca.write_text('ca', encoding='utf-8')
        config = workspace / 'client.json'
        self._write_config(config, ca=ca, certificate=cert, private_key=key)
        with self.assertRaises(OSError):
            self._load_client(config, require_existing_files=True)

    def test_from_config_symlink_base_preserves_realpath(self):
        real = self.root / 'real config'
        link = self.root / 'link config'
        real.mkdir()
        ca, cert, key = self._make_fake_files(real)
        config = real / 'client.json'
        self._write_config(config, ca=Path('tls/ca.pem'), certificate=Path('tls/server.pem'), private_key=Path('tls/server.key'))
        link.symlink_to(real)
        _, captured = self._load_client(link / 'client.json')
        self.assertEqual(captured['cafile'], str(real / 'tls' / 'ca.pem'))


class ClientDownloadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()

    def tearDown(self):
        self.temp.cleanup()

    def _new_client_with_data(self, data: bytes):
        client = Client.__new__(Client)
        def request(op, **fields):
            self.assertEqual(op, 'read_artifact')
            self.assertEqual(fields['sha256'], hashlib.sha256(data).hexdigest())
            offset = fields['offset']
            payload = data[offset:fields['offset'] + fields['max_bytes']]
            return {'data_hex': payload.hex()}
        client.request = request
        return client

    @unittest.skipIf(os.name == 'nt', 'strict directory durability requires POSIX')
    def test_download_creates_nested_destination_ancestors_with_durable_mkdir(self):
        from cedegrid.worker import durable_mkdir as real_durable_mkdir
        payload = b'download test payload'
        client = self._new_client_with_data(payload)
        destination = self.root / 'nested 目录' / 'layer one' / 'layer two' / 'artifact.bin'
        seen = []

        def recording_durable_mkdir(path, **options):
            seen.append(Path(path))
            real_durable_mkdir(path, **options)

        with patch('cedegrid.worker.durable_mkdir', side_effect=recording_durable_mkdir):
            result = client.download({'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, destination)
        self.assertEqual(result, destination)
        self.assertEqual(destination.read_bytes(), payload)
        self.assertEqual(seen, [destination.parent])

    @unittest.skipIf(os.name == 'nt', 'strict directory durability requires POSIX')
    def test_download_repeated_and_empty_artifacts(self):
        payload = b''
        client = self._new_client_with_data(payload)
        empty = self.root / 'empty' / 'artifact.bin'
        empty.parent.mkdir(parents=True)
        self.assertEqual(client.download({'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, empty), empty)
        self.assertEqual(empty.read_bytes(), payload)
        empty_again = client.download({'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, empty)
        self.assertEqual(empty_again.read_bytes(), payload)

    @unittest.skipIf(os.name == 'nt', 'strict directory durability requires POSIX')
    def test_download_rejects_conflicting_existing_destination(self):
        payload = b'accepted artifact'
        destination = self.root / 'conflict' / 'artifact.bin'
        destination.parent.mkdir(parents=True)
        destination.write_bytes(b'preserve')
        client = self._new_client_with_data(payload)
        with self.assertRaises(ValueError):
            client.download({'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, destination)
        self.assertEqual(destination.read_bytes(), b'preserve')

    @unittest.skipIf(os.name == 'nt', 'strict directory durability requires POSIX')
    def test_download_surface_fsync_errors_and_cleanup(self):
        payload = b'unused'
        client = self._new_client_with_data(payload)
        destination = self.root / 'sync-fail' / 'artifact.bin'
        with patch('cedegrid.client.os.fsync', side_effect=OSError('fsync blocked')):
            with self.assertRaises(OSError):
                client.download({'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, destination)
        self.assertFalse(destination.exists())

    @unittest.skipIf(os.name == 'nt', 'strict directory durability requires POSIX')
    def test_download_reports_uncertain_after_publication_parent_sync_fails(self):
        from cedegrid import DownloadUncertain
        payload = b'published before failed directory durability'
        destination = self.root / 'uncertain.bin'
        client = self._new_client_with_data(payload)
        expected = hashlib.sha256(payload).hexdigest()
        with patch('cedegrid.worker.sync_directory', side_effect=OSError('directory sync failed')):
            with self.assertRaises(DownloadUncertain) as failure:
                client.download({'sha256': expected, 'size': len(payload)}, destination)
        self.assertEqual(destination.read_bytes(), payload)
        self.assertEqual(failure.exception.digest, expected)
        self.assertEqual(failure.exception.destination, str(destination))
        self.assertTrue(failure.exception.operation_id)
        self.assertEqual(failure.exception.code, 'ERR_CEDEGRID_DOWNLOAD_UNCERTAIN')

    def test_portable_download_explicitly_skips_directory_sync(self):
        payload = b'portable output'
        destination = self.root / 'portable' / 'nested' / 'output.bin'
        with patch('cedegrid.worker.sync_directory', side_effect=AssertionError('portable requested')):
            self._new_client_with_data(payload).download(
                {'sha256': hashlib.sha256(payload).hexdigest(), 'size': len(payload)}, destination,
                durability='portable')
        self.assertEqual(destination.read_bytes(), payload)

    def test_windows_strict_download_rejects_before_filesystem_mutation(self):
        from cedegrid import ValidationError
        destination = self.root / 'never-created' / 'output.bin'
        with patch('cedegrid.client.os.name', 'nt'):
            with self.assertRaises(ValidationError):
                self._new_client_with_data(b'').download(
                    {'sha256': hashlib.sha256(b'').hexdigest(), 'size': 0}, destination)
        self.assertFalse(destination.parent.exists())


if __name__ == '__main__':
    unittest.main()
