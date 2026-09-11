#!/usr/bin/env python3
"""Tests for crab-s3-gateway retained qualification evidence."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


SCRIPT = Path(__file__).with_name("s3_gateway_qualification_report.py")
SPEC = importlib.util.spec_from_file_location("s3_gateway_qualification_report", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
REPORT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = REPORT
SPEC.loader.exec_module(REPORT)


def valid_report() -> dict[str, Any]:
    checks = [
        {"name": name, "owner": owner, "status": "passed"}
        for name, owner in sorted(REPORT.CHECK_OWNERS.items())
    ]
    return {
        "schema": REPORT.SCHEMA,
        "schema_version": REPORT.SCHEMA_VERSION,
        "status": "passed",
        "terminal_state": "exited-zero",
        "suite": REPORT.SUITE,
        "source": {
            "sha": "a" * 40,
            "dirty": False,
            "diff_sha256": REPORT.EMPTY_SHA256,
        },
        "expected_source_sha": "a" * 40,
        "workflow_run_id": "123",
        "workflow_run_attempt": "2",
        "started_unix_ms": 100,
        "finished_unix_ms": 250,
        "elapsed_ms": 150,
        "versions": {
            "gateway_image_id": "sha256:" + "b" * 64,
            "backend_image": REPORT.BACKEND_IMAGE,
            "aws_cli": "aws-cli/2",
            "curl": "curl 8",
            "platform": "Linux x86_64",
        },
        "fixture": {"bytes": 2 * 1024 * 1024, "sha256": "c" * 64},
        "assertion_count": len(checks),
        "passed_assertions": len(checks),
        "skipped": [],
        "checks": checks,
        "measurements": {
            "http_requests": 20,
            "http_duration_count": 20,
            "http_duration_seconds": 1.0,
            "http_average_duration_seconds": 0.05,
            "backend_requests": 25,
            "backend_bytes_read": 100,
            "backend_bytes_written": 200,
            "rss_bytes": 1_000_000,
            "cache_retained_bytes": 100,
            "scratch_active_bytes": 0,
            "container_writable_bytes": 0,
        },
    }


class S3GatewayQualificationReportTests(unittest.TestCase):
    def verify(self, report: dict[str, Any]) -> dict[str, Any]:
        return REPORT.verify_report(
            report, source_sha="a" * 40, run_id="123", run_attempt="2"
        )

    def test_accepts_complete_v1_report(self) -> None:
        result = self.verify(valid_report())
        self.assertEqual(result["status"], "verified")
        self.assertEqual(result["checks"], len(REPORT.REQUIRED_CHECKS))

    def test_rejects_stale_or_dirty_source(self) -> None:
        for field, value in (
            ("sha", "b" * 40),
            ("dirty", True),
            ("diff_sha256", "d" * 64),
        ):
            with self.subTest(field=field):
                report = valid_report()
                report["source"][field] = value
                with self.assertRaisesRegex(REPORT.EvidenceError, "source"):
                    self.verify(report)

    def test_rejects_missing_failed_or_skipped_checks(self) -> None:
        report = valid_report()
        report["checks"] = report["checks"][1:]
        with self.assertRaisesRegex(REPORT.EvidenceError, "inventory mismatch"):
            self.verify(report)

        report = valid_report()
        report["checks"][0]["status"] = "incomplete"
        with self.assertRaisesRegex(REPORT.EvidenceError, "did not pass"):
            self.verify(report)

        report = valid_report()
        report["skipped"] = ["sigv4"]
        with self.assertRaisesRegex(REPORT.EvidenceError, "skipped"):
            self.verify(report)

    def test_rejects_missing_measurements(self) -> None:
        for field, value in (
            ("http_requests", 0),
            ("backend_bytes_read", None),
            ("rss_bytes", -1),
            ("scratch_active_bytes", 1),
            ("http_duration_seconds", 0.0),
        ):
            with self.subTest(field=field):
                report = valid_report()
                report["measurements"][field] = value
                with self.assertRaisesRegex(REPORT.EvidenceError, "measurement|scratch"):
                    self.verify(report)

    def test_rejects_identity_and_secret_fields(self) -> None:
        for field in REPORT.SENSITIVE_KEYS:
            with self.subTest(field=field):
                report = valid_report()
                report["provenance"] = {field: "must-not-appear"}
                with self.assertRaisesRegex(REPORT.EvidenceError, "forbidden identity"):
                    self.verify(report)

    def test_metric_parser_sums_bounded_series(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metrics.txt"
            path.write_text(
                "# TYPE request counter\n"
                'request{method="get"} 2\n'
                'request{method="put"} 3\n'
                "gauge 7\n",
                encoding="utf-8",
            )
            metrics = REPORT.parse_metrics(path)
        self.assertEqual(metrics, {"request": [2.0, 3.0], "gauge": [7.0]})

    def test_builder_marks_failed_steps_incomplete(self) -> None:
        report = valid_report()
        metrics = {
            "crab_s3_gateway_http_requests_total": [20.0],
            "crab_s3_gateway_http_request_duration_seconds_count": [20.0],
            "crab_s3_gateway_http_request_duration_seconds_sum": [1.0],
            "crab_s3_gateway_backend_requests_total": [25.0],
            "crab_s3_gateway_backend_bytes_read_total": [100.0],
            "crab_s3_gateway_backend_bytes_written_total": [200.0],
            "crab_s3_gateway_cache_retained_bytes": [100.0],
            "crab_s3_gateway_scratch_bytes": [0.0],
        }
        built = REPORT.build_report(
            source=report["source"],
            steps={name: "success" for name in REPORT.REQUIRED_STEPS - {"traffic"}},
            metrics=metrics,
            fixture=report["fixture"],
            rss_bytes=1_000_000,
            container_writable_bytes=0,
            started_unix_ms=100,
            finished_unix_ms=250,
            source_sha="a" * 40,
            run_id="123",
            run_attempt="2",
            gateway_image_id="sha256:" + "b" * 64,
            aws_cli_version="aws-cli/2",
            curl_version="curl 8",
            platform="Linux x86_64",
        )
        self.assertEqual(built["status"], "failed")
        self.assertTrue(
            any(check["status"] == "incomplete" for check in built["checks"])
        )

    def test_builder_retains_a_failed_report_without_measurements(self) -> None:
        report = valid_report()
        built = REPORT.build_report(
            source=report["source"],
            steps={name: "skipped" for name in REPORT.REQUIRED_STEPS},
            metrics={},
            fixture={"bytes": None, "sha256": None},
            rss_bytes=None,
            container_writable_bytes=None,
            started_unix_ms=100,
            finished_unix_ms=250,
            source_sha="a" * 40,
            run_id="123",
            run_attempt="2",
            gateway_image_id="unavailable",
            aws_cli_version="aws-cli/2",
            curl_version="curl 8",
            platform="Linux x86_64",
        )
        self.assertEqual(built["status"], "failed")
        self.assertEqual(built["measurements"]["http_requests"], 0)
        self.assertIsNone(built["measurements"]["http_average_duration_seconds"])


if __name__ == "__main__":
    unittest.main()
