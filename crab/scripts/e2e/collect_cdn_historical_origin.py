#!/usr/bin/env python3
"""Check approved retained object versions on a public CDN fixture's native origin."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from email.utils import formatdate
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.parse import SplitResult, urlencode

from collect_cdn_fixture_cleanup import bearer
from collect_cloudfront_deployment_policy import aws
from run_cdn_transport_checks import approved_fixture, fixture_object_key, https_response, native_base, request_sample

from cdn_qualification import (
    COMMIT, HISTORICAL_KINDS, MAX_HISTORICAL_OBJECT_BYTES, MAX_HISTORICAL_VERSIONS, RUN_NUMBER, SHA256,
    decode, fields, historical_approval_digest, matches, positive, require, scope_parts,
)


VERSION_FORMATS = {
    "versionId": re.compile(r"[!-~]{1,1024}"),
    "generation": re.compile(r"[1-9][0-9]{0,19}"),
    "versionid": re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z", re.ASCII),
    "snapshot": re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z", re.ASCII),
}


def history_plan(path: Path, edge: str, current_digest: str) -> list[dict]:
    with path.open("rb") as source:
        raw = source.read(16_385)
    require(len(raw) <= 16_384, "native history input is too large")
    versions = fields(decode(raw), {"versions"}, "native history input")["versions"]
    require(isinstance(versions, list) and 1 <= len(versions) <= MAX_HISTORICAL_VERSIONS,
            "native history input needs one or more retained versions")
    plan = []
    seen = set()
    for value in versions:
        entry = fields(value, {"kind", "selector", "size", "sha256"}, "approved native history version")
        kind, selector = entry["kind"], entry["selector"]
        require(isinstance(kind, str) and kind in HISTORICAL_KINDS[edge] and matches(VERSION_FORMATS[kind], selector),
                "historical selector must match the approved provider")
        require(positive(entry["size"]) and entry["size"] <= MAX_HISTORICAL_OBJECT_BYTES
                and matches(SHA256, entry["sha256"]) and entry["sha256"] != current_digest,
                "native history input requires bounded, distinct retired bytes")
        require((kind, selector) not in seen, "duplicate native history selector")
        seen.add((kind, selector))
        plan.append(entry)
    return plan


def redacted_version(entry: dict) -> dict:
    return {field: entry[field] for field in ("kind", "size", "sha256")} | {
        "selector_sha256": hashlib.sha256(entry["selector"].encode()).hexdigest(),
    }


def s3_history(fixture: dict, origin: SplitResult, key: str, entry: dict, profile: str | None) -> bytes:
    placement = fixture["source_placement"]
    region = placement["location"]
    require(origin.netloc == f"s3.{region}.amazonaws.com", "S3 history origin differs from the approved region")
    size = entry["size"]
    with TemporaryDirectory(prefix="crab-cdn-history-") as scratch:
        path = Path(scratch) / "payload"
        metadata = aws(
            profile, region, "s3api", "get-object",
            "--bucket", origin.path.removeprefix("/"), "--key", key, "--version-id=" + entry["selector"],
            "--expected-bucket-owner", placement["resource"].split("/")[1],
            "--range", f"bytes=0-{size - 1}", "--endpoint-url", f"https://{origin.netloc}", str(path),
        )
        require(metadata.get("VersionId") == entry["selector"]
                and metadata.get("ContentRange") == f"bytes 0-{size - 1}/{size}"
                and metadata.get("ContentLength") == size and metadata.get("ContentEncoding") in (None, "identity"),
                "S3 did not return the complete approved version")
        with path.open("rb") as payload:
            return payload.read(size + 1)


def token_history(origin: SplitResult, edge: str, key: str, query: str, entry: dict, authorization: str) -> bytes:
    headers = {"Authorization": authorization, "Accept-Encoding": "identity"}
    if edge == "front-door":
        headers |= {"x-ms-version": "2019-12-12", "x-ms-date": formatdate(usegmt=True)}
    with https_response(origin, "GET", f"{origin.path}/{key}{query}", headers, "authenticated native history") as response:
        require(response.status == 200 and response.headers.get_all("Content-Encoding") in (None, ["identity"]),
                "native history did not return the complete approved representation")
        if edge == "cloud-cdn":
            require(response.headers.get_all("x-goog-generation") == [entry["selector"]],
                    "GCS did not return the approved historical generation")
        return response.read(entry["size"] + 1)


def collect(fixture: dict, edge: str, key: str, plan: list[dict], aws_profile: str | None) -> list[dict]:
    origin = native_base(fixture["native_origin"], edge)
    token = None
    if edge == "cloud-cdn":
        token = bearer("gcloud", "auth", "print-access-token", "--quiet")
    elif edge == "front-door":
        token = bearer("az", "account", "get-access-token", "--resource", "https://storage.azure.com/",
                       "--query", "accessToken", "--output", "tsv", "--only-show-errors")
    versions = []
    for entry in plan:
        query = "?" + urlencode({entry["kind"]: entry["selector"]})
        if edge == "cloudfront":
            body = s3_history(fixture, origin, key, entry, aws_profile)
            status = 206
        else:
            body = token_history(origin, edge, key, query, entry, token)
            status = 200
        digest = hashlib.sha256(body).hexdigest()
        require(len(body) == entry["size"] and digest == entry["sha256"], "native history differs from the approved bytes")
        anonymous, _ = request_sample(origin, None, key, {}, size=entry["size"], check="anonymous native history", query=query)
        versions.append(redacted_version(entry) | {
            "sha256": digest, "size": len(body), "authorized_status": status, "anonymous_status": anonymous["status"],
        })
    return versions


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--history", type=Path, required=True, help="operator-reviewed native version selectors and payload digests")
    parser.add_argument("--historical-origin-sha256", help="preregistered digest, required to collect observations")
    parser.add_argument("--output", type=Path, help="write the retained trace, or print the candidate digest when omitted")
    parser.add_argument("--source-sha")
    parser.add_argument("--run-id")
    parser.add_argument("--run-attempt")
    parser.add_argument("--aws-profile")
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, access, _, family, _ = scope_parts(fixture["scope"])
        require(access == "public", "native history qualification needs a public CDN fixture")
        key = fixture_object_key(fixture, access, family)
        plan = history_plan(args.history, edge, fixture["sha256"])
        origin_digest = hashlib.sha256(fixture["native_origin"].encode()).hexdigest()
        key_digest = hashlib.sha256(key.encode()).hexdigest()
        approval = historical_approval_digest(origin_digest, key_digest, [redacted_version(entry) for entry in plan])
        if args.output is None:
            print(approval)
            return 0
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id) and matches(RUN_NUMBER, args.run_attempt),
                "native history needs exact workflow provenance")
        require(approval == args.historical_origin_sha256, "native history differs from preregistration")
        require(not args.output.exists() and not args.output.is_symlink(), "native history trace output must be new")
        trace = {
            "schema": "crab.cdn-historical-origin/v1", "scope": fixture["scope"],
            "source_sha": args.source_sha, "configuration_sha256": args.configuration_sha256,
            "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
            "origin_sha256": origin_digest, "key_sha256": key_digest,
            "versions": collect(fixture, edge, key, plan, args.aws_profile),
        }
        with args.output.open("x", encoding="utf-8") as stream:
            json.dump(trace, stream, sort_keys=True)
            stream.write("\n")
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
