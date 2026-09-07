import os
import unittest
from unittest.mock import patch

from resmgr import command_task, spawn_managed, SpawnUncertain
from resmgr.process import ManagedChild, _SupervisorClient, _InvalidReply
from resmgr.client import _TransferPacer


class FakeClient:
    def __init__(self, replies):
        self.replies, self.calls = iter(replies), []
    def request(self, op, **fields):
        self.calls.append((op, fields))
        reply = next(self.replies)
        if isinstance(reply, BaseException):
            raise reply
        return reply


class ManagedChildTests(unittest.TestCase):
    def test_explicit_assignment_and_child_contract(self):
        for limit, single, escape in [(1, True, True), (1, False, False), (9, False, True), (-1, False, True)]:
            with self.assertRaises(ValueError):
                command_task('t', ['worker'], '/home/user', managed_child_limit=limit, single_process=single, no_escape=escape)
        task = command_task('t', ['worker'], '/home/user', managed_child_limit=2, single_process=False, no_escape=True)
        self.assertEqual(task['managed_child_limit'], 2)
        with self.assertRaises(ValueError):
            spawn_managed(['child'])
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, 'did not authorize'):
                spawn_managed(['child'], single_process=True, no_escape=True)

    def test_lost_spawn_reply_preserves_deduplication_identity(self):
        for error in [TimeoutError(), _InvalidReply('EOF')]:
            fake = FakeClient([error, {'child_id': 'c', 'ok': True, 'state': 'running'}])
            with patch.object(_SupervisorClient, 'from_env', return_value=fake):
                with self.assertRaises(SpawnUncertain) as caught:
                    spawn_managed(['child'], single_process=True, no_escape=True)
                child = spawn_managed(['child'], single_process=True, no_escape=True, request_id=caught.exception.request_id)
            self.assertEqual(child.child_id, 'c')
            self.assertEqual(fake.calls[0][1]['request_id'], fake.calls[1][1]['request_id'])
            self.assertNotIn('pid', fake.calls[0][1])

    def test_timeout_and_reconciliation_never_signal(self):
        for reply, expected in [({'state': 'running'}, TimeoutError), ({'state': 'needs_reconciliation'}, RuntimeError)]:
            fake = FakeClient([reply])
            with self.assertRaises(expected):
                ManagedChild(fake, 'c').wait(timeout=0)
            self.assertEqual([op for op, _ in fake.calls], ['status'])

    def test_stop_is_request_and_wait_requires_release(self):
        fake = FakeClient([{'state': 'draining'}, {'state': 'released', 'exit_code': 0}])
        child = ManagedChild(fake, 'c')
        self.assertEqual(child.stop()['state'], 'draining')
        self.assertEqual(child.wait(timeout=1)['exit_code'], 0)
        self.assertEqual(fake.calls, [('stop', {'child_id': 'c'}), ('status', {'child_id': 'c'})])


class PacingTests(unittest.TestCase):
    def test_serialized_hex_expansion_is_charged_without_burst(self):
        clock = [10.]
        waits = []
        def sleep(seconds):
            waits.append(seconds)
            clock[0] += seconds
        pacer = _TransferPacer(1000, clock=lambda: clock[0], sleep=sleep)
        pacer.account(1800)
        pacer.account(900)
        self.assertEqual(waits, [2., 1.])
        self.assertEqual(clock[0], 13.)

    def test_pacing_cannot_be_disabled_by_invalid_configuration(self):
        for rate in [0, -1, float('inf'), float('nan')]:
            with self.assertRaises(ValueError):
                _TransferPacer(rate)

class AttemptBoundTests(unittest.TestCase):
    def test_explicit_attempt_cap_preserves_legacy_omission(self):
        default=command_task('t',['worker'],'/home/user')
        self.assertNotIn('max_attempts',default)
        bounded=command_task('t',['worker'],'/home/user',max_attempts=4)
        self.assertEqual(bounded['max_attempts'],4)
        for invalid in [0,-1,True,1.5]:
            with self.assertRaises(ValueError):command_task('t',['worker'],'/home/user',max_attempts=invalid)
