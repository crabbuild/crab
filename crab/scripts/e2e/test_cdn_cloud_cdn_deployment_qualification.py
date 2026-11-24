"""Exercise Cloud CDN control-plane checks with representative Google API resources."""

import socket
import unittest
from unittest.mock import patch

import collect_cloud_cdn_deployment_policy as COLLECTOR
from cdn_qualification import deployment_component_digest, deployment_component_names, public_export_binding
from test_cdn_transport_qualification import REPO, fixture


PROJECT = "crab-cdn-test"
NUMBER = "123456789012"
HOST = "fixture.example.com"
ADDRESS = "203.0.113.5"
RULE = "crab-fixture-https"
AUTHORIZATION = "Bearer fixture-secret"


class CloudCdnDeploymentQualificationTests(unittest.TestCase):
    def configure(self, access: str = "private", *, protected: bool = False) -> None:
        self.bucket = "fixture-bucket" if access == "private" else "export-bucket"
        self.fixture = fixture("cloud-cdn", access) | {"base_url": f"https://{HOST}"}
        view = f"/acl-views/v2/{'a' * 64}/7-{'b' * 64}" if protected else ""
        physical = REPO + view
        self.fixture["view_suffix"] = view
        self.documents = {(COLLECTOR.RESOURCES, f"/v1/projects/{PROJECT}"): {"projectId": PROJECT, "projectNumber": NUMBER}}
        self.forwarding = self.compute("forwardingRules", RULE, {
            "target": self.reference("targetHttpsProxies", "proxy"), "IPProtocol": "TCP", "portRange": "443-443",
            "loadBalancingScheme": "EXTERNAL_MANAGED", "networkTier": "PREMIUM", "IPAddress": ADDRESS,
        })
        self.proxy = self.compute("targetHttpsProxies", "proxy", {
            "sslCertificates": [self.reference("sslCertificates", "certificate")],
            "urlMap": self.reference("urlMaps", "map"),
        })
        self.compute("sslCertificates", "certificate", {"type": "MANAGED", "managed": {"status": "ACTIVE", "domains": [HOST]}})
        self.url_map = self.compute("urlMaps", "map", {"defaultService": self.reference("backendBuckets", "bucket")})
        cdn_policy = {"cacheMode": "USE_ORIGIN_HEADERS", "negativeCaching": False, "serveWhileStale": 0}
        headers = ["X-Crab-CDN-Cache: {cdn_cache_status}"]
        if access == "private":
            cdn_policy |= {"signedUrlKeyNames": ["test-key"], "signedUrlCacheMaxAgeSec": "86400"}
            headers.append("Cache-Control: private, no-store")
        self.backend = self.compute("backendBuckets", "bucket", {
            "bucketName": self.bucket, "enableCdn": True, "compressionMode": "DISABLED", "cdnPolicy": cdn_policy,
            "customResponseHeaders": headers, "edgeSecurityPolicy": self.reference("securityPolicies", "armor"),
        })
        denies, allows = COLLECTOR.expected_armor_expressions(HOST, physical, protected, access)
        self.rules = [{"priority": offset + index, "action": action, "preview": False,
                       "match": {"expr": {"expression": expression}, "versionedExpr": "SRC_IPS_V1"}}
                      for offset, action, expressions in ((100, "deny(403)", denies), (1000, "allow", allows))
                      for index, expression in enumerate(expressions)]
        self.rules.append({"priority": 2147483647, "action": "deny(403)",
                           "match": {"versionedExpr": "SRC_IPS_V1", "config": {"srcIpRanges": ["*"]}}})
        self.compute("securityPolicies", "armor", {"type": "CLOUD_ARMOR_EDGE", "rules": self.rules})

        path = f"/storage/v1/b/{self.bucket}"
        self.metadata = {"name": self.bucket, "projectNumber": NUMBER, "location": "NAM4", "iamConfiguration": {
            "publicAccessPrevention": "enforced" if access == "private" else "inherited",
            "uniformBucketLevelAccess": {"enabled": True},
        }}
        self.documents[(COLLECTOR.STORAGE, path)] = self.metadata
        condition = {"expression": COLLECTOR.bucket_condition(self.bucket, physical, protected)}
        if access == "private":
            self.bindings = [{"role": "roles/storage.objectViewer", "condition": condition,
                              "members": [f"serviceAccount:service-{NUMBER}@cloud-cdn-fill.iam.gserviceaccount.com"]}]
        else:
            self.source = {"name": "fixture-bucket", "projectNumber": "123455", "location": "US"}
            self.documents[(COLLECTOR.STORAGE, "/storage/v1/b/fixture-bucket")] = self.source
            self.metadata |= {"softDeletePolicy": {"retentionDurationSeconds": "0"}, "labels": {
                "crab_public_export": "true", "crab_inventory_0": "b" * 32, "crab_inventory_1": "c" * 32,
            }}
            reader_role = f"projects/{PROJECT}/roles/crab_reader"
            writer_role = f"projects/{PROJECT}/roles/crab_writer"
            self.bindings = [
                {"role": reader_role, "members": ["allUsers"]},
                {"role": writer_role, "members": [f"serviceAccount:writer@{PROJECT}.iam.gserviceaccount.com"], "condition": condition},
            ]
            self.reader = {"name": reader_role, "includedPermissions": ["storage.objects.get"]}
            self.documents[(COLLECTOR.IAM, f"/v1/{reader_role}")] = self.reader
            self.documents[(COLLECTOR.IAM, f"/v1/{writer_role}")] = {
                "name": writer_role, "includedPermissions": ["storage.objects.get", "storage.objects.create"],
            }
        self.documents[(COLLECTOR.STORAGE, path + "/iam?optionsRequestedPolicyVersion=3")] = {"version": 3, "bindings": self.bindings}

    @staticmethod
    def reference(kind: str, name: str) -> str:
        return f"https://www.googleapis.com/compute/v1/projects/{PROJECT}/global/{kind}/{name}"

    def compute(self, kind: str, name: str, fields: dict) -> dict:
        resource = fields | {"name": name, "id": "1", "selfLink": self.reference(kind, name)}
        self.documents[(COLLECTOR.COMPUTE, f"/compute/v1/projects/{PROJECT}/global/{kind}/{name}")] = resource
        return resource

    def read(self, base, path, authorization):
        self.assertEqual(authorization, AUTHORIZATION)
        return self.documents[(base, path)]

    def collect(self):
        addresses = [(socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "", (ADDRESS, 443))]
        with (patch.object(COLLECTOR, "document", side_effect=self.read),
              patch.object(COLLECTOR, "bearer", return_value=AUTHORIZATION) as auth,
              patch.object(COLLECTOR.socket, "getaddrinfo", return_value=addresses) as resolve):
            snapshot = COLLECTOR.collect(self.fixture, PROJECT, RULE, "fixture-reader")
        resolve.assert_called_once_with(HOST, 443, type=socket.SOCK_STREAM)
        auth.assert_called_once_with("gcloud", "--configuration=fixture-reader", "auth", "print-access-token", "--quiet")
        return snapshot

    def test_reference_profiles_attest_their_linked_control_plane(self):
        for access, protected in (("private", False), ("private", True), ("public", False)):
            with self.subTest(access=access, protected=protected):
                self.configure(access, protected=protected)
                observation = self.collect()
                snapshot = observation.bound_components(self.fixture)
                self.assertEqual(snapshot.keys(), deployment_component_names("cloud-cdn", access))
                if access == "public":
                    labels = self.metadata["labels"]
                    inventory = labels["crab_inventory_0"] + labels["crab_inventory_1"]
                    binding = public_export_binding(
                        self.fixture, f"projects/{PROJECT}/global/backendBuckets/bucket", inventory,
                        self.fixture["source_placement"],
                        {"resource": f"projects/{NUMBER}/buckets/{self.bucket}", "location": "NAM4"},
                    )
                    self.assertEqual(snapshot["public-export"], deployment_component_digest(binding))
                else:
                    binding = observation.private_binding.route(self.fixture)
                    self.assertEqual((binding["deployment"], binding["native_resource"], binding["native_location"]),
                                     (f"projects/{PROJECT}/global/backendBuckets/bucket", f"projects/{NUMBER}/buckets/{self.bucket}", "NAM4"))
                    self.assertEqual(snapshot["private-binding"], deployment_component_digest(binding))
                self.assertEqual(snapshot["viewer-key-ids"], deployment_component_digest(["test-key"] if access == "private" else []))
                self.assertTrue(all(len(digest) == 64 for digest in snapshot.values()))
                self.assertNotIn(AUTHORIZATION, str(snapshot))

    def test_anonymous_custom_role_with_listing_fails_public_attestation(self):
        self.configure("public")
        self.reader["includedPermissions"].append("storage.objects.list")
        with self.assertRaisesRegex(ValueError, "GCS export IAM permissions differ"):
            self.collect()

    def test_public_source_project_mismatch_cannot_attest(self):
        self.configure("public")
        self.source["projectNumber"] = "54321"
        with self.assertRaisesRegex(ValueError, "live source differs"):
            self.collect()

    def test_private_attestation_requires_provider_reported_location(self):
        self.configure()
        self.metadata.pop("location")
        with self.assertRaisesRegex(ValueError, "GCS native bucket needs an observed location"):
            self.collect()

    def test_fill_identity_cannot_receive_unscoped_bucket_permission(self):
        self.configure(protected=True)
        self.bindings[0]["condition"]["expression"] = "true"
        with self.assertRaisesRegex(ValueError, "fill identity must be confined"):
            self.collect()

    def test_private_edge_policy_requires_enforced_cookie_check(self):
        self.configure()
        self.rules[2]["preview"] = True
        with self.assertRaisesRegex(ValueError, "must be enforced"):
            self.collect()

    def test_alternate_project_reference_cannot_receive_token(self):
        self.configure()
        self.proxy["urlMap"] = self.proxy["urlMap"].replace(PROJECT, "foreign-project")
        with self.assertRaisesRegex(ValueError, "in the approved project's global route"):
            self.collect()

    def test_backend_cache_key_cannot_depend_on_viewer_authorization(self):
        self.configure()
        self.backend["cdnPolicy"]["cacheKeyPolicy"] = {"includeHttpHeaders": ["Cookie"]}
        with self.assertRaisesRegex(ValueError, "unsafe Cloud CDN cache policy or variant key"):
            self.collect()
