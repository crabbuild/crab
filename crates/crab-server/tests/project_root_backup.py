"""Full-root copy proof for the isolated project process qualification."""

from concurrent.futures import ThreadPoolExecutor
import datetime
import hashlib
import hmac
import json
import os
import shutil
import subprocess
import time
import urllib.parse
import urllib.request
import uuid


def copy_root(storage_url, artifacts):
    endpoint = os.environ["AWS_ENDPOINT_URL_S3"].rstrip("/")
    assert urllib.parse.urlsplit(endpoint).hostname in ["127.0.0.1", "localhost"]
    source = urllib.parse.urlsplit(storage_url)
    assert source.scheme == "s3" and source.netloc and source.path.strip("/")
    bucket = source.netloc
    prefix = source.path.strip("/") + "/"
    destination = "crab-project-restore-" + uuid.uuid4().hex
    aws = shutil.which("aws")
    assert aws, "AWS CLI is required"
    environment = dict(os.environ)
    config = artifacts / "aws-config"
    config.write_text("[default]\nregion = us-east-1\ns3 =\n    max_concurrent_requests = 32\n    multipart_threshold = 64MB\n    multipart_chunksize = 32MB\n")
    environment["AWS_CONFIG_FILE"] = str(config)

    def cli(*arguments, output=False):
        with (artifacts / "storage.log").open("ab") as log:
            result = subprocess.run([aws, "--endpoint-url", endpoint, *arguments], env=environment,
                                    stdout=subprocess.PIPE if output else log, stderr=log, check=True)
        return json.loads(result.stdout) if output else None

    def inventory(name):
        objects = {}
        token = None
        while True:
            arguments = ["s3api", "list-objects-v2", "--bucket", name, "--prefix", prefix, "--no-paginate"]
            if token:
                arguments += ["--continuation-token", token]
            page = cli(*arguments, output=True)
            for item in page.get("Contents", []):
                objects[item["Key"]] = {key: item[key] for key in ["Size", "ETag"]}
            if not page.get("IsTruncated"):
                return objects
            token = page["NextContinuationToken"]

    def fingerprint(name, key):
        now = datetime.datetime.now(datetime.timezone.utc)
        day, stamp = now.strftime("%Y%m%d"), now.strftime("%Y%m%dT%H%M%SZ")
        region = environment.get("AWS_REGION", "us-east-1")
        host = urllib.parse.urlsplit(endpoint).netloc
        path = "/" + name + "/" + urllib.parse.quote(key, safe="/")
        digest = hashlib.sha256(b"").hexdigest()
        headers = {"host": host, "x-amz-content-sha256": digest, "x-amz-date": stamp}
        if environment.get("AWS_SESSION_TOKEN"):
            headers["x-amz-security-token"] = environment["AWS_SESSION_TOKEN"]
        signed = ";".join(sorted(headers))
        canonical = "".join(f"{key}:{headers[key]}\n" for key in sorted(headers))
        scope = f"{day}/{region}/s3/aws4_request"
        canonical_request = "\n".join(["GET", path, "", canonical, signed, digest])
        to_sign = "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hashlib.sha256(canonical_request.encode()).hexdigest()
        signing = ("AWS4" + environment["AWS_SECRET_ACCESS_KEY"]).encode()
        for term in [day, region, "s3", "aws4_request"]:
            signing = hmac.new(signing, term.encode(), hashlib.sha256).digest()
        signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
        headers["authorization"] = f"AWS4-HMAC-SHA256 Credential={environment['AWS_ACCESS_KEY_ID']}/{scope}, SignedHeaders={signed}, Signature={signature}"
        payload = hashlib.sha256()
        size = 0
        with urllib.request.urlopen(urllib.request.Request(endpoint + path, headers=headers), timeout=60) as response:
            # Timestamps on native LTX objects are application metadata. Copying
            # bytes alone would not prove the restored persistence contract.
            metadata = {key.lower(): value for key, value in response.headers.items() if key.lower().startswith("x-amz-meta-")}
            while chunk := response.read(1024 * 1024):
                payload.update(chunk)
                size += len(chunk)
        return {"bytes": size, "sha256": payload.hexdigest(), "metadata": metadata}

    before = inventory(bucket)
    assert before, "The configured storage root is empty"
    total = sum(item["Size"] for item in before.values())
    assert shutil.disk_usage(artifacts).free > total + 8 * 1024**3, "Insufficient space for an independent root and restore cache"
    cli("s3api", "create-bucket", "--bucket", destination)
    restored_url = f"s3://{destination}/{prefix}"
    (artifacts / "restore-root.json").write_text(json.dumps({"source": storage_url, "destination": restored_url}))
    started = time.monotonic()
    print(json.dumps({"phase": "root_copy", "objects": len(before), "bytes": total}), flush=True)
    cli("s3", "sync", storage_url.rstrip("/") + "/", restored_url, "--copy-props", "default", "--only-show-errors")
    restored = inventory(destination)
    assert {key: value["Size"] for key, value in before.items()} == {key: value["Size"] for key, value in restored.items()}, "Restored key/size inventory differs"

    def compare(key):
        original = fingerprint(bucket, key)
        assert original["bytes"] == before[key]["Size"], key
        assert fingerprint(destination, key) == original, f"Restored bytes or metadata differ: {key}"
        return key, original

    with (artifacts / "verified-objects.jsonl").open("w") as log, ThreadPoolExecutor(max_workers=16) as workers:
        for count, (key, proof) in enumerate(workers.map(compare, before), 1):
            log.write(json.dumps({"key": key, **proof}) + "\n")
            if count % 1000 == 0:
                log.flush()
                print(json.dumps({"phase": "root_hash", "verified_objects": count, "objects": len(before)}), flush=True)
    assert inventory(bucket) == before, "Source root changed while writers were stopped"
    assert inventory(destination) == restored, "Destination changed before restore startup"
    proof = {"source": storage_url, "destination": restored_url, "objects": len(before), "bytes": total, "sha256_and_metadata_verified": True, "elapsed_seconds": time.monotonic() - started}
    (artifacts / "root-copy-proof.json").write_text(json.dumps(proof, indent=2))

    def cleanup():
        # This bucket was created above with a fresh UUID. The source bucket is
        # never a cleanup target, including when restoration assertions fail.
        cli("s3", "rb", "s3://" + destination, "--force", "--only-show-errors")

    return restored_url, proof, cleanup
