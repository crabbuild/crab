#!/usr/bin/env python3
"""Qualification evidence must cover real entropy and overwritten history."""

import hashlib
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_lfs_qualification import Qualification, QualificationError, parse_args, write_deterministic


class LfsQualificationTest(unittest.TestCase):
    def test_payload_has_incompressible_entropy_and_exact_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "payload"
            oid = write_deterministic(path, 1024 * 1024 + 3, "qualification", 1)
            data = path.read_bytes()
            self.assertEqual((len(data), hashlib.sha256(data).hexdigest()), (1024 * 1024 + 3, oid))
            self.assertGreater(len(zlib.compress(data)), len(data) * .99)

    def test_rejects_shapes_that_cannot_commit_every_generated_object(self):
        with tempfile.TemporaryDirectory() as directory:
            args = parse_args(["--crab-bin", "/bin/echo", "--git-bin", "/bin/echo",
                "--git-lfs-bin", "/bin/echo", "--remote-url", "crab://test/isolated",
                "--evidence-dir", str(Path(directory) / "evidence"),
                "--object-count", "3", "--commit-count", "1", "--path-count", "1"])
            with self.assertRaises(QualificationError):
                Qualification(args)

    def test_final_checkout_cannot_mask_missing_historical_object(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            clone = root / "clone"
            clone.mkdir()
            body = b"current payload"
            oid = hashlib.sha256(body).hexdigest()
            (clone / "asset.bin").write_bytes(body)
            current = {"index": 1, "path": "asset.bin", "oid": oid, "size": len(body)}
            old = {**current, "index": 0, "oid": "0" * 64}
            q = object.__new__(Qualification)
            q.env = {}
            q.evidence_dir = root
            q.args = SimpleNamespace(remote_url="crab://test/isolated", crab_args=[])
            q.final_objects = {"asset.bin": current}
            q.report = {"objects": [old, current], "checks": []}
            q.run_git = Mock()
            q.run_lfs = Mock()
            q.run_crab = Mock()
            with self.assertRaises(QualificationError):
                q.verify_clone()
            self.assertEqual(q.report["checks"][-1]["name"], "historical-object-byte-identity-0")


if __name__ == "__main__":
    unittest.main()
