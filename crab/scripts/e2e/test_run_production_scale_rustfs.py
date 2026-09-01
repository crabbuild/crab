#!/usr/bin/env python3
"""Tests for the production-scale RustFS workload contract."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parent / "run_production_scale_rustfs.py"
SPEC = importlib.util.spec_from_file_location("run_production_scale_rustfs", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
QUALIFICATION = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = QUALIFICATION
SPEC.loader.exec_module(QUALIFICATION)


class ProductionScaleWorkloadTests(unittest.TestCase):
    def test_plan_is_exactly_300_gib_across_six_versioned_size_classes(self) -> None:
        plan = QUALIFICATION.build_workload_plan()

        self.assertEqual(plan.logical_bytes, 300 * QUALIFICATION.GIB)
        self.assertEqual(len(plan.files), 897)
        self.assertEqual(
            [target.size for target in plan.version_targets],
            [
                100 * QUALIFICATION.MIB,
                256 * QUALIFICATION.MIB,
                512 * QUALIFICATION.MIB,
                QUALIFICATION.GIB,
                2 * QUALIFICATION.GIB,
                5 * QUALIFICATION.GIB,
            ],
        )


if __name__ == "__main__":
    unittest.main()
