#!/usr/bin/env python3
"""Tests for concurrent-push HTTP request metering."""

from __future__ import annotations

import argparse
import http.client
import json
import os
import socket
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_concurrent_push_smoke import (
    ConcurrentPushSmoke,
    RequestCountingProxy,
    SmokeError,
    locator_requests_per_success,
    parse_stage_counts,
    push_failure_stages,
    store_category,
)


class FsckGateTest(unittest.TestCase):
    def test_requires_explicit_clean_result_without_repairing_first(self) -> None:
        clean = {"passed": True, "errors": 0, "repaired": 0, "repair_failures": 0}
        cases = [
            ("clean", clean, True),
            ("missing-output", None, False),
            ("missing-fields", {}, False),
            ("failed", {**clean, "passed": False}, False),
            ("errors", {**clean, "errors": 1}, False),
            ("repaired", {**clean, "repaired": 1}, False),
            ("repair-failures", {**clean, "repair_failures": 1}, False),
        ]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "fsck.json"
            for name, data, accepted in cases:
                with self.subTest(name=name):
                    output.write_text(
                        json.dumps({"schema": "fsck", "data": data}) if data is not None else "",
                        encoding="utf-8",
                    )
                    smoke = object.__new__(ConcurrentPushSmoke)
                    smoke.args = SimpleNamespace(skip_fsck=False)
                    smoke.seed = Path(directory)
                    smoke.run_crab = Mock(return_value=SimpleNamespace(stdout_log=output))

                    def check(_name, ok, _detail=None):
                        if not ok:
                            raise SmokeError("unclean or missing integrity proof")

                    smoke.check = check
                    if accepted:
                        smoke.run_fsck()
                    else:
                        with self.assertRaises(SmokeError):
                            smoke.run_fsck()
                    self.assertEqual(
                        smoke.run_crab.call_args_list,
                        [call(smoke.seed, ["fsck", "--json"], name="crab fsck")],
                    )


class PushCommandArgumentsTest(unittest.TestCase):
    def test_fault_probes_control_agent_integration_retry_mode(self) -> None:
        smoke = object.__new__(ConcurrentPushSmoke)
        smoke.args = SimpleNamespace(
            crab_bin="crab",
            manifest_cas_retries=64,
            upload_concurrency=2,
            omit_lock_wait_secs=True,
            lock_wait_secs=30,
            rebase_on_non_fast_forward=True,
            rebase_retry_limit=64,
        )

        args = smoke.push_args(
            "HEAD:refs/heads/pre-marker-crash",
            lock_wait_secs=0,
            rebase_on_non_fast_forward=False,
        )

        self.assertEqual(args[3:5], ["--lock-wait-secs", "0"])
        self.assertNotIn("--rebase-on-non-fast-forward", args)

        bounded_retry_args = smoke.push_args(
            "HEAD:refs/heads/marker-write-failure",
            lock_wait_secs=0,
            rebase_retry_limit=2,
        )
        self.assertEqual(bounded_retry_args[-2:], ["--rebase-retry-limit", "2"])


class LocatorRequestBudgetTest(unittest.TestCase):
    def test_counts_only_locator_categories_per_success(self) -> None:
        snapshot = {
            "successful_pushes": 4,
            "categories": {
                "git_object_catalog_db/manifest": 80,
                "git_object_catalog_db/compacted": 20,
                "packs": 400,
            },
        }

        self.assertEqual(locator_requests_per_success(snapshot), 25.0)

    def test_requires_a_successful_push(self) -> None:
        self.assertIsNone(
            locator_requests_per_success(
                {
                    "successful_pushes": 0,
                    "categories": {"git_object_catalog_db/wal": 1},
                }
            )
        )


class PushArgsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = object.__new__(ConcurrentPushSmoke)
        self.smoke.args = argparse.Namespace(
            crab_bin="crab",
            manifest_cas_retries=3,
            upload_concurrency=4,
            omit_lock_wait_secs=False,
            lock_wait_secs=5,
            rebase_on_non_fast_forward=True,
            rebase_retry_limit=6,
        )

    def test_can_disable_rebase_for_immediate_lock_probe(self) -> None:
        args = self.smoke.push_args(
            "HEAD:refs/heads/recovery",
            lock_wait_secs=0,
            rebase_on_non_fast_forward=False,
        )

        self.assertNotIn("--rebase-on-non-fast-forward", args)
        self.assertEqual(args[args.index("--lock-wait-secs") + 1], "0")


class PushFailureStagesTest(unittest.TestCase):
    def test_parses_only_nonnegative_integer_stage_counts(self) -> None:
        self.assertEqual(
            parse_stage_counts(
                {"ref-commit": 2, "lock": 1, "bad": -1, "bool": True}
            ),
            {"lock": 1, "ref-commit": 2},
        )

    def test_counts_only_attributed_failures_from_current_command_slice(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            previous = {
                "operation": "push",
                "outcome": "failure",
                "details": {"failure_stage": "lock"},
            }
            path.write_text(json.dumps(previous) + "\n", encoding="utf-8")
            offset = path.stat().st_size
            events = [
                {
                    "operation": "push",
                    "outcome": "failure",
                    "details": {"failure_stage": "ref-commit"},
                },
                {
                    "operation": "push",
                    "outcome": "failure",
                    "details": {"failure_stage": "ref-commit"},
                },
                {
                    "operation": "push",
                    "outcome": "success",
                    "details": {},
                },
                {
                    "operation": "fetch",
                    "outcome": "failure",
                    "details": {"failure_stage": "remote-state"},
                },
            ]
            with path.open("a", encoding="utf-8") as audit:
                for event in events:
                    audit.write(json.dumps(event) + "\n")

            self.assertEqual(
                push_failure_stages(path, offset),
                {"ref-commit": 2},
            )


class UpstreamHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    put_bodies: list[bytes] = []
    request_ports: list[int] = []
    put_status = 200
    reject_put_before_body = False
    get_status = 200
    disconnect_get = False
    close_idle_get = False
    idle_closed = threading.Event()

    def do_GET(self) -> None:
        self.request_ports.append(self.client_address[1])
        if self.disconnect_get:
            self.close_connection = True
            return
        self.send_response(self.get_status)
        self.send_header("Content-Length", "0")
        self.end_headers()
        if self.close_idle_get:
            self.connection.shutdown(socket.SHUT_WR)
            self.close_connection = True
            self.idle_closed.set()

    def do_HEAD(self) -> None:
        self.request_ports.append(self.client_address[1])
        self.send_response(200)
        self.send_header("Content-Length", "123")
        self.end_headers()

    def do_PUT(self) -> None:
        self.request_ports.append(self.client_address[1])
        if self.reject_put_before_body:
            self.send_response(self.put_status)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.put_bodies.append(body)
        self.send_response(self.put_status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class RequestCountingProxyTest(unittest.TestCase):
    def setUp(self) -> None:
        UpstreamHandler.put_bodies.clear()
        UpstreamHandler.request_ports.clear()
        UpstreamHandler.put_status = 200
        UpstreamHandler.reject_put_before_body = False
        UpstreamHandler.get_status = 200
        UpstreamHandler.disconnect_get = False
        UpstreamHandler.close_idle_get = False
        UpstreamHandler.idle_closed.clear()
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
            + "/crab/e2e-concurrent-push/run/git_object_catalog_db/manifest/current",
            data=b"payload",
            method="PUT",
        )

        with urllib.request.urlopen(request) as response:
            body = response.read()

        self.assertEqual(body, b"payload")
        self.assertEqual(
            self.proxy.snapshot()["classes"],
            {"git_object_catalog_db/manifest:put": 1},
        )

    def test_reuses_client_and_upstream_connections_without_hiding_requests(self) -> None:
        endpoint = urllib.parse.urlsplit(self.proxy.url)
        client = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=2)
        responses = []
        try:
            for method, body in [("GET", None), ("HEAD", None), ("PUT", b"payload")]:
                client.request(method, "/crab/e2e-concurrent-push/run/packs/one", body=body)
                response = client.getresponse()
                responses.append((response.status, response.will_close, response.read()))
        finally:
            client.close()
        self.assertEqual(
            responses, [(200, False, b""), (200, False, b""), (200, False, b"payload")]
        )
        self.assertEqual(len(set(UpstreamHandler.request_ports)), 1)
        self.assertEqual(self.proxy.snapshot()["requests"], 3)

    @unittest.skipUnless(os.name == "posix", "requires POSIX descriptor duplication")
    def test_reuses_upstream_socket_above_select_descriptor_limit(self) -> None:
        import fcntl
        import resource

        if resource.getrlimit(resource.RLIMIT_NOFILE)[0] <= 1024:
            self.skipTest("process descriptor limit does not permit the reproduction")
        connect = http.client.HTTPConnection.connect
        upstream_port = self.upstream.server_port
        descriptors = []

        def connect_with_high_descriptor(connection):
            connect(connection)
            if connection.port == upstream_port:
                descriptor = fcntl.fcntl(connection.sock.fileno(), fcntl.F_DUPFD, 1024)
                replacement = socket.socket(fileno=descriptor)
                replacement.settimeout(connection.sock.gettimeout())
                connection.sock.close()
                connection.sock = replacement
                descriptors.append(descriptor)

        endpoint = urllib.parse.urlsplit(self.proxy.url)
        client = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=2)
        responses = []
        with patch.object(http.client.HTTPConnection, "connect", connect_with_high_descriptor):
            try:
                for _ in range(2):
                    client.request("GET", "/crab/e2e-concurrent-push/run/one")
                    response = client.getresponse()
                    responses.append((response.status, response.read()))
            finally:
                client.close()
        self.assertEqual(responses, [(200, b""), (200, b"")])
        self.assertEqual(len(descriptors), 1)
        self.assertGreaterEqual(descriptors[0], 1024)
        self.assertEqual(len(set(UpstreamHandler.request_ports)), 1)
        self.assertEqual(self.proxy.snapshot()["proxy_errors"], {})

    def test_distinguishes_proxy_transport_failure_from_upstream_502(self) -> None:
        self.proxy.trace_paths = True
        UpstreamHandler.get_status = 502
        for disconnected in [False, True]:
            with self.subTest(disconnected=disconnected):
                UpstreamHandler.disconnect_get = disconnected
                before = self.proxy.snapshot()
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    urllib.request.urlopen(self.proxy.url + "/crab/e2e-concurrent-push/run/one")
                self.assertEqual(raised.exception.code, 502)
                raised.exception.close()
                delta = RequestCountingProxy.delta(before, self.proxy.snapshot())
                self.assertEqual(
                    delta["proxy_errors"], {"RemoteDisconnected": 1} if disconnected else {}
                )
                self.assertEqual(
                    delta["paths"][0].get("proxy_error"),
                    "RemoteDisconnected" if disconnected else None,
                )

    def test_reconnects_idle_closed_upstream_without_manufacturing_retry(self) -> None:
        UpstreamHandler.close_idle_get = True
        endpoint = urllib.parse.urlsplit(self.proxy.url)
        client = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=2)
        responses = []
        try:
            client.request("GET", "/crab/e2e-concurrent-push/run/first")
            response = client.getresponse()
            responses.append((response.status, response.read()))
            self.assertTrue(UpstreamHandler.idle_closed.wait(timeout=2))
            UpstreamHandler.close_idle_get = False
            client.request("GET", "/crab/e2e-concurrent-push/run/second")
            response = client.getresponse()
            responses.append((response.status, response.read()))
        finally:
            client.close()

        self.assertEqual(responses, [(200, b""), (200, b"")])
        self.assertEqual(len(set(UpstreamHandler.request_ports)), 2)
        snapshot = self.proxy.snapshot()
        self.assertEqual(snapshot["requests"], 2)
        self.assertEqual(snapshot["proxy_errors"], {})

    def test_lightweight_snapshot_uses_path_cursor_without_copying_history(self) -> None:
        self.proxy.trace_paths = True
        before = self.proxy.snapshot(include_paths=False)
        request = urllib.request.Request(
            self.proxy.url
            + "/crab/e2e-concurrent-push/run/git_object_catalog_db/manifest/current",
            data=b"payload",
            method="PUT",
        )

        with urllib.request.urlopen(request):
            pass

        after = self.proxy.snapshot(include_paths=False)

        self.assertEqual(before["path_count"], 0)
        self.assertEqual(after["path_count"], 1)
        self.assertEqual(
            RequestCountingProxy.delta(before, after)["requests"],
            1,
        )
        [request] = self.proxy.paths_since(before["path_count"])
        self.assertEqual(
            {key: value for key, value in request.items() if key != "elapsed_ms"},
            {
                "method": "PUT",
                "operation": "put",
                "category": "git_object_catalog_db/manifest",
                "status": 200,
                "key": "git_object_catalog_db/manifest/current",
                "range": None,
            },
        )
        self.assertIsInstance(request["elapsed_ms"], int)
        self.assertGreaterEqual(request["elapsed_ms"], 0)

    def test_preserves_head_content_length(self) -> None:
        request = urllib.request.Request(
            self.proxy.url + "/crab/e2e-concurrent-push/run/packs/pack.idx",
            method="HEAD",
        )

        with urllib.request.urlopen(request) as response:
            content_length = response.headers["Content-Length"]

        self.assertEqual(content_length, "123")

    def test_preserves_early_precondition_response_during_large_put(self) -> None:
        UpstreamHandler.put_status = 412
        UpstreamHandler.reject_put_before_body = True
        body = b"x" * (32 * 1024 * 1024)
        request = urllib.request.Request(
            self.proxy.url + "/crab/.crab/xorbs/aa/content-address",
            data=body,
            method="PUT",
        )

        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.urlopen(request)

        self.assertEqual(raised.exception.code, 412)
        raised.exception.close()
        snapshot = self.proxy.snapshot()
        self.assertEqual(snapshot["statuses"], {"4xx": 1})
        self.assertEqual(snapshot["request_body_bytes"], len(body))

    def test_streamed_rejection_drains_only_unread_client_bytes(self) -> None:
        UpstreamHandler.put_status = 412
        UpstreamHandler.reject_put_before_body = True
        body = b"x" * (96 * 1024 * 1024)
        endpoint = urllib.parse.urlsplit(self.proxy.url)
        client = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=5)
        responses = []
        try:
            client.request("PUT", "/crab/.crab/xorbs/aa/content-address", body=body)
            response = client.getresponse()
            responses.append((response.status, response.read()))
            # Reusing the client proves the rejected upload was consumed exactly,
            # without waiting for bytes from or eating the subsequent request.
            client.request("GET", "/crab/e2e-concurrent-push/run/next")
            response = client.getresponse()
            responses.append((response.status, response.read()))
        finally:
            client.close()

        self.assertEqual(responses, [(412, b""), (200, b"")])
        snapshot = self.proxy.snapshot()
        self.assertEqual(snapshot["request_body_bytes"], len(body))
        self.assertEqual(snapshot["proxy_errors"], {})

    def test_list_uses_query_prefix_for_repository_category(self) -> None:
        request = urllib.request.Request(
            self.proxy.url
            + "/crab?list-type=2&prefix=e2e-concurrent-push%2Frun%2Fgit_object_catalog_db%2Fmanifest%2F",
            method="GET",
        )

        with urllib.request.urlopen(request):
            pass

        self.assertEqual(
            self.proxy.snapshot()["classes"],
            {"git_object_catalog_db/manifest:list": 1},
        )

    def assert_ref_journal_gate_waits(
        self,
        boundary: str,
        path: str,
    ) -> None:
        self.proxy.arm_ref_journal_gate(boundary)
        result: list[bytes] = []

        def put_ref_journal_object() -> None:
            request = urllib.request.Request(
                self.proxy.url + path,
                data=b"journal-object",
                method="PUT",
            )
            with urllib.request.urlopen(request) as response:
                result.append(response.read())

        request = threading.Thread(target=put_ref_journal_object)
        request.start()

        self.assertTrue(self.proxy.wait_for_ref_journal_gate(2))
        time.sleep(0.05)
        self.assertTrue(request.is_alive())
        self.proxy.release_ref_journal_gate()
        request.join(timeout=2)
        self.assertEqual(result, [b"journal-object"])

    def test_active_marker_gate_waits_after_upstream_commit(self) -> None:
        self.assert_ref_journal_gate_waits(
            "active-marker",
            "/crab/e2e-concurrent-push/run/refs/journal/active/abc.json",
        )

    def test_prepared_head_gate_waits_after_upstream_write(self) -> None:
        self.assert_ref_journal_gate_waits(
            "prepared-head",
            "/crab/e2e-concurrent-push/run/refs/journal/heads/abc.json",
        )

    def test_v2_capsule_gate_waits_before_ref_visibility(self) -> None:
        self.assert_ref_journal_gate_waits(
            "prepared-head",
            "/crab/e2e-concurrent-push/run/v2/capsules/aa/capsule",
        )

    def test_v2_ref_gate_waits_after_ref_visibility(self) -> None:
        self.assert_ref_journal_gate_waits(
            "active-marker",
            "/crab/e2e-concurrent-push/run/v2/refs/heads/abc.json",
        )

    def assert_active_marker_fault(self, phase: str, forwarded: bool) -> None:
        self.proxy.arm_ref_journal_fault("active-marker", phase, attempts=1)
        request = urllib.request.Request(
            self.proxy.url
            + "/crab/e2e-concurrent-push/run/refs/journal/active/abc.json",
            data=b"active-marker",
            method="PUT",
        )

        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.urlopen(request)

        self.assertEqual(raised.exception.code, 503)
        raised.exception.close()
        self.assertTrue(self.proxy.wait_for_ref_journal_fault(2))
        self.assertEqual(UpstreamHandler.put_bodies, [b"active-marker"] if forwarded else [])

    def test_active_marker_fault_before_upstream_does_not_commit(self) -> None:
        self.assert_active_marker_fault("before-upstream", forwarded=False)

    def test_active_marker_fault_after_upstream_loses_committed_response(self) -> None:
        self.assert_active_marker_fault("after-upstream", forwarded=True)

    def test_after_upstream_fault_does_not_mask_rejected_write(self) -> None:
        UpstreamHandler.put_status = 412
        self.proxy.arm_ref_journal_fault("active-marker", "after-upstream", attempts=1)
        request = urllib.request.Request(
            self.proxy.url
            + "/crab/e2e-concurrent-push/run/refs/journal/active/abc.json",
            data=b"conflicting-marker",
            method="PUT",
        )

        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.urlopen(request)

        self.assertEqual(raised.exception.code, 412)
        raised.exception.close()
        self.assertFalse(self.proxy.wait_for_ref_journal_fault(0))

    def test_internal_lock_category_retains_only_bounded_resource(self) -> None:
        category = store_category("locks/internal/git-manifest/lock/clock")

        self.assertEqual(category, "locks/internal/git-manifest")


if __name__ == "__main__":
    unittest.main()
