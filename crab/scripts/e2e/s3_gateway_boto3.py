#!/usr/bin/env python3
"""Exercise the gateway with the pinned official Boto3 S3 client."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import secrets
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError


def _error_code(error: ClientError) -> str:
    return str(error.response.get("Error", {}).get("Code", ""))


def _expect_error(operation: str, callback: Callable[[], object], expected: str) -> None:
    try:
        callback()
    except ClientError as error:
        actual = _error_code(error)
        if actual != expected:
            raise RuntimeError(f"{operation} returned {actual}, expected {expected}") from error
    else:
        raise RuntimeError(f"{operation} unexpectedly succeeded")


def _write_report(path: Path | None, report: dict[str, object]) -> None:
    encoded = json.dumps(report, sort_keys=True) + "\n"
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(encoded, encoding="utf-8")
    temporary.replace(path)


def _write_large_fixture(path: Path, size: int) -> str:
    digest = hashlib.sha256()
    block_size = 1024 * 1024
    offset = 0
    with path.open("wb") as output:
        while offset < size:
            length = min(block_size, size - offset)
            seed = hashlib.sha256(f"crab-s3-gateway-large:{offset}".encode()).digest()
            block = (seed * ((length + len(seed) - 1) // len(seed)))[:length]
            output.write(block)
            digest.update(block)
            offset += length
    return digest.hexdigest()


def _digest_file(path: Path) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(8 * 1024 * 1024), b""):
            digest.update(block)
            size += len(block)
    return size, digest.hexdigest()


def _digest_range(path: Path, start: int, length: int) -> tuple[int, str]:
    digest = hashlib.sha256()
    remaining = length
    with path.open("rb") as stream:
        stream.seek(start)
        while remaining:
            block = stream.read(min(8 * 1024 * 1024, remaining))
            if not block:
                raise RuntimeError("large-object range source ended early")
            digest.update(block)
            remaining -= len(block)
    return length, digest.hexdigest()


def _qualify_large_object(
    client, bucket: str, key: str, size: int, temporary_root: Path
) -> dict[str, object]:
    part_size = 64 * 1024 * 1024
    if size < part_size + 8192 or size > 5 * 1024**4:
        raise ValueError("large-object size must be between 64 MiB and 5 TiB")
    source_path = temporary_root / "large-input.bin"
    output_path = temporary_root / "large-output.bin"
    source_digest = _write_large_fixture(source_path, size)
    part_count = (size + part_size - 1) // part_size
    upload_id = None
    completed = False
    upload_started = time.monotonic_ns()
    try:
        upload_id = client.create_multipart_upload(Bucket=bucket, Key=key)["UploadId"]
        parts = []
        with source_path.open("rb") as source:
            for number in range(1, part_count + 1):
                length = min(part_size, size - (number - 1) * part_size)
                payload = source.read(length)
                if len(payload) != length:
                    raise RuntimeError("large-object multipart source ended early")
                result = client.upload_part(
                    Bucket=bucket,
                    Key=key,
                    UploadId=upload_id,
                    PartNumber=number,
                    Body=payload,
                    ContentLength=length,
                )
                parts.append({"PartNumber": number, "ETag": result["ETag"]})
        client.complete_multipart_upload(
            Bucket=bucket,
            Key=key,
            UploadId=upload_id,
            MultipartUpload={"Parts": parts},
        )
        completed = True
        upload_elapsed_ms = max(1, (time.monotonic_ns() - upload_started) // 1_000_000)

        head = client.head_object(Bucket=bucket, Key=key)
        if head.get("ContentLength") != size:
            raise RuntimeError("large-object HeadObject returned the wrong size")
        get_started = time.monotonic_ns()
        response = client.get_object(Bucket=bucket, Key=key)
        try:
            with output_path.open("wb") as output:
                for block in iter(lambda: response["Body"].read(8 * 1024 * 1024), b""):
                    output.write(block)
        finally:
            response["Body"].close()
        get_elapsed_ms = max(1, (time.monotonic_ns() - get_started) // 1_000_000)
        output_size, output_digest = _digest_file(output_path)
        if output_size != size or output_digest != source_digest:
            raise RuntimeError("large-object full GET was not byte exact")

        range_start = part_size - 4096
        range_length = min(part_size + 8192, size - range_start)
        range_started = time.monotonic_ns()
        response = client.get_object(
            Bucket=bucket,
            Key=key,
            Range=f"bytes={range_start}-{range_start + range_length - 1}",
        )
        try:
            range_bytes = response["Body"].read()
        finally:
            response["Body"].close()
        range_elapsed_ms = max(1, (time.monotonic_ns() - range_started) // 1_000_000)
        expected_range_size, expected_range_digest = _digest_range(
            source_path, range_start, range_length
        )
        actual_range_digest = hashlib.sha256(range_bytes).hexdigest()
        if len(range_bytes) != expected_range_size or actual_range_digest != expected_range_digest:
            raise RuntimeError("large-object range GET was not byte exact")
        return {
            "bytes": size,
            "part_bytes": part_size,
            "parts": part_count,
            "source_sha256": source_digest,
            "full_get_sha256": output_digest,
            "range_start": range_start,
            "range_bytes": len(range_bytes),
            "range_sha256": actual_range_digest,
            "range_source_sha256": expected_range_digest,
            "range_exact": True,
            "upload_elapsed_ms": upload_elapsed_ms,
            "full_get_elapsed_ms": get_elapsed_ms,
            "range_get_elapsed_ms": range_elapsed_ms,
        }
    finally:
        if upload_id is not None and not completed:
            client.abort_multipart_upload(Bucket=bucket, Key=key, UploadId=upload_id)


def run(
    client, bucket: str, prefix: str, large_object_bytes: int = 0
) -> dict[str, object]:
    token = secrets.token_hex(8)
    root = f"{prefix}/{token}"
    object_key = f"{root}/object.bin"
    copy_key = f"{root}/copy.bin"
    conditional_key = f"{root}/conditional.bin"
    multipart_key = f"{root}/multipart.bin"
    delete_key = f"{root}/delete.bin"
    conditional_delete_key = f"{root}/conditional-delete.bin"
    large_key = f"{root}/large.bin"
    keys = [
        object_key,
        copy_key,
        conditional_key,
        multipart_key,
        delete_key,
        conditional_delete_key,
        large_key,
    ]
    body = b"boto3 gateway qualification\n" + bytes(range(256)) * 4096
    multipart_parts = [b"a" * (5 * 1024 * 1024), b"boto3-final-part"]
    upload_id = None
    completed = False
    checks: dict[str, bool] = {}

    try:
        put = client.put_object(
            Bucket=bucket,
            Key=object_key,
            Body=io.BytesIO(body),
            ContentType="application/octet-stream",
            Metadata={"client": "boto3"},
            Tagging="client=boto3&team=storage",
        )
        expected_etag = hashlib.md5(body).hexdigest()
        if put["ETag"].strip('"') != expected_etag:
            raise RuntimeError("Boto3 PutObject returned an unexpected ETag")
        checks["put"] = True

        head = client.head_object(Bucket=bucket, Key=object_key)
        if head.get("Metadata", {}).get("client") != "boto3":
            raise RuntimeError("Boto3 HeadObject did not preserve metadata")
        checks["head"] = True

        response = client.get_object(Bucket=bucket, Key=object_key)
        if response["Body"].read() != body:
            raise RuntimeError("Boto3 GetObject returned different bytes")
        ranged = client.get_object(Bucket=bucket, Key=object_key, Range="bytes=17-4096")
        if ranged["Body"].read() != body[17 : 4097]:
            raise RuntimeError("Boto3 ranged GetObject returned different bytes")
        checks["get_and_range"] = True

        tagged = client.get_object_tagging(Bucket=bucket, Key=object_key)
        tag_set = {(tag["Key"], tag["Value"]) for tag in tagged["TagSet"]}
        if tag_set != {("client", "boto3"), ("team", "storage")}:
            raise RuntimeError("Boto3 object tags were not preserved")
        client.put_object_tagging(
            Bucket=bucket,
            Key=object_key,
            Tagging={"TagSet": [{"Key": "updated", "Value": "yes"}]},
        )
        updated = client.get_object_tagging(Bucket=bucket, Key=object_key)
        if updated["TagSet"] != [{"Key": "updated", "Value": "yes"}]:
            raise RuntimeError("Boto3 PutObjectTagging did not publish the replacement tags")
        client.delete_object_tagging(Bucket=bucket, Key=object_key)
        if client.get_object_tagging(Bucket=bucket, Key=object_key)["TagSet"]:
            raise RuntimeError("Boto3 DeleteObjectTagging did not clear tags")
        checks["tagging"] = True

        _expect_error(
            "PutObject If-None-Match",
            lambda: client.put_object(
                Bucket=bucket,
                Key=object_key,
                Body=io.BytesIO(b"replacement"),
                IfNoneMatch="*",
            ),
            "PreconditionFailed",
        )
        client.put_object(Bucket=bucket, Key=conditional_key, Body=b"conditional")
        _expect_error(
            "DeleteObject stale If-Match",
            lambda: client.delete_object(
                Bucket=bucket,
                Key=conditional_key,
                IfMatch='"00000000000000000000000000000000"',
            ),
            "PreconditionFailed",
        )
        conditional_head = client.head_object(Bucket=bucket, Key=conditional_key)
        client.delete_object(
            Bucket=bucket,
            Key=conditional_key,
            IfMatch=conditional_head["ETag"],
        )
        _expect_error(
            "DeleteObject missing If-Match",
            lambda: client.delete_object(
                Bucket=bucket,
                Key=f"{root}/missing.bin",
                IfMatch='"00000000000000000000000000000000"',
            ),
            "PreconditionFailed",
        )
        checks["conditions"] = True

        client.copy_object(
            Bucket=bucket,
            Key=copy_key,
            CopySource={"Bucket": bucket, "Key": object_key},
            MetadataDirective="COPY",
            IfNoneMatch="*",
        )
        if client.get_object(Bucket=bucket, Key=copy_key)["Body"].read() != body:
            raise RuntimeError("Boto3 CopyObject returned different bytes")
        _expect_error(
            "CopyObject destination If-None-Match",
            lambda: client.copy_object(
                Bucket=bucket,
                Key=copy_key,
                CopySource={"Bucket": bucket, "Key": object_key},
                IfNoneMatch="*",
            ),
            "PreconditionFailed",
        )
        copy_head = client.head_object(Bucket=bucket, Key=copy_key)
        _expect_error(
            "CopyObject destination stale If-Match",
            lambda: client.copy_object(
                Bucket=bucket,
                Key=copy_key,
                CopySource={"Bucket": bucket, "Key": object_key},
                IfMatch='"00000000000000000000000000000000"',
            ),
            "PreconditionFailed",
        )
        client.copy_object(
            Bucket=bucket,
            Key=copy_key,
            CopySource={"Bucket": bucket, "Key": object_key},
            IfMatch=copy_head["ETag"],
        )
        checks["copy"] = True
        checks["copy_destination_conditions"] = True

        listing = client.list_objects_v2(Bucket=bucket, Prefix=root, MaxKeys=1000)
        listed = {entry["Key"] for entry in listing.get("Contents", [])}
        if not {object_key, copy_key}.issubset(listed):
            raise RuntimeError("Boto3 ListObjectsV2 omitted gateway objects")
        checks["list"] = True

        upload = client.create_multipart_upload(Bucket=bucket, Key=multipart_key)
        upload_id = upload["UploadId"]
        uploaded = []
        for number, part in enumerate(multipart_parts, start=1):
            result = client.upload_part(
                Bucket=bucket,
                Key=multipart_key,
                UploadId=upload_id,
                PartNumber=number,
                Body=io.BytesIO(part),
            )
            uploaded.append({"PartNumber": number, "ETag": result["ETag"]})
        listed_parts = client.list_parts(
            Bucket=bucket,
            Key=multipart_key,
            UploadId=upload_id,
        )
        if [part["PartNumber"] for part in listed_parts["Parts"]] != [1, 2]:
            raise RuntimeError("Boto3 ListParts returned the wrong part sequence")
        client.complete_multipart_upload(
            Bucket=bucket,
            Key=multipart_key,
            UploadId=upload_id,
            MultipartUpload={"Parts": uploaded},
        )
        completed = True
        expected_multipart = b"".join(multipart_parts)
        if client.get_object(Bucket=bucket, Key=multipart_key)["Body"].read() != expected_multipart:
            raise RuntimeError("Boto3 multipart completion returned different bytes")
        checks["multipart"] = True

        client.put_object(Bucket=bucket, Key=delete_key, Body=b"delete")
        client.put_object(
            Bucket=bucket,
            Key=conditional_delete_key,
            Body=b"conditional delete",
        )
        conditional_delete_head = client.head_object(
            Bucket=bucket,
            Key=conditional_delete_key,
        )
        stale_delete = client.delete_objects(
            Bucket=bucket,
            Delete={
                "Objects": [
                    {
                        "Key": conditional_delete_key,
                        "ETag": '"00000000000000000000000000000000"',
                    }
                ]
            },
        )
        stale_errors = stale_delete.get("Errors", [])
        if len(stale_errors) != 1 or stale_errors[0].get("Code") != "PreconditionFailed":
            raise RuntimeError("Boto3 conditional DeleteObjects did not report the stale ETag")
        client.head_object(Bucket=bucket, Key=conditional_delete_key)
        missing_delete = client.delete_objects(
            Bucket=bucket,
            Delete={"Objects": [{"Key": f"{root}/missing-delete.bin", "ETag": "*"}]},
        )
        missing_errors = missing_delete.get("Errors", [])
        if len(missing_errors) != 1 or missing_errors[0].get("Code") != "NoSuchKey":
            raise RuntimeError("Boto3 conditional DeleteObjects did not report the missing key")
        matched_delete = client.delete_objects(
            Bucket=bucket,
            Delete={
                "Objects": [
                    {
                        "Key": conditional_delete_key,
                        "ETag": conditional_delete_head["ETag"],
                    },
                    {"Key": delete_key, "ETag": "*"},
                ]
            },
        )
        if {entry["Key"] for entry in matched_delete.get("Deleted", [])} != {
            conditional_delete_key,
            delete_key,
        }:
            raise RuntimeError("Boto3 conditional DeleteObjects omitted successful deletes")
        client.delete_objects(
            Bucket=bucket,
            Delete={"Objects": [{"Key": object_key}, {"Key": copy_key}]},
        )
        checks["multi_delete"] = True
        checks["multi_delete_conditions"] = True
        large_object = None
        if large_object_bytes:
            with tempfile.TemporaryDirectory(prefix="crab-s3-gateway-boto3-") as directory:
                large_object = _qualify_large_object(
                    client,
                    bucket,
                    large_key,
                    large_object_bytes,
                    Path(directory),
                )
            checks["large_object_range"] = True
        return {
            "schema": "crab.s3-gateway-boto3-smoke",
            "status": "passed",
            "checks": checks,
            "object_bytes": len(body),
            "multipart_bytes": len(b"".join(multipart_parts)),
            "multipart_parts": len(multipart_parts),
            "large_object": large_object,
        }
    finally:
        if upload_id is not None and not completed:
            client.abort_multipart_upload(
                Bucket=bucket,
                Key=multipart_key,
                UploadId=upload_id,
            )
        remaining = []
        for key in keys:
            try:
                client.head_object(Bucket=bucket, Key=key)
            except ClientError:
                continue
            remaining.append({"Key": key})
        if remaining:
            client.delete_objects(Bucket=bucket, Delete={"Objects": remaining, "Quiet": True})


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--prefix", default="main/qualification/boto3")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument(
        "--large-object-bytes",
        type=int,
        default=0,
        help="run the sequential LFS-path multipart qualification at this size",
    )
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    prefix = args.prefix.strip("/")
    if not prefix or any(value in prefix for value in "?#"):
        parser.error("--prefix must be a non-empty S3 prefix without query or fragment")
    access_key = os.environ.get("S3_GATEWAY_BOTO3_ACCESS_KEY")
    secret_key = os.environ.get("S3_GATEWAY_BOTO3_SECRET_KEY")
    if not access_key or not secret_key:
        parser.error("S3_GATEWAY_BOTO3_ACCESS_KEY and S3_GATEWAY_BOTO3_SECRET_KEY are required")
    client = boto3.client(
        "s3",
        endpoint_url=args.endpoint,
        region_name=args.region,
        aws_access_key_id=access_key,
        aws_secret_access_key=secret_key,
        aws_session_token=os.environ.get("S3_GATEWAY_BOTO3_SESSION_TOKEN"),
        config=Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
            retries={"max_attempts": 0},
        ),
    )
    try:
        if args.large_object_bytes < 0:
            parser.error("--large-object-bytes must not be negative")
        report = run(client, args.bucket, prefix, args.large_object_bytes)
    except (ClientError, OSError, RuntimeError, ValueError) as error:
        print(f"error: Boto3 qualification failed: {error}")
        return 1
    _write_report(args.report, report)
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
