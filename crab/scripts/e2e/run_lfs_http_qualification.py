#!/usr/bin/env python3
"""Qualify unmodified Git LFS against crab-lfs-server and an S3 origin.

The harness starts an isolated gateway, creates a local bare Git remote, and
drives the real Git LFS client through signed Batch actions. It verifies
upload, Git ref publication, download byte identity, ``git lfs fsck``, and
File Locking. The root must be unique and is retained as evidence on failure.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence
from urllib.error import URLError
from urllib.request import urlopen


DEFAULT_ENDPOINT = os.environ.get("AWS_ENDPOINT_URL", "http://127.0.0.1:9000")
DEFAULT_MIN_SIZE = 1024 * 1024
DEFAULT_MAX_SIZE = 2 * 1024 * 1024
CHUNK_SIZE = 1024 * 1024
PASSWORD_HASH = "513aa29dba88e034b1f55f1f8f488c781e5823c21e852603dc84a3807421590f"
URL_RE = re.compile(r"https?://[^\s\"']+")
RUN_ID_RE = re.compile(r"[^A-Za-z0-9_.-]+")


class QualificationError(RuntimeError):
    """Raised when the harness cannot produce trustworthy evidence."""


def executable(value: str, label: str) -> str:
    resolved = shutil.which(value) if not Path(value).is_absolute() else value
    if resolved is None or not Path(resolved).is_file() or not os.access(resolved, os.X_OK):
        raise QualificationError(f"{label} is not executable: {value}")
    return str(Path(resolved).resolve())


def redact_text(value: str, secrets_to_hide: Sequence[str]) -> str:
    result = value
    for secret in sorted({item for item in secrets_to_hide if item}, key=len, reverse=True):
        result = result.replace(secret, "<redacted>")
    return URL_RE.sub("<redacted-url>", result)


def toml_string(value: str) -> str:
    return json.dumps(value)


def deterministic_block(seed: str, object_index: int, block_index: int) -> bytes:
    return hashlib.sha256(
        f"crab-lfs-http:{seed}:{object_index}:{block_index}".encode("utf-8")
    ).digest()


def write_payload(path: Path, size: int, seed: str, object_index: int) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    remaining = size
    block_index = 0
    with path.open("wb") as handle:
        while remaining:
            block = deterministic_block(seed, object_index, block_index)
            chunk_size = min(remaining, CHUNK_SIZE)
            chunk = (block * ((chunk_size + len(block) - 1) // len(block)))[:chunk_size]
            handle.write(chunk)
            digest.update(chunk)
            remaining -= chunk_size
            block_index += 1
    return digest.hexdigest()


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.hexdigest()


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


class Qualification:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = args.root.resolve()
        if self.root.exists():
            raise QualificationError(f"qualification root already exists: {self.root}")
        if args.object_count < args.path_count:
            raise QualificationError("object-count must be at least path-count")
        if args.object_count < args.commit_count:
            raise QualificationError("object-count must be at least commit-count")
        if args.object_count < 1 or args.commit_count < 1 or args.path_count < 1:
            raise QualificationError("object-count, commit-count, and path-count must be positive")
        if args.min_size < 0 or args.max_size < args.min_size:
            raise QualificationError("size range is invalid")
        if args.max_size == 0:
            raise QualificationError("max-size must be greater than zero")
        self.server_bin = executable(args.server_bin, "crab-lfs-server binary")
        self.git_bin = executable(args.git_bin, "Git binary")
        self.git_lfs_bin = executable(args.git_lfs_bin, "Git LFS binary")
        access_key = os.environ.get("AWS_ACCESS_KEY_ID", "")
        secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY", "")
        if not access_key or not secret_key:
            raise QualificationError(
                "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are required in the environment"
            )
        self.hidden_secrets = (access_key, secret_key)
        self.remote = self.root / "remote.git"
        self.source = self.root / "source"
        self.clone = self.root / "clone"
        self.spool = self.root / "spool"
        self.server_log = self.root / "server.log"
        self.config = self.root / "server.toml"
        self.port = free_port()
        self.repository = f"http-{RUN_ID_RE.sub('-', self.root.name).strip('-') or 'qualification'}"
        self.server_url = f"http://127.0.0.1:{self.port}"
        self.origin_prefix = f"e2e-lfs-http/{self.repository}"
        self.action_secret = secrets.token_hex(32)
        self.hidden_secrets = (*self.hidden_secrets, self.action_secret)
        self.server: subprocess.Popen[str] | None = None
        self.report: dict[str, Any] = {
            "schema": "crab.lfs-http-qualification",
            "version": "1.0",
            "status": "running",
            "started_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "finished_at": None,
            "root": str(self.root),
            "endpoint": self.server_url,
            "repository": self.repository,
            "workload": {
                "object_count": args.object_count,
                "commit_count": args.commit_count,
                "path_count": args.path_count,
                "min_size": args.min_size,
                "max_size": args.max_size,
            },
            "checks": [],
            "objects": [],
            "error": None,
        }
        self.final_objects: dict[str, dict[str, Any]] = {}

    def save_report(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        target = self.root / "report.json"
        temporary = target.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(self.report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(target)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report["checks"].append({"name": name, "ok": ok, "detail": detail or {}})
        self.save_report()
        if not ok:
            raise QualificationError(f"check failed: {name}")

    def environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        endpoint = self.args.endpoint_url.rstrip("/")
        environment.update(
            {
                "AWS_ENDPOINT_URL": endpoint,
                "AWS_ENDPOINT_URL_S3": endpoint,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_AUTHOR_NAME": "Crab LFS HTTP qualification",
                "GIT_AUTHOR_EMAIL": "crab-lfs-http@example.invalid",
                "GIT_COMMITTER_NAME": "Crab LFS HTTP qualification",
                "GIT_COMMITTER_EMAIL": "crab-lfs-http@example.invalid",
                "GIT_LFS_SKIP_SMUDGE": "1",
                "CRAB_LFS_ACTION_SECRET": self.action_secret,
            }
        )
        return environment

    def redact_server_log(self) -> None:
        if not self.server_log.exists():
            return
        contents = self.server_log.read_text(encoding="utf-8")
        self.server_log.write_text(
            redact_text(contents, self.hidden_secrets), encoding="utf-8"
        )

    def run_command(
        self,
        name: str,
        command: Sequence[str],
        cwd: Path,
        *,
        environment: dict[str, str] | None = None,
    ) -> str:
        started = time.monotonic()
        completed = subprocess.run(
            list(command),
            cwd=cwd,
            env=environment or self.environment(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
            timeout=self.args.command_timeout,
        )
        stdout = redact_text(completed.stdout, self.hidden_secrets)
        stderr = redact_text(completed.stderr, self.hidden_secrets)
        if completed.returncode != 0:
            detail = stderr.strip() or stdout.strip() or "no command output"
            raise QualificationError(
                f"{name} failed with exit code {completed.returncode}: {detail}"
            )
        self.report.setdefault("commands", []).append(
            {
                "name": name,
                "argv": [redact_text(str(item), self.hidden_secrets) for item in command],
                "cwd": str(cwd),
                "duration_ms": int((time.monotonic() - started) * 1000),
                "stdout": stdout[-8192:],
                "stderr": stderr[-8192:],
            }
        )
        self.save_report()
        return stdout

    def git(self, args: Sequence[str], cwd: Path, name: str) -> str:
        return self.run_command(name, [self.git_bin, *args], cwd)

    def lfs(self, args: Sequence[str], cwd: Path, name: str) -> str:
        return self.run_command(name, [self.git_lfs_bin, *args], cwd)

    def write_config(self) -> None:
        self.root.mkdir(parents=True)
        self.config.write_text(
            "\n".join(
                [
                    "[server]",
                    'listen_addr = "127.0.0.1:%d"' % self.port,
                    'public_url = "http://127.0.0.1:%d"' % self.port,
                    f"spool_dir = {toml_string(str(self.spool))}",
                    "max_batch_objects = 1000",
                    f"max_object_bytes = {self.args.max_size}",
                    "max_uploads = 4",
                    "request_timeout_secs = 600",
                    "",
                    "[auth]",
                    'mechanism = "basic"',
                    "",
                    "[auth.users]",
                    f'crab = "{PASSWORD_HASH}"',
                    "",
                    "[origin]",
                    f'url = {toml_string(f"s3://{self.args.bucket}/{self.origin_prefix}")}',
                    "",
                ]
            ),
            encoding="utf-8",
        )

    def start_server(self) -> None:
        environment = self.environment()
        self.server_log.parent.mkdir(parents=True, exist_ok=True)
        log = self.server_log.open("w", encoding="utf-8")
        self.server = subprocess.Popen(
            [self.server_bin, "--config", str(self.config)],
            cwd=self.root,
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            text=True,
        )
        deadline = time.monotonic() + self.args.startup_timeout
        health_url = f"{self.server_url}/healthz"
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                break
            try:
                with urlopen(health_url, timeout=1) as response:
                    if response.status == 200:
                        log.close()
                        return
            except (OSError, URLError):
                time.sleep(0.25)
        log.close()
        self.redact_server_log()
        detail = self.server_log.read_text(encoding="utf-8")
        raise QualificationError(f"crab-lfs-server did not become healthy: {detail[-4096:]}")

    def stop_server(self) -> None:
        if self.server is None or self.server.poll() is not None:
            return
        self.server.send_signal(signal.SIGTERM)
        try:
            self.server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.server.kill()
            self.server.wait(timeout=10)

    def setup_repository(self) -> None:
        self.remote.mkdir(parents=True)
        self.source.mkdir(parents=True)
        self.git(["init", "--bare", str(self.remote)], self.root, "git init bare remote")
        self.git(["init", "--initial-branch=main"], self.source, "git init source")
        self.git(["config", "user.name", "Crab LFS HTTP qualification"], self.source, "git identity name")
        self.git(["config", "user.email", "crab-lfs-http@example.invalid"], self.source, "git identity email")
        self.git(["remote", "add", "origin", str(self.remote)], self.source, "git add origin")
        self.git(
            ["config", "lfs.url", f"{self.server_url}/{self.repository}.git/info/lfs"],
            self.source,
            "configure HTTP LFS URL",
        )
        self.git(
            [
                "config",
                "credential.helper",
                "!f() { echo username=crab; echo password=crab; }; f",
            ],
            self.source,
            "configure test credentials",
        )
        self.lfs(["install", "--local"], self.source, "git lfs install")
        self.lfs(["track", "*.bin"], self.source, "git lfs track")
        self.git(["add", ".gitattributes"], self.source, "stage LFS attributes")
        self.git(["commit", "-m", "configure Git LFS"], self.source, "commit LFS attributes")

    def create_history(self) -> None:
        for index in range(self.args.object_count):
            span = self.args.max_size - self.args.min_size
            size = self.args.min_size + ((index * 1_000_003) % (span + 1)) if span else self.args.min_size
            path = self.source / f"asset-{index % self.args.path_count:05d}.bin"
            oid = write_payload(path, size, self.args.seed, index)
            item = {"index": index, "path": path.name, "size": size, "oid": oid}
            self.report["objects"].append(item)
            self.final_objects[path.name] = item
            should_commit = (
                index == self.args.object_count - 1
                or (index + 1) * self.args.commit_count // self.args.object_count
                > index * self.args.commit_count // self.args.object_count
            )
            if should_commit:
                commit_index = (index + 1) * self.args.commit_count // self.args.object_count
                staged = [
                    f"asset-{path_index:05d}.bin"
                    for path_index in range(self.args.path_count)
                    if (self.source / f"asset-{path_index:05d}.bin").exists()
                ]
                self.git(["add", "--", *staged], self.source, f"stage objects {index + 1}")
                self.git(
                    ["commit", "-m", f"qualification commit {commit_index}"],
                    self.source,
                    f"commit {commit_index}",
                )
        self.report["logical_bytes"] = sum(item["size"] for item in self.report["objects"])
        self.save_report()

    def configure_clone(self) -> None:
        self.git(["config", "lfs.url", f"{self.server_url}/{self.repository}.git/info/lfs"], self.clone, "configure clone LFS URL")
        self.git(
            [
                "config",
                "credential.helper",
                "!f() { echo username=crab; echo password=crab; }; f",
            ],
            self.clone,
            "configure clone credentials",
        )
        self.lfs(["install", "--local"], self.clone, "install clone LFS")

    def qualify(self) -> None:
        self.write_config()
        self.start_server()
        self.setup_repository()
        self.create_history()
        self.lfs(["push", "origin", "main"], self.source, "git lfs push")
        self.git(["push", "origin", "main"], self.source, "git push")
        local_ref = self.git(["rev-parse", "HEAD"], self.source, "read local ref").strip()
        remote_ref = self.git(
            ["ls-remote", str(self.remote), "refs/heads/main"], self.source, "read remote ref"
        ).split()[0]
        self.check("git-ref-equality", local_ref == remote_ref, {"local": local_ref, "remote": remote_ref})
        self.lfs(["fsck"], self.source, "source git lfs fsck")

        self.git(["clone", str(self.remote), str(self.clone)], self.root, "clone with smudge disabled")
        self.configure_clone()
        self.lfs(["pull"], self.clone, "git lfs pull")
        self.lfs(["fsck"], self.clone, "clone git lfs fsck")
        for item in self.final_objects.values():
            path = self.clone / item["path"]
            self.check(
                f"byte-identity-{item['index']}",
                path.is_file() and path.stat().st_size == item["size"] and hash_file(path) == item["oid"],
                {"path": item["path"], "size": item["size"]},
            )

        lock_path = next(iter(self.final_objects))
        self.git(["config", f"lfs.{self.server_url}/{self.repository}.git/info/lfs.locksverify", "true"], self.source, "enable LFS locking")
        self.lfs(["lock", lock_path], self.source, "git lfs lock")
        self.lfs(["locks"], self.source, "git lfs locks")
        self.lfs(["unlock", lock_path], self.source, "git lfs unlock")
        self.check("file-locking-round-trip", True)

    def run(self) -> None:
        try:
            self.qualify()
            self.report["status"] = "passed"
        except (OSError, QualificationError, subprocess.SubprocessError) as error:
            self.report["status"] = "failed"
            self.report["error"] = redact_text(str(error), self.hidden_secrets)
            raise
        finally:
            self.stop_server()
            self.redact_server_log()
            self.report["finished_at"] = datetime.now(timezone.utc).isoformat(timespec="seconds")
            self.save_report()


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-bin", default="crab-lfs-server")
    parser.add_argument("--git-bin", default="git")
    parser.add_argument("--git-lfs-bin", default="git-lfs")
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--bucket", default="crab")
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--object-count", type=int, default=10)
    parser.add_argument("--commit-count", type=int, default=3)
    parser.add_argument("--path-count", type=int, default=10)
    parser.add_argument("--min-size", type=int, default=DEFAULT_MIN_SIZE)
    parser.add_argument("--max-size", type=int, default=DEFAULT_MAX_SIZE)
    parser.add_argument("--seed", default="crab-lfs-http-qualification")
    parser.add_argument("--startup-timeout", type=float, default=30)
    parser.add_argument("--command-timeout", type=float, default=900)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    qualification: Qualification | None = None
    try:
        qualification = Qualification(parse_args(argv or sys.argv[1:]))
        qualification.run()
        print(json.dumps({"status": "passed", "report": str(qualification.root / "report.json")}))
        return 0
    except (OSError, QualificationError, subprocess.SubprocessError) as error:
        if qualification is not None:
            print(
                json.dumps(
                    {
                        "status": "failed",
                        "report": str(qualification.root / "report.json"),
                        "error": redact_text(str(error), qualification.hidden_secrets),
                    }
                ),
                file=sys.stderr,
            )
        else:
            print(f"qualification failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
