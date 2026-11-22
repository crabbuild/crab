#!/usr/bin/env python3
"""Measure one verified read of a dedicated Crab HTTP or S3 gateway fixture."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import http.client
import os
import re
import stat
import sys
import time
from pathlib import Path
from urllib.parse import SplitResult

from collect_cdn_product_metrics import measure, measure_native
from run_cdn_git_workload import runner_platform
from run_cdn_sdk_workload import json_record
from run_cdn_transport_checks import approved_fixture, fixture_base, fixture_object_key, https_response
from s3_gateway_workload import _canonical_path, _signed_headers

from cdn_qualification import (
    COMMIT, GIT_FETCH_RECORD_LIMIT, GIT_OID, PRODUCT_COUNTERS, RUN_NUMBER, SHA256,
    decode, fields, is_direct_service_scope, matches, positive, read_git_record, require, scope_parts,
)


PAYLOAD = "cdn-qualification.bin"
SERVICE_WORKLOAD_FIELDS = {
    "schema", "configuration_sha256", "publication_sha256", "endpoint", "repository", "commit",
    "payload_sha256", "payload_bytes", "etag_sha256",
}
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,79}")
BUCKET = re.compile(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]")
REGION = re.compile(r"[a-z0-9-]{1,32}")
SESSION_COOKIE = re.compile(r"__Host-crab_session=[A-Za-z0-9._~+=/%-]{1,8192}")
BASIC = re.compile(r"Basic [A-Za-z0-9+/]{2,8192}={0,2}")
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
SERVICE_BODY_LIMIT = 512 * 1024 * 1024


def workload(args: argparse.Namespace, consumer: str) -> dict:
    raw = read_git_record(args.workload)
    approved = fields(decode(raw), SERVICE_WORKLOAD_FIELDS, "CDN service workload")
    require(hashlib.sha256(raw).hexdigest() == args.workload_sha256
            and approved["schema"] == "crab.cdn-service-workload/v1"
            and approved["configuration_sha256"] == args.configuration_sha256
            and all(matches(SHA256, approved[field]) for field in ("publication_sha256", "payload_sha256", "etag_sha256"))
            and matches(GIT_OID, approved["commit"]) and positive(approved["payload_bytes"])
            and approved["payload_bytes"] <= SERVICE_BODY_LIMIT,
            "CDN service read needs the exact approved workload, publication, and payload")
    repo = approved["repository"]
    require(isinstance(repo, str), "service workload needs the approved logical repository")
    if consumer == "server":
        parts = repo.split("/")
        require(len(parts) == 2 and all(matches(NAME, part) for part in parts), "invalid Crab HTTP repository")
    else:
        require(matches(BUCKET, repo), "invalid logical S3 gateway bucket")
    return approved


def request_target(approved: dict, consumer: str, family: str, credentials: Path) -> tuple[SplitResult, str, dict[str, str]]:
    base = fixture_base(approved["endpoint"])
    require(base.path in ("", "/"), "CDN service evidence requires the dedicated service origin")
    raw = decode(read_git_record(credentials, limit=16 * 1024))
    repo = approved["repository"]
    commit = approved["commit"]
    headers = {"accept-encoding": "identity", "host": base.netloc}
    if consumer == "s3-gateway":
        auth = fields(raw, {"access_key", "secret_key", "region"}, "S3 gateway credential")
        require(matches(re.compile(r"[A-Za-z0-9_-]{1,128}"), auth["access_key"])
                and matches(re.compile(r"[\x21-\x7e]{1,1024}"), auth["secret_key"])
                and matches(REGION, auth["region"]), "invalid dedicated S3 gateway credential")
        path = _canonical_path(repo, f"{commit}/{PAYLOAD}")
        headers |= {"x-amz-content-sha256": EMPTY_SHA256,
                    "x-amz-date": datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%SZ")}
        headers = _signed_headers("GET", path, "", headers, EMPTY_SHA256,
                                  auth["secret_key"], auth["access_key"], auth["region"], headers["x-amz-date"])
    elif family == "git":
        auth = fields(raw, {"cookie"}, "Crab browser credential")
        require(matches(SESSION_COOKIE, auth["cookie"]), "invalid dedicated Crab browser credential")
        headers["cookie"] = auth["cookie"]
        path = f"/api/repos/{repo}/blob?rev={commit}&path={PAYLOAD}"
    else:
        auth = fields(raw, {"authorization"}, "Crab Git credential")
        require(matches(BASIC, auth["authorization"]), "invalid dedicated Crab Git credential")
        headers["authorization"] = auth["authorization"]
        oid, size = approved["payload_sha256"], approved["payload_bytes"]
        path = f"/git/{repo}.git/info/lfs/objects/{oid}?size={size}"
    return base, path, headers


def verify_body(response: http.client.HTTPResponse, approved: dict, *, git: bool, server: bool) -> str | None:
    require(response.status == 200 and response.headers.get_all("Content-Range") is None
            and response.headers.get_all("Content-Encoding") in (None, ["identity"]),
            "service returned an incomplete or transformed response")
    size = approved["payload_bytes"]
    etags = response.headers.get_all("ETag") or []
    require(len(etags) == 1 and re.fullmatch(r'"[\x21\x23-\x7e]+"', etags[0]) is not None
            and hashlib.sha256(etags[0].encode()).hexdigest() == approved["etag_sha256"],
            "service did not return the approved strong ETag")
    require(response.headers.get_all("Content-Length") in (None, [str(size)]), "service returned the wrong payload length")
    payload_hash = hashlib.sha256()
    git_hash = hashlib.sha1(f"blob {size}\0".encode(), usedforsecurity=False) if git else None
    remaining = size
    while remaining:
        chunk = response.read(min(remaining, 64 * 1024))
        require(0 < len(chunk) <= remaining, "service response did not match the approved payload length")
        payload_hash.update(chunk)
        if git_hash is not None:
            git_hash.update(chunk)
        remaining -= len(chunk)
    require(not response.read(1) and payload_hash.hexdigest() == approved["payload_sha256"],
            "service returned different payload bytes")
    if git_hash is None:
        return None
    if server:
        require(response.headers.get_all("x-crab-blob-oid") == [git_hash.hexdigest()],
                "server Git blob ID did not match the verified HTTP body")
    return hashlib.sha256(git_hash.digest()).hexdigest()


def probe(args: argparse.Namespace) -> dict:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    _, access, consumer, family, _ = scope_parts(fixture["scope"])
    require(is_direct_service_scope(fixture["scope"])
            and not fixture["view_suffix"], "service read evidence requires a direct Git or LFS repository fixture")
    approved = workload(args, consumer)
    lfs_key = None
    if family == "lfs":
        require(fixture["sha256"] == approved["payload_sha256"] and fixture["size"] == approved["payload_bytes"],
                "service LFS workload differs from the approved object")
        lfs_key = hashlib.sha256(fixture_object_key(fixture, access, family).encode()).hexdigest()
    base, path, headers = request_target(approved, consumer, family, args.credentials)
    with os.fdopen(os.open(args.service_log, os.O_RDONLY | os.O_NONBLOCK), "rb") as log:
        file = os.fstat(log.fileno())
        require(stat.S_ISREG(file.st_mode), "service logs must be a regular append-only capture file")
        offset = log.seek(0, os.SEEK_END)
        start = time.time_ns() // 1_000_000
        monotonic = time.monotonic_ns()
        with https_response(base, "GET", path, headers, "CDN service read") as response:
            blob = verify_body(response, approved, git=family == "git", server=consumer == "server")
        finish = time.time_ns() // 1_000_000
        elapsed = max(1, (time.monotonic_ns() - monotonic) // 1_000_000)
        end_state = args.service_log.stat()
        count = end_state.st_size - offset
        require((file.st_dev, file.st_ino) == (end_state.st_dev, end_state.st_ino)
                and 0 <= count <= GIT_FETCH_RECORD_LIMIT, "service log rotated or exceeded the capture limit")
        capture = log.read(count)
        require(start <= finish and len(capture) == count and capture.endswith(b"\n"),
                "service logs did not retain the complete read interval")
    lines = capture.splitlines()
    responses = {}
    counters = measure(lines, fixture["scope"], expected_git_blob=blob, edge_responses=responses)
    native = measure_native(lines, fixture["scope"])
    if args.arm == "edge":
        require(all(positive(counters[key]) for key in ("observed_edge_gets", "edge_bytes", "verified_edge_payloads"))
                and counters["local_payload_hits"] == 0
                and counters["edge_attempts"] == counters["all_edge_attempts"] == len(responses),
                "service read requires verified edge delivery and a fingerprint for every viewer attempt")
    else:
        require(all(counters[key] == 0 for key in PRODUCT_COUNTERS) and native["get_responses"] > 0
                and native["body_bytes"] > 0, "service rollback requires native body reads and zero edge attempts")
    return counters | {
        "schema": "crab.cdn-service-read/v1", "scope": fixture["scope"], "arm": args.arm,
        "configuration_sha256": args.configuration_sha256, "workload_sha256": args.workload_sha256,
        "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
        "platform": runner_platform(), "publication_sha256": approved["publication_sha256"], "commit": approved["commit"],
        "payload_sha256": approved["payload_sha256"], "payload_bytes": approved["payload_bytes"],
        "etag_sha256": approved["etag_sha256"], "git_blob_oid_sha256": blob,
        "lfs_key_sha256": lfs_key,
        "started_at_unix_ms": start, "finished_at_unix_ms": finish, "elapsed_ms": elapsed,
        "native_traffic": native, "edge_responses": responses,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin"))
    for name in ("fixture", "workload", "credentials", "service-log"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    for name in ("configuration-sha256", "workload-sha256", "source-sha", "run-id", "run-attempt"):
        parser.add_argument(f"--{name}", required=True)
    try:
        print(json_record(probe(parser.parse_args())), end="")
    except (ValueError, OSError, http.client.HTTPException):
        print("error: CDN service read evidence could not be verified", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
