"""SDK protocol unit tests. Native durability is tested by the Rust integration suite."""
import hashlib
from pathlib import Path
import tempfile
import unittest
from cedegrid import WorkerContext, DrainRequested, PublicationUncertain, RemoteError, command_task
from cedegrid.codec import stringify_json

class SupervisorFixture:
    def __init__(self):
        self.calls, self.data, self.publications = [], bytearray(), {}
        self.fail_commit = False
        self.sequence = 0
    def request(self, op, **fields):
        self.calls.append((op, fields))
        if op == 'artifact_begin':
            self.data.clear()
            return {'upload_id':'upload','offset':0}
        if op == 'artifact_chunk':
            assert fields['offset'] == len(self.data)
            chunk = bytes.fromhex(fields['data_hex'])
            assert len(chunk) <= 256 * 1024
            self.data.extend(chunk)
            return {'upload_id':'upload','offset':len(self.data)}
        if op == 'artifact_finish':
            return {'artifact_id':'sealed','sha256':hashlib.sha256(self.data).hexdigest(),'size':len(self.data)}
        if op == 'publication_commit':
            if self.fail_commit: raise OSError('acknowledgement lost')
            publication_id = fields['publication_id']
            previous = self.publications.get(publication_id)
            if previous:
                if previous[0] != fields: raise RemoteError('publication ID conflict', code='ERR_CEDEGRID_CONFLICT')
                return previous[1]
            self.sequence += 1
            result = {'publication_id':publication_id,'state':'committed','kind':fields['kind'],
                      'sequence':self.sequence,'sha256':hashlib.sha256(stringify_json(fields).encode()).hexdigest(),
                      'assurance':'durable_local'}
            self.publications[publication_id] = fields, result
            return result
        if op == 'publication_status': return self.publications[fields['publication_id']][1]
        raise AssertionError(op)

class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.supervisor = SupervisorFixture()
        self.descriptor = {'schema_version':2,'namespace_id':'namespace','session_id':'session',
                           'task_id':'task','assignment_id':'assignment','generation':2,
                           'output_dir':str(self.root/'uncreated-output')}
        self.context = WorkerContext(self.descriptor, self.root/'drain', client=self.supervisor)
    def tearDown(self): self.temp.cleanup()
    def test_streamed_artifact_and_repeatable_publication_identity(self):
        source = self.root/'payload'
        source.write_bytes(b'a'*(600*1024))
        artifact = self.context.artifact('payload', source)
        self.assertEqual(len([c for c in self.supervisor.calls if c[0]=='artifact_chunk']),3)
        first = self.context.complete({'value':2**53+1,'f':1.0},[artifact],publication_id='publication')
        self.assertEqual(first,self.context.complete({'value':2**53+1,'f':1.0},[artifact],publication_id='publication'))
        self.assertEqual(first,self.context.publication_status('publication'))
        with self.assertRaises(RemoteError): self.context.complete({'value':43},[artifact],publication_id='publication')
        self.assertFalse(self.context.output.exists())
    def test_native_supervisor_allocates_checkpoint_sequence(self):
        first=self.context.checkpoint({})
        reopened=WorkerContext(self.descriptor,client=self.supervisor)
        second=reopened.checkpoint({})
        self.assertEqual((first['sequence'],second['sequence']),(1,2))
    def test_uncertain_commit_preserves_operation_and_attempt(self):
        self.supervisor.fail_commit=True
        with self.assertRaises(PublicationUncertain) as caught:
            self.context.complete({},publication_id='recover-this')
        error=caught.exception
        self.assertEqual(error.publication_id,'recover-this')
        self.assertEqual(error.identity['generation'],2)
        self.assertIsNone(error.digest)
        self.assertEqual(len(error.request_digest),64)
        self.assertIsNotNone(error.cause)
        self.assertFalse(self.context.output.exists())
    def test_worker_rejects_unsealed_and_tampered_artifact_handles(self):
        source=self.root/'data';source.write_bytes(b'original')
        artifact=self.context.artifact('payload',source)
        with self.assertRaises(ValueError): self.context.complete({},[{**artifact,'sha256':'0'*64}])
        with self.assertRaises(ValueError): self.context.complete({},[{**artifact,'artifact_id':'foreign'}])
    def test_drain_hooks_run_once(self):
        calls=[];self.context.on_drain(lambda _:calls.append(1))
        self.assertFalse(self.context.draining());(self.root/'drain').touch()
        with self.assertRaises(DrainRequested):self.context.safe_point()
        self.assertTrue(self.context.draining());self.assertEqual(calls,[1])
    def test_context_and_names_fail_before_native_requests(self):
        with self.assertRaises(ValueError):WorkerContext({**self.descriptor,'schema_version':1},client=self.supervisor)
        with self.assertRaises(ValueError):WorkerContext({**self.descriptor,'generation':2**63},client=self.supervisor)
        for name in ['.','..','x/y','x\\y','x\0y','é'*65]:
            with self.assertRaises(ValueError):self.context.artifact(name,self.root/'absent')
        self.assertEqual(self.supervisor.calls,[])
    def test_resource_wire_names_match_rust(self):
        task=command_task('t',['worker'],'/home/user/work',gpu_vram_mib={'GPU-test':100})
        self.assertEqual(task['resources']['gpu_memory_mib'],{'GPU-test':100})
        self.assertFalse(task['replay_safe']);self.assertFalse(task['single_process']);self.assertFalse(task['no_escape'])

if __name__=='__main__':unittest.main()
