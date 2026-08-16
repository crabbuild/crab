#!/usr/bin/env python3
"""Render retained NFS evidence JSON as Markdown summaries."""

from __future__ import annotations

import argparse
import json
import math
import sys
import tempfile
from pathlib import Path
from typing import Any


class EvidenceSummaryError(Exception):
    """Raised when retained evidence cannot be summarized."""


BENCHMARK_SUITE = "nfs-read-path-bench"
BENCHMARK_REPORT_NAMES = ("nfs-read-path-bench-report.json",)
BENCHMARK_EXPECTED_SCENARIOS = (
    "pointer_sequential_path_read",
    "pointer_sequential_lease_read",
    "pointer_random_path_read",
    "pointer_random_lease_read",
    "overlay_modified_path_reread",
    "overlay_modified_lease_reread",
)
BENCHMARK_RATIO_KEYS = (
    "pointer_sequential",
    "pointer_random",
    "overlay_modified",
)
BENCHMARK_RATIO_SCENARIOS = (
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
BENCHMARK_NUMERIC_RECORD_FIELDS = (
    "file_size",
    "chunk_size",
    "read_size",
    "reads",
    "bytes_returned",
    "elapsed_ms",
    "mib_per_sec",
)
SMOKE_SUMMARY_NAMES = (
    "nfs-smoke-retained-summary.json",
    "nfs-release-smoke-summary.json",
)
RELEASE_SMOKE_PLATFORMS = ("linux", "macos", "windows")
RELEASE_SMOKE_SUITES = ("mount-nfs-linux", "mount-nfs-macos", "mount-nfs-windows")
RELEASE_SMOKE_SUITE_PLATFORMS = {
    "mount-nfs-linux": "linux",
    "mount-nfs-macos": "macos",
    "mount-nfs-windows": "windows",
}


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceSummaryError(f"failed to read {path}: {error}") from error
    if not isinstance(value, dict):
        raise EvidenceSummaryError(f"{path} must contain a JSON object")
    return value


def optional_json(path: Path | None, allow_missing: bool) -> dict[str, Any] | None:
    if path is None:
        return None
    if not path.exists():
        if allow_missing:
            return None
        raise EvidenceSummaryError(f"missing evidence JSON: {path}")
    return load_json(path)


def md(value: Any) -> str:
    return str(value).replace("\n", " ").replace("|", "\\|")


def short_commit(value: Any) -> str:
    if not isinstance(value, str) or not value:
        return "unknown"
    return value[:12]


def number(value: Any, digits: int = 2) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "n/a"
    if isinstance(value, int):
        return str(value)
    return f"{value:.{digits}f}"


def pct(value: Any) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "n/a"
    return f"{value:+.2f}%"


def bytes_mib(value: Any) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "n/a"
    return f"{float(value) / (1024 * 1024):.2f} MiB"


def metric_value(metrics: dict[str, Any], key: str) -> Any:
    value = metrics.get(key)
    if isinstance(value, dict):
        return value.get("value")
    return None


def metric_regression(metrics: dict[str, Any], key: str) -> Any:
    value = metrics.get(key)
    if isinstance(value, dict):
        return value.get("regression_pct")
    return None


def doctor_state(report: dict[str, Any]) -> str:
    doctor = report.get("mount_doctor")
    if not isinstance(doctor, dict):
        return "n/a"
    errors = doctor.get("errors")
    if isinstance(errors, list) and errors:
        return "error"
    if doctor.get("ready") is True and doctor.get("nfs_preflight_ready") is True:
        return "ready"
    return "blocked"


def doctor_warnings(report: dict[str, Any]) -> Any:
    doctor = report.get("mount_doctor")
    if not isinstance(doctor, dict):
        return None
    return doctor.get("nfs_preflight_warnings", doctor.get("warn"))


def finite_float(value: Any, field: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise EvidenceSummaryError(f"{field} must be numeric")
    result = float(value)
    if result != result or result in (float("inf"), float("-inf")):
        raise EvidenceSummaryError(f"{field} must be finite")
    return result


def fmt_arg(value: float) -> str:
    return f"{value:.4f}".rstrip("0").rstrip(".")


def paths_arg(value: Any) -> list[Path]:
    if value is None:
        return []
    if isinstance(value, list):
        return value
    return [value]


def unique_paths(paths: list[Path]) -> list[Path]:
    unique = []
    seen: set[str] = set()
    for path in paths:
        key = str(path)
        if key in seen:
            continue
        seen.add(key)
        unique.append(path)
    return unique


def discover_named_files(
    directories: list[Path],
    names: tuple[str, ...],
    label: str,
) -> list[Path]:
    discovered: list[Path] = []
    for directory in directories:
        if not directory.exists():
            raise EvidenceSummaryError(f"{label} directory does not exist: {directory}")
        if not directory.is_dir():
            raise EvidenceSummaryError(f"{label} path must be a directory: {directory}")
        matches = sorted(
            path
            for name in names
            for path in directory.rglob(name)
            if path.is_file()
        )
        if not matches:
            joined = ", ".join(names)
            raise EvidenceSummaryError(
                f"{label} directory contains no retained evidence files: {joined}"
            )
        discovered.extend(matches)
    return discovered


def retained_evidence_paths(
    files: Any,
    directories: Any,
    names: tuple[str, ...],
    label: str,
) -> list[Path]:
    return unique_paths(
        [
            *paths_arg(files),
            *discover_named_files(paths_arg(directories), names, label),
        ]
    )


def benchmark_missing_markdown(path: Path) -> str:
    return "\n".join(
        (
            "## NFS Read-Path Benchmark Evidence",
            "",
            f"No benchmark report found at `{md(path)}`.",
            "",
        )
    )


def render_benchmark_markdown(
    report_path: Path,
    comparison_path: Path | None,
    *,
    allow_missing: bool,
) -> str:
    if not report_path.exists():
        if allow_missing:
            return benchmark_missing_markdown(report_path)
        raise EvidenceSummaryError(f"missing benchmark report: {report_path}")

    report = load_json(report_path)
    comparison = optional_json(comparison_path, allow_missing)
    summary = report.get("summary") if isinstance(report.get("summary"), dict) else {}
    ratios = (
        summary.get("lease_vs_path_mib_per_sec_ratio")
        if isinstance(summary.get("lease_vs_path_mib_per_sec_ratio"), dict)
        else {}
    )
    records = report.get("records") if isinstance(report.get("records"), list) else []
    git = report.get("git") if isinstance(report.get("git"), dict) else {}

    lines = [
        "## NFS Read-Path Benchmark Evidence",
        "",
        f"Report: `{md(report_path)}`",
        f"Commit: `{md(git.get('commit', 'unknown'))}` dirty={md(git.get('dirty', 'unknown'))}",
        "",
        "| Ratio | Lease/path MiB/s |",
        "|---|---:|",
    ]
    for key, label in (
        ("pointer_sequential", "Pointer sequential"),
        ("pointer_random", "Pointer random"),
        ("overlay_modified", "Overlay modified"),
    ):
        lines.append(f"| {label} | {number(ratios.get(key), 3)} |")

    lines.extend(
        (
            "",
            "| Scenario | MiB/s | Reads | Returned |",
            "|---|---:|---:|---:|",
        )
    )
    for record in records:
        if not isinstance(record, dict):
            continue
        lines.append(
            "| "
            f"{md(record.get('scenario', 'unknown'))} | "
            f"{number(record.get('mib_per_sec'))} | "
            f"{number(record.get('reads'), 0)} | "
            f"{bytes_mib(record.get('bytes_returned'))} |"
        )

    if comparison is not None:
        lines.extend(
            (
                "",
                "### Benchmark Trend",
                "",
                "| Scenario | Baseline MiB/s | Current MiB/s | Change |",
                "|---|---:|---:|---:|",
            )
        )
        trends = comparison.get("scenario_trends")
        if isinstance(trends, list):
            for trend in trends:
                if not isinstance(trend, dict):
                    continue
                lines.append(
                    "| "
                    f"{md(trend.get('scenario', 'unknown'))} | "
                    f"{number(trend.get('baseline_mib_per_sec'))} | "
                    f"{number(trend.get('current_mib_per_sec'))} | "
                    f"{pct(trend.get('change_pct'))} |"
                )

        lines.extend(
            (
                "",
                "| Ratio | Baseline | Current | Change |",
                "|---|---:|---:|---:|",
            )
        )
        ratio_trends = comparison.get("lease_vs_path_ratio_trends")
        if isinstance(ratio_trends, dict):
            for key, trend in ratio_trends.items():
                if not isinstance(trend, dict):
                    continue
                lines.append(
                    "| "
                    f"{md(key)} | "
                    f"{number(trend.get('baseline'), 3)} | "
                    f"{number(trend.get('current'), 3)} | "
                    f"{pct(trend.get('change_pct'))} |"
                )

    lines.append("")
    return "\n".join(lines)


def smoke_missing_markdown(path: Path) -> str:
    return "\n".join(
        (
            "## Retained Native NFS Smoke Evidence",
            "",
            f"No retained smoke summary found at `{md(path)}`.",
            "",
        )
    )


def render_smoke_markdown(
    summary_path: Path,
    comparison_path: Path | None,
    *,
    allow_missing: bool,
) -> str:
    if not summary_path.exists():
        if allow_missing:
            return smoke_missing_markdown(summary_path)
        raise EvidenceSummaryError(f"missing retained smoke summary: {summary_path}")

    summary = load_json(summary_path)
    comparison = optional_json(comparison_path, allow_missing)
    reports = summary.get("reports") if isinstance(summary.get("reports"), list) else []
    suites = summary.get("suites") if isinstance(summary.get("suites"), list) else []
    platforms = summary.get("platforms") if isinstance(summary.get("platforms"), list) else []

    lines = [
        "## Retained Native NFS Smoke Evidence",
        "",
        f"Summary: `{md(summary_path)}`",
        f"Evidence commit: `{md(short_commit(summary.get('git_commit')))}`",
        f"Run suffix: `{md(summary.get('run_id_suffix', 'unknown'))}`",
        f"Reports: {number(summary.get('report_count'), 0)}",
        f"Suites: {md(', '.join(str(suite) for suite in suites))}",
        f"Platforms: {md(', '.join(str(platform) for platform in platforms))}",
        "",
        "| Suite | Platform | Run ID | Commit | Doctor | Doctor warns | MiB/s | RPC/MiB | Lease hits/MiB | Lease misses/MiB | VFS calls/MiB | Hydration remote/user byte |",
        "|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for report in reports:
        if not isinstance(report, dict):
            continue
        native_read = report.get("native_read")
        metrics = native_read.get("metrics") if isinstance(native_read, dict) else {}
        if not isinstance(metrics, dict):
            metrics = {}
        lines.append(
            "| "
            f"{md(report.get('suite', 'unknown'))} | "
            f"{md(report.get('platform', 'unknown'))} | "
            f"{md(report.get('run_id', 'unknown'))} | "
            f"{md(short_commit(report.get('git_commit')))} | "
            f"{md(doctor_state(report))} | "
            f"{number(doctor_warnings(report), 0)} | "
            f"{number(metric_value(metrics, 'mib_per_sec'))} | "
            f"{number(metric_value(metrics, 'read_rpcs_per_mib'))} | "
            f"{number(metric_value(metrics, 'read_lease_hits_per_mib'))} | "
            f"{number(metric_value(metrics, 'read_lease_misses_per_mib'))} | "
            f"{number(metric_value(metrics, 'vfs_read_calls_per_mib'))} | "
            f"{number(metric_value(metrics, 'hydration_remote_bytes_per_user_byte'), 3)} |"
        )

    if comparison is not None:
        comparisons = (
            comparison.get("comparisons")
            if isinstance(comparison.get("comparisons"), list)
            else []
        )
        lines.extend(
            (
                "",
                "### Native Smoke Trend",
                "",
                "| Suite | Platform | MiB/s regression | RPC density regression | Lease-hit regression | Lease-miss regression | VFS call-density regression | Hydration remote-byte regression |",
                "|---|---|---:|---:|---:|---:|---:|---:|",
            )
        )
        for item in comparisons:
            if not isinstance(item, dict):
                continue
            native_read = item.get("native_read")
            metrics = native_read.get("metrics") if isinstance(native_read, dict) else {}
            if not isinstance(metrics, dict):
                metrics = {}
            lines.append(
                "| "
                f"{md(item.get('smoke_suite', 'unknown'))} | "
                f"{md(item.get('platform', 'unknown'))} | "
                f"{number(metric_regression(metrics, 'mib_per_sec'))}% | "
                f"{number(metric_regression(metrics, 'read_rpcs_per_mib'))}% | "
                f"{number(metric_regression(metrics, 'read_lease_hits_per_mib'))}% | "
                f"{number(metric_regression(metrics, 'read_lease_misses_per_mib'))}% | "
                f"{number(metric_regression(metrics, 'vfs_read_calls_per_mib'))}% | "
                f"{number(metric_regression(metrics, 'hydration_remote_bytes_per_user_byte'))}% |"
            )

    lines.append("")
    return "\n".join(lines)


def suggest_benchmark_verify_args(
    reports: list[dict[str, Any]],
    *,
    margin_pct: float,
) -> str:
    margin_pct = finite_float(margin_pct, "--benchmark-margin-pct")
    if not reports:
        raise EvidenceSummaryError("at least one benchmark report is required")

    factor = 1.0 - (margin_pct / 100.0)
    if factor <= 0.0:
        raise EvidenceSummaryError("--benchmark-margin-pct must be below 100")

    flags = (
        (
            "--min-pointer-sequential-lease-ratio",
            "pointer_sequential",
        ),
        ("--min-pointer-random-lease-ratio", "pointer_random"),
        ("--min-overlay-modified-lease-ratio", "overlay_modified"),
    )
    values: dict[str, list[float]] = {key: [] for _, key in flags}
    for index, report in enumerate(reports):
        summary = report.get("summary")
        if not isinstance(summary, dict):
            raise EvidenceSummaryError(
                f"benchmark reports[{index}].summary must be an object"
            )
        ratios = summary.get("lease_vs_path_mib_per_sec_ratio")
        if not isinstance(ratios, dict):
            raise EvidenceSummaryError(
                "benchmark reports"
                f"[{index}].summary.lease_vs_path_mib_per_sec_ratio must be an object"
            )
        for _, key in flags:
            value = finite_float(ratios.get(key), f"benchmark ratio {key}")
            if value <= 0.0:
                raise EvidenceSummaryError(f"benchmark ratio {key} must be positive")
            values[key].append(value)

    parts = []
    for flag, key in flags:
        value = min(values[key]) * factor
        parts.append(f"{flag} {fmt_arg(value)}")
    return " ".join(parts)


def suggest_smoke_verify_args(
    summaries: list[dict[str, Any]],
    *,
    margin_pct: float,
) -> str:
    margin_pct = finite_float(margin_pct, "--smoke-margin-pct")
    if not summaries:
        raise EvidenceSummaryError("at least one smoke summary is required")

    values: dict[str, list[float]] = {
        "mib_per_sec": [],
        "requested_bytes_per_user_byte": [],
        "returned_bytes_per_user_byte": [],
        "read_rpcs_per_mib": [],
        "read_lease_hits_per_mib": [],
        "read_lease_misses_per_mib": [],
    }
    for summary_index, summary in enumerate(summaries):
        reports = summary.get("reports")
        if not isinstance(reports, list) or not reports:
            raise EvidenceSummaryError(
                f"smoke summaries[{summary_index}].reports must be a non-empty list"
            )
        for report_index, report in enumerate(reports):
            if not isinstance(report, dict):
                raise EvidenceSummaryError(
                    "smoke summaries"
                    f"[{summary_index}].reports[{report_index}] must be an object"
                )
            native_read = report.get("native_read")
            metrics = native_read.get("metrics") if isinstance(native_read, dict) else None
            if not isinstance(metrics, dict):
                raise EvidenceSummaryError(
                    "smoke summaries"
                    f"[{summary_index}].reports[{report_index}].native_read.metrics"
                    " must be an object"
                )
            for key in values:
                metric = metrics.get(key)
                if not isinstance(metric, dict):
                    raise EvidenceSummaryError(f"smoke metric {key} must be an object")
                values[key].append(
                    finite_float(metric.get("value"), f"smoke metric {key}.value")
                )

    low_factor = 1.0 - (margin_pct / 100.0)
    high_factor = 1.0 + (margin_pct / 100.0)
    if low_factor <= 0.0:
        raise EvidenceSummaryError("--smoke-margin-pct must be below 100")

    suggestions = {
        "--min-native-read-mib-per-sec": min(values["mib_per_sec"]) * low_factor,
        "--max-native-read-requested-bytes-per-user-byte": max(
            values["requested_bytes_per_user_byte"]
        )
        * high_factor,
        "--max-native-read-returned-bytes-per-user-byte": max(
            values["returned_bytes_per_user_byte"]
        )
        * high_factor,
        "--max-native-read-rpcs-per-mib": max(values["read_rpcs_per_mib"]) * high_factor,
        "--min-native-read-lease-hits-per-mib": min(
            values["read_lease_hits_per_mib"]
        )
        * low_factor,
        "--max-native-read-lease-misses-per-mib": max(
            values["read_lease_misses_per_mib"]
        )
        * high_factor,
    }
    return " ".join(f"{flag} {fmt_arg(value)}" for flag, value in suggestions.items())


def smoke_summary_reports(summaries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    reports: list[dict[str, Any]] = []
    for summary in summaries:
        raw_reports = summary.get("reports")
        if not isinstance(raw_reports, list):
            continue
        for report in raw_reports:
            if not isinstance(report, dict):
                continue
            reports.append(report)
    return reports


def string_list(value: Any) -> list[str] | None:
    if not isinstance(value, list):
        return None
    result: list[str] = []
    for item in value:
        if not isinstance(item, str) or not item:
            return None
        result.append(item)
    return result


def report_strings(reports: list[dict[str, Any]], key: str) -> list[str]:
    values = {
        value
        for report in reports
        for value in [report.get(key)]
        if isinstance(value, str) and value
    }
    return sorted(values)


def report_string_values(reports: list[dict[str, Any]], key: str) -> list[str]:
    return [
        value
        for report in reports
        for value in [report.get(key)]
        if isinstance(value, str) and value
    ]


def smoke_summary_report_count(summaries: list[dict[str, Any]]) -> int:
    return len(smoke_summary_reports(summaries))


def smoke_report_run_suffix(report: dict[str, Any]) -> str | None:
    suite = report.get("suite")
    run_id = report.get("run_id")
    if not isinstance(suite, str) or not suite:
        return None
    if not isinstance(run_id, str) or not run_id:
        return None
    prefix = f"{suite}-"
    if not run_id.startswith(prefix):
        return None
    suffix = run_id[len(prefix) :]
    if not suffix:
        return None
    return suffix


def benchmark_report_run_suffix(report: dict[str, Any]) -> str | None:
    run_id = report.get("run_id")
    if not isinstance(run_id, str) or not run_id:
        return None
    prefix = f"{BENCHMARK_SUITE}-"
    if not run_id.startswith(prefix):
        return None
    suffix = run_id[len(prefix) :]
    if not suffix:
        return None
    return suffix


def full_git_commit(value: Any) -> str | None:
    if not isinstance(value, str) or len(value) != 40:
        return None
    if value != value.lower():
        return None
    if not all(char in "0123456789abcdef" for char in value):
        return None
    return value


def smoke_summary_git_commits(summaries: list[dict[str, Any]]) -> tuple[list[str], int]:
    commits: set[str] = set()
    invalid_count = 0
    for report in smoke_summary_reports(summaries):
        commit = full_git_commit(report.get("git_commit"))
        if commit is None:
            invalid_count += 1
            continue
        commits.add(commit)
    return sorted(commits), invalid_count


def benchmark_git_commits(reports: list[dict[str, Any]]) -> tuple[list[str], int]:
    commits: set[str] = set()
    invalid_count = 0
    for report in reports:
        git = report.get("git")
        commit = git.get("commit") if isinstance(git, dict) else None
        full_commit = full_git_commit(commit)
        if full_commit is None:
            invalid_count += 1
            continue
        commits.add(full_commit)
    return sorted(commits), invalid_count


def benchmark_run_id_suffixes(reports: list[dict[str, Any]]) -> tuple[list[str], int]:
    suffixes: set[str] = set()
    invalid_count = 0
    for report in reports:
        suffix = benchmark_report_run_suffix(report)
        if suffix is None:
            invalid_count += 1
            continue
        suffixes.add(suffix)
    return sorted(suffixes), invalid_count


def smoke_summary_run_id_suffixes(summaries: list[dict[str, Any]]) -> list[str]:
    suffixes = {
        suffix
        for summary in summaries
        for suffix in [summary.get("run_id_suffix")]
        if isinstance(suffix, str) and suffix
    }
    return sorted(suffixes)


def benchmark_expected_ratios(records: list[dict[str, Any]]) -> dict[str, float] | None:
    by_scenario = {
        record["scenario"]: record
        for record in records
        if isinstance(record.get("scenario"), str)
    }
    ratios: dict[str, float] = {}
    for key, numerator, denominator in BENCHMARK_RATIO_SCENARIOS:
        numerator_record = by_scenario.get(numerator)
        denominator_record = by_scenario.get(denominator)
        if numerator_record is None or denominator_record is None:
            return None
        top = numerator_record.get("mib_per_sec")
        bottom = denominator_record.get("mib_per_sec")
        if isinstance(top, bool) or isinstance(bottom, bool):
            return None
        if not isinstance(top, (int, float)) or not isinstance(bottom, (int, float)):
            return None
        if not math.isfinite(float(top)) or not math.isfinite(float(bottom)):
            return None
        if float(bottom) <= 0.0:
            return None
        ratios[key] = float(top) / float(bottom)
    return ratios


def benchmark_consistency_blockers(reports: list[dict[str, Any]]) -> list[str]:
    blockers: list[str] = []
    for index, report in enumerate(reports):
        label = f"benchmark report {index + 1}"
        if report.get("schema_version") != 1:
            blockers.append(f"{label} schema_version is not 1")
        if report.get("suite") != BENCHMARK_SUITE:
            blockers.append(f"{label} suite is not {BENCHMARK_SUITE}")
        run_suffix = benchmark_report_run_suffix(report)
        if run_suffix is None:
            blockers.append(f"{label} run_id must start with {BENCHMARK_SUITE}-")
        if report.get("run_id_suffix") != run_suffix:
            blockers.append(f"{label} run_id_suffix must match run_id")
        git = report.get("git")
        if not isinstance(git, dict):
            blockers.append(f"{label} git must be an object")
            continue
        dirty = git.get("dirty")
        if not isinstance(dirty, bool):
            blockers.append(f"{label} git.dirty must be a boolean")
        elif dirty:
            blockers.append(f"{label} git.dirty must be false for promotable evidence")
        records = report.get("records")
        if not isinstance(records, list) or not all(
            isinstance(record, dict) for record in records
        ):
            blockers.append(f"{label} records must be a list of objects")
            records = []
        for record_index, record in enumerate(records):
            for field in BENCHMARK_NUMERIC_RECORD_FIELDS:
                value = record.get(field)
                record_field = f"{label} records[{record_index}].{field}"
                if isinstance(value, bool) or not isinstance(value, (int, float)):
                    blockers.append(f"{record_field} must be numeric")
                    continue
                if not math.isfinite(float(value)):
                    blockers.append(f"{record_field} must be finite")
                    continue
                if field == "elapsed_ms":
                    if value < 0:
                        blockers.append(f"{record_field} must be non-negative")
                    continue
                if value <= 0:
                    blockers.append(f"{record_field} must be positive")
        record_scenarios = report_string_values(records, "scenario")
        unique_record_scenarios = sorted(set(record_scenarios))
        missing_scenarios = sorted(set(BENCHMARK_EXPECTED_SCENARIOS) - set(record_scenarios))
        extra_scenarios = sorted(set(record_scenarios) - set(BENCHMARK_EXPECTED_SCENARIOS))
        if len(record_scenarios) != len(records):
            blockers.append(f"{label} every record must have a scenario")
        if len(record_scenarios) != len(unique_record_scenarios):
            blockers.append(f"{label} scenarios must be unique")
        if missing_scenarios:
            blockers.append(
                f"{label} missing scenario(s): " + ", ".join(missing_scenarios)
            )
        if extra_scenarios:
            blockers.append(
                f"{label} unexpected scenario(s): " + ", ".join(extra_scenarios)
            )
        summary = report.get("summary")
        if not isinstance(summary, dict):
            blockers.append(f"{label} summary must be an object")
            continue
        if summary.get("scenario_count") != len(records):
            blockers.append(f"{label} summary.scenario_count must match records")
        total_bytes = sum(
            int(record["bytes_returned"])
            for record in records
            if isinstance(record.get("bytes_returned"), int)
            and not isinstance(record.get("bytes_returned"), bool)
        )
        if summary.get("total_bytes_returned") != total_bytes:
            blockers.append(f"{label} summary.total_bytes_returned must match records")
        total_elapsed = sum(
            int(record["elapsed_ms"])
            for record in records
            if isinstance(record.get("elapsed_ms"), int)
            and not isinstance(record.get("elapsed_ms"), bool)
        )
        if summary.get("total_elapsed_ms") != total_elapsed:
            blockers.append(f"{label} summary.total_elapsed_ms must match records")
        summary_scenarios = string_list(summary.get("scenarios"))
        if summary_scenarios is None:
            blockers.append(f"{label} summary.scenarios must be a string list")
        elif summary_scenarios != list(BENCHMARK_EXPECTED_SCENARIOS):
            blockers.append(f"{label} summary.scenarios must match expected scenarios")
        ratios = summary.get("lease_vs_path_mib_per_sec_ratio")
        if not isinstance(ratios, dict):
            blockers.append(
                f"{label} summary.lease_vs_path_mib_per_sec_ratio must be an object"
            )
            continue
        actual_ratios: dict[str, float] = {}
        for key in BENCHMARK_RATIO_KEYS:
            value = ratios.get(key)
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                blockers.append(f"{label} ratio {key} must be numeric")
                continue
            if float(value) <= 0.0:
                blockers.append(f"{label} ratio {key} must be positive")
                continue
            actual_ratios[key] = float(value)
        expected_ratios = benchmark_expected_ratios(records)
        if expected_ratios is None:
            continue
        for key, expected_ratio in expected_ratios.items():
            actual_ratio = actual_ratios.get(key)
            if actual_ratio is None:
                continue
            if not math.isclose(
                actual_ratio,
                expected_ratio,
                rel_tol=1e-9,
                abs_tol=1e-12,
            ):
                blockers.append(f"{label} ratio {key} must match records")
    return blockers


def smoke_summary_consistency_blockers(summaries: list[dict[str, Any]]) -> list[str]:
    blockers: list[str] = []
    for index, summary in enumerate(summaries):
        label = f"smoke summary {index + 1}"
        if summary.get("schema_version") != 1:
            blockers.append(f"{label} schema_version is not 1")
        if summary.get("status") != "ok":
            blockers.append(f"{label} status is not ok")
        reports = summary.get("reports")
        if not isinstance(reports, list):
            blockers.append(f"{label} reports must be a list")
            continue
        report_objects = [report for report in reports if isinstance(report, dict)]
        if len(report_objects) != len(reports):
            blockers.append(f"{label} reports must contain only objects")
        summary_commit = full_git_commit(summary.get("git_commit"))
        if summary_commit is None:
            blockers.append(f"{label} git_commit must be a full Git object id")
        report_commits = sorted(
            {
                commit
                for report in report_objects
                for commit in [full_git_commit(report.get("git_commit"))]
                if commit is not None
            }
        )
        if summary_commit is not None and report_commits != [summary_commit]:
            blockers.append(f"{label} git_commit must match report rows")
        summary_run_suffix = summary.get("run_id_suffix")
        if not isinstance(summary_run_suffix, str) or not summary_run_suffix:
            blockers.append(f"{label} run_id_suffix must be a non-empty string")
        report_run_suffixes = sorted(
            {
                suffix
                for report in report_objects
                for suffix in [smoke_report_run_suffix(report)]
                if suffix is not None
            }
        )
        invalid_run_suffix_count = len(report_objects) - sum(
            1 for report in report_objects if smoke_report_run_suffix(report) is not None
        )
        if invalid_run_suffix_count:
            blockers.append(f"{label} reports must use suite-prefixed run_id values")
        if (
            isinstance(summary_run_suffix, str)
            and summary_run_suffix
            and report_run_suffixes != [summary_run_suffix]
        ):
            blockers.append(f"{label} run_id_suffix must match report rows")
        report_count = summary.get("report_count")
        if not isinstance(report_count, int) or isinstance(report_count, bool):
            blockers.append(f"{label} report_count must be an integer")
        elif report_count != len(report_objects):
            blockers.append(
                f"{label} report_count {report_count} does not match "
                f"{len(report_objects)} report row(s)"
            )
        suites = string_list(summary.get("suites"))
        report_suites = report_strings(report_objects, "suite")
        if suites is None:
            blockers.append(f"{label} suites must be a string list")
        elif sorted(suites) != report_suites:
            blockers.append(f"{label} suites must match report rows")
        platforms = string_list(summary.get("platforms"))
        report_platforms = report_strings(report_objects, "platform")
        if platforms is None:
            blockers.append(f"{label} platforms must be a string list")
        elif sorted(platforms) != report_platforms:
            blockers.append(f"{label} platforms must match report rows")
        for report in report_objects:
            suite = report.get("suite")
            platform = report.get("platform")
            expected_platform = (
                RELEASE_SMOKE_SUITE_PLATFORMS.get(suite)
                if isinstance(suite, str)
                else None
            )
            if expected_platform is not None and platform != expected_platform:
                blockers.append(
                    f"{label} {suite} report platform must be {expected_platform}"
                )
    return blockers


def smoke_summary_release_shape_blockers(summaries: list[dict[str, Any]]) -> list[str]:
    blockers: list[str] = []
    for index, summary in enumerate(summaries):
        label = f"smoke summary {index + 1}"
        reports = summary.get("reports")
        if not isinstance(reports, list):
            continue
        report_objects = [report for report in reports if isinstance(report, dict)]
        suites = report_strings(report_objects, "suite")
        platforms = report_strings(report_objects, "platform")
        missing_suites = [suite for suite in RELEASE_SMOKE_SUITES if suite not in suites]
        missing_platforms = [
            platform for platform in RELEASE_SMOKE_PLATFORMS if platform not in platforms
        ]
        if missing_suites:
            blockers.append(
                f"{label} missing release smoke suite(s): "
                + ", ".join(missing_suites)
            )
        if missing_platforms:
            blockers.append(
                f"{label} missing release smoke platform(s): "
                + ", ".join(missing_platforms)
            )
        if len(report_objects) < len(RELEASE_SMOKE_SUITES):
            blockers.append(
                f"{label} found {len(report_objects)} smoke report(s), "
                f"need {len(RELEASE_SMOKE_SUITES)}"
            )
    return blockers


def threshold_evidence_tier(
    *,
    benchmark_paths: list[Path],
    benchmarks: list[dict[str, Any]],
    smoke_paths: list[Path],
    smoke_summaries: list[dict[str, Any]],
    min_benchmark_reports: int,
    min_smoke_summaries: int,
) -> dict[str, Any]:
    benchmark_count = len(benchmark_paths)
    smoke_summary_count = len(smoke_paths)
    smoke_reports = smoke_summary_reports(smoke_summaries)
    smoke_report_count = smoke_summary_report_count(smoke_summaries)
    suites = report_strings(smoke_reports, "suite")
    platforms = report_strings(smoke_reports, "platform")
    run_id_suffixes = smoke_summary_run_id_suffixes(smoke_summaries)
    git_commits, invalid_git_commit_count = smoke_summary_git_commits(smoke_summaries)
    benchmark_commits, invalid_benchmark_git_count = benchmark_git_commits(benchmarks)
    benchmark_run_suffixes, invalid_benchmark_run_id_count = benchmark_run_id_suffixes(
        benchmarks
    )
    benchmark_blockers = benchmark_consistency_blockers(benchmarks)
    consistency_blockers = smoke_summary_consistency_blockers(smoke_summaries)
    release_shape_blockers = smoke_summary_release_shape_blockers(smoke_summaries)
    missing_suites = [suite for suite in RELEASE_SMOKE_SUITES if suite not in suites]
    missing_platforms = [
        platform for platform in RELEASE_SMOKE_PLATFORMS if platform not in platforms
    ]

    release_blockers: list[str] = []
    calibration_blockers: list[str] = []
    if benchmark_count == 0:
        release_blockers.append("missing retained synthetic benchmark report")
        calibration_blockers.append("missing retained synthetic benchmark report")
    if smoke_summary_count == 0:
        release_blockers.append("missing retained native smoke summary")
        calibration_blockers.append("missing retained native smoke summary")
    if benchmark_blockers:
        release_blockers.extend(benchmark_blockers)
        calibration_blockers.extend(benchmark_blockers)
    if consistency_blockers:
        release_blockers.extend(consistency_blockers)
        calibration_blockers.extend(consistency_blockers)
    if release_shape_blockers:
        release_blockers.extend(release_shape_blockers)
        calibration_blockers.extend(release_shape_blockers)
    if missing_suites:
        release_blockers.append(
            "missing native smoke suite(s): " + ", ".join(missing_suites)
        )
        calibration_blockers.append(
            "missing native smoke suite(s): " + ", ".join(missing_suites)
        )
    if missing_platforms:
        release_blockers.append(
            "missing native smoke platform(s): " + ", ".join(missing_platforms)
        )
        calibration_blockers.append(
            "missing native smoke platform(s): " + ", ".join(missing_platforms)
        )
    if smoke_report_count < len(RELEASE_SMOKE_SUITES):
        release_blockers.append(
            "found "
            f"{smoke_report_count} smoke report(s), need {len(RELEASE_SMOKE_SUITES)}"
        )
    if invalid_git_commit_count:
        release_blockers.append(
            "found "
            f"{invalid_git_commit_count} smoke report(s) without a full git_commit"
        )
    if invalid_benchmark_git_count:
        release_blockers.append(
            "found "
            f"{invalid_benchmark_git_count} benchmark report(s) without a full git.commit"
        )
    if invalid_benchmark_run_id_count:
        release_blockers.append(
            "found "
            f"{invalid_benchmark_run_id_count} benchmark report(s) without a valid run_id"
        )
    if smoke_paths and not git_commits:
        release_blockers.append("missing native smoke git_commit")
    if benchmark_paths and not benchmark_commits:
        release_blockers.append("missing benchmark git.commit")
    if len(git_commits) > 1:
        release_blockers.append(
            "native smoke reports span multiple git commits: "
            + ", ".join(commit[:12] for commit in git_commits)
        )
    if len(benchmark_commits) > 1:
        release_blockers.append(
            "benchmark reports span multiple git commits: "
            + ", ".join(commit[:12] for commit in benchmark_commits)
        )
    if len(git_commits) == 1 and len(benchmark_commits) == 1:
        smoke_commit = git_commits[0]
        benchmark_commit = benchmark_commits[0]
        if smoke_commit != benchmark_commit:
            release_blockers.append(
                "benchmark git.commit "
                f"{benchmark_commit[:12]} does not match native smoke git_commit "
                f"{smoke_commit[:12]}"
            )
    if benchmark_count < min_benchmark_reports:
        calibration_blockers.append(
            "found "
            f"{benchmark_count} benchmark report(s), need {min_benchmark_reports}"
        )
    if benchmark_paths and len(benchmark_run_suffixes) < min_benchmark_reports:
        calibration_blockers.append(
            "found "
            f"{len(benchmark_run_suffixes)} benchmark run attempt(s), need "
            f"{min_benchmark_reports}"
        )
    if smoke_summary_count < min_smoke_summaries:
        calibration_blockers.append(
            "found "
            f"{smoke_summary_count} smoke summary file(s), need {min_smoke_summaries}"
        )
    if smoke_paths and len(run_id_suffixes) < min_smoke_summaries:
        calibration_blockers.append(
            "found "
            f"{len(run_id_suffixes)} smoke run attempt(s), need {min_smoke_summaries}"
        )
    if min_benchmark_reports < 2:
        calibration_blockers.append(
            "minimum benchmark report count is below calibration depth 2"
        )
    if min_smoke_summaries < 2:
        calibration_blockers.append(
            "minimum smoke summary count is below calibration depth 2"
        )

    if not calibration_blockers:
        tier = "calibration"
    elif not release_blockers:
        tier = "release-evidence-shaped"
    else:
        tier = "advisory"

    return {
        "tier": tier,
        "release_grade": not release_blockers,
        "calibration_ready": not calibration_blockers,
        "benchmark_report_count": benchmark_count,
        "benchmark_run_attempt_count": len(benchmark_run_suffixes),
        "benchmark_run_id_suffix": benchmark_run_suffixes[0]
        if len(benchmark_run_suffixes) == 1
        else None,
        "benchmark_run_id_suffixes": benchmark_run_suffixes,
        "smoke_summary_count": smoke_summary_count,
        "smoke_report_count": smoke_report_count,
        "smoke_run_attempt_count": len(run_id_suffixes),
        "suites": suites,
        "platforms": platforms,
        "run_id_suffix": run_id_suffixes[0] if len(run_id_suffixes) == 1 else None,
        "run_id_suffixes": run_id_suffixes,
        "git_commit": git_commits[0] if len(git_commits) == 1 else None,
        "git_commits": git_commits,
        "benchmark_git_commit": benchmark_commits[0]
        if len(benchmark_commits) == 1
        else None,
        "benchmark_git_commits": benchmark_commits,
        "required_platforms": list(RELEASE_SMOKE_PLATFORMS),
        "required_suites": list(RELEASE_SMOKE_SUITES),
        "missing_suites": missing_suites,
        "missing_platforms": missing_platforms,
        "release_blockers": release_blockers,
        "calibration_blockers": calibration_blockers,
    }


def suggested_thresholds_payload(args: argparse.Namespace) -> dict[str, Any]:
    benchmark_margin_pct = finite_float(
        args.benchmark_margin_pct,
        "--benchmark-margin-pct",
    )
    smoke_margin_pct = finite_float(args.smoke_margin_pct, "--smoke-margin-pct")
    benchmark_regression_pct = finite_float(
        args.benchmark_regression_pct,
        "--benchmark-regression-pct",
    )
    smoke_regression_pct = finite_float(
        args.smoke_regression_pct,
        "--smoke-regression-pct",
    )

    if benchmark_margin_pct < 0.0:
        raise EvidenceSummaryError("--benchmark-margin-pct must be non-negative")
    if smoke_margin_pct < 0.0:
        raise EvidenceSummaryError("--smoke-margin-pct must be non-negative")
    if benchmark_regression_pct < 0.0:
        raise EvidenceSummaryError("--benchmark-regression-pct must be non-negative")
    if smoke_regression_pct < 0.0:
        raise EvidenceSummaryError("--smoke-regression-pct must be non-negative")
    if args.min_benchmark_reports < 0:
        raise EvidenceSummaryError("--min-benchmark-reports must be non-negative")
    if args.min_smoke_summaries < 0:
        raise EvidenceSummaryError("--min-smoke-summaries must be non-negative")

    benchmark_paths = retained_evidence_paths(
        args.benchmark_report,
        args.benchmark_dir,
        BENCHMARK_REPORT_NAMES,
        "benchmark",
    )
    smoke_paths = retained_evidence_paths(
        args.smoke_summary,
        args.smoke_dir,
        SMOKE_SUMMARY_NAMES,
        "smoke summary",
    )
    if benchmark_paths and len(benchmark_paths) < args.min_benchmark_reports:
        raise EvidenceSummaryError(
            "threshold calibration requires at least "
            f"{args.min_benchmark_reports} benchmark report(s), found "
            f"{len(benchmark_paths)}"
        )
    if smoke_paths and len(smoke_paths) < args.min_smoke_summaries:
        raise EvidenceSummaryError(
            "threshold calibration requires at least "
            f"{args.min_smoke_summaries} smoke summary file(s), found "
            f"{len(smoke_paths)}"
        )

    benchmarks = [load_json(path) for path in benchmark_paths]
    smoke_summaries = [load_json(path) for path in smoke_paths]

    payload: dict[str, Any] = {
        "schema_version": 1,
        "suite": "nfs-threshold-suggestions",
        "evidence": {
            "benchmark_reports": [str(path) for path in benchmark_paths],
            "smoke_summaries": [str(path) for path in smoke_paths],
        },
        "margins": {
            "benchmark_margin_pct": benchmark_margin_pct,
            "smoke_margin_pct": smoke_margin_pct,
            "benchmark_regression_pct": benchmark_regression_pct,
            "smoke_regression_pct": smoke_regression_pct,
        },
        "minimums": {
            "min_benchmark_reports": args.min_benchmark_reports,
            "min_smoke_summaries": args.min_smoke_summaries,
        },
        "evidence_tier": threshold_evidence_tier(
            benchmark_paths=benchmark_paths,
            benchmarks=benchmarks,
            smoke_paths=smoke_paths,
            smoke_summaries=smoke_summaries,
            min_benchmark_reports=args.min_benchmark_reports,
            min_smoke_summaries=args.min_smoke_summaries,
        ),
        "suggestions": {},
    }
    suggestions = payload["suggestions"]

    if benchmark_paths:
        suggestions["NFS_READ_PATH_BENCH_VERIFY_ARGS"] = suggest_benchmark_verify_args(
            benchmarks,
            margin_pct=benchmark_margin_pct,
        )
        suggestions["NFS_READ_PATH_BENCH_COMPARE_ARGS"] = (
            f"--max-throughput-regression-pct {fmt_arg(benchmark_regression_pct)} "
            f"--max-ratio-regression-pct {fmt_arg(benchmark_regression_pct)}"
        )

    if smoke_paths:
        suggestions["NFS_SMOKE_VERIFY_ARGS"] = suggest_smoke_verify_args(
            smoke_summaries,
            margin_pct=smoke_margin_pct,
        )
        suggestions["NFS_SMOKE_COMPARE_ARGS"] = " ".join(
            (
                f"--max-native-read-throughput-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-requested-amplification-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-returned-amplification-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-rpc-density-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-vfs-call-density-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-lease-hit-density-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-lease-miss-density-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-resolver-avoidance-regression-pct {fmt_arg(smoke_regression_pct)}",
                f"--max-native-read-hydration-remote-byte-regression-pct {fmt_arg(smoke_regression_pct)}",
            )
        )

    if not suggestions:
        raise EvidenceSummaryError("provide --benchmark-report, --smoke-summary, or both")
    return payload


def render_threshold_suggestions(payload: dict[str, Any]) -> str:
    suggestions = payload.get("suggestions")
    if not isinstance(suggestions, dict):
        raise EvidenceSummaryError("threshold suggestions payload must contain suggestions")
    lines = [
        "# Suggested NFS threshold args",
        "",
        "# Review these against multiple retained runs before promoting them to repository variables.",
    ]
    evidence = payload.get("evidence")
    if isinstance(evidence, dict):
        benchmark_count = len(evidence.get("benchmark_reports", []))
        smoke_count = len(evidence.get("smoke_summaries", []))
        lines.extend(
            (
                f"# Benchmark reports: {benchmark_count}",
                f"# Smoke summaries: {smoke_count}",
            )
        )
    minimums = payload.get("minimums")
    if isinstance(minimums, dict):
        lines.extend(
            (
                f"# Minimum benchmark reports: {minimums.get('min_benchmark_reports', 'n/a')}",
                f"# Minimum smoke summaries: {minimums.get('min_smoke_summaries', 'n/a')}",
            )
        )
    evidence_tier = payload.get("evidence_tier")
    if isinstance(evidence_tier, dict):
        lines.extend(
            (
                f"# Evidence tier: {evidence_tier.get('tier', 'unknown')}",
                f"# Release grade: {str(evidence_tier.get('release_grade', False)).lower()}",
                f"# Calibration ready: {str(evidence_tier.get('calibration_ready', False)).lower()}",
            )
        )
        benchmark_run_attempt_count = evidence_tier.get("benchmark_run_attempt_count")
        if isinstance(benchmark_run_attempt_count, int) and not isinstance(
            benchmark_run_attempt_count,
            bool,
        ):
            lines.append(f"# Benchmark run attempts: {benchmark_run_attempt_count}")
        run_attempt_count = evidence_tier.get("smoke_run_attempt_count")
        if isinstance(run_attempt_count, int) and not isinstance(
            run_attempt_count,
            bool,
        ):
            lines.append(f"# Smoke run attempts: {run_attempt_count}")
        benchmark_run_suffixes = evidence_tier.get("benchmark_run_id_suffixes")
        if isinstance(benchmark_run_suffixes, list) and benchmark_run_suffixes:
            lines.append(
                "# Benchmark run suffixes: "
                + ", ".join(str(suffix) for suffix in benchmark_run_suffixes)
            )
        suites = evidence_tier.get("suites")
        if isinstance(suites, list) and suites:
            lines.append(
                "# Native smoke suites: " + ", ".join(str(suite) for suite in suites)
            )
        run_id_suffixes = evidence_tier.get("run_id_suffixes")
        if isinstance(run_id_suffixes, list) and run_id_suffixes:
            lines.append(
                "# Native smoke run suffixes: "
                + ", ".join(str(suffix) for suffix in run_id_suffixes)
            )
        git_commit = evidence_tier.get("git_commit")
        if isinstance(git_commit, str) and git_commit:
            lines.append(f"# Native smoke git commit: {git_commit}")
        benchmark_git_commit = evidence_tier.get("benchmark_git_commit")
        if isinstance(benchmark_git_commit, str) and benchmark_git_commit:
            lines.append(f"# Benchmark git commit: {benchmark_git_commit}")
        release_blockers = evidence_tier.get("release_blockers")
        if isinstance(release_blockers, list) and release_blockers:
            lines.append("# Release blockers:")
            for blocker in release_blockers:
                lines.append(f"# - {blocker}")
        calibration_blockers = evidence_tier.get("calibration_blockers")
        if isinstance(calibration_blockers, list) and calibration_blockers:
            lines.append("# Calibration blockers:")
            for blocker in calibration_blockers:
                lines.append(f"# - {blocker}")
    for key in (
        "NFS_READ_PATH_BENCH_VERIFY_ARGS",
        "NFS_READ_PATH_BENCH_COMPARE_ARGS",
        "NFS_SMOKE_VERIFY_ARGS",
        "NFS_SMOKE_COMPARE_ARGS",
    ):
        value = suggestions.get(key)
        if isinstance(value, str) and value:
            lines.append(f'{key}="{value}"')
    lines.append("")
    return "\n".join(lines)


def write_markdown(path: Path | None, markdown: str, *, append: bool) -> None:
    if path is None:
        print(markdown, end="" if markdown.endswith("\n") else "\n")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    mode = "a" if append else "w"
    with path.open(mode, encoding="utf-8") as output:
        output.write(markdown)
        if not markdown.endswith("\n"):
            output.write("\n")


def benchmark_command(args: argparse.Namespace) -> int:
    markdown = render_benchmark_markdown(
        args.report,
        args.comparison,
        allow_missing=args.allow_missing,
    )
    write_markdown(args.output, markdown, append=args.append)
    return 0


def smoke_command(args: argparse.Namespace) -> int:
    markdown = render_smoke_markdown(
        args.summary,
        args.comparison,
        allow_missing=args.allow_missing,
    )
    write_markdown(args.output, markdown, append=args.append)
    return 0


def thresholds_command(args: argparse.Namespace) -> int:
    payload = suggested_thresholds_payload(args)
    evidence_tier = payload.get("evidence_tier")
    if not isinstance(evidence_tier, dict):
        raise EvidenceSummaryError("threshold suggestions omitted evidence tier")
    if getattr(args, "require_release_grade", False):
        if evidence_tier.get("release_grade") is not True:
            blockers = evidence_tier.get("release_blockers")
            if not isinstance(blockers, list) or not blockers:
                blockers = ["evidence is not release-grade"]
            raise EvidenceSummaryError(
                "threshold suggestions require release-grade evidence: "
                + "; ".join(str(blocker) for blocker in blockers)
            )
    if getattr(args, "require_calibration_ready", False):
        if evidence_tier.get("calibration_ready") is not True:
            blockers = evidence_tier.get("calibration_blockers")
            if not isinstance(blockers, list) or not blockers:
                blockers = ["evidence is not calibration-ready"]
            raise EvidenceSummaryError(
                "threshold suggestions require calibration-ready evidence: "
                + "; ".join(str(blocker) for blocker in blockers)
            )
    text = render_threshold_suggestions(payload)
    if args.json_output is not None:
        args.json_output.parent.mkdir(parents=True, exist_ok=True)
        args.json_output.write_text(
            json.dumps(payload, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    write_markdown(args.output, text, append=args.append)
    return 0


def self_test(_: argparse.Namespace) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        bench_report = root / "nfs-read-path-bench-report.json"
        bench_dir = root / "bench-artifact"
        bench_report_two = bench_dir / "nfs-read-path-bench-report.json"
        bench_comparison = root / "nfs-read-path-bench-comparison.json"
        smoke_summary = root / "nfs-smoke-retained-summary.json"
        smoke_dir = root / "smoke-artifact"
        smoke_summary_two = smoke_dir / "nfs-smoke-retained-summary.json"
        smoke_comparison = root / "nfs-smoke-comparison-summary.json"
        output = root / "summary.md"
        bench_dir.mkdir()
        smoke_dir.mkdir()

        records = [
            {
                "scenario": "pointer_sequential_path_read",
                "mib_per_sec": 100.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
            {
                "scenario": "pointer_sequential_lease_read",
                "mib_per_sec": 150.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
            {
                "scenario": "pointer_random_path_read",
                "mib_per_sec": 110.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
            {
                "scenario": "pointer_random_lease_read",
                "mib_per_sec": 121.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
            {
                "scenario": "overlay_modified_path_reread",
                "mib_per_sec": 120.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
            {
                "scenario": "overlay_modified_lease_reread",
                "mib_per_sec": 144.0,
                "reads": 8,
                "bytes_returned": 8 * 1024 * 1024,
            },
        ]
        for index, record in enumerate(records, start=1):
            record.update(
                {
                    "file_size": 8 * 1024 * 1024,
                    "chunk_size": 64 * 1024,
                    "read_size": 1024 * 1024,
                    "elapsed_ms": index,
                }
            )
        records_two = json.loads(json.dumps(records))
        for record in records_two:
            if record["scenario"] == "pointer_sequential_lease_read":
                record["mib_per_sec"] = 125.0
            elif record["scenario"] == "pointer_random_lease_read":
                record["mib_per_sec"] = 115.5
            elif record["scenario"] == "overlay_modified_lease_reread":
                record["mib_per_sec"] = 138.0

        def complete_smoke_summary(suffix: str, commit: str) -> dict[str, Any]:
            reports = [
                {
                    "suite": suite,
                    "platform": RELEASE_SMOKE_SUITE_PLATFORMS[suite],
                    "run_id": f"{suite}-{suffix}",
                    "git_commit": commit,
                }
                for suite in RELEASE_SMOKE_SUITES
            ]
            return {
                "schema_version": 1,
                "status": "ok",
                "git_commit": commit,
                "run_id_suffix": suffix,
                "report_count": len(reports),
                "suites": list(RELEASE_SMOKE_SUITES),
                "platforms": list(RELEASE_SMOKE_PLATFORMS),
                "reports": reports,
            }

        bench_report.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-run-1",
                    "run_id_suffix": "run-1",
                    "git": {"commit": "0" * 40, "dirty": False},
                    "summary": {
                        "scenario_count": len(records),
                        "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                        "total_bytes_returned": sum(
                            int(record["bytes_returned"]) for record in records
                        ),
                        "total_elapsed_ms": sum(
                            int(record["elapsed_ms"]) for record in records
                        ),
                        "lease_vs_path_mib_per_sec_ratio": {
                            "pointer_sequential": 1.5,
                            "pointer_random": 1.1,
                            "overlay_modified": 1.2,
                        }
                    },
                    "records": records,
                }
            ),
            encoding="utf-8",
        )
        bench_report_two.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-run-2",
                    "run_id_suffix": "run-2",
                    "git": {"commit": "1" * 40, "dirty": False},
                    "summary": {
                        "scenario_count": len(records_two),
                        "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                        "total_bytes_returned": sum(
                            int(record["bytes_returned"]) for record in records_two
                        ),
                        "total_elapsed_ms": sum(
                            int(record["elapsed_ms"]) for record in records_two
                        ),
                        "lease_vs_path_mib_per_sec_ratio": {
                            "pointer_sequential": 1.25,
                            "pointer_random": 1.05,
                            "overlay_modified": 1.15,
                        }
                    },
                    "records": records_two,
                }
            ),
            encoding="utf-8",
        )
        bench_comparison.write_text(
            json.dumps(
                {
                    "scenario_trends": [
                        {
                            "scenario": "pointer_sequential_lease_read",
                            "baseline_mib_per_sec": 140.0,
                            "current_mib_per_sec": 150.0,
                            "change_pct": 7.14,
                        }
                    ],
                    "lease_vs_path_ratio_trends": {
                        "pointer_sequential": {
                            "baseline": 1.4,
                            "current": 1.5,
                            "change_pct": 7.14,
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        smoke_summary.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "0123456789abcdef0123456789abcdef01234567",
                    "run_id_suffix": "run-1",
                    "report_count": 1,
                    "suites": ["mount-nfs-linux"],
                    "platforms": ["linux"],
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-run-1",
                            "git_commit": "0123456789abcdef0123456789abcdef01234567",
                            "mount_doctor": {
                                "ready": True,
                                "nfs_preflight_ready": True,
                                "nfs_preflight_warnings": 0,
                            },
                            "native_read": {
                                "metrics": {
                                    "mib_per_sec": {"value": 64.0},
                                    "requested_bytes_per_user_byte": {"value": 1.25},
                                    "returned_bytes_per_user_byte": {"value": 1.0},
                                    "read_rpcs_per_mib": {"value": 32.0},
                                    "read_lease_hits_per_mib": {"value": 31.0},
                                    "read_lease_misses_per_mib": {"value": 1.0},
                                    "vfs_read_calls_per_mib": {"value": 32.0},
                                    "hydration_remote_bytes_per_user_byte": {"value": 1.0},
                                }
                            },
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        smoke_summary_two.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "fedcba9876543210fedcba9876543210fedcba98",
                    "run_id_suffix": "run-2",
                    "report_count": 1,
                    "suites": ["mount-nfs-linux"],
                    "platforms": ["linux"],
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-run-2",
                            "git_commit": "fedcba9876543210fedcba9876543210fedcba98",
                            "mount_doctor": {
                                "ready": True,
                                "nfs_preflight_ready": True,
                                "nfs_preflight_warnings": 1,
                            },
                            "native_read": {
                                "metrics": {
                                    "mib_per_sec": {"value": 48.0},
                                    "requested_bytes_per_user_byte": {"value": 1.6},
                                    "returned_bytes_per_user_byte": {"value": 1.1},
                                    "read_rpcs_per_mib": {"value": 44.0},
                                    "read_lease_hits_per_mib": {"value": 28.0},
                                    "read_lease_misses_per_mib": {"value": 1.2},
                                    "vfs_read_calls_per_mib": {"value": 40.0},
                                    "hydration_remote_bytes_per_user_byte": {"value": 1.1},
                                }
                            },
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        smoke_comparison.write_text(
            json.dumps(
                {
                    "comparisons": [
                        {
                            "smoke_suite": "mount-nfs-linux",
                            "platform": "linux",
                            "native_read": {
                                "metrics": {
                                    "mib_per_sec": {"regression_pct": 0.0},
                                    "read_rpcs_per_mib": {"regression_pct": 3.0},
                                    "read_lease_hits_per_mib": {
                                        "regression_pct": 2.5
                                    },
                                    "read_lease_misses_per_mib": {
                                        "regression_pct": 1.5
                                    },
                                    "vfs_read_calls_per_mib": {"regression_pct": 2.0},
                                    "hydration_remote_bytes_per_user_byte": {
                                        "regression_pct": 1.0
                                    },
                                }
                            },
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )

        write_markdown(
            output,
            render_benchmark_markdown(
                bench_report,
                bench_comparison,
                allow_missing=False,
            ),
            append=False,
        )
        write_markdown(
            output,
            render_smoke_markdown(
                smoke_summary,
                smoke_comparison,
                allow_missing=False,
            ),
            append=True,
        )
        markdown = output.read_text(encoding="utf-8")
        for needle in (
            "NFS Read-Path Benchmark Evidence",
            "Benchmark Trend",
            "Retained Native NFS Smoke Evidence",
            "Native Smoke Trend",
            "Commit",
            "0123456789ab",
            "Doctor",
            "ready",
            "Lease misses/MiB",
            "Lease-miss regression",
            "Hydration remote/user byte",
        ):
            if needle not in markdown:
                print(f"error: self-test summary omitted {needle!r}", file=sys.stderr)
                return 1

        missing = root / "missing.json"
        if "No benchmark report found" not in render_benchmark_markdown(
            missing,
            None,
            allow_missing=True,
        ):
            print("error: self-test missing benchmark report was not summarized", file=sys.stderr)
            return 1
        try:
            render_smoke_markdown(missing, None, allow_missing=False)
        except EvidenceSummaryError:
            pass
        else:
            print("error: self-test missing smoke summary was not rejected", file=sys.stderr)
            return 1

        threshold_output = root / "thresholds.env"
        threshold_json = root / "thresholds.json"
        result = thresholds_command(
            argparse.Namespace(
                benchmark_report=[bench_report],
                benchmark_dir=[bench_dir],
                smoke_summary=[smoke_summary],
                smoke_dir=[smoke_dir],
                benchmark_margin_pct=20.0,
                smoke_margin_pct=25.0,
                benchmark_regression_pct=20.0,
                smoke_regression_pct=20.0,
                min_benchmark_reports=2,
                min_smoke_summaries=2,
                output=threshold_output,
                json_output=threshold_json,
                append=False,
            )
        )
        if result != 0:
            print("error: self-test threshold suggestions failed", file=sys.stderr)
            return 1
        threshold_text = threshold_output.read_text(encoding="utf-8")
        threshold_payload = load_json(threshold_json)
        for needle in (
            "NFS_READ_PATH_BENCH_VERIFY_ARGS",
            "--min-pointer-sequential-lease-ratio 1",
            "--min-pointer-random-lease-ratio 0.84",
            "# Benchmark reports: 2",
            "# Minimum benchmark reports: 2",
            "# Benchmark run attempts: 2",
            "NFS_SMOKE_VERIFY_ARGS",
            "--min-native-read-mib-per-sec 36",
            "--max-native-read-rpcs-per-mib 55",
            "--min-native-read-lease-hits-per-mib 21",
            "--max-native-read-lease-misses-per-mib 1.5",
            "# Smoke summaries: 2",
            "# Smoke run attempts: 2",
            "# Minimum smoke summaries: 2",
            "# Evidence tier: advisory",
            "# Release grade: false",
            "# Calibration ready: false",
            "# Native smoke run suffixes: run-1, run-2",
            "# Release blockers:",
            "# - missing native smoke platform(s): macos, windows",
            "NFS_SMOKE_COMPARE_ARGS",
            "--max-native-read-lease-hit-density-regression-pct 20",
            "--max-native-read-lease-miss-density-regression-pct 20",
        ):
            if needle not in threshold_text:
                print(
                    f"error: self-test threshold suggestions omitted {needle!r}",
                    file=sys.stderr,
                )
                return 1
        if threshold_payload.get("suite") != "nfs-threshold-suggestions":
            print("error: self-test threshold JSON used the wrong suite", file=sys.stderr)
            return 1
        if threshold_payload.get("minimums") != {
            "min_benchmark_reports": 2,
            "min_smoke_summaries": 2,
        }:
            print("error: self-test threshold JSON omitted minimums", file=sys.stderr)
            return 1
        evidence_tier = threshold_payload.get("evidence_tier")
        if not isinstance(evidence_tier, dict):
            print("error: self-test threshold JSON omitted evidence tier", file=sys.stderr)
            return 1
        if evidence_tier.get("tier") != "advisory":
            print("error: self-test threshold tier ignored missing platforms", file=sys.stderr)
            return 1
        if evidence_tier.get("release_grade") is not False:
            print("error: self-test threshold release grade was not false", file=sys.stderr)
            return 1
        if evidence_tier.get("missing_platforms") != ["macos", "windows"]:
            print("error: self-test threshold missing platforms were wrong", file=sys.stderr)
            return 1

        complete_benchmarks = [
            {
                "schema_version": 1,
                "suite": BENCHMARK_SUITE,
                "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                "run_id_suffix": "attempt-1",
                "git": {"commit": "a" * 40, "dirty": False},
                "summary": {
                    "scenario_count": len(records),
                    "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                    "total_bytes_returned": sum(
                        int(record["bytes_returned"]) for record in records
                    ),
                    "total_elapsed_ms": sum(
                        int(record["elapsed_ms"]) for record in records
                    ),
                    "lease_vs_path_mib_per_sec_ratio": {
                        "pointer_sequential": 1.5,
                        "pointer_random": 1.1,
                        "overlay_modified": 1.2,
                    },
                },
                "records": records,
            },
            {
                "schema_version": 1,
                "suite": BENCHMARK_SUITE,
                "run_id": f"{BENCHMARK_SUITE}-attempt-2",
                "run_id_suffix": "attempt-2",
                "git": {"commit": "a" * 40, "dirty": False},
                "summary": {
                    "scenario_count": len(records_two),
                    "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                    "total_bytes_returned": sum(
                        int(record["bytes_returned"]) for record in records_two
                    ),
                    "total_elapsed_ms": sum(
                        int(record["elapsed_ms"]) for record in records_two
                    ),
                    "lease_vs_path_mib_per_sec_ratio": {
                        "pointer_sequential": 1.25,
                        "pointer_random": 1.05,
                        "overlay_modified": 1.15,
                    },
                },
                "records": records_two,
            },
        ]

        complete_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report, bench_report_two],
            benchmarks=complete_benchmarks,
            smoke_paths=[smoke_summary, smoke_summary_two],
            smoke_summaries=[
                complete_smoke_summary("attempt-1", "a" * 40),
                complete_smoke_summary("attempt-2", "a" * 40),
            ],
            min_benchmark_reports=2,
            min_smoke_summaries=2,
        )
        if complete_tier.get("tier") != "calibration":
            print("error: self-test complete evidence was not calibration-ready", file=sys.stderr)
            return 1
        if complete_tier.get("release_grade") is not True:
            print("error: self-test complete evidence was not release-shaped", file=sys.stderr)
            return 1
        if complete_tier.get("run_id_suffix") is not None:
            print("error: self-test multi-run tier collapsed run suffixes", file=sys.stderr)
            return 1
        if complete_tier.get("run_id_suffixes") != ["attempt-1", "attempt-2"]:
            print("error: self-test complete evidence omitted run suffixes", file=sys.stderr)
            return 1
        if complete_tier.get("benchmark_run_id_suffix") is not None:
            print("error: self-test multi-run tier collapsed benchmark suffixes", file=sys.stderr)
            return 1
        if complete_tier.get("benchmark_run_id_suffixes") != ["attempt-1", "attempt-2"]:
            print("error: self-test complete evidence omitted benchmark suffixes", file=sys.stderr)
            return 1

        duplicate_benchmark_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report, bench_report_two],
            benchmarks=[complete_benchmarks[0], complete_benchmarks[0]],
            smoke_paths=[smoke_summary, smoke_summary_two],
            smoke_summaries=[
                complete_smoke_summary("attempt-1", "a" * 40),
                complete_smoke_summary("attempt-2", "a" * 40),
            ],
            min_benchmark_reports=2,
            min_smoke_summaries=2,
        )
        if duplicate_benchmark_tier.get("release_grade") is not True:
            print(
                "error: self-test duplicate benchmark attempt was not release-shaped",
                file=sys.stderr,
            )
            return 1
        if duplicate_benchmark_tier.get("calibration_ready") is not False:
            print(
                "error: self-test duplicate benchmark attempt was calibration-ready",
                file=sys.stderr,
            )
            return 1
        duplicate_benchmark_blockers = duplicate_benchmark_tier.get(
            "calibration_blockers"
        )
        if not isinstance(duplicate_benchmark_blockers, list) or (
            "found 1 benchmark run attempt(s), need 2"
            not in duplicate_benchmark_blockers
        ):
            print(
                "error: self-test duplicate benchmark attempt omitted calibration blocker",
                file=sys.stderr,
            )
            return 1

        duplicate_attempt_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report, bench_report_two],
            benchmarks=complete_benchmarks,
            smoke_paths=[smoke_summary, smoke_summary_two],
            smoke_summaries=[
                complete_smoke_summary("attempt-1", "a" * 40),
                complete_smoke_summary("attempt-1", "a" * 40),
            ],
            min_benchmark_reports=2,
            min_smoke_summaries=2,
        )
        if duplicate_attempt_tier.get("release_grade") is not True:
            print(
                "error: self-test duplicate smoke attempt was not release-shaped",
                file=sys.stderr,
            )
            return 1
        if duplicate_attempt_tier.get("calibration_ready") is not False:
            print(
                "error: self-test duplicate smoke attempt was calibration-ready",
                file=sys.stderr,
            )
            return 1
        duplicate_attempt_blockers = duplicate_attempt_tier.get("calibration_blockers")
        if not isinstance(duplicate_attempt_blockers, list) or (
            "found 1 smoke run attempt(s), need 2" not in duplicate_attempt_blockers
        ):
            print(
                "error: self-test duplicate smoke attempt omitted calibration blocker",
                file=sys.stderr,
            )
            return 1

        split_smoke_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report, bench_report_two],
            benchmarks=complete_benchmarks,
            smoke_paths=[smoke_summary, smoke_summary_two],
            smoke_summaries=[
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "a" * 40,
                    "run_id_suffix": "attempt-1",
                    "report_count": 2,
                    "suites": ["mount-nfs-linux", "mount-nfs-macos"],
                    "platforms": ["linux", "macos"],
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-attempt-1",
                            "git_commit": "a" * 40,
                        },
                        {
                            "suite": "mount-nfs-macos",
                            "platform": "macos",
                            "run_id": "mount-nfs-macos-attempt-1",
                            "git_commit": "a" * 40,
                        },
                    ],
                },
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "a" * 40,
                    "run_id_suffix": "attempt-2",
                    "report_count": 1,
                    "suites": ["mount-nfs-windows"],
                    "platforms": ["windows"],
                    "reports": [
                        {
                            "suite": "mount-nfs-windows",
                            "platform": "windows",
                            "run_id": "mount-nfs-windows-attempt-2",
                            "git_commit": "a" * 40,
                        }
                    ],
                },
            ],
            min_benchmark_reports=2,
            min_smoke_summaries=2,
        )
        if split_smoke_tier.get("release_grade") is not False:
            print("error: self-test split smoke evidence was release-shaped", file=sys.stderr)
            return 1
        if split_smoke_tier.get("calibration_ready") is not False:
            print("error: self-test split smoke evidence was calibration-ready", file=sys.stderr)
            return 1
        split_smoke_blockers = split_smoke_tier.get("release_blockers")
        if not isinstance(split_smoke_blockers, list):
            print("error: self-test split smoke evidence omitted blockers", file=sys.stderr)
            return 1
        for blocker in (
            "smoke summary 1 missing release smoke suite(s): mount-nfs-windows",
            "smoke summary 2 missing release smoke suite(s): mount-nfs-linux, mount-nfs-macos",
            "smoke summary 2 found 1 smoke report(s), need 3",
        ):
            if blocker not in split_smoke_blockers:
                print(
                    f"error: self-test split smoke evidence omitted {blocker!r}",
                    file=sys.stderr,
                )
                return 1

        mismatch_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report],
            benchmarks=[
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                    "run_id_suffix": "attempt-1",
                    "git": {"commit": "c" * 40, "dirty": False},
                }
            ],
            smoke_paths=[smoke_summary],
            smoke_summaries=[
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "d" * 40,
                    "run_id_suffix": "attempt-1",
                    "report_count": 3,
                    "suites": list(RELEASE_SMOKE_SUITES),
                    "platforms": list(RELEASE_SMOKE_PLATFORMS),
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-attempt-1",
                            "git_commit": "d" * 40,
                        },
                        {
                            "suite": "mount-nfs-macos",
                            "platform": "macos",
                            "run_id": "mount-nfs-macos-attempt-1",
                            "git_commit": "d" * 40,
                        },
                        {
                            "suite": "mount-nfs-windows",
                            "platform": "windows",
                            "run_id": "mount-nfs-windows-attempt-1",
                            "git_commit": "d" * 40,
                        },
                    ],
                }
            ],
            min_benchmark_reports=1,
            min_smoke_summaries=1,
        )
        mismatch_blockers = mismatch_tier.get("release_blockers")
        if not isinstance(mismatch_blockers, list) or (
            "benchmark git.commit cccccccccccc does not match "
            "native smoke git_commit dddddddddddd"
        ) not in mismatch_blockers:
            print(
                "error: self-test benchmark/native commit mismatch was not rejected",
                file=sys.stderr,
            )
            return 1

        inconsistent_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report],
            benchmarks=[
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                    "run_id_suffix": "attempt-1",
                    "git": {"commit": "b" * 40, "dirty": False},
                }
            ],
            smoke_paths=[smoke_summary],
            smoke_summaries=[
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "b" * 40,
                    "run_id_suffix": "attempt-1",
                    "report_count": 3,
                    "suites": list(RELEASE_SMOKE_SUITES),
                    "platforms": list(RELEASE_SMOKE_PLATFORMS),
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-attempt-1",
                            "git_commit": "b" * 40,
                        }
                    ],
                }
            ],
            min_benchmark_reports=1,
            min_smoke_summaries=1,
        )
        inconsistent_blockers = inconsistent_tier.get("release_blockers")
        if not isinstance(inconsistent_blockers, list):
            print("error: self-test inconsistent evidence omitted blockers", file=sys.stderr)
            return 1
        for blocker in (
            "smoke summary 1 report_count 3 does not match 1 report row(s)",
            "smoke summary 1 suites must match report rows",
            "smoke summary 1 platforms must match report rows",
            "missing native smoke suite(s): mount-nfs-macos, mount-nfs-windows",
            "missing native smoke platform(s): macos, windows",
        ):
            if blocker not in inconsistent_blockers:
                print(
                    f"error: self-test inconsistent evidence omitted {blocker!r}",
                    file=sys.stderr,
                )
                return 1

        forged_smoke_header_tier = threshold_evidence_tier(
            benchmark_paths=[],
            benchmarks=[],
            smoke_paths=[smoke_summary],
            smoke_summaries=[
                {
                    "schema_version": 1,
                    "status": "ok",
                    "git_commit": "b" * 40,
                    "run_id_suffix": "attempt-forged",
                    "report_count": 1,
                    "suites": ["mount-nfs-linux"],
                    "platforms": ["linux"],
                    "reports": [
                        {
                            "suite": "mount-nfs-linux",
                            "platform": "linux",
                            "run_id": "mount-nfs-linux-attempt-real",
                            "git_commit": "c" * 40,
                        }
                    ],
                }
            ],
            min_benchmark_reports=0,
            min_smoke_summaries=1,
        )
        forged_smoke_header_blockers = forged_smoke_header_tier.get(
            "release_blockers"
        )
        if not isinstance(forged_smoke_header_blockers, list):
            print("error: self-test forged smoke header omitted blockers", file=sys.stderr)
            return 1
        for blocker in (
            "smoke summary 1 git_commit must match report rows",
            "smoke summary 1 run_id_suffix must match report rows",
        ):
            if blocker not in forged_smoke_header_blockers:
                print(
                    f"error: self-test forged smoke header omitted {blocker!r}",
                    file=sys.stderr,
                )
                return 1

        dirty_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report],
            benchmarks=[
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                    "run_id_suffix": "attempt-1",
                    "git": {"commit": "e" * 40, "dirty": True},
                }
            ],
            smoke_paths=[],
            smoke_summaries=[],
            min_benchmark_reports=1,
            min_smoke_summaries=0,
        )
        dirty_blockers = dirty_tier.get("calibration_blockers")
        if not isinstance(dirty_blockers, list) or (
            "benchmark report 1 git.dirty must be false for promotable evidence"
            not in dirty_blockers
        ):
            print("error: self-test dirty benchmark evidence was promotable", file=sys.stderr)
            return 1

        malformed_benchmark_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report],
            benchmarks=[
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                    "run_id_suffix": "attempt-1",
                    "git": {"commit": "f" * 40, "dirty": False},
                    "summary": {
                        "scenario_count": len(records) - 1,
                        "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                        "lease_vs_path_mib_per_sec_ratio": {
                            "pointer_sequential": 1.5,
                            "pointer_random": 1.1,
                            "overlay_modified": 1.2,
                        },
                    },
                    "records": records[:-1],
                }
            ],
            smoke_paths=[],
            smoke_summaries=[],
            min_benchmark_reports=1,
            min_smoke_summaries=0,
        )
        malformed_blockers = malformed_benchmark_tier.get("calibration_blockers")
        if not isinstance(malformed_blockers, list) or (
            "benchmark report 1 missing scenario(s): overlay_modified_lease_reread"
            not in malformed_blockers
        ):
            print(
                "error: self-test malformed benchmark evidence was promotable",
                file=sys.stderr,
            )
            return 1

        forged_ratio_tier = threshold_evidence_tier(
            benchmark_paths=[bench_report],
            benchmarks=[
                {
                    "schema_version": 1,
                    "suite": BENCHMARK_SUITE,
                    "run_id": f"{BENCHMARK_SUITE}-attempt-1",
                    "run_id_suffix": "attempt-1",
                    "git": {"commit": "f" * 40, "dirty": False},
                    "summary": {
                        "scenario_count": len(records),
                        "scenarios": list(BENCHMARK_EXPECTED_SCENARIOS),
                        "total_bytes_returned": sum(
                            int(record["bytes_returned"]) for record in records
                        ),
                        "total_elapsed_ms": sum(
                            int(record["elapsed_ms"]) for record in records
                        ),
                        "lease_vs_path_mib_per_sec_ratio": {
                            "pointer_sequential": 999.0,
                            "pointer_random": 1.1,
                            "overlay_modified": 1.2,
                        },
                    },
                    "records": records,
                }
            ],
            smoke_paths=[],
            smoke_summaries=[],
            min_benchmark_reports=1,
            min_smoke_summaries=0,
        )
        forged_ratio_blockers = forged_ratio_tier.get("calibration_blockers")
        if not isinstance(forged_ratio_blockers, list) or (
            "benchmark report 1 ratio pointer_sequential must match records"
            not in forged_ratio_blockers
        ):
            print("error: self-test forged benchmark ratio was promotable", file=sys.stderr)
            return 1

        try:
            thresholds_command(
                argparse.Namespace(
                    benchmark_report=[bench_report],
                    benchmark_dir=[],
                    smoke_summary=[smoke_summary],
                    smoke_dir=[],
                    benchmark_margin_pct=20.0,
                    smoke_margin_pct=25.0,
                    benchmark_regression_pct=20.0,
                    smoke_regression_pct=20.0,
                    min_benchmark_reports=1,
                    min_smoke_summaries=1,
                    require_release_grade=True,
                    require_calibration_ready=False,
                    output=None,
                    json_output=None,
                    append=False,
                )
            )
        except EvidenceSummaryError as error:
            if "missing native smoke platform(s): macos, windows" not in str(error):
                raise
        else:
            print(
                "error: self-test release-grade threshold requirement was not enforced",
                file=sys.stderr,
            )
            return 1

        try:
            thresholds_command(
                argparse.Namespace(
                    benchmark_report=[bench_report, bench_report_two],
                    benchmark_dir=[],
                    smoke_summary=[smoke_summary, smoke_summary_two],
                    smoke_dir=[],
                    benchmark_margin_pct=20.0,
                    smoke_margin_pct=25.0,
                    benchmark_regression_pct=20.0,
                    smoke_regression_pct=20.0,
                    min_benchmark_reports=2,
                    min_smoke_summaries=2,
                    require_release_grade=False,
                    require_calibration_ready=True,
                    output=None,
                    json_output=None,
                    append=False,
                )
            )
        except EvidenceSummaryError as error:
            if "missing native smoke platform(s): macos, windows" not in str(error):
                raise
        else:
            print(
                "error: self-test calibration threshold requirement was not enforced",
                file=sys.stderr,
            )
            return 1

        try:
            thresholds_command(
                argparse.Namespace(
                    benchmark_report=[bench_report],
                    benchmark_dir=[],
                    smoke_summary=None,
                    smoke_dir=None,
                    benchmark_margin_pct=20.0,
                    smoke_margin_pct=25.0,
                    benchmark_regression_pct=20.0,
                    smoke_regression_pct=20.0,
                    min_benchmark_reports=2,
                    min_smoke_summaries=1,
                    output=None,
                    json_output=None,
                    append=False,
                )
            )
        except EvidenceSummaryError as error:
            if "at least 2 benchmark report" not in str(error):
                raise
        else:
            print(
                "error: self-test threshold minimum benchmark count was not enforced",
                file=sys.stderr,
            )
            return 1

    print("ok: NFS evidence summary self-test passed")
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    benchmark = subcommands.add_parser(
        "benchmark",
        help="render a retained NFS read-path benchmark summary",
    )
    benchmark.add_argument("--report", required=True, type=Path)
    benchmark.add_argument("--comparison", type=Path)
    benchmark.add_argument("--output", type=Path)
    benchmark.add_argument("--append", action="store_true")
    benchmark.add_argument("--allow-missing", action="store_true")
    benchmark.set_defaults(func=benchmark_command)

    smoke = subcommands.add_parser(
        "smoke",
        help="render a retained native NFS smoke summary",
    )
    smoke.add_argument("--summary", required=True, type=Path)
    smoke.add_argument("--comparison", type=Path)
    smoke.add_argument("--output", type=Path)
    smoke.add_argument("--append", action="store_true")
    smoke.add_argument("--allow-missing", action="store_true")
    smoke.set_defaults(func=smoke_command)

    thresholds = subcommands.add_parser(
        "thresholds",
        help="suggest threshold args from retained NFS evidence",
    )
    thresholds.add_argument("--benchmark-report", action="append", type=Path)
    thresholds.add_argument("--benchmark-dir", action="append", type=Path)
    thresholds.add_argument("--smoke-summary", action="append", type=Path)
    thresholds.add_argument("--smoke-dir", action="append", type=Path)
    thresholds.add_argument("--benchmark-margin-pct", type=float, default=20.0)
    thresholds.add_argument("--smoke-margin-pct", type=float, default=25.0)
    thresholds.add_argument("--benchmark-regression-pct", type=float, default=20.0)
    thresholds.add_argument("--smoke-regression-pct", type=float, default=20.0)
    thresholds.add_argument("--min-benchmark-reports", type=int, default=1)
    thresholds.add_argument("--min-smoke-summaries", type=int, default=1)
    thresholds.add_argument("--require-release-grade", action="store_true")
    thresholds.add_argument("--require-calibration-ready", action="store_true")
    thresholds.add_argument("--output", type=Path)
    thresholds.add_argument("--json-output", type=Path)
    thresholds.add_argument("--append", action="store_true")
    thresholds.set_defaults(func=thresholds_command)

    self_test_parser = subcommands.add_parser("self-test", help="exercise summary rendering")
    self_test_parser.set_defaults(func=self_test)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        return args.func(args)
    except EvidenceSummaryError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
