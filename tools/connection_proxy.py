#!/usr/bin/env python3
"""Bounded TLS-byte proxy for owned connection fault injection.

TLS and client authentication terminate only at the coordinator. The proxy never
terminates TLS or logs payloads. An explicit public listener pins the existing
coordinator configuration and public certificates and admits one source IP.
An optional fixed command connects stdin/stdout to the pinned authenticated
endpoint. It exposes no destination selection, SOCKS, shell or credentials to
local clients. Loopback is not an account boundary: other local users may attempt
the same endpoint, but must supply their own authorized client certificate.
Stop/restart affects only its accepted test
connections; no routing, firewall, SSH configuration, or global settings change.
"""
import argparse
import asyncio
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from urllib.parse import urlsplit
from validation_runtime import atomic_json, inside_home
from runtime_config import read_runtime_config


def public_ip(value):
    address = ipaddress.ip_address(value)
    if address.is_loopback or address.is_unspecified or address.is_multicast:
        raise ValueError('public transport requires exact nonloopback unicast IPs')
    return str(address)


def pinned(path, digest):
    path = inside_home(path)
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise ValueError('public proxy coordinator or certificate pin changed')
    return path


def validate_public(config):
    contract = config['public_listener']
    if contract.get('enabled') is not True:
        raise ValueError('explicit public listener opt-in required')
    public_ip(config['listen'][0])
    sources = contract.get('source_ips')
    if not isinstance(sources, list) or len(sources) != 1:
        raise ValueError('exactly one explicitly approved source IP required')
    public_ip(sources[0])
    if not 0 < float(config['duration_seconds']) <= 1400:
        raise ValueError('public proxy lifetime must not exceed 1400 seconds')
    if type(config.get('max_forward_bytes')) is not int or not 1 <= config['max_forward_bytes'] <= 2 * 1024**3:
        raise ValueError('public forwarding requires a hard byte budget of at most 2 GiB')
    validate_authenticated_target(config, contract)


def validate_authenticated_target(config, contract):
    """Pin the authenticated final endpoint, including opaque command transports."""
    deployment = read_runtime_config(pinned(contract['coordinator_deployment'], contract['coordinator_sha256']))
    endpoint = urlsplit('tcp://' + deployment['listen'])
    if [endpoint.hostname, endpoint.port] != config['target']:
        raise ValueError('proxy target differs from pinned coordinator listen address')
    tls = deployment['tls']
    pinned(tls['ca_cert'], contract['ca_certificate_sha256'])
    pinned(tls['certificate'], contract['server_certificate_sha256'])
    roles = deployment.get('clients')
    if not isinstance(roles, dict) or not roles or any(
        len(key) != 64 or any(c not in '0123456789abcdef' for c in key)
        or not isinstance(role, dict) or role.get('role') not in ('operator', 'node')
        or (role['role'] == 'node' and not role.get('node_id')) for key, role in roles.items()
    ) or {role['role'] for role in roles.values()} != {'operator', 'node'}:
        raise ValueError('pinned coordinator must require explicit authenticated operator and node roles')


def validate_stream_command(config):
    command = config['stream_command']
    argv = command.get('argv')
    if (not isinstance(argv, list) or not 1 <= len(argv) <= 32
            or any(not isinstance(arg, str) or not arg or '\0' in arg or '\n' in arg or '\r' in arg
                   or len(arg) > 4096 for arg in argv)):
        raise ValueError('stream command requires a bounded fixed argv without shell expansion')
    if not Path(argv[0]).is_absolute() or Path(argv[0]).is_symlink():
        raise ValueError('stream executable must be an absolute home file, not a symlink')
    executable = pinned(argv[0], command['executable_sha256'])
    if not os.access(executable, os.X_OK):
        raise ValueError('stream executable is not executable')
    if command.get('single_process') is not True:
        raise ValueError('stream command requires an explicit single-process, no-daemon contract')
    if 'public_listener' in config or not ipaddress.ip_address(config['listen'][0]).is_loopback:
        raise ValueError('stream command listener must remain loopback-only')
    if not 0 < float(config['duration_seconds']) <= 1400:
        raise ValueError('stream command lifetime must not exceed 1400 seconds')
    if type(config.get('max_forward_bytes')) is not int or not 1 <= config['max_forward_bytes'] <= 2 * 1024**3:
        raise ValueError('stream command requires a hard byte budget of at most 2 GiB')
    if type(config.get('max_connections')) is not int or not 1 <= config['max_connections'] <= 8:
        raise ValueError('stream command requires one to eight bounded connections')
    launches = command.get('max_launches', 256)
    if type(launches) is not int or not 1 <= launches <= 4096:
        raise ValueError('stream command requires a bounded total child launch count')
    if not isinstance(config.get('authenticated_target'), dict):
        raise ValueError('stream command requires a pinned authenticated target')
    validate_authenticated_target(config, config['authenticated_target'])


async def pipe_ready(fd, writing=False):
    loop = asyncio.get_running_loop(); ready = loop.create_future()
    def wake():
        if not ready.done(): ready.set_result(None)
    (loop.add_writer if writing else loop.add_reader)(fd, wake)
    try: await ready
    finally: (loop.remove_writer if writing else loop.remove_reader)(fd)


class CommandStream:
    """One exclusive Popen reaper; a trusted direct child cannot daemonize.

    No polling/reaping occurs before pidfd acquisition. The fallback retains its
    unreaped child, so no other process can reuse that PID before signaling.
    A leader handle does not identify descendants. No shell or client arguments
    are evaluated, and stderr/payloads are not retained.
    """
    def __init__(self, command, output):
        if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
            raise RuntimeError('exclusive stream child reaping requires default SIGCHLD handling')
        pinned(command['argv'][0], command['executable_sha256'])
        env = dict(os.environ, TMPDIR=str(output), TMP=str(output), TEMP=str(output),
                   XDG_CACHE_HOME=str(output / 'cache'), PYTHONDONTWRITEBYTECODE='1')
        self.child = subprocess.Popen(command['argv'], shell=False, cwd=output, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            close_fds=True, start_new_session=True)
        self.pidfd = None
        self.mode = 'exclusive_unreaped_direct_child_fallback'
        try:
            if hasattr(os, 'pidfd_open') and hasattr(signal, 'pidfd_send_signal'):
                try:
                    self.pidfd = os.pidfd_open(self.child.pid)
                    signal.pidfd_send_signal(self.pidfd, 0)
                    self.mode = 'pidfd'
                except OSError:
                    if self.pidfd is not None: os.close(self.pidfd)
                    self.pidfd = None
            os.set_blocking(self.child.stdout.fileno(), False)
            os.set_blocking(self.child.stdin.fileno(), False)
        except BaseException:
            # This is still our sole unreaped direct child; no numeric PID adoption.
            self.child.kill(); self.child.wait(timeout=5)
            self.child.stdin.close(); self.child.stdout.close()
            if self.pidfd is not None: os.close(self.pidfd)
            raise
        self.pending = b''

    async def read(self, size):
        fd = self.child.stdout.fileno()
        while True:
            try: return os.read(fd, size)
            except BlockingIOError: await pipe_ready(fd)

    def write(self, data):
        if self.pending: raise RuntimeError('stream write requires bounded drain')
        self.pending = data

    async def drain(self):
        fd = self.child.stdin.fileno()
        while self.pending:
            try: self.pending = self.pending[os.write(fd, self.pending):]
            except BlockingIOError: await pipe_ready(fd, writing=True)

    async def stop(self):
        signals = []
        def send(sig):
            if self.child.poll() is not None: return
            try:
                if self.pidfd is not None: signal.pidfd_send_signal(self.pidfd, sig)
                else: self.child.send_signal(sig)
                signals.append(sig.name)
            except ProcessLookupError: pass
        try:
            self.child.stdin.close(); self.child.stdout.close()
            for sig, grace in ((signal.SIGTERM, .5), (signal.SIGKILL, 2)):
                send(sig); deadline = time.monotonic() + grace
                while self.child.poll() is None and time.monotonic() < deadline:
                    await asyncio.sleep(.02)
                if self.child.returncode is not None: break
            return {'pid': self.child.pid, 'returncode': self.child.returncode,
                    'reaped': self.child.returncode is not None, 'signals': signals,
                    'handle_mode': self.mode, 'descendant_contract': 'single direct process only'}
        finally:
            if self.pidfd is not None: os.close(self.pidfd); self.pidfd = None


def peer_allowed(config, peer):
    if 'public_listener' not in config:
        return True
    try:
        return str(ipaddress.ip_address(peer[0])) in {
            str(ipaddress.ip_address(value)) for value in config['public_listener']['source_ips']}
    except (TypeError, ValueError, IndexError):
        return False


def verify_local_listener(config):
    if 'public_listener' not in config:
        return
    result = subprocess.run(['ip', '-j', 'address', 'show'], capture_output=True, text=True, check=True, timeout=5)
    addresses = {str(ipaddress.ip_address(item['local'])) for interface in json.loads(result.stdout)
                 for item in interface.get('addr_info', []) if 'local' in item}
    if public_ip(config['listen'][0]) not in addresses:
        raise ValueError('public listener IP is not an observed local interface address')


def validate(config):
    if not config.get('execution_approved'): raise ValueError('owned proxy execution is not approved')
    for key in ['listen','target']:
        host,port=config[key]
        if (not ipaddress.ip_address(host).is_loopback and (key == 'target' or 'public_listener' not in config)) or type(port) is not int or not 1024<port<65536:
            raise ValueError('proxy endpoints must be explicit loopback addresses and unprivileged ports')
    if config['listen']==config['target']: raise ValueError('proxy cannot target itself')
    if not 0<float(config['duration_seconds'])<=86460: raise ValueError('proxy must have a bounded lifetime')
    if not 1<=config.get('max_connections',16)<=32: raise ValueError('connection count exceeds its bound')
    if 'public_listener' in config: validate_public(config)
    if 'stream_command' in config: validate_stream_command(config)
    inside_home(config['output'])


async def run(config):
    validate(config);verify_local_listener(config)
    output=inside_home(config['output']);output.mkdir(parents=True,exist_ok=True)
    stop=asyncio.Event(); tasks=set();loop=asyncio.get_running_loop()
    for sig in [signal.SIGINT,signal.SIGTERM]: loop.add_signal_handler(sig,stop.set)
    report={'started_unix':time.time(),'connections_accepted':0,'connections_rejected':0,'bytes_forwarded':0,
            'tls_termination':'coordinator only','status':'running', 'listen':config['listen'],
            'source_allowlist':config.get('public_listener',{}).get('source_ips'),
            'max_forward_bytes':config.get('max_forward_bytes'),
            'transport':'fixed_stream_command' if 'stream_command' in config else 'tcp',
            'stream_children':[], 'stream_failures':0, 'stream_launches':0}
    async def copy(reader,writer):
        while True:
            data=await reader.read(65536)
            if not data: return
            if report['bytes_forwarded'] + len(data) > config.get('max_forward_bytes', float('inf')):
                report['status']='byte_budget_exhausted'; stop.set(); return
            report['bytes_forwarded']+=len(data);writer.write(data);await writer.drain()
    async def handle(reader,writer):
        task=asyncio.current_task();remote=None;command=None
        if stop.is_set() or not peer_allowed(config, writer.get_extra_info('peername')) or len(tasks)>=config.get('max_connections',16):
            report['connections_rejected']+=1;writer.close();await writer.wait_closed();return
        tasks.add(task);report['connections_accepted']+=1
        try:
            if 'stream_command' in config:
                if report['stream_launches'] >= config['stream_command'].get('max_launches', 256):
                    report['status']='stream_launch_budget_exhausted';stop.set();return
                command=CommandStream(config['stream_command'], output)
                report['stream_launches']+=1
                incoming=remote=command
            else:
                incoming,remote=await asyncio.wait_for(asyncio.open_connection(*config['target'],limit=65536),5)
            transfers=[asyncio.create_task(copy(reader,remote)),asyncio.create_task(copy(incoming,writer))]
            try:
                await asyncio.wait(transfers,return_when=asyncio.FIRST_COMPLETED)
            finally:
                for transfer in transfers: transfer.cancel()
                await asyncio.gather(*transfers,return_exceptions=True)
        except (OSError,ValueError,RuntimeError,asyncio.TimeoutError):
            report['connections_rejected']+=1
            if 'stream_command' in config: report['stream_failures']+=1
        finally:
            writer.close()
            if command is not None:
                cleanup=asyncio.create_task(command.stop())
                try: outcome=await asyncio.shield(cleanup)
                except asyncio.CancelledError: outcome=await cleanup
                report['stream_children'].append(outcome)
                if not outcome['reaped']:
                    report['status']='failed_child_cleanup';stop.set()
                elif outcome['returncode'] not in (0, -signal.SIGTERM, -signal.SIGKILL):
                    report['stream_failures']+=1
            elif remote is not None: remote.close()
            tasks.discard(task)
    server=await asyncio.start_server(handle,*config['listen'],limit=65536)
    start=time.monotonic()
    try:
        while time.monotonic()-start<config['duration_seconds'] and not stop.is_set() and not (output/'STOP').exists():
            atomic_json(output/'status.json',report)
            try: await asyncio.wait_for(stop.wait(),.5)
            except asyncio.TimeoutError: pass
        if report['status']=='running': report['status']='stopped'
    finally:
        # Python 3.12+ waits for active clients here. Close their streams and
        # reap owned helper processes before awaiting server completion.
        stop.set()
        server.close()
        active=list(tasks)
        for task in active: task.cancel()
        await asyncio.gather(*active,return_exceptions=True)
        await server.wait_closed()
        if report['status']=='stopped' and report['stream_failures']:
            report['status']='failed_stream_command'
        report.update(elapsed_seconds=time.monotonic()-start,remaining_owned_connections=len(tasks))
        atomic_json(output/'status.json',report)
        for sig in [signal.SIGINT,signal.SIGTERM]: loop.remove_signal_handler(sig)
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config',required=True);parser.add_argument('--execute',action='store_true')
    args=parser.parse_args();config=json.loads(inside_home(args.config).read_text())
    if not args.execute:print(json.dumps({'mode':'plan_only','configuration':config}));return 0
    report=asyncio.run(run(config));print(json.dumps(report,indent=2))
    return 1 if report['status'].startswith('failed') else 0
if __name__=='__main__':sys.exit(main())
