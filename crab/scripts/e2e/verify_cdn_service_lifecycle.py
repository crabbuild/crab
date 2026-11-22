#!/usr/bin/env python3
"""Verify service reads, rollback and cleanup against independent native observations."""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

from run_cdn_git_workload import read_target_fingerprints, runner_platform
from run_cdn_service_read import service_publication, workload
from run_cdn_transport_checks import approved_fixture, fixture_object_key
from run_cdn_sdk_workload import json_record

from cdn_qualification import (
    COMMIT, GIT_FETCH_RECORD_LIMIT, RUN_NUMBER, decode, is_direct_service_scope, matches,
    read_git_record, read_trace, require, scope_parts, validate_service_read,
    verify_cleanup_trace, verify_git_lifecycle, verify_git_retirement, verify_viewer_requests,
)


def verify(args: argparse.Namespace) -> dict:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    scope = fixture["scope"]
    provider, access, consumer, family, _ = scope_parts(scope)
    require(is_direct_service_scope(scope) and fixture["view_suffix"] == "",
            "service lifecycle requires an approved direct Git or LFS repository fixture")
    row = {"scope": scope, "source_sha": args.source_sha, "configuration_sha256": args.configuration_sha256,
           "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
           "platform": runner_platform(), "fixture_prefix": fixture["fixture_prefix"]}
    approved = workload(args, consumer)
    publication_raw, publication, publication_sha = service_publication(args.publication, row, approved)
    native = {phase: read_git_record(getattr(args, f"native_{phase}")) for phase in ("published", "retired")}
    published_sha = hashlib.sha256(native["published"]).hexdigest()
    cleanup_raw = read_trace(args.cleanup, "cleanup")
    cleanup = decode(cleanup_raw)
    cleanup_at = verify_cleanup_trace(cleanup, row, provider, access)
    origins = {"native": fixture["native_origin"]}
    if access == "public":
        origins["export"] = fixture["export_origin"]
    require(all(cleanup["targets"][role]["origin_sha256"] == hashlib.sha256(address.encode()).hexdigest()
                for role, address in origins.items()), "service cleanup covered different storage targets")
    raw = {arm: read_git_record(getattr(args, arm), limit=GIT_FETCH_RECORD_LIMIT) for arm in ("edge", "origin")}
    reads = {arm: validate_service_read(decode(data), row, arm) for arm, data in raw.items()}
    edge, origin = reads["edge"], reads["origin"]
    for arm, read in reads.items():
        require(all(read[key] == approved[key] for key in
                    ("commit", "publication_sha256", "payload_sha256", "payload_bytes", "etag_sha256"))
                and read["workload_sha256"] == args.workload_sha256 and read["native_published_sha256"] == published_sha,
                "edge and rollback reads must use the same approved workload and native publication")
        export = approved["public_export_sha256"] if arm == "edge" else ""
        require(read["reader"] == read_target_fingerprints(fixture, edge=arm == "edge", export_sha256=export),
                "service read used a different native target or viewer binding")
        if family == "lfs":
            key = fixture_object_key(fixture, access, family)
            require(read["lfs_key_sha256"] == hashlib.sha256(key.encode()).hexdigest()
                    and read["payload_sha256"] == fixture["sha256"] and read["payload_bytes"] == fixture["size"],
                    "service lifecycle requires the approved LFS object")
    require(edge["git_blob_oid_sha256"] == origin["git_blob_oid_sha256"]
            and edge["finished_at_unix_ms"] <= origin["started_at_unix_ms"],
            "service rollback must follow the edge read and verify the same Git blob")
    viewer_raw = read_trace(args.viewer_requests, "viewer requests")
    edge_sha = hashlib.sha256(raw["edge"]).hexdigest()
    verify_viewer_requests(decode(viewer_raw), edge, edge_sha, provider)
    retirement = read_git_record(args.retirement)
    retired_at = verify_git_retirement(retirement, row, publication_sha, publication["candidate"], origin["finished_at_unix_ms"])
    verify_git_lifecycle(native, row, publication, publication_sha, edge["started_at_unix_ms"], edge["reader"],
                         retired_at, cleanup_at, cleanup["targets"]["native"]["origin_sha256"])
    retained = native | raw | {"publication": publication_raw, "retirement": retirement,
                               "viewer_requests": viewer_raw, "cleanup": cleanup_raw}
    return {key: value for key, value in row.items() if key != "fixture_prefix"} | {
        "schema": "crab.cdn-service-lifecycle/v1", "workload_sha256": args.workload_sha256,
        "records": {name: hashlib.sha256(data).hexdigest() for name, data in retained.items()},
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("fixture", "workload", "publication", "retirement", "native-published", "native-retired",
                 "edge", "origin", "viewer-requests", "cleanup"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    for name in ("configuration-sha256", "workload-sha256", "source-sha", "run-id", "run-attempt"):
        parser.add_argument(f"--{name}", required=True)
    try:
        print(json_record(verify(parser.parse_args())), end="")
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
