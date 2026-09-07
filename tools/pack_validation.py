#!/usr/bin/env python3
"""Create a source-only, hashed validation bundle; no credentials or environments."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import tarfile
from validation_runtime import inside_home


def pack(manager, application, dataset, output):
    manager, application, dataset, output = map(inside_home,[manager,application,dataset,output])
    files=[]
    for name in ['Cargo.toml','Cargo.lock','README.md']:
        files.append((manager/name,'source/ResourceManager/'+name))
    for directory in ['src','tests','python','tools','examples','docs']:
        for p in sorted((manager/directory).rglob('*')):
            if p.is_file() and not any(x in p.parts for x in ['__pycache__','.pytest_cache']):
                if p.suffix not in ['.py','.rs','.json','.toml','.md','.yaml','.yml','.sh']:continue
                files.append((p,'source/ResourceManager/'+str(p.relative_to(manager))))
    for name in ['main.py','requirements-train.txt']:
        files.append((application/name,'source/Kaggriculture/'+name))
    for directory in ['training','integration','ln_v4','tests','scripts','config','opponents','bases','weights']:
        for p in sorted((application/directory).rglob('*')):
            if p.is_file() and not any(x in p.parts for x in ['__pycache__','.pytest_cache']):
                if p.suffix not in ['.py','.json','.npz','.md','.sh']:continue
                files.append((p,'source/Kaggriculture/'+str(p.relative_to(application))))
    for p in sorted(dataset.rglob('*')):
        if p.is_file():files.append((p,'bootstrap-dataset/'+str(p.relative_to(dataset))))
    manifest={'schema_version':1,'classification':'private_validation_source_and_derived_dataset','files':{}}
    output.parent.mkdir(parents=True,exist_ok=True)
    with output.open('xb') as stream, tarfile.open(fileobj=stream,mode='w:gz') as archive:
        for p,name in files:
            data=p.read_bytes();manifest['files'][name]={'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)}
            info=tarfile.TarInfo(name);info.size=len(data);info.mode=0o600;archive.addfile(info,io.BytesIO(data))
        data=json.dumps(manifest,indent=2).encode();info=tarfile.TarInfo('source-manifest.json');info.size=len(data);info.mode=0o600;archive.addfile(info,io.BytesIO(data))
    result={'path':str(output),'sha256':hashlib.sha256(output.read_bytes()).hexdigest(),'bytes':output.stat().st_size,'files':len(files)}
    output.with_suffix(output.suffix+'.json').write_text(json.dumps(result,indent=2)+'\n')
    return result

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['manager','application','dataset','output']:p.add_argument('--'+name,required=True)
    a=p.parse_args();print(json.dumps(pack(a.manager,a.application,a.dataset,a.output),indent=2))
