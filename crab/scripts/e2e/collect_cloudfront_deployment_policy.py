#!/usr/bin/env python3
"""Attest a deployed CloudFront qualification fixture using read-only AWS APIs."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
from pathlib import Path
from tempfile import TemporaryDirectory
from textwrap import dedent
from typing import Any

from cdn_deployment_policy_cli import (
    PolicyDeploymentPending, PolicyObservation, PrivateBinding, collect_deployment_policy, deployment_parser,
)
from run_cdn_transport_checks import FAMILY_ROOTS, S3_REGION, origin_bucket

from cdn_qualification import (
    SHA256, CdnEvidenceError, decode, deployment_component_digest, matches, public_export_binding, require, scope_parts,
)


TEMPLATES = Path(__file__).resolve().parents[2] / "deploy/cdn/aws"
RESOURCE_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
PUBLIC_ACCESS_BLOCK = dict.fromkeys(("BlockPublicAcls", "IgnorePublicAcls", "BlockPublicPolicy", "RestrictPublicBuckets"), True)


def s3_region(response: dict[str, Any]) -> str:
    location = response["LocationConstraint"]
    return "us-east-1" if location is None else "eu-west-1" if location == "EU" else location


def aws(profile: str | None, region: str, service: str, operation: str, *arguments: str) -> dict[str, Any]:
    credentials = ["--profile", profile] if profile is not None else []
    try:
        result = subprocess.run(
            ["aws", *credentials, "--region", region, "--no-cli-pager", "--no-cli-auto-prompt",
             "--output", "json", service, operation, *arguments], capture_output=True, timeout=45,
            env={**os.environ, "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS": "true"},
        )
    except (OSError, subprocess.TimeoutExpired):
        raise CdnEvidenceError(f"could not run AWS {operation}") from None
    require(result.returncode == 0, f"AWS {operation} failed (exit {result.returncode})")
    value = decode(result.stdout)
    require(isinstance(value, dict), f"AWS {operation} did not return an object")
    return value


def aws_items(parent: dict[str, Any], key: str) -> list[Any]:
    group = parent.get(key, {"Quantity": 0})
    items = group.get("Items", [])
    require(isinstance(items, list) and group.get("Quantity") == len(items), f"incomplete CloudFront {key}")
    return items


def cloudfront_id(value: Any) -> str:
    require(matches(RESOURCE_ID, value), "invalid CloudFront resource identifier")
    return value


def verify_distribution(distribution: dict, bucket: str, region: str) -> tuple[dict, dict]:
    config = distribution["DistributionConfig"]
    if config["Enabled"] is True and distribution["Status"] == "InProgress":
        raise PolicyDeploymentPending("CloudFront distribution deployment is in progress")
    require(distribution["Status"] == "Deployed" and config["Enabled"] is True,
            "CloudFront distribution must be deployed and enabled")
    require(not any(aws_items(config, name) for name in ("Aliases", "OriginGroups", "CacheBehaviors"))
            and not any(config.get(name) for name in ("DefaultRootObject", "Staging", "ContinuousDeploymentPolicyId")),
            "qualification distribution needs one default route and its own CloudFront hostname")
    origins = aws_items(config, "Origins")
    require(len(origins) == 1, "qualification CloudFront distribution needs exactly one origin")
    origin = origins[0]
    domains = {f"{bucket}.s3.{region}.amazonaws.com"}
    if region == "us-east-1":
        domains.add(f"{bucket}.s3.amazonaws.com")
    require(origin["DomainName"] in domains and not origin.get("OriginPath") and not aws_items(origin, "CustomHeaders")
            and origin["S3OriginConfig"]["OriginAccessIdentity"] == "", "CloudFront needs the approved unrewritten S3 REST origin")
    behavior = config["DefaultCacheBehavior"]
    require(behavior["TargetOriginId"] == origin["Id"] and behavior["ViewerProtocolPolicy"] == "https-only"
            and behavior["Compress"] is False and not aws_items(behavior, "LambdaFunctionAssociations")
            and not any(behavior.get(name) for name in ("OriginRequestPolicyId", "ForwardedValues", "FieldLevelEncryptionId")),
            "CloudFront default behavior must use HTTPS and preserve the approved representation and authorization")
    require(set(aws_items(behavior, "AllowedMethods")) == {"GET", "HEAD"}
            and set(aws_items(behavior["AllowedMethods"], "CachedMethods")) == {"GET", "HEAD"},
            "CloudFront behavior must allow only read methods")
    errors = aws_items(config, "CustomErrorResponses")
    require({403, 404, 500, 502, 503, 504} <= {item["ErrorCode"] for item in errors} and all(
        item["ErrorCachingMinTTL"] == 0 and not item.get("ResponseCode") and not item.get("ResponsePagePath") for item in errors
    ), "CloudFront must preserve origin error status and use the lowest configured error TTL")
    return origin, behavior


def verify_cache(cache: dict[str, Any]) -> None:
    key = cache["ParametersInCacheKeyAndForwardedToOrigin"]
    require(cache["MinTTL"] == 0 and 0 < cache["DefaultTTL"] <= cache["MaxTTL"]
            and key["EnableAcceptEncodingGzip"] is False and key.get("EnableAcceptEncodingBrotli", False) is False
            and key["CookiesConfig"]["CookieBehavior"] == "none" and key["HeadersConfig"]["HeaderBehavior"] == "none"
            and key["QueryStringsConfig"]["QueryStringBehavior"] == "none", "unsafe CloudFront cache key or TTL")


def expected_function(access: str, prefix: str, suffix: str) -> bytes:
    template = TEMPLATES / ("cloudfront.yaml" if access == "private" else "public-export.yaml")
    source = re.search(r"FunctionCode: !Sub \|\n((?: {8}[^\n]*\n)+)", template.read_text(encoding="utf-8"))
    require(source is not None, "could not identify the reference CloudFront viewer function")
    return dedent(source[1]).replace("${RepositoryPrefix}", prefix).replace("${ProtectedViewSuffix}", suffix).strip().encode()


def viewer_controls(cloudfront, behavior: dict, access: str) -> dict[str, Any]:
    group = behavior.get("TrustedKeyGroups", {"Enabled": False, "Quantity": 0})
    groups = aws_items(behavior, "TrustedKeyGroups")
    require(not aws_items(behavior, "TrustedSigners") and group["Enabled"] is (access == "private")
            and len(groups) == (1 if access == "private" else 0), "CloudFront trusted key groups disagree with the approved access mode")
    if access == "public":
        require(not behavior.get("ResponseHeadersPolicyId"), "unexpected public CloudFront response header policy")
        return {"key_groups": []}
    group_id = cloudfront_id(groups[0])
    trusted = cloudfront("get-key-group-config", "--id", group_id)["KeyGroupConfig"]
    keys = trusted["Items"]
    require(isinstance(keys, list) and 1 <= len(keys) <= 2 and len(set(keys)) == len(keys),
            "private CloudFront requires one or two distinct trusted viewer keys")
    headers_id = cloudfront_id(behavior.get("ResponseHeadersPolicyId"))
    headers = cloudfront("get-response-headers-policy-config", "--id", headers_id)["ResponseHeadersPolicyConfig"]
    expected = [{"Header": "Cache-Control", "Value": "private, no-store", "Override": True}]
    require(aws_items(headers, "CustomHeadersConfig") == expected and not headers.get("RemoveHeadersConfig", {}).get("Quantity"),
            "private CloudFront responses must override Cache-Control with private, no-store")
    public_keys = {key_id: cloudfront("get-public-key-config", "--id", cloudfront_id(key_id))["PublicKeyConfig"] for key_id in sorted(keys)}
    return {"key_group": trusted, "public_keys": public_keys, "response_headers": headers}


def verify_active_keys(distribution: dict, behavior: dict, expected_keys: dict) -> None:
    configured = aws_items(behavior, "TrustedKeyGroups")
    groups = aws_items(distribution, "ActiveTrustedKeyGroups")
    require(distribution.get("ActiveTrustedKeyGroups", {}).get("Enabled") is True
            and [group["KeyGroupId"] for group in groups] == configured,
            "CloudFront active key group differs from the approved route")
    active_keys = aws_items(groups[0], "KeyPairIds")
    # The key group can report the new configuration before the distribution can use it.
    if sorted(active_keys) != sorted(expected_keys):
        raise PolicyDeploymentPending("CloudFront active viewer keys are still propagating")


def verify_bucket_policy(policy: dict, bucket: str, prefix: str, protected: bool, arn: str, access: str) -> None:
    statements = policy["Statement"]
    families = set().union(*(FAMILY_ROOTS[name] for name in ("capsule", "git", "lfs"))) - {"v2/history/source-roots"}
    if protected:
        families |= set().union(*(FAMILY_ROOTS[name] for name in ("xorb", "chunk-page", "shard")))
    resources = {f"arn:aws:s3:::{bucket}/{prefix}/{family}/*" for family in families}
    allows = [rule for rule in statements if rule["Effect"] == "Allow"]
    require(len(allows) == 1, "S3 fixture policy needs exactly one read allow")
    allow = {key: value for key, value in allows[0].items() if key != "Sid"}
    expected = {"Effect": "Allow", "Principal": {"Service": "cloudfront.amazonaws.com"}, "Action": "s3:GetObject",
                "Resource": sorted(resources), "Condition": {"StringEquals": {"AWS:SourceArn": arn}}}
    require(allow | {"Resource": sorted(allow["Resource"])} == expected,
            "S3 read allow must cover only the intended distribution and immutable prefixes")
    if access == "private":
        require(len(statements) == 1, "unexpected private S3 bucket policy statements")
        return
    validate_export_writer(statements, bucket)


def validate_export_writer(statements: list[dict], bucket: str) -> None:
    by_sid = {rule.get("Sid"): rule for rule in statements if rule["Effect"] == "Deny"}
    require(len(statements) == 4 and len(by_sid) == 3, "public S3 export needs the dedicated create-only writer policy")
    role = by_sid["RequireDedicatedWriter"]["Condition"]["ArnNotEquals"]["aws:PrincipalArn"]
    require(matches(re.compile(r"arn:aws:iam::[0-9]{12}:role/[A-Za-z0-9+=,.@_/-]+"), role), "invalid S3 export writer role")
    resource = f"arn:aws:s3:::{bucket}/*"
    deny = {"Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "Resource": resource}
    expected = {
        "RequireDedicatedWriter": deny | {"Condition": {"ArnNotEquals": {"aws:PrincipalArn": role}}},
        "RequireCreateOnlyWrites": deny | {"Condition": {"Bool": {"s3:ObjectCreationOperation": "true"},
                                                          "StringNotEquals": {"s3:if-none-match": "*"}}},
        "KeepWriterFromReplacingViaDelete": {"Effect": "Deny", "Principal": {"AWS": role}, "Resource": resource,
                                             "Action": ["s3:DeleteObject", "s3:DeleteObjectVersion"]},
    }
    require(all(by_sid[name] == config | {"Sid": name} for name, config in expected.items()),
            "public S3 export needs conditional writes and writer deletion denial")


def collect(fixture: dict, distribution_id: str, profile: str | None, region: str) -> PolicyObservation:
    edge, access, _, _, _ = scope_parts(fixture["scope"])
    require(edge == "cloudfront" and matches(S3_REGION, region), "CloudFront qualification needs the AWS fixture and a commercial region")
    request = ("--id", cloudfront_id(distribution_id))

    def cloudfront(operation: str, *arguments: str) -> dict[str, Any]:
        return aws(profile, region, "cloudfront", operation, *arguments)

    observed = cloudfront("get-distribution", *request)
    distribution = observed["Distribution"]
    arn = distribution["ARN"]
    owner = re.fullmatch(rf"arn:aws:cloudfront::([0-9]{{12}}):distribution/{distribution_id}", arn)
    hostname = distribution["DomainName"]
    require(owner is not None and distribution["Id"] == distribution_id
            and fixture["base_url"].rstrip("/") == f"https://{hostname}" and hostname.endswith(".cloudfront.net"),
            "CloudFront control-plane identity differs from the approved fixture")
    bucket = origin_bucket(fixture["export_origin"] if access == "public" else fixture["native_origin"], edge)
    origin, behavior = verify_distribution(distribution, bucket, region)
    oac = cloudfront("get-origin-access-control-config", "--id", cloudfront_id(origin.get("OriginAccessControlId")))["OriginAccessControlConfig"]
    require((oac["OriginAccessControlOriginType"], oac["SigningBehavior"], oac["SigningProtocol"]) == ("s3", "always", "sigv4"),
            "CloudFront S3 OAC must sign every origin request")
    cache = cloudfront("get-cache-policy-config", "--id", cloudfront_id(behavior.get("CachePolicyId")))["CachePolicyConfig"]
    verify_cache(cache)
    viewer = viewer_controls(cloudfront, behavior, access)
    if access == "private":
        verify_active_keys(distribution, behavior, viewer["public_keys"])

    functions = aws_items(behavior, "FunctionAssociations")
    require(len(functions) == 1 and functions[0]["EventType"] == "viewer-request", "CloudFront needs one viewer-request function")
    function = re.fullmatch(rf"arn:aws:cloudfront::{owner[1]}:function/([A-Za-z0-9_-]{{1,64}})", functions[0]["FunctionARN"])
    require(function is not None, "invalid CloudFront viewer function identity")
    with TemporaryDirectory(prefix="crab-cloudfront-function-") as temporary:
        path = Path(temporary) / "function.js"
        cloudfront("get-function", "--name", function[1], "--stage", "LIVE", str(path))
        function_code = path.read_bytes().strip()
    prefix, suffix = fixture["fixture_prefix"], fixture["view_suffix"]
    require(function_code == expected_function(access, prefix, suffix), "active CloudFront function differs from the reviewed fixture policy")

    def s3(operation: str) -> dict[str, Any]:
        return aws(profile, region, "s3api", operation, "--bucket", bucket, "--expected-bucket-owner", owner[1])

    policy = decode(s3("get-bucket-policy")["Policy"].encode())
    verify_bucket_policy(policy, bucket, prefix + suffix, bool(suffix), arn, access)
    public_block = s3("get-public-access-block")["PublicAccessBlockConfiguration"]
    ownership = s3("get-bucket-ownership-controls")["OwnershipControls"]
    require(all(public_block.get(name) is True for name in PUBLIC_ACCESS_BLOCK)
            and ownership["Rules"] == [{"ObjectOwnership": "BucketOwnerEnforced"}]
            and s3("get-bucket-policy-status")["PolicyStatus"]["IsPublic"] is False,
            "S3 fixture must block direct public access and disable ACLs")
    bucket_controls = {"public_access_block": public_block, "ownership": ownership}
    location = s3_region(s3("get-bucket-location"))
    require(location == region, "S3 native bucket location differs from the approved distribution origin")
    placement = {"resource": f"accounts/{owner[1]}/buckets/{bucket}", "location": location}
    export_binding = {}
    private_binding = None
    if access == "public":
        tags = {entry["Key"]: entry["Value"] for entry in s3("get-bucket-tagging")["TagSet"]}
        inventory = tags.get("CrabApprovedInventorySha256")
        require(tags.get("CrabSourceOrigin") == fixture["native_origin"] and matches(SHA256, inventory),
                "public S3 bucket tags must identify the approved source and inventory")
        bucket_controls["tags"] = tags
        source = fixture["source_placement"]
        source_bucket = origin_bucket(fixture["native_origin"], edge)
        source_location = s3_region(aws(
            profile, source["location"], "s3api", "get-bucket-location", "--bucket", source_bucket,
            "--expected-bucket-owner", source["resource"].split("/")[1],
        ))
        export_binding["public-export"] = public_export_binding(
            fixture, arn, inventory, source | {"location": source_location}, placement,
        )
    else:
        private_binding = PrivateBinding(arn, placement["resource"], location)
    current = cloudfront("get-distribution", *request)
    current_distribution = current["Distribution"]
    if current_distribution["Status"] == "InProgress" and current_distribution["DistributionConfig"]["Enabled"] is True:
        raise PolicyDeploymentPending("CloudFront distribution deployment is in progress")
    require(current["ETag"] == observed["ETag"] and current_distribution["Status"] == "Deployed",
            "CloudFront distribution changed during attestation")
    if access == "private":
        verify_active_keys(current_distribution, behavior, viewer["public_keys"])
    components = {name: deployment_component_digest(value) for name, value in {
        "distribution": {"id": distribution_id, "arn": arn, "hostname": hostname, "config": distribution["DistributionConfig"]},
        "origin-access-control": oac, "cache-policy": cache, "viewer-controls": viewer,
        "bucket-policy": policy, "bucket-access": bucket_controls,
        "viewer-key-ids": sorted(viewer.get("public_keys", {})), **export_binding,
    }.items()} | {"viewer-function": hashlib.sha256(function_code).hexdigest()}
    return PolicyObservation(components, private_binding)


def main() -> int:
    parser = deployment_parser(__doc__)
    parser.add_argument("--distribution-id", required=True)
    parser.add_argument("--profile")
    parser.add_argument("--region", required=True)
    args = parser.parse_args()
    return collect_deployment_policy(args, "CloudFront", lambda fixture: collect(fixture, args.distribution_id, args.profile, args.region))


if __name__ == "__main__":
    raise SystemExit(main())
