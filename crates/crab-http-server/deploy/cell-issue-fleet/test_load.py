"""Exercise scheduled arrivals against a real, controllable HTTP service."""

import io
import json
import threading
import time
import unittest
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

import load


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
                    value = next(value for value in fixture.receipts.values() if value["number"] == number)
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
