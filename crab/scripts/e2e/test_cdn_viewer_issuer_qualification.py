"""Exercise the hosted issuer exchange and its credential boundaries."""

from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import TestCase
from unittest.mock import Mock, patch
from urllib.parse import parse_qs, urlsplit
import base64
import hashlib
import json
import sys

import cdn_deployment_policy_cli as DEPLOYMENT
import cdn_viewer_issuer as ISSUER
import run_cdn_transport_checks as TRANSPORT
from test_cdn_transport_qualification import fixture, response


NOW = 1_800_000_000
RUNTIME = "https://pipelines-test.actions.githubusercontent.com/id/job?api-version=2.0"
ISSUER_URL = "https://issuer.example.com/v1/credentials"
AUDIENCE = "urn:crab:qualification"
OIDC_TOKEN = "oidc.jwt.fixture"
RUNTIME_TOKEN = "runtime-credential-fixture"
ENVIRONMENT = {"ACTIONS_ID_TOKEN_REQUEST_URL": RUNTIME, "ACTIONS_ID_TOKEN_REQUEST_TOKEN": RUNTIME_TOKEN}


def encoded(value):
    return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")


def settings(edge, *, protected=False):
    config = fixture(edge, "private") | {"viewer_issuer": {"endpoint": ISSUER_URL, "audience": AUDIENCE}}
    if protected:
        config["view_suffix"] = f"/acl-views/v2/{'a' * 64}/1-{'b' * 64}"
        config["scope"] = f"{edge}/private/cli/xorb/direct"
        config["object_path"] = ".crab/xorbs/aa/" + "a" * 64
    return config


def credential(edge, config, prefix, expires):
    resource = f"{config['base_url']}/{prefix}"
    if edge == "cloudfront":
        policy = {"Statement": [{"Resource": resource + "*", "Condition": {"DateLessThan": {"AWS:EpochTime": expires}}}]}
        cookie = base64.b64encode(json.dumps(policy).encode()).decode().translate(str.maketrans("+=/", "-_~"))
        return f"CloudFront-Policy={cookie}; CloudFront-Signature=fixture-signature; CloudFront-Key-Pair-Id=fixture"
    if edge == "cloud-cdn":
        policy = base64.urlsafe_b64encode(resource.encode()).decode()
        return f"Cloud-CDN-Cookie=URLPrefix={policy}:Expires={expires}:KeyName=fixture:Signature=fixture-signature"
    payload = encoded({"iss": config["base_url"], "aud": config["base_url"], "prefix": prefix, "exp": expires,
                       "sub": config["fixture_prefix"] + config["view_suffix"], "origin": config["native_origin"]})
    return f"header.{payload}.fixture-signature"


def issue_reply(edge, config):
    provider, cdn = ISSUER.PROVIDERS[edge]
    repo = config["fixture_prefix"] + config["view_suffix"]
    family = ".crab/xorbs" if config["view_suffix"] else "v2/pack-layers"
    prefix = f"{repo}/{family}/"
    expires = NOW + 180
    reply = {"provider": provider, "credentials": {"access_token": "native-credential-fixture"},
             "permissions": ["read"], "expires_at": datetime.fromtimestamp(NOW + 300, timezone.utc).isoformat(),
             "cdn": {"provider": cdn, "origin": config["native_origin"], "base_url": config["base_url"],
                     "repo_prefix": repo, "expires_at": expires,
                     "grants": [{"prefix": prefix, "credential": credential(edge, config, prefix, expires)}]}}
    if config["view_suffix"]:
        reply["cdn"]["global_prefix"] = repo + "/.crab"
        reply["storage_scope"] = {"repo_prefix": repo, "global_prefix": repo + "/.crab",
                                  "source_repo": config["fixture_prefix"], "scope_hash": "a" * 64}
    return reply


class CdnViewerIssuerQualificationTests(TestCase):
    def setUp(self):
        self.http = self.enterContext(patch.object(TRANSPORT.http.client, "HTTPSConnection"))
        self.connection = self.http.return_value
        self.enterContext(patch.dict(ISSUER.os.environ, ENVIRONMENT))
        self.enterContext(patch.object(ISSUER.time, "time", return_value=NOW))

    def test_mints_only_the_approved_provider_viewer(self):
        for edge in ISSUER.PROVIDERS:
            with self.subTest(edge=edge):
                self.http.reset_mock()
                protected = edge == "front-door"
                config = settings(edge, protected=protected)
                reply = issue_reply(edge, config)
                self.connection.getresponse.side_effect = [
                    response(200, json.dumps({"value": OIDC_TOKEN}).encode()),
                    response(200, json.dumps(reply).encode(), Cache_Control="no-store"),
                ]
                viewer = ISSUER.issue_github_viewer(config, edge, "xorb" if protected else "git")
                credential_value = reply["cdn"]["grants"][0]["credential"]
                expected = ({"Authorization": f"Bearer {credential_value}"} if protected else {"Cookie": credential_value})
                self.assertEqual(viewer, expected)
                self.assertEqual([call.args[0] for call in self.http.call_args_list if call.args],
                                 ["pipelines-test.actions.githubusercontent.com", "issuer.example.com"])
                request, exchange = self.connection.request.call_args_list
                self.assertEqual(request.args[0], "GET")
                self.assertEqual(parse_qs(urlsplit(request.args[1]).query), {"api-version": ["2.0"], "audience": [AUDIENCE]})
                self.assertEqual(request.kwargs["headers"]["Authorization"], f"Bearer {RUNTIME_TOKEN}")
                self.assertEqual(exchange.args, ("POST", "/v1/credentials"))
                self.assertEqual(exchange.kwargs["headers"]["Authorization"], f"Bearer {OIDC_TOKEN}")
                self.assertEqual(exchange.kwargs["headers"]["X-Crab-CDN-Grant-Version"], "1")
                self.assertEqual(json.loads(exchange.kwargs["body"]), {
                    "id_token": OIDC_TOKEN, "repo_url": TRANSPORT.native_remote(config, edge),
                    "operation": "fetch", "client_version": "provider-cdn-qualification",
                })

    def test_runtime_token_is_never_sent_to_an_unapproved_url(self):
        for runtime in (
            "http://pipelines-test.actions.githubusercontent.com/id?api-version=2.0",
            "https://actions.githubusercontent.com.other.example.com/id?api-version=2.0",
            "https://other.example.com@pipelines-test.actions.githubusercontent.com/id?api-version=2.0",
            RUNTIME + "&audi%65nce=wrong",
        ):
            with (self.subTest(runtime=runtime), patch.dict(ISSUER.os.environ, {"ACTIONS_ID_TOKEN_REQUEST_URL": runtime}),
                  self.assertRaisesRegex(ValueError, "GitHub OIDC")):
                ISSUER.github_id_token(AUDIENCE)
        self.http.assert_not_called()

    def test_issuer_rejection_and_nonprivate_responses_are_redacted(self):
        cases = (
            (response(302, b"unexpected-secret", Location="https://other.example.com"), "HTTP 302"),
            (response(200, b"unexpected-secret"), "must require no-store"),
            (response(200, b"unexpected-secret", Cache_Control="no-store"), "invalid JSON"),
            (response(200, b"x" * (ISSUER.MAX_ISSUER_RESPONSE + 1), Cache_Control="no-store"), "response is too large"),
        )
        for rejected, error in cases:
            with self.subTest(error=error):
                self.connection.getresponse.side_effect = [response(200, json.dumps({"value": OIDC_TOKEN}).encode()), rejected]
                with self.assertRaisesRegex(ValueError, error) as failure:
                    ISSUER.issue_github_viewer(settings("cloudfront"), "cloudfront", "git")
                self.assertNotIn("unexpected-secret", str(failure.exception))
        self.assertEqual(self.http.call_count, 2 * len(cases))
        cases[0][0].read.assert_not_called()

    def test_viewer_claims_and_permissions_must_match_the_exact_fixture(self):
        for edge in ISSUER.PROVIDERS:
            config = settings(edge, protected=True)
            for mismatch, error in (
                ("signed-prefix", "signed viewer policy differs"),
                ("native-target", "differs from the approved origin"),
                ("protected-view", "different protected view"),
                ("parent-expiration", "bounded by its parent"),
                ("permissions", "different provider or read permission"),
                ("duplicate", "one grant for the approved object family"),
            ):
                with self.subTest(edge=edge, mismatch=mismatch):
                    reply = issue_reply(edge, config)
                    grant = reply["cdn"]["grants"][0]
                    if mismatch == "signed-prefix":
                        grant["credential"] = credential(edge, config, grant["prefix"].replace("xorbs", "shards"), NOW + 180)
                    elif mismatch == "native-target":
                        reply["cdn"]["origin"] = "wrong-origin"
                    elif mismatch == "protected-view":
                        reply["storage_scope"]["repo_prefix"] += "/sibling"
                    elif mismatch == "parent-expiration":
                        reply["expires_at"] = datetime.fromtimestamp(NOW + 60, timezone.utc).isoformat()
                    elif mismatch == "permissions":
                        reply["permissions"].append("write")
                    else:
                        reply["cdn"]["grants"].append(grant.copy())
                    with self.assertRaisesRegex(ValueError, error):
                        ISSUER.approved_credential(config, edge, "xorb", reply)

    def test_cloudfront_rejects_additional_cookies_before_edge_delivery(self):
        config = settings("cloudfront")
        reply = issue_reply("cloudfront", config)
        reply["cdn"]["grants"][0]["credential"] += "; Unexpected=other-credential"
        with self.assertRaisesRegex(ValueError, "invalid CloudFront cookie set"):
            ISSUER.approved_credential(config, "cloudfront", "git", reply)

    def test_hosted_primary_mints_after_preflight_and_serializes_no_secrets(self):
        config = settings("cloudfront")
        order = Mock()
        order.attest.return_value = {"schema": "test-policy"}
        order.mint.return_value = {"Cookie": "private-viewer-fixture"}
        order.probe.return_value = {"exact_get": [{"source": "edge", "status": 200}]}
        with TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "fixture.json"
            output = root / "traces"
            raw = json.dumps(config).encode()
            path.write_bytes(raw)
            args = ["probe", "--fixture", str(path), "--configuration-sha256", hashlib.sha256(raw).hexdigest(),
                    "--trace-directory", str(output), "--source-sha", "a" * 40, "--run-id", "123",
                    "--run-attempt", "2", "--attest-deployment", "--github-viewer"]
            with (patch.object(sys, "argv", args), patch.object(DEPLOYMENT, "attest_fixture", order.attest),
                  patch.object(TRANSPORT, "collect", order.probe), patch.object(ISSUER, "issue_github_viewer", order.mint),
                  redirect_stdout(StringIO())):
                self.assertEqual(TRANSPORT.main(), 0)
            self.assertEqual([call[0] for call in order.mock_calls], ["attest", "mint", "probe", "attest"])
            self.assertEqual(order.probe.call_args.args[4], order.mint.return_value)
            self.assertNotIn("private-viewer-fixture", (output / "exact_get.json").read_text())

    def test_mixed_hosted_and_primary_file_inputs_fail_before_attestation(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "fixture.json"
            raw = json.dumps(settings("cloudfront")).encode()
            path.write_bytes(raw)
            args = ["probe", "--fixture", str(path), "--configuration-sha256", hashlib.sha256(raw).hexdigest(),
                    "--trace-directory", str(root / "traces"), "--source-sha", "a" * 40, "--run-id", "123",
                    "--run-attempt", "2", "--attest-deployment", "--github-viewer", "--viewer-file", str(root / "viewer")]
            with (patch.object(sys, "argv", args), patch.object(DEPLOYMENT, "attest_fixture") as attest,
                  redirect_stderr(StringIO()) as error):
                self.assertEqual(TRANSPORT.main(), 1)
            attest.assert_not_called()
            self.assertIn("exclusive primary authentication", error.getvalue())
