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
from run_cdn_git_workload import PAYLOAD, approved_public_export, read_target_fingerprints, runner_platform, verify_transition_events
from run_cdn_sdk_workload import json_record
from run_cdn_transport_checks import approved_fixture, fixture_base, fixture_object_key, https_response
from s3_gateway_workload import _canonical_path, _signed_headers
from public_inventory import ExportError

from cdn_qualification import (
    CLI_CANDIDATE_FIELDS, COMMIT, GIT_FETCH_RECORD_LIMIT, GIT_OID, RUN_NUMBER, SERVICE_PAYLOAD_LIMIT, SHA256,
    decode, fields, is_direct_service_scope, matches, positive, read_git_record, require, scope_parts,
    validate_git_publication, validate_service_read, verify_git_native_ref, verify_service_candidate, verify_service_repeat,
)


READ_TARGET = "crab_storage::cdn::read_target"
SERVICE_BUILD_TARGET = "crab_storage::cdn::service_build"
SERVICE_WORKLOAD_FIELDS = {
    "schema", "configuration_sha256", "publication_sha256", "endpoint", "repository", "commit",
    "payload_sha256", "payload_bytes", "etag_sha256", "public_export_sha256",
}
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,79}")
BUCKET = re.compile(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]")
REGION = re.compile(r"[a-z0-9-]{1,32}")
SESSION_COOKIE = re.compile(r"__Host-crab_session=[A-Za-z0-9._~+=/%-]{1,8192}")
BASIC = re.compile(r"Basic [A-Za-z0-9+/]{2,8192}={0,2}")
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


def workload(path: Path, digest: str, configuration: str, consumer: str) -> dict:
    raw = read_git_record(path)
    approved = fields(decode(raw), SERVICE_WORKLOAD_FIELDS, "CDN service workload")
    require(hashlib.sha256(raw).hexdigest() == digest
            and approved["schema"] == "crab.cdn-service-workload/v1"
            and approved["configuration_sha256"] == configuration
            and all(matches(SHA256, approved[field]) for field in ("publication_sha256", "payload_sha256", "etag_sha256"))
            and matches(GIT_OID, approved["commit"]) and positive(approved["payload_bytes"])
            and approved["payload_bytes"] <= SERVICE_PAYLOAD_LIMIT,
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


def verify_read_target(lines: list[bytes], reader: dict, consumer: str) -> str:
    seen = []
    for line in lines:
        if READ_TARGET.encode() not in line:
            continue
        event = decode(line)
        if isinstance(event, dict) and event.get("target") == READ_TARGET:
            seen.append(event.get("fields"))
    expected = reader | {"message": "CDN service read target", "consumer": consumer}
    require(len(seen) == 1, "service did not report exactly one approved native target")
    observed = fields(seen[0], expected.keys() | {"service_instance_sha256"}, "service read target")
    require(all(observed[key] == value for key, value in expected.items())
            and matches(SHA256, observed["service_instance_sha256"]),
            "service did not confirm the approved native target, prefix and active CDN binding")
    return observed["service_instance_sha256"]


def service_candidate(startup: list[bytes], consumer: str, instance: str, provenance: dict) -> dict:
    builds = []
    for line in startup:
        if SERVICE_BUILD_TARGET.encode() not in line:
            continue
        event = decode(line)
        if isinstance(event, dict) and event.get("target") == SERVICE_BUILD_TARGET:
            builds.append(event.get("fields"))
    require(len(builds) == 1, "service log needs exactly one startup build observation")
    build = fields(builds[0], CLI_CANDIDATE_FIELDS | {"consumer", "service_instance_sha256", "message"}, "service build")
    require(build["message"] == "CDN service build identity" and build["consumer"] == consumer
            and build["service_instance_sha256"] == instance,
            "service read came from a different process than the startup build")
    return verify_service_candidate({key: build[key] for key in CLI_CANDIDATE_FIELDS},
                                    provenance["source_sha"], provenance["platform"])


def service_publication(path: Path, provenance: dict, approved: dict) -> tuple[bytes, dict, str]:
    raw = read_git_record(path)
    digest = hashlib.sha256(raw).hexdigest()
    publication = validate_git_publication(raw, provenance)
    require(publication["published"] and digest == approved["publication_sha256"]
            and publication["oid"] == approved["commit"] and publication["sha256"] == approved["payload_sha256"],
            "service workload needs the exact published commit and payload")
    return raw, publication, digest


def probe(args: argparse.Namespace) -> dict:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    _, access, consumer, family, _ = scope_parts(fixture["scope"])
    require(is_direct_service_scope(fixture["scope"])
            and not fixture["view_suffix"], "service read evidence requires a direct Git or LFS repository fixture")
    local = args.arm == "warm_local"
    prior = args.arm == "prior_export"
    require((args.primer is not None) == local,
            "service warm-local reads require their own edge primer")
    require((not prior or args.transition_object is not None)
            and (args.transition_object is None or (access == "public" and args.arm in ("edge", "prior_export"))),
            "service export transition requires a public edge or prior export read and the exact object")
    approved = workload(args.workload, args.workload_sha256, args.configuration_sha256, consumer)
    provenance = {"scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
                  "source_sha": args.source_sha, "workflow_run_id": args.run_id,
                  "workflow_run_attempt": args.run_attempt, "platform": runner_platform()}
    _, publication, publication_sha256 = service_publication(args.publication, provenance, approved)
    primer_raw = read_git_record(args.primer, limit=GIT_FETCH_RECORD_LIMIT) if local else None
    primer = validate_service_read(decode(primer_raw), provenance, "edge") if primer_raw else None
    require(not local or primer is not None, "service warm-local read needs the completed edge primer")
    public_edge = access == "public" and args.arm != "origin"
    require((args.export_approval is not None) == public_edge,
            "only public service edge reads require the approved export directory")
    export_sha256 = ""
    transition = None
    if access == "public":
        require(matches(SHA256, approved["public_export_sha256"]), "public workload needs its registered export receipt")
    else:
        require(approved["public_export_sha256"] == "", "private workload cannot name a public export receipt")
    if public_edge:
        export_sha256, objects, sealed_at = approved_public_export(args.export_approval, fixture)
        require(approved["public_export_sha256"] == export_sha256,
                "service workload must pin the exact public export receipt")
        if args.transition_object is not None:
            path = args.transition_object
            require(family != "lfs" or path == fixture["object_path"],
                    "service LFS transition must use the approved payload key")
            key = fixture_object_key(fixture | {"object_path": path}, access, family)
            require(any(obj.path == path for obj in objects) != prior,
                    "service export inventory membership does not match the transition phase")
            require(not prior or sealed_at <= publication["visible_at_unix_ms"],
                    "prior service export must be sealed before native publication")
            transition = {"key_sha256": hashlib.sha256(key.encode()).hexdigest(),
                          "receipt_sha256": export_sha256, "phase": "before" if prior else "after"}
    reader = read_target_fingerprints(fixture, edge=args.arm != "origin", export_sha256=export_sha256)
    raw_observation = read_git_record(args.native_published)
    observed = verify_git_native_ref(decode(raw_observation), provenance | {"fixture_prefix": fixture["fixture_prefix"]},
                                     publication, publication_sha256, "published", reader["origin_sha256"])
    require(publication["visible_at_unix_ms"] <= observed["started_at_unix_ms"],
            "native ref observation started before publication")
    lfs_key = None
    if family == "lfs":
        require(fixture["sha256"] == approved["payload_sha256"] and fixture["size"] == approved["payload_bytes"],
                "service LFS workload differs from the approved object")
        lfs_key = hashlib.sha256(fixture_object_key(fixture, access, family).encode()).hexdigest()
    base, path, headers = request_target(approved, consumer, family, args.credentials)
    with os.fdopen(os.open(args.service_log, os.O_RDONLY | os.O_NONBLOCK), "rb") as log:
        file = os.fstat(log.fileno())
        require(stat.S_ISREG(file.st_mode), "service logs must be a regular append-only capture file")
        startup = log.read(GIT_FETCH_RECORD_LIMIT).split(b"\n")[:-1]
        offset = log.seek(0, os.SEEK_END)
        start = time.time_ns() // 1_000_000
        require(observed["observed_at_unix_ms"] <= start, "service read started before the independent publication observation")
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
    instance = verify_read_target(lines, reader, consumer)
    candidate = service_candidate(startup, consumer, instance, provenance)
    responses = {}
    counters = measure(lines, fixture["scope"], expected_git_blob=blob, expected_lfs_key=lfs_key,
                       edge_responses=responses)
    native = measure_native(lines, fixture["scope"])
    if transition is not None:
        verify_transition_events(lines, fixture["scope"], transition, git_blob=blob)
    record = provenance | counters | {
        "schema": "crab.cdn-service-read/v1", "arm": args.arm,
        "workload_sha256": args.workload_sha256, "publication_sha256": publication_sha256,
        "native_published_sha256": hashlib.sha256(raw_observation).hexdigest(), "commit": approved["commit"],
        "payload_sha256": approved["payload_sha256"], "payload_bytes": approved["payload_bytes"],
        "etag_sha256": approved["etag_sha256"], "git_blob_oid_sha256": blob,
        "lfs_key_sha256": lfs_key, "reader": reader,
        "service_instance_sha256": instance, "candidate": candidate,
        "started_at_unix_ms": start, "finished_at_unix_ms": finish, "elapsed_ms": elapsed,
        "native_traffic": native, "edge_responses": responses,
    }
    if primer is not None:
        record["primer_sha256"] = hashlib.sha256(primer_raw).hexdigest()
        verify_service_repeat(record, primer, record["primer_sha256"])
    if transition is not None:
        record["transition_key_sha256"] = transition["key_sha256"]
    return validate_service_read(record, provenance, args.arm)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin", "warm_local", "prior_export"))
    for name in ("fixture", "workload", "credentials", "service-log", "publication", "native-published"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--export-approval", type=Path)
    parser.add_argument("--primer", type=Path, help="complete edge read record from the same service process")
    parser.add_argument("--transition-object", help="exact repository-relative key newly approved for public export")
    for name in ("configuration-sha256", "workload-sha256", "source-sha", "run-id", "run-attempt"):
        parser.add_argument(f"--{name}", required=True)
    try:
        print(json_record(probe(parser.parse_args())), end="")
    except (ValueError, OSError, http.client.HTTPException, ExportError):
        print("error: CDN service read evidence could not be verified", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
