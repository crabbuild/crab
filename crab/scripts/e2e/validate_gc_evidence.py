#!/usr/bin/env python3
"""Validate GC qualification evidence without trusting process exit status."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
SECRET_PATTERNS = (
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"(?i)aws_secret_access_key\s*[:=]"),
    re.compile(r"(?i)client_secret\s*[:=]"),
    re.compile(r"(?i)signature=[0-9a-f]{16,}"),
)
REQUIRED_KEYS = {
    "schema_version",
    "run_id",
    "status",
    "provider",
    "qualification_level",
    "fixture",
    "metrics",
    "checks",
    "commands",
    "artifacts",
    "started_at",
    "finished_at",
}
REQUIRED_CHECKS = {
    "live_objects_preserved",
    "unreachable_objects_deleted",
    "fsck_after_gc",
    "fresh_clone_readback",
    "writer_race",
    "resume_after_delete_crash",
    "resume_after_journal_crash",
    "bounded_memory",
    "bounded_writer_pause",
}


def fail(message: str) -> None:
    raise ValueError(message)


def walk_strings(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        return [item for child in value.values() for item in walk_strings(child)]
    if isinstance(value, list):
        return [item for child in value for item in walk_strings(child)]
    return []


def validate_report(path: Path, require_pass: bool) -> None:
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"{path}: invalid JSON: {error}")
    if not isinstance(report, dict):
        fail(f"{path}: report must be an object")
    missing = REQUIRED_KEYS.difference(report)
    if missing:
        fail(f"{path}: missing keys: {sorted(missing)}")
    if report["schema_version"] != SCHEMA_VERSION:
        fail(f"{path}: unsupported evidence schema")
    if report["status"] not in {"passed", "failed", "blocked", "unsupported"}:
        fail(f"{path}: invalid status")
    if report["qualification_level"] not in {"supplementary", "end_to_end"}:
        fail(f"{path}: invalid qualification level")
    if not isinstance(report["fixture"], dict) or report["fixture"].get("objects", 0) <= 0:
        fail(f"{path}: fixture cardinality is missing")
    if not isinstance(report["checks"], list) or not isinstance(report["commands"], list):
        fail(f"{path}: checks and commands must be arrays")
    for text in walk_strings(report):
        if any(pattern.search(text) for pattern in SECRET_PATTERNS):
            fail(f"{path}: possible credential or signed URL in evidence")
    if report["status"] in {"blocked", "unsupported"}:
        if not report.get("unsupported_reason"):
            fail(f"{path}: blocked/unsupported evidence needs a reason")
        if require_pass:
            fail(f"{path}: production validation requires a passing end-to-end report")
        return
    if report["status"] == "failed":
        if not report.get("unsupported_reason") and not any(
            check.get("status") == "failed" for check in report["checks"]
        ):
            fail(f"{path}: failed evidence has no failure detail")
        if require_pass:
            fail(f"{path}: production validation requires a passing end-to-end report")
        return
    if require_pass and report["qualification_level"] != "end_to_end":
        fail(f"{path}: supplementary evidence cannot satisfy a production pass")
    if report["qualification_level"] != "end_to_end":
        fail(f"{path}: passed evidence must be end-to-end")
    required_artifacts = {"journal", "fsck", "clone", "readback"}
    if not required_artifacts.issubset(report["artifacts"]):
        fail(f"{path}: end-to-end evidence is missing required artifacts")
    for name in required_artifacts:
        artifact = report["artifacts"].get(name)
        if not isinstance(artifact, str) or not artifact:
            fail(f"{path}: artifact {name} must name a retained file")
        artifact_path = Path(artifact)
        if not artifact_path.is_absolute():
            artifact_path = path.parent / artifact_path
        if not artifact_path.is_file():
            fail(f"{path}: retained artifact {name} does not exist")
    if any(check.get("status") != "passed" for check in report["checks"]):
        fail(f"{path}: every qualification check must pass")
    check_names = {
        check.get("name") for check in report["checks"] if isinstance(check, dict)
    }
    missing_checks = REQUIRED_CHECKS.difference(check_names)
    if missing_checks:
        fail(f"{path}: missing qualification checks: {sorted(missing_checks)}")
    if any(command.get("status") != "passed" for command in report["commands"]):
        fail(f"{path}: every qualification command must pass")
    if report["metrics"].get("referenced_shard_body_gets") not in {0, 0.0}:
        fail(f"{path}: closure-complete evidence reports shard body GETs")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--require-pass", action="store_true")
    args = parser.parse_args()
    reports = sorted(args.root.rglob("evidence.json"))
    if not reports:
        print("no GC evidence reports found", file=sys.stderr)
        return 2
    try:
        for report in reports:
            validate_report(report, args.require_pass)
    except ValueError as error:
        print(f"GC evidence validation failed: {error}", file=sys.stderr)
        return 1
    print(f"validated {len(reports)} GC evidence report(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
