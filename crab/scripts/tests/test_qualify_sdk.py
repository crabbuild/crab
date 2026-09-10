"""Tests for the SDK qualification report contract."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "crab/scripts/qualify_sdk.py"
DIGEST = "38f0f2ca3cd8afbb9d29853c2f8cfd77ceac0caa37b23f1b43d5153041266b15"


class QualificationRunnerTests(unittest.TestCase):
    def write_read_fixture(self, root: Path, *, core_requests: int = 10) -> Path:
        trials = []
        for implementation in ("core", "sdk"):
            for cache_state in ("cold", "warm"):
                for index in range(5):
                    stem = f"{implementation}-{cache_state}-{index}"
                    (root / f"{stem}.json").write_text(json.dumps({
                        "elapsed_seconds": 1.0 if implementation == "core" else 1.05,
                        "peak_rss_bytes": 256 * 1024 * 1024,
                        "verified": True,
                        "terminal_state": "exited",
                    }))
                    requests = core_requests if implementation == "core" else 10
                    (root / f"{stem}-origin.json").write_text(json.dumps({
                        "read_requests": requests,
                        "read_response_bytes": 1024,
                        "write_requests": 0,
                        "failures": 0,
                    }))
                    trials.append({
                        "trial": index + 1,
                        "implementation": implementation,
                        "cache_state": cache_state,
                        "measurement": f"{stem}.json",
                        "origin_metrics": f"{stem}-origin.json",
                    })
        manifest = root / "manifest.json"
        manifest.write_text(json.dumps({
            "source_sha": "a" * 40,
            "tool_versions": {"rustc": "fixture"},
            "backend": "rustfs",
            "fixture_digest": DIGEST,
            "runner": "fixture",
            "trials": trials,
        }))
        return manifest

    def test_run_records_required_fields_and_exact_test_count(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.json"
            result = subprocess.run([
                "python3", str(SCRIPT), "run", "--output", str(report),
                "--backend", "rustfs", "--features", "remote,content",
                "--fixture-digest", DIGEST, "--expected-tests", "1", "--",
                "python3", "-c",
                "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out')",
            ], cwd=ROOT, check=False)
            payload = json.loads(report.read_text())
            self.assertEqual(
                (result.returncode, payload["terminal_state"], payload["tests"]["passed"],
                 payload["backend"], payload["fixture_digest"], bool(payload["source_sha"])),
                (0, "passed", 1, "rustfs", DIGEST, True),
            )

    def test_run_rejects_ignored_live_test(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.json"
            result = subprocess.run([
                "python3", str(SCRIPT), "run", "--output", str(report),
                "--backend", "managed", "--fixture-digest", DIGEST,
                "--expected-tests", "1", "--", "python3", "-c",
                "print('test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out')",
            ], cwd=ROOT, check=False)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(report.read_text())["terminal_state"], "failed")

    def test_run_requires_verified_zero_write_transport_report(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "report.json"
            transport = root / "transport.json"
            transport.write_text(json.dumps({
                "verified": True,
                "requests": {"GET": 3, "PUT": 1},
                "response_body_bytes": 4096,
                "write_attempts": 1,
                "proxy_failures": 0,
            }))
            result = subprocess.run([
                "python3", str(SCRIPT), "run", "--output", str(report),
                "--backend", "rustfs", "--fixture-digest", DIGEST,
                "--expected-tests", "1", "--transport-report", str(transport), "--",
                "python3", "-c",
                "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out')",
            ], cwd=ROOT, check=False)
            payload = json.loads(report.read_text())
            self.assertEqual((result.returncode, payload["terminal_state"],
                              payload["transport"]["write_requests"]), (1, "failed", 1))

    def test_compare_read_accepts_complete_bounded_trials(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.write_read_fixture(root)
            report = root / "report.json"
            result = subprocess.run([
                "python3", str(SCRIPT), "compare-read", "--manifest", str(manifest),
                "--output", str(report),
            ], cwd=ROOT, check=False)
            payload = json.loads(report.read_text())
            self.assertEqual((result.returncode, payload["terminal_state"],
                              payload["sdk_to_core_ratios"]["cold"]["wall_seconds"]),
                             (0, "passed", 1.05))

    def test_compare_read_rejects_zero_traffic_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.write_read_fixture(root, core_requests=0)
            result = subprocess.run([
                "python3", str(SCRIPT), "compare-read", "--manifest", str(manifest),
                "--output", str(root / "report.json"),
            ], cwd=ROOT, check=False, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("shared-core baselines", result.stderr)

    def test_compare_read_normalizes_each_alternating_pair(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.write_read_fixture(root)
            payload = json.loads(manifest.read_text())
            elapsed = {
                "core": [10.15, 7.71, 7.19, 7.60, 8.47],
                "sdk": [11.00, 9.13, 7.89, 8.66, 6.40],
            }
            for trial in payload["trials"]:
                if trial["cache_state"] != "cold":
                    continue
                measurement = root / trial["measurement"]
                values = json.loads(measurement.read_text())
                values["elapsed_seconds"] = elapsed[trial["implementation"]][trial["trial"] - 1]
                measurement.write_text(json.dumps(values))
            report = root / "report.json"

            result = subprocess.run([
                "python3", str(SCRIPT), "compare-read", "--manifest", str(manifest),
                "--output", str(report),
            ], cwd=ROOT, check=False)
            report_payload = json.loads(report.read_text())

            unpaired_ratio = (
                report_payload["medians"]["sdk_cold"]["wall_seconds"]
                / report_payload["medians"]["core_cold"]["wall_seconds"]
            )
            self.assertGreater(unpaired_ratio, 1.10)
            self.assertLessEqual(
                report_payload["sdk_to_core_ratios"]["cold"]["wall_seconds"], 1.10
            )
            self.assertEqual(result.returncode, 0)

    def test_compare_read_rejects_duplicate_pair_member(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.write_read_fixture(root)
            payload = json.loads(manifest.read_text())
            payload["trials"][1]["trial"] = payload["trials"][0]["trial"]
            manifest.write_text(json.dumps(payload))

            result = subprocess.run([
                "python3", str(SCRIPT), "compare-read", "--manifest", str(manifest),
                "--output", str(root / "report.json"),
            ], cwd=ROOT, check=False, capture_output=True, text=True)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("duplicate", result.stderr)


if __name__ == "__main__":
    unittest.main()
