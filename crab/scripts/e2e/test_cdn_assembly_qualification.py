"""Exercise staging and canonical report verification before artifact publication."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

import assemble_cdn_qualification as ASSEMBLE
import test_cdn_qualification as fixture
import test_cdn_release_qualification as release


class CdnAssemblyQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        example = fixture.CdnQualificationTests()
        example.setUp()
        self.addCleanup(example.doCleanups)
        self.example = example
        self.root = example.directory
        self.output = self.root / "artifact"

    def inputs(self, provider: str = "s3", consumer: str = "sdk", access: str = "private", *,
               family: str = "git", profile: str = "direct") -> str:
        self.example.configure(provider, access, profile, consumer=consumer, family=family)
        if fixture.CDN.is_direct_desktop_git_scope(self.example.scope):
            self.example.record_desktop_product()
        report = self.example.report
        self.row = f"{report['scope']}/{report['platform']}"
        row = self.root / self.row
        row.mkdir(parents=True)
        (self.root / "traces").rename(row / "traces")
        (self.root / "traces").mkdir()
        native = self.root / provider
        native.mkdir()
        (self.root / "report.json").rename(native / "report.json")
        observations = {key: report[key] for key in ASSEMBLE.OBSERVATION_FIELDS - {"check_results"}}
        if fixture.CDN.requires_cli_workload(self.example.scope):
            observations.pop("performance")
            observations.pop("product_action")
            for name in (fixture.CDN.GIT_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                (self.root / name).rename(row / name)
        if (fixture.CDN.is_direct_sdk_git_scope(self.example.scope)
                or fixture.CDN.requires_desktop_workload(self.example.scope, self.example.registration["workload"])):
            observations.pop("product_action")
            observations.pop("performance")
            for name in (fixture.CDN.GIT_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                (self.root / name).rename(row / name)
        if fixture.CDN.is_direct_xorb_read_scope(self.example.scope):
            observations.pop("product_action")
            observations.pop("performance")
            for name in (fixture.CDN.XORB_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                (self.root / name).rename(row / name)
        if fixture.CDN.is_direct_service_scope(self.example.scope):
            observations.pop("product_action")
            observations.pop("performance")
            for name in (fixture.CDN.SERVICE_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                (self.root / name).rename(row / name)
        if access == "public":
            name = fixture.CDN.PUBLIC_EXPORT_DIR
            (self.root / name).rename(row / name)
        observations["schema"] = "crab.cdn-observations/v1"
        observations["check_results"] = dict.fromkeys(report["checks"], True)
        self.observations = row / "observations.json"
        fixture.json_file(self.observations, observations)
        return fixture.json_file(self.root / "benchmarks.json", {self.row: self.example.registration})

    def assemble(self, digest: str, scopes: set[str] | None = None, *, transport: Path | None = None) -> int:
        return ASSEMBLE.assemble_bundle(
            self.root, self.output, digest, scopes or set(), fixture.COMMIT, "123", "2",
            transport_traces=transport,
        )

    def canary_inputs(self, provider: str) -> Path:
        root = self.root / "transport"
        destination = root / provider
        destination.mkdir(parents=True)
        names = ASSEMBLE.CANARY_REQUIRED[self.example.scope.split("/")[1]]
        observations = json.loads(self.observations.read_bytes())
        for name in names:
            filename = f"{name}.json"
            (self.root / self.row / "traces" / filename).rename(destination / filename)
            observations["check_results"].pop(name)
        fixture.json_file(self.observations, observations)
        return root

    def test_exposes_the_exact_release_artifact_after_verification(self) -> None:
        digest = self.inputs("gcs")
        (self.root / self.row / "raw.log").write_text("local transport output", encoding="utf-8")
        result = subprocess.run([
            sys.executable, str(Path(ASSEMBLE.__file__)),
            "--inputs", str(self.root), "--output", str(self.output),
            "--benchmarks-sha256", digest, "--source-sha", fixture.COMMIT, "--run-id", "123", "--run-attempt", "2",
        ], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        row = self.output / self.row
        expected = {"cdn-report.json", "traces", fixture.CDN.GIT_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR}
        self.assertEqual({path.name for path in row.iterdir()}, expected)
        self.assertEqual({path.name for path in self.output.iterdir()}, {"benchmarks.json", "gcs", "cloud-cdn"})
        self.assertEqual((self.root / "benchmarks.json").read_bytes(), (self.output / "benchmarks.json").read_bytes())
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["status"], "qualified")
        self.assertEqual(report["scope"], self.example.scope)
        self.assertEqual(set(report["checks"]), set(self.example.report["checks"]))

        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        sdk_product = row / fixture.CDN.GIT_PRODUCT_DIR
        self.assertEqual({path.name for path in sdk_product.iterdir()},
                         {"publication.json", "snapshot.json", "retirement.json", *fixture.CDN.GIT_NATIVE_REF_FILES.values(),
                          *fixture.CDN.SDK_CLONE_FILES.values(), *fixture.CDN.SDK_FETCH_FILES.values(),
                          fixture.CDN.PRODUCT_VIEWER_RECORD, *fixture.CDN.SDK_WORKTREE_VIEWER_FILES.values()})
        self.assertEqual(report["product_action"], self.example.report["product_action"])
        self.assertEqual(
            release.RELEASE.verify_bundle(
                self.output, policy, {self.example.scope}, fixture.COMMIT, "123", "2", digest,
            ), 1,
        )
        retained = row / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001/warm_local/client.json"
        unknown = json.loads(retained.read_bytes()) | {"primer_sha256": "1" * 64}
        fixture.json_file(retained, unknown)
        with self.assertRaises(subprocess.CalledProcessError):
            release.RELEASE.verify_bundle(self.output, policy, {self.example.scope}, fixture.COMMIT, "123", "2", digest)

    def test_sdk_hydration_assembly_derives_product_from_the_native_lifecycle_and_pair(self) -> None:
        digest = self.inputs("s3", family="xorb")
        relative = Path(self.row) / fixture.CDN.XORB_PRODUCT_DIR / fixture.CDN.PRODUCT_VIEWER_RECORD
        proof = (self.root / relative).read_bytes()
        (self.root / relative).unlink()
        with self.assertRaisesRegex(FileNotFoundError, fixture.CDN.PRODUCT_VIEWER_RECORD):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        (self.root / relative).write_bytes(proof)
        self.assertEqual(self.assemble(digest), 1)
        self.assertEqual((self.output / relative).read_bytes(), proof)
        row = self.output / self.row
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["product_action"], self.example.report["product_action"])
        self.assertEqual(report["performance"], self.example.report["performance"])
        product = row / fixture.CDN.XORB_PRODUCT_DIR
        self.assertEqual({entry.name for entry in product.iterdir()},
                         {"hydration.json", "workload.json", "publication.json", "retirement.json",
                          fixture.CDN.PRODUCT_VIEWER_RECORD, *fixture.CDN.GIT_NATIVE_REF_FILES.values()})
        # Recheck retained files using the same verifier that consumes the release bundle.
        fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")
        edge = product / "hydration.json"
        original = json.loads(edge.read_bytes())
        fixture.json_file(edge, original | {"xorb_key_sha256": "0" * 64})
        with self.assertRaises(subprocess.CalledProcessError):
            fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")

    def test_managed_git_assembly_binds_regional_grants_and_native_ref_epoch(self) -> None:
        digest = self.inputs("s3", consumer="cli", profile="managed")
        cdn = fixture.CDN
        pair = self.root / self.row / cdn.GIT_BENCHMARK_DIR / "near/0001"
        client = pair / "origin/client.json"
        native = pair / cdn.GIT_NATIVE_REF_FILES["published"]
        client_record = json.loads(client.read_bytes())
        native_record = json.loads(native.read_bytes())
        changes = (
            (client, client_record | {"managed_grant": client_record["managed_grant"] | {"repository_sha256": "0" * 64}},
             "different registered managed issuer, repository or approval"),
            (native, native_record | {"native_ref": native_record["native_ref"] | {"epoch_sha256": "0" * 64}},
             "must observe the publication in the managed Git product's native repository epoch"),
        )
        for path, value, error in changes:
            with self.subTest(file=path.name):
                original = path.read_bytes()
                fixture.json_file(path, value)
                with self.assertRaisesRegex(ValueError, error):
                    self.assemble(digest)
                self.assertFalse(self.output.exists())
                path.write_bytes(original)
        self.assertEqual(self.assemble(digest), 1)
        retained = self.output / native.relative_to(self.root)
        self.assertEqual(retained.read_bytes(), native.read_bytes())
        cdn.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")

    def test_mount_assembly_rechecks_physical_view_live_revision_and_detach(self) -> None:
        digest = self.inputs("azure", consumer="mount", family="xorb")
        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        retained = row / fixture.CDN.XORB_PRODUCT_DIR / "mount.json"
        self.assertTrue(retained.is_file())
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["product_action"]["name"], fixture.CDN.MOUNT_XORB_ACTION)
        fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")
        original = json.loads(retained.read_bytes())
        for change in ({"unmounted": False}, {"mounted_oid": "a" * 40}):
            fixture.json_file(retained, original | change)
            with self.assertRaises(subprocess.CalledProcessError):
                fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")

    def test_sdk_requires_measured_snapshot_and_rejects_operator_supplied_product_summary(self) -> None:
        digest = self.inputs("gcs")
        product = self.root / self.row / fixture.CDN.GIT_PRODUCT_DIR
        (product / "snapshot.json").unlink()
        with self.assertRaises(FileNotFoundError):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        observations = json.loads(self.observations.read_bytes())
        fixture.json_file(self.observations, observations | {"product_action": self.example.report["product_action"]})
        with self.assertRaisesRegex(ValueError, "invalid fields in CDN observations"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_private_desktop_assembly_requires_fresh_rollback_and_retains_regional_evidence(self) -> None:
        digest = self.inputs("s3", consumer="desktop")
        inputs = self.root / self.row
        cdn = fixture.CDN
        read = json.loads((inputs / cdn.GIT_PRODUCT_DIR / cdn.DESKTOP_READ_RECORD).read_bytes())
        rollback = inputs / "traces/rollback.json"
        original = rollback.read_bytes()
        fixture.json_file(rollback, json.loads(original) | {"cache_root_sha256": read["cache_root_sha256"]})
        with self.assertRaisesRegex(ValueError, "desktop product reads must run in order with fresh caches"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        rollback.write_bytes(original)
        with self.assertRaisesRegex(ValueError, "missing required CDN platform registrations"):
            self.assemble(digest, {self.example.scope})
        self.assertFalse(self.output.exists())

        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        measured = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(measured["product_action"], self.example.report["product_action"])
        files = {"publication.json", "retirement.json", cdn.DESKTOP_READ_RECORD,
                 cdn.PRODUCT_VIEWER_RECORD, *cdn.GIT_NATIVE_REF_FILES.values()}
        self.assertEqual({path.name for path in (row / cdn.GIT_PRODUCT_DIR).iterdir()}, files)
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        spec = self.example.registration
        cdn.verify_bundle_row(self.output, self.row, spec, fixture.COMMIT, "123", "2", policy=policy)
        qualification_build = spec["desktop_candidate"] | {"qualification_feature": True}
        with self.assertRaises(subprocess.CalledProcessError):
            cdn.verify_bundle_row(self.output, self.row, spec | {"desktop_candidate": qualification_build},
                                  fixture.COMMIT, "123", "2", policy=policy)
        (row / cdn.GIT_BENCHMARK_DIR / "near/0001/provider-published.json").unlink()
        with self.assertRaises(subprocess.CalledProcessError):
            cdn.verify_bundle_row(self.output, self.row, spec, fixture.COMMIT, "123", "2", policy=policy)

    def test_public_desktop_assembly_derives_the_inventory_transition_from_viewer_logs(self) -> None:
        digest = self.inputs("gcs", consumer="desktop", access="public")
        cdn = fixture.CDN
        product = self.root / self.row / cdn.GIT_PRODUCT_DIR
        viewer = product / cdn.PRODUCT_VIEWER_RECORD
        raw_viewer = viewer.read_bytes()
        wrong = json.loads(raw_viewer)
        next(iter(wrong["records"].values()))["key_sha256"] = "0" * 64
        fixture.json_file(viewer, wrong)
        with self.assertRaisesRegex(ValueError, "CDN logs did not confirm the new export object"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        viewer.write_bytes(raw_viewer)
        (self.root / self.row / "traces" / f"{cdn.PUBLIC_TRANSITION_CHECK}.json").unlink()
        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        trace = json.loads((row / "traces" / f"{cdn.PUBLIC_TRANSITION_CHECK}.json").read_bytes())
        self.assertEqual(trace["after"]["verified_edge_key_sha256"], self.example.object_key)
        self.assertTrue((row / cdn.GIT_PRODUCT_DIR / cdn.GIT_PRIOR_EXPORT_RECORD).is_file())
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        cdn.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2", policy=policy)

    def test_front_door_desktop_bundle_bounds_regional_reads_by_cleanup(self) -> None:
        digest = self.inputs("azure", consumer="desktop")
        row = self.root / self.row
        cleanup = row / "traces/cleanup.json"
        trace = json.loads(cleanup.read_bytes())
        retired_path = row / fixture.CDN.GIT_PRODUCT_DIR / "provider-retired.json"
        retired = json.loads(retired_path.read_bytes())
        fixture.json_file(cleanup, trace | {"observed_at_unix_ms": retired["observed_at_unix_ms"]})
        with self.assertRaisesRegex(ValueError, "ran outside the desktop fixture lifetime"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        fixture.json_file(cleanup, trace)
        self.assertEqual(self.assemble(digest), 1)

    def test_public_sdk_assembly_requires_and_retains_the_transition_viewer_proof(self) -> None:
        digest = self.inputs("azure", access="public")
        relative = Path(self.row) / fixture.CDN.GIT_PRODUCT_DIR / fixture.CDN.PRODUCT_VIEWER_RECORD
        proof = (self.root / relative).read_bytes()
        (self.root / relative).unlink()
        with self.assertRaises(FileNotFoundError):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        (self.root / relative).write_bytes(proof)
        self.assertEqual(self.assemble(digest), 1)
        self.assertEqual((self.output / relative).read_bytes(), proof)

    def test_service_assembly_derives_the_product_from_the_retained_lifecycle(self) -> None:
        digest = self.inputs("gcs", consumer="s3-gateway", family="lfs")
        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["product_action"], self.example.report["product_action"])
        product = row / fixture.CDN.SERVICE_PRODUCT_DIR
        self.assertEqual({file.name for file in product.iterdir()}, set(fixture.CDN.SERVICE_PRODUCT_FILES.values()))
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2",
                                      policy=policy)
        edge = product / "edge.json"
        candidate = json.loads(edge.read_bytes())
        fixture.json_file(edge, candidate | {"etag_sha256": "0" * 64})
        with self.assertRaises(subprocess.CalledProcessError):
            fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2")

    def test_service_assembly_rejects_a_generic_summary_and_a_missing_provider_record(self) -> None:
        digest = self.inputs("azure", consumer="server")
        observations = json.loads(self.observations.read_bytes())
        fixture.json_file(self.observations, observations | {"product_action": self.example.report["product_action"]})
        with self.assertRaisesRegex(ValueError, "invalid fields in CDN observations"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        fixture.json_file(self.observations, observations)
        (self.root / self.row / fixture.CDN.SERVICE_PRODUCT_DIR / fixture.CDN.GIT_NATIVE_REF_FILES["retired"]).unlink()
        with self.assertRaises(FileNotFoundError):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_public_service_assembly_requires_the_registered_export(self) -> None:
        digest = self.inputs("s3", consumer="server", access="public")
        row = self.root / self.row
        path = row / fixture.CDN.SERVICE_PRODUCT_DIR / "edge.json"
        edge = json.loads(path.read_bytes())
        fixture.json_file(path, edge | {"reader": edge["reader"] | {"public_export_sha256": "0" * 64}})
        with self.assertRaisesRegex(ValueError, "applying the approved receipts"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        fixture.json_file(path, edge)
        trace_path = Path(self.row) / "traces" / f"{fixture.CDN.PUBLIC_TRANSITION_CHECK}.json"
        (self.root / trace_path).unlink()
        self.assertEqual(self.assemble(digest), 1)
        derived = json.loads((self.output / trace_path).read_bytes())
        self.assertEqual(derived["before"]["edge_attempts"], 0)
        self.assertEqual(derived["after"]["verified_edge_key_sha256"], edge["transition_key_sha256"])
        self.assertTrue((self.output / self.row / fixture.CDN.SERVICE_PRODUCT_DIR / fixture.CDN.SERVICE_PUBLIC_PRIOR_RECORD).exists())
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        fixture.CDN.verify_bundle_row(self.output, self.row, self.example.registration, fixture.COMMIT, "123", "2", policy=policy)

    def test_public_service_lfs_transition_rejects_a_prior_process_reused_at_the_edge(self) -> None:
        digest = self.inputs("azure", consumer="s3-gateway", access="public", family="lfs")
        product = self.root / self.row / fixture.CDN.SERVICE_PRODUCT_DIR
        edge = json.loads((product / "edge.json").read_bytes())
        prior = product / fixture.CDN.SERVICE_PUBLIC_PRIOR_RECORD
        original = json.loads(prior.read_bytes())
        fixture.json_file(prior, original | {"service_instance_sha256": edge["service_instance_sha256"]})
        with self.assertRaisesRegex(ValueError, "requires fresh processes"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())
        fixture.json_file(prior, original)
        self.assertEqual(self.assemble(digest), 1)

    def test_regional_service_lfs_rejects_reused_disks_and_an_unpaired_local_repeat(self) -> None:
        digest = self.inputs("azure", consumer="s3-gateway", family="lfs")
        row = self.root / self.row
        edge = json.loads((row / fixture.CDN.SERVICE_PRODUCT_DIR / "edge.json").read_bytes())
        pair = row / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001"
        local = pair / "warm_local/client.json"
        local_record = json.loads(local.read_bytes())
        fixture.json_file(local, local_record | {"cache_root_sha256": edge["cache_root_sha256"]})
        with self.assertRaisesRegex(ValueError, "reuse its own edge primer's cache root"):
            self.assemble(digest)
        fixture.json_file(local, local_record)

        cold = pair / "cold_edge/client.json"
        cold_record = json.loads(cold.read_bytes())
        for slot, root in ((cold, edge["cache_root_sha256"]),
                           (pair / "warm_edge/client.json", cold_record["cache_root_sha256"])):
            record = json.loads(slot.read_bytes())
            fixture.json_file(slot, record | {"cache_root_sha256": root})
            with self.assertRaisesRegex(ValueError, "reused a service LFS cache root"):
                self.assemble(digest)
            fixture.json_file(slot, record)
        self.assertFalse(self.output.exists())
        self.assertEqual(self.assemble(digest), 1)

    def test_regional_service_git_rejects_cross_process_primers_and_reused_workers(self) -> None:
        digest = self.inputs("gcs", consumer="server")
        pair = self.root / self.row / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001"
        slot = pair / "warm_local" / "client.json"
        original = json.loads(slot.read_bytes())
        for field, value in (("primer_sha256", "f" * 64), ("service_instance_sha256", "f" * 64)):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "repeat its own edge primer in the same process"):
                fixture.json_file(slot, original | {field: value})
                self.assemble(digest)
        self.assertFalse(self.output.exists())
        fixture.json_file(slot, original)

        cold = pair / "cold_edge" / "client.json"
        warm = json.loads((pair / "warm_edge" / "client.json").read_bytes())
        cold_original = json.loads(cold.read_bytes())
        for change, error in (
            ({"started_at_unix_ms": 1, "finished_at_unix_ms": 2}, "outside the independently observed publication interval"),
            ({"service_instance_sha256": warm["service_instance_sha256"]}, "must use a fresh service process"),
            ({"candidate": warm["candidate"] | {"binary_sha256": "0" * 64}}, "different service workload or read target"),
        ):
            with self.subTest(error=error), self.assertRaisesRegex(ValueError, error):
                replacement = fixture.json_file(cold, cold_original | change)
                for name in ("measurement.json", fixture.CDN.VIEWER_REQUEST_FILES["gcs"]):
                    path = cold.parent / name
                    previous = json.loads(path.read_bytes())
                    fixture.json_file(path, previous | {"client_report_sha256": replacement})
                self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_private_service_git_requires_the_registered_release_candidate(self) -> None:
        digest = self.inputs("gcs", consumer="server")
        self.assertEqual(self.assemble(digest), 1)
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        spec = self.example.registration
        fixture.CDN.verify_bundle_row(self.output, self.row, spec, fixture.COMMIT, "123", "2", policy=policy)

        qualified = spec["service_candidate"] | {
            "cargo_features": "provider-cdn,provider-cdn-qualification", "qualification_feature": True,
        }
        with self.assertRaises(subprocess.CalledProcessError):
            fixture.CDN.verify_bundle_row(self.output, self.row, spec | {"service_candidate": qualified},
                                          fixture.COMMIT, "123", "2", policy=policy)

    def test_failed_operational_check_leaves_no_artifact(self) -> None:
        self.inputs("gcs")
        registrations = json.loads((self.root / "benchmarks.json").read_bytes())
        self.inputs("s3")
        registrations[self.row] = self.example.registration
        digest = fixture.json_file(self.root / "benchmarks.json", registrations)
        observations = json.loads(self.observations.read_bytes())
        observations["check_results"]["rollback"] = False
        fixture.json_file(self.observations, observations)
        with self.assertRaisesRegex(ValueError, "failed measured check: rollback"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_public_artifact_retains_the_receipt_with_same_attempt_transport_evidence(self) -> None:
        digest = self.inputs("gcs", access="public")
        transport = self.canary_inputs("gcs")
        transition = Path(self.row) / "traces" / f"{fixture.CDN.PUBLIC_TRANSITION_CHECK}.json"
        (self.root / transition).unlink()
        self.assertEqual(self.assemble(digest, transport=transport), 1)
        retained = json.loads((self.output / transition).read_bytes())
        sdk_product = self.output / self.row / fixture.CDN.GIT_PRODUCT_DIR
        self.assertTrue((sdk_product / fixture.CDN.GIT_PRIOR_EXPORT_RECORD).is_file())
        self.assertEqual(retained["before"]["edge_attempts"], 0)
        self.assertEqual(retained["after"]["key_sha256"], self.example.object_key)
        public = Path(self.row) / fixture.CDN.PUBLIC_EXPORT_DIR
        for version in ("", fixture.CDN.PUBLIC_EXPORT_BEFORE):
            for name in fixture.CDN.PUBLIC_EXPORT_FILES:
                self.assertEqual((self.output / public / version / name).read_bytes(),
                                 (self.root / public / version / name).read_bytes())
        proof = f"{fixture.CDN.PUBLIC_EXPORT_CHECK}.json"
        self.assertEqual((self.output / self.row / "traces" / proof).read_bytes(), (transport / "gcs" / proof).read_bytes())

    def test_public_artifact_requires_the_raw_approved_inventory(self) -> None:
        digest = self.inputs("azure", access="public")
        (self.root / self.row / fixture.CDN.PUBLIC_EXPORT_DIR / "inventory.json").unlink()
        with self.assertRaisesRegex(ValueError, "regular publication artifact"):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_rejects_trace_with_another_workflow_attempt(self) -> None:
        digest = self.inputs("azure")
        trace = self.root / self.row / "traces/exact_get.json"
        evidence = json.loads(trace.read_bytes())
        evidence["workflow_run_attempt"] = "3"
        fixture.json_file(trace, evidence)
        with self.assertRaises(subprocess.CalledProcessError):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_missing_required_platform_or_unapproved_registration_leaves_no_artifact(self) -> None:
        digest = self.inputs(consumer="cli")
        with self.assertRaisesRegex(ValueError, "differ from the approved digest"):
            self.assemble("b" * 64)
        with self.assertRaisesRegex(ValueError, "missing required CDN platform .*darwin"):
            self.assemble(digest, {self.example.scope})
        self.assertFalse(self.output.exists())

    def test_assembles_direct_git_from_only_the_registered_measurement_files(self) -> None:
        digest = self.inputs("azure", consumer="cli")
        measurements = self.root / self.row / fixture.CDN.GIT_BENCHMARK_DIR
        sample = "near/0001/origin"
        (measurements / sample / "raw.log").write_text("local diagnostic output", encoding="utf-8")
        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        retained = row / fixture.CDN.GIT_BENCHMARK_DIR / sample
        publication = Path(sample).parent / "publication.json"
        self.assertEqual((row / fixture.CDN.GIT_BENCHMARK_DIR / publication).read_bytes(),
                         (measurements / publication).read_bytes())
        self.assertEqual({path.name for path in retained.iterdir()}, {"client.json", "measurement.json"})
        for file in retained.iterdir():
            self.assertEqual(file.read_bytes(), (measurements / sample / file.name).read_bytes())
        proof = Path("near/0001/warm_edge") / fixture.CDN.VIEWER_REQUEST_FILES["azure"]
        self.assertEqual((row / fixture.CDN.GIT_BENCHMARK_DIR / proof).read_bytes(), (measurements / proof).read_bytes())
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["performance"], self.example.report["performance"])
        self.assertEqual(report["product_action"], self.example.report["product_action"])
        self.assertEqual({path.name for path in (row / fixture.CDN.GIT_PRODUCT_DIR).iterdir()},
                         {"publication.json", "fetch.json", "retirement.json", fixture.CDN.PRODUCT_VIEWER_RECORD,
                          *fixture.CDN.GIT_NATIVE_REF_FILES.values()})

    def test_missing_git_measurement_prevents_artifact_publication(self) -> None:
        digest = self.inputs(consumer="cli")
        path = self.root / self.row / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001/origin/client.json"
        path.unlink()
        with self.assertRaises(FileNotFoundError):
            self.assemble(digest)
        self.assertFalse(self.output.exists())

    def test_assembles_lfs_performance_from_the_timed_payload_fetches(self) -> None:
        digest = self.inputs("azure", consumer="cli", family="lfs")
        self.assertEqual(self.assemble(digest), 1)
        row = self.output / self.row
        report = json.loads((row / "cdn-report.json").read_bytes())
        self.assertEqual(report["product_action"]["name"], "lfs-fetch")
        self.assertEqual(report["performance"], self.example.report["performance"])
        cached_client = row / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001/warm_local/client.json"
        self.assertEqual(json.loads(cached_client.read_bytes())["local_payload_hits"], 1)

    def test_cloudfront_viewer_records_are_preserved_with_measured_requests(self) -> None:
        digest = self.inputs("s3", consumer="cli")
        proof = Path(self.row) / fixture.CDN.GIT_BENCHMARK_DIR / "near/0001/warm_edge" / fixture.CDN.VIEWER_REQUEST_FILES["s3"]
        self.assertEqual(self.assemble(digest), 1)
        self.assertEqual((self.output / proof).read_bytes(), (self.root / proof).read_bytes())

    def test_cli_takes_native_reports_from_the_workflow_download(self) -> None:
        digest = self.inputs("gcs")
        downloaded = self.root / "downloaded"
        downloaded.mkdir()
        (self.root / "gcs").rename(downloaded / "gcs")
        (self.root / "gcs").mkdir()
        (self.root / "gcs/report.json").write_text("irrelevant runner file", encoding="utf-8")

        result = subprocess.run([
            sys.executable, str(Path(ASSEMBLE.__file__)),
            "--inputs", str(self.root), "--native-reports", str(downloaded), "--output", str(self.output),
            "--benchmarks-sha256", digest, "--source-sha", fixture.COMMIT, "--run-id", "123", "--run-attempt", "2",
        ], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.output / "gcs/report.json").read_bytes(), (downloaded / "gcs/report.json").read_bytes())

    def test_missing_workflow_native_report_cannot_use_the_measurement_copy(self) -> None:
        digest = self.inputs()
        downloaded = self.root / "downloaded"
        downloaded.mkdir()
        with self.assertRaises(FileNotFoundError):
            ASSEMBLE.assemble_bundle(
                self.root, self.output, digest, set(), fixture.COMMIT, "123", "2", native_reports=downloaded,
            )
        self.assertFalse(self.output.exists())

    def test_cli_imports_same_attempt_canary_bytes_without_redundant_measurements(self) -> None:
        digest = self.inputs("gcs")
        transport = self.canary_inputs("gcs")
        cache_reuse = fixture.CDN.CACHE_REUSE_CHECK
        filename = f"{cache_reuse}.json"
        (self.root / self.row / "traces" / filename).rename(transport / "gcs" / filename)
        observations = json.loads(self.observations.read_bytes())
        observations["check_results"].pop(cache_reuse)
        fixture.json_file(self.observations, observations)
        result = subprocess.run([
            sys.executable, str(Path(ASSEMBLE.__file__)), "--inputs", str(self.root), "--transport-traces", str(transport),
            "--output", str(self.output), "--benchmarks-sha256", digest, "--source-sha", fixture.COMMIT,
            "--run-id", "123", "--run-attempt", "2",
        ], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        retained = self.output / self.row / "traces"
        for path in (transport / "gcs").iterdir():
            self.assertEqual((retained / path.name).read_bytes(), path.read_bytes())
        self.assertTrue((retained / "cleanup.json").exists())
        policy = self.root / "qualified-scopes.txt"
        policy.write_text(fixture.release_policy_row(self.example.scope), encoding="utf-8")
        self.assertEqual(release.RELEASE.verify_bundle(self.output, policy, {self.example.scope}, fixture.COMMIT, "123", "2", digest), 1)

    def test_rejects_mixed_attempt_canary_before_copying_traces(self) -> None:
        digest = self.inputs("azure")
        transport = self.canary_inputs("azure")
        trace = transport / "azure/exact_range.json"
        observed = json.loads(trace.read_bytes()) | {"workflow_run_attempt": "1"}
        fixture.json_file(trace, observed)
        with self.assertRaisesRegex(ValueError, "canary trace provenance mismatch: exact_range"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_downloaded_canary_requires_its_authenticated_native_read(self) -> None:
        digest = self.inputs()
        transport = self.canary_inputs("s3")
        (transport / "s3/native_origin_read.json").unlink()
        with self.assertRaisesRegex(ValueError, "canary artifact is missing required transport observations"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_canary_expiry_observations_must_arrive_together(self) -> None:
        digest = self.inputs()
        transport = self.canary_inputs("s3")
        filename = f"{fixture.CDN.EXPIRY_RANGE_CHECK}.json"
        (self.root / self.row / "traces" / filename).rename(transport / "s3" / filename)
        with self.assertRaisesRegex(ValueError, "must include both expiry observations together"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_canary_from_a_different_fixture_cannot_supply_required_observations(self) -> None:
        digest = self.inputs()
        transport = self.canary_inputs("s3")
        for trace in (transport / "s3").iterdir():
            observed = json.loads(trace.read_bytes()) | {"configuration_sha256": "a" * 64}
            fixture.json_file(trace, observed)
        with self.assertRaisesRegex(ValueError, "invalid fields in measured check results"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_canary_cannot_replace_conflicting_measured_evidence(self) -> None:
        digest = self.inputs()
        transport = self.canary_inputs("s3")
        source = transport / "s3/exact_get.json"
        target = self.root / self.row / "traces" / source.name
        changed = json.loads(source.read_bytes())
        changed["requests"][0]["elapsed_ms"] += 2
        fixture.json_file(target, changed)
        with self.assertRaisesRegex(ValueError, "measured trace conflicts with same-attempt canary"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_canary_cannot_override_a_failed_measurement(self) -> None:
        digest = self.inputs()
        transport = self.canary_inputs("s3")
        observations = json.loads(self.observations.read_bytes())
        observations["check_results"]["exact_get"] = False
        fixture.json_file(self.observations, observations)
        with self.assertRaisesRegex(ValueError, "failed measured check: exact_get"):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_canary_policy_digest_must_match_benchmark_approval(self) -> None:
        digest = self.inputs("azure")
        transport = self.canary_inputs("azure")
        path = transport / "azure/deployment_policy.json"
        policy = json.loads(path.read_bytes())
        policy["components"]["viewer-policy"] = "0" * 64
        policy["deployment_policy_sha256"] = fixture.CDN.deployment_policy_digest("front-door", "private", policy["components"])
        fixture.json_file(path, policy)
        with self.assertRaises(subprocess.CalledProcessError):
            self.assemble(digest, transport=transport)
        self.assertFalse(self.output.exists())

    def test_refuses_to_replace_an_existing_artifact(self) -> None:
        digest = self.inputs()
        self.output.mkdir()
        existing = self.output / "keeper"
        existing.write_text("keep this", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "artifact directory must be new"):
            self.assemble(digest)
        self.assertEqual(existing.read_text(encoding="utf-8"), "keep this")


if __name__ == "__main__":
    unittest.main()
