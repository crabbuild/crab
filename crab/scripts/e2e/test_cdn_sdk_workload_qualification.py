"""Check SDK snapshot results against approved fixtures and actual transport events."""

import argparse
import hashlib
import json
import subprocess
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import run_cdn_sdk_workload as SDK
from test_cdn_qualification import COMMIT, cli_candidate, json_file
from test_cdn_transport_qualification import fixture


class CdnSdkWorkloadQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        scope = "cloudfront/private/sdk/git/direct"
        self.issuer = "https://issuer.example.com/v1/credentials"
        self.approval = "6" * 64
        config = fixture(access="private") | {"scope": scope, "viewer_issuer": {
            "endpoint": self.issuer, "audience": "https://issuer.example.com", "approval_sha256": self.approval,
        }}
        config_path = root / "fixture.json"
        configuration = json_file(config_path, config)
        self.oid, self.sha = "a" * 40, "b" * 64
        self.branch = "crab-cdn-qualification-123-2-0123456789ab"
        receipt = root / "receipt.json"
        self.receipt_digest = json_file(receipt, {
            "schema": "crab.cdn-git-publication/v1", "scope": scope, "configuration_sha256": configuration,
            "source_sha": COMMIT, "workflow_run_id": "123", "workflow_run_attempt": "2",
            "ref": "refs/heads/" + self.branch, "oid": self.oid, "sha256": self.sha, "published": True,
            "visible_at_unix_ms": 1, "candidate": cli_candidate(scope, SDK.runner_platform()),
        })
        probe = root / "cdn-sdk-read"
        probe.write_bytes(b"fixture probe")
        reader = root / "reader.json"
        json_file(reader, {"oidc_issuer": "https://login.example.com", "oidc_client_id": "crab",
                           "token_cache_directory": str(root / "login")})
        self.args = argparse.Namespace(arm="edge", fixture=config_path, configuration_sha256=configuration,
                                       receipt=receipt, probe=probe, source_sha=COMMIT, run_id="123", run_attempt="2",
                                       private_reader=reader, export_approval=None)

    def observe(self, *, edge: bool, observed_blob: str = "e" * 64, **result: object) -> subprocess.CompletedProcess[bytes]:
        report = {"commit": self.oid, "sha256": self.sha, "bytes": SDK.GIT_PAYLOAD_SIZE,
                  "source_sha": COMMIT, "qualification_feature": True, "git_blob_oid_sha256": "e" * 64} | result
        native = {"target": "crab_storage::native_read", "fields": {
            "message": "Native object store read", "provider": "s3", "method": "get", "status": 200, "bytes": 8,
        }}
        route = {"provider": "s3", "access": "private", "consumer": "sdk", "family": "git", "profile": "direct"}
        transfer = {"target": "crab_storage::cdn::transfer", "fields": route | {
            "message": "CDN immutable GET", "status": 206, "bytes": 64, "elapsed_ms": 1, "cache": "hit",
            "range": True, "complete": True, "request_sha256": "d" * 64,
        }}
        verify = {"target": "crab_storage::cdn::transfer", "fields": route | {
            "message": "CDN verified Git payload", "source": "edge", "git_blob_oid_sha256": observed_blob,
        }}
        logs = [native, transfer, verify] if edge else [native]
        return subprocess.CompletedProcess([], 0, json.dumps(report).encode(),
                                           "\n".join(json.dumps(event) for event in logs).encode())

    def test_keeps_redacted_sdk_payload_and_origin_controls_for_the_same_publication(self) -> None:
        for arm in ("edge", "origin"):
            with self.subTest(arm=arm), patch.object(SDK.subprocess, "run", return_value=self.observe(edge=arm == "edge")) as run:
                self.args.arm = arm
                report = SDK.fetch(self.args)
                request = json.loads(run.call_args.kwargs["input"])
                self.assertEqual(request["branch"], self.branch)
                self.assertEqual(request["binding"]["endpoint"], self.issuer)
                self.assertEqual(request["binding"]["approval_sha256"], self.approval if arm == "edge" else None)
                self.assertEqual(report["publication_sha256"], self.receipt_digest)
                self.assertEqual(report["probe_sha256"], hashlib.sha256(b"fixture probe").hexdigest())
                self.assertEqual(report["verified_edge_payloads"], int(arm == "edge"))
                self.assertNotIn(self.issuer, json.dumps(report))

    def test_rejects_a_fallback_or_a_different_candidate_as_edge_evidence(self) -> None:
        for response, reason in (
            (self.observe(edge=False), "did not accept verified Git bytes"),
            (self.observe(edge=True, observed_blob="f" * 64), "did not accept verified Git bytes"),
            (self.observe(edge=True, commit="c" * 40), "did not verify the published Git payload"),
            (self.observe(edge=True, source_sha="f" * 40), "did not use the requested source build"),
        ):
            with self.subTest(reason=reason), patch.object(SDK.subprocess, "run", return_value=response):
                with self.assertRaisesRegex(ValueError, reason):
                    SDK.fetch(self.args)
