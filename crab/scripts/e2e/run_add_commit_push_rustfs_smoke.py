#!/usr/bin/env python3
"""Run Crab add/commit/push smokes against a local RustFS/S3 endpoint.

The harness exercises both user-facing staging paths:

* ``crab add`` followed by ``crab push``
* ``git add`` followed by ``git push`` through ``git-remote-crab``

The large-file cases prove pointer staging, object-store publication, fresh
clone, hydration, and byte identity. A separate ordinary-Git matrix proves
missing/corrupt manifest handling, update/fetch, deletion, force and
non-fast-forward behavior, atomic rejection, tag following, shallow history,
immutable-pack failures, and concurrent push serialization.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import http.client
import json
import os
import shutil
import sqlite3
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


DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
DEFAULT_ROOT = Path(os.environ.get("TMPDIR", "/tmp")) / "crab-add-commit-push-smoke"
REPO_ROOT = Path(__file__).resolve().parents[3]
REMOTE_PREFIX = "e2e-add-commit-push"
POINTER_VERSION = "version https://crab.dev/spec/v1"
SECRET_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}
SECRET_FLAGS = {"--access-key", "--secret-key", "--session-token"}


class SmokeError(RuntimeError):
    """Raised when a smoke step fails."""


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
class PointerRecord:
    file_hash: str
    size: int
    byte_len: int
    has_shard_hint: bool


@dataclass
class CaseRecord:
    name: str
    remote_url: str
    repo_prefix: str
    file_size: int
    pointer: dict[str, Any]
    duplicate_pointer: dict[str, Any]
    staging_files: int
    new_xorbs: int
    new_shards: int
    original_sha256: str
    hydrated_sha256: str


@dataclass
class SmokeReport:
    run_id: str
    status: str
    root: str
    endpoint_url: str
    bucket: str
    env: dict[str, str]
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    cases: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    updated_at: str = ""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "add-commit-push-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


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


def credential_default(
    name: str, development_default: str, env: dict[str, str] | None = None
) -> str:
    source = os.environ if env is None else env
    value = source.get(name, "").strip()
    return value or development_default


def redact_command_args(args: list[str]) -> list[str]:
    redacted: list[str] = []
    redact_next = False
    for arg in args:
        if redact_next:
            redacted.append("<redacted>")
            redact_next = False
            continue
        if arg in SECRET_FLAGS:
            redacted.append(arg)
            redact_next = True
            continue
        flag, separator, _value = arg.partition("=")
        if separator and flag in SECRET_FLAGS:
            redacted.append(f"{flag}=<redacted>")
            continue
        redacted.append(arg)
    return redacted


def find_credential_leaks(
    sources: dict[str, str], credentials: dict[str, str]
) -> list[dict[str, str]]:
    """Return source/credential labels without copying secret values."""
    leaks: list[dict[str, str]] = []
    for credential, value in credentials.items():
        if not value or value == "crab":
            continue
        for source, text in sources.items():
            if value in text:
                leaks.append({"credential": credential, "source": source})
    return leaks


def redact_credential_text(text: str, credentials: dict[str, str]) -> str:
    for value in credentials.values():
        if value and value != "crab":
            text = text.replace(value, "<redacted>")
    return text


def redact_credential_value(value: Any, credentials: dict[str, str]) -> Any:
    if isinstance(value, str):
        return redact_credential_text(value, credentials)
    if isinstance(value, dict):
        return {
            key: redact_credential_value(item, credentials)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [redact_credential_value(item, credentials) for item in value]
    return value


def deterministic_bytes(size: int, seed: str) -> bytes:
    data = bytearray()
    counter = 0
    while len(data) < size:
        block = hashlib.sha256(f"{seed}:{counter}".encode("utf-8")).digest()
        data.extend(block)
        counter += 1
    return bytes(data[:size])


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_pointer(blob: str) -> PointerRecord:
    raw = blob.encode("utf-8")
    if len(raw) > 256:
        raise SmokeError(f"indexed blob is too large for a Crab pointer: {len(raw)} bytes")
    lines = blob.splitlines()
    if len(lines) not in (3, 4):
        raise SmokeError(f"indexed blob has {len(lines)} lines, expected a Crab pointer")
    if lines[0] != POINTER_VERSION:
        raise SmokeError(f"unexpected pointer version line: {lines[0]!r}")
    if not lines[1].startswith("file-hash "):
        raise SmokeError("pointer is missing file-hash line")
    file_hash = lines[1].removeprefix("file-hash ")
    if len(file_hash) != 64 or any(c not in "0123456789abcdefABCDEF" for c in file_hash):
        raise SmokeError(f"invalid pointer file hash: {file_hash!r}")
    if not lines[2].startswith("size "):
        raise SmokeError("pointer is missing size line")
    try:
        size = int(lines[2].removeprefix("size "))
    except ValueError as exc:
        raise SmokeError(f"invalid pointer size line: {lines[2]!r}") from exc
    has_shard_hint = len(lines) == 4 and lines[3].startswith("shard-hint ")
    return PointerRecord(
        file_hash=file_hash.lower(),
        size=size,
        byte_len=len(raw),
        has_shard_hint=has_shard_hint,
    )


class AddCommitPushSmoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        self.run_root = args.root / self.run_id
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.cache_dir = self.run_root / "cache"
        self.command_index = 0
        self.command_lock = threading.Lock()
        self.crab_bin = str(Path(shutil.which(args.crab_bin) or args.crab_bin).resolve())
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

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_DEFAULT_REGION": self.args.region,
                "AWS_ENDPOINT_URL": self.args.endpoint_url,
                "AWS_ENDPOINT_URL_S3": self.args.endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "CRAB_CACHE_DIR": str(self.cache_dir),
                "GIT_TERMINAL_PROMPT": "0",
            }
        )
        if self.args.session_token:
            env["AWS_SESSION_TOKEN"] = self.args.session_token
        else:
            env.pop("AWS_SESSION_TOKEN", None)
        helper_dir = str(Path(self.crab_bin).parent)
        env["PATH"] = helper_dir + os.pathsep + env.get("PATH", "")
        return env

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.report.updated_at = utc_now()
        path = self.artifacts / "report.json"
        payload = asdict(self.report)
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.report.artifacts["report"] = str(path)

    def credentials(self) -> dict[str, str]:
        return {
            "access_key": self.args.access_key,
            "secret_key": self.args.secret_key,
            "session_token": self.args.session_token,
        }

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report.checks.append(
            {
                "name": name,
                "ok": ok,
                "detail": redact_credential_value(detail or {}, self.credentials()),
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
        stdout = redact_credential_text(stdout, self.credentials())
        stderr = redact_credential_text(stderr, self.credentials())
        stdout_log.write_text(stdout, encoding="utf-8", errors="replace")
        stderr_log.write_text(stderr, encoding="utf-8", errors="replace")
        record = CommandRecord(
            name=name,
            args=redact_command_args(args),
            cwd=str(cwd),
            exit_code=exit_code,
            duration_ms=duration_ms,
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
        )
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
    ) -> CommandRecord:
        start = time.monotonic()
        try:
            proc = subprocess.run(
                args,
                cwd=cwd,
                env=self.env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout or self.args.timeout,
                check=False,
            )
            exit_code = proc.returncode
            stdout = proc.stdout
            stderr = proc.stderr
        except subprocess.TimeoutExpired as exc:
            exit_code = -124
            stdout = exc.stdout.decode("utf-8", errors="replace") if isinstance(exc.stdout, bytes) else (exc.stdout or "")
            stderr = exc.stderr.decode("utf-8", errors="replace") if isinstance(exc.stderr, bytes) else (exc.stderr or "")
            stderr += f"\ncommand timed out after {timeout or self.args.timeout} seconds\n"
        duration_ms = int((time.monotonic() - start) * 1000)
        record = self.record_command(name, args, cwd, exit_code, duration_ms, stdout, stderr)
        if check and record.exit_code != 0:
            raise SmokeError(
                f"{name} failed with exit {record.exit_code}; stderr log: {record.stderr_log}"
            )
        return record

    def run_git(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str | None = None,
        check: bool = True,
        timeout: int | None = None,
    ) -> CommandRecord:
        return self.run_cmd(
            name or "git " + " ".join(args),
            ["git", *args],
            repo,
            check=check,
            timeout=timeout,
        )

    def run_crab(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str | None = None,
        check: bool = True,
        timeout: int | None = None,
    ) -> CommandRecord:
        return self.run_cmd(
            name or "crab " + " ".join(args),
            [self.crab_bin, *args],
            repo,
            check=check,
            timeout=timeout,
        )

    def run_aws(
        self,
        name: str,
        args: list[str],
        *,
        check: bool = True,
    ) -> CommandRecord:
        return self.run_cmd(
            "aws " + name,
            ["aws", "s3api", *args, "--endpoint-url", self.args.endpoint_url],
            self.run_root,
            check=check,
        )

    def aws_json(self, name: str, args: list[str]) -> dict[str, Any]:
        record = self.run_aws(name, [*args, "--output", "json"])
        text = Path(record.stdout_log).read_text(encoding="utf-8", errors="replace").strip()
        if not text:
            return {}
        return json.loads(text)

    def list_keys(self, prefix: str) -> set[str]:
        payload = self.aws_json(
            f"list {prefix}",
            ["list-objects-v2", "--bucket", self.args.bucket, "--prefix", prefix],
        )
        return {item["Key"] for item in payload.get("Contents", [])}

    def head_key(self, key: str) -> None:
        self.run_aws("head " + key, ["head-object", "--bucket", self.args.bucket, "--key", key])

    def signed_s3_request(
        self,
        method: str,
        key: str,
        *,
        body: bytes = b"",
        extra_headers: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        endpoint = urllib.parse.urlparse(self.args.endpoint_url)
        if endpoint.scheme not in ("http", "https"):
            raise SmokeError(f"unsupported S3 endpoint scheme: {endpoint.scheme}")
        host = endpoint.netloc
        path = "/" + self.args.bucket + "/" + key.lstrip("/")
        canonical_uri = urllib.parse.quote(path, safe="/~")
        payload_hash = hashlib.sha256(body).hexdigest()
        now = datetime.now(timezone.utc)
        amz_date = now.strftime("%Y%m%dT%H%M%SZ")
        date_stamp = now.strftime("%Y%m%d")

        headers = {
            "host": host,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": amz_date,
        }
        if self.args.session_token:
            headers["x-amz-security-token"] = self.args.session_token
        for header, value in (extra_headers or {}).items():
            headers[header.lower()] = value

        canonical_headers = "".join(
            f"{name}:{' '.join(value.strip().split())}\n"
            for name, value in sorted(headers.items())
        )
        signed_headers = ";".join(name for name, _ in sorted(headers.items()))
        canonical_request = "\n".join(
            [
                method,
                canonical_uri,
                "",
                canonical_headers,
                signed_headers,
                payload_hash,
            ]
        )
        credential_scope = f"{date_stamp}/{self.args.region}/s3/aws4_request"
        string_to_sign = "\n".join(
            [
                "AWS4-HMAC-SHA256",
                amz_date,
                credential_scope,
                hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
            ]
        )
        signing_key = self.sigv4_signing_key(date_stamp)
        signature = hmac.new(
            signing_key, string_to_sign.encode("utf-8"), hashlib.sha256
        ).hexdigest()
        auth = (
            "AWS4-HMAC-SHA256 "
            f"Credential={self.args.access_key}/{credential_scope}, "
            f"SignedHeaders={signed_headers}, "
            f"Signature={signature}"
        )
        request_headers = {
            name: value for name, value in headers.items() if name != "host"
        }
        request_headers["Authorization"] = auth

        connection_cls = (
            http.client.HTTPSConnection if endpoint.scheme == "https" else http.client.HTTPConnection
        )
        conn = connection_cls(host, timeout=self.args.timeout)
        try:
            conn.request(method, canonical_uri, body=body, headers=request_headers)
            response = conn.getresponse()
            response_body = response.read()
            response_headers = {k.lower(): v for k, v in response.getheaders()}
            return response.status, response_headers, response_body
        finally:
            conn.close()

    def sigv4_signing_key(self, date_stamp: str) -> bytes:
        def sign(key: bytes, message: str) -> bytes:
            return hmac.new(key, message.encode("utf-8"), hashlib.sha256).digest()

        key = ("AWS4" + self.args.secret_key).encode("utf-8")
        date_key = sign(key, date_stamp)
        region_key = sign(date_key, self.args.region)
        service_key = sign(region_key, "s3")
        return sign(service_key, "aws4_request")

    def preflight(self) -> None:
        self.run_root.mkdir(parents=True, exist_ok=True)
        self.logs.mkdir(parents=True, exist_ok=True)
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.write_report()

        for binary in ("git", "aws", self.crab_bin):
            self.check(f"{binary}-available", shutil.which(binary) is not None)

        helper_bin = self.run_root / "bin" / "git-remote-crab"
        helper_bin.parent.mkdir(parents=True, exist_ok=True)
        if helper_bin.exists() or helper_bin.is_symlink():
            helper_bin.unlink()
        helper_bin.symlink_to(Path(self.crab_bin))
        self.env["PATH"] = str(helper_bin.parent) + os.pathsep + self.env.get("PATH", "")
        self.report.artifacts["crab_binary"] = self.crab_bin
        self.report.artifacts["crab_binary_sha256"] = sha256_file(Path(self.crab_bin))

        helper = shutil.which("git-remote-crab", path=self.env.get("PATH"))
        self.check("git-remote-crab-available", helper is not None)
        self.check(
            "git-remote-crab-targets-selected-binary",
            helper is not None and Path(helper).resolve() == Path(self.crab_bin),
            {"helper": helper, "crab_bin": self.crab_bin},
        )
        self.run_git(self.run_root, ["--version"], name="git version")
        self.run_crab(self.run_root, ["version", "--json"], name="crab version")
        source = self.run_git(
            REPO_ROOT, ["rev-parse", "HEAD"], name="Crab source revision"
        )
        self.report.artifacts["source_head_sha"] = self.read_stdout(source)
        source_status = self.run_git(
            REPO_ROOT,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            name="Crab source worktree status",
        )
        self.report.artifacts["source_worktree_dirty"] = str(
            bool(self.read_stdout(source_status))
        ).lower()
        rustfs_bin = shutil.which("rustfs")
        if rustfs_bin:
            self.run_cmd(
                "rustfs version",
                [rustfs_bin, "--version"],
                self.run_root,
                check=False,
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

        record = self.run_aws(
            "create bucket",
            ["create-bucket", "--bucket", self.args.bucket],
            check=False,
        )
        stderr = Path(record.stderr_log).read_text(encoding="utf-8", errors="replace")
        already_exists = "BucketAlready" in stderr or "already" in stderr.lower()
        self.check(
            "bucket-create-or-exists",
            record.exit_code == 0 or already_exists,
            {"exit_code": record.exit_code, "already_exists": already_exists},
        )
        self.check_conditional_put_contract()

    def check_credential_disclosure(self) -> None:
        sources = {
            "report": json.dumps(asdict(self.report), sort_keys=True),
        }
        for path in sorted(self.logs.glob("*.log")):
            sources[str(path)] = path.read_text(encoding="utf-8", errors="replace")
        leaks = find_credential_leaks(
            sources,
            self.credentials(),
        )
        self.check("credential-values-not-disclosed", not leaks, {"leaks": leaks})

    def check_conditional_put_contract(self) -> None:
        body = f"cas probe {self.run_id}\n".encode("utf-8")
        key = f"{REMOTE_PREFIX}/{self.run_id}/cas-probe"

        first_status, _, first_body = self.signed_s3_request(
            "PUT",
            key,
            body=body,
            extra_headers={"if-none-match": "*"},
        )
        self.check(
            "s3-if-none-match-create",
            first_status in (200, 201),
            {"status": first_status, "body": first_body.decode("utf-8", errors="replace")[:200]},
        )

        second_status, _, second_body = self.signed_s3_request(
            "PUT",
            key,
            body=body,
            extra_headers={"if-none-match": "*"},
        )
        self.check(
            "s3-if-none-match-conflict",
            second_status == 412,
            {"status": second_status, "body": second_body.decode("utf-8", errors="replace")[:200]},
        )

        head_status, head_headers, _ = self.signed_s3_request("HEAD", key)
        etag = head_headers.get("etag", "")
        self.check("s3-head-returned-etag", bool(etag))
        self.check("s3-head-status-ok", head_status == 200, {"status": head_status})

        update_status, _, update_body = self.signed_s3_request(
            "PUT",
            key,
            body=body,
            extra_headers={"if-match": etag},
        )
        self.check(
            "s3-if-match-update",
            update_status in (200, 201),
            {
                "status": update_status,
                "body": update_body.decode("utf-8", errors="replace")[:200],
            },
        )

        wrong_status, _, wrong_body = self.signed_s3_request(
            "PUT",
            key,
            body=body,
            extra_headers={"if-match": '"deadbeef00000000000000000000dead"'},
        )
        self.check(
            "s3-if-match-conflict",
            wrong_status == 412,
            {"status": wrong_status, "body": wrong_body.decode("utf-8", errors="replace")[:200]},
        )

    def configure_git_identity(self, repo: Path, who: str) -> None:
        self.run_git(repo, ["config", "user.name", f"Crab {who}"])
        self.run_git(repo, ["config", "user.email", f"{who}@example.invalid"])

    def staging_file_count(self, repo: Path) -> int:
        staging = repo / ".crab" / "staging"
        if not staging.exists():
            return 0
        return sum(1 for path in staging.rglob("*") if path.is_file() and path.stat().st_size > 0)

    def staging_payload_inventory(self, repo: Path) -> dict[str, int]:
        index_path = repo / ".crab" / "staging" / "index.db"
        if not index_path.is_file():
            raise SmokeError(f"staging index does not exist: {index_path}")
        connection = sqlite3.connect(f"file:{index_path}?mode=ro", uri=True)
        try:
            tables = (
                "chunks",
                "chunk_payloads",
                "prepared_xorbs",
                "prepared_payloads",
                "recipe_remote_chunks",
            )
            return {
                table: int(
                    connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                )
                for table in tables
            }
        finally:
            connection.close()

    def sqlite_cache_files(self) -> list[str]:
        return sorted(path.name for path in self.cache_dir.rglob("*.sqlite"))

    def redb_cache_files(self) -> list[str]:
        return sorted(str(path.relative_to(self.cache_dir)) for path in self.cache_dir.rglob("*.redb"))

    def remote_for_case(self, case_name: str) -> tuple[str, str]:
        repo_prefix = f"{REMOTE_PREFIX}/{self.run_id}/{case_name}"
        return f"crab://{self.args.bucket}/{repo_prefix}", repo_prefix

    def prepare_repo(self, case_name: str) -> tuple[Path, str, str]:
        case_root = self.run_root / case_name
        repo = case_root / "repo"
        repo.mkdir(parents=True)
        remote_url, repo_prefix = self.remote_for_case(case_name)
        self.run_git(repo, ["init", "-b", "main"])
        self.configure_git_identity(repo, case_name)
        self.run_crab(
            repo, ["init", remote_url], name=f"{case_name} crab init"
        )
        self.run_crab(repo, ["track", "*.bin"], name=f"{case_name} crab track")
        self.run_git(repo, ["add", "crab.toml", ".gitattributes"], name=f"{case_name} add config")
        return repo, remote_url, repo_prefix

    def prepare_git_repo(self, case_name: str) -> tuple[Path, str, str]:
        case_root = self.run_root / case_name
        repo = case_root / "repo"
        repo.mkdir(parents=True)
        remote_url, repo_prefix = self.remote_for_case(case_name)
        self.run_git(repo, ["init", "-b", "main"])
        self.configure_git_identity(repo, case_name)
        self.run_crab(
            repo, ["init", remote_url], name=f"{case_name} crab init"
        )
        return repo, remote_url, repo_prefix

    @staticmethod
    def read_stdout(record: CommandRecord) -> str:
        return Path(record.stdout_log).read_text(
            encoding="utf-8", errors="replace"
        ).strip()

    def rev_parse(self, repo: Path, revision: str) -> str:
        return self.read_stdout(
            self.run_git(repo, ["rev-parse", revision], name=f"rev-parse {revision}")
        )

    def commit_text(self, repo: Path, path: str, content: str, message: str) -> str:
        (repo / path).write_text(content, encoding="utf-8")
        self.run_git(repo, ["add", path], name=f"add {path}")
        self.run_git(repo, ["commit", "-m", message], name=f"commit {message}")
        return self.rev_parse(repo, "HEAD")

    def ls_remote(self, remote_url: str, *, name: str) -> dict[str, str]:
        record = self.run_git(
            self.run_root, ["ls-remote", remote_url], name=name
        )
        refs: dict[str, str] = {}
        for line in self.read_stdout(record).splitlines():
            if not line.strip():
                continue
            sha, ref_name = line.split(maxsplit=1)
            refs[ref_name] = sha
        return refs

    def clone_git(
        self,
        remote_url: str,
        destination: Path,
        *,
        name: str,
        extra_args: list[str] | None = None,
        check: bool = True,
    ) -> CommandRecord:
        return self.run_git(
            self.run_root,
            ["clone", *(extra_args or []), remote_url, str(destination)],
            name=name,
            check=check,
        )

    def run_missing_manifest_case(self) -> None:
        case_name = "missing-manifest"
        _repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        self.run_aws(
            "delete canonical manifest fixture",
            [
                "delete-object",
                "--bucket",
                self.args.bucket,
                "--key",
                f"{repo_prefix}/manifest",
            ],
        )
        listing = self.run_git(
            self.run_root,
            ["ls-remote", remote_url],
            name=f"{case_name} git ls-remote",
            check=False,
        )
        self.check(
            f"{case_name}-list-fails-closed",
            listing.exit_code != 0,
            {"repo_prefix": repo_prefix, "exit_code": listing.exit_code},
        )
        clone_dir = self.run_root / case_name / "clone"
        clone = self.clone_git(
            remote_url, clone_dir, name=f"{case_name} git clone", check=False
        )
        self.check(
            f"{case_name}-clone-fails-closed",
            clone.exit_code != 0,
            {"exit_code": clone.exit_code},
        )

    def run_ref_update_delete_force_and_tag_case(self) -> None:
        case_name = "git-ref-matrix"
        repo, remote_url, _repo_prefix = self.prepare_git_repo(case_name)
        first = self.commit_text(repo, "state.txt", "one\n", "first")
        self.run_git(repo, ["push", "-u", "origin", "main"])

        existing = self.run_root / case_name / "existing-clone"
        self.clone_git(remote_url, existing, name=f"{case_name} initial clone")
        self.configure_git_identity(existing, f"{case_name}-existing")

        second = self.commit_text(repo, "state.txt", "two\n", "second")
        self.run_git(repo, ["push", "origin", "main"])
        self.run_git(existing, ["fetch", "origin"], name=f"{case_name} fetch update")
        self.check(
            f"{case_name}-existing-clone-observes-update",
            self.rev_parse(existing, "refs/remotes/origin/main") == second,
            {"first": first, "second": second},
        )

        self.run_git(repo, ["branch", "obsolete"])
        self.run_git(repo, ["push", "origin", "obsolete"])
        self.run_git(repo, ["push", "origin", ":refs/heads/obsolete"])
        refs_after_delete = self.ls_remote(
            remote_url, name=f"{case_name} list after deletion"
        )
        self.check(
            f"{case_name}-deleted-branch-is-invisible",
            "refs/heads/obsolete" not in refs_after_delete,
            {"refs": refs_after_delete},
        )

        contender = self.run_root / case_name / "contender"
        self.clone_git(remote_url, contender, name=f"{case_name} contender clone")
        self.configure_git_identity(contender, f"{case_name}-contender")
        deleted_ref = self.run_git(
            contender,
            ["show-ref", "--verify", "refs/remotes/origin/obsolete"],
            name=f"{case_name} fresh clone deleted ref check",
            check=False,
        )
        self.check(
            f"{case_name}-fresh-clone-omits-deleted-branch",
            deleted_ref.exit_code != 0,
            {"exit_code": deleted_ref.exit_code},
        )
        third = self.commit_text(repo, "state.txt", "three\n", "third")
        self.run_git(repo, ["push", "origin", "main"])
        divergent = self.commit_text(
            contender, "contender.txt", "divergent\n", "divergent"
        )
        rejected = self.run_git(
            contender,
            ["push", "origin", "main"],
            name=f"{case_name} non-fast-forward push",
            check=False,
        )
        self.check(
            f"{case_name}-non-fast-forward-rejected",
            rejected.exit_code != 0,
            {"exit_code": rejected.exit_code, "remote_tip": third},
        )
        self.check(
            f"{case_name}-rejected-push-preserves-remote-tip",
            self.ls_remote(remote_url, name=f"{case_name} list after rejection").get(
                "refs/heads/main"
            )
            == third,
            {"expected": third},
        )

        self.run_git(contender, ["push", "--force", "origin", "main"])
        self.check(
            f"{case_name}-force-push-updates-tip",
            self.ls_remote(remote_url, name=f"{case_name} list after force").get(
                "refs/heads/main"
            )
            == divergent,
            {"expected": divergent},
        )

        self.run_git(contender, ["tag", "-a", "v1", "-m", "version one"])
        tag_oid = self.rev_parse(contender, "refs/tags/v1")
        self.run_git(contender, ["push", "--follow-tags", "origin", "main"])
        refs_after_tag = self.ls_remote(
            remote_url, name=f"{case_name} list after follow-tags"
        )
        self.check(
            f"{case_name}-follow-tags-publishes-tag-object-sha",
            refs_after_tag.get("refs/tags/v1") == tag_oid,
            {"expected": tag_oid, "actual": refs_after_tag.get("refs/tags/v1")},
        )
        self.run_git(contender, ["tag", "-a", "v2", "-m", "version two"])
        native_tag_oid = self.rev_parse(contender, "refs/tags/v2")
        self.run_crab(
            contender,
            ["push", "--follow-tags", "origin", "main"],
            name=f"{case_name} native crab follow-tags push",
        )
        refs_after_native_tag = self.ls_remote(
            remote_url, name=f"{case_name} list after native follow-tags"
        )
        self.check(
            f"{case_name}-native-follow-tags-publishes-tag-object-sha",
            refs_after_native_tag.get("refs/tags/v2") == native_tag_oid,
            {
                "expected": native_tag_oid,
                "actual": refs_after_native_tag.get("refs/tags/v2"),
            },
        )
        tag_clone = self.run_root / case_name / "tag-clone"
        self.clone_git(remote_url, tag_clone, name=f"{case_name} tag clone")
        tag_type = self.read_stdout(
            self.run_git(tag_clone, ["cat-file", "-t", tag_oid], name="verify tag object")
        )
        self.check(
            f"{case_name}-fresh-clone-has-annotated-tag-object",
            tag_type == "tag" and self.rev_parse(tag_clone, "refs/tags/v1") == tag_oid,
            {"tag_type": tag_type, "tag_oid": tag_oid},
        )
        self.run_git(
            tag_clone,
            ["fsck", "--connectivity-only"],
            name=f"{case_name} connectivity fsck",
        )

    def run_atomic_rejection_case(self) -> None:
        case_name = "atomic-rejection"
        repo, remote_url, _repo_prefix = self.prepare_git_repo(case_name)
        base = self.commit_text(repo, "base.txt", "base\n", "base")
        self.run_git(repo, ["branch", "stable"])
        self.run_git(repo, ["push", "origin", "main", "stable"])

        updater = self.run_root / case_name / "stable-updater"
        self.clone_git(remote_url, updater, name=f"{case_name} updater clone")
        self.configure_git_identity(updater, f"{case_name}-updater")
        self.run_git(updater, ["checkout", "stable"])
        stable_tip = self.commit_text(updater, "stable.txt", "advanced\n", "advance stable")
        self.run_git(updater, ["push", "origin", "stable"])

        main_tip = self.commit_text(repo, "main.txt", "candidate\n", "advance main")
        before = self.ls_remote(remote_url, name=f"{case_name} refs before atomic")
        rejected = self.run_git(
            repo,
            ["push", "--atomic", "origin", "main", "stable"],
            name=f"{case_name} atomic push",
            check=False,
        )
        after = self.ls_remote(remote_url, name=f"{case_name} refs after atomic")
        self.check(
            f"{case_name}-batch-rejected",
            rejected.exit_code != 0,
            {"exit_code": rejected.exit_code},
        )
        self.check(
            f"{case_name}-neither-ref-changed",
            after == before
            and after.get("refs/heads/main") == base
            and after.get("refs/heads/stable") == stable_tip,
            {
                "before": before,
                "after": after,
                "rejected_main_tip": main_tip,
            },
        )

    def run_shallow_case(self) -> None:
        case_name = "shallow-history"
        repo, remote_url, _repo_prefix = self.prepare_git_repo(case_name)
        first = self.commit_text(repo, "history.txt", "one\n", "one")
        self.commit_text(repo, "history.txt", "two\n", "two")
        self.commit_text(repo, "history.txt", "three\n", "three")
        self.run_git(repo, ["push", "-u", "origin", "main"])

        selector_clone = self.run_root / case_name / "shallow-since-clone"
        selector_result = self.clone_git(
            remote_url,
            selector_clone,
            name=f"{case_name} unsupported shallow-since clone",
            extra_args=["--shallow-since", "2100-01-01T00:00:00Z"],
            check=False,
        )
        selector_stderr = Path(selector_result.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        self.check(
            f"{case_name}-shallow-since-fails-explicitly",
            selector_result.exit_code != 0
            and "deepen-since is not supported" in selector_stderr
            and not (selector_clone / "history.txt").exists(),
            {
                "exit_code": selector_result.exit_code,
                "stderr_log": selector_result.stderr_log,
            },
        )
        exclude_clone = self.run_root / case_name / "shallow-exclude-clone"
        exclude_result = self.clone_git(
            remote_url,
            exclude_clone,
            name=f"{case_name} unsupported shallow-exclude clone",
            extra_args=["--shallow-exclude", "main"],
            check=False,
        )
        exclude_stderr = Path(exclude_result.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        self.check(
            f"{case_name}-shallow-exclude-fails-explicitly",
            exclude_result.exit_code != 0
            and "deepen-not is not supported" in exclude_stderr
            and not (exclude_clone / "history.txt").exists(),
            {
                "exit_code": exclude_result.exit_code,
                "stderr_log": exclude_result.stderr_log,
            },
        )

        raw_fetch = self.run_root / case_name / "raw-fetch"
        raw_fetch.mkdir()
        self.run_git(raw_fetch, ["init", "-b", "main"])
        self.run_crab(raw_fetch, ["init", remote_url], name=f"{case_name} raw fetch init")
        denied = self.run_git(
            raw_fetch,
            ["fetch", "origin", first],
            name=f"{case_name} raw reachable SHA denied by default",
            check=False,
        )
        self.check(
            f"{case_name}-raw-reachable-sha-denied-by-default",
            denied.exit_code != 0,
            {"exit_code": denied.exit_code, "sha": first},
        )
        with (raw_fetch / ".crab" / "local.toml").open("a", encoding="utf-8") as config:
            config.write("\n[uploadpack]\nallow_reachable_sha_in_want = true\n")
        self.run_git(
            raw_fetch,
            ["fetch", "origin", first],
            name=f"{case_name} raw reachable SHA admitted",
        )
        fetched_commit = self.run_git(
            raw_fetch,
            ["cat-file", "-e", f"{first}^{{commit}}"],
            name=f"{case_name} verify raw reachable commit",
            check=False,
        )
        self.check(
            f"{case_name}-raw-reachable-sha-uses-published-graph",
            fetched_commit.exit_code == 0,
            {"exit_code": fetched_commit.exit_code, "sha": first},
        )

        shallow = self.run_root / case_name / "clone"
        self.clone_git(
            remote_url,
            shallow,
            name=f"{case_name} depth-one clone",
            extra_args=["--depth", "1"],
        )
        depth_one = int(
            self.read_stdout(
                self.run_git(shallow, ["rev-list", "--count", "HEAD"], name="depth one count")
            )
        )
        self.check(
            f"{case_name}-depth-one-boundary",
            depth_one == 1 and (shallow / ".git" / "shallow").is_file(),
            {"commit_count": depth_one},
        )
        self.run_git(shallow, ["fetch", "--deepen", "1", "origin"])
        depth_two = int(
            self.read_stdout(
                self.run_git(shallow, ["rev-list", "--count", "HEAD"], name="depth two count")
            )
        )
        self.check(
            f"{case_name}-deepen-adds-history",
            depth_two == 2,
            {"commit_count": depth_two},
        )
        self.run_git(shallow, ["fetch", "--unshallow", "origin"])
        full_depth = int(
            self.read_stdout(
                self.run_git(shallow, ["rev-list", "--count", "HEAD"], name="full depth count")
            )
        )
        self.check(
            f"{case_name}-unshallow-restores-full-history",
            full_depth == 3 and not (shallow / ".git" / "shallow").exists(),
            {"commit_count": full_depth},
        )

    def run_corrupt_manifest_case(self) -> None:
        case_name = "corrupt-manifest"
        remote_url, repo_prefix = self.remote_for_case(case_name)
        manifest_key = f"{repo_prefix}/manifest"
        status, _, response_body = self.signed_s3_request(
            "PUT",
            manifest_key,
            body=b'{"version":',
        )
        self.check(
            f"{case_name}-fixture-written",
            status in (200, 201),
            {
                "status": status,
                "repo_prefix": repo_prefix,
                "body": response_body.decode("utf-8", errors="replace")[:200],
            },
        )

        ls_remote = self.run_git(
            self.run_root,
            ["ls-remote", remote_url],
            name=f"{case_name} git ls-remote",
            check=False,
        )
        ls_remote_stderr = Path(ls_remote.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        self.check(
            f"{case_name}-ls-remote-fails",
            ls_remote.exit_code != 0,
            {"exit_code": ls_remote.exit_code, "remote_url": remote_url},
        )
        self.check(
            f"{case_name}-ls-remote-surfaces-integrity-error",
            "CRAB-E0020" in ls_remote_stderr or "corrupt object" in ls_remote_stderr.lower(),
            {"stderr_log": ls_remote.stderr_log},
        )

        clone_dir = self.run_root / case_name / "clone"
        clone = self.run_git(
            self.run_root,
            ["clone", remote_url, str(clone_dir)],
            name=f"{case_name} git clone",
            check=False,
        )
        clone_stderr = Path(clone.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        self.check(
            f"{case_name}-clone-fails",
            clone.exit_code != 0,
            {"exit_code": clone.exit_code, "remote_url": remote_url},
        )
        self.check(
            f"{case_name}-clone-surfaces-integrity-error",
            "CRAB-E0020" in clone_stderr or "corrupt object" in clone_stderr.lower(),
            {"stderr_log": clone.stderr_log},
        )

    def run_immutable_object_failure_case(self, object_kind: str) -> None:
        case_name = f"missing-{object_kind}"
        repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        remote_tip = self.commit_text(repo, "payload.txt", "immutable\n", "immutable")
        self.run_git(repo, ["push", "-u", "origin", "main"])
        keys = self.list_keys(repo_prefix)
        suffix = f".{object_kind}"
        candidates = sorted(key for key in keys if key.endswith(suffix))
        self.check(
            f"{case_name}-fixture-object-found",
            bool(candidates),
            {"suffix": suffix, "keys": sorted(keys)},
        )
        target_key = candidates[0]
        status, _, body = self.signed_s3_request("DELETE", target_key)
        self.check(
            f"{case_name}-fixture-object-deleted",
            status in (200, 204),
            {
                "status": status,
                "key": target_key,
                "body": body.decode("utf-8", errors="replace")[:200],
            },
        )

        clone_dir = self.run_root / case_name / "clone"
        clone = self.clone_git(
            remote_url, clone_dir, name=f"{case_name} git clone", check=False
        )
        head = self.run_git(
            self.run_root,
            ["-C", str(clone_dir), "rev-parse", "--verify", "HEAD"],
            name=f"{case_name} verify no local ref update",
            check=False,
        )
        self.check(
            f"{case_name}-clone-fails-before-checkout",
            clone.exit_code != 0
            and head.exit_code != 0
            and not (clone_dir / "payload.txt").exists(),
            {
                "clone_exit_code": clone.exit_code,
                "head_exit_code": head.exit_code,
                "target_key": target_key,
            },
        )
        refs = self.ls_remote(remote_url, name=f"{case_name} refs remain coherent")
        self.check(
            f"{case_name}-remote-ref-remains-original",
            refs.get("refs/heads/main") == remote_tip,
            {"expected": remote_tip, "actual": refs.get("refs/heads/main")},
        )

    def run_corrupt_index_failure_case(self) -> None:
        case_name = "corrupt-idx"
        repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        remote_tip = self.commit_text(repo, "payload.txt", "immutable\n", "immutable")
        self.run_git(repo, ["push", "-u", "origin", "main"])
        candidates = sorted(
            key for key in self.list_keys(repo_prefix) if key.endswith(".idx")
        )
        self.check(
            f"{case_name}-fixture-object-found",
            bool(candidates),
            {"suffix": ".idx", "keys": candidates},
        )
        target_key = candidates[0]
        status, _, body = self.signed_s3_request(
            "PUT", target_key, body=b"not a valid git pack index\n"
        )
        self.check(
            f"{case_name}-fixture-object-corrupted",
            status in (200, 201),
            {
                "status": status,
                "key": target_key,
                "body": body.decode("utf-8", errors="replace")[:200],
            },
        )

        clone_dir = self.run_root / case_name / "clone"
        clone = self.clone_git(
            remote_url, clone_dir, name=f"{case_name} git clone", check=False
        )
        head = self.run_git(
            self.run_root,
            ["-C", str(clone_dir), "rev-parse", "--verify", "HEAD"],
            name=f"{case_name} verify no local ref update",
            check=False,
        )
        clone_stderr = Path(clone.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        self.check(
            f"{case_name}-clone-fails-before-checkout",
            clone.exit_code != 0
            and head.exit_code != 0
            and not (clone_dir / "payload.txt").exists(),
            {
                "clone_exit_code": clone.exit_code,
                "head_exit_code": head.exit_code,
                "target_key": target_key,
            },
        )
        self.check(
            f"{case_name}-clone-surfaces-integrity-error",
            "CRAB-E0020" in clone_stderr
            or "corrupt object" in clone_stderr.lower(),
            {"stderr_log": clone.stderr_log},
        )
        refs = self.ls_remote(remote_url, name=f"{case_name} refs remain coherent")
        self.check(
            f"{case_name}-remote-ref-remains-original",
            refs.get("refs/heads/main") == remote_tip,
            {"expected": remote_tip, "actual": refs.get("refs/heads/main")},
        )

    def run_concurrent_push_case(self) -> None:
        case_name = "concurrent-cas"
        repo, remote_url, _repo_prefix = self.prepare_git_repo(case_name)
        self.commit_text(repo, "base.txt", "base\n", "base")
        self.run_git(repo, ["push", "-u", "origin", "main"])

        left = self.run_root / case_name / "left"
        right = self.run_root / case_name / "right"
        self.clone_git(remote_url, left, name=f"{case_name} left clone")
        self.clone_git(remote_url, right, name=f"{case_name} right clone")
        self.configure_git_identity(left, f"{case_name}-left")
        self.configure_git_identity(right, f"{case_name}-right")
        left_tip = self.commit_text(left, "left.txt", "left\n", "left")
        right_tip = self.commit_text(right, "right.txt", "right\n", "right")

        barrier = threading.Barrier(3)
        results: dict[str, tuple[int, int, str, str]] = {}

        def push(side: str, cwd: Path) -> None:
            barrier.wait()
            start = time.monotonic()
            try:
                proc = subprocess.run(
                    ["git", "push", "origin", "main"],
                    cwd=cwd,
                    env=self.env,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=self.args.push_timeout,
                    check=False,
                )
                result = (proc.returncode, proc.stdout, proc.stderr)
            except subprocess.TimeoutExpired as exc:
                stdout = (
                    exc.stdout.decode("utf-8", errors="replace")
                    if isinstance(exc.stdout, bytes)
                    else (exc.stdout or "")
                )
                stderr = (
                    exc.stderr.decode("utf-8", errors="replace")
                    if isinstance(exc.stderr, bytes)
                    else (exc.stderr or "")
                )
                stderr += (
                    f"\ncommand timed out after {self.args.push_timeout} seconds\n"
                )
                result = (-124, stdout, stderr)
            results[side] = (
                result[0],
                int((time.monotonic() - start) * 1000),
                result[1],
                result[2],
            )

        threads = [
            threading.Thread(target=push, args=("left", left), daemon=True),
            threading.Thread(target=push, args=("right", right), daemon=True),
        ]
        for thread in threads:
            thread.start()
        barrier.wait()
        for thread in threads:
            thread.join(timeout=self.args.push_timeout + 5)
        self.check(
            f"{case_name}-both-pushes-finished",
            all(not thread.is_alive() for thread in threads) and len(results) == 2,
            {"result_sides": sorted(results)},
        )
        records: dict[str, CommandRecord] = {}
        for side, (exit_code, duration_ms, stdout, stderr) in sorted(results.items()):
            records[side] = self.record_command(
                f"{case_name} {side} push",
                ["git", "push", "origin", "main"],
                left if side == "left" else right,
                exit_code,
                duration_ms,
                stdout,
                stderr,
            )
        winners = [side for side, record in records.items() if record.exit_code == 0]
        self.check(
            f"{case_name}-exactly-one-winner",
            len(winners) == 1,
            {side: record.exit_code for side, record in records.items()},
        )
        winning_tip = left_tip if winners[0] == "left" else right_tip
        refs = self.ls_remote(remote_url, name=f"{case_name} winning refs")
        self.check(
            f"{case_name}-manifest-has-coherent-winning-tip",
            refs.get("refs/heads/main") == winning_tip,
            {"expected": winning_tip, "actual": refs.get("refs/heads/main")},
        )
        clone_dir = self.run_root / case_name / "winner-clone"
        self.clone_git(remote_url, clone_dir, name=f"{case_name} winner clone")
        self.run_git(
            clone_dir,
            ["fsck", "--connectivity-only"],
            name=f"{case_name} winner fsck",
        )

    def assert_index_pointer(self, repo: Path, path: str, expected_size: int) -> PointerRecord:
        record = self.run_git(repo, ["show", f":{path}"], name=f"git show pointer {path}")
        blob = Path(record.stdout_log).read_text(encoding="utf-8", errors="replace")
        pointer = parse_pointer(blob)
        self.check(
            f"{path}-indexed-as-crab-pointer",
            pointer.size == expected_size,
            {"pointer": asdict(pointer), "expected_size": expected_size},
        )
        return pointer

    def run_v1_hard_cutover_reset_case(self) -> None:
        case_name = "v1-hard-cutover-reset"
        remote_url, repo_prefix = self.remote_for_case(case_name)
        probe = self.run_root / "v1-reset-probe"
        probe.mkdir(parents=True)
        self.run_git(probe, ["init", "-b", "main"], name="v1 reset probe git init")
        self.configure_git_identity(probe, "v1-reset-probe")

        fixture = self.artifacts / "non-v1-layout.json"
        fixture.write_text('{"schema_version":2}\n', encoding="utf-8")
        layout_key = f"{repo_prefix}/layout"
        self.run_aws(
            "seed non-v1 layout",
            [
                "put-object",
                "--bucket",
                self.args.bucket,
                "--key",
                layout_key,
                "--body",
                str(fixture),
            ],
        )

        refused = self.run_crab(
            probe,
            ["init", remote_url],
            name="non-v1 remote open is refused",
            check=False,
        )
        refusal_text = (
            Path(refused.stdout_log).read_text(encoding="utf-8", errors="replace")
            + Path(refused.stderr_log).read_text(encoding="utf-8", errors="replace")
        )
        self.check(
            "non-v1-layout-fails-closed",
            refused.exit_code != 0
            and "canonical v1" in refusal_text
            and "reset this isolated development repository" in refusal_text,
            {"exit_code": refused.exit_code},
        )
        missing_manifest = self.run_aws(
            "non-v1 fixture did not create manifest",
            [
                "head-object",
                "--bucket",
                self.args.bucket,
                "--key",
                f"{repo_prefix}/manifest",
            ],
            check=False,
        )
        self.check(
            "non-v1-open-creates-no-manifest",
            missing_manifest.exit_code != 0,
            {"exit_code": missing_manifest.exit_code},
        )

        seeded = self.list_keys(f"{repo_prefix}/")
        self.check(
            "v1-reset-scope-is-exact",
            seeded == {layout_key},
            {"repo_prefix": repo_prefix, "objects": sorted(seeded)},
        )
        self.run_aws(
            "delete exact non-v1 repository fixture",
            ["delete-object", "--bucket", self.args.bucket, "--key", layout_key],
        )
        self.check(
            "v1-reset-prefix-is-empty",
            not self.list_keys(f"{repo_prefix}/"),
            {"repo_prefix": repo_prefix},
        )

        shutil.rmtree(probe)
        self.run_case(case_name, use_crab_add=True)

    def run_case(self, case_name: str, use_crab_add: bool) -> None:
        repo, remote_url, repo_prefix = self.prepare_repo(case_name)
        file_size = self.args.size_mib * 1024 * 1024
        content = deterministic_bytes(file_size, f"{self.run_id}:{case_name}:model")
        model = repo / "model.bin"
        duplicate = repo / "duplicate.bin"
        model.write_bytes(content)
        duplicate.write_bytes(content)
        original_hash = hashlib.sha256(content).hexdigest()

        before_xorbs = self.list_keys(".crab/xorbs/")
        before_shards = self.list_keys(".crab/shards/")

        if use_crab_add:
            self.run_crab(
                repo,
                ["add", "--jobs", "0", "model.bin", "duplicate.bin"],
                name=f"{case_name} crab add binaries",
            )
        else:
            self.run_git(repo, ["add", "model.bin", "duplicate.bin"], name=f"{case_name} git add binaries")

        pointer = self.assert_index_pointer(repo, "model.bin", file_size)
        duplicate_pointer = self.assert_index_pointer(repo, "duplicate.bin", file_size)
        self.check(
            f"{case_name}-duplicate-content-dedupes-to-same-file-hash",
            pointer.file_hash == duplicate_pointer.file_hash,
            {"file_hash": pointer.file_hash},
        )

        staging_files = self.staging_file_count(repo)
        self.check(
            f"{case_name}-local-staging-written-before-push",
            staging_files > 0,
            {"staging_files": staging_files},
        )

        self.run_git(repo, ["commit", "-m", f"{case_name}: add staged binaries"])

        if use_crab_add:
            self.run_crab(
                repo,
                [
                    "push",
                    "--json",
                    "--upload-concurrency",
                    "0",
                    "origin",
                    "HEAD:refs/heads/main",
                ],
                name=f"{case_name} crab push",
                timeout=self.args.push_timeout,
            )
        else:
            self.run_git(
                repo,
                ["push", "origin", "HEAD:refs/heads/main"],
                name=f"{case_name} git push",
            )

        self.head_key(f"{repo_prefix}/manifest")
        after_xorbs = self.list_keys(".crab/xorbs/")
        after_shards = self.list_keys(".crab/shards/")
        new_xorbs = len(after_xorbs - before_xorbs)
        new_shards = len(after_shards - before_shards)
        self.check(f"{case_name}-uploaded-xorbs", new_xorbs > 0, {"new_xorbs": new_xorbs})
        self.check(f"{case_name}-uploaded-shards", new_shards > 0, {"new_shards": new_shards})

        clone_dir = self.run_root / case_name / "clone"
        self.run_cmd(
            f"{case_name} crab clone",
            [self.crab_bin, "clone", remote_url, str(clone_dir), "--jsonl"],
            self.run_root,
        )
        lazy_pointer = parse_pointer(
            (clone_dir / "model.bin").read_text(encoding="utf-8")
        )
        self.check(
            f"{case_name}-lazy-clone-retains-pointer",
            lazy_pointer.file_hash == pointer.file_hash
            and lazy_pointer.size == file_size,
            {"pointer": asdict(lazy_pointer)},
        )
        self.run_crab(clone_dir, ["hydrate", "--all"], name=f"{case_name} crab hydrate")
        hydrated_hash = sha256_file(clone_dir / "model.bin")
        duplicate_hash = sha256_file(clone_dir / "duplicate.bin")
        self.check(
            f"{case_name}-hydrated-model-byte-identical",
            hydrated_hash == original_hash,
            {"original_sha256": original_hash, "hydrated_sha256": hydrated_hash},
        )
        self.check(
            f"{case_name}-hydrated-duplicate-byte-identical",
            duplicate_hash == original_hash,
            {"original_sha256": original_hash, "hydrated_sha256": duplicate_hash},
        )
        self.run_git(
            clone_dir,
            ["fsck", "--connectivity-only"],
            name=f"{case_name} git fsck connectivity",
        )
        self.run_crab(
            clone_dir, ["dehydrate", "--all"], name=f"{case_name} crab dehydrate"
        )
        dehydrated_pointer = parse_pointer(
            (clone_dir / "model.bin").read_text(encoding="utf-8")
        )
        self.check(
            f"{case_name}-dehydrate-restores-pointer",
            dehydrated_pointer.file_hash == pointer.file_hash,
            {"pointer": asdict(dehydrated_pointer)},
        )
        self.run_crab(
            clone_dir, ["hydrate", "--all"], name=f"{case_name} crab rehydrate"
        )
        self.check(
            f"{case_name}-rehydrate-remains-byte-identical",
            sha256_file(clone_dir / "model.bin") == original_hash,
            {"original_sha256": original_hash},
        )

        sqlite_files = self.sqlite_cache_files()
        self.check(
            f"{case_name}-sqlite-cache-used",
            "chunk-index.sqlite" in sqlite_files,
            {"sqlite_files": sqlite_files},
        )
        redb_files = self.redb_cache_files()
        self.check(
            f"{case_name}-no-redb-cache-files",
            not redb_files,
            {"redb_files": redb_files},
        )

        case = CaseRecord(
            name=case_name,
            remote_url=remote_url,
            repo_prefix=repo_prefix,
            file_size=file_size,
            pointer=asdict(pointer),
            duplicate_pointer=asdict(duplicate_pointer),
            staging_files=staging_files,
            new_xorbs=new_xorbs,
            new_shards=new_shards,
            original_sha256=original_hash,
            hydrated_sha256=hydrated_hash,
        )
        self.report.cases.append(asdict(case))
        self.write_report()

    def run_cross_repository_remote_duplicate_case(self) -> None:
        case_name = "cross-repository-remote-duplicate"
        file_size = self.args.size_mib * 1024 * 1024
        content = deterministic_bytes(file_size, f"{self.run_id}:{case_name}:shared")
        expected_sha256 = hashlib.sha256(content).hexdigest()

        source, _source_url, source_prefix = self.prepare_repo(f"{case_name}-source")
        (source / "model.bin").write_bytes(content)
        self.run_crab(source, ["add", "model.bin"], name=f"{case_name} source add")
        source_pointer = self.assert_index_pointer(source, "model.bin", file_size)
        self.run_git(source, ["commit", "-m", "publish shared payload"])
        self.run_crab(
            source,
            ["push", "origin", "HEAD:refs/heads/main"],
            name=f"{case_name} source push",
            timeout=self.args.push_timeout,
        )
        self.head_key(f"{source_prefix}/manifest")
        source_xorbs = self.list_keys(".crab/xorbs/")

        consumer, consumer_url, consumer_prefix = self.prepare_repo(
            f"{case_name}-consumer"
        )
        (consumer / "model.bin").write_bytes(content)
        before_add_xorbs = self.list_keys(".crab/xorbs/")
        self.run_crab(
            consumer,
            ["add", "model.bin"],
            name=f"{case_name} consumer add",
        )
        consumer_pointer = self.assert_index_pointer(consumer, "model.bin", file_size)
        self.check(
            f"{case_name}-same-content-has-same-file-hash",
            consumer_pointer.file_hash == source_pointer.file_hash,
            {
                "source_file_hash": source_pointer.file_hash,
                "consumer_file_hash": consumer_pointer.file_hash,
            },
        )
        inventory = self.staging_payload_inventory(consumer)
        self.check(
            f"{case_name}-add-records-proof-bearing-remote-authority",
            inventory["recipe_remote_chunks"] > 0,
            inventory,
        )
        self.check(
            f"{case_name}-add-keeps-no-local-segment-payload",
            inventory["chunks"] == 0 and inventory["chunk_payloads"] == 0,
            inventory,
        )
        self.check(
            f"{case_name}-add-builds-no-prepared-xorb",
            inventory["prepared_xorbs"] == 0
            and inventory["prepared_payloads"] == 0,
            inventory,
        )
        after_add_xorbs = self.list_keys(".crab/xorbs/")
        self.check(
            f"{case_name}-add-writes-no-remote-xorb",
            after_add_xorbs == before_add_xorbs == source_xorbs,
            {
                "before": len(before_add_xorbs),
                "after": len(after_add_xorbs),
            },
        )

        self.run_git(consumer, ["commit", "-m", "reuse shared payload"])
        consumer_push = self.run_crab(
            consumer,
            [
                "push",
                "--log-level",
                "debug",
                "origin",
                "HEAD:refs/heads/main",
            ],
            name=f"{case_name} consumer push",
            timeout=self.args.push_timeout,
        )
        push_stderr = Path(consumer_push.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        remote_chunk_count = inventory["recipe_remote_chunks"]
        self.check(
            f"{case_name}-push-revalidates-staged-proof-directly",
            "revalidated staged remote placement proofs" in push_stderr
            and f'"planned":{remote_chunk_count}' in push_stderr
            and f'"matched":{remote_chunk_count}' in push_stderr
            and f'"generation_verified":{remote_chunk_count}' in push_stderr
            and f'"payload_verified":{remote_chunk_count}' in push_stderr
            and '"stale_existing":0' in push_stderr
            and '"global_existing":0' in push_stderr,
            {"remote_chunks": remote_chunk_count},
        )
        self.head_key(f"{consumer_prefix}/manifest")
        after_push_xorbs = self.list_keys(".crab/xorbs/")
        self.check(
            f"{case_name}-push-uploads-no-new-xorb",
            after_push_xorbs == source_xorbs,
            {
                "before": len(source_xorbs),
                "after": len(after_push_xorbs),
            },
        )

        clone_dir = self.run_root / case_name / "consumer-clone"
        self.run_cmd(
            f"{case_name} consumer clone",
            [self.crab_bin, "clone", consumer_url, str(clone_dir), "--jsonl"],
            self.run_root,
        )
        self.run_crab(clone_dir, ["hydrate", "--all"], name=f"{case_name} hydrate")
        self.check(
            f"{case_name}-fresh-hydrate-is-byte-identical",
            sha256_file(clone_dir / "model.bin") == expected_sha256,
            {"expected_sha256": expected_sha256},
        )

    def run_committed_restage_before_first_push_case(self) -> None:
        case_name = "committed-restage-before-first-push"
        repo, remote_url, repo_prefix = self.prepare_repo(case_name)
        file_size = self.args.size_mib * 1024 * 1024
        first_content = deterministic_bytes(file_size, f"{self.run_id}:{case_name}:first")
        second_content = deterministic_bytes(file_size, f"{self.run_id}:{case_name}:second")
        model = repo / "model.bin"

        model.write_bytes(first_content)
        self.run_crab(repo, ["add", "model.bin"], name=f"{case_name} add first version")
        first_pointer = self.assert_index_pointer(repo, "model.bin", file_size)
        self.run_git(repo, ["commit", "-m", "commit first staged version"])
        first_commit = self.rev_parse(repo, "HEAD")
        self.run_git(repo, ["branch", "-m", "history-a"])
        self.run_git(repo, ["checkout", "--orphan", "main"])
        self.run_git(repo, ["rm", "-rf", "."])

        model.write_bytes(second_content)
        self.run_crab(
            repo,
            ["add", "--skip-git-add", "model.bin"],
            name=f"{case_name} prepare second version",
        )
        self.run_git(repo, ["add", "model.bin"], name=f"{case_name} promote second version")
        second_pointer = self.assert_index_pointer(repo, "model.bin", file_size)
        self.check(
            f"{case_name}-versions-have-distinct-file-hashes",
            first_pointer.file_hash != second_pointer.file_hash,
        )
        self.run_git(repo, ["commit", "-m", "commit second staged version"])

        before_push_record = self.run_crab(
            repo,
            ["staging", "stats", "--json"],
            name=f"{case_name} staging ownership before push",
        )
        before_push = json.loads(self.read_stdout(before_push_record))["data"]["lifecycle"]
        self.check(
            f"{case_name}-retains-current-and-committed-history-owners",
            before_push["path_heads"] == 2
            and before_push["path_leases"] == 2
            and before_push["open_batches_without_publication"] == 0
            and before_push["reclaimable_superseded_leases"] == 0
            and before_push["reclaimable_files"] == 0,
            before_push,
        )

        self.run_crab(
            repo,
            [
                "push",
                "origin",
                "refs/heads/history-a:refs/heads/history-a",
                "refs/heads/main:refs/heads/main",
            ],
            name=f"{case_name} push both versions",
            timeout=self.args.push_timeout,
        )
        self.head_key(f"{repo_prefix}/manifest")

        after_push_record = self.run_crab(
            repo,
            ["staging", "stats", "--json"],
            name=f"{case_name} staging ownership after push",
        )
        after_push = json.loads(self.read_stdout(after_push_record))["data"]["lifecycle"]
        self.check(
            f"{case_name}-retires-both-pushed-version-owners",
            after_push["path_heads"] == 0
            and after_push["path_leases"] == 0
            and after_push["open_batches_without_publication"] == 0
            and after_push["reclaimable_superseded_leases"] == 0
            and after_push["reclaimable_files"] == 0,
            after_push,
        )

        clone_dir = self.run_root / case_name / "clone"
        self.run_cmd(
            f"{case_name} crab clone",
            [self.crab_bin, "clone", remote_url, str(clone_dir), "--jsonl"],
            self.run_root,
        )
        self.run_crab(clone_dir, ["hydrate", "--all"], name=f"{case_name} hydrate second")
        self.check(
            f"{case_name}-latest-version-is-byte-identical",
            sha256_file(clone_dir / "model.bin") == hashlib.sha256(second_content).hexdigest(),
        )

        history_clone = self.run_root / case_name / "history-clone"
        self.run_cmd(
            f"{case_name} history crab clone",
            [self.crab_bin, "clone", remote_url, str(history_clone), "--jsonl"],
            self.run_root,
        )
        (history_clone / ".gitattributes").unlink(missing_ok=True)
        self.run_git(history_clone, ["checkout", "--detach", first_commit])
        self.run_crab(history_clone, ["hydrate", "--all"], name=f"{case_name} hydrate first")
        self.check(
            f"{case_name}-committed-prior-version-is-byte-identical",
            sha256_file(history_clone / "model.bin") == hashlib.sha256(first_content).hexdigest(),
        )

    def run(self) -> None:
        self.preflight()
        if self.args.only_cross_repo_duplicate:
            self.run_cross_repository_remote_duplicate_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        self.run_v1_hard_cutover_reset_case()
        self.run_missing_manifest_case()
        self.run_corrupt_manifest_case()
        self.run_ref_update_delete_force_and_tag_case()
        self.run_atomic_rejection_case()
        self.run_shallow_case()
        self.run_immutable_object_failure_case("pack")
        self.run_immutable_object_failure_case("idx")
        self.run_corrupt_index_failure_case()
        self.run_concurrent_push_case()
        self.run_committed_restage_before_first_push_case()
        self.run_cross_repository_remote_duplicate_case()
        self.run_case("crab-add-crab-push", use_crab_add=True)
        self.run_case("git-add-git-push", use_crab_add=False)
        self.check_credential_disclosure()
        self.report.status = "passed"
        self.write_report()


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
    parser.add_argument(
        "--endpoint-url",
        default=credential_default(
            "AWS_ENDPOINT_URL_S3",
            credential_default("AWS_ENDPOINT_URL", DEFAULT_ENDPOINT),
        ),
    )
    parser.add_argument(
        "--access-key", default=credential_default("AWS_ACCESS_KEY_ID", "crab")
    )
    parser.add_argument(
        "--secret-key", default=credential_default("AWS_SECRET_ACCESS_KEY", "crab")
    )
    parser.add_argument(
        "--session-token", default=credential_default("AWS_SESSION_TOKEN", "")
    )
    parser.add_argument(
        "--region",
        default=credential_default(
            "AWS_REGION", credential_default("AWS_DEFAULT_REGION", "us-east-1")
        ),
    )
    parser.add_argument("--crab-bin", default="crab")
    parser.add_argument("--run-id")
    parser.add_argument("--size-mib", type=positive_int, default=4)
    parser.add_argument("--timeout", type=positive_int, default=120)
    parser.add_argument("--push-timeout", type=positive_int, default=240)
    parser.add_argument("--only-cross-repo-duplicate", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    smoke = AddCommitPushSmoke(args)
    try:
        smoke.run()
    except SmokeError as exc:
        smoke.report.status = "failed"
        smoke.write_report()
        print(f"FAILED: {exc}", file=os.sys.stderr)
        print(f"report: {smoke.report.artifacts.get('report', '')}", file=os.sys.stderr)
        return 1
    print("PASS add/commit/push RustFS smoke")
    print(f"report: {smoke.report.artifacts.get('report', '')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
