#!/usr/bin/env python3
"""Build release binaries with private build paths remapped; never publish.

Linux cross builds use Zig's glibc 2.35 target. Compilation is not minimum-OS
qualification; the release gates must run the resulting bytes on those systems.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

ROOT=Path(__file__).resolve().parent.parent
TARGETS={
 'darwin-arm64':('aarch64-apple-darwin',None),
 'darwin-x64':('x86_64-apple-darwin',None),
 'linux-x64-gnu':('x86_64-unknown-linux-gnu','x86_64-linux-gnu.2.35'),
 'linux-arm64-gnu':('aarch64-unknown-linux-gnu','aarch64-linux-gnu.2.35'),
 'win32-x64':('x86_64-pc-windows-msvc',None),
}
def main():
 parser=argparse.ArgumentParser(description=__doc__)
 parser.add_argument('--target',choices=TARGETS,required=True)
 parser.add_argument('--out',type=Path,required=True)
 parser.add_argument('--zig',type=Path,help='Zig executable for cross builds')
 parser.add_argument('--cargo',default='cargo')
 parser.add_argument('--xwin',type=Path,help='cargo-xwin executable for MSVC cross builds')
 args=parser.parse_args()
 out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
 target,zig_target=TARGETS[args.target]
 env=os.environ.copy()
 remap=str(Path.home().resolve())
 env['RUSTFLAGS']=f'{env.get("RUSTFLAGS", "")} --remap-path-prefix={remap}=/builder --remap-path-prefix={ROOT}=/src/cedegrid'.strip()
 if args.target=='win32-x64':env['RUSTFLAGS']+=' -C target-feature=+crt-static'
 for variable in ('CFLAGS','CXXFLAGS'):
  env[variable]=f'{env.get(variable, "")} -ffile-prefix-map={remap}=/builder -ffile-prefix-map={ROOT}=/src/cedegrid'.strip()
 env['MACOSX_DEPLOYMENT_TARGET']='14.0'
 env['CARGO_PROFILE_RELEASE_STRIP']='none'
 sysroot=Path(subprocess.check_output(['rustc','--print','sysroot'],env=env,text=True).strip())
 if sys.platform=='darwin':env['DYLD_LIBRARY_PATH']=str(sysroot/'lib')+os.pathsep+env.get('DYLD_LIBRARY_PATH','')
 if args.zig:
  if not zig_target:parser.error('Zig is used only for Linux/Windows cross builds')
  zig=args.zig.resolve(strict=True)
  scripts=out/'build-tools';scripts.mkdir(exist_ok=True)
  wrapper=scripts/'cc.py'
  rust_host=subprocess.check_output(['rustc','-vV'],env=env,text=True).split('host: ')[1].splitlines()[0]
  lld=sysroot/'lib/rustlib'/rust_host/'bin/rust-lld'
  wrapper.write_text('#!'+sys.executable+'\nimport os,sys,subprocess,shlex\n'+f'zig={str(zig)!r}\ntarget={zig_target!r}\nold={target!r}\nlld={str(lld)!r}\n'+'''args=[a.replace('--target='+old,'--target='+target) for a in sys.argv[1:]]
fix='-Wl,--fix-cortex-a53-843419'
if fix not in args:os.execv(zig,[zig,'cc','-target',target]+args)
# Zig 0.14 cannot parse this Rust 1.98 linker option. Use its exact CRT/sysroot
# link command, then perform the final link through LLVM with the erratum fix.
result=subprocess.run([zig,'cc','-target',target,'-v']+[a for a in args if a!=fix],capture_output=True,text=True)
sys.stderr.write(result.stdout+result.stderr)
if result.returncode:sys.exit(result.returncode)
output=args[args.index('-o')+1]
links=[shlex.split(line) for line in result.stderr.splitlines() if line.startswith('ld.lld ')]
links=[line for line in links if '-o' in line and line[line.index('-o')+1]==output]
if len(links)!=1:raise RuntimeError('cannot identify the exact Zig final link command')
os.execv(lld,[lld,'-flavor','gnu','--fix-cortex-a53-843419']+links[0][1:])
''')
  wrapper.chmod(0o755)
  archiver=scripts/'ar.sh';archiver.write_text('#!/bin/sh\nexec '+shlex.quote(str(zig))+' ar "$@"\n');archiver.chmod(0o755)
  env['CARGO_TARGET_'+target.upper().replace('-','_')+'_LINKER']=str(wrapper)
  env['CC_'+target.replace('-','_')]=str(wrapper)
  env['AR_'+target.replace('-','_')]=str(archiver)
  env['ZIG_GLOBAL_CACHE_DIR']=str(out/'zig-global-cache')
  env['ZIG_LOCAL_CACHE_DIR']=str(out/'zig-local-cache')
  if args.target=='win32-x64':
   dlltool=scripts/'x86_64-w64-mingw32-dlltool'
   dlltool.write_text('#!/bin/sh\nexec '+shlex.quote(str(zig))+' dlltool "$@"\n');dlltool.chmod(0o755)
   env['PATH']=str(scripts)+os.pathsep+env.get('PATH','')
 command=[args.cargo,'build','--release','--locked','--offline','--target',target]
 if args.xwin:
  if args.target!='win32-x64':parser.error('--xwin is only for the Windows MSVC target')
  env['XWIN_CACHE_DIR']=str(ROOT/'.runtime/release-0.2/xwin-cache')
  env['XWIN_ARCH']='x86_64'
  command=[str(args.xwin.resolve()),'xwin',*command[1:]]
 with (out/'build.log').open('w') as log:
  result=subprocess.run(command,cwd=ROOT,env=env,stdout=log,stderr=subprocess.STDOUT)
 if result.returncode:
  print((out/'build.log').read_text()[-12000:],file=sys.stderr);raise SystemExit(result.returncode)
 binary=ROOT/'target'/target/'release'/('cedegrid.exe' if args.target=='win32-x64' else 'cedegrid')
 destination=out/binary.name
 if destination.exists():raise RuntimeError('refusing to replace an existing native candidate')
 shutil.copyfile(binary,destination);destination.chmod(0o755)
 if args.target.startswith('darwin-'):
  # Mach-O N_OSO entries preserve linker object paths independently of Rust
  # source remapping. Remove debug/local symbols before the final ad-hoc seal.
  subprocess.run(['/usr/bin/strip','-S','-x',str(destination)],check=True)
  subprocess.run(['/usr/bin/codesign','--force','--sign','-',str(destination)],check=True)
 elif args.target.startswith('linux-'):
  rust_host=subprocess.check_output(['rustc','-vV'],env=env,text=True).split('host: ')[1].splitlines()[0]
  objcopy=sysroot/'lib/rustlib'/rust_host/'bin/rust-objcopy'
  if objcopy.is_file():
   subprocess.run([str(objcopy),'--strip-debug',str(destination)],env=env,check=True)
  elif args.zig:
   subprocess.run([str(args.zig.resolve()),'objcopy','--strip-debug',str(destination)],env=env,check=True)
  elif sys.platform.startswith('linux'):
   subprocess.run(['strip','--strip-debug',str(destination)],check=True)
  else:raise RuntimeError('cross-built Linux candidates require a target-aware debug stripper')
 sha=hashlib.sha256(destination.read_bytes()).hexdigest()
 record={'version':'0.2.0','target':target,'package_target':args.target,'sha256':sha,'size':destination.stat().st_size,
 'path_remapping':True,'linux_glibc_target':'2.35' if args.target.startswith('linux-') else None,
 'macos_deployment_target':'14.0' if args.target.startswith('darwin-') else None,
 'signature':'ad-hoc' if args.target.startswith('darwin-') else 'unsigned',
 'command':command,'rustc':subprocess.check_output(['rustc','--version'],env=env,text=True).strip(),
 'platform_qualification':'NOT_RUN','publication':'NOT_RUN'}
 (out/'native-build.json').write_text(json.dumps(record,indent=2)+'\n')
 print(json.dumps(record))
if __name__=='__main__':main()
