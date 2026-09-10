#!/usr/bin/env python3
"""Send an S3 SigV4 request whose payload has chained chunk signatures."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import hmac
import http.client
import os
import sys
import urllib.parse
from collections.abc import Iterable


ALGORITHM = "AWS4-HMAC-SHA256"
PAYLOAD_MODE = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _hmac(key: bytes, value: str) -> bytes:
    return hmac.new(key, value.encode("utf-8"), hashlib.sha256).digest()


def _signing_key(secret_key: str, date: str, region: str, service: str) -> bytes:
    date_key = _hmac(f"AWS4{secret_key}".encode("utf-8"), date)
    region_key = _hmac(date_key, region)
    service_key = _hmac(region_key, service)
    return _hmac(service_key, "aws4_request")


def _signature(key: bytes, value: str) -> str:
    return hmac.new(key, value.encode("utf-8"), hashlib.sha256).hexdigest()


def _canonical_request(
    method: str,
    path: str,
    headers: dict[str, str],
    signed_headers: Iterable[str],
) -> str:
    names = tuple(signed_headers)
    canonical_headers = "".join(f"{name}:{headers[name].strip()}\n" for name in names)
    return (
        f"{method}\n{path}\n\n{canonical_headers}\n"
        f"{';'.join(names)}\n{PAYLOAD_MODE}"
    )


def _seed_signature(
    secret_key: str,
    amz_date: str,
    region: str,
    service: str,
    canonical_request: str,
) -> tuple[str, bytes, str]:
    date = amz_date[:8]
    scope = f"{date}/{region}/{service}/aws4_request"
    string_to_sign = f"{ALGORITHM}\n{amz_date}\n{scope}\n{_sha256(canonical_request.encode())}"
    key = _signing_key(secret_key, date, region, service)
    return _signature(key, string_to_sign), key, scope


def _chunk_signature(
    key: bytes,
    amz_date: str,
    scope: str,
    previous_signature: str,
    chunk: bytes,
) -> str:
    string_to_sign = (
        f"AWS4-HMAC-SHA256-PAYLOAD\n{amz_date}\n{scope}\n"
        f"{previous_signature}\n{EMPTY_SHA256}\n{_sha256(chunk)}"
    )
    return _signature(key, string_to_sign)


def _encoded_body(
    chunks: list[bytes],
    key: bytes,
    amz_date: str,
    scope: str,
    seed_signature: str,
    tamper_first_chunk: bool,
) -> bytes:
    body = bytearray()
    previous_signature = seed_signature
    for index, chunk in enumerate(chunks):
        signature = _chunk_signature(key, amz_date, scope, previous_signature, chunk)
        previous_signature = signature
        if index == 0 and tamper_first_chunk:
            replacement = "0" if signature[-1] != "0" else "1"
            signature = f"{signature[:-1]}{replacement}"
        body.extend(f"{len(chunk):x};chunk-signature={signature}\r\n".encode())
        body.extend(chunk)
        body.extend(b"\r\n")

    final_signature = _chunk_signature(
        key, amz_date, scope, previous_signature, b""
    )
    body.extend(f"0;chunk-signature={final_signature}\r\n\r\n".encode())
    return bytes(body)


def _encoded_length(chunks: list[bytes]) -> int:
    metadata = len(";chunk-signature=") + 64 + len("\r\n")
    framed = sum(len(f"{len(chunk):x}") + metadata + len(chunk) + 2 for chunk in chunks)
    return framed + len("0") + metadata + 2


def _request(
    endpoint: str,
    bucket: str,
    object_key: str,
    payload: bytes,
    access_key: str,
    secret_key: str,
    region: str,
    tamper_first_chunk: bool,
) -> tuple[int, bytes]:
    parsed = urllib.parse.urlsplit(endpoint)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise ValueError("endpoint must be an absolute HTTP or HTTPS URL")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        raise ValueError("endpoint must not contain a path, query, or fragment")

    raw_path = f"/{bucket}/{object_key}"
    path = urllib.parse.quote(raw_path, safe="/-_.~")
    host = parsed.netloc
    now = datetime.datetime.now(datetime.UTC)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    chunks = [payload[offset : offset + 64 * 1024] for offset in range(0, len(payload), 64 * 1024)]

    headers = {
        "content-encoding": "aws-chunked",
        "content-length": str(_encoded_length(chunks)),
        "host": host,
        "x-amz-content-sha256": PAYLOAD_MODE,
        "x-amz-date": amz_date,
        "x-amz-decoded-content-length": str(len(payload)),
    }
    signed_headers = tuple(sorted(headers))
    canonical_request = _canonical_request("PUT", path, headers, signed_headers)
    seed_signature, signing_key, scope = _seed_signature(
        secret_key, amz_date, region, "s3", canonical_request
    )
    body = _encoded_body(
        chunks,
        signing_key,
        amz_date,
        scope,
        seed_signature,
        tamper_first_chunk,
    )
    if len(body) != int(headers["content-length"]):
        raise RuntimeError("encoded body length does not match signed Content-Length")

    headers["authorization"] = (
        f"{ALGORITHM} Credential={access_key}/{scope},"
        f"SignedHeaders={';'.join(signed_headers)},Signature={seed_signature}"
    )
    connection_type = (
        http.client.HTTPSConnection
        if parsed.scheme == "https"
        else http.client.HTTPConnection
    )
    connection = connection_type(parsed.hostname, parsed.port, timeout=30)
    try:
        connection.request("PUT", path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, response.read(16 * 1024)
    finally:
        connection.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--key", required=True)
    parser.add_argument("--input", required=True)
    parser.add_argument("--region", default=os.environ.get("AWS_DEFAULT_REGION", "us-east-1"))
    parser.add_argument("--tamper-first-chunk", action="store_true")
    parser.add_argument("--expect-rejection", action="store_true")
    args = parser.parse_args()

    access_key = os.environ.get("AWS_ACCESS_KEY_ID")
    secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY")
    if not access_key or not secret_key:
        parser.error("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are required")

    with open(args.input, "rb") as input_file:
        payload = input_file.read()
    status, response = _request(
        args.endpoint,
        args.bucket,
        args.key,
        payload,
        access_key,
        secret_key,
        args.region,
        args.tamper_first_chunk,
    )
    expected_status = 400 <= status < 500 if args.expect_rejection else 200 <= status < 300
    if not expected_status:
        print(
            f"unexpected S3 status {status}: {response.decode('utf-8', errors='replace')}",
            file=sys.stderr,
        )
        return 1
    print(f"streaming SigV4 request returned HTTP {status}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
