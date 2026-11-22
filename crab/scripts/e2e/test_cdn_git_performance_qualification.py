"""Exercise the paired benchmark join against complete synthetic measurements."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import collect_cdn_git_performance as BENCHMARK
from collect_cdn_product_metrics import NATIVE_TARGET, measure_native
import test_cdn_qualification as fixture


REGIONS = fixture.REGIONS
PLATFORM = "linux-x86_64"
RUN_ID = "123"
ATTEMPT = "2"


class NativeReadCountersTests(unittest.TestCase):
    def test_counts_responses_and_consumed_error_bytes_for_the_approved_provider(self) -> None:
        def event(method: str, status: int, body: int) -> bytes:
            return json.dumps({"target": NATIVE_TARGET, "fields": {
                "message": "Native object store read", "provider": "s3", "method": method,
                "status": status, "bytes": body,
            }}).encode()

        lines = [event("get", 206, 8), event("get", 404, 3), event("get", 0, 0), event("head", 200, 0)]
        self.assertEqual(measure_native(lines, "cloudfront/private/cli/git/direct"),
                         {"get_responses": 2, "head_responses": 1, "body_bytes": 11})
        with self.assertRaisesRegex(ValueError, "invalid native read event for the approved provider"):
            measure_native(lines, "cloud-cdn/private/cli/git/direct")


class CdnGitPerformanceQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = TemporaryDirectory(prefix="crab-cdn-benchmark-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.registration = {
            "schema": "crab.cdn-benchmark/v1", "source_sha": fixture.COMMIT,
            "scope": "cloudfront/private/cli/git/direct", "configuration_sha256": fixture.CONFIGURATION,
            "workload": "git-fetch", "samples": 30, "p95_budget_ms": 150, "cost_budget_microusd": 10,
            "regions": REGIONS, "deployment_policy_sha256": "d" * 64,
            "viewer_binding_sha256": "e" * 64,
            "scoped_view_sha256": "c" * 64,
            "cache_reuse_sha256": "b" * 64,
            "rotation_overlap_policy_sha256": "a" * 64,
        }
        self.registration["candidate"] = fixture.cli_candidate(self.registration["scope"], PLATFORM)
        fixture.write_git_measurements(self.root, self.registration, PLATFORM)

    def collect(self) -> dict:
        return fixture.CDN.collect_git_performance(self.root, self.registration, PLATFORM, RUN_ID, ATTEMPT)

    def amend_client(self, path: str, **changes: object) -> None:
        directory = self.root / path
        client = json.loads((directory / "client.json").read_bytes()) | changes
        digest = fixture.json_file(directory / "client.json", client)
        external = json.loads((directory / "measurement.json").read_bytes())
        fixture.json_file(directory / "measurement.json", external | {"client_report_sha256": digest})
        proof_path = directory / fixture.CDN.VIEWER_REQUEST_FILES["s3"]
        if proof_path.exists():
            proof = json.loads(proof_path.read_bytes())
            fixture.json_file(proof_path, proof | {"client_report_sha256": digest})

    def test_cli_produces_a_report_that_the_release_contract_accepts(self) -> None:
        benchmarks = self.root / "benchmarks.json"
        row = f"{self.registration['scope']}/{PLATFORM}"
        digest = fixture.json_file(benchmarks, {row: self.registration})
        result = subprocess.run([
            sys.executable, "-B", str(Path(BENCHMARK.__file__)),
            "--inputs", str(self.root), "--benchmarks", str(benchmarks), "--benchmarks-sha256", digest, "--row", row,
            "--run-id", RUN_ID, "--run-attempt", ATTEMPT,
        ], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        performance = json.loads(result.stdout)
        self.assertEqual(BENCHMARK.verify_performance(performance, self.registration), 180)
        self.assertEqual(performance["near"]["pairs"][0]["cold_edge"]["origin_fill_bytes"], 8)
        self.assertEqual(performance["near"]["pairs"][0]["warm_local"]["local_payload_hits"], 1)

    def test_rejects_a_different_commit_with_the_same_content(self) -> None:
        self.amend_client("near/0001/warm_edge", publication_oid="b" * 40)
        with self.assertRaisesRegex(ValueError, "fetched different published fixtures"):
            self.collect()

    def test_cli_pair_requires_its_native_publication_receipt(self) -> None:
        publication = self.root / "near/0001/publication.json"
        publication.unlink()
        with self.assertRaisesRegex(FileNotFoundError, "publication.json"):
            self.collect()

    def test_cli_pair_binds_the_exact_publication_receipt_bytes(self) -> None:
        path = self.root / "near/0001/publication.json"
        path.write_bytes(path.read_bytes() + b"\n")
        with self.assertRaisesRegex(ValueError, "must verify the retained CLI publication"):
            self.collect()

    def test_cli_pair_checks_the_retained_publication_even_when_all_arms_agree(self) -> None:
        path = self.root / "near/0001/publication.json"
        publication = json.loads(path.read_bytes())
        origin = json.loads((path.parent / "origin/client.json").read_bytes())
        for change in (
            {"oid": "f" * 40},
            {"sha256": "f" * 64},
            {"published": False, "visible_at_unix_ms": None},
            {"visible_at_unix_ms": origin["started_at_unix_ms"] + 1},
            {"candidate": publication["candidate"] | {"binary_sha256": "f" * 64}},
        ):
            with self.subTest(change=change):
                digest = fixture.json_file(path, publication | change)
                for arm in fixture.CDN.ARMS:
                    self.amend_client(f"near/0001/{arm}", publication_sha256=digest)
                with self.assertRaisesRegex(ValueError, "must verify the retained CLI publication"):
                    self.collect()

    def test_rejects_a_client_measurement_from_a_different_release_platform(self) -> None:
        self.amend_client("near/0001/origin", platform="linux-aarch64")
        with self.assertRaisesRegex(ValueError, "mismatched Git fetch provenance"):
            self.collect()

    def test_rejects_a_different_binary_on_the_registered_platform_and_commit(self) -> None:
        candidate = self.registration["candidate"] | {"binary_sha256": "f" * 64}
        self.amend_client("distant/0001/warm_edge", candidate=candidate)
        with self.assertRaisesRegex(ValueError, "did not run the registered CLI build"):
            self.collect()

    def test_rejects_the_same_client_result_reused_in_another_arm(self) -> None:
        issued = self.root / "near/0001/cold_edge/client.json"
        (self.root / "near/0001/warm_edge/client.json").write_bytes(issued.read_bytes())
        with self.assertRaisesRegex(ValueError, "fetch was reused"):
            self.collect()

    def test_rejects_an_active_binding_in_the_origin_arm(self) -> None:
        arm = "near/0001/origin"
        reader = json.loads((self.root / arm / "client.json").read_bytes())["reader"]
        self.amend_client(arm, reader=reader | {"binding_sha256": self.registration["viewer_binding_sha256"]})
        with self.assertRaisesRegex(ValueError, "requires an inactive viewer binding"):
            self.collect()

    def test_origin_arm_requires_an_observed_native_payload_transfer(self) -> None:
        self.amend_client("near/0001/origin", native_traffic={"get_responses": 0, "head_responses": 1, "body_bytes": 0})
        with self.assertRaisesRegex(ValueError, "observed no native payload transfer"):
            self.collect()

    def test_public_benchmark_requires_the_registered_export_with_the_same_viewer(self) -> None:
        scope = "cloudfront/public/cli/git/direct"
        self.registration.pop("scoped_view_sha256")
        self.registration.pop("cache_reuse_sha256")
        self.registration.pop("rotation_overlap_policy_sha256")
        self.registration.update(scope=scope, candidate=fixture.cli_candidate(scope, PLATFORM),
                                 publication_sha256="6" * 64, previous_publication_sha256="7" * 64,
                                 historical_origin_sha256="8" * 64)
        fixture.write_git_measurements(self.root, self.registration, PLATFORM)
        slot = "near/0001/warm_local"
        reader = json.loads((self.root / slot / "client.json").read_bytes())["reader"]
        self.amend_client(slot, reader=reader | {"public_export_sha256": self.registration["previous_publication_sha256"]})
        with self.assertRaisesRegex(ValueError, "did not use the registered public export approval"):
            self.collect()

    def test_runner_and_provider_metrics_must_cover_native_reads_during_an_edge_hit(self) -> None:
        report = self.root / "near/0001/warm_edge/measurement.json"
        external = json.loads(report.read_bytes())
        for changes, expected in (
            ({"origin_gets": 0}, "undercounts observed native GET responses"),
            ({"client_bytes": 15}, "undercounts observed client bytes"),
        ):
            with self.subTest(changes=changes):
                fixture.json_file(report, external | changes)
                with self.assertRaisesRegex(ValueError, expected):
                    self.collect()

    def test_cloud_cdn_fill_metrics_cover_child_requests_without_double_charging_viewer_bytes(self) -> None:
        scope = "cloud-cdn/private/cli/git/direct"
        self.registration.update(scope=scope, candidate=fixture.cli_candidate(scope, PLATFORM))
        fixture.write_git_measurements(self.root, self.registration, PLATFORM)
        arm = self.root / "near/0001/cold_edge"
        log = arm / fixture.CDN.VIEWER_REQUEST_FILES["gcs"]
        proof = json.loads(log.read_bytes())
        row = next(iter(proof["records"].values()))
        row["origin_requests"] = {
            "a" * 64: {"method": "GET", "status": 206, "cache_fill_bytes": 10},
            "b" * 64: {"method": "GET", "status": 206, "cache_fill_bytes": 13},
            "c" * 64: {"method": "GET", "status": 404, "cache_fill_bytes": 3},
        }
        fixture.json_file(log, proof)
        report = arm / "measurement.json"
        external = json.loads(report.read_bytes())
        for changes, expected in (
            ({}, "undercounts observed native GET responses or logged CDN origin GETs"),
            ({"origin_gets": 4, "origin_fill_bytes": 25}, "undercounts logged CDN cache fill bytes"),
        ):
            with self.subTest(changes=changes):
                fixture.json_file(report, external | changes)
                with self.assertRaisesRegex(ValueError, expected):
                    self.collect()
        fixture.json_file(report, external | {"origin_gets": 4, "origin_fill_bytes": 26})
        self.assertEqual(self.collect()["near"]["pairs"][0]["cold_edge"]["origin_fill_bytes"], 26)

        warm_log = arm.with_name("warm_edge") / log.name
        warm = json.loads(warm_log.read_bytes())
        next(iter(warm["records"].values()))["origin_requests"] = row["origin_requests"]
        fixture.json_file(warm_log, warm)
        with self.assertRaisesRegex(ValueError, "CDN log requests were reused across arms"):
            self.collect()

    def test_cloud_cdn_cli_cold_arm_requires_a_successful_fill(self) -> None:
        scope = "cloud-cdn/private/cli/git/direct"
        self.registration.update(scope=scope, candidate=fixture.cli_candidate(scope, PLATFORM))
        fixture.write_git_measurements(self.root, self.registration, PLATFORM)
        arm = self.root / "near/0001/cold_edge"
        log = arm / fixture.CDN.VIEWER_REQUEST_FILES["gcs"]
        proof = json.loads(log.read_bytes())
        row = next(iter(proof["records"].values()))
        row["cache_fill_bytes"] = 0
        report = arm / "measurement.json"
        fixture.json_file(report, json.loads(report.read_bytes()) | {"origin_gets": 2})
        for method, status, size in (("GET", 206, 0), ("GET", 404, 8), ("HEAD", 200, 8)):
            with self.subTest(method=method, status=status, size=size):
                row["origin_requests"] = {"a" * 64: {"method": method, "status": status, "cache_fill_bytes": size}}
                fixture.json_file(log, proof)
                with self.assertRaisesRegex(ValueError, "has no logged Cloud CDN fill"):
                    self.collect()
        row["origin_requests"] = {"a" * 64: {"method": "GET", "status": 206, "cache_fill_bytes": 8}}
        fixture.json_file(log, proof)
        self.assertEqual(self.collect()["near"]["pairs"][0]["cold_edge"]["origin_fill_bytes"], 8)

    def test_cloudfront_hashes_must_cover_the_timed_requests_and_agree_on_cache_status(self) -> None:
        path = self.root / "near/0001/cold_edge" / fixture.CDN.VIEWER_REQUEST_FILES["s3"]
        proof = json.loads(path.read_bytes())
        request_id, record = next(iter(proof["records"].items()))
        for records, error in (
            ({}, "must cover each measured viewer attempt"),
            ({request_id: record | {"status": 200}}, "log status or response bytes disagree"),
            ({request_id: record | {"cache": "hit"}}, "log cache result disagrees"),
        ):
            with self.subTest(records=records):
                fixture.json_file(path, proof | {"records": records})
                with self.assertRaisesRegex(ValueError, error):
                    self.collect()

    def test_cloudfront_cold_arm_requires_an_approved_s3_fill(self) -> None:
        path = self.root / "near/0001/cold_edge" / fixture.CDN.VIEWER_REQUEST_FILES["s3"]
        proof = json.loads(path.read_bytes())
        request_id, request = next(iter(proof["origin_requests"].items()))
        for origins, error in (
            ({}, "has no logged CloudFront fill"),
            ({request_id: request | {"key_sha256": "f" * 64}}, "does not match the timed CloudFront request"),
            ({request_id: request | {"at_unix_s": request["at_unix_s"] - 1}}, "does not match the timed CloudFront request"),
        ):
            with self.subTest(origins=origins):
                fixture.json_file(path, proof | {"origin_requests": origins})
                with self.assertRaisesRegex(ValueError, error):
                    self.collect()

    def test_cloudfront_cold_arm_totals_must_cover_the_s3_fill(self) -> None:
        report = self.root / "near/0001/cold_edge/measurement.json"
        external = json.loads(report.read_bytes())
        for changes, expected in (
            ({"origin_gets": 1}, "undercounts observed native GET responses or logged CDN origin GETs"),
            ({"origin_fill_bytes": 7}, "undercounts logged CDN cache fill bytes"),
        ):
            with self.subTest(changes=changes):
                fixture.json_file(report, external | changes)
                with self.assertRaisesRegex(ValueError, expected):
                    self.collect()

    def test_cloudfront_warm_and_cold_arms_must_use_distinct_s3_log_seconds(self) -> None:
        self.amend_client("near/0001/warm_edge", started_at_unix_ms=1_011_500, finished_at_unix_ms=1_011_620)
        with self.assertRaisesRegex(ValueError, "CloudFront edge arms must use disjoint S3 log seconds"):
            self.collect()

    def test_front_door_cold_arm_requires_blob_origin_bytes_within_its_measured_window(self) -> None:
        scope = "front-door/private/cli/git/direct"
        self.registration.update(scope=scope, candidate=fixture.cli_candidate(scope, PLATFORM))
        fixture.write_git_measurements(self.root, self.registration, PLATFORM)
        arm = self.root / "near/0001/cold_edge"
        log = arm / fixture.CDN.VIEWER_REQUEST_FILES["azure"]
        proof = json.loads(log.read_bytes())
        request_id, request = next(iter(proof["origin_requests"].items()))
        for change, expected in (
            ({"at_unix_ms": request["at_unix_ms"] - 1}, "does not match the timed Front Door request"),
            ({"status": 0}, "has no logged Front Door fill"),
        ):
            with self.subTest(change=change):
                fixture.json_file(log, proof | {"origin_requests": {request_id: request | change}})
                with self.assertRaisesRegex(ValueError, expected):
                    self.collect()
        fixture.json_file(log, proof)
        self.assertEqual(self.collect()["near"]["pairs"][0]["cold_edge"]["origin_gets"], 2)

    def test_rejects_reusing_a_local_cache_across_arms(self) -> None:
        client = json.loads((self.root / "near/0001/cold_edge/client.json").read_bytes())
        self.amend_client("near/0001/warm_edge", reader=client["reader"])
        with self.assertRaisesRegex(ValueError, "benchmark arms reused a local cache root"):
            self.collect()

    def test_rejects_overlapping_runs_or_cross_pair_interleaving(self) -> None:
        self.amend_client("near/0002/origin", started_at_unix_ms=1_010_000, finished_at_unix_ms=1_010_100)
        with self.assertRaisesRegex(ValueError, "arms and pairs must be interleaved without overlapping"):
            self.collect()

    def test_cold_edge_must_be_measured_before_warm_edge(self) -> None:
        self.amend_client("near/0001/warm_edge", started_at_unix_ms=1_010_500, finished_at_unix_ms=1_010_650)
        with self.assertRaisesRegex(ValueError, "must measure cold edge, warm edge and warm local in order"):
            self.collect()

    def test_rejects_wrong_region_or_unbound_provider_metrics(self) -> None:
        report = self.root / "near/0001/cold_edge/measurement.json"
        external = json.loads(report.read_bytes())
        for change in ({"region": REGIONS["distant"]}, {"client_report_sha256": "f" * 64}, {"provider": "gcs"}):
            with self.subTest(change=change):
                fixture.json_file(report, external | change)
                with self.assertRaisesRegex(ValueError, "mismatched provider measurements"):
                    self.collect()

    def test_rejects_unmeasured_incomplete_edge_bytes(self) -> None:
        self.amend_client("near/0001/cold_edge", incomplete_edge_bytes=3)
        with self.assertRaisesRegex(ValueError, "undercounts observed client bytes"):
            self.collect()

    def test_rejects_unredacted_client_fields_before_retaining_a_record(self) -> None:
        self.amend_client("near/0001/origin", url="private-fixture-url")
        retained = self.root / "retained"
        with self.assertRaisesRegex(ValueError, "invalid fields in redacted Git fetch"):
            fixture.CDN.collect_git_performance(
                self.root, self.registration, PLATFORM, RUN_ID, ATTEMPT, retain=retained,
            )
        self.assertFalse(retained.exists())

    def test_rejects_regions_that_were_not_registered_for_the_recorded_arm(self) -> None:
        self.registration["regions"] = REGIONS | {"near": "another-region"}
        with self.assertRaisesRegex(ValueError, "mismatched provider measurements"):
            self.collect()


class DesktopGitPerformanceQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = TemporaryDirectory(prefix="crab-desktop-benchmark-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write_benchmark(self, edge: str = "cloudfront") -> tuple[Path, dict]:
        scope = f"{edge}/private/desktop/git/direct"
        writer = fixture.cli_candidate(scope, PLATFORM)
        spec = {
            "schema": "crab.cdn-benchmark/v1", "source_sha": fixture.COMMIT, "scope": scope,
            "configuration_sha256": fixture.CONFIGURATION, "workload": fixture.CDN.DESKTOP_GIT_ACTION,
            "samples": 30, "p95_budget_ms": 150, "cost_budget_microusd": 10, "regions": REGIONS,
            "deployment_policy_sha256": "d" * 64, "viewer_binding_sha256": "e" * 64,
            "viewer_approval_sha256": "9" * 64, "scoped_view_sha256": "c" * 64, "cache_reuse_sha256": "b" * 64,
            "rotation_overlap_policy_sha256": "a" * 64,
            "candidate": writer, "desktop_candidate": writer | {"cargo_features": "", "binary_sha256": "a" * 64},
            "desktop_native_target": {"origin_sha256": "7" * 64, "prefix_sha256": "8" * 64},
        }
        inputs = self.root / edge
        fixture.write_git_measurements(inputs, spec, PLATFORM)
        return inputs, spec

    def test_recomputes_and_retains_paired_desktop_samples_for_each_provider(self) -> None:
        for edge in fixture.CDN.PROVIDER_FOR_EDGE:
            with self.subTest(edge=edge):
                inputs, spec = self.write_benchmark(edge)
                retained = self.root / f"retained-{edge}"
                performance = fixture.CDN.collect_git_performance(inputs, spec, PLATFORM, RUN_ID, ATTEMPT, retain=retained)
                self.assertEqual(BENCHMARK.verify_performance(performance, spec), 180)
                for name in ("publication.json", "provider-published.json"):
                    self.assertEqual((retained / "near/0001" / name).read_bytes(), (inputs / "near/0001" / name).read_bytes())
                self.assertEqual(fixture.CDN.collect_git_performance(retained, spec, PLATFORM, RUN_ID, ATTEMPT), performance)

    def test_rejects_a_repeat_with_another_primer_or_cache(self) -> None:
        inputs, spec = self.write_benchmark()
        repeat = inputs / "near/0001/warm_local/client.json"
        original = json.loads(repeat.read_bytes())
        for field in ("primer_sha256", "cache_root_sha256"):
            with self.subTest(field=field):
                fixture.json_file(repeat, original | {field: "f" * 64})
                with self.assertRaisesRegex(ValueError, "did not repeat its own warm edge desktop read"):
                    fixture.CDN.collect_git_performance(inputs, spec, PLATFORM, RUN_ID, ATTEMPT)

    def test_generic_desktop_canary_cannot_supply_raw_benchmark_records(self) -> None:
        inputs, spec = self.write_benchmark()
        generic = {key: value for key, value in spec.items() if key not in {
            "candidate", "desktop_candidate", "desktop_native_target", "viewer_binding_sha256", "viewer_approval_sha256",
        }} | {"workload": "git-fetch"}
        with self.assertRaisesRegex(ValueError, "benchmark must register a supported CLI"):
            fixture.CDN.collect_git_performance(inputs, generic, PLATFORM, RUN_ID, ATTEMPT)

    def test_requires_the_independent_observation_of_the_published_commit_before_reading(self) -> None:
        inputs, spec = self.write_benchmark()
        proof = inputs / "near/0001/provider-published.json"
        original = json.loads(proof.read_bytes())
        first = json.loads((inputs / "near/0001/origin/client.json").read_bytes())["started_at_unix_ms"]
        for change, message in (
            ({"native_ref": original["native_ref"] | {"oid": "f" * 40}}, "unexpected committed tip"),
            ({"observed_at_unix_ms": first + 1}, "independently observed native publication"),
        ):
            with self.subTest(message=message):
                fixture.json_file(proof, original | change)
                with self.assertRaisesRegex(ValueError, message):
                    fixture.CDN.collect_git_performance(inputs, spec, PLATFORM, RUN_ID, ATTEMPT)

    def test_regional_bandwidth_needs_the_related_immutable_metadata_bytes(self) -> None:
        inputs, spec = self.write_benchmark()
        arm = inputs / "near/0001/cold_edge"
        client_path = arm / "client.json"
        client = json.loads(client_path.read_bytes())
        client["all_edge_attempts"] += 1
        new_id = "c" * 64
        client["edge_responses"][new_id] = {"status": 206, "complete": True, "cache": "hit", "bytes": 500}
        digest = fixture.json_file(client_path, client)
        viewer_path = arm / "cloudfront-requests.json"
        proof = json.loads(viewer_path.read_bytes())
        proof["records"][new_id] = {"status": 206, "cache": "hit", "response_bytes": 512, "key_sha256": "d" * 64}
        fixture.json_file(viewer_path, proof | {"client_report_sha256": digest})
        external = arm / "measurement.json"
        fixture.json_file(external, json.loads(external.read_bytes()) | {"client_report_sha256": digest})
        with self.assertRaisesRegex(ValueError, "undercounts observed client bytes"):
            fixture.CDN.collect_git_performance(inputs, spec, PLATFORM, RUN_ID, ATTEMPT)


if __name__ == "__main__":
    unittest.main()
