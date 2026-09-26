"""Exercise scheduled arrivals against a real, controllable HTTP service."""

import io
import json
import tempfile
import threading
import time
import unittest
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

import load
import qualify


class ImageProvenanceTests(unittest.TestCase):
    def test_missing_or_malformed_revision_cannot_qualify(self):
        for labels in (None, {}, {"org.opencontainers.image.revision": "main"}):
            image = {"Config": {"Labels": labels}}
            with self.subTest(labels=labels), \
                    patch.object(qualify, "command", return_value=json.dumps([image])), \
                    self.assertRaisesRegex(RuntimeError, "source revision"):
                qualify.image_provenance("candidate:local")

    def test_pinning_removes_mutable_tags_and_rejects_a_different_running_image(self):
        image = {
            "Id": "sha256:" + "1" * 64,
            "Os": "linux", "Architecture": "arm64",
            "Config": {"Labels": {"org.opencontainers.image.revision": "a" * 40}},
        }
        with tempfile.TemporaryDirectory() as state:
            path = qualify.render(Path(state), "crab-cell-issue-pin-test", 18080, 18100, 19010)
            with patch.object(qualify, "command", return_value=json.dumps([image])):
                qualify.pin_image(path, "a" * 40)
            deployment = json.loads(path.read_text())
            for name in ["release-init", "repository-init", "fleet-net"] + [qualify.node_name(i) for i in range(1, 21)]:
                service = deployment["services"][name]
                self.assertEqual(service["image"], image["Id"])
                self.assertEqual(service["pull_policy"], "never")
                self.assertNotIn("build", service)
            self.assertEqual(deployment["services"]["release-init"]["command"][-1], image["Id"])
            inspected = f"1000000000 1073741824 1073741824 healthy sha256:{'2' * 64}"
            with patch.object(qualify, "compose", return_value="node-container"), \
                    patch.object(qualify, "command", return_value=inspected), \
                    self.assertRaisesRegex(RuntimeError, "pinned server image"):
                qualify.prove_node(path, (), 1)

    def test_skip_build_refuses_wrong_source_before_starting_nodes(self):
        source = "a" * 40
        image = {
            "Id": "sha256:" + "1" * 64,
            "Os": "linux", "Architecture": "arm64",
            "Config": {"Labels": {"org.opencontainers.image.revision": "b" * 40}},
        }

        def command(*args):
            if args[0] == "git":
                return source if "rev-parse" in args else ""
            if args[:3] == ("docker", "image", "inspect"):
                return json.dumps([image])
            return ""

        with tempfile.TemporaryDirectory() as state, \
                patch.object(qualify.sys, "argv", [
                    "qualify.py", "--state", state, "--project", "crab-cell-issue-source-test", "--skip-build",
                ]), \
                patch.object(qualify, "command", side_effect=command), \
                patch.object(qualify, "run_stage", side_effect=AssertionError("started wrong-source node")):
            with self.assertRaisesRegex(RuntimeError, "source revision"):
                qualify.main()


class LoadTests(unittest.TestCase):
    def setUp(self):
        self.delay = 0
        self.lose_response = False
        self.corrupt_read = False
        self.read_status = 200
        self.receipts = {}
        self.requests = []
        self.active = 0
        self.peak = 0
        self.lock = threading.Lock()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def respond(self, status, body):
                self.send_response(status)
                self.send_header("X-Crab-Fleet-Entry", "127.0.0.1:8201")
                self.end_headers()
                self.wfile.write(json.dumps(body).encode())

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with fixture.lock:
                    fixture.active += 1
                    fixture.peak = max(fixture.peak, fixture.active)
                    fixture.requests.append(body["request_id"])
                    fresh = body["request_id"] not in fixture.receipts
                    value = fixture.receipts.setdefault(body["request_id"], {
                        "number": len(fixture.receipts) + 1, "title": body["title"],
                    })
                time.sleep(fixture.delay)
                self.respond(503 if fresh and fixture.lose_response else 201, value)
                with fixture.lock:
                    fixture.active -= 1

            def do_GET(self):
                number = int(self.path.rsplit("/", 1)[1])
                with fixture.lock:
                    value = next((value for value in fixture.receipts.values() if value["number"] == number), None)
                if value is None:
                    self.respond(404, {})
                    return
                self.respond(fixture.read_status, {**value, "title": "corrupted"} if fixture.corrupt_read else value)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever)
        self.worker.start()
        self.gateway = f"http://127.0.0.1:{self.server.server_port}"
        self.raw = io.StringIO()

    def tearDown(self):
        self.server.shutdown()
        self.worker.join()
        self.server.server_close()

    def test_overload_keeps_the_arrival_schedule_and_bounds_in_flight_work(self):
        self.delay = 0.2
        summary, samples = load.scheduled_load(
            self.gateway, 3, load.Workload(4, 20, 1, 1, 0), "bounded", self.raw,
        )
        self.assertEqual(summary["offered_pairs"], 20)
        self.assertGreater(summary["outcomes"].get("client_capacity", 0), 0)
        self.assertEqual(summary["peak_in_flight"], 1)
        self.assertEqual(self.peak, 1)
        self.assertEqual(Counter(sample["cell"] for sample in samples), {1: 5, 2: 5, 3: 5, 4: 5})
        self.assertEqual(len(self.raw.getvalue().splitlines()), 20)
        self.assertGreaterEqual(summary["elapsed_seconds"], 1)

    def test_uncertain_write_retries_the_same_receipt_and_checks_readback(self):
        self.lose_response = True
        summary, samples = load.scheduled_load(
            self.gateway, 3, load.Workload(1, 1, 0.1, 1, 0), "retry", self.raw,
        )
        self.assertEqual(summary["outcomes"], {"success": 1})
        self.assertEqual(len(set(self.requests)), 1)
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(len(self.receipts), 1)
        self.assertEqual(samples[0]["operations"][0]["retry_reasons"], [503])
        self.assertGreater(samples[0]["scheduled_latency_ms"], 100)

    def test_acknowledged_readback_mismatch_stops_new_arrivals_and_retains_evidence(self):
        self.corrupt_read = True
        summary, samples = load.scheduled_load(
            self.gateway, 3, load.Workload(1, 5, 2, 1, 0), "corrupt", self.raw,
        )
        self.assertTrue(summary["stopped_on_invariant"])
        self.assertLess(summary["offered_pairs"], summary["planned_pairs"])
        self.assertEqual(samples[0]["outcome"], "contract_error")
        self.assertIn("acknowledged", json.loads(self.raw.getvalue().splitlines()[0]))

    def test_hot_share_preserves_a_fixed_cell_count(self):
        workload = load.Workload(5, 10, 10, 8, 0.8)
        self.assertEqual(Counter(workload.cell(i) for i in range(100)), {1: 80, 2: 5, 3: 5, 4: 5, 5: 5})
        self.assertEqual(load.percentiles([]), {"count": 0})
        for rate, duration in [(float("nan"), 1), (1, float("inf")), (1e300, 1e300), (0, 1)]:
            with self.subTest(rate=rate, duration=duration), self.assertRaises(ValueError):
                load.Workload(5, rate, duration, 8, 0)

    def test_recovery_checks_earlier_acknowledgements_before_restarting_the_owner(self):
        samples = [load.load_pair(self.gateway, 3, 1, i, "recover", time.monotonic()) for i in range(3)]
        proof = load.verify_acknowledged(self.gateway, 3, samples)
        self.assertEqual(proof["verified"], 3)
        self.assertEqual(proof["by_cell"], {1: 3})
        before = {"owner": {"session": "old"}, "root": {"commit_sequence": 3, "txid": 3}}
        after = {**before, "owner": {"session": "new"}}
        calls = []

        def compose(*args):
            calls.append(args)
            if args[-1] == "metrics":
                return "crab_cell_node_log_uncovered_bytes 0\n"
            if "status" in args:
                return json.dumps(after)
            if "up" in args:
                # A restarted old owner can hide a missing recovered result.
                # Verification must fail before this restoration happens.
                self.receipts[samples[0]["request_id"]] = first
            return ""

        first = self.receipts[samples[0]["request_id"]]
        for corruption in ("missing", "changed"):
            with self.subTest(corruption=corruption):
                if corruption == "missing":
                    del self.receipts[samples[0]["request_id"]]
                else:
                    self.receipts[samples[0]["request_id"]] = {**first, "title": "changed"}
                calls.clear()
                with patch.object(load, "status", return_value=before), \
                        patch.object(load, "compose", side_effect=compose), \
                        self.assertRaisesRegex(RuntimeError, samples[0]["request_id"]):
                    load.recover_owner(Path("fixture"), (), self.gateway, 3, "node-03",
                                       before, samples[-1]["acknowledged"], 1, samples)
                self.assertIn("kill", calls[1])
                self.assertIn("up", calls[-1])
        with patch.object(load, "status", return_value=before), \
                patch.object(load, "compose", side_effect=compose):
            recovered = load.recover_owner(Path("fixture"), (), self.gateway, 3, "node-03",
                                           before, samples[-1]["acknowledged"], 1, samples)
        self.assertEqual(recovered["acknowledgements"]["verified"], 3)

    def test_missing_acknowledged_issue_stops_new_arrivals(self):
        self.read_status = 404
        summary, samples = load.scheduled_load(
            self.gateway, 3, load.Workload(1, 5, 2, 1, 0), "missing", self.raw,
        )
        self.assertTrue(summary["stopped_on_invariant"])
        self.assertLess(summary["offered_pairs"], summary["planned_pairs"])
        self.assertEqual(samples[0]["error"], "acknowledged issue disappeared")
        self.assertIn("acknowledged", json.loads(self.raw.getvalue().splitlines()[0]))

    def test_late_scheduler_records_missed_arrivals_without_a_catchup_burst(self):
        clock = [0.0]

        def delayed_sleep(seconds):
            clock[0] += seconds + 0.05

        with patch.object(load.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(load.time, "sleep", side_effect=delayed_sleep):
            summary, _ = load.scheduled_load(
                self.gateway, 3, load.Workload(3, 100, 0.03, 4, 0), "late", self.raw,
            )
        self.assertEqual(summary["outcomes"], {"scheduler_late": 3})
        self.assertEqual(summary["admitted_pairs"], 0)
        self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main()
