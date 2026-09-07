"""Container opt-in never changes native stress guards or accepts an unverified host."""
import sys
from pathlib import Path
import unittest
from unittest.mock import patch

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'tools'))
import local_smoke


class ContainerValidationProfileTests(unittest.TestCase):
    def test_native_guard_is_unchanged(self):
        for stress in (False, True):
            with self.subTest(stress=stress):
                profile = local_smoke.runtime_profile('native', stress)
                self.assertEqual(profile['minimum_free_ram'], 16 * 1024**3)
                self.assertEqual(profile['allocation_ram_mib'], 4096)

    def test_container_profile_cannot_override_native_stress(self):
        with self.assertRaises(ValueError):
            local_smoke.runtime_profile('docker-cpu', True)

    def test_container_profile_refuses_nonlinux(self):
        with patch.object(local_smoke.sys, 'platform', 'darwin'):
            with self.assertRaises(ValueError):
                local_smoke.runtime_profile('docker-cpu')


if __name__ == '__main__':
    unittest.main()
