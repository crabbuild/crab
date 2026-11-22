"""Common fixture approval and output for read-only CDN policy collectors."""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from run_cdn_transport_checks import approved_fixture, deployment_target

from cdn_qualification import (
    COMMIT, RUN_NUMBER, CdnEvidenceError, deployment_component_digest, deployment_policy_digest, matches, require, scope_parts,
)


class PolicyDeploymentPending(CdnEvidenceError):
    """The provider explicitly reports that deployment is in progress."""


@dataclass(frozen=True)
class PrivateBinding:
    deployment: str
    native_resource: str
    native_location: str

    def route(self, fixture: dict) -> dict[str, str]:
        return {
            "deployment": self.deployment,
            "native_resource": self.native_resource,
            "native_location": self.native_location,
            "native_origin": fixture["native_origin"],
            "repo_prefix": fixture["fixture_prefix"],
            "view_suffix": fixture["view_suffix"],
            "base_url": fixture["base_url"].rstrip("/"),
        }


@dataclass(frozen=True)
class PolicyObservation:
    components: dict[str, str]
    private_binding: PrivateBinding | None = None

    def bound_components(self, fixture: dict) -> dict[str, str]:
        if self.private_binding is None:
            return self.components
        return self.components | {"private-binding": deployment_component_digest(self.private_binding.route(fixture))}


def deployment_parser(description: str) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--output", type=Path, help="write attested evidence, or print a candidate digest when omitted")
    parser.add_argument("--source-sha")
    parser.add_argument("--run-id")
    parser.add_argument("--run-attempt")
    return parser


def observe_deployment_policy(fixture: dict, edge: str, access: str) -> dict:
    target = deployment_target(fixture, edge)
    try:
        if edge == "cloudfront":
            from collect_cloudfront_deployment_policy import collect

            observation = collect(fixture, profile=None, **target)
        elif edge == "cloud-cdn":
            from collect_cloud_cdn_deployment_policy import collect

            observation = collect(fixture, configuration=None, **target)
        else:
            from collect_front_door_deployment_policy import collect

            observation = collect(fixture, subscription=target["subscription"], group=target["resource_group"], profile=target["profile"])
        components = observation.bound_components(fixture)
    except CdnEvidenceError:
        raise
    except (ValueError, OSError, KeyError, TypeError, AttributeError):
        raise CdnEvidenceError("CDN deployment policy collection failed") from None
    digest = deployment_policy_digest(edge, access, components)
    policy = {"schema": "crab.cdn-deployment-policy/v1", "components": components, "deployment_policy_sha256": digest}
    if observation.private_binding is not None:
        policy["private_binding"] = observation.private_binding.route(fixture)
    return policy


def attest_fixture(fixture: dict, edge: str, access: str) -> dict:
    policy = observe_deployment_policy(fixture, edge, access)
    require(policy["deployment_policy_sha256"] == fixture["deployment_policy_sha256"],
            "observed CDN deployment policy differs from the approved fixture")
    return policy


def collect_deployment_policy(args: argparse.Namespace, provider: str, collect: Callable[[dict], PolicyObservation]) -> int:
    try:
        if args.output is not None:
            require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id) and matches(RUN_NUMBER, args.run_attempt),
                    "attestation needs exact workflow provenance")
            require(not args.output.exists(), "deployment policy trace must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, access, _, _, _ = scope_parts(fixture["scope"])
        components = collect(fixture).bound_components(fixture)
        digest = deployment_policy_digest(edge, access, components)
        policy = {"components": components, "deployment_policy_sha256": digest}
        if args.output is None:
            print(json.dumps(policy, sort_keys=True))
            return 0
        require(digest == fixture["deployment_policy_sha256"], f"observed {provider} policy differs from the approved fixture")
        policy |= {"schema": "crab.cdn-deployment-policy/v1", "scope": fixture["scope"], "source_sha": args.source_sha,
                   "configuration_sha256": args.configuration_sha256,
                   "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(policy, sort_keys=True) + "\n")
    except CdnEvidenceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except (ValueError, OSError, KeyError, TypeError, AttributeError):
        print(f"error: {provider} policy collection failed", file=sys.stderr)
        return 1
    return 0
