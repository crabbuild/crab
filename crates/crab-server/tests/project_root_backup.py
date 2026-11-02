"""Full-root copy proof for the isolated project process qualification."""

from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
import shutil
import tempfile
import time
import urllib.parse
import uuid


def copy_root(storage_url, artifacts):
    import boto3
    from boto3.s3.transfer import TransferConfig
    from botocore.config import Config

    endpoint = os.environ["AWS_ENDPOINT_URL_S3"].rstrip("/")
    assert urllib.parse.urlsplit(endpoint).hostname in ["127.0.0.1", "localhost"]
    source = urllib.parse.urlsplit(storage_url)
    assert source.scheme == "s3" and source.netloc and source.path.strip("/")
    bucket = source.netloc
    prefix = source.path.strip("/") + "/"
    destination = "crab-project-restore-" + uuid.uuid4().hex
    client = boto3.client(
        "s3", endpoint_url=endpoint, region_name=os.environ.get("AWS_REGION", "us-east-1"),
        aws_access_key_id=os.environ["AWS_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["AWS_SECRET_ACCESS_KEY"],
        aws_session_token=os.environ.get("AWS_SESSION_TOKEN"),
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"},
                      proxies={}, connect_timeout=10, read_timeout=90, max_pool_connections=32,
                      retries={"total_max_attempts": 3, "mode": "standard"},
                      request_checksum_calculation="when_required", response_checksum_validation="when_required"),
    )
    transfer = TransferConfig(multipart_threshold=64 * 1024**2, multipart_chunksize=32 * 1024**2,
                              max_concurrency=1, use_threads=False)
    properties = ["CacheControl", "ContentDisposition", "ContentEncoding", "ContentLanguage", "ContentType", "Expires"]

    def inventory(name):
        objects = {}
        for page in client.get_paginator("list_objects_v2").paginate(Bucket=name, Prefix=prefix):
            for item in page.get("Contents", []):
                objects[item["Key"]] = {key: item[key] for key in ["Size", "ETag"]}
        return objects

    def fingerprint(response, output=None):
        payload, size = hashlib.sha256(), 0
        try:
            while chunk := response["Body"].read(1024 * 1024):
                payload.update(chunk)
                size += len(chunk)
                if output is not None:
                    output.write(chunk)
        finally:
            response["Body"].close()
        return {"bytes": size, "sha256": payload.hexdigest(), "metadata": response.get("Metadata", {})}

    before = inventory(bucket)
    assert before, "The configured storage root is empty"
    total = sum(item["Size"] for item in before.values())
    assert shutil.disk_usage(artifacts).free > total + 8 * 1024**3, "Insufficient space for an independent root and restore cache"
    client.create_bucket(Bucket=destination)
    restored_url = f"s3://{destination}/{prefix}"
    (artifacts / "restore-root.json").write_text(json.dumps({"source": storage_url, "destination": restored_url}))
    (artifacts / "source-inventory.json").write_text(json.dumps(before))
    started = time.monotonic()
    print(json.dumps({"phase": "root_copy", "objects": len(before), "bytes": total}), flush=True)

    def copy(key):
        response = client.get_object(Bucket=bucket, Key=key, IfMatch=before[key]["ETag"])
        extra = {name: response[name] for name in properties if name in response}
        extra["Metadata"] = response.get("Metadata", {})
        # Client-side transfer proves independent bytes without depending on a
        # provider's server-side CopyObject implementation or retaining a cache.
        with tempfile.TemporaryFile(dir=artifacts) as temporary:
            original = fingerprint(response, temporary)
            assert original["bytes"] == before[key]["Size"], key
            tags = client.get_object_tagging(Bucket=bucket, Key=key)["TagSet"]
            if tags:
                extra["Tagging"] = urllib.parse.urlencode([(tag["Key"], tag["Value"]) for tag in tags])
            temporary.seek(0)
            client.upload_fileobj(temporary, destination, key, ExtraArgs=extra, Config=transfer)
        restored = client.get_object(Bucket=destination, Key=key)
        assert fingerprint(restored) == original, f"Restored bytes or metadata differ: {key}"
        assert all(restored.get(name) == response.get(name) for name in properties), key
        assert sorted(client.get_object_tagging(Bucket=destination, Key=key)["TagSet"], key=lambda tag: tag["Key"]) == sorted(tags, key=lambda tag: tag["Key"]), key
        return key, original

    workers = ThreadPoolExecutor(max_workers=8)
    futures = [workers.submit(copy, key) for key in before]
    try:
        with (artifacts / "verified-objects.jsonl").open("w") as log:
            for count, future in enumerate(as_completed(futures), 1):
                key, proof = future.result()
                log.write(json.dumps({"key": key, **proof}) + "\n")
                if count % 1000 == 0:
                    log.flush()
                    print(json.dumps({"phase": "root_hash", "verified_objects": count, "objects": len(before)}), flush=True)
    finally:
        workers.shutdown(wait=True, cancel_futures=True)
    restored = inventory(destination)
    assert {key: value["Size"] for key, value in before.items()} == {key: value["Size"] for key, value in restored.items()}, "Restored key/size inventory differs"
    assert inventory(bucket) == before, "Source root changed while writers were stopped"
    assert inventory(destination) == restored, "Destination changed before restore startup"
    proof = {"source": storage_url, "destination": restored_url, "objects": len(before), "bytes": total, "sha256_and_metadata_verified": True, "elapsed_seconds": time.monotonic() - started}
    (artifacts / "root-copy-proof.json").write_text(json.dumps(proof, indent=2))

    def cleanup():
        # This fresh bucket belongs only to this run. Source coordination and
        # released writer ledgers are never individual cleanup targets.
        while objects := client.list_objects_v2(Bucket=destination, MaxKeys=1000).get("Contents", []):
            result = client.delete_objects(Bucket=destination, Delete={"Objects": [{"Key": item["Key"]} for item in objects], "Quiet": True})
            assert not result.get("Errors"), "Restored fixture cleanup failed"
        client.delete_bucket(Bucket=destination)

    return restored_url, proof, cleanup
