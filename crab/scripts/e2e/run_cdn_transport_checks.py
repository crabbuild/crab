#!/usr/bin/env python3
"""Collect redacted HTTP evidence from an existing Crab CDN qualification fixture."""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import json
import re
import sys
import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from email.utils import formatdate
from pathlib import Path
from threading import Barrier, BrokenBarrierError
from typing import Any
from urllib.parse import SplitResult, parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CACHE_REUSE_CHECK, COMMIT, EDGE_ROUTE_QUERY_PROBES, EXPIRY_CHECKS, EXPIRY_FULL_CHECK, EXPIRY_RANGE_CHECK, EXPIRY_SETTLE_SECONDS,
    MAX_VIEWER_GRANT_SECONDS, PREFIX, PROVIDERS, PROVIDER_FOR_EDGE, PUBLIC_EXPORT_CHECK,
    RUN_NUMBER, SHA256, UNRELATED_VIEWER_CHECK, checked_placement, cli_payload_bytes, decode, edge_head_denials, edge_route_probe_names,
    fields, matches, require, requires_cli_workload, scope_parts, require_isolated_public_origins,
    verify_expiry_full_window, verify_expiry_range_window, verify_unrelated_viewer_denial,
)


BODY_LIMIT = 1024 * 1024
VIEW_SUFFIX = re.compile(r"/acl-views/v2/[a-f0-9]{64}/(?:0|[1-9][0-9]{0,19})-[a-f0-9]{64}")
SAFE_COMPONENT = re.compile(r"[a-zA-Z0-9._-]+")
REPOSITORY_PREFIX = re.compile(r"[a-zA-Z0-9._-]+(?:/[a-zA-Z0-9._-]+)*")
MANAGED_AUTHORITY = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?")
MANAGED_SLUG = re.compile(r"[a-z0-9._@+=,-]+")
DEPLOYMENT_FIELDS = {
    "cloudfront": {"distribution_id", "region"},
    "cloud-cdn": {"project", "forwarding_rule"},
    "front-door": {"subscription", "resource_group", "profile"},
}
DEPLOYMENT_VALUE = re.compile(r"[A-Za-z0-9._()-]{1,90}")
S3_REGION = re.compile(r"(?!cn-)[a-z]{2}-[a-z]+-[1-9][0-9]*")
ORIGIN_BUCKET = {
    "cloudfront": re.compile(r"s3://([a-z0-9][a-z0-9.-]{1,61}[a-z0-9])"),
    "cloud-cdn": re.compile(r"gs://([a-z0-9][a-z0-9._-]{1,220}[a-z0-9])"),
    "front-door": re.compile(r"https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/([a-z0-9][a-z0-9-]{1,61}[a-z0-9])"),
}
LIST_QUERY = {
    "cloudfront": "?list-type=2&max-keys=1",
    "cloud-cdn": "?max-keys=1",
    "front-door": "?restype=container&comp=list&maxresults=1",
}
SIGNATURE_FIELDS = ("Algorithm", "Credential", "Date", "Expires", "SignedHeaders", "Signature")
BLOB_SAS_REQUIRED = {"sv", "se", "sr", "sp", "sig"}
BLOB_SAS_OPTIONAL = {"spr", "st", "sip", "skoid", "sktid", "skt", "ske", "sks", "skv", "saoid", "suoid", "scid", "ses"}
FAMILY_ROOTS = {
    "git": {"v2/pack-layers"},
    "capsule": {"v2/capsules", "v2/checkpoints", "v2/catalogs", "v2/history", "v2/history/source-roots"},
    "lfs": {"lfs/objects"},
    "xorb": {".crab/xorbs"},
    "chunk-page": {".crab/capsule-chunk-pages"},
    "shard": {".crab/shards"},
}
IMMUTABLE_ROOTS = set().union(*FAMILY_ROOTS.values())
CACHE_CODES = {
    "cloudfront": ("X-Cache", {"Hit from cloudfront": "hit", "Miss from cloudfront": "miss"}),
    "cloud-cdn": ("X-Crab-CDN-Cache", {"hit": "hit", "miss": "miss"}),
    "front-door": ("X-Cache", {"TCP_HIT": "hit", "TCP_REMOTE_HIT": "hit", "TCP_MISS": "miss"}),
}


def approved_fixture(path: Path, configuration_sha256: str, *, operator_binding: bool = False) -> dict[str, Any]:
    with path.open("rb") as source:
        raw = source.read(16 * 1024 + 1)
    require(len(raw) <= 16 * 1024 and hashlib.sha256(raw).hexdigest() == configuration_sha256,
            "CDN fixture differs from the approved configuration digest")
    fixture = decode(raw)
    require(isinstance(fixture, dict) and isinstance(fixture.get("scope"), str),
            "fixture must identify a scope")
    edge, access, consumer, family, profile = scope_parts(fixture["scope"])
    expected = {
        "scope", "base_url", "native_origin", "fixture_prefix", "view_suffix", "neighbor_prefix",
        "object_path", "size", "sha256",
    }
    if operator_binding:
        require(access == "private", "operator binding collection requires a private deployment")
        expected |= {"deployment"}
    else:
        expected.add("deployment_policy_sha256")
        if access == "public":
            expected |= {"export_origin", "source_placement"}
        else:
            expected |= {"unrelated_control", "viewer_issuer"} & fixture.keys()
            if (consumer, family, profile) == ("cli", "git", "managed"):
                expected.add("managed_url")
        expected |= {"deployment"} & fixture.keys()
        if requires_cli_workload(fixture["scope"]) and "cli_payload_bytes" in fixture:
            expected.add("cli_payload_bytes")
            cli_payload_bytes(fixture)
    fields(fixture, expected, "CDN fixture")
    if not operator_binding:
        require(matches(SHA256, fixture["deployment_policy_sha256"]), "fixture needs an approved deployment policy digest")
    if "deployment" in fixture:
        deployment_target(fixture, edge)
    fixture_base(fixture["base_url"])
    if "viewer_issuer" in fixture:
        from cdn_viewer_issuer import issuer_settings

        issuer_settings(fixture)
    fixture_repository_prefix(fixture, access, family, operator_binding=operator_binding)
    if "managed_url" in fixture:
        locator = fixture["managed_url"]
        require(isinstance(locator, str), "invalid managed qualification repository")
        parsed = urlsplit(locator)
        parts = parsed.path.split("/")
        require(len(locator) <= 255 and parsed.scheme == "crab" and len(parsed.netloc) <= 253
                and all(matches(MANAGED_AUTHORITY, label) for label in parsed.netloc.split("."))
                and len(parts) == 3 and all(matches(MANAGED_SLUG, part) and part not in {".", ".."} for part in parts[1:])
                and locator == f"crab://{parsed.netloc}{parsed.path}" and fixture["view_suffix"] == "",
                "managed qualification needs an exact repository-local logical URL")
    origin_bucket(fixture["native_origin"], edge)
    if access == "public":
        origin_bucket(fixture["export_origin"], edge)
        require_isolated_public_origins(fixture)
        checked_placement(fixture["native_origin"], fixture["source_placement"])
    return fixture


def deployment_target(fixture: dict[str, Any], edge: str) -> dict[str, str]:
    target = fields(fixture.get("deployment"), DEPLOYMENT_FIELDS[edge], "CDN deployment target")
    require(all(matches(DEPLOYMENT_VALUE, value) for value in target.values()), "invalid CDN deployment target identifiers")
    return target


def origin_bucket(origin: Any, edge: str) -> str:
    match = ORIGIN_BUCKET[edge].fullmatch(origin) if isinstance(origin, str) else None
    require(match is not None, "qualification fixture needs a canonical native origin for the selected provider")
    return match.group(1)


def native_remote(fixture: dict[str, Any], edge: str) -> str:
    bucket = origin_bucket(fixture["native_origin"], edge)
    return f"crab://{bucket}/{fixture['fixture_prefix']}"


def fixture_base(value: Any) -> SplitResult:
    require(isinstance(value, str) and not re.search(r"[\x00-\x20\x7f%?#\\]", value),
            "invalid CDN fixture URL")
    base = urlsplit(value)
    require(base.scheme == "https" and base.hostname is not None and base.username is None
            and all(matches(SAFE_COMPONENT, part) and part not in {".", ".."}
                    for part in base.path.strip("/").split("/") if part),
            "CDN fixture requires an HTTPS base without credentials or ambiguous path components")
    require("//" not in base.path and base.port != 0, "invalid CDN fixture URL")
    return base


def fixture_repository_prefix(fixture: dict[str, Any], access: str, family: str, *, operator_binding: bool = False) -> str:
    repo, neighbor, view = (fixture[key] for key in ("fixture_prefix", "neighbor_prefix", "view_suffix"))
    if operator_binding:
        require(matches(REPOSITORY_PREFIX, repo) and len(repo) <= 255
                and all(part not in {".", ".."} for part in repo.split("/")),
                "operator binding requires a canonical repository prefix")
        require(matches(PREFIX, neighbor) and repo != neighbor
                and not (repo.startswith(neighbor + "/") or neighbor.startswith(repo + "/")),
                "operator binding requires a separate qualification namespace for the denial probe")
    else:
        require(matches(PREFIX, repo) and matches(PREFIX, neighbor) and repo != neighbor,
                "two distinct qualification repository prefixes are required")
    require(view == "" or (access == "private" and matches(VIEW_SUFFIX, view)),
            "qualification view must be an exact private materialized namespace")
    if family in {"xorb", "chunk-page", "shard"}:
        require(bool(view), "bucket-global families require a protected qualification view")
    prefix = fixture_object_prefix(fixture, FAMILY_ROOTS[family])
    if "unrelated_control" in fixture:
        control = fields(fixture["unrelated_control"], {"object_path", "size", "sha256"}, "unrelated viewer control")
        other_prefix = fixture_object_prefix(control, IMMUTABLE_ROOTS)
        require(other_prefix != prefix and (view or not other_prefix.startswith(".crab/")),
                "unrelated viewer control needs a disjoint immutable grant prefix in the approved namespace")
    return repo + view


def fixture_object_prefix(fixture: dict[str, Any], roots: set[str]) -> str:
    relative = fixture["object_path"]
    require(isinstance(relative, str), "invalid qualification object key")
    parts = relative.split("/")
    digest = parts[-1]
    depth = 3 if relative.startswith("lfs/objects/") else 2
    fanout = [digest[:2], digest[2:4]] if depth == 3 else [digest[:2]]
    root = "/".join(parts[:-depth])
    require(matches(SHA256, digest) and parts[-depth:-1] == fanout and root in roots,
            "qualification key does not match the declared immutable read family")
    body_size = fixture["size"]
    require(type(body_size) is int and 2 <= body_size <= BODY_LIMIT
            and matches(SHA256, fixture["sha256"]), "invalid qualification object size or digest")
    if root == "lfs/objects":
        require(digest == fixture["sha256"], "LFS key must match the approved body digest")
    # The viewer grant treats source-roots as part of the history prefix.
    return "v2/history" if root == "v2/history/source-roots" else root


def fixture_object_key(fixture: dict[str, Any], access: str, family: str) -> str:
    return f"{fixture_repository_prefix(fixture, access, family)}/{fixture['object_path']}"


def viewer_headers(edge: str, access: str, credential_file: Path | None) -> dict[str, str]:
    if access == "public":
        require(credential_file is None, "public qualification uses anonymous reads")
        return {}
    require(credential_file is not None, "private qualification requires a viewer credential file")
    with credential_file.open("rb") as credential:
        raw = credential.read(8195)
    require(len(raw) <= 8194, "invalid viewer credential")
    return private_viewer_headers(edge, raw.removesuffix(b"\n").removesuffix(b"\r").decode("latin-1"))


def private_viewer_headers(edge: str, value: str) -> dict[str, str]:
    require(isinstance(value, str) and re.fullmatch(r"[\x20-\x7e]{1,8192}", value) is not None,
            "invalid viewer credential")
    if edge == "front-door":
        return {"Authorization": "Bearer " + value}
    return {"Cookie": value}


def viewer_policy(edge: str, viewer: dict[str, str]) -> tuple[int, Any]:
    try:
        if edge == "cloud-cdn":
            cookie = re.fullmatch(r"Cloud-CDN-Cookie=URLPrefix=([A-Za-z0-9_=-]+):Expires=([0-9]{1,20})"
                                  r":KeyName=[A-Za-z0-9_-]+:Signature=[A-Za-z0-9_=-]+", viewer["Cookie"])
            require(cookie is not None, "invalid signed cookie")
            policy = cookie[1]
            expires = int(cookie[2])
        elif edge == "cloudfront":
            cookies = (part.strip() for part in viewer["Cookie"].split(";"))
            policies = [part.removeprefix("CloudFront-Policy=") for part in cookies if part.startswith("CloudFront-Policy=")]
            require(len(policies) == 1, "invalid signed cookie policy")
            encoded = policies[0].translate(str.maketrans("-_~", "+=/"))
            policy = decode(base64.b64decode(encoded, validate=True))
            statements = policy["Statement"]
            require(isinstance(statements, list) and len(statements) == 1, "invalid signed cookie statement")
            expires = statements[0]["Condition"]["DateLessThan"]["AWS:EpochTime"]
        else:
            _, encoded, _ = viewer["Authorization"].removeprefix("Bearer ").split(".")
            payload = base64.b64decode(encoded + "=" * (-len(encoded) % 4), altchars=b"-_", validate=True)
            policy = decode(payload)
            expires = policy["exp"]
        require(type(expires) is int and expires > 0, "invalid expiry claim")
    except (ValueError, KeyError, TypeError, RecursionError):
        raise ValueError("expiry probe needs a viewer credential with a readable signed expiration") from None
    return expires, policy


def viewer_expiry(edge: str, viewer: dict[str, str]) -> int:
    return viewer_policy(edge, viewer)[0]


def viewer_grant_fingerprint(viewer: dict[str, str]) -> str:
    return hashlib.sha256(json.dumps(viewer, sort_keys=True).encode()).hexdigest()


@contextmanager
def https_response(
    base: SplitResult, method: str, target: str, headers: dict[str, str], check: str, *, body: bytes | None = None,
) -> Iterator[http.client.HTTPResponse]:
    connection = http.client.HTTPSConnection(base.hostname, base.port, timeout=15)
    try:
        if body is None:
            connection.request(method, target, headers=headers)
        else:
            connection.request(method, target, headers=headers, body=body)
        response = connection.getresponse()
        try:
            yield response
        finally:
            response.close()
    except (http.client.HTTPException, OSError):
        # HTTP exceptions can include a request line or headers.
        raise ValueError(f"{check} failed to read the HTTPS fixture") from None
    finally:
        connection.close()


def native_base(origin: str, edge: str) -> SplitResult:
    bucket = origin_bucket(origin, edge)
    if edge == "front-door":
        return urlsplit(origin)
    if edge == "cloud-cdn":
        return urlsplit(f"https://storage.googleapis.com/{bucket}")

    # A general-purpose bucket returns its region even on an anonymous 403.
    # Use the provider's regional REST endpoint without following redirects.
    discovery = urlsplit("https://s3.amazonaws.com")
    with https_response(discovery, "HEAD", f"/{bucket}", {}, "S3 bucket region lookup") as response:
        regions = response.headers.get_all("x-amz-bucket-region", [])
        require(response.status in (200, 301, 403) and len(regions) == 1
                and len(regions[0]) <= 20 and matches(S3_REGION, regions[0]),
                "S3 native origin did not report a commercial bucket region")
    return urlsplit(f"https://s3.{regions[0]}.amazonaws.com/{bucket}")


def signed_native_target(raw: bytes, native: SplitResult, edge: str, key: str) -> SplitResult:
    value = raw.removesuffix(b"\n").removesuffix(b"\r")
    require(re.fullmatch(rb"[\x21-\x7e]{1,16384}", value) is not None and b"#" not in value,
            "invalid signed native read URL")
    try:
        url = urlsplit(value.decode("ascii"))
        pairs = parse_qsl(url.query, keep_blank_values=True, strict_parsing=True, max_num_fields=24)
    except ValueError:
        raise ValueError("invalid signed native read URL") from None

    endpoint_paths = {(native.netloc, native.path)}
    if edge != "front-door":
        bucket = native.path.removeprefix("/")
        endpoint_paths.add((f"{bucket}.{native.netloc}", ""))
        if native.netloc == "s3.us-east-1.amazonaws.com":
            endpoint_paths |= {("s3.amazonaws.com", native.path), (f"{bucket}.s3.amazonaws.com", "")}
    suffix = f"/{key}"
    prefix = url.path.removesuffix(suffix)
    require(url.scheme == "https" and url.path == prefix + suffix and (url.netloc, prefix) in endpoint_paths,
            "signed native read URL must address the approved native origin and exact object key")

    params = dict(pairs)
    require(len(params) == len(pairs) and all(params.values()), "invalid signed native read parameters")
    if edge == "front-door":
        require(BLOB_SAS_REQUIRED <= params.keys() <= (BLOB_SAS_REQUIRED | BLOB_SAS_OPTIONAL)
                and params["sr"] == "b" and params["sp"] == "r", "native Blob URL requires a read-only blob SAS")
    else:
        signing = "X-Amz-" if edge == "cloudfront" else "X-Goog-"
        required = {signing + field for field in SIGNATURE_FIELDS}
        optional = {"X-Amz-Security-Token"} if edge == "cloudfront" else set()
        require(required <= params.keys() <= (required | optional) and params[signing + "SignedHeaders"] == "host",
                "native object URL requires a V4 signature without extra request parameters")
    return url._replace(path=prefix)


def cloudfront_cache_timing(response: http.client.HTTPResponse) -> dict[str, Any]:
    metrics = {}
    descriptions = {"cdn-pop": r"[A-Z0-9][A-Z0-9-]{0,31}", "cdn-hit-layer": r"EDGE|REC|Origin Shield"}
    for header in response.headers.get_all("Server-Timing", []):
        for metric in header.split(","):
            name, _, argument = metric.strip().partition(";")
            name = name.strip()
            if name in descriptions:
                match = re.fullmatch(rf'desc\s*=\s*"({descriptions[name]})"', argument.strip())
                require(name not in metrics and match is not None, "invalid or repeated CloudFront cache timing metric")
                metrics[name] = match[1]
    require("cdn-pop" in metrics, "CloudFront cache reuse requires an observed point of presence")
    return {"cloudfront_pop_sha256": hashlib.sha256(metrics["cdn-pop"].encode()).hexdigest(),
            "cloudfront_hit_layer": metrics.get("cdn-hit-layer")}


def request_sample(
    base: SplitResult, edge: str | None, key: str, viewer: dict[str, str], *, size: int, check: str,
    wanted: int | None = None, range_offset: int | None = None, query: str = "", method: str = "GET",
    denied_statuses: tuple[int, ...] = (401, 403), origin_authorization: str | None = None,
) -> tuple[dict[str, Any], bytes]:
    headers = viewer | {"Accept-Encoding": "identity"}
    if edge == "cloudfront" and check == CACHE_REUSE_CHECK:
        headers["Pragma"] = "server-timing"
    if edge is None:
        require(not viewer, "native origin probes cannot use viewer credentials")
        if not base.query and base.netloc.endswith(".blob.core.windows.net"):
            # This API version requests the bearer challenge for anonymous denial.
            headers["x-ms-version"] = "2019-12-12"
    if origin_authorization is not None:
        blob = matches(ORIGIN_BUCKET["front-door"], base.geturl())
        require(edge is None and method == "GET" and wanted == 200 and not base.query and not query
                and base.scheme == "https" and (base.netloc == "storage.googleapis.com" or blob),
                "native credentials require an authenticated GCS or Blob object GET")
        headers["Authorization"] = origin_authorization
        if blob:
            headers["x-ms-date"] = formatdate(usegmt=True)
    if range_offset is not None:
        headers["Range"] = f"bytes={range_offset}-{range_offset}"
    if check in ("mutable_route_denied", "neighbor_route_denied"):
        # Keep these unrelated probe paths out of caches if a fixture is misconfigured.
        headers["Cache-Control"] = "no-store"
    length = 1 if range_offset is not None else size
    started = time.monotonic_ns()
    target = base.path.rstrip("/") + (f"/{key}" if key else "") + query
    with https_response(base, method, target, headers, check) as response:
        if wanted is None:
            require(response.status in denied_statuses, f"{check} expected denial, got HTTP {response.status}")
            body = b""
        else:
            require(response.status == wanted, f"{check} expected HTTP {wanted}, got {response.status}")
            expected_range = [f"bytes {range_offset}-{range_offset}/{size}"] if range_offset is not None else None
            require(response.headers.get_all("Content-Range") == expected_range, f"{check} returned the wrong range")
            require(response.headers.get_all("Content-Encoding") in (None, ["identity"]),
                    f"{check} transformed the object representation")
            require(response.headers.get_all("Content-Length") in (None, [str(length)]),
                    f"{check} returned the wrong length")
            if viewer:
                directives = {value.strip().lower() for value in response.getheader("Cache-Control", "").split(",")}
                require({"private", "no-store"} <= directives, f"{check} allows downstream caching")
            body = response.read(length + 1)
            require(len(body) == length, f"{check} returned the wrong length")
        cache = "unknown"
        if edge is not None:
            cache_header, codes = CACHE_CODES[edge]
            cache = codes.get(response.getheader(cache_header), "unknown")
        observation = {
            "source": "origin" if edge is None else "edge", "status": response.status, "cache": cache, "bytes": len(body),
            "elapsed_ms": (time.monotonic_ns() - started) // 1_000_000,
            "key_sha256": hashlib.sha256((key or target).encode("utf-8")).hexdigest(),
        }
        if check == CACHE_REUSE_CHECK:
            if edge == "cloudfront":
                observation.update(cloudfront_cache_timing(response))
            elif edge == "front-door":
                references = response.headers.get_all("X-Azure-Ref", [])
                require(len(references) == 1 and re.fullmatch(r"[\x21-\x7e]{1,256}", references[0]),
                        "Front Door cache reuse requires one viewer tracking reference")
                observation["front_door_request_sha256"] = hashlib.sha256(references[0].encode()).hexdigest()
        return observation, body


def authenticated_origin_read(
    fixture: dict[str, Any], native: SplitResult, edge: str, key: str, size: int, credential_file: Path | None,
) -> tuple[dict[str, Any], bytes]:
    from collect_cdn_fixture_cleanup import bearer, command_output

    if credential_file is not None:
        with credential_file.open("rb") as credential:
            signed = signed_native_target(credential.read(16387), native, edge, key)
    elif edge == "cloudfront":
        raw = command_output(
            "aws", "s3", "presign", f"{fixture['native_origin']}/{key}", "--region", native.netloc.split(".")[1],
            "--endpoint-url", f"https://{native.netloc}", "--expires-in", "60", "--no-cli-pager", "--no-cli-auto-prompt",
        )
        signed = signed_native_target(raw, native, edge, key)
    else:
        if edge == "cloud-cdn":
            token = bearer("gcloud", "auth", "print-access-token", "--quiet")
        else:
            token = bearer("az", "account", "get-access-token", "--resource", "https://storage.azure.com/",
                           "--query", "accessToken", "--output", "tsv", "--only-show-errors")
        return request_sample(native, None, key, {}, size=size, check="native_origin_read", wanted=200,
                              origin_authorization=token)
    return request_sample(signed, None, key, {}, size=size, check="native_origin_read", wanted=200, query=f"?{signed.query}")


def collect_origin_access(
    fixture: dict[str, Any], edge: str, access: str, key: str, body: bytes,
    *, native_read_url_file: Path | None = None, native_identity_read: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    check = "private_origin_denied" if access == "private" else "live_origin_denied"
    native = native_base(fixture["native_origin"], edge)
    denied, _ = request_sample(native, None, key, {}, size=len(body), check=check)
    traces = {check: [denied]}
    if native_read_url_file is not None or native_identity_read:
        observed, raw = authenticated_origin_read(fixture, native, edge, key, len(body), native_read_url_file)
        require(raw == body, "authenticated native origin returned different object bytes")
        traces["native_origin_read"] = [observed]
    if access == "public":
        export = native_base(fixture["export_origin"], edge)
        allowed = 200 if edge != "cloudfront" else None
        observed, raw = request_sample(export, None, key, {}, size=len(body), check="export_origin_access", wanted=allowed)
        require(allowed is None or raw == body, "anonymous export origin returned different object bytes")
        listing, _ = request_sample(export, None, "", {}, size=0, check="public_listing_denied", query=LIST_QUERY[edge])
        traces |= {"export_origin_access": [observed], "public_listing_denied": [listing]}
    return traces


def warm_full_read(base: SplitResult, edge: str, key: str, viewer: dict[str, str], body: bytes) -> dict[str, Any]:
    read, cached = request_sample(base, edge, key, viewer, size=len(body), check="warm edge GET", wanted=200)
    require(read["cache"] == "hit" and cached == body, "warm edge GET requires a cache hit with the approved bytes")
    return read


def collect_edge_route_policy(
    base: SplitResult, edge: str, access: str, key: str, viewer: dict[str, str], body: bytes,
) -> list[dict[str, Any]]:
    first = warm_full_read(base, edge, key, viewer, body)
    head, _ = request_sample(base, edge, key, viewer, size=len(body), check="edge HEAD policy",
                             method="HEAD", denied_statuses=edge_head_denials(edge, access))
    probes = [first, head]
    for query in EDGE_ROUTE_QUERY_PROBES[edge].values():
        denied, _ = request_sample(base, edge, key, viewer, size=len(body), check="edge query policy", query=query)
        probes.append(denied)
    probes.append(warm_full_read(base, edge, key, viewer, body))
    return probes


def unrelated_viewer_samples(
    base: SplitResult, edge: str, repo: str, target: str, target_size: int,
    control: dict[str, Any], viewer: dict[str, str], expires_at: int,
) -> list[dict[str, Any]]:
    control_key = f"{repo}/{control['object_path']}"
    grant = {"viewer_expires_at_unix": expires_at, "viewer_grant_sha256": viewer_grant_fingerprint(viewer)}
    samples = []
    for key, size, wanted, offset in (
        (control_key, control["size"], 200, None),
        (target, target_size, None, None),
        (target, target_size, None, 0),
        (control_key, control["size"], 200, None),
    ):
        started = time.time_ns() // 1_000
        row, body = request_sample(base, edge, key, viewer, size=size, check=UNRELATED_VIEWER_CHECK,
                                   wanted=wanted, range_offset=offset)
        finished = time.time_ns() // 1_000
        digest = hashlib.sha256(body).hexdigest() if wanted is not None else None
        require(digest is None or digest == control["sha256"], "unrelated viewer control differs from the approved bytes")
        samples.append(row | grant | {"body_sha256": digest, "range_offset": offset,
                                      "started_at_unix_us": started, "finished_at_unix_us": finished})
    return samples


def parallel_range_samples(
    base: SplitResult, edge: str, key: str, viewer: dict[str, str], body: bytes, *, expired: bool,
) -> list[dict[str, Any]]:
    ready = Barrier(2, timeout=5)

    def read(offset: int) -> dict[str, Any]:
        try:
            ready.wait()
        except BrokenBarrierError:
            raise ValueError("parallel expiry range workers did not start together") from None
        started = time.time_ns() // 1_000
        observed, payload = request_sample(base, edge, key, viewer, size=len(body), check=EXPIRY_RANGE_CHECK,
                                           wanted=None if expired else 206, range_offset=offset)
        finished = time.time_ns() // 1_000
        require(expired or payload == body[offset:offset + 1], "parallel range differs from the approved byte")
        return observed | {"range_offset": offset, "started_at_unix_us": started, "finished_at_unix_us": finished}

    with ThreadPoolExecutor(max_workers=2) as executor:
        return list(executor.map(read, (0, 1)))


def collect_expiry_window(
    fixture: dict[str, Any], edge: str, family: str, viewer: dict[str, str], expires_at: int,
    *, operator_binding: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    base = fixture_base(fixture["base_url"])
    repo = fixture_repository_prefix(fixture, "private", family, operator_binding=operator_binding)
    key = f"{repo}/{fixture['object_path']}"

    def full_sample(wanted: int | None) -> tuple[dict[str, Any], bytes]:
        started = time.time_ns() // 1_000
        observed, payload = request_sample(base, edge, key, viewer, size=fixture["size"],
                                           check=EXPIRY_FULL_CHECK, wanted=wanted)
        return observed | {"started_at_unix_us": started, "finished_at_unix_us": time.time_ns() // 1_000}, payload

    warm, body = full_sample(200)
    require(warm["cache"] == "hit" and hashlib.sha256(body).hexdigest() == fixture["sha256"],
            "expiry check requires an authorized cache hit with the approved bytes")
    before = parallel_range_samples(base, edge, key, viewer, body, expired=False)
    require(time.time() < expires_at, "parallel expiry ranges must complete before the grant expires")
    time.sleep(max(0.0, expires_at + EXPIRY_SETTLE_SECONDS - time.time()))
    after = parallel_range_samples(base, edge, key, viewer, body, expired=True)
    denied, _ = full_sample(None)
    verify_expiry_range_window(before + after, expires_at)
    verify_expiry_full_window([warm, denied], expires_at)
    return {EXPIRY_FULL_CHECK: [warm, denied], EXPIRY_RANGE_CHECK: before + after}


def collect(
    fixture: dict[str, Any], edge: str, access: str, family: str, viewer: dict[str, str],
    expires_at: int | None, *, unrelated_viewer: dict[str, str] | None = None,
    native_read_url_file: Path | None = None, native_identity_read: bool = False,
    operator_binding: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    base = fixture_base(fixture["base_url"])
    repo = fixture_repository_prefix(fixture, access, family, operator_binding=operator_binding)
    scope_key = f"{repo}/{fixture['object_path']}"
    size = fixture["size"]
    require(bool(viewer) == (access == "private"), "private transport qualification requires an authorized viewer grant")
    if expires_at is not None:
        require(access == "private" and 5 <= expires_at - time.time() <= 120,
                "expiry check needs a private credential expiring within 5 to 120 seconds")
        require(viewer_expiry(edge, viewer) == expires_at, "expiry argument differs from the viewer grant")
    if unrelated_viewer is not None:
        require(access == "private" and "unrelated_control" in fixture and unrelated_viewer != viewer,
                "unrelated viewer check requires a separate private grant and approved control object")
        from cdn_viewer_issuer import verify_viewer_scope

        control = fixture["unrelated_control"]
        prefix = f"{repo}/{fixture_object_prefix(control, IMMUTABLE_ROOTS)}/"
        unrelated_expires_at = verify_viewer_scope(
            edge, unrelated_viewer, fixture["native_origin"], base.geturl().rstrip("/"), repo, prefix,
        )

    full, body = request_sample(base, edge, scope_key, viewer, size=size, check="exact_get", wanted=200)
    observed_digest = hashlib.sha256(body).hexdigest()
    require(observed_digest == fixture["sha256"], "exact_get differs from the approved bytes")
    ranged, first_byte = request_sample(base, edge, scope_key, viewer, size=size, check="exact_range",
                                       wanted=206, range_offset=0)
    require(first_byte == body[:1], "exact_range differs from the approved bytes")
    traces = {"exact_get": [full], "exact_range": [ranged]}
    if access == "public":
        traces[PUBLIC_EXPORT_CHECK] = [full | {"body_sha256": observed_digest}]
    for check, key in (
        ("mutable_route_denied", f"{repo}/v2/root"),
        ("neighbor_route_denied", f"{fixture['neighbor_prefix']}/{fixture['object_path']}"),
    ):
        denied, _ = request_sample(base, edge, key, viewer, size=size, check=check, range_offset=0)
        traces[check] = [denied]

    route = collect_edge_route_policy(base, edge, access, scope_key, viewer, body)
    traces["edge_route_policy"] = route
    if access == "private":
        check = "warm_anonymous_viewer_denied"
        warm = route[-1]
        anonymous, _ = request_sample(base, edge, scope_key, {}, size=size, check=check)
        anonymous_range, _ = request_sample(base, edge, scope_key, {}, size=size, check=check, range_offset=0)
        confirmed = warm_full_read(base, edge, scope_key, viewer, body)
        traces[check] = [warm, anonymous, anonymous_range, confirmed]
        if unrelated_viewer is not None:
            controls = unrelated_viewer_samples(base, edge, repo, scope_key, size, control,
                                                unrelated_viewer, unrelated_expires_at)
            samples = [confirmed, *controls, warm_full_read(base, edge, scope_key, viewer, body)]
            verify_unrelated_viewer_denial(samples)
            traces[UNRELATED_VIEWER_CHECK] = samples
        if expires_at is not None:
            traces.update(collect_expiry_window(fixture, edge, family, viewer, expires_at, operator_binding=operator_binding))
    return traces | collect_origin_access(fixture, edge, access, scope_key, body,
                                          native_read_url_file=native_read_url_file, native_identity_read=native_identity_read)


def private_binding_candidate(
    fixture: dict, policy: dict, traces: dict, configuration_sha256: str, *,
    operator_binding: bool = False,
) -> dict:
    binding = policy.get("private_binding")
    require(isinstance(binding, dict), "private binding requires an observed deployment and native placement")
    edge, _, _, family, _ = scope_parts(fixture["scope"])
    repo = fixture_repository_prefix(fixture, "private", family, operator_binding=operator_binding)
    key = f"{repo}/{fixture['object_path']}"
    key_sha256 = hashlib.sha256(key.encode()).hexdigest()
    for check, source in (("exact_get", "edge"), ("native_origin_read", "origin")):
        samples = traces.get(check, [])
        require(len(samples) == 1 and all(samples[0].get(name) == value for name, value in {
            "source": source, "status": 200, "bytes": fixture["size"], "key_sha256": key_sha256,
        }.items()), "private binding requires successful edge and authenticated native reads of the same object")
    # The transport collector has compared both complete bodies to the approved fixture digest.
    return {
        "schema": "crab.cdn-private-binding-candidate/v1",
        "edge": edge,
        "binding": binding,
        "configuration_sha256": configuration_sha256,
        "deployment_policy_sha256": policy["deployment_policy_sha256"],
        "object": {"key_sha256": key_sha256, "body_sha256": fixture["sha256"], "bytes": fixture["size"]},
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", choices=sorted(PROVIDERS))
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--trace-directory", type=Path, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--viewer-file", type=Path)
    parser.add_argument("--github-viewer", action="store_true", help="mint the viewer grant when the approved fixture selects GitHub OIDC")
    parser.add_argument("--unrelated-viewer-file", type=Path)
    parser.add_argument("--viewer-expires-at", type=int, metavar="UNIX_SECONDS")
    native_reader = parser.add_mutually_exclusive_group()
    native_reader.add_argument("--native-read-url-file", type=Path)
    native_reader.add_argument("--native-identity-read", action="store_true",
                               help="authenticate the exact native GET with the current provider identity")
    parser.add_argument("--attest-deployment", action="store_true", help="verify the approved cloud policy before and after transport")
    parser.add_argument("--private-binding-candidate", type=Path,
                        help="write the observed private deployment and placement after attested native and edge reads")
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
        require(not args.trace_directory.exists(), "CDN trace directory must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, access, _, family, _ = scope_parts(fixture["scope"])
        if args.private_binding_candidate is not None:
            require(access == "private" and args.attest_deployment
                    and (args.native_identity_read or args.native_read_url_file is not None),
                    "private binding candidate requires a private fixture, deployment attestation and an authenticated native read")
            require(not args.private_binding_candidate.exists() and args.private_binding_candidate.parent.is_dir()
                    and not args.private_binding_candidate.resolve().is_relative_to(args.trace_directory.resolve()),
                    "private binding candidate requires a new file outside the trace directory")
        require(args.provider is None or PROVIDER_FOR_EDGE[edge] == args.provider,
                "CDN fixture provider differs from the selected native provider")
        github_viewer = args.github_viewer and "viewer_issuer" in fixture
        if github_viewer:
            require(args.attest_deployment and args.viewer_file is None and args.viewer_expires_at is None,
                    "GitHub viewer issuance requires deployment attestation and exclusive primary authentication")
        else:
            viewer = viewer_headers(edge, access, args.viewer_file)
        unrelated = viewer_headers(edge, access, args.unrelated_viewer_file) if args.unrelated_viewer_file else None
        if args.attest_deployment:
            from cdn_deployment_policy_cli import attest_fixture

            attest_fixture(fixture, edge, access)
        if github_viewer:
            from cdn_viewer_issuer import issue_github_viewer

            viewer = issue_github_viewer(fixture, edge, family)
            primary_received_at = time.time()
        traces = collect(fixture, edge, access, family, viewer, args.viewer_expires_at,
                         unrelated_viewer=unrelated, native_read_url_file=args.native_read_url_file,
                         native_identity_read=args.native_identity_read)
        expiry_viewer = viewer
        expires_at = args.viewer_expires_at
        if github_viewer:
            # A second grant issued in the same second can have identical signed claims.
            time.sleep(max(0.0, int(primary_received_at) + 1 - time.time()))
            expiry_viewer = issue_github_viewer(fixture, edge, family)
            require(expiry_viewer != viewer, "hosted expiry check requires a separately signed viewer grant")
            expires_at = viewer_expiry(edge, expiry_viewer)
            require(5 <= expires_at - time.time() <= MAX_VIEWER_GRANT_SECONDS,
                    "hosted expiry grant has insufficient time or exceeds the maximum signed lifetime")
            traces.update(collect_expiry_window(fixture, edge, family, expiry_viewer, expires_at))
        expiry_metadata = {}
        if expires_at is not None:
            expiry_metadata = {
                "viewer_expires_at_unix": expires_at,
                "viewer_grant_sha256": viewer_grant_fingerprint(expiry_viewer),
            }
        policy = attest_fixture(fixture, edge, access) if args.attest_deployment else None
        provenance = {
            "schema": "crab.cdn-trace/v1", "source_sha": args.source_sha, "scope": fixture["scope"],
            "configuration_sha256": args.configuration_sha256,
            "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
        }
        candidate = (private_binding_candidate(fixture, policy, traces, args.configuration_sha256)
                     if args.private_binding_candidate is not None else None)
        args.trace_directory.mkdir(parents=True, exist_ok=False)
        for check, requests in traces.items():
            metadata = expiry_metadata if check in EXPIRY_CHECKS else {}
            if check == "edge_route_policy":
                requests = [row | {"probe": probe} for row, probe in zip(requests, edge_route_probe_names(edge), strict=True)]
            encoded = json.dumps(provenance | metadata | {"check": check, "requests": requests}, sort_keys=True)
            (args.trace_directory / f"{check}.json").write_text(encoded + "\n", encoding="utf-8")
        if policy is not None:
            record = json.dumps(provenance | {name: value for name, value in policy.items() if name != "private_binding"},
                                sort_keys=True)
            (args.trace_directory / "deployment_policy.json").write_text(record + "\n", encoding="utf-8")
        if candidate is not None:
            with args.private_binding_candidate.open("x", encoding="utf-8") as output:
                output.write(json.dumps(candidate, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"Recorded {len(traces)} CDN transport checks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
