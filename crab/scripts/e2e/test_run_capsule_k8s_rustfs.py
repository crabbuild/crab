#!/usr/bin/env python3
"""Tests for the Kubernetes capsule-protocol qualification harness."""

from __future__ import annotations

import argparse
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
    def test_staged_binary_survives_loss_of_candidate_source(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            candidate.write_bytes(b"#!/bin/sh\nprintf staged\n")
            candidate.chmod(0o755)
            qualification = QUALIFICATION.Qualification(argparse.Namespace(
                root=root, run_id="run", endpoint_url="http://127.0.0.1:9000",
                bucket="fixture", crab_bin=str(candidate), source=str(candidate),
            ))
            qualification.git = Mock(side_effect=RuntimeError("stop after binary staging"))

            with self.assertRaisesRegex(RuntimeError, "stop after binary staging"):
                qualification.initialize()

            candidate.unlink()
            self.assertEqual(
                subprocess.run([qualification.crab], check=True, capture_output=True).stdout,
                b"staged",
            )
            self.assertEqual(
                (qualification.bin_dir / "git-remote-crab").resolve(), qualification.crab,
            )

    def test_partial_clone_source_is_rejected_before_remote_initialization(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            subprocess.run(["git", "init", str(source)], check=True, capture_output=True)
            subprocess.run(["git", "-C", str(source), "config", "user.name", "Fixture"], check=True)
            subprocess.run(
                ["git", "-C", str(source), "config", "user.email", "fixture@example.invalid"],
                check=True,
            )
            for name in ("first", "second"):
                (source / "history").write_text(name)
                subprocess.run(
                    ["git", "-C", str(source), "add", "history"], check=True, capture_output=True,
                )
                subprocess.run(
                    ["git", "-C", str(source), "commit", "-m", name], check=True, capture_output=True,
                )
            subprocess.run(
                ["git", "-C", str(source), "config", "remote.origin.url", "https://example.invalid/repo"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(source), "config", "remote.origin.promisor", "true"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(source), "config", "remote.origin.partialclonefilter", "blob:none"],
                check=True,
            )
            binary = root / "candidate"
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            qualification = QUALIFICATION.Qualification(argparse.Namespace(
                root=root / "runs",
                run_id="partial-source",
                endpoint_url="http://127.0.0.1:9000",
                bucket="fixture",
                access_key="fixture-access",
                secret_key="fixture-secret",
                region="auto",
                git_bin="git",
                source=str(source),
                commits=1,
                crab_bin=str(binary),
            ))
            qualification.proxy = Mock()
            qualification.proxy.url = "http://127.0.0.1:9001"

            with self.assertRaisesRegex(RuntimeError, "source Git repository uses promisor objects"):
                qualification.initialize()

            self.assertFalse(qualification.replay.exists())
            self.assertFalse(qualification.report_path.exists())

    def test_saved_diagnostics_redact_credentials_without_changing_command_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "tmp").mkdir()
            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.root = root
            qualification.proxy = Mock()
            qualification.proxy.snapshot.return_value = {}
            qualification.proxy.paths_since.return_value = []
            qualification.env = Mock(return_value={
                "AWS_ACCESS_KEY_ID": "fixture-access", "AWS_SECRET_ACCESS_KEY": "fixture-secret",
                "AWS_SESSION_TOKEN": "fixture-token",
            })
            diagnostics = root / "artifacts" / "fetch.stderr.log"
            script = (
                "import os,sys; values=' '.join(os.environ[k] for k in "
                "('AWS_ACCESS_KEY_ID','AWS_SECRET_ACCESS_KEY','AWS_SESSION_TOKEN')); "
                "print(values); print(values,file=sys.stderr); sys.exit(int(sys.argv[1]))"
            )
            for exit_code in (0, 1):
                with self.subTest(exit_code=exit_code):
                    command = [sys.executable, "-c", script, str(exit_code)]
                    if exit_code:
                        with self.assertRaisesRegex(RuntimeError, "command failed") as error:
                            qualification.run(command, root, stderr_path=diagnostics)
                        for value in qualification.env().values():
                            self.assertNotIn(value, str(error.exception))
                    else:
                        *_, stdout = qualification.run(command, root, stderr_path=diagnostics)
                        self.assertEqual(stdout, "fixture-access fixture-secret fixture-token\n")
                    self.assertEqual(diagnostics.read_text(), "<redacted> <redacted> <redacted>\n")

    def test_fetch_phases_match_parent_sessions_without_summing_overlaps(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "fetch.jsonl"
            events = [
                {"event": "start", "sid": "root", "argv": ["git", "fetch", "origin"]},
                {"event": "child_start", "sid": "root", "child_id": 0,
                 "child_class": "remote-crab", "argv": ["git", "remote-crab"]},
                {"event": "child_start", "sid": "root/child", "child_id": 0,
                 "argv": ["git", "rev-list"]},
                {"event": "child_exit", "sid": "root/child", "child_id": 0, "t_rel": 99, "code": 0},
                {"event": "child_start", "sid": "root", "child_id": 1,
                 "argv": ["git", "index-pack", "--stdin"]},
                {"event": "child_exit", "sid": "root", "child_id": 1, "t_rel": 2, "code": 0},
                {"event": "child_exit", "sid": "root", "child_id": 0, "t_rel": 7, "code": 0},
                {"event": "child_start", "sid": "root", "child_id": 2,
                 "argv": ["git", "rev-list", "--quiet"]},
                {"event": "child_exit", "sid": "root", "child_id": 2, "t_rel": 3, "code": 0},
                {"event": "exit", "sid": "root", "t_abs": 10.1, "code": 0},
                {"event": "atexit", "sid": "root", "t_abs": 10.2, "code": 0},
            ]
            trace.write_text("\n".join(json.dumps(event) for event in events))
            self.assertEqual(QUALIFICATION.git_fetch_phase_summary(trace), {
                "fetch_ms": 10100.0,
                "exit_code": 0,
                "complete": True,
                "children": [
                    {"phase": "remote-helper", "elapsed_ms": 7000.0, "exit_code": 0},
                    {"phase": "index-pack", "elapsed_ms": 2000.0, "exit_code": 0},
                    {"phase": "connectivity", "elapsed_ms": 3000.0, "exit_code": 0},
                ],
            })

    def test_missing_or_truncated_fetch_trace_is_not_complete(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "fetch.jsonl"
            self.assertFalse(QUALIFICATION.git_fetch_phase_summary(trace)["complete"])
            trace.write_text('\n'.join([
                json.dumps({"event": "start", "sid": "root"}),
                json.dumps({"event": "child_start", "sid": "root", "child_id": 0,
                            "argv": ["git", "index-pack"]}),
                json.dumps({"event": "exit", "sid": "root", "t_abs": 1, "code": 1}),
                '{"truncated":',
            ]))
            self.assertEqual(QUALIFICATION.git_fetch_phase_summary(trace), {
                "fetch_ms": 1000.0, "exit_code": 1, "complete": False,
                "children": [{"phase": "index-pack", "elapsed_ms": None, "exit_code": None}],
            })

    def test_fetch_retains_diagnostics_and_runs_integrity_checks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.root = root
            qualification.incremental = root / "client"
            qualification.trace2_root = root / "trace2"
            qualification.args = Mock(git_bin="git")
            qualification.git = Mock(return_value="expected-tip")
            qualification.report = {"maintenance": []}
            qualification.save = Mock()

            def run(command: list[str], cwd: Path, **options: object) -> tuple:
                self.assertEqual(command, ["git", "fetch", "origin"])
                self.assertEqual(cwd, qualification.incremental)
                self.assertTrue(options["meter"])
                self.assertIn("crab_remote_git::telemetry=info", options["extra_env"]["CRAB_LOG"])
                self.assertIn("crab::git::remote_helper=info", options["extra_env"]["CRAB_LOG"])
                trace = Path(options["extra_env"]["GIT_TRACE2_EVENT"])
                trace.write_text('\n'.join(json.dumps(event) for event in [
                    {"event": "start", "sid": "fetch"},
                    {"event": "child_start", "sid": "fetch", "child_id": 0,
                     "argv": ["git", "unpack-objects"]},
                    {"event": "child_exit", "sid": "fetch", "child_id": 0,
                     "t_rel": 0.1, "code": 0},
                    {"event": "exit", "sid": "fetch", "t_abs": 0.2, "code": 0},
                ]))
                diagnostics = options["stderr_path"]
                diagnostics.parent.mkdir(parents=True)
                diagnostics.write_text("pack-generation measurement\n")
                return 201, {"requests": 8}, {}, ""

            qualification.run = run
            qualification.fetch(500, "expected-tip")
            measurement, = qualification.report["maintenance"]
            self.assertEqual(measurement["git_fetch_phases"]["children"], [
                {"phase": "unpack-objects", "elapsed_ms": 100.0, "exit_code": 0},
            ])
            self.assertEqual(Path(measurement["diagnostics"]).read_text(), "pack-generation measurement\n")
            qualification.git.assert_any_call(
                ["fsck", "--connectivity-only"], qualification.incremental, timeout=7200,
            )
            qualification.save.assert_called_once()

    def test_clone_leaves_private_cache_creation_to_crab_and_reuses_it(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "cache" / "final-clones"
            qualification = object.__new__(QUALIFICATION.Qualification)
            qualification.root = root
            qualification.crab = Path("crab")
            qualification.remote_url = "crab://fixture/repo"
            qualification.trace_path = Mock(return_value=root / "trace.jsonl")
            qualification.save = Mock()
            qualification.report = {"maintenance": []}

            def run(_command: list[str], _cwd: Path, **options: object) -> tuple:
                self.assertEqual(options["extra_env"]["CRAB_CACHE_DIR"], str(cache))
                if options["operation"] == "cold":
                    self.assertFalse(cache.exists(), "Crab must create its private cache root")
                    cache.mkdir(parents=True, mode=0o700)
                    (cache / "retained").write_bytes(b"verified pack")
                else:
                    self.assertEqual((cache / "retained").read_bytes(), b"verified pack")
                return 1, {}, {}, ""

            qualification.run = run
            for name in ("cold", "warm"):
                qualification.clone(root / name, name, 5000, cache_name="final-clones")

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
        high_request_count = QUALIFICATION.fetch_summary(
            [{"elapsed_ms": 5550, "object_store": {"requests": 27}}]
        )
        high_request_gate = QUALIFICATION.fetch_performance_gate(
            high_request_count, commits=500, interval=500
        )
        self.assertEqual(high_request_gate["status"], "passed")
        self.assertEqual(high_request_gate["object_store_requests_p95_observed"], 27)
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
