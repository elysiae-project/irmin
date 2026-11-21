import json
import pathlib
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

    def test_summary_reports_repeat_statistics(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100, 101, 99, 100, 100], [(0.1, 0.1)] * 5)
            result = summarize(root, 5)
            self.assertNotIn("verdict", result)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)
            self.assertAlmostEqual(result["benchmarks"]["download/test"]["median_Bps"], 100)

    def test_spread_statistic_is_reported_as_data(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [80, 100, 120, 100, 100], [(0.1, 0.1)] * 5)
            result = summarize(root, 5)
            self.assertNotIn("verdict", result)
            self.assertGreater(result["benchmarks"]["download/test"]["spread_fraction"], 0.05)

    def test_high_load_does_not_exclude_repeat(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            loads = [(0.1, 0.1)] * 5 + [(2.0, 0.1)]
            self.make_runs(root, [100] * 6, loads)
            result = summarize(root, 6)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 6)

    def test_one_repeat_is_included_in_raw_summary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            result = summarize(root, 1)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 1)

    def test_missing_load_samples_do_not_exclude_throughput_repeats(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5)
            (root / "run-03.log").write_text("load_before=unavailable\nload_after=unavailable\n")
            result = summarize(root, 5)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)

    def test_non_finite_load_samples_are_preserved_as_missing_context(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5)
            (root / "run-03.log").write_text("load_before=nan\nload_after=inf\n")
            result = summarize(root, 5)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)
            self.assertIsNone(result["runs"][2]["load_before"])
            self.assertIsNone(result["runs"][2]["load_after"])

    def test_each_benchmark_reports_available_repeat_count(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 5, [(0.1, 0.1)] * 5, secondary=[100] * 3)
            result = summarize(root, 5)
            self.assertNotIn("verdict", result["benchmarks"]["download/test"])
            self.assertEqual(result["benchmarks"]["download/other"]["valid_runs"], 3)

    def test_missing_sibling_estimate_is_not_silently_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            (root / "run-01-criterion" / "test_case" / "estimates.json").unlink()
            result = summarize(root, 1)
            self.assertEqual(result["benchmarks"], {})
            self.assertEqual(len(result["report_errors"]), 1)

    def test_missing_benchmark_id_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100] * 6, [(0.1, 0.1)] * 6)
            (root / "run-06-criterion").rename(root / "run-06-criterion-missing")
            result = summarize(root, 6)
            self.assertEqual(result["benchmarks"]["download/test"]["valid_runs"], 5)
            self.assertEqual(result["missing_benchmark_runs"]["download/test"], [6])

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
            result = summarize(root, 1)
            self.assertTrue(any("duplicate benchmark full_id" in error for error in result["report_errors"]))

    def test_orphan_estimate_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "run-01.log").write_text("load_before=0.1\nload_after=0.1\n")
            report = root / "run-01-criterion" / "orphan"
            report.mkdir(parents=True)
            (report / "estimates.json").write_text(json.dumps({"median": {"point_estimate": 1}}))
            result = summarize(root, 1)
            self.assertIn("missing benchmark.json", result["report_errors"][0])

    def test_non_finite_estimate_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self.make_runs(root, [100], [(0.1, 0.1)])
            report = root / "run-01-criterion" / "test_case" / "estimates.json"
            report.write_text('{"median":{"point_estimate":NaN}}')
            result = summarize(root, 1)
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
                    result = summarize(root, 1)
                    self.assertTrue(result["report_errors"])

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
                    result = summarize(root, 1)
                    self.assertTrue(result["report_errors"])

    def test_missing_run_log_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            result = summarize(root, 1)
            self.assertIn("unreadable run log", result["report_errors"][0])


if __name__ == "__main__":
    unittest.main()
