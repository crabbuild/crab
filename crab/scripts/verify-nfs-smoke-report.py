#!/usr/bin/env python3
"""Verify retained native NFS mount smoke evidence."""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
from pathlib import Path
from typing import Any


EXPECTED_SUITES = {
    "mount-nfs-linux": "linux",
    "mount-nfs-macos": "macos",
    "mount-nfs-windows": "windows",
}

UNIX_SYMLINK_PLATFORMS = {"linux", "macos"}

REQUIRED_CHECKS = {
    "build",
    "helper_version",
    "mount_doctor",
    "initial_read",
    "native_read_benchmark",
    "writeback",
    "mount_list",
    "mount_status",
    "control_status",
    "unmount",
    "control_shutdown",
    "remount",
}

REQUIRED_ARTIFACTS = (
    "mount_list",
    "mount_doctor",
    "mount_status",
    "control_status",
    "native_read_benchmark",
    "writeback_check",
    "unmount_check",
    "control_shutdown",
    "remount_check",
)

NATIVE_PROTOCOL_COUNTERS = (
    "read_rpcs",
    "read_requested_bytes",
    "read_returned_bytes",
)

NFS_RUNTIME_PROTOCOL_COUNTERS = (
    "read_rpcs",
    "read_requested_bytes",
    "read_returned_bytes",
    "read_size_le_4k",
    "read_size_le_64k",
    "read_size_le_1m",
    "read_size_gt_1m",
    "readdirplus_rpcs",
    "readdirplus_entries",
    "readdirplus_materialized_entries",
    "readdirplus_returned_candidates",
    "readdirplus_attr_resolutions",
    "readdirplus_prefetch_paths",
    "readdirplus_cookie_resumes",
    "readdirplus_cookie_misses",
    "readdirplus_skipped_entries",
    "readdirplus_large_dirs",
    "readdirplus_prefetch_errors",
)

NATIVE_VFS_COUNTERS = (
    "open_read_calls",
    "read_at_calls",
    "returned_bytes",
    "source_cache_hits",
    "resolver_calls_avoided",
    "source_cache_misses",
    "source_cache_evictions",
    "source_cache_invalidations",
    "source_cache_stale_evictions",
    "stale_generation_rejections",
    "stale_overlay_view_rejections",
    "stale_overlay_file_rejections",
    "base_pointer_reads",
    "base_pointer_bytes",
    "base_blob_reads",
    "base_blob_bytes",
    "base_empty_reads",
    "base_empty_bytes",
    "overlay_file_reads",
    "overlay_file_bytes",
    "adaptive_first",
    "adaptive_sequential",
    "adaptive_strided",
    "adaptive_repeated",
    "adaptive_random",
)

NATIVE_HYDRATION_COUNTERS = (
    "read_range_requests",
    "read_range_requested_bytes",
    "read_range_returned_bytes",
    "read_window_cache_hits",
    "read_window_cache_misses",
    "read_window_inflight_waits",
    "read_window_remote_fetches",
    "read_window_remote_bytes",
    "read_window_prefetch_requests",
    "read_window_prefetch_scheduled",
    "read_window_prefetch_skipped",
    "read_window_prefetch_errors",
    "chunk_cache_hits",
    "chunk_cache_misses",
    "chunk_inflight_waits",
    "chunk_remote_fetches",
    "chunk_remote_bytes",
)

NATIVE_READ_LEASE_COUNTERS = (
    "temporary_overflows",
    "hits",
    "misses",
    "evictions",
    "stale_retries",
)

NATIVE_READ_TREND_METRICS = (
    ("mib_per_sec", "throughput", True),
    (
        "requested_bytes_per_user_byte",
        "requested-byte amplification",
        False,
    ),
    (
        "returned_bytes_per_user_byte",
        "returned-byte amplification",
        False,
    ),
    ("read_rpcs_per_mib", "RPC density", False),
    ("vfs_read_calls_per_mib", "VFS read-call density", False),
    (
        "vfs_returned_bytes_per_user_byte",
        "VFS returned-byte amplification",
        False,
    ),
    (
        "resolver_calls_avoided_per_mib",
        "resolver avoidance density",
        True,
    ),
    ("read_lease_hits_per_mib", "read-lease hit density", True),
    ("read_lease_misses_per_mib", "read-lease miss density", False),
    (
        "hydration_remote_bytes_per_user_byte",
        "hydration remote-byte amplification",
        False,
    ),
    ("hydration_cache_hits_per_mib", "hydration cache-hit density", True),
    (
        "hydration_prefetch_requests_per_mib",
        "hydration prefetch-request density",
        False,
    ),
)


def check(condition: bool, errors: list[str], message: str) -> None:
    if not condition:
        errors.append(message)


def load_report(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise ValueError(f"invalid JSON: {error}") from error
    if not isinstance(payload, dict):
        raise ValueError("report root must be an object")
    return payload


def resolve_artifact_path(value: str, artifact_base: Path | None) -> Path | None:
    path = Path(value)
    if path.is_file():
        return path

    if artifact_base is None:
        return None

    candidates: list[Path] = []
    if not path.is_absolute():
        candidates.append(artifact_base / path)
    candidates.append(artifact_base / path.name)

    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def check_string_field(
    report: dict[str, Any],
    field: str,
    errors: list[str],
) -> str:
    value = report.get(field)
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{field} must be a non-empty string")
        return ""
    return value


def is_full_git_object_id(value: Any) -> bool:
    return (
        isinstance(value, str)
        and len(value) in (40, 64)
        and all(char in "0123456789abcdef" for char in value)
    )


def check_git_commit_field(report: dict[str, Any], errors: list[str]) -> str:
    value = check_string_field(report, "git_commit", errors)
    if not value:
        return ""
    if not is_full_git_object_id(value):
        errors.append("git_commit must be a lowercase full Git object id")
    return value


def check_nonempty_string(value: Any, errors: list[str], field: str) -> str | None:
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{field} must be a non-empty string")
        return None
    return value


def contains_unredacted_tcp_control_token(value: str) -> bool:
    search_from = 0
    while True:
        tcp_index = value.find("tcp:", search_from)
        if tcp_index < 0:
            return False
        token_index = value.find("?token=", tcp_index)
        if token_index < 0:
            return False
        token_value_index = token_index + len("?token=")
        if not value.startswith("<redacted>", token_value_index):
            return True
        search_from = token_value_index + len("<redacted>")


def check_retained_control_endpoints_redacted(
    value: Any,
    errors: list[str],
    field: str,
) -> None:
    if isinstance(value, str):
        if contains_unredacted_tcp_control_token(value):
            errors.append(f"{field} must redact TCP control token")
        return

    if isinstance(value, dict):
        for key, child in value.items():
            child_field = f"{field}.{key}"
            check_retained_control_endpoints_redacted(child, errors, child_field)
        return

    if isinstance(value, list):
        for index, child in enumerate(value):
            check_retained_control_endpoints_redacted(
                child,
                errors,
                f"{field}[{index}]",
            )


def check_checks(report: dict[str, Any], errors: list[str]) -> None:
    raw_checks = report.get("checks")
    if not isinstance(raw_checks, list):
        errors.append("checks must be a list")
        return

    names: set[str] = set()
    for item in raw_checks:
        if isinstance(item, str):
            names.add(item)
        elif isinstance(item, dict) and isinstance(item.get("name"), str):
            if item.get("status", "ok") != "ok":
                errors.append(f"check {item['name']!r} did not report ok")
            names.add(item["name"])
        else:
            errors.append(f"unsupported check entry: {item!r}")

    missing = sorted(REQUIRED_CHECKS - names)
    if missing:
        errors.append(f"missing required checks: {', '.join(missing)}")


def check_artifacts(
    report: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
    artifact_base: Path | None,
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        errors.append("artifacts must be an object")
        return

    for key in REQUIRED_ARTIFACTS:
        value = artifacts.get(key)
        if not isinstance(value, str) or not value.strip():
            errors.append(f"artifacts.{key} must be a non-empty path")
            continue
        if require_artifacts and resolve_artifact_path(value, artifact_base) is None:
            errors.append(f"artifacts.{key} does not exist: {value}")


def load_json_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
    key: str,
) -> Any | None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return None

    value = artifacts.get(key)
    if not isinstance(value, str) or not value.strip():
        return None

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return None

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.{key} is invalid JSON: {error}")
        return None
    check_retained_control_endpoints_redacted(payload, errors, f"artifacts.{key}")
    return payload


def check_mount_list_artifact(
    report: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
    artifact_base: Path | None,
) -> list[dict[str, Any]]:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return []

    value = artifacts.get("mount_list")
    if not isinstance(value, str) or not value.strip():
        return []

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return []

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.mount_list is invalid JSON: {error}")
        return []
    if not isinstance(payload, list):
        errors.append("artifacts.mount_list root must be a list")
        return []
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.mount_list")
    if require_artifacts and not payload:
        errors.append("artifacts.mount_list must include a running nfs entry")
        return []

    running_nfs_entries: list[dict[str, Any]] = []
    for index, entry in enumerate(payload):
        field = f"artifacts.mount_list[{index}]"
        if not isinstance(entry, dict):
            errors.append(f"{field} must be an object")
            continue
        if entry.get("backend") != "nfs":
            continue

        state = entry.get("state")
        if not isinstance(state, str) or not state.startswith("running"):
            continue

        running_nfs_entries.append(entry)
        for key in ("source", "mountpoint", "log_path", "control_endpoint"):
            check_nonempty_string(entry.get(key), errors, f"{field}.{key}")
        check_positive_int(entry.get("pid"), errors, f"{field}.pid")
        if not isinstance(entry.get("read_only"), bool):
            errors.append(f"{field}.read_only must be a boolean")

    if require_artifacts and not running_nfs_entries:
        errors.append("artifacts.mount_list must include a running nfs entry")
    return running_nfs_entries


def check_mount_doctor_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("mount_doctor")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.mount_doctor is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.mount_doctor root must be an object")
        return

    if payload.get("requested_backend") != "nfs":
        errors.append("artifacts.mount_doctor.requested_backend must be nfs")
    if payload.get("checked_backend") != "nfs":
        errors.append("artifacts.mount_doctor.checked_backend must be nfs")
    doctor_mountpoint = check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.mount_doctor.mountpoint",
    )
    if doctor_mountpoint and mount_list_entries:
        if not any(entry.get("mountpoint") == doctor_mountpoint for entry in mount_list_entries):
            errors.append(
                "artifacts.mount_doctor.mountpoint must match a running nfs mount-list entry"
            )

    summary = payload.get("summary")
    summary_counts: dict[str, int] = {}
    if not isinstance(summary, dict):
        errors.append("artifacts.mount_doctor.summary must be an object")
    else:
        if summary.get("ready") is not True:
            errors.append("artifacts.mount_doctor.summary.ready must be true")
        for key in ("ok", "warn", "fail"):
            value = check_nonnegative_int(
                summary.get(key),
                errors,
                f"artifacts.mount_doctor.summary.{key}",
            )
            if value is not None:
                summary_counts[key] = value
        if summary_counts.get("fail") is not None and summary_counts["fail"] != 0:
            errors.append("artifacts.mount_doctor.summary.fail must be 0")

    checks = payload.get("checks")
    if not isinstance(checks, list) or not checks:
        errors.append("artifacts.mount_doctor.checks must be a non-empty array")
    else:
        statuses_by_name: dict[str, list[str]] = {}
        status_counts = {"ok": 0, "warn": 0, "fail": 0}
        for index, item in enumerate(checks):
            field = f"artifacts.mount_doctor.checks[{index}]"
            if not isinstance(item, dict):
                errors.append(f"{field} must be an object")
                continue
            name = check_nonempty_string(item.get("name"), errors, f"{field}.name")
            raw_status = item.get("status")
            status = raw_status if isinstance(raw_status, str) else None
            if name is not None and status is not None:
                statuses_by_name.setdefault(name, []).append(status)
            if status == "fail":
                status_counts["fail"] += 1
                errors.append(f"{field}.status must not be fail")
            elif status in {"ok", "warn"}:
                status_counts[status] += 1
            else:
                errors.append(f"{field}.status must be ok or warn")
        for key, expected in summary_counts.items():
            if expected != status_counts[key]:
                errors.append(
                    f"artifacts.mount_doctor.summary.{key} must match check statuses"
                )
        for required in (
            "nfs feature",
            "nfs helper",
            "nfs helper version",
            "nfs helper layout",
            "nfs preflight",
        ):
            statuses = statuses_by_name.get(required)
            if statuses is None:
                errors.append(f"artifacts.mount_doctor.checks must include {required}")
            elif "ok" not in statuses:
                errors.append(f"artifacts.mount_doctor.checks entry {required} must be ok")

    preflight = payload.get("nfs_preflight")
    if not isinstance(preflight, dict):
        errors.append("artifacts.mount_doctor.nfs_preflight must be an object")
        return
    for key in (
        "ready",
        "backend_available",
        "native_client_available",
        "mountpoint_ready",
        "loopback_bind_ready",
        "control_endpoint_ready",
        "privilege_ready",
    ):
        if preflight.get(key) is not True:
            errors.append(f"artifacts.mount_doctor.nfs_preflight.{key} must be true")

    blocker_count = check_nonnegative_int(
        preflight.get("blocker_count"),
        errors,
        "artifacts.mount_doctor.nfs_preflight.blocker_count",
    )
    if blocker_count is not None and blocker_count != 0:
        errors.append("artifacts.mount_doctor.nfs_preflight.blocker_count must be 0")
    blockers = preflight.get("blockers")
    if not isinstance(blockers, list):
        errors.append("artifacts.mount_doctor.nfs_preflight.blockers must be an array")
    elif blockers:
        errors.append("artifacts.mount_doctor.nfs_preflight.blockers must be empty")
    elif blocker_count is not None and blocker_count != len(blockers):
        errors.append(
            "artifacts.mount_doctor.nfs_preflight.blocker_count must match blockers"
        )
    warning_count = check_nonnegative_int(
        preflight.get("warning_count"),
        errors,
        "artifacts.mount_doctor.nfs_preflight.warning_count",
    )
    warnings = preflight.get("warnings")
    if not isinstance(warnings, list):
        errors.append("artifacts.mount_doctor.nfs_preflight.warnings must be an array")
    elif warning_count is not None and warning_count != len(warnings):
        errors.append(
            "artifacts.mount_doctor.nfs_preflight.warning_count must match warnings"
        )


def check_nonnegative_int(value: Any, errors: list[str], field: str) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        errors.append(f"{field} must be a non-negative integer")
        return None
    if value < 0:
        errors.append(f"{field} must be a non-negative integer")
        return None
    return value


def check_optional_nonnegative_int(
    value: Any,
    errors: list[str],
    field: str,
) -> int | None:
    if value is None:
        return None
    return check_nonnegative_int(value, errors, field)


def check_positive_int(value: Any, errors: list[str], field: str) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        errors.append(f"{field} must be a positive integer")
        return None
    if value <= 0:
        errors.append(f"{field} must be a positive integer")
        return None
    return value


def check_positive_number(value: Any, errors: list[str], field: str) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        errors.append(f"{field} must be a positive number")
        return None
    numeric = float(value)
    if numeric <= 0:
        errors.append(f"{field} must be a positive number")
        return None
    return numeric


def check_protocol_counter_snapshot(
    value: Any,
    errors: list[str],
    field: str,
) -> dict[str, int] | None:
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return None

    snapshot: dict[str, int] = {}
    for key in ("read_rpcs", "read_requested_bytes", "read_returned_bytes"):
        counter = check_nonnegative_int(value.get(key), errors, f"{field}.{key}")
        if counter is not None:
            snapshot[key] = counter
    if len(snapshot) != 3:
        return None
    return snapshot


def check_named_counter_snapshot(
    value: Any,
    errors: list[str],
    field: str,
    keys: tuple[str, ...],
) -> dict[str, int] | None:
    snapshot = check_counter_group(value, errors, field, keys)
    if snapshot is None:
        return None
    return snapshot


def check_counter_delta(
    before: dict[str, int],
    after: dict[str, int],
    delta: dict[str, int],
    errors: list[str],
    field: str,
) -> None:
    for key, before_value in before.items():
        expected = after[key] - before_value
        if delta[key] != expected:
            errors.append(f"{field}.{key} must equal after-before")


def check_efficiency_metrics(
    value: Any,
    errors: list[str],
    field: str,
) -> dict[str, float] | None:
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return None

    metrics: dict[str, float] = {}
    for key in (
        "requested_bytes_per_user_byte",
        "returned_bytes_per_user_byte",
        "read_rpcs_per_mib",
    ):
        metric = check_positive_number(value.get(key), errors, f"{field}.{key}")
        if metric is not None:
            metrics[key] = metric
    if len(metrics) != 3:
        return None
    return metrics


def check_counter_group(
    value: Any,
    errors: list[str],
    field: str,
    keys: tuple[str, ...],
) -> dict[str, int] | None:
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return None

    counters: dict[str, int] = {}
    for key in keys:
        counter = check_nonnegative_int(value.get(key), errors, f"{field}.{key}")
        if counter is not None:
            counters[key] = counter
    if len(counters) != len(keys):
        return None
    return counters


def check_adaptive_read_status(
    value: Any,
    errors: list[str],
    field: str,
) -> None:
    check_counter_group(
        value,
        errors,
        field,
        ("first", "sequential", "strided", "repeated", "random"),
    )


def check_vfs_source_read_status(
    value: Any,
    errors: list[str],
    field: str,
) -> None:
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return

    for key in ("reads", "bytes"):
        check_nonnegative_int(value.get(key), errors, f"{field}.{key}")
    check_adaptive_read_status(value.get("adaptive"), errors, f"{field}.adaptive")


def check_vfs_runtime_status(
    runtime: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
) -> None:
    field = "artifacts.mount_status.nfs_runtime.vfs"
    counters = check_counter_group(
        runtime.get("vfs"),
        errors,
        field,
        (
            "open_read_calls",
            "read_at_calls",
            "returned_bytes",
            "stale_generation_rejections",
            "stale_overlay_view_rejections",
            "stale_overlay_file_rejections",
            "source_cache_entries",
            "source_cache_max_entries",
            "source_cache_estimated_bytes",
            "source_cache_max_estimated_bytes",
            "source_cache_hits",
            "resolver_calls_avoided",
            "source_cache_misses",
            "source_cache_evictions",
            "source_cache_invalidations",
            "source_cache_stale_evictions",
            "invalidation_path_events",
            "invalidation_subtree_events",
            "invalidation_rename_events",
            "invalidation_generation_events",
            "invalidation_overlay_reset_events",
            "invalidation_compacted_full_resets",
        ),
    )
    vfs = runtime.get("vfs")
    if not isinstance(vfs, dict):
        return

    if require_artifacts and counters is not None:
        if counters["read_at_calls"] == 0:
            errors.append(f"{field}.read_at_calls must be positive")
        if counters["returned_bytes"] == 0:
            errors.append(f"{field}.returned_bytes must be positive")
        if counters["source_cache_max_entries"] == 0:
            errors.append(f"{field}.source_cache_max_entries must be positive")
        if counters["source_cache_max_estimated_bytes"] == 0:
            errors.append(f"{field}.source_cache_max_estimated_bytes must be positive")

    for key in ("base_pointer", "base_blob", "base_empty", "overlay_file"):
        check_vfs_source_read_status(vfs.get(key), errors, f"{field}.{key}")


def check_hydration_runtime_status(
    runtime: dict[str, Any],
    errors: list[str],
) -> None:
    check_counter_group(
        runtime.get("hydration"),
        errors,
        "artifacts.mount_status.nfs_runtime.hydration",
        (
            "read_range_requests",
            "read_range_requested_bytes",
            "read_range_returned_bytes",
            "read_window_cache_hits",
            "read_window_cache_misses",
            "read_window_inflight_waits",
            "read_window_remote_fetches",
            "read_window_remote_bytes",
            "read_window_prefetch_requests",
            "read_window_prefetch_scheduled",
            "read_window_prefetch_skipped",
            "read_window_prefetch_errors",
            "chunk_cache_hits",
            "chunk_cache_misses",
            "chunk_inflight_waits",
            "chunk_remote_fetches",
            "chunk_remote_bytes",
        ),
    )


def check_write_journal_entry(
    value: Any,
    errors: list[str],
    field: str,
) -> dict[str, int | bool] | None:
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return None

    path = check_nonempty_string(value.get("path"), errors, f"{field}.path")
    overlay_version = check_nonnegative_int(
        value.get("overlay_version"),
        errors,
        f"{field}.overlay_version",
    )
    stability = value.get("last_write_stability")
    if stability not in {"unstable", "data_sync", "file_sync"}:
        errors.append(
            f"{field}.last_write_stability must be unstable, data_sync, or file_sync"
        )
    dirty_age = check_optional_nonnegative_int(
        value.get("dirty_age_secs"),
        errors,
        f"{field}.dirty_age_secs",
    )
    last_sync_error = value.get("last_sync_error")
    has_sync_error = last_sync_error is not None
    if has_sync_error and not isinstance(last_sync_error, str):
        errors.append(f"{field}.last_sync_error must be a string or null")
    elif isinstance(last_sync_error, str) and not last_sync_error:
        errors.append(f"{field}.last_sync_error must not be empty")

    if path is None or overlay_version is None:
        return None
    return {
        "dirty_age_secs": dirty_age if dirty_age is not None else -1,
        "has_sync_error": has_sync_error,
    }


def check_write_journal_runtime_status(
    runtime: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
) -> None:
    field = "artifacts.mount_status.nfs_runtime.write_journal"
    write_journal = runtime.get("write_journal")
    if not isinstance(write_journal, dict):
        errors.append(f"{field} must be an object")
        return

    pending_paths = check_nonnegative_int(
        write_journal.get("pending_paths"),
        errors,
        f"{field}.pending_paths",
    )
    oldest_dirty_age = check_optional_nonnegative_int(
        write_journal.get("oldest_dirty_age_secs"),
        errors,
        f"{field}.oldest_dirty_age_secs",
    )
    paths_with_sync_errors = check_nonnegative_int(
        write_journal.get("paths_with_sync_errors"),
        errors,
        f"{field}.paths_with_sync_errors",
    )
    sync_attempts = check_nonnegative_int(
        write_journal.get("sync_attempts"),
        errors,
        f"{field}.sync_attempts",
    )
    sync_successes = check_nonnegative_int(
        write_journal.get("sync_successes"),
        errors,
        f"{field}.sync_successes",
    )
    sync_failures = check_nonnegative_int(
        write_journal.get("sync_failures"),
        errors,
        f"{field}.sync_failures",
    )
    total_sync_latency = check_nonnegative_int(
        write_journal.get("total_sync_latency_ms"),
        errors,
        f"{field}.total_sync_latency_ms",
    )
    last_sync_latency = check_optional_nonnegative_int(
        write_journal.get("last_sync_latency_ms"),
        errors,
        f"{field}.last_sync_latency_ms",
    )
    max_sync_latency = check_optional_nonnegative_int(
        write_journal.get("max_sync_latency_ms"),
        errors,
        f"{field}.max_sync_latency_ms",
    )
    poisoned = write_journal.get("poisoned")
    if not isinstance(poisoned, bool):
        errors.append(f"{field}.poisoned must be a boolean")
    elif require_artifacts and poisoned:
        errors.append(f"{field}.poisoned must be false")

    entries = write_journal.get("entries")
    if not isinstance(entries, list):
        errors.append(f"{field}.entries must be an array")
        entries = []

    entry_snapshots: list[dict[str, int | bool]] = []
    for index, entry in enumerate(entries):
        snapshot = check_write_journal_entry(entry, errors, f"{field}.entries[{index}]")
        if snapshot is not None:
            entry_snapshots.append(snapshot)

    if pending_paths is not None and pending_paths != len(entries):
        errors.append(f"{field}.pending_paths must equal entries length")

    sync_error_entries = sum(
        1 for entry in entry_snapshots if bool(entry["has_sync_error"])
    )
    if (
        paths_with_sync_errors is not None
        and paths_with_sync_errors != sync_error_entries
    ):
        errors.append(
            f"{field}.paths_with_sync_errors must equal entries with last_sync_error"
        )

    dirty_ages = [
        int(entry["dirty_age_secs"])
        for entry in entry_snapshots
        if int(entry["dirty_age_secs"]) >= 0
    ]
    if dirty_ages:
        expected_oldest = max(dirty_ages)
        if oldest_dirty_age != expected_oldest:
            errors.append(f"{field}.oldest_dirty_age_secs must equal max entry dirty age")
    elif oldest_dirty_age is not None:
        errors.append(f"{field}.oldest_dirty_age_secs must be null when no entries are dirty")

    if (
        sync_attempts is not None
        and sync_successes is not None
        and sync_failures is not None
        and sync_successes + sync_failures > sync_attempts
    ):
        errors.append(f"{field} sync successes and failures must not exceed attempts")
    if sync_attempts is not None and sync_attempts > 0:
        if last_sync_latency is None:
            errors.append(f"{field}.last_sync_latency_ms must be present after sync")
        if max_sync_latency is None:
            errors.append(f"{field}.max_sync_latency_ms must be present after sync")
    if (
        last_sync_latency is not None
        and max_sync_latency is not None
        and last_sync_latency > max_sync_latency
    ):
        errors.append(f"{field}.last_sync_latency_ms must not exceed max_sync_latency_ms")
    if (
        total_sync_latency is not None
        and max_sync_latency is not None
        and total_sync_latency < max_sync_latency
    ):
        errors.append(f"{field}.total_sync_latency_ms must cover max_sync_latency_ms")


def check_native_read_benchmark_artifact(
    report: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
    thresholds: dict[str, float],
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("native_read_benchmark")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.native_read_benchmark is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.native_read_benchmark root must be an object")
        return

    if payload.get("schema_version") != 1:
        errors.append("artifacts.native_read_benchmark.schema_version must be 1")
    if payload.get("suite") != "nfs-native-read-benchmark":
        errors.append(
            "artifacts.native_read_benchmark.suite must be nfs-native-read-benchmark"
        )
    if payload.get("scenario") != "native_sequential_read":
        errors.append(
            "artifacts.native_read_benchmark.scenario must be native_sequential_read"
        )
    if not isinstance(payload.get("path"), str) or not payload["path"]:
        errors.append("artifacts.native_read_benchmark.path must be a non-empty string")
    native_mountpoint = (
        payload.get("mountpoint") if isinstance(payload.get("mountpoint"), str) else None
    )
    if not native_mountpoint:
        errors.append("artifacts.native_read_benchmark.mountpoint must be a non-empty string")
    elif mount_list_entries:
        if not any(entry.get("mountpoint") == native_mountpoint for entry in mount_list_entries):
            errors.append(
                "artifacts.native_read_benchmark.mountpoint must match a running nfs mount-list entry"
            )

    file_size = check_nonnegative_int(
        payload.get("file_size"),
        errors,
        "artifacts.native_read_benchmark.file_size",
    )
    read_size = check_nonnegative_int(
        payload.get("read_size"),
        errors,
        "artifacts.native_read_benchmark.read_size",
    )
    reads = check_nonnegative_int(
        payload.get("reads"),
        errors,
        "artifacts.native_read_benchmark.reads",
    )
    bytes_returned = check_nonnegative_int(
        payload.get("bytes_returned"),
        errors,
        "artifacts.native_read_benchmark.bytes_returned",
    )
    check_nonnegative_int(
        payload.get("elapsed_ms"),
        errors,
        "artifacts.native_read_benchmark.elapsed_ms",
    )
    mib_per_sec = check_positive_number(
        payload.get("mib_per_sec"),
        errors,
        "artifacts.native_read_benchmark.mib_per_sec",
    )

    if file_size is not None and file_size == 0:
        errors.append("artifacts.native_read_benchmark.file_size must be positive")
    if read_size is not None and read_size == 0:
        errors.append("artifacts.native_read_benchmark.read_size must be positive")
    if reads is not None and reads == 0:
        errors.append("artifacts.native_read_benchmark.reads must be positive")
    if bytes_returned is not None and bytes_returned == 0:
        errors.append("artifacts.native_read_benchmark.bytes_returned must be positive")
    if read_size is not None and file_size is not None and read_size > file_size:
        errors.append("artifacts.native_read_benchmark.read_size must not exceed file_size")
    if (
        bytes_returned is not None
        and file_size is not None
        and bytes_returned < file_size
    ):
        errors.append(
            "artifacts.native_read_benchmark.bytes_returned must cover at least one file pass"
        )
    checksum = payload.get("sha256")
    if not isinstance(checksum, str) or len(checksum) != 64:
        errors.append("artifacts.native_read_benchmark.sha256 must be a SHA-256 hex string")
    elif any(char not in "0123456789abcdef" for char in checksum):
        errors.append("artifacts.native_read_benchmark.sha256 must be lowercase hex")

    if require_artifacts and bytes_returned == 0:
        errors.append("artifacts.native_read_benchmark.bytes_returned must be positive")

    protocol_before = check_protocol_counter_snapshot(
        payload.get("nfs_protocol_before"),
        errors,
        "artifacts.native_read_benchmark.nfs_protocol_before",
    )
    protocol_after = check_protocol_counter_snapshot(
        payload.get("nfs_protocol_after"),
        errors,
        "artifacts.native_read_benchmark.nfs_protocol_after",
    )
    protocol_delta = check_protocol_counter_snapshot(
        payload.get("nfs_protocol_delta"),
        errors,
        "artifacts.native_read_benchmark.nfs_protocol_delta",
    )
    if protocol_before is None or protocol_after is None or protocol_delta is None:
        return

    check_counter_delta(
        protocol_before,
        protocol_after,
        protocol_delta,
        errors,
        "artifacts.native_read_benchmark.nfs_protocol_delta",
    )
    for key in NATIVE_PROTOCOL_COUNTERS:
        if protocol_delta[key] <= 0:
            errors.append(
                f"artifacts.native_read_benchmark.nfs_protocol_delta.{key} must be positive"
            )
    if file_size is not None and protocol_delta["read_returned_bytes"] < file_size:
        errors.append(
            "artifacts.native_read_benchmark.nfs_protocol_delta.read_returned_bytes must cover at least one file pass"
        )

    read_leases_before = check_named_counter_snapshot(
        payload.get("nfs_read_leases_before"),
        errors,
        "artifacts.native_read_benchmark.nfs_read_leases_before",
        NATIVE_READ_LEASE_COUNTERS,
    )
    read_leases_after = check_named_counter_snapshot(
        payload.get("nfs_read_leases_after"),
        errors,
        "artifacts.native_read_benchmark.nfs_read_leases_after",
        NATIVE_READ_LEASE_COUNTERS,
    )
    read_leases_delta = check_named_counter_snapshot(
        payload.get("nfs_read_leases_delta"),
        errors,
        "artifacts.native_read_benchmark.nfs_read_leases_delta",
        NATIVE_READ_LEASE_COUNTERS,
    )
    if (
        read_leases_before is None
        or read_leases_after is None
        or read_leases_delta is None
    ):
        return
    check_counter_delta(
        read_leases_before,
        read_leases_after,
        read_leases_delta,
        errors,
        "artifacts.native_read_benchmark.nfs_read_leases_delta",
    )
    for key in ("hits", "misses"):
        if read_leases_delta[key] <= 0:
            errors.append(
                f"artifacts.native_read_benchmark.nfs_read_leases_delta.{key} must be positive"
            )

    vfs_before = check_named_counter_snapshot(
        payload.get("nfs_vfs_before"),
        errors,
        "artifacts.native_read_benchmark.nfs_vfs_before",
        NATIVE_VFS_COUNTERS,
    )
    vfs_after = check_named_counter_snapshot(
        payload.get("nfs_vfs_after"),
        errors,
        "artifacts.native_read_benchmark.nfs_vfs_after",
        NATIVE_VFS_COUNTERS,
    )
    vfs_delta = check_named_counter_snapshot(
        payload.get("nfs_vfs_delta"),
        errors,
        "artifacts.native_read_benchmark.nfs_vfs_delta",
        NATIVE_VFS_COUNTERS,
    )
    if vfs_before is None or vfs_after is None or vfs_delta is None:
        return
    check_counter_delta(
        vfs_before,
        vfs_after,
        vfs_delta,
        errors,
        "artifacts.native_read_benchmark.nfs_vfs_delta",
    )
    for key in ("read_at_calls", "returned_bytes"):
        if vfs_delta[key] <= 0:
            errors.append(
                f"artifacts.native_read_benchmark.nfs_vfs_delta.{key} must be positive"
            )

    hydration_before = check_named_counter_snapshot(
        payload.get("nfs_hydration_before"),
        errors,
        "artifacts.native_read_benchmark.nfs_hydration_before",
        NATIVE_HYDRATION_COUNTERS,
    )
    hydration_after = check_named_counter_snapshot(
        payload.get("nfs_hydration_after"),
        errors,
        "artifacts.native_read_benchmark.nfs_hydration_after",
        NATIVE_HYDRATION_COUNTERS,
    )
    hydration_delta = check_named_counter_snapshot(
        payload.get("nfs_hydration_delta"),
        errors,
        "artifacts.native_read_benchmark.nfs_hydration_delta",
        NATIVE_HYDRATION_COUNTERS,
    )
    if (
        hydration_before is None
        or hydration_after is None
        or hydration_delta is None
    ):
        return
    check_counter_delta(
        hydration_before,
        hydration_after,
        hydration_delta,
        errors,
        "artifacts.native_read_benchmark.nfs_hydration_delta",
    )

    efficiency = check_efficiency_metrics(
        payload.get("efficiency"),
        errors,
        "artifacts.native_read_benchmark.efficiency",
    )
    if efficiency is None:
        return

    if bytes_returned is None or bytes_returned == 0:
        return
    expected_requested_ratio = protocol_delta["read_requested_bytes"] / bytes_returned
    expected_returned_ratio = protocol_delta["read_returned_bytes"] / bytes_returned
    expected_rpcs_per_mib = protocol_delta["read_rpcs"] / (bytes_returned / (1024 * 1024))
    for key, expected in (
        ("requested_bytes_per_user_byte", expected_requested_ratio),
        ("returned_bytes_per_user_byte", expected_returned_ratio),
        ("read_rpcs_per_mib", expected_rpcs_per_mib),
    ):
        if abs(efficiency[key] - expected) > max(0.000001, expected * 0.000001):
            errors.append(
                f"artifacts.native_read_benchmark.efficiency.{key} must match protocol delta"
            )

    min_mib_per_sec = thresholds.get("min_mib_per_sec")
    if min_mib_per_sec is not None and mib_per_sec is not None and mib_per_sec < min_mib_per_sec:
        errors.append(
            "artifacts.native_read_benchmark.mib_per_sec is below the configured threshold"
        )
    for metric_key, threshold_key in (
        ("requested_bytes_per_user_byte", "max_requested_bytes_per_user_byte"),
        ("returned_bytes_per_user_byte", "max_returned_bytes_per_user_byte"),
        ("read_rpcs_per_mib", "max_read_rpcs_per_mib"),
    ):
        threshold = thresholds.get(threshold_key)
        if threshold is not None and efficiency[metric_key] > threshold:
            errors.append(
                f"artifacts.native_read_benchmark.efficiency.{metric_key} exceeds the configured threshold"
            )
    user_mib = bytes_returned / (1024 * 1024)
    read_lease_hits_per_mib = read_leases_delta["hits"] / user_mib
    min_read_lease_hits = thresholds.get("min_read_lease_hits_per_mib")
    if (
        min_read_lease_hits is not None
        and read_lease_hits_per_mib < min_read_lease_hits
    ):
        errors.append(
            "artifacts.native_read_benchmark.read_lease_hits_per_mib is below the configured threshold"
        )
    read_lease_misses_per_mib = read_leases_delta["misses"] / user_mib
    max_read_lease_misses = thresholds.get("max_read_lease_misses_per_mib")
    if (
        max_read_lease_misses is not None
        and read_lease_misses_per_mib > max_read_lease_misses
    ):
        errors.append(
            "artifacts.native_read_benchmark.read_lease_misses_per_mib exceeds the configured threshold"
        )


def check_mount_status_artifact(
    report: dict[str, Any],
    errors: list[str],
    require_artifacts: bool,
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("mount_status")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.mount_status is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.mount_status root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.mount_status")

    if payload.get("backend") != "nfs":
        errors.append("artifacts.mount_status backend must be nfs")

    status_mountpoint = check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.mount_status.mountpoint",
    )
    status_source = check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.mount_status.source",
    )
    status_state = check_nonempty_string(
        payload.get("state"),
        errors,
        "artifacts.mount_status.state",
    )
    status_log_path = check_nonempty_string(
        payload.get("log_path"),
        errors,
        "artifacts.mount_status.log_path",
    )
    status_control_endpoint = check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.mount_status.control_endpoint",
    )
    status_pid = check_positive_int(
        payload.get("pid"),
        errors,
        "artifacts.mount_status.pid",
    )
    status_read_only = payload.get("read_only")
    if not isinstance(status_read_only, bool):
        errors.append("artifacts.mount_status.read_only must be a boolean")
    if (
        require_artifacts
        and status_state is not None
        and not status_state.startswith("running")
    ):
        errors.append("artifacts.mount_status.state must be running")

    if mount_list_entries and status_mountpoint and status_source:
        matching_entries = [
            entry
            for entry in mount_list_entries
            if entry.get("mountpoint") == status_mountpoint
            or entry.get("source") == status_source
        ]
        if not matching_entries:
            errors.append(
                "artifacts.mount_status must match a running nfs mount-list entry"
            )
        else:
            entry = matching_entries[0]
            for key, status_value in (
                ("mountpoint", status_mountpoint),
                ("source", status_source),
                ("log_path", status_log_path),
                ("control_endpoint", status_control_endpoint),
            ):
                if status_value is not None and entry.get(key) != status_value:
                    errors.append(
                        f"artifacts.mount_status.{key} must match artifacts.mount_list entry"
                    )
            if isinstance(status_read_only, bool) and entry.get("read_only") != status_read_only:
                errors.append(
                    "artifacts.mount_status.read_only must match artifacts.mount_list entry"
                )
            if status_pid is not None and entry.get("pid") != status_pid:
                errors.append(
                    "artifacts.mount_status.pid must match artifacts.mount_list entry"
                )

    runtime = payload.get("nfs_runtime")
    if not isinstance(runtime, dict):
        errors.append("artifacts.mount_status.nfs_runtime must be an object")
        return

    lifecycle = runtime.get("lifecycle")
    if not isinstance(lifecycle, dict):
        errors.append("artifacts.mount_status.nfs_runtime.lifecycle must be an object")
        return

    server_bind_ms = check_nonnegative_int(
        lifecycle.get("server_bind_ms"),
        errors,
        "artifacts.mount_status.nfs_runtime.lifecycle.server_bind_ms",
    )
    native_mount_ms = check_nonnegative_int(
        lifecycle.get("native_mount_ms"),
        errors,
        "artifacts.mount_status.nfs_runtime.lifecycle.native_mount_ms",
    )
    startup_ms = check_nonnegative_int(
        lifecycle.get("startup_ms"),
        errors,
        "artifacts.mount_status.nfs_runtime.lifecycle.startup_ms",
    )
    if (
        startup_ms is not None
        and server_bind_ms is not None
        and startup_ms < server_bind_ms
    ):
        errors.append(
            "artifacts.mount_status.nfs_runtime.lifecycle.startup_ms must cover server_bind_ms"
        )
    if (
        startup_ms is not None
        and native_mount_ms is not None
        and startup_ms < native_mount_ms
    ):
        errors.append(
            "artifacts.mount_status.nfs_runtime.lifecycle.startup_ms must cover native_mount_ms"
        )

    protocol = runtime.get("protocol")
    protocol_counters = check_counter_group(
        protocol,
        errors,
        "artifacts.mount_status.nfs_runtime.protocol",
        NFS_RUNTIME_PROTOCOL_COUNTERS,
    )
    if protocol_counters is not None:
        read_rpcs = protocol_counters["read_rpcs"]
        if require_artifacts and read_rpcs == 0:
            errors.append(
                "artifacts.mount_status.nfs_runtime.protocol.read_rpcs must be positive"
            )
        if require_artifacts and protocol_counters["read_returned_bytes"] == 0:
            errors.append(
                "artifacts.mount_status.nfs_runtime.protocol.read_returned_bytes must be positive"
            )

    read_leases = runtime.get("read_leases")
    if not isinstance(read_leases, dict):
        errors.append("artifacts.mount_status.nfs_runtime.read_leases must be an object")
    else:
        check_nonnegative_int(
            read_leases.get("entries"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.entries",
        )
        check_positive_int(
            read_leases.get("max_entries"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.max_entries",
        )
        check_nonnegative_int(
            read_leases.get("estimated_bytes"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.estimated_bytes",
        )
        check_positive_int(
            read_leases.get("max_estimated_bytes"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.max_estimated_bytes",
        )
        for key in (
            "pinned_entries",
            "active_pins",
            "temporary_overflows",
            "evictions",
            "stale_retries",
        ):
            check_nonnegative_int(
                read_leases.get(key),
                errors,
                f"artifacts.mount_status.nfs_runtime.read_leases.{key}",
            )
        hits = check_nonnegative_int(
            read_leases.get("hits"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.hits",
        )
        misses = check_nonnegative_int(
            read_leases.get("misses"),
            errors,
            "artifacts.mount_status.nfs_runtime.read_leases.misses",
        )
        if require_artifacts and hits == 0:
            errors.append(
                "artifacts.mount_status.nfs_runtime.read_leases.hits must be positive"
            )
        if require_artifacts and misses == 0:
            errors.append(
                "artifacts.mount_status.nfs_runtime.read_leases.misses must be positive"
            )

    directory_pages = runtime.get("directory_pages")
    if not isinstance(directory_pages, dict):
        errors.append("artifacts.mount_status.nfs_runtime.directory_pages must be an object")
    else:
        for key in (
            "entries",
            "max_entries",
            "estimated_bytes",
            "max_estimated_bytes",
            "hits",
            "misses",
            "evictions",
            "stale_evictions",
        ):
            check_nonnegative_int(
                directory_pages.get(key),
                errors,
                f"artifacts.mount_status.nfs_runtime.directory_pages.{key}",
            )

    check_vfs_runtime_status(runtime, errors, require_artifacts)
    check_hydration_runtime_status(runtime, errors)
    check_write_journal_runtime_status(runtime, errors, require_artifacts)


def check_control_status_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
) -> None:
    payload = load_json_artifact(report, errors, artifact_base, "control_status")
    if payload is None:
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.control_status root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.control_status")

    if payload.get("backend") != "nfs":
        errors.append("artifacts.control_status backend must be nfs")
    state = check_nonempty_string(
        payload.get("state"),
        errors,
        "artifacts.control_status.state",
    )
    if state is not None and not state.startswith("running"):
        errors.append("artifacts.control_status.state must be running")
    check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.control_status.mountpoint",
    )
    check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.control_status.source",
    )
    check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.control_status.control_endpoint",
    )
    check_positive_int(payload.get("pid"), errors, "artifacts.control_status.pid")
    control_runtime = payload.get("nfs_runtime")
    if not isinstance(control_runtime, dict):
        errors.append("artifacts.control_status.nfs_runtime must be an object")

    mount_status = load_json_artifact(report, errors, artifact_base, "mount_status")
    if not isinstance(mount_status, dict):
        return
    for key in (
        "backend",
        "mountpoint",
        "source",
        "state",
        "pid",
        "read_only",
        "control_endpoint",
        "log_path",
    ):
        if payload.get(key) != mount_status.get(key):
            errors.append(
                f"artifacts.control_status.{key} must match artifacts.mount_status"
            )
    if (
        isinstance(control_runtime, dict)
        and isinstance(mount_status.get("nfs_runtime"), dict)
        and control_runtime != mount_status["nfs_runtime"]
    ):
        errors.append(
            "artifacts.control_status.nfs_runtime must match artifacts.mount_status"
        )


def check_writeback_content_checks(
    value: Any,
    errors: list[str],
    platform: str | None,
) -> None:
    field = "artifacts.writeback_check.content_checks"
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return
    required = [
        "hello_appended",
        "renamed_file_created",
        "exclusive_file_created",
        "gitdir_preserved",
        "gitdir_overwrite_rejected",
        "gitdir_rename_rejected",
        "removed_directory_absent",
    ]
    if platform in UNIX_SYMLINK_PLATFORMS:
        required.append("symlink_created")
    for key in required:
        if value.get(key) is not True:
            errors.append(f"{field}.{key} must be true")


def check_writeback_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
    platform: str | None,
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("writeback_check")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.writeback_check is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.writeback_check root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.writeback_check")

    if payload.get("schema_version") != 1:
        errors.append("artifacts.writeback_check.schema_version must be 1")
    if payload.get("action") != "writeback":
        errors.append("artifacts.writeback_check.action must be writeback")

    mountpoint = check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.writeback_check.mountpoint",
    )
    source = check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.writeback_check.source",
    )
    state = check_nonempty_string(
        payload.get("state"),
        errors,
        "artifacts.writeback_check.state",
    )
    control_endpoint = check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.writeback_check.control_endpoint",
    )
    log_path = check_nonempty_string(
        payload.get("log_path"),
        errors,
        "artifacts.writeback_check.log_path",
    )
    pid = check_positive_int(
        payload.get("pid"),
        errors,
        "artifacts.writeback_check.pid",
    )
    if state is not None and not state.startswith("running"):
        errors.append("artifacts.writeback_check.state must be running")
    check_writeback_content_checks(payload.get("content_checks"), errors, platform)

    if mount_list_entries and mountpoint and source:
        matching_entries = [
            entry
            for entry in mount_list_entries
            if entry.get("mountpoint") == mountpoint or entry.get("source") == source
        ]
        if not matching_entries:
            errors.append(
                "artifacts.writeback_check must match a running nfs mount-list entry"
            )
            return

        entry = matching_entries[0]
        for key, value in (
            ("mountpoint", mountpoint),
            ("source", source),
            ("control_endpoint", control_endpoint),
            ("log_path", log_path),
        ):
            if value is not None and entry.get(key) != value:
                errors.append(
                    f"artifacts.writeback_check.{key} must match artifacts.mount_list entry"
                )
        if pid is not None and entry.get("pid") != pid:
            errors.append(
                "artifacts.writeback_check.pid must match artifacts.mount_list entry"
            )


def check_unmount_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("unmount_check")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.unmount_check is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.unmount_check root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.unmount_check")

    if payload.get("schema_version") != 1:
        errors.append("artifacts.unmount_check.schema_version must be 1")
    if payload.get("action") != "control_shutdown":
        errors.append("artifacts.unmount_check.action must be control_shutdown")
    if payload.get("mounted_after") is not False:
        errors.append("artifacts.unmount_check.mounted_after must be false")

    mountpoint = check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.unmount_check.mountpoint",
    )
    source = check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.unmount_check.source",
    )
    control_endpoint = check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.unmount_check.control_endpoint",
    )
    log_path = check_nonempty_string(
        payload.get("log_path"),
        errors,
        "artifacts.unmount_check.log_path",
    )
    pid = check_positive_int(
        payload.get("pid"),
        errors,
        "artifacts.unmount_check.pid",
    )

    if mount_list_entries and mountpoint and source:
        matching_entries = [
            entry
            for entry in mount_list_entries
            if entry.get("mountpoint") == mountpoint or entry.get("source") == source
        ]
        if not matching_entries:
            errors.append(
                "artifacts.unmount_check must match a running nfs mount-list entry"
            )
            return

        entry = matching_entries[0]
        for key, value in (
            ("mountpoint", mountpoint),
            ("source", source),
            ("control_endpoint", control_endpoint),
            ("log_path", log_path),
        ):
            if value is not None and entry.get(key) != value:
                errors.append(
                    f"artifacts.unmount_check.{key} must match artifacts.mount_list entry"
                )
        if pid is not None and entry.get("pid") != pid:
            errors.append("artifacts.unmount_check.pid must match artifacts.mount_list entry")


def check_control_shutdown_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
) -> None:
    payload = load_json_artifact(report, errors, artifact_base, "control_shutdown")
    if payload is None:
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.control_shutdown root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.control_shutdown")

    if payload.get("schema_version") != 1:
        errors.append("artifacts.control_shutdown.schema_version must be 1")
    if payload.get("action") != "control_shutdown":
        errors.append("artifacts.control_shutdown.action must be control_shutdown")
    if payload.get("mounted_after") is not False:
        errors.append("artifacts.control_shutdown.mounted_after must be false")
    check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.control_shutdown.mountpoint",
    )
    check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.control_shutdown.source",
    )
    check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.control_shutdown.control_endpoint",
    )
    check_positive_int(payload.get("pid"), errors, "artifacts.control_shutdown.pid")

    unmount_check = load_json_artifact(report, errors, artifact_base, "unmount_check")
    if not isinstance(unmount_check, dict):
        return
    for key in (
        "schema_version",
        "action",
        "mountpoint",
        "source",
        "pid",
        "control_endpoint",
        "log_path",
        "mounted_after",
    ):
        if payload.get(key) != unmount_check.get(key):
            errors.append(
                f"artifacts.control_shutdown.{key} must match artifacts.unmount_check"
            )


def check_remount_content_checks(
    value: Any,
    errors: list[str],
    platform: str | None,
) -> None:
    field = "artifacts.remount_check.content_checks"
    if not isinstance(value, dict):
        errors.append(f"{field} must be an object")
        return
    required = [
        "hello_preserved",
        "renamed_file_preserved",
        "exclusive_file_preserved",
        "gitdir_preserved",
        "removed_directory_absent",
    ]
    if platform in UNIX_SYMLINK_PLATFORMS:
        required.append("symlink_preserved")
    for key in required:
        if value.get(key) is not True:
            errors.append(f"{field}.{key} must be true")


def check_remount_artifact(
    report: dict[str, Any],
    errors: list[str],
    artifact_base: Path | None,
    mount_list_entries: list[dict[str, Any]],
    platform: str | None,
) -> None:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        return

    value = artifacts.get("remount_check")
    if not isinstance(value, str) or not value.strip():
        return

    path = resolve_artifact_path(value, artifact_base)
    if path is None:
        return

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"artifacts.remount_check is invalid JSON: {error}")
        return
    if not isinstance(payload, dict):
        errors.append("artifacts.remount_check root must be an object")
        return
    check_retained_control_endpoints_redacted(payload, errors, "artifacts.remount_check")

    if payload.get("schema_version") != 1:
        errors.append("artifacts.remount_check.schema_version must be 1")
    if payload.get("action") != "remount":
        errors.append("artifacts.remount_check.action must be remount")
    if payload.get("mounted_after") is not True:
        errors.append("artifacts.remount_check.mounted_after must be true")

    mountpoint = check_nonempty_string(
        payload.get("mountpoint"),
        errors,
        "artifacts.remount_check.mountpoint",
    )
    source = check_nonempty_string(
        payload.get("source"),
        errors,
        "artifacts.remount_check.source",
    )
    state = check_nonempty_string(
        payload.get("state"),
        errors,
        "artifacts.remount_check.state",
    )
    check_nonempty_string(
        payload.get("control_endpoint"),
        errors,
        "artifacts.remount_check.control_endpoint",
    )
    check_nonempty_string(
        payload.get("log_path"),
        errors,
        "artifacts.remount_check.log_path",
    )
    check_positive_int(payload.get("pid"), errors, "artifacts.remount_check.pid")
    if state is not None and not state.startswith("running"):
        errors.append("artifacts.remount_check.state must be running")
    check_remount_content_checks(payload.get("content_checks"), errors, platform)

    if mount_list_entries and mountpoint and source:
        if not any(
            entry.get("mountpoint") == mountpoint and entry.get("source") == source
            for entry in mount_list_entries
        ):
            errors.append(
                "artifacts.remount_check must match source and mountpoint from mount-list entry"
            )


def validate(
    report: dict[str, Any],
    *,
    expected_suite: str | None,
    expected_platform: str | None,
    expected_run_suffix: str | None,
    require_artifacts: bool,
    expected_git_commit: str | None = None,
    native_read_thresholds: dict[str, float] | None = None,
    artifact_base: Path | None = None,
) -> list[str]:
    errors: list[str] = []
    if expected_git_commit is not None and not is_full_git_object_id(expected_git_commit):
        errors.append("--expected-git-commit must be a lowercase full Git object id")

    check(report.get("schema_version") == 1, errors, "schema_version must be 1")
    check(report.get("status") == "ok", errors, "status must be ok")
    check(report.get("backend") == "nfs", errors, "backend must be nfs")

    suite = check_string_field(report, "suite", errors)
    platform = check_string_field(report, "platform", errors)
    run_id = check_string_field(report, "run_id", errors)
    git_commit = check_git_commit_field(report, errors)
    check_string_field(report, "artifact_root", errors)

    if suite:
        expected = EXPECTED_SUITES.get(suite)
        if expected is None:
            errors.append(f"unknown suite: {suite}")
        elif platform and platform != expected:
            errors.append(f"suite {suite} must report platform {expected}, got {platform}")

    if expected_suite is not None and suite != expected_suite:
        errors.append(f"expected suite {expected_suite}, got {suite or '<missing>'}")
    if expected_platform is not None and platform != expected_platform:
        errors.append(f"expected platform {expected_platform}, got {platform or '<missing>'}")
    if expected_run_suffix is not None and suite and run_id:
        expected_run_id = f"{suite}-{expected_run_suffix}"
        if run_id != expected_run_id:
            errors.append(f"expected run_id {expected_run_id}, got {run_id}")
    if expected_git_commit is not None and git_commit and git_commit != expected_git_commit:
        errors.append(f"expected git_commit {expected_git_commit}, got {git_commit}")

    crab_version = check_string_field(report, "crab_version", errors)
    helper_version = check_string_field(report, "helper_version", errors)
    if crab_version and not crab_version.startswith("crab "):
        errors.append(f"crab_version must start with 'crab ', got {crab_version!r}")
    if helper_version and not helper_version.startswith("crab "):
        errors.append(f"helper_version must start with 'crab ', got {helper_version!r}")
    if crab_version and helper_version and crab_version != helper_version:
        errors.append(
            f"helper_version must match crab_version, got {helper_version!r} and {crab_version!r}"
        )

    check_checks(report, errors)
    check_artifacts(report, errors, require_artifacts, artifact_base)
    mount_list_entries = check_mount_list_artifact(
        report,
        errors,
        require_artifacts,
        artifact_base,
    )
    check_mount_doctor_artifact(report, errors, artifact_base, mount_list_entries)
    check_native_read_benchmark_artifact(
        report,
        errors,
        require_artifacts,
        native_read_thresholds or {},
        artifact_base,
        mount_list_entries,
    )
    check_mount_status_artifact(
        report,
        errors,
        require_artifacts,
        artifact_base,
        mount_list_entries,
    )
    check_control_status_artifact(report, errors, artifact_base)
    check_writeback_artifact(report, errors, artifact_base, mount_list_entries, platform)
    check_unmount_artifact(report, errors, artifact_base, mount_list_entries)
    check_control_shutdown_artifact(report, errors, artifact_base)
    check_remount_artifact(report, errors, artifact_base, mount_list_entries, platform)

    return errors


def verify_report_file(
    path: Path,
    *,
    expected_suite: str | None,
    expected_platform: str | None,
    expected_run_suffix: str | None = None,
    require_artifacts: bool,
    thresholds: dict[str, float],
    artifact_base: Path | None,
    expected_git_commit: str | None = None,
) -> tuple[dict[str, Any] | None, list[str]]:
    try:
        report = load_report(path)
    except ValueError as error:
        return None, [str(error)]

    return (
        report,
        validate(
            report,
            expected_suite=expected_suite,
            expected_platform=expected_platform,
            expected_run_suffix=expected_run_suffix,
            expected_git_commit=expected_git_commit,
            require_artifacts=require_artifacts,
            native_read_thresholds=thresholds,
            artifact_base=artifact_base,
        ),
    )


def retained_run_suffix(
    report_path: Path,
    report: dict[str, Any],
    errors: list[str],
) -> str | None:
    suite = report.get("suite")
    run_id = report.get("run_id")
    if not isinstance(suite, str) or not suite:
        errors.append(f"{report_path}: suite must be a non-empty string")
        return None
    if not isinstance(run_id, str) or not run_id:
        errors.append(f"{report_path}: run_id must be a non-empty string")
        return None

    prefix = f"{suite}-"
    if not run_id.startswith(prefix):
        errors.append(
            f"{report_path}: run_id must start with suite prefix {prefix!r}"
        )
        return None
    suffix = run_id[len(prefix) :]
    if not suffix:
        errors.append(f"{report_path}: run_id suffix must be non-empty")
        return None
    return suffix


def check_retained_directory_consistency(
    root: Path,
    reports: list[tuple[Path, dict[str, Any]]],
) -> list[tuple[Path, list[str]]]:
    if len(reports) <= 1:
        return []

    errors: list[str] = []
    commits = sorted({str(report.get("git_commit", "")) for _path, report in reports})
    if len(commits) > 1:
        errors.append("mixed git_commit values in retained NFS smoke directory")

    suffixes: dict[str, list[Path]] = {}
    for report_path, report in reports:
        suffix = retained_run_suffix(report_path, report, errors)
        if suffix is not None:
            suffixes.setdefault(suffix, []).append(report_path)

    if len(suffixes) > 1:
        errors.append("mixed run_id suffixes in retained NFS smoke directory")

    if errors:
        return [(root, errors)]
    return []


def retained_directory_identity(
    reports: list[tuple[Path, dict[str, Any]]],
) -> tuple[str | None, str | None]:
    commits = sorted(
        {
            str(report.get("git_commit", ""))
            for _path, report in reports
            if isinstance(report.get("git_commit"), str) and report.get("git_commit")
        }
    )
    errors: list[str] = []
    suffixes = sorted(
        {
            suffix
            for report_path, report in reports
            for suffix in [retained_run_suffix(report_path, report, errors)]
            if suffix is not None
        }
    )
    commit = commits[0] if len(commits) == 1 else None
    run_suffix = suffixes[0] if len(suffixes) == 1 and not errors else None
    return commit, run_suffix


def write_verification_summary(
    path: Path,
    root: Path,
    reports: list[tuple[Path, dict[str, Any]]],
) -> None:
    git_commit, run_id_suffix = retained_directory_identity(reports)
    payload = {
        "schema_version": 1,
        "status": "ok",
        "root": str(root),
        "git_commit": git_commit,
        "run_id_suffix": run_id_suffix,
        "report_count": len(reports),
        "suites": sorted({str(report.get("suite", "")) for _path, report in reports}),
        "platforms": sorted(
            {str(report.get("platform", "")) for _path, report in reports}
        ),
        "reports": [
            {
                "path": str(report_path),
                "suite": report.get("suite"),
                "platform": report.get("platform"),
                "run_id": report.get("run_id"),
                "git_commit": report.get("git_commit"),
                "crab_version": report.get("crab_version"),
                "helper_version": report.get("helper_version"),
                "mount_doctor": mount_doctor_summary_for_report(report_path, report),
                "native_read": native_read_summary_for_report(report_path, report),
            }
            for report_path, report in reports
        ],
    }
    write_json(path, payload)


def verify_directory(
    root: Path,
    *,
    require_artifacts: bool,
    require_all_platforms: bool,
    expected_run_suffix: str | None,
    thresholds: dict[str, float],
    summary_output: Path | None,
    expected_git_commit: str | None = None,
    emit_output: bool = True,
) -> int:
    if not root.is_dir():
        if emit_output:
            print(f"error: NFS smoke report directory does not exist: {root}", file=sys.stderr)
        return 1

    report_paths = sorted(root.rglob("nfs-smoke-report.json"))
    if not report_paths:
        if emit_output:
            print(f"error: no nfs-smoke-report.json files found under {root}", file=sys.stderr)
        return 1

    valid_reports: list[tuple[Path, dict[str, Any]]] = []
    failures: list[tuple[Path, list[str]]] = []

    for report_path in report_paths:
        report, errors = verify_report_file(
            report_path,
            expected_suite=None,
            expected_platform=None,
            expected_run_suffix=expected_run_suffix,
            expected_git_commit=expected_git_commit,
            require_artifacts=require_artifacts,
            thresholds=thresholds,
            artifact_base=report_path.parent,
        )
        if errors:
            failures.append((report_path, errors))
            continue
        if report is not None:
            valid_reports.append((report_path, report))

    failures.extend(check_retained_directory_consistency(root, valid_reports))

    if require_all_platforms:
        suite_counts: dict[str, int] = {}
        for _path, report in valid_reports:
            suite = str(report.get("suite", ""))
            suite_counts[suite] = suite_counts.get(suite, 0) + 1
        seen_suites = set(suite_counts)
        missing = sorted(set(EXPECTED_SUITES) - seen_suites)
        if missing:
            failures.append((root, [f"missing required NFS smoke suites: {', '.join(missing)}"]))
        duplicates = sorted(
            suite for suite, count in suite_counts.items() if count > 1
        )
        if duplicates:
            failures.append(
                (
                    root,
                    [
                        "duplicate required NFS smoke suites: "
                        + ", ".join(
                            f"{suite} ({suite_counts[suite]} reports)"
                            for suite in duplicates
                        )
                    ],
                )
            )

    if failures:
        if emit_output:
            print(f"error: invalid NFS smoke report directory {root}:", file=sys.stderr)
            for report_path, errors in failures:
                print(f"  {report_path}:", file=sys.stderr)
                for error in errors:
                    print(f"    - {error}", file=sys.stderr)
        return 1

    if summary_output is not None:
        write_verification_summary(summary_output, root, valid_reports)

    if emit_output:
        suites = ", ".join(
            sorted({str(report.get("suite", "")) for _path, report in valid_reports})
        )
        print(
            f"ok: verified {len(valid_reports)} NFS smoke report(s) under {root}: {suites}"
        )
    return 0


def write_json(path: Path, value: dict[str, Any] | list[Any]) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def load_native_read_artifact(
    report_path: Path,
    report: dict[str, Any],
    errors: list[str],
) -> tuple[dict[str, Any] | None, Path | None]:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        errors.append(f"{report_path}: artifacts must be an object")
        return None, None

    value = artifacts.get("native_read_benchmark")
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{report_path}: artifacts.native_read_benchmark must be a non-empty path")
        return None, None

    artifact_path = resolve_artifact_path(value, report_path.parent)
    if artifact_path is None:
        errors.append(f"{report_path}: artifacts.native_read_benchmark does not exist: {value}")
        return None, None

    try:
        payload = json.loads(artifact_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"{artifact_path}: invalid JSON: {error}")
        return None, artifact_path
    if not isinstance(payload, dict):
        errors.append(f"{artifact_path}: root must be an object")
        return None, artifact_path
    return payload, artifact_path


def load_mount_doctor_artifact(
    report_path: Path,
    report: dict[str, Any],
    errors: list[str],
) -> tuple[dict[str, Any] | None, Path | None]:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        errors.append(f"{report_path}: artifacts must be an object")
        return None, None

    value = artifacts.get("mount_doctor")
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{report_path}: artifacts.mount_doctor must be a non-empty path")
        return None, None

    artifact_path = resolve_artifact_path(value, report_path.parent)
    if artifact_path is None:
        errors.append(f"{report_path}: artifacts.mount_doctor does not exist: {value}")
        return None, None

    try:
        payload = json.loads(artifact_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        errors.append(f"{artifact_path}: invalid JSON: {error}")
        return None, artifact_path
    if not isinstance(payload, dict):
        errors.append(f"{artifact_path}: root must be an object")
        return None, artifact_path
    return payload, artifact_path


def percent_regression(
    baseline: float,
    current: float,
    *,
    higher_is_better: bool,
) -> float:
    if baseline <= 0:
        if higher_is_better:
            if current >= baseline:
                return 0.0
            return float("inf")
        if current <= baseline:
            return 0.0
        return float("inf")
    if higher_is_better:
        return max(0.0, ((baseline - current) / baseline) * 100.0)
    return max(0.0, ((current - baseline) / baseline) * 100.0)


def native_read_metric(payload: dict[str, Any], metric: str) -> float:
    if metric == "mib_per_sec":
        return float(payload["mib_per_sec"])
    if metric in payload["efficiency"]:
        return float(payload["efficiency"][metric])

    bytes_returned = float(payload["bytes_returned"])
    user_mib = bytes_returned / (1024 * 1024)
    if metric == "vfs_read_calls_per_mib":
        return float(payload["nfs_vfs_delta"]["read_at_calls"]) / user_mib
    if metric == "vfs_returned_bytes_per_user_byte":
        return float(payload["nfs_vfs_delta"]["returned_bytes"]) / bytes_returned
    if metric == "resolver_calls_avoided_per_mib":
        return float(payload["nfs_vfs_delta"]["resolver_calls_avoided"]) / user_mib
    if metric == "read_lease_hits_per_mib":
        return float(payload["nfs_read_leases_delta"]["hits"]) / user_mib
    if metric == "read_lease_misses_per_mib":
        return float(payload["nfs_read_leases_delta"]["misses"]) / user_mib
    if metric == "hydration_remote_bytes_per_user_byte":
        return (
            float(payload["nfs_hydration_delta"]["read_window_remote_bytes"])
            / bytes_returned
        )
    if metric == "hydration_cache_hits_per_mib":
        return float(payload["nfs_hydration_delta"]["read_window_cache_hits"]) / user_mib
    if metric == "hydration_prefetch_requests_per_mib":
        return (
            float(payload["nfs_hydration_delta"]["read_window_prefetch_requests"])
            / user_mib
        )
    raise KeyError(metric)


def native_read_workload_summary(payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "scenario": payload.get("scenario"),
        "file_size": payload.get("file_size"),
        "read_size": payload.get("read_size"),
        "reads": payload.get("reads"),
        "bytes_returned": payload.get("bytes_returned"),
    }


def native_read_metrics_summary(payload: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {
        metric: {
            "label": label,
            "direction": "higher_is_better" if higher_is_better else "lower_is_better",
            "value": native_read_metric(payload, metric),
        }
        for metric, label, higher_is_better in NATIVE_READ_TREND_METRICS
    }


def native_read_summary_for_report(
    report_path: Path,
    report: dict[str, Any],
) -> dict[str, Any]:
    errors: list[str] = []
    native_read, artifact_path = load_native_read_artifact(report_path, report, errors)
    if native_read is None:
        return {
            "artifact": str(artifact_path) if artifact_path is not None else None,
            "errors": errors,
        }

    return {
        "artifact": str(artifact_path),
        "workload": native_read_workload_summary(native_read),
        "metrics": native_read_metrics_summary(native_read),
        "deltas": {
            "protocol": native_read.get("nfs_protocol_delta"),
            "read_leases": native_read.get("nfs_read_leases_delta"),
            "vfs": native_read.get("nfs_vfs_delta"),
            "hydration": native_read.get("nfs_hydration_delta"),
        },
    }


def mount_doctor_summary_for_report(
    report_path: Path,
    report: dict[str, Any],
) -> dict[str, Any]:
    errors: list[str] = []
    doctor, artifact_path = load_mount_doctor_artifact(report_path, report, errors)
    if doctor is None:
        return {
            "artifact": str(artifact_path) if artifact_path is not None else None,
            "errors": errors,
        }

    summary = doctor.get("summary") if isinstance(doctor.get("summary"), dict) else {}
    preflight = (
        doctor.get("nfs_preflight")
        if isinstance(doctor.get("nfs_preflight"), dict)
        else {}
    )
    return {
        "artifact": str(artifact_path),
        "requested_backend": doctor.get("requested_backend"),
        "checked_backend": doctor.get("checked_backend"),
        "ready": summary.get("ready"),
        "ok": summary.get("ok"),
        "warn": summary.get("warn"),
        "fail": summary.get("fail"),
        "nfs_preflight_ready": preflight.get("ready"),
        "nfs_preflight_blockers": preflight.get("blocker_count"),
        "nfs_preflight_warnings": preflight.get("warning_count"),
        "nfs_preflight_next_action": preflight.get("next_action"),
    }


def compare_native_read_reports(
    baseline_report_path: Path,
    current_report_path: Path,
    *,
    thresholds: dict[str, float],
    output: Path | None,
    emit_output: bool = True,
) -> int:
    errors: list[str] = []
    baseline_report, baseline_errors = verify_report_file(
        baseline_report_path,
        expected_suite=None,
        expected_platform=None,
        expected_run_suffix=None,
        require_artifacts=True,
        thresholds={},
        artifact_base=baseline_report_path.parent,
    )
    current_report, current_errors = verify_report_file(
        current_report_path,
        expected_suite=None,
        expected_platform=None,
        expected_run_suffix=None,
        require_artifacts=True,
        thresholds={},
        artifact_base=current_report_path.parent,
    )
    errors.extend(f"{baseline_report_path}: {error}" for error in baseline_errors)
    errors.extend(f"{current_report_path}: {error}" for error in current_errors)
    if baseline_report is None or current_report is None or errors:
        if emit_output:
            print("error: invalid NFS smoke report comparison:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
        return 1

    for field in ("suite", "platform"):
        if baseline_report.get(field) != current_report.get(field):
            errors.append(
                f"{field} mismatch: baseline={baseline_report.get(field)!r}, "
                f"current={current_report.get(field)!r}"
            )

    baseline_native, baseline_artifact = load_native_read_artifact(
        baseline_report_path,
        baseline_report,
        errors,
    )
    current_native, current_artifact = load_native_read_artifact(
        current_report_path,
        current_report,
        errors,
    )
    if baseline_native is None or current_native is None or errors:
        if emit_output:
            print("error: invalid NFS smoke report comparison:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
        return 1

    for field in ("suite", "scenario", "file_size", "read_size", "reads", "bytes_returned"):
        if baseline_native.get(field) != current_native.get(field):
            errors.append(
                f"native read workload mismatch for {field}: "
                f"baseline={baseline_native.get(field)!r}, current={current_native.get(field)!r}"
            )

    metrics: dict[str, dict[str, Any]] = {}
    for metric, label, higher_is_better in NATIVE_READ_TREND_METRICS:
        baseline_value = native_read_metric(baseline_native, metric)
        current_value = native_read_metric(current_native, metric)
        regression_pct = percent_regression(
            baseline_value,
            current_value,
            higher_is_better=higher_is_better,
        )
        threshold = thresholds.get(metric)
        metrics[metric] = {
            "label": label,
            "direction": "higher_is_better" if higher_is_better else "lower_is_better",
            "baseline": baseline_value,
            "current": current_value,
            "regression_pct": regression_pct,
            "threshold_pct": threshold,
        }
        if threshold is not None and regression_pct > threshold:
            errors.append(
                f"native read {label} regressed by {regression_pct:.2f}% "
                f"over the configured {threshold:.2f}% threshold"
            )

    if errors:
        if emit_output:
            print("error: invalid NFS smoke report comparison:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
        return 1

    comparison = {
        "schema_version": 1,
        "suite": "nfs-smoke-report-comparison",
        "status": "ok",
        "baseline_report": str(baseline_report_path),
        "current_report": str(current_report_path),
        "baseline_artifact": str(baseline_artifact),
        "current_artifact": str(current_artifact),
        "smoke_suite": baseline_report.get("suite"),
        "platform": baseline_report.get("platform"),
        "baseline_run_id": baseline_report.get("run_id"),
        "current_run_id": current_report.get("run_id"),
        "native_read": {
            "workload": {
                "scenario": baseline_native.get("scenario"),
                "file_size": baseline_native.get("file_size"),
                "read_size": baseline_native.get("read_size"),
                "reads": baseline_native.get("reads"),
                "bytes_returned": baseline_native.get("bytes_returned"),
            },
            "metrics": metrics,
        },
    }
    if output is not None:
        write_json(output, comparison)

    if emit_output:
        print(
            "ok: NFS smoke report comparison passed: "
            f"{baseline_report_path} -> {current_report_path}"
        )
        if output is not None:
            print(f"nfs_smoke_report_comparison={output}")
    return 0


def retained_report_paths_by_suite(root: Path) -> tuple[dict[str, Path], list[str]]:
    errors: list[str] = []
    if not root.is_dir():
        return {}, [f"NFS smoke report directory does not exist: {root}"]

    report_paths = sorted(root.rglob("nfs-smoke-report.json"))
    if not report_paths:
        return {}, [f"no nfs-smoke-report.json files found under {root}"]

    reports: dict[str, Path] = {}
    valid_reports: list[tuple[Path, dict[str, Any]]] = []
    for report_path in report_paths:
        report, report_errors = verify_report_file(
            report_path,
            expected_suite=None,
            expected_platform=None,
            expected_run_suffix=None,
            expected_git_commit=None,
            require_artifacts=True,
            thresholds={},
            artifact_base=report_path.parent,
        )
        errors.extend(f"{report_path}: {error}" for error in report_errors)
        if report is None or report_errors:
            continue
        valid_reports.append((report_path, report))

        suite = report.get("suite")
        if not isinstance(suite, str) or not suite:
            errors.append(f"{report_path}: suite must be a non-empty string")
            continue
        if suite in reports:
            errors.append(
                f"duplicate retained NFS smoke report for suite {suite}: "
                f"{reports[suite]} and {report_path}"
            )
            continue
        reports[suite] = report_path

    for _path, consistency_errors in check_retained_directory_consistency(
        root,
        valid_reports,
    ):
        errors.extend(consistency_errors)

    return reports, errors


def compare_directory(
    baseline_root: Path,
    current_root: Path,
    *,
    require_all_platforms: bool,
    thresholds: dict[str, float],
    summary_output: Path | None,
    emit_output: bool = True,
) -> int:
    baseline_reports, baseline_errors = retained_report_paths_by_suite(baseline_root)
    current_reports, current_errors = retained_report_paths_by_suite(current_root)
    errors = [f"baseline: {error}" for error in baseline_errors]
    errors.extend(f"current: {error}" for error in current_errors)

    if require_all_platforms:
        for label, reports in (("baseline", baseline_reports), ("current", current_reports)):
            missing = sorted(set(EXPECTED_SUITES) - set(reports))
            if missing:
                errors.append(
                    f"{label}: missing required NFS smoke suites: {', '.join(missing)}"
                )

    missing_baselines = sorted(set(current_reports) - set(baseline_reports))
    if missing_baselines:
        errors.append(
            "baseline: missing suite(s) present in current evidence: "
            + ", ".join(missing_baselines)
        )

    if errors:
        if emit_output:
            print("error: invalid NFS smoke report directory comparison:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
        return 1

    comparisons: list[dict[str, Any]] = []
    failures: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        comparison_root = Path(tmp)
        for suite in sorted(current_reports):
            comparison_path = comparison_root / f"{suite}.json"
            status = compare_native_read_reports(
                baseline_reports[suite],
                current_reports[suite],
                thresholds=thresholds,
                output=comparison_path,
                emit_output=False,
            )
            if status != 0:
                if emit_output:
                    compare_native_read_reports(
                        baseline_reports[suite],
                        current_reports[suite],
                        thresholds=thresholds,
                        output=None,
                        emit_output=True,
                    )
                failures.append(suite)
                continue
            comparisons.append(load_report(comparison_path))

    if failures:
        if emit_output:
            print("error: invalid NFS smoke report directory comparison:", file=sys.stderr)
            for suite in failures:
                print(f"  - comparison failed for suite {suite}", file=sys.stderr)
        return 1

    summary = {
        "schema_version": 1,
        "suite": "nfs-smoke-report-directory-comparison",
        "status": "ok",
        "baseline_root": str(baseline_root),
        "current_root": str(current_root),
        "comparison_count": len(comparisons),
        "suites": [comparison["smoke_suite"] for comparison in comparisons],
        "platforms": [comparison["platform"] for comparison in comparisons],
        "comparisons": comparisons,
    }
    if summary_output is not None:
        write_json(summary_output, summary)

    if emit_output:
        suites = ", ".join(str(suite) for suite in summary["suites"])
        print(
            f"ok: compared {len(comparisons)} retained NFS smoke report(s): {suites}"
        )
        if summary_output is not None:
            print(f"nfs_smoke_report_directory_comparison={summary_output}")
    return 0


def build_self_test_report(root: Path) -> tuple[dict[str, Any], Path]:
    mount_list = root / "mount-list.json"
    mount_doctor = root / "mount-doctor.json"
    mount_status = root / "mount-status.json"
    control_status = root / "control-status.json"
    native_read = root / "native-read-benchmark.json"
    writeback_check = root / "writeback-check.json"
    unmount_check = root / "unmount-check.json"
    control_shutdown = root / "control-shutdown.json"
    remount_check = root / "remount-check.json"
    native_vfs_before = {
        "open_read_calls": 2,
        "read_at_calls": 10,
        "returned_bytes": 1_000,
        "source_cache_hits": 0,
        "resolver_calls_avoided": 0,
        "source_cache_misses": 1,
        "source_cache_evictions": 0,
        "source_cache_invalidations": 0,
        "source_cache_stale_evictions": 0,
        "stale_generation_rejections": 0,
        "stale_overlay_view_rejections": 0,
        "stale_overlay_file_rejections": 0,
        "base_pointer_reads": 10,
        "base_pointer_bytes": 1_000,
        "base_blob_reads": 0,
        "base_blob_bytes": 0,
        "base_empty_reads": 0,
        "base_empty_bytes": 0,
        "overlay_file_reads": 0,
        "overlay_file_bytes": 0,
        "adaptive_first": 1,
        "adaptive_sequential": 8,
        "adaptive_strided": 0,
        "adaptive_repeated": 1,
        "adaptive_random": 0,
    }
    native_vfs_delta = {
        "open_read_calls": 1,
        "read_at_calls": 32,
        "returned_bytes": 8_388_608,
        "source_cache_hits": 1,
        "resolver_calls_avoided": 1,
        "source_cache_misses": 1,
        "source_cache_evictions": 0,
        "source_cache_invalidations": 0,
        "source_cache_stale_evictions": 0,
        "stale_generation_rejections": 0,
        "stale_overlay_view_rejections": 0,
        "stale_overlay_file_rejections": 0,
        "base_pointer_reads": 32,
        "base_pointer_bytes": 8_388_608,
        "base_blob_reads": 0,
        "base_blob_bytes": 0,
        "base_empty_reads": 0,
        "base_empty_bytes": 0,
        "overlay_file_reads": 0,
        "overlay_file_bytes": 0,
        "adaptive_first": 1,
        "adaptive_sequential": 30,
        "adaptive_strided": 0,
        "adaptive_repeated": 1,
        "adaptive_random": 0,
    }
    native_vfs_after = {
        key: native_vfs_before[key] + native_vfs_delta[key]
        for key in NATIVE_VFS_COUNTERS
    }
    native_hydration_before = {
        "read_range_requests": 10,
        "read_range_requested_bytes": 1_000,
        "read_range_returned_bytes": 1_000,
        "read_window_cache_hits": 0,
        "read_window_cache_misses": 1,
        "read_window_inflight_waits": 0,
        "read_window_remote_fetches": 1,
        "read_window_remote_bytes": 1_000,
        "read_window_prefetch_requests": 0,
        "read_window_prefetch_scheduled": 0,
        "read_window_prefetch_skipped": 0,
        "read_window_prefetch_errors": 0,
        "chunk_cache_hits": 0,
        "chunk_cache_misses": 1,
        "chunk_inflight_waits": 0,
        "chunk_remote_fetches": 1,
        "chunk_remote_bytes": 1_000,
    }
    native_hydration_delta = {
        "read_range_requests": 32,
        "read_range_requested_bytes": 9_388_608,
        "read_range_returned_bytes": 8_388_608,
        "read_window_cache_hits": 2,
        "read_window_cache_misses": 1,
        "read_window_inflight_waits": 0,
        "read_window_remote_fetches": 1,
        "read_window_remote_bytes": 8_388_608,
        "read_window_prefetch_requests": 1,
        "read_window_prefetch_scheduled": 1,
        "read_window_prefetch_skipped": 0,
        "read_window_prefetch_errors": 0,
        "chunk_cache_hits": 16,
        "chunk_cache_misses": 2,
        "chunk_inflight_waits": 0,
        "chunk_remote_fetches": 2,
        "chunk_remote_bytes": 8_388_608,
    }
    native_hydration_after = {
        key: native_hydration_before[key] + native_hydration_delta[key]
        for key in NATIVE_HYDRATION_COUNTERS
    }
    native_read_leases_before = {
        "temporary_overflows": 0,
        "hits": 1,
        "misses": 1,
        "evictions": 0,
        "stale_retries": 0,
    }
    native_read_leases_delta = {
        "temporary_overflows": 0,
        "hits": 31,
        "misses": 1,
        "evictions": 0,
        "stale_retries": 0,
    }
    native_read_leases_after = {
        key: native_read_leases_before[key] + native_read_leases_delta[key]
        for key in NATIVE_READ_LEASE_COUNTERS
    }

    write_json(
        mount_list,
        [
            {
                "name": "self-test",
                "backend": "nfs",
                "mountpoint": "/mnt/crab",
                "source": "/tmp/crab-source",
                "ref": "main",
                "state": "running",
                "pid": 123,
                "uptime": "< 1m",
                "read_only": False,
                "start_time": "2026-01-01T00:00:00Z",
                "log_path": str(root / "crab-nfs.log"),
                "control_endpoint": "unix:/tmp/crab-nfs.sock",
            }
        ],
    )
    write_json(
        mount_doctor,
        {
            "requested_backend": "nfs",
            "checked_backend": "nfs",
            "mountpoint": "/mnt/crab",
            "checks": [
                {
                    "name": "nfs feature",
                    "status": "ok",
                    "detail": "NFS support is compiled into this Crab build",
                },
                {
                    "name": "nfs helper",
                    "status": "ok",
                    "detail": "crab-nfs-mount found",
                },
                {
                    "name": "nfs helper version",
                    "status": "ok",
                    "detail": "crab-nfs-mount reports Crab self-test",
                },
                {
                    "name": "nfs helper layout",
                    "status": "ok",
                    "detail": "crab-nfs-mount is installed next to crab",
                },
                {
                    "name": "nfs preflight",
                    "status": "ok",
                    "detail": "native client, mountpoint, loopback, control endpoint, and privilege checks passed for /mnt/crab",
                },
            ],
            "summary": {
                "ok": 5,
                "warn": 0,
                "fail": 0,
                "ready": True,
            },
            "nfs_preflight": {
                "ready": True,
                "backend_available": True,
                "native_client_available": True,
                "mountpoint_ready": True,
                "loopback_bind_ready": True,
                "control_endpoint_ready": True,
                "privilege_ready": True,
                "blocker_count": 0,
                "warning_count": 0,
                "blockers": [],
                "warnings": [],
            },
        },
    )
    write_json(
        mount_status,
        {
            "mountpoint": "/mnt/crab",
            "backend": "nfs",
            "source": "/tmp/crab-source",
            "ref": "main",
            "state": "running",
            "pid": 123,
            "read_only": False,
            "log_path": str(root / "crab-nfs.log"),
            "control_endpoint": "unix:/tmp/crab-nfs.sock",
            "nfs_runtime": {
                "lifecycle": {
                    "server_bind_ms": 1,
                    "native_mount_ms": 1,
                    "startup_ms": 2,
                },
                "protocol": {
                    "read_rpcs": 42,
                    "read_requested_bytes": 9_388_608,
                    "read_returned_bytes": 8_388_608,
                    "read_size_le_4k": 1,
                    "read_size_le_64k": 8,
                    "read_size_le_1m": 33,
                    "read_size_gt_1m": 0,
                    "readdirplus_rpcs": 2,
                    "readdirplus_entries": 8,
                    "readdirplus_materialized_entries": 8,
                    "readdirplus_returned_candidates": 6,
                    "readdirplus_attr_resolutions": 6,
                    "readdirplus_prefetch_paths": 4,
                    "readdirplus_cookie_resumes": 1,
                    "readdirplus_cookie_misses": 0,
                    "readdirplus_skipped_entries": 2,
                    "readdirplus_large_dirs": 0,
                    "readdirplus_prefetch_errors": 0,
                },
                "read_leases": {
                    "entries": 2,
                    "max_entries": 256,
                    "estimated_bytes": 4096,
                    "max_estimated_bytes": 16_777_216,
                    "pinned_entries": 0,
                    "active_pins": 0,
                    "temporary_overflows": 0,
                    "hits": 40,
                    "misses": 2,
                    "evictions": 0,
                    "stale_retries": 0,
                },
                "directory_pages": {
                    "entries": 0,
                    "max_entries": 256,
                    "estimated_bytes": 0,
                    "max_estimated_bytes": 16_777_216,
                    "hits": 0,
                    "misses": 0,
                    "evictions": 0,
                    "stale_evictions": 0,
                },
                "vfs": {
                    "open_read_calls": 2,
                    "read_at_calls": 42,
                    "returned_bytes": 8_388_608,
                    "stale_generation_rejections": 0,
                    "stale_overlay_view_rejections": 0,
                    "stale_overlay_file_rejections": 0,
                    "source_cache_entries": 1,
                    "source_cache_max_entries": 256,
                    "source_cache_estimated_bytes": 4096,
                    "source_cache_max_estimated_bytes": 16_777_216,
                    "source_cache_hits": 1,
                    "resolver_calls_avoided": 1,
                    "source_cache_misses": 1,
                    "source_cache_evictions": 0,
                    "source_cache_invalidations": 0,
                    "source_cache_stale_evictions": 0,
                    "invalidation_path_events": 0,
                    "invalidation_subtree_events": 0,
                    "invalidation_rename_events": 0,
                    "invalidation_generation_events": 0,
                    "invalidation_overlay_reset_events": 0,
                    "invalidation_compacted_full_resets": 0,
                    "base_pointer": {
                        "reads": 42,
                        "bytes": 8_388_608,
                        "adaptive": {
                            "first": 1,
                            "sequential": 40,
                            "strided": 0,
                            "repeated": 1,
                            "random": 0,
                        },
                    },
                    "base_blob": {
                        "reads": 0,
                        "bytes": 0,
                        "adaptive": {
                            "first": 0,
                            "sequential": 0,
                            "strided": 0,
                            "repeated": 0,
                            "random": 0,
                        },
                    },
                    "base_empty": {
                        "reads": 0,
                        "bytes": 0,
                        "adaptive": {
                            "first": 0,
                            "sequential": 0,
                            "strided": 0,
                            "repeated": 0,
                            "random": 0,
                        },
                    },
                    "overlay_file": {
                        "reads": 0,
                        "bytes": 0,
                        "adaptive": {
                            "first": 0,
                            "sequential": 0,
                            "strided": 0,
                            "repeated": 0,
                            "random": 0,
                        },
                    },
                },
                "hydration": {
                    "read_range_requests": 42,
                    "read_range_requested_bytes": 9_388_608,
                    "read_range_returned_bytes": 8_388_608,
                    "read_window_cache_hits": 2,
                    "read_window_cache_misses": 1,
                    "read_window_inflight_waits": 0,
                    "read_window_remote_fetches": 1,
                    "read_window_remote_bytes": 8_388_608,
                    "read_window_prefetch_requests": 1,
                    "read_window_prefetch_scheduled": 1,
                    "read_window_prefetch_skipped": 0,
                    "read_window_prefetch_errors": 0,
                    "chunk_cache_hits": 16,
                    "chunk_cache_misses": 2,
                    "chunk_inflight_waits": 0,
                    "chunk_remote_fetches": 2,
                    "chunk_remote_bytes": 8_388_608,
                },
                "write_journal": {
                    "pending_paths": 0,
                    "oldest_dirty_age_secs": None,
                    "paths_with_sync_errors": 0,
                    "sync_attempts": 1,
                    "sync_successes": 1,
                    "sync_failures": 0,
                    "total_sync_latency_ms": 0,
                    "last_sync_latency_ms": 0,
                    "max_sync_latency_ms": 0,
                    "poisoned": False,
                    "entries": [],
                },
            },
        },
    )
    write_json(
        native_read,
        {
            "schema_version": 1,
            "suite": "nfs-native-read-benchmark",
            "scenario": "native_sequential_read",
            "path": "/mnt/crab/native-read.bin",
            "mountpoint": "/mnt/crab",
            "file_size": 4_194_304,
            "read_size": 262_144,
            "reads": 32,
            "bytes_returned": 8_388_608,
            "elapsed_ms": 5,
            "mib_per_sec": 100.0,
            "sha256": "0" * 64,
            "nfs_protocol_before": {
                "read_rpcs": 10,
                "read_requested_bytes": 1_000,
                "read_returned_bytes": 1_000,
            },
            "nfs_protocol_after": {
                "read_rpcs": 42,
                "read_requested_bytes": 9_389_608,
                "read_returned_bytes": 8_389_608,
            },
            "nfs_protocol_delta": {
                "read_rpcs": 32,
                "read_requested_bytes": 9_388_608,
                "read_returned_bytes": 8_388_608,
            },
            "nfs_read_leases_before": native_read_leases_before,
            "nfs_read_leases_after": native_read_leases_after,
            "nfs_read_leases_delta": native_read_leases_delta,
            "nfs_vfs_before": native_vfs_before,
            "nfs_vfs_after": native_vfs_after,
            "nfs_vfs_delta": native_vfs_delta,
            "nfs_hydration_before": native_hydration_before,
            "nfs_hydration_after": native_hydration_after,
            "nfs_hydration_delta": native_hydration_delta,
            "efficiency": {
                "requested_bytes_per_user_byte": 9_388_608 / 8_388_608,
                "returned_bytes_per_user_byte": 1.0,
                "read_rpcs_per_mib": 4.0,
            },
        },
    )
    write_json(
        writeback_check,
        {
            "schema_version": 1,
            "action": "writeback",
            "mountpoint": "/mnt/crab",
            "source": "/tmp/crab-source",
            "state": "running",
            "pid": 123,
            "control_endpoint": "unix:/tmp/crab-nfs.sock",
            "log_path": str(root / "crab-nfs.log"),
            "content_checks": {
                "hello_appended": True,
                "renamed_file_created": True,
                "exclusive_file_created": True,
                "symlink_created": True,
                "gitdir_preserved": True,
                "gitdir_overwrite_rejected": True,
                "gitdir_rename_rejected": True,
                "removed_directory_absent": True,
            },
        },
    )
    write_json(
        unmount_check,
        {
            "schema_version": 1,
            "action": "control_shutdown",
            "mountpoint": "/mnt/crab",
            "source": "/tmp/crab-source",
            "pid": 123,
            "control_endpoint": "unix:/tmp/crab-nfs.sock",
            "log_path": str(root / "crab-nfs.log"),
            "mounted_after": False,
        },
    )
    control_status.write_text(mount_status.read_text(encoding="utf-8"), encoding="utf-8")
    control_shutdown.write_text(
        unmount_check.read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    write_json(
        remount_check,
        {
            "schema_version": 1,
            "action": "remount",
            "mountpoint": "/mnt/crab",
            "source": "/tmp/crab-source",
            "state": "running",
            "pid": 456,
            "control_endpoint": "unix:/tmp/crab-nfs-remount.sock",
            "log_path": str(root / "crab-nfs-remount.log"),
            "mounted_after": True,
            "content_checks": {
                "hello_preserved": True,
                "renamed_file_preserved": True,
                "exclusive_file_preserved": True,
                "symlink_preserved": True,
                "gitdir_preserved": True,
                "removed_directory_absent": True,
            },
        },
    )

    return (
        {
            "schema_version": 1,
            "status": "ok",
            "backend": "nfs",
            "suite": "mount-nfs-linux",
            "platform": "linux",
            "run_id": "self-test",
            "git_commit": "0123456789abcdef0123456789abcdef01234567",
            "artifact_root": str(root),
            "crab_version": "crab self-test",
            "helper_version": "crab self-test",
            "checks": [
                "build",
                "helper_version",
                "mount_doctor",
                "initial_read",
                "native_read_benchmark",
                "writeback",
                "mount_list",
                "mount_status",
                "control_status",
                "unmount",
                "control_shutdown",
                "remount",
            ],
            "artifacts": {
                "mount_list": str(mount_list),
                "mount_doctor": str(mount_doctor),
                "mount_status": str(mount_status),
                "control_status": str(control_status),
                "native_read_benchmark": str(native_read),
                "writeback_check": str(writeback_check),
                "unmount_check": str(unmount_check),
                "control_shutdown": str(control_shutdown),
                "remount_check": str(remount_check),
            },
        },
        native_read,
    )


def expect_self_test_error(errors: list[str], expected: str) -> bool:
    return any(expected in error for error in errors)


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            expected_git_commit="0123456789abcdef0123456789abcdef01234567",
            artifact_base=Path(tmp),
        )
        if errors:
            print("error: self-test valid report failed:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        del report["artifacts"]["control_status"]
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "artifacts.control_status must be a non-empty path"):
            print("error: self-test missing control-status artifact was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "must redact TCP control token"):
            print("error: self-test raw retained TCP control token was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        payload["diagnostics"] = {
            "last_error": "failed to call tcp:127.0.0.1:58123?token=secret-token"
        }
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "mount_status.diagnostics.last_error must redact TCP control token",
        ):
            print(
                "error: self-test raw diagnostic TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        writeback_check = Path(report["artifacts"]["writeback_check"])
        payload = json.loads(writeback_check.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(writeback_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "writeback_check.control_endpoint must redact TCP control token"):
            print(
                "error: self-test raw writeback TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        unmount_check = Path(report["artifacts"]["unmount_check"])
        payload = json.loads(unmount_check.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(unmount_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "unmount_check.control_endpoint must redact TCP control token"):
            print(
                "error: self-test raw unmount TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        control_shutdown = Path(report["artifacts"]["control_shutdown"])
        payload = json.loads(control_shutdown.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(control_shutdown, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "control_shutdown.control_endpoint must redact TCP control token",
        ):
            print(
                "error: self-test raw control-shutdown TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        control_status = Path(report["artifacts"]["control_status"])
        payload = json.loads(control_status.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(control_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "control_status.control_endpoint must redact TCP control token",
        ):
            print(
                "error: self-test raw control-status TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        remount_check = Path(report["artifacts"]["remount_check"])
        payload = json.loads(remount_check.read_text(encoding="utf-8"))
        payload["control_endpoint"] = "tcp:127.0.0.1:58123?token=secret-token"
        write_json(remount_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "remount_check.control_endpoint must redact TCP control token",
        ):
            print(
                "error: self-test raw remount TCP control token was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        control_status = Path(report["artifacts"]["control_status"])
        payload = json.loads(control_status.read_text(encoding="utf-8"))
        payload["pid"] = 999
        write_json(control_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "control_status.pid must match"):
            print("error: self-test mismatched control-status artifact was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        control_status = Path(report["artifacts"]["control_status"])
        payload = json.loads(control_status.read_text(encoding="utf-8"))
        payload["nfs_runtime"]["protocol"]["read_rpcs"] += 1
        write_json(control_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "control_status.nfs_runtime must match"):
            print(
                "error: self-test mismatched control-status runtime was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            expected_git_commit="fedcba9876543210fedcba9876543210fedcba98",
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "expected git_commit"):
            print("error: self-test mismatched git commit was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            expected_git_commit="not-a-full-commit",
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "--expected-git-commit must be"):
            print("error: self-test malformed expected git commit was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        report["helper_version"] = "crab different-helper-version"
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "helper_version must match crab_version"):
            print("error: self-test mismatched helper version was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        payload["nfs_preflight"]["ready"] = False
        payload["nfs_preflight"]["blocker_count"] = 1
        payload["nfs_preflight"]["blockers"] = [
            {
                "key": "mount.nfs not found",
                "detail": "native NFS client is unavailable",
                "action": "Install nfs-common.",
            }
        ]
        payload["summary"]["ready"] = False
        payload["summary"]["fail"] = 1
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "mount_doctor.nfs_preflight.ready must be true"):
            print("error: self-test failed mount doctor preflight was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        payload["checks"] = [
            check
            for check in payload["checks"]
            if check.get("name") != "nfs helper layout"
        ]
        payload["summary"]["ok"] = 4
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "checks must include nfs helper layout"):
            print("error: self-test missing mount doctor helper layout was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        for check in payload["checks"]:
            if check.get("name") == "nfs helper version":
                check["status"] = "warn"
                break
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "checks entry nfs helper version must be ok"):
            print("error: self-test degraded mount doctor helper version was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        payload["summary"]["ok"] = 4
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "summary.ok must match check statuses"):
            print("error: self-test stale mount doctor summary was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        payload["nfs_preflight"]["warning_count"] = 1
        payload["nfs_preflight"]["warnings"] = []
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "warning_count must match warnings"):
            print("error: self-test stale mount doctor warning count was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_doctor = Path(report["artifacts"]["mount_doctor"])
        payload = json.loads(mount_doctor.read_text(encoding="utf-8"))
        payload["mountpoint"] = "/mnt/other-crab"
        write_json(mount_doctor, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "mount_doctor.mountpoint must match"):
            print("error: self-test mismatched mount doctor mountpoint was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        write_json(Path(report["artifacts"]["mount_list"]), [])
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "mount_list must include a running nfs entry"):
            print("error: self-test missing mount-list NFS entry was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        payload["source"] = "/tmp/other-crab-source"
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "must match artifacts.mount_list entry"):
            print("error: self-test mount-status/list mismatch was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        payload["pid"] = 456
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "mount_status.pid must match"):
            print("error: self-test mount-status/list PID mismatch was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        payload["nfs_runtime"]["read_leases"]["hits"] = 0
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "read_leases.hits must be positive"):
            print("error: self-test missing read lease hit evidence was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        del payload["nfs_runtime"]["protocol"]["readdirplus_materialized_entries"]
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "protocol.readdirplus_materialized_entries",
        ):
            print(
                "error: self-test missing READDIRPLUS runtime counter was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        writeback_check = Path(report["artifacts"]["writeback_check"])
        payload = json.loads(writeback_check.read_text(encoding="utf-8"))
        payload["content_checks"]["hello_appended"] = False
        write_json(writeback_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "writeback_check.content_checks.hello_appended"):
            print("error: self-test failed writeback content check was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        writeback_check = Path(report["artifacts"]["writeback_check"])
        payload = json.loads(writeback_check.read_text(encoding="utf-8"))
        payload["content_checks"]["gitdir_rename_rejected"] = False
        write_json(writeback_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "writeback_check.content_checks.gitdir_rename_rejected",
        ):
            print(
                "error: self-test failed .git rename rejection check was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        writeback_check = Path(report["artifacts"]["writeback_check"])
        payload = json.loads(writeback_check.read_text(encoding="utf-8"))
        payload["content_checks"]["symlink_created"] = False
        write_json(writeback_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "writeback_check.content_checks.symlink_created",
        ):
            print(
                "error: self-test failed Unix symlink writeback check was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        unmount_check = Path(report["artifacts"]["unmount_check"])
        payload = json.loads(unmount_check.read_text(encoding="utf-8"))
        payload["mounted_after"] = True
        write_json(unmount_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "unmount_check.mounted_after must be false"):
            print("error: self-test mounted-after shutdown artifact was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        remount_check = Path(report["artifacts"]["remount_check"])
        payload = json.loads(remount_check.read_text(encoding="utf-8"))
        payload["content_checks"]["hello_preserved"] = False
        write_json(remount_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "remount_check.content_checks.hello_preserved"):
            print("error: self-test failed remount content check was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        remount_check = Path(report["artifacts"]["remount_check"])
        payload = json.loads(remount_check.read_text(encoding="utf-8"))
        payload["content_checks"]["symlink_preserved"] = False
        write_json(remount_check, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "remount_check.content_checks.symlink_preserved",
        ):
            print(
                "error: self-test failed Unix symlink remount check was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        del payload["nfs_runtime"]["vfs"]
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "nfs_runtime.vfs must be an object"):
            print("error: self-test missing VFS runtime evidence was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        del payload["nfs_runtime"]["hydration"]
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "nfs_runtime.hydration must be an object"):
            print("error: self-test missing hydration runtime evidence was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        del payload["nfs_runtime"]["write_journal"]["pending_paths"]
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "write_journal.pending_paths"):
            print(
                "error: self-test missing write-journal pending count was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        mount_status = Path(report["artifacts"]["mount_status"])
        payload = json.loads(mount_status.read_text(encoding="utf-8"))
        write_journal = payload["nfs_runtime"]["write_journal"]
        write_journal["pending_paths"] = 1
        write_journal["oldest_dirty_age_secs"] = 3
        write_journal["paths_with_sync_errors"] = 0
        write_journal["entries"] = [
            {
                "path": "dirty.bin",
                "overlay_version": 2,
                "last_write_stability": "unstable",
                "dirty_age_secs": 3,
                "last_sync_error": "NFS3ERR_IO",
            }
        ]
        write_json(mount_status, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "paths_with_sync_errors must equal"):
            print(
                "error: self-test inconsistent write-journal sync-error count was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        report["checks"] = [
            check for check in report["checks"] if check != "native_read_benchmark"
        ]
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "missing required checks"):
            print("error: self-test missing native benchmark check was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["mountpoint"] = "/mnt/other-crab"
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "native_read_benchmark.mountpoint must match"):
            print("error: self-test mismatched native-read mountpoint was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["nfs_protocol_delta"]["read_rpcs"] = 31
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "must equal after-before"):
            print("error: self-test mismatched protocol delta was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        del payload["nfs_vfs_delta"]
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "nfs_vfs_delta must be an object"):
            print(
                "error: self-test missing native VFS delta was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["nfs_read_leases_delta"]["hits"] = 0
        payload["nfs_read_leases_after"]["hits"] = payload["nfs_read_leases_before"][
            "hits"
        ]
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "nfs_read_leases_delta.hits must be positive",
        ):
            print(
                "error: self-test missing native read lease hit delta was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["nfs_read_leases_delta"]["misses"] = 0
        payload["nfs_read_leases_after"]["misses"] = payload["nfs_read_leases_before"][
            "misses"
        ]
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(
            errors,
            "nfs_read_leases_delta.misses must be positive",
        ):
            print(
                "error: self-test missing native read lease miss delta was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["nfs_hydration_delta"]["read_range_requests"] -= 1
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "nfs_hydration_delta.read_range_requests must equal after-before"):
            print("error: self-test mismatched native hydration delta was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, native_read = build_self_test_report(Path(tmp))
        payload = json.loads(native_read.read_text(encoding="utf-8"))
        payload["efficiency"]["read_rpcs_per_mib"] = 1.0
        write_json(native_read, payload)
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "must match protocol delta"):
            print("error: self-test mismatched efficiency metric was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            native_read_thresholds={"max_read_rpcs_per_mib": 1.0},
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "exceeds the configured threshold"):
            print("error: self-test native read threshold violation was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            native_read_thresholds={"min_read_lease_hits_per_mib": 10.0},
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "read_lease_hits_per_mib is below"):
            print(
                "error: self-test native read lease-hit threshold violation was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        report, _native_read = build_self_test_report(Path(tmp))
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            native_read_thresholds={"max_read_lease_misses_per_mib": 0.1},
            artifact_base=Path(tmp),
        )
        if not expect_self_test_error(errors, "read_lease_misses_per_mib exceeds"):
            print(
                "error: self-test native read lease-miss threshold violation was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        report, _native_read = build_self_test_report(root)
        report["artifacts"] = {
            "mount_list": "/runner/original/mount-list.json",
            "mount_doctor": "/runner/original/mount-doctor.json",
            "mount_status": "/runner/original/mount-status.json",
            "control_status": "/runner/original/control-status.json",
            "native_read_benchmark": "/runner/original/native-read-benchmark.json",
            "writeback_check": "/runner/original/writeback-check.json",
            "unmount_check": "/runner/original/unmount-check.json",
            "control_shutdown": "/runner/original/control-shutdown.json",
            "remount_check": "/runner/original/remount-check.json",
        }
        errors = validate(
            report,
            expected_suite="mount-nfs-linux",
            expected_platform="linux",
            expected_run_suffix=None,
            require_artifacts=True,
            artifact_base=root,
        )
        if errors:
            print("error: self-test retained artifact fallback failed:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=root / "summary.json",
            emit_output=False,
        )
        if status != 0:
            print("error: self-test verify-dir valid report set failed", file=sys.stderr)
            return 1
        summary = json.loads((root / "summary.json").read_text(encoding="utf-8"))
        if summary.get("git_commit") != "0123456789abcdef0123456789abcdef01234567":
            print("error: self-test verify-dir summary omitted evidence git commit", file=sys.stderr)
            return 1
        if summary.get("run_id_suffix") != "12345-1":
            print("error: self-test verify-dir summary omitted run suffix", file=sys.stderr)
            return 1
        first_report = summary["reports"][0]
        if first_report.get("git_commit") != "0123456789abcdef0123456789abcdef01234567":
            print("error: self-test verify-dir summary omitted git commit", file=sys.stderr)
            return 1
        metrics = first_report["native_read"]["metrics"]
        if (
            "hydration_remote_bytes_per_user_byte" not in metrics
            or "read_lease_hits_per_mib" not in metrics
        ):
            print("error: self-test verify-dir summary omitted native read metrics", file=sys.stderr)
            return 1
        doctor = first_report.get("mount_doctor")
        if (
            not isinstance(doctor, dict)
            or doctor.get("ready") is not True
            or doctor.get("nfs_preflight_ready") is not True
        ):
            print("error: self-test verify-dir summary omitted mount doctor readiness", file=sys.stderr)
            return 1
        deltas = first_report["native_read"]["deltas"]
        if "read_leases" not in deltas or "vfs" not in deltas or "hydration" not in deltas:
            print("error: self-test verify-dir summary omitted read-path deltas", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        suite_dir = root / "mount-nfs-linux"
        suite_dir.mkdir()
        report, _native_read = build_self_test_report(suite_dir)
        write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir missing platform set was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        duplicate_dir = root / "duplicate-linux"
        duplicate_dir.mkdir()
        duplicate_report, _duplicate_native = build_self_test_report(duplicate_dir)
        duplicate_report["suite"] = "mount-nfs-linux"
        duplicate_report["platform"] = "linux"
        duplicate_report["run_id"] = "mount-nfs-linux-12345-1"
        write_json(duplicate_dir / "nfs-smoke-report.json", duplicate_report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir duplicate suite was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            if suite == "mount-nfs-windows":
                report["run_id"] = f"{suite}-99999-1"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir mixed run suffixes were not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            if suite == "mount-nfs-windows":
                report["git_commit"] = "fedcba9876543210fedcba9876543210fedcba98"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir mixed git commits were not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            if suite == "mount-nfs-windows":
                report["run_id"] = "self-test"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir malformed run id was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-2"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix="12345-2",
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status != 0:
            print("error: self-test verify-dir expected run suffix failed", file=sys.stderr)
            return 1
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix="99999-1",
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir mismatched run suffix was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for suite, platform in EXPECTED_SUITES.items():
            suite_dir = root / suite
            suite_dir.mkdir()
            report, _native_read = build_self_test_report(suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-12345-1"
            write_json(suite_dir / "nfs-smoke-report.json", report)
        status = verify_directory(
            root,
            require_artifacts=True,
            require_all_platforms=True,
            expected_run_suffix=None,
            expected_git_commit="fedcba9876543210fedcba9876543210fedcba98",
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test verify-dir mismatched git commit was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, _current_native = build_self_test_report(current_dir)
        baseline_report["run_id"] = "baseline"
        current_report["run_id"] = "current"
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        output = root / "comparison.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"mib_per_sec": 0.0, "read_rpcs_per_mib": 0.0},
            output=output,
            emit_output=False,
        )
        if status != 0 or not output.is_file():
            print("error: self-test native smoke comparison failed", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, baseline_native = build_self_test_report(baseline_dir)
        current_report, _current_native = build_self_test_report(current_dir)
        baseline_payload = json.loads(baseline_native.read_text(encoding="utf-8"))
        baseline_payload["nfs_vfs_delta"]["resolver_calls_avoided"] = 0
        baseline_payload["nfs_vfs_after"]["resolver_calls_avoided"] = (
            baseline_payload["nfs_vfs_before"]["resolver_calls_avoided"]
        )
        write_json(baseline_native, baseline_payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"resolver_calls_avoided_per_mib": 0.0},
            output=None,
            emit_output=False,
        )
        if status != 0:
            print(
                "error: self-test native smoke zero-baseline improvement was rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, current_native = build_self_test_report(current_dir)
        payload = json.loads(current_native.read_text(encoding="utf-8"))
        payload["mib_per_sec"] = 50.0
        write_json(current_native, payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"mib_per_sec": 10.0},
            output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test native smoke trend regression was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, current_native = build_self_test_report(current_dir)
        payload = json.loads(current_native.read_text(encoding="utf-8"))
        payload["nfs_read_leases_delta"]["hits"] = 8
        payload["nfs_read_leases_after"]["hits"] = (
            payload["nfs_read_leases_before"]["hits"]
            + payload["nfs_read_leases_delta"]["hits"]
        )
        write_json(current_native, payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"read_lease_hits_per_mib": 10.0},
            output=None,
            emit_output=False,
        )
        if status == 0:
            print(
                "error: self-test native read lease-hit trend regression was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, current_native = build_self_test_report(current_dir)
        payload = json.loads(current_native.read_text(encoding="utf-8"))
        payload["nfs_read_leases_delta"]["misses"] = 12
        payload["nfs_read_leases_after"]["misses"] = (
            payload["nfs_read_leases_before"]["misses"]
            + payload["nfs_read_leases_delta"]["misses"]
        )
        write_json(current_native, payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"read_lease_misses_per_mib": 10.0},
            output=None,
            emit_output=False,
        )
        if status == 0:
            print(
                "error: self-test native read lease-miss trend regression was not rejected",
                file=sys.stderr,
            )
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, current_native = build_self_test_report(current_dir)
        payload = json.loads(current_native.read_text(encoding="utf-8"))
        payload["nfs_hydration_delta"]["read_window_remote_bytes"] *= 2
        payload["nfs_hydration_after"]["read_window_remote_bytes"] = (
            payload["nfs_hydration_before"]["read_window_remote_bytes"]
            + payload["nfs_hydration_delta"]["read_window_remote_bytes"]
        )
        write_json(current_native, payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={"hydration_remote_bytes_per_user_byte": 10.0},
            output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test native hydration trend regression was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_dir = root / "baseline"
        current_dir = root / "current"
        baseline_dir.mkdir()
        current_dir.mkdir()
        baseline_report, _baseline_native = build_self_test_report(baseline_dir)
        current_report, current_native = build_self_test_report(current_dir)
        payload = json.loads(current_native.read_text(encoding="utf-8"))
        payload["read_size"] = 131_072
        write_json(current_native, payload)
        baseline_path = baseline_dir / "nfs-smoke-report.json"
        current_path = current_dir / "nfs-smoke-report.json"
        write_json(baseline_path, baseline_report)
        write_json(current_path, current_report)
        status = compare_native_read_reports(
            baseline_path,
            current_path,
            thresholds={},
            output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test native smoke workload mismatch was not rejected", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_root = root / "baseline"
        current_root = root / "current"
        baseline_root.mkdir()
        current_root.mkdir()
        for suite, platform in EXPECTED_SUITES.items():
            for parent, run_id in ((baseline_root, "baseline"), (current_root, "current")):
                suite_dir = parent / suite
                suite_dir.mkdir()
                report, _native_read = build_self_test_report(suite_dir)
                report["suite"] = suite
                report["platform"] = platform
                report["run_id"] = f"{suite}-{run_id}"
                write_json(suite_dir / "nfs-smoke-report.json", report)
        output = root / "directory-comparison.json"
        status = compare_directory(
            baseline_root,
            current_root,
            require_all_platforms=True,
            thresholds={"mib_per_sec": 0.0},
            summary_output=output,
            emit_output=False,
        )
        if status != 0 or not output.is_file():
            print("error: self-test native smoke directory comparison failed", file=sys.stderr)
            return 1

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        baseline_root = root / "baseline"
        current_root = root / "current"
        baseline_root.mkdir()
        current_root.mkdir()
        for suite, platform in EXPECTED_SUITES.items():
            current_suite_dir = current_root / suite
            current_suite_dir.mkdir()
            report, _native_read = build_self_test_report(current_suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-current"
            write_json(current_suite_dir / "nfs-smoke-report.json", report)

            if suite == "mount-nfs-windows":
                continue
            baseline_suite_dir = baseline_root / suite
            baseline_suite_dir.mkdir()
            report, _native_read = build_self_test_report(baseline_suite_dir)
            report["suite"] = suite
            report["platform"] = platform
            report["run_id"] = f"{suite}-baseline"
            write_json(baseline_suite_dir / "nfs-smoke-report.json", report)
        status = compare_directory(
            baseline_root,
            current_root,
            require_all_platforms=True,
            thresholds={},
            summary_output=None,
            emit_output=False,
        )
        if status == 0:
            print("error: self-test native smoke missing baseline suite was not rejected", file=sys.stderr)
            return 1

    print("ok: NFS smoke report verifier self-test passed")
    return 0


def native_read_thresholds_from_args(args: argparse.Namespace) -> tuple[dict[str, float], list[str]]:
    thresholds: dict[str, float] = {}
    errors: list[str] = []
    for arg_name, threshold_name in (
        ("min_native_read_mib_per_sec", "min_mib_per_sec"),
        (
            "max_native_read_requested_bytes_per_user_byte",
            "max_requested_bytes_per_user_byte",
        ),
        (
            "max_native_read_returned_bytes_per_user_byte",
            "max_returned_bytes_per_user_byte",
        ),
        ("max_native_read_rpcs_per_mib", "max_read_rpcs_per_mib"),
        ("min_native_read_lease_hits_per_mib", "min_read_lease_hits_per_mib"),
        ("max_native_read_lease_misses_per_mib", "max_read_lease_misses_per_mib"),
    ):
        value = getattr(args, arg_name)
        if value is None:
            continue
        if value <= 0:
            errors.append(f"--{arg_name.replace('_', '-')} must be positive")
            continue
        thresholds[threshold_name] = value
    return thresholds, errors


def add_threshold_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--min-native-read-mib-per-sec",
        type=float,
        help="Fail when the native sequential read artifact is slower than this.",
    )
    parser.add_argument(
        "--max-native-read-requested-bytes-per-user-byte",
        type=float,
        help="Fail when NFS requested-byte amplification exceeds this ratio.",
    )
    parser.add_argument(
        "--max-native-read-returned-bytes-per-user-byte",
        type=float,
        help="Fail when NFS returned-byte amplification exceeds this ratio.",
    )
    parser.add_argument(
        "--max-native-read-rpcs-per-mib",
        type=float,
        help="Fail when native read RPC density exceeds this ratio.",
    )
    parser.add_argument(
        "--min-native-read-lease-hits-per-mib",
        type=float,
        help="Fail when native read lease-hit density is below this ratio.",
    )
    parser.add_argument(
        "--max-native-read-lease-misses-per-mib",
        type=float,
        help="Fail when native read lease-miss density exceeds this ratio.",
    )


def native_read_comparison_thresholds_from_args(
    args: argparse.Namespace,
) -> tuple[dict[str, float], list[str]]:
    thresholds: dict[str, float] = {}
    errors: list[str] = []
    for arg_name, metric_name in (
        ("max_native_read_throughput_regression_pct", "mib_per_sec"),
        (
            "max_native_read_requested_amplification_regression_pct",
            "requested_bytes_per_user_byte",
        ),
        (
            "max_native_read_returned_amplification_regression_pct",
            "returned_bytes_per_user_byte",
        ),
        ("max_native_read_rpc_density_regression_pct", "read_rpcs_per_mib"),
        ("max_native_read_vfs_call_density_regression_pct", "vfs_read_calls_per_mib"),
        (
            "max_native_read_lease_hit_density_regression_pct",
            "read_lease_hits_per_mib",
        ),
        (
            "max_native_read_lease_miss_density_regression_pct",
            "read_lease_misses_per_mib",
        ),
        (
            "max_native_read_resolver_avoidance_regression_pct",
            "resolver_calls_avoided_per_mib",
        ),
        (
            "max_native_read_hydration_remote_byte_regression_pct",
            "hydration_remote_bytes_per_user_byte",
        ),
    ):
        value = getattr(args, arg_name)
        if value is None:
            continue
        if value < 0:
            errors.append(f"--{arg_name.replace('_', '-')} must be non-negative")
            continue
        thresholds[metric_name] = value
    return thresholds, errors


def add_comparison_threshold_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--max-native-read-throughput-regression-pct",
        type=float,
        help="Fail when native read throughput regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-requested-amplification-regression-pct",
        type=float,
        help="Fail when requested-byte amplification regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-returned-amplification-regression-pct",
        type=float,
        help="Fail when returned-byte amplification regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-rpc-density-regression-pct",
        type=float,
        help="Fail when native read RPC density regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-vfs-call-density-regression-pct",
        type=float,
        help="Fail when VFS read-call density regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-lease-hit-density-regression-pct",
        type=float,
        help="Fail when read-lease hit density regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-lease-miss-density-regression-pct",
        type=float,
        help="Fail when read-lease miss density regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-resolver-avoidance-regression-pct",
        type=float,
        help="Fail when resolver-avoidance density regresses by more than this percent.",
    )
    parser.add_argument(
        "--max-native-read-hydration-remote-byte-regression-pct",
        type=float,
        help="Fail when hydration remote-byte amplification regresses by more than this percent.",
    )


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "self-test":
        return self_test()
    if len(sys.argv) >= 2 and sys.argv[1] == "verify-dir":
        parser = argparse.ArgumentParser(
            description="Verify retained native NFS mount smoke reports under a directory.",
        )
        parser.add_argument("directory", type=Path, help="Directory containing retained NFS smoke artifacts.")
        parser.add_argument(
            "--require-artifacts",
            action="store_true",
            help="Require artifact paths in each report to exist or resolve beside that report.",
        )
        parser.add_argument(
            "--require-all-platforms",
            action="store_true",
            help="Require Linux, macOS, and Windows smoke suites in the directory.",
        )
        parser.add_argument(
            "--summary-output",
            type=Path,
            help="Write a JSON summary of the verified retained report set.",
        )
        parser.add_argument(
            "--expected-run-suffix",
            help="Require each report run_id to equal '<suite>-<suffix>'.",
        )
        parser.add_argument(
            "--expected-git-commit",
            help="Require each report git_commit to equal this full Git object id.",
        )
        add_threshold_args(parser)
        args = parser.parse_args(sys.argv[2:])
        thresholds, threshold_errors = native_read_thresholds_from_args(args)
        if threshold_errors:
            for error in threshold_errors:
                print(f"error: {error}", file=sys.stderr)
            return 2
        return verify_directory(
            args.directory,
            require_artifacts=args.require_artifacts,
            require_all_platforms=args.require_all_platforms,
            expected_run_suffix=args.expected_run_suffix,
            expected_git_commit=args.expected_git_commit,
            thresholds=thresholds,
            summary_output=args.summary_output,
        )
    if len(sys.argv) >= 2 and sys.argv[1] == "compare":
        parser = argparse.ArgumentParser(
            description="Compare retained native NFS mount smoke reports.",
        )
        parser.add_argument("baseline_report", type=Path, help="Baseline nfs-smoke-report.json")
        parser.add_argument("current_report", type=Path, help="Current nfs-smoke-report.json")
        parser.add_argument(
            "--output",
            type=Path,
            help="Write a JSON comparison summary.",
        )
        add_comparison_threshold_args(parser)
        args = parser.parse_args(sys.argv[2:])
        thresholds, threshold_errors = native_read_comparison_thresholds_from_args(args)
        if threshold_errors:
            for error in threshold_errors:
                print(f"error: {error}", file=sys.stderr)
            return 2
        return compare_native_read_reports(
            args.baseline_report,
            args.current_report,
            thresholds=thresholds,
            output=args.output,
        )
    if len(sys.argv) >= 2 and sys.argv[1] == "compare-dir":
        parser = argparse.ArgumentParser(
            description="Compare retained native NFS mount smoke report directories.",
        )
        parser.add_argument(
            "baseline_directory",
            type=Path,
            help="Directory containing baseline retained NFS smoke artifacts.",
        )
        parser.add_argument(
            "current_directory",
            type=Path,
            help="Directory containing current retained NFS smoke artifacts.",
        )
        parser.add_argument(
            "--require-all-platforms",
            action="store_true",
            help="Require Linux, macOS, and Windows smoke suites in both directories.",
        )
        parser.add_argument(
            "--summary-output",
            type=Path,
            help="Write a JSON summary of all platform comparisons.",
        )
        add_comparison_threshold_args(parser)
        args = parser.parse_args(sys.argv[2:])
        thresholds, threshold_errors = native_read_comparison_thresholds_from_args(args)
        if threshold_errors:
            for error in threshold_errors:
                print(f"error: {error}", file=sys.stderr)
            return 2
        return compare_directory(
            args.baseline_directory,
            args.current_directory,
            require_all_platforms=args.require_all_platforms,
            thresholds=thresholds,
            summary_output=args.summary_output,
        )

    parser = argparse.ArgumentParser(
        description="Verify a native NFS mount smoke JSON report.",
    )
    parser.add_argument("report", type=Path, help="Path to nfs-smoke-report.json")
    parser.add_argument("--suite", choices=sorted(EXPECTED_SUITES), help="Expected suite name.")
    parser.add_argument(
        "--platform",
        choices=sorted(set(EXPECTED_SUITES.values())),
        help="Expected platform name.",
    )
    parser.add_argument(
        "--require-artifacts",
        action="store_true",
        help="Require artifact paths in the report to exist or resolve beside the report.",
    )
    parser.add_argument(
        "--expected-run-suffix",
        help="Require report run_id to equal '<suite>-<suffix>'.",
    )
    parser.add_argument(
        "--expected-git-commit",
        help="Require report git_commit to equal this full Git object id.",
    )
    add_threshold_args(parser)
    args = parser.parse_args()

    thresholds, threshold_errors = native_read_thresholds_from_args(args)
    if threshold_errors:
        for error in threshold_errors:
            print(f"error: {error}", file=sys.stderr)
        return 2

    try:
        report = load_report(args.report)
    except ValueError as error:
        print(f"error: {args.report}: {error}", file=sys.stderr)
        return 1

    errors = validate(
        report,
        expected_suite=args.suite,
        expected_platform=args.platform,
        expected_run_suffix=args.expected_run_suffix,
        require_artifacts=args.require_artifacts,
        expected_git_commit=args.expected_git_commit,
        native_read_thresholds=thresholds,
        artifact_base=args.report.parent,
    )
    if errors:
        print(f"error: invalid NFS smoke report {args.report}:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print(f"ok: NFS smoke report verified: {args.report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
