#!/usr/bin/env python3
"""Compile split-crate interface slices for the multi-crate architecture."""

from __future__ import annotations

import argparse
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Check:
    label: str
    args: tuple[str, ...]


CHECKS = (
    Check("crab-types default Interface", ("check", "-p", "crab-types", "--all-targets")),
    Check("crab-git default Interface", ("check", "-p", "crab-git", "--all-targets")),
    Check("crab-lfs default Interface", ("check", "-p", "crab-lfs", "--all-targets")),
    Check("crab-diff default Interface", ("check", "-p", "crab-diff", "--all-targets")),
    Check("crab-workflow default Interface", ("check", "-p", "crab-workflow", "--all-targets")),
    Check("crab-storage default Interface", ("check", "-p", "crab-storage", "--all-targets")),
    Check("crab-read default Interface", ("check", "-p", "crab-read", "--all-targets")),
    Check("crab-staging default Interface", ("check", "-p", "crab-staging", "--all-targets")),
    Check("crab-xet default Interface", ("check", "-p", "crab-xet", "--all-targets")),
    Check(
        "crab-xet chunker Interface",
        ("check", "-p", "crab-xet", "--all-targets", "--features", "chunker"),
    ),
    Check(
        "crab-xet upload concurrency Interface",
        (
            "check",
            "-p",
            "crab-xet",
            "--all-targets",
            "--features",
            "upload-concurrency",
        ),
    ),
    Check("crab-auth default Interface", ("check", "-p", "crab-auth", "--all-targets")),
    Check(
        "crab-auth client-provider Interfaces",
        (
            "check",
            "-p",
            "crab-auth",
            "--all-targets",
            "--features",
            "oidc-client,crab-auth-client,aws-oidc-client,gcp-workload-identity-client,azure-entra-client",
        ),
    ),
    Check(
        "crab-auth-store default Interface",
        ("check", "-p", "crab-auth-store", "--all-targets"),
    ),
    Check(
        "crab-auth-store refresh Adapter Interface",
        ("check", "-p", "crab-auth-store", "--all-targets", "--features", "refreshing-store"),
    ),
    Check("crab-cache default Interface", ("check", "-p", "crab-cache", "--all-targets")),
    Check(
        "crab-cache explicit feature Interfaces",
        (
            "check",
            "-p",
            "crab-cache",
            "--all-targets",
            "--features",
            "local-cache,remote-client,xet-chunk-cache",
        ),
    ),
    Check(
        "crab-cache-store local Adapter Interface",
        ("check", "-p", "crab-cache-store", "--all-targets", "--no-default-features"),
    ),
    Check(
        "crab-cache-store remote Adapter Interface",
        (
            "check",
            "-p",
            "crab-cache-store",
            "--all-targets",
            "--no-default-features",
            "--features",
            "remote-client",
        ),
    ),
    Check(
        "crab-coordination default Interface",
        ("check", "-p", "crab-coordination", "--all-targets"),
    ),
    Check(
        "crab-coordination provider Adapter Interfaces",
        (
            "check",
            "-p",
            "crab-coordination",
            "--all-targets",
            "--features",
            "coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb",
        ),
    ),
    Check("crab-metadata default Interface", ("check", "-p", "crab-metadata", "--all-targets")),
    Check(
        "crab-metadata storage/index Interfaces",
        (
            "check",
            "-p",
            "crab-metadata",
            "--all-targets",
            "--features",
            "local-index,file-index-reader,remote-index,storage",
        ),
    ),
    Check(
        "crab-auth-server package Interface",
        ("check", "-p", "crab-auth-server", "--all-targets"),
    ),
    Check(
        "crab-cache-server package Interface",
        ("check", "-p", "crab-cache-server", "--all-targets"),
    ),
    Check(
        "crab-lfs-server package Interface",
        ("check", "-p", "crab-lfs-server", "--all-targets"),
    ),
)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run_check(root: Path, cargo: str, check: Check) -> bool:
    command = [cargo, *check.args]
    print(f"checking: {check.label}")
    result = subprocess.run(
        command,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if result.returncode == 0:
        print(f"ok: {check.label}")
        return True

    print(f"error: {check.label} failed:", file=sys.stderr)
    print("  command:", " ".join(command), file=sys.stderr)
    print(result.stdout, file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compile split-crate Interface slices used by architecture CI.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for crate interface checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    for check in CHECKS:
        if not run_check(root, args.cargo, check):
            return 1

    print(f"ok: {len(CHECKS)} split-crate Interface checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
