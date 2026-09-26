"""Prove trace joins refuse incomplete or ambiguous acknowledgement attribution."""

import copy
import json
import unittest

import action_traces as traces


class TraceTests(unittest.TestCase):
    def setUp(self):
        identity = {"cell": "CellId(11)", "incarnation": "IncarnationId(22)", "mutation_request_id": "RequestId(33)"}
        http = {"request_id": "http-attempt", "node": "node-01"}
        owner = {**identity, "node": "node-02", "owner_session": "SessionId(44)"}
        self.events = [
            {**http, "event": "application_submission", "submission_id": "submission"},
            {**http, **identity, "event": "cell_invocation_completed", "outcome": "committed", "commit_sequence": 7, "elapsed_us": 20},
            {**http, "event": "http_response_ready", "status": 201, "elapsed_us": 30},
            {**owner, "event": "cell_execution_started", "actor_queue_us": 1},
            {**owner, "event": "cell_worker_started", "worker_queue_us": 2},
            {**owner, "event": "cell_worker_completed", "worker_execute_us": 3, "succeeded": True},
            {**owner, "event": "cell_execution_completed", "worker_round_trip_us": 6, "succeeded": True},
            {**owner, "event": "cell_capture_completed", "capture_ns": 1000, "succeeded": True},
            {**owner, "event": "cell_proof_completed", "proof_wait_us": 4, "commit_sequence": 7, "succeeded": True},
            {**owner, "event": "cell_command_response", "response_us": 12, "confirmation_us": 1, "commit_sequence": 7, "source": "Object"},
        ]
        self.samples = [{
            "request_id": "submission", "acknowledged": {"number": 1},
            "operations": [{"operation": "write", "http_request_id": "http-attempt", "entry": "node-01", "latency_ms": 0.04, "attempts": [{"status": 201, "http_request_id": "http-attempt"}]}],
        }]

    def test_join_binds_submission_receipt_actual_owner_and_proof(self):
        joined = traces.join(self.samples, self.events)
        self.assertEqual((joined[0]["entry"], joined[0]["owner"], joined[0]["proof"]), ("node-01", "node-02", "object"))
        self.assertEqual(joined[0]["phases"]["worker_execute_us"], 3)
        self.assertEqual(joined[0]["http_latency_ms"], 0.04)

    def test_each_missing_phase_fails_attribution(self):
        for omitted in range(len(self.events)):
            with self.subTest(omitted=self.events[omitted]["event"]):
                with self.assertRaises(ValueError):
                    traces.join(self.samples, self.events[:omitted] + self.events[omitted + 1:])

    def test_missing_identity_or_timing_has_an_actionable_error(self):
        for index, field in ((1, "cell"), (1, "incarnation"), (1, "elapsed_us"), (5, "worker_execute_us"), (9, "response_us")):
            with self.subTest(field=field):
                events = copy.deepcopy(self.events)
                del events[index][field]
                with self.assertRaisesRegex(ValueError, "incomplete.*" + field):
                    traces.join(self.samples, events)

    def test_acknowledgement_must_name_its_successful_http_attempt(self):
        for attempts in ([], [{"status": 503, "http_request_id": "http-attempt"}], [{"status": 201, "http_request_id": "different"}]):
            with self.subTest(attempts=attempts):
                samples = copy.deepcopy(self.samples)
                samples[0]["operations"][0]["attempts"] = attempts
                with self.assertRaisesRegex(ValueError, "successful HTTP attempt"):
                    traces.join(samples, self.events)

    def test_ambiguous_or_mismatched_proofs_are_never_guessed(self):
        for field, value in (("commit_sequence", 8), ("source", "Local"), ("owner_session", "SessionId(other)"), ("cell", "CellId(other)"), ("incarnation", "IncarnationId(other)")):
            with self.subTest(field=field):
                events = copy.deepcopy(self.events)
                events[-1][field] = value
                with self.assertRaises(ValueError):
                    traces.join(self.samples, events)
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            traces.join(self.samples, self.events + [self.events[-1]])

    def test_recorded_reply_does_not_invent_a_new_proof_or_capture(self):
        events = [event for event in copy.deepcopy(self.events) if event["event"] not in ("cell_capture_completed", "cell_proof_completed")]
        events[-1]["source"] = "Recorded"
        joined = traces.join(self.samples, events)[0]
        self.assertNotIn("proof_wait_us", joined["phases"])
        self.assertEqual(joined["captures"], [])

    def test_text_formatter_spans_colors_and_booleans_preserve_the_join(self):
        events = []
        for original in self.events:
            event = dict(original)
            node = event.pop("node")
            prefix = f'http_request{{request_id={event.pop("request_id")}}}: ' if "request_id" in event else ""
            fields = " ".join(f"{key}={json.dumps(value)}" for key, value in event.items())
            events.extend(traces.parse_log(f"2026-09-26T12:00:00Z \x1b[34mDEBUG\x1b[0m {prefix}{fields}", node))
        self.assertEqual(traces.join(self.samples, events), traces.join(self.samples, self.events))
        with self.assertRaisesRegex(ValueError, "invalid"):
            traces.parse_log('event="cell_command_response" response_us=-1', "node-01")


if __name__ == "__main__":
    unittest.main()
