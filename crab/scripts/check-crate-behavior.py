#!/usr/bin/env python3
"""Run focused split-crate behavior checks for moved architecture seams."""

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
    Check("crab-types shared contract behavior", ("test", "-p", "crab-types")),
    Check("crab-storage provider-store behavior", ("test", "-p", "crab-storage", "provider_store")),
    Check("crab-xet chunker behavior", ("test", "-p", "crab-xet", "--features", "chunker", "chunker")),
    Check(
        "crab-xet upload-concurrency behavior",
        ("test", "-p", "crab-xet", "--features", "upload-concurrency", "upload_concurrency"),
    ),
    Check("crab-auth credential response behavior", ("test", "-p", "crab-auth", "credential_response")),
    Check("crab-auth OIDC behavior", ("test", "-p", "crab-auth", "--features", "oidc-client", "oidc")),
    Check(
        "crab-auth Crab Auth client behavior",
        ("test", "-p", "crab-auth", "--features", "crab-auth-client", "crab_auth_client"),
    ),
    Check(
        "crab-auth AWS OIDC client behavior",
        ("test", "-p", "crab-auth", "--features", "aws-oidc-client", "aws_oidc"),
    ),
    Check(
        "crab-auth GCP workload identity behavior",
        (
            "test",
            "-p",
            "crab-auth",
            "--features",
            "gcp-workload-identity-client",
            "gcp_federation",
        ),
    ),
    Check(
        "crab-auth Azure Entra behavior",
        ("test", "-p", "crab-auth", "--features", "azure-entra-client", "azure_entra"),
    ),
    Check(
        "crab-auth-store credential-store behavior",
        ("test", "-p", "crab-auth-store", "--features", "refreshing-store"),
    ),
    Check(
        "crab-cache client/cache behavior",
        (
            "test",
            "-p",
            "crab-cache",
            "--features",
            "local-cache,remote-client,xet-chunk-cache",
        ),
    ),
    Check(
        "crab-metadata file-index lookup behavior",
        ("test", "-p", "crab-metadata", "--features", "file-index-reader", "file_index_lookup"),
    ),
    Check(
        "crab-metadata catalog-bound visibility behavior",
        (
            "test",
            "-p",
            "crab-metadata",
            "--features",
            "storage,remote-index",
            "git_visibility",
        ),
    ),
    Check(
        "crab-metadata persistent chunk index behavior",
        ("test", "-p", "crab-metadata", "--features", "local-index", "persistent_chunk_index"),
    ),
    Check("crab-staging behavior", ("test", "-p", "crab-staging")),
    Check("crab-workflow YAML contract behavior", ("test", "-p", "crab-workflow", "yaml")),
    Check("crab-coordination contract behavior", ("test", "-p", "crab-coordination")),
    Check("crab-auth-server package behavior", ("test", "-p", "crab-auth-server", "--lib")),
    Check("crab-cache-server package behavior", ("test", "-p", "crab-cache-server", "--lib")),
    Check("crab-lfs-server package behavior", ("test", "-p", "crab-lfs-server", "--lib")),
)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run_check(root: Path, cargo: str, check: Check) -> bool:
    command = [cargo, *check.args]
    print(f"testing: {check.label}")
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
        description="Run focused owner-crate behavior checks used by architecture CI.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for crate behavior checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    for check in CHECKS:
        if not run_check(root, args.cargo, check):
            return 1

    print(f"ok: {len(CHECKS)} split-crate behavior checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
