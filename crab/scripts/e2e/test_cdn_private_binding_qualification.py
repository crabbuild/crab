"""Exercise the operator's read-only approval workflow on customer repository paths."""

import hashlib
import json
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import collect_private_cdn_binding as OPERATOR
import run_cdn_transport_checks as TRANSPORT
from cdn_deployment_policy_cli import PrivateBinding
from cdn_qualification import deployment_component_digest
from test_cdn_transport_qualification import (
    BLOB_SIGNATURE, BODY, DIGEST, GCS_SIGNATURE, PACK, S3_SIGNATURE, fixture, response, route_responses, scoped_credential,
)
from test_cdn_viewer_issuer_qualification import NOW


REPO = "Models/Video_v2/inference.v1"
DECLARATIONS = {
    "cloudfront": (
        {"distribution_id": "EDIST", "region": "us-west-2"},
        "arn:aws:cloudfront::123456789012:distribution/EDIST", "accounts/123456789012/buckets/fixture-bucket", "us-west-2",
        "https://s3.us-west-2.amazonaws.com/fixture-bucket", S3_SIGNATURE, "X_Cache", "Hit from cloudfront",
    ),
    "cloud-cdn": (
        {"project": "test-project", "forwarding_rule": "fixture"},
        "projects/test-project/global/backendBuckets/fixture", "projects/123456789/buckets/fixture-bucket", "US",
        "https://storage.googleapis.com/fixture-bucket", GCS_SIGNATURE, "X_Crab_CDN_Cache", "hit",
    ),
    "front-door": (
        {"subscription": "00000000-0000-0000-0000-000000000001", "resource_group": "test", "profile": "fixture"},
        "/subscriptions/00000000-0000-0000-0000-000000000001/resourcegroups/test/providers/microsoft.cdn/profiles/fixture/afdendpoints/fixture/routes/crab-immutable",
        "/subscriptions/00000000-0000-0000-0000-000000000001/resourcegroups/test/providers/microsoft.storage/storageaccounts/fixtureacct/blobservices/default/containers/container",
        "westus2", "https://fixtureacct.blob.core.windows.net/container", BLOB_SIGNATURE, "X_Cache", "TCP_HIT",
    ),
}


class PrivateCdnBindingTests(unittest.TestCase):
    def configure(self, root, edge, **overrides):
        target, deployment, resource, location, base, signature, header, hit = DECLARATIONS[edge]
        self.config = fixture(edge, "private") | {"deployment": target, "fixture_prefix": REPO} | overrides
        self.config.pop("deployment_policy_sha256")
        raw = json.dumps(self.config).encode()
        path = root / "binding.json"
        path.write_bytes(raw)
        self.configuration_sha = hashlib.sha256(raw).hexdigest()
        self.args = ["collect", "--binding", str(path), "--configuration-sha256", self.configuration_sha]
        binding = PrivateBinding(deployment, resource, location).route(self.config)
        self.policy = {"private_binding": binding, "deployment_policy_sha256": deployment_component_digest(binding)}
        repo = self.config["fixture_prefix"] + self.config["view_suffix"]
        key = f"{repo}/{self.config['object_path']}"
        native = root / "native-url"
        native.write_text(f"{base}/{key}?{signature}")
        self.viewer = root / "viewer"
        self.viewer.write_text(scoped_credential(edge, self.config, f"{repo}/v2/pack-layers/", NOW + 180))
        self.approval = root / "approved.json"
        self.transport_args = ["--output", str(self.approval), "--deployment-policy-sha256",
                               self.policy["deployment_policy_sha256"], "--viewer-file", str(self.viewer),
                               "--native-read-url-file", str(native)]
        denied = response(403)
        hot = response(200, BODY, Cache_Control="private, no-store", **{header: hit})
        self.responses = [
            hot, response(206, BODY[:1], Content_Range="bytes 0-0/6", Cache_Control="private, no-store"),
            denied, denied, *route_responses(edge, hot, 404 if edge == "front-door" else 403),
            denied, denied, hot,
        ]
        if edge == "cloudfront":
            self.responses.append(response(403, X_Amz_Bucket_Region="us-west-2"))
        self.responses.extend([denied, response(200, BODY)])

    def run_operator(self, *arguments):
        with (patch.object(sys, "argv", [*self.args, *arguments]),
              redirect_stdout(StringIO()) as output, redirect_stderr(StringIO()) as errors):
            result = OPERATOR.main()
        return result, output.getvalue(), errors.getvalue()

    def test_all_providers_probe_the_customer_repository_and_emit_a_digest(self):
        for edge in DECLARATIONS:
            with self.subTest(edge=edge), TemporaryDirectory() as directory:
                # A protected operation uses an exact materialized view path.
                view = f"/acl-views/v2/{'1' * 64}/3-{'2' * 64}" if edge == "front-door" else ""
                self.configure(Path(directory), edge, view_suffix=view)
                with (patch.object(OPERATOR, "observe_deployment_policy", return_value=self.policy) as inspect,
                      patch.object(TRANSPORT.http.client, "HTTPSConnection") as http,
                      patch("cdn_viewer_issuer.time.time", return_value=NOW)):
                    http.return_value.getresponse.side_effect = self.responses
                    status, draft, _ = self.run_operator()
                    self.assertEqual(status, 0)
                    self.assertEqual(json.loads(draft), self.policy)
                    http.assert_not_called()
                    status, output, errors = self.run_operator(*self.transport_args)
                self.assertEqual(status, 0, errors)
                self.assertEqual(inspect.call_count, 3)
                raw = self.approval.read_bytes()
                receipt = json.loads(raw)
                self.assertEqual(set(receipt), {"schema", "edge", "binding", "configuration_sha256", "deployment_policy_sha256", "object"})
                self.assertEqual(receipt["edge"], edge)
                self.assertEqual(receipt["binding"], self.policy["private_binding"])
                self.assertEqual(receipt["configuration_sha256"], self.configuration_sha)
                key = f"{REPO}{view}/{PACK}"
                self.assertEqual(receipt["object"], {"body_sha256": DIGEST, "bytes": len(BODY),
                                                     "key_sha256": hashlib.sha256(key.encode()).hexdigest()})
                self.assertEqual(json.loads(output), {"approval_sha256": hashlib.sha256(raw).hexdigest()})
                requests = http.return_value.request.call_args_list
                self.assertEqual(requests[0].args, ("GET", f"/crab/{key}"))
                self.assertIn(f"/{key}?", requests[-1].args[1])
                self.assertNotIn(b"test-signature", raw)
                self.assertNotIn(self.viewer.read_bytes(), raw)

    def test_operator_binding_rejects_an_outside_viewer_and_live_policy_drift(self):
        for failure in ("misbound-viewer", "wrong-policy", "changed-policy", "different-native-bytes"):
            with self.subTest(failure=failure), TemporaryDirectory() as directory:
                self.configure(Path(directory), "cloudfront")
                changed = self.policy | {"deployment_policy_sha256": "f" * 64}
                first, last = self.policy, self.policy
                if failure == "misbound-viewer":
                    self.viewer.write_text(scoped_credential("cloudfront", self.config, "other/path/v2/pack-layers/", NOW + 180))
                elif failure == "wrong-policy":
                    first = changed
                elif failure == "changed-policy":
                    last = changed
                else:
                    self.responses[-1] = response(200, b"wrong!")
                with (patch.object(OPERATOR, "observe_deployment_policy", side_effect=[first, last]),
                      patch.object(TRANSPORT.http.client, "HTTPSConnection") as http,
                      patch("cdn_viewer_issuer.time.time", return_value=NOW)):
                    http.return_value.getresponse.side_effect = self.responses
                    status, _, errors = self.run_operator(*self.transport_args)
                self.assertEqual(status, 1)
                self.assertFalse(self.approval.exists())
                self.assertNotIn("test-signature", errors)
                if failure in {"misbound-viewer", "wrong-policy"}:
                    http.assert_not_called()

    def test_customer_prefix_does_not_bypass_release_fixture_isolation(self):
        with TemporaryDirectory() as directory:
            self.configure(Path(directory), "cloud-cdn")
            path = Path(directory) / "binding.json"
            raw = json.dumps(self.config | {"deployment_policy_sha256": "a" * 64}).encode()
            path.write_bytes(raw)
            with self.assertRaisesRegex(ValueError, "two distinct qualification"):
                TRANSPORT.approved_fixture(path, hashlib.sha256(raw).hexdigest())

    def test_bootstrap_only_signs_after_policy_verification(self):
        for approved in (True, False):
            with self.subTest(approved=approved), TemporaryDirectory() as directory:
                root = Path(directory)
                self.configure(root, "cloud-cdn")
                secret_path = root / "signing-key"
                viewer = {"Cookie": self.viewer.read_text()}
                arguments = [*self.transport_args[:4], "--signing-key-file", str(secret_path),
                             "--signing-key-id", "test-key", *self.transport_args[-2:]]
                observed = self.policy if approved else self.policy | {"deployment_policy_sha256": "f" * 64}
                with (patch.object(OPERATOR, "observe_deployment_policy", return_value=observed) as inspect,
                      patch.object(OPERATOR, "sign_probe", return_value=viewer) as sign,
                      patch.object(TRANSPORT.http.client, "HTTPSConnection") as http,
                      patch("cdn_viewer_issuer.time.time", return_value=NOW)):
                    http.return_value.getresponse.side_effect = self.responses

                    def sign_after_inspection(*_args):
                        self.assertEqual(inspect.call_count, 1)
                        http.assert_not_called()
                        return viewer

                    sign.side_effect = sign_after_inspection
                    status, _, errors = self.run_operator(*arguments)
                if approved:
                    self.assertEqual(status, 0, errors)
                    sign.assert_called_once_with(self.config, "cloud-cdn", REPO,
                                                 f"{REPO}/v2/pack-layers/", secret_path, "test-key")
                else:
                    self.assertEqual(status, 1)
                    sign.assert_not_called()
                    http.assert_not_called()

    def test_operator_requires_a_canonical_prefix_and_an_isolated_denial_probe(self):
        for repo, error in (("org/../other", "canonical repository prefix"),
                            ("crab-cdn-qualification", "separate qualification namespace")):
            with self.subTest(repo=repo), TemporaryDirectory() as directory:
                self.configure(Path(directory), "cloud-cdn", fixture_prefix=repo)
                with patch.object(OPERATOR, "observe_deployment_policy") as inspect:
                    status, _, errors = self.run_operator()
                self.assertEqual(status, 1)
                self.assertIn(error, errors)
                inspect.assert_not_called()
