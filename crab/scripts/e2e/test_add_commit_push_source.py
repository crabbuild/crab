#!/usr/bin/env python3
"""Read-only fixture and cache isolation tests for managed-file qualification."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("run_add_commit_push_rustfs_smoke.py")
SPEC = importlib.util.spec_from_file_location("add_commit_push_smoke", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
SMOKE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SMOKE
SPEC.loader.exec_module(SMOKE)


class SourceWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / "source"
        self.source.mkdir()
        self.env = {
            **os.environ,
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Fixture",
            "GIT_COMMITTER_NAME": "Fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        }
        self.git("init", "-b", "main")
        (self.source / "tracked.txt").write_text("original\n", encoding="utf-8")
        self.git("add", "tracked.txt")
        self.git("commit", "-m", "fixture")
        self.head = self.git("rev-parse", "HEAD")
        with patch.object(sys, "argv", [
            str(SCRIPT), "--source", str(self.source),
            "--root", str(self.root / "Workspace" / "qualification"),
            "--run-id", "test", "--crab-bin", sys.executable,
        ]):
            self.smoke = SMOKE.AddCommitPushSmoke(SMOKE.parse_args())
        self.smoke.env.update(self.env)
        self.smoke.report.artifacts["fixture_source_head"] = self.head

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.source, env=self.env, check=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ).stdout.strip()

    def preflight(self) -> None:
        self.smoke.run_root.mkdir(parents=True)
        self.smoke.report.artifacts["crab_binary_sha256"] = SMOKE.sha256_file(
            Path(self.smoke.crab_bin)
        )

    def test_source_clone_uses_pinned_commit_without_copying_dirty_worktree(self) -> None:
        (self.source / "tracked.txt").write_text("later commit\n", encoding="utf-8")
        self.git("commit", "-am", "later")
        (self.source / "tracked.txt").write_text("uncommitted\n", encoding="utf-8")
        before = (self.git("rev-parse", "HEAD"), self.git("status", "--porcelain"))

        def product_setup(repo: Path, args: list[str], **kwargs: object) -> None:
            if args[0] == "init":
                (repo / "crab.toml").touch()
                (repo / ".gitattributes").touch()

        # Only product setup is stubbed; clone, checkout, config, and staging use real Git.
        with patch.object(self.smoke, "run_crab", side_effect=product_setup):
            repo, _, _ = self.smoke.prepare_repo("managed")
        observed = (
            (repo / "tracked.txt").read_text(encoding="utf-8"),
            self.smoke.rev_parse(repo, "HEAD"),
            self.git("rev-parse", "HEAD"), self.git("status", "--porcelain"),
        )
        self.assertEqual(observed, ("original\n", self.head, *before))

    def test_existing_payload_path_is_rejected_before_product_setup(self) -> None:
        (self.source / "model.bin").write_bytes(b"original fixture payload")
        self.git("add", "model.bin")
        self.git("commit", "-m", "existing model")
        self.smoke.report.artifacts["fixture_source_head"] = self.git("rev-parse", "HEAD")
        with patch.object(self.smoke, "run_crab") as product:
            with self.assertRaisesRegex(SMOKE.SmokeError, "fixture-paths-unused"):
                self.smoke.prepare_repo("managed")
            product.assert_not_called()

    def test_synthetic_case_accepts_existing_parent_without_reusing_repository(self) -> None:
        self.smoke.args.source = None
        (self.smoke.run_root / "cutover").mkdir(parents=True)

        def product_setup(repo: Path, args: list[str], **kwargs: object) -> None:
            if args[0] == "init":
                (repo / "crab.toml").touch()
                (repo / ".gitattributes").touch()

        with patch.object(self.smoke, "run_crab", side_effect=product_setup):
            repo, _, _ = self.smoke.prepare_repo("cutover")
        record = self.smoke.run_git(repo, ["symbolic-ref", "HEAD"])
        self.assertEqual(self.smoke.read_stdout(record), "refs/heads/main")

    def test_overlap_is_rejected_before_preflight(self) -> None:
        self.smoke.args.source = self.root / "Workspace"
        with patch.object(SMOKE.Path, "home", return_value=self.root):
            with patch.object(self.smoke, "preflight") as preflight:
                with self.assertRaisesRegex(SMOKE.SmokeError, "must not overlap"):
                    self.smoke.run_source_workflows()
                preflight.assert_not_called()

    def test_rejected_overlap_does_not_write_a_failure_report_inside_source(self) -> None:
        workspace = self.root / "Workspace"
        with patch.object(SMOKE.Path, "home", return_value=self.root):
            with patch.object(sys, "argv", [
                str(SCRIPT), "--source", str(workspace), "--root", str(workspace / "nested"),
                "--run-id", "test", "--crab-bin", sys.executable,
            ]):
                with contextlib.redirect_stderr(io.StringIO()):
                    code = SMOKE.main()
        self.assertEqual((code, workspace.exists()), (1, False))

    def test_fixture_symlink_cannot_redirect_generated_payload_outside_clone(self) -> None:
        (self.source / "model.bin").symlink_to(self.root / "missing-payload")
        self.git("add", "model.bin")
        self.git("commit", "-m", "payload symlink")
        self.smoke.report.artifacts["fixture_source_head"] = self.git("rev-parse", "HEAD")
        with patch.object(self.smoke, "run_crab") as product:
            with self.assertRaisesRegex(SMOKE.SmokeError, "fixture-paths-unused"):
                self.smoke.prepare_repo("managed")
            product.assert_not_called()

    def test_failed_workflow_still_checks_read_only_source(self) -> None:
        with patch.object(SMOKE.Path, "home", return_value=self.root):
            with patch.object(self.smoke, "preflight", side_effect=self.preflight):
                with patch.object(self.smoke, "run_case", side_effect=SMOKE.SmokeError("transfer failed")):
                    with self.assertRaisesRegex(SMOKE.SmokeError, "transfer failed"):
                        self.smoke.run_source_workflows()
        self.assertIn(
            ("fixture-source-checkout-unchanged", True),
            [(check["name"], check["ok"]) for check in self.smoke.report.checks],
        )

    def test_command_cache_override_does_not_change_writer_environment(self) -> None:
        original_cache = self.smoke.env["CRAB_CACHE_DIR"]
        clone_cache = str(self.root / "cold-cache")
        completed = subprocess.CompletedProcess([], 0, "", "")
        with patch.object(SMOKE.subprocess, "run", return_value=completed) as run:
            self.smoke.run_crab(
                self.root, ["hydrate", "--all"],
                extra_env={"CRAB_CACHE_DIR": clone_cache},
            )
        self.assertEqual(
            (run.call_args.kwargs["env"]["CRAB_CACHE_DIR"], self.smoke.env["CRAB_CACHE_DIR"]),
            (clone_cache, original_cache),
        )

    def test_source_cannot_silently_select_a_synthetic_case(self) -> None:
        with patch.object(sys, "argv", [str(SCRIPT), "--source", str(self.source), "--only-partial-overlap"]):
            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as result:
                    SMOKE.parse_args()
        self.assertEqual(result.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
