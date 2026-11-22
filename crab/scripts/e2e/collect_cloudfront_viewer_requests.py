#!/usr/bin/env python3
"""Join an ordinary Crab fetch to CloudFront standard viewer access logs."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CDN_PROVENANCE_FIELDS, GIT_FETCH_RECORD_LIMIT, decode, read_git_record, require, scope_parts,
    validate_git_fetch, verify_cloudfront_requests,
)
from run_cdn_transport_checks import approved_fixture, fixture_base  # noqa: E402


LOG_FIELDS = {b"x-edge-request-id", b"x-host-header", b"cs-method", b"cs-uri-stem", b"cs-protocol",
              b"sc-status", b"sc-bytes", b"x-edge-response-result-type"}
RESULT = {b"Hit": "hit", b"Miss": "miss"}
MAX_LINE = 64 * 1024


def collect_logs(paths: list[Path], expected: dict, host: bytes, prefix: bytes) -> dict:
    records = {}
    for path in paths:
        names = []
        with path.open("rb") as source:
            while line := source.readline(MAX_LINE + 1):
                require(len(line) <= MAX_LINE, "CloudFront standard log line exceeds the size limit")
                line = line.rstrip(b"\r\n")
                if line.startswith(b"#Fields:"):
                    names = line.removeprefix(b"#Fields:").split()
                    require(set(names) >= LOG_FIELDS and len(names) == len(set(names)),
                            "CloudFront standard log is missing required or unique fields")
                elif line and not line.startswith(b"#"):
                    values = line.split(b"\t")
                    require(names and len(values) == len(names), "invalid CloudFront standard log record")
                    event = dict(zip(names, values, strict=True))
                    fingerprint = hashlib.sha256(event[b"x-edge-request-id"]).hexdigest()
                    if fingerprint not in expected:
                        continue
                    path_name = event[b"cs-uri-stem"]
                    require(event[b"x-host-header"] == host and event[b"cs-method"] == b"GET"
                            and event[b"cs-protocol"] == b"https" and path_name.startswith(prefix),
                            "CloudFront request was logged outside the approved viewer host or repository prefix")
                    status, size = event[b"sc-status"], event[b"sc-bytes"]
                    require(re.fullmatch(rb"[1-5][0-9]{2}", status) and re.fullmatch(rb"[0-9]{1,20}", size),
                            "CloudFront log has invalid HTTP status or response size")
                    record = {"status": int(status), "response_bytes": int(size),
                              "cache": RESULT.get(event[b"x-edge-response-result-type"], "unknown")}
                    require(fingerprint not in records or records[fingerprint] == record,
                            "CloudFront exports disagree about one viewer request")
                    records[fingerprint] = record
            require(names, "CloudFront standard log needs a W3C field header")
    return records


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--client", required=True, type=Path, help="exact timed client.json")
    parser.add_argument("--log", required=True, action="append", type=Path, help="decompressed CloudFront W3C access log")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, _, _, _, _ = scope_parts(fixture["scope"])
        require(edge == "cloudfront", "CloudFront request evidence requires an approved AWS fixture")
        raw = read_git_record(args.client, limit=GIT_FETCH_RECORD_LIMIT)
        candidate = decode(raw)
        require(isinstance(candidate, dict) and CDN_PROVENANCE_FIELDS | {"arm", "platform"} <= candidate.keys()
                and candidate["arm"] in ("edge", "warm_local") and candidate["scope"] == fixture["scope"]
                and candidate["configuration_sha256"] == args.configuration_sha256,
                "CloudFront viewer evidence needs a fetch for the approved fixture")
        provenance = {key: candidate[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}}
        client = validate_git_fetch(raw, provenance, candidate["arm"], "CloudFront benchmark fetch")
        require(client["all_edge_attempts"] > 0 and len(client["edge_responses"]) == client["all_edge_attempts"],
                "the timed fetch lacks a CloudFront request fingerprint for each viewer attempt")
        base = fixture_base(fixture["base_url"])
        path = f"{base.path.rstrip('/')}/{fixture['fixture_prefix']}{fixture['view_suffix']}/"
        records = collect_logs(args.log, client["edge_responses"], base.netloc.encode(), path.encode())
        digest = hashlib.sha256(raw).hexdigest()
        proof = {"schema": "crab.cloudfront-viewer-requests/v1", "client_report_sha256": digest, "records": records}
        verify_cloudfront_requests(proof, client, digest)
        with args.output.open("x", encoding="utf-8") as destination:
            destination.write(json.dumps(proof, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
