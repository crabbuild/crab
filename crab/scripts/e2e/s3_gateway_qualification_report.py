#!/usr/bin/env python3
"""Produce and verify retained crab-s3-gateway qualification evidence."""

from __future__ import annotations

import argparse
import decimal
import hashlib
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


SCHEMA = "crab.s3-gateway-evidence"
SCHEMA_VERSION = 1
SUITE = "deployment"
BACKEND_IMAGE = "rustfs/rustfs:1.0.0-beta.8-glibc"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
REQUIRED_STEPS = {
    "build",
    "runtime",
    "initialize",
    "gateway",
    "traffic",
    "measurements",
    "graceful",
    "compose",
}
CHECK_OWNERS = {
    "packaged_image": "build",
    "runtime_contract": "runtime",
    "backend_initialization": "initialize",
    "constrained_runtime": "gateway",
    "signed_catalog": "traffic",
    "put_get_round_trip": "traffic",
    "exact_range_read": "traffic",
    "streaming_sigv4": "traffic",
    "streaming_rejection_atomicity": "traffic",
    "static_presigned_get": "traffic",
    "temporary_header_auth": "traffic",
    "temporary_presigned_get": "traffic",
    "temporary_token_rejection": "traffic",
    "unsigned_and_bad_signature_rejection": "traffic",
    "scratch_capacity_rejection": "traffic",
    "metrics_contract": "traffic",
    "credential_leak_scan": "traffic",
    "runtime_measurements": "measurements",
    "graceful_termination": "graceful",
    "compose_deployment": "compose",
    "multipart_restart_recovery": "compose",
}
REQUIRED_CHECKS = set(CHECK_OWNERS)
SENSITIVE_KEYS = {
    "access_key",
    "authorization",
    "bucket",
    "endpoint",
    "object_key",
    "repository",
    "secret_key",
    "session_token",
}
METRIC_RE = re.compile(
    r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{[^}]*\})?\s+([-+0-9.eE]+)$"
)
MEMORY_RE = re.compile(r"^\s*([0-9]+(?:\.[0-9]+)?)\s*([KMGT]?i?B)\s*/")
MEMORY_UNITS = {
    "B": 1,
    "KB": 1_000,
    "MB": 1_000_000,
    "GB": 1_000_000_000,
    "TB": 1_000_000_000_000,
    "KiB": 1 << 10,
    "MiB": 1 << 20,
    "GiB": 1 << 30,
    "TiB": 1 << 40,
}


class EvidenceError(ValueError):
    """The report does not prove the gateway qualification contract."""


def _run(root: Path, *command: str) -> bytes:
    return subprocess.run(
        command,
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
    ).stdout


def source_identity(root: Path) -> dict[str, Any]:
    status = _run(root, "git", "status", "--porcelain", "--untracked-files=all")
    diff = _run(root, "git", "diff", "--binary", "HEAD")
    return {
        "sha": _run(root, "git", "rev-parse", "HEAD").decode().strip(),
        "dirty": bool(status),
        "diff_sha256": hashlib.sha256(diff).hexdigest(),
    }


def parse_metrics(path: Path) -> dict[str, list[float]]:
    values: dict[str, list[float]] = {}
    if not path.is_file():
        return values
    for line in path.read_text(encoding="utf-8").splitlines():
        match = METRIC_RE.fullmatch(line)
        if match is not None:
            values.setdefault(match.group(1), []).append(float(match.group(2)))
    return values


def _sum(metrics: dict[str, list[float]], name: str) -> float:
    return float(sum(metrics.get(name, [])))


def _integer_sum(metrics: dict[str, list[float]], name: str) -> int:
    value = _sum(metrics, name)
    return int(value) if value.is_integer() else -1


def _read_nonnegative_integer(path: Path) -> int | None:
    try:
        value = int(path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return None
    return value if value >= 0 else None


def read_resident_memory(path: Path) -> int | None:
    try:
        value = path.read_text(encoding="utf-8")
    except OSError:
        return None
    match = MEMORY_RE.match(value)
    if match is None:
        return None
    amount = decimal.Decimal(match.group(1)) * MEMORY_UNITS[match.group(2)]
    resident_bytes = int(amount)
    return resident_bytes if resident_bytes > 0 else None


def _fixture(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {"bytes": None, "sha256": None}
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return {"bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def build_report(
    *,
    source: dict[str, Any],
    steps: dict[str, str],
    metrics: dict[str, list[float]],
    fixture: dict[str, Any],
    resident_memory_bytes: int | None,
    container_writable_bytes: int | None,
    started_unix_ms: int,
    finished_unix_ms: int,
    source_sha: str,
    run_id: str,
    run_attempt: str,
    gateway_image_id: str,
    aws_cli_version: str,
    curl_version: str,
    platform: str,
) -> dict[str, Any]:
    checks = [
        {
            "name": name,
            "owner": owner,
            "status": "passed" if steps.get(owner) == "success" else "incomplete",
        }
        for name, owner in sorted(CHECK_OWNERS.items())
    ]
    passed_assertions = sum(check["status"] == "passed" for check in checks)
    duration_sum = _sum(metrics, "crab_s3_gateway_http_request_duration_seconds_sum")
    duration_count = _integer_sum(
        metrics, "crab_s3_gateway_http_request_duration_seconds_count"
    )
    required_steps_passed = set(steps) == REQUIRED_STEPS and all(
        outcome == "success" for outcome in steps.values()
    )
    status = "passed" if required_steps_passed else "failed"
    return {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "terminal_state": "exited-zero" if status == "passed" else "incomplete",
        "suite": SUITE,
        "source": source,
        "expected_source_sha": source_sha,
        "workflow_run_id": str(run_id),
        "workflow_run_attempt": str(run_attempt),
        "started_unix_ms": started_unix_ms,
        "finished_unix_ms": finished_unix_ms,
        "elapsed_ms": max(0, finished_unix_ms - started_unix_ms),
        "versions": {
            "gateway_image_id": gateway_image_id,
            "backend_image": BACKEND_IMAGE,
            "aws_cli": aws_cli_version,
            "curl": curl_version,
            "platform": platform,
        },
        "fixture": fixture,
        "assertion_count": len(checks),
        "passed_assertions": passed_assertions,
        "skipped": [],
        "checks": checks,
        "measurements": {
            "http_requests": _integer_sum(
                metrics, "crab_s3_gateway_http_requests_total"
            ),
            "http_duration_count": duration_count,
            "http_duration_seconds": duration_sum,
            "http_average_duration_seconds": (
                duration_sum / duration_count if duration_count > 0 else None
            ),
            "backend_requests": _integer_sum(
                metrics, "crab_s3_gateway_backend_requests_total"
            ),
            "backend_bytes_read": _integer_sum(
                metrics, "crab_s3_gateway_backend_bytes_read_total"
            ),
            "backend_bytes_written": _integer_sum(
                metrics, "crab_s3_gateway_backend_bytes_written_total"
            ),
            "resident_memory_bytes": resident_memory_bytes,
            "cache_retained_bytes": _integer_sum(
                metrics, "crab_s3_gateway_cache_retained_bytes"
            ),
            "scratch_active_bytes": _integer_sum(
                metrics, "crab_s3_gateway_scratch_bytes"
            ),
            "container_writable_bytes": container_writable_bytes,
        },
    }


def _require_string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise EvidenceError(f"{name} must be a non-empty string")
    return value


def _walk_keys(value: Any) -> None:
    if isinstance(value, dict):
        for key, nested in value.items():
            if str(key).lower() in SENSITIVE_KEYS:
                raise EvidenceError(f"report contains forbidden identity field {key!r}")
            _walk_keys(nested)
    elif isinstance(value, list):
        for nested in value:
            _walk_keys(nested)


def verify_report(
    report: dict[str, Any], *, source_sha: str, run_id: str, run_attempt: str
) -> dict[str, Any]:
    if report.get("schema") != SCHEMA or report.get("schema_version") != SCHEMA_VERSION:
        raise EvidenceError("report is not the canonical S3 gateway qualification v1 schema")
    if report.get("status") != "passed" or report.get("terminal_state") != "exited-zero":
        raise EvidenceError("qualification did not reach a successful terminal state")
    if report.get("suite") != SUITE or report.get("skipped") != []:
        raise EvidenceError("qualification suite is wrong or contains skipped cells")
    if report.get("expected_source_sha") != source_sha:
        raise EvidenceError("expected source SHA does not match the qualified commit")
    source = report.get("source")
    if not isinstance(source, dict) or source.get("sha") != source_sha:
        raise EvidenceError("observed source SHA does not match the qualified commit")
    if source.get("dirty") is not False or source.get("diff_sha256") != EMPTY_SHA256:
        raise EvidenceError("qualification source is dirty")
    if str(report.get("workflow_run_id")) != str(run_id):
        raise EvidenceError("workflow run ID does not match")
    if str(report.get("workflow_run_attempt")) != str(run_attempt):
        raise EvidenceError("workflow run attempt does not match")
    started = report.get("started_unix_ms")
    finished = report.get("finished_unix_ms")
    elapsed = report.get("elapsed_ms")
    if not all(type(value) is int and value >= 0 for value in (started, finished, elapsed)):
        raise EvidenceError("qualification timing is missing")
    if finished < started or elapsed != finished - started:
        raise EvidenceError("qualification timing is inconsistent")

    versions = report.get("versions")
    if not isinstance(versions, dict) or versions.get("backend_image") != BACKEND_IMAGE:
        raise EvidenceError("pinned backend identity is missing or stale")
    image_id = _require_string(versions.get("gateway_image_id"), "gateway image ID")
    if re.fullmatch(r"sha256:[0-9a-f]{64}", image_id) is None:
        raise EvidenceError("gateway image ID is not a content digest")
    for name in ("aws_cli", "curl", "platform"):
        _require_string(versions.get(name), f"versions.{name}")

    fixture = report.get("fixture")
    if not isinstance(fixture, dict) or fixture.get("bytes") != 2 * 1024 * 1024:
        raise EvidenceError("qualification fixture size is wrong")
    if re.fullmatch(r"[0-9a-f]{64}", str(fixture.get("sha256"))) is None:
        raise EvidenceError("qualification fixture digest is missing")

    checks = report.get("checks")
    if not isinstance(checks, list):
        raise EvidenceError("checks must be a list")
    by_name: dict[str, dict[str, Any]] = {}
    for check in checks:
        if not isinstance(check, dict):
            raise EvidenceError("each check must be an object")
        name = check.get("name")
        if not isinstance(name, str) or name in by_name:
            raise EvidenceError(f"duplicate or invalid check name: {name!r}")
        by_name[name] = check
    missing = sorted(REQUIRED_CHECKS.difference(by_name))
    extra = sorted(set(by_name).difference(REQUIRED_CHECKS))
    if missing or extra:
        raise EvidenceError(f"check inventory mismatch; missing={missing}, extra={extra}")
    for name, check in by_name.items():
        if check.get("owner") != CHECK_OWNERS[name] or check.get("status") != "passed":
            raise EvidenceError(f"qualification check did not pass: {name}")
    if report.get("assertion_count") != len(REQUIRED_CHECKS):
        raise EvidenceError("assertion count does not match the required inventory")
    if report.get("passed_assertions") != len(REQUIRED_CHECKS):
        raise EvidenceError("not every required assertion passed")

    measurements = report.get("measurements")
    if not isinstance(measurements, dict):
        raise EvidenceError("measurements are missing")
    for name in (
        "http_requests",
        "http_duration_count",
        "backend_requests",
        "backend_bytes_read",
        "backend_bytes_written",
        "resident_memory_bytes",
    ):
        if type(measurements.get(name)) is not int or measurements[name] <= 0:
            raise EvidenceError(f"measurement {name} must be a positive integer")
    for name in (
        "cache_retained_bytes",
        "container_writable_bytes",
    ):
        if type(measurements.get(name)) is not int or measurements[name] < 0:
            raise EvidenceError(f"measurement {name} must be a non-negative integer")
    if measurements.get("scratch_active_bytes") != 0:
        raise EvidenceError("qualification retained active scratch bytes")
    for name in ("http_duration_seconds", "http_average_duration_seconds"):
        value = measurements.get(name)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or value <= 0:
            raise EvidenceError(f"measurement {name} must be positive")

    _walk_keys(report)
    return {
        "status": "verified",
        "checks": len(by_name),
        "http_requests": measurements["http_requests"],
        "backend_bytes_read": measurements["backend_bytes_read"],
        "backend_bytes_written": measurements["backend_bytes_written"],
    }


def _parse_steps(values: list[str]) -> dict[str, str]:
    steps: dict[str, str] = {}
    for value in values:
        name, separator, outcome = value.partition("=")
        if not separator or name in steps:
            raise EvidenceError(f"invalid or duplicate step outcome: {value!r}")
        steps[name] = outcome
    return steps


def produce(args: argparse.Namespace) -> int:
    try:
        started_unix_ms = int(args.started.read_text(encoding="utf-8").strip())
        steps = _parse_steps(args.step)
        report = build_report(
            source=source_identity(args.repository),
            steps=steps,
            metrics=parse_metrics(args.metrics),
            fixture=_fixture(args.fixture),
            resident_memory_bytes=read_resident_memory(args.resident_memory),
            container_writable_bytes=_read_nonnegative_integer(args.container_writable),
            started_unix_ms=started_unix_ms,
            finished_unix_ms=time.time_ns() // 1_000_000,
            source_sha=args.source_sha,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
            gateway_image_id=args.gateway_image_id,
            aws_cli_version=args.aws_cli_version,
            curl_version=args.curl_version,
            platform=args.platform,
        )
        args.report.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.report.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        temporary.replace(args.report)
    except (EvidenceError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"error: cannot produce qualification report: {error}", file=sys.stderr)
        return 1
    print(args.report)
    return 0


def verify(args: argparse.Namespace) -> int:
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        if not isinstance(report, dict):
            raise EvidenceError("report root must be an object")
        result = verify_report(
            report,
            source_sha=args.source_sha,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
        )
    except (EvidenceError, OSError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)
    producer = commands.add_parser("produce")
    producer.add_argument("--report", type=Path, required=True)
    producer.add_argument("--repository", type=Path, required=True)
    producer.add_argument("--metrics", type=Path, required=True)
    producer.add_argument("--fixture", type=Path, required=True)
    producer.add_argument("--resident-memory", type=Path, required=True)
    producer.add_argument("--container-writable", type=Path, required=True)
    producer.add_argument("--started", type=Path, required=True)
    producer.add_argument("--source-sha", required=True)
    producer.add_argument("--run-id", required=True)
    producer.add_argument("--run-attempt", required=True)
    producer.add_argument("--gateway-image-id", required=True)
    producer.add_argument("--aws-cli-version", required=True)
    producer.add_argument("--curl-version", required=True)
    producer.add_argument("--platform", required=True)
    producer.add_argument("--step", action="append", default=[])
    producer.set_defaults(handler=produce)
    verifier = commands.add_parser("verify")
    verifier.add_argument("--report", type=Path, required=True)
    verifier.add_argument("--source-sha", required=True)
    verifier.add_argument("--run-id", required=True)
    verifier.add_argument("--run-attempt", required=True)
    verifier.set_defaults(handler=verify)
    return root


def main() -> int:
    args = parser().parse_args()
    return args.handler(args)


if __name__ == "__main__":
    raise SystemExit(main())
