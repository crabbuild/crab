"""Verify wire observations, fixture isolation and redacted trace output."""

from __future__ import annotations

import base64
import hashlib
import json
import sys
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from http.client import HTTPMessage
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import Mock, patch
from urllib.parse import urlsplit

import collect_cdn_key_rotation as ROTATION
import collect_cdn_scoped_view as SCOPED
import run_cdn_transport_checks as TRANSPORT
from cdn_qualification import CDN_PROVENANCE_FIELDS, deployment_component_digest, deployment_component_names, deployment_policy_digest


BODY = b"sample"
DIGEST = hashlib.sha256(BODY).hexdigest()
REPO = "crab-cdn-qualification/current"
NEIGHBOR = "crab-cdn-qualification/neighbor"
PACK = "v2/pack-layers/aa/" + ("a" * 64)
CONTROL_BODY = b"authorized control"
CONTROL_DIGEST = hashlib.sha256(CONTROL_BODY).hexdigest()
CONTROL = {
    "object_path": f"lfs/objects/{CONTROL_DIGEST[:2]}/{CONTROL_DIGEST[2:4]}/{CONTROL_DIGEST}",
    "size": len(CONTROL_BODY), "sha256": CONTROL_DIGEST,
}
S3_SIGNATURE = ("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=test%2Fcredential&X-Amz-Date=20260101T000000Z"
                "&X-Amz-Expires=120&X-Amz-SignedHeaders=host&X-Amz-Signature=test-signature&X-Amz-Security-Token=test-token")
GCS_SIGNATURE = S3_SIGNATURE.split("&X-Amz-Security-Token")[0].replace("X-Amz-", "X-Goog-").replace(
    "AWS4-HMAC-SHA256", "GOOG4-RSA-SHA256",
)
BLOB_SIGNATURE = "sv=2024-11-04&se=2026-01-01T01%3A00%3A00Z&sr=b&sp=r&sig=test-signature%2Bencoded"


def fixture(edge: str = "cloudfront", access: str = "public") -> dict:
    origin = {
        "cloudfront": "s3://fixture-bucket",
        "cloud-cdn": "gs://fixture-bucket",
        "front-door": "https://fixtureacct.blob.core.windows.net/container",
    }[edge]
    sample = {
        "scope": f"{edge}/{access}/cli/git/direct", "base_url": "https://fixture.example.com/crab",
        "native_origin": origin,
        "fixture_prefix": REPO, "neighbor_prefix": NEIGHBOR, "view_suffix": "",
        "object_path": PACK, "size": len(BODY), "sha256": DIGEST, "deployment_policy_sha256": "d" * 64,
    }
    if access == "public":
        sample["export_origin"] = origin.replace("fixture", "export")
        sample["source_placement"] = {
            "cloudfront": {"resource": "accounts/210987654321/buckets/fixture-bucket", "location": "us-east-1"},
            "cloud-cdn": {"resource": "projects/123455/buckets/fixture-bucket", "location": "US"},
            "front-door": {"resource": "/subscriptions/12345678-1234-1234-1234-123456789012/resourcegroups/live/"
                                       "providers/microsoft.storage/storageaccounts/fixtureacct/"
                                       "blobservices/default/containers/container", "location": "eastus"},
        }[edge]
    return sample


def response(status: int, body: bytes = b"", **headers: str) -> Mock:
    result = Mock(status=status, headers=HTTPMessage())
    for name, value in headers.items():
        result.headers[name.replace("_", "-")] = value
    result.getheader.side_effect = result.headers.get
    result.read.return_value = body
    return result


def route_responses(edge: str, hot: Mock, head_status: int = 403) -> list[Mock]:
    return [hot, response(head_status), *(response(403) for _ in TRANSPORT.EDGE_ROUTE_QUERY_PROBES[edge]), hot]


def expiring_viewer(edge: str, expiry: int) -> dict[str, str]:
    if edge == "cloudfront":
        claims = {"Statement": [{"Condition": {"DateLessThan": {"AWS:EpochTime": expiry}}}]}
        policy = base64.b64encode(json.dumps(claims).encode()).decode().translate(str.maketrans("+=/", "-_~"))
        return {"Cookie": f"CloudFront-Policy={policy}; CloudFront-Signature=fixture-secret; CloudFront-Key-Pair-Id=test"}
    if edge == "cloud-cdn":
        return {"Cookie": f"Cloud-CDN-Cookie=URLPrefix=dGVzdA==:Expires={expiry}:KeyName=test:Signature=fixture-secret"}
    payload = base64.urlsafe_b64encode(json.dumps({"exp": expiry}).encode()).decode().rstrip("=")
    return {"Authorization": f"Bearer header.{payload}.fixture-secret"}


def scoped_credential(edge: str, config: dict, prefix: str, expires: int) -> str:
    base = config["base_url"]
    resource = f"{base}/{prefix}"
    if edge == "cloudfront":
        policy = {"Statement": [{"Resource": resource + "*", "Condition": {"DateLessThan": {"AWS:EpochTime": expires}}}]}
        cookie = base64.b64encode(json.dumps(policy).encode()).decode().translate(str.maketrans("+=/", "-_~"))
        return f"CloudFront-Policy={cookie}; CloudFront-Signature=fixture-signature; CloudFront-Key-Pair-Id=fixture"
    if edge == "cloud-cdn":
        policy = base64.urlsafe_b64encode(resource.encode()).decode()
        return f"Cloud-CDN-Cookie=URLPrefix={policy}:Expires={expires}:KeyName=fixture:Signature=fixture-signature"
    claims = {"iss": base, "aud": base, "prefix": prefix, "exp": expires,
              "sub": config["fixture_prefix"] + config["view_suffix"], "origin": config["native_origin"]}
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    return f"header.{payload}.fixture-signature"


class CdnTransportQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        ctx = patch.object(TRANSPORT.http.client, "HTTPSConnection")
        self.client = ctx.start()
        self.addCleanup(ctx.stop)
        self.connection = self.client.return_value

    def test_scoped_view_requires_live_sibling_control_on_all_providers(self) -> None:
        for edge, header, hit, endpoint, query in (
            ("cloudfront", "X_Cache", "Hit from cloudfront", "s3.us-east-1.amazonaws.com/fixture-bucket", S3_SIGNATURE),
            ("cloud-cdn", "X_Crab_CDN_Cache", "hit", "storage.googleapis.com/fixture-bucket", GCS_SIGNATURE),
            ("front-door", "X_Cache", "TCP_HIT", "fixtureacct.blob.core.windows.net/container", BLOB_SIGNATURE),
        ):
            with self.subTest(edge=edge), TemporaryDirectory() as temporary:
                root = Path(temporary)
                target_suffix = f"/acl-views/v2/{'a' * 64}/1-{'b' * 64}"
                sibling_suffix = f"/acl-views/v2/{'a' * 64}/2-{'c' * 64}"
                config = fixture(edge, "private") | {"view_suffix": target_suffix}
                raw = json.dumps(config).encode()
                (root / "fixture.json").write_bytes(raw)
                sibling = {"view_suffix": sibling_suffix, "object_path": PACK,
                           "size": len(CONTROL_BODY), "sha256": CONTROL_DIGEST}
                (root / "sibling.json").write_text(json.dumps(sibling), encoding="utf-8")
                fingerprint = hashlib.sha256(raw).hexdigest()
                repo = config["fixture_prefix"]
                expiry = int(time.time()) + 120
                grant = f"{repo}{target_suffix}/v2/pack-layers/"
                base = config["base_url"]
                resource = f"{base}/{grant}"
                if edge == "cloudfront":
                    policy = {"Statement": [{"Resource": resource + "*",
                                             "Condition": {"DateLessThan": {"AWS:EpochTime": expiry}}}]}
                    encoded = base64.b64encode(json.dumps(policy).encode()).decode().translate(str.maketrans("+=/", "-_~"))
                    credential = f"CloudFront-Policy={encoded}; CloudFront-Signature=fixture-secret; CloudFront-Key-Pair-Id=test"
                elif edge == "cloud-cdn":
                    encoded = base64.urlsafe_b64encode(resource.encode()).decode()
                    credential = f"Cloud-CDN-Cookie=URLPrefix={encoded}:Expires={expiry}:KeyName=test:Signature=fixture-secret"
                else:
                    claims = {"iss": base, "aud": base, "sub": repo + target_suffix, "prefix": grant,
                              "origin": config["native_origin"], "exp": expiry}
                    encoded = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
                    credential = f"header.{encoded}.fixture-secret"
                (root / "viewer").write_text(credential, encoding="utf-8")
                signed = root / "signed"
                signed.write_text(f"https://{endpoint}/{repo}{sibling_suffix}/{PACK}?{query}", encoding="utf-8")
                hot = response(200, BODY, Cache_Control="private, no-store", **{header: hit})
                control = response(200, CONTROL_BODY)
                native_discovery = [response(403, x_amz_bucket_region="us-east-1")] if edge == "cloudfront" else []
                self.connection.getresponse.side_effect = native_discovery + [hot, hot, control, response(403), response(401), control, hot]
                self.connection.request.reset_mock()
                args = ["probe", "--fixture", str(root / "fixture.json"), "--configuration-sha256", fingerprint,
                        "--sibling", str(root / "sibling.json")]
                captured = StringIO()
                with patch.object(sys, "argv", args), redirect_stdout(captured):
                    self.assertEqual(SCOPED.main(), 0)
                digest = json.loads(captured.getvalue())["scoped_view_sha256"]
                self.connection.request.assert_not_called()
                output = root / "trace.json"
                with (patch.object(sys, "argv", args + ["--output", str(output), "--scoped-view-sha256", digest,
                        "--row-configuration-sha256", "9" * 64, "--viewer-file", str(root / "viewer"),
                        "--sibling-read-url-file", str(signed), "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "1"]),
                      patch.object(SCOPED, "attest_fixture") as attest):
                    self.assertEqual(SCOPED.main(), 0)
                self.assertEqual(attest.call_count, 2)
                trace = json.loads(output.read_bytes())
                self.assertEqual(trace["configuration_sha256"], "9" * 64)
                self.assertEqual(trace["placement"]["fixture_sha256"], fingerprint)
                sibling_requests = [call for call in self.connection.request.call_args_list if sibling_suffix in call.args[1]]
                self.assertEqual([call.kwargs["headers"].get("Range") for call in sibling_requests], [None, None, "bytes=0-0", None])
                self.assertEqual(["?" in call.args[1] for call in sibling_requests], [True, False, False, True])
                viewer_header = "Authorization" if edge == "front-door" else "Cookie"
                self.assertEqual([viewer_header in call.kwargs["headers"] for call in sibling_requests], [False, True, True, False])
                for secret in ("fixture-secret", "test-signature", repo, config["base_url"]):
                    self.assertNotIn(secret, output.read_text(encoding="utf-8"))

    def test_rotation_reads_the_same_grants_on_both_sides_of_retirement(self) -> None:
        headers = {
            "cloudfront": ("X_Cache", "Hit from cloudfront"),
            "cloud-cdn": ("X_Crab_CDN_Cache", "hit"),
            "front-door": ("X_Cache", "TCP_HIT"),
        }
        expiry = 900
        for edge, (header, hit) in headers.items():
            with self.subTest(edge=edge):
                config = fixture(edge, "private")
                provenance = dict.fromkeys(CDN_PROVENANCE_FIELDS, "test") | {"scope": config["scope"]}
                retired = expiring_viewer(edge, expiry)
                if edge == "front-door":
                    payload = retired["Authorization"].split(".")[1]
                    retired, replacement = (
                        {"Authorization": f"Bearer {base64.urlsafe_b64encode(json.dumps({'kid': key}).encode()).decode().rstrip('=')}"
                                          f".{payload}.fixture-secret"}
                        for key in ("test", "new")
                    )
                else:
                    replacement = {"Cookie": retired["Cookie"].replace("=test", "=new")}
                before = dict.fromkeys(deployment_component_names(edge, "private"), "e" * 64)
                before["viewer-key-ids"] = deployment_component_digest(["new", "test"])
                after = before | {"viewer-key-ids": deployment_component_digest(["new"])}
                overlap_digest = deployment_policy_digest(edge, "private", before)
                config["deployment_policy_sha256"] = deployment_policy_digest(edge, "private", after)
                warm = response(200, BODY, Cache_Control="private, no-store", **{header: hit})
                self.connection.getresponse.side_effect = [warm, warm, warm, response(403), response(403), warm]
                self.connection.request.reset_mock()
                retire = Mock()
                with (patch.object(ROTATION.time, "time", return_value=1),
                      patch.object(ROTATION, "now_us", side_effect=range(1, 16)),
                      patch.object(ROTATION, "attest_fixture", side_effect=[
                          {"components": before}, {"components": after}, {"components": after},
                      ]) as attest):
                    trace = ROTATION.collect_rotation(config, provenance, overlap_digest, retired, replacement, retire)
                retire.assert_called_once_with(expiry)
                self.assertEqual([call.args[0]["deployment_policy_sha256"] for call in attest.call_args_list],
                                 [overlap_digest, config["deployment_policy_sha256"], config["deployment_policy_sha256"]])
                requests = [call.kwargs["headers"] for call in self.connection.request.call_args_list]
                self.assertEqual(requests[0], requests[3])
                self.assertEqual(requests[4], requests[3] | {"Range": "bytes=0-0"})
                self.assertNotIn("fixture-secret", json.dumps(trace))

    def test_all_provider_cache_headers_are_normalized_and_denial_bodies_are_untouched(self) -> None:
        providers = {
            "cloudfront": ("X_Cache", "Hit from cloudfront", "Miss from cloudfront"),
            "cloud-cdn": ("X_Crab_CDN_Cache", "hit", "miss"),
            "front-door": ("X_Cache", "TCP_REMOTE_HIT", "TCP_MISS"),
        }
        for edge, (name, hit, miss) in providers.items():
            with self.subTest(edge=edge):
                self.connection.reset_mock()
                denied = response(403)
                self.connection.getresponse.side_effect = [
                    response(200, BODY, **{name: miss}),
                    response(206, BODY[:1], Content_Range="bytes 0-0/6", **{name: hit}),
                    denied, denied,
                ]
                with (patch.object(TRANSPORT, "collect_origin_access", return_value={}),
                      patch.object(TRANSPORT, "collect_edge_route_policy", return_value=[])):
                    traces = TRANSPORT.collect(fixture(edge), edge, "public", "git", {}, None)
                self.assertEqual(traces["exact_get"][0]["cache"], "miss")
                self.assertEqual(traces[TRANSPORT.PUBLIC_EXPORT_CHECK], [traces["exact_get"][0] | {"body_sha256": DIGEST}])
                self.assertEqual(traces["exact_range"][0]["cache"], "hit")
                self.assertEqual(traces["neighbor_route_denied"][0]["status"], 403)
                denied.read.assert_not_called()
                paths = [call.args[:2] for call in self.connection.request.call_args_list]
                self.assertEqual(paths, [
                    ("GET", f"/crab/{REPO}/{PACK}"), ("GET", f"/crab/{REPO}/{PACK}"),
                    ("GET", f"/crab/{REPO}/v2/root"), ("GET", f"/crab/{NEIGHBOR}/{PACK}"),
                ])

    def test_expired_viewer_replays_the_same_warm_request(self) -> None:
        hot = response(200, BODY, X_Cache="Hit from cloudfront", Cache_Control="private, no-store")
        self.connection.getresponse.side_effect = [
            response(200, BODY, Cache_Control="private, no-store"),
            response(206, BODY[:1], Content_Range="bytes 0-0/6", Cache_Control="private, no-store"),
            response(403), response(403),
            *route_responses("cloudfront", hot),
            response(403), response(403),
            hot, hot,
            response(403),
            response(403, X_Amz_Bucket_Region="us-west-2"), response(403),
        ]
        with (patch.object(TRANSPORT.time, "time", return_value=100), patch.object(TRANSPORT.time, "sleep") as wait,
              patch.object(TRANSPORT.time, "time_ns", side_effect=[
                  100_000_000_000, 100_001_000_000, 112_000_000_000, 112_001_000_000,
              ]),
              patch.object(TRANSPORT, "parallel_range_samples", side_effect=[[{"before": 1}], [{"after": 2}]]) as ranges,
              patch.object(TRANSPORT, "verify_expiry_range_window") as verify):
            traces = TRANSPORT.collect(
                fixture(access="private"), "cloudfront", "private", "git", expiring_viewer("cloudfront", 110), 110,
            )
        wait.assert_called_once_with(12)
        self.assertEqual([call.kwargs for call in ranges.call_args_list], [{"expired": False}, {"expired": True}])
        verify.assert_called_once_with(traces[TRANSPORT.EXPIRY_RANGE_CHECK], 110)
        warm, rejected = traces["warm_expired_viewer_denied"]
        self.assertEqual(warm["key_sha256"], rejected["key_sha256"])
        self.assertEqual((warm["cache"], rejected["status"]), ("hit", 403))
        before, after = self.connection.request.call_args_list[-4:-2]
        self.assertEqual(before, after)

    def test_expiry_probe_requires_the_actual_grant_timestamp(self) -> None:
        for edge in TRANSPORT.PROVIDER_FOR_EDGE:
            with self.subTest(edge=edge), patch.object(TRANSPORT.time, "time", return_value=100):
                viewer = expiring_viewer(edge, 111)
                with self.assertRaisesRegex(ValueError, "expiry argument differs from the viewer grant"):
                    TRANSPORT.collect(fixture(edge, "private"), edge, "private", "git", viewer, 110)
                broken = dict.fromkeys(viewer, "malformed-fixture-secret")
                with self.assertRaisesRegex(ValueError, "readable signed expiration") as error:
                    TRANSPORT.collect(fixture(edge, "private"), edge, "private", "git", broken, 110)
                self.assertNotIn("fixture-secret", str(error.exception))
        self.client.assert_not_called()

    def test_anonymous_get_and_range_are_denied_without_avoiding_the_warm_cache(self) -> None:
        hot = response(200, BODY, X_Crab_CDN_Cache="hit", Cache_Control="private, no-store")
        denied = response(403)
        self.connection.getresponse.side_effect = [
            response(200, BODY, Cache_Control="private, no-store"),
            response(206, BODY[:1], Content_Range="bytes 0-0/6", Cache_Control="private, no-store"),
            denied, denied, *route_responses("cloud-cdn", hot), denied, denied, hot, denied,
        ]
        traces = TRANSPORT.collect(fixture("cloud-cdn", "private"), "cloud-cdn", "private", "git",
                                   {"Cookie": "valid-viewer"}, None)
        warm = traces["warm_anonymous_viewer_denied"]
        self.assertEqual([item["status"] for item in warm], [200, 403, 403, 200])
        self.assertEqual({item["key_sha256"] for item in warm}, {traces["exact_get"][0]["key_sha256"]})
        self.assertEqual([call.kwargs["headers"] for call in self.connection.request.call_args_list[-5:-1]], [
            {"Accept-Encoding": "identity", "Cookie": "valid-viewer"},
            {"Accept-Encoding": "identity"},
            {"Accept-Encoding": "identity", "Range": "bytes=0-0"},
            {"Accept-Encoding": "identity", "Cookie": "valid-viewer"},
        ])
        denied.read.assert_not_called()

    def test_anonymous_range_success_or_lost_warm_entry_fails_the_probe(self) -> None:
        hot = response(200, BODY, X_Cache="TCP_HIT", Cache_Control="private, no-store")
        denied = response(403)
        anonymous_bytes = response(206, BODY[:1], Content_Range="bytes 0-0/6")
        miss = response(200, BODY, X_Cache="TCP_MISS", Cache_Control="private, no-store")
        for responses, error in (([anonymous_bytes], "expected denial, got HTTP 206"),
                                 ([denied, miss], "requires a cache hit")):
            with self.subTest(error=error):
                self.connection.getresponse.side_effect = [
                    hot, response(206, BODY[:1], Content_Range="bytes 0-0/6", Cache_Control="private, no-store"),
                    denied, denied, *route_responses("front-door", hot), denied, *responses,
                ]
                with self.assertRaisesRegex(ValueError, error):
                    TRANSPORT.collect(fixture("front-door", "private"), "front-door", "private", "git",
                                      {"Authorization": "Bearer valid-viewer"}, None)
        anonymous_bytes.read.assert_not_called()

    def test_warm_object_denies_head_and_provider_query_variants(self) -> None:
        providers = (
            ("cloudfront", "public", "X_Cache", "Hit from cloudfront", 403, {}),
            ("cloud-cdn", "public", "X_Crab_CDN_Cache", "hit", 405, {}),
            ("front-door", "private", "X_Cache", "TCP_HIT", 404, {"Authorization": "Bearer test-viewer"}),
        )
        key = f"{REPO}/{PACK}"
        for edge, access, header, hit, head_status, viewer in providers:
            with self.subTest(edge=edge):
                self.connection.reset_mock()
                hot = response(200, BODY, Cache_Control="private, no-store", **{header: hit})
                replies = route_responses(edge, hot, head_status)
                self.connection.getresponse.side_effect = replies
                base = TRANSPORT.fixture_base("https://fixture.example.com/crab")
                observed = TRANSPORT.collect_edge_route_policy(base, edge, access, key, viewer, BODY)
                calls = self.connection.request.call_args_list
                self.assertEqual(calls[1].args, ("HEAD", f"/crab/{key}"))
                self.assertEqual([call.args for call in calls[2:-1]], [
                    ("GET", f"/crab/{key}{query}") for query in TRANSPORT.EDGE_ROUTE_QUERY_PROBES[edge].values()
                ])
                self.assertTrue(all(call.kwargs["headers"] == viewer | {"Accept-Encoding": "identity"} for call in calls))
                self.assertEqual([observed[0]["cache"], observed[-1]["cache"]], ["hit", "hit"])
                for rejected in replies[1:-1]:
                    rejected.read.assert_not_called()

    def test_query_missing_or_allowed_is_not_a_policy_denial(self) -> None:
        hot = response(200, BODY, X_Cache="TCP_HIT", Cache_Control="private, no-store")
        base = TRANSPORT.fixture_base("https://fixture.example.com/crab")
        for status in (200, 404):
            with self.subTest(status=status):
                unexpected = response(status, BODY)
                self.connection.getresponse.side_effect = [hot, response(404), unexpected]
                with self.assertRaisesRegex(ValueError, f"edge query policy expected denial, got HTTP {status}"):
                    TRANSPORT.collect_edge_route_policy(base, "front-door", "private", f"{REPO}/{PACK}",
                                                        {"Authorization": "Bearer test-viewer"}, BODY)
                unexpected.read.assert_not_called()

    def test_accepts_only_byte_exact_representations(self) -> None:
        for result, reason in (
            (response(206, BODY[:1], Content_Range="bytes 1-1/6"), "wrong range"),
            (response(206, b"?", Content_Range="bytes 0-0/6"), "approved bytes"),
            (response(206, BODY[:1], Content_Range="bytes 0-0/6", Content_Encoding="gzip"), "transformed"),
            (response(302), "expected HTTP 206"),
        ):
            with self.subTest(reason=reason):
                self.connection.getresponse.side_effect = [response(200, BODY), result]
                with self.assertRaisesRegex(ValueError, reason):
                    TRANSPORT.collect(fixture(), "cloudfront", "public", "git", {}, None)

    def test_refuses_unscoped_global_keys_and_unsafe_paths(self) -> None:
        path = ".crab/xorbs/aa/" + ("a" * 64)
        with self.assertRaisesRegex(ValueError, "protected qualification view"):
            TRANSPORT.fixture_repository_prefix(fixture(access="private") | {"object_path": path}, "private", "xorb")
        for relative in ("../" + PACK, PACK.replace("aa/", "bb/"), PACK + "/extra"):
            with self.subTest(relative=relative), self.assertRaisesRegex(ValueError, "immutable read family"):
                TRANSPORT.fixture_repository_prefix(fixture() | {"object_path": relative}, "public", "git")
        self.client.assert_not_called()

    def test_public_export_requires_distinct_storage_on_the_same_cloud(self) -> None:
        sample = fixture("front-door")
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "fixture.json"
            sibling = sample["native_origin"].rsplit("/", 1)[0] + "/export"
            for invalid in (sample["native_origin"], sibling, "gs://export-bucket"):
                with self.subTest(invalid=invalid):
                    raw = json.dumps(sample | {"export_origin": invalid}).encode()
                    path.write_bytes(raw)
                    with self.assertRaises(ValueError):
                        TRANSPORT.approved_fixture(path, hashlib.sha256(raw).hexdigest())
            self.client.assert_not_called()

    def test_public_origin_policy_uses_exact_cloud_endpoints_without_credentials(self) -> None:
        key = f"{REPO}/{PACK}"
        examples = {
            "cloudfront": (
                [response(403, X_Amz_Bucket_Region="us-west-2"), response(403),
                 response(301, X_Amz_Bucket_Region="eu-west-1"), response(403), response(403)],
                ["s3.amazonaws.com", "s3.us-west-2.amazonaws.com", "s3.amazonaws.com",
                 "s3.eu-west-1.amazonaws.com", "s3.eu-west-1.amazonaws.com"],
                [("HEAD", "/fixture-bucket"), ("GET", f"/fixture-bucket/{key}"),
                 ("HEAD", "/export-bucket"), ("GET", f"/export-bucket/{key}"),
                 ("GET", "/export-bucket?list-type=2&max-keys=1")],
                403,
            ),
            "cloud-cdn": (
                [response(403), response(200, BODY), response(403)], ["storage.googleapis.com"] * 3,
                [("GET", f"/fixture-bucket/{key}"), ("GET", f"/export-bucket/{key}"),
                 ("GET", "/export-bucket?max-keys=1")],
                200,
            ),
            "front-door": (
                [response(401), response(200, BODY), response(401)],
                ["fixtureacct.blob.core.windows.net", "exportacct.blob.core.windows.net", "exportacct.blob.core.windows.net"],
                [("GET", f"/container/{key}"), ("GET", f"/container/{key}"),
                 ("GET", "/container?restype=container&comp=list&maxresults=1")],
                200,
            ),
        }
        for edge, (replies, hosts, requests, export_status) in examples.items():
            with self.subTest(edge=edge):
                self.connection.reset_mock()
                self.client.reset_mock()
                self.connection.getresponse.side_effect = replies
                traces = TRANSPORT.collect_origin_access(fixture(edge), edge, "public", key, BODY)
                self.assertEqual([call.args[0] for call in self.client.call_args_list], hosts)
                self.assertEqual([call.args for call in self.connection.request.call_args_list], requests)
                for request in self.connection.request.call_args_list:
                    headers = {name.lower(): value for name, value in request.kwargs["headers"].items()}
                    self.assertTrue({"authorization", "cookie"}.isdisjoint(headers))
                    if edge == "front-door":
                        self.assertEqual(headers["x-ms-version"], "2019-12-12")
                self.assertEqual(traces["export_origin_access"][0]["status"], export_status)
                self.assertEqual(traces["public_listing_denied"][0]["bytes"], 0)
                replies[-1].read.assert_not_called()

    def test_origin_probe_rejects_redirects_unexpected_disclosure_and_wrong_bytes(self) -> None:
        key = f"{REPO}/{PACK}"
        for replies, reason in (
            ([response(302)], "live_origin_denied expected denial"),
            ([response(200, BODY)], "live_origin_denied expected denial"),
            ([response(403), response(200, b"wrong!")], "anonymous export origin returned different object bytes"),
            ([response(403), response(200, BODY), response(200, b"listed")], "public_listing_denied expected denial"),
        ):
            with self.subTest(reason=reason):
                self.connection.getresponse.side_effect = replies
                with self.assertRaisesRegex(ValueError, reason):
                    TRANSPORT.collect_origin_access(fixture("cloud-cdn"), "cloud-cdn", "public", key, BODY)
                if not reason.startswith("anonymous export"):
                    replies[-1].read.assert_not_called()

    def test_s3_region_discovery_cannot_redirect_the_origin_probe(self) -> None:
        self.connection.getresponse.return_value = response(
            301, X_Amz_Bucket_Region="us-gov-west-1", Location="https://unapproved.example.com/fixture-bucket",
        )
        with self.assertRaisesRegex(ValueError, "did not report a commercial bucket region"):
            TRANSPORT.collect_origin_access(fixture("cloudfront"), "cloudfront", "public", f"{REPO}/{PACK}", BODY)
        self.client.assert_called_once_with("s3.amazonaws.com", None, timeout=15)

    def test_signed_native_read_matches_the_fixture_endpoint_and_preserves_the_signature(self) -> None:
        examples = (
            ("cloudfront", "https://s3.us-west-2.amazonaws.com/fixture-bucket",
             "https://s3.us-west-2.amazonaws.com/fixture-bucket", S3_SIGNATURE),
            ("cloudfront", "https://s3.us-east-1.amazonaws.com/fixture-bucket",
             "https://fixture-bucket.s3.amazonaws.com", S3_SIGNATURE),
            ("cloud-cdn", "https://storage.googleapis.com/fixture-bucket",
             "https://fixture-bucket.storage.googleapis.com", GCS_SIGNATURE),
            ("front-door", "https://fixtureacct.blob.core.windows.net/container",
             "https://fixtureacct.blob.core.windows.net/container", BLOB_SIGNATURE),
        )
        key = f"{REPO}/{PACK}"
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "signed-url"
            for edge, native, target, signature in examples:
                with self.subTest(edge=edge, target=target):
                    path.write_bytes(f"{target}/{key}?{signature}\r\n".encode())
                    signed = TRANSPORT.signed_native_target(path.read_bytes(), urlsplit(native), edge, key)
                    self.assertEqual(signed._replace(query="").geturl(), target)
                    self.assertEqual(signed.query, signature)

    def test_signed_native_read_cannot_leave_the_approved_origin_key_or_api(self) -> None:
        edge = "front-door"
        key = f"{REPO}/{PACK}"
        base = fixture(edge)["native_origin"]
        target = f"{base}/{key}?{BLOB_SIGNATURE}"
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "signed-url"
            for invalid in (
                target.replace("fixtureacct", "exportacct"), target.replace(REPO, NEIGHBOR),
                target.replace("https://", "https://user@"), target + "&snapshot=previous",
                target + "&sig=duplicate", target.replace("sp=r", "sp=rw"), target.replace("sr=b", "sr=c"),
            ):
                with self.subTest(invalid=invalid):
                    path.write_text(invalid, encoding="ascii")
                    with self.assertRaises(ValueError) as caught:
                        TRANSPORT.signed_native_target(path.read_bytes(), urlsplit(base), edge, key)
                    self.assertNotIn("test-signature", str(caught.exception))
        self.client.assert_not_called()

    def test_authenticated_origin_rejects_different_bytes(self) -> None:
        key = f"{REPO}/{PACK}"
        sample = fixture("cloud-cdn", "private")
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "signed-url"
            path.write_text(f"https://storage.googleapis.com/fixture-bucket/{key}?{GCS_SIGNATURE}", encoding="ascii")
            self.connection.getresponse.side_effect = [response(403), response(200, b"wrong!")]
            with self.assertRaisesRegex(ValueError, "authenticated native origin returned different object bytes"):
                TRANSPORT.collect_origin_access(sample, "cloud-cdn", "private", key, BODY, native_read_url_file=path)

    def test_native_identity_reads_exact_fixture_without_sending_credentials_to_export(self) -> None:
        import collect_cdn_fixture_cleanup as providers

        key = f"{REPO}/{PACK}"
        token = "fixture-provider-token"
        endpoints = {
            "cloudfront": ("s3.us-west-2.amazonaws.com", "fixture-bucket"),
            "cloud-cdn": ("storage.googleapis.com", "fixture-bucket"),
            "front-door": ("fixtureacct.blob.core.windows.net", "container"),
        }
        for edge, (host, prefix) in endpoints.items():
            with self.subTest(edge=edge):
                self.connection.reset_mock()
                self.client.reset_mock()
                discovery = response(403, X_Amz_Bucket_Region="us-west-2")
                if edge == "cloudfront":
                    replies = [discovery, response(403), response(200, BODY), discovery, response(403), response(403)]
                    credential = f"https://{host}/{prefix}/{key}?{S3_SIGNATURE}\n".encode()
                else:
                    replies = [response(403), response(200, BODY), response(200, BODY), response(403)]
                    credential = token.encode() + b"\n"
                self.connection.getresponse.side_effect = replies
                sample = fixture(edge, "public")
                with patch.object(providers, "command_output", return_value=credential) as cli:
                    traces = TRANSPORT.collect_origin_access(sample, edge, "public", key, BODY, native_identity_read=True)
                command = cli.call_args.args
                if edge == "cloudfront":
                    self.assertEqual(command[:4], ("aws", "s3", "presign", f"s3://fixture-bucket/{key}"))
                    self.assertEqual(command[command.index("--region") + 1], "us-west-2")
                elif edge == "cloud-cdn":
                    self.assertEqual(command, ("gcloud", "auth", "print-access-token", "--quiet"))
                else:
                    self.assertEqual(command[command.index("--resource") + 1], "https://storage.azure.com/")
                observed = traces["native_origin_read"][0]
                self.assertEqual(observed["bytes"], len(BODY))
                self.assertEqual(observed["key_sha256"], traces["live_origin_denied"][0]["key_sha256"])
                encoded = json.dumps(traces)
                self.assertNotIn(token, encoded)
                self.assertNotIn("test-signature", encoded)
                requests = self.connection.request.call_args_list
                index = 2 if edge == "cloudfront" else 1
                signed = f"?{S3_SIGNATURE}" if edge == "cloudfront" else ""
                self.assertEqual(requests[index].args, ("GET", f"/{prefix}/{key}{signed}"))
                self.assertEqual(self.client.call_args_list[index].args[:1], (host,))
                for offset, call in enumerate(requests):
                    headers = call.kwargs["headers"]
                    expected = f"Bearer {token}" if offset == index and edge != "cloudfront" else None
                    self.assertEqual(headers.get("Authorization"), expected)
                    self.assertNotIn("Cookie", headers)
                    if offset == index and edge == "front-door":
                        self.assertIn("x-ms-date", headers)

    def test_native_token_cannot_be_sent_to_edge_or_unapproved_endpoint(self) -> None:
        for base, edge in (("https://fixture.example.com/crab", "cloud-cdn"), ("https://fixture.example.com", None)):
            with self.subTest(base=base), self.assertRaisesRegex(ValueError, "native credentials require"):
                TRANSPORT.request_sample(urlsplit(base), edge, f"{REPO}/{PACK}", {}, size=len(BODY), wanted=200,
                                         check="native_origin_read", origin_authorization="Bearer fixture-provider-token")
        self.client.assert_not_called()

    def test_private_output_is_redacted_and_binds_the_approved_config(self) -> None:
        hot = response(200, BODY, Cache_Control="private, no-store", X_Cache="TCP_HIT")
        control = response(200, CONTROL_BODY, Cache_Control="private, no-store")
        denied = response(403)
        self.connection.getresponse.side_effect = [
            response(200, BODY, Cache_Control="private, no-store"),
            response(206, BODY[:1], Content_Range="bytes 0-0/6", Cache_Control="private, no-store"),
            response(403), response(403),
            *route_responses("front-door", hot, 404),
            response(401), response(403),
            hot,
            control, denied, denied, control, hot,
            response(401), response(200, BODY),
        ]
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = fixture("front-door", "private") | {"unrelated_control": CONTROL}
            raw = json.dumps(config).encode()
            (root / "fixture.json").write_bytes(raw)
            (root / "token").write_text("fixture-secret\n", encoding="utf-8")
            expiry = int(time.time()) + 120
            unrelated_credential = scoped_credential("front-door", config, f"{REPO}/lfs/objects/", expiry)
            (root / "unrelated").write_text(unrelated_credential + "\n", encoding="utf-8")
            signed = root / "signed-url"
            signed.write_text(f"https://fixtureacct.blob.core.windows.net/container/{REPO}/{PACK}?{BLOB_SIGNATURE}")
            digest = hashlib.sha256(raw).hexdigest()
            argv = [
                "probe", "--fixture", str(root / "fixture.json"), "--trace-directory", str(root / "traces"),
                "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2",
                "--viewer-file", str(root / "token"), "--unrelated-viewer-file", str(root / "unrelated"),
                "--native-read-url-file", str(signed), "--configuration-sha256",
            ]
            with patch.object(sys, "argv", argv + ["e" * 64]), redirect_stderr(StringIO()):
                self.assertEqual(TRANSPORT.main(), 1)
            error = StringIO()
            with (patch.object(sys, "argv", argv + [digest, "--provider", "s3"]),
                  redirect_stderr(error), patch.object(TRANSPORT, "viewer_headers") as read_viewer):
                self.assertEqual(TRANSPORT.main(), 1)
            self.assertIn("differs from the selected native provider", error.getvalue())
            read_viewer.assert_not_called()
            self.client.assert_not_called()
            with patch.object(sys, "argv", argv + [digest, "--provider", "azure"]), redirect_stdout(StringIO()):
                self.assertEqual(TRANSPORT.main(), 0)

            for item in (root / "traces").iterdir():
                text = item.read_text(encoding="utf-8")
                self.assertNotIn("fixture-secret", text)
                self.assertNotIn(unrelated_credential, text)
                self.assertNotIn("test-signature", text)
                self.assertNotIn("fixture.example.com", text)
                self.assertNotIn("fixtureacct.blob.core.windows.net", text)
                self.assertNotIn(REPO, text)
                trace = json.loads(text)
                self.assertEqual(trace["configuration_sha256"], digest)
                self.assertEqual(trace["check"], item.stem)
            self.assertTrue((root / "traces" / "private_origin_denied.json").is_file())
            read = json.loads((root / "traces" / "native_origin_read.json").read_bytes())["requests"][0]
            self.assertEqual(read["bytes"], len(BODY))
            native = self.connection.request.call_args_list[-1]
            self.assertEqual(native.args, ("GET", f"/container/{REPO}/{PACK}?{BLOB_SIGNATURE}"))
            self.assertNotIn("Authorization", native.kwargs["headers"])
            headers = self.connection.request.call_args_list[0].kwargs["headers"]
            self.assertEqual(headers["Authorization"], "Bearer fixture-secret")
            observed = json.loads((root / "traces" / "warm_unrelated_principal_denied.json").read_bytes())["requests"]
            self.assertEqual([row["status"] for row in observed], [200, 200, 403, 403, 200, 200])
            self.assertEqual({row["viewer_expires_at_unix"] for row in observed[1:5]}, {expiry})
            self.assertEqual([row["body_sha256"] for row in observed[1:5]], [CONTROL_DIGEST, None, None, CONTROL_DIGEST])
            control_path, target = f"/crab/{REPO}/{CONTROL['object_path']}", f"/crab/{REPO}/{PACK}"
            unrelated = [call for call in self.connection.request.call_args_list
                         if call.kwargs["headers"].get("Authorization") == f"Bearer {unrelated_credential}"]
            self.assertEqual([call.args for call in unrelated], [("GET", path) for path in
                                                                (control_path, target, target, control_path)])
            self.assertEqual([call.kwargs["headers"].get("Range") for call in unrelated], [None, None, "bytes=0-0", None])
            denied.read.assert_not_called()

    def test_unrelated_control_cannot_share_the_same_grant_prefix(self) -> None:
        primary = fixture("cloud-cdn", "private")
        history = "v2/history/aa/" + ("a" * 64)
        same_prefix = {"object_path": "v2/history/source-roots/bb/" + ("b" * 64), "size": 6, "sha256": DIGEST}
        sample = primary | {"object_path": history, "unrelated_control": same_prefix}
        with self.assertRaisesRegex(ValueError, "disjoint immutable grant prefix"):
            TRANSPORT.fixture_repository_prefix(sample, "private", "capsule")

    def test_unrelated_grant_must_be_live_and_scoped_to_the_control_before_reading(self) -> None:
        for edge in TRANSPORT.PROVIDER_FOR_EDGE:
            config = fixture(edge, "private") | {"unrelated_control": CONTROL}
            now = int(time.time())
            for prefix, expiry, reason in (
                (f"{REPO}/v2/pack-layers/", now + 120, "signed viewer policy differs"),
                (f"{REPO}/lfs/objects/", now - 1, "must be live and short-lived"),
            ):
                grant = TRANSPORT.private_viewer_headers(edge, scoped_credential(edge, config, prefix, expiry))
                with self.subTest(edge=edge, reason=reason), self.assertRaisesRegex(ValueError, reason):
                    TRANSPORT.collect(config, edge, "private", "git", {"Cookie": "primary"}, None, unrelated_viewer=grant)
        self.client.assert_not_called()

    def test_attestation_brackets_the_transport_in_one_output_directory(self) -> None:
        import collect_cloudfront_deployment_policy
        from cdn_deployment_policy_cli import PolicyObservation
        from cdn_qualification import verify_deployment_policy_trace

        components = dict.fromkeys(deployment_component_names("cloudfront", "public"), "d" * 64)
        policy_digest = deployment_policy_digest("cloudfront", "public", components)
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "fixture.json"
            sample = fixture("cloudfront", "public") | {
                "deployment": {"distribution_id": "EDIST", "region": "us-east-1"}, "deployment_policy_sha256": policy_digest,
            }
            raw = json.dumps(sample).encode()
            path.write_bytes(raw)
            digest = hashlib.sha256(raw).hexdigest()
            for changed in (False, True):
                with self.subTest(changed=changed):
                    output = root / f"traces-{changed}"
                    order = []

                    def attest(config, *, distribution_id, profile, region):
                        self.assertEqual((config, distribution_id, profile, region), (sample, "EDIST", None, "us-east-1"))
                        order.append("attest")
                        if changed and len(order) == 3:
                            return PolicyObservation(components | {"viewer-function": "a" * 64})
                        return PolicyObservation(components)

                    def collect(*_args, **_kwargs):
                        order.append("probe")
                        return {"exact_get": [{"source": "edge", "status": 200}]}

                    args = ["probe", "--fixture", str(path), "--configuration-sha256", digest,
                            "--trace-directory", str(output), "--source-sha", "a" * 40, "--run-id", "123",
                            "--run-attempt", "2", "--attest-deployment"]
                    with (patch.object(sys, "argv", args), patch.object(collect_cloudfront_deployment_policy, "collect", side_effect=attest),
                          patch.object(TRANSPORT, "collect", side_effect=collect), redirect_stdout(StringIO()), redirect_stderr(StringIO()) as error):
                        status = TRANSPORT.main()
                    self.assertEqual(order, ["attest", "probe", "attest"])
                    if changed:
                        self.assertEqual(status, 1)
                        self.assertIn("differs from the approved fixture", error.getvalue())
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(status, 0)
                        policy = json.loads((output / "deployment_policy.json").read_bytes())
                        probe = json.loads((output / "exact_get.json").read_bytes())
                        verify_deployment_policy_trace(policy, probe, "cloudfront", "public", policy_digest)

    def test_private_binding_candidate_requires_unchanged_placement_and_native_read(self) -> None:
        import collect_cloudfront_deployment_policy
        from cdn_deployment_policy_cli import PolicyObservation, PrivateBinding

        binding = PrivateBinding("arn:aws:cloudfront::123456789012:distribution/EDIST",
                                 "accounts/123456789012/buckets/fixture-bucket", "us-east-1")
        sample = fixture("cloudfront", "private") | {
            "deployment": {"distribution_id": "EDIST", "region": "us-east-1"},
        }
        names = deployment_component_names("cloudfront", "private") - {"private-binding"}
        observation = PolicyObservation(dict.fromkeys(names, "d" * 64), binding)
        components = observation.bound_components(sample)
        sample["deployment_policy_sha256"] = deployment_policy_digest("cloudfront", "private", components)
        changed = PolicyObservation(observation.components,
                                    PrivateBinding(binding.deployment, binding.native_resource, "us-west-2"))
        key = hashlib.sha256(f"{REPO}/{PACK}".encode()).hexdigest()
        edge_read = {"source": "edge", "status": 200, "bytes": len(BODY), "key_sha256": key}
        native_read = edge_read | {"source": "origin"}
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "fixture.json"
            raw = json.dumps(sample).encode()
            source.write_bytes(raw)
            for case, snapshot, native in (
                ("matched", observation, [native_read]),
                ("changed", changed, [native_read]),
                ("unverified", observation, []),
                ("wrong-key", observation, [native_read | {"key_sha256": "0" * 64}]),
            ):
                with self.subTest(case=case):
                    trace = root / f"trace-{case}"
                    candidate = root / f"candidate-{case}.json"
                    args = ["probe", "--fixture", str(source), "--configuration-sha256", hashlib.sha256(raw).hexdigest(),
                            "--trace-directory", str(trace), "--source-sha", "a" * 40, "--run-id", "123",
                            "--run-attempt", "2", "--attest-deployment", "--native-identity-read",
                            "--private-binding-candidate", str(candidate)]
                    with (patch.object(sys, "argv", args),
                          patch.object(collect_cloudfront_deployment_policy, "collect", side_effect=[observation, snapshot]),
                          patch.object(TRANSPORT, "viewer_headers", return_value={"Cookie": "viewer-test-secret"}),
                          patch.object(TRANSPORT, "collect", return_value={"exact_get": [edge_read], "native_origin_read": native}),
                          redirect_stdout(StringIO()), redirect_stderr(StringIO())):
                        self.assertEqual(TRANSPORT.main(), 0 if case == "matched" else 1)
                    if case == "matched":
                        record = json.loads(candidate.read_text())
                        self.assertEqual(record["schema"], "crab.cdn-private-binding-candidate/v1")
                        self.assertEqual(record["binding"], binding.route(sample))
                        self.assertEqual(record["object"], {"key_sha256": key, "body_sha256": DIGEST, "bytes": len(BODY)})
                        self.assertNotIn("private_binding", json.loads((trace / "deployment_policy.json").read_text()))
                        self.assertNotIn("viewer-test-secret", candidate.read_text())
                    else:
                        self.assertFalse(candidate.exists())
                        self.assertFalse(trace.exists())

    def test_unrelated_grant_with_wrong_control_bytes_cannot_prove_denial(self) -> None:
        self.connection.getresponse.return_value = response(
            200, b"?" * len(CONTROL_BODY), Cache_Control="private, no-store",
        )
        base = TRANSPORT.fixture_base("https://fixture.example.com/crab")
        with self.assertRaisesRegex(ValueError, "unrelated viewer control differs from the approved bytes"):
            TRANSPORT.unrelated_viewer_samples(base, "cloudfront", REPO, f"{REPO}/{PACK}", len(BODY),
                                               CONTROL, {"Cookie": "test-viewer"}, int(time.time()) + 120)


if __name__ == "__main__":
    unittest.main()
