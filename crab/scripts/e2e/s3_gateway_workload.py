#!/usr/bin/env python3
"""Qualify sustained small-object writes with signed S3 requests."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import hmac
import http.client
import json
import math
import os
import secrets
import time
import urllib.parse
import xml.etree.ElementTree as ElementTree
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Mapping


ALGORITHM = "AWS4-HMAC-SHA256"
SERVICE = "s3"
MAX_RESPONSE_BYTES = 8 * 1024 * 1024


@dataclass(frozen=True)
class Endpoint:
    url: str
    bucket: str
    access_key: str
    secret_key: str
    region: str
    session_token: str | None = None


@dataclass
class RequestResult:
    status: int | None
    latency_ms: float
    error: str | None = None


@dataclass
class WindowStats:
    requests: int = 0
    acknowledged: int = 0
    latencies_ms: list[float] = field(default_factory=list)
    errors: Counter[str] = field(default_factory=Counter)

    @property
    def failed(self) -> int:
        return self.requests - self.acknowledged


@dataclass
class WorkloadStats:
    requests: int = 0
    acknowledged: int = 0
    latencies_ms: list[float] = field(default_factory=list)
    errors: Counter[str] = field(default_factory=Counter)
    acknowledged_keys: set[str] = field(default_factory=set)
    listed_keys: int = 0
    missing_acknowledgements: int = 0
    unexpected_keys: int = 0
    duplicate_list_entries: int = 0
    elapsed_ms: int = 0
    windows: dict[int, WindowStats] = field(default_factory=dict)

    @property
    def failed(self) -> int:
        return self.requests - self.acknowledged

    @property
    def throughput_per_second(self) -> float:
        if self.elapsed_ms <= 0:
            return 0.0
        return self.acknowledged * 1000.0 / self.elapsed_ms

    def percentile(self, fraction: float) -> float:
        if not self.latencies_ms:
            return 0.0
        ordered = sorted(self.latencies_ms)
        index = min(len(ordered) - 1, max(0, math.ceil(len(ordered) * fraction) - 1))
        return ordered[index]


def _quote(value: str) -> str:
    return urllib.parse.quote(value, safe="-_.~")


def _canonical_query(parameters: Iterable[tuple[str, str]]) -> str:
    encoded = sorted((_quote(key), _quote(value)) for key, value in parameters)
    return "&".join(f"{key}={value}" for key, value in encoded)


def _canonical_path(bucket: str, key: str) -> str:
    encoded_key = urllib.parse.quote(key, safe="/-_.~")
    return f"/{_quote(bucket)}/{encoded_key}"


def _normalize_header(value: str) -> str:
    return " ".join(value.strip().split())


def _signing_key(secret_key: str, date: str, region: str) -> bytes:
    def derive(key: bytes, value: str) -> bytes:
        return hmac.new(key, value.encode("utf-8"), hashlib.sha256).digest()

    date_key = derive(f"AWS4{secret_key}".encode("utf-8"), date)
    region_key = derive(date_key, region)
    service_key = derive(region_key, SERVICE)
    return derive(service_key, "aws4_request")


def _signed_headers(
    method: str,
    path: str,
    query: str,
    headers: Mapping[str, str],
    payload_hash: str,
    secret_key: str,
    access_key: str,
    region: str,
    amz_date: str,
) -> dict[str, str]:
    names = sorted(headers)
    canonical_headers = "".join(
        f"{name}:{_normalize_header(headers[name])}\n" for name in names
    )
    canonical_request = "\n".join(
        (method, path, query, canonical_headers, ";".join(names), payload_hash)
    )
    date = amz_date[:8]
    scope = f"{date}/{region}/{SERVICE}/aws4_request"
    string_to_sign = "\n".join(
        (
            ALGORITHM,
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
        )
    )
    signature = hmac.new(
        _signing_key(secret_key, date, region),
        string_to_sign.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    signed = dict(headers)
    signed["authorization"] = (
        f"{ALGORITHM} Credential={access_key}/{scope},"
        f"SignedHeaders={';'.join(names)},Signature={signature}"
    )
    return signed


def _parse_endpoint(endpoint: Endpoint) -> urllib.parse.SplitResult:
    parsed = urllib.parse.urlsplit(endpoint.url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise ValueError("endpoint must be an absolute HTTP or HTTPS URL")
    if parsed.username is not None or parsed.password is not None:
        raise ValueError("endpoint must not contain userinfo")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        raise ValueError("endpoint must not contain a path, query, or fragment")
    if not endpoint.bucket or "/" in endpoint.bucket:
        raise ValueError("bucket must be a bare name")
    return parsed


def _request(
    endpoint: Endpoint,
    method: str,
    key: str,
    query: Iterable[tuple[str, str]] = (),
    body: bytes = b"",
    timeout: float = 30.0,
) -> tuple[RequestResult, bytes]:
    parsed = _parse_endpoint(endpoint)
    query_text = _canonical_query(query)
    path = _canonical_path(endpoint.bucket, key)
    amz_date = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%SZ")
    headers = {
        "host": parsed.netloc,
        "x-amz-content-sha256": hashlib.sha256(body).hexdigest(),
        "x-amz-date": amz_date,
    }
    if body:
        headers["content-length"] = str(len(body))
    if endpoint.session_token:
        headers["x-amz-security-token"] = endpoint.session_token
    signed = _signed_headers(
        method,
        path,
        query_text,
        headers,
        headers["x-amz-content-sha256"],
        endpoint.secret_key,
        endpoint.access_key,
        endpoint.region,
        amz_date,
    )
    wire_headers = {name.title(): value for name, value in signed.items()}
    target = path + (f"?{query_text}" if query_text else "")
    connection_type = (
        http.client.HTTPSConnection
        if parsed.scheme == "https"
        else http.client.HTTPConnection
    )
    started = time.monotonic()
    try:
        connection = connection_type(parsed.hostname, parsed.port, timeout=timeout)
        try:
            connection.request(method, target, body=body or None, headers=wire_headers)
            response = connection.getresponse()
            response_body = response.read(MAX_RESPONSE_BYTES + 1)
            if len(response_body) > MAX_RESPONSE_BYTES:
                response_body = response_body[:MAX_RESPONSE_BYTES]
            result = RequestResult(
                status=response.status,
                latency_ms=(time.monotonic() - started) * 1000.0,
            )
            return result, response_body
        finally:
            connection.close()
    except (OSError, http.client.HTTPException, ValueError) as error:
        return (
            RequestResult(
                status=None,
                latency_ms=(time.monotonic() - started) * 1000.0,
                error=type(error).__name__,
            ),
            b"",
        )


def _payload(writer: int, sequence: int, size: int) -> bytes:
    seed = f"crab-s3-gateway-workload:{writer}:{sequence}".encode("ascii")
    block = hashlib.sha256(seed).digest()
    return (block * ((size + len(block) - 1) // len(block)))[:size]


def _put_worker(
    endpoint: Endpoint,
    prefix: str,
    writer: int,
    started: float,
    deadline: float,
    window_seconds: float,
    object_bytes: int,
    timeout: float,
) -> WorkloadStats:
    stats = WorkloadStats()
    sequence = 0
    while time.monotonic() < deadline:
        key = f"{prefix}/writer-{writer:02d}/object-{sequence:08d}"
        result, _ = _request(
            endpoint,
            "PUT",
            key,
            body=_payload(writer, sequence, object_bytes),
            timeout=timeout,
        )
        completed = time.monotonic()
        window = stats.windows.setdefault(
            int((completed - started) / window_seconds), WindowStats()
        )
        stats.requests += 1
        stats.latencies_ms.append(result.latency_ms)
        window.requests += 1
        window.latencies_ms.append(result.latency_ms)
        if result.status is not None and 200 <= result.status < 300:
            stats.acknowledged += 1
            stats.acknowledged_keys.add(key)
            window.acknowledged += 1
        elif result.status is None:
            stats.errors[result.error or "transport"] += 1
            window.errors[result.error or "transport"] += 1
        else:
            stats.errors[f"http_{result.status}"] += 1
            window.errors[f"http_{result.status}"] += 1
        sequence += 1
    return stats


def _merge_stats(target: WorkloadStats, source: WorkloadStats) -> None:
    target.requests += source.requests
    target.acknowledged += source.acknowledged
    target.latencies_ms.extend(source.latencies_ms)
    target.errors.update(source.errors)
    target.acknowledged_keys.update(source.acknowledged_keys)
    for index, source_window in source.windows.items():
        target_window = target.windows.setdefault(index, WindowStats())
        target_window.requests += source_window.requests
        target_window.acknowledged += source_window.acknowledged
        target_window.latencies_ms.extend(source_window.latencies_ms)
        target_window.errors.update(source_window.errors)


def _element_text(root: ElementTree.Element, name: str) -> str | None:
    for element in root.iter():
        if element.tag.rsplit("}", 1)[-1] == name:
            return element.text
    return None


def _list_keys(endpoint: Endpoint, prefix: str, timeout: float) -> tuple[set[str], int, int]:
    keys: set[str] = set()
    entries = 0
    duplicates = 0
    token: str | None = None
    for _ in range(10_000):
        query = [("list-type", "2"), ("max-keys", "1000"), ("prefix", prefix)]
        if token is not None:
            query.append(("continuation-token", token))
        result, body = _request(endpoint, "GET", "", query=query, timeout=timeout)
        if result.status != 200:
            raise RuntimeError(f"list returned HTTP {result.status}")
        try:
            root = ElementTree.fromstring(body)
        except ElementTree.ParseError as error:
            raise RuntimeError("list returned invalid XML") from error
        for element in root.iter():
            if element.tag.rsplit("}", 1)[-1] != "Key":
                continue
            key = element.text or ""
            entries += 1
            if key in keys:
                duplicates += 1
            keys.add(key)
        if _element_text(root, "IsTruncated") != "true":
            return keys, entries, duplicates
        next_token = _element_text(root, "NextContinuationToken")
        if not next_token or next_token == token:
            raise RuntimeError("truncated list omitted a new continuation token")
        token = next_token
    raise RuntimeError("list exceeded the 10,000-page safety bound")


def _run_endpoint(
    endpoint: Endpoint,
    prefix: str,
    writers: int,
    duration_seconds: float,
    object_bytes: int,
    timeout: float,
    window_seconds: float = 60.0,
) -> WorkloadStats:
    started = time.monotonic()
    deadline = started + duration_seconds
    stats = WorkloadStats()
    with ThreadPoolExecutor(max_workers=writers, thread_name_prefix="s3-load") as pool:
        futures = [
            pool.submit(
                _put_worker,
                endpoint,
                prefix,
                writer,
                started,
                deadline,
                window_seconds,
                object_bytes,
                timeout,
            )
            for writer in range(writers)
        ]
        for future in futures:
            _merge_stats(stats, future.result())
    stats.elapsed_ms = max(1, int((time.monotonic() - started) * 1000))
    try:
        keys, listed, duplicates = _list_keys(endpoint, prefix, timeout)
    except RuntimeError:
        stats.errors["listing"] += 1
        stats.missing_acknowledgements = len(stats.acknowledged_keys)
    else:
        stats.listed_keys = listed
        stats.duplicate_list_entries = duplicates
        stats.missing_acknowledgements = len(stats.acknowledged_keys - keys)
        stats.unexpected_keys = len(keys - stats.acknowledged_keys)
    return stats


def _stats_report(
    stats: WorkloadStats,
    duration_seconds: float,
    window_seconds: float,
) -> dict[str, object]:
    complete_windows = int(duration_seconds // window_seconds)
    windows = []
    for index in range(complete_windows):
        window = stats.windows.get(index, WindowStats())
        windows.append(
            {
                "index": index,
                "requests": window.requests,
                "acknowledged": window.acknowledged,
                "failed": window.failed,
                "errors": dict(sorted(window.errors.items())),
                "p50_latency_ms": round(_percentile(window.latencies_ms, 0.50), 3),
                "p95_latency_ms": round(_percentile(window.latencies_ms, 0.95), 3),
                "throughput_per_second": round(
                    window.acknowledged / window_seconds, 6
                ),
            }
        )
    return {
        "requests": stats.requests,
        "acknowledged": stats.acknowledged,
        "failed": stats.failed,
        "errors": dict(sorted(stats.errors.items())),
        "p50_latency_ms": round(stats.percentile(0.50), 3),
        "p95_latency_ms": round(stats.percentile(0.95), 3),
        "throughput_per_second": round(stats.throughput_per_second, 6),
        "elapsed_ms": stats.elapsed_ms,
        "listed_keys": stats.listed_keys,
        "missing_acknowledgements": stats.missing_acknowledgements,
        "unexpected_keys": stats.unexpected_keys,
        "duplicate_list_entries": stats.duplicate_list_entries,
        "complete_windows": windows,
    }


def _percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, math.ceil(len(ordered) * fraction) - 1))
    return ordered[index]


def _degradation(
    stats: WorkloadStats,
    duration_seconds: float,
    window_seconds: float,
    min_terminal_throughput_ratio: float,
    max_terminal_p95_ratio: float,
) -> dict[str, object]:
    complete_windows = [
        stats.windows.get(index, WindowStats())
        for index in range(int(duration_seconds // window_seconds))
    ]
    if len(complete_windows) < 4:
        return {
            "measured": False,
            "passed": False,
            "complete_windows": len(complete_windows),
            "required_complete_windows": 4,
        }
    cohort_size = max(1, len(complete_windows) // 3)
    early = complete_windows[:cohort_size]
    terminal = complete_windows[-cohort_size:]

    def throughput(windows: list[WindowStats]) -> float:
        return sum(window.acknowledged for window in windows) / (
            len(windows) * window_seconds
        )

    def p95(windows: list[WindowStats]) -> float:
        return _percentile(
            [latency for window in windows for latency in window.latencies_ms], 0.95
        )

    early_throughput = throughput(early)
    terminal_throughput = throughput(terminal)
    early_p95 = p95(early)
    terminal_p95 = p95(terminal)
    throughput_ratio = (
        terminal_throughput / early_throughput if early_throughput > 0 else 0.0
    )
    p95_ratio = terminal_p95 / early_p95 if early_p95 > 0 else 0.0
    windows_are_clean = all(
        window.acknowledged > 0 and window.failed == 0 for window in complete_windows
    )
    passed = (
        windows_are_clean
        and throughput_ratio >= min_terminal_throughput_ratio
        and p95_ratio <= max_terminal_p95_ratio
    )
    return {
        "measured": True,
        "passed": passed,
        "complete_windows": len(complete_windows),
        "cohort_windows": cohort_size,
        "early_throughput_per_second": round(early_throughput, 6),
        "terminal_throughput_per_second": round(terminal_throughput, 6),
        "terminal_throughput_ratio": round(throughput_ratio, 6),
        "early_p95_latency_ms": round(early_p95, 3),
        "terminal_p95_latency_ms": round(terminal_p95, 3),
        "terminal_p95_latency_ratio": round(p95_ratio, 6),
        "min_terminal_throughput_ratio": min_terminal_throughput_ratio,
        "max_terminal_p95_latency_ratio": max_terminal_p95_ratio,
        "windows_are_clean": windows_are_clean,
    }


def build_report(
    *,
    prefix: str,
    writers: int,
    duration_seconds: float,
    object_bytes: int,
    timeout: float,
    gateway: WorkloadStats,
    baseline: WorkloadStats | None,
    min_throughput_ratio: float,
    max_p95_ratio: float,
    window_seconds: float = 60.0,
    min_terminal_throughput_ratio: float = 0.80,
    max_terminal_p95_ratio: float = 1.50,
    source: str = "sustained-small-write",
) -> dict[str, object]:
    baseline_throughput = baseline.throughput_per_second if baseline else None
    throughput_ratio = (
        gateway.throughput_per_second / baseline_throughput
        if baseline_throughput is not None and baseline_throughput > 0
        else None
    )
    baseline_p95 = baseline.percentile(0.95) if baseline else None
    p95_ratio = (
        gateway.percentile(0.95) / baseline_p95
        if baseline_p95 is not None and baseline_p95 > 0
        else None
    )
    measured_stats = [gateway] + ([baseline] if baseline else [])
    integrity_passed = all(
        stats.acknowledged > 0
        and stats.failed == 0
        and stats.missing_acknowledgements == 0
        and stats.unexpected_keys == 0
        and stats.duplicate_list_entries == 0
        for stats in measured_stats
    )
    gateway_degradation = _degradation(
        gateway,
        duration_seconds,
        window_seconds,
        min_terminal_throughput_ratio,
        max_terminal_p95_ratio,
    )
    baseline_degradation = (
        _degradation(
            baseline,
            duration_seconds,
            window_seconds,
            min_terminal_throughput_ratio,
            max_terminal_p95_ratio,
        )
        if baseline
        else None
    )
    baseline_comparison_passed = baseline is None or (
        throughput_ratio is not None
        and p95_ratio is not None
        and throughput_ratio >= min_throughput_ratio
        and p95_ratio <= max_p95_ratio
    )
    performance_passed = bool(
        gateway_degradation["passed"]
        and baseline_comparison_passed
        and (baseline_degradation is None or baseline_degradation["passed"])
    )
    return {
        "schema": "crab.s3-gateway-workload",
        "schema_version": 2,
        "source": source,
        "status": "passed" if integrity_passed and performance_passed else "failed",
        "workload": {
            "prefix_sha256": hashlib.sha256(prefix.encode("utf-8")).hexdigest(),
            "writers": writers,
            "duration_seconds": duration_seconds,
            "object_bytes": object_bytes,
            "request_timeout_seconds": timeout,
            "window_seconds": window_seconds,
        },
        "gateway": _stats_report(gateway, duration_seconds, window_seconds),
        "direct_baseline": (
            _stats_report(baseline, duration_seconds, window_seconds)
            if baseline
            else None
        ),
        "comparison": {
            "throughput_ratio": (
                round(throughput_ratio, 6) if throughput_ratio is not None else None
            ),
            "p95_latency_ratio": round(p95_ratio, 6) if p95_ratio is not None else None,
            "min_throughput_ratio": min_throughput_ratio,
            "max_p95_latency_ratio": max_p95_ratio,
            "integrity_passed": integrity_passed,
            "performance_passed": performance_passed,
            "baseline_comparison_measured": baseline is not None,
            "baseline_comparison_passed": baseline_comparison_passed,
            "gateway_degradation": gateway_degradation,
            "baseline_degradation": baseline_degradation,
        },
    }


def _credentials(name: str) -> tuple[str, str, str | None]:
    access = os.environ.get(f"{name}_ACCESS_KEY")
    secret = os.environ.get(f"{name}_SECRET_KEY")
    token = os.environ.get(f"{name}_SESSION_TOKEN")
    if not access or not secret:
        raise ValueError(f"{name}_ACCESS_KEY and {name}_SECRET_KEY are required")
    return access, secret, token


def _write_report(path: Path, report: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway-endpoint", required=True)
    parser.add_argument("--gateway-bucket", required=True)
    parser.add_argument("--baseline-endpoint")
    parser.add_argument("--baseline-bucket")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--prefix", default="main/qualification/small-write")
    parser.add_argument("--writers", type=int, default=16)
    parser.add_argument("--duration-seconds", type=float, default=300.0)
    parser.add_argument("--object-bytes", type=int, default=4096)
    parser.add_argument("--request-timeout-seconds", type=float, default=30.0)
    parser.add_argument("--min-throughput-ratio", type=float, default=0.90)
    parser.add_argument("--max-p95-ratio", type=float, default=1.25)
    parser.add_argument("--window-seconds", type=float, default=60.0)
    parser.add_argument("--min-terminal-throughput-ratio", type=float, default=0.80)
    parser.add_argument("--max-terminal-p95-ratio", type=float, default=1.50)
    parser.add_argument("--region", default="us-east-1")
    return parser


def main() -> int:
    argument_parser = parser()
    args = argument_parser.parse_args()
    if args.writers < 1 or args.writers > 256:
        argument_parser.error("--writers must be between 1 and 256")
    if args.duration_seconds <= 0 or args.duration_seconds > 3600:
        argument_parser.error("--duration-seconds must be greater than 0 and at most 3600")
    if args.object_bytes < 1 or args.object_bytes > 5 * 1024 * 1024:
        argument_parser.error("--object-bytes must be between 1 and 5 MiB")
    if args.request_timeout_seconds <= 0 or args.request_timeout_seconds > 300:
        argument_parser.error("--request-timeout-seconds must be between 0 and 300")
    if not math.isfinite(args.min_throughput_ratio) or not 0 < args.min_throughput_ratio <= 1:
        argument_parser.error("--min-throughput-ratio must be greater than 0 and at most 1")
    if not math.isfinite(args.max_p95_ratio) or args.max_p95_ratio <= 0:
        argument_parser.error("--max-p95-ratio must be greater than 0")
    if (
        not math.isfinite(args.window_seconds)
        or args.window_seconds <= 0
        or args.duration_seconds // args.window_seconds < 4
    ):
        argument_parser.error(
            "--window-seconds must produce at least four complete workload windows"
        )
    if (
        not math.isfinite(args.min_terminal_throughput_ratio)
        or not 0 < args.min_terminal_throughput_ratio <= 1
    ):
        argument_parser.error(
            "--min-terminal-throughput-ratio must be greater than 0 and at most 1"
        )
    if (
        not math.isfinite(args.max_terminal_p95_ratio)
        or args.max_terminal_p95_ratio <= 0
    ):
        argument_parser.error("--max-terminal-p95-ratio must be greater than 0")
    if bool(args.baseline_endpoint) != bool(args.baseline_bucket):
        argument_parser.error(
            "--baseline-endpoint and --baseline-bucket must be configured together"
        )
    prefix = args.prefix.strip("/")
    if not prefix or "?" in prefix or "#" in prefix:
        argument_parser.error("--prefix must be a non-empty S3 prefix without query or fragment")

    try:
        gateway_access, gateway_secret, gateway_token = _credentials("S3_GATEWAY_WORKLOAD")
        gateway = Endpoint(
            args.gateway_endpoint,
            args.gateway_bucket,
            gateway_access,
            gateway_secret,
            args.region,
            gateway_token,
        )
        baseline = None
        if args.baseline_endpoint and args.baseline_bucket:
            baseline_access, baseline_secret, baseline_token = _credentials(
                "S3_BASELINE_WORKLOAD"
            )
            baseline = Endpoint(
                args.baseline_endpoint,
                args.baseline_bucket,
                baseline_access,
                baseline_secret,
                args.region,
                baseline_token,
            )
        run_prefix = f"{prefix}/{secrets.token_hex(8)}"
        baseline_stats = None
        if baseline:
            baseline_stats = _run_endpoint(
                baseline,
                run_prefix,
                args.writers,
                args.duration_seconds,
                args.object_bytes,
                args.request_timeout_seconds,
                args.window_seconds,
            )
        gateway_stats = _run_endpoint(
            gateway,
            run_prefix,
            args.writers,
            args.duration_seconds,
            args.object_bytes,
            args.request_timeout_seconds,
            args.window_seconds,
        )
        report = build_report(
            prefix=run_prefix,
            writers=args.writers,
            duration_seconds=args.duration_seconds,
            object_bytes=args.object_bytes,
            timeout=args.request_timeout_seconds,
            gateway=gateway_stats,
            baseline=baseline_stats,
            min_throughput_ratio=args.min_throughput_ratio,
            max_p95_ratio=args.max_p95_ratio,
            window_seconds=args.window_seconds,
            min_terminal_throughput_ratio=args.min_terminal_throughput_ratio,
            max_terminal_p95_ratio=args.max_terminal_p95_ratio,
        )
        _write_report(args.report, report)
        print(json.dumps(report, sort_keys=True))
        return 0 if report["status"] == "passed" else 1
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: workload qualification failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
