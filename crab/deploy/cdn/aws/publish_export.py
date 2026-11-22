#!/usr/bin/env python3
"""Publish an approved Crab inventory to the dedicated CloudFront export bucket."""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from public_inventory import (
    CACHE_CONTROL, CONTENT_TYPE, ExportError, publication_receipt, read_inventory, verify_body, viewer_url,
    write_publication,
)


BUCKET = re.compile(r"s3://([a-z0-9][a-z0-9.-]{1,61}[a-z0-9])")
MAX_OBJECT = 5_000_000_000


class AwsError(ExportError):
    def __init__(self, operation, stderr):
        match = re.search(r"An error occurred \(([A-Za-z0-9]+)\)", stderr)
        self.code = match.group(1) if match else None
        super().__init__(f"AWS {operation} failed ({self.code or 'unclassified'})")


def bucket(origin):
    match = BUCKET.fullmatch(origin) if isinstance(origin, str) else None
    if not match:
        raise ExportError("expected a native s3:// bucket")
    return match.group(1)


def aws(profile, region, service, operation, *arguments):
    result = subprocess.run(
        ["aws", "--profile", profile, "--region", region, "--no-cli-pager",
         "--no-cli-auto-prompt", "--output", "json", service, operation, *arguments],
        check=False, capture_output=True, text=True,
        env={**os.environ, "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS": "true"},
    )
    if result.returncode:
        raise AwsError(operation, result.stderr)
    return json.loads(result.stdout)


def verify_placement(profile, origin, placement):
    region = placement["location"]
    owner = placement["resource"].split("/")[1]
    response = aws(profile, region, "s3api", "get-bucket-location", "--bucket", bucket(origin),
                   "--expected-bucket-owner", owner)
    observed = response["LocationConstraint"]
    observed = "us-east-1" if observed is None else "eu-west-1" if observed == "EU" else observed
    if observed != region:
        raise ExportError("S3 bucket region differs from the approved placement")


def find_export(options, source, prefix, digest):
    response = aws(options.operator_profile, options.region, "cloudformation", "describe-stacks",
                   "--stack-name", options.stack)
    stack, = response["Stacks"]
    if stack["StackStatus"] not in ("CREATE_COMPLETE", "UPDATE_COMPLETE"):
        raise ExportError("export stack must have completed its deployment")
    owner = re.fullmatch(r"arn:aws:cloudformation:[a-z0-9-]+:([0-9]{12}):stack/.+",
                         stack["StackId"])
    outputs = {output["OutputKey"]: output["OutputValue"] for output in stack["Outputs"]}
    if (not owner or outputs.get("SourceOrigin") != source
            or outputs.get("RepositoryPrefix") != prefix
            or outputs.get("ApprovedInventorySha256") != digest
            or outputs.get("PublicDeliveryEnabled") != "false"):
        raise ExportError("export stack must approve this inventory with public delivery disabled")
    export = bucket(outputs.get("ExportOrigin"))
    if export == bucket(source):
        raise ExportError("export stack must use a separate destination bucket")
    resource = aws(
        options.operator_profile, options.region, "cloudformation", "describe-stack-resource",
        "--stack-name", stack["StackId"], "--logical-resource-id", "ExportBucket",
    )["StackResourceDetail"]
    if resource.get("ResourceType") != "AWS::S3::Bucket" or resource.get("PhysicalResourceId") != export:
        raise ExportError("export output does not identify the stack's dedicated bucket")
    distribution_id = outputs["DistributionId"]
    if not re.fullmatch(r"[A-Z0-9]{1,64}", distribution_id):
        raise ExportError("invalid CloudFront distribution identity")
    distribution = aws(options.operator_profile, options.region, "cloudfront", "get-distribution",
                       "--id", distribution_id)["Distribution"]
    arn = f"arn:aws:cloudfront::{owner[1]}:distribution/{distribution_id}"
    base_url = viewer_url("https://" + distribution["DomainName"])
    config = distribution["DistributionConfig"]
    origins = config["Origins"]["Items"]
    if (distribution["Id"] != distribution_id or distribution["ARN"] != arn
            or distribution["Status"] != "Deployed" or config["Enabled"] is not False
            or base_url != outputs["BaseUrl"] or not base_url.endswith(".cloudfront.net")
            or len(origins) != 1 or origins[0].get("OriginPath")
            or origins[0]["DomainName"] != f"{export}.s3.{options.region}.amazonaws.com"):
        raise ExportError("CloudFront does not serve the staged export bucket and viewer hostname")
    return export, owner[1], arn, base_url


def download_verified(profile, region, location, obj, path):
    meta = aws(profile, region, "s3api", "head-object", *location)
    if meta.get("ContentLength") != obj.size or not meta.get("ETag"):
        raise ExportError(f"inventory size or source identity mismatch for {obj.path}")
    received = aws(profile, region, "s3api", "get-object", *location, "--if-match", meta["ETag"], str(path))
    verify_body(obj, path)
    return received


def publish(options):
    source, source_placement, prefix, digest, objects = read_inventory(options.inventory, bucket, MAX_OBJECT)
    source_bucket = bucket(source)
    staged = find_export(options, source, prefix, digest)
    export, owner, distribution, base_url = staged
    export_origin = f"s3://{export}"
    export_placement = {"resource": f"accounts/{owner}/buckets/{export}", "location": options.region}
    verify_placement(options.source_profile, source, source_placement)
    verify_placement(options.operator_profile, export_origin, export_placement)
    source_owner = source_placement["resource"].split("/")[1]
    source_region = source_placement["location"]
    with tempfile.TemporaryDirectory(
        prefix="crab-cdn-export-", dir=options.scratch_directory
    ) as scratch:
        for obj in objects:
            path = Path(scratch) / "payload"
            key = f"{prefix}/{obj.path}"
            download_verified(options.source_profile, source_region,
                              ["--bucket", source_bucket, "--key", key, "--expected-bucket-owner", source_owner], obj, path)
            destination = ["--bucket", export, "--key", key, "--expected-bucket-owner", owner]
            checksum = base64.b64encode(bytes.fromhex(obj.sha256)).decode("ascii")
            try:
                result = aws(
                    options.writer_profile, options.region, "s3api", "put-object", *destination,
                    "--body", str(path), "--if-none-match", "*", "--checksum-sha256", checksum,
                    "--content-type", CONTENT_TYPE,
                    "--cache-control", CACHE_CONTROL,
                )
            except AwsError as error:
                if error.code != "PreconditionFailed":
                    raise
            else:
                if result.get("ChecksumSHA256") != checksum:
                    raise ExportError(f"export upload returned a different checksum for {obj.path}")
            path.unlink()
            existing = download_verified(options.writer_profile, options.region, destination, obj, path)
            if (existing.get("ContentType") != CONTENT_TYPE or existing.get("CacheControl") != CACHE_CONTROL
                    or any(existing.get(field) for field in (
                        "ContentEncoding", "ContentDisposition", "ContentLanguage", "Metadata",
                    ))):
                raise ExportError(f"export representation differs for {obj.path}")
            path.unlink()
    if find_export(options, source, prefix, digest) != staged:
        raise ExportError("export target changed during publication")
    verify_placement(options.source_profile, source, source_placement)
    verify_placement(options.operator_profile, export_origin, export_placement)
    return publication_receipt(source, export_origin, source_placement, export_placement,
                               prefix, base_url, distribution, digest, objects)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("--stack", required=True)
    parser.add_argument("--region", required=True)
    parser.add_argument("--operator-profile", required=True)
    parser.add_argument("--source-profile", required=True)
    parser.add_argument("--writer-profile", required=True)
    parser.add_argument("--scratch-directory", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    options = parser.parse_args()
    try:
        print(json.dumps(write_publication(options.receipt, lambda: publish(options))))
    except (ExportError, ValueError, OSError, KeyError) as error:
        print(f"Export stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
