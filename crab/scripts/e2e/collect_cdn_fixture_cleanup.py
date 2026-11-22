#!/usr/bin/env python3
"""Verify native and public-export fixture prefixes after operator cleanup."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from email.utils import formatdate
from pathlib import Path
from urllib.parse import SplitResult, urlencode, urlsplit

from run_cdn_transport_checks import approved_fixture, https_response, native_base, origin_bucket

from cdn_qualification import COMMIT, RUN_NUMBER, CdnEvidenceError, decode, matches, require, scope_parts  # noqa: E402


def command_output(*command: str) -> bytes:
    try:
        result = subprocess.run(command, capture_output=True, check=False, timeout=45)
    except (OSError, subprocess.TimeoutExpired):
        raise CdnEvidenceError(f"could not run {command[0]}") from None
    require(result.returncode == 0, f"{command[0]} failed (exit {result.returncode})")
    return result.stdout


def bearer(*command: str) -> str:
    token = command_output(*command).strip()
    require(re.fullmatch(rb"[A-Za-z0-9._~+/=-]{1,16384}", token) is not None,
            "cloud CLI did not return an access token")
    return "Bearer " + token.decode("ascii")


def api_body(base: SplitResult, target: str, headers: dict[str, str], check: str) -> bytes:
    with https_response(base, "GET", target, headers, check) as response:
        require(response.status == 200, f"{check} expected HTTP 200, got {response.status}")
        raw = response.read(65537)
        require(len(raw) <= 65536, f"{check} response is too large")
    return raw


def s3_inventory(origin: str, prefix: str) -> dict[str, str]:
    base = native_base(origin, "cloudfront")
    region = base.netloc.split(".")[1]
    bucket = base.path.removeprefix("/")
    target = (
        "--bucket", bucket, "--prefix", prefix, "--region", region, "--endpoint-url", f"https://{base.netloc}",
        "--no-paginate", "--no-cli-pager", "--output", "json",
    )
    # A retired upload can become a version between requests. Check uploads
    # first so a concurrent completion cannot move between the two inventories.
    uploads = decode(command_output("aws", "s3api", "list-multipart-uploads", *target, "--max-uploads", "1"))
    require(isinstance(uploads, dict) and uploads.get("IsTruncated") is False
            and uploads.get("Bucket") == bucket and uploads.get("Prefix") == prefix
            and all(uploads.get(name, []) == [] for name in ("Uploads", "CommonPrefixes")),
            "S3 fixture prefix still contains incomplete multipart uploads, or its inventory is incomplete")
    result = decode(command_output("aws", "s3api", "list-object-versions", *target, "--max-keys", "1"))
    require(isinstance(result, dict) and result.get("IsTruncated") is False
            and result.get("Prefix") == prefix
            and all(result.get(name, []) == [] for name in ("Versions", "DeleteMarkers", "CommonPrefixes")),
            "S3 fixture prefix still contains versions or delete markers, or its inventory is incomplete")
    return {"multipart_uploads": "empty", "versions_and_delete_markers": "empty"}


def gcs_inventory(origin: str, prefix: str, authorization: str) -> dict[str, str]:
    base = urlsplit("https://storage.googleapis.com")
    bucket = origin_bucket(origin, "cloud-cdn")
    headers = {"Authorization": authorization, "Accept-Encoding": "identity"}
    bucket_path = f"/storage/v1/b/{bucket}"
    query = urlencode({"fields": "name,softDeletePolicy(retentionDurationSeconds)"})
    settings = decode(api_body(base, f"{bucket_path}?{query}", headers, "GCS soft delete policy"))
    require(isinstance(settings, dict) and settings.get("name") == bucket,
            "GCS did not identify the approved cleanup bucket")
    policy = settings.get("softDeletePolicy", {})
    require(isinstance(policy, dict), "invalid GCS soft delete policy")
    retention = policy.get("retentionDurationSeconds", "0")
    require(isinstance(retention, str) and re.fullmatch(r"[0-9]{1,20}", retention) is not None,
            "invalid GCS soft delete retention")

    inventories = {"generations": "versions"}
    if int(retention) > 0:
        inventories["soft_deleted"] = "softDeleted"
    for name, selector in inventories.items():
        query = urlencode({"prefix": prefix, "maxResults": 1, selector: "true", "fields": "kind,items(name),nextPageToken"})
        result = decode(api_body(base, f"{bucket_path}/o?{query}", headers, f"GCS {name} cleanup"))
        require(isinstance(result, dict) and result.get("kind") == "storage#objects"
                and result.get("items", []) == [] and not result.get("nextPageToken"),
                f"GCS fixture prefix still contains {name} or its inventory is incomplete")
    return {"generations": "empty", "soft_deleted": "empty" if int(retention) > 0 else "disabled"}


def azure_inventory(origin: str, prefix: str, authorization: str) -> dict[str, str]:
    base = urlsplit(origin)
    headers = {"Authorization": authorization, "Accept-Encoding": "identity", "x-ms-version": "2023-11-03",
               "x-ms-date": formatdate(usegmt=True)}
    with https_response(base, "GET", "/?restype=account&comp=properties", headers, "Azure account information") as response:
        require(response.status == 200, f"Azure account information expected HTTP 200, got {response.status}")
        namespace = response.getheader("x-ms-is-hns-enabled")
    require(namespace in ("true", "false"), "Azure did not report its account namespace type")
    if namespace == "true":
        inventories = {"blobs": {}, "soft_deleted": {"showonly": "deleted"}}
    else:
        inventories = {"versions_and_snapshots": {"include": "snapshots,versions,deleted,deletedwithversions"}}
    for name, selector in inventories.items():
        query = urlencode({"restype": "container", "comp": "list", "prefix": prefix, "maxresults": 1} | selector)
        raw = api_body(base, f"{base.path}?{query}", headers, f"Azure {name} cleanup")
        try:
            result = ET.fromstring(raw)
        except ET.ParseError:
            raise ValueError("Azure returned an invalid cleanup inventory") from None
        blobs, marker = result.find("Blobs"), result.find("NextMarker")
        require(result.tag == "EnumerationResults" and result.findtext("Prefix") == prefix
                and blobs is not None and len(blobs) == 0 and marker is not None and not marker.text,
                f"Azure fixture prefix still contains {name} or its inventory is incomplete")
    return dict.fromkeys(inventories, "empty")


def collect(fixture: dict) -> dict:
    edge, access, _, _, _ = scope_parts(fixture["scope"])
    prefix = fixture["fixture_prefix"] + "/"
    origins = {"native": fixture["native_origin"]}
    if access == "public":
        origins["export"] = fixture["export_origin"]
    if edge == "cloud-cdn":
        authorization = bearer("gcloud", "auth", "print-access-token", "--quiet")
    if edge == "front-door":
        authorization = bearer("az", "account", "get-access-token", "--resource", "https://storage.azure.com/",
                               "--query", "accessToken", "--output", "tsv", "--only-show-errors")

    targets = {}
    for role, origin in origins.items():
        if edge == "cloudfront":
            inventories = s3_inventory(origin, prefix)
        elif edge == "cloud-cdn":
            inventories = gcs_inventory(origin, prefix, authorization)
        else:
            inventories = azure_inventory(origin, prefix, authorization)
        targets[role] = {"origin_sha256": hashlib.sha256(origin.encode()).hexdigest(), "inventories": inventories}
    return {"prefix_sha256": hashlib.sha256(prefix.encode()).hexdigest(), "targets": targets,
            "observed_at_unix_ms": time.time_ns() // 1_000_000}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
        require(not args.output.exists() and not args.output.is_symlink(), "cleanup trace output must be new")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        result = collect(fixture) | {
            "schema": "crab.cdn-cleanup-trace/v1", "check": "cleanup", "scope": fixture["scope"],
            "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
            "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
        }
        with args.output.open("x", encoding="utf-8") as file:
            file.write(json.dumps(result, sort_keys=True) + "\n")
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print("Recorded empty native fixture inventories")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
