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
PUBLIC_SEAL = 1_011_990


def azure_resource(account: str) -> str:
    return ("/subscriptions/11111111-2222-3333-4444-555555555555/resourcegroups/fixture/"
            f"providers/microsoft.storage/storageaccounts/{account}/blobservices/default/containers/repo")


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


def sample(provider: str) -> dict[str, Any]:
    metrics = dict.fromkeys(CDN.METRICS, 0) | {
        "client_bytes": 64, "origin_gets": 1, "peak_memory_bytes": 4096, "peak_disk_bytes": 8192,
    }
    return {
        "origin": metrics | {"elapsed_ms": 300, "client_bytes": 8},
        "cold_edge": metrics | {"elapsed_ms": 420, "edge_misses": 1, "origin_fill_bytes": 8, "edge_bytes": 8,
                                "origin_gets": 2 if provider in ("s3", "azure") else 1},
        "warm_edge": metrics | {"elapsed_ms": 120, "edge_hits": 1, "edge_bytes": 8},
        "warm_local": metrics | {"elapsed_ms": 6, "local_payload_hits": 1},
    }


def cli_candidate(scope: str, platform: str) -> dict:
    system, arch = platform.split("-", 1)
    target = {"darwin": "apple-darwin", "linux": "unknown-linux-gnu", "windows": "pc-windows-msvc"}[system]
    return {
        "source_sha": COMMIT, "target_triple": f"{arch}-{target}", "cargo_profile": "release",
        "cargo_features": "default,tier",
        "qualification_feature": False, "binary_sha256": hashlib.sha256(platform.encode()).hexdigest(),
        "release_policy_sha256": hashlib.sha256(f"{scope}\n".encode()).hexdigest(),
    }


def sdk_candidate(scope: str, platform: str) -> dict:
    return cli_candidate(scope, platform) | {
        "cargo_features": "default,files,local,publish,remote,service",
        "binary_sha256": hashlib.sha256(f"sdk/{platform}".encode()).hexdigest(),
    }


def write_git_measurements(inputs: Path, registration: dict, platform: str, sdk_native: dict | None = None) -> None:
    provider = CDN.PROVIDER_FOR_EDGE[registration["scope"].split("/", 1)[0]]
    sdk = CDN.is_direct_sdk_git_scope(registration["scope"])
    action = CDN.cli_action_name(registration["scope"])
    for ordinal, (location, region) in enumerate(registration["regions"].items()):
        for index in range(1, registration["samples"] + 1):
            oid = hashlib.sha1(f"{location}/{index}".encode()).hexdigest()
            blob = hashlib.sha256(oid.encode()).hexdigest()
            publication_digest = hashlib.sha256(f"{location}/{index}/receipt".encode()).hexdigest()
            for sequence, (arm, metrics) in enumerate(sample(provider).items()):
                directory = inputs / location / f"{index:04d}" / arm
                directory.mkdir(parents=True, exist_ok=True)
                started = 1_000_000 * (ordinal + 1) + index * 10_000 + sequence * 1_000
                edge = int(arm in ("cold_edge", "warm_edge"))
                counters = dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
                    "verified_edge_payloads": edge, "edge_attempts": edge,
                    "all_edge_attempts": edge, "observed_edge_gets": edge,
                }
                client = counters | {key: metrics[key] for key in CDN.GIT_CLIENT_METRICS} | {
                    "schema": f"crab.cdn-{action}/v1", "name": action, "arm": CDN.GIT_MODES[arm],
                    "source_sha": registration["source_sha"], "scope": registration["scope"],
                    "configuration_sha256": registration["configuration_sha256"],
                    "workflow_run_id": "123", "workflow_run_attempt": "2", "platform": platform,
                    "candidate": registration["sdk_candidate" if sdk else "candidate"],
                    "reader": {
                        "binding_sha256": "" if arm == "origin" else registration["viewer_binding_sha256"],
                        "cache_root_sha256": hashlib.sha256(f"{location}/{index}/{arm}/cache".encode()).hexdigest(),
                        "public_export_sha256": "" if arm == "origin" else registration.get("publication_sha256", ""),
                        "origin_sha256": "7" * 64, "prefix_sha256": "8" * 64,
                    },
                    "publication_oid": oid, "expected_sha256": blob, "observed_sha256": blob,
                    "publication_sha256": publication_digest,
                    "expected_bytes": CDN.GIT_PAYLOAD_SIZE, "observed_bytes": CDN.GIT_PAYLOAD_SIZE,
                    "started_at_unix_ms": started, "finished_at_unix_ms": started + metrics["elapsed_ms"],
                    "visible_new_ref": True, "native_writes": 1,
                    "edge_responses": {}, "native_traffic": {"get_responses": 1, "head_responses": 0, "body_bytes": 8},
                }
                proof = {}
                origin_requests = {}
                if provider in CDN.VIEWER_REQUEST_FILES and edge:
                    request_id = hashlib.sha256(f"{location}/{index}/{arm}/viewer".encode()).hexdigest()
                    cache = "miss" if arm == "cold_edge" else "hit"
                    client["edge_responses"][request_id] = {"status": 206, "cache": cache, "bytes": metrics["edge_bytes"],
                                                           "complete": True}
                    proof[request_id] = {"status": 206, "response_bytes": 800, "cache": cache}
                    if provider in ("s3", "azure"):
                        key_sha256 = hashlib.sha256(f"{location}/{index}/key".encode()).hexdigest()
                        proof[request_id]["key_sha256"] = key_sha256
                        if arm == "cold_edge":
                            origin_id = hashlib.sha256(f"{location}/{index}/origin".encode()).hexdigest()
                            time = {"at_unix_s": started // 1000} if provider == "s3" else {"at_unix_ms": started}
                            origin_requests[origin_id] = {"key_sha256": key_sha256, "status": 206,
                                                          "response_body_bytes": metrics["origin_fill_bytes"]} | time
                    if provider == "gcs":
                        proof[request_id]["cache_fill_bytes"] = metrics["origin_fill_bytes"]
                        proof[request_id]["origin_requests"] = {}
                if sdk:
                    client["reader"].pop("cache_root_sha256")
                    client["reader"].update(sdk_native or {})
                    client = {key: value for key, value in client.items() if key in CDN.SDK_GIT_READ_FIELDS} | {
                        "schema": "crab.cdn-sdk-git-read/v1", "commit": oid, "sha256": blob,
                        "git_blob_oid_sha256": hashlib.sha256(f"{blob}/git".encode()).hexdigest(),
                        "bytes": CDN.GIT_PAYLOAD_SIZE, "approval_sha256": "" if arm == "origin" else registration.get(
                            "viewer_approval_sha256", registration.get("publication_sha256", ""),
                        ),
                    }
                    if arm == "warm_local":
                        client["primer_sha256"] = hashlib.sha256((directory.parent / "warm_edge/client.json").read_bytes()).hexdigest()
                digest = json_file(directory / "client.json", client)
                if proof:
                    edge_name = CDN.PROVIDERS[provider][0]
                    evidence = {
                        "schema": f"crab.{edge_name}-viewer-requests/v1", "client_report_sha256": digest, "records": proof,
                    }
                    if provider in ("s3", "azure"):
                        evidence["origin_requests"] = origin_requests
                    json_file(directory / CDN.VIEWER_REQUEST_FILES[provider], evidence)
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

    def configure(self, provider: str, access: str, profile: str, *, consumer: str = "cli", family: str = "git",
                  platform: str = "linux-x86_64") -> None:
        self.provider = provider
        self.scope = f"{CDN.PROVIDERS[provider][0]}/{access}/{consumer}/{family}/{profile}"
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
            publication = self.write_public_export(provider)
        policy_digest = CDN.deployment_policy_digest(edge, access, self.policy_components)
        self.registration = {
            "schema": "crab.cdn-benchmark/v1", "source_sha": COMMIT, "scope": self.scope,
            "configuration_sha256": CONFIGURATION, "workload": CDN.cli_action_name(self.scope), "samples": 30,
            "p95_budget_ms": 150, "cost_budget_microusd": 10, "regions": REGIONS, "deployment_policy_sha256": policy_digest,
        } | publication
        if CDN.requires_cli_workload(self.scope):
            self.registration["candidate"] = cli_candidate(self.scope, platform)
            self.registration["viewer_binding_sha256"] = "e" * 64
        sdk = CDN.is_direct_sdk_git_scope(self.scope)
        hydration = CDN.is_direct_sdk_hydration_scope(self.scope)
        if sdk or hydration:
            self.registration.update(workload=CDN.SDK_HYDRATION_ACTION if hydration else CDN.SDK_GIT_ACTION,
                                     sdk_candidate=sdk_candidate(self.scope, platform),
                                     viewer_binding_sha256="e" * 64)
            if access == "private":
                self.registration["viewer_approval_sha256"] = "d" * 64
        if profile == "managed":
            self.registration["managed_issuer"] = {
                "identity_sha256": "1" * 64, "version": "2026.11.22", "approval_sha256": "2" * 64,
            }
        self.report = {
            "schema": CDN.SCHEMA, "schema_version": 1, "status": "qualified", "scope": self.scope,
            "source_sha": COMMIT, "workflow_run_id": "123", "workflow_run_attempt": "2",
            "origin_report_sha256": origin_digest, "configuration_sha256": CONFIGURATION,
            "deployment": "fixture-a", "platform": platform,
            "fixture_prefix": FIXTURE_PREFIX, "checks": {},
            "product_action": {
                "name": self.registration["workload"], "expected_sha256": "b" * 64, "observed_sha256": "b" * 64,
                "expected_bytes": 8, "observed_bytes": 8, "native_writes": 1, "observed_edge_gets": 1,
                "visible_new_ref": True, "fixture_cleanup_verified": True, "verified_edge_payloads": 1,
            },
            "performance": {
                "near": {"region": "region-1", "pairs": [sample(provider) for _ in range(30)]},
                "distant": {"region": "region-2", "pairs": [sample(provider) for _ in range(30)]},
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
            "warm_anonymous_viewer_denied": [edge_hit, edge_deny, edge_deny, edge_hit],
            "warm_expired_viewer_denied": [edge_hit, edge_deny],
            "warm_unrelated_principal_denied": [edge_hit, control, edge_deny, edge_deny, control, edge_hit],
            CDN.EXPIRY_RANGE_CHECK: expiry_rows(edge_hit | {"status": 206, "bytes": 1}, edge_deny),
            "private_origin_denied": [origin_deny],
            "live_origin_denied": [origin_deny],
            "export_origin_access": [origin_deny if provider == "s3" else origin_read],
            "public_listing_denied": [origin_deny],
            "rollback": [edge_hit, origin_read],
        }
        for check in CDN.required_check_names(access, profile):
            if check == "cleanup":
                self.cleanup_trace(access, CDN.PROVIDERS[provider][0])
            elif check == "deployment_policy":
                self.deployment_trace(self.policy_components, policy_digest)
            elif check == CDN.KEY_ROTATION_CHECK:
                self.rotation_trace(edge)
            elif check == CDN.CACHE_REUSE_CHECK:
                self.cache_reuse_trace()
            elif check == CDN.SCOPED_VIEW_CHECK:
                self.scoped_view_trace()
            elif check == CDN.MANAGED_ISSUER_CHECK:
                self.managed_issuer_trace()
            elif check == CDN.HISTORICAL_ORIGIN_CHECK:
                self.historical_origin_trace(edge)
            elif check == CDN.PUBLIC_TRANSITION_CHECK:
                continue
            elif check == "rollback" and (CDN.requires_cli_workload(self.scope) or sdk or hydration):
                continue
            else:
                self.trace(check, observations.get(check, [edge_hit]))
        if CDN.requires_cli_workload(self.scope):
            write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform)
            self.record_git_product()
        elif sdk:
            self.record_sdk_product()
            cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
            native = {"origin_sha256": cleanup["targets"]["native"]["origin_sha256"], "prefix_sha256": cleanup["prefix_sha256"]}
            write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform, sdk_native=native)
        elif hydration:
            self.record_sdk_hydration()
        if access == "public":
            self.public_transition_trace()

    def write_public_export(self, provider: str) -> dict[str, str]:
        source, export, deployment, source_resource, export_resource, location = {
            "s3": ("s3://source-bucket", "s3://export-bucket", "arn:aws:cloudfront::123456789012:distribution/EDIST",
                   "accounts/111111111111/buckets/source-bucket", "accounts/123456789012/buckets/export-bucket", "us-east-1"),
            "gcs": ("gs://source-bucket", "gs://export-bucket", "projects/crab-test/global/backendBuckets/export",
                    "projects/111111/buckets/source-bucket", "projects/222222/buckets/export-bucket", "US"),
            "azure": ("https://sourceacct.blob.core.windows.net/repo", "https://exportacct.blob.core.windows.net/repo",
                      "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Cdn/profiles/fixture/afdEndpoints/export/routes/objects",
                      azure_resource("sourceacct"), azure_resource("exportacct"), "westus2"),
        }[provider]
        source_placement = {"resource": source_resource, "location": location}
        export_placement = {"resource": export_resource, "location": location}
        root = self.directory / CDN.PUBLIC_EXPORT_DIR
        previous = root / CDN.PUBLIC_EXPORT_BEFORE
        previous.mkdir(parents=True, exist_ok=True)
        self.public_origins = {"native": source, "export": export}
        entry = Object("v2/pack-layers/aa/" + "a" * 64, len(EXPORT_BODY), hashlib.sha256(EXPORT_BODY).hexdigest())
        old_entry = Object("v2/pack-layers/bb/" + "b" * 64, len(EXPORT_BODY), "c" * 64)
        self.object_key = hashlib.sha256(f"{FIXTURE_PREFIX}/{entry.path}".encode()).hexdigest()
        inventory = json_file(root / "inventory.json", {
            "schema": INVENTORY_SCHEMA, "source_origin": source, "repository_prefix": FIXTURE_PREFIX,
            "source_placement": source_placement,
            "objects": [vars(entry), vars(old_entry)],
        })
        receipt = publication_receipt(source, export, source_placement, export_placement, FIXTURE_PREFIX,
                                      "https://export.example.com", deployment, inventory, [entry, old_entry])
        self.policy_components["public-export"] = CDN.deployment_component_digest({
            name: receipt[name] for name in PUBLICATION_BINDING_FIELDS
        })
        old_inventory = json_file(previous / "inventory.json", {
            "schema": INVENTORY_SCHEMA, "source_origin": source, "source_placement": source_placement,
            "repository_prefix": FIXTURE_PREFIX, "objects": [vars(old_entry)],
        })
        old_receipt = publication_receipt(source, export, source_placement, export_placement, FIXTURE_PREFIX,
                                          "https://export.example.com", deployment, old_inventory, [old_entry])
        return {
            "publication_sha256": json_file(root / "publication.json", receipt | {"sealed_at_unix_ms": PUBLIC_SEAL}),
            "previous_publication_sha256": json_file(previous / "publication.json", old_receipt | {"sealed_at_unix_ms": 100}),
        }

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
        cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
        fetch["reader"].update({"origin_sha256": cleanup["targets"]["native"]["origin_sha256"],
                                "prefix_sha256": cleanup["prefix_sha256"]})
        ref = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef"
        receipt = json_file(records / "publication.json", provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": ref,
            "oid": fetch["publication_oid"], "sha256": fetch["expected_sha256"], "published": True,
            "visible_at_unix_ms": fetch["started_at_unix_ms"] - 50, "candidate": fetch["candidate"],
        })
        json_file(records / "fetch.json", fetch | {"publication_sha256": receipt})
        self.rollback_trace(fetch | dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "publication_sha256": receipt, "arm": "origin", "edge_responses": {},
            "started_at_unix_ms": fetch["finished_at_unix_ms"] + 5,
            "finished_at_unix_ms": fetch["finished_at_unix_ms"] + 10,
            "reader": fetch["reader"] | {"binding_sha256": "", "cache_root_sha256": "1" * 64, "public_export_sha256": ""},
        })
        json_file(records / "retirement.json", provenance | {
            "schema": "crab.cdn-git-retirement/v1", "publication_sha256": receipt, "ref_retired": True,
            "retired_at_unix_ms": fetch["finished_at_unix_ms"] + 50, "candidate": fetch["candidate"],
        })
        for phase, timestamp, marker in (
            ("published", fetch["started_at_unix_ms"] - 40, "5"),
            ("retired", fetch["finished_at_unix_ms"] + 60, "6"),
        ):
            json_file(records / CDN.GIT_NATIVE_REF_FILES[phase], provenance | {
                "schema": "crab.cdn-native-ref/v1", "phase": phase, "platform": self.report["platform"],
                "publication_sha256": receipt, "started_at_unix_ms": timestamp, "observed_at_unix_ms": timestamp + 5,
                "native_ref": {
                    "probe_source_sha": COMMIT, "origin_sha256": cleanup["targets"]["native"]["origin_sha256"],
                    "prefix_sha256": cleanup["prefix_sha256"], "ref_sha256": hashlib.sha256(ref.encode()).hexdigest(),
                    "repository_sha256": "3" * 64, "epoch_sha256": "4" * 64, "root_digest": "b" * 64,
                    "head_sha256": marker * 64, "revision_sha256": marker * 64, "transaction_id": marker * 64,
                    "oid": fetch["publication_oid"] if phase == "published" else None,
                },
            })
        self.report["product_action"].update({key: fetch[key] for key in self.report["product_action"] if key in fetch})

    def record_sdk_product(self) -> None:
        records = self.directory / CDN.GIT_PRODUCT_DIR
        records.mkdir(exist_ok=True)
        provenance = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS}
        platform = self.report["platform"]
        ref = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef"
        cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
        reader = {"origin_sha256": cleanup["targets"]["native"]["origin_sha256"], "prefix_sha256": cleanup["prefix_sha256"]}
        writer = cli_candidate(self.scope, platform)
        oid, payload = "a" * 40, "b" * 64
        receipt = json_file(records / "publication.json", provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": oid, "sha256": payload, "published": True,
            "visible_at_unix_ms": PUBLIC_SEAL - 40, "candidate": writer,
        })
        base = provenance | dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "schema": "crab.cdn-sdk-git-read/v1", "platform": platform, "candidate": self.registration["sdk_candidate"],
            "commit": oid, "sha256": payload, "bytes": CDN.GIT_PAYLOAD_SIZE, "git_blob_oid_sha256": "4" * 64,
            "publication_sha256": receipt, "elapsed_ms": 10, "edge_responses": {},
            "native_traffic": {"get_responses": 1, "head_responses": 0, "body_bytes": 5},
        }
        export = self.registration.get("publication_sha256", "")
        transition = {"transition_key_sha256": self.object_key} if export else {}
        binding = reader | {"binding_sha256": self.registration["viewer_binding_sha256"]}
        json_file(records / "snapshot.json", base | dict.fromkeys(
            ("edge_attempts", "observed_edge_gets", "edge_hits", "edge_bytes", "verified_edge_payloads", "all_edge_attempts"), 1,
        ) | {
            "arm": "edge", "started_at_unix_ms": PUBLIC_SEAL + 10, "finished_at_unix_ms": PUBLIC_SEAL + 20,
            "approval_sha256": self.registration.get("viewer_approval_sha256", export),
            "reader": binding | {"public_export_sha256": export},
        } | transition)
        if export:
            prior = self.registration["previous_publication_sha256"]
            json_file(records / CDN.SDK_PRIOR_EXPORT_RECORD, base | transition | {
                "arm": "prior_export", "started_at_unix_ms": PUBLIC_SEAL - 20, "finished_at_unix_ms": PUBLIC_SEAL - 10,
                "approval_sha256": prior, "reader": binding | {"public_export_sha256": prior},
            })
        self.rollback_trace(base | {
            "arm": "origin", "started_at_unix_ms": PUBLIC_SEAL + 25, "finished_at_unix_ms": PUBLIC_SEAL + 35,
            "reader": reader | {"binding_sha256": "", "public_export_sha256": ""}, "approval_sha256": "",
        })
        pack = "8" * 64
        for arm, filename, start in (("edge", "snapshot.json", 36), ("origin", "traces/rollback.json", 42)):
            path = records / filename if arm == "edge" else self.directory / filename
            read = json.loads(path.read_bytes())
            read.pop("transition_key_sha256", None)
            json_file(records / CDN.SDK_CLONE_FILES[arm], read | {
                "schema": "crab.cdn-sdk-git-clone/v1", "crab_sha256": writer["binary_sha256"],
                "started_at_unix_ms": PUBLIC_SEAL + start, "finished_at_unix_ms": PUBLIC_SEAL + start + 5,
                "elapsed_ms": 5, "installed_packs": [pack], "verified_edge_packs": [pack] if arm == "edge" else [],
            })
            json_file(records / CDN.SDK_FETCH_FILES[arm], read | {
                "schema": "crab.cdn-sdk-git-fetch/v1", "crab_sha256": writer["binary_sha256"],
                "started_at_unix_ms": PUBLIC_SEAL + start + 12, "finished_at_unix_ms": PUBLIC_SEAL + start + 17,
                "elapsed_ms": 5, "installed_packs": [pack], "verified_edge_packs": [pack] if arm == "edge" else [],
                "previous_head": "5" * 40, "head_after_fetch": "5" * 40,
            })
        json_file(records / "retirement.json", provenance | {
            "schema": "crab.cdn-git-retirement/v1", "publication_sha256": receipt, "ref_retired": True,
            "retired_at_unix_ms": PUBLIC_SEAL + 59, "candidate": writer,
        })
        for phase, delta, marker in (("published", -30, "5"), ("retired", 60, "6")):
            json_file(records / CDN.GIT_NATIVE_REF_FILES[phase], provenance | {
                "schema": "crab.cdn-native-ref/v1", "phase": phase, "platform": platform, "publication_sha256": receipt,
                "started_at_unix_ms": PUBLIC_SEAL + delta, "observed_at_unix_ms": PUBLIC_SEAL + delta + 5,
                "native_ref": reader | {
                    "probe_source_sha": COMMIT, "ref_sha256": hashlib.sha256(ref.encode()).hexdigest(),
                    "repository_sha256": "3" * 64, "epoch_sha256": "4" * 64, "root_digest": "b" * 64,
                    "head_sha256": marker * 64, "revision_sha256": marker * 64, "transaction_id": marker * 64,
                    "oid": oid if phase == "published" else None,
                },
            })
        self.report["product_action"].update(expected_bytes=CDN.GIT_PAYLOAD_SIZE, observed_bytes=CDN.GIT_PAYLOAD_SIZE)

    def record_sdk_hydration(self) -> None:
        self.record_sdk_product()
        records, traces = self.directory / CDN.SDK_HYDRATION_DIR, self.directory / "traces"
        source = self.directory / CDN.GIT_PRODUCT_DIR
        records.mkdir()
        for name in ("publication.json", "retirement.json", *CDN.GIT_NATIVE_REF_FILES.values()):
            (source / name).rename(records / name)
        publication_path, retirement_path = (records / name for name in ("publication.json", "retirement.json"))
        seed = {"scope": self.scope.replace("/xorb/", "/git/"), "configuration_sha256": "9" * 64}
        publication = json.loads(publication_path.read_bytes()) | seed
        receipt = json_file(publication_path, publication)
        json_file(retirement_path, json.loads(retirement_path.read_bytes()) | seed | {"publication_sha256": receipt})
        for filename in CDN.GIT_NATIVE_REF_FILES.values():
            path = records / filename
            json_file(path, json.loads(path.read_bytes()) | {"publication_sha256": receipt})
        self.registration["hydration_workload_sha256"] = json_file(records / "workload.json", {
            "schema": "crab.cdn-sdk-hydration-workload/v1", "configuration_sha256": CONFIGURATION,
            "pointer_sha256": publication["sha256"], "payload_sha256": "c" * 64,
            "payload_bytes": 1024, "crab_sha256": publication["candidate"]["binary_sha256"],
            "global_prefix_sha256": "5" * 64,
            "publication_sha256": receipt,
        })
        control = json.loads((traces / f"{CDN.SCOPED_VIEW_CHECK}.json").read_bytes())
        cleanup = json.loads((traces / "cleanup.json").read_bytes())
        origin = cleanup["targets"]["native"]["origin_sha256"]
        placement = control["placement"]
        target = placement["views"]["target"]
        placement.update(origin_sha256=origin, source_prefix_sha256=cleanup["prefix_sha256"],
                         deployment_policy_sha256=self.registration["deployment_policy_sha256"])
        target.update(key_sha256=self.object_key)
        for request in control["requests"]:
            if request["probe"].startswith("target_"):
                request["key_sha256"] = self.object_key
        self.registration["scoped_view_sha256"] = CDN.deployment_component_digest(placement)
        self.scoped_view_trace(control)
        for arm, raw_path, destination, marker in (
            ("edge", source / "snapshot.json", records / "hydration.json", "1"),
            ("origin", traces / "rollback.json", traces / "rollback.json", "2"),
        ):
            raw = json.loads(raw_path.read_bytes())
            reader = {
                "origin_sha256": origin, "prefix_sha256": target["physical_prefix_sha256"],
                "global_prefix_sha256": "5" * 64, "cache_root_sha256": marker * 64, "local_cache_only": True,
                "binding_sha256": raw["reader"]["binding_sha256"], "approval_sha256": raw["approval_sha256"],
            }
            evidence = {
                name: raw[name] for name in CDN.CDN_PROVENANCE_FIELDS | CDN.PRODUCT_COUNTERS | CDN.SDK_TIMING_FIELDS
                | {"platform", "arm", "candidate", "native_traffic", "edge_responses"}
            } | {
                "schema": "crab.cdn-sdk-hydration/v1", "workload_sha256": self.registration["hydration_workload_sha256"],
                "crab_sha256": publication["candidate"]["binary_sha256"], "pointer_sha256": publication["sha256"],
                "payload": {"sha256": "c" * 64, "bytes": 1024}, "xorb_key_sha256": self.object_key, "reader": reader,
            }
            if arm == "edge":
                json_file(destination, evidence)
            else:
                self.rollback_trace(evidence)
        self.report["product_action"].update(expected_sha256="c" * 64, observed_sha256="c" * 64,
                                             expected_bytes=1024, observed_bytes=1024)

    def rollback_trace(self, evidence: dict[str, Any]) -> None:
        path = self.directory / "traces/rollback.json"
        self.report["checks"]["rollback"] = {"passed": True, "trace_sha256": json_file(path, evidence)}

    def cache_reuse_trace(self, evidence: dict[str, Any] | None = None) -> None:
        check = CDN.CACHE_REUSE_CHECK
        if evidence is None:
            principals = {
                role: {"subject_sha256": marker * 64, "policy_identity_sha256": marker * 64}
                for role, marker in (("primary", "a"), ("second", "b"))
            }
            authority = {"issuer_endpoint_sha256": "c" * 64, "approval_sha256": "d" * 64, "principals": principals,
                         "object": {"key_sha256": self.object_key, "body_sha256": "e" * 64, "bytes": 8}}
            self.registration["cache_reuse_sha256"] = CDN.deployment_component_digest(authority)
            evidence = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS} | {
                "schema": "crab.cdn-authenticated-cache-reuse/v1", "check": check, "authority": authority,
                "grants": {role: principals[role] | {"issued_at_unix_ms": 1000, "expires_at_unix": 5} for role in principals},
                "requests": [self.request("edge", 200, cache) | {
                    "principal": role, "body_sha256": "e" * 64,
                    "started_at_unix_ms": 1010 + index * 100, "finished_at_unix_ms": 1020 + index * 100,
                } for index, (role, cache) in enumerate((("primary", "miss"), ("second", "hit")))],
            }
        path = self.directory / "traces" / f"{check}.json"
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, evidence)}

    def scoped_view_trace(self, evidence: dict[str, Any] | None = None) -> None:
        check = CDN.SCOPED_VIEW_CHECK
        if evidence is None:
            views = {
                role: dict.fromkeys(CDN.SCOPED_VIEW_FIELDS - {"bytes"}, marker * 64) | {"bytes": 8}
                for role, marker in (("target", "a"), ("sibling", "b"))
            }
            placement = {"fixture_sha256": CONFIGURATION, "origin_sha256": "c" * 64,
                         "source_prefix_sha256": "d" * 64, "deployment_policy_sha256": "e" * 64, "views": views}
            self.registration["scoped_view_sha256"] = CDN.deployment_component_digest(placement)
            requests = []
            for probe in CDN.SCOPED_VIEW_PROBES:
                target = probe.startswith("target_")
                denied = probe in {"sibling_full", "sibling_range"}
                view = views["target" if target else "sibling"]
                requests.append(self.request("edge" if target or denied else "origin", 403 if denied else 200,
                                             "hit" if target else "unknown") | {
                    "probe": probe, "body_sha256": None if denied else view["body_sha256"],
                    "key_sha256": view["key_sha256"], "range_offset": 0 if probe == "sibling_range" else None,
                })
            evidence = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS} | {
                "schema": "crab.cdn-scoped-view/v1", "check": check, "placement": placement, "requests": requests,
            }
        path = self.directory / "traces" / f"{check}.json"
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, evidence)}

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

    def historical_origin_trace(self, edge: str) -> None:
        origin_digest = hashlib.sha256(self.public_origins["native"].encode()).hexdigest()
        version = {"kind": sorted(CDN.HISTORICAL_KINDS[edge])[0], "selector_sha256": "1" * 64,
                   "sha256": hashlib.sha256(b"retired").hexdigest(), "size": 7}
        self.registration["historical_origin_sha256"] = CDN.historical_approval_digest(
            origin_digest, self.object_key, [version],
        )
        trace = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS} | {
            "schema": "crab.cdn-historical-origin/v1", "origin_sha256": origin_digest, "key_sha256": self.object_key,
            "versions": [version | {"authorized_status": 206 if edge == "cloudfront" else 200, "anonymous_status": 403}],
        }
        check = CDN.HISTORICAL_ORIGIN_CHECK
        path = self.directory / "traces" / f"{check}.json"
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, trace)}

    def public_transition_trace(self) -> None:
        reference, visible, start, finish, attempts = "4" * 64, PUBLIC_SEAL - 40, PUBLIC_SEAL + 10, PUBLIC_SEAL + 30, 1
        if CDN.requires_cli_workload(self.scope):
            records = self.directory / CDN.GIT_PRODUCT_DIR
            publication = json.loads((records / "publication.json").read_bytes())
            fetch = json.loads((records / "fetch.json").read_bytes())
            reference = CDN.deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
            visible = publication["visible_at_unix_ms"]
            start, finish, attempts = (fetch[key] for key in ("started_at_unix_ms", "finished_at_unix_ms", "all_edge_attempts"))
        action = self.report["product_action"]
        common = {"action": action["name"], "reference_sha256": reference, "key_sha256": self.object_key,
                  "result_sha256": action["observed_sha256"], "result_bytes": action["observed_bytes"], "binding_active": True}
        trace = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS | {"platform"}} | {
            "schema": "crab.cdn-public-transition/v1",
            "key_sha256": self.object_key,
            "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": visible},
            "before": common | {"receipt_sha256": self.registration["previous_publication_sha256"],
                                "started_at_unix_ms": visible + 5, "finished_at_unix_ms": visible + 10,
                                "edge_attempts": 0, "verified_edge_key_sha256": None},
            "after": common | {"receipt_sha256": self.registration["publication_sha256"],
                               "started_at_unix_ms": start, "finished_at_unix_ms": finish,
                               "edge_attempts": attempts, "verified_edge_key_sha256": self.object_key},
        }
        if CDN.requires_cli_workload(self.scope):
            trace["after"]["reader"] = fetch["reader"]
            trace["before"]["reader"] = fetch["reader"] | {"cache_root_sha256": "9" * 64,
                                                         "public_export_sha256": self.registration["previous_publication_sha256"]}
        elif CDN.is_direct_sdk_git_scope(self.scope):
            trace = CDN.derive_sdk_public_transition(self.directory / CDN.GIT_PRODUCT_DIR, self.report, self.registration)
        check = CDN.PUBLIC_TRANSITION_CHECK
        path = self.directory / "traces" / f"{check}.json"
        self.report["checks"][check] = {"passed": True, "trace_sha256": json_file(path, trace)}

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

    def managed_issuer_trace(self) -> None:
        issuer = self.registration["managed_issuer"]
        approval = issuer["approval_sha256"]
        action = self.report["product_action"]
        issued = {"request_versions": [2], "request_approval_sha256": approval, "status": 201,
                  "grant_version": 2, "grant_approval_sha256": approval}
        verified = {"result": "verified", "verified_output_sha256": action["observed_sha256"],
                    "verified_output_bytes": action["observed_bytes"], "edge_payload_requests": 1, "native_payload_reads": 0}
        failed = {"verified_output_sha256": None, "verified_output_bytes": 0,
                  "edge_payload_requests": 0, "native_payload_reads": 0}
        trace = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS | {"platform"}} | {
            "schema": "crab.cdn-managed-issuer/v1",
            "issuer": {key: issuer[key] for key in ("identity_sha256", "version")},
            "binding_sha256": approval,
            "placement_sha256": self.policy_components["private-binding"],
            "workload": action["name"],
            "operations": {
                "approved_v2": issued | verified,
                "v1_peer": issued | verified | {
                    "request_versions": [1], "request_approval_sha256": None, "grant_version": 1,
                    "grant_approval_sha256": None, "edge_payload_requests": 0, "native_payload_reads": 1,
                },
                "denied_v2": issued | failed | {
                    "status": 403, "grant_version": None, "grant_approval_sha256": None, "result": "issuer_denied",
                },
                "mismatched_v2": issued | failed | {"grant_approval_sha256": "3" * 64, "result": "client_rejected"},
            },
        }
        check = CDN.MANAGED_ISSUER_CHECK
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
        policy.write_bytes((self.scope + "\r\n").encode())
        result = self.command(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        original = self.registration["candidate"]
        for change in ({"qualification_feature": True}, {"cargo_profile": "debug"}):
            with self.subTest(change=change):
                self.registration["candidate"] = original | change
                self.rejected("requires the ordinary release build", *args)
        self.registration["candidate"] = original | {"target_triple": "x86_64-unknown-linux-musl"}
        self.rejected("requires the packaged CLI target triple", *args)
        self.registration["candidate"] = original
        policy.write_text("# Changed after the build\n" + self.scope + "\n")
        self.rejected("did not embed the release checkout", *args)
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
        del self.report["checks"][CDN.MANAGED_ISSUER_CHECK]
        self.rejected("invalid fields in checks")

    def test_managed_issuer_rejects_generic_trace(self) -> None:
        self.configure("s3", "private", "managed")
        self.trace(CDN.MANAGED_ISSUER_CHECK, [self.request("edge", 200, "hit")])
        self.rejected("invalid fields in managed issuer trace")

    def test_managed_observations_are_bound_to_approval_and_terminal_errors(self) -> None:
        self.configure("gcs", "private", "managed")
        check = CDN.MANAGED_ISSUER_CHECK
        path = self.directory / "traces" / f"{check}.json"
        original = path.read_bytes()
        cases = (
            (lambda t: t["issuer"].update(identity_sha256="4" * 64), "differs from the approved production issuer"),
            (lambda t: t["issuer"].update(version="reference-v2"), "differs from the approved production issuer"),
            (lambda t: t.update(placement_sha256="4" * 64), "approved binding and attested placement"),
            (lambda t: t.update(binding_sha256="4" * 64), "approved binding and attested placement"),
            (lambda t: t.update(workflow_run_attempt="1"), "match the release row and workflow attempt"),
            (lambda t: t["operations"]["approved_v2"].update(request_approval_sha256="4" * 64),
             "did not use the authenticated approval"),
            (lambda t: t["operations"]["approved_v2"].update(grant_approval_sha256="4" * 64),
             "did not verify the granted authority"),
            (lambda t: t["operations"]["approved_v2"].update(verified_output_sha256="4" * 64),
             "did not verify the granted authority"),
            (lambda t: t["operations"]["v1_peer"].update(edge_payload_requests=1), "wrong payload path"),
            (lambda t: t["operations"]["denied_v2"].update(request_versions=[2, 1]), "retried or downgraded"),
            (lambda t: t["operations"]["denied_v2"].update(status=401), "issuer denial was not terminal"),
            (lambda t: t["operations"]["mismatched_v2"].update(native_payload_reads=1), "attempted payload reads"),
            (lambda t: t["operations"]["mismatched_v2"].update(grant_approval_sha256="2" * 64),
             "did not reject a mismatched managed v2 approval"),
        )
        for mutate, reason in cases:
            with self.subTest(reason=reason):
                trace = json.loads(original)
                mutate(trace)
                self.report["checks"][check]["trace_sha256"] = json_file(path, trace)
                self.rejected(reason)

    def test_managed_benchmark_requires_production_preregistration(self) -> None:
        self.configure("azure", "private", "managed")
        del self.registration["managed_issuer"]
        self.rejected("invalid fields in registered production issuer")

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
        document["objects"].append({"path": "v2/pack-layers/cc/" + "c" * 64, "sha256": "d" * 64, "size": 60})
        json_file(inventory, document)
        self.rejected("publication receipt does not match the exact approved inventory")

    def test_public_release_rejects_export_approval_for_another_deployment(self) -> None:
        self.configure("azure", "public", "direct", consumer="sdk")
        path = self.directory / CDN.PUBLIC_EXPORT_DIR / "publication.json"
        receipt = json.loads(path.read_bytes())
        receipt["deployment"] += "-other"
        self.registration["publication_sha256"] = json_file(path, receipt)
        self.rejected("publication receipt must match the attested public export")

    def test_public_release_rejects_a_reapproved_azure_export_on_the_source_account(self) -> None:
        self.configure("azure", "public", "direct", consumer="sdk")
        path = self.directory / CDN.PUBLIC_EXPORT_DIR / "publication.json"
        receipt = json.loads(path.read_bytes())
        receipt["export_origin"] = receipt["source_origin"].rsplit("/", 1)[0] + "/export"
        placement = receipt["source_placement"]
        receipt["export_placement"] = placement | {"resource": placement["resource"].rsplit("/", 1)[0] + "/export"}
        self.registration["publication_sha256"] = json_file(path, receipt)
        self.rejected("public CDN requires a separate export bucket or Azure storage account")

    def test_public_release_needs_denied_access_to_the_same_verified_native_version(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        check = CDN.HISTORICAL_ORIGIN_CHECK
        path = self.directory / "traces" / f"{check}.json"
        original = json.loads(path.read_bytes())
        version, = original["versions"]
        cases = (
            ({"key_sha256": "8" * 64}, "must match the approved live source"),
            ({"versions": [version | {"kind": "versionId"}]}, "provider's distinct retired bytes"),
            ({"versions": [version | {"sha256": hashlib.sha256(EXPORT_BODY).hexdigest()}]}, "provider's distinct retired bytes"),
            ({"versions": [version | {"authorized_status": 404}]}, "readable with authorization and denied anonymously"),
            ({"versions": [version | {"anonymous_status": 200}]}, "readable with authorization and denied anonymously"),
            ({"versions": [version | {"selector_sha256": "8" * 64}]}, "differs from preregistration"),
        )
        for changes, reason in cases:
            with self.subTest(changes=changes):
                self.report["checks"][check]["trace_sha256"] = json_file(path, original | changes)
                self.rejected(reason)

    def test_public_transition_requires_the_same_verified_action_around_export_sealing(self) -> None:
        self.configure("s3", "public", "direct")
        check = CDN.PUBLIC_TRANSITION_CHECK
        path = self.directory / "traces" / f"{check}.json"
        raw = path.read_bytes()
        cases = (
            ("native_publication", {"reference_sha256": "9" * 64}, "confirmed native publication"),
            ("before", {"binding_active": False}, "each active approval"),
            ("before", {"edge_attempts": 1}, "must avoid the edge before approval"),
            ("before", {"finished_at_unix_ms": PUBLIC_SEAL}, "must be sealed after the native-only read"),
            ("after", {"verified_edge_key_sha256": "8" * 64}, "verify the exact edge object"),
            ("after", {"edge_attempts": 2}, "must use the retained Git product fetch"),
        )
        for phase, changes, reason in cases:
            with self.subTest(phase=phase, changes=changes):
                trace = json.loads(raw)
                trace[phase].update(changes)
                self.report["checks"][check]["trace_sha256"] = json_file(path, trace)
                self.rejected(reason)

    def test_public_transition_requires_the_object_to_be_unlisted_beforehand(self) -> None:
        self.configure("azure", "public", "direct", consumer="sdk")
        root = self.directory / CDN.PUBLIC_EXPORT_DIR
        prior = root / CDN.PUBLIC_EXPORT_BEFORE
        (prior / "inventory.json").write_bytes((root / "inventory.json").read_bytes())
        current_receipt = json.loads((root / "publication.json").read_bytes())
        self.registration["previous_publication_sha256"] = json_file(
            prior / "publication.json", current_receipt | {"sealed_at_unix_ms": 100},
        )
        self.rejected("already approved before the native-only read")

    def test_public_release_pins_observed_export_owner_and_location_even_with_reapproved_receipt(self) -> None:
        self.configure("s3", "public", "direct", consumer="sdk")
        path = self.directory / CDN.PUBLIC_EXPORT_DIR / "publication.json"
        original = json.loads(path.read_bytes())
        for changes in ({"resource": "accounts/999999999999/buckets/export-bucket"}, {"location": "us-west-2"}):
            with self.subTest(changes=changes):
                receipt = original | {"export_placement": original["export_placement"] | changes}
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
        self.rejected("accepted no verified edge payload")

    def test_warm_local_performance_requires_a_payload_cache_hit(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["performance"]["near"]["pairs"][0]["warm_local"]["local_payload_hits"] = 0
        self.rejected("warm local sample must prove a local payload hit")

    def test_rejects_a_valid_summary_that_does_not_match_retained_git_fetches(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["performance"]["near"]["pairs"][0]["warm_edge"]["elapsed_ms"] += 1
        self.rejected("CLI performance summary differs from the retained measurements")

    def test_rejects_a_positive_product_summary_that_differs_from_the_fetch(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["product_action"]["verified_edge_payloads"] += 1
        self.rejected("product action differs from the retained publication, fetch, and retirement evidence")

    def test_product_record_requires_retirement_of_the_same_receipt_after_rollback(self) -> None:
        self.configure("gcs", "private", "direct")
        path = self.directory / CDN.GIT_PRODUCT_DIR / "retirement.json"
        retirement = json.loads(path.read_bytes())
        edge = json.loads((path.parent / "fetch.json").read_bytes())
        for changes in ({"publication_sha256": "f" * 64}, {"retired_at_unix_ms": edge["finished_at_unix_ms"]}):
            with self.subTest(changes=changes):
                json_file(path, retirement | changes)
                self.rejected("requires retirement of the same published ref after rollback")

    def test_cleanup_inventory_must_be_captured_after_git_retirement(self) -> None:
        self.configure("gcs", "private", "direct")
        self.cleanup_trace("private", "cloud-cdn", observed_at_unix_ms=1)
        self.rejected("fixture cleanup must be verified after Git ref retirement")

    def test_provider_retirement_requires_a_new_tombstone_in_the_observed_epoch(self) -> None:
        self.configure("s3", "private", "direct")
        records = self.directory / CDN.GIT_PRODUCT_DIR
        publication = json.loads((records / CDN.GIT_NATIVE_REF_FILES["published"]).read_bytes())
        path = records / CDN.GIT_NATIVE_REF_FILES["retired"]
        retired = json.loads(path.read_bytes())
        for change, reason in (
            ({"oid": publication["native_ref"]["oid"]}, "unexpected committed tip"),
            ({"probe_source_sha": "f" * 40}, "observed for a different source"),
            ({"origin_sha256": "f" * 64}, "observed for a different source"),
            ({"epoch_sha256": "f" * 64}, "distinct tombstone in the same repository epoch"),
            ({"revision_sha256": publication["native_ref"]["revision_sha256"]}, "distinct tombstone in the same repository epoch"),
        ):
            with self.subTest(change=change):
                json_file(path, retired | {"native_ref": retired["native_ref"] | change})
                self.rejected(reason)

    def test_provider_observation_must_precede_cleanup(self) -> None:
        self.configure("azure", "private", "direct")
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.GIT_NATIVE_REF_FILES["retired"]
        retired = json.loads(path.read_bytes())
        self.cleanup_trace("private", "front-door", observed_at_unix_ms=retired["started_at_unix_ms"])
        self.rejected("observations must bracket the fetch and precede fixture cleanup")

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
        self.configure("azure", "private", "direct", consumer="server")
        self.trace("rollback", [self.request("edge", 200), self.request("origin", 200) | {"key_sha256": "c" * 64}])
        self.rejected("rollback requires an edge read followed by a native read of the same complete object")

    def test_sdk_release_rejects_product_summary_without_regional_sdk_benchmark_records(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        self.assertEqual(self.command().returncode, 0)
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(self.scope + "\n", encoding="utf-8")
        (self.directory / CDN.GIT_BENCHMARK_DIR).rename(self.directory / "benchmark-backup")
        self.rejected("requires retained regional SDK benchmark records", "--cdn-release-policy", str(policy))

    def test_sdk_release_requires_clone_of_the_same_publication_before_retirement(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(self.scope + "\n", encoding="utf-8")
        args = ("--cdn-release-policy", str(policy))
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.SDK_CLONE_FILES["edge"]
        original = json.loads(path.read_bytes())
        for changes, reason in (
            ({"schema": "crab.cdn-sdk-git-read/v1"}, "mismatched product or workflow provenance"),
            ({"crab_sha256": "9" * 64}, "the approved Crab build"),
            ({"verified_edge_packs": ["9" * 64]}, "needs verified pack evidence"),
            ({"commit": "f" * 40}, "published blob"),
        ):
            with self.subTest(changes=changes):
                json_file(path, original | changes)
                self.rejected(reason, *args)
        path.unlink()
        self.rejected(CDN.SDK_CLONE_FILES["edge"], *args)

    def test_sdk_release_requires_fetch_after_clone_from_the_same_local_head(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(self.scope + "\n", encoding="utf-8")
        args = ("--cdn-release-policy", str(policy))
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.SDK_FETCH_FILES["origin"]
        original = json.loads(path.read_bytes())
        for changes, reason in (
            ({"head_after_fetch": original["commit"]}, "did not preserve the local HEAD"),
            (dict.fromkeys(("previous_head", "head_after_fetch"), "6" * 40), "same local HEAD"),
            ({"installed_packs": ["9" * 64]}, "must install the same verified CDN packs"),
            ({"started_at_unix_ms": PUBLIC_SEAL + 47}, "must follow the preceding workload"),
            ({"finished_at_unix_ms": PUBLIC_SEAL + 65}, "requires retirement of the same published ref after rollback"),
        ):
            with self.subTest(changes=changes):
                json_file(path, original | changes)
                self.rejected(reason, *args)
        path.unlink()
        self.rejected(CDN.SDK_FETCH_FILES["origin"], *args)

    def test_private_sdk_release_accepts_regional_provider_records(self) -> None:
        for provider in ("s3", "gcs", "azure"):
            with self.subTest(provider=provider):
                self.configure(provider, "private", "direct", consumer="sdk")
                policy = self.directory / "qualified-scopes.txt"
                policy.write_text(self.scope + "\n", encoding="utf-8")
                result = self.command("--cdn-release-policy", str(policy))
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_sdk_hydration_requires_the_exact_lifecycle_and_rejects_generic_promotion(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk", family="xorb")
        self.assertEqual(self.command().returncode, 0)
        policy = self.directory / "policy.txt"
        policy.write_text(self.scope + "\n", encoding="utf-8")
        self.rejected("promotion requires regional hydration evidence", "--cdn-release-policy", str(policy))
        records = self.directory / CDN.SDK_HYDRATION_DIR
        edge = records / "hydration.json"
        native = self.directory / "traces/rollback.json"
        original = json.loads(native.read_bytes())
        edge_read = json.loads(edge.read_bytes())
        for changes, reason in (
            ({"reader": original["reader"] | {"cache_root_sha256": edge_read["reader"]["cache_root_sha256"]}}, "fresh cache"),
            ({"reader": original["reader"] | {"binding_sha256": "5" * 64}}, "required viewer approval"),
            ({"reader": original["reader"] | {"global_prefix_sha256": "6" * 64}}, "different projected view"),
            ({"observed_edge_gets": 1, "edge_hits": 1, "edge_attempts": 1, "all_edge_attempts": 1}, "without edge or cache activity"),
            ({"finished_at_unix_ms": PUBLIC_SEAL + 65}, "same published ref after rollback"),
        ):
            with self.subTest(changes=changes):
                self.rollback_trace(original | changes)
                self.rejected(reason)
        self.rollback_trace(original)
        publication = records / "publication.json"
        approved_publication = publication.read_bytes()
        json_file(publication, json.loads(approved_publication) | {"sha256": "7" * 64})
        self.rejected("native publication of the approved pointer")
        publication.write_bytes(approved_publication)
        (records / CDN.GIT_NATIVE_REF_FILES["published"]).unlink()
        self.rejected(CDN.GIT_NATIVE_REF_FILES["published"])

    def test_public_sdk_release_requires_ordinary_export_transition_calls(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(self.scope + "\n", encoding="utf-8")
        args = ("--cdn-release-policy", str(policy))
        result = self.command(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.SDK_PRIOR_EXPORT_RECORD
        original = json.loads(path.read_bytes())
        for changes, reason in (
            ({"git_blob_oid_sha256": "9" * 64}, "did not verify the same published Git blob"),
            ({"transition_key_sha256": "9" * 64}, "did not verify the same pack-layer key"),
            ({"all_edge_attempts": 1}, "requires native payload reads without edge or cached payload activity"),
            ({"reader": original["reader"] | {"public_export_sha256": "9" * 64}}, "required viewer binding and approval"),
        ):
            with self.subTest(changes=changes):
                json_file(path, original | changes)
                self.rejected(reason, *args)
        path.unlink()
        self.rejected(CDN.SDK_PRIOR_EXPORT_RECORD, *args)

    def test_sdk_rollback_requires_same_approved_build_and_blob_at_origin(self) -> None:
        self.configure("azure", "private", "direct", consumer="sdk")
        origin = json.loads((self.directory / "traces/rollback.json").read_bytes())
        edge = json.loads((self.directory / CDN.GIT_PRODUCT_DIR / "snapshot.json").read_bytes())
        for changes, reason in (
            ({"candidate": origin["candidate"] | {"binary_sha256": "9" * 64}}, "registered SDK build"),
            ({"git_blob_oid_sha256": "9" * 64}, "same published Git blob"),
            ({"reader": origin["reader"] | {"origin_sha256": "9" * 64}}, "changed the selected native target"),
            ({"all_edge_attempts": 1}, "without edge or cached payload activity"),
            ({"started_at_unix_ms": edge["started_at_unix_ms"]}, "must follow the verified edge snapshot"),
        ):
            with self.subTest(changes=changes):
                self.rollback_trace(origin | changes)
                self.rejected(reason)

    def test_sdk_product_summary_must_match_the_retained_read(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        self.report["product_action"]["observed_edge_gets"] += 1
        self.rejected("product action differs from the retained SDK snapshot evidence")

    def test_cli_rollback_requires_an_ordered_native_fetch_of_the_same_publication(self) -> None:
        self.configure("azure", "private", "direct")
        trace = json.loads((self.directory / "traces/rollback.json").read_bytes())
        edge = json.loads((self.directory / CDN.GIT_PRODUCT_DIR / "fetch.json").read_bytes())
        reader = trace["reader"]
        for changes, reason in (
            ({"started_at_unix_ms": edge["started_at_unix_ms"]}, "must follow the verified edge fetch"),
            ({"publication_oid": "f" * 40}, "must verify the same published payload"),
            ({"reader": reader | {"prefix_sha256": "f" * 64}}, "changed the selected native target"),
            ({"reader": reader | {"cache_root_sha256": edge["reader"]["cache_root_sha256"]}}, "reused the edge fetch cache"),
            ({"reader": reader | {"binding_sha256": edge["reader"]["binding_sha256"]}}, "requires an inactive viewer binding"),
            ({"all_edge_attempts": 1}, "zero edge or local payload reads"),
            ({"local_payload_hits": 1}, "zero edge or local payload reads"),
        ):
            with self.subTest(changes=changes):
                self.rollback_trace(trace | changes)
                self.rejected(reason)

    def test_cli_rollback_rejects_an_http_only_trace(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace("rollback", [self.request("edge", 200), self.request("origin", 200)])
        self.rejected("invalid fields in redacted Git fetch")

    def test_cli_rollback_checks_the_native_target_against_the_independent_provider_observation(self) -> None:
        self.configure("gcs", "private", "direct")
        fetch_path = self.directory / CDN.GIT_PRODUCT_DIR / "fetch.json"
        fetch = json.loads(fetch_path.read_bytes())
        rollback = json.loads((self.directory / "traces/rollback.json").read_bytes())
        other_origin = {"origin_sha256": "f" * 64}
        json_file(fetch_path, fetch | {"reader": fetch["reader"] | other_origin})
        self.rollback_trace(rollback | {"reader": rollback["reader"] | other_origin})
        self.rejected("must use the independently observed native target")

    def test_scoped_view_cannot_be_qualified_with_a_bare_denial(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace(CDN.SCOPED_VIEW_CHECK, [self.request("edge", 403)])
        self.rejected("invalid fields in scoped_view_isolation")

    def test_scoped_view_rejects_wrong_sibling_payload_and_missing_ranges(self) -> None:
        self.configure("azure", "private", "direct")
        trace = json.loads((self.directory / "traces" / f"{CDN.SCOPED_VIEW_CHECK}.json").read_bytes())
        for index, changes, reason in (
            (1, {"body_sha256": "7" * 64}, "verified warm target and authorized sibling reads"),
            (2, {"key_sha256": trace["placement"]["views"]["target"]["key_sha256"]}, "approved target and sibling keys"),
            (3, {"range_offset": None}, "denied a full and ranged GET of the sibling"),
            (5, {"cache": "miss"}, "verified warm target and authorized sibling reads"),
        ):
            with self.subTest(index=index, changes=changes):
                requests = trace["requests"].copy()
                requests[index] = requests[index] | changes
                self.scoped_view_trace(trace | {"requests": requests})
                self.rejected(reason)

    def test_scoped_view_rejects_unapproved_placement(self) -> None:
        self.configure("gcs", "private", "direct")
        trace = json.loads((self.directory / "traces" / f"{CDN.SCOPED_VIEW_CHECK}.json").read_bytes())
        self.scoped_view_trace(trace | {"placement": trace["placement"] | {"source_prefix_sha256": "7" * 64}})
        self.rejected("placement differs from the approved control fixture")

    def test_cache_reuse_requires_authenticated_issuer_observations(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace(CDN.CACHE_REUSE_CHECK, [self.request("edge", 200, "miss"), self.request("edge", 200, "hit")])
        self.rejected("invalid fields in authorized_cache_reuse")

    def test_cache_reuse_checks_the_read_and_its_signed_grant(self) -> None:
        self.configure("s3", "private", "direct")
        trace = json.loads((self.directory / "traces" / f"{CDN.CACHE_REUSE_CHECK}.json").read_bytes())
        for changes, reason in (
            ({"cache": "miss"}, "verified first miss and second hit"),
            ({"key_sha256": "f" * 64}, "verified first miss and second hit"),
            ({"finished_at_unix_ms": 5000}, "complete within the corresponding signed grant lifetime"),
        ):
            with self.subTest(changes=changes):
                primary, second = trace["requests"]
                self.cache_reuse_trace(trace | {"requests": [primary, second | changes]})
                self.rejected(reason)
        grants = trace["grants"] | {"second": trace["grants"]["primary"]}
        self.cache_reuse_trace(trace | {"grants": grants})
        self.rejected("an authenticated grant for each approved principal")

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
        self.configure("s3", "private", "direct", consumer="server")
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
