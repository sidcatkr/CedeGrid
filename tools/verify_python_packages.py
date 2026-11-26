#!/usr/bin/env python3
"""Test the original wheel and independently rebuilt sdist outside the checkout.

The wheel must hold the SDK only: the `cedegrid` package's Python files and its metadata, with no
command-line binary, launcher, script or console entry point (003 FR-073). The CLI ships as its
own release files.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

import release_version

def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()

def sdk_only(wheel,version):
    """Refuse a wheel that holds anything beyond the SDK's Python files and metadata."""
    info=f'cedegrid-{version}.dist-info/'
    with zipfile.ZipFile(wheel) as archive:
        names=[item.filename for item in archive.infolist() if not item.is_dir()]
        for name in names:
            if name.startswith(info):continue
            if not (name.startswith('cedegrid/') and name.endswith('.py') and name.count('/')==1):
                raise RuntimeError(f'the wheel holds more than the SDK: {name}')
        if info+'entry_points.txt' in names:
            entry_points=archive.read(info+'entry_points.txt').decode()
            if 'console_scripts' in entry_points or 'gui_scripts' in entry_points:
                raise RuntimeError('the wheel installs a command; the CLI ships as its own release files')
    if 'cedegrid/__init__.py' not in names:raise RuntimeError('the wheel holds no cedegrid package')
def run(args,cwd,env):
    result=subprocess.run([str(arg) for arg in args],cwd=cwd,env=env,text=True,capture_output=True)
    if result.returncode:raise RuntimeError(f'{args[0]} failed ({result.returncode})\n{result.stdout}\n{result.stderr}')
    return result.stdout+result.stderr

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidate',type=Path)
    parser.add_argument('--python',action='append',required=True)
    args=parser.parse_args()
    candidate=args.candidate.resolve()
    version=json.loads((candidate/'manifest.json').read_text())['version'] if (candidate/'manifest.json').exists() else release_version.version()
    wheel=candidate/f'pypi/cedegrid-{version}-py3-none-any.whl';sdist=candidate/f'pypi/cedegrid-{version}.tar.gz'
    original={wheel.name:sha(wheel),sdist.name:sha(sdist)}
    env=dict(os.environ);env.pop('PYTHONPATH',None)
    env.setdefault('UV_CACHE_DIR','/private/tmp/cedegrid-uv-cache')
    with tempfile.TemporaryDirectory(prefix='cedegrid-python-packages-') as directory:
        root=Path(directory).resolve();source=root/'source';source.mkdir()
        with tarfile.open(sdist) as archive:archive.extractall(source,filter='data')
        source=source/f'cedegrid-{version}'
        tests=root/'tests';shutil.copytree(source/'tests',tests)
        rebuilt=root/'rebuilt'
        run(['uv','build','--python',args.python[-1],'--wheel','--out-dir',rebuilt,source],root,env)
        rebuilt_wheel=rebuilt/wheel.name
        with zipfile.ZipFile(wheel) as archive:
            metadata=archive.read(f'cedegrid-{version}.dist-info/WHEEL').decode()
            if 'Root-Is-Purelib: true' not in metadata or 'Tag: py3-none-any' not in metadata:raise RuntimeError('wheel must be pure Python')
        sdk_only(wheel,version);sdk_only(rebuilt_wheel,version)
        results=[]
        for index,interpreter in enumerate(args.python):
            for kind,artifact in [('original-wheel',wheel),('sdist-rebuilt-wheel',rebuilt_wheel)]:
                environment=root/f'env-{index}-{kind}'
                run(['uv','venv','--python',interpreter,environment],root,env)
                python=environment/('Scripts/python.exe' if os.name=='nt' else 'bin/python')
                run(['uv','pip','install','--python',python,artifact],root,env)
                version=run([python,'-I','-c','import sys; print(sys.version.split()[0])'],root,env).strip()
                run([python,'-I','-c',f'import cedegrid; assert cedegrid.__file__.startswith({str(environment)!r}), cedegrid.__file__'],root,env)
                output=run([python,'-I','-m','unittest','discover','-s',tests],root,env)
                results.append({'python':version,'artifact':kind,'sha256':sha(artifact),'status':'PASS','test_output':output.strip()})
        if original!={wheel.name:sha(wheel),sdist.name:sha(sdist)}:raise RuntimeError('original release files changed during tests')
        evidence={'schema_version':1,'status':'PASS','original_artifacts':original,'rebuild_is_release_file':False,'results':results}
        (candidate/'python-install-evidence.json').write_text(json.dumps(evidence,indent=2)+'\n')
        print(json.dumps(evidence,indent=2))

if __name__=='__main__':main()
