#!/usr/bin/env python3
"""Summarize Criterion throughput estimates and load samples for download runs."""

import json
import math
import pathlib
import statistics
import sys


def summarize(
    root: pathlib.Path,
    repeat_count: int,
) -> dict:
    runs = []
    report_errors = []
    for number in range(1, repeat_count + 1):
        log_path = root / f"run-{number:02d}.log"
        load_values = {}
        try:
            log_text = log_path.read_text()
        except OSError as error:
            log_text = ""
            report_errors.append(f"unreadable run log {log_path.name}: {error}")
        for line in log_text.splitlines():
            for label in ("load_before", "load_after"):
                if line.startswith(label + "="):
                    try:
                        value = float(line.split("=", 1)[1])
                        if math.isfinite(value) and value >= 0:
                            load_values[label] = value
                    except (ValueError, OverflowError):
                        pass
        load_before = load_values.get("load_before")
        load_after = load_values.get("load_after")

        report_root = root / f"run-{number:02d}-criterion"
        measurements = {}
        metadata_files = list(report_root.rglob("benchmark.json"))
        for estimates_path in report_root.rglob("estimates.json"):
            if not estimates_path.with_name("benchmark.json").exists():
                report_errors.append(f"missing benchmark.json: {estimates_path.parent}")
        for metadata_path in metadata_files:
            estimates_path = metadata_path.with_name("estimates.json")
            if not estimates_path.exists():
                report_errors.append(f"missing estimates.json: {estimates_path}")
                continue
            try:
                metadata = json.loads(metadata_path.read_text())
                estimates = json.loads(estimates_path.read_text())
            except (OSError, json.JSONDecodeError) as error:
                report_errors.append(f"invalid report pair: {metadata_path.parent}: {error}")
                continue
            if not isinstance(metadata, dict) or not isinstance(estimates, dict):
                report_errors.append(f"invalid report object: {metadata_path.parent}")
                continue
            throughput_data = metadata.get("throughput")
            median_data = estimates.get("median")
            if not isinstance(throughput_data, dict) or not isinstance(median_data, dict):
                report_errors.append(f"invalid report fields: {metadata_path.parent}")
                continue
            byte_count = throughput_data.get("Bytes")
            median_ns = median_data.get("point_estimate")
            full_id = metadata.get("full_id")
            try:
                byte_count = float(byte_count) if not isinstance(byte_count, bool) else math.nan
                median_ns = float(median_ns) if not isinstance(median_ns, bool) else math.nan
            except (TypeError, ValueError, OverflowError):
                byte_count = median_ns = math.nan
            if (
                not isinstance(full_id, str)
                or not full_id
                or not math.isfinite(byte_count)
                or byte_count <= 0
                or not math.isfinite(median_ns)
                or median_ns <= 0
            ):
                report_errors.append(f"missing byte throughput or median estimate: {metadata_path.parent}")
                continue
            throughput = byte_count * 1e9 / median_ns
            if not math.isfinite(throughput) or throughput <= 0:
                report_errors.append(f"invalid throughput estimate: {metadata_path.parent}")
                continue
            if full_id in measurements:
                report_errors.append(f"duplicate benchmark full_id in run {number}: {full_id}")
                continue
            measurements[full_id] = throughput
        runs.append(
            {
                "run": number,
                "load_before": load_values.get("load_before"),
                "load_after": load_values.get("load_after"),
                "load_recorded": load_before is not None and load_after is not None,
                "throughput_Bps": measurements,
            }
        )

    benchmark_ids = sorted({name for run in runs for name in run["throughput_Bps"]})
    benchmarks = {}
    missing_benchmark_runs = {}
    for name in benchmark_ids:
        missing_runs = [run["run"] for run in runs if name not in run["throughput_Bps"]]
        if missing_runs:
            missing_benchmark_runs[name] = missing_runs
        values = [
            run["throughput_Bps"][name]
            for run in runs
            if name in run["throughput_Bps"]
        ]
        if not values:
            benchmarks[name] = {"valid_runs": 0}
            continue

        median = statistics.median(values)
        spread = (max(values) - min(values)) / median if median > 0 else math.inf
        benchmarks[name] = {
            "valid_runs": len(values),
            "min_Bps": min(values),
            "median_Bps": median,
            "max_Bps": max(values),
            "spread_fraction": spread,
        }

    return {
        "repeats": repeat_count,
        "report_errors": report_errors,
        "missing_benchmark_runs": missing_benchmark_runs,
        "benchmarks": benchmarks,
        "runs": runs,
    }


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: summarize_download_bench.py OUTPUT_DIR REPEATS")
    root = pathlib.Path(sys.argv[1])
    try:
        repeat_count = int(sys.argv[2])
    except ValueError as error:
        raise SystemExit(f"invalid numeric aggregation argument: {error}") from error
    if repeat_count < 1:
        raise SystemExit("REPEATS must be positive")
    result = summarize(root, repeat_count)
    (root / "summary.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
