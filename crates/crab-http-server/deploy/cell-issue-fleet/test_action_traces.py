"""Prove trace joins refuse incomplete or ambiguous acknowledgement attribution."""

import copy
import json
import unittest

import action_traces as traces


class TraceTests(unittest.TestCase):
    def setUp(self):
        identity = {"cell": "CellId(11)", "incarnation": "IncarnationId(22)", "mutation_request_id": "RequestId(33)", "module": "repository", "operation_id": 1}
        http = {"request_id": "http-attempt", "node": "node-01"}
        owner = {**identity, "node": "node-02", "owner_session": "SessionId(44)"}
        self.events = [
            {**http, "event": "application_submission", "submission_id": "submission"},
            {**http, **identity, "event": "cell_invocation_completed", "outcome": "committed", "commit_sequence": 7, "elapsed_us": 20},
            {**http, "event": "http_response_ready", "status": 201, "elapsed_us": 30},
            {**http, "event": "http_authentication_completed", "elapsed_us": 1},
            {**http, "event": "http_archive_check_completed", "elapsed_us": 2},
            {**http, "event": "repository_route_completed", "action": "repository.issue.create", "elapsed_us": 3, "succeeded": True},
            {**http, **identity, "event": "cell_command_prepared", "elapsed_us": 1},
            {**http, "event": "application_response_prepared", "elapsed_us": 2},
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
        for name, field in (("cell_invocation_completed", "cell"), ("cell_invocation_completed", "incarnation"), ("cell_invocation_completed", "elapsed_us"), ("cell_worker_completed", "worker_execute_us"), ("cell_command_response", "response_us"), ("cell_command_prepared", "elapsed_us")):
            with self.subTest(field=field):
                events = copy.deepcopy(self.events)
                del next(event for event in events if event["event"] == name)[field]
                with self.assertRaisesRegex(ValueError, "incomplete.*" + field):
                    traces.join(self.samples, events)

    def test_preparation_must_match_the_acknowledged_mutation(self):
        for field in ("cell", "incarnation", "mutation_request_id", "module", "operation_id"):
            with self.subTest(field=field):
                events = copy.deepcopy(self.events)
                next(event for event in events if event["event"] == "cell_command_prepared")[field] = 2 if field == "operation_id" else "different"
                with self.assertRaisesRegex(ValueError, "prepared route"):
                    traces.join(self.samples, events)

    def test_command_route_is_separate_from_nested_archive_and_enrichment_routes(self):
        events = self.events + [{"request_id": "http-attempt", "node": "node-01", "event": "repository_route_completed", "action": "repository.read", "elapsed_us": 7, "succeeded": True}]
        joined = traces.join(self.samples, events)[0]
        self.assertEqual(joined["phases"]["command_route_us"], 3)
        self.assertEqual([route["elapsed_us"] for route in joined["repository_routes"]], [3, 7])
        for field, value in (("succeeded", False), ("node", "different")):
            with self.subTest(field=field):
                invalid = copy.deepcopy(events)
                next(event for event in invalid if event.get("action") == "repository.issue.create")[field] = value
                with self.assertRaises(ValueError):
                    traces.join(self.samples, invalid)

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

    def test_phase_summary_keeps_populations_and_subtracts_only_matched_timings(self):
        remote = traces.join(self.samples, self.events)[0]
        remote["phases"].update(http_response_ready_us=10_000, client_invocation_us=9_000)
        local = copy.deepcopy(remote)
        local.update(owner=local["entry"], proof="recorded", captures=[])
        local["phases"].update(http_response_ready_us=20_000, client_invocation_us=1_000)
        del local["phases"]["proof_wait_us"]
        summary = traces.summarize([remote, local])
        self.assertEqual(summary["all"]["http_outside_invocation"]["p50_ms"], 1)
        self.assertEqual(summary["all"]["http_outside_invocation"]["p99_ms"], 19)
        self.assertEqual(summary["all"]["proof_wait"]["count"], 1)
        self.assertEqual(summary["local"]["capture"], {"count": 0})
        self.assertEqual(summary["forwarded"], summary["object"])
        self.assertEqual(summary["fleet"]["http"], {"count": 0})
        self.assertEqual(traces.summarize([])["all"]["http"], {"count": 0})

    def test_phase_summary_refuses_an_invocation_longer_than_its_http_request(self):
        action = traces.join(self.samples, self.events)[0]
        action["phases"]["http_response_ready_us"] = 1
        with self.assertRaisesRegex(ValueError, "nested HTTP"):
            traces.summarize([action])

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

    def test_publication_is_joined_by_owner_cell_incarnation_and_sequence(self):
        start, end = self.publication_events()
        action = traces.join(self.samples, self.events + [start, end])[0]
        self.assertEqual(action["publication"]["status"], "completed")
        self.assertEqual(action["phases"]["publication_work_us"], 8_000)
        self.assertEqual(traces.summarize([action])["publication_states"], {"completed": 1})
        for field in ("node", "cell", "incarnation", "commit_sequence"):
            with self.subTest(field=field):
                unrelated = copy.deepcopy([start, end])
                for event in unrelated:
                    event[field] = 8 if field == "commit_sequence" else "different"
                unmatched = traces.join(self.samples, self.events + unrelated)[0]
                self.assertEqual(unmatched["publication"], {"status": "not_observed"})
                self.assertNotIn("publication_lag_us", unmatched["phases"])

    def test_incomplete_or_failed_publication_is_not_a_success_latency_sample(self):
        start, end = self.publication_events()
        fleet = copy.deepcopy(self.events)
        fleet[-1]["source"] = "Fleet"
        for events, state in (([], "not_observed"), ([start], "started"),
                              ([start, {**end, "succeeded": False}], "failed")):
            with self.subTest(state=state):
                action = traces.join(self.samples, fleet + events)[0]
                self.assertEqual(action["publication"]["status"], state)
                self.assertNotIn("publication_lag_us", action["phases"])
                summary = traces.summarize([action])
                self.assertEqual(summary["publication_states"], {state: 1})
                self.assertEqual(summary["all"]["publication_lag"], {"count": 0})

    def test_publication_refuses_ambiguous_or_impossible_pairs(self):
        start, end = self.publication_events()
        for events in ([end], [start, start, end], [start, end, end],
                       [start, {**end, "publication_lag_ms": 1}]):
            with self.subTest(events=events), self.assertRaises(ValueError):
                traces.join(self.samples, self.events + events)

    def test_recorded_replay_does_not_reuse_the_original_publication_latency(self):
        events = [event for event in copy.deepcopy(self.events)
                  if event["event"] not in ("cell_capture_completed", "cell_proof_completed")]
        events[-1]["source"] = "Recorded"
        action = traces.join(self.samples, events + self.publication_events())[0]
        self.assertEqual(action["publication"], {"status": "recorded"})
        self.assertNotIn("publication_lag_us", action["phases"])

    def publication_events(self):
        identity = {key: self.events[-1][key] for key in ("node", "cell", "incarnation", "commit_sequence")}
        return [
            {**identity, "event": "cell_publication_started", "queue_wait_ms": 2,
             "pending_publications": 3, "publication_bytes": 1_024, "root_sequence_lag": 1},
            {**identity, "event": "cell_publication_completed", "publication_lag_ms": 10, "succeeded": True},
        ]


if __name__ == "__main__":
    unittest.main()
