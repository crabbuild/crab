#!/usr/bin/env python3
"""Run cache-service traffic smokes against a local RustFS/S3 endpoint.

The harness starts a real ``crab-cache-server`` with a RustFS/S3 origin, writes
immutable Crab objects to the origin, verifies direct cache-service full/range
reads, then pushes and hydrates a real Crab repository through the cache
service. A local forwarding proxy sits between the cache server and RustFS so
the report can prove how many origin GET and PUT requests actually reached
object storage.

Use a new dedicated disposable bucket and a fresh run ID. Synthetic origin objects
are create-only and confined to run prefixes; global metadata route probes use
real objects observed during this run's push. Existing reports are never reused.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.client
import json
import os
import shutil
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = os.environ.get("AWS_ENDPOINT_URL", "http://127.0.0.1:9000")
DEFAULT_ROOT = Path(os.environ.get("TMPDIR", "/tmp")) / "crab-cache-service-smoke"
DEFAULT_PSK = "cache-smoke-psk"
DEFAULT_PSK_BLAKE3 = "4fb898757c4c93662343bbbb25419f8c4f9c979352d40ff896578cabf620cf6e"
EVIDENCE_MANIFEST_SCHEMA = "crab-cache-service.evidence-manifest.v1"
REMOTE_PREFIX = "e2e-cache-service"
SECRET_KEYS = {
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "CRAB_CACHE_PSK",
}
HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
EXPECTED_ROUTE_SCHEMA = "crab-cache-service.routes.v1"
EXPECTED_IMMUTABLE_ROUTE_PATTERNS = [
    ".crab/xorbs/{first-two-hex}/{hash}",
    ".crab/shards/{first-two-hex}/{hash}",
    "{repo}/packs/pack-{id}.pack",
    "{repo}/packs/pack-{id}.idx",
    "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
    "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
    "{repo}/file_index_db/compacted/*.sst",
    "{repo}/file_index_db/manifest/*.manifest",
    "{repo}/file_index_db/wal/*.sst",
    "{repo}/file_index_db/compactions/*.compactions",
    ".crab/chunk_index_db/compacted/*.sst",
    ".crab/chunk_index_db/manifest/*.manifest",
    ".crab/chunk_index_db/wal/*.sst",
    ".crab/chunk_index_db/compactions/*.compactions",
]
EXPECTED_MUTABLE_ROUTE_PATTERNS = [
    "{repo}/refs/heads/*",
    "{repo}/HEAD",
    "{repo}/locks/*",
    "{repo}/packs/pack-{id}.meta",
    "{repo}/manifests/*",
    "{repo}/pack-list",
    "{repo}/shard-list",
    ".crab/ref-registry/*",
    "{repo}/file_index_db/manifest/current",
    ".crab/chunk_index_db/manifest/current",
]


class SmokeError(RuntimeError):
    """Raised when a smoke step fails."""


@dataclass
class CommandRecord:
    name: str
    args: list[str]
    cwd: str
    exit_code: int | None
    duration_ms: int
    stdout_log: str
    stderr_log: str
    timed_out: bool = False


@dataclass
class CacheReadRecord:
    name: str
    key: str
    status: int
    cache_status: str
    origin_gets_for_key: int
    body_len: int


@dataclass
class ImmutableRouteBehaviorRecord:
    name: str
    pattern: str
    key: str
    first_status: int
    first_cache_status: str
    second_status: int
    second_cache_status: str
    range_status: int
    range_cache_status: str
    origin_gets_before: int
    origin_gets_after_first: int
    origin_gets_after_second: int
    origin_gets_after_range: int
    body_len: int
    range_body_len: int


@dataclass
class ImmutableRouteWriteBehaviorRecord:
    name: str
    pattern: str
    key: str
    put_status: int
    put_cache_status: str
    get_status: int
    get_cache_status: str
    head_status: int
    head_cache_status: str
    range_status: int
    range_cache_status: str
    evict_status: int
    origin_gets_before: int
    origin_gets_after_put: int
    origin_gets_after_get: int
    origin_gets_after_head: int
    origin_gets_after_range: int
    origin_puts_before: int
    origin_puts_after: int
    total_origin_gets_before: int
    total_origin_gets_after: int
    total_origin_puts_before: int
    total_origin_puts_after: int
    total_bytes_before: int
    total_bytes_after: int
    push_warming_writes_before: int
    push_warming_writes_after: int
    push_warming_bytes_before: int
    push_warming_bytes_after: int
    body_len: int
    get_body_len: int
    range_body_len: int


@dataclass
class ImmutablePoisoningControlRecord:
    name: str
    pattern: str
    key: str
    corrupt_status: int
    corrupt_cache_status: str
    recovery_status: int
    recovery_cache_status: str
    second_status: int
    second_cache_status: str
    evict_status: int
    origin_gets_before: int
    origin_gets_after_reject: int
    origin_gets_after_recovery: int
    origin_gets_after_second: int
    origin_puts_before: int
    origin_puts_after: int
    total_origin_gets_before: int
    total_origin_gets_after_reject: int
    total_origin_gets_after_recovery: int
    total_origin_gets_after_second: int
    total_origin_puts_before: int
    total_origin_puts_after: int
    total_bytes_before: int
    total_bytes_after_reject: int
    total_bytes_after_recovery: int
    push_warming_writes_before: int
    push_warming_writes_after_reject: int
    push_warming_writes_after_recovery: int
    push_warming_bytes_before: int
    push_warming_bytes_after_reject: int
    push_warming_bytes_after_recovery: int
    valid_body_len: int
    corrupt_body_len: int
    corrupt_response_body_len: int
    recovery_body_len: int
    second_body_len: int


@dataclass
class MutableRouteBehaviorRecord:
    name: str
    pattern: str
    key: str
    status: int
    cache_status: str
    origin_gets_before: int
    origin_gets_after: int
    body_len: int


@dataclass
class MutableRouteWriteBehaviorRecord:
    name: str
    pattern: str
    key: str
    status: int
    cache_status: str
    origin_gets_before: int
    origin_gets_after: int
    origin_puts_before: int
    origin_puts_after: int
    total_origin_gets_before: int
    total_origin_gets_after: int
    total_origin_puts_before: int
    total_origin_puts_after: int
    total_bytes_before: int
    total_bytes_after: int
    push_warming_writes_before: int
    push_warming_writes_after: int
    push_warming_bytes_before: int
    push_warming_bytes_after: int
    request_body_len: int
    response_body_len: int


@dataclass
class AuthControlRecord:
    name: str
    key: str
    status: int
    cache_status: str
    origin_gets_before: int
    origin_gets_after: int
    body_len: int


@dataclass
class TransparentMutableAuthRecord:
    name: str
    key: str
    method: str
    status: int
    origin_gets_before: int
    origin_gets_after: int
    origin_heads_before: int
    origin_heads_after: int
    mutable_proxy_reads_before: int
    mutable_proxy_reads_after: int
    body_len: int


@dataclass
class RequestLimitRecord:
    name: str
    key: str
    status: int
    max_object_bytes: int
    declared_content_length: int
    body_bytes_sent: int
    origin_gets_before: int
    origin_gets_after: int
    origin_puts_before: int
    origin_puts_after: int
    total_origin_gets_before: int
    total_origin_gets_after: int
    total_origin_puts_before: int
    total_origin_puts_after: int
    total_bytes_before: int
    total_bytes_after: int
    xorb_count_before: int
    xorb_count_after: int
    push_warming_writes_before: int
    push_warming_writes_after: int
    push_warming_bytes_before: int
    push_warming_bytes_after: int


@dataclass
class CapabilitiesRecord:
    name: str
    status: int
    schema: str
    route_schema: str
    route_transport_prefix: str
    immutable_route_patterns: list[str]
    mutable_route_patterns: list[str]
    max_cache_bytes: int
    max_object_bytes: int
    admin_max_cache_bytes: int
    admin_max_object_bytes: int


@dataclass
class CliHydrateRecord:
    name: str
    origin_gets_before: int
    origin_gets_after: int
    origin_get_key_delta: dict[str, int]
    cache_hits_delta: int
    cache_misses_delta: int
    origin_fetches_delta: int
    origin_avoided_reads_delta: int
    mutable_read_rejections_delta: int
    mutable_write_rejections_delta: int
    hydrated_sha256: str


@dataclass
class RestartPersistenceRecord:
    name: str
    direct_key: str
    old_cache_service_url: str
    new_cache_service_url: str
    cache_root: str
    direct_status: int
    direct_cache_status: str
    range_status: int
    range_cache_status: str
    direct_origin_gets_before: int
    direct_origin_gets_after_direct: int
    direct_origin_gets_after_range: int
    total_origin_gets_before_direct: int
    total_origin_gets_after_direct: int
    total_origin_gets_after_range: int
    direct_body_len: int
    range_body_len: int
    cli_origin_gets_before: int
    cli_origin_gets_after: int
    cli_origin_get_key_delta: dict[str, int]
    cli_cache_hits_delta: int
    cli_origin_fetches_delta: int
    cli_origin_avoided_reads_delta: int
    cli_mutable_read_rejections_delta: int
    cli_mutable_write_rejections_delta: int
    cli_hydrated_sha256: str


@dataclass
class CacheIntegrityRepairRecord:
    name: str
    pattern: str
    key: str
    object_type: str
    cache_file: str
    old_cache_service_url: str
    new_cache_service_url: str
    corrupt_body_len: int
    valid_body_len: int
    repair_status: int
    repair_cache_status: str
    second_status: int
    second_cache_status: str
    origin_gets_before_repair: int
    origin_gets_after_repair: int
    origin_gets_after_second: int
    total_origin_gets_before_repair: int
    total_origin_gets_after_repair: int
    total_origin_gets_after_second: int
    total_bytes_before_repair: int
    total_bytes_after_repair: int
    total_bytes_after_second: int
    runtime_invalid_objects_evicted_before: int
    runtime_invalid_objects_evicted_after_repair: int
    runtime_invalid_objects_evicted_after_second: int
    runtime_missing_files_repaired_before: int
    runtime_missing_files_repaired_after_second: int
    runtime_metadata_entries_recreated_before: int
    runtime_metadata_entries_recreated_after_second: int
    startup_integrity_repairs_after_restart: int
    repair_body_len: int
    second_body_len: int


@dataclass
class OriginOutageRecord:
    name: str
    hot_key: str
    cold_key: str
    health_status: int
    live_status: int
    warm_status: int
    warm_cache_status: str
    hot_status: int
    hot_cache_status: str
    range_status: int
    range_cache_status: str
    cold_status: int
    cold_cache_status: str
    hot_origin_gets_before_outage: int
    hot_origin_gets_after_hot: int
    hot_origin_gets_after_range: int
    cold_origin_gets_before_outage: int
    cold_origin_gets_after_cold: int
    total_origin_gets_before_outage: int
    total_origin_gets_after_hot: int
    total_origin_gets_after_range: int
    total_origin_gets_after_cold: int
    cache_hits_before_outage: int
    cache_hits_after_outage: int
    origin_fetches_before_outage: int
    origin_fetches_after_outage: int
    hot_body_len: int
    range_body_len: int
    cold_body_len: int


@dataclass
class CliPushDedupRecord:
    name: str
    dedup_queries_delta: int
    dedup_known_chunks_delta: int
    dedup_unknown_chunks_delta: int
    xorb_gets_delta: int
    shard_gets_delta: int
    metadata_gets_delta: int
    xorb_puts_delta: int
    total_puts_delta: int
    origin_gets_delta: int
    origin_get_key_delta: dict[str, int]
    cacheable_origin_gets_delta: int
    cacheable_origin_get_key_delta: dict[str, int]
    mutable_origin_gets_delta: int
    mutable_origin_get_key_delta: dict[str, int]
    mutable_read_rejections_delta: int
    mutable_write_rejections_delta: int


@dataclass
class CachePressureRecord:
    name: str
    object_bytes: int
    pressure_objects: int
    total_bytes_before: int
    total_bytes_after: int
    max_bytes: int
    hot_origin_gets_before: int
    hot_origin_gets_after: int
    expected_bytes_without_eviction: int
    evictions_before: int
    evictions_after: int


@dataclass
class SupportBundleRecord:
    name: str
    path: str
    schema: str
    health_ok: bool | None
    health_status: int | None
    auth_ok: bool | None
    auth_status: int | None
    auth_endpoint: str | None
    capabilities_ok: bool | None
    capabilities_status: int | None
    authz_ok: bool | None
    authz_status: int | None
    admin_stats_ok: bool | None
    admin_stats_status: int | None
    metrics_ok: bool | None
    metrics_status: int | None
    cache_hit_rate: float | None
    origin_fallback_rate: float | None
    integrity_repairs: int | None
    push_warming_writes: int | None
    evicted_objects: int | None
    capabilities_schema: str
    capabilities_max_cache_bytes: int | None
    capabilities_max_object_bytes: int | None
    authz_schema: str
    authz_read: bool | None
    authz_write: bool | None
    authz_dedup: bool | None
    authz_admin: bool | None
    max_object_bytes: int | None
    cache_hit_total: float | None
    origin_avoided_reads_total: float | None
    origin_fetch_total: float | None
    cache_eviction_total: float | None
    cache_max_bytes: float | None
    cache_max_object_bytes: float | None


@dataclass
class EnterpriseOnboardingRecord:
    name: str
    bundle: str
    check_status: str
    probe_status: str
    server_config: str
    policy: str
    client_config: str
    client_env: str


@dataclass
class SmokeReport:
    run_id: str
    status: str
    root: str
    endpoint_url: str
    bucket: str
    cache_service_url: str = ""
    origin_proxy_url: str = ""
    env: dict[str, str] = field(default_factory=dict)
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    reads: list[dict[str, Any]] = field(default_factory=list)
    immutable_route_behaviors: list[dict[str, Any]] = field(default_factory=list)
    immutable_route_write_behaviors: list[dict[str, Any]] = field(default_factory=list)
    immutable_poisoning_controls: list[dict[str, Any]] = field(default_factory=list)
    mutable_route_behaviors: list[dict[str, Any]] = field(default_factory=list)
    mutable_route_write_behaviors: list[dict[str, Any]] = field(default_factory=list)
    auth_controls: list[dict[str, Any]] = field(default_factory=list)
    transparent_mutable_controls: list[dict[str, Any]] = field(default_factory=list)
    request_limits: list[dict[str, Any]] = field(default_factory=list)
    capabilities: list[dict[str, Any]] = field(default_factory=list)
    cli_hydrates: list[dict[str, Any]] = field(default_factory=list)
    restart_persistence: list[dict[str, Any]] = field(default_factory=list)
    cache_integrity_repairs: list[dict[str, Any]] = field(default_factory=list)
    origin_outages: list[dict[str, Any]] = field(default_factory=list)
    cli_push_dedup: list[dict[str, Any]] = field(default_factory=list)
    cache_pressure: list[dict[str, Any]] = field(default_factory=list)
    support_bundles: list[dict[str, Any]] = field(default_factory=list)
    enterprise_onboarding: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    updated_at: str = ""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "cache-service-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    out = "".join(c if c.isalnum() or c in "._-" else "-" for c in value.lower())
    return out.strip("-") or "command"


def redact_env(env: dict[str, str]) -> dict[str, str]:
    redacted: dict[str, str] = {}
    for key, value in sorted(env.items()):
        if key in SECRET_KEYS:
            redacted[key] = "<redacted>"
        elif key.startswith("AWS_") or key.startswith("CRAB_") or key.startswith("GIT_"):
            redacted[key] = value
    return redacted


def deterministic_bytes(size: int, seed: str) -> bytes:
    data = bytearray()
    counter = 0
    while len(data) < size:
        data.extend(hashlib.sha256(f"{seed}:{counter}".encode("utf-8")).digest())
        counter += 1
    return bytes(data[:size])


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_reference(path: Path, report_dir: Path) -> str:
    path = path.resolve()
    report_dir = report_dir.resolve()
    return os.path.relpath(path, report_dir)


def file_evidence(path: Path, report_dir: Path) -> dict[str, Any]:
    path = path.resolve()
    return {
        "path": artifact_reference(path, report_dir),
        "sha256": sha256_file(path),
        "bytes": path.stat().st_size,
    }


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def resolve_executable(value: str) -> str | None:
    if os.sep in value:
        path = Path(value)
        if path.is_file() and os.access(path, os.X_OK):
            return str(path.resolve())
        return None
    return shutil.which(value)


class OriginProxyState:
    def __init__(self, endpoint_url: str, bucket: str) -> None:
        endpoint = urllib.parse.urlparse(endpoint_url)
        if endpoint.scheme not in ("http", "https"):
            raise SmokeError(f"unsupported endpoint scheme: {endpoint.scheme}")
        self.endpoint = endpoint
        self.bucket = bucket
        self.condition = threading.Condition()
        self.get_counts: dict[str, int] = {}
        self.head_counts: dict[str, int] = {}
        self.put_counts: dict[str, int] = {}
        self.total_gets = 0
        self.total_puts = 0
        self.delay_once_by_key: dict[str, float] = {}
        self.connections: set[socket.socket] = set()

    def key_from_path(self, path: str) -> str | None:
        prefix = f"/{self.bucket}/"
        if not path.startswith(prefix):
            return None
        return urllib.parse.unquote(path[len(prefix) :])

    def record(self, method: str, raw_path: str) -> None:
        parsed = urllib.parse.urlparse(raw_path)
        key = self.key_from_path(parsed.path)
        delay = 0.0
        with self.condition:
            if method == "GET" and key:
                self.get_counts[key] = self.get_counts.get(key, 0) + 1
                self.total_gets += 1
                delay = self.delay_once_by_key.pop(key, 0.0)
                self.condition.notify_all()
            elif method == "HEAD" and key:
                self.head_counts[key] = self.head_counts.get(key, 0) + 1
                self.condition.notify_all()
            elif method == "PUT" and key:
                self.put_counts[key] = self.put_counts.get(key, 0) + 1
                self.total_puts += 1
                self.condition.notify_all()
        if delay > 0:
            time.sleep(delay)

    def count_for_key(self, key: str) -> int:
        with self.condition:
            return self.get_counts.get(key, 0)

    def count_head_for_key(self, key: str) -> int:
        with self.condition:
            return self.head_counts.get(key, 0)

    def count_put_for_key(self, key: str) -> int:
        with self.condition:
            return self.put_counts.get(key, 0)

    def counts_snapshot(self) -> dict[str, int]:
        with self.condition:
            return dict(self.get_counts)

    def put_counts_snapshot(self) -> dict[str, int]:
        with self.condition:
            return dict(self.put_counts)

    def total_get_count(self) -> int:
        with self.condition:
            return self.total_gets

    def total_put_count(self) -> int:
        with self.condition:
            return self.total_puts

    def wait_for_count(self, key: str, count: int, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self.condition:
            while self.get_counts.get(key, 0) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self.condition.wait(remaining)
            return True

    def delay_next_get(self, key: str, seconds: float) -> None:
        with self.condition:
            self.delay_once_by_key[key] = seconds

    def register_connection(self, connection: socket.socket) -> None:
        with self.condition:
            self.connections.add(connection)

    def unregister_connection(self, connection: socket.socket) -> None:
        with self.condition:
            self.connections.discard(connection)

    def close_connections(self) -> None:
        with self.condition:
            connections = list(self.connections)
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass


class CountingProxyHandler(BaseHTTPRequestHandler):
    state: OriginProxyState
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.state.register_connection(self.connection)

    def finish(self) -> None:
        try:
            super().finish()
        finally:
            self.state.unregister_connection(self.connection)

    def do_DELETE(self) -> None:
        self.proxy_request()

    def do_GET(self) -> None:
        self.proxy_request()

    def do_HEAD(self) -> None:
        self.proxy_request()

    def do_POST(self) -> None:
        self.proxy_request()

    def do_PUT(self) -> None:
        self.proxy_request()

    def log_message(self, fmt: str, *args: Any) -> None:
        return

    def proxy_request(self) -> None:
        self.state.record(self.command, self.path)
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else None
        endpoint = self.state.endpoint
        host = endpoint.hostname or "127.0.0.1"
        port = endpoint.port or (443 if endpoint.scheme == "https" else 80)
        conn_cls = http.client.HTTPSConnection if endpoint.scheme == "https" else http.client.HTTPConnection
        conn = conn_cls(host, port, timeout=60)
        try:
            headers = {
                name: value
                for name, value in self.headers.items()
                if name.lower() not in HOP_BY_HOP_HEADERS
            }
            conn.request(self.command, self.path, body=body, headers=headers)
            response = conn.getresponse()
            payload = response.read()

            self.send_response(response.status, response.reason)
            upstream_content_length = None
            for name, value in response.getheaders():
                lower = name.lower()
                if lower == "content-length":
                    upstream_content_length = value
                    continue
                if lower not in HOP_BY_HOP_HEADERS:
                    self.send_header(name, value)
            if self.command == "HEAD":
                if upstream_content_length is not None:
                    self.send_header("Content-Length", upstream_content_length)
            else:
                self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(payload)
        except OSError as exc:
            message = str(exc).encode("utf-8", errors="replace")
            self.send_response(502, "Bad Gateway")
            self.send_header("Content-Length", str(len(message)))
            self.end_headers()
            self.wfile.write(message)
        finally:
            conn.close()


@contextlib.contextmanager
def recovering_cache_service(origin: OriginProxyState, service_url: str, *, gate_seconds: float = 17):
    """Fail one metadata warm, then hold two origin publications across cooldown."""
    lock = threading.Lock()
    stopping = threading.Event()
    requests: list[dict[str, Any]] = []
    gates: list[dict[str, Any]] = []
    failed = False
    gate_active = False
    started = time.monotonic()
    original_record = origin.record
    forwarding = OriginProxyState(service_url, "v1")

    def elapsed() -> float:
        return time.monotonic() - started

    def is_metadata(raw_path: str, bucket: str) -> bool:
        path = urllib.parse.urlsplit(raw_path).path
        prefix = f"/{bucket}/"
        return path.startswith(prefix) and CacheServiceRustfsSmoke.is_versioned_metadb_key(
            urllib.parse.unquote(path[len(prefix):])
        )

    def record_origin(method: str, path: str) -> None:
        nonlocal gate_active
        original_record(method, path)
        gate = None
        with lock:
            if failed and method == "PUT" and is_metadata(path, origin.bucket) and not gate_active and len(gates) < 2:
                gate_active = True
                gate = {"path": urllib.parse.urlsplit(path).path, "start_s": elapsed()}
                gates.append(gate)
        if gate is not None:
            # Two sequential holds leave lease/control traffic live and avoid
            # making any single origin request exceed its own HTTP deadline.
            cancelled = stopping.wait(gate_seconds)
            with lock:
                gate.update(end_s=elapsed(), cancelled=cancelled)
                gate_active = False

    class Handler(CountingProxyHandler):
        state = forwarding

        def handle(self) -> None:
            try:
                super().handle()
            except (ConnectionResetError, BrokenPipeError):
                # Closing the owned endpoint or a client's keep-alive socket
                # must not leave handler threads waiting on request headers.
                pass

        def send_response(self, code: int, message: str | None = None) -> None:
            self.observed_status = code
            super().send_response(code, message)

        def proxy_request(self) -> None:
            nonlocal failed
            self.observed_status = None
            entry = {"method": self.command, "path": urllib.parse.urlsplit(self.path).path, "start_s": elapsed()}
            with lock:
                inject = not failed and self.command == "PUT" and is_metadata(self.path, "v1")
                failed = failed or inject
                requests.append(entry)
            try:
                if not inject:
                    super().proxy_request()
                    return
                length = int(self.headers.get("Content-Length", "0") or "0")
                body = self.rfile.read(length)
                with lock:
                    entry.update(injected=True, body_len=len(body), body_sha256=hashlib.sha256(body).hexdigest())
                self.send_response(503)
                self.send_header("Content-Length", "0")
                self.end_headers()
                self.wfile.flush()
            finally:
                with lock:
                    entry.update(status=self.observed_status, end_s=elapsed())

    def snapshot() -> dict[str, Any]:
        with lock:
            return {"requests": [dict(row) for row in requests], "origin_gates": [dict(row) for row in gates]}

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin.record = record_origin
        try:
            yield f"http://127.0.0.1:{server.server_port}", snapshot
        finally:
            stopping.set()
            origin.record = original_record
            server.shutdown()
            forwarding.close_connections()
            thread.join(timeout=5)


class CacheServiceRustfsSmoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        if self.run_id in {".", ".."} or any(
            not (character.isascii() and (character.isalnum() or character in "._-"))
            for character in self.run_id
        ):
            raise SmokeError("run ID must be one non-traversing ASCII path component")
        self.run_root = args.root / self.run_id
        try:
            self.run_root.mkdir(parents=True, exist_ok=False)
        except FileExistsError as exc:
            raise SmokeError("run directory already exists; choose a fresh run ID") from exc
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.private = self.run_root / "private"
        self.cache_root = self.run_root / "server-cache"
        self.client_cache = self.run_root / "client-cache"
        self.command_index = 0
        self.command_lock = threading.Lock()
        self.crab_bin = resolve_executable(args.crab_bin) or args.crab_bin
        self.cache_server_bin = resolve_executable(args.cache_server_bin) or args.cache_server_bin
        self.env = self.build_env()
        self.report = SmokeReport(
            run_id=self.run_id,
            status="running",
            root=str(self.run_root),
            endpoint_url=args.endpoint_url,
            bucket=args.bucket,
            env=redact_env(self.env),
            updated_at=utc_now(),
        )
        self.proxy_state: OriginProxyState | None = None
        self.proxy_server: ThreadingHTTPServer | None = None
        self.proxy_thread: threading.Thread | None = None
        self.cache_proc: subprocess.Popen[bytes] | None = None
        self.cache_service_url = ""
        self.onboarding_bundle: Path | None = None

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_ENDPOINT_URL": self.args.endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "CRAB_CACHE_DIR": str(self.client_cache),
                "CRAB_CACHE_PSK": self.args.cache_psk,
                "CRAB_METADB_CHUNK_INDEX_PATH": ".crab/chunk_index_db/",
                "GIT_TERMINAL_PROMPT": "0",
            }
        )
        crab_path = resolve_executable(self.crab_bin)
        if crab_path is not None:
            helper_dir = str(Path(crab_path).parent)
            env["PATH"] = helper_dir + os.pathsep + env.get("PATH", "")
        return env

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.report.updated_at = utc_now()
        path = self.artifacts / "report.json"
        self.set_report_artifact("report", path)
        payload = asdict(self.report)
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    def artifact_ref(self, path: Path) -> str:
        return artifact_reference(path, self.artifacts)

    def set_report_artifact(self, key: str, path: Path) -> None:
        self.report.artifacts[key] = self.artifact_ref(path)

    def report_artifact_path(self, key: str) -> Path:
        value = self.report.artifacts[key]
        path = Path(value)
        if not path.is_absolute():
            path = self.artifacts / path
        return path.resolve()

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report.checks.append(
            {
                "name": name,
                "ok": ok,
                "detail": detail or {},
                "timestamp": utc_now(),
            }
        )
        self.write_report()
        if not ok:
            raise SmokeError(f"check failed: {name}")

    def record_read(self, name: str, key: str, status: int, cache_status: str, body_len: int) -> None:
        state = self.require_proxy_state()
        self.report.reads.append(
            asdict(
                CacheReadRecord(
                    name=name,
                    key=key,
                    status=status,
                    cache_status=cache_status,
                    origin_gets_for_key=state.count_for_key(key),
                    body_len=body_len,
                )
            )
        )
        self.write_report()

    def next_log_paths(self, name: str) -> tuple[Path, Path]:
        with self.command_lock:
            self.command_index += 1
            index = self.command_index
        base = f"{index:03d}-{slug(name)}"
        return self.logs / f"{base}.out.log", self.logs / f"{base}.err.log"

    def run_cmd(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        *,
        check: bool = True,
        timeout: int | None = None,
        env: dict[str, str] | None = None,
        report_args: list[str] | None = None,
    ) -> CommandRecord:
        stdout_log, stderr_log = self.next_log_paths(name)
        started = time.monotonic()
        exit_code = None
        timeout_error = None
        with stdout_log.open("wb") as stdout, stderr_log.open("wb") as stderr:
            try:
                proc = subprocess.run(
                    args,
                    cwd=cwd,
                    env=env or self.env,
                    stdout=stdout,
                    stderr=stderr,
                    timeout=timeout or self.args.timeout,
                    check=False,
                )
                exit_code = proc.returncode
            except subprocess.TimeoutExpired as exc:
                # subprocess.run kills and waits for its direct child. Preserve
                # the timed-out attempt before propagating, without inventing
                # an exit code that the dependency does not return.
                timeout_error = exc
            except FileNotFoundError as exc:
                raise SmokeError(f"{name} could not start: {exc}") from exc
        record = CommandRecord(
            name=name,
            args=report_args or args,
            cwd=str(cwd),
            exit_code=exit_code,
            duration_ms=int((time.monotonic() - started) * 1000),
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
            timed_out=timeout_error is not None,
        )
        self.report.commands.append(asdict(record))
        if timeout_error is not None:
            self.report.status = "failed"
        self.write_report()
        if timeout_error is not None:
            raise timeout_error
        if check and exit_code != 0:
            stderr = stderr_log.read_text(encoding="utf-8", errors="replace")[-2000:]
            raise SmokeError(f"{name} failed with exit {exit_code}: {stderr}")
        return record

    def run_aws(self, name: str, args: list[str], *, check: bool = True) -> CommandRecord:
        return self.run_cmd(
            "aws " + name,
            ["aws", "s3api", *args, "--endpoint-url", self.args.endpoint_url],
            self.run_root,
            check=check,
        )

    def binary_version(self, binary: str) -> str:
        proc = subprocess.run(
            [binary, "--version"],
            cwd=self.run_root,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
            check=False,
        )
        if proc.returncode != 0:
            raise SmokeError(
                f"{binary} --version failed with exit {proc.returncode}: "
                f"{proc.stderr[-500:]}"
            )
        return proc.stdout.strip()

    def write_evidence_manifest(self) -> None:
        manifest_path = self.artifacts / "cache-service-evidence-manifest.json"
        self.set_report_artifact("cache_service_evidence_manifest", manifest_path)
        smoke_script = Path(__file__).resolve()
        verifier_script = smoke_script.parents[1] / "verify-cache-service-smoke-report.py"
        retained_smoke_script = self.artifacts / "rustfs-smoke-script.py"
        retained_verifier_script = self.artifacts / "smoke-report-verifier.py"
        shutil.copyfile(smoke_script, retained_smoke_script)
        shutil.copyfile(verifier_script, retained_verifier_script)
        self.set_report_artifact("rustfs_smoke_script", retained_smoke_script)
        self.set_report_artifact("smoke_report_verifier", retained_verifier_script)
        self.write_report()

        report_path = self.report_artifact_path("report")
        preflight_path = self.report_artifact_path("cache_server_preflight_json")
        manifest = {
            "schema": EVIDENCE_MANIFEST_SCHEMA,
            "generated_at": utc_now(),
            "run_id": self.run_id,
            "artifacts": {
                "report": file_evidence(report_path, self.artifacts),
                "cache_server_preflight_json": file_evidence(preflight_path, self.artifacts),
                "rustfs_smoke_script": file_evidence(retained_smoke_script, self.artifacts),
                "smoke_report_verifier": file_evidence(retained_verifier_script, self.artifacts),
            },
            "runtime": {
                "crab_bin": self.crab_bin,
                "crab_version": self.binary_version(self.crab_bin),
                "cache_server_bin": self.cache_server_bin,
                "cache_server_version": self.binary_version(self.cache_server_bin),
                "rustfs_endpoint": self.args.endpoint_url,
                "rustfs_bucket": self.args.bucket,
            },
            "parameters": {
                "object_kib": self.args.object_kib,
                "cli_file_kib": self.args.cli_file_kib,
                "max_cache_bytes": self.args.max_cache_bytes,
                "dedup_scope": "all",
                "mutable_path_mode": "strict",
            },
        }
        for key in (
            "cache_server_config",
            "transparent_cache_server_config",
            "cache_server_policy",
            "onboarding_check_json",
            "onboarding_probe_json",
            "onboarding_client_probe_json",
            "onboarding_client_config",
            "onboarding_client_env",
            "onboarding_readme",
        ):
            if key in self.report.artifacts:
                manifest["artifacts"][key] = file_evidence(
                    self.report_artifact_path(key),
                    self.artifacts,
                )
        manifest_path.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    def client_env(self, cache_name: str) -> dict[str, str]:
        env = self.env.copy()
        cache_dir = self.run_root / cache_name
        # Crab owns private-root creation. A precreated 0755 directory makes
        # local cache I/O bypass rather than qualifying its normal behavior.
        env["CRAB_CACHE_DIR"] = str(cache_dir)
        if self.report.origin_proxy_url:
            env["AWS_ENDPOINT_URL"] = self.report.origin_proxy_url
        return env

    def psk_hash(self) -> str:
        if self.args.cache_psk == DEFAULT_PSK and not shutil.which("b3sum"):
            return DEFAULT_PSK_BLAKE3
        b3sum = shutil.which("b3sum")
        if b3sum is None:
            raise SmokeError("b3sum is required when --cache-psk is customized")
        proc = subprocess.run(
            [b3sum],
            input=self.args.cache_psk.encode("utf-8"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if proc.returncode != 0:
            raise SmokeError(proc.stderr.decode("utf-8", errors="replace"))
        return proc.stdout.decode("utf-8").split()[0]

    def write_policy(self) -> Path:
        policy_path = self.private / "policy.yaml"
        retained_policy_path = self.artifacts / "policy.yaml"
        policy = "\n".join(
            [
                "rules:",
                '  - principal: "psk-client"',
                f'    repos: ["{REMOTE_PREFIX}/{self.run_id}/*", ".crab"]',
                '    actions: ["read", "write", "dedup", "admin"]',
                "",
            ]
        )
        retained_policy = "\n".join(
            [
                "rules:",
                '  - principal: "<redacted>"',
                '    repos: ["<run-scope>", ".crab"]',
                '    actions: ["read", "write", "dedup", "admin"]',
                "",
            ]
        )
        policy_path.write_text(policy, encoding="utf-8")
        retained_policy_path.write_text(retained_policy, encoding="utf-8")
        self.set_report_artifact("cache_server_policy", retained_policy_path)
        self.write_report()
        return policy_path

    def render_enterprise_onboarding_bundle(self, listen_port: int) -> Path:
        cache_bin = resolve_executable(self.cache_server_bin)
        if cache_bin is None:
            raise SmokeError(f"cache server binary not found: {self.args.cache_server_bin}")

        bundle = self.private / "enterprise-onboarding"
        policy_path = bundle / "policy.yaml"
        cache_service_url = f"http://127.0.0.1:{listen_port}"
        psk_hash = self.psk_hash()
        render_args = [
            cache_bin,
            "onboarding",
            "render",
            "--output-dir",
            str(bundle),
            "--origin-url",
            f"s3://{self.args.bucket}",
            "--cache-service-url",
            cache_service_url,
            "--repo-prefix",
            f"{REMOTE_PREFIX}/{self.run_id}/*",
            "--psk-hash",
            psk_hash,
            "--cache-root",
            str(self.cache_root),
            "--max-cache-bytes",
            str(self.args.max_cache_bytes),
            "--listen-addr",
            f"127.0.0.1:{listen_port}",
            "--policy-path",
            str(policy_path),
            "--force",
        ]
        report_args = list(render_args)
        report_args[report_args.index("--psk-hash") + 1] = "<redacted-psk-hash>"
        self.run_cmd(
            "crab-cache-server onboarding render",
            render_args,
            self.run_root,
            timeout=self.args.startup_timeout,
            report_args=report_args,
        )

        check_record = self.run_cmd(
            "crab-cache-server onboarding check",
            [
                cache_bin,
                "onboarding",
                "check",
                "--bundle-dir",
                str(bundle),
                "--json",
            ],
            self.run_root,
            timeout=self.args.startup_timeout,
        )
        check_text = Path(check_record.stdout_log).read_text(encoding="utf-8")
        check_json_path = self.artifacts / "onboarding-check.json"
        check_json_path.write_text(check_text, encoding="utf-8")
        self.set_report_artifact("onboarding_check_json", check_json_path)

        check_payload = json.loads(check_text)
        probe_env = self.env.copy()
        if self.report.origin_proxy_url:
            probe_env["AWS_ENDPOINT_URL"] = self.report.origin_proxy_url
        probe_record = self.run_cmd(
            "crab-cache-server onboarding probe",
            [
                cache_bin,
                "onboarding",
                "probe",
                "--bundle-dir",
                str(bundle),
                "--json",
                "--trusted-proxy-boundary",
            ],
            self.run_root,
            timeout=self.args.startup_timeout,
            env=probe_env,
        )
        probe_text = Path(probe_record.stdout_log).read_text(encoding="utf-8")
        probe_json_path = self.artifacts / "onboarding-probe.json"
        probe_json_path.write_text(probe_text, encoding="utf-8")
        self.set_report_artifact("onboarding_probe_json", probe_json_path)
        probe_payload = json.loads(probe_text)
        server_config_path = bundle / "server-config.toml"
        policy_path = bundle / "policy.yaml"
        client_config_path = bundle / "client-config.toml"
        client_env_path = bundle / "client.env"
        readme_path = bundle / "README.md"
        server_config = server_config_path.read_text(encoding="utf-8")
        policy = policy_path.read_text(encoding="utf-8")
        client_config = client_config_path.read_text(encoding="utf-8")
        client_env = client_env_path.read_text(encoding="utf-8")

        retained_config_path = self.artifacts / "cache-server.toml"
        retained_policy_path = self.artifacts / "policy.yaml"
        retained_client_config_path = self.artifacts / "onboarding-client-config.toml"
        retained_client_env_path = self.artifacts / "onboarding-client.env"
        retained_readme_path = self.artifacts / "onboarding-README.md"
        retained_config_path.write_text(
            server_config.replace(f'psk_hash = "{psk_hash}"', 'psk_hash = "<redacted>"'),
            encoding="utf-8",
        )
        retained_policy_path.write_text(
            policy.replace('principal: "psk-client"', 'principal: "<redacted>"')
            .replace(f'"{REMOTE_PREFIX}/{self.run_id}/*"', '"<run-scope>"'),
            encoding="utf-8",
        )
        shutil.copyfile(client_config_path, retained_client_config_path)
        shutil.copyfile(client_env_path, retained_client_env_path)
        shutil.copyfile(readme_path, retained_readme_path)
        self.set_report_artifact("cache_server_config", retained_config_path)
        self.set_report_artifact("cache_server_policy", retained_policy_path)
        self.set_report_artifact("onboarding_client_config", retained_client_config_path)
        self.set_report_artifact("onboarding_client_env", retained_client_env_path)
        self.set_report_artifact("onboarding_readme", retained_readme_path)

        self.report.enterprise_onboarding.append(
            asdict(
                EnterpriseOnboardingRecord(
                    name="rendered-bundle",
                    bundle=str(bundle),
                    check_status=str(check_payload.get("status", "")),
                    probe_status=str(probe_payload.get("status", "")),
                    server_config=self.artifact_ref(retained_config_path),
                    policy=self.artifact_ref(retained_policy_path),
                    client_config=self.artifact_ref(retained_client_config_path),
                    client_env=self.artifact_ref(retained_client_env_path),
                )
            )
        )
        self.write_report()

        self.check(
            "enterprise-onboarding-rendered",
            all(
                path.is_file()
                for path in [
                    server_config_path,
                    policy_path,
                    client_config_path,
                    client_env_path,
                    readme_path,
                ]
            ),
        )
        self.check(
            "enterprise-onboarding-check-ok",
            check_payload.get("status") == "ok",
            {"status": check_payload.get("status")},
        )
        self.check(
            "enterprise-onboarding-probe-ok-or-warn",
            probe_payload.get("status") in ("ok", "warn"),
            {"status": probe_payload.get("status")},
        )
        self.check(
            "enterprise-onboarding-policy-path-wired",
            f'policy_path = "{policy_path}"' in server_config,
            {"policy_path": str(policy_path)},
        )
        self.check(
            "enterprise-onboarding-client-config-cache-dedup",
            f'service_url = "{cache_service_url}"' in client_config
            and 'service_mode = "cache+dedup"' in client_config
            and 'service_auth = "psk"' in client_config
            and "push_warming = true" in client_config,
        )
        self.check(
            "enterprise-onboarding-client-env-secret-manager-placeholder",
            "CRAB_CACHE_PSK" in client_env
            and self.args.cache_psk not in client_env
            and psk_hash not in client_env,
        )
        self.check(
            "enterprise-onboarding-check-secret-redacted",
            self.args.cache_psk not in check_text
            and psk_hash not in check_text
            and "psk-client" not in check_text,
        )
        self.check(
            "enterprise-onboarding-probe-secret-redacted",
            self.args.cache_psk not in probe_text
            and psk_hash not in probe_text
            and "psk-client" not in probe_text,
        )
        self.onboarding_bundle = bundle
        return server_config_path

    def preflight(self) -> None:
        self.logs.mkdir(parents=True, exist_ok=True)
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.private.mkdir(parents=True, exist_ok=True)
        self.private.chmod(0o700)
        self.cache_root.mkdir(parents=True, exist_ok=True)
        self.client_cache.mkdir(parents=True, exist_ok=True)
        self.write_report()

        for binary in ("git", "aws"):
            self.check(f"{binary}-available", shutil.which(binary) is not None)
        self.check(
            "crab-available",
            resolve_executable(self.crab_bin) is not None,
            {"crab_bin": self.args.crab_bin},
        )
        self.check(
            "crab-cache-server-available",
            resolve_executable(self.cache_server_bin) is not None,
            {"cache_server_bin": self.args.cache_server_bin},
        )

        helper_bin = self.run_root / "bin" / "git-remote-crab"
        helper_bin.parent.mkdir(parents=True, exist_ok=True)
        if helper_bin.exists() or helper_bin.is_symlink():
            helper_bin.unlink()
        try:
            helper_bin.symlink_to(Path(self.crab_bin))
        except (NotImplementedError, OSError):
            shutil.copy2(self.crab_bin, helper_bin)
        self.env["PATH"] = str(helper_bin.parent) + os.pathsep + self.env.get("PATH", "")
        helper = shutil.which("git-remote-crab", path=self.env.get("PATH"))
        self.check(
            "git-remote-crab-available",
            helper is not None,
            {"helper": helper, "crab_bin": self.crab_bin},
        )

        try:
            with urllib.request.urlopen(self.args.endpoint_url, timeout=5) as response:
                status = response.status
        except urllib.error.HTTPError as exc:
            status = exc.code
        except OSError as exc:
            self.check("rustfs-endpoint-reachable", False, {"error": str(exc)})
            return
        self.check("rustfs-endpoint-reachable", status < 500, {"status": status})

        self.create_disposable_bucket()

    def create_disposable_bucket(self) -> None:
        # CreateBucket can succeed for an already-owned bucket. Refuse reuse
        # before any write; real push exercises the default bucket-global index.
        record = self.run_aws("list buckets", ["list-buckets", "--output", "json"])
        buckets = load_json_file(Path(record.stdout_log))["Buckets"]
        self.check(
            "bucket-is-new",
            all(bucket["Name"] != self.args.bucket for bucket in buckets),
            {"bucket": self.args.bucket},
        )
        self.run_aws(
            "create bucket", ["create-bucket", "--bucket", self.args.bucket]
        )

    def start_origin_proxy(self) -> None:
        state = OriginProxyState(self.args.endpoint_url, self.args.bucket)

        class Handler(CountingProxyHandler):
            pass

        Handler.state = state
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, name="origin-proxy", daemon=True)
        thread.start()
        self.proxy_state = state
        self.proxy_server = server
        self.proxy_thread = thread
        proxy_url = f"http://127.0.0.1:{server.server_address[1]}"
        self.report.origin_proxy_url = proxy_url
        self.write_report()
        self.check("origin-counting-proxy-started", True, {"url": proxy_url})

    def write_cache_server_config(
        self,
        listen_port: int,
        *,
        config_name: str = "cache-server.toml",
        mutable_path_mode: str = "strict",
        cache_root: Path | None = None,
        artifact_key: str = "cache_server_config",
    ) -> Path:
        config_path = self.private / config_name
        retained_config_path = self.artifacts / config_name
        policy_path = self.write_policy()
        cache_root = self.cache_root if cache_root is None else cache_root
        psk_hash = self.psk_hash()
        config = "\n".join(
            [
                "[server]",
                f'listen_addr = "127.0.0.1:{listen_port}"',
                f'mutable_path_mode = "{mutable_path_mode}"',
                f"policy_path = {json.dumps(str(policy_path))}",
                "drain_timeout_secs = 1",
                "",
                "[auth]",
                'mechanism = "psk"',
                f'psk_hash = "{psk_hash}"',
                "",
                "[origin]",
                f'url = "s3://{self.args.bucket}"',
                "",
                "[cache]",
                f"root = {json.dumps(str(cache_root))}",
                f"max_bytes = {self.args.max_cache_bytes}",
                "",
                "[dedup]",
                'scope = "all"',
                "",
                "[eviction]",
                "high_water_ratio = 0.95",
                "low_water_ratio = 0.90",
                "",
            ]
        )
        retained_config = config.replace(
            f"policy_path = {json.dumps(str(policy_path))}",
            f"policy_path = {json.dumps(self.report.artifacts['cache_server_policy'])}",
        ).replace(f'psk_hash = "{psk_hash}"', 'psk_hash = "<redacted>"')
        config_path.write_text(config, encoding="utf-8")
        retained_config_path.write_text(retained_config, encoding="utf-8")
        self.set_report_artifact(artifact_key, retained_config_path)
        self.write_report()
        return config_path

    def start_cache_server(self) -> None:
        state = self.require_proxy_state()
        origin_gets_before_start = state.total_get_count()
        proxy_url = self.report.origin_proxy_url
        listen_port = find_free_port()
        config_path = self.render_enterprise_onboarding_bundle(listen_port)
        cache_bin = resolve_executable(self.cache_server_bin)
        if cache_bin is None:
            raise SmokeError(f"cache server binary not found: {self.args.cache_server_bin}")

        stdout_log = self.logs / "cache-server.out.log"
        stderr_log = self.logs / "cache-server.err.log"
        self.set_report_artifact("cache_server_stdout", stdout_log)
        self.set_report_artifact("cache_server_stderr", stderr_log)
        self.write_report()

        env = self.env.copy()
        env["AWS_ENDPOINT_URL"] = proxy_url
        env["RUST_LOG"] = self.args.cache_server_log
        record = self.run_cmd(
            "crab-cache-server preflight",
            [
                cache_bin,
                "--config",
                str(config_path),
                "check",
                "--json",
                "--profile",
                "enterprise",
                "--trusted-proxy-boundary",
            ],
            self.run_root,
            env=env,
            timeout=self.args.startup_timeout,
        )
        report_text = Path(record.stdout_log).read_text(encoding="utf-8")
        preflight_json = self.artifacts / "cache-server-preflight.json"
        preflight_json.write_text(report_text, encoding="utf-8")
        self.set_report_artifact("cache_server_preflight_json", preflight_json)
        self.write_report()

        payload = json.loads(report_text)
        summary = payload.get("summary", {})
        checks = payload.get("checks", [])
        by_name = {str(check.get("name")): check for check in checks}
        startup_check = by_name.get("startup components")
        origin_check = by_name.get("origin")
        policy_check = by_name.get("authorization policy")
        enterprise_check = by_name.get("enterprise profile")
        policy_diagnostics = summary.get("policy_diagnostics") or {}
        issue_codes = {
            str(check.get("code"))
            for check in checks
            if check.get("code")
        }
        self.check(
            "cache-server-preflight-no-failures",
            payload.get("status") in ("ok", "warn"),
            {"status": payload.get("status"), "codes": sorted(issue_codes)},
        )
        self.check(
            "cache-server-preflight-startup-ok",
            startup_check is not None and startup_check.get("status") == "ok",
            {"detail": startup_check.get("detail") if startup_check else None},
        )
        self.check(
            "cache-server-preflight-origin-ok",
            origin_check is not None and origin_check.get("status") == "ok",
            {"detail": origin_check.get("detail") if origin_check else None},
        )
        self.check(
            "cache-server-preflight-policy-loaded",
            policy_check is not None and policy_check.get("status") == "ok",
            {"detail": policy_check},
        )
        self.check(
            "cache-server-preflight-enterprise-profile-ok",
            enterprise_check is not None and enterprise_check.get("status") == "ok",
            {"detail": enterprise_check},
        )
        self.check(
            "cache-server-preflight-max-object-bytes-present",
            isinstance(summary.get("max_object_bytes"), int)
            and summary["max_object_bytes"] > 0,
            {"summary": summary},
        )
        self.check(
            "cache-server-preflight-policy-diagnostics",
            policy_diagnostics
            == {
                "rule_count": 1,
                "repo_pattern_count": 2,
                "actions": ["read", "write", "dedup", "admin"],
            }
            and "psk-client" not in json.dumps(policy_diagnostics),
            {"policy_diagnostics": policy_diagnostics},
        )
        self.check(
            "cache-server-preflight-no-enterprise-profile-failures",
            not any(code.startswith("enterprise_") for code in issue_codes),
            {"codes": sorted(issue_codes)},
        )
        psk_hash = self.psk_hash()
        self.check(
            "cache-server-preflight-secret-redacted",
            self.args.cache_psk not in report_text
            and psk_hash not in report_text
            and "psk-client" not in report_text,
            {"checked": ["cache_psk", "psk_hash", "policy_principal"]},
        )
        stdout = stdout_log.open("wb")
        stderr = stderr_log.open("wb")
        proc = subprocess.Popen(
            [cache_bin, "--config", str(config_path)],
            cwd=self.run_root,
            env=env,
            stdout=stdout,
            stderr=stderr,
        )
        stdout.close()
        stderr.close()
        self.cache_proc = proc
        self.cache_service_url = f"http://127.0.0.1:{listen_port}"
        self.report.cache_service_url = self.cache_service_url
        self.write_report()

        deadline = time.monotonic() + self.args.startup_timeout
        healthy = False
        last_error = ""
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                last_error = f"cache server exited with {proc.returncode}"
                break
            try:
                with urllib.request.urlopen(f"{self.cache_service_url}/v1/health", timeout=2) as response:
                    healthy = response.status == 200
                    if healthy:
                        break
            except OSError as exc:
                last_error = str(exc)
                time.sleep(0.1)
        self.check("cache-server-health", healthy, {"last_error": last_error})
        self.check(
            "cache-server-origin-health-did-not-get-object",
            state.total_get_count() == origin_gets_before_start,
            {
                "before": origin_gets_before_start,
                "after": state.total_get_count(),
            },
        )

    def probe_enterprise_onboarding_client(self) -> None:
        if self.onboarding_bundle is None:
            raise SmokeError("enterprise onboarding bundle is not rendered")
        cache_bin = resolve_executable(self.cache_server_bin)
        if cache_bin is None:
            raise SmokeError(f"cache server binary not found: {self.args.cache_server_bin}")

        probe_repo = f"{REMOTE_PREFIX}/{self.run_id}/client-config"
        env = self.env.copy()
        env["CRAB_CACHE_PSK"] = self.args.cache_psk
        record = self.run_cmd(
            "crab-cache-server onboarding active client probe",
            [
                cache_bin,
                "onboarding",
                "probe",
                "--bundle-dir",
                str(self.onboarding_bundle),
                "--json",
                "--trusted-proxy-boundary",
                "--client-probe",
                "--client-probe-repo",
                probe_repo,
            ],
            self.run_root,
            timeout=self.args.startup_timeout,
            env=env,
            check=False,
        )
        probe_text = Path(record.stdout_log).read_text(encoding="utf-8", errors="replace")
        probe_json_path = self.artifacts / "onboarding-client-probe.json"
        probe_json_path.write_text(probe_text, encoding="utf-8")
        self.set_report_artifact("onboarding_client_probe_json", probe_json_path)
        self.write_report()

        try:
            probe_payload = json.loads(probe_text)
        except json.JSONDecodeError as exc:
            probe_payload = {"parse_error": str(exc)}
        client_probe = probe_payload.get("client_probe") if isinstance(probe_payload, dict) else None
        self.check(
            "enterprise-onboarding-client-probe-ok",
            record.exit_code == 0
            and isinstance(client_probe, dict)
            and probe_payload.get("status") == "ok"
            and client_probe.get("status") == "ok",
            {
                "exit_code": record.exit_code,
                "status": probe_payload.get("status") if isinstance(probe_payload, dict) else None,
                "client_probe_status": client_probe.get("status") if isinstance(client_probe, dict) else None,
            },
        )
        psk_hash = self.psk_hash()
        self.check(
            "enterprise-onboarding-client-probe-secret-redacted",
            self.args.cache_psk not in probe_text
            and psk_hash not in probe_text
            and "psk-client" not in probe_text,
        )

    def start_transparent_cache_server(self) -> tuple[subprocess.Popen[bytes], str]:
        self.require_proxy_state()
        proxy_url = self.report.origin_proxy_url
        listen_port = find_free_port()
        config_path = self.write_cache_server_config(
            listen_port,
            config_name="transparent-cache-server.toml",
            mutable_path_mode="transparent",
            cache_root=self.run_root / "transparent-server-cache",
            artifact_key="transparent_cache_server_config",
        )
        cache_bin = resolve_executable(self.cache_server_bin)
        if cache_bin is None:
            raise SmokeError(f"cache server binary not found: {self.args.cache_server_bin}")

        stdout_log = self.logs / "transparent-cache-server.out.log"
        stderr_log = self.logs / "transparent-cache-server.err.log"
        self.set_report_artifact("transparent_cache_server_stdout", stdout_log)
        self.set_report_artifact("transparent_cache_server_stderr", stderr_log)
        self.write_report()

        env = self.env.copy()
        env["AWS_ENDPOINT_URL"] = proxy_url
        env["RUST_LOG"] = self.args.cache_server_log
        stdout = stdout_log.open("wb")
        stderr = stderr_log.open("wb")
        proc = subprocess.Popen(
            [cache_bin, "--config", str(config_path)],
            cwd=self.run_root,
            env=env,
            stdout=stdout,
            stderr=stderr,
        )
        stdout.close()
        stderr.close()
        service_url = f"http://127.0.0.1:{listen_port}"

        deadline = time.monotonic() + self.args.startup_timeout
        healthy = False
        last_error = ""
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                last_error = f"transparent cache server exited with {proc.returncode}"
                break
            try:
                with urllib.request.urlopen(f"{service_url}/v1/health", timeout=2) as response:
                    healthy = response.status == 200
                    if healthy:
                        break
            except OSError as exc:
                last_error = str(exc)
                time.sleep(0.1)
        if not healthy:
            self.terminate_process(proc)
        self.check("transparent-cache-server-health", healthy, {"last_error": last_error})
        return proc, service_url

    def configure_client_repo(self) -> Path:
        repo = self.run_root / "client-config"
        repo.mkdir(parents=True, exist_ok=True)
        remote_url = f"crab://{self.args.bucket}/{REMOTE_PREFIX}/{self.run_id}/client-config"
        self.run_cmd("git init client config", ["git", "init", "-b", "main"], repo)
        self.run_cmd(
            "crab init client config",
            [self.crab_bin, "init", remote_url],
            repo,
        )
        self.configure_repo_cache_service(repo)
        config_path = repo / ".crab" / "local.toml"
        config = config_path.read_text(encoding="utf-8")
        self.set_report_artifact("client_config", config_path)
        self.check("client-cache-service-url-configured", self.cache_service_url in config)
        self.check("client-cache-service-psk-not-written-to-config", self.args.cache_psk not in config)
        return repo

    def configure_repo_cache_service(self, repo: Path, *, env: dict[str, str] | None = None) -> None:
        if self.onboarding_bundle is None:
            raise SmokeError("enterprise onboarding bundle is not rendered")
        config_path = repo / ".crab" / "local.toml"
        client_config_path = self.onboarding_bundle / "client-config.toml"
        existing = config_path.read_text(encoding="utf-8")
        client_config = client_config_path.read_text(encoding="utf-8")
        marker = "# Generated by `crab-cache-server onboarding render`."
        if marker in existing:
            existing = existing[: existing.index(marker)].rstrip()
        config_path.write_text(
            existing.rstrip() + "\n\n" + client_config,
            encoding="utf-8",
        )
        repo_name = slug(repo.name)
        self.check(
            f"{repo_name}-onboarding-client-config-installed",
            self.cache_service_url in client_config
            and 'service_mode = "cache+dedup"' in client_config
            and 'service_auth = "psk"' in client_config
            and "push_warming = true" in client_config
            and self.args.cache_psk not in client_config,
        )

    def verify_doctor_cache_service(self, repo: Path) -> None:
        record = self.run_cmd(
            "crab doctor cache service",
            [self.crab_bin, "doctor", "--json"],
            repo,
        )
        payload = json.loads(Path(record.stdout_log).read_text(encoding="utf-8"))
        checks = payload.get("data", {}).get("checks", [])
        by_name = {str(check.get("name")): check for check in checks}
        cache_check = by_name.get("cache service")
        auth_check = by_name.get("cache service auth")
        caps_check = by_name.get("cache service caps")
        authz_check = by_name.get("cache service authz")
        admin_check = by_name.get("cache service admin")
        self.check(
            "doctor-cache-service-check-present",
            cache_check is not None
            and auth_check is not None
            and caps_check is not None
            and authz_check is not None
            and admin_check is not None,
            {"checks": [check.get("name") for check in checks]},
        )
        self.check(
            "doctor-cache-service-health-ok",
            cache_check is not None and cache_check.get("status") == "ok",
            {"detail": cache_check.get("detail") if cache_check else None},
        )
        self.check(
            "doctor-cache-service-auth-ok",
            auth_check is not None and auth_check.get("status") == "ok",
            {"detail": auth_check.get("detail") if auth_check else None},
        )
        self.check(
            "doctor-cache-service-caps-ok",
            caps_check is not None and caps_check.get("status") == "ok",
            {"detail": caps_check.get("detail") if caps_check else None},
        )
        self.check(
            "doctor-cache-service-authz-ok",
            authz_check is not None and authz_check.get("status") == "ok",
            {"detail": authz_check.get("detail") if authz_check else None},
        )
        self.check(
            "doctor-cache-service-admin-ok",
            admin_check is not None and admin_check.get("status") == "ok",
            {"detail": admin_check.get("detail") if admin_check else None},
        )
        cache_details = " ".join(
            str(check.get("detail", ""))
            for check in checks
            if str(check.get("name", "")).startswith("cache service")
        )
        self.check(
            "doctor-cache-service-secret-redacted",
            self.args.cache_psk not in cache_details,
            {"details": cache_details},
        )

    def verify_doctor_cache_service_active_probe(self, repo: Path) -> None:
        record = self.run_cmd(
            "crab doctor cache service active probe",
            [self.crab_bin, "doctor", "--json", "--cache-service-active-probe"],
            repo,
        )
        payload = json.loads(Path(record.stdout_log).read_text(encoding="utf-8"))
        checks = payload.get("data", {}).get("checks", [])
        by_name = {str(check.get("name")): check for check in checks}
        active_check = by_name.get("cache service active")
        self.check(
            "doctor-cache-service-active-check-present",
            active_check is not None,
            {"checks": [check.get("name") for check in checks]},
        )
        self.check(
            "doctor-cache-service-active-ok",
            active_check is not None and active_check.get("status") == "ok",
            {"detail": active_check.get("detail") if active_check else None},
        )
        cache_details = " ".join(
            str(check.get("detail", ""))
            for check in checks
            if str(check.get("name", "")).startswith("cache service")
        )
        self.check(
            "doctor-cache-service-active-secret-redacted",
            self.args.cache_psk not in cache_details,
            {"details": cache_details},
        )

    def verify_support_bundle(
        self,
        repo: Path,
        name: str,
        *,
        expect_origin_degraded: bool = False,
    ) -> None:
        bundle_path = self.artifacts / f"{slug(name)}.json"
        record = self.run_cmd(
            f"{name} crab doctor support bundle",
            [
                self.crab_bin,
                "doctor",
                "--support-bundle",
                "--json",
                "--output",
                str(bundle_path),
            ],
            repo,
        )
        stdout_text = Path(record.stdout_log).read_text(encoding="utf-8", errors="replace")
        bundle_text = bundle_path.read_text(encoding="utf-8")
        envelope = json.loads(stdout_text)
        bundle = json.loads(bundle_text)
        data = envelope.get("data", {})
        probes = bundle.get("probes", {})
        signals = bundle.get("signals", {})
        service = bundle.get("service", {})
        metrics_totals = probes.get("metrics_totals", {})
        admin_snapshot = probes.get("admin_snapshot", {})
        traffic = admin_snapshot.get("traffic", {})
        limits = admin_snapshot.get("limits", {})
        if not isinstance(limits, dict):
            limits = {}
        capabilities = probes.get("capabilities_snapshot", {})
        if not isinstance(capabilities, dict):
            capabilities = {}
        capability_limits = capabilities.get("limits", {})
        if not isinstance(capability_limits, dict):
            capability_limits = {}
        capability_routes = capabilities.get("routes", {})
        if not isinstance(capability_routes, dict):
            capability_routes = {}
        authz = probes.get("authz_snapshot", {})
        if not isinstance(authz, dict):
            authz = {}
        authz_actions = authz.get("actions", {})
        if not isinstance(authz_actions, dict):
            authz_actions = {}
        eviction = admin_snapshot.get("eviction", {})
        if not isinstance(eviction, dict):
            eviction = {}
        metric_admin_pairs = {
            "cache_hit_total": "cache_hits",
            "origin_avoided_reads_total": "origin_avoided_reads",
            "cache_miss_total": "cache_misses",
            "origin_fetch_total": "origin_fetches",
            "cache_bytes_served": "bytes_served_total",
            "push_warming_total": "push_warming_writes",
            "mutable_path_proxy_read_total": "mutable_proxy_reads",
        }

        def int_field(payload: dict[str, Any], key: str, default: int) -> int:
            value = payload.get(key)
            if isinstance(value, (int, float)):
                return int(value)
            return default

        def probe_for(probe_name: str) -> dict[str, Any]:
            probe = probes.get(probe_name, {})
            return probe if isinstance(probe, dict) else {}

        def probe_ok(probe_name: str) -> bool | None:
            value = probe_for(probe_name).get("ok")
            return value if isinstance(value, bool) else None

        def probe_status(probe_name: str) -> int | None:
            value = probe_for(probe_name).get("http_status")
            if isinstance(value, bool) or not isinstance(value, int):
                return None
            return value

        def probe_endpoint(probe_name: str) -> str | None:
            value = probe_for(probe_name).get("endpoint")
            return value if isinstance(value, str) else None

        self.set_report_artifact(f"{slug(name)}_support_bundle", bundle_path)
        self.report.support_bundles.append(
            asdict(
                SupportBundleRecord(
                    name=name,
                    path=self.artifact_ref(bundle_path),
                    schema=str(envelope.get("schema", "")),
                    health_ok=probe_ok("health"),
                    health_status=probe_status("health"),
                    auth_ok=probe_ok("auth"),
                    auth_status=probe_status("auth"),
                    auth_endpoint=probe_endpoint("auth"),
                    capabilities_ok=probe_ok("capabilities"),
                    capabilities_status=probe_status("capabilities"),
                    authz_ok=probe_ok("authz"),
                    authz_status=probe_status("authz"),
                    admin_stats_ok=probe_ok("admin_stats"),
                    admin_stats_status=probe_status("admin_stats"),
                    metrics_ok=probe_ok("metrics"),
                    metrics_status=probe_status("metrics"),
                    cache_hit_rate=signals.get("cache_hit_rate"),
                    origin_fallback_rate=signals.get("origin_fallback_rate"),
                    integrity_repairs=signals.get("integrity_repairs"),
                    push_warming_writes=signals.get("push_warming_writes"),
                    evicted_objects=signals.get("evicted_objects"),
                    capabilities_schema=str(capabilities.get("schema", "")),
                    capabilities_max_cache_bytes=capability_limits.get("max_cache_bytes"),
                    capabilities_max_object_bytes=capability_limits.get("max_object_bytes"),
                    authz_schema=str(authz.get("schema", "")),
                    authz_read=authz_actions.get("read"),
                    authz_write=authz_actions.get("write"),
                    authz_dedup=authz_actions.get("dedup"),
                    authz_admin=authz_actions.get("admin"),
                    max_object_bytes=limits.get("max_object_bytes"),
                    cache_hit_total=metrics_totals.get("cache_hit_total"),
                    origin_avoided_reads_total=metrics_totals.get("origin_avoided_reads_total"),
                    origin_fetch_total=metrics_totals.get("origin_fetch_total"),
                    cache_eviction_total=metrics_totals.get("cache_eviction_total"),
                    cache_max_bytes=metrics_totals.get("cache_max_bytes"),
                    cache_max_object_bytes=metrics_totals.get("cache_max_object_bytes"),
                )
            )
        )
        self.write_report()

        self.check(
            f"{name}-support-bundle-schema",
            envelope.get("schema") == "cache-service.support-bundle",
            {"schema": envelope.get("schema")},
        )
        self.check(f"{name}-support-bundle-output-matches-stdout", data == bundle)
        self.check(f"{name}-support-bundle-redacted", bundle.get("redacted") is True)
        self.check(
            f"{name}-support-bundle-secret-redacted",
            self.args.cache_psk not in bundle_text and self.args.cache_psk not in stdout_text,
        )
        self.check(
            f"{name}-support-bundle-url-redacted",
            self.cache_service_url not in bundle_text
            and self.cache_service_url not in stdout_text,
        )
        self.check(
            f"{name}-support-bundle-service-configured",
            service.get("configured") is True
            and service.get("service_url") == "configured-redacted"
            and service.get("mode") == "cache+dedup"
            and service.get("push_warming") is True,
            {"service": service},
        )
        self.check(
            f"{name}-support-bundle-auth-posture-redacted",
            service.get("auth") == "psk via CRAB_CACHE_PSK",
            {"auth": service.get("auth")},
        )

        self.check(
            f"{name}-support-bundle-auth-probe-control-plane",
            probe_endpoint("auth") == "/v1/capabilities",
            {"probe": probe_for("auth")},
        )

        if expect_origin_degraded:
            self.check(
                f"{name}-support-bundle-health-probe-degraded",
                probe_ok("health") is False and probe_status("health") == 503,
                {"probe": probe_for("health")},
            )
            for probe_name in ("auth", "capabilities", "authz", "admin_stats", "metrics"):
                self.check(
                    f"{name}-support-bundle-{probe_name}-probe-ok",
                    probe_ok(probe_name) is True and probe_status(probe_name) == 200,
                    {"probe": probe_for(probe_name)},
                )
        else:
            for probe_name in ("health", "auth", "capabilities", "authz", "admin_stats", "metrics"):
                self.check(
                    f"{name}-support-bundle-{probe_name}-probe-ok",
                    probe_ok(probe_name) is True,
                    {"probe": probe_for(probe_name)},
                )

        self.check(
            f"{name}-support-bundle-capabilities-schema",
            capabilities.get("schema") == "crab-cache-service.capabilities.v1",
            {"capabilities": capabilities},
        )
        self.check(
            f"{name}-support-bundle-capabilities-route-schema",
            capability_routes.get("schema") == EXPECTED_ROUTE_SCHEMA,
            {"routes": capability_routes},
        )
        self.check(
            f"{name}-support-bundle-capabilities-cache-limit-matches-admin",
            int_field(capability_limits, "max_cache_bytes", -1)
            == int_field(limits, "max_cache_bytes", -2),
            {"capability_limits": capability_limits, "admin_limits": limits},
        )
        self.check(
            f"{name}-support-bundle-capabilities-object-limit-matches-admin",
            int_field(capability_limits, "max_object_bytes", -1)
            == int_field(limits, "max_object_bytes", -2),
            {"capability_limits": capability_limits, "admin_limits": limits},
        )
        self.check(
            f"{name}-support-bundle-authz-schema",
            authz.get("schema") == "crab-cache-service.authz-check.v1",
            {"authz": authz},
        )
        self.check(
            f"{name}-support-bundle-authz-repo",
            authz.get("repo_path") == f"{REMOTE_PREFIX}/{self.run_id}/client-config",
            {"authz": authz},
        )
        self.check(
            f"{name}-support-bundle-authz-actions-allowed",
            all(authz_actions.get(action) is True for action in ("read", "write", "dedup", "admin")),
            {"actions": authz_actions},
        )

        self.check(
            f"{name}-support-bundle-admin-snapshot-present",
            isinstance(traffic, dict) and bool(traffic),
            {"traffic_keys": sorted(traffic.keys()) if isinstance(traffic, dict) else []},
        )
        self.check(
            f"{name}-support-bundle-cache-hit-rate-positive",
            isinstance(signals.get("cache_hit_rate"), (int, float))
            and signals["cache_hit_rate"] > 0,
            {"signals": signals},
        )
        self.check(
            f"{name}-support-bundle-origin-fallback-bounded",
            isinstance(signals.get("origin_fallback_rate"), (int, float))
            and 0 <= signals["origin_fallback_rate"] < 1,
            {"signals": signals},
        )
        self.check(
            f"{name}-support-bundle-push-warming-observed",
            isinstance(signals.get("push_warming_writes"), int)
            and signals["push_warming_writes"] > 0,
            {"signals": signals},
        )
        self.check(
            f"{name}-support-bundle-integrity-repairs-observed",
            isinstance(signals.get("integrity_repairs"), int)
            and signals["integrity_repairs"] >= len(self.report.cache_integrity_repairs),
            {"signals": signals},
        )
        self.check(
            f"{name}-support-bundle-no-mutable-proxy-reads",
            signals.get("mutable_proxy_reads") == 0,
            {"signals": signals},
        )
        self.check(
            f"{name}-support-bundle-metrics-cache-hits",
            metrics_totals.get("cache_hit_total", 0) > 0,
            {"metrics_totals": metrics_totals},
        )
        self.check(
            f"{name}-support-bundle-metrics-origin-avoidance",
            metrics_totals.get("origin_avoided_reads_total", 0) > 0,
            {"metrics_totals": metrics_totals},
        )
        self.check(
            f"{name}-support-bundle-metrics-origin-fetches",
            metrics_totals.get("origin_fetch_total", 0) > 0,
            {"metrics_totals": metrics_totals},
        )
        self.check(
            f"{name}-support-bundle-metrics-push-warming",
            metrics_totals.get("push_warming_total", 0) > 0,
            {"metrics_totals": metrics_totals},
        )
        self.check(
            f"{name}-support-bundle-metrics-match-admin-snapshot",
            all(
                int_field(metrics_totals, metric, -1) == int_field(traffic, admin, -2)
                for metric, admin in metric_admin_pairs.items()
            ),
            {
                "metrics_totals": {
                    metric: metrics_totals.get(metric)
                    for metric in metric_admin_pairs
                },
                "admin_traffic": {
                    admin: traffic.get(admin)
                    for admin in metric_admin_pairs.values()
                },
            },
        )
        self.check(
            f"{name}-support-bundle-eviction-metrics-match-admin",
            int_field(metrics_totals, "cache_eviction_total", -1)
            == int_field(eviction, "total", -2),
            {
                "metrics_cache_eviction_total": metrics_totals.get("cache_eviction_total"),
                "admin_eviction": eviction,
            },
        )
        self.check(
            f"{name}-support-bundle-cache-max-matches-admin",
            int_field(metrics_totals, "cache_max_bytes", -1)
            == int_field(admin_snapshot, "max_bytes", -2),
            {
                "metrics_cache_max_bytes": metrics_totals.get("cache_max_bytes"),
                "admin_max_bytes": admin_snapshot.get("max_bytes"),
            },
        )
        self.check(
            f"{name}-support-bundle-object-max-matches-admin",
            int_field(metrics_totals, "cache_max_object_bytes", -1)
            == int_field(limits, "max_object_bytes", -2),
            {
                "metrics_cache_max_object_bytes": metrics_totals.get("cache_max_object_bytes"),
                "admin_limits": limits,
            },
        )
        self.check(
            f"{name}-support-bundle-eviction-signal-matches-admin",
            int_field(signals, "evicted_objects", -1)
            == int_field(eviction, "total", -2),
            {
                "signal_evicted_objects": signals.get("evicted_objects"),
                "admin_eviction": eviction,
            },
        )

    def require_proxy_state(self) -> OriginProxyState:
        if self.proxy_state is None:
            raise SmokeError("origin proxy is not started")
        return self.proxy_state

    def origin_key(self, name: str, data: bytes) -> str:
        identity = hashlib.sha256(name.encode("utf-8") + b"\0" + data).hexdigest()
        return f"{REMOTE_PREFIX}/{self.run_id}/direct/{name}/packs/{identity}.pack"

    def put_origin_object(self, key: str, data: bytes) -> None:
        # Synthetic bodies must never become bucket-global metadata or overwrite
        # another run. Real global objects are created only through Crab's push.
        prefixes = (f"{REMOTE_PREFIX}/{self.run_id}/", f"{REMOTE_PREFIX}-denied/{self.run_id}/")
        if not key.startswith(prefixes) or "\\" in key or any(
            part in {"", ".", ".."} for part in key.split("/")
        ):
            raise SmokeError(f"synthetic origin write is outside this run's prefixes: {key}")
        body_path = self.artifacts / f"{slug(key)}.bin"
        body_path.write_bytes(data)
        self.run_aws(
            "put " + key,
            [
                "put-object", "--bucket", self.args.bucket, "--key", key,
                "--body", str(body_path), "--if-none-match", "*",
            ],
        )

    def get_origin_object(self, key: str) -> bytes:
        body_path = self.artifacts / f"origin-{slug(key)}.bin"
        self.run_aws(
            "get " + key,
            ["get-object", "--bucket", self.args.bucket, "--key", key, str(body_path)],
        )
        return body_path.read_bytes()

    def cache_get(
        self,
        key: str,
        *,
        byte_range: str | None = None,
        psk: str | None = None,
        include_psk: bool = True,
        base_url: str | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        headers = {}
        if include_psk:
            headers["x-cache-psk"] = self.args.cache_psk if psk is None else psk
        if byte_range is not None:
            headers["Range"] = byte_range
        request = urllib.request.Request(
            f"{base_url or self.cache_service_url}/v1/{key}",
            method="GET",
            headers=headers,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                return response.status, {k.lower(): v for k, v in response.headers.items()}, response.read()
        except urllib.error.HTTPError as exc:
            return exc.code, {k.lower(): v for k, v in exc.headers.items()}, exc.read()

    def cache_put(
        self,
        key: str,
        data: bytes,
        *,
        psk: str | None = None,
        include_psk: bool = True,
        base_url: str | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        headers = {}
        if include_psk:
            headers["x-cache-psk"] = self.args.cache_psk if psk is None else psk
        request = urllib.request.Request(
            f"{base_url or self.cache_service_url}/v1/{key}",
            data=data,
            method="PUT",
            headers=headers,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                return response.status, {k.lower(): v for k, v in response.headers.items()}, response.read()
        except urllib.error.HTTPError as exc:
            return exc.code, {k.lower(): v for k, v in exc.headers.items()}, exc.read()

    def cache_put_declared_length_without_body(
        self,
        key: str,
        declared_content_length: int,
    ) -> tuple[int, bytes]:
        parsed = urllib.parse.urlparse(self.cache_service_url)
        if parsed.scheme != "http":
            raise SmokeError(f"raw request-limit probe requires http URL: {self.cache_service_url}")
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        request = (
            f"PUT /v1/{key} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            f"x-cache-psk: {self.args.cache_psk}\r\n"
            f"Content-Length: {declared_content_length}\r\n"
            "Connection: close\r\n"
            "\r\n"
        ).encode("ascii")
        with socket.create_connection((host, port), timeout=self.args.startup_timeout) as sock:
            sock.settimeout(self.args.startup_timeout)
            sock.sendall(request)
            response = sock.recv(4096)

        parts = response.split(b" ", 2)
        if len(parts) < 2 or not parts[1].isdigit():
            raise SmokeError(
                "request-limit probe returned malformed HTTP response: "
                + response[:200].decode("utf-8", errors="replace")
            )
        return int(parts[1]), response

    def record_auth_control(
        self,
        name: str,
        key: str,
        status: int,
        cache_status: str,
        before: int,
        after: int,
        body_len: int,
    ) -> None:
        self.report.auth_controls.append(
            asdict(
                AuthControlRecord(
                    name=name,
                    key=key,
                    status=status,
                    cache_status=cache_status,
                    origin_gets_before=before,
                    origin_gets_after=after,
                    body_len=body_len,
                )
            )
        )
        self.write_report()

    def cache_head(
        self,
        key: str,
        *,
        psk: str | None = None,
        include_psk: bool = True,
        base_url: str | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        headers = {}
        if include_psk:
            headers["x-cache-psk"] = self.args.cache_psk if psk is None else psk
        request = urllib.request.Request(
            f"{base_url or self.cache_service_url}/v1/{key}",
            method="HEAD",
            headers=headers,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                return response.status, {k.lower(): v for k, v in response.headers.items()}, response.read()
        except urllib.error.HTTPError as exc:
            return exc.code, {k.lower(): v for k, v in exc.headers.items()}, exc.read()

    def cache_probe(self, path: str) -> tuple[int, bytes]:
        request = urllib.request.Request(
            f"{self.cache_service_url}{path}",
            method="GET",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read()

    def cache_admin_evict_path(self, key: str) -> int:
        request = urllib.request.Request(
            f"{self.cache_service_url}/v1/admin/evict",
            data=json.dumps({"path": key}).encode("utf-8"),
            method="POST",
            headers={
                "content-type": "application/json",
                "x-cache-psk": self.args.cache_psk,
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                response.read()
                return response.status
        except urllib.error.HTTPError as exc:
            exc.read()
            return exc.code

    def cache_admin_stats(
        self,
        *,
        base_url: str | None = None,
        artifact_name: str = "admin-stats.json",
        artifact_key: str = "admin_stats",
        check_name: str = "admin-stats-status",
    ) -> dict[str, Any]:
        request = urllib.request.Request(
            f"{base_url or self.cache_service_url}/v1/admin/stats",
            method="GET",
            headers={"x-cache-psk": self.args.cache_psk},
        )
        with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
            body = response.read()
            self.check(check_name, response.status == 200, {"status": response.status})
        stats = json.loads(body.decode("utf-8"))
        path = self.artifacts / artifact_name
        path.write_text(json.dumps(stats, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.set_report_artifact(artifact_key, path)
        return stats

    def cache_capabilities(self) -> tuple[int, dict[str, Any], bytes]:
        request = urllib.request.Request(
            f"{self.cache_service_url}/v1/capabilities",
            method="GET",
            headers={"x-cache-psk": self.args.cache_psk},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
                body = response.read()
                return response.status, json.loads(body.decode("utf-8")), body
        except urllib.error.HTTPError as exc:
            return exc.code, {}, exc.read()

    def verify_capabilities_contract(self) -> None:
        admin_stats = self.cache_admin_stats()
        admin_limits = admin_stats.get("limits", {})
        if not isinstance(admin_limits, dict):
            admin_limits = {}
        status, capabilities, body = self.cache_capabilities()
        limits = capabilities.get("limits", {}) if isinstance(capabilities, dict) else {}
        if not isinstance(limits, dict):
            limits = {}
        routes = capabilities.get("routes", {}) if isinstance(capabilities, dict) else {}
        if not isinstance(routes, dict):
            routes = {}

        def int_field(payload: dict[str, Any], key: str) -> int:
            value = payload.get(key)
            if isinstance(value, bool) or not isinstance(value, int):
                return 0
            return value

        def route_patterns(payload: Any) -> list[str]:
            if not isinstance(payload, list):
                return []
            return [
                str(route.get("pattern", ""))
                for route in payload
                if isinstance(route, dict) and isinstance(route.get("pattern"), str)
            ]

        record = CapabilitiesRecord(
            name="cache-service-capabilities",
            status=status,
            schema=str(capabilities.get("schema", "")),
            route_schema=str(routes.get("schema", "")),
            route_transport_prefix=str(routes.get("transport_prefix", "")),
            immutable_route_patterns=route_patterns(routes.get("immutable")),
            mutable_route_patterns=route_patterns(routes.get("mutable")),
            max_cache_bytes=int_field(limits, "max_cache_bytes"),
            max_object_bytes=int_field(limits, "max_object_bytes"),
            admin_max_cache_bytes=int_field(admin_limits, "max_cache_bytes"),
            admin_max_object_bytes=int_field(admin_limits, "max_object_bytes"),
        )
        self.report.capabilities.append(asdict(record))
        self.write_report()

        self.check("capabilities-status", status == 200, {"status": status})
        self.check(
            "capabilities-schema",
            record.schema == "crab-cache-service.capabilities.v1",
            {"schema": record.schema},
        )
        self.check(
            "capabilities-route-schema",
            record.route_schema == EXPECTED_ROUTE_SCHEMA,
            {"route_schema": record.route_schema},
        )
        self.check(
            "capabilities-route-transport-prefix",
            record.route_transport_prefix == "/v1/",
            {"route_transport_prefix": record.route_transport_prefix},
        )
        self.check(
            "capabilities-immutable-route-contract",
            record.immutable_route_patterns == EXPECTED_IMMUTABLE_ROUTE_PATTERNS,
            {"immutable_route_patterns": record.immutable_route_patterns},
        )
        self.check(
            "capabilities-mutable-route-contract",
            record.mutable_route_patterns == EXPECTED_MUTABLE_ROUTE_PATTERNS,
            {"mutable_route_patterns": record.mutable_route_patterns},
        )
        self.check(
            "capabilities-cache-limit-matches-admin",
            record.max_cache_bytes == record.admin_max_cache_bytes and record.max_cache_bytes > 0,
            {
                "capabilities": record.max_cache_bytes,
                "admin": record.admin_max_cache_bytes,
            },
        )
        self.check(
            "capabilities-object-limit-matches-admin",
            record.max_object_bytes == record.admin_max_object_bytes
            and record.max_object_bytes > 0,
            {
                "capabilities": record.max_object_bytes,
                "admin": record.admin_max_object_bytes,
            },
        )
        self.check(
            "capabilities-secret-not-returned",
            self.args.cache_psk.encode("utf-8") not in body,
        )

    def assert_cache_read(
        self,
        name: str,
        key: str,
        data: bytes,
        *,
        expected_status: int,
        expected_cache: str,
        expected_origin_gets: int,
        byte_range: str | None = None,
        expected_body: bytes | None = None,
        expected_content_range: str | None = None,
    ) -> None:
        status, headers, body = self.cache_get(key, byte_range=byte_range)
        expected_body = data if expected_body is None else expected_body
        self.record_read(name, key, status, headers.get("x-cache", ""), len(body))
        self.check(f"{name}-status", status == expected_status, {"status": status})
        self.check(f"{name}-body", body == expected_body, {"body_len": len(body)})
        self.check(
            f"{name}-x-cache",
            headers.get("x-cache") == expected_cache,
            {"x-cache": headers.get("x-cache", "")},
        )
        if expected_content_range is not None:
            self.check(
                f"{name}-content-range",
                headers.get("content-range") == expected_content_range,
                {
                    "expected": expected_content_range,
                    "actual": headers.get("content-range", ""),
                },
            )
        self.check(
            f"{name}-origin-gets",
            self.require_proxy_state().count_for_key(key) == expected_origin_gets,
            {
                "expected": expected_origin_gets,
                "actual": self.require_proxy_state().count_for_key(key),
            },
        )

    def assert_cache_head(
        self,
        name: str,
        key: str,
        *,
        expected_status: int,
        expected_cache: str,
        expected_content_length: int,
        expected_origin_gets: int,
        expected_origin_heads: int,
    ) -> None:
        status, headers, body = self.cache_head(key)
        state = self.require_proxy_state()
        self.record_read(name, key, status, headers.get("x-cache", ""), len(body))
        self.check(f"{name}-status", status == expected_status, {"status": status})
        self.check(f"{name}-body-empty", body == b"", {"body_len": len(body)})
        self.check(
            f"{name}-x-cache",
            headers.get("x-cache") == expected_cache,
            {"x-cache": headers.get("x-cache", "")},
        )
        self.check(
            f"{name}-content-length",
            headers.get("content-length") == str(expected_content_length),
            {
                "expected": expected_content_length,
                "actual": headers.get("content-length"),
            },
        )
        self.check(
            f"{name}-origin-gets",
            state.count_for_key(key) == expected_origin_gets,
            {"expected": expected_origin_gets, "actual": state.count_for_key(key)},
        )
        self.check(
            f"{name}-origin-heads",
            state.count_head_for_key(key) == expected_origin_heads,
            {
                "expected": expected_origin_heads,
                "actual": state.count_head_for_key(key),
            },
        )

    def assert_malformed_range_rejected(self, name: str, key: str) -> None:
        state = self.require_proxy_state()
        before_gets = state.count_for_key(key)
        before_heads = state.count_head_for_key(key)
        status, headers, body = self.cache_get(key, byte_range="bytes=abc-def")
        self.record_read(name, key, status, headers.get("x-cache", ""), len(body))
        self.check(f"{name}-status", status == 400, {"status": status})
        self.check(
            f"{name}-no-cache-status",
            "x-cache" not in headers,
            {"x-cache": headers.get("x-cache", "")},
        )
        self.check(
            f"{name}-origin-gets",
            state.count_for_key(key) == before_gets,
            {"expected": before_gets, "actual": state.count_for_key(key)},
        )
        self.check(
            f"{name}-origin-heads",
            state.count_head_for_key(key) == before_heads,
            {
                "expected": before_heads,
                "actual": state.count_head_for_key(key),
            },
        )

    def assert_mutable_route_rejected(self, name: str, key: str) -> None:
        before = self.require_proxy_state().count_for_key(key)
        status, headers, body = self.cache_get(key)
        self.record_read(name, key, status, headers.get("x-cache", ""), len(body))
        self.check(f"{name}-status", status == 400, {"status": status})
        self.check(
            f"{name}-no-cache-status",
            "x-cache" not in headers,
            {"x-cache": headers.get("x-cache", "")},
        )
        self.check(
            f"{name}-origin-gets",
            self.require_proxy_state().count_for_key(key) == before,
            {
                "expected": before,
                "actual": self.require_proxy_state().count_for_key(key),
            },
        )

    def record_mutable_route_behavior(
        self,
        name: str,
        pattern: str,
        key: str,
        status: int,
        cache_status: str,
        origin_gets_before: int,
        origin_gets_after: int,
        body_len: int,
    ) -> None:
        self.report.mutable_route_behaviors.append(
            asdict(
                MutableRouteBehaviorRecord(
                    name=name,
                    pattern=pattern,
                    key=key,
                    status=status,
                    cache_status=cache_status,
                    origin_gets_before=origin_gets_before,
                    origin_gets_after=origin_gets_after,
                    body_len=body_len,
                )
            )
        )
        self.write_report()

    def assert_mutable_route_pattern_rejected(self, pattern: str, key: str) -> None:
        name = f"route-contract-mutable-{slug(pattern)}"
        state = self.require_proxy_state()
        before = state.count_for_key(key)
        status, headers, body = self.cache_get(key)
        after = state.count_for_key(key)
        cache_status = headers.get("x-cache", "")
        self.record_mutable_route_behavior(
            name,
            pattern,
            key,
            status,
            cache_status,
            before,
            after,
            len(body),
        )
        self.record_read(name, key, status, cache_status, len(body))
        self.check(f"{name}-status", status == 400, {"status": status})
        self.check(
            f"{name}-no-cache-status",
            "x-cache" not in headers,
            {"x-cache": cache_status},
        )
        self.check(
            f"{name}-origin-gets-flat",
            after == before,
            {"before": before, "after": after},
        )

    def record_mutable_route_write_behavior(
        self,
        record: MutableRouteWriteBehaviorRecord,
    ) -> None:
        self.report.mutable_route_write_behaviors.append(asdict(record))
        self.write_report()

    def assert_mutable_route_pattern_write_rejected(self, pattern: str, key: str) -> None:
        name = f"route-contract-mutable-write-{slug(pattern)}"
        data = deterministic_bytes(257, f"{self.run_id}:mutable-write:{pattern}")
        state = self.require_proxy_state()
        before_stats = self.cache_admin_stats()
        before_gets = state.count_for_key(key)
        before_puts = state.count_put_for_key(key)
        before_total_gets = state.total_get_count()
        before_total_puts = state.total_put_count()

        status, headers, body = self.cache_put(key, data)

        after_stats = self.cache_admin_stats()
        after_gets = state.count_for_key(key)
        after_puts = state.count_put_for_key(key)
        after_total_gets = state.total_get_count()
        after_total_puts = state.total_put_count()
        cache_status = headers.get("x-cache", "")
        record = MutableRouteWriteBehaviorRecord(
            name=name,
            pattern=pattern,
            key=key,
            status=status,
            cache_status=cache_status,
            origin_gets_before=before_gets,
            origin_gets_after=after_gets,
            origin_puts_before=before_puts,
            origin_puts_after=after_puts,
            total_origin_gets_before=before_total_gets,
            total_origin_gets_after=after_total_gets,
            total_origin_puts_before=before_total_puts,
            total_origin_puts_after=after_total_puts,
            total_bytes_before=int(before_stats.get("total_bytes", 0)),
            total_bytes_after=int(after_stats.get("total_bytes", 0)),
            push_warming_writes_before=self.traffic_value(before_stats, "push_warming_writes"),
            push_warming_writes_after=self.traffic_value(after_stats, "push_warming_writes"),
            push_warming_bytes_before=self.traffic_value(before_stats, "push_warming_bytes"),
            push_warming_bytes_after=self.traffic_value(after_stats, "push_warming_bytes"),
            request_body_len=len(data),
            response_body_len=len(body),
        )
        self.record_mutable_route_write_behavior(record)

        self.check(f"{name}-status", status == 400, {"status": status})
        self.check(
            f"{name}-no-cache-status",
            "x-cache" not in headers,
            {"x-cache": cache_status},
        )
        self.check(
            f"{name}-origin-gets-flat",
            after_gets == before_gets,
            {"before": before_gets, "after": after_gets},
        )
        self.check(
            f"{name}-origin-puts-flat",
            after_puts == before_puts,
            {"before": before_puts, "after": after_puts},
        )
        self.check(
            f"{name}-total-origin-traffic-flat",
            after_total_gets == before_total_gets and after_total_puts == before_total_puts,
            {
                "gets_before": before_total_gets,
                "gets_after": after_total_gets,
                "puts_before": before_total_puts,
                "puts_after": after_total_puts,
            },
        )
        self.check(
            f"{name}-cache-bytes-flat",
            record.total_bytes_after == record.total_bytes_before,
            {"before": record.total_bytes_before, "after": record.total_bytes_after},
        )
        self.check(
            f"{name}-push-warming-flat",
            record.push_warming_writes_after == record.push_warming_writes_before
            and record.push_warming_bytes_after == record.push_warming_bytes_before,
            {
                "writes_before": record.push_warming_writes_before,
                "writes_after": record.push_warming_writes_after,
                "bytes_before": record.push_warming_bytes_before,
                "bytes_after": record.push_warming_bytes_after,
            },
        )
        self.check(
            f"{name}-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in body,
        )

    def record_immutable_route_behavior(
        self,
        name: str,
        pattern: str,
        key: str,
        first: tuple[int, dict[str, str], bytes],
        second: tuple[int, dict[str, str], bytes],
        range_result: tuple[int, dict[str, str], bytes],
        origin_gets_before: int,
        origin_gets_after_first: int,
        origin_gets_after_second: int,
        origin_gets_after_range: int,
    ) -> None:
        first_status, first_headers, first_body = first
        second_status, second_headers, _second_body = second
        range_status, range_headers, range_body = range_result
        self.report.immutable_route_behaviors.append(
            asdict(
                ImmutableRouteBehaviorRecord(
                    name=name,
                    pattern=pattern,
                    key=key,
                    first_status=first_status,
                    first_cache_status=first_headers.get("x-cache", ""),
                    second_status=second_status,
                    second_cache_status=second_headers.get("x-cache", ""),
                    range_status=range_status,
                    range_cache_status=range_headers.get("x-cache", ""),
                    origin_gets_before=origin_gets_before,
                    origin_gets_after_first=origin_gets_after_first,
                    origin_gets_after_second=origin_gets_after_second,
                    origin_gets_after_range=origin_gets_after_range,
                    body_len=len(first_body),
                    range_body_len=len(range_body),
                )
            )
        )
        self.write_report()

    def assert_immutable_route_pattern_cached(
        self,
        pattern: str,
        key: str,
        data: bytes | None = None,
    ) -> None:
        name = f"route-contract-immutable-{slug(pattern)}"
        if data is not None:
            self.put_origin_object(key, data)

        state = self.require_proxy_state()
        before = state.count_for_key(key)
        first = self.cache_get(key)
        after_first = state.count_for_key(key)
        second = self.cache_get(key)
        after_second = state.count_for_key(key)
        first_status, first_headers, first_body = first
        second_status, second_headers, second_body = second
        range_end = min(3, len(first_body) - 1)
        range_header = f"bytes=1-{range_end}" if range_end >= 1 else "bytes=0-0"
        range_result = self.cache_get(key, byte_range=range_header)
        after_range = state.count_for_key(key)
        range_status, range_headers, range_body = range_result

        self.record_immutable_route_behavior(
            name,
            pattern,
            key,
            first,
            second,
            range_result,
            before,
            after_first,
            after_second,
            after_range,
        )
        self.record_read(name + "-first", key, first_status, first_headers.get("x-cache", ""), len(first_body))
        self.record_read(name + "-second", key, second_status, second_headers.get("x-cache", ""), len(second_body))
        self.record_read(name + "-range", key, range_status, range_headers.get("x-cache", ""), len(range_body))

        self.check(f"{name}-first-status", first_status == 200, {"status": first_status})
        self.check(
            f"{name}-first-cache-status",
            first_headers.get("x-cache") in {"MISS", "HIT"},
            {"x-cache": first_headers.get("x-cache", "")},
        )
        if data is not None:
            self.check(f"{name}-first-body", first_body == data, {"body_len": len(first_body)})
        else:
            self.check(f"{name}-first-body-present", len(first_body) > 3, {"body_len": len(first_body)})

        expected_after_first = before + (1 if first_headers.get("x-cache") == "MISS" else 0)
        self.check(
            f"{name}-first-origin-gets",
            after_first == expected_after_first,
            {
                "before": before,
                "after": after_first,
                "x-cache": first_headers.get("x-cache", ""),
            },
        )
        self.check(f"{name}-second-status", second_status == 200, {"status": second_status})
        self.check(
            f"{name}-second-cache-hit",
            second_headers.get("x-cache") == "HIT",
            {"x-cache": second_headers.get("x-cache", "")},
        )
        self.check(f"{name}-second-body", second_body == first_body, {"body_len": len(second_body)})
        self.check(
            f"{name}-second-origin-gets-flat",
            after_second == after_first,
            {"after_first": after_first, "after_second": after_second},
        )
        expected_range_body = first_body[1 : range_end + 1] if range_end >= 1 else first_body[0:1]
        self.check(f"{name}-range-status", range_status == 206, {"status": range_status})
        self.check(
            f"{name}-range-cache-hit",
            range_headers.get("x-cache") == "HIT",
            {"x-cache": range_headers.get("x-cache", "")},
        )
        self.check(f"{name}-range-body", range_body == expected_range_body, {"body_len": len(range_body)})
        self.check(
            f"{name}-range-origin-gets-flat",
            after_range == after_second,
            {"after_second": after_second, "after_range": after_range},
        )

    def record_immutable_route_write_behavior(
        self,
        record: ImmutableRouteWriteBehaviorRecord,
    ) -> None:
        self.report.immutable_route_write_behaviors.append(asdict(record))
        self.write_report()

    def record_immutable_poisoning_control(
        self,
        record: ImmutablePoisoningControlRecord,
    ) -> None:
        self.report.immutable_poisoning_controls.append(asdict(record))
        self.write_report()

    def assert_immutable_route_pattern_push_warmed(
        self,
        pattern: str,
        key: str,
        data: bytes,
    ) -> None:
        name = f"route-contract-immutable-write-{slug(pattern)}"
        evict_status = self.cache_admin_evict_path(key)
        self.check(f"{name}-evict-status", evict_status == 200, {"status": evict_status})

        state = self.require_proxy_state()
        before_stats = self.cache_admin_stats()
        before_gets = state.count_for_key(key)
        before_puts = state.count_put_for_key(key)
        before_total_gets = state.total_get_count()
        before_total_puts = state.total_put_count()

        put_status, put_headers, put_body = self.cache_put(key, data)
        after_put = state.count_for_key(key)
        get_status, get_headers, get_body = self.cache_get(key)
        after_get = state.count_for_key(key)
        head_status, head_headers, head_body = self.cache_head(key)
        after_head = state.count_for_key(key)
        range_end = min(3, len(data) - 1)
        range_header = f"bytes=1-{range_end}" if range_end >= 1 else "bytes=0-0"
        range_status, range_headers, range_body = self.cache_get(key, byte_range=range_header)
        after_range = state.count_for_key(key)
        after_stats = self.cache_admin_stats()
        after_puts = state.count_put_for_key(key)
        after_total_gets = state.total_get_count()
        after_total_puts = state.total_put_count()
        expected_range_body = data[1 : range_end + 1] if range_end >= 1 else data[0:1]

        record = ImmutableRouteWriteBehaviorRecord(
            name=name,
            pattern=pattern,
            key=key,
            put_status=put_status,
            put_cache_status=put_headers.get("x-cache", ""),
            get_status=get_status,
            get_cache_status=get_headers.get("x-cache", ""),
            head_status=head_status,
            head_cache_status=head_headers.get("x-cache", ""),
            range_status=range_status,
            range_cache_status=range_headers.get("x-cache", ""),
            evict_status=evict_status,
            origin_gets_before=before_gets,
            origin_gets_after_put=after_put,
            origin_gets_after_get=after_get,
            origin_gets_after_head=after_head,
            origin_gets_after_range=after_range,
            origin_puts_before=before_puts,
            origin_puts_after=after_puts,
            total_origin_gets_before=before_total_gets,
            total_origin_gets_after=after_total_gets,
            total_origin_puts_before=before_total_puts,
            total_origin_puts_after=after_total_puts,
            total_bytes_before=int(before_stats.get("total_bytes", 0)),
            total_bytes_after=int(after_stats.get("total_bytes", 0)),
            push_warming_writes_before=self.traffic_value(before_stats, "push_warming_writes"),
            push_warming_writes_after=self.traffic_value(after_stats, "push_warming_writes"),
            push_warming_bytes_before=self.traffic_value(before_stats, "push_warming_bytes"),
            push_warming_bytes_after=self.traffic_value(after_stats, "push_warming_bytes"),
            body_len=len(data),
            get_body_len=len(get_body),
            range_body_len=len(range_body),
        )
        self.record_immutable_route_write_behavior(record)
        self.record_read(name + "-get", key, get_status, get_headers.get("x-cache", ""), len(get_body))
        self.record_read(name + "-range", key, range_status, range_headers.get("x-cache", ""), len(range_body))

        self.check(f"{name}-put-status", put_status == 201, {"status": put_status})
        self.check(
            f"{name}-put-no-cache-status",
            "x-cache" not in put_headers,
            {"x-cache": put_headers.get("x-cache", "")},
        )
        self.check(f"{name}-put-body-empty", len(put_body) == 0, {"body_len": len(put_body)})
        self.check(f"{name}-get-status", get_status == 200, {"status": get_status})
        self.check(
            f"{name}-get-hit",
            get_headers.get("x-cache") == "HIT",
            {"x-cache": get_headers.get("x-cache", "")},
        )
        self.check(f"{name}-get-body", get_body == data, {"body_len": len(get_body)})
        self.check(f"{name}-head-status", head_status == 200, {"status": head_status})
        self.check(
            f"{name}-head-hit",
            head_headers.get("x-cache") == "HIT",
            {"x-cache": head_headers.get("x-cache", "")},
        )
        self.check(f"{name}-head-body-empty", len(head_body) == 0, {"body_len": len(head_body)})
        self.check(f"{name}-range-status", range_status == 206, {"status": range_status})
        self.check(
            f"{name}-range-hit",
            range_headers.get("x-cache") == "HIT",
            {"x-cache": range_headers.get("x-cache", "")},
        )
        self.check(f"{name}-range-body", range_body == expected_range_body, {"body_len": len(range_body)})
        self.check(
            f"{name}-origin-gets-flat",
            after_put == before_gets
            and after_get == before_gets
            and after_head == before_gets
            and after_range == before_gets,
            {
                "before": before_gets,
                "after_put": after_put,
                "after_get": after_get,
                "after_head": after_head,
                "after_range": after_range,
            },
        )
        self.check(
            f"{name}-origin-puts-flat",
            after_puts == before_puts,
            {"before": before_puts, "after": after_puts},
        )
        self.check(
            f"{name}-total-origin-traffic-flat",
            after_total_gets == before_total_gets and after_total_puts == before_total_puts,
            {
                "gets_before": before_total_gets,
                "gets_after": after_total_gets,
                "puts_before": before_total_puts,
                "puts_after": after_total_puts,
            },
        )
        self.check(
            f"{name}-cache-bytes-increased",
            record.total_bytes_after == record.total_bytes_before + len(data),
            {
                "before": record.total_bytes_before,
                "after": record.total_bytes_after,
                "body_len": len(data),
            },
        )
        self.check(
            f"{name}-push-warming-recorded",
            record.push_warming_writes_after == record.push_warming_writes_before + 1
            and record.push_warming_bytes_after == record.push_warming_bytes_before + len(data),
            {
                "writes_before": record.push_warming_writes_before,
                "writes_after": record.push_warming_writes_after,
                "bytes_before": record.push_warming_bytes_before,
                "bytes_after": record.push_warming_bytes_after,
                "body_len": len(data),
            },
        )
        self.check(
            f"{name}-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in put_body
            and self.args.cache_psk.encode("utf-8") not in get_body
            and self.args.cache_psk.encode("utf-8") not in range_body,
        )

    def assert_immutable_route_pattern_rejects_poisoning(
        self,
        pattern: str,
        key: str,
        valid_data: bytes,
    ) -> None:
        name = f"route-contract-immutable-poison-{slug(pattern)}"
        self.check(f"{name}-valid-body-present", len(valid_data) > 0, {"body_len": len(valid_data)})
        corrupt_data = bytearray(valid_data)
        corrupt_data[0] ^= 0xFF
        corrupt_body = bytes(corrupt_data)

        evict_status = self.cache_admin_evict_path(key)
        self.check(f"{name}-evict-status", evict_status == 200, {"status": evict_status})

        state = self.require_proxy_state()
        before_stats = self.cache_admin_stats()
        before_gets = state.count_for_key(key)
        before_puts = state.count_put_for_key(key)
        before_total_gets = state.total_get_count()
        before_total_puts = state.total_put_count()

        corrupt_status, corrupt_headers, corrupt_response = self.cache_put(key, corrupt_body)
        after_reject_stats = self.cache_admin_stats()
        after_reject_gets = state.count_for_key(key)
        after_reject_total_gets = state.total_get_count()

        recovery_status, recovery_headers, recovery_body = self.cache_get(key)
        after_recovery_stats = self.cache_admin_stats()
        after_recovery_gets = state.count_for_key(key)
        after_recovery_total_gets = state.total_get_count()

        second_status, second_headers, second_body = self.cache_get(key)
        after_second_gets = state.count_for_key(key)
        after_second_total_gets = state.total_get_count()
        after_puts = state.count_put_for_key(key)
        after_total_puts = state.total_put_count()

        record = ImmutablePoisoningControlRecord(
            name=name,
            pattern=pattern,
            key=key,
            corrupt_status=corrupt_status,
            corrupt_cache_status=corrupt_headers.get("x-cache", ""),
            recovery_status=recovery_status,
            recovery_cache_status=recovery_headers.get("x-cache", ""),
            second_status=second_status,
            second_cache_status=second_headers.get("x-cache", ""),
            evict_status=evict_status,
            origin_gets_before=before_gets,
            origin_gets_after_reject=after_reject_gets,
            origin_gets_after_recovery=after_recovery_gets,
            origin_gets_after_second=after_second_gets,
            origin_puts_before=before_puts,
            origin_puts_after=after_puts,
            total_origin_gets_before=before_total_gets,
            total_origin_gets_after_reject=after_reject_total_gets,
            total_origin_gets_after_recovery=after_recovery_total_gets,
            total_origin_gets_after_second=after_second_total_gets,
            total_origin_puts_before=before_total_puts,
            total_origin_puts_after=after_total_puts,
            total_bytes_before=int(before_stats.get("total_bytes", 0)),
            total_bytes_after_reject=int(after_reject_stats.get("total_bytes", 0)),
            total_bytes_after_recovery=int(after_recovery_stats.get("total_bytes", 0)),
            push_warming_writes_before=self.traffic_value(before_stats, "push_warming_writes"),
            push_warming_writes_after_reject=self.traffic_value(after_reject_stats, "push_warming_writes"),
            push_warming_writes_after_recovery=self.traffic_value(after_recovery_stats, "push_warming_writes"),
            push_warming_bytes_before=self.traffic_value(before_stats, "push_warming_bytes"),
            push_warming_bytes_after_reject=self.traffic_value(after_reject_stats, "push_warming_bytes"),
            push_warming_bytes_after_recovery=self.traffic_value(after_recovery_stats, "push_warming_bytes"),
            valid_body_len=len(valid_data),
            corrupt_body_len=len(corrupt_body),
            corrupt_response_body_len=len(corrupt_response),
            recovery_body_len=len(recovery_body),
            second_body_len=len(second_body),
        )
        self.record_immutable_poisoning_control(record)
        self.record_read(name + "-recovery", key, recovery_status, recovery_headers.get("x-cache", ""), len(recovery_body))
        self.record_read(name + "-second", key, second_status, second_headers.get("x-cache", ""), len(second_body))

        self.check(f"{name}-corrupt-status", corrupt_status == 409, {"status": corrupt_status})
        self.check(
            f"{name}-corrupt-no-cache-status",
            "x-cache" not in corrupt_headers,
            {"x-cache": corrupt_headers.get("x-cache", "")},
        )
        self.check(
            f"{name}-corrupt-no-origin-get",
            after_reject_gets == before_gets,
            {"before": before_gets, "after": after_reject_gets},
        )
        self.check(
            f"{name}-corrupt-no-origin-put",
            after_puts == before_puts,
            {"before": before_puts, "after": after_puts},
        )
        self.check(
            f"{name}-corrupt-total-origin-flat",
            after_reject_total_gets == before_total_gets and after_total_puts == before_total_puts,
            {
                "gets_before": before_total_gets,
                "gets_after_reject": after_reject_total_gets,
                "puts_before": before_total_puts,
                "puts_after": after_total_puts,
            },
        )
        self.check(
            f"{name}-corrupt-cache-bytes-flat",
            record.total_bytes_after_reject == record.total_bytes_before,
            {
                "before": record.total_bytes_before,
                "after_reject": record.total_bytes_after_reject,
            },
        )
        self.check(
            f"{name}-corrupt-push-warming-flat",
            record.push_warming_writes_after_reject == record.push_warming_writes_before
            and record.push_warming_bytes_after_reject == record.push_warming_bytes_before,
            {
                "writes_before": record.push_warming_writes_before,
                "writes_after_reject": record.push_warming_writes_after_reject,
                "bytes_before": record.push_warming_bytes_before,
                "bytes_after_reject": record.push_warming_bytes_after_reject,
            },
        )
        self.check(f"{name}-recovery-status", recovery_status == 200, {"status": recovery_status})
        self.check(
            f"{name}-recovery-miss",
            recovery_headers.get("x-cache") == "MISS",
            {"x-cache": recovery_headers.get("x-cache", "")},
        )
        self.check(f"{name}-recovery-body", recovery_body == valid_data, {"body_len": len(recovery_body)})
        self.check(
            f"{name}-recovery-origin-get-once",
            after_recovery_gets == before_gets + 1 and after_recovery_total_gets == before_total_gets + 1,
            {
                "before": before_gets,
                "after_recovery": after_recovery_gets,
                "total_before": before_total_gets,
                "total_after_recovery": after_recovery_total_gets,
            },
        )
        self.check(
            f"{name}-recovery-push-warming-flat",
            record.push_warming_writes_after_recovery == record.push_warming_writes_before
            and record.push_warming_bytes_after_recovery == record.push_warming_bytes_before,
            {
                "writes_before": record.push_warming_writes_before,
                "writes_after_recovery": record.push_warming_writes_after_recovery,
                "bytes_before": record.push_warming_bytes_before,
                "bytes_after_recovery": record.push_warming_bytes_after_recovery,
            },
        )
        self.check(f"{name}-second-status", second_status == 200, {"status": second_status})
        self.check(
            f"{name}-second-hit",
            second_headers.get("x-cache") == "HIT",
            {"x-cache": second_headers.get("x-cache", "")},
        )
        self.check(f"{name}-second-body", second_body == valid_data, {"body_len": len(second_body)})
        self.check(
            f"{name}-second-origin-flat",
            after_second_gets == after_recovery_gets and after_second_total_gets == after_recovery_total_gets,
            {
                "after_recovery": after_recovery_gets,
                "after_second": after_second_gets,
                "total_after_recovery": after_recovery_total_gets,
                "total_after_second": after_second_total_gets,
            },
        )
        self.check(
            f"{name}-cache-restored-with-valid-body",
            record.total_bytes_after_recovery == record.total_bytes_before + len(valid_data),
            {
                "before": record.total_bytes_before,
                "after_recovery": record.total_bytes_after_recovery,
                "valid_body_len": len(valid_data),
            },
        )
        self.check(
            f"{name}-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in corrupt_response
            and self.args.cache_psk.encode("utf-8") not in recovery_body
            and self.args.cache_psk.encode("utf-8") not in second_body,
        )

    def assert_auth_control(
        self,
        name: str,
        key: str,
        *,
        expected_status: int,
        expected_origin_gets: int,
        psk: str | None = None,
        include_psk: bool = True,
    ) -> None:
        state = self.require_proxy_state()
        before = state.count_for_key(key)
        status, headers, body = self.cache_get(key, psk=psk, include_psk=include_psk)
        after = state.count_for_key(key)
        self.record_auth_control(
            name,
            key,
            status,
            headers.get("x-cache", ""),
            before,
            after,
            len(body),
        )
        self.check(f"{name}-status", status == expected_status, {"status": status})
        self.check(
            f"{name}-origin-gets",
            after == expected_origin_gets,
            {"before": before, "after": after, "expected": expected_origin_gets},
        )
        self.check(
            f"{name}-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in body,
        )

    def verify_enterprise_auth_controls(self) -> None:
        data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:auth")
        key = self.origin_key("auth-allowed", data)
        self.put_origin_object(key, data)
        before = self.require_proxy_state().count_for_key(key)

        self.assert_auth_control(
            "auth-missing-psk-rejected",
            key,
            expected_status=401,
            expected_origin_gets=before,
            include_psk=False,
        )
        self.assert_auth_control(
            "auth-wrong-psk-rejected",
            key,
            expected_status=401,
            expected_origin_gets=before,
            psk="wrong-cache-smoke-psk",
        )
        self.assert_auth_control(
            "auth-valid-psk-accepted",
            key,
            expected_status=200,
            expected_origin_gets=before + 1,
        )

        denied_data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:denied")
        identity = hashlib.sha256(b"denied\0" + denied_data).hexdigest()
        denied_key = (
            f"{REMOTE_PREFIX}-denied/{self.run_id}/direct/forbidden/packs/{identity}.pack"
        )
        self.put_origin_object(denied_key, denied_data)
        denied_before = self.require_proxy_state().count_for_key(denied_key)
        self.assert_auth_control(
            "auth-policy-denies-out-of-scope-read",
            denied_key,
            expected_status=403,
            expected_origin_gets=denied_before,
        )

    def transparent_admin_stats(self, base_url: str, name: str, phase: str) -> dict[str, Any]:
        return self.cache_admin_stats(
            base_url=base_url,
            artifact_name=f"{slug(name)}-{phase}-admin-stats.json",
            artifact_key=f"{slug(name)}_{phase}_admin_stats",
            check_name=f"{name}-{phase}-admin-stats-status",
        )

    def assert_transparent_mutable_control(
        self,
        name: str,
        service_url: str,
        key: str,
        *,
        method: str,
        expected_status: int,
        expected_origin_get_delta: int,
        expected_origin_head_delta: int,
        expected_proxy_read_delta: int,
        expected_body: bytes | None = None,
    ) -> None:
        state = self.require_proxy_state()
        before_stats = self.transparent_admin_stats(service_url, name, "before")
        before_proxy_reads = self.traffic_value(before_stats, "mutable_proxy_reads")
        before_gets = state.count_for_key(key)
        before_heads = state.count_head_for_key(key)
        if method == "GET":
            status, headers, body = self.cache_get(key, base_url=service_url)
        elif method == "HEAD":
            status, headers, body = self.cache_head(key, base_url=service_url)
        else:
            raise SmokeError(f"unsupported transparent mutable probe method: {method}")
        after_stats = self.transparent_admin_stats(service_url, name, "after")
        after_proxy_reads = self.traffic_value(after_stats, "mutable_proxy_reads")
        after_gets = state.count_for_key(key)
        after_heads = state.count_head_for_key(key)
        self.report.transparent_mutable_controls.append(
            asdict(
                TransparentMutableAuthRecord(
                    name=name,
                    key=key,
                    method=method,
                    status=status,
                    origin_gets_before=before_gets,
                    origin_gets_after=after_gets,
                    origin_heads_before=before_heads,
                    origin_heads_after=after_heads,
                    mutable_proxy_reads_before=before_proxy_reads,
                    mutable_proxy_reads_after=after_proxy_reads,
                    body_len=len(body),
                )
            )
        )
        self.write_report()

        self.check(f"{name}-status", status == expected_status, {"status": status})
        self.check(
            f"{name}-origin-gets",
            after_gets == before_gets + expected_origin_get_delta,
            {
                "before": before_gets,
                "after": after_gets,
                "expected_delta": expected_origin_get_delta,
            },
        )
        self.check(
            f"{name}-origin-heads",
            after_heads == before_heads + expected_origin_head_delta,
            {
                "before": before_heads,
                "after": after_heads,
                "expected_delta": expected_origin_head_delta,
            },
        )
        self.check(
            f"{name}-mutable-proxy-reads",
            after_proxy_reads == before_proxy_reads + expected_proxy_read_delta,
            {
                "before": before_proxy_reads,
                "after": after_proxy_reads,
                "expected_delta": expected_proxy_read_delta,
            },
        )
        self.check(
            f"{name}-no-cache-status",
            "x-cache" not in headers,
            {"x-cache": headers.get("x-cache", "")},
        )
        if expected_body is not None:
            self.check(f"{name}-body", body == expected_body, {"body_len": len(body)})
        self.check(
            f"{name}-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in body,
        )

    def verify_transparent_mutable_auth_controls(self) -> None:
        process, service_url = self.start_transparent_cache_server()
        try:
            allowed_key = f"{REMOTE_PREFIX}/{self.run_id}/transparent-auth/manifest"
            allowed_body = deterministic_bytes(4096, f"{self.run_id}:transparent-allowed")
            self.put_origin_object(allowed_key, allowed_body)
            self.assert_transparent_mutable_control(
                "transparent-mutable-allowed-get",
                service_url,
                allowed_key,
                method="GET",
                expected_status=200,
                expected_origin_get_delta=1,
                expected_origin_head_delta=0,
                expected_proxy_read_delta=1,
                expected_body=allowed_body,
            )

            denied_key = f"{REMOTE_PREFIX}-denied/{self.run_id}/transparent-auth/manifest"
            denied_body = deterministic_bytes(4096, f"{self.run_id}:transparent-denied")
            self.put_origin_object(denied_key, denied_body)
            self.assert_transparent_mutable_control(
                "transparent-mutable-denied-get",
                service_url,
                denied_key,
                method="GET",
                expected_status=403,
                expected_origin_get_delta=0,
                expected_origin_head_delta=0,
                expected_proxy_read_delta=0,
            )
            self.assert_transparent_mutable_control(
                "transparent-mutable-denied-head",
                service_url,
                denied_key,
                method="HEAD",
                expected_status=403,
                expected_origin_get_delta=0,
                expected_origin_head_delta=0,
                expected_proxy_read_delta=0,
            )
            self.assert_transparent_mutable_control(
                "transparent-mutable-ambiguous-get",
                service_url,
                "opaque-control-object",
                method="GET",
                expected_status=400,
                expected_origin_get_delta=0,
                expected_origin_head_delta=0,
                expected_proxy_read_delta=0,
            )
            self.check(
                "transparent-cache-server-still-running",
                process.poll() is None,
                {"returncode": process.returncode},
            )
        finally:
            self.terminate_process(process)

    def verify_request_limit_controls(self) -> None:
        state = self.require_proxy_state()
        key = f".crab/xorbs/{'f' * 2}/{'f' * 64}"
        before_stats = self.cache_admin_stats()
        limits = before_stats.get("limits", {})
        max_object_bytes = limits.get("max_object_bytes") if isinstance(limits, dict) else None
        self.check(
            "request-limit-live-max-object-bytes-present",
            isinstance(max_object_bytes, int) and max_object_bytes > 0,
            {"limits": limits},
        )
        max_cache_bytes = limits.get("max_cache_bytes") if isinstance(limits, dict) else None
        self.check(
            "request-limit-live-cache-limit-matches-root",
            max_cache_bytes == before_stats.get("max_bytes"),
            {"limits": limits, "max_bytes": before_stats.get("max_bytes")},
        )
        declared_length = max_object_bytes + 1
        before_origin_gets = state.count_for_key(key)
        before_origin_puts = state.count_put_for_key(key)
        before_total_origin_gets = state.total_get_count()
        before_total_origin_puts = state.total_put_count()

        status, response = self.cache_put_declared_length_without_body(key, declared_length)

        after_stats = self.cache_admin_stats()
        after_origin_gets = state.count_for_key(key)
        after_origin_puts = state.count_put_for_key(key)
        after_total_origin_gets = state.total_get_count()
        after_total_origin_puts = state.total_put_count()
        record = RequestLimitRecord(
            name="oversized-push-warming-rejected-before-body",
            key=key,
            status=status,
            max_object_bytes=max_object_bytes,
            declared_content_length=declared_length,
            body_bytes_sent=0,
            origin_gets_before=before_origin_gets,
            origin_gets_after=after_origin_gets,
            origin_puts_before=before_origin_puts,
            origin_puts_after=after_origin_puts,
            total_origin_gets_before=before_total_origin_gets,
            total_origin_gets_after=after_total_origin_gets,
            total_origin_puts_before=before_total_origin_puts,
            total_origin_puts_after=after_total_origin_puts,
            total_bytes_before=int(before_stats.get("total_bytes", 0)),
            total_bytes_after=int(after_stats.get("total_bytes", 0)),
            xorb_count_before=int(before_stats.get("xorb_count", 0)),
            xorb_count_after=int(after_stats.get("xorb_count", 0)),
            push_warming_writes_before=self.traffic_value(before_stats, "push_warming_writes"),
            push_warming_writes_after=self.traffic_value(after_stats, "push_warming_writes"),
            push_warming_bytes_before=self.traffic_value(before_stats, "push_warming_bytes"),
            push_warming_bytes_after=self.traffic_value(after_stats, "push_warming_bytes"),
        )
        self.report.request_limits.append(asdict(record))
        self.write_report()

        self.check("request-limit-oversized-status", status == 413, {"status": status})
        self.check(
            "request-limit-secret-not-returned",
            self.args.cache_psk.encode("utf-8") not in response,
        )
        self.check(
            "request-limit-no-origin-get-for-key",
            record.origin_gets_after == record.origin_gets_before,
            {"before": record.origin_gets_before, "after": record.origin_gets_after},
        )
        self.check(
            "request-limit-no-origin-put-for-key",
            record.origin_puts_after == record.origin_puts_before,
            {"before": record.origin_puts_before, "after": record.origin_puts_after},
        )
        self.check(
            "request-limit-no-origin-traffic",
            record.total_origin_gets_after == record.total_origin_gets_before
            and record.total_origin_puts_after == record.total_origin_puts_before,
            {
                "gets_before": record.total_origin_gets_before,
                "gets_after": record.total_origin_gets_after,
                "puts_before": record.total_origin_puts_before,
                "puts_after": record.total_origin_puts_after,
            },
        )
        self.check(
            "request-limit-total-bytes-unchanged",
            record.total_bytes_after == record.total_bytes_before,
            {"before": record.total_bytes_before, "after": record.total_bytes_after},
        )
        self.check(
            "request-limit-xorb-count-unchanged",
            record.xorb_count_after == record.xorb_count_before,
            {"before": record.xorb_count_before, "after": record.xorb_count_after},
        )
        self.check(
            "request-limit-push-warming-counters-unchanged",
            record.push_warming_writes_after == record.push_warming_writes_before
            and record.push_warming_bytes_after == record.push_warming_bytes_before,
            {
                "writes_before": record.push_warming_writes_before,
                "writes_after": record.push_warming_writes_after,
                "bytes_before": record.push_warming_bytes_before,
                "bytes_after": record.push_warming_bytes_after,
            },
        )

    def mutable_route_key(self, pattern: str) -> str:
        prefix = f"{REMOTE_PREFIX}/{self.run_id}/route-contract-mutable"
        hash_hex = "1" * 64
        return {
            "{repo}/refs/heads/*": f"{prefix}/refs/heads/main",
            "{repo}/HEAD": f"{prefix}/HEAD",
            "{repo}/locks/*": f"{prefix}/locks/push-main",
            "{repo}/packs/pack-{id}.meta": f"{prefix}/packs/pack-not-cacheable.meta",
            "{repo}/manifests/*": f"{prefix}/manifests/pack-list-cafebabe",
            "{repo}/pack-list": f"{prefix}/pack-list",
            "{repo}/shard-list": f"{prefix}/shard-list",
            ".crab/ref-registry/*": f".crab/ref-registry/records/11/{self.run_id}.json",
            "{repo}/file_index_db/manifest/current": f"{prefix}/file_index_db/manifest/current",
            ".crab/chunk_index_db/manifest/current": ".crab/chunk_index_db/manifest/current",
        }[pattern]

    def synthetic_immutable_route_specs(self) -> list[tuple[str, str, bytes]]:
        prefix = f"{REMOTE_PREFIX}/{self.run_id}/route-contract-immutable"
        generated_hash = "9" * 64
        specs = [
            ("{repo}/packs/pack-{id}.pack", f"{prefix}/packs/pack-route-contract.pack"),
            ("{repo}/packs/pack-{id}.idx", f"{prefix}/packs/pack-route-contract.idx"),
            (
                "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
                f"{prefix}/generated-packs/v1/artifacts/99/{generated_hash}.pack",
            ),
            (
                "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
                f"{prefix}/generated-packs/v1/requests/99/{generated_hash}.json",
            ),
            (
                "{repo}/file_index_db/compacted/*.sst",
                f"{prefix}/file_index_db/compacted/00000000000000000001.sst",
            ),
            (
                "{repo}/file_index_db/manifest/*.manifest",
                f"{prefix}/file_index_db/manifest/00000000000000000002.manifest",
            ),
            (
                "{repo}/file_index_db/wal/*.sst",
                f"{prefix}/file_index_db/wal/00000000000000000003.sst",
            ),
            (
                "{repo}/file_index_db/compactions/*.compactions",
                f"{prefix}/file_index_db/compactions/00000000000000000004.compactions",
            ),
        ]
        return [
            (pattern, key, deterministic_bytes(4096, f"{self.run_id}:{pattern}"))
            for pattern, key in specs
        ]

    def origin_object_matching(self, pattern: str, predicate: Any) -> tuple[str, bytes]:
        state = self.require_proxy_state()
        keys = sorted(
            (key for key in state.put_counts_snapshot() if predicate(key)), reverse=True
        )
        name = f"route-contract-origin-key-{slug(pattern)}"
        self.check(name, bool(keys), {"matches": keys[:5]})
        for key in keys:
            body = self.get_origin_object(key)
            # SlateDB also uploads empty WAL fencing markers. Exercise range
            # reads using a real non-empty table rather than replacing a marker.
            if len(body) > 3:
                return key, body
        raise SmokeError(f"no non-empty origin object produced by this run for {pattern}")

    def verify_advertised_mutable_route_contract_behavior(self) -> None:
        before_stats = self.cache_admin_stats()
        before_read_rejections = self.traffic_value(before_stats, "mutable_read_rejections")
        before_write_rejections = self.traffic_value(before_stats, "mutable_write_rejections")
        before_proxy_reads = self.traffic_value(before_stats, "mutable_proxy_reads")
        for pattern in EXPECTED_MUTABLE_ROUTE_PATTERNS:
            self.assert_mutable_route_pattern_rejected(pattern, self.mutable_route_key(pattern))
        after_read_stats = self.cache_admin_stats()
        expected_delta = len(EXPECTED_MUTABLE_ROUTE_PATTERNS)
        self.check(
            "route-contract-mutable-read-rejections",
            self.traffic_value(after_read_stats, "mutable_read_rejections")
            == before_read_rejections + expected_delta,
            {
                "before": before_read_rejections,
                "after": self.traffic_value(after_read_stats, "mutable_read_rejections"),
                "expected_delta": expected_delta,
            },
        )
        self.check(
            "route-contract-mutable-read-phase-write-rejections-unchanged",
            self.traffic_value(after_read_stats, "mutable_write_rejections")
            == before_write_rejections,
            {
                "before": before_write_rejections,
                "after": self.traffic_value(after_read_stats, "mutable_write_rejections"),
            },
        )
        self.check(
            "route-contract-mutable-read-phase-proxy-reads-unchanged",
            self.traffic_value(after_read_stats, "mutable_proxy_reads") == before_proxy_reads,
            {
                "before": before_proxy_reads,
                "after": self.traffic_value(after_read_stats, "mutable_proxy_reads"),
            },
        )
        self.check(
            "route-contract-mutable-patterns-covered",
            sorted(record["pattern"] for record in self.report.mutable_route_behaviors)
            == sorted(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {"patterns": [record["pattern"] for record in self.report.mutable_route_behaviors]},
        )
        for pattern in EXPECTED_MUTABLE_ROUTE_PATTERNS:
            self.assert_mutable_route_pattern_write_rejected(pattern, self.mutable_route_key(pattern))
        after_write_stats = self.cache_admin_stats()
        self.check(
            "route-contract-mutable-write-rejections",
            self.traffic_value(after_write_stats, "mutable_write_rejections")
            == before_write_rejections + expected_delta,
            {
                "before": before_write_rejections,
                "after": self.traffic_value(after_write_stats, "mutable_write_rejections"),
                "expected_delta": expected_delta,
            },
        )
        self.check(
            "route-contract-mutable-write-phase-read-rejections-unchanged",
            self.traffic_value(after_write_stats, "mutable_read_rejections")
            == before_read_rejections + expected_delta,
            {
                "before": before_read_rejections,
                "after": self.traffic_value(after_write_stats, "mutable_read_rejections"),
            },
        )
        self.check(
            "route-contract-mutable-write-phase-proxy-reads-unchanged",
            self.traffic_value(after_write_stats, "mutable_proxy_reads") == before_proxy_reads,
            {
                "before": before_proxy_reads,
                "after": self.traffic_value(after_write_stats, "mutable_proxy_reads"),
            },
        )
        self.check(
            "route-contract-mutable-write-patterns-covered",
            sorted(record["pattern"] for record in self.report.mutable_route_write_behaviors)
            == sorted(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {"patterns": [record["pattern"] for record in self.report.mutable_route_write_behaviors]},
        )

    def verify_advertised_immutable_route_contract_behavior(self) -> None:
        xorb_key, xorb_body = self.origin_object_matching(
            ".crab/xorbs/{first-two-hex}/{hash}",
            lambda key: key.startswith(".crab/xorbs/"),
        )
        shard_key, shard_body = self.origin_object_matching(
            ".crab/shards/{first-two-hex}/{hash}",
            lambda key: key.startswith(".crab/shards/"),
        )
        synthetic_specs = self.synthetic_immutable_route_specs()
        real_specs = [
            (".crab/xorbs/{first-two-hex}/{hash}", xorb_key, xorb_body),
            (".crab/shards/{first-two-hex}/{hash}", shard_key, shard_body),
        ]
        for family, extension in (
            ("compacted", "sst"), ("manifest", "manifest"),
            ("wal", "sst"), ("compactions", "compactions"),
        ):
            prefix = f".crab/chunk_index_db/{family}/"
            suffix = f".{extension}"
            pattern = f"{prefix}*{suffix}"
            key, body = self.origin_object_matching(
                pattern, lambda key: key.startswith(prefix) and key.endswith(suffix)
            )
            real_specs.append((pattern, key, body))

        for pattern, key, _body in real_specs:
            self.assert_immutable_route_pattern_cached(pattern, key)
        for pattern, key, data in synthetic_specs:
            self.assert_immutable_route_pattern_cached(pattern, key, data)
        self.check(
            "route-contract-immutable-patterns-covered",
            sorted(record["pattern"] for record in self.report.immutable_route_behaviors)
            == sorted(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {"patterns": [record["pattern"] for record in self.report.immutable_route_behaviors]},
        )
        for pattern, key, data in real_specs + synthetic_specs:
            self.assert_immutable_route_pattern_push_warmed(pattern, key, data)
        self.check(
            "route-contract-immutable-write-patterns-covered",
            sorted(record["pattern"] for record in self.report.immutable_route_write_behaviors)
            == sorted(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {"patterns": [record["pattern"] for record in self.report.immutable_route_write_behaviors]},
        )
        self.assert_immutable_route_pattern_rejects_poisoning(
            ".crab/xorbs/{first-two-hex}/{hash}", xorb_key, xorb_body
        )
        self.assert_immutable_route_pattern_rejects_poisoning(
            ".crab/shards/{first-two-hex}/{hash}", shard_key, shard_body
        )
        self.check(
            "route-contract-immutable-poisoning-patterns-covered",
            sorted(record["pattern"] for record in self.report.immutable_poisoning_controls)
            == [
                ".crab/shards/{first-two-hex}/{hash}",
                ".crab/xorbs/{first-two-hex}/{hash}",
            ],
            {"patterns": [record["pattern"] for record in self.report.immutable_poisoning_controls]},
        )

    def verify_full_and_warm_range_reads(self) -> None:
        state = self.require_proxy_state()
        data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:full")
        key = self.origin_key("full", data)
        self.put_origin_object(key, data)
        before = state.count_for_key(key)
        self.assert_cache_read(
            "full-first-miss",
            key,
            data,
            expected_status=200,
            expected_cache="MISS",
            expected_origin_gets=before + 1,
        )
        self.assert_cache_read(
            "full-second-hit",
            key,
            data,
            expected_status=200,
            expected_cache="HIT",
            expected_origin_gets=before + 1,
        )
        head_before = state.count_head_for_key(key)
        self.assert_cache_head(
            "warm-head-hit",
            key,
            expected_status=200,
            expected_cache="HIT",
            expected_content_length=len(data),
            expected_origin_gets=before + 1,
            expected_origin_heads=head_before,
        )
        self.assert_cache_read(
            "warm-range-hit",
            key,
            data,
            expected_status=206,
            expected_cache="HIT",
            expected_origin_gets=before + 1,
            byte_range="bytes=10-30",
            expected_body=data[10:31],
        )
        clipped_start = len(data) - 11
        self.assert_cache_read(
            "warm-range-clipped-hit",
            key,
            data,
            expected_status=206,
            expected_cache="HIT",
            expected_origin_gets=before + 1,
            byte_range=f"bytes={clipped_start}-{len(data) + 100}",
            expected_body=data[clipped_start:],
            expected_content_range=f"bytes {clipped_start}-{len(data) - 1}/{len(data)}",
        )
        self.assert_malformed_range_rejected("malformed-range-rejected", key)

    def verify_cold_range_reads(self) -> None:
        state = self.require_proxy_state()
        head_data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:cold-head")
        head_key = self.origin_key("cold-head", head_data)
        self.put_origin_object(head_key, head_data)
        head_before_gets = state.count_for_key(head_key)
        head_before_heads = state.count_head_for_key(head_key)
        self.assert_cache_head(
            "cold-head-miss",
            head_key,
            expected_status=200,
            expected_cache="MISS",
            expected_content_length=len(head_data),
            expected_origin_gets=head_before_gets,
            expected_origin_heads=head_before_heads + 1,
        )

        data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:cold-range")
        key = self.origin_key("cold-range", data)
        self.put_origin_object(key, data)
        before = state.count_for_key(key)
        self.assert_cache_read(
            "cold-range-first-miss",
            key,
            data,
            expected_status=206,
            expected_cache="MISS",
            expected_origin_gets=before + 1,
            byte_range="bytes=7-18",
            expected_body=data[7:19],
        )
        self.assert_cache_read(
            "cold-range-second-hit",
            key,
            data,
            expected_status=206,
            expected_cache="HIT",
            expected_origin_gets=before + 1,
            byte_range="bytes=20-41",
            expected_body=data[20:42],
        )

    def verify_concurrent_cold_misses(self) -> None:
        state = self.require_proxy_state()
        data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:concurrent")
        key = self.origin_key("concurrent", data)
        self.put_origin_object(key, data)
        before = state.count_for_key(key)
        state.delay_next_get(key, self.args.origin_delay)

        results: dict[str, tuple[int, dict[str, str], bytes]] = {}
        errors: dict[str, BaseException] = {}

        def worker(name: str, byte_range: str) -> None:
            try:
                results[name] = self.cache_get(key, byte_range=byte_range)
            except BaseException as exc:
                errors[name] = exc

        first = threading.Thread(target=worker, args=("first", "bytes=0-31"), daemon=True)
        second = threading.Thread(target=worker, args=("second", "bytes=32-63"), daemon=True)
        first.start()
        saw_first_origin = state.wait_for_count(key, before + 1, self.args.startup_timeout)
        self.check("concurrent-first-origin-get-started", saw_first_origin)
        second.start()
        time.sleep(self.args.concurrent_probe_delay)
        self.check(
            "concurrent-origin-get-coalesced-while-in-flight",
            state.count_for_key(key) == before + 1,
            {"origin_gets": state.count_for_key(key), "expected": before + 1},
        )

        first.join(self.args.timeout)
        second.join(self.args.timeout)
        self.check("concurrent-requests-finished", not first.is_alive() and not second.is_alive())
        if errors:
            raise SmokeError(f"concurrent request failed: {errors}")

        first_status, first_headers, first_body = results["first"]
        second_status, second_headers, second_body = results["second"]
        self.record_read(
            "concurrent-first",
            key,
            first_status,
            first_headers.get("x-cache", ""),
            len(first_body),
        )
        self.record_read(
            "concurrent-second",
            key,
            second_status,
            second_headers.get("x-cache", ""),
            len(second_body),
        )
        self.check("concurrent-first-status", first_status == 206, {"status": first_status})
        self.check("concurrent-second-status", second_status == 206, {"status": second_status})
        self.check("concurrent-first-body", first_body == data[0:32])
        self.check("concurrent-second-body", second_body == data[32:64])
        cache_statuses = sorted(
            [first_headers.get("x-cache", ""), second_headers.get("x-cache", "")]
        )
        self.check(
            "concurrent-cache-statuses",
            cache_statuses == ["HIT", "MISS"],
            {"statuses": cache_statuses},
        )
        self.check(
            "concurrent-origin-get-total",
            state.count_for_key(key) == before + 1,
            {"origin_gets": state.count_for_key(key), "expected": before + 1},
        )

    def verify_cache_pressure_keeps_recent_hot_object_warm(self) -> None:
        state = self.require_proxy_state()
        before_stats = self.cache_admin_stats()
        max_bytes = int(before_stats.get("max_bytes", self.args.max_cache_bytes))
        total_before = int(before_stats.get("total_bytes", 0))
        evictions_before = self.eviction_total(before_stats)
        if max_bytes < 256 * 1024:
            self.check(
                "cache-pressure-skipped-budget-too-small",
                True,
                {"max_bytes": max_bytes},
            )
            return

        object_bytes = max(64 * 1024, min(max_bytes // 8, 8 * 1024 * 1024))
        high_water = int(max_bytes * 0.95)
        bytes_needed = max(0, high_water - total_before)
        pressure_objects = max(3, bytes_needed // object_bytes + 2)
        if pressure_objects > 16:
            self.check(
                "cache-pressure-skipped-budget-too-large",
                True,
                {
                    "max_bytes": max_bytes,
                    "object_bytes": object_bytes,
                    "pressure_objects": pressure_objects,
                },
            )
            return

        hot_data = deterministic_bytes(object_bytes, f"{self.run_id}:pressure-hot")
        hot_key = self.origin_key("pressure-hot", hot_data)
        self.put_origin_object(hot_key, hot_data)
        hot_origin_before = state.count_for_key(hot_key)
        self.assert_cache_read(
            "cache-pressure-hot-first-miss",
            hot_key,
            hot_data,
            expected_status=200,
            expected_cache="MISS",
            expected_origin_gets=hot_origin_before + 1,
        )
        hot_origin_after_miss = state.count_for_key(hot_key)
        self.assert_cache_read(
            "cache-pressure-hot-warm-hit",
            hot_key,
            hot_data,
            expected_status=200,
            expected_cache="HIT",
            expected_origin_gets=hot_origin_after_miss,
        )

        for index in range(pressure_objects):
            time.sleep(0.01)
            self.assert_cache_read(
                f"cache-pressure-hot-touch-{index}",
                hot_key,
                hot_data,
                expected_status=200,
                expected_cache="HIT",
                expected_origin_gets=hot_origin_after_miss,
            )
            pressure_data = deterministic_bytes(
                object_bytes,
                f"{self.run_id}:pressure:{index}",
            )
            pressure_key = self.origin_key(f"pressure-{index}", pressure_data)
            pressure_origin_before = state.count_for_key(pressure_key)
            self.put_origin_object(pressure_key, pressure_data)
            self.assert_cache_read(
                f"cache-pressure-fill-{index}",
                pressure_key,
                pressure_data,
                expected_status=200,
                expected_cache="MISS",
                expected_origin_gets=pressure_origin_before + 1,
            )

        after_stats = self.cache_admin_stats()
        total_after = int(after_stats.get("total_bytes", 0))
        evictions_after = self.eviction_total(after_stats)
        expected_without_eviction = total_before + object_bytes * (pressure_objects + 1)
        hot_origin_after = state.count_for_key(hot_key)
        self.report.cache_pressure.append(
            asdict(
                CachePressureRecord(
                    name="cache-pressure",
                    object_bytes=object_bytes,
                    pressure_objects=pressure_objects,
                    total_bytes_before=total_before,
                    total_bytes_after=total_after,
                    max_bytes=max_bytes,
                    hot_origin_gets_before=hot_origin_after_miss,
                    hot_origin_gets_after=hot_origin_after,
                    expected_bytes_without_eviction=expected_without_eviction,
                    evictions_before=evictions_before,
                    evictions_after=evictions_after,
                )
            )
        )
        self.write_report()

        self.check(
            "cache-pressure-stayed-within-budget",
            total_after <= max_bytes,
            {"total_bytes": total_after, "max_bytes": max_bytes},
        )
        self.check(
            "cache-pressure-evicted-or-skipped-cold-objects",
            total_after < expected_without_eviction,
            {
                "total_bytes": total_after,
                "expected_without_eviction": expected_without_eviction,
            },
        )
        self.check(
            "cache-pressure-eviction-count-increased",
            evictions_after > evictions_before,
            {"before": evictions_before, "after": evictions_after},
        )
        self.check(
            "cache-pressure-hot-object-stayed-warm",
            hot_origin_after == hot_origin_after_miss,
            {
                "hot_origin_gets_after_miss": hot_origin_after_miss,
                "hot_origin_gets_after_pressure": hot_origin_after,
            },
        )

    def verify_admin_traffic_stats(self) -> None:
        stats = self.cache_admin_stats()
        traffic = stats.get("traffic", {})
        object_bytes = self.args.object_kib * 1024
        expected = {
            "cache_hits": 6,
            "cache_misses": 4,
            "origin_avoided_reads": 6,
            "coalesced_misses": 1,
            "origin_fetches": 3,
            "origin_head_requests": 1,
            "origin_fetch_bytes": object_bytes * 3,
            "bytes_served_from_cache": object_bytes + 86,
            "bytes_served_from_origin": object_bytes + 44,
            "inflight_misses": 0,
            "push_warming_writes": 0,
            "dedup_queries": 0,
            "mutable_read_rejections": len(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            "mutable_write_rejections": len(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            "mutable_proxy_reads": 0,
        }
        for name, want in expected.items():
            actual = traffic.get(name)
            self.check(
                f"admin-traffic-{name}",
                actual == want,
                {"actual": actual, "expected": want},
            )

        for name, want in expected.items():
            if name in {
                "dedup_queries",
                "push_warming_writes",
                "inflight_misses",
                "mutable_read_rejections",
                "mutable_write_rejections",
                "mutable_proxy_reads",
            }:
                continue
            actual = self.object_traffic_value(stats, "pack", name)
            self.check(
                f"admin-traffic-pack-{name}",
                actual == want,
                {"actual": actual, "expected": want},
            )

        for object_type in ("xorb", "shard", "pack_index", "metadata"):
            actual = self.object_traffic_value(stats, object_type, "cache_hits")
            self.check(
                f"admin-traffic-{object_type}-initially-zero",
                actual == 0,
                {"actual": actual},
            )

        dedup_index = stats.get("dedup_index", {})
        self.check(
            "admin-dedup-index-initial-count",
            dedup_index.get("indexed_chunks") == 0,
            {"actual": dedup_index.get("indexed_chunks")},
        )
        self.check(
            "admin-dedup-index-scope",
            dedup_index.get("scope") == "all",
            {"actual": dedup_index.get("scope")},
        )
        self.check(
            "admin-dedup-index-requires-repo-context",
            dedup_index.get("requires_repo_context") is False,
            {"actual": dedup_index.get("requires_repo_context")},
        )
        startup_rebuild = dedup_index.get("startup_rebuild", {})
        self.check(
            "admin-dedup-index-startup-rebuild-ok",
            startup_rebuild.get("status") == "ok" and startup_rebuild.get("error") is None,
            {"startup_rebuild": startup_rebuild},
        )
        self.check(
            "admin-dedup-index-last-ingestion-error-empty",
            dedup_index.get("last_ingestion_error") is None,
            {"actual": dedup_index.get("last_ingestion_error")},
        )

    @staticmethod
    def traffic_value(stats: dict[str, Any], name: str) -> int:
        value = stats.get("traffic", {}).get(name)
        return int(value) if isinstance(value, int) else 0

    @staticmethod
    def eviction_total(stats: dict[str, Any]) -> int:
        value = stats.get("eviction", {}).get("total")
        return int(value) if isinstance(value, int) else 0

    @staticmethod
    def object_traffic_value(stats: dict[str, Any], object_type: str, name: str) -> int:
        value = (
            stats.get("traffic", {})
            .get("by_object_type", {})
            .get(object_type, {})
            .get(name)
        )
        return int(value) if isinstance(value, int) else 0

    @staticmethod
    def integrity_value(stats: dict[str, Any], phase: str, name: str) -> int:
        value = stats.get(phase, {}).get(name)
        return int(value) if isinstance(value, int) else 0

    @classmethod
    def startup_integrity_repair_total(cls, stats: dict[str, Any]) -> int:
        return sum(
            cls.integrity_value(stats, "startup_integrity", name)
            for name in (
                "metadata_entries_removed",
                "metadata_size_corrections",
                "unindexed_objects_indexed",
                "unindexed_paths_removed",
            )
        )

    def configure_git_identity(self, repo: Path, who: str) -> None:
        self.run_cmd(
            f"{who} git config user name",
            ["git", "config", "user.name", f"Crab {who}"],
            repo,
        )
        self.run_cmd(
            f"{who} git config user email",
            ["git", "config", "user.email", f"{who}@example.invalid"],
            repo,
        )

    def prepare_cli_source_repo(self) -> tuple[Path, str, bytes, str]:
        env = self.client_env("cli-source-cache")
        repo = self.run_root / "cli-source"
        repo.mkdir(parents=True, exist_ok=True)
        remote_url = f"crab://{self.args.bucket}/{REMOTE_PREFIX}/{self.run_id}/cli-hydrate"
        self.run_cmd("cli source git init", ["git", "init", "-b", "main"], repo, env=env)
        self.configure_git_identity(repo, "cli-source")
        self.run_cmd(
            "cli source crab init",
            [self.crab_bin, "init", remote_url],
            repo,
            env=env,
        )
        self.configure_repo_cache_service(repo, env=env)
        self.run_cmd("cli source crab track", [self.crab_bin, "track", "*.bin"], repo, env=env)
        tracked_config = [
            name for name in ("crab.toml", ".gitattributes") if (repo / name).exists()
        ]
        if tracked_config:
            self.run_cmd(
                "cli source git add tracking config",
                ["git", "add", *tracked_config],
                repo,
                env=env,
            )

        data = deterministic_bytes(self.args.cli_file_kib * 1024, f"{self.run_id}:cli-model")
        model = repo / "model.bin"
        model.write_bytes(data)
        expected_sha = hashlib.sha256(data).hexdigest()
        self.run_cmd(
            "cli source crab add model",
            [self.crab_bin, "add", "--jobs", "0", "model.bin"],
            repo,
            env=env,
            timeout=self.args.push_timeout,
        )
        self.run_cmd(
            "cli source git commit",
            ["git", "commit", "-m", "cache-service cli hydrate proof"],
            repo,
            env=env,
        )
        self.run_cmd(
            "cli source crab push",
            [
                self.crab_bin,
                "push",
                "--json",
                "origin",
                "HEAD:refs/heads/main",
            ],
            repo,
            env=env,
            timeout=self.args.push_timeout,
        )
        return repo, remote_url, data, expected_sha

    @staticmethod
    def xorb_put_delta(before: dict[str, int], after: dict[str, int]) -> int:
        keys = set(before) | set(after)
        return sum(
            after.get(key, 0) - before.get(key, 0)
            for key in keys
            if key.startswith(".crab/xorbs/")
        )

    @staticmethod
    def xorb_get_delta(before: dict[str, int], after: dict[str, int]) -> int:
        keys = set(before) | set(after)
        return sum(
            after.get(key, 0) - before.get(key, 0)
            for key in keys
            if key.startswith(".crab/xorbs/")
        )

    @staticmethod
    def shard_get_delta(before: dict[str, int], after: dict[str, int]) -> int:
        keys = set(before) | set(after)
        return sum(
            after.get(key, 0) - before.get(key, 0)
            for key in keys
            if key.startswith(".crab/shards/")
        )

    @staticmethod
    def metadata_get_delta(before: dict[str, int], after: dict[str, int]) -> int:
        keys = set(before) | set(after)
        return sum(
            after.get(key, 0) - before.get(key, 0)
            for key in keys
            if CacheServiceRustfsSmoke.is_versioned_metadb_key(key)
        )

    @staticmethod
    def positive_get_delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
        return {
            key: after.get(key, 0) - before.get(key, 0)
            for key in sorted(set(before) | set(after))
            if after.get(key, 0) - before.get(key, 0) > 0
        }

    @staticmethod
    def is_cacheable_origin_key(key: str) -> bool:
        if key.startswith(".crab/xorbs/") or key.startswith(".crab/shards/"):
            return True
        if "/packs/" in key and (key.endswith(".pack") or key.endswith(".idx")):
            return True
        return CacheServiceRustfsSmoke.is_versioned_metadb_key(key)

    @staticmethod
    def is_versioned_metadb_key(key: str) -> bool:
        if "/file_index_db/" not in key and "/chunk_index_db/" not in key:
            return False
        return (
            ("/wal/" in key and key.endswith(".sst"))
            or ("/compacted/" in key and key.endswith(".sst"))
            or ("/manifest/" in key and key.endswith(".manifest"))
            or ("/compactions/" in key and key.endswith(".compactions"))
        )

    def push_duplicate_repo_uses_cache_service_dedup(self, data: bytes) -> CliPushDedupRecord:
        state = self.require_proxy_state()
        env = self.client_env("cli-dedup-cache")
        repo = self.run_root / "cli-dedup-source"
        repo.mkdir(parents=True, exist_ok=True)
        remote_url = f"crab://{self.args.bucket}/{REMOTE_PREFIX}/{self.run_id}/cli-dedup"
        self.run_cmd("cli dedup git init", ["git", "init", "-b", "main"], repo, env=env)
        self.configure_git_identity(repo, "cli-dedup")
        self.run_cmd(
            "cli dedup crab init",
            [self.crab_bin, "init", remote_url],
            repo,
            env=env,
        )
        self.configure_repo_cache_service(repo, env=env)
        self.run_cmd("cli dedup crab track", [self.crab_bin, "track", "*.bin"], repo, env=env)
        tracked_config = [
            name for name in ("crab.toml", ".gitattributes") if (repo / name).exists()
        ]
        if tracked_config:
            self.run_cmd(
                "cli dedup git add tracking config",
                ["git", "add", *tracked_config],
                repo,
                env=env,
            )

        (repo / "model.bin").write_bytes(data)
        # Deduplication now happens while `crab add` prepares the staged xorb.
        # Measure the complete add-to-push pipeline so this qualification proves
        # both the cache-service classification and push-time proof reuse.
        before_stats = self.cache_admin_stats()
        before_gets = state.total_get_count()
        before_get_counts = state.counts_snapshot()
        before_puts = state.total_put_count()
        before_put_counts = state.put_counts_snapshot()
        self.run_cmd(
            "cli dedup crab add model",
            [self.crab_bin, "add", "--jobs", "0", "model.bin"],
            repo,
            env=env,
            timeout=self.args.push_timeout,
        )
        self.run_cmd(
            "cli dedup git commit",
            ["git", "commit", "-m", "cache-service dedup proof"],
            repo,
            env=env,
        )

        self.run_cmd(
            "cli dedup crab push",
            [
                self.crab_bin,
                "push",
                "--json",
                "origin",
                "HEAD:refs/heads/main",
            ],
            repo,
            env=env,
            timeout=self.args.push_timeout,
        )
        after_stats = self.cache_admin_stats()
        after_gets = state.total_get_count()
        after_get_counts = state.counts_snapshot()
        after_puts = state.total_put_count()
        after_put_counts = state.put_counts_snapshot()
        origin_key_delta = self.positive_get_delta(before_get_counts, after_get_counts)
        cacheable_origin_key_delta = {
            key: delta
            for key, delta in origin_key_delta.items()
            if self.is_cacheable_origin_key(key)
        }
        mutable_origin_key_delta = {
            key: delta
            for key, delta in origin_key_delta.items()
            if key not in cacheable_origin_key_delta
        }

        record = CliPushDedupRecord(
            name="cli-dedup-push",
            dedup_queries_delta=self.traffic_value(after_stats, "dedup_queries")
            - self.traffic_value(before_stats, "dedup_queries"),
            dedup_known_chunks_delta=self.traffic_value(after_stats, "dedup_known_chunks")
            - self.traffic_value(before_stats, "dedup_known_chunks"),
            dedup_unknown_chunks_delta=self.traffic_value(after_stats, "dedup_unknown_chunks")
            - self.traffic_value(before_stats, "dedup_unknown_chunks"),
            xorb_gets_delta=self.xorb_get_delta(before_get_counts, after_get_counts),
            shard_gets_delta=self.shard_get_delta(before_get_counts, after_get_counts),
            metadata_gets_delta=self.metadata_get_delta(before_get_counts, after_get_counts),
            xorb_puts_delta=self.xorb_put_delta(before_put_counts, after_put_counts),
            total_puts_delta=after_puts - before_puts,
            origin_gets_delta=after_gets - before_gets,
            origin_get_key_delta=origin_key_delta,
            cacheable_origin_gets_delta=sum(cacheable_origin_key_delta.values()),
            cacheable_origin_get_key_delta=cacheable_origin_key_delta,
            mutable_origin_gets_delta=sum(mutable_origin_key_delta.values()),
            mutable_origin_get_key_delta=mutable_origin_key_delta,
            mutable_read_rejections_delta=self.traffic_value(
                after_stats, "mutable_read_rejections"
            )
            - self.traffic_value(before_stats, "mutable_read_rejections"),
            mutable_write_rejections_delta=self.traffic_value(
                after_stats, "mutable_write_rejections"
            )
            - self.traffic_value(before_stats, "mutable_write_rejections"),
        )
        self.report.cli_push_dedup.append(asdict(record))
        self.write_report()

        self.check(
            "cli-dedup-add-push-advisory-query-bypassed",
            record.dedup_queries_delta == 0,
            {"delta": record.dedup_queries_delta},
        )
        self.check(
            "cli-dedup-add-push-advisory-results-empty",
            record.dedup_known_chunks_delta == 0 and record.dedup_unknown_chunks_delta == 0,
            {
                "known_delta": record.dedup_known_chunks_delta,
                "unknown_delta": record.dedup_unknown_chunks_delta,
            },
        )
        self.check(
            "cli-dedup-push-skipped-xorb-put",
            record.xorb_puts_delta == 0,
            {
                "xorb_puts_delta": record.xorb_puts_delta,
                "total_puts_delta": record.total_puts_delta,
            },
        )
        self.check(
            "cli-dedup-push-canonical-xorb-proof",
            record.xorb_gets_delta > 0,
            {
                "xorb_gets_delta": record.xorb_gets_delta,
                "origin_gets_delta": record.origin_gets_delta,
                "key_delta": record.origin_get_key_delta,
            },
        )
        self.check(
            "cli-dedup-push-canonical-shard-proof",
            record.shard_gets_delta > 0,
            {
                "shard_gets_delta": record.shard_gets_delta,
                "origin_gets_delta": record.origin_gets_delta,
                "key_delta": record.origin_get_key_delta,
            },
        )
        self.check(
            "cli-dedup-push-metadata-reads-allowed",
            record.metadata_gets_delta > 0,
            {
                "metadata_gets_delta": record.metadata_gets_delta,
                "origin_gets_delta": record.origin_gets_delta,
                "key_delta": record.origin_get_key_delta,
            },
        )
        cacheable_keys = record.cacheable_origin_get_key_delta
        self.check(
            "cli-dedup-push-cacheable-origin-proof",
            record.cacheable_origin_gets_delta > 0
            and any(key.startswith(".crab/xorbs/") for key in cacheable_keys)
            and any(key.startswith(".crab/shards/") for key in cacheable_keys),
            {
                "cacheable_origin_gets_delta": record.cacheable_origin_gets_delta,
                "cacheable_origin_get_key_delta": record.cacheable_origin_get_key_delta,
                "origin_get_key_delta": record.origin_get_key_delta,
            },
        )
        self.check(
            "cli-dedup-push-cache-service-mutable-rejections-flat",
            record.mutable_read_rejections_delta == 0
            and record.mutable_write_rejections_delta == 0,
            {
                "mutable_read_rejections_delta": record.mutable_read_rejections_delta,
                "mutable_write_rejections_delta": record.mutable_write_rejections_delta,
            },
        )
        self.check(
            "cli-dedup-push-retired-commit-graph-summary-unused",
            not any(key.endswith("/commit-graph-summary") for key in record.origin_get_key_delta),
            {"key_delta": record.origin_get_key_delta},
        )
        self.check(
            "cli-dedup-push-origin-lock-acquired",
            any("/locks/" in key for key in record.origin_get_key_delta),
            {"key_delta": record.origin_get_key_delta},
        )
        manifest_key = f"{REMOTE_PREFIX}/{self.run_id}/cli-dedup/manifest"
        self.check(
            "cli-dedup-push-manifest-cas-read",
            record.mutable_origin_get_key_delta.get(manifest_key, 0) > 0,
            {
                "expected_key": manifest_key,
                "actual": record.origin_get_key_delta,
                "mutable_actual": record.mutable_origin_get_key_delta,
            },
        )
        return record

    def clone_configure_and_hydrate(
        self,
        name: str,
        remote_url: str,
        expected_sha: str,
        *,
        cache_name: str,
        expect_origin_gets: bool,
    ) -> CliHydrateRecord:
        state = self.require_proxy_state()
        env = self.client_env(cache_name)
        clone_dir = self.run_root / name
        self.run_cmd(
            f"{name} crab clone lazy",
            [self.crab_bin, "clone", remote_url, str(clone_dir), "--jsonl"],
            self.run_root,
            env=env,
            timeout=self.args.push_timeout,
        )
        self.configure_repo_cache_service(clone_dir, env=env)

        before_gets = state.total_get_count()
        before_key_counts = state.counts_snapshot()
        before_stats = self.cache_admin_stats()
        self.run_cmd(
            f"{name} crab hydrate all",
            [self.crab_bin, "hydrate", "--all"],
            clone_dir,
            env=env,
            timeout=self.args.push_timeout,
        )
        after_gets = state.total_get_count()
        after_key_counts = state.counts_snapshot()
        after_stats = self.cache_admin_stats()
        hydrated_sha = sha256_file(clone_dir / "model.bin")
        self.check(
            f"{name}-hydrated-byte-identical",
            hydrated_sha == expected_sha,
            {"expected_sha256": expected_sha, "hydrated_sha256": hydrated_sha},
        )

        record = CliHydrateRecord(
            name=name,
            origin_gets_before=before_gets,
            origin_gets_after=after_gets,
            origin_get_key_delta={
                key: after_key_counts.get(key, 0) - before_key_counts.get(key, 0)
                for key in sorted(set(before_key_counts) | set(after_key_counts))
                if after_key_counts.get(key, 0) - before_key_counts.get(key, 0) > 0
            },
            cache_hits_delta=self.traffic_value(after_stats, "cache_hits")
            - self.traffic_value(before_stats, "cache_hits"),
            cache_misses_delta=self.traffic_value(after_stats, "cache_misses")
            - self.traffic_value(before_stats, "cache_misses"),
            origin_fetches_delta=self.traffic_value(after_stats, "origin_fetches")
            - self.traffic_value(before_stats, "origin_fetches"),
            origin_avoided_reads_delta=self.traffic_value(after_stats, "origin_avoided_reads")
            - self.traffic_value(before_stats, "origin_avoided_reads"),
            mutable_read_rejections_delta=self.traffic_value(
                after_stats, "mutable_read_rejections"
            )
            - self.traffic_value(before_stats, "mutable_read_rejections"),
            mutable_write_rejections_delta=self.traffic_value(
                after_stats, "mutable_write_rejections"
            )
            - self.traffic_value(before_stats, "mutable_write_rejections"),
            hydrated_sha256=hydrated_sha,
        )
        self.report.cli_hydrates.append(asdict(record))
        self.write_report()

        self.check(
            f"{name}-cache-service-mutable-rejections-flat",
            record.mutable_read_rejections_delta == 0
            and record.mutable_write_rejections_delta == 0,
            {
                "mutable_read_rejections_delta": record.mutable_read_rejections_delta,
                "mutable_write_rejections_delta": record.mutable_write_rejections_delta,
            },
        )

        origin_get_delta = after_gets - before_gets
        if expect_origin_gets:
            self.check(
                f"{name}-origin-gets-increased-on-cold-hydrate",
                origin_get_delta > 0,
                {"before": before_gets, "after": after_gets},
            )
            self.check(
                f"{name}-cache-service-recorded-origin-fetches",
                record.origin_fetches_delta > 0,
                {"delta": record.origin_fetches_delta},
            )
            self.check(
                f"{name}-cache-service-recorded-misses",
                record.cache_misses_delta > 0,
                {"delta": record.cache_misses_delta},
            )
        else:
            immutable_origin_gets = {
                key: count
                for key, count in record.origin_get_key_delta.items()
                if key.startswith(".crab/xorbs/") or key.startswith(".crab/shards/")
            }
            self.check(
                f"{name}-immutable-origin-gets-flat-on-warm-hydrate",
                not immutable_origin_gets,
                {
                    "before": before_gets,
                    "after": after_gets,
                    "immutable_key_delta": immutable_origin_gets,
                },
            )
            self.check(
                f"{name}-cache-service-origin-fetches-flat",
                record.origin_fetches_delta == 0,
                {"delta": record.origin_fetches_delta},
            )
            self.check(
                f"{name}-cache-service-recorded-hit",
                record.cache_hits_delta > 0,
                {"delta": record.cache_hits_delta},
            )
            self.check(
                f"{name}-cache-service-recorded-origin-avoidance",
                record.origin_avoided_reads_delta > 0,
                {"delta": record.origin_avoided_reads_delta},
            )

        return record

    def verify_cli_hydrate_uses_cache_service(self) -> tuple[str, str]:
        _, remote_url, data, expected_sha = self.prepare_cli_source_repo()
        self.check(
            "cli-source-model-artifact-created",
            len(data) == self.args.cli_file_kib * 1024,
            {"bytes": len(data)},
        )
        self.push_duplicate_repo_uses_cache_service_dedup(data)
        cold = self.clone_configure_and_hydrate(
            "cli-cold-hydrate",
            remote_url,
            expected_sha,
            cache_name="cli-cold-cache",
            expect_origin_gets=False,
        )
        warm = self.clone_configure_and_hydrate(
            "cli-warm-hydrate",
            remote_url,
            expected_sha,
            cache_name="cli-warm-cache",
            expect_origin_gets=False,
        )
        self.check(
            "cli-hydrates-use-push-warmed-cache-service",
            cold.cache_hits_delta > 0 and warm.cache_hits_delta > 0,
            {
                "cold_cache_hits_delta": cold.cache_hits_delta,
                "cold_origin_fetches_delta": cold.origin_fetches_delta,
                "warm_cache_hits_delta": warm.cache_hits_delta,
                "warm_origin_fetches_delta": warm.origin_fetches_delta,
            },
        )
        stats = self.cache_admin_stats()
        self.check(
            "cli-admin-dedup-index-populated",
            stats.get("dedup_index", {}).get("indexed_chunks", 0) > 0,
            {"dedup_index": stats.get("dedup_index", {})},
        )
        self.check(
            "cli-admin-metadata-cache-observed",
            self.object_traffic_value(stats, "metadata", "cache_hits") > 0
            and self.object_traffic_value(stats, "metadata", "push_warming_writes") > 0,
            {"metadata": stats.get("traffic", {}).get("by_object_type", {}).get("metadata")},
        )
        self.check(
            "cli-admin-shard-cache-observed",
            self.object_traffic_value(stats, "shard", "cache_hits") > 0
            and self.object_traffic_value(stats, "shard", "push_warming_writes") > 0,
            {"shard": stats.get("traffic", {}).get("by_object_type", {}).get("shard")},
        )
        self.check(
            "cli-admin-xorb-cache-observed",
            self.object_traffic_value(stats, "xorb", "cache_hits") > 0
            and self.object_traffic_value(stats, "xorb", "push_warming_writes") > 0
            and self.object_traffic_value(stats, "xorb", "origin_fetches") == 0,
            {"xorb": stats.get("traffic", {}).get("by_object_type", {}).get("xorb")},
        )
        return remote_url, expected_sha

    def verify_cli_cache_service_recovery(self, remote_url: str, original_sha: str) -> None:
        repo = self.run_root / "cli-source"
        model = repo / "model.bin"
        self.check("cache-recovery-source-is-original", sha256_file(model) == original_sha)
        data = bytearray(model.read_bytes())
        offset = len(data) // 2
        data[offset:offset + 4096] = bytes(value ^ 0xA5 for value in data[offset:offset + 4096])
        expected_sha = hashlib.sha256(data).hexdigest()
        model.write_bytes(data)
        env = self.client_env("cli-source-cache")
        self.run_cmd("cache recovery add", [self.crab_bin, "add", "--jobs", "0", "model.bin"], repo, env=env, timeout=self.args.push_timeout)
        self.run_cmd("cache recovery commit", ["git", "commit", "-m", "cache recovery incremental version"], repo, env=env)

        observation: dict[str, Any] = {"cooldown_seconds": 30, "expected_sha256": expected_sha}
        with recovering_cache_service(self.require_proxy_state(), self.cache_service_url) as (url, snapshot):
            fault_env = dict(env, CRAB_CACHE_SERVICE_URL=url)
            try:
                record = self.run_cmd(
                    "cache recovery single push", [self.crab_bin, "push", "--json", "origin", "HEAD:refs/heads/main"],
                    repo, env=fault_env, timeout=self.args.push_timeout,
                )
                observation.update(push_duration_ms=record.duration_ms, push_exit_code=record.exit_code)
            finally:
                observation.update(snapshot())
                # Keep the timeline even when the command times out or fails.
                # Embedding it in the report binds it to the evidence manifest.
                self.report.checks.append({
                    "name": "cache-recovery-command-evidence", "ok": observation.get("push_exit_code") == 0,
                    "detail": observation, "timestamp": utc_now(),
                })
                self.write_report()

        requests = observation["requests"]
        failures = [row for row in requests if row.get("injected")]
        self.check("cache-recovery-one-injected-failure", len(failures) == 1 and failures[0]["status"] == 503)
        failure = failures[0]
        for name in ("health", "capabilities"):
            rows = [row for row in requests if row["path"] == f"/v1/{name}"]
            self.check(
                f"cache-recovery-single-healthy-{name}",
                len(rows) == 1 and rows[0]["status"] == 200 and rows[0]["end_s"] < failure["start_s"],
                {"requests": rows},
            )
        gates = observation["origin_gates"]
        self.check("cache-recovery-two-sequential-origin-gates", len(gates) == 2 and all(row.get("cancelled") is False for row in gates))
        later = [row for row in requests if row["start_s"] > failure["end_s"]]
        premature = [row for row in later if row["start_s"] < failure["end_s"] + 30]
        self.check("cache-recovery-no-requests-during-cooldown", not premature, {"requests": premature})
        recovered = [row for row in later if row["method"] == "PUT" and row["status"] == 201]
        self.check("cache-recovery-same-push-resumes-warming", bool(recovered), {"first_recovered": recovered[0] if recovered else None})

        key = urllib.parse.unquote(recovered[0]["path"][len("/v1/"):])
        origin_bytes = self.get_origin_object(key)
        state = self.require_proxy_state()
        before = state.count_for_key(key)
        status, headers, cache_bytes = self.cache_get(key)
        self.check(
            "cache-recovery-warmed-bytes-match-origin",
            status == 200 and headers.get("x-cache") == "HIT" and cache_bytes == origin_bytes and state.count_for_key(key) == before,
            {"key": key, "sha256": hashlib.sha256(origin_bytes).hexdigest(), "cache_status": headers.get("x-cache")},
        )
        clone = self.run_root / "cli-recovery-clone"
        clone_env = self.client_env("cli-recovery-cache")
        self.run_cmd("cache recovery clone", [self.crab_bin, "clone", remote_url, str(clone), "--jsonl"], self.run_root, env=clone_env, timeout=self.args.push_timeout)
        self.configure_repo_cache_service(clone, env=clone_env)
        self.check("cache-recovery-clone-has-pointer", (clone / "model.bin").stat().st_size < len(data))
        self.run_cmd("cache recovery hydrate", [self.crab_bin, "hydrate", "--all", "--json"], clone, env=clone_env, timeout=self.args.push_timeout)
        hydrated_sha = sha256_file(clone / "model.bin")
        self.check("cache-recovery-hydrated-byte-identical", hydrated_sha == expected_sha, {"expected_sha256": expected_sha, "hydrated_sha256": hydrated_sha})
        clean = self.run_cmd("cache recovery clean Git status", ["git", "status", "--porcelain", "--untracked-files=no"], clone, env=clone_env)
        self.check("cache-recovery-hydrated-worktree-clean", not Path(clean.stdout_log).read_text().strip())

    def verify_restart_persistence_uses_cache_service(
        self,
        cli_remote_url: str,
        cli_expected_sha: str,
        support_repo: Path,
    ) -> None:
        state = self.require_proxy_state()
        data = deterministic_bytes(self.args.object_kib * 1024, f"{self.run_id}:restart-direct")
        key = self.origin_key("restart-direct", data)
        self.put_origin_object(key, data)
        warm_before = state.count_for_key(key)
        self.assert_cache_read(
            "restart-persistence-direct-warm-miss",
            key,
            data,
            expected_status=200,
            expected_cache="MISS",
            expected_origin_gets=warm_before + 1,
        )

        old_url = self.cache_service_url
        self.stop_cache_server()
        self.check("restart-persistence-cache-server-stopped", self.cache_proc is None)
        self.start_cache_server()
        new_url = self.cache_service_url
        self.check(
            "restart-persistence-cache-service-url-changed",
            old_url != new_url,
            {"old": old_url, "new": new_url},
        )
        self.configure_repo_cache_service(support_repo)

        direct_before = state.count_for_key(key)
        total_before_direct = state.total_get_count()
        direct_status, direct_headers, direct_body = self.cache_get(key)
        direct_after = state.count_for_key(key)
        total_after_direct = state.total_get_count()
        range_status, range_headers, range_body = self.cache_get(key, byte_range="bytes=2-9")
        range_after = state.count_for_key(key)
        total_after_range = state.total_get_count()

        cli_record = self.clone_configure_and_hydrate(
            "restart-cli-hydrate",
            cli_remote_url,
            cli_expected_sha,
            cache_name="cli-restart-cache",
            expect_origin_gets=False,
        )

        record = RestartPersistenceRecord(
            name="cache-server-restart-persistence",
            direct_key=key,
            old_cache_service_url=old_url,
            new_cache_service_url=new_url,
            cache_root=str(self.cache_root),
            direct_status=direct_status,
            direct_cache_status=direct_headers.get("x-cache", ""),
            range_status=range_status,
            range_cache_status=range_headers.get("x-cache", ""),
            direct_origin_gets_before=direct_before,
            direct_origin_gets_after_direct=direct_after,
            direct_origin_gets_after_range=range_after,
            total_origin_gets_before_direct=total_before_direct,
            total_origin_gets_after_direct=total_after_direct,
            total_origin_gets_after_range=total_after_range,
            direct_body_len=len(direct_body),
            range_body_len=len(range_body),
            cli_origin_gets_before=cli_record.origin_gets_before,
            cli_origin_gets_after=cli_record.origin_gets_after,
            cli_origin_get_key_delta=cli_record.origin_get_key_delta,
            cli_cache_hits_delta=cli_record.cache_hits_delta,
            cli_origin_fetches_delta=cli_record.origin_fetches_delta,
            cli_origin_avoided_reads_delta=cli_record.origin_avoided_reads_delta,
            cli_mutable_read_rejections_delta=cli_record.mutable_read_rejections_delta,
            cli_mutable_write_rejections_delta=cli_record.mutable_write_rejections_delta,
            cli_hydrated_sha256=cli_record.hydrated_sha256,
        )
        self.report.restart_persistence.append(asdict(record))
        self.write_report()

        self.check("restart-persistence-direct-status", direct_status == 200, {"status": direct_status})
        self.check(
            "restart-persistence-direct-hit",
            direct_headers.get("x-cache") == "HIT",
            {"x-cache": direct_headers.get("x-cache", "")},
        )
        self.check("restart-persistence-direct-body", direct_body == data, {"body_len": len(direct_body)})
        self.check("restart-persistence-range-status", range_status == 206, {"status": range_status})
        self.check(
            "restart-persistence-range-hit",
            range_headers.get("x-cache") == "HIT",
            {"x-cache": range_headers.get("x-cache", "")},
        )
        self.check("restart-persistence-range-body", range_body == data[2:10], {"body_len": len(range_body)})
        self.check(
            "restart-persistence-direct-origin-flat",
            direct_after == direct_before
            and range_after == direct_before
            and total_after_direct == total_before_direct
            and total_after_range == total_before_direct,
            {
                "key_before": direct_before,
                "key_after_direct": direct_after,
                "key_after_range": range_after,
                "total_before": total_before_direct,
                "total_after_direct": total_after_direct,
                "total_after_range": total_after_range,
            },
        )
        self.check(
            "restart-persistence-cli-hydrate-origin-flat",
            not {
                key: count
                for key, count in cli_record.origin_get_key_delta.items()
                if key.startswith(".crab/xorbs/") or key.startswith(".crab/shards/")
            },
            {
                "before": cli_record.origin_gets_before,
                "after": cli_record.origin_gets_after,
                "immutable_key_delta": {
                    key: count
                    for key, count in cli_record.origin_get_key_delta.items()
                    if key.startswith(".crab/xorbs/") or key.startswith(".crab/shards/")
                },
            },
        )
        self.check(
            "restart-persistence-cli-hydrate-cache-hit",
            cli_record.cache_hits_delta > 0
            and cli_record.origin_fetches_delta == 0
            and cli_record.origin_avoided_reads_delta > 0,
            {
                "cache_hits_delta": cli_record.cache_hits_delta,
                "origin_fetches_delta": cli_record.origin_fetches_delta,
                "origin_avoided_reads_delta": cli_record.origin_avoided_reads_delta,
            },
        )
        self.check(
            "restart-persistence-cli-hydrate-mutable-rejections-flat",
            cli_record.mutable_read_rejections_delta == 0
            and cli_record.mutable_write_rejections_delta == 0,
            {
                "mutable_read_rejections_delta": cli_record.mutable_read_rejections_delta,
                "mutable_write_rejections_delta": cli_record.mutable_write_rejections_delta,
            },
        )

    def cache_file_for_integrity_key(self, key: str) -> tuple[str, Path]:
        hash_hex = key.rsplit("/", 1)[-1]
        if key.startswith(".crab/xorbs/"):
            return "xorb", self.cache_root / "xorbs" / hash_hex[:2] / hash_hex
        if key.startswith(".crab/shards/"):
            return "shard", self.cache_root / "shards" / hash_hex[:2] / hash_hex
        raise SmokeError(f"unsupported integrity repair key: {key}")

    def corrupt_persisted_cache_file(self, path: Path, object_type: str) -> int:
        data = bytearray(path.read_bytes())
        if not data:
            raise SmokeError(f"cannot corrupt empty cache file: {path}")
        offset = len(data) - 1 if object_type == "xorb" else 0
        data[offset] ^= 0xFF
        path.write_bytes(bytes(data))
        return len(data)

    def verify_persisted_cache_integrity_repairs(self, support_repo: Path) -> None:
        state = self.require_proxy_state()
        fixtures: list[dict[str, Any]] = []
        for pattern, predicate in (
            (
                ".crab/xorbs/{first-two-hex}/{hash}",
                lambda key: key.startswith(".crab/xorbs/"),
            ),
            (
                ".crab/shards/{first-two-hex}/{hash}",
                lambda key: key.startswith(".crab/shards/"),
            ),
        ):
            key, origin_body = self.origin_object_matching(pattern, predicate)
            object_type, cache_file = self.cache_file_for_integrity_key(key)
            status, headers, body = self.cache_get(key)
            self.check(
                f"integrity-repair-{object_type}-pre-restart-hot-read",
                status == 200 and headers.get("x-cache") == "HIT" and body == origin_body,
                {"status": status, "x-cache": headers.get("x-cache", "")},
            )
            self.check(
                f"integrity-repair-{object_type}-cache-file-present-before-restart",
                cache_file.is_file(),
                {"cache_file": str(cache_file)},
            )
            fixtures.append(
                {
                    "pattern": pattern,
                    "key": key,
                    "object_type": object_type,
                    "cache_file": cache_file,
                    "valid_body": body,
                }
            )

        old_url = self.cache_service_url
        self.stop_cache_server()
        self.check("integrity-repair-cache-server-stopped", self.cache_proc is None)
        for fixture in fixtures:
            corrupt_len = self.corrupt_persisted_cache_file(
                fixture["cache_file"],
                fixture["object_type"],
            )
            fixture["corrupt_body_len"] = corrupt_len
            self.check(
                f"integrity-repair-{fixture['object_type']}-corrupt-file-same-size",
                corrupt_len == len(fixture["valid_body"]),
                {
                    "corrupt_body_len": corrupt_len,
                    "valid_body_len": len(fixture["valid_body"]),
                },
            )

        self.start_cache_server()
        new_url = self.cache_service_url
        self.check(
            "integrity-repair-cache-service-url-changed",
            old_url != new_url,
            {"old": old_url, "new": new_url},
        )
        self.configure_repo_cache_service(support_repo)

        post_restart_stats = self.cache_admin_stats(
            artifact_name="integrity-repair-post-restart-admin-stats.json",
            artifact_key="integrity_repair_post_restart_admin_stats",
            check_name="integrity-repair-post-restart-admin-stats-status",
        )
        startup_repairs = self.startup_integrity_repair_total(post_restart_stats)
        self.check(
            "integrity-repair-startup-repairs-clean",
            startup_repairs == 0,
            {"startup_integrity": post_restart_stats.get("startup_integrity", {})},
        )

        for fixture in fixtures:
            key = fixture["key"]
            object_type = fixture["object_type"]
            valid_body = fixture["valid_body"]
            cache_file = fixture["cache_file"]
            before_stats = self.cache_admin_stats(
                artifact_name=f"integrity-repair-{object_type}-before-admin-stats.json",
                artifact_key=f"integrity_repair_{object_type}_before_admin_stats",
                check_name=f"integrity-repair-{object_type}-before-admin-stats-status",
            )
            before_origin_gets = state.count_for_key(key)
            before_total_origin_gets = state.total_get_count()
            before_invalid = self.integrity_value(
                before_stats,
                "runtime_integrity",
                "invalid_objects_evicted",
            )
            before_missing = self.integrity_value(
                before_stats,
                "runtime_integrity",
                "missing_files_repaired",
            )
            before_recreated = self.integrity_value(
                before_stats,
                "runtime_integrity",
                "metadata_entries_recreated",
            )

            repair_status, repair_headers, repair_body = self.cache_get(key)
            after_repair_stats = self.cache_admin_stats(
                artifact_name=f"integrity-repair-{object_type}-after-repair-admin-stats.json",
                artifact_key=f"integrity_repair_{object_type}_after_repair_admin_stats",
                check_name=f"integrity-repair-{object_type}-after-repair-admin-stats-status",
            )
            after_repair_origin_gets = state.count_for_key(key)
            after_repair_total_origin_gets = state.total_get_count()

            second_status, second_headers, second_body = self.cache_get(key)
            after_second_stats = self.cache_admin_stats(
                artifact_name=f"integrity-repair-{object_type}-after-second-admin-stats.json",
                artifact_key=f"integrity_repair_{object_type}_after_second_admin_stats",
                check_name=f"integrity-repair-{object_type}-after-second-admin-stats-status",
            )
            after_second_origin_gets = state.count_for_key(key)
            after_second_total_origin_gets = state.total_get_count()

            after_repair_invalid = self.integrity_value(
                after_repair_stats,
                "runtime_integrity",
                "invalid_objects_evicted",
            )
            after_second_invalid = self.integrity_value(
                after_second_stats,
                "runtime_integrity",
                "invalid_objects_evicted",
            )
            after_second_missing = self.integrity_value(
                after_second_stats,
                "runtime_integrity",
                "missing_files_repaired",
            )
            after_second_recreated = self.integrity_value(
                after_second_stats,
                "runtime_integrity",
                "metadata_entries_recreated",
            )

            record = CacheIntegrityRepairRecord(
                name=f"persisted-cache-integrity-repair-{object_type}",
                pattern=fixture["pattern"],
                key=key,
                object_type=object_type,
                cache_file=str(cache_file),
                old_cache_service_url=old_url,
                new_cache_service_url=new_url,
                corrupt_body_len=fixture["corrupt_body_len"],
                valid_body_len=len(valid_body),
                repair_status=repair_status,
                repair_cache_status=repair_headers.get("x-cache", ""),
                second_status=second_status,
                second_cache_status=second_headers.get("x-cache", ""),
                origin_gets_before_repair=before_origin_gets,
                origin_gets_after_repair=after_repair_origin_gets,
                origin_gets_after_second=after_second_origin_gets,
                total_origin_gets_before_repair=before_total_origin_gets,
                total_origin_gets_after_repair=after_repair_total_origin_gets,
                total_origin_gets_after_second=after_second_total_origin_gets,
                total_bytes_before_repair=int(before_stats.get("total_bytes", 0)),
                total_bytes_after_repair=int(after_repair_stats.get("total_bytes", 0)),
                total_bytes_after_second=int(after_second_stats.get("total_bytes", 0)),
                runtime_invalid_objects_evicted_before=before_invalid,
                runtime_invalid_objects_evicted_after_repair=after_repair_invalid,
                runtime_invalid_objects_evicted_after_second=after_second_invalid,
                runtime_missing_files_repaired_before=before_missing,
                runtime_missing_files_repaired_after_second=after_second_missing,
                runtime_metadata_entries_recreated_before=before_recreated,
                runtime_metadata_entries_recreated_after_second=after_second_recreated,
                startup_integrity_repairs_after_restart=startup_repairs,
                repair_body_len=len(repair_body),
                second_body_len=len(second_body),
            )
            self.report.cache_integrity_repairs.append(asdict(record))
            self.write_report()
            self.record_read(
                record.name + "-repair",
                key,
                repair_status,
                repair_headers.get("x-cache", ""),
                len(repair_body),
            )
            self.record_read(
                record.name + "-second",
                key,
                second_status,
                second_headers.get("x-cache", ""),
                len(second_body),
            )

            self.check(f"{record.name}-repair-status", repair_status == 200, {"status": repair_status})
            self.check(
                f"{record.name}-repair-miss",
                repair_headers.get("x-cache") == "MISS",
                {"x-cache": repair_headers.get("x-cache", "")},
            )
            self.check(f"{record.name}-repair-body", repair_body == valid_body, {"body_len": len(repair_body)})
            self.check(f"{record.name}-second-status", second_status == 200, {"status": second_status})
            self.check(
                f"{record.name}-second-hit",
                second_headers.get("x-cache") == "HIT",
                {"x-cache": second_headers.get("x-cache", "")},
            )
            self.check(f"{record.name}-second-body", second_body == valid_body, {"body_len": len(second_body)})
            self.check(
                f"{record.name}-origin-refetch-once",
                after_repair_origin_gets == before_origin_gets + 1
                and after_repair_total_origin_gets == before_total_origin_gets + 1,
                {
                    "key_before": before_origin_gets,
                    "key_after": after_repair_origin_gets,
                    "total_before": before_total_origin_gets,
                    "total_after": after_repair_total_origin_gets,
                },
            )
            self.check(
                f"{record.name}-second-origin-flat",
                after_second_origin_gets == after_repair_origin_gets
                and after_second_total_origin_gets == after_repair_total_origin_gets,
                {
                    "key_after_repair": after_repair_origin_gets,
                    "key_after_second": after_second_origin_gets,
                    "total_after_repair": after_repair_total_origin_gets,
                    "total_after_second": after_second_total_origin_gets,
                },
            )
            self.check(
                f"{record.name}-runtime-invalid-eviction-recorded",
                after_repair_invalid == before_invalid + 1
                and after_second_invalid == after_repair_invalid,
                {
                    "before": before_invalid,
                    "after_repair": after_repair_invalid,
                    "after_second": after_second_invalid,
                },
            )
            self.check(
                f"{record.name}-other-runtime-repairs-flat",
                after_second_missing == before_missing
                and after_second_recreated == before_recreated,
                {
                    "missing_before": before_missing,
                    "missing_after": after_second_missing,
                    "recreated_before": before_recreated,
                    "recreated_after": after_second_recreated,
                },
            )
            self.check(
                f"{record.name}-cache-bytes-restored",
                record.total_bytes_after_repair == record.total_bytes_before_repair
                and record.total_bytes_after_second == record.total_bytes_after_repair,
                {
                    "before": record.total_bytes_before_repair,
                    "after_repair": record.total_bytes_after_repair,
                    "after_second": record.total_bytes_after_second,
                },
            )
            self.check(
                f"{record.name}-cache-file-restored",
                cache_file.read_bytes() == valid_body,
                {"cache_file": str(cache_file)},
            )
            self.check(
                f"{record.name}-no-secret-in-body",
                self.args.cache_psk.encode("utf-8") not in repair_body
                and self.args.cache_psk.encode("utf-8") not in second_body,
            )

        warm_key = (
            f"{REMOTE_PREFIX}/{self.run_id}/integrity-repair/"
            "packs/pack-post-restart-warm.pack"
        )
        warm_data = deterministic_bytes(4096, f"{self.run_id}:integrity-repair-push-warm")
        before_stats = self.cache_admin_stats(
            artifact_name="integrity-repair-push-warm-before-admin-stats.json",
            artifact_key="integrity_repair_push_warm_before_admin_stats",
            check_name="integrity-repair-push-warm-before-admin-stats-status",
        )
        before_gets = state.total_get_count()
        before_puts = state.total_put_count()
        put_status, put_headers, put_body = self.cache_put(warm_key, warm_data)
        after_stats = self.cache_admin_stats(
            artifact_name="integrity-repair-push-warm-after-admin-stats.json",
            artifact_key="integrity_repair_push_warm_after_admin_stats",
            check_name="integrity-repair-push-warm-after-admin-stats-status",
        )
        self.check(
            "integrity-repair-post-restart-push-warming-status",
            put_status == 201,
            {"status": put_status},
        )
        self.check(
            "integrity-repair-post-restart-push-warming-no-cache-status",
            "x-cache" not in put_headers,
            {"x-cache": put_headers.get("x-cache", "")},
        )
        self.check(
            "integrity-repair-post-restart-push-warming-body-empty",
            len(put_body) == 0,
            {"body_len": len(put_body)},
        )
        self.check(
            "integrity-repair-post-restart-push-warming-origin-flat",
            state.total_get_count() == before_gets and state.total_put_count() == before_puts,
            {
                "gets_before": before_gets,
                "gets_after": state.total_get_count(),
                "puts_before": before_puts,
                "puts_after": state.total_put_count(),
            },
        )
        self.check(
            "integrity-repair-post-restart-push-warming-recorded",
            self.traffic_value(after_stats, "push_warming_writes")
            == self.traffic_value(before_stats, "push_warming_writes") + 1
            and self.traffic_value(after_stats, "push_warming_bytes")
            == self.traffic_value(before_stats, "push_warming_bytes") + len(warm_data),
            {
                "writes_before": self.traffic_value(before_stats, "push_warming_writes"),
                "writes_after": self.traffic_value(after_stats, "push_warming_writes"),
                "bytes_before": self.traffic_value(before_stats, "push_warming_bytes"),
                "bytes_after": self.traffic_value(after_stats, "push_warming_bytes"),
                "body_len": len(warm_data),
            },
        )

    def verify_origin_outage_serves_cached_objects(self, repo: Path) -> None:
        state = self.require_proxy_state()
        hot_data = deterministic_bytes(4096, f"{self.run_id}:origin-outage-hot")
        hot_key = self.origin_key("origin-outage-hot", hot_data)
        cold_data = deterministic_bytes(4096, f"{self.run_id}:origin-outage-cold")
        cold_key = self.origin_key("origin-outage-cold", cold_data)
        self.put_origin_object(hot_key, hot_data)
        self.put_origin_object(cold_key, cold_data)

        warm_status, warm_headers, warm_body = self.cache_get(hot_key)
        self.check(
            "origin-outage-warm-miss",
            warm_status == 200 and warm_headers.get("x-cache") == "MISS" and warm_body == hot_data,
            {"status": warm_status, "x-cache": warm_headers.get("x-cache", "")},
        )
        before_stats = self.cache_admin_stats(
            artifact_name="origin-outage-before-admin-stats.json",
            artifact_key="origin_outage_before_admin_stats",
            check_name="origin-outage-before-admin-stats-status",
        )
        hot_before = state.count_for_key(hot_key)
        cold_before = state.count_for_key(cold_key)
        total_before = state.total_get_count()
        cache_hits_before = self.traffic_value(before_stats, "cache_hits")
        origin_fetches_before = self.traffic_value(before_stats, "origin_fetches")

        self.stop_origin_proxy()
        time.sleep(6)

        health_status, health_body = self.cache_probe("/v1/health")
        live_status, live_body = self.cache_probe("/v1/health/live")
        hot_status, hot_headers, hot_body = self.cache_get(hot_key)
        hot_after = state.count_for_key(hot_key)
        total_after_hot = state.total_get_count()
        range_status, range_headers, range_body = self.cache_get(hot_key, byte_range="bytes=8-31")
        hot_after_range = state.count_for_key(hot_key)
        total_after_range = state.total_get_count()
        cold_status, cold_headers, cold_body = self.cache_get(cold_key)
        cold_after = state.count_for_key(cold_key)
        total_after_cold = state.total_get_count()
        after_stats = self.cache_admin_stats(
            artifact_name="origin-outage-after-admin-stats.json",
            artifact_key="origin_outage_after_admin_stats",
            check_name="origin-outage-after-admin-stats-status",
        )
        cache_hits_after = self.traffic_value(after_stats, "cache_hits")
        origin_fetches_after = self.traffic_value(after_stats, "origin_fetches")

        record = OriginOutageRecord(
            name="origin-outage-cached-read-through",
            hot_key=hot_key,
            cold_key=cold_key,
            health_status=health_status,
            live_status=live_status,
            warm_status=warm_status,
            warm_cache_status=warm_headers.get("x-cache", ""),
            hot_status=hot_status,
            hot_cache_status=hot_headers.get("x-cache", ""),
            range_status=range_status,
            range_cache_status=range_headers.get("x-cache", ""),
            cold_status=cold_status,
            cold_cache_status=cold_headers.get("x-cache", ""),
            hot_origin_gets_before_outage=hot_before,
            hot_origin_gets_after_hot=hot_after,
            hot_origin_gets_after_range=hot_after_range,
            cold_origin_gets_before_outage=cold_before,
            cold_origin_gets_after_cold=cold_after,
            total_origin_gets_before_outage=total_before,
            total_origin_gets_after_hot=total_after_hot,
            total_origin_gets_after_range=total_after_range,
            total_origin_gets_after_cold=total_after_cold,
            cache_hits_before_outage=cache_hits_before,
            cache_hits_after_outage=cache_hits_after,
            origin_fetches_before_outage=origin_fetches_before,
            origin_fetches_after_outage=origin_fetches_after,
            hot_body_len=len(hot_body),
            range_body_len=len(range_body),
            cold_body_len=len(cold_body),
        )
        self.report.origin_outages.append(asdict(record))
        self.write_report()

        self.record_read(
            "origin-outage-hot-hit",
            hot_key,
            hot_status,
            hot_headers.get("x-cache", ""),
            len(hot_body),
        )
        self.record_read(
            "origin-outage-range-hit",
            hot_key,
            range_status,
            range_headers.get("x-cache", ""),
            len(range_body),
        )
        self.record_read(
            "origin-outage-cold-miss",
            cold_key,
            cold_status,
            cold_headers.get("x-cache", ""),
            len(cold_body),
        )

        self.check(
            "origin-outage-health-degraded",
            health_status == 503 and b"origin unreachable" in health_body,
            {"status": health_status, "body": health_body.decode("utf-8", errors="replace")},
        )
        self.check(
            "origin-outage-live-still-ok",
            live_status == 200 and live_body == b"ok",
            {"status": live_status, "body": live_body.decode("utf-8", errors="replace")},
        )
        self.check(
            "origin-outage-hot-hit",
            hot_status == 200 and hot_headers.get("x-cache") == "HIT" and hot_body == hot_data,
            {"status": hot_status, "x-cache": hot_headers.get("x-cache", "")},
        )
        self.check(
            "origin-outage-range-hit",
            range_status == 206
            and range_headers.get("x-cache") == "HIT"
            and range_body == hot_data[8:32],
            {"status": range_status, "x-cache": range_headers.get("x-cache", "")},
        )
        self.check(
            "origin-outage-cold-miss-fails-closed",
            cold_status == 504,
            {"status": cold_status, "body_len": len(cold_body)},
        )
        self.check(
            "origin-outage-origin-counters-flat",
            hot_after == hot_before
            and hot_after_range == hot_before
            and cold_after == cold_before
            and total_after_hot == total_before
            and total_after_range == total_before
            and total_after_cold == total_before,
            {
                "hot_before": hot_before,
                "hot_after": hot_after,
                "hot_after_range": hot_after_range,
                "cold_before": cold_before,
                "cold_after": cold_after,
                "total_before": total_before,
                "total_after_hot": total_after_hot,
                "total_after_range": total_after_range,
                "total_after_cold": total_after_cold,
            },
        )
        self.check(
            "origin-outage-admin-stats-continue",
            cache_hits_after >= cache_hits_before + 2 and origin_fetches_after == origin_fetches_before,
            {
                "cache_hits_before": cache_hits_before,
                "cache_hits_after": cache_hits_after,
                "origin_fetches_before": origin_fetches_before,
                "origin_fetches_after": origin_fetches_after,
            },
        )
        self.check(
            "origin-outage-no-secret-in-body",
            self.args.cache_psk.encode("utf-8") not in health_body
            and self.args.cache_psk.encode("utf-8") not in live_body
            and self.args.cache_psk.encode("utf-8") not in cold_body,
        )
        self.verify_support_bundle(
            repo,
            "origin-outage",
            expect_origin_degraded=True,
        )

    def stop_cache_server(self) -> None:
        if self.cache_proc is None:
            return
        self.terminate_process(self.cache_proc)
        self.cache_proc = None

    @staticmethod
    def terminate_process(proc: subprocess.Popen[bytes]) -> None:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)

    def stop_origin_proxy(self) -> None:
        if self.proxy_state is not None:
            self.proxy_state.close_connections()
        if self.proxy_server is not None:
            self.proxy_server.shutdown()
            self.proxy_server.server_close()
            self.proxy_server = None
        if self.proxy_state is not None:
            self.proxy_state.close_connections()
        if self.proxy_thread is not None:
            self.proxy_thread.join(timeout=5)
            self.proxy_thread = None

    def run(self) -> None:
        try:
            self.preflight()
            self.start_origin_proxy()
            self.start_cache_server()
            client_repo = self.configure_client_repo()
            self.verify_doctor_cache_service(client_repo)
            self.verify_advertised_mutable_route_contract_behavior()
            self.verify_transparent_mutable_auth_controls()
            self.verify_full_and_warm_range_reads()
            self.verify_cold_range_reads()
            self.verify_concurrent_cold_misses()
            self.verify_admin_traffic_stats()
            self.probe_enterprise_onboarding_client()
            self.verify_doctor_cache_service_active_probe(client_repo)
            self.verify_enterprise_auth_controls()
            self.verify_capabilities_contract()
            self.verify_request_limit_controls()
            cli_remote_url, cli_expected_sha = self.verify_cli_hydrate_uses_cache_service()
            self.verify_restart_persistence_uses_cache_service(
                cli_remote_url,
                cli_expected_sha,
                client_repo,
            )
            self.verify_advertised_immutable_route_contract_behavior()
            self.verify_persisted_cache_integrity_repairs(client_repo)
            self.verify_cache_pressure_keeps_recent_hot_object_warm()
            self.verify_support_bundle(client_repo, "post-traffic")
            # The incremental version follows checks expecting the original ref.
            self.verify_cli_cache_service_recovery(cli_remote_url, cli_expected_sha)
            self.verify_origin_outage_serves_cached_objects(client_repo)
            self.check(
                "cache-server-still-running",
                self.cache_proc is not None and self.cache_proc.poll() is None,
            )
            self.report.status = "passed"
            self.write_evidence_manifest()
        finally:
            self.stop_cache_server()
            self.stop_origin_proxy()


def load_json_file(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise SmokeError(f"failed to read {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise SmokeError(f"failed to parse {path} as JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise SmokeError(f"{path} must contain a JSON object")
    return payload


def artifact_path(report_path: Path, report: dict[str, Any], key: str) -> Path:
    artifacts = report.get("artifacts")
    if not isinstance(artifacts, dict):
        raise SmokeError("report.artifacts must be an object")
    value = artifacts.get(key)
    if not isinstance(value, str) or not value:
        raise SmokeError(f"report.artifacts.{key} is missing")
    path = Path(value)
    if path.is_absolute():
        raise SmokeError(f"report.artifacts.{key} must be relative to report.json")
    path = report_path.parent / path
    return path.resolve()


def audit_rustfs_report(report_path: Path) -> dict[str, Any]:
    report_path = report_path.resolve()
    script = Path(__file__).resolve()
    # v1.1.0 ships both this entry point and the retained two-script bundle.
    # Select code from that trusted package layout, never from report artifacts.
    verifier = (
        script.with_name("smoke-report-verifier.py")
        if script.name == "rustfs-smoke-script.py"
        else script.parents[1] / "verify-cache-service-smoke-report.py"
    )
    try:
        result = subprocess.run(
            [os.sys.executable, str(verifier), str(report_path)],
            capture_output=True, text=True, timeout=120, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SmokeError(f"report verifier could not finish: {exc}") from exc
    if result.returncode != 0:
        raise SmokeError(f"report audit failed: {result.stderr.strip()[-2000:]}")

    report = load_json_file(report_path)
    return {
        "report": str(report_path),
        "run_id": report.get("run_id"),
        "checks": len(report.get("checks", [])),
        "preflight_json": str(artifact_path(report_path, report, "cache_server_preflight_json")),
        "evidence_manifest": str(artifact_path(report_path, report, "cache_service_evidence_manifest")),
    }


def parse_args() -> argparse.Namespace:
    def positive_int(value: str) -> int:
        try:
            parsed = int(value)
        except ValueError as exc:
            raise argparse.ArgumentTypeError(f"{value!r} is not an integer") from exc
        if parsed < 1:
            raise argparse.ArgumentTypeError(f"{value!r} must be greater than zero")
        return parsed

    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--access-key", default=os.environ.get("AWS_ACCESS_KEY_ID", "crab"))
    parser.add_argument("--secret-key", default=os.environ.get("AWS_SECRET_ACCESS_KEY", "crab"))
    parser.add_argument("--region", default=os.environ.get("AWS_REGION", "us-east-1"))
    parser.add_argument("--crab-bin", default="crab")
    parser.add_argument("--cache-server-bin", default="crab-cache-server")
    parser.add_argument("--cache-psk", default=DEFAULT_PSK)
    parser.add_argument("--cache-server-log", default="info")
    parser.add_argument("--run-id")
    parser.add_argument("--object-kib", type=positive_int, default=256)
    parser.add_argument("--cli-file-kib", type=positive_int, default=1024)
    parser.add_argument("--max-cache-bytes", type=positive_int, default=64 * 1024 * 1024)
    parser.add_argument("--timeout", type=positive_int, default=120)
    parser.add_argument("--push-timeout", type=positive_int, default=240)
    parser.add_argument("--startup-timeout", type=positive_int, default=15)
    parser.add_argument("--origin-delay", type=float, default=1.0)
    parser.add_argument("--concurrent-probe-delay", type=float, default=0.2)
    parser.add_argument(
        "--audit-report",
        type=Path,
        help="Validate an existing RustFS smoke report.json without running live traffic.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.audit_report is not None:
        try:
            summary = audit_rustfs_report(args.audit_report)
        except SmokeError as exc:
            print(f"FAILED: {exc}", file=os.sys.stderr)
            return 1
        print("PASS cache-service RustFS report audit")
        print(f"report: {summary['report']}")
        print(f"run_id: {summary['run_id']}")
        print(f"checks: {summary['checks']}")
        print(f"preflight_json: {summary['preflight_json']}")
        print(f"evidence_manifest: {summary['evidence_manifest']}")
        return 0

    smoke = None
    try:
        smoke = CacheServiceRustfsSmoke(args)
        smoke.run()
    except Exception as exc:
        if smoke is not None:
            smoke.report.status = "failed"
            smoke.write_report()
        if not isinstance(exc, SmokeError):
            raise
        print(f"FAILED: {exc}", file=os.sys.stderr)
        if smoke is not None:
            print(f"report: {smoke.report.artifacts.get('report', '')}", file=os.sys.stderr)
        return 1
    print("PASS cache-service RustFS smoke")
    print(f"report: {smoke.report.artifacts.get('report', '')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
