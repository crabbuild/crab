"""Tests for the SDK qualification report contract."""

import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "crab/scripts/qualify_sdk.py"
DIGEST = "38f0f2ca3cd8afbb9d29853c2f8cfd77ceac0caa37b23f1b43d5153041266b15"
QUALIFICATION = runpy.run_path(str(SCRIPT))


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
                "--backend", "rustfs", "--features", "remote,files",
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


class ReadDiagnosticTests(unittest.TestCase):
    def test_origin_retains_pending_and_completed_range_without_credentials(self) -> None:
        started = threading.Event()
        release = threading.Event()

        class Upstream(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                started.set()
                if not release.wait(5):
                    return
                self.send_response(206)
                self.send_header("Content-Length", "3")
                self.end_headers()
                self.wfile.write(b"abc")

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        proxy = QUALIFICATION["ReadOrigin"](urlsplit(f"http://127.0.0.1:{upstream.server_port}"))
        workers = [threading.Thread(target=server.serve_forever) for server in (upstream, proxy)]
        for worker in workers:
            worker.start()
        connection = http.client.HTTPConnection("127.0.0.1", proxy.server_port, timeout=5)
        try:
            connection.request("GET", "/object?credential=private", headers={
                "Range": "bytes=1-3", "Authorization": "private",
            })
            self.assertTrue(started.wait(5))
            counts, size, failures, reads = proxy.snapshot()
            self.assertEqual((counts["GET"], size, failures, reads[0]["complete"]), (1, 0, 0, False))
            release.set()
            body = connection.getresponse().read()
        finally:
            release.set()
            connection.close()
            for server in (proxy, upstream):
                server.shutdown()
                server.server_close()
            for worker in workers:
                worker.join()
        counts, size, failures, reads = proxy.snapshot()
        self.assertEqual((body, counts["GET"], size, failures, reads), (b"abc", 1, 3, 0, [{
            "method": "GET", "path_sha256": hashlib.sha256(b"/object").hexdigest(),
            "range": "bytes=1-3", "complete": True,
            "status": 206, "response_bytes": 3, "failed": False,
        }]))

    @unittest.skipUnless(os.name == "posix", "symlinks need Windows developer privileges")
    def test_cache_inventory_does_not_follow_payload_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "cache"
            cache.mkdir()
            outside = root / "outside"
            outside.write_bytes(b"external payload")
            (cache / "link").symlink_to(outside)
            (cache / "owned").write_bytes(b"owned")
            inventory = QUALIFICATION["cache_inventory"](cache)
            self.assertEqual((inventory["bytes"], [(entry["path"], entry["bytes"])
                                                   for entry in inventory["files"]]),
                             (5, [("owned", 5)]))


if __name__ == "__main__":
    unittest.main()
