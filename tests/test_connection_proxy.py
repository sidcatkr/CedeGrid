"""Public validation transport guards; no public listener or workload is started."""
import asyncio
import copy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import connection_proxy as proxy
from runtime_config import runtime_toml


class PublicProxyTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='.cedegrid-proxy-test-', dir=Path.home())
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in ('ca.pem', 'server.pem'):
            (self.root / name).write_text(name)
        self.deployment = {'listen': '127.0.0.1:19002',
                           'tls': {'ca_cert': str(self.root / 'ca.pem'), 'certificate': str(self.root / 'server.pem')},
                           'clients': {'a' * 64: {'role': 'operator'}, 'b' * 64: {'role': 'node', 'node_id': 'burst'}}}
        self.path = self.root / 'coordinator.toml'; self.path.write_text(runtime_toml(self.deployment, 'coordinator'))
        self.config = {'execution_approved': True, 'listen': ['192.0.2.10', 19001],
                       'target': ['127.0.0.1', 19002], 'duration_seconds': 1, 'max_connections': 2,
                       'max_forward_bytes': 4, 'output': str(self.root / 'output'),
                       'public_listener': {'enabled': True, 'source_ips': ['198.51.100.20'],
                           'coordinator_deployment': str(self.path), 'coordinator_sha256': self.digest(self.path),
                           'ca_certificate_sha256': self.digest(self.root / 'ca.pem'),
                           'server_certificate_sha256': self.digest(self.root / 'server.pem')}}

    def digest(self, path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def test_unapproved_public_wildcard_target_and_oversized_bounds_rejected(self):
        proxy.validate(self.config)
        for field, value in [('listen', ['0.0.0.0', 19001]), ('target', ['192.0.2.20', 19002]),
                             ('duration_seconds', 1401), ('max_forward_bytes', 2 * 1024**3 + 1)]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                proxy.validate({**self.config, field: value})
        config = copy.deepcopy(self.config); config.pop('public_listener')
        with self.assertRaises(ValueError): proxy.validate(config)
        config = copy.deepcopy(self.config); config['public_listener']['enabled'] = False
        with self.assertRaises(ValueError): proxy.validate(config)
        config = copy.deepcopy(self.config); config['public_listener']['source_ips'] = []
        with self.assertRaises(ValueError): proxy.validate(config)

    def test_changed_server_certificate_configuration_or_roles_is_refused(self):
        (self.root / 'server.pem').write_text('different certificate')
        with self.assertRaisesRegex(ValueError, 'pin changed'): proxy.validate(self.config)
        (self.root / 'server.pem').write_text('server.pem')
        self.deployment['clients'] = {}
        self.path.write_text(runtime_toml(self.deployment, 'coordinator'))
        with self.assertRaisesRegex(ValueError, 'pin changed'): proxy.validate(self.config)
        self.config['public_listener']['coordinator_sha256'] = self.digest(self.path)
        with self.assertRaisesRegex(ValueError, 'authenticated'): proxy.validate(self.config)

    def test_listener_must_be_observed_on_actual_local_interface(self):
        with patch.object(proxy.subprocess, 'run', return_value=SimpleNamespace(stdout=json.dumps([
            {'addr_info': [{'local': '192.0.2.11'}]}]))):
            with self.assertRaisesRegex(ValueError, 'local interface'): proxy.verify_local_listener(self.config)
        with patch.object(proxy.subprocess, 'run', return_value=SimpleNamespace(stdout=json.dumps([
            {'addr_info': [{'local': '192.0.2.10'}]}]))):
            proxy.verify_local_listener(self.config)

    async def run_fake_transport(self, source, chunks):
        class Writer:
            def __init__(self): self.data = b''; self.closed = False
            def get_extra_info(self, name): return (source, 12345)
            def write(self, data): self.data += data
            async def drain(self): pass
            def close(self): self.closed = True
            async def wait_closed(self): pass
        incoming_writer, target_writer = Writer(), Writer()
        source_reader = SimpleNamespace(read=AsyncMock(side_effect=chunks))
        async def blocked_read(_): await asyncio.Future()
        target_reader = SimpleNamespace(read=blocked_read)
        upstream = AsyncMock(return_value=(target_reader, target_writer))
        server = SimpleNamespace(close=lambda: None, wait_closed=AsyncMock())
        handles = []
        async def listen(handler, *args, **kwargs):
            handles.append(asyncio.create_task(handler(source_reader, incoming_writer)))
            if source != '198.51.100.20':
                asyncio.get_running_loop().call_later(.02, lambda: (self.root / 'output/STOP').touch())
            return server
        loop = asyncio.get_running_loop()
        with patch.object(proxy, 'verify_local_listener'), patch.object(proxy.asyncio, 'start_server', side_effect=listen), \
             patch.object(proxy.asyncio, 'open_connection', upstream), \
             patch.object(loop, 'add_signal_handler'), patch.object(loop, 'remove_signal_handler'):
            result = await proxy.run(self.config)
            await asyncio.gather(*handles, return_exceptions=True)
        return result, upstream, incoming_writer, target_writer

    async def test_disallowed_source_is_closed_before_upstream_connection(self):
        result, upstream, incoming, target = await self.run_fake_transport('198.51.100.99', [b'private bytes'])
        upstream.assert_not_called()
        self.assertTrue(incoming.closed)
        self.assertEqual(result['connections_rejected'], 1)
        self.assertEqual(result['bytes_forwarded'], 0)
        self.assertEqual(target.data, b'')

    async def test_hard_byte_limit_stops_before_excess_payload_is_forwarded(self):
        result, upstream, incoming, target = await self.run_fake_transport('198.51.100.20', [b'abcd', b'excess'])
        upstream.assert_awaited_once()
        self.assertEqual(target.data, b'abcd')
        self.assertEqual(result['bytes_forwarded'], 4)
        self.assertEqual(result['status'], 'byte_budget_exhausted')
        self.assertEqual(result['remaining_owned_connections'], 0)
        self.assertTrue(incoming.closed and target.closed)


if __name__ == '__main__':
    unittest.main()
