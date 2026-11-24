#!/usr/bin/env python3
"""Record a viewer key transition on an existing private CDN fixture."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import sys
import time
from pathlib import Path
from typing import Any

from cdn_deployment_policy_cli import PolicyDeploymentPending, attest_fixture, observe_deployment_policy
from cdn_viewer_issuer import verify_viewer_scope
from run_cdn_transport_checks import (
    FAMILY_ROOTS, approved_fixture, fixture_base, fixture_object_prefix, fixture_repository_prefix, request_sample, viewer_headers,
)

from cdn_qualification import (
    COMMIT, KEY_ROTATION_CHECK, RUN_NUMBER, SHA256, VIEWER_KEY_ID, CdnEvidenceError,
    decode, deployment_component_digest, matches, require, scope_parts, verify_key_rotation,
)


def viewer_key_id(edge: str, viewer: dict[str, str]) -> str:
    try:
        if edge == "cloudfront":
            cookies = [part.strip() for part in viewer["Cookie"].split(";")]
            key_ids = [part.removeprefix("CloudFront-Key-Pair-Id=") for part in cookies
                       if part.startswith("CloudFront-Key-Pair-Id=")]
        elif edge == "cloud-cdn":
            key_ids = re.findall(r":KeyName=([A-Za-z0-9_-]+):Signature=", viewer["Cookie"])
        else:
            header = viewer["Authorization"].removeprefix("Bearer ").split(".", 1)[0]
            payload = base64.b64decode(header + "=" * (-len(header) % 4), altchars=b"-_", validate=True)
            key_ids = [decode(payload)["kid"]]
        require(len(key_ids) == 1 and matches(VIEWER_KEY_ID, key_ids[0]), "rotation needs an unambiguous viewer key ID")
    except (ValueError, KeyError, TypeError, RecursionError):
        raise CdnEvidenceError("rotation needs an unambiguous viewer key ID") from None
    return key_ids[0]


def now_us() -> int:
    return time.time_ns() // 1000


def wait_for_retirement(fixture: dict, edge: str, overlap_digest: str, expires_at: int) -> dict[str, str]:
    print("Overlap verified. Retire the old viewer key while the collector checks the deployed policy.",
          file=sys.stderr, flush=True)
    final_digest = fixture["deployment_policy_sha256"]
    while time.time() < expires_at:
        try:
            policy = observe_deployment_policy(fixture, edge, "private")
        except PolicyDeploymentPending:
            pass
        else:
            digest = policy["deployment_policy_sha256"]
            if digest == final_digest and time.time() < expires_at:
                return policy["components"]
            require(digest in (overlap_digest, final_digest), "rotation observed a policy outside the approved transition")
        time.sleep(max(0, min(5, expires_at - time.time())))
    raise CdnEvidenceError("viewer grant expired before the approved retired policy was observed")


def collect_rotation(
    fixture: dict, provenance: dict[str, str], overlap_digest: str, retired: dict[str, str], replacement: dict[str, str],
) -> dict:
    edge, access, _, family, _ = scope_parts(fixture["scope"])
    require(access == "private", "viewer key rotation requires a private CDN fixture")
    base = fixture_base(fixture["base_url"])
    repo = fixture_repository_prefix(fixture, access, family)
    prefix = f"{repo}/{fixture_object_prefix(fixture, FAMILY_ROOTS[family])}/"
    key = f"{repo}/{fixture['object_path']}"
    viewers = {"retired": retired, "replacement": replacement}
    grants = {role: {
        "key_id": viewer_key_id(edge, viewer),
        "expires_at_unix": verify_viewer_scope(edge, viewer, fixture["native_origin"], base.geturl().rstrip("/"), repo, prefix),
    } for role, viewer in viewers.items()}
    old_id, new_id = (grants[role]["key_id"] for role in viewers)
    require(old_id != new_id, "rotation requires two distinct viewer keys")
    expires_at = min(grant["expires_at_unix"] for grant in grants.values())
    policies = {}
    requests = []

    def snapshot(phase: str, components: dict[str, str], keys: list[str]) -> None:
        require(components["viewer-key-ids"] == deployment_component_digest(sorted(keys)),
                "rotation policy does not have the expected viewer keys")
        policies[phase] = {"observed_at_unix_us": now_us(), "components": components}

    def probe(phase: str, signer: str, *, denied: bool = False, ranged: bool = False) -> None:
        started = now_us()
        require(started < grants[signer]["expires_at_unix"] * 1_000_000, "viewer grant expired during rotation")
        row, body = request_sample(base, edge, key, viewers[signer], size=fixture["size"], check=KEY_ROTATION_CHECK,
                                   wanted=None if denied else 200, range_offset=0 if ranged else None)
        finished = now_us()
        if not denied:
            require(hashlib.sha256(body).hexdigest() == fixture["sha256"],
                    "rotation probe returned different bytes from the approved fixture")
        requests.append(row | {"phase": phase, "signer": signer, "read": "range" if ranged else "full",
                               "started_at_unix_us": started, "finished_at_unix_us": finished})

    overlap = attest_fixture(fixture | {"deployment_policy_sha256": overlap_digest}, edge, access)
    snapshot("overlap", overlap["components"], [old_id, new_id])
    probe("overlap", "retired")
    probe("overlap", "replacement")
    final_digest = fixture["deployment_policy_sha256"]
    retired_components = wait_for_retirement(fixture, edge, overlap_digest, expires_at)
    snapshot("retired", retired_components, [new_id])
    probe("retired", "replacement")
    probe("retired", "retired", denied=True)
    probe("retired", "retired", denied=True, ranged=True)
    probe("retired", "replacement")
    confirmed = attest_fixture(fixture, edge, access)
    snapshot("confirmed", confirmed["components"], [new_id])
    trace = provenance | {"schema": "crab.cdn-key-rotation/v1", "grants": grants,
                          "policies": policies, "requests": requests}
    verify_key_rotation(trace, provenance, edge, final_digest, overlap_digest)
    return trace


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--overlap-policy-sha256", required=True)
    parser.add_argument("--retired-viewer-file", required=True, type=Path)
    parser.add_argument("--replacement-viewer-file", required=True, type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt) and matches(SHA256, args.overlap_policy_sha256),
                "rotation requires an approved overlap policy and exact workflow provenance")
        require(not args.output.exists(), "rotation evidence output must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, access, _, _, _ = scope_parts(fixture["scope"])
        require(access == "private", "viewer key rotation requires a private CDN fixture")
        provenance = {"scope": fixture["scope"], "source_sha": args.source_sha,
                      "configuration_sha256": args.configuration_sha256,
                      "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
        trace = collect_rotation(fixture, provenance, args.overlap_policy_sha256,
                                 viewer_headers(edge, access, args.retired_viewer_file),
                                 viewer_headers(edge, access, args.replacement_viewer_file))
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(trace, sort_keys=True) + "\n")
    except CdnEvidenceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError):
        print("error: could not collect viewer key rotation evidence", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
