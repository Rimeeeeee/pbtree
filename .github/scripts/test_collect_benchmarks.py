"""Sample counts and source provenance come from the measured workload."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("collector", Path(__file__).with_name("collect-benchmarks.py"))
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class CollectorTests(unittest.TestCase):
    def test_records_actual_samples_and_keeps_unchanged_suite_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "benches").mkdir()
            (root / "benches/tree.rs").write_text("fn tree_benchmark() {}\n")
            estimates = root / "criterion/tree_op/new"
            estimates.mkdir(parents=True)
            (estimates / "benchmark.json").write_text(json.dumps({"group_id": "tree/op", "full_id": "tree/op/1024"}))
            (estimates / "estimates.json").write_text(json.dumps({"mean": {"point_estimate": 100, "confidence_interval": {"lower_bound": 90, "upper_bound": 110, "confidence_level": 0.95}}}))
            sample_file = estimates / "sample.json"
            sample_file.write_text(json.dumps({"iters": list(range(1, 51)), "times": [100] * 50}))

            def command(arguments, **_):
                return "rustc 1.96.0" if arguments[0] == "rustc" else "#socket,core\n0,0\n0,0\n0,1\n0,1\n"

            with patch.object(collector.subprocess, "check_output", side_effect=command):
                before = collector.collect(root, root / "criterion", "a" * 40)
                (root / "benches/backends.rs").write_text("fn arena_benchmark() {}\n")
                after = collector.collect(root, root / "criterion", "b" * 40)
                self.assertEqual(after["benchmarks"][0]["samples"], 50)
                self.assertEqual(after["physical_cores"], 2)
                self.assertEqual(before["benchmarks"][0]["source_hash"], after["benchmarks"][0]["source_hash"])
                self.assertNotEqual(before["suite_hash"], after["suite_hash"])
                sample_file.write_text(json.dumps({"iters": [1] * 49, "times": [100] * 50}))
                with self.assertRaisesRegex(ValueError, "sample count"):
                    collector.collect(root, root / "criterion", "b" * 40)


if __name__ == "__main__":
    unittest.main()
