import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from summarize_download_bench import summarize


class SummarizeDownloadBenchTests(unittest.TestCase):
    def make_runs(
        self,
        root: pathlib.Path,
        throughputs: list[float],
        loads: list[tuple[float, float]],
        secondary: list[float] | None = None,
    ) -> None:
        for index, (throughput, (before, after)) in enumerate(zip(throughputs, loads), start=1):
            (root / f"run-{index:02d}.log").write_text(
                f"load_before={before}\nload_after={after}\n"
            )
            report = root / f"run-{index:02d}-criterion" / "test_case"
            report.mkdir(parents=True)
            (report / "benchmark.json").write_text(
                json.dumps({"full_id": "download/test", "throughput": {"Bytes": 1000}})
            )
            (report / "estimates.json").write_text(
                json.dumps({"median": {"point_estimate": 1e12 / throughput}})
            )
            if secondary is not None and index <= len(secondary):
                other = root / f"run-{index:02d}-criterion" / "other_case"
                other.mkdir(parents=True)
                (other / "benchmark.json").write_text(
                    json.dumps({"full_id": "download/other", "throughput": {"Bytes": 1000}})
                )
                (other / "estimates.json").write_text(
                    json.dumps({"median": {"point_estimate": 1e12 / secondary[index - 1]}})
                )

    def test_quiet_when_five_runs_meet_load_and_spread_thresholds(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100, 101, 99, 100, 100], [(0.1, 0.1)] * 5)
            result = summarize(root, 5, 0.05, 1.0)
            self.assertEqual(result["verdict"], "quiet")
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)

    def test_noisy_when_spread_exceeds_threshold(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [80, 100, 120, 100, 100], [(0.1, 0.1)] * 5)
            result = summarize(root, 5, 0.05, 1.0)
            self.assertEqual(result["verdict"], "noisy")
            self.assertEqual(result["benchmarks"]["download/test"]["verdict"], "noisy")

    def test_contaminated_run_is_excluded_when_five_clean_runs_remain(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            loads = [(0.1, 0.1)] * 5 + [(2.0, 0.1)]
            self.make_runs(root, [100] * 6, loads)
            result = summarize(root, 6, 0.05, 1.0)
            self.assertEqual(result["verdict"], "quiet")
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)

    def test_insufficient_when_fewer_than_five_repeats(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            result = summarize(root, 1, 0.05, 1.0)
            self.assertEqual(result["verdict"], "insufficient")

    def test_missing_load_samples_are_not_quiet_without_a_threshold(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5)
            (root / "run-03.log").write_text("load_before=unavailable\nload_after=unavailable\n")
            result = summarize(root, 5, 0.05, None)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 4)
            self.assertEqual(result["verdict"], "insufficient")

    def test_non_finite_load_samples_are_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5)
            (root / "run-03.log").write_text("load_before=nan\nload_after=inf\n")
            result = summarize(root, 5, 0.05, None)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 4)
            self.assertEqual(result["verdict"], "insufficient")

    def test_top_level_is_insufficient_if_any_benchmark_lacks_five_valid_runs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5, secondary=[100] * 3)
            result = summarize(root, 5, 0.05, 1.0)
            self.assertEqual(result["benchmarks"]["download/test"]["verdict"], "quiet")
            self.assertEqual(result["benchmarks"]["download/other"]["verdict"], "insufficient")
            self.assertEqual(result["verdict"], "insufficient")

    def test_missing_sibling_estimate_is_not_silently_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            (root / "run-01-criterion" / "test_case" / "estimates.json").unlink()
            result = summarize(root, 1, 0.05, None)
            self.assertEqual(result["benchmarks"], {})
            self.assertEqual(result["verdict"], "insufficient")
            self.assertEqual(len(result["report_errors"]), 1)

    def test_missing_benchmark_id_in_a_run_prevents_quiet_verdict(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 6, [(0.1, 0.1)] * 6)
            (root / "run-06-criterion").rename(root / "run-06-criterion-missing")
            result = summarize(root, 6, 0.05, 1.0)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)
            self.assertEqual(result["missing_benchmark_runs"]["download/test"], [6])
            self.assertEqual(result["verdict"], "insufficient")

    def test_duplicate_benchmark_id_in_run_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            duplicate = root / "run-01-criterion" / "duplicate"
            duplicate.mkdir()
            (duplicate / "benchmark.json").write_text(
                json.dumps({"full_id": "download/test", "throughput": {"Bytes": 1000}})
            )
            (duplicate / "estimates.json").write_text(
                json.dumps({"median": {"point_estimate": 1e10}})
            )
            result = summarize(root, 1, 0.05, None)
            self.assertEqual(result["verdict"], "insufficient")
            self.assertTrue(any("duplicate benchmark full_id" in error for error in result["report_errors"]))

    def test_orphan_estimate_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "run-01.log").write_text("load_before=0.1\nload_after=0.1\n")
            report = root / "run-01-criterion" / "orphan"
            report.mkdir(parents=True)
            (report / "estimates.json").write_text(json.dumps({"median": {"point_estimate": 1}}))
            result = summarize(root, 1, 0.05, None)
            self.assertEqual(result["verdict"], "insufficient")
            self.assertIn("missing benchmark.json", result["report_errors"][0])

    def test_non_finite_estimate_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            report = root / "run-01-criterion" / "test_case" / "estimates.json"
            report.write_text('{"median":{"point_estimate":NaN}}')
            result = summarize(root, 1, 0.05, None)
            self.assertEqual(result["verdict"], "insufficient")
            self.assertIn("missing byte throughput or median estimate", result["report_errors"][0])

    def test_null_criterion_fields_are_reported_not_crashed(self) -> None:
        for metadata, estimates in [
            ({"full_id": "download/test", "throughput": None}, {"median": {"point_estimate": 1}}),
            ({"full_id": "download/test", "throughput": {"Bytes": 1}}, {"median": None}),
            ({"full_id": None, "throughput": {"Bytes": 1}}, {"median": {"point_estimate": 1}}),
        ]:
            with self.subTest(metadata=metadata, estimates=estimates):
                with tempfile.TemporaryDirectory() as tmp:
                    root = pathlib.Path(tmp)
                    (root / "run-01.log").write_text("load_before=0.1\nload_after=0.1\n")
                    report = root / "run-01-criterion" / "test_case"
                    report.mkdir(parents=True)
                    (report / "benchmark.json").write_text(json.dumps(metadata))
                    (report / "estimates.json").write_text(json.dumps(estimates))
                    result = summarize(root, 1, 0.05, None)
                    self.assertEqual(result["verdict"], "insufficient")
                    self.assertTrue(result["report_errors"])

    def test_cli_rejects_non_finite_or_negative_load_threshold(self) -> None:
        helper = pathlib.Path(__file__).with_name("summarize_download_bench.py")
        with tempfile.TemporaryDirectory() as tmp:
            for threshold in ("nan", "-1"):
                completed = subprocess.run(
                    [sys.executable, str(helper), tmp, "5", "0.05", threshold],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("LOAD_LIMIT", completed.stderr)

    def test_boolean_and_oversized_numeric_fields_are_rejected(self) -> None:
        for byte_count, median_ns in [(True, 1000), (1000, False), (10**600, 1000)]:
            with self.subTest(byte_count=byte_count, median_ns=median_ns):
                with tempfile.TemporaryDirectory() as tmp:
                    root = pathlib.Path(tmp)
                    (root / "run-01.log").write_text("load_before=0.1\nload_after=0.1\n")
                    report = root / "run-01-criterion" / "test_case"
                    report.mkdir(parents=True)
                    (report / "benchmark.json").write_text(
                        json.dumps({"full_id": "download/test", "throughput": {"Bytes": byte_count}})
                    )
                    (report / "estimates.json").write_text(
                        json.dumps({"median": {"point_estimate": median_ns}})
                    )
                    result = summarize(root, 1, 0.05, None)
                    self.assertEqual(result["verdict"], "insufficient")
                    self.assertTrue(result["report_errors"])

    def test_missing_run_log_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            result = summarize(root, 1, 0.05, None)
            self.assertEqual(result["verdict"], "insufficient")
            self.assertIn("unreadable run log", result["report_errors"][0])


if __name__ == "__main__":
    unittest.main()
