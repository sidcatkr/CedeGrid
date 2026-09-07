"""Pure harness tests. No subprocess, cgroup control, or benchmark is executed."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("comparison", Path(__file__).parents[1] / "tools" / "compare.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class ComparisonTests(unittest.TestCase):
    def test_quantile_interpolation_and_missing_data(self):
        self.assertEqual(comparison.quantile([1, 2, 3, 4, 5], .95), 4.8)
        self.assertEqual(comparison.quantile([7], .99), 7)
        self.assertIsNone(comparison.quantile([], .95))
        self.assertIsNone(comparison.quantile([None, float("nan")], .95))
        with self.assertRaises(ValueError):
            comparison.quantile([1], 2)

    def test_slowdown_requires_present_positive_reference(self):
        self.assertAlmostEqual(comparison.slowdown(12, 10), .2)
        self.assertAlmostEqual(comparison.slowdown(8, 10), -.2)
        for observed, alone in [(None, 1), (1, None), (1, 0)]:
            self.assertIsNone(comparison.slowdown(observed, alone))

    def test_warmup_arrivals_are_not_counted_after_queue_delay(self):
        self.assertFalse(comparison.measurement_eligible("protected", 31, 31.1, 29, 30, 150))
        self.assertTrue(comparison.measurement_eligible("protected", 31, 31.1, 30, 30, 150))
        self.assertTrue(comparison.measurement_eligible("managed", 31, 31.1, 29, 30, 150))
        self.assertFalse(comparison.measurement_eligible("protected", 149, 151, 149, 30, 150))

    def test_default_only_prints_protocol_even_if_profiles_are_supplied(self):
        with patch.object(comparison.subprocess, "run", side_effect=AssertionError("must not launch")), patch.object(comparison.subprocess, "Popen", side_effect=AssertionError("must not launch")):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(comparison.main(["--baseline-config", "missing.yaml", "--cgroup-config", "missing-other.yaml"]), 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["mode"], "plan_only")
        self.assertFalse(report["execution_authorized"])
        self.assertEqual(report["repetitions"], 5)
        self.assertEqual(report["warmup_seconds"], 30)
        self.assertEqual(report["measurement_seconds"], 120)

    def test_execute_on_macos_fails_before_any_subprocess(self):
        with patch.object(comparison.sys, "platform", "darwin"), patch.object(comparison.subprocess, "run", side_effect=AssertionError("must not launch")):
            with self.assertRaisesRegex(RuntimeError, "Linux execution"):
                comparison.main(["--execute"])

    def test_internal_worker_cannot_be_started_without_explicit_paths(self):
        with patch.object(comparison, "worker", side_effect=AssertionError("must not launch")):
            with self.assertRaises(ValueError):
                comparison.main(["--worker", "managed"])

    def test_worker_also_requires_execute_flag(self):
        with patch.object(comparison, "worker", side_effect=AssertionError("must not launch")):
            with self.assertRaises(ValueError):
                comparison.main(["--worker", "managed", "--ready", "r", "--start", "s", "--result", "o", "--cpu", "0"])

    def test_summary_keeps_missing_latency_distinct_from_zero(self):
        summaries = comparison.summarize([{"status": "completed", "scenario": "idle", "profile": "rootless", "managed": {"throughput_per_second": 20}}])
        self.assertEqual(summaries["idle:rootless"]["managed_batches_per_second"]["median"], 20)
        self.assertIsNone(summaries["idle:rootless"]["protected_p99_ms"]["median"])

    def test_counter_reset_and_missing_data_are_not_zero(self):
        def reading(total):
            return {"value": None if total is None else f"some avg10=0.0 total={total}\n", "error": None}
        before = {"monotonic_ns": 0, "system": {key: reading(100) for key in ["cpu", "memory", "io"]}, "workload_cgroup": None}
        after = {"monotonic_ns": 1_000_000, "system": {"cpu": reading(120), "memory": reading(20), "io": reading(None)}, "workload_cgroup": None}
        deltas = comparison.counter_deltas(before, after)
        self.assertEqual(deltas["system"]["cpu"]["some.total_us"], 20)
        self.assertIsNone(deltas["system"]["memory"]["some.total_us"])
        self.assertIsNone(deltas["system"]["io"])

    def test_invalid_duration_never_executes(self):
        for options in [["--measure", "0"], ["--warmup", "-1"], ["--measure", "nan"], ["--repetitions", "0"]]:
            with self.assertRaises(ValueError):
                comparison.main(options)


if __name__ == "__main__":
    unittest.main()

class RootlessTargetTests(unittest.TestCase):
    def test_protected_baseline_is_alone_not_an_already_slow_contended_run(self):
        targets=comparison.protocol(comparison.parser().parse_args([]))['pre_registered_targets']
        cases=[{'status':'completed','scenario':'protected_alone','profile':'rootless','protected':{'p99_ms':10}},
               {'status':'completed','scenario':'contention','profile':'unmanaged','protected':{'p99_ms':100}},
               {'status':'completed','scenario':'contention','profile':'rootless','protected':{'p99_ms':50}}]
        result=comparison.evaluate_targets(cases,targets)['protected_p99_ratio_max']
        self.assertEqual(result['observed'],5)
        self.assertEqual(result['status'],'failed')

    def test_missing_comparison_metrics_remain_pending(self):
        args=comparison.parser().parse_args([])
        targets=comparison.protocol(args)['pre_registered_targets']
        self.assertTrue(all(item['status']=='pending' for item in comparison.evaluate_targets([],targets).values()))

    def test_pre_registered_overhead_target_uses_matched_throughput(self):
        targets=comparison.protocol(comparison.parser().parse_args([]))['pre_registered_targets']
        cases=[{'status':'completed','scenario':'idle','profile':'unmanaged','managed':{'throughput_per_second':100}},
               {'status':'completed','scenario':'idle','profile':'rootless','managed':{'throughput_per_second':85},
                'manager_mean_cpu_cores':.1,'manager_peak_rss_bytes':10*1024**2}]
        result=comparison.evaluate_targets(cases,targets)
        self.assertEqual(result['idle_managed_throughput_ratio_min']['status'],'failed')
        self.assertEqual(result['protected_p99_ratio_max']['status'],'pending')
