"""Check SDK snapshot results against approved fixtures and actual transport events."""

import argparse
import hashlib
import json
import subprocess
import unittest
from itertools import cycle
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import run_cdn_sdk_workload as SDK
from cdn_qualification import CLI_BUILD_FIELDS
from test_cdn_qualification import COMMIT, cli_candidate, json_file, sdk_candidate
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
        self.config = config
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
                                       private_reader=reader, export_approval=None, warm_local_output=None)
        self.build = {key: value for key, value in sdk_candidate(scope, SDK.runner_platform()).items() if key in CLI_BUILD_FIELDS}
        clock = patch.object(SDK.time, "time_ns", side_effect=cycle((1_000_000_000, 1_010_000_000)))
        clock.start()
        self.addCleanup(clock.stop)

    def observe(self, *, edge: bool, observed_blob: str = "e" * 64, reader: dict | None = None,
                local_blob: str | None = None, local_uses_edge: bool = False,
                **result: object) -> subprocess.CompletedProcess[bytes]:
        report = {"commit": self.oid, "payload": {"sha256": self.sha, "bytes": SDK.GIT_PAYLOAD_SIZE,
                  "git_blob_oid_sha256": "e" * 64}, "build": self.build, "warm_local": None,
                  "started_at_unix_ms": 1001, "finished_at_unix_ms": 1005, "elapsed_ms": 4} | result
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
        selected = {"target": SDK.READ_TARGET, "fields": {
            "message": "SDK captured CDN read target",
            "origin_sha256": hashlib.sha256(self.config["native_origin"].encode()).hexdigest(),
            "prefix_sha256": hashlib.sha256(f"{self.config['fixture_prefix']}/".encode()).hexdigest(),
            "binding_sha256": SDK.viewer_binding_sha256(self.config) if edge else "",
            "public_export_sha256": "",
        } | (reader or {})}
        logs = [selected, native, transfer, verify] if edge else [selected, native]
        logs.append({"target": SDK.ARM_BOUNDARY, "fields": {"phase": "primary_finish"}})
        if local_blob is not None:
            report["warm_local"] = {"started_at_unix_ms": 1006, "finished_at_unix_ms": 1008, "elapsed_ms": 2}
            logs.extend([
                {"target": SDK.ARM_BOUNDARY, "fields": {"phase": "local_start"}},
                native,
                verify | {"fields": verify["fields"] | {"source": "local", "git_blob_oid_sha256": local_blob}},
            ])
            if local_uses_edge:
                logs.append(transfer)
            logs.append({"target": SDK.ARM_BOUNDARY, "fields": {"phase": "local_finish"}})
        logs.append(native)
        return subprocess.CompletedProcess([], 0, json.dumps(report).encode(),
                                           "\n".join(json.dumps(event) for event in logs).encode())

    def test_keeps_redacted_sdk_payload_and_origin_controls_for_the_same_publication(self) -> None:
        for arm in ("edge", "origin"):
            with self.subTest(arm=arm), patch.object(SDK.subprocess, "run", return_value=self.observe(edge=arm == "edge")) as run:
                self.args.arm = arm
                report, local = SDK.fetch(self.args)
                self.assertIsNone(local)
                request = json.loads(run.call_args.kwargs["input"])
                self.assertEqual(request["branch"], self.branch)
                self.assertEqual(request["binding"]["endpoint"], self.issuer)
                self.assertEqual(request["binding"]["approval_sha256"], self.approval if arm == "edge" else None)
                self.assertEqual(report["publication_sha256"], self.receipt_digest)
                self.assertEqual(report["candidate"], self.build | {
                    "binary_sha256": hashlib.sha256(b"fixture probe").hexdigest(),
                })
                self.assertEqual(report["verified_edge_payloads"], int(arm == "edge"))
                self.assertEqual(report["reader"]["binding_sha256"],
                                 SDK.viewer_binding_sha256(self.config) if arm == "edge" else "")
                self.assertNotIn(self.issuer, json.dumps(report))

    def test_rejects_a_fallback_or_a_different_candidate_as_edge_evidence(self) -> None:
        for response, reason in (
            (self.observe(edge=False, reader={"binding_sha256": SDK.viewer_binding_sha256(self.config)}),
             "did not accept verified Git bytes"),
            (self.observe(edge=True, observed_blob="f" * 64), "did not accept verified Git bytes"),
            (self.observe(edge=True, commit="c" * 40), "did not verify the published Git payload"),
            (self.observe(edge=True, build=self.build | {"source_sha": "f" * 40}), "differs from the approved source"),
        ):
            with self.subTest(reason=reason), patch.object(SDK.subprocess, "run", return_value=response):
                with self.assertRaisesRegex(ValueError, reason):
                    SDK.fetch(self.args)

    def test_rejects_different_resolved_targets_even_when_bytes_match(self) -> None:
        for key in ("origin_sha256", "prefix_sha256", "binding_sha256", "public_export_sha256"):
            for arm in ("edge", "origin"):
                with self.subTest(field=key, arm=arm):
                    self.args.arm = arm
                    result = self.observe(edge=arm == "edge", reader={key: "0" * 64})
                    with patch.object(SDK.subprocess, "run", return_value=result):
                        with self.assertRaisesRegex(ValueError, "did not confirm the approved native target"):
                            SDK.fetch(self.args)

    def test_public_edge_read_must_report_the_loaded_export_receipt(self) -> None:
        self.config["scope"] = "cloudfront/public/sdk/git/direct"
        result = self.observe(edge=True, reader={"public_export_sha256": self.approval})
        lines = result.stderr.splitlines()
        self.assertEqual(SDK.verified_target(lines, self.config, "edge", self.approval)["public_export_sha256"],
                         self.approval)
        with self.assertRaisesRegex(ValueError, "did not confirm the approved native target"):
            SDK.verified_target(lines, self.config, "edge", "0" * 64)

    def test_warm_local_record_wraps_only_the_repeat_of_its_own_verified_primer(self) -> None:
        self.args.warm_local_output = self.args.probe.with_name("warm-local.json")
        with patch.object(SDK.subprocess, "run", return_value=self.observe(edge=True, local_blob="e" * 64)) as run:
            edge, local = SDK.fetch(self.args)
        self.assertTrue(json.loads(run.call_args.kwargs["input"])["repeat_snapshot"])
        self.assertEqual(local["primer_sha256"], hashlib.sha256(SDK.json_record(edge).encode()).hexdigest())
        self.assertEqual((edge["verified_edge_payloads"], edge["local_payload_hits"]), (1, 0))
        self.assertEqual(edge["native_traffic"]["get_responses"], 1)
        self.assertEqual((local["all_edge_attempts"], local["local_payload_hits"]), (0, 1))
        self.assertEqual((local["elapsed_ms"], local["native_traffic"]["get_responses"]), (2, 1))

    def test_warm_local_rejects_an_unrelated_cached_blob_or_edge_activity(self) -> None:
        self.args.warm_local_output = self.args.probe.with_name("warm-local.json")
        for cached, edge_reads in (("d" * 64, False), ("e" * 64, True)):
            with self.subTest(cached=cached, edge_reads=edge_reads), patch.object(
                SDK.subprocess, "run", return_value=self.observe(edge=True, local_blob=cached, local_uses_edge=edge_reads),
            ):
                with self.assertRaisesRegex(ValueError, "verified in-memory Git blob without edge traffic"):
                    SDK.fetch(self.args)
