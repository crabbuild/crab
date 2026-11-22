#!/usr/bin/env python3
"""Collect a private CDN view denial with an exact native sibling control."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

from cdn_deployment_policy_cli import attest_fixture
from cdn_qualification import (
    COMMIT, RUN_NUMBER, SHA256, SCOPED_VIEW_CHECK, decode, deployment_component_digest, fields, matches, require,
    scope_parts, verify_scoped_view,
)
from cdn_viewer_issuer import verify_viewer_scope
from run_cdn_transport_checks import (
    FAMILY_ROOTS, VIEW_SUFFIX, approved_fixture, authenticated_origin_read, fixture_base, fixture_object_prefix,
    native_base, request_sample, viewer_headers, warm_full_read,
)


def digest(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()


def scoped_view_placement(fixture: dict, fixture_digest: str, sibling: dict, family: str) -> tuple[dict, dict, str]:
    fields(sibling, {"view_suffix", "object_path", "size", "sha256"}, "scoped sibling fixture")
    source = fixture["fixture_prefix"]
    views, keys, target_grant = {}, {}, ""
    require(fixture["view_suffix"] != sibling["view_suffix"], "scoped control must use a distinct materialized sibling")
    for role, entry in (("target", fixture), ("sibling", sibling)):
        suffix = entry["view_suffix"]
        require(matches(VIEW_SUFFIX, suffix), "scoped isolation requires exact materialized views of one repository")
        physical = source + suffix
        grant = f"{physical}/{fixture_object_prefix(entry, FAMILY_ROOTS[family])}/"
        key = f"{physical}/{entry['object_path']}"
        keys[role] = key
        views[role] = {
            "scope_sha256": digest(suffix.split("/")[3]), "generation_sha256": digest(suffix),
            "physical_prefix_sha256": digest(f"{physical}/"), "grant_prefix_sha256": digest(grant),
            "key_sha256": digest(key), "body_sha256": entry["sha256"], "bytes": entry["size"],
        }
        if role == "target":
            target_grant = grant
    return {
        "fixture_sha256": fixture_digest, "origin_sha256": digest(fixture["native_origin"]),
        "source_prefix_sha256": digest(f"{source}/"), "deployment_policy_sha256": fixture["deployment_policy_sha256"],
        "views": views,
    }, keys, target_grant


def collect_scoped_view(
    fixture: dict, placement: dict, keys: dict, grant: str, viewer_file: Path, sibling_url_file: Path,
) -> list[dict]:
    edge, access, _, _, _ = scope_parts(fixture["scope"])
    base = fixture_base(fixture["base_url"])
    viewer = viewer_headers(edge, access, viewer_file)
    verify_viewer_scope(edge, viewer, fixture["native_origin"], fixture["base_url"].rstrip("/"),
                        fixture["fixture_prefix"] + fixture["view_suffix"], grant)
    attest_fixture(fixture, edge, access)
    native = native_base(fixture["native_origin"], edge)
    target, sibling = (placement["views"][role] for role in ("target", "sibling"))
    _, body = request_sample(base, edge, keys["target"], viewer, size=target["bytes"], check=SCOPED_VIEW_CHECK, wanted=200)
    require(hashlib.sha256(body).hexdigest() == target["body_sha256"], "scoped viewer returned different target bytes")

    def target_read(probe: str) -> dict:
        row = warm_full_read(base, edge, keys["target"], viewer, body)
        return row | {"probe": probe, "range_offset": None, "body_sha256": target["body_sha256"]}

    def sibling_read(probe: str) -> dict:
        row, content = authenticated_origin_read(fixture, native, edge, keys["sibling"], sibling["bytes"], sibling_url_file)
        observed = hashlib.sha256(content).hexdigest()
        require(observed == sibling["body_sha256"], "native control returned different sibling bytes")
        return row | {"probe": probe, "range_offset": None, "body_sha256": observed}

    requests = [target_read("target_before"), sibling_read("sibling_before")]
    for probe, offset in (("sibling_full", None), ("sibling_range", 0)):
        row, _ = request_sample(base, edge, keys["sibling"], viewer, size=sibling["bytes"], check=SCOPED_VIEW_CHECK,
                                range_offset=offset)
        requests.append(row | {"probe": probe, "range_offset": offset, "body_sha256": None})
    requests.extend((sibling_read("sibling_after"), target_read("target_after")))
    attest_fixture(fixture, edge, access)
    return requests


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True, help="approved protected-view fixture digest")
    parser.add_argument("--sibling", required=True, type=Path, help="sibling view suffix and immutable object descriptor")
    parser.add_argument("--scoped-view-sha256", help="the preregistered digest, from inspection without --output")
    parser.add_argument("--row-configuration-sha256", help="release row configuration digest when using a companion fixture")
    parser.add_argument("--viewer-file", type=Path)
    parser.add_argument("--sibling-read-url-file", type=Path)
    parser.add_argument("--source-sha")
    parser.add_argument("--run-id")
    parser.add_argument("--run-attempt")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        _, access, _, family, _ = scope_parts(fixture["scope"])
        require(access == "private", "scoped view isolation requires a private fixture")
        with args.sibling.open("rb") as path:
            raw = path.read(4097)
        require(len(raw) <= 4096, "scoped sibling fixture is too large")
        placement, keys, grant = scoped_view_placement(fixture, args.configuration_sha256, decode(raw), family)
        approval = deployment_component_digest(placement)
        if args.output is None:
            print(json.dumps({"scoped_view_sha256": approval}, sort_keys=True))
            return 0
        require(args.scoped_view_sha256 == approval, "scoped view fixture differs from the preregistered approval")
        configuration = args.row_configuration_sha256 or args.configuration_sha256
        require(matches(SHA256, configuration) and matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "scoped view trace requires exact workflow provenance")
        require(args.viewer_file is not None and args.sibling_read_url_file is not None,
                "scoped view trace requires a viewer grant and native sibling control grant")
        require(not args.output.exists() and not args.output.is_symlink() and args.output.parent.is_dir(),
                "scoped view output must be a new trace file")
        provenance = {"scope": fixture["scope"], "configuration_sha256": configuration, "source_sha": args.source_sha,
                      "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
        requests = collect_scoped_view(fixture, placement, keys, grant, args.viewer_file, args.sibling_read_url_file)
        trace = provenance | {"schema": "crab.cdn-scoped-view/v1", "check": SCOPED_VIEW_CHECK,
                              "placement": placement, "requests": requests}
        verify_scoped_view(trace, provenance, approval)
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(trace, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
