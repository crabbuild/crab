#!/usr/bin/env python3
"""Verify retained evidence for one real object-store provider."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


SCHEMA = "crab.provider-qualification"
SCHEMA_VERSION = 1
OBJECT_STORE_VERSION = "0.14.1"
REQUIRED_CHECKS = {
    "isolated_prefix_preflight",
    "create_only",
    "match_token_and_identity",
    "multipart_complete",
    "multipart_abort",
    "file_backed_staged_multipart",
    "exact_range_read",
    "provider_pagination",
    "retry_and_error_mapping",
    "multipart_cancellation",
    "origin_receipt",
    "isolated_prefix_cleanup",
}
PAGE_MINIMUM = {"s3": 1_001, "gcs": 1_001, "azure": 5_001}


class EvidenceError(ValueError):
    """The retained report does not prove the provider contract."""


def _require_string(report: dict[str, Any], name: str) -> str:
    value = report.get(name)
    if not isinstance(value, str) or not value:
        raise EvidenceError(f"{name} must be a non-empty string")
    return value


def verify_report(
    report: dict[str, Any],
    *,
    provider: str,
    source_sha: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, Any]:
    if report.get("schema") != SCHEMA or report.get("schema_version") != SCHEMA_VERSION:
        raise EvidenceError("report is not the canonical provider-qualification v1 schema")
    if report.get("status") != "ok":
        raise EvidenceError(f"qualification status is {report.get('status')!r}")
    if report.get("provider") != provider:
        raise EvidenceError("provider does not match the retained-evidence gate")
    if report.get("source_sha") != source_sha:
        raise EvidenceError("source SHA does not match the qualified commit")
    if str(report.get("workflow_run_id")) != str(run_id):
        raise EvidenceError("workflow run ID does not match")
    if str(report.get("workflow_run_attempt")) != str(run_attempt):
        raise EvidenceError("workflow run attempt does not match")
    if report.get("object_store_version") != OBJECT_STORE_VERSION:
        raise EvidenceError("object_store dependency version is missing or stale")

    service = _require_string(report, "service")
    region = _require_string(report, "region")
    bucket = _require_string(report, "bucket")
    prefix = _require_string(report, "isolated_prefix")
    if not prefix.startswith("crab-provider-qualification/") or prefix.count("/") != 1:
        raise EvidenceError("isolated prefix is outside the qualification namespace")
    if report.get("finished_unix_ms", 0) < report.get("started_unix_ms", 1):
        raise EvidenceError("qualification timestamps are invalid")

    commands = report.get("commands")
    if not isinstance(commands, list) or not commands or not all(
        isinstance(command, str) and command for command in commands
    ):
        raise EvidenceError("qualification command provenance is missing")

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
    failed = sorted(name for name, check in by_name.items() if check.get("ok") is not True)
    if failed:
        raise EvidenceError(f"provider checks failed: {', '.join(failed)}")
    if any(check.get("error") is not None for check in by_name.values()):
        raise EvidenceError("successful checks must not retain error text")

    pagination = by_name["provider_pagination"].get("details")
    if not isinstance(pagination, dict):
        raise EvidenceError("provider pagination details are missing")
    object_count = pagination.get("objects")
    if not isinstance(object_count, int) or object_count < PAGE_MINIMUM[provider]:
        raise EvidenceError("pagination evidence does not cross the provider page boundary")
    if pagination.get("crosses_default_provider_page") is not True:
        raise EvidenceError("pagination evidence is not marked as crossing a provider page")

    metrics = report.get("request_metrics")
    if not isinstance(metrics, dict):
        raise EvidenceError("request metrics are missing")
    for name in (
        "logical_read_requests",
        "logical_read_bytes",
        "logical_write_requests",
        "logical_write_bytes",
        "listed_objects",
    ):
        value = metrics.get(name)
        if not isinstance(value, int) or value <= 0:
            raise EvidenceError(f"request metric {name} must be positive")
    if metrics["listed_objects"] < object_count:
        raise EvidenceError("listed-object metrics do not cover pagination evidence")

    serialized = json.dumps(report, sort_keys=True).lower()
    forbidden = (
        "aws_secret_access_key",
        "google_service_account_key",
        "azure_storage_account_key",
        "client_secret",
        "bearer token",
    )
    leaked = [name for name in forbidden if name in serialized]
    if leaked:
        raise EvidenceError(f"report contains credential material: {', '.join(leaked)}")

    return {
        "status": "verified",
        "provider": provider,
        "service": service,
        "region": region,
        "bucket": bucket,
        "checks": len(by_name),
        "listed_objects": object_count,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    parser.add_argument("--provider", choices=sorted(PAGE_MINIMUM), required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        if not isinstance(report, dict):
            raise EvidenceError("report root must be an object")
        result = verify_report(
            report,
            provider=args.provider,
            source_sha=args.source_sha,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
        )
    except (OSError, json.JSONDecodeError, EvidenceError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
