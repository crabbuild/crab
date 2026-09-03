#!/usr/bin/env python3
"""Qualify the client-side Git v2 partial-clone matrix against object storage.

The runner creates a disposable Crab remote, invokes the real Git executable,
and retains a redacted JSON report with packet traces, promisor state, local
ODB sizes, remote object-store size, remote-reader request telemetry, process
resource measurements, and source/artifact provenance. It intentionally does
not start or require a Crab service: the helper talks directly to the
configured object store.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import signal
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from collections.abc import Callable

try:
    import resource
except ImportError:  # pragma: no cover - Windows has no resource module.
    resource = None  # type: ignore[assignment]


DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
DEFAULT_ROOT = Path.home() / "Workspace" / "CrabRepos"
REMOTE_PREFIX = "e2e-protocol-v2-partial"
SECRET_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}
SECRET_FLAGS = {"--access-key", "--secret-key", "--session-token"}


class SmokeError(RuntimeError):
    """Raised when a lifecycle proof fails."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "protocol-v2-partial-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    result = "".join(char if char.isalnum() or char in "._-" else "-" for char in value.lower())
    return result.strip("-") or "command"


def redact_text(text: str, credentials: dict[str, str]) -> str:
    for value in credentials.values():
        if value and value != "crab":
            text = text.replace(value, "<redacted>")
    return text


def redact_args(args: list[str]) -> list[str]:
    result: list[str] = []
    redact_next = False
    for arg in args:
        if redact_next:
            result.append("<redacted>")
            redact_next = False
            continue
        if arg in SECRET_FLAGS:
            result.append(arg)
            redact_next = True
            continue
        flag, separator, _value = arg.partition("=")
        if separator and flag in SECRET_FLAGS:
            result.append(f"{flag}=<redacted>")
        else:
            result.append(arg)
    return result


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def deterministic_bytes(size: int, seed: str) -> bytes:
    result = bytearray()
    counter = 0
    while len(result) < size:
        result.extend(hashlib.sha256(f"{seed}:{counter}".encode()).digest())
        counter += 1
    return bytes(result[:size])


@contextmanager
def hold_cache_lock(path: Path):
    """Hold the native lock used by Crab without replacing its inode."""
    with path.open("a+b") as lock:
        lock.seek(0)
        if os.name == "nt":
            import msvcrt
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield


class ProtocolV2PartialCloneSmoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        self.run_root = args.root / self.run_id
        if self.run_root.exists():
            raise SmokeError(f"run root already exists: {self.run_root}")
        self.run_root.mkdir(parents=True)
        self.temp_root = self.run_root / "tmp"
        self.temp_root.mkdir()
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.bin_dir = self.run_root / "bin"
        self.source = self.run_root / "source"
        self.full = self.run_root / "full-clone"
        self.legacy = self.run_root / "legacy-clone"
        self.shallow = self.run_root / "shallow-clone"
        self.filtered = self.run_root / "filtered-clone"
        self.performance_root = self.run_root / "performance"
        self.crab_pointer_bytes: bytes | None = None
        self.lfs_pointer_bytes: bytes | None = None
        self.fixture_branches = [
            ("normal-blobs", "main"),
            ("deep-history", "fixture/deep"),
            ("many-small-files", "fixture/many-small"),
            ("pointer-heavy", "fixture/pointers"),
        ]
        self.command_index = 0
        self.command_outputs: dict[str, str] = {}
        self.trace_redactions: set[str] = set()
        self.crab_bin = self.resolve_crab(args.crab_bin)
        self.rollback_crab_bin = (
            self.resolve_crab(args.rollback_crab_bin) if args.rollback_crab_bin else None
        )
        self.rollback_crab_tag = args.rollback_crab_tag
        self.git_bin = self.resolve_git(args.git_bin)
        self.crab_source = self.resolve_source(args.source_root)
        self.remote_url = f"crab://{args.bucket}/{REMOTE_PREFIX}/{self.run_id}"
        self.env = self.build_env()
        self.report: dict[str, Any] = {
            "schema": "crab.protocol-v2-partial-clone-smoke",
            "version": "1.1",
            "run_id": self.run_id,
            "status": "running",
            "root": str(self.run_root),
            "remote_url": self.remote_url,
            "bucket": args.bucket,
            "endpoint_url": args.endpoint_url,
            "backend": args.backend,
            "require_existing_bucket": args.require_existing_bucket,
            "env": self.redacted_env(),
            "commands": [],
            "checks": [],
            "store_snapshots": [],
            "telemetry": {},
            "provenance": {},
            "artifacts": {},
            "performance": {},
            "updated_at": utc_now(),
        }
        self.install_helper_alias()
        self.write_report()

    def resolve_crab(self, value: str) -> Path:
        candidate = Path(value)
        if not candidate.is_absolute():
            located = shutil.which(value)
            if located:
                candidate = Path(located)
        candidate = candidate.resolve()
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            raise SmokeError(f"crab binary is not executable: {candidate}")
        return candidate

    def resolve_source(self, value: Path) -> Path:
        candidate = value.resolve()
        if not (candidate / ".git").exists():
            raise SmokeError(f"Crab source root is not a Git checkout: {candidate}")
        return candidate

    def resolve_git(self, value: str) -> Path:
        located = shutil.which(value)
        if located is None:
            candidate = Path(value)
            if not candidate.is_absolute():
                raise SmokeError(f"Git executable is not available: {value}")
        else:
            candidate = Path(located)
        candidate = candidate.resolve()
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            raise SmokeError(f"Git executable is not executable: {candidate}")
        return candidate

    def git_supports_filter(self, filter_spec: str) -> tuple[bool, str]:
        probe = subprocess.run(
            [
                str(self.git_bin),
                "-C",
                str(self.source),
                "rev-list",
                "--objects",
                f"--filter={filter_spec}",
                "--all",
            ],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        return probe.returncode == 0, probe.stderr.strip()

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_DEFAULT_REGION": self.args.region,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_AUTHOR_NAME": "Crab protocol smoke",
                "GIT_AUTHOR_EMAIL": "smoke@example.invalid",
                "GIT_COMMITTER_NAME": "Crab protocol smoke",
                "GIT_COMMITTER_EMAIL": "smoke@example.invalid",
                "CRAB_LOG": "crab=debug,crab_remote_git=debug",
            }
        )
        if self.args.endpoint_url:
            env["AWS_ENDPOINT_URL"] = self.args.endpoint_url
            env["AWS_ENDPOINT_URL_S3"] = self.args.endpoint_url
        else:
            env.pop("AWS_ENDPOINT_URL", None)
            env.pop("AWS_ENDPOINT_URL_S3", None)
        if self.args.session_token:
            env["AWS_SESSION_TOKEN"] = self.args.session_token
        else:
            env.pop("AWS_SESSION_TOKEN", None)
        env["TMPDIR"] = str(self.temp_root)
        env["TMP"] = str(self.temp_root)
        env["TEMP"] = str(self.temp_root)
        env["PATH"] = str(self.bin_dir) + os.pathsep + env.get("PATH", "")
        return env

    def temp_disk_bytes(self) -> int:
        total = 0
        for path in self.temp_root.rglob("*"):
            try:
                if path.is_file():
                    total += path.stat().st_size
            except FileNotFoundError:
                continue
        return total

    def child_usage(self) -> dict[str, int]:
        if resource is None:
            return {"user_cpu_ms": 0, "system_cpu_ms": 0, "children_max_rss": 0}
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        return {
            "user_cpu_ms": int(usage.ru_utime * 1000),
            "system_cpu_ms": int(usage.ru_stime * 1000),
            "children_max_rss": int(usage.ru_maxrss),
        }

    def process_tree_rss_bytes(self, root_pid: int) -> int:
        """Return a sampled high-water RSS for a child process tree."""
        if sys.platform.startswith("linux"):
            process_info: dict[int, tuple[int, int]] = {}
            for status_path in Path("/proc").glob("[0-9]*/status"):
                try:
                    fields = {
                        line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                        for line in status_path.read_text(encoding="utf-8").splitlines()
                        if ":" in line
                    }
                    pid = int(status_path.parent.name)
                    parent = int(fields.get("PPid", "0"))
                    rss = fields.get("VmHWM", fields.get("VmRSS", "0")).split()[0]
                    process_info[pid] = (parent, int(rss) * 1024)
                except (OSError, ValueError, IndexError):
                    continue
        elif os.name != "nt":
            try:
                output = subprocess.run(
                    ["ps", "-axo", "pid=,ppid=,rss="],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                    check=False,
                ).stdout
            except OSError:
                return 0
            process_info = {}
            for line in output.splitlines():
                fields = line.split()
                if len(fields) != 3:
                    continue
                try:
                    pid, parent, rss_kib = (int(value) for value in fields)
                except ValueError:
                    continue
                process_info[pid] = (parent, rss_kib * 1024)
        else:
            try:
                output = subprocess.run(
                    [
                        "tasklist",
                        "/FI",
                        f"PID eq {root_pid}",
                        "/FO",
                        "CSV",
                        "/NH",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                    check=False,
                ).stdout
                fields = next(csv.reader(output.splitlines()), [])
                if len(fields) < 5 or fields[1] != str(root_pid):
                    return 0
                value = fields[4].replace(",", "").split()[0]
                return int(value) * 1024
            except (OSError, StopIteration, ValueError, IndexError):
                return 0

        children: dict[int, list[int]] = {}
        for pid, (parent, _rss) in process_info.items():
            children.setdefault(parent, []).append(pid)
        pending = [root_pid]
        tree: set[int] = set()
        while pending:
            pid = pending.pop()
            if pid in tree:
                continue
            tree.add(pid)
            pending.extend(children.get(pid, []))
        return sum(process_info[pid][1] for pid in tree if pid in process_info)

    def resource_delta(
        self,
        before: dict[str, int],
        after: dict[str, int],
        temp_before: int,
        temp_after: int,
        temp_peak: int,
        rss_peak: int,
    ) -> dict[str, int | str]:
        return {
            "user_cpu_ms": max(0, after["user_cpu_ms"] - before["user_cpu_ms"]),
            "system_cpu_ms": max(0, after["system_cpu_ms"] - before["system_cpu_ms"]),
            "children_max_rss": rss_peak,
            "children_max_rss_unit": "bytes",
            "children_max_rss_scope": "sampled_process_tree",
            "temp_disk_peak_bytes": max(temp_before, temp_after, temp_peak),
            "temp_disk_after_bytes": temp_after,
        }

    def terminate_process_tree(self, process: subprocess.Popen[bytes]) -> None:
        if os.name == "nt":
            subprocess.run(
                ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            return
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    def run_process(
        self,
        args: list[str],
        cwd: Path,
        env: dict[str, str],
        input_data: bytes | None,
        timeout: int,
        on_started: Callable[[subprocess.Popen[bytes]], None] | None = None,
    ) -> tuple[int, bytes, bytes, int, int]:
        """Run one child while sampling session temporary space."""
        started = time.monotonic()
        temp_peak = self.temp_disk_bytes()
        rss_peak = 0
        process: subprocess.Popen[bytes] | None = None
        timed_out = False
        with tempfile.TemporaryFile() as stdout_stream, tempfile.TemporaryFile() as stderr_stream:
            try:
                process = subprocess.Popen(
                    args,
                    cwd=cwd,
                    env=env,
                    stdin=subprocess.PIPE,
                    stdout=stdout_stream,
                    stderr=stderr_stream,
                    creationflags=(
                        subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
                    ),
                    start_new_session=os.name != "nt",
                )
                rss_peak = max(rss_peak, self.process_tree_rss_bytes(process.pid))
                if process.stdin is not None:
                    if input_data is not None:
                        try:
                            process.stdin.write(input_data)
                            process.stdin.flush()
                        except BrokenPipeError:
                            pass
                    process.stdin.close()

                if on_started is not None:
                    on_started(process)

                while process.poll() is None:
                    temp_peak = max(temp_peak, self.temp_disk_bytes())
                    rss_peak = max(rss_peak, self.process_tree_rss_bytes(process.pid))
                    if time.monotonic() - started >= timeout:
                        self.terminate_process_tree(process)
                        timed_out = True
                        break
                    time.sleep(0.05)
                exit_code = process.wait()
                temp_peak = max(temp_peak, self.temp_disk_bytes())
                rss_peak = max(rss_peak, self.process_tree_rss_bytes(process.pid))
            except BaseException:
                if process is not None and process.poll() is None:
                    self.terminate_process_tree(process)
                    process.wait()
                raise

            stdout_stream.seek(0)
            stderr_stream.seek(0)
            stdout = stdout_stream.read()
            stderr = stderr_stream.read()

        if timed_out:
            stderr += f"\ncommand timed out after {timeout} seconds\n".encode()
            exit_code = -124
        return exit_code, stdout, stderr, temp_peak, rss_peak

    def credentials(self) -> dict[str, str]:
        return {
            "access_key": self.args.access_key,
            "secret_key": self.args.secret_key,
            "session_token": self.args.session_token,
        }

    def add_trace_redactions(self, *values: str) -> None:
        self.trace_redactions.update(value for value in values if value)

    def redact_sensitive(self, text: str) -> str:
        for value in sorted(self.trace_redactions, key=len, reverse=True):
            text = text.replace(value, "<oid>")
        return text

    def redacted_env(self) -> dict[str, str]:
        result: dict[str, str] = {}
        for key, value in sorted(self.env.items()):
            if key in SECRET_KEYS:
                result[key] = "<redacted>"
            elif key.startswith(("AWS_", "CRAB_", "GIT_")):
                result[key] = value
        return result

    def install_helper_alias(self, crab_bin: Path | None = None) -> None:
        self.bin_dir.mkdir(parents=True, exist_ok=True)
        target = crab_bin or self.crab_bin
        # Hooks invoke `crab` via PATH. Pin that command too, otherwise the
        # fixture can silently qualify an unrelated installed binary. Only
        # the remote helper changes during the prior-release rollback probe.
        for name, executable in (("git-remote-crab", target), ("crab", self.crab_bin)):
            alias = self.bin_dir / name
            if alias.is_symlink() or alias.exists():
                alias.unlink()
            try:
                alias.symlink_to(executable)
            except (NotImplementedError, OSError):
                shutil.copy2(executable, alias)
            if os.name == "nt":
                windows_alias = self.bin_dir / f"{name}.exe"
                if windows_alias.exists():
                    windows_alias.unlink()
                shutil.copy2(executable, windows_alias)

    def configure_reachable_oid_admission(self, repo: Path, enabled: bool) -> None:
        """Set the internal policy used by a fixture's raw-OID probes."""
        policy_path = repo / ".crab" / "local.toml"
        policy_path.parent.mkdir(parents=True, exist_ok=True)
        policy = policy_path.read_text(encoding="utf-8") if policy_path.exists() else ""
        value = "true" if enabled else "false"
        if "[uploadpack]" not in policy:
            if policy and not policy.endswith("\n"):
                policy += "\n"
            policy += f"\n[uploadpack]\nallowReachableSHA1InWant = {value}\n"
        else:
            policy = re.sub(
                r"(?m)^allow(?:ReachableSHA1InWant|_reachable_sha_in_want)\s*=\s*.*$",
                f"allowReachableSHA1InWant = {value}",
                policy,
            )
        policy_path.write_text(policy, encoding="utf-8")

    def configure_hidden_refs(self, repo: Path) -> None:
        """Hide the security fixture's branch through internal config."""
        policy_path = repo / ".crab" / "local.toml"
        policy_path.parent.mkdir(parents=True, exist_ok=True)
        policy = policy_path.read_text(encoding="utf-8") if policy_path.exists() else ""
        if "[transfer]" not in policy:
            if policy and not policy.endswith("\n"):
                policy += "\n"
            policy += '\n[transfer]\nhideRefs = ["refs/heads/hidden"]\n'
        else:
            policy = re.sub(
                r"(?m)^hide(?:Refs|_refs)\s*=\s*.*$",
                'hideRefs = ["refs/heads/hidden"]',
                policy,
            )
        policy_path.write_text(policy, encoding="utf-8")

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.report["updated_at"] = utc_now()
        path = self.artifacts / "report.json"
        path.write_text(json.dumps(self.report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.report["artifacts"]["report"] = str(path)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report["checks"].append(
            {"name": name, "ok": ok, "detail": detail or {}, "timestamp": utc_now()}
        )
        self.write_report()
        if not ok:
            raise SmokeError(f"check failed: {name}")

    def log_paths(self, name: str) -> tuple[Path, Path]:
        self.command_index += 1
        # Full command labels stay in the report. Bound UTF-8 filename bytes;
        # the monotonic index distinguishes truncated or repeated labels.
        label = slug(name).encode("utf-8")[:120].decode("utf-8", errors="ignore")
        base = f"{self.command_index:03d}-{label}"
        self.logs.mkdir(parents=True, exist_ok=True)
        return self.logs / f"{base}.stdout.log", self.logs / f"{base}.stderr.log"

    def run_cmd(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        *,
        check: bool = True,
        extra_env: dict[str, str] | None = None,
        input_data: bytes | None = None,
        timeout: int | None = None,
        on_started: Callable[[subprocess.Popen[bytes]], None] | None = None,
    ) -> dict[str, Any]:
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        started = time.monotonic()
        usage_before = self.child_usage()
        temp_before = self.temp_disk_bytes()
        exit_code, stdout_bytes, stderr_bytes, temp_peak, rss_peak = self.run_process(
            args,
            cwd,
            env,
            input_data,
            timeout or self.args.timeout,
            on_started,
        )
        stdout = stdout_bytes.decode("utf-8", errors="replace")
        stderr = stderr_bytes.decode("utf-8", errors="replace")
        raw_stdout = stdout
        stdout = redact_text(stdout, self.credentials())
        stderr = redact_text(stderr, self.credentials())
        stdout = self.redact_sensitive(stdout)
        stderr = self.redact_sensitive(stderr)
        stdout_path, stderr_path = self.log_paths(name)
        self.command_outputs[str(stdout_path)] = raw_stdout
        stdout_path.write_text(stdout, encoding="utf-8", errors="replace")
        stderr_path.write_text(stderr, encoding="utf-8", errors="replace")
        usage_after = self.child_usage()
        temp_after = self.temp_disk_bytes()
        record = {
            "name": name,
            "args": redact_args(args),
            "cwd": str(cwd),
            "exit_code": exit_code,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "resources": self.resource_delta(
                usage_before,
                usage_after,
                temp_before,
                temp_after,
                temp_peak,
                rss_peak,
            ),
            "stdout_log": str(stdout_path),
            "stderr_log": str(stderr_path),
        }
        self.report["commands"].append(record)
        self.write_report()
        if check and exit_code != 0:
            raise SmokeError(f"{name} failed with exit {exit_code}: {stderr_path}")
        return record

    def run_binary(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        output: Path,
        *,
        check: bool = True,
        extra_env: dict[str, str] | None = None,
        input_data: bytes | None = None,
    ) -> dict[str, Any]:
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        started = time.monotonic()
        usage_before = self.child_usage()
        temp_before = self.temp_disk_bytes()
        exit_code, stdout, stderr_bytes, temp_peak, rss_peak = self.run_process(
            args,
            cwd,
            env,
            input_data,
            self.args.timeout,
        )
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(stdout)
        stderr = redact_text(stderr_bytes.decode("utf-8", errors="replace"), self.credentials())
        stderr = self.redact_sensitive(stderr)
        _stdout_path, stderr_path = self.log_paths(name)
        stderr_path.write_text(stderr, encoding="utf-8", errors="replace")
        usage_after = self.child_usage()
        temp_after = self.temp_disk_bytes()
        record = {
            "name": name,
            "args": redact_args(args),
            "cwd": str(cwd),
            "exit_code": exit_code,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "resources": self.resource_delta(
                usage_before,
                usage_after,
                temp_before,
                temp_after,
                temp_peak,
                rss_peak,
            ),
            "stdout_log": str(output),
            "stderr_log": str(stderr_path),
        }
        self.report["commands"].append(record)
        self.write_report()
        if check and exit_code != 0:
            raise SmokeError(f"{name} failed with exit {exit_code}: {stderr_path}")
        return record

    def run_git(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str | None = None,
        check: bool = True,
        extra_env: dict[str, str] | None = None,
        input_data: bytes | None = None,
    ) -> dict[str, Any]:
        return self.run_cmd(
            name or "git " + " ".join(args),
            [str(self.git_bin), *args],
            repo,
            check=check,
            extra_env=extra_env,
            input_data=input_data,
        )

    def run_aws(self, args: list[str], *, name: str, check: bool = True) -> dict[str, Any]:
        command = ["aws", "s3api", *args]
        if self.args.endpoint_url:
            command.extend(["--endpoint-url", self.args.endpoint_url])
        return self.run_cmd(
            name,
            command,
            self.run_root,
            check=check,
        )

    def run_aws_s3(self, args: list[str], *, name: str, check: bool = True) -> dict[str, Any]:
        command = ["aws", "s3", *args]
        if self.args.endpoint_url:
            command.extend(["--endpoint-url", self.args.endpoint_url])
        return self.run_cmd(name, command, self.run_root, check=check)

    def stdout(self, record: dict[str, Any]) -> str:
        return Path(record["stdout_log"]).read_text(encoding="utf-8", errors="replace")

    def git_value(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str,
        input_data: bytes | None = None,
    ) -> str:
        record = self.run_git(repo, args, name=name, input_data=input_data)
        return self.command_outputs.get(record["stdout_log"], self.stdout(record)).strip()

    def ensure_bucket(self) -> None:
        head = self.run_aws(
            ["head-bucket", "--bucket", self.args.bucket],
            name="object-store head bucket",
            check=False,
        )
        if head["exit_code"] != 0:
            if self.args.require_existing_bucket:
                raise SmokeError(
                    f"object-store bucket is unavailable: {self.args.bucket}"
                )
            self.run_aws_s3(
                ["mb", f"s3://{self.args.bucket}"],
                name="object-store create bucket",
            )
        self.run_aws(
            ["head-bucket", "--bucket", self.args.bucket],
            name="object-store verify bucket",
        )

    def store_snapshot(self, stage: str) -> dict[str, int]:
        repository_prefix = f"{REMOTE_PREFIX}/{self.run_id}/"
        record = self.run_aws(
            [
                "list-objects-v2",
                "--bucket",
                self.args.bucket,
                "--prefix",
                repository_prefix,
                "--output",
                "json",
            ],
            name=f"object-store list objects {stage}",
        )
        payload = json.loads(self.stdout(record) or "{}")
        items = payload.get("Contents", [])
        canonical_items = []
        generated_cache_items = []
        coordination_items = []
        for item in items:
            key = str(item.get("Key", ""))
            if self.is_generated_pack_cache_key(key, repository_prefix):
                target = generated_cache_items
            elif self.is_coordination_key(key, repository_prefix):
                target = coordination_items
            else:
                target = canonical_items
            target.append(item)
        snapshot = {
            "stage": stage,
            "objects": len(items),
            "bytes": sum(int(item.get("Size", 0)) for item in items),
            "canonical_objects": len(canonical_items),
            "canonical_bytes": sum(int(item.get("Size", 0)) for item in canonical_items),
            "generated_cache_objects": len(generated_cache_items),
            "generated_cache_bytes": sum(
                int(item.get("Size", 0)) for item in generated_cache_items
            ),
            "coordination_objects": len(coordination_items),
            "coordination_bytes": sum(int(item.get("Size", 0)) for item in coordination_items),
        }
        self.report["store_snapshots"].append(snapshot)
        self.write_report()
        return snapshot

    @staticmethod
    def is_generated_pack_cache_key(key: str, repository_prefix: str) -> bool:
        if not key.startswith(repository_prefix):
            return False
        relative = key[len(repository_prefix) :]
        return relative.startswith("generated-packs/") or relative.startswith(
            "locks/internal/generated-pack-"
        )

    @staticmethod
    def is_coordination_key(key: str, repository_prefix: str) -> bool:
        if not key.startswith(repository_prefix):
            return False
        return key[len(repository_prefix) :].startswith("locks/")

    def storage_telemetry(self) -> dict[str, int]:
        requests = 0
        bytes_read = 0
        by_kind: dict[str, int] = {}
        cache_hits = 0
        cache_misses = 0
        for path in self.logs.glob("*.stderr.log"):
            text = path.read_text(encoding="utf-8", errors="replace")
            for line in text.splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                fields = event.get("fields")
                if not isinstance(fields, dict):
                    continue
                if "storage_request" in fields:
                    kind = str(fields["storage_request"])
                    if kind != "range_get_coalesced":
                        requests += 1
                    bytes_read += int(fields.get("storage_bytes", 0))
                    by_kind[kind] = by_kind.get(kind, 0) + 1
                cache_event = str(fields.get("cache_event", "")).casefold()
                if cache_event == "hit":
                    cache_hits += 1
                elif cache_event == "miss":
                    cache_misses += 1
        return {
            "requests": requests,
            "bytes": bytes_read,
            "cache_hits": cache_hits,
            "cache_misses": cache_misses,
            **by_kind,
        }

    def protocol_telemetry(self) -> list[dict[str, Any]]:
        events: list[dict[str, Any]] = []
        for path in sorted(self.logs.glob("*.stderr.log")):
            for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                fields = event.get("fields")
                if not isinstance(fields, dict) or fields.get("protocol_version") != 2:
                    continue
                if "planned_objects" not in fields and "transferred_bytes" not in fields:
                    continue
                selected = {
                    key: fields[key]
                    for key in (
                        "message",
                        "request_class",
                        "canonical_filter",
                        "negotiation_rounds",
                        "wants",
                        "haves",
                        "common_haves",
                        "planned_objects",
                        "omitted_objects",
                        "reconstructed_objects",
                        "transferred_bytes",
                        "latency_ms",
                        "lazy_fetch_latency_ms",
                    )
                    if key in fields
                }
                selected["log"] = str(path)
                events.append(selected)
        return events

    def record_telemetry_delta(self, stage: str, before: dict[str, int]) -> dict[str, int]:
        after = self.storage_telemetry()
        delta = {
            "stage": stage,
            "requests": after["requests"] - before["requests"],
            "bytes": after["bytes"] - before["bytes"],
            "range_get": after.get("range_get", 0) - before.get("range_get", 0),
            "range_get_coalesced": after.get("range_get_coalesced", 0)
            - before.get("range_get_coalesced", 0),
            "locator_lookup": after.get("locator_lookup", 0)
            - before.get("locator_lookup", 0),
            "cache_hits": after.get("cache_hits", 0) - before.get("cache_hits", 0),
            "cache_misses": after.get("cache_misses", 0) - before.get("cache_misses", 0),
        }
        self.report["telemetry"][stage] = delta
        self.write_report()
        return after

    def trace_env(self, path: Path) -> dict[str, str]:
        return {
            "GIT_TRACE": "1",
            "GIT_TRACE_PACKET": "1",
            "GIT_TRACE2_EVENT": str(path),
        }

    def redact_trace(self, path: Path, artifact_name: str) -> None:
        if not path.exists():
            return
        text = redact_text(path.read_text(encoding="utf-8", errors="replace"), self.credentials())
        text = self.redact_sensitive(text)
        text = text.replace(self.remote_url, "<remote>")
        path.write_text(text, encoding="utf-8")
        artifact = self.artifacts / artifact_name
        artifact.write_text(text, encoding="utf-8")
        self.report["artifacts"][artifact_name] = str(artifact)
        self.write_report()

    def endpoint_health(self) -> dict[str, Any]:
        if self.args.backend == "s3":
            return {"ready": True, "service": "external-s3", "version": "external"}
        request = urllib.request.Request(f"{self.args.endpoint_url.rstrip('/')}/health/ready")
        with urllib.request.urlopen(request, timeout=self.args.timeout) as response:
            return json.loads(response.read().decode("utf-8"))

    def setup_source(self) -> tuple[str, str, str, str, str, str, str]:
        self.source.mkdir()
        self.run_git(self.run_root, ["init", "-b", "main", str(self.source)], name="git init source")
        self.run_git(self.source, ["config", "user.name", "Crab protocol smoke"])
        self.run_git(self.source, ["config", "user.email", "smoke@example.invalid"])
        self.run_cmd(
            "initialize Crab source",
            [str(self.crab_bin), "init", self.remote_url],
            self.source,
        )
        self.run_cmd(
            "track Crab pointer fixture",
            [str(self.crab_bin), "track", "*.ptr"],
            self.source,
        )
        (self.source / "normal.bin").write_bytes(deterministic_bytes(128 * 1024, self.run_id))
        (self.source / "small.txt").write_text("small ordinary blob\n", encoding="utf-8")
        (self.source / "nested").mkdir()
        (self.source / "nested" / "third.txt").write_text("another ordinary blob\n", encoding="utf-8")
        (self.source / "sparse-spec.txt").write_text("nested/\n", encoding="utf-8")
        self.run_git(self.source, ["add", "."])
        self.run_git(self.source, ["commit", "-m", "initial ordinary Git content"])
        self.run_git(self.source, ["tag", "-a", "v1", "-m", "v1"])
        (self.source / "history.txt").write_text("second generation\n", encoding="utf-8")
        self.run_git(self.source, ["add", "history.txt"])
        self.run_git(self.source, ["commit", "-m", "second ordinary Git commit"])
        self.run_git(self.source, ["tag", "-a", "v2", "-m", "v2"])
        self.run_git(self.source, ["switch", "-c", "fixture/deep"], name="create deep fixture")
        for index in range(24):
            (self.source / "deep-history.txt").write_text(
                f"deep generation {index}\n", encoding="utf-8"
            )
            self.run_git(self.source, ["add", "deep-history.txt"])
            self.run_git(self.source, ["commit", "-m", f"deep history {index}"])
        self.run_git(
            self.source,
            ["switch", "main"],
            name="restore source main after deep fixture",
        )

        self.run_git(
            self.source,
            ["switch", "-c", "fixture/many-small"],
            name="create many-small fixture",
        )
        many_small = self.source / "many-small"
        many_small.mkdir()
        for index in range(256):
            (many_small / f"file-{index:04d}.txt").write_text(
                f"small fixture file {index}\n", encoding="utf-8"
            )
        self.run_git(self.source, ["add", "many-small"])
        self.run_git(self.source, ["commit", "-m", "many small files"])
        self.run_git(
            self.source,
            ["switch", "main"],
            name="restore source main after many-small fixture",
        )

        self.run_git(
            self.source,
            ["switch", "-c", "fixture/pointers"],
            name="create pointer fixture",
        )
        pointer_dir = self.source / "pointer-heavy"
        pointer_dir.mkdir()
        for index in range(256):
            (pointer_dir / f"file-{index:04d}.ptr").write_bytes(
                deterministic_bytes(16 * 1024, f"pointer:{self.run_id}:{index}")
            )
        self.run_cmd(
            "stage Crab pointer fixture",
            [str(self.crab_bin), "add", "*.ptr", "--json"],
            self.source,
        )
        self.crab_pointer_bytes = self.stdout(
            self.run_git(
                self.source,
                ["show", ":pointer-heavy/file-0000.ptr"],
                name="read staged Crab pointer",
            )
        ).encode()
        self.run_git(self.source, ["commit", "-m", "pointer-heavy fixture"])
        self.run_git(
            self.source,
            ["switch", "main"],
            name="restore source main after pointer fixture",
        )

        self.run_git(
            self.source,
            ["switch", "-c", "fixture/lfs-pointers"],
            name="create LFS pointer fixture",
        )
        lfs_content = deterministic_bytes(8 * 1024 * 1024, f"{self.run_id}:lfs-object")
        lfs_oid_hex = hashlib.sha256(lfs_content).hexdigest()
        lfs_pointer = (
            "version https://git-lfs.github.com/spec/v1\n"
            f"oid sha256:{lfs_oid_hex}\n"
            f"size {len(lfs_content)}\n"
        ).encode()
        lfs_path = self.source / "lfs-pointer.bin"
        self.run_binary(
            "clean LFS fixture",
            [str(self.crab_bin), "lfs", "clean", lfs_path.name],
            self.source,
            lfs_path,
            input_data=lfs_content,
        )
        lfs_pointer = lfs_path.read_bytes()
        self.lfs_pointer_bytes = lfs_pointer
        lfs_object = self.source / ".git" / "lfs" / "objects" / lfs_oid_hex[:2] / lfs_oid_hex[2:4] / lfs_oid_hex
        lfs_object.parent.mkdir(parents=True, exist_ok=True)
        lfs_object.write_bytes(lfs_content)
        self.run_git(self.source, ["add", lfs_path.name])
        self.run_git(self.source, ["commit", "-m", "LFS pointer fixture"])
        lfs_oid = self.git_value(
            self.source,
            ["rev-parse", "HEAD:lfs-pointer.bin"],
            name="LFS pointer blob oid",
        )
        self.run_git(
            self.source,
            ["switch", "main"],
            name="restore source main after LFS pointer fixture",
        )
        commit = self.git_value(self.source, ["rev-parse", "HEAD"], name="source revision")
        large_oid = self.git_value(self.source, ["rev-parse", "HEAD:normal.bin"], name="large blob oid")
        small_oid = self.git_value(self.source, ["rev-parse", "HEAD:small.txt"], name="small blob oid")
        batch_first_oid = self.git_value(
            self.source, ["rev-parse", "HEAD:history.txt"], name="first batched lazy blob oid"
        )
        batch_second_oid = self.git_value(
            self.source,
            ["rev-parse", "HEAD:nested/third.txt"],
            name="second batched lazy blob oid",
        )
        sparse_oid = self.git_value(
            self.source,
            ["rev-parse", "HEAD:sparse-spec.txt"],
            name="sparse filter specification oid",
        )
        self.add_trace_redactions(
            commit, large_oid, small_oid, batch_first_oid, batch_second_oid, sparse_oid, lfs_oid
        )
        object_list = subprocess.run(
            [str(self.git_bin), "-C", str(self.source), "rev-list", "--objects", "--all"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        ).stdout
        self.add_trace_redactions(*(line.split(maxsplit=1)[0] for line in object_list.splitlines()))
        self.run_git(
            self.source,
            [
                "push",
                self.remote_url,
                "HEAD:refs/heads/main",
                "refs/tags/v1",
                "refs/tags/v2",
                "fixture/deep:refs/heads/fixture/deep",
                "fixture/many-small:refs/heads/fixture/many-small",
                "fixture/pointers:refs/heads/fixture/pointers",
                "fixture/lfs-pointers:refs/heads/fixture/lfs-pointers",
            ],
            name="push source history and tags",
        )
        return commit, large_oid, small_oid, batch_first_oid, batch_second_oid, sparse_oid, lfs_oid

    def filter_matrix(self, large_oid: str, small_oid: str, sparse_oid: str) -> None:
        """Qualify every client-supported rev-list filter through real Git."""
        filter_root = self.run_root / "filter-matrix"
        filter_root.mkdir()
        commit_oid = self.git_value(self.source, ["rev-parse", "HEAD"], name="matrix commit oid")
        tree_oid = self.git_value(self.source, ["rev-parse", "HEAD^{tree}"], name="matrix tree oid")
        tag_oid = self.git_value(
            self.source, ["rev-parse", "refs/tags/v1"], name="matrix tag oid"
        )
        filters = [
            ("blob-none", "blob:none", "blob:none", False, None),
            ("blob-limit", "blob:limit=1k", "blob:limit=1024", False, None),
            ("tree-depth", "tree:1", "tree:1", False, None),
            ("sparse", f"sparse:oid={sparse_oid}", "sparse:oid=<oid>", False, None),
            ("combine", "combine:blob:none+tree:1", "combine:blob:none+tree:1", False, None),
        ]
        object_type_supported, object_type_probe_error = self.git_supports_filter(
            "object:type=tag"
        )
        if object_type_supported:
            filters[3:3] = [
                ("object-type-tag", "object:type=tag", "object:type=tag", False, tag_oid),
                (
                    "object-type-commit",
                    "object:type=commit",
                    "object:type=commit",
                    False,
                    commit_oid,
                ),
                ("object-type-tree", "object:type=tree", "object:type=tree", False, tree_oid),
                ("object-type-blob", "object:type=blob", "object:type=blob", True, small_oid),
            ]
        before = self.store_snapshot("before-filter-matrix")
        nested_oid = self.git_value(
            self.source,
            ["rev-parse", "HEAD:nested/third.txt"],
            name="sparse selected blob oid",
        )
        rows: dict[str, Any] = {}
        for label, filter_spec, expected_canonical, expect_large, object_probe in filters:
            if filter_spec.startswith("object:type="):
                support = self.run_git(
                    self.source,
                    ["rev-list", "--objects", "--all", f"--filter={filter_spec}"],
                    name=f"probe {label} filter support",
                    check=False,
                )
                if support["exit_code"] != 0:
                    support_stderr = Path(support["stderr_log"]).read_text(
                        encoding="utf-8", errors="replace"
                    )
                    if "invalid filter-spec" not in support_stderr:
                        raise SmokeError(
                            f"{label} filter capability probe failed: {support['stderr_log']}"
                        )
                    rows[label] = {
                        "requested_filter": filter_spec,
                        "skipped": True,
                        "reason": "Git client rejected this filter syntax",
                        "probe_exit_code": support["exit_code"],
                        "probe_stderr_log": support["stderr_log"],
                    }
                    self.check(
                        f"filter-matrix-{label}-client-support",
                        True,
                        rows[label],
                    )
                    continue
            clone = filter_root / label
            clone_record = self.run_git(
                self.run_root,
                [
                    "-c",
                    "protocol.version=2",
                    "clone",
                    f"--filter={filter_spec}",
                    "--no-checkout",
                    self.remote_url,
                    str(clone),
                ],
                name=f"filter matrix {label} clone",
            )
            self.configure_reachable_oid_admission(clone, True)
            stderr = Path(clone_record["stderr_log"]).read_text(
                encoding="utf-8", errors="replace"
            )
            plan_events: list[dict[str, Any]] = []
            for line in stderr.splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                fields = event.get("fields")
                if (
                    isinstance(fields, dict)
                    and fields.get("message") == "protocol-v2 upload-pack plan selected"
                ):
                    plan_events.append(fields)
            promisor = self.git_config(clone, "remote.origin.promisor", f"{label} promisor config")
            configured_filter = self.git_config(
                clone,
                "remote.origin.partialclonefilter",
                f"{label} partial filter config",
            )
            fsck = self.run_git(clone, ["fsck", "--strict"], name=f"filter matrix {label} fsck", check=False)
            plan = next(
                (
                    event
                    for event in plan_events
                    if event.get("protocol_version") == 2
                    and event.get("canonical_filter") == expected_canonical
                ),
                None,
            )
            config_ok = promisor == "true" and configured_filter in {
                filter_spec,
                expected_canonical,
            }
            protocol_ok = plan is not None
            row: dict[str, Any] = {
                "requested_filter": filter_spec,
                "configured_filter": configured_filter,
                "promisor": promisor,
                "protocol_v2": protocol_ok,
                "canonical_filter": plan.get("canonical_filter") if plan else None,
                "planned_objects": plan.get("planned_objects") if plan else None,
                "omitted_objects": plan.get("omitted_objects") if plan else None,
                "fsck_exit_code": fsck["exit_code"],
                "odb_bytes": self.odb_bytes(clone),
            }
            rows[label] = row
            self.check(
                f"filter-matrix-{label}-protocol-and-promisor",
                clone_record["exit_code"] == 0
                and config_ok
                and protocol_ok
                and fsck["exit_code"] == 0,
                row,
            )

            large_present = self.object_present_in_odb(clone, large_oid)
            if expect_large:
                self.check(
                    f"filter-matrix-{label}-retains-large-before-lazy-fetch",
                    large_present,
                    {"present": large_present},
                )
            else:
                self.check(
                    f"filter-matrix-{label}-omits-large-before-lazy-fetch",
                    not large_present,
                    {"present": large_present},
                )

                lazy_output = self.artifacts / f"filter-matrix-{label}-lazy.bin"
                lazy = self.run_binary(
                    f"filter matrix {label} lazy fetch",
                    [str(self.git_bin), "cat-file", "blob", large_oid],
                    clone,
                    lazy_output,
                )
                self.check(
                    f"filter-matrix-{label}-lazy-byte-identity",
                    lazy["exit_code"] == 0
                    and lazy_output.read_bytes() == (self.source / "normal.bin").read_bytes(),
                    {"exit_code": lazy["exit_code"], "sha256": sha256_file(lazy_output)},
                )

            if object_probe is not None:
                probe_present = self.object_present_in_odb(clone, object_probe)
                self.check(
                    f"filter-matrix-{label}-object-selection",
                    probe_present,
                    {"oid": object_probe, "present": probe_present},
                )
            repack = self.run_git(
                clone,
                ["repack", "-ad"],
                name=f"filter matrix {label} repack",
                check=False,
            )
            maintenance = self.run_git(
                clone,
                ["maintenance", "run", "--auto"],
                name=f"filter matrix {label} maintenance",
                check=False,
            )
            post_maintenance_fsck = self.run_git(
                clone,
                ["fsck", "--strict"],
                name=f"filter matrix {label} post-maintenance fsck",
                check=False,
            )
            self.check(
                f"filter-matrix-{label}-maintenance",
                repack["exit_code"] == 0
                and maintenance["exit_code"] == 0
                and post_maintenance_fsck["exit_code"] == 0,
                {
                    "repack_exit_code": repack["exit_code"],
                    "maintenance_exit_code": maintenance["exit_code"],
                    "fsck_exit_code": post_maintenance_fsck["exit_code"],
                },
            )

            if label == "blob-limit":
                small_present = self.object_present_in_odb(clone, small_oid)
                self.check(
                    "filter-matrix-blob-limit-selects-by-size",
                    not large_present and small_present,
                    {
                        "large_present": large_present,
                        "small_present": small_present,
                    },
                )

            if label == "sparse":
                nested_present = self.object_present_in_odb(clone, nested_oid)
                self.check(
                    "filter-matrix-sparse-path-selection",
                    not large_present and nested_present,
                    {
                        "excluded_root_blob_present": large_present,
                        "selected_nested_blob_present": nested_present,
                    },
                )

        after = self.store_snapshot("after-filter-matrix")
        self.report["performance"]["filter-matrix"] = {
            "filters": rows,
            "client_capabilities": {
                "object_type_filter": object_type_supported,
                "object_type_probe_error": object_type_probe_error or None,
            },
            "remote_unchanged": after["canonical_objects"]
            == before["canonical_objects"]
            and after["canonical_bytes"] == before["canonical_bytes"],
            "before": before,
            "after": after,
        }
        self.check(
            "filter-matrix-read-only-remote",
            self.report["performance"]["filter-matrix"]["remote_unchanged"],
            {"before": before, "after": after},
        )

    def pointer_fixture_checks(self, lfs_oid: str) -> None:
        """Qualify Crab and Git LFS pointer blobs as distinct Git content."""
        if self.crab_pointer_bytes is None or self.lfs_pointer_bytes is None:
            raise SmokeError("pointer fixture bytes were not initialized")

        crab_clone = self.performance_root / "pointer-heavy-full"
        crab_oid = self.git_value(
            self.source,
            ["rev-parse", "fixture/pointers:pointer-heavy/file-0000.ptr"],
            name="Crab pointer blob oid",
        )
        crab_output = self.artifacts / "crab-pointer-fixture.bin"
        crab_record = self.run_binary(
            "read Crab pointer fixture",
            [str(self.git_bin), "cat-file", "blob", crab_oid],
            crab_clone,
            crab_output,
        )
        lfs_clone = self.run_git(
            self.run_root,
            [
                "-c",
                "protocol.version=2",
                "clone",
                "--filter=blob:limit=1k",
                "--no-checkout",
                "--single-branch",
                "--branch",
                "fixture/lfs-pointers",
                self.remote_url,
                str(self.run_root / "lfs-pointer-filtered"),
            ],
            name="LFS pointer filtered clone",
        )
        lfs_present = self.object_present_in_odb(self.run_root / "lfs-pointer-filtered", lfs_oid)
        lfs_output = self.artifacts / "lfs-pointer-fixture.bin"
        lfs_record = self.run_binary(
            "read LFS pointer fixture",
            [str(self.git_bin), "cat-file", "blob", lfs_oid],
            self.run_root / "lfs-pointer-filtered",
            lfs_output,
        )
        self.report["performance"]["pointer-fixtures"] = {
            "crab_pointer": {"oid": crab_oid, "bytes": len(self.crab_pointer_bytes)},
            "lfs_pointer": {"oid": lfs_oid, "bytes": len(self.lfs_pointer_bytes)},
        }
        self.write_report()
        self.check(
            "pointer-fixtures-remain-byte-identified",
            crab_record["exit_code"] == 0
            and crab_output.read_bytes() == self.crab_pointer_bytes
            and lfs_clone["exit_code"] == 0
            and lfs_present
            and lfs_record["exit_code"] == 0
            and lfs_output.read_bytes() == self.lfs_pointer_bytes,
            {
                "crab_exit_code": crab_record["exit_code"],
                "lfs_clone_exit_code": lfs_clone["exit_code"],
                "lfs_present": lfs_present,
                "lfs_read_exit_code": lfs_record["exit_code"],
            },
        )

    def create_security_refs(self) -> tuple[str, str]:
        """Leave one hidden-only and one dangling object in the remote ODB."""
        self.run_git(self.source, ["switch", "-c", "hidden"], name="create hidden ref")
        hidden_file = self.source / "hidden-only.txt"
        hidden_file.write_text("hidden-only object\n", encoding="utf-8")
        self.run_git(self.source, ["add", hidden_file.name])
        self.run_git(self.source, ["commit", "-m", "hidden ref fixture"])
        hidden_oid = self.git_value(
            self.source, ["rev-parse", "HEAD:hidden-only.txt"], name="hidden blob oid"
        )
        self.run_git(
            self.source,
            ["push", self.remote_url, "HEAD:refs/heads/hidden"],
            name="push hidden ref fixture",
        )

        self.run_git(self.source, ["switch", "main"], name="restore source main")
        self.run_git(self.source, ["switch", "-c", "dangling"], name="create dangling ref")
        dangling_file = self.source / "dangling-only.txt"
        dangling_file.write_text("dangling-only object\n", encoding="utf-8")
        self.run_git(self.source, ["add", dangling_file.name])
        self.run_git(self.source, ["commit", "-m", "dangling ref fixture"])
        dangling_oid = self.git_value(
            self.source, ["rev-parse", "HEAD:dangling-only.txt"], name="dangling blob oid"
        )
        self.run_git(
            self.source,
            ["push", self.remote_url, "HEAD:refs/heads/dangling"],
            name="push dangling ref fixture",
        )
        self.run_git(self.source, ["switch", "main"], name="restore source main after dangling ref")
        self.run_git(
            self.source,
            ["push", self.remote_url, ":refs/heads/dangling"],
            name="delete dangling ref fixture",
        )
        self.run_git(self.source, ["branch", "-D", "dangling"], name="remove local dangling fixture")
        self.run_git(self.source, ["branch", "-D", "hidden"], name="remove local hidden fixture")
        self.add_trace_redactions(hidden_oid, dangling_oid)
        return hidden_oid, dangling_oid

    def clone_full(self, telemetry_before: dict[str, int]) -> dict[str, int]:
        record = self.run_git(
            self.run_root,
            ["-c", "protocol.version=2", "clone", self.remote_url, str(self.full)],
            name="full clone",
            extra_env=self.trace_env(self.artifacts / "full-clone.trace2.json"),
        )
        fsck = self.run_git(self.full, ["fsck", "--strict"], name="full clone fsck")
        self.check("complete-clone-strict-fsck", record["exit_code"] == 0 and fsck["exit_code"] == 0)
        self.redact_trace(self.artifacts / "full-clone.trace2.json", "full-clone.trace2.redacted.json")
        after = self.record_telemetry_delta("full_clone", telemetry_before)
        self.report["performance"]["normal-v2-full"] = {
            "odb_bytes": self.odb_bytes(self.full),
            "telemetry": self.report["telemetry"].get("full_clone", {}),
            "resources": record.get("resources", {}),
        }
        self.write_report()
        return after

    def clone_legacy(self, telemetry_before: dict[str, int]) -> None:
        record = self.run_git(
            self.run_root,
            ["-c", "protocol.version=0", "clone", self.remote_url, str(self.legacy)],
            name="legacy complete-pack clone",
            extra_env=self.trace_env(self.artifacts / "legacy-clone.trace2.json"),
        )
        self.run_git(self.legacy, ["fsck", "--strict"], name="legacy clone fsck")
        self.redact_trace(
            self.artifacts / "legacy-clone.trace2.json",
            "legacy-clone.trace2.redacted.json",
        )
        legacy_trace = self.artifacts / "legacy-clone.trace2.redacted.json"
        trace_text = (
            legacy_trace.read_text(encoding="utf-8", errors="replace")
            if legacy_trace.exists()
            else ""
        )
        self.check(
            "legacy-protocol-v0-path",
            "version 2" not in trace_text and "stateless-connect" not in trace_text,
            {"trace_artifact": str(legacy_trace)},
        )
        telemetry = self.record_telemetry_delta("legacy_clone", telemetry_before)
        self.report["performance"]["normal-legacy-full"] = {
            "odb_bytes": self.odb_bytes(self.legacy),
            "telemetry": self.report["telemetry"].get("legacy_clone", {}),
            "resources": record.get("resources", {}),
        }
        self.write_report()
        self.check(
            "legacy-complete-pack-path",
            record["exit_code"] == 0,
            {"telemetry": self.report["telemetry"].get("legacy_clone", {}), "after": telemetry},
        )

    def performance_fixtures(self) -> None:
        """Measure v2/filter behavior on distinct repository-shape fixtures."""
        self.performance_root.mkdir(parents=True, exist_ok=True)
        for label, branch in self.fixture_branches:
            full = self.performance_root / f"{slug(label)}-full"
            filtered = self.performance_root / f"{slug(label)}-filtered"
            before = self.storage_telemetry()
            full_record = self.run_git(
                self.run_root,
                [
                    "-c",
                    "protocol.version=2",
                    "clone",
                    "--no-checkout",
                    "--single-branch",
                    "--branch",
                    branch,
                    self.remote_url,
                    str(full),
                ],
                name=f"performance {label} complete clone",
            )
            full_telemetry = self.record_telemetry_delta(f"performance_{label}_full", before)
            filtered_before = self.storage_telemetry()
            filtered_record = self.run_git(
                self.run_root,
                [
                    "-c",
                    "protocol.version=2",
                    "clone",
                    "--filter=blob:none",
                    "--no-checkout",
                    "--single-branch",
                    "--branch",
                    branch,
                    self.remote_url,
                    str(filtered),
                ],
                name=f"performance {label} filtered clone",
            )
            filtered_telemetry = self.record_telemetry_delta(
                f"performance_{label}_filtered", filtered_before
            )
            full_odb = self.odb_bytes(full)
            filtered_odb = self.odb_bytes(filtered)
            self.report["performance"][label] = {
                "branch": branch,
                "full": {
                    "odb_bytes": full_odb,
                    "telemetry": self.report["telemetry"].get(f"performance_{label}_full", {}),
                    "resources": full_record.get("resources", {}),
                },
                "filtered": {
                    "odb_bytes": filtered_odb,
                    "telemetry": self.report["telemetry"].get(
                        f"performance_{label}_filtered", {}
                    ),
                    "resources": filtered_record.get("resources", {}),
                },
            }
            self.write_report()
            self.check(
                f"performance-{slug(label)}-filtered-odb-smaller",
                filtered_odb < full_odb,
                {
                    "full_odb_bytes": full_odb,
                    "filtered_odb_bytes": filtered_odb,
                    "full_telemetry": full_telemetry,
                    "filtered_telemetry": filtered_telemetry,
                },
            )

        fixture_tips = {
            label: self.git_value(
                self.performance_root / f"{slug(label)}-full",
                ["rev-parse", "HEAD"],
                name=f"performance {label} tip",
            )
            for label, _branch in self.fixture_branches
        }
        self.check(
            "performance-fixtures-have-distinct-tips",
            len(set(fixture_tips.values())) == len(fixture_tips),
            {"tips": fixture_tips},
        )
        resource_rows = [
            self.report["performance"][label][kind]["resources"]
            for label, _branch in self.fixture_branches
            for kind in ("full", "filtered")
        ]
        resource_metrics_ok = len(resource_rows) == len(self.fixture_branches) * 2 and all(
            isinstance(row.get("children_max_rss"), int)
            and row.get("children_max_rss", -1) >= 0
            and row.get("children_max_rss_unit") == "bytes"
            and row.get("children_max_rss_scope") == "sampled_process_tree"
            and isinstance(row.get("temp_disk_peak_bytes"), int)
            and row.get("temp_disk_peak_bytes", -1) >= 0
            for row in resource_rows
        )
        self.check(
            "performance-resource-metrics",
            resource_metrics_ok
            and any(row["children_max_rss"] > 0 for row in resource_rows),
            {"resources": resource_rows},
        )

    def disconnect_check(self) -> None:
        """Close each terminal handoff boundary and verify cleanup."""
        requests = {
            "before-pkt-line": (
                b"capabilities\n\n"
                b"stateless-connect git-upload-pack\n"
            ),
            "during-ls-refs": (
                b"capabilities\n\n"
                b"stateless-connect git-upload-pack\n"
                b"0014command=ls-refs\n0001"
            ),
            "during-fetch": (
                b"capabilities\n\n"
                b"stateless-connect git-upload-pack\n"
                b"0012command=fetch\n0001"
            ),
        }
        results = []
        for boundary, input_data in requests.items():
            record = self.run_cmd(
                f"protocol disconnect {boundary}",
                [str(self.bin_dir / "git-remote-crab"), "origin", self.remote_url],
                self.run_root,
                check=False,
                input_data=input_data,
            )
            leftovers = [str(path) for path in self.temp_root.rglob("*") if path.is_file()]
            results.append(
                {
                    "boundary": boundary,
                    "exit_code": record["exit_code"],
                    "temporary_files": leftovers,
                }
            )
        self.check(
            "protocol-disconnect-cleans-session-state",
            all(
                not result["temporary_files"]
                and (
                    result["boundary"] == "before-pkt-line"
                    or result["exit_code"] != 0
                )
                for result in results
            ),
            {"boundaries": results},
        )

    def clone_shallow(self) -> None:
        self.run_git(
            self.run_root,
            [
                "-c",
                "protocol.version=2",
                "clone",
                "--depth",
                "1",
                self.remote_url,
                str(self.shallow),
            ],
            name="shallow clone",
            extra_env=self.trace_env(self.artifacts / "shallow-clone.trace2.json"),
        )
        count = int(self.git_value(self.shallow, ["rev-list", "--count", "HEAD"], name="shallow count"))
        shallow = self.git_value(self.shallow, ["rev-parse", "--is-shallow-repository"], name="shallow state")
        self.check("shallow-clone-boundary", count == 1 and shallow == "true", {"count": count, "is_shallow": shallow})
        self.run_git(
            self.shallow,
            ["-c", "protocol.version=2", "fetch", "--deepen=1", "origin"],
            name="deepen shallow clone",
        )
        deepened = int(self.git_value(self.shallow, ["rev-list", "--count", "HEAD"], name="deepened count"))
        self.check("shallow-clone-deepen", deepened >= 2, {"count": deepened})
        self.run_git(
            self.shallow,
            ["-c", "protocol.version=2", "fetch", "--unshallow", "origin"],
            name="unshallow clone",
        )
        unshallowed = self.git_value(
            self.shallow, ["rev-parse", "--is-shallow-repository"], name="unshallowed state"
        )
        self.check("shallow-clone-unshallow", unshallowed == "false", {"is_shallow": unshallowed})
        self.run_git(self.shallow, ["fsck", "--strict"], name="shallow lifecycle fsck")
        self.redact_trace(self.artifacts / "shallow-clone.trace2.json", "shallow-clone.trace2.redacted.json")

    def parse_batch_output(self, data: bytes, oids: list[str]) -> list[bytes] | None:
        contents: list[bytes] = []
        offset = 0
        for oid in oids:
            line_end = data.find(b"\n", offset)
            if line_end < 0:
                return None
            fields = data[offset:line_end].split()
            if len(fields) != 3 or fields[0].decode("ascii", errors="replace") != oid:
                return None
            try:
                size = int(fields[2])
            except ValueError:
                return None
            offset = line_end + 1
            payload_end = offset + size
            if payload_end >= len(data) or data[payload_end : payload_end + 1] != b"\n":
                return None
            contents.append(data[offset:payload_end])
            offset = payload_end + 1
        return contents if offset == len(data) else None

    def clone_filtered(
        self,
        large_oid: str,
        small_oid: str,
        batch_oids: tuple[str, str],
        telemetry_before: dict[str, int],
    ) -> dict[str, Any]:
        trace_path = self.artifacts / "filtered-clone.trace2.json"
        clone_record = self.run_git(
            self.run_root,
            [
                "-c",
                "protocol.version=2",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                self.remote_url,
                str(self.filtered),
            ],
            name="filtered blobless clone",
            extra_env=self.trace_env(trace_path),
        )
        self.redact_trace(trace_path, "filtered-clone.trace2.redacted.json")
        trace_text = "\n".join(
            [
                (self.artifacts / "filtered-clone.trace2.redacted.json").read_text(
                    encoding="utf-8", errors="replace"
                )
                if (self.artifacts / "filtered-clone.trace2.redacted.json").exists()
                else "",
                Path(clone_record["stderr_log"]).read_text(encoding="utf-8", errors="replace"),
            ]
        )
        self.check(
            "protocol-v2-packet-trace",
            "version 2" in trace_text and "command=fetch" in trace_text,
            {"trace_artifact": str(self.artifacts / "filtered-clone.trace2.redacted.json")},
        )

        promisor = self.git_config(self.filtered, "remote.origin.promisor", "promisor config")
        filter_value = self.git_config(self.filtered, "remote.origin.partialclonefilter", "partial filter config")
        # Current Git records the remote-scoped promisor keys and sidecars but
        # may omit the older extensions.partialClone key. Retain the observed
        # value in the report without making that optional key a gate.
        extension = self.git_config(self.filtered, "extensions.partialClone", "partial clone extension config")
        config_detail = {
            "remote.origin.promisor": promisor,
            "remote.origin.partialclonefilter": filter_value,
            "extensions.partialClone": extension,
        }
        self.check(
            "promisor-configuration",
            promisor == "true" and filter_value == "blob:none",
            config_detail,
        )
        initial_sidecars = self.promisor_sidecars(self.filtered)
        self.check("initial-promisor-sidecar", bool(initial_sidecars), {"count": len(initial_sidecars)})

        # Lazy fetches from older Git clients use the remote-helper batch path
        # instead of terminal protocol v2. Opt this fixture into reachable
        # object admission before any raw-object probe so both paths exercise
        # the same repository policy without changing the production default.
        # Upload-pack admission is an internal repository policy. Keep it in
        # `.crab/local.toml`, which is the file loaded by the remote helper;
        # `crab.toml` only carries project metadata such as the remote URL.
        self.configure_reachable_oid_admission(self.filtered, True)
        tags = self.run_git(
            self.filtered,
            ["tag", "--list", "v1", "v2"],
            name="verify filtered tags",
        )
        tag_lines = set(self.stdout(tags).splitlines())
        self.check(
            "filtered-annotated-tags",
            {"v1", "v2"}.issubset(tag_lines),
            {"tags": sorted(tag_lines)},
        )

        large_present = self.object_present_in_odb(self.filtered, large_oid)
        small_present = self.object_present_in_odb(self.filtered, small_oid)
        self.check(
            "initial-ordinary-blobs-absent",
            not large_present and not small_present,
            {"large_present": large_present, "small_present": small_present},
        )

        self.concurrent_lazy_fetch_check(large_oid)

        batch_output = self.artifacts / "batched-lazy-fetch.bin"
        batch_record = self.run_binary(
            "batched lazy fetch",
            [str(self.git_bin), "cat-file", "--batch"],
            self.filtered,
            batch_output,
            input_data=("\n".join(batch_oids) + "\n").encode("ascii"),
        )
        batch_contents = self.parse_batch_output(batch_output.read_bytes(), list(batch_oids))
        expected_batch_contents = [
            (self.source / "history.txt").read_bytes(),
            (self.source / "nested" / "third.txt").read_bytes(),
        ]
        self.check(
            "batched-lazy-oid-fetch",
            batch_record["exit_code"] == 0 and batch_contents == expected_batch_contents,
            {
                "exit_code": batch_record["exit_code"],
                "requested_objects": len(batch_oids),
                "byte_identity": batch_contents == expected_batch_contents,
            },
        )
        initial_odb_bytes = self.odb_bytes(self.filtered)
        self.record_telemetry_delta("filtered_clone", telemetry_before)

        offline_pack_count = len(self.pack_files(self.filtered))
        offline_present = self.run_git(
            self.filtered,
            ["cat-file", "-e", "HEAD^{tree}"],
            name="offline present tree access",
            check=False,
            extra_env={
                "AWS_ENDPOINT_URL": "http://127.0.0.1:1",
                "AWS_ENDPOINT_URL_S3": "http://127.0.0.1:1",
            },
        )
        self.check(
            "offline-present-object-access",
            offline_present["exit_code"] == 0,
            {"exit_code": offline_present["exit_code"]},
        )
        offline = self.run_git(
            self.filtered,
            ["cat-file", "blob", small_oid],
            name="offline promised-object failure",
            check=False,
            extra_env={
                "AWS_ENDPOINT_URL": "http://127.0.0.1:1",
                "AWS_ENDPOINT_URL_S3": "http://127.0.0.1:1",
            },
        )
        self.check(
            "offline-promised-object-error",
            offline["exit_code"] != 0 and len(self.pack_files(self.filtered)) == offline_pack_count,
            {"exit_code": offline["exit_code"], "pack_count": offline_pack_count},
        )

        self.rollback_compatibility_check(large_oid)
        self.filtered_incremental_fetch()

        partial_file = self.filtered / "partial-clone-push.txt"
        partial_file.write_text("pushed from incomplete ODB\n", encoding="utf-8")
        new_blob = self.git_value(
            self.filtered,
            ["hash-object", "-w", "--stdin"],
            name="write incomplete-ODB push blob",
            input_data=partial_file.read_bytes(),
        )
        tree = self.git_value(self.filtered, ["ls-tree", "HEAD"], name="read push base tree")
        tree += f"100644 blob {new_blob}\tpartial-clone-push.txt\n"
        new_tree = self.git_value(
            self.filtered,
            ["mktree", "--missing"],
            name="write incomplete-ODB push tree",
            input_data=tree.encode("utf-8"),
        )
        new_commit = self.git_value(
            self.filtered,
            ["commit-tree", new_tree, "-p", "HEAD", "-m", "push from incomplete promisor repository"],
            name="write incomplete-ODB push commit",
        )
        push_baseline = self.storage_telemetry()
        self.run_git(
            self.filtered,
            ["push", "origin", f"{new_commit}:refs/heads/partial-clone-push"],
            name="push from incomplete ODB",
        )
        telemetry_after_push = self.record_telemetry_delta("incomplete_odb_push", push_baseline)
        admission_baseline = self.storage_telemetry()
        admission_fetch = self.run_git(
            self.filtered,
            [
                "-c",
                "protocol.version=2",
                "fetch",
                "origin",
                "refs/heads/partial-clone-push:refs/remotes/origin/partial-clone-push",
            ],
            name="settle post-push read admission",
        )
        admission_telemetry = self.record_telemetry_delta(
            "post_push_read_admission", admission_baseline
        )
        self.check(
            "post-push-read-admission",
            admission_fetch["exit_code"] == 0,
            {
                "fetch_exit": admission_fetch["exit_code"],
                "telemetry": self.report["telemetry"]["post_push_read_admission"],
            },
        )
        source_blob = self.source / "normal.bin"
        lazy_output = self.artifacts / "lazy-normal.bin"
        lazy_trace = self.artifacts / "lazy-fetch.trace2.json"
        lazy = self.run_binary(
            "lazy fetch ordinary blob",
            [str(self.git_bin), "cat-file", "blob", large_oid],
            self.filtered,
            lazy_output,
            extra_env=self.trace_env(lazy_trace),
        )
        self.redact_trace(lazy_trace, "lazy-fetch.trace2.redacted.json")
        self.check(
            "lazy-blob-byte-identity",
            lazy["exit_code"] == 0 and lazy_output.read_bytes() == source_blob.read_bytes(),
            {"source_sha256": sha256_file(source_blob), "lazy_sha256": sha256_file(lazy_output)},
        )
        lazy_sidecars = self.promisor_sidecars(self.filtered)
        self.check(
            "lazy-promisor-pack-installed",
            len(lazy_sidecars) >= len(initial_sidecars),
            {"before": len(initial_sidecars), "after": len(lazy_sidecars)},
        )
        checkout = self.run_git(
            self.filtered,
            ["checkout", "--detach", "HEAD"],
            name="filtered checkout after lazy fetch",
            check=False,
        )
        diff = self.run_git(
            self.filtered,
            ["diff", "--stat", "HEAD^", "HEAD"],
            name="filtered diff after lazy fetch",
            check=False,
        )
        log = self.run_git(
            self.filtered,
            ["log", "--format=%H", "-2"],
            name="filtered log after lazy fetch",
            check=False,
        )
        merge = self.run_git(
            self.filtered,
            ["merge", "--ff-only", "HEAD"],
            name="filtered merge after lazy fetch",
            check=False,
        )
        self.check(
            "filtered-git-lifecycle",
            all(record["exit_code"] == 0 for record in (checkout, diff, log, merge)),
            {
                "checkout_exit": checkout["exit_code"],
                "diff_exit": diff["exit_code"],
                "log_exit": log["exit_code"],
                "merge_exit": merge["exit_code"],
            },
        )
        self.run_git(self.filtered, ["fsck", "--strict"], name="filtered clone fsck after lazy fetch")
        self.run_git(self.filtered, ["gc"], name="filtered clone gc")
        self.run_git(self.filtered, ["repack", "-ad"], name="filtered clone repack")
        self.run_git(self.filtered, ["fsck", "--strict"], name="filtered clone fsck after repack")

        lazy_baseline = admission_telemetry
        telemetry_after = self.record_telemetry_delta("lazy_fetch_and_maintenance", lazy_baseline)
        lazy_delta = self.report["telemetry"]["lazy_fetch_and_maintenance"]
        initial_filtered = self.report["telemetry"].get("filtered_clone", {})
        filtered_total = {
            "stage": "filtered_clone_and_lazy_fetch",
            "requests": int(initial_filtered.get("requests", 0)) + int(lazy_delta.get("requests", 0)),
            "bytes": int(initial_filtered.get("bytes", 0)) + int(lazy_delta.get("bytes", 0)),
            "range_get": int(initial_filtered.get("range_get", 0)) + int(lazy_delta.get("range_get", 0)),
            "range_get_coalesced": int(initial_filtered.get("range_get_coalesced", 0))
            + int(lazy_delta.get("range_get_coalesced", 0)),
            "locator_lookup": int(initial_filtered.get("locator_lookup", 0))
            + int(lazy_delta.get("locator_lookup", 0)),
            "cache_hits": int(initial_filtered.get("cache_hits", 0))
            + int(lazy_delta.get("cache_hits", 0)),
            "cache_misses": int(initial_filtered.get("cache_misses", 0))
            + int(lazy_delta.get("cache_misses", 0)),
        }
        self.report["telemetry"]["filtered_clone_and_lazy_fetch"] = filtered_total
        self.write_report()
        return {
            "initial_odb_bytes": initial_odb_bytes,
            "initial_promisor_sidecars": len(initial_sidecars),
            "lazy_promisor_sidecars": len(lazy_sidecars),
            "telemetry_after": telemetry_after,
            "telemetry_push": self.report["telemetry"]["incomplete_odb_push"],
        }

    def concurrent_lazy_fetch_check(self, oid: str) -> None:
        """Prove simultaneous lazy requests preserve bytes and repository state."""
        expected = (self.source / "normal.bin").read_bytes()
        before = self.storage_telemetry()

        def fetch_once() -> tuple[int | None, bytes, str]:
            try:
                result = subprocess.run(
                    [str(self.git_bin), "cat-file", "blob", oid],
                    cwd=self.filtered,
                    env=self.env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                    timeout=self.args.timeout,
                )
            except subprocess.TimeoutExpired:
                return None, b"", "command timed out"
            stderr = redact_text(result.stderr.decode("utf-8", errors="replace"), self.credentials())
            return result.returncode, result.stdout, self.redact_sensitive(stderr)

        with ThreadPoolExecutor(max_workers=4) as workers:
            results = list(workers.map(lambda _index: fetch_once(), range(4)))

        telemetry = self.record_telemetry_delta("concurrent_lazy_fetch", before)
        fsck = self.run_git(
            self.filtered,
            ["fsck", "--strict"],
            name="concurrent lazy fetch fsck",
            check=False,
        )
        exit_codes = [result[0] for result in results]
        byte_lengths = [len(result[1]) for result in results]
        stderr_details = [result[2] for result in results]
        self.report["performance"]["concurrent-lazy-fetch"] = {
            "requests": len(results),
            "exit_codes": exit_codes,
            "byte_lengths": byte_lengths,
            "stderr": stderr_details,
            "telemetry": telemetry,
            "fsck_exit_code": fsck["exit_code"],
        }
        self.write_report()
        self.check(
            "concurrent-lazy-fetch-is-byte-identical",
            all(code == 0 and body == expected for code, body, _stderr in results)
            and fsck["exit_code"] == 0,
            {
                "exit_codes": exit_codes,
                "byte_lengths": byte_lengths,
                "stderr": stderr_details,
                "fsck_exit_code": fsck["exit_code"],
                "telemetry": telemetry,
            },
        )

    def filtered_incremental_fetch(self) -> None:
        """Fetch a new filtered generation without hydrating its ordinary blob."""
        self.run_git(self.source, ["switch", "main"], name="restore source main for incremental fetch")
        incremental = self.source / "incremental-filtered.txt"
        incremental.write_text("incremental filtered content\n", encoding="utf-8")
        self.run_git(self.source, ["add", incremental.name])
        self.run_git(self.source, ["commit", "-m", "incremental filtered fetch fixture"])
        new_commit = self.git_value(self.source, ["rev-parse", "HEAD"], name="incremental source commit")
        new_blob = self.git_value(
            self.source, ["rev-parse", "HEAD:incremental-filtered.txt"], name="incremental blob oid"
        )
        self.add_trace_redactions(new_commit, new_blob)
        self.run_git(
            self.source,
            ["push", self.remote_url, "HEAD:refs/heads/main"],
            name="push incremental filtered fixture",
        )
        before = self.storage_telemetry()
        fetch = self.run_git(
            self.filtered,
            [
                "-c",
                "protocol.version=2",
                "fetch",
                "origin",
                "refs/heads/main:refs/remotes/origin/main",
            ],
            name="filtered incremental fetch",
        )
        telemetry = self.record_telemetry_delta("filtered_incremental_fetch", before)
        fetched_commit = self.git_value(
            self.filtered,
            ["rev-parse", "refs/remotes/origin/main"],
            name="verify incremental fetched commit",
        )
        new_blob_present = self.object_present_in_odb(self.filtered, new_blob)
        self.check(
            "filtered-incremental-fetch",
            fetch["exit_code"] == 0
            and fetched_commit == new_commit
            and not new_blob_present,
            {
                "fetch_exit": fetch["exit_code"],
                "fetched_commit": fetched_commit,
                "expected_commit": new_commit,
                "blob_absent": not new_blob_present,
                "telemetry": telemetry,
            },
        )

    def rollback_compatibility_check(self, large_oid: str) -> None:
        """Prove an older binary serves or refuses a promised raw OID safely."""
        if self.rollback_crab_bin is None:
            return
        output = self.artifacts / "rollback-lazy-normal.bin"
        packs_before = {path.name for path in self.pack_files(self.filtered)}
        sidecars_before = {path.name for path in self.promisor_sidecars(self.filtered)}
        telemetry_before = self.storage_telemetry()
        try:
            self.install_helper_alias(self.rollback_crab_bin)
            result = self.run_binary(
                "rollback prior-binary lazy fetch",
                [str(self.git_bin), "cat-file", "blob", large_oid],
                self.filtered,
                output,
                check=False,
            )
        finally:
            self.install_helper_alias()
        source = self.source / "normal.bin"
        output_bytes = output.read_bytes()
        packs_after = {path.name for path in self.pack_files(self.filtered)}
        sidecars_after = {path.name for path in self.promisor_sidecars(self.filtered)}
        self.record_telemetry_delta("rollback_prior_binary", telemetry_before)
        telemetry = self.report["telemetry"]["rollback_prior_binary"]
        byte_identity = result["exit_code"] == 0 and output_bytes == source.read_bytes()
        no_remote_pack_ranges = telemetry["range_get"] == 0 and telemetry["range_get_coalesced"] == 0
        refused_before_install = (
            result["exit_code"] != 0
            and not output_bytes
            and packs_before == packs_after
            and sidecars_before == sidecars_after
            and no_remote_pack_ranges
        )
        self.check(
            "rollback-prior-binary-raw-oid-or-refusal",
            byte_identity or refused_before_install,
            {
                "exit_code": result["exit_code"],
                "mode": "service" if byte_identity else "refuse-before-pack-install",
                "byte_identity": byte_identity,
                "pack_install_unchanged": packs_before == packs_after,
                "promisor_sidecars_unchanged": sidecars_before == sidecars_after,
                "remote_pack_ranges_unchanged": no_remote_pack_ranges,
                "telemetry": telemetry,
                "rollback_binary": str(self.rollback_crab_bin),
            },
        )

    def security_checks(self, hidden_oid: str, dangling_oid: str) -> None:
        """Prove failed raw-OID admission does not reach pack bytes."""
        self.configure_reachable_oid_admission(self.filtered, False)
        self.configure_hidden_refs(self.filtered)
        for name, oid in (
            ("hidden-only-oid", hidden_oid),
            ("dangling-oid", dangling_oid),
            ("unknown-oid", "f" * 40),
        ):
            before = self.storage_telemetry()
            result = self.run_git(
                self.filtered,
                ["cat-file", "blob", oid],
                name=f"reject {name}",
                check=False,
            )
            after = self.record_telemetry_delta(f"security_{name}", before)
            delta = self.report["telemetry"][f"security_{name}"]
            self.check(
                f"{name}-rejected-before-range-read",
                result["exit_code"] != 0
                and int(delta.get("range_get", 0)) == 0
                and int(delta.get("range_get_coalesced", 0)) == 0,
                {
                    "exit_code": result["exit_code"],
                    "range_get": delta.get("range_get", 0),
                    "range_get_coalesced": delta.get("range_get_coalesced", 0),
                    "telemetry_requests": after.get("requests", 0),
                },
            )

    def json_data(self, record: dict[str, Any], schema: str) -> dict[str, Any]:
        raw = self.command_outputs.get(record["stdout_log"], self.stdout(record))
        payload = json.loads(raw)
        if payload.get("schema") != schema or not isinstance(payload.get("data"), dict):
            raise SmokeError(f"{record['name']} did not emit {schema} success JSON")
        return payload["data"]

    def native_ref_lifecycle_checks(self) -> None:
        """Prove empty discovery, exact ref names, and atomic lease semantics."""
        source = self.run_root / "ref-source"
        clone = self.run_root / "empty-clone"
        remote = self.remote_url + "-refs"
        self.run_git(self.run_root, ["init", "-b", "main", str(source)])
        self.run_cmd("initialize empty ref remote", [str(self.crab_bin), "init", remote], source)
        empty_refs = self.run_git(source, ["ls-remote", remote], name="list empty remote")
        self.run_git(
            self.run_root,
            ["-c", "init.defaultBranch=main", "clone", remote, str(clone)],
            name="clone empty remote",
        )
        empty_head = self.git_value(clone, ["symbolic-ref", "HEAD"], name="empty clone HEAD")
        self.check(
            "empty-clone-and-unborn-discovery",
            not self.stdout(empty_refs).strip()
            and empty_head == "refs/heads/main"
            and not self.pack_files(clone),
            {"head": empty_head},
        )
        (source / "content.txt").write_text("first\n", encoding="utf-8")
        self.run_git(source, ["add", "content.txt"])
        self.run_git(source, ["commit", "-m", "initial ref lifecycle"])
        first = self.git_value(source, ["rev-parse", "HEAD"], name="initial ref lifecycle OID")
        # A full hexadecimal branch name must remain a ref, never an OID hint.
        hex_ref = "refs/heads/" + "a" * 40
        self.run_git(source, ["update-ref", hex_ref, first])
        self.run_git(
            source,
            ["push", "--atomic", remote, "refs/heads/main:refs/heads/main", f"{hex_ref}:{hex_ref}"],
            name="atomic creation with SHA-looking branch",
        )
        self.run_git(clone, ["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"])
        hex_tip = self.git_value(
            clone, ["rev-parse", "refs/remotes/origin/" + "a" * 40], name="SHA-looking remote ref OID"
        )
        fsck = self.run_git(clone, ["fsck", "--strict"], name="empty clone strict fsck after first fetch")
        self.check("sha-looking-ref-fetch", hex_tip == first)
        self.check("ref-lifecycle-strict-fsck", fsck["exit_code"] == 0)

        (source / "content.txt").write_text("second\n", encoding="utf-8")
        self.run_git(source, ["add", "content.txt"])
        self.run_git(source, ["commit", "-m", "advance ref lifecycle"])
        second = self.git_value(source, ["rev-parse", "HEAD"], name="advanced ref lifecycle OID")
        self.run_git(source, ["update-ref", hex_ref, second])
        self.run_git(
            source,
            ["push", "--atomic", remote, "refs/heads/main:refs/heads/main", f"{hex_ref}:{hex_ref}"],
            name="atomic update of both refs",
        )

        def remote_refs() -> dict[str, str]:
            record = self.run_git(source, ["ls-remote", "--refs", remote])
            return {name: oid for oid, name in (line.split() for line in self.stdout(record).splitlines())}

        advanced = {"refs/heads/main": second, hex_ref: second}
        self.check("atomic-multi-ref-update", remote_refs() == advanced)
        rejected = self.run_git(
            source,
            [
                "push", "--atomic", "--force",
                f"--force-with-lease={hex_ref}:{first}",
                remote, f":{hex_ref}", f"{first}:refs/heads/main",
            ],
            name="stale deletion lease rejects whole atomic push",
            check=False,
        )
        self.check(
            "atomic-stale-lease-retains-all-refs",
            rejected["exit_code"] != 0 and remote_refs() == advanced,
        )
        self.run_git(
            source,
            [
                "push", "--atomic", f"--force-with-lease=refs/heads/main:{second}",
                f"--force-with-lease={hex_ref}:{second}",
                remote, f"{first}:refs/heads/main", f":{hex_ref}",
            ],
            name="approved leased force update and deletion",
        )
        self.check("leased-force-and-branch-deletion", remote_refs() == {"refs/heads/main": first})

    def mirror_cache_cleanup_checks(
        self, source: Path, remote: str, cache: Path, plan: Path
    ) -> None:
        """Clean only this run's explicit cache, never the user's default roots."""
        control = self.run_root / "cache-clean-control"
        self.run_git(self.run_root, ["init", str(control)])
        (control / ".crab").mkdir()
        (control / ".crab/local.toml").write_text(
            "[cache]\nchunk_cache_dir = " + json.dumps(str(cache / "chunks")) + "\n",
            encoding="utf-8",
        )
        clean_env = {"CRAB_CACHE_DIR": str(cache)}
        mirror_args = [str(self.crab_bin), "mirror", str(source), remote,
                       "--cache-dir", str(cache), "--json"]
        refs_before = self.git_value(cache, ["show-ref"], name="cache refs before cleanup")
        remote_before = self.git_value(
            control, ["ls-remote", "--refs", remote], name="remote refs before cache cleanup"
        )
        with hold_cache_lock(cache.with_name(cache.name + ".crab-cache-use.lock")):
            busy = self.run_cmd(
                "cache clean refuses active mirror owner",
                [str(self.crab_bin), "cache", "clean"], control,
                extra_env=clean_env, check=False,
            )
            alias_busy = self.run_cmd(
                "optimize cache clean refuses active mirror owner",
                [str(self.crab_bin), "optimize", "cache", "clean"],
                control, extra_env=clean_env, check=False,
            )
            self.check(
                "mirror-owner-blocks-cache-clean-before-deletion",
                busy["exit_code"] != 0 and alias_busy["exit_code"] != 0
                and self.git_value(cache, ["show-ref"], name="refs after busy cleanup") == refs_before,
            )

        with hold_cache_lock(cache.with_name(cache.name + ".crab-cache-clean.lock")):
            blocked_check = self.run_cmd(
                "mirror check refuses active cache cleanup", mirror_args + ["--check", "--ci"],
                control, check=False,
            )
            blocked_apply = self.run_cmd(
                "mirror apply refuses active cache cleanup",
                mirror_args + ["--apply-plan", str(plan)], control, check=False,
            )
            self.check(
                "cache-clean-blocks-mirror-check-and-apply",
                blocked_check["exit_code"] != 0 and blocked_apply["exit_code"] != 0
                and self.json_data(blocked_check, "mirror.check").get("state") == "unverifiable"
                and self.git_value(cache, ["show-ref"], name="refs during cleanup lock") == refs_before,
            )

        self.run_cmd(
            "clean idle isolated mirror cache",
            [str(self.crab_bin), "cache", "clean"], control, extra_env=clean_env,
        )
        self.check(
            "idle-cache-clean-removes-only-isolated-cache-data",
            cache.is_dir() and not any(cache.iterdir())
            and self.git_value(
                control, ["ls-remote", "--refs", remote], name="remote refs after cache cleanup"
            ) == remote_before,
        )
        rebuilt = self.run_cmd(
            "mirror reclones cache emptied by cleanup", mirror_args + ["--check"], control,
        )
        rebuilt_data = self.json_data(rebuilt, "mirror.check")
        self.run_git(cache, ["fsck", "--strict", "--full"], name="strict fsck rebuilt mirror cache")
        self.check(
            "mirror-reclones-cleaned-cache-with-exact-refs-and-origin-proof",
            rebuilt_data.get("state") == "source_ahead"
            and rebuilt_data.get("pointers", {}).get("state") == "verified"
            and self.git_value(cache, ["show-ref"], name="rebuilt mirror refs") == refs_before,
        )

        # A nested cleanup leaves its lock inode inside the parent Git cache.
        # Rebuilding must preserve that inode even after all Git data is gone.
        self.run_cmd(
            "clean isolated nested Git object cache",
            [str(self.crab_bin), "cache", "clean"], control,
            extra_env={"CRAB_CACHE_DIR": str(cache / "objects")},
        )
        marker = cache / "objects.crab-cache-clean.lock"
        with marker.open("rb") as opened_before_cleanup:
            self.run_cmd(
                "clean parent mirror cache retaining nested marker",
                [str(self.crab_bin), "cache", "clean"], control, extra_env=clean_env,
            )
            self.check(
                "mirror-cleanup-leaves-a-marker-only-skeleton",
                marker.is_file() and not (cache / "HEAD").exists(),
            )
            control_config = sha256_file(control / ".git/config")
            restored = self.run_cmd(
                "rebuild marker-only cache with ambient Git paths",
                mirror_args + ["--check"], control,
                extra_env={
                    "GIT_COMMON_DIR": str(control / ".git"),
                    "GIT_OBJECT_DIRECTORY": str(control / ".git/objects"),
                    "GIT_CONFIG": str(control / ".git/config"),
                },
            )
            restored_data = self.json_data(restored, "mirror.check")
            self.run_git(cache, ["fsck", "--strict", "--full"], name="strict fsck marker-only rebuilt cache")
            self.check(
                "mirror-rebuilds-marker-only-cache-with-stable-lock-inodes",
                os.path.samestat(os.fstat(opened_before_cleanup.fileno()), marker.stat())
                and restored_data.get("state") == "source_ahead"
                and restored_data.get("pointers", {}).get("state") == "verified"
                and self.git_value(cache, ["show-ref"], name="marker-only rebuilt refs") == refs_before,
            )
            self.check(
                "mirror-cache-ignores-ambient-git-repository-paths",
                sha256_file(control / ".git/config") == control_config
                and not (cache / "objects/info/alternates").exists()
                and self.git_value(
                    control, ["ls-remote", "--refs", remote], name="remote refs after marker-only rebuild"
                ) == remote_before,
            )

    def mirror_cancellation_checks(self, source: Path, destination: str) -> None:
        # CLI signal injection below is POSIX-only. Windows token/job contracts
        # run natively in CI; do not claim a Windows CLI signal qualification.
        if os.name == "nt":
            return
        cache = self.run_root / "mirror-cancellation-cache.git"
        control = self.run_root / "mirror-cancellation-control"
        self.run_git(self.run_root, ["init", str(control)])
        (control / ".crab").mkdir()
        (control / ".crab/local.toml").write_text(
            "[cache]\nchunk_cache_dir = " + json.dumps(str(cache / "chunks")) + "\n",
            encoding="utf-8",
        )
        ready = self.run_root / "mirror-pack-hook-ready"
        stopped = self.run_root / "mirror-pack-hook-stopped"
        hook = self.run_root / "mirror-pack-hook.py"
        hook.write_text(
            "import os, signal, time\nfrom pathlib import Path\n"
            f"ready = Path({str(ready)!r})\nstopped = Path({str(stopped)!r})\n"
            "def stop(_signal, _frame):\n"
            "    stopped.write_text('stopped')\n    raise SystemExit(0)\n"
            "signal.signal(signal.SIGTERM, stop)\n"
            "ready.write_text(str(os.getpid()))\n"
            "deadline = time.monotonic() + 60\n"
            "while time.monotonic() < deadline: time.sleep(0.05)\n"
            "raise SystemExit(1)\n",
            encoding="utf-8",
        )
        config_root = self.run_root / "mirror-pack-hook-config"
        (config_root / "git").mkdir(parents=True)
        self.run_git(
            self.run_root,
            ["config", "--file", str(config_root / "git/config"), "uploadpack.packObjectsHook",
             f"{shlex.quote(sys.executable)} {shlex.quote(str(hook))}"],
            name="configure isolated upload-pack hook",
        )
        before = self.git_value(
            self.run_root, ["ls-remote", "--refs", destination],
            name="refs before cancelled mirror fetch",
        )

        def cancel_when_fetch_started(process: subprocess.Popen[bytes]) -> None:
            deadline = time.monotonic() + 30
            while not ready.exists():
                if process.poll() is not None:
                    # Return through run_cmd so the failed fixture's complete
                    # stdout/stderr survive in the retained report.
                    return
                if time.monotonic() >= deadline:
                    raise SmokeError("mirror did not reach the real upload-pack hook")
                time.sleep(0.05)
            cleanup = self.run_cmd(
                "cache cleanup during active mirror fetch",
                [str(self.crab_bin), "cache", "clean"], control,
                check=False, extra_env={"CRAB_CACHE_DIR": str(cache)},
            )
            self.check(
                "mirror-cancel-retains-cache-owner-while-fetch-runs",
                cleanup["exit_code"] != 0 and (cache / "HEAD").is_file()
                and "in use" in Path(cleanup["stderr_log"]).read_text(encoding="utf-8"),
            )
            process.send_signal(signal.SIGTERM)

        cancelled = self.run_cmd(
            "cancel mirror during real source pack generation",
            [str(self.crab_bin), "mirror", source.as_uri(), destination,
             "--cache-dir", str(cache), "--check", "--json"],
            self.run_root, check=False, timeout=60, on_started=cancel_when_fetch_started,
            extra_env={
                # Local transport clears command-scope Git parameters. XDG
                # global config reaches upload-pack on Git 2.30.9 and later,
                # without editing the user's real global configuration.
                "XDG_CONFIG_HOME": str(config_root),
            },
        )
        self.check(
            "mirror-cancel-stops-real-pack-child-before-return",
            cancelled["exit_code"] != 0 and stopped.exists()
            and "cancel" in self.command_outputs[cancelled["stdout_log"]].lower(),
        )
        self.run_cmd(
            "clean mirror cache after cancelled fetch",
            [str(self.crab_bin), "cache", "clean"], control,
            extra_env={"CRAB_CACHE_DIR": str(cache)},
        )
        self.check(
            "mirror-cancel-releases-cache-without-publishing-refs",
            not (cache / "HEAD").exists()
            and self.git_value(self.run_root, ["ls-remote", "--refs", destination],
                name="refs after cancelled mirror fetch") == before,
        )

    def mirror_lfs_discovery_check(self, source: Path, destination: str) -> None:
        """Refuse a corrupt Git LFS pointer before any publication, then recover."""
        oid = self.git_value(source, ["rev-parse", "HEAD:hook-lfs.bin"], name="capture LFS pointer Git OID")
        head = self.git_value(source, ["rev-parse", "HEAD"], name="capture LFS discovery source ref")
        before = self.git_value(source, ["ls-remote", "--refs", destination], name="capture refs before LFS discovery fault")
        path = source / ".git/objects" / oid[:2] / oid[2:]
        original = path.read_bytes()
        working_bytes = (source / "hook-lfs.bin").read_bytes()
        large_file = self.artifacts / "lfs-discovery-large-blob.bin"
        large_file.write_bytes(b"x" * 65536)
        large_oid = self.git_value(source, ["hash-object", "-w", str(large_file)], name="create LFS discovery large-header fault")
        replacement = (source / ".git/objects" / large_oid[:2] / large_oid[2:]).read_bytes()
        command = [str(self.crab_bin), "lfs", "push", "--dry-run", destination]
        # Only this disposable fixture's directory entry changes. Restore it
        # even if the assertion fails; no source worktree data is rewritten.
        path.unlink()
        try:
            path.write_bytes(replacement)
            rejected = self.run_cmd("LFS discovery rejects oversized corrupt pointer", command, source, check=False)
            self.check(
                "mirror-lfs-discovery-rejects-corrupt-git-pointer",
                rejected["exit_code"] != 0
                and f"checksum differs from {oid}" in Path(rejected["stderr_log"]).read_text(encoding="utf-8")
                and self.git_value(source, ["rev-parse", "HEAD"], name="verify source ref after LFS discovery refusal") == head
                and self.git_value(source, ["ls-remote", "--refs", destination], name="verify remote refs after LFS discovery refusal") == before,
            )
        finally:
            if path.exists():
                path.unlink()
            path.write_bytes(original)
        self.run_cmd("LFS discovery accepts restored Git pointer", command, source)
        self.check(
            "mirror-lfs-discovery-restoration-preserves-refs",
            path.read_bytes() == original and (source / "hook-lfs.bin").read_bytes() == working_bytes
            and self.git_value(source, ["ls-remote", "--refs", destination], name="verify refs after LFS discovery restoration") == before,
        )

    def mirror_pre_push_batch_checks(self) -> None:
        """Publish real hook batches while HEAD differs from the selected refs."""
        source = self.run_root / "hook-source"
        upstream = self.run_root / "hook-upstream.git"
        destination = self.remote_url + "-hook"
        source.mkdir()
        self.run_git(self.run_root, ["init", "-b", "main", str(source)])
        self.run_git(source, ["config", "core.hooksPath", ".hooks"])
        self.run_git(self.run_root, ["init", "--bare", "-b", "main", str(upstream)])
        self.run_git(source, ["remote", "add", "origin", str(upstream)])
        self.run_cmd(
            "initialize actual collaboration hook fixture",
            [str(self.crab_bin), "init", "--mirror=origin", destination], source,
        )
        (source / "baseline.txt").write_text("hook baseline\n", encoding="utf-8")
        self.run_git(source, ["add", "baseline.txt", "crab.toml"])
        self.run_git(source, ["commit", "-m", "hook baseline"])
        baseline = self.git_value(source, ["rev-parse", "HEAD"], name="hook baseline oid")
        self.run_git(
            source, ["push", "--atomic", "origin", "main:main", "main:retired"],
            name="seed both remotes through installed collaboration hook",
        )
        initial = self.git_value(
            source, ["ls-remote", "--refs", "origin"], name="initial collaboration refs"
        )
        self.check(
            "mirror-hook-publishes-initial-batch-from-custom-hooks-path",
            (source / ".hooks/pre-push").is_file()
            and initial == self.git_value(
                source, ["ls-remote", "--refs", destination], name="initial Crab hook refs"
            ),
        )

        self.run_git(source, ["switch", "-c", "topic"])
        content = deterministic_bytes(1024 * 1024, f"hook-pointer:{self.run_id}")
        (source / "hook-data.bin").write_bytes(content)
        self.run_cmd("track hook pointer", [str(self.crab_bin), "track", "hook-data.bin"], source)
        self.run_cmd("stage hook pointer", [str(self.crab_bin), "add", "hook-data.bin"], source)
        self.run_git(source, ["add", ".gitattributes"])
        self.run_git(source, ["commit", "-m", "hook pointer data"])
        revision = self.git_value(source, ["rev-parse", "HEAD"], name="hook revision-expression oid")

        self.run_cmd("compose LFS with mirror hook", [str(self.crab_bin), "lfs", "install", "--local"], source)
        lfs_content = deterministic_bytes(128 * 1024, f"hook-lfs:{self.run_id}")
        (source / "hook-lfs.bin").write_bytes(lfs_content)
        attributes = source / ".gitattributes"
        attributes.write_text(
            attributes.read_text(encoding="utf-8")
            + "\nhook-lfs.bin filter=lfs diff=lfs merge=lfs -text\n",
            encoding="utf-8",
        )
        self.run_git(source, ["add", ".gitattributes", "hook-lfs.bin"])
        self.run_git(source, ["commit", "-m", "hook LFS data"])
        self.mirror_lfs_discovery_check(source, destination)
        self.run_git(source, ["tag", "-a", "hook-v1", "-m", "hook annotated tag"])
        selected = self.git_value(source, ["rev-parse", "topic"], name="hook selected commit")
        tag_oid = self.git_value(source, ["rev-parse", "hook-v1"], name="hook selected tag object")
        self.run_git(source, ["switch", "--detach", baseline])
        self.run_git(
            source,
            ["push", "--atomic", "origin", "topic~:refs/heads/revision",
             "topic:refs/heads/review", "topic:refs/heads/main",
             "refs/tags/hook-v1:refs/tags/released", ":refs/heads/retired"],
            name="mixed hook batch from detached HEAD with pointer and LFS data",
        )
        expected = {
            "refs/heads/main": selected, "refs/heads/review": selected,
            "refs/heads/revision": revision, "refs/tags/released": tag_oid,
        }
        for remote, label in (("origin", "collaboration"), (destination, "Crab")):
            refs = self.git_value(source, ["ls-remote", "--refs", remote], name=f"mixed hook {label} refs")
            observed = {ref: oid for oid, ref in (line.split() for line in refs.splitlines())}
            self.check(f"mirror-hook-exact-mixed-batch-{label}", observed == expected, {"refs": observed})
        self.check(
            "mirror-hook-preserves-detached-source-HEAD",
            self.git_value(source, ["rev-parse", "HEAD"], name="source HEAD after mixed hook") == baseline,
        )

        clone = self.run_root / "hook-clone"
        self.run_cmd(
            "fresh clone and hydrate hook-published pointer",
            [str(self.crab_bin), "clone", destination, str(clone), "--no-lazy"], self.run_root,
        )
        self.check("mirror-hook-publishes-exact-pointer-bytes", (clone / "hook-data.bin").read_bytes() == content)
        lfs_pointer = self.git_value(clone, ["show", "HEAD:hook-lfs.bin"], name="hook LFS pointer")
        lfs_output = self.artifacts / "hook-lfs-hydrated.bin"
        self.run_binary(
            "hydrate hook-published LFS object from fresh clone",
            [str(self.crab_bin), "lfs", "smudge", "hook-lfs.bin"], clone, lfs_output,
            input_data=(lfs_pointer + "\n").encode(),
        )
        self.check("mirror-hook-publishes-exact-LFS-bytes", lfs_output.read_bytes() == lfs_content)
        self.run_git(clone, ["fsck", "--strict", "--full"], name="strict fsck hook-published clone")

        # Rejection by the second remote is not a distributed rollback. The
        # first remote must retain the selected snapshot, and retry converge.
        reject_hook = upstream / "hooks/pre-receive"
        reject_hook.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        reject_hook.chmod(0o755)
        retry_ref = "refs/heads/retry"
        retry_spec = f"topic:{retry_ref}"
        try:
            rejected = self.run_git(
                source, ["push", "origin", retry_spec], check=False,
                name="collaboration rejects after Crab hook publication",
            )
        finally:
            reject_hook.unlink()
        self.check(
            "mirror-hook-retains-Crab-ahead-after-collaboration-rejection",
            rejected["exit_code"] != 0
            and not self.git_value(source, ["ls-remote", "origin", retry_ref], name="rejected collaboration ref")
            and self.git_value(source, ["ls-remote", destination, retry_ref], name="retained Crab retry ref").split()[0] == selected,
        )
        self.run_git(source, ["push", "origin", retry_spec], name="retry rejected collaboration push")
        self.check(
            "mirror-hook-retry-converges-without-extra-refs",
            self.git_value(source, ["ls-remote", "--refs", "origin"], name="retried collaboration refs")
            == self.git_value(source, ["ls-remote", "--refs", destination], name="retried Crab refs"),
        )

        self.run_git(
            source,
            ["push", "--atomic", "--force", "origin", f"{baseline}:refs/heads/main",
             f"{baseline}:refs/heads/review", ":refs/tags/released"],
            name="explicit rewrite and deletion through collaboration hook",
        )
        expected.update({"refs/heads/main": baseline, "refs/heads/review": baseline, retry_ref: selected})
        expected.pop("refs/tags/released")
        rewritten = self.git_value(source, ["ls-remote", "--refs", destination], name="rewritten Crab refs")
        self.check(
            "mirror-hook-rewrites-and-deletes-only-explicit-refs",
            {ref: oid for oid, ref in (line.split() for line in rewritten.splitlines())} == expected
            and rewritten == self.git_value(source, ["ls-remote", "--refs", "origin"], name="rewritten collaboration refs"),
        )
        self.run_git(clone, ["fetch", "--prune", "--prune-tags", "origin"], name="fetch rewritten and deleted hook refs")
        fsck = self.run_git(clone, ["fsck", "--strict", "--full"], name="strict fsck after hook rewrite and deletion")
        self.check("mirror-hook-rewrites-remain-fetchable", fsck["exit_code"] == 0)

        # Direct Crab pushes bypass the mirror guard, creating deliberate drift.
        self.run_git(
            source, ["push", "--force", destination, f"{revision}:refs/heads/main"],
            name="independently advance Crab for hook conflict",
        )
        crab_before = self.git_value(source, ["ls-remote", "--refs", destination], name="Crab refs before hook conflict")
        source_before = self.git_value(source, ["ls-remote", "--refs", "origin"], name="source refs before hook conflict")
        conflict = self.run_git(
            source, ["push", "--atomic", "origin", "topic:refs/heads/main", "topic:refs/heads/must-not-publish"],
            name="diverged Crab rejects complete collaboration batch", check=False,
        )
        self.check(
            "mirror-hook-conflict-preserves-both-remotes-and-whole-batch",
            conflict["exit_code"] != 0
            and crab_before == self.git_value(source, ["ls-remote", "--refs", destination], name="Crab after hook conflict")
            and source_before == self.git_value(source, ["ls-remote", "--refs", "origin"], name="source after hook conflict"),
        )

    def mirror_metadata_staleness_check(self, source: Path, destination: str, label: str) -> None:
        """A metadata-only CAS must invalidate plans even when Git refs do not move."""
        plan = self.artifacts / f"mirror-{label}-metadata-plan.json"
        before = self.run_cmd(
            f"save {label} mirror plan before metadata change",
            [str(self.crab_bin), "mirror", str(source), destination,
             "--check", "--write-plan", str(plan), "--json"],
            self.run_root,
        )
        before_data = self.json_data(before, "mirror.check")
        plan_bytes = plan.read_bytes()
        plan_data = json.loads(plan_bytes)
        if (
            plan_data.get("blocked")
            or not plan_data.get("destination_snapshot")
            or not plan_data.get("recipe_digest")
            or before_data.get("state") != label.replace("-", "_")
            or bool(plan_data.get("actions")) != (label == "source-ahead")
        ):
            raise SmokeError("metadata staleness fixture requires a fully verified plan")

        # This is the isolated smoke repository, never a user-selected prefix.
        # Preserve Git/data roots and use CAS so the fixture cannot overwrite a
        # concurrent commit. The old Git-only digest deliberately does not move.
        key = f"{REMOTE_PREFIX}/{self.run_id}-mirror/manifest"
        original = self.artifacts / f"mirror-{label}-manifest-before.json"
        current = self.run_aws(
            ["get-object", "--bucket", self.args.bucket, "--key", key, str(original)],
            name=f"capture {label} mirror manifest identity",
        )
        etag = json.loads(self.stdout(current))["ETag"]
        manifest = json.loads(original.read_bytes())
        manifest["session_id"] = f"mirror-metadata-{self.run_id}-{label}"
        changed = self.artifacts / f"mirror-{label}-manifest-changed.json"
        changed.write_text(json.dumps(manifest, sort_keys=True) + "\n", encoding="utf-8")
        self.run_aws(
            ["put-object", "--bucket", self.args.bucket, "--key", key,
             "--if-match", etag, "--body", str(changed)],
            name=f"CAS {label} mirror metadata without changing refs",
        )
        refused = self.run_cmd(
            f"refuse stale {label} mirror metadata plan",
            [str(self.crab_bin), "mirror", str(source), destination,
             "--apply-plan", str(plan), "--json"],
            self.run_root,
            check=False,
        )
        refusal = json.loads(self.stdout(refused))
        after = self.run_cmd(
            f"verify {label} mirror refs and bytes after stale plan refusal",
            [str(self.crab_bin), "mirror", str(source), destination, "--check", "--json"],
            self.run_root,
        )
        after_data = self.json_data(after, "mirror.check")
        confirmed = self.artifacts / f"mirror-{label}-manifest-after.json"
        self.run_aws(
            ["get-object", "--bucket", self.args.bucket, "--key", key, str(confirmed)],
            name=f"verify {label} stale plan preserved canonical metadata",
        )
        pointer_proof = after_data.get("pointers", {})
        self.check(
            f"mirror-{label}-plan-rejects-metadata-only-change",
            refused["exit_code"] != 0
            and refusal.get("error", {}).get("code") == "CRAB-E0060"
            and before_data.get("refs") == after_data.get("refs")
            and before_data.get("destination_snapshot") != after_data.get("destination_snapshot")
            and pointer_proof.get("recipe_digest") == plan_data["recipe_digest"]
            and pointer_proof.get("state") == "verified"
            and pointer_proof.get("verified") == 1
            and plan.read_bytes() == plan_bytes
            and confirmed.read_bytes() == changed.read_bytes(),
            {
                "exit_code": refused["exit_code"],
                "error": refusal.get("error"),
                "actions": len(plan_data["actions"]),
                "before_snapshot": before_data.get("destination_snapshot"),
                "after_snapshot": after_data.get("destination_snapshot"),
                "recipe_digest": pointer_proof.get("recipe_digest"),
            },
        )

    def mirror_layout_identity_check(self, source: Path, destination: str) -> None:
        """Plan identity follows validated layout semantics, never missing/corrupt layout."""
        plan = self.artifacts / "mirror-layout-plan.json"
        checked = self.run_cmd(
            "save mirror plan before layout changes",
            [str(self.crab_bin), "mirror", str(source), destination,
             "--check", "--write-plan", str(plan), "--json"], self.run_root,
        )
        before = self.json_data(checked, "mirror.check")
        plan_bytes = plan.read_bytes()
        if json.loads(plan_bytes).get("blocked") or before.get("state") != "equal":
            raise SmokeError("layout identity fixture requires a verified equal plan")

        # Only the generated smoke repository is modified, always through CAS.
        # Restore its exact descriptor in finally; do not repair through Crab.
        prefix = f"{REMOTE_PREFIX}/{self.run_id}-mirror"
        key = f"{prefix}/layout"
        original = self.artifacts / "mirror-layout-original.json"
        fetched = self.run_aws(
            ["get-object", "--bucket", self.args.bucket, "--key", key, str(original)],
            name="capture mirror layout identity",
        )
        etag = json.loads(self.stdout(fetched))["ETag"]
        manifest_before = self.artifacts / "mirror-layout-manifest-before.json"
        self.run_aws(
            ["get-object", "--bucket", self.args.bucket, "--key", f"{prefix}/manifest", str(manifest_before)],
            name="capture canonical manifest before layout fixture",
        )
        layout = json.loads(original.read_bytes())
        formatted = self.artifacts / "mirror-layout-formatted.json"
        formatted.write_text(json.dumps(layout, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        updated = self.run_aws(
            ["put-object", "--bucket", self.args.bucket, "--key", key,
             "--if-match", etag, "--body", str(formatted)], name="change only layout JSON formatting",
        )
        etag = json.loads(self.stdout(updated))["ETag"]
        try:
            equivalent = self.run_cmd(
                "verify equivalent layout preserves mirror identity",
                [str(self.crab_bin), "mirror", str(source), destination, "--check", "--json"], self.run_root,
            )
            equivalent_data = self.json_data(equivalent, "mirror.check")
            self.check(
                "mirror-layout-formatting-preserves-plan-identity",
                before.get("destination_identity") == equivalent_data.get("destination_identity")
                and before.get("destination_snapshot") == equivalent_data.get("destination_snapshot")
                and equivalent_data.get("pointers", {}).get("state") == "verified"
                and equivalent_data.get("pointers", {}).get("verified") == 1,
            )
            layout["schema_version"] += 1
            unsupported = self.artifacts / "mirror-layout-unsupported.json"
            unsupported.write_text(json.dumps(layout) + "\n", encoding="utf-8")
            updated = self.run_aws(
                ["put-object", "--bucket", self.args.bucket, "--key", key,
                 "--if-match", etag, "--body", str(unsupported)], name="install unsupported fixture layout",
            )
            etag = json.loads(self.stdout(updated))["ETag"]
            blocked_plan = self.artifacts / "mirror-layout-blocked-plan.json"
            blocked = self.run_cmd(
                "invalid layout blocks mirror check and planning",
                [str(self.crab_bin), "mirror", str(source), destination,
                 "--check", "--ci", "--write-plan", str(blocked_plan), "--json"],
                self.run_root, check=False,
            )
            refused = self.run_cmd(
                "invalid layout refuses saved mirror plan replay",
                [str(self.crab_bin), "mirror", str(source), destination,
                 "--apply-plan", str(plan), "--json"], self.run_root, check=False,
            )
            after_layout = self.artifacts / "mirror-layout-after-refusal.json"
            manifest_after = self.artifacts / "mirror-layout-manifest-after.json"
            for object_key, output in [(key, after_layout), (f"{prefix}/manifest", manifest_after)]:
                self.run_aws(
                    ["get-object", "--bucket", self.args.bucket, "--key", object_key, str(output)],
                    name=f"verify canonical {output.stem} after layout refusal",
                )
            blocked_data = self.json_data(blocked, "mirror.check")
            self.check(
                "mirror-invalid-layout-blocks-check-plan-and-replay",
                blocked["exit_code"] != 0 and refused["exit_code"] != 0
                and blocked_data.get("state") == "unverifiable"
                and blocked_data.get("ci_passed") is False
                and json.loads(blocked_plan.read_bytes()).get("blocked") is True
                and after_layout.read_bytes() == unsupported.read_bytes()
                and manifest_after.read_bytes() == manifest_before.read_bytes()
                and plan.read_bytes() == plan_bytes,
            )
        finally:
            self.run_aws(
                ["put-object", "--bucket", self.args.bucket, "--key", key,
                 "--if-match", etag, "--body", str(original)], name="restore isolated fixture layout through CAS",
            )

    def mirror_oversized_header_check(self, source: Path, destination: str) -> None:
        """Corrupt only a disposable cache; a large header must not hide a pointer."""
        cache = self.run_root / "mirror-oversized-header.git"
        self.run_git(self.run_root, ["clone", "--mirror", str(source), str(cache)])
        oid = self.git_value(source, ["rev-parse", "HEAD:mirror-data.bin"], name="capture pointer OID before oversized-header fault")
        object_path = cache / "objects" / oid[:2] / oid[2:]
        original = object_path.read_bytes()
        source_object = source / ".git/objects" / oid[:2] / oid[2:]
        source_original = source_object.read_bytes()
        large_file = self.artifacts / "oversized-header-blob.bin"
        large_file.write_bytes(deterministic_bytes(65536, "oversized-header"))
        large_oid = self.git_value(cache, ["hash-object", "-w", str(large_file)], name="create oversized-header cache fixture")
        replacement = (cache / "objects" / large_oid[:2] / large_oid[2:]).read_bytes()
        before = self.git_value(source, ["ls-remote", "--refs", destination], name="capture remote refs before oversized-header fault")
        plan = self.run_root / "oversized-header-plan.json"
        args = [str(self.crab_bin), "mirror", str(source), destination,
                "--cache-dir", str(cache), "--check", "--ci", "--json"]
        # Local clone can hardlink source objects. Replace only this cache's
        # directory entry, then restore its captured bytes on every exit path.
        object_path.unlink()
        try:
            object_path.write_bytes(replacement)
            result = self.run_cmd(
                "mirror rejects pointer disguised by oversized blob header",
                args + ["--write-plan", str(plan)], self.run_root, check=False,
            )
            data = self.json_data(result, "mirror.check")
            pointers = data.get("pointers", {})
            # Destination inspection may import a healthy packed copy of this
            # OID. Git and gitoxide can then select different copies: exact
            # header binding must reject that before the body checksum stage.
            identity_errors = (f"Git blob batch header differs for {oid}",
                               f"Git blob checksum differs from {oid}")
            self.check(
                "mirror-oversized-header-cannot-hide-corrupt-pointer",
                result["exit_code"] != 0 and data.get("ci_passed") is False
                and pointers.get("state") == "unverifiable"
                and any(issue.get("detail", "").endswith(identity_errors)
                        for issue in pointers.get("issues", []))
                and json.loads(plan.read_text(encoding="utf-8")).get("blocked") is True
                and source_object.read_bytes() == source_original
                and self.git_value(source, ["ls-remote", "--refs", destination], name="verify refs after oversized-header refusal") == before,
                pointers,
            )
        finally:
            if object_path.exists():
                object_path.unlink()
            object_path.write_bytes(original)
        repaired = self.run_cmd("mirror verifies restored cache object", args, self.run_root)
        self.check(
            "mirror-restored-cache-resumes-complete-pointer-proof",
            self.json_data(repaired, "mirror.check").get("pointers", {}).get("verified") == 1
            and source_object.read_bytes() == source_original
            and self.git_value(source, ["ls-remote", "--refs", destination], name="verify refs after cache restoration") == before,
        )

    def mirror_reconciliation_checks(self) -> None:
        """Exercise read-only drift, immutable plan/apply, CI, and deletion approval."""
        mirror_source = self.run_root / "mirror-source"
        mirror_upstream = self.run_root / "mirror-upstream.git"
        mirror_url = self.remote_url + "-mirror"
        mirror_source.mkdir()
        self.run_git(self.run_root, ["init", "-b", "main", str(mirror_source)])
        self.run_git(mirror_source, ["config", "core.hooksPath", ".git/hooks"])
        self.run_git(self.run_root, ["init", "--bare", str(mirror_upstream)])
        self.run_git(
            mirror_source,
            ["remote", "add", "origin", str(mirror_upstream)],
        )
        (mirror_source / "baseline.txt").write_text("mirror baseline\n", encoding="utf-8")
        self.run_git(mirror_source, ["add", "baseline.txt"])
        self.run_git(mirror_source, ["commit", "-m", "mirror baseline"])
        self.run_cmd(
            "initialize isolated mirror destination",
            [str(self.crab_bin), "init", "--mirror=origin", mirror_url],
            mirror_source,
        )
        content = deterministic_bytes(1024 * 1024, f"mirror-origin:{self.run_id}")
        (mirror_source / "mirror-data.bin").write_bytes(content)
        self.run_cmd(
            "track real mirror pointer data",
            [str(self.crab_bin), "track", "mirror-data.bin"],
            mirror_source,
        )
        self.run_cmd(
            "stage real mirror pointer data",
            [str(self.crab_bin), "add", "mirror-data.bin", "--json"],
            mirror_source,
        )
        self.run_git(mirror_source, ["add", ".gitattributes"])
        self.run_git(mirror_source, ["commit", "-m", "mirror origin byte fixture"])
        self.run_git(mirror_source, ["tag", "-a", "mirror-v1", "-m", "mirror annotated tag"])
        self.run_git(
            mirror_source,
            ["push", mirror_url, "refs/heads/main:refs/heads/main", "refs/tags/mirror-v1:refs/tags/mirror-v1"],
            name="seed isolated mirror destination",
        )
        baseline = self.run_cmd(
            "verify isolated mirror baseline",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--json",
            ],
            self.run_root,
        )
        baseline_data = self.json_data(baseline, "mirror.check")
        self.check(
            "mirror-equal-baseline-passes-CI-policy",
            baseline_data.get("state") == "equal"
            and baseline_data.get("ci_passed") is True
            and baseline_data.get("hook", {}).get("state") == "installed",
        )
        pointer_proof = baseline_data.get("pointers", {})
        self.check(
            "mirror-verifies-real-origin-pointer-bytes",
            pointer_proof.get("discovered") == 1
            and pointer_proof.get("verified") == 1
            and pointer_proof.get("state") == "verified",
            pointer_proof,
        )
        index_prefix = f"{REMOTE_PREFIX}/{self.run_id}-mirror/file_index_db/"
        index_listing = self.run_aws(
            ["list-objects-v2", "--bucket", self.args.bucket, "--prefix", index_prefix],
            name="inventory isolated mirror acceleration before fault",
        )
        index_keys = [entry["Key"] for entry in json.loads(self.stdout(index_listing)).get("Contents", [])]
        if not index_keys or any(not key.startswith(index_prefix) for key in index_keys):
            raise SmokeError("mirror index fault requires a nonempty exact-prefix inventory")
        # Remove only this disposable repository's derived file index. Canonical
        # manifest/shards/xorbs remain intact; inspection must not recreate a DB.
        for key in index_keys:
            self.run_aws(
                ["delete-object", "--bucket", self.args.bucket, "--key", key],
                name="remove isolated mirror acceleration object",
            )
        without_index = self.run_cmd(
            "verify mirror origin without acceleration",
            [str(self.crab_bin), "mirror", str(mirror_source), mirror_url, "--check", "--ci", "--json"],
            self.run_root,
        )
        without_index_data = self.json_data(without_index, "mirror.check")
        index_after = self.run_aws(
            ["list-objects-v2", "--bucket", self.args.bucket, "--prefix", index_prefix],
            name="verify mirror check did not recreate acceleration",
        )
        self.check(
            "mirror-verifies-canonical-data-without-acceleration-writes",
            without_index_data.get("ci_passed") is True
            and without_index_data.get("pointers", {}).get("verified") == 1
            and not json.loads(self.stdout(index_after)).get("Contents"),
            {"removed_derived_objects": len(index_keys), "pointers": without_index_data.get("pointers")},
        )
        mirror_clone = self.run_root / "mirror-hydrated-clone"
        self.run_cmd(
            "clone and hydrate mirrored pointer data",
            [str(self.crab_bin), "clone", mirror_url, str(mirror_clone), "--no-lazy"],
            self.run_root,
        )
        self.check(
            "mirror-clone-reconstructs-exact-source-bytes",
            (mirror_clone / "mirror-data.bin").read_bytes() == content,
            {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()},
        )
        self.run_git(mirror_clone, ["fsck", "--strict", "--full"], name="strict fsck mirrored data clone")
        self.mirror_layout_identity_check(mirror_source, mirror_url)
        self.mirror_oversized_header_check(mirror_source, mirror_url)
        self.mirror_metadata_staleness_check(mirror_source, mirror_url, "equal")

        hook = mirror_source / ".git/hooks/pre-push"
        disabled_hook = hook.with_name("pre-push.disabled-for-smoke")
        hook.rename(disabled_hook)
        try:
            missing_hook = self.run_cmd(
                "mirror CI detects missing hook",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url, "--check", "--ci", "--json"],
                self.run_root,
                check=False,
            )
        finally:
            disabled_hook.rename(hook)
        missing_hook_data = self.json_data(missing_hook, "mirror.check")
        self.check(
            "mirror-CI-detects-missing-hook",
            missing_hook["exit_code"] != 0
            and missing_hook_data.get("hook", {}).get("state") == "missing"
            and missing_hook_data.get("ci_passed") is False,
        )

        plan = self.artifacts / "mirror-source-ahead-plan.json"
        self.run_git(mirror_source, ["switch", "-c", "mirror/reconciliation"])
        (mirror_source / "mirror-reconciliation.txt").write_text(
            "source-ahead mirror state\n", encoding="utf-8"
        )
        self.run_git(mirror_source, ["add", "mirror-reconciliation.txt"])
        self.run_git(mirror_source, ["commit", "-m", "mirror source-ahead fixture"])
        self.mirror_metadata_staleness_check(mirror_source, mirror_url, "source-ahead")

        check = self.run_cmd(
            "mirror source-ahead check and plan",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--write-plan",
                str(plan),
                "--json",
            ],
            self.run_root,
        )
        check_data = self.json_data(check, "mirror.check")
        plan_data = json.loads(plan.read_text(encoding="utf-8"))
        self.check(
            "mirror-plan-captures-source-ahead",
            check_data.get("state") == "source_ahead"
            and plan_data.get("blocked") is False
            and any(
                action.get("kind") == "update_crab_ref"
                and action.get("ref_name") == "refs/heads/mirror/reconciliation"
                for action in plan_data.get("actions", [])
            ),
            {"state": check_data.get("state"), "plan_id": plan_data.get("plan_id")},
        )

        relative_cache = "relative-mirror/cache.git"
        relative_check = self.run_cmd(
            "inspect mirror using nested relative cache",
            [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
             "--check", "--cache-dir", relative_cache, "--json"],
            self.run_root,
        )
        relative_data = self.json_data(relative_check, "mirror.check")
        self.check(
            "mirror-relative-cache-resolves-from-invocation",
            Path(relative_data["cache_dir"]).resolve() == (self.run_root / relative_cache).resolve()
            and relative_data.get("state") == "source_ahead",
        )

        cache = Path(check_data["cache_dir"]).resolve()
        cache_refs_before = self.git_value(cache, ["show-ref"], name="cache refs before mirror faults")
        crab_refs_before = self.git_value(
            self.run_root, ["ls-remote", "--refs", mirror_url], name="Crab refs before mirror faults"
        )
        # Hold the same native advisory lock from a separate process. The
        # released-shape CLI must refuse all three paths before cache refresh.
        lock_path = cache.with_name(cache.name + ".crab-cache-use.lock")
        with hold_cache_lock(lock_path):
            busy_check = self.run_cmd(
                "mirror CI refuses busy cache",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
                 "--check", "--ci", "--json"],
                self.run_root, check=False,
            )
            busy_apply = self.run_cmd(
                "mirror apply refuses busy cache",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
                 "--apply-plan", str(plan), "--json"],
                self.run_root, check=False,
            )
            busy_legacy = self.run_cmd(
                "legacy mirror refuses busy cache",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
                 "--skip-lfs", "--json"],
                self.run_root, check=False,
            )
        busy_data = self.json_data(busy_check, "mirror.check")
        self.check(
            "mirror-cache-contention-fails-closed",
            all(result["exit_code"] != 0 for result in (busy_check, busy_apply, busy_legacy))
            and busy_data.get("state") == "unverifiable"
            and busy_data.get("ci_passed") is False,
        )

        offline_source = self.run_root / "mirror-source-offline"
        mirror_source.rename(offline_source)
        try:
            source_outage = self.run_cmd(
                "mirror CI refuses stale cache when source is unavailable",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
                 "--check", "--ci", "--json"],
                self.run_root, check=False,
            )
            outage_apply = self.run_cmd(
                "mirror plan cannot apply with unavailable source",
                [str(self.crab_bin), "mirror", str(mirror_source), mirror_url,
                 "--apply-plan", str(plan), "--json"],
                self.run_root, check=False,
            )
        finally:
            offline_source.rename(mirror_source)
        outage_data = self.json_data(source_outage, "mirror.check")
        self.check(
            "mirror-source-unavailable-fails-closed",
            source_outage["exit_code"] != 0
            and outage_data.get("state") == "unverifiable"
            and outage_data.get("refs") == []
            and outage_data.get("ci_passed") is False,
        )
        self.check("mirror-unavailable-source-refuses-apply", outage_apply["exit_code"] != 0)
        cache_refs_after = self.git_value(cache, ["show-ref"], name="cache refs after mirror faults")
        crab_refs_after = self.git_value(
            self.run_root, ["ls-remote", "--refs", mirror_url], name="Crab refs after mirror faults"
        )
        self.check(
            "mirror-cache-and-source-faults-preserve-refs",
            cache_refs_after == cache_refs_before and crab_refs_after == crab_refs_before,
        )

        self.mirror_cache_cleanup_checks(
            mirror_source, mirror_url, self.run_root / relative_cache, plan
        )
        self.mirror_cancellation_checks(mirror_source, mirror_url)

        shallow_source = self.run_root / "mirror-shallow-source.git"
        shallow_plan = self.artifacts / "mirror-shallow-source-plan.json"
        self.run_git(
            self.run_root,
            ["clone", "--bare", "--depth=1", "--no-local", str(mirror_source), str(shallow_source)],
            name="prepare shallow mirror source",
        )
        shallow_check = self.run_cmd(
            "incomplete mirror source cannot plan destination deletions",
            [str(self.crab_bin), "mirror", str(shallow_source), mirror_url,
             "--cache-dir", str(self.run_root / "mirror-shallow-cache.git"),
             "--check", "--ci", "--allow-delete-refs", "--write-plan", str(shallow_plan), "--json"],
            self.run_root, check=False,
        )
        shallow_data = self.json_data(shallow_check, "mirror.check")
        self.check(
            "mirror-shallow-source-cannot-propose-destination-deletions",
            shallow_check["exit_code"] != 0
            and shallow_data.get("state") == "unverifiable"
            and json.loads(shallow_plan.read_text(encoding="utf-8")).get("blocked") is True
            and self.git_value(
                self.run_root, ["ls-remote", "--refs", mirror_url],
                name="destination refs after incomplete source rejection",
            ) == crab_refs_before,
        )

        applied = self.run_cmd(
            "apply mirror source-ahead plan",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--apply-plan",
                str(plan),
                "--json",
            ],
            self.run_root,
        )
        applied_data = self.json_data(applied, "mirror.apply")
        source_oid = self.git_value(
            mirror_source,
            ["rev-parse", "refs/heads/mirror/reconciliation"],
            name="mirror source-ahead oid",
        )
        crab_oid = self.git_value(
            mirror_source,
            ["ls-remote", mirror_url, "refs/heads/mirror/reconciliation"],
            name="mirror applied oid",
        ).split()[0]
        self.check(
            "mirror-plan-apply-publishes-exact-ref",
            applied_data.get("final_state") == "equal" and crab_oid == source_oid,
            {"source_oid": source_oid, "crab_oid": crab_oid},
        )

        reapplied = self.run_cmd(
            "reapply mirror plan",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--apply-plan",
                str(plan),
                "--json",
            ],
            self.run_root,
        )
        self.check(
            "mirror-plan-reapply-is-idempotent",
            self.json_data(reapplied, "mirror.apply").get("already_applied") is True,
        )

        self.run_git(
            mirror_source,
            ["push", mirror_url, "HEAD:refs/heads/mirror-recoverable"],
            name="create Crab-only recoverable ref",
        )
        blocked_plan = self.artifacts / "mirror-blocked-deletion-plan.json"
        blocked = self.run_cmd(
            "plan Crab-only ref without deletion approval",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--write-plan",
                str(blocked_plan),
                "--json",
            ],
            self.run_root,
        )
        blocked_data = json.loads(blocked_plan.read_text(encoding="utf-8"))
        self.check(
            "mirror-retains-Crab-only-ref-by-default",
            self.json_data(blocked, "mirror.check").get("state") == "crab_ahead"
            and blocked_data.get("blocked") is True
            and blocked_data.get("actions") == [],
        )

        delete_plan = self.artifacts / "mirror-approved-deletion-plan.json"
        self.run_cmd(
            "plan explicitly approved Crab-only deletion",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--allow-delete-refs",
                "--write-plan",
                str(delete_plan),
                "--json",
            ],
            self.run_root,
        )
        refused = self.run_cmd(
            "refuse deletion plan without second approval",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--apply-plan",
                str(delete_plan),
                "--json",
            ],
            self.run_root,
            check=False,
        )
        self.check("mirror-delete-requires-apply-approval", refused["exit_code"] != 0)
        self.run_cmd(
            "apply explicitly approved Crab-only deletion",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--apply-plan",
                str(delete_plan),
                "--allow-delete-refs",
                "--json",
            ],
            self.run_root,
        )
        deleted = self.run_git(
            mirror_source,
            ["ls-remote", mirror_url, "refs/heads/mirror-recoverable"],
            name="verify Crab-only ref deletion",
        )
        self.check("mirror-approved-delete-removes-only-planned-ref", not self.stdout(deleted).strip())

        unavailable = self.run_cmd(
            "mirror CI reports unavailable provider",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--ci",
                "--json",
            ],
            self.run_root,
            check=False,
            extra_env={
                "AWS_ENDPOINT": "http://127.0.0.1:1",
                "AWS_ENDPOINT_URL": "http://127.0.0.1:1",
                "AWS_ENDPOINT_URL_S3": "http://127.0.0.1:1",
                "ENDPOINT_URL": "http://127.0.0.1:1",
                "AWS_MAX_ATTEMPTS": "1",
            },
        )
        unavailable_data = self.json_data(unavailable, "mirror.check")
        self.check(
            "mirror-CI-fails-closed-when-provider-unavailable",
            unavailable["exit_code"] != 0
            and unavailable_data.get("ci_passed") is False
            and unavailable_data.get("state") == "unverifiable",
        )

        divergence_base = self.git_value(
            mirror_source,
            ["rev-parse", "HEAD"],
            name="mirror divergence base",
        )
        (mirror_source / "source-divergence.txt").write_text(
            "source side\n", encoding="utf-8"
        )
        self.run_git(mirror_source, ["add", "source-divergence.txt"])
        self.run_git(mirror_source, ["commit", "-m", "source side divergence"])
        source_divergence_oid = self.git_value(
            mirror_source,
            ["rev-parse", "HEAD"],
            name="mirror source divergence oid",
        )
        self.run_git(mirror_source, ["reset", "--hard", divergence_base])
        (mirror_source / "crab-divergence.txt").write_text(
            "Crab side\n", encoding="utf-8"
        )
        self.run_git(mirror_source, ["add", "crab-divergence.txt"])
        self.run_git(mirror_source, ["commit", "-m", "Crab side divergence"])
        self.run_git(
            mirror_source,
            [
                "push",
                "--force",
                mirror_url,
                "HEAD:refs/heads/mirror/reconciliation",
            ],
            name="publish Crab side divergence",
        )
        self.run_git(mirror_source, ["reset", "--hard", source_divergence_oid])
        diverged_plan = self.artifacts / "mirror-diverged-plan.json"
        diverged = self.run_cmd(
            "mirror detects and blocks diverged ref",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--write-plan",
                str(diverged_plan),
                "--json",
            ],
            self.run_root,
        )
        diverged_data = self.json_data(diverged, "mirror.check")
        diverged_plan_data = json.loads(diverged_plan.read_text(encoding="utf-8"))
        self.check(
            "mirror-divergence-is-detected-without-mutation",
            diverged_data.get("state") == "diverged"
            and diverged_plan_data.get("blocked") is True
            and diverged_plan_data.get("actions") == []
            and any(
                ref.get("name") == "refs/heads/mirror/reconciliation"
                and ref.get("state") == "diverged"
                for ref in diverged_data.get("refs", [])
            ),
        )

        (mirror_source / "missing-data.ptr").write_text(
            "version https://crab.dev/spec/v1\n" + "file-hash " + "f" * 64 + "\nsize 1\n",
            encoding="utf-8",
        )
        self.run_git(mirror_source, ["add", "missing-data.ptr"])
        self.run_git(mirror_source, ["commit", "-m", "missing pointer data fixture"])
        missing = self.run_cmd(
            "mirror CI rejects missing pointer data",
            [
                str(self.crab_bin),
                "mirror",
                str(mirror_source),
                mirror_url,
                "--check",
                "--ci",
                "--json",
            ],
            self.run_root,
            check=False,
        )
        missing_data = self.json_data(missing, "mirror.check")
        self.check(
            "mirror-CI-fails-missing-pointer-data",
            missing["exit_code"] != 0
            and missing_data.get("ci_passed") is False
            and missing_data.get("pointers", {}).get("state") == "missing",
        )

    def redaction_check(self) -> None:
        leaks: list[str] = []
        for path in (*self.logs.glob("*.log"), *self.artifacts.glob("*.json")):
            if path.name == "report.json":
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            if any(value and value != "crab" and value in text for value in self.credentials().values()):
                leaks.append(str(path))
        self.check("retained-artifacts-redacted", not leaks, {"leaks": leaks})

    def git_config(self, repo: Path, key: str, label: str) -> str | None:
        record = self.run_git(repo, ["config", "--get", key], name=label, check=False)
        value = self.stdout(record).strip()
        return value or None

    def promisor_sidecars(self, repo: Path) -> list[Path]:
        return sorted((repo / ".git" / "objects" / "pack").glob("*.promisor"))

    def pack_files(self, repo: Path) -> list[Path]:
        return sorted((repo / ".git" / "objects" / "pack").glob("*.pack"))

    def object_present_in_odb(self, repo: Path, oid: str) -> bool:
        """Check the local object database without allowing a promisor fetch."""
        objects = repo / ".git" / "objects"
        if (objects / oid[:2] / oid[2:]).is_file():
            return True
        for index in sorted((objects / "pack").glob("*.idx")):
            result = subprocess.run(
                [str(self.git_bin), "verify-pack", "-v", str(index)],
                cwd=repo,
                env=self.env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                detail = result.stderr.strip() or f"exit {result.returncode}"
                raise SmokeError(f"cannot inspect pack index {index}: {detail}")
            if any(line.startswith(f"{oid} ") for line in result.stdout.splitlines()):
                return True
        return False

    def odb_bytes(self, repo: Path) -> int:
        root = repo / ".git" / "objects"
        return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())

    def record_provenance(self, fixture_revision: str, health: dict[str, Any]) -> None:
        git_version = subprocess.run(
            [str(self.git_bin), "--version"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        ).stdout.strip()
        crab_version = subprocess.run(
            [str(self.crab_bin), "--version"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        ).stdout.strip()
        rollback_version = None
        if self.rollback_crab_bin is not None:
            rollback_version = subprocess.run(
                [str(self.rollback_crab_bin), "--version"],
                env=self.env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            ).stdout.strip()
        aws_version = subprocess.run(
            ["aws", "--version"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        ).stdout.strip()
        source_revision_result = subprocess.run(
            [str(self.git_bin), "-C", str(self.crab_source), "rev-parse", "HEAD"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        source_revision = source_revision_result.stdout.strip() if source_revision_result.returncode == 0 else None
        source_status_result = subprocess.run(
            [
                str(self.git_bin),
                "-C",
                str(self.crab_source),
                "status",
                "--porcelain",
                "--untracked-files=all",
            ],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        source_status = source_status_result.stdout if source_status_result.returncode == 0 else ""
        binary_metadata_result = subprocess.run(
            [str(self.crab_bin), "version", "--json"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        binary_git_sha: str | None = None
        if binary_metadata_result.returncode == 0:
            try:
                binary_metadata = json.loads(binary_metadata_result.stdout)
                binary_git_sha = binary_metadata.get("data", {}).get("git_sha")
            except json.JSONDecodeError:
                binary_git_sha = None
        source_matches_binary = bool(
            source_revision
            and binary_git_sha
            and binary_git_sha != "unknown"
            and source_revision.startswith(binary_git_sha)
        )
        self.report["provenance"] = {
            "fixture_source_revision": fixture_revision,
            "crab_source_root": str(self.crab_source),
            "crab_source_revision": source_revision,
            "crab_source_dirty": bool(source_status),
            "crab_source_status_sha256": hashlib.sha256(source_status.encode()).hexdigest(),
            "crab_binary_reported_git_sha": binary_git_sha,
            "crab_binary_matches_source_revision": source_matches_binary,
            "crab_source_checkout_clean": not bool(source_status),
            "crab_binary": str(self.crab_bin),
            "crab_binary_sha256": sha256_file(self.crab_bin),
            "rollback_binary": str(self.rollback_crab_bin) if self.rollback_crab_bin else None,
            "rollback_binary_sha256": (
                sha256_file(self.rollback_crab_bin) if self.rollback_crab_bin else None
            ),
            "rollback_crab_tag": self.rollback_crab_tag,
            "git_version": redact_text(git_version, self.credentials()),
            "crab_version": redact_text(crab_version, self.credentials()),
            "rollback_crab_version": redact_text(rollback_version, self.credentials())
            if rollback_version
            else None,
            "aws_cli_version": redact_text(aws_version, self.credentials()),
            "backend": self.args.backend,
            "object_store_health": health,
            "operating_system": {"darwin": "macos", "win32": "windows"}.get(sys.platform, sys.platform),
            "repository_mode": "direct",
            "object_store_container": self.args.rustfs_container,
        }
        self.write_report()
        self.check(
            "crab-binary-matches-source-revision",
            source_matches_binary,
            {
                "crab_source_revision": source_revision,
                "crab_binary_reported_git_sha": binary_git_sha,
                "crab_source_dirty": bool(source_status),
            },
        )

    def run(self) -> None:
        health = self.endpoint_health()
        self.check("object-store-health", health.get("ready") is True, {"service": health.get("service"), "version": health.get("version")})
        self.ensure_bucket()
        self.store_snapshot("before-push")
        (
            source_revision,
            large_oid,
            small_oid,
            batch_first_oid,
            batch_second_oid,
            sparse_oid,
            lfs_oid,
        ) = self.setup_source()
        self.record_provenance(source_revision, health)
        self.store_snapshot("after-push")
        self.native_ref_lifecycle_checks()

        baseline = self.storage_telemetry()
        self.clone_full(baseline)
        self.store_snapshot("after-full-clone")
        self.clone_legacy(self.storage_telemetry())
        self.performance_fixtures()
        self.pointer_fixture_checks(lfs_oid)
        self.clone_shallow()
        self.store_snapshot("after-shallow-lifecycle")
        filtered_baseline = self.storage_telemetry()
        filtered_result = self.clone_filtered(
            large_oid,
            small_oid,
            (batch_first_oid, batch_second_oid),
            filtered_baseline,
        )
        self.store_snapshot("after-filtered-lifecycle")
        self.filter_matrix(large_oid, small_oid, sparse_oid)
        hidden_oid, dangling_oid = self.create_security_refs()
        self.security_checks(hidden_oid, dangling_oid)
        self.disconnect_check()
        if self.args.mirror_reconciliation:
            self.mirror_pre_push_batch_checks()
            self.mirror_reconciliation_checks()
        self.redaction_check()
        self.report["protocol_telemetry"] = self.protocol_telemetry()
        self.write_report()

        full_bytes = self.odb_bytes(self.full)
        filtered_bytes = filtered_result["initial_odb_bytes"]
        self.check(
            "filtered-initial-odb-smaller",
            filtered_bytes < full_bytes,
            {"full_odb_bytes": full_bytes, "filtered_initial_odb_bytes": filtered_bytes},
        )
        full_reads = self.report["telemetry"].get("full_clone", {})
        filtered_reads = self.report["telemetry"].get("filtered_clone", {})
        self.check(
            "filtered-transfer-smaller",
            int(filtered_reads.get("bytes", 0)) < int(full_reads.get("bytes", 0)),
            {"full_clone": full_reads, "filtered_clone_and_lazy_fetch": filtered_reads},
        )
        self.report["status"] = "passed"
        self.write_report()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crab-bin", default="crab", help="Crab binary used by the helper alias")
    parser.add_argument(
        "--rollback-crab-bin",
        help="Optional prior Crab binary used to qualify raw-OID promisor rollback compatibility",
    )
    parser.add_argument(
        "--rollback-crab-tag",
        help="Optional tag recorded with the prior Crab binary used for rollback qualification",
    )
    parser.add_argument(
        "--git-bin",
        default=os.environ.get("CRAB_GIT_BIN", "git"),
        help="Git executable used for all fixture, protocol, and provenance commands",
    )
    parser.add_argument("--bucket", default=os.environ.get("CRAB_E2E_BUCKET", DEFAULT_BUCKET))
    parser.add_argument("--endpoint-url", default=os.environ.get("AWS_ENDPOINT_URL", DEFAULT_ENDPOINT))
    parser.add_argument(
        "--backend",
        choices=("rustfs", "s3"),
        default=os.environ.get("CRAB_E2E_BACKEND", "rustfs"),
        help="Object-store backend; s3 skips the RustFS health endpoint",
    )
    parser.add_argument(
        "--require-existing-bucket",
        action="store_true",
        help="Refuse to create a missing bucket (required for external qualification)",
    )
    parser.add_argument(
        "--mirror-reconciliation",
        action="store_true",
        help="Also qualify mirror check, plan/apply, CI, and deletion safety",
    )
    parser.add_argument("--access-key", default=os.environ.get("AWS_ACCESS_KEY_ID", "crab"))
    parser.add_argument("--secret-key", default=os.environ.get("AWS_SECRET_ACCESS_KEY", "crab"))
    parser.add_argument("--session-token", default=os.environ.get("AWS_SESSION_TOKEN", ""))
    parser.add_argument("--region", default=os.environ.get("AWS_REGION", "us-east-1"))
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument(
        "--source-root",
        type=Path,
        default=Path(__file__).resolve().parents[3],
        help="Crab Git checkout used for source/artifact provenance",
    )
    parser.add_argument("--run-id")
    parser.add_argument("--rustfs-container", default=os.environ.get("RUSTFS_CONTAINER", "rustfs"))
    parser.add_argument("--timeout", type=int, default=180)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    smoke: ProtocolV2PartialCloneSmoke | None = None
    try:
        smoke = ProtocolV2PartialCloneSmoke(args)
        smoke.run()
    except Exception as error:  # noqa: BLE001 - the report must retain any failed step
        if smoke is not None:
            smoke.report["status"] = "failed"
            smoke.report["error"] = str(error)
            smoke.write_report()
            print(f"protocol-v2 partial-clone smoke failed; report: {smoke.artifacts / 'report.json'}")
        else:
            print(f"protocol-v2 partial-clone smoke failed before report creation: {error}")
        return 1
    print(f"protocol-v2 partial-clone smoke passed; report: {smoke.artifacts / 'report.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
