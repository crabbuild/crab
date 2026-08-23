#!/usr/bin/env python3
"""Tests for concurrent-push HTTP request metering."""

from __future__ import annotations

import argparse
import json
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

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_concurrent_push_smoke import (
    ConcurrentPushSmoke,
    RequestCountingProxy,
    locator_requests_per_success,
    parse_stage_counts,
    push_failure_stages,
    store_category,
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
                "git_locator_db/manifest": 80,
                "git_locator_db/compacted": 20,
                "packs": 400,
            },
        }

        self.assertEqual(locator_requests_per_success(snapshot), 25.0)

    def test_requires_a_successful_push(self) -> None:
        self.assertIsNone(
            locator_requests_per_success(
                {"successful_pushes": 0, "categories": {"git_locator_db/wal": 1}}
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
    put_bodies: list[bytes] = []
    put_status = 200

    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_HEAD(self) -> None:
        self.send_response(200)
        self.send_header("Content-Length", "123")
        self.end_headers()

    def do_PUT(self) -> None:
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
        UpstreamHandler.put_status = 200
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

    def test_list_uses_query_prefix_for_repository_category(self) -> None:
        request = urllib.request.Request(
            self.proxy.url
            + "/crab?list-type=2&prefix=e2e-concurrent-push%2Frun%2Fgit_locator_db%2Fmanifest%2F",
            method="GET",
        )

        with urllib.request.urlopen(request):
            pass

        self.assertEqual(
            self.proxy.snapshot()["classes"],
            {"git_locator_db/manifest:list": 1},
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
