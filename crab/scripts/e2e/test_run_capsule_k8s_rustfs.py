#!/usr/bin/env python3
"""Tests for the Kubernetes capsule-protocol qualification harness."""

from __future__ import annotations

import importlib.util
import hashlib
import json
import sqlite3
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock


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
    def test_changed_binary_cannot_pass_qualification(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "candidate"
            binary.write_bytes(b"replacement binary")
            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.crab = binary
            qualification.report = {
                "provenance": {"crab_sha256": hashlib.sha256(b"original binary").hexdigest()}
            }

            with self.assertRaisesRegex(RuntimeError, "binary changed"):
                qualification.summarize()

            self.assertFalse(qualification.report["provenance"]["binary_unchanged"])

    def test_seed_is_checked_before_incremental_replay(self) -> None:
        qualification = object.__new__(QUALIFICATION.Qualification)
        qualification.proxy = Mock()
        qualification.initialize = Mock()
        qualification.save = Mock()
        qualification.repack = Mock()
        qualification.clone = Mock()
        qualification.incremental = Path("seed-clone")
        qualification.report = {
            "source": {"base": "base"},
            "commit_oids": ["next"],
            "correctness": {},
        }
        qualification.git = Mock(return_value="base")
        qualification.remote_fsck = Mock()

        def push(ordinal: int, _oid: str) -> None:
            if ordinal:
                qualification.git.assert_any_call(
                    ["fsck", "--strict", "--full", "--no-reflogs"],
                    qualification.incremental,
                    timeout=7200,
                )
                qualification.remote_fsck.assert_called_once_with(0, "seed")
                raise RuntimeError("stop after verified seed")

        qualification.push = push
        with self.assertRaisesRegex(RuntimeError, "stop after verified seed"):
            qualification.execute()

    def test_staging_source_snapshot_includes_uncheckpointed_wal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source-staging"
            source.mkdir()
            connection = sqlite3.connect(source / "index.db")
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("PRAGMA wal_autocheckpoint=0")
            connection.execute(
                "CREATE TABLE staging_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
            )
            connection.execute(
                "INSERT INTO staging_meta (key, value) VALUES ('layout_version', '1')"
            )
            connection.commit()
            self.assertGreater((source / "index.db-wal").stat().st_size, 0)

            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.replay = root / "replay"
            qualification.copy_staging_source(source)

            snapshot = sqlite3.connect(
                (qualification.replay / ".crab" / "staging" / "index.db").as_uri()
                + "?mode=ro",
                uri=True,
            )
            try:
                version = snapshot.execute(
                    "SELECT value FROM staging_meta WHERE key = 'layout_version'"
                ).fetchone()
            finally:
                snapshot.close()
                connection.close()

        self.assertEqual(version, ("1",))

    def test_request_log_preserves_run_operation_with_transport_operation(self) -> None:
        request = {"operation": "get", "method": "GET"}

        entry = QUALIFICATION.request_log_entry("push-00001", request)

        self.assertEqual(
            entry,
            {"run_operation": "push-00001", "operation": "get", "method": "GET"},
        )

    def test_request_latency_summary_reports_percentiles_and_slowest_call(self) -> None:
        summary = QUALIFICATION.request_latency_summary(
            [
                {
                    "method": "GET",
                    "operation": "get",
                    "category": "v2",
                    "key": "v2/root",
                    "status": 200,
                    "elapsed_ms": 2,
                },
                {
                    "method": "PUT",
                    "operation": "put",
                    "category": "v2",
                    "key": "v2/capsules/hash",
                    "status": 200,
                    "elapsed_ms": 8,
                },
                {"method": "GET", "elapsed_ms": 4},
            ]
        )

        self.assertEqual(summary["count"], 3)
        self.assertEqual(summary["p50"], 4)
        self.assertEqual(summary["p95"], 8)
        self.assertEqual(summary["p99"], 8)
        self.assertEqual(summary["max"], 8)
        self.assertEqual(summary["slowest"]["key"], "v2/capsules/hash")

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

    def test_fetch_summary_reports_latency_io_and_pack_counts(self) -> None:
        fetches = [
            {
                "elapsed_ms": 700,
                "object_store": {
                    "requests": 8,
                    "request_body_bytes": 2,
                    "response_body_bytes": 30,
                },
                "new_local_packs": ["pack-a"],
            },
            {
                "elapsed_ms": 900,
                "object_store": {
                    "requests": 10,
                    "request_body_bytes": 3,
                    "response_body_bytes": 40,
                },
                "new_local_packs": ["pack-b"],
            },
        ]

        summary = QUALIFICATION.fetch_summary(fetches)

        self.assertEqual(summary["latency_ms"]["p95"], 900)
        self.assertEqual(summary["object_store_requests"]["p95"], 10)
        self.assertEqual(summary["request_body_bytes"], 5)
        self.assertEqual(summary["response_body_bytes"], 70)
        self.assertEqual(summary["new_local_pack_count"], 2)
        self.assertEqual(summary["max_new_local_packs"], 1)

    def test_fetch_performance_gate_only_evaluates_complete_500_commit_windows(self) -> None:
        fetches = [
            {"elapsed_ms": 9000, "object_store": {"requests": 10}},
            {"elapsed_ms": 10000, "object_store": {"requests": 9}},
        ]
        summary = QUALIFICATION.fetch_summary(fetches)

        self.assertEqual(
            QUALIFICATION.fetch_performance_gate(summary, commits=1000, interval=500)["status"],
            "passed",
        )
        self.assertEqual(
            QUALIFICATION.fetch_performance_gate(summary, commits=20, interval=10)["status"],
            "not_evaluated",
        )
        over_budget = QUALIFICATION.fetch_summary(
            [{"elapsed_ms": 10_001, "object_store": {"requests": 11}}]
        )
        self.assertEqual(
            QUALIFICATION.fetch_performance_gate(over_budget, commits=500, interval=500)["status"],
            "failed",
        )

    def test_failed_performance_gates_fail_the_qualification(self) -> None:
        self.assertEqual(
            QUALIFICATION.qualification_performance_status(
                push_requests_ok=True,
                push_latency_ok=True,
                fetch_status="failed",
            ),
            "failed",
        )
        self.assertEqual(
            QUALIFICATION.qualification_performance_status(
                push_requests_ok=False,
                push_latency_ok=True,
                fetch_status="passed",
            ),
            "failed",
        )
        self.assertEqual(
            QUALIFICATION.qualification_performance_status(
                push_requests_ok=True,
                push_latency_ok=True,
                fetch_status="not_evaluated",
            ),
            "not_evaluated",
        )

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

    def test_pack_inventory_reports_only_new_complete_pack_bodies(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            pack_directory = repository / ".git" / "objects" / "pack"
            pack_directory.mkdir(parents=True)
            (pack_directory / "pack-old.pack").write_bytes(b"old")
            (pack_directory / "pack-new.pack").write_bytes(b"new")
            (pack_directory / "pack-incomplete.idx").write_bytes(b"index only")

            before = {"old"}
            after = QUALIFICATION.git_pack_inventory(repository)

        self.assertEqual(after, {"old", "new"})
        self.assertEqual(QUALIFICATION.new_pack_ids(before, after), ["new"])
        self.assertEqual(QUALIFICATION.require_at_most_one_new_pack(500, before, after), ["new"])

    def test_repack_parser_distinguishes_rewrite_from_noop_auto_check(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            commands = [
                ["git", "maintenance", "run", "--auto"],
                ["git", "index-pack", "--stdin"],
                ["git", "repack", "-d", "-A"],
            ]
            trace.write_text(
                "\n".join(json.dumps({"event": "child_start", "argv": argv}) for argv in commands),
                encoding="utf-8",
            )

            self.assertEqual(QUALIFICATION.git_repack_events(trace), [commands[-1]])

    def test_fetch_pack_gate_rejects_multiple_new_packs(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "installed 2 local packs"):
            QUALIFICATION.require_at_most_one_new_pack(500, set(), {"one", "two"})

    def test_fetch_pack_gate_rejects_replacing_an_installed_pack(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "removed 1 installed local packs"):
            QUALIFICATION.require_at_most_one_new_pack(500, {"old"}, {"replacement"})

    def test_repack_summary_reads_the_structured_payload(self) -> None:
        summary = {
            "packs_before": 12,
            "packs_after": 8,
            "bytes_before": 1000,
            "bytes_after": 900,
            "bytes_read": 200,
            "bytes_written": 100,
            "elapsed_ms": 25,
        }

        parsed = QUALIFICATION.parse_repack_summary(
            json.dumps({"schema": "repack", "version": "1.0", "data": summary})
        )

        self.assertEqual(parsed, summary)

    def test_repack_records_git_enumeration_separately_from_compression(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            events = [
                {"event": "region_leave", "category": "pack-objects", "label": label, "t_rel": seconds}
                for label, seconds in (
                    ("enumerate-objects", 71.412),
                    ("prepare-pack", 2.789),
                    ("write-pack-file", 0.468),
                    ("enumerate-objects", 0.25),
                )
            ]
            events.extend([
                {"event": "region_enter", "category": "pack-objects", "label": "prepare-pack"},
                {"event": "region_leave", "category": "index-pack", "label": "parse", "t_rel": 99},
            ])
            trace.write_text(
                "\n".join(json.dumps(event) for event in events) + "\ntruncated event",
                encoding="utf-8",
            )
            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.crab = Path("crab")
            qualification.replay = Path("replay")
            qualification.trace_path = Mock(return_value=trace)
            qualification.save = Mock()
            qualification.report = {"maintenance": []}
            summary = dict.fromkeys(
                ("packs_before", "packs_after", "bytes_before", "bytes_after",
                 "bytes_read", "bytes_written", "elapsed_ms"),
                0,
            )
            qualification.run = Mock(return_value=(75000, {}, {}, json.dumps({"data": summary})))

            qualification.repack(500, "interval")

            self.assertEqual(
                qualification.report["maintenance"][0]["git_pack_phases"],
                {
                    "enumerate-objects": {"count": 2, "total_ms": 71662.0, "max_ms": 71412.0},
                    "prepare-pack": {"count": 1, "total_ms": 2789.0, "max_ms": 2789.0},
                    "write-pack-file": {"count": 1, "total_ms": 468.0, "max_ms": 468.0},
                },
            )

    def test_repack_summary_rejects_missing_fields(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "missing its structured summary"):
            QUALIFICATION.parse_repack_summary(json.dumps({"data": {"packs_before": 1}}))

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
