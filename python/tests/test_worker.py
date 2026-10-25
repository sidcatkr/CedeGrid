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
    def test_a_drain_only_worker_behaves_as_under_0_2(self):
        """A worker that only watches the drain file keeps its 0.2 drain behavior beside
        a target file the supervisor rewrites, with either context shape (FR-036)."""
        import json, os
        from unittest.mock import patch
        target = self.root/'resource-target.json'
        files = [json.dumps({'version':1,'sequence':1,'deadline_unix_ms':1793000000000,'step':'hint',
                             'targets':{'ram_mib':64},'allowance':{'ram_mib':128},'allowance_sequence':1}),
                 json.dumps({'version':2,'sequence':2}), '{not json', None]
        new = {**self.descriptor,'resource_target_file':str(target),'capabilities':['resource_targets']}
        for descriptor in (self.descriptor, new):
            drain = self.root/'drain'
            drain.unlink(missing_ok=True)
            supervisor = SupervisorFixture()
            with patch.dict(os.environ, {'CEDEGRID_RESOURCE_TARGET_FILE':str(target)}):
                context = WorkerContext(descriptor, drain, client=supervisor)
                calls = []
                context.on_drain(lambda _: calls.append(1))
                for content in files:
                    if content is None: target.unlink(missing_ok=True)
                    else: target.write_text(content)
                    self.assertFalse(context.draining())
                    context.safe_point()
                drain.touch()
                with self.assertRaises(DrainRequested): context.safe_point()
                self.assertTrue(context.draining())
            self.assertEqual(calls, [1])
            self.assertEqual(supervisor.calls, [])
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

class ResourceTargetTests(unittest.TestCase):
    """The resource-target channel (contracts/resource-target-file.md)."""
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.target_file = self.root/'resource-target.json'
        self.ack = self.root/'resource-target.ack'
        descriptor = {'schema_version':2,'namespace_id':'namespace','session_id':'session',
                      'task_id':'task','assignment_id':'assignment','generation':2,
                      'resource_target_file':str(self.target_file),'capabilities':['resource_targets']}
        self.context = WorkerContext(descriptor, self.root/'drain', client=SupervisorFixture())
    def tearDown(self): self.temp.cleanup()
    def write(self, sequence, version=1, **targets):
        import json
        self.target_file.write_text(json.dumps({'version':version,'sequence':sequence,'deadline_unix_ms':1793000000000,
                                                'step':'hint','targets':targets,'future_key':True}))
    def test_resource_targets_read_the_current_file(self):
        self.assertIsNone(self.context.resource_targets())
        self.write(3, ram_mib=6144, gpu_memory_mib={'GPU-a':10240})
        targets = self.context.resource_targets()
        self.assertEqual((targets.sequence, targets.deadline_unix_ms, targets.step), (3, 1793000000000, 'hint'))
        self.assertEqual((targets.ram_mib, targets.cpu_millicores, targets.gpu_memory_mib), (6144, None, {'GPU-a':10240}))
        # A relaxed target is a new file with a higher sequence.
        self.write(4)
        relaxed = self.context.resource_targets()
        self.assertEqual((relaxed.sequence, relaxed.ram_mib), (4, None))
    def test_hooks_fire_once_per_sequence_during_drain_polls(self):
        seen = []
        self.context.on_resource_target(lambda targets: seen.append(targets.sequence))
        self.assertFalse(self.context.draining())
        self.write(1, ram_mib=100)
        self.context.draining(); self.context.safe_point(); self.context.draining()
        self.write(2, ram_mib=50)
        self.context.draining()
        self.assertEqual(seen, [1, 2])
    def test_a_late_subscriber_receives_the_current_target_once(self):
        self.write(4, ram_mib=100)
        self.assertFalse(self.context.draining())
        seen = []
        self.context.on_resource_target(lambda targets: seen.append(targets.sequence))
        self.assertEqual(seen, [4])
        self.context.draining(); self.context.safe_point()
        self.assertEqual(seen, [4])
        self.write(5, ram_mib=50)
        self.context.draining()
        self.assertEqual(seen, [4, 5])
    def test_hook_errors_are_isolated(self):
        seen = []
        def failing(_): raise RuntimeError('worker hook failed')
        self.context.on_resource_target(failing)
        self.context.on_resource_target(lambda targets: seen.append(targets.sequence))
        self.write(1, ram_mib=100)
        with self.assertLogs('cedegrid', level='WARNING'):
            self.assertFalse(self.context.draining())
        self.assertEqual(seen, [1])
    def test_signals_from_the_environment_alone(self):
        from unittest.mock import patch
        from cedegrid import ResourceSignals
        self.write(5, cpu_millicores=2000)
        environment = {'CEDEGRID_RESOURCE_TARGET_FILE':str(self.target_file),'CEDEGRID_DRAIN_FILE':str(self.root/'drain')}
        with patch.dict('os.environ', environment, clear=True):
            signals = ResourceSignals.from_env()
        self.assertEqual(signals.resource_targets().cpu_millicores, 2000)
        self.assertFalse(signals.draining())
        (self.root/'drain').touch()
        self.assertTrue(signals.draining())
        with patch.dict('os.environ', {}, clear=True):
            empty = ResourceSignals.from_env()
        self.assertIsNone(empty.resource_targets())
        self.assertFalse(empty.draining())
    def write_file(self, value):
        import json
        self.target_file.write_text(json.dumps(value))
    def test_allowance_hooks_fire_now_and_on_each_change_apart_from_targets(self):
        targets, allowances = [], []
        self.context.on_resource_target(lambda t: targets.append(t.sequence))
        self.context.on_resource_allowance(lambda a: allowances.append((a.sequence, a.ram_mib)))
        self.assertEqual((targets, allowances), ([], []))
        file = {'version':1,'sequence':0,'deadline_unix_ms':0,'step':'none','targets':{},
                'allowance':{'ram_mib':20480},'allowance_sequence':1}
        self.write_file(file)
        self.context.draining()
        # An allowance without a target calls no target hook; the ack still starts at 0.
        self.assertEqual((targets, allowances), ([], [(1, 20480)]))
        self.assertIsNone(self.context.resource_targets())
        self.assertEqual(self.ack.read_text().strip(), '0')
        # An allowance-only change calls only the allowance hook.
        file.update(allowance={'ram_mib':16384}, allowance_sequence=2)
        self.write_file(file); self.context.draining(); self.context.draining()
        self.assertEqual((targets, allowances), ([], [(1, 20480), (2, 16384)]))
        # A target change calls only the target hook, and the ack follows it.
        file.update(sequence=1, step='hint', targets={'ram_mib':8192}, deadline_unix_ms=1793000000000)
        self.write_file(file); self.context.draining()
        self.assertEqual((targets, allowances), ([1], [(1, 20480), (2, 16384)]))
        self.assertEqual(self.ack.read_text().strip(), '1')
        # A late subscriber receives the current allowance at once.
        late = []
        self.context.on_resource_allowance(lambda a: late.append(a.sequence))
        self.assertEqual(late, [2])
        self.assertEqual(self.context.resource_allowance().ram_mib, 16384)
    def test_target_file_interop_cases(self):
        import json
        from cedegrid import ResourceSignals
        fixture = json.loads((Path(__file__).resolve().parents[2]/'tests/contracts/cedegrid-0.2.json').read_text())
        for case in fixture['target_file_cases']:
            self.write_file(case['file'])
            signals = ResourceSignals(None, self.target_file)
            targets, allowance = signals.resource_targets(), signals.resource_allowance()
            got_targets = None if targets is None else {
                'sequence':targets.sequence,'deadline_unix_ms':targets.deadline_unix_ms,'step':targets.step,
                'cpu_millicores':targets.cpu_millicores,'ram_mib':targets.ram_mib,'gpu_memory_mib':targets.gpu_memory_mib}
            got_allowance = None if allowance is None else {
                'sequence':allowance.sequence,'cpu_millicores':allowance.cpu_millicores,
                'ram_mib':allowance.ram_mib,'gpu_memory_mib':allowance.gpu_memory_mib}
            self.assertEqual(got_targets, case['targets'], case['id'])
            self.assertEqual(got_allowance, case['allowance'], case['id'])
    def test_worker_context_cases(self):
        import json
        from cedegrid.errors import ValidationError
        fixture = json.loads((Path(__file__).resolve().parents[2]/'tests/contracts/cedegrid-0.2.json').read_text())
        for case in fixture['worker_context_cases']:
            if not case['accepted']:
                with self.assertRaises(ValidationError, msg=case['id']):
                    WorkerContext(case['context'], self.root/'drain', client=SupervisorFixture())
                continue
            context = WorkerContext(case['context'], self.root/'drain', client=SupervisorFixture())
            self.assertEqual(context.context.get('resource_target_file'), case['resource_target_file'], case['id'])
            self.assertEqual(context.context.get('capabilities', []), case['capabilities'], case['id'])
    def test_a_newer_file_version_is_treated_as_absent(self):
        self.write(9, version=2, ram_mib=1)
        self.assertIsNone(self.context.resource_targets())
        for bad in ['{', '[]', '{"version":1}', '{"version":1,"sequence":-1,"deadline_unix_ms":0,"step":"hint","targets":{}}']:
            self.target_file.write_text(bad)
            self.assertIsNone(self.context.resource_targets(), bad)
    def test_the_ack_records_the_highest_sequence_seen(self):
        self.context.draining()
        self.assertEqual(self.ack.read_text().strip(), '0')
        self.write(7, ram_mib=1)
        self.context.resource_targets()
        self.assertEqual(self.ack.read_text().strip(), '7')
        # An older file never lowers the ack.
        self.write(6, ram_mib=1)
        self.context.resource_targets()
        self.assertEqual(self.ack.read_text().strip(), '7')

if __name__=='__main__':unittest.main()
