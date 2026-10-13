#!/usr/bin/env python3
"""Regression checks for fixed-history inputs and the independent attribution oracle."""
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from run_remote_read_scaling_rustfs import fixture_import
from verify_remote_browse import verify


class ReadScalingTests(unittest.TestCase):
    def setUp(self):
        root = Path.home() / "Workspace" / "CrabData" / "indexed-read-oracle-tests"
        root.mkdir(parents=True, exist_ok=True)
        self.workspace = tempfile.TemporaryDirectory(dir=root)
        self.root = Path(self.workspace.name)
        self.addCleanup(self.workspace.cleanup)

    def git(self, repo, *args, body=None):
        return subprocess.check_output(["git", "-C", str(repo), *args], input=body,
                                       stderr=subprocess.PIPE)

    def source(self, name, count):
        repo = self.root / name
        repo.mkdir()
        self.git(repo, "init", "-b", "main")
        self.git(repo, "fast-import", "--quiet", body=fixture_import(count))
        return repo

    def test_unrelated_history_keeps_target_bytes_and_change_count_fixed(self):
        small, large = self.source("small", 25), self.source("large", 100)
        self.assertEqual(self.git(small, "show", "HEAD:target.txt"), self.git(large, "show", "HEAD:target.txt"))
        for repo, count in ((small, 25), (large, 100)):
            self.assertEqual(int(self.git(repo, "rev-list", "--count", "HEAD")), count)
            self.assertEqual(int(self.git(repo, "rev-list", "--count", "HEAD", "--", "target.txt")), 21)

    def evidence(self, repo):
        head = self.git(repo, "rev-parse", "HEAD").decode().strip()
        expected = self.git(repo, "blame", "--first-parent", "--root", "--line-porcelain", head, "--", "target.txt")
        origins = [line.split()[0].decode() for line in expected.splitlines()
                   if len(line.split()) >= 3 and len(line.split()[0]) == 40
                   and line.split()[1].isdigit() and not line.startswith(b"\t")]
        entries = [{"start":line, "lines":1, "commit":origin, "source_path":list(b"target.txt")}
                   for line, origin in enumerate(origins, 1)]
        records = [{"kind":"measurement", "operation":operation, "cache":cache,
                    "outcome":"ok", "active_backend_operations":0}
                   for operation, cache in (("open", "cold"), ("blame", "cold"), ("blame", "warm"), ("open", "warm"))]
        records += [{"kind":"result", "operation":"blame", "path":list(b"target.txt"), "value":{"ranges":entries}},
                    {"kind":"complete", "schema":1, "selection":"blame"}]
        return records

    def write_evidence(self, records):
        output = self.root / "evidence.jsonl"
        output.write_text("".join(json.dumps(record) + "\n" for record in records))
        return output

    def test_every_blame_line_is_checked(self):
        repo = self.source("source", 25)
        records = self.evidence(repo)
        proof = verify(repo, self.write_evidence(records), "HEAD")
        self.assertEqual(proof["blame_lines_checked"], 50)
        records[-2]["value"]["ranges"][-1]["commit"] = "0" * 40
        with self.assertRaisesRegex(ValueError, "blame line origins differ"):
            verify(repo, self.write_evidence(records), "HEAD")

    def test_incomplete_measurements_do_not_pass_the_oracle(self):
        repo = self.source("source", 25)
        records = self.evidence(repo)
        records.pop(2)
        with self.assertRaisesRegex(ValueError, "missing cold/warm"):
            verify(repo, self.write_evidence(records), "HEAD")

    def test_optimized_python_still_rejects_incorrect_origins(self):
        repo = self.source("source", 25)
        records = self.evidence(repo)
        records[-2]["value"]["ranges"][-1]["commit"] = "0" * 40
        result = subprocess.run([os.sys.executable, "-O", str(Path(__file__).with_name("verify_remote_browse.py")),
                                 str(repo), str(self.write_evidence(records)), "--output", str(self.root / "proof.json")],
                                capture_output=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
