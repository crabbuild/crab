#!/usr/bin/env python3
"""Run bounded no-cloud integration checks across the split crate seams."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class CargoTestCheck:
    label: str
    args: tuple[str, ...]


@dataclass(frozen=True)
class HelperDoctorCheck:
    label: str
    package: str
    binary: str


INTEGRATION_TESTS = (
    CargoTestCheck(
        "CLI pointer round trip through crab",
        (
            "test",
            "-p",
            "crab",
            "--test",
            "integration",
            "happy_path_init_track_clean_pointer_roundtrip",
        ),
    ),
    CargoTestCheck(
        "cache server cache-miss/cache-hit flow through CacheClient",
        (
            "test",
            "-p",
            "crab-cache-server",
            "--test",
            "cache_service_integration",
            "test_cache_miss_fetches_and_caches",
        ),
    ),
    CargoTestCheck(
        "cache server shard/xorb ingestion plus dedup query",
        (
            "test",
            "-p",
            "crab-cache-server",
            "--test",
            "cache_service_integration",
            "test_dedup_query_after_shard_and_xorb_ingestion",
        ),
    ),
    CargoTestCheck(
        "auth receive Module reads staged Git pack",
        (
            "test",
            "-p",
            "crab-auth-server",
            "--lib",
            "compute_changed_paths_reads_staged_git_pack",
        ),
    ),
    CargoTestCheck(
        "auth view materializes allowed Crab content",
        (
            "test",
            "-p",
            "crab-auth-server",
            "--lib",
            "build_filtered_view_keeps_allowed_crab_content_view_local",
        ),
    ),
)


HELPER_DOCTORS = (
    HelperDoctorCheck("auth receive helper doctor", "crab-auth-server", "crab-auth-receive"),
    HelperDoctorCheck("auth view helper doctor", "crab-auth-server", "crab-auth-view"),
)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run(command: list[str], root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )


def test_ran(output: str) -> bool:
    return any(
        line.startswith("running ") and not line.startswith("running 0 tests")
        for line in output.splitlines()
    )


def run_cargo_test(root: Path, cargo: str, check: CargoTestCheck) -> bool:
    command = [cargo, *check.args]
    print(f"testing: {check.label}")
    result = run(command, root)
    output = result.stdout + result.stderr
    if result.returncode == 0 and test_ran(output):
        print(f"ok: {check.label}")
        return True

    print(f"error: {check.label} failed:", file=sys.stderr)
    print("  command:", " ".join(command), file=sys.stderr)
    if result.returncode == 0:
        print("  matched tests: none", file=sys.stderr)
    print(output, file=sys.stderr)
    return False


def run_helper_doctor(root: Path, cargo: str, check: HelperDoctorCheck) -> bool:
    command = [
        cargo,
        "run",
        "--quiet",
        "-p",
        check.package,
        "--bin",
        check.binary,
        "--",
        "doctor",
    ]
    print(f"testing: {check.label}")
    result = run(command, root)
    if result.returncode == 0:
        try:
            payload = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            payload = {"json_error": str(error)}
        if (
            payload.get("status") == "ok"
            and isinstance(payload.get("git_version"), str)
            and payload["git_version"].startswith("git version ")
        ):
            print(f"ok: {check.label}")
            return True

    print(f"error: {check.label} failed:", file=sys.stderr)
    print("  command:", " ".join(command), file=sys.stderr)
    print(f"  stdout: {result.stdout.strip()!r}", file=sys.stderr)
    if result.stderr.strip():
        print("  stderr:", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run no-cloud final integration checks used by architecture CI.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for final integration checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    for check in INTEGRATION_TESTS:
        if not run_cargo_test(root, args.cargo, check):
            return 1

    for check in HELPER_DOCTORS:
        if not run_helper_doctor(root, args.cargo, check):
            return 1

    total = len(INTEGRATION_TESTS) + len(HELPER_DOCTORS)
    print(f"ok: {total} final integration checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
