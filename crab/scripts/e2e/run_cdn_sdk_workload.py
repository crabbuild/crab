#!/usr/bin/env python3
"""Verify an ordinary SDK Git snapshot read of a published CDN fixture."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import time
from pathlib import Path

from collect_cdn_product_metrics import measure, measure_native
from cdn_viewer_issuer import issuer_settings
from run_cdn_git_workload import approved_public_export, runner_platform, viewer_binding_sha256
from run_cdn_transport_checks import approved_fixture

from cdn_qualification import (
    COMMIT, GIT_PAYLOAD_SIZE, PRODUCT_COUNTERS, RUN_NUMBER, SDK_PAYLOAD_FIELDS, SDK_PROBE_FIELDS, SDK_TIMING_FIELDS,
    SHA256, decode, fields, is_direct_sdk_git_scope, matches, positive, read_git_record, require, scope_parts,
    validate_git_publication, verify_sdk_candidate,
)
from public_inventory import ExportError


PRIVATE_READER_FIELDS = {"oidc_issuer", "oidc_client_id", "token_cache_directory"}
READ_TARGET = "crab_sdk::cdn::read_target"
ARM_BOUNDARY = "crab_cdn_probe::arm_boundary"


def verified_target(lines: list[bytes], fixture: dict, arm: str, approval: str) -> dict:
    expected = {
        "origin_sha256": hashlib.sha256(fixture["native_origin"].encode()).hexdigest(),
        "prefix_sha256": hashlib.sha256(f"{fixture['fixture_prefix']}/".encode()).hexdigest(),
        "binding_sha256": viewer_binding_sha256(fixture) if arm == "edge" else "",
        "public_export_sha256": approval if scope_parts(fixture["scope"])[1] == "public" else "",
    }
    observations = []
    for line in lines:
        if READ_TARGET.encode() not in line:
            continue
        event = decode(line)
        if isinstance(event, dict) and event.get("target") == READ_TARGET:
            observations.append(event.get("fields"))
    require(observations == [expected | {"message": "SDK captured CDN read target"}],
            "SDK snapshot did not confirm the approved native target, prefix and active CDN binding")
    return expected


def binding_for(args: argparse.Namespace, fixture: dict) -> tuple[dict, str]:
    access = scope_parts(fixture["scope"])[1]
    if access == "private":
        require(args.private_reader is not None and args.export_approval is None,
                "private SDK reads require the trusted issuer configuration")
        issuer = fields(decode(read_git_record(args.private_reader)), PRIVATE_READER_FIELDS, "SDK issuer configuration")
        require(all(isinstance(value, str) and bool(value) for value in issuer.values())
                and Path(issuer["token_cache_directory"]).is_absolute(),
                "SDK issuer values must include an absolute token-cache directory")
        endpoint, _, approved_issuer = issuer_settings(fixture)
        approval = approved_issuer if args.arm == "edge" else ""
        return {"access": access, **issuer, "endpoint": endpoint.geturl(), "approval_sha256": approval or None}, approval
    require(args.private_reader is None and ((args.export_approval is not None) == (args.arm == "edge")),
            "public SDK edge reads require the approved export directory")
    publication, approval = None, ""
    if args.export_approval is not None:
        approval, _, _ = approved_public_export(args.export_approval, fixture)
        publication = {"path": str((args.export_approval / "publication.json").resolve()), "sha256": approval}
    return {"access": access, "export_origin": fixture["export_origin"], "publication": publication}, approval


def split_observations(lines: list[bytes], repeat: bool) -> tuple[list[bytes], list[bytes]]:
    markers = []
    for index, line in enumerate(lines):
        if ARM_BOUNDARY.encode() in line:
            event = decode(line)
            if isinstance(event, dict) and event.get("target") == ARM_BOUNDARY:
                marker = fields(event.get("fields"), {"phase"}, "SDK measurement boundary")
                markers.append((index, marker["phase"]))
    phases = ["primary_finish"]
    if repeat:
        phases.extend(("local_start", "local_finish"))
    require([phase for _, phase in markers] == phases, "SDK probe did not isolate each timed read")
    primary_end, _ = markers[0]
    if not repeat:
        return lines[:primary_end], []
    _, (start, _), (finish, _) = markers
    return lines[:primary_end], lines[start + 1:finish]


def observed_interval(value: dict, earliest: int, latest: int) -> dict:
    timing = fields(value, SDK_TIMING_FIELDS, "SDK timing")
    require(all(positive(point) for point in timing.values())
            and earliest <= timing["started_at_unix_ms"] <= timing["finished_at_unix_ms"] <= latest,
            "SDK probe returned an invalid or out-of-order measurement interval")
    return timing


def json_record(value: dict) -> str:
    return json.dumps(value, sort_keys=True) + "\n"


def fetch(args: argparse.Namespace) -> tuple[dict, dict | None]:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    scope = fixture["scope"]
    require(is_direct_sdk_git_scope(scope) and fixture["view_suffix"] == "",
            "SDK snapshot qualification uses a direct Git fixture on the repository prefix")
    provenance = {"scope": scope, "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
                  "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
    raw = read_git_record(args.receipt)
    publication = validate_git_publication(raw, provenance)
    require(publication["published"], "SDK read requires a confirmed native publication")
    repeat = args.warm_local_output is not None
    require(not repeat or args.arm == "edge", "SDK warm-local reads must reuse an edge primer")
    binding, approval = binding_for(args, fixture)
    request = {"origin": fixture["native_origin"], "repository_prefix": fixture["fixture_prefix"],
               "base_url": fixture["base_url"], "branch": publication["ref"].removeprefix("refs/heads/"),
               "expected_oid": publication["oid"], "expected_sha256": publication["sha256"],
               "expected_bytes": GIT_PAYLOAD_SIZE, "binding": binding, "repeat_snapshot": repeat}
    probe = args.probe.resolve(strict=True)
    with probe.open("rb") as binary:
        probe_digest = hashlib.file_digest(binary, "sha256").hexdigest()
    started_at = time.time_ns() // 1_000_000
    require(publication["visible_at_unix_ms"] <= started_at, "SDK measurement started before native publication")
    try:
        result = subprocess.run([str(probe)], input=json.dumps(request).encode(), capture_output=True, timeout=300, check=False)
    except subprocess.TimeoutExpired:
        raise ValueError("SDK snapshot read timed out") from None
    finished_at = time.time_ns() // 1_000_000
    require(result.returncode == 0, f"SDK snapshot read failed with exit {result.returncode}")
    observed = fields(decode(result.stdout), SDK_PROBE_FIELDS, "SDK snapshot result")
    platform = runner_platform()
    build = observed.pop("build")
    require(isinstance(build, dict) and "binary_sha256" not in build, "invalid SDK snapshot build")
    candidate = verify_sdk_candidate(build | {"binary_sha256": probe_digest}, args.source_sha, platform)
    payload = fields(observed["payload"], SDK_PAYLOAD_FIELDS, "SDK snapshot payload")
    require(observed["commit"] == publication["oid"] and payload["sha256"] == publication["sha256"]
            and type(payload["bytes"]) is int and payload["bytes"] == GIT_PAYLOAD_SIZE
            and matches(SHA256, payload["git_blob_oid_sha256"]),
            "SDK snapshot did not verify the published Git payload")
    timing = observed_interval({key: observed[key] for key in SDK_TIMING_FIELDS}, started_at, finished_at)
    lines, local_lines = split_observations(result.stderr.splitlines(), repeat)
    reader = verified_target(lines, fixture, args.arm, approval)
    responses = {}
    counters = measure(lines, scope, edge_responses=responses, expected_git_blob=payload["git_blob_oid_sha256"])
    native = measure_native(lines, scope)
    if args.arm == "edge":
        require(counters["observed_edge_gets"] > 0 and counters["verified_edge_payloads"] > 0,
                "SDK snapshot did not accept verified Git bytes through the approved CDN")
    else:
        require(counters["all_edge_attempts"] == 0 and native["get_responses"] > 0 and native["body_bytes"] > 0,
                "SDK origin control requires native payload reads without edge activity")
    context = provenance | payload | {
        "schema": "crab.cdn-sdk-git-read/v1", "platform": platform, "candidate": candidate, "approval_sha256": approval,
        "publication_sha256": hashlib.sha256(raw).hexdigest(), "reader": reader, "commit": observed["commit"],
    }
    primary = context | timing | counters | {"arm": args.arm, "edge_responses": responses, "native_traffic": native}
    local = None
    if repeat:
        local_timing = observed_interval(observed["warm_local"], primary["finished_at_unix_ms"], finished_at)
        local_counts = measure(local_lines, scope, expected_git_blob=payload["git_blob_oid_sha256"])
        require(positive(local_counts["local_payload_hits"])
                and all(local_counts[key] == 0 for key in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "SDK repeat read requires the verified in-memory Git blob without edge traffic")
        local = context | local_timing | local_counts | {
            "arm": "warm_local", "edge_responses": {}, "native_traffic": measure_native(local_lines, scope),
            "primer_sha256": hashlib.sha256(json_record(primary).encode()).hexdigest(),
        }
    else:
        require(observed["warm_local"] is None, "unexpected SDK repeat output")
    return primary, local


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin"))
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--probe", required=True, type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--private-reader", type=Path)
    parser.add_argument("--export-approval", type=Path)
    parser.add_argument("--warm-local-output", type=Path, help="write the timed repeat read of the same pinned SDK snapshot")
    args = parser.parse_args()
    try:
        primary, local = fetch(args)
        if local is not None:
            with args.warm_local_output.open("x", encoding="utf-8", newline="\n") as output:
                output.write(json_record(local))
        sys.stdout.buffer.write(json_record(primary).encode())
    except (ExportError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
