#!/usr/bin/env python3
"""Tests for versioned Xet qualification transport evidence."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_add_push_scale_rustfs as scale
from run_add_push_scale_rustfs import measured_read, verify, write_transport_report


class CapacityPreflightTests(unittest.TestCase):
    def test_rejects_the_previous_40_gib_run_capacity_level(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            run_root = root / "run"
            run_root.mkdir()
            checks: list[tuple[str, bool]] = []

            def record_check(name: str, ok: bool, _detail: dict | None = None) -> None:
                checks.append((name, ok))
                if name == "disk-capacity":
                    raise StopIteration

            runner = SimpleNamespace(
                run_root=run_root,
                env={},
                signed_s3_request=Mock(return_value=(404, None, None)),
                preflight=Mock(),
                check=record_check,
            )
            args = SimpleNamespace(root=root, files=20, file_mib=2048, versions=3)
            with patch(
                "run_add_push_scale_rustfs.shutil.disk_usage",
                return_value=SimpleNamespace(free=153 * 1024**3),
            ):
                with self.assertRaises(StopIteration):
                    verify(args, runner, Mock(), [], [])

            self.assertEqual(checks[-1], ("disk-capacity", False))

    def test_releases_verified_output_inside_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "history-cache-0"
            cache.mkdir()
            (cache / "entry").write_bytes(b"cached")

            scale.release_verified_run_child(SimpleNamespace(run_root=root), cache)

            self.assertFalse(cache.exists())

    def test_refuses_to_release_output_outside_the_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "unrelated" / "cache"
            cache.mkdir(parents=True)

            with self.assertRaises(ValueError):
                scale.release_verified_run_child(SimpleNamespace(run_root=root / "run"), cache)

            self.assertTrue(cache.exists())

    def test_refuses_to_release_symlink_to_outside_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outside = root / "outside"
            outside.mkdir()
            run_root = root / "run"
            run_root.mkdir()
            link = run_root / "linked"
            link.symlink_to(outside, target_is_directory=True)

            with self.assertRaises(ValueError):
                scale.release_verified_run_child(SimpleNamespace(run_root=run_root), link)

            self.assertTrue(outside.exists())

    def test_refuses_to_release_parent_path_alias(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            run_root = root / "run"
            run_root.mkdir()

            with self.assertRaises(ValueError):
                scale.release_verified_run_child(SimpleNamespace(run_root=run_root), run_root / "..")

            self.assertTrue(run_root.exists())

    def test_hydration_rechecks_headroom_before_starting(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def reject_low_capacity(_name: str, ok: bool, _detail: dict) -> None:
                if not ok:
                    raise StopIteration

            runner = SimpleNamespace(
                args=SimpleNamespace(root=root),
                check=reject_low_capacity,
                run_crab=Mock(side_effect=AssertionError("hydrate ran without capacity")),
            )
            with patch(
                "run_add_push_scale_rustfs.shutil.disk_usage",
                return_value=SimpleNamespace(free=29 * 1024**3),
            ):
                with self.assertRaises(StopIteration):
                    scale.measured_hydrate(
                        runner, Mock(), [], root, "cold hydrate",
                        20 * 1024**3, 10 * 1024**3,
                    )


class ReadPhaseEvidenceTests(unittest.TestCase):
    def test_proxy_error_fails_qualification(self) -> None:
        def reject_error(_name: str, ok: bool, _detail: dict) -> None:
            if not ok:
                raise StopIteration

        runner = SimpleNamespace(check=reject_error)
        proxy = SimpleNamespace(snapshot=Mock(return_value={"proxy_errors": {"TimeoutError": 1}}))

        with self.assertRaises(StopIteration):
            scale.verify_no_proxy_errors(runner, proxy)

    def test_clean_proxy_allows_qualification(self) -> None:
        runner = SimpleNamespace(check=Mock())
        proxy = SimpleNamespace(snapshot=Mock(return_value={"proxy_errors": {}}))

        scale.verify_no_proxy_errors(runner, proxy)

        runner.check.assert_called_once_with("request-meter-no-proxy-errors", True, {"proxy_errors": {}})

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
