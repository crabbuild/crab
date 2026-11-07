"""Existing-ref measurements require a complete, acknowledged bootstrap."""

import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from run_concurrent_push_smoke import ConcurrentPushSmoke, SmokeError, parse_args
from run_existing_ref_push_rustfs import ExistingRefPush


class ExistingRefPushTest(unittest.TestCase):
    def fixture(self, root, refs):
        binary = root / "crab"
        binary.write_bytes(b"qualification-binary")
        args = parse_args(["--root", str(root), "--run-id", "existing-ref-test",
                           "--agents", "2", "--skip-same-branch", "--crab-bin", str(binary)])
        smoke = ExistingRefPush(args)
        stdout = root / "bootstrap.json"
        stdout.write_text(json.dumps({"schema": "push", "data": {"refs": refs}}))
        smoke.run_crab = Mock(return_value=SimpleNamespace(stdout_log=str(stdout), duration_ms=200))
        smoke.git_value = Mock(return_value="a" * 40)
        smoke.write_report = Mock()
        smoke.request_snapshot = Mock()
        smoke.store_snapshot = Mock()
        return smoke

    def refs(self):
        return [{"src": "a" * 40, "dst": f"refs/heads/agent-{index:03d}", "status": "ok"}
                for index in range(2)]

    def test_bootstrap_publishes_every_agent_ref_before_timed_pushes(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory), self.refs())
            with patch.object(ConcurrentPushSmoke, "init_seed"):
                smoke.init_seed()
            smoke.run_crab.assert_called_once_with(smoke.seed, [
                "push", "--json", "origin", f"{'a' * 40}:refs/heads/agent-000",
                f"{'a' * 40}:refs/heads/agent-001",
            ], name="bootstrap agent refs")
            self.assertEqual(smoke.report.workload["ref_setup"], {
                "method": "bulk-bootstrap", "refs": 2, "base_oid": "a" * 40,
                "duration_ms": 200,
            })
            self.assertEqual(smoke.report.push_batches, [])

    def test_incomplete_or_rejected_bootstrap_cannot_be_reported_as_existing_refs(self):
        for failure in ("missing", "duplicate", "wrong-base", "rejected"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                refs = self.refs()
                if failure == "missing":
                    refs.pop()
                elif failure == "duplicate":
                    refs[1] = refs[0].copy()
                elif failure == "wrong-base":
                    refs[1]["src"] = "b" * 40
                else:
                    refs[1]["status"] = "error"
                smoke = self.fixture(Path(directory), refs)
                with patch.object(ConcurrentPushSmoke, "init_seed"), self.assertRaises(SmokeError):
                    smoke.init_seed()
                self.assertNotIn("ref_setup", smoke.report.workload)

    def test_each_update_commits_a_fresh_four_kib_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory), self.refs())
            repo = smoke.branch_agents / "branch-agent-000"
            (repo / "agents").mkdir(parents=True)
            smoke.logs.mkdir()
            smoke.run_git(repo, ["init", "-b", "agent-000"])
            smoke.configure_git_identity(repo, "payload-test")
            payloads = []
            for round_index in (1, 2):
                smoke.prepare_branch_update(0, round_index)
                payloads.append(subprocess.check_output(
                    [smoke.args.git_bin, "show", "HEAD:payload.bin"], cwd=repo, env=smoke.env,
                ))
            self.assertEqual((len(payloads[0]), len(payloads[1]), payloads[0] != payloads[1]),
                             (4096, 4096, True))


if __name__ == "__main__":
    unittest.main()
