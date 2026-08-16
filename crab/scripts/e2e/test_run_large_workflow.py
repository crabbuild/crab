#!/usr/bin/env python3
"""Validate large-workflow fixture mirroring helpers."""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import run_large_workflow as workflow


def mirror_copy_preserves_source_metadata() -> None:
    with tempfile.TemporaryDirectory() as temporary_directory:
        root = Path(temporary_directory)
        source = root / "source.bin"
        target = root / "target.bin"
        source.write_bytes(b"dense fixture payload")
        before = source.stat()

        strategy = workflow.copy_fixture_file(source, target)

        after = source.stat()
        if target.read_bytes() != source.read_bytes():
            raise AssertionError("mirrored fixture content changed")
        if target.stat().st_ino == source.stat().st_ino:
            raise AssertionError("fixture mirror must not hard-link the source")
        if after.st_ctime_ns != before.st_ctime_ns:
            raise AssertionError("fixture mirror changed source ctime")
        if strategy not in {"cloned", "copied"}:
            raise AssertionError(f"unexpected mirror strategy: {strategy}")


def command_budget_requires_duration_and_rss() -> None:
    record = workflow.CommandRecord(
        name="dense add",
        args=["crab", "add"],
        cwd="/tmp/repo",
        stdout_log="/tmp/stdout",
        stderr_log="/tmp/stderr",
        started_at="2026-07-19T00:00:00+00:00",
        duration_ms=49_999,
        exit_code=0,
        resource_usage={"max_resident_set_size": 512 * 1024 * 1024},
    )

    ok, detail = workflow.evaluate_command_budget(
        record,
        max_duration_ms=50_000,
        max_rss_bytes=512 * 1024 * 1024,
    )
    if not ok:
        raise AssertionError(f"command at both limits should pass: {detail}")

    record.resource_usage = {}
    ok, detail = workflow.evaluate_command_budget(
        record,
        max_duration_ms=50_000,
        max_rss_bytes=512 * 1024 * 1024,
    )
    if ok or detail.get("rss_bytes") is not None:
        raise AssertionError("missing RSS must fail an RSS-enforced budget")


def phase_budget_rejects_missing_or_slow_phase() -> None:
    fast = [{"data": {"operation": "push", "phase": "lookup", "elapsed_ms": 4_999}}]
    ok, detail = workflow.evaluate_phase_budget(
        fast,
        operation="push",
        phase="lookup",
        max_duration_ms=5_000,
    )
    if not ok or detail.get("elapsed_ms") != 4_999:
        raise AssertionError(f"fast phase should pass: {detail}")

    slow = [{"data": {"operation": "push", "phase": "lookup", "elapsed_ms": 5_001}}]
    ok, _ = workflow.evaluate_phase_budget(
        slow,
        operation="push",
        phase="lookup",
        max_duration_ms=5_000,
    )
    if ok:
        raise AssertionError("slow phase must fail")

    ok, detail = workflow.evaluate_phase_budget(
        [],
        operation="push",
        phase="lookup",
        max_duration_ms=5_000,
    )
    if ok or detail.get("elapsed_ms") is not None:
        raise AssertionError("missing phase instrumentation must fail")


def main() -> int:
    mirror_copy_preserves_source_metadata()
    command_budget_requires_duration_and_rss()
    phase_budget_rejects_missing_or_slow_phase()
    print("PASS large-workflow fixture mirroring")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
