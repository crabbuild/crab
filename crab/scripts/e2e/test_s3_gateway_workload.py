#!/usr/bin/env python3
"""Unit tests for the sustained S3 gateway workload runner."""

from __future__ import annotations

import importlib.util
import json
import sys
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


SCRIPT = Path(__file__).with_name("s3_gateway_workload.py")
SPEC = importlib.util.spec_from_file_location("s3_gateway_workload", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
WORKLOAD = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = WORKLOAD
SPEC.loader.exec_module(WORKLOAD)


def stats(*, count: int, latency: float = 10.0) -> WORKLOAD.WorkloadStats:
    return WORKLOAD.WorkloadStats(
        requests=count,
        acknowledged=count,
        latencies_ms=[latency] * count,
        acknowledged_keys={f"key-{index}" for index in range(count)},
        listed_keys=count,
        elapsed_ms=1000,
    )


class S3GatewayWorkloadTests(unittest.TestCase):
    def test_live_http_workload_proves_acknowledged_keys_are_listed(self) -> None:
        class Handler(BaseHTTPRequestHandler):
            objects: dict[str, set[str]] = {"gateway": set(), "baseline": set()}
            lock = threading.Lock()

            def do_PUT(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
                parsed = urllib.parse.urlsplit(self.path)
                bucket, _, encoded_key = parsed.path.lstrip("/").partition("/")
                size = int(self.headers.get("Content-Length", "0"))
                self.rfile.read(size)
                with self.lock:
                    self.objects.setdefault(bucket, set()).add(
                        urllib.parse.unquote(encoded_key)
                    )
                self.send_response(200)
                self.end_headers()

            def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
                parsed = urllib.parse.urlsplit(self.path)
                bucket = parsed.path.lstrip("/").split("/", 1)[0]
                prefix = urllib.parse.parse_qs(parsed.query).get("prefix", [""])[0]
                with self.lock:
                    keys = sorted(
                        key for key in self.objects.get(bucket, set()) if key.startswith(prefix)
                    )
                contents = "".join(f"<Contents><Key>{key}</Key></Contents>" for key in keys)
                body = (
                    "<ListBucketResult><IsTruncated>false</IsTruncated>"
                    f"{contents}</ListBucketResult>"
                ).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_: object) -> None:
                return

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        endpoint = f"http://127.0.0.1:{server.server_port}"
        try:
            baseline = WORKLOAD._run_endpoint(
                WORKLOAD.Endpoint(endpoint, "baseline", "access", "secret", "us-east-1"),
                "isolated",
                writers=2,
                duration_seconds=0.05,
                object_bytes=4,
                timeout=2,
            )
            gateway = WORKLOAD._run_endpoint(
                WORKLOAD.Endpoint(endpoint, "gateway", "access", "secret", "us-east-1"),
                "isolated",
                writers=2,
                duration_seconds=0.05,
                object_bytes=4,
                timeout=2,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
        for result in (baseline, gateway):
            self.assertGreater(result.acknowledged, 0)
            self.assertEqual(result.failed, 0)
            self.assertEqual(result.missing_acknowledgements, 0)
            self.assertEqual(result.unexpected_keys, 0)
            self.assertEqual(result.duplicate_list_entries, 0)

    def test_query_and_path_encoding_are_sigv4_canonical(self) -> None:
        self.assertEqual(
            WORKLOAD._canonical_query(
                [("prefix", "a b/"), ("list-type", "2"), ("prefix", "a+b")]
            ),
            "list-type=2&prefix=a%20b%2F&prefix=a%2Bb",
        )
        self.assertEqual(
            WORKLOAD._canonical_path("repo", "main/space name/%value"),
            "/repo/main/space%20name/%25value",
        )

    def test_payload_is_deterministic_and_exact_size(self) -> None:
        first = WORKLOAD._payload(3, 7, 4096)
        self.assertEqual(first, WORKLOAD._payload(3, 7, 4096))
        self.assertNotEqual(first, WORKLOAD._payload(3, 8, 4096))
        self.assertEqual(len(first), 4096)

    def test_report_contains_only_aggregate_identity_free_measurements(self) -> None:
        report = WORKLOAD.build_report(
            prefix="main/qualification/secret-object-prefix",
            writers=16,
            duration_seconds=5,
            object_bytes=4096,
            timeout=30,
            gateway=stats(count=10, latency=10),
            baseline=stats(count=10, latency=10),
            min_throughput_ratio=0.9,
            max_p95_ratio=1.25,
        )
        encoded = json.dumps(report, sort_keys=True)
        self.assertEqual(report["status"], "passed")
        self.assertNotIn("secret-object-prefix", encoded)
        self.assertNotIn("gateway", report["workload"])
        self.assertEqual(report["workload"]["writers"], 16)
        self.assertEqual(report["comparison"]["p95_latency_ratio"], 1.0)

    def test_report_fails_when_an_acknowledged_key_is_missing(self) -> None:
        gateway = stats(count=2)
        gateway.missing_acknowledgements = 1
        report = WORKLOAD.build_report(
            prefix="isolated",
            writers=1,
            duration_seconds=1,
            object_bytes=4096,
            timeout=2,
            gateway=gateway,
            baseline=stats(count=2),
            min_throughput_ratio=0.9,
            max_p95_ratio=1.25,
        )
        self.assertEqual(report["status"], "failed")
        self.assertFalse(report["comparison"]["integrity_passed"])

    def test_percentile_uses_nearest_observed_sample(self) -> None:
        value = WORKLOAD.WorkloadStats(latencies_ms=[4, 1, 9, 3])
        self.assertEqual(value.percentile(0.50), 3)
        self.assertEqual(value.percentile(0.95), 9)


if __name__ == "__main__":
    unittest.main()
