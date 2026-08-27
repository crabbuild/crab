#!/usr/bin/env python3
"""Validate shipped binary `--version` output after package moves."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class BinaryVersionCheck:
    label: str
    package: str
    binary: str


CHECKS = (
    BinaryVersionCheck("Crab CLI", "crab", "crab"),
    BinaryVersionCheck("Crab Auth receive helper", "crab-auth-server", "crab-auth-receive"),
    BinaryVersionCheck("Crab Auth view helper", "crab-auth-server", "crab-auth-view"),
    BinaryVersionCheck("Crab cache server", "crab-cache-server", "crab-cache-server"),
)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[3]


def run(args: list[str], root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )


def product_version(root: Path, cargo: str) -> str:
    result = run([cargo, "metadata", "--format-version", "1", "--no-deps"], root)
    if result.returncode != 0:
        raise RuntimeError(result.stderr or result.stdout)
    metadata = json.loads(result.stdout)
    for package in metadata["packages"]:
        if package["name"] == "crab":
            return package["version"]
    raise RuntimeError("cargo metadata did not include crab package")


def check_binary(root: Path, cargo: str, version: str, check: BinaryVersionCheck) -> bool:
    command = [
        cargo,
        "run",
        "--quiet",
        "-p",
        check.package,
        "--bin",
        check.binary,
        "--",
        "--version",
    ]
    result = run(command, root)
    expected = f"{check.binary} {version}"
    actual = result.stdout.strip()

    if result.returncode == 0 and actual == expected:
        print(f"ok: {check.label} reports {expected}")
        return True

    print(f"error: {check.label} version contract drifted:", file=sys.stderr)
    print("  command:", " ".join(command), file=sys.stderr)
    print(f"  expected stdout: {expected!r}", file=sys.stderr)
    print(f"  actual stdout: {actual!r}", file=sys.stderr)
    if result.stderr.strip():
        print("  stderr:", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run shipped binary --version contract checks.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for binary version checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    try:
        version = product_version(root, args.cargo)
    except RuntimeError as error:
        print("error: cargo metadata failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return 1

    for check in CHECKS:
        if not check_binary(root, args.cargo, version, check):
            return 1

    print(f"ok: {len(CHECKS)} shipped binaries report product version {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
