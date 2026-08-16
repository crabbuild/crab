#!/usr/bin/env python3
"""Run or verify retained NFS read-path benchmark evidence."""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import subprocess
import sys
import tempfile
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


EXPECTED_SCENARIOS = (
    "pointer_sequential_path_read",
    "pointer_sequential_lease_read",
    "pointer_random_path_read",
    "pointer_random_lease_read",
    "overlay_modified_path_reread",
    "overlay_modified_lease_reread",
)

BENCHMARK_SUITE = "nfs-read-path-bench"

RATIO_THRESHOLD_ARGS = (
    (
        "min_pointer_sequential_lease_ratio",
        "pointer_sequential",
        "--min-pointer-sequential-lease-ratio",
    ),
    (
        "min_pointer_random_lease_ratio",
        "pointer_random",
        "--min-pointer-random-lease-ratio",
    ),
    (
        "min_overlay_modified_lease_ratio",
        "overlay_modified",
        "--min-overlay-modified-lease-ratio",
    ),
)

RATIO_SCENARIOS = (
    (
        "pointer_sequential",
        "pointer_sequential_lease_read",
        "pointer_sequential_path_read",
    ),
    ("pointer_random", "pointer_random_lease_read", "pointer_random_path_read"),
    (
        "overlay_modified",
        "overlay_modified_lease_reread",
        "overlay_modified_path_reread",
    ),
)

NUMERIC_RECORD_FIELDS = (
    "file_size",
    "chunk_size",
    "read_size",
    "reads",
    "bytes_returned",
    "elapsed_ms",
    "mib_per_sec",
)


def crab_dir() -> Path:
    return Path(__file__).resolve().parents[1]


def now_iso() -> str:
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def benchmark_run_id_suffix(run_id: Any) -> str | None:
    if not isinstance(run_id, str) or not run_id:
        return None
    prefix = f"{BENCHMARK_SUITE}-"
    if not run_id.startswith(prefix):
        return None
    suffix = run_id[len(prefix) :]
    if not suffix:
        return None
    return suffix


def benchmark_run_id() -> str:
    configured = os.environ.get("CRAB_NFS_READ_PATH_BENCH_RUN_ID")
    if configured:
        return configured
    timestamp = datetime.now(UTC).strftime("%Y%m%d-%H%M%S")
    return f"{BENCHMARK_SUITE}-{timestamp}"


def run_text(command: list[str], cwd: Path) -> str | None:
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def git_metadata(cwd: Path) -> dict[str, Any]:
    commit = run_text(["git", "rev-parse", "HEAD"], cwd)
    dirty = subprocess.run(
        ["git", "diff", "--quiet", "--"],
        cwd=cwd,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode != 0
    return {"commit": commit, "dirty": dirty}


def is_full_git_object_id(value: Any) -> bool:
    return (
        isinstance(value, str)
        and len(value) in (40, 64)
        and all(char in "0123456789abcdef" for char in value)
    )


def tool_versions(cwd: Path) -> dict[str, Any]:
    return {
        "cargo": run_text(["cargo", "--version"], cwd),
        "rustc": run_text(["rustc", "--version"], cwd),
    }


def parse_bench_records(stdout: str) -> tuple[list[dict[str, Any]], list[str]]:
    records: list[dict[str, Any]] = []
    errors: list[str] = []
    for line_number, raw_line in enumerate(stdout.splitlines(), start=1):
        line = raw_line.strip()
        if not line:
            continue
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            errors.append(f"stdout line {line_number} is not valid JSON: {error}")
            continue
        if not isinstance(value, dict):
            errors.append(f"stdout line {line_number} must be a JSON object")
            continue
        records.append(value)
    return records, errors


def validate_record(record: dict[str, Any], errors: list[str], index: int) -> None:
    scenario = record.get("scenario")
    if not isinstance(scenario, str) or not scenario:
        errors.append(f"records[{index}].scenario must be a non-empty string")
    for field in NUMERIC_RECORD_FIELDS:
        value = record.get(field)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            errors.append(f"records[{index}].{field} must be numeric")
            continue
        if not math.isfinite(float(value)):
            errors.append(f"records[{index}].{field} must be finite")
            continue
        if field == "elapsed_ms":
            if value < 0:
                errors.append(f"records[{index}].elapsed_ms must be non-negative")
            continue
        if value <= 0:
            errors.append(f"records[{index}].{field} must be positive")
    file_size = record.get("file_size")
    chunk_size = record.get("chunk_size")
    read_size = record.get("read_size")
    if isinstance(file_size, int) and isinstance(chunk_size, int) and file_size % chunk_size != 0:
        errors.append(f"records[{index}].file_size must be a multiple of chunk_size")
    if isinstance(file_size, int) and isinstance(read_size, int) and read_size > file_size:
        errors.append(f"records[{index}].read_size must not exceed file_size")


def validate_records(records: list[dict[str, Any]]) -> list[str]:
    errors: list[str] = []
    seen: set[str] = set()
    for index, record in enumerate(records):
        validate_record(record, errors, index)
        scenario = record.get("scenario")
        if isinstance(scenario, str):
            if scenario in seen:
                errors.append(f"duplicate scenario: {scenario}")
            seen.add(scenario)
    missing = sorted(set(EXPECTED_SCENARIOS) - seen)
    extra = sorted(seen - set(EXPECTED_SCENARIOS))
    if missing:
        errors.append(f"missing scenarios: {', '.join(missing)}")
    if extra:
        errors.append(f"unexpected scenarios: {', '.join(extra)}")
    return errors


def record_by_scenario(records: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    return {str(record["scenario"]): record for record in records}


def throughput_ratio(records: dict[str, dict[str, Any]], numerator: str, denominator: str) -> float:
    top = float(records[numerator]["mib_per_sec"])
    bottom = float(records[denominator]["mib_per_sec"])
    if bottom <= 0.0:
        return 0.0
    return top / bottom


def build_summary(records: list[dict[str, Any]]) -> dict[str, Any]:
    by_scenario = record_by_scenario(records)
    return {
        "scenario_count": len(records),
        "scenarios": list(EXPECTED_SCENARIOS),
        "total_bytes_returned": sum(int(record["bytes_returned"]) for record in records),
        "total_elapsed_ms": sum(int(record["elapsed_ms"]) for record in records),
        "lease_vs_path_mib_per_sec_ratio": {
            key: throughput_ratio(by_scenario, numerator, denominator)
            for key, numerator, denominator in RATIO_SCENARIOS
        },
    }


def validate_report(report: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    if report.get("schema_version") != 1:
        errors.append("schema_version must be 1")
    if report.get("suite") != BENCHMARK_SUITE:
        errors.append(f"suite must be {BENCHMARK_SUITE}")
    if not isinstance(report.get("generated_at"), str) or not report["generated_at"]:
        errors.append("generated_at must be a non-empty string")
    run_id = report.get("run_id")
    run_id_suffix = benchmark_run_id_suffix(run_id)
    if run_id_suffix is None:
        errors.append(f"run_id must start with {BENCHMARK_SUITE}-")
    if report.get("run_id_suffix") != run_id_suffix:
        errors.append("run_id_suffix must match run_id")
    platform_info = report.get("platform")
    if not isinstance(platform_info, dict):
        errors.append("platform must be an object")
    else:
        for field in ("system", "release", "machine", "python"):
            if not isinstance(platform_info.get(field), str) or not platform_info[field]:
                errors.append(f"platform.{field} must be a non-empty string")
    tools = report.get("tools")
    if not isinstance(tools, dict):
        errors.append("tools must be an object")
    else:
        for field in ("cargo", "rustc"):
            if not isinstance(tools.get(field), str) or not tools[field]:
                errors.append(f"tools.{field} must be a non-empty string")
    git = report.get("git")
    if not isinstance(git, dict):
        errors.append("git must be an object")
    else:
        if not is_full_git_object_id(git.get("commit")):
            errors.append("git.commit must be a lowercase full Git object id")
        if not isinstance(git.get("dirty"), bool):
            errors.append("git.dirty must be a boolean")
    if not isinstance(report.get("command"), list) or not all(
        isinstance(item, str) and item for item in report.get("command", [])
    ):
        errors.append("command must be a non-empty string list")
    if not isinstance(report.get("bench_args"), list) or not all(
        isinstance(item, str) for item in report.get("bench_args", [])
    ):
        errors.append("bench_args must be a string list")
    records = report.get("records")
    record_errors: list[str] = []
    if not isinstance(records, list) or not all(isinstance(record, dict) for record in records):
        errors.append("records must be a list of objects")
        records = []
    else:
        record_errors = validate_records(records)
        errors.extend(record_errors)
    summary = report.get("summary")
    if not isinstance(summary, dict):
        errors.append("summary must be an object")
    elif records and not record_errors:
        expected_summary = build_summary(records)
        if summary.get("scenario_count") != expected_summary["scenario_count"]:
            errors.append("summary.scenario_count does not match records")
        if summary.get("total_bytes_returned") != expected_summary["total_bytes_returned"]:
            errors.append("summary.total_bytes_returned does not match records")
        if summary.get("total_elapsed_ms") != expected_summary["total_elapsed_ms"]:
            errors.append("summary.total_elapsed_ms does not match records")
        ratios = summary.get("lease_vs_path_mib_per_sec_ratio")
        if not isinstance(ratios, dict):
            errors.append("summary.lease_vs_path_mib_per_sec_ratio must be an object")
        else:
            expected_ratios = expected_summary["lease_vs_path_mib_per_sec_ratio"]
            for field, _, _ in RATIO_SCENARIOS:
                value = ratios.get(field)
                if isinstance(value, bool) or not isinstance(value, (int, float)):
                    errors.append(
                        f"summary.lease_vs_path_mib_per_sec_ratio.{field} must be numeric"
                    )
                    continue
                if not math.isfinite(float(value)) or value <= 0:
                    errors.append(
                        f"summary.lease_vs_path_mib_per_sec_ratio.{field} must be positive"
                    )
                    continue
                if not math.isclose(
                    float(value),
                    float(expected_ratios[field]),
                    rel_tol=1e-9,
                    abs_tol=1e-12,
                ):
                    errors.append(
                        f"summary.lease_vs_path_mib_per_sec_ratio.{field} does not match records"
                    )
    return errors


def validate_thresholds(report: dict[str, Any], args: argparse.Namespace) -> list[str]:
    errors: list[str] = []
    summary = report.get("summary")
    ratios = summary.get("lease_vs_path_mib_per_sec_ratio") if isinstance(summary, dict) else None
    if not isinstance(ratios, dict):
        return ["summary.lease_vs_path_mib_per_sec_ratio must be an object"]

    for arg_name, ratio_name, flag_name in RATIO_THRESHOLD_ARGS:
        threshold = getattr(args, arg_name, None)
        if threshold is None:
            continue
        if threshold <= 0:
            errors.append(f"{flag_name} must be positive")
            continue
        value = ratios.get(ratio_name)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            errors.append(
                f"summary.lease_vs_path_mib_per_sec_ratio.{ratio_name} must be numeric"
            )
            continue
        if float(value) < threshold:
            errors.append(
                f"summary.lease_vs_path_mib_per_sec_ratio.{ratio_name} is below the configured threshold"
            )
    return errors


def validate_expected_git_commit(report: dict[str, Any], expected: str | None) -> list[str]:
    if expected is None:
        return []
    if not is_full_git_object_id(expected):
        return ["--expected-git-commit must be a lowercase full Git object id"]
    git = report.get("git")
    commit = git.get("commit") if isinstance(git, dict) else None
    if commit != expected:
        return [f"expected git.commit {expected}, got {commit or '<missing>'}"]
    return []


def validate_expected_run_id(report: dict[str, Any], expected: str | None) -> list[str]:
    if expected is None:
        return []
    if benchmark_run_id_suffix(expected) is None:
        return [f"--expected-run-id must start with {BENCHMARK_SUITE}-"]
    run_id = report.get("run_id")
    if run_id != expected:
        return [f"expected run_id {expected}, got {run_id or '<missing>'}"]
    return []


def load_report(path: Path) -> tuple[dict[str, Any] | None, list[str]]:
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return None, [f"failed to read benchmark report {path}: {error}"]
    if not isinstance(report, dict):
        return None, ["report must be a JSON object"]
    errors = validate_report(report)
    return report, errors


def compatible_report_errors(
    baseline: dict[str, Any],
    current: dict[str, Any],
) -> list[str]:
    errors: list[str] = []
    baseline_records = record_by_scenario(baseline["records"])
    current_records = record_by_scenario(current["records"])
    for scenario in EXPECTED_SCENARIOS:
        baseline_record = baseline_records[scenario]
        current_record = current_records[scenario]
        for field in ("file_size", "chunk_size", "read_size", "reads"):
            if baseline_record[field] != current_record[field]:
                errors.append(
                    f"{scenario}.{field} differs between baseline and current reports"
                )
    if baseline.get("bench_args") != current.get("bench_args"):
        errors.append("bench_args differ between baseline and current reports")
    return errors


def percent_change(baseline: float, current: float) -> float:
    return ((current - baseline) / baseline) * 100.0


def build_trend_comparison(
    baseline_path: Path,
    current_path: Path,
    baseline: dict[str, Any],
    current: dict[str, Any],
) -> dict[str, Any]:
    baseline_records = record_by_scenario(baseline["records"])
    current_records = record_by_scenario(current["records"])
    scenario_trends = []
    for scenario in EXPECTED_SCENARIOS:
        baseline_mib = float(baseline_records[scenario]["mib_per_sec"])
        current_mib = float(current_records[scenario]["mib_per_sec"])
        scenario_trends.append(
            {
                "scenario": scenario,
                "baseline_mib_per_sec": baseline_mib,
                "current_mib_per_sec": current_mib,
                "change_pct": percent_change(baseline_mib, current_mib),
            }
        )

    baseline_ratios = baseline["summary"]["lease_vs_path_mib_per_sec_ratio"]
    current_ratios = current["summary"]["lease_vs_path_mib_per_sec_ratio"]
    ratio_trends = {}
    for ratio in ("pointer_sequential", "pointer_random", "overlay_modified"):
        baseline_ratio = float(baseline_ratios[ratio])
        current_ratio = float(current_ratios[ratio])
        ratio_trends[ratio] = {
            "baseline": baseline_ratio,
            "current": current_ratio,
            "change_pct": percent_change(baseline_ratio, current_ratio),
        }

    return {
        "schema_version": 1,
        "suite": "nfs-read-path-bench-comparison",
        "generated_at": now_iso(),
        "baseline_report": str(baseline_path),
        "current_report": str(current_path),
        "baseline_run_id": baseline.get("run_id"),
        "current_run_id": current.get("run_id"),
        "baseline_git": baseline.get("git"),
        "current_git": current.get("git"),
        "bench_args": current.get("bench_args", []),
        "scenario_trends": scenario_trends,
        "lease_vs_path_ratio_trends": ratio_trends,
    }


def validate_comparison_thresholds(
    comparison: dict[str, Any],
    args: argparse.Namespace,
) -> list[str]:
    errors: list[str] = []
    throughput_threshold = args.max_throughput_regression_pct
    if throughput_threshold is not None:
        if throughput_threshold < 0:
            errors.append("--max-throughput-regression-pct must be non-negative")
        else:
            for trend in comparison["scenario_trends"]:
                change = float(trend["change_pct"])
                if change < -throughput_threshold:
                    errors.append(
                        f"{trend['scenario']}.mib_per_sec regressed by {-change:.2f}% "
                        f"which exceeds --max-throughput-regression-pct {throughput_threshold:.2f}%"
                    )

    ratio_threshold = args.max_ratio_regression_pct
    if ratio_threshold is not None:
        if ratio_threshold < 0:
            errors.append("--max-ratio-regression-pct must be non-negative")
        else:
            for ratio, trend in comparison["lease_vs_path_ratio_trends"].items():
                change = float(trend["change_pct"])
                if change < -ratio_threshold:
                    errors.append(
                        f"{ratio} lease/path ratio regressed by {-change:.2f}% "
                        f"which exceeds --max-ratio-regression-pct {ratio_threshold:.2f}%"
                    )
    return errors


def compare_reports(args: argparse.Namespace) -> int:
    baseline_path = Path(args.baseline_report)
    current_path = Path(args.current_report)
    baseline, errors = load_report(baseline_path)
    current, current_errors = load_report(current_path)
    errors.extend(current_errors)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    if baseline is None or current is None:
        print("error: comparison reports were not loaded", file=sys.stderr)
        return 1
    errors = compatible_report_errors(baseline, current)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    comparison = build_trend_comparison(baseline_path, current_path, baseline, current)
    errors = validate_comparison_thresholds(comparison, args)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    if args.output is not None:
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(
            json.dumps(comparison, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"nfs_read_path_bench_comparison={output}")
    print(f"ok: NFS read-path benchmark comparison passed: {baseline_path} -> {current_path}")
    return 0


def run_bench(args: argparse.Namespace) -> int:
    cwd = crab_dir()
    cargo = args.cargo
    bench_args = args.bench_args or []
    command = [
        cargo,
        "bench",
        "--manifest-path",
        str(cwd / "Cargo.toml"),
        "--bench",
        "nfs_read_path_bench",
        "--no-default-features",
        "--features",
        "nfs",
        "--",
        *bench_args,
    ]
    result = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode != 0:
        return result.returncode

    records, parse_errors = parse_bench_records(result.stdout)
    record_errors = validate_records(records)
    errors = parse_errors + record_errors
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    run_id = benchmark_run_id()
    report = {
        "schema_version": 1,
        "suite": BENCHMARK_SUITE,
        "generated_at": now_iso(),
        "run_id": run_id,
        "run_id_suffix": benchmark_run_id_suffix(run_id),
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "git": git_metadata(cwd),
        "tools": tool_versions(cwd),
        "command": command,
        "bench_args": bench_args,
        "summary": build_summary(records),
        "records": records,
    }
    errors = validate_report(report)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"nfs_read_path_bench_report={output}")
    return 0


def verify_report(args: argparse.Namespace) -> int:
    path = Path(args.report)
    report, errors = load_report(path)
    if report is not None:
        errors.extend(validate_thresholds(report, args))
        errors.extend(validate_expected_git_commit(report, args.expected_git_commit))
        errors.extend(validate_expected_run_id(report, args.expected_run_id))
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"ok: NFS read-path benchmark report verified: {path}")
    return 0


def self_test(_: argparse.Namespace) -> int:
    records = []
    for index, scenario in enumerate(EXPECTED_SCENARIOS, start=1):
        records.append(
            {
                "scenario": scenario,
                "file_size": 1024 * 1024,
                "chunk_size": 64 * 1024,
                "read_size": 64 * 1024,
                "reads": index,
                "bytes_returned": index * 64 * 1024,
                "elapsed_ms": index,
                "mib_per_sec": float(index),
            }
        )
    report = {
        "schema_version": 1,
        "suite": BENCHMARK_SUITE,
        "generated_at": now_iso(),
        "run_id": f"{BENCHMARK_SUITE}-attempt-1",
        "run_id_suffix": "attempt-1",
        "platform": {
            "system": "TestOS",
            "release": "1",
            "machine": "test",
            "python": platform.python_version(),
        },
        "git": {"commit": "0" * 40, "dirty": False},
        "tools": {"cargo": "cargo test", "rustc": "rustc test"},
        "command": ["cargo", "bench"],
        "bench_args": [],
        "summary": build_summary(records),
        "records": records,
    }
    errors = validate_report(report)
    if errors:
        for error in errors:
            print(f"error: self-test valid report failed: {error}", file=sys.stderr)
        return 1

    broken = dict(report)
    broken["records"] = records[:-1]
    if not validate_report(broken):
        print("error: self-test missing scenario was not rejected", file=sys.stderr)
        return 1

    forged_ratio = json.loads(json.dumps(report))
    forged_ratio["summary"]["lease_vs_path_mib_per_sec_ratio"]["pointer_sequential"] = 999.0
    ratio_errors = validate_report(forged_ratio)
    if not any("does not match records" in error for error in ratio_errors):
        print("error: self-test forged benchmark ratio was not rejected", file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "report.json"
        path.write_text(json.dumps(report), encoding="utf-8")
        result = verify_report(
            argparse.Namespace(
                report=str(path),
                min_pointer_sequential_lease_ratio=None,
                min_pointer_random_lease_ratio=None,
                min_overlay_modified_lease_ratio=None,
                expected_git_commit="0" * 40,
                expected_run_id=f"{BENCHMARK_SUITE}-attempt-1",
            )
        )
        if result != 0:
            return result
        commit_errors = validate_expected_git_commit(report, "f" * 40)
        if not any("expected git.commit" in error for error in commit_errors):
            print("error: self-test benchmark git commit mismatch was not rejected", file=sys.stderr)
            return 1
        run_id_errors = validate_expected_run_id(report, f"{BENCHMARK_SUITE}-attempt-2")
        if not any("expected run_id" in error for error in run_id_errors):
            print("error: self-test benchmark run id mismatch was not rejected", file=sys.stderr)
            return 1
        threshold_errors = validate_thresholds(
            report,
            argparse.Namespace(
                min_pointer_sequential_lease_ratio=999.0,
                min_pointer_random_lease_ratio=None,
                min_overlay_modified_lease_ratio=None,
            ),
        )
        if not any("below the configured threshold" in error for error in threshold_errors):
            print("error: self-test benchmark ratio threshold was not rejected", file=sys.stderr)
            return 1
        current = json.loads(json.dumps(report))
        current["git"] = {"commit": "1" * 40, "dirty": False}
        current_records = current["records"]
        current_records[0]["mib_per_sec"] = current_records[0]["mib_per_sec"] * 0.90
        current["summary"] = build_summary(current_records)
        current_path = Path(tmp) / "current.json"
        comparison_path = Path(tmp) / "comparison.json"
        current_path.write_text(json.dumps(current), encoding="utf-8")
        result = compare_reports(
            argparse.Namespace(
                baseline_report=str(path),
                current_report=str(current_path),
                max_throughput_regression_pct=20.0,
                max_ratio_regression_pct=None,
                output=str(comparison_path),
            )
        )
        if result != 0:
            return result
        comparison = json.loads(comparison_path.read_text(encoding="utf-8"))
        if comparison.get("suite") != "nfs-read-path-bench-comparison":
            print("error: self-test comparison output has wrong suite", file=sys.stderr)
            return 1
        trend_errors = validate_comparison_thresholds(
            comparison,
            argparse.Namespace(
                max_throughput_regression_pct=1.0,
                max_ratio_regression_pct=None,
            ),
        )
        if not any("exceeds --max-throughput-regression-pct" in error for error in trend_errors):
            print("error: self-test benchmark trend regression was not rejected", file=sys.stderr)
            return 1
    print("ok: NFS read-path benchmark report self-test passed")
    return 0


def add_threshold_args(command: argparse.ArgumentParser) -> None:
    command.add_argument(
        "--min-pointer-sequential-lease-ratio",
        type=float,
        help="Fail when pointer sequential lease/path throughput ratio is below this value.",
    )
    command.add_argument(
        "--min-pointer-random-lease-ratio",
        type=float,
        help="Fail when pointer random lease/path throughput ratio is below this value.",
    )
    command.add_argument(
        "--min-overlay-modified-lease-ratio",
        type=float,
        help="Fail when overlay-modified lease/path throughput ratio is below this value.",
    )


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    run = subcommands.add_parser("run", help="run the benchmark and write a report")
    run.add_argument("--output", default="nfs-read-path-bench-report.json")
    run.add_argument("--cargo", default="cargo")
    run.add_argument("bench_args", nargs=argparse.REMAINDER)
    run.set_defaults(func=run_bench)

    verify = subcommands.add_parser("verify", help="verify an existing report")
    verify.add_argument("report")
    verify.add_argument(
        "--expected-git-commit",
        help="Require report git.commit to equal this full Git object id.",
    )
    verify.add_argument(
        "--expected-run-id",
        help=f"Require report run_id to equal this {BENCHMARK_SUITE}-prefixed id.",
    )
    add_threshold_args(verify)
    verify.set_defaults(func=verify_report)

    compare = subcommands.add_parser("compare", help="compare retained benchmark reports")
    compare.add_argument("baseline_report")
    compare.add_argument("current_report")
    compare.add_argument(
        "--max-throughput-regression-pct",
        type=float,
        help="Fail when any scenario throughput regresses by more than this percentage.",
    )
    compare.add_argument(
        "--max-ratio-regression-pct",
        type=float,
        help="Fail when any lease/path ratio regresses by more than this percentage.",
    )
    compare.add_argument("--output", help="Write a JSON comparison summary.")
    compare.set_defaults(func=compare_reports)

    self_test_parser = subcommands.add_parser("self-test", help="exercise verifier regressions")
    self_test_parser.set_defaults(func=self_test)
    return root


def main() -> int:
    args = parser().parse_args()
    if getattr(args, "bench_args", None) and args.bench_args[0] == "--":
        args.bench_args = args.bench_args[1:]
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
