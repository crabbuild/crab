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

    def test_warm_complete_xet_ranges_are_not_counted_as_future_cache_growth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "cache"
            cache.mkdir()
            stats = SimpleNamespace()
            hydrate = SimpleNamespace(duration_ms=123)
            checks: list[tuple[str, bool, dict]] = []

            def require_capacity(name: str, ok: bool, detail: dict) -> None:
                checks.append((name, ok, detail))
                if not ok:
                    raise StopIteration((name, detail))

            runner = SimpleNamespace(
                args=SimpleNamespace(root=Path(directory)),
                env={"CRAB_CACHE_DIR": str(cache)},
                run_crab=Mock(side_effect=[stats, hydrate]),
                read_stdout=Mock(return_value=json.dumps({"data": {
                    "scan_complete": True,
                    "families": {"decoded-range": {
                        "allocated_bytes": 20 * 1024**3,
                        "complete": True,
                        "issues": 0,
                    }},
                }})),
                check=require_capacity,
            )
            proxy = SimpleNamespace(snapshot=Mock(side_effect=[{}, {}]))
            phases: list[dict] = []

            with patch(
                "run_add_push_scale_rustfs.shutil.disk_usage",
                return_value=SimpleNamespace(free=130 * 1024**3),
            ):
                result = scale.measured_hydrate(
                    runner, proxy, phases, Path(directory), "rehydrated hydrate",
                    100 * 1024**3, 20 * 1024**3,
                )

            self.assertIs(result, hydrate)
            self.assertEqual(runner.run_crab.call_count, 2)
            self.assertEqual(checks, [("rehydrated hydrate capacity", True, {
                "required_bytes": 120 * 1024**3,
                "cache_resident_bytes": 20 * 1024**3,
                "cache_growth_bytes": 0,
                "available_bytes": 130 * 1024**3,
            })])

    def test_incomplete_cache_inventory_receives_no_capacity_credit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "cache"
            cache.mkdir()

            def reject_low_capacity(name: str, ok: bool, detail: dict) -> None:
                if not ok:
                    raise StopIteration((name, detail))

            runner = SimpleNamespace(
                args=SimpleNamespace(root=Path(directory)),
                env={"CRAB_CACHE_DIR": str(cache)},
                run_crab=Mock(return_value=SimpleNamespace()),
                read_stdout=Mock(return_value=json.dumps({"data": {
                    "scan_complete": False,
                    "families": {"decoded-range": {
                        "allocated_bytes": 20 * 1024**3,
                        "complete": True,
                        "issues": 0,
                    }},
                }})),
                check=reject_low_capacity,
            )

            with patch(
                "run_add_push_scale_rustfs.shutil.disk_usage",
                return_value=SimpleNamespace(free=130 * 1024**3),
            ):
                with self.assertRaises(StopIteration) as failure:
                    scale.measured_hydrate(
                        runner, Mock(), [], Path(directory), "rehydrated hydrate",
                        100 * 1024**3, 20 * 1024**3,
                    )

            self.assertEqual(failure.exception.args[0][1]["required_bytes"], 140 * 1024**3)
            self.assertEqual(runner.run_crab.call_count, 1)


class CapacityStopResumeTests(unittest.TestCase):
    def make_capacity_stop(self, root: Path) -> tuple[SimpleNamespace, Path]:
        run_id = "xet-capacity-stop"
        run_root = root / run_id
        artifacts = run_root / "artifacts"
        artifacts.mkdir(parents=True)
        clone = run_root / "clone"
        (clone / ".git").mkdir(parents=True)
        repo = run_root / "scale" / "repo"
        (repo / ".git").mkdir(parents=True)
        binary = root / "crab"
        binary.write_bytes(b"frozen Crab binary")
        history = artifacts / "expected-history-sha256.json"
        files = {
            **{f"models/model-{index:03}.bin": "a" * 64 for index in range(50)},
            **{f"src/module_{index:04}.rs": "b" * 64 for index in range(500)},
        }
        snapshots = [
            {
                "version": version,
                "commit": f"{version + 1:040x}",
                "generation": version,
                "digest": f"{version + 1:064x}",
                "files": files,
            }
            for version in range(3)
        ]
        history.write_text(json.dumps(snapshots) + "\n")
        expected = artifacts / "expected-sha256.json"
        expected.write_text(json.dumps(files) + "\n")
        transport = artifacts / "capsule-xet-transport.json"
        transport.write_text(json.dumps({"versions": [], "read_phases": [], "total": {"proxy_errors": {}}}))
        report_path = artifacts / "report.json"
        report_path.write_text(json.dumps({
            "run_id": run_id,
            "root": str(run_root),
            "endpoint_url": "http://127.0.0.1:19118",
            "bucket": "crab-xet-test",
            "status": "failed",
            "commands": [{"name": "cold dehydrate", "cwd": str(clone), "exit_code": 0}],
            "checks": [
                {
                    "name": "workload-shape",
                    "ok": True,
                    "detail": {
                        "large_files": 50,
                        "logical_bytes": 50 * 2048 * scale.MIB,
                        "small_code_files": 500,
                        "versions": 3,
                    },
                },
                {"name": "cold bytes verified", "ok": True},
                {"name": "rehydrated hydrate capacity", "ok": False},
            ],
            "artifacts": {
                "crab_binary": str(binary),
                "crab_binary_sha256": scale.sha256_file(binary),
                "source_head_sha": "c" * 40,
                "failure": "check failed: rehydrated hydrate capacity",
                "expected_history": str(history),
                "capsule_xet_transport": str(transport),
            },
        }))
        args = SimpleNamespace(
            root=root,
            run_id=run_id,
            endpoint_url="http://127.0.0.1:19118",
            bucket="crab-xet-test",
            crab_bin=str(binary),
            files=50,
            file_mib=2048,
            code_files=500,
            versions=3,
        )
        return args, report_path

    def test_resume_accepts_only_the_terminal_rehydrated_capacity_preflight(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, report_path = self.make_capacity_stop(Path(directory))
            before = report_path.read_bytes()

            report, report_sha256 = scale.validate_capacity_stop_report(args)

            self.assertEqual(report["run_id"], args.run_id)
            self.assertEqual(report_sha256, scale.hashlib.sha256(before).hexdigest())
            self.assertEqual(report_path.read_bytes(), before)

    def test_resume_rejects_a_different_bucket(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, _ = self.make_capacity_stop(Path(directory))
            args.bucket = "other-bucket"

            with self.assertRaisesRegex(ValueError, "bucket"):
                scale.validate_capacity_stop_report(args)

    def test_resume_rejects_a_different_terminal_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, report_path = self.make_capacity_stop(Path(directory))
            report = json.loads(report_path.read_text())
            report["checks"][-1]["name"] = "rehydrated hydrate bytes"
            report_path.write_text(json.dumps(report))

            with self.assertRaisesRegex(ValueError, "capacity stop"):
                scale.validate_capacity_stop_report(args)

    def test_resume_rejects_a_changed_frozen_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, _ = self.make_capacity_stop(Path(directory))
            Path(args.crab_bin).write_bytes(b"changed binary")

            with self.assertRaisesRegex(ValueError, "binary identity"):
                scale.validate_capacity_stop_report(args)

    def test_resume_rejects_a_prior_failed_check(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, report_path = self.make_capacity_stop(Path(directory))
            report = json.loads(report_path.read_text())
            report["checks"][1]["ok"] = False
            report_path.write_text(json.dumps(report))

            with self.assertRaisesRegex(ValueError, "capacity stop"):
                scale.validate_capacity_stop_report(args)

    def test_resume_stops_before_writing_when_history_capacity_is_insufficient(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args, report_path = self.make_capacity_stop(Path(directory))
            before = report_path.read_bytes()

            with patch(
                "run_add_push_scale_rustfs.shutil.disk_usage",
                return_value=SimpleNamespace(free=130 * 1024**3),
            ):
                with self.assertRaisesRegex(ValueError, "isolated history hydration"):
                    scale.resume_capacity_stop(args)

            self.assertEqual(report_path.read_bytes(), before)
            self.assertEqual(list((Path(directory) / args.run_id).glob("resume-*")), [])

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
