#!/usr/bin/env python3
"""Reduce Crab's redacted CDN transfer events for one qualification scope."""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import PRODUCT_COUNTERS, PROVIDER_FOR_EDGE, decode, fields, require, scope_parts  # noqa: E402


TARGET = "crab_storage::cdn::transfer"
TARGET_MARKER = TARGET.encode()
EVENT = "CDN immutable GET"
VERIFIED_GIT_EVENT = "CDN verified Git payload"
VERIFIED_LFS_EVENT = "CDN verified LFS payload"
ROUTE = ("provider", "access", "consumer", "family", "profile")
ROUTE_FIELDS = {"message", *ROUTE}
VERIFIED_FIELDS = ROUTE_FIELDS | {"source"}
TRANSFER_FIELDS = ROUTE_FIELDS | {"status", "cache", "range", "bytes", "complete", "elapsed_ms"}


def measure(lines: Iterable[bytes], scope: str) -> dict[str, int]:
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
        if isinstance(event, dict) and event.get("message") in (VERIFIED_GIT_EVENT, VERIFIED_LFS_EVENT):
            fields(event, VERIFIED_FIELDS, "CDN verified payload event")
            source = event["source"]
            lfs = event["message"] == VERIFIED_LFS_EVENT
            require(lfs == (event["family"] == "lfs") and source in (("edge",) if lfs else ("edge", "local")),
                    "unknown verified payload source or family")
            if tuple(event[name] for name in ROUTE) == selected:
                metric = "verified_edge_payloads" if source == "edge" else "local_payload_hits"
                metrics[metric] += 1
            continue
        event = fields(event, TRANSFER_FIELDS, "CDN transfer event")
        require(event["message"] == EVENT, "unrecognized CDN transfer event")
        metrics["all_edge_attempts"] += 1
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
    args = parser.parse_args()
    try:
        with args.stderr.open("rb") as source:
            result = measure(source, args.scope)
    except (OSError, ValueError):
        # Git and the product may have other diagnostics in their raw stderr.
        print("error: could not collect CDN product metrics for this scope", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
