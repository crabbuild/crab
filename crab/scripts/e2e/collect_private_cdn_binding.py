#!/usr/bin/env python3
"""Inspect and prove a private CDN binding for an operator's repository."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from pathlib import Path

from cdn_deployment_policy_cli import observe_deployment_policy
from cdn_viewer_issuer import verify_viewer_scope
from run_cdn_transport_checks import (
    FAMILY_ROOTS, approved_fixture, collect, fixture_repository_prefix, fixture_object_prefix,
    private_binding_candidate, private_viewer_headers, viewer_headers,
)

from cdn_qualification import SHA256, CdnEvidenceError, matches, require, scope_parts


def sign_probe(fixture: dict, edge: str, physical: str, prefix: str, key_file: Path, key_id: str) -> dict[str, str]:
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "deploy/auth-service"))
    try:
        from src.cdn import CdnExport, sign_viewer_prefixes
    except ImportError:
        raise CdnEvidenceError("operator signing requires the Crab Auth Python dependencies") from None
    try:
        export = CdnExport(provider=edge.replace("-", "_"), origin=fixture["native_origin"],
                           base_url=fixture["base_url"], repo_prefix=fixture["fixture_prefix"],
                           protected_view_suffix=fixture["view_suffix"], key_file=key_file, key_id=key_id)
        now = int(time.time())
        (grant,) = sign_viewer_prefixes(export, physical, [prefix], now, now + 300)
        return private_viewer_headers(edge, grant.credential)
    except (ValueError, OSError, TypeError):
        raise CdnEvidenceError("could not sign the approved probe's viewer prefix") from None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binding", type=Path, required=True, help="private repository and deployment JSON")
    parser.add_argument("--configuration-sha256", required=True, help="SHA-256 of the reviewed binding JSON")
    parser.add_argument("--output", type=Path, help="write a verified candidate. Omit to inspect policy without a viewer credential")
    parser.add_argument("--deployment-policy-sha256", help="approved digest from the policy inspection")
    viewer = parser.add_mutually_exclusive_group()
    viewer.add_argument("--viewer-file", type=Path, help="short-lived viewer credential for the exact approved object family")
    viewer.add_argument("--signing-key-file", type=Path, help="sign the probe with a local operator key without writing a viewer file")
    parser.add_argument("--signing-key-id", help="provider key ID paired with --signing-key-file")
    parser.add_argument("--native-read-url-file", type=Path, help="use a signed exact-object native URL instead of the provider login")
    args = parser.parse_args()
    try:
        require((args.signing_key_file is not None) == (args.signing_key_id is not None),
                "operator signing requires both the key file and its provider key ID")
        if args.output is None:
            require(not any((args.deployment_policy_sha256, args.viewer_file, args.signing_key_file, args.native_read_url_file)),
                    "policy inspection does not accept transport credentials or a policy digest")
        else:
            require(matches(SHA256, args.deployment_policy_sha256) and (args.viewer_file or args.signing_key_file),
                    "binding collection requires an approved policy digest and viewer credential or operator key")
            require(args.output.parent.is_dir() and not args.output.exists(), "binding candidate needs a new output file")
        fixture = approved_fixture(args.binding, args.configuration_sha256, operator_binding=True)
        edge, access, _, family, _ = scope_parts(fixture["scope"])
        policy = observe_deployment_policy(fixture, edge, access)
        if args.output is None:
            print(json.dumps(policy, indent=2, sort_keys=True))
            return 0
        require(policy["deployment_policy_sha256"] == args.deployment_policy_sha256,
                "observed CDN deployment policy differs from the operator's approval")
        physical = fixture_repository_prefix(fixture, access, family, operator_binding=True)
        prefix = f"{physical}/{fixture_object_prefix(fixture, FAMILY_ROOTS[family])}/"
        if args.signing_key_file is not None:
            viewer = sign_probe(fixture, edge, physical, prefix, args.signing_key_file, args.signing_key_id)
        else:
            viewer = viewer_headers(edge, access, args.viewer_file)
        verify_viewer_scope(edge, viewer, fixture["native_origin"], fixture["base_url"].rstrip("/"), physical, prefix)
        traces = collect(fixture, edge, access, family, viewer, None,
                         native_read_url_file=args.native_read_url_file,
                         native_identity_read=args.native_read_url_file is None, operator_binding=True)
        require(observe_deployment_policy(fixture, edge, access) == policy,
                "CDN deployment changed while probing the operator binding")
        candidate = private_binding_candidate(fixture, policy, traces, args.configuration_sha256, operator_binding=True)
        raw = (json.dumps(candidate, sort_keys=True) + "\n").encode()
        with args.output.open("xb") as output:
            output.write(raw)
        print(json.dumps({"approval_sha256": hashlib.sha256(raw).hexdigest()}))
    except CdnEvidenceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, RecursionError):
        print("error: could not collect the private CDN binding", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
