#!/usr/bin/env python3
"""Bounded home-only filesystem probe; never overrides ResourceManager admission."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid


def home_path(value):
    path = Path(value).expanduser().resolve()
    path.relative_to(Path.home().resolve())
    return path


def qualify(root):
    root = home_path(root)
    root.mkdir(parents=True, exist_ok=True)
    directory = root / ('storage-probe-' + uuid.uuid4().hex)
    directory.mkdir(mode=0o700)
    started = time.monotonic()
    result = {'schema_version': 1, 'path': str(directory), 'sqlite_version': sqlite3.sqlite_version,
              'admission_override': False, 'power_loss_durability': 'unverified', 'checks': {}}
    if os.name == 'posix' and Path('/proc/mounts').exists():
        proc = subprocess.run(['findmnt', '-T', str(directory), '-n', '-o', 'TARGET,FSTYPE,OPTIONS'], capture_output=True, text=True, check=False)
        result['mount'] = proc.stdout.strip()
    try:
        payload = b'cedegrid-home-only-qualification\n' * 4096
        temporary, published = directory / 'attempt.tmp', directory / 'published.bin'
        with temporary.open('xb') as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, published)
        fd = os.open(directory, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
        result['checks']['file_directory_sync_and_readback'] = published.read_bytes() == payload
        result['sha256'] = hashlib.sha256(payload).hexdigest()
        # A process-crash probe is not a machine/power-loss durability test.
        if sqlite3.sqlite_version_info >= (3, 51, 3):
            code = "import sqlite3,sys,os; c=sqlite3.connect(sys.argv[1]); c.execute('pragma journal_mode=WAL'); c.execute('pragma synchronous=FULL'); c.execute('create table receipt(id integer primary key)'); c.execute('insert into receipt values(1)'); c.commit(); os._exit(0)"
            subprocess.run([os.sys.executable, '-c', code, str(directory/'probe.sqlite3')], check=True, timeout=10)
            with sqlite3.connect(directory/'probe.sqlite3') as db:
                result['checks']['committed_receipt_after_process_exit'] = db.execute('select id from receipt').fetchall() == [(1,)]
                result['checks']['sqlite_integrity'] = db.execute('pragma integrity_check').fetchone()[0]
        else:
            result['checks']['wal_probe'] = 'not tested: runtime lacks required upstream WAL fix version'
        result['qualification'] = 'observations_only; filesystem support policy unchanged'
    except Exception as error:
        result['error'] = str(error)
    result['elapsed_seconds'] = time.monotonic() - started
    (directory/'report.json').write_text(json.dumps(result, indent=2)+'\n')
    return result

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    print(json.dumps(qualify(args.output), indent=2))
