#!/usr/bin/env python3
"""Verify service reads, rollback and cleanup against independent native observations."""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

from run_cdn_git_workload import runner_platform, viewer_binding_sha256
from run_cdn_service_read import service_publication, workload
from run_cdn_transport_checks import approved_fixture
from run_cdn_sdk_workload import json_record

from cdn_qualification import (
    COMMIT, GIT_FETCH_RECORD_LIMIT, RUN_NUMBER, decode, is_direct_service_scope, matches,
    read_git_record, read_trace, require, scope_parts,
    verify_cleanup_trace, verify_service_lifecycle,
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
    publication_raw, _, _ = service_publication(args.publication, row, approved)
    native = {phase: read_git_record(getattr(args, f"native_{phase}")) for phase in ("published", "retired")}
    cleanup_raw = read_trace(args.cleanup, "cleanup")
    cleanup = decode(cleanup_raw)
    cleanup_at = verify_cleanup_trace(cleanup, row, provider, access)
    origins = {"native": fixture["native_origin"]}
    if access == "public":
        origins["export"] = fixture["export_origin"]
    require(all(cleanup["targets"][role]["origin_sha256"] == hashlib.sha256(address.encode()).hexdigest()
                for role, address in origins.items()), "service cleanup covered different storage targets")
    if family == "lfs":
        require(approved["payload_sha256"] == fixture["sha256"] and approved["payload_bytes"] == fixture["size"],
                "service lifecycle requires the approved LFS object")
    records = native | {arm: read_git_record(getattr(args, arm), limit=GIT_FETCH_RECORD_LIMIT) for arm in ("edge", "origin")}
    records |= {"publication": publication_raw, "retirement": read_git_record(args.retirement),
                "viewer_requests": read_trace(args.viewer_requests, "viewer requests")}
    native_origin = cleanup["targets"]["native"]["origin_sha256"]
    verify_service_lifecycle(records, row, {
        "service_workload": {"sha256": args.workload_sha256, "origin_sha256": native_origin,
                             "prefix_sha256": cleanup["prefix_sha256"], "etag_sha256": approved["etag_sha256"],
                             "payload_bytes": approved["payload_bytes"]},
        "viewer_binding_sha256": viewer_binding_sha256(fixture),
        "service_candidate": decode(records["edge"]).get("candidate"),
        "publication_sha256": approved["public_export_sha256"],
    }, cleanup_at=cleanup_at, native_origin_sha256=native_origin)
    retained = records | {"cleanup": cleanup_raw}
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
