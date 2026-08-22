#!/usr/bin/env python3
"""Run concurrent Crab push smokes against a local RustFS/S3 endpoint.

The harness creates a unique remote under ``crab://<bucket>/e2e-concurrent-push``
and local workdirs under ``/Volumes/Workspace/CrabRepos`` by default. It models
two AI-agent push cases:

* branch fanout: many agents push independent branches at the same time; all
  pushes must succeed, then fresh protocol-v2 clients must clone and fsck every
  branch with byte-identical content.
* same-branch contention: many agents push divergent commits to ``main`` at the
  same time; exactly one push may land, and all losers must fail with structured
  push statuses rather than corrupting remote state. With
  ``--rebase-on-non-fast-forward``, every same-branch agent must eventually
  integrate and the final clone must contain every agent file. The command loop
  acquires the push lock before refreshing/rebasing, then hands that lock into
  the push pipeline.

The retained report is written atomically across worker threads and includes
per-phase S3 HTTP attempts plus RustFS object-count and stored-byte deltas for
cost analysis.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import http.client
import http.server
import json
import os
import signal
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_ROOT = Path("/Volumes/Workspace/CrabRepos")
DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
REMOTE_PREFIX = "e2e-concurrent-push"
SECRET_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}
BAD_PUSH_STATUSES = {"internal", "unpack-failed", "missing-object", "malformed-object"}


class SmokeError(RuntimeError):
    """Raised when a smoke step fails."""


class RequestCountingProxy:
    """Forward S3 traffic while counting every HTTP request attempt."""

    def __init__(self, upstream_url: str, key_prefix: str) -> None:
        upstream = urllib.parse.urlsplit(upstream_url)
        if upstream.scheme not in {"http", "https"} or not upstream.hostname:
            raise SmokeError(f"unsupported request-meter upstream: {upstream_url}")
        self.upstream = upstream
        self.key_prefix = key_prefix.strip("/") + "/"
        self.lock = threading.Lock()
        self.requests = 0
        self.request_bytes = 0
        self.response_bytes = 0
        self.methods: dict[str, int] = {}
        self.operations: dict[str, int] = {}
        self.categories: dict[str, int] = {}
        self.classes: dict[str, int] = {}
        self.statuses: dict[str, int] = {}
        self.server: http.server.ThreadingHTTPServer | None = None
        self.thread: threading.Thread | None = None
        self.active_marker_armed = False
        self.active_marker_committed = threading.Event()
        self.active_marker_release = threading.Event()

    @property
    def url(self) -> str:
        if self.server is None:
            raise SmokeError("request meter has not started")
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}"

    def start(self) -> None:
        proxy = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:
                self.forward()

            def do_HEAD(self) -> None:
                self.forward()

            def do_PUT(self) -> None:
                self.forward()

            def do_POST(self) -> None:
                self.forward()

            def do_DELETE(self) -> None:
                self.forward()

            def do_PATCH(self) -> None:
                self.forward()

            def log_message(self, _format: str, *_args: object) -> None:
                return

            def forward(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length) if length else None
                headers = {
                    key: value
                    for key, value in self.headers.items()
                    if key.lower() not in {"connection", "proxy-connection"}
                }
                upstream_path = proxy.upstream.path.rstrip("/") + self.path
                connection_class = (
                    http.client.HTTPSConnection
                    if proxy.upstream.scheme == "https"
                    else http.client.HTTPConnection
                )
                connection = connection_class(
                    proxy.upstream.hostname,
                    proxy.upstream.port,
                    timeout=60,
                )
                recorded = False
                try:
                    connection.request(self.command, upstream_path, body=body, headers=headers)
                    response = connection.getresponse()
                    response_body = response.read()
                    response_headers = response.getheaders()
                    status = response.status
                    proxy.record(
                        self.command,
                        self.path,
                        self.headers,
                        length,
                        len(response_body),
                        status,
                    )
                    recorded = True
                    proxy.gate_active_marker_response(self.command, self.path, status)
                    self.send_response_only(status, response.reason)
                    original_content_length = None
                    for key, value in response_headers:
                        lower = key.lower()
                        if lower == "content-length":
                            original_content_length = value
                            continue
                        if lower in {
                            "connection",
                            "keep-alive",
                            "proxy-authenticate",
                            "proxy-authorization",
                            "te",
                            "trailer",
                            "transfer-encoding",
                            "upgrade",
                        }:
                            continue
                        self.send_header(key, value)
                    content_length = (
                        original_content_length
                        if self.command == "HEAD" and original_content_length is not None
                        else str(len(response_body))
                    )
                    self.send_header("Content-Length", content_length)
                    self.send_header("Connection", "close")
                    self.end_headers()
                    if self.command != "HEAD" and response_body:
                        self.wfile.write(response_body)
                except Exception as exc:
                    message = f"request meter upstream failure: {exc}".encode()
                    if not recorded:
                        proxy.record(
                            self.command,
                            self.path,
                            self.headers,
                            length,
                            len(message),
                            502,
                        )
                    try:
                        self.send_response(502)
                        self.send_header("Content-Type", "text/plain")
                        self.send_header("Content-Length", str(len(message)))
                        self.send_header("Connection", "close")
                        self.end_headers()
                        if self.command != "HEAD":
                            self.wfile.write(message)
                    except (BrokenPipeError, ConnectionResetError):
                        pass
                finally:
                    connection.close()
                    self.close_connection = True

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(
            target=self.server.serve_forever,
            name="rustfs-request-meter",
            daemon=True,
        )
        self.thread.start()

    def close(self) -> None:
        self.release_active_marker_gate()
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
        if self.thread is not None:
            self.thread.join(timeout=5)

    def arm_active_marker_gate(self) -> None:
        with self.lock:
            if self.active_marker_armed:
                raise SmokeError("active-marker response gate is already armed")
            self.active_marker_armed = True
            self.active_marker_committed.clear()
            self.active_marker_release.clear()

    def gate_active_marker_response(self, method: str, path: str, status: int) -> None:
        decoded_path = urllib.parse.unquote(urllib.parse.urlsplit(path).path)
        with self.lock:
            matches = (
                self.active_marker_armed
                and method == "PUT"
                and 200 <= status < 300
                and "/refs/journal/active/" in decoded_path
            )
            if matches:
                self.active_marker_armed = False
        if not matches:
            return
        self.active_marker_committed.set()
        self.active_marker_release.wait(timeout=60)

    def wait_for_active_marker(self, timeout: float) -> bool:
        return self.active_marker_committed.wait(timeout=timeout)

    def release_active_marker_gate(self) -> None:
        self.active_marker_release.set()

    def record(
        self,
        method: str,
        path: str,
        headers: http.client.HTTPMessage,
        request_bytes: int,
        response_bytes: int,
        status: int,
    ) -> None:
        operation = self.operation(method, path, headers)
        key = urllib.parse.unquote(urllib.parse.urlsplit(path).path).lstrip("/")
        relative_key = key.removeprefix(self.key_prefix)
        category = (
            store_category(relative_key)
            if relative_key != key
            else "outside-repository"
        )
        request_class = f"{category}:{operation}"
        status_class = f"{status // 100}xx"
        with self.lock:
            self.requests += 1
            self.request_bytes += request_bytes
            self.response_bytes += response_bytes
            self.methods[method] = self.methods.get(method, 0) + 1
            self.operations[operation] = self.operations.get(operation, 0) + 1
            self.categories[category] = self.categories.get(category, 0) + 1
            self.classes[request_class] = self.classes.get(request_class, 0) + 1
            self.statuses[status_class] = self.statuses.get(status_class, 0) + 1

    @staticmethod
    def operation(method: str, path: str, headers: http.client.HTTPMessage) -> str:
        query = urllib.parse.parse_qs(
            urllib.parse.urlsplit(path).query, keep_blank_values=True
        )
        if method == "HEAD":
            return "head"
        if method == "GET":
            return "list" if "list-type" in query or "versions" in query else "get"
        if method == "PUT":
            if "x-amz-copy-source" in headers:
                return "copy"
            return "multipart_part" if "partNumber" in query else "put"
        if method == "POST":
            if "uploads" in query:
                return "multipart_create"
            if "uploadId" in query:
                return "multipart_complete"
            if "delete" in query:
                return "delete_batch"
            return "post"
        if method == "DELETE":
            return "multipart_abort" if "uploadId" in query else "delete"
        return method.lower()

    def snapshot(self) -> dict[str, Any]:
        with self.lock:
            return {
                "requests": self.requests,
                "request_body_bytes": self.request_bytes,
                "response_body_bytes": self.response_bytes,
                "methods": dict(sorted(self.methods.items())),
                "operations": dict(sorted(self.operations.items())),
                "categories": dict(sorted(self.categories.items())),
                "classes": dict(sorted(self.classes.items())),
                "statuses": dict(sorted(self.statuses.items())),
            }

    @staticmethod
    def delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key in {"requests", "request_body_bytes", "response_body_bytes"}:
            result[key] = int(after.get(key, 0)) - int(before.get(key, 0))
        for key in {"methods", "operations", "categories", "classes", "statuses"}:
            earlier = before.get(key, {})
            later = after.get(key, {})
            result[key] = {
                name: int(later.get(name, 0)) - int(earlier.get(name, 0))
                for name in sorted(set(earlier) | set(later))
                if int(later.get(name, 0)) != int(earlier.get(name, 0))
            }
        return result


@dataclass
class CommandRecord:
    name: str
    args: list[str]
    cwd: str
    exit_code: int
    duration_ms: int
    stdout_log: str
    stderr_log: str


@dataclass
class PushRecord:
    agent: str
    branch: str
    command: CommandRecord
    status: str
    retryable: bool | None = None
    retry_after_secs: int | None = None
    integration_retries: int | None = None
    integration_retry_limit: int | None = None


@dataclass
class SmokeReport:
    schema: str
    version: str
    run_id: str
    status: str
    remote_url: str
    root: str
    endpoint_url: str
    env: dict[str, str]
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    branch_fanout: list[dict[str, Any]] = field(default_factory=list)
    branch_reads: list[dict[str, Any]] = field(default_factory=list)
    same_branch: list[dict[str, Any]] = field(default_factory=list)
    same_branch_read: dict[str, Any] = field(default_factory=dict)
    crash_boundary: dict[str, Any] = field(default_factory=dict)
    request_snapshots: list[dict[str, Any]] = field(default_factory=list)
    store_snapshots: list[dict[str, Any]] = field(default_factory=list)
    cost_model: dict[str, Any] = field(default_factory=dict)
    artifacts: dict[str, str] = field(default_factory=dict)
    updated_at: str = ""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "concurrent-push-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    out = "".join(c if c.isalnum() or c in "._-" else "-" for c in value.lower())
    return out.strip("-") or "command"


def store_category(relative_key: str) -> str:
    parts = relative_key.split("/")
    if len(parts) > 2 and parts[0] == "metadata" and parts[1] in {"pack", "shard"}:
        return "/".join(parts[:3])
    if len(parts) > 2 and parts[:2] == ["locks", "internal"]:
        return "/".join(parts[:3])
    if len(parts) > 1 and parts[0] in {
        "git_locator_db",
        "git-visibility",
        "locks",
        "manifests",
        "metadata",
        "ref-journal",
        "refs",
    }:
        return "/".join(parts[:2])
    return parts[0] or "repository-root"


def redact_env(env: dict[str, str]) -> dict[str, str]:
    redacted: dict[str, str] = {}
    for key, value in sorted(env.items()):
        if key in SECRET_KEYS:
            redacted[key] = "<redacted>"
        elif key.startswith("AWS_") or key.startswith("CRAB_") or key.startswith("GIT_"):
            redacted[key] = value
    return redacted


def first_json_object(text: str, schema: str) -> dict[str, Any] | None:
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("schema") == schema:
            return value
    return None


class ConcurrentPushSmoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        self.run_root = args.root / self.run_id
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.bin_dir = self.run_root / "bin"
        self.seed = self.run_root / "seed"
        self.branch_agents = self.run_root / "branch-agents"
        self.branch_readers = self.run_root / "branch-readers"
        self.same_agents = self.run_root / "same-branch-agents"
        self.same_branch_reader = self.run_root / "same-branch-reader"
        self.crash_agent = self.run_root / "crash-agent"
        self.crash_reader = self.run_root / "crash-reader"
        self.remote_url = f"crab://{args.bucket}/{REMOTE_PREFIX}/{self.run_id}"
        self.request_proxy: RequestCountingProxy | None = None
        self.env = self.build_env()
        self.command_index = 0
        self.command_lock = threading.Lock()
        self.report_lock = threading.RLock()
        self.store_inventory: dict[str, int] = {}
        self.report = SmokeReport(
            schema="crab.concurrent-push-smoke",
            version="1.4",
            run_id=self.run_id,
            status="running",
            remote_url=self.remote_url,
            root=str(self.run_root),
            endpoint_url=args.endpoint_url,
            env=redact_env(self.env),
            cost_model={
                "basis": "net live-object inventory deltas",
                "request_counts_available": False,
                "limitations": [
                    "overwrites of an existing key are not counted as new requests",
                    "GET, HEAD, LIST, DELETE, retry, and egress charges are not observed",
                    "provider minimum billable object sizes are not applied",
                ],
            },
            updated_at=utc_now(),
        )

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        endpoint_url = (
            self.request_proxy.url if self.request_proxy is not None else self.args.endpoint_url
        )
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_ENDPOINT_URL": endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_MERGE_AUTOEDIT": "no",
            }
        )
        env["PATH"] = str(self.bin_dir) + os.pathsep + env.get("PATH", "")
        return env

    def write_report(self) -> None:
        with self.report_lock:
            self.artifacts.mkdir(parents=True, exist_ok=True)
            self.report.updated_at = utc_now()
            path = self.artifacts / "report.json"
            temp_path = self.artifacts / "report.json.tmp"
            self.report.artifacts["report"] = str(path)
            payload = asdict(self.report)
            temp_path.write_text(
                json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            os.replace(temp_path, path)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        with self.report_lock:
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

    def next_log_paths(self, name: str) -> tuple[Path, Path]:
        with self.command_lock:
            self.command_index += 1
            index = self.command_index
        base = f"{index:03d}-{slug(name)}"
        self.logs.mkdir(parents=True, exist_ok=True)
        return self.logs / f"{base}.stdout.log", self.logs / f"{base}.stderr.log"

    def record_command(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        exit_code: int,
        duration_ms: int,
        stdout: str,
        stderr: str,
    ) -> CommandRecord:
        stdout_log, stderr_log = self.next_log_paths(name)
        stdout_log.write_text(stdout, encoding="utf-8", errors="replace")
        stderr_log.write_text(stderr, encoding="utf-8", errors="replace")
        record = CommandRecord(
            name=name,
            args=args,
            cwd=str(cwd),
            exit_code=exit_code,
            duration_ms=duration_ms,
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
        )
        with self.report_lock:
            self.report.commands.append(asdict(record))
            self.write_report()
        return record

    def run_cmd(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        *,
        check: bool = True,
        timeout: int | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> CommandRecord:
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        start = time.monotonic()
        proc = subprocess.run(
            args,
            cwd=cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout or self.args.timeout,
            check=False,
        )
        duration_ms = int((time.monotonic() - start) * 1000)
        record = self.record_command(
            name,
            args,
            cwd,
            proc.returncode,
            duration_ms,
            proc.stdout,
            proc.stderr,
        )
        if check and proc.returncode != 0:
            raise SmokeError(
                f"{name} failed with exit {proc.returncode}; stderr log: {record.stderr_log}"
            )
        return record

    def run_killed_after_active_marker(
        self,
        name: str,
        args: list[str],
        cwd: Path,
    ) -> tuple[CommandRecord, float]:
        proxy = self.request_proxy
        if proxy is None:
            raise SmokeError("active-marker crash injection requires request capture")
        proxy.arm_active_marker_gate()
        start = time.monotonic()
        proc = subprocess.Popen(
            args,
            cwd=cwd,
            env=self.env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=os.name == "posix",
        )
        marker_committed = proxy.wait_for_active_marker(self.args.push_timeout)
        killed_at = time.monotonic()

        def kill_process_group() -> None:
            if proc.poll() is not None:
                return
            if os.name == "posix":
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                proc.kill()

        try:
            kill_process_group()
            stdout, stderr = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            kill_process_group()
            stdout, stderr = proc.communicate()
        finally:
            proxy.release_active_marker_gate()
        duration_ms = int((time.monotonic() - start) * 1000)
        record = self.record_command(
            name,
            args,
            cwd,
            proc.returncode,
            duration_ms,
            stdout,
            stderr,
        )
        if not marker_committed:
            raise SmokeError(
                "push did not reach the active-marker boundary before timeout; "
                f"stderr log: {record.stderr_log}"
            )
        if proc.returncode == 0:
            raise SmokeError("crash-injected push exited successfully instead of being killed")
        return record, killed_at

    def run_git(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> CommandRecord:
        return self.run_cmd(
            name or "git " + " ".join(args),
            [self.args.git_bin, *args],
            repo,
            extra_env=extra_env,
        )

    def run_crab(self, repo: Path, args: list[str], *, name: str | None = None) -> CommandRecord:
        return self.run_cmd(name or "crab " + " ".join(args), [self.args.crab_bin, *args], repo)

    def configure_git_identity(self, repo: Path, who: str) -> None:
        self.run_git(repo, ["config", "user.name", f"Crab {who}"])
        self.run_git(repo, ["config", "user.email", f"{who}@example.invalid"])

    def configure_crab_repo(self, repo: Path) -> None:
        self.run_crab(repo, ["config", "set", "push.lock_wait_secs", str(self.args.lock_wait_secs)])
        self.run_crab(
            repo,
            ["config", "set", "push.max_cas_retries", str(self.args.manifest_cas_retries)],
        )

    def install_helper_alias(self) -> None:
        self.bin_dir.mkdir(parents=True, exist_ok=True)
        target = Path(self.args.crab_bin)
        alias = self.bin_dir / "git-remote-crab"
        try:
            alias.symlink_to(target)
        except (NotImplementedError, OSError):
            shutil.copy2(target, alias)

    def preflight(self) -> None:
        self.run_root.mkdir(parents=True, exist_ok=True)
        self.logs.mkdir(parents=True, exist_ok=True)
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.install_helper_alias()
        self.write_report()

        try:
            with urllib.request.urlopen(self.args.endpoint_url, timeout=5) as response:
                status = response.status
        except urllib.error.HTTPError as exc:
            status = exc.code
        except OSError as exc:
            self.check("rustfs-endpoint-reachable", False, {"error": str(exc)})
            return
        self.check("rustfs-endpoint-reachable", status < 500, {"status": status})

        if not self.args.no_request_capture:
            self.request_proxy = RequestCountingProxy(
                self.args.endpoint_url,
                f"{self.args.bucket}/{REMOTE_PREFIX}/{self.run_id}",
            )
            self.request_proxy.start()
            self.env = self.build_env()
            with self.report_lock:
                self.report.env = redact_env(self.env)
                self.report.cost_model = {
                    "basis": "metered S3 HTTP attempts plus net live-object inventory deltas",
                    "request_counts_available": True,
                    "request_scope": (
                        "every HTTP attempt crossing the local meter, including object_store "
                        "retries and LIST pages"
                    ),
                    "limitations": [
                        "provider prices and free tiers are not applied",
                        "request operation classes are inferred from HTTP method and S3 query",
                        "provider minimum billable object sizes are not applied",
                        "network transfer price depends on provider, region, and destination",
                    ],
                }
                self.write_report()

        if shutil.which("aws"):
            record = self.run_cmd(
                "aws create bucket",
                [
                    "aws",
                    "s3api",
                    "create-bucket",
                    "--bucket",
                    self.args.bucket,
                    "--endpoint-url",
                    self.args.endpoint_url,
                ],
                self.run_root,
                check=False,
            )
            stderr = Path(record.stderr_log).read_text(encoding="utf-8", errors="replace")
            already_exists = "BucketAlready" in stderr or "already" in stderr.lower()
            self.check(
                "bucket-create-or-exists",
                record.exit_code == 0 or already_exists,
                {"exit_code": record.exit_code, "already_exists": already_exists},
            )
        else:
            with self.report_lock:
                self.report.checks.append(
                    {
                        "name": "bucket-create-skipped",
                        "ok": True,
                        "detail": {"reason": "aws CLI not found; assuming bucket exists"},
                        "timestamp": utc_now(),
                    }
                )
                self.write_report()

    def request_snapshot(
        self,
        label: str,
        before: dict[str, Any] | None,
        *,
        attempted_pushes: int,
        successful_pushes: int,
    ) -> None:
        if self.request_proxy is None or before is None:
            return
        delta = RequestCountingProxy.delta(before, self.request_proxy.snapshot())
        snapshot = {
            "label": label,
            "attempted_pushes": attempted_pushes,
            "successful_pushes": successful_pushes,
            **delta,
            "timestamp": utc_now(),
        }
        if attempted_pushes > 0:
            snapshot["requests_per_attempt"] = round(
                delta["requests"] / attempted_pushes, 3
            )
            snapshot["request_body_bytes_per_attempt"] = round(
                delta["request_body_bytes"] / attempted_pushes, 3
            )
            snapshot["response_body_bytes_per_attempt"] = round(
                delta["response_body_bytes"] / attempted_pushes, 3
            )
        if successful_pushes > 0:
            snapshot["requests_per_successful_push"] = round(
                delta["requests"] / successful_pushes, 3
            )
            snapshot["request_body_bytes_per_successful_push"] = round(
                delta["request_body_bytes"] / successful_pushes, 3
            )
            snapshot["response_body_bytes_per_successful_push"] = round(
                delta["response_body_bytes"] / successful_pushes, 3
            )
        with self.report_lock:
            self.report.request_snapshots.append(snapshot)
            self.write_report()

    def store_snapshot(
        self,
        label: str,
        *,
        attempted_pushes: int,
        successful_pushes: int,
    ) -> None:
        if not shutil.which("aws"):
            return
        prefix = f"{REMOTE_PREFIX}/{self.run_id}/"
        record = self.run_cmd(
            f"aws store snapshot {label}",
            [
                "aws",
                "s3api",
                "list-objects-v2",
                "--bucket",
                self.args.bucket,
                "--prefix",
                prefix,
                "--endpoint-url",
                self.args.endpoint_url,
                "--output",
                "json",
            ],
            self.run_root,
        )
        payload = json.loads(Path(record.stdout_log).read_text(encoding="utf-8"))
        contents = payload.get("Contents") or []
        inventory = {str(item["Key"]): int(item.get("Size", 0)) for item in contents}
        categories: dict[str, dict[str, int]] = {}
        for key, size in inventory.items():
            relative = key.removeprefix(prefix)
            category = store_category(relative)
            entry = categories.setdefault(category, {"objects": 0, "stored_bytes": 0})
            entry["objects"] += 1
            entry["stored_bytes"] += size
        snapshot = {
            "label": label,
            "prefix": prefix,
            "object_count": len(inventory),
            "stored_bytes": sum(inventory.values()),
            "categories": dict(sorted(categories.items())),
            "attempted_pushes": attempted_pushes,
            "successful_pushes": successful_pushes,
            "timestamp": utc_now(),
        }
        with self.report_lock:
            snapshot["delta_objects"] = len(inventory) - len(self.store_inventory)
            snapshot["delta_stored_bytes"] = sum(inventory.values()) - sum(
                self.store_inventory.values()
            )
            delta_categories: dict[str, dict[str, int]] = {}
            for key in set(self.store_inventory) | set(inventory):
                before = self.store_inventory.get(key)
                after = inventory.get(key)
                relative = key.removeprefix(prefix)
                category = store_category(relative)
                entry = delta_categories.setdefault(
                    category, {"objects": 0, "stored_bytes": 0}
                )
                entry["objects"] += int(after is not None) - int(before is not None)
                entry["stored_bytes"] += (after or 0) - (before or 0)
            snapshot["delta_categories"] = {
                category: values
                for category, values in sorted(delta_categories.items())
                if values["objects"] != 0 or values["stored_bytes"] != 0
            }
            if attempted_pushes > 0:
                snapshot["net_objects_per_attempt"] = round(
                    snapshot["delta_objects"] / attempted_pushes, 3
                )
                snapshot["net_stored_bytes_per_attempt"] = round(
                    snapshot["delta_stored_bytes"] / attempted_pushes, 3
                )
            if successful_pushes > 0:
                snapshot["net_objects_per_successful_push"] = round(
                    snapshot["delta_objects"] / successful_pushes, 3
                )
                snapshot["net_stored_bytes_per_successful_push"] = round(
                    snapshot["delta_stored_bytes"] / successful_pushes, 3
                )
            self.store_inventory = inventory
            self.report.store_snapshots.append(snapshot)
            self.write_report()

    def protocol_v2_clone(self, branch: str, target: Path, name: str) -> dict[str, Any]:
        record = self.run_git(
            self.run_root,
            [
                "-c",
                "protocol.version=2",
                "clone",
                "--single-branch",
                "--branch",
                branch,
                self.remote_url,
                str(target),
            ],
            name=name,
            extra_env={"GIT_TRACE_PACKET": "1"},
        )
        trace = Path(record.stderr_log).read_text(encoding="utf-8", errors="replace")
        self.run_git(target, ["fsck", "--strict"], name=f"{name} fsck")
        return {
            "branch": branch,
            "clone_duration_ms": record.duration_ms,
            "protocol_v2": "version 2" in trace and "command=fetch" in trace,
            "clone_stdout_log": record.stdout_log,
            "clone_stderr_log": record.stderr_log,
        }

    def read_branch_tip(self, index: int) -> dict[str, Any]:
        branch = f"agents/agent-{index:03d}"
        target = self.branch_readers / f"reader-{index:03d}"
        result = self.protocol_v2_clone(branch, target, f"protocol v2 clone {branch}")
        path = target / "agents" / f"agent-{index:03d}.txt"
        expected = f"branch fanout agent {index}\nrun_id {self.run_id}\n"
        actual = path.read_text(encoding="utf-8") if path.is_file() else None
        result.update(
            {
                "agent": f"branch-agent-{index:03d}",
                "content_visible": actual == expected,
            }
        )
        return result

    def verify_branch_tip_reads(self) -> None:
        self.branch_readers.mkdir(parents=True, exist_ok=True)
        max_workers = max(1, min(self.args.agents, self.args.max_parallel_pushes))
        with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
            futures = [pool.submit(self.read_branch_tip, index) for index in range(self.args.agents)]
            results = [future.result() for future in concurrent.futures.as_completed(futures)]
        results.sort(key=lambda result: str(result["branch"]))
        with self.report_lock:
            self.report.branch_reads = results
            self.write_report()
        failed = [
            result
            for result in results
            if not result["protocol_v2"] or not result["content_visible"]
        ]
        self.check(
            "branch-fanout-protocol-v2-objects-visible",
            not failed and len(results) == self.args.agents,
            {
                "readers": len(results),
                "expected": self.args.agents,
                "failed": failed,
            },
        )

    def init_seed(self) -> None:
        self.seed.mkdir(parents=True)
        self.run_git(self.seed, ["init", "-b", "main"])
        self.configure_git_identity(self.seed, "seed")
        self.run_crab(self.seed, ["init", self.remote_url], name="crab init seed")
        self.configure_crab_repo(self.seed)
        (self.seed / "README.md").write_text(
            f"# Concurrent push smoke\n\nrun_id: {self.run_id}\n",
            encoding="utf-8",
        )
        self.run_git(self.seed, ["add", "-A"])
        self.run_git(self.seed, ["commit", "-m", "seed concurrent push smoke"])
        request_before = self.request_proxy.snapshot() if self.request_proxy else None
        self.run_crab(
            self.seed,
            [
                "push",
                "--json",
                "--lock-wait-secs",
                str(self.args.lock_wait_secs),
                "origin",
                "HEAD:refs/heads/main",
            ],
            name="crab push seed",
        )
        self.request_snapshot(
            "seed", request_before, attempted_pushes=1, successful_pushes=1
        )
        self.store_snapshot("seed", attempted_pushes=1, successful_pushes=1)

    def run_crash_boundary(self) -> None:
        if self.request_proxy is None:
            raise SmokeError("--crash-boundary requires HTTP request capture")
        self.run_cmd(
            "crab clone crash agent",
            [self.args.crab_bin, "clone", self.remote_url, str(self.crash_agent), "--jsonl"],
            self.run_root,
        )
        self.configure_git_identity(self.crash_agent, "crash-agent")
        self.configure_crab_repo(self.crash_agent)
        self.run_crab(
            self.crash_agent,
            [
                "config",
                "set",
                "push.lock_ttl_secs",
                str(self.args.crash_lock_ttl_secs),
            ],
        )
        branch = "crash-boundary"
        remote_ref = f"refs/heads/{branch}"
        refspec = f"HEAD:{remote_ref}"
        self.run_git(self.crash_agent, ["checkout", "-b", branch])
        payload = self.crash_agent / "crash-boundary.txt"
        payload.write_text(
            f"committed before process death\nrun_id {self.run_id}\n",
            encoding="utf-8",
        )
        self.run_git(self.crash_agent, ["add", payload.name])
        self.run_git(self.crash_agent, ["commit", "-m", "crash boundary first commit"])
        first_tip_record = self.run_git(
            self.crash_agent,
            ["rev-parse", "HEAD"],
            name="resolve crash-boundary first tip",
        )
        first_tip = Path(first_tip_record.stdout_log).read_text(encoding="utf-8").strip()

        request_before = self.request_proxy.snapshot()
        killed, killed_at = self.run_killed_after_active_marker(
            "crash-boundary push killed after active marker",
            self.push_args(refspec, lock_wait_secs=0),
            self.crash_agent,
        )
        advertised = self.run_git(
            self.seed,
            ["ls-remote", self.remote_url, remote_ref],
            name="git ls-remote after active-marker process death",
        )
        advertised_lines = Path(advertised.stdout_log).read_text(encoding="utf-8").splitlines()
        self.check(
            "crash-boundary-ref-readable-after-sigkill",
            f"{first_tip}\t{remote_ref}" in advertised_lines,
            {"ref": remote_ref, "tip": first_tip},
        )

        payload.write_text(
            f"committed before process death\nrecovered from visible commit\nrun_id {self.run_id}\n",
            encoding="utf-8",
        )
        self.run_git(self.crash_agent, ["add", payload.name])
        self.run_git(self.crash_agent, ["commit", "-m", "crash boundary recovery commit"])

        attempts: list[PushRecord] = []
        deadline = killed_at + self.args.crash_lock_ttl_secs + 30
        while True:
            attempt = self.run_push_job(
                f"crash-recovery-{len(attempts) + 1:03d}",
                remote_ref,
                self.crash_agent,
                refspec,
                lock_wait_secs=0,
            )
            attempts.append(attempt)
            if attempt.status == "ok" and attempt.command.exit_code == 0:
                break
            if attempt.status != "lock-contention":
                raise SmokeError(
                    "same-ref recovery returned unexpected status "
                    f"{attempt.status}; stderr log: {attempt.command.stderr_log}"
                )
            if time.monotonic() >= deadline:
                raise SmokeError(
                    "same-ref recovery did not acquire the expired lock within the RTO bound"
                )
            time.sleep(1)

        recovery_ms = int((time.monotonic() - killed_at) * 1000)
        self.check(
            "crash-boundary-committed-holder-reclaimed-before-expiry",
            attempts[0].status == "ok"
            and attempts[0].command.exit_code == 0
            and recovery_ms < self.args.crash_lock_ttl_secs * 1000,
            {
                "status": attempts[0].status,
                "exit_code": attempts[0].command.exit_code,
                "recovery_ms": recovery_ms,
                "lock_ttl_ms": self.args.crash_lock_ttl_secs * 1000,
            },
        )
        clone = self.protocol_v2_clone(
            branch,
            self.crash_reader,
            "protocol v2 clone after active-marker process death",
        )
        expected = (
            f"committed before process death\nrecovered from visible commit\nrun_id {self.run_id}\n"
        )
        actual = (self.crash_reader / payload.name).read_text(encoding="utf-8")
        self.check(
            "crash-boundary-restart-restores-v2-and-content",
            clone["protocol_v2"] and actual == expected,
            {"protocol_v2": clone["protocol_v2"], "content_visible": actual == expected},
        )
        with self.report_lock:
            self.report.crash_boundary = {
                "killed_command": asdict(killed),
                "durable_ref": remote_ref,
                "durable_tip": first_tip,
                "lock_ttl_secs": self.args.crash_lock_ttl_secs,
                "recovery_ms": recovery_ms,
                "attempts": [asdict(attempt) for attempt in attempts],
                "clone": clone,
            }
            self.write_report()
        self.request_snapshot(
            "crash-boundary",
            request_before,
            attempted_pushes=1 + len(attempts),
            successful_pushes=1,
        )
        self.store_snapshot(
            "crash-boundary",
            attempted_pushes=1 + len(attempts),
            successful_pushes=1,
        )

    def clone_agent(self, root: Path, index: int, prefix: str) -> Path:
        root.mkdir(parents=True, exist_ok=True)
        target = root / f"{prefix}-{index:03d}"
        self.run_cmd(
            f"crab clone {prefix}-{index:03d}",
            [self.args.crab_bin, "clone", self.remote_url, str(target), "--jsonl"],
            self.run_root,
        )
        self.configure_git_identity(target, f"{prefix}-{index:03d}")
        self.configure_crab_repo(target)
        return target

    def prepare_branch_agent(self, index: int) -> tuple[str, str, Path]:
        repo = self.clone_agent(self.branch_agents, index, "branch-agent")
        branch = f"agents/agent-{index:03d}"
        dst = f"refs/heads/{branch}"
        self.run_git(repo, ["checkout", "-b", branch])
        path = repo / "agents" / f"agent-{index:03d}.txt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            f"branch fanout agent {index}\nrun_id {self.run_id}\n",
            encoding="utf-8",
        )
        self.run_git(repo, ["add", str(path.relative_to(repo))])
        self.run_git(repo, ["commit", "-m", f"agent {index:03d} branch fanout"])
        return f"branch-agent-{index:03d}", dst, repo

    def prepare_same_branch_agent(self, index: int) -> tuple[str, str, Path]:
        repo = self.clone_agent(self.same_agents, index, "same-agent")
        path = repo / "same-branch" / f"agent-{index:03d}.txt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            f"same branch agent {index}\nrun_id {self.run_id}\n",
            encoding="utf-8",
        )
        self.run_git(repo, ["add", str(path.relative_to(repo))])
        self.run_git(repo, ["commit", "-m", f"agent {index:03d} same branch"])
        return f"same-agent-{index:03d}", "refs/heads/main", repo

    def push_args(self, refspec: str, lock_wait_secs: int | None = None) -> list[str]:
        args = [
            self.args.crab_bin,
            "push",
            "--json",
            "--manifest-cas-retries",
            str(self.args.manifest_cas_retries),
            "--upload-concurrency",
            str(self.args.upload_concurrency),
            "origin",
            refspec,
        ]
        if lock_wait_secs is not None or not self.args.omit_lock_wait_secs:
            wait_secs = self.args.lock_wait_secs if lock_wait_secs is None else lock_wait_secs
            args[3:3] = ["--lock-wait-secs", str(wait_secs)]
        if self.args.rebase_on_non_fast_forward:
            args.extend(
                [
                    "--rebase-on-non-fast-forward",
                    "--rebase-retry-limit",
                    str(self.args.rebase_retry_limit),
                ]
            )
        return args

    def run_push_job(
        self,
        agent: str,
        branch: str,
        repo: Path,
        refspec: str,
        lock_wait_secs: int | None = None,
    ) -> PushRecord:
        record = self.run_cmd(
            f"{agent} crab push",
            self.push_args(refspec, lock_wait_secs),
            repo,
            check=False,
            timeout=self.args.push_timeout,
        )
        payload = first_json_object(Path(record.stdout_log).read_text(encoding="utf-8"), "push")
        status = "missing-json"
        retryable = None
        retry_after_secs = None
        integration_retries = None
        integration_retry_limit = None
        if payload and payload.get("data"):
            integration_retries = payload["data"].get("integration_retries")
            integration_retry_limit = payload["data"].get("integration_retry_limit")
            refs = payload["data"].get("refs") or []
            if refs:
                status = str(refs[0].get("status"))
                retryable = refs[0].get("retryable")
                retry_after_secs = refs[0].get("retry_after_secs")
        elif record.exit_code == 0:
            status = "ok"
        return PushRecord(
            agent,
            branch,
            record,
            status,
            retryable,
            retry_after_secs,
            integration_retries,
            integration_retry_limit,
        )

    def push_concurrently(self, jobs: list[tuple[str, str, Path, str]]) -> list[PushRecord]:
        max_workers = max(1, min(len(jobs), self.args.max_parallel_pushes))
        with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
            futures = [
                pool.submit(self.run_push_job, agent, branch, repo, refspec)
                for agent, branch, repo, refspec in jobs
            ]
            return [future.result() for future in concurrent.futures.as_completed(futures)]

    def run_branch_fanout(self) -> None:
        prepared = [self.prepare_branch_agent(i) for i in range(self.args.agents)]
        jobs = [
            (agent, branch, repo, f"HEAD:{branch}")
            for agent, branch, repo in prepared
        ]
        request_before = self.request_proxy.snapshot() if self.request_proxy else None
        results = self.push_concurrently(jobs)
        self.request_snapshot(
            "branch-fanout",
            request_before,
            attempted_pushes=len(results),
            successful_pushes=sum(result.status == "ok" for result in results),
        )
        with self.report_lock:
            self.report.branch_fanout = [asdict(result) for result in results]
            self.write_report()

        statuses = {result.status for result in results}
        self.check(
            "branch-fanout-all-pushed",
            statuses == {"ok"},
            {"statuses": sorted(statuses), "count": len(results)},
        )
        refs = self.run_git(
            self.seed,
            ["ls-remote", self.remote_url, "refs/heads/agents/*"],
            name="git ls-remote branch fanout refs",
        )
        visible = [
            line
            for line in Path(refs.stdout_log).read_text(encoding="utf-8").splitlines()
            if "refs/heads/agents/" in line
        ]
        self.check(
            "branch-fanout-refs-visible",
            len(visible) >= self.args.agents,
            {"visible": len(visible), "expected": self.args.agents},
        )
        self.verify_branch_tip_reads()
        self.store_snapshot(
            "branch-fanout",
            attempted_pushes=len(results),
            successful_pushes=len(results),
        )

    def run_same_branch_contention(self) -> None:
        prepared = [self.prepare_same_branch_agent(i) for i in range(self.args.same_branch_agents)]
        jobs = [(agent, branch, repo, "HEAD:refs/heads/main") for agent, branch, repo in prepared]
        request_before = self.request_proxy.snapshot() if self.request_proxy else None
        results = self.push_concurrently(jobs)
        with self.report_lock:
            self.report.same_branch = [asdict(result) for result in results]
            self.write_report()

        ok = [result for result in results if result.status == "ok" and result.command.exit_code == 0]
        self.request_snapshot(
            "same-branch",
            request_before,
            attempted_pushes=len(results),
            successful_pushes=len(ok),
        )
        rejected = [result for result in results if result.status != "ok"]
        bad = [result for result in results if result.status in BAD_PUSH_STATUSES]
        if self.args.rebase_on_non_fast_forward:
            retry_counts = [
                result.integration_retries
                for result in results
                if result.integration_retries is not None
            ]
            telemetry_ok = all(
                result.integration_retries is not None
                and result.integration_retry_limit == self.args.rebase_retry_limit
                for result in results
            )
            self.check(
                "same-branch-all-integrated",
                len(ok) == self.args.same_branch_agents and not bad and telemetry_ok,
                {
                    "ok": len(ok),
                    "total": len(results),
                    "statuses": sorted({r.status for r in results}),
                    "telemetry_ok": telemetry_ok,
                    "max_integration_retries": max(retry_counts) if retry_counts else None,
                    "retry_limit": self.args.rebase_retry_limit,
                    "bad_statuses": [asdict(result) for result in bad],
                },
            )
            self.check_same_branch_files_visible(self.args.same_branch_agents)
            self.store_snapshot(
                "same-branch-integrated",
                attempted_pushes=len(results),
                successful_pushes=len(ok),
            )
            return

        self.check(
            "same-branch-one-winner",
            len(ok) == 1,
            {"ok": len(ok), "total": len(results), "statuses": sorted({r.status for r in results})},
        )
        self.check(
            "same-branch-losers-structured",
            len(rejected) == self.args.same_branch_agents - 1 and not bad,
            {
                "rejected": len(rejected),
                "bad_statuses": [asdict(result) for result in bad],
            },
        )
        self.check_same_branch_files_visible(1)
        self.store_snapshot(
            "same-branch-contention",
            attempted_pushes=len(results),
            successful_pushes=len(ok),
        )

    def check_same_branch_files_visible(self, expected_count: int) -> None:
        result = self.protocol_v2_clone(
            "main", self.same_branch_reader, "protocol v2 clone same-branch final"
        )
        visible = sorted((self.same_branch_reader / "same-branch").glob("agent-*.txt"))
        valid_contents = all(
            path.read_text(encoding="utf-8")
            == f"same branch agent {int(path.stem.removeprefix('agent-'))}\nrun_id {self.run_id}\n"
            for path in visible
        )
        result.update(
            {
                "visible_files": [path.name for path in visible],
                "content_valid": valid_contents,
            }
        )
        with self.report_lock:
            self.report.same_branch_read = result
            self.write_report()
        self.check(
            "same-branch-protocol-v2-objects-visible",
            result["protocol_v2"] and valid_contents and len(visible) == expected_count,
            {"visible": len(visible), "expected": expected_count, **result},
        )

    def run_fsck(self) -> None:
        if self.args.skip_fsck:
            return
        record = self.run_crab(self.seed, ["fsck", "--json"], name="crab fsck")
        payload = first_json_object(Path(record.stdout_log).read_text(encoding="utf-8"), "fsck")
        errors = None
        if payload and payload.get("data"):
            errors = payload["data"].get("errors")
        self.check("fsck-clean-or-no-errors", errors in (None, 0), {"errors": errors})

    def run(self) -> int:
        try:
            self.preflight()
            self.init_seed()
            if self.args.crash_boundary:
                self.run_crash_boundary()
            if not self.args.skip_branch_fanout:
                self.run_branch_fanout()
            if not self.args.skip_same_branch:
                self.run_same_branch_contention()
            self.run_fsck()
        except Exception as exc:
            with self.report_lock:
                self.report.status = "failed"
                self.report.checks.append(
                    {
                        "name": "exception",
                        "ok": False,
                        "detail": {"error": str(exc)},
                        "timestamp": utc_now(),
                    }
                )
                self.write_report()
            print(f"FAILED: {exc}")
            print(f"report: {self.report.artifacts.get('report')}")
            return 1
        else:
            with self.report_lock:
                self.report.status = "ok"
                self.write_report()
            print("OK")
            print(f"run: {self.run_root}")
            print(f"remote: {self.remote_url}")
            print(f"report: {self.report.artifacts.get('report')}")
            return 0
        finally:
            if self.request_proxy is not None:
                self.request_proxy.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--access-key", default="crab")
    parser.add_argument("--secret-key", default="crab")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--run-id")
    parser.add_argument("--crab-bin", default=shutil.which("crab") or "crab")
    parser.add_argument("--git-bin", default=shutil.which("git") or "git")
    parser.add_argument("--agents", type=int, default=8)
    parser.add_argument("--same-branch-agents", type=int, default=8)
    parser.add_argument("--max-parallel-pushes", type=int, default=32)
    parser.add_argument("--upload-concurrency", type=int, default=4)
    parser.add_argument("--lock-wait-secs", type=int, default=30)
    parser.add_argument("--omit-lock-wait-secs", action="store_true")
    parser.add_argument("--manifest-cas-retries", type=int, default=128)
    parser.add_argument("--rebase-on-non-fast-forward", action="store_true")
    parser.add_argument("--rebase-retry-limit", type=int, default=256)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--push-timeout", type=int, default=300)
    parser.add_argument("--crash-boundary", action="store_true")
    parser.add_argument("--crash-lock-ttl-secs", type=int, default=21)
    parser.add_argument("--skip-branch-fanout", action="store_true")
    parser.add_argument("--skip-same-branch", action="store_true")
    parser.add_argument("--skip-fsck", action="store_true")
    parser.add_argument("--no-request-capture", action="store_true")
    args = parser.parse_args()
    if args.crash_boundary and args.no_request_capture:
        parser.error("--crash-boundary requires request capture")
    if args.crash_lock_ttl_secs <= 20:
        parser.error("--crash-lock-ttl-secs must be greater than 20")
    args.crab_bin = resolve_executable(args.crab_bin)
    args.git_bin = resolve_executable(args.git_bin)
    return args


def resolve_executable(value: str) -> str:
    path = Path(value).expanduser()
    if path.is_absolute() or os.sep in value:
        return str(path.resolve())
    resolved = shutil.which(value)
    return resolved or value


def main() -> int:
    args = parse_args()
    smoke = ConcurrentPushSmoke(args)
    return smoke.run()


if __name__ == "__main__":
    raise SystemExit(main())
