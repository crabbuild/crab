#!/usr/bin/env python3
"""Run Crab add/commit/push smokes against a local RustFS/S3 endpoint.

The harness exercises both user-facing staging paths:

* ``crab add`` followed by ``crab push``
* ``git add`` followed by ``git push`` through ``git-remote-crab``

The large-file cases prove pointer staging, object-store publication, fresh
clone, hydration, and byte identity. A separate ordinary-Git matrix proves
missing/corrupt metadata-root handling, update/fetch, deletion, force and
non-fast-forward behavior, atomic rejection, tag following, shallow history,
capsule loss, embedded pack/index corruption, and concurrent push serialization.

With ``--source``, run only the two managed-file workflows on disposable
clones of a read-only real repository. Use a run root on the workspace volume
and increase the command timeouts for large histories.
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
POINTER_VERSION = "version https://crab.build/spec/v1"
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


def capsule_staged_proof_complete(stderr: str, count: int) -> bool:
    """Require complete direct reuse evidence for the capsule duplicate fixture."""
    if type(count) is not int or count <= 0:
        return False
    proof_message = "revalidated staged remote placement proofs"
    adoption_message = "capsule staged remote proof adoption complete"
    events = []
    for line in stderr.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        fields = record.get("fields") if isinstance(record, dict) else None
        if isinstance(fields, dict) and fields.get("message") in (
            proof_message, adoption_message
        ):
            events.append(fields)
    # This fixture makes one push with one direct-proof batch. Counts from
    # different batches cannot jointly prove that every planned chunk survived.
    if len(events) != 2:
        return False
    proof, adoption = events
    if proof.get("message") != proof_message or adoption.get("message") != adoption_message:
        return False
    expected = {
        "planned": count, "indexed": count, "matched": count,
        "capsule_source_verified": count, "generation_verified": 0,
        "payload_verified": count, "placement_mismatches": 0,
        "origin_proof_mismatches": 0,
    }
    adopted = {"planned_existing": count, "stale_existing": 0, "global_existing": 0}
    return all(
        type(event.get(key)) is int and event[key] == value
        for event, counters in [(proof, expected), (adoption, adopted)]
        for key, value in counters.items()
    )


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


def capsule_member_range(body: bytes, kind: str) -> tuple[int, int]:
    """Locate one pack/index fault inside the synthetic single-member run."""
    if len(body) < 48 or body[-8:] != b"CRBRUN01" or kind not in ("pack", "index"):
        raise SmokeError("fault fixture is not a capsule run with a pack/index target")
    footer_length = int.from_bytes(body[-48:-40], "big")
    footer_start = len(body) - 48 - footer_length
    if not 0 < footer_length <= 8 * 1024 * 1024 or footer_start < 0:
        raise SmokeError("fault fixture footer is outside its container")
    try:
        footer = json.loads(body[footer_start:-48])
        members = footer["git_packs"]
        if footer["version"] != 4 or len(members) != 1:
            raise SmokeError("fault fixture requires one current-format pack member")
        offset, length = members[0][kind]["offset"], members[0][kind]["length"]
    except (KeyError, TypeError, ValueError) as exc:
        raise SmokeError("fault fixture has no usable pack member") from exc
    if (type(offset) is not int or type(length) is not int
            or offset < 0 or length < 4 or offset + length > footer_start):
        raise SmokeError("fault fixture range overlaps control bytes or escapes its container")
    magic = b"PACK" if kind == "pack" else b"\xfftOc"
    if body[offset:offset + 4] != magic:
        raise SmokeError("fault fixture member does not contain the selected Git format")
    return offset, length


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
        self.report_initialized = False
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
        self.report_initialized = True
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
        extra_env: dict[str, str] | None = None,
    ) -> CommandRecord:
        start = time.monotonic()
        try:
            proc = subprocess.run(
                args,
                cwd=cwd,
                env={**self.env, **(extra_env or {})},
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
        extra_env: dict[str, str] | None = None,
    ) -> CommandRecord:
        return self.run_cmd(
            name or "crab " + " ".join(args),
            [self.crab_bin, *args],
            repo,
            check=check,
            timeout=timeout,
            extra_env=extra_env,
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

    def head_repository_state(self, repo_prefix: str) -> None:
        """Require the authoritative v2 root, with legacy-v1 fallback for old fixtures."""
        root = self.run_aws(
            "head " + repo_prefix + "/v2/root",
            [
                "head-object",
                "--bucket",
                self.args.bucket,
                "--key",
                f"{repo_prefix}/v2/root",
            ],
            check=False,
        )
        if root.exit_code == 0:
            return
        root_stderr = Path(root.stderr_log).read_text(
            encoding="utf-8", errors="replace"
        )
        if "404" not in root_stderr and "Not Found" not in root_stderr:
            raise SmokeError(
                f"v2 root probe failed for {repo_prefix}; stderr log: {root.stderr_log}"
            )
        self.head_key(f"{repo_prefix}/manifest")

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
        self.cache_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
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
                "prepared_payload_chunks",
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

    def assert_staging_retired(self, repo: Path, check_name: str) -> None:
        record = self.run_crab(repo, ["staging", "stats", "--json"], name=check_name)
        lifecycle = json.loads(self.read_stdout(record))["data"]["lifecycle"]
        self.check(
            check_name,
            all(lifecycle[field] == 0 for field in (
                "path_heads", "path_leases", "open_batches_without_publication",
                "reclaimable_superseded_leases", "reclaimable_files",
                "open_push_snapshots", "committed_push_snapshots", "payloads",
                "current_head_prepared_bytes", "snapshot_pinned_prepared_bytes",
            )),
            lifecycle,
        )

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
        case_root.mkdir(parents=True, exist_ok=True)
        remote_url, repo_prefix = self.remote_for_case(case_name)
        if self.args.source:
            self.run_git(
                case_root,
                ["clone", "--shared", "--no-checkout", str(self.args.source), str(repo)],
                name=f"{case_name} clone read-only source",
            )
            self.run_git(repo, ["remote", "remove", "origin"])
            source_head = self.report.artifacts["fixture_source_head"]
            self.run_git(repo, ["checkout", "--detach", source_head])
            self.run_git(repo, ["switch", "-C", "main", source_head])
            self.check(
                f"{case_name}-fixture-paths-unused",
                not any(
                    (repo / path).exists() or (repo / path).is_symlink()
                    for path in ("model.bin", "duplicate.bin", ".crab")
                )
                and all(
                    not (repo / path).is_symlink()
                    and (not (repo / path).exists() or (repo / path).is_file())
                    for path in ("crab.toml", ".gitattributes")
                ),
            )
        else:
            repo.mkdir()
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

    def run_missing_root_case(self) -> None:
        case_name = "missing-root"
        _repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        root_key = f"{repo_prefix}/v2/root"
        self.ls_remote(remote_url, name=f"{case_name} healthy list")
        self.head_key(root_key)
        self.run_aws(
            "delete canonical root fixture",
            [
                "delete-object",
                "--bucket",
                self.args.bucket,
                "--key",
                root_key,
            ],
        )
        status, _, _ = self.signed_s3_request("HEAD", root_key)
        self.check(f"{case_name}-fixture-removed", status == 404,
                   {"key": root_key, "status": status})
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

        missing_tip = self.run_git(
            contender,
            ["cat-file", "-e", third],
            name=f"{case_name} remote tip absent locally",
            check=False,
        )
        self.check(
            f"{case_name}-force-push-does-not-require-fetch",
            missing_tip.exit_code != 0,
            {"remote_tip": third},
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

        self.run_crab(
            repo,
            ["push", "--force", "origin", "main"],
            name=f"{case_name} native force without fetching contender",
        )
        self.check(
            f"{case_name}-native-force-push-updates-tip",
            self.ls_remote(remote_url, name=f"{case_name} list after native force").get(
                "refs/heads/main"
            )
            == third,
            {"expected": third},
        )
        self.run_git(contender, ["push", "--force", "origin", "main"])
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
        commits = []
        for index, text in enumerate(("one", "two", "three"), start=1):
            (repo / "history.txt").write_text(text + "\n", encoding="utf-8")
            self.run_git(repo, ["add", "history.txt"])
            date = f"2023-11-{14 + index:02d}T22:13:20Z"
            self.run_cmd(
                f"{case_name} dated commit {text}",
                ["git", "commit", "-m", text],
                repo,
                extra_env={"GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date},
            )
            commits.append(self.rev_parse(repo, "HEAD"))
        first = commits[0]
        # Exclude the first commit through a side tip, keeping that ancestor
        # unadvertised for the raw-SHA policy check below.
        archive = self.read_stdout(self.run_cmd(
            f"{case_name} archive side tip",
            ["git", "commit-tree", f"{first}^{{tree}}", "-p", first, "-m", "archive"],
            repo,
            extra_env={
                "GIT_AUTHOR_DATE": "2023-11-15T23:13:20Z",
                "GIT_COMMITTER_DATE": "2023-11-15T23:13:20Z",
            },
        ))
        self.run_git(repo, ["tag", "archive", archive])
        self.run_git(repo, ["push", "-u", "origin", "main", "--tags"])

        # Match native upload-pack before asserting Crab's selector behavior.
        # Fixed dates separate date parsing from commit-boundary selection.
        selectors = (
            ("since", ["--shallow-since", "2023-11-16T22:13:20Z"]),
            ("exclude", ["--shallow-exclude", "archive"]),
            ("future", ["--shallow-since", "2030-01-01T00:00:00Z"]),
            ("exclude-all", ["--shallow-exclude", "main"]),
        )
        for selector, arguments in selectors:
            expected_success = selector in ("since", "exclude")
            transports = (
                ("native", repo.resolve().as_uri(), []),
                ("crab-v2", remote_url, ["--config", "protocol.version=2"]),
                ("crab-classic", remote_url, ["--config", "protocol.version=0"]),
            )
            for transport, url, protocol in transports:
                clone = self.run_root / case_name / f"{transport}-{selector}-clone"
                result = self.clone_git(
                    url, clone,
                    name=f"{case_name} {transport} {selector} clone",
                    extra_args=[*protocol, *arguments], check=False,
                )
                detail = {"exit_code": result.exit_code, "stderr_log": result.stderr_log}
                if not expected_success:
                    diagnostic = Path(result.stderr_log).read_text(
                        encoding="utf-8", errors="replace"
                    )
                    self.check(
                        f"{case_name}-{transport}-{selector}-rejects-empty-selection",
                        result.exit_code != 0
                        and "no commits selected for shallow requests" in diagnostic
                        and not (clone / "history.txt").exists(),
                        detail,
                    )
                    continue
                self.check(
                    f"{case_name}-{transport}-{selector}-clone-succeeds",
                    result.exit_code == 0, detail,
                )
                selected = self.read_stdout(self.run_git(clone, ["rev-list", "HEAD"]))
                boundary = (clone / ".git" / "shallow").read_text().splitlines()
                self.check(
                    f"{case_name}-{transport}-{selector}-exact-history-and-boundary",
                    selected.splitlines() == list(reversed(commits[1:]))
                    and boundary == [commits[1]]
                    and (clone / "history.txt").read_text() == "three\n",
                    {"commits": selected.splitlines(), "boundary": boundary},
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

    def run_corrupt_root_case(self) -> None:
        case_name = "corrupt-root"
        _repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        root_key = f"{repo_prefix}/v2/root"
        self.ls_remote(remote_url, name=f"{case_name} healthy list")
        self.head_key(root_key)
        status, _, response_body = self.signed_s3_request(
            "PUT",
            root_key,
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

        status, _, body = self.signed_s3_request("GET", root_key)
        self.check(f"{case_name}-fixture-overwritten",
                   status == 200 and body == b'{"version":',
                   {"key": root_key, "status": status})

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

    def run_capsule_failure_case(self, fault: str) -> None:
        case_name = "missing-capsule" if fault == "missing" else f"corrupt-{fault}"
        repo, remote_url, repo_prefix = self.prepare_git_repo(case_name)
        remote_tip = self.commit_text(repo, "payload.txt", "immutable\n", "immutable")
        self.run_git(repo, ["push", "-u", "origin", "main"])
        self.check(
            f"{case_name}-healthy-tip-before-fault",
            self.ls_remote(remote_url, name=f"{case_name} healthy refs").get("refs/heads/main") == remote_tip,
        )
        keys = self.list_keys(repo_prefix)
        candidates = sorted(key for key in keys if key.startswith(f"{repo_prefix}/v2/capsules/"))
        self.check(f"{case_name}-fixture-object-found", len(candidates) == 1, {"keys": candidates})
        target_key = candidates[0]
        authority = {}
        for key in sorted(key for key in keys if key == f"{repo_prefix}/v2/root" or key.startswith(f"{repo_prefix}/v2/refs/")):
            status, _, value = self.signed_s3_request("GET", key)
            self.check(f"{case_name}-authority-readable-before-fault", status == 200, {"key": key})
            authority[key] = value
        status, headers, original = self.signed_s3_request("GET", target_key)
        self.check(f"{case_name}-fixture-body-readable", status == 200 and len(original) < 1024 * 1024)
        if fault == "missing":
            status, _, _ = self.signed_s3_request("DELETE", target_key)
            absent, _, _ = self.signed_s3_request("GET", target_key)
            self.check(f"{case_name}-fixture-object-deleted", status in (200, 204) and absent == 404)
        else:
            offset, length = capsule_member_range(original, fault)
            damaged = bytearray(original)
            damaged[offset] ^= 1
            status, _, _ = self.signed_s3_request(
                "PUT", target_key, body=bytes(damaged), extra_headers={"if-match": headers["etag"]},
            )
            read_status, _, stored = self.signed_s3_request("GET", target_key)
            self.check(
                f"{case_name}-fixture-member-corrupted",
                status in (200, 201) and read_status == 200 and stored == bytes(damaged),
                {"key": target_key, "member_offset": offset, "member_length": length},
            )
        clone_dir = self.run_root / case_name / "clone"
        clone_cache = self.run_root / case_name / "cold-cache"
        self.check(f"{case_name}-clone-cache-starts-empty", not clone_cache.exists())
        clone = self.run_cmd(
            f"{case_name} git clone", ["git", "clone", remote_url, str(clone_dir)], self.run_root,
            check=False, extra_env={"CRAB_CACHE_DIR": str(clone_cache)},
        )
        head = self.run_git(
            self.run_root, ["-C", str(clone_dir), "rev-parse", "--verify", "HEAD"],
            name=f"{case_name} verify no local ref update", check=False,
        )
        self.check(
            f"{case_name}-clone-fails-before-checkout",
            clone.exit_code != 0 and head.exit_code != 0 and not (clone_dir / "payload.txt").exists(),
            {"clone_exit_code": clone.exit_code, "head_exit_code": head.exit_code, "target_key": target_key},
        )
        if fault != "missing":
            diagnostic = Path(clone.stderr_log).read_text(encoding="utf-8", errors="replace")
            self.check(
                f"{case_name}-clone-surfaces-integrity-error",
                "CRAB-E0020" in diagnostic or "corrupt object" in diagnostic.lower(),
                {"stderr_log": clone.stderr_log},
            )
        # Payloads share their physical object with discovery controls. Compare
        # authority bytes directly; failed discovery cannot prove unchanged refs.
        after = {}
        for key in authority:
            status, _, value = self.signed_s3_request("GET", key)
            self.check(f"{case_name}-authority-readable-after-fault", status == 200, {"key": key})
            after[key] = value
        self.check(f"{case_name}-remote-authority-unchanged", after == authority)

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
        refused_without_mutation = (
            "canonical v1" in refusal_text and "not supported" in refusal_text
        ) or (
            "CRAB-E0020" in refusal_text
            and "repository prefix contains data but has no capsule-protocol root"
            in refusal_text
            and "left it unchanged" in refusal_text
        )
        self.check(
            "non-v1-layout-fails-closed",
            refused.exit_code != 0 and refused_without_mutation,
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

        self.head_repository_state(repo_prefix)
        self.assert_staging_retired(repo, f"{case_name}-retires-published-staging-owners")
        after_xorbs = self.list_keys(".crab/xorbs/")
        after_shards = self.list_keys(".crab/shards/")
        new_xorbs = len(after_xorbs - before_xorbs)
        new_shards = len(after_shards - before_shards)
        self.check(f"{case_name}-uploaded-xorbs", new_xorbs > 0, {"new_xorbs": new_xorbs})
        self.check(f"{case_name}-uploaded-shards", new_shards > 0, {"new_shards": new_shards})

        clone_dir = self.run_root / case_name / "clone"
        clone_cache = self.run_root / case_name / "clone-cache"
        self.check(f"{case_name}-clone-cache-starts-empty", not clone_cache.exists())
        clone_env = {"CRAB_CACHE_DIR": str(clone_cache)}
        self.run_cmd(
            f"{case_name} crab clone",
            [self.crab_bin, "clone", remote_url, str(clone_dir), "--jsonl"],
            self.run_root,
            extra_env=clone_env,
        )
        source_tip = self.rev_parse(repo, "HEAD")
        self.check(
            f"{case_name}-clone-tip-matches-published-commit",
            self.rev_parse(clone_dir, "HEAD") == source_tip
            and self.ls_remote(remote_url, name=f"{case_name} advertised refs").get(
                "refs/heads/main"
            ) == source_tip,
            {"expected_tip": source_tip},
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
        self.run_crab(
            clone_dir, ["hydrate", "--all"], name=f"{case_name} crab hydrate",
            extra_env=clone_env,
        )
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
            clone_dir, ["dehydrate", "--all"], name=f"{case_name} crab dehydrate",
            extra_env=clone_env,
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
            clone_dir, ["hydrate", "--all"], name=f"{case_name} crab rehydrate",
            extra_env=clone_env,
        )
        self.check(
            f"{case_name}-rehydrate-remains-byte-identical",
            sha256_file(clone_dir / "model.bin") == original_hash,
            {"original_sha256": original_hash},
        )

        # Reopen from a new cache after catalog compaction; the original clone
        # can otherwise hide a checkpoint reader that only works with warm data.
        self.run_crab(
            repo, ["metadb", "owner", "--once", "--jsonl"],
            name=f"{case_name} checkpoint catalog", timeout=self.args.push_timeout,
        )
        diagnosis = self.run_crab(
            repo, ["metadb", "diagnose", "--json"],
            name=f"{case_name} checkpoint diagnosis",
        )
        self.check(
            f"{case_name}-checkpoint-present",
            json.loads(self.read_stdout(diagnosis))["data"]["capsule"]["checkpoint_present"],
        )
        checkpoint_clone = self.run_root / case_name / "checkpoint-clone"
        checkpoint_cache = self.run_root / case_name / "checkpoint-cache"
        self.check(f"{case_name}-checkpoint-cache-starts-empty", not checkpoint_cache.exists())
        checkpoint_env = {"CRAB_CACHE_DIR": str(checkpoint_cache)}
        self.run_cmd(
            f"{case_name} clone checkpoint",
            [self.crab_bin, "clone", remote_url, str(checkpoint_clone), "--jsonl"],
            self.run_root, extra_env=checkpoint_env,
        )
        self.check(
            f"{case_name}-checkpoint-clone-tip-matches",
            self.rev_parse(checkpoint_clone, "HEAD") == source_tip,
        )
        self.run_crab(
            checkpoint_clone, ["hydrate", "--all"],
            name=f"{case_name} cold checkpoint hydration", extra_env=checkpoint_env,
        )
        self.check(
            f"{case_name}-checkpoint-model-byte-identical",
            sha256_file(checkpoint_clone / "model.bin") == original_hash,
        )
        self.check(
            f"{case_name}-checkpoint-duplicate-byte-identical",
            sha256_file(checkpoint_clone / "duplicate.bin") == original_hash,
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

    def run_partial_overlap_case(self) -> None:
        case_name = "partial-overlap-canonical-authority"
        repo, remote_url, repo_prefix = self.prepare_repo(case_name)
        shared_size = max(self.args.size_mib, 20) * 1024 * 1024
        prefix_size = 4 * 1024 * 1024
        shared = deterministic_bytes(
            shared_size, f"{self.run_id}:{case_name}:shared"
        )
        first_prefix = deterministic_bytes(
            prefix_size, f"{self.run_id}:{case_name}:first"
        )
        second_prefix = deterministic_bytes(
            prefix_size, f"{self.run_id}:{case_name}:second"
        )
        first = first_prefix + shared
        second = second_prefix + shared
        (repo / "first.bin").write_bytes(first)
        (repo / "second.bin").write_bytes(second)
        first_sha256 = hashlib.sha256(first).hexdigest()
        second_sha256 = hashlib.sha256(second).hexdigest()
        before_xorbs = self.list_keys(".crab/xorbs/")

        self.run_crab(
            repo,
            ["add", "--jobs", "2", "first.bin", "second.bin"],
            name=f"{case_name} add overlapping files",
        )
        first_pointer = self.assert_index_pointer(repo, "first.bin", len(first))
        second_pointer = self.assert_index_pointer(repo, "second.bin", len(second))
        self.check(
            f"{case_name}-files-remain-distinct",
            first_pointer.file_hash != second_pointer.file_hash,
        )

        index_path = repo / ".crab" / "staging" / "index.db"
        connection = sqlite3.connect(f"file:{index_path}?mode=ro", uri=True)
        try:
            prepared_chunks = int(
                connection.execute(
                    "SELECT COUNT(*) FROM prepared_payload_chunks"
                ).fetchone()[0]
            )
            unique_prepared_chunks = int(
                connection.execute(
                    "SELECT COUNT(DISTINCT chunk_hash) FROM prepared_payload_chunks"
                ).fetchone()[0]
            )
            prepared_payloads = int(
                connection.execute("SELECT COUNT(*) FROM prepared_payloads").fetchone()[0]
            )
            segment_chunks = int(
                connection.execute("SELECT COUNT(*) FROM chunks").fetchone()[0]
            )
            shared_chunks = int(
                connection.execute(
                    """
                    SELECT COUNT(*) FROM (
                        SELECT occurrence.chunk_hash
                        FROM recipe_occurrences AS occurrence
                        JOIN file_recipes AS recipe USING (recipe_hash)
                        WHERE hex(recipe.file_hash) = upper(?)
                        INTERSECT
                        SELECT occurrence.chunk_hash
                        FROM recipe_occurrences AS occurrence
                        JOIN file_recipes AS recipe USING (recipe_hash)
                        WHERE hex(recipe.file_hash) = upper(?)
                    )
                    """,
                    (first_pointer.file_hash, second_pointer.file_hash),
                ).fetchone()[0]
            )
        finally:
            connection.close()
        payload_files = sum(
            1
            for path in (repo / ".crab/staging/push-plans/payloads").rglob("*.xorb")
            if path.is_file()
        )
        self.check(
            f"{case_name}-fixture-shares-many-chunks",
            shared_chunks > 8,
            {"shared_chunks": shared_chunks},
        )
        self.check(
            f"{case_name}-one-canonical-placement-per-chunk",
            prepared_chunks == unique_prepared_chunks,
            {
                "prepared_chunks": prepared_chunks,
                "unique_prepared_chunks": unique_prepared_chunks,
            },
        )
        self.check(
            f"{case_name}-one-local-body-per-payload",
            payload_files == prepared_payloads and prepared_payloads > 0,
            {"payload_files": payload_files, "prepared_payloads": prepared_payloads},
        )
        self.check(
            f"{case_name}-direct-prepared-needs-no-segment-copy",
            segment_chunks == 0,
            {"segment_chunks": segment_chunks},
        )

        self.run_git(repo, ["commit", "-m", "add partially overlapping files"])
        self.run_crab(
            repo,
            ["push", "origin", "HEAD:refs/heads/main"],
            name=f"{case_name} push",
            timeout=self.args.push_timeout,
        )
        self.head_repository_state(repo_prefix)
        after_xorbs = self.list_keys(".crab/xorbs/")
        new_xorbs = len(after_xorbs - before_xorbs)
        self.check(
            f"{case_name}-uploads-only-distinct-local-payloads",
            0 < new_xorbs <= prepared_payloads,
            {"new_xorbs": new_xorbs, "prepared_payloads": prepared_payloads},
        )

        clone_dir = self.run_root / case_name / "clone"
        self.run_cmd(
            f"{case_name} crab clone",
            [self.crab_bin, "clone", remote_url, str(clone_dir), "--jsonl"],
            self.run_root,
        )
        self.run_crab(clone_dir, ["hydrate", "--all"], name=f"{case_name} hydrate")
        self.check(
            f"{case_name}-first-file-byte-identical",
            sha256_file(clone_dir / "first.bin") == first_sha256,
        )
        self.check(
            f"{case_name}-second-file-byte-identical",
            sha256_file(clone_dir / "second.bin") == second_sha256,
        )

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
        self.head_repository_state(source_prefix)
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
            inventory["prepared_payload_chunks"] == 0
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
            capsule_staged_proof_complete(push_stderr, remote_chunk_count),
            {"remote_chunks": remote_chunk_count},
        )
        self.head_repository_state(consumer_prefix)
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
        clone_cache = self.run_root / case_name / "consumer-clone-cache"
        self.check(f"{case_name}-clone-cache-starts-empty", not clone_cache.exists())
        clone_env = {"CRAB_CACHE_DIR": str(clone_cache)}
        self.run_cmd(
            f"{case_name} consumer clone",
            [self.crab_bin, "clone", consumer_url, str(clone_dir), "--jsonl"],
            self.run_root,
            extra_env=clone_env,
        )
        self.run_crab(
            clone_dir, ["hydrate", "--all"], name=f"{case_name} hydrate",
            extra_env=clone_env,
        )
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
        self.run_git(
            repo,
            ["checkout", "history-a", "--", "crab.toml", ".gitattributes"],
            name=f"{case_name} retain project configuration",
        )

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
        before_push_data = json.loads(self.read_stdout(before_push_record))["data"]
        before_push = before_push_data["lifecycle"]
        expected_versions = {first_pointer.file_hash, second_pointer.file_hash}
        retained_versions = {entry["file_hash"] for entry in before_push_data["files"]}
        # Native preparation and Git-clean retain independent handoff leases.
        # Version coverage, followed by exact retirement, proves their lifetime;
        # a fixed pre-push lease count cannot identify the preserved versions.
        self.check(
            f"{case_name}-retains-current-and-committed-history-owners",
            retained_versions == expected_versions
            and before_push["recipes"] == len(expected_versions)
            and before_push["path_leases"] >= len(expected_versions)
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
        self.head_repository_state(repo_prefix)
        fsck_record = self.run_crab(
            repo,
            ["fsck", "--json"],
            name=f"{case_name} full store fsck",
        )
        fsck_data = json.loads(self.read_stdout(fsck_record))["data"]
        self.check(
            f"{case_name}-publishes-complete-acceleration-metadata",
            fsck_data["passed"]
            and fsck_data["errors"] == 0
            and fsck_data["repair_failures"] == 0,
            fsck_data,
        )

        self.assert_staging_retired(repo, f"{case_name}-retires-both-pushed-version-owners")

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

    def run_gc_fence_upgrade_case(self) -> None:
        repo, remote, domain = self.prepare_git_repo("gc-fence-upgrade")

        # Keep a legacy v1 descriptor beside the v2 root so the rollback
        # writer reaches the upgraded fence instead of failing admission on a
        # deliberately absent v1 layout. The v2 root remains authoritative for
        # the migration command under test.
        legacy_layout = json.dumps({
            "schema_version": 1,
            "layout": "partitioned",
            "chunk_partition_bits": 8,
            "file_partition_bits": 8,
            "receipt_partition_bits": 8,
            "recipe_page_entries": 512,
            "recipe_page_max_bytes": 65536,
            "digest": "67991fbbc08a032d74558b1fbecfa32a04c54cf02f1e36afa10fba14d46078f6",
        }).encode()
        layout_status, _, _ = self.signed_s3_request(
            "PUT", f"{domain}/layout", body=legacy_layout,
            extra_headers={"if-none-match": "*"},
        )
        self.check("legacy-layout-fixture-created-only-if-absent", layout_status == 200)
        legacy_manifest = json.dumps({
            "version": 1,
            "generation": 0,
            "created_at": "",
            "pusher": None,
            "session_id": "",
            "refs": {},
            "peeled_refs": {},
            "head": "refs/heads/main",
            "shard_index_hash": "",
            "pack_index_hash": "",
            "git_validation_digest": "7e9adc65b2225882ef7ae62b6cdfd8b13383f536152b55c47a9f54ce3db37ea8",
            "commit_graph_hash": None,
            "ref_registry_hash": None,
        }).encode()
        manifest_status, _, _ = self.signed_s3_request(
            "PUT", f"{domain}/manifest", body=legacy_manifest,
            extra_headers={"if-none-match": "*"},
        )
        self.check("legacy-manifest-fixture-created-only-if-absent", manifest_status == 200)
        key = f"{domain}/locks/internal/gc-fence/state"
        legacy = json.dumps({
            "schema_version": 1, "epoch": 0, "writer_epoch": 0,
            "writers": [], "sweep": None, "quarantine": [],
            "quarantine_block_until_backend": None,
        }).encode()
        status, _, _ = self.signed_s3_request("PUT", key, body=legacy, extra_headers={"if-none-match": "*"})
        self.check("migration-fixture-created-only-if-absent", status == 200)
        args = ["migrate", "gc-fence", "--remote", remote, "--domain", domain]
        self.run_crab(repo, args, name="plan coordination migration")
        status, _, body = self.signed_s3_request("GET", key)
        self.check("migration-plan-does-not-change-state", status == 200 and body == legacy)
        self.run_crab(repo, args + ["--apply", "--quiesced"], name="apply coordination migration")
        status, _, upgraded = self.signed_s3_request("GET", key)
        state = json.loads(upgraded)
        self.check("migration-persists-new-incarnation", status == 200 and state["schema_version"] == 2 and bool(state["incarnation"]))
        self.run_crab(repo, args + ["--apply", "--quiesced"], name="repeat coordination migration")
        status, _, repeated = self.signed_s3_request("GET", key)
        self.check("migration-repeat-is-idempotent", status == 200 and repeated == upgraded)
        self.run_git(repo, ["commit", "--allow-empty", "-m", "qualification"])
        before = self.run_git(repo, ["ls-remote", remote], name="refs before old writer")
        old = self.run_cmd("tagged old writer refusal", [str(Path(self.args.rollback_crab_bin).resolve()), "push", "--json"], repo, check=False)
        diagnostic = Path(old.stderr_log).read_text() + Path(old.stdout_log).read_text()
        self.check("old-writer-refuses-new-fence-contract", old.exit_code != 0 and "incarnation" in diagnostic, {"exit_code": old.exit_code})
        after = self.run_git(repo, ["ls-remote", remote], name="refs after old writer")
        status, _, after_refusal = self.signed_s3_request("GET", key)
        self.check("old-writer-leaves-fence-and-refs-unchanged", status == 200 and after_refusal == upgraded and self.read_stdout(before) == self.read_stdout(after))
        self.report.artifacts["rollback_binary_sha256"] = sha256_file(Path(self.args.rollback_crab_bin))

    def run(self) -> None:
        if self.args.source:
            self.run_source_workflows()
            return
        self.preflight()
        if self.args.only_gc_fence_upgrade:
            self.run_gc_fence_upgrade_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        if self.args.only_xorb:
            self.run_v1_hard_cutover_reset_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        if self.args.only_cross_repo_duplicate:
            self.run_cross_repository_remote_duplicate_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        if self.args.only_partial_overlap:
            self.run_partial_overlap_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        if self.args.only_committed_restage:
            self.run_committed_restage_before_first_push_case()
            self.check_credential_disclosure()
            self.report.status = "passed"
            self.write_report()
            return
        self.run_v1_hard_cutover_reset_case()
        self.run_missing_root_case()
        self.run_corrupt_root_case()
        self.run_ref_update_delete_force_and_tag_case()
        self.run_atomic_rejection_case()
        self.run_shallow_case()
        for fault in ("missing", "pack", "index"):
            self.run_capsule_failure_case(fault)
        self.run_concurrent_push_case()
        self.run_committed_restage_before_first_push_case()
        self.run_partial_overlap_case()
        self.run_cross_repository_remote_duplicate_case()
        self.run_case("crab-add-crab-push", use_crab_add=True)
        self.run_case("git-add-git-push", use_crab_add=False)
        self.check_credential_disclosure()
        self.report.status = "passed"
        self.write_report()

    def run_source_workflows(self) -> None:
        # Never let a fixture checkout or an existing run become a write target.
        source = self.args.source.resolve()
        run_root = self.run_root.resolve()
        workspace = (Path.home() / "Workspace").resolve()
        if not run_root.is_relative_to(workspace) or run_root == workspace:
            raise SmokeError("real-repository run root must be beneath the workspace volume")
        if run_root.is_relative_to(source) or source.is_relative_to(run_root):
            raise SmokeError("source checkout and run root must not overlap")
        if run_root.exists():
            raise SmokeError("real-repository run root already exists")
        self.args.source = source
        self.preflight()
        source_head = self.rev_parse(source, "HEAD")
        status_args = ["--no-optional-locks", "status", "--porcelain=v1", "--untracked-files=all"]
        source_status = self.read_stdout(self.run_git(source, status_args))
        self.report.artifacts.update({
            "fixture_source": str(source),
            "fixture_source_head": source_head,
            "qualification_scope": "real-repository managed-file workflows; not performance comparison",
        })
        self.write_report()
        try:
            self.run_case("crab-add-crab-push", use_crab_add=True)
            self.run_case("git-add-git-push", use_crab_add=False)
        finally:
            self.check(
                "fixture-source-checkout-unchanged",
                self.rev_parse(source, "HEAD") == source_head
                and self.read_stdout(self.run_git(source, status_args)) == source_status,
                {"source_head": source_head},
            )
        self.check(
            "selected-binary-unchanged",
            sha256_file(Path(self.crab_bin)) == self.report.artifacts["crab_binary_sha256"],
        )
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
    parser.add_argument(
        "--source", type=Path,
        help="read-only repository for the two managed-file workflows (skips synthetic Git matrix)",
    )
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
    parser.add_argument("--only-xorb", action="store_true")
    parser.add_argument("--only-partial-overlap", action="store_true")
    parser.add_argument("--only-committed-restage", action="store_true")
    parser.add_argument("--only-gc-fence-upgrade", action="store_true")
    parser.add_argument("--rollback-crab-bin", type=Path)
    args = parser.parse_args()
    if args.only_gc_fence_upgrade and not args.rollback_crab_bin:
        parser.error("--only-gc-fence-upgrade requires --rollback-crab-bin")
    if args.only_gc_fence_upgrade and any((args.source, args.only_xorb, args.only_cross_repo_duplicate, args.only_partial_overlap, args.only_committed_restage)):
        parser.error("--only-gc-fence-upgrade cannot be combined with another case selector")
    if args.source and any((args.only_xorb, args.only_cross_repo_duplicate, args.only_partial_overlap, args.only_committed_restage)):
        parser.error("--source cannot be combined with a synthetic-case selector")
    if args.only_xorb and any((args.only_cross_repo_duplicate, args.only_partial_overlap, args.only_committed_restage)):
        parser.error("--only-xorb cannot be combined with another synthetic-case selector")
    return args


def main() -> int:
    args = parse_args()
    smoke = AddCommitPushSmoke(args)
    try:
        smoke.run()
    except Exception as exc:
        smoke.report.status = "failed"
        diagnostic = redact_credential_text(str(exc), smoke.credentials())
        # An unsafe source/run overlap is rejected before any report is created.
        if smoke.report_initialized:
            # Unexpected driver failures must produce terminal evidence too;
            # otherwise CI can retain a stopped run with status "running".
            if not isinstance(exc, SmokeError):
                smoke.report.checks.append({
                    "name": "driver-completed",
                    "ok": False,
                    "detail": {"error_type": type(exc).__name__, "error": diagnostic},
                    "timestamp": utc_now(),
                })
            smoke.write_report()
        print(f"FAILED: {diagnostic}", file=os.sys.stderr)
        if smoke.report_initialized:
            print(f"report: {smoke.report.artifacts.get('report', '')}", file=os.sys.stderr)
        return 1
    print("PASS add/commit/push RustFS smoke")
    print(f"report: {smoke.report.artifacts.get('report', '')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
