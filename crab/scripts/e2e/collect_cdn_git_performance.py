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
    LABEL, benchmark_registration, collect_git_performance, decode, matches, require, verify_performance,
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--benchmarks", type=Path, required=True)
    parser.add_argument("--benchmarks-sha256", required=True)
    parser.add_argument("--row", required=True, metavar="SCOPE/PLATFORM")
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--service-row", type=Path, help="row directory containing service-product and traces")
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
        performance = collect_git_performance(args.inputs, registration, platform, args.run_id, args.run_attempt,
                                              service_row=args.service_row)
        verify_performance(performance, registration)
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(performance, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
