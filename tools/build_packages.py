#!/usr/bin/env python3
"""Build immutable SDK/native package candidates; never upload to a registry.

Native input files must already be built and signed as intended. This tool copies
those bytes unchanged. It records missing targets and leaves platform qualification
NOT_RUN; successful packaging is not platform or release acceptance.
"""
from __future__ import annotations
import argparse
import hashlib
import io
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent
from pack_source_review import inspect_text

VERSION = '0.2.0'
TARGETS = {
    'linux-x64-gnu': ('linux', 'x64', 'cedegrid'),
    'linux-arm64-gnu': ('linux', 'arm64', 'cedegrid'),
    'darwin-x64': ('darwin', 'x64', 'cedegrid'),
    'darwin-arm64': ('darwin', 'arm64', 'cedegrid'),
    'win32-x64': ('win32', 'x64', 'cedegrid.exe'),
}

def run(args, cwd=ROOT):
    subprocess.run([str(arg) for arg in args], cwd=cwd, check=True)

def digest(path):
    result=hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda:stream.read(1024*1024),b''):result.update(chunk)
    return result.hexdigest()

def native_notices(cargo):
    metadata=json.loads(subprocess.check_output([cargo,'metadata','--locked','--format-version','1'],cwd=ROOT))
    lines=['# Native dependency notices\n\nGenerated from the locked Cargo dependency graph.\n']
    for package in sorted(metadata['packages'],key=lambda item:(item['name'],item['version'])):
        if package['name']=='cedegrid':continue
        base=Path(package['manifest_path']).parent
        lines.append(f"\n## {package['name']} {package['version']}\n\nLicense expression: {package.get('license') or 'see included license file'}\nSource: {package.get('repository') or package.get('source') or 'Cargo registry package'}\n")
        paths=[path for path in base.iterdir() if path.is_file() and path.name.upper().startswith(('LICENSE','COPYING','NOTICE'))]
        if package.get('license_file'):
            path=base/package['license_file']
            if path not in paths:paths.append(path)
        for path in sorted(paths):
            if path.stat().st_size<=1024*1024:
                lines.append(f'\n### {path.name}\n\n'+path.read_text(encoding='utf-8')+'\n')
    return ''.join(lines)

def scan_archive(path):
    if path.suffix=='.whl' or path.suffix=='.zip':
        with zipfile.ZipFile(path) as archive:members=[(item.filename,archive.read(item)) for item in archive.infolist() if not item.is_dir()]
    else:
        with tarfile.open(path) as archive:
            members=[]
            for item in archive.getmembers():
                if item.issym() or item.islnk():raise RuntimeError(f'archive link rejected: {path.name}:{item.name}')
                if item.isfile():members.append((item.name,archive.extractfile(item).read()))
    for name,data in members:
        parts=Path(name).parts
        if Path(name).is_absolute() or '..' in parts or any(part in ('.git','.runtime','node_modules') for part in parts):raise RuntimeError(f'private/unsafe archive path: {path.name}:{name}')
        if any(token in data for token in (b'-----BEGIN '+b'PRIVATE KEY-----',b'-----BEGIN '+b'RSA PRIVATE KEY-----',b'-----BEGIN '+b'EC PRIVATE KEY-----')):raise RuntimeError(f'private key in archive: {path.name}:{name}')
        try:
            data.decode('utf-8')
            inspect_text(name,data)
        except UnicodeDecodeError:
            # Native files may contain embedded build paths and endpoints. Scan
            # printable strings without pretending the executable is UTF-8 text.
            printable=b'\n'.join(re.findall(rb'[\x20-\x7e]{8,}',data))
            inspect_text(name,printable)
    return {'file_count':len(members),'expanded_bytes':sum(len(data) for _,data in members)}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--python',default='python3.14')
    parser.add_argument('--cargo',default=shutil.which('cargo') or str(Path.home()/'.cargo/bin/cargo'))
    parser.add_argument('--resume',action='store_true',help='resume an incomplete candidate without rebuilding existing SDK archives')
    parser.add_argument('--defer-manifest',action='store_true',help='freeze available archives but leave the candidate open for additional native targets')
    parser.add_argument('--binary',action='append',default=[],metavar='TARGET=PATH')
    parser.add_argument('--skip-python',action='store_true')
    parser.add_argument('--skip-npm',action='store_true')
    args=parser.parse_args()
    out=args.out.resolve()
    if args.resume:
        if not out.is_dir() or (out/'manifest.json').exists():parser.error('--resume requires an incomplete candidate directory')
    else:out.mkdir(parents=True,exist_ok=False)
    binaries={}
    for assignment in args.binary:
        target,separator,path=assignment.partition('=')
        if not separator or target not in TARGETS or target in binaries:parser.error('each --binary must have one unique supported TARGET=PATH')
        binary=Path(path).resolve(strict=True)
        if not binary.is_file():parser.error('native input must be a regular file')
        binaries[target]=binary
    if not args.skip_python and not (out/'pypi'/f'cedegrid-{VERSION}-py3-none-any.whl').exists():
        (out/'pypi').mkdir()
        run(['uv','build','--python',args.python,'--out-dir',out/'pypi',ROOT/'python'])
    if not args.skip_npm and not (out/'npm'/f'cedegrid-{VERSION}.tgz').exists():
        (out/'npm').mkdir()
        run(['npm','run','build'],ROOT/'npm')
        run(['npm','pack','--ignore-scripts','--pack-destination',out/'npm'],ROOT/'npm')
    complete_native = all((out/'standalone'/f'cedegrid-{VERSION}-{target}.{"zip" if target.startswith("win32") else "tar.gz"}').exists() and (args.skip_npm or (out/'npm'/f'cedegrid-{target}-{VERSION}.tgz').exists()) for target in binaries)
    if binaries and not complete_native:
        notices=native_notices(args.cargo);(out/'standalone').mkdir(exist_ok=True)
        for target,binary in binaries.items():
            operating_system,architecture,executable=TARGETS[target]
            standalone=out/'standalone'/f'cedegrid-{VERSION}-{target}.{"zip" if operating_system=="win32" else "tar.gz"}'
            npm_archive=out/'npm'/f'cedegrid-{target}-{VERSION}.tgz'
            if standalone.exists() and (args.skip_npm or npm_archive.exists()):continue
            stage=out/'staging'/('cedegrid-'+target);(stage/'bin').mkdir(parents=True,exist_ok=True)
            destination=stage/'bin'/executable;shutil.copyfile(binary,destination);destination.chmod(0o755)
            if digest(destination)!=digest(binary):raise RuntimeError('native bytes changed while staging')
            for name in ('LICENSE','NOTICE'):shutil.copyfile(ROOT/name,stage/name)
            (stage/'THIRD_PARTY_NOTICES.md').write_text(notices,encoding='utf-8')
            manifest={'name':'cedegrid-'+target,'version':VERSION,'description':f'CedeGrid native CLI for {target}','license':'Apache-2.0','os':[operating_system],'cpu':[architecture],'files':['bin/','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md'],'repository':{'type':'git','url':'https://github.com/sidcatkr/CedeGrid.git'}}
            if target.endswith('-gnu'):manifest['libc']=['glibc']
            (stage/'package.json').write_text(json.dumps(manifest,indent=2)+'\n')
            if not args.skip_npm and not npm_archive.exists():run(['npm','pack','--ignore-scripts','--pack-destination',out/'npm'],stage)
            if standalone.exists():continue
            if operating_system=='win32':
                with zipfile.ZipFile(standalone,'x',compression=zipfile.ZIP_DEFLATED) as archive:
                    for name in (f'bin/{executable}','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md'):archive.write(stage/name,arcname=f'cedegrid-{VERSION}-{target}/{Path(name).name}')
            else:
                with tarfile.open(standalone,'x:gz') as archive:
                    for name in (f'bin/{executable}','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md'):archive.add(stage/name,arcname=f'cedegrid-{VERSION}-{target}/{Path(name).name}',recursive=False)
        shutil.rmtree(out/'staging')
    (out/'pypi'/'.gitignore').unlink(missing_ok=True)
    for target,binary in binaries.items():
        if not args.skip_npm:
            with tarfile.open(out/'npm'/f'cedegrid-{target}-{VERSION}.tgz') as archive:
                embedded=archive.extractfile('package/bin/'+TARGETS[target][2]).read()
                if hashlib.sha256(embedded).hexdigest()!=digest(binary):raise RuntimeError('native input differs from the immutable existing archive')
        standalone=out/'standalone'/f'cedegrid-{VERSION}-{target}.{"zip" if target.startswith("win32") else "tar.gz"}'
        if standalone.suffix=='.zip':
            with zipfile.ZipFile(standalone) as archive:embedded=archive.read(f'cedegrid-{VERSION}-{target}/{TARGETS[target][2]}')
        else:
            with tarfile.open(standalone) as archive:embedded=archive.extractfile(f'cedegrid-{VERSION}-{target}/{TARGETS[target][2]}').read()
        if hashlib.sha256(embedded).hexdigest()!=digest(binary):raise RuntimeError('native input differs from the immutable standalone archive')
    for target in TARGETS:
        if any((out/'standalone').glob(f'cedegrid-{VERSION}-{target}.*')) and target not in binaries:
            raise RuntimeError('resume must include every previously frozen native target')
    artifacts=[]
    for directory in ('npm','pypi','standalone'):
        for path in sorted((out/directory).rglob('*')):
            if path.is_file():artifacts.append({'path':path.relative_to(out).as_posix(),'sha256':digest(path),'size':path.stat().st_size,'scan':scan_archive(path)})
    record={'schema_version':1,'version':VERSION,'publication':'NOT_RUN','platform_qualification':'NOT_RUN','missing_native_targets':sorted(set(TARGETS)-set(binaries)),'native_inputs':{target:{'sha256':digest(binary),'size':binary.stat().st_size} for target,binary in binaries.items()},'artifacts':artifacts}
    manifest_path=out/('manifest.partial.json' if args.defer_manifest else 'manifest.json')
    manifest_path.write_text(json.dumps(record,indent=2)+'\n')
    if not args.defer_manifest:(out/'manifest.partial.json').unlink(missing_ok=True)
    print(manifest_path)

if __name__=='__main__':main()
