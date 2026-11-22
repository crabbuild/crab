#!/usr/bin/env python3
"""Reduce Crab's redacted CDN transfer events for one qualification scope."""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    NATIVE_TRAFFIC_FIELDS, PRODUCT_COUNTERS, PROVIDER_FOR_EDGE, SHA256,
    VIEWER_REQUEST_FILES, decode, fields, matches, require, scope_parts,
)


TARGET = "crab_storage::cdn::transfer"
TARGET_MARKER = TARGET.encode()
EVENT = "CDN immutable GET"
VERIFIED_GIT_EVENT = "CDN verified Git payload"
VERIFIED_LFS_EVENT = "CDN verified LFS payload"
VERIFIED_XORB_EVENT = "CDN verified xorb payload"
ROUTE = ("provider", "access", "consumer", "family", "profile")
ROUTE_FIELDS = {"message", *ROUTE}
VERIFIED_FIELDS = ROUTE_FIELDS | {"source"}
TRANSFER_FIELDS = ROUTE_FIELDS | {"status", "cache", "range", "bytes", "complete", "elapsed_ms", "request_sha256"}
NATIVE_TARGET = "crab_storage::native_read"
NATIVE_TARGET_MARKER = NATIVE_TARGET.encode()


def measure_native(lines: Iterable[bytes], scope: str) -> dict[str, int]:
    provider = PROVIDER_FOR_EDGE[scope_parts(scope)[0]]
    metrics = dict.fromkeys(NATIVE_TRAFFIC_FIELDS, 0)
    for raw in lines:
        if NATIVE_TARGET_MARKER not in raw:
            continue
        line = decode(raw)
        if not isinstance(line, dict) or line.get("target") != NATIVE_TARGET:
            continue
        event = fields(line.get("fields"), {"message", "provider", "method", "status", "bytes"}, "native read event")
        method, status, body = (event[name] for name in ("method", "status", "bytes"))
        require(event["message"] == "Native object store read" and event["provider"] == provider
                and method in ("get", "head") and type(status) is int and (status == 0 or 100 <= status <= 599)
                and type(body) is int and body >= 0 and (status != 0 or body == 0),
                "invalid native read event for the approved provider")
        if status:
            metrics[f"{method}_responses"] += 1
            metrics["body_bytes"] += body
    return metrics


def measure(lines: Iterable[bytes], scope: str, *, edge_responses: dict | None = None,
            expected_git_blob: str | None = None, expected_git_packs: set[str] | None = None,
            verified_git_packs: set[str] | None = None, expected_xorb_key: str | None = None,
            local_xorb_only: bool = False) -> dict[str, int]:
    edge, access, consumer, family, profile = scope_parts(scope)
    selected = (PROVIDER_FOR_EDGE[edge], access, consumer, family, profile)
    metrics = dict.fromkeys(PRODUCT_COUNTERS, 0)
    for raw in lines:
        if TARGET_MARKER not in raw:
            continue
        line = decode(raw)
        if not isinstance(line, dict) or line.get("target") != TARGET:
            continue
        event = line.get("fields")
        if isinstance(event, dict) and event.get("message") == VERIFIED_XORB_EVENT:
            fields(event, VERIFIED_FIELDS | {"xorb_key_sha256"}, "CDN verified xorb event")
            source = event["source"]
            require(event["family"] == "xorb" and source in ("edge", "local", "origin")
                    and matches(SHA256, event["xorb_key_sha256"]), "invalid verified CDN xorb key")
            exact_key = event["xorb_key_sha256"] == expected_xorb_key
            same_route = tuple(event[name] for name in ROUTE) == selected
            if local_xorb_only:
                require(same_route and exact_key and source == "local",
                        "SDK warm hydration read xorb chunks outside its approved local cache")
            if same_route and (expected_xorb_key is None or exact_key) and source != "origin":
                metric = "verified_edge_payloads" if source == "edge" else "local_payload_hits"
                metrics[metric] += 1
            continue
        if isinstance(event, dict) and event.get("message") in (VERIFIED_GIT_EVENT, VERIFIED_LFS_EVENT):
            blob, pack = "git_blob_oid_sha256", "git_pack_blake3"
            fingerprints = event.keys() & {blob, pack}
            fields(event, VERIFIED_FIELDS | fingerprints, "CDN verified payload event")
            source = event["source"]
            lfs = event["message"] == VERIFIED_LFS_EVENT
            require(lfs == (event["family"] == "lfs") and event["family"] != "xorb" and source in ("edge", "local"),
                    "unknown verified payload source or family")
            require(not fingerprints or (len(fingerprints) == 1 and event["family"] == "git"
                    and all(matches(SHA256, event[key]) for key in fingerprints)
                    and (pack not in event or source == "edge")),
                    "invalid verified CDN Git payload fingerprint")
            if tuple(event[name] for name in ROUTE) == selected:
                if expected_git_blob is not None and event.get(blob) != expected_git_blob:
                    continue
                if expected_git_packs is not None and event.get(pack) not in expected_git_packs:
                    continue
                if verified_git_packs is not None and pack in event:
                    verified_git_packs.add(event[pack])
                metric = "verified_edge_payloads" if source == "edge" else "local_payload_hits"
                metrics[metric] += 1
            continue
        event = fields(event, TRANSFER_FIELDS, "CDN transfer event")
        require(event["message"] == EVENT, "unrecognized CDN transfer event")
        metrics["all_edge_attempts"] += 1
        request_id = event["request_sha256"]
        provider = event["provider"]
        require(request_id == "" or (isinstance(provider, str) and provider in VIEWER_REQUEST_FILES and matches(SHA256, request_id)),
                "invalid CDN viewer request fingerprint")
        if edge_responses is not None and request_id:
            require(request_id not in edge_responses, "duplicate CDN viewer request fingerprint")
            edge_responses[request_id] = {key: event[key] for key in ("status", "cache", "bytes", "complete")}
        route = tuple(event[name] for name in ROUTE)
        if route != selected:
            continue
        status = event["status"]
        body = event["bytes"]
        elapsed = event["elapsed_ms"]
        complete = event["complete"]
        ranged = event["range"]
        cache = event["cache"]
        require(type(status) is int and (status == 0 or 100 <= status <= 599)
                and type(body) is int and body >= 0 and type(elapsed) is int and elapsed >= 0
                and type(complete) is bool and type(ranged) is bool and cache in ("hit", "miss", "unknown"),
                "invalid CDN transfer observation")
        metrics["edge_attempts"] += 1
        metrics["edge_elapsed_ms_total"] += elapsed
        if complete:
            require(status == (206 if ranged else 200), "inconsistent completed CDN response")
            if body > 0:
                metrics["observed_edge_gets"] += 1
                metrics["edge_bytes"] += body
                category = {"hit": "edge_hits", "miss": "edge_misses", "unknown": "edge_unknown"}[cache]
                metrics[category] += 1
        else:
            metrics["incomplete_edge_bytes"] += body
    return metrics


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stderr", type=Path, required=True)
    parser.add_argument("--scope", required=True)
    parser.add_argument("--xorb-key-sha256", help="SHA-256 of the approved physical xorb key")
    args = parser.parse_args()
    try:
        if scope_parts(args.scope)[3] == "xorb":
            require(matches(SHA256, args.xorb_key_sha256), "xorb evidence requires the exact approved physical key")
        else:
            require(args.xorb_key_sha256 is None, "xorb key evidence requires an xorb scope")
        with args.stderr.open("rb") as source:
            result = measure(source, args.scope, expected_xorb_key=args.xorb_key_sha256)
    except (OSError, ValueError):
        # Git and the product may have other diagnostics in their raw stderr.
        print("error: could not collect CDN product metrics for this scope", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
