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


def release_policy_row(scope: str) -> str:
    return f"{scope} 0 16 8 5000\n"


def cli_candidate(scope: str, platform: str) -> dict:
    system, arch = platform.split("-", 1)
    target = {"darwin": "apple-darwin", "linux": "unknown-linux-gnu", "windows": "pc-windows-msvc"}[system]
    return {
        "source_sha": COMMIT, "target_triple": f"{arch}-{target}", "cargo_profile": "release",
        "cargo_features": "default,tier",
        "qualification_feature": False, "binary_sha256": hashlib.sha256(platform.encode()).hexdigest(),
        "release_policy_sha256": hashlib.sha256(release_policy_row(scope).encode()).hexdigest(),
    }


def sdk_candidate(scope: str, platform: str) -> dict:
    return cli_candidate(scope, platform) | {
        "cargo_features": "default,files,local,publish,remote,service",
        "binary_sha256": hashlib.sha256(f"sdk/{platform}".encode()).hexdigest(),
    }


def service_candidate(scope: str, platform: str) -> dict:
    return cli_candidate(scope, platform) | {
        "cargo_features": "provider-cdn", "binary_sha256": hashlib.sha256(f"{scope}/{platform}".encode()).hexdigest(),
    }


def range_response(family: str, size: int, cache: str = "hit") -> dict:
    return {"family": family, "status": 206, "cache": cache, "bytes": size, "complete": True,
            "range": True, "object_bytes": 2 * size, "requested_bytes": size, "elapsed_ms": 1}


def write_product_viewer_proof(client_path: Path, key_sha256: str | None = None, *,
                                   filename: str = CDN.PRODUCT_VIEWER_RECORD) -> None:
    raw = client_path.read_bytes()
    client = json.loads(raw)
    edge = client["scope"].split("/", 1)[0]
    records = {}
    proof = {"schema": f"crab.{edge}-viewer-requests/v1", "client_report_sha256": hashlib.sha256(raw).hexdigest(),
             "records": records}
    if edge != "cloud-cdn":
        proof["origin_requests"] = {}
    for request, response in client["edge_responses"].items():
        log = {"status": response["status"], "cache": response["cache"], "response_bytes": response["bytes"]}
        if edge != "cloud-cdn" or key_sha256 is not None:
            log["key_sha256"] = key_sha256 or "8" * 64
        if edge == "cloud-cdn":
            log.update(cache_fill_bytes=0, origin_requests={})
        records[request] = log
    json_file(client_path.parent / filename, proof)


def write_git_measurements(inputs: Path, registration: dict, platform: str, native_target: dict | None = None,
                           hydration: dict | None = None, service: dict | None = None) -> None:
    provider = CDN.PROVIDER_FOR_EDGE[registration["scope"].split("/", 1)[0]]
    sdk = CDN.is_direct_sdk_git_scope(registration["scope"])
    desktop = CDN.is_direct_desktop_git_scope(registration["scope"])
    managed = CDN.is_managed_cli_git_scope(registration["scope"])
    cli = CDN.requires_cli_workload(registration["scope"])
    payload_bytes = CDN.cli_payload_bytes(registration)
    action = CDN.cli_action_name(registration["scope"])
    for ordinal, (location, region) in enumerate(registration["regions"].items()):
        for index in range(1, registration["samples"] + 1):
            oid = hashlib.sha1(f"{location}/{index}".encode()).hexdigest()
            blob = hashlib.sha256(oid.encode()).hexdigest()
            pair = inputs / location / f"{index:04d}"
            pair.mkdir(parents=True, exist_ok=True)
            start = 1_000_000 * (ordinal + 1) + index * 10_000
            if desktop and "publication_sha256" in registration:
                start += PUBLIC_SEAL
            publication_digest = hashlib.sha256(f"{location}/{index}/receipt".encode()).hexdigest()
            if cli or desktop:
                publication = {
                    "schema": "crab.cdn-git-publication/v1", "oid": oid, "sha256": blob, "published": True,
                    "payload_bytes": payload_bytes,
                    "ref": f"refs/heads/crab-cdn-qualification-123-2-{oid[:12]}",
                    "visible_at_unix_ms": start - (3 if desktop or managed else 1), "candidate": registration["candidate"],
                    "source_sha": registration["source_sha"], "scope": registration["scope"],
                    "configuration_sha256": registration["configuration_sha256"],
                    "workflow_run_id": "123", "workflow_run_attempt": "2",
                }
                publication_digest = json_file(pair / "publication.json", publication)
                if desktop or managed:
                    target = registration["desktop_native_target"] if desktop else native_target
                    observed = dict.fromkeys(CDN.GIT_NATIVE_REF_FIELDS, "4" * 64) | target | {
                        "ref_sha256": hashlib.sha256(publication["ref"].encode()).hexdigest(),
                        "oid": oid, "probe_source_sha": publication["source_sha"],
                    }
                    if managed:
                        observed["repository_sha256"] = "3" * 64
                    json_file(pair / CDN.GIT_NATIVE_REF_FILES["published"], {
                        key: publication[key] for key in CDN.CDN_PROVENANCE_FIELDS
                    } | {"schema": "crab.cdn-native-ref/v1", "phase": "published", "platform": platform,
                         "publication_sha256": publication_digest, "started_at_unix_ms": start - 2,
                         "observed_at_unix_ms": start - 1, "native_ref": observed})
            for sequence, (arm, metrics) in enumerate(sample(provider).items()):
                directory = pair / arm
                directory.mkdir(exist_ok=True)
                started = start + sequence * 1_000
                if hydration is not None:
                    started += 100_000
                if service is not None:
                    started += (service["started_at_unix_ms"] // 1000) * 1000
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
                    "candidate": registration.get("sdk_candidate" if sdk or hydration is not None else "candidate"),
                    "reader": {
                        "binding_sha256": "" if arm == "origin" else registration["viewer_binding_sha256"],
                        "cache_root_sha256": hashlib.sha256(f"{location}/{index}/{arm}/cache".encode()).hexdigest(),
                        "public_export_sha256": "" if arm == "origin" else registration.get("publication_sha256", ""),
                        "origin_sha256": "7" * 64, "prefix_sha256": "8" * 64,
                    } | (native_target or {}),
                    "publication_oid": oid, "expected_sha256": blob, "observed_sha256": blob,
                    "publication_sha256": publication_digest,
                    "expected_bytes": payload_bytes, "observed_bytes": payload_bytes,
                    "started_at_unix_ms": started, "finished_at_unix_ms": started + metrics["elapsed_ms"],
                    "visible_new_ref": True, "native_writes": 1,
                    "edge_responses": {}, "native_traffic": {"get_responses": 1, "head_responses": 0, "body_bytes": 8},
                }
                if managed:
                    client["managed_grant"] = registration["managed_issuer"] | {"grant_version": 2}
                    if arm == "origin":
                        client["managed_grant"].update(approval_sha256=None, grant_version=1)
                proof = {}
                origin_requests = {}
                if provider in CDN.VIEWER_REQUEST_FILES and edge:
                    request_id = hashlib.sha256(f"{location}/{index}/{arm}/viewer".encode()).hexdigest()
                    cache = "miss" if arm == "cold_edge" else "hit"
                    family = CDN.scope_parts(registration["scope"])[3]
                    client["edge_responses"][request_id] = range_response(family, metrics["edge_bytes"], cache)
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
                    if hydration is not None:
                        key_sha256 = hydration["xorb_key_sha256"]
                        proof[request_id]["key_sha256"] = key_sha256
                        for record in origin_requests.values():
                            record["key_sha256"] = key_sha256
                if service is not None:
                    read_fields = CDN.PRODUCT_COUNTERS | CDN.SDK_TIMING_FIELDS | {"arm", "edge_responses", "native_traffic"}
                    client = service | {key: client[key] for key in read_fields}
                    client.pop("transition_key_sha256", None)
                    client["reader"] = service["reader"]
                    if arm == "origin":
                        client["reader"] = service["reader"] | {"binding_sha256": "", "public_export_sha256": ""}
                    instance_arm = "warm_edge" if arm == "warm_local" else arm
                    client["service_instance_sha256"] = hashlib.sha256(f"{location}/{index}/{instance_arm}/process".encode()).hexdigest()
                    if service["lfs_key_sha256"] is not None:
                        client["cache_root_sha256"] = ("" if arm == "origin" else hashlib.sha256(
                            f"{location}/{index}/{instance_arm}/disk".encode()).hexdigest())
                        for record in (*proof.values(), *origin_requests.values()):
                            record["key_sha256"] = service["lfs_key_sha256"]
                    if arm == "warm_local":
                        client["primer_sha256"] = hashlib.sha256((directory.parent / "warm_edge/client.json").read_bytes()).hexdigest()
                    if provider == "gcs" and arm == "cold_edge":
                        client["native_traffic"] = {"get_responses": 0, "head_responses": 0, "body_bytes": 0}
                elif hydration is not None:
                    root = client["reader"]["cache_root_sha256"]
                    read_fields = CDN.PRODUCT_COUNTERS | CDN.SDK_TIMING_FIELDS | {"arm", "edge_responses", "native_traffic"}
                    client = hydration | {key: client[key] for key in read_fields}
                    reader = hydration["reader"] | {"cache_root_sha256": root}
                    if arm == "origin":
                        reader |= {"binding_sha256": "", "approval_sha256": ""}
                    if arm == "warm_local":
                        primer = (directory.parent / "warm_edge/client.json").read_bytes()
                        client["primer_sha256"] = hashlib.sha256(primer).hexdigest()
                        reader["cache_root_sha256"] = json.loads(primer)["reader"]["cache_root_sha256"]
                    client["reader"] = reader
                elif sdk or desktop:
                    cache = client["reader"].pop("cache_root_sha256")
                    client = {key: value for key, value in client.items() if key in CDN.SDK_GIT_READ_FIELDS} | {
                        "schema": "crab.cdn-sdk-git-read/v1", "commit": oid, "sha256": blob,
                        "git_blob_oid_sha256": hashlib.sha256(f"{blob}/git".encode()).hexdigest(),
                        "bytes": CDN.GIT_PAYLOAD_SIZE, "approval_sha256": "" if arm == "origin" else registration.get(
                            "viewer_approval_sha256", registration.get("publication_sha256", ""),
                        ),
                    }
                    if desktop:
                        client["schema"] = "crab.cdn-desktop-git-read/v1"
                        client["candidate"] = registration["desktop_candidate"]
                        client["reader"] |= registration["desktop_native_target"]
                        client["cache_root_sha256"] = cache
                    if arm == "warm_local":
                        primer = (directory.parent / "warm_edge/client.json").read_bytes()
                        client["primer_sha256"] = hashlib.sha256(primer).hexdigest()
                        if desktop:
                            client["cache_root_sha256"] = json.loads(primer)["cache_root_sha256"]
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
    def test_release_rejects_inconsistent_retained_transfer_size(self):
        client = dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "started_at_unix_ms": 1, "finished_at_unix_ms": 2, "elapsed_ms": 1,
            "all_edge_attempts": 1, "native_traffic": dict.fromkeys(CDN.NATIVE_TRAFFIC_FIELDS, 0),
            "edge_responses": {"1" * 64: range_response("xorb", 8) | {"requested_bytes": 7}},
        }
        with self.assertRaisesRegex(ValueError, "inconsistent completed CDN response"):
            CDN.verify_git_traffic(client, "cloudfront", "product read")

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        (self.directory / "traces").mkdir()

    def configure(self, provider: str, access: str, profile: str, *, consumer: str = "cli", family: str = "git",
                  platform: str = "linux-x86_64", cli_payload_bytes: int | None = None) -> None:
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
            if cli_payload_bytes is not None:
                self.registration["cli_payload_bytes"] = cli_payload_bytes
        sdk = CDN.is_direct_sdk_git_scope(self.scope)
        hydration = CDN.is_direct_xorb_read_scope(self.scope)
        mount = CDN.is_direct_mount_xorb_scope(self.scope)
        service = CDN.is_direct_service_scope(self.scope)
        if mount:
            self.registration.update(workload=CDN.MOUNT_XORB_ACTION, viewer_binding_sha256="e" * 64,
                                     viewer_approval_sha256="d" * 64,
                                     candidate=cli_candidate(self.scope, platform) | {"cargo_features": "default,nfs,tier"})
        elif sdk or hydration:
            self.registration.update(workload=CDN.SDK_HYDRATION_ACTION if hydration else CDN.SDK_GIT_ACTION,
                                     sdk_candidate=sdk_candidate(self.scope, platform),
                                     viewer_binding_sha256="e" * 64)
            if access == "private":
                self.registration["viewer_approval_sha256"] = "d" * 64
        managed_git = CDN.is_managed_cli_git_scope(self.scope)
        if profile == "managed":
            self.registration["managed_issuer"] = {
                "identity_sha256": "1" * 64, "version": "2026.11.22", "approval_sha256": "2" * 64,
            }
            if managed_git:
                self.registration["managed_issuer"]["repository_sha256"] = "7" * 64
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
            CDN.UNRELATED_VIEWER_CHECK: [edge_hit, control, edge_deny, edge_deny, control, edge_hit],
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
                if not managed_git:
                    self.managed_issuer_trace()
            elif check == CDN.HISTORICAL_ORIGIN_CHECK:
                self.historical_origin_trace(edge)
            elif check == CDN.PUBLIC_TRANSITION_CHECK:
                continue
            elif check == "rollback" and (CDN.requires_cli_workload(self.scope) or sdk or hydration or service):
                continue
            else:
                self.trace(check, observations.get(check, [edge_hit]))
        if CDN.requires_cli_workload(self.scope) or sdk:
            cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
            native = {"origin_sha256": cleanup["targets"]["native"]["origin_sha256"], "prefix_sha256": cleanup["prefix_sha256"]}
            write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform, native_target=native)
            if sdk:
                self.record_git_blob_product()
            else:
                self.record_git_product()
                if managed_git:
                    self.managed_issuer_trace()
        elif hydration:
            self.record_xorb_read()
        elif service:
            self.record_service_product()
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
        if CDN.scope_parts(self.scope)[3] == "lfs":
            entry = Object(CDN.lfs_object_path(entry.sha256), entry.size, entry.sha256)
            old_entry = Object(CDN.lfs_object_path("f" * 64), entry.size, "f" * 64)
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
        managed = CDN.is_managed_cli_git_scope(self.scope)
        if managed:
            fetch["publication_oid"] = "f" * 40
            fetch["edge_responses"] = {"2" * 64: next(iter(fetch["edge_responses"].values()))}
            fetch["reader"]["cache_root_sha256"] = "c" * 64
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
            "payload_bytes": fetch["expected_bytes"],
            "visible_at_unix_ms": fetch["started_at_unix_ms"] - 50, "candidate": fetch["candidate"],
        })
        public = "publication_sha256" in self.registration
        transition = {"transition_key_sha256": self.object_key} if public else {}
        fetch_path = records / "fetch.json"
        json_file(fetch_path, fetch | {"publication_sha256": receipt} | transition)
        write_product_viewer_proof(fetch_path, transition.get("transition_key_sha256"))
        rollback = fetch | dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "publication_sha256": receipt, "arm": "origin", "edge_responses": {},
            "started_at_unix_ms": fetch["finished_at_unix_ms"] + 5,
            "finished_at_unix_ms": fetch["finished_at_unix_ms"] + 10,
            "reader": fetch["reader"] | {"binding_sha256": "", "cache_root_sha256": "1" * 64, "public_export_sha256": ""},
        }
        if managed:
            rollback["managed_grant"] = self.registration["managed_issuer"] | {"approval_sha256": None, "grant_version": 1}
        self.rollback_trace(rollback)
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

    def record_service_product(self) -> None:
        report = self.report
        records = self.directory / CDN.SERVICE_PRODUCT_DIR
        records.mkdir()
        edge, access, _, family, _ = CDN.scope_parts(self.scope)
        if access == "public":
            self.cleanup_trace(access, edge, observed_at_unix_ms=PUBLIC_SEAL + 4_000_000)
        cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
        cleanup_at = cleanup["observed_at_unix_ms"]
        published_at = PUBLIC_SEAL - 35 if access == "public" else cleanup_at - 3_000_100
        edge_at = PUBLIC_SEAL + 10 if access == "public" else published_at + 30
        reader = {"origin_sha256": cleanup["targets"]["native"]["origin_sha256"], "prefix_sha256": cleanup["prefix_sha256"]}
        spec = reader | {"sha256": "5" * 64, "etag_sha256": "6" * 64, "payload_bytes": 8}
        service = service_candidate(self.scope, report["platform"])
        self.registration.update(workload=CDN.SERVICE_READ_ACTION, service_workload=spec, viewer_binding_sha256="e" * 64,
                                 service_candidate=service)
        report["product_action"]["name"] = CDN.SERVICE_READ_ACTION
        provenance = {key: report[key] for key in CDN.CDN_PROVENANCE_FIELDS}
        platform = report["platform"]
        ref = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef"
        commit, payload = "a" * 40, report["product_action"]["expected_sha256"]
        if access == "public" and family == "lfs":
            payload = hashlib.sha256(EXPORT_BODY).hexdigest()
            report["product_action"].update(expected_sha256=payload, observed_sha256=payload)
        writer = cli_candidate(self.scope, platform)
        receipt = json_file(records / "publication.json", provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": commit, "sha256": payload,
            "payload_bytes": spec["payload_bytes"],
            "published": True, "visible_at_unix_ms": published_at - 5, "candidate": writer,
        })
        json_file(records / "retirement.json", provenance | {
            "schema": "crab.cdn-git-retirement/v1", "publication_sha256": receipt, "ref_retired": True,
            "retired_at_unix_ms": cleanup_at - 30, "candidate": writer,
        })
        for phase, at, marker in (("published", published_at, "1"), ("retired", cleanup_at - 20, "2")):
            digest = json_file(records / CDN.GIT_NATIVE_REF_FILES[phase], provenance | {
                "schema": "crab.cdn-native-ref/v1", "phase": phase, "platform": platform,
                "publication_sha256": receipt, "started_at_unix_ms": at, "observed_at_unix_ms": at + 5,
                "native_ref": reader | {
                    "probe_source_sha": COMMIT, "ref_sha256": hashlib.sha256(ref.encode()).hexdigest(),
                    "repository_sha256": "3" * 64, "epoch_sha256": "4" * 64, "root_digest": "b" * 64,
                    "head_sha256": marker * 64, "revision_sha256": marker * 64, "transaction_id": marker * 64,
                    "oid": commit if phase == "published" else None,
                },
            })
            if phase == "published":
                published_digest = digest
        lfs_key = (hashlib.sha256(f"{FIXTURE_PREFIX}/{CDN.lfs_object_path(payload)}".encode()).hexdigest()
                   if family == "lfs" else None)
        common = provenance | dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "schema": "crab.cdn-service-read/v1", "platform": platform, "arm": "origin",
            "publication_sha256": receipt, "native_published_sha256": published_digest, "commit": commit,
            "workload_sha256": spec["sha256"], "payload_sha256": payload, "payload_bytes": spec["payload_bytes"],
            "etag_sha256": spec["etag_sha256"], "git_blob_oid_sha256": "7" * 64 if family == "git" else None,
            "service_instance_sha256": "1" * 64,
            "candidate": service,
            "lfs_key_sha256": lfs_key, "elapsed_ms": 10, "edge_responses": {},
            "reader": reader | {"binding_sha256": "", "public_export_sha256": ""},
            "native_traffic": {"get_responses": 1, "head_responses": 0, "body_bytes": 8},
        }
        if lfs_key is not None:
            common["cache_root_sha256"] = ""
        self.rollback_trace(common | {"started_at_unix_ms": edge_at + 20, "finished_at_unix_ms": edge_at + 30})
        transition = {}
        if access == "public":
            self.registration["service_prior_workload_sha256"] = "4" * 64
            transition = {"transition_key_sha256": self.object_key}
            json_file(records / CDN.SERVICE_PUBLIC_PRIOR_RECORD, common | transition | {
                "arm": "prior_export", "started_at_unix_ms": PUBLIC_SEAL - 20, "finished_at_unix_ms": PUBLIC_SEAL - 10,
                "service_instance_sha256": "3" * 64, "workload_sha256": self.registration["service_prior_workload_sha256"],
                "reader": reader | {"binding_sha256": "e" * 64,
                                    "public_export_sha256": self.registration["previous_publication_sha256"]},
            })
        request = "d" * 64
        response = range_response("lfs" if lfs_key is not None else "git", 8)
        cache = {"cache_root_sha256": "5" * 64} if lfs_key is not None else {}
        client = json_file(records / "edge.json", common | transition | cache | {
            "arm": "edge", "started_at_unix_ms": edge_at, "finished_at_unix_ms": edge_at + 10,
            "service_instance_sha256": "2" * 64,
            "all_edge_attempts": 1, "edge_attempts": 1, "observed_edge_gets": 1, "verified_edge_payloads": 1,
            "edge_bytes": 8, "edge_hits": 1, "edge_responses": {request: response},
            "reader": reader | {"binding_sha256": "e" * 64,
                                "public_export_sha256": self.registration.get("publication_sha256", "")},
        })
        log = {"status": 206, "cache": "hit", "response_bytes": 30}
        if edge != "cloud-cdn" or lfs_key or transition:
            log["key_sha256"] = lfs_key or transition.get("transition_key_sha256", "8" * 64)
        proof = {"schema": f"crab.{edge}-viewer-requests/v1", "client_report_sha256": client, "records": {request: log}}
        if edge == "cloud-cdn":
            log.update(cache_fill_bytes=0, origin_requests={})
        else:
            proof["origin_requests"] = {}
        json_file(records / "viewer-requests.json", proof)
        edge_read = json.loads((records / "edge.json").read_bytes())
        write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform, service=edge_read)

    def record_desktop_product(self) -> None:
        platform = self.report["platform"]
        writer = cli_candidate(self.scope, platform)
        cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
        self.registration.update(workload=CDN.DESKTOP_GIT_ACTION, candidate=writer,
            desktop_candidate=writer | {"cargo_features": "", "binary_sha256": "a" * 64}, viewer_binding_sha256="e" * 64,
            desktop_native_target={"origin_sha256": cleanup["targets"]["native"]["origin_sha256"],
                                   "prefix_sha256": cleanup["prefix_sha256"]})
        if "publication_sha256" not in self.registration:
            self.registration["viewer_approval_sha256"] = "d" * 64
        self.report["product_action"]["name"] = CDN.DESKTOP_GIT_ACTION
        write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration, platform)
        self.record_git_blob_product(desktop=True)
        if "publication_sha256" in self.registration:
            self.public_transition_trace()

    def record_git_blob_product(self, *, desktop: bool = False) -> None:
        records = self.directory / CDN.GIT_PRODUCT_DIR
        records.mkdir(exist_ok=True)
        provenance = {key: self.report[key] for key in CDN.CDN_PROVENANCE_FIELDS}
        platform = self.report["platform"]
        ref = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef"
        cleanup = json.loads((self.directory / "traces/cleanup.json").read_bytes())
        reader = {"origin_sha256": cleanup["targets"]["native"]["origin_sha256"], "prefix_sha256": cleanup["prefix_sha256"]}
        mount = CDN.is_direct_mount_xorb_scope(self.scope)
        writer = self.registration["candidate"] if mount else cli_candidate(self.scope, platform)
        oid, payload = "a" * 40, "b" * 64
        receipt = json_file(records / "publication.json", provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": oid, "sha256": payload, "published": True,
            "payload_bytes": CDN.GIT_PAYLOAD_SIZE,
            "visible_at_unix_ms": PUBLIC_SEAL - 40, "candidate": writer,
        })
        base = provenance | dict.fromkeys(CDN.PRODUCT_COUNTERS, 0) | {
            "schema": f"crab.cdn-{'desktop' if desktop else 'sdk'}-git-read/v1", "platform": platform,
            "candidate": self.registration["candidate" if mount else "desktop_candidate" if desktop else "sdk_candidate"],
            "commit": oid, "sha256": payload, "bytes": CDN.GIT_PAYLOAD_SIZE, "git_blob_oid_sha256": "4" * 64,
            "publication_sha256": receipt, "elapsed_ms": 10, "edge_responses": {},
            "native_traffic": {"get_responses": 1, "head_responses": 0, "body_bytes": 5},
        }
        export = self.registration.get("publication_sha256", "")
        transition = {"transition_key_sha256": self.object_key} if export else {}
        binding = reader | {"binding_sha256": self.registration["viewer_binding_sha256"]}
        request = hashlib.sha256(b"product-git-blob-viewer").hexdigest()
        response = range_response("git", 1)
        snapshot = records / (CDN.DESKTOP_READ_RECORD if desktop else "snapshot.json")
        if desktop:
            base["cache_root_sha256"] = "1" * 64
        json_file(snapshot, base | dict.fromkeys(
            ("edge_attempts", "observed_edge_gets", "edge_hits", "edge_bytes", "verified_edge_payloads", "all_edge_attempts"), 1,
        ) | {
            "arm": "edge", "started_at_unix_ms": PUBLIC_SEAL + 10, "finished_at_unix_ms": PUBLIC_SEAL + 20,
            "approval_sha256": self.registration.get("viewer_approval_sha256", export),
            "reader": binding | {"public_export_sha256": export},
            "edge_responses": {request: response},
        } | transition)
        write_product_viewer_proof(snapshot, transition.get("transition_key_sha256"))
        if export:
            prior = self.registration["previous_publication_sha256"]
            json_file(records / CDN.GIT_PRIOR_EXPORT_RECORD, base | transition | {
                "arm": "prior_export", "started_at_unix_ms": PUBLIC_SEAL - 20, "finished_at_unix_ms": PUBLIC_SEAL - 10,
                "approval_sha256": prior, "reader": binding | {"public_export_sha256": prior},
            } | ({"cache_root_sha256": "2" * 64} if desktop else {}))
        self.rollback_trace(base | {
            "arm": "origin", "started_at_unix_ms": PUBLIC_SEAL + 25, "finished_at_unix_ms": PUBLIC_SEAL + 35,
            "reader": reader | {"binding_sha256": "", "public_export_sha256": ""}, "approval_sha256": "",
        } | ({"cache_root_sha256": "3" * 64} if desktop else {}))
        pack = "8" * 64
        worktrees = () if desktop else (("edge", "snapshot.json", 36), ("origin", "traces/rollback.json", 42))
        for arm, filename, start in worktrees:
            path = records / filename if arm == "edge" else self.directory / filename
            read = json.loads(path.read_bytes())
            read.pop("transition_key_sha256", None)
            clone, fetch = (records / files[arm] for files in (CDN.SDK_CLONE_FILES, CDN.SDK_FETCH_FILES))
            json_file(clone, read | {
                "schema": "crab.cdn-sdk-git-clone/v1", "crab_sha256": writer["binary_sha256"],
                "started_at_unix_ms": PUBLIC_SEAL + start, "finished_at_unix_ms": PUBLIC_SEAL + start + 5,
                "elapsed_ms": 5, "installed_packs": [pack], "verified_edge_packs": [pack] if arm == "edge" else [],
                "edge_responses": {hashlib.sha256(b"sdk-clone-viewer").hexdigest(): response} if arm == "edge" else {},
            })
            json_file(fetch, read | {
                "schema": "crab.cdn-sdk-git-fetch/v1", "crab_sha256": writer["binary_sha256"],
                "started_at_unix_ms": PUBLIC_SEAL + start + 12, "finished_at_unix_ms": PUBLIC_SEAL + start + 17,
                "elapsed_ms": 5, "installed_packs": [pack], "verified_edge_packs": [pack] if arm == "edge" else [],
                "previous_head": "5" * 40, "head_after_fetch": "5" * 40,
                "edge_responses": {hashlib.sha256(b"sdk-fetch-viewer").hexdigest(): response} if arm == "edge" else {},
            })
            if arm == "edge":
                for action, client in (("clone", clone), ("fetch", fetch)):
                    write_product_viewer_proof(client, filename=CDN.SDK_WORKTREE_VIEWER_FILES[action])
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

    def record_xorb_read(self) -> None:
        self.record_git_blob_product()
        mount = CDN.is_direct_mount_xorb_scope(self.scope)
        mounted = {"mounted_oid": "f" * 40} if mount else {}
        records, traces = self.directory / CDN.XORB_PRODUCT_DIR, self.directory / "traces"
        source = self.directory / CDN.GIT_PRODUCT_DIR
        records.mkdir()
        for name in ("publication.json", "retirement.json", *CDN.GIT_NATIVE_REF_FILES.values()):
            (source / name).rename(records / name)
        publication_path, retirement_path = (records / name for name in ("publication.json", "retirement.json"))
        seed = {"scope": self.scope.split("/", 1)[0] + "/private/sdk/git/direct", "configuration_sha256": "9" * 64}
        publication = json.loads(publication_path.read_bytes()) | seed
        receipt = json_file(publication_path, publication)
        retired = 3_000_000
        json_file(retirement_path, json.loads(retirement_path.read_bytes()) | seed | {
            "publication_sha256": receipt, "retired_at_unix_ms": retired,
        })
        for phase, filename in CDN.GIT_NATIVE_REF_FILES.items():
            path = records / filename
            record = json.loads(path.read_bytes()) | {"publication_sha256": receipt}
            if phase == "retired":
                record |= {"started_at_unix_ms": retired + 1, "observed_at_unix_ms": retired + 2}
            json_file(path, record)
        self.registration["hydration_workload_sha256"] = json_file(records / "workload.json", {
            "schema": "crab.cdn-mount-xorb-workload/v1" if mount else "crab.cdn-sdk-hydration-workload/v1",
            "configuration_sha256": CONFIGURATION,
            "pointer_sha256": publication["sha256"], "payload_sha256": "c" * 64,
            "payload_bytes": 1024, "crab_sha256": publication["candidate"]["binary_sha256"],
            "global_prefix_sha256": "5" * 64,
            "publication_sha256": receipt,
        } | mounted)
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
            ("edge", source / "snapshot.json", records / ("mount.json" if mount else "hydration.json"), "1"),
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
                "schema": "crab.cdn-mount-xorb-read/v1" if mount else "crab.cdn-sdk-hydration/v1",
                "workload_sha256": self.registration["hydration_workload_sha256"],
                "crab_sha256": publication["candidate"]["binary_sha256"], "pointer_sha256": publication["sha256"],
                "payload": {"sha256": "c" * 64, "bytes": 1024}, "xorb_key_sha256": self.object_key, "reader": reader,
            } | (mounted | {"unmounted": True} if mount else {})
            if arm == "edge":
                json_file(destination, evidence)
                write_product_viewer_proof(destination, self.object_key)
                write_git_measurements(self.directory / CDN.GIT_BENCHMARK_DIR, self.registration,
                                       self.report["platform"], hydration=evidence)
            else:
                self.rollback_trace(evidence)
        self.record_sparse_xorb_reads(records, traces, mount, target["bytes"])
        self.report["product_action"].update(expected_sha256="c" * 64, observed_sha256="c" * 64,
                                             expected_bytes=1024, observed_bytes=1024)

    def record_sparse_xorb_reads(self, records: Path, traces: Path, mount: bool, object_bytes: int) -> None:
        spec = json.loads((records / "workload.json").read_bytes())
        edge = json.loads((records / ("mount.json" if mount else "hydration.json")).read_bytes())
        origin = json.loads((traces / "rollback.json").read_bytes())
        window = {"offset": 128, "bytes": 32, "sha256": "d" * 64}
        projected = {} if mount else {"projected_oid": "f" * 40, "sdk_sha256": edge["candidate"]["binary_sha256"]}
        approval = json_file(records / "sparse-workload.json", spec | projected | {
            "schema": CDN.MOUNT_XORB_RANGE_WORKLOAD if mount else CDN.SDK_XORB_RANGE_WORKLOAD,
            "read_range": window,
        })
        self.registration["sparse_workload_sha256"] = approval
        for index, arm in enumerate(("origin", "cold-edge", "warm-edge")):
            start = 2_000_000 + index * 2_000
            name = f"sparse-{arm}"
            path = records / f"{name}.json"
            read = (origin if arm == "origin" else edge) | {
                "schema": CDN.MOUNT_XORB_RANGE_READ if mount else CDN.SDK_XORB_RANGE_READ,
                "workload_sha256": approval, "range_offset": window["offset"],
                "payload": {key: window[key] for key in ("bytes", "sha256")},
                "started_at_unix_ms": start, "finished_at_unix_ms": start + 10, "elapsed_ms": 10,
                "reader": edge["reader"] | {"cache_root_sha256": hashlib.sha256(name.encode()).hexdigest()},
            } | ({} if mount else {"commit": projected["projected_oid"]})
            if arm == "origin":
                json_file(path, read | {"reader": origin["reader"] | {
                    "cache_root_sha256": read["reader"]["cache_root_sha256"],
                }})
                continue
            cold = arm == "cold-edge"
            cache = "miss" if cold else "hit"
            request = hashlib.sha256(name.encode()).hexdigest()
            response = range_response("xorb", object_bytes // 2, cache)
            json_file(path, read | {
                "edge_hits": int(not cold), "edge_misses": int(cold), "edge_bytes": response["bytes"],
                "edge_responses": {request: response},
            })
            filename = f"{name}-viewer-requests.json"
            write_product_viewer_proof(path, self.object_key, filename=filename)
            if cold:
                proof_path = records / filename
                proof = json.loads(proof_path.read_bytes())
                if proof["schema"] == "crab.cloud-cdn-viewer-requests/v1":
                    proof["records"][request]["cache_fill_bytes"] = object_bytes
                else:
                    timestamp = {"at_unix_s": start // 1000} if proof["schema"] == "crab.cloudfront-viewer-requests/v1" else {
                        "at_unix_ms": start,
                    }
                    proof["origin_requests"]["c" * 64] = timestamp | {
                        "key_sha256": self.object_key, "status": 206, "response_body_bytes": object_bytes,
                    }
                json_file(proof_path, proof)

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
                "grants": {role: principals[role] | {"issued_at_unix_ms": 1000, "expires_at_unix": 5 + index}
                           for index, role in enumerate(principals)},
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
        if check in CDN.EXPIRY_CHECKS:
            extra.setdefault("viewer_expires_at_unix", EXPIRY)
            extra.setdefault("viewer_grant_sha256", "b" * 64)
        if check == CDN.EXPIRY_FULL_CHECK:
            requests = [
                {"started_at_unix_us": started, "finished_at_unix_us": started + 100} | row
                for started, row in zip((EXPIRY * 1_000_000 - 2000, (EXPIRY + 2) * 1_000_000 + 1000), requests)
            ]
        if check == CDN.UNRELATED_VIEWER_CHECK:
            requests = [
                ({
                    "viewer_expires_at_unix": EXPIRY, "viewer_grant_sha256": "b" * 64,
                    "started_at_unix_us": 1000 + index * 100, "finished_at_unix_us": 1050 + index * 100,
                    "body_sha256": "e" * 64 if index in (1, 4) else None,
                    "range_offset": 0 if index == 3 else None,
                } if 1 <= index <= 4 else {}) | row for index, row in enumerate(requests)
            ]
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
        elif CDN.is_direct_sdk_git_scope(self.scope) or CDN.requires_desktop_workload(self.scope, action["name"]):
            trace = CDN.derive_git_blob_public_transition(self.directory / CDN.GIT_PRODUCT_DIR, self.report, self.registration)
        elif CDN.is_direct_service_scope(self.scope):
            trace = CDN.derive_service_public_transition(self.directory / CDN.SERVICE_PRODUCT_DIR, self.report, self.registration)
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
        self.registration["rotation_overlap_policy_sha256"] = CDN.deployment_policy_digest(edge, "private", overlap)
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
        if CDN.is_managed_cli_git_scope(self.scope):
            trace["repository_sha256"] = issuer["repository_sha256"]
            trace["product_reads"] = {}
            for role, path in zip(CDN.MANAGED_GIT_READ_ROLES, ("git-product/fetch.json", "traces/rollback.json")):
                raw = (self.directory / path).read_bytes()
                trace["product_reads"][role] = hashlib.sha256(raw).hexdigest()
                trace["operations"][role]["native_payload_reads"] = json.loads(raw)["native_traffic"]["get_responses"]
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
            (lambda trace: trace["policies"]["overlap"]["components"].update({CDN.ROTATING_POLICY["cloudfront"]: "a" * 64}),
             "rotation overlap policy differs from the preregistered deployment"),
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
        policy.write_text("# Release rows\n" + release_policy_row(self.scope.replace("/cli/", "/sdk/")))
        self.rejected("scope is absent from the release policy", *args)
        policy.write_text(release_policy_row(self.scope))
        result = self.command(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        policy.write_bytes(release_policy_row(self.scope).replace("\n", "\r\n").encode())
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
        policy.write_text("# Changed after the build\n" + release_policy_row(self.scope))
        self.rejected("did not embed the release checkout", *args)
        policy.write_text(release_policy_row(self.scope) * 2)
        self.rejected("duplicate CDN release scope", *args)

    def test_repository_release_policy_contains_only_valid_rows(self) -> None:
        policy = SCRIPT.parents[2] / "crates/crab-storage/src/cdn/qualified-scopes.txt"
        CDN.release_scopes(policy)

    def test_release_policy_rejects_unbounded_or_ambiguous_rows(self):
        scope = "cloudfront/private/cli/git/direct"
        policy = self.directory / "qualified-scopes.txt"
        for limits in ("", "0 0 0 100", "0 64 0 100", "0 8 64 100", "6 0 0 0", "06 0 0 100",
                       "6 0 0 4294967296", "6 0 0 +100", "6 0 0 100 extra"):
            with self.subTest(limits=limits):
                policy.write_text(f"{scope} {limits}\n")
                with self.assertRaisesRegex(ValueError, "invalid CDN release read limits"):
                    CDN.release_scopes(policy)

    def test_release_verification_rejects_provider_joined_transfer_outside_limits(self):
        self.configure("s3", "private", "direct")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope))
        arm = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001/cold_edge"
        client = json.loads((arm / "client.json").read_bytes())
        next(iter(client["edge_responses"].values()))["elapsed_ms"] = 5001
        digest = json_file(arm / "client.json", client)
        for name in (CDN.VIEWER_REQUEST_FILES["s3"], "measurement.json"):
            path = arm / name
            json_file(path, json.loads(path.read_bytes()) | {"client_report_sha256": digest})
        self.rejected("disagrees with its compiled CDN read limits", "--cdn-release-policy", str(policy))

    def test_release_uses_front_door_logs_when_the_client_cache_result_is_unknown(self):
        self.configure("azure", "private", "direct")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope))
        for label, hits, misses in (("cold_edge", 0, 1), ("warm_edge", 1, 0)):
            arm = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001" / label
            client = json.loads((arm / "client.json").read_bytes())
            self.assertEqual((client["edge_hits"], client["edge_misses"]), (hits, misses))
            client.update(edge_hits=0, edge_misses=0, edge_unknown=1)
            next(iter(client["edge_responses"].values()))["cache"] = "unknown"
            digest = json_file(arm / "client.json", client)
            for name in (CDN.VIEWER_REQUEST_FILES["azure"], "measurement.json"):
                path = arm / name
                json_file(path, json.loads(path.read_bytes()) | {"client_report_sha256": digest})
        result = self.command("--cdn-release-policy", str(policy))
        self.assertEqual(result.returncode, 0, result.stderr)

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
            (lambda t: t.update(repository_sha256="4" * 64), "different registered repository"),
            (lambda t: t["product_reads"].update(v1_peer="4" * 64), "match the retained Git fetch and rollback"),
            (lambda t: t.update(workflow_run_attempt="1"), "match the release row and workflow attempt"),
            (lambda t: t["operations"]["approved_v2"].update(request_approval_sha256="4" * 64),
             "did not use the authenticated approval"),
            (lambda t: t["operations"]["approved_v2"].update(grant_approval_sha256="4" * 64),
             "did not verify the granted authority"),
            (lambda t: t["operations"]["approved_v2"].update(verified_output_sha256="4" * 64),
             "did not verify the granted authority"),
            (lambda t: t["operations"]["v1_peer"].update(edge_payload_requests=1), "wrong payload path"),
            (lambda t: t["operations"]["approved_v2"].update(edge_payload_requests=2), "differs from the retained Git product read"),
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

    def test_cli_benchmark_pair_must_use_the_independently_observed_native_target(self) -> None:
        self.configure("s3", "private", "direct")
        pair = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001"
        clients = {path: json.loads(path.read_bytes()) for path in (pair / arm / "client.json" for arm in CDN.ARMS)}
        for field in ("origin_sha256", "prefix_sha256"):
            with self.subTest(field=field):
                for path, client in clients.items():
                    reader = client["reader"] | {field: "0" * 64}
                    digest = json_file(path, client | {"reader": reader})
                    for filename in ("measurement.json", CDN.VIEWER_REQUEST_FILES["s3"]):
                        evidence = path.with_name(filename)
                        if evidence.exists():
                            json_file(evidence, json.loads(evidence.read_bytes()) | {"client_report_sha256": digest})
                self.rejected("changed the independently observed native target")

    def test_rejects_a_positive_product_summary_that_differs_from_the_fetch(self) -> None:
        self.configure("s3", "private", "direct")
        self.report["product_action"]["verified_edge_payloads"] += 1
        self.rejected("product action differs from the retained publication, fetch, and retirement evidence")

    def test_cli_product_requires_logs_for_its_own_viewer_requests(self) -> None:
        self.configure("s3", "private", "direct")
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.PRODUCT_VIEWER_RECORD
        proof = json.loads(path.read_bytes())
        records = {"0" * 64: next(iter(proof["records"].values()))}
        json_file(path, proof | {"records": records})
        self.rejected("CDN logs must cover each measured viewer attempt exactly once")
        path.unlink()
        self.rejected(CDN.PRODUCT_VIEWER_RECORD)

    def test_public_cli_product_logs_must_confirm_the_new_export_key(self) -> None:
        self.configure("gcs", "public", "direct")
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.PRODUCT_VIEWER_RECORD
        proof = json.loads(path.read_bytes())
        for record in proof["records"].values():
            record["key_sha256"] = "0" * 64
        json_file(path, proof)
        self.rejected("CDN logs did not confirm the new export object")

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
        target = {"origin_sha256": "1" * 64, "inventories": {"versions_and_snapshots": "empty", "uncommitted_blobs": "empty"}}
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
        self.configure("azure", "private", "direct", consumer="desktop")
        self.trace("rollback", [self.request("edge", 200), self.request("origin", 200) | {"key_sha256": "c" * 64}])
        self.rejected("rollback requires an edge read followed by a native read of the same complete object")

    def test_sdk_release_rejects_product_summary_without_regional_sdk_benchmark_records(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        self.assertEqual(self.command().returncode, 0)
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
        (self.directory / CDN.GIT_BENCHMARK_DIR).rename(self.directory / "benchmark-backup")
        self.rejected("requires retained regional SDK benchmark records", "--cdn-release-policy", str(policy))

    def test_generic_fixture_summary_cannot_promote_a_release_scope(self) -> None:
        for provider, consumer, profile in (("gcs", "desktop", "direct"), ("azure", "sdk", "managed")):
            with self.subTest(provider=provider, consumer=consumer, profile=profile), TemporaryDirectory() as directory:
                self.directory = Path(directory)
                (self.directory / "traces").mkdir()
                self.configure(provider, "private", profile, consumer=consumer)
                fixture = self.command()
                self.assertEqual(fixture.returncode, 0, fixture.stderr)
                policy = self.directory / "qualified-scopes.txt"
                policy.write_text(release_policy_row(self.scope))
                self.rejected("CDN promotion requires a product-specific workload", "--cdn-release-policy", str(policy))

    def test_sdk_release_requires_clone_of_the_same_publication_before_retirement(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
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
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
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
                policy.write_text(release_policy_row(self.scope), encoding="utf-8")
                result = self.command("--cdn-release-policy", str(policy))
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_private_sdk_release_requires_provider_logs_for_each_product_action(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        product = self.directory / CDN.GIT_PRODUCT_DIR
        for name in (CDN.PRODUCT_VIEWER_RECORD, *CDN.SDK_WORKTREE_VIEWER_FILES.values()):
            with self.subTest(name=name):
                path = product / name
                raw = path.read_bytes()
                path.unlink()
                self.rejected(name)
                path.write_bytes(raw)

    def test_sdk_product_actions_cannot_reuse_the_same_provider_request(self) -> None:
        self.configure("gcs", "private", "direct", consumer="sdk")
        product = self.directory / CDN.GIT_PRODUCT_DIR
        clone = product / CDN.SDK_CLONE_FILES["edge"]
        fetch = product / CDN.SDK_FETCH_FILES["edge"]
        responses = json.loads(clone.read_bytes())["edge_responses"]
        json_file(fetch, json.loads(fetch.read_bytes()) | {"edge_responses": responses})
        write_product_viewer_proof(fetch, filename=CDN.SDK_WORKTREE_VIEWER_FILES["fetch"])
        self.rejected("SDK product actions reused provider requests")

    def test_sdk_distinct_viewer_reads_can_share_an_s3_fill_with_second_precision(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk")
        product = self.directory / CDN.GIT_PRODUCT_DIR
        snapshot = json.loads((product / "snapshot.json").read_bytes())
        at = snapshot["started_at_unix_ms"] // 1000
        for name in (CDN.PRODUCT_VIEWER_RECORD, *CDN.SDK_WORKTREE_VIEWER_FILES.values()):
            path = product / name
            proof = json.loads(path.read_bytes())
            key = next(iter(proof["records"].values()))["key_sha256"]
            json_file(path, proof | {"origin_requests": {"0" * 64: {
                "key_sha256": key, "status": 206, "response_body_bytes": 32, "at_unix_s": at,
            }}})
        result = self.command()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_sdk_regional_pair_cannot_reuse_a_product_request(self) -> None:
        for family, product, action in (
            ("git", CDN.GIT_PRODUCT_DIR, "snapshot.json"),
            ("xorb", CDN.XORB_PRODUCT_DIR, "hydration.json"),
        ):
            with self.subTest(family=family), TemporaryDirectory() as directory:
                self.directory = Path(directory)
                (self.directory / "traces").mkdir()
                self.configure("gcs", "private", "direct", consumer="sdk", family=family)
                path = self.directory / product / action
                request = next(iter(json.loads(path.read_bytes())["edge_responses"]))
                arm = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001/cold_edge"
                client = json.loads((arm / "client.json").read_bytes())
                response = next(iter(client["edge_responses"].values()))
                digest = json_file(arm / "client.json", client | {"edge_responses": {request: response}})
                log = arm / CDN.VIEWER_REQUEST_FILES["gcs"]
                proof = json.loads(log.read_bytes())
                record = next(iter(proof["records"].values()))
                json_file(log, proof | {"client_report_sha256": digest, "records": {request: record}})
                self.rejected("CDN log requests were reused across arms")

    def test_sdk_hydration_product_proof_must_identify_the_approved_xorb(self) -> None:
        self.configure("gcs", "private", "direct", consumer="sdk", family="xorb")
        path = self.directory / CDN.XORB_PRODUCT_DIR / CDN.PRODUCT_VIEWER_RECORD
        proof = json.loads(path.read_bytes())
        next(iter(proof["records"].values()))["key_sha256"] = "0" * 64
        json_file(path, proof)
        self.rejected("did not read the approved hydration xorb")

    def test_sdk_hydration_requires_the_exact_lifecycle(self) -> None:
        self.configure("s3", "private", "direct", consumer="sdk", family="xorb")
        self.assertEqual(self.command().returncode, 0)
        policy = self.directory / "policy.txt"
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
        result = self.command("--cdn-release-policy", str(policy))
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.directory / CDN.XORB_PRODUCT_DIR
        edge = records / "hydration.json"
        native = self.directory / "traces/rollback.json"
        original = json.loads(native.read_bytes())
        edge_read = json.loads(edge.read_bytes())
        for changes, reason in (
            ({"reader": original["reader"] | {"cache_root_sha256": edge_read["reader"]["cache_root_sha256"]}}, "fresh cache"),
            ({"reader": original["reader"] | {"binding_sha256": "5" * 64}}, "required viewer approval"),
            ({"reader": original["reader"] | {"global_prefix_sha256": "6" * 64}}, "different projected view"),
            ({"observed_edge_gets": 1, "edge_hits": 1, "edge_attempts": 1, "all_edge_attempts": 1}, "without edge or cache activity"),
            ({"finished_at_unix_ms": 3_000_001}, "same published ref after rollback"),
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

    def test_sparse_xorb_release_requires_verified_ranges_and_provider_observations(self) -> None:
        self.configure("gcs", "private", "direct", consumer="sdk", family="xorb")
        records = self.directory / CDN.XORB_PRODUCT_DIR
        cold = records / "sparse-cold-edge.json"
        warm = records / "sparse-warm-edge.json"
        warm_log = records / "sparse-warm-edge-viewer-requests.json"
        cold_log = records / "sparse-cold-edge-viewer-requests.json"
        raw = warm.read_bytes()
        warm_data = json.loads(raw)
        cold_data = json.loads(cold.read_bytes())
        cold_proof = json.loads(cold_log.read_bytes())
        request, response = next(iter(warm_data["edge_responses"].items()))
        failures = (
            (warm, warm_data | {"range_offset": 0}, "approved file offset"),
            (warm, warm_data | {"reader": cold_data["reader"]}, "reused a local cache"),
            (warm, warm_data | {"edge_responses": {request: response | {
                "range": False, "status": 200, "object_bytes": response["bytes"],
            }}}, "sparse read must verify xorb ranges"),
            (cold_log, cold_proof | {"records": {
                key: record | {"cache_fill_bytes": 0} for key, record in cold_proof["records"].items()
            }}, "provider-confirmed miss and origin fill"),
        )
        for path, data, message in failures:
            with self.subTest(message=message):
                original = path.read_bytes()
                json_file(path, data)
                self.rejected(message)
                path.write_bytes(original)
        log = json.loads(warm_log.read_bytes())
        replayed = next(iter(cold_data["edge_responses"]))
        digest = json_file(warm, warm_data | {"edge_responses": {replayed: response}})
        json_file(warm_log, log | {"client_report_sha256": digest, "records": {replayed: log["records"][request]}})
        self.rejected("reused a CDN viewer request")
        warm.write_bytes(raw)
        unknown = response | {"cache": "unknown"}
        digest = json_file(warm, warm_data | {"edge_hits": 0, "edge_unknown": 1, "edge_responses": {request: unknown}})
        json_file(warm_log, log | {"client_report_sha256": digest, "records": {
            request: log["records"][request] | {"cache": "unknown"},
        }})
        self.rejected("provider-confirmed hits for every xorb range")
        warm_log.unlink()
        self.rejected(warm_log.name)

    def test_sdk_hydration_benchmarks_require_their_own_cache_and_provider_requests(self) -> None:
        for provider in ("s3", "gcs", "azure"):
            with self.subTest(provider=provider), TemporaryDirectory() as directory:
                self.directory = Path(directory)
                (self.directory / "traces").mkdir()
                self.configure(provider, "private", "direct", consumer="sdk", family="xorb")
                results = self.command()
                self.assertEqual(results.returncode, 0, results.stderr)
                pair = self.directory / CDN.GIT_BENCHMARK_DIR / "near/0001"
                local = pair / "warm_local/client.json"
                cold = pair / "cold_edge/client.json"
                local_raw = local.read_bytes()
                warm_read = json.loads(local_raw)
                for change, error in (
                    ({"primer_sha256": "a" * 64}, "did not repeat its own warm edge hydration"),
                    ({"reader": warm_read["reader"] | {"cache_root_sha256": "a" * 64}}, "in the same cache"),
                    ({"xorb_key_sha256": "0" * 64}, "different projected view"),
                    ({"started_at_unix_ms": 3_000_001, "finished_at_unix_ms": 3_000_002}, "outside the independently observed publication lifetime"),
                ):
                    with self.subTest(change=change):
                        json_file(local, warm_read | change)
                        self.rejected(error)
                local.write_bytes(local_raw)
                cold_raw = cold.read_bytes()
                cold_read = json.loads(cold_raw)
                json_file(cold, cold_read | {"reader": cold_read["reader"] | {
                    "cache_root_sha256": json.loads((pair / "origin/client.json").read_bytes())["reader"]["cache_root_sha256"],
                }})
                self.rejected("reused a local cache root")
                cold.write_bytes(cold_raw)
                log = cold.parent / CDN.VIEWER_REQUEST_FILES[provider]
                original_proof = log.read_bytes()
                proof = json.loads(original_proof)
                next(iter(proof["records"].values()))["key_sha256"] = "0" * 64
                json_file(log, proof)
                self.rejected("did not read the approved hydration xorb")
                proof = json.loads(original_proof)
                proof["client_report_sha256"] = json_file(cold, cold_read | {"edge_hits": 1, "edge_misses": 0})
                json_file(log, proof)
                self.rejected("transfer totals disagree with the logged responses")

    def test_public_sdk_release_requires_ordinary_export_transition_calls(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
        args = ("--cdn-release-policy", str(policy))
        result = self.command(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.GIT_PRIOR_EXPORT_RECORD
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
        self.rejected(CDN.GIT_PRIOR_EXPORT_RECORD, *args)

    def test_public_sdk_release_requires_provider_delivery_of_the_transition_key(self) -> None:
        self.configure("gcs", "public", "direct", consumer="sdk")
        policy = self.directory / "qualified-scopes.txt"
        policy.write_text(release_policy_row(self.scope), encoding="utf-8")
        args = ("--cdn-release-policy", str(policy))
        path = self.directory / CDN.GIT_PRODUCT_DIR / CDN.PRODUCT_VIEWER_RECORD
        proof = json.loads(path.read_bytes())
        json_file(path, proof | {"client_report_sha256": "0" * 64})
        self.rejected("CDN viewer evidence differs from the measured fetch", *args)
        for record in proof["records"].values():
            record["key_sha256"] = "0" * 64
        json_file(path, proof)
        self.rejected("CDN logs did not confirm the new export object", *args)
        path.unlink()
        self.rejected(CDN.PRODUCT_VIEWER_RECORD, *args)

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
            ({"finished_at_unix_ms": 6000}, "complete within the corresponding signed grant lifetime"),
        ):
            with self.subTest(changes=changes):
                primary, second = trace["requests"]
                self.cache_reuse_trace(trace | {"requests": [primary, second | changes]})
                self.rejected(reason)
        grants = trace["grants"] | {"second": trace["grants"]["primary"]}
        self.cache_reuse_trace(trace | {"grants": grants})
        self.rejected("an authenticated grant for each approved principal")
        grants = trace["grants"] | {"second": trace["grants"]["second"] | {"expires_at_unix": 5}}
        self.cache_reuse_trace(trace | {"grants": grants})
        self.rejected("distinct signed viewer grants with different expirations")

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

    def test_full_expiry_requires_deny_after_signed_expiry(self) -> None:
        self.configure("s3", "private", "direct")
        warm = self.request("edge", 200, "hit")
        denied = self.request("edge", 403) | {
            "started_at_unix_us": (EXPIRY + 1) * 1_000_000,
            "finished_at_unix_us": (EXPIRY + 1) * 1_000_000 + 100,
        }
        self.trace(CDN.EXPIRY_FULL_CHECK, [warm, denied])
        self.rejected("full GET must hit the warm object before expiry and be denied after expiry")

    def test_full_and_parallel_expiry_require_the_same_signed_viewer(self) -> None:
        self.configure("s3", "private", "direct")
        self.trace(CDN.EXPIRY_FULL_CHECK, [self.request("edge", 200, "hit"), self.request("edge", 403)],
                   viewer_grant_sha256="d" * 64)
        self.rejected("full and ranged expiry checks must use the same signed viewer grant")

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
        key = CDN.UNRELATED_VIEWER_CHECK
        self.trace(key, [hit, deny])
        self.rejected("unrelated viewer denial requires six edge observations")
        self.trace(key, [hit, control, deny, deny, deny | {"key_sha256": control["key_sha256"]}, hit])
        self.rejected("unrelated viewer must read the same distinct control object before and after denial")
        self.trace(key, [hit, hit, deny, deny, hit, hit])
        self.rejected("unrelated viewer must read the same distinct control object before and after denial")
        self.trace(key, [hit, control, deny, hit | {"status": 206, "bytes": 1}, control, hit])
        self.rejected("unrelated viewer must be denied a full and ranged GET of the warm target")

    def test_unrelated_denials_require_the_same_live_grant_and_a_real_range(self) -> None:
        self.configure("s3", "private", "direct")
        key = CDN.UNRELATED_VIEWER_CHECK
        observed = json.loads((self.directory / "traces" / f"{key}.json").read_bytes())["requests"]
        for index, tamper, reason in (
            (3, {"range_offset": None}, "full GET and an exact single-byte range"),
            (3, {"viewer_grant_sha256": "c" * 64}, "same signed grant"),
            (4, {"finished_at_unix_us": EXPIRY * 1_000_000}, "finish before the signed grant expires"),
            (4, {"body_sha256": "d" * 64}, "identical control bytes"),
        ):
            with self.subTest(tamper=tamper):
                requests = observed.copy()
                requests[index] = requests[index] | tamper
                self.trace(key, requests)
                self.rejected(reason)

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
        self.configure("s3", "private", "direct", consumer="desktop")
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
