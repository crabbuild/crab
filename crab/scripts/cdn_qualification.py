"""Validate one retained CDN qualification row for a real provider."""

from __future__ import annotations

import hashlib
import json
import math
import re
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, Callable

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "deploy/cdn"))
from public_inventory import ExportError, native_placement, read_publication, validate_public_origins  # noqa: E402


SCHEMA = "crab.cdn-qualification"
PROVIDERS = {
    "s3": ("cloudfront", "amazon-s3"),
    "gcs": ("cloud-cdn", "google-cloud-storage"),
    "azure": ("front-door", "azure-blob-storage"),
}
PROVIDER_FOR_EDGE = {edge: provider for provider, (edge, _) in PROVIDERS.items()}
CONSUMERS = {"cli", "sdk", "desktop", "mount", "server", "s3-gateway"}
FAMILIES = {"git", "lfs", "xorb", "capsule", "chunk-page", "shard"}
COMMON_CHECKS = {
    "exact_get",
    "exact_range",
    "mutable_route_denied",
    "neighbor_route_denied",
    "edge_route_policy",
    "native_origin_read",
    "origin_only_control",
    "deployment_policy",
    "rollback",
    "cleanup",
}
DEPLOYMENT_POLICY_COMPONENTS = {
    "cloudfront": {"distribution", "origin-access-control", "cache-policy", "viewer-controls", "viewer-function",
                   "bucket-policy", "bucket-access", "viewer-key-ids"},
    "cloud-cdn": {"https-route", "backend-cache", "viewer-policy", "bucket-access", "viewer-key-ids"},
    "front-door": {"front-door-route", "viewer-policy", "blob-access", "viewer-key-ids"},
}
KEY_ROTATION_CHECK = "key_rotation"
CACHE_REUSE_CHECK = "authorized_cache_reuse"
CACHE_REUSE_ROLES = ("primary", "second")
SCOPED_VIEW_CHECK = "scoped_view_isolation"
SCOPED_VIEW_ROLES = {"target", "sibling"}
SCOPED_VIEW_PROBES = ("target_before", "sibling_before", "sibling_full", "sibling_range", "sibling_after", "target_after")
SCOPED_VIEW_FIELDS = {"scope_sha256", "generation_sha256", "physical_prefix_sha256", "grant_prefix_sha256",
                      "key_sha256", "body_sha256", "bytes"}
MANAGED_ISSUER_CHECK = "managed_v2_issuer"
MANAGED_ISSUER_FIELDS = {"identity_sha256", "version", "approval_sha256"}
MAX_ROTATION_GRANT_SECONDS = 900
MAX_VIEWER_GRANT_SECONDS = 900
VIEWER_KEY_ID = re.compile(r"[A-Za-z0-9_-]{1,90}")
ROTATING_POLICY = {"cloudfront": "viewer-controls", "cloud-cdn": "backend-cache", "front-door": "viewer-policy"}
TRACE_REQUEST_FIELDS = {"source", "status", "cache", "bytes", "elapsed_ms", "key_sha256"}
EDGE_ROUTE_QUERY_PROBES = {
    "cloudfront": {"version": "?versionId=null", "response_override": "?response-content-type=application%2Foctet-stream"},
    "cloud-cdn": {"generation": "?generation=1", "response_override": "?response-content-type=application%2Foctet-stream"},
    "front-door": {"version": "?versionid=1970-01-01T00%3A00%3A00.0000000Z",
                   "snapshot": "?snapshot=1970-01-01T00%3A00%3A00.0000000Z",
                   "response_override": "?rsct=application%2Foctet-stream"},
}
EXPIRY_RANGE_CHECK = "expired_parallel_range_denied"
EXPIRY_RANGE_FIELDS = {"range_offset", "started_at_unix_us", "finished_at_unix_us"}
EXPIRY_SETTLE_SECONDS = 2
WARM_DENIAL_CHECKS = {
    "warm_anonymous_viewer_denied",
    "warm_expired_viewer_denied",
    "warm_unrelated_principal_denied",
}
PRIVATE_CHECKS = WARM_DENIAL_CHECKS | {
    "private_origin_denied",
    KEY_ROTATION_CHECK,
    CACHE_REUSE_CHECK,
    EXPIRY_RANGE_CHECK,
    SCOPED_VIEW_CHECK,
}
PUBLIC_EXPORT_CHECK = "approved_public_export"
HISTORICAL_ORIGIN_CHECK = "historical_origin"
PUBLIC_TRANSITION_CHECK = "public_export_transition"
HISTORICAL_KINDS = {
    "cloudfront": {"versionId"},
    "cloud-cdn": {"generation"},
    "front-door": {"versionid", "snapshot"},
}
MAX_HISTORICAL_OBJECT_BYTES = 1024 * 1024
MAX_HISTORICAL_VERSIONS = 8
HISTORICAL_VERSION_FIELDS = {"kind", "selector_sha256", "sha256", "size"}
PUBLIC_EXPORT_DIR = "public-export"
PUBLIC_EXPORT_BEFORE = "before"
PUBLIC_EXPORT_FILES = ("inventory.json", "publication.json")
SDK_HYDRATION_ACTION = "sdk-xorb-hydration"
SDK_HYDRATION_DIR = "xorb-product"
SDK_HYDRATION_WORKLOAD_FIELDS = {
    "schema", "configuration_sha256", "pointer_sha256", "payload_sha256", "payload_bytes", "crab_sha256",
    "global_prefix_sha256", "publication_sha256",
}
PUBLIC_CHECKS = {
    PUBLIC_EXPORT_CHECK, HISTORICAL_ORIGIN_CHECK, PUBLIC_TRANSITION_CHECK, "live_origin_denied",
    "export_origin_access", "public_listing_denied",
}
CLEANUP_INVENTORIES = {
    "cloudfront": ({"versions_and_delete_markers": "empty"},),
    "cloud-cdn": ({"generations": "empty", "soft_deleted": "empty"},
                  {"generations": "empty", "soft_deleted": "disabled"}),
    "front-door": ({"blobs": "empty", "soft_deleted": "empty"}, {"versions_and_snapshots": "empty"}),
}
ARMS = ("origin", "cold_edge", "warm_edge", "warm_local")
METRICS = {
    "elapsed_ms",
    "client_bytes",
    "origin_gets",
    "origin_fill_bytes",
    "edge_hits",
    "edge_misses",
    "edge_unknown",
    "edge_bytes",
    "local_payload_hits",
    "rejected_bytes",
    "peak_memory_bytes",
    "peak_disk_bytes",
    "cost_microusd",
}
GIT_BENCHMARK_DIR = "git-benchmarks"
GIT_PRODUCT_DIR = "git-product"
SERVICE_PRODUCT_DIR = "service-product"
SERVICE_READ_ACTION = "service-http-read"
SERVICE_PAYLOAD_LIMIT = 512 * 1024 * 1024
SERVICE_WORKLOAD_REGISTRATION_FIELDS = {"sha256", "origin_sha256", "prefix_sha256", "etag_sha256", "payload_bytes"}
SDK_PRIOR_EXPORT_RECORD = "before-export.json"
SDK_CLONE_FILES = {"edge": "clone-edge.json", "origin": "clone-origin.json"}
SDK_FETCH_FILES = {"edge": "fetch-edge.json", "origin": "fetch-origin.json"}
VIEWER_REQUEST_FILES = {"s3": "cloudfront-requests.json", "gcs": "cloud-cdn-requests.json", "azure": "front-door-requests.json"}
GIT_FETCH_RECORD_LIMIT = 256 * 1024
GIT_NATIVE_REF_FILES = {"published": "provider-published.json", "retired": "provider-retired.json"}
SERVICE_PRODUCT_FILES = {
    "publication": "publication.json", "retirement": "retirement.json", "edge": "edge.json",
    "viewer_requests": "viewer-requests.json", **GIT_NATIVE_REF_FILES,
}
GIT_NATIVE_REF_FIELDS = {
    "origin_sha256", "prefix_sha256", "ref_sha256", "repository_sha256", "epoch_sha256",
    "root_digest", "head_sha256", "revision_sha256", "transaction_id", "oid", "probe_source_sha",
}
GIT_PAYLOAD_SIZE = 256 * 1024
GIT_OID = re.compile(r"[0-9a-f]{40}")
GIT_TEST_REF = re.compile(r"refs/heads/crab-cdn-qualification-[1-9][0-9]{0,19}-[1-9][0-9]{0,19}-[0-9a-f]{12}")
CDN_PROVENANCE_FIELDS = {"source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt"}
GIT_PUBLICATION_FIELDS = CDN_PROVENANCE_FIELDS | {"schema", "ref", "oid", "sha256", "published", "visible_at_unix_ms", "candidate"}
GIT_RETIREMENT_FIELDS = CDN_PROVENANCE_FIELDS | {"schema", "publication_sha256", "ref_retired", "retired_at_unix_ms", "candidate"}
CLI_BUILD_FIELDS = {"source_sha", "target_triple", "cargo_profile", "cargo_features", "qualification_feature", "release_policy_sha256"}
CLI_CANDIDATE_FIELDS = CLI_BUILD_FIELDS | {"binary_sha256"}
CLI_NATIVE_FIELDS = {"origin_sha256", "prefix_sha256"}
CLI_READ_FIELDS = CLI_NATIVE_FIELDS | {"binding_sha256", "cache_root_sha256", "public_export_sha256"}
CLI_TARGET = re.compile(r"(aarch64|x86_64)-(apple-darwin|unknown-linux-(?:gnu|musl)|pc-windows-(?:msvc|gnu|gnullvm))")
PRODUCT_COUNTERS = {
    "edge_attempts", "observed_edge_gets", "edge_bytes", "edge_hits", "edge_misses", "edge_unknown",
    "incomplete_edge_bytes", "edge_elapsed_ms_total", "verified_edge_payloads", "all_edge_attempts", "local_payload_hits",
}
GIT_CLIENT_METRICS = {"elapsed_ms", "edge_hits", "edge_misses", "edge_unknown", "edge_bytes", "local_payload_hits"}
GIT_EXTERNAL_METRICS = METRICS - GIT_CLIENT_METRICS
NATIVE_TRAFFIC_FIELDS = {"get_responses", "head_responses", "body_bytes"}
GIT_MEASUREMENT_FIELDS = GIT_EXTERNAL_METRICS | {"schema", "client_report_sha256", "provider", "region"}
GIT_FETCH_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | {
    "schema", "name", "arm", "expected_sha256", "observed_sha256", "expected_bytes", "observed_bytes",
    "native_writes", "visible_new_ref", "elapsed_ms", "publication_oid", "started_at_unix_ms", "finished_at_unix_ms",
    "platform", "publication_sha256", "candidate", "reader", "edge_responses", "native_traffic",
}
SDK_GIT_ACTION = "sdk-git-snapshot"
DIRECT_READ_TARGET_FIELDS = CLI_NATIVE_FIELDS | {"binding_sha256", "public_export_sha256"}
SDK_PAYLOAD_FIELDS = {"sha256", "bytes", "git_blob_oid_sha256"}
SDK_TIMING_FIELDS = {"started_at_unix_ms", "finished_at_unix_ms", "elapsed_ms"}
SDK_PROBE_FIELDS = SDK_TIMING_FIELDS | {"commit", "payload", "build", "warm_local"}
SDK_GIT_READ_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | SDK_PAYLOAD_FIELDS | SDK_TIMING_FIELDS | {
    "schema", "arm", "platform", "candidate", "approval_sha256", "publication_sha256",
    "edge_responses", "native_traffic", "reader", "commit",
}
SDK_HYDRATION_READ_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | SDK_TIMING_FIELDS | {
    "schema", "arm", "platform", "workload_sha256", "candidate", "crab_sha256", "pointer_sha256", "payload",
    "reader", "xorb_key_sha256", "native_traffic", "edge_responses",
}
SERVICE_READ_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | SDK_TIMING_FIELDS | {
    "schema", "arm", "platform", "workload_sha256", "publication_sha256", "commit", "payload_sha256",
    "payload_bytes", "etag_sha256", "git_blob_oid_sha256", "lfs_key_sha256", "reader", "native_traffic", "edge_responses",
    "native_published_sha256", "service_instance_sha256", "candidate",
}
GIT_MODES = {"origin": "origin", "cold_edge": "edge", "warm_edge": "edge", "warm_local": "warm_local"}
SHA256 = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
RUN_NUMBER = re.compile(r"[1-9][0-9]{0,19}")
LABEL = re.compile(r"[a-z0-9][a-z0-9_.-]{0,79}")
PREFIX = re.compile(r"crab-cdn-qualification/[a-z0-9][a-z0-9-]{0,79}")
CLI_PLATFORMS = (
    "darwin-aarch64", "darwin-x86_64", "linux-aarch64", "linux-x86_64",
    "windows-aarch64", "windows-x86_64",
)
DESKTOP_PLATFORMS = ("darwin-aarch64", "darwin-x86_64", "linux-aarch64", "linux-x86_64", "windows-x86_64")


class CdnEvidenceError(ValueError):
    """The retained evidence does not qualify this release row."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CdnEvidenceError(message)


def fields(value: Any, names: set[str], where: str) -> dict[str, Any]:
    require(isinstance(value, dict) and value.keys() == names, f"invalid fields in {where}")
    return value


def matches(pattern: re.Pattern[str], value: Any) -> bool:
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def positive(value: Any) -> bool:
    return type(value) is int and value > 0


def unique_fields(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result = dict(pairs)
    require(len(result) == len(pairs), "duplicate evidence field")
    return result


def decode(raw: bytes) -> Any:
    return json.loads(raw, object_pairs_hook=unique_fields)


def scope_parts(scope: str) -> tuple[str, str, str, str, str]:
    parts = scope.split("/")
    require(len(parts) == 5, "expected CDN scope provider/access/consumer/read-family/profile")
    edge, access, consumer, family, profile = parts
    require(edge in PROVIDER_FOR_EDGE and access in {"private", "public"} and consumer in CONSUMERS
            and family in FAMILIES and profile in {"direct", "managed"}, "invalid CDN qualification scope")
    require(access != "public" or (family in {"git", "capsule", "lfs"} and profile == "direct"),
            "public qualification requires direct Git, capsule, or LFS delivery")
    return edge, access, consumer, family, profile


def release_scopes(path: Path) -> set[str]:
    """Read the same exact scope rows that the Rust build embeds."""
    scopes = set()
    for line in path.read_bytes().decode("utf-8").replace("\r\n", "\n").split("\n"):
        if not line or line.startswith("#"):
            continue
        scope_parts(line)
        require(line not in scopes, f"duplicate CDN release scope: {line}")
        scopes.add(line)
    return scopes


def required_platform_rows(scopes: set[str]) -> set[str]:
    rows = set()
    for scope in scopes:
        _, _, consumer, _, _ = scope_parts(scope)
        # The embedded policy is active on every published platform for the
        # product. Other consumers currently qualify on Linux x86_64.
        platforms = {
            "cli": CLI_PLATFORMS, "mount": CLI_PLATFORMS, "desktop": DESKTOP_PLATFORMS,
        }.get(consumer, ("linux-x86_64",))
        rows.update(f"{scope}/{platform}" for platform in platforms)
    return rows


def verify_bundle_row(
    directory: Path, row: str, registration: Any, source: str, run_id: str, attempt: str,
    *, policy: Path | None = None,
) -> None:
    scope, platform = row.rsplit("/", 1)
    edge, _, _, _, _ = scope_parts(scope)
    provider = PROVIDER_FOR_EDGE[edge]
    require(matches(LABEL, platform), "invalid CDN qualification platform")
    require(isinstance(registration, dict), f"missing approved CDN benchmark registration for {row}")
    configuration = registration.get("configuration_sha256")
    require(matches(SHA256, configuration), f"invalid CDN configuration digest for {row}")
    with TemporaryDirectory(prefix="crab-cdn-benchmark-") as temporary:
        benchmark = Path(temporary) / "registration.json"
        benchmark.write_text(json.dumps(registration), encoding="utf-8")
        command = [
            sys.executable, str(Path(__file__).with_name("verify-provider-qualification-report.py")),
            str(directory / provider / "report.json"), "--provider", provider, "--source-sha", source,
            "--run-id", run_id, "--run-attempt", attempt,
            "--cdn-report", str(directory / row / "cdn-report.json"), "--cdn-scope", scope,
            "--cdn-configuration-sha256", configuration, "--cdn-platform", platform,
            "--cdn-benchmark-registration", str(benchmark),
        ]
        if policy is not None:
            command.extend(("--cdn-release-policy", str(policy)))
        subprocess.run(command, check=True)


def required_check_names(access: str, profile: str) -> set[str]:
    required = COMMON_CHECKS | (PRIVATE_CHECKS if access == "private" else PUBLIC_CHECKS)
    if profile == "managed":
        required |= {MANAGED_ISSUER_CHECK}
    return required


def read_trace(path: Path, name: str) -> bytes:
    with path.open("rb") as trace_file:
        raw = trace_file.read(256 * 1024 + 1)
    require(len(raw) <= 256 * 1024, f"trace artifact is too large: {name}")
    return raw


def verify_cleanup_trace(value: Any, report: dict[str, Any], edge: str, access: str) -> int:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "check", "prefix_sha256", "targets", "observed_at_unix_ms"},
                   "cleanup inventory trace")
    require(trace["schema"] == "crab.cdn-cleanup-trace/v1" and trace["check"] == "cleanup"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS), "trace provenance mismatch: cleanup")
    prefix = (report["fixture_prefix"] + "/").encode()
    require(trace["prefix_sha256"] == hashlib.sha256(prefix).hexdigest() and positive(trace["observed_at_unix_ms"]),
            "cleanup inventory must observe the approved fixture prefix")
    roles = {"native", "export"} if access == "public" else {"native"}
    targets = fields(trace["targets"], roles, "cleanup targets")
    origins = set()
    for target in targets.values():
        observed = fields(target, {"origin_sha256", "inventories"}, "cleanup target")
        require(matches(SHA256, observed["origin_sha256"]) and observed["inventories"] in CLEANUP_INVENTORIES[edge],
                "cleanup requires completed empty native inventories for the selected provider")
        origins.add(observed["origin_sha256"])
    require(len(origins) == len(roles), "cleanup must check the live origin and export independently")
    return trace["observed_at_unix_ms"]


def deployment_component_digest(value: Any) -> str:
    canonical = json.dumps(value, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode()).hexdigest()


def deployment_component_names(edge: str, access: str) -> set[str]:
    return DEPLOYMENT_POLICY_COMPONENTS[edge] | ({"public-export"} if access == "public" else {"private-binding"})


def deployment_policy_digest(edge: str, access: str, components: Any) -> str:
    require(isinstance(components, dict) and components.keys() == deployment_component_names(edge, access),
            "deployment policy components are incomplete")
    require(all(matches(SHA256, digest) for digest in components.values()), "deployment policy requires redacted component digests")
    return deployment_component_digest({"edge": edge, "access": access, "components": components})


def public_export_binding(
    fixture: dict[str, Any], deployment: str, inventory_sha256: str,
    source_placement: dict[str, str], export_placement: dict[str, str],
) -> dict[str, Any]:
    require_isolated_public_origins(fixture)
    require(source_placement == fixture["source_placement"], "live source differs from the approved public placement")
    return {"source_origin": fixture["native_origin"], "source_placement": source_placement,
            "export_origin": fixture["export_origin"],
            "export_placement": checked_placement(fixture["export_origin"], export_placement),
            "repository_prefix": fixture["fixture_prefix"], "base_url": fixture["base_url"].rstrip("/"),
            "deployment": deployment, "inventory_sha256": inventory_sha256}


def require_isolated_public_origins(fixture: dict[str, Any]) -> None:
    try:
        validate_public_origins(fixture["native_origin"], fixture["export_origin"])
    except ExportError as error:
        raise CdnEvidenceError(str(error)) from None


def checked_placement(origin: str, placement: Any) -> dict[str, str]:
    try:
        return native_placement(origin, placement)
    except ExportError as error:
        raise CdnEvidenceError(str(error)) from None


def verify_deployment_policy_trace(value: Any, report: dict[str, Any], edge: str, access: str, expected: str) -> None:
    provenance = {"source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt"}
    trace = fields(value, provenance | {"schema", "components", "deployment_policy_sha256"}, "deployment policy trace")
    require(trace["schema"] == "crab.cdn-deployment-policy/v1" and all(trace[key] == report[key] for key in provenance),
            "deployment policy trace must match the release scope, configuration and workflow attempt")
    actual = deployment_policy_digest(edge, access, trace["components"])
    require(trace["deployment_policy_sha256"] == actual == expected,
            "observed deployment policy differs from the approved policy")


def verify_key_rotation(value: Any, report: dict[str, Any], edge: str, approved_policy: str) -> tuple[str, int]:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "grants", "policies", "requests"},
                   "viewer key rotation trace")
    require(trace["schema"] == "crab.cdn-key-rotation/v1"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS),
            "viewer key rotation must match the release scope, configuration and workflow attempt")
    roles = ("retired", "replacement")
    grants = fields(trace["grants"], set(roles), "viewer key rotation grants")
    keys = []
    for role in roles:
        grant = fields(grants[role], {"key_id", "expires_at_unix"}, "rotation grant")
        require(matches(VIEWER_KEY_ID, grant["key_id"]) and positive(grant["expires_at_unix"]),
                "rotation grant needs a valid signer and signed expiration")
        keys.append(grant["key_id"])
    require(keys[0] != keys[1], "rotation requires two distinct viewer keys")

    phases = ("overlap", "retired", "confirmed")
    policies = fields(trace["policies"], set(phases), "viewer key rotation policies")
    for phase in phases:
        snapshot = fields(policies[phase], {"observed_at_unix_us", "components"}, "rotation policy snapshot")
        components = snapshot["components"]
        digest = deployment_policy_digest(edge, "private", components)
        expected_keys = sorted(keys) if phase == "overlap" else [keys[1]]
        require(positive(snapshot["observed_at_unix_us"]) and
                components["viewer-key-ids"] == deployment_component_digest(expected_keys),
                "rotation policy must attest the expected deployed key IDs")
        if phase != "overlap":
            require(digest == approved_policy, "rotated policy differs from the approved deployment")
    overlap_start = policies["overlap"]["observed_at_unix_us"]
    require(all(grant["expires_at_unix"] * 1_000_000 - overlap_start <= MAX_ROTATION_GRANT_SECONDS * 1_000_000
                for grant in grants.values()), "rotation requires short-lived viewer grants")
    for component in deployment_component_names(edge, "private") - {ROTATING_POLICY[edge], "viewer-key-ids"}:
        require(policies["overlap"]["components"][component] == policies["retired"]["components"][component],
                "deployment changed outside viewer signing controls during rotation")

    requests = trace["requests"]
    expected = [
        ("overlap", "retired", "full", (200,)),
        ("overlap", "replacement", "full", (200,)),
        ("retired", "replacement", "full", (200,)),
        ("retired", "retired", "full", (401, 403)),
        ("retired", "retired", "range", (401, 403)),
        ("retired", "replacement", "full", (200,)),
    ]
    require(isinstance(requests, list) and len(requests) == len(expected), "key rotation needs all six ordered edge probes")
    key, size = None, None
    previous = 0
    for request, (phase, role, read, statuses) in zip(requests, expected):
        row = fields(request, TRACE_REQUEST_FIELDS | {
            "phase", "signer", "read", "started_at_unix_us", "finished_at_unix_us",
        }, "rotation request")
        start, end = row["started_at_unix_us"], row["finished_at_unix_us"]
        next_phase = "retired" if phase == "overlap" else "confirmed"
        require((row["phase"], row["signer"], row["read"], row["source"]) == (phase, role, read, "edge")
                and type(row["status"]) is int and row["status"] in statuses and matches(SHA256, row["key_sha256"])
                and type(row["elapsed_ms"]) is int and row["elapsed_ms"] >= 0
                and positive(start) and positive(end) and previous <= start <= end
                and policies[phase]["observed_at_unix_us"] <= start
                and end <= policies[next_phase]["observed_at_unix_us"]
                and end < grants[role]["expires_at_unix"] * 1_000_000,
                "rotation probes must match the deployed phase and complete before the tested grant expires")
        previous = end
        if key is None:
            key, size = row["key_sha256"], row["bytes"]
            require(type(size) is int and size > 1, "rotation needs a complete range-capable object")
        require(row["key_sha256"] == key and type(row["bytes"]) is int, "rotation must use the same object")
        if row["status"] == 200:
            require(row["bytes"] == size and row["cache"] in ("hit", "miss", "unknown")
                    and (phase == "overlap" or row["cache"] == "hit"),
                    "replacement viewer must receive the complete warm object around retired key denial")
        else:
            require(row["bytes"] == 0 and row["cache"] in ("miss", "unknown"),
                    "the retired viewer key must receive no cached content")
    return key, size


def historical_approval_digest(origin_sha256: str, key_sha256: str, versions: list[dict[str, Any]]) -> str:
    return deployment_component_digest({
        "origin_sha256": origin_sha256,
        "key_sha256": key_sha256,
        "versions": sorted(versions, key=lambda item: (item["kind"], item["selector_sha256"])),
    })


def verify_historical_origin(
    value: Any, report: dict[str, Any], registration: dict[str, Any],
    origin_sha256: str, current: dict[str, Any],
) -> None:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {
        "schema", "origin_sha256", "key_sha256", "versions",
    }, "historical native origin trace")
    require(trace["schema"] == "crab.cdn-historical-origin/v1"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS)
            and trace["origin_sha256"] == origin_sha256 and trace["key_sha256"] == current["key_sha256"],
            "historical origin must match the approved live source, object and workflow attempt")
    versions = trace["versions"]
    edge = scope_parts(report["scope"])[0]
    require(isinstance(versions, list) and 1 <= len(versions) <= MAX_HISTORICAL_VERSIONS,
            "historical origin requires retained native versions")
    observed = []
    selectors = set()
    for value in versions:
        version = fields(value, HISTORICAL_VERSION_FIELDS | {"authorized_status", "anonymous_status"},
                         "historical native version")
        require(isinstance(version["kind"], str) and version["kind"] in HISTORICAL_KINDS[edge]
                and matches(SHA256, version["selector_sha256"])
                and matches(SHA256, version["sha256"]) and version["sha256"] != current["body_sha256"]
                and positive(version["size"]) and version["size"] <= MAX_HISTORICAL_OBJECT_BYTES,
                "historical origin requires the selected provider's distinct retired bytes")
        selector = (version["kind"], version["selector_sha256"])
        require(selector not in selectors, "duplicate native history selector")
        selectors.add(selector)
        require(type(version["authorized_status"]) is int and version["authorized_status"] == (206 if edge == "cloudfront" else 200)
                and type(version["anonymous_status"]) is int and version["anonymous_status"] in (401, 403),
                "historical native version must be readable with authorization and denied anonymously")
        observed.append({field: version[field] for field in HISTORICAL_VERSION_FIELDS})
    require(historical_approval_digest(trace["origin_sha256"], trace["key_sha256"], observed)
            == registration["historical_origin_sha256"], "observed native history differs from preregistration")


def verified_publication(root: Path, approval: str) -> tuple:
    require(root.is_dir() and not root.is_symlink(), "public qualification needs retained publication inputs")
    require(all((root / name).is_file() and not (root / name).is_symlink() for name in PUBLIC_EXPORT_FILES),
            "public qualification needs the exact approved inventory and publication receipt")
    try:
        return read_publication(root, approval)
    except (ExportError, ValueError, OSError) as error:
        raise CdnEvidenceError(f"invalid public export publication evidence: {error}") from None


def verify_public_transition(
    value: Any, directory: Path, report: dict[str, Any], registration: dict[str, Any],
    binding: dict[str, Any], entries: Any, sealed_at: int,
) -> None:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "platform", "key_sha256", "native_publication", "before", "after"},
                   "public export transition trace")
    require(trace["schema"] == "crab.cdn-public-transition/v1"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "public export transition does not match the release row and workflow attempt")
    sdk = is_direct_sdk_git_scope(report["scope"])
    previous_approval = registration["previous_publication_sha256"]
    previous, previous_entries, previous_seal = verified_publication(directory / PUBLIC_EXPORT_DIR / PUBLIC_EXPORT_BEFORE,
                                                                      previous_approval)
    require(all(previous[key] == binding[key] for key in binding.keys() - {"inventory_sha256"}),
            "public export transition must keep the same source, export and viewer binding")
    def keys(publication: dict[str, Any], inventory: Any) -> dict[str, Any]:
        return {hashlib.sha256(f"{publication['repository_prefix']}/{entry.path}".encode()).hexdigest(): entry
                for entry in inventory}

    key = trace["key_sha256"]
    prior = keys(previous, previous_entries)
    latest = keys(binding, entries)
    require(matches(SHA256, key) and key in latest, "public transition object is missing from the sealed export")
    require(key not in prior, "the transition object was already approved before the native-only read")
    require(all(latest.get(identity) == entry for identity, entry in prior.items()),
            "the transition export must preserve previously approved objects")
    family = scope_parts(report["scope"])[3]
    path = latest[key].path
    observed_family = ("git" if path.startswith("v2/pack-layers/")
                       else "lfs" if path.startswith("lfs/objects/") else "capsule")
    require(observed_family == family, "public transition object must match the selected read family")
    native = fields(trace["native_publication"], {"reference_sha256", "visible_at_unix_ms"}, "transition native publication")
    require(matches(SHA256, native["reference_sha256"]) and positive(native["visible_at_unix_ms"]),
            "public transition needs a visible native publication")
    if requires_cli_workload(report["scope"]):
        publication = validate_git_publication(read_git_record(directory / GIT_PRODUCT_DIR / "publication.json"), report)
        reference = deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
        require(publication["published"] and native == {
            "reference_sha256": reference, "visible_at_unix_ms": publication["visible_at_unix_ms"],
        }, "public transition must use the ordinary Git workload's confirmed native publication")
    records = {"action", "receipt_sha256", "key_sha256", "reference_sha256", "result_sha256", "result_bytes",
               "started_at_unix_ms", "finished_at_unix_ms", "edge_attempts", "verified_edge_key_sha256", "binding_active"}
    cli_workload = requires_cli_workload(report["scope"])
    if cli_workload or sdk:
        records.add("reader")
    product = report["product_action"]
    if family == "lfs":
        require(path == lfs_object_path(product["observed_sha256"]) and latest[key].sha256 == product["observed_sha256"]
                and latest[key].size == product["observed_bytes"],
                "public transition must verify the exported LFS object itself")
    for phase, approval in (("before", previous_approval), ("after", registration["publication_sha256"])):
        observation = fields(trace[phase], records, f"public transition {phase} action")
        require(observation["action"] == product["name"] and observation["receipt_sha256"] == approval
                and observation["key_sha256"] == key and observation["reference_sha256"] == native["reference_sha256"]
                and observation["result_sha256"] == product["observed_sha256"]
                and positive(observation["result_bytes"]) and observation["result_bytes"] == product["observed_bytes"]
                and observation["binding_active"] is True
                and positive(observation["started_at_unix_ms"]) and positive(observation["finished_at_unix_ms"])
                and observation["started_at_unix_ms"] <= observation["finished_at_unix_ms"],
                "public transition needs the same verified product result with each active approval")
        if cli_workload:
            reader = fields(observation["reader"], CLI_READ_FIELDS, "public transition reader")
            require(reader["binding_sha256"] == registration["viewer_binding_sha256"]
                    and matches(SHA256, reader["cache_root_sha256"]),
                    "public transition must use the registered viewer binding and an isolated local cache")
            require(reader["public_export_sha256"] == approval,
                    "public transition reader did not use the approved export receipt")
    before, after = trace["before"], trace["after"]
    require(type(before["edge_attempts"]) is int and before["edge_attempts"] == 0 and before["verified_edge_key_sha256"] is None
            and positive(after["edge_attempts"]) and after["verified_edge_key_sha256"] == key,
            "public transition must avoid the edge before approval and verify the exact edge object afterward")
    require(previous_seal <= native["visible_at_unix_ms"] <= before["started_at_unix_ms"]
            and before["finished_at_unix_ms"] < sealed_at <= after["started_at_unix_ms"],
            "public export must be sealed after the native-only read and before the verified edge action")
    if cli_workload:
        provenance = {key: report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}}
        fetch = validate_git_fetch(read_git_record(directory / GIT_PRODUCT_DIR / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT),
                                   provenance, "edge", "public transition Git action")
        require(after["started_at_unix_ms"] == fetch["started_at_unix_ms"]
                and after["finished_at_unix_ms"] == fetch["finished_at_unix_ms"]
                and after["edge_attempts"] == fetch["all_edge_attempts"]
                and after["reader"] == fetch["reader"],
                "the public transition must use the retained Git product fetch")
        require(all(before["reader"][key] == after["reader"][key] for key in CLI_NATIVE_FIELDS),
                "the public transition changed the selected native target")
        require(before["reader"]["cache_root_sha256"] != after["reader"]["cache_root_sha256"],
                "the public transition reused the native-only fetch cache")
    elif sdk:
        measured = derive_sdk_public_transition(directory / GIT_PRODUCT_DIR, report, registration)
        require(trace == measured, "public transition differs from the retained SDK snapshot reads")


def verify_public_export(
    directory: Path, report: dict[str, Any], registration: dict[str, Any], policy: dict[str, Any],
    reads: dict[str, list[dict[str, Any]]], native_reads: set[str], cleanup_targets: dict[str, Any],
    history: Any, transition: Any,
) -> None:
    binding, entries, sealed_at = verified_publication(directory / PUBLIC_EXPORT_DIR, registration["publication_sha256"])
    require(binding["repository_prefix"] == report["fixture_prefix"]
            and policy["components"]["public-export"] == deployment_component_digest(binding),
            "publication receipt must match the attested public export and approved repository")
    require(all(
        cleanup_targets[role]["origin_sha256"] == hashlib.sha256(binding[origin].encode()).hexdigest()
        for role, origin in (("native", "source_origin"), ("export", "export_origin"))
    ), "public cleanup must cover the source and destination of the approved publication")
    observations = reads[PUBLIC_EXPORT_CHECK]
    require(len(observations) == 1, "public export needs one complete verified edge observation")
    read = observations[0]
    require(read["source"] == "edge" and read["status"] == 200 and positive(read["bytes"])
            and matches(SHA256, read["body_sha256"]), "public export needs the actual full edge body digest")
    key = read["key_sha256"]
    family = scope_parts(report["scope"])[3]
    for entry in entries:
        path = f"{binding['repository_prefix']}/{entry.path}"
        if hashlib.sha256(path.encode()).hexdigest() == key:
            observed_family = ("git" if entry.path.startswith("v2/pack-layers/")
                               else "lfs" if entry.path.startswith("lfs/objects/") else "capsule")
            require(entry.sha256 == read["body_sha256"] and entry.size == read["bytes"] and family == observed_family,
                    "verified edge object differs from the approved publication entry or read family")
            break
    else:
        raise CdnEvidenceError("verified edge object is absent from the approved publication")
    full_observation = {name: read[name] for name in TRACE_REQUEST_FIELDS}
    require(key in native_reads
            and full_observation in reads["exact_get"]
            and any(row["key_sha256"] == key for row in reads["live_origin_denied"])
            and any(row["key_sha256"] == key for row in reads["export_origin_access"])
            and all(row["key_sha256"] == key for row in reads["edge_route_policy"])
            and any(row["key_sha256"] == key and row["source"] == "edge"
                    and row["status"] == 206 and 0 < row["bytes"] < read["bytes"] for row in reads["exact_range"]),
            "public export proof must cover the same edge, native, and export object")
    verify_historical_origin(history, report, registration, cleanup_targets["native"]["origin_sha256"], read)
    verify_public_transition(transition, directory, report, registration, binding, entries, sealed_at)


def verify_managed_issuer(
    value: Any, report: dict[str, Any], registration: dict[str, Any], policy: dict[str, Any],
) -> None:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {
        "schema", "platform", "issuer", "binding_sha256", "placement_sha256", "workload", "operations",
    }, "managed issuer trace")
    require(trace["schema"] == "crab.cdn-managed-issuer/v1"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "managed issuer trace must match the release row and workflow attempt")
    approved = registration["managed_issuer"]
    issuer = fields(trace["issuer"], {"identity_sha256", "version"}, "observed production issuer")
    require(all(issuer[key] == approved[key] for key in issuer),
            "managed issuer identity or version differs from the approved production issuer")
    approval = approved["approval_sha256"]
    require(trace["binding_sha256"] == approval
            and trace["placement_sha256"] == policy["components"]["private-binding"],
            "managed resolution must observe the approved binding and attested placement")
    action = report["product_action"]
    require(trace["workload"] == action["name"], "managed issuer trace must exercise the registered product action")

    operations = fields(trace["operations"], {"approved_v2", "v1_peer", "denied_v2", "mismatched_v2"},
                        "managed issuer operations")
    for role, value in operations.items():
        operation = fields(value, {
            "request_versions", "request_approval_sha256", "status", "grant_version", "grant_approval_sha256",
            "result", "verified_output_sha256", "verified_output_bytes", "edge_payload_requests", "native_payload_reads",
        }, "managed issuer operation")
        versions = operation["request_versions"]
        expected_version = 1 if role == "v1_peer" else 2
        require(isinstance(versions, list) and len(versions) == 1
                and type(versions[0]) is int and versions[0] == expected_version,
                "managed issuer operation retried or downgraded transfer issuance")
        require(operation["request_approval_sha256"] == (None if role == "v1_peer" else approval),
                "managed issuance did not use the authenticated approval")
        status, grant_version, grant_approval = (operation[key] for key in ("status", "grant_version", "grant_approval_sha256"))
        edge, native = (operation[key] for key in ("edge_payload_requests", "native_payload_reads"))
        require(all(type(n) is int and n >= 0 for n in (edge, native))
                and type(status) is int and type(operation["verified_output_bytes"]) is int,
                "invalid managed issuer observation")
        if role in {"approved_v2", "v1_peer"}:
            require(status == 201 and type(grant_version) is int and grant_version == expected_version
                    and grant_approval == (approval if role == "approved_v2" else None)
                    and operation["result"] == "verified"
                    and operation["verified_output_sha256"] == action["observed_sha256"]
                    and operation["verified_output_bytes"] == action["observed_bytes"],
                    "managed operation did not verify the granted authority and expected output")
            require((edge >= action["observed_edge_gets"] if role == "approved_v2" else edge == 0 and native > 0),
                    "managed operation used the wrong payload path")
        else:
            require(operation["verified_output_sha256"] is None and operation["verified_output_bytes"] == edge == native == 0,
                    "failed managed issuance attempted payload reads or returned content")
            if role == "denied_v2":
                require(status == 403 and grant_version is None and grant_approval is None
                        and operation["result"] == "issuer_denied", "managed v2 issuer denial was not terminal")
            else:
                require(status == 201 and type(grant_version) is int and grant_version == 2
                        and matches(SHA256, grant_approval) and grant_approval != approval
                        and operation["result"] == "client_rejected", "client did not reject a mismatched managed v2 approval")


def verify_check_evidence(
    report: dict[str, Any], edge: str, access: str, profile: str, directory: Path, *, registration: dict[str, Any],
) -> tuple[int, int, str, bytes | None]:
    required = required_check_names(access, profile)
    checks = fields(report["checks"], required, "checks")
    origin_denial = "private_origin_denied" if access == "private" else "live_origin_denied"
    fixture_checks = WARM_DENIAL_CHECKS | {
        "exact_get", "exact_range", "edge_route_policy", "native_origin_read", origin_denial, "export_origin_access",
        EXPIRY_RANGE_CHECK, PUBLIC_EXPORT_CHECK,
    }
    fixture_reads = {}
    cleanup_at = 0
    cleanup_targets = {}
    rotation_key = None
    cache_reuse_key = None
    policy = {}
    managed_issuer = None
    history = None
    transition = None
    rollback = None
    scoped_placement = None
    deployment_policy_sha256 = registration["deployment_policy_sha256"]
    for name, value in checks.items():
        check = fields(value, {"passed", "trace_sha256"}, name)
        require(check["passed"] is True, f"required check failed: {name}")
        raw = read_trace(directory / "traces" / f"{name}.json", name)
        require(
            matches(SHA256, check["trace_sha256"])
            and check["trace_sha256"] == hashlib.sha256(raw).hexdigest(),
            f"trace digest mismatch: {name}",
        )
        if name == "rollback" and (requires_cli_workload(report["scope"]) or is_direct_sdk_git_scope(report["scope"])
                                   or is_direct_sdk_hydration_scope(report["scope"])
                                   or is_direct_service_scope(report["scope"])):
            rollback = raw
            continue
        decoded = decode(raw)
        if name == "cleanup":
            cleanup_at = verify_cleanup_trace(decoded, report, edge, access)
            cleanup_targets = decoded["targets"]
            continue
        if name == "deployment_policy":
            verify_deployment_policy_trace(decoded, report, edge, access, deployment_policy_sha256)
            policy = decoded
            continue
        if name == KEY_ROTATION_CHECK:
            rotation_key = verify_key_rotation(decoded, report, edge, deployment_policy_sha256)
            continue
        if name == CACHE_REUSE_CHECK:
            cache_reuse_key = verify_cache_reuse(decoded, report, registration["cache_reuse_sha256"])
            continue
        if name == SCOPED_VIEW_CHECK:
            verify_scoped_view(decoded, report, registration["scoped_view_sha256"])
            scoped_placement = decoded["placement"]
            continue
        if name == MANAGED_ISSUER_CHECK:
            managed_issuer = decoded
            continue
        if name == HISTORICAL_ORIGIN_CHECK:
            history = decoded
            continue
        if name == PUBLIC_TRANSITION_CHECK:
            transition = decoded
            continue
        trace_fields = {"schema", "source_sha", "scope", "configuration_sha256",
                        "workflow_run_id", "workflow_run_attempt", "check", "requests"}
        if name == EXPIRY_RANGE_CHECK:
            trace_fields.add("viewer_expires_at_unix")
        trace = fields(decoded, trace_fields, name)
        require(trace["schema"] == "crab.cdn-trace/v1" and trace["check"] == name and all(
            trace[key] == report[key] for key in (
                "source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt",
            )
        ), f"trace provenance mismatch: {name}")
        requests = trace["requests"]
        require(isinstance(requests, list) and requests, f"trace has no observations: {name}")
        request_fields = TRACE_REQUEST_FIELDS.copy()
        if name == "edge_route_policy":
            request_fields.add("probe")
        if name == PUBLIC_EXPORT_CHECK:
            request_fields.add("body_sha256")
        if name == EXPIRY_RANGE_CHECK:
            request_fields.update(EXPIRY_RANGE_FIELDS)
        for value in requests:
            request = fields(value, request_fields, f"{name} request")
            require(request["source"] in ("edge", "origin")
                    and request["cache"] in ("hit", "miss", "unknown")
                    and type(request["status"]) is int and 100 <= request["status"] <= 599
                    and type(request["bytes"]) is int and request["bytes"] >= 0
                    and type(request["elapsed_ms"]) is int and request["elapsed_ms"] >= 0,
                    f"invalid trace observation: {name}")
            require(matches(SHA256, request["key_sha256"]), f"invalid trace key fingerprint: {name}")
        verify_trace_observations(name, requests)
        if name == EXPIRY_RANGE_CHECK:
            verify_expiry_range_window(requests, trace["viewer_expires_at_unix"])
        if name in fixture_checks:
            fixture_reads[name] = requests
    if profile == "managed":
        verify_managed_issuer(managed_issuer, report, registration, policy)
    full_sizes = {}
    for row in fixture_reads["exact_get"]:
        if row["source"] == "edge" and row["status"] == 200:
            key = row["key_sha256"]
            full_sizes[key] = max(full_sizes.get(key, 0), row["bytes"])
    require(any(row["source"] == "edge" and row["status"] == 206
                and 0 < row["bytes"] < full_sizes.get(row["key_sha256"], 0)
                for row in fixture_reads["exact_range"]),
            "exact range and full GET must observe the same edge object")
    verify_edge_route_policy(fixture_reads["edge_route_policy"], edge, access, full_sizes)
    if access == "private":
        key, size = rotation_key
        require(full_sizes.get(key) == size, "rotation must probe the same complete object verified by exact GET")
        key, size = cache_reuse_key
        require(full_sizes.get(key) == size, "authenticated cache reuse must probe the same complete object verified by exact GET")
        expired = fixture_reads["warm_expired_viewer_denied"][0]
        require(fixture_reads[EXPIRY_RANGE_CHECK][0]["key_sha256"] == expired["key_sha256"],
                "expired parallel ranges must probe the same warm object")
        for name in sorted(WARM_DENIAL_CHECKS):
            warm = fixture_reads[name][0]
            require(full_sizes.get(warm["key_sha256"]) == warm["bytes"],
                    f"warm denial must probe the complete object verified by exact GET: {name}")
    native_reads = {row["key_sha256"] for row in fixture_reads["native_origin_read"]
                    if row["source"] == "origin" and row["status"] == 200
                    and row["cache"] == "unknown"
                    and row["bytes"] == full_sizes.get(row["key_sha256"], 0) > 0}
    require(any(row["key_sha256"] in native_reads for row in fixture_reads[origin_denial]),
            "anonymous origin denial needs a verified native read of the same complete edge object")
    if is_direct_sdk_hydration_scope(report["scope"]):
        target = scoped_placement["views"]["target"]
        require(scoped_placement["fixture_sha256"] == report["configuration_sha256"]
                and scoped_placement["deployment_policy_sha256"] == deployment_policy_sha256
                and target["key_sha256"] in native_reads and full_sizes[target["key_sha256"]] == target["bytes"],
                "SDK hydration needs independently verified transport and scope evidence for the same xorb")
    if access == "public":
        export = fixture_reads["export_origin_access"]
        if edge == "cloudfront":
            require(all(anonymous_origin_denial(row) for row in export),
                    "the S3 export must deny anonymous object reads")
        else:
            require(all(row["source"] == "origin" and row["status"] == 200 and row["cache"] == "unknown"
                        and row["bytes"] == full_sizes.get(row["key_sha256"], 0) > 0 for row in export),
                    "the public export must serve the complete approved object")
        require(any(row["key_sha256"] in native_reads for row in export),
                "export access must use the same object key as the live source")
        verify_public_export(directory, report, registration, policy, fixture_reads, native_reads, cleanup_targets,
                             history, transition)
    return len(checks), cleanup_at, cleanup_targets["native"]["origin_sha256"], rollback


def anonymous_origin_denial(row: dict[str, Any]) -> bool:
    return row["source"] == "origin" and row["status"] in (401, 403) and row["cache"] == "unknown" and row["bytes"] == 0


def edge_head_denials(edge: str, access: str) -> tuple[int, ...]:
    # API Management can reject HEAD at operation matching, before policy evaluation.
    return (401, 403, 404, 405) if (edge, access) == ("front-door", "private") else (401, 403, 405)


def edge_route_probe_names(edge: str) -> list[str]:
    return ["warm_before", "head", *EDGE_ROUTE_QUERY_PROBES[edge], "warm_after"]


def verify_edge_route_policy(
    requests: list[dict[str, Any]], edge: str, access: str, full_sizes: dict[str, int],
) -> None:
    require([row["probe"] for row in requests] == edge_route_probe_names(edge), "edge route policy trace lacks required probes")
    first, method, *queries, last = requests
    key = first["key_sha256"]
    require(all(row["source"] == "edge" and row["key_sha256"] == key for row in requests)
            and all(row["status"] == 200 and row["cache"] == "hit"
                    and 0 < row["bytes"] == full_sizes.get(key) for row in (first, last)),
            "edge route policy requires same-object cache hits before and after denials")
    require(method["status"] in edge_head_denials(edge, access) and method["bytes"] == 0,
            "edge route policy must deny HEAD on the approved object")
    require(all(row["status"] in (401, 403) and row["bytes"] == 0 for row in queries),
            "edge route policy must deny version, snapshot, and response override queries")


def verify_expiry_range_window(requests: list[dict[str, Any]], expires_at: int) -> None:
    require(positive(expires_at) and len(requests) == 4
            and all(type(row["range_offset"]) is int for row in requests)
            and [row["range_offset"] for row in requests] == [0, 1, 0, 1],
            "parallel expiry needs two distinct byte ranges on each side of expiry")
    start, end = "started_at_unix_us", "finished_at_unix_us"
    require(all(row["source"] == "edge" and row["key_sha256"] == requests[0]["key_sha256"]
                and positive(row[start]) and positive(row[end]) and row[start] < row[end] for row in requests),
            "parallel expiry ranges need valid intervals on the same edge object")
    before, after = requests[:2], requests[2:]
    require(all(max(row[start] for row in group) < min(row[end] for row in group) for group in (before, after)),
            "parallel expiry range requests must overlap in each phase")
    expiration = expires_at * 1_000_000
    require(all(row[end] < expiration and row["status"] == 206 and row["bytes"] == 1 for row in before),
            "parallel ranges must return the approved bytes before the viewer grant expires")
    settled = (expires_at + EXPIRY_SETTLE_SECONDS) * 1_000_000
    require(all(row[start] >= settled and row["status"] in (401, 403) and row["bytes"] == 0
                for row in after), "parallel ranges must be denied after the viewer grant expires")


def verify_trace_observations(name: str, requests: list[dict[str, Any]]) -> None:
    successful = {"exact_get": ("edge", 200), "exact_range": ("edge", 206),
                  "native_origin_read": ("origin", 200), "origin_only_control": ("origin", 200)}
    if name in successful:
        require(any((row["source"], row["status"]) == successful[name] and row["bytes"] > 0
                    for row in requests), f"positive read evidence missing: {name}")
    if name == "origin_only_control":
        require(all(row["source"] == "origin" for row in requests),
                "origin-only control observed edge traffic")
    if name == "rollback":
        require(ordered_full_reads(requests, ("edge", None), ("origin", None)),
                "rollback requires an edge read followed by a native read of the same complete object")
    if name in {"private_origin_denied", "live_origin_denied", "public_listing_denied"}:
        require(all(anonymous_origin_denial(row) for row in requests), f"authorization denial evidence missing: {name}")
    if name in {"mutable_route_denied", "neighbor_route_denied"}:
        require(any(row["source"] == "edge" and row["status"] in (401, 403) for row in requests),
                f"authorization denial evidence missing: {name}")
    if name in WARM_DENIAL_CHECKS:
        verify_warm_denial(name, requests)


def verify_warm_denial(name: str, requests: list[dict[str, Any]]) -> None:
    if name == "warm_unrelated_principal_denied":
        verify_unrelated_viewer_denial(requests)
        return
    anonymous = name == "warm_anonymous_viewer_denied"
    require(len(requests) == (4 if anonymous else 2), f"authorized warm hit evidence missing before same-key denial: {name}")
    warm = requests[0]
    require(warm["status"] == 200 and warm["cache"] == "hit" and warm["bytes"] > 0
            and all(row["source"] == "edge" and row["key_sha256"] == warm["key_sha256"] for row in requests),
            f"authorized warm hit evidence missing before same-key denial: {name}")
    denials = requests[1:3] if anonymous else requests[1:]
    require(all(row["status"] in (401, 403) and row["bytes"] == 0 for row in denials),
            f"authorization denial evidence missing: {name}")
    if anonymous:
        after = requests[3]
        require(after["status"] == 200 and after["cache"] == "hit" and after["bytes"] == warm["bytes"],
                "authorized warm hit evidence missing after anonymous denial")


def verify_unrelated_viewer_denial(requests: list[dict[str, Any]]) -> None:
    require(len(requests) == 6 and all(row["source"] == "edge" for row in requests),
            "unrelated viewer denial requires six edge observations")
    warm, control_before, denied, denied_range, control_after, confirmed = requests
    key, size = warm["key_sha256"], warm["bytes"]
    require(all(row["status"] == 200 and row["cache"] == "hit" and row["bytes"] == size > 0
                and row["key_sha256"] == key for row in (warm, confirmed)),
            "unrelated viewer denial requires full cache hits of the same target")
    require(all(row["status"] in (401, 403) and row["bytes"] == 0 and row["key_sha256"] == key
                for row in (denied, denied_range)),
            "unrelated viewer must be denied a full and ranged GET of the warm target")
    control_key, control_size = control_before["key_sha256"], control_before["bytes"]
    require(control_key != key and all(row["status"] == 200 and row["bytes"] == control_size > 0
                                      and row["key_sha256"] == control_key for row in (control_before, control_after)),
            "unrelated viewer must read the same distinct control object before and after denial")


def verify_scoped_view(value: Any, report: dict[str, Any], approved: str) -> None:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "check", "placement", "requests"}, SCOPED_VIEW_CHECK)
    require(trace["schema"] == "crab.cdn-scoped-view/v1" and trace["check"] == SCOPED_VIEW_CHECK
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS),
            "scoped view evidence must match the qualification row and workflow attempt")
    placement = fields(trace["placement"], {"fixture_sha256", "origin_sha256", "source_prefix_sha256",
                                            "deployment_policy_sha256", "views"}, "scoped view placement")
    require(all(matches(SHA256, placement[key]) for key in placement.keys() - {"views"})
            and deployment_component_digest(placement) == approved,
            "scoped view placement differs from the approved control fixture")
    views = fields(placement["views"], SCOPED_VIEW_ROLES, "approved scoped views")
    for role, view in views.items():
        fields(view, SCOPED_VIEW_FIELDS, f"scoped view {role}")
        require(all(matches(SHA256, view[key]) for key in SCOPED_VIEW_FIELDS - {"bytes"})
                and positive(view["bytes"]) and 2 <= view["bytes"] <= 1024 * 1024,
                "scoped view requires the exact physical view, immutable prefix, and expected bytes")
    target, sibling = views["target"], views["sibling"]
    require(all(target[key] != sibling[key] for key in ("generation_sha256", "physical_prefix_sha256",
                                                       "grant_prefix_sha256", "key_sha256")),
            "scoped view control must address a distinct materialized view")
    requests = trace["requests"]
    require(isinstance(requests, list) and len(requests) == len(SCOPED_VIEW_PROBES),
            "scoped view isolation needs authorized reads around full and ranged denials")
    for name, request in zip(SCOPED_VIEW_PROBES, requests, strict=True):
        row = fields(request, TRACE_REQUEST_FIELDS | {"probe", "range_offset", "body_sha256"}, "scoped view probe")
        is_target = name.startswith("target_")
        denied = name in {"sibling_full", "sibling_range"}
        view = target if is_target else sibling
        require(row["probe"] == name and row["source"] == ("edge" if is_target or denied else "origin")
                and row["key_sha256"] == view["key_sha256"]
                and row["cache"] in ("hit", "miss", "unknown")
                and type(row["elapsed_ms"]) is int and row["elapsed_ms"] >= 0
                and type(row["status"]) is int and type(row["bytes"]) is int,
                "scoped view probes must use the approved target and sibling keys in order")
        if denied:
            offset = row["range_offset"]
            require(row["status"] in (401, 403) and row["bytes"] == 0 and row["body_sha256"] is None
                    and (type(offset) is int and offset == 0 if name == "sibling_range" else offset is None),
                    "the scoped viewer must be denied a full and ranged GET of the sibling")
        else:
            require(row["status"] == 200 and row["cache"] == ("hit" if is_target else "unknown")
                    and row["bytes"] == view["bytes"] and row["body_sha256"] == view["body_sha256"]
                    and row["range_offset"] is None,
                    "scoped view needs verified warm target and authorized sibling reads before and after denial")


def verify_cache_reuse(value: Any, report: dict[str, Any], approved: str) -> tuple[str, int]:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "check", "authority", "grants", "requests"}, CACHE_REUSE_CHECK)
    require(trace["schema"] == "crab.cdn-authenticated-cache-reuse/v1" and trace["check"] == CACHE_REUSE_CHECK
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS),
            "authenticated cache reuse must match the qualification row and workflow attempt")
    authority = fields(trace["authority"], {"issuer_endpoint_sha256", "approval_sha256", "principals", "object"},
                       "cache reuse authority")
    require(matches(SHA256, authority["issuer_endpoint_sha256"]) and matches(SHA256, authority["approval_sha256"])
            and deployment_component_digest(authority) == approved,
            "cache reuse authority differs from the approved issuer, principals, or object")
    roles = set(CACHE_REUSE_ROLES)
    principals = fields(authority["principals"], roles, "approved cache reuse principals")
    for role, principal in principals.items():
        fields(principal, {"subject_sha256", "policy_identity_sha256"}, f"cache reuse principal {role}")
        require(all(matches(SHA256, digest) for digest in principal.values()), "invalid cache reuse principal fingerprint")
    require(all(principals["primary"][key] != principals["second"][key] for key in principals["primary"]),
            "cache reuse requires two distinct OIDC subjects and Crab Auth policy identities")
    obj = fields(authority["object"], {"key_sha256", "body_sha256", "bytes"}, "cache reuse object")
    require(matches(SHA256, obj["key_sha256"]) and matches(SHA256, obj["body_sha256"])
            and positive(obj["bytes"]) and 2 <= obj["bytes"] <= 1024 * 1024, "invalid cache reuse object descriptor")
    grants = fields(trace["grants"], roles, "authenticated cache reuse grants")
    requests = trace["requests"]
    require(isinstance(requests, list) and len(requests) == 2, "cache reuse requires two authenticated reads")
    previous_finish = 0
    for role, cache, value in zip(CACHE_REUSE_ROLES, ("miss", "hit"), requests, strict=True):
        grant = fields(grants[role], {"subject_sha256", "policy_identity_sha256", "issued_at_unix_ms", "expires_at_unix"},
                       "authenticated cache reuse grant")
        require(all(grant[key] == principals[role][key] for key in principals[role])
                and positive(grant["issued_at_unix_ms"]) and positive(grant["expires_at_unix"]),
                "cache reuse requires an authenticated grant for each approved principal")
        row = fields(value, TRACE_REQUEST_FIELDS | {"principal", "body_sha256", "started_at_unix_ms", "finished_at_unix_ms"},
                     "authenticated cache reuse read")
        require(row["principal"] == role and row["source"] == "edge" and type(row["status"]) is int
                and row["status"] == 200 and row["cache"] == cache
                and all(row[key] == obj[key] for key in obj),
                "cache reuse requires a verified first miss and second hit for the same complete object")
        start, finish, expires = row["started_at_unix_ms"], row["finished_at_unix_ms"], grant["expires_at_unix"] * 1000
        require(type(row["elapsed_ms"]) is int and row["elapsed_ms"] >= 0 and type(row["bytes"]) is int
                and positive(start) and positive(finish)
                and previous_finish <= start and grant["issued_at_unix_ms"] <= start <= finish < expires
                and expires <= grant["issued_at_unix_ms"] + MAX_VIEWER_GRANT_SECONDS * 1000,
                "cache reuse reads must be ordered and complete within the corresponding signed grant lifetime")
        previous_finish = finish
    return obj["key_sha256"], obj["bytes"]


def ordered_full_reads(
    requests: list[dict[str, Any]], before: tuple[str, str | None], after: tuple[str, str | None],
) -> bool:
    previous = set()
    for row in requests:
        if row["status"] != 200 or row["bytes"] == 0:
            continue
        identity = (row["key_sha256"], row["bytes"])
        source, cache = row["source"], row["cache"]
        if identity in previous and source == after[0] and (after[1] is None or cache == after[1]):
            return True
        if source == before[0] and (before[1] is None or cache == before[1]):
            previous.add(identity)
    return False


def verify_product_action(value: Any, family: str, *, sdk_hydration: bool = False) -> None:
    required = {
        "name", "expected_sha256", "observed_sha256", "expected_bytes", "observed_bytes",
        "native_writes", "observed_edge_gets", "visible_new_ref", "fixture_cleanup_verified",
    }
    verified_payload_required = family in {"git", "capsule", "lfs"} or sdk_hydration
    if verified_payload_required:
        required.add("verified_edge_payloads")
    action = fields(value, required, "product action")
    require(matches(LABEL, action["name"]), "invalid product action name")
    require(
        matches(SHA256, action["expected_sha256"])
        and action["expected_sha256"] == action["observed_sha256"]
        and positive(action["expected_bytes"]) and positive(action["observed_bytes"])
        and action["expected_bytes"] == action["observed_bytes"],
        "product action did not reconstruct the expected bytes",
    )
    require(positive(action["native_writes"]) and positive(action["observed_edge_gets"]),
            "product action must prove a native write and a CDN GET")
    if verified_payload_required:
        require(positive(action["verified_edge_payloads"]), "product action accepted no verified edge payload")
    require(action["visible_new_ref"] is True and action["fixture_cleanup_verified"] is True,
            "product action must prove immediate ref visibility and fixture cleanup")


def median_savings_lower_bound(savings: list[int]) -> int:
    # Exact sign interval for paired median savings. Use the largest rank
    # whose lower tail has probability <= 2.5% for a two-sided 95% interval.
    ordered = sorted(savings)
    count = len(savings)
    tail = 0
    rank = 0
    for index in range(count // 2):
        tail += math.comb(count, index)
        if tail * 40 > 2 ** count:
            break
        rank = index
    return ordered[rank]


def benchmark_registration(registration: Any) -> dict[str, Any]:
    expected = {
        "schema", "source_sha", "scope", "configuration_sha256", "workload", "samples", "p95_budget_ms",
        "cost_budget_microusd", "regions", "deployment_policy_sha256",
    }
    require(isinstance(registration, dict) and isinstance(registration.get("scope"), str), "benchmark needs a CDN scope")
    _, access, _, _, profile = scope_parts(registration["scope"])
    if requires_cli_workload(registration["scope"]):
        expected.update(("candidate", "viewer_binding_sha256"))
        verify_read_candidate(registration.get("candidate"), registration.get("source_sha"))
        require(matches(SHA256, registration.get("viewer_binding_sha256")),
                "CLI benchmark needs the approved viewer binding digest")
    if is_direct_sdk_git_scope(registration["scope"]):
        expected.update(("sdk_candidate", "viewer_binding_sha256"))
        verify_sdk_candidate(registration.get("sdk_candidate"), registration.get("source_sha"))
        require(registration.get("workload") == SDK_GIT_ACTION and matches(SHA256, registration.get("viewer_binding_sha256")),
                "SDK benchmark needs the snapshot workload and the approved viewer binding")
        if access == "private":
            expected.add("viewer_approval_sha256")
            require(matches(SHA256, registration.get("viewer_approval_sha256")),
                    "private SDK benchmark needs the approved issuer binding")
    if is_direct_sdk_hydration_scope(registration["scope"]):
        expected.update(("sdk_candidate", "viewer_binding_sha256", "viewer_approval_sha256",
                         "hydration_workload_sha256"))
        verify_sdk_candidate(registration.get("sdk_candidate"), registration.get("source_sha"))
        require(registration.get("workload") == SDK_HYDRATION_ACTION
                and all(matches(SHA256, registration.get(key)) for key in
                        ("viewer_binding_sha256", "viewer_approval_sha256", "hydration_workload_sha256")),
                "SDK hydration benchmark needs the approved workload and viewer binding")
    if is_direct_service_scope(registration["scope"]):
        expected.update(("service_workload", "viewer_binding_sha256", "service_candidate"))
        verify_service_candidate(registration.get("service_candidate"), registration.get("source_sha"))
        workload = fields(registration.get("service_workload"), SERVICE_WORKLOAD_REGISTRATION_FIELDS,
                          "registered service HTTP workload")
        require(registration.get("workload") == SERVICE_READ_ACTION
                and matches(SHA256, registration.get("viewer_binding_sha256"))
                and all(matches(SHA256, workload[key]) for key in SERVICE_WORKLOAD_REGISTRATION_FIELDS - {"payload_bytes"})
                and positive(workload["payload_bytes"]) and workload["payload_bytes"] <= SERVICE_PAYLOAD_LIMIT,
                "service benchmark needs the approved HTTP workload and viewer binding")
    if access == "public":
        expected |= {"publication_sha256", "previous_publication_sha256", "historical_origin_sha256"}
        require(matches(SHA256, registration.get("publication_sha256")),
                "public benchmark needs the approved publication receipt digest")
        require(matches(SHA256, registration.get("previous_publication_sha256"))
                and registration["previous_publication_sha256"] != registration["publication_sha256"],
                "public benchmark needs the distinct prior publication approval")
        require(matches(SHA256, registration.get("historical_origin_sha256")),
                "public benchmark needs approved native history")
    else:
        expected.update(("scoped_view_sha256", "cache_reuse_sha256"))
        require(matches(SHA256, registration.get("scoped_view_sha256")),
                "private benchmark needs the approved scoped view control digest")
        require(matches(SHA256, registration.get("cache_reuse_sha256")),
                "private benchmark needs the approved cache reuse authority digest")
    if profile == "managed":
        expected.add("managed_issuer")
        issuer = fields(registration.get("managed_issuer"), MANAGED_ISSUER_FIELDS, "registered production issuer")
        require(matches(SHA256, issuer["identity_sha256"]) and matches(LABEL, issuer["version"])
                and matches(SHA256, issuer["approval_sha256"]),
                "managed benchmark needs an approved issuer and binding receipt")
    spec = fields(registration, expected, "benchmark registration")
    require(spec["schema"] == "crab.cdn-benchmark/v1" and matches(LABEL, spec["workload"]),
            "invalid benchmark registration")
    require(matches(SHA256, spec["deployment_policy_sha256"]), "benchmark registration requires the approved deployment policy digest")
    sample_count = spec["samples"]
    require(type(sample_count) is int and 30 <= sample_count <= 1000
            and positive(spec["p95_budget_ms"]) and positive(spec["cost_budget_microusd"]),
            "invalid preregistered sample size or budgets")
    regions = fields(spec["regions"], {"near", "distant"}, "preregistered client regions")
    require(all(matches(LABEL, region) for region in regions.values()) and regions["near"] != regions["distant"],
            "benchmark requires distinct preregistered near and distant regions")
    return spec


def verify_performance(value: Any, registration: Any) -> int:
    performance = fields(value, {"near", "distant"}, "performance")
    spec = benchmark_registration(registration)
    sample_count = spec["samples"]
    for location, value in performance.items():
        region = fields(value, {"region", "pairs"}, location)
        require(region["region"] == spec["regions"][location], f"client region differs from registration: {location}")
        pairs = region["pairs"]
        require(isinstance(pairs, list) and len(pairs) == sample_count,
                f"sample count differs from registration: {location}")
        for pair in pairs:
            arms = fields(pair, set(ARMS), "benchmark pair")
            for name, value in arms.items():
                arm = fields(value, METRICS, name)
                require(all(type(n) is int and n >= 0 for n in arm.values()) and arm["elapsed_ms"] > 0,
                        f"invalid benchmark metrics: {name}")
            require(arms["origin"]["origin_gets"] > 0 and all(
                arms["origin"][metric] == 0 for metric in ("edge_hits", "edge_misses", "edge_unknown", "edge_bytes")
            ), "origin-only sample must prove an origin read")
            require(arms["cold_edge"]["edge_misses"] > 0 and arms["cold_edge"]["origin_fill_bytes"] > 0
                    and arms["cold_edge"]["edge_bytes"] > 0,
                    "cold edge sample must prove a miss, origin fill and delivered bytes")
            require(arms["warm_edge"]["edge_hits"] > 0 and arms["warm_edge"]["edge_bytes"] > 0,
                    "warm edge sample must prove a hit and delivered bytes")
            require(arms["warm_local"]["local_payload_hits"] > 0 and
                    all(arms["warm_local"][metric] == 0
                        for metric in ("edge_hits", "edge_misses", "edge_unknown", "edge_bytes")),
                    "warm local sample must prove a local payload hit without edge payload bytes")
            require(arms["warm_edge"]["cost_microusd"] <= spec["cost_budget_microusd"],
                    f"warm edge exceeds the registered cost budget: {location}")
        warm = sorted(pair["warm_edge"]["elapsed_ms"] for pair in pairs)
        p95_index = (sample_count * 95 + 99) // 100 - 1
        require(warm[p95_index] <= spec["p95_budget_ms"],
                f"warm edge p95 exceeds the registered budget: {location}")
    savings = [pair["origin"]["elapsed_ms"] - pair["warm_edge"]["elapsed_ms"]
               for pair in performance["distant"]["pairs"]]
    lower = median_savings_lower_bound(savings)
    require(lower > 0, "distant warm edge did not show positive paired median savings")
    return lower


def requires_cli_lfs_workload(scope: str) -> bool:
    _, _, consumer, family, profile = scope_parts(scope)
    return consumer == "cli" and family == "lfs" and profile == "direct"


def requires_cli_workload(scope: str) -> bool:
    _, _, consumer, family, profile = scope_parts(scope)
    return consumer == "cli" and family in {"git", "capsule", "lfs"} and profile == "direct"


def is_direct_sdk_git_scope(scope: str) -> bool:
    return scope_parts(scope)[2:] == ("sdk", "git", "direct")


def is_direct_sdk_hydration_scope(scope: str) -> bool:
    return scope_parts(scope)[1:] == ("private", "sdk", "xorb", "direct")


def is_direct_service_scope(scope: str) -> bool:
    _, _, consumer, family, profile = scope_parts(scope)
    return consumer in ("server", "s3-gateway") and family in ("git", "lfs") and profile == "direct"


def is_direct_service_git_scope(scope: str) -> bool:
    return is_direct_service_scope(scope) and scope_parts(scope)[3] == "git"


def cli_action_name(scope: str) -> str:
    return "lfs-fetch" if requires_cli_lfs_workload(scope) else "git-fetch"


def lfs_object_path(digest: str) -> str:
    return f"lfs/objects/{digest[:2]}/{digest[2:4]}/{digest}"


def verify_read_candidate(
    value: Any, source: str, platform: str | None = None, *, policy: Path | None = None, consumer: str = "CLI",
) -> dict[str, Any]:
    candidate = fields(value, CLI_CANDIDATE_FIELDS, f"{consumer} build identity")
    triple = candidate["target_triple"]
    raw_features = candidate["cargo_features"]
    require(isinstance(raw_features, str), f"{consumer} build features must be recorded")
    features = raw_features.split(",") if raw_features else []
    require(matches(COMMIT, source) and candidate["source_sha"] == source
            and matches(SHA256, candidate["binary_sha256"]) and matches(SHA256, candidate["release_policy_sha256"])
            and matches(CLI_TARGET, triple) and candidate["cargo_profile"] in ("debug", "release")
            and type(candidate["qualification_feature"]) is bool
            and all(matches(LABEL, feature) for feature in features) and features == sorted(set(features))
            and ("provider-cdn-qualification" not in features or candidate["qualification_feature"]),
            f"{consumer} build identity is invalid or differs from the approved source")
    arch, system = triple.split("-", 1)
    os_label = {"apple": "darwin", "unknown": "linux", "pc": "windows"}[system.split("-", 1)[0]]
    if platform is not None:
        require(platform == f"{os_label}-{arch}", f"{consumer} build target differs from the approved platform")
    if policy is not None:
        require(candidate["qualification_feature"] is False and candidate["cargo_profile"] == "release",
                "CDN promotion requires the ordinary release build")
        require(system in {"apple-darwin", "unknown-linux-gnu", "pc-windows-msvc"},
                f"CDN promotion requires the packaged {consumer} target triple")
        compiled_policy = hashlib.sha256(policy.read_bytes().replace(b"\r\n", b"\n")).hexdigest()
        require(candidate["release_policy_sha256"] == compiled_policy,
                f"{consumer} build did not embed the release checkout's CDN policy")
    return candidate


def verify_sdk_candidate(
    value: Any, source: str, platform: str | None = None, *, policy: Path | None = None,
) -> dict[str, Any]:
    candidate = verify_read_candidate(value, source, platform, policy=policy, consumer="SDK")
    require("service" in candidate["cargo_features"].split(","), "SDK snapshot probe needs the service feature")
    return candidate


def verify_service_candidate(
    value: Any, source: str, platform: str | None = None, *, policy: Path | None = None,
) -> dict[str, Any]:
    candidate = verify_read_candidate(value, source, platform, policy=policy, consumer="service")
    require("provider-cdn" in candidate["cargo_features"].split(","),
            "service candidate must compile the provider-cdn feature")
    return candidate


def read_git_record(path: Path, *, limit: int = 8 * 1024) -> bytes:
    with path.open("rb") as record:
        raw = record.read(limit + 1)
    require(len(raw) <= limit, f"Git evidence record is too large: {path.name}")
    return raw


def validate_git_publication(raw: bytes, provenance: dict[str, Any]) -> dict[str, Any]:
    record = fields(decode(raw), GIT_PUBLICATION_FIELDS, "Git publication receipt")
    prefix = f"refs/heads/crab-cdn-qualification-{provenance['workflow_run_id']}-{provenance['workflow_run_attempt']}-"
    require(record["schema"] == "crab.cdn-git-publication/v1"
            and all(record[key] == provenance[key] for key in CDN_PROVENANCE_FIELDS)
            and matches(GIT_TEST_REF, record["ref"]) and record["ref"].startswith(prefix)
            and matches(GIT_OID, record["oid"]) and matches(SHA256, record["sha256"])
            and type(record["published"]) is bool
            and (positive(record["visible_at_unix_ms"]) if record["published"] else record["visible_at_unix_ms"] is None),
            "Git publication receipt does not match this qualification run")
    verify_read_candidate(record["candidate"], provenance["source_sha"], provenance.get("platform"))
    return record


def validate_git_fetch(raw: bytes, provenance: dict[str, Any], mode: str, label: str) -> dict[str, Any]:
    action = cli_action_name(provenance["scope"])
    client = fields(decode(raw), GIT_FETCH_FIELDS, "redacted LFS fetch" if action == "lfs-fetch" else "redacted Git fetch")
    require(client["schema"] == f"crab.cdn-{action}/v1" and client["name"] == action
            and client["arm"] == mode and all(client[key] == value for key, value in provenance.items() if key != "candidate"),
            f"{label} has mismatched Git fetch provenance")
    verify_read_candidate(client["candidate"], provenance["source_sha"], client["platform"])
    reader = fields(client["reader"], CLI_READ_FIELDS, "verified CLI read configuration")
    require(all(matches(SHA256, reader[key]) for key in CLI_NATIVE_FIELDS | {"cache_root_sha256"})
            and (matches(SHA256, reader["binding_sha256"])
                 or (mode == "origin" and reader["binding_sha256"] == "")),
            f"{label} has an invalid CDN binding, native target or local cache fingerprint")
    edge, access, _, _, _ = scope_parts(provenance["scope"])
    require(matches(SHA256, reader["public_export_sha256"]) if mode != "origin" and access == "public"
            else reader["public_export_sha256"] == "",
            f"{label} has an invalid public export approval")
    if "candidate" in provenance:
        require(client["candidate"] == provenance["candidate"], f"{label} did not run the registered CLI build")
    require(matches(GIT_OID, client["publication_oid"]) and matches(SHA256, client["publication_sha256"])
            and matches(SHA256, client["expected_sha256"]) and client["expected_sha256"] == client["observed_sha256"]
            and client["expected_bytes"] == client["observed_bytes"] == GIT_PAYLOAD_SIZE
            and positive(client["native_writes"]) and client["visible_new_ref"] is True,
            f"{label} did not verify the published payload")
    verify_git_traffic(client, edge, label)
    native = client["native_traffic"]
    if mode == "origin":
        require(reader["binding_sha256"] == "" and all(client[key] == 0 for key in PRODUCT_COUNTERS),
                f"{label} requires an inactive viewer binding and zero edge or local payload reads")
        require(positive(native["get_responses"]) and positive(native["body_bytes"]),
                f"{label} observed no native payload transfer")
    if mode == "edge":
        require(positive(client["observed_edge_gets"]) and positive(client["verified_edge_payloads"]),
                f"{label} accepted no verified edge payload")
    if mode == "warm_local":
        require(positive(client["local_payload_hits"]) and client["edge_attempts"] == client["verified_edge_payloads"] == 0
                and (action != "lfs-fetch" or client["all_edge_attempts"] == 0),
                f"{label} requires a verified local payload without an edge read for this scope")
    return client


def verify_git_traffic(client: dict[str, Any], edge: str, label: str) -> None:
    start, finish = client["started_at_unix_ms"], client["finished_at_unix_ms"]
    require(positive(start) and positive(finish) and finish >= start and positive(client["elapsed_ms"]),
            f"{label} has an invalid measurement interval")
    require(all(type(client[key]) is int and client[key] >= 0 for key in PRODUCT_COUNTERS),
            f"{label} has invalid Git transfer counters")
    native = fields(client["native_traffic"], NATIVE_TRAFFIC_FIELDS, "native object-store traffic")
    require(all(type(count) is int and count >= 0 for count in native.values())
            and (native["body_bytes"] == 0 or native["get_responses"] + native["head_responses"] > 0),
            f"{label} has invalid native object-store counters")
    responses = client["edge_responses"]
    provider = PROVIDER_FOR_EDGE[edge]
    require(isinstance(responses, dict) and len(responses) <= min(1024, client["all_edge_attempts"])
            and (not responses or provider in VIEWER_REQUEST_FILES),
            f"{label} has invalid CDN viewer response evidence")
    for request_id, evidence in responses.items():
        response = fields(evidence, {"status", "cache", "bytes", "complete"}, "CDN viewer response")
        require(matches(SHA256, request_id) and type(response["status"]) is int and 100 <= response["status"] <= 599
                and response["cache"] in ("hit", "miss", "unknown") and type(response["complete"]) is bool
                and type(response["bytes"]) is int and response["bytes"] >= 0,
                f"{label} has an invalid CDN viewer response")
    require(client["edge_hits"] + client["edge_misses"] + client["edge_unknown"] == client["observed_edge_gets"]
            <= client["edge_attempts"] <= client["all_edge_attempts"], f"{label} has inconsistent edge counts")


def validate_service_read(value: Any, provenance: dict[str, Any], arm: str) -> dict[str, Any]:
    local = arm == "warm_local"
    client = fields(value, SERVICE_READ_FIELDS | ({"primer_sha256"} if local else set()), "CDN service read")
    edge, access, _, family, _ = scope_parts(provenance["scope"])
    require(is_direct_service_scope(provenance["scope"]) and (arm in ("edge", "origin") or (local and family == "git"))
            and client["schema"] == "crab.cdn-service-read/v1" and client["arm"] == arm
            and all(client[key] == provenance[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "CDN service read has mismatched workflow or product provenance")
    verify_service_candidate(client["candidate"], provenance["source_sha"], provenance["platform"])
    require(matches(GIT_OID, client["commit"]) and positive(client["payload_bytes"])
            and client["payload_bytes"] <= SERVICE_PAYLOAD_LIMIT
            and all(matches(SHA256, client[key]) for key in ("workload_sha256", "publication_sha256",
                    "native_published_sha256", "payload_sha256", "etag_sha256", "service_instance_sha256")),
            "CDN service read has invalid publication or payload fingerprints")
    git, lfs = client["git_blob_oid_sha256"], client["lfs_key_sha256"]
    require((matches(SHA256, git) and lfs is None) if family == "git" else (matches(SHA256, lfs) and git is None),
            "CDN service read needs the exact Git blob or LFS object fingerprint")
    reader = fields(client["reader"], DIRECT_READ_TARGET_FIELDS, "CDN service read target")
    require(all(matches(SHA256, reader[key]) for key in CLI_NATIVE_FIELDS)
            and (matches(SHA256, reader["binding_sha256"]) if arm != "origin" else reader["binding_sha256"] == "")
            and (matches(SHA256, reader["public_export_sha256"]) if arm != "origin" and access == "public"
                 else reader["public_export_sha256"] == ""),
            "CDN service read has an invalid native target, viewer binding or public receipt")
    verify_git_traffic(client, edge, "CDN service read")
    if local:
        require(matches(SHA256, client["primer_sha256"]) and positive(client["local_payload_hits"])
                and all(client[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "service warm-local read requires a verified cached blob without edge traffic")
    elif arm == "origin":
        native = client["native_traffic"]
        require(all(client[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "service rollback requires native body reads and zero edge attempts")
    else:
        require(all(positive(client[key]) for key in ("observed_edge_gets", "edge_bytes", "verified_edge_payloads"))
                and client["local_payload_hits"] == 0
                and client["edge_attempts"] == client["all_edge_attempts"] == len(client["edge_responses"]),
                "service read requires verified edge delivery and a fingerprint for every viewer attempt")
    return client


def verify_service_repeat(local: dict[str, Any], primer: dict[str, Any], primer_sha256: str) -> None:
    require(local["primer_sha256"] == primer_sha256 and all(local[key] == primer[key] for key in
                ("candidate", "service_instance_sha256", "publication_sha256", "native_published_sha256", "workload_sha256",
                 "reader", "commit", "payload_sha256", "payload_bytes", "etag_sha256", "git_blob_oid_sha256"))
            and primer["finished_at_unix_ms"] <= local["started_at_unix_ms"],
            "service warm-local read must repeat its own edge primer in the same process")


def verify_sdk_git_read(raw: bytes, row: dict[str, Any], spec: dict[str, Any], mode: str, *,
                        worktree_action: str | None = None) -> dict[str, Any]:
    local = mode == "warm_local"
    prior = mode == "prior_export"
    expected_fields = SDK_GIT_READ_FIELDS | ({"primer_sha256"} if local else set())
    if worktree_action:
        expected_fields |= {"crab_sha256", "installed_packs", "verified_edge_packs"}
    if worktree_action == "fetch":
        expected_fields |= {"previous_head", "head_after_fetch"}
    edge, access, _, _, _ = scope_parts(row["scope"])
    value = decode(raw)
    transition = isinstance(value, dict) and "transition_key_sha256" in value
    if transition:
        expected_fields.add("transition_key_sha256")
    client = fields(value, expected_fields, "redacted SDK Git snapshot")
    require(not prior or transition, "SDK prior export needs the verified transition object")
    if transition:
        require(access == "public" and mode in ("edge", "prior_export") and matches(SHA256, client["transition_key_sha256"]),
                "SDK transition needs an exact public pack-layer fingerprint")
    schema = f"crab.cdn-sdk-git-{worktree_action or 'read'}/v1"
    require(client["schema"] == schema and client["arm"] == mode
            and all(client[key] == row[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "SDK snapshot has mismatched product or workflow provenance")
    verify_sdk_candidate(client["candidate"], row["source_sha"], row["platform"])
    require(client["candidate"] == spec["sdk_candidate"], "SDK snapshot did not run the registered SDK build")
    if worktree_action:
        require(mode in ("edge", "origin") and not transition
                and "local" in client["candidate"]["cargo_features"].split(",")
                and matches(SHA256, client["crab_sha256"]),
                "SDK worktree action needs the local build and an approved Crab executable")
        packs, edge_packs = client["installed_packs"], client["verified_edge_packs"]
        require(isinstance(packs, list) and isinstance(edge_packs, list) and 0 < len(packs) <= 1024
                and len(edge_packs) <= len(packs)
                and all(matches(SHA256, pack) for pack in packs + edge_packs)
                and sorted(set(packs)) == packs and sorted(set(edge_packs)) == edge_packs
                and set(edge_packs) <= set(packs) and bool(edge_packs) == (mode == "edge"),
                "SDK worktree action needs verified pack evidence for the checked-out blob")
    if worktree_action == "fetch":
        require(matches(GIT_OID, client["previous_head"])
                and client["head_after_fetch"] == client["previous_head"] != client["commit"],
                "SDK fetch did not preserve the local HEAD before checkout")
    require(matches(GIT_OID, client["commit"]) and all(matches(SHA256, client[key])
            for key in ("sha256", "publication_sha256", "git_blob_oid_sha256"))
            and type(client["bytes"]) is int and client["bytes"] == GIT_PAYLOAD_SIZE,
            "SDK snapshot did not verify the published payload")
    reader = fields(client["reader"], DIRECT_READ_TARGET_FIELDS, "verified SDK read target")
    require(all(matches(SHA256, reader[key]) for key in CLI_NATIVE_FIELDS),
            "SDK snapshot has invalid native origin or prefix fingerprints")
    expected_binding = spec["viewer_binding_sha256"] if mode != "origin" else ""
    expected_export = ""
    if mode != "origin" and access == "public":
        expected_export = spec["previous_publication_sha256" if prior else "publication_sha256"]
    expected_approval = spec["viewer_approval_sha256"] if mode != "origin" and access == "private" else expected_export
    require(reader["binding_sha256"] == expected_binding and reader["public_export_sha256"] == expected_export
            and client["approval_sha256"] == expected_approval,
            "SDK snapshot did not use the required viewer binding and approval")
    verify_git_traffic(client, edge, "SDK snapshot")
    if local:
        require(matches(SHA256, client["primer_sha256"]) and positive(client["local_payload_hits"])
                and all(client[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "SDK warm-local sample requires its primer and a verified cached blob without edge traffic")
    elif mode == "origin" or prior:
        native = client["native_traffic"]
        require(all(client[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "SDK native control requires native payload reads without edge or cached payload activity")
    else:
        require(positive(client["observed_edge_gets"]) and positive(client["verified_edge_payloads"])
                and positive(client["edge_bytes"]) and client["local_payload_hits"] == 0,
                "SDK snapshot requires a verified edge Git payload from a fresh client")
    return client


def verify_viewer_requests(value: Any, client: dict[str, Any], client_digest: str, edge: str) -> tuple[set[str], int, int]:
    keyed_origin = edge in ("cloudfront", "front-door")
    xorb_key = client.get("xorb_key_sha256")
    lfs_key = client.get("lfs_key_sha256")
    object_key = xorb_key or lfs_key
    service = client.get("schema") == "crab.cdn-service-read/v1"
    proof_fields = {"schema", "client_report_sha256", "records"}
    if keyed_origin:
        proof_fields.add("origin_requests")
    proof = fields(value, proof_fields, "CDN viewer request evidence")
    require(proof["schema"] == f"crab.{edge}-viewer-requests/v1" and proof["client_report_sha256"] == client_digest,
            "CDN viewer evidence differs from the measured fetch")
    expected = client["edge_responses"]
    records = proof["records"]
    require(isinstance(records, dict) and records.keys() == expected.keys()
            and len(records) == client["all_edge_attempts"],
            "CDN logs must cover each measured viewer attempt exactly once")
    record_fields = {"status", "cache", "response_bytes"}
    if edge == "cloud-cdn":
        record_fields |= {"cache_fill_bytes", "origin_requests"}
    if keyed_origin or object_key is not None:
        record_fields.add("key_sha256")
    viewer_keys = set()
    logged_ids = set(records)
    origin_gets = child_fill_bytes = viewer_fill_bytes = 0
    observed_transfer = dict.fromkeys(("edge_hits", "edge_misses", "edge_unknown", "edge_bytes", "incomplete_edge_bytes"), 0)
    for request_id, record in records.items():
        row = fields(record, record_fields, "CDN viewer log record")
        response = expected[request_id]
        require(type(row["status"]) is int and row["status"] == response["status"]
                and row["cache"] in ("hit", "miss", "unknown")
                and type(row["response_bytes"]) is int and row["response_bytes"] >= response["bytes"],
                "CDN log status or response bytes disagree with the client")
        if response["complete"] and response["cache"] != "unknown":
            require(row["cache"] == response["cache"], "CDN log cache result disagrees with the completed response")
        if keyed_origin or object_key is not None:
            require(matches(SHA256, row["key_sha256"]), "invalid CDN object key fingerprint")
            require(xorb_key is None or row["key_sha256"] == xorb_key,
                    "CDN viewer request did not read the approved hydration xorb")
            require(lfs_key is None or row["key_sha256"] == lfs_key,
                    "CDN viewer request did not read the approved service LFS object")
            viewer_keys.add(row["key_sha256"])
        if xorb_key is not None or service:
            if response["complete"]:
                require(response["status"] in (200, 206), "completed CDN response did not return object bytes")
                if response["bytes"]:
                    metric = {"hit": "edge_hits", "miss": "edge_misses", "unknown": "edge_unknown"}[response["cache"]]
                    observed_transfer[metric] += 1
                    observed_transfer["edge_bytes"] += response["bytes"]
            else:
                observed_transfer["incomplete_edge_bytes"] += response["bytes"]
        if edge == "cloud-cdn":
            require(type(row["cache_fill_bytes"]) is int and row["cache_fill_bytes"] >= 0,
                    "Cloud CDN log has invalid reported cache fill bytes")
            viewer_fill_bytes = max(viewer_fill_bytes, row["cache_fill_bytes"])
            children = row["origin_requests"]
            require(isinstance(children, dict), "invalid Cloud CDN origin requests")
            for origin_id, value in children.items():
                origin = fields(value, {"method", "status", "cache_fill_bytes"}, "Cloud CDN origin request")
                require(matches(SHA256, origin_id) and origin_id not in logged_ids
                        and origin["method"] in ("GET", "HEAD") and type(origin["status"]) is int and 100 <= origin["status"] <= 599
                        and type(origin["cache_fill_bytes"]) is int and origin["cache_fill_bytes"] >= 0,
                        "invalid or repeated Cloud CDN origin request")
                logged_ids.add(origin_id)
                origin_gets += origin["method"] == "GET"
                child_fill_bytes += origin["cache_fill_bytes"]
    if xorb_key is not None or service:
        require(client["edge_attempts"] == len(records) and all(client[key] == count for key, count in observed_transfer.items()),
                "CDN transfer totals disagree with the logged responses")
    if keyed_origin:
        service, viewer, timestamp, precision = (
            ("S3", "CloudFront", "at_unix_s", 1000) if edge == "cloudfront"
            else ("Azure Blob", "Front Door", "at_unix_ms", 1)
        )
        origins = proof["origin_requests"]
        require(isinstance(origins, dict), f"invalid {service} origin requests")
        start, finish = (client[key] // precision for key in ("started_at_unix_ms", "finished_at_unix_ms"))
        for origin_id, value in origins.items():
            origin = fields(value, {"key_sha256", timestamp, "status", "response_body_bytes"}, f"{service} origin request")
            require(matches(SHA256, origin_id) and origin_id not in logged_ids and matches(SHA256, origin["key_sha256"])
                    and origin["key_sha256"] in viewer_keys and type(origin[timestamp]) is int
                    and start <= origin[timestamp] <= finish
                    and type(origin["status"]) is int
                    and (100 <= origin["status"] <= 599 or (edge == "front-door" and origin["status"] == 0))
                    and type(origin["response_body_bytes"]) is int and origin["response_body_bytes"] >= 0,
                    f"{service} origin request does not match the timed {viewer} request")
            logged_ids.add(origin_id)
            origin_gets += 1
            if origin["status"] in (200, 206):
                child_fill_bytes += origin["response_body_bytes"]
    # Collapsed traffic and separate range-fills can describe the same bytes
    # at both levels. Use the strongest floor without adding overlapping totals.
    return logged_ids, origin_gets, max(viewer_fill_bytes, child_fill_bytes)


def verify_git_native_ref(
    value: Any, row: dict[str, Any], publication: dict[str, Any], publication_sha256: str,
    phase: str, origin_sha256: str, *, prefix_sha256: str | None = None,
) -> dict[str, Any]:
    trace = fields(value, CDN_PROVENANCE_FIELDS | {"schema", "phase", "platform", "publication_sha256",
                                                  "started_at_unix_ms", "observed_at_unix_ms", "native_ref"},
                   "native ref observation")
    require(trace["schema"] == "crab.cdn-native-ref/v1" and trace["phase"] == phase
            and trace["publication_sha256"] == publication_sha256
            and all(trace[key] == row[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "native ref observation does not match the Git publication and workflow attempt")
    observed = fields(trace["native_ref"], GIT_NATIVE_REF_FIELDS, "provider ref head")
    if prefix_sha256 is None:
        prefix_sha256 = hashlib.sha256(f"{row['fixture_prefix']}/".encode()).hexdigest()
    require(all(matches(SHA256, observed[key]) for key in GIT_NATIVE_REF_FIELDS - {"oid", "probe_source_sha"})
            and observed["probe_source_sha"] == row["source_sha"]
            and observed["origin_sha256"] == origin_sha256
            and observed["prefix_sha256"] == prefix_sha256
            and observed["ref_sha256"] == hashlib.sha256(publication["ref"].encode()).hexdigest(),
            "provider ref head was observed for a different source, origin, prefix, or ref")
    expected_oid = publication["oid"] if phase == "published" else None
    require(observed["oid"] == expected_oid, "provider ref head has an unexpected committed tip")
    require(positive(trace["started_at_unix_ms"]) and positive(trace["observed_at_unix_ms"])
            and trace["started_at_unix_ms"] <= trace["observed_at_unix_ms"],
            "native ref observation has an invalid interval")
    return trace


def verify_git_lifecycle(
    observations: dict[str, bytes], row: dict[str, Any], publication: dict[str, Any], publication_sha256: str,
    read_started: int, native_target: dict[str, str], retired_at: int, cleanup_at: int, origin_sha256: str,
) -> int:
    published, retired = (
        verify_git_native_ref(decode(observations[phase]), row, publication, publication_sha256, phase, origin_sha256)
        for phase in GIT_NATIVE_REF_FILES
    )
    require(publication["visible_at_unix_ms"] <= published["started_at_unix_ms"]
            <= published["observed_at_unix_ms"] <= read_started
            <= retired_at <= retired["started_at_unix_ms"] <= retired["observed_at_unix_ms"] <= cleanup_at,
            "native ref observations must bracket the fetch and precede fixture cleanup")
    before, after = published["native_ref"], retired["native_ref"]
    require(all(native_target[key] == before[key] for key in CLI_NATIVE_FIELDS),
            "Git product fetch and rollback must use the independently observed native target")
    require(all(before[key] == after[key] for key in ("repository_sha256", "epoch_sha256"))
            and all(before[key] != after[key] for key in ("transaction_id", "head_sha256", "revision_sha256")),
            "native ref retirement must commit a distinct tombstone in the same repository epoch")
    return published["observed_at_unix_ms"]


def verify_git_retirement(raw: bytes, row: dict[str, Any], publication_digest: str,
                          writer: dict[str, Any], rollback_finished: int) -> int:
    retirement = fields(decode(raw), GIT_RETIREMENT_FIELDS, "Git retirement report")
    require(retirement["schema"] == "crab.cdn-git-retirement/v1"
            and all(retirement[key] == row[key] for key in CDN_PROVENANCE_FIELDS)
            and retirement["publication_sha256"] == publication_digest and retirement["ref_retired"] is True
            and retirement["candidate"] == writer
            and positive(retirement["retired_at_unix_ms"])
            and retirement["retired_at_unix_ms"] >= rollback_finished,
            "Git product action requires retirement of the same published ref after rollback")
    return retirement["retired_at_unix_ms"]


def verify_service_lifecycle(
    records: dict[str, bytes], row: dict[str, Any], spec: dict[str, Any], *,
    cleanup_at: int, native_origin_sha256: str,
) -> dict[str, Any]:
    publication_raw = records["publication"]
    publication = validate_git_publication(publication_raw, row)
    publication_sha = hashlib.sha256(publication_raw).hexdigest()
    require(publication["published"], "service lifecycle requires a confirmed native publication")
    edge = validate_service_read(decode(records["edge"]), row, "edge")
    origin = validate_service_read(decode(records["origin"]), row, "origin")
    provider, _, _, family, _ = scope_parts(row["scope"])
    published = hashlib.sha256(records["published"]).hexdigest()
    prefix_sha256 = hashlib.sha256(f"{row['fixture_prefix']}/".encode()).hexdigest()
    workload = spec["service_workload"]
    require(workload["origin_sha256"] == native_origin_sha256 and workload["prefix_sha256"] == prefix_sha256,
            "service cleanup did not cover the registered native target")
    lfs_key = None
    if family == "lfs":
        key = f"{row['fixture_prefix']}/{lfs_object_path(publication['sha256'])}"
        lfs_key = hashlib.sha256(key.encode()).hexdigest()
    for arm, read in (("edge", edge), ("origin", origin)):
        require(read["candidate"] == spec["service_candidate"],
                "service read did not use the registered service binary")
        require(read["workload_sha256"] == workload["sha256"]
                and read["native_published_sha256"] == published and read["publication_sha256"] == publication_sha
                and read["commit"] == publication["oid"] and read["payload_sha256"] == publication["sha256"],
                "edge and rollback reads must use the same approved workload and native publication")
        require(read["payload_bytes"] == workload["payload_bytes"] and read["etag_sha256"] == workload["etag_sha256"],
                "service read does not match the approved payload size and ETag")
        require(read["reader"] == {
            "origin_sha256": native_origin_sha256, "prefix_sha256": prefix_sha256,
            "binding_sha256": spec["viewer_binding_sha256"] if arm == "edge" else "",
            "public_export_sha256": spec.get("publication_sha256", "") if arm == "edge" else "",
        }, "service read used a different native target or viewer binding")
        require(read["lfs_key_sha256"] == lfs_key, "service lifecycle requires the approved LFS object")
    require(edge["service_instance_sha256"] != origin["service_instance_sha256"],
            "service rollback must use a separate process")
    require(edge["git_blob_oid_sha256"] == origin["git_blob_oid_sha256"]
            and edge["finished_at_unix_ms"] <= origin["started_at_unix_ms"],
            "service rollback must follow the edge read and verify the same published blob")
    verify_viewer_requests(decode(records["viewer_requests"]), edge, hashlib.sha256(records["edge"]).hexdigest(), provider)
    retired_at = verify_git_retirement(
        records["retirement"], row, publication_sha, publication["candidate"], origin["finished_at_unix_ms"],
    )
    verify_git_lifecycle(records, row, publication, publication_sha, edge["started_at_unix_ms"], edge["reader"],
                         retired_at, cleanup_at, native_origin_sha256)
    return {
        "name": SERVICE_READ_ACTION, "expected_sha256": publication["sha256"], "observed_sha256": edge["payload_sha256"],
        "expected_bytes": origin["payload_bytes"], "observed_bytes": edge["payload_bytes"],
        "native_writes": 1, "visible_new_ref": True, "fixture_cleanup_verified": True,
        "observed_edge_gets": edge["observed_edge_gets"], "verified_edge_payloads": edge["verified_edge_payloads"],
    }


def collect_service_product_action(
    inputs: Path, row: dict[str, Any], spec: dict[str, Any], rollback_raw: bytes,
    *, cleanup_at: int, native_origin_sha256: str, retain: Path | None = None,
) -> dict[str, Any]:
    records = {name: read_git_record(inputs / filename, limit=GIT_FETCH_RECORD_LIMIT if name in ("edge", "viewer_requests")
                                    else 8 * 1024) for name, filename in SERVICE_PRODUCT_FILES.items()}
    result = verify_service_lifecycle(records | {"origin": rollback_raw}, row, spec,
                                      cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256)
    if retain is not None:
        retain.mkdir()
        for name, filename in SERVICE_PRODUCT_FILES.items():
            (retain / filename).write_bytes(records[name])
    return result


def validate_sdk_hydration_workload(raw: bytes, approval: str, configuration: str) -> dict[str, Any]:
    require(matches(SHA256, approval) and hashlib.sha256(raw).hexdigest() == approval,
            "hydration workload differs from the approved digest")
    spec = fields(decode(raw), SDK_HYDRATION_WORKLOAD_FIELDS, "SDK hydration workload")
    require(spec["schema"] == "crab.cdn-sdk-hydration-workload/v1"
            and spec["configuration_sha256"] == configuration
            and all(matches(SHA256, spec[name]) for name in
                    ("pointer_sha256", "payload_sha256", "crab_sha256", "global_prefix_sha256", "publication_sha256"))
            and positive(spec["payload_bytes"]) and spec["payload_bytes"] <= 512 * 1024 * 1024,
            "invalid approved SDK hydration workload")
    return spec


def validate_sdk_hydration_publication(raw: bytes, row: dict[str, Any]) -> dict[str, Any]:
    # The writer publishes before the projected view exists. The view fixture
    # therefore gets its own approval after publication.
    value = fields(decode(raw), GIT_PUBLICATION_FIELDS, "native SDK hydration publication")
    edge = scope_parts(row["scope"])[0]
    require(matches(SHA256, value["configuration_sha256"]), "SDK hydration publication needs its source fixture digest")
    provenance = row | {"scope": f"{edge}/private/sdk/git/direct", "configuration_sha256": value["configuration_sha256"]}
    return validate_git_publication(raw, provenance)


def verify_sdk_hydration_read(
    raw: bytes, row: dict[str, Any], registration: dict[str, Any], arm: str,
    spec: dict[str, Any], placement: dict[str, Any],
) -> dict[str, Any]:
    local = arm == "warm_local"
    expected_fields = SDK_HYDRATION_READ_FIELDS | ({"primer_sha256"} if local else set())
    read = fields(decode(raw), expected_fields, "SDK hydration record")
    require(read["schema"] == "crab.cdn-sdk-hydration/v1" and read["arm"] == arm
            and all(read[key] == row[key] for key in CDN_PROVENANCE_FIELDS | {"platform"})
            and read["workload_sha256"] == registration["hydration_workload_sha256"]
            and read["candidate"] == registration["sdk_candidate"]
            and read["crab_sha256"] == spec["crab_sha256"] and read["pointer_sha256"] == spec["pointer_sha256"],
            "SDK hydration used a different workflow, workload or executable")
    require("local" in read["candidate"]["cargo_features"].split(","), "SDK hydration requires the local feature")
    payload = fields(read["payload"], {"sha256", "bytes"}, "verified hydrated file")
    require(type(payload["bytes"]) is int and payload == {"sha256": spec["payload_sha256"], "bytes": spec["payload_bytes"]},
            "SDK hydration did not verify the approved file")
    target = placement["views"]["target"]
    reader = fields(read["reader"], CLI_NATIVE_FIELDS | {
        "global_prefix_sha256", "binding_sha256", "approval_sha256", "cache_root_sha256", "local_cache_only",
    }, "verified SDK hydration target")
    require(reader["origin_sha256"] == placement["origin_sha256"]
            and reader["prefix_sha256"] == target["physical_prefix_sha256"]
            and read["xorb_key_sha256"] == target["key_sha256"]
            and reader["global_prefix_sha256"] == spec["global_prefix_sha256"]
            and matches(SHA256, reader["cache_root_sha256"])
            and reader["local_cache_only"] is True,
            "SDK hydration used a different projected view, xorb, or cache service")
    approved = arm != "origin"
    require(reader["binding_sha256"] == (registration["viewer_binding_sha256"] if approved else "")
            and reader["approval_sha256"] == (registration["viewer_approval_sha256"] if approved else ""),
            "SDK hydration did not use the required viewer approval")
    verify_git_traffic(read, scope_parts(row["scope"])[0], "SDK hydration")
    if local:
        require(matches(SHA256, read["primer_sha256"]) and positive(read["local_payload_hits"])
                and all(read[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "SDK warm hydration requires verified local chunks with zero edge attempts")
    elif approved:
        require(all(positive(read[key]) for key in ("observed_edge_gets", "verified_edge_payloads", "edge_bytes"))
                and read["local_payload_hits"] == 0, "SDK hydration accepted no verified edge xorb")
    else:
        native = read["native_traffic"]
        require(all(read[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "SDK hydration rollback requires native payload reads without edge or cache activity")
    return read


def collect_sdk_hydration_evidence(
    directory: Path, row: dict[str, Any], registration: dict[str, Any], rollback_raw: bytes, *,
    placement: dict[str, Any], cleanup_at: int, native_origin_sha256: str,
    retain: Path | None = None, policy: Path | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    require(deployment_component_digest(placement) == registration["scoped_view_sha256"]
            and placement["fixture_sha256"] == row["configuration_sha256"]
            and placement["origin_sha256"] == native_origin_sha256,
            "SDK hydration view differs from the approved fixture and native origin")
    inputs = directory / SDK_HYDRATION_DIR
    workload_raw = read_git_record(inputs / "workload.json")
    approval = registration["hydration_workload_sha256"]
    spec = validate_sdk_hydration_workload(workload_raw, approval, row["configuration_sha256"])
    publication_raw = read_git_record(inputs / "publication.json")
    receipt = hashlib.sha256(publication_raw).hexdigest()
    publication = validate_sdk_hydration_publication(publication_raw, row)
    require(receipt == spec["publication_sha256"] and publication["published"]
            and publication["sha256"] == spec["pointer_sha256"],
            "SDK hydration requires independent native publication of the approved pointer")
    writer = verify_read_candidate(publication["candidate"], row["source_sha"], row["platform"], policy=policy)
    require(writer["binary_sha256"] == spec["crab_sha256"],
            "SDK hydration must use the approved Crab executable for the native publication and reads")
    edge_raw = read_git_record(inputs / "hydration.json", limit=GIT_FETCH_RECORD_LIMIT)

    def verify_read(raw: bytes, arm: str) -> dict[str, Any]:
        return verify_sdk_hydration_read(raw, row, registration, arm, spec, placement)

    edge, native = verify_read(edge_raw, "edge"), verify_read(rollback_raw, "origin")
    product_caches = {read["reader"]["cache_root_sha256"] for read in (edge, native)}
    require(len(product_caches) == 2
            and edge["finished_at_unix_ms"] <= native["started_at_unix_ms"],
            "SDK hydration rollback must follow the edge read with the same view and a fresh cache")
    retirement_raw = read_git_record(inputs / "retirement.json")
    retired = verify_git_retirement(retirement_raw, publication, receipt, writer, native["finished_at_unix_ms"])
    observations = {phase: read_git_record(inputs / filename) for phase, filename in GIT_NATIVE_REF_FILES.items()}
    # The native ref canary sees the logical source; the SDK receives its independently approved projection.
    source = {"origin_sha256": placement["origin_sha256"], "prefix_sha256": placement["source_prefix_sha256"]}
    published = verify_git_lifecycle(observations, row, publication, receipt, edge["started_at_unix_ms"], source,
                                     retired, cleanup_at, native_origin_sha256)

    def verify_measurement(raw: bytes, arm: str) -> dict[str, Any]:
        read = verify_read(raw, arm)
        require(published <= read["started_at_unix_ms"] <= read["finished_at_unix_ms"] <= retired,
                "SDK hydration benchmark ran outside the independently observed publication lifetime")
        require(read["reader"]["cache_root_sha256"] not in product_caches,
                "SDK hydration benchmark reused its lifecycle canary's local cache")
        return read

    performance = collect_git_performance(
        directory / GIT_BENCHMARK_DIR, registration, row["platform"], row["workflow_run_id"], row["workflow_run_attempt"],
        retain=retain / GIT_BENCHMARK_DIR if retain is not None else None, verify_hydration=verify_measurement,
    )
    if retain is not None:
        product = retain / SDK_HYDRATION_DIR
        product.mkdir()
        records = {"workload.json": workload_raw, "publication.json": publication_raw, "hydration.json": edge_raw,
                   "retirement.json": retirement_raw}
        records.update({filename: observations[phase] for phase, filename in GIT_NATIVE_REF_FILES.items()})
        for name, raw in records.items():
            (product / name).write_bytes(raw)
    return {
        "name": SDK_HYDRATION_ACTION, "expected_sha256": spec["payload_sha256"], "observed_sha256": edge["payload"]["sha256"],
        "expected_bytes": spec["payload_bytes"], "observed_bytes": edge["payload"]["bytes"],
        "native_writes": 1, "visible_new_ref": True, "fixture_cleanup_verified": True,
        "observed_edge_gets": edge["observed_edge_gets"], "verified_edge_payloads": edge["verified_edge_payloads"],
    }, performance


def collect_git_product_action(
    inputs: Path, row: dict[str, Any], rollback_raw: bytes, *, retain: Path | None = None, cleanup_at: int | None = None,
    native_origin_sha256: str | None = None, public_export_sha256: str = "",
) -> dict[str, Any]:
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    require(publication["published"], "Git product action requires a ref confirmed after native push")
    provenance = {key: row[key] for key in CDN_PROVENANCE_FIELDS}
    fetch_provenance = provenance | {"platform": row["platform"], "candidate": row["candidate"]}
    require(publication["candidate"] == row["candidate"], "publication did not run the registered CLI build")
    fetch_raw = read_git_record(inputs / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT)
    fetch = validate_git_fetch(fetch_raw, fetch_provenance, "edge", "Git product action")
    require(fetch["reader"]["binding_sha256"] == row.get("viewer_binding_sha256"),
            "product fetch did not use the registered viewer binding")
    require(fetch["reader"]["public_export_sha256"] == public_export_sha256,
            "product fetch did not use the registered public export approval")
    digest = hashlib.sha256(publication_raw).hexdigest()
    require(fetch["publication_sha256"] == digest and fetch["publication_oid"] == publication["oid"]
            and fetch["expected_sha256"] == publication["sha256"]
            and fetch["started_at_unix_ms"] >= publication["visible_at_unix_ms"],
            "Git product fetch did not verify this native publication after the ref became visible")
    rollback = validate_git_fetch(rollback_raw, fetch_provenance, "origin", "CDN rollback")
    require(all(rollback[key] == fetch[key] for key in ("publication_sha256", "publication_oid", "observed_sha256")),
            "CDN rollback must verify the same published payload as the edge fetch")
    require(all(rollback["reader"][key] == fetch["reader"][key] for key in CLI_NATIVE_FIELDS),
            "CDN rollback changed the selected native target")
    require(rollback["reader"]["cache_root_sha256"] != fetch["reader"]["cache_root_sha256"],
            "CDN rollback reused the edge fetch cache")
    require(fetch["finished_at_unix_ms"] <= rollback["started_at_unix_ms"],
            "CDN rollback must follow the verified edge fetch")
    retirement_raw = read_git_record(inputs / "retirement.json")
    retired_at = verify_git_retirement(retirement_raw, row, digest, row["candidate"], rollback["finished_at_unix_ms"])
    if cleanup_at is not None:
        require(cleanup_at >= retired_at, "fixture cleanup must be verified after Git ref retirement")
    observations = {}
    if retain is not None or native_origin_sha256 is not None:
        observations = {phase: read_git_record(inputs / filename) for phase, filename in GIT_NATIVE_REF_FILES.items()}
    if native_origin_sha256 is not None:
        require(cleanup_at is not None, "native ref observations require a verified cleanup")
        verify_git_lifecycle(observations, row, publication, digest, fetch["started_at_unix_ms"], fetch["reader"],
                             retired_at, cleanup_at, native_origin_sha256)
    if retain is not None:
        retain.mkdir()
        for filename, raw in (("publication.json", publication_raw), ("fetch.json", fetch_raw),
                              ("retirement.json", retirement_raw)):
            (retain / filename).write_bytes(raw)
        for phase, raw in observations.items():
            (retain / GIT_NATIVE_REF_FILES[phase]).write_bytes(raw)
    return {key: fetch[key] for key in (
        "name", "expected_sha256", "observed_sha256", "expected_bytes", "observed_bytes",
        "native_writes", "observed_edge_gets", "visible_new_ref", "verified_edge_payloads",
    )}


def collect_sdk_product_action(
    inputs: Path, row: dict[str, Any], spec: dict[str, Any], rollback_raw: bytes,
    *, cleanup_at: int, native_origin_sha256: str, retain: Path | None = None,
) -> dict[str, Any]:
    native_records = {phase: read_git_record(inputs / name) for phase, name in GIT_NATIVE_REF_FILES.items()}
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    require(publication["published"], "SDK snapshot requires a ref confirmed after native push")
    receipt = hashlib.sha256(publication_raw).hexdigest()
    edge_raw = read_git_record(inputs / "snapshot.json", limit=GIT_FETCH_RECORD_LIMIT)
    edge = verify_sdk_git_read(edge_raw, row, spec, "edge")
    origin = verify_sdk_git_read(rollback_raw, row, spec, "origin")
    require(all(read["publication_sha256"] == receipt and read["commit"] == publication["oid"]
                and read["sha256"] == publication["sha256"] for read in (edge, origin))
            and edge["git_blob_oid_sha256"] == origin["git_blob_oid_sha256"],
            "SDK snapshot and rollback must verify the same published Git blob")
    require(all(edge["reader"][key] == origin["reader"][key] for key in CLI_NATIVE_FIELDS),
            "SDK rollback changed the selected native target")
    require(edge["finished_at_unix_ms"] <= origin["started_at_unix_ms"],
            "SDK rollback must follow the verified edge snapshot")
    retained = {"publication.json": publication_raw, "snapshot.json": edge_raw}
    previous_end = origin["finished_at_unix_ms"]
    for action, files in (("clone", SDK_CLONE_FILES), ("fetch", SDK_FETCH_FILES)):
        pair = {}
        for arm, name in files.items():
            raw = read_git_record(inputs / name, limit=GIT_FETCH_RECORD_LIMIT)
            retained[name] = raw
            pair[arm] = verify_sdk_git_read(raw, row, spec, arm, worktree_action=action)
        edge_worktree, native_worktree = pair["edge"], pair["origin"]
        require(all(worktree["crab_sha256"] == publication["candidate"]["binary_sha256"]
                    and all(worktree[field] == edge[field] for field in (
                        "publication_sha256", "commit", "sha256", "git_blob_oid_sha256",
                    ))
                    and all(worktree["reader"][field] == edge["reader"][field] for field in CLI_NATIVE_FIELDS)
                    for worktree in pair.values()),
                f"SDK {action} must use the approved Crab build, native target, and published blob")
        require(set(edge_worktree["verified_edge_packs"]) <= set(native_worktree["installed_packs"]),
                f"SDK native {action} must install the same verified CDN packs")
        if action == "fetch":
            require(edge_worktree["previous_head"] == native_worktree["previous_head"],
                    "SDK fetch arms must start from the same local HEAD")
        require(previous_end <= edge_worktree["started_at_unix_ms"]
                and edge_worktree["finished_at_unix_ms"] <= native_worktree["started_at_unix_ms"],
                f"SDK {action} arms must follow the preceding workload in order")
        previous_end = native_worktree["finished_at_unix_ms"]
    retirement_raw = read_git_record(inputs / "retirement.json")
    retired_at = verify_git_retirement(retirement_raw, row, receipt, publication["candidate"], previous_end)
    earliest = edge
    retained["retirement.json"] = retirement_raw
    if scope_parts(row["scope"])[1] == "public":
        prior_raw = read_git_record(inputs / SDK_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
        retained[SDK_PRIOR_EXPORT_RECORD] = prior_raw
        earliest = verify_sdk_git_read(prior_raw, row, spec, "prior_export")
    verify_git_lifecycle(native_records, row, publication, receipt, earliest["started_at_unix_ms"], earliest["reader"],
                         retired_at, cleanup_at, native_origin_sha256)
    if retain is not None:
        retain.mkdir()
        retained.update({name: native_records[phase] for phase, name in GIT_NATIVE_REF_FILES.items()})
        for name, raw in retained.items():
            (retain / name).write_bytes(raw)
    return {
        "name": SDK_GIT_ACTION, "expected_sha256": publication["sha256"], "observed_sha256": edge["sha256"],
        "expected_bytes": GIT_PAYLOAD_SIZE, "observed_bytes": edge["bytes"],
        "native_writes": 1, "visible_new_ref": True, "fixture_cleanup_verified": True,
        "observed_edge_gets": edge["observed_edge_gets"], "verified_edge_payloads": edge["verified_edge_payloads"],
    }


def derive_sdk_public_transition(inputs: Path, row: dict[str, Any], spec: dict[str, Any]) -> dict[str, Any]:
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    digest = hashlib.sha256(publication_raw).hexdigest()
    prior_raw = read_git_record(inputs / SDK_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    edge_raw = read_git_record(inputs / "snapshot.json", limit=GIT_FETCH_RECORD_LIMIT)
    prior = verify_sdk_git_read(prior_raw, row, spec, "prior_export")
    edge = verify_sdk_git_read(edge_raw, row, spec, "edge")
    key = prior["transition_key_sha256"]
    require(key == edge.get("transition_key_sha256"), "SDK export transition did not verify the same pack-layer key")
    require(publication["published"] and
            all(read["publication_sha256"] == digest and read["commit"] == publication["oid"]
                and read["sha256"] == publication["sha256"] for read in (prior, edge))
            and prior["git_blob_oid_sha256"] == edge["git_blob_oid_sha256"],
            "SDK export transition did not verify the same published Git blob")
    require(all(prior["reader"][field] == edge["reader"][field] for field in CLI_NATIVE_FIELDS),
            "SDK export transition changed the selected native target")
    reference = deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
    trace = {field: row[field] for field in CDN_PROVENANCE_FIELDS | {"platform"}} | {
        "schema": "crab.cdn-public-transition/v1", "key_sha256": key,
        "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": publication["visible_at_unix_ms"]},
    }
    for phase, read in (("before", prior), ("after", edge)):
        trace[phase] = {
            "action": SDK_GIT_ACTION, "receipt_sha256": read["approval_sha256"], "key_sha256": key,
            "reference_sha256": reference, "result_sha256": read["sha256"], "result_bytes": read["bytes"],
            "started_at_unix_ms": read["started_at_unix_ms"], "finished_at_unix_ms": read["finished_at_unix_ms"],
            "edge_attempts": read["all_edge_attempts"], "binding_active": True, "reader": read["reader"],
            "verified_edge_key_sha256": key if phase == "after" else None,
        }
    return trace


def collect_git_performance(
    inputs: Path, registration: dict[str, Any], platform: str, run_id: str, attempt: str,
    *, retain: Path | None = None, sdk_native: dict[str, str] | None = None,
    verify_hydration: Callable[[bytes, str], dict[str, Any]] | None = None,
    service_row: Path | None = None,
) -> dict[str, Any]:
    spec = benchmark_registration(registration)
    scope = spec["scope"]
    hydration = is_direct_sdk_hydration_scope(scope)
    require(hydration == (verify_hydration is not None), "SDK hydration benchmarks require verified publication and placement")
    sdk = is_direct_sdk_git_scope(scope)
    service = is_direct_service_git_scope(scope)
    require(service == (service_row is not None), "service Git benchmarks require the retained lifecycle row")
    require(sdk or hydration or service or (requires_cli_workload(scope) and spec["workload"] == cli_action_name(scope)),
            "benchmark must register a supported direct CLI, SDK or service workload")
    if sdk:
        require(inputs.is_dir(), "direct SDK Git requires retained regional SDK benchmark records")
    if hydration:
        require(inputs.is_dir(), "direct SDK hydration requires retained regional SDK benchmark records")
    require(matches(COMMIT, spec["source_sha"]) and matches(SHA256, spec["configuration_sha256"])
            and matches(RUN_NUMBER, run_id) and matches(RUN_NUMBER, attempt) and matches(LABEL, platform),
            "invalid benchmark provenance")
    provider = PROVIDER_FOR_EDGE[scope.split("/", 1)[0]]
    provenance = {key: spec[key] for key in ("source_sha", "scope", "configuration_sha256")}
    provenance |= {"workflow_run_id": run_id, "workflow_run_attempt": attempt, "platform": platform}
    if not (sdk or hydration or service):
        provenance["candidate"] = spec["candidate"]
    service_anchor = None
    service_instances = set()
    provider_request_ids = set()
    published_at, retired_at = 0, 0
    if service_row is not None:
        require(inputs.is_dir(), "direct service Git requires retained regional benchmark records")
        product = service_row / SERVICE_PRODUCT_DIR
        edge_raw = read_git_record(product / "edge.json", limit=GIT_FETCH_RECORD_LIMIT)
        service_anchor = validate_service_read(
            decode(edge_raw), provenance, "edge",
        )
        service_origin = validate_service_read(decode(read_trace(service_row / "traces/rollback.json", "rollback")),
                                               provenance, "origin")
        service_instances.update(record["service_instance_sha256"] for record in (service_anchor, service_origin))
        workload = spec["service_workload"]
        publication_raw = read_git_record(product / "publication.json")
        publication = validate_git_publication(publication_raw, provenance)
        receipt = hashlib.sha256(publication_raw).hexdigest()
        observed_raw = read_git_record(product / GIT_NATIVE_REF_FILES["published"])
        require(publication["published"] and service_anchor["publication_sha256"] == receipt
                and service_anchor["native_published_sha256"] == hashlib.sha256(observed_raw).hexdigest(),
                "regional service reads require the independently observed publication")
        observed = verify_git_native_ref(decode(observed_raw), provenance, publication, receipt, "published",
                                         workload["origin_sha256"], prefix_sha256=workload["prefix_sha256"])
        published_at = observed["observed_at_unix_ms"]
        require(publication["visible_at_unix_ms"] <= observed["started_at_unix_ms"],
                "regional service reads require a valid publication interval")
        retired_at = verify_git_retirement(read_git_record(product / "retirement.json"), provenance, receipt,
                                           publication["candidate"], service_origin["finished_at_unix_ms"])
        require(service_anchor["candidate"] == service_origin["candidate"] == spec["service_candidate"],
                "regional service reads require the registered service binary")
        require(service_anchor["workload_sha256"] == workload["sha256"]
                and service_anchor["commit"] == publication["oid"] and service_anchor["payload_sha256"] == publication["sha256"]
                and service_anchor["payload_bytes"] == workload["payload_bytes"]
                and service_anchor["etag_sha256"] == workload["etag_sha256"]
                and service_anchor["reader"] == {
                    "origin_sha256": workload["origin_sha256"], "prefix_sha256": workload["prefix_sha256"],
                    "binding_sha256": spec["viewer_binding_sha256"], "public_export_sha256": spec.get("publication_sha256", ""),
                }, "regional service reads require the approved lifecycle workload and viewer")
        provider_request_ids, _, _ = verify_viewer_requests(
            decode(read_trace(product / "viewer-requests.json", "service viewer requests")), service_anchor,
            hashlib.sha256(edge_raw).hexdigest(), PROVIDERS[provider][0],
        )
    seen = set()
    cache_roots = set()
    performance = {}
    for location, region in spec["regions"].items():
        pairs = []
        previous_end = 0
        for index in range(1, spec["samples"] + 1):
            pair = {}
            fixtures = set()
            windows = {}
            primer_digest = None
            primer_cache = None
            service_primer = None
            for arm in ARMS:
                slot = Path(location) / f"{index:04d}" / arm
                label = f"{location} {index} {arm}"
                client_raw = read_git_record(inputs / slot / "client.json", limit=GIT_FETCH_RECORD_LIMIT)
                digest = hashlib.sha256(client_raw).hexdigest()
                require(digest not in seen, "a fetch was reused in another benchmark arm or pair")
                seen.add(digest)
                if verify_hydration is not None:
                    client = verify_hydration(client_raw, GIT_MODES[arm])
                    reader = client["reader"]
                    cache_root = reader["cache_root_sha256"]
                    if arm == "warm_local":
                        require(client["primer_sha256"] == primer_digest and cache_root == primer_cache,
                                f"{label} did not repeat its own warm edge hydration in the same cache")
                    else:
                        require(cache_root not in cache_roots, "SDK hydration benchmark arms reused a local cache root")
                        cache_roots.add(cache_root)
                    if arm == "warm_edge":
                        primer_digest, primer_cache = digest, cache_root
                    payload = (client["pointer_sha256"], client["payload"]["sha256"], client["xorb_key_sha256"])
                    receipt = client["workload_sha256"]
                elif sdk:
                    client = verify_sdk_git_read(client_raw, provenance, spec, GIT_MODES[arm])
                    reader = client["reader"]
                    require(sdk_native is None or all(reader[key] == sdk_native[key] for key in CLI_NATIVE_FIELDS),
                            f"{label} changed the approved SDK native target")
                    if arm == "warm_edge":
                        primer_digest = digest
                    if arm == "warm_local":
                        require(client["primer_sha256"] == primer_digest, f"{label} did not repeat its own warm edge snapshot")
                    payload = (client["commit"], client["sha256"], client["git_blob_oid_sha256"])
                    receipt = client["publication_sha256"]
                elif service_anchor is not None:
                    client = validate_service_read(decode(client_raw), provenance, GIT_MODES[arm])
                    reader = client["reader"]
                    expected = service_anchor["reader"]
                    if arm == "origin":
                        expected = expected | {"binding_sha256": "", "public_export_sha256": ""}
                    require(reader == expected and all(client[key] == service_anchor[key] for key in (
                        "candidate", "workload_sha256", "publication_sha256", "native_published_sha256", "commit",
                        "payload_sha256", "payload_bytes", "etag_sha256", "git_blob_oid_sha256",
                    )), f"{label} used a different service workload or read target")
                    require(published_at <= client["started_at_unix_ms"] <= client["finished_at_unix_ms"] <= retired_at,
                            f"{label} ran outside the independently observed publication interval")
                    if arm == "warm_local":
                        require(service_primer is not None, f"{label} has no warm edge primer")
                        verify_service_repeat(client, service_primer, primer_digest)
                    else:
                        instance = client["service_instance_sha256"]
                        require(instance not in service_instances, f"{label} must use a fresh service process")
                        service_instances.add(instance)
                    if arm == "warm_edge":
                        service_primer, primer_digest = client, digest
                    payload = (client["commit"], client["payload_sha256"], client["etag_sha256"], client["git_blob_oid_sha256"])
                    receipt = client["publication_sha256"]
                else:
                    client = validate_git_fetch(client_raw, provenance, GIT_MODES[arm], label)
                    reader = client["reader"]
                    require(arm == "origin" or reader["binding_sha256"] == spec["viewer_binding_sha256"],
                            f"{label} did not use the required viewer configuration")
                    require(arm == "origin" or reader["public_export_sha256"] == spec.get("publication_sha256", ""),
                            f"{label} did not use the registered public export approval")
                    cache_root = reader["cache_root_sha256"]
                    require(cache_root not in cache_roots, "benchmark arms reused a local cache root")
                    cache_roots.add(cache_root)
                    payload = (client["publication_oid"], client["expected_sha256"])
                    receipt = client["publication_sha256"]
                fixtures.add((*payload, receipt, reader["origin_sha256"], reader["prefix_sha256"]))
                windows[arm] = (client["started_at_unix_ms"], client["finished_at_unix_ms"])
                viewer_log = None
                cdn_origin_gets = cdn_fill_bytes = 0
                if provider in VIEWER_REQUEST_FILES and client["all_edge_attempts"]:
                    log_name = VIEWER_REQUEST_FILES[provider]
                    viewer_log = read_git_record(inputs / slot / log_name, limit=GIT_FETCH_RECORD_LIMIT)
                    request_ids, cdn_origin_gets, cdn_fill_bytes = verify_viewer_requests(
                        decode(viewer_log), client, digest, PROVIDERS[provider][0],
                    )
                    require(provider_request_ids.isdisjoint(request_ids), "CDN log requests were reused across arms")
                    provider_request_ids.update(request_ids)
                external_raw = read_git_record(inputs / slot / "measurement.json")
                external = fields(decode(external_raw), GIT_MEASUREMENT_FIELDS, "benchmark measurement")
                require(external["schema"] == "crab.cdn-git-measurement/v1" and external["provider"] == provider
                        and external["region"] == region and external["client_report_sha256"] == digest,
                        f"{label} has mismatched provider measurements")
                measured = {key: client[key] for key in GIT_CLIENT_METRICS}
                measured.update({key: external[key] for key in GIT_EXTERNAL_METRICS})
                require(all(type(value) is int and value >= 0 for value in measured.values()),
                        f"{label} is missing measured metrics")
                require(positive(measured["peak_memory_bytes"]) and (sdk or service or positive(measured["peak_disk_bytes"])),
                        f"{label} is missing fetch memory or disk measurements")
                incomplete = client["incomplete_edge_bytes"]
                native = client["native_traffic"]
                require(measured["origin_gets"] >= native["get_responses"] + cdn_origin_gets,
                        f"{label} undercounts observed native GET responses or logged CDN origin GETs")
                require(measured["origin_fill_bytes"] >= cdn_fill_bytes,
                        f"{label} undercounts logged CDN cache fill bytes")
                require(provider != "s3" or arm != "cold_edge" or positive(cdn_fill_bytes),
                        f"{label} has no logged CloudFront fill from the approved S3 origin")
                require(provider != "azure" or arm != "cold_edge" or positive(cdn_fill_bytes),
                        f"{label} has no logged Front Door fill from the approved Blob origin")
                require(not (sdk or hydration or service) or provider != "gcs" or arm != "cold_edge" or positive(cdn_fill_bytes),
                        f"{label} has no logged Cloud CDN fill from the approved GCS origin")
                require(incomplete <= measured["rejected_bytes"]
                        and measured["client_bytes"] >= native["body_bytes"] + measured["edge_bytes"] + incomplete,
                        f"{label} undercounts observed client bytes")
                pair[arm] = measured
                if retain is not None:
                    destination = retain / slot
                    destination.mkdir(parents=True)
                    (destination / "client.json").write_bytes(client_raw)
                    (destination / "measurement.json").write_bytes(external_raw)
                    if viewer_log is not None:
                        (destination / log_name).write_bytes(viewer_log)
            require(len(fixtures) == 1, f"{location} {index} arms fetched different published fixtures")
            intervals = sorted(windows.values())
            require(intervals[0][0] >= previous_end
                    and all(left[1] <= right[0] for left, right in zip(intervals, intervals[1:])),
                    f"{location} {index} benchmark arms and pairs must be interleaved without overlapping")
            require(windows["cold_edge"][1] <= windows["warm_edge"][0]
                    and windows["warm_edge"][1] <= windows["warm_local"][0],
                    f"{location} {index} must measure cold edge, warm edge and warm local in order")
            if provider == "s3":
                require(windows["cold_edge"][1] // 1000 < windows["warm_edge"][0] // 1000,
                        f"{location} {index} CloudFront edge arms must use disjoint S3 log seconds")
            previous_end = intervals[-1][1]
            pairs.append(pair)
        performance[location] = {"region": region, "pairs": pairs}
    return performance


def verify_cdn_report(
    report: Any, *, scope: str, configuration_sha256: str, platform: str, provider: str,
    verified_origin: dict[str, Any], origin_sha256: str, source_sha: str,
    run_id: str, run_attempt: str, directory: Path, registration: Any, policy: Path | None = None,
) -> dict[str, Any]:
    """Validate a report after the existing verifier has checked its origin report."""
    row = fields(report, {
        "schema", "schema_version", "status", "scope", "source_sha", "workflow_run_id",
        "workflow_run_attempt", "origin_report_sha256", "configuration_sha256", "deployment",
        "platform", "fixture_prefix", "checks", "product_action", "performance",
    }, "CDN report")
    require(row["schema"] == SCHEMA and type(row["schema_version"]) is int
            and row["schema_version"] == 1, "invalid CDN report schema")
    require(row["status"] == "qualified", "report does not claim a qualified CDN row")
    require(matches(COMMIT, source_sha) and matches(RUN_NUMBER, run_id) and matches(RUN_NUMBER, run_attempt),
            "CDN qualification requires an exact commit and numeric workflow run and attempt")
    require(row["scope"] == scope, "CDN report scope does not match the required row")
    edge, access, _, family, profile = scope_parts(scope)
    expected_edge, expected_service = PROVIDERS[provider]
    require(edge == expected_edge and verified_origin["provider"] == provider
            and verified_origin["service"] == expected_service,
            "CDN qualification requires the matching real cloud provider")
    require(row["source_sha"] == source_sha and row["workflow_run_id"] == run_id
            and row["workflow_run_attempt"] == run_attempt
            and row["origin_report_sha256"] == origin_sha256,
            "CDN report does not match the verified origin report commit, run, attempt and digest")
    require(matches(SHA256, configuration_sha256) and row["configuration_sha256"] == configuration_sha256,
            "CDN configuration digest does not match")
    require(matches(LABEL, row["deployment"]), "invalid deployment label")
    require(matches(LABEL, platform) and row["platform"] == platform, "CDN platform does not match")
    require(matches(PREFIX, row["fixture_prefix"]), "unsafe CDN qualification fixture prefix")
    require(isinstance(registration, dict) and all(
        registration.get(name) == row[name] for name in ("source_sha", "scope", "configuration_sha256")
    ), "benchmark registration does not match the release row")

    cli_workload = requires_cli_workload(scope)
    sdk_git = is_direct_sdk_git_scope(scope)
    sdk_hydration = is_direct_sdk_hydration_scope(scope)
    # Public service rows need their own transition proof. Service LFS still lacks warm-local measurements.
    service_release = is_direct_service_git_scope(scope) and access == "private"
    require(policy is None or cli_workload or sdk_git or sdk_hydration or service_release,
            "CDN promotion requires a product-specific workload and per-run regional evidence verifier")
    if cli_workload:
        verify_read_candidate(registration.get("candidate"), source_sha, platform, policy=policy)
    if sdk_git or sdk_hydration:
        verify_sdk_candidate(registration.get("sdk_candidate"), source_sha, platform, policy=policy)
    if service_release:
        verify_service_candidate(registration.get("service_candidate"), source_sha, platform, policy=policy)
    lower = verify_performance(row["performance"], registration)
    verify_product_action(row["product_action"], family, sdk_hydration=sdk_hydration)
    require(row["product_action"]["name"] == registration["workload"],
            "product action does not match the registered workload")
    checks, cleanup_at, native_origin_sha256, rollback = verify_check_evidence(
        row, edge, access, profile, directory, registration=registration,
    )
    if cli_workload:
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt)
        require(measured == row["performance"], "CLI performance summary differs from the retained measurements")
        require(rollback is not None, "direct CLI qualification requires a product rollback fetch")
        registered_read = {key: registration[key] for key in ("candidate", "viewer_binding_sha256")}
        product = collect_git_product_action(
            directory / GIT_PRODUCT_DIR, row | registered_read, rollback, cleanup_at=cleanup_at,
            native_origin_sha256=native_origin_sha256, public_export_sha256=registration.get("publication_sha256", ""),
        )
        require(row["product_action"] == product | {"fixture_cleanup_verified": True},
                "product action differs from the retained publication, fetch, and retirement evidence")
    if sdk_git:
        require(rollback is not None, "direct SDK qualification requires an ordinary SDK rollback read")
        product = collect_sdk_product_action(directory / GIT_PRODUCT_DIR, row, registration, rollback,
                                             cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256)
        require(row["product_action"] == product, "product action differs from the retained SDK snapshot evidence")
        sdk_native = {"origin_sha256": native_origin_sha256,
                      "prefix_sha256": hashlib.sha256(f"{row['fixture_prefix']}/".encode()).hexdigest()}
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                          sdk_native=sdk_native)
        require(measured == row["performance"], "SDK performance summary differs from the retained measurements")
    if sdk_hydration:
        require(rollback is not None, "direct SDK hydration requires its native rollback")
        placement = decode(read_trace(directory / "traces" / f"{SCOPED_VIEW_CHECK}.json", SCOPED_VIEW_CHECK))["placement"]
        product, measured = collect_sdk_hydration_evidence(
            directory, row, registration, rollback, placement=placement,
            cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256, policy=policy,
        )
        require(row["product_action"] == product, "product action differs from the retained SDK hydration evidence")
        require(row["performance"] == measured, "SDK hydration performance summary differs from the retained measurements")
    if is_direct_service_scope(scope):
        require(rollback is not None, "direct service qualification requires an ordinary HTTP rollback read")
        product = collect_service_product_action(directory / SERVICE_PRODUCT_DIR, row, registration, rollback,
                                                 cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256)
        require(row["product_action"] == product, "product action differs from the retained service lifecycle evidence")
        if is_direct_service_git_scope(scope):
            measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                               service_row=directory)
            require(row["performance"] == measured, "service performance summary differs from the retained measurements")
    return {"status": "verified", "scope": scope, "platform": platform, "deployment": row["deployment"],
            "checks": checks, "distant_median_savings_lower_bound_ms": lower}
