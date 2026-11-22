"""Check CloudFront control-plane evidence against approved reference fixtures."""

from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from itertools import repeat
import hashlib
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import collect_cloudfront_deployment_policy as COLLECTOR
from cdn_qualification import (
    deployment_component_digest, deployment_component_names, public_export_binding, verify_deployment_policy_trace,
)
from test_cdn_transport_qualification import REPO, fixture


ID = "DISTRIBUTION"
ACCOUNT = "123456789012"
ARN = f"arn:aws:cloudfront::{ACCOUNT}:distribution/{ID}"
REGION = "us-west-2"
HOST = "fixture.cloudfront.net"
PUBLIC_KEY = "public-fixture-key"
WRITER = f"arn:aws:iam::{ACCOUNT}:role/export-writer"


def items(*values):
    return {"Quantity": len(values)} | ({"Items": list(values)} if values else {})


class CloudFrontDeploymentQualificationTests(unittest.TestCase):
    def configure(self, access="private", *, protected=False):
        self.fixture = fixture("cloudfront", access) | {"base_url": f"https://{HOST}"}
        suffix = f"/acl-views/v2/{'a' * 64}/4-{'b' * 64}" if protected else ""
        self.fixture["view_suffix"] = suffix
        self.source = COLLECTOR.expected_function(access, REPO, suffix)
        self.bucket = "fixture-bucket" if access == "private" else "export-bucket"
        self.behavior = {
            "TargetOriginId": "s3", "ViewerProtocolPolicy": "https-only", "Compress": False,
            "AllowedMethods": items("HEAD", "GET") | {"CachedMethods": items("HEAD", "GET")},
            "CachePolicyId": "cache-id", "FunctionAssociations": items({
                "EventType": "viewer-request", "FunctionARN": f"arn:aws:cloudfront::{ACCOUNT}:function/fixture",
            }),
        }
        if access == "private":
            self.behavior |= {"TrustedKeyGroups": items("key-group") | {"Enabled": True}, "ResponseHeadersPolicyId": "headers-id"}
        self.config = {
            "Enabled": True, "Origins": items({"Id": "s3", "DomainName": f"{self.bucket}.s3.{REGION}.amazonaws.com",
                                               "OriginAccessControlId": "oac", "S3OriginConfig": {"OriginAccessIdentity": ""}}),
            "DefaultCacheBehavior": self.behavior,
            "CustomErrorResponses": items(*({"ErrorCode": status, "ErrorCachingMinTTL": 0}
                                            for status in (403, 404, 500, 502, 503, 504))),
        }
        self.cache_key = {"EnableAcceptEncodingGzip": False, "HeadersConfig": {"HeaderBehavior": "none"},
                          "CookiesConfig": {"CookieBehavior": "none"}, "QueryStringsConfig": {"QueryStringBehavior": "none"}}
        immutable = ["v2/capsules", "v2/checkpoints", "v2/catalogs", "v2/history", "v2/pack-layers", "lfs/objects"]
        if protected:
            immutable.extend((".crab/xorbs", ".crab/shards", ".crab/capsule-chunk-pages"))
        self.statements = [{
            "Effect": "Allow", "Principal": {"Service": "cloudfront.amazonaws.com"}, "Action": "s3:GetObject",
            "Resource": [f"arn:aws:s3:::{self.bucket}/{REPO}{suffix}/{family}/*" for family in immutable],
            "Condition": {"StringEquals": {"AWS:SourceArn": ARN}},
        }]
        if access == "public":
            deny = {"Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "Resource": f"arn:aws:s3:::{self.bucket}/*"}
            self.statements.extend((
                deny | {"Sid": "RequireDedicatedWriter", "Condition": {"ArnNotEquals": {"aws:PrincipalArn": WRITER}}},
                deny | {"Sid": "RequireCreateOnlyWrites", "Condition": {"Bool": {"s3:ObjectCreationOperation": "true"},
                                                                          "StringNotEquals": {"s3:if-none-match": "*"}}},
                deny | {"Sid": "KeepWriterFromReplacingViaDelete", "Principal": {"AWS": WRITER},
                        "Action": ["s3:DeleteObject", "s3:DeleteObjectVersion"]},
            ))
        self.etags = iter(("deployed", "deployed"))
        self.active_keys = repeat(["key-id"] if access == "private" else [])
        self.distribution_status = "Deployed"
        self.location = REGION
        self.source_location = None
        self.public_block = {key: True for key in ("BlockPublicAcls", "IgnorePublicAcls", "BlockPublicPolicy", "RestrictPublicBuckets")}

    def aws_response(self, profile, region, service, operation, *arguments):
        if service == "s3api" and self.bucket == "export-bucket" and arguments[1] == "fixture-bucket":
            self.assertEqual((profile, region, operation, arguments),
                             ("crab", "us-east-1", "get-bucket-location",
                              ("--bucket", "fixture-bucket", "--expected-bucket-owner", "210987654321")))
            return {"LocationConstraint": self.source_location}
        self.assertEqual((profile, region), ("crab", REGION))
        if service == "s3api":
            self.assertEqual(arguments, ("--bucket", self.bucket, "--expected-bucket-owner", ACCOUNT))
        if operation == "get-distribution":
            self.assertEqual(arguments, ("--id", ID))
            keys = next(self.active_keys)
            groups = items({"KeyGroupId": "key-group", "KeyPairIds": items(*keys)}) if keys else items()
            return {"ETag": next(self.etags), "Distribution": {
                "ARN": ARN, "Id": ID, "Status": self.distribution_status, "DomainName": HOST, "DistributionConfig": self.config,
                "ActiveTrustedKeyGroups": groups | {"Enabled": bool(keys)},
            }}
        if operation == "get-function":
            self.assertEqual(arguments[:-1], ("--name", "fixture", "--stage", "LIVE"))
            Path(arguments[-1]).write_bytes(self.source + b"\n")
            return {"ETag": "function"}
        return {
            "get-cache-policy-config": {"CachePolicyConfig": {"MinTTL": 0, "MaxTTL": 86400, "DefaultTTL": 86400,
                                                             "ParametersInCacheKeyAndForwardedToOrigin": self.cache_key}},
            "get-origin-access-control-config": {"OriginAccessControlConfig": {"OriginAccessControlOriginType": "s3",
                                                                                "SigningBehavior": "always", "SigningProtocol": "sigv4"}},
            "get-key-group-config": {"KeyGroupConfig": {"Items": ["key-id"]}},
            "get-public-key-config": {"PublicKeyConfig": {"EncodedKey": PUBLIC_KEY}},
            "get-response-headers-policy-config": {"ResponseHeadersPolicyConfig": {"CustomHeadersConfig": items(
                {"Header": "Cache-Control", "Value": "private, no-store", "Override": True},
            )}},
            "get-bucket-policy": {"Policy": json.dumps({"Version": "2012-10-17", "Statement": self.statements})},
            "get-public-access-block": {"PublicAccessBlockConfiguration": self.public_block},
            "get-bucket-ownership-controls": {"OwnershipControls": {"Rules": [{"ObjectOwnership": "BucketOwnerEnforced"}]}},
            "get-bucket-policy-status": {"PolicyStatus": {"IsPublic": False}},
            "get-bucket-location": {"LocationConstraint": self.location},
            "get-bucket-tagging": {"TagSet": [{"Key": "CrabSourceOrigin", "Value": self.fixture["native_origin"]},
                                             {"Key": "CrabApprovedInventorySha256", "Value": "a" * 64}]},
        }[operation]

    def collect(self):
        with patch.object(COLLECTOR, "aws", side_effect=self.aws_response):
            return COLLECTOR.collect(self.fixture, ID, "crab", REGION)

    def test_reference_profiles_produce_only_component_hashes(self):
        for access, protected in (("private", False), ("private", True), ("public", False)):
            with self.subTest(access=access, protected=protected):
                self.configure(access, protected=protected)
                snapshot = self.collect()
                observed = snapshot.bound_components(self.fixture)
                self.assertEqual(observed.keys(), deployment_component_names("cloudfront", access))
                if access == "public":
                    binding = public_export_binding(
                        self.fixture, ARN, "a" * 64, self.fixture["source_placement"],
                        {"resource": f"accounts/{ACCOUNT}/buckets/{self.bucket}", "location": REGION},
                    )
                    self.assertEqual(observed["public-export"], deployment_component_digest(binding))
                else:
                    binding = snapshot.private_binding.route(self.fixture)
                    self.assertEqual((binding["deployment"], binding["native_resource"], binding["native_location"]),
                                     (ARN, f"accounts/{ACCOUNT}/buckets/{self.bucket}", REGION))
                    self.assertEqual(observed["private-binding"], deployment_component_digest(binding))
                self.assertEqual(observed["viewer-key-ids"], deployment_component_digest(["key-id"] if access == "private" else []))
                self.assertEqual(observed["viewer-function"], hashlib.sha256(self.source).hexdigest())
                self.assertNotIn(self.bucket, json.dumps(observed))
                self.assertNotIn(PUBLIC_KEY, json.dumps(observed))

    def test_unsafe_live_settings_cannot_attest(self):
        for control, reason in (
            ("cache", "unsafe CloudFront cache key or TTL"), ("function", "active CloudFront function differs"),
            ("bucket", "S3 fixture must block direct public access"), ("changed", "distribution changed during attestation"),
            ("writer", "export needs conditional writes"), ("error", "must preserve origin error status"),
            ("region", "S3 native bucket location differs"),
        ):
            with self.subTest(control=control):
                self.configure("public" if control == "writer" else "private")
                if control == "cache":
                    self.cache_key["CookiesConfig"]["CookieBehavior"] = "all"
                elif control == "function":
                    self.source += b" changed"
                elif control == "bucket":
                    self.public_block["RestrictPublicBuckets"] = False
                elif control == "writer":
                    self.statements[2]["Condition"]["StringNotEquals"]["s3:if-none-match"] = "different"
                elif control == "error":
                    self.config["CustomErrorResponses"]["Items"][-1]["ErrorCachingMinTTL"] = 10
                elif control == "region":
                    self.location = "us-east-2"
                else:
                    self.etags = iter(("deployed", "changed"))
                with self.assertRaisesRegex(ValueError, reason):
                    self.collect()

    def test_rotation_waits_until_the_configured_keys_are_active_on_both_reads(self):
        for snapshots in ((["key-id", "retired"], ["key-id"]), (["key-id"], ["key-id", "retired"])):
            with self.subTest(snapshots=snapshots):
                self.configure()
                self.active_keys = iter(snapshots)
                with self.assertRaisesRegex(COLLECTOR.PolicyDeploymentPending, "active viewer keys are still propagating"):
                    self.collect()
        self.configure()
        self.distribution_status = "InProgress"
        with self.assertRaisesRegex(COLLECTOR.PolicyDeploymentPending, "deployment is in progress"):
            self.collect()

    def test_public_source_region_mismatch_cannot_attest(self):
        self.configure("public")
        self.source_location = "us-west-1"
        with self.assertRaisesRegex(ValueError, "live source differs"):
            self.collect()

    def test_cli_rechecks_candidate_digest_against_approved_configuration(self):
        self.configure()
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = root / "fixture.json"
            trace = root / "deployment_policy.json"

            def run(*extra):
                raw = json.dumps(self.fixture).encode()
                config.write_bytes(raw)
                args = ["probe", "--fixture", str(config), "--configuration-sha256", hashlib.sha256(raw).hexdigest(),
                        "--distribution-id", ID, "--profile", "crab", "--region", REGION, *extra]
                self.etags = iter(("deployed", "deployed"))
                with (patch.object(COLLECTOR, "aws", side_effect=self.aws_response), patch.object(sys, "argv", args),
                      redirect_stdout(StringIO()) as output, redirect_stderr(StringIO()) as error):
                    status = COLLECTOR.main()
                return status, output.getvalue(), error.getvalue()

            status, output, error = run()
            self.assertEqual((status, error), (0, ""))
            args = ("--output", str(trace), "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2")
            self.assertIn("differs from the approved fixture", run(*args)[2])
            self.assertFalse(trace.exists())
            approved = json.loads(output)["deployment_policy_sha256"]
            self.fixture["deployment_policy_sha256"] = approved
            self.assertEqual(run(*args), (0, "", ""))
            record = json.loads(trace.read_bytes())
            verify_deployment_policy_trace(record, record, "cloudfront", "private", approved)
            self.assertEqual(record["configuration_sha256"], hashlib.sha256(config.read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
