#!/usr/bin/env python3
"""Verify retained provider evidence for the CDN routes in a release checkout."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from cdn_qualification import (  # noqa: E402
    COMMIT, RUN_NUMBER, SHA256, decode, fields, matches, release_scopes, require,
    required_platform_rows, verify_bundle_row,
    verify_release_cli_binaries,
)


POLICY = SCRIPTS.parents[1] / "crates/crab-storage/src/cdn/qualified-scopes.txt"
WORKFLOW = ".github/workflows/pb-provider-qualification.yml"
REPOSITORY = re.compile(r"[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+")


def qualified_run(metadata: Any, repository: str, source: str, run_id: str, attempt: str) -> None:
    require(isinstance(metadata, dict), "GitHub workflow attempt must be an object")
    require(
        str(metadata.get("id")) == run_id and str(metadata.get("run_attempt")) == attempt
        and metadata.get("path") == WORKFLOW and metadata.get("head_sha") == source
        and metadata.get("event") == "workflow_dispatch" and metadata.get("status") == "completed"
        and metadata.get("conclusion") == "success",
        "CDN evidence must come from a successful provider qualification dispatch for this release SHA and attempt",
    )
    head = metadata.get("head_repository")
    require(isinstance(head, dict) and head.get("full_name") == repository,
            "CDN evidence attempt must run in the release repository")


def verify_bundle(
    directory: Path, policy: Path, scopes: set[str], source: str,
    run_id: str, attempt: str, benchmarks_sha256: str,
) -> int:
    raw = (directory / "benchmarks.json").read_bytes()
    require(hashlib.sha256(raw).hexdigest() == benchmarks_sha256,
            "CDN benchmark registrations differ from the approved digest")
    registrations = decode(raw)
    require(isinstance(registrations, dict), "CDN benchmark registrations must be an object")
    rows = required_platform_rows(scopes)
    verify_release_cli_binaries(registrations, rows)
    for row in sorted(rows):
        verify_bundle_row(directory, row, registrations.get(row), source, run_id, attempt, policy=policy)
    return len(rows)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-sha", required=True)
    args = parser.parse_args()
    try:
        scopes = release_scopes(POLICY)
        if not scopes:
            print("CDN release policy has no enabled routes")
            return 0
        require(matches(COMMIT, args.source_sha), "CDN release requires the exact source SHA")
        approved = fields(decode(os.environ.get("CRAB_CDN_RELEASE_EVIDENCE", "null").encode()),
                          {"run_id", "run_attempt", "benchmarks_sha256"}, "CDN release evidence approval")
        run_id, attempt, digest = (approved[key] for key in ("run_id", "run_attempt", "benchmarks_sha256"))
        require(matches(RUN_NUMBER, run_id) and matches(RUN_NUMBER, attempt) and matches(SHA256, digest),
                "CDN release evidence requires an exact run ID, attempt and approved benchmark SHA-256")
        repository = os.environ.get("GITHUB_REPOSITORY")
        require(matches(REPOSITORY, repository), "CDN release evidence requires GITHUB_REPOSITORY")
        metadata = subprocess.check_output([
            "gh", "api", f"repos/{repository}/actions/runs/{run_id}/attempts/{attempt}",
        ])
        qualified_run(decode(metadata), repository, args.source_sha, run_id, attempt)
        with TemporaryDirectory(prefix="crab-cdn-release-") as temporary:
            subprocess.run([
                "gh", "run", "download", run_id, "--repo", repository, "--dir", temporary,
                "--name", f"cdn-release-evidence-{run_id}-{attempt}",
            ], check=True)
            count = verify_bundle(Path(temporary), POLICY, scopes, args.source_sha, run_id, attempt, digest)
        print(f"Verified {count} CDN reports for {len(scopes)} enabled release scopes")
    except subprocess.CalledProcessError as error:
        # The child already reported the failure. Avoid logging its arguments.
        print(f"error: CDN release evidence command failed ({error.returncode})", file=sys.stderr)
        return 1
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
