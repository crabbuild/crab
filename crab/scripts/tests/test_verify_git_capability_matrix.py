#!/usr/bin/env python3
"""Contract tests for the Git capability matrix verifier."""

from __future__ import annotations

import copy
import importlib.util
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "verify_git_capability_matrix.py"
SPEC = importlib.util.spec_from_file_location("verify_git_capability_matrix", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MATRIX = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MATRIX)


class CapabilityMatrixTest(unittest.TestCase):
    def setUp(self) -> None:
        self.matrix = MATRIX.load_matrix()
        self.report = {
            "schema": "crab.protocol-v2-partial-clone-smoke",
            "version": "1.1",
            "status": "passed",
            "updated_at": datetime.now(timezone.utc).isoformat(),
            "backend": "rustfs",
            "provenance": {
                "backend": "rustfs",
                "operating_system": "linux",
                "repository_mode": "direct",
                "git_version": "git version 2.45.4",
                "crab_binary_sha256": "a" * 64,
                "crab_source_revision": "b" * 40,
                "crab_source_checkout_clean": True,
                "crab_binary_matches_source_revision": True,
                "rollback_crab_tag": "v1.0.1",
                "rollback_binary_sha256": "c" * 64,
            },
            "checks": [
                {"name": name, "ok": True, "detail": {}}
                for name in sorted({name for names in self.matrix["evidence_checks"].values() for name in names})
            ],
        }

    def validate_report(self, report: dict) -> None:
        MATRIX.validate_report(
            self.matrix, report, "linux-rustfs-release", "2.45.4", "git version 2.45.4",
            "a" * 64, "b" * 40, "c" * 64,
        )

    def test_complete_exact_binary_report_passes(self) -> None:
        self.validate_report(self.report)

    def test_missing_check_is_not_inferred_from_runner_source(self) -> None:
        self.report["checks"].pop()
        with self.assertRaisesRegex(ValueError, "missing required evidence"):
            self.validate_report(self.report)

    def test_incorrect_provenance_is_rejected(self) -> None:
        for field, value in (
            ("backend", "s3"), ("operating_system", "macos"), ("repository_mode", "managed"),
            ("git_version", "git version 2.30.9"), ("crab_binary_sha256", "d" * 64),
            ("crab_source_revision", "e" * 40), ("crab_source_checkout_clean", False),
            ("crab_binary_matches_source_revision", False), ("rollback_crab_tag", None),
            ("rollback_binary_sha256", None),
        ):
            with self.subTest(field=field):
                report = copy.deepcopy(self.report)
                report["provenance"][field] = value
                with self.assertRaises(ValueError):
                    self.validate_report(report)

    def test_skipped_failed_or_duplicate_check_is_rejected(self) -> None:
        for scenario in ("skipped", "failed", "duplicate"):
            with self.subTest(scenario=scenario):
                report = copy.deepcopy(self.report)
                if scenario == "skipped":
                    report["checks"][0]["detail"]["skipped"] = True
                elif scenario == "failed":
                    report["checks"][0]["ok"] = False
                else:
                    report["checks"].append(report["checks"][0])
                with self.assertRaises(ValueError):
                    self.validate_report(report)

    def test_supported_client_filters_require_actual_check_results(self) -> None:
        self.report["performance"] = {
            "filter-matrix": {"client_capabilities": {"object_type_filter": True}}
        }
        with self.assertRaisesRegex(ValueError, "missing supported client filter"):
            self.validate_report(self.report)

    def test_stale_report_is_rejected(self) -> None:
        self.report["updated_at"] = (datetime.now(timezone.utc) - timedelta(days=2)).isoformat()
        with self.assertRaisesRegex(ValueError, "stale"):
            self.validate_report(self.report)

    def test_repository_matrix_is_complete(self) -> None:
        MATRIX.validate(MATRIX.load_matrix())

    def test_missing_cell_is_rejected(self) -> None:
        matrix = copy.deepcopy(MATRIX.load_matrix())
        matrix["profiles"][0]["operations"].remove("rollback_client")

        with self.assertRaisesRegex(ValueError, "capability cells have no explicit status"):
            MATRIX.validate(matrix)

    def test_overlapping_cell_is_rejected(self) -> None:
        matrix = copy.deepcopy(MATRIX.load_matrix())
        duplicate = copy.deepcopy(matrix["profiles"][0])
        duplicate["id"] = "duplicate"
        matrix["profiles"].append(duplicate)

        with self.assertRaisesRegex(ValueError, "appears in both"):
            MATRIX.validate(matrix)

    def test_conflicting_protocol_status_is_rejected(self) -> None:
        matrix = copy.deepcopy(MATRIX.load_matrix())
        matrix["protocol_profile"]["unsupported"].append("protocol_v2_fetch")

        with self.assertRaisesRegex(ValueError, "conflicting status"):
            MATRIX.validate(matrix)


if __name__ == "__main__":
    unittest.main()
