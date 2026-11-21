#!/usr/bin/env python3
"""Qualification driver regressions with native Git and real HTTP transport."""
import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
from urllib.parse import parse_qs, urlsplit

from qualify_v2_read_service import (GitOracle, Transport, compare_baseline, load_headers, main,
                                     percentiles, read_journal, run_case, server_timings, summarize, validate_profile,
                                     validate_report, wire_workloads, workload_identity)


class ReadServiceTests(unittest.TestCase):
    def setUp(self):
        root = Path.home() / "Workspace" / "CrabData" / "read-service-driver-tests"
        root.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.git("init", "-b", "main")
        self.body = b"fn target() {}\n"
        stream = (b"commit refs/heads/main\ncommitter Qualification <qualification@example.invalid> 1700000000 +0000\n"
                  b"data 0\n\nM 100644 inline target.rs\ndata " + str(len(self.body)).encode()
                  + b"\n" + self.body + b"\ndone\n")
        subprocess.run(["git", "-C", str(self.source), "fast-import", "--quiet"], input=stream,
                       check=True, capture_output=True)
        self.head = self.git("rev-parse", "HEAD").decode().strip()
        self.workload = {"name":"source", "action":"file", "class":"interactive", "paths":["target.rs"]}
        self.binding = {"source_revision":self.head, "source_patch_sha256":"1" * 64,
                        "server_binary_sha256":"2" * 64, "compiler":"fixture", "build_profile":"fixture"}
        self.profile = {"schema":1, "provider":"fixture", "region":"loopback",
                        "source_repository":str(self.source), "build_receipt":str(self.root / "build.json"),
                        "resources":{"cpu":1, "memory_bytes":1024**3, "disk_bytes":1024**3},
                        "workloads":[self.workload], "targets":[{"case":"mixed", "workload":"source"}],
                        "cases":[{"name":"mixed", "cache":"warm_objects", "pattern":"same_key", "clients":2,
                                  "duration_seconds":.1, "warmup_seconds":0, "timeout_seconds":1,
                                  "workloads":["source"], "state_receipt":str(self.root / "state.json")}],
                        "thresholds":{"minimum_successes":20, "maximum_failure_fraction":0,
                                      "target_p95_ratio":.8, "maximum_control_regression_ratio":1.1,
                                      "maximum_interactive_ratio":2}}

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.source), *args], stderr=subprocess.PIPE)

    def serve(self, status=200, corrupt=False, delay=0, request_id=None):
        owner = self
        self.seen_hosts = []

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.seen_hosts.append(self.headers["Host"])
                if delay:
                    time.sleep(delay)
                parsed = urlsplit(self.path)
                params = parse_qs(parsed.query)
                if parsed.path.endswith("/blob"):
                    body = b"wrong bytes" if corrupt else owner.body
                else:
                    body = json.dumps({"oid":owner.git("rev-parse", "HEAD:target.rs").decode().strip(),
                                       "size":len(owner.body), "text":"incorrect" if corrupt else owner.body.decode(),
                                       "text_truncated":False, "mode":"100644", "classification":"OrdinaryGit",
                                       "commit":params["rev"][0], "generation":1}).encode()
                self.send_response(status)
                if request_id is not None:
                    self.send_header("X-Request-Id", request_id)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_port}"

    def test_real_http_content_is_verified_against_native_git(self):
        url = self.serve()
        oracle = GitOracle(self.source, self.head)
        oracle.prepare(self.workload, "target.rs")
        with (self.root / "journal.jsonl").open("w") as journal:
            elapsed = run_case(self.profile["cases"][0], {"source":self.workload}, [url], {},
                               "owner/repository", oracle, journal)
        records = list(read_journal(self.root / "journal.jsonl"))
        summary = summarize(iter(records), elapsed, self.root)["source"]
        self.assertGreater(summary["successes"], 0)
        self.assertEqual(summary["errors"], 0)
        self.assertTrue(all(item["ttfb_us"] <= item["elapsed_us"] for item in records))
        self.assertTrue(all(item["cache_scope"] == "case_initial_state" for item in records))

    def test_corrupt_success_and_rejected_requests_cannot_look_like_successful_throughput(self):
        oracle = GitOracle(self.source, self.head)
        oracle.prepare(self.workload, "target.rs")
        for status, corrupt in ((200, True), (429, False), (503, False)):
            with self.subTest(status=status):
                url = self.serve(status, corrupt)
                with (self.root / f"journal-{status}.jsonl").open("w") as journal:
                    elapsed = run_case(self.profile["cases"][0], {"source":self.workload}, [url], {},
                                       "owner/repository", oracle, journal)
                summary = summarize(read_journal(self.root / f"journal-{status}.jsonl"),
                                    elapsed, self.root)["source"]
                self.assertEqual(summary["successful_requests_per_second"], 0)
                self.assertEqual(summary["attempts"], summary["errors"] + summary["rejected"])

    def test_raw_downloads_are_verified_without_retaining_response_body(self):
        workload = {**self.workload, "action":"blob"}
        oracle = GitOracle(self.source, self.head)
        oracle.prepare(workload, "target.rs")
        transport = Transport([self.serve()], {}, 1)
        self.addCleanup(transport.close)
        record, body = transport.get(0, "/api/repos/owner/repository/blob", "blob")
        oracle.check(workload, "target.rs", body, record["sha256"], record["bytes"])
        self.assertEqual(body, b"")

    def test_transport_correlates_server_request_ids_without_recording_arbitrary_headers(self):
        identifier = "019aab12-3456-7000-8000-123456789abc"
        for value, expected in ((identifier, identifier), ("arbitrary-header-text", None)):
            with self.subTest(value=value):
                transport = Transport([self.serve(request_id=value)], {}, 1)
                self.addCleanup(transport.close)
                record, _ = transport.get(0, "/api/repos/owner/repository/blob", "blob")
                self.assertEqual(record.get("request_id"), expected)

    def test_client_cancellation_closes_the_pending_socket_and_remains_a_failed_request(self):
        transport = Transport([self.serve(delay=.2)], {}, 1)
        self.addCleanup(transport.close)
        record, _ = transport.get(0, "/api/repos/owner/repository/blob", "blob", cancel_after=.01)
        record.update(phase="measured", workload="source")
        summary = summarize(iter([record]), 1, self.root)["source"]
        self.assertEqual((summary["successes"], summary["errors"], summary["client_cancellations"]), (0, 1, 1))
        self.assertFalse(transport.connections)

    def test_headers_are_private_and_never_accepted_for_plaintext_hosted_reads(self):
        path = self.root / "headers.json"
        path.write_text(json.dumps({"Authorization":"fixture-placeholder"}))
        path.chmod(0o600)
        self.assertEqual(load_headers(path, ["https://fixture.example.invalid"]),
                         {"Authorization":"fixture-placeholder"})
        with self.assertRaisesRegex(ValueError, "loopback"):
            load_headers(path, ["http://fixture.example.invalid"])
        path.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "owner-private"):
            load_headers(path, ["https://fixture.example.invalid"])
        with self.assertRaisesRegex(ValueError, "headers file"):
            load_headers(None, ["https://fixture.example.invalid"])

    def test_profile_rejects_missing_identity_and_unclassified_cache_states(self):
        validate_profile(self.profile)
        for mutate in (lambda value:value.pop("region"),
                       lambda value:value["cases"][0].update(cache="unknown"),
                       lambda value:value.update(headers={"Authorization":"fixture-placeholder"}),
                       lambda value:value.update(public_origin="http://fixture.example.invalid"),
                       lambda value:value.pop("thresholds")):
            value = copy.deepcopy(self.profile)
            mutate(value)
            with self.assertRaises(ValueError):
                validate_profile(value)

    def test_cold_deep_pages_cannot_warm_the_measured_runtime_to_prepare_a_cursor(self):
        workload = {**self.workload, "action":"commits", "page_depth":10}
        case = {**self.profile["cases"][0], "cache":"cold"}
        oracle = GitOracle(self.source, self.head)
        with (self.root / "journal.jsonl").open("w") as journal:
            with self.assertRaisesRegex(ValueError, "prepared before process restart"):
                run_case(case, {"source":workload}, ["http://127.0.0.1:1"], {},
                         "owner/repository", oracle, journal)

    def test_report_rejects_missing_cells_source_binding_and_failed_request_counts(self):
        summary = {"attempts":20, "successes":20, "rejected":0, "errors":0, "oracle_mismatches":0,
                   "offered_requests_per_second":10, "successful_requests_per_second":10,
                   "successful_latency_us":{"p50":1, "p95":2}, "ttfb_us":{"p50":1, "p95":2}}
        report = {"schema":1, "binding":self.binding,
                  "cases":[{"name":"mixed", "cache":"warm_objects", "workloads":{"source":summary}}]}
        self.assertEqual(validate_report(report, self.profile), [])
        mutations = (lambda value:value.update(cases=[]),
                     lambda value:value["binding"].pop("source_revision"),
                     lambda value:value["cases"][0]["workloads"]["source"].update(attempts=21),
                     lambda value:value["cases"][0]["workloads"]["source"]["successful_latency_us"].update(p95=float("nan")),
                     lambda value:value["cases"][0]["workloads"]["source"]["successful_latency_us"].update(p99=3))
        for mutate in mutations:
            value = copy.deepcopy(report)
            mutate(value)
            with self.assertRaises(ValueError):
                validate_report(value, self.profile)

    def test_failure_threshold_is_not_satisfied_by_more_rejections(self):
        report = {"schema":1, "binding":self.binding,
                  "cases":[{"name":"mixed", "cache":"warm_objects", "workloads":{
                      "source":{"attempts":40, "successes":0, "rejected":40, "errors":0, "oracle_mismatches":0,
                                "offered_requests_per_second":1, "successful_requests_per_second":0}}}]}
        self.assertIn("mixed:failure_fraction", validate_report(report, self.profile))

    def test_oracle_mismatches_fail_even_inside_an_overload_envelope(self):
        self.profile["cases"][0]["maximum_failure_fraction"] = .75
        summary = {"attempts":40,"successes":20,"errors":1,"rejected":19,"oracle_mismatches":1,
                   "offered_requests_per_second":10,"successful_requests_per_second":5,
                   "successful_latency_us":{"p50":1,"p95":2},"ttfb_us":{"p50":1,"p95":2}}
        report = {"schema":1,"binding":self.binding,"cases":[{"name":"mixed","cache":"warm_objects",
                  "workloads":{"source":summary}}]}
        self.assertEqual(validate_report(report, self.profile), ["mixed:git_oracle_mismatch"])

    def test_p99_requires_at_least_one_thousand_observations(self):
        self.assertNotIn("p99", percentiles(list(range(999))))
        self.assertEqual(percentiles(list(range(1000)))["p99"], 989)

    def test_journal_statistics_preserve_all_samples_and_exact_quantile_ranks(self):
        path = self.root / "samples.jsonl"
        with path.open("w") as journal:
            journal.write("earlier case is outside the selected range\n")
            start = journal.tell()
            for i in range(1000):
                record = {"workload":"source", "phase":"measured", "status":200, "error":None,
                          "elapsed_us":i + .25, "ttfb_us":i / 2,
                          "server_timing_ms":{"read":i / 1000}}
                journal.write(json.dumps(record) + "\n")
            for status, error in ((429, None), (503, None), (200, "GitOracleMismatch"),
                                  (0, "ClientCancelled")):
                record.update(status=status, error=error, elapsed_us=2000)
                journal.write(json.dumps(record) + "\n")
            for phase in ("warmup", "setup"):
                record.update(phase=phase)
                journal.write(json.dumps(record) + "\n")
            end = journal.tell()
            journal.write("later case is outside the selected range\n")
        summary = summarize(read_journal(path, start, end), 2, self.root)["source"]
        self.assertEqual({key:summary[key] for key in ("attempts", "successes", "rejected", "errors",
                         "oracle_mismatches", "client_cancellations", "warmup_attempts", "setup_attempts")},
                         {"attempts":1004, "successes":1000, "rejected":1, "errors":3,
                          "oracle_mismatches":1, "client_cancellations":1,
                          "warmup_attempts":1, "setup_attempts":1})
        self.assertEqual(summary["successful_latency_us"], {"p50":499.25, "p95":949.25, "p99":989.25})
        self.assertEqual(summary["latency_us"], {"p50":501.25, "p95":953.25, "p99":993.25})
        self.assertEqual(summary["server_timing_ms"], {
            "read":{"samples":1000, "p50":.499, "p95":.949, "p99":.989}})
        self.assertEqual(summary["successful_requests_per_second"], 500)
        self.assertFalse(list(self.root.glob("sample-summary-*")))

    def test_failed_journal_replay_removes_its_temporary_workspace(self):
        path = self.root / "malformed.jsonl"
        path.write_text("not a measurement\n")
        with self.assertRaises(json.JSONDecodeError):
            summarize(read_journal(path), 1, self.root)
        self.assertFalse(list(self.root.glob("sample-summary-*")))

    def test_capture_query_and_serialization_timings_remain_separate(self):
        self.assertEqual(server_timings("open;dur=1.5, read;dur=2, encode;dur=0.25, total;dur=4"),
                         {"open":1.5, "read":2, "encode":.25, "total":4})
        self.assertEqual(server_timings("open;dur=1, read;dur=2, total;dur=3"),
                         {"open":1, "read":2, "total":3})

    def test_baseline_comparison_requires_the_frozen_profile_and_actual_improvement(self):
        summary = {"attempts":20, "successes":20, "rejected":0, "errors":0, "oracle_mismatches":0,
                   "successful_latency_us":{"p50":10, "p95":100}, "ttfb_us":{"p50":1, "p95":2},
                   "offered_requests_per_second":10, "successful_requests_per_second":10}
        baseline = {"schema":1, "phase":"baseline", "binding":self.binding, "workload_sha256":workload_identity(self.profile),
                    "fixture_commit":self.head,
                    "cases":[{"name":"mixed", "cache":"warm_objects", "workloads":{"source":summary}}]}
        candidate = copy.deepcopy(baseline)
        candidate["phase"] = "candidate"
        self.assertIn("mixed:source:p95_regression", compare_baseline(candidate, baseline, self.profile))
        candidate["cases"][0]["workloads"]["source"]["successful_latency_us"]["p95"] = 80
        self.assertEqual(compare_baseline(candidate, baseline, self.profile), [])
        for mutate in (lambda value:value.update(fixture_commit="0" * 40),
                       lambda value:value["binding"].update(compiler="different compiler")):
            different = copy.deepcopy(candidate)
            mutate(different)
            with self.assertRaisesRegex(ValueError, "differs"):
                compare_baseline(different, baseline, self.profile)
        candidate["workload_sha256"] = "changed"
        with self.assertRaisesRegex(ValueError, "profile differs"):
            compare_baseline(candidate, baseline, self.profile)

    def test_baseline_starvation_does_not_hide_candidate_improvement_or_relax_its_target(self):
        small = {**self.profile["cases"][0], "name":"small"}
        self.profile["cases"] = [small, {**self.profile["cases"][0], "reference_case":"small"}]
        summary = {"attempts":20, "successes":20, "rejected":0, "errors":0, "oracle_mismatches":0,
                   "successful_latency_us":{"p50":10, "p95":100}, "ttfb_us":{"p50":1, "p95":2},
                   "offered_requests_per_second":10, "successful_requests_per_second":10}
        baseline = {"schema":1, "phase":"baseline", "binding":self.binding, "fixture_commit":self.head,
                    "workload_sha256":workload_identity(self.profile), "cases":[
                        {"name":name, "cache":"warm_objects", "workloads":{"source":copy.deepcopy(summary)}}
                        for name in ("small", "mixed")]}
        baseline["cases"][1]["workloads"]["source"]["successful_latency_us"]["p95"] = 300
        self.assertEqual(validate_report(baseline, self.profile), ["mixed:interactive_latency"])
        candidate = copy.deepcopy(baseline)
        candidate["phase"] = "candidate"
        candidate["cases"][1]["workloads"]["source"]["successful_latency_us"]["p95"] = 50
        self.assertEqual(compare_baseline(candidate, baseline, self.profile), [])
        self.assertEqual(validate_report(baseline, self.profile), ["mixed:interactive_latency"])
        candidate["cases"][1]["workloads"]["source"]["successful_latency_us"]["p95"] = 250
        self.assertIn("mixed:interactive_latency", validate_report(candidate, self.profile))
        baseline["cases"][0]["workloads"]["source"].update(successes=19, errors=1)
        with self.assertRaisesRegex(ValueError, "measurement gates"):
            compare_baseline(candidate, baseline, self.profile)

    def test_preview_migration_selects_a_frozen_wire_variant_without_changing_the_profile(self):
        profile = copy.deepcopy(self.profile)
        profile["workloads"][0].update(action="file-preview", baseline_action="file")
        validate_profile(profile)
        identity = workload_identity(profile)
        self.assertEqual(wire_workloads(profile, "baseline")["source"]["action"], "file")
        self.assertEqual(wire_workloads(profile, "candidate")["source"]["action"], "file-preview")
        self.assertEqual(workload_identity(profile), identity)
        profile["workloads"][0]["baseline_action"] = "blame"
        with self.assertRaisesRegex(ValueError, "endpoint migration"):
            validate_profile(profile)

    def test_cursor_variants_and_overload_envelopes_are_part_of_the_frozen_profile(self):
        profile = copy.deepcopy(self.profile)
        profile["workloads"][0].update(action="commits", page_depth=10,
                                       cursor_receipt="candidate.json", baseline_cursor_receipt="baseline.json")
        validate_profile(profile)
        self.assertEqual(wire_workloads(profile, "baseline")["source"]["cursor_receipt"], "baseline.json")
        self.assertEqual(wire_workloads(profile, "candidate")["source"]["cursor_receipt"], "candidate.json")
        identity = workload_identity(profile)
        profile["cases"][0]["maximum_failure_fraction"] = .5
        self.assertNotEqual(identity, workload_identity(profile))
        validate_profile(profile)
        profile["cases"][0]["maximum_failure_fraction"] = 1
        with self.assertRaisesRegex(ValueError, "failure envelope"):
            validate_profile(profile)

    def cli_args(self, url, output):
        self.profile["public_origin"] = "http://localhost:8788/"
        binary = Path(sys.executable).resolve()
        binding = {**self.binding, "server_binary":str(binary), "server_binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),
                   "source_inputs":{"fixture":"3" * 64}}
        Path(self.profile["build_receipt"]).write_text(json.dumps(binding))
        Path(self.profile["cases"][0]["state_receipt"]).write_text(json.dumps({"cache":"warm_objects", "replicas":[{
            "process_start_identity":"http-fixture", "server_binary_sha256":binding["server_binary_sha256"],
            "cache_counters":{"fixture":0}}]}))
        profile = self.root / "profile.json"
        profile.write_text(json.dumps(self.profile))
        return [str(Path(__file__).with_name("qualify_v2_read_service.py")),
                "--base-url", url, "--repository", "owner/repository", "--revision", self.head,
                "--text-path", "target.rs", "--profile", str(profile), "--output", str(output)]

    def test_reporting_failure_retains_the_measured_duration_and_journal_range(self):
        output = self.root / "receipts"
        args = self.cli_args(self.serve(status=429), output)
        with mock.patch.object(sys, "argv", args), mock.patch(
                "qualify_v2_read_service.summarize", side_effect=OSError("fixture reporting failure")):
            with self.assertRaisesRegex(OSError, "fixture reporting failure"):
                main()
        report = json.loads((output / "report.json").read_text())
        self.assertEqual((report["status"], report["error_kind"]), ("failed", "OSError"))
        self.assertEqual(report["pending_case"]["name"], "mixed")
        self.assertGreater(report["pending_case"]["elapsed_seconds"], 0)
        self.assertEqual(report["pending_case"]["journal_end"], (output / "requests.jsonl").stat().st_size)
        self.assertTrue(list(read_journal(output / "requests.jsonl")))

    def test_cli_retains_source_bound_measurements_and_request_failures(self):
        output = self.root / "receipts"
        args = self.cli_args(self.serve(status=429), output)
        result = subprocess.run([sys.executable, *args],
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 1)
        report = json.loads((output / "report.json").read_text())
        self.assertEqual(report["status"], "failed")
        self.assertEqual(report["binding"]["server_binary_sha256"],
                         hashlib.sha256(Path(sys.executable).resolve().read_bytes()).hexdigest())
        self.assertIn("mixed:failure_fraction", report["failures"])
        self.assertTrue((output / "requests.jsonl").read_text())
        self.assertEqual(set(self.seen_hosts), {"localhost:8788"})


if __name__ == "__main__":
    unittest.main()
