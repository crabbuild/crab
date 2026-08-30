#!/usr/bin/env python3
"""Tests for provider qualification retained-evidence validation."""

from __future__ import annotations

import copy
import importlib.util
import sys
import unittest
from pathlib import Path
from typing import Any


SCRIPT = Path(__file__).resolve().parents[1] / "verify-provider-qualification-report.py"
SPEC = importlib.util.spec_from_file_location("verify_provider_qualification", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
VERIFY = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VERIFY
SPEC.loader.exec_module(VERIFY)


def valid_report(provider: str = "s3") -> dict[str, Any]:
    object_count = VERIFY.PAGE_MINIMUM[provider] + 4
    checks = []
    for name in sorted(VERIFY.REQUIRED_CHECKS):
        details: dict[str, Any] = {}
        if name == "provider_pagination":
            details = {
                "objects": object_count,
                "crosses_default_provider_page": True,
            }
        checks.append(
            {
                "name": name,
                "ok": True,
                "duration_ms": 1,
                "details": details,
                "error": None,
            }
        )
    return {
        "schema": VERIFY.SCHEMA,
        "schema_version": VERIFY.SCHEMA_VERSION,
        "status": "ok",
        "provider": provider,
        "service": "rustfs" if provider == "s3" else provider,
        "region": "test-region",
        "bucket": "qualification-bucket",
        "isolated_prefix": "crab-provider-qualification/run-1",
        "source_sha": "a" * 40,
        "workflow_run_id": "123",
        "workflow_run_attempt": "2",
        "object_store_version": VERIFY.OBJECT_STORE_VERSION,
        "started_unix_ms": 1,
        "finished_unix_ms": 2,
        "commands": ["cargo test provider_contracts"],
        "request_metrics": {
            "logical_read_requests": 10,
            "logical_read_bytes": 20,
            "logical_write_requests": object_count + 10,
            "logical_write_bytes": object_count + 20,
            "listed_objects": object_count,
        },
        "checks": checks,
    }


class VerifyProviderQualificationReportTests(unittest.TestCase):
    def verify(self, report: dict[str, Any], provider: str = "s3") -> dict[str, Any]:
        return VERIFY.verify_report(
            report,
            provider=provider,
            source_sha="a" * 40,
            run_id="123",
            run_attempt="2",
        )

    def test_accepts_complete_v1_report(self) -> None:
        result = self.verify(valid_report())
        self.assertEqual(result["status"], "verified")
        self.assertEqual(result["checks"], len(VERIFY.REQUIRED_CHECKS))

    def test_rejects_non_v1_schema(self) -> None:
        report = valid_report()
        report["schema_version"] = 2
        with self.assertRaisesRegex(VERIFY.EvidenceError, "canonical.*v1"):
            self.verify(report)

    def test_rejects_missing_or_failed_check(self) -> None:
        report = valid_report()
        report["checks"] = report["checks"][1:]
        with self.assertRaisesRegex(VERIFY.EvidenceError, "check inventory mismatch"):
            self.verify(report)

        report = valid_report()
        report["checks"][0]["ok"] = False
        report["checks"][0]["error"] = "failed"
        with self.assertRaisesRegex(VERIFY.EvidenceError, "checks failed"):
            self.verify(report)

    def test_rejects_stale_provenance(self) -> None:
        for field, value, message in (
            ("source_sha", "b" * 40, "source SHA"),
            ("workflow_run_id", "999", "run ID"),
            ("workflow_run_attempt", "9", "run attempt"),
            ("object_store_version", "0.13.0", "dependency version"),
        ):
            with self.subTest(field=field):
                report = valid_report()
                report[field] = value
                with self.assertRaisesRegex(VERIFY.EvidenceError, message):
                    self.verify(report)

    def test_rejects_pagination_below_provider_boundary(self) -> None:
        report = valid_report("azure")
        pagination = next(
            check for check in report["checks"] if check["name"] == "provider_pagination"
        )
        pagination["details"]["objects"] = 5_000
        with self.assertRaisesRegex(VERIFY.EvidenceError, "page boundary"):
            self.verify(report, "azure")

    def test_rejects_secret_names(self) -> None:
        report = copy.deepcopy(valid_report())
        report["commands"].append("export AWS_SECRET_ACCESS_KEY=redacted")
        with self.assertRaisesRegex(VERIFY.EvidenceError, "credential material"):
            self.verify(report)


if __name__ == "__main__":
    unittest.main()
