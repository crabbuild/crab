#!/usr/bin/env python3
"""Verify a retained command-level Crab workflow evidence report.

The verifier intentionally checks the report contents, not its filename or
the producer job conclusion. Release jobs pass the exact source SHA and run
identity they are about to publish so stale or incomplete evidence fails
closed.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def fail(message: str) -> int:
    print(f"error: {message}", file=sys.stderr)
    return 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()

    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return fail(f"cannot read evidence report: {exc}")
    required = {
        "run_id",
        "status",
        "source_sha",
        "workflow_run_id",
        "workflow_run_attempt",
        "crab_version",
        "platform",
        "rustfs_image",
        "commands",
        "checks",
    }
    missing = sorted(required.difference(report))
    if missing:
        return fail(f"evidence report is missing fields: {', '.join(missing)}")
    if report["status"] != "ok":
        return fail(f"smoke status is {report['status']!r}")
    if report["source_sha"] != args.source_sha:
        return fail("evidence source SHA does not match the release commit")
    if report["workflow_run_id"] != args.run_id:
        return fail("evidence workflow run ID does not match")
    if str(report["workflow_run_attempt"]) != str(args.run_attempt):
        return fail("evidence workflow attempt does not match")
    if not report["crab_version"] or report["crab_version"] == "unknown":
        return fail("evidence does not identify the Crab release candidate")
    checks = report["checks"]
    if not isinstance(checks, list) or not checks:
        return fail("evidence contains no checks")
    failed = [check.get("name", "<unnamed>") for check in checks if not check.get("ok")]
    if failed:
        return fail(f"failed smoke checks: {', '.join(failed)}")
    for command in report["commands"]:
        if command.get("exit_code") != 0:
            return fail(f"command failed: {command.get('name', '<unnamed>')}")
    env = report.get("env", {})
    if env.get("AWS_ACCESS_KEY_ID") != "<redacted>" or env.get("AWS_SECRET_ACCESS_KEY") != "<redacted>":
        return fail("evidence environment is not redacted")
    serialized = json.dumps(report, sort_keys=True)
    for secret in ("crab", "secret", "token"):
        if f'"AWS_ACCESS_KEY_ID": "{secret}"' in serialized or f'"AWS_SECRET_ACCESS_KEY": "{secret}"' in serialized:
            return fail("evidence contains an unredacted RustFS credential")
    print(json.dumps({"status": "verified", "run_id": report["run_id"]}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
