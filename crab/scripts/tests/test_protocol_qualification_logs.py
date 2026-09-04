#!/usr/bin/env python3
"""Filesystem contracts for protocol qualification evidence logs."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "e2e/run_protocol_v2_partial_clone_rustfs_smoke.py"
SPEC = importlib.util.spec_from_file_location("protocol_qualification", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class EvidenceLogsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.runner = RUNNER.ProtocolV2PartialCloneSmoke.__new__(RUNNER.ProtocolV2PartialCloneSmoke)
        self.runner.command_index = 0
        self.runner.logs = Path(self.directory.name) / "logs"

    def test_long_labels_are_writable_and_remain_unique(self) -> None:
        paths = []
        for prefix in ("git clone /nested/path/" * 30, "仓库" * 200):
            for suffix in ("first", "second", "second"):
                for path in self.runner.log_paths(prefix + suffix):
                    with self.subTest(label=prefix[:20], suffix=suffix, stream=path.suffix):
                        self.assertLessEqual(len(path.name.encode("utf-8")), 255)
                        with path.open("x", encoding="utf-8") as output:
                            output.write(prefix + suffix)
                        self.assertEqual(path.read_text(encoding="utf-8"), prefix + suffix)
                        paths.append(path)
        self.assertEqual(len(set(paths)), len(paths))

    def test_short_labels_keep_readable_stream_names(self) -> None:
        stdout, stderr = self.runner.log_paths("Git fetch origin")
        self.assertEqual((stdout.name, stderr.name),
                         ("001-git-fetch-origin.stdout.log", "001-git-fetch-origin.stderr.log"))

    def test_temp_sampling_tolerates_directory_removed_during_walk(self) -> None:
        self.runner.temp_root = Path(self.directory.name)
        stable = self.runner.temp_root / "stable"
        stable.mkdir()
        (stable / "pack").write_bytes(b"known")
        removed = self.runner.temp_root / "removed"
        removed.mkdir()
        (removed / "pack").write_bytes(b"temporary")
        scandir = RUNNER.os.scandir

        def remove_before_scan(path):
            if Path(path) == removed:
                (removed / "pack").unlink()
                removed.rmdir()
            return scandir(path)

        with mock.patch.object(RUNNER.os, "scandir", side_effect=remove_before_scan):
            self.assertEqual(self.runner.temp_disk_bytes(), len(b"known"))

    def test_temp_sampling_surfaces_scan_permission_errors(self) -> None:
        self.runner.temp_root = Path(self.directory.name)
        with mock.patch.object(RUNNER.os, "scandir", side_effect=PermissionError("denied")):
            with self.assertRaises(PermissionError):
                self.runner.temp_disk_bytes()


if __name__ == "__main__":
    unittest.main()
