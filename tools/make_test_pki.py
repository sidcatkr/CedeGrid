#!/usr/bin/env python3
"""Generate private, short-lived validation mTLS certificates under home."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
from validation_runtime import inside_home


def generate(output, hosts, node_ids=None):
    node_ids = list(node_ids) if node_ids is not None else ['node']
    if not node_ids or len(set(node_ids)) != len(node_ids) or any(not isinstance(node, str) or not node or len(node) > 128 for node in node_ids):
        raise ValueError('node IDs must be unique nonempty strings of at most 128 characters')
    output = inside_home(output)
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = dict(os.environ, TMPDIR=str(output))
    def run(*args):
        subprocess.run(['openssl', *map(str,args)], cwd=output, env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    (output / 'ca.cnf').write_text('[req]\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n')
    run('req','-x509','-newkey','rsa:2048','-nodes','-keyout','ca.key','-out','ca.pem','-days','7','-subj','/CN=cedegrid-validation-ca','-config','ca.cnf')
    clients = {}
    roles = {}
    node_identities = {}
    names = [('server', None), ('operator', None)] + [
        ('node' if node_ids == ['node'] else f'node-{index}', node_id)
        for index, node_id in enumerate(node_ids)]
    for name, node_id in names:
        run('req','-newkey','rsa:2048','-nodes','-keyout',name+'.key','-out',name+'.csr','-subj','/CN=cedegrid-validation-'+name)
        extension = output/(name+'.ext')
        extension.write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\nextendedKeyUsage='+('serverAuth' if name=='server' else 'clientAuth')+'\n'+('subjectAltName='+','.join(hosts)+'\n' if name=='server' else ''))
        run('x509','-req','-in',name+'.csr','-CA','ca.pem','-CAkey','ca.key','-CAcreateserial','-out',name+'.pem','-days','7','-extfile',name+'.ext')
        os.chmod(output/(name+'.key'),0o600)
        if name != 'server':
            der = subprocess.check_output(['openssl','x509','-in',str(output/(name+'.pem')),'-outform','DER'])
            clients[name] = hashlib.sha256(der).hexdigest()
            roles[clients[name]] = {'role': 'operator'} if name == 'operator' else {'role': 'node', 'node_id': node_id}
            if node_id is not None:
                node_identities[node_id] = {'certificate': str(output/(name+'.pem')),
                    'private_key': str(output/(name+'.key')), 'fingerprint': clients[name]}
    os.chmod(output/'ca.key',0o600)
    return {'directory':str(output),'client_fingerprints':clients,'clients':roles,
            'node_identities':node_identities,'expires_days':7}

if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',required=True)
    p.add_argument('--san',action='append',default=['DNS:localhost','IP:127.0.0.1'])
    p.add_argument('--node-id',action='append',help='repeat for each stable configured node ID; default node')
    a=p.parse_args()
    print(json.dumps(generate(a.output,a.san,a.node_id),indent=2))
