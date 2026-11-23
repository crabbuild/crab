"""Validate one retained CDN qualification row for a real provider."""

from __future__ import annotations

import hashlib
import json
import math
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, Callable, NamedTuple

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
EXPIRY_FULL_CHECK = "warm_expired_viewer_denied"
EXPIRY_RANGE_CHECK = "expired_parallel_range_denied"
EXPIRY_CHECKS = {EXPIRY_FULL_CHECK, EXPIRY_RANGE_CHECK}
EXPIRY_TIMING_FIELDS = {"started_at_unix_us", "finished_at_unix_us"}
EXPIRY_GRANT_FIELDS = {"viewer_expires_at_unix", "viewer_grant_sha256"}
EXPIRY_SETTLE_SECONDS = 2
UNRELATED_VIEWER_CHECK = "warm_unrelated_principal_denied"
UNRELATED_REQUEST_FIELDS = EXPIRY_GRANT_FIELDS | EXPIRY_TIMING_FIELDS | {"range_offset", "body_sha256"}
WARM_DENIAL_CHECKS = {
    "warm_anonymous_viewer_denied",
    EXPIRY_FULL_CHECK,
    UNRELATED_VIEWER_CHECK,
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
MOUNT_XORB_ACTION = "mount-xorb-read"
MOUNT_XORB_RANGE_WORKLOAD = "crab.cdn-mount-xorb-range-workload/v1"
MOUNT_XORB_RANGE_READ = "crab.cdn-mount-xorb-range-read/v1"
SDK_XORB_RANGE_WORKLOAD = "crab.cdn-sdk-xorb-range-workload/v1"
SDK_XORB_RANGE_READ = "crab.cdn-sdk-xorb-range-read/v1"
XORB_PRODUCT_DIR = "xorb-product"
XORB_WORKLOAD_FIELDS = {
    "schema", "configuration_sha256", "pointer_sha256", "payload_sha256", "payload_bytes", "crab_sha256",
    "global_prefix_sha256", "publication_sha256",
}
PUBLIC_CHECKS = {
    PUBLIC_EXPORT_CHECK, HISTORICAL_ORIGIN_CHECK, PUBLIC_TRANSITION_CHECK, "live_origin_denied",
    "export_origin_access", "public_listing_denied",
}
CLEANUP_INVENTORIES = {
    "cloudfront": ({"multipart_uploads": "empty", "versions_and_delete_markers": "empty"},),
    "cloud-cdn": ({"generations": "empty", "soft_deleted": "empty"},
                  {"generations": "empty", "soft_deleted": "disabled"}),
    "front-door": ({"blobs": "empty", "soft_deleted": "empty", "uncommitted_blobs": "empty"},
                   {"versions_and_snapshots": "empty", "uncommitted_blobs": "empty"}),
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
PRODUCT_VIEWER_RECORD = "viewer-requests.json"
SERVICE_PRODUCT_DIR = "service-product"
SERVICE_READ_ACTION = "service-http-read"
SERVICE_PUBLIC_PRIOR_RECORD = "before-export.json"
WORKLOAD_PAYLOAD_LIMIT = 512 * 1024 * 1024
SERVICE_WORKLOAD_REGISTRATION_FIELDS = {"sha256", "origin_sha256", "prefix_sha256", "etag_sha256", "payload_bytes"}
GIT_PRIOR_EXPORT_RECORD = "before-export.json"
DESKTOP_READ_RECORD = "read.json"
SDK_CLONE_FILES = {"edge": "clone-edge.json", "origin": "clone-origin.json"}
SDK_FETCH_FILES = {"edge": "fetch-edge.json", "origin": "fetch-origin.json"}
SDK_WORKTREE_VIEWER_FILES = {"clone": "clone-viewer-requests.json", "fetch": "fetch-viewer-requests.json"}
VIEWER_REQUEST_FILES = {"s3": "cloudfront-requests.json", "gcs": "cloud-cdn-requests.json", "azure": "front-door-requests.json"}
GIT_FETCH_RECORD_LIMIT = 256 * 1024
GIT_NATIVE_REF_FILES = {"published": "provider-published.json", "retired": "provider-retired.json"}
SERVICE_PRODUCT_FILES = {
    "publication": "publication.json", "retirement": "retirement.json", "edge": "edge.json",
    "viewer_requests": PRODUCT_VIEWER_RECORD, **GIT_NATIVE_REF_FILES,
}
GIT_NATIVE_REF_FIELDS = {
    "origin_sha256", "prefix_sha256", "ref_sha256", "repository_sha256", "epoch_sha256",
    "root_digest", "head_sha256", "revision_sha256", "transaction_id", "oid", "probe_source_sha",
}
GIT_PAYLOAD_SIZE = 256 * 1024
GIT_OID = re.compile(r"[0-9a-f]{40}")
GIT_TEST_REF = re.compile(r"refs/heads/crab-cdn-qualification-[1-9][0-9]{0,19}-[1-9][0-9]{0,19}-[0-9a-f]{12}")
CDN_PROVENANCE_FIELDS = {"source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt"}
GIT_PUBLICATION_FIELDS = CDN_PROVENANCE_FIELDS | {
    "schema", "ref", "oid", "sha256", "payload_bytes", "published", "visible_at_unix_ms", "candidate",
}
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
EDGE_RESPONSE_FIELDS = {
    "family", "status", "cache", "range", "object_bytes", "requested_bytes", "bytes", "complete", "elapsed_ms",
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
MANAGED_GIT_GRANT_FIELDS = MANAGED_ISSUER_FIELDS | {"repository_sha256", "grant_version"}
MANAGED_GIT_READ_ROLES = ("approved_v2", "v1_peer")
SDK_GIT_ACTION = "sdk-git-snapshot"
DESKTOP_GIT_ACTION = "desktop-git-read"
DIRECT_READ_TARGET_FIELDS = CLI_NATIVE_FIELDS | {"binding_sha256", "public_export_sha256"}
SDK_PAYLOAD_FIELDS = {"sha256", "bytes", "git_blob_oid_sha256"}
SDK_TIMING_FIELDS = {"started_at_unix_ms", "finished_at_unix_ms", "elapsed_ms"}
SDK_PROBE_FIELDS = SDK_TIMING_FIELDS | {"commit", "payload", "build", "warm_local"}
DIRECT_GIT_READ_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | SDK_PAYLOAD_FIELDS | SDK_TIMING_FIELDS | {
    "schema", "arm", "platform", "approval_sha256", "publication_sha256",
    "edge_responses", "native_traffic", "reader", "commit",
}
SDK_GIT_READ_FIELDS = DIRECT_GIT_READ_FIELDS | {"candidate"}
DESKTOP_GIT_READ_FIELDS = DIRECT_GIT_READ_FIELDS | {"candidate", "cache_root_sha256"}
SDK_HYDRATION_READ_FIELDS = PRODUCT_COUNTERS | CDN_PROVENANCE_FIELDS | SDK_TIMING_FIELDS | {
    "schema", "arm", "platform", "workload_sha256", "candidate", "crab_sha256", "pointer_sha256", "payload",
    "reader", "xorb_key_sha256", "native_traffic", "edge_responses",
}
MOUNT_XORB_READ_FIELDS = SDK_HYDRATION_READ_FIELDS | {"mounted_oid", "unmounted"}
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


class ReleaseReadLimits(NamedTuple):
    full_bytes: int
    ranged_object_bytes: int
    range_bytes: int
    timeout_ms: int

    def admits(self, response: dict[str, Any]) -> bool:
        if response["elapsed_ms"] > self.timeout_ms:
            return False
        if response["range"]:
            return (response["object_bytes"] <= self.ranged_object_bytes
                    and 0 < response["requested_bytes"] <= self.range_bytes)
        return 0 < self.full_bytes and response["object_bytes"] <= self.full_bytes


def release_read_limits(path: Path) -> dict[str, ReleaseReadLimits]:
    """Read the same exact scope and transfer limits that the Rust build embeds."""
    scopes = {}
    for line in path.read_bytes().decode("utf-8").replace("\r\n", "\n").split("\n"):
        if not line or line.startswith("#"):
            continue
        scope, *raw = line.split(" ")
        scope_parts(scope)
        require(scope not in scopes, f"duplicate CDN release scope: {scope}")
        require(len(raw) == 4 and all(re.fullmatch(r"0|[1-9][0-9]{0,19}", value) for value in raw),
                f"invalid CDN release read limits for {scope}")
        limits = ReleaseReadLimits(*map(int, raw))
        full, ranged_object, byte_range, timeout = limits
        require(0 < timeout < 2**32 and 0 <= byte_range <= ranged_object < 2**64
                and full < 2**64 and bool(full or byte_range) and bool(ranged_object) == bool(byte_range),
                f"invalid CDN release read limits for {scope}")
        scopes[scope] = limits
    return scopes


def release_scopes(path: Path) -> set[str]:
    return set(release_read_limits(path))


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


def verify_key_rotation(
    value: Any, report: dict[str, Any], edge: str, approved_policy: str, approved_overlap: str,
) -> tuple[str, int]:
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
        else:
            overlap_digest = digest
    overlap_start = policies["overlap"]["observed_at_unix_us"]
    require(all(grant["expires_at_unix"] * 1_000_000 - overlap_start <= MAX_VIEWER_GRANT_SECONDS * 1_000_000
                for grant in grants.values()), "rotation requires short-lived viewer grants")
    for component in deployment_component_names(edge, "private") - {ROTATING_POLICY[edge], "viewer-key-ids"}:
        require(policies["overlap"]["components"][component] == policies["retired"]["components"][component],
                "deployment changed outside viewer signing controls during rotation")
    require(overlap_digest == approved_overlap, "rotation overlap policy differs from the preregistered deployment")

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
    desktop = requires_desktop_workload(report["scope"], registration.get("workload"))
    service = is_direct_service_scope(report["scope"])
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
    if cli_workload or sdk or desktop or service:
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
        measured = derive_cli_public_transition(directory / GIT_PRODUCT_DIR, report, registration)
        require(trace == measured, "public transition must use the retained CLI product fetches")
    elif sdk or desktop:
        measured = derive_git_blob_public_transition(directory / GIT_PRODUCT_DIR, report, registration)
        require(trace == measured, f"public transition differs from the retained {'desktop' if desktop else 'SDK snapshot'} reads")
    elif service:
        measured = derive_service_public_transition(directory / SERVICE_PRODUCT_DIR, report, registration)
        require(trace == measured, "public transition differs from the retained service reads")


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
    *, product_reads: dict[str, bytes] | None = None,
) -> None:
    managed_git = is_managed_cli_git_scope(report["scope"])
    expected = CDN_PROVENANCE_FIELDS | {
        "schema", "platform", "issuer", "binding_sha256", "placement_sha256", "workload", "operations",
    }
    if managed_git:
        expected |= {"repository_sha256", "product_reads"}
    trace = fields(value, expected, "managed issuer trace")
    require(trace["schema"] == "crab.cdn-managed-issuer/v1"
            and all(trace[key] == report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "managed issuer trace must match the release row and workflow attempt")
    approved = registration["managed_issuer"]
    managed_clients = {}
    if managed_git:
        require(trace["repository_sha256"] == approved["repository_sha256"],
                "managed issuer trace used a different registered repository")
        observed = fields(trace["product_reads"], set(MANAGED_GIT_READ_ROLES), "managed Git product read digests")
        require(product_reads is not None and observed == {
            role: hashlib.sha256(raw).hexdigest() for role, raw in product_reads.items()
        }, "managed issuer trace must match the retained Git fetch and rollback")
        provenance = {key: report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}}
        provenance["candidate"] = registration["candidate"]
        managed_clients = {
            role: validate_git_fetch(raw, provenance, "edge" if role == "approved_v2" else "origin",
                                     "managed issuer product read", managed_issuer=approved)
            for role, raw in product_reads.items()
        }
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
            if managed_git:
                client = managed_clients[role]
                require(edge == client["observed_edge_gets"] and native == client["native_traffic"]["get_responses"],
                        "managed issuer traffic differs from the retained Git product read")
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
    expiry_grants = {}
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
                                   or is_direct_xorb_read_scope(report["scope"])
                                   or requires_desktop_workload(report["scope"], registration.get("workload"))
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
            rotation_key = verify_key_rotation(decoded, report, edge, deployment_policy_sha256,
                                               registration["rotation_overlap_policy_sha256"])
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
        if name in EXPIRY_CHECKS:
            trace_fields.update(EXPIRY_GRANT_FIELDS)
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
        if name in EXPIRY_CHECKS:
            request_fields.update(EXPIRY_TIMING_FIELDS)
        if name == EXPIRY_RANGE_CHECK:
            request_fields.add("range_offset")
        for index, value in enumerate(requests):
            expected = request_fields
            if name == UNRELATED_VIEWER_CHECK and 1 <= index <= 4:
                expected = request_fields | UNRELATED_REQUEST_FIELDS
            request = fields(value, expected, f"{name} request")
            require(request["source"] in ("edge", "origin")
                    and request["cache"] in ("hit", "miss", "unknown")
                    and type(request["status"]) is int and 100 <= request["status"] <= 599
                    and type(request["bytes"]) is int and request["bytes"] >= 0
                    and type(request["elapsed_ms"]) is int and request["elapsed_ms"] >= 0,
                    f"invalid trace observation: {name}")
            require(matches(SHA256, request["key_sha256"]), f"invalid trace key fingerprint: {name}")
        verify_trace_observations(name, requests)
        if name in EXPIRY_CHECKS:
            expiry = trace["viewer_expires_at_unix"]
            digest = trace["viewer_grant_sha256"]
            require(matches(SHA256, digest), "invalid expiry grant fingerprint")
            verify_window = verify_expiry_range_window if name == EXPIRY_RANGE_CHECK else verify_expiry_full_window
            verify_window(requests, expiry)
            expiry_grants[name] = (expiry, digest)
        if name in fixture_checks:
            fixture_reads[name] = requests
    if profile == "managed":
        product_reads = None
        if is_managed_cli_git_scope(report["scope"]):
            require(rollback is not None, "managed Git qualification requires its native rollback fetch")
            fetch = read_git_record(directory / GIT_PRODUCT_DIR / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT)
            product_reads = dict(zip(MANAGED_GIT_READ_ROLES, (fetch, rollback)))
        verify_managed_issuer(managed_issuer, report, registration, policy, product_reads=product_reads)
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
        expired = fixture_reads[EXPIRY_FULL_CHECK][0]
        require(fixture_reads[EXPIRY_RANGE_CHECK][0]["key_sha256"] == expired["key_sha256"],
                "expired parallel ranges must probe the same warm object")
        require(expiry_grants[EXPIRY_RANGE_CHECK] == expiry_grants[EXPIRY_FULL_CHECK],
                "full and ranged expiry checks must use the same signed viewer grant")
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
    if is_direct_xorb_read_scope(report["scope"]):
        target = scoped_placement["views"]["target"]
        require(scoped_placement["fixture_sha256"] == report["configuration_sha256"]
                and scoped_placement["deployment_policy_sha256"] == deployment_policy_sha256
                and target["key_sha256"] in native_reads and full_sizes[target["key_sha256"]] == target["bytes"],
                "xorb reads need independently verified transport and scope evidence for the same xorb")
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


def verify_expiry_full_window(requests: list[dict[str, Any]], expires_at: int) -> None:
    verify_warm_denial(EXPIRY_FULL_CHECK, requests)
    start, end = "started_at_unix_us", "finished_at_unix_us"
    warm, denied = requests
    require(positive(expires_at)
            and all(positive(row[start]) and positive(row[end]) and row[start] < row[end] for row in requests)
            and warm[end] < expires_at * 1_000_000
            and denied[start] >= (expires_at + EXPIRY_SETTLE_SECONDS) * 1_000_000,
            "full GET must hit the warm object before expiry and be denied after expiry")


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
    if name == UNRELATED_VIEWER_CHECK:
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
    digest = control_before["body_sha256"]
    require(matches(SHA256, digest) and control_after["body_sha256"] == digest
            and all(row["body_sha256"] is None for row in (denied, denied_range)),
            "unrelated viewer must verify identical control bytes on both reads")
    observations = requests[1:5]
    require(type(denied_range["range_offset"]) is int and denied_range["range_offset"] == 0
            and all(row["range_offset"] is None for row in (control_before, denied, control_after)),
            "unrelated viewer denials must include a full GET and an exact single-byte range")
    expires = control_before["viewer_expires_at_unix"]
    grant = control_before["viewer_grant_sha256"]
    require(positive(expires) and matches(SHA256, grant)
            and all(type(row["viewer_expires_at_unix"]) is int and row["viewer_expires_at_unix"] == expires
                    and row["viewer_grant_sha256"] == grant for row in observations),
            "unrelated viewer must use the same signed grant for both controls and both denials")
    previous_finish = 0
    for row in observations:
        started, finished = row["started_at_unix_us"], row["finished_at_unix_us"]
        require(positive(started) and positive(finished)
                and previous_finish <= started < finished < expires * 1_000_000,
                "unrelated viewer probes must be ordered and finish before the signed grant expires")
        previous_finish = finished


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
    for role, row in zip(CACHE_REUSE_ROLES, requests, strict=True):
        previous_finish = verify_cache_reuse_read(role, principals[role], obj, grants[role], row, previous_finish)
    require(grants["primary"]["expires_at_unix"] != grants["second"]["expires_at_unix"],
            "cache reuse needs distinct signed viewer grants with different expirations")
    return obj["key_sha256"], obj["bytes"]


def verify_cache_reuse_read(
    role: str, principal: dict[str, str], obj: dict[str, Any], grant: Any, row: Any, previous_finish: int = 0,
) -> int:
    fields(grant, {"subject_sha256", "policy_identity_sha256", "issued_at_unix_ms", "expires_at_unix"},
           "authenticated cache reuse grant")
    require(all(grant[key] == principal[key] for key in principal)
            and positive(grant["issued_at_unix_ms"]) and positive(grant["expires_at_unix"]),
            "cache reuse requires an authenticated grant for each approved principal")
    fields(row, TRACE_REQUEST_FIELDS | {"principal", "body_sha256", "started_at_unix_ms", "finished_at_unix_ms"},
           "authenticated cache reuse read")
    cache = "miss" if role == "primary" else "hit"
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
    return finish


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


def verify_product_action(value: Any, family: str, *, xorb_read: bool = False) -> None:
    required = {
        "name", "expected_sha256", "observed_sha256", "expected_bytes", "observed_bytes",
        "native_writes", "observed_edge_gets", "visible_new_ref", "fixture_cleanup_verified",
    }
    verified_payload_required = family in {"git", "capsule", "lfs"} or xorb_read
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


def cli_payload_bytes(configuration: dict[str, Any]) -> int:
    size = configuration.get("cli_payload_bytes", GIT_PAYLOAD_SIZE)
    require(positive(size) and size <= WORKLOAD_PAYLOAD_LIMIT, "invalid CLI qualification payload size")
    return size


def benchmark_registration(registration: Any) -> dict[str, Any]:
    expected = {
        "schema", "source_sha", "scope", "configuration_sha256", "workload", "samples", "p95_budget_ms",
        "cost_budget_microusd", "regions", "deployment_policy_sha256",
    }
    require(isinstance(registration, dict) and isinstance(registration.get("scope"), str), "benchmark needs a CDN scope")
    _, access, _, _, profile = scope_parts(registration["scope"])
    if requires_cli_workload(registration["scope"]):
        expected.update(("candidate", "viewer_binding_sha256"))
        expected |= {"cli_payload_bytes"} & registration.keys()
        cli_payload_bytes(registration)
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
    # A transport canary can report a fixture summary. Only the desktop
    # product workload can feed the regional raw evidence collector.
    if requires_desktop_workload(registration["scope"], registration.get("workload")):
        expected.update(("candidate", "desktop_candidate", "desktop_native_target", "viewer_binding_sha256"))
        verify_read_candidate(registration.get("candidate"), registration.get("source_sha"))
        verify_read_candidate(registration.get("desktop_candidate"), registration.get("source_sha"), consumer="desktop sidecar")
        target = fields(registration.get("desktop_native_target"), CLI_NATIVE_FIELDS, "registered desktop native target")
        require(matches(SHA256, registration.get("viewer_binding_sha256"))
                and all(matches(SHA256, value) for value in target.values()),
                "desktop benchmark needs the read workload and approved native target and viewer binding")
        if access == "private":
            expected.add("viewer_approval_sha256")
            require(matches(SHA256, registration.get("viewer_approval_sha256")),
                    "private desktop benchmark needs the approved issuer binding")
    if is_direct_xorb_read_scope(registration["scope"]):
        mount = is_direct_mount_xorb_scope(registration["scope"])
        expected.update(("candidate" if mount else "sdk_candidate", "viewer_binding_sha256", "viewer_approval_sha256",
                         "hydration_workload_sha256", "sparse_workload_sha256"))
        if mount:
            verify_read_candidate(registration.get("candidate"), registration.get("source_sha"))
        else:
            verify_sdk_candidate(registration.get("sdk_candidate"), registration.get("source_sha"))
        require(registration.get("workload") == (MOUNT_XORB_ACTION if mount else SDK_HYDRATION_ACTION)
                and all(matches(SHA256, registration.get(key)) for key in
                        ("viewer_binding_sha256", "viewer_approval_sha256", "hydration_workload_sha256",
                         "sparse_workload_sha256")),
                "xorb read benchmark needs the approved full and sparse workloads and viewer binding")
    if is_direct_service_scope(registration["scope"]):
        expected.update(("service_workload", "viewer_binding_sha256", "service_candidate"))
        verify_service_candidate(registration.get("service_candidate"), registration.get("source_sha"))
        workload = fields(registration.get("service_workload"), SERVICE_WORKLOAD_REGISTRATION_FIELDS,
                          "registered service HTTP workload")
        require(registration.get("workload") == SERVICE_READ_ACTION
                and matches(SHA256, registration.get("viewer_binding_sha256"))
                and all(matches(SHA256, workload[key]) for key in SERVICE_WORKLOAD_REGISTRATION_FIELDS - {"payload_bytes"})
                and positive(workload["payload_bytes"]) and workload["payload_bytes"] <= WORKLOAD_PAYLOAD_LIMIT,
                "service benchmark needs the approved HTTP workload and viewer binding")
        if access == "public":
            expected.add("service_prior_workload_sha256")
            prior = registration.get("service_prior_workload_sha256")
            require(matches(SHA256, prior) and prior != workload["sha256"],
                    "public service benchmark needs the distinct prior HTTP workload")
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
        expected.update(("scoped_view_sha256", "cache_reuse_sha256", "rotation_overlap_policy_sha256"))
        require(matches(SHA256, registration.get("scoped_view_sha256")),
                "private benchmark needs the approved scoped view control digest")
        require(matches(SHA256, registration.get("cache_reuse_sha256")),
                "private benchmark needs the approved cache reuse authority digest")
        overlap = registration.get("rotation_overlap_policy_sha256")
        require(matches(SHA256, overlap) and overlap != registration.get("deployment_policy_sha256"),
                "private benchmark needs a distinct approved rotation overlap policy digest")
    if profile == "managed":
        expected.add("managed_issuer")
        issuer_fields = MANAGED_ISSUER_FIELDS
        if is_managed_cli_git_scope(registration["scope"]):
            issuer_fields = issuer_fields | {"repository_sha256"}
        issuer = fields(registration.get("managed_issuer"), issuer_fields, "registered production issuer")
        require(matches(SHA256, issuer["identity_sha256"]) and matches(LABEL, issuer["version"])
                and matches(SHA256, issuer["approval_sha256"])
                and ("repository_sha256" not in issuer or matches(SHA256, issuer["repository_sha256"])),
                "managed benchmark needs an approved issuer, repository and binding receipt")
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
    return (consumer == "cli" and family in {"git", "capsule", "lfs"} and profile == "direct"
            or is_managed_cli_git_scope(scope))


def is_managed_cli_git_scope(scope: str) -> bool:
    return scope_parts(scope)[1:] == ("private", "cli", "git", "managed")


def is_direct_sdk_git_scope(scope: str) -> bool:
    return scope_parts(scope)[2:] == ("sdk", "git", "direct")


def is_direct_desktop_git_scope(scope: str) -> bool:
    return scope_parts(scope)[2:] == ("desktop", "git", "direct")


def requires_desktop_workload(scope: str, workload: Any) -> bool:
    return workload == DESKTOP_GIT_ACTION and is_direct_desktop_git_scope(scope)


def is_direct_sdk_hydration_scope(scope: str) -> bool:
    return scope_parts(scope)[1:] == ("private", "sdk", "xorb", "direct")


def is_direct_mount_xorb_scope(scope: str) -> bool:
    return scope_parts(scope)[1:] == ("private", "mount", "xorb", "direct")


def is_direct_xorb_read_scope(scope: str) -> bool:
    return is_direct_sdk_hydration_scope(scope) or is_direct_mount_xorb_scope(scope)

def is_direct_service_scope(scope: str) -> bool:
    _, _, consumer, family, profile = scope_parts(scope)
    return consumer in ("server", "s3-gateway") and family in ("git", "lfs") and profile == "direct"


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
            and positive(record["payload_bytes"]) and record["payload_bytes"] <= WORKLOAD_PAYLOAD_LIMIT
            and type(record["published"]) is bool
            and (positive(record["visible_at_unix_ms"]) if record["published"] else record["visible_at_unix_ms"] is None),
            "Git publication receipt does not match this qualification run")
    verify_read_candidate(record["candidate"], provenance["source_sha"], provenance.get("platform"))
    return record


def validate_git_fetch(
    raw: bytes, provenance: dict[str, Any], mode: str, label: str, *, managed_issuer: dict[str, Any] | None = None,
) -> dict[str, Any]:
    action = cli_action_name(provenance["scope"])
    edge, access, _, _, _ = scope_parts(provenance["scope"])
    value = decode(raw)
    expected = GIT_FETCH_FIELDS
    prior = mode == "prior_export"
    managed = is_managed_cli_git_scope(provenance["scope"])
    if managed:
        expected = expected | {"managed_grant"}
    if prior or (isinstance(value, dict) and "transition_key_sha256" in value):
        require(access == "public" and mode in ("edge", "prior_export") and isinstance(value, dict)
                and matches(SHA256, value.get("transition_key_sha256")),
                "CLI transition needs an exact public object fingerprint")
        expected = expected | {"transition_key_sha256"}
    client = fields(value, expected, "redacted LFS fetch" if action == "lfs-fetch" else "redacted Git fetch")
    require(client["schema"] == f"crab.cdn-{action}/v1" and client["name"] == action
            and client["arm"] == mode and all(client[key] == value for key, value in provenance.items() if key != "candidate"),
            f"{label} has mismatched Git fetch provenance")
    verify_read_candidate(client["candidate"], provenance["source_sha"], client["platform"])
    reader = fields(client["reader"], CLI_READ_FIELDS, "verified CLI read configuration")
    require(all(matches(SHA256, reader[key]) for key in CLI_NATIVE_FIELDS | {"cache_root_sha256"})
            and (matches(SHA256, reader["binding_sha256"])
                 or (mode == "origin" and reader["binding_sha256"] == "")),
            f"{label} has an invalid CDN binding, native target or local cache fingerprint")
    require(matches(SHA256, reader["public_export_sha256"]) if mode != "origin" and access == "public"
            else reader["public_export_sha256"] == "",
            f"{label} has an invalid public export approval")
    if "candidate" in provenance:
        require(client["candidate"] == provenance["candidate"], f"{label} did not run the registered CLI build")
    if managed:
        grant = fields(client["managed_grant"], MANAGED_GIT_GRANT_FIELDS, "managed Git read grant")
        origin = mode == "origin"
        require(matches(SHA256, grant["identity_sha256"]) and matches(SHA256, grant["repository_sha256"])
                and matches(LABEL, grant["version"]) and type(grant["grant_version"]) is int
                and grant["grant_version"] == (1 if origin else 2)
                and (grant["approval_sha256"] is None if origin else matches(SHA256, grant["approval_sha256"])),
                f"{label} did not observe the expected managed grant version and approval")
        if managed_issuer is not None:
            expected_grant = managed_issuer | {"grant_version": 1 if origin else 2}
            if origin:
                expected_grant["approval_sha256"] = None
            require(grant == expected_grant, f"{label} used a different registered managed issuer, repository or approval")
    require(matches(GIT_OID, client["publication_oid"]) and matches(SHA256, client["publication_sha256"])
            and matches(SHA256, client["expected_sha256"]) and client["expected_sha256"] == client["observed_sha256"]
            and positive(client["expected_bytes"]) and positive(client["observed_bytes"])
            and client["expected_bytes"] == client["observed_bytes"] <= WORKLOAD_PAYLOAD_LIMIT
            and positive(client["native_writes"]) and client["visible_new_ref"] is True,
            f"{label} did not verify the published payload")
    verify_git_traffic(client, edge, label)
    native = client["native_traffic"]
    if mode == "origin":
        require(reader["binding_sha256"] == "" and all(client[key] == 0 for key in PRODUCT_COUNTERS),
                f"{label} requires an inactive viewer binding and zero edge or local payload reads")
    if prior:
        require(all(client[key] == 0 for key in PRODUCT_COUNTERS), f"{label} requires zero edge or local payload reads")
    if mode == "origin" or prior:
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


def verify_edge_response(response: dict[str, Any]) -> None:
    status, ranged, requested, size = (
        response["status"], response["range"], response["requested_bytes"], response["object_bytes"],
    )
    require(isinstance(response["family"], str) and response["family"] in FAMILIES
            and type(status) is int and (status == 0 or 100 <= status <= 599)
            and response["cache"] in ("hit", "miss", "unknown")
            and type(response["complete"]) is bool and type(ranged) is bool
            and all(type(response[key]) is int and 0 <= response[key] < 2**64
                    for key in ("bytes", "elapsed_ms", "object_bytes", "requested_bytes"))
            and (0 < requested <= size if ranged else requested == size)
            and (status != 0 or response["bytes"] == 0),
            "invalid CDN transfer observation")
    if response["complete"]:
        require(status == (206 if ranged else 200) and response["bytes"] == requested,
                "inconsistent completed CDN response")


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
        response = fields(evidence, EDGE_RESPONSE_FIELDS | {"scope"}, "CDN viewer response")
        verify_edge_response(response)
        require(matches(SHA256, request_id) and response["status"] != 0, f"{label} has an invalid CDN viewer response")
        require(isinstance(response["scope"], str), f"{label} has an invalid CDN viewer scope")
        observed_edge, _, _, observed_family, _ = scope_parts(response["scope"])
        require(observed_edge == edge and observed_family == response["family"],
                f"{label} has a CDN viewer scope inconsistent with its provider or family")
    require(client["edge_hits"] + client["edge_misses"] + client["edge_unknown"] == client["observed_edge_gets"]
            <= client["edge_attempts"] <= client["all_edge_attempts"], f"{label} has inconsistent edge counts")


def verify_compiled_edge_limits(
    client: dict[str, Any], policies: dict[str, ReleaseReadLimits], label: str,
) -> None:
    responses = client["edge_responses"].values()
    for response in responses:
        scope = response["scope"]
        require(scope in policies, f"{label} contacted the CDN outside its compiled release scopes: {scope}")
        require(policies[scope].admits(response), f"{label} disagrees with its compiled CDN read limits: {scope}")
    require(not client["observed_edge_gets"]
            or any(row["scope"] == client["scope"] and row["complete"] and row["bytes"] for row in responses),
            f"{label} reports edge delivery without a completed response for {client['scope']}")


def validate_desktop_git_read(value: Any, scope: str, arm: str) -> dict[str, Any]:
    edge, access, _, _, _ = scope_parts(scope)
    expected = DESKTOP_GIT_READ_FIELDS | ({"primer_sha256"} if arm == "warm_local" else set())
    transition = isinstance(value, dict) and "transition_key_sha256" in value
    if transition:
        expected.add("transition_key_sha256")
    client = fields(value, expected, "desktop Git read")
    require(is_direct_desktop_git_scope(scope) and client["scope"] == scope
            and (arm in ("edge", "origin", "warm_local") or (arm == "prior_export" and transition))
            and client["schema"] == "crab.cdn-desktop-git-read/v1" and client["arm"] == arm
            and matches(COMMIT, client["source_sha"]) and matches(LABEL, client["platform"])
            and all(matches(RUN_NUMBER, client[key]) for key in ("workflow_run_id", "workflow_run_attempt")),
            "desktop Git read has mismatched product or workflow provenance")
    if transition:
        require(access == "public" and arm in ("edge", "prior_export") and matches(SHA256, client["transition_key_sha256"]),
                "desktop transition needs the exact public pack-layer fingerprint")
    verify_read_candidate(client["candidate"], client["source_sha"], client["platform"], consumer="desktop sidecar")
    require(matches(GIT_OID, client["commit"]) and type(client["bytes"]) is int and client["bytes"] == GIT_PAYLOAD_SIZE
            and all(matches(SHA256, client[key]) for key in (
                "sha256", "git_blob_oid_sha256", "publication_sha256", "configuration_sha256",
                "cache_root_sha256",
            )), "desktop read did not verify the published Git payload")
    reader = fields(client["reader"], DIRECT_READ_TARGET_FIELDS, "desktop read target")
    require(all(matches(SHA256, reader[key]) for key in CLI_NATIVE_FIELDS), "invalid desktop native placement")
    verify_git_traffic(client, edge, "desktop Git read")
    if arm != "origin":
        require(matches(SHA256, reader["binding_sha256"]) and matches(SHA256, client["approval_sha256"])
                and reader["public_export_sha256"] == (client["approval_sha256"] if access == "public" else ""),
                "desktop edge read requires the approved binding")
    if arm == "warm_local":
        require(matches(SHA256, client["primer_sha256"]) and positive(client["local_payload_hits"])
                and all(client[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "desktop repeat requires its own primer and a verified local Git blob without edge traffic")
    elif arm == "edge":
        require(all(positive(client[key]) for key in ("observed_edge_gets", "verified_edge_payloads", "edge_bytes"))
                and client["local_payload_hits"] == 0
                # A fixture build may serve related immutable metadata through the edge.
                # Git counters prove the payload; every request still needs a provider log join.
                and client["all_edge_attempts"] == len(client["edge_responses"]),
                "desktop edge read requires verified Git delivery and a fingerprint for every viewer attempt")
    else:
        native = client["native_traffic"]
        require(arm == "prior_export"
                or reader["binding_sha256"] == reader["public_export_sha256"] == client["approval_sha256"] == "",
                "desktop origin control requires the withdrawn binding")
        require(all(client[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "desktop native control requires native payload reads without edge or cached payload activity")
    return client


def verify_desktop_git_read(raw: bytes, provenance: dict[str, Any], spec: dict[str, Any], arm: str) -> dict[str, Any]:
    client = validate_desktop_git_read(decode(raw), spec["scope"], arm)
    require(all(client[key] == provenance[key] for key in CDN_PROVENANCE_FIELDS | {"platform"})
            and client["candidate"] == spec["desktop_candidate"],
            "desktop read did not run the registered desktop build for this workflow")
    binding = "" if arm == "origin" else spec["viewer_binding_sha256"]
    export_key = "previous_publication_sha256" if arm == "prior_export" else "publication_sha256"
    export = "" if arm == "origin" else spec.get(export_key, "")
    approval = "" if arm == "origin" else spec.get("viewer_approval_sha256", export)
    reader = spec["desktop_native_target"] | {"binding_sha256": binding, "public_export_sha256": export}
    require(client["reader"] == reader
            and client["approval_sha256"] == approval, "desktop read changed its registered target or viewer approval")
    return client


def validate_service_read(value: Any, provenance: dict[str, Any], arm: str) -> dict[str, Any]:
    local = arm == "warm_local"
    prior = arm == "prior_export"
    transition = isinstance(value, dict) and "transition_key_sha256" in value
    edge, access, _, family, _ = scope_parts(provenance["scope"])
    expected = SERVICE_READ_FIELDS | ({"primer_sha256"} if local else set())
    if family == "lfs":
        expected.add("cache_root_sha256")
    if transition:
        expected.add("transition_key_sha256")
    client = fields(value, expected, "CDN service read")
    require(is_direct_service_scope(provenance["scope"])
            and (arm in ("edge", "origin", "warm_local") or (prior and transition))
            and client["schema"] == "crab.cdn-service-read/v1" and client["arm"] == arm
            and all(client[key] == provenance[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "CDN service read has mismatched workflow or product provenance")
    verify_service_candidate(client["candidate"], provenance["source_sha"], provenance["platform"])
    require(matches(GIT_OID, client["commit"]) and positive(client["payload_bytes"])
            and client["payload_bytes"] <= WORKLOAD_PAYLOAD_LIMIT
            and all(matches(SHA256, client[key]) for key in ("workload_sha256", "publication_sha256",
                    "native_published_sha256", "payload_sha256", "etag_sha256", "service_instance_sha256")),
            "CDN service read has invalid publication or payload fingerprints")
    git, lfs = client["git_blob_oid_sha256"], client["lfs_key_sha256"]
    require((matches(SHA256, git) and lfs is None) if family == "git" else (matches(SHA256, lfs) and git is None),
            "CDN service read needs the exact Git blob or LFS object fingerprint")
    if family == "lfs":
        root = client["cache_root_sha256"]
        require(matches(SHA256, root) if arm in ("edge", "warm_local") else root == "",
                "service LFS read has an invalid selected cache root")
    if transition:
        key = client["transition_key_sha256"]
        require(access == "public" and arm in ("prior_export", "edge") and matches(SHA256, key)
                and (family != "lfs" or key == lfs), "service transition needs the exact public object fingerprint")
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
                "service warm-local read requires a verified cached payload without edge traffic")
    elif arm == "origin" or prior:
        native = client["native_traffic"]
        require(all(client[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "service native control requires native body reads and zero edge attempts")
    else:
        require(all(positive(client[key]) for key in ("observed_edge_gets", "edge_bytes", "verified_edge_payloads"))
                and client["local_payload_hits"] == 0
                and client["edge_attempts"] == client["all_edge_attempts"] == len(client["edge_responses"]),
                "service read requires verified edge delivery and a fingerprint for every viewer attempt")
    return client


def verify_service_repeat(local: dict[str, Any], primer: dict[str, Any], primer_sha256: str) -> None:
    require(local["primer_sha256"] == primer_sha256 and all(local[key] == primer[key] for key in
                ("candidate", "service_instance_sha256", "publication_sha256", "native_published_sha256", "workload_sha256",
                 "reader", "commit", "payload_sha256", "payload_bytes", "etag_sha256",
                 "git_blob_oid_sha256", "lfs_key_sha256"))
            and primer["finished_at_unix_ms"] <= local["started_at_unix_ms"],
            "service warm-local read must repeat its own edge primer in the same process")
    require(local.get("cache_root_sha256") == primer.get("cache_root_sha256"),
            "service warm-local LFS read must reuse its own edge primer's cache root")


def validate_sdk_git_record(value: Any, access: str, mode: str, *,
                            worktree_action: str | None = None) -> dict[str, Any]:
    expected_fields = SDK_GIT_READ_FIELDS | ({"primer_sha256"} if mode == "warm_local" else set())
    if worktree_action:
        expected_fields |= {"crab_sha256", "installed_packs", "verified_edge_packs"}
    if worktree_action == "fetch":
        expected_fields |= {"previous_head", "head_after_fetch"}
    transition = isinstance(value, dict) and "transition_key_sha256" in value
    if transition:
        expected_fields.add("transition_key_sha256")
    client = fields(value, expected_fields, "redacted SDK Git snapshot")
    require(mode != "prior_export" or transition, "SDK prior export needs the verified transition object")
    if transition:
        require(worktree_action is None and access == "public" and mode in ("edge", "prior_export")
                and matches(SHA256, client["transition_key_sha256"]),
                "SDK transition needs an exact public pack-layer fingerprint")
    schema = f"crab.cdn-sdk-git-{worktree_action or 'read'}/v1"
    require(client["schema"] == schema and client["arm"] == mode, "SDK snapshot has mismatched product or workflow provenance")
    return client


def verify_sdk_git_read(raw: bytes, row: dict[str, Any], spec: dict[str, Any], mode: str, *,
                        worktree_action: str | None = None) -> dict[str, Any]:
    local = mode == "warm_local"
    prior = mode == "prior_export"
    edge, access, _, _, _ = scope_parts(row["scope"])
    client = validate_sdk_git_record(decode(raw), access, mode, worktree_action=worktree_action)
    require(all(client[key] == row[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}),
            "SDK snapshot has mismatched product or workflow provenance")
    verify_sdk_candidate(client["candidate"], row["source_sha"], row["platform"])
    require(client["candidate"] == spec["sdk_candidate"], "SDK snapshot did not run the registered SDK build")
    if worktree_action:
        require(mode in ("edge", "origin")
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


class ViewerRequestMetrics(NamedTuple):
    request_ids: set[str]
    origin_gets: int
    fill_bytes: int
    successful_fill: bool
    completed_cache: dict[str, str]


def verify_viewer_requests(
    value: Any, client: dict[str, Any], client_digest: str, edge: str, *, include_adjacent_origin: bool = False,
) -> ViewerRequestMetrics:
    keyed_origin = edge in ("cloudfront", "front-door")
    xorb_key = client.get("xorb_key_sha256")
    lfs_key = client.get("lfs_key_sha256")
    service = client.get("schema") == "crab.cdn-service-read/v1"
    transition_key = client.get("transition_key_sha256")
    verified_transition = False
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
    record_fields = {"status", "cache", "response_bytes", "key_sha256"}
    if edge == "cloud-cdn":
        record_fields |= {"cache_fill_bytes", "origin_requests"}
    viewer_keys = set()
    logged_ids = set(records)
    origin_gets_by_key: dict[str, int] = defaultdict(int)
    viewer_fill_by_key: dict[str, int] = defaultdict(int)
    origin_fill_by_key: dict[str, int] = defaultdict(int)
    successful_fill = False
    completed_cache = {}
    observed_transfer = dict.fromkeys(("edge_hits", "edge_misses", "edge_unknown", "edge_bytes", "incomplete_edge_bytes"), 0)
    for request_id, record in records.items():
        row = fields(record, record_fields, "CDN viewer log record")
        response = expected[request_id]
        selected = xorb_key is None or response["family"] == "xorb"
        served_payload = response["status"] in (200, 206) and response["complete"] and response["bytes"] > 0
        require(type(row["status"]) is int and row["status"] == response["status"]
                and row["cache"] in ("hit", "miss", "unknown")
                and type(row["response_bytes"]) is int and row["response_bytes"] >= response["bytes"],
                "CDN log status or response bytes disagree with the client")
        cache = row["cache"]
        key = row["key_sha256"]
        require(matches(SHA256, key), "invalid CDN object key fingerprint")
        require(xorb_key is None or (key == xorb_key) == selected,
                "CDN viewer request did not read the approved hydration xorb")
        require(lfs_key is None or key == lfs_key, "CDN viewer request did not read the approved service LFS object")
        viewer_keys.add(key)
        verified_transition |= key == transition_key and served_payload
        if (xorb_key is not None or service) and selected:
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
            supplied_from_origin = positive(row["cache_fill_bytes"])
            viewer_fill_by_key[key] = max(viewer_fill_by_key[key], row["cache_fill_bytes"])
            children = row["origin_requests"]
            require(isinstance(children, dict), "invalid Cloud CDN origin requests")
            for origin_id, value in children.items():
                origin = fields(value, {"method", "status", "cache_fill_bytes"}, "Cloud CDN origin request")
                require(matches(SHA256, origin_id) and origin_id not in logged_ids
                        and origin["method"] in ("GET", "HEAD") and type(origin["status"]) is int and 100 <= origin["status"] <= 599
                        and type(origin["cache_fill_bytes"]) is int and origin["cache_fill_bytes"] >= 0,
                        "invalid or repeated Cloud CDN origin request")
                logged_ids.add(origin_id)
                origin_gets_by_key[key] += origin["method"] == "GET"
                origin_fill_by_key[key] += origin["cache_fill_bytes"]
                supplied_from_origin |= (origin["method"] == "GET" and origin["status"] in (200, 206)
                                         and positive(origin["cache_fill_bytes"]))
            if served_payload:
                # Cloud CDN can log byte_range_caching for a request whose
                # missing ranges were filled by linked backend GETs.
                successful_fill |= selected and supplied_from_origin
                if supplied_from_origin and cache == "hit":
                    cache = "miss"
                elif children and cache == "hit":
                    cache = "unknown"
        if response["complete"] and response["cache"] != "unknown":
            require(response["cache"] in (row["cache"], cache), "CDN log cache result disagrees with the completed response")
        if selected and served_payload and cache in ("hit", "miss"):
            completed_cache[request_id] = cache
    if xorb_key is not None or service:
        attempts = sum(response["family"] == "xorb" for response in expected.values()) if xorb_key is not None else len(records)
        require(client["edge_attempts"] == attempts and all(client[key] == count for key, count in observed_transfer.items()),
                "CDN transfer totals disagree with the logged responses")
    require(transition_key is None or verified_transition, "CDN logs did not confirm the new export object")
    if keyed_origin:
        service, viewer, timestamp, precision = (
            ("S3", "CloudFront", "at_unix_s", 1000) if edge == "cloudfront"
            else ("Azure Blob", "Front Door", "at_unix_ms", 1)
        )
        origins = proof["origin_requests"]
        require(isinstance(origins, dict), f"invalid {service} origin requests")
        start, finish = (client[key] // precision for key in ("started_at_unix_ms", "finished_at_unix_ms"))
        origin_keys = set()
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
            key = origin["key_sha256"]
            origin_keys.add(key)
            origin_gets_by_key[key] += 1
            if origin["status"] in (200, 206):
                origin_fill_by_key[key] += origin["response_body_bytes"]
                successful_fill |= (xorb_key is None or key == xorb_key) and positive(origin["response_body_bytes"])
        # These providers join origin GETs by key and time. If a key touched
        # origin in this arm, its viewer hits alone cannot prove a warm read.
        completed_cache = {request_id: cache for request_id, cache in completed_cache.items()
                           if cache != "hit" or records[request_id]["key_sha256"] not in origin_keys}
    # Xorb fills prove payload delivery. Benchmarks also charge for origin
    # traffic from adjacent reads, without using it as xorb cache evidence.
    keys = viewer_keys if xorb_key is None or include_adjacent_origin else {xorb_key}
    origin_gets = sum(origin_gets_by_key[key] for key in keys)
    # Cloud CDN can report one fill at both levels or collapse same-key
    # requests. Distinct keys cannot share a fill and add to the floor.
    fill_bytes = sum(max(viewer_fill_by_key[key], origin_fill_by_key[key]) for key in keys)
    return ViewerRequestMetrics(logged_ids, origin_gets, fill_bytes, successful_fill, completed_cache)


def verify_product_viewer_requests(
    raw: bytes, client: dict[str, Any], client_raw: bytes, edge: str, *,
    policies: dict[str, ReleaseReadLimits] | None = None,
) -> set[str]:
    if policies is not None:
        verify_compiled_edge_limits(client, policies, "CDN product read")
    verify_viewer_requests(decode(raw), client, hashlib.sha256(client_raw).hexdigest(), edge)
    require(any(response["status"] in (200, 206) and response["complete"] and response["bytes"] > 0
                for response in client["edge_responses"].values()), "provider logs confirm no completed product response")
    # S3 logs have second precision and can associate a shared origin fill
    # with distinct viewer requests. Deduplicate product reads by viewer ID.
    return set(client["edge_responses"])


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
    cleanup_at: int, native_origin_sha256: str, policies: dict[str, ReleaseReadLimits] | None = None,
) -> dict[str, Any]:
    publication_raw = records["publication"]
    publication = validate_git_publication(publication_raw, row)
    publication_sha = hashlib.sha256(publication_raw).hexdigest()
    require(publication["published"], "service lifecycle requires a confirmed native publication")
    edge = validate_service_read(decode(records["edge"]), row, "edge")
    origin = validate_service_read(decode(records["origin"]), row, "origin")
    provider, access, _, family, _ = scope_parts(row["scope"])
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
        require(read["payload_bytes"] == workload["payload_bytes"] == publication["payload_bytes"]
                and read["etag_sha256"] == workload["etag_sha256"],
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
    verify_product_viewer_requests(records["viewer_requests"], edge, records["edge"], provider, policies=policies)
    retired_at = verify_git_retirement(
        records["retirement"], row, publication_sha, publication["candidate"], origin["finished_at_unix_ms"],
    )
    earliest = edge
    if access == "public":
        earliest = verify_service_transition_reads(records["before_export"], edge, row, spec)
        require(earliest["service_instance_sha256"] != origin["service_instance_sha256"],
                "prior export and rollback must use separate service processes")
    verify_git_lifecycle(records, row, publication, publication_sha, earliest["started_at_unix_ms"], edge["reader"],
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
    policies: dict[str, ReleaseReadLimits] | None = None,
) -> dict[str, Any]:
    paths = SERVICE_PRODUCT_FILES
    if scope_parts(row["scope"])[1] == "public":
        paths = paths | {"before_export": SERVICE_PUBLIC_PRIOR_RECORD}
    records = {name: read_git_record(inputs / filename, limit=GIT_FETCH_RECORD_LIMIT if name in ("edge", "viewer_requests")
                                    else 8 * 1024) for name, filename in paths.items()}
    result = verify_service_lifecycle(records | {"origin": rollback_raw}, row, spec,
                                      cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256, policies=policies)
    if retain is not None:
        retain.mkdir()
        for name, filename in paths.items():
            (retain / filename).write_bytes(records[name])
    return result


def validate_xorb_workload(raw: bytes, approval: str, configuration: str, *, mount: bool = False,
                          allow_sparse: bool = False) -> dict[str, Any]:
    require(matches(SHA256, approval) and hashlib.sha256(raw).hexdigest() == approval,
            "hydration workload differs from the approved digest")
    expected = XORB_WORKLOAD_FIELDS | ({"mounted_oid"} if mount else set())
    value = decode(raw)
    schema = "crab.cdn-mount-xorb-workload/v1" if mount else "crab.cdn-sdk-hydration-workload/v1"
    sparse_schema = MOUNT_XORB_RANGE_WORKLOAD if mount else SDK_XORB_RANGE_WORKLOAD
    sparse = allow_sparse and isinstance(value, dict) and value.get("schema") == sparse_schema
    if sparse:
        expected.add("read_range")
        if not mount:
            expected |= {"projected_oid", "sdk_sha256"}
        schema = sparse_schema
    spec = fields(value, expected, "xorb read workload")
    require(spec["schema"] == schema
            and spec["configuration_sha256"] == configuration
            and (not mount or matches(GIT_OID, spec["mounted_oid"]))
            and all(matches(SHA256, spec[name]) for name in
                    ("pointer_sha256", "payload_sha256", "crab_sha256", "global_prefix_sha256", "publication_sha256"))
            and positive(spec["payload_bytes"]) and spec["payload_bytes"] <= 512 * 1024 * 1024,
            "invalid approved xorb read workload")
    if sparse:
        window = fields(spec["read_range"], {"offset", "bytes", "sha256"}, "approved xorb file range")
        require((mount or (matches(GIT_OID, spec["projected_oid"]) and matches(SHA256, spec["sdk_sha256"])))
                and type(window["offset"]) is int and window["offset"] >= 0
                and positive(window["bytes"]) and window["bytes"] < spec["payload_bytes"]
                and window["offset"] + window["bytes"] <= spec["payload_bytes"]
                and matches(SHA256, window["sha256"]), "invalid approved xorb file range")
    return spec


def verify_sparse_xorb_traffic(read: dict[str, Any], object_bytes: int) -> None:
    # Related catalog and shard requests still need a provider log join.
    # Only xorb responses can prove the file window was sparse.
    transfers = [response for response in read["edge_responses"].values() if response["scope"] == read["scope"]]
    require(0 < read["edge_bytes"] == sum(response["bytes"] for response in transfers) < object_bytes
            and read["edge_attempts"] == read["observed_edge_gets"] == len(transfers)
            and read["all_edge_attempts"] == len(read["edge_responses"])
            and positive(read["verified_edge_payloads"]) and read["local_payload_hits"] == 0
            and all(response["range"] and response["complete"] and response["object_bytes"] == object_bytes
                    for response in transfers), "sparse read must verify xorb ranges totaling less than the approved object")


def validate_xorb_publication(raw: bytes, row: dict[str, Any]) -> dict[str, Any]:
    # The writer publishes before the projected view exists. The view fixture
    # therefore gets its own approval after publication.
    value = fields(decode(raw), GIT_PUBLICATION_FIELDS, "native xorb workload publication")
    edge = scope_parts(row["scope"])[0]
    require(matches(SHA256, value["configuration_sha256"]), "xorb workload publication needs its source fixture digest")
    provenance = row | {"scope": f"{edge}/private/sdk/git/direct", "configuration_sha256": value["configuration_sha256"]}
    return validate_git_publication(raw, provenance)


def verify_xorb_read(
    raw: bytes, row: dict[str, Any], registration: dict[str, Any], arm: str,
    spec: dict[str, Any], placement: dict[str, Any],
) -> dict[str, Any]:
    local = arm == "warm_local"
    mount = is_direct_mount_xorb_scope(row["scope"])
    window = spec.get("read_range")
    expected_fields = MOUNT_XORB_READ_FIELDS if mount else SDK_HYDRATION_READ_FIELDS
    if window is not None:
        expected_fields = expected_fields | {"range_offset"}
        if not mount:
            expected_fields.add("commit")
    read = fields(decode(raw), expected_fields | ({"primer_sha256"} if local else set()), "xorb read record")
    schema = "crab.cdn-mount-xorb-read/v1" if mount else "crab.cdn-sdk-hydration/v1"
    if window is not None:
        schema = MOUNT_XORB_RANGE_READ if mount else SDK_XORB_RANGE_READ
        require(not local and type(read["range_offset"]) is int and read["range_offset"] == window["offset"],
                "sparse xorb read must match the approved file offset")
    require(read["schema"] == schema and read["arm"] == arm
            and all(read[key] == row[key] for key in CDN_PROVENANCE_FIELDS | {"platform"})
            and read["workload_sha256"] == registration["hydration_workload_sha256"]
            and read["candidate"] == registration["candidate" if mount else "sdk_candidate"]
            and read["crab_sha256"] == spec["crab_sha256"] and read["pointer_sha256"] == spec["pointer_sha256"],
            "xorb read used a different workflow, workload or executable")
    if mount:
        require(read["mounted_oid"] == spec["mounted_oid"] and read["unmounted"] is True
                and read["candidate"]["binary_sha256"] == spec["crab_sha256"]
                and "nfs" in read["candidate"]["cargo_features"].split(","),
                "mount read requires the approved projected Git commit and clean NFS detach")
    else:
        require("local" in read["candidate"]["cargo_features"].split(","), "SDK hydration requires the local feature")
        if window is not None:
            require(read["candidate"]["binary_sha256"] == spec["sdk_sha256"] and read["commit"] == spec["projected_oid"],
                    "sparse SDK read requires the approved SDK executable and projected commit")
    payload = fields(read["payload"], {"sha256", "bytes"}, "verified hydrated file")
    expected_payload = ({"sha256": window["sha256"], "bytes": window["bytes"]} if window is not None
                        else {"sha256": spec["payload_sha256"], "bytes": spec["payload_bytes"]})
    require(type(payload["bytes"]) is int and payload == expected_payload,
            "xorb read did not verify the approved file")
    target = placement["views"]["target"]
    reader = fields(read["reader"], CLI_NATIVE_FIELDS | {
        "global_prefix_sha256", "binding_sha256", "approval_sha256", "cache_root_sha256", "local_cache_only",
    }, "verified xorb read target")
    require(reader["origin_sha256"] == placement["origin_sha256"]
            and reader["prefix_sha256"] == target["physical_prefix_sha256"]
            and read["xorb_key_sha256"] == target["key_sha256"]
            and reader["global_prefix_sha256"] == spec["global_prefix_sha256"]
            and matches(SHA256, reader["cache_root_sha256"])
            and reader["local_cache_only"] is True,
            "xorb read used a different projected view, xorb, or cache service")
    approved = arm != "origin"
    require(reader["binding_sha256"] == (registration["viewer_binding_sha256"] if approved else "")
            and reader["approval_sha256"] == (registration["viewer_approval_sha256"] if approved else ""),
            "xorb read did not use the required viewer approval")
    verify_git_traffic(read, scope_parts(row["scope"])[0], "xorb read")
    if local:
        require(matches(SHA256, read["primer_sha256"]) and positive(read["local_payload_hits"])
                and all(read[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "warm hydration requires verified local chunks with zero edge attempts")
    elif approved:
        require(all(positive(read[key]) for key in ("observed_edge_gets", "verified_edge_payloads", "edge_bytes"))
                and read["local_payload_hits"] == 0, "xorb read accepted no verified edge xorb")
    else:
        native = read["native_traffic"]
        require(all(read[key] == 0 for key in PRODUCT_COUNTERS)
                and positive(native["get_responses"]) and positive(native["body_bytes"]),
                "xorb read rollback requires native payload reads without edge or cache activity")
    return read


def verify_sparse_xorb_product(
    inputs: Path, row: dict[str, Any], registration: dict[str, Any], placement: dict[str, Any],
    full_spec: dict[str, Any], published: int, retired: int, cache_roots: set[str], viewer_ids: set[str],
    *, policies: dict[str, ReleaseReadLimits] | None,
) -> tuple[dict[str, bytes], tuple[int, int] | None]:
    edge = scope_parts(row["scope"])[0]
    mount = is_direct_mount_xorb_scope(row["scope"])
    records = {"sparse-workload.json": read_git_record(inputs / "sparse-workload.json")}
    approval = registration["sparse_workload_sha256"]
    spec = validate_xorb_workload(records["sparse-workload.json"], approval, row["configuration_sha256"],
                                  mount=mount, allow_sparse=True)
    require("read_range" in spec and all(spec[key] == full_spec[key] for key in full_spec if key != "schema"),
            "sparse xorb read must use the complete hydration workload's publication and projection")
    sparse_registration = registration | {"hydration_workload_sha256": approval}
    object_bytes = placement["views"]["target"]["bytes"]
    range_witness = None
    previous_end = published
    for arm in ("origin", "cold-edge", "warm-edge"):
        name = f"sparse-{arm}"
        client_name = f"{name}.json"
        raw = records[client_name] = read_git_record(inputs / client_name, limit=GIT_FETCH_RECORD_LIMIT)
        read = verify_xorb_read(raw, row, sparse_registration, "origin" if arm == "origin" else "edge", spec, placement)
        require(previous_end <= read["started_at_unix_ms"] <= read["finished_at_unix_ms"] <= retired,
                "sparse xorb reads must run in order within the observed publication lifetime")
        if arm == "warm-edge" and edge == "cloudfront":
            require(previous_end // 1000 < read["started_at_unix_ms"] // 1000,
                    "CloudFront sparse reads need distinct cold and warm S3 log seconds")
        previous_end = read["finished_at_unix_ms"]
        root = read["reader"]["cache_root_sha256"]
        require(root not in cache_roots, "sparse xorb read reused a local cache")
        cache_roots.add(root)
        if arm == "origin":
            continue
        verify_sparse_xorb_traffic(read, object_bytes)
        requests = read["edge_responses"]
        if policies is not None:
            verify_compiled_edge_limits(read, policies, f"{name} xorb read")
        log_name = f"{name}-viewer-requests.json"
        log = records[log_name] = read_git_record(inputs / log_name, limit=GIT_FETCH_RECORD_LIMIT)
        metrics = verify_viewer_requests(decode(log), read, hashlib.sha256(raw).hexdigest(), edge)
        require(viewer_ids.isdisjoint(requests), "sparse xorb read reused a CDN viewer request")
        viewer_ids.update(requests)
        if arm == "cold-edge":
            require("miss" in metrics.completed_cache.values() and metrics.successful_fill,
                    "cold sparse xorb read needs a provider-confirmed miss and origin fill")
        else:
            require(len(metrics.completed_cache) == read["edge_attempts"]
                    and all(result == "hit" for result in metrics.completed_cache.values()),
                    "warm sparse xorb read needs provider-confirmed hits for every xorb range")
            range_witness = (object_bytes, max(response["requested_bytes"] for response in requests.values()
                                               if response["scope"] == row["scope"]))
    return records, range_witness


def collect_xorb_read_evidence(
    directory: Path, row: dict[str, Any], registration: dict[str, Any], rollback_raw: bytes, *,
    placement: dict[str, Any], cleanup_at: int, native_origin_sha256: str,
    retain: Path | None = None, policy: Path | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    require(deployment_component_digest(placement) == registration["scoped_view_sha256"]
            and placement["fixture_sha256"] == row["configuration_sha256"]
            and placement["origin_sha256"] == native_origin_sha256,
            "xorb read view differs from the approved fixture and native origin")
    inputs = directory / XORB_PRODUCT_DIR
    mount = is_direct_mount_xorb_scope(row["scope"])
    workload_raw = read_git_record(inputs / "workload.json")
    approval = registration["hydration_workload_sha256"]
    spec = validate_xorb_workload(workload_raw, approval, row["configuration_sha256"], mount=mount)
    publication_raw = read_git_record(inputs / "publication.json")
    receipt = hashlib.sha256(publication_raw).hexdigest()
    publication = validate_xorb_publication(publication_raw, row)
    require(receipt == spec["publication_sha256"] and publication["published"]
            and publication["sha256"] == spec["pointer_sha256"],
            "xorb read requires independent native publication of the approved pointer")
    writer = verify_read_candidate(publication["candidate"], row["source_sha"], row["platform"], policy=policy)
    require(writer["binary_sha256"] == spec["crab_sha256"],
            "xorb read must use the approved Crab executable for the native publication and reads")
    read_name = "mount.json" if mount else "hydration.json"
    edge_raw = read_git_record(inputs / read_name, limit=GIT_FETCH_RECORD_LIMIT)
    policies = release_read_limits(policy) if policy is not None else None
    object_bytes = placement["views"]["target"]["bytes"]

    def verify_read(raw: bytes, arm: str) -> dict[str, Any]:
        read = verify_xorb_read(raw, row, registration, arm, spec, placement)
        xorbs = [response for response in read["edge_responses"].values() if response["scope"] == row["scope"]]
        require(arm != "edge" or any(response["complete"] and response["bytes"] for response in xorbs),
                "complete xorb read has no completed xorb response")
        require(all(response["object_bytes"] == object_bytes for response in xorbs),
                "xorb read disagrees with the approved object size")
        return read

    edge, native = verify_read(edge_raw, "edge"), verify_read(rollback_raw, "origin")
    product_caches = {read["reader"]["cache_root_sha256"] for read in (edge, native)}
    require(len(product_caches) == 2
            and edge["finished_at_unix_ms"] <= native["started_at_unix_ms"],
            "xorb read rollback must follow the edge read with the same view and a fresh cache")
    retirement_raw = read_git_record(inputs / "retirement.json")
    retired = verify_git_retirement(retirement_raw, publication, receipt, writer, native["finished_at_unix_ms"])
    observations = {phase: read_git_record(inputs / filename) for phase, filename in GIT_NATIVE_REF_FILES.items()}
    # The native ref canary sees the logical source; the workload reads its independently approved projection.
    source = {"origin_sha256": placement["origin_sha256"], "prefix_sha256": placement["source_prefix_sha256"]}
    published = verify_git_lifecycle(observations, row, publication, receipt, edge["started_at_unix_ms"], source,
                                     retired, cleanup_at, native_origin_sha256)
    viewer_raw = read_git_record(inputs / PRODUCT_VIEWER_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    product_request_ids = verify_product_viewer_requests(viewer_raw, edge, edge_raw, scope_parts(row["scope"])[0],
                                                         policies=policies)
    sparse_records, range_witness = verify_sparse_xorb_product(
        inputs, row, registration, placement, spec, published, retired, product_caches, product_request_ids,
        policies=policies,
    )

    def verify_measurement(raw: bytes, arm: str) -> dict[str, Any]:
        read = verify_read(raw, arm)
        require(published <= read["started_at_unix_ms"] <= read["finished_at_unix_ms"] <= retired,
                "xorb read benchmark ran outside the independently observed publication lifetime")
        require(read["reader"]["cache_root_sha256"] not in product_caches,
                "xorb read benchmark reused its lifecycle canary's local cache")
        return read

    performance = collect_git_performance(
        directory / GIT_BENCHMARK_DIR, registration, row["platform"], row["workflow_run_id"], row["workflow_run_attempt"],
        retain=retain / GIT_BENCHMARK_DIR if retain is not None else None, verify_xorb=verify_measurement,
        product_request_ids=product_request_ids, policy=policy, sparse_xorb_hit=range_witness,
    )
    if retain is not None:
        product = retain / XORB_PRODUCT_DIR
        product.mkdir()
        records = {"workload.json": workload_raw, "publication.json": publication_raw, read_name: edge_raw,
                   "retirement.json": retirement_raw, PRODUCT_VIEWER_RECORD: viewer_raw} | sparse_records
        records.update({filename: observations[phase] for phase, filename in GIT_NATIVE_REF_FILES.items()})
        for name, raw in records.items():
            (product / name).write_bytes(raw)
    return {
        "name": MOUNT_XORB_ACTION if mount else SDK_HYDRATION_ACTION,
        "expected_sha256": spec["payload_sha256"], "observed_sha256": edge["payload"]["sha256"],
        "expected_bytes": spec["payload_bytes"], "observed_bytes": edge["payload"]["bytes"],
        "native_writes": 1, "visible_new_ref": True, "fixture_cleanup_verified": True,
        "observed_edge_gets": edge["observed_edge_gets"], "verified_edge_payloads": edge["verified_edge_payloads"],
    }, performance


def collect_git_product_action(
    inputs: Path, row: dict[str, Any], rollback_raw: bytes, *, retain: Path | None = None, cleanup_at: int | None = None,
    native_origin_sha256: str | None = None, public_export_sha256: str = "",
    policies: dict[str, ReleaseReadLimits] | None = None,
) -> dict[str, Any]:
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    require(publication["published"], "Git product action requires a ref confirmed after native push")
    provenance = {key: row[key] for key in CDN_PROVENANCE_FIELDS}
    fetch_provenance = provenance | {"platform": row["platform"], "candidate": row["candidate"]}
    require(publication["candidate"] == row["candidate"], "publication did not run the registered CLI build")
    fetch_raw = read_git_record(inputs / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT)
    issuer = row.get("managed_issuer")
    fetch = validate_git_fetch(fetch_raw, fetch_provenance, "edge", "Git product action", managed_issuer=issuer)
    require(fetch["reader"]["binding_sha256"] == row.get("viewer_binding_sha256"),
            "product fetch did not use the registered viewer binding")
    require(fetch["reader"]["public_export_sha256"] == public_export_sha256,
            "product fetch did not use the registered public export approval")
    digest = hashlib.sha256(publication_raw).hexdigest()
    require(fetch["publication_sha256"] == digest and fetch["publication_oid"] == publication["oid"]
            and fetch["expected_sha256"] == publication["sha256"]
            and fetch["observed_bytes"] == publication["payload_bytes"] == cli_payload_bytes(row)
            and fetch["started_at_unix_ms"] >= publication["visible_at_unix_ms"],
            "Git product fetch did not verify this native publication after the ref became visible")
    rollback = validate_git_fetch(rollback_raw, fetch_provenance, "origin", "CDN rollback", managed_issuer=issuer)
    require(all(rollback[key] == fetch[key] for key in ("publication_sha256", "publication_oid", "observed_sha256", "observed_bytes")),
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
    viewer_raw = read_git_record(inputs / PRODUCT_VIEWER_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    verify_product_viewer_requests(viewer_raw, fetch, fetch_raw, scope_parts(row["scope"])[0], policies=policies)
    if retain is not None:
        retain.mkdir()
        retained = {"publication.json": publication_raw, "fetch.json": fetch_raw,
                    "retirement.json": retirement_raw, PRODUCT_VIEWER_RECORD: viewer_raw}
        if public_export_sha256:
            retained[GIT_PRIOR_EXPORT_RECORD] = read_git_record(inputs / GIT_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
        for filename, raw in retained.items():
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
    policies: dict[str, ReleaseReadLimits] | None = None,
) -> tuple[dict[str, Any], set[str]]:
    provider_edge, access, _, _, _ = scope_parts(row["scope"])
    native_records = {phase: read_git_record(inputs / name) for phase, name in GIT_NATIVE_REF_FILES.items()}
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    require(publication["published"], "SDK snapshot requires a ref confirmed after native push")
    receipt = hashlib.sha256(publication_raw).hexdigest()
    edge_raw = read_git_record(inputs / "snapshot.json", limit=GIT_FETCH_RECORD_LIMIT)
    edge = verify_sdk_git_read(edge_raw, row, spec, "edge")
    origin = verify_sdk_git_read(rollback_raw, row, spec, "origin")
    require(all(read["publication_sha256"] == receipt and read["commit"] == publication["oid"]
                and read["sha256"] == publication["sha256"] and read["bytes"] == publication["payload_bytes"]
                for read in (edge, origin))
            and edge["git_blob_oid_sha256"] == origin["git_blob_oid_sha256"],
            "SDK snapshot and rollback must verify the same published Git blob")
    require(all(edge["reader"][key] == origin["reader"][key] for key in CLI_NATIVE_FIELDS),
            "SDK rollback changed the selected native target")
    require(edge["finished_at_unix_ms"] <= origin["started_at_unix_ms"],
            "SDK rollback must follow the verified edge snapshot")
    retained = {"publication.json": publication_raw, "snapshot.json": edge_raw}
    viewer_raw = read_git_record(inputs / PRODUCT_VIEWER_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    viewer_ids = verify_product_viewer_requests(viewer_raw, edge, edge_raw, provider_edge, policies=policies)
    retained[PRODUCT_VIEWER_RECORD] = viewer_raw
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
        viewer_name = SDK_WORKTREE_VIEWER_FILES[action]
        viewer_raw = read_git_record(inputs / viewer_name, limit=GIT_FETCH_RECORD_LIMIT)
        ids = verify_product_viewer_requests(viewer_raw, edge_worktree, retained[files["edge"]], provider_edge,
                                             policies=policies)
        require(viewer_ids.isdisjoint(ids), "SDK product actions reused provider requests")
        viewer_ids.update(ids)
        retained[viewer_name] = viewer_raw
        previous_end = native_worktree["finished_at_unix_ms"]
    retirement_raw = read_git_record(inputs / "retirement.json")
    retired_at = verify_git_retirement(retirement_raw, row, receipt, publication["candidate"], previous_end)
    earliest = edge
    retained["retirement.json"] = retirement_raw
    if access == "public":
        prior_raw = read_git_record(inputs / GIT_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
        retained[GIT_PRIOR_EXPORT_RECORD] = prior_raw
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
    }, viewer_ids


class DesktopProductEvidence(NamedTuple):
    action: dict[str, Any]
    viewer_ids: set[str]
    cache_roots: set[str]
    commit: str
    cleanup_at: int
    export_ready_at: int


def collect_desktop_product_action(
    inputs: Path, row: dict[str, Any], spec: dict[str, Any], rollback_raw: bytes,
    *, cleanup_at: int, native_origin_sha256: str, retain: Path | None = None,
    policies: dict[str, ReleaseReadLimits] | None = None,
) -> DesktopProductEvidence:
    provider_edge, access, _, _, _ = scope_parts(row["scope"])
    files = ("publication.json", "retirement.json", *GIT_NATIVE_REF_FILES.values())
    retained = {name: read_git_record(inputs / name) for name in files}
    publication_raw = retained["publication.json"]
    publication = validate_git_publication(publication_raw, row)
    require(publication["published"] and publication["candidate"] == spec["candidate"],
            "desktop product read requires a ref confirmed by the registered native writer")
    receipt = hashlib.sha256(publication_raw).hexdigest()
    for name in (DESKTOP_READ_RECORD, PRODUCT_VIEWER_RECORD):
        retained[name] = read_git_record(inputs / name, limit=GIT_FETCH_RECORD_LIMIT)
    edge_raw = retained[DESKTOP_READ_RECORD]
    edge = verify_desktop_git_read(edge_raw, row, spec, "edge")
    native = verify_desktop_git_read(rollback_raw, row, spec, "origin")
    reads = [edge, native]
    earliest = edge
    export_ready_at = 0
    if access == "public":
        _, _, export_ready_at = verified_publication(inputs.parent / PUBLIC_EXPORT_DIR, spec["publication_sha256"])
        prior_raw = read_git_record(inputs / GIT_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
        retained[GIT_PRIOR_EXPORT_RECORD] = prior_raw
        earliest = verify_desktop_git_read(prior_raw, row, spec, "prior_export")
        reads.insert(0, earliest)
    require(all(read["publication_sha256"] == receipt and read["commit"] == publication["oid"]
                and read["sha256"] == publication["sha256"] and read["bytes"] == publication["payload_bytes"]
                and read["git_blob_oid_sha256"] == edge["git_blob_oid_sha256"]
                for read in reads), "desktop product reads must verify the same published Git blob")
    roots = {read["cache_root_sha256"] for read in reads}
    require(len(roots) == len(reads) and all(left["finished_at_unix_ms"] <= right["started_at_unix_ms"]
                                           for left, right in zip(reads, reads[1:])),
            "desktop product reads must run in order with fresh caches")
    viewers = verify_product_viewer_requests(retained[PRODUCT_VIEWER_RECORD], edge, edge_raw, provider_edge,
                                             policies=policies)
    retired_at = verify_git_retirement(retained["retirement.json"], row, receipt, spec["candidate"], native["finished_at_unix_ms"])
    native_records = {phase: retained[name] for phase, name in GIT_NATIVE_REF_FILES.items()}
    verify_git_lifecycle(native_records, row, publication, receipt, earliest["started_at_unix_ms"], earliest["reader"],
                         retired_at, cleanup_at, native_origin_sha256)
    if retain is not None:
        retain.mkdir()
        for name, raw in retained.items():
            (retain / name).write_bytes(raw)
    action = {
        "name": DESKTOP_GIT_ACTION, "expected_sha256": publication["sha256"], "observed_sha256": edge["sha256"],
        "expected_bytes": GIT_PAYLOAD_SIZE, "observed_bytes": edge["bytes"],
        "native_writes": 1, "visible_new_ref": True, "fixture_cleanup_verified": True,
        "observed_edge_gets": edge["observed_edge_gets"], "verified_edge_payloads": edge["verified_edge_payloads"],
    }
    return DesktopProductEvidence(action, viewers, roots, publication["oid"], cleanup_at, export_ready_at)


def derive_cli_public_transition(inputs: Path, row: dict[str, Any], spec: dict[str, Any]) -> dict[str, Any]:
    provenance = {field: row[field] for field in CDN_PROVENANCE_FIELDS | {"platform"}}
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, provenance)
    digest = hashlib.sha256(publication_raw).hexdigest()
    reference = deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
    provenance["candidate"] = spec["candidate"]
    prior = validate_git_fetch(read_git_record(inputs / GIT_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT),
                               provenance, "prior_export", "CLI prior export fetch")
    edge = validate_git_fetch(read_git_record(inputs / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT),
                               provenance, "edge", "CLI current export fetch")
    key = prior["transition_key_sha256"]
    require(key == edge.get("transition_key_sha256"), "CLI export transition must verify the same object")
    require(publication["published"] and publication["candidate"] == spec["candidate"]
            and all(read["publication_sha256"] == digest and read["publication_oid"] == publication["oid"]
                    and read["observed_sha256"] == publication["sha256"]
                    and read["observed_bytes"] == publication["payload_bytes"] == cli_payload_bytes(spec)
                    for read in (prior, edge)),
            "CLI export transition must verify the same published payload")
    require(all(prior["reader"][field] == edge["reader"][field] for field in CLI_NATIVE_FIELDS)
            and prior["reader"]["cache_root_sha256"] != edge["reader"]["cache_root_sha256"],
            "CLI export transition needs the same native target and fresh caches")
    trace = {field: row[field] for field in CDN_PROVENANCE_FIELDS | {"platform"}} | {
        "schema": "crab.cdn-public-transition/v1", "key_sha256": key,
        "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": publication["visible_at_unix_ms"]},
    }
    for phase, read, approval in (("before", prior, spec["previous_publication_sha256"]),
                                  ("after", edge, spec["publication_sha256"])):
        reader = read["reader"]
        require(reader["binding_sha256"] == spec["viewer_binding_sha256"] and reader["public_export_sha256"] == approval,
                "CLI export transition must use the registered binding and approval for each fetch")
        trace[phase] = {
            "action": read["name"], "receipt_sha256": approval, "key_sha256": key,
            "reference_sha256": reference, "result_sha256": read["observed_sha256"], "result_bytes": read["observed_bytes"],
            "started_at_unix_ms": read["started_at_unix_ms"], "finished_at_unix_ms": read["finished_at_unix_ms"],
            "edge_attempts": read["all_edge_attempts"], "binding_active": True, "reader": reader,
            "verified_edge_key_sha256": key if phase == "after" else None,
        }
    return trace


def derive_git_blob_public_transition(inputs: Path, row: dict[str, Any], spec: dict[str, Any]) -> dict[str, Any]:
    desktop = is_direct_desktop_git_scope(row["scope"])
    read_record = DESKTOP_READ_RECORD if desktop else "snapshot.json"
    verify_read = verify_desktop_git_read if desktop else verify_sdk_git_read
    consumer = "desktop" if desktop else "SDK"
    action = DESKTOP_GIT_ACTION if desktop else SDK_GIT_ACTION
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    digest = hashlib.sha256(publication_raw).hexdigest()
    prior_raw = read_git_record(inputs / GIT_PRIOR_EXPORT_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    edge_raw = read_git_record(inputs / read_record, limit=GIT_FETCH_RECORD_LIMIT)
    prior = verify_read(prior_raw, row, spec, "prior_export")
    edge = verify_read(edge_raw, row, spec, "edge")
    key = prior["transition_key_sha256"]
    require(key == edge.get("transition_key_sha256"), f"{consumer} export transition did not verify the same pack-layer key")
    viewer_raw = read_git_record(inputs / PRODUCT_VIEWER_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
    verify_viewer_requests(decode(viewer_raw), edge, hashlib.sha256(edge_raw).hexdigest(), scope_parts(row["scope"])[0])
    require(publication["published"] and
            all(read["publication_sha256"] == digest and read["commit"] == publication["oid"]
                and read["sha256"] == publication["sha256"] for read in (prior, edge))
            and prior["git_blob_oid_sha256"] == edge["git_blob_oid_sha256"],
            f"{consumer} export transition did not verify the same published Git blob")
    require(all(prior["reader"][field] == edge["reader"][field] for field in CLI_NATIVE_FIELDS),
            f"{consumer} export transition changed the selected native target")
    reference = deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
    trace = {field: row[field] for field in CDN_PROVENANCE_FIELDS | {"platform"}} | {
        "schema": "crab.cdn-public-transition/v1", "key_sha256": key,
        "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": publication["visible_at_unix_ms"]},
    }
    for phase, read in (("before", prior), ("after", edge)):
        trace[phase] = {
            "action": action, "receipt_sha256": read["approval_sha256"], "key_sha256": key,
            "reference_sha256": reference, "result_sha256": read["sha256"], "result_bytes": read["bytes"],
            "started_at_unix_ms": read["started_at_unix_ms"], "finished_at_unix_ms": read["finished_at_unix_ms"],
            "edge_attempts": read["all_edge_attempts"], "binding_active": True, "reader": read["reader"],
            "verified_edge_key_sha256": key if phase == "after" else None,
        }
    return trace


def verify_service_transition_reads(prior_raw: bytes, edge: dict[str, Any], row: dict[str, Any],
                                    spec: dict[str, Any]) -> dict[str, Any]:
    prior = validate_service_read(decode(prior_raw), row, "prior_export")
    key = prior["transition_key_sha256"]
    require(key == edge.get("transition_key_sha256"), "service transition must prove the same export object")
    require(prior["workload_sha256"] == spec["service_prior_workload_sha256"]
            and edge["workload_sha256"] == spec["service_workload"]["sha256"],
            "service transition must use the approved prior and current workloads")
    require(all(prior[key] == edge[key] for key in (
        "candidate", "publication_sha256", "native_published_sha256", "commit",
        "payload_sha256", "payload_bytes", "etag_sha256", "git_blob_oid_sha256", "lfs_key_sha256",
    )), "service transition must verify the same published payload and service binary")
    require(prior["reader"] == edge["reader"] | {"public_export_sha256": spec["previous_publication_sha256"]}
            and edge["reader"]["public_export_sha256"] == spec["publication_sha256"],
            "service transition must keep the native target and viewer while applying the approved receipts")
    require(prior["service_instance_sha256"] != edge["service_instance_sha256"]
            and prior["finished_at_unix_ms"] <= edge["started_at_unix_ms"],
            "service transition requires fresh processes and a prior read before edge delivery")
    return prior


def derive_service_public_transition(inputs: Path, row: dict[str, Any], spec: dict[str, Any]) -> dict[str, Any]:
    publication_raw = read_git_record(inputs / "publication.json")
    publication = validate_git_publication(publication_raw, row)
    edge = validate_service_read(decode(read_git_record(inputs / "edge.json", limit=GIT_FETCH_RECORD_LIMIT)), row, "edge")
    prior = verify_service_transition_reads(read_git_record(inputs / SERVICE_PUBLIC_PRIOR_RECORD), edge, row, spec)
    require(publication["published"] and edge["publication_sha256"] == hashlib.sha256(publication_raw).hexdigest()
            and edge["commit"] == publication["oid"] and edge["payload_sha256"] == publication["sha256"],
            "service transition must use the confirmed native publication")
    key = edge["transition_key_sha256"]
    reference = deployment_component_digest({"ref": publication["ref"], "oid": publication["oid"]})
    trace = {field: row[field] for field in CDN_PROVENANCE_FIELDS | {"platform"}} | {
        "schema": "crab.cdn-public-transition/v1", "key_sha256": key,
        "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": publication["visible_at_unix_ms"]},
    }
    for phase, read in (("before", prior), ("after", edge)):
        trace[phase] = {
            "action": SERVICE_READ_ACTION, "receipt_sha256": read["reader"]["public_export_sha256"],
            "key_sha256": key, "reference_sha256": reference, "result_sha256": read["payload_sha256"],
            "result_bytes": read["payload_bytes"], "started_at_unix_ms": read["started_at_unix_ms"],
            "finished_at_unix_ms": read["finished_at_unix_ms"], "edge_attempts": read["all_edge_attempts"],
            "binding_active": True, "reader": read["reader"], "verified_edge_key_sha256": key if phase == "after" else None,
        }
    return trace


def collect_git_performance(
    inputs: Path, registration: dict[str, Any], platform: str, run_id: str, attempt: str,
    *, retain: Path | None = None, native_target: dict[str, str] | None = None,
    verify_xorb: Callable[[bytes, str], dict[str, Any]] | None = None,
    service_row: Path | None = None, product_request_ids: set[str] | None = None,
    desktop_product: DesktopProductEvidence | None = None,
    managed_row: Path | None = None, policy: Path | None = None, sparse_xorb_hit: tuple[int, int] | None = None,
) -> dict[str, Any]:
    spec = benchmark_registration(registration)
    scope = spec["scope"]
    policies = release_read_limits(policy) if policy is not None else None
    limits = policies.get(scope) if policies is not None else None
    missing_boundaries = {}
    if limits is not None:
        if limits.full_bytes:
            missing_boundaries["full GET"] = (limits.full_bytes, limits.full_bytes)
        # Whole hydration can prefer full GETs throughout the benchmark.
        # Its separate sparse read can witness the same xorb's ranged limit.
        if limits.range_bytes and sparse_xorb_hit != (limits.ranged_object_bytes, limits.range_bytes):
            missing_boundaries["ranged GET"] = (limits.ranged_object_bytes, limits.range_bytes)
    hydration = is_direct_xorb_read_scope(scope)
    require(hydration == (verify_xorb is not None), "xorb read benchmarks require verified publication and placement")
    require(hydration or sparse_xorb_hit is None, "sparse size evidence requires an xorb read workload")
    sdk = is_direct_sdk_git_scope(scope)
    desktop = requires_desktop_workload(scope, spec["workload"])
    service = is_direct_service_scope(scope)
    cli_workload = requires_cli_workload(scope)
    managed_git = is_managed_cli_git_scope(scope)
    require(managed_git == (managed_row is not None)
            and (not managed_git or native_target is not None),
            "managed Git benchmarks require the retained product lifecycle and native target")
    require(service == (service_row is not None), "service benchmarks require the retained lifecycle row")
    require(sdk or desktop or hydration or service or (cli_workload and spec["workload"] == cli_action_name(scope)),
            "benchmark must register a supported CLI, SDK, desktop or service workload")
    if sdk:
        require(inputs.is_dir(), "direct SDK Git requires retained regional SDK benchmark records")
    if hydration:
        require(inputs.is_dir(), "direct xorb reads require retained regional benchmark records")
    require(matches(COMMIT, spec["source_sha"]) and matches(SHA256, spec["configuration_sha256"])
            and matches(RUN_NUMBER, run_id) and matches(RUN_NUMBER, attempt) and matches(LABEL, platform),
            "invalid benchmark provenance")
    provider = PROVIDER_FOR_EDGE[scope.split("/", 1)[0]]
    fill_source = {
        "s3": "CloudFront fill from the approved S3 origin",
        "gcs": "Cloud CDN fill from the approved GCS origin",
        "azure": "Front Door fill from the approved Blob origin",
    }[provider]
    provenance = {key: spec[key] for key in ("source_sha", "scope", "configuration_sha256")}
    provenance |= {"workflow_run_id": run_id, "workflow_run_attempt": attempt, "platform": platform}
    if cli_workload:
        provenance["candidate"] = spec["candidate"]
    service_anchor = None
    service_instances = set()
    provider_request_ids = set(product_request_ids or ())
    cache_roots = set()
    desktop_commits = set()
    managed_commits = set()
    managed_native = None
    managed_cleanup_at = None
    if managed_row is not None:
        managed_product = managed_row / GIT_PRODUCT_DIR
        edge_raw = read_git_record(managed_product / "fetch.json", limit=GIT_FETCH_RECORD_LIMIT)
        edge_fetch = validate_git_fetch(edge_raw, provenance, "edge", "managed Git product fetch",
                                        managed_issuer=spec["managed_issuer"])
        origin_fetch = validate_git_fetch(read_trace(managed_row / "traces/rollback.json", "rollback"), provenance,
                                          "origin", "managed Git product rollback", managed_issuer=spec["managed_issuer"])
        managed_commits.add(edge_fetch["publication_oid"])
        cache_roots.update(fetch["reader"]["cache_root_sha256"] for fetch in (edge_fetch, origin_fetch))
        viewer_raw = read_git_record(managed_product / PRODUCT_VIEWER_RECORD, limit=GIT_FETCH_RECORD_LIMIT)
        provider_request_ids.update(verify_product_viewer_requests(viewer_raw, edge_fetch, edge_raw, PROVIDERS[provider][0],
                                                                   policies=policies))
        publication_raw = read_git_record(managed_product / "publication.json")
        publication = validate_git_publication(publication_raw, provenance)
        observed = verify_git_native_ref(decode(read_git_record(managed_product / GIT_NATIVE_REF_FILES["published"])),
                                         provenance, publication, hashlib.sha256(publication_raw).hexdigest(), "published",
                                         native_target["origin_sha256"], prefix_sha256=native_target["prefix_sha256"])
        managed_native = observed["native_ref"]
        managed_cleanup_at = decode(read_trace(managed_row / "traces/cleanup.json", "cleanup"))["observed_at_unix_ms"]
    if desktop_product is not None:
        require(desktop, "desktop product evidence requires a desktop benchmark")
        provider_request_ids.update(desktop_product.viewer_ids)
        cache_roots.update(desktop_product.cache_roots)
        desktop_commits.add(desktop_product.commit)
    published_at, retired_at, export_ready_at = 0, 0, 0
    if service_row is not None:
        require(inputs.is_dir(), "direct service reads require retained regional benchmark records")
        product = service_row / SERVICE_PRODUCT_DIR
        edge_raw = read_git_record(product / "edge.json", limit=GIT_FETCH_RECORD_LIMIT)
        service_anchor = validate_service_read(
            decode(edge_raw), provenance, "edge",
        )
        service_origin = validate_service_read(decode(read_trace(service_row / "traces/rollback.json", "rollback")),
                                               provenance, "origin")
        service_instances.update(record["service_instance_sha256"] for record in (service_anchor, service_origin))
        if service_anchor["lfs_key_sha256"] is not None:
            cache_roots.add(service_anchor["cache_root_sha256"])
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
        if scope_parts(scope)[1] == "public":
            prior = verify_service_transition_reads(read_git_record(product / SERVICE_PUBLIC_PRIOR_RECORD),
                                                    service_anchor, provenance, spec)
            service_instances.add(prior["service_instance_sha256"])
            _, _, export_ready_at = verified_publication(service_row / PUBLIC_EXPORT_DIR, spec["publication_sha256"])
        provider_request_ids = verify_product_viewer_requests(
            read_trace(product / PRODUCT_VIEWER_RECORD, "service viewer requests"), service_anchor,
            edge_raw, PROVIDERS[provider][0], policies=policies,
        )
    seen = set()
    performance = {}
    for location, region in spec["regions"].items():
        pairs = []
        previous_end = 0
        for index in range(1, spec["samples"] + 1):
            pair_path = Path(location) / f"{index:04d}"
            pair = {}
            fixtures = set()
            windows = {}
            primer_digest = None
            primer_cache = None
            service_primer = None
            for arm in ARMS:
                slot = pair_path / arm
                label = f"{location} {index} {arm}"
                client_raw = read_git_record(inputs / slot / "client.json", limit=GIT_FETCH_RECORD_LIMIT)
                digest = hashlib.sha256(client_raw).hexdigest()
                require(digest not in seen, "a fetch was reused in another benchmark arm or pair")
                seen.add(digest)
                if verify_xorb is not None:
                    client = verify_xorb(client_raw, GIT_MODES[arm])
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
                    if arm == "warm_edge":
                        primer_digest = digest
                    if arm == "warm_local":
                        require(client["primer_sha256"] == primer_digest, f"{label} did not repeat its own warm edge snapshot")
                    payload = (client["commit"], client["sha256"], client["git_blob_oid_sha256"])
                    receipt = client["publication_sha256"]
                elif desktop:
                    client = verify_desktop_git_read(client_raw, provenance, spec, GIT_MODES[arm])
                    require("transition_key_sha256" not in client, f"{label} reused a desktop public transition read")
                    if desktop_product is not None:
                        require(client["finished_at_unix_ms"] <= desktop_product.cleanup_at
                                and (arm == "origin" or desktop_product.export_ready_at <= client["started_at_unix_ms"]),
                                f"{label} ran outside the desktop fixture lifetime")
                    reader = client["reader"]
                    cache_root = client["cache_root_sha256"]
                    if arm == "warm_local":
                        require(client["primer_sha256"] == primer_digest and cache_root == primer_cache,
                                f"{label} did not repeat its own warm edge desktop read in the same cache")
                    else:
                        require(cache_root not in cache_roots, f"{label} reused a desktop cache root")
                        cache_roots.add(cache_root)
                    if arm == "warm_edge":
                        primer_digest, primer_cache = digest, cache_root
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
                        "payload_sha256", "payload_bytes", "etag_sha256", "git_blob_oid_sha256", "lfs_key_sha256",
                    )), f"{label} used a different service workload or read target")
                    require(published_at <= client["started_at_unix_ms"] <= client["finished_at_unix_ms"] <= retired_at,
                            f"{label} ran outside the independently observed publication interval")
                    require(arm == "origin" or export_ready_at <= client["started_at_unix_ms"],
                            f"{label} started before the registered export was sealed")
                    if arm == "warm_local":
                        require(service_primer is not None, f"{label} has no warm edge primer")
                        verify_service_repeat(client, service_primer, primer_digest)
                    else:
                        instance = client["service_instance_sha256"]
                        require(instance not in service_instances, f"{label} must use a fresh service process")
                        service_instances.add(instance)
                        cache_root = client.get("cache_root_sha256")
                        if cache_root:
                            require(cache_root not in cache_roots, f"{label} reused a service LFS cache root")
                            cache_roots.add(cache_root)
                    if arm == "warm_edge":
                        service_primer, primer_digest = client, digest
                    payload = (client["commit"], client["payload_sha256"], client["etag_sha256"],
                               client["git_blob_oid_sha256"], client["lfs_key_sha256"])
                    receipt = client["publication_sha256"]
                else:
                    client = validate_git_fetch(client_raw, provenance, GIT_MODES[arm], label,
                                                managed_issuer=spec.get("managed_issuer"))
                    require(client["observed_bytes"] == cli_payload_bytes(spec),
                            f"{label} differs from the registered CLI payload size")
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
                if native_target is not None:
                    require(all(reader[key] == native_target[key] for key in CLI_NATIVE_FIELDS),
                            f"{label} changed the independently observed native target")
                fixtures.add((payload, receipt, reader["origin_sha256"], reader["prefix_sha256"]))
                windows[arm] = (client["started_at_unix_ms"], client["finished_at_unix_ms"])
                viewer_log = None
                traffic = ViewerRequestMetrics(set(), 0, 0, False, {})
                cache_corrections = dict.fromkeys(("hit", "miss", "unknown"), 0)
                if policies is not None:
                    verify_compiled_edge_limits(client, policies, label)
                if provider in VIEWER_REQUEST_FILES and client["all_edge_attempts"]:
                    log_name = VIEWER_REQUEST_FILES[provider]
                    viewer_log = read_git_record(inputs / slot / log_name, limit=GIT_FETCH_RECORD_LIMIT)
                    traffic = verify_viewer_requests(
                        decode(viewer_log), client, digest, PROVIDERS[provider][0],
                        include_adjacent_origin=hydration,
                    )
                    require(provider_request_ids.isdisjoint(traffic.request_ids), "CDN log requests were reused across arms")
                    provider_request_ids.update(traffic.request_ids)
                    responses = {request_id: row for request_id, row in client["edge_responses"].items()
                                 if row["scope"] == scope}
                    for request_id, row in responses.items():
                        if not row["complete"] or not row["bytes"]:
                            continue
                        # Provider logs can disprove a hit reported by the
                        # viewer, or resolve an ambiguous viewer header.
                        cache = traffic.completed_cache.get(request_id, "unknown")
                        cache_corrections[row["cache"]] -= 1
                        cache_corrections[cache] += 1
                        if arm == "warm_edge" and cache == "hit":
                            form = "ranged GET" if row["range"] else "full GET"
                            if missing_boundaries.get(form) == (row["object_bytes"], row["requested_bytes"]):
                                del missing_boundaries[form]
                external_raw = read_git_record(inputs / slot / "measurement.json")
                external = fields(decode(external_raw), GIT_MEASUREMENT_FIELDS, "benchmark measurement")
                require(external["schema"] == "crab.cdn-git-measurement/v1" and external["provider"] == provider
                        and external["region"] == region and external["client_report_sha256"] == digest,
                        f"{label} has mismatched provider measurements")
                measured = {key: client[key] for key in GIT_CLIENT_METRICS}
                measured.update({key: external[key] for key in GIT_EXTERNAL_METRICS})
                for cache, metric in (("hit", "edge_hits"), ("miss", "edge_misses"), ("unknown", "edge_unknown")):
                    measured[metric] += cache_corrections[cache]
                require(all(type(value) is int and value >= 0 for value in measured.values()),
                        f"{label} is missing measured metrics")
                require(positive(measured["peak_memory_bytes"])
                        and (sdk or desktop or service or positive(measured["peak_disk_bytes"])),
                        f"{label} is missing fetch memory or disk measurements")
                incomplete = client["incomplete_edge_bytes"]
                transferred = measured["edge_bytes"] + incomplete
                if desktop or hydration:
                    # Product counters select payloads. Cost and bandwidth cover every viewer request.
                    responses = client["edge_responses"].values()
                    transferred = max(transferred, sum(row["bytes"] for row in responses))
                    incomplete = max(incomplete, sum(row["bytes"] for row in responses if not row["complete"]))
                native = client["native_traffic"]
                require(measured["origin_gets"] >= native["get_responses"] + traffic.origin_gets,
                        f"{label} undercounts observed native GET responses or logged CDN origin GETs")
                require(measured["origin_fill_bytes"] >= traffic.fill_bytes,
                        f"{label} undercounts logged CDN cache fill bytes")
                require(arm != "cold_edge" or traffic.successful_fill, f"{label} has no logged {fill_source}")
                require(incomplete <= measured["rejected_bytes"]
                        and measured["client_bytes"] >= native["body_bytes"] + transferred,
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
            if cli_workload or desktop:
                publication_path = pair_path / "publication.json"
                raw = read_git_record(inputs / publication_path)
                publication = validate_git_publication(raw, provenance)
                payload, receipt, _, _ = next(iter(fixtures))
                oid, payload_sha256 = payload[:2]
                require(publication["published"] and publication["candidate"] == spec["candidate"]
                        and receipt == hashlib.sha256(raw).hexdigest()
                        and (oid, payload_sha256) == (publication["oid"], publication["sha256"])
                        and publication["payload_bytes"] == (GIT_PAYLOAD_SIZE if desktop else cli_payload_bytes(spec))
                        and publication["visible_at_unix_ms"] <= intervals[0][0],
                        f"{location} {index} must verify the retained {'desktop' if desktop else 'CLI'} publication after the ref became visible")
                if retain is not None:
                    (retain / publication_path).write_bytes(raw)
                if desktop:
                    require(oid not in desktop_commits, f"{location} {index} reused a desktop benchmark commit")
                    desktop_commits.add(oid)
                    native_path = pair_path / GIT_NATIVE_REF_FILES["published"]
                    native_raw = read_git_record(inputs / native_path)
                    target = spec["desktop_native_target"]
                    observed = verify_git_native_ref(decode(native_raw), provenance, publication, receipt, "published",
                                                     target["origin_sha256"], prefix_sha256=target["prefix_sha256"])
                    require(publication["visible_at_unix_ms"] <= observed["started_at_unix_ms"]
                            <= observed["observed_at_unix_ms"] <= intervals[0][0],
                            f"{location} {index} desktop reads require an independently observed native publication")
                    if retain is not None:
                        (retain / native_path).write_bytes(native_raw)
                if managed_native is not None:
                    require(oid not in managed_commits, f"{location} {index} reused a managed Git benchmark commit")
                    managed_commits.add(oid)
                    native_path = pair_path / GIT_NATIVE_REF_FILES["published"]
                    native_raw = read_git_record(inputs / native_path)
                    observed = verify_git_native_ref(decode(native_raw), provenance, publication, receipt, "published",
                                                     native_target["origin_sha256"], prefix_sha256=native_target["prefix_sha256"])
                    require(publication["visible_at_unix_ms"] <= observed["started_at_unix_ms"]
                            <= observed["observed_at_unix_ms"] <= intervals[0][0]
                            and all(observed["native_ref"][key] == managed_native[key]
                                    for key in ("repository_sha256", "epoch_sha256")),
                            f"{location} {index} must observe the publication in the managed Git product's native repository epoch")
                    require(intervals[-1][1] <= managed_cleanup_at,
                            f"{location} {index} finished after the managed Git fixture cleanup")
                    if retain is not None:
                        (retain / native_path).write_bytes(native_raw)
            previous_end = intervals[-1][1]
            pairs.append(pair)
        performance[location] = {"region": region, "pairs": pairs}
    require(not missing_boundaries,
            f"{scope}/{platform} lacks a provider-confirmed warm edge hit at its compiled "
            f"{', '.join(missing_boundaries)} size limit")
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
    mount = is_direct_mount_xorb_scope(scope)
    xorb_read = is_direct_xorb_read_scope(scope)
    desktop = requires_desktop_workload(scope, registration.get("workload"))
    service = is_direct_service_scope(scope)
    require(policy is None or cli_workload or sdk_git or xorb_read or desktop or service,
            "CDN promotion requires a product-specific workload and per-run regional evidence verifier")
    if cli_workload or desktop or mount:
        verify_read_candidate(registration.get("candidate"), source_sha, platform, policy=policy)
    if desktop:
        verify_read_candidate(registration.get("desktop_candidate"), source_sha, platform,
                              policy=policy, consumer="desktop sidecar")
    if sdk_git or is_direct_sdk_hydration_scope(scope):
        verify_sdk_candidate(registration.get("sdk_candidate"), source_sha, platform, policy=policy)
    if service:
        verify_service_candidate(registration.get("service_candidate"), source_sha, platform, policy=policy)
    lower = verify_performance(row["performance"], registration)
    verify_product_action(row["product_action"], family, xorb_read=xorb_read)
    require(row["product_action"]["name"] == registration["workload"],
            "product action does not match the registered workload")
    checks, cleanup_at, native_origin_sha256, rollback = verify_check_evidence(
        row, edge, access, profile, directory, registration=registration,
    )
    if cli_workload or sdk_git or desktop:
        native_target = {"origin_sha256": native_origin_sha256,
                         "prefix_sha256": hashlib.sha256(f"{row['fixture_prefix']}/".encode()).hexdigest()}
    policies = release_read_limits(policy) if policy is not None else None
    if cli_workload:
        require(rollback is not None, "CLI qualification requires a product rollback fetch")
        registered_read = {key: registration[key] for key in ("candidate", "viewer_binding_sha256")}
        registered_read["cli_payload_bytes"] = cli_payload_bytes(registration)
        if is_managed_cli_git_scope(scope):
            registered_read["managed_issuer"] = registration["managed_issuer"]
        product = collect_git_product_action(
            directory / GIT_PRODUCT_DIR, row | registered_read, rollback, cleanup_at=cleanup_at,
            native_origin_sha256=native_origin_sha256, public_export_sha256=registration.get("publication_sha256", ""),
            policies=policies,
        )
        require(row["product_action"] == product | {"fixture_cleanup_verified": True},
                "product action differs from the retained publication, fetch, and retirement evidence")
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                          native_target=native_target,
                                          managed_row=directory if is_managed_cli_git_scope(scope) else None,
                                          policy=policy)
        require(measured == row["performance"], "CLI performance summary differs from the retained measurements")
    if sdk_git:
        require(rollback is not None, "direct SDK qualification requires an ordinary SDK rollback read")
        product, request_ids = collect_sdk_product_action(directory / GIT_PRODUCT_DIR, row, registration, rollback,
                                                          cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256,
                                                          policies=policies)
        require(row["product_action"] == product, "product action differs from the retained SDK snapshot evidence")
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                          native_target=native_target, product_request_ids=request_ids, policy=policy)
        require(measured == row["performance"], "SDK performance summary differs from the retained measurements")
    if xorb_read:
        require(rollback is not None, "direct xorb reads require their native rollback")
        placement = decode(read_trace(directory / "traces" / f"{SCOPED_VIEW_CHECK}.json", SCOPED_VIEW_CHECK))["placement"]
        product, measured = collect_xorb_read_evidence(
            directory, row, registration, rollback, placement=placement,
            cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256, policy=policy,
        )
        require(row["product_action"] == product, "product action differs from the retained xorb read evidence")
        require(row["performance"] == measured, "xorb read performance summary differs from the retained measurements")
    if desktop:
        require(rollback is not None, "direct desktop qualification requires an ordinary desktop rollback read")
        product = collect_desktop_product_action(directory / GIT_PRODUCT_DIR, row, registration, rollback,
                                                 cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256,
                                                 policies=policies)
        require(row["product_action"] == product.action, "product action differs from the retained desktop read evidence")
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                          native_target=native_target, desktop_product=product, policy=policy)
        require(measured == row["performance"], "desktop performance summary differs from the retained measurements")
    if service:
        require(rollback is not None, "direct service qualification requires an ordinary HTTP rollback read")
        product = collect_service_product_action(directory / SERVICE_PRODUCT_DIR, row, registration, rollback,
                                                 cleanup_at=cleanup_at, native_origin_sha256=native_origin_sha256,
                                                 policies=policies)
        require(row["product_action"] == product, "product action differs from the retained service lifecycle evidence")
        measured = collect_git_performance(directory / GIT_BENCHMARK_DIR, registration, platform, run_id, run_attempt,
                                           service_row=directory, policy=policy)
        require(row["performance"] == measured, "service performance summary differs from the retained measurements")
    return {"status": "verified", "scope": scope, "platform": platform, "deployment": row["deployment"],
            "checks": checks, "distant_median_savings_lower_bound_ms": lower}
