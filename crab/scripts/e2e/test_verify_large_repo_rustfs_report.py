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

QUALIFICATION_SCRIPT = Path(__file__).resolve().parent / "run_large_repo_rustfs.py"
QUALIFICATION_SPEC = importlib.util.spec_from_file_location(
    "run_large_repo_rustfs", QUALIFICATION_SCRIPT
)
if QUALIFICATION_SPEC is None or QUALIFICATION_SPEC.loader is None:
    raise RuntimeError(f"cannot import {QUALIFICATION_SCRIPT}")
QUALIFICATION = importlib.util.module_from_spec(QUALIFICATION_SPEC)
sys.modules[QUALIFICATION_SPEC.name] = QUALIFICATION
QUALIFICATION_SPEC.loader.exec_module(QUALIFICATION)

PROTOCOL_SCRIPT = Path(__file__).resolve().parent / "run_protocol_v2_partial_clone_rustfs_smoke.py"
PROTOCOL_SPEC = importlib.util.spec_from_file_location(
    "run_protocol_v2_partial_clone_rustfs_smoke", PROTOCOL_SCRIPT
)
if PROTOCOL_SPEC is None or PROTOCOL_SPEC.loader is None:
    raise RuntimeError(f"cannot import {PROTOCOL_SCRIPT}")
PROTOCOL = importlib.util.module_from_spec(PROTOCOL_SPEC)
sys.modules[PROTOCOL_SPEC.name] = PROTOCOL
PROTOCOL_SPEC.loader.exec_module(PROTOCOL)


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
        "source_download_ms": 1,
        "locator_scan": 0,
        "locator_full_scan": 0,
        "locator_exact_fallback": 0,
        "locator_ordinal_scan": 0,
        "locator_ordinal_metadata": 1,
        "locator_ordinal_metadata_scan": 0,
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
        "version": "1.2",
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
            "clone_refs": {"refs/heads/main": OID},
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


def team_summary() -> dict[str, int]:
    return {
        "duration_ms": 100,
        "median_client_ms": 10,
        "p95_client_ms": 20,
        "p99_client_ms": 30,
    }


def team_results(count: int, categories: list[str], *, fetch: bool = False) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for ordinal in range(1, count + 1):
        result: dict[str, Any] = {
            "ordinal": ordinal,
            "duration_ms": 10,
            "exit_code": 0 if categories[ordinal - 1] in {"accepted", "ok"} else 1,
            "failure_category": categories[ordinal - 1],
        }
        if fetch:
            result.update(
                {
                    "fetch_exit_code": 0,
                    "fsck_exit_code": 0,
                    "tip_matches": True,
                }
            )
        else:
            result["commit"] = OID
        results.append(result)
    return results


def valid_team_load() -> dict[str, Any]:
    fetch_count = 100
    independent_count = 20
    contended_count = 20
    return {
        "enabled": True,
        "fetch_fanout": fetch_count,
        "independent_pushes": independent_count,
        "contended_pushes": contended_count,
        "fetch_seed": {
            "clients": fetch_count,
            "successful_clones": fetch_count,
            **team_summary(),
            "results": team_results(fetch_count, ["ok"] * fetch_count),
        },
        "concurrent_incremental_fetches": {
            "clients": fetch_count,
            "successful": fetch_count,
            "failed": 0,
            **team_summary(),
            "results": team_results(fetch_count, ["ok"] * fetch_count, fetch=True),
        },
        "independent_ref_pushes": {
            "clients": independent_count,
            "successful": independent_count,
            "rejected": 0,
            "unexpected_failures": 0,
            **team_summary(),
            "results": team_results(independent_count, ["accepted"] * independent_count),
        },
        "same_ref_pushes": {
            "clients": contended_count,
            "successful": 1,
            "rejected": contended_count - 1,
            "unexpected_failures": 0,
            **team_summary(),
            "results": team_results(
                contended_count,
                ["accepted", *(["non_fast_forward"] * (contended_count - 1))],
            ),
        },
    }


def valid_cache_service() -> dict[str, Any]:
    return {
        "configured": True,
        "required": True,
        "url": "http://127.0.0.1:19002",
        "health_status": 200,
        "capabilities_status": 200,
        "capabilities_schema": "crab-cache-service.capabilities.v1",
        "route_schema": "crab-cache-service.routes.v3",
        "stats": {
            "status": 200,
            "error": None,
            "pack": {
                "cache_hits": 1,
                "cache_misses": 1,
                "origin_fetches": 1,
                "origin_head_requests": 1,
                "bytes_served_from_cache": 10,
                "bytes_served_from_origin": 10,
                "bytes_served_total": 20,
                "push_warming_writes": 1,
                "push_warming_bytes": 10,
                "read_requests": 3,
            },
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

    def test_telemetry_parser_accepts_debug_enum_cache_events(self) -> None:
        path = self.root / "stderr.log"
        path.write_text(
            "\n".join(
                json.dumps({"fields": {"cache_event": event}})
                for event in ("Hit", "Miss", "hit", "miss")
            ),
            encoding="utf-8",
        )
        qualification = QUALIFICATION.LargeRepositoryQualification.__new__(
            QUALIFICATION.LargeRepositoryQualification
        )

        parsed = qualification.telemetry_from_log(path)

        self.assertEqual(parsed["cache_hits"], 2)
        self.assertEqual(parsed["cache_misses"], 2)

    def test_protocol_telemetry_parser_accepts_debug_enum_cache_events(self) -> None:
        logs = self.root / "logs"
        logs.mkdir()
        (logs / "client.stderr.log").write_text(
            "\n".join(
                json.dumps({"fields": {"cache_event": event}})
                for event in ("Hit", "Miss", "hit", "miss")
            ),
            encoding="utf-8",
        )
        smoke = PROTOCOL.ProtocolV2PartialCloneSmoke.__new__(
            PROTOCOL.ProtocolV2PartialCloneSmoke
        )
        smoke.logs = logs

        parsed = smoke.storage_telemetry()

        self.assertEqual(parsed["cache_hits"], 2)
        self.assertEqual(parsed["cache_misses"], 2)

    def test_valid_smoke_report_is_accepted_explicitly(self) -> None:
        result = VERIFY.verify_report(self.write("report.json", valid_report()), allow_smoke=True)
        self.assertEqual(result.replay_count, 3)

    def test_release_team_load_contract_requires_all_scenarios(self) -> None:
        VERIFY.verify_team_load(valid_team_load(), require_release_counts=True)

    def test_required_cache_service_contract(self) -> None:
        report = valid_report()
        report["cache_service"] = valid_cache_service()
        report["checks"].extend(
            {
                "name": name,
                "ok": True,
            }
            for name in (
                "cache-service-configured",
                "cache-service-healthy",
                "cache-service-capabilities",
                "cache-service-admin-stats",
                "cache-service-pack-traffic",
            )
        )
        result = VERIFY.verify_report(
            self.write("report.json", report),
            allow_smoke=True,
            require_cache_service=True,
        )
        self.assertEqual(result.replay_count, 3)

    def test_required_cache_service_contract_rejects_missing_pack_traffic(self) -> None:
        report = valid_report()
        report["cache_service"] = valid_cache_service()
        report["cache_service"]["stats"]["pack"]["read_requests"] = 0
        with self.assertRaisesRegex(VERIFY.VerificationError, "no Git pack read traffic"):
            VERIFY.verify_report(
                self.write("report.json", report),
                allow_smoke=True,
                require_cache_service=True,
            )

    def test_team_load_missing_latency_field_is_rejected(self) -> None:
        report = valid_team_load()
        del report["same_ref_pushes"]["p95_client_ms"]
        with self.assertRaisesRegex(VERIFY.VerificationError, "p95_client_ms"):
            VERIFY.verify_team_load(report)

    def test_fetch_seed_failure_is_visible_in_result_contract(self) -> None:
        report = valid_team_load()
        report["fetch_seed"]["results"] = team_results(
            100,
            ["clone_failed", *(["ok"] * 99)],
        )
        with self.assertRaisesRegex(VERIFY.VerificationError, "clone_failed"):
            VERIFY.verify_team_load(report)

    def test_prevalidated_owner_path_allows_no_remote_visibility_build(self) -> None:
        VERIFY.verify_full_visibility_telemetry(
            {
                "visibility_owner_seed": {
                    "actions": [
                        "catalog_advance",
                        "commit_graph_incremental",
                        "commit_graph_rebuild",
                        "none",
                    ],
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

    def test_catalog_visibility_handoff_allows_metadata_only_proof(self) -> None:
        VERIFY.verify_full_visibility_telemetry(
            {
                "visibility_owner_seed": {
                    "actions": ["catalog_visibility_handoff", "none"],
                    "visibility_states": ["published", "published"],
                    "telemetry": {
                        "visibility_duration_ms": 0,
                        "storage_requests": 0,
                        "storage_bytes": 0,
                    },
                }
            }
        )

    def test_blobless_catalog_filter_requires_ordinal_metadata_telemetry(self) -> None:
        report = valid_report()
        report["stages"]["blob_none_clone"]["telemetry"]["locator_ordinal_metadata"] = 0
        with self.assertRaisesRegex(VERIFY.VerificationError, "ordinal metadata"):
            VERIFY.verify_catalog_filter_telemetry(report["stages"])

    def test_full_owner_report_requires_locator_sweep_telemetry(self) -> None:
        stages = {
            "visibility_owner_seed": {
                "locator_sweep": [
                    {
                        "action": "none",
                        "object_rows_scanned": 0,
                        "object_rows_deleted": 0,
                        "pack_rows_scanned": 0,
                        "pack_rows_deleted": 0,
                    }
                ]
            }
        }
        VERIFY.verify_locator_sweep_telemetry(stages)
        del stages["visibility_owner_seed"]["locator_sweep"]
        with self.assertRaisesRegex(VERIFY.VerificationError, "locator sweep telemetry"):
            VERIFY.verify_locator_sweep_telemetry(stages)

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

    def test_missing_clone_ref_evidence_is_rejected(self) -> None:
        report = valid_report()
        del report["correctness"]["clone_refs"]
        with self.assertRaisesRegex(VERIFY.VerificationError, "clone_refs"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)

    def test_clone_ref_drift_is_rejected(self) -> None:
        report = valid_report()
        report["correctness"]["clone_refs"]["refs/heads/main"] = BASE
        with self.assertRaisesRegex(VERIFY.VerificationError, "clone advertised refs"):
            VERIFY.verify_report(self.write("report.json", report), allow_smoke=True)


if __name__ == "__main__":
    unittest.main()
