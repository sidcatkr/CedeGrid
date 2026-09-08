#!/usr/bin/env python3
"""Test the original wheel and independently rebuilt sdist outside the checkout."""
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

def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()
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
    wheel=candidate/'pypi/cedegrid-0.2.0-py3-none-any.whl';sdist=candidate/'pypi/cedegrid-0.2.0.tar.gz'
    original={wheel.name:sha(wheel),sdist.name:sha(sdist)}
    env=dict(os.environ);env.pop('PYTHONPATH',None)
    env.setdefault('UV_CACHE_DIR','/private/tmp/cedegrid-uv-cache')
    with tempfile.TemporaryDirectory(prefix='cedegrid-python-packages-') as directory:
        root=Path(directory).resolve();source=root/'source';source.mkdir()
        with tarfile.open(sdist) as archive:archive.extractall(source,filter='data')
        source=source/'cedegrid-0.2.0'
        tests=root/'tests';shutil.copytree(source/'tests',tests)
        rebuilt=root/'rebuilt'
        run(['uv','build','--python',args.python[-1],'--wheel','--out-dir',rebuilt,source],root,env)
        rebuilt_wheel=rebuilt/wheel.name
        with zipfile.ZipFile(wheel) as archive:
            metadata=archive.read('cedegrid-0.2.0.dist-info/WHEEL').decode()
            if 'Root-Is-Purelib: true' not in metadata or 'Tag: py3-none-any' not in metadata:raise RuntimeError('wheel must be pure Python')
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
        if original!={wheel.name:sha(wheel),sdist.name:sha(sdist)}:raise RuntimeError('original publish artifacts changed during tests')
        evidence={'schema_version':1,'status':'PASS','original_artifacts':original,'rebuild_is_publish_artifact':False,'results':results,'registry_publication':'NOT_RUN'}
        (candidate/'python-install-evidence.json').write_text(json.dumps(evidence,indent=2)+'\n')
        print(json.dumps(evidence,indent=2))

if __name__=='__main__':main()
