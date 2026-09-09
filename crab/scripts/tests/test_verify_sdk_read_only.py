"""Prove that rejected write attempts cannot qualify as read-only success."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "verify-sdk-read-only.py"


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "3")
        self.end_headers()
        self.wfile.write(b"abc")


class ReadOnlyTests(unittest.TestCase):
    def test_rejected_write_fails_even_when_reader_reports_success(self):
        with tempfile.TemporaryDirectory() as directory, ThreadingHTTPServer(
            ("127.0.0.1", 0), Upstream
        ) as upstream:
            worker = threading.Thread(target=upstream.serve_forever)
            worker.start()
            try:
                root = Path(directory)
                executable = root / "reader"
                executable.write_text(
                    f"#!{sys.executable}\n"
                    "import os,sys,urllib.request,urllib.error\n"
                    "with urllib.request.urlopen(os.environ['AWS_ENDPOINT_URL_S3']) as response: response.read()\n"
                    "request=urllib.request.Request(os.environ['AWS_ENDPOINT_URL_S3'],method=sys.argv[1])\n"
                    "try:\n"
                    " with urllib.request.urlopen(request) as response: response.read()\n"
                    "except urllib.error.HTTPError: pass\n"
                    "print('verified bytes')\n"
                    "code=int(sys.argv[2])\n"
                    "if code: print('Indexing', file=sys.stderr)\n"
                    "sys.exit(code)\n"
                )
                executable.chmod(0o700)
                cases = root / "cases.json"
                for method, code, expected_code, stderr, expected_status, expected_writes in [
                    ("GET", 0, 0, "", 0, 0), ("PUT", 0, 0, "", 1, 1),
                    ("TRACE", 0, 0, "", 1, 1),
                    ("GET", 7, 7, "Indexing\n", 0, 0),
                    ("GET", 7, 1, "Indexing\n", 1, 0),
                    ("GET", 7, 7, "NotFound\n", 1, 0),
                ]:
                    with self.subTest(method=method, code=code, expected_code=expected_code):
                        cases.write_text(json.dumps([
                            {"args": [method, str(code)], "expected_stdout": "verified bytes\n",
                             "expected_exit_code": expected_code,
                             "expected_stderr": stderr}
                        ]))
                        result = subprocess.run([
                            sys.executable, str(RUNNER), str(executable),
                            f"http://127.0.0.1:{upstream.server_port}", str(cases),
                        ], capture_output=True, text=True, check=False, timeout=10)
                        report = json.loads(result.stdout)
                        self.assertEqual(
                            (result.returncode, report["verified"], report["write_attempts"],
                             report["response_body_bytes"]),
                            (expected_status, expected_status == 0, expected_writes,
                             6 if method == "GET" else 3),
                        )
            finally:
                upstream.shutdown()
                worker.join()


if __name__ == "__main__":
    unittest.main()
