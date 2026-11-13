"""CedeGrid's own cost inside a worker's process: a drain check and a target-file read, by the
process's CPU time, and the CPU a second they cost at 20 checks a second (the matched probe's
rate). Prints the figures; fails if that is 1% of one core or more."""
import json
import tempfile
import time
import unittest
from pathlib import Path

from cedegrid.worker import ResourceSignals, _read_signals

CHECKS_PER_SECOND = 20


def per_call(call, count):
    began = time.process_time()
    for _ in range(count):
        call()
    return (time.process_time() - began) / count


class OwnCostTests(unittest.TestCase):
    def test_a_drain_check_and_a_target_read_cost_under_one_percent_of_a_core(self):
        with tempfile.TemporaryDirectory(dir=Path.home()) as temporary:
            root = Path(temporary)
            target = root / "resource-target.json"
            target.write_text(json.dumps({"version": 1, "sequence": 1, "deadline_unix_ms": 1793000000000,
                                          "step": "hint", "targets": {"ram_mib": 64},
                                          "allowance": {"ram_mib": 128}, "allowance_sequence": 1}))
            # Old enough that the cached read applies, as between two supervisor writes.
            old = time.time() - 10
            import os
            os.utime(target, (old, old))
            signals = ResourceSignals(str(root / "drain"), str(target))
            signals.draining()
            check = per_call(signals.draining, 20000)
            read = per_call(lambda: _read_signals(target), 5000)
            share = check * CHECKS_PER_SECOND
            print(json.dumps({"sdk": "python", "drain_check_us": round(check * 1e6, 2),
                              "target_read_us": round(read * 1e6, 2),
                              "core_share_at_20_per_second": round(share, 6)}))
            self.assertLess(share, 0.01)


if __name__ == "__main__":
    unittest.main()
