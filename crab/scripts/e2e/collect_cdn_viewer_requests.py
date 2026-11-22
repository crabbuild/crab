#!/usr/bin/env python3
"""Join an ordinary Crab fetch to its provider's viewer access logs."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CDN_PROVENANCE_FIELDS, GIT_FETCH_RECORD_LIMIT, PROVIDER_FOR_EDGE, VIEWER_REQUEST_FILES, decode, matches,
    read_git_record, require, scope_parts, validate_git_fetch, verify_viewer_requests,
)
from collect_front_door_deployment_policy import FRONT_DOOR_HOST, SUBSCRIPTION  # noqa: E402
from run_cdn_transport_checks import approved_fixture, deployment_target, fixture_base  # noqa: E402


LOG_FIELDS = {b"x-edge-request-id", b"x-host-header", b"cs-method", b"cs-uri-stem", b"cs-protocol",
              b"sc-status", b"sc-bytes", b"x-edge-response-result-type"}
CF_CACHE_STATUS = {b"Hit": "hit", b"Miss": "miss"}
AZURE_CACHE_STATUS = {"HIT": "hit", "REMOTE_HIT": "hit", "MISS": "miss"}
MAX_LINE = 64 * 1024
AZURE_ARCHIVE_LIMIT = 32 * 1024 * 1024


def cloudfront_logs(paths: list[Path], expected: dict, fixture: dict) -> dict:
    base = fixture_base(fixture["base_url"])
    host = base.netloc.encode()
    prefix = f"{base.path.rstrip('/')}/{fixture['fixture_prefix']}{fixture['view_suffix']}/".encode()
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
                              "cache": CF_CACHE_STATUS.get(event[b"x-edge-response-result-type"], "unknown")}
                    require(fingerprint not in records or records[fingerprint] == record,
                            "CloudFront exports disagree about one viewer request")
                    records[fingerprint] = record
            require(names, "CloudFront standard log needs a W3C field header")
    return records


def front_door_logs(paths: list[Path], expected: dict, fixture: dict) -> dict:
    deployment = deployment_target(fixture, "front-door")
    subscription, group, profile = (deployment[key] for key in ("subscription", "resource_group", "profile"))
    require(matches(SUBSCRIPTION, subscription), "Front Door viewer evidence needs the approved Azure subscription")
    resource = f"/subscriptions/{subscription}/resourceGroups/{group}/providers/Microsoft.Cdn/profiles/{profile}".lower()
    _, access, _, _, _ = scope_parts(fixture["scope"])
    base = fixture_base(fixture["base_url"])
    prefix = f"/{fixture['fixture_prefix']}{fixture['view_suffix']}/"
    records = {}
    for path in paths:
        with path.open("rb") as source:
            raw = source.read(AZURE_ARCHIVE_LIMIT + 1)
        require(len(raw) <= AZURE_ARCHIVE_LIMIT, "Front Door archive exceeds the size limit")
        archive = decode(raw)
        require(isinstance(archive, dict) and isinstance(archive.get("records"), list),
                "expected an Azure Monitor resource log archive with records")
        for row in archive["records"]:
            require(isinstance(row, dict), "invalid Azure Monitor resource log record")
            if str(row.get("category", "")).lower() != "frontdooraccesslog":
                continue
            props = row.get("properties")
            require(isinstance(props, dict), "invalid Front Door access log properties")
            reference = props.get("trackingReference")
            require(isinstance(reference, str) and bool(reference), "missing Front Door tracking reference")
            fingerprint = hashlib.sha256(reference.encode()).hexdigest()
            if fingerprint not in expected:
                continue
            from_client = props.get("isReceivedFromClient")
            if from_client is False or from_client == "false":
                # Front Door can also log the parent cache for the same request.
                continue
            require(from_client is True or from_client == "true", "missing Front Door viewer or origin-shield classification")
            endpoint, uri = props.get("endpoint"), props.get("requestUri")
            require(matches(FRONT_DOOR_HOST, endpoint) and isinstance(uri, str), "invalid Front Door request endpoint")
            url = urlsplit(uri)
            # Private APIM removes its /crab API prefix before forwarding to the
            # only endpoint in the attested Front Door profile.
            require(str(row.get("resourceId", "")).lower() == resource and (access == "private" or endpoint == base.netloc)
                    and url.scheme == "https" and url.netloc in (endpoint, f"{endpoint}:443") and not url.query and not url.fragment
                    and props.get("httpMethod") == "GET" and url.path.startswith(prefix),
                    "Front Door request was logged outside the approved profile or repository prefix")
            status = str(props.get("httpStatusCode"))
            size = str(props.get("responseBytes"))
            cache = props.get("cacheStatus")
            require(re.fullmatch(r"[1-5][0-9]{2}", status) and re.fullmatch(r"[0-9]{1,20}", size) and isinstance(cache, str),
                    "Front Door log has invalid HTTP status, response size or cache status")
            record = {"status": int(status), "response_bytes": int(size), "cache": AZURE_CACHE_STATUS.get(cache, "unknown")}
            require(fingerprint not in records or records[fingerprint] == record,
                    "Front Door exports disagree about one viewer request")
            records[fingerprint] = record
    return records


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--client", required=True, type=Path, help="exact timed client.json")
    parser.add_argument("--log", required=True, action="append", type=Path,
                        help="CloudFront W3C access log or Azure Monitor Front Door archive")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, _, _, _, _ = scope_parts(fixture["scope"])
        require(PROVIDER_FOR_EDGE[edge] in VIEWER_REQUEST_FILES, "no viewer log collector is defined for this CDN fixture")
        raw = read_git_record(args.client, limit=GIT_FETCH_RECORD_LIMIT)
        candidate = decode(raw)
        require(isinstance(candidate, dict) and CDN_PROVENANCE_FIELDS | {"arm", "platform"} <= candidate.keys()
                and candidate["arm"] in ("edge", "warm_local") and candidate["scope"] == fixture["scope"]
                and candidate["configuration_sha256"] == args.configuration_sha256,
                "CDN viewer evidence needs a fetch for the approved fixture")
        provenance = {key: candidate[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}}
        client = validate_git_fetch(raw, provenance, candidate["arm"], "CDN benchmark fetch")
        require(client["all_edge_attempts"] > 0 and len(client["edge_responses"]) == client["all_edge_attempts"],
                "the timed fetch lacks a CDN request fingerprint for each viewer attempt")
        load = cloudfront_logs if edge == "cloudfront" else front_door_logs
        records = load(args.log, client["edge_responses"], fixture)
        digest = hashlib.sha256(raw).hexdigest()
        proof = {"schema": f"crab.{edge}-viewer-requests/v1", "client_report_sha256": digest, "records": records}
        verify_viewer_requests(proof, client, digest, edge)
        with args.output.open("x", encoding="utf-8") as destination:
            destination.write(json.dumps(proof, sort_keys=True) + "\n")
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
