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
import collect_cdn_cache_reuse as REUSE
import run_cdn_transport_checks as TRANSPORT
from cdn_qualification import verify_front_door_cache_reuse
from test_cdn_transport_qualification import BODY, fixture, response, scoped_credential


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
    config = fixture(edge, "private") | {"viewer_issuer": {
        "endpoint": ISSUER_URL, "audience": AUDIENCE, "approval_sha256": "a" * 64,
    }}
    if edge == "front-door":
        config["deployment"] = {"subscription": "12345678-1234-1234-1234-123456789012",
                                "resource_group": "test", "profile": "test-cdn"}
    if protected:
        config["view_suffix"] = f"/acl-views/v2/{'a' * 64}/1-{'b' * 64}"
        config["scope"] = f"{edge}/private/cli/xorb/direct"
        config["object_path"] = ".crab/xorbs/aa/" + "a" * 64
    return config


def azure_viewer_archive(config, warm_cache="HIT"):
    if not config["scope"].startswith("front-door/"):
        return []
    resource, _, _ = REUSE.front_door_resources(config)
    endpoint = "fixture-0123456789abcdef.z01.azurefd.net"
    key = config["fixture_prefix"] + config["view_suffix"] + "/" + config["object_path"]
    common = {"category": "FrontDoorAccessLog", "resourceId": resource}
    viewer = {"endpoint": endpoint, "requestUri": f"https://{endpoint}/{key}", "httpMethod": "GET",
              "httpStatusCode": 200, "responseBytes": len(BODY), "isReceivedFromClient": True, "pop": "SJC"}
    return [{"records": [
        common | {"properties": viewer | {"trackingReference": "primary-viewer-ref", "cacheStatus": "MISS"}},
        common | {"properties": viewer | {"trackingReference": "second-viewer-ref", "cacheStatus": warm_cache}},
        common | {"properties": viewer | {"trackingReference": "second-viewer-ref", "cacheStatus": "HIT",
                                           "isReceivedFromClient": False, "pop": "SEA"}},
    ]}]


def issue_reply(edge, config, *, expires_in=180):
    provider, cdn = ISSUER.PROVIDERS[edge]
    repo = config["fixture_prefix"] + config["view_suffix"]
    family = ".crab/xorbs" if config["view_suffix"] else "v2/pack-layers"
    prefix = f"{repo}/{family}/"
    expires = NOW + expires_in
    reply = {"provider": provider, "credentials": {"access_token": "native-credential-fixture"},
             "permissions": ["read"], "expires_at": datetime.fromtimestamp(NOW + 300, timezone.utc).isoformat(),
             "cdn": {"provider": cdn, "origin": config["native_origin"], "base_url": config["base_url"],
                     "repo_prefix": repo, "expires_at": expires,
                     "grants": [{"prefix": prefix, "credential": scoped_credential(edge, config, prefix, expires)}]}}
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
                    response(200, json.dumps(reply).encode(), Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="a" * 64),
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
                self.assertEqual(exchange.kwargs["headers"][ISSUER.APPROVAL_HEADER], "a" * 64)
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

    def test_cache_reuse_mints_both_grants_and_keeps_oidc_tokens_off_the_edge(self):
        for edge, header, miss, hit in (
            ("cloudfront", "X_Cache", "Miss from cloudfront", "Hit from cloudfront"),
            ("cloud-cdn", "X_Crab_CDN_Cache", "miss", "hit"),
            ("front-door", "X_Cache", "TCP_MISS", "TCP_HIT"),
        ):
            with self.subTest(edge=edge), TemporaryDirectory() as directory:
                root = Path(directory)
                protected = edge == "front-door"
                config = settings(edge, protected=protected)
                raw = json.dumps(config).encode()
                (root / "fixture.json").write_bytes(raw)
                roles = ("primary", "second")
                issuer = "https://idp.example.com"
                subjects = {role: {"issuer": issuer, "subject": f"{role}-subject", "policy_identity": f"{role}@example.com"}
                            for role in roles}
                principals = root / "principals.json"
                principals.write_text(json.dumps(subjects), encoding="utf-8")
                tokens = {role: f"header.{encoded({'iss': issuer, 'sub': spec['subject'], 'email': spec['policy_identity'], 'aud': AUDIENCE})}.token"
                          for role, spec in subjects.items()}
                for role, token in tokens.items():
                    (root / role).write_text(token + "\n", encoding="utf-8")
                args = ["probe", "--fixture", str(root / "fixture.json"), "--configuration-sha256", hashlib.sha256(raw).hexdigest(),
                        "--principals", str(principals)]
                stdout = StringIO()
                with patch.object(sys, "argv", args), redirect_stdout(stdout):
                    self.assertEqual(REUSE.main(), 0)
                fingerprint = json.loads(stdout.getvalue())["cache_reuse_sha256"]
                reply = issue_reply(edge, config)
                issued = response(200, json.dumps(reply).encode(), Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="a" * 64)
                second = response(200, json.dumps(issue_reply(edge, config, expires_in=181)).encode(),
                                  Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="a" * 64)
                cold_timing, warm_timing = {}, {}
                if edge == "cloudfront":
                    cold_timing["Server_Timing"] = 'cdn-cache-miss,cdn-pop;desc="PHX50-C2"'
                    warm_timing["Server_Timing"] = 'cdn-cache-hit,cdn-pop;desc="PHX50-C2",cdn-hit-layer;desc="EDGE"'
                elif edge == "front-door":
                    cold_timing["X_Azure_Ref"] = "primary-viewer-ref"
                    warm_timing["X_Azure_Ref"] = "second-viewer-ref"
                self.connection.getresponse.side_effect = [
                    issued, response(200, BODY, Cache_Control="private, no-store", **{header: miss}, **cold_timing),
                    second, response(200, BODY, Cache_Control="private, no-store", **{header: hit}, **warm_timing),
                ]
                self.http.reset_mock()
                output = root / "trace.json"
                args += ["--cache-reuse-sha256", fingerprint, "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2",
                         "--primary-token-file", str(root / "primary"), "--second-token-file", str(root / "second"),
                         "--output", str(output)]
                reused = [str(root / "primary") if value == str(root / "second") else value for value in args]
                error = StringIO()
                with (patch.object(sys, "argv", reused), patch.object(REUSE, "attest_fixture") as attest,
                      redirect_stderr(error)):
                    self.assertEqual(REUSE.main(), 1)
                self.assertIn("different approved cache reuse principal", error.getvalue())
                self.assertFalse(output.exists())
                attest.assert_not_called()
                self.http.assert_not_called()
                with (patch.object(sys, "argv", args), patch.object(REUSE, "attest_fixture") as attest,
                      patch.object(REUSE.time, "sleep"),
                      patch.object(REUSE, "azure_monitor_tokens", return_value=("arm", "storage")),
                      patch.object(REUSE, "azure_log_archives", side_effect=[[], azure_viewer_archive(config)]) as logs,
                      patch.object(REUSE.time, "time_ns", side_effect=[NOW * 10**9 + i * 10**6 for i in range(6)])):
                    self.assertEqual(REUSE.main(), 0)
                self.assertEqual(attest.call_count, 2)
                self.assertEqual(logs.call_count, 2 if edge == "front-door" else 0)
                calls = self.connection.request.call_args_list
                for role, grant, read in zip(roles, calls[::2], calls[1::2], strict=True):
                    self.assertEqual(grant.args, ("POST", "/v1/credentials"))
                    self.assertEqual(json.loads(grant.kwargs["body"])["id_token"], tokens[role])
                    self.assertEqual(grant.kwargs["headers"]["Authorization"], f"Bearer {tokens[role]}")
                    self.assertNotIn(tokens[role], str(read.kwargs["headers"]))
                    if edge == "cloudfront":
                        self.assertEqual(read.kwargs["headers"].get("Pragma"), "server-timing")
                self.assertEqual(calls[1].args, calls[3].args)
                trace = output.read_text()
                for secret in (*tokens.values(), "primary@example.com", "native-credential-fixture", "fixture-signature"):
                    self.assertNotIn(secret, trace)
                self.assertEqual([row["cache"] for row in json.loads(trace)["requests"]], ["miss", "hit"])
                if edge == "cloudfront":
                    reads = json.loads(trace)["requests"]
                    self.assertEqual({row["cloudfront_pop_sha256"] for row in reads}, {hashlib.sha256(b"PHX50-C2").hexdigest()})
                    self.assertEqual([row["cloudfront_hit_layer"] for row in reads], [None, "EDGE"])
                    self.assertNotIn("PHX50-C2", trace)
                elif edge == "front-door":
                    retained = json.loads(trace)["front_door_viewer_logs"]
                    self.assertEqual({row["pop_sha256"] for row in retained.values()}, {hashlib.sha256(b"SJC").hexdigest()})
                    for raw_field in ("primary-viewer-ref", "second-viewer-ref", "SJC"):
                        self.assertNotIn(raw_field, trace)

    def test_cloudfront_cache_reuse_requires_one_point_of_presence(self):
        for timing, error in (
            ("cdn-cache-hit", "requires an observed point of presence"),
            ('cdn-pop;desc="PHX50-C2",cdn-pop;desc="SEA19-C1"', "invalid or repeated CloudFront cache timing metric"),
        ):
            with self.subTest(timing=timing), self.assertRaisesRegex(ValueError, error):
                TRANSPORT.cloudfront_cache_timing(response(200, Server_Timing=timing))

    def test_front_door_cache_reuse_rejects_a_remote_hit(self):
        config = settings("front-door")
        key = config["fixture_prefix"] + "/" + config["object_path"]
        requests = [{"front_door_request_sha256": hashlib.sha256(f"{role}-viewer-ref".encode()).hexdigest(),
                     "status": 200, "cache": cache, "key_sha256": hashlib.sha256(key.encode()).hexdigest(),
                     "bytes": len(BODY), "started_at_unix_ms": 1000, "finished_at_unix_ms": 1050}
                    for role, cache in (("primary", "miss"), ("second", "hit"))]
        with (patch.object(REUSE, "azure_monitor_tokens", return_value=("arm", "storage")),
              patch.object(REUSE, "azure_log_archives", return_value=azure_viewer_archive(config, "REMOTE_HIT"))):
            logs = REUSE.azure_reuse_viewer_logs(config, requests)
        with self.assertRaisesRegex(ValueError, "disagrees with the verified miss or edge hit"):
            verify_front_door_cache_reuse(logs, requests)

    def test_cache_reuse_cannot_relabel_one_policy_identity_as_two_principals(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            raw = json.dumps(settings("cloudfront")).encode()
            (root / "fixture.json").write_bytes(raw)
            principal = {"issuer": "https://idp.example.com", "subject": "first", "policy_identity": "reader@example.com"}
            (root / "principals.json").write_text(json.dumps({
                "primary": principal, "second": principal | {"subject": "different", "policy_identity": "Reader@example.com"},
            }))
            error = StringIO()
            with (patch.object(sys, "argv", ["probe", "--fixture", str(root / "fixture.json"), "--configuration-sha256",
                                            hashlib.sha256(raw).hexdigest(), "--principals", str(root / "principals.json")]),
                  redirect_stderr(error)):
                self.assertEqual(REUSE.main(), 1)
            self.assertIn("two distinct OIDC subjects and Crab Auth policy identities", error.getvalue())
            self.http.assert_not_called()

    def test_hosted_cache_reuse_passes_only_the_redacted_cold_read_between_jobs(self):
        for edge, header, miss, hit in (
            ("cloudfront", "X_Cache", "Miss from cloudfront", "Hit from cloudfront"),
            ("cloud-cdn", "X_Crab_CDN_Cache", "miss", "hit"),
            ("front-door", "X_Cache", "TCP_MISS", "TCP_HIT"),
        ):
            with self.subTest(edge=edge), TemporaryDirectory() as directory:
                self.http.reset_mock()
                root = Path(directory)
                config = settings(edge)
                raw = json.dumps(config).encode()
                (root / "fixture.json").write_bytes(raw)
                issuer = "https://token.actions.githubusercontent.com"
                subjects = {role: f"repo:org/crab:environment:test-{role}" for role in ("primary", "second")}
                principals = {role: {"issuer": issuer, "subject": sub, "policy_identity": sub}
                              for role, sub in subjects.items()}
                (root / "principals.json").write_text(json.dumps(principals), encoding="utf-8")
                tokens = {role: f"header.{encoded({'iss': issuer, 'sub': sub, 'aud': AUDIENCE})}.token"
                          for role, sub in subjects.items()}
                args = ["probe", "--fixture", str(root / "fixture.json"), "--configuration-sha256",
                        hashlib.sha256(raw).hexdigest(), "--principals", str(root / "principals.json")]
                with patch.object(sys, "argv", args), redirect_stdout(StringIO()) as inspection:
                    self.assertEqual(REUSE.main(), 0)
                args += ["--cache-reuse-sha256", json.loads(inspection.getvalue())["cache_reuse_sha256"],
                         "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2"]
                primary = root / "primary.json"
                output = root / "authorized_cache_reuse.json"
                for role, cache in (("primary", miss), ("second", hit)):
                    reply = issue_reply(edge, config, expires_in=180 if role == "primary" else 181)
                    timing = {}
                    if edge == "cloudfront":
                        timing["Server_Timing"] = 'cdn-pop;desc="PHX50-C2"'
                        if role == "second":
                            timing["Server_Timing"] += ',cdn-hit-layer;desc="EDGE"'
                    elif edge == "front-door":
                        timing["X_Azure_Ref"] = f"{role}-viewer-ref"
                    self.connection.getresponse.side_effect = [
                        response(200, json.dumps({"value": tokens[role]}).encode()),
                        response(200, json.dumps(reply).encode(),
                                 Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="a" * 64),
                        response(200, BODY, Cache_Control="private, no-store", **{header: cache}, **timing),
                    ]
                    selected = args + ["--github-role", role, "--output", str(primary if role == "primary" else output)]
                    if role == "second":
                        selected += ["--primary-observation", str(primary)]
                        for field, value in (("workflow_run_attempt", "1"), ("runner_sha256", "1" * 64),
                                             ("authority_sha256", "2" * 64)):
                            original = primary.read_text()
                            primary.write_text(json.dumps(json.loads(original) | {field: value}))
                            with (patch.object(sys, "argv", selected), patch.dict(ISSUER.os.environ, {"RUNNER_NAME": "worker"}),
                                  patch.object(REUSE, "github_id_token") as oidc, redirect_stderr(StringIO()) as error):
                                self.assertEqual(REUSE.main(), 1)
                            self.assertIn("approved fixture, runner, and attempt", error.getvalue())
                            oidc.assert_not_called()
                            primary.write_text(original)
                    with (patch.object(sys, "argv", selected), patch.dict(ISSUER.os.environ, {"RUNNER_NAME": "worker"}),
                          patch.object(REUSE, "attest_fixture") as attest,
                          patch.object(REUSE, "azure_monitor_tokens", return_value=("arm", "storage")),
                          patch.object(REUSE, "azure_log_archives", return_value=azure_viewer_archive(config)) as logs,
                          patch.object(REUSE.time, "time_ns", side_effect=[
                              NOW * 10**9 + (offset + (0 if role == "primary" else 10)) * 10**6 for offset in range(3)
                          ])):
                        self.assertEqual(REUSE.main(), 0)
                    self.assertEqual(attest.call_count, 2)
                    self.assertEqual(logs.call_count, int(edge == "front-door" and role == "second"))
                trace = json.loads(output.read_text())
                self.assertEqual([row["cache"] for row in trace["requests"]], ["miss", "hit"])
                for path in (primary, output):
                    for secret in (*tokens.values(), *subjects.values(), "native-credential-fixture", "fixture-signature"):
                        self.assertNotIn(secret, path.read_text())

    def test_issuer_rejection_and_nonprivate_responses_are_redacted(self):
        cases = (
            (response(302, b"unexpected-secret", Location="https://other.example.com"), "HTTP 302"),
            (response(200, b"unexpected-secret"), "must require no-store"),
            (response(200, b"unexpected-secret", Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="a" * 64), "invalid JSON"),
            (response(200, b"x" * (ISSUER.MAX_ISSUER_RESPONSE + 1), Cache_Control="no-store",
                      X_Crab_CDN_Approval_Sha256="a" * 64), "response is too large"),
            (response(200, b"unexpected-secret", Cache_Control="no-store", X_Crab_CDN_Approval_Sha256="b" * 64),
             "different private binding approval"),
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
                        grant["credential"] = scoped_credential(edge, config, grant["prefix"].replace("xorbs", "shards"), NOW + 180)
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
        primary = {"Cookie": "private-viewer-fixture"}
        expiry = {"Cookie": "separate-expiring-viewer-fixture"}
        order.mint.side_effect = [primary, expiry]
        order.probe.return_value = {"exact_get": [{"source": "edge", "status": 200}]}
        order.expiry.return_value = {check: [] for check in TRANSPORT.EXPIRY_CHECKS}
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
                  patch.object(TRANSPORT.time, "time", side_effect=[100, 102, 102]),
                  patch.object(TRANSPORT, "viewer_expiry", return_value=160) as signed_expiry,
                  patch.object(TRANSPORT, "collect_expiry_window", order.expiry),
                  redirect_stdout(StringIO())):
                self.assertEqual(TRANSPORT.main(), 0)
            self.assertEqual([call[0] for call in order.mock_calls], ["attest", "mint", "probe", "mint", "expiry", "attest"])
            self.assertEqual(order.probe.call_args.args[4], primary)
            self.assertEqual(order.expiry.call_args.args[3:], (expiry, 160))
            signed_expiry.assert_called_once_with("cloudfront", expiry)
            for record in output.glob("*.json"):
                encoded = record.read_text()
                self.assertNotIn(primary["Cookie"], encoded)
                self.assertNotIn(expiry["Cookie"], encoded)
                if record.stem in TRANSPORT.EXPIRY_CHECKS:
                    trace = json.loads(encoded)
                    self.assertEqual(trace["viewer_expires_at_unix"], 160)
                    self.assertEqual(trace["viewer_grant_sha256"], hashlib.sha256(json.dumps(expiry).encode()).hexdigest())

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
