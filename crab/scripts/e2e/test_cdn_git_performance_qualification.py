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
import test_cdn_qualification as fixture


REGIONS = fixture.REGIONS
PLATFORM = "linux-x86_64"
RUN_ID = "123"
ATTEMPT = "2"


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
        with self.assertRaisesRegex(ValueError, "fetched different published Git fixtures"):
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
        self.amend_client("near/0001/cold_edge", started_at_unix_ms=1_012_400, finished_at_unix_ms=1_012_800)
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


if __name__ == "__main__":
    unittest.main()
