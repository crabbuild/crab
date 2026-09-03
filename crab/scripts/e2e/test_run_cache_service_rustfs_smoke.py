#!/usr/bin/env python3
"""Safety contracts for cache-service qualification against shared storage."""

from __future__ import annotations

import contextlib
import io
import json
import runpy
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_cache_service_rustfs_smoke as smoke_module


verifier_module = runpy.run_path(str(Path(__file__).resolve().parents[1] / "verify-cache-service-smoke-report.py"))


class CommandEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.smoke = object.__new__(smoke_module.CacheServiceRustfsSmoke)
        self.smoke.logs = root / "logs"
        self.smoke.logs.mkdir()
        self.smoke.artifacts = root / "artifacts"
        self.smoke.command_lock = threading.Lock()
        self.smoke.command_index = 0
        self.smoke.args = SimpleNamespace(timeout=10)
        self.smoke.env = {}
        self.smoke.report = smoke_module.SmokeReport(
            run_id="command-proof", status="running", root=str(root),
            endpoint_url="http://127.0.0.1:1", bucket="unused",
        )

    def retained_report(self) -> dict:
        return json.loads((self.smoke.artifacts / "report.json").read_text())

    def test_timeout_retains_attempt_and_logs_before_propagation(self) -> None:
        command = [sys.executable, "-c", (
            "import sys, threading; print('started', flush=True); "
            "print('waiting', file=sys.stderr, flush=True); threading.Event().wait()"
        )]
        with self.assertRaises(subprocess.TimeoutExpired):
            self.smoke.run_cmd("bounded command", command, self.smoke.logs, timeout=1, check=False)
        report = self.retained_report()
        self.assertEqual(report["status"], "failed")
        self.assertEqual(len(report["commands"]), 1)
        record = report["commands"][0]
        self.assertIsNone(record["exit_code"])
        self.assertTrue(record["timed_out"])
        self.assertGreaterEqual(record["duration_ms"], 1000)
        self.assertEqual(Path(record["stdout_log"]).read_text(), "started\n")
        self.assertEqual(Path(record["stderr_log"]).read_text(), "waiting\n")
        with self.assertRaises(verifier_module["VerifyError"]):
            verifier_module["Verifier"](report, Path("report.json")).verify_report_status()

    def test_completed_command_keeps_real_exit_code_and_redacted_args(self) -> None:
        for exit_code in (0, 3):
            with self.subTest(exit_code=exit_code):
                record = self.smoke.run_cmd(
                    "completed command", [sys.executable, "-c", f"raise SystemExit({exit_code})"],
                    self.smoke.logs, check=False, report_args=["redacted-command"],
                )
                retained = self.retained_report()["commands"][-1]
                self.assertEqual(record.exit_code, exit_code)
                self.assertFalse(retained["timed_out"])
                self.assertEqual(retained["args"], ["redacted-command"])



class OriginFixtureSafetyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.smoke = object.__new__(smoke_module.CacheServiceRustfsSmoke)
        self.smoke.run_id = "owned-run"
        self.smoke.args = SimpleNamespace(bucket="bucket")
        self.smoke.artifacts = Path(self.temp.name)
        self.smoke.run_aws = Mock()

    def test_synthetic_writes_reject_global_and_other_run_keys_before_io(self) -> None:
        for key in (
            ".crab/chunk_index_db/manifest/00000000000000000006.manifest",
            "e2e-cache-service/another-run/file",
            "e2e-cache-service/owned-run-suffix/file",
            "e2e-cache-service/owned-run/../other/file",
            "e2e-cache-service/owned-run/nested\\file",
        ):
            with self.subTest(key=key), self.assertRaises(smoke_module.SmokeError):
                self.smoke.put_origin_object(key, b"fixture")
        self.smoke.run_aws.assert_not_called()
        self.assertEqual(list(self.smoke.artifacts.iterdir()), [])

    def test_owned_fixture_writes_are_create_only(self) -> None:
        for prefix in ("e2e-cache-service", "e2e-cache-service-denied"):
            key = f"{prefix}/owned-run/probe"
            self.smoke.put_origin_object(key, b"fixture")
            args = self.smoke.run_aws.call_args.args[1]
            self.assertEqual(args[args.index("--if-none-match") + 1], "*")

    def test_synthetic_route_specs_never_target_global_metadata(self) -> None:
        specs = self.smoke.synthetic_immutable_route_specs()
        self.assertTrue(specs)
        self.assertTrue(all(key.startswith("e2e-cache-service/owned-run/") for _, key, _ in specs))

    def test_global_route_selection_uses_observed_nonempty_origin_bytes(self) -> None:
        self.smoke.check = Mock()
        prefix = ".crab/chunk_index_db/wal/"
        bodies = {prefix + "2.sst": b"", prefix + "1.sst": b"real WAL bytes"}
        self.smoke.require_proxy_state = Mock(return_value=SimpleNamespace(
            put_counts_snapshot=lambda: {key: 1 for key in bodies},
        ))
        self.smoke.get_origin_object = Mock(side_effect=bodies.__getitem__)
        result = self.smoke.origin_object_matching(prefix + "*.sst", lambda key: key.startswith(prefix))
        self.assertEqual(result, (prefix + "1.sst", b"real WAL bytes"))
        self.smoke.run_aws.assert_not_called()


class RunDirectorySafetyTests(unittest.TestCase):
    def test_unexpected_failure_retains_failed_status_and_original_exception(self) -> None:
        smoke = Mock()
        failure = AttributeError("missing integration method")
        smoke.run.side_effect = failure
        args = SimpleNamespace(audit_report=None)
        with patch.object(smoke_module, "parse_args", return_value=args), patch.object(
            smoke_module, "CacheServiceRustfsSmoke", return_value=smoke
        ), self.assertRaises(AttributeError) as result:
            smoke_module.main()
        self.assertIs(result.exception, failure)
        self.assertEqual(smoke.report.status, "failed")
        smoke.write_report.assert_called_once()

    def test_existing_run_is_not_overwritten_even_by_error_reporting(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory) / "retained"
            artifacts = run / "artifacts"
            artifacts.mkdir(parents=True)
            report = artifacts / "report.json"
            report.write_bytes(b"retained evidence")
            args = SimpleNamespace(root=Path(directory), run_id="retained", audit_report=None)
            with patch.object(smoke_module, "parse_args", return_value=args), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(smoke_module.main(), 1)
            self.assertEqual(report.read_bytes(), b"retained evidence")

    def test_run_id_cannot_escape_local_or_remote_run_scope(self) -> None:
        for run_id in (".", "..", "../escape", "nested/run", "windows\\run", "non-ascii-é"):
            with self.subTest(run_id=run_id), self.assertRaises(smoke_module.SmokeError):
                smoke_module.CacheServiceRustfsSmoke(SimpleNamespace(run_id=run_id))


class BucketSafetyTests(unittest.TestCase):
    def test_existing_bucket_is_rejected_before_create(self) -> None:
        for existing in (True, False):
            with self.subTest(existing=existing), tempfile.TemporaryDirectory() as directory:
                inventory = Path(directory) / "buckets.json"
                inventory.write_text(json.dumps({"Buckets": [{"Name": "owned"}] if existing else []}))
                smoke = object.__new__(smoke_module.CacheServiceRustfsSmoke)
                smoke.args = SimpleNamespace(bucket="owned")
                smoke.run_aws = Mock(return_value=SimpleNamespace(stdout_log=str(inventory)))
                smoke.write_report = Mock()
                smoke.report = SimpleNamespace(checks=[])
                if existing:
                    with self.assertRaises(smoke_module.SmokeError):
                        smoke.create_disposable_bucket()
                else:
                    smoke.create_disposable_bucket()
                operations = [call.args[1][0] for call in smoke.run_aws.call_args_list]
                self.assertEqual(operations, ["list-buckets"] if existing else ["list-buckets", "create-bucket"])


if __name__ == "__main__":
    unittest.main()
