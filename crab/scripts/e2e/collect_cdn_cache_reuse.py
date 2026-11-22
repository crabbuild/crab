#!/usr/bin/env python3
"""Verify CDN cache reuse across two independently authenticated Crab Auth readers."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import sys
import time
from pathlib import Path

from cdn_deployment_policy_cli import attest_fixture
from cdn_qualification import (
    CACHE_REUSE_CHECK, CACHE_REUSE_ROLES, COMMIT, RUN_NUMBER, decode, deployment_component_digest, fields, matches,
    require, scope_parts, verify_cache_reuse,
)
from cdn_viewer_issuer import BEARER, JWT, issue_viewer, issuer_settings
from run_cdn_transport_checks import approved_fixture, fixture_base, fixture_object_key, request_sample, viewer_expiry


IDENTITY_CLAIM = re.compile(r"[^\x00-\x1f\x7f]{1,1024}")


def principal_fingerprints(issuer: str, subject: str, identity: str) -> dict[str, str]:
    require(all(matches(IDENTITY_CLAIM, item) for item in (issuer, subject, identity)) and issuer.startswith("https://"),
            "cache reuse needs a bounded HTTPS OIDC issuer, subject and Crab Auth policy identity")
    return {
        "subject_sha256": deployment_component_digest({"iss": issuer, "sub": subject}),
        "policy_identity_sha256": hashlib.sha256(identity.lower().encode()).hexdigest(),
    }


def read_token(path: Path, audience: str) -> tuple[str, dict[str, str]]:
    with path.open("rb") as token_file:
        token = token_file.read(20003).removesuffix(b"\n").removesuffix(b"\r").decode("latin-1")
    require(matches(BEARER, token) and matches(JWT, token), "expected a bounded OIDC ID token file")
    encoded = token.split(".")[1]
    try:
        claims = decode(base64.b64decode(encoded + "=" * (-len(encoded) % 4), altchars=b"-_", validate=True))
    except (ValueError, RecursionError):
        raise ValueError("OIDC ID token has invalid claims") from None
    require(isinstance(claims, dict), "OIDC ID token has invalid claims")
    aud = claims.get("aud")
    require(aud == audience or (isinstance(aud, list) and audience in aud), "OIDC audience differs from the approved issuer")
    issuer, subject = claims.get("iss"), claims.get("sub")
    return token, principal_fingerprints(issuer, subject, claims.get("email") or subject)


def measure(fixture: dict, key: str, authority: dict, token_files: dict[str, Path], audience: str) -> tuple[dict, list]:
    edge, access, _, family, _ = scope_parts(fixture["scope"])
    tokens = {}
    for role in CACHE_REUSE_ROLES:
        token, principal = read_token(token_files[role], audience)
        require(principal == authority["principals"][role], "OIDC token is from a different approved cache reuse principal")
        tokens[role] = token
    attest_fixture(fixture, edge, access)
    base = fixture_base(fixture["base_url"])
    grants, requests = {}, []
    for role in CACHE_REUSE_ROLES:
        # The pinned issuer validates this exact token in both the header and
        # request body before issuing a read grant for the approved fixture.
        viewer = issue_viewer(fixture, edge, family, tokens[role])
        issued = time.time_ns() // 1_000_000
        grants[role] = authority["principals"][role] | {
            "issued_at_unix_ms": issued, "expires_at_unix": viewer_expiry(edge, viewer),
        }
        started = time.time_ns() // 1_000_000
        row, body = request_sample(base, edge, key, viewer, size=fixture["size"], check=CACHE_REUSE_CHECK, wanted=200)
        finished = time.time_ns() // 1_000_000
        requests.append(row | {"principal": role, "body_sha256": hashlib.sha256(body).hexdigest(),
                               "started_at_unix_ms": started, "finished_at_unix_ms": finished})
    attest_fixture(fixture, edge, access)
    return grants, requests


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--principals", required=True, type=Path, help="approved OIDC issuers, subjects and policy identities")
    parser.add_argument("--cache-reuse-sha256", help="preregistered digest from inspection without --output")
    parser.add_argument("--primary-token-file", type=Path)
    parser.add_argument("--second-token-file", type=Path)
    parser.add_argument("--source-sha")
    parser.add_argument("--run-id")
    parser.add_argument("--run-attempt")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        _, access, _, family, _ = scope_parts(fixture["scope"])
        require(access == "private", "authenticated cache reuse requires a private fixture")
        endpoint, audience, approval = issuer_settings(fixture)
        with args.principals.open("rb") as spec:
            raw = spec.read(8193)
        require(len(raw) <= 8192, "cache reuse principal descriptor is too large")
        principals = {}
        for role, value in fields(decode(raw), set(CACHE_REUSE_ROLES), "cache reuse principals").items():
            spec = fields(value, {"issuer", "subject", "policy_identity"}, f"cache reuse principal {role}")
            principals[role] = principal_fingerprints(spec["issuer"], spec["subject"], spec["policy_identity"])
        require(all(principals["primary"][key] != principals["second"][key] for key in principals["primary"]),
                "cache reuse requires two distinct OIDC subjects and Crab Auth policy identities")
        key = fixture_object_key(fixture, access, family)
        authority = {
            "issuer_endpoint_sha256": hashlib.sha256(endpoint.geturl().encode()).hexdigest(),
            "approval_sha256": approval,
            "principals": principals,
            "object": {"key_sha256": hashlib.sha256(key.encode()).hexdigest(), "body_sha256": fixture["sha256"],
                       "bytes": fixture["size"]},
        }
        digest = deployment_component_digest(authority)
        if args.output is None:
            print(json.dumps({"cache_reuse_sha256": digest}, sort_keys=True))
            return 0
        require(digest == args.cache_reuse_sha256, "cache reuse authority differs from the preregistered approval")
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id) and matches(RUN_NUMBER, args.run_attempt),
                "cache reuse needs exact workflow provenance")
        require(args.primary_token_file is not None and args.second_token_file is not None, "supply both OIDC ID token files")
        require(not args.output.exists() and not args.output.is_symlink() and args.output.parent.is_dir(),
                "cache reuse output must be a new trace file")
        provenance = {"scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
                      "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
        token_files = {"primary": args.primary_token_file, "second": args.second_token_file}
        grants, requests = measure(fixture, key, authority, token_files, audience)
        trace = provenance | {"schema": "crab.cdn-authenticated-cache-reuse/v1", "check": CACHE_REUSE_CHECK,
                              "authority": authority, "grants": grants, "requests": requests}
        verify_cache_reuse(trace, provenance, digest)
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(trace, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
