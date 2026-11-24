"""Read the private access logs of a deployed AWS qualification companion."""

from __future__ import annotations

import gzip
import re
from datetime import datetime, timezone
from pathlib import Path

from cdn_qualification import matches, positive, require, scope_parts
from collect_cloudfront_deployment_policy import aws, cloudfront_id
from run_cdn_transport_checks import deployment_target, origin_bucket


REGION = "us-east-1"
HOUR = 3600
DAY = 24 * HOUR
STACK_NAME = re.compile(r"[A-Za-z][A-Za-z0-9-]{0,127}")
STACK_ARN = re.compile(r"arn:aws:cloudformation:us-east-1:([0-9]{12}):stack/([A-Za-z][A-Za-z0-9-]*)/[A-Za-z0-9-]+")
ETAG = re.compile(r'"[\x21\x23-\x7e]+"')
LOG_BYTES = 32 * 1024 * 1024
LIST_LIMIT = 1000
DOWNLOAD_LIMIT = 128


def log_destination(fixture: dict, stack: str, profile: str | None) -> tuple[str, str, str, str]:
    require(matches(STACK_NAME, stack), "AWS log collection requires the named qualification companion stack")
    deployment = deployment_target(fixture, "cloudfront")
    require(deployment["region"] == REGION, "AWS log companion requires a us-east-1 fixture")
    expected_distribution = cloudfront_id(deployment["distribution_id"])
    _, access, _, _, _ = scope_parts(fixture["scope"])
    source = fixture["export_origin"] if access == "public" else fixture["native_origin"]
    origin = origin_bucket(source, "cloudfront")
    response = aws(profile, REGION, "cloudformation", "describe-stacks", "--stack-name", stack)
    stacks = response.get("Stacks")
    require(isinstance(stacks, list) and len(stacks) == 1 and isinstance(stacks[0], dict),
            "AWS log collection requires one deployed companion stack")
    deployed = stacks[0]
    arn = STACK_ARN.fullmatch(deployed.get("StackId", "")) if isinstance(deployed.get("StackId"), str) else None
    require(arn is not None and arn[2] == stack and deployed.get("StackName") == stack
            and deployed.get("StackStatus") in ("CREATE_COMPLETE", "UPDATE_COMPLETE"),
            "AWS log companion is outside the required Region or has not finished deployment")
    owner = arn[1]
    outputs = deployed.get("Outputs")
    require(isinstance(outputs, list) and len(outputs) == 5 and all(
        isinstance(row, dict) and isinstance(row.get("OutputKey"), str) and isinstance(row.get("OutputValue"), str)
        for row in outputs), "AWS log stack does not expose the qualification companion outputs")
    values = {row["OutputKey"]: row["OutputValue"] for row in outputs}
    viewer_prefix = f"AWSLogs/{owner}/CloudFront/"
    require(values.keys() == {"LogBucket", "OriginBucket", "DistributionId", "ViewerLogPrefix", "OriginLogPrefix"}
            and values["OriginBucket"] == origin
            and values["DistributionId"] == expected_distribution and values["ViewerLogPrefix"] == viewer_prefix
            and values["OriginLogPrefix"] == "origin/", "AWS log stack does not match the approved CloudFront fixture")
    bucket = origin_bucket(f"s3://{values['LogBucket']}", "cloudfront")
    require(bucket != origin, "AWS raw logs require a separate destination bucket")
    logging = aws(profile, REGION, "s3api", "get-bucket-logging", "--bucket", origin, "--expected-bucket-owner", owner)
    status = logging.get("LoggingEnabled")
    require(isinstance(status, dict) and status.get("TargetBucket") == bucket and status.get("TargetPrefix") == "origin/"
            and not status.get("TargetGrants")
            and status.get("TargetObjectKeyFormat") == {"PartitionedPrefix": {"PartitionDateSource": "EventTime"}},
            "S3 origin access logging needs EventTime partitions in the approved companion bucket and prefix")
    return owner, bucket, viewer_prefix, f"origin/{owner}/{REGION}/{origin}/"


def log_objects(profile: str | None, owner: str, bucket: str, prefix: str, start: int) -> list[tuple[str, int, str]]:
    listing = aws(profile, REGION, "s3api", "list-objects-v2", "--bucket", bucket, "--expected-bucket-owner", owner,
                  "--prefix", prefix, "--max-keys", str(LIST_LIMIT), "--no-paginate")
    rows = listing.get("Contents", [])
    require(listing.get("IsTruncated") is False and isinstance(rows, list) and len(rows) <= LIST_LIMIT,
            "AWS qualification log prefix exceeds the bounded inventory")
    selected = []
    keys = set()
    for row in rows:
        require(isinstance(row, dict) and isinstance(row.get("Key"), str) and row["Key"].startswith(prefix)
                and row["Key"] not in keys and matches(ETAG, row.get("ETag"))
                and type(row.get("Size")) is int and row["Size"] >= 0 and isinstance(row.get("LastModified"), str),
                "invalid or repeated qualification log object listing")
        modified = datetime.fromisoformat(row["LastModified"])
        require(modified.utcoffset() == timezone.utc.utcoffset(None), "AWS log object needs a UTC modification time")
        keys.add(row["Key"])
        # Log delivery can be delayed. Only exclude objects already in S3
        # before the read began, and let the collector match the request IDs.
        if row["Size"] and modified.timestamp() >= start:
            selected.append((row["Key"], row["Size"], row["ETag"]))
    return selected


def copy_log(profile: str | None, owner: str, bucket: str, obj: tuple[str, int, str],
             raw: Path, target: Path, remaining: int) -> int:
    key, size, etag = obj
    metadata = aws(profile, REGION, "s3api", "get-object", "--bucket", bucket, "--expected-bucket-owner", owner,
                   "--key", key, "--if-match", etag, str(raw))
    require(metadata.get("ContentLength") == size and metadata.get("ETag") == etag and raw.stat().st_size == size,
            "downloaded log object disagrees with the listed snapshot")
    with raw.open("rb") as source, target.open("xb") as destination:
        compressed = source.read(2) == b"\x1f\x8b"
        source.seek(0)
        try:
            with (gzip.GzipFile(fileobj=source) if compressed else source) as body:
                while data := body.read(64 * 1024):
                    remaining -= len(data)
                    require(remaining >= 0, "AWS qualification logs exceed the expanded size limit")
                    destination.write(data)
        except EOFError:
            raise ValueError("truncated CloudFront log object") from None
    raw.unlink()
    return remaining


def event_prefixes(prefix: str, start: int, finish: int, interval: int, date_format: str) -> list[str]:
    return [prefix + datetime.fromtimestamp(at, timezone.utc).strftime(date_format)
            for at in range(start // interval * interval, finish + 1, interval)]


def download_logs(fixture: dict, client: dict, stack: str, profile: str | None, scratch: Path) -> tuple[list[Path], list[Path]]:
    started, finished = (client.get(key) for key in ("started_at_unix_ms", "finished_at_unix_ms"))
    require(positive(started) and positive(finished) and 0 <= finished - started <= HOUR * 1000,
            "AWS log download needs a timed client action of at most one hour")
    owner, bucket, viewer_prefix, origin_prefix = log_destination(fixture, stack, profile)
    since, until = started // 1000, finished // 1000
    # Search by event time so delayed delivery stays in the measured partitions.
    # Viewer timestamps can straddle an hour when the runner clock differs.
    viewer_start = max(0, since - 60)
    viewers = [obj for prefix in event_prefixes(viewer_prefix, viewer_start, until + 60, HOUR, "%Y/%m/%d/%H/")
               for obj in log_objects(profile, owner, bucket, prefix, viewer_start)]
    origins = [obj for prefix in event_prefixes(origin_prefix, since, until, DAY, "%Y/%m/%d/")
               for obj in log_objects(profile, owner, bucket, prefix, since)]
    require(len(viewers) + len(origins) <= DOWNLOAD_LIMIT, "too many log objects for one qualification read")
    require(sum(obj[1] for obj in viewers + origins) <= LOG_BYTES,
            "AWS qualification logs exceed the download size limit")
    remaining = LOG_BYTES
    result: list[list[Path]] = [[], []]
    for stream, objects in enumerate((viewers, origins)):
        for index, obj in enumerate(objects):
            destination = scratch / f"{stream}-{index}.log"
            remaining = copy_log(profile, owner, bucket, obj, scratch / "download", destination, remaining)
            result[stream].append(destination)
    return result[0], result[1]
