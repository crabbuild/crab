#!/usr/bin/env python3
"""Verify exact SDK example reads while rejecting and counting S3 write attempts."""

import argparse
from collections import Counter
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import threading
from urllib.parse import urlsplit


class ReadGuard(ThreadingHTTPServer):
    daemon_threads = False

    def __init__(self, upstream):
        super().__init__(("127.0.0.1", 0), GuardHandler)
        self.upstream = upstream
        self.counts = Counter()
        self.failures = 0
        self.response_bytes = 0
        self.lock = threading.Lock()


class GuardHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        # Request targets and headers can contain signed credentials.
        pass

    def send_error(self, code, message=None, explain=None):
        # The base handler rejects unknown verbs before calling a do_* method.
        # Count those attempts too; an unrecognized method cannot pass the guard.
        if code == 501:
            with self.server.lock:
                self.server.counts[self.command] += 1
        super().send_error(code, message, explain)

    def reject_write(self):
        with self.server.lock:
            self.server.counts[self.command] += 1
        self.send_error(403, "qualification forbids writes")
        self.close_connection = True

    do_PUT = do_POST = do_DELETE = do_PATCH = reject_write

    def forward_read(self):
        with self.server.lock:
            self.server.counts[self.command] += 1
        upstream = self.server.upstream
        connection = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=120)
        # Preserve Host and signed headers. Only transport framing changes;
        # http.client removes upstream chunk framing while reading the body.
        headers = dict(self.headers.items())
        headers["Connection"] = "close"
        transferred = 0
        try:
            connection.request(self.command, self.path, headers=headers)
            response = connection.getresponse()
            self.send_response_only(response.status)
            for name, value in response.getheaders():
                if name.lower() not in {"connection", "transfer-encoding"}:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                while data := response.read(64 * 1024):
                    self.wfile.write(data)
                    transferred += len(data)
        except (OSError, http.client.HTTPException):
            with self.server.lock:
                self.server.failures += 1
        finally:
            connection.close()
            with self.server.lock:
                self.server.response_bytes += transferred
            self.close_connection = True

    do_GET = do_HEAD = forward_read


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("upstream", help="HTTP S3 endpoint, without a path or credentials")
    parser.add_argument("cases", type=Path, help="JSON array of args and expected_stdout")
    args = parser.parse_args()
    upstream = urlsplit(args.upstream)
    if (upstream.scheme != "http" or not upstream.hostname or upstream.username
            or upstream.password or upstream.path not in {"", "/"}
            or upstream.query or upstream.fragment):
        parser.error("upstream must be an HTTP endpoint without credentials, query or path")
    case_bytes = args.cases.read_bytes()
    cases = json.loads(case_bytes)
    if not isinstance(cases, list) or not cases:
        parser.error("cases must be a nonempty array")
    for case in cases:
        if (not isinstance(case, dict) or not isinstance(case.get("args"), list)
                or not all(isinstance(arg, str) for arg in case["args"])
                or not isinstance(case.get("expected_stdout"), str)):
            parser.error("each case requires string args and expected_stdout")
        code = case.get("expected_exit_code", 0)
        if type(code) is not int or not 0 <= code <= 255:
            parser.error("expected_exit_code must be an integer from 0 through 255")
        if not isinstance(case.get("expected_stderr", ""), str):
            parser.error("expected_stderr must be a string")
    executable = args.executable.resolve(strict=True)
    with executable.open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    reports = []
    with ReadGuard(upstream) as guard:
        worker = threading.Thread(target=guard.serve_forever)
        worker.start()
        environment = dict(os.environ)
        environment["AWS_ENDPOINT_URL_S3"] = f"http://127.0.0.1:{guard.server_port}"
        environment["AWS_ALLOW_HTTP"] = "true"
        environment["AWS_VIRTUAL_HOSTED_STYLE_REQUEST"] = "false"
        try:
            for case in cases:
                with guard.lock:
                    before = guard.counts.copy()
                try:
                    result = subprocess.run(
                        [str(executable), *case["args"]], env=environment,
                        capture_output=True, text=True, timeout=180,
                    )
                    report = {
                        "exit_code": result.returncode,
                        "stderr": result.stderr,
                        "verified": result.returncode == case.get("expected_exit_code", 0)
                        and result.stdout == case["expected_stdout"]
                        and result.stderr == case.get("expected_stderr", ""),
                    }
                except subprocess.TimeoutExpired:
                    report = {"verified": False, "timeout": True}
                with guard.lock:
                    requests = guard.counts - before
                report["requests"] = dict(requests)
                report["verified"] = (report["verified"]
                                      and requests["GET"] + requests["HEAD"] > 0
                                      and not any(method not in {"GET", "HEAD"} for method in requests))
                reports.append(report)
        finally:
            guard.shutdown()
            worker.join()
    writes = sum(count for method, count in guard.counts.items() if method not in {"GET", "HEAD"})
    verified = (all(report["verified"] for report in reports)
                and guard.counts["GET"] + guard.counts["HEAD"] > 0
                and not writes and not guard.failures)
    print(json.dumps({
        "executable_sha256": digest,
        "cases_sha256": hashlib.sha256(case_bytes).hexdigest(), "cases": reports,
        "requests": dict(guard.counts), "write_attempts": writes,
        "response_body_bytes": guard.response_bytes,
        "proxy_failures": guard.failures, "verified": verified,
    }, indent=2))
    raise SystemExit(0 if verified else 1)


if __name__ == "__main__":
    main()
