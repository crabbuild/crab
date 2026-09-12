#!/usr/bin/env python3
"""Unit tests for the S3 gateway ecosystem qualification runner."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("s3_gateway_ecosystem.py")
SPEC = importlib.util.spec_from_file_location("s3_gateway_ecosystem", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
ECOSYSTEM = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ECOSYSTEM
SPEC.loader.exec_module(ECOSYSTEM)


class S3GatewayEcosystemTests(unittest.TestCase):
    def test_payload_is_deterministic_and_exact(self) -> None:
        first = ECOSYSTEM._payload("client", 4097)
        self.assertEqual(first, ECOSYSTEM._payload("client", 4097))
        self.assertNotEqual(first, ECOSYSTEM._payload("other", 4097))
        self.assertEqual(len(first), 4097)

    def test_report_is_identity_free_and_requires_every_client(self) -> None:
        report = ECOSYSTEM.build_report(
            "main/qualification/private-run",
            {
                "boto3": {"status": "passed", "version": "1"},
                "duckdb": {"status": "failed", "error_type": "Failure"},
            },
            123,
        )
        encoded = json.dumps(report, sort_keys=True)
        self.assertEqual(report["status"], "failed")
        self.assertNotIn("private-run", encoded)
        self.assertNotIn("endpoint", encoded)
        self.assertNotIn("bucket", encoded)
        self.assertFalse(report["coverage"]["complete"])

    def test_full_client_report_marks_complete_coverage(self) -> None:
        results = {
            name: {"status": "passed", "version": "1"}
            for name in ECOSYSTEM.CLIENTS
        }
        report = ECOSYSTEM.build_report("main/qualification/run", results, 123)
        self.assertEqual(report["status"], "passed")
        self.assertTrue(report["coverage"]["complete"])

    def test_selected_client_report_is_passed_but_marks_partial_coverage(self) -> None:
        report = ECOSYSTEM.build_report(
            "main/qualification/run",
            {"boto3": {"status": "passed", "version": "1"}},
            123,
        )
        self.assertEqual(report["status"], "passed")
        self.assertFalse(report["coverage"]["complete"])

    def test_context_process_environment_carries_session_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            context = ECOSYSTEM.Context(
                endpoint="http://127.0.0.1:8080",
                bucket="repository",
                access_key="access",
                secret_key="secret",
                session_token="token",
                region="us-east-1",
                prefix="main/qualification",
                work_dir=Path(directory),
                duckdb="duckdb",
                aws="aws",
                s3cmd="s3cmd",
                maven="mvn",
                go="go",
            )
            environment = context.process_env()
        self.assertEqual(environment["AWS_ACCESS_KEY_ID"], "access")
        self.assertEqual(environment["AWS_SESSION_TOKEN"], "token")
        self.assertEqual(environment["S3_GATEWAY_ECOSYSTEM_PREFIX"], context.prefix)

    def test_subprocess_failure_does_not_expose_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            context = ECOSYSTEM.Context(
                endpoint="http://127.0.0.1:8080",
                bucket="repository",
                access_key="access",
                secret_key="secret",
                session_token=None,
                region="us-east-1",
                prefix="main/qualification",
                work_dir=Path(directory),
                duckdb="duckdb",
                aws="aws",
                s3cmd="s3cmd",
                maven="mvn",
                go="go",
            )
            with self.assertRaises(ECOSYSTEM.QualificationError) as raised:
                ECOSYSTEM._run(
                    [sys.executable, "-c", "import sys; print('secret'); sys.exit(7)"],
                    context=context,
                )
        self.assertNotIn("secret", str(raised.exception))
        self.assertIn("status 7", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
