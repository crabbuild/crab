"""Exercise the CDN supplement through the retained-evidence command."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from test_verify_provider_qualification_report import SCRIPT, valid_report

import cdn_qualification as CDN
from public_inventory import PUBLICATION_BINDING_FIELDS, SCHEMA as INVENTORY_SCHEMA, Object, publication_receipt


COMMIT = "a" * 40
CONFIGURATION = "c" * 64
REGIONS = {"near": "region-1", "distant": "region-2"}
EXPIRY = 120
FIXTURE_PREFIX = "crab-cdn-qualification/run-123-2"
EXPORT_BODY = b"approved"


def expiry_rows(allowed: dict, denied: dict) -> list[dict]:
    expiration = EXPIRY * 1_000_000
    return [
        response | {"range_offset": offset, "started_at_unix_us": start + offset, "finished_at_unix_us": start + offset + 100}
        for response, start in ((allowed, expiration - 1000), (denied, expiration + 2_000_000))
        for offset in (0, 1)
    ]


def json_file(path: Path, data: Any) -> str:
    raw = json.dumps(data).encode()
    path.write_bytes(raw)
    return hashlib.sha256(raw).hexdigest()


def sample() -> dict[str, Any]:
    metrics = dict.fromkeys(CDN.METRICS, 0) | {"client_bytes": 64, "peak_memory_bytes": 4096, "peak_disk_bytes": 8192}
    return {
        "origin": metrics | {"elapsed_ms": 300, "origin_gets": 1, "client_bytes": 8},
        "cold_edge": metrics | {"elapsed_ms": 420, "edge_misses": 1, "origin_fill_bytes": 8, "edge_bytes": 8},
        "warm_edge": metrics | {"elapsed_ms": 120, "edge_hits": 1, "edge_bytes": 8},
        "warm_local": metrics | {"elapsed_ms": 6, "local_payload_hits": 1},
    }


def write_git_measurements(inputs: Path, registration: dict, platform: str) -> None:
    provider = CDN.PROVIDER_FOR_EDGE[registration["scope"].split("/", 1)[0]]
    for ordinal, (location, region) in enumerate(registration["regions"].items()):
        for index in range(1, registration["samples"] + 1):
            oid = hashlib.sha1(f"{location}/{index}".encode()).hexdigest()
            blob = hashlib.sha256(oid.encode()).hexdigest()
            publication_digest = hashlib.sha256(f"{location}/{index}/receipt".encode()).hexdigest()
            for sequence, (arm, metrics) in enumerate(sample().items()):
                directory = inputs / location / f"{index:04d}" / arm
                directory.mkdir(parents=True, exist_ok=True)
                started = 1_000_000 * (ordinal + 1) + index * 10_000 + sequence * 1_000
                edge = int(arm in ("cold_edge", "warm_edge"))
                counters = dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
                    "verified_edge_payloads": edge, "edge_attempts": edge,
                    "all_edge_attempts": edge, "observed_edge_gets": edge,
                }
                client = counters | {key: metrics[key] for key in CDN.GIT_CLIENT_METRICS} | {
                    "schema": "crab.cdn-git-fetch/v1", "name": "git-fetch", "arm": CDN.GIT_MODES[arm],
                    "source_sha": registration["source_sha"], "scope": registration["scope"],
                    "configuration_sha256": registration["configuration_sha256"],
                    "workflow_run_id": "123", "workflow_run_attempt": "2", "platform": platform,
                    "publication_oid": oid, "expected_sha256": blob, "observed_sha256": blob,
                    "publication_sha256": publication_digest,
                    "expected_bytes": CDN.GIT_PAYLOAD_SIZE, "observed_bytes": CDN.GIT_PAYLOAD_SIZE,
                    "started_at_unix_ms": started, "finished_at_unix_ms": started + metrics["elapsed_ms"],
                    "visible_new_ref": True, "native_writes": 1,
                }
                digest = json_file(directory / "client.json", client)
                external = {key: metrics[key] for key in CDN.GIT_EXTERNAL_METRICS} | {
                    "schema": "crab.cdn-git-measurement/v1", "provider": provider,
                    "region": region, "client_report_sha256": digest,
                }
                json_file(directory / "measurement.json", external)


class CdnQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        (self.directory / "traces").mkdir()

    def configure(self, provider: str, access: str, profile: str, *, consumer: str = "cli",
                  platform: str = "linux-x86_64") -> None:
        self.provider = provider
        self.scope = f"{CDN.PROVIDERS[provider][0]}/{access}/{consumer}/git/{profile}"
        self.origin = valid_report(provider)
        self.origin["service"] = CDN.PROVIDERS[provider][1]
        origin_digest = json_file(self.directory / "report.json", self.origin)
        edge = CDN.PROVIDERS[provider][0]
        names = CDN.deployment_component_names(edge, access)
        self.policy_components = {name: hashlib.sha256(name.encode()).hexdigest() for name in names}
        self.policy_components["viewer-key-ids"] = CDN.deployment_component_digest(["replacement"] if access == "private" else [])
        self.object_key = "e" * 64
        publication = {}
        if access == "public":
            publication = {"publication_sha256": self.write_public_export(provider)}
        policy_digest = CDN.deployment_policy_digest(edge, access, self.policy_components)
        self.registration = {
            "schema": "crab.cdn-benchmark/v1", "source_sha": COMMIT, "scope": self.scope,
            "configuration_sha256": CONFIGURATION, "workload": "git-fetch", "samples": 30,
            "p95_budget_ms": 150, "cost_budget_microusd": 10, "regions": REGIONS, "deployment_policy_sha256": policy_digest,
        } | publication
        self.report = {
            "schema": CDN.SCHEMA, "schema_version": 1, "status": "qualified", "scope": self.scope,
            "source_sha": COMMIT, "workflow_run_id": "123", "workflow_run_attempt": "2",
            "origin_report_sha256": origin_digest, "configuration_sha256": CONFIGURATION,
            "deployment": "fixture-a", "platform": platform,
            "fixture_prefix": FIXTURE_PREFIX, "checks": {},
            "product_action": {
                "name": "git-fetch", "expected_sha256": "b" * 64, "observed_sha256": "b" * 64,
                "expected_bytes": 8, "observed_bytes": 8, "native_writes": 1, "observed_edge_gets": 1,
                "visible_new_ref": True, "fixture_cleanup_verified": True, "verified_edge_payloads": 1,
            },
            "performance": {
                "near": {"region": "region-1", "pairs": [sample() for _ in range(30)]},
                "distant": {"region": "region-2", "pairs": [sample() for _ in range(30)]},
            },
        }
        edge_hit = self.request("edge", 200, "hit")
        edge_deny = self.request("edge", 403)
        control = self.request("edge", 200) | {"key_sha256": "0" * 64}
        origin_read = self.request("origin", 200)
        origin_deny = self.request("origin", 403)
        observations = {
            "exact_get": [edge_hit],
            CDN.PUBLIC_EXPORT_CHECK: [edge_hit | {"body_sha256": hashlib.sha256(EXPORT_BODY).hexdigest()}],
            "exact_range": [self.request("edge", 206, "hit") | {"bytes": 1}],
            "native_origin_read": [origin_read],
            "origin_only_control": [origin_read],
            "mutable_route_denied": [edge_deny],
            "neighbor_route_denied": [edge_deny],
            "edge_route_policy": [edge_hit, edge_deny, *(
                edge_deny for _ in CDN.EDGE_ROUTE_QUERY_PROBES[CDN.PROVIDERS[provider][0]]
            ), edge_hit],
            "primary_and_second_principal": [self.request("edge", 200), edge_hit],
            "warm_anonymous_viewer_denied": [edge_hit, edge_deny, edge_deny, edge_hit],
            "warm_expired_viewer_denied": [edge_hit, edge_deny],
            "warm_unrelated_principal_denied": [edge_hit, control, edge_deny, edge_deny, control, edge_hit],
            CDN.EXPIRY_RANGE_CHECK: expiry_rows(edge_hit | {"status": 206, "bytes": 1}, edge_deny),
            "scoped_view_isolation": [edge_deny],
            "private_origin_denied": [origin_deny],
            "live_origin_denied": [origin_deny],
            "export_origin_access": [origin_deny if provider == "s3" else origin_read],
            "public_listing_denied": [origin_deny],
            "authorized_cache_reuse": [self.request("edge", 200, "miss"), edge_hit],
            "rollback": [edge_hit, origin_read],
        }
        for check in CDN.required_check_names(access, profile):
            if check == "cleanup":
                self.cleanup_trace(access, CDN.PROVIDERS[provider][0])
            elif check == "deployment_policy":
                self.deployment_trace(self.policy_components, policy_digest)
            elif check == CDN.KEY_ROTATION_CHECK:
                self.rotation_trace(edge)
            else:
                self.trace(check, observations.get(check, [edge_hit]))
        if CDN.requires_git_workload(self.scope):
            write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform)
            self.record_git_product()

    def write_public_export(self, provider: str) -> str:
        source, export, deployment = {
            "s3": ("s3://source-bucket", "s3://export-bucket", "arn:aws:cloudfront::123456789012:distribution/EDIST"),
            "gcs": ("gs://source-bucket", "gs://export-bucket", "projects/crab-test/global/backendBuckets/export"),
            "azure": ("https://sourceacct.blob.core.windows.net/repo", "https://exportacct.blob.core.windows.net/repo",
                      "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Cdn/profiles/fixture/afdEndpoints/export/routes/objects"),
        }[provider]
        root = self.directory / CDN.PUBLIC_EXPORT_DIR
        root.mkdir(exist_ok=True)
        self.public_origins = {"native": source, "export": export}
        entry = Object("v2/pack-layers/aa/" + "a" * 64, len(EXPORT_BODY), hashlib.sha256(EXPORT_BODY).hexdigest())
        self.object_key = hashlib.sha256(f"{FIXTURE_PREFIX}/{entry.path}".encode()).hexdigest()
        inventory = json_file(root / "inventory.json", {
            "schema": INVENTORY_SCHEMA, "source_origin": source, "repository_prefix": FIXTURE_PREFIX,
            "objects": [vars(entry)],
        })
        receipt = publication_receipt(source, export, FIXTURE_PREFIX, "https://export.example.com", deployment, inventory, [entry])
        self.policy_components["public-export"] = CDN.deployment_component_digest({
            name: receipt[name] for name in PUBLICATION_BINDING_FIELDS
        })
        return json_file(root / "publication.json", receipt)

    def cleanup_trace(self, access: str, edge: str, **changes: Any) -> None:
        inventories = CDN.CLEANUP_INVENTORIES[edge][0]
        targets = (
            {role: {"origin_sha256": hashlib.sha256(origin.encode()).hexdigest(), "inventories": inventories}
             for role, origin in self.public_origins.items()}
            if access == "public" else {"native": {"origin_sha256": "1" * 64, "inventories": inventories}}
        )
        trace = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS} | {
            "schema": "crab.cdn-cleanup-trace/v1", "check": "cleanup", "targets": targets,
            "prefix_sha256": hashlib.sha256((self.report["fixture_prefix"] + "/").encode()).hexdigest(),
            "observed_at_unix_ms": time.time_ns() // 1_000_000,
        }
        digest = json_file(self.directory / "traces/cleanup.json", trace | changes)
        self.report["checks"]["cleanup"] = {"passed": True, "trace_sha256": digest}

    def record_git_product(self) -> None:
        sample = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001/warm_edge/client.json"
        fetch = json.loads(sample.read_bytes())
        records = self.directory / CDN.GIT_PRODUCT_DIR
        records.mkdir(exist_ok=True)
        provenance = {key: fetch[key] for key in CDN.CDN_PROVENANCE_FIELDS}
        receipt = json_file(records / "publication.json", provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef",
            "oid": fetch["publication_oid"], "sha256": fetch["expected_sha256"], "published": True,
            "visible_at_unix_ms": fetch["started_at_unix_ms"] - 50,
        })
        json_file(records / "fetch.json", fetch | {"publication_sha256": receipt})
        json_file(records / "retirement.json", provenance | {
            "schema": "crab.cdn-git-retirement/v1", "publication_sha256": receipt, "ref_retired": True,
            "retired_at_unix_ms": fetch["finished_at_unix_ms"] + 50,
        })
        self.report["product_action"].update({key: fetch[key] for key in self.report["product_action"] if key in fetch})

    def request(self, source: str, status: int, cache: str = "unknown") -> dict[str, Any]:
        return {"source": source, "status": status, "bytes": 0 if status in (401, 403) else 8,
                "cache": cache, "elapsed_ms": 1,
                "key_sha256": self.object_key}

    def trace(self, check: str, requests: list[dict[str, Any]], **extra: Any) -> None:
        path = self.directory / "traces" / f"{check}.json"
        if check == "edge_route_policy":
            probes = CDN.edge_route_probe_names(self.scope.split("/", 1)[0])
            requests = [{"probe": probe} | row for probe, row in zip(probes, requests)]
        if check == CDN.EXPIRY_RANGE_CHECK:
            extra.setdefault("viewer_expires_at_unix", EXPIRY)
        trace = {key: self.report[key] for key in (
            "source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt",
        )}
        trace.update({"schema": "crab.cdn-trace/v1", "check": check, "requests": requests} | extra)
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, trace)}

    def deployment_trace(self, components: dict, digest: str, **extra: Any) -> None:
        trace = {key: self.report[key] for key in (
            "source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt",
        )}
        trace.update({"schema": "crab.cdn-deployment-policy/v1", "components": components,
                      "deployment_policy_sha256": digest} | extra)
        path = self.directory / "traces" / "deployment_policy.json"
        self.report["checks"]["deployment_policy"] = {"passed": True, "trace_sha256": json_file(path, trace)}

    def rotation_trace(self, edge: str) -> None:
        hit = self.request("edge", 200, "hit")
        deny = self.request("edge", 403)
        overlap = self.policy_components | {
            "viewer-key-ids": CDN.deployment_component_digest(["replacement", "retired"]),
            CDN.ROTATING_POLICY[edge]: "9" * 64,
        }
        trace = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS} | {
            "schema": "crab.cdn-key-rotation/v1",
            "grants": {role: {"key_id": role, "expires_at_unix": EXPIRY} for role in ("retired", "replacement")},
            "policies": {phase: {"observed_at_unix_us": time_us, "components": components}
                         for phase, time_us, components in (
                             ("overlap", 100_000, overlap), ("retired", 800_000, self.policy_components),
                             ("confirmed", 2_200_000, self.policy_components),
                         )},
            "requests": [row | {"phase": phase, "signer": role, "read": read,
                                "started_at_unix_us": index * 300_000, "finished_at_unix_us": index * 300_000 + 10}
                         for index, (phase, role, read, row) in enumerate((
                             ("overlap", "retired", "full", hit),
                             ("overlap", "replacement", "full", hit),
                             ("retired", "replacement", "full", hit),
                             ("retired", "retired", "full", deny),
                             ("retired", "retired", "range", deny),
                             ("retired", "replacement", "full", hit),
                         ), 1)],
        }
        check = CDN.KEY_ROTATION_CHECK
        path = self.directory / "traces" / f"{check}.json"
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, trace)}

    def command(self, *extra: str) -> subprocess.CompletedProcess[str]:
        report = self.directory / "cdn-report.json"
        registration = self.directory / "benchmark.json"
        json_file(report, self.report)
        json_file(registration, self.registration)
        return subprocess.run([
            sys.executable, str(SCRIPT), str(self.directory / "report.json"),
            "--provider", self.provider, "--source-sha", COMMIT, "--run-id", "123", "--run-attempt", "2",
            "--cdn-report", str(report), "--cdn-scope", self.scope, "--cdn-configuration-sha256", CONFIGURATION,
            "--cdn-platform", "linux-x86_64", "--cdn-benchmark-registration", str(registration), *extra,
        ], check=False, capture_output=True, text=True)

    def rejected(self, reason: str, *extra: str) -> None:
        result = self.command(*extra)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(reason, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_accepts_all_cloud_providers_and_modes(self) -> None:
        for provider, access, profile in (("s3", "private", "direct"), ("gcs", "public", "direct"),
                                          ("azure", "private", "managed")):
            with self.subTest(provider=provider):
                self.configure(provider, access, profile)
                result = self.command()
                self.assertEqual(result.returncode, 0, result.stderr)
                cdn = json.loads(result.stdout)["cdn"]
                self.assertEqual(cdn["scope"], self.scope)
                self.assertEqual(cdn["distant_median_savings_lower_bound_ms"], 180)

    def test_rejects_generic_rotation_trace(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace(CDN.KEY_ROTATION_CHECK, [self.request("edge", 200, "hit")])
        self.rejected("invalid fields in viewer key rotation trace")

    def test_rotation_observations_are_bound_to_live_keys_and_fixture(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        check = CDN.KEY_ROTATION_CHECK
        path = self.directory / "traces" / f"{check}.json"
        original = path.read_bytes()
        cases = (
            (lambda trace: trace["grants"]["retired"].update(expires_at_unix=0),
             "rotation grant needs a valid signer and signed expiration"),
            (lambda trace: trace["grants"]["retired"].update(expires_at_unix=1),
             "rotation probes must match the deployed phase"),
            (lambda trace: trace["policies"]["retired"]["components"].update({"viewer-key-ids": "a" * 64}),
             "rotation policy must attest the expected deployed key IDs"),
            (lambda trace: trace["policies"]["overlap"]["components"].update({"bucket-policy": "a" * 64}),
             "deployment changed outside viewer signing controls"),
            (lambda trace: trace["requests"][4].update(read="full"), "rotation probes must match the deployed phase"),
            (lambda trace: trace["requests"][-1].update(cache="miss"), "complete warm object"),
            (lambda trace: [row.update(key_sha256="0" * 64) for row in trace["requests"]],
             "rotation must probe the same complete object verified by exact GET"),
        )
        for mutate, reason in cases:
            with self.subTest(reason=reason):
                trace = json.loads(original)
                mutate(trace)
                self.report["checks"][check]["trace_sha256"] = json_file(path, trace)
                self.rejected(reason)

    def test_rejects_a_passing_compatible_origin_mock(self) -> None:
        self.configure("s3", "private", "direct")
        self.origin["service"] = "rustfs-1.0.0-ga"
        self.report["origin_report_sha256"] = json_file(self.directory / "report.json", self.origin)
        self.rejected("matching real cloud provider")

    def test_rejects_changed_origin_report_even_at_same_commit_and_run(self) -> None:
        self.configure("gcs", "private", "direct")
        self.origin["region"] = "different-region"
        json_file(self.directory / "report.json", self.origin)
        self.rejected("commit, run, attempt and digest")

    def test_rejects_another_consumer_or_platform(self) -> None:
        self.configure("azure", "private", "direct")
        self.rejected("does not match the required row", "--cdn-scope", self.scope.replace("/cli/", "/sdk/"))
        self.rejected("platform does not match", "--cdn-platform", "windows-x86_64")

    def test_release_verification_requires_the_exact_policy_row(self) -> None:
        self.configure("s3", "private", "direct")
        policy = self.directory / "qualified-scopes.txt"
        args = ("--cdn-release-policy", str(policy))
        policy.write_text("# Release rows\n" + self.scope.replace("/cli/", "/sdk/") + "\n")
        self.rejected("scope is absent from the release policy", *args)
        policy.write_text(self.scope + "\n")
        result = self.command(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        policy.write_text(self.scope + "\n" + self.scope + "\n")
        self.rejected("duplicate CDN release scope", *args)

    def test_repository_release_policy_contains_only_valid_rows(self) -> None:
        policy = SCRIPT.parents[2] / "crates/crab-storage/src/cdn/qualified-scopes.txt"
        CDN.release_scopes(policy)

    def test_rejects_unqualified_or_unproven_managed_mode(self) -> None:
        self.configure("s3", "private", "managed")
        self.report["status"] = "fixture-only"
        self.rejected("does not claim a qualified CDN row")
        self.report["status"] = "qualified"
        del self.report["checks"]["managed_v2_issuer"]
        self.rejected("invalid fields in checks")

    def test_rejects_public_global_reads(self) -> None:
        self.configure("s3", "public", "direct")
        self.scope = self.scope.replace("/git/", "/xorb/")
        self.report["scope"] = self.scope
        self.rejected("public qualification requires direct Git, capsule, or LFS")

    def test_rejects_a_range_served_by_origin_instead_of_edge(self) -> None:
        self.configure("azure", "private", "direct")
        self.trace("exact_range", [self.request("origin", 206)])
        self.rejected("positive read evidence missing: exact_range")

    def test_range_must_be_partial_and_observe_the_full_get_key(self) -> None:
        self.configure("azure", "public", "direct")
        ranged = self.request("edge", 206, "hit")
        for invalid in (ranged, ranged | {"key_sha256": "f" * 64, "bytes": 1}):
            with self.subTest(invalid=invalid):
                self.trace("exact_range", [invalid])
                self.rejected("exact range and full GET must observe the same edge object")

    def test_origin_only_control_rejects_edge_traffic(self) -> None:
        self.configure("gcs", "public", "direct")
        self.trace("origin_only_control", [self.request("origin", 200), self.request("edge", 403)])
        self.rejected("origin-only control observed edge traffic")

    def test_origin_denial_needs_the_same_known_object_from_an_authorized_native_read(self) -> None:
        self.configure("azure", "private", "direct")
        native = self.request("origin", 200)
        for invalid in (native | {"key_sha256": "f" * 64}, native | {"bytes": 1}, native | {"cache": "hit"}):
            with self.subTest(invalid=invalid):
                self.trace("native_origin_read", [invalid])
                self.rejected("anonymous origin denial needs a verified native read of the same complete edge object")

    def test_public_export_access_must_match_the_provider_policy(self) -> None:
        for provider, status, reason in (
            ("s3", 200, "the S3 export must deny anonymous object reads"),
            ("gcs", 403, "the public export must serve the complete approved object"),
            ("azure", 401, "the public export must serve the complete approved object"),
        ):
            with self.subTest(provider=provider):
                self.configure(provider, "public", "direct")
                self.trace("export_origin_access", [self.request("origin", status)])
                self.rejected(reason)

    def test_public_release_requires_the_exact_preregistered_export_receipt(self) -> None:
        for provider in ("s3", "gcs", "azure"):
            with self.subTest(provider=provider):
                self.configure(provider, "public", "direct", consumer="sdk")
                result = self.command()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.registration["publication_sha256"] = "a" * 64
                self.rejected("publication receipt differs from the preregistered SHA-256")

    def test_public_release_pins_the_complete_approved_inventory(self) -> None:
        self.configure("s3", "public", "direct", consumer="sdk")
        inventory = self.directory / CDN.PUBLIC_EXPORT_DIR / "inventory.json"
        document = json.loads(inventory.read_bytes())
        document["objects"].append({"path": "v2/pack-layers/bb/" + "b" * 64, "sha256": "d" * 64, "size": 60})
        json_file(inventory, document)
        self.rejected("publication receipt does not match the exact approved inventory")

    def test_public_release_rejects_export_approval_for_another_deployment(self) -> None:
        self.configure("azure", "public", "direct", consumer="sdk")
        path = self.directory / CDN.PUBLIC_EXPORT_DIR / "publication.json"
        receipt = json.loads(path.read_bytes())
        receipt["deployment"] += "-other"
        self.registration["publication_sha256"] = json_file(path, receipt)
        self.rejected("publication receipt must match the attested public export")

    def test_public_release_rejects_cleanup_of_another_export(self) -> None:
        self.configure("azure", "public", "direct", consumer="sdk")
        path = self.directory / "traces" / "cleanup.json"
        trace = json.loads(path.read_bytes())
        trace["targets"]["export"]["origin_sha256"] = "b" * 64
        self.report["checks"]["cleanup"]["trace_sha256"] = json_file(path, trace)
        self.rejected("public cleanup must cover the source and destination of the approved publication")

    def test_public_release_requires_the_probed_object_to_match_the_receipt(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        check = CDN.PUBLIC_EXPORT_CHECK
        verified = self.request("edge", 200, "hit") | {"body_sha256": hashlib.sha256(EXPORT_BODY).hexdigest()}
        for change, reason in (
            ({"body_sha256": "f" * 64}, "verified edge object differs from the approved publication entry"),
            ({"key_sha256": "f" * 64}, "verified edge object is absent from the approved publication"),
            ({"elapsed_ms": 2}, "public export proof must cover the same edge, native, and export object"),
        ):
            with self.subTest(change=change):
                self.trace(check, [verified | change])
                self.rejected(reason)

    def test_listing_trace_cannot_pass_when_anonymous_listing_also_succeeds(self) -> None:
        self.configure("gcs", "public", "direct")
        self.trace("public_listing_denied", [self.request("origin", 403), self.request("origin", 200)])
        self.rejected("authorization denial evidence missing: public_listing_denied")

    def test_completed_get_does_not_replace_accepted_git_payload(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["product_action"]["verified_edge_payloads"] = 0
        self.rejected("accepted no verified edge Git payload")

    def test_warm_local_performance_requires_a_payload_cache_hit(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["performance"]["near"]["pairs"][0]["warm_local"]["local_payload_hits"] = 0
        self.rejected("warm local sample must prove a local payload hit")

    def test_rejects_a_valid_summary_that_does_not_match_retained_git_fetches(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["performance"]["near"]["pairs"][0]["warm_edge"]["elapsed_ms"] += 1
        self.rejected("Git performance summary differs from the retained measurements")

    def test_rejects_a_positive_product_summary_that_differs_from_the_fetch(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["product_action"]["verified_edge_payloads"] += 1
        self.rejected("Git product action differs from the retained publication, fetch, and retirement evidence")

    def test_product_record_requires_retirement_of_the_same_receipt_after_fetch(self) -> None:
        self.configure("gcs", "private", "direct")
        path = self.directory / CDN.GIT_PRODUCT_DIR / "retirement.json"
        retirement = json.loads(path.read_bytes())
        for changes in ({"publication_sha256": "f" * 64}, {"retired_at_unix_ms": 1}):
            with self.subTest(changes=changes):
                json_file(path, retirement | changes)
                self.rejected("requires retirement of the same published ref after the fetch")

    def test_cleanup_inventory_must_be_captured_after_git_retirement(self) -> None:
        self.configure("gcs", "private", "direct")
        self.cleanup_trace("private", "cloud-cdn", observed_at_unix_ms=1)
        self.rejected("fixture cleanup must be verified after Git ref retirement")

    def test_cleanup_must_check_the_fixture_prefix_and_both_public_origins(self) -> None:
        self.configure("azure", "public", "direct")
        self.cleanup_trace("public", "front-door", prefix_sha256="f" * 64)
        self.rejected("cleanup inventory must observe the approved fixture prefix")
        target = {"origin_sha256": "1" * 64, "inventories": {"versions_and_snapshots": "empty"}}
        self.cleanup_trace("public", "front-door", targets={"native": target, "export": target})
        self.rejected("cleanup must check the live origin and export independently")

    def test_performance_measurements_follow_the_preregistered_regions(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        self.report["performance"]["near"]["region"] = "other-region"
        self.rejected("client region differs from registration: near")

    def test_cold_edge_fill_requires_delivered_payload_bytes(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["performance"]["near"]["pairs"][0]["cold_edge"]["edge_bytes"] = 0
        self.rejected("cold edge sample must prove a miss, origin fill and delivered bytes")

    def test_rollback_rejects_native_read_for_a_different_key(self) -> None:
        self.configure("azure", "private", "direct")
        self.trace("rollback", [self.request("edge", 200), self.request("origin", 200) | {"key_sha256": "c" * 64}])
        self.rejected("rollback requires an edge read followed by a native read of the same complete object")

    def test_cache_reuse_requires_a_hit_after_the_miss(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace("authorized_cache_reuse", [self.request("edge", 200, "hit"), self.request("edge", 200, "miss")])
        self.rejected("cache reuse requires a miss followed by a hit")

    def test_second_principal_requires_a_warm_read_of_the_same_key(self) -> None:
        self.configure("s3", "private", "direct")
        full = self.request("edge", 200)
        different_key = self.request("edge", 200, "hit") | {"key_sha256": "f" * 64}
        for invalid in ([full], [full, full], [full, different_key]):
            with self.subTest(invalid=invalid):
                self.trace("primary_and_second_principal", invalid)
                self.rejected("the second principal requires a cache hit")

    def test_rejects_expiry_without_evidence_the_edge_was_warm(self) -> None:
        self.configure("azure", "private", "direct")
        self.trace("warm_expired_viewer_denied", [self.request("edge", 403)])
        self.rejected("authorized warm hit evidence missing")
        denied = self.request("edge", 403) | {"key_sha256": "0" * 64}
        self.trace("warm_expired_viewer_denied", [self.request("edge", 200, "hit"), denied])
        self.rejected("authorized warm hit evidence missing")

    def test_parallel_expiry_requires_overlapping_verified_ranges_on_both_sides_of_expiry(self) -> None:
        self.configure("gcs", "private", "direct")
        allowed = self.request("edge", 206, "hit") | {"bytes": 1}
        denied = self.request("edge", 403)
        check = CDN.EXPIRY_RANGE_CHECK
        rows = expiry_rows(allowed, denied)
        for index, change, error in (
            (3, {"range_offset": 0}, "two distinct byte ranges"),
            (3, {"started_at_unix_us": rows[2]["finished_at_unix_us"],
                 "finished_at_unix_us": rows[2]["finished_at_unix_us"] + 100}, "overlap in each phase"),
            (1, {"finished_at_unix_us": EXPIRY * 1_000_000}, "before the viewer grant expires"),
            (2, {"started_at_unix_us": EXPIRY * 1_000_000}, "denied after the viewer grant expires"),
            (3, {"status": 206, "bytes": 1}, "denied after the viewer grant expires"),
        ):
            with self.subTest(error=error, change=change):
                altered = rows.copy()
                altered[index] = rows[index] | change
                self.trace(check, altered)
                self.rejected(error)
        self.trace(check, [row | {"key_sha256": "d" * 64} for row in rows])
        self.rejected("expired parallel ranges must probe the same warm object")

    def test_anonymous_denial_requires_a_warm_hit_after_both_denials(self) -> None:
        self.configure("gcs", "private", "direct")
        hit = self.request("edge", 200, "hit")
        denied = self.request("edge", 403)
        self.trace("warm_anonymous_viewer_denied", [hit, denied, denied, self.request("edge", 200, "miss")])
        self.rejected("authorized warm hit evidence missing after anonymous denial")

    def test_unrelated_viewer_denial_requires_a_valid_separate_control(self) -> None:
        self.configure("azure", "private", "direct")
        hit, deny = self.request("edge", 200, "hit"), self.request("edge", 403)
        control = self.request("edge", 200) | {"key_sha256": "0" * 64}
        key = "warm_unrelated_principal_denied"
        self.trace(key, [hit, deny])
        self.rejected("unrelated viewer denial requires six edge observations")
        self.trace(key, [hit, control, deny, deny, deny | {"key_sha256": control["key_sha256"]}, hit])
        self.rejected("unrelated viewer must read the same distinct control object before and after denial")
        self.trace(key, [hit, hit, deny, deny, hit, hit])
        self.rejected("unrelated viewer must read the same distinct control object before and after denial")
        self.trace(key, [hit, control, deny, hit | {"status": 206, "bytes": 1}, control, hit])
        self.rejected("unrelated viewer must be denied a full and ranged GET of the warm target")

    def test_different_object_cannot_supply_the_warm_denial_observation(self) -> None:
        self.configure("azure", "private", "direct")
        hit = self.request("edge", 200, "hit") | {"key_sha256": "f" * 64}
        denied = self.request("edge", 401) | {"key_sha256": "f" * 64}
        self.trace("warm_anonymous_viewer_denied", [hit, denied, denied, hit])
        self.rejected("warm denial must probe the complete object verified by exact GET")

    def test_policy_probe_must_deny_the_documented_query_variants(self) -> None:
        self.configure("azure", "private", "direct")
        hit, deny = self.request("edge", 200, "hit"), self.request("edge", 403)
        absent = self.request("edge", 404) | {"bytes": 0}
        self.trace("edge_route_policy", [hit, absent, deny, absent, deny, hit])
        self.rejected("edge route policy must deny version, snapshot, and response override queries")
        self.trace("edge_route_policy", [hit, absent, deny, deny | {"probe": "version"}, deny, hit])
        self.rejected("edge route policy trace lacks required probes")

    def test_policy_probe_must_end_with_a_hit_of_the_verified_object(self) -> None:
        self.configure("s3", "public", "direct")
        hit, deny = self.request("edge", 200, "hit"), self.request("edge", 403)
        self.trace("edge_route_policy", [hit, deny, deny, deny, hit | {"cache": "miss"}])
        self.rejected("edge route policy requires same-object cache hits before and after denials")

    def test_rejects_trace_with_no_observations(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace("rollback", [])
        self.rejected("trace has no observations: rollback")

    def test_deployment_attestation_requires_the_preregistered_digest(self) -> None:
        self.configure("s3", "private", "direct")
        changed = self.policy_components | {"viewer-function": "9" * 64}
        digest = CDN.deployment_policy_digest("cloudfront", "private", changed)
        self.deployment_trace(changed, digest)
        self.rejected("observed deployment policy differs from the approved policy")

    def test_deployment_policy_cannot_be_an_unrelated_http_success(self) -> None:
        self.configure("gcs", "public", "direct")
        self.trace("deployment_policy", [self.request("edge", 200, "hit")])
        self.rejected("invalid fields in deployment policy trace")

    def test_azure_attestation_requires_blob_access_evidence(self) -> None:
        self.configure("azure", "private", "direct")
        without_blob = {name: digest for name, digest in self.policy_components.items() if name != "blob-access"}
        self.deployment_trace(without_blob, self.registration["deployment_policy_sha256"])
        self.rejected("deployment policy components are incomplete")

    def test_rejects_cache_sample_above_the_registered_cost_budget(self) -> None:
        self.configure("gcs", "public", "direct")
        self.report["performance"]["near"]["pairs"][0]["warm_edge"]["cost_microusd"] = 11
        self.rejected("warm edge exceeds the registered cost budget")

    def test_rejects_traces_containing_headers_and_reused_trace_artifacts(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace("exact_get", [self.request("edge", 200, "hit")], headers={"authorization": "fake"})
        self.rejected("invalid fields in exact_get")
        self.trace("exact_get", [self.request("edge", 200, "hit")], workflow_run_id="456")
        self.rejected("trace provenance mismatch")
        (self.directory / "traces" / "exact_get.json").write_text("{}")
        self.rejected("trace digest mismatch")

    def test_rejects_digest_mismatch_unregistered_budget_and_no_benefit(self) -> None:
        self.configure("gcs", "public", "direct")
        self.report["product_action"]["observed_sha256"] = "f" * 64
        self.rejected("did not reconstruct the expected bytes")
        self.report["product_action"]["observed_sha256"] = "b" * 64
        self.registration["p95_budget_ms"] = 100
        self.rejected("p95 exceeds the registered budget")
        self.registration["p95_budget_ms"] = 350
        for pair in self.report["performance"]["distant"]["pairs"][:12]:
            pair["warm_edge"]["elapsed_ms"] = 305
        self.rejected("did not show positive paired median savings")


if __name__ == "__main__":
    unittest.main()
