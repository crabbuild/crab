#!/usr/bin/env python3
"""Retain an independent native-provider observation of a qualification ref."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from pathlib import Path

from collect_cdn_fixture_cleanup import command_output
from run_cdn_git_workload import runner_platform
from run_cdn_transport_checks import approved_fixture

from cdn_qualification import (
    COMMIT, GIT_NATIVE_REF_FILES, RUN_NUMBER, decode, is_direct_sdk_git_scope,
    is_direct_sdk_hydration_scope, matches, read_git_record,
    require, requires_cli_workload, validate_git_publication, validate_sdk_hydration_publication, verify_git_native_ref,
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=GIT_NATIVE_REF_FILES)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--publication", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--probe", required=True, type=Path, help="built cdn-native-ref example from the approved commit")
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
        require(not args.output.exists() and not args.output.is_symlink(), "native ref observation output must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        scope = fixture["scope"]
        require(requires_cli_workload(scope) or (is_direct_sdk_git_scope(scope) and fixture["view_suffix"] == "")
                or is_direct_sdk_hydration_scope(scope),
                "native ref observation needs a supported direct CLI or SDK publication workload")
        provenance = {
            "scope": fixture["scope"], "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
            "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt, "platform": runner_platform(),
        }
        raw = read_git_record(args.publication)
        publication = (validate_sdk_hydration_publication(raw, provenance) if is_direct_sdk_hydration_scope(scope)
                       else validate_git_publication(raw, provenance))
        require(publication["published"], "native observation requires a confirmed test publication")
        digest = hashlib.sha256(raw).hexdigest()
        started = time.time_ns() // 1_000_000
        probe_output = command_output(
            str(args.probe.absolute()), fixture["native_origin"], fixture["fixture_prefix"], publication["ref"],
        )
        observed_at = time.time_ns() // 1_000_000
        require(len(probe_output) <= 8192, "native ref probe output is too large")
        observation = provenance | {
            "schema": "crab.cdn-native-ref/v1", "phase": args.phase, "publication_sha256": digest,
            "started_at_unix_ms": started, "observed_at_unix_ms": observed_at, "native_ref": decode(probe_output),
        }
        verify_git_native_ref(
            observation, provenance | {"fixture_prefix": fixture["fixture_prefix"]}, publication, digest,
            args.phase, hashlib.sha256(fixture["native_origin"].encode()).hexdigest(),
        )
        with args.output.open("x", encoding="utf-8") as output:
            json.dump(observation, output, sort_keys=True)
            output.write("\n")
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
