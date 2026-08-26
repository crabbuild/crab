#!/usr/bin/env python3
"""Tests for the large-repository qualification report contract."""

from __future__ import annotations

import copy
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


SCRIPT = Path(__file__).resolve().parents[1] / "verify-large-repo-rustfs-report.py"
SPEC = importlib.util.spec_from_file_location("verify_large_repo_report", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
VERIFY = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VERIFY
SPEC.loader.exec_module(VERIFY)


OID = "a" * 40
BASE = "b" * 40
DIGEST = "c" * 64


def resources() -> dict[str, Any]:
    return {
        "user_cpu_ms": 1,
        "system_cpu_ms": 1,
        "children_max_rss": 1,
        "children_max_rss_unit": "bytes",
    }


def telemetry() -> dict[str, int]:
    return {
        "storage_requests": 1,
        "storage_bytes": 1,
        "range_get": 1,
        "range_get_coalesced": 1,
        "locator_lookup": 1,
        "cache_hits": 0,
        "cache_misses": 1,
        "logical_objects": 1,
        "inflated_bytes": 1,
        "response_bytes": 1,
        "operation_duration_ms": 1,
        "visibility_duration_ms": 1,
        "upload_pack_duration_ms": 1,
        "visibility_plan_ms": 1,
        "pack_generation_ms": 1,
    }


def stage(duration: int = 100) -> dict[str, Any]:
    return {"duration_ms": duration, "resources": resources(), "telemetry": telemetry()}


def valid_report() -> dict[str, Any]:
    replay_count = 3
    checks = [
        "workspace-volume",
        "source-is-git-repository",
        "replay-commit-count",
        "workspace-free-space",
        "crab-build-matches-source",
        "isolated-remote-prefix",
        "advertised-refs-match-source",
        "clone-tips-match-source",
        "deterministic-object-sample-size",
        "sampled-objects-byte-identical",
        "source-checkout-unchanged",
        "retained-artifacts-redacted",
        "acceleration-current-seed",
        "acceleration-current-1",
        "acceleration-current-3",
        "incremental-fetch-tip-1",
        "incremental-fetch-tip-3",
    ]
    stages = {
        "initial_import": stage(),
        "incremental_seed_clone": stage(),
        "full_clone_cold": stage(),
        "full_clone_warm": stage(),
        "blob_none_clone": stage(),
        "depth_1_clone": stage(),
        "depth_100_clone": stage(),
        "incremental_fetch_1": stage(),
        "incremental_fetch_3": stage(),
        "pack_inventory_1": {
            "duration_ms": 1,
            "active_packs": 2,
            "active_pack_bytes": 100,
        },
        "pack_inventory_3": {
            "duration_ms": 1,
            "active_packs": 4,
            "active_pack_bytes": 120,
        },
    }
    for checkpoint in ("seed", "1", "3"):
        stages[f"visibility_owner_{checkpoint}"] = stage()
        stages[f"acceleration_{checkpoint}"] = {
            "duration_ms": 1,
            "manifest_generation": 4,
            "generation_receipt_valid": True,
            "ref_registry_repo_complete": True,
            "locator_available": True,
            "locator_generation": 4,
            "locator_pack_index_hash": DIGEST,
            "visibility_generation": 4,
            "visibility_available": True,
            "visibility_pack_index_hash": DIGEST,
            "visibility_current": True,
            "repair_required": False,
            "notes": [],
        }
    stages["pack_inventory_seed"] = {
        "duration_ms": 1,
        "active_packs": 1,
        "active_pack_bytes": 90,
    }
    summary = {
        "count": 3,
        "min_ms": 10,
        "median_ms": 20,
        "p95_ms": 30,
        "p99_ms": 30,
        "max_ms": 30,
    }
    return {
        "schema": "crab.large-repository-rustfs",
        "version": "1.0",
        "profile": "smoke",
        "run_id": "test-run",
        "status": "ok",
        "valid_for_comparison": True,
        "comparison_invalid_reason": None,
        "started_at": "2026-08-23T00:00:00+00:00",
        "finished_at": "2026-08-23T00:01:00+00:00",
        "error": None,
        "source": {
            "revision": OID,
            "base_revision": BASE,
            "replay_count": replay_count,
        },
        "provenance": {
            "git": "git version 2.50.1",
            "crab": "crab 1.0.0",
            "aws": "aws-cli/2.0.0",
            "python": "3.13.0",
            "platform": "test",
            "host": "test-host",
            "cpu_count": 8,
            "crab_source_revision": OID,
            "crab_binary_sha256": DIGEST,
            "harness_sha256": DIGEST,
            "verifier_sha256": DIGEST,
            "crab_build": {
                "crab_version": "1.0.0",
                "git_sha": OID,
                "build_timestamp": "2026-08-23 00:00:00 UTC",
            },
            "object_store": {
                "kind": "rustfs",
                "endpoint_url": "http://127.0.0.1:9000",
                "version": "rustfs/rustfs:1.0.0",
            },
        },
        "commands": [
            {
                "name": "command",
                "required_success": True,
                "exit_code": 0,
                "duration_ms": 1,
                "resources": resources(),
                "telemetry": telemetry(),
            }
        ],
        "checks": [{"name": name, "ok": True} for name in checks],
        "stages": stages,
        "pushes": [
            {
                "ordinal": index,
                "commit": OID,
                "duration_ms": 10,
                "resources": resources(),
                "telemetry": telemetry(),
            }
            for index in range(replay_count + 1)
        ],
        "store_snapshots": [
            {
                "stage": "seed",
                "objects": 4,
                "bytes": 40,
                "physical_packs": 1,
                "physical_pack_bytes": 20,
            },
            {
                "stage": "1",
                "objects": 6,
                "bytes": 60,
                "physical_packs": 2,
                "physical_pack_bytes": 40,
            },
            {
                "stage": "3",
                "objects": 8,
                "bytes": 80,
                "physical_packs": 4,
                "physical_pack_bytes": 60,
            },
            {
                "stage": "final",
                "objects": 10,
                "bytes": 100,
                "physical_packs": 4,
                "physical_pack_bytes": 80,
            }
        ],
        "correctness": {
            "fingerprint": DIGEST,
            "source_head": OID,
            "full_clone_head": OID,
            "incremental_clone_head": OID,
            "full_fsck": True,
            "incremental_fsck": True,
            "sample_size": 3,
            "advertised_refs": {"refs/heads/main": OID},
        },
        "metrics": {
            "replay_pushes": replay_count,
            "total_pushes": replay_count + 1,
            "operation_summaries": {
                "push": copy.deepcopy(summary),
                "clone": copy.deepcopy(summary),
                "fetch": copy.deepcopy(summary),
            },
        },
        "cleanup": {
            "remote_requested": False,
            "remote_completed": False,
            "local_worktrees_retained": False,
            "local_worktrees_removed": True,
        },
    }


class ReportVerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, name: str, report: dict[str, Any]) -> Path:
        path = self.root / name
        path.write_text(json.dumps(report), encoding="utf-8")
        return path

    def test_valid_smoke_report_is_accepted_explicitly(self) -> None:
        result = VERIFY.verify_report(self.write("report.json", valid_report()), allow_smoke=True)
        self.assertEqual(result.replay_count, 3)

    def test_prevalidated_owner_path_allows_no_remote_visibility_build(self) -> None:
        VERIFY.verify_full_visibility_telemetry(
            {
                "visibility_owner_seed": {
                    "actions": ["catalog_advance", "commit_graph_rebuild", "none"],
                    "telemetry": {
                        "visibility_duration_ms": 0,
                        "storage_requests": 0,
                        "storage_bytes": 0,
                    },
                }
            }
        )

    def test_visibility_repair_requires_telemetry(self) -> None:
        with self.assertRaisesRegex(VERIFY.VerificationError, "visibility repair"):
            VERIFY.verify_full_visibility_telemetry(
                {
                    "visibility_owner_seed": {
                        "actions": ["visibility_repair", "none"],
                        "telemetry": {
                            "visibility_duration_ms": 0,
                            "storage_requests": 0,
                            "storage_bytes": 0,
                        },
                    }
                }
            )

    def test_abbreviated_binary_revision_is_accepted(self) -> None:
        report = valid_report()
        report["provenance"]["crab_build"]["git_sha"] = OID[:7]
        result = VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)
        self.assertEqual(result.source_revision, OID)

    def test_binary_revision_that_does_not_prefix_source_is_rejected(self) -> None:
        report = valid_report()
        report["provenance"]["crab_build"]["git_sha"] = "d" * 7
        with self.assertRaisesRegex(VERIFY.VerificationError, "binary revision"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_smoke_report_cannot_satisfy_full_gate(self) -> None:
        with self.assertRaisesRegex(VERIFY.VerificationError, "smoke report"):
            VERIFY.verify_report(self.write("report.json", valid_report()))

    def test_negative_duration_is_rejected(self) -> None:
        report = valid_report()
        report["stages"]["full_clone_cold"]["duration_ms"] = -1
        with self.assertRaisesRegex(VERIFY.VerificationError, "must not be negative"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_failed_check_is_rejected(self) -> None:
        report = valid_report()
        report["checks"][0]["ok"] = False
        with self.assertRaisesRegex(VERIFY.VerificationError, "check did not pass"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_secret_value_is_rejected(self) -> None:
        report = valid_report()
        report["AWS_SECRET_ACCESS_KEY"] = "live-secret"
        with self.assertRaisesRegex(VERIFY.VerificationError, "credential value"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_comparison_accepts_matching_reports_within_drift(self) -> None:
        baseline = self.write("baseline.json", valid_report())
        candidate_report = valid_report()
        candidate_report["metrics"]["operation_summaries"]["push"]["median_ms"] = 22
        candidate = self.write("candidate.json", candidate_report)
        result = VERIFY.compare_reports(
            baseline,
            candidate,
            maximum_drift=0.20,
            allow_smoke=True,
        )
        self.assertEqual(result["status"], "ok")

    def test_comparison_rejects_excessive_median_drift(self) -> None:
        baseline = self.write("baseline.json", valid_report())
        candidate_report = valid_report()
        candidate_report["metrics"]["operation_summaries"]["push"]["median_ms"] = 30
        candidate = self.write("candidate.json", candidate_report)
        result = VERIFY.compare_reports(
            baseline,
            candidate,
            maximum_drift=0.20,
            allow_smoke=True,
        )
        self.assertEqual(result["status"], "invalid")
        self.assertIn("host contention", result["comparison_invalid_reason"])

    def test_missing_acceleration_stage_is_rejected(self) -> None:
        report = valid_report()
        del report["stages"]["acceleration_3"]
        with self.assertRaisesRegex(VERIFY.VerificationError, "missing required stages"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_stale_acceleration_identity_is_rejected(self) -> None:
        report = valid_report()
        report["stages"]["acceleration_3"]["visibility_generation"] = 3
        with self.assertRaisesRegex(VERIFY.VerificationError, "visibility generation is stale"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_unrelated_generation_receipt_repair_does_not_fail_git_acceleration(self) -> None:
        report = valid_report()
        for name, stage_data in report["stages"].items():
            if name.startswith("acceleration_"):
                stage_data["generation_receipt_valid"] = False
                stage_data["repair_required"] = True
                stage_data["notes"] = ["generation-index receipt missing"]
        result = VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)
        self.assertEqual(result.replay_count, 3)

    def test_inconsistent_advertised_ref_is_rejected(self) -> None:
        report = valid_report()
        report["correctness"]["advertised_refs"]["refs/heads/main"] = BASE
        with self.assertRaisesRegex(VERIFY.VerificationError, "advertised main ref mismatch"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)


if __name__ == "__main__":
    unittest.main()
