"""Bounded, secret-safe tmux bootstrap diagnostics; no remote calls."""
import base64
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import traceback
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import session_transfer


class SessionTransferTests(unittest.TestCase):
    def setUp(self):
        # Command tests isolate submission; pane inspection has separate tests.
        guard = patch.object(session_transfer, 'require_ssh_pane')
        self.addCleanup(guard.stop)
        guard.start()

    def test_command_keeps_guard_and_private_diagnostics(self):
        with patch.object(session_transfer.subprocess, 'run') as run:
            session_transfer.command('existing-pane', "print('ready')")
        self.assertEqual(run.call_count, 2)
        first = run.call_args_list[0]
        self.assertEqual(first.args[0][:5], ['tmux', 'send-keys', '-l', '-t', 'existing-pane'])
        decoded_script = shlex.split(first.args[0][-1])[2]
        encoded = decoded_script.split("b64decode('", 1)[1].split("')", 1)[0]
        remote = base64.b64decode(encoded).decode()
        self.assertIn("os.environ.get('SSH_CONNECTION')", remote)
        self.assertIn("sys.platform == 'linux'", remote)
        self.assertTrue(remote.endswith("print('ready')"))
        for call in run.call_args_list:
            self.assertTrue(call.kwargs['capture_output'])
            self.assertTrue(call.kwargs['check'])

    def test_submission_failure_never_formats_private_payload(self):
        secret = 'private-credential-test-value'
        def fail(argv, **_kwargs):
            raise subprocess.CalledProcessError(1, argv, output=secret, stderr=secret)
        with patch.object(session_transfer.subprocess, 'run', side_effect=fail) as run:
            try:
                session_transfer.command('existing-pane', 'token=' + repr(secret))
            except RuntimeError as error:
                rendered = ''.join(traceback.format_exception(error))
                self.assertNotIn(secret, rendered)
                self.assertNotIn('b64decode', rendered)
                self.assertNotIn('CalledProcessError', rendered)
                self.assertIn('submission state is uncertain', str(error))
                self.assertIsNone(error.__cause__)
                self.assertTrue(error.__suppress_context__)
            else:
                self.fail('tmux failure must propagate a sanitized error')
        self.assertEqual(run.call_count, 1)

    def test_enter_failure_is_sanitized_and_not_retried(self):
        error = subprocess.CalledProcessError(1, ['tmux', 'secret'], stderr='secret')
        with patch.object(session_transfer.subprocess, 'run', side_effect=[None, error]) as run:
            with self.assertRaisesRegex(RuntimeError, 'submission state is uncertain') as caught:
                session_transfer.command('existing-pane', "print('ready')")
        self.assertEqual(run.call_count, 2)
        self.assertNotIn('secret', ''.join(traceback.format_exception(caught.exception)))

    def test_missing_tmux_does_not_disclose_error_filename(self):
        error = FileNotFoundError(2, 'private diagnostic', 'private-filename')
        with patch.object(session_transfer.subprocess, 'run', side_effect=error):
            with self.assertRaises(RuntimeError) as caught:
                session_transfer.command('existing-pane', "print('ready')")
        self.assertNotIn('private', str(caught.exception).replace('privately', ''))

    def test_oversized_command_is_rejected_before_any_keys_are_sent(self):
        with patch.object(session_transfer.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'use bounded transfer') as caught:
                session_transfer.command('existing-pane', 'private-payload' * 1000)
        run.assert_not_called()
        self.assertNotIn('private-payload', str(caught.exception))

    def test_file_transfer_chunks_remain_inside_command_bound(self):
        with tempfile.TemporaryDirectory(dir=Path.home()) as directory:
            source = Path(directory) / 'payload'
            source.write_bytes(b'x' * 4000)
            with patch.object(session_transfer.subprocess, 'run') as run, \
                    patch.object(session_transfer.time, 'sleep'):
                session_transfer.transfer('existing-pane', source, '.local/share/resmgr/test-file')
        lines = [call.args[0][-1] for call in run.call_args_list if '-l' in call.args[0]]
        self.assertEqual(len(lines), 6)  # Begin, four chunks, immutable publication.
        self.assertTrue(all(len(line.encode()) <= session_transfer.MAX_COMMAND_BYTES for line in lines))


class PaneInspectionTests(unittest.TestCase):
    def test_live_ssh_pane_permits_inspection_without_sending_keys(self):
        with patch.object(session_transfer.subprocess, 'run', return_value=
                          subprocess.CompletedProcess([], 0, 'ssh\t0\n')) as run:
            session_transfer.require_ssh_pane('existing-pane')
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0][1], 'display-message')
        self.assertEqual(run.call_args.kwargs['timeout'], 5)

    def test_local_dead_or_unknown_panes_receive_no_payload(self):
        for status in ('zsh\t0\n', 'ssh\t1\n', '', 'private-unknown-command\t0'):
            with self.subTest(status=status), patch.object(
                    session_transfer.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 0, status)) as run:
                with self.assertRaisesRegex(RuntimeError, 'no payload sent') as caught:
                    session_transfer.command('existing-pane', "print('private-payload')")
                self.assertEqual(run.call_count, 1)
                self.assertNotIn('private', str(caught.exception).replace('privately', ''))

    def test_inspection_failures_are_sanitized_without_retry_or_payload(self):
        for error in (subprocess.CalledProcessError(1, ['private-command'], stderr='private'),
                      subprocess.TimeoutExpired(['private-command'], 5, output='private'),
                      FileNotFoundError('private')):
            with self.subTest(error=type(error).__name__), patch.object(
                    session_transfer.subprocess, 'run', side_effect=error) as run:
                with self.assertRaisesRegex(RuntimeError, 'no payload sent') as caught:
                    session_transfer.command('existing-pane', "print('private-payload')")
                self.assertEqual(run.call_count, 1)
                self.assertNotIn('private', ''.join(traceback.format_exception(caught.exception)))


if __name__ == '__main__':
    unittest.main()
