#!/usr/bin/env python3
"""Tests for versioned Xet qualification transport evidence."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, call

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_add_push_scale_rustfs import measured_read, write_transport_report


class ReadPhaseEvidenceTests(unittest.TestCase):
    def test_hydration_records_only_its_own_origin_traffic(self) -> None:
        before = {"requests": 10, "response_body_bytes": 100, "methods": {"GET": 10}}
        after = {"requests": 13, "response_body_bytes": 900, "methods": {"GET": 13}}
        proxy = SimpleNamespace(snapshot=Mock(side_effect=[before, after]))
        command = SimpleNamespace(duration_ms=123)
        runner = SimpleNamespace(run_crab=Mock(return_value=command))
        phases: list[dict] = []

        result = measured_read(runner, proxy, phases, Path("clone"),
                               ["hydrate", "--all"], "cold hydrate")

        self.assertEqual((result, phases), (command, [{
            "name": "cold hydrate",
            "duration_ms": 123,
            "transport": {
                "requests": 3,
                "response_body_bytes": 800,
                "request_body_bytes": 0,
                "methods": {"GET": 3},
                "operations": {}, "categories": {}, "classes": {},
                "statuses": {}, "proxy_errors": {},
            },
        }]))

    def test_transport_report_retains_phase_and_total_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory)
            runner = SimpleNamespace(
                artifacts=artifacts,
                report=SimpleNamespace(artifacts={}),
                write_report=Mock(),
            )
            phases = [{"name": "cold hydrate", "transport": {"requests": 3}}]

            write_transport_report(runner, [{"version": 0}], phases, {"requests": 7})

            report = json.loads((artifacts / "capsule-xet-transport.json").read_text())
            self.assertEqual((report, runner.write_report.call_args_list), ({
                "versions": [{"version": 0}],
                "read_phases": phases,
                "total": {"requests": 7},
            }, [call()]))


if __name__ == "__main__":
    unittest.main()
