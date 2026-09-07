#!/usr/bin/env python3
"""Transfer a bounded file through an already authenticated, idle tmux SSH pane.

Explicit operator bootstrap utility; no coordinator or job semantics use this.
The remote target must be beneath its user's home; SSH configuration is untouched.
"""
import argparse
import base64
import hashlib
from pathlib import Path
import shlex
import subprocess
import time
import uuid


# Leave room below tmux's command-size limit for the send-keys framing. Larger
# scripts/files must use transfer(), whose individual chunks stay bounded.
MAX_COMMAND_BYTES = 8000


def require_ssh_pane(session):
    """Reject a disconnected pane before sending payloads to a local shell.

    This is an early diagnostic, not authentication proof: the remote Linux/SSH
    guard remains necessary if the connection closes between inspection and send.
    """
    try:
        pane = subprocess.run(
            ['tmux', 'display-message', '-p', '-t', session,
             '#{pane_current_command}\t#{pane_dead}'],
            check=True, capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError('cannot inspect the existing SSH pane; no payload sent') from None
    if pane.stdout.strip() != 'ssh\t0':
        raise RuntimeError('existing pane is not running SSH; reconnect and authenticate privately before retrying; no payload sent')


def command(session, code):
    guarded = "import os,sys; os.umask(0o077); assert os.environ.get('SSH_CONNECTION') and sys.platform == 'linux', 'authenticated Linux SSH session required'; " + code
    encoded = base64.b64encode(guarded.encode()).decode()
    line = 'python3 -c ' + shlex.quote('import base64;exec(base64.b64decode('+repr(encoded)+'))')
    if len(line.encode()) > MAX_COMMAND_BYTES:
        raise ValueError('bootstrap command exceeds the 8000-byte limit; use bounded transfer() for larger payloads')
    require_ssh_pane(session)
    try:
        # Payloads may include private deployment data. Neither tmux diagnostics
        # nor CalledProcessError's command rendering may reach caller tracebacks.
        subprocess.run(['tmux', 'send-keys', '-l', '-t', session, line],
                       check=True, capture_output=True)
        subprocess.run(['tmux', 'send-keys', '-t', session, 'Enter'],
                       check=True, capture_output=True)
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError('tmux bootstrap command failed; inspect the existing pane privately before retrying because submission state is uncertain') from None


def transfer(session, source, relative_target):
    target = Path(relative_target)
    if target.is_absolute() or '..' in target.parts:
        raise ValueError('remote destination must be a home-relative path without traversal')
    data = Path(source).read_bytes()
    if len(data) > 16 * 1024 * 1024:
        raise ValueError('bootstrap file exceeds 16 MiB bound')
    staging = str(target) + '.upload-' + uuid.uuid4().hex
    prefix = "import pathlib,base64,os; p=pathlib.Path.home()/"+repr(staging)+"; p.resolve().relative_to(pathlib.Path.home().resolve()); "
    command(session, prefix + "p.parent.mkdir(parents=True,exist_ok=True); f=p.open('xb'); f.close()")
    time.sleep(.15)
    for offset in range(0, len(data), 1200):
        chunk = base64.b64encode(data[offset:offset+1200]).decode()
        command(session, prefix + "assert p.stat().st_size == "+str(offset)+"; f=p.open('ab'); f.write(base64.b64decode("+repr(chunk)+")); f.close()")
        time.sleep(.015)
    digest = hashlib.sha256(data).hexdigest()
    command(session, prefix + "import hashlib,json; d=p.read_bytes(); assert hashlib.sha256(d).hexdigest()=="+repr(digest)+"; dest=pathlib.Path.home()/"+repr(str(target))+"; dest.resolve().relative_to(pathlib.Path.home().resolve()); os.link(p,dest); p.unlink(); print(json.dumps({'transfer':str(dest),'bytes':len(d),'sha256':hashlib.sha256(d).hexdigest()}))")


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('session')
    parser.add_argument('source')
    parser.add_argument('home_relative_target')
    args = parser.parse_args()
    transfer(args.session, args.source, args.home_relative_target)
