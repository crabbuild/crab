#!/usr/bin/env python3
"""Tests for the Kubernetes capsule-protocol qualification harness."""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_ROOT))
SCRIPT = SCRIPT_ROOT / "run_capsule_k8s_rustfs.py"
SPEC = importlib.util.spec_from_file_location("run_capsule_k8s_rustfs", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
QUALIFICATION = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = QUALIFICATION
SPEC.loader.exec_module(QUALIFICATION)


class CapsuleKubernetesQualificationTests(unittest.TestCase):
    def test_request_log_preserves_run_operation_with_transport_operation(self) -> None:
        request = {"operation": "get", "method": "GET"}

        entry = QUALIFICATION.request_log_entry("push-00001", request)

        self.assertEqual(
            entry,
            {"run_operation": "push-00001", "operation": "get", "method": "GET"},
        )

    def test_push_windows_report_latency_requests_and_sampled_resources(self) -> None:
        pushes = [
            {
                "ordinal": 1,
                "elapsed_ms": 100,
                "object_store": {"requests": 4, "request_body_bytes": 10},
                "resources": {"user_cpu_ms": 3, "system_cpu_ms": 1, "children_max_rss": 50},
            },
            {
                "ordinal": 2,
                "elapsed_ms": 300,
                "object_store": {"requests": 8, "response_body_bytes": 30},
            },
            {
                "ordinal": 3,
                "elapsed_ms": 200,
                "object_store": {"requests": 6},
            },
        ]

        windows = QUALIFICATION.push_window_summaries(pushes, 2)

        self.assertEqual(
            [(window["start_ordinal"], window["end_ordinal"]) for window in windows],
            [(1, 2), (3, 3)],
        )
        self.assertEqual(windows[0]["latency_ms"]["mean"], 200)
        self.assertEqual(windows[0]["object_store_requests"]["mean"], 6)
        self.assertEqual(windows[0]["request_body_bytes"], 10)
        self.assertEqual(windows[0]["response_body_bytes"], 30)
        self.assertEqual(windows[0]["resource_sample_count"], 1)
        self.assertEqual(windows[0]["children_max_rss"], 50)

    def test_git_auto_maintenance_parser_ignores_other_children(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            trace.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "event": "child_start",
                                "argv": ["git", "gc", "--auto"],
                            }
                        ),
                        json.dumps(
                            {
                                "event": "child_start",
                                "argv": ["git", "pack-objects", "--stdout"],
                            }
                        ),
                        "invalid json",
                    ]
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                QUALIFICATION.git_auto_maintenance_events(trace),
                [["git", "gc", "--auto"]],
            )

    @unittest.skipUnless(shutil.which("git"), "Git is required")
    def test_sampled_blob_digests_match_between_repository_and_clone(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            clone = root / "clone.git"
            source.mkdir()
            for args in (
                ["git", "init", "-q", str(source)],
                ["git", "-C", str(source), "config", "user.name", "Crab Test"],
                ["git", "-C", str(source), "config", "user.email", "crab-test@example.invalid"],
            ):
                subprocess.run(args, check=True)
            (source / "sample.txt").write_bytes(b"verified blob bytes\x00\xff")
            subprocess.run(["git", "-C", str(source), "add", "sample.txt"], check=True)
            subprocess.run(
                ["git", "-C", str(source), "commit", "-q", "-m", "sample"], check=True
            )
            tip = subprocess.run(
                ["git", "-C", str(source), "rev-parse", "HEAD"],
                check=True,
                stdout=subprocess.PIPE,
                text=True,
            ).stdout.strip()
            subprocess.run(["git", "clone", "-q", "--bare", str(source), str(clone)], check=True)

            source_samples = QUALIFICATION.sampled_blob_digests("git", source, tip)
            clone_samples = QUALIFICATION.sampled_blob_digests("git", clone, tip)

        self.assertEqual(source_samples, clone_samples)
        self.assertEqual(next(iter(source_samples.values()))["size"], len(b"verified blob bytes\x00\xff"))


if __name__ == "__main__":
    unittest.main()
