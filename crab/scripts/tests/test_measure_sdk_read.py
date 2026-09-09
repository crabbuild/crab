"""Verify that Linux measurement rejects incorrect reader output."""

import contextlib
import hashlib
import io
import json
import platform
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

RUNNER = Path(__file__).resolve().parents[1] / "measure-sdk-read.py"


@unittest.skipUnless(sys.platform == "linux", "Linux ru_maxrss measurement")
class MeasureReadTests(unittest.TestCase):
    def test_exact_result_is_verified_and_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "reader"
            executable.write_text(
                f"#!{sys.executable}\n"
                "print('commit=fixture bytes=3 blake3=digest')\n"
            )
            executable.chmod(0o700)
            for expected_size, expected_status in [(3, 0), (4, 1)]:
                with self.subTest(expected_size=expected_size):
                    result = subprocess.run(
                        [sys.executable, str(RUNNER), str(executable), "bucket",
                         "repository", "main", "file", str(root), "--commit",
                         "fixture", "--bytes", str(expected_size), "--blake3", "digest"],
                        capture_output=True, text=True, check=False,
                    )
                    report = json.loads(result.stdout)
                    self.assertEqual(
                        (result.returncode, report["verified"], report["peak_rss_bytes"] > 0,
                         report["executable_sha256"], report["expected"]),
                        (expected_status, expected_status == 0, True,
                         hashlib.sha256(executable.read_bytes()).hexdigest(),
                         {"commit": "fixture", "bytes": expected_size, "blake3": "digest"}),
                    )


class FailedMeasurementTests(unittest.TestCase):
    def test_unexecutable_binary_produces_failed_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "reader"
            executable.write_bytes(b"not an executable image")
            executable.chmod(0o700)
            args = [str(RUNNER), str(executable), "bucket", "repository", "main",
                    "file", str(root), "--commit", "fixture", "--bytes", "3",
                    "--blake3", "digest"]
            stdout = io.StringIO()
            with mock.patch.object(sys, "argv", args), \
                    mock.patch.object(sys, "platform", "linux"), \
                    mock.patch.object(platform, "platform", return_value="fixture Linux"), \
                    contextlib.redirect_stdout(stdout), \
                    self.assertRaises(SystemExit) as stopped:
                runpy.run_path(str(RUNNER), run_name="__main__")
            report = json.loads(stdout.getvalue())
            self.assertEqual(
                (stopped.exception.code, report["terminal_state"], report["exit_code"],
                 report["verified"], report["stdout"], report["stderr"]),
                (1, "spawn_failed", None, False, "", ""),
            )
            self.assertIsInstance(report["launch_error"]["errno"], int)
            self.assertIn(str(executable), report["launch_error"]["message"])

    def test_timeout_keeps_partial_output_without_verifying_a_complete_line(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "reader"
            executable.write_bytes(b"fixture executable")
            args = [str(RUNNER), str(executable), "bucket", "repository", "main",
                    "file", str(root), "--commit", "fixture", "--bytes", "3",
                    "--blake3", "digest"]
            stdout = io.StringIO()
            failure = subprocess.TimeoutExpired(
                [str(executable)], 180,
                output=b"commit=fixture bytes=3 blake3=digest\n",
                stderr=b"cleanup still pending\xff",
            )
            with mock.patch.object(sys, "argv", args), \
                    mock.patch.object(sys, "platform", "linux"), \
                    mock.patch.object(platform, "platform", return_value="fixture Linux"), \
                    mock.patch.object(subprocess, "run", side_effect=failure), \
                    contextlib.redirect_stdout(stdout), \
                    self.assertRaises(SystemExit) as stopped:
                runpy.run_path(str(RUNNER), run_name="__main__")
            report = json.loads(stdout.getvalue())
            self.assertEqual(
                (stopped.exception.code, report["terminal_state"], report["exit_code"],
                 report["verified"], report["stdout"], report["stderr"]),
                (1, "timed_out", None, False,
                 "commit=fixture bytes=3 blake3=digest\n", "cleanup still pending\ufffd"),
            )


if __name__ == "__main__":
    unittest.main()
