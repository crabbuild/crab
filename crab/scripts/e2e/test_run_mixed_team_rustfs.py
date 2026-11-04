"""Retained push evidence and independent verification lifecycle tests."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from run_concurrent_push_smoke import SmokeError, parse_args
from run_mixed_team_rustfs import MixedTeam


class QualificationCheckpointTest(unittest.TestCase):
    def fixture(self, root, phase="push"):
        source = root / "source"
        source.mkdir(exist_ok=True)
        (source / ".git").mkdir(exist_ok=True)
        binary = root / "crab"
        if not binary.exists():
            binary.write_bytes(b"qualification-binary")
        args = parse_args(["--root", str(root), "--run-id", "checkpoint-test",
                           "--source", str(source), "--agents", "1", "--branch-update-rounds", "1",
                           "--skip-same-branch", "--consumer-mode", "cold-final",
                           "--crab-bin", str(binary), "--no-request-capture"])
        args.phase = phase
        smoke = MixedTeam(args)
        smoke.source_identity = {"head": "1" * 40, "status": ""}
        smoke.report.workload["source_identity"] = smoke.source_identity
        smoke.report.artifacts["payload_history"] = {0: []}
        smoke.source_state = Mock(return_value=smoke.source_identity)
        smoke.git_value = Mock(return_value="2" * 40)
        return smoke

    def sealed(self, root):
        smoke = self.fixture(root)
        smoke.artifacts.mkdir(parents=True)
        (smoke.run_root / "tmp").mkdir()
        record = {"branch": "refs/heads/agent-000", "status": "ok", "command": {"exit_code": 0}}
        smoke.report.branch_fanout = [record]
        smoke.report.branch_updates = [{"round": 1, **record}]
        smoke.report.push_batches = [{"jobs": 1, "successful": 1}] * 2
        smoke.seal_pushes()
        checkpoint = smoke.artifacts / "push-checkpoint.json"
        return smoke, checkpoint, checkpoint.read_bytes()

    def test_checkpoints_cannot_overwrite_retained_failed_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "batch.json"
            MixedTeam.write_checkpoint(path, {"status": "failed"})
            original = path.read_bytes()
            with self.assertRaises(FileExistsError):
                MixedTeam.write_checkpoint(path, {"status": "ok"})
            self.assertEqual(path.read_bytes(), original)

    def test_seal_rejects_a_push_without_a_successful_acknowledgement(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory))
            smoke.report.branch_fanout = [{"status": "git-rejected", "command": {"exit_code": 128}}]
            smoke.report.branch_updates = [{"status": "ok", "command": {"exit_code": 0}}]
            smoke.report.push_batches = [{"jobs": 1}] * 2
            with self.assertRaises(SmokeError):
                smoke.seal_pushes()
            self.assertFalse((smoke.artifacts / "push-checkpoint.json").exists())

    def test_failed_batch_records_preserve_the_rejected_push(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory))
            smoke.report.push_batches = [{"jobs": 1, "successful": 0}]
            rejected = [{"status": "git-rejected", "command": {"exit_code": 128}}]
            smoke.save_batch(0, rejected)
            saved = json.loads((smoke.artifacts / "push-batch-00.json").read_text())
            self.assertEqual(saved["pushes"], rejected)

    def test_seal_rejects_duplicated_branches_even_when_every_command_succeeded(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory))
            smoke.args.agents = 2
            record = {"branch": "refs/heads/agent-000", "status": "ok", "command": {"exit_code": 0}}
            smoke.report.branch_fanout = [record, record]
            smoke.report.branch_updates = [{"round": 1, **record}] * 2
            smoke.report.push_batches = [{"jobs": 2, "successful": 2}] * 2
            with self.assertRaises(SmokeError):
                smoke.seal_pushes()

    def test_push_phase_leaves_all_readback_and_no_repair_audits_pending(self):
        with tempfile.TemporaryDirectory() as directory:
            smoke = self.fixture(Path(directory))
            for method in ("preflight", "init_seed", "run_branch_fanout", "run_branch_updates", "seal_pushes"):
                setattr(smoke, method, Mock())
            smoke.seal_pushes.side_effect = lambda _: setattr(smoke.report, "status", "pushes-complete")
            smoke.verify_cold_team = Mock()
            smoke.run_fsck = Mock()
            self.assertEqual(smoke.run(), 0)
            smoke.verify_cold_team.assert_not_called()
            smoke.run_fsck.assert_not_called()
            self.assertEqual(smoke.report.status, "pushes-complete")

    def test_verification_rejects_changed_inputs_without_rewriting_push_evidence(self):
        for field in ("binary", "endpoint", "cohort"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                original, checkpoint, raw = self.sealed(root)
                report = (original.artifacts / "report.json").read_bytes()
                smoke = self.fixture(root, "verify")
                if field == "binary":
                    Path(smoke.args.crab_bin).write_bytes(b"different-binary")
                elif field == "endpoint":
                    smoke.args.endpoint_url = "http://127.0.0.1:9999"
                else:
                    smoke.args.agents = 2
                self.assertEqual(smoke.run(), 1)
                self.assertEqual(checkpoint.read_bytes(), raw)
                self.assertEqual((original.artifacts / "report.json").read_bytes(), report)
                self.assertEqual(list(original.run_root.glob("verification-*")), [])

    def test_verification_attempts_use_new_consumers_and_preserve_push_proofs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original, checkpoint, raw = self.sealed(root)
            report = (original.artifacts / "report.json").read_bytes()
            attempts = []
            for _ in range(2):
                smoke = self.fixture(root, "verify")
                smoke.verify_cold_team = Mock()
                smoke.run_fsck = Mock()
                self.assertEqual(smoke.run(), 0)
                smoke.verify_cold_team.assert_called_once()
                smoke.run_fsck.assert_called_once()
                self.assertEqual(smoke.report.push_batches, original.report.push_batches)
                attempts.append((smoke.run_root, smoke.env["CRAB_CACHE_DIR"]))
            self.assertNotEqual(attempts[0], attempts[1])
            self.assertEqual(checkpoint.read_bytes(), raw)
            self.assertEqual((original.artifacts / "report.json").read_bytes(), report)

    def paused(self, root):
        smoke = self.fixture(root)
        smoke.args.branch_update_rounds = 3
        smoke.stop_round = 1
        smoke.artifacts.mkdir(parents=True)
        record = {"branch": "refs/heads/agent-000", "status": "ok", "command": {"exit_code": 0,
                  "duration_ms": 1234}}
        smoke.report.branch_fanout = [record]
        smoke.report.branch_updates = [{"round": 1, **record}]
        smoke.report.push_batches = [{"jobs": 1, "successful": 1, "service_ms": {"p95": 1234}}] * 2
        smoke.seal_pushes(1)
        return smoke

    @staticmethod
    def readonly_git(argv, **kwargs):
        args = argv[3:]
        if args == ["rev-parse", "HEAD"]:
            stdout = ("1" if Path(argv[2]).name == "source" else "2") * 40
        elif args == ["status", "--porcelain"]:
            stdout = ""
        else:
            stdout = "\n".join(f"{'2' * 40}\trefs/heads/{name}" for name in ["main", "agent-000"])
        return subprocess.CompletedProcess(argv, 0, stdout=stdout, stderr="")

    def test_push_resume_preserves_original_measurements_and_starts_the_next_round(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = self.paused(root)
            checkpoint = original.artifacts / "pause-through-01.json"
            raw = checkpoint.read_bytes()
            smoke = self.fixture(root)
            smoke.args.branch_update_rounds = 3
            smoke.stop_round = 3
            with patch("run_mixed_team_rustfs.subprocess.run", side_effect=self.readonly_git):
                smoke.resume_pushes()
            self.assertEqual((smoke.next_round, smoke.report.branch_fanout, smoke.report.branch_updates,
                              smoke.report.push_batches, checkpoint.read_bytes()),
                             (2, original.report.branch_fanout, original.report.branch_updates,
                              original.report.push_batches, raw))

    def test_push_resume_rejects_changed_state_without_rewriting_pause_evidence(self):
        for field in ("source", "writer", "dirty-writer", "remote", "workers", "running", "failed", "controller"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                original = self.paused(root)
                report = original.artifacts / "report.json"
                if field in ("running", "failed"):
                    data = json.loads(report.read_text())
                    data["status"] = field
                    report.write_text(json.dumps(data))
                before = {p.name: p.read_bytes() for p in original.artifacts.iterdir()}
                smoke = self.fixture(root)
                smoke.args.branch_update_rounds = smoke.stop_round = 3
                smoke.args.resume = True
                if field == "workers":
                    smoke.args.max_parallel_pushes += 1
                if field == "controller":
                    (root / f".{smoke.run_id}.push-controller").mkdir()

                def changed(argv, **kwargs):
                    result = self.readonly_git(argv, **kwargs)
                    if ((field == "source" and Path(argv[2]).name == "source")
                            or (field == "writer" and Path(argv[2]).name == "branch-agent-000")
                            or (field == "remote" and argv[3] == "ls-remote")):
                        result.stdout = ""
                    if (field == "dirty-writer" and Path(argv[2]).name == "branch-agent-000"
                            and argv[3:] == ["status", "--porcelain"]):
                        result.stdout = " M agents/agent-000.txt"
                    return result

                with patch("run_mixed_team_rustfs.subprocess.run", side_effect=changed):
                    self.assertEqual(smoke.run(), 1)
                self.assertEqual({p.name: p.read_bytes() for p in original.artifacts.iterdir()}, before)


if __name__ == "__main__":
    unittest.main()
