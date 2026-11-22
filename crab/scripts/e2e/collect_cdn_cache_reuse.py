#!/usr/bin/env python3
"""Verify CDN cache reuse across two independently authenticated Crab Auth readers."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import sys
import time
from pathlib import Path

from cdn_deployment_policy_cli import attest_fixture
from cdn_qualification import (
    CACHE_REUSE_CHECK, CACHE_REUSE_ROLES, CDN_PROVENANCE_FIELDS, COMMIT, RUN_NUMBER,
    decode, deployment_component_digest, fields, matches, read_trace, require, scope_parts,
    verify_cache_reuse, verify_cache_reuse_read,
)
from cdn_viewer_issuer import BEARER, JWT, github_id_token, issue_viewer, issuer_settings
from run_cdn_transport_checks import approved_fixture, fixture_base, fixture_object_key, request_sample, viewer_expiry


IDENTITY_CLAIM = re.compile(r"[^\x00-\x1f\x7f]{1,1024}")
PRIMARY_OBSERVATION_SCHEMA = "crab.cdn-cache-reuse-primary/v1"


def principal_fingerprints(issuer: str, subject: str, identity: str) -> dict[str, str]:
    require(all(matches(IDENTITY_CLAIM, item) for item in (issuer, subject, identity)) and issuer.startswith("https://"),
            "cache reuse needs a bounded HTTPS OIDC issuer, subject and Crab Auth policy identity")
    return {
        "subject_sha256": deployment_component_digest({"iss": issuer, "sub": subject}),
        "policy_identity_sha256": hashlib.sha256(identity.lower().encode()).hexdigest(),
    }


def token_principal(token: str, audience: str) -> dict[str, str]:
    require(matches(BEARER, token) and matches(JWT, token), "expected a bounded OIDC ID token")
    encoded = token.split(".")[1]
    try:
        claims = decode(base64.b64decode(encoded + "=" * (-len(encoded) % 4), altchars=b"-_", validate=True))
    except (ValueError, RecursionError):
        raise ValueError("OIDC ID token has invalid claims") from None
    require(isinstance(claims, dict), "OIDC ID token has invalid claims")
    aud = claims.get("aud")
    require(aud == audience or (isinstance(aud, list) and audience in aud), "OIDC audience differs from the approved issuer")
    issuer, subject = claims.get("iss"), claims.get("sub")
    return principal_fingerprints(issuer, subject, claims.get("email") or subject)


def read_token(path: Path) -> str:
    with path.open("rb") as token_file:
        return token_file.read(20003).removesuffix(b"\n").removesuffix(b"\r").decode("latin-1")


def measure(fixture: dict, key: str, authority: dict, tokens: dict[str, str | None], audience: str) -> tuple[dict, list]:
    def approve_token(role: str, token: str) -> None:
        require(token_principal(token, audience) == authority["principals"][role],
                "OIDC token is from a different approved cache reuse principal")

    for role, token in tokens.items():
        if token is not None:
            approve_token(role, token)
    edge, access, _, family, _ = scope_parts(fixture["scope"])
    attest_fixture(fixture, edge, access)
    base = fixture_base(fixture["base_url"])
    grants, requests = {}, []
    for role, token in tokens.items():
        if token is None:
            token = github_id_token(audience)
            approve_token(role, token)
        if role == "second" and "primary" in grants:
            # The reference issuer can sign an identical policy twice in one second.
            next_second = grants["primary"]["issued_at_unix_ms"] // 1000 + 1
            time.sleep(max(0, next_second - time.time()))
        # The pinned issuer validates this exact token in both the header and
        # request body before issuing a read grant for the approved fixture.
        viewer = issue_viewer(fixture, edge, family, token)
        issued = time.time_ns() // 1_000_000
        grants[role] = authority["principals"][role] | {
            "issued_at_unix_ms": issued, "expires_at_unix": viewer_expiry(edge, viewer),
        }
        started = time.time_ns() // 1_000_000
        row, body = request_sample(base, edge, key, viewer, size=fixture["size"], check=CACHE_REUSE_CHECK, wanted=200)
        finished = time.time_ns() // 1_000_000
        row |= {"principal": role, "body_sha256": hashlib.sha256(body).hexdigest(),
                "started_at_unix_ms": started, "finished_at_unix_ms": finished}
        verify_cache_reuse_read(role, authority["principals"][role], authority["object"], grants[role], row)
        requests.append(row)
    attest_fixture(fixture, edge, access)
    return grants, requests


def hosted_measure(
    fixture: dict, key: str, authority: dict, audience: str, provenance: dict, role: str, primary_path: Path | None,
) -> dict:
    require((primary_path is not None) == (role == "second"), "second viewer requires the same-attempt primary observation")
    runner = os.environ.get("RUNNER_NAME", "")
    require(matches(IDENTITY_CLAIM, runner), "hosted cache reuse requires the protected measurement runner")
    common = provenance | {
        "schema": PRIMARY_OBSERVATION_SCHEMA, "authority_sha256": deployment_component_digest(authority),
        "runner_sha256": hashlib.sha256(runner.encode()).hexdigest(),
    }
    if primary_path is not None:
        primary = fields(decode(read_trace(primary_path, "primary cache reuse observation")),
                         CDN_PROVENANCE_FIELDS | {"schema", "authority_sha256", "runner_sha256", "grant", "request"},
                         "primary cache reuse observation")
        require(all(primary[name] == value for name, value in common.items()),
                "primary cache reuse observation must match the approved fixture, runner, and attempt")
        verify_cache_reuse_read("primary", authority["principals"]["primary"], authority["object"],
                                primary["grant"], primary["request"])
    grants, requests = measure(fixture, key, authority, {role: None}, audience)
    if role == "primary":
        return common | {"grant": grants[role], "request": requests[0]}
    return full_trace(provenance, authority, {"primary": primary["grant"]} | grants, [primary["request"], *requests])


def full_trace(provenance: dict, authority: dict, grants: dict, requests: list) -> dict:
    trace = provenance | {"schema": "crab.cdn-authenticated-cache-reuse/v1", "check": CACHE_REUSE_CHECK,
                          "authority": authority, "grants": grants, "requests": requests}
    verify_cache_reuse(trace, provenance, deployment_component_digest(authority))
    return trace


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--principals", required=True, type=Path, help="approved OIDC issuers, subjects and policy identities")
    parser.add_argument("--cache-reuse-sha256", help="preregistered digest from inspection without --output")
    parser.add_argument("--primary-token-file", type=Path)
    parser.add_argument("--second-token-file", type=Path)
    parser.add_argument("--github-role", choices=CACHE_REUSE_ROLES, help="use this job's protected GitHub OIDC subject")
    parser.add_argument("--primary-observation", type=Path, help="redacted observation from the primary GitHub job")
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
        require(not args.output.exists() and not args.output.is_symlink() and args.output.parent.is_dir(),
                "cache reuse output must be a new trace file")
        provenance = {"scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
                      "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
        if args.github_role is not None:
            require(args.primary_token_file is None and args.second_token_file is None,
                    "GitHub cache reuse must use this job's OIDC token exclusively")
            trace = hosted_measure(fixture, key, authority, audience, provenance, args.github_role, args.primary_observation)
        else:
            require(args.primary_observation is None and args.primary_token_file is not None and args.second_token_file is not None,
                    "supply both OIDC ID token files")
            tokens = {"primary": read_token(args.primary_token_file), "second": read_token(args.second_token_file)}
            grants, requests = measure(fixture, key, authority, tokens, audience)
            trace = full_trace(provenance, authority, grants, requests)
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(trace, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
