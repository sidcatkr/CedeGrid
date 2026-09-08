"""Release-preparation regressions; run against installed artifacts as well."""
import unittest
from unittest.mock import patch

from resmgr.process import ManagedChild, SpawnUncertain, _InvalidReply, spawn_managed


class SpawnValidationTests(unittest.TestCase):
    def test_invalid_inputs_do_not_access_environment(self):
        invalid = [
            {"argv": "echo"}, {"argv": b"echo"}, {"argv": []},
            {"argv": [""]}, {"argv": ["echo", "x\0y"]}, {"argv": [1]},
            {"cwd": "relative"}, {"cwd": 1}, {"cwd": "/tmp/\0bad"},
            {"env": []}, {"env": {"A=B": "x"}}, {"env": {"": "x"}},
            {"env": {"A": 1}}, {"env": {"A": "x\0y"}},
            {"env": {"RESMGR_SUPERVISOR_TOKEN": "x"}},
            {"env": {"CEDEGRID_SUPERVISOR_TOKEN": "x"}},
            {"env": {"CUDA_VISIBLE_DEVICES": "0"}},
            {"request_id": ""}, {"request_id": "../x"}, {"request_id": 1},
            {"request_id": "a" * 129}, {"no_escape": 1}, {"single_process": 1},
        ]
        for override in invalid:
            with self.subTest(override=override), patch("resmgr.process._SupervisorClient.from_env") as access:
                args = dict(argv=["echo"], no_escape=True, single_process=True)
                args.update(override)
                with self.assertRaises(ValueError):
                    spawn_managed(**args)
                access.assert_not_called()

    def test_malformed_success_preserves_supplied_request_id(self):
        for response in ({"ok": True}, {"ok": True, "child_id": 1}, {"ok": True, "child_id": "../bad"}, [], None):
            with self.subTest(response=response), patch("resmgr.process._SupervisorClient.from_env") as factory:
                factory.return_value.request.return_value = response
                with self.assertRaises(SpawnUncertain) as error:
                    spawn_managed(["echo"], no_escape=True, single_process=True, request_id="same-request")
                self.assertEqual(error.exception.request_id, "same-request")

    def test_malformed_success_preserves_generated_request_id(self):
        with patch("resmgr.process._SupervisorClient.from_env") as factory:
            factory.return_value.request.return_value = {"ok": True}
            with self.assertRaises(SpawnUncertain) as error:
                spawn_managed(["echo"], no_escape=True, single_process=True)
            self.assertEqual(error.exception.request_id, factory.return_value.request.call_args.kwargs["request_id"])

    def test_valid_empty_argument_and_environment_value_are_not_rejected(self):
        with patch("resmgr.process._SupervisorClient.from_env") as factory:
            factory.return_value.request.return_value = {"ok": True, "child_id": "child-1"}
            child = spawn_managed(["echo", ""], env={"A": ""}, no_escape=True, single_process=True)
            self.assertEqual(child.child_id, "child-1")

    def test_invalid_wait_parameters_fail_before_transport(self):
        for args in ({"timeout": float("inf")}, {"timeout": True}, {"timeout": -1},
                     {"poll_interval": float("nan")}, {"poll_interval": 0}, {"poll_interval": False}):
            with self.subTest(args=args), patch("resmgr.process._SupervisorClient") as factory:
                child = ManagedChild(factory.return_value, "child-1")
                with self.assertRaises(ValueError):
                    child.wait(**args)
                factory.return_value.request.assert_not_called()

    def test_wait_accepts_rust_preparing_wire_state_until_confirmed_release(self):
        with patch("resmgr.process._SupervisorClient") as factory, patch("resmgr.process.time.sleep"):
            replies = [
                {"ok": True, "child_id": "child-1", "state": state}
                for state in ("preparing", "running", "draining", "released")
            ]
            factory.return_value.request.side_effect = replies
            child = ManagedChild(factory.return_value, "child-1")
            self.assertEqual(child.wait(), replies[-1])
            self.assertEqual(factory.return_value.request.call_count, 4)
            self.assertTrue(all(call.args[0] == "status" for call in factory.return_value.request.call_args_list))

    def test_wait_rejects_malformed_states_without_signaling(self):
        for response in ({"state": []}, {"state": {}}, {"state": None}, {"state": 1},
                         {"state": "not-a-state"}, {}, [], None):
            with self.subTest(response=response), patch("resmgr.process._SupervisorClient") as factory:
                factory.return_value.request.return_value = response
                child = ManagedChild(factory.return_value, "child-1")
                with self.assertRaises(_InvalidReply):
                    child.wait()
                factory.return_value.request.assert_called_once_with("status", child_id="child-1")


if __name__ == "__main__":
    unittest.main()
