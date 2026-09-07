"""Public SDK artifact methods against tls_transport.rs's real mTLS coordinator.

The report below is an explicit protocol fixture, not Linux telemetry. No agent,
Prepared message, process identity, execution authorization, or workload is used.
"""
import hashlib
from pathlib import Path
import sys
import time

from resmgr import Client, RemoteError, command_task


def main():
    endpoint, root = sys.argv[1], Path(sys.argv[2])


    def client(name):
        return Client(endpoint, ca=root / 'ca.pem', certificate=root / (name + '.pem'),
                      private_key=root / (name + '.key'))


    operator, node = client('operator'), client('node')
    assert operator.status()['kind'] == 'status'
    assert operator.result('sdk-missing-task') is None
    print('sdk-mtls-pass')

    operator.put_pool('sdk-artifacts', ['node'], max_workers=1)
    operator.submit('sdk-artifacts', 'sdk-artifacts', [command_task(
        'sdk-artifacts', ['/bin/true'], str(root), single_process=True, no_escape=True)])
    report = {
        'node_id': 'node', 'boot_id': 'protocol-fixture-no-runtime',
        'observed_at_unix_ms': int(time.time() * 1000),
        'managed_budget': {'cpu_millicores': 1000, 'ram_mib': 512, 'gpu_memory_mib': {}},
        'expansion_allowed': True, 'gpu_expansion_allowed': False, 'launch_slots': 1,
        'available_controls': [], 'allocations': [],
    }
    assignment = node.request('heartbeat', report=report)['reply']['assignments'][0]
    assignment_id, generation = assignment['request']['assignment_id'], assignment['generation']
    source = root / 'sdk-source'
    data = b'public SDK resumable artifact roundtrip\x00\xff\n'
    source.write_bytes(data)
    artifact = {'sha256': hashlib.sha256(data).hexdigest(), 'size': len(data)}

    # Persist a prefix through real RPC, then resume using the public upload method.
    upload = node.request('begin_upload', assignment_id=assignment_id,
                          generation=generation, artifact=artifact)
    node.request('upload_chunk', upload_id=upload['upload_id'], offset=0,
                 data_hex=data[:7].hex())
    assert node.upload(source, assignment_id, generation) == artifact
    # Repeating the entire operation after discarding its ACK returns the same blob.
    assert node.upload(source, assignment_id, generation) == artifact
    assert node.request('begin_upload', assignment_id=assignment_id,
                        generation=generation, artifact=artifact)['offset'] == len(data)
    destination = root / 'sdk-download'
    assert operator.download(artifact, destination) == destination
    assert destination.read_bytes() == data
    assert operator.download(artifact, destination) == destination

    # A wrong expected size reaches SDK digest verification via the real server.
    # It must preserve an existing destination and remove its failed temporary file.
    protected = root / 'sdk-protected-destination'
    protected.write_bytes(b'preserve-existing-content')
    try:
        operator.download({**artifact, 'size': len(data) - 1}, protected)
    except RemoteError as error:
        assert 'artifact integrity failure' in str(error), error
    else:
        raise AssertionError('SDK accepted a truncated artifact')
    assert protected.read_bytes() == b'preserve-existing-content'
    assert not list(root.glob('.sdk-protected-destination.*.download'))

    # Resume cannot turn a corrupt persisted prefix into an accepted publication.
    bad_source = root / 'sdk-bad-source'
    bad_data = b'expected-upload-content'
    bad_source.write_bytes(bad_data)
    bad_artifact = {'sha256': hashlib.sha256(bad_data).hexdigest(), 'size': len(bad_data)}
    bad_upload = node.request('begin_upload', assignment_id=assignment_id,
                              generation=generation, artifact=bad_artifact)
    node.request('upload_chunk', upload_id=bad_upload['upload_id'], offset=0,
                 data_hex=(b'x' * len(bad_data)).hex())
    try:
        node.upload(bad_source, assignment_id, generation)
    except RemoteError as error:
        assert 'artifact checksum mismatch' in str(error), error
    else:
        raise AssertionError('SDK upload accepted corrupted staging bytes')
    assert not (root / 'state' / 'artifacts' / 'blobs' / bad_artifact['sha256']).exists()
    node.request('abort_upload', upload_id=bad_upload['upload_id'])

    empty_source = root / 'sdk-empty-source'
    empty_source.write_bytes(b'')
    empty = node.upload(empty_source, assignment_id, generation)
    assert empty == {'sha256': hashlib.sha256(b'').hexdigest(), 'size': 0}
    assert operator.download(empty, root / 'sdk-empty-download').read_bytes() == b''

    # Revoke the unexecuted offer; report its fixture release without any fake PID.
    operator.cancel('sdk-artifacts')
    report.update(observed_at_unix_ms=int(time.time() * 1000), expansion_allowed=False,
                  launch_slots=0, allocations=[{
                      'assignment_id': assignment_id, 'generation': generation,
                      'phase': 'released', 'observed': None,
                      'detail': 'protocol-only offer was never prepared or executed',
                  }])
    node.request('heartbeat', report=report)
    status = operator.status('sdk-artifacts')
    assert all(item['phase'] == 'released' for item in status['allocations'])
    print('sdk-artifact-roundtrip-pass')


if __name__ == '__main__':
    main()
