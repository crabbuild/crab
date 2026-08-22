#!/usr/bin/env python3
"""Tests for concurrent-push HTTP request metering."""

from __future__ import annotations

import sys
import threading
import unittest
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_concurrent_push_smoke import RequestCountingProxy


class UpstreamHandler(BaseHTTPRequestHandler):
    def do_HEAD(self) -> None:
        self.send_response(200)
        self.send_header("Content-Length", "123")
        self.end_headers()

    def do_PUT(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class RequestCountingProxyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.upstream = ThreadingHTTPServer(("127.0.0.1", 0), UpstreamHandler)
        self.upstream.daemon_threads = True
        self.upstream_thread = threading.Thread(
            target=self.upstream.serve_forever, daemon=True
        )
        self.upstream_thread.start()
        self.proxy = RequestCountingProxy(
            f"http://127.0.0.1:{self.upstream.server_port}",
            "crab/e2e-concurrent-push/run",
        )
        self.proxy.start()

    def tearDown(self) -> None:
        self.proxy.close()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.upstream_thread.join(timeout=5)

    def test_forwards_body_and_records_bounded_request_class(self) -> None:
        request = urllib.request.Request(
            self.proxy.url
            + "/crab/e2e-concurrent-push/run/git_locator_db/manifest/current",
            data=b"payload",
            method="PUT",
        )

        with urllib.request.urlopen(request) as response:
            body = response.read()

        self.assertEqual(body, b"payload")
        self.assertEqual(
            self.proxy.snapshot()["classes"],
            {"git_locator_db/manifest:put": 1},
        )

    def test_preserves_head_content_length(self) -> None:
        request = urllib.request.Request(
            self.proxy.url + "/crab/e2e-concurrent-push/run/packs/pack.idx",
            method="HEAD",
        )

        with urllib.request.urlopen(request) as response:
            content_length = response.headers["Content-Length"]

        self.assertEqual(content_length, "123")


if __name__ == "__main__":
    unittest.main()
