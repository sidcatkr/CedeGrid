#!/usr/bin/env python3
"""Build immutable release file candidates; never upload anything.

The candidate holds the SDK files (a Python wheel and sdist, a TypeScript SDK tarball), one
standalone archive per native target, install.sh with the release version written in, and the
Homebrew formula cedegrid.rb with each macOS and Linux tarball's sha256 written in. Native
input files must already be built and signed as intended; this tool copies those bytes unchanged.
It records missing targets and leaves platform qualification NOT_RUN; successful packaging is not
platform or release acceptance.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent
from pack_source_review import inspect_text
import release_version

TARGETS = {
    'linux-x64-gnu': ('linux', 'cedegrid'),
    'linux-arm64-gnu': ('linux', 'cedegrid'),
    'darwin-x64': ('darwin', 'cedegrid'),
    'darwin-arm64': ('darwin', 'cedegrid'),
    'win32-x64': ('win32', 'cedegrid.exe'),
}
INSTALL_SCRIPT = ROOT / 'tools' / 'install.sh'
VERSION_PLACEHOLDER = '@VERSION@'
HOMEBREW_TEMPLATE = ROOT / 'packaging' / 'homebrew' / 'cedegrid.rb'
# The targets the Homebrew formula installs: macOS and Linux, each by its tarball's sha256.
HOMEBREW_TARGETS = ('darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu')

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
    if path.suffix in ('.sh','.rb'):members=[(path.name,path.read_bytes())]
    elif path.suffix=='.whl' or path.suffix=='.zip':
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

def sdk_tarball_only(path):
    """Refuse a TypeScript SDK tarball that installs a command or pulls in a native package."""
    with tarfile.open(path) as archive:
        names=[item.name for item in archive.getmembers() if item.isfile()]
        manifest=json.loads(archive.extractfile('package/package.json').read())
    for key in ('bin','optionalDependencies','scripts'):
        if key=='scripts':
            hooks={'preinstall','install','postinstall'}&set(manifest.get('scripts',{}))
            if hooks:raise RuntimeError(f'the SDK tarball runs install scripts: {sorted(hooks)}')
        elif manifest.get(key):raise RuntimeError(f'the SDK tarball declares {key}; the CLI ships as its own release files')
    stray=[name for name in names if 'launcher' in Path(name).name]
    if stray:raise RuntimeError(f'the SDK tarball holds a launcher: {stray}')

def standalone_name(version, target):
    suffix = 'zip' if TARGETS[target][0] == 'win32' else 'tar.gz'
    return f'cedegrid-{version}-{target}.{suffix}'

def install_script(version):
    """install.sh with the release version in place of its one placeholder."""
    text = INSTALL_SCRIPT.read_text(encoding='utf-8')
    if text.count(VERSION_PLACEHOLDER) != 1:
        raise RuntimeError('install.sh must hold its version placeholder exactly once')
    return text.replace(VERSION_PLACEHOLDER, version)

def homebrew_formula(version, digests):
    """The formula with the release URL, the version and each tarball's sha256 written in."""
    text = HOMEBREW_TEMPLATE.read_text(encoding='utf-8')
    values = {'@RELEASE_URL@': release_version.release_url(version), VERSION_PLACEHOLDER: version}
    for target in HOMEBREW_TARGETS:
        if not re.fullmatch(r'[0-9a-f]{64}', digests.get(target, '')):
            raise RuntimeError(f'the Homebrew formula needs the {target} tarball')
        values['@SHA256_' + target.upper().replace('-', '_') + '@'] = digests[target]
    for placeholder, value in values.items():
        if placeholder not in text:
            raise RuntimeError(f'the Homebrew template lacks {placeholder}')
        text = text.replace(placeholder, value)
    left = re.findall(r'@[A-Z0-9_]+@', text)
    if left:
        raise RuntimeError(f'the Homebrew template has values nothing writes: {sorted(set(left))}')
    return text

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
    version=release_version.check()
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
    if not args.skip_python and not (out/'pypi'/f'cedegrid-{version}-py3-none-any.whl').exists():
        (out/'pypi').mkdir()
        run(['uv','build','--python',args.python,'--out-dir',out/'pypi',ROOT/'python'])
    if not args.skip_npm and not (out/'npm'/f'cedegrid-{version}.tgz').exists():
        (out/'npm').mkdir()
        run(['npm','run','build'],ROOT/'npm')
        run(['npm','pack','--ignore-scripts','--pack-destination',out/'npm'],ROOT/'npm')
    if not args.skip_npm:sdk_tarball_only(out/'npm'/f'cedegrid-{version}.tgz')
    script=out/'install'/'install.sh'
    if script.exists():
        if script.read_text(encoding='utf-8')!=install_script(version):raise RuntimeError('install.sh differs from the immutable existing candidate')
    else:
        script.parent.mkdir(exist_ok=True)
        script.write_text(install_script(version),encoding='utf-8')
    missing=[target for target in binaries if not (out/'standalone'/standalone_name(version,target)).exists()]
    if missing:
        notices=native_notices(args.cargo);(out/'standalone').mkdir(exist_ok=True)
        for target in missing:
            binary=binaries[target]
            executable=TARGETS[target][1]
            standalone=out/'standalone'/standalone_name(version,target)
            stage=out/'staging'/('cedegrid-'+target);(stage/'bin').mkdir(parents=True,exist_ok=True)
            destination=stage/'bin'/executable;shutil.copyfile(binary,destination);destination.chmod(0o755)
            if digest(destination)!=digest(binary):raise RuntimeError('native bytes changed while staging')
            for name in ('LICENSE','NOTICE'):shutil.copyfile(ROOT/name,stage/name)
            (stage/'THIRD_PARTY_NOTICES.md').write_text(notices,encoding='utf-8')
            directory=f'cedegrid-{version}-{target}'
            if standalone.suffix=='.zip':
                with zipfile.ZipFile(standalone,'x',compression=zipfile.ZIP_DEFLATED) as archive:
                    for name in (f'bin/{executable}','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md'):archive.write(stage/name,arcname=f'{directory}/{Path(name).name}')
            else:
                with tarfile.open(standalone,'x:gz') as archive:
                    for name in (f'bin/{executable}','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md'):archive.add(stage/name,arcname=f'{directory}/{Path(name).name}',recursive=False)
        shutil.rmtree(out/'staging')
    (out/'pypi'/'.gitignore').unlink(missing_ok=True)
    for target,binary in binaries.items():
        standalone=out/'standalone'/standalone_name(version,target)
        name=f'cedegrid-{version}-{target}/{TARGETS[target][1]}'
        if standalone.suffix=='.zip':
            with zipfile.ZipFile(standalone) as archive:embedded=archive.read(name)
        else:
            with tarfile.open(standalone) as archive:embedded=archive.extractfile(name).read()
        if hashlib.sha256(embedded).hexdigest()!=digest(binary):raise RuntimeError('native input differs from the immutable standalone archive')
    for target in TARGETS:
        if (out/'standalone'/standalone_name(version,target)).exists() and target not in binaries:
            raise RuntimeError('resume must include every previously frozen native target')
    if all(target in binaries for target in HOMEBREW_TARGETS):
        formula=homebrew_formula(version,{target:digest(out/'standalone'/standalone_name(version,target)) for target in HOMEBREW_TARGETS})
        destination=out/'homebrew'/'cedegrid.rb'
        if destination.exists():
            if destination.read_text(encoding='utf-8')!=formula:raise RuntimeError('cedegrid.rb differs from the immutable existing candidate')
        else:
            destination.parent.mkdir(exist_ok=True)
            destination.write_text(formula,encoding='utf-8')
    artifacts=[]
    for directory in ('homebrew','install','npm','pypi','standalone'):
        for path in sorted((out/directory).rglob('*')):
            if path.is_file():artifacts.append({'path':path.relative_to(out).as_posix(),'sha256':digest(path),'size':path.stat().st_size,'scan':scan_archive(path)})
    record={'schema_version':1,'version':version,'publication':'NOT_RUN','platform_qualification':'NOT_RUN','missing_native_targets':sorted(set(TARGETS)-set(binaries)),'native_inputs':{target:{'sha256':digest(binary),'size':binary.stat().st_size} for target,binary in binaries.items()},'artifacts':artifacts}
    manifest_path=out/('manifest.partial.json' if args.defer_manifest else 'manifest.json')
    manifest_path.write_text(json.dumps(record,indent=2)+'\n')
    if not args.defer_manifest:(out/'manifest.partial.json').unlink(missing_ok=True)
    print(manifest_path)

if __name__=='__main__':main()
