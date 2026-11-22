#!/usr/bin/env python3
"""Join paired product reads and provider measurements into a CDN performance report."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    GIT_PRODUCT_DIR, LABEL, benchmark_registration, collect_git_performance, collect_git_product_action, decode,
    is_managed_cli_git_scope, matches, read_trace, require, scope_parts, verify_cleanup_trace, verify_performance,
)
from run_cdn_transport_checks import approved_fixture  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--benchmarks", type=Path, required=True)
    parser.add_argument("--benchmarks-sha256", required=True)
    parser.add_argument("--row", required=True, metavar="SCOPE/PLATFORM")
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--service-row", type=Path, help="row directory containing service-product and traces")
    parser.add_argument("--managed-row", type=Path, help="managed Git row directory containing git-product and traces")
    parser.add_argument("--fixture", type=Path, help="approved fixture JSON for the managed Git row")
    args = parser.parse_args()
    try:
        raw = args.benchmarks.read_bytes()
        require(hashlib.sha256(raw).hexdigest() == args.benchmarks_sha256,
                "benchmark registrations differ from the approved digest")
        registrations = decode(raw)
        require(isinstance(registrations, dict) and args.row in registrations,
                "requested row has no approved benchmark registration")
        scope, _, platform = args.row.rpartition("/")
        registration = benchmark_registration(registrations[args.row])
        require(matches(LABEL, platform) and registration["scope"] == scope,
                "benchmark registration does not match its approved row")
        managed = is_managed_cli_git_scope(scope)
        require(managed == (args.managed_row is not None) == (args.fixture is not None),
                "managed Git performance requires the measured row and approved fixture")
        native_target = None
        if managed:
            fixture = approved_fixture(args.fixture, registration["configuration_sha256"])
            require(fixture["scope"] == scope, "managed Git performance fixture differs from the approved row")
            edge, access, *_ = scope_parts(scope)
            cleanup = decode(read_trace(args.managed_row / "traces/cleanup.json", "cleanup"))
            row = registration | {
                "platform": platform, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
                "fixture_prefix": fixture["fixture_prefix"],
            }
            cleanup_at = verify_cleanup_trace(cleanup, row, edge, access)
            native_target = {"origin_sha256": hashlib.sha256(fixture["native_origin"].encode()).hexdigest(),
                             "prefix_sha256": cleanup["prefix_sha256"]}
            require(native_target["origin_sha256"] == cleanup["targets"]["native"]["origin_sha256"],
                    "managed Git cleanup did not observe the approved native target")
            rollback = read_trace(args.managed_row / "traces/rollback.json", "rollback")
            collect_git_product_action(args.managed_row / GIT_PRODUCT_DIR, row, rollback, cleanup_at=cleanup_at,
                                       native_origin_sha256=native_target["origin_sha256"])
        performance = collect_git_performance(args.inputs, registration, platform, args.run_id, args.run_attempt,
                                              service_row=args.service_row, managed_row=args.managed_row, native_target=native_target)
        verify_performance(performance, registration)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(performance, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
