#!/usr/bin/env python3
"""Isolate signed S3 publication costs on a private loopback RustFS fixture."""

import argparse
import concurrent.futures
import datetime
import hashlib
import hmac
import http.client
import json
import math
from pathlib import Path
import threading
import time
from urllib.parse import urlsplit
import uuid


def request(connection, method, path, body=b"", extra=None, expected=(200, 204)):
    now = datetime.datetime.now(datetime.timezone.utc)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    day = stamp[:8]
    headers = {
        "host": f"{connection.host}:{connection.port}",
        "x-amz-content-sha256": hashlib.sha256(body).hexdigest(),
        "x-amz-date": stamp,
    } | (extra or {})
    names = ";".join(sorted(headers))
    canonical_headers = "".join(f"{name}:{headers[name].strip()}\n" for name in sorted(headers))
    canonical = "\n".join([
        method, path, "", canonical_headers, names, headers["x-amz-content-sha256"],
    ])
    scope = f"{day}/us-east-1/s3/aws4_request"
    signing_input = "\n".join([
        "AWS4-HMAC-SHA256", stamp, scope, hashlib.sha256(canonical.encode()).hexdigest(),
    ])
    key = b"AWS4crab"
    for part in (day, "us-east-1", "s3", "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, signing_input.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential=crab/{scope}, "
        f"SignedHeaders={names}, Signature={signature}"
    )
    connection.request(method, path, body, headers)
    response = connection.getresponse()
    content = response.read()
    if response.status not in expected:
        raise RuntimeError(f"{method} returned {response.status}: {content[:200]!r}")
    return content, response.getheader("etag")


def measure(endpoint, bucket, actors, mode):
    clients = [http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=60)
               for _ in range(actors)]
    prefix = f"/{bucket}/{actors}-{mode}"
    try:
        # Warm each private connection outside timing; independent heads keep
        # same-ref contention out of this storage-capacity control.
        tags = [request(client, "PUT", f"{prefix}/{actor}/head", b"base",
                        {"if-none-match": "*"})[1]
                for actor, client in enumerate(clients)]
        if not all(tags):
            raise RuntimeError("head creation returned no conditional-write identity")
        timings, waves = [], []
        cpu_started = time.process_time()
        for wave in range(10):
            payloads = [hashlib.shake_256(f"{actors}:{mode}:{wave}:{actor}".encode()).digest(8192)
                        for actor in range(actors)]
            barrier = threading.Barrier(actors)

            def publish(actor):
                client = clients[actor]
                body = payloads[actor]
                path = f"{prefix}/{actor}/{wave}"
                barrier.wait(timeout=60)
                started = time.perf_counter_ns()
                request(client, "PUT", path, body, {"if-none-match": "*"})
                if mode == "publication-chain":
                    actual, _ = request(client, "GET", path)
                    if actual != body:
                        raise RuntimeError("immutable readback differs from uploaded bytes")
                    _, tags[actor] = request(client, "PUT", f"{prefix}/{actor}/head",
                                             str(wave).encode(), {"if-match": tags[actor]})
                    if not tags[actor]:
                        raise RuntimeError("head update returned no conditional-write identity")
                return (time.perf_counter_ns() - started) / 1e6

            started = time.perf_counter()
            with concurrent.futures.ThreadPoolExecutor(max_workers=actors) as pool:
                timings.extend(pool.map(publish, range(actors)))
            waves.append(time.perf_counter() - started)
        cpu_seconds = time.process_time() - cpu_started
        if mode == "put":
            # PUT-only timing excludes reads, but dropped or altered bytes
            # must still fail the control before its report is accepted.
            for actor, client in enumerate(clients):
                for wave in range(10):
                    expected = hashlib.shake_256(f"{actors}:{mode}:{wave}:{actor}".encode()).digest(8192)
                    actual, _ = request(client, "GET", f"{prefix}/{actor}/{wave}")
                    if actual != expected:
                        raise RuntimeError("untimed readback differs from uploaded bytes")
        else:
            for actor, client in enumerate(clients):
                actual, _ = request(client, "GET", f"{prefix}/{actor}/head")
                if actual != b"9":
                    raise RuntimeError("final conditional head does not contain the last update")
        timings.sort()
        return {
            "actors": actors, "mode": mode, "payload_bytes": 8192, "samples": len(timings),
            "p50_ms": timings[math.ceil(len(timings) * .5) - 1],
            "p95_ms": timings[math.ceil(len(timings) * .95) - 1], "max_ms": max(timings),
            "wave_operations_per_second": len(timings) / sum(waves),
            "client_cpu_seconds": cpu_seconds, "all_uploaded_bytes_verified": True,
            "readback_inside_timing": mode == "publication-chain",
        }
    finally:
        for client in clients:
            client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint-url", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    endpoint = urlsplit(args.endpoint_url)
    if (endpoint.scheme != "http" or endpoint.hostname not in {"127.0.0.1", "localhost"}
            or endpoint.port is None or endpoint.path not in {"", "/"}
            or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment):
        parser.error("this control requires a private HTTP loopback endpoint with an explicit port")
    bucket = f"crab-storage-control-{uuid.uuid4().hex}"
    connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=60)
    try:
        request(connection, "PUT", f"/{bucket}")
        path = f"/{bucket}/conditional-check"
        _, old = request(connection, "PUT", path, b"old", {"if-none-match": "*"})
        if not old:
            raise RuntimeError("conditional-write check has no object identity")
        # A fast acknowledgement is insufficient if stale writers can win.
        request(connection, "PUT", path, b"new", {"if-match": old})
        request(connection, "PUT", path, b"rejected", {"if-match": old}, expected=(412,))
        request(connection, "PUT", path, b"rejected", {"if-none-match": "*"}, expected=(412,))
        actual, _ = request(connection, "GET", path)
        if actual != b"new":
            raise RuntimeError("a rejected conditional write changed the head")
    finally:
        connection.close()
    rows = []
    for actors in (1, 10, 50):
        for mode in ("put", "publication-chain"):
            row = measure(endpoint, bucket, actors, mode)
            rows.append(row)
            print(json.dumps(row), flush=True)
    args.output.write_text(json.dumps({
        "status": "ok", "conditional_writes_verified": True, "retries": 0,
        "description": "storage controls only; not a decomposition of the Crab push median",
        "cohorts": rows,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
