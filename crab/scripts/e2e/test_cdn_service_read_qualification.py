"""Bind ordinary service responses to the approved payload and fresh storage observations."""

import argparse
import hashlib
import io
import json
import subprocess
import sys
import unittest
from contextlib import contextmanager
from email.message import Message
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

import collect_cdn_viewer_requests as COLLECT
import run_cdn_service_read as SERVICE
from cdn_qualification import PROVIDER_FOR_EDGE, lfs_object_path
from test_cdn_qualification import COMMIT, json_file
from test_cdn_transport_qualification import fixture


BODY = b"approved service bytes"
SHA256 = hashlib.sha256(BODY).hexdigest()
GIT_OID = hashlib.sha1(f"blob {len(BODY)}\0".encode() + BODY, usedforsecurity=False).digest()
GIT_SHA256 = hashlib.sha256(GIT_OID).hexdigest()
ETAG = '"approved-validator"'


def events(provider, consumer, family):
    route = {"provider": provider, "access": "private", "consumer": consumer, "family": family, "profile": "direct"}
    transfer = {"message": "CDN immutable GET", "status": 206, "bytes": 64, "cache": "hit", "range": True,
                "complete": True, "elapsed_ms": 1, "request_sha256": "d" * 64}
    payload = {"message": f"CDN verified {'Git' if family == 'git' else 'LFS'} payload", "source": "edge"}
    if family == "git":
        payload["git_blob_oid_sha256"] = GIT_SHA256
    return [route | transfer, route | payload]


class CdnServiceReadTests(unittest.TestCase):
    def prepare(self, root, provider, consumer, family):
        config = fixture(provider, "private") | {"scope": f"{provider}/private/{consumer}/{family}/direct"}
        if provider == "cloud-cdn":
            config["deployment"] = {"project": "fixture-project", "forwarding_rule": "crab-https"}
        if family == "lfs":
            config |= {"object_path": lfs_object_path(SHA256), "sha256": SHA256, "size": len(BODY)}
        config_sha = json_file(root / "fixture.json", config)
        repo = "org/repo" if consumer == "server" else "fixture-bucket"
        workload = {"schema": "crab.cdn-service-workload/v1", "configuration_sha256": config_sha,
                    "publication_sha256": "a" * 64, "endpoint": "https://service.example.com", "repository": repo,
                    "commit": "c" * 40, "payload_sha256": SHA256, "payload_bytes": len(BODY),
                    "etag_sha256": hashlib.sha256(ETAG.encode()).hexdigest()}
        credentials = ({"access_key": "fixture-key", "secret_key": "fixture-secret", "region": "us-east-1"}
                       if consumer == "s3-gateway" else {"cookie": "__Host-crab_session=fixture-token"}
                       if family == "git" else {"authorization": "Basic Zml4dHVyZTp0b2tlbg=="})
        json_file(root / "credentials.json", credentials)
        return argparse.Namespace(fixture=root / "fixture.json", configuration_sha256=config_sha,
            workload=root / "workload.json", workload_sha256=json_file(root / "workload.json", workload),
            credentials=root / "credentials.json", service_log=root / "server.log",
            source_sha=COMMIT, run_id="123", run_attempt="2", arm="edge")

    def request(self, args, provider, consumer, family, fault=""):
        edge = events(PROVIDER_FOR_EDGE[provider], consumer, family)
        natives = {"target": "crab_storage::native_read", "fields": {"message": "Native object store read",
                   "provider": PROVIDER_FOR_EDGE[provider], "method": "get", "status": 200, "bytes": len(BODY)}}
        prior = {"target": "crab_storage::cdn::transfer", "fields": edge[0]}
        args.service_log.write_text(json.dumps(prior) + "\n", encoding="utf-8")

        @contextmanager
        def exchange(base, method, path, headers, _label):
            self.assertEqual((base.netloc, method), ("service.example.com", "GET"))
            if consumer == "s3-gateway":
                self.assertEqual(path, f"/fixture-bucket/{'c' * 40}/{SERVICE.PAYLOAD}")
                self.assertTrue(headers["authorization"].startswith("AWS4-HMAC-SHA256 Credential=fixture-key/"))
            elif family == "git":
                self.assertEqual(path, f"/api/repos/org/repo/blob?rev={'c' * 40}&path={SERVICE.PAYLOAD}")
                self.assertEqual(headers["cookie"], "__Host-crab_session=fixture-token")
            else:
                self.assertEqual(path, f"/git/org/repo.git/info/lfs/objects/{SHA256}?size={len(BODY)}")
                self.assertTrue(headers["authorization"].startswith("Basic "))
            if fault == "rotate":
                args.service_log.rename(args.service_log.with_suffix(".old"))
            if fault == "wrong-blob-event":
                edge[1]["git_blob_oid_sha256"] = "e" * 64
            if fault == "unrelated-route":
                edge[0]["consumer"] = "sdk"
            if fault == "missing-request":
                edge[0]["request_sha256"] = ""
            lines = [natives]
            if args.arm == "edge" and fault != "no-edge" or fault == "origin-edge":
                lines += [{"target": "crab_storage::cdn::transfer", "fields": event} for event in edge]
            with args.service_log.open("a", encoding="utf-8") as log:
                log.writelines(json.dumps(line) + "\n" for line in lines)
            output = Message()
            output["ETag"] = ETAG if fault != "wrong-etag" else '"another"'
            output["Content-Length"] = str(len(BODY))
            if consumer == "server" and family == "git":
                output["x-crab-blob-oid"] = GIT_OID.hex() if fault != "wrong-blob-header" else "f" * 40
            body = BODY if fault != "wrong-body" else b"z" * len(BODY)
            yield SimpleNamespace(status=206 if fault == "partial" else 200, headers=output, read=io.BytesIO(body).read)

        with patch.object(SERVICE, "https_response", exchange):
            return SERVICE.probe(args)

    def test_measures_each_consumer_and_rejects_stale_traffic(self):
        for provider, consumer, family in (("cloud-cdn", "server", "git"), ("front-door", "server", "lfs"),
                                           ("cloudfront", "s3-gateway", "git"), ("cloudfront", "s3-gateway", "lfs")):
            with self.subTest(provider=provider, consumer=consumer, family=family), TemporaryDirectory() as directory:
                args = self.prepare(Path(directory), provider, consumer, family)
                edge = self.request(args, provider, consumer, family)
                args.arm = "origin"
                origin = self.request(args, provider, consumer, family)
                self.assertEqual((edge["verified_edge_payloads"], origin["all_edge_attempts"]), (1, 0))
                self.assertEqual((edge["payload_sha256"], origin["payload_sha256"]), (SHA256, SHA256))
                self.assertEqual(edge["etag_sha256"], origin["etag_sha256"])
                self.assertEqual(set(edge["edge_responses"]), {"d" * 64})
                self.assertNotIn("fixture-token", json.dumps(edge))
                self.assertNotIn("fixture-secret", json.dumps(edge))

    def test_requires_verified_http_content_and_exact_new_storage_observations(self):
        provider, consumer, family = "cloudfront", "server", "git"
        with TemporaryDirectory() as directory:
            args = self.prepare(Path(directory), provider, consumer, family)
            for fault, reason in (("wrong-etag", "approved strong ETag"), ("wrong-body", "different payload bytes"),
                                  ("wrong-blob-header", "blob ID did not match"), ("partial", "incomplete or transformed"),
                                  ("no-edge", "requires verified edge delivery"), ("missing-request", "every viewer attempt"),
                                  ("wrong-blob-event", "requires verified edge delivery"),
                                  ("unrelated-route", "requires verified edge delivery"), ("rotate", "log rotated")):
                with self.subTest(fault=fault), self.assertRaisesRegex(ValueError, reason):
                    self.request(args, provider, consumer, family, fault)
            args.arm = "origin"
            with self.assertRaisesRegex(ValueError, "zero edge attempts"):
                self.request(args, provider, consumer, family, "origin-edge")

    def test_cloud_cdn_viewer_logs_require_the_exact_service_lfs_key_and_traffic(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            args = self.prepare(root, "cloud-cdn", "server", "lfs")
            report = self.request(args, "cloud-cdn", "server", "lfs")
            tag = f"crab-cdn-qualification/{'0a' * 16}"
            fingerprint = hashlib.sha256(tag.encode()).hexdigest()
            report["edge_responses"] = {fingerprint: report["edge_responses"]["d" * 64]}
            project, rule = "fixture-project", "crab-https"
            config = json.loads(args.fixture.read_bytes())
            prefix = f"{config['base_url']}/{config['fixture_prefix']}"
            viewer = {"logName": f"projects/{project}/logs/requests", "insertId": "viewer",
                      "resource": {"type": "http_load_balancer",
                                   "labels": {"project_id": project, "forwarding_rule_name": rule}},
                      "jsonPayload": {"statusDetails": "response_from_cache"}}
            client, log, output = root / "client.json", root / "gcloud.json", root / "viewer.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(args.fixture),
                       "--configuration-sha256", args.configuration_sha256, "--client", str(client),
                       "--log", str(log), "--output", str(output)]
            for key, extra_bytes, error in (
                (lfs_object_path("1" * 64), 0, "approved service LFS object"),
                (config["object_path"], 1, "transfer totals disagree with the logged responses"),
                (config["object_path"], 0, None),
            ):
                with self.subTest(error=error):
                    request = {"requestMethod": "GET", "requestUrl": f"{prefix}/{key}",
                               "responseSize": "300", "status": 206, "userAgent": tag,
                               "cacheHit": True, "cacheLookup": True}
                    json_file(log, [viewer | {"httpRequest": request}])
                    json_file(client, report | {"edge_bytes": report["edge_bytes"] + extra_bytes})
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if error:
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertIn(error, result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        proof = json.loads(output.read_bytes())
                        self.assertEqual(proof["records"][fingerprint]["key_sha256"], report["lfs_key_sha256"])


if __name__ == "__main__":
    unittest.main()
