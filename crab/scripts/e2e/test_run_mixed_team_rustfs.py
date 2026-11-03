"""Retained push evidence and independent verification lifecycle tests."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

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


if __name__ == "__main__":
    unittest.main()
