"""Release gate tests use synthetic evidence and a simulated GitHub run."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import patch

import test_cdn_qualification as fixture


SCRIPT = fixture.SCRIPT.parent / "release/verify-cdn-release-evidence.py"
SPEC = importlib.util.spec_from_file_location("verify_cdn_release", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)
REPOSITORY = "crabbuild/crab"
RUN_ID = "123"
ATTEMPT = "2"


class CdnReleaseQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = fixture.CdnQualificationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.directory = self.fixture.directory
        self.policy = self.directory / "qualified-scopes.txt"
        self.metadata = {
            "id": int(RUN_ID), "run_attempt": int(ATTEMPT), "path": RELEASE.WORKFLOW,
            "head_sha": fixture.COMMIT, "event": "workflow_dispatch",
            "status": "completed", "conclusion": "success",
            "head_repository": {"full_name": REPOSITORY},
        }

    def bundle(self, consumer: str, *platforms: str, provider: str = "s3", family: str = "git", profile: str = "direct",
               cli_payload_bytes: int | None = None) -> str:
        registrations = {}
        for platform in platforms:
            self.fixture.configure(provider, "private", profile, consumer=consumer, platform=platform, family=family,
                                   cli_payload_bytes=cli_payload_bytes)
            scope = self.fixture.scope
            row = f"{scope}/{platform}"
            directory = self.directory / row
            directory.mkdir(parents=True)
            (self.directory / "traces").rename(directory / "traces")
            (self.directory / "traces").mkdir()
            if fixture.CDN.requires_cli_workload(scope) or fixture.CDN.is_direct_sdk_git_scope(scope):
                for name in (fixture.CDN.GIT_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                    (self.directory / name).rename(directory / name)
            if fixture.CDN.is_direct_xorb_read_scope(scope):
                for name in (fixture.CDN.XORB_PRODUCT_DIR, fixture.CDN.GIT_BENCHMARK_DIR):
                    (self.directory / name).rename(directory / name)
            fixture.json_file(directory / "cdn-report.json", self.fixture.report)
            registrations[row] = self.fixture.registration
        (self.directory / provider).mkdir()
        (self.directory / "report.json").rename(self.directory / provider / "report.json")
        self.policy.write_text(fixture.release_policy_row(scope), encoding="utf-8")
        return fixture.json_file(self.directory / "benchmarks.json", registrations)

    def test_complete_cli_bundle_verifies_every_release_platform(self) -> None:
        digest = self.bundle("cli", *fixture.CDN.CLI_PLATFORMS, cli_payload_bytes=32 * 1024 * 1024)
        count = RELEASE.verify_bundle(
            self.directory, self.policy, RELEASE.release_scopes(self.policy), fixture.COMMIT,
            RUN_ID, ATTEMPT, digest,
        )
        self.assertEqual(count, 6)

    def test_managed_cli_git_policy_requires_every_packaged_platform(self) -> None:
        digest = self.bundle("cli", *fixture.CDN.CLI_PLATFORMS, profile="managed")
        self.assertEqual(RELEASE.verify_bundle(
            self.directory, self.policy, RELEASE.release_scopes(self.policy), fixture.COMMIT,
            RUN_ID, ATTEMPT, digest,
        ), len(fixture.CDN.CLI_PLATFORMS))

    def test_cli_scopes_require_one_measured_binary_per_platform(self) -> None:
        scopes = {"cloudfront/private/cli/git/direct", "cloudfront/private/cli/lfs/direct"}
        self.policy.write_text("".join(map(fixture.release_policy_row, sorted(scopes))), encoding="utf-8")
        policy_digest = hashlib.sha256(self.policy.read_bytes()).hexdigest()
        registrations = {
            f"{scope}/{platform}": {
                "candidate": fixture.cli_candidate(scope, platform) | {"release_policy_sha256": policy_digest},
            }
            for scope in scopes for platform in fixture.CDN.CLI_PLATFORMS
        }
        path = self.directory / "benchmarks.json"
        with patch.object(RELEASE, "verify_bundle_row") as individual_row:
            digest = fixture.json_file(path, registrations)
            self.assertEqual(RELEASE.verify_bundle(
                self.directory, self.policy, scopes, fixture.COMMIT, RUN_ID, ATTEMPT, digest,
            ), len(registrations))
            individual_row.reset_mock()

            lfs = registrations["cloudfront/private/cli/lfs/direct/linux-x86_64"]
            lfs["candidate"]["binary_sha256"] = "f" * 64
            digest = fixture.json_file(path, registrations)
            with self.assertRaisesRegex(ValueError, "same measured CLI binary.*linux-x86_64"):
                RELEASE.verify_bundle(
                    self.directory, self.policy, scopes, fixture.COMMIT, RUN_ID, ATTEMPT, digest,
                )
            individual_row.assert_not_called()

    def test_complete_mount_xorb_bundle_verifies_every_release_platform(self) -> None:
        digest = self.bundle("mount", *fixture.CDN.CLI_PLATFORMS, family="xorb")
        self.assertEqual(RELEASE.verify_bundle(
            self.directory, self.policy, RELEASE.release_scopes(self.policy), fixture.COMMIT,
            RUN_ID, ATTEMPT, digest,
        ), len(fixture.CDN.CLI_PLATFORMS))

    def test_cloud_cdn_and_front_door_use_their_matching_native_reports(self) -> None:
        for provider in ("gcs", "azure"):
            with self.subTest(provider=provider):
                digest = self.bundle("sdk", "linux-x86_64", provider=provider)
                count = RELEASE.verify_bundle(
                    self.directory, self.policy, RELEASE.release_scopes(self.policy),
                    fixture.COMMIT, RUN_ID, ATTEMPT, digest,
                )
                self.assertEqual(count, 1)

    def test_cli_cannot_ship_where_its_embedded_route_has_no_evidence(self) -> None:
        digest = self.bundle("cli", "linux-x86_64")
        with self.assertRaisesRegex(ValueError, "missing approved .*darwin-aarch64"):
            RELEASE.verify_bundle(
                self.directory, self.policy, RELEASE.release_scopes(self.policy),
                fixture.COMMIT, RUN_ID, ATTEMPT, digest,
            )

    def test_changing_registrations_after_approval_fails(self) -> None:
        digest = self.bundle("sdk", "linux-x86_64")
        with (self.directory / "benchmarks.json").open("a", encoding="utf-8") as registrations:
            registrations.write("\n")
        with self.assertRaisesRegex(ValueError, "differ from the approved digest"):
            RELEASE.verify_bundle(
                self.directory, self.policy, RELEASE.release_scopes(self.policy),
                fixture.COMMIT, RUN_ID, ATTEMPT, digest,
            )

    def test_approval_requires_exact_successful_provider_workflow_attempt(self) -> None:
        RELEASE.qualified_run(self.metadata, REPOSITORY, fixture.COMMIT, RUN_ID, ATTEMPT)
        for changes in (
            {"id": 456}, {"run_attempt": 3}, {"conclusion": "failure"},
            {"path": ".github/workflows/other.yml"}, {"head_sha": "f" * 40},
            {"event": "push"}, {"head_repository": {"full_name": "fork/crab"}},
        ):
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "CDN evidence"):
                RELEASE.qualified_run(self.metadata | changes, REPOSITORY, fixture.COMMIT, RUN_ID, ATTEMPT)

    def test_origin_only_release_requires_no_approval_or_github(self) -> None:
        self.policy.write_text("# No CDN release scopes\n", encoding="utf-8")
        with (
            patch.object(RELEASE, "POLICY", self.policy),
            patch.object(sys, "argv", [str(SCRIPT), "--source-sha", fixture.COMMIT]),
            patch.dict(os.environ, clear=True),
            patch.object(subprocess, "check_output") as api,
            patch.object(subprocess, "run") as download,
            redirect_stdout(StringIO()) as output,
        ):
            self.assertEqual(RELEASE.main(), 0)
        api.assert_not_called()
        download.assert_not_called()
        self.assertIn("no enabled routes", output.getvalue())

    def test_enabled_release_checks_run_download_and_report_together(self) -> None:
        digest = self.bundle("sdk", "linux-x86_64")
        approval = {"run_id": RUN_ID, "run_attempt": ATTEMPT, "benchmarks_sha256": digest}
        run = subprocess.run

        def simulate_download(command, *, check):
            if command[:3] == ["gh", "run", "download"]:
                self.assertEqual(command[-2:], ["--name", "cdn-release-evidence-123-2"])
                self.assertEqual(command[3:6], [RUN_ID, "--repo", REPOSITORY])
                shutil.copytree(self.directory, command[7], dirs_exist_ok=True)
                return None
            return run(command, check=check, capture_output=True)

        with (
            patch.object(RELEASE, "POLICY", self.policy),
            patch.object(sys, "argv", [str(SCRIPT), "--source-sha", fixture.COMMIT]),
            patch.dict(os.environ, {"CRAB_CDN_RELEASE_EVIDENCE": json.dumps(approval),
                                    "GITHUB_REPOSITORY": REPOSITORY}),
            patch.object(subprocess, "check_output", return_value=json.dumps(self.metadata)) as api,
            patch.object(subprocess, "run", side_effect=simulate_download),
            redirect_stdout(StringIO()) as output,
        ):
            self.assertEqual(RELEASE.main(), 0)
        api.assert_called_once_with(["gh", "api", f"repos/{REPOSITORY}/actions/runs/{RUN_ID}/attempts/{ATTEMPT}"])
        self.assertIn("Verified 1 CDN reports for 1 enabled release scopes", output.getvalue())

    def test_enabled_release_requires_approval(self) -> None:
        self.bundle("sdk", "linux-x86_64")
        with (
            patch.object(RELEASE, "POLICY", self.policy),
            patch.object(sys, "argv", [str(SCRIPT), "--source-sha", fixture.COMMIT]),
            patch.dict(os.environ, clear=True),
            redirect_stderr(StringIO()) as errors,
        ):
            self.assertEqual(RELEASE.main(), 1)
        self.assertIn("CDN release evidence approval", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
