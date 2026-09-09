"""Real bounded opaque command streams, lifecycle cleanup and admission guards."""
import asyncio
import hashlib
import json
from pathlib import Path
import signal
import socket
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'tools'))
import connection_proxy as proxy
from runtime_config import runtime_toml


class StreamProxyTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='.cedegrid-stream-test-', dir=Path.home())
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.root.chmod(0o700)
        self.program = self.root / 'stream'
        self.write_program('import os\nwhile data := os.read(0, 65536): os.write(1, data)\n')
        for name in ('ca.pem', 'server.pem'):
            (self.root / name).write_text(name)
        self.deployment = self.root / 'coordinator.toml'
        self.deployment.write_text(runtime_toml({'listen': '127.0.0.1:19002',
            'tls': {'ca_cert': str(self.root / 'ca.pem'), 'certificate': str(self.root / 'server.pem')},
            'clients': {'a' * 64: {'role': 'operator'}, 'b' * 64: {'role': 'node', 'node_id': 'burst'}}}, 'coordinator'))
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0)); port = reservation.getsockname()[1]
        self.config = {'execution_approved': True, 'listen': ['127.0.0.1', port],
            'target': ['127.0.0.1', 19002], 'duration_seconds': 5, 'max_connections': 2,
            'max_forward_bytes': 1024, 'output': str(self.root / 'output'),
            'stream_command': {'argv': [str(self.program)], 'executable_sha256': self.digest(self.program),
                               'single_process': True, 'max_launches': 4},
            'authenticated_target': {'coordinator_deployment': str(self.deployment),
                'coordinator_sha256': self.digest(self.deployment),
                'ca_certificate_sha256': self.digest(self.root / 'ca.pem'),
                'server_certificate_sha256': self.digest(self.root / 'server.pem')}}

    def write_program(self, code):
        self.program.write_text('#!' + sys.executable + '\n' + code)
        self.program.chmod(0o700)

    @staticmethod
    def digest(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    async def start(self):
        self.driver = asyncio.create_task(proxy.run(self.config))
        async def cleanup():
            if not self.driver.done():
                (self.root / 'output/STOP').touch()
                await asyncio.wait_for(self.driver, 5)
        self.addAsyncCleanup(cleanup)
        for _ in range(100):
            if (self.root / 'output/status.json').exists(): break
            if self.driver.done(): await self.driver
            await asyncio.sleep(.01)
        return await asyncio.open_connection(*self.config['listen'])

    async def finish(self, writer):
        writer.close(); await writer.wait_closed()
        (self.root / 'output/STOP').touch()
        result = await asyncio.wait_for(self.driver, 5)
        self.assertEqual(result['remaining_owned_connections'], 0)
        self.assertTrue(all(child['reaped'] for child in result['stream_children']))
        return result

    def test_fixed_program_target_and_resource_bounds_are_required(self):
        proxy.validate(self.config)
        variants = []
        for key, value in [('listen', ['0.0.0.0', 19001]), ('duration_seconds', 1401),
                           ('max_forward_bytes', None), ('max_connections', 9),
                           ('authenticated_target', None)]:
            variants.append({**self.config, key: value})
        for key, value in [('argv', ['stream']), ('argv', [str(self.program), 'bad\nargument']),
                           ('single_process', False), ('max_launches', 4097),
                           ('executable_sha256', '0' * 64)]:
            variants.append({**self.config, 'stream_command': {**self.config['stream_command'], key: value}})
        for config in variants:
            with self.subTest(config=config), self.assertRaises(ValueError): proxy.validate(config)
        (self.root / 'server.pem').write_text('replaced')
        with self.assertRaisesRegex(ValueError, 'pin changed'): proxy.validate(self.config)

    async def test_real_opaque_round_trip_reaps_only_its_direct_child(self):
        reader, writer = await self.start()
        payload = b'\x16\x03\x01opaque end-to-end encrypted bytes'
        writer.write(payload); await writer.drain()
        self.assertEqual(await asyncio.wait_for(reader.readexactly(len(payload)), 2), payload)
        result = await self.finish(writer)
        self.assertEqual(result['bytes_forwarded'], 2 * len(payload))
        self.assertEqual(result['stream_launches'], 1)
        self.assertEqual(result['status'], 'stopped')
        self.assertEqual(result['stream_failures'], 0)

    async def test_byte_budget_stops_and_reaps_command(self):
        self.config['max_forward_bytes'] = 4
        reader, writer = await self.start()
        writer.write(b'oversized bytes'); await writer.drain()
        self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
        result = await self.finish(writer)
        self.assertEqual(result['status'], 'byte_budget_exhausted')
        self.assertLessEqual(result['bytes_forwarded'], 4)

    async def test_early_command_error_is_reported_and_reaped(self):
        self.write_program('raise SystemExit(43)\n')
        self.config['stream_command']['executable_sha256'] = self.digest(self.program)
        reader, writer = await self.start()
        self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
        result = await self.finish(writer)
        self.assertEqual(result['status'], 'failed_stream_command')
        self.assertEqual(result['stream_children'][0]['returncode'], 43)
        self.assertEqual(result['stream_failures'], 1)

    async def test_proxy_stop_escalates_and_reaps_term_ignoring_child(self):
        self.write_program('import os, signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nos.write(1, b"ready")\ntime.sleep(60)\n')
        self.config['stream_command']['executable_sha256'] = self.digest(self.program)
        reader, writer = await self.start()
        self.assertEqual(await asyncio.wait_for(reader.readexactly(5), 2), b'ready')
        # Stop the proxy while its stream remains connected: cancellation must
        # await the exclusive reaper, not abandon the helper process.
        (self.root / 'output/STOP').touch()
        result = await asyncio.wait_for(self.driver, 5)
        writer.close(); await writer.wait_closed()
        self.assertEqual(result['remaining_owned_connections'], 0)
        self.assertEqual(result['stream_children'][0]['signals'], ['SIGTERM', 'SIGKILL'])
        self.assertTrue(result['stream_children'][0]['reaped'])

    async def test_failed_spawn_is_counted_without_a_child_claim(self):
        with patch.object(proxy.subprocess, 'Popen', side_effect=OSError('permission denied')):
            reader, writer = await self.start()
            self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
            result = await self.finish(writer)
        self.assertEqual(result['status'], 'failed_stream_command')
        self.assertEqual(result['stream_children'], [])

    async def test_changed_executable_before_connection_refuses_launch(self):
        # Runtime validation is followed by another pin check at actual spawn.
        original = proxy.CommandStream
        def replaced(command, output):
            self.program.write_text('changed')
            return original(command, output)
        with patch.object(proxy, 'CommandStream', side_effect=replaced):
            reader, writer = await self.start()
            self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
            result = await self.finish(writer)
        self.assertEqual(result['status'], 'failed_stream_command')
        self.assertEqual(result['stream_launches'], 0)

    async def test_pidfd_failure_uses_exclusive_child_fallback(self):
        with patch.object(proxy.os, 'pidfd_open', side_effect=PermissionError('unavailable'), create=True), \
             patch.object(proxy.signal, 'pidfd_send_signal', create=True):
            reader, writer = await self.start()
            writer.write(b'fallback'); await writer.drain()
            self.assertEqual(await asyncio.wait_for(reader.readexactly(8), 2), b'fallback')
            result = await self.finish(writer)
        self.assertEqual(result['stream_children'][0]['handle_mode'], 'exclusive_unreaped_direct_child_fallback')
        self.assertTrue(result['stream_children'][0]['reaped'])

    async def test_total_launch_limit_stops_before_extra_process(self):
        self.config['stream_command']['max_launches'] = 1
        reader, writer = await self.start()
        writer.write(b'a'); await writer.drain()
        self.assertEqual(await asyncio.wait_for(reader.readexactly(1), 2), b'a')
        writer.close(); await writer.wait_closed()
        reader, writer = await asyncio.open_connection(*self.config['listen'])
        self.assertEqual(await asyncio.wait_for(reader.read(), 2), b'')
        result = await self.finish(writer)
        self.assertEqual(result['status'], 'stream_launch_budget_exhausted')
        self.assertEqual(result['stream_launches'], 1)


if __name__ == '__main__': unittest.main()
